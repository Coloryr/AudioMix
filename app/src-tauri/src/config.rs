//! 配置持久化：app-data 目录下的 config.json。

use std::fs;
use std::path::PathBuf;

use audiomix_core::GraphSettings;
use tauri::Manager;

pub fn config_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_data_dir().ok()?;
    Some(dir.join("config.json"))
}

pub fn load(app: &tauri::AppHandle) -> GraphSettings {
    let Some(path) = config_path(app) else {
        return GraphSettings::default();
    };
    match fs::read_to_string(&path) {
        Ok(text) => {
            let parsed: Result<GraphSettings, _> = serde_json::from_str(&text);
            match parsed {
                Ok(mut c) => {
                    // 内置线路只有 UAC1：把旧配置里超规格的线缆（例如 192k/24bit）降到可用组合，
                    // 而不是让整份配置校验失败、应用起不来。
                    for note in c.settings.usbip.clamp_cables_to_supported() {
                        tracing::warn!("{note}");
                    }
                    c
                }
                Err(e) => {
                    tracing::warn!("配置解析失败，使用默认配置: {e}");
                    GraphSettings::default()
                }
            }
        }
        Err(_) => GraphSettings::default(),
    }
}

pub fn save(app: &tauri::AppHandle, config: &GraphSettings) -> Result<(), String> {
    let Some(path) = config_path(app) else {
        return Err("无法确定配置目录".into());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("写入配置失败: {e}"))?;
    Ok(())
}
