//! DSP 节点链运行时：滤波用 [`biquad`](https://crates.io/crates/biquad)（RBJ 公式的
//! 成熟实现，Direct Form 1，在线改参数伪影最小），加简单的延迟线与增益节点，
//! 按 [`DspKind`](crate::model::DspKind) 列表构建成级联处理链，对交错 f32 样本
//! **原地**处理。
//!
//! 设计约束：音频线程上使用 —— 构建时一次性分配全部状态，`process` 无锁无分配；
//! 参数变更由引擎比对节点签名后整链重建（接受重建瞬间滤波器状态清零）。

use biquad::{Biquad, Coefficients, DirectForm1, ToHertz, Type, Q_BUTTERWORTH_F32};

use crate::model::DspKind;

/// 便捷构造系数：钳到奈奎斯特内，Q 防负；构造失败（理论不可达）回退直通。
fn coeffs(ty: Type<f32>, f0: f32, q: f32, fs: u32) -> Coefficients<f32> {
    let f0 = f0.clamp(10.0, fs as f32 / 2.0 - 100.0);
    Coefficients::<f32>::from_params(ty, (fs as f32).hz(), f0.hz(), q.max(0.05))
        .unwrap_or(Coefficients { a1: 0.0, a2: 0.0, b0: 1.0, b1: 0.0, b2: 0.0 })
}

/// 每声道独立的 Direct Form 1 双二阶滤波器组
type BiquadBank = Vec<DirectForm1<f32>>;

fn bank(ty: Type<f32>, f0: f32, q: f32, ch: usize, fs: u32) -> BiquadBank {
    let c = coeffs(ty, f0, q, fs);
    (0..ch).map(|_| DirectForm1::new(c)).collect()
}

/// 每声道一条的环形延迟线
#[derive(Debug, Clone)]
struct DelayLine {
    buf: Vec<f32>,
    pos: usize,
}

impl DelayLine {
    fn new(samples: usize) -> Self {
        Self { buf: vec![0.0; samples.max(1)], pos: 0 }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.buf[self.pos];
        self.buf[self.pos] = x;
        self.pos = (self.pos + 1) % self.buf.len();
        y
    }
}

/// 链中的一个节点（只在 `enabled` 时构建）
enum Stage {
    Gain { linear: f32 },
    Delay { lines: Vec<DelayLine> },
    /// 级联的滤波器：外层 = 级数，内层 = 每声道一个
    Filters(Vec<BiquadBank>),
}

/// 一条路由的 DSP 处理链（构建时绑定采样率与声道数）。
pub struct DspChain {
    stages: Vec<Stage>,
    ch: usize,
}

impl DspChain {
    /// 从节点列表构建；`enabled=false` 的节点跳过，参数先钳制到合法范围。
    pub fn new(nodes: &[crate::model::DspNode], rate: u32, ch: usize) -> Self {
        let mut stages = Vec::new();
        if ch == 0 || rate == 0 {
            return Self { stages, ch };
        }
        for node in nodes {
            if !node.enabled {
                continue;
            }
            let mut kind = node.kind.clone();
            kind.clamp_params();
            match kind {
                DspKind::Gain { db } => {
                    stages.push(Stage::Gain { linear: 10f32.powf(db / 20.0) });
                }
                DspKind::Delay { ms } => {
                    let samples = ((ms as f64 / 1000.0) * rate as f64).round() as usize;
                    if samples > 0 {
                        stages.push(Stage::Delay {
                            lines: (0..ch).map(|_| DelayLine::new(samples)).collect(),
                        });
                    }
                }
                DspKind::Eq3 {
                    low_gain_db,
                    low_freq,
                    mid_gain_db,
                    mid_freq,
                    mid_q,
                    high_gain_db,
                    high_freq,
                } => stages.push(Stage::Filters(vec![
                    bank(Type::LowShelf(low_gain_db), low_freq, Q_BUTTERWORTH_F32, ch, rate),
                    bank(Type::PeakingEQ(mid_gain_db), mid_freq, mid_q, ch, rate),
                    bank(Type::HighShelf(high_gain_db), high_freq, Q_BUTTERWORTH_F32, ch, rate),
                ])),
                DspKind::PeakEq { freq, gain_db, q } => stages.push(Stage::Filters(vec![bank(
                    Type::PeakingEQ(gain_db),
                    freq,
                    q,
                    ch,
                    rate,
                )])),
                DspKind::GraphEq { gains_db } => stages.push(Stage::Filters(
                    DspKind::GRAPH_EQ_BANDS
                        .iter()
                        .zip(gains_db.iter())
                        .map(|(&f, &g)| bank(Type::PeakingEQ(g), f, 1.41, ch, rate))
                        .collect(),
                )),
                DspKind::Highpass { freq, q } => {
                    stages.push(Stage::Filters(vec![bank(Type::HighPass, freq, q, ch, rate)]))
                }
                DspKind::Lowpass { freq, q } => {
                    stages.push(Stage::Filters(vec![bank(Type::LowPass, freq, q, ch, rate)]))
                }
                DspKind::Bandpass { freq, q } => {
                    stages.push(Stage::Filters(vec![bank(Type::BandPass, freq, q, ch, rate)]))
                }
            }
        }
        Self { stages, ch }
    }

    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    /// 对交错样本原地处理（`io.len()` 应为声道数的整数倍）
    pub fn process(&mut self, io: &mut [f32]) {
        let ch = self.ch;
        if ch == 0 || io.len() % ch != 0 {
            return;
        }
        for stage in &mut self.stages {
            match stage {
                Stage::Gain { linear } => {
                    for s in io.iter_mut() {
                        *s *= *linear;
                    }
                }
                Stage::Delay { lines } => {
                    for (i, s) in io.iter_mut().enumerate() {
                        *s = lines[i % ch].process(*s);
                    }
                }
                Stage::Filters(cascade) => {
                    for per_ch in cascade.iter_mut() {
                        for (i, s) in io.iter_mut().enumerate() {
                            *s = per_ch[i % ch].run(*s);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DspKind, DspNode};

    fn node(kind: DspKind) -> DspNode {
        DspNode { kind, enabled: true }
    }

    /// 用 biquad crate 的系数算归一化频率处的复频响幅度（测试专用）
    fn magnitude_at(c: &Coefficients<f32>, f: f32, fs: u32) -> f32 {
        let w = 2.0 * std::f64::consts::PI * f as f64 / fs as f64;
        let (wr, wi) = (w.cos(), w.sin());
        let (b0, b1, b2, a1, a2) =
            (c.b0 as f64, c.b1 as f64, c.b2 as f64, c.a1 as f64, c.a2 as f64);
        let num_re = b0 + b1 * wr + b2 * (2.0 * w).cos();
        let num_im = -(b1 * wi + b2 * (2.0 * w).sin());
        let den_re = 1.0 + a1 * wr + a2 * (2.0 * w).cos();
        let den_im = -(a1 * wi + a2 * (2.0 * w).sin());
        ((num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im + 1e-30)).sqrt()
            as f32
    }

    #[test]
    fn peaking_boost_and_cut_at_center() {
        let fs = 48_000;
        let boost = coeffs(Type::PeakingEQ(12.0), 1000.0, 1.0, fs);
        let cut = coeffs(Type::PeakingEQ(-12.0), 1000.0, 1.0, fs);
        // 中心频率处幅度 ≈ 增益（+12dB ≈ 4 倍）
        let g_boost = magnitude_at(&boost, 1000.0, fs);
        let g_cut = magnitude_at(&cut, 1000.0, fs);
        assert!((g_boost - 3.98).abs() < 0.15, "peaking +12dB @1k = {g_boost}");
        assert!((g_cut - 0.251).abs() < 0.02, "peaking -12dB @1k = {g_cut}");
        // 离中心远的频段几乎不受影响
        assert!((magnitude_at(&boost, 100.0, fs) - 1.0).abs() < 0.1);
    }

    #[test]
    fn lowpass_highpass_cutoffs() {
        let fs = 48_000;
        let lp = coeffs(Type::LowPass, 500.0, Q_BUTTERWORTH_F32, fs);
        let hp = coeffs(Type::HighPass, 500.0, Q_BUTTERWORTH_F32, fs);
        // 截止频率处 ≈ -3dB（0.707），通带远端接近全通/全阻
        assert!((magnitude_at(&lp, 500.0, fs) - 0.707).abs() < 0.05);
        assert!(magnitude_at(&lp, 50.0, fs) > 0.95, "低通低频应全通");
        assert!(magnitude_at(&lp, 5000.0, fs) < 0.05, "低通高频应全阻");
        assert!((magnitude_at(&hp, 500.0, fs) - 0.707).abs() < 0.05);
        assert!(magnitude_at(&hp, 5000.0, fs) > 0.95, "高通高频应全通");
        assert!(magnitude_at(&hp, 50.0, fs) < 0.05, "高通低频应全阻");
    }

    #[test]
    fn shelves_boost_passband_only() {
        let fs = 48_000;
        let low = coeffs(Type::LowShelf(6.0), 200.0, Q_BUTTERWORTH_F32, fs);
        let high = coeffs(Type::HighShelf(6.0), 4000.0, Q_BUTTERWORTH_F32, fs);
        // +6dB ≈ 2.0 倍
        assert!(
            (magnitude_at(&low, 50.0, fs) - 2.0).abs() < 0.1,
            "low shelf @50 = {}",
            magnitude_at(&low, 50.0, fs)
        );
        assert!(
            (magnitude_at(&low, 4000.0, fs) - 1.0).abs() < 0.05,
            "low shelf 高频应不受影响 = {}",
            magnitude_at(&low, 4000.0, fs)
        );
        assert!(
            (magnitude_at(&high, 10000.0, fs) - 2.0).abs() < 0.15,
            "high shelf @10k = {}",
            magnitude_at(&high, 10000.0, fs)
        );
        assert!((magnitude_at(&high, 500.0, fs) - 1.0).abs() < 0.05);
    }

    #[test]
    fn delay_line_length_and_content() {
        // 1ms @48k = 48 样本延迟：冲激进，48 样本后出
        let mut line = DelayLine::new(48);
        let mut out = Vec::new();
        for i in 0..96 {
            out.push(line.process(if i == 0 { 1.0 } else { 0.0 }));
        }
        assert_eq!(out.iter().position(|&v| v == 1.0), Some(48));
    }

    #[test]
    fn chain_gain_and_bypass() {
        let mut chain_on = DspChain::new(&[node(DspKind::Gain { db: -20.0 })], 48_000, 2);
        assert!(!chain_on.is_empty());
        let mut io = vec![1.0f32; 8];
        chain_on.process(&mut io);
        assert!(io.iter().all(|&v| (v - 0.1).abs() < 1e-6));

        // bypass 节点不进链 → 空链直通
        let mut chain_off =
            DspChain::new(&[DspNode { kind: DspKind::Gain { db: -20.0 }, enabled: false }], 48_000, 2);
        assert!(chain_off.is_empty());
        let mut io = vec![1.0f32; 8];
        chain_off.process(&mut io);
        assert!(io.iter().all(|&v| v == 1.0));
    }

    #[test]
    fn chain_processes_channels_independently() {
        // 延迟 1ms @48k = 48 帧：冲激从左声道进，48 帧后原样出现，右声道不受影响
        let mut chain = DspChain::new(&[node(DspKind::Delay { ms: 1.0 })], 48_000, 2);
        let frames = 100;
        let mut io = vec![0.0f32; frames * 2];
        io[0] = 1.0; // 左（帧 0 的 ch0）
        io[1] = 0.5; // 右（帧 0 的 ch1）
        chain.process(&mut io);
        let left: Vec<f32> =
            io.iter().enumerate().filter(|(i, _)| i % 2 == 0).map(|(_, v)| *v).collect();
        let right: Vec<f32> =
            io.iter().enumerate().filter(|(i, _)| i % 2 == 1).map(|(_, v)| *v).collect();
        assert!(left[..48].iter().all(|&v| v == 0.0), "延迟期间左声道应为静音");
        assert_eq!(left[48], 1.0, "左通道冲激应延迟 48 帧出现");
        assert!(right[..48].iter().all(|&v| v == 0.0), "右声道同样延迟 48 帧（不应被左声道影响）");
        assert_eq!(right[48], 0.5, "右声道自己的样本延迟 48 帧回来");
    }

    #[test]
    fn graph_eq_ten_bands_roundtrip() {
        // 10 段全 0 增益 = 直通
        let mut chain = DspChain::new(&[node(DspKind::GraphEq { gains_db: [0.0; 10] })], 48_000, 1);
        let mut io: Vec<f32> = (0..100).map(|i| (i as f32 * 0.01).sin()).collect();
        let before = io.clone();
        chain.process(&mut io);
        for (a, b) in before.iter().zip(&io) {
            assert!((a - b).abs() < 1e-4, "{a} vs {b}");
        }
    }

    #[test]
    fn highpass_chain_attenuates_bass() {
        // 高通 200Hz：50Hz 正弦应被显著衰减
        let mut chain = DspChain::new(&[node(DspKind::Highpass { freq: 200.0, q: 0.707 })], 48_000, 1);
        let n = 48_00; // 100ms
        let mut io: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 50.0 * i as f32 / 48_000.0).sin())
            .collect();
        chain.process(&mut io);
        let tail_peak = io[n / 2..].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(tail_peak < 0.2, "50Hz 通过 200Hz 高通后峰值应 <0.2，实际 {tail_peak}");
    }

    #[test]
    fn params_are_clamped_at_build() {
        let _chain = DspChain::new(&[node(DspKind::Delay { ms: 99999.0 })], 48_000, 1);
        // 只要不 panic / 不爆内存即可（1000ms 上限 → 48000 样本）
        let mut chain = DspChain::new(&[node(DspKind::Gain { db: -999.0 })], 48_000, 1);
        let mut io = vec![1.0f32; 4];
        chain.process(&mut io);
        assert!(io.iter().all(|v| *v <= 0.001), "超低增益应被钳到 -60dB 附近");
    }
}
