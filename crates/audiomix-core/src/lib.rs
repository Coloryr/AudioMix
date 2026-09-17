//! audiomix-core — 平台无关的音频混音引擎。
//!
//! 引擎只依赖 [`AudioBackend`] trait 与平台无关的数据模型，
//! Windows / Linux / macOS 通过各自的后端 crate 提供实现。

pub mod backend;
pub mod dsp;
pub mod engine;
pub mod error;
pub mod mixer;
pub mod model;
pub mod resample;
pub mod ring;
#[doc(hidden)]
pub mod testing;

pub use backend::{
    AudioBackend, CaptureCallback, CompositeBackend, RenderCallback, StartedStream, StreamHandle,
    StreamInfo,
};
pub use engine::{make_processor, Engine, EngineEvent};
pub use error::{Error, Result};
pub use model::{
    ControlApiSettings, DeviceInfo, DeviceKind, DspKind, DspNode, GraphConfig, GraphSettings,
    Processor, Route, Settings, Sink, Source, SourceMode, UsbIpCableMode, UsbIpCableSettings,
    UsbIpSettings,
};
