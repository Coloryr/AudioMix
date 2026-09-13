//! 应用状态：引擎句柄 + 配置 + 控制 API 运行句柄 + USB/IP 服务器。

use std::sync::Arc;

use audiomix_backend_windows::usbip::UsbIpManager;
use audiomix_control_api::ApiServer;
use audiomix_core::{Engine, GraphSettings};
use parking_lot::Mutex;

pub struct AppState {
    pub engine: Arc<Engine>,
    pub config: Mutex<GraphSettings>,
    pub api: Mutex<Option<Arc<ApiServer>>>,
    /// 内置 USB/IP 服务器（虚拟声卡线缆的传输层）
    pub usbip: Arc<UsbIpManager>,
}

impl AppState {
    pub fn new(engine: Arc<Engine>, config: GraphSettings, usbip: Arc<UsbIpManager>) -> Self {
        Self {
            engine,
            config: Mutex::new(config),
            api: Mutex::new(None),
            usbip,
        }
    }
}
