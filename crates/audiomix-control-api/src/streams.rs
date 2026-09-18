//! 事件与电平流（SSE）。
//!
//! - `/api/events`：引擎事件（`graph_applied` / `devices_changed` / `underrun`）。
//!   广播通道满时会丢旧事件（客户端收到的是「发生过」而非「完整序列」），
//!   重连后建议用 `GET /api/graph` + `GET /api/status` 重新对齐状态。
//! - `/api/stream/levels`：按间隔推电平/频谱，省掉轮询（远程电平表用）。

use std::convert::Infallible;
use std::time::Duration;

use audiomix_core::engine::EngineEvent;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::Deserialize;

use crate::ApiState;

/// 电平快照：各节点峰值 + 频段（fft 关闭时 spectra 为空）
pub(crate) fn levels_payload(st: &ApiState) -> String {
    let levels: std::collections::HashMap<String, f32> =
        st.engine.levels().into_iter().collect();
    serde_json::json!({ "levels": levels, "spectra": st.engine.stats().spectra }).to_string()
}

pub async fn levels_snapshot(State(st): State<ApiState>) -> axum::Json<serde_json::Value> {
    let levels: std::collections::HashMap<String, f32> =
        st.engine.levels().into_iter().collect();
    axum::Json(serde_json::json!({
        "levels": levels,
        "spectra": st.engine.stats().spectra,
    }))
}

#[derive(Deserialize)]
pub struct LevelsQuery {
    /// 推送间隔（ms，钳制 20..=1000，默认 100）
    #[serde(default)]
    pub interval_ms: Option<u64>,
}

pub async fn levels_stream(
    State(st): State<ApiState>,
    Query(q): Query<LevelsQuery>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let period = Duration::from_millis(q.interval_ms.unwrap_or(100).clamp(20, 1000));
    // unfold 每轮先睡再取快照：客户端连上就能立刻收到第一帧
    let stream = futures::stream::unfold(st, move |st| async move {
        tokio::time::sleep(period).await;
        let event = Event::default().event("levels").data(levels_payload(&st));
        Some((Ok(event), st))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub async fn events(
    State(st): State<ApiState>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::StreamExt;
    let rx = st.engine.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(ev) => {
            let name = match &ev {
                EngineEvent::GraphApplied => "graph_applied",
                EngineEvent::DevicesChanged => "devices_changed",
                EngineEvent::Underrun { .. } => "underrun",
            };
            Some(Ok(Event::default()
                .event(name)
                .data(serde_json::to_string(&ev).unwrap_or_default())))
        }
        Err(_) => None, // lagged：跳过错过的旧事件
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
