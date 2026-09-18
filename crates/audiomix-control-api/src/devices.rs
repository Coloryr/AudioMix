//! 设备端点：列表/重新枚举/系统默认设备/端点音量与静音。
//!
//! 设备 id 里可能含 `/`（虚拟线路的合成 id 形如 `usbip://1/playback`），
//! 放路径参数需要百分号转义、容易踩坑，因此这些端点统一用 `?device_id=` 查询参数。

use axum::extract::{Json, Query, State};
use serde::Deserialize;

use crate::error::{body, ApiResult};
use crate::{run_blocking, ApiState};

#[derive(Deserialize)]
pub struct DeviceQuery {
    /// 缺参数时留空，由 [`DeviceQuery::id`] 报统一的 400（而不是框架的纯文本拒绝）
    #[serde(default)]
    pub device_id: String,
}

impl DeviceQuery {
    fn id(&self) -> ApiResult<String> {
        let id = self.device_id.trim();
        if id.is_empty() {
            return Err(crate::ApiError::bad_request(
                "缺少查询参数 `device_id`（设备 id 形如 `{0.0.0.0.…}.{…}` 或 `usbip://1/playback`）",
            ));
        }
        Ok(id.to_string())
    }
}

#[derive(Deserialize)]
pub struct DefaultDeviceBody {
    pub device_id: String,
}

#[derive(Deserialize)]
pub struct VolumeBody {
    pub level: f32,
}

#[derive(Deserialize)]
pub struct MuteBody {
    pub mute: bool,
}

/// 设备列表（读引擎缓存，不重新枚举设备）
pub async fn list(State(st): State<ApiState>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(st.engine.list_devices()).unwrap_or_default())
}

/// 重新枚举设备（热插拔后刷新；顺带对齐受影响的流）。
/// 宿主能接管时交给宿主（顺带跑默认设备守护），否则用引擎枚举。
pub async fn refresh(State(st): State<ApiState>) -> ApiResult<Json<serde_json::Value>> {
    if let Some(devices) = run_blocking(st.host.clone(), |h| h.refresh_devices()).await? {
        return Ok(Json(devices));
    }
    let devices = st.engine.refresh_devices()?;
    Ok(Json(serde_json::to_value(devices).unwrap_or_default()))
}

/// 设为 Windows 默认播放/录音设备（虚拟线路的合成 id 会自动映射到真实端点）
pub async fn set_default(
    State(st): State<ApiState>,
    payload: Result<Json<DefaultDeviceBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<serde_json::Value>> {
    let body = body(payload)?;
    let id = body.device_id.clone();
    let out = run_blocking(st.host.clone(), move |h| h.set_default_device(&id)).await?;
    Ok(Json(out))
}

pub async fn get_volume(
    State(st): State<ApiState>,
    Query(q): Query<DeviceQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = q.id()?;
    let level = run_blocking(st.host.clone(), move |h| h.device_volume(&id)).await?;
    Ok(Json(serde_json::json!({ "device_id": q.device_id, "level": level })))
}

pub async fn set_volume(
    State(st): State<ApiState>,
    Query(q): Query<DeviceQuery>,
    payload: Result<Json<VolumeBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<serde_json::Value>> {
    let body = body(payload)?;
    if !(0.0..=1.0).contains(&body.level) {
        return Err(crate::ApiError::bad_request(format!(
            "音量需在 0.0 – 1.0 之间（收到 {}）",
            body.level
        )));
    }
    let id = q.id()?;
    let level = body.level;
    run_blocking(st.host.clone(), move |h| h.set_device_volume(&id, level)).await?;
    Ok(Json(serde_json::json!({ "device_id": q.device_id, "level": level })))
}

pub async fn get_mute(
    State(st): State<ApiState>,
    Query(q): Query<DeviceQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = q.id()?;
    let mute = run_blocking(st.host.clone(), move |h| h.device_mute(&id)).await?;
    Ok(Json(serde_json::json!({ "device_id": q.device_id, "mute": mute })))
}

pub async fn set_mute(
    State(st): State<ApiState>,
    Query(q): Query<DeviceQuery>,
    payload: Result<Json<MuteBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<serde_json::Value>> {
    let body = body(payload)?;
    let id = q.id()?;
    let mute = body.mute;
    run_blocking(st.host.clone(), move |h| h.set_device_mute(&id, mute)).await?;
    Ok(Json(serde_json::json!({ "device_id": q.device_id, "mute": mute })))
}
