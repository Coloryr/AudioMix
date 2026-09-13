//! 临时诊断（用完即删）：打印虚拟声卡两个端点的**系统音量/静音/格式**。
//! 麦克风端音量接近 0 会把"录下来的音乐"变成近乎静音 + 毛刺，很容易被误判成"卡"。

use audiomix_backend_windows::policy::{get_endpoint_mute, get_endpoint_volume, hardware_support};
use audiomix_backend_windows::wasapi::device::{enumerate_devices, mix_format_of_by_id};
use audiomix_core::model::DeviceKind;

fn main() {
    let devices = match enumerate_devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("枚举失败: {e}");
            return;
        }
    };
    for d in devices
        .iter()
        .filter(|d| d.name.contains("Virtual Cable") || d.name.contains("Minifuse"))
    {
        let kind = match d.kind {
            DeviceKind::Input => "输入",
            DeviceKind::Output => "输出",
        };
        let vol = get_endpoint_volume(&d.id);
        let mute = get_endpoint_mute(&d.id);
        let hw = hardware_support(&d.id);
        let fmt = mix_format_of_by_id(&d.id);
        println!(
            "{kind} {} | 音量={:?} | 静音={:?} | 硬件支持={:?} | 格式={:?}",
            d.name,
            vol.map(|v| format!("{v:.4}")),
            mute,
            hw,
            fmt.map(|f| format!("{}Hz/{}ch", f.sample_rate, f.channels))
        );
    }
}
