//! 混音图端点：整图读写 + 节点级增删改 + 延迟实测。
//!
//! 节点级接口都是「读当前图 → 改 → 整体应用」，全程持 `edit_lock`，
//! 因此多客户端并发增删不会互相覆盖（界面侧保存整图时仍是后写胜出，
//! 所以 API 改动后会通过宿主钩子通知界面重新拉取）。

use audiomix_core::engine::{make_processor, make_route, make_sink, make_source};
use audiomix_core::model::{DspNode, GraphConfig, Route, Sink, Source, SourceMode};
use axum::extract::{Json, Path, State};
use serde::Deserialize;

use crate::error::{body, ApiError, ApiResult};
use crate::{edit_graph, ApiState};

// ---------- 整图 ----------

pub async fn get_graph(State(st): State<ApiState>) -> Json<GraphConfig> {
    Json(st.engine.get_graph())
}

/// 整体替换混音图。返回应用后的图（引擎可能跳过当前不存在的设备）。
pub async fn put_graph(
    State(st): State<ApiState>,
    payload: Result<Json<GraphConfig>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<GraphConfig>> {
    let graph = body(payload)?;
    let _guard = st.edit_lock.lock();
    st.engine.apply_graph(graph)?;
    st.host.graph_changed();
    Ok(Json(st.engine.get_graph()))
}

// ---------- source ----------

pub async fn list_sources(State(st): State<ApiState>) -> Json<Vec<Source>> {
    Json(st.engine.get_graph().sources)
}

pub async fn get_source(State(st): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Source>> {
    st.engine
        .get_graph()
        .sources
        .into_iter()
        .find(|s| s.id == id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("source {id} 不存在")))
}

#[derive(Deserialize)]
pub struct NewSource {
    /// 不传则自动生成
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub device_id: String,
    /// `device_input`（默认）| `loopback`
    #[serde(default)]
    pub mode: Option<SourceMode>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

pub async fn add_source(
    State(st): State<ApiState>,
    payload: Result<Json<NewSource>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<(axum::http::StatusCode, Json<Source>)> {
    let req = body(payload)?;
    let mode = req.mode.unwrap_or(SourceMode::DeviceInput);
    let name = req.name.unwrap_or_else(|| req.device_id.clone());
    let mut src = make_source(&req.device_id, &name, mode);
    if let Some(id) = req.id {
        src.id = id;
    }
    if let Some(enabled) = req.enabled {
        src.enabled = enabled;
    }
    let created = src.clone();
    let id = src.id.clone();
    edit_graph(&st, move |cfg| {
        if cfg.sources.iter().any(|s| s.id == src.id) {
            return Err(ApiError::conflict(format!("source {} 已存在", src.id)));
        }
        cfg.sources.push(src);
        Ok(())
    })?;
    tracing::info!("控制 API 添加 source {id}（{}）", created.device_id);
    Ok((axum::http::StatusCode::CREATED, Json(created)))
}

#[derive(Deserialize)]
pub struct PatchSource {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub mode: Option<SourceMode>,
}

pub async fn patch_source(
    State(st): State<ApiState>,
    Path(id): Path<String>,
    payload: Result<Json<PatchSource>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<Source>> {
    let req = body(payload)?;
    let target = id.clone();
    let updated = edit_graph(&st, move |cfg| {
        let src = cfg
            .sources
            .iter_mut()
            .find(|s| s.id == target)
            .ok_or_else(|| ApiError::not_found(format!("source {target} 不存在")))?;
        if let Some(name) = req.name {
            src.name = name;
        }
        if let Some(enabled) = req.enabled {
            src.enabled = enabled;
        }
        if let Some(mode) = req.mode {
            src.mode = mode;
        }
        Ok(src.clone())
    })?;
    Ok(Json(updated))
}

pub async fn delete_source(
    State(st): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = id.clone();
    let removed_routes = edit_graph(&st, move |cfg| {
        let before = cfg.sources.len();
        cfg.sources.retain(|s| s.id != target);
        if cfg.sources.len() == before {
            return Err(ApiError::not_found(format!("source {target} 不存在")));
        }
        let before = cfg.routes.len();
        cfg.routes
            .retain(|r| r.source_id != target && r.sink_id != target);
        Ok(before - cfg.routes.len())
    })?;
    tracing::info!("控制 API 删除 source {id}（连带 {removed_routes} 条路由）");
    Ok(Json(
        serde_json::json!({ "removed": id, "removed_routes": removed_routes }),
    ))
}

// ---------- sink ----------

pub async fn list_sinks(State(st): State<ApiState>) -> Json<Vec<Sink>> {
    Json(st.engine.get_graph().sinks)
}

pub async fn get_sink(State(st): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Sink>> {
    st.engine
        .get_graph()
        .sinks
        .into_iter()
        .find(|s| s.id == id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("sink {id} 不存在")))
}

#[derive(Deserialize)]
pub struct NewSink {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub device_id: String,
    #[serde(default)]
    pub volume: Option<f32>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

pub async fn add_sink(
    State(st): State<ApiState>,
    payload: Result<Json<NewSink>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<(axum::http::StatusCode, Json<Sink>)> {
    let req = body(payload)?;
    let name = req.name.unwrap_or_else(|| req.device_id.clone());
    let mut sink = make_sink(&req.device_id, &name);
    if let Some(id) = req.id {
        sink.id = id;
    }
    if let Some(volume) = req.volume {
        sink.volume = volume.clamp(0.0, 1.0);
    }
    if let Some(enabled) = req.enabled {
        sink.enabled = enabled;
    }
    let created = sink.clone();
    let id = sink.id.clone();
    edit_graph(&st, move |cfg| {
        if cfg.sinks.iter().any(|s| s.id == sink.id) {
            return Err(ApiError::conflict(format!("sink {} 已存在", sink.id)));
        }
        cfg.sinks.push(sink);
        Ok(())
    })?;
    tracing::info!("控制 API 添加 sink {id}（{}）", created.device_id);
    Ok((axum::http::StatusCode::CREATED, Json(created)))
}

#[derive(Deserialize)]
pub struct PatchSink {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub volume: Option<f32>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

pub async fn patch_sink(
    State(st): State<ApiState>,
    Path(id): Path<String>,
    payload: Result<Json<PatchSink>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<Sink>> {
    let req = body(payload)?;
    let target = id.clone();
    let updated = edit_graph(&st, move |cfg| {
        let sink = cfg
            .sinks
            .iter_mut()
            .find(|s| s.id == target)
            .ok_or_else(|| ApiError::not_found(format!("sink {target} 不存在")))?;
        if let Some(name) = req.name {
            sink.name = name;
        }
        if let Some(volume) = req.volume {
            sink.volume = volume.clamp(0.0, 1.0);
        }
        if let Some(enabled) = req.enabled {
            sink.enabled = enabled;
        }
        Ok(sink.clone())
    })?;
    Ok(Json(updated))
}

pub async fn delete_sink(
    State(st): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = id.clone();
    let removed_routes = edit_graph(&st, move |cfg| {
        let before = cfg.sinks.len();
        cfg.sinks.retain(|s| s.id != target);
        if cfg.sinks.len() == before {
            return Err(ApiError::not_found(format!("sink {target} 不存在")));
        }
        let before = cfg.routes.len();
        cfg.routes
            .retain(|r| r.source_id != target && r.sink_id != target);
        Ok(before - cfg.routes.len())
    })?;
    tracing::info!("控制 API 删除 sink {id}（连带 {removed_routes} 条路由）");
    Ok(Json(
        serde_json::json!({ "removed": id, "removed_routes": removed_routes }),
    ))
}

// ---------- route ----------

pub async fn list_routes(State(st): State<ApiState>) -> Json<Vec<Route>> {
    Json(st.engine.get_graph().routes)
}

pub async fn get_route(State(st): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<Route>> {
    st.engine
        .get_graph()
        .routes
        .into_iter()
        .find(|r| r.id == id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("route {id} 不存在")))
}

#[derive(Deserialize)]
pub struct NewRoute {
    #[serde(default)]
    pub id: Option<String>,
    pub source_id: String,
    pub sink_id: String,
    /// 线性增益（默认 1.0 = 0dB），钳制 0.0 – 4.0
    #[serde(default)]
    pub gain: Option<f32>,
}

pub async fn add_route(
    State(st): State<ApiState>,
    payload: Result<Json<NewRoute>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<(axum::http::StatusCode, Json<Route>)> {
    let req = body(payload)?;
    let mut route = make_route(&req.source_id, &req.sink_id);
    if let Some(id) = req.id {
        route.id = id;
    }
    if let Some(gain) = req.gain {
        route.gain = gain.clamp(0.0, 4.0);
    }
    let created = route.clone();
    let id = created.id.clone();
    edit_graph(&st, move |cfg| {
        // 端点既可以是 source/sink，也可以是 DSP 方块（画布上都是节点）
        let known = |node: &str| {
            cfg.sources.iter().any(|s| s.id == node)
                || cfg.sinks.iter().any(|s| s.id == node)
                || cfg.processors.iter().any(|p| p.id == node)
        };
        if !known(&route.source_id) {
            return Err(ApiError::not_found(format!(
                "起点 {} 不存在（source 或 processor）",
                route.source_id
            )));
        }
        if !known(&route.sink_id) {
            return Err(ApiError::not_found(format!(
                "终点 {} 不存在（sink 或 processor）",
                route.sink_id
            )));
        }
        if cfg
            .routes
            .iter()
            .any(|r| r.source_id == route.source_id && r.sink_id == route.sink_id)
        {
            return Err(ApiError::conflict(format!(
                "{} → {} 的连线已存在",
                route.source_id, route.sink_id
            )));
        }
        cfg.routes.push(route);
        Ok(())
    })?;
    tracing::info!("控制 API 添加路由 {id}");
    Ok((axum::http::StatusCode::CREATED, Json(created)))
}

#[derive(Deserialize)]
pub struct PatchRoute {
    #[serde(default)]
    pub gain: Option<f32>,
    #[serde(default)]
    pub muted: Option<bool>,
}

pub async fn patch_route(
    State(st): State<ApiState>,
    Path(id): Path<String>,
    payload: Result<Json<PatchRoute>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<Route>> {
    let req = body(payload)?;
    let target = id.clone();
    let updated = edit_graph(&st, move |cfg| {
        let route = cfg
            .routes
            .iter_mut()
            .find(|r| r.id == target)
            .ok_or_else(|| ApiError::not_found(format!("route {target} 不存在")))?;
        if let Some(gain) = req.gain {
            route.gain = gain.clamp(0.0, 4.0);
        }
        if let Some(muted) = req.muted {
            route.muted = muted;
        }
        Ok(route.clone())
    })?;
    Ok(Json(updated))
}

pub async fn delete_route(
    State(st): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = id.clone();
    edit_graph(&st, move |cfg| {
        let before = cfg.routes.len();
        cfg.routes.retain(|r| r.id != target);
        if cfg.routes.len() == before {
            return Err(ApiError::not_found(format!("route {target} 不存在")));
        }
        Ok(())
    })?;
    Ok(Json(serde_json::json!({ "removed": id })))
}

// ---------- processor（DSP 方块）----------

pub async fn list_processors(State(st): State<ApiState>) -> Json<Vec<audiomix_core::Processor>> {
    Json(st.engine.get_graph().processors)
}

pub async fn get_processor(
    State(st): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<audiomix_core::Processor>> {
    st.engine
        .get_graph()
        .processors
        .into_iter()
        .find(|p| p.id == id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("processor {id} 不存在")))
}

/// 加一个 DSP 方块：body 就是 DspNode（如 `{"type":"gain","db":-6}`），
/// 可以额外带 `id` 指定节点 id。
pub async fn add_processor(
    State(st): State<ApiState>,
    payload: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<(axum::http::StatusCode, Json<audiomix_core::Processor>)> {
    let mut value = body(payload)?;
    let id = value
        .as_object_mut()
        .and_then(|o| o.remove("id"))
        .and_then(|v| v.as_str().map(str::to_string));
    let mut node: DspNode = serde_json::from_value(value)
        .map_err(|e| ApiError::bad_request(format!("DSP 参数无效: {e}")))?;
    node.kind.clamp_params();
    let mut processor = make_processor(node);
    if let Some(id) = id {
        processor.id = id;
    }
    let created = processor.clone();
    let node_id = created.id.clone();
    edit_graph(&st, move |cfg| {
        if cfg.processors.iter().any(|p| p.id == processor.id) {
            return Err(ApiError::conflict(format!("processor {} 已存在", processor.id)));
        }
        cfg.processors.push(processor);
        Ok(())
    })?;
    tracing::info!("控制 API 添加 DSP 方块 {node_id}");
    Ok((axum::http::StatusCode::CREATED, Json(created)))
}

/// 改 DSP 方块参数（body 即完整的 DspNode，未给的参数回默认值）。
/// 只重发运行时快照，不重启流。
pub async fn put_processor(
    State(st): State<ApiState>,
    Path(id): Path<String>,
    payload: Result<Json<DspNode>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<audiomix_core::Processor>> {
    let mut node = body(payload)?;
    node.kind.clamp_params();
    let _guard = st.edit_lock.lock();
    let mut graph = st.engine.get_graph();
    let proc = graph
        .processors
        .iter_mut()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::not_found(format!("processor {id} 不存在")))?;
    proc.node = node;
    let updated = proc.clone();
    st.engine.apply_graph(graph)?;
    st.host.graph_changed();
    Ok(Json(updated))
}

pub async fn delete_processor(
    State(st): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = id.clone();
    let removed_routes = edit_graph(&st, move |cfg| {
        let before = cfg.processors.len();
        cfg.processors.retain(|p| p.id != target);
        if cfg.processors.len() == before {
            return Err(ApiError::not_found(format!("processor {target} 不存在")));
        }
        let before = cfg.routes.len();
        cfg.routes
            .retain(|r| r.source_id != target && r.sink_id != target);
        Ok(before - cfg.routes.len())
    })?;
    Ok(Json(
        serde_json::json!({ "removed": id, "removed_routes": removed_routes }),
    ))
}

// ---------- 延迟实测 ----------

#[derive(Deserialize)]
pub struct LatencyRequest {
    /// 测一条已有连线的路径延迟
    #[serde(default)]
    pub route_id: Option<String>,
    /// 或测两个节点之间（可跨线缆/跨 DSP）的整链延迟
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub sink_id: Option<String>,
}

/// 实测延迟（ms）：注入扫频脉冲 + 相关检测，阻塞约 2.5–4 秒
pub async fn measure_latency(
    State(st): State<ApiState>,
    payload: Result<Json<LatencyRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<serde_json::Value>> {
    let req = body(payload)?;
    let engine = st.engine.clone();
    let (ms, what) = match (req.route_id, req.source_id, req.sink_id) {
        (Some(route_id), _, _) => {
            let id = route_id.clone();
            let exists = engine.get_graph().routes.iter().any(|r| r.id == route_id);
            if !exists {
                return Err(ApiError::not_found(format!("route {route_id} 不存在")));
            }
            let ms = tokio::task::spawn_blocking(move || engine.measure_path_latency(&id))
                .await
                .map_err(|e| ApiError::internal(format!("测量任务失败: {e}")))??;
            (ms, serde_json::json!({ "route_id": route_id }))
        }
        (None, Some(source_id), Some(sink_id)) => {
            let (s, d) = (source_id.clone(), sink_id.clone());
            let exists = {
                let g = engine.get_graph();
                g.sinks.iter().any(|x| x.id == sink_id)
            };
            if !exists {
                return Err(ApiError::not_found(format!("sink {sink_id} 不存在")));
            }
            let ms = tokio::task::spawn_blocking(move || engine.measure_latency_between(&s, &d))
                .await
                .map_err(|e| ApiError::internal(format!("测量任务失败: {e}")))??;
            (
                ms,
                serde_json::json!({ "source_id": source_id, "sink_id": sink_id }),
            )
        }
        _ => {
            return Err(ApiError::bad_request(
                "需要 route_id，或同时给 source_id + sink_id",
            ))
        }
    };
    let mut out = what;
    out["ms"] = serde_json::json!(ms);
    Ok(Json(out))
}
