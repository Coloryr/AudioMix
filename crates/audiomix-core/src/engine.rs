//! 混音引擎：管理采集/渲染流、路由边缓冲与运行时图快照。
//!
//! 架构：
//! - 每个 source 一条后端采集线程，把 interleaved f32 推入它参与的每条
//!   路由边（source→sink）的无锁环形缓冲。
//! - 每个 sink 一条后端渲染线程，事件驱动地在需要帧时"拉动"数据：
//!   从各条边的环形缓冲取出样本 → 线性重采样 → 通道变换 → 增益累加
//!   → 软限幅 → 写入设备。
//! - 图变更通过 `ArcSwap<GraphRuntime>` 原子发布，音频线程无锁读取；
//!   未受影响的流不会被重启（改音量/增益不产生爆音间隙）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use arc_swap::ArcSwap;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::backend::{AudioBackend, CaptureCallback, RenderCallback, StartedStream};
use crate::error::{Error, Result};
use crate::mixer::{convert_channels, mix_into, peak_of, soft_clip};
use crate::model::{DeviceInfo, DeviceKind, GraphConfig, Id, SourceMode, Route, Sink, Source};
use crate::resample::PullResampler;
use crate::ring::{new_edge_ring, EdgeReader, EdgeRing, EdgeWriter};

/// 边缓冲容量：250ms（按 source 采样率折算）
const EDGE_CAPACITY_MS: usize = 250;
/// 渲染线程每 tick 最多拉取的样本数上限（防延迟堆积），约 500ms
const MAX_PULL_SAMPLES: usize = 48000 * 2;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    GraphApplied,
    DevicesChanged,
    Underrun { sink_id: Id },
}

struct SourceHandle {
    device_id: String,
    mode: SourceMode,
    info: crate::backend::StreamInfo,
    writers: Arc<ArcSwap<Vec<Arc<EdgeWriter>>>>,
    peak: Arc<AtomicU32>,
    _stream: StartedStream,
}

struct SinkHandle {
    device_id: String,
    #[allow(dead_code)] // 保留：未来统计/展示 sink 实际流格式
    info: crate::backend::StreamInfo,
    peak: Arc<AtomicU32>,
    underruns: Arc<AtomicU64>,
    _stream: StartedStream,
}

struct EdgeHandle {
    ring: EdgeRing,
}

struct Inner {
    config: GraphConfig,
    sources: HashMap<Id, SourceHandle>,
    sinks: HashMap<Id, SinkHandle>,
    edges: HashMap<(Id, Id), EdgeHandle>,
    device_cache: Vec<DeviceInfo>,
}

/// 音频线程读取的不可变运行时快照
pub struct GraphRuntime {
    pub sink_volume: HashMap<Id, f32>,
    pub sink_edges: HashMap<Id, Vec<EdgeRef>>,
}

pub struct EdgeRef {
    pub route_id: Id,
    pub gain: f32,
    pub muted: bool,
    pub src_rate: u32,
    pub src_ch: u16,
    pub reader: Arc<EdgeReader>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct EngineStats {
    pub source_dropped: HashMap<Id, u64>,
    pub sink_underruns: HashMap<Id, u64>,
}

pub struct Engine {
    backend: Arc<dyn AudioBackend>,
    inner: Mutex<Inner>,
    runtime: Arc<ArcSwap<GraphRuntime>>,
    events: broadcast::Sender<EngineEvent>,
}

impl Engine {
    pub fn new(backend: Arc<dyn AudioBackend>) -> Result<Arc<Self>> {
        let devices = backend.enumerate_devices()?;
        let (events, _) = broadcast::channel(64);
        let engine = Arc::new(Self {
            backend,
            inner: Mutex::new(Inner {
                config: GraphConfig::default(),
                sources: HashMap::new(),
                sinks: HashMap::new(),
                edges: HashMap::new(),
                device_cache: devices,
            }),
            runtime: Arc::new(ArcSwap::from_pointee(GraphRuntime {
                sink_volume: HashMap::new(),
                sink_edges: HashMap::new(),
            })),
            events,
        });
        Ok(engine)
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    fn emit(&self, ev: EngineEvent) {
        let _ = self.events.send(ev);
    }

    // ---------- 设备 ----------

    pub fn list_devices(&self) -> Vec<DeviceInfo> {
        self.inner.lock().device_cache.clone()
    }

    pub fn refresh_devices(&self) -> Result<Vec<DeviceInfo>> {
        let devices = self.backend.enumerate_devices()?;
        let changed = {
            let mut inner = self.inner.lock();
            let changed = inner.device_cache.len() != devices.len()
                || inner
                    .device_cache
                    .iter()
                    .any(|old| !devices.iter().any(|new| new.id == old.id))
                || devices
                    .iter()
                    .any(|new| !inner.device_cache.iter().any(|old| old.id == new.id));
            inner.device_cache = devices.clone();
            changed
        };
        self.emit(EngineEvent::DevicesChanged);
        // 设备热插拔后必须重新对齐流：虚拟线缆是**应用启动之后**才 attach 的，
        // 启动时设备还不存在 → 那条 source/sink 被「跳过」，不补启就会一直静默
        // （混音页看起来路由接好了却完全没声音）。
        if changed {
            self.sync_streams()?;
        }
        Ok(devices)
    }

    // ---------- 图 ----------

    pub fn get_graph(&self) -> GraphConfig {
        self.inner.lock().config.clone()
    }

    /// 整体应用混音图（diff：只重启设备/模式/启停变化的流）。
    pub fn apply_graph(&self, config: GraphConfig) -> Result<()> {
        config.validate()?;
        self.inner.lock().config = config;
        self.sync_streams()
    }

    pub fn set_route_gain(&self, route_id: &str, gain: f32) -> Result<()> {
        let mut inner = self.inner.lock();
        let route = inner
            .config
            .routes
            .iter_mut()
            .find(|r| r.id == route_id)
            .ok_or_else(|| Error::InvalidGraph(format!("route {route_id} 不存在")))?;
        route.gain = gain.clamp(0.0, 4.0);
        self.sync_streams_locked(&mut inner)
    }

    pub fn set_route_muted(&self, route_id: &str, muted: bool) -> Result<()> {
        let mut inner = self.inner.lock();
        let route = inner
            .config
            .routes
            .iter_mut()
            .find(|r| r.id == route_id)
            .ok_or_else(|| Error::InvalidGraph(format!("route {route_id} 不存在")))?;
        route.muted = muted;
        self.sync_streams_locked(&mut inner)
    }

    pub fn set_sink_volume(&self, sink_id: &str, volume: f32) -> Result<()> {
        let mut inner = self.inner.lock();
        let sink = inner
            .config
            .sinks
            .iter_mut()
            .find(|s| s.id == sink_id)
            .ok_or_else(|| Error::InvalidGraph(format!("sink {sink_id} 不存在")))?;
        sink.volume = volume.clamp(0.0, 1.0);
        self.sync_streams_locked(&mut inner)
    }

    pub fn set_source_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let mut inner = self.inner.lock();
        let src = inner
            .config
            .sources
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| Error::InvalidGraph(format!("source {id} 不存在")))?;
        src.enabled = enabled;
        self.sync_streams_locked(&mut inner)
    }

    pub fn set_sink_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let mut inner = self.inner.lock();
        let sink = inner
            .config
            .sinks
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| Error::InvalidGraph(format!("sink {id} 不存在")))?;
        sink.enabled = enabled;
        self.sync_streams_locked(&mut inner)
    }

    // ---------- 状态 ----------

    /// 各 source/sink 的峰值电平快照（0..=1+）
    pub fn levels(&self) -> HashMap<Id, f32> {
        let inner = self.inner.lock();
        let mut out = HashMap::new();
        for (id, h) in &inner.sources {
            out.insert(id.clone(), f32::from_bits(h.peak.load(Ordering::Relaxed)));
        }
        for (id, h) in &inner.sinks {
            out.insert(id.clone(), f32::from_bits(h.peak.load(Ordering::Relaxed)));
        }
        out
    }

    pub fn stats(&self) -> EngineStats {
        let inner = self.inner.lock();
        EngineStats {
            source_dropped: inner
                .sources
                .iter()
                .map(|(id, h)| {
                    let dropped = h.writers.load().iter().map(|w| w.dropped()).sum();
                    (id.clone(), dropped)
                })
                .collect(),
            sink_underruns: inner
                .sinks
                .iter()
                .map(|(id, h)| (id.clone(), h.underruns.load(Ordering::Relaxed)))
                .collect(),
        }
    }

    /// 停止全部流（退出前调用）
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock();
        inner.sources.clear();
        inner.sinks.clear();
        inner.edges.clear();
        self.runtime.store(Arc::new(GraphRuntime {
            sink_volume: HashMap::new(),
            sink_edges: HashMap::new(),
        }));
    }

    // ---------- 内部 ----------

    fn sync_streams(&self) -> Result<()> {
        let mut inner = self.inner.lock();
        self.sync_streams_locked(&mut inner)
    }

    /// 对比配置与运行中的流，启动/停止/重建受影响部分，并重新发布运行时快照。
    fn sync_streams_locked(&self, inner: &mut Inner) -> Result<()> {
        let devices = self.backend.enumerate_devices()?;
        inner.device_cache = devices;

        // ---- sources ----
        let mut keep_sources: HashMap<Id, SourceHandle> = HashMap::new();
        for src in &inner.config.sources {
            let Some(dev) = find_device(&inner.device_cache, &src.device_id) else {
                tracing::warn!("source {} 的设备 {} 不存在，跳过", src.id, src.device_id);
                continue;
            };
            let need_kind = match src.mode {
                SourceMode::DeviceInput => DeviceKind::Input,
                SourceMode::Loopback => DeviceKind::Output,
            };
            if dev.kind != need_kind {
                tracing::warn!(
                    "source {}: 设备 {} 类型与模式不匹配，跳过",
                    src.id,
                    src.device_id
                );
                continue;
            }
            if !src.enabled {
                continue;
            }
            // 已有流且设备/模式未变 → 保留
            if let Some(h) = inner.sources.remove(&src.id) {
                if h.device_id == src.device_id && h.mode == src.mode {
                    keep_sources.insert(src.id.clone(), h);
                    continue;
                }
                // 设备或模式变了：丢弃旧句柄，走下面的重建路径
                tracing::info!("source {} 设备/模式变更，重启流", src.id);
            }
            // 新建流
            let writers: Arc<ArcSwap<Vec<Arc<EdgeWriter>>>> =
                Arc::new(ArcSwap::from_pointee(Vec::new()));
            let peak = Arc::new(AtomicU32::new(0f32.to_bits()));
            let cb = make_capture_callback(peak.clone(), writers.clone());
            let started = match src.mode {
                SourceMode::DeviceInput => self.backend.start_capture(&src.device_id, cb),
                SourceMode::Loopback => self.backend.start_loopback(&src.device_id, cb),
            }
            .map_err(|e| {
                tracing::error!("启动采集流失败 (source={}, dev={}): {e}", src.id, src.device_id);
                e
            })?;
            tracing::info!(
                "source {} 已启动: dev={} rate={} ch={} mode={:?}",
                src.id,
                src.device_id,
                started.info.sample_rate,
                started.info.channels,
                src.mode
            );
            keep_sources.insert(
                src.id.clone(),
                SourceHandle {
                    device_id: src.device_id.clone(),
                    mode: src.mode,
                    info: started.info,
                    writers,
                    peak,
                    _stream: started,
                },
            );
        }
        inner.sources = keep_sources;

        // ---- sinks ----
        let mut keep_sinks: HashMap<Id, SinkHandle> = HashMap::new();
        for sink in &inner.config.sinks {
            if !sink.enabled {
                continue;
            }
            let Some(dev) = find_device(&inner.device_cache, &sink.device_id) else {
                tracing::warn!("sink {} 的设备 {} 不存在，跳过", sink.id, sink.device_id);
                continue;
            };
            if dev.kind != DeviceKind::Output {
                tracing::warn!("sink {}: 设备 {} 不是输出设备，跳过", sink.id, sink.device_id);
                continue;
            }
            if let Some(h) = inner.sinks.remove(&sink.id) {
                if h.device_id == sink.device_id {
                    keep_sinks.insert(sink.id.clone(), h);
                    continue;
                }
                tracing::info!("sink {} 设备变更，重启流", sink.id);
            }
            let peak = Arc::new(AtomicU32::new(0f32.to_bits()));
            let underruns = Arc::new(AtomicU64::new(0));
            // 流格式在打开设备后注入（以 GetMixFormat 实际结果为准）
            let info_rate = Arc::new(AtomicU32::new(0));
            let info_ch = Arc::new(AtomicU32::new(0));
            let cb = make_render_callback(
                sink.id.clone(),
                self.runtime.clone(),
                info_rate.clone(),
                info_ch.clone(),
                peak.clone(),
                underruns.clone(),
            );
            let started = self
                .backend
                .start_render(&sink.device_id, cb)
                .map_err(|e| {
                    tracing::error!("启动渲染流失败 (sink={}, dev={}): {e}", sink.id, sink.device_id);
                    e
                })?;
            info_rate.store(started.info.sample_rate, Ordering::Release);
            info_ch.store(started.info.channels as u32, Ordering::Release);
            tracing::info!(
                "sink {} 已启动: dev={} rate={} ch={}",
                sink.id,
                sink.device_id,
                started.info.sample_rate,
                started.info.channels
            );
            keep_sinks.insert(
                sink.id.clone(),
                SinkHandle {
                    device_id: sink.device_id.clone(),
                    info: started.info,
                    peak,
                    underruns,
                    _stream: started,
                },
            );
        }
        inner.sinks = keep_sinks;

        // ---- edges：仅保留两端流都在运行的路由，未变的复用缓冲 ----
        let mut new_edges: HashMap<(Id, Id), EdgeHandle> = HashMap::new();
        for route in &inner.config.routes {
            let key = (route.source_id.clone(), route.sink_id.clone());
            if !inner.sources.contains_key(&route.source_id)
                || !inner.sinks.contains_key(&route.sink_id)
            {
                continue;
            }
            let ring = match inner.edges.remove(&key) {
                Some(h) => h.ring,
                None => {
                    let src = &inner.sources[&route.source_id];
                    let cap_frames = src.info.sample_rate as usize * EDGE_CAPACITY_MS / 1000;
                    new_edge_ring(cap_frames, src.info.channels as usize)
                }
            };
            new_edges.insert(key, EdgeHandle { ring });
        }
        inner.edges = new_edges;

        // ---- 构建并发布运行时快照 ----
        let mut sink_edges: HashMap<Id, Vec<EdgeRef>> = HashMap::new();
        let mut source_writers: HashMap<Id, Vec<Arc<EdgeWriter>>> = HashMap::new();
        for (key, eh) in &inner.edges {
            let Some(route) = inner.config.routes.iter().find(|r| {
                r.source_id == key.0 && r.sink_id == key.1
            }) else {
                continue;
            };
            let src = &inner.sources[&key.0];
            source_writers
                .entry(key.0.clone())
                .or_default()
                .push(eh.ring.writer.clone());
            sink_edges
                .entry(key.1.clone())
                .or_default()
                .push(EdgeRef {
                    route_id: route.id.clone(),
                    gain: route.gain,
                    muted: route.muted,
                    src_rate: src.info.sample_rate,
                    src_ch: src.info.channels,
                    reader: eh.ring.reader.clone(),
                });
        }
        let sink_volume = inner
            .config
            .sinks
            .iter()
            .map(|s| (s.id.clone(), s.volume))
            .collect();
        self.runtime.store(Arc::new(GraphRuntime {
            sink_volume,
            sink_edges,
        }));
        for (id, h) in &inner.sources {
            let writers = source_writers.get(id).cloned().unwrap_or_default();
            h.writers.store(Arc::new(writers));
        }

        self.emit(EngineEvent::GraphApplied);
        Ok(())
    }
}

fn find_device(devices: &[DeviceInfo], id: &str) -> Option<DeviceInfo> {
    devices.iter().find(|d| d.id == id).cloned()
}

fn make_capture_callback(
    peak: Arc<AtomicU32>,
    writers: Arc<ArcSwap<Vec<Arc<EdgeWriter>>>>,
) -> CaptureCallback {
    Box::new(move |data: &[f32]| {
        peak.store(peak_of(data).to_bits(), Ordering::Relaxed);
        for w in writers.load().iter() {
            w.push(data);
        }
    })
}

/// 每个 sink 渲染线程私有的边状态
struct SinkEdgeState {
    resampler: PullResampler,
    scratch: Vec<f32>,
    gen: Vec<f32>,
    converted: Vec<f32>,
}

impl SinkEdgeState {
    fn new(src_rate: u32, dst_rate: u32, src_ch: u16) -> Self {
        Self {
            resampler: PullResampler::new(src_rate, dst_rate, src_ch),
            scratch: Vec::new(),
            gen: Vec::new(),
            converted: Vec::new(),
        }
    }
}

fn make_render_callback(
    sink_id: Id,
    runtime: Arc<ArcSwap<GraphRuntime>>,
    info_rate: Arc<AtomicU32>,
    info_ch: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    underruns: Arc<AtomicU64>,
) -> RenderCallback {
    let states: Arc<Mutex<HashMap<Id, SinkEdgeState>>> = Arc::new(Mutex::new(HashMap::new()));
    Box::new(move |out: &mut [f32]| {
        let rt = runtime.load_full();
        let volume = rt.sink_volume.get(&sink_id).copied().unwrap_or(1.0);
        let edges = rt.sink_edges.get(&sink_id);
        let ch_out = info_ch.load(Ordering::Acquire) as usize;
        let out_rate = info_rate.load(Ordering::Acquire);
        let frames = if ch_out == 0 { 0 } else { out.len() / ch_out };
        if ch_out == 0 || out_rate == 0 {
            // 流格式尚未注入（启动竞态窗口），输出静音
            out.fill(0.0);
            return;
        }
        out.fill(0.0);
        let Some(edges) = edges else {
            return;
        };

        let mut states = states.lock();
        // 回收已删除路由的状态
        states.retain(|rid, _| edges.iter().any(|e| &e.route_id == rid));

        for e in edges {
            let created = !states.contains_key(&e.route_id);
            let st = states
                .entry(e.route_id.clone())
                .or_insert_with(|| SinkEdgeState::new(e.src_rate, out_rate, e.src_ch));
            if created {
                tracing::info!(
                    "路由 {} 重采样器: {}Hz/{}ch → {}Hz/{}ch（step={:.6}）",
                    e.route_id,
                    e.src_rate,
                    e.src_ch,
                    out_rate,
                    ch_out,
                    e.src_rate as f64 / out_rate.max(1) as f64
                );
            }
            // 1. 从环形缓冲取原始样本
            let mut scratch = std::mem::take(&mut st.scratch);
            scratch.clear();
            e.reader.drain(MAX_PULL_SAMPLES, &mut scratch);
            st.resampler.input_samples(&scratch);
            st.scratch = scratch;
            // 2. 重采样到 sink 采样率（frames * src_ch）
            let mut gen = std::mem::take(&mut st.gen);
            gen.clear();
            st.resampler.generate(frames, &mut gen);
            let gain = if e.muted { 0.0 } else { e.gain } * volume;
            // 3. 通道变换 + 增益累加
            if e.src_ch as usize == ch_out {
                mix_into(out, &gen[..gen.len().min(out.len())], gain);
            } else {
                let mut converted = std::mem::take(&mut st.converted);
                converted.clear();
                converted.resize(frames * ch_out, 0.0);
                convert_channels(&gen, e.src_ch as usize, &mut converted, ch_out, frames);
                mix_into(out, &converted, gain);
                st.converted = converted;
            }
            st.gen = gen;
        }
        // 4. 软限幅 + 电平
        soft_clip(out);
        peak.store(peak_of(out).to_bits(), Ordering::Relaxed);
        if states.values().any(|s| s.resampler.underruns > 0) {
            // 简单上报：一次性清零后累加到 sink 级计数
            for s in states.values_mut() {
                let u = s.resampler.underruns;
                s.resampler.underruns = 0;
                underruns.fetch_add(u, Ordering::Relaxed);
            }
        }
    })
}

/// 生成短随机 id
pub fn new_id(prefix: &str) -> Id {
    let u = uuid::Uuid::new_v4();
    format!("{prefix}-{}", &u.simple().to_string()[..8])
}

/// 便捷构造（供控制层使用）
pub fn make_source(device_id: &str, name: &str, mode: SourceMode) -> Source {
    Source {
        id: new_id("src"),
        name: name.to_string(),
        device_id: device_id.to_string(),
        mode,
        enabled: true,
    }
}

pub fn make_sink(device_id: &str, name: &str) -> Sink {
    Sink {
        id: new_id("sink"),
        name: name.to_string(),
        device_id: device_id.to_string(),
        volume: 1.0,
        enabled: true,
    }
}

pub fn make_route(source_id: &str, sink_id: &str) -> Route {
    Route {
        id: new_id("route"),
        source_id: source_id.to_string(),
        sink_id: sink_id.to_string(),
        gain: 1.0,
        muted: false,
    }
}
