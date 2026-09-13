//! Tauri invoke 命令：与控制 API 共享同一个引擎，无重复逻辑。

use std::collections::HashMap;

use audiomix_backend_windows::driver;
use audiomix_backend_windows::usbip::{attach as usbip_attach, cable_configs};use audiomix_core::engine::EngineStats;
use audiomix_core::model::NodePos;
use audiomix_core::{
    DeviceInfo, DeviceKind, GraphConfig, Settings, UsbIpCableSettings, UsbIpSettings,
};
use serde_json::json;
use tauri::{AppHandle, Manager, State};

use crate::config;
use crate::state::AppState;
use crate::tray;

// ---------- 设备 ----------

#[tauri::command]
pub fn list_devices(state: State<AppState>) -> Vec<DeviceInfo> {
    state.engine.list_devices()
}

#[tauri::command]
pub fn refresh_devices(app: AppHandle, state: State<AppState>) -> Result<Vec<DeviceInfo>, String> {
    // 虚拟线路接入时 Windows 有可能把系统默认设备抢过去（用户反馈过），
    // 每次刷新设备都顺手检查一次并恢复用户的选择。
    let (devices, saved) = refresh_and_guard_defaults(&state, None)?;
    if saved {
        persist(&app, &state)?;
    }
    Ok(devices)
}

/// 枚举设备；若某个 flow 的默认设备被**虚拟线路**抢走，则恢复用户选择
/// （没有历史选择时退回到第一个非虚拟设备）。
/// `just_set` 是刚刚由用户显式指定的设备（用于把选择记进配置）。
/// 返回 (设备列表, 配置是否有改动需要落盘)。
fn refresh_and_guard_defaults(
    state: &AppState,
    just_set: Option<&DeviceInfo>,
) -> Result<(Vec<DeviceInfo>, bool), String> {
    let devices = state.engine.refresh_devices().map_err(|e| e.to_string())?;
    let mut saved = false;

    // 1) 用户刚选过：记住偏好
    if let Some(d) = just_set {
        let mut cfg = state.config.lock();
        let slot = match d.kind {
            DeviceKind::Output => &mut cfg.settings.default_output,
            DeviceKind::Input => &mut cfg.settings.default_input,
        };
        if slot.as_deref() != Some(d.id.as_str()) {
            *slot = Some(d.id.clone());
            saved = true;
        }
    }

    // 2) 默认设备被虚拟线路抢走 → 恢复
    let mut restore: Vec<(DeviceKind, String)> = Vec::new();
    {
        let mut cfg = state.config.lock();
        for kind in [DeviceKind::Output, DeviceKind::Input] {
            let saved_id = match kind {
                DeviceKind::Output => cfg.settings.default_output.clone(),
                DeviceKind::Input => cfg.settings.default_input.clone(),
            };
            let Some(current) = devices.iter().find(|d| d.kind == kind && d.is_default) else {
                continue;
            };
            // 当前默认是物理设备：采纳它作为偏好（下次接入虚拟线路时用它恢复）
            if !current.is_virtual {
                if saved_id.as_deref() != Some(current.id.as_str()) {
                    let slot = match kind {
                        DeviceKind::Output => &mut cfg.settings.default_output,
                        DeviceKind::Input => &mut cfg.settings.default_input,
                    };
                    *slot = Some(current.id.clone());
                    saved = true;
                }
                continue;
            }
            // 当前默认是虚拟线路：如果就是用户自己选的（= 保存的偏好），尊重选择不动
            if saved_id.as_deref() == Some(current.id.as_str()) {
                continue;
            }
            // 否则视为 Windows 抢过去的：优先回到记住的物理设备，没有就挑第一个非虚拟设备
            let target = saved_id
                .as_deref()
                .and_then(|id| devices.iter().find(|d| d.id == id && d.kind == kind && !d.is_virtual))
                .or_else(|| devices.iter().find(|d| d.kind == kind && !d.is_virtual));
            let Some(t) = target else { continue };
            restore.push((kind, t.id.clone()));
            let slot = match kind {
                DeviceKind::Output => &mut cfg.settings.default_output,
                DeviceKind::Input => &mut cfg.settings.default_input,
            };
            if slot.as_deref() != Some(t.id.as_str()) {
                *slot = Some(t.id.clone());
                saved = true;
            }
        }
    }
    if restore.is_empty() {
        return Ok((devices, saved));
    }
    let mut changed = false;
    for (kind, id) in &restore {
        match audiomix_backend_windows::policy::set_default_endpoint(id) {
            Ok(()) => {
                changed = true;
                tracing::info!("默认{kind:?}设备被虚拟线路抢占，已恢复为 {id}");
            }
            Err(e) => tracing::warn!("恢复默认{kind:?}设备失败: {e}"),
        }
    }
    if changed {
        let devices = state.engine.refresh_devices().map_err(|e| e.to_string())?;
        Ok((devices, saved))
    } else {
        Ok((devices, saved))
    }
}

/// 常驻的「默认设备守护」。
///
/// 现象：虚拟线路**接入系统后几秒内**，Windows 会把系统默认播放/录音设备都抢成它
/// （实测复现）。这里只在「刚发现有新的虚拟端点接入」后的一个短窗口内纠正，
/// 窗口过去就不再干预 —— 这样用户在 Windows 里主动把默认设成线路时不会被我们反复改掉。
pub fn spawn_default_device_guard(app: AppHandle) {
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    /// 接入后守护窗口长度
    const GUARD_WINDOW: Duration = Duration::from_secs(30);

    std::thread::spawn(move || {
        let mut known: HashSet<String> = HashSet::new();
        let mut guard_until: Option<Instant> = None;
        let mut ticks: u64 = 0;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            ticks += 1;

            // 每 ~15 秒顺手做一次「重复附加」自检：
            // 每次 `usbip attach` 都会新占一个 vhci 端口 → Windows 里多出一对
            // 「扬声器/麦克风 (N- Virtual Cable …)」端点。端口数超过线缆数就断开重来。
            if ticks % 15 == 0 {
                let state = app.state::<AppState>();
                let cables = state.usbip.cables().len();
                if cables > 0 {
                    if let Ok(ports) = usbip_attach::ports() {
                        if ports.len() > cables {
                            tracing::warn!(
                                "检测到 {} 个已附加端口但只有 {cables} 条线缆（重复 attach），先全部断开",
                                ports.len()
                            );
                            let _ = usbip_attach::detach_all();
                        }
                    }
                }
            }

            // 枚举一次设备，看有没有新的虚拟端点（虚拟线路每次重连都会换端点 id）
            let devices = audiomix_backend_windows::wasapi::device::enumerate_devices()
                .unwrap_or_default();
            let virtual_ids: HashSet<String> = devices
                .iter()
                .filter(|d| d.is_virtual)
                .map(|d| d.id.clone())
                .collect();
            let just_arrived = !virtual_ids.is_subset(&known);
            known = virtual_ids;
            if just_arrived {
                guard_until = Some(Instant::now() + GUARD_WINDOW);
                // 线缆刚接入：让引擎重新对齐流，补启启动时因设备不存在而被跳过的
                // source/sink（否则混音页的路由会一直没声音）。
                let state = app.state::<AppState>();
                if let Err(e) = state.engine.refresh_devices() {
                    tracing::debug!("虚拟线路接入后刷新设备失败: {e}");
                }
            }
            let in_window = guard_until.map(|t| Instant::now() < t).unwrap_or(false);
            if !in_window {
                continue;
            }
            // 只有默认端点真的被虚拟设备占了才动手
            let stolen = [true, false].into_iter().any(|render| {
                audiomix_backend_windows::wasapi::device::default_endpoint_is_virtual(render)
            });
            if !stolen {
                continue;
            }
            let state = app.state::<AppState>();
            match refresh_and_guard_defaults(&state, None) {
                Ok((_, saved)) => {
                    if saved {
                        let _ = persist(&app, &state);
                    }
                }
                Err(e) => tracing::debug!("默认设备守护检查失败: {e}"),
            }
        }
    });
}

/// 把某个端点设为 Windows 默认设备（播放/录音都适用；三种角色一并设置），
/// 并把这次选择记进配置（虚拟线路接入时用它恢复）。
/// 返回刷新后的设备列表，便于前端立即反映 `is_default`。
#[tauri::command]
pub fn set_default_device(    app: AppHandle,
    state: State<AppState>,
    device_id: String,
) -> Result<Vec<DeviceInfo>, String> {
    audiomix_backend_windows::policy::set_default_endpoint(&device_id)?;
    let devices = state.engine.refresh_devices().map_err(|e| e.to_string())?;
    let picked = devices.iter().find(|d| d.id == device_id).cloned();
    if picked.is_none() {
        return Ok(devices); // 设备刚拔掉：Windows 那边已尽力，列表照原样返回
    }
    let (devices, saved) = refresh_and_guard_defaults(&state, picked.as_ref())?;
    if saved {
        persist(&app, &state)?;
    }
    Ok(devices)
}

/// 读取某输出/输入端点的 **Windows 系统音量**（0.0..=1.0）
#[tauri::command]
pub fn get_device_volume(device_id: String) -> Result<f32, String> {
    audiomix_backend_windows::policy::get_endpoint_volume(&device_id)
}

/// 设置端点的 **Windows 系统音量**（影响该设备上所有声音，不只是混音器输出）
#[tauri::command]
pub fn set_device_volume(device_id: String, level: f32) -> Result<(), String> {
    audiomix_backend_windows::policy::set_endpoint_volume(&device_id, level)
}

/// 端点是否静音
#[tauri::command]
pub fn get_device_mute(device_id: String) -> Result<bool, String> {
    audiomix_backend_windows::policy::get_endpoint_mute(&device_id)
}

/// 设置端点静音
#[tauri::command]
pub fn set_device_mute(device_id: String, mute: bool) -> Result<(), String> {
    audiomix_backend_windows::policy::set_endpoint_mute(&device_id, mute)
}

// ---------- 混音图 ----------

/// 混音画布节点位置（node key → 归一化坐标）
#[tauri::command]
pub fn get_mixer_layout(state: State<AppState>) -> HashMap<String, NodePos> {
    state.config.lock().layout.clone()
}

/// 保存混音画布节点位置（纯前端布局，与图一起持久化）。
///
/// 参数故意收 `serde_json::Value` 而不是 `HashMap<String, NodePos>`：
/// 前端一旦出现 `NaN`（JSON 里变成 `null`）就会让 f64 反序列化整体失败，
/// 这里改成逐项校验、丢弃坏项，避免一个坏节点位置把整次保存打挂。
#[tauri::command]
pub fn set_mixer_layout(
    app: AppHandle,
    state: State<AppState>,
    layout: serde_json::Value,
) -> Result<(), String> {
    let mut clean: HashMap<String, NodePos> = HashMap::new();
    if let Some(obj) = layout.as_object() {
        for (key, value) in obj {
            let Some(arr) = value.as_array() else { continue };
            if arr.len() < 2 {
                continue;
            }
            let (Some(x), Some(y)) = (arr[0].as_f64(), arr[1].as_f64()) else {
                continue;
            };
            if !x.is_finite() || !y.is_finite() {
                continue;
            }
            clean.insert(key.clone(), [x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)]);
        }
    }
    {
        let mut cfg = state.config.lock();
        cfg.layout = clean;
    }
    persist(&app, &state)
}

#[tauri::command]
pub fn get_graph(state: State<AppState>) -> GraphConfig {
    state.engine.get_graph()
}

#[tauri::command]
pub fn apply_graph(
    app: AppHandle,
    state: State<AppState>,
    graph: GraphConfig,
) -> Result<GraphConfig, String> {
    state
        .engine
        .apply_graph(graph)
        .map_err(|e| e.to_string())?;
    let applied = state.engine.get_graph();
    {
        let mut cfg = state.config.lock();
        cfg.graph = applied.clone();
    }
    persist(&app, &state)?;
    Ok(applied)
}

#[tauri::command]
pub fn set_route_gain(
    app: AppHandle,
    state: State<AppState>,
    route_id: String,
    gain: f32,
) -> Result<(), String> {
    state
        .engine
        .set_route_gain(&route_id, gain)
        .map_err(|e| e.to_string())?;
    {
        let mut cfg = state.config.lock();
        cfg.graph = state.engine.get_graph();
    }
    persist(&app, &state)
}

#[tauri::command]
pub fn set_route_muted(
    app: AppHandle,
    state: State<AppState>,
    route_id: String,
    muted: bool,
) -> Result<(), String> {
    state
        .engine
        .set_route_muted(&route_id, muted)
        .map_err(|e| e.to_string())?;
    {
        let mut cfg = state.config.lock();
        cfg.graph = state.engine.get_graph();
    }
    persist(&app, &state)
}

#[tauri::command]
pub fn set_sink_volume(
    app: AppHandle,
    state: State<AppState>,
    sink_id: String,
    volume: f32,
) -> Result<(), String> {
    state
        .engine
        .set_sink_volume(&sink_id, volume)
        .map_err(|e| e.to_string())?;
    {
        let mut cfg = state.config.lock();
        cfg.graph = state.engine.get_graph();
    }
    persist(&app, &state)
}

#[tauri::command]
pub fn get_levels(state: State<AppState>) -> HashMap<String, f32> {
    state.engine.levels()
}

#[tauri::command]
pub fn get_stats(state: State<AppState>) -> EngineStats {
    state.engine.stats()
}

// ---------- 设置 ----------

#[tauri::command]
pub fn get_settings(state: State<AppState>) -> Settings {
    state.config.lock().settings.clone()
}

#[tauri::command]
pub fn update_settings(app: AppHandle, state: State<AppState>, settings: Settings) -> Result<(), String> {
    {
        let mut cfg = state.config.lock();
        cfg.settings = settings.clone();
    }
    // 控制 API 开关/端口变化 → 重启服务
    restart_control_api(&state)?;
    // 自启参数可能变化（headless 偏好）→ 若已开启自启则重新注册
    if crate::autostart::get() {
        crate::autostart::set(true, settings.autostart_headless)?;
    }
    persist(&app, &state)
}

// ---------- 控制 API ----------

#[tauri::command]
pub fn get_control_api_status(state: State<AppState>) -> serde_json::Value {
    let api = state.api.lock();
    json!({
        "running": api.is_some(),
        "addr": api.as_ref().map(|a| a.addr.to_string()),
    })
}

#[tauri::command]
pub fn set_control_api_enabled(app: AppHandle, state: State<AppState>, enabled: bool) -> Result<(), String> {
    {
        let mut cfg = state.config.lock();
        cfg.settings.control_api.enabled = enabled;
    }
    restart_control_api(&state)?;
    persist(&app, &state)
}

/// 启动/停止控制 API，使其与 settings.control_api 一致
pub fn restart_control_api(state: &State<AppState>) -> Result<(), String> {
    let (enabled, bind, port) = {
        let cfg = state.config.lock();
        let api = &cfg.settings.control_api;
        (api.enabled, api.bind.clone(), api.port)
    };
    // 先停旧的
    if let Some(old) = state.api.lock().take() {
        old.stop();
    }
    if !enabled {
        return Ok(());
    }
    let engine = state.engine.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    tauri::async_runtime::spawn(async move {
        let result = audiomix_control_api::spawn(engine, &bind, port).await;
        let _ = tx.send(result);
    });
    let server = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| "控制 API 启动超时".to_string())??;
    *state.api.lock() = Some(std::sync::Arc::new(server));
    Ok(())
}

// ---------- 自启动 ----------

#[tauri::command]
pub fn get_autostart() -> bool {
    crate::autostart::get()
}

#[tauri::command]
pub fn set_autostart(state: State<AppState>, enabled: bool) -> Result<(), String> {
    let headless = state.config.lock().settings.autostart_headless;
    crate::autostart::set(enabled, headless)
}

// ---------- USB/IP 虚拟声卡（usbip-win2 + UAC2）----------

/// 一条线缆的运行态（UI 用）
#[derive(Debug, Clone, serde::Serialize)]
pub struct UsbIpCableStatus {
    pub number: u8,
    /// 自定义名（用户输入原值，可为空）
    pub name: String,
    /// 实际显示名（空名回退 Virtual Cable NN）
    pub display_name: String,
    /// USB 音频类版本："uac1"（usbaudio.sys，全速，兼容优先）| "uac2"（usbaudio2.sys，高速）
    pub protocol: String,
    pub sample_rate: u32,
    pub bits: u16,
    /// "loopback" | "mixer"
    pub mode: String,
    pub buffer_ms: u32,
    /// 混音图引用的两个设备 id
    pub device_id_playback: String,
    pub device_id_capture: String,
    /// 是否已接入系统（按 busid 匹配 usbip port）
    pub attached: bool,
    pub port: Option<u32>,
}

/// usbip.exe / 传输驱动状态
#[derive(Debug, Clone, serde::Serialize)]
pub struct UsbIpDriverInfo {
    pub installed: bool,
    pub usbip_path: Option<String>,
    /// 随包安装包路径（未找到为 null）
    pub installer_path: Option<String>,
    pub test_signing: Option<bool>,
    pub hvci_enabled: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UsbIpStatus {
    pub enabled: bool,
    pub running: bool,
    pub bind: String,
    pub local_addr: Option<String>,
    pub cables: Vec<UsbIpCableStatus>,
    pub driver: UsbIpDriverInfo,
    /// `usbip port` 解析结果
    pub ports: Vec<usbip_attach::AttachedPort>,
    /// `usbip port` 失败原因（多为需要管理员权限）
    pub ports_error: Option<String>,
}

fn build_usbip_status(
    enabled: bool,
    bind: String,
    running: bool,
    local_addr: Option<String>,
    cables: &[UsbIpCableSettings],
) -> UsbIpStatus {
    let (ports, ports_error) = match usbip_attach::ports() {
        Ok(p) => (p, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let cable_status = cables
        .iter()
        .map(|c| {
            let bus_id = format!("1-{}", c.number);
            let hit = ports.iter().find(|p| p.bus_id.as_deref() == Some(bus_id.as_str()));
            UsbIpCableStatus {
                number: c.number,
                sample_rate: c.sample_rate,
                bits: c.bits,
                mode: match c.mode {
                    audiomix_core::UsbIpCableMode::Loopback => "loopback".into(),
                    audiomix_core::UsbIpCableMode::Reverse => "reverse".into(),
                    audiomix_core::UsbIpCableMode::Mixer => "mixer".into(),
                },
                protocol: match c.protocol {
                    audiomix_core::UsbIpCableProtocol::Uac1 => "uac1".into(),
                    audiomix_core::UsbIpCableProtocol::Uac2 => "uac2".into(),
                },
                buffer_ms: c.buffer_ms,
                device_id_playback: format!("usbip://{}/playback", c.number),
                device_id_capture: format!("usbip://{}/capture", c.number),
                name: c.name.clone(),
                display_name: c.display_name(),
                attached: hit.is_some(),
                port: hit.map(|p| p.port),
            }
        })
        .collect();

    UsbIpStatus {
        enabled,
        running,
        bind,
        local_addr,
        cables: cable_status,
        driver: UsbIpDriverInfo {
            installed: usbip_attach::installed(),
            usbip_path: usbip_attach::find_usbip().map(|p| p.to_string_lossy().to_string()),
            installer_path: usbip_attach::bundled_installer().map(|p| p.to_string_lossy().to_string()),
            test_signing: driver::test_signing_enabled(),
            hvci_enabled: driver::hvci_enabled(),
        },
        ports,
        ports_error,
    }
}

/// 查询 USB/IP 虚拟声卡状态（会执行 `usbip port`，放到阻塞线程）
#[tauri::command]
pub async fn usbip_status(state: State<'_, AppState>) -> Result<UsbIpStatus, String> {
    let (enabled, bind, cables) = {
        let cfg = state.config.lock();
        (
            cfg.settings.usbip.enabled,
            cfg.settings.usbip.bind.clone(),
            cfg.settings.usbip.cables.clone(),
        )
    };
    let running = state.usbip.running();
    let local_addr = state.usbip.local_addr().map(|a| a.to_string());
    tauri::async_runtime::spawn_blocking(move || {
        build_usbip_status(enabled, bind, running, local_addr, &cables)
    })
    .await
    .map_err(|e| format!("状态查询失败: {e}"))
}

/// 保存线缆配置并（重）启服务器。返回新状态。
///
/// 服务器已在运行时**不重绑端口**，只替换线缆表（改格式/改名要重新 attach 才生效，
/// UI 会自动重新附加）。启动失败会回滚到上一次的线缆表，避免把在跑的服务停在半路。
#[tauri::command]
pub async fn usbip_set_cables(
    app: AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
    cables: Vec<UsbIpCableSettings>,
) -> Result<UsbIpStatus, String> {
    let bind = state.config.lock().settings.usbip.bind.clone();
    let settings = UsbIpSettings {
        enabled,
        bind: bind.clone(),
        cables,
    };
    settings.validate().map_err(|e| e.to_string())?;

    let (previous, previous_enabled) = {
        let cfg = state.config.lock();
        (cable_configs(&cfg.settings.usbip), cfg.settings.usbip.enabled)
    };
    let manager = state.usbip.clone();
    // RuntimeHandle 是临时值，.inner() 借它；闭包要求 'static，这里克隆出 tokio Handle（廉价 Arc 克隆）
    let rt_owner = tauri::async_runtime::handle();
    let rt = rt_owner.inner().clone();
    let new_configs = cable_configs(&settings);
    let wants_enabled = settings.enabled;

    // 放到阻塞线程：start/stop 内部要等旧 accept 任务退出并重试绑定，最多阻塞约 1 秒
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        if !wants_enabled {
            manager.stop();
            return Ok(());
        }
        match manager.start(&rt, new_configs) {
            Ok(()) => Ok(()),
            Err(e) => {
                if previous_enabled && !previous.is_empty() {
                    match manager.start(&rt, previous) {
                        Ok(()) => tracing::warn!("USB/IP 服务器启动失败（{e}），已回滚到上次的线缆配置"),
                        Err(e2) => tracing::error!("USB/IP 服务器启动失败（{e}），回滚亦失败: {e2}"),
                    }
                }
                Err(e)
            }
        }
    })
    .await
    .map_err(|e| format!("USB/IP 任务失败: {e}"))?;
    outcome.map_err(|e| format!("启动 USB/IP 服务器失败: {e}"))?;

    {
        let mut cfg = state.config.lock();
        cfg.settings.usbip = settings.clone();
    }
    persist(&app, &state)?;
    // 设备列表随线缆变化
    let _ = state.engine.refresh_devices();

    let running = state.usbip.running();
    let local_addr = state.usbip.local_addr().map(|a| a.to_string());
    tauri::async_runtime::spawn_blocking(move || {
        build_usbip_status(enabled, bind, running, local_addr, &settings.cables)
    })
    .await
    .map_err(|e| format!("状态查询失败: {e}"))
}

/// 重新附加全部线缆（提权：先 detach --all，再逐条 attach）
#[tauri::command]
pub async fn usbip_attach_all(state: State<'_, AppState>) -> Result<usbip_attach::AttachReport, String> {
    let Some(addr) = state.usbip.local_addr() else {
        return Err("USB/IP 服务器未运行——请先启用虚拟声卡并保存线缆".into());
    };
    let bus_ids: Vec<String> = state.usbip.cables().iter().map(|c| c.bus_id.clone()).collect();
    if bus_ids.is_empty() {
        return Err("尚未配置任何虚拟线缆".into());
    }
    let (host, port) = (addr.ip().to_string(), addr.port());
    let report = tauri::async_runtime::spawn_blocking(move || {
        usbip_attach::attach_all(&host, port, &bus_ids)
    })
    .await
    .map_err(|e| format!("附加任务失败: {e}"))??;
    let _ = state.engine.refresh_devices();
    Ok(report)
}

/// 断开全部已接入的线缆（提权）
#[tauri::command]
pub async fn usbip_detach_all(state: State<'_, AppState>) -> Result<String, String> {
    let out = tauri::async_runtime::spawn_blocking(usbip_attach::detach_all)
        .await
        .map_err(|e| format!("断开任务失败: {e}"))??;
    let _ = state.engine.refresh_devices();
    Ok(out)
}

/// 安装随包捆绑的 usbip-win2（提权静默安装，需要 UAC）
#[tauri::command]
pub async fn usbip_install_driver() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(usbip_attach::install_bundled)
        .await
        .map_err(|e| format!("安装任务失败: {e}"))?
}

// ---------- 运行日志（设置页右侧面板） ----------

/// 增量取日志：`since` 为已拿到的最大 seq；返回 { lines, next }
#[tauri::command]
pub fn get_logs(since: u64) -> serde_json::Value {
    let (lines, next) = crate::logbuf::buffer().since(since);
    json!({
        "lines": lines
            .into_iter()
            .map(|l| json!({ "seq": l.seq, "text": l.text }))
            .collect::<Vec<_>>(),
        "next": next,
    })
}

#[tauri::command]
pub fn clear_logs() {
    crate::logbuf::buffer().clear();
}

// ---------- 窗口 ----------

#[tauri::command]
pub fn open_main_window(app: AppHandle) {
    tray::show_main(&app);
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    let state = app.state::<AppState>();
    state.engine.shutdown();
    app.exit(0);
}

fn persist(app: &AppHandle, state: &State<AppState>) -> Result<(), String> {
    let snapshot = state.config.lock().clone();
    config::save(app, &snapshot)
}
