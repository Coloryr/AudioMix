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
use std::time::Duration;

use parking_lot::Mutex;

use arc_swap::ArcSwap;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::backend::{AudioBackend, CaptureCallback, RenderCallback, StartedStream};
use crate::dsp::DspChain;
use crate::error::{Error, Result};
use crate::mixer::{convert_channels, mix_into, peak_of, soft_clip};
use crate::model::{
    DeviceInfo, DeviceKind, DspNode, GraphConfig, Id, Processor, SourceMode, Route, Sink, Source,
};
use crate::resample::{PullResampler, ResamplerQuality};
use crate::ring::{new_edge_ring, EdgeReader, EdgeRing, EdgeWriter};

/// 边缓冲容量：250ms（按 source 采样率折算）
const EDGE_CAPACITY_MS: usize = 250;
/// 渲染线程每 tick 最多拉取的样本数上限（防延迟堆积），约 500ms
const MAX_PULL_SAMPLES: usize = 48000 * 2;
/// 设备热插拔看门狗的轮询间隔（`spawn_device_watchdog`）
const DEVICE_WATCHDOG_INTERVAL: Duration = Duration::from_secs(3);

/// 引擎事件（`subscribe` 广播，供 UI / SSE 推送）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    /// 混音图已被应用（流有启动/停止/重建）
    GraphApplied,
    /// 设备列表发生变化（热插拔）
    DevicesChanged,
    /// 某 sink 渲染欠载（数据到达不及时被补静音）
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
    /// 有效路径的边缓冲（key = 路径 id，见 [`resolve_paths`]）
    edges: HashMap<String, EdgeHandle>,
    device_cache: Vec<DeviceInfo>,
    /// 重采样质量档位（Settings 可切换）
    resample_quality: ResamplerQuality,
    /// 边环形缓冲容量（ms，按 source 采样率折算帧数）
    edge_capacity_ms: usize,
}

/// 音频线程读取的不可变运行时快照
pub struct GraphRuntime {
    /// sink id → 系统音量（0..=1）
    pub sink_volume: HashMap<Id, f32>,
    /// sink id → 该 sink 的全部路由边
    pub sink_edges: HashMap<Id, Vec<EdgeRef>>,
    /// 重采样质量档位（切换后渲染线程按此重建各边重采样器）
    pub resample_quality: ResamplerQuality,
}

/// 快照里的一条路由边：source → sink 的采样格式与增益参数。
pub struct EdgeRef {
    /// 所属 Route 的 id（渲染线程用它管理每条边的重采样状态）
    pub route_id: Id,
    /// 路由增益（muted 时实际按 0 处理）
    pub gain: f32,
    pub muted: bool,
    /// source 侧采样率（边缓冲内的样本格式）
    pub src_rate: u32,
    /// source 侧通道数
    pub src_ch: u16,
    /// DSP 节点链（渲染线程按值变化重建 DSP 状态）
    pub nodes: Arc<[DspNode]>,
    /// 该边的环形缓冲读端
    pub reader: Arc<EdgeReader>,
}

/// 引擎运行统计（`stats()` 快照）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct EngineStats {
    /// source id → 采集侧因边缓冲满而被丢弃的样本数（持续增长 = 上游消费不及时）
    pub source_dropped: HashMap<Id, u64>,
    /// sink id → 渲染欠载次数（每次欠载输出被补静音）
    pub sink_underruns: HashMap<Id, u64>,
}

/// 混音引擎：持有全部采集/渲染流与路由边缓冲，接收图变更并 diff 应用。
pub struct Engine {
    backend: Arc<dyn AudioBackend>,
    inner: Mutex<Inner>,
    runtime: Arc<ArcSwap<GraphRuntime>>,
    events: broadcast::Sender<EngineEvent>,
}

impl Engine {
    /// 用给定后端创建引擎并缓存首次设备枚举结果。
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
                resample_quality: ResamplerQuality::default(),
                edge_capacity_ms: EDGE_CAPACITY_MS,
            }),
            runtime: Arc::new(ArcSwap::from_pointee(GraphRuntime {
                sink_volume: HashMap::new(),
                sink_edges: HashMap::new(),
                resample_quality: ResamplerQuality::default(),
            })),
            events,
        });
        engine.spawn_device_watchdog();
        Ok(engine)
    }

    /// 后端名称（诊断/状态页用）。
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// 订阅引擎事件（tokio broadcast；落后会收到 `Lagged`，调用方自行跳过即可）。
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    fn emit(&self, ev: EngineEvent) {
        let _ = self.events.send(ev);
    }

    // ---------- 设备 ----------

    /// 缓存的设备列表（上次枚举结果；要最新数据用 [`Self::refresh_devices`]）。
    pub fn list_devices(&self) -> Vec<DeviceInfo> {
        self.inner.lock().device_cache.clone()
    }

    /// 重新枚举设备；列表有变化时广播 [`EngineEvent::DevicesChanged`] 并重新对齐流。
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
            tracing::info!("设备列表变化（现在 {} 台），已重新对齐流", devices.len());
            self.sync_streams()?;
        }
        Ok(devices)
    }

    /// 设备热插拔看门狗：蓝牙/USB 耳机插拔、虚拟线缆接入后自动重新枚举。
    ///
    /// 不用 `IMMNotificationClient`（纯 Rust 接 COM 事件源繁琐，还要管理回调对象
    /// 生命周期），轮询枚举每几秒一次开销可忽略；好处是窗口关到托盘 / headless
    /// 运行时也照常工作，且设备一出现就 `sync_streams`，路由不用手动刷新。
    /// 引擎实例释放（Arc 归零）后线程自动退出。
    fn spawn_device_watchdog(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        if let Err(e) = std::thread::Builder::new().name("device-watchdog".into()).spawn(move || {
            loop {
                std::thread::sleep(DEVICE_WATCHDOG_INTERVAL);
                // 睡醒后引擎可能已被释放（应用退出/测试结束）
                let Some(engine) = weak.upgrade() else { break };
                if let Err(e) = engine.refresh_devices() {
                    tracing::debug!("设备看门狗枚举失败（下一轮重试）: {e}");
                }
            }
        }) {
            tracing::warn!("设备看门狗线程启动失败（热插拔后需手动刷新设备）: {e}");
        }
    }

    // ---------- 图 ----------

    /// 当前混音图。
    pub fn get_graph(&self) -> GraphConfig {
        self.inner.lock().config.clone()
    }

    /// 整体应用混音图（diff：只重启设备/模式/启停变化的流）。
    pub fn apply_graph(&self, config: GraphConfig) -> Result<()> {
        config.validate()?;
        self.inner.lock().config = config;
        self.sync_streams()
    }

    /// 修改某条路由的增益（0.0..=4.0，超范围被钳制）。
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

    /// 静音/取消静音某条路由。
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

    /// 更新某个 DSP 处理方块（类型 + 参数 + 旁路开关）。
    ///
    /// 只重发运行时快照，不重启任何流；受影响路径的 DSP 链签名变化后由音频
    /// 线程就地重建（重建瞬间滤波器状态清零）。方块必须已存在（画布上先添加）。
    pub fn set_processor_params(&self, processor_id: &str, node: DspNode) -> Result<()> {
        let mut inner = self.inner.lock();
        let proc = inner
            .config
            .processors
            .iter_mut()
            .find(|p| p.id == processor_id)
            .ok_or_else(|| Error::InvalidGraph(format!("processor {processor_id} 不存在")))?;
        proc.node = node;
        self.publish_runtime(&inner);
        Ok(())
    }

    /// 设置 sink 音量（0.0..=1.0，超范围被钳制）。
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

    /// 启用/停用 source（停用即停止其采集流）。
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

    /// 启用/停用 sink（停用即停止其渲染流）。
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

    /// 运行统计快照（丢弃/欠载计数，见 [`EngineStats`]）。
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
            resample_quality: inner.resample_quality,
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

        // ---- edges：解析有效路径（源 → …processor… → 汇），两端流都在运行的
        //        路径按 key 复用缓冲，其余重建 ----
        let paths = resolve_paths(&inner.config);
        let mut new_edges: HashMap<String, EdgeHandle> = HashMap::new();
        for path in &paths {
            if !inner.sources.contains_key(&path.source_id)
                || !inner.sinks.contains_key(&path.sink_id)
            {
                continue;
            }
            let ring = match inner.edges.remove(&path.key) {
                Some(h) => h.ring,
                None => {
                    let src = &inner.sources[&path.source_id];
                    let cap_frames =
                        src.info.sample_rate as usize * inner.edge_capacity_ms / 1000;
                    new_edge_ring(cap_frames, src.info.channels as usize)
                }
            };
            new_edges.insert(path.key.clone(), EdgeHandle { ring });
        }
        inner.edges = new_edges;

        // ---- 构建并发布运行时快照 ----
        self.publish_runtime(inner);

        self.emit(EngineEvent::GraphApplied);
        Ok(())
    }

    /// 从 Inner 构建运行时快照并原子发布（音频线程无锁读取新配置）
    fn publish_runtime(&self, inner: &Inner) {
        let mut sink_edges: HashMap<Id, Vec<EdgeRef>> = HashMap::new();
        let mut source_writers: HashMap<Id, Vec<Arc<EdgeWriter>>> = HashMap::new();
        for path in resolve_paths(&inner.config) {
            let Some(eh) = inner.edges.get(&path.key) else {
                continue;
            };
            let src = &inner.sources[&path.source_id];
            source_writers
                .entry(path.source_id.clone())
                .or_default()
                .push(eh.ring.writer.clone());
            sink_edges
                .entry(path.sink_id.clone())
                .or_default()
                .push(EdgeRef {
                    route_id: path.key,
                    gain: path.gain,
                    muted: path.muted,
                    src_rate: src.info.sample_rate,
                    src_ch: src.info.channels,
                    nodes: path.nodes.into(),
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
            resample_quality: inner.resample_quality,
        }));
        for (id, h) in &inner.sources {
            let writers = source_writers.get(id).cloned().unwrap_or_default();
            h.writers.store(Arc::new(writers));
        }
    }

    /// 设置重采样质量档位；立即重新发布快照，各边渲染状态在下一次渲染时重建。
    pub fn set_resample_quality(&self, quality: ResamplerQuality) {
        let mut inner = self.inner.lock();
        if inner.resample_quality == quality {
            return;
        }
        inner.resample_quality = quality;
        self.publish_runtime(&inner);
        tracing::info!("重采样质量切换为 {quality:?}，各路由重采样器将在下次渲染时重建");
    }

    /// 设置边环形缓冲容量（ms，钳制在 50..=1000）。
    /// 容量是「卡顿时延迟/丢音频的上限」旋钮：加大更抗卡顿（引擎卡住时积缓冲
    /// 而不是丢样本），调小则上限更低；不影响日常稳态延迟。
    /// 变更后清除全部边缓冲，随下次 sync 按新容量重建（瞬时可能有一小段间隙）。
    pub fn set_edge_buffer_ms(&self, ms: u32) {
        let ms = ms.clamp(50, 1000) as usize;
        let mut inner = self.inner.lock();
        if inner.edge_capacity_ms == ms {
            return;
        }
        inner.edge_capacity_ms = ms;
        inner.edges.clear();
        let _ = self.sync_streams_locked(&mut inner);
        tracing::info!("边缓冲容量调整为 {ms}ms，已按新容量重建路由边缓冲");
    }
}

fn find_device(devices: &[DeviceInfo], id: &str) -> Option<DeviceInfo> {
    devices.iter().find(|d| d.id == id).cloned()
}

/// 一条解析完成的有效路径：source →（途经 0..n 个 processor）→ sink。
struct ResolvedPath {
    /// 唯一键（= 终点连线 id + 分支序号），边缓冲按它复用
    key: String,
    source_id: Id,
    sink_id: Id,
    /// 沿途各段连线增益相乘
    gain: f32,
    /// 任一段静音即整条静音
    muted: bool,
    /// 途经的 DSP 节点链（源 → 汇顺序；旁路的 processor 不进链）
    nodes: Vec<DspNode>,
}

/// 把图里的连线解析成有效路径。
///
/// 连线两端可以是 source/processor（出）与 sink/processor（入）的任意组合；
/// 对每个 sink 从它的每条入线向上游回溯，途经 processor 时把其节点（旁路则跳过）
/// 压入链、增益相乘、静音取或。一个 processor 有多条入线 = 扇入混合（每条分支
/// 独立成路径，在 sink 处相加）；成环或悬空的分支丢弃并 warn。
fn resolve_paths(config: &GraphConfig) -> Vec<ResolvedPath> {
    fn walk(
        config: &GraphConfig,
        cur: &Id,
        chain: &mut Vec<DspNode>,
        gain: f32,
        muted: bool,
        visiting: &std::collections::HashSet<&str>,
        out: &mut Vec<(Id, Vec<DspNode>, f32, bool)>,
    ) {
        if visiting.contains(cur.as_str()) {
            tracing::warn!("混音图在 {} 处成环，忽略该分支", cur);
            return;
        }
        if let Some(src) = config.source(cur) {
            out.push((src.id.clone(), chain.clone(), gain, muted));
            return;
        }
        if let Some(proc) = config.processor(cur) {
            let mut visiting = visiting.clone();
            visiting.insert(cur.as_str());
            let pushed = proc.node.enabled;
            if pushed {
                chain.push(proc.node.clone());
            }
            let mut branches = 0;
            for up in config.routes.iter().filter(|w| &w.sink_id == cur) {
                branches += 1;
                walk(config, &up.source_id, chain, gain * up.gain, muted || up.muted, &visiting, out);
            }
            if pushed {
                chain.pop();
            }
            if branches == 0 {
                tracing::warn!("processor {} 没有上游输入，忽略该路径", cur);
            }
            return;
        }
        tracing::warn!("路径上游 {} 不存在（悬空连线），忽略该分支", cur);
    }

    let mut out = Vec::new();
    for sink in &config.sinks {
        for r in config.routes.iter().filter(|r| &r.sink_id == &sink.id) {
            let mut paths: Vec<(Id, Vec<DspNode>, f32, bool)> = Vec::new();
            let visiting: std::collections::HashSet<&str> = [sink.id.as_str()].into_iter().collect();
            walk(config, &r.source_id, &mut Vec::new(), r.gain, r.muted, &visiting, &mut paths);
            // walk 是从 sink 往上游走的，链序反了 → 翻回「源 → 汇」
            for (idx, (source_id, mut nodes, gain, muted)) in paths.into_iter().enumerate() {
                nodes.reverse();
                out.push(ResolvedPath {
                    key: format!("{}#{}", r.id, idx),
                    source_id,
                    sink_id: sink.id.clone(),
                    gain,
                    muted,
                    nodes,
                });
            }
        }
    }
    out
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
    /// 创建本状态时使用的质量档位（与快照不一致时重建）
    quality: ResamplerQuality,
    scratch: Vec<f32>,
    gen: Vec<f32>,
    converted: Vec<f32>,
    /// DSP 节点签名（与快照不一致时重建处理链）
    dsp_sig: Arc<[DspNode]>,
    /// 构建链时使用的声道数（sink 声道数变化时重建）
    dsp_ch: usize,
    dsp: DspChain,
}

impl SinkEdgeState {
    fn new(src_rate: u32, dst_rate: u32, src_ch: u16, quality: ResamplerQuality) -> Self {
        Self {
            resampler: PullResampler::new(src_rate, dst_rate, src_ch, quality),
            quality,
            scratch: Vec::new(),
            gen: Vec::new(),
            converted: Vec::new(),
            dsp_sig: Vec::new().into(),
            dsp_ch: 0,
            dsp: DspChain::new(&[], dst_rate, 0),
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
                .or_insert_with(|| {
                    SinkEdgeState::new(e.src_rate, out_rate, e.src_ch, rt.resample_quality)
                });
            // 快照里的质量档位变了 → 重建该边重采样器（设置切换即时生效）
            if st.quality != rt.resample_quality {
                *st = SinkEdgeState::new(e.src_rate, out_rate, e.src_ch, rt.resample_quality);
                tracing::info!("路由 {} 重采样质量切换为 {:?}", e.route_id, rt.resample_quality);
            } else if created {
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
            // 3. DSP 节点链（在 sink 采样率/声道数上处理；节点签名变了 → 重建）
            if st.dsp_sig != e.nodes || st.dsp_ch != ch_out {
                st.dsp = DspChain::new(&e.nodes, out_rate, ch_out);
                st.dsp_sig = e.nodes.clone();
                st.dsp_ch = ch_out;
                tracing::info!("路由 {} DSP 链已重建（{} 节点）", e.route_id, e.nodes.len());
            }
            // 4. 通道变换 + 增益累加
            if e.src_ch as usize == ch_out {
                if !st.dsp.is_empty() {
                    st.dsp.process(&mut gen);
                }
                mix_into(out, &gen[..gen.len().min(out.len())], gain);
            } else {
                let mut converted = std::mem::take(&mut st.converted);
                converted.clear();
                converted.resize(frames * ch_out, 0.0);
                convert_channels(&gen, e.src_ch as usize, &mut converted, ch_out, frames);
                if !st.dsp.is_empty() {
                    st.dsp.process(&mut converted);
                }
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

/// 便捷构造（供控制层使用）
pub fn make_sink(device_id: &str, name: &str) -> Sink {
    Sink {
        id: new_id("sink"),
        name: name.to_string(),
        device_id: device_id.to_string(),
        volume: 1.0,
        enabled: true,
    }
}

/// 便捷构造（供控制层使用）：画布上的 DSP 处理方块
pub fn make_processor(node: DspNode) -> Processor {
    Processor { id: new_id("dsp"), node }
}

/// 便捷构造（供控制层使用）
pub fn make_route(source_id: &str, sink_id: &str) -> Route {
    Route {
        id: new_id("route"),
        source_id: source_id.to_string(),
        sink_id: sink_id.to_string(),
        gain: 1.0,
        muted: false,
        nodes: Vec::new(),
    }
}
