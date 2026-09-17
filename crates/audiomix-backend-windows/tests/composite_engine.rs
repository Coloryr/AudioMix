//! 接线级集成测试：`main.rs` 里「复合后端 = WASAPI + USB/IP 虚拟声卡」的组装方式。
//!
//! 覆盖单元测试覆盖不到的一环：真实 `WindowsBackend` 与真实 `UsbIpBackend`
//! 经 `CompositeBackend` 合并后，引擎能看到物理设备**与**虚拟线缆设备，
//! 且线缆的采样率/位深/名称取自配置。
//!
//! USB/IP 服务器绑在 `127.0.0.1:0`（随机端口），不干扰真实运行的实例。

use std::sync::Arc;

use audiomix_backend_windows::usbip::{cable_configs, UsbIpBackend, UsbIpManager};
use audiomix_backend_windows::WindowsBackend;
use audiomix_core::model::{UsbIpCableMode, UsbIpCableSettings, UsbIpSettings};
use audiomix_core::{AudioBackend, CompositeBackend, Engine};

fn cable(number: u8, rate: u32, bits: u16, mode: UsbIpCableMode) -> UsbIpCableSettings {
    UsbIpCableSettings {
        number,
        name: String::new(),
        sample_rate: rate,
        bits,
        mode,
        buffer_ms: 250,
    }
}

fn test_settings() -> UsbIpSettings {
    UsbIpSettings {
        enabled: true,
        bind: "127.0.0.1:0".into(),
        cables: vec![
            cable(1, 48_000, 16, UsbIpCableMode::Loopback),
            cable(3, 96_000, 16, UsbIpCableMode::Mixer),
        ],
    }
}

#[test]
fn engine_sees_physical_and_usbip_devices() {
    let settings = test_settings();
    settings.validate().expect("测试配置应合法");

    // 多线程运行时：服务器任务在 worker 线程上跑，不需要 block_on 驱动
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let usbip = UsbIpManager::new("127.0.0.1:0");
    usbip
        .start(rt.handle(), cable_configs(&settings))
        .expect("USB/IP 服务器应能启动");
    let addr = usbip.local_addr().expect("应能查到监听地址");
    assert_ne!(addr.port(), 0);

    let backend: Arc<dyn AudioBackend> = Arc::new(CompositeBackend::new(vec![
        Arc::new(WindowsBackend::new()),
        Arc::new(UsbIpBackend::new(usbip.registry())),
    ]));
    let engine = Engine::new(backend).expect("引擎应能初始化（WASAPI 枚举 + 复合后端）");
    let devices = engine.list_devices();

    // —— 虚拟线缆：每条第 1 对 playback(Input) / capture(Output) ——
    for number in [1u8, 3] {
        let playback = devices
            .iter()
            .find(|d| d.id == format!("usbip://{number}/playback"))
            .unwrap_or_else(|| panic!("缺少 usbip://{number}/playback：{devices:#?}"));
        assert_eq!(playback.kind, audiomix_core::DeviceKind::Input);
        assert!(playback.is_virtual);
        assert_eq!(playback.channels, 2);
        assert!(playback
            .name
            .contains(&format!("Virtual Cable {number:02}")));

        let capture = devices
            .iter()
            .find(|d| d.id == format!("usbip://{number}/capture"))
            .unwrap_or_else(|| panic!("缺少 usbip://{number}/capture"));
        assert_eq!(capture.kind, audiomix_core::DeviceKind::Output);
        assert!(capture.is_virtual);
    }

    // 格式随配置走（内置 UAC1：44.1–96kHz 的 16/24/32bit，176.4/192kHz 只 16bit）
    let c1 = devices
        .iter()
        .find(|d| d.id == "usbip://1/playback")
        .unwrap();
    assert_eq!(c1.sample_rate, 48_000);
    let c3 = devices
        .iter()
        .find(|d| d.id == "usbip://3/capture")
        .unwrap();
    assert_eq!(c3.sample_rate, 96_000);

    // 物理设备（若本机存在）与虚拟设备共存，且 id 不冲突
    let mut ids: Vec<&str> = devices.iter().map(|d| d.id.as_str()).collect();
    let total = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), total, "设备 id 不应重复（复合后端需去重）");
    assert!(total >= 4, "至少 4 个虚拟设备，实际 {total}");

    // 刷新设备后虚拟线缆仍在（路由表重建）
    let again = engine.refresh_devices().expect("刷新设备应成功");
    assert!(again.iter().any(|d| d.id == "usbip://3/playback"));

    // 停止服务器后虚拟设备消失（物理设备不受影响）
    let physical = devices.iter().filter(|d| !d.is_virtual).count();
    usbip.stop();
    let after = engine.refresh_devices().expect("刷新设备应成功");
    assert!(
        !after.iter().any(|d| d.id.starts_with("usbip://")),
        "服务器停止后不应再有虚拟线缆设备"
    );
    assert_eq!(
        after.iter().filter(|d| !d.is_virtual).count(),
        physical,
        "物理设备数量不应因虚拟服务器停止而变化"
    );

    // 复合后端能找到设备归属
    let backend = engine.backend_name();
    assert_eq!(backend, "composite");
}

#[test]
fn composite_backend_routes_streams_by_device_id() {
    let settings = test_settings();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let usbip = UsbIpManager::new("127.0.0.1:0");
    usbip.start(rt.handle(), cable_configs(&settings)).unwrap();

    let composite = CompositeBackend::new(vec![
        Arc::new(WindowsBackend::new()),
        Arc::new(UsbIpBackend::new(usbip.registry())),
    ]);

    // 虚拟设备归属 USB/IP 后端
    assert_eq!(composite.owner_name("usbip://1/playback"), Some("usbip"));
    // 未知设备 → DeviceNotFound（而不是打到错误的子后端）
    assert!(matches!(
        composite.start_capture("usbip://9/playback", Box::new(|_| {})),
        Err(audiomix_core::Error::DeviceNotFound(_))
    ));
    // USB/IP 线缆不支持 loopback 采集（回环由线缆内部完成）
    assert!(composite
        .start_loopback("usbip://1/capture", Box::new(|_| {}))
        .is_err());

    // 采集流能真正跑起来：往线缆回环写入后，源回调应收到数据
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    let stream = composite
        .start_capture(
            "usbip://1/playback",
            Box::new(move |data| {
                let _ = tx.send(data.len());
            }),
        )
        .expect("应能打开虚拟线缆采集流");
    assert_eq!(stream.info.sample_rate, 48_000);
    assert_eq!(stream.info.channels, 2);

    let cable = usbip
        .cables()
        .into_iter()
        .find(|c| c.cfg.number == 1)
        .expect("应有 1 号线缆");
    let samples: Vec<f32> = (0..960).map(|i| (i as f32 * 0.01).sin()).collect();
    cable.play_ring.push(&samples);

    let got = rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .expect("采集线程应在 3s 内取到数据");
    assert!(got > 0, "回调长度应大于 0");
    drop(stream);
    usbip.stop();
}
