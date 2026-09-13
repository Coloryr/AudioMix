//! `UsbIpBackend`：把 USB/IP 虚拟线缆适配为引擎的 [`AudioBackend`]。
//!
//! 设备视角映射（混音图语义）：
//! - 线缆的**播放端**（Windows 应用播放进虚拟扬声器，ISO OUT 数据流入
//!   `play_ring`）→ 引擎的 **Input** 设备：`start_capture` 排空 play_ring；
//! - 线缆的**采集端**（Windows 应用从虚拟麦克风录音，ISO IN 从 `cap_ring`
//!   取数据）→ 引擎的 **Output** 设备：`start_render` 写入 cap_ring。
//!
//! 设备 id 形如 `usbip://{n}/playback` / `usbip://{n}/capture`。

use std::sync::Arc;
use std::time::{Duration, Instant};

use audiomix_core::backend::{
    AudioBackend, CaptureCallback, RenderCallback, StartedStream, StreamHandle, StreamInfo,
};
use audiomix_core::error::{Error, Result};
use audiomix_core::model::{DeviceInfo, DeviceKind};

use super::CableRegistry;

/// 采集/渲染线程的推进粒度
const TICK: Duration = Duration::from_millis(10);
/// 预先多喂给引擎的余量（毫秒）。
///
/// 引擎是**拉取式**的：sink 线程按自己的节奏取数，取不到时重采样器会「保持上一帧」
/// （见 core::resample::PullResampler），听感就是声音被拉慢 + 一卡一卡。
/// 留一点余量就能让引擎永远取得到数据，抖动由余量吸收。
const CUSHION_MS: u64 = 50;
/// 单次最多补多少毫秒（时钟跳变/线程被抢占后别一次灌太多）
const MAX_CATCHUP_MS: u64 = 100;

/// 按**单调时钟**算「到此刻为止本应喂给引擎的帧数」。
///
/// 关键：不能每拍固定喂 `sample_rate/100` 帧 —— Windows 上 `sleep(10ms)` 实测要
/// 10.38ms（定时器粒度），固定帧数会让喂数速率只有 96.3%，引擎随即欠载：
/// 音调变低约 3.8%、并且一卡一卡。这里改成按累计时间算目标帧数，
/// 睡眠抖动只会让每拍帧数略有出入，**长期平均速率精确等于采样率**。
fn frames_due(elapsed_secs: f64, rate: u64, cushion_frames: u64, fed_frames: u64) -> usize {
    let target = (elapsed_secs * rate as f64) as u64 + cushion_frames;
    let want = target.saturating_sub(fed_frames);
    want.min(rate * MAX_CATCHUP_MS / 1000) as usize
}

pub struct UsbIpBackend {
    registry: Arc<CableRegistry>,
}

impl UsbIpBackend {
    pub fn new(registry: Arc<CableRegistry>) -> Self {
        Self { registry }
    }
}

fn parse_endpoint(device_id: &str) -> Option<(u8, &'static str)> {
    let rest = device_id.strip_prefix("usbip://")?;
    let (num, side) = rest.split_once('/')?;
    let number = num.parse().ok()?;
    let side = match side {
        "playback" => "playback",
        "capture" => "capture",
        _ => return None,
    };
    Some((number, side))
}

impl AudioBackend for UsbIpBackend {
    fn enumerate_devices(&self) -> Result<Vec<DeviceInfo>> {
        let mut devices = Vec::new();
        for c in self.registry.read().iter() {
            let n = c.cfg.number;
            devices.push(DeviceInfo {
                id: format!("usbip://{n}/playback"),
                name: format!("{} (输入)", c.product_name()),
                kind: DeviceKind::Input,
                is_default: false,
                is_virtual: true,
                channels: 2,
                sample_rate: c.cfg.sample_rate,
            });
            devices.push(DeviceInfo {
                id: format!("usbip://{n}/capture"),
                name: format!("{} (输出)", c.product_name()),
                kind: DeviceKind::Output,
                is_default: false,
                is_virtual: true,
                channels: 2,
                sample_rate: c.cfg.sample_rate,
            });
        }
        Ok(devices)
    }

    fn start_capture(&self, device_id: &str, mut on_data: CaptureCallback) -> Result<StartedStream> {
        let Some((number, "playback")) = parse_endpoint(device_id) else {
            return Err(Error::DeviceNotFound(device_id.into()));
        };
        let cable = self
            .registry
            .read()
            .iter()
            .find(|c| c.cfg.number == number)
            .cloned()
            .ok_or_else(|| Error::DeviceNotFound(device_id.into()))?;
        let info = StreamInfo { sample_rate: cable.cfg.sample_rate, channels: 2 };

        // 10ms 一拍，但帧数按单调时钟累计算（见 frames_due）；不足补静音后推给混音引擎
        let rate = info.sample_rate as u64;
        let cushion = rate * CUSHION_MS / 1000;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let builder = std::thread::Builder::new().name("usbip-capture".into());
        let handle = StreamHandle::spawn(stop, builder, move |stop| {
            let start = Instant::now();
            let mut fed: u64 = 0;
            let mut ticks: u64 = 0;
            let mut buf: Vec<f32> = Vec::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(TICK);
                let frames = frames_due(start.elapsed().as_secs_f64(), rate, cushion, fed);
                if frames == 0 {
                    continue;
                }
                buf.clear();
                buf.resize(frames * 2, 0.0);
                let got = cable.play_ring.pop(&mut buf);
                on_data(&buf);
                fed += frames as u64;
                // 线缆环的自检（每 ~2 秒一条）：欠载/丢弃是「声音变慢、一卡一卡」的直接证据
                ticks += 1;
                if ticks % 200 == 0 {
                    let (p_avail, p_dropped, p_under) = cable.play_ring.stats();
                    let (c_avail, c_dropped, c_under) = cable.cap_ring.stats();
                    let fed_secs = fed as f64 / rate as f64;
                    let real = start.elapsed().as_secs_f64();
                    tracing::debug!(
                        "线缆 {} 采集: 已喂 {fed_secs:.2}s 音频 / 实际 {real:.2}s（比 {:.4}），本次取到 {got} 帧；播放接口 {} 录音接口 {}；播放环 avail={p_avail} 丢={p_dropped} 欠={p_under}；录音环 avail={c_avail} 丢={c_dropped} 欠={c_under} **裁剪={trimmed}**",
                        cable.bus_id,
                        fed_secs / real.max(f64::MIN_POSITIVE),
                        if cable.playback_active() { "alt1(流式中)" } else { "alt0(未流式)" },
                        if cable.capture_active() { "alt1(流式中)" } else { "alt0(未流式)" },
                        trimmed = cable.cap_ring.trimmed(),
                    );
                }
            }
        })?;
        Ok(StartedStream { info, handle })
    }

    fn start_loopback(&self, device_id: &str, _on_data: CaptureCallback) -> Result<StartedStream> {
        // 线缆自身已内置播放→麦克风回环；引擎视角下输出设备不支持 loopback 采集
        Err(Error::Backend(format!(
            "USB/IP 线缆不支持 loopback 采集（{device_id}）；播放端音频直接作为 Input 源接入"
        )))
    }

    fn start_render(&self, device_id: &str, mut on_fill: RenderCallback) -> Result<StartedStream> {
        let Some((number, "capture")) = parse_endpoint(device_id) else {
            return Err(Error::DeviceNotFound(device_id.into()));
        };
        let cable = self
            .registry
            .read()
            .iter()
            .find(|c| c.cfg.number == number)
            .cloned()
            .ok_or_else(|| Error::DeviceNotFound(device_id.into()))?;
        let info = StreamInfo { sample_rate: cable.cfg.sample_rate, channels: 2 };

        // 10ms 一拍，帧数同样按单调时钟算；写入 cap_ring 供 ISO IN 侧取走
        let rate = info.sample_rate as u64;
        let cushion = rate * CUSHION_MS / 1000;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let builder = std::thread::Builder::new().name("usbip-render".into());
        let handle = StreamHandle::spawn(stop, builder, move |stop| {
            let start = Instant::now();
            let mut produced: u64 = 0;
            let mut buf: Vec<f32> = Vec::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(TICK);
                let frames = frames_due(start.elapsed().as_secs_f64(), rate, cushion, produced);
                if frames == 0 {
                    continue;
                }
                buf.clear();
                buf.resize(frames * 2, 0.0);
                on_fill(&mut buf);
                // 写入录音端；reverse 模式下会同时回灌到播放端（见 Cable::write_capture）
                cable.write_capture(&buf);
                produced += frames as u64;
            }
        })?;
        Ok(StartedStream { info, handle })
    }

    fn name(&self) -> &'static str {
        "usbip"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usbip::descriptors::CableProtocol;
    use crate::usbip::device::{CableConfig, CableMode};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn cable(number: u8) -> CableConfig {
        CableConfig {
            number,
            name: String::new(),
            sample_rate: 48_000,
            bits: 16,
            mode: CableMode::Loopback,
            buffer_ms: 250,
            protocol: CableProtocol::Uac2,
        }
    }

    fn registry_with(cfgs: &[CableConfig]) -> Arc<CableRegistry> {
        let reg: Arc<CableRegistry> = Arc::new(parking_lot::RwLock::new(Vec::new()));
        let mut cables = reg.write();
        for c in cfgs {
            cables.push(crate::usbip::device::Cable::new(c.clone()).unwrap());
        }
        drop(cables);
        reg
    }

    /// 喂数节奏必须按单调时钟算：模拟 Windows 上「sleep(10ms) 实际 10.38ms」，
    /// 固定每拍 480 帧会少喂 3.7%（音调变低 + 引擎欠载卡顿），
    /// 按累计时间算则 10 秒后总帧数精确等于采样率。
    #[test]
    fn pacing_follows_monotonic_clock() {
        let rate = 48_000u64;
        let cushion = rate * CUSHION_MS / 1000;

        // 模拟 10 秒、每拍 10.384ms
        let tick = 0.010384f64;
        let mut fed: u64 = 0;
        let mut ticks = 0u32;
        while (ticks as f64) * tick < 10.0 {
            let want = frames_due(ticks as f64 * tick, rate, cushion, fed) as u64;
            fed += want;
            ticks += 1;
        }
        let target = (10.0 * rate as f64) as u64 + cushion;
        assert!(
            fed.abs_diff(target) <= rate / 100,
            "10 秒应喂 {target} 帧（含 {cushion} 帧余量），实际 {fed}"
        );
        // 固定帧数的旧做法会少喂约 3.7%
        let fixed = 480u64 * ticks as u64;
        let deficit = 1.0 - fixed as f64 / (10.0 * rate as f64);
        assert!(deficit > 0.03, "旧做法应明显少喂，实测缺口 {deficit:.4}");

        // 还没到下一拍时不应重复喂
        assert_eq!(frames_due(0.0, rate, cushion, fed), 0);
        // 单次补数有上限
        assert_eq!(frames_due(100.0, rate, cushion, 0), (rate * MAX_CATCHUP_MS / 1000) as usize);
    }

    #[test]
    fn enumerate_lists_both_sides() {
        let backend = UsbIpBackend::new(registry_with(&[cable(1), cable(2)]));
        let devices = backend.enumerate_devices().unwrap();
        assert_eq!(devices.len(), 4);
        let input = devices.iter().find(|d| d.id == "usbip://1/playback").unwrap();
        assert_eq!(input.kind, DeviceKind::Input);
        assert!(input.is_virtual);
        assert_eq!(input.sample_rate, 48_000);
        assert!(input.name.contains("Virtual Cable 01"));
        let output = devices.iter().find(|d| d.id == "usbip://1/capture").unwrap();
        assert_eq!(output.kind, DeviceKind::Output);
        assert!(audiomix_core::model::looks_virtual(&input.name));
    }

    #[test]
    fn capture_and_render_roundtrip() {
        let backend = UsbIpBackend::new(registry_with(&[cable(1)]));
        let reg = backend.registry.clone();
        let c = reg.read()[0].clone();
        // 模拟激活接口
        let set_cfg = crate::usbip::device::SetupPacket {
            request_type: 0x00,
            request: 0x09,
            value: 1,
            index: 0,
            length: 0,
        };
        c.handle_control(set_cfg, &[]);
        for iface in 1..=2u16 {
            c.handle_control(
                crate::usbip::device::SetupPacket {
                    request_type: 0x01,
                    request: 0x0B,
                    value: 1,
                    index: iface,
                    length: 0,
                },
                &[],
            );
        }

        let got = Arc::new(parking_lot::Mutex::new(Vec::<f32>::new()));
        let got2 = got.clone();
        let stream = backend
            .start_capture("usbip://1/playback", Box::new(move |d| {
                got2.lock().extend_from_slice(d);
            }))
            .unwrap();
        assert_eq!(stream.info.sample_rate, 48_000);

        // 直接往 play_ring 写数据，等待采集线程取走
        let samples: Vec<f32> = (0..960).map(|i| (i as f32 * 0.001).sin()).collect();
        c.play_ring.push(&samples);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while got.lock().len() < 960 {
            assert!(std::time::Instant::now() < deadline, "采集线程未取到数据");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!((got.lock()[0] - samples[0]).abs() < 1e-6);
        stream.handle.request_stop();
    }

    #[test]
    fn unknown_device_errors() {
        let backend = UsbIpBackend::new(registry_with(&[cable(1)]));
        assert!(matches!(
            backend.start_capture("usbip://9/playback", Box::new(|_| {})),
            Err(Error::DeviceNotFound(_))
        ));
        assert!(matches!(
            backend.start_capture("wasapi://whatever", Box::new(|_| {})),
            Err(Error::DeviceNotFound(_))
        ));
        assert!(backend.start_loopback("usbip://1/capture", Box::new(|_| {})).is_err());
    }

    #[test]
    fn stop_flag_ends_thread() {
        let backend = UsbIpBackend::new(registry_with(&[cable(1)]));
        let stream = backend.start_capture("usbip://1/playback", Box::new(|_| {})).unwrap();
        assert!(!stream.handle.is_stopped());
        stream.handle.request_stop();
        // Drop 会 join 线程
        drop(stream);
        let stop = Arc::new(AtomicBool::new(false));
        assert!(!stop.load(Ordering::Relaxed));
    }
}
