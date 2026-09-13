//! usbip.exe 探测 / 提权安装 / attach / detach。
//!
//! usbip-win2 的 CLI 与虚拟主控（UDE）设备都需要管理员权限，因此本模块统一用
//! 「临时 .ps1 + `Start-Process -Verb RunAs -Wait -PassThru`」提权执行，
//! 并把子进程输出重定向到临时文件后读回——`-Verb RunAs` 不能与
//! `-RedirectStandardOutput` 同时使用，只能让提权后的 PowerShell 自己写文件。
//!
//! 命令语义（以 usbip-win2 v0.9.8.0 源码为准）：
//! - `usbip attach -r <host> -b <busid> [-t] [--once]` → "succesfully attached to port N"
//! - `usbip port` → 逐行 "Port NN: device in use at <speed>" + 缩进详情（含 usbip://host:port/busid）
//! - `usbip detach -p N` / `usbip detach --all`
//! - 全局选项 `--tcp-port <port>` 必须写在子命令**之前**（attach 的 `-t` 是 --terse）

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

/// usbip-win2 默认 TCP 端口（同时也是本应用内置服务器的默认端口）
pub const DEFAULT_TCP_PORT: u16 = 3240;
/// 安装包默认安装目录（Inno Setup `DefaultDirName={autopf}\USBip`）
const USERMODE_DIRS: &[&str] = &["USBip"];

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// 已接入（imported）的虚拟设备端口
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AttachedPort {
    /// 端口号（`usbip detach -p N` 用）
    pub port: u32,
    pub in_use: bool,
    /// 远端 busid（从 `usbip://host:port/1-N` 解析）
    pub bus_id: Option<String>,
    /// 详情原文（多行）
    pub detail: String,
}

/// attach 结果（供 UI 展示）
#[derive(Debug, Clone, Default, Serialize)]
pub struct AttachReport {
    pub host: String,
    pub tcp_port: u16,
    /// 本次成功附加：busid → 端口号
    pub attached: Vec<(String, Option<u32>)>,
    /// 附加失败：busid + 原因
    pub failed: Vec<(String, String)>,
    /// attach 之前断开的端口
    pub detached: Vec<u32>,
    /// 原始输出（诊断）
    pub log: String,
}

// ---------- 路径探测 ----------

/// 可能的驱动资源根目录（开发期在 exe 同级的 drivers/，打包后在 resources/drivers/）
fn driver_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.join("drivers"));
            roots.push(dir.join("resources").join("drivers"));
            roots.push(dir.to_path_buf());
            if let Some(parent) = dir.parent() {
                roots.push(parent.join("Resources").join("drivers"));
            }
        }
    }
    roots
}

/// 随包捆绑的 usbip-win2 安装包（`drivers/usbip/USBip-*-x64.exe`）
pub fn bundled_installer() -> Option<PathBuf> {
    for root in driver_roots() {
        let dir = root.join("usbip");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension().map(|e| e.eq_ignore_ascii_case("exe")).unwrap_or(false)
                    && p.file_name()
                        .map(|n| n.to_string_lossy().to_ascii_lowercase().starts_with("usbip"))
                        .unwrap_or(false)
            })
            .collect();
        // 文件名带版本号：取字典序最大者（同目录一般只有一个）
        found.sort();
        if let Some(p) = found.pop() {
            return Some(p);
        }
    }
    None
}

/// 定位 usbip.exe：环境变量覆盖 → PATH → 常见安装目录 → 卸载注册表项
pub fn find_usbip() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("AUDIOMIX_USBIP") {
        let p = PathBuf::from(override_path);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("usbip.exe");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            let mut dir = PathBuf::from(base);
            if var == "LOCALAPPDATA" {
                dir.push("Programs");
            }
            for sub in USERMODE_DIRS {
                let cand = dir.join(sub).join("usbip.exe");
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    registry_install_dir().map(|d| d.join("usbip.exe")).filter(|p| p.is_file())
}

/// 从卸载注册表项读取安装目录（安装时若用户改过路径）
fn registry_install_dir() -> Option<PathBuf> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY};

    // Inno Setup 的 AppId（见 usbip-win2 userspace/innosetup/setup.iss）
    const SUBKEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{199505b0-b93d-4521-a8c7-897818e0205a}_is1";
    let hklm = winreg::RegKey::predef(HKEY_LOCAL_MACHINE);
    for flags in [KEY_READ | KEY_WOW64_64KEY, KEY_READ | KEY_WOW64_32KEY] {
        let Ok(key) = hklm.open_subkey_with_flags(SUBKEY, flags) else {
            continue;
        };
        if let Ok(loc) = key.get_value::<String, _>("InstallLocation") {
            let p = PathBuf::from(loc.trim());
            if p.is_dir() {
                return Some(p);
            }
        }
    }
    None
}

/// usbip.exe 是否已安装
pub fn installed() -> bool {
    find_usbip().is_some()
}

// ---------- 输出解析（纯函数，便于单测）----------

fn extract_bus_id(line: &str) -> Option<String> {
    let idx = line.find("usbip://")?;
    let rest = &line[idx + "usbip://".len()..];
    let seg = rest.split_whitespace().next()?;
    let bus = seg.rsplit('/').next()?;
    (!bus.is_empty()).then(|| bus.to_string())
}

/// 解析 `usbip port` 输出
pub fn parse_ports(text: &str) -> Vec<AttachedPort> {
    let mut ports: Vec<AttachedPort> = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if let Some(rest) = trimmed.strip_prefix("Port ") {
            // "00: device in use at High Speed(480Mbps)" / "00: <Port in Use> ..."
            if let Some((num, tail)) = rest.split_once(':') {
                if let Ok(port) = num.trim().parse::<u32>() {
                    ports.push(AttachedPort {
                        port,
                        in_use: tail.to_ascii_lowercase().contains("in use"),
                        bus_id: None,
                        detail: tail.trim().to_string(),
                    });
                    continue;
                }
            }
        }
        if let Some(cur) = ports.last_mut() {
            if trimmed.is_empty() {
                continue;
            }
            cur.detail.push('\n');
            cur.detail.push_str(trimmed);
            if cur.bus_id.is_none() {
                cur.bus_id = extract_bus_id(trimmed);
            }
        }
    }
    ports
}

/// 从 attach 输出解析端口号（"succesfully attached to port 3" / terse 模式 "3"）
pub fn parse_attached_port(text: &str) -> Option<u32> {
    let lower = text.to_ascii_lowercase();
    if let Some(idx) = lower.find("port ") {
        let digits: String = lower[idx + "port ".len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(p) = digits.parse() {
            return Some(p);
        }
    }
    // terse 模式（`usbip attach -t`）：输出只有端口号本身
    lower.split_whitespace().last()?.parse().ok()
}

/// 判断错误输出是否为"没有可断开的端口"（无害）
fn is_nothing_to_detach(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("detach") && (lower.contains("no ") || lower.contains("not found"))
}

// ---------- 进程执行 ----------

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub code: i32,
    pub output: String,
}

/// 同步执行并捕获 stdout+stderr（带超时；Windows 下不弹控制台窗口）
fn run_capture(program: &Path, args: &[String], timeout: Duration) -> Result<RunOutput, String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 {} 失败: {e}", program.display()))?;
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_reader = out_pipe.take().map(|mut p| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = p.read_to_end(&mut b);
            b
        })
    });
    let err_reader = err_pipe.take().map(|mut p| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = p.read_to_end(&mut b);
            b
        })
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("命令超时（{}s）", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(e) => return Err(format!("等待进程失败: {e}")),
        }
    };

    let mut text = String::new();
    if let Some(t) = out_reader {
        text.push_str(&String::from_utf8_lossy(&t.join().unwrap_or_default()));
    }
    if let Some(t) = err_reader {
        let err = String::from_utf8_lossy(&t.join().unwrap_or_default()).to_string();
        if !err.trim().is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&err);
        }
    }
    Ok(RunOutput {
        code: status.code().unwrap_or(-1),
        output: text,
    })
}

fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}

fn scratch_dir() -> PathBuf {
    std::env::temp_dir().join("audiomix-usbip")
}

/// 提权执行（UAC）。输出经临时文件回传；返回的 code 由提权进程给出。
/// 1223 = 用户取消 UAC。
fn run_elevated(program: &Path, args: &[String], timeout: Duration) -> Result<RunOutput, String> {
    let steps = [args.to_vec()];
    let mut results = run_elevated_sequence(program, &steps, timeout)?;
    Ok(results.remove(0))
}

/// 依次执行**多条**命令，全部在**同一次 UAC** 内完成（每条命令的退出码与输出分别回传）。
///
/// 逐条 `Start-Process -Verb RunAs` 会给每个动作弹一次 UAC；「附加全部」要 detach 1 次 +
/// attach N 次，因此合并成一个提权脚本，脚本内顺序执行并写标记分隔的日志。
fn run_elevated_sequence(
    program: &Path,
    steps: &[Vec<String>],
    timeout: Duration,
) -> Result<Vec<RunOutput>, String> {
    let dir = scratch_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    let id = SCRATCH_SEQ.fetch_add(1, Ordering::SeqCst);
    let stamp = format!("{}-{id}", std::process::id());
    let ps1 = dir.join(format!("run-{stamp}.ps1"));
    let log = dir.join(format!("run-{stamp}.log"));
    let _ = std::fs::remove_file(&log);

    let mut script = String::from("$ErrorActionPreference = 'Continue'\n");
    script.push_str("[Console]::OutputEncoding = [Text.Encoding]::UTF8\n");
    script.push_str(&format!(
        "$log = '{}'\n",
        ps_quote(&log.to_string_lossy())
    ));
    for (i, args) in steps.iter().enumerate() {
        script.push_str(&format!("Add-Content -LiteralPath $log -Value '@@STEP {i}'\n"));
        script.push_str(&format!("$out = & '{}'", ps_quote(&program.to_string_lossy())));
        for a in args {
            script.push_str(&format!(" '{}'", ps_quote(a)));
        }
        script.push_str(" 2>&1 | Out-String\n");
        script.push_str("if ($null -eq $out) { $out = '' }\n");
        script.push_str("Add-Content -LiteralPath $log -Value $out\n");
        script.push_str("Add-Content -LiteralPath $log -Value \"@@CODE $LASTEXITCODE\"\n");
    }
    script.push_str("exit 0\n");
    // 带 BOM 写出，Windows PowerShell 5.1 才会按 UTF-8 解析含非 ASCII 的路径
    std::fs::write(&ps1, format!("\u{FEFF}{script}")).map_err(|e| format!("写入提权脚本失败: {e}"))?;

    let elevated_cmd = format!(
        "$ErrorActionPreference='Stop'; try {{ $p = Start-Process -Verb RunAs -Wait -PassThru \
         -FilePath 'powershell' -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File','{}'); \
         exit $p.ExitCode }} catch {{ exit 1223 }}",
        ps_quote(&ps1.to_string_lossy())
    );

    let result = run_capture(
        Path::new("powershell"),
        &["-NoProfile".into(), "-Command".into(), elevated_cmd],
        timeout,
    );

    let output = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .trim_start_matches('\u{FEFF}')
        .to_string();
    let _ = std::fs::remove_file(&ps1);
    let _ = std::fs::remove_file(&log);

    let outer = result?;
    tracing::info!(
        "提权执行 {}（{} 步）→ 外层退出码 {}",
        program.display(),
        steps.len(),
        outer.code
    );
    if outer.code == 1223 {
        return Ok(vec![RunOutput { code: 1223, output: String::new() }; steps.len()]);
    }

    let parsed: Vec<RunOutput> = parse_sequence_log(&output, steps.len())
        .into_iter()
        .map(|(code, output)| RunOutput { code, output })
        .collect();
    for (i, step) in steps.iter().enumerate() {
        if let Some(o) = parsed.get(i) {
            tracing::info!("  提权步骤 {i} [{}] → {}：{}", step.join(" "), o.code, o.output.trim());
        }
    }
    if outer.code != 0 {
        tracing::warn!("提权外层进程退出码 {}：{}", outer.code, outer.output.trim());
    }
    Ok(parsed)
}

/// 解析 `@@STEP n` / `@@CODE x` 标记的分步日志
fn parse_sequence_log(log: &str, steps: usize) -> Vec<(i32, String)> {
    let mut out: Vec<(i32, String)> = (0..steps).map(|_| (-1, String::new())).collect();
    let mut current: Option<usize> = None;
    for line in log.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("@@STEP ") {
            current = rest.trim().parse::<usize>().ok();
            continue;
        }
        if let Some(rest) = line.trim_start().strip_prefix("@@CODE ") {
            if let Some(i) = current {
                if let Some(slot) = out.get_mut(i) {
                    slot.0 = rest.trim().parse().unwrap_or(-1);
                }
            }
            current = None;
            continue;
        }
        if let Some(i) = current {
            if let Some(slot) = out.get_mut(i) {
                slot.1.push_str(line);
                slot.1.push('\n');
            }
        }
    }
    out
}

// ---------- 对外操作 ----------

/// `usbip port`（不提权；失败时给出可读原因）
pub fn ports() -> Result<Vec<AttachedPort>, String> {
    let exe = find_usbip().ok_or("未找到 usbip.exe，请先安装 USB/IP 驱动")?;
    let out = run_capture(&exe, &["port".into()], Duration::from_secs(20))?;
    if out.code != 0 {
        tracing::warn!("usbip port 失败（退出码 {}）：{}", out.code, out.output.trim());
        return Err(format!(
            "usbip port 失败（退出码 {}）：{}",
            out.code,
            out.output.trim()
        ));
    }
    Ok(parse_ports(&out.output))
}

/// 安装随包捆绑的 usbip-win2（提权静默安装）。返回安装程序输出。
pub fn install_bundled() -> Result<String, String> {
    let installer = bundled_installer().ok_or(
        "未找到随包安装包（resources/drivers/usbip/USBip-*-x64.exe）",
    )?;
    tracing::info!("开始安装 USB/IP 驱动: {}", installer.display());
    let args: Vec<String> = ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let out = run_elevated(&installer, &args, Duration::from_secs(900))?;
    match out.code {
        0 => {
            tracing::info!("USB/IP 驱动安装完成，usbip.exe: {:?}", find_usbip());
            Ok(out.output)
        }
        1223 => Err("已取消 UAC 授权".into()),
        code => {
            let mut msg = format!("安装程序退出码 {code}（驱动未安装成功）");
            if out.output.trim().is_empty() {
                // 官方安装包在「未开启测试签名」时会直接中止且不产生输出
                msg.push_str(
                    "\n安装程序没有输出就中止：该版本可能要求开启测试签名（管理员执行 \
                     `bcdedit /set testsigning on` 后重启）。注意测试签名与内存完整性（HVCI）\
                     互斥——若已开启内存完整性，请改用 attestation 签名的驱动版本。",
                );
            } else {
                msg.push('\n');
                msg.push_str(out.output.trim());
            }
            Err(msg)
        }
    }
}

fn attach_args(host: &str, tcp_port: u16, bus_id: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    // 全局 --tcp-port 必须写在子命令之前（attach 的 -t 是 --terse）
    if tcp_port != DEFAULT_TCP_PORT {
        args.push("--tcp-port".into());
        args.push(tcp_port.to_string());
    }
    args.push("attach".into());
    args.push("-r".into());
    args.push(host.into());
    args.push("-b".into());
    args.push(bus_id.into());
    args.push("--once".into());
    args
}

fn detach_args_all() -> Vec<String> {
    vec!["detach".into(), "--all".into()]
}

/// 附加一条线缆（提权）
pub fn attach(host: &str, tcp_port: u16, bus_id: &str) -> Result<String, String> {
    let exe = find_usbip().ok_or("未找到 usbip.exe，请先安装 USB/IP 驱动")?;
    let out = run_elevated(&exe, &attach_args(host, tcp_port, bus_id), Duration::from_secs(120))?;
    match out.code {
        0 => Ok(out.output),
        1223 => Err("已取消 UAC 授权".into()),
        code => Err(format!("附加 {bus_id} 失败（退出码 {code}）：{}", out.output.trim())),
    }
}

/// 断开全部已接入的线缆（提权）
pub fn detach_all() -> Result<String, String> {
    let exe = find_usbip().ok_or("未找到 usbip.exe，请先安装 USB/IP 驱动")?;
    let out = run_elevated(&exe, &detach_args_all(), Duration::from_secs(120))?;
    match out.code {
        0 => Ok(out.output),
        1223 => Err("已取消 UAC 授权".into()),
        code => {
            if is_nothing_to_detach(&out.output) {
                Ok(out.output)
            } else {
                Err(format!("断开全部端口失败（退出码 {code}）：{}", out.output.trim()))
            }
        }
    }
}

/// 重新附加全部线缆：先断开所有端口，再逐条 attach。
/// **所有步骤在同一次 UAC 内完成**（N 条线缆只弹一次授权）。
pub fn attach_all(host: &str, tcp_port: u16, bus_ids: &[String]) -> Result<AttachReport, String> {
    let exe = find_usbip().ok_or("未找到 usbip.exe，请先安装 USB/IP 驱动")?;
    tracing::info!("附加全部线缆: {bus_ids:?} @ {host}:{tcp_port}");
    let mut report = AttachReport {
        host: host.to_string(),
        tcp_port,
        ..Default::default()
    };

    // 步骤 0 = detach --all（清掉旧端口，服务器重启后旧句柄已失效），其后每步一条线缆
    let mut steps: Vec<Vec<String>> = vec![detach_args_all()];
    for bus in bus_ids {
        steps.push(attach_args(host, tcp_port, bus));
    }
    let results = run_elevated_sequence(&exe, &steps, Duration::from_secs(300))?;
    let mut iter = results.into_iter();
    if let Some(o) = iter.next() {
        if o.code == 1223 {
            return Err("已取消 UAC 授权".into());
        }
        report.log.push_str(&format!("[detach --all] code={}\n{}\n", o.code, o.output.trim()));
    }

    // 逐条附加
    for bus in bus_ids {
        match iter.next() {
            Some(o) if o.code == 0 => {
                let port = parse_attached_port(&o.output);
                report.log.push_str(&format!("[attach {bus}] code=0 port={port:?}\n{}\n", o.output.trim()));
                report.attached.push((bus.clone(), port));
            }
            Some(o) if o.code == 1223 => return Err("已取消 UAC 授权".into()),
            Some(o) => {
                report
                    .log
                    .push_str(&format!("[attach {bus}] code={}\n{}\n", o.code, o.output.trim()));
                report.failed.push((bus.clone(), o.output.trim().to_string()));
            }
            None => {
                let e = "提权脚本未返回该步骤结果".to_string();
                report.log.push_str(&format!("[attach {bus}] {e}\n"));
                report.failed.push((bus.clone(), e));
            }
        }
    }

    // 3) 回读端口状态
    if let Ok(list) = ports() {
        report.detached = list.iter().map(|p| p.port).collect();
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_OUTPUT: &str = "\
Port 00: device in use at High Speed(480Mbps)
         Virtual Cable 01 (ffff:ca01)
           -> usbip://127.0.0.1:3240/1-1
           -> remote bus/dev: 001/001
           -> serial: AUDIOMIX-VCABLE-001
           -> mode: Zero copy
Port 03: device in use at High Speed(480Mbps)
         Virtual Cable 02 (ffff:ca02)
           -> usbip://127.0.0.1:3240/1-2
           -> remote bus/dev: 001/002
";

    #[test]
    fn parses_usbip_win2_port_output() {
        let ports = parse_ports(REAL_OUTPUT);
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].port, 0);
        assert!(ports[0].in_use);
        assert_eq!(ports[0].bus_id.as_deref(), Some("1-1"));
        assert!(ports[0].detail.contains("Virtual Cable 01"));
        assert!(ports[0].detail.contains("usbip://127.0.0.1:3240/1-1"));
        assert_eq!(ports[1].port, 3);
        assert_eq!(ports[1].bus_id.as_deref(), Some("1-2"));
    }

    #[test]
    fn parses_linux_style_port_output() {
        // 兼容 Linux usbip / 旧版格式
        let text = "\
Imported USB devices
====================
Port 00: <Port in Use> at High Speed(480Mbps)
       unknown vendor : unknown product (ffff:ca01)
       1-1 -> usbip://127.0.0.1:3240/1-1
";
        let ports = parse_ports(text);
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port, 0);
        assert!(ports[0].in_use);
        assert_eq!(ports[0].bus_id.as_deref(), Some("1-1"));
    }

    #[test]
    fn empty_output_yields_no_ports() {
        assert!(parse_ports("").is_empty());
        assert!(parse_ports("Imported USB devices\n====================\n").is_empty());
    }

    #[test]
    fn parses_attached_port_from_attach_output() {
        assert_eq!(parse_attached_port("succesfully attached to port 1\n"), Some(1));
        assert_eq!(parse_attached_port("successfully attached to port 12"), Some(12));
        assert_eq!(parse_attached_port("12\n"), Some(12));
        assert_eq!(parse_attached_port("error: cannot connect"), None);
    }

    #[test]
    fn attach_args_put_tcp_port_before_subcommand() {
        let a = attach_args("127.0.0.1", DEFAULT_TCP_PORT, "1-3");
        assert_eq!(a, vec!["attach", "-r", "127.0.0.1", "-b", "1-3", "--once"]);
        let b = attach_args("127.0.0.1", 4000, "1-3");
        assert_eq!(
            b,
            vec!["--tcp-port", "4000", "attach", "-r", "127.0.0.1", "-b", "1-3", "--once"]
        );
    }

    #[test]
    fn bus_id_extraction_tolerates_extra_text() {
        assert_eq!(
            extract_bus_id("1-1 -> usbip://192.168.1.9:3240/2-4"),
            Some("2-4".to_string())
        );
        assert_eq!(extract_bus_id("no location here"), None);
    }

    #[test]
    fn sequence_log_parsing_splits_steps() {
        // detach --all 失败 + 两条 attach（一成功一失败）在一次提权里的日志形态
        let log = "\
@@STEP 0
error: no imported devices
@@CODE 1
@@STEP 1
succesfully attached to port 1
@@CODE 0
@@STEP 2
error: cannot connect to 127.0.0.1
@@CODE 255
";
        let parsed = parse_sequence_log(log, 3);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].0, 1);
        assert!(parsed[0].1.contains("no imported devices"));
        assert_eq!(parsed[1].0, 0);
        assert_eq!(parsed[1].1.trim(), "succesfully attached to port 1");
        assert_eq!(parsed[2].0, 255);
        // 缺失的步骤返回 (-1, 空)
        let short = parse_sequence_log("@@STEP 0\nok\n@@CODE 0\n", 3);
        assert_eq!(short.len(), 3);
        assert_eq!(short[1], (-1, String::new()));
        assert_eq!(short[2], (-1, String::new()));
    }
}
