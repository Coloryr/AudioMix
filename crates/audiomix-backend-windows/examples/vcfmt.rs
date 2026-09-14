//! 临时诊断（用完即删）：虚拟线路端点的格式/开流判定器。
//!
//! 对**每一个**音频端点：
//! 1. 读 `GetMixFormat`（失败就是没格式可用：AUDCLNT_E_UNSUPPORTED_FORMAT）；
//! 2. 对输出端点（跳过 Minifuse，避免真出声）真的开一条渲染流播 0.6 秒 —— 能开流
//!    说明 KS pin/格式完全可用，同时会触发 ISO URB，服务器日志里就能看到该线缆在传数据。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use audiomix_backend_windows::wasapi::device::{enumerate_devices, mix_format_of_by_id};
use audiomix_backend_windows::wasapi::render;
use audiomix_core::model::DeviceKind;

fn main() {
    let devices = match enumerate_devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("枚举失败: {e}");
            return;
        }
    };
    let mut fmt_ok = 0;
    let mut fmt_fail = 0;
    let mut stream_ok = 0;
    let mut stream_fail = 0;
    println!("--- 格式 / 开流判定 ---");
    for d in devices.iter() {
        let kind = match d.kind {
            DeviceKind::Input => "输入",
            DeviceKind::Output => "输出",
        };
        let fmt = match mix_format_of_by_id(&d.id) {
            Ok(f) => {
                fmt_ok += 1;
                format!("{}Hz/{}ch", f.sample_rate, f.channels)
            }
            Err(e) => {
                fmt_fail += 1;
                println!("FAIL {kind} {:<40} 格式读不到: {e}", d.name);
                continue;
            }
        };
        // 只对虚拟线路的输出端点真的开流（跳过物理设备，避免吵到人）
        let virtual_out = d.kind == DeviceKind::Output
            && !d.name.contains("Minifuse")
            && (d.name.contains("MTX") || d.name.contains("Virtual Cable"));
        if !virtual_out {
            println!("OK   {kind} {:<40} {fmt}", d.name);
            continue;
        }
        let phase = Arc::new(AtomicU32::new(0));
        let ph = phase.clone();
        let ch = d.channels.max(1) as usize;
        let rate = d.sample_rate.max(1) as f32;
        let started = render::start_render(
            &d.id,
            Box::new(move |out| {
                let mut n = ph.load(Ordering::Relaxed);
                // 很轻的 1kHz（幅度 0.05 ≈ -26dBFS），只是为了触发流
                let _ = n;
                for frame in out.chunks_mut(ch) {
                    let v = (n as f32 * 1000.0 * std::f32::consts::TAU / rate).sin() * 0.05;
                    n += 1;
                    for y in frame.iter_mut() {
                        *y = v;
                    }
                }
                ph.store(n, Ordering::Relaxed);
            }),
        );
        match started {
            Ok(s) => {
                stream_ok += 1;
                println!("OK   {kind} {:<40} {fmt}  + 开流成功", d.name);
                std::thread::sleep(Duration::from_millis(2500));
                drop(s);
            }
            Err(e) => {
                stream_fail += 1;
                println!("FAIL {kind} {:<40} {fmt}  但开流失败: {e}", d.name);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!(
        "\n共 {} 个端点：格式 OK {fmt_ok} / 失败 {fmt_fail}；虚拟输出开流成功 {stream_ok} / 失败 {stream_fail}",
        devices.len()
    );
}
