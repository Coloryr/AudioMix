//! 临时诊断（用完即删）：扫频（chirp）连续性测试。
//!
//! 固定单音看不出丢样（丢一小段正弦还是正弦、相位跳一下听不出来），
//! 扫频把每一刻变成**唯一频率**，于是：
//! - 重复了一小段 → 瞬时频率曲线出现"平台"；
//! - 丢了一小段 → 曲线出现"跳变"；
//! - 速率不对 → 曲线整体斜率/延迟不匹配。
//!
//! 同时采 A（Windows→我们）与 B（我们→Minifuse），比较两条 f(t) 曲线：
//! 两条都坏 ⇒ 问题在 Windows/播放器侧；只有 B 坏 ⇒ 在我们这条链上。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audiomix_backend_windows::wasapi::capture;
use audiomix_backend_windows::wasapi::device::enumerate_devices;
use audiomix_backend_windows::wasapi::render;
use audiomix_core::backend::StartedStream;
use audiomix_core::model::DeviceKind;

const F0: f32 = 300.0;
const F1: f32 = 8000.0;
const T: f32 = 10.0; // 扫频时长
const AMP: f32 = 0.3;
const WIN: f32 = 0.03; // 瞬时频率窗口 30ms
const TOTAL: f32 = 12.0;

fn start(label: &str, id: &str) -> Option<(Arc<Mutex<Vec<f32>>>, f32, StartedStream)> {
    let rec = Arc::new(Mutex::new(Vec::<f32>::new()));
    let r2 = rec.clone();
    let s = capture::start_capture(
        id,
        true,
        Box::new(move |d| {
            let mut v = r2.lock().unwrap();
            for &x in d.iter().step_by(2) {
                v.push(x);
            }
        }),
    )
    .ok()?;
    println!("{label}: {}Hz/{}ch", s.info.sample_rate, s.info.channels);
    Some((rec, s.info.sample_rate as f32, s))
}

/// 逐窗口的瞬时频率（正过零计数），返回 (窗口中心时刻, 频率, 窗口 RMS)
fn if_curve(x: &[f32], fs: f32) -> Vec<(f32, f32, f32)> {
    let w = (fs * WIN) as usize;
    let mut out = Vec::new();
    let mut i = 0;
    while i + w < x.len() {
        let seg = &x[i..i + w];
        let rms = (seg.iter().map(|v| v * v).sum::<f32>() / w as f32).sqrt();
        let mut first = None;
        let mut last = 0usize;
        let mut cy = 0usize;
        for k in 1..w {
            if seg[k - 1] <= 0.0 && seg[k] > 0.0 {
                if first.is_none() {
                    first = Some(k);
                } else {
                    cy += 1;
                }
                last = k;
            }
        }
        let f = match first {
            Some(f0) if cy > 1 => cy as f32 * fs / (last - f0) as f32,
            _ => 0.0,
        };
        out.push(((i as f32 + w as f32 / 2.0) / fs, f, rms));
        i += w;
    }
    out
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
    let a = start("A 虚拟声卡播放端", &vc_out.id);
    let b = start("B Minifuse 输出", &main_out.id);

    let ch = vc_out.channels.max(1) as usize;
    let rate = vc_out.sample_rate.max(1) as f32;
    let phase = Arc::new(AtomicU32::new(0));
    let ph = phase.clone();
    let play = render::start_render(
        &vc_out.id,
        Box::new(move |out| {
            let mut n = ph.load(Ordering::Relaxed);
            for frame in out.chunks_mut(ch) {
                let t = n as f32 / rate;
                let t = t.min(T);
                // 线性扫频的相位：φ(t) = 2π(f0 t + (f1-f0)/(2T) t²)
                let k = (F1 - F0) / T;
                let phi = std::f32::consts::TAU * (F0 * t + 0.5 * k * t * t);
                let s = if n as f32 / rate < T { phi.sin() * AMP } else { 0.0 };
                for y in frame.iter_mut() {
                    *y = s;
                }
                n += 1;
            }
            ph.store(n, Ordering::Relaxed);
        }),
    );
    let _play = match play {
        Ok(p) => p,
        Err(e) => {
            eprintln!("打开线缆播放端失败: {e}");
            return;
        }
    };
    println!("播扫频 {F0}→{F1}Hz（{T}s），采 {TOTAL}s…\n");
    std::thread::sleep(Duration::from_secs_f32(TOTAL));
    drop(_play);

    let (Some((da, fa, _)), Some((db, fb, _))) = (&a, &b) else {
        println!("某一路没起来");
        return;
    };
    if (*fa - *fb).abs() > 1.0 {
        println!("两路采样率不同，跳过");
        return;
    }
    let fs = *fa;
    let ca = if_curve(&da.lock().unwrap(), fs);
    let cb = if_curve(&db.lock().unwrap(), fs);
    // 有效窗口：RMS 足够大
    let thr = AMP * 0.15;
    let good_a: Vec<_> = ca.iter().filter(|(_, _, r)| *r > thr).cloned().collect();
    let good_b: Vec<_> = cb.iter().filter(|(_, _, r)| *r > thr).cloned().collect();
    println!(
        "有效窗口：A {} 个，B {} 个（共 {}）",
        good_a.len(),
        good_b.len(),
        ca.len()
    );
    if good_a.is_empty() || good_b.is_empty() {
        println!("采到的信号太弱");
        return;
    }
    // 找 A→B 的延迟（对齐频率曲线）
    let mut best = (0f32, f32::MAX);
    let mut lag = 0.0f32;
    let span = good_a.last().map(|v| v.0).unwrap_or(0.0) - good_a[0].0;
    while lag < 2.0 {
        let mut sum = 0f32;
        let mut n = 0;
        for (t, f, _) in &good_a {
            let tt = t + lag;
            if let Some((_, fb2, _)) = good_b.iter().find(|(tb, _, _)| (tb - tt).abs() < WIN / 2.0) {
                sum += (f - fb2).abs();
                n += 1;
            }
        }
        if n > 10 {
            let avg = sum / n as f32;
            if avg < best.1 {
                best = (lag, avg);
            }
        }
        lag += WIN;
    }
    println!(
        "频率曲线对齐：延迟 {:.0} ms（扫频跨度 {:.1}s），平均频差 {:.1}Hz",
        best.0 * 1000.0,
        span,
        best.1
    );
    // 对齐后的逐点比较
    println!("\n  t(A)    f(A)     f(B对齐)  偏差     判定");
    let mut anomalies = 0;
    let mut checked = 0;
    for (t, f, _) in good_a.iter() {
        let tt = t + best.0;
        if let Some((_, fb2, _)) = good_b.iter().find(|(tb, _, _)| (tb - tt).abs() < WIN / 2.0) {
            let dev = fb2 - f;
            let rel = dev / f.max(1.0);
            checked += 1;
            let bad = rel.abs() > 0.03;
            if bad {
                anomalies += 1;
            }
            // 只打印部分行，避免刷屏
            if bad || checked % 25 == 0 {
                println!(
                    "{:6.2}s  {:7.1}  {:7.1}  {:>+7.1}  {:>6.1}%  {}",
                    t,
                    f,
                    fb2,
                    dev,
                    rel * 100.0,
                    if bad { "← 异常（丢/重样）" } else { "" }
                );
            }
        }
    }
    println!(
        "\n共比对 {checked} 个窗口，偏差 >3% 的 {anomalies} 个（{:.1}%）",
        100.0 * anomalies as f32 / checked.max(1) as f32
    );
    println!(
        "结论：{}",
        if anomalies * 20 > checked {
            "❌ 扫频曲线对不上 → 这条链上确实在丢/重样（问题在我们这段）"
        } else if anomalies > 0 {
            "⚠️ 有少量异常窗口"
        } else {
            "✅ 扫频曲线完全对得上 → 我们这段没有丢样/重样"
        }
    );
}
