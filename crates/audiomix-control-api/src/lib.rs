//! 本地控制 API：REST + SSE，与 Tauri commands 共享同一个 `Engine`。
//!
//! 在 headless（无 GUI）模式下也可用于第三方集成与远程脚本控制。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::{Json, Router};
use audiomix_core::engine::EngineEvent;
use audiomix_core::{Engine, GraphConfig};
use parking_lot::Mutex;
use tokio::sync::watch;

#[derive(Clone)]
struct ApiState {
    engine: Arc<Engine>,
}

/// 运行中的 API 服务句柄；Drop 时自动停止
pub struct ApiServer {
    shutdown: watch::Sender<bool>,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub addr: SocketAddr,
}

impl ApiServer {
    pub fn stop(&self) {
        let _ = self.shutdown.send(true);
        if let Some(h) = self.handle.lock().take() {
            let _ = h;
            // 不阻塞等待，交给 runtime 收尾
        }
    }
}

/// 启动控制 API（需要在 tokio runtime 内调用）
pub async fn spawn(engine: Arc<Engine>, bind: &str, port: u16) -> Result<ApiServer, String> {
    let addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .map_err(|e| format!("无效的监听地址: {e}"))?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/devices", get(devices))
        .route("/api/graph", get(graph).put(put_graph))
        .route("/api/status", get(status))
        .route("/api/events", get(events))
        .with_state(ApiState { engine });

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("监听 {addr} 失败: {e}"))?;
    let actual = listener
        .local_addr()
        .map_err(|e| format!("获取监听地址失败: {e}"))?;

    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await
            .ok();
    });

    tracing::info!("控制 API 已启动: http://{actual}");
    Ok(ApiServer {
        shutdown: shutdown_tx,
        handle: Mutex::new(Some(handle)),
        addr: actual,
    })
}

async fn health() -> &'static str {
    "ok"
}

async fn devices(State(st): State<ApiState>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(st.engine.refresh_devices().unwrap_or_default()).unwrap_or_default())
}

async fn graph(State(st): State<ApiState>) -> Json<GraphConfig> {
    Json(st.engine.get_graph())
}

async fn put_graph(
    State(st): State<ApiState>,
    Json(config): Json<GraphConfig>,
) -> Json<serde_json::Value> {
    match st.engine.apply_graph(config) {
        Ok(()) => Json(serde_json::json!({ "ok": true })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn status(State(st): State<ApiState>) -> Json<serde_json::Value> {
    let levels: HashMap<String, f32> = st
        .engine
        .levels()
        .into_iter()
        .collect();
    Json(serde_json::json!({
        "backend": st.engine.backend_name(),
        "levels": levels,
        "stats": st.engine.stats(),
    }))
}

async fn events(
    State(st): State<ApiState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use tokio_stream::StreamExt;
    use tokio_stream::wrappers::BroadcastStream;
    let rx = st.engine.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(ev) => {
            let name = match &ev {
                EngineEvent::GraphApplied => "graph_applied",
                EngineEvent::DevicesChanged => "devices_changed",
                EngineEvent::Underrun { .. } => "underrun",
            };
            Some(Ok(Event::default().event(name).data(
                serde_json::to_string(&ev).unwrap_or_default(),
            )))
        }
        Err(_) => None, // lagged：跳过错过的旧事件
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
