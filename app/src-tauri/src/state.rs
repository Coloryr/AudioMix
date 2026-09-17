//! 应用状态：引擎句柄 + 配置 + 控制 API 运行句柄 + USB/IP 服务器。

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use audiomix_backend_windows::usbip::UsbIpManager;
use audiomix_control_api::ApiServer;
use audiomix_core::{DeviceInfo, Engine, GraphSettings};
use parking_lot::Mutex;
use serde::Serialize;
use tauri::ipc::Channel;

/// 电平推送载荷：各节点电平 + 频段 dB（fft 关闭时 spectra 为空表）
#[derive(Clone, Serialize)]
pub struct LevelsPayload {
    pub levels: HashMap<String, f32>,
    pub spectra: HashMap<String, Vec<f32>>,
}

pub struct AppState {
    pub engine: Arc<Engine>,
    pub config: Mutex<GraphSettings>,
    pub api: Mutex<Option<Arc<ApiServer>>>,
    /// 内置 USB/IP 服务器（虚拟声卡线缆的传输层）
    pub usbip: Arc<UsbIpManager>,
    /// 电平推送通道：前端 subscribe_levels 注册（取代 get_levels 轮询）；None = 未订阅
    pub levels_channel: Mutex<Option<Channel<LevelsPayload>>>,
    /// 电平推送线程只启动一次的标记
    pub levels_thread_started: AtomicBool,
    /// 设备列表推送通道：前端 subscribe_devices 注册（取代 list_devices 轮询）
    pub devices_channel: Mutex<Option<Channel<Vec<DeviceInfo>>>>,
    /// 设备推送线程只启动一次的标记
    pub devices_thread_started: AtomicBool,
}

impl AppState {
    pub fn new(engine: Arc<Engine>, config: GraphSettings, usbip: Arc<UsbIpManager>) -> Self {
        Self {
            engine,
            config: Mutex::new(config),
            api: Mutex::new(None),
            usbip,
            levels_channel: Mutex::new(None),
            levels_thread_started: AtomicBool::new(false),
            devices_channel: Mutex::new(None),
            devices_thread_started: AtomicBool::new(false),
        }
    }
}
