//! AudioMix — Tauri 壳。
//!
//! 引擎运行在 Rust 后端进程，webview 仅是控制面：
//! 关闭窗口 = 隐藏到托盘；`--headless` 启动时不创建窗口。

// 桌面应用：启动时不要弹出命令行窗口（日志走「设置」页右侧面板 + 落盘文件）
#![windows_subsystem = "windows"]

mod autostart;
mod commands;
mod config;
mod logbuf;
mod state;
mod tray;

use std::sync::Arc;

use audiomix_backend_windows::usbip::{cable_configs, UsbIpBackend, UsbIpManager};
use audiomix_backend_windows::WindowsBackend;
use audiomix_core::{CompositeBackend, Engine};
use tauri::{Manager, State, WindowEvent};

fn main() {
    let headless = std::env::args().any(|a| a == "--headless");

    // 日志同时输出到 stdout 和内存缓冲（「设置」页右侧的日志面板按 seq 增量拉取）
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,audiomix=debug".into()),
        )
        .with_ansi(false)
        .with_writer(logbuf::make_writer())
        .init();

    // 提权 broker 模式：主进程经一次 UAC 把自己再启动一份（`--usbip-broker <addr> <token>`），
    // 本进程常驻后台执行 usbip attach/detach（已提权），主进程退出即随之退出。
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--usbip-broker") {
        let addr = args.get(i + 1).cloned().unwrap_or_default();
        let token = args.get(i + 2).cloned().unwrap_or_default();
        if let Err(e) = audiomix_backend_windows::usbip::broker::run_broker(&addr, &token) {
            tracing::error!("USB/IP 提权代理异常退出: {e}");
            std::process::exit(1);
        }
        return;
    }

    tauri::Builder::default()
        .setup(move |app| {
            let handle = app.handle().clone();

            // 加载配置 + 启动引擎
            let settings = config::load(&handle);

            // 内置 USB/IP 服务器（虚拟声卡）：先建管理器，把线缆注册表共享给后端适配器
            let usbip = Arc::new(UsbIpManager::new(settings.settings.usbip.bind.clone()));
            let registry = usbip.registry();
            let usbip_cfg = settings.settings.usbip.clone();

            // 引擎初始化涉及 WASAPI/COM（MTA）；而主窗口创建要求主线程保持 STA
            // （OleInitialize），因此引擎初始化放到独立线程执行，避免污染主线程 COM 模式
            let engine = {
                let backend: Arc<dyn audiomix_core::AudioBackend> =
                    Arc::new(CompositeBackend::new(vec![
                        Arc::new(WindowsBackend::new()),
                        Arc::new(UsbIpBackend::new(registry)),
                    ]));
                let graph = settings.graph.clone();
                let resample_quality = settings.settings.resample_quality;
                let edge_buffer_ms = settings.settings.edge_buffer_ms;
                std::thread::spawn(move || -> Result<_, String> {
                    let engine =
                        Engine::new(backend).map_err(|e| format!("音频引擎初始化失败: {e}"))?;
                    engine.set_resample_quality(resample_quality);
                    engine.set_edge_buffer_ms(edge_buffer_ms);
                    if let Err(e) = engine.apply_graph(graph) {
                        // 设备缺失等情况不阻断启动，引擎会跳过不可用项
                        tracing::warn!("应用已保存的混音图时出现问题: {e}");
                    }
                    Ok(engine)
                })
                .join()
                .map_err(|_| String::from("引擎初始化线程异常退出"))??
            };
            app.manage(state::AppState::new(engine, settings, usbip.clone()));

            // 虚拟声卡服务器（配置里启用时随应用启动）
            if usbip_cfg.enabled {
                match usbip.start(
                    tauri::async_runtime::handle().inner(),
                    cable_configs(&usbip_cfg),
                ) {
                    Ok(()) => tracing::info!("USB/IP 虚拟声卡服务器已随应用启动"),
                    Err(e) => tracing::warn!("USB/IP 服务器启动失败: {e}"),
                }
            }

            // 控制 API
            let state: State<state::AppState> = handle.state();
            if state.config.lock().settings.control_api.enabled {
                if let Err(e) = commands::restart_control_api(&state) {
                    tracing::warn!("控制 API 启动失败: {e}");
                }
            }

            // 托盘
            tray::setup(&handle)?;

            // 默认设备守护：虚拟线路接入时 Windows 会把系统默认播放/录音抢过去
            // （实测接入后几秒才发生），这里常驻轻量轮询，一旦发现默认设备变成
            // 虚拟线路就恢复成用户选的物理设备。
            commands::spawn_default_device_guard(handle.clone());

            // 启动时自动恢复线缆附加：vhci 端口还挂着上次的设备就不动（不弹 UAC），
            // 端口空了（如重启过电脑）才自动提权 attach 一次。放后台线程，不阻塞窗口创建。
            if usbip_cfg.enabled && !usbip_cfg.cables.is_empty() {
                let auto_handle = handle.clone();
                std::thread::spawn(move || {
                    // 给 USB/IP 服务器一点就绪时间
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    commands::auto_attach_if_needed(&auto_handle);
                });
            }

            // 主窗口（headless 模式不创建）
            if headless {
                tracing::info!("以 headless 模式启动（无主窗口）");
            } else {
                tray::create_main_window(&handle)?;
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let state: State<state::AppState> = app.state();
                if state.config.lock().settings.close_to_tray {
                    // 关闭 = 隐藏到托盘，引擎继续运行
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_devices,
            commands::refresh_devices,
            commands::set_default_device,
            commands::get_device_volume,
            commands::set_device_volume,
            commands::get_device_mute,
            commands::set_device_mute,
            commands::get_graph,
            commands::apply_graph,
            commands::get_mixer_layout,
            commands::set_mixer_layout,
            commands::set_route_gain,
            commands::set_route_muted,
            commands::set_processor_params,
            commands::set_sink_volume,
            commands::subscribe_levels,
            commands::unsubscribe_levels,
            commands::get_stats,
            commands::get_settings,
            commands::update_settings,
            commands::get_control_api_status,
            commands::set_control_api_enabled,
            commands::get_autostart,
            commands::set_autostart,
            commands::usbip_status,
            commands::usbip_set_cables,
            commands::usbip_attach_all,
            commands::usbip_detach_all,
            commands::usbip_install_driver,
            commands::get_logs,
            commands::clear_logs,
            commands::open_main_window,
            commands::quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("AudioMix 启动失败");
}
