use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("backend error: {0}")]
    Backend(String),
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("invalid graph: {0}")]
    InvalidGraph(String),
    #[error("invalid settings: {0}")]
    InvalidSettings(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("channel closed")]
    Closed,
}

impl Error {
    pub fn backend(e: impl std::fmt::Display) -> Self {
        Error::Backend(e.to_string())
    }
}
