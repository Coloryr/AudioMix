//! 临时诊断（用完即删）：判定「虚拟声卡 → 混音器 → 物理输出」这条链是否**透明**。
//!
//! 现在线缆和 Minifuse 都是 48k，所以两路可以直接逐样本比对：
//!   A = 虚拟声卡播放端的 loopback（Windows 交给我们的信号）
//!   B = Minifuse 的 loopback（我们交给物理设备的信号）
//! 1) 用抽取后的互相关求最佳延迟（预期 ~300ms）；
//! 2) 在该延迟下算残差 RMS（B − A）/ RMS(B)：很小 ⇒ 这条链没加东西；
//! 3) 分别统计两路的静音缺口 / 冻结样本 / 逐样本残差尖峰。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audiomix_backend_windows::wasapi::capture;
use audiomix_backend_windows::wasapi::device::enumerate_devices;
use audiomix_core::backend::StartedStream;
use audiomix_core::model::DeviceKind;

fn secs() -> f32 {
    std::env::var("VC_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(12.0)
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

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

/// 抽取（块平均）到 1/DEC
fn decimate(x: &[f32], dec: usize) -> Vec<f32> {
    x.chunks(dec).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect()
}

fn stats(label: &str, data: &[f32], fs: f32) {
    let mut zero_run = 0usize;
    let mut zero_long = 0usize;
    let mut same_run = 1usize;
    let mut same_long = 0usize;
    let mut same_total = 0usize;
    let mut same2_total = 0usize;
    for i in 1..data.len() {
        if data[i] == 0.0 {
            zero_run += 1;
        } else {
            zero_long = zero_long.max(zero_run);
            zero_run = 0;
        }
        if data[i] == data[i - 1] {
            same_run += 1;
            if same_run == 2 {
                same2_total += 2;
            } else {
                same2_total += 1;
            }
        } else {
            if same_run >= 4 {
                same_total += same_run;
            }
            same_long = same_long.max(same_run);
            same_run = 1;
        }
    }
    if same_run >= 4 {
        same_total += same_run;
    }
    zero_long = zero_long.max(zero_run);
    same_long = same_long.max(same_run);
    println!(
        "[{label}] 样本 {:.1}s，RMS {:.4}；最长静音缺口 {:.1}ms；冻结≥2 合计 {:.1}ms、冻结≥4 合计 {:.1}ms（最长 {} 样本）",
        data.len() as f32 / fs,
        rms(data),
        zero_long as f32 / fs * 1000.0,
        same2_total as f32 / fs * 1000.0,
        same_total as f32 / fs * 1000.0,
        same_long
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
    let main_out = devices
        .iter()
        .find(|d| d.kind == DeviceKind::Output && d.name.contains("Minifuse"))
        .cloned();
    let (Some(vc_out), Some(main_out)) = (vc_out, main_out) else {
        println!("(缺少线缆播放端或主播放设备)");
        return;
    };
    println!("A 设备: {} ({}Hz)", vc_out.name, vc_out.sample_rate);
    println!("B 设备: {} ({}Hz)", main_out.name, main_out.sample_rate);

    let a = start("A 虚拟声卡播放端", &vc_out.id);
    let b = start("B Minifuse 输出", &main_out.id);
    let secs = secs();
    // 可选：自己放一段 1kHz 测试音（VC_TONE=1）或白噪声（VC_NOISE=1）
    let tone = std::env::var("VC_TONE").map(|v| v == "1").unwrap_or(false);
    let noise = std::env::var("VC_NOISE").map(|v| v == "1").unwrap_or(false);
    let mut _play: Option<StartedStream> = None;
    if tone || noise {
        use std::sync::atomic::AtomicU32 as AU;
        let phase = Arc::new(AU::new(0));
        let ph = phase.clone();
        let ch = vc_out.channels.max(1) as usize;
        let rate = vc_out.sample_rate.max(1) as f32;
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        _play = audiomix_backend_windows::wasapi::render::start_render(
            &vc_out.id,
            Box::new(move |out| {
                let mut idx = ph.load(Ordering::Relaxed);
                for frame in out.chunks_mut(ch) {
                    let v = if noise {
                        // xorshift 白噪声：连续相同样本几乎不可能自然出现
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        ((seed >> 11) as f64 / (1u64 << 53) as f64) as f32 * 0.4 - 0.2
                    } else {
                        (idx as f32 * 1000.0 * std::f32::consts::TAU / rate).sin() * 0.25
                    };
                    idx += 1;
                    for y in frame.iter_mut() {
                        *y = v;
                    }
                }
                ph.store(idx, Ordering::Relaxed);
            }),
        )
        .ok();
        println!("（自测模式：正在放{}）", if noise { "白噪声" } else { "1kHz 测试音" });
    }
    println!(
        "\n采集 {secs}s —— {}…\n",
        if tone || noise { "自测" } else { "请现在放歌（保持连续播放）" }
    );
    std::thread::sleep(Duration::from_secs_f32(secs));
    drop(_play);

    let (Some((da, fa, _ha)), Some((db, fb, _hb))) = (&a, &b) else {
        println!("某一路没起来");
        return;
    };
    let (fa, fb) = (*fa, *fb);
    let a = da.lock().unwrap().clone();
    let b = db.lock().unwrap().clone();
    if fa != fb {
        println!("两路采样率不同（{fa} vs {fb}），无法直接比对");
        return;
    }
    stats("A Windows→我们", &a, fa);
    stats("B 我们→Minifuse", &b, fb);

    // 互相关（抽取 16 倍）求延迟
    const DEC: usize = 16;
    let ad = decimate(&a, DEC);
    let bd = decimate(&b, DEC);
    let n = ad.len().min(bd.len());
    let max_lag = ((fa as usize / DEC) as f32 * 0.8) as usize; // 最多 800ms
    let mut best = (0usize, f32::MIN);
    // B[n] ≈ A[n - lag]
    for lag in 0..max_lag.min(n / 2) {
        let len = n - lag;
        let mut dot = 0f32;
        let mut na = 0f32;
        let mut nb = 0f32;
        for i in 0..len {
            let x = ad[i];
            let y = bd[i + lag];
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        let corr = if na > 0.0 && nb > 0.0 { dot / (na.sqrt() * nb.sqrt()) } else { 0.0 };
        if corr > best.1 {
            best = (lag, corr);
        }
    }
    let coarse = best.0 * DEC;
    println!(
        "\n互相关：粗延迟 {} 样本（{:.0} ms），相关系数 {:.4}",
        coarse,
        coarse as f32 / fa * 1000.0,
        best.1
    );
    // 全速细化 ±DEC
    let mut best_lag = coarse;
    let mut best_corr = f32::MIN;
    let lo = coarse.saturating_sub(DEC * 2);
    let hi = (coarse + DEC * 2).min(b.len().saturating_sub(1));
    for lag in lo..=hi {
        let len = (a.len()).min(b.len() - lag);
        if len < 1000 {
            continue;
        }
        let mut dot = 0f32;
        let mut na = 0f32;
        let mut nb = 0f32;
        for i in 0..len {
            dot += a[i] * b[i + lag];
            na += a[i] * a[i];
            nb += b[i + lag] * b[i + lag];
        }
        let corr = if na > 0.0 && nb > 0.0 { dot / (na.sqrt() * nb.sqrt()) } else { 0.0 };
        if corr > best_corr {
            best_corr = corr;
            best_lag = lag;
        }
    }
    println!(
        "细化延迟 {} 样本（{:.1} ms），相关系数 {:.4}",
        best_lag,
        best_lag as f32 / fa * 1000.0,
        best_corr
    );

    // 两路是**不同设备**的采集流，时钟有漂移 → 单一批延迟对不齐整段，
    // 必须逐窗口各自对齐（并在窗口间跟踪延迟）。
    let win = (fa * 0.5) as usize;
    let search = 128usize; // 每个窗口在上一窗口延迟 ±128 样本内搜索
    let mut lag = coarse;
    let mut corrs: Vec<f32> = Vec::new();
    let mut resids: Vec<f32> = Vec::new();
    let mut gains: Vec<f32> = Vec::new();
    let mut worst_resid = 0f32;
    let mut start = 0usize;
    while start + win < a.len() && start + win + lag + search < b.len() {
        let a_win = &a[start..start + win];
        let lo = lag.saturating_sub(search);
        let hi = lag + search;
        let mut best = (lag, f32::MIN, 0f32);
        for l in lo..=hi {
            if l + win >= b.len() {
                break;
            }
            let b_win = &b[l..l + win];
            let mut dot = 0f64;
            let mut na = 0f64;
            let mut nb = 0f64;
            for i in 0..win {
                dot += a_win[i] as f64 * b_win[i] as f64;
                na += a_win[i] as f64 * a_win[i] as f64;
                nb += b_win[i] as f64 * b_win[i] as f64;
            }
            let corr = if na > 0.0 && nb > 0.0 { (dot / (na.sqrt() * nb.sqrt())) as f32 } else { 0.0 };
            if corr > best.1 {
                best = (l, corr, if na > 0.0 { (dot / na) as f32 } else { 1.0 });
            }
        }
        lag = best.0;
        let b_win = &b[lag..lag + win];
        let g = best.2;
        let mut res = Vec::with_capacity(win);
        for i in 0..win {
            res.push(b_win[i] - g * a_win[i]);
        }
        let rr = rms(&res);
        let rb = rms(b_win);
        if rb > 1e-5 {
            corrs.push(best.1);
            resids.push(rr / rb);
            gains.push(g);
            if rr / rb > worst_resid {
                worst_resid = rr / rb;
            }
        }
        start += win;
    }
    if corrs.is_empty() {
        println!("没有有效窗口（是不是没在放声音？）");
        return;
    }
    let mut cs = corrs.clone();
    cs.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let mut rs2 = resids.clone();
    rs2.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let mut gs = gains.clone();
    gs.sort_by(|x, y| x.partial_cmp(y).unwrap());
    println!(
        "\n逐 0.5s 窗口（各自对齐后）：{} 个窗口\n  相关系数  最小 {:.4} / 中位 {:.4}\n  残差占比  中位 {:.4}（{:.1} dB）/ 最差 {:.4}\n  音量增益  最小 {:.3} / 中位 {:.3} / 最大 {:.3}",
        corrs.len(),
        cs[0],
        cs[cs.len() / 2],
        rs2[rs2.len() / 2],
        20.0 * rs2[rs2.len() / 2].max(1e-9).log10(),
        rs2[rs2.len() - 1],
        gs[0],
        gs[gs.len() / 2],
        gs[gs.len() - 1]
    );
    println!(
        "\n结论：{}",
        if cs[cs.len() / 2] > 0.9 {
            "✅ 这条链是透明的（B 与 A 对齐后几乎相同）→ 卡不是在这条链上引入的"
        } else if cs[cs.len() / 2] > 0.5 {
            "⚠️ 只对上一部分：这条链改动了信号（看残差/增益分布）"
        } else {
            "❌ B 与 A 基本对不上（要么延迟超出搜索范围，要么内容被改）"
        }
    );
}
