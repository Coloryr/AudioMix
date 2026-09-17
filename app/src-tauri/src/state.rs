//! 应用状态：引擎句柄 + 配置 + 控制 API 运行句柄 + USB/IP 服务器。

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use audiomix_backend_windows::usbip::UsbIpManager;
use audiomix_control_api::ApiServer;
use audiomix_core::{Engine, GraphSettings};
use parking_lot::Mutex;
use tauri::ipc::Channel;

pub struct AppState {
    pub engine: Arc<Engine>,
    pub config: Mutex<GraphSettings>,
    pub api: Mutex<Option<Arc<ApiServer>>>,
    /// 内置 USB/IP 服务器（虚拟声卡线缆的传输层）
    pub usbip: Arc<UsbIpManager>,
    /// 电平推送通道：前端 subscribe_levels 注册（取代 get_levels 轮询）；None = 未订阅
    pub levels_channel: Mutex<Option<Channel<HashMap<String, f32>>>>,
    /// 电平推送线程只启动一次的标记
    pub levels_thread_started: AtomicBool,
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
        }
    }
}
