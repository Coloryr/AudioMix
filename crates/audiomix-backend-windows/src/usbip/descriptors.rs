//! 参数化 **UAC2**（USB Audio Class 2.0）描述符构建器。
//!
//! 每条虚拟线缆按 `{sample_rate, bits, channels=2}` 生成一套完整描述符，
//! Windows 通过 usbip-win2 附加后由内置 usbaudio2.sys 加载，暴露为标准的
//! 播放 + 录音端点。采样率/位深因此可在 44.1–192kHz / 16-24-32bit 间配置。
//!
//! 与 Virtual-Cables 的 UAC1 描述符（固定 48k/16bit、full-speed）关键差异：
//! - bcdUSB 0x0200，设备类 0xEF/0x02/0x01（IAD 复合设备）+ device qualifier；
//! - AC 头 bcdADC 0x0200，**必需 Clock Source 实体**（采样率控制挂在其上）；
//! - AS.general **16 字节**（UAC2 布局）+ UAC2 Type I 格式描述符（6B）；
//! - 接口/IAD 的 protocol 必须是 **0x20**（IP version 2.00），否则 Windows
//!   根本不会加载 usbaudio2.sys（见下面 `PROTOCOL_IP_VERSION_02_00` 注释）；
//! - 高速 iso：bInterval=4（8 微帧 = 1ms 服务间隔），
//!   wMaxPacketSize 用「单事务字节数 + 每微帧事务数」位域编码。

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
        Ok(())
    }

    /// 子槽大小（每样本字节数，UAC2 bSubslotSize）
    pub fn subslot(&self) -> u16 {
        (self.bits / 8) as u16
    }

    /// 每 1ms 服务间隔的负载字节数（**向上取整**）。
    /// 44.1k 系不是整数字节/毫秒（44.1k/16bit/stereo = 176.4），取整过小会让
    /// 主机发不出足够的包，故向上取整。
    pub fn frame_bytes(&self) -> u16 {
        let total = self.sample_rate as u64 * self.channels as u64 * self.subslot() as u64;
        ((total + 999) / 1000) as u16
    }

    /// 一个音频帧的字节数（2 通道 × 子槽）
    fn frame_size(&self) -> u32 {
        self.channels as u32 * self.subslot() as u32
    }

    /// 高速下 bInterval=B 表示每 2^(B-1) 个微帧（125µs）一次服务间隔
    pub fn service_interval_micros(bint: u8) -> u32 {
        (1u32 << (bint.clamp(1, 4) - 1)) * 125
    }

    /// 某个 bInterval 下，一个服务间隔必须容纳的字节数。
    ///
    /// **必须多留 1 个音频帧**：Linux `f_uac2.c` 里写得很明确 ——
    /// “Win10 requires max packet size + 1 frame”，否则 Windows 的 usbaudio2
    /// 直接判该端点无法承载这个格式（表现为 `GetMixFormat` →
    /// `AUDCLNT_E_UNSUPPORTED_FORMAT`，属性页「级别」空白，音频引擎反复重试）。
    pub fn packet_bytes_at(&self, bint: u8) -> u32 {
        let micros = Self::service_interval_micros(bint) as u64;
        // 每毫秒 1000µs → 每秒 1_000_000/micros 个服务间隔
        let per_sec = 1_000_000u64 / micros;
        let frames = (self.sample_rate as u64 + per_sec - 1) / per_sec; // 每间隔音频帧数，向上取整
        (frames as u32 + 1) * self.frame_size() // +1 帧余量（Win10 要求）
    }

    /// 全速（UAC1）iso 端点：bInterval=1 → 每 1ms 一包，包长 = 每毫秒字节数（USB 2.0 全速上限 1023）
    pub fn fs_wmax_packet(&self) -> u16 {
        self.frame_bytes().min(1023)
    }

    /// UAC1（全速）能否承载该格式：单包不能超过 1023 字节
    pub fn fs_supported(&self) -> bool {
        self.frame_bytes() <= 1023
    }

    /// 高速 iso 端点的 bInterval：在「单次事务 ≤ 1024 字节」的前提下尽量用长服务间隔
    /// （与 Linux `f_uac2` 的自动选择一致）。192k/32bit/2ch 需 1544B/1ms → 退到 bInterval=3。
    pub fn iso_b_interval(&self) -> u8 {
        for bint in (1u8..=ISO_B_INTERVAL).rev() {
            if self.packet_bytes_at(bint) <= HS_MAX_PACKET as u32 {
                return bint;
            }
        }
        1
    }

    /// 高速 iso 端点 wMaxPacketSize = 一个服务间隔的字节数（≤1024，不用多事务位域）。
    ///
    /// 不再用 USB 2.0 §9.6.6 的 bits 12:11「额外事务机会数」：那套编码配 bInterval>1
    /// 时 Windows 的 USB 音频栈并不接受。改成缩短服务间隔（bInterval 3/2/1）来满足带宽，
    /// 与 Linux `f_uac2.c`（在 Windows 上验证可用）的做法一致。
    pub fn iso_wmax_packet(&self) -> u16 {
        self.packet_bytes_at(self.iso_b_interval())
            .min(HS_MAX_PACKET as u32) as u16
    }

    /// 单次事务字节数（= wMaxPacketSize 本体，不再有 mult 位域）
    pub fn iso_transaction_bytes(&self) -> u16 {
        self.iso_wmax_packet()
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

// —— 音频接口 类/子类/协议 ——
//
// `bInterfaceProtocol` / IAD 的 `bFunctionProtocol` 必须写 **0x20（IP version 2.00）**：
// Windows 自带的 `%WINDIR%\INF\usbaudio2.inf` 只匹配这四组硬件 ID，
// 全部要求 `Prot_20`（实测本机 usbaudio2.inf）：
//   USB\Class_01&SubClass_00&Prot_20   ← 复合设备的 IAD 生成这个（音频功能）
//   USB\Class_01&SubClass_01&Prot_20   ← AudioControl 接口
//   USB\Class_01&SubClass_02&Prot_20   ← AudioStreaming 接口
//   USB\Class_01&SubClass_03&Prot_20   ← MIDIStreaming 接口
// 写成 0x00（UAC1 的取值）会导致 Windows 根本不给设备装 usbaudio2.sys。
const AUDIO_CLASS: u8 = 0x01;
const SUBCLASS_AUDIOCONTROL: u8 = 0x01;
const SUBCLASS_AUDIOSTREAMING: u8 = 0x02;
const PROTOCOL_IP_VERSION_02_00: u8 = 0x20;
/// 音频功能的 IAD 子类：未定义（UAC2 规范 / 量产固件均为 0x00）
const FUNCTION_SUBCLASS_UNDEFINED: u8 = 0x00;

// —— UAC2 类特定子类型 ——
const CS_AC_HEADER: u8 = 0x01;
const CS_INPUT_TERMINAL: u8 = 0x02;
const CS_OUTPUT_TERMINAL: u8 = 0x03;
const CS_FEATURE_UNIT: u8 = 0x06;
const CS_AS_GENERAL: u8 = 0x01;
const CS_FORMAT_TYPE: u8 = 0x02;
const CS_EP_GENERAL: u8 = 0x01;
const CS_CLOCK_SOURCE: u8 = 0x0A;

// —— 实体 ID（描述符内部交叉引用）——
// 编号顺序对齐 Linux f_uac2（Windows usbaudio2 实测可用的那套）
const ID_CLOCK: u8 = 10; // 播放侧时钟（Clock Source）
const ID_USB_STREAMING_IT: u8 = 1; // 播放路径：USB streaming → Input Terminal
const ID_FEATURE_PLAY: u8 = 2;
const ID_SPEAKER_OT: u8 = 3;
const ID_MIC_IT: u8 = 4; // 采集路径：Mic → Feature → USB streaming OT
const ID_FEATURE_CAPTURE: u8 = 5;
const ID_USB_STREAMING_OT: u8 = 6;

/// 实验性本地 VID（Virtual-Cables 同款做法）；PID 按线缆号区分
const VID: u16 = 0xFFFF;
const EP0_MAX_PACKET: u8 = 64;
/// 高速 iso 的 bInterval：2^(4-1) = 8 微帧 = 1ms 服务间隔
/// （UAC1/full-speed 的 bInterval 本身就以 1ms 帧计；高速下用 4 保持同样的 1ms 节拍）
const ISO_B_INTERVAL: u8 = 4;
/// 高速 iso **单次事务**上限（wMaxPacketSize bits 10:0）
pub const HS_MAX_PACKET: u16 = 1024;
/// 每微帧最多 3 次事务（mult 0..2）
const HS_MAX_TRANSACTIONS: u32 = 3;
/// 单个 1ms 服务间隔的容量上限 = 3 × 1024
pub const HS_MAX_SERVICE_BYTES: u32 = HS_MAX_PACKET as u32 * HS_MAX_TRANSACTIONS;
/// wMaxPacketSize 中事务数字段的移位
const WMAX_MULT_SHIFT: u16 = 11;

/// 一套完整描述符
pub struct Descriptors {
    pub product: String,
    pub serial: String,
    /// 该套描述符对应的 PCM 格式（自检需要：iso 服务间隔容量 vs 每毫秒需求）
    fmt: CableFormat,
    /// USB 音频类版本
    protocol: CableProtocol,
    device: Vec<u8>,
    qualifier: Vec<u8>,
    config: Vec<u8>,
    strings: BTreeMap<u8, Vec<u8>>,
}

/// USB 音频类版本：UAC1（usbaudio.sys）/ UAC2（usbaudio2.sys）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CableProtocol {
    #[default]
    Uac1,
    Uac2,
}

impl Descriptors {
    pub fn protocol(&self) -> CableProtocol {
        self.protocol
    }
}

/// 为第 `number` 号线缆构建描述符集。
/// `product` 是自定义显示名（空则用 `Virtual Cable NN`），会写进 USB 产品字符串——
/// Windows 声音设置/设备管理器里显示的就是它。
pub fn build(
    number: u8,
    product: &str,
    fmt: &CableFormat,
    protocol: CableProtocol,
) -> Result<Descriptors, String> {
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
    let serial = format!("AUDIOMIX-VCABLE-{number:03}");
    let pid = 0xCA00 + number as u16;

    let mut strings = BTreeMap::new();
    strings.insert(0u8, vec![4, DESC_STRING, 0x09, 0x04]); // en-US
    strings.insert(1u8, string_descriptor("AudioMix"));
    strings.insert(2u8, string_descriptor(&product));
    strings.insert(3u8, string_descriptor(&serial));

    let d = Descriptors {
        product,
        serial,
        fmt: *fmt,
        protocol,
        device: device_descriptor(pid, protocol),
        qualifier: match protocol {
            // 全速设备不需要 qualifier（参考实现同样不提供）
            CableProtocol::Uac1 => Vec::new(),
            CableProtocol::Uac2 => device_qualifier(),
        },
        config: configuration_descriptor(fmt, protocol),
        strings,
    };
    d.validate()?;
    Ok(d)
}

fn device_descriptor(pid: u16, protocol: CableProtocol) -> Vec<u8> {
    // UAC1：USB 1.1 全速（bcdUSB=0x0110，不需要 device qualifier / BOS），
    //       bDeviceClass/SubClass/Protocol 全 0（音频类定义在接口层，无 IAD）
    // UAC2：USB 2.0 高速 + 0xEF/0x02/0x01（复合设备 + IAD，由 usbaudio2.inf 的 Prot_20 匹配）
    let (bcd_usb_lo, bcd_usb_hi, class, subclass, proto, bcd_dev) = match protocol {
        CableProtocol::Uac1 => (0x10, 0x01, 0x00, 0x00, 0x00, 0x0100u16),
        CableProtocol::Uac2 => (0x00, 0x02, 0xEF, 0x02, 0x01, 0x0100),
    };
    vec![
        18,
        DESC_DEVICE,
        bcd_usb_lo,
        bcd_usb_hi,
        class,
        subclass,
        proto,
        EP0_MAX_PACKET,
        VID as u8,
        (VID >> 8) as u8,
        pid as u8,
        (pid >> 8) as u8,
        bcd_dev as u8,
        (bcd_dev >> 8) as u8,
        1, // iManufacturer
        2, // iProduct
        3, // iSerialNumber
        1, // bNumConfigurations
    ]
}

fn device_qualifier() -> Vec<u8> {
    vec![
        10,
        DESC_QUALIFIER,
        0x00,
        0x02, // bcdUSB 2.0
        0xEF,
        0x02,
        0x01,
        EP0_MAX_PACKET,
        1, // bNumConfigurations
        0, // bReserved
    ]
}

fn configuration_descriptor(fmt: &CableFormat, protocol: CableProtocol) -> Vec<u8> {
    match protocol {
        CableProtocol::Uac1 => uac1_configuration(fmt),
        CableProtocol::Uac2 => uac2_configuration(fmt),
    }
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
    app(&iso_endpoint(0x01, 0x09, fmt, CableProtocol::Uac1));
    app(&uac1_cs_iso_endpoint());

    // —— 接口 2：AudioStreaming 采集（EP 0x82 IN）——
    app(&[9, DESC_INTERFACE, 2, 0, 0, AUDIO_CLASS, SUBCLASS_AUDIOSTREAMING, 0x00, 0]);
    app(&[9, DESC_INTERFACE, 2, 1, 1, AUDIO_CLASS, SUBCLASS_AUDIOSTREAMING, 0x00, 0]);
    app(&uac1_as_general(ID_USB_STREAMING_OT));
    app(&uac1_type_i_format(fmt));
    app(&iso_endpoint(0x82, 0x0D, fmt, CableProtocol::Uac1));
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
        1,    // bDelay = 1 帧
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

fn uac2_configuration(fmt: &CableFormat) -> Vec<u8> {
    let mut b: Vec<u8> = Vec::with_capacity(256);
    let mut app = |bytes: &[u8]| b.extend_from_slice(bytes);

    // 配置头（wTotalLength 最后回填）
    app(&[9, DESC_CONFIG, 0, 0, 3, 1, 0, 0x80, 50]);

    // —— IAD：把 AC + 2×AS 归组为一个音频功能 ——
    // 复合设备的子设备硬件 ID 由这里的 Class/SubClass/Prot 生成：
    //   USB\Class_01&SubClass_00&Prot_20 → usbaudio2.inf 的第一条匹配
    app(&[
        8,
        DESC_IAD,
        0, // bFirstInterface
        3, // bInterfaceCount
        AUDIO_CLASS,
        FUNCTION_SUBCLASS_UNDEFINED,
        PROTOCOL_IP_VERSION_02_00,
        0, // iFunction
    ]);

    // —— 接口 0：AudioControl（1 个中断端点，见 f_uac2：有 Feature Unit 就带中断端点）——
    app(&[
        9,
        DESC_INTERFACE,
        0,
        0,
        1,
        AUDIO_CLASS,
        SUBCLASS_AUDIOCONTROL,
        PROTOCOL_IP_VERSION_02_00,
        0,
    ]);
    // CS AC 头：| len | CS_IF | AC_HEADER | bcdADC(2) | bCategory | wTotalLength(2) | bmControls |
    // bcdADC=0x0200、bCategory=IO_BOX(0x08)、bmControls=0
    let ac_total = ac_block_length();
    app(&[
        9,
        DESC_CS_INTERFACE,
        CS_AC_HEADER,
        0x00,
        0x02,
        0x08,
        (ac_total & 0xff) as u8,
        (ac_total >> 8) as u8,
        0,
    ]);

    // 单个时钟源：MS 文档明确「The driver supports one single clock source only」，
    // 多时钟源需要配 Clock Selector，否则端点解析不到时钟 → 整个 pin 没有可用格式
    app(&clock_source(ID_CLOCK));

    // 播放路径：USB streaming IT → Feature Unit → Speaker OT
    app(&input_terminal(ID_USB_STREAMING_IT, ID_CLOCK));
    app(&feature_unit(ID_FEATURE_PLAY, ID_USB_STREAMING_IT));
    app(&output_terminal(ID_SPEAKER_OT, ID_FEATURE_PLAY, ID_CLOCK, 0x0301));

    // 采集路径：Mic IT → Feature Unit → USB streaming OT
    app(&input_terminal(ID_MIC_IT, ID_CLOCK));
    app(&feature_unit(ID_FEATURE_CAPTURE, ID_MIC_IT));
    app(&output_terminal(ID_USB_STREAMING_OT, ID_FEATURE_CAPTURE, ID_CLOCK, 0x0101));

    // AC 中断端点（属于接口 0；UAC2 用它上报控制变化，Windows 侧会轮询）
    app(&interrupt_endpoint(0x83));


    // —— 接口 1：AudioStreaming 播放（EP 0x01 OUT）——
    // alt 0：零带宽
    app(&[
        9,
        DESC_INTERFACE,
        1,
        0,
        0,
        AUDIO_CLASS,
        SUBCLASS_AUDIOSTREAMING,
        PROTOCOL_IP_VERSION_02_00,
        0,
    ]);
    // alt 1
    app(&[
        9,
        DESC_INTERFACE,
        1,
        1,
        1,
        AUDIO_CLASS,
        SUBCLASS_AUDIOSTREAMING,
        PROTOCOL_IP_VERSION_02_00,
        0,
    ]);
    app(&as_general(ID_USB_STREAMING_IT, fmt));
    app(&type_i_format(fmt));
    app(&iso_endpoint(0x01, 0x09, fmt, CableProtocol::Uac2));
    app(&cs_iso_endpoint());

    // —— 接口 2：AudioStreaming 采集（EP 0x82 IN）——
    app(&[
        9,
        DESC_INTERFACE,
        2,
        0,
        0,
        AUDIO_CLASS,
        SUBCLASS_AUDIOSTREAMING,
        PROTOCOL_IP_VERSION_02_00,
        0,
    ]);
    app(&[
        9,
        DESC_INTERFACE,
        2,
        1,
        1,
        AUDIO_CLASS,
        SUBCLASS_AUDIOSTREAMING,
        PROTOCOL_IP_VERSION_02_00,
        0,
    ]);
    app(&as_general(ID_USB_STREAMING_OT, fmt));
    app(&type_i_format(fmt));
    // 采集端点用异步（0x05）：与 Linux f_uac2 的 HS IN 端点一致（参考设备在 Windows 上可用）
    app(&iso_endpoint(0x82, 0x05, fmt, CableProtocol::Uac2));
    app(&cs_iso_endpoint());

    let total = b.len() as u16;
    b[2] = (total & 0xff) as u8;
    b[3] = (total >> 8) as u8;
    b
}

/// AC 接口类描述符块总长（头 + 1 个 Clock Source + 2×(IT+FU+OT)）
/// 注意：AC 的中断端点**不计入** wTotalLength（与 f_uac2 一致）
fn ac_block_length() -> u16 {
    9 + 8 + 2 * (17 + 18 + 12)
}

/// Clock Source：| len | CS_IF | CLOCK_SOURCE | bClockID | bmAttributes | bmControls | bAssocTerminal | iClockSource |
/// bmAttributes=0x01（内部固定时钟）、bmControls=0x03（采样率可读可写，不宣告有效性）
fn clock_source(id: u8) -> Vec<u8> {
    vec![8, DESC_CS_INTERFACE, CS_CLOCK_SOURCE, id, 0x01, 0x03, 0x00, 0]
}

/// AC 接口的中断端点（INT IN，1ms 轮询；UAC2 控制变化通知用）
fn interrupt_endpoint(addr: u8) -> Vec<u8> {
    vec![
        9,
        DESC_ENDPOINT,
        addr,
        0x03, // 中断传输
        6,    // wMaxPacketSize（UAC2 §6.1：bStatusType + bAttribute + 附加信息）
        0,
        4, // bInterval：高速 2^(4-1)=8 微帧 = 1ms
        0, // bRefresh
        0, // bSynchAddress
    ]
}

fn input_terminal(id: u8, clock: u8) -> Vec<u8> {
    // 播放路径终端类型 = USB streaming(0x0101)；采集路径 = 麦克风(0x0201)
    let tt = if id == ID_MIC_IT { 0x0201u16 } else { 0x0101u16 };
    let mut v = vec![
        17,
        DESC_CS_INTERFACE,
        CS_INPUT_TERMINAL,
        id,
        tt as u8,
        (tt >> 8) as u8,
        0,     // bAssocTerminal
        clock, // bCSourceID
        2,     // bNrChannels
    ];
    v.extend_from_slice(&3u32.to_le_bytes()); // bmChannelConfig: FL|FR
    v.push(0); // iChannelNames
    v.extend_from_slice(&[0x03, 0x00]); // bmControls: Copy Control 可读写（对齐 f_uac2）
    v.push(0); // iTerminal
    v
}

fn feature_unit(id: u8, source: u8) -> Vec<u8> {
    // UAC2 bmaControls：每通道 4 字节位图。bmaControls[0] = master：
    // Mute(bits0-1) + Volume(bits2-3) 均可读写 = 0x0F；各通道条目留 0（与 f_uac2 一致）
    let mut v = vec![6 + 4 * 3, DESC_CS_INTERFACE, CS_FEATURE_UNIT, id, source];
    v.extend_from_slice(&[0x0F, 0x00, 0x00, 0x00]); // master
    for _ in 0..2 {
        v.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // 通道 1/2
    }
    v.push(0); // iFeature
    v
}

fn output_terminal(id: u8, source: u8, clock: u8, term_type: u16) -> Vec<u8> {
    let mut v = vec![
        12,
        DESC_CS_INTERFACE,
        CS_OUTPUT_TERMINAL,
        id,
        term_type as u8,
        (term_type >> 8) as u8,
        0,      // bAssocTerminal
        source, // bSourceID
        clock,  // bCSourceID
    ];
    v.extend_from_slice(&[0x03, 0x00]); // bmControls: Copy Control 可读写
    v.push(0); // iTerminal
    v
}

/// UAC2 CS AS Interface Descriptor（AS_GENERAL）——**16 字节**（USB Audio 2.0 §4.9.2）。
///
/// 布局（与 Linux `uac2_as_header_descriptor`、XMOS 量产固件一致）：
/// `bLength=16, type=0x24, subtype=1, bTerminalLink, bmControls(1B), bFormatType(1B),`
/// `bmFormats(4B 位图，bit0 = PCM), bNrChannels(1B), bmChannelConfig(4B), iChannelNames(1B)`
///
/// 注意 UAC1 的同名描述符是 7 字节（`bDelay` + `wFormatTag`），照抄会导致
/// usbaudio2.sys 解析不了 AS 描述符链 —— 这里必须是 16 字节。
fn as_general(terminal_link: u8, fmt: &CableFormat) -> Vec<u8> {
    const BMFORMAT_PCM: u32 = 1 << 0; // bmFormats: D0 = PCM
    const CHANNEL_CONFIG_FL_FR: u32 = 0x03;
    let mut v = vec![
        16,
        DESC_CS_INTERFACE,
        CS_AS_GENERAL,
        terminal_link,
        0, // bmControls：本接口无源控制
        1, // bFormatType = FORMAT_TYPE_I
    ];
    v.extend_from_slice(&BMFORMAT_PCM.to_le_bytes());
    v.push(fmt.channels as u8);
    v.extend_from_slice(&CHANNEL_CONFIG_FL_FR.to_le_bytes());
    v.push(0); // iChannelNames
    v
}

fn type_i_format(fmt: &CableFormat) -> Vec<u8> {
    vec![
        6,
        DESC_CS_INTERFACE,
        CS_FORMAT_TYPE,
        1, // FORMAT_TYPE_I
        fmt.subslot() as u8,
        fmt.bits as u8,
    ]
}

fn iso_endpoint(addr: u8, attrs: u8, fmt: &CableFormat, protocol: CableProtocol) -> Vec<u8> {
    // UAC1（全速）：bInterval=1 表示每帧(1ms)一个包，包长 = 每毫秒字节数（参考实现就是 0xC0=192）
    // UAC2（高速）：包长 = 一个服务间隔的字节数（含给 Windows 留的 1 帧余量），
    //              服务间隔由 bInterval 决定（高码率自动缩短到 0.5/0.25/0.125ms）
    let (wmax, binterval) = match protocol {
        CableProtocol::Uac1 => (fmt.fs_wmax_packet(), 1u8),
        CableProtocol::Uac2 => (fmt.iso_wmax_packet(), fmt.iso_b_interval()),
    };
    debug_assert!(wmax <= HS_MAX_PACKET);
    vec![
        9,
        DESC_ENDPOINT,
        addr,
        attrs,
        wmax as u8,
        (wmax >> 8) as u8,
        binterval,
        0, // bRefresh
        0, // bSynchAddress
    ]
}

fn cs_iso_endpoint() -> Vec<u8> {
    vec![
        8, DESC_CS_ENDPOINT, CS_EP_GENERAL, 0, 0, 0, 0, 0, // bLockDelayUnits=0, wLockDelay=0
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

impl Descriptors {
    /// GET_DESCRIPTOR 分发
    pub fn get(&self, descriptor_type: u8, index: u8) -> Option<Vec<u8>> {
        match descriptor_type {
            DESC_DEVICE if index == 0 => Some(self.device.clone()),
            DESC_QUALIFIER if index == 0 && !self.qualifier.is_empty() => {
                Some(self.qualifier.clone())
            }
            DESC_CONFIG if index == 0 => Some(self.config.clone()),
            DESC_STRING => self.strings.get(&index).cloned(),
            _ => None,
        }
    }

    /// 结构自检：总长一致、逐级描述符链完整、iso 包长符合高速规范
    pub fn validate(&self) -> Result<(), String> {
        if self.device.len() != 18 || self.device[1] != DESC_DEVICE {
            return Err("设备描述符无效".into());
        }
        match self.protocol {
            // 全速设备不提供 qualifier
            CableProtocol::Uac1 => {
                if !self.qualifier.is_empty() {
                    return Err("UAC1 不应提供 device qualifier".into());
                }
                if !self.fmt.fs_supported() {
                    return Err(format!(
                        "UAC1（全速）单包 {} 字节超过 1023，请把采样率/位深调低或改用 UAC2",
                        self.fmt.frame_bytes()
                    ));
                }
            }
            CableProtocol::Uac2 => {
                if self.qualifier.len() != 10 || self.qualifier[1] != DESC_QUALIFIER {
                    return Err("device qualifier 无效".into());
                }
            }
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
        // 设备描述符的类字段要跟协议一致（UAC1 = 0x00/0x00/0x00；UAC2 = 0xEF/0x02/0x01）
        // IAD 紧跟在配置描述符之后（偏移 9）
        let has_iad = cfg.len() > 11 && cfg[9] == 8 && cfg[10] == DESC_IAD;
        match self.protocol {
            CableProtocol::Uac1 => {
                if self.device[4] != 0x00 || self.device[5] != 0x00 || self.device[6] != 0x00 {
                    return Err("UAC1 设备描述符类字段必须全 0".into());
                }
                if has_iad {
                    return Err("UAC1 不应带 IAD".into());
                }
            }
            CableProtocol::Uac2 => {
                if self.device[4] != 0xEF {
                    return Err("UAC2 设备描述符应为 IAD 复合设备（0xEF）".into());
                }
                if !has_iad {
                    return Err("UAC2 缺少 IAD".into());
                }
            }
        }
        // AC 头里内嵌的 wTotalLength 必须等于 AC 块实际长度
        let ac_len = match self.protocol {
            CableProtocol::Uac1 => uac1_ac_block_length(),
            CableProtocol::Uac2 => ac_block_length(),
        };
        match cfg
            .windows(10)
            .position(|c| c[1] == DESC_CS_INTERFACE && c[2] == CS_AC_HEADER)
        {
            Some(p) => {
                // UAC1：bcdADC(2) + wTotalLength 在偏移 5/6；UAC2：bcdADC + bCategory + wTotalLength 在 6/7
                let off = match self.protocol {
                    CableProtocol::Uac1 => 5,
                    CableProtocol::Uac2 => 6,
                };
                let declared = u16::from_le_bytes([cfg[p + off], cfg[p + off + 1]]) as usize;
                if declared != ac_len as usize {
                    return Err(format!("AC 头 wTotalLength={declared}，应为 {ac_len}"));
                }
            }
            None => return Err("缺少 AC 接口头描述符".into()),
        }
        // iso 端点包长：单事务 ≤ 1024，且一个服务间隔的容量 ≥ 该间隔的需求 + 1 帧余量（Win10 要求）
        for chunk in cfg.windows(9) {
            if chunk[0] == 9 && chunk[1] == DESC_ENDPOINT && chunk[3] & 0x03 == 0x01 {
                let wmax = u16::from_le_bytes([chunk[4], chunk[5]]);
                let bint = chunk[6];
                let per = wmax & 0x07FF;
                if per == 0 || per > HS_MAX_PACKET {
                    return Err(format!("iso 单事务字节数 {per} 非法（1..={HS_MAX_PACKET}）"));
                }
                if wmax >> WMAX_MULT_SHIFT != 0 {
                    return Err("iso 不应使用多事务位域（Windows 音频栈不接受）".into());
                }
                match self.protocol {
                    // 全速：bInterval=1（1ms/包），包长 = 每毫秒字节数上限 1023
                    CableProtocol::Uac1 => {
                        if bint != 1 {
                            return Err(format!("UAC1 全速端点的 bInterval 应为 1，实际 {bint}"));
                        }
                        let need = self.fmt.fs_wmax_packet();
                        if per != need {
                            return Err(format!("UAC1 包长 {per}，应为 {need}"));
                        }
                    }
                    // 高速：服务间隔容量 ≥ 需求 + 1 帧余量，bInterval 按带宽自动选择
                    CableProtocol::Uac2 => {
                        let need = self.fmt.packet_bytes_at(bint);
                        if (per as u32) < need {
                            return Err(format!(
                                "iso 服务间隔容量 {per} 小于需求 {need}（bInterval={bint}，含 1 帧余量）"
                            ));
                        }
                        if bint != self.fmt.iso_b_interval() {
                            return Err(format!(
                                "iso bInterval={bint}，应为 {}",
                                self.fmt.iso_b_interval()
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(rate: u32, bits: u16) -> CableFormat {
        CableFormat { sample_rate: rate, bits, channels: 2 }
    }

    #[test]
    fn builds_for_all_supported_combinations() {
        for rate in [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000] {
            for bits in [16u16, 24, 32] {
                let d = build(1, "", &fmt(rate, bits), CableProtocol::Uac2).expect("应构建成功");
                d.validate().expect("自检应通过");
            }
        }
    }

    /// UAC1：逐字节对齐参考实现 Virtual-Cables（`src/internal/uac1/descriptors.go`）
    #[test]
    fn uac1_layout_matches_reference_implementation() {
        let d = build(1, "", &fmt(48_000, 16), CableProtocol::Uac1).expect("UAC1 应能构建");
        d.validate().expect("UAC1 自检应通过");

        // 设备描述符：USB 1.1 全速、类字段全 0、无 qualifier
        assert_eq!(&d.device[2..4], &[0x10, 0x01], "bcdUSB = 0x0110");
        assert_eq!(&d.device[4..7], &[0x00, 0x00, 0x00], "类字段全 0");
        assert!(d.qualifier.is_empty(), "全速设备不提供 device qualifier");
        assert!(d.get(DESC_QUALIFIER, 0).is_none(), "qualifier 请求应 STALL");

        let cfg = &d.config;
        // AC 头：10 字节、bcdADC=0x0100、wTotalLength=72、bInCollection=2、baInterfaceNr=[1,2]
        // 布局：配置头(9) + AC 接口描述符(9) + AC 头
        assert_eq!(cfg[9 + 9], 10, "AC 头 bLength");
        assert_eq!(
            &cfg[18..28],
            &[10, 0x24, 0x01, 0x00, 0x01, 72, 0, 2, 1, 2],
            "UAC1 AC 头"
        );
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

    /// UAC1 是全速：单包超过 1023 字节的格式要明确报错，而不是生成非法描述符
    #[test]
    fn uac1_rejects_formats_beyond_full_speed() {
        assert!(build(1, "", &fmt(192_000, 24), CableProtocol::Uac1).is_err(), "192k/24bit 超过全速");
        assert!(build(1, "", &fmt(176_400, 24), CableProtocol::Uac1).is_err(), "176.4k/24bit 超过全速");
        assert!(build(1, "", &fmt(192_000, 16), CableProtocol::Uac1).is_ok(), "192k/16bit 可以");
        assert!(build(1, "", &fmt(96_000, 32), CableProtocol::Uac1).is_ok(), "96k/32bit 可以");
    }

    #[test]
    fn rejects_out_of_range() {
        assert!(build(1, "", &fmt(8_000, 16), CableProtocol::Uac2).is_err(), "8kHz 超范围");
        assert!(build(1, "", &fmt(384_000, 16), CableProtocol::Uac2).is_err());
        assert!(build(1, "", &fmt(48_000, 8), CableProtocol::Uac2).is_err(), "8bit 不支持");
    }

    #[test]
    fn max_packet_scales_with_format() {
        let d = build(1, "", &fmt(192_000, 32), CableProtocol::Uac2).unwrap();
        // 找到 alt1 的 OUT 端点（addr 0x01）
        let pos = d.config.windows(9).position(|c| c[1] == DESC_ENDPOINT && c[2] == 0x01).unwrap();
        let ep = &d.config[pos..pos + 9];
        assert_eq!(ep[3], 0x09, "attrs=ISO|adaptive");
        // 每 1ms 需要 1536B + 1 帧余量 = 1544B > 单事务上限 1024
        // → 服务间隔缩短到 bInterval=3（0.5ms）：96 帧 + 1 帧 = 776B
        assert_eq!(fmt(192_000, 32).frame_bytes(), 1536);
        assert_eq!(ep[6], 3, "bInterval=3（0.5ms 服务间隔）");
        let wmax = u16::from_le_bytes([ep[4], ep[5]]);
        assert_eq!(wmax, 776, "96 帧 + 1 帧余量 = 776B");
        assert_eq!(wmax >> 11, 0, "不得使用多事务位域（Windows 音频栈不接受）");
        assert_eq!(fmt(192_000, 32).iso_b_interval(), 3);

        // 低速率：1ms 服务间隔就够，且带上 Win10 要求的 1 帧余量
        let g = fmt(48_000, 16);
        assert_eq!(g.frame_bytes(), 192);
        assert_eq!(g.iso_b_interval(), 4, "48k/16bit 用 1ms 服务间隔");
        assert_eq!(g.iso_wmax_packet(), 196, "48 帧 + 1 帧余量 = 196B");
    }

    #[test]
    fn wmax_packet_is_spec_valid_for_every_format() {
        for rate in [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000] {
            for bits in [16u16, 24, 32] {
                let f = fmt(rate, bits);
                let bint = f.iso_b_interval();
                let per = f.iso_wmax_packet();
                let need = f.packet_bytes_at(bint);
                assert!((1..=4).contains(&bint), "{rate}/{bits}: bInterval {bint} 越界");
                assert!(per >= 1 && per <= HS_MAX_PACKET, "{rate}/{bits}: 包长 {per} 越界");
                // 一个服务间隔的容量必须覆盖「该间隔需求 + 1 帧余量」（Win10 要求）
                assert!(per as u32 >= need, "{rate}/{bits}: {per} < {need}");
                // 余量不能浪费太多（最多比需求多留 2 个音频帧）
                assert!(
                    per as u32 <= need + 2 * f.frame_size(),
                    "{rate}/{bits}: 包长 {per} 相对需求 {need} 过大"
                );
            }
        }
    }

    #[test]
    fn frame_bytes_rounds_up_for_44100_family() {
        // 44.1k 系不是整数字节/毫秒：必须向上取整，否则主机发不满
        assert_eq!(fmt(44_100, 16).frame_bytes(), 177); // 176.4
        assert_eq!(fmt(44_100, 24).frame_bytes(), 265); // 264.6
        assert_eq!(fmt(88_200, 32).frame_bytes(), 706); // 705.6
        assert_eq!(fmt(48_000, 16).frame_bytes(), 192); // 整数保持
    }

    #[test]
    fn single_clock_source_per_usbaudio2_limit() {
        // MS 文档：The driver supports one single clock source only
        let d = build(1, "", &fmt(48_000, 16), CableProtocol::Uac2).unwrap();
        let clocks: Vec<usize> = d
            .config
            .windows(8)
            .enumerate()
            .filter(|(_, c)| c[1] == DESC_CS_INTERFACE && c[2] == CS_CLOCK_SOURCE)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(clocks.len(), 1, "只能有一个 Clock Source");
        let cs = &d.config[clocks[0]..clocks[0] + 8];
        assert_eq!(cs[4], 0x01, "内部固定时钟");
        assert_eq!(cs[5], 0x03, "bmControls = 采样率可读写");
        // 四条终端都指向这一个时钟（按描述符链逐条走，避免滑窗误匹配）
        let cfg = &d.config;
        let (mut it, mut ot) = (Vec::new(), Vec::new());
        let mut pos = 0usize;
        while pos + 2 <= cfg.len() {
            let len = cfg[pos] as usize;
            if len < 2 || pos + len > cfg.len() {
                break;
            }
            let desc = &cfg[pos..pos + len];
            if desc[1] == DESC_CS_INTERFACE {
                match desc[2] {
                    CS_INPUT_TERMINAL if len >= 17 => it.push(desc[7]),
                    CS_OUTPUT_TERMINAL if len >= 12 => ot.push(desc[8]),
                    _ => {}
                }
            }
            pos += len;
        }
        assert_eq!(it, vec![ID_CLOCK, ID_CLOCK], "两个输入终端的时钟");
        assert_eq!(ot, vec![ID_CLOCK, ID_CLOCK], "两个输出终端的时钟");
    }

    #[test]
    fn contains_required_uac2_entities() {
        let d = build(1, "", &fmt(48_000, 24), CableProtocol::Uac2).unwrap();
        // Clock Source 描述符存在
        assert!(d.config.windows(8).any(|c| c[1] == DESC_CS_INTERFACE && c[2] == CS_CLOCK_SOURCE));
        // IAD 存在且设备类为 0xEF
        assert!(d.config.windows(8).any(|c| c[1] == DESC_IAD));
        assert_eq!(d.device[4], 0xEF);
        // 两个 AS general 均存在（UAC2 = 16 字节；AC 头同为 subtype 1 但长度为 9，用长度区分）
        assert_eq!(
            d.config
                .windows(16)
                .filter(|c| c[0] == 16 && c[1] == DESC_CS_INTERFACE && c[2] == CS_AS_GENERAL)
                .count(),
            2
        );
    }

    /// UAC2 的 AS General 描述符必须是 16 字节（UAC1 的 7/10 字节布局会让 usbaudio2 解析失败）
    #[test]
    fn as_general_matches_uac2_layout() {
        let f = fmt(48_000, 24);
        let g = as_general(ID_USB_STREAMING_IT, &f);
        assert_eq!(g.len(), 16, "bLength 与向量长度一致");
        assert_eq!(g[0], 16, "bLength = 16");
        assert_eq!(g[1], DESC_CS_INTERFACE);
        assert_eq!(g[2], CS_AS_GENERAL);
        assert_eq!(g[3], ID_USB_STREAMING_IT, "bTerminalLink");
        assert_eq!(g[4], 0, "bmControls");
        assert_eq!(g[5], 1, "bFormatType = FORMAT_TYPE_I");
        assert_eq!(u32::from_le_bytes([g[6], g[7], g[8], g[9]]), 1, "bmFormats bit0 = PCM");
        assert_eq!(g[10], f.channels as u8, "bNrChannels");
        assert_eq!(u32::from_le_bytes([g[11], g[12], g[13], g[14]]), 3, "bmChannelConfig = FL|FR");
        assert_eq!(g[15], 0, "iChannelNames");

        // 描述符链自洽：遍历时能正好走完
        let d = build(1, "", &fmt(48_000, 24), CableProtocol::Uac2).unwrap();
        d.validate().expect("含 16B AS general 的描述符链应通过自检");
    }

    #[test]
    fn get_descriptor_dispatch() {
        let d = build(3, "", &fmt(48_000, 16), CableProtocol::Uac2).unwrap();
        assert_eq!(d.get(DESC_DEVICE, 0).unwrap()[8], 0xFF);
        assert_eq!(d.get(DESC_QUALIFIER, 0).unwrap()[1], DESC_QUALIFIER);
        let s = d.get(DESC_STRING, 2).unwrap();
        assert_eq!(s[1], DESC_STRING);
        assert!(d.get(DESC_CONFIG, 1).is_none(), "只有 1 个配置");
        assert!(d.get(DESC_STRING, 9).is_none());
    }

    #[test]
    fn custom_product_name_lands_in_string_descriptor() {
        let d = build(1, "直播线", &fmt(48_000, 16), CableProtocol::Uac2).unwrap();
        assert_eq!(d.product, "直播线");
        let s = d.get(DESC_STRING, 2).unwrap();
        assert_eq!(s[1], DESC_STRING);
        // UTF-16LE 编码："直" = 0x76F4
        assert_eq!(u16::from_le_bytes([s[2], s[3]]), '直' as u16);
        // 空白名回退到 Virtual Cable NN
        let d = build(2, "   ", &fmt(48_000, 16), CableProtocol::Uac2).unwrap();
        assert_eq!(d.product, "Virtual Cable 02");
        // 过长名称被拒绝
        assert!(build(1, &"x".repeat(65), &fmt(48_000, 16), CableProtocol::Uac2).is_err());
    }

    /// Windows 的 `usbaudio2.inf` 只匹配 `Prot_20`；协议字节写错驱动根本不会绑定。
    /// 这条测试把「能被 usbaudio2 认领」钉死在描述符里。
    #[test]
    fn hardware_ids_match_usbaudio2_inf_prot_20() {
        let d = build(1, "", &fmt(48_000, 16), CableProtocol::Uac2).unwrap();
        let cfg = &d.config;

        // IAD：USB\Class_01&SubClass_00&Prot_20（复合设备子设备的硬件 ID 来源）
        let iad_pos = cfg.windows(8).position(|c| c[1] == DESC_IAD).expect("必须有 IAD");
        let iad = &cfg[iad_pos..iad_pos + 8];
        assert_eq!(iad[4], 0x01, "bFunctionClass = AUDIO");
        assert_eq!(iad[5], 0x00, "bFunctionSubClass = 未定义");
        assert_eq!(iad[6], 0x20, "bFunctionProtocol = IP version 2.00");
        assert_eq!(
            format!("USB\\Class_{:02X}&SubClass_{:02X}&Prot_{:02X}", iad[4], iad[5], iad[6]),
            "USB\\Class_01&SubClass_00&Prot_20"
        );

        // 所有接口的 bInterfaceProtocol 也必须是 0x20
        let ifaces: Vec<&[u8]> = cfg
            .windows(9)
            .filter(|c| c[0] == 9 && c[1] == DESC_INTERFACE)
            .collect();
        assert_eq!(ifaces.len(), 5, "AC(alt0) + 播放 AS(alt0/alt1) + 录音 AS(alt0/alt1)");
        for iface in &ifaces {
            assert_eq!(iface[5], AUDIO_CLASS, "bInterfaceClass = AUDIO");
            assert_eq!(iface[7], 0x20, "bInterfaceProtocol 必须是 IP version 2.00");
        }
        // 子类：接口 0 = AudioControl，接口 1/2 = AudioStreaming
        assert!(ifaces.iter().filter(|c| c[2] == 0).all(|c| c[6] == SUBCLASS_AUDIOCONTROL));
        assert!(ifaces.iter().filter(|c| c[2] != 0).all(|c| c[6] == SUBCLASS_AUDIOSTREAMING));
    }

    /// 与量产 UAC2 固件（XMOS sw_usb_audio）逐字节对齐的接口头
    #[test]
    fn interface_headers_match_production_uac2_device() {
        let d = build(2, "", &fmt(96_000, 24), CableProtocol::Uac2).unwrap();
        let cfg = &d.config;
        // 期望的接口头（去掉前 2 字节的 bLength/bDescriptorType）：
        // [iface, alt, nEP, class, subclass, protocol, iInterface]
        let expected: [[u8; 7]; 3] = [
            [0, 0, 1, 0x01, 0x01, 0x20, 0], // AC（1 个中断端点，对齐 f_uac2）
            [1, 0, 0, 0x01, 0x02, 0x20, 0], // AS 播放 alt0
            [1, 1, 1, 0x01, 0x02, 0x20, 0], // AS 播放 alt1
        ];
        for (idx, exp) in expected.iter().enumerate() {
            let pos = cfg
                .windows(9)
                .enumerate()
                .filter(|(_, c)| c[0] == 9 && c[1] == DESC_INTERFACE)
                .nth(idx)
                .map(|(p, _)| p)
                .expect("接口头存在");
            let got = &cfg[pos..pos + 9];
            assert_eq!(
                &got[2..9],
                &exp[..],
                "第 {idx} 个接口头与量产 UAC2 设备不一致: {got:02x?}"
            );
        }
    }

    /// AC 接口的中断端点（0x83 / INT / 1ms）—— 参考设备有，Windows 侧会轮询它
    #[test]
    fn ac_interface_has_interrupt_endpoint() {
        let d = build(1, "", &fmt(48_000, 16), CableProtocol::Uac2).unwrap();
        let ac = d.config.windows(9).position(|c| c[0] == 9 && c[1] == DESC_INTERFACE).unwrap();
        assert_eq!(d.config[ac + 4], 1, "AC 接口 bNumEndpoints = 1");
        let ep = d
            .config
            .windows(9)
            .position(|c| c[1] == DESC_ENDPOINT && c[2] == 0x83)
            .expect("必须有 0x83 中断端点");
        assert_eq!(d.config[ep + 3], 0x03, "中断传输");
        let wmax = u16::from_le_bytes([d.config[ep + 4], d.config[ep + 5]]);
        assert_eq!(wmax, 6, "wMaxPacketSize = 6");
        assert_eq!(d.config[ep + 6], 4, "bInterval = 4（高速 1ms）");
    }
}
