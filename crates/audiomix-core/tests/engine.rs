//! 引擎集成测试：用 FakeBackend 走完整的 apply_graph → 采集 → 混音 → 渲染链路。
//!
//! 信号全部用 DC（常数）块：passthrough 无插值误差、欠载保持帧与信号一致，
//! 因此对实时时序抖动免疫，输出均值断言可以做到紧容差。

use std::time::{Duration, Instant};

use audiomix_core::engine::Engine;
use audiomix_core::model::{
    DspKind, DspNode, GraphConfig, Processor, Route, Sink, Source, SourceMode,
};
use audiomix_core::testing::FakeBackend;

const MIN_SAMPLES: usize = 150_000;
/// 丢弃开头样本数：规避 gain 变更时环形缓冲中滞留的旧值（≈0.26s 容量）
const SKIP_SAMPLES: usize = 60_000;
const TOL: f32 = 0.02;

// ---------- 构造工具 ----------

fn dc_block(value: f32, frames: usize, channels: usize) -> Vec<f32> {
    vec![value; frames * channels]
}

fn source(id: &str, device: &str, mode: SourceMode) -> Source {
    Source {
        id: id.into(),
        name: id.into(),
        device_id: device.into(),
        mode,
        enabled: true,
    }
}

fn sink(id: &str, device: &str) -> Sink {
    Sink {
        id: id.into(),
        name: id.into(),
        device_id: device.into(),
        volume: 1.0,
        enabled: true,
    }
}

fn route(id: &str, source_id: &str, sink_id: &str, gain: f32) -> Route {
    Route {
        id: id.into(),
        source_id: source_id.into(),
        sink_id: sink_id.into(),
        gain,
        muted: false,
        nodes: Vec::new(),
    }
}

fn processor(id: &str, kind: DspKind) -> Processor {
    Processor { id: id.into(), node: DspNode { kind, enabled: true } }
}

/// 等待渲染 buffer 积累到 min_samples，然后返回 [skip..] 段的均值
fn wait_mean(backend: &FakeBackend, device: &str, min_samples: usize, skip: usize) -> f32 {
    let buf = backend.render_buffer(device);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let len = buf.lock().unwrap().len();
        if len >= min_samples {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "超时：等待 {device} 渲染输出（{len}/{min_samples}）"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let seg = &buf.lock().unwrap()[skip..];
    seg.iter().sum::<f32>() / seg.len() as f32
}

// ---------- 测试 ----------

#[test]
fn passthrough_single_source() {
    let backend = FakeBackend::new();
    // 48k 2ch，源信号 DC 0.5，256 帧
    backend.set_capture_data("dev-in-1", dc_block(0.5, 256, 2));

    let engine = Engine::new(backend.clone()).unwrap();
    engine
        .apply_graph(GraphConfig {
            processors: Vec::new(),
            sources: vec![source("src", "dev-in-1", SourceMode::DeviceInput)],
            sinks: vec![sink("out", "dev-out-1")],
            routes: vec![route("r", "src", "out", 1.0)],
        })
        .unwrap();

    let mean = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((mean - 0.5).abs() < TOL, "均值 {mean} 应为 0.5");

    // 源电平表：DC 峰值恰为 0.5
    let levels = engine.levels();
    let peak = levels["src"];
    assert!((peak - 0.5).abs() < 0.01, "源峰值 {peak} 应为 0.5");
    let out_peak = levels["out"];
    assert!((out_peak - 0.5).abs() < 0.05, "输出峰值 {out_peak} 应约为 0.5");
}

#[test]
fn two_sources_sum() {
    let backend = FakeBackend::new();
    backend.set_capture_data("dev-in-1", dc_block(0.5, 256, 2));
    backend.set_capture_data("dev-mono", dc_block(0.25, 256, 1));

    let engine = Engine::new(backend.clone()).unwrap();
    engine
        .apply_graph(GraphConfig {
            processors: Vec::new(),
            sources: vec![
                source("a", "dev-in-1", SourceMode::DeviceInput),
                source("b", "dev-mono", SourceMode::DeviceInput),
            ],
            sinks: vec![sink("out", "dev-out-1")],
            routes: vec![route("ra", "a", "out", 1.0), route("rb", "b", "out", 1.0)],
        })
        .unwrap();

    // 0.5 + 0.25 = 0.75（mono 通道复制后同值）
    let mean = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((mean - 0.75).abs() < TOL, "均值 {mean} 应为 0.75");
}

#[test]
fn gain_hot_update() {
    let backend = FakeBackend::new();
    backend.set_capture_data("dev-in-1", dc_block(0.5, 256, 2));

    let engine = Engine::new(backend.clone()).unwrap();
    engine
        .apply_graph(GraphConfig {
            processors: Vec::new(),
            sources: vec![source("src", "dev-in-1", SourceMode::DeviceInput)],
            sinks: vec![sink("out", "dev-out-1")],
            routes: vec![route("r", "src", "out", 1.0)],
        })
        .unwrap();

    let m1 = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((m1 - 0.5).abs() < TOL, "初始增益 1.0 → 均值 {m1} 应为 0.5");

    // 热更新增益，不重启流
    let started_before = backend.captures_started.load(std::sync::atomic::Ordering::SeqCst);
    engine.set_route_gain("r", 0.5).unwrap();
    backend.reset_render_buffer("dev-out-1");
    let m2 = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((m2 - 0.25).abs() < TOL, "增益 0.5 → 均值 {m2} 应为 0.25");

    let started_after = backend.captures_started.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(started_before, started_after, "改增益不应重启采集流");
}

#[test]
fn mute_route_outputs_silence() {
    let backend = FakeBackend::new();
    backend.set_capture_data("dev-in-1", dc_block(0.5, 256, 2));

    let engine = Engine::new(backend.clone()).unwrap();
    engine
        .apply_graph(GraphConfig {
            processors: Vec::new(),
            sources: vec![source("src", "dev-in-1", SourceMode::DeviceInput)],
            sinks: vec![sink("out", "dev-out-1")],
            routes: vec![route("r", "src", "out", 1.0)],
        })
        .unwrap();

    let m1 = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((m1 - 0.5).abs() < TOL);

    engine.set_route_muted("r", true).unwrap();
    backend.reset_render_buffer("dev-out-1");
    let m2 = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!(m2.abs() < TOL, "静音后均值 {m2} 应为 0");

    engine.set_route_muted("r", false).unwrap();
    backend.reset_render_buffer("dev-out-1");
    let m3 = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((m3 - 0.5).abs() < TOL, "取消静音后均值 {m3} 应恢复 0.5");
}

#[test]
fn processor_chain_hot_update_without_stream_restart() {
    let backend = FakeBackend::new();
    backend.set_capture_data("dev-in-1", dc_block(0.5, 256, 2));

    let engine = Engine::new(backend.clone()).unwrap();
    engine
        .apply_graph(GraphConfig {
            processors: vec![processor("dsp", DspKind::Gain { db: -20.0 })],
            sources: vec![source("src", "dev-in-1", SourceMode::DeviceInput)],
            sinks: vec![sink("out", "dev-out-1")],
            routes: vec![route("r1", "src", "dsp", 1.0), route("r2", "dsp", "out", 1.0)],
        })
        .unwrap();

    // 信号走 src → dsp(-20dB) → out：0.5 × 0.1 = 0.05
    let m1 = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((m1 - 0.05).abs() < TOL, "经处理器后均值 {m1} 应为 0.05");

    // 热改参数（-20dB → -6dB ≈ ×0.501），不重启流
    let started_before = backend.captures_started.load(std::sync::atomic::Ordering::SeqCst);
    engine
        .set_processor_params(
            "dsp",
            DspNode { kind: DspKind::Gain { db: -6.0 }, enabled: true },
        )
        .unwrap();
    backend.reset_render_buffer("dev-out-1");
    let m2 = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!(
        (m2 - 0.5 * 10f32.powf(-6.0 / 20.0)).abs() < TOL,
        "改参后均值 {m2} 应为 0.251"
    );
    assert_eq!(
        started_before,
        backend.captures_started.load(std::sync::atomic::Ordering::SeqCst),
        "改处理器参数不应重启采集流"
    );

    // 旁路处理器 → 直通 0.5
    engine
        .set_processor_params(
            "dsp",
            DspNode { kind: DspKind::Gain { db: -6.0 }, enabled: false },
        )
        .unwrap();
    backend.reset_render_buffer("dev-out-1");
    let m3 = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((m3 - 0.5).abs() < TOL, "旁路后均值 {m3} 应为 0.5");
}

#[test]
fn processor_fan_in_mixes_upstream_sources() {
    let backend = FakeBackend::new();
    backend.set_capture_data("dev-in-1", dc_block(0.5, 256, 2));
    backend.set_capture_data("dev-mono", dc_block(0.25, 256, 1));

    let engine = Engine::new(backend.clone()).unwrap();
    engine
        .apply_graph(GraphConfig {
            processors: vec![processor("dsp", DspKind::Gain { db: 0.0 })],
            sources: vec![
                source("a", "dev-in-1", SourceMode::DeviceInput),
                source("b", "dev-mono", SourceMode::DeviceInput),
            ],
            sinks: vec![sink("out", "dev-out-1")],
            routes: vec![
                route("w1", "a", "dsp", 1.0),
                route("w2", "b", "dsp", 1.0),
                route("w3", "dsp", "out", 1.0),
            ],
        })
        .unwrap();

    // 两条路径（a→dsp、b→dsp）独立成边，在 sink 相加：0.5 + 0.25 = 0.75
    let mean = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((mean - 0.75).abs() < TOL, "扇入混合均值 {mean} 应为 0.75");
}

#[test]
fn processor_cycle_branch_is_dropped() {
    let backend = FakeBackend::new();
    backend.set_capture_data("dev-in-1", dc_block(0.5, 256, 2));

    let engine = Engine::new(backend.clone()).unwrap();
    // dsp 输出接回自己的输入形成环：环分支被丢弃，正常分支（src→dsp→out）仍工作
    engine
        .apply_graph(GraphConfig {
            processors: vec![processor("dsp", DspKind::Gain { db: 0.0 })],
            sources: vec![source("src", "dev-in-1", SourceMode::DeviceInput)],
            sinks: vec![sink("out", "dev-out-1")],
            routes: vec![
                route("w0", "src", "dsp", 1.0),
                route("w1", "dsp", "dsp", 1.0),
                route("w2", "dsp", "out", 1.0),
            ],
        })
        .unwrap();

    let mean = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((mean - 0.5).abs() < TOL, "环分支应被丢弃，正常路径均值 {mean} 应为 0.5");
    engine.shutdown();
}

#[test]
fn sink_volume_applies() {
    let backend = FakeBackend::new();
    backend.set_capture_data("dev-in-1", dc_block(0.5, 256, 2));

    let engine = Engine::new(backend.clone()).unwrap();
    engine
        .apply_graph(GraphConfig {
            processors: Vec::new(),
            sources: vec![source("src", "dev-in-1", SourceMode::DeviceInput)],
            sinks: vec![sink("out", "dev-out-1")],
            routes: vec![route("r", "src", "out", 1.0)],
        })
        .unwrap();

    let m1 = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((m1 - 0.5).abs() < TOL);

    engine.set_sink_volume("out", 0.5).unwrap();
    backend.reset_render_buffer("dev-out-1");
    let m2 = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((m2 - 0.25).abs() < TOL, "主音量 0.5 → 均值 {m2} 应为 0.25");
}

#[test]
fn mono_source_to_stereo_sink() {
    let backend = FakeBackend::new();
    // mono 源（1ch）
    backend.set_capture_data("dev-mono", dc_block(0.5, 256, 1));

    let engine = Engine::new(backend.clone()).unwrap();
    engine
        .apply_graph(GraphConfig {
            processors: Vec::new(),
            sources: vec![source("src", "dev-mono", SourceMode::DeviceInput)],
            sinks: vec![sink("out", "dev-out-1")], // 2ch
            routes: vec![route("r", "src", "out", 1.0)],
        })
        .unwrap();

    // mono → stereo 复制后均值不变
    let mean = wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((mean - 0.5).abs() < TOL, "均值 {mean} 应为 0.5");
}

#[test]
fn resample_48k_to_96k() {
    let backend = FakeBackend::new();
    // 源 48k（dev-in-1），输出 96k（dev-out-hi）
    backend.set_capture_data("dev-in-1", dc_block(0.5, 256, 2));

    let engine = Engine::new(backend.clone()).unwrap();
    engine
        .apply_graph(GraphConfig {
            processors: Vec::new(),
            sources: vec![source("src", "dev-in-1", SourceMode::DeviceInput)],
            sinks: vec![sink("out", "dev-out-hi")],
            routes: vec![route("r", "src", "out", 1.0)],
        })
        .unwrap();

    // DC 信号经线性重采样值不变
    let mean = wait_mean(&backend, "dev-out-hi", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((mean - 0.5).abs() < TOL, "96k 输出均值 {mean} 应为 0.5");
}

#[test]
fn loopback_mode_routes_output_capture() {
    let backend = FakeBackend::new();
    backend.set_capture_data("dev-out-1", dc_block(0.4, 256, 2));

    let engine = Engine::new(backend.clone()).unwrap();
    engine
        .apply_graph(GraphConfig {
            processors: Vec::new(),
            sources: vec![source("src", "dev-out-1", SourceMode::Loopback)],
            sinks: vec![sink("out", "dev-out-hi")],
            routes: vec![route("r", "src", "out", 1.0)],
        })
        .unwrap();

    let mean = wait_mean(&backend, "dev-out-hi", MIN_SAMPLES, SKIP_SAMPLES);
    assert!((mean - 0.4).abs() < TOL, "loopback 混音均值 {mean} 应为 0.4");
}

#[test]
fn stream_lifecycle_start_stop_restart() {
    let backend = FakeBackend::new();
    backend.set_capture_data("dev-in-1", dc_block(0.5, 256, 2));

    let engine = Engine::new(backend.clone()).unwrap();
    let graph = GraphConfig {
            processors: Vec::new(),
        sources: vec![source("src", "dev-in-1", SourceMode::DeviceInput)],
        sinks: vec![sink("out", "dev-out-1")],
        routes: vec![route("r", "src", "out", 1.0)],
    };
    engine.apply_graph(graph).unwrap();
    assert_eq!(backend.alive_captures(), 1);
    assert_eq!(backend.alive_renders(), 1);
    assert_eq!(backend.captures_started.load(std::sync::atomic::Ordering::SeqCst), 1);

    // 禁用源 → 采集流停止
    engine.set_source_enabled("src", false).unwrap();
    assert_eq!(backend.alive_captures(), 0, "禁用源后采集流应停止");
    assert_eq!(backend.alive_renders(), 1, "输出流不受影响");

    // 重新启用 → 新流启动
    engine.set_source_enabled("src", true).unwrap();
    assert_eq!(backend.alive_captures(), 1);
    assert_eq!(
        backend.captures_started.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "重新启用应创建新流"
    );

    // 移除 sink → 渲染流停止
    let mut g = engine.get_graph();
    g.sinks.clear();
    g.routes.clear();
    engine.apply_graph(g).unwrap();
    assert_eq!(backend.alive_renders(), 0);
    assert_eq!(backend.alive_captures(), 1, "源流不应受移除 sink 影响");

    // 清图后全部停止
    engine.shutdown();
    assert_eq!(backend.alive_captures(), 0);
}

#[test]
fn source_device_change_restarts_stream() {
    let backend = FakeBackend::new();
    backend.set_capture_data("dev-in-1", dc_block(0.5, 256, 2));
    backend.set_capture_data("dev-mono", dc_block(0.2, 256, 1));

    let engine = Engine::new(backend.clone()).unwrap();
    let mut g = GraphConfig {
            processors: Vec::new(),
        sources: vec![source("src", "dev-in-1", SourceMode::DeviceInput)],
        sinks: vec![sink("out", "dev-out-1")],
        routes: vec![route("r", "src", "out", 1.0)],
    };
    engine.apply_graph(g.clone()).unwrap();
    assert_eq!(
        backend.captures_started.load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    // 同 id 换绑设备 → 流应重启
    g.sources[0].device_id = "dev-mono".into();
    engine.apply_graph(g).unwrap();
    assert_eq!(
        backend.captures_started.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "换绑设备应重启采集流"
    );
    assert_eq!(backend.alive_captures(), 1, "旧流应已停止");
}

#[test]
fn missing_device_is_skipped_without_error() {
    let backend = FakeBackend::new();
    let engine = Engine::new(backend.clone()).unwrap();
    engine
        .apply_graph(GraphConfig {
            processors: Vec::new(),
            sources: vec![source("src", "dev-not-exist", SourceMode::DeviceInput)],
            sinks: vec![sink("out", "dev-out-1")],
            routes: vec![route("r", "src", "out", 1.0)],
        })
        .unwrap();

    // 源被跳过（warn），sink 仍应启动；无路由连接 → 输出静音
    assert_eq!(backend.alive_captures(), 0);
    assert_eq!(backend.alive_renders(), 1);
    engine.shutdown();
}

#[test]
fn stats_reflect_edge_counts() {
    let backend = FakeBackend::new();
    backend.set_capture_data("dev-in-1", dc_block(0.5, 256, 2));

    let engine = Engine::new(backend.clone()).unwrap();
    engine
        .apply_graph(GraphConfig {
            processors: Vec::new(),
            sources: vec![source("src", "dev-in-1", SourceMode::DeviceInput)],
            sinks: vec![sink("out", "dev-out-1")],
            routes: vec![route("r", "src", "out", 1.0)],
        })
        .unwrap();

    // 等 render 跑起来，stats 应包含两个节点
    wait_mean(&backend, "dev-out-1", MIN_SAMPLES, SKIP_SAMPLES);
    let stats = engine.stats();
    assert!(stats.source_dropped.contains_key("src"));
    assert!(stats.sink_underruns.contains_key("out"));
    engine.shutdown();
}
