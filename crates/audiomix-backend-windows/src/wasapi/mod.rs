//! WASAPI 后端内部的公共设施。

use windows::core::w;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};

pub mod capture;
pub mod device;
pub mod render;

/// 把当前线程注册进 MMCSS 的「Pro Audio」类别，drop 时注销。
///
/// 音频回调线程必须拿到 MMCSS 的调度优先级与延迟预算：系统一忙（编译、杀毒、
/// 索引、DPC 风暴）普通线程会被抢占几百毫秒，WASAPI 缓冲一旦被抽干，输出就被
/// 补成数字静音 —— 实测出现过 **645ms 的静音空洞**，听感就是「卡」。
#[must_use]
pub struct ProAudio(Option<HANDLE>);

impl ProAudio {
    pub fn join() -> Self {
        let mut index = 0u32;
        // 失败（比如 MMCSS 服务被禁用）不算致命错误，照常跑，只是没有优先级保障
        let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) }.ok();
        Self(handle)
    }
}

impl Drop for ProAudio {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(h);
            }
        }
    }
}

/// 音频回调的「空档」看门狗：两次回调间隔远超预期就记一条警告。
///
/// WASAPI 事件回调正常每 5–10ms 一次；间隔达到几百毫秒说明线程被系统抢占了，
/// 这一段输出必然被补静音（用户听到的就是卡顿）。日志能直接定位到「卡」的时刻。
pub struct CallbackGap {
    last: std::time::Instant,
    /// 触发警告的间隔阈值
    threshold: std::time::Duration,
    /// 上次告警时间（避免刷屏）
    last_warn: std::time::Instant,
    pub max_gap: std::time::Duration,
}

impl CallbackGap {
    pub fn new(threshold: std::time::Duration) -> Self {
        let now = std::time::Instant::now();
        Self { last: now, threshold, last_warn: now, max_gap: std::time::Duration::ZERO }
    }

    /// 每次回调开头调用，返回本次与上次的间隔
    pub fn tick(&mut self, label: &str) -> std::time::Duration {
        let now = std::time::Instant::now();
        let gap = now.duration_since(self.last);
        self.last = now;
        if gap > self.max_gap {
            self.max_gap = gap;
        }
        if gap > self.threshold && now.duration_since(self.last_warn) > std::time::Duration::from_secs(1)
        {
            self.last_warn = now;
            tracing::warn!(
                "{label} 回调空档 {:.0} ms（正常 5–10ms）→ 这段输出会被补静音（听感卡顿）",
                gap.as_secs_f64() * 1000.0
            );
        }
        gap
    }
}
