//! App 侧扩展能力（[`ApiHost`]）——控制 API 只依赖引擎，其余能力（配置读写、
//! USB/IP 虚拟声卡、系统默认设备、日志）由宿主 App 注入。
//!
//! 未注入（纯引擎 / 测试）时这些端点返回 `501 unsupported`，核心端点照常可用。
//! 所有方法都是**阻塞**语义（文件 IO / COM / 外部进程），调用方经
//! [`crate::run_blocking`] 丢到阻塞线程执行。

use serde_json::Value;

use crate::error::{ApiError, ApiResult};

pub trait ApiHost: Send + Sync + 'static {
    fn app_name(&self) -> &'static str {
        "audiomix"
    }

    /// 应用版本（`/api/health`、`/api` 索引里报给客户端）
    fn app_version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    // ---------- 配置 ----------

    fn settings(&self) -> ApiResult<Value> {
        Err(ApiError::unsupported("当前宿主未提供配置读写"))
    }

    /// 局部更新设置：`patch` 是与设置同构的 JSON，递归合并进当前设置
    fn update_settings(&self, _patch: Value) -> ApiResult<Value> {
        Err(ApiError::unsupported("当前宿主未提供配置读写"))
    }

    /// 整体替换设置（未给的字段回默认值）
    fn replace_settings(&self, _settings: Value) -> ApiResult<Value> {
        Err(ApiError::unsupported("当前宿主未提供配置读写"))
    }

    // ---------- 设备 ----------

    /// 重新枚举设备。宿主可返回 `Some(设备列表)` 接管（例如顺带跑「默认设备守护」，
    /// 恢复被虚拟线路抢走的系统默认设备）；返回 `None` 表示用引擎的枚举结果。
    fn refresh_devices(&self) -> ApiResult<Option<Value>> {
        Ok(None)
    }

    // ---------- 系统端点（音量/静音/默认设备） ----------

    fn device_volume(&self, _device_id: &str) -> ApiResult<f32> {
        Err(ApiError::unsupported("当前宿主未提供系统端点音量控制"))
    }

    fn set_device_volume(&self, _device_id: &str, _level: f32) -> ApiResult<()> {
        Err(ApiError::unsupported("当前宿主未提供系统端点音量控制"))
    }

    fn device_mute(&self, _device_id: &str) -> ApiResult<bool> {
        Err(ApiError::unsupported("当前宿主未提供系统端点静音控制"))
    }

    fn set_device_mute(&self, _device_id: &str, _mute: bool) -> ApiResult<()> {
        Err(ApiError::unsupported("当前宿主未提供系统端点静音控制"))
    }

    /// 设为 Windows 默认播放/录音设备，返回刷新后的设备列表
    fn set_default_device(&self, _device_id: &str) -> ApiResult<Value> {
        Err(ApiError::unsupported("当前宿主未提供默认设备切换"))
    }

    // ---------- USB/IP 虚拟声卡 ----------

    fn usbip_status(&self) -> ApiResult<Value> {
        Err(ApiError::unsupported("当前宿主未提供虚拟声卡管理"))
    }

    /// 保存线缆配置并（重）启服务器；`enabled` 为 None 时保持原开关
    fn usbip_set_cables(&self, _enabled: Option<bool>, _cables: Value) -> ApiResult<Value> {
        Err(ApiError::unsupported("当前宿主未提供虚拟声卡管理"))
    }

    /// 附加全部线缆到系统（需要管理员权限，可能弹 UAC）
    fn usbip_attach(&self) -> ApiResult<Value> {
        Err(ApiError::unsupported("当前宿主未提供虚拟声卡管理"))
    }

    fn usbip_detach(&self) -> ApiResult<Value> {
        Err(ApiError::unsupported("当前宿主未提供虚拟声卡管理"))
    }

    fn usbip_install_driver(&self) -> ApiResult<Value> {
        Err(ApiError::unsupported("当前宿主未提供虚拟声卡管理"))
    }

    // ---------- 日志 ----------

    /// 增量取运行日志：`since` 为已取到的最大序号，`limit` 为本次上限
    fn logs(&self, _since: u64, _limit: usize) -> ApiResult<Value> {
        Err(ApiError::unsupported("当前宿主未提供日志读取"))
    }

    // ---------- 通知 ----------

    /// 混音图被本 API 改动后调用：宿主用它落盘配置并通知界面刷新。
    /// （引擎已经广播 `GraphApplied` 给 SSE 订阅者，这里只处理 App 侧同步。）
    fn graph_changed(&self) {}
}

/// 空宿主：除引擎自带能力外全部返回 501（纯引擎模式、集成测试用）
pub struct NoHost;

impl ApiHost for NoHost {}
