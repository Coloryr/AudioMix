//! Tauri invoke 命令：与控制 API 共享同一个引擎，无重复逻辑。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use audiomix_backend_windows::driver;
use audiomix_backend_windows::usbip::{
    attach as usbip_attach, broker as usbip_broker, cable_configs,
};
use audiomix_core::engine::{EngineEvent, EngineStats};
use audiomix_core::model::NodePos;
use audiomix_core::{
    DeviceInfo, DeviceKind, GraphConfig, Settings, UsbIpCableSettings, UsbIpSettings,
};
use serde_json::json;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};

use crate::config;
use crate::state::{AppState, LevelsPayload};
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

/// 虚拟线路的 Windows 侧真实端点（is_virtual 且非合成 `usbip://` id —— 合成 id 系统不认）。
fn real_virtual_endpoint<'a>(devices: &'a [DeviceInfo], kind: DeviceKind) -> Option<&'a DeviceInfo> {
    devices
        .iter()
        .find(|d| d.kind == kind && d.is_virtual && !d.id.starts_with("usbip://"))
}

fn virtual_flag_mut(cfg: &mut audiomix_core::GraphSettings, kind: DeviceKind) -> &mut bool {
    match kind {
        DeviceKind::Output => &mut cfg.settings.default_output_virtual,
        DeviceKind::Input => &mut cfg.settings.default_input_virtual,
    }
}

fn virtual_flag(cfg: &audiomix_core::GraphSettings, kind: DeviceKind) -> bool {
    match kind {
        DeviceKind::Output => cfg.settings.default_output_virtual,
        DeviceKind::Input => cfg.settings.default_input_virtual,
    }
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

    // 1) 用户刚选过：记住偏好（id + 是否虚拟线路的意图）
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
        let flag = virtual_flag_mut(&mut cfg, d.kind);
        if *flag != d.is_virtual {
            *flag = d.is_virtual;
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
            // 当前默认是物理设备：线缆在 Windows 侧有真实端点且用户偏好是虚拟线路 →
            // 用户在 Windows 里改了主意，更新意图；线缆不在（未接入）时默认必然落在
            // 物理设备上，虚拟偏好保持原样 —— 等线缆接入后由 restore_virtual_defaults 设回
            if !current.is_virtual {
                if real_virtual_endpoint(&devices, kind).is_some()
                    && *virtual_flag_mut(&mut cfg, kind)
                {
                    *virtual_flag_mut(&mut cfg, kind) = false;
                    saved = true;
                }
                if virtual_flag(&cfg, kind) {
                    // 线路不在线、偏好仍是虚拟线路：物理默认只是临时落点，不采纳
                    continue;
                }
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
            // 当前默认是虚拟线路（= 线缆的真实端点，is_default 只有 Windows 侧会标）
            if *virtual_flag_mut(&mut cfg, kind) {
                // 偏好就是虚拟线路：尊重现状，id 跟随当前端点（每次重连都会变）
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
            // 偏好是物理设备：视为 Windows 抢过去的，优先回到记住的物理设备，
            // 没有就挑第一个非虚拟设备
            let target = saved_id
                .as_deref()
                .and_then(|id| {
                    devices
                        .iter()
                        .find(|d| d.id == id && d.kind == kind && !d.is_virtual)
                })
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

/// 用户偏好（意图）是虚拟线路、但当前默认不是：把默认设回该 flow 的虚拟线路的
/// **Windows 侧真实端点**（线缆接入时调用 —— 启动时线缆未接入，默认必然落在物理
/// 设备上，接入后要设回去；端点 id 每次重连都会变，所以按 is_virtual 匹配而不是 id）。
/// 返回是否做了更改（需要落盘）。
fn restore_virtual_defaults(state: &AppState) -> bool {
    let devices = state.engine.list_devices();
    // 先算好要恢复的目标（短暂持锁），恢复动作放在锁外
    let mut targets: Vec<(DeviceKind, String)> = Vec::new();
    {
        let cfg = state.config.lock();
        for kind in [DeviceKind::Output, DeviceKind::Input] {
            if !virtual_flag(&cfg, kind) {
                continue;
            }
            // 目标：线缆的真实端点（合成 usbip:// id 不是 MMDevice id，系统不认）
            let Some(target) = real_virtual_endpoint(&devices, kind) else {
                continue;
            };
            let current = devices
                .iter()
                .find(|d| d.kind == kind && d.is_default)
                .map(|d| d.id.clone());
            if current.as_deref() != Some(target.id.as_str()) {
                targets.push((kind, target.id.clone()));
            }
        }
    }
    if targets.is_empty() {
        return false;
    }
    let mut changed = false;
    for (kind, id) in &targets {
        match audiomix_backend_windows::policy::set_default_endpoint(id) {
            Ok(()) => {
                changed = true;
                tracing::info!("用户默认{kind:?}偏好为虚拟线路，线缆接入后已设回 {id}");
                let mut cfg = state.config.lock();
                let slot = match kind {
                    DeviceKind::Output => &mut cfg.settings.default_output,
                    DeviceKind::Input => &mut cfg.settings.default_input,
                };
                *slot = Some(id.clone());
            }
            Err(e) => tracing::warn!("恢复默认{kind:?}为虚拟线路失败: {e}"),
        }
    }
    if changed {
        // 重新枚举，把新的 is_default 刷进引擎缓存（前端会跟着设备推送刷新）
        let _ = state.engine.refresh_devices();
    }
    changed
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
    /// 重复端口自愈的冷却：attach 要弹 UAC，用户取消后不能 15 秒一弹
    const REPAIR_COOLDOWN: Duration = Duration::from_secs(60);

    use std::sync::Arc;

    std::thread::spawn(move || {
        let mut known: HashSet<String> = HashSet::new();
        let mut guard_until: Option<Instant> = None;
        let mut ticks: u64 = 0;
        let mut last_repair: Option<Instant> = None;
        let repairing = Arc::new(AtomicBool::new(false));
        loop {
            std::thread::sleep(Duration::from_secs(1));
            ticks += 1;

            // 每 ~15 秒顺手做一次「重复附加」自检 + 自愈：
            // vhci 偶尔会自己重复 import（Windows 重新枚举 usbaudio2 时又拉一次），
            // 同一条线缆占两个端口。**只拆多余/失效的端口，保留正常工作的那个**——
            // 不需要 detach --all 重来，正常会话不断、设备端点不消失；
            // 实在分类不了（端口没有 bus_id 等）才退回「全断 + 重新附加」。
            // 清理走提权 broker（在线时零 UAC），所以随时清理都不打扰用户。
            if ticks % 15 == 0 {
                let state = app.state::<AppState>();
                let cable_bus_ids: std::collections::HashSet<String> = state
                    .usbip
                    .cables()
                    .iter()
                    .map(|c| c.bus_id.clone())
                    .collect();
                if !cable_bus_ids.is_empty() {
                    if let Ok(ports) = usbip_attach::ports() {
                        let in_use: Vec<_> = ports.iter().filter(|p| p.in_use).collect();
                        // 同一 bus_id 占多个端口：保留最小端口号（最早附加、在流式的那个）
                        let mut by_bus: std::collections::BTreeMap<String, Vec<u32>> =
                            Default::default();
                        let mut stale: Vec<u32> = Vec::new(); // bus_id 不在线缆表里（线缆已删除）
                        let mut unclassified = 0usize; // bus_id 解析不出来，没法分类
                        for p in &in_use {
                            match &p.bus_id {
                                Some(b) if cable_bus_ids.contains(b) => {
                                    by_bus.entry(b.clone()).or_default().push(p.port);
                                }
                                Some(_) => stale.push(p.port),
                                None => unclassified += 1,
                            }
                        }
                        let mut dup_ports: Vec<u32> = Vec::new();
                        for (bus, mut ps) in by_bus {
                            if ps.len() > 1 {
                                ps.sort_unstable();
                                tracing::warn!(
                                    "线缆 {bus} 占了 {} 个端口（vhci 重复 import），保留端口 {}，拆除 {:?}",
                                    ps.len(),
                                    ps[0],
                                    &ps[1..]
                                );
                                dup_ports.extend_from_slice(&ps[1..]);
                            }
                        }
                        let to_detach: Vec<u32> = dup_ports.iter().chain(&stale).copied().collect();
                        // 有解析不出 bus_id 的占用端口、且总数对不上线缆数 → 没法定位多余的，全断重来
                        let needs_full_repair =
                            unclassified > 0 && in_use.len() > cable_bus_ids.len();
                        if needs_full_repair
                            && last_repair.map_or(true, |t| t.elapsed() > REPAIR_COOLDOWN)
                            && !repairing.load(Ordering::Relaxed)
                        {
                            tracing::warn!(
                                "检测到 {} 个占用中的端口但无法定位多余的（线缆 {} 条），全断后重新附加",
                                in_use.len(),
                                cable_bus_ids.len()
                            );
                            last_repair = Some(Instant::now());
                            repairing.store(true, Ordering::Relaxed);
                            let app2 = app.clone();
                            let repairing2 = repairing.clone();
                            // 附加要弹 UAC、可能等几分钟，放独立线程，别卡住守护循环
                            std::thread::spawn(move || {
                                let result = repair_cables(&app2);
                                repairing2.store(false, Ordering::Relaxed);
                                if let Err(e) = result {
                                    tracing::warn!(
                                        "重复端口自愈失败（冷却 60s 后会自动重试，也可手动点「附加全部」）: {e}"
                                    );
                                }
                            });
                        } else if !to_detach.is_empty()
                            && last_repair.map_or(true, |t| t.elapsed() > REPAIR_COOLDOWN)
                            && !repairing.load(Ordering::Relaxed)
                        {
                            last_repair = Some(Instant::now());
                            repairing.store(true, Ordering::Relaxed);
                            let app2 = app.clone();
                            let repairing2 = repairing.clone();
                            std::thread::spawn(move || {
                                // 优先走提权 broker（零 UAC）；不在时回退一次性提权
                                let result = if usbip_broker::ensure_broker().is_ok() {
                                    usbip_broker::broker_detach_ports(&to_detach)
                                } else {
                                    usbip_attach::detach_ports(&to_detach)
                                };
                                repairing2.store(false, Ordering::Relaxed);
                                let state = app2.state::<AppState>();
                                match result {
                                    Ok(_) => {
                                        let _ = state.engine.refresh_devices();
                                        tracing::info!("重复端口拆除完成，正常会话未受影响");
                                        emit_usbip_status(&app2);
                                    }
                                    Err(e) => tracing::warn!(
                                        "拆除重复端口失败（冷却 60s 后会自动重试）: {e}"
                                    ),
                                }
                            });
                        }
                    }
                }
            }

            // 检查有没有新的虚拟端点（虚拟线路每次重连都会换端点 id）。
            // 读引擎的设备缓存而不自己枚举：引擎看门狗（3s）和前端刷新都会更新缓存，
            // 这里每秒自己再枚举一遍纯属重复的 COM 开销
            let devices = app.state::<AppState>().engine.list_devices();
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
                // 用户此前把虚拟线路设为默认输出的：启动时线缆未接入、默认落在物理设备上，
                // 现在接入了要设回去（含「启动时线缆已在」的情形 —— known 初始为空，这里同样触发）
                if restore_virtual_defaults(&state) {
                    let _ = persist(&app, &state);
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

/// 重复端口自愈：断开全部端口后按当前线缆表重新附加（与手动「附加全部」
/// 同一条路径：detach --all + 逐条 attach，一次 UAC）。服务器没在跑时只断开。
fn repair_cables(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let Some(addr) = state.usbip.local_addr() else {
        let out = detach_via_broker_or_elevated()?;
        tracing::info!("USB/IP 服务器未运行，自愈仅执行断开: {out}");
        return Ok(());
    };
    let bus_ids: Vec<String> = state
        .usbip
        .cables()
        .iter()
        .map(|c| c.bus_id.clone())
        .collect();
    if bus_ids.is_empty() {
        return Ok(());
    }
    let (host, port) = (addr.ip().to_string(), addr.port());
    // broker 在线（或本次 UAC 启动成功）→ 零 UAC；用户取消 UAC 则中止，不回退再弹
    let result = if usbip_broker::ensure_broker().is_ok() {
        usbip_broker::broker_attach_all(&host, port, &bus_ids)
    } else {
        usbip_attach::attach_all(&host, port, &bus_ids)
    };
    tracing::info!("重复端口自愈完成: {result:?}");
    let _ = state.engine.refresh_devices();
    // 无论成败都广播：附加可能部分成功（或自愈只做了断开），前端不能被蒙在鼓里
    emit_usbip_status(app);
    result.map(|_| ())
}

/// 断开全部端口：优先提权 broker（零 UAC），不在时回退一次性提权
fn detach_via_broker_or_elevated() -> Result<String, String> {
    if usbip_broker::ensure_broker().is_ok() {
        usbip_broker::broker_detach_all()
    } else {
        usbip_attach::detach_all()
    }
}

/// 启动时自动恢复线缆附加（放后台线程跑）。
///
/// vhci 的附加状态独立于本应用：上次 attach 过、又没 detach/重启的话，端口还挂着
/// ——这时什么都不做（也就**不弹 UAC**）。只有端口空了（如重启过电脑）才走
/// 「附加全部」路径，弹一次 UAC；用户取消也不影响其他启动流程，等手动附加即可。
pub fn auto_attach_if_needed(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.usbip.cables().is_empty() {
        return;
    }
    let bus_ids: std::collections::HashSet<String> = state
        .usbip
        .cables()
        .iter()
        .map(|c| c.bus_id.clone())
        .collect();
    let attached = usbip_attach::ports().map_or(false, |ports| {
        ports
            .iter()
            .any(|p| p.in_use && p.bus_id.as_deref().map_or(false, |b| bus_ids.contains(b)))
    });
    if attached {
        tracing::info!("线缆仍处于附加状态，启动无需重新附加");
        return;
    }
    tracing::info!("线缆未附加，启动时自动附加（如弹出 UAC 请确认）");
    match repair_cables(app) {
        Ok(()) => tracing::info!("启动自动附加完成"),
        Err(e) => tracing::warn!("启动自动附加未完成（可在「虚拟声卡」页手动点「附加全部」）: {e}"),
    }
}

/// 把某个端点设为 Windows 默认设备（播放/录音都适用；三种角色一并设置），
/// 并把这次选择记进配置（虚拟线路接入时用它恢复）。
/// 返回刷新后的设备列表，便于前端立即反映 `is_default`。
#[tauri::command]
pub fn set_default_device(
    app: AppHandle,
    state: State<AppState>,
    device_id: String,
) -> Result<Vec<DeviceInfo>, String> {
    // 虚拟线路的合成 id（usbip://N/playback|capture）不是 MMDevice 端点 id，
    // IPolicyConfig 不认：映射到线缆在 Windows 侧的真实端点再设默认
    let mut target_id = device_id.clone();
    if let Some(side) = device_id.strip_prefix("usbip://") {
        // playback 侧是录入端（Input），capture 侧是输出端（Output），与枚举口径一致
        let kind = if side.ends_with("/playback") {
            DeviceKind::Input
        } else {
            DeviceKind::Output
        };
        let devices = state.engine.list_devices();
        let real = real_virtual_endpoint(&devices, kind)
            .ok_or_else(|| "虚拟线路未接入系统，无法设为默认设备".to_string())?;
        target_id = real.id.clone();
    }
    audiomix_backend_windows::policy::set_default_endpoint(&target_id)?;
    let devices = state.engine.refresh_devices().map_err(|e| e.to_string())?;
    let picked = devices.iter().find(|d| d.id == target_id).cloned();
    if picked.is_none() {
        return Ok(devices); // 设备刚拔掉：Windows 那边已尽力，列表照原样返回
    }
    let (devices, saved) = refresh_and_guard_defaults(&state, picked.as_ref())?;
    if saved {
        persist(&app, &state)?;
    }
    Ok(devices)
}

/// 合成 `usbip://N/playback|capture` 线路 id → 接入后 Windows 创建的真实端点 id。
///
/// 线路端点本身不是 MMDevice 设备（没有 `{0.0.0.0...}.{guid}` 形式的端点 id），
/// `IMMDeviceEnumerator::GetDevice` 直接报「找不到设备」，音量接口无从打开；
/// 音量控制器在 usbaudio.sys 为线缆创建的**真实**扬声器/麦克风端点上
/// （UAC1 描述符两条路径都带 Feature Unit，所以两个真实端点都有音量）。
/// 普通设备 id 原样透传。
fn real_cable_endpoint_id(state: &AppState, device_id: &str) -> Result<String, String> {
    let Some(rest) = device_id.strip_prefix("usbip://") else {
        return Ok(device_id.to_string());
    };
    let Some((num, side)) = rest.split_once('/') else {
        return Err(format!("无效的线路设备 id: {device_id}"));
    };
    let Ok(number) = num.parse::<u8>() else {
        return Err(format!("无效的线路设备 id: {device_id}"));
    };
    let display = state
        .usbip
        .cables()
        .iter()
        .find(|c| c.cfg.number == number)
        .map(|c| c.cfg.display_name())
        .ok_or_else(|| format!("线路 {number} 不存在（已删除或服务器未启用）"))?;
    // 线路的播放端 = Windows 扬声器（Output），采集端 = 麦克风（Input）；
    // 真实端点名里含线缆显示名（与前端 realCableDevice 同一判据）
    let want_kind = if side == "playback" {
        DeviceKind::Output
    } else {
        DeviceKind::Input
    };
    state
        .engine
        .list_devices()
        .into_iter()
        .find(|d| d.kind == want_kind && !d.id.starts_with("usbip://") && d.name.contains(&display))
        .map(|d| d.id)
        .ok_or_else(|| "该线路未接入系统（请先附加线缆），暂无音量控制器".to_string())
}

/// 读取某输出/输入端点的 **Windows 系统音量**（0.0..=1.0）
#[tauri::command]
pub fn get_device_volume(state: State<'_, AppState>, device_id: String) -> Result<f32, String> {
    let id = real_cable_endpoint_id(&state, &device_id)?;
    audiomix_backend_windows::policy::get_endpoint_volume(&id)
}

/// 设置端点的 **Windows 系统音量**（影响该设备上所有声音，不只是混音器输出）
#[tauri::command]
pub fn set_device_volume(state: State<'_, AppState>, device_id: String, level: f32) -> Result<(), String> {
    let id = real_cable_endpoint_id(&state, &device_id)?;
    audiomix_backend_windows::policy::set_endpoint_volume(&id, level)
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
            let Some(arr) = value.as_array() else {
                continue;
            };
            if arr.len() < 2 {
                continue;
            }
            let (Some(x), Some(y)) = (arr[0].as_f64(), arr[1].as_f64()) else {
                continue;
            };
            if !x.is_finite() || !y.is_finite() {
                continue;
            }
            // 前端布局直接存像素坐标（相对画布左上角），平移后可视区可能是负坐标区域；
            // 后端只做宽松的合理性钳制（前端会按实际画布尺寸精钳）
            clean.insert(
                key.clone(),
                [x.clamp(-8192.0, 16384.0), y.clamp(-8192.0, 16384.0)],
            );
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
    state.engine.apply_graph(graph).map_err(|e| e.to_string())?;
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
pub fn set_processor_params(
    app: AppHandle,
    state: State<AppState>,
    processor_id: String,
    node: audiomix_core::DspNode,
) -> Result<(), String> {
    state
        .engine
        .set_processor_params(&processor_id, node)
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

/// 实测一条连线路径的延迟（ms）：向源节点的采集流注入扫频脉冲（每 400ms 一次，
/// 共 5 次），在路径信号里做相关检测取中位数。整段阻塞约 2.5~4s，放 blocking 线程跑。
#[tauri::command]
pub async fn measure_route_latency(
    state: State<'_, AppState>,
    route_id: String,
) -> Result<f64, String> {
    let engine = state.engine.clone();
    tauri::async_runtime::spawn_blocking(move || {
        engine
            .measure_path_latency(&route_id)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("测量任务失败: {e}"))?
}

/// 实测「源节点 → 输出节点」之间路径的延迟（ms）。同上，注入扫频脉冲阻塞约 3s。
#[tauri::command]
pub async fn measure_nodes_latency(
    state: State<'_, AppState>,
    source_id: String,
    sink_id: String,
) -> Result<f64, String> {
    let engine = state.engine.clone();
    tauri::async_runtime::spawn_blocking(move || {
        engine
            .measure_latency_between(&source_id, &sink_id)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("测量任务失败: {e}"))?
}

/// 电平推送：前端用 Tauri Channel 订阅，后端线程按 20fps 主动推 ——
/// 取代旧的前端 setInterval + get_levels 轮询（每 tick 一次完整 IPC 往返）。
/// 不可见（最小化/托盘）或未订阅时线程只做空读，开销可忽略。
#[tauri::command]
pub fn subscribe_levels(
    state: State<AppState>,
    app: AppHandle,
    channel: Channel<LevelsPayload>,
) {
    *state.levels_channel.lock() = Some(channel);
    // 推送线程常驻（只起一次），循环里每 tick 检查是否真的要推送
    if !state.levels_thread_started.swap(true, Ordering::Relaxed) {
        let engine = state.engine.clone();
        std::thread::spawn(move || loop {
            let app_state = app.state::<AppState>();
            // 间隔是设置项（settings.levels_interval_ms），改动即时生效，无需重新订阅
            let interval = app_state.config.lock().settings.levels_interval_ms.clamp(20, 500);
            std::thread::sleep(std::time::Duration::from_millis(interval));
            let Some(channel) = app_state.levels_channel.lock().clone() else {
                continue;
            };
            // 窗口不可见（最小化/关到托盘）时跳过：读电平 + 序列化纯属白跑
            let visible = app
                .get_webview_window(tray::MAIN_WINDOW)
                .map(|w| w.is_visible().unwrap_or(false))
                .unwrap_or(false);
            if !visible {
                continue;
            }
            // 频谱分析关闭时 stats().spectra 为空表，照常推送电平
            let stats = engine.stats();
            let _ = channel.send(LevelsPayload {
                levels: engine.levels(),
                spectra: stats.spectra,
            });
        });
    }
}

/// 停止电平推送（前端切走页签/窗口隐藏时调用；通道置空后线程自动空转）
#[tauri::command]
pub fn unsubscribe_levels(state: State<AppState>) {
    *state.levels_channel.lock() = None;
}

/// 设备列表推送：订阅引擎事件广播（看门狗周期性枚举 → DevicesChanged），
/// 列表内容有变化才推 —— 取代前端固定 4s 的 list_devices 轮询。
#[tauri::command]
pub fn subscribe_devices(
    state: State<AppState>,
    app: AppHandle,
    channel: Channel<Vec<DeviceInfo>>,
) {
    // 注册即推一次当前列表，前端不必等下一次热插拔
    let initial = state.engine.list_devices();
    if channel.send(initial).is_ok() {
        *state.devices_channel.lock() = Some(channel);
    }
    // 推送线程常驻（只起一次）
    if !state.devices_thread_started.swap(true, Ordering::Relaxed) {
        let engine = state.engine.clone();
        std::thread::spawn(move || {
            use tokio::sync::broadcast::error::RecvError;
            let mut rx = engine.subscribe();
            // 上次推送的列表（序列化后比对，避免每 3s 空推）
            let mut last = String::new();
            loop {
                match rx.blocking_recv() {
                    Ok(EngineEvent::DevicesChanged) => {}
                    Ok(_) => continue, // 图应用/欠载等事件不关心
                    Err(RecvError::Lagged(_)) => {} // 落后了就当作有变化
                    Err(RecvError::Closed) => break,
                }
                let visible = app
                    .get_webview_window(tray::MAIN_WINDOW)
                    .map(|w| w.is_visible().unwrap_or(false))
                    .unwrap_or(false);
                if !visible {
                    continue;
                }
                let devices = engine.list_devices();
                let Ok(json) = serde_json::to_string(&devices) else {
                    continue;
                };
                if json == last {
                    continue;
                }
                let Some(channel) = app.state::<AppState>().devices_channel.lock().clone() else {
                    continue;
                };
                if channel.send(devices).is_ok() {
                    last = json;
                }
            }
        });
    }
}

/// 停止设备列表推送（前端切走页签/窗口隐藏时调用）
#[tauri::command]
pub fn unsubscribe_devices(state: State<AppState>) {
    *state.devices_channel.lock() = None;
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
pub fn update_settings(
    app: AppHandle,
    state: State<AppState>,
    settings: Settings,
) -> Result<(), String> {
    let old_settings = state.config.lock().settings.clone();
    {
        let mut cfg = state.config.lock();
        cfg.settings = settings.clone();
    }
    // 重采样质量变化 → 引擎重建各边重采样器（渲染线程自动跟随快照）
    if settings.resample_quality != old_settings.resample_quality {
        state.engine.set_resample_quality(settings.resample_quality);
    }
    // 边缓冲容量变化 → 重建边缓冲（瞬时可能有一小段间隙）
    if settings.edge_buffer_ms != old_settings.edge_buffer_ms {
        state.engine.set_edge_buffer_ms(settings.edge_buffer_ms);
    }
    // 频谱分析开关 → 引擎音频线程开始/停止采样
    if settings.fft_enabled != old_settings.fft_enabled {
        state.engine.set_fft_enabled(settings.fft_enabled);
    }
    // 频谱参数（FFT 点数/频段边界）→ 引擎更新统计口径（无需重建流）
    if settings.fft_size != old_settings.fft_size
        || settings.fft_bands != old_settings.fft_bands
    {
        state
            .engine
            .set_fft_params(settings.fft_size, settings.fft_bands.clone());
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
pub fn set_control_api_enabled(
    app: AppHandle,
    state: State<AppState>,
    enabled: bool,
) -> Result<(), String> {
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

// ---------- USB/IP 虚拟声卡（usbip-win2 + UAC1）----------

/// 随包捆绑的 usbip-win2 安装包。exe 旁边有 `drivers/usbip/` 就用那份；没有
/// （CI 只发单个 exe）就用编译进 exe 的这份，安装时释放到本地缓存目录再执行。
/// 换驱动版本时同步改这里的文件名与 `drivers/usbip/` 下的文件。
const EMBEDDED_INSTALLER_NAME: &str = "USBip-0.9.8.0-x64.exe";
const EMBEDDED_INSTALLER: &[u8] = include_bytes!("../drivers/usbip/USBip-0.9.8.0-x64.exe");

/// 一条线缆的运行态（UI 用）
#[derive(Debug, Clone, serde::Serialize)]
pub struct UsbIpCableStatus {
    pub number: u8,
    /// 自定义名（用户输入原值，可为空）
    pub name: String,
    /// 实际显示名（空名回退 Virtual Cable NN）
    pub display_name: String,
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
    /// 可用的随包安装包路径：优先 exe 旁边的 `drivers/usbip/`，否则是内嵌副本
    /// 释放后的落地路径（那时文件还没写出，但安装确实能完成）
    pub installer_path: Option<String>,
    /// `installer_path` 指的是内嵌副本（单文件分发），而非磁盘上已存在的文件
    pub installer_embedded: bool,
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

/// 广播 USB/IP 状态给前端（附加/断开/自愈完成后调用，前端监听 `usbip-status`
/// 事件刷新，不用轮询）。`usbip port` 是阻塞的外部调用，丢到后台线程跑。
pub fn emit_usbip_status(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let (enabled, bind, cables, running, local_addr) = {
            let state = app.state::<AppState>();
            let (enabled, bind, cables) = {
                let cfg = state.config.lock();
                (
                    cfg.settings.usbip.enabled,
                    cfg.settings.usbip.bind.clone(),
                    cfg.settings.usbip.cables.clone(),
                )
            };
            (
                enabled,
                bind,
                cables,
                state.usbip.running(),
                state.usbip.local_addr().map(|a| a.to_string()),
            )
        };
        let status = tauri::async_runtime::spawn_blocking(move || {
            build_usbip_status(enabled, bind, running, local_addr, &cables)
        })
        .await
        .ok();
        if let Some(status) = status {
            use tauri::Emitter;
            let _ = app.emit("usbip-status", &status);
        }
    });
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
    // 磁盘上有捆绑安装包就用它；没有（单文件分发）报内嵌副本的落地路径，
    // 否则前端会因为 installer_path 为空而禁用安装按钮——明明装得了。
    let (installer_path, installer_embedded) = match usbip_attach::bundled_installer() {
        Some(p) => (Some(p), false),
        None => (
            Some(usbip_attach::embedded_installer_path(EMBEDDED_INSTALLER_NAME)),
            true,
        ),
    };
    let cable_status = cables
        .iter()
        .map(|c| {
            let bus_id = format!("1-{}", c.number);
            let hit = ports
                .iter()
                .find(|p| p.bus_id.as_deref() == Some(bus_id.as_str()));
            UsbIpCableStatus {
                number: c.number,
                sample_rate: c.sample_rate,
                bits: c.bits,
                mode: match c.mode {
                    audiomix_core::UsbIpCableMode::Loopback => "loopback".into(),
                    audiomix_core::UsbIpCableMode::Reverse => "reverse".into(),
                    audiomix_core::UsbIpCableMode::Mixer => "mixer".into(),
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
            installer_path: installer_path.map(|p| p.to_string_lossy().to_string()),
            installer_embedded,
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

/// 检测本机端口是否空闲（可绑定）。false = 已被其它程序占用。
#[tauri::command]
pub async fn usbip_port_available(port: u16) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
    })
    .await
    .map_err(|e| format!("端口检测失败: {e}"))
}

/// 修改虚拟声卡服务器监听端口（主机地址不变）。返回新状态。
///
/// 服务器运行中不允许改——监听套接字已绑定旧端口，必须先停用；
/// 保存前会试绑定目标端口，被占用则拒绝。
#[tauri::command]
pub async fn usbip_set_port(
    app: AppHandle,
    state: State<'_, AppState>,
    port: u16,
) -> Result<UsbIpStatus, String> {
    if !(1024..=65535).contains(&port) {
        return Err("端口需在 1024 – 65535 之间".into());
    }
    if state.usbip.running() {
        return Err("虚拟声卡服务器正在运行，请先停用再修改端口".into());
    }
    // 保留主机地址，只换端口（bind 形如 host:port）
    let old_bind = state.config.lock().settings.usbip.bind.clone();
    let host = old_bind.rsplit_once(':').map_or(old_bind.as_str(), |(h, _)| h);
    let new_bind = format!("{host}:{port}");

    // 目标端口被占用（可能是别的程序）就直接拒绝，免得下次启用服务器才报错
    let bindable = tauri::async_runtime::spawn_blocking(move || {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
    })
    .await
    .map_err(|e| format!("端口检测失败: {e}"))?;
    if !bindable {
        return Err(format!("端口 {port} 已被其它程序占用"));
    }

    state.usbip.set_bind(new_bind.clone());
    {
        let mut cfg = state.config.lock();
        cfg.settings.usbip.bind = new_bind.clone();
    }
    persist(&app, &state)?;
    tracing::info!("USB/IP 监听端口已改为 {port}（下次启用服务器时生效）");

    let (enabled, cables) = {
        let cfg = state.config.lock();
        (
            cfg.settings.usbip.enabled,
            cfg.settings.usbip.cables.clone(),
        )
    };
    let running = state.usbip.running();
    let local_addr = state.usbip.local_addr().map(|a| a.to_string());
    tauri::async_runtime::spawn_blocking(move || {
        build_usbip_status(enabled, new_bind, running, local_addr, &cables)
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
        (
            cable_configs(&cfg.settings.usbip),
            cfg.settings.usbip.enabled,
        )
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
                        Ok(()) => {
                            tracing::warn!("USB/IP 服务器启动失败（{e}），已回滚到上次的线缆配置")
                        }
                        Err(e2) => {
                            tracing::error!("USB/IP 服务器启动失败（{e}），回滚亦失败: {e2}")
                        }
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
    // 混音页等其它页面监听事件同步线缆状态
    emit_usbip_status(&app);

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
pub async fn usbip_attach_all(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<usbip_attach::AttachReport, String> {
    let Some(addr) = state.usbip.local_addr() else {
        return Err("USB/IP 服务器未运行——请先启用虚拟声卡并保存线缆".into());
    };
    let bus_ids: Vec<String> = state
        .usbip
        .cables()
        .iter()
        .map(|c| c.bus_id.clone())
        .collect();
    if bus_ids.is_empty() {
        return Err("尚未配置任何虚拟线缆".into());
    }
    let (host, port) = (addr.ip().to_string(), addr.port());
    let report = tauri::async_runtime::spawn_blocking(move || {
        // broker 在线（或本次 UAC 启动成功）→ 零 UAC；取消 UAC 则中止，不回退再弹
        if usbip_broker::ensure_broker().is_ok() {
            match usbip_broker::broker_attach_all(&host, port, &bus_ids) {
                Ok(r) => Ok(r),
                Err(e) => {
                    tracing::warn!("broker 附加失败，回退一次性提权: {e}");
                    usbip_attach::attach_all(&host, port, &bus_ids)
                }
            }
        } else {
            usbip_attach::attach_all(&host, port, &bus_ids)
        }
    })
    .await
    .map_err(|e| format!("附加任务失败: {e}"));
    let report = match report {
        Ok(r) => r,
        Err(e) => {
            // 失败也可能已经动了端口（先 detach 再 attach 中途失败），照样广播状态
            let _ = state.engine.refresh_devices();
            emit_usbip_status(&app);
            return Err(e);
        }
    };
    let _ = state.engine.refresh_devices();
    emit_usbip_status(&app);
    Ok(report?)
}

/// 断开全部已接入的线缆（提权 broker 优先，回退一次性提权）
#[tauri::command]
pub async fn usbip_detach_all(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let out = tauri::async_runtime::spawn_blocking(detach_via_broker_or_elevated)
        .await
        .map_err(|e| format!("断开任务失败: {e}"))??;
    let _ = state.engine.refresh_devices();
    emit_usbip_status(&app);
    Ok(out)
}

/// 安装随包捆绑的 usbip-win2（提权静默安装，需要 UAC）
#[tauri::command]
pub async fn usbip_install_driver() -> Result<String, String> {
    // exe 旁边没找到捆绑安装包时，用编译进 exe 的那份（单文件分发也能装驱动）。
    const EMBEDDED: (&str, &[u8]) = (EMBEDDED_INSTALLER_NAME, EMBEDDED_INSTALLER);
    tauri::async_runtime::spawn_blocking(move || usbip_attach::install_bundled(Some(EMBEDDED)))
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
    // 通知提权代理收工（连接断开后它也会自行退出）
    usbip_broker::shutdown_broker();
    app.exit(0);
}

fn persist(app: &AppHandle, state: &State<AppState>) -> Result<(), String> {
    let snapshot = state.config.lock().clone();
    config::save(app, &snapshot)
}
