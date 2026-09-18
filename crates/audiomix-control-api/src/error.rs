//! 统一错误响应：HTTP 状态码 + `{"error": {...}}` 结构体。
//!
//! 客户端据状态码分流（4xx = 客户端问题，5xx = 服务器问题），
//! `error.kind` 是稳定的机器可读标签，`error.message` 是给人看的中文说明。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

/// 控制 API 的错误：状态码 + 说明
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    /// 机器可读分类（`bad_request` / `not_found` / `unauthorized` / …）
    pub kind: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    /// 平台侧/内部错误（没做成，但不是调用方写错了）
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    /// 该能力需要 App 侧支持（纯引擎模式 / 测试环境没有）
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, "unsupported", message)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.status)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": { "kind": self.kind, "message": self.message }
        });
        (self.status, Json(body)).into_response()
    }
}

/// 引擎错误 → 合适的 HTTP 状态码
impl From<audiomix_core::Error> for ApiError {
    fn from(e: audiomix_core::Error) -> Self {
        use audiomix_core::Error as E;
        let msg = e.to_string();
        match e {
            E::InvalidGraph(_) | E::InvalidSettings(_) | E::Json(_) => ApiError::bad_request(msg),
            E::DeviceNotFound(_) => ApiError::not_found(msg),
            _ => ApiError::internal(msg),
        }
    }
}

/// `Result<_, String>`（各平台后端/App 钩子的惯例返回）→ 500
impl From<String> for ApiError {
    fn from(message: String) -> Self {
        ApiError::internal(message)
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// 提取 JSON 请求体，把 axum 的解析拒绝转成统一错误体
pub(crate) fn body<T: serde::de::DeserializeOwned>(
    r: Result<Json<T>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<T> {
    r.map(|Json(v)| v)
        .map_err(|e| ApiError::bad_request(format!("请求体解析失败: {e}")))
}
