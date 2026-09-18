//! 控制 API 集成测试：在 port 0 上起真实 axum 服务，用裸 TCP 手写 HTTP/1.1
//! 请求验证各端点（不引入 reqwest，保持依赖精简）。

use std::sync::Arc;
use std::time::Duration;

use audiomix_control_api::{spawn, ApiHost, ApiOptions, ApiResult, ApiServer};
use audiomix_core::model::{GraphConfig, Route, Sink, Source, SourceMode};
use audiomix_core::testing::FakeBackend;
use audiomix_core::Engine;
use serde_json::{json, Value};

/// 发送 HTTP 请求并读取完整响应文本。
/// 不主动 shutdown 写端：hyper 会把客户端半关闭当作连接中止；
/// 请求带 `Connection: close`，服务端响应后关闭连接，read_to_end 到 EOF。
async fn request_raw(addr: std::net::SocketAddr, req: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let mut chunk = [0u8; 8192];
        let n = match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n == 0 => break,
            Ok(Ok(n)) => n,
            _ => break,
        };
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// 简化请求：`extra` 是附加请求头（如鉴权）
async fn req(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
    extra: &str,
) -> String {
    let (ctype, payload) = match body {
        Some(b) => ("Content-Type: application/json\r\n", b),
        None => ("", ""),
    };
    request_raw(
        addr,
        &format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{ctype}{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        ),
    )
    .await
}

async fn get(addr: std::net::SocketAddr, path: &str) -> String {
    req(addr, "GET", path, None, "").await
}

/// 取响应体（去掉头部）并解析 JSON
fn json_body(resp: &str) -> Value {
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    serde_json::from_str(body).unwrap_or_else(|e| panic!("响应不是 JSON（{e}）: {resp}"))
}

fn status_of(resp: &str) -> u16 {
    resp.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// 测试用宿主：把设置/USB/IP/日志换成内存实现，验证端点接线
struct TestHost {
    settings: parking_lot::Mutex<Value>,
}

impl TestHost {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            settings: parking_lot::Mutex::new(json!({
                "control_api": { "enabled": true, "bind": "127.0.0.1", "port": 17643 },
                "edge_buffer_ms": 250,
            })),
        })
    }
}

impl ApiHost for TestHost {
    fn app_name(&self) -> &'static str {
        "audiomix-test"
    }

    fn settings(&self) -> ApiResult<Value> {
        Ok(self.settings.lock().clone())
    }

    fn update_settings(&self, patch: Value) -> ApiResult<Value> {
        let mut cur = self.settings.lock();
        merge(&mut cur, patch);
        Ok(cur.clone())
    }

    fn replace_settings(&self, settings: Value) -> ApiResult<Value> {
        *self.settings.lock() = settings;
        Ok(self.settings.lock().clone())
    }

    fn usbip_status(&self) -> ApiResult<Value> {
        Ok(json!({ "enabled": true, "running": true, "cables": [] }))
    }

    fn logs(&self, since: u64, limit: usize) -> ApiResult<Value> {
        Ok(json!({ "lines": [{ "seq": since + 1, "text": "hello" }], "next": since + 1, "limit": limit }))
    }
}

/// 递归合并（对象逐字段、其余整体替换）——与 App 侧实现同语义
fn merge(base: &mut Value, patch: Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                merge(b.entry(k).or_insert(Value::Null), v);
            }
        }
        (b, p) => *b = p,
    }
}

fn graph_one_route(gain: f32) -> GraphConfig {
    GraphConfig {
        sources: vec![Source {
            id: "src".into(),
            name: "mic".into(),
            device_id: "dev-in-1".into(),
            mode: SourceMode::DeviceInput,
            enabled: true,
        }],
        sinks: vec![Sink {
            id: "out".into(),
            name: "spk".into(),
            device_id: "dev-out-1".into(),
            volume: 1.0,
            enabled: true,
        }],
        routes: vec![Route {
            id: "r".into(),
            source_id: "src".into(),
            sink_id: "out".into(),
            gain,
            muted: false,
            nodes: Vec::new(),
        }],
        processors: Vec::new(),
    }
}

async fn start_with(opts: ApiOptions) -> (Arc<Engine>, ApiServer) {
    let backend = FakeBackend::new();
    let engine = Engine::new(backend).unwrap();
    let server = spawn(engine.clone(), opts).await.unwrap();
    (engine, server)
}

/// 默认：随机端口，不鉴权，无宿主扩展
async fn start_server() -> (Arc<Engine>, ApiServer) {
    start_with(ApiOptions::new("127.0.0.1", 0)).await
}

#[tokio::test]
async fn health_and_index() {
    let (_engine, server) = start_server().await;
    let resp = get(server.addr, "/api/health").await;
    assert_eq!(status_of(&resp), 200, "响应: {resp}");
    let body = json_body(&resp);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["backend"], "fake");
    assert!(body["uptime_ms"].is_number());

    // 端点索引自描述
    let body = json_body(&get(server.addr, "/api").await);
    assert_eq!(body["api_version"], 1);
    assert_eq!(body["auth_required"], false);
    let paths: Vec<String> = body["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect();
    for p in ["/api/graph", "/api/sources", "/api/events", "/api/settings"] {
        assert!(paths.iter().any(|x| x.starts_with(p)), "索引应含 {p}");
    }
    // `/` 同样返回索引
    assert_eq!(status_of(&get(server.addr, "/").await), 200);
}

#[tokio::test]
async fn devices_returns_fake_list() {
    let (_engine, server) = start_server().await;
    let resp = get(server.addr, "/api/devices").await;
    assert_eq!(status_of(&resp), 200);
    assert!(resp.contains("dev-in-1"), "应含默认输入设备: {resp}");
    assert!(resp.contains("Fake Microphone"));
    // 重新枚举
    let resp = req(server.addr, "POST", "/api/devices/refresh", None, "").await;
    assert_eq!(status_of(&resp), 200);
    assert!(resp.contains("dev-out-1"));
}

#[tokio::test]
async fn graph_roundtrip_and_validation() {
    let (_engine, server) = start_server().await;

    // 初始为空图
    let body = json_body(&get(server.addr, "/api/graph").await);
    assert_eq!(body["sources"].as_array().unwrap().len(), 0);

    // PUT 合法图 → 回读一致
    let body = serde_json::to_string(&graph_one_route(0.5)).unwrap();
    let resp = req(server.addr, "PUT", "/api/graph", Some(&body), "").await;
    assert_eq!(status_of(&resp), 200);
    assert_eq!(json_body(&resp)["routes"][0]["gain"], 0.5);
    let resp = get(server.addr, "/api/graph").await;
    assert!(resp.contains("\"gain\":0.5"), "gain 应已生效: {resp}");
    assert!(resp.contains("\"id\":\"src\""));

    // PUT 悬空引用的图 → 400 + 结构化错误
    let bad = r#"{"sources":[],"sinks":[],"routes":[{"id":"r","source_id":"ghost","sink_id":"out","gain":1.0,"muted":false}]}"#;
    let resp = req(server.addr, "PUT", "/api/graph", Some(bad), "").await;
    assert_eq!(status_of(&resp), 400, "悬空 route 应 400: {resp}");
    assert_eq!(json_body(&resp)["error"]["kind"], "bad_request");
}

#[tokio::test]
async fn graph_apply_starts_streams() {
    let (_engine, server) = start_server().await;
    let body = serde_json::to_string(&graph_one_route(1.0)).unwrap();
    assert_eq!(
        status_of(&req(server.addr, "PUT", "/api/graph", Some(&body), "").await),
        200
    );

    let body = json_body(&get(server.addr, "/api/status").await);
    assert_eq!(body["backend"], "fake");
    assert!(body["levels"].is_object(), "status 应含电平表");
    assert!(body["stats"].is_object(), "status 应含统计");
    // levels 快照端点
    assert!(json_body(&get(server.addr, "/api/levels").await)["levels"].is_object());
}

#[tokio::test]
async fn node_crud_over_rest() {
    let (_engine, server) = start_server().await;

    // 加 source / sink
    let resp = req(
        server.addr,
        "POST",
        "/api/sources",
        Some(r#"{"device_id":"dev-in-1","name":"mic","mode":"deviceinput"}"#),
        "",
    )
    .await;
    assert_eq!(status_of(&resp), 201, "{resp}");
    let src_id = json_body(&resp)["id"].as_str().unwrap().to_string();

    let resp = req(
        server.addr,
        "POST",
        "/api/sinks",
        Some(r#"{"device_id":"dev-out-1","name":"spk"}"#),
        "",
    )
    .await;
    assert_eq!(status_of(&resp), 201);
    let sink_id = json_body(&resp)["id"].as_str().unwrap().to_string();

    // 加路由：起点不存在 → 404
    let resp = req(
        server.addr,
        "POST",
        "/api/routes",
        Some(r#"{"source_id":"ghost","sink_id":"x"}"#),
        "",
    )
    .await;
    assert_eq!(status_of(&resp), 404, "{resp}");
    assert_eq!(json_body(&resp)["error"]["kind"], "not_found");

    let payload = format!(r#"{{"source_id":"{src_id}","sink_id":"{sink_id}","gain":0.25}}"#);
    let resp = req(server.addr, "POST", "/api/routes", Some(&payload), "").await;
    assert_eq!(status_of(&resp), 201, "{resp}");
    let route_id = json_body(&resp)["id"].as_str().unwrap().to_string();
    assert_eq!(json_body(&resp)["gain"], 0.25);

    // 同一条连线再来一次 → 409
    let resp = req(server.addr, "POST", "/api/routes", Some(&payload), "").await;
    assert_eq!(status_of(&resp), 409, "{resp}");

    // PATCH 路由增益（超范围被钳制到 4.0）
    let resp = req(
        server.addr,
        "PATCH",
        &format!("/api/routes/{route_id}"),
        Some(r#"{"gain":9.0,"muted":true}"#),
        "",
    )
    .await;
    assert_eq!(status_of(&resp), 200, "{resp}");
    let body = json_body(&resp);
    assert_eq!(body["gain"], 4.0);
    assert_eq!(body["muted"], true);

    // PATCH sink 音量
    let resp = req(
        server.addr,
        "PATCH",
        &format!("/api/sinks/{sink_id}"),
        Some(r#"{"volume":0.5}"#),
        "",
    )
    .await;
    assert_eq!(json_body(&resp)["volume"], 0.5);

    // 删 source → 连带删掉它的路由
    let resp = req(
        server.addr,
        "DELETE",
        &format!("/api/sources/{src_id}"),
        None,
        "",
    )
    .await;
    assert_eq!(status_of(&resp), 200);
    assert_eq!(json_body(&resp)["removed_routes"], 1);

    let routes = json_body(&get(server.addr, "/api/routes").await);
    assert_eq!(routes.as_array().unwrap().len(), 0, "路由应被连带删除");

    // 再删一次 → 404
    let resp = req(
        server.addr,
        "DELETE",
        &format!("/api/sources/{src_id}"),
        None,
        "",
    )
    .await;
    assert_eq!(status_of(&resp), 404);
}

#[tokio::test]
async fn processors_lifecycle() {
    let (_engine, server) = start_server().await;

    let resp = req(
        server.addr,
        "POST",
        "/api/processors",
        Some(r#"{"id":"dsp-1","type":"gain","db":100.0}"#),
        "",
    )
    .await;
    assert_eq!(status_of(&resp), 201, "{resp}");
    let body = json_body(&resp);
    assert_eq!(body["id"], "dsp-1");
    assert_eq!(body["type"], "gain");
    assert_eq!(body["db"], 12.0, "参数应被钳制到合法范围");

    // 重复 id → 409
    let resp = req(
        server.addr,
        "POST",
        "/api/processors",
        Some(r#"{"id":"dsp-1","type":"switch"}"#),
        "",
    )
    .await;
    assert_eq!(status_of(&resp), 409);

    // 非法参数（缺 db）→ 400
    let resp = req(
        server.addr,
        "POST",
        "/api/processors",
        Some(r#"{"type":"gain"}"#),
        "",
    )
    .await;
    assert_eq!(status_of(&resp), 400, "{resp}");

    // PUT 换参数为延迟节点
    let resp = req(
        server.addr,
        "PUT",
        "/api/processors/dsp-1",
        Some(r#"{"type":"delay","ms":5000.0,"enabled":false}"#),
        "",
    )
    .await;
    assert_eq!(status_of(&resp), 200, "{resp}");
    let body = json_body(&resp);
    assert_eq!(body["type"], "delay");
    assert_eq!(body["ms"], 1000.0, "延迟应钳制到 1000ms 上限");
    assert_eq!(body["enabled"], false);

    // 列表 + 删除
    assert_eq!(
        json_body(&get(server.addr, "/api/processors").await)
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        status_of(&req(server.addr, "DELETE", "/api/processors/dsp-1", None, "").await),
        200
    );
    assert_eq!(
        status_of(&req(server.addr, "DELETE", "/api/processors/dsp-1", None, "").await),
        404
    );
}

#[tokio::test]
async fn latency_requests_validate_input() {
    let (_engine, server) = start_server().await;
    // 缺参数 → 400
    let resp = req(server.addr, "POST", "/api/latency", Some("{}"), "").await;
    assert_eq!(status_of(&resp), 400);
    // 路由不存在 → 404
    let resp = req(
        server.addr,
        "POST",
        "/api/latency",
        Some(r#"{"route_id":"nope"}"#),
        "",
    )
    .await;
    assert_eq!(status_of(&resp), 404);
}

#[tokio::test]
async fn device_volume_requires_query_param() {
    let (_engine, server) = start_server().await;
    // 缺 device_id → 我们的 400（不是框架的纯文本拒绝）
    let resp = get(server.addr, "/api/devices/volume").await;
    assert_eq!(status_of(&resp), 400, "{resp}");
    let body = json_body(&resp);
    assert_eq!(body["error"]["kind"], "bad_request");
    assert!(body["error"]["message"].as_str().unwrap().contains("device_id"));

    // 设备 id 里有 `/` 的合成 id 走 query 参数不丢字符 → 到宿主这层（无宿主 = 501）
    let resp = get(server.addr, "/api/devices/volume?device_id=usbip%3A%2F%2F1%2Fplayback").await;
    assert_eq!(status_of(&resp), 501, "{resp}");

    // 音量越界 → 400
    let resp = req(
        server.addr,
        "PUT",
        "/api/devices/volume?device_id=dev-out-1",
        Some(r#"{"level":2.0}"#),
        "",
    )
    .await;
    assert_eq!(status_of(&resp), 400, "{resp}");
}

#[tokio::test]
async fn unknown_route_returns_structured_404() {
    let (_engine, server) = start_server().await;
    let resp = get(server.addr, "/api/nope").await;
    assert_eq!(status_of(&resp), 404, "未知路径应 404: {resp}");
    let body = json_body(&resp);
    assert_eq!(body["error"]["kind"], "not_found");
    assert!(body["error"]["message"].as_str().unwrap().contains("/api"));
}

#[tokio::test]
async fn token_auth_gates_api() {
    let (_engine, server) = start_with(
        ApiOptions::new("127.0.0.1", 0).with_token("s3cret"),
    )
    .await;

    // 无令牌 → 401
    let resp = get(server.addr, "/api/graph").await;
    assert_eq!(status_of(&resp), 401, "{resp}");
    assert_eq!(json_body(&resp)["error"]["kind"], "unauthorized");

    // 索引公开（客户端据此发现需要令牌）
    let body = json_body(&get(server.addr, "/api").await);
    assert_eq!(body["auth_required"], true);

    // Bearer 头 / X-Api-Token / query 三种都能过
    for extra in [
        "Authorization: Bearer s3cret\r\n",
        "X-Api-Token: s3cret\r\n",
    ] {
        let resp = req(server.addr, "GET", "/api/graph", None, extra).await;
        assert_eq!(status_of(&resp), 200, "带 {extra:?} 应放行: {resp}");
    }
    let resp = get(server.addr, "/api/graph?token=s3cret").await;
    assert_eq!(status_of(&resp), 200, "query 令牌应放行: {resp}");

    // 错误令牌 → 401
    let resp = req(server.addr, "GET", "/api/graph", None, "Authorization: Bearer wrong\r\n").await;
    assert_eq!(status_of(&resp), 401);
}

#[tokio::test]
async fn cors_only_when_enabled() {
    // 默认关闭：无 CORS 头，且 OPTIONS 预检不被允许
    let (_engine, server) = start_server().await;
    let resp = request_raw(
        server.addr,
        "OPTIONS /api/graph HTTP/1.1\r\nHost: localhost\r\nOrigin: http://evil.test\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(!resp.contains("access-control-allow-origin"), "{resp}");

    // 开启后：普通响应带 CORS 头，预检 204 + 允许头
    let (_engine, server) = start_with(ApiOptions::new("127.0.0.1", 0).with_cors(true)).await;
    let resp = get(server.addr, "/api/graph").await;
    assert!(
        resp.to_lowercase().contains("access-control-allow-origin: *"),
        "应带 CORS 头: {resp}"
    );
    let resp = request_raw(
        server.addr,
        "OPTIONS /api/graph HTTP/1.1\r\nHost: localhost\r\nOrigin: http://panel.test\r\nAccess-Control-Request-Method: PUT\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status_of(&resp), 204, "预检应 204: {resp}");
    let lower = resp.to_lowercase();
    assert!(lower.contains("access-control-allow-methods"));
    assert!(lower.contains("authorization"), "应允许 Authorization 头: {resp}");
}

#[tokio::test]
async fn host_backed_endpoints_and_unsupported_without_host() {
    // 无宿主：501 unsupported
    let (_engine, server) = start_server().await;
    let resp = get(server.addr, "/api/settings").await;
    assert_eq!(status_of(&resp), 501, "{resp}");
    assert_eq!(json_body(&resp)["error"]["kind"], "unsupported");

    // 有宿主：设置读写 + USB/IP + 日志
    let host = TestHost::new();
    let (_engine, server) = start_with(ApiOptions::new("127.0.0.1", 0).with_host(host.clone())).await;

    let body = json_body(&get(server.addr, "/api/settings").await);
    assert_eq!(body["edge_buffer_ms"], 250);

    // PATCH 只改给定字段（递归合并）
    let resp = req(
        server.addr,
        "PATCH",
        "/api/settings",
        Some(r#"{"control_api":{"port":19000}}"#),
        "",
    )
    .await;
    assert_eq!(status_of(&resp), 200, "{resp}");
    let body = json_body(&resp);
    assert_eq!(body["control_api"]["port"], 19000);
    assert_eq!(body["control_api"]["enabled"], true, "未提到的字段应保留");
    assert_eq!(body["edge_buffer_ms"], 250);

    // PUT 整体替换
    let resp = req(
        server.addr,
        "PUT",
        "/api/settings",
        Some(r#"{"edge_buffer_ms":900}"#),
        "",
    )
    .await;
    let body = json_body(&resp);
    assert_eq!(body["edge_buffer_ms"], 900);
    assert!(body.get("control_api").is_none(), "替换后不留旧字段");

    let body = json_body(&get(server.addr, "/api/usbip").await);
    assert_eq!(body["running"], true);
    // 宿主未实现 attach → 501
    let resp = req(server.addr, "POST", "/api/usbip/attach", None, "").await;
    assert_eq!(status_of(&resp), 501);

    let body = json_body(&get(server.addr, "/api/logs?since=0&limit=10").await);
    assert_eq!(body["lines"][0]["text"], "hello");
    assert_eq!(body["limit"], 10);

    // 非法 body → 400 结构化错误
    let resp = req(server.addr, "PATCH", "/api/settings", Some("[1,2]"), "").await;
    assert_eq!(status_of(&resp), 400);
}

#[tokio::test]
async fn sse_receives_graph_applied_event() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_engine, server) = start_server().await;

    let mut stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(b"GET /api/events HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n")
        .await
        .unwrap();

    let body = serde_json::to_string(&graph_one_route(1.0)).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = req(server.addr, "PUT", "/api/graph", Some(&body), "").await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut acc = String::new();
    let mut buf = vec![0u8; 4096];
    while !acc.contains("event: graph_applied") && tokio::time::Instant::now() < deadline {
        let n = match tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            _ => continue,
        };
        acc.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(acc.contains("event: graph_applied"), "SSE 应收到 graph_applied: {acc}");
    assert!(acc.starts_with("HTTP/1.1 200"), "SSE 握手应 200: {acc}");
}

#[tokio::test]
async fn levels_stream_pushes_frames() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_engine, server) = start_server().await;
    let body = serde_json::to_string(&graph_one_route(1.0)).unwrap();
    let _ = req(server.addr, "PUT", "/api/graph", Some(&body), "").await;

    let mut stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(b"GET /api/stream/levels?interval_ms=20 HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut acc = String::new();
    let mut buf = vec![0u8; 4096];
    while !acc.contains("event: levels") && tokio::time::Instant::now() < deadline {
        let n = match tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            _ => continue,
        };
        acc.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(acc.contains("event: levels"), "应收到电平帧: {acc}");
    assert!(acc.contains("\"levels\""), "帧里应含 levels: {acc}");
    assert!(acc.contains("\"spectra\""), "帧里应含 spectra: {acc}");
}
