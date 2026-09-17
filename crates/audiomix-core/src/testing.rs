//! 测试替身：FakeBackend。
//!
//! 模拟 WASAPI 行为供引擎/控制层测试使用：
//! - 采集流：线程循环推送 `set_capture_data` 预置的 DC（常数）信号块
//! - 渲染流：线程周期性调用 `on_fill`，把输出累积到可观测 buffer
//! - 提供 alive 计数用于流生命周期断言
//!
//! 用常数信号的好处：passthrough 无插值误差、欠载保持帧与信号相同，
//! 对时序抖动完全免疫，均值断言可以做到很紧的容差。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::backend::{
    AudioBackend, CaptureCallback, RenderCallback, StartedStream, StreamHandle, StreamInfo,
};
use crate::error::Result;
use crate::model::{looks_virtual, DeviceInfo, DeviceKind};

const CAPTURE_PERIOD: Duration = Duration::from_millis(4);
const RENDER_PERIOD: Duration = Duration::from_millis(4);
/// 渲染帧块（帧数），每次 on_fill 的长度
const RENDER_CHUNK_FRAMES: usize = 512;
/// 渲染 buffer 上限（样本数），防止长时间运行内存增长
const RENDER_BUFFER_CAP: usize = 4_000_000;

/// 内存中的假后端（`#[doc(hidden)]`，仅供测试与示例使用）。
pub struct FakeBackend {
    devices: Vec<DeviceInfo>,
    capture_data: Mutex<HashMap<String, Vec<f32>>>,
    render_buffers: Mutex<HashMap<String, Arc<Mutex<Vec<f32>>>>>,
    pub captures_started: AtomicUsize,
    pub captures_alive: Arc<AtomicUsize>,
    pub renders_started: AtomicUsize,
    pub renders_alive: Arc<AtomicUsize>,
}

impl FakeBackend {
    /// 默认设备集：
    /// - `dev-out-1`  输出 48kHz 2ch（默认输出）
    /// - `dev-out-hi` 输出 96kHz 2ch
    /// - `dev-in-1`   输入 48kHz 2ch（默认输入）
    /// - `dev-mono`   输入 48kHz 1ch（虚拟设备名）
    pub fn new() -> Arc<Self> {
        Self::with_devices(Self::default_devices())
    }

    pub fn default_devices() -> Vec<DeviceInfo> {
        vec![
            DeviceInfo {
                id: "dev-out-1".into(),
                name: "Fake Speakers".into(),
                kind: DeviceKind::Output,
                is_default: true,
                is_virtual: false,
                channels: 2,
                sample_rate: 48000,
            },
            DeviceInfo {
                id: "dev-out-hi".into(),
                name: "Fake HiRate Output".into(),
                kind: DeviceKind::Output,
                is_default: false,
                is_virtual: false,
                channels: 2,
                sample_rate: 96000,
            },
            DeviceInfo {
                id: "dev-in-1".into(),
                name: "Fake Microphone".into(),
                kind: DeviceKind::Input,
                is_default: true,
                is_virtual: false,
                channels: 2,
                sample_rate: 48000,
            },
            DeviceInfo {
                id: "dev-mono".into(),
                name: "Virtual Mono Mic".into(),
                kind: DeviceKind::Input,
                is_default: false,
                is_virtual: looks_virtual("Virtual Mono Mic"),
                channels: 1,
                sample_rate: 48000,
            },
        ]
    }

    /// 自定义设备集的 FakeBackend（供组合后端等需要区分子后端的测试使用）
    pub fn with_devices(devices: Vec<DeviceInfo>) -> Arc<Self> {
        Arc::new(Self {
            devices,
            capture_data: Mutex::new(HashMap::new()),
            render_buffers: Mutex::new(HashMap::new()),
            captures_started: AtomicUsize::new(0),
            captures_alive: Arc::new(AtomicUsize::new(0)),
            renders_started: AtomicUsize::new(0),
            renders_alive: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// 预置某设备的采集信号（interleaved，一块会被循环推送）
    pub fn set_capture_data(&self, device_id: &str, data: Vec<f32>) {
        self.capture_data
            .lock()
            .unwrap()
            .insert(device_id.to_string(), data);
    }

    /// 某设备渲染输出的累积 buffer（不存在的设备惰性创建）
    pub fn render_buffer(&self, device_id: &str) -> Arc<Mutex<Vec<f32>>> {
        self.render_buffers
            .lock()
            .unwrap()
            .entry(device_id.to_string())
            .or_default()
            .clone()
    }

    /// 清空某设备的渲染 buffer（用于阶段划分）
    pub fn reset_render_buffer(&self, device_id: &str) {
        if let Some(b) = self.render_buffers.lock().unwrap().get(device_id) {
            b.lock().unwrap().clear();
        }
    }

    /// 仍在运行的采集流数（应与已 drop 的流数对称归零）。
    pub fn alive_captures(&self) -> usize {
        self.captures_alive.load(Ordering::SeqCst)
    }

    /// 仍在运行的渲染流数。
    pub fn alive_renders(&self) -> usize {
        self.renders_alive.load(Ordering::SeqCst)
    }

    fn find(&self, device_id: &str) -> Result<DeviceInfo> {
        self.devices
            .iter()
            .find(|d| d.id == device_id)
            .cloned()
            .ok_or_else(|| crate::error::Error::DeviceNotFound(device_id.to_string()))
    }
}

impl AudioBackend for FakeBackend {
    fn enumerate_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(self.devices.clone())
    }

    fn start_capture(
        &self,
        device_id: &str,
        mut on_data: CaptureCallback,
    ) -> Result<StartedStream> {
        let dev = self.find(device_id)?;
        let data = self
            .capture_data
            .lock()
            .unwrap()
            .get(device_id)
            .cloned()
            .unwrap_or_default();
        let info = StreamInfo {
            sample_rate: dev.sample_rate,
            channels: dev.channels,
        };
        self.captures_started.fetch_add(1, Ordering::SeqCst);
        self.captures_alive.fetch_add(1, Ordering::SeqCst);
        let stop = Arc::new(AtomicBool::new(false));
        let alive = AliveGuard(self.captures_alive.clone());
        let handle = StreamHandle::spawn(
            stop,
            std::thread::Builder::new().name("fake-capture".into()),
            move |stop| {
                let _alive = alive; // drop 时减计数
                while !stop.load(Ordering::SeqCst) {
                    if !data.is_empty() {
                        on_data(&data);
                    }
                    std::thread::sleep(CAPTURE_PERIOD);
                }
            },
        )?;
        Ok(StartedStream { info, handle })
    }

    fn start_loopback(&self, device_id: &str, on_data: CaptureCallback) -> Result<StartedStream> {
        self.start_capture(device_id, on_data)
    }

    fn start_render(&self, device_id: &str, mut on_fill: RenderCallback) -> Result<StartedStream> {
        let dev = self.find(device_id)?;
        let info = StreamInfo {
            sample_rate: dev.sample_rate,
            channels: dev.channels,
        };
        let buffer = self.render_buffer(device_id);
        self.renders_started.fetch_add(1, Ordering::SeqCst);
        self.renders_alive.fetch_add(1, Ordering::SeqCst);
        let stop = Arc::new(AtomicBool::new(false));
        let alive = AliveGuard(self.renders_alive.clone());
        let handle = StreamHandle::spawn(
            stop,
            std::thread::Builder::new().name("fake-render".into()),
            move |stop| {
                let _alive = alive;
                while !stop.load(Ordering::SeqCst) {
                    let mut tmp = vec![0.0f32; RENDER_CHUNK_FRAMES * dev.channels as usize];
                    on_fill(&mut tmp);
                    let mut buf = buffer.lock().unwrap();
                    let cap = RENDER_BUFFER_CAP.max(tmp.len());
                    if buf.len() + tmp.len() > cap {
                        let drop = buf.len() + tmp.len() - cap;
                        buf.drain(..drop);
                    }
                    buf.extend_from_slice(&tmp);
                    drop(buf);
                    std::thread::sleep(RENDER_PERIOD);
                }
            },
        )?;
        Ok(StartedStream { info, handle })
    }

    fn name(&self) -> &'static str {
        "fake"
    }
}

/// 采集/渲染线程内持有，线程退出时自动减 alive 计数
struct AliveGuard(Arc<AtomicUsize>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
