//! 真机测试：需要实际音频设备，默认 `#[ignore]`。
//! 手动运行：`cargo test -p audiomix-backend-windows -- --ignored --nocapture`

use audiomix_backend_windows::WindowsBackend;
use audiomix_core::backend::AudioBackend;

#[test]
#[ignore = "需要真实音频设备与 Windows 环境"]
fn enumerate_real_devices() {
    let backend = WindowsBackend::new();
    let devices = backend.enumerate_devices().expect("设备枚举失败");
    println!("共 {} 个设备：", devices.len());
    for d in &devices {
        println!(
            "  [{:?}] {} (id={}, {}Hz {}ch, default={}, virtual={})",
            d.kind, d.name, d.id, d.sample_rate, d.channels, d.is_default, d.is_virtual
        );
    }
    // 有声卡的机器至少应有默认输出
    assert!(devices.iter().any(|d| d.is_default), "至少应有一个默认设备");
}
