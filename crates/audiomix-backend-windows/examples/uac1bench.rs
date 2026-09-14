//! 无 GUI 的虚拟线路测试台（内置 UAC1）。
//!
//! 按环境变量起 N 条线缆，跑指定秒数后自动退出；期间可以用
//! `usbip.exe attach -r 127.0.0.1 -b 1-N` 真机验证格式/播放/录音。
//!
//! ```text
//! 用法：
//!   $env:UAC1BENCH_FORMATS = "48000:16,96000:24,192000:16"
//!   $env:UAC1BENCH_SECS    = "120"
//!   $env:UAC1BENCH_MODE    = "loopback"   # loopback / reverse / mixer
//!   cargo run -p audiomix-backend-windows --example uac1bench
//! ```
//!
//! 输出示例：`线缆 #1 1-1 48000Hz/16bit/loopback → 192 B/ms`

use std::time::{Duration, Instant};

use audiomix_backend_windows::usbip::device::{CableConfig, CableMode};
use audiomix_backend_windows::usbip::UsbIpManager;

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn main() {
    let formats = env("UAC1BENCH_FORMATS", "48000:16");
    let secs: u64 = env("UAC1BENCH_SECS", "60").parse().unwrap_or(60);
    let mode = match env("UAC1BENCH_MODE", "loopback").as_str() {
        "reverse" => CableMode::Reverse,
        "mixer" => CableMode::Mixer,
        _ => CableMode::Loopback,
    };
    let bind = env("UAC1BENCH_BIND", "127.0.0.1:3240");

    let mut cables = Vec::new();
    for (i, spec) in formats.split(',').map(str::trim).filter(|s| !s.is_empty()).enumerate() {
        // 规格写法：`rate:bits`（线缆号 = 序号+1）或 `number:rate:bits`（改端口/实例用）
        let parts: Vec<&str> = spec.split(':').map(str::trim).collect();
        let (number, rate, bits) = match parts.as_slice() {
            [n, r, b] => (
                n.parse::<u8>().unwrap_or((i + 1) as u8),
                r.parse::<u32>().unwrap_or(48_000),
                b.parse::<u16>().unwrap_or(16),
            ),
            [r, b] => (
                (i + 1) as u8,
                r.parse::<u32>().unwrap_or(48_000),
                b.parse::<u16>().unwrap_or(16),
            ),
            _ => ((i + 1) as u8, 48_000, 16),
        };
        let bytes_per_ms = (rate as u64).div_ceil(1000) * 2 * (bits as u64 / 8);
        println!("线缆 #{number} 1-{number} {rate}Hz/{bits}bit/{mode:?} → 包长 {bytes_per_ms} B/ms");
        cables.push(CableConfig { number, name: String::new(), sample_rate: rate, bits, mode, buffer_ms: 250 });
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("应能建 tokio 运行时");
    let manager = UsbIpManager::new(&bind);
    if let Err(e) = manager.start(rt.handle(), cables) {
        eprintln!("启动失败: {e}");
        return;
    }
    println!("USB/IP 服务器已监听 {} —— 可用 usbip.exe attach 验证", bind);

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut last_report = Instant::now();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        if last_report.elapsed() >= Duration::from_secs(2) {
            last_report = Instant::now();
            for c in manager.cables() {
                // 没人读时 play_ring 会填满并开始丢 —— 说明 ISO OUT 数据真的到了
                let (play_size, play_drop, play_under) = c.play_ring.stats();
                let (cap_size, cap_drop, cap_under) = c.cap_ring.stats();
                println!(
                    "#{:02} play {}/{} 丢{} 欠{} | cap {}/{} 丢{} 欠{} | ISO 节拍 1ms",
                    c.cfg.number,
                    play_size,
                    c.play_ring.capacity(),
                    play_drop,
                    play_under,
                    cap_size,
                    c.cap_ring.capacity(),
                    cap_drop,
                    cap_under,
                );
            }
        }
    }
    manager.stop();
    println!("已停止（跑了 {secs} 秒）");
}
