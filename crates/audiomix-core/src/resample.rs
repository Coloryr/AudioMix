//! 拉取式（pull）线性重采样器。
//!
//! 设计动机：混音由 sink 渲染线程按设备需要的帧数"拉动"数据，
//! 线性插值可以任意比率工作并自然适应时钟漂移（欠载时保持上一帧，
//! 后续可加自适应比率微调，见 M6）。中等质量对混音路由场景足够，
//! 高质量 sinc 重采样留作后续增强。

use std::collections::VecDeque;

/// 每条路由边一个实例，由 sink 线程独占（不做 Sync）。
pub struct PullResampler {
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
    /// 欠载计数（请求帧但无输入时 +1）
    pub underruns: u64,
}

impl PullResampler {
    pub fn new(src_rate: u32, dst_rate: u32, channels: u16) -> Self {
        Self {
            src_ch: channels.max(1) as usize,
            step: src_rate as f64 / dst_rate.max(1) as f64,
            prev: vec![0.0; channels.max(1) as usize],
            pos: 0.0,
            buf: VecDeque::new(),
            primed: false,
            underruns: 0,
        }
    }

    pub fn input_samples(&mut self, data: &[f32]) {
        self.buf.extend(data.iter().copied());
    }

    /// 生成 `frames` 帧输出（interleaved，src_ch 通道），追加到 out。
    /// 输入不足时保持上一帧（已初始化）或输出静音，并累计欠载。
    pub fn generate(&mut self, frames: usize, out: &mut Vec<f32>) {
        if self.step == 1.0 {
            self.generate_passthrough(frames, out);
            return;
        }
        let ch = self.src_ch;
        let mut produced = 0usize;
        while produced < frames {
            if !self.primed {
                // 首帧直接消费为 prev（插值左端点）
                if self.buf.len() < ch {
                    out.resize(out.len() + (frames - produced) * ch, 0.0);
                    self.underruns += (frames - produced) as u64;
                    return;
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
                self.underruns += (frames - produced) as u64;
                return;
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
    }

    /// 比率为 1 时直接拷贝，避免插值损失高频。
    fn generate_passthrough(&mut self, frames: usize, out: &mut Vec<f32>) {
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
            self.underruns += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_passthrough() {
        let mut r = PullResampler::new(48000, 48000, 2);
        r.input_samples(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]);
        let mut out = Vec::new();
        r.generate(4, &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]);
    }

    #[test]
    fn test_upsample_2x() {
        // 24000 → 48000：每个输出帧在两输入帧之间插值
        let mut r = PullResampler::new(24000, 48000, 1);
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
        let mut r = PullResampler::new(96000, 48000, 1);
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
        let mut r = PullResampler::new(48000, 48000, 1);
        r.input_samples(&[0.5]);
        let mut out = Vec::new();
        r.generate(4, &mut out);
        assert_eq!(out, vec![0.5; 4]);
        assert!(r.underruns > 0);
    }

    #[test]
    fn test_silence_before_first_input() {
        let mut r = PullResampler::new(48000, 48000, 2);
        let mut out = Vec::new();
        r.generate(2, &mut out);
        assert_eq!(out, vec![0.0; 4]);
        assert!(r.underruns > 0);
    }
}
