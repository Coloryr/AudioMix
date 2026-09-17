//! 虚拟声卡对管理：程序化创建/枚举/卸载根枚举设备节点（devcon 等价实现）。
//!
//! 驱动包（VirtualAudioDriver.inf）预先 staged 进驱动库后，每个
//! `ROOT\VirtualAudioDriver\NNNN` 设备节点对应一对虚拟端点（speaker + mic）。
//! 节点创建后 KS 接口热注册，AudioEndpointBuilder 即时构建端点——
//! 无需重启音频服务或系统；卸载节点端点即消失。
//!
//! 创建/卸载需要管理员权限（建议：应用以自身 `--install-vpair` /
//! `--uninstall-vpair <instance>` 重新拉起并触发 UAC）。

use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DiInstallDevice, DiUninstallDevice, SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList,
    SetupDiCreateDeviceInfoW, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
    SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiOpenDeviceInfoW,
    SetupDiSetDeviceRegistryPropertyW, DIF_REGISTERDEVICE, DIGCF_ALLCLASSES, DIGCF_PRESENT,
    DIINSTALLDEVICE_FLAGS, HDEVINFO, SETUP_DI_DEVICE_CREATION_FLAGS, SPDRP_HARDWAREID,
    SP_DEVINFO_DATA,
};

use audiomix_core::error::{Error, Result};

/// MEDIA 类 GUID（Sound, video and game controllers）
const MEDIA_CLASS_GUID: GUID = GUID::from_u128(0x4d36e96c_e325_11ce_bfc1_08002be10318);
/// INF models 行声明的硬件 ID（枚举器 ROOT 前缀）
const HWID: &str = "ROOT\\VirtualAudioDriver";
/// 设备 ID 前缀（不含实例号）
const DEVNODE_PREFIX: &str = "ROOT\\VIRTUALAUDIODRIVER\\";

fn err(op: &str, e: windows::core::Error) -> Error {
    Error::Backend(format!("{op}: {e} (0x{:08X})", e.code().0 as u32))
}

/// 当前存在的虚拟声卡设备节点实例 id 列表（无需管理员）
pub fn list_pair_nodes() -> Result<Vec<String>> {
    unsafe {
        let set = SetupDiGetClassDevsW(
            Some(&MEDIA_CLASS_GUID),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_ALLCLASSES,
        )
        .map_err(|e| err("SetupDiGetClassDevs", e))?;
        let mut out = Vec::new();
        for i in 0.. {
            let mut data = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(set, i, &mut data).is_err() {
                break;
            }
            match instance_id_of(set, &data) {
                Ok(id) if is_pair_node(&id) => out.push(id),
                _ => {}
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
        Ok(out)
    }
}

/// 创建一对新的虚拟声卡（speaker + mic），返回新节点实例 id。
/// **需要管理员权限。** 驱动包必须已 staged（见 driver.rs install_driver）。
pub fn create_pair_node() -> Result<String> {
    // 挑一个空闲实例号：现有节点最大编号 + 1
    let existing = list_pair_nodes()?;
    let next = existing
        .iter()
        .filter_map(|id| {
            id.rsplit('\\')
                .next()
                .and_then(|s| s.trim_start_matches('0').parse::<u32>().ok().or(Some(0)))
        })
        .max()
        .map(|n| n + 1)
        .unwrap_or(0);
    let devinst = format!("{}{:04}", DEVNODE_PREFIX, next);

    unsafe {
        let set = SetupDiCreateDeviceInfoList(Some(&MEDIA_CLASS_GUID), None)
            .map_err(|e| err("SetupDiCreateDeviceInfoList", e))?;
        let mut data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        let name = windows::core::HSTRING::from(&devinst);
        let desc = windows::core::HSTRING::from("Virtual Audio Driver Pair");
        SetupDiCreateDeviceInfoW(
            set,
            PCWSTR(name.as_ptr()),
            &MEDIA_CLASS_GUID,
            PCWSTR(desc.as_ptr()),
            None,
            SETUP_DI_DEVICE_CREATION_FLAGS(0), // 显式完整 devinst，不用 GENERATE_ID
            Some(&mut data),
        )
        .map_err(|e| err("SetupDiCreateDeviceInfo", e))?;

        // 设置硬件 ID（REG_MULTI_SZ，双 NUL 结尾）
        let mut hwid: Vec<u16> = HWID.encode_utf16().collect();
        hwid.push(0);
        SetupDiSetDeviceRegistryPropertyW(
            set,
            &mut data,
            SPDRP_HARDWAREID,
            Some(std::slice::from_raw_parts(
                hwid.as_ptr() as *const u8,
                hwid.len() * 2,
            )),
        )
        .map_err(|e| err("SetDeviceRegistryProperty(HARDWAREID)", e))?;

        // 注册设备节点
        SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, Some(&data))
            .map_err(|e| err("DIF_REGISTERDEVICE", e))?;

        // 安装最佳匹配驱动（已在驱动库中的签名包）
        let mut reboot = windows::core::BOOL::default();
        DiInstallDevice(
            None,
            set,
            &data,
            None,
            DIINSTALLDEVICE_FLAGS(0),
            Some(&mut reboot),
        )
        .map_err(|e| err("DiInstallDevice", e))?;

        let id = instance_id_of(set, &data)?;
        let _ = SetupDiDestroyDeviceInfoList(set);
        Ok(id)
    }
}

/// 卸载一对虚拟声卡（按实例 id）。**需要管理员权限。**
pub fn remove_pair_node(instance_id: &str) -> Result<()> {
    unsafe {
        let set = SetupDiCreateDeviceInfoList(Some(&MEDIA_CLASS_GUID), None)
            .map_err(|e| err("SetupDiCreateDeviceInfoList", e))?;
        let mut data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        let id_h = windows::core::HSTRING::from(instance_id);
        SetupDiOpenDeviceInfoW(set, PCWSTR(id_h.as_ptr()), None, 0, Some(&mut data))
            .map_err(|e| err("SetupDiOpenDeviceInfo", e))?;
        let mut reboot = windows::core::BOOL::default();
        DiUninstallDevice(
            windows::Win32::Foundation::HWND::default(),
            set,
            &data,
            0,
            Some(&mut reboot),
        )
        .map_err(|e| err("DiUninstallDevice", e))?;
        let _ = SetupDiDestroyDeviceInfoList(set);
        Ok(())
    }
}

unsafe fn instance_id_of(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> Result<String> {
    let mut len = 0u32;
    // 第一次调用返回缓冲区不足错误并同时写入所需长度
    let _ = SetupDiGetDeviceInstanceIdW(set, data, None, Some(&mut len));
    if len == 0 {
        return Err(Error::Backend(
            "GetDeviceInstanceId: 无法获取实例 id 长度".into(),
        ));
    }
    let mut buf = vec![0u16; len as usize];
    SetupDiGetDeviceInstanceIdW(set, data, Some(&mut buf), Some(&mut len))
        .map_err(|e| err("GetDeviceInstanceId", e))?;
    let s = String::from_utf16_lossy(&buf[..len as usize - 1]);
    Ok(s)
}

fn is_pair_node(instance_id: &str) -> bool {
    instance_id.len() > DEVNODE_PREFIX.len()
        && instance_id[..DEVNODE_PREFIX.len()].eq_ignore_ascii_case(DEVNODE_PREFIX)
}
