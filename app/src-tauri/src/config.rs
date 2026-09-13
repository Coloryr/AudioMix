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
        Ok(text) => match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("配置解析失败，使用默认配置: {e}");
                GraphSettings::default()
            }
        },
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
