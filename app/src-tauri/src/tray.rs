//! 托盘：常驻图标、关闭窗口 = 隐藏到托盘、菜单打开/退出。

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub const MAIN_WINDOW: &str = "main";

pub fn create_main_window(app: &AppHandle) -> tauri::Result<()> {
    if app.get_webview_window(MAIN_WINDOW).is_some() {
        return Ok(());
    }
    WebviewWindowBuilder::new(app, MAIN_WINDOW, WebviewUrl::App("index.html".into()))
        .title("AudioMix")
        .inner_size(1100.0, 780.0)
        .min_inner_size(880.0, 600.0)
        // Windows 上 Tauri 默认拦截 HTML5 拖放（用于原生文件拖入），
        // 会让页面的 dragstart/drop 全部失效；本应用不用原生文件拖放，关掉它。
        .disable_drag_drop_handler()
        // 调试期开着 devtools：F12 可以看控制台报错
        .devtools(true)
        .build()?;
    Ok(())
}

pub fn show_main(app: &AppHandle) {
    match app.get_webview_window(MAIN_WINDOW) {
        Some(w) => {
            let _ = w.show();
            let _ = w.unminimize();
            let _ = w.set_focus();
        }
        None => {
            let _ = create_main_window(app);
        }
    }
}

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "打开 AudioMix", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;

    TrayIconBuilder::with_id("main-tray")
        .icon(
            app.default_window_icon()
                .cloned()
                .expect("缺少应用图标"),
        )
        .tooltip("AudioMix")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id.as_ref() {
            "open" => show_main(app),
            "quit" => {
                let state = app.state::<crate::state::AppState>();
                state.engine.shutdown();
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}
