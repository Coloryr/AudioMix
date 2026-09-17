//! 控制 API 集成测试：在 port 0 上起真实 axum 服务，用裸 TCP 手写 HTTP/1.1
//! 请求验证各端点（不引入 reqwest，保持依赖精简）。

use std::sync::Arc;
use std::time::Duration;

use audiomix_control_api::{spawn, ApiServer};
use audiomix_core::model::{GraphConfig, Route, Sink, Source, SourceMode};
use audiomix_core::testing::FakeBackend;
use audiomix_core::Engine;

/// 发送 HTTP 请求并读取完整响应文本。
/// 不主动 shutdown 写端：hyper 会把客户端半关闭当作连接中止；
/// 请求带 `Connection: close`，服务端响应后关闭连接，read_to_end 到 EOF。
async fn request(addr: std::net::SocketAddr, req: &str) -> String {
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

async fn get(addr: std::net::SocketAddr, path: &str) -> String {
    request(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
    )
    .await
}

async fn put_json(addr: std::net::SocketAddr, path: &str, body: &str) -> String {
    request(
        addr,
        &format!(
            "PUT {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await
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

async fn start_server() -> (Arc<Engine>, ApiServer) {
    let backend = FakeBackend::new();
    let engine = audiomix_core::Engine::new(backend).unwrap();
    let server = spawn(
        engine.clone(),
        "127.0.0.1",
        0, // 随机端口，避免并发测试冲突
    )
    .await
    .unwrap();
    (engine, server)
}

#[tokio::test]
async fn health_returns_ok() {
    let (_engine, server) = start_server().await;
    let resp = get(server.addr, "/api/health").await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "响应: {resp}");
    assert!(resp.ends_with("ok"), "响应体应为 ok: {resp}");
}

#[tokio::test]
async fn devices_returns_fake_list() {
    let (_engine, server) = start_server().await;
    let resp = get(server.addr, "/api/devices").await;
    assert!(resp.contains("200 OK"));
    assert!(resp.contains("dev-in-1"), "应含默认输入设备: {resp}");
    assert!(resp.contains("dev-out-1"), "应含默认输出设备: {resp}");
    assert!(resp.contains("Fake Microphone"));
}

#[tokio::test]
async fn graph_roundtrip_and_validation() {
    let (_engine, server) = start_server().await;

    // 初始为空图
    let resp = get(server.addr, "/api/graph").await;
    assert!(resp.contains("200 OK"));
    assert!(resp.contains("\"sources\":[]"), "初始应为空图: {resp}");

    // PUT 合法图
    let body = serde_json::to_string(&graph_one_route(0.5)).unwrap();
    let resp = put_json(server.addr, "/api/graph", &body).await;
    assert!(resp.contains("200 OK"));
    assert!(resp.contains("\"ok\":true"), "PUT 应成功: {resp}");

    // GET 回读一致
    let resp = get(server.addr, "/api/graph").await;
    assert!(resp.contains("\"gain\":0.5"), "gain 应已生效: {resp}");
    assert!(resp.contains("\"id\":\"src\""));

    // PUT 悬空引用的图 → ok:false
    let bad = r#"{"sources":[],"sinks":[],"routes":[{"id":"r","source_id":"ghost","sink_id":"out","gain":1.0,"muted":false}]}"#;
    let resp = put_json(server.addr, "/api/graph", bad).await;
    assert!(resp.contains("\"ok\":false"), "悬空 route 应被拒绝: {resp}");
}

#[tokio::test]
async fn graph_apply_starts_streams() {
    let (_engine, server) = start_server().await;
    let body = serde_json::to_string(&graph_one_route(1.0)).unwrap();
    let resp = put_json(server.addr, "/api/graph", &body).await;
    assert!(resp.contains("\"ok\":true"));

    // status 应报告 fake 后端
    let resp = get(server.addr, "/api/status").await;
    assert!(
        resp.contains("\"backend\":\"fake\""),
        "status 应含后端名: {resp}"
    );
    assert!(resp.contains("\"levels\""), "status 应含电平表: {resp}");
    assert!(resp.contains("\"stats\""), "status 应含统计: {resp}");
}

#[tokio::test]
async fn unknown_route_returns_404() {
    let (_engine, server) = start_server().await;
    let resp = get(server.addr, "/api/nope").await;
    assert!(resp.starts_with("HTTP/1.1 404"), "未知路径应 404: {resp}");
}

#[tokio::test]
async fn sse_receives_graph_applied_event() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_engine, server) = start_server().await;

    // 连上 SSE 后保持连接，触发 PUT，循环读直到收到事件
    let mut stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(
            b"GET /api/events HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n",
        )
        .await
        .unwrap();

    let body = serde_json::to_string(&graph_one_route(1.0)).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = put_json(server.addr, "/api/graph", &body).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut acc = String::new();
    let mut buf = vec![0u8; 4096];
    while !acc.contains("event: graph_applied") && tokio::time::Instant::now() < deadline {
        let n = match tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await
        {
            Ok(Ok(n)) => n,
            _ => continue,
        };
        acc.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(
        acc.contains("event: graph_applied"),
        "SSE 应收到 graph_applied: {acc}"
    );
    assert!(acc.starts_with("HTTP/1.1 200"), "SSE 握手应 200: {acc}");
}
