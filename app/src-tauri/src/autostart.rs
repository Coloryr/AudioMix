//! 开机自启（HKCU Run 注册表键，可附加 --headless 参数）。

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "AudioMix";

pub fn get() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey_with_flags(RUN_KEY, KEY_READ)
        .and_then(|k| k.get_value::<String, _>(VALUE_NAME))
        .map(|_| true)
        .unwrap_or(false)
}

pub fn set(enabled: bool, headless: bool) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
        .map_err(|e| format!("打开注册表失败: {e}"))?;
    if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("获取程序路径失败: {e}"))?;
        let mut value = format!("\"{}\"", exe.display());
        if headless {
            value.push_str(" --headless");
        }
        key.set_value(VALUE_NAME, &value)
            .map_err(|e| format!("写入注册表失败: {e}"))?;
    } else {
        // 不存在时忽略错误
        let _ = key.delete_value(VALUE_NAME);
    }
    Ok(())
}
