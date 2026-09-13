//! 临时诊断（用完即删）：**被动**监听主播放设备（不放任何测试音），
//! 在用户用真实播放器放歌时检查输出是否被"冻住"/丢样。
//!
//! 三个探测器（对音乐也有效）：
//! - 连续 >=8 个精确 0 → 静音缺口（欠载补零）；
//! - 连续 >=4 个**完全相同**的样本 → 重采样器"保持上一帧"（输入被取空的典型特征，
//!   真实音乐里相邻样本几乎不可能完全相同）；
//! - 50ms RMS 的最小值 / 中位数 → 整体电平是否有塌陷。

use std::sync::atomic::{AtomicU64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audiomix_backend_windows::wasapi::device::enumerate_devices;
use audiomix_backend_windows::wasapi::capture;
use audiomix_core::model::DeviceKind;

const SECS: f32 = 30.0;

fn main() {
    let devices = match enumerate_devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("枚举设备失败: {e}");
            return;
        }
    };
    let out = devices
        .iter()
        .find(|d| d.kind == DeviceKind::Output && d.name.contains("Minifuse"))
        .cloned();
    let Some(out) = out else {
        println!("(没找到 Minifuse)");
        return;
    };
    println!("被动监听: {} ({}Hz/{}ch)", out.name, out.sample_rate, out.channels);

    let rec = Arc::new(Mutex::new(Vec::<f32>::new()));
    let peak = Arc::new(AtomicU32::new(0));
    let n_cb = Arc::new(AtomicU64::new(0));
    let (r2, p2, n2) = (rec.clone(), peak.clone(), n_cb.clone());
    let cap = capture::start_capture(
        &out.id,
        true,
        Box::new(move |d| {
            n2.fetch_add(1, Ordering::Relaxed);
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
    );
    let cap = match cap {
        Ok(c) => c,
        Err(e) => {
            eprintln!("回环采集失败: {e}");
            return;
        }
    };
    let fs = cap.info.sample_rate as f32;
    println!("开始被动采样 {SECS}s（请让播放器继续放歌）…\n");

    let start = Instant::now();
    let mut next = 5.0f32;
    while start.elapsed() < Duration::from_secs_f32(SECS) {
        std::thread::sleep(Duration::from_millis(200));
        let t = start.elapsed().as_secs_f32();
        if t >= next {
            next += 5.0;
            println!(
                "t={t:4.1}s 回调 {} 次，已采 {:.2}s，最近峰值={:.4}",
                n_cb.load(Ordering::Relaxed),
                rec.lock().unwrap().len() as f32 / fs,
                f32::from_bits(peak.swap(0, Ordering::Relaxed))
            );
        }
    }
    drop(cap);

    let data = rec.lock().unwrap().clone();
    if data.len() < (fs * 2.0) as usize {
        println!("\n❌ 采到的数据太少（{} 样本）", data.len());
        return;
    }
    // 1) 静音缺口
    let mut zero_run = 0usize;
    let mut zero_total = 0usize;
    let mut longest_zero = 0usize;
    // 2) 完全相同样本的长串
    let mut same_run = 1usize;
    let mut same_total = 0usize;
    let mut longest_same = 0usize;
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
            longest_same = longest_same.max(same_run);
            same_run = 1;
        }
    }
    longest_zero = longest_zero.max(zero_run);
    longest_same = longest_same.max(same_run);

    println!(
        "\n总样本 {:.1}s",
        data.len() as f32 / fs
    );
    println!(
        "静音缺口（连续≥8个精确0）：合计 {:.1} ms，最长 {:.1} ms",
        zero_total as f32 / fs * 1000.0,
        longest_zero as f32 / fs * 1000.0
    );
    println!(
        "冻结样本（连续≥4个完全相同，重采样器保持上一帧的特征）：合计 {:.1} ms，最长 {:.1} ms",
        same_total as f32 / fs * 1000.0,
        longest_same as f32 / fs * 1000.0
    );

    // 3) 50ms RMS 分布
    let win = (fs * 0.05) as usize;
    let mut rms: Vec<f32> = Vec::new();
    for chunk in data.chunks(win) {
        rms.push((chunk.iter().map(|x| x * x).sum::<f32>() / chunk.len() as f32).sqrt());
    }
    let mut rs = rms.clone();
    rs.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let med = rs[rs.len() / 2];
    let p10 = rs[rs.len() / 10];
    let mn = rs[0];
    let zeros = rs.iter().filter(|v| **v == 0.0).count();
    println!(
        "50ms RMS：中位 {med:.4}，10% 分位 {p10:.4}，最小 {mn:.4}；完全静音的窗口 {zeros}/{}（{:.1}%）",
        rs.len(),
        100.0 * zeros as f32 / rs.len() as f32
    );
    let below = rs.iter().filter(|v| **v < med * 0.1).count();
    println!(
        "  低于中位 -20dB 的窗口：{below}/{}（{:.1}%）",
        rs.len(),
        100.0 * below as f32 / rs.len() as f32
    );

    // 4) 咔哒声检测：二阶差分 |x[n]-2x[n-1]+x[n-2]| 的孤立尖峰。
    //    正常音乐里二阶差分是连续分布，出现远超中位数的孤立尖峰 = 波形被"掰断"了一下，
    //    听起来就是"咔"。丢掉/重复一个样本（没有补零）只有这个探测器能抓到。
    let mut d2: Vec<f32> = Vec::with_capacity(data.len());
    for i in 2..data.len() {
        d2.push((data[i] - 2.0 * data[i - 1] + data[i - 2]).abs());
    }
    let mut ds = d2.clone();
    ds.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let d_med = ds[ds.len() / 2];
    let d_p999 = ds[ds.len() * 999 / 1000];
    let d_max = *ds.last().unwrap();
    let thr = (d_med * 30.0).max(d_p999 * 3.0);
    let spikes = d2.iter().filter(|v| **v > thr).count();
    // 孤立的（前后一个样本都不超阈值）才算"咔"
    let mut isolated = 0usize;
    let mut i = 1;
    while i + 1 < d2.len() {
        if d2[i] > thr && d2[i - 1] <= thr && d2[i + 1] <= thr {
            isolated += 1;
        }
        i += 1;
    }
    println!(
        "二阶差分：中位 {d_med:.2e}，99.9% 分位 {d_p999:.2e}，最大 {d_max:.2e}；超阈值({thr:.2e}) {spikes} 个样本，其中**孤立尖峰 {isolated} 个**"
    );
    if isolated > 0 {
        println!("  平均每 {:.2} 秒一次", 30.0 / isolated as f32);
    }

    // 5) 频率（播放 1kHz 测试音频时用）：逐 0.5s 窗口过零计数
    let win = (fs * 0.5) as usize;
    let mut freqs: Vec<f32> = Vec::new();
    let mut i = 0usize;
    while i + win < data.len() {
        let w = &data[i..i + win];
        let mut f0 = None;
        let mut last0 = 0usize;
        let mut cy = 0usize;
        for k in 1..w.len() {
            if w[k - 1] <= 0.0 && w[k] > 0.0 {
                if f0.is_none() {
                    f0 = Some(k);
                } else {
                    cy += 1;
                }
                last0 = k;
            }
        }
        if let (Some(f0), true) = (f0, cy > 5) {
            freqs.push(cy as f32 * fs / (last0 - f0) as f32);
        }
        i += win;
    }
    if !freqs.is_empty() {
        let mut s = freqs.clone();
        s.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let near = freqs.iter().filter(|f| (**f - 1000.0).abs() < 25.0).count();
        println!(
            "逐 0.5s 窗口估频：{} 个（有声窗口），中位 {:.1}Hz，最小 {:.1}，最大 {:.1}；落在 1000±25Hz 的 {} 个",
            freqs.len(),
            s[s.len() / 2],
            s[0],
            s[s.len() - 1],
            near
        );
    }
}
