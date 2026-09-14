//! 平台音频后端抽象。
//!
//! 每个平台（Windows WASAPI / Linux PulseAudio/PipeWire / macOS CoreAudio）
//! 提供一个 `AudioBackend` 实现；引擎只与本 trait 交互。
//!
//! 流式接口采用"回调"形式：
//! - 采集：后端驱动线程把 interleaved f32 帧推给回调
//! - 渲染：后端驱动线程在需要数据时调用回调填充
//! 这样混音逻辑（pull 式混音）完全留在 core，后端只负责设备 IO。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;

use crate::error::{Error, Result};
use crate::model::DeviceInfo;

/// 设备流参数（共享模式引擎格式）
#[derive(Debug, Clone, Copy)]
pub struct StreamInfo {
    /// 采样率 Hz（WASAPI 共享模式即 GetMixFormat 的采样率）
    pub sample_rate: u32,
    /// 通道数
    pub channels: u16,
}

/// 采集回调：每次收到 interleaved f32 帧块（采集线程调用，禁止阻塞）。
pub type CaptureCallback = Box<dyn FnMut(&[f32]) + Send>;
/// 渲染回调：把 interleaved f32 写进给定缓冲（渲染线程按需调用，禁止阻塞）。
pub type RenderCallback = Box<dyn FnMut(&mut [f32]) + Send>;

/// 可停止的流句柄。Drop 时置位停止标志并等待线程退出。
pub struct StreamHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StreamHandle {
    /// 在独立线程启动 `f`，并把停止标志交给它轮询（协作式退出）。
    pub fn spawn(
        stop: Arc<AtomicBool>,
        thread: std::thread::Builder,
        f: impl FnOnce(Arc<AtomicBool>) + Send + 'static,
    ) -> crate::error::Result<Self> {
        let stop2 = stop.clone();
        let handle = thread
            .spawn(move || f(stop2))
            .map_err(crate::Error::backend)?;
        Ok(Self {
            stop,
            thread: Some(handle),
        })
    }

    /// 请求流线程停止（不等待；等待由 [`Drop`] 完成）。
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// 停止标志是否已置位（请求停止 ≠ 线程已退出）。
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.request_stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// 已启动的流：参数 + 停止句柄
pub struct StartedStream {
    pub info: StreamInfo,
    pub handle: StreamHandle,
}

/// 平台音频后端 trait（每个平台一个实现，见模块文档）。
pub trait AudioBackend: Send + Sync {
    /// 枚举当前活动设备
    fn enumerate_devices(&self) -> Result<Vec<DeviceInfo>>;

    /// 打开采集流（`device_id` 为录入设备；loopback 模式由 mode 之外的调用方区分，
    /// 见 `start_loopback`）。数据为 interleaved f32。
    fn start_capture(&self, device_id: &str, on_data: CaptureCallback) -> Result<StartedStream>;

    /// loopback 采集：捕获输出设备上系统正在播放的声音（仅输出设备支持）。
    fn start_loopback(&self, device_id: &str, on_data: CaptureCallback) -> Result<StartedStream>;

    /// 打开渲染流，后端在线程中按需调用 `on_fill` 填充 interleaved f32。
    fn start_render(&self, device_id: &str, on_fill: RenderCallback) -> Result<StartedStream>;

    /// 后端名称（诊断用）
    fn name(&self) -> &'static str {
        "generic"
    }
}

/// 组合后端：把多个子后端（如 WASAPI + 内置 USB/IP 虚拟声卡）拼成一个 [`AudioBackend`]。
///
/// - `enumerate_devices` 按子后端顺序拼接设备列表（**重复 id 保留先出现的那个**），
///   同时建立 `device_id → 子后端` 路由表；
/// - `start_capture` / `start_loopback` / `start_render` 按路由表把调用分发给
///   拥有该设备的子后端；路由表未命中时先重新枚举一次再判定。
///
/// 单个子后端枚举失败只记警告并跳过，不影响其它子后端的设备（虚拟声卡后端不可用
/// 不应导致物理设备也消失）。
pub struct CompositeBackend {
    backends: Vec<Arc<dyn AudioBackend>>,
    /// device_id → 子后端下标（最近一次枚举建立）
    routes: RwLock<HashMap<String, usize>>,
}

impl CompositeBackend {
    /// 按优先顺序组合子后端（先注册的设备在重复 id 时胜出）。
    pub fn new(backends: Vec<Arc<dyn AudioBackend>>) -> Self {
        Self {
            backends,
            routes: RwLock::new(HashMap::new()),
        }
    }

    /// 子后端列表（按注册顺序）。
    pub fn backends(&self) -> &[Arc<dyn AudioBackend>] {
        &self.backends
    }

    /// 重新枚举全部子后端并重建路由表，返回拼接后的设备列表。
    pub fn refresh(&self) -> Result<Vec<DeviceInfo>> {
        let mut devices: Vec<DeviceInfo> = Vec::new();
        let mut routes: HashMap<String, usize> = HashMap::new();
        for (idx, backend) in self.backends.iter().enumerate() {
            match backend.enumerate_devices() {
                Ok(found) => {
                    for dev in found {
                        if routes.contains_key(&dev.id) {
                            // 前一个子后端优先（物理设备先于虚拟设备）
                            continue;
                        }
                        routes.insert(dev.id.clone(), idx);
                        devices.push(dev);
                    }
                }
                Err(e) => {
                    tracing::warn!("子后端 {} 枚举失败，已跳过: {e}", backend.name());
                }
            }
        }
        *self.routes.write().unwrap() = routes;
        Ok(devices)
    }

    /// 拥有该设备的子后端；未命中路由表时重新枚举后再查一次。
    pub fn owner(&self, device_id: &str) -> Option<Arc<dyn AudioBackend>> {
        if let Some(&idx) = self.routes.read().unwrap().get(device_id) {
            return self.backends.get(idx).cloned();
        }
        self.refresh().ok()?;
        let idx = *self.routes.read().unwrap().get(device_id)?;
        self.backends.get(idx).cloned()
    }

    fn require(&self, device_id: &str) -> Result<Arc<dyn AudioBackend>> {
        self.owner(device_id)
            .ok_or_else(|| Error::DeviceNotFound(device_id.to_string()))
    }

    /// 子后端名称（诊断/状态页用）
    pub fn owner_name(&self, device_id: &str) -> Option<&'static str> {
        self.owner(device_id).map(|b| b.name())
    }
}

impl Default for CompositeBackend {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl AudioBackend for CompositeBackend {
    fn enumerate_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.refresh()
    }

    fn start_capture(&self, device_id: &str, on_data: CaptureCallback) -> Result<StartedStream> {
        self.require(device_id)?.start_capture(device_id, on_data)
    }

    fn start_loopback(&self, device_id: &str, on_data: CaptureCallback) -> Result<StartedStream> {
        self.require(device_id)?.start_loopback(device_id, on_data)
    }

    fn start_render(&self, device_id: &str, on_fill: RenderCallback) -> Result<StartedStream> {
        self.require(device_id)?.start_render(device_id, on_fill)
    }

    fn name(&self) -> &'static str {
        "composite"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeviceKind;
    use crate::testing::FakeBackend;

    fn device(id: &str, kind: DeviceKind) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: format!("Fake {id}"),
            kind,
            is_default: false,
            is_virtual: true,
            channels: 2,
            sample_rate: 48_000,
        }
    }

    /// 造一个只含指定设备的 FakeBackend（`new()` 的设备 id 固定，会互相重复）
    fn fake(id: &str, kind: DeviceKind) -> Arc<FakeBackend> {
        FakeBackend::with_devices(vec![device(id, kind)])
    }

    fn composite(a: Arc<FakeBackend>, b: Arc<FakeBackend>) -> CompositeBackend {
        CompositeBackend::new(vec![a, b])
    }

    #[test]
    fn enumerate_concatenates_all_backends() {
        let c = composite(fake("wasapi-out", DeviceKind::Output), fake("usbip-in", DeviceKind::Input));
        let devices = c.enumerate_devices().unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].id, "wasapi-out");
        assert_eq!(devices[1].id, "usbip-in");
        assert_eq!(c.name(), "composite");
    }

    #[test]
    fn enumerate_dedupes_keeping_first_backend() {
        let a = fake("dup", DeviceKind::Output);
        let b = fake("dup", DeviceKind::Input);
        let c = composite(a, b);
        let devices = c.enumerate_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].kind, DeviceKind::Output);
    }

    #[test]
    fn capture_routes_to_owning_backend() {
        let a = fake("wasapi-out", DeviceKind::Output);
        let b = fake("usbip-in", DeviceKind::Input);
        let c = composite(a.clone(), b.clone());
        c.enumerate_devices().unwrap();

        let got = Arc::new(std::sync::Mutex::new(Vec::<f32>::new()));
        b.set_capture_data("usbip-in", vec![0.5, -0.5]);
        let got2 = got.clone();
        let stream = c
            .start_capture("usbip-in", Box::new(move |d| got2.lock().unwrap().extend_from_slice(d)))
            .unwrap();
        assert_eq!(stream.info.sample_rate, 48_000);
        assert_eq!(b.captures_started.load(Ordering::SeqCst), 1);
        assert_eq!(a.captures_started.load(Ordering::SeqCst), 0, "不应打到第一个后端");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while got.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline, "采集回调未触发");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(stream);
        assert_eq!(b.alive_captures(), 0);
    }

    #[test]
    fn render_routes_to_owning_backend() {
        let a = fake("wasapi-out", DeviceKind::Output);
        let b = fake("usbip-cap", DeviceKind::Output);
        let c = composite(a.clone(), b.clone());
        c.enumerate_devices().unwrap();

        let stream = c.start_render("usbip-cap", Box::new(|buf| buf.fill(0.25))).unwrap();
        assert_eq!(b.renders_started.load(Ordering::SeqCst), 1);
        assert_eq!(a.renders_started.load(Ordering::SeqCst), 0);
        drop(stream);
        assert_eq!(b.alive_renders(), 0);
    }

    #[test]
    fn loopback_routes_to_owning_backend() {
        let a = fake("wasapi-out", DeviceKind::Output);
        let b = fake("usbip-out", DeviceKind::Output);
        let c = composite(a.clone(), b.clone());
        let stream = c.start_loopback("usbip-out", Box::new(|_| {})).unwrap();
        assert_eq!(b.captures_started.load(Ordering::SeqCst), 1);
        assert_eq!(a.captures_started.load(Ordering::SeqCst), 0);
        drop(stream);
    }

    #[test]
    fn routing_works_without_prior_enumerate() {
        // 未枚举过时（路由表为空）应自动重新枚举
        let c = composite(fake("wasapi-out", DeviceKind::Output), fake("usbip-in", DeviceKind::Input));
        let stream = c.start_capture("usbip-in", Box::new(|_| {})).unwrap();
        drop(stream);
    }

    #[test]
    fn unknown_device_is_not_found() {
        let c = composite(fake("wasapi-out", DeviceKind::Output), fake("usbip-in", DeviceKind::Input));
        assert!(matches!(
            c.start_capture("nope", Box::new(|_| {})),
            Err(Error::DeviceNotFound(_))
        ));
        assert!(matches!(
            c.start_render("nope", Box::new(|_| {})),
            Err(Error::DeviceNotFound(_))
        ));
        assert!(c.owner_name("nope").is_none());
    }

    #[test]
    fn owner_name_reports_source_backend() {
        let c = composite(fake("wasapi-out", DeviceKind::Output), fake("usbip-in", DeviceKind::Input));
        c.enumerate_devices().unwrap();
        assert_eq!(c.owner_name("wasapi-out"), Some("fake"));
        assert_eq!(c.owner_name("usbip-in"), Some("fake"));
    }

    #[test]
    fn empty_composite_is_usable() {
        let c = CompositeBackend::default();
        assert!(c.enumerate_devices().unwrap().is_empty());
        assert!(c.backends().is_empty());
    }
}
