//! 临时诊断（用完即删）：解剖一个 16bit PCM WAV，找"卡"的病灶。
//!
//! 输出：
//! - 时长/峰值/RMS、100ms RMS 的最小值（掉音）
//! - 静音缺口（连续精确 0）
//! - 相同样本串（保持/重复）≥2/≥4/≥8，以及它们**间隔的分布**（周期性能指向机制：
//!   每 1ms = USB 帧、每 ~10ms = 喂数节拍、每 250ms = 环形缓冲…）
//! - 二阶差分尖峰（爆音/波形被掰断）的数量与间隔分布

use std::fs;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        r"H:\Temp\AudioMix\M_虚拟麦克风端.wav".to_string()
    });
    let raw = match fs::read(&path) {
        Ok(r) => r,
        Err(e) => {
            println!("读 {path} 失败: {e}");
            return;
        }
    };
    // 找 data chunk
    let mut pos = 12usize;
    let mut data_start = 44usize;
    let mut ch = 2u16;
    let mut rate = 48000u32;
    let mut bits = 16u16;
    while pos + 8 <= raw.len() {
        let id = &raw[pos..pos + 4];
        let sz = u32::from_le_bytes([raw[pos + 4], raw[pos + 5], raw[pos + 6], raw[pos + 7]]) as usize;
        if id == b"fmt " {
            ch = u16::from_le_bytes([raw[pos + 10], raw[pos + 11]]);
            rate = u32::from_le_bytes([raw[pos + 12], raw[pos + 13], raw[pos + 14], raw[pos + 15]]);
            bits = u16::from_le_bytes([raw[pos + 22], raw[pos + 23]]);
        } else if id == b"data" {
            data_start = pos + 8;
            break;
        }
        pos += 8 + sz + (sz & 1);
    }
    if bits != 16 {
        println!("只支持 16bit，实际 {bits}");
        return;
    }
    let n = (raw.len() - data_start) / 2;
    let samples: Vec<i16> = (0..n)
        .map(|i| i16::from_le_bytes([raw[data_start + i * 2], raw[data_start + i * 2 + 1]]))
        .collect();
    let ch = ch.max(1) as usize;
    let frames = samples.len() / ch;
    println!(
        "文件 {path}\n  {}Hz / {}ch / {:.2}s（{} 样本）",
        rate,
        ch,
        frames as f32 / rate as f32,
        samples.len()
    );
    let f = |v: i16| v as f32 / 32768.0;
    let peak = samples.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0);
    let sum: f64 = samples.iter().map(|v| (f(*v) as f64).powi(2)).sum();
    println!(
        "  峰值 {:.4}，RMS {:.4}",
        peak as f32 / 32768.0,
        (sum / samples.len().max(1) as f64).sqrt()
    );

    // 100ms RMS
    let win = rate as usize / 10 * ch;
    let mut rms: Vec<f32> = Vec::new();
    for c in samples.chunks(win) {
        let s: f64 = c.iter().map(|v| (f(*v) as f64).powi(2)).sum();
        rms.push((s / c.len().max(1) as f64).sqrt() as f32);
    }
    let mut rs = rms.clone();
    rs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "  100ms RMS：中位 {:.4}，最小 {:.4}，最小时刻 {:.2}s；完全静音窗口 {}/{}",
        rs[rs.len() / 2],
        rs[0],
        rms.iter().position(|v| *v == rs[0]).unwrap_or(0) as f32 / 10.0,
        rs.iter().filter(|v| **v == 0.0).count(),
        rs.len()
    );

    // 静音缺口
    let mut zrun = 0usize;
    let mut zmax = 0usize;
    let mut ztotal = 0usize;
    for &v in &samples {
        if v == 0 {
            zrun += 1;
        } else {
            if zrun > 0 {
                ztotal += zrun;
            }
            zmax = zmax.max(zrun);
            zrun = 0;
        }
    }
    zmax = zmax.max(zrun);
    println!(
        "  静音缺口：合计 {:.1}ms，最长 {:.1}ms",
        ztotal as f32 / (rate as f32 * ch as f32) * 1000.0,
        zmax as f32 / (rate as f32 * ch as f32) * 1000.0
    );

    // 相同样本串（按通道分别看）
    for thr in [2usize, 4, 8] {
        let mut poses: Vec<usize> = Vec::new();
        let mut total = 0usize;
        for c in 0..ch {
            let mut run = 1usize;
            let mut prev = samples[c];
            let mut i = c + ch;
            while i < samples.len() {
                let v = samples[i];
                if v == prev && v != 0 {
                    run += 1;
                } else {
                    if run >= thr {
                        total += run;
                        poses.push(i / ch);
                    }
                    run = 1;
                }
                prev = v;
                i += ch;
            }
        }
        print!("  相同样本≥{thr}：合计 {}", total);
        // 间隔分布（取前 3 个高频间隔）
        if poses.len() >= 3 {
            poses.sort_unstable();
            poses.dedup();
            let mut gaps: Vec<u32> = poses.windows(2).map(|w| w[1].saturating_sub(w[0]) as u32).collect();
            gaps.sort_unstable();
            let mut best: Vec<(u32, usize)> = Vec::new();
            let mut i = 0;
            while i < gaps.len() {
                let g = gaps[i];
                let mut j = i;
                while j < gaps.len() && gaps[j] == g {
                    j += 1;
                }
                best.push((g, j - i));
                i = j;
            }
            best.sort_by(|a, b| b.1.cmp(&a.1));
            print!("，{} 处，间隔最常见：", poses.len());
            for (g, cnt) in best.iter().take(3) {
                print!(" {:.2}ms×{}", *g as f32 / rate as f32 * 1000.0, cnt);
            }
        }
        println!();
    }

    // 二阶差分尖峰（爆音/断点）
    let mut d2max = 0i32;
    let mut spikes: Vec<usize> = Vec::new();
    for c in 0..ch {
        let mut a = samples.get(c).copied().unwrap_or(0) as i32;
        let mut b = samples.get(c + ch).copied().unwrap_or(0) as i32;
        let mut i = c + 2 * ch;
        while i < samples.len() {
            let v = samples[i] as i32;
            let d2 = (v - 2 * b + a).abs();
            if d2 > d2max {
                d2max = d2;
            }
            if d2 > 2000 {
                spikes.push(i / ch);
            }
            a = b;
            b = v;
            i += ch;
        }
    }
    println!("  二阶差分最大 {d2max}（满幅 32768 的 {:.1}%）；>2000 的尖峰 {} 处", d2max as f32 / 32768.0 * 100.0, spikes.len());
    if spikes.len() >= 3 {
        spikes.sort_unstable();
        spikes.dedup();
        let mut gaps: Vec<u32> = spikes.windows(2).map(|w| w[1].saturating_sub(w[0]) as u32).collect();
        gaps.sort_unstable();
        let mut best: Vec<(u32, usize)> = Vec::new();
        let mut i = 0;
        while i < gaps.len() {
            let g = gaps[i];
            let mut j = i;
            while j < gaps.len() && gaps[j] == g {
                j += 1;
            }
            best.push((g, j - i));
            i = j;
        }
        best.sort_by(|a, b| b.1.cmp(&a.1));
        print!("    尖峰间隔最常见：");
        for (g, cnt) in best.iter().take(5) {
            print!(" {:.2}ms×{}", *g as f32 / rate as f32 * 1000.0, cnt);
        }
        println!();
    }
}
