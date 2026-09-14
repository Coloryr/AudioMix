//! AudioMix 的 Windows 音频后端：WASAPI 共享模式（采集 / loopback / 渲染）、
//! USB/IP 虚拟声卡（UAC2）与系统默认端点切换。

pub mod driver;
pub mod pair;
// IPolicyConfig 的 vtable 方法名沿用 Windows SDK 命名（GetMixFormat 等）
#[allow(non_snake_case)]
pub mod policy;
pub mod usbip;
pub mod wasapi;

use audiomix_core::backend::{AudioBackend, CaptureCallback, RenderCallback, StartedStream};
use audiomix_core::error::Result;
use audiomix_core::model::DeviceInfo;

/// [`AudioBackend`] 的 Windows 实现（纯 WASAPI，不含虚拟声卡——后者由
/// [`UsbIpBackend`](crate::usbip::UsbIpBackend) 提供并通过
/// [`CompositeBackend`](audiomix_core::CompositeBackend) 组合）。
pub struct WindowsBackend;

impl Default for WindowsBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsBackend {
    pub fn new() -> Self {
        Self
    }
}

impl AudioBackend for WindowsBackend {
    fn enumerate_devices(&self) -> Result<Vec<DeviceInfo>> {
        wasapi::device::enumerate_devices()
    }

    fn start_capture(&self, device_id: &str, on_data: CaptureCallback) -> Result<StartedStream> {
        wasapi::capture::start_capture(device_id, false, on_data)
    }

    fn start_loopback(&self, device_id: &str, on_data: CaptureCallback) -> Result<StartedStream> {
        wasapi::capture::start_capture(device_id, true, on_data)
    }

    fn start_render(&self, device_id: &str, on_fill: RenderCallback) -> Result<StartedStream> {
        wasapi::render::start_render(device_id, on_fill)
    }

    fn name(&self) -> &'static str {
        "windows-wasapi"
    }
}
