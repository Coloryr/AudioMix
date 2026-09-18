//! 路径延迟测量：向源节点的采集流注入扫频脉冲，在目标路径的信号里做匹配相关检测。
//!
//! 分工（与 fft.rs 相同的思路——音频线程只做最便宜的事）：
//! - 采集回调：armed 时把测试脉冲叠加到本回调的数据里（只在测量期间复制一份，
//!   电平表/频谱仍用原始数据），并记录注入时刻；
//! - 渲染回调：armed 时把目标边（DSP 处理后、混合前）的信号送进 tap 缓冲；
//! - 相关检测与配对在测量线程里跑（`measure` 内的 std::thread），不占音频线程。
//!
//! 测得的延迟 = 采集侧注入时刻 → 渲染侧该样本交给设备前的时刻，
//! 覆盖：边缓冲排队 + 重采样 + DSP 链（延迟节点的 ms 会如实计入）；
//! 不含两端设备自身的缓冲周期。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// 脉冲次数（每 400ms 一次，取中位数抗偶发误检）
pub const BURSTS: usize = 5;
/// 脉冲间隔（源采样时钟）
pub const BURST_INTERVAL_SECS: f64 = 0.4;
/// 首个脉冲的起始延时（留出布防时间）
pub const FIRST_BURST_DELAY_SECS: f64 = 0.25;
/// 脉冲时长（扫频，Hann 包络）
pub const BURST_LEN_SECS: f64 = 0.006;
/// 扫频范围（Hz）：音乐里罕见 0.8→3.2kHz 的 6ms 快扫，相关峰不易误触发
const SWEEP_LO_HZ: f32 = 800.0;
const SWEEP_HI_HZ: f32 = 3200.0;
/// 脉冲幅度（叠加到原始信号上；相关检测对增益不敏感，这里只求突出）
const BURST_AMP: f32 = 0.9;
/// 归一化相关的检出阈值（音乐偶发峰值通常低于此；配合间隔一致性过滤兜底）
const DETECT_THRESHOLD: f32 = 0.6;
/// tap 缓冲容量（秒）：必须 ≥ 总测量时长，同时吸收边缓冲 250ms 上限
const TAP_CAP_SECS: f64 = 2.6;
/// 单次测量总超时
const MEASURE_TIMEOUT: Duration = Duration::from_secs(4);
/// 检测线程的轮询间隔
const DETECT_POLL: Duration = Duration::from_millis(20);
/// 检出后的不应期（秒）：一次脉冲只取首个到达——跨线缆回环的再入会晚到，
/// 跳过它们避免同一脉冲被检出多次（配对错位）。
const DETECT_REFRACTORY_SECS: f64 = 0.2;

/// 共享探测状态：引擎持有，采集/渲染回调与测量线程各拿一份 Arc。
pub struct LatencyProbe {
    armed: AtomicBool,
    /// 采集侧配置（只对目标 source 生效）
    src: Mutex<Option<SrcCfg>>,
    /// 渲染侧 tap（只对目标边生效）
    dst: Mutex<Option<DstCfg>>,
    /// 最终结果（测量线程写入，命令层读取）
    result: Mutex<Option<Result<f64, String>>>,
}

impl Default for LatencyProbe {
    fn default() -> Self {
        Self {
            armed: AtomicBool::new(false),
            src: Mutex::new(None),
            dst: Mutex::new(None),
            result: Mutex::new(None),
        }
    }
}

/// 采集侧配置：脉冲日程 + 注入时刻锚点。
struct SrcCfg {
    src_id: String,
    ch: usize,
    template: Vec<f32>,
    /// 每个脉冲的起始帧（自布防起，源采样时钟）
    burst_starts: Vec<u64>,
    /// 已处理的帧计数（自布防起）
    counter: u64,
    /// (锚点时刻, 锚点时的 counter)：把帧号换算成绝对时刻用
    anchor: Option<(Instant, u64)>,
    /// 每个脉冲的注入时刻（与 burst_starts 一一对应，检测配对用）
    inject_at: Vec<Instant>,
}

/// 渲染侧 tap：目标边（DSP 后）的 mono 信号 + 时刻锚点。
struct DstCfg {
    edge_key: String,
    rate: u32,
    /// **终点采样率**的检测模板：注入在源侧（src_rate），检测在渲染侧（dst_rate），
    /// 两侧采样率不同时必须各按各的率生成（扫频的物理频率/时长经重采样保持不变）
    template: Vec<f32>,
    /// mono 信号缓冲（drop-oldest），buf[0] 的全局帧号 = frame_base
    buf: Vec<f32>,
    frame_base: u64,
    /// (锚点时刻, 锚点时的全局帧号)
    anchor: Option<(Instant, u64)>,
    /// 检测线程已扫描到的全局帧号
    consumed: u64,
    /// 已检出的 (时刻, 相关峰) 列表
    detections: Vec<(Instant, f32)>,
}

impl LatencyProbe {
    /// 音频线程入口（采集侧）：armed 时对目标 source 计数并把脉冲叠加进数据。
    /// 返回 Some(注入后的数据副本)（需要推给边缓冲），None 表示照常用原始数据。
    /// 电平表/频谱仍应使用**原始**数据（调用方先算 peak/fft 再调这里）。
    pub fn capture_inject<'a>(&self, src_id: &str, data: &'a [f32]) -> Option<Vec<f32>> {
        if !self.armed.load(Ordering::Relaxed) {
            return None;
        }
        let mut cfg = self.src.lock();
        let Some(cfg) = cfg.as_mut() else {
            return None;
        };
        if cfg.src_id != src_id {
            return None;
        }
        let ch = cfg.ch.max(1);
        let frames = data.len() / ch.max(1);
        let start = cfg.counter;
        cfg.counter = start + frames as u64;
        // 锚点：每个回调都记录一次（只在测量期间，几秒钟）
        cfg.anchor = Some((Instant::now(), start));

        // 本回调与哪个脉冲的帧区间重叠（脉冲可能跨回调）
        let mut burst_idx = None;
        for (i, &b) in cfg.burst_starts.iter().enumerate() {
            let len = cfg.template.len() as u64;
            if b < start + frames as u64 && start < b + len {
                burst_idx = Some(i);
                break;
            }
        }
        let Some(i) = burst_idx else {
            return None;
        };
        // 注入时刻：直接记当前时刻（误差 ≤ 一个回调周期 ~10ms，中位数能抹平）
        if cfg.inject_at.len() == i {
            cfg.inject_at.push(Instant::now());
        }
        // 叠加脉冲（每个声道同相加，任何声道变换后都保留）
        let b = cfg.burst_starts[i];
        let (f0, t0) = if b >= start {
            ((b - start) as usize, 0usize)
        } else {
            (0usize, (start - b) as usize)
        };
        let mut out = data.to_vec();
        let mut t = t0;
        for f in f0..frames {
            if t >= cfg.template.len() {
                break;
            }
            let s = cfg.template[t] * BURST_AMP;
            for c in 0..ch {
                out[f * ch + c] += s;
            }
            t += 1;
        }
        Some(out)
    }

    /// 音频线程入口（渲染侧）：armed 时把目标边的信号（interleaved）送进 tap。
    /// 在 DSP 处理后、增益/混合前调用。
    pub fn render_tap(&self, edge_key: &str, data: &[f32], channels: usize) {
        if !self.armed.load(Ordering::Relaxed) {
            return;
        }
        let mut cfg = self.dst.lock();
        let Some(cfg) = cfg.as_mut() else {
            return;
        };
        if cfg.edge_key != edge_key || channels == 0 {
            return;
        }
        let frames = data.len() / channels;
        if frames == 0 {
            return;
        }
        cfg.anchor = Some((Instant::now(), cfg.frame_base + cfg.buf.len() as u64));
        let ch = channels as f32;
        for frame in data[..frames * channels].chunks_exact(channels) {
            let mono = frame.iter().sum::<f32>() / ch;
            cfg.buf.push(mono);
        }
        // drop-oldest：超容量丢弃最旧（frame_base 随之前移）
        let cap = (cfg.rate as f64 * TAP_CAP_SECS) as usize;
        if cfg.buf.len() > cap {
            let drop = cfg.buf.len() - cap;
            cfg.buf.drain(..drop);
            cfg.frame_base += drop as u64;
        }
    }

    /// 布防并阻塞测量：对 `route_id` 的路径（source → sink）注入脉冲并检测，
    /// 返回中位数延迟（ms）。失败返回可读错误。整段最多 `MEASURE_TIMEOUT`。
    pub fn measure(
        self: &Arc<Self>,
        src_id: &str,
        src_rate: u32,
        src_ch: usize,
        edge_key: &str,
        dst_rate: u32,
    ) -> Result<f64, String> {
        // 采样率与格式的合法性
        if src_rate == 0 || dst_rate == 0 || src_ch == 0 {
            return Err("流格式尚未就绪，请稍后再测".into());
        }
        let template = sweep_template(src_rate);
        let dst_template = sweep_template(dst_rate);
        let burst_starts: Vec<u64> = (0..BURSTS)
            .map(|i| {
                ((FIRST_BURST_DELAY_SECS + BURST_INTERVAL_SECS * i as f64) * src_rate as f64) as u64
            })
            .collect();
        *self.src.lock() = Some(SrcCfg {
            src_id: src_id.to_string(),
            ch: src_ch,
            template,
            burst_starts,
            counter: 0,
            anchor: None,
            inject_at: Vec::new(),
        });
        *self.dst.lock() = Some(DstCfg {
            edge_key: edge_key.to_string(),
            rate: dst_rate,
            template: dst_template,
            buf: Vec::with_capacity(4096),
            frame_base: 0,
            anchor: None,
            consumed: 0,
            detections: Vec::new(),
        });
        *self.result.lock() = None;
        self.armed.store(true, Ordering::Release);

        // 检测/收尾线程：轮询 tap 做相关，凑够脉冲或超时后出结果
        let probe = Arc::clone(self);
        let detector = std::thread::Builder::new()
            .name("latency-probe".into())
            .spawn(move || probe.detect_loop())
            .map_err(|e| format!("探测线程启动失败: {e}"));
        // 布防后等待结果（命令层阻塞在此，UI 显示 loading）
        let started = Instant::now();
        let out;
        loop {
            std::thread::sleep(Duration::from_millis(50));
            if let Some(r) = self.result.lock().as_ref() {
                out = r.clone();
                break;
            }
            if started.elapsed() > MEASURE_TIMEOUT + Duration::from_secs(1) {
                out = Err("测量超时".into());
                break;
            }
        }
        let _ = detector;
        self.disarm();
        // 保留 1 位小数（ms）
        out.map(|ms| (ms * 10.0).round() / 10.0)
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::Release);
        *self.src.lock() = None;
        *self.dst.lock() = None;
    }

    /// 检测线程主体：扫描 tap 新数据做归一化相关，凑够脉冲数后配对出中位数。
    fn detect_loop(self: Arc<Self>) {
        let started = Instant::now();
        loop {
            std::thread::sleep(DETECT_POLL);
            // 收尾：结果已出（不该发生）或超时
            if self.result.lock().is_some() || started.elapsed() > MEASURE_TIMEOUT {
                break;
            }
            let (inject_at, finished) = {
                let src_cfg = self.src.lock();
                let mut dst = self.dst.lock();
                let Some(dst) = dst.as_mut() else { return };
                let Some(src_cfg) = src_cfg.as_ref() else { return };
                // 扫描新增样本（用 dst 侧模板，见 DstCfg::template 注释）
                self.scan(dst);
                (
                    src_cfg.inject_at.clone(),
                    dst.detections.len() >= BURSTS,
                )
            };
            if finished {
                let dets = self
                    .dst
                    .lock()
                    .as_ref()
                    .map(|d| d.detections.clone())
                    .unwrap_or_default();
                let r = finalize(&inject_at, &dets);
                *self.result.lock() = Some(r);
                return;
            }
        }
        // 超时收尾：用手头已有的检出配对
        let r = {
            let src_cfg = self.src.lock();
            let dst = self.dst.lock();
            let inject_at = src_cfg
                .as_ref()
                .map(|s| s.inject_at.clone())
                .unwrap_or_default();
            let detections = dst.as_ref().map(|d| d.detections.clone()).unwrap_or_default();
            finalize(&inject_at, &detections)
        };
        *self.result.lock() = Some(r);
    }

    /// 扫描 tap 里 [consumed, end) 的新样本做归一化相关，命中记入 detections。
    fn scan(&self, dst: &mut DstCfg) {
        let template = dst.template.clone();
        let t = template.len();
        if t == 0 {
            return;
        }
        let end = dst.frame_base + dst.buf.len() as u64;
        if end < dst.consumed + t as u64 {
            return;
        }
        let t_energy: f32 = template.iter().map(|x| x * x).sum();
        if t_energy <= 0.0 {
            return;
        }
        let mut pos = dst.consumed;
        while pos + t as u64 <= end {
            let off = (pos - dst.frame_base) as usize;
            let win = &dst.buf[off..off + t];
            let w_energy: f32 = win.iter().map(|x| x * x).sum();
            if w_energy > 1e-9 {
                let dot: f32 = win.iter().zip(template.iter()).map(|(a, b)| a * b).sum();
                let r = dot / (w_energy * t_energy).sqrt();
                if r > DETECT_THRESHOLD {
                    // 换算绝对时刻：锚点 + 帧差（帧号为全局，锚点记录其起始帧）
                    let instant = dst
                        .anchor
                        .map(|(at, af)| {
                            let delta_frames = pos as i64 - af as i64;
                            let secs = delta_frames as f64 / dst.rate as f64;
                            at + Duration::from_secs_f64(secs.max(-30.0))
                        })
                        .unwrap_or_else(Instant::now);
                    dst.detections.push((instant, r));
                    // 不应期：跳过本脉冲及其回环再入（脉冲间隔 400ms > 不应期 200ms，
                    // 不会吞掉下一个脉冲）
                    pos += (dst.rate as f64 * DETECT_REFRACTORY_SECS) as u64;
                    continue;
                }
            }
            pos += 1;
        }
        dst.consumed = pos;
    }
}

/// 6ms 0.8→3.2kHz 扫频 + Hann 包络（mono 模板，按源采样率生成）。
fn sweep_template(rate: u32) -> Vec<f32> {
    let n = (rate as f64 * BURST_LEN_SECS) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / n as f32;
            // 线性扫频相位
            let f = SWEEP_LO_HZ + (SWEEP_HI_HZ - SWEEP_LO_HZ) * t;
            let phase = 2.0 * std::f32::consts::PI
                * (SWEEP_LO_HZ * t + (SWEEP_HI_HZ - SWEEP_LO_HZ) * t * t / 2.0)
                * n as f32
                / rate as f32;
            let _ = f;
            phase.sin() * hann(n, i)
        })
        .collect()
}

fn hann(n: usize, i: usize) -> f32 {
    0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos()
}

/// 检出与注入配对 → 过滤离群 → 中位数（ms）。
/// 配对按顺序：第 k 个检出对应第 k 个脉冲；漏检会让后续错位（差出 400ms 的整倍数），
/// 因此先取中位数，再剔除偏离中位数 > 5ms 的值，再取一次中位数。
fn finalize(inject_at: &[Instant], detections: &[(Instant, f32)]) -> Result<f64, String> {
    if inject_at.is_empty() {
        return Err("未能注入测试信号（源流未运行？）".into());
    }
    if detections.is_empty() {
        return Err("未检测到测试信号（检查路由是否静音、输出设备是否有声音）".into());
    }
    let lats: Vec<f64> = detections
        .iter()
        .zip(inject_at.iter())
        .map(|((det, _), inj)| det.duration_since(*inj).as_secs_f64() * 1000.0)
        .collect();
    let median = |mut v: Vec<f64>| -> Option<f64> {
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Some(v[v.len() / 2])
    };
    let Some(m) = median(lats.clone()) else {
        return Err("无有效测量值".into());
    };
    let kept: Vec<f64> = lats.iter().filter(|&&l| (l - m).abs() < 5.0).copied().collect();
    let final_m = median(kept).unwrap_or(m);
    if !(0.0..=2000.0).contains(&final_m) {
        return Err(format!("测得异常延迟 {final_m:.1}ms，请重试"));
    }
    Ok(final_m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_has_unit_peak_correlation() {
        let rate = 48_000u32;
        let t = sweep_template(rate);
        assert_eq!(t.len(), (rate as f64 * BURST_LEN_SECS) as usize);
        // 模板与自身归一化相关 = 1
        let e: f32 = t.iter().map(|x| x * x).sum();
        let dot: f32 = t.iter().zip(&t).map(|(a, b)| a * b).sum();
        assert!((dot / e - 1.0).abs() < 1e-4);
    }

    #[test]
    fn detect_finds_burst_in_noise() {
        // 构造 LatencyProbe 并手工布防 dst，用随机信号 + 延迟的模板验证检出与时刻换算
        let rate = 48_000u32;
        let template = sweep_template(rate);
        let mut signal: Vec<f32> = (0..rate as usize)
            .map(|i| ((i * 2654435761) % 97) as f32 / 97.0 * 0.1 - 0.05)
            .collect();
        let delay = 2400usize; // 50ms
        for (i, &s) in template.iter().enumerate() {
            signal[delay + i] += s * 0.5;
        }
        let probe = Arc::new(LatencyProbe::default());
        *probe.dst.lock() = Some(DstCfg {
            edge_key: "e".into(),
            rate,
            template: template.clone(),
            buf: signal,
            frame_base: 0,
            anchor: Some((Instant::now(), 0)),
            consumed: 0,
            detections: Vec::new(),
        });
        let mut dst = probe.dst.lock();
        let cfg = dst.as_mut().unwrap();
        probe.scan(cfg);
        let d = &cfg.detections;
        assert_eq!(d.len(), 1, "应恰好检出一次: {d:?}");
        // 检出后扫描应至少越过整个脉冲（consumed 推进到缓冲尾也可）
        assert!(cfg.consumed >= (delay + template.len()) as u64);
    }

    /// 源侧 96k 注入、终点侧 48k 检测：采样率不同必须各按各的率生成模板
    /// （半取样降采对 <24kHz 的扫频是干净的，相关峰应保持）
    #[test]
    fn detect_survives_resample() {
        let src_rate = 96_000u32;
        let dst_rate = 48_000u32;
        let src_t = sweep_template(src_rate);
        let mut signal: Vec<f32> = (0..dst_rate as usize * 2)
            .map(|i| ((i * 2654435761 % 97) as f32 / 97.0) * 0.1 - 0.05)
            .collect();
        let delay = 2400usize; // 50ms @48k
        for (i, &s) in src_t.iter().enumerate() {
            if i % 2 == 0 {
                signal[delay + i / 2] += s * 0.9;
            }
        }
        let probe = Arc::new(LatencyProbe::default());
        *probe.dst.lock() = Some(DstCfg {
            edge_key: "e".into(),
            rate: dst_rate,
            template: sweep_template(dst_rate),
            buf: signal,
            frame_base: 0,
            anchor: Some((Instant::now(), 0)),
            consumed: 0,
            detections: Vec::new(),
        });
        let mut dst = probe.dst.lock();
        let cfg = dst.as_mut().unwrap();
        probe.scan(cfg);
        assert_eq!(cfg.detections.len(), 1, "跨采样率应仍能检出");
    }

    #[test]
    fn finalize_filters_outliers() {
        let now = Instant::now();
        // 3 个一致 50ms + 1 个错位 450ms（漏检导致）
        let inj: Vec<Instant> = (0..4).map(|i| now - Duration::from_millis(1000 + 400 * i as u64)).collect();
        let dets: Vec<(Instant, f32)> = inj
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let lat = if i == 3 { 450.0 } else { 50.0 };
                (*t + Duration::from_secs_f64(lat / 1000.0), 0.9)
            })
            .collect();
        let ms = finalize(&inj, &dets).unwrap();
        assert!((ms - 50.0).abs() < 2.0, "应收敛到 50ms，实际 {ms}");
    }

    #[test]
    fn capture_inject_adds_burst_and_counts() {
        let probe = Arc::new(LatencyProbe::default());
        let rate = 48_000u32;
        *probe.src.lock() = Some(SrcCfg {
            src_id: "s".into(),
            ch: 2,
            template: sweep_template(rate),
            burst_starts: vec![100],
            counter: 0,
            anchor: None,
            inject_at: Vec::new(),
        });
        probe.armed.store(true, Ordering::Release);
        let data = vec![0.0f32; 200 * 2];
        let out = probe.capture_inject("s", &data).expect("首个脉冲应注入");
        // 脉冲从帧 100 开始：前 100 帧不变
        assert!(out[..200].iter().all(|&x| x == 0.0));
        assert!(out[200..].iter().any(|&x| x != 0.0));
        // 无关 source 不注入
        assert!(probe.capture_inject("other", &data).is_none());
    }
}
