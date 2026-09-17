//! 提权 broker（参考实现 Virtual-Cables 的同款架构）：
//!
//! `usbip.exe` 的 attach/detach 操作 vhci 驱动，**每次都要管理员权限**；逐次提权
//! 意味着每次操作弹一次 UAC。这里改成：主进程开一个本地 TCP listener + 一次性
//! token，把**自己**以管理员身份再启动一份（`--usbip-broker <addr> <token>`，
//! GUI 子系统进程天然无窗口）——用户只确认**一次** UAC，broker 常驻后台，
//! 之后所有 attach / detach / 重复端口清理都通过这条连接执行，零 UAC。
//!
//! 协议（一问一答，行分隔）：
//! - broker 连上后先发一行 token；
//! - 应用发 `ATTACH <host> <port> <busid,..>` / `DETACH_ALL` / `DETACH_PORTS <n,..>` / `QUIT`；
//! - broker 回 `OK` 或 `ERR <原因>`，随后一行 payload 长度 + payload（usbip 原始输出，
//!   ATTACH 带 `@@STEP n` / `@@CODE x` 分步标记，与提权脚本同格式）。
//! - 应用退出（或进程死亡）时连接关闭，broker 随即退出。

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use uuid::Uuid;

use super::attach::{
    attach_args, attach_report_from, detach_args_all, find_usbip, parse_sequence_log, ps_quote,
    run_capture, AttachReport,
};

/// 单条命令的读写超时（attach 含服务器就绪等待，放宽一些）
const COMMAND_TIMEOUT: Duration = Duration::from_secs(180);
/// 等待 broker 连回主进程的窗口（要覆盖用户点 UAC 的耗时）
const ACCEPT_WINDOW: Duration = Duration::from_secs(120);

static CONN: Mutex<Option<TcpStream>> = Mutex::new(None);
static STARTING: AtomicBool = AtomicBool::new(false);

/// broker 是否在线
pub fn broker_online() -> bool {
    CONN.lock().unwrap().is_some()
}

/// 确保 broker 在线；不在线则自启动（**弹一次 UAC**）并等待连回。
/// - `Ok(())`：broker 已就绪，后续操作零 UAC；
/// - `Err`：用户取消 UAC（错误信息含「取消」）或启动失败。调用方据此决定是否
///   回退到一次性提权脚本（取消 UAC 时不应再弹）。
pub fn ensure_broker() -> Result<(), String> {
    if broker_online() {
        return Ok(());
    }
    if STARTING.swap(true, Ordering::SeqCst) {
        return Err("提权代理正在启动中".into());
    }
    let result = start_broker();
    STARTING.store(false, Ordering::SeqCst);
    result
}

fn start_broker() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法定位自身可执行文件: {e}"))?;
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("无法创建 broker 通道: {e}"))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    // 一次性 token：两个 UUID 拼 64 个 hex 字符
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    tracing::info!("启动 USB/IP 提权代理（需要一次管理员确认）");
    // 提权自启动；-WindowStyle Hidden + GUI 子系统 = 无窗口
    let ps = format!(
        "$ErrorActionPreference='Stop'; try {{ Start-Process -Verb RunAs -WindowStyle Hidden \
         -FilePath '{}' -ArgumentList '--usbip-broker','{}','{}' | Out-Null }} catch {{ exit 1223 }}",
        ps_quote(&exe.to_string_lossy()),
        addr,
        token
    );
    let out = run_capture(
        Path::new("powershell"),
        &["-NoProfile".into(), "-Command".into(), ps],
        Duration::from_secs(120),
    )?;
    if out.code == 1223 {
        return Err("已取消 UAC 授权".into());
    }
    if out.code != 0 {
        return Err(format!("提权代理启动失败（退出码 {}）", out.code));
    }

    // 等提权后的 broker 连回来并完成 token 认证
    let deadline = std::time::Instant::now() + ACCEPT_WINDOW;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    loop {
        if std::time::Instant::now() > deadline {
            return Err("提权代理未在规定时间内连回".into());
        }
        match listener.accept() {
            Ok((conn, peer)) if peer.ip().is_loopback() => {
                handshake(conn, &token)?;
                tracing::info!("USB/IP 提权代理已连接，本次会话内不再弹出 UAC");
                return Ok(());
            }
            Ok((_, _)) => continue, // 非 loopback 直接拒绝（关掉再等下一个）
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("等待 broker 连接失败: {e}")),
        }
    }
}

/// 从流里读一行（到 `\n` 为止，含换行）；TcpStream 没有现成的 read_line
fn read_line_from(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte)?;
        if n == 0 {
            break; // EOF
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&line).to_string())
}

/// 认证：broker 先发一行 token，匹配才收编这条连接
fn handshake(mut conn: TcpStream, token: &str) -> Result<(), String> {
    // WinSock 里 accept 出来的连接会**继承监听 socket 的非阻塞属性**；
    // 非阻塞下 set_read_timeout 无效，读应答时缓冲区暂时没数据就立刻
    // WouldBlock(10035) 而不是等待 —— 必须先切回阻塞模式。
    conn.set_nonblocking(false).map_err(|e| e.to_string())?;
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let line = read_line_from(&mut conn).map_err(|e| format!("读取 broker 认证失败: {e}"))?;
    if line.trim() != token {
        return Err("broker 认证失败".into());
    }
    let _ = conn.set_read_timeout(None);
    *CONN.lock().unwrap() = Some(conn);
    Ok(())
}

/// 发一条命令并收回 payload（status = OK；ERR 时返回 Err）
fn broker_command(line: &str) -> Result<String, String> {
    let mut guard = CONN.lock().unwrap();
    let Some(conn) = guard.as_mut() else {
        return Err("提权代理未运行".into());
    };
    let _ = conn.set_read_timeout(Some(COMMAND_TIMEOUT));
    let _ = conn.set_write_timeout(Some(Duration::from_secs(10)));
    if let Err(e) = writeln!(conn, "{line}") {
        *guard = None;
        return Err(format!("发送 broker 命令失败: {e}"));
    }
    let mut status = String::new();
    if let Err(e) = read_line_from(conn).map(|l| status = l) {
        *guard = None;
        return Err(format!("读取 broker 应答失败: {e}"));
    }
    let status = status.trim();
    if let Some(reason) = status.strip_prefix("ERR") {
        return Err(format!("broker 执行失败:{}", reason));
    }
    if status != "OK" {
        *guard = None;
        return Err(format!("broker 应答异常: {status}"));
    }
    let mut len_line = String::new();
    if let Err(e) = read_line_from(conn).map(|l| len_line = l) {
        *guard = None;
        return Err(format!("读取 broker 数据长度失败: {e}"));
    }
    let len: usize = len_line
        .trim()
        .parse()
        .map_err(|_| "broker 数据长度非法".to_string())?;
    let mut payload = vec![0u8; len];
    if let Err(e) = conn.read_exact(&mut payload) {
        *guard = None;
        return Err(format!("读取 broker 数据失败: {e}"));
    }
    Ok(String::from_utf8_lossy(&payload).to_string())
}

/// 通过 broker 附加全部线缆（broker 已提权，零 UAC）。
/// 输出格式与提权脚本一致（`@@STEP n` / `@@CODE x`），解析共用。
pub fn broker_attach_all(
    host: &str,
    tcp_port: u16,
    bus_ids: &[String],
) -> Result<AttachReport, String> {
    if bus_ids.is_empty() {
        return Err("尚未配置任何虚拟线缆".into());
    }
    // 与提权回退路径 attach_all 同语义：先 detach --all 清掉旧端口。
    // 改采样率/位宽/协议后，旧端口上的设备还是旧描述符，必须先从 Windows
    // 移除、重新附加重新枚举，新配置才会生效；也避免同一 busid 重复 import。
    match broker_detach_all() {
        Ok(out) => tracing::info!("broker detach --all（重新附加前置）: {}", out.trim()),
        Err(e) => tracing::warn!("broker detach --all 失败（继续尝试附加）: {e}"),
    }
    let cmd = format!("ATTACH {host} {tcp_port} {}", bus_ids.join(","));
    let out = broker_command(&cmd)?;
    let results = parse_sequence_log(&out, bus_ids.len())
        .into_iter()
        .map(|(code, output)| super::attach::RunOutput { code, output })
        .collect();
    Ok(attach_report_from(host, tcp_port, bus_ids, results))
}

/// 通过 broker 断开全部端口（零 UAC）
pub fn broker_detach_all() -> Result<String, String> {
    broker_command("DETACH_ALL")
}

/// 通过 broker 拆掉指定端口（零 UAC；用于清理 vhci 重复 import）
pub fn broker_detach_ports(ports: &[u32]) -> Result<String, String> {
    if ports.is_empty() {
        return Ok(String::new());
    }
    let list: Vec<String> = ports.iter().map(|p| p.to_string()).collect();
    broker_command(&format!("DETACH_PORTS {}", list.join(",")))
}

/// 应用退出前通知 broker 收工（连接断开后 broker 自行退出）
pub fn shutdown_broker() {
    if let Ok(mut guard) = CONN.lock() {
        if let Some(conn) = guard.as_mut() {
            let _ = writeln!(conn, "QUIT");
            let _ = conn.flush();
        }
        *guard = None;
    }
}

// ---------- broker 侧（提权进程入口） ----------

/// broker 主循环：认证后逐条执行命令；连接断开即退出。
pub fn run_broker(addr: &str, token: &str) -> Result<(), String> {
    if !addr.starts_with("127.0.0.1:")
        || token.len() != 64
        || !token.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("broker 参数非法".into());
    }
    let stream_addr: std::net::SocketAddr = addr.parse().map_err(|e| format!("地址非法: {e}"))?;
    let mut conn = TcpStream::connect_timeout(&stream_addr, Duration::from_secs(10))
        .map_err(|e| format!("连回主进程失败: {e}"))?;
    writeln!(conn, "{token}").map_err(|e| format!("发送认证失败: {e}"))?;
    tracing::info!("USB/IP 提权代理已启动（{}）", addr);

    let exe = find_usbip().ok_or("未找到 usbip.exe，请先安装 USB/IP 驱动")?;
    // 读端交给 BufReader，写端克隆出来回传结果
    let mut writer = conn.try_clone().map_err(|e| format!("克隆连接失败: {e}"))?;
    for line in std::io::BufReader::new(conn).lines() {
        let Ok(line) = line else { break }; // 主进程退出 → 连接关闭 → broker 跟着退
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "QUIT" {
            tracing::info!("USB/IP 提权代理收到退出指令");
            break;
        }
        let (status, payload) = execute(line, &exe);
        let _ = writeln!(writer, "{status}");
        let _ = writeln!(writer, "{}", payload.len());
        let _ = writer.write_all(payload.as_bytes());
        let _ = writer.flush();
    }
    Ok(())
}

/// 执行一条命令，返回 (状态行, payload)
fn execute(line: &str, exe: &Path) -> (String, String) {
    let fields: Vec<&str> = line.split_whitespace().collect();
    match fields.as_slice() {
        ["ATTACH", host, port, bus_list] => {
            let Ok(tcp_port) = port.parse::<u16>() else {
                return ("ERR 非法端口".into(), String::new());
            };
            let bus_ids: Vec<&str> = bus_list.split(',').collect();
            let mut payload = String::new();
            let mut all_ok = true;
            for (i, bus) in bus_ids.iter().enumerate() {
                payload.push_str(&format!("@@STEP {i}\n"));
                let r = run_capture(
                    exe,
                    &attach_args(host, tcp_port, bus),
                    Duration::from_secs(120),
                )
                .unwrap_or(super::attach::RunOutput {
                    code: -1,
                    output: "执行失败".into(),
                });
                tracing::info!(
                    "  broker attach {host}:{tcp_port} {bus} → {}：{}",
                    r.code,
                    r.output.trim()
                );
                payload.push_str(&r.output);
                if !payload.ends_with('\n') {
                    payload.push('\n');
                }
                payload.push_str(&format!("@@CODE {}\n", r.code));
                if r.code != 0 {
                    all_ok = false;
                }
            }
            if all_ok {
                ("OK".into(), payload)
            } else {
                ("ERR 附加失败".into(), payload)
            }
        }
        ["DETACH_ALL"] => {
            let r = run_capture(exe, &detach_args_all(), Duration::from_secs(120)).unwrap_or(
                super::attach::RunOutput {
                    code: -1,
                    output: "执行失败".into(),
                },
            );
            tracing::info!("  broker detach --all → {}：{}", r.code, r.output.trim());
            (ok_or_err(r.code), r.output)
        }
        ["DETACH_PORTS", list] => {
            let mut payload = String::new();
            let mut all_ok = true;
            for p in list.split(',') {
                let args = vec!["detach".to_string(), "-p".to_string(), p.to_string()];
                let r = run_capture(exe, &args, Duration::from_secs(120)).unwrap_or(
                    super::attach::RunOutput {
                        code: -1,
                        output: "执行失败".into(),
                    },
                );
                tracing::info!("  broker detach -p {p} → {}：{}", r.code, r.output.trim());
                payload.push_str(&format!(
                    "[detach -p {p}] code={}\n{}\n",
                    r.code,
                    r.output.trim()
                ));
                if r.code != 0 {
                    all_ok = false;
                }
            }
            (ok_or_err(if all_ok { 0 } else { -1 }), payload)
        }
        _ => ("ERR 非法命令".into(), String::new()),
    }
}

fn ok_or_err(code: i32) -> String {
    if code == 0 {
        "OK".into()
    } else {
        format!("ERR 退出码 {code}")
    }
}
