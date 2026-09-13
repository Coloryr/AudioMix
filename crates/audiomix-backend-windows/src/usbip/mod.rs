//! USB/IP 虚拟声卡：应用内置 USB/IP 服务器 + UAC2 设备仿真。
//!
//! usbip-win2 传输驱动（微软签名，HVCI 兼容）通过 `usbip attach` 把本服务器
//! 仿真的线缆接入 Windows，由内置 usbaudio2.sys 暴露为标准播放/录音端点。
//! 每条线缆的采样率（44.1–192kHz）与位深（16/24/32bit）独立可配置。

pub mod backend;
pub mod descriptors;
pub mod device;
pub mod protocol;
pub mod ring;
pub mod server;

#[cfg(windows)]
pub mod attach;

use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

pub use backend::UsbIpBackend;
pub use descriptors::CableProtocol;
use device::{Cable, CableConfig, CableMode};

use audiomix_core::model::{UsbIpCableMode, UsbIpCableSettings, UsbIpSettings};

impl From<UsbIpCableMode> for CableMode {
    fn from(mode: UsbIpCableMode) -> Self {
        match mode {
            UsbIpCableMode::Loopback => CableMode::Loopback,
            UsbIpCableMode::Reverse => CableMode::Reverse,
            UsbIpCableMode::Mixer => CableMode::Mixer,
        }
    }
}

impl From<UsbIpCableSettings> for CableConfig {
    fn from(c: UsbIpCableSettings) -> Self {
        Self {
            number: c.number,
            name: c.display_name(),
            sample_rate: c.sample_rate,
            bits: c.bits,
            mode: c.mode.into(),
            buffer_ms: c.buffer_ms,
            protocol: match c.protocol {
                audiomix_core::model::UsbIpCableProtocol::Uac1 => CableProtocol::Uac1,
                audiomix_core::model::UsbIpCableProtocol::Uac2 => CableProtocol::Uac2,
            },
        }
    }
}

/// 设置里的线缆表 → 后端线缆配置（启动服务器用）
pub fn cable_configs(settings: &UsbIpSettings) -> Vec<CableConfig> {
    settings.cables.iter().cloned().map(CableConfig::from).collect()
}

/// 从 `host:port` 形式拆出（host, port）；缺端口时用默认 3240
pub fn split_host_port(bind: &str) -> (String, u16) {
    match bind.rsplit_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(p) => (host.to_string(), p),
            Err(_) => (bind.to_string(), 3240),
        },
        None => (bind.to_string(), 3240),
    }
}

/// 活动线缆表（服务器与后端适配器共享）
pub type CableRegistry = RwLock<Vec<Arc<Cable>>>;

pub const DEFAULT_BIND: &str = "127.0.0.1:3240";

/// `stop()` 最多等 accept 任务退出多久（套接字随之释放）
const STOP_RELEASE_WAIT: std::time::Duration = std::time::Duration::from_millis(300);
/// 重启时等上一次 accept 任务退出的上限
const BIND_RELEASE_WAIT: std::time::Duration = std::time::Duration::from_millis(300);
/// 端口刚释放时的 bind 重试次数与间隔
const BIND_ATTEMPTS: usize = 10;
const BIND_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(30);

/// USB/IP 服务器生命周期管理。
/// 线缆配置变化 = 重启服务器（描述符是静态的）+ Windows 端重新 attach。
pub struct UsbIpManager {
    registry: Arc<CableRegistry>,
    bind: String,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    local_addr: Mutex<Option<std::net::SocketAddr>>,
}

impl Default for UsbIpManager {
    fn default() -> Self {
        Self::new(DEFAULT_BIND)
    }
}

impl UsbIpManager {
    pub fn new(bind: impl Into<String>) -> Self {
        Self {
            registry: Arc::new(RwLock::new(Vec::new())),
            bind: bind.into(),
            task: Mutex::new(None),
            local_addr: Mutex::new(None),
        }
    }

    /// 线缆注册表句柄（后端适配器共享同一份）
    pub fn registry(&self) -> Arc<CableRegistry> {
        self.registry.clone()
    }

    /// 当前线缆快照
    pub fn cables(&self) -> Vec<Arc<Cable>> {
        self.registry.read().clone()
    }

    pub fn running(&self) -> bool {
        // 任务句柄存在但已经跑完（panic / 被 abort）时不算运行中
        self.task.lock().as_ref().is_some_and(|t| !t.is_finished())
    }

    pub fn bind_addr(&self) -> &str {
        &self.bind
    }

    /// 实际监听地址（bind :0 时可查分配到的端口）
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        *self.local_addr.lock()
    }

    /// 以给定配置（重）启服务器。绑定失败同步返回错误。
    ///
    /// 已经在运行时**不重绑监听套接字**，只替换线缆表——USB/IP 的 devlist/import
    /// 都从共享的线缆表读，换表即生效；而重绑会踩两个坑：
    /// ① 已接入的设备在内核侧还攥着到 127.0.0.1:3240 的连接；
    /// ② accept 任务的 abort 是异步的，紧接着 bind 会撞上自己还没释放的监听套接字
    ///    （实测就是用户看到的「绑定 127.0.0.1:3240 失败（端口被占用？）」）。
    /// 已接入的设备保持连接，改格式/改名要重新 attach 才生效（UI 已提示）。
    pub fn start(&self, rt: &tokio::runtime::Handle, configs: Vec<CableConfig>) -> Result<(), String> {
        // 先建线缆：描述符自检不过就别动正在跑的服务
        let mut seen = std::collections::HashSet::new();
        let mut cables = Vec::with_capacity(configs.len());
        for cfg in configs {
            if !seen.insert(cfg.number) {
                return Err(format!("线缆号 {} 重复", cfg.number));
            }
            cables.push(Cable::new(cfg)?);
        }

        if self.running() {
            let count = cables.len();
            *self.registry.write() = cables;
            tracing::info!("USB/IP 服务器线缆已更新（{count} 条，继续监听 {}）", self.bind);
            return Ok(());
        }

        let listener = self.bind_listener(rt)?;
        let addr = listener.local_addr().ok();

        // 先发布线缆表再启动服务，保证首个 devlist 请求就能看到
        let count = cables.len();
        *self.registry.write() = cables;
        *self.local_addr.lock() = addr;
        let reg = self.registry.clone();
        let task = rt.spawn(async move { server::serve(listener, reg).await });
        *self.task.lock() = Some(task);
        tracing::info!("USB/IP 服务器已启动（{count} 条线缆，监听 {}）", self.bind);
        Ok(())
    }

    /// 绑定监听套接字：先等上一次的 accept 任务真正退出（它一退出，
    /// JoinSet 就把所有会话连套接字一起关掉），再带重试地 bind。
    fn bind_listener(&self, rt: &tokio::runtime::Handle) -> Result<tokio::net::TcpListener, String> {
        self.close_task(BIND_RELEASE_WAIT);

        let mut last_err = String::new();
        let mut bound = None;
        for attempt in 0..BIND_ATTEMPTS {
            match std::net::TcpListener::bind(&self.bind) {
                Ok(l) => {
                    bound = Some(l);
                    break;
                }
                Err(e) => {
                    last_err = e.to_string();
                    if attempt == 0 {
                        tracing::debug!("绑定 {} 失败（{e}），等待端口释放后重试", self.bind);
                    }
                    std::thread::sleep(BIND_RETRY_INTERVAL);
                }
            }
        }
        let std_listener = bound.ok_or_else(|| {
            format!("绑定 {} 失败: {last_err}（端口被其它程序占用？先关掉占用者再试）", self.bind)
        })?;
        std_listener
            .set_nonblocking(true)
            .map_err(|e| format!("设置监听套接字非阻塞失败: {e}"))?;
        // from_std 需要运行时上下文
        let _guard = rt.enter();
        tokio::net::TcpListener::from_std(std_listener)
            .map_err(|e| format!("迁移监听套接字失败: {e}"))
    }

    /// abort accept 任务并（限时）等它退出，确认监听套接字与所有会话都已释放
    fn close_task(&self, wait: std::time::Duration) {
        let Some(task) = self.task.lock().take() else {
            *self.local_addr.lock() = None;
            return;
        };
        task.abort();
        let deadline = std::time::Instant::now() + wait;
        while !task.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        *self.local_addr.lock() = None;
    }

    pub fn stop(&self) {
        let was_running = self.task.lock().is_some();
        self.close_task(STOP_RELEASE_WAIT);
        if was_running {
            tracing::info!("USB/IP 服务器已停止");
        }
        // 服务器停了就不该再对外暴露线缆：否则引擎仍会列出 usbip://N/* 设备，
        // 但没有任何 USB 主机在取数据（打开流只会静默空转）。
        self.registry.write().clear();
    }
}

impl Drop for UsbIpManager {
    fn drop(&mut self) {
        // 这里不等待任务退出：abort 已经发出，进程/实例都要没了，不必再阻塞调用线程
        self.close_task(std::time::Duration::ZERO);
        self.registry.write().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(number: u8) -> CableConfig {
        CableConfig {
            number,
            name: String::new(),
            sample_rate: 48_000,
            bits: 16,
            mode: CableMode::Loopback,
            buffer_ms: 250,
            protocol: CableProtocol::default(),
        }
    }

    #[test]
    fn settings_convert_to_cable_configs() {
        let settings = UsbIpSettings {
            enabled: true,
            bind: "127.0.0.1:3240".into(),
            cables: vec![UsbIpCableSettings {
                number: 2,
                name: "直播线".into(),
                sample_rate: 192_000,
                bits: 32,
                mode: UsbIpCableMode::Mixer,
                buffer_ms: 120,
                // 192k/32bit 超出全速带宽，必须用 UAC2
                protocol: audiomix_core::model::UsbIpCableProtocol::Uac2,
            }],
        };
        let cfgs = cable_configs(&settings);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].number, 2);
        assert_eq!(cfgs[0].name, "直播线", "自定义名要带到后端配置");
        assert_eq!(cfgs[0].sample_rate, 192_000);
        assert_eq!(cfgs[0].bits, 32);
        assert_eq!(cfgs[0].mode, CableMode::Mixer);
        assert_eq!(cfgs[0].buffer_ms, 120);
        // 转换后的配置必须能真正建起线缆（描述符自检通过），且产品名就是自定义名
        let cable = Cable::new(cfgs[0].clone()).expect("应能建起线缆");
        assert_eq!(cable.product_name(), "直播线");
    }

    #[test]
    fn unnamed_cable_falls_back_to_default_product_name() {
        let settings = UsbIpSettings {
            enabled: true,
            bind: "127.0.0.1:3240".into(),
            cables: vec![UsbIpCableSettings { number: 5, ..Default::default() }],
        };
        let cfgs = cable_configs(&settings);
        let cable = Cable::new(cfgs[0].clone()).unwrap();
        assert_eq!(cable.product_name(), "Virtual Cable 05");
    }

    #[test]
    fn bind_address_is_split() {
        assert_eq!(split_host_port("127.0.0.1:3240"), ("127.0.0.1".into(), 3240));
        assert_eq!(split_host_port("127.0.0.1:4000"), ("127.0.0.1".into(), 4000));
        assert_eq!(split_host_port("0.0.0.0:0"), ("0.0.0.0".into(), 0));
        assert_eq!(split_host_port("127.0.0.1"), ("127.0.0.1".into(), 3240));
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[test]
    fn start_stop_and_registry_visibility() {
        let manager = UsbIpManager::new("127.0.0.1:0");
        let rt = runtime();
        manager.start(rt.handle(), vec![cfg(1), cfg(2)]).unwrap();
        assert!(manager.running());
        assert_eq!(manager.cables().len(), 2);
        assert_eq!(manager.cables()[0].bus_id, "1-1");
        assert_eq!(manager.cables()[1].bus_id, "1-2");
        manager.stop();
        assert!(!manager.running());
    }

    #[test]
    fn duplicate_numbers_rejected() {
        let manager = UsbIpManager::new("127.0.0.1:0");
        let rt = runtime();
        let err = manager.start(rt.handle(), vec![cfg(1), cfg(1)]).unwrap_err();
        assert!(err.contains("重复"));
        assert!(!manager.running());
    }

    #[test]
    fn bad_format_rejected() {
        let manager = UsbIpManager::new("127.0.0.1:0");
        let rt = runtime();
        let bad = CableConfig { sample_rate: 8000, ..cfg(1) };
        assert!(manager.start(rt.handle(), vec![bad]).is_err());
        assert!(!manager.running());
    }

    #[test]
    fn restart_replaces_registry() {
        let manager = UsbIpManager::new("127.0.0.1:0");
        let rt = runtime();
        manager.start(rt.handle(), vec![cfg(1)]).unwrap();
        manager.start(rt.handle(), vec![cfg(2), cfg(3)]).unwrap();
        assert_eq!(manager.cables().len(), 2);
        assert!(manager.cables().iter().all(|c| c.cfg.number != 1));
        manager.stop();
    }
}
