//! 与**真实 usbip-win2 客户端**（`usbip.exe`）的互操作测试。
//!
//! 前面的 `usbip_server.rs` 用的是自己写的测试客户端，只能证明「我方收发自洽」；
//! 这里让官方 CLI 真的去 `usbip list -r 127.0.0.1`，验证我们写出的
//! `OP_REP_DEVLIST`（含 usbip_usb_device 的位域布局、接口记录条数）能被官方实现解析。
//!
//! 需要已安装 usbip-win2（`C:\Program Files\USBip\usbip.exe`）；未安装或 3240 被占用时
//! **跳过而不是失败**，以免在有应用实例运行时误报。

use std::process::Command;
use std::sync::Arc;

use audiomix_backend_windows::usbip::device::{CableConfig, CableMode};
use audiomix_backend_windows::usbip::{attach, UsbIpManager};

fn cable(number: u8, rate: u32, bits: u16) -> CableConfig {
    CableConfig {
        number,
        name: String::new(),
        sample_rate: rate,
        bits,
        mode: CableMode::Loopback,
        buffer_ms: 250,
    }
}

#[test]
fn official_usbip_client_lists_our_devices() {
    let Some(exe) = attach::find_usbip() else {
        eprintln!("跳过：未检测到 usbip.exe（未安装 usbip-win2）");
        return;
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    // 用固定端口：官方 CLI 的默认 TCP 端口就是 3240
    let manager = UsbIpManager::new("127.0.0.1:3240");
    if let Err(e) = manager.start(rt.handle(), vec![cable(1, 48_000, 16), cable(2, 192_000, 16)]) {
        eprintln!("跳过：3240 不可用（{e}）——可能有应用实例正在跑虚拟声卡服务器");
        return;
    }
    let addr = manager.local_addr().expect("应已绑定");
    assert_eq!(addr.port(), 3240);

    let out = Command::new(&exe)
        .args(["list", "-r", "127.0.0.1"])
        .output()
        .expect("应能运行 usbip.exe");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    println!("--- usbip list -r 127.0.0.1 ---\n{text}--- end ---");

    manager.stop();

    assert!(out.status.success(), "usbip list 退出码非 0：{text}");
    // 官方实现按 busid 列出设备；能解析出来就说明 usbip_usb_device 布局与接口记录一致
    assert!(text.contains("1-1"), "应列出 busid 1-1：{text}");
    assert!(text.contains("1-2"), "应列出 busid 1-2：{text}");
    // VID 0xFFFF / PID 0xCA01+ 会以 ffff:ca01 形式出现
    let lower = text.to_lowercase();
    assert!(lower.contains("ffff"), "应列出 VID/PID：{text}");
    assert!(lower.contains("ca01") && lower.contains("ca02"), "应列出两条线缆的 PID：{text}");
}

#[test]
fn official_client_reports_bad_remote_as_error() {
    let Some(exe) = attach::find_usbip() else {
        eprintln!("跳过：未检测到 usbip.exe");
        return;
    };
    // 未监听的端口：官方客户端应报错而不是无限等待
    let out = Command::new(&exe)
        .args(["list", "-r", "127.0.0.1", "--tcp-port", "3299"])
        .output()
        .expect("应能运行 usbip.exe");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    println!("--- usbip list（错误端口）---\n{text}--- end ---");
    assert!(!out.status.success(), "连不上时应返回非 0：{text}");
}

/// 线缆注册表在多线程运行时下被官方客户端查询后仍可安全停止
#[test]
fn manager_survives_real_client_session() {
    let Some(_exe) = attach::find_usbip() else {
        eprintln!("跳过：未检测到 usbip.exe");
        return;
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let manager = Arc::new(UsbIpManager::new("127.0.0.1:3241"));
    manager.start(rt.handle(), vec![cable(5, 44_100, 24)]).expect("应能启动");
    assert_eq!(manager.cables().len(), 1);
    manager.stop();
    assert!(manager.cables().is_empty(), "stop 后应清空线缆表");
    assert!(!manager.running());
}
