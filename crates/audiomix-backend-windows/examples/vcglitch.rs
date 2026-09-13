//! 临时诊断（用完即删）：长时间测「线缆 → 混音器 → 主播放设备」的
//! 连续性（爆音/丢样/相位跳变）与端到端延迟。
//!
//! 放 20 秒 1kHz 正弦，同时 loopback 采主播放设备：
//! - 延迟 = 采到信号起点 − 开始播放的时刻；
//! - 每 0.5s 窗口算一次频率：正常应≈1000Hz，出现跳变（丢/重样）会把估频拉高或压低；
//! - 每 100ms 的 RMS 最小值 / 静音缺口占比：掉音会直接露出来。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audiomix_backend_windows::wasapi::device::enumerate_devices;
use audiomix_backend_windows::wasapi::{capture, render};
use audiomix_core::model::DeviceKind;

const TONE_HZ: f32 = 1000.0;
const TONE_AMP: f32 = 0.25;
const PLAY_SECS: f32 = 20.0;
const TOTAL_SECS: f32 = 23.0;

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
    println!("线缆播放端: {} / 主播放设备: {}", vc_out.name, main_out.name);

    let rec = Arc::new(Mutex::new(Vec::<f32>::new()));
    let peak = Arc::new(AtomicU32::new(0));
    let (r2, p2) = (rec.clone(), peak.clone());
    let cap = capture::start_capture(
        &main_out.id,
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
    );
    let cap = match cap {
        Ok(c) => c,
        Err(e) => {
            eprintln!("回环采集失败: {e}");
            return;
        }
    };
    let fs = cap.info.sample_rate as f32;
    println!("回环采集 {fs}Hz/{}ch；放 {TONE_HZ}Hz/{TONE_AMP} 共 {PLAY_SECS}s，测 {TOTAL_SECS}s…\n", cap.info.channels);

    let vc_rate = vc_out.sample_rate.max(1) as f32;
    let ch = vc_out.channels.max(1) as usize;
    let t0 = Instant::now();
    let phase = Arc::new(AtomicU32::new(0));
    let ph = phase.clone();
    let play = render::start_render(
        &vc_out.id,
        Box::new(move |out| {
            let on = t0.elapsed().as_secs_f32() < PLAY_SECS;
            let mut idx = ph.load(Ordering::Relaxed);
            for frame in out.chunks_mut(ch) {
                let s = if on {
                    let v = (idx as f32 * TONE_HZ * std::f32::consts::TAU / vc_rate).sin() * TONE_AMP;
                    idx += 1;
                    v
                } else {
                    0.0
                };
                for y in frame.iter_mut() {
                    *y = s;
                }
            }
            ph.store(idx, Ordering::Relaxed);
        }),
    );
    let play = match play {
        Ok(p) => p,
        Err(e) => {
            eprintln!("打开线缆播放端失败: {e}");
            return;
        }
    };

    let start = Instant::now();
    let mut next_report = 5.0f32;
    while start.elapsed() < Duration::from_secs_f32(TOTAL_SECS) {
        std::thread::sleep(Duration::from_millis(250));
        let t = start.elapsed().as_secs_f32();
        if t >= next_report {
            next_report += 5.0;
            let n = rec.lock().unwrap().len();
            println!(
                "t={t:4.1}s 已采 {:.2}s，最近峰值={:.4}",
                n as f32 / fs,
                f32::from_bits(peak.swap(0, Ordering::Relaxed))
            );
        }
    }
    drop(play);

    let data = rec.lock().unwrap().clone();
    let fs = fs;
    let Some(onset) = data.iter().position(|&x| x.abs() > TONE_AMP * 0.2) else {
        println!("\n❌ 没采到信号");
        return;
    };
    let tail = data.iter().rposition(|&x| x.abs() > TONE_AMP * 0.2).unwrap_or(onset);
    println!(
        "\n信号 {:.3}s → {:.3}s（{:.2}s，播放 {PLAY_SECS}s）",
        onset as f32 / fs,
        tail as f32 / fs,
        (tail - onset) as f32 / fs
    );
    println!("端到端延迟（开始播放 → 听到）= **{:.0} ms**", onset as f32 / fs * 1000.0);

    // 逐 0.5s 窗口频率 + RMS
    let win = (fs * 0.5) as usize;
    let rms_win = (fs * 0.1) as usize;
    let a = onset + (fs * 0.3) as usize;
    let b = tail.saturating_sub((fs * 0.3) as usize);
    let mut freqs: Vec<f32> = Vec::new();
    let mut i = a;
    while i + win < b {
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
        if let (Some(f0), true) = (f0, cy > 0) {
            freqs.push(cy as f32 * fs / (last0 - f0) as f32);
        }
        i += win;
    }
    let mut sorted = freqs.clone();
    sorted.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let bad = freqs.iter().filter(|f| (**f / TONE_HZ - 1.0).abs() > 0.02).count();
    println!(
        "逐 0.5s 窗口频率：{} 个，中位 {:.1}Hz，最小 {:.1}，最大 {:.1}；偏离 >2% 的窗口 **{} 个**",
        freqs.len(),
        sorted.get(sorted.len() / 2).copied().unwrap_or(0.0),
        sorted.first().copied().unwrap_or(0.0),
        sorted.last().copied().unwrap_or(0.0),
        bad
    );

    let mut rms: Vec<f32> = Vec::new();
    for chunk in data[a..b].chunks(rms_win) {
        rms.push((chunk.iter().map(|x| x * x).sum::<f32>() / chunk.len() as f32).sqrt());
    }
    let mut rs = rms.clone();
    rs.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let med = rs.get(rs.len() / 2).copied().unwrap_or(0.0);
    let mn = rs.first().copied().unwrap_or(0.0);
    println!(
        "100ms RMS：中位 {med:.4}，最小 {mn:.4}（{:.1} dB 相对中位），{} 个窗口",
        if mn > 0.0 { 20.0 * (mn / med).log10() } else { -99.0 },
        rms.len()
    );

    // 静音缺口（连续 >=8 个精确 0）
    let mut gap = 0usize;
    let mut run = 0usize;
    for &x in &data[onset..=tail] {
        if x == 0.0 {
            run += 1;
        } else {
            if run >= 8 {
                gap += run;
            }
            run = 0;
        }
    }
    println!(
        "静音缺口合计 {:.0} ms（占 {:.2}%）",
        gap as f32 / fs * 1000.0,
        100.0 * gap as f32 / (tail - onset) as f32
    );

    // 单样本级的不连续（"咔"声）：纯正弦的二阶差分应当极小，
    // 出现尖峰说明有丢样/重复/错位（RMS 看不出来，耳朵能听出来）。
    let expected = TONE_AMP * (2.0 * std::f32::consts::PI * TONE_HZ / fs).powi(2);
    let thr = expected * 20.0;
    let mut spikes: Vec<usize> = Vec::new();
    for i in a + 2..b {
        let d2 = (data[i] - 2.0 * data[i - 1] + data[i - 2]).abs();
        if d2 > thr {
            spikes.push(i);
        }
    }
    println!(
        "二阶差分尖峰（>20×理论值，理论 {expected:.2e}，阈值 {thr:.2e}）：**{} 个样本**",
        spikes.len()
    );
    if spikes.len() >= 2 {
        let mut sp: Vec<f32> = spikes.windows(2).map(|w| (w[1] - w[0]) as f32 / fs).collect();
        sp.sort_by(|x, y| x.partial_cmp(y).unwrap());
        println!(
            "尖峰间隔：中位 {:.3}s，最小 {:.3}s，最大 {:.3}s（等间隔 = 周期性故障）",
            sp[sp.len() / 2],
            sp[0],
            sp[sp.len() - 1]
        );
    }
    // 最大跳变幅度（相邻样本最大差，正常正弦应 ≈ A·2πf/fs = 0.0082）
    let mut max_step = 0f32;
    for i in a + 1..b {
        let d = (data[i] - data[i - 1]).abs();
        if d > max_step {
            max_step = d;
        }
    }
    println!(
        "相邻样本最大跳变 {:.4}（纯正弦理论 {:.4}，比值 {:.1}×）",
        max_step,
        TONE_AMP * 2.0 * std::f32::consts::PI * TONE_HZ / fs,
        max_step / (TONE_AMP * 2.0 * std::f32::consts::PI * TONE_HZ / fs)
    );
}
