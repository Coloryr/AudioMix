//! 系统环境探测：测试签名 / 内存完整性（HVCI）。
//!
//! 旧的 MTT 内核虚拟声卡驱动（Virtual-Audio-Driver）已废弃——HVCI 开启时非微软签名的
//! 内核驱动无法加载（错误代码 52）。当前虚拟声卡走 **usbip-win2 + UAC2** 路线
//! （见 [`crate::usbip`]），音频端点由 Windows 自带的 usbaudio2.sys 提供，
//! 本应用不需要安装任何内核驱动。
//!
//! 这里只保留两个环境探测，供 USB/IP 状态页在驱动装不上时给出排查方向。

/// 读取测试签名（testsigning）开关状态。
/// `bcdedit` 需要管理员权限，权限不足时返回 None（调用方按"未知"展示）。
pub fn test_signing_enabled() -> Option<bool> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("bcdedit")
        .args(["/enum", "{current}"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let lower = line.to_lowercase();
        if lower.contains("testsigning") {
            if lower.contains("yes") {
                return Some(true);
            }
            if lower.contains("no") || line.contains("否") {
                return Some(false);
            }
        }
    }
    None
}

/// 读取内存完整性（HVCI）开关状态：开启时只有微软签名的内核驱动能加载。
pub fn hvci_enabled() -> Option<bool> {
    let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SYSTEM\CurrentControlSet\Control\DeviceGuard\Scenarios\HypervisorEnforcedCodeIntegrity")
        .ok()?;
    key.get_value::<u32, _>("Enabled").ok().map(|v| v != 0)
}
