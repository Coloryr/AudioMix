//! 统一错误类型。

use thiserror::Error;

/// core 各层的 `Result` 别名。
pub type Result<T> = std::result::Result<T, Error>;

/// 引擎与后端的统一错误。
///
/// 平台后端的原始错误一律经 [`Error::Backend`] 转成字符串——错误要跨越
/// FFI/异步边界进 UI 层展示，不携带平台类型可以保持 core 平台无关。
#[derive(Debug, Error)]
pub enum Error {
    /// 后端（平台 API）错误，消息已本地化为可直接展示的文本
    #[error("backend error: {0}")]
    Backend(String),
    /// 设备 id 不存在（可能已拔出）
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    /// 混音图校验失败（route 引用了不存在的 source/sink 等）
    #[error("invalid graph: {0}")]
    InvalidGraph(String),
    /// 设置项超出合法范围（采样率、位深、线缆号等）
    #[error("invalid settings: {0}")]
    InvalidSettings(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// 事件通道已关闭（订阅方全部退出）
    #[error("channel closed")]
    Closed,
}

impl Error {
    /// 把任意平台错误（实现 `Display`）包装成 [`Error::Backend`]。
    pub fn backend(e: impl std::fmt::Display) -> Self {
        Error::Backend(e.to_string())
    }
}
