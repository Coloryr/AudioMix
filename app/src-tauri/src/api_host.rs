//! 控制 API 的宿主实现：把 App 侧能力（配置读写、USB/IP 虚拟声卡、系统端点、
//! 日志、图变更通知）接到 `audiomix_control_api::ApiHost` 上。
//!
//! 所有方法都是**阻塞**语义（文件 IO / COM / 外部进程），由控制 API 的
//! `run_blocking` 丢到阻塞线程执行；这里直接复用 commands 的同步核心。

use std::sync::Arc;

use audiomix_control_api::{ApiError, ApiHost, ApiResult};
use audiomix_core::{Settings, UsbIpCableSettings};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands;
use crate::state::AppState;

pub struct TauriHost {
    app: AppHandle,
}

/// 构造宿主句柄（控制 API 启动时注入）
pub fn host(app: AppHandle) -> Arc<dyn ApiHost> {
    Arc::new(TauriHost { app })
}

impl TauriHost {
    fn state(&self) -> State<'_, AppState> {
        self.app.state::<AppState>()
    }

    /// 当前设置的 JSON 表示
    fn settings_value(&self) -> ApiResult<Value> {
        let settings = self.state().config.lock().settings.clone();
        serde_json::to_value(settings).map_err(|e| ApiError::internal(e.to_string()))
    }
}

impl ApiHost for TauriHost {
    fn app_name(&self) -> &'static str {
        "audiomix"
    }

    fn app_version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    // ---------- 配置 ----------

    fn settings(&self) -> ApiResult<Value> {
        self.settings_value()
    }

    fn update_settings(&self, patch: Value) -> ApiResult<Value> {
        let mut merged = self.settings_value()?;
        merge(&mut merged, patch);
        let settings: Settings = serde_json::from_value(merged)
            .map_err(|e| ApiError::bad_request(format!("设置无效: {e}")))?;
        self.apply(settings)
    }

    fn replace_settings(&self, settings: Value) -> ApiResult<Value> {
        let settings: Settings = serde_json::from_value(settings)
            .map_err(|e| ApiError::bad_request(format!("设置无效: {e}")))?;
        self.apply(settings)
    }

    // ---------- 设备 ----------

    /// 走命令版枚举：顺带跑「默认设备守护」（虚拟线路接入后 Windows 会把系统默认
    /// 播放/录音抢过去，这里恢复用户的选择）——与界面点刷新完全一致
    fn refresh_devices(&self) -> ApiResult<Option<Value>> {
        let devices = commands::refresh_devices(self.app.clone(), self.state())
            .map_err(ApiError::internal)?;
        let value = serde_json::to_value(devices).map_err(|e| ApiError::internal(e.to_string()))?;
        Ok(Some(value))
    }

    // ---------- 系统端点 ----------

    fn device_volume(&self, device_id: &str) -> ApiResult<f32> {
        commands::get_device_volume(self.state(), device_id.to_string()).map_err(ApiError::internal)
    }

    fn set_device_volume(&self, device_id: &str, level: f32) -> ApiResult<()> {
        commands::set_device_volume(self.state(), device_id.to_string(), level)
            .map_err(ApiError::internal)
    }

    fn device_mute(&self, device_id: &str) -> ApiResult<bool> {
        commands::get_device_mute(device_id.to_string()).map_err(ApiError::internal)
    }

    fn set_device_mute(&self, device_id: &str, mute: bool) -> ApiResult<()> {
        commands::set_device_mute(device_id.to_string(), mute).map_err(ApiError::internal)
    }

    fn set_default_device(&self, device_id: &str) -> ApiResult<Value> {
        let devices =
            commands::set_default_device(self.app.clone(), self.state(), device_id.to_string())
                .map_err(ApiError::internal)?;
        serde_json::to_value(devices).map_err(|e| ApiError::internal(e.to_string()))
    }

    // ---------- USB/IP ----------

    fn usbip_status(&self) -> ApiResult<Value> {
        let status = commands::usbip_status_sync(commands::usbip_status_inputs(&self.state()));
        serde_json::to_value(status).map_err(|e| ApiError::internal(e.to_string()))
    }

    fn usbip_set_cables(&self, enabled: Option<bool>, cables: Value) -> ApiResult<Value> {
        let cables: Vec<UsbIpCableSettings> = serde_json::from_value(cables)
            .map_err(|e| ApiError::bad_request(format!("线缆配置无效: {e}")))?;
        let state = self.state();
        let enabled = enabled.unwrap_or_else(|| state.config.lock().settings.usbip.enabled);
        // 先自检一遍，把「参数不合法」报成 400 而不是 500
        let check = audiomix_core::UsbIpSettings {
            enabled,
            bind: state.config.lock().settings.usbip.bind.clone(),
            cables: cables.clone(),
        };
        check
            .validate()
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        commands::usbip_set_cables_sync(&self.app, &state, enabled, cables)
            .map_err(|e| precondition_or_internal(e))?;
        self.usbip_status()
    }

    fn usbip_attach(&self) -> ApiResult<Value> {
        let report = commands::usbip_attach_all_sync(&self.app, &self.state())
            .map_err(|e| precondition_or_internal(e))?;
        serde_json::to_value(report).map_err(|e| ApiError::internal(e.to_string()))
    }

    fn usbip_detach(&self) -> ApiResult<Value> {
        let out = commands::usbip_detach_all_sync(&self.app, &self.state())
            .map_err(|e| precondition_or_internal(e))?;
        Ok(serde_json::json!({ "output": out }))
    }

    fn usbip_install_driver(&self) -> ApiResult<Value> {
        let out = commands::usbip_install_driver_sync().map_err(ApiError::internal)?;
        Ok(serde_json::json!({ "output": out }))
    }

    // ---------- 日志 ----------

    fn logs(&self, since: u64, limit: usize) -> ApiResult<Value> {
        let (lines, next) = crate::logbuf::buffer().since(since);
        let total = lines.len();
        let truncated = total > limit;
        let lines: Vec<Value> = lines
            .into_iter()
            .take(limit)
            .map(|l| serde_json::json!({ "seq": l.seq, "text": l.text }))
            .collect();
        // 截断时把游标停在最后一行，客户端下次接着取
        let next = lines
            .last()
            .and_then(|l| l.get("seq"))
            .and_then(|s| s.as_u64())
            .unwrap_or_else(|| if truncated { since } else { next });
        Ok(serde_json::json!({
            "lines": lines,
            "next": next,
            "truncated": truncated,
        }))
    }

    // ---------- 图变更通知 ----------

    /// 控制 API 改了混音图：落盘到配置并通知界面重新拉取
    fn graph_changed(&self) {
        let state = self.state();
        {
            let mut cfg = state.config.lock();
            cfg.graph = state.engine.get_graph();
        }
        if let Err(e) = commands::persist(&self.app, &state) {
            tracing::warn!("控制 API 改动混音图后保存配置失败: {e}");
        }
        // 界面持有自己的图副本，不通知就会在下次保存时把 API 的改动覆盖掉
        if let Err(e) = self.app.emit("graph-changed", ()) {
            tracing::warn!("混音图变更事件广播失败: {e}");
        }
    }
}

impl TauriHost {
    /// 应用一份完整设置（走命令同一套副作用），返回生效后的设置 JSON
    fn apply(&self, settings: Settings) -> ApiResult<Value> {
        // defer_api_restart：从 API 改 API 自身的参数时先让响应发出去再重启服务
        commands::apply_settings(&self.app, &self.state(), settings, true)
            .map_err(ApiError::bad_request)?;
        self.settings_value()
    }
}

/// 递归合并 JSON 对象（对象逐字段、其余整体替换）——PATCH 语义
fn merge(base: &mut Value, patch: Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                merge(b.entry(k).or_insert(Value::Null), v);
            }
        }
        (b, p) => *b = p,
    }
}

/// USB/IP 的「前置条件没满足」（服务器没开、没配线缆）按 409 报，
/// 其余（提权被取消、设备被占用等）算 500。
fn precondition_or_internal(msg: String) -> ApiError {
    const PRECONDITION: [&str; 3] = ["服务器未运行", "尚未配置任何虚拟线缆", "请先启用"];
    if PRECONDITION.iter().any(|p| msg.contains(p)) {
        ApiError::conflict(msg)
    } else {
        ApiError::internal(msg)
    }
}
