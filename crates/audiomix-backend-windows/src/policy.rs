//! 切换 Windows 默认音频端点。
//!
//! Windows 没有公开 API 设置「默认播放/录音设备」——只能调用系统自带的未公开 COM
//! 接口 `IPolicyConfig`（CLSID `CPolicyConfigClient`，Vista 起一直可用，Win10/11 同样有效）。
//! 设备 id 就是 WASAPI/MMDevice 端点 id，形如
//! `{0.0.0.00000000}.{bc6f1e4e-...}`（本应用 `DeviceInfo.id` 即此格式）。
//!
//! 默认设备有三种「角色」，Windows 声音设置里改的就是这三者，这里一并设置：
//! eConsole(0) / eMultimedia(1) / eCommunications(2)。

use windows::core::{interface, IUnknown, IUnknown_Vtbl, HRESULT, PCWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

/// IPolicyConfig 的 IID（方法名沿用 Windows SDK 的 vtable 命名，模块级已 allow(non_snake_case)）
#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: IUnknown {
    // 方法顺序必须与系统 vtable 一致（这里只调用 SetDefaultEndpoint，其余只为占位）
    unsafe fn GetMixFormat(&self, device: PCWSTR, format: *mut *mut core::ffi::c_void) -> HRESULT;
    unsafe fn GetDeviceFormat(
        &self,
        device: PCWSTR,
        default: i32,
        format: *mut *mut core::ffi::c_void,
    ) -> HRESULT;
    unsafe fn ResetDeviceFormat(&self, device: PCWSTR) -> HRESULT;
    unsafe fn SetDeviceFormat(
        &self,
        device: PCWSTR,
        endpoint_format: *mut core::ffi::c_void,
        mix_format: *mut core::ffi::c_void,
    ) -> HRESULT;
    unsafe fn GetProcessingPeriod(
        &self,
        device: PCWSTR,
        default: i32,
        period: *mut i64,
        min_period: *mut i64,
    ) -> HRESULT;
    unsafe fn SetProcessingPeriod(&self, device: PCWSTR, period: *mut i64) -> HRESULT;
    unsafe fn GetShareMode(&self, device: PCWSTR, mode: *mut core::ffi::c_void) -> HRESULT;
    unsafe fn SetShareMode(&self, device: PCWSTR, mode: *mut core::ffi::c_void) -> HRESULT;
    unsafe fn GetPropertyValue(
        &self,
        device: PCWSTR,
        key: *const core::ffi::c_void,
        value: *mut core::ffi::c_void,
    ) -> HRESULT;
    unsafe fn SetPropertyValue(
        &self,
        device: PCWSTR,
        key: *const core::ffi::c_void,
        value: *mut core::ffi::c_void,
    ) -> HRESULT;
    unsafe fn SetDefaultEndpoint(&self, device: PCWSTR, role: u32) -> HRESULT;
    unsafe fn SetEndpointVisibility(&self, device: PCWSTR, visible: i32) -> HRESULT;
}

/// CPolicyConfigClient
const CLSID_POLICY_CONFIG_CLIENT: windows::core::GUID =
    windows::core::GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// 端点角色（与 ERole 一致）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 常规（eConsole）
    Console,
    /// 多媒体（eMultimedia）
    Multimedia,
    /// 通信（eCommunications）
    Communications,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Console, Role::Multimedia, Role::Communications];

    fn as_u32(self) -> u32 {
        match self {
            Role::Console => 0,
            Role::Multimedia => 1,
            Role::Communications => 2,
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 把 `device_id` 设为默认端点（三种角色全设）。
///
/// `device_id` 必须是 MMDevice 端点 id（即 `DeviceInfo.id`）。
pub fn set_default_endpoint(device_id: &str) -> Result<(), String> {
    set_default_endpoint_roles(device_id, &Role::ALL)
}

pub fn set_default_endpoint_roles(device_id: &str, roles: &[Role]) -> Result<(), String> {
    if device_id.trim().is_empty() {
        return Err("设备 id 为空".into());
    }
    unsafe {
        // 失败（已初始化过）不是错误，忽略返回值
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let client: IPolicyConfig = CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)
            .map_err(|e| format!("创建 IPolicyConfig 失败（系统不支持？）: {e}"))?;
        let wide = to_wide(device_id);
        for role in roles {
            client
                .SetDefaultEndpoint(PCWSTR(wide.as_ptr()), role.as_u32())
                .ok()
                .map_err(|e| format!("设置默认设备失败（role={role:?}）: {e}"))?;
        }
    }
    Ok(())
}

// ---------- 端点（Windows 系统）音量 ----------

/// 打开某个端点的音量控制接口
unsafe fn endpoint_volume(device_id: &str) -> Result<windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume, String> {
    use windows::core::HSTRING;
    use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator};

    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        .map_err(|e| format!("创建音频设备枚举器失败: {e}"))?;
    let device = enumerator
        .GetDevice(&HSTRING::from(device_id))
        .map_err(|e| format!("找不到设备 {device_id}: {e}"))?;
    device
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| format!("打开端点音量控制失败: {e}"))
}

/// 当前默认端点的 id（`render=true` 查播放，false 查录音；角色用 eConsole）。
///
/// 轻量查询：用于「默认设备守护」高频轮询，不必枚举全部端点。
pub fn default_endpoint_id(render: bool) -> Option<String> {
    use windows::Win32::Media::Audio::{eCapture, eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let flow = if render { eRender } else { eCapture };
        let dev = enumerator.GetDefaultAudioEndpoint(flow, eConsole).ok()?;
        let id = dev.GetId().ok()?;
        let s = id.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(id.as_ptr() as *const _));
        s.filter(|s| !s.is_empty())
    }
}

/// 读取端点的**系统音量**（0.0..=1.0）
pub fn get_endpoint_volume(device_id: &str) -> Result<f32, String> {
    unsafe {
        let vol = endpoint_volume(device_id)?;
        vol.GetMasterVolumeLevelScalar()
            .map_err(|e| format!("读取系统音量失败: {e}"))
    }
}

/// 端点的**硬件**支持位：bit0 = 硬件音量、bit1 = 硬件静音。
///
/// 用来判断 UAC2 的 Feature Unit 有没有被 Windows 认出来 —— 没有硬件音量时
/// 「声音设置 → 设备属性 → 级别」不会出现设备滑块（只有系统软件音量）。
pub fn hardware_support(device_id: &str) -> Result<u32, String> {
    unsafe {
        let vol = endpoint_volume(device_id)?;
        vol.QueryHardwareSupport()
            .map(|m| m as u32)
            .map_err(|e| format!("查询硬件音量支持失败: {e}"))
    }
}

/// 硬件音量范围（min, max, increment），单位 dB
pub fn volume_range_db(device_id: &str) -> Result<(f32, f32, f32), String> {
    unsafe {
        let vol = endpoint_volume(device_id)?;
        let (mut min, mut max, mut inc) = (0f32, 0f32, 0f32);
        vol.GetVolumeRange(&mut min, &mut max, &mut inc)
            .map_err(|e| format!("读取音量范围失败: {e}"))?;
        Ok((min, max, inc))
    }
}

/// 设置端点的**系统音量**（0.0..=1.0），立刻影响该设备上所有声音
pub fn set_endpoint_volume(device_id: &str, level: f32) -> Result<(), String> {
    unsafe {
        let vol = endpoint_volume(device_id)?;
        vol.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null())
            .map_err(|e| format!("设置系统音量失败: {e}"))
    }
}

/// 读取端点静音状态
pub fn get_endpoint_mute(device_id: &str) -> Result<bool, String> {
    unsafe {
        let vol = endpoint_volume(device_id)?;
        vol.GetMute()
            .map_err(|e| format!("读取静音状态失败: {e}"))
            .map(|b| b.as_bool())
    }
}

/// 设置端点静音
pub fn set_endpoint_mute(device_id: &str, mute: bool) -> Result<(), String> {
    unsafe {
        let vol = endpoint_volume(device_id)?;
        vol.SetMute(mute, std::ptr::null())
            .map_err(|e| format!("设置静音失败: {e}"))
    }
}

/// 该端点当前是否为默认设备（三种角色任一，主要给诊断/测试用）
pub fn is_default_endpoint(device_id: &str) -> bool {    use windows::Win32::Media::Audio::{
        eConsole, eMultimedia, eRender, IMMDeviceEnumerator, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::CoCreateInstance as CoCreate;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(enumerator): Result<IMMDeviceEnumerator, _> =
            CoCreate(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return false;
        };
        for role in [eConsole, eMultimedia] {
            if let Ok(dev) = enumerator.GetDefaultAudioEndpoint(eRender, role) {
                if let Ok(id) = dev.GetId() {
                    let s = id.to_string().unwrap_or_default();
                    if s.eq_ignore_ascii_case(device_id) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_match_erole() {
        assert_eq!(Role::Console.as_u32(), 0);
        assert_eq!(Role::Multimedia.as_u32(), 1);
        assert_eq!(Role::Communications.as_u32(), 2);
        assert_eq!(Role::ALL.len(), 3);
    }

    #[test]
    fn empty_device_id_is_rejected() {
        assert!(set_default_endpoint("").is_err());
        assert!(set_default_endpoint("   ").is_err());
    }

    #[test]
    fn wide_conversion_is_null_terminated() {
        let w = to_wide("ab");
        assert_eq!(w, vec![0x61, 0x62, 0x00]);
    }

    #[test]
    #[ignore = "会调用真实系统 COM，需要默认播放设备"]
    fn endpoint_volume_roundtrip_on_default_output() {
        // 真机：读取默认输出的系统音量，再原样写回去（无副作用），验证 IAudioEndpointVolume 可用
        use windows::Win32::Media::Audio::{
            eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator,
        };
        let current = unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).expect("枚举器");
            let dev = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).expect("默认输出");
            dev.GetId().expect("端点 id").to_string().expect("转字符串")
        };
        let level = get_endpoint_volume(&current).expect("应能读到系统音量");
        assert!((0.0..=1.0).contains(&level), "音量应在 0..1：{level}");
        set_endpoint_volume(&current, level).expect("应能设置系统音量");
        let after = get_endpoint_volume(&current).expect("再读一次");
        assert!((after - level).abs() < 0.02, "写回后应基本一致：{level} vs {after}");
        let mute = get_endpoint_mute(&current).expect("应能读静音状态");
        set_endpoint_mute(&current, mute).expect("写回静音状态");
    }

    /// 真机验证 `IPolicyConfig` 可用：把默认输出设备**重新设成它自己**（无副作用），
    /// 需要真实声卡，因此默认 `--ignored` 跳过。
    #[test]
    #[ignore = "会调用真实系统 COM，需要默认播放设备"]
    fn set_default_endpoint_roundtrip_on_current_default() {
        use windows::Win32::Media::Audio::{
            eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator,
        };
        let current = unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).expect("枚举器");
            let dev = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).expect("默认输出");
            dev.GetId().expect("端点 id").to_string().expect("转字符串")
        };
        println!("当前默认输出: {current}");
        set_default_endpoint(&current).expect("IPolicyConfig 应可用");
        assert!(is_default_endpoint(&current), "设置后应仍是默认设备");
    }
}
