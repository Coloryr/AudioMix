//! 本地控制 API：REST + SSE，与 Tauri commands 共享同一个 `Engine`。
//!
//! 用于第三方集成、脚本控制与远程面板；headless（无 GUI）模式同样可用。
//!
//! - 只依赖引擎的能力（设备/混音图/电平/统计/延迟测量/事件流）开箱即用；
//! - 配置读写、USB/IP 虚拟声卡、系统默认设备、日志等 App 侧能力经
//!   [`ApiHost`] 注入，未注入时对应端点返回 `501 unsupported`；
//! - 鉴权：设置了令牌后 `/api/*` 需带 `Authorization: Bearer <token>` /
//!   `X-Api-Token: <token>` / `?token=<token>`（SSE 用 query，因为 EventSource
//!   不能自定义请求头）；`/` 与 `/api`（端点索引）始终公开，便于客户端自描述。

mod devices;
mod error;
mod ext;
mod graph;
mod host;
mod streams;

use std::sync::Arc;
use std::time::Instant;

use audiomix_core::{Engine, GraphConfig};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use parking_lot::Mutex;
use tokio::sync::watch;

pub use error::{ApiError, ApiResult};
pub use host::{ApiHost, NoHost};

/// 请求体上限（混音图最大也就几十 KB）
const MAX_BODY_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub(crate) struct ApiState {
    pub(crate) engine: Arc<Engine>,
    pub(crate) host: Arc<dyn ApiHost>,
    /// 非空 = 启用鉴权
    pub(crate) token: Option<String>,
    pub(crate) started: Instant,
    /// 图编辑串行锁：多客户端同时增删节点时保证「读-改-写」原子性
    pub(crate) edit_lock: Arc<Mutex<()>>,
}

/// 启动参数
pub struct ApiOptions {
    pub bind: String,
    pub port: u16,
    /// 访问令牌；空/None = 不鉴权
    pub token: Option<String>,
    /// 允许浏览器跨域调用（默认关闭，见 `ControlApiSettings::cors`）
    pub cors: bool,
    /// App 侧扩展能力
    pub host: Arc<dyn ApiHost>,
}

impl Default for ApiOptions {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".into(),
            port: 17643,
            token: None,
            cors: false,
            host: Arc::new(NoHost),
        }
    }
}

impl ApiOptions {
    pub fn new(bind: impl Into<String>, port: u16) -> Self {
        Self {
            bind: bind.into(),
            port,
            ..Default::default()
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        self.token = if token.trim().is_empty() {
            None
        } else {
            Some(token.trim().to_string())
        };
        self
    }

    pub fn with_cors(mut self, cors: bool) -> Self {
        self.cors = cors;
        self
    }

    pub fn with_host(mut self, host: Arc<dyn ApiHost>) -> Self {
        self.host = host;
        self
    }
}

/// 运行中的 API 服务句柄；Drop 时自动停止
pub struct ApiServer {
    shutdown: watch::Sender<bool>,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub addr: std::net::SocketAddr,
    /// 是否启用了令牌鉴权（界面提示用）
    pub auth_enabled: bool,
}

impl ApiServer {
    /// 请求停止（不等收尾，交给 runtime）
    pub fn stop(&self) {
        let _ = self.shutdown.send(true);
        if let Some(h) = self.handle.lock().take() {
            let _ = h;
        }
    }

    /// 基址，如 `http://127.0.0.1:17643`
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动控制 API（需要在 tokio runtime 内调用）
pub async fn spawn(engine: Arc<Engine>, opts: ApiOptions) -> Result<ApiServer, String> {
    let addr: std::net::SocketAddr = format!("{}:{}", opts.bind, opts.port)
        .parse()
        .map_err(|e| format!("无效的监听地址: {e}"))?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let state = ApiState {
        engine,
        host: opts.host,
        token: opts.token.clone(),
        started: Instant::now(),
        edit_lock: Arc::new(Mutex::new(())),
    };

    let api = Router::new()
        // —— 自描述 ——
        .route("/", get(index))
        .route("/api", get(index))
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/levels", get(streams::levels_snapshot))
        // —— 设备 ——
        .route("/api/devices", get(devices::list))
        .route("/api/devices/refresh", post(devices::refresh))
        .route("/api/devices/default", post(devices::set_default))
        .route(
            "/api/devices/volume",
            get(devices::get_volume).put(devices::set_volume),
        )
        .route(
            "/api/devices/mute",
            get(devices::get_mute).put(devices::set_mute),
        )
        // —— 混音图 ——
        .route("/api/graph", get(graph::get_graph).put(graph::put_graph))
        .route("/api/sources", get(graph::list_sources).post(graph::add_source))
        .route(
            "/api/sources/{id}",
            get(graph::get_source)
                .patch(graph::patch_source)
                .delete(graph::delete_source),
        )
        .route("/api/sinks", get(graph::list_sinks).post(graph::add_sink))
        .route(
            "/api/sinks/{id}",
            get(graph::get_sink)
                .patch(graph::patch_sink)
                .delete(graph::delete_sink),
        )
        .route("/api/routes", get(graph::list_routes).post(graph::add_route))
        .route(
            "/api/routes/{id}",
            get(graph::get_route)
                .patch(graph::patch_route)
                .delete(graph::delete_route),
        )
        .route(
            "/api/processors",
            get(graph::list_processors).post(graph::add_processor),
        )
        .route(
            "/api/processors/{id}",
            get(graph::get_processor)
                .put(graph::put_processor)
                .delete(graph::delete_processor),
        )
        // —— 测量 ——
        .route("/api/latency", post(graph::measure_latency))
        // —— 事件流 ——
        .route("/api/events", get(streams::events))
        .route("/api/stream/levels", get(streams::levels_stream))
        // —— App 侧能力（未注入 → 501）——
        .route(
            "/api/settings",
            get(ext::get_settings).patch(ext::patch_settings).put(ext::put_settings),
        )
        .route("/api/usbip", get(ext::usbip_status))
        .route("/api/usbip/cables", put(ext::usbip_set_cables))
        .route("/api/usbip/attach", post(ext::usbip_attach))
        .route("/api/usbip/detach", post(ext::usbip_detach))
        .route("/api/usbip/driver", post(ext::usbip_install_driver))
        .route("/api/logs", get(ext::logs))
        .fallback(not_found);

    let cors = opts.cors;
    let app = api
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        // 认证在内层（先于它执行的是 CORS，预检不带 Authorization）
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(middleware::from_fn_with_state(cors, cors_layer))
        .with_state(state);

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

    tracing::info!(
        "控制 API 已启动: http://{actual}{}",
        if opts.token.is_some() { "（已启用令牌鉴权）" } else { "" }
    );
    Ok(ApiServer {
        shutdown: shutdown_tx,
        handle: Mutex::new(Some(handle)),
        addr: actual,
        auth_enabled: opts.token.is_some(),
    })
}

/// 把宿主钩子（阻塞语义）丢到阻塞线程执行
pub(crate) async fn run_blocking<T, F>(host: Arc<dyn ApiHost>, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&dyn ApiHost) -> ApiResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(host.as_ref()))
        .await
        .map_err(|e| ApiError::internal(format!("后台任务失败: {e}")))?
}

/// 图编辑的「读-改-写」临界区：持锁期间独占，避免多客户端互相覆盖
pub(crate) fn edit_graph<T>(
    st: &ApiState,
    f: impl FnOnce(&mut GraphConfig) -> ApiResult<T>,
) -> ApiResult<T> {
    let _guard = st.edit_lock.lock();
    let mut cfg = st.engine.get_graph();
    let out = f(&mut cfg)?;
    st.engine.apply_graph(cfg.clone())?;
    st.host.graph_changed();
    Ok(out)
}

// ---------- 中间件 ----------

/// CORS：默认关闭（防止任意网页操控本机混音器）。开启时允许任意来源与常用方法/头。
async fn cors_layer(
    State(enabled): State<bool>,
    req: Request,
    next: Next,
) -> Response {
    if !enabled {
        return next.run(req).await;
    }
    let preflight = req.method() == Method::OPTIONS;
    let mut resp = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let h = resp.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type, x-api-token"),
    );
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    resp
}

/// 令牌鉴权：`/` 与 `/api` 索引公开，其余 `/api/*` 都要令牌
async fn auth(State(st): State<ApiState>, req: Request, next: Next) -> Response {
    let Some(expected) = st.token.as_deref() else {
        return next.run(req).await; // 未设令牌 = 不鉴权
    };
    let path = req.uri().path();
    if path == "/" || path == "/api" {
        return next.run(req).await;
    }
    for candidate in token_candidates(&req) {
        if constant_time_eq(expected.as_bytes(), candidate.as_bytes()) {
            return next.run(req).await;
        }
    }
    ApiError::unauthorized(
        "缺少或错误的访问令牌（`Authorization: Bearer <token>` / `X-Api-Token` / `?token=`）",
    )
    .into_response()
}

fn token_candidates(req: &Request) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(v) = req.headers().get(header::AUTHORIZATION) {
        if let Ok(s) = v.to_str() {
            let s = s.trim();
            let t = s
                .strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
                .unwrap_or(s);
            out.push(t.trim().to_string());
        }
    }
    if let Some(v) = req.headers().get("x-api-token") {
        if let Ok(s) = v.to_str() {
            out.push(s.trim().to_string());
        }
    }
    if let Some(q) = req.uri().query() {
        for pair in q.split('&') {
            if let Some(v) = pair.strip_prefix("token=") {
                out.push(percent_decode(v));
            }
        }
    }
    out
}

/// 令牌比较：长度不同也走满循环，避免用响应时间试探
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------- 基础端点 ----------

async fn not_found(req: Request) -> Response {
    ApiError::not_found(format!(
        "未知端点 {} {}（可用端点见 GET /api）",
        req.method(),
        req.uri().path()
    ))
    .into_response()
}

/// 端点索引：客户端自描述，也是「这个版本有哪些能力」的权威列表
const ENDPOINTS: &[(&str, &str, &str)] = &[
    ("GET", "/api", "端点索引 + 版本 + 鉴权状态"),
    ("GET", "/api/health", "健康检查（status/version/uptime/backend）"),
    ("GET", "/api/status", "后端名 + 各节点电平 + 欠载/丢帧统计"),
    ("GET", "/api/levels", "各节点电平 + 频段频谱（快照）"),
    ("GET", "/api/stream/levels", "SSE：电平/频谱流（?interval_ms=，20..1000）"),
    ("GET", "/api/events", "SSE：graph_applied / devices_changed / underrun"),
    ("GET", "/api/devices", "设备列表（缓存，不重新枚举）"),
    ("POST", "/api/devices/refresh", "重新枚举设备并返回列表"),
    ("POST", "/api/devices/default", "设为系统默认设备 {device_id}"),
    ("GET", "/api/devices/volume?device_id=", "读系统端点音量"),
    ("PUT", "/api/devices/volume?device_id=", "写系统端点音量 {level}"),
    ("GET", "/api/devices/mute?device_id=", "读系统端点静音"),
    ("PUT", "/api/devices/mute?device_id=", "写系统端点静音 {mute}"),
    ("GET", "/api/graph", "整个混音图"),
    ("PUT", "/api/graph", "整体替换混音图（body 为 GraphConfig）"),
    ("GET", "/api/sources", "全部 source"),
    ("POST", "/api/sources", "加 source {name, device_id, mode, enabled?}"),
    ("GET", "/api/sources/{id}", "单个 source"),
    ("PATCH", "/api/sources/{id}", "改 source {name?, enabled?}"),
    ("DELETE", "/api/sources/{id}", "删 source（连带其路由）"),
    ("GET", "/api/sinks", "全部 sink"),
    ("POST", "/api/sinks", "加 sink {name, device_id, volume?, enabled?}"),
    ("GET", "/api/sinks/{id}", "单个 sink"),
    ("PATCH", "/api/sinks/{id}", "改 sink {name?, volume?, enabled?}"),
    ("DELETE", "/api/sinks/{id}", "删 sink（连带其路由）"),
    ("GET", "/api/routes", "全部路由"),
    ("POST", "/api/routes", "加路由 {source_id, sink_id, gain?}"),
    ("GET", "/api/routes/{id}", "单条路由"),
    ("PATCH", "/api/routes/{id}", "改路由 {gain?, muted?}"),
    ("DELETE", "/api/routes/{id}", "删路由"),
    ("GET", "/api/processors", "全部 DSP 方块"),
    ("POST", "/api/processors", "加 DSP 方块（body 即 DspNode，如 {\"type\":\"gain\",\"db\":-6}）"),
    ("GET", "/api/processors/{id}", "单个 DSP 方块"),
    ("PUT", "/api/processors/{id}", "替换 DSP 方块参数（body 即 DspNode）"),
    ("DELETE", "/api/processors/{id}", "删 DSP 方块（连带其路由）"),
    ("POST", "/api/latency", "实测延迟 {route_id} 或 {source_id, sink_id} → {ms}"),
    ("GET", "/api/settings", "应用设置（需宿主支持）"),
    ("PATCH", "/api/settings", "局部更新设置（递归合并，需宿主支持）"),
    ("PUT", "/api/settings", "整体替换设置（需宿主支持）"),
    ("GET", "/api/usbip", "虚拟声卡状态（服务器/线缆/驱动/端口，需宿主支持）"),
    ("PUT", "/api/usbip/cables", "保存线缆并（重）启服务器 {enabled?, cables:[...]}"),
    ("POST", "/api/usbip/attach", "附加全部线缆（需管理员）"),
    ("POST", "/api/usbip/detach", "断开全部线缆"),
    ("POST", "/api/usbip/driver", "安装随包 usbip-win2 驱动（需管理员）"),
    ("GET", "/api/logs?since=&limit=", "增量取运行日志（需宿主支持）"),
];

async fn index(State(st): State<ApiState>) -> Json<serde_json::Value> {
    let endpoints: Vec<serde_json::Value> = ENDPOINTS
        .iter()
        .map(|(m, p, d)| serde_json::json!({ "method": m, "path": p, "description": d }))
        .collect();
    Json(serde_json::json!({
        "app": st.host.app_name(),
        "version": st.host.app_version(),
        "api_version": 1,
        "auth_required": st.token.is_some(),
        "auth": if st.token.is_some() {
            "Authorization: Bearer <token> / X-Api-Token / ?token="
        } else {
            "未启用（设置里可设令牌）"
        },
        "events": ["graph_applied", "devices_changed", "underrun"],
        "endpoints": endpoints,
    }))
}

async fn health(State(st): State<ApiState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "app": st.host.app_name(),
        "version": st.host.app_version(),
        "backend": st.engine.backend_name(),
        "uptime_ms": st.started.elapsed().as_millis() as u64,
    }))
}

async fn status(State(st): State<ApiState>) -> Json<serde_json::Value> {
    let levels: std::collections::HashMap<String, f32> = st.engine.levels().into_iter().collect();
    Json(serde_json::json!({
        "backend": st.engine.backend_name(),
        "levels": levels,
        "stats": st.engine.stats(),
    }))
}
