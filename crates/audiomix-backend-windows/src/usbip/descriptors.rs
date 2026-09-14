//! 参数化 **UAC1**（USB Audio Class 1.0）描述符构建器。
//!
//! 每条虚拟线缆按 `{sample_rate, bits, channels=2}` 生成一套完整描述符，
//! Windows 通过 usbip-win2 附加后由内置 `usbaudio.sys` 加载，暴露为标准的
//! 播放 + 录音端点。UAC1 的特征（与 UAC2 的差别）：
//! - 设备描述符类字段全 0，音频类只定义在接口层（无 IAD、无 device qualifier）；
//! - 采样率控制挂在**端点**上（recipient=endpoint），值 3 字节；
//! - 范围查询是分开的 GET_MIN/GET_MAX/GET_RES（0x82/0x83/0x84），不是一次返回整块。
//!
//! # 为什么只有 UAC1（UAC2 已移除）
//!
//! USB/IP 在 Windows 侧走 UDE（USB Device Emulation），等时传输受 Microsoft
//! 的 `ucx01000.sys` 约束：iso 端点的 `bInterval` 必须 ≥4 微帧（服务间隔 ≥1ms），
//! 于是每个端点每毫秒最多 1024 字节。而 UAC2 唯一的优势 —— 亚毫秒服务间隔 ——
//! 恰好是这条通路不允许的（192k/24bit = 1152 B/ms 必然超限；`usbaudio.sys`
//! 另外还会把单包限到 `min(wMaxPacketSize, 1024)`）。详见 TASK.md「传输层天花板」。
//!
//! 所以内置线路固定 **UAC1 + 全速**：
//! - iso 端点 `bInterval=1`（每 1ms 一包），包长 = 每毫秒 PCM 字节数（≤1023）；
//! - 支持矩阵：44.1–96 kHz 的 16/24/32bit；176.4/192 kHz 只给 16bit。
//!
//! 需要更高规格（192k/24bit 等）的线路，由用户自行安装第三方虚拟声卡
//! （VB-CABLE 等）；那些设备会作为普通 Windows 端点出现在混音画布里。

use std::collections::BTreeMap;

/// 线缆 PCM 格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CableFormat {
    pub sample_rate: u32,
    /// 16 / 24 / 32
    pub bits: u16,
    /// 固定立体声
    pub channels: u16,
}

pub const MIN_RATE: u32 = 44_100;
pub const MAX_RATE: u32 = 192_000;
/// 内置线路（UAC1 全速）每毫秒字节上限：USB 1.1 全速等时端点单包 1023 字节
pub const MAX_BYTES_PER_MS: u32 = 1023;
/// 超过这个采样率时内置线路只提供 16-bit
pub const MULTIBIT_MAX_RATE: u32 = 96_000;

impl CableFormat {
    pub fn validate(&self) -> Result<(), String> {
        if !(MIN_RATE..=MAX_RATE).contains(&self.sample_rate) {
            return Err(format!(
                "采样率 {} 超出支持范围 {MIN_RATE}–{MAX_RATE} Hz",
                self.sample_rate
            ));
        }
        if !matches!(self.bits, 16 | 24 | 32) {
            return Err(format!("位深 {} 不受支持（16/24/32）", self.bits));
        }
        if self.channels != 2 {
            return Err(format!("通道数 {} 不受支持（当前固定 2）", self.channels));
        }
        if self.sample_rate > MULTIBIT_MAX_RATE && self.bits != 16 {
            return Err(format!(
                "内置线路在 {} Hz 以上只提供 16-bit（当前 {} Hz/{}-bit）—— \
                 更高规格请自装第三方虚拟声卡（如 VB-CABLE）",
                MULTIBIT_MAX_RATE, self.sample_rate, self.bits
            ));
        }
        if self.frame_bytes() > MAX_BYTES_PER_MS as u16 {
            return Err(format!(
                "{} Hz/{}-bit 每毫秒需要 {} 字节，超过内置线路（UAC1 全速）上限 {} —— \
                 请降低采样率或位深，或自装第三方虚拟声卡",
                self.sample_rate,
                self.bits,
                self.frame_bytes(),
                MAX_BYTES_PER_MS
            ));
        }
        Ok(())
    }

    /// 子槽大小（每样本字节数，UAC1 bSubframeSize）
    pub fn subslot(&self) -> u16 {
        (self.bits / 8) as u16
    }

    /// 一个音频帧（2 通道）的字节数
    fn frame_size(&self) -> u32 {
        self.channels as u32 * self.subslot() as u32
    }

    /// 每 1ms 服务间隔的**标称**负载字节数（向上取整）。
    /// 44.1k 系不是整数字节/毫秒（44.1k/16bit/stereo = 176.4），取整过小会让主机发不出
    /// 足够的包，故向上取整。**注意这只是标称值**：写到端点描述符里必须用
    /// [`Self::fs_wmax_packet`]（按整帧对齐）。
    pub fn frame_bytes(&self) -> u16 {
        let total = self.sample_rate as u64 * self.channels as u64 * self.subslot() as u64;
        ((total + 999) / 1000) as u16
    }

    /// 全速 iso 端点的 `wMaxPacketSize`（bInterval=1 → 每 1ms 一包）。
    ///
    /// **必须按「整数个采样帧」向上取整**，不能直接写标称字节数：44.1k 系每毫秒是
    /// 小数帧（44.1k/24bit/stereo = 264.6 字节 = 44.1 帧），标称向上取整得到 265
    /// = 44 帧 + 1 字节，**不是合法的音频包**。真机实测：这样写出来的渲染端点在
    /// Windows 里拿不到任何设备格式（`GetMixFormat` → `AUDCLNT_E_UNSUPPORTED_FORMAT`，
    /// 注册表里也没有 `PKEY_AudioEngine_DeviceFormat`），而 48k 系（192/576/768 恰好
    /// 是整帧）全部正常。按整帧取整后 44.1k/24 → 45 帧 = 270 字节。
    pub fn fs_wmax_packet(&self) -> u16 {
        let frames_per_ms = (self.sample_rate as u64).div_ceil(1000);
        (frames_per_ms * self.frame_size() as u64).min(u16::MAX as u64) as u16
    }

    /// UAC1（全速）能否承载该格式：单包不能超过 1023 字节（按实际写出的包长判断）
    pub fn fs_supported(&self) -> bool {
        self.fs_wmax_packet() <= MAX_BYTES_PER_MS as u16
    }
}

// —— 描述符类型 ——
const DESC_DEVICE: u8 = 0x01;
const DESC_CONFIG: u8 = 0x02;
const DESC_STRING: u8 = 0x03;
const DESC_INTERFACE: u8 = 0x04;
const DESC_ENDPOINT: u8 = 0x05;
const DESC_QUALIFIER: u8 = 0x06;
const DESC_IAD: u8 = 0x0B;
const DESC_CS_INTERFACE: u8 = 0x24;
const DESC_CS_ENDPOINT: u8 = 0x25;

// —— 音频类常量 ——
const AUDIO_CLASS: u8 = 0x01;
const SUBCLASS_AUDIOCONTROL: u8 = 0x01;
const SUBCLASS_AUDIOSTREAMING: u8 = 0x02;

// —— 类特定描述符子类型 ——
const CS_AC_HEADER: u8 = 0x01;
const CS_INPUT_TERMINAL: u8 = 0x02;
const CS_OUTPUT_TERMINAL: u8 = 0x03;
const CS_FEATURE_UNIT: u8 = 0x06;
const CS_AS_GENERAL: u8 = 0x01;
const CS_FORMAT_TYPE: u8 = 0x02;
const CS_EP_GENERAL: u8 = 0x01;

// —— 实体 ID（与 device.rs 的 ID_*_ENTITY 对应）——
const ID_USB_STREAMING_IT: u8 = 1; // 播放路径：USB streaming → Input Terminal
const ID_FEATURE_PLAY: u8 = 2;
const ID_SPEAKER_OT: u8 = 3;
const ID_MIC_IT: u8 = 4; // 采集路径：Mic → Feature → USB streaming OT
const ID_FEATURE_CAPTURE: u8 = 5;
const ID_USB_STREAMING_OT: u8 = 6;

const VID: u16 = 0xFFFF;
const EP0_MAX_PACKET: u8 = 64;
/// 全速 iso 端点的 bInterval：1 帧 = 1ms 一个包
const FS_ISO_B_INTERVAL: u8 = 1;
/// 全速 iso 单包上限（USB 2.0 §5.8.3）
const FS_MAX_PACKET: u16 = 1023;

/// 一套完整描述符
pub struct Descriptors {
    pub product: String,
    pub serial: String,
    /// 该套描述符对应的 PCM 格式（自检需要：包长 vs 每毫秒需求）
    fmt: CableFormat,
    device: Vec<u8>,
    config: Vec<u8>,
    strings: BTreeMap<u8, Vec<u8>>,
}

impl Descriptors {
    /// 配置里的接口数量（= max(bInterfaceNumber) + 1）。USB/IP 的设备信息帧要用它。
    pub fn num_interfaces(&self) -> u8 {
        let mut max_num = 0u8;
        let cfg = &self.config;
        let mut i = 0usize;
        while i + 1 < cfg.len() {
            let len = cfg[i] as usize;
            if len == 0 {
                break;
            }
            if cfg[i + 1] == DESC_INTERFACE && len >= 9 {
                max_num = max_num.max(cfg[i + 2]);
            }
            i += len;
        }
        max_num + 1
    }

    /// 一个 iso 包代表的服务间隔（微秒）——必须与 `iso_endpoint()` 写出的 bInterval 一致，
    /// 服务端按它给 URB 排完成时刻。全速 bInterval=1 → 1ms。
    pub fn iso_service_micros(&self) -> u64 {
        1000
    }

    /// GET_DESCRIPTOR 分发。全速设备没有 device qualifier（请求它应当 STALL）。
    pub fn get(&self, descriptor_type: u8, index: u8) -> Option<Vec<u8>> {
        match descriptor_type {
            DESC_DEVICE if index == 0 => Some(self.device.clone()),
            DESC_CONFIG if index == 0 => Some(self.config.clone()),
            DESC_STRING => self.strings.get(&index).cloned(),
            // 全速设备不提供 device qualifier（标准做法：STALL，主机据此判定「无另一种速度」）
            DESC_QUALIFIER => None,
            _ => None,
        }
    }

    /// 结构自检：总长一致、逐级描述符链完整、iso 包长符合全速规范
    pub fn validate(&self) -> Result<(), String> {
        if self.device.len() != 18 || self.device[1] != DESC_DEVICE {
            return Err("设备描述符无效".into());
        }
        if self.device[4] != 0x00 || self.device[5] != 0x00 || self.device[6] != 0x00 {
            return Err("UAC1 设备描述符类字段必须全 0".into());
        }
        if !self.fmt.fs_supported() {
            return Err(format!(
                "UAC1（全速）单包 {} 字节超过 {}，请把采样率/位深调低",
                self.fmt.frame_bytes(),
                FS_MAX_PACKET
            ));
        }
        let cfg = &self.config;
        if cfg.len() < 9 || cfg[1] != DESC_CONFIG {
            return Err("配置描述符无效".into());
        }
        let total = u16::from_le_bytes([cfg[2], cfg[3]]) as usize;
        if total != cfg.len() {
            return Err(format!("wTotalLength={total}，实际 {}", cfg.len()));
        }
        // 逐级描述符链必须正好走完
        let mut pos = 0;
        while pos < cfg.len() {
            let len = cfg[pos] as usize;
            if len < 2 || pos + len > cfg.len() {
                return Err(format!("偏移 {pos} 处描述符长度无效"));
            }
            pos += len;
        }
        if cfg.len() > 11 && cfg[9] == 8 && cfg[10] == DESC_IAD {
            return Err("UAC1 不应带 IAD".into());
        }
        // AC 头里内嵌的 wTotalLength 必须等于 AC 块实际长度
        match cfg
            .windows(10)
            .position(|c| c[1] == DESC_CS_INTERFACE && c[2] == CS_AC_HEADER)
        {
            Some(p) => {
                let declared = u16::from_le_bytes([cfg[p + 5], cfg[p + 6]]) as usize;
                let ac_len = uac1_ac_block_length() as usize;
                if declared != ac_len {
                    return Err(format!("AC 头 wTotalLength={declared}，应为 {ac_len}"));
                }
            }
            None => return Err("缺少 AC 接口头描述符".into()),
        }
        // iso 端点：bInterval=1、包长 = 每毫秒字节数、≤1023
        for chunk in cfg.windows(9) {
            if chunk[0] == 9 && chunk[1] == DESC_ENDPOINT && chunk[3] & 0x03 == 0x01 {
                let wmax = u16::from_le_bytes([chunk[4], chunk[5]]);
                let bint = chunk[6];
                let per = wmax & 0x07FF;
                if per == 0 || per > FS_MAX_PACKET {
                    return Err(format!("iso 单包字节数 {per} 非法（1..={FS_MAX_PACKET}）"));
                }
                if wmax >> 11 != 0 {
                    return Err("全速 iso 不应使用多事务位域".into());
                }
                if bint != FS_ISO_B_INTERVAL {
                    return Err(format!("全速端点的 bInterval 应为 1，实际 {bint}"));
                }
                let need = self.fmt.fs_wmax_packet();
                if per != need {
                    return Err(format!("iso 包长 {per}，应为 {need}"));
                }
                if chunk[7] != 0 || chunk[8] != 0 {
                    return Err("全速 iso 的 bRefresh/bSynchAddress 应为 0".into());
                }
            }
        }
        Ok(())
    }
}

/// 为第 `number` 号线缆构建描述符集。
/// `product` 是自定义显示名（空则用 `Virtual Cable NN`），会写进 USB 产品字符串——
/// Windows 声音设置/设备管理器里显示的就是它。
pub fn build(number: u8, product: &str, fmt: &CableFormat) -> Result<Descriptors, String> {
    fmt.validate()?;
    let product = {
        let trimmed = product.trim();
        if trimmed.is_empty() {
            format!("Virtual Cable {number:02}")
        } else {
            trimmed.to_string()
        }
    };
    if product.chars().count() > 64 {
        return Err(format!("线缆 {number} 名称过长（上限 64 字）"));
    }
    let pid = 0xCA00 + number as u16;
    // 序列号决定 Windows 的设备实例 ID：保持稳定，用户改采样率/位深不该变成"新设备"，
    // 否则会丢默认设备设置、重新枚举驱动。
    let serial = format!("AUDIOMIX-VCABLE-{number:03}");

    let mut strings = BTreeMap::new();
    strings.insert(0u8, vec![4, DESC_STRING, 0x09, 0x04]); // en-US
    strings.insert(1u8, string_descriptor("AudioMix"));
    strings.insert(2u8, string_descriptor(&product));
    strings.insert(3u8, string_descriptor(&serial));

    let d = Descriptors {
        product,
        serial,
        fmt: *fmt,
        device: device_descriptor(pid),
        config: uac1_configuration(fmt),
        strings,
    };
    d.validate()?;
    Ok(d)
}

/// UAC1 设备描述符：USB 1.1 全速（bcdUSB=0x0110），
/// bDeviceClass/SubClass/Protocol 全 0（音频类定义在接口层，无 IAD、无 qualifier）
fn device_descriptor(pid: u16) -> Vec<u8> {
    vec![
        18,
        DESC_DEVICE,
        0x10,
        0x01, // bcdUSB = 1.10
        0x00,
        0x00,
        0x00, // 类/子类/协议
        EP0_MAX_PACKET,
        VID as u8,
        (VID >> 8) as u8,
        pid as u8,
        (pid >> 8) as u8,
        0x00,
        0x01, // bcdDevice = 1.00
        1,    // iManufacturer
        2,    // iProduct
        3,    // iSerialNumber
        1,    // bNumConfigurations
    ]
}

/// UAC1（USB Audio 1.0）：设备描述符类字段全 0，音频类只定义在接口层，
/// 采样率控制挂在**端点**上（3 字节值）。Windows 用内置 usbaudio.sys 驱动。
fn uac1_configuration(fmt: &CableFormat) -> Vec<u8> {
    let mut b: Vec<u8> = Vec::with_capacity(256);
    let mut app = |bytes: &[u8]| b.extend_from_slice(bytes);

    // 配置头（wTotalLength 最后回填）：3 个接口
    app(&[9, DESC_CONFIG, 0, 0, 3, 1, 0, 0x80, 50]);

    // —— 接口 0：AudioControl（UAC1 无 IAD、无中断端点）——
    app(&[
        9,
        DESC_INTERFACE,
        0,
        0,
        0,
        AUDIO_CLASS,
        SUBCLASS_AUDIOCONTROL,
        0x00, // UAC1：bInterfaceProtocol = 0
        0,
    ]);
    // CS AC 头（UAC1）：| len | CS_IF | AC_HEADER | bcdADC(2)=0x0100 | wTotalLength(2) |
    //                   bInCollection(1) | baInterfaceNr[bInCollection]
    let ac_total = uac1_ac_block_length();
    app(&[
        10,
        DESC_CS_INTERFACE,
        CS_AC_HEADER,
        0x00,
        0x01, // bcdADC = 1.00
        (ac_total & 0xff) as u8,
        (ac_total >> 8) as u8,
        2, // bInCollection：两个 AS 接口
        1, // baInterfaceNr[0] = 接口 1（播放）
        2, // baInterfaceNr[1] = 接口 2（采集）
    ]);

    // 播放路径：USB streaming IT → Feature Unit → Speaker OT
    app(&uac1_input_terminal(ID_USB_STREAMING_IT, 0x0101));
    app(&uac1_feature_unit(ID_FEATURE_PLAY, ID_USB_STREAMING_IT));
    app(&uac1_output_terminal(ID_SPEAKER_OT, ID_FEATURE_PLAY, 0x0301));

    // 采集路径：Mic IT → Feature Unit → USB streaming OT
    app(&uac1_input_terminal(ID_MIC_IT, 0x0201));
    app(&uac1_feature_unit(ID_FEATURE_CAPTURE, ID_MIC_IT));
    app(&uac1_output_terminal(ID_USB_STREAMING_OT, ID_FEATURE_CAPTURE, 0x0101));

    // —— 接口 1：AudioStreaming 播放（EP 0x01 OUT）——
    app(&[9, DESC_INTERFACE, 1, 0, 0, AUDIO_CLASS, SUBCLASS_AUDIOSTREAMING, 0x00, 0]);
    app(&[9, DESC_INTERFACE, 1, 1, 1, AUDIO_CLASS, SUBCLASS_AUDIOSTREAMING, 0x00, 0]);
    app(&uac1_as_general(ID_USB_STREAMING_IT));
    app(&uac1_type_i_format(fmt));
    app(&iso_endpoint(0x01, 0x09, fmt));
    app(&uac1_cs_iso_endpoint());

    // —— 接口 2：AudioStreaming 采集（EP 0x82 IN）——
    app(&[9, DESC_INTERFACE, 2, 0, 0, AUDIO_CLASS, SUBCLASS_AUDIOSTREAMING, 0x00, 0]);
    app(&[9, DESC_INTERFACE, 2, 1, 1, AUDIO_CLASS, SUBCLASS_AUDIOSTREAMING, 0x00, 0]);
    app(&uac1_as_general(ID_USB_STREAMING_OT));
    app(&uac1_type_i_format(fmt));
    app(&iso_endpoint(0x82, 0x0D, fmt));
    app(&uac1_cs_iso_endpoint());

    let total = b.len() as u16;
    b[2] = (total & 0xff) as u8;
    b[3] = (total >> 8) as u8;
    b
}

/// UAC1 AC 类描述符块总长：AC 头(10) + 2×(IT 12 + FU 10 + OT 9)
fn uac1_ac_block_length() -> u16 {
    10 + 2 * (12 + 10 + 9)
}

/// UAC1 输入终端（12 字节）：无 bCSourceID / bmControls
fn uac1_input_terminal(id: u8, term_type: u16) -> Vec<u8> {
    vec![
        12,
        DESC_CS_INTERFACE,
        CS_INPUT_TERMINAL,
        id,
        term_type as u8,
        (term_type >> 8) as u8,
        0, // bAssocTerminal
        2, // bNrChannels
        0x03,
        0x00, // wChannelConfig = FL|FR
        0,    // iChannelNames
        0,    // iTerminal
    ]
}

/// UAC1 输出终端（9 字节）
fn uac1_output_terminal(id: u8, source: u8, term_type: u16) -> Vec<u8> {
    vec![
        9,
        DESC_CS_INTERFACE,
        CS_OUTPUT_TERMINAL,
        id,
        term_type as u8,
        (term_type >> 8) as u8,
        0,      // bAssocTerminal
        source, // bSourceID
        0,      // iTerminal
    ]
}

/// UAC1 特性单元（bControlSize=1，master + 2 通道，各 1 字节）
fn uac1_feature_unit(id: u8, source: u8) -> Vec<u8> {
    vec![
        10,
        DESC_CS_INTERFACE,
        CS_FEATURE_UNIT,
        id,
        source,
        1,    // bControlSize
        0x03, // master：bit0 Mute + bit1 Volume
        0x00, // 通道 1
        0x00, // 通道 2
        0,    // iFeature
    ]
}

/// UAC1 AS_GENERAL（7 字节）：bTerminalLink + bDelay + wFormatTag
fn uac1_as_general(terminal_link: u8) -> Vec<u8> {
    vec![
        7,
        DESC_CS_INTERFACE,
        CS_AS_GENERAL,
        terminal_link,
        1, // bDelay = 1 帧
        0x01,
        0x00, // wFormatTag = PCM(0x0001)
    ]
}

/// UAC1 FORMAT_TYPE_I（11 字节）：一个离散采样率（3 字节 LE）
fn uac1_type_i_format(fmt: &CableFormat) -> Vec<u8> {
    let rate = fmt.sample_rate;
    vec![
        11,
        DESC_CS_INTERFACE,
        CS_FORMAT_TYPE,
        1, // FORMAT_TYPE_I
        fmt.channels as u8,
        fmt.subslot() as u8,
        fmt.bits as u8,
        1, // bSamFreqType = 1 个离散频率
        (rate & 0xFF) as u8,
        ((rate >> 8) & 0xFF) as u8,
        ((rate >> 16) & 0xFF) as u8,
    ]
}

/// UAC1 类特定等时端点描述符（7 字节）
fn uac1_cs_iso_endpoint() -> Vec<u8> {
    vec![7, DESC_CS_ENDPOINT, CS_EP_GENERAL, 0, 0, 0, 0]
}

/// 全速 iso 端点（9 字节）：bInterval=1（每 1ms 一包），
/// 包长 = 每毫秒 PCM 字节数（参考实现 48k/16bit = 0xC0 = 192）。
fn iso_endpoint(addr: u8, attrs: u8, fmt: &CableFormat) -> Vec<u8> {
    let wmax = fmt.fs_wmax_packet();
    vec![
        9,
        DESC_ENDPOINT,
        addr,
        attrs,
        wmax as u8,
        (wmax >> 8) as u8,
        FS_ISO_B_INTERVAL,
        0, // bRefresh
        0, // bSynchAddress
    ]
}

fn string_descriptor(s: &str) -> Vec<u8> {
    let encoded: Vec<u16> = s.encode_utf16().collect();
    let len = (2 + encoded.len() * 2).min(254);
    let count = (len - 2) / 2;
    let mut b = Vec::with_capacity(len);
    b.push(len as u8);
    b.push(DESC_STRING);
    for v in encoded.into_iter().take(count) {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(rate: u32, bits: u16) -> CableFormat {
        CableFormat { sample_rate: rate, bits, channels: 2 }
    }

    /// 内置支持矩阵：44.1–96 kHz 的 16/24/32bit + 176.4/192 kHz 只 16bit
    const SUPPORTED: &[(u32, u16)] = &[
        (44_100, 16),
        (44_100, 24),
        (44_100, 32),
        (48_000, 16),
        (48_000, 24),
        (48_000, 32),
        (88_200, 16),
        (88_200, 24),
        (88_200, 32),
        (96_000, 16),
        (96_000, 24),
        (96_000, 32),
        (176_400, 16),
        (192_000, 16),
    ];

    #[test]
    fn every_supported_format_builds_and_validates() {
        for &(rate, bits) in SUPPORTED {
            let d = build(1, "", &fmt(rate, bits)).unwrap_or_else(|e| panic!("{rate}/{bits}: {e}"));
            d.validate().unwrap_or_else(|e| panic!("{rate}/{bits} 自检失败: {e}"));
            // 包长 = 每毫秒字节数
            let pos = d
                .config
                .windows(9)
                .position(|c| c[1] == DESC_ENDPOINT && c[2] == 0x01)
                .expect("播放端点存在");
            let wmax = u16::from_le_bytes([d.config[pos + 4], d.config[pos + 5]]);
            assert_eq!(wmax, fmt(rate, bits).fs_wmax_packet(), "{rate}/{bits} 包长不符");
            assert!(wmax <= FS_MAX_PACKET, "{rate}/{bits} 超过全速单包上限");
            assert_eq!(d.config[pos + 6], 1, "{rate}/{bits} bInterval 应为 1");
            // 包长必须是整数个采样帧（真机实测：44.1k 系按标称字节数写会导致 Windows 认不出格式）
            let frame_size = (fmt(rate, bits).channels * fmt(rate, bits).subslot()) as u16;
            assert_eq!(wmax % frame_size, 0, "{rate}/{bits} 包长 {wmax} 不是整帧（帧长 {frame_size}）");
            assert!(wmax >= fmt(rate, bits).frame_bytes(), "{rate}/{bits} 包长小于标称需求");
        }
    }

    #[test]
    fn rejects_formats_beyond_builtin_capability() {
        // 96 kHz 以上只给 16bit
        assert!(build(1, "", &fmt(192_000, 24)).is_err(), "192k/24bit 应被拒绝");
        assert!(build(1, "", &fmt(176_400, 24)).is_err(), "176.4k/24bit 应被拒绝");
        assert!(build(1, "", &fmt(192_000, 32)).is_err(), "192k/32bit 应被拒绝");
        assert!(build(1, "", &fmt(100_000, 24)).is_err(), "100k/24bit 应被拒绝（>96kHz）");
        // 超出全速单包上限
        assert!(fmt(192_000, 24).frame_bytes() > FS_MAX_PACKET, "1152 > 1023");
    }

    #[test]
    fn rejects_out_of_range() {
        assert!(build(1, "", &fmt(8_000, 16)).is_err(), "8kHz 超范围");
        assert!(build(1, "", &fmt(384_000, 16)).is_err());
        assert!(build(1, "", &fmt(48_000, 8)).is_err(), "8bit 不支持");
        assert!(fmt(48_000, 16).validate().is_ok());
    }

    /// UAC1：逐字节对齐参考实现 Virtual-Cables（`src/internal/uac1/descriptors.go`）
    #[test]
    fn uac1_layout_matches_reference_implementation() {
        let d = build(1, "", &fmt(48_000, 16)).expect("UAC1 应能构建");
        d.validate().expect("UAC1 自检应通过");

        // 设备描述符：USB 1.1 全速、类字段全 0、无 qualifier
        assert_eq!(&d.device[2..4], &[0x10, 0x01], "bcdUSB = 0x0110");
        assert_eq!(&d.device[4..7], &[0x00, 0x00, 0x00], "类字段全 0");
        assert!(d.get(DESC_QUALIFIER, 0).is_none(), "全速设备无 qualifier，请求应 STALL");

        let cfg = &d.config;
        // AC 头：10 字节、bcdADC=0x0100、wTotalLength=72、bInCollection=2、baInterfaceNr=[1,2]
        // 布局：配置头(9) + AC 接口描述符(9) + AC 头
        assert_eq!(cfg[9 + 9], 10, "AC 头 bLength");
        assert_eq!(&cfg[18..28], &[10, 0x24, 0x01, 0x00, 0x01, 72, 0, 2, 1, 2], "UAC1 AC 头");
        // 播放 AS：AS_GENERAL(7B) + FORMAT_TYPE_I(11B) + EP + CS_EP
        assert!(cfg.windows(7).any(|w| w == [7, 0x24, 0x01, 1, 1, 0x01, 0x00]), "UAC1 AS_GENERAL");
        assert!(
            cfg.windows(11).any(|w| w == [11, 0x24, 0x02, 1, 2, 2, 16, 1, 0x80, 0xBB, 0x00]),
            "UAC1 FORMAT_TYPE_I（48kHz/16bit 离散）"
        );
        // 端点：全速 bInterval=1，包长 = 每毫秒字节数 192
        assert!(
            cfg.windows(9).any(|w| w == [9, 0x05, 0x01, 0x09, 0xC0, 0x00, 1, 0, 0]),
            "播放端点应等于参考实现 [.. 0x09, 0xC0, 0x00, 1 ..]"
        );
        assert!(
            cfg.windows(9).any(|w| w == [9, 0x05, 0x82, 0x0D, 0xC0, 0x00, 1, 0, 0]),
            "采集端点应等于参考实现（同步 IN）"
        );
        // CS 端点：UAC1 是 7 字节
        assert!(cfg.windows(7).any(|w| w == [7, 0x25, 0x01, 0, 0, 0, 0]), "UAC1 CS 端点");
    }

    #[test]
    fn all_interface_headers_are_uac1() {
        let d = build(1, "", &fmt(96_000, 24)).unwrap();
        let ifaces: Vec<&[u8]> =
            d.config.windows(9).filter(|c| c[0] == 9 && c[1] == DESC_INTERFACE).collect();
        assert_eq!(ifaces.len(), 5, "AC(alt0) + 播放 AS(alt0/alt1) + 录音 AS(alt0/alt1)");
        for iface in &ifaces {
            assert_eq!(iface[5], AUDIO_CLASS, "bInterfaceClass = AUDIO");
            assert_eq!(iface[7], 0x00, "UAC1 的 bInterfaceProtocol = 0");
        }
        assert!(ifaces.iter().filter(|c| c[2] == 0).all(|c| c[6] == SUBCLASS_AUDIOCONTROL));
        assert!(ifaces.iter().filter(|c| c[2] != 0).all(|c| c[6] == SUBCLASS_AUDIOSTREAMING));
        assert_eq!(d.num_interfaces(), 3, "接口 0/1/2");
    }

    #[test]
    fn frame_bytes_rounds_up_for_44100_family() {
        // 44.1k 系不是整数字节/毫秒：标称值向上取整，避免主机发不满
        assert_eq!(fmt(44_100, 16).frame_bytes(), 177); // 176.4
        assert_eq!(fmt(44_100, 24).frame_bytes(), 265); // 264.6
        assert_eq!(fmt(88_200, 32).frame_bytes(), 706); // 705.6
        assert_eq!(fmt(48_000, 16).frame_bytes(), 192); // 整数保持
        assert_eq!(fmt(192_000, 16).frame_bytes(), 768);
    }

    /// 端点包长必须是**整数个采样帧**：44.1k 系每毫秒是小数帧，
    /// 直接写标称字节数（265 = 44 帧 + 1 字节）会让 Windows 拿不到设备格式。
    #[test]
    fn fs_packet_size_is_whole_frames() {
        // 45 帧 × 6 字节 = 270（标称 264.6）
        assert_eq!(fmt(44_100, 24).fs_wmax_packet(), 270);
        assert_eq!(fmt(44_100, 16).fs_wmax_packet(), 180); // 45 × 4
        assert_eq!(fmt(44_100, 32).fs_wmax_packet(), 360); // 45 × 8
        assert_eq!(fmt(176_400, 16).fs_wmax_packet(), 708); // 177 × 4（标称 705.6）
        assert_eq!(fmt(88_200, 24).fs_wmax_packet(), 534); // 89 × 6
        // 48k 系本来就是整帧，不能变
        assert_eq!(fmt(48_000, 16).fs_wmax_packet(), 192);
        assert_eq!(fmt(96_000, 24).fs_wmax_packet(), 576);
        assert_eq!(fmt(192_000, 16).fs_wmax_packet(), 768);
        for &(rate, bits) in SUPPORTED {
            let f = fmt(rate, bits);
            let frame = (f.channels * f.subslot()) as u16;
            assert_eq!(f.fs_wmax_packet() % frame, 0, "{rate}/{bits}");
            assert!(f.fs_wmax_packet() >= f.frame_bytes(), "{rate}/{bits}");
            assert!(f.fs_supported(), "{rate}/{bits} 应可承载");
        }
    }

    #[test]
    fn get_descriptor_dispatch() {
        let d = build(3, "", &fmt(48_000, 16)).unwrap();
        assert_eq!(d.get(DESC_DEVICE, 0).unwrap()[8], 0xFF);
        assert!(d.get(DESC_QUALIFIER, 0).is_none());
        let s = d.get(DESC_STRING, 2).unwrap();
        assert_eq!(s[1], DESC_STRING);
        assert!(d.get(DESC_CONFIG, 1).is_none(), "只有 1 个配置");
        assert!(d.get(DESC_STRING, 9).is_none());
    }

    #[test]
    fn serial_is_stable_per_cable() {
        let a = build(7, "x", &fmt(48_000, 16)).unwrap();
        let b = build(7, "y", &fmt(96_000, 24)).unwrap();
        assert_eq!(a.serial, "AUDIOMIX-VCABLE-007");
        assert_eq!(a.serial, b.serial, "改格式/名称不应换设备实例（否则会丢默认设备设置）");
    }

    #[test]
    fn custom_product_name_lands_in_string_descriptor() {
        let d = build(1, "直播线", &fmt(48_000, 16)).unwrap();
        assert_eq!(d.product, "直播线");
        let s = d.get(DESC_STRING, 2).unwrap();
        assert_eq!(s[1], DESC_STRING);
        // UTF-16LE 编码："直" = 0x76F4
        assert_eq!(u16::from_le_bytes([s[2], s[3]]), '直' as u16);
        // 空白名回退到 Virtual Cable NN
        let d = build(2, "   ", &fmt(48_000, 16)).unwrap();
        assert_eq!(d.product, "Virtual Cable 02");
        // 过长名称被拒绝
        assert!(build(1, &"x".repeat(65), &fmt(48_000, 16)).is_err());
    }

    #[test]
    fn iso_service_interval_is_one_millisecond() {
        for &(rate, bits) in SUPPORTED {
            let d = build(1, "", &fmt(rate, bits)).unwrap();
            assert_eq!(d.iso_service_micros(), 1000, "{rate}/{bits}");
        }
    }
}
