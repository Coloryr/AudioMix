//! 临时诊断（用完即删）：**立体声专属**问题的自检。
//!
//! 之前的测试音左右声道写的是同一个值（等于单声道），所以任何"左右声道之间"的
//! 问题都测不出来。这里放 L=1kHz、R=3kHz，然后在 Minifuse 的 loopback 上分别量
//! 两个声道的谱线：
//! - 串音（左里听到 3kHz / 右里听到 1kHz）：> -40dB 就是问题；
//! - 左右是否被交换、是否有一个声道被延迟（会导致"发毛发涩、不流畅"）。

use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audiomix_backend_windows::wasapi::capture;
use audiomix_backend_windows::wasapi::device::enumerate_devices;
use audiomix_backend_windows::wasapi::render;
use audiomix_core::model::DeviceKind;

const L_HZ: f32 = 1000.0;
const R_HZ: f32 = 3000.0;
const AMP: f32 = 0.2;
const SECS: f32 = 6.0;

fn goertzel(x: &[f32], freq: f32, fs: f32) -> f32 {
    let n = x.len();
    if n == 0 {
        return 0.0;
    }
    let k = (freq * n as f32 / fs).round();
    let w = 2.0 * std::f32::consts::PI * k / n as f32;
    let cw = w.cos();
    let coeff = 2.0 * cw;
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &v in x {
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let real = s1 - s2 * cw;
    let imag = s2 * w.sin();
    2.0 * (real * real + imag * imag).sqrt() / n as f32
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

    // 左右分开记录
    let left = Arc::new(Mutex::new(Vec::<f32>::new()));
    let right = Arc::new(Mutex::new(Vec::<f32>::new()));
    let (l2, r2) = (left.clone(), right.clone());
    let cap = capture::start_capture(
        &main_out.id,
        true,
        Box::new(move |d| {
            let mut lv = l2.lock().unwrap();
            let mut rv = r2.lock().unwrap();
            for f in d.chunks_exact(2) {
                lv.push(f[0]);
                rv.push(f[1]);
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
    println!(
        "播放 L={L_HZ}Hz / R={R_HZ}Hz（各 {AMP}），采 Minifuse 回环 {}Hz/{}ch",
        cap.info.sample_rate, cap.info.channels
    );

    let ch = vc_out.channels.max(1) as usize;
    let rate = vc_out.sample_rate.max(1) as f32;
    let phase = Arc::new(AtomicU32::new(0));
    let ph = phase.clone();
    let play = render::start_render(
        &vc_out.id,
        Box::new(move |out| {
            let mut idx = ph.load(std::sync::atomic::Ordering::Relaxed);
            for frame in out.chunks_mut(ch) {
                let t = idx as f32 / rate;
                let l = (t * L_HZ * std::f32::consts::TAU).sin() * AMP;
                let r = (t * R_HZ * std::f32::consts::TAU).sin() * AMP;
                if ch >= 2 {
                    frame[0] = l;
                    frame[1] = r;
                    for y in frame[2..].iter_mut() {
                        *y = 0.0;
                    }
                } else {
                    frame[0] = l;
                }
                idx += 1;
            }
            ph.store(idx, std::sync::atomic::Ordering::Relaxed);
        }),
    );
    let _play = match play {
        Ok(p) => p,
        Err(e) => {
            eprintln!("打开线缆播放端失败: {e}");
            return;
        }
    };
    std::thread::sleep(Duration::from_secs_f32(SECS));
    drop(_play);
    drop(cap);

    let l = left.lock().unwrap().clone();
    let r = right.lock().unwrap().clone();
    // 取中间 3 秒（跳过起止）
    let a = (fs * 1.5) as usize;
    let b = ((fs * 4.5) as usize).min(l.len()).min(r.len());
    if b <= a + 1000 {
        println!("采到的数据太少");
        return;
    }
    let lw = &l[a..b];
    let rw = &r[a..b];
    let (ll, lr) = (goertzel(lw, L_HZ, fs), goertzel(lw, R_HZ, fs));
    let (rl, rr) = (goertzel(rw, L_HZ, fs), goertzel(rw, R_HZ, fs));
    println!("\n左声道：{L_HZ}Hz = {ll:.4}（应≈{AMP}），{R_HZ}Hz = {lr:.4}");
    println!("右声道：{R_HZ}Hz = {rr:.4}（应≈{AMP}），{L_HZ}Hz = {rl:.4}");
    let db = |x: f32| 20.0 * x.max(1e-9).log10();
    println!(
        "串音：左里的右声道 {:.1} dB，右里的左声道 {:.1} dB（< -40dB 算正常）",
        db(lr / ll.max(1e-9)),
        db(rl / rr.max(1e-9))
    );
    // 左右相位/延迟：把右声道与"左声道移频后"对比没意义，改为直接看两声道互相关是否有一个非零最佳延迟
    // （若某个声道被延迟 1 个样本以上，会在这里露出来）
    let mut best = (0i32, f32::MIN);
    for lag in -40i32..=40 {
        let mut dot = 0f32;
        let mut n = 0;
        for i in 40..lw.len().saturating_sub(40) {
            let j = i as i32 + lag;
            if j < 0 || j as usize >= rw.len() {
                continue;
            }
            dot += lw[i] * rw[j as usize];
            n += 1;
        }
        let v = if n > 0 { dot / n as f32 } else { 0.0 };
        if v > best.1 {
            best = (lag, v);
        }
    }
    println!("左右互相关最佳延迟（仅参考，两声道内容不同所以值本身很小）：{} 样本", best.0);
    println!(
        "\n结论：{}",
        if db(lr / ll.max(1e-9)) < -40.0 && db(rl / rr.max(1e-9)) < -40.0 && ll > AMP * 0.5 && rr > AMP * 0.5 {
            "✅ 左右声道完全分离、无交换、无串音"
        } else {
            "❌ 立体声有问题（串音/交换/某声道缺失）"
        }
    );
}
