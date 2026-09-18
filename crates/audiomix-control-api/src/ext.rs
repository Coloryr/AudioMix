//! App 侧能力端点：设置、USB/IP 虚拟声卡、运行日志。
//!
//! 这些能力由宿主经 [`crate::ApiHost`] 注入（平台/文件系统相关），
//! 未注入时统一返回 `501 unsupported`，引擎自带的端点不受影响。

use axum::extract::{Json, Query, State};
use serde::Deserialize;

use crate::error::{body, ApiError, ApiResult};
use crate::{run_blocking, ApiState};

// ---------- 设置 ----------

pub async fn get_settings(State(st): State<ApiState>) -> ApiResult<Json<serde_json::Value>> {
    let value = run_blocking(st.host.clone(), |h| h.settings()).await?;
    Ok(Json(value))
}

/// 局部更新：与当前设置递归合并（只发改动的字段即可）
pub async fn patch_settings(
    State(st): State<ApiState>,
    payload: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<serde_json::Value>> {
    let patch = body(payload)?;
    if !patch.is_object() {
        return Err(ApiError::bad_request("设置补丁必须是 JSON 对象"));
    }
    let value = run_blocking(st.host.clone(), move |h| h.update_settings(patch)).await?;
    Ok(Json(value))
}

/// 整体替换（未给字段回默认值）
pub async fn put_settings(
    State(st): State<ApiState>,
    payload: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<serde_json::Value>> {
    let settings = body(payload)?;
    if !settings.is_object() {
        return Err(ApiError::bad_request("设置必须是 JSON 对象"));
    }
    let value = run_blocking(st.host.clone(), move |h| h.replace_settings(settings)).await?;
    Ok(Json(value))
}

// ---------- USB/IP 虚拟声卡 ----------

pub async fn usbip_status(State(st): State<ApiState>) -> ApiResult<Json<serde_json::Value>> {
    let value = run_blocking(st.host.clone(), |h| h.usbip_status()).await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
pub struct CablesBody {
    /// 不传 = 保持原开关
    #[serde(default)]
    pub enabled: Option<bool>,
    pub cables: serde_json::Value,
}

/// 保存线缆配置并（重）启服务器。改格式/增删线缆后需要在系统侧重
/// 新附加（`POST /api/usbip/attach`）才会生效。
pub async fn usbip_set_cables(
    State(st): State<ApiState>,
    payload: Result<Json<CablesBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<serde_json::Value>> {
    let body = body(payload)?;
    let value = run_blocking(st.host.clone(), move |h| {
        h.usbip_set_cables(body.enabled, body.cables)
    })
    .await?;
    Ok(Json(value))
}

/// 附加全部线缆到系统（提权，可能弹 UAC；阻塞到 attach 结束）
pub async fn usbip_attach(State(st): State<ApiState>) -> ApiResult<Json<serde_json::Value>> {
    let value = run_blocking(st.host.clone(), |h| h.usbip_attach()).await?;
    Ok(Json(value))
}

pub async fn usbip_detach(State(st): State<ApiState>) -> ApiResult<Json<serde_json::Value>> {
    let value = run_blocking(st.host.clone(), |h| h.usbip_detach()).await?;
    Ok(Json(value))
}

pub async fn usbip_install_driver(State(st): State<ApiState>) -> ApiResult<Json<serde_json::Value>> {
    let value = run_blocking(st.host.clone(), |h| h.usbip_install_driver()).await?;
    Ok(Json(value))
}

// ---------- 日志 ----------

#[derive(Deserialize)]
pub struct LogsQuery {
    /// 已取到的最大序号（增量拉取）
    #[serde(default)]
    pub since: u64,
    /// 本次最多返回多少行（默认 500，上限 5000）
    #[serde(default)]
    pub limit: Option<usize>,
}

pub async fn logs(
    State(st): State<ApiState>,
    Query(q): Query<LogsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let limit = q.limit.unwrap_or(500).clamp(1, 5000);
    let since = q.since;
    let value = run_blocking(st.host.clone(), move |h| h.logs(since, limit)).await?;
    Ok(Json(value))
}
