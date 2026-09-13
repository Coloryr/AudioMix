//! 临时诊断（用完即删）：用"每帧 +1 LSB 的序列信号"精确检出**丢样/重样**。
//!
//! 序列信号每帧只加 1 个 LSB，于是接收端相邻样本的差应当是 +1：
//! - 差 = +2 → 中间**丢了 1 个样本**（差值-1 就是丢的个数）
//! - 差 = 0  → **重复**了一个样本
//! - 差为其它值 → 一次丢/重了更多，或者发生了跳变
//! 报告各类事件的次数、总丢/重样本数，以及前若干个事件的时刻。

use std::fs;

fn read_wav(path: &str) -> Option<(Vec<i16>, u32, usize)> {
    let raw = fs::read(path).ok()?;
    let mut pos = 12usize;
    let mut data_start = 44usize;
    let mut ch = 2usize;
    let mut rate = 48000u32;
    while pos + 8 <= raw.len() {
        let id = &raw[pos..pos + 4];
        let sz = u32::from_le_bytes([raw[pos + 4], raw[pos + 5], raw[pos + 6], raw[pos + 7]]) as usize;
        if id == b"fmt " {
            ch = u16::from_le_bytes([raw[pos + 10], raw[pos + 11]]) as usize;
            rate = u32::from_le_bytes([raw[pos + 12], raw[pos + 13], raw[pos + 14], raw[pos + 15]]);
        } else if id == b"data" {
            data_start = pos + 8;
            break;
        }
        pos += 8 + sz + (sz & 1);
    }
    let n = (raw.len() - data_start) / 2;
    let s: Vec<i16> = (0..n)
        .map(|i| i16::from_le_bytes([raw[data_start + i * 2], raw[data_start + i * 2 + 1]]))
        .collect();
    Some((s, rate, ch))
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r"H:\Temp\AudioMix\M_虚拟麦克风端.wav".into());
    // 期望的每帧步长（测试信号用 16 LSB；容差 ±2 以吸收 16bit 抖动）
    let step: i32 = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    let tol = 2i32;
    let Some((s, rate, ch)) = read_wav(&path) else {
        println!("读文件失败: {path}");
        return;
    };
    let frames = s.len() / ch.max(1);
    println!("{path}\n  {}Hz/{}ch/{:.2}s", rate, ch, frames as f32 / rate as f32);
    // 只看第一声道（左右写的是同一个值）
    let x: Vec<i32> = (0..frames).map(|i| s[i * ch] as i32).collect();
    if x.len() < 1000 {
        println!("样本太少");
        return;
    }
    let span = x.iter().max().unwrap() - x.iter().min().unwrap();
    println!(
        "  取值范围 {}（峰峰 {}），首样本 {}",
        span,
        span,
        x[0]
    );
    let mut drops = 0u64; // 丢掉的样本数
    let mut repeats = 0u64; // 重复的样本数
    let mut big = 0u64; // 差值过大（丢/重超过 10 个或方向可疑）
    let mut events: Vec<(usize, i32)> = Vec::new();
    for i in 1..x.len() {
        let d = x[i] - x[i - 1];
        // 回绕（一个周期末 → 周期首）不算异常
        let wrap = d < -(step * 8);
        if wrap || d == 0 || (d - step).abs() <= tol {
            if d == 0 {
                repeats += 1;
                if events.len() < 20 {
                    events.push((i, 0));
                }
            }
            continue;
        }
        if d > step + tol && d < 2000 {
            drops += ((d - step + step / 2) / step) as u64;
            if events.len() < 20 {
                events.push((i, d));
            }
        } else {
            big += 1;
            if events.len() < 20 {
                events.push((i, d));
            }
        }
    }
    println!(
        "  丢样 {drops} 个（差值>1 的次数另计）｜重复 {repeats} 个｜其它跳变 {big} 次"
    );
    if !events.is_empty() {
        println!("  前若干事件（时刻 / 差值）：");
        for (i, d) in events.iter().take(20) {
            println!(
                "    t={:8.3}s  样本#{:<9} 差值 {:>5}（{}）",
                *i as f32 / rate as f32,
                i,
                d,
                if *d > 1 {
                    format!("丢了 {} 个", d - 1)
                } else if *d == 0 {
                    "重复 1 个".to_string()
                } else {
                    "异常".to_string()
                }
            );
        }
    }
    // 事件间隔分布（周期性 = 机制性故障）
    if events.len() >= 3 {
        let mut gaps: Vec<i64> = events.windows(2).map(|w| w[1].0 as i64 - w[0].0 as i64).collect();
        gaps.sort_unstable();
        println!(
            "  事件间隔：最小 {:.2}ms / 中位 {:.2}ms / 最大 {:.2}ms",
            gaps[0] as f32 / rate as f32 * 1000.0,
            gaps[gaps.len() / 2] as f32 / rate as f32 * 1000.0,
            gaps[gaps.len() - 1] as f32 / rate as f32 * 1000.0
        );
    }
}
