//! WASAPI 设备枚举与格式查询（MMDevice API）。

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::IAudioClient;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

use audiomix_core::error::{Error, Result};
use audiomix_core::model::{looks_virtual, DeviceInfo, DeviceKind};

/// 初始化当前线程的 COM（MTA）；已初始化时的错误码直接忽略。
pub fn com_init() {
    unsafe {
        // 已初始化时返回 RPC_E_CHANGED_MODE 等，忽略即可
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// 每个线程独立枚举（COM apartment 限制），调用方无需预先 CoInitialize。
pub fn enumerate_devices() -> Result<Vec<DeviceInfo>> {
    com_init();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(Error::backend)?;
        let mut out = Vec::new();

        let default_render = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .ok()
            .and_then(|d| device_id_of(&d).ok());
        let default_capture = enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .ok()
            .and_then(|d| device_id_of(&d).ok());

        for (flow, kind, default_id) in [
            (eRender, DeviceKind::Output, default_render),
            (eCapture, DeviceKind::Input, default_capture),
        ] {
            let coll = enumerator
                .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
                .map_err(Error::backend)?;
            let count = coll.GetCount().map_err(Error::backend)?;
            for i in 0..count {
                let Ok(dev) = coll.Item(i) else { continue };
                let Some(id) = device_id_of(&dev).ok() else {
                    continue;
                };
                let name = device_name_of(&dev).unwrap_or_else(|_| id.clone());
                let (ch, rate) = mix_format_of(&dev).unwrap_or((2, 48000));
                out.push(DeviceInfo {
                    is_default: default_id.as_deref() == Some(id.as_str()),
                    is_virtual: looks_virtual(&name),
                    id,
                    name,
                    kind,
                    channels: ch,
                    sample_rate: rate,
                });
            }
        }
        Ok(out)
    }
}

/// 当前默认端点是否为「虚拟声卡」设备（按友好名判断）。
///
/// 轻量：只查一个默认端点 + 读友好名，不枚举全部端点；而且不依赖设备缓存
/// ——虚拟线路每次重连都会换端点 id，用缓存判断会漏。
pub fn default_endpoint_is_virtual(render: bool) -> bool {
    com_init();
    unsafe {
        let Ok(enumerator) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return false;
        };
        let flow = if render { eRender } else { eCapture };
        let Ok(dev) = enumerator.GetDefaultAudioEndpoint(flow, eConsole) else {
            return false;
        };
        let name = device_name_of(&dev).unwrap_or_default();
        looks_virtual(&name)
    }
}

/// 读取端点 id（调用方负责 COM 已初始化）。
unsafe fn device_id_of(dev: &IMMDevice) -> Result<String> {
    let pwstr = dev.GetId().map_err(Error::backend)?;
    let s = pwstr.to_string().unwrap_or_default();
    CoTaskMemFree(Some(pwstr.as_ptr() as *const _));
    Ok(s)
}

unsafe fn device_name_of(dev: &IMMDevice) -> Result<String> {
    let store: IPropertyStore = dev.OpenPropertyStore(STGM_READ).map_err(Error::backend)?;
    let pv = store
        .GetValue(&PKEY_Device_FriendlyName)
        .map_err(Error::backend)?;
    Ok(propvariant_string(&pv).unwrap_or_default())
}

unsafe fn propvariant_string(pv: &PROPVARIANT) -> Option<String> {
    if pv.Anonymous.Anonymous.vt == VT_LPWSTR {
        let p = pv.Anonymous.Anonymous.Anonymous.pwszVal;
        if !p.is_null() {
            return p.to_string().ok();
        }
    }
    None
}

unsafe fn mix_format_of(dev: &IMMDevice) -> Option<(u16, u32)> {
    let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).ok()?;
    let fmt = client.GetMixFormat().ok()?;
    if fmt.is_null() {
        return Some((2, 48000));
    }
    // WAVEFORMATEX 为 packed 结构，字段用 read_unaligned
    let ch = std::ptr::addr_of!((*fmt).nChannels).read_unaligned();
    let rate = std::ptr::addr_of!((*fmt).nSamplesPerSec).read_unaligned();
    CoTaskMemFree(Some(fmt as *const _));
    Some((ch, rate))
}

/// 按设备 id 打开并查询其共享引擎格式（capture/render 启动时用于报告 StreamInfo）。
pub fn mix_format_of_by_id(device_id: &str) -> Result<audiomix_core::backend::StreamInfo> {
    com_init();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(Error::backend)?;
        let id = windows::core::HSTRING::from(device_id);
        let dev = enumerator
            .GetDevice(windows::core::PCWSTR(id.as_ptr()))
            .map_err(|_| Error::DeviceNotFound(device_id.to_string()))?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(Error::backend)?;
        let fmt = client.GetMixFormat().map_err(Error::backend)?;
        if fmt.is_null() {
            return Err(Error::Backend("GetMixFormat 返回空格式".into()));
        }
        let bits = std::ptr::addr_of!((*fmt).wBitsPerSample).read_unaligned();
        if bits != 32 {
            return Err(Error::Backend(format!("不支持的设备格式: bits={bits}")));
        }
        let ch = std::ptr::addr_of!((*fmt).nChannels).read_unaligned();
        let rate = std::ptr::addr_of!((*fmt).nSamplesPerSec).read_unaligned();
        CoTaskMemFree(Some(fmt as *const _));
        Ok(audiomix_core::backend::StreamInfo {
            sample_rate: rate,
            channels: ch,
        })
    }
}
