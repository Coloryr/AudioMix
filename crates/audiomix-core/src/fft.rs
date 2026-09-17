//! 频谱分析：单声道环形缓冲 + Hann 窗 + FFT → 可配置的固定频段（dB）。
//!
//! 分工：音频线程只做「下混 + 写环形缓冲」（FFT 关闭时一次原子读就返回，零成本）；
//! FFT 与分频段聚合在 [`Engine::stats`](crate::engine::Engine::stats)（推送线程/控制 API）里执行，
//! 不占音频线程。FFT 点数与频段边界是设置项（`Settings::fft_size` / `Settings::fft_bands`）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use num_complex::Complex;
use parking_lot::Mutex;
use rustfft::{Fft, FftPlanner};

use crate::model::Id;

/// 环形缓冲容量 = 最大 FFT 点数（点数设置只影响取窗口径，参数切换无需重建 tap）
pub const MAX_FFT_SIZE: usize = 4096;
/// 默认 FFT 点数（设置可调 1024/2048/4096，须为 2 的幂）
pub const DEFAULT_FFT_SIZE: usize = 4096;
/// 默认频段边界（Hz，20 段：band 0 含 50Hz 以下，高于 20kHz 的 bin 不显示）
pub const DEFAULT_BAND_EDGES: [f32; 20] = [
    50.0, 69.0, 94.0, 129.0, 176.0, 241.0, 331.0, 453.0, 620.0, 850.0, 1200.0, 1600.0, 2200.0,
    3000.0, 4100.0, 5600.0, 7700.0, 11000.0, 14000.0, 20000.0,
];
/// 频段 dB 下限（静音段钳到该值，前端画柱用）
pub const FLOOR_DB: f32 = -90.0;

/// 每个 source/sink 一个 tap：音频线程写、统计线程读。
pub struct SpectrumTap {
    /// 最近 MAX_FFT_SIZE 个 mono 采样（环形覆盖；fft_size 是统计时的取窗口径）
    buf: Mutex<TapRing>,
    /// 关闭（默认）时音频线程 push 一次原子读就返回
    enabled: AtomicBool,
}

struct TapRing {
    data: Vec<f32>,
    pos: usize,
}

impl SpectrumTap {
    pub fn new(enabled: bool) -> Self {
        Self {
            buf: Mutex::new(TapRing {
                data: vec![0.0; MAX_FFT_SIZE],
                pos: 0,
            }),
            enabled: AtomicBool::new(enabled),
        }
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// 音频线程入口：interleaved 采样下混成 mono 写入环形缓冲（只保留最近 MAX_FFT_SIZE 点）。
    pub fn push(&self, data: &[f32], channels: usize) {
        if channels == 0 || !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let mut ring = self.buf.lock();
        let ch = channels as f32;
        for frame in data.chunks_exact(channels) {
            let mono = frame.iter().sum::<f32>() / ch;
            let pos = ring.pos;
            ring.data[pos] = mono;
            ring.pos = if pos + 1 == ring.data.len() { 0 } else { pos + 1 };
        }
    }

    /// 当前窗口的频段 dB。`fft_size` 为取样点数（2 的幂 ≤ MAX_FFT_SIZE），
    /// `edges` 为频段边界 Hz（升序，段数 = 边界数）。
    pub fn bands(&self, sample_rate: u32, fft_size: usize, edges: &[f32]) -> Vec<f32> {
        let ring = self.buf.lock();
        if sample_rate == 0 || edges.is_empty() {
            return vec![FLOOR_DB; edges.len()];
        }
        // 环形展开成按时间顺序，取最近 fft_size 个点
        let n = ring.data.len();
        let take = fft_size.min(n);
        let mut window = Vec::with_capacity(take);
        for k in (n - take)..n {
            window.push(ring.data[(ring.pos + k) % n]);
        }
        spectrum_bands(&window, sample_rate, edges)
    }
}

/// 一段采样（≤ 点数，不足补零）→ 每个频段一个 dB 值（段数 = edges.len()）。
pub fn spectrum_bands(samples: &[f32], sample_rate: u32, edges: &[f32]) -> Vec<f32> {
    if sample_rate == 0 || samples.is_empty() || edges.is_empty() {
        return vec![FLOOR_DB; edges.len()];
    }
    let n = samples.len().next_power_of_two();
    // Hann 窗 + 补零，装进复数缓冲（虚部为 0）
    let mut buf: Vec<Complex<f32>> = samples
        .iter()
        .take(n)
        .enumerate()
        .map(|(i, &s)| Complex::new(s * hann_window(n, i), 0.0))
        .collect();
    buf.resize(n, Complex::new(0.0, 0.0));
    fft_plan(n).process(&mut buf);

    // 幅度谱（0..=N/2 bin）
    let mags: Vec<f32> = buf[..=n / 2]
        .iter()
        .map(|c| (c.re * c.re + c.im * c.im).sqrt())
        .collect();
    // 频段 bin 上界（band b 覆盖 (bins[b-1], bins[b]]，band 0 含 DC；
    // 高于最后边界的 bin 不属于任何频段，不显示）
    let nyquist = sample_rate as f32 / 2.0;
    let mut bins = vec![0usize; edges.len()];
    for (b, &hz) in edges.iter().enumerate() {
        let edge = ((hz / nyquist * (n / 2) as f32).round() as usize).min(n / 2);
        bins[b] = edge.max(if b > 0 { bins[b - 1] + 1 } else { 0 });
    }
    let norm = n as f32 / 4.0; // Hann 相干增益 0.5：满幅正弦峰 ≈ N/4 → 0dB
    (0..edges.len())
        .map(|b| {
            let lo = if b == 0 { 0 } else { bins[b - 1] };
            let hi = bins[b].clamp(lo + 1, mags.len());
            let mag = mags[lo..hi].iter().fold(0.0f32, |m, &v| m.max(v));
            let db = 20.0 * (mag / norm).max(1e-6).log10();
            db.max(FLOOR_DB)
        })
        .collect()
}

/// 按点数缓存的 forward FFT 规划（每种点数只规划一次，反复使用）。
fn fft_plan(size: usize) -> Arc<dyn Fft<f32>> {
    static PLANS: OnceLock<Mutex<HashMap<usize, Arc<dyn Fft<f32>>>>> = OnceLock::new();
    PLANS.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .entry(size)
        .or_insert_with(|| FftPlanner::<f32>::new().plan_fft_forward(size))
        .clone()
}

/// Hann 窗（按点数缓存，规划一次反复使用）。
fn hann_window(size: usize, i: usize) -> f32 {
    static CACHE: OnceLock<Mutex<HashMap<usize, Arc<Vec<f32>>>>> = OnceLock::new();
    let w = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .entry(size)
        .or_insert_with(|| {
            Arc::new(
                (0..size)
                    .map(|i| {
                        let t = i as f32 / size as f32;
                        0.5 - 0.5 * (2.0 * std::f32::consts::PI * t).cos()
                    })
                    .collect(),
            )
        })
        .clone();
    w[i]
}

/// 全部节点的频段快照（fft 关闭时返回空表）。
pub type Spectra = HashMap<Id, Vec<f32>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, amp: f32) -> Vec<f32> {
        (0..DEFAULT_FFT_SIZE)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    /// 1kHz 满幅正弦 → 能量集中在对应频段、电平 ≈ 0dB
    #[test]
    fn sine_lands_in_expected_band() {
        let rate = 48_000u32;
        let bands = spectrum_bands(&sine(1000.0, rate, 1.0), rate, &DEFAULT_BAND_EDGES);
        // 1kHz 落在 (850, 1200] → 频段 9
        let expect = DEFAULT_BAND_EDGES
            .iter()
            .position(|&e| 1000.0 <= e)
            .expect("1kHz 应落在某个频段");
        assert!(bands[expect] > -3.0, "主频段 {} dB，应 ≈0", bands[expect]);
        // 其余频段（至少隔一个）应明显更低
        for (b, db) in bands.iter().enumerate() {
            if (b as i32 - expect as i32).abs() > 1 {
                assert!(*db < bands[expect] - 18.0, "频段 {b} 泄漏过大: {db}");
            }
        }
    }

    /// 静音 → 全部钳到 FLOOR
    #[test]
    fn silence_is_floor() {
        let bands = spectrum_bands(&vec![0.0; DEFAULT_FFT_SIZE], 48_000, &DEFAULT_BAND_EDGES);
        assert!(bands.iter().all(|&db| db <= FLOOR_DB + 0.5));
    }

    /// 自定义频段边界：段数跟随边界数
    #[test]
    fn band_count_follows_edges() {
        let bands = spectrum_bands(&sine(1000.0, 48_000, 1.0), 48_000, &[100.0, 2000.0, 8000.0]);
        assert_eq!(bands.len(), 3);
        assert!(bands[1] > -3.0, "1kHz 应落在 (100, 2000] 段: {}", bands[1]);
        assert!(bands[2] < bands[1] - 18.0, "8k+ 段应远低于主段: {}", bands[2]);
    }

    /// 环形缓冲：推入超过容量后，窗口 = 最后 MAX_FFT_SIZE 个点
    #[test]
    fn ring_keeps_last_samples() {
        let tap = SpectrumTap::new(true);
        let data: Vec<f32> = (0..MAX_FFT_SIZE + 300).map(|i| i as f32).collect();
        tap.push(&data, 1);
        let ring = tap.buf.lock();
        // 位置回绕后：data[300..] 顺序铺满环形
        for k in 0..MAX_FFT_SIZE {
            let idx = (ring.pos + k) % MAX_FFT_SIZE;
            assert_eq!(ring.data[idx], (300 + k) as f32);
        }
    }

    /// 取窗口径：fft_size < 缓冲容量时只统计最近 fft_size 个点
    #[test]
    fn bands_use_requested_window() {
        let tap = SpectrumTap::new(true);
        let half = MAX_FFT_SIZE / 2;
        // 前半填 1.0（会被推出窗口），后半填 0.0
        tap.push(&vec![1.0; half], 1);
        tap.push(&vec![0.0; half], 1);
        let bands = tap.bands(48_000, half, &DEFAULT_BAND_EDGES);
        assert!(bands.iter().all(|&db| db <= FLOOR_DB + 0.5));
    }

    /// 双声道下混：L=1、R=0 → mono 0.5
    #[test]
    fn push_downmixes_to_mono() {
        let tap = SpectrumTap::new(true);
        tap.push(&[1.0, 0.0, 1.0, 0.0], 2);
        let ring = tap.buf.lock();
        let idx = (ring.pos + MAX_FFT_SIZE - 1) % MAX_FFT_SIZE;
        assert_eq!(ring.data[idx], 0.5);
    }

    /// 关闭时 push 是空操作
    #[test]
    fn disabled_tap_stores_nothing() {
        let tap = SpectrumTap::new(false);
        tap.push(&[1.0; 64], 1);
        let ring = tap.buf.lock();
        assert!(ring.data.iter().all(|&s| s == 0.0));
    }
}
