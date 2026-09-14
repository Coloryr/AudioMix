//! 临时诊断（用完即删）：**功能级**验证麦克风出音（不纠结细粒度抖动）。
//! 播 1kHz 正弦进虚拟声卡，录虚拟麦克风端，检查：频率是否稳、电平是否稳、有没有缺口。
//! 这是"听起来是否正常"最直接的量化指标。

use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audiomix_backend_windows::wasapi::capture;
use audiomix_backend_windows::wasapi::device::enumerate_devices;
use audiomix_backend_windows::wasapi::render;
use audiomix_core::model::DeviceKind;

const HZ: f32 = 1000.0;
const AMP: f32 = 0.25;
const SECS: f32 = 8.0;

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

fn main() {
    // 可选参数：线路名过滤（默认取第一条 "Virtual Cable"），用来逐条线路跑格式矩阵
    let filter = std::env::args().nth(1).unwrap_or_else(|| "Virtual Cable".to_string());
    let devices = match enumerate_devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("枚举失败: {e}");
            return;
        }
    };
    let vc_out = devices
        .iter()
        .find(|d| d.name.contains(&filter) && d.kind == DeviceKind::Output)
        .cloned();
    let vc_in = devices
        .iter()
        .find(|d| d.name.contains(&filter) && d.kind == DeviceKind::Input)
        .cloned();
    let (Some(vc_out), Some(vc_in)) = (vc_out, vc_in) else {
        println!("(缺少虚拟声卡端点: {filter})");
        return;
    };
    println!(
        "线路「{}」播放端 {}Hz/{}ch · 录音端 {}Hz/{}ch",
        filter, vc_out.sample_rate, vc_out.channels, vc_in.sample_rate, vc_in.channels
    );

    let rec = Arc::new(Mutex::new(Vec::<f32>::new()));
    let r2 = rec.clone();
    let cap = capture::start_capture(
        &vc_in.id,
        false,
        Box::new(move |d| {
            let mut v = r2.lock().unwrap();
            for &x in d.iter().step_by(2) {
                v.push(x);
            }
        }),
    );
    let cap = match cap {
        Ok(c) => c,
        Err(e) => {
            eprintln!("打开麦克风失败: {e}");
            return;
        }
    };
    let fs = cap.info.sample_rate as f32;
    println!("录音 {fs}Hz（虚拟麦克风）");

    let ch = vc_out.channels.max(1) as usize;
    let rate = vc_out.sample_rate.max(1) as f32;
    let phase = Arc::new(AtomicU32::new(0));
    let ph = phase.clone();
    let play = render::start_render(
        &vc_out.id,
        Box::new(move |out| {
            let mut idx = ph.load(std::sync::atomic::Ordering::Relaxed);
            for frame in out.chunks_mut(ch) {
                let v = (idx as f32 * HZ * std::f32::consts::TAU / rate).sin() * AMP;
                idx += 1;
                for y in frame.iter_mut() {
                    *y = v;
                }
            }
            ph.store(idx, std::sync::atomic::Ordering::Relaxed);
        }),
    );
    let _play = match play {
        Ok(p) => p,
        Err(e) => {
            eprintln!("打开播放端失败: {e}");
            return;
        }
    };
    println!("播 {HZ}Hz/{AMP} 共 {SECS}s…");
    std::thread::sleep(Duration::from_secs_f32(SECS));
    drop(_play);
    drop(cap);

    let d = rec.lock().unwrap().clone();
    let a = (fs * 1.5) as usize;
    let b = ((fs * 6.5) as usize).min(d.len());
    if b <= a + 4800 {
        println!("数据太少（{} 样本）", d.len());
        return;
    }
    let seg = &d[a..b];
    // 频率（过零）
    let mut first = None;
    let mut last = 0usize;
    let mut cy = 0usize;
    for i in 1..seg.len() {
        if seg[i - 1] <= 0.0 && seg[i] > 0.0 {
            if first.is_none() {
                first = Some(i);
            } else {
                cy += 1;
            }
            last = i;
        }
    }
    let freq = match first {
        Some(f) if cy > 0 => cy as f32 * fs / (last - f) as f32,
        _ => 0.0,
    };
    // 电平（100ms 窗口）
    let win = (fs * 0.1) as usize;
    let mut rr: Vec<f32> = seg.chunks(win).map(rms).collect();
    rr.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let med = rr[rr.len() / 2];
    // 精确零缺口
    let mut run = 0usize;
    let mut longest = 0usize;
    for &v in seg {
        if v == 0.0 {
            run += 1;
        } else {
            longest = longest.max(run);
            run = 0;
        }
    }
    println!(
        "实测频率 {freq:.2}Hz（期望 {HZ}）→ 倍率 {:.4}",
        freq / HZ
    );
    println!(
        "100ms RMS：中位 {med:.4}，最小 {:.4}（{:.1} dB 相对中位），最大 {:.4}",
        rr[0],
        if rr[0] > 0.0 { 20.0 * (rr[0] / med).log10() } else { -99.0 },
        rr[rr.len() - 1]
    );
    println!("最长静音缺口 {:.1}ms", longest as f32 / fs * 1000.0);
    println!(
        "结论：{}",
        if (freq / HZ - 1.0).abs() < 0.02 && med > AMP * 0.2 && rr[0] > med * 0.5 && longest < fs as usize / 100
        {
            "✅ 麦克风出音功能正常（频率稳、电平稳、无缺口）"
        } else {
            "❌ 麦克风出音仍有问题"
        }
    );
}
