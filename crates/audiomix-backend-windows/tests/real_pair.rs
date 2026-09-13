//! 真机配对测试：验证虚拟声卡驱动的"输入↔输出"拷贝语义——
//! 向虚拟输出（speaker）播放信号，应能从配对的虚拟输入（mic）采集到同一信号。
//!
//! 前置条件：驱动已安装且音频端点已出现（装完驱动需重启音频服务或重启电脑）。
//! 运行：`cargo test -p audiomix-backend-windows --test real_pair -- --ignored --nocapture`

use std::sync::{Arc, Mutex};
use std::time::Duration;

use audiomix_backend_windows::WindowsBackend;
use audiomix_core::backend::{AudioBackend, CaptureCallback, RenderCallback};
use audiomix_core::model::{DeviceKind, DeviceInfo};

fn find_virtual(devices: &[DeviceInfo], kind: DeviceKind) -> Option<DeviceInfo> {
    devices
        .iter()
        .find(|d| d.kind == kind && d.is_virtual)
        .cloned()
}

#[test]
#[ignore = "需要已安装虚拟声卡驱动且端点可见的真机环境"]
fn virtual_pair_copies_render_to_capture() {
    let backend = WindowsBackend::new();
    let devices = backend.enumerate_devices().expect("设备枚举失败");

    let out = find_virtual(&devices, DeviceKind::Output)
        .expect("未找到虚拟输出设备（驱动已装但端点未出现？请重启音频服务后重试）");
    let input = find_virtual(&devices, DeviceKind::Input).expect("未找到虚拟输入设备");
    println!("配对: 输出 \"{}\" <-> 输入 \"{}\"", out.name, input.name);

    // ---- 渲染端：向虚拟输出播放 DC 0.4 ----
    const SIGNAL: f32 = 0.4;
    let render = backend
        .start_render(&out.id, Box::new(move |buf: &mut [f32]| {
            buf.fill(SIGNAL);
        }) as RenderCallback)
        .expect("打开虚拟输出失败");

    // ---- 采集端：从虚拟输入采集，累积到 buffer ----
    let captured: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let reader = captured.clone();
    let capture = backend
        .start_capture(
            &input.id,
            Box::new(move |data: &[f32]| {
                reader.lock().unwrap().extend_from_slice(data);
            }) as CaptureCallback,
        )
        .expect("打开虚拟输入失败");

    // ---- 采 1.5 秒 ----
    std::thread::sleep(Duration::from_millis(1500));

    // 先停渲染（避免停采集期间继续写入），再停采集（Drop 会 join 线程）
    drop(render);
    std::thread::sleep(Duration::from_millis(100));
    drop(capture);

    let data = captured.lock().unwrap();
    let n = data.len();
    assert!(n > 48000, "采集样本过少: {n}");
    // 跳过开头 20%（可能包含流启动时的静音残留）
    let skip = n / 5;
    let seg = &data[skip..];
    let mean = seg.iter().sum::<f32>() / seg.len() as f32;
    let peak = seg.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    println!("采集 {n} 样本, 段均值 {mean:.4}, 段峰值 {peak:.4}");
    assert!(
        (mean - SIGNAL).abs() < 0.05,
        "虚拟输入均值 {mean:.4} 应接近渲染信号 {SIGNAL}（配对拷贝失败？）"
    );
}
