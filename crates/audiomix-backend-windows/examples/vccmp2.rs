//! 临时诊断（用完即删）：**同时**在两处取样，定位"卡"发生在虚拟声卡之前还是之后。
//!
//! A) 虚拟声卡**播放端**（扬声器 Virtual Cable 01）的 loopback 采集
//!    = Windows/播放器交给我们设备的信号（卡在这里 = 播放器/audiodg 的问题）；
//! B) 主播放设备（Minifuse）的 loopback 采集 = 混音器最终输出
//!    （卡在这里而 A 干净 = 我们这条链（线缆→引擎→物理设备）的问题）。
//!
//! 两边都按 100ms 统计 RMS，最后列出任何一边掉到中位数 20% 以下的时刻。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audiomix_backend_windows::wasapi::device::enumerate_devices;
use audiomix_backend_windows::wasapi::capture;
use audiomix_core::backend::StartedStream;
use audiomix_core::model::DeviceKind;

fn secs() -> f32 {
    std::env::var("VC_SECS")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(30.0)
}

fn start(label: &str, id: &str) -> Option<(Arc<Mutex<Vec<f32>>>, f32, StartedStream)> {
    let rec = Arc::new(Mutex::new(Vec::<f32>::new()));
    let r2 = rec.clone();
    let peak = Arc::new(AtomicU32::new(0));
    let p2 = peak.clone();
    let s = capture::start_capture(
        id,
        true,
        Box::new(move |d| {
            let mut v = r2.lock().unwrap();
            let mut m = 0f32;
            for &x in d.iter().step_by(2) {
                if x.abs() > m {
                    m = x.abs();
                }
                v.push(x);
            }
            let bits = m.to_bits();
            let mut cur = p2.load(Ordering::Relaxed);
            while bits > cur {
                match p2.compare_exchange_weak(cur, bits, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => break,
                    Err(y) => cur = y,
                }
            }
        }),
    )
    .ok()?;
    println!("{label}: {}Hz/{}ch", s.info.sample_rate, s.info.channels);
    Some((rec, s.info.sample_rate as f32, s))
}

/// 100ms 的 RMS 序列 + 掉音点
fn rms_series(data: &[f32], fs: f32) -> Vec<f32> {
    let win = (fs * 0.1) as usize;
    data.chunks(win)
        .map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt())
        .collect()
}

fn report(label: &str, data: &[f32], fs: f32) -> Vec<f32> {
    if data.is_empty() {
        println!("[{label}] 没采到数据");
        return Vec::new();
    }
    let mut zero_run = 0usize;
    let mut zero_total = 0usize;
    let mut longest_zero = 0usize;
    let mut same_run = 1usize;
    let mut same_total = 0usize;
    for i in 1..data.len() {
        if data[i] == 0.0 {
            zero_run += 1;
            if zero_run == 8 {
                zero_total += 8;
            } else if zero_run > 8 {
                zero_total += 1;
            }
        } else {
            longest_zero = longest_zero.max(zero_run);
            zero_run = 0;
        }
        if data[i] == data[i - 1] {
            same_run += 1;
        } else {
            if same_run >= 4 {
                same_total += same_run;
            }
            same_run = 1;
        }
    }
    longest_zero = longest_zero.max(zero_run);
    let series = rms_series(data, fs);
    let mut s = series.clone();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = s[s.len() / 2];
    let dips: Vec<usize> = series
        .iter()
        .enumerate()
        .filter(|(_, v)| med > 0.0 && **v < med * 0.2)
        .map(|(i, _)| i)
        .collect();
    println!(
        "[{label}] {:.1}s 样本：静音缺口 {:.0}ms（最长 {:.0}ms）；冻结样本 {:.0}ms；100ms RMS 中位 {med:.4}，掉到 20% 以下的窗口 {} 个",
        data.len() as f32 / fs,
        zero_total as f32 / fs * 1000.0,
        longest_zero as f32 / fs * 1000.0,
        same_total as f32 / fs * 1000.0,
        dips.len()
    );
    if !dips.is_empty() {
        let list: Vec<String> = dips.iter().take(40).map(|i| format!("{:.1}s", *i as f32 / 10.0)).collect();
        println!("[{label}] 掉音时刻: {}", list.join(", "));
    }
    series
}

fn main() {
    let devices = match enumerate_devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("枚举设备失败: {e}");
            return;
        }
    };
    let vc_out = devices
        .iter()
        .find(|d| d.name.contains("Virtual Cable") && d.kind == DeviceKind::Output)
        .cloned();
    let main_out = devices
        .iter()
        .find(|d| d.kind == DeviceKind::Output && d.name.contains("Minifuse"))
        .cloned();
    let (Some(vc_out), Some(main_out)) = (vc_out, main_out) else {
        println!("(缺少线缆播放端或主播放设备)");
        return;
    };

    let a = start("A 虚拟声卡播放端(Windows→我们)", &vc_out.id);
    let b = start("B 主播放设备(混音器输出)", &main_out.id);
    let secs = secs();
    println!("\n被动采集 {secs}s —— 请现在放歌（高解析度那段）…\n");
    std::thread::sleep(Duration::from_secs_f32(secs));

    let (sa, sb) = match (&a, &b) {
        (Some((da, fa, _)), Some((db, fb, _))) => {
            let ta = da.lock().unwrap().clone();
            let tb = db.lock().unwrap().clone();
            (report("A 虚拟声卡播放端", &ta, *fa), report("B 混音器输出", &tb, *fb))
        }
        _ => {
            println!("某一路采集没起来");
            return;
        }
    };
    // 逐秒对照：任何一路 RMS 低于自身中位 20% 就打印该秒
    println!("\n--- 逐秒对照（只列异常秒）---");
    let mut med_a = sa.clone();
    med_a.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let mut med_b = sb.clone();
    med_b.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let ma = med_a.get(med_a.len() / 2).copied().unwrap_or(0.0);
    let mb = med_b.get(med_b.len() / 2).copied().unwrap_or(0.0);
    let secs = sa.len().min(sb.len()) / 10;
    for s in 0..secs {
        let wa = &sa[s * 10..(s * 10 + 10).min(sa.len())];
        let wb = &sb[s * 10..(s * 10 + 10).min(sb.len())];
        let va = wa.iter().cloned().fold(f32::INFINITY, f32::min);
        let vb = wb.iter().cloned().fold(f32::INFINITY, f32::min);
        if (ma > 0.0 && va < ma * 0.2) || (mb > 0.0 && vb < mb * 0.2) {
            println!(
                "  {s:2}s  A最小={va:.4}（中位 {ma:.4}）  B最小={vb:.4}（中位 {mb:.4}）  {}",
                if ma > 0.0 && va < ma * 0.2 && mb > 0.0 && vb < mb * 0.2 {
                    "两边同时掉"
                } else if ma > 0.0 && va < ma * 0.2 {
                    "只有 A 掉（Windows/播放器侧）"
                } else {
                    "只有 B 掉（我们这条链）"
                }
            );
        }
    }
}
