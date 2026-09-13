//! 临时诊断（用完即删）：把三个位置的**数字信号**录成 16bit/48k 立体声 WAV，
//! 用别的播放器/设备听，就能判断"卡"到底存在于哪一段：
//!
//!   A_虚拟声卡播放端.wav  = Windows/播放器交给我们设备的信号（这条已经"卡"就与我们无关）
//!   M_虚拟麦克风端.wav    = 我们从虚拟麦克风送出去的东西（loopback 模式下 = A 的回环）
//!   B_Minifuse输出.wav    = 混音器送给物理设备的信号
//!
//! 三份都在 `H:\Temp\AudioMix\`。

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audiomix_backend_windows::wasapi::capture;
use audiomix_backend_windows::wasapi::device::enumerate_devices;
use audiomix_core::backend::StartedStream;
use audiomix_core::model::DeviceKind;

const SECS: f32 = 12.0;
const OUT_DIR: &str = r"H:\Temp\AudioMix";

struct Tap {
    label: &'static str,
    file: String,
    rec: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    ch: u16,
    _stream: StartedStream,
}

fn start(label: &'static str, file: &str, id: &str, loopback: bool) -> Option<Tap> {
    let rec = Arc::new(Mutex::new(Vec::<f32>::new()));
    let r2 = rec.clone();
    let peak = Arc::new(AtomicU32::new(0));
    let p2 = peak.clone();
    let s = capture::start_capture(
        id,
        loopback,
        Box::new(move |d| {
            let mut v = r2.lock().unwrap();
            let mut m = 0f32;
            for &x in d {
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
    println!(
        "{label}: {}Hz/{}ch → {file}",
        s.info.sample_rate, s.info.channels
    );
    Some(Tap {
        label,
        file: file.to_string(),
        rec,
        rate: s.info.sample_rate,
        ch: s.info.channels,
        _stream: s,
    })
}

fn write_wav(tap: &Tap) {
    let data = tap.rec.lock().unwrap();
    let frames = data.len() / tap.ch.max(1) as usize;
    let path = format!("{OUT_DIR}\\{}", tap.file);
    let f = match File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            println!("写 {} 失败: {e}", tap.file);
            return;
        }
    };
    let mut w = BufWriter::new(f);
    let data_bytes = (frames * tap.ch as usize * 2) as u32;
    let byte_rate = tap.rate * tap.ch as u32 * 2;
    let block_align = (tap.ch * 2) as u16;
    let _ = w.write_all(b"RIFF");
    let _ = w.write_all(&(36 + data_bytes).to_le_bytes());
    let _ = w.write_all(b"WAVEfmt ");
    let _ = w.write_all(&16u32.to_le_bytes());
    let _ = w.write_all(&1u16.to_le_bytes()); // PCM
    let _ = w.write_all(&tap.ch.to_le_bytes());
    let _ = w.write_all(&tap.rate.to_le_bytes());
    let _ = w.write_all(&byte_rate.to_le_bytes());
    let _ = w.write_all(&block_align.to_le_bytes());
    let _ = w.write_all(&16u16.to_le_bytes());
    let _ = w.write_all(b"data");
    let _ = w.write_all(&data_bytes.to_le_bytes());
    let mut peak = 0f32;
    let mut sum = 0f64;
    let mut n = 0u64;
    let mut buf = Vec::with_capacity(4096);
    for &v in data.iter() {
        let c = v.clamp(-1.0, 1.0);
        if c.abs() > peak {
            peak = c.abs();
        }
        sum += (c as f64) * (c as f64);
        n += 1;
        buf.push((c * 32767.0) as i16);
        if buf.len() >= 4096 {
            for s in &buf {
                let _ = w.write_all(&s.to_le_bytes());
            }
            buf.clear();
        }
    }
    for s in &buf {
        let _ = w.write_all(&s.to_le_bytes());
    }
    let _ = w.flush();
    let rms = if n > 0 { (sum / n as f64).sqrt() } else { 0.0 };
    println!(
        "  [{0}] {1:.1}s，峰值 {2:.4}，RMS {3:.4}（RMS=0 就是一路静音）",
        tap.label,
        frames as f32 / tap.rate as f32,
        peak,
        rms
    );
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
    let vc_in = devices
        .iter()
        .find(|d| d.name.contains("Virtual Cable") && d.kind == DeviceKind::Input)
        .cloned();
    let main_out = devices
        .iter()
        .find(|d| d.kind == DeviceKind::Output && d.name.contains("Minifuse"))
        .cloned();
    let (Some(vc_out), Some(vc_in), Some(main_out)) = (vc_out, vc_in, main_out) else {
        println!("(缺少某个端点)");
        return;
    };
    std::fs::create_dir_all(OUT_DIR).ok();

    let mut taps: Vec<Tap> = Vec::new();
    if let Some(t) = start("A 虚拟声卡播放端", "A_虚拟声卡播放端.wav", &vc_out.id, true) {
        taps.push(t);
    }
    if let Some(t) = start("M 虚拟麦克风端", "M_虚拟麦克风端.wav", &vc_in.id, false) {
        taps.push(t);
    }
    if let Some(t) = start("B Minifuse 输出", "B_Minifuse输出.wav", &main_out.id, true) {
        taps.push(t);
    }
    println!("\n录 {SECS}s —— 请现在开始播放（保持连续）…\n");
    // 自测模式：白噪声（VC_NOISE=1）或"序列"信号（VC_RAMP=1，每帧 +1 LSB 的低幅锯齿，
    // 用来精确检出丢样/重样：接收端出现 +2 LSB 就是丢了 1 个样本，+0 就是重复）
    let noise = std::env::var("VC_NOISE").map(|v| v == "1").unwrap_or(false);
    let ramp = std::env::var("VC_RAMP").map(|v| v == "1").unwrap_or(false);
    let mut _play = None;
    if noise || ramp {
        let ch = vc_out.channels.max(1) as usize;
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut n: u64 = 0;
        _play = audiomix_backend_windows::wasapi::render::start_render(
            &vc_out.id,
            Box::new(move |out| {
                for frame in out.chunks_mut(ch) {
                    let v = if ramp {
                        // 每帧 +16 LSB 的锯齿：步长必须远大于 Windows 16bit 转换时的抖动(±1 LSB)，
                        // 否则测出来的全是假阳性。512 帧一个周期 → 93.75Hz，幅度 ±0.125。
                        const SPAN: u64 = 512;
                        const STEP: i32 = 16;
                        (((n % SPAN) as i32 * STEP - 4096) as f32) / 32768.0
                    } else {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        ((seed >> 11) as f64 / (1u64 << 53) as f64) as f32 * 0.4 - 0.2
                    };
                    n += 1;
                    for y in frame.iter_mut() {
                        *y = v;
                    }
                }
            }),
        )
        .ok();
        println!("（自测：正在放{}）", if ramp { "序列信号（每帧+1 LSB）" } else { "白噪声" });
    }
    std::thread::sleep(Duration::from_secs_f32(SECS));
    drop(_play);

    println!("写文件：");
    for t in &taps {
        write_wav(t);
    }
    println!("\n完成，文件在 {OUT_DIR}（16bit/48k 立体声 WAV）");
}
