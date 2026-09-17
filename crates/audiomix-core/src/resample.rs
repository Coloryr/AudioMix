//! 拉取式（pull）重采样器，三档质量：
//!
//! - [`ResamplerQuality::Sinc256`]（默认）：256 点加窗 sinc（rubato 库 `Async`，
//!   Blackman-Harris2 窗 + 三次相位插值），通带平坦、抗混叠好，THD+N 在 f32 极限；
//! - [`ResamplerQuality::Sinc128`]：128 点轻量 sinc，质量仍远好于线性，延迟约减半；
//! - [`ResamplerQuality::Linear`]：线性插值，零延迟、CPU 最省（高频失真较大）。
//!
//! 拉取语义各档一致：sink 渲染线程按需要的帧数取数据；输入不足时保持上一帧
//! （从未有输入时输出静音）并累计 `underruns`；比率为 1 时直接拷贝，
//! 避免插值损失高频。

use std::collections::VecDeque;

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use serde::{Deserialize, Serialize};

/// 重采样质量档位（设置里可切换；切换后引擎重建各边重采样器，立即生效）。
/// 延迟参考（44.1k↔48k）：Sinc256 ≈ 6.7ms，Sinc128 ≈ 3.5ms，Linear ≈ 0。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResamplerQuality {
    /// 256 点加窗 sinc（rubato），高质量，默认
    #[default]
    #[serde(alias = "sinc")] // 旧配置的 "sinc" 映射过来
    Sinc256,
    /// 128 点加窗 sinc，质量仍远好于线性，延迟约减半
    Sinc128,
    /// 线性插值，零延迟低 CPU（高频失真较大）
    Linear,
}

impl ResamplerQuality {
    /// sinc 滤波器长度（点数）；linear 无滤波器
    fn sinc_len(self) -> usize {
        match self {
            ResamplerQuality::Sinc256 => 256,
            ResamplerQuality::Sinc128 => 128,
            ResamplerQuality::Linear => 0,
        }
    }
}

/// 每条路由边一个实例，由 sink 线程独占（不做 Sync）。
pub struct PullResampler {
    inner: Inner,
    /// 欠载计数（请求帧但无输入时按帧累计；引擎定期读取并清零）
    pub underruns: u64,
}

enum Inner {
    /// 比率为 1：直接拷贝
    Passthrough(PassthroughCore),
    Linear(LinearCore),
    Sinc(SincCore),
}

impl PullResampler {
    pub fn new(src_rate: u32, dst_rate: u32, channels: u16, quality: ResamplerQuality) -> Self {
        let ch = channels.max(1) as usize;
        let inner = if src_rate == dst_rate.max(1) {
            Inner::Passthrough(PassthroughCore::new(ch))
        } else {
            match quality {
                ResamplerQuality::Sinc256 | ResamplerQuality::Sinc128 => {
                    match SincCore::new(src_rate, dst_rate, ch, quality) {
                        Some(c) => Inner::Sinc(c),
                        // rubato 初始化失败（极端参数）时退回线性，保证出声
                        None => Inner::Linear(LinearCore::new(src_rate, dst_rate, ch)),
                    }
                }
                ResamplerQuality::Linear => Inner::Linear(LinearCore::new(src_rate, dst_rate, ch)),
            }
        };
        Self {
            inner,
            underruns: 0,
        }
    }

    pub fn input_samples(&mut self, data: &[f32]) {
        match &mut self.inner {
            Inner::Passthrough(c) => c.buf.extend(data.iter().copied()),
            Inner::Linear(c) => c.buf.extend(data.iter().copied()),
            Inner::Sinc(c) => c.in_q.extend(data.iter().copied()),
        }
    }

    /// 生成 `frames` 帧输出（interleaved，src_ch 通道），追加到 out。
    /// 输入不足时保持上一帧（已初始化）或输出静音，并累计欠载。
    /// 返回本次欠载（保持/静音）的帧数。
    pub fn generate(&mut self, frames: usize, out: &mut Vec<f32>) -> usize {
        let held = match &mut self.inner {
            Inner::Passthrough(c) => c.generate(frames, out),
            Inner::Linear(c) => c.generate(frames, out),
            Inner::Sinc(c) => c.generate(frames, out),
        };
        self.underruns += held as u64;
        held
    }

    /// 内部待处理的输入样本数（观测/测试用）
    pub fn buf_len(&self) -> usize {
        match &self.inner {
            Inner::Passthrough(c) => c.buf.len(),
            Inner::Linear(c) => c.buf.len(),
            Inner::Sinc(c) => c.in_q.len(),
        }
    }

    /// 内部已重采样、待拉取的输出样本数（观测/测试用；仅 sinc 档有缓冲）
    pub fn out_len(&self) -> usize {
        match &self.inner {
            Inner::Sinc(c) => c.out_q.len(),
            _ => 0,
        }
    }
}

// ---------- 比率为 1：直接拷贝 ----------

struct PassthroughCore {
    src_ch: usize,
    buf: VecDeque<f32>,
    prev: Vec<f32>,
    primed: bool,
}

impl PassthroughCore {
    fn new(ch: usize) -> Self {
        Self {
            src_ch: ch,
            buf: VecDeque::new(),
            prev: vec![0.0; ch],
            primed: false,
        }
    }

    fn generate(&mut self, frames: usize, out: &mut Vec<f32>) -> usize {
        let ch = self.src_ch;
        let want = frames * ch;
        let mut taken = self.buf.len().min(want);
        // 不取走不完整的帧
        taken -= taken % ch;
        out.extend(self.buf.drain(..taken));
        let produced = taken / ch;
        if taken > 0 {
            // prev ← 最后一个取出的帧
            for c in 0..ch {
                self.prev[c] = out[out.len() - ch + c];
            }
            self.primed = true;
        }
        if produced < frames {
            if self.primed {
                // 保持上一帧
                for _ in produced..frames {
                    out.extend_from_slice(&self.prev);
                }
            } else {
                // 从未有输入：静音
                out.resize(out.len() + (frames - produced) * ch, 0.0);
            }
        }
        frames - produced
    }
}

// ---------- 线性插值 ----------

struct LinearCore {
    src_ch: usize,
    /// 每个 sink 输出帧对应的输入帧数（src_rate / dst_rate）
    step: f64,
    /// 上一已消费输入帧（插值左端点）
    prev: Vec<f32>,
    /// 插值位置 0..1（prev 与队首帧之间）
    pos: f64,
    /// 待处理的 interleaved 输入样本
    buf: VecDeque<f32>,
    /// 是否已从 buf 初始化过 prev
    primed: bool,
}

impl LinearCore {
    fn new(src_rate: u32, dst_rate: u32, ch: usize) -> Self {
        Self {
            src_ch: ch,
            step: src_rate as f64 / dst_rate.max(1) as f64,
            prev: vec![0.0; ch],
            pos: 0.0,
            buf: VecDeque::new(),
            primed: false,
        }
    }

    fn generate(&mut self, frames: usize, out: &mut Vec<f32>) -> usize {
        let ch = self.src_ch;
        let mut produced = 0usize;
        while produced < frames {
            if !self.primed {
                // 首帧直接消费为 prev（插值左端点）
                if self.buf.len() < ch {
                    out.resize(out.len() + (frames - produced) * ch, 0.0);
                    return frames - produced;
                }
                for c in 0..ch {
                    self.prev[c] = self.buf.pop_front().unwrap();
                }
                self.primed = true;
            }
            if self.buf.is_empty() {
                // 欠载：保持上一帧
                for _ in 0..(frames - produced) {
                    out.extend_from_slice(&self.prev);
                }
                return frames - produced;
            }
            // prev 与 buf 队首帧之间插值
            let start = out.len();
            out.resize(start + ch, 0.0);
            for c in 0..ch {
                let a = self.prev[c];
                let b = self.buf[c];
                out[start + c] = a + (b - a) * self.pos as f32;
            }
            produced += 1;
            self.pos += self.step;
            while self.pos >= 1.0 {
                self.pos -= 1.0;
                for c in 0..ch {
                    self.prev[c] = self.buf.pop_front().unwrap_or(self.prev[c]);
                }
                if self.buf.is_empty() {
                    break;
                }
            }
        }
        0
    }
}

// ---------- 加窗 sinc（rubato） ----------

struct SincCore {
    src_ch: usize,
    /// rubato 固定输入块的异步 sinc 重采样器（比率 = dst/src）
    res: Async<f32>,
    /// 待处理的 interleaved 输入样本
    in_q: VecDeque<f32>,
    /// 已重采样、待拉取的 interleaved 输出样本
    out_q: VecDeque<f32>,
    /// rubato 输入适配器底层数组（按通道分离）
    in_planar: Vec<Vec<f32>>,
    /// rubato 输出适配器底层数组（按通道分离，容量 = output_frames_max）
    out_planar: Vec<Vec<f32>>,
    out_max: usize,
    /// 启动阶段待裁剪的输出帧数（sinc 群延迟，输出帧计）
    delay_left: usize,
    /// 最后一个输出帧（欠载时保持）
    prev: Vec<f32>,
    primed: bool,
}

impl SincCore {
    fn new(src_rate: u32, dst_rate: u32, ch: usize, quality: ResamplerQuality) -> Option<Self> {
        let ratio = dst_rate as f64 / src_rate.max(1) as f64;
        let sinc_len = quality.sinc_len();
        let params = SincInterpolationParameters {
            sinc_len,
            f_cutoff: None,
            interpolation: SincInterpolationType::Cubic,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };
        // 输入块 = sinc 长度：块越大单位开销越低，块越小管道延迟越小
        let res =
            Async::<f32>::new_sinc(ratio, 1.1, &params, sinc_len, ch, FixedAsync::Input).ok()?;
        let in_need = res.input_frames_next();
        let out_max = res.output_frames_max();
        let delay_left = res.output_delay();
        Some(Self {
            src_ch: ch,
            res,
            in_q: VecDeque::new(),
            out_q: VecDeque::new(),
            in_planar: vec![Vec::with_capacity(in_need); ch],
            out_planar: vec![vec![0.0; out_max]; ch],
            out_max,
            delay_left,
            prev: vec![0.0; ch],
            primed: false,
        })
    }

    fn generate(&mut self, frames: usize, out: &mut Vec<f32>) -> usize {
        let ch = self.src_ch;
        let want = frames * ch;
        // 反复喂块，直到输出够或输入不足
        while self.out_q.len() < want {
            let need = self.res.input_frames_next();
            if self.in_q.len() < need * ch {
                break;
            }
            for c in 0..ch {
                self.in_planar[c].clear();
            }
            for _ in 0..need {
                for c in 0..ch {
                    self.in_planar[c].push(self.in_q.pop_front().unwrap());
                }
            }
            let Ok(in_ad) = SequentialSliceOfVecs::new(&self.in_planar, ch, need) else {
                break;
            };
            let Ok(mut out_ad) =
                SequentialSliceOfVecs::new_mut(&mut self.out_planar, ch, self.out_max)
            else {
                break;
            };
            let Ok((_, nout)) = self.res.process_into_buffer(&in_ad, &mut out_ad, None) else {
                break;
            };
            // 裁掉启动群延迟（输出帧计），之后输出与输入时间对齐
            let skip = self.delay_left.min(nout);
            self.delay_left -= skip;
            for f in skip..nout {
                for c in 0..ch {
                    self.out_q.push_back(self.out_planar[c][f]);
                }
            }
        }
        // 从输出队列取帧
        let mut produced = 0usize;
        while produced < frames && self.out_q.len() >= ch {
            for c in 0..ch {
                let v = self.out_q.pop_front().unwrap();
                self.prev[c] = v;
                out.push(v);
            }
            produced += 1;
        }
        if produced > 0 {
            self.primed = true;
        }
        if produced < frames {
            if self.primed {
                for _ in produced..frames {
                    out.extend_from_slice(&self.prev);
                }
            } else {
                out.resize(out.len() + (frames - produced) * ch, 0.0);
            }
        }
        frames - produced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_passthrough() {
        let mut r = PullResampler::new(48000, 48000, 2, ResamplerQuality::Sinc256);
        r.input_samples(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]);
        let mut out = Vec::new();
        r.generate(4, &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]);
    }

    #[test]
    fn test_upsample_2x() {
        // 24000 → 48000：每个输出帧在两输入帧之间插值
        let mut r = PullResampler::new(24000, 48000, 1, ResamplerQuality::Linear);
        r.input_samples(&[0.0, 1.0]);
        let mut out = Vec::new();
        r.generate(3, &mut out);
        // step = 0.5：帧0=pos0→0.0, 帧1=pos0.5→0.5, 帧2=pos0(消费1.0后)→1.0
        assert_eq!(out.len(), 3);
        assert!(out[0].abs() < 1e-6);
        assert!((out[1] - 0.5).abs() < 1e-6);
        assert!((out[2] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_downsample() {
        // 96000 → 48000：隔帧取值
        let mut r = PullResampler::new(96000, 48000, 1, ResamplerQuality::Linear);
        r.input_samples(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        let mut out = Vec::new();
        r.generate(4, &mut out);
        assert_eq!(out.len(), 4);
        // step = 2.0：帧0 = 0.0, 帧1 = 2.0, ...
        assert!((out[0] - 0.0).abs() < 1e-6);
        assert!((out[1] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn test_underrun_hold() {
        let mut r = PullResampler::new(48000, 48000, 1, ResamplerQuality::Linear);
        r.input_samples(&[0.5]);
        let mut out = Vec::new();
        r.generate(4, &mut out);
        assert_eq!(out, vec![0.5; 4]);
        assert!(r.underruns > 0);
    }

    #[test]
    fn test_silence_before_first_input() {
        let mut r = PullResampler::new(48000, 48000, 2, ResamplerQuality::Sinc256);
        let mut out = Vec::new();
        r.generate(2, &mut out);
        assert_eq!(out, vec![0.0; 4]);
        assert!(r.underruns > 0);
    }

    /// THD+N：15kHz 正弦 44.1k→48k。8192 点 @48k 的 bin 间距为 48000/8192，
    /// 15000Hz 恰好是第 2560 个 bin（整 bin，无泄漏），可直接用 DFT 测主音能量。
    fn thdn_db(quality: ResamplerQuality) -> f64 {
        const F: f64 = 15000.0;
        let mut r = PullResampler::new(44100, 48000, 1, quality);
        let n_in = (44100.0 * 0.6) as usize;
        let mut pos = 0usize;
        let mut all: Vec<f32> = Vec::new();
        // 模拟 pull 节奏：每 480 输入帧喂一次，每次拉 480 输出帧
        while all.len() < 24000 {
            let end = (pos + 480).min(n_in);
            let batch: Vec<f32> = (pos..end)
                .map(|m| (2.0 * std::f64::consts::PI * F * m as f64 / 44100.0).sin() as f32)
                .collect();
            r.input_samples(&batch);
            pos = end;
            let mut out = Vec::new();
            r.generate(480, &mut out);
            all.extend_from_slice(&out);
        }
        // 分析中段 8192 帧（远离启动延迟与结尾欠载区）
        let n = 8192usize;
        let seg = &all[all.len() - 2 * n..all.len() - n];
        let total: f64 = seg
            .iter()
            .map(|v| {
                let v = *v as f64;
                v * v
            })
            .sum::<f64>()
            / n as f64;
        let k = 2560usize;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, v) in seg.iter().enumerate() {
            let ang = -2.0 * std::f64::consts::PI * (k * i % n) as f64 / n as f64;
            re += *v as f64 * ang.cos();
            im += *v as f64 * ang.sin();
        }
        let tone = 2.0 * (re * re + im * im) / (n * n) as f64;
        10.0 * ((total - tone) / tone).log10()
    }

    #[test]
    fn test_sinc_quality_beats_linear() {
        let sinc = thdn_db(ResamplerQuality::Sinc256);
        let sinc128 = thdn_db(ResamplerQuality::Sinc128);
        let linear = thdn_db(ResamplerQuality::Linear);
        // 15kHz 每周期只有约 2.9 个输入样本，线性插值失真严重；sinc 档应远好于线性
        assert!(sinc < -40.0, "sinc256 THD+N = {sinc:.1} dB，应 < -40 dB");
        assert!(
            sinc128 < -40.0,
            "sinc128 THD+N = {sinc128:.1} dB，应 < -40 dB"
        );
        assert!(
            linear > -25.0,
            "linear THD+N = {linear:.1} dB，应 > -25 dB（对照组）"
        );
    }

    #[test]
    fn test_sinc_dc_gain() {
        // 直流增益应为 1（sinc 核归一化）：喂恒定 0.5，输出恢复 0.5
        let mut r = PullResampler::new(44100, 48000, 1, ResamplerQuality::Sinc256);
        r.input_samples(&vec![0.5f32; 9600]);
        let mut out = Vec::new();
        r.generate(4096, &mut out);
        assert_eq!(out.len(), 4096);
        let tail = &out[2048..];
        let mean: f64 = tail.iter().map(|v| *v as f64).sum::<f64>() / tail.len() as f64;
        assert!((mean - 0.5).abs() < 1e-3, "直流增益偏差过大: mean={mean}");
    }

    #[test]
    fn test_sinc_underrun_hold() {
        let mut r = PullResampler::new(44100, 48000, 1, ResamplerQuality::Sinc256);
        r.input_samples(&vec![0.25f32; 4096]);
        // 持续拉取直到输入与输出队列全部耗尽
        let mut last = 0.0f32;
        for _ in 0..40 {
            let mut out = Vec::new();
            r.generate(256, &mut out);
            if let Some(v) = out.last() {
                last = *v;
            }
        }
        assert!(r.underruns > 0);
        // 之后无输入：保持上一帧
        let mut out = Vec::new();
        r.generate(8, &mut out);
        assert_eq!(out, vec![last; 8]);
    }
}
