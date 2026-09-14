//! 虚拟线缆设备：UAC2 类请求状态机 + PCM↔f32 数据通路。
//!
//! 每条线缆两条 f32 环：
//! - `play_ring`：ISO OUT（Windows 应用播放进虚拟扬声器）写入，混音引擎 Source 读取；
//! - `cap_ring`：混音引擎 Sink（或 loop_to_mic 的回环拷贝）写入，
//!   ISO IN（Windows 应用从虚拟麦克风录音）读取，空时数字静音。

use std::sync::Arc;

use parking_lot::Mutex;

use super::descriptors::{self, CableFormat, CableProtocol, Descriptors};
use super::ring::AudioRing;

// —— 标准请求 ——
const REQ_GET_STATUS: u8 = 0x00;
const REQ_SET_ADDRESS: u8 = 0x05;
const REQ_GET_DESCRIPTOR: u8 = 0x06;
const REQ_GET_CONFIGURATION: u8 = 0x08;
const REQ_SET_CONFIGURATION: u8 = 0x09;
const REQ_GET_INTERFACE: u8 = 0x0A;
const REQ_SET_INTERFACE: u8 = 0x0B;
const REQ_SYNCH_FRAME: u8 = 0x0C;
const REQ_CLEAR_FEATURE: u8 = 0x01;
const REQ_SET_FEATURE: u8 = 0x03;

// —— UAC 类请求（bRequest）——
// USB Audio 2.0 A.14：bit7 置位 = GET；UAC1 用 GET_MIN/GET_MAX/GET_RES 分段查询范围
const UAC_SET_CUR: u8 = 0x01;
const UAC_GET_CUR: u8 = 0x81;
const UAC_SET_RANGE: u8 = 0x02;
const UAC_GET_RANGE: u8 = 0x82;
/// UAC1 专用：最小值 / 最大值 / 步进（端点采样率控制的常用查询方式）
const UAC_GET_MIN: u8 = 0x82;
const UAC_GET_MAX: u8 = 0x83;
const UAC_GET_RES: u8 = 0x84;

/// 请求码种类（低 7 位）：CUR / RANGE
const UAC_KIND_CUR: u8 = 0x01;
const UAC_KIND_RANGE: u8 = 0x02;

/// 归一化请求码。真实驱动（usbaudio2.sys / Linux usb-audio）发 GET_CUR=0x81、
/// GET_RANGE=0x82；同时容忍少数实现把 GET 写成不带方向位的 0x01/0x02。
fn uac_kind(request: u8) -> u8 {
    match request {
        UAC_SET_CUR | UAC_GET_CUR => UAC_KIND_CUR,
        UAC_SET_RANGE | UAC_GET_RANGE => UAC_KIND_RANGE,
        other => other & 0x7F,
    }
}

/// 请求是否为 GET（方向位在 bmRequestType 的 bit7）
fn is_dir_in(request_type: u8) -> bool {
    (request_type & RT_DIR_IN) != 0
}

// —— 控制选择器（wValue 高字节）——
const CS_SAMPLING_FREQ: u8 = 0x01;
/// Clock Source 的时钟有效性（描述符宣告为只读）
const CS_CLOCK_VALID: u8 = 0x02;
const CS_MUTE: u8 = 0x01;
const CS_VOLUME: u8 = 0x02;

/// 音量范围（1/256 dB）：-60dB .. 0dB，步进 1dB
const VOLUME_MIN_DB_256: i16 = -60 * 256;
const VOLUME_RES_DB_256: i16 = 256;

// bmRequestType 位域
const RT_DIR_IN: u8 = 0x80;
const RT_TYPE_CLASS: u8 = 0x20; // class + interface recipient = 0x21/0xA1
const RT_RECIPIENT_STANDARD: u8 = 0x00;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CableMode {
    /// 线路输入 → 拷贝到 → 线路输出：播放进扬声器的音频原样出现在麦克风端（经典虚拟线缆）
    Loopback,
    /// 线路输出 → 拷贝到 → 线路输入：写进麦克风端的数据同时回灌到扬声器端
    Reverse,
    /// 不拷贝：麦克风端只输出混音图写入的信号（未写入时为静音）
    Mixer,
}

impl CableMode {
    /// 是否为「拷贝」模式（两个方向之一）
    pub fn is_copy(self) -> bool {
        matches!(self, CableMode::Loopback | CableMode::Reverse)
    }
}

/// 线缆配置
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CableConfig {
    /// 线缆号 1..=32（决定 busid "1-N" 与 PID）
    pub number: u8,
    /// 显示名（USB 产品字符串）；空则用 "Virtual Cable NN"
    #[serde(default)]
    pub name: String,
    pub sample_rate: u32,
    pub bits: u16,
    pub mode: CableMode,
    /// 环形缓冲容量（毫秒）
    pub buffer_ms: u32,
    /// USB 音频类版本（默认 UAC1）
    #[serde(default)]
    pub protocol: CableProtocol,
}

impl CableConfig {
    pub fn format(&self) -> CableFormat {
        CableFormat { sample_rate: self.sample_rate, bits: self.bits, channels: 2 }
    }

    /// 最终显示名（与 `UsbIpCableSettings::display_name` 一致）
    pub fn display_name(&self) -> String {
        let trimmed = self.name.trim();
        if trimmed.is_empty() {
            format!("Virtual Cable {:02}", self.number)
        } else {
            trimmed.to_string()
        }
    }
}

/// 设备内部状态（EP0 控制通道；USB 枚举要求串行处理）
struct DevState {
    configuration: u8,
    /// 接口号 → alternate setting
    alt: [u8; 3],
    sample_rate: u32,
    mute: [bool; 2],
    volume: [i16; 2], // 1/256 dB
}

/// 一条虚拟线缆
pub struct Cable {
    pub cfg: CableConfig,
    pub bus_id: String,
    pub descriptors: Descriptors,
    /// ISO OUT 数据 → 混音引擎读取
    pub play_ring: AudioRing,
    /// 混音引擎写入（或回环）→ ISO IN 数据
    pub cap_ring: AudioRing,
    state: Mutex<DevState>,
}

impl Cable {
    pub fn new(cfg: CableConfig) -> Result<Arc<Self>, String> {
        let fmt = cfg.format();
        fmt.validate()?;
        let number = cfg.number;
        if !(1..=32).contains(&number) {
            return Err(format!("线缆号 {number} 超出 1..=32"));
        }
        let buffer_ms = cfg.buffer_ms.clamp(20, 5000);
        let name = cfg.display_name();
        if name.chars().count() > 64 {
            return Err(format!("线缆 {number} 名称过长（上限 64 字）"));
        }
        // 容量 = 采样率 × 2 通道 × 毫秒
        let capacity = fmt.sample_rate as usize * 2 * buffer_ms as usize / 1000;
        Ok(Arc::new(Self {
            bus_id: format!("1-{number}"),
            cfg: CableConfig { buffer_ms, ..cfg },
            descriptors: descriptors::build(number, &name, &fmt, cfg.protocol)?,
            play_ring: AudioRing::new(capacity),
            cap_ring: AudioRing::new(capacity),
            state: Mutex::new(DevState {
                configuration: 0,
                alt: [0; 3],
                sample_rate: fmt.sample_rate,
                mute: [false; 2],
                volume: [0; 2],
            }),
        }))
    }

    pub fn product_name(&self) -> &str {
        &self.descriptors.product
    }

    pub fn format(&self) -> CableFormat {
        self.cfg.format()
    }

    /// 播放接口（ISO OUT）是否激活
    pub fn playback_active(&self) -> bool {
        let s = self.state.lock();
        s.configuration == 1 && s.alt[1] == 1
    }

    /// 采集接口（ISO IN）是否激活
    pub fn capture_active(&self) -> bool {
        let s = self.state.lock();
        s.configuration == 1 && s.alt[2] == 1
    }

    /// ISO OUT（线路输入 ← 系统播放）：把 Windows 播放来的 PCM（小端）写入 play_ring。
    /// `loopback` 模式下同步拷贝进 cap_ring（输入 → 拷贝到 → 输出）。
    /// 返回接受的字节数。
    pub fn write_playback(&self, pcm: &[u8]) -> usize {
        if !self.playback_active() {
            return 0;
        }
        let fmt = self.format();
        let mut samples = Vec::with_capacity(pcm.len() / fmt.subslot() as usize);
        pcm_to_f32(pcm, fmt.bits, &mut samples);
        self.play_ring.push(&samples);
        // loopback：播放端收到的音频原样出现在麦克风端。
        //
        // 关键：**只有主机真的在读麦克风（录音接口 alt1）时才拷贝**。
        // 参考实现（Virtual-Cables）只有一个 ring，播放写、采集读；我们额外多一个
        // cap_ring 是为了让混音器也能读播放端，但"没人读的时候照样往里灌"会让它被灌满，
        // 之后每次 push 都丢掉**还没被读走**的音频 —— 实测麦克风端因此被打碎
        // （序列测试：丢样 66 万、重样 20 万）。
        if self.cfg.mode == CableMode::Loopback && self.capture_active() {
            self.cap_ring.push(&samples);
        }
        pcm.len()
    }

    /// 混音引擎写入录音端（线路输出 → 系统录音）。
    /// `reverse` 模式下同时把同一份数据回灌到 play_ring（输出 → 拷贝到 → 输入），
    /// 这样混音图的「线路输入」源就能读到它。
    pub fn write_capture(&self, samples: &[f32]) {
        // 同样只在主机真的在读麦克风时才写（没人读就没必要积累）
        if !self.capture_active() {
            return;
        }
        self.cap_ring.push(samples);
        if self.cfg.mode == CableMode::Reverse {
            self.play_ring.push(samples);
        }
    }

    /// ISO IN（线路输出 → 系统录音）：从 cap_ring 取数据填充录音请求（小端 PCM）；空时数字静音。
    /// 返回有效字节数（静音部分也已写入缓冲，全部按有效计）。
    pub fn read_capture(&self, out_pcm: &mut [u8]) -> usize {
        if !self.capture_active() {
            out_pcm.fill(0);
            return out_pcm.len();
        }
        let fmt = self.format();
        let n = out_pcm.len() / fmt.subslot() as usize;
        let mut samples = vec![0f32; n];
        self.cap_ring.pop(&mut samples);
        f32_to_pcm(&samples, fmt.bits, out_pcm);
        out_pcm.len()
    }

    /// EP0 控制请求分发。返回 (数据, 状态)；STALL 用 STATUS_PIPE 表示。
    pub fn handle_control(&self, setup: SetupPacket, out: &[u8]) -> (Vec<u8>, i32) {
        use super::protocol::STATUS_PIPE;

        let req_type = setup.request_type;
        let r = match req_type & 0x60 {
            RT_RECIPIENT_STANDARD => self.handle_standard(setup, out),
            _ if req_type & 0x60 == RT_TYPE_CLASS => self.handle_class(setup, out),
            _ => (Vec::new(), STATUS_PIPE),
        };
        // 控制请求留痕：默认只记被拒的（STALL）请求 —— Windows 侧（usbaudio2）拿不到就会
        // 退化成「设备无可用格式」（GetMixFormat → AUDCLNT_E_UNSUPPORTED_FORMAT）。
        // 全量跟踪用 trace 级（`RUST_LOG=audiomix_backend_windows=trace`），
        // 否则像「驱动反复 SET_INTERFACE」这种情况会瞬间刷爆日志。
        if r.1 == STATUS_PIPE {
            tracing::debug!(
                "EP0 STALL: bmRequestType=0x{:02x} bRequest=0x{:02x} wValue=0x{:04x} wIndex=0x{:04x} wLength={}",
                setup.request_type,
                setup.request,
                setup.value,
                setup.index,
                setup.length
            );
        } else {
            tracing::trace!(
                "EP0 {} bmRequestType=0x{:02x} bRequest=0x{:02x} wValue=0x{:04x} wIndex=0x{:04x} wLength={} -> {} 字节, status={}",
                if setup.request_type & 0x80 != 0 { "IN " } else { "OUT" },
                setup.request_type,
                setup.request,
                setup.value,
                setup.index,
                setup.length,
                r.0.len(),
                r.1
            );
        }
        r
    }

    fn handle_standard(&self, setup: SetupPacket, _out: &[u8]) -> (Vec<u8>, i32) {
        use super::protocol::{STATUS_OK, STATUS_PIPE};

        match setup.request {
            REQ_GET_DESCRIPTOR => {
                let typ = (setup.value >> 8) as u8;
                let idx = setup.value as u8;
                match self.descriptors.get(typ, idx) {
                    Some(mut data) => {
                        data.truncate(setup.length as usize);
                        (data, STATUS_OK)
                    }
                    None => (Vec::new(), STATUS_PIPE),
                }
            }
            REQ_SET_ADDRESS => (Vec::new(), STATUS_OK),
            REQ_SET_CONFIGURATION => {
                let cfg = setup.value as u8;
                let mut s = self.state.lock();
                if cfg > 1 {
                    return (Vec::new(), STATUS_PIPE);
                }
                s.configuration = cfg;
                // 选配置将所有接口复位到 alt 0
                s.alt = [0; 3];
                self.play_ring.reset();
                self.cap_ring.reset();
                (Vec::new(), STATUS_OK)
            }
            REQ_GET_CONFIGURATION => {
                let s = self.state.lock();
                (vec![s.configuration], STATUS_OK)
            }
            REQ_SET_INTERFACE => {
                let iface = setup.index as u8;
                let alt = setup.value as u8;
                let mut s = self.state.lock();
                if s.configuration != 1 || iface > 2 || alt > 1 || (iface == 0 && alt != 0) {
                    return (Vec::new(), STATUS_PIPE);
                }
                s.alt[iface as usize] = alt;
                if alt == 0 && (iface == 1 || iface == 2) {
                    self.play_ring.reset();
                    self.cap_ring.reset();
                }
                (Vec::new(), STATUS_OK)
            }
            REQ_GET_INTERFACE => {
                let iface = setup.index as u8;
                if iface > 2 {
                    return (Vec::new(), STATUS_PIPE);
                }
                let s = self.state.lock();
                (vec![s.alt[iface as usize]], STATUS_OK)
            }
            REQ_GET_STATUS => (vec![0, 0], STATUS_OK),
            REQ_SYNCH_FRAME => (vec![0, 0], STATUS_OK),
            // 主机开流时会 SET/CLEAR_FEATURE(ENDPOINT_HALT)；参考实现直接接受，
            // 这里也照做（STALL 会让 usbaudio.sys 启动失败）
            REQ_CLEAR_FEATURE | REQ_SET_FEATURE => (Vec::new(), STATUS_OK),
            _ => (Vec::new(), STATUS_PIPE),
        }
    }

    /// UAC2 类请求：wIndex 高字节 = 实体 ID，wValue 高字节 = 控制选择器
    fn handle_class(&self, setup: SetupPacket, out: &[u8]) -> (Vec<u8>, i32) {
        use super::protocol::{STATUS_OK, STATUS_PIPE};

        let entity = (setup.index >> 8) as u8;
        let selector = (setup.value >> 8) as u8;
        let kind = uac_kind(setup.request);
        let get = is_dir_in(setup.request_type);
        let mut s = self.state.lock();

        // —— UAC1（usbaudio.sys）——
        //
        // 与 UAC2 的两处关键差别（照抄参考实现 Virtual-Cables 的处理）：
        // 1) 采样率控制挂在**端点**上（recipient=endpoint，wIndex 低字节 = 端点地址），值 3 字节；
        // 2) 范围查询是**分开的** GET_MIN(0x82)/GET_MAX(0x83)/GET_RES(0x84)，
        //    UAC2 的 GET_RANGE(0x82) 一次返回整块范围在这里会被当成 2 字节的 MIN 值 ——
        //    实测 usbaudio.sys 拿到 8 字节后直接启动失败（设备管理器代码 10）。
        if self.descriptors.protocol() == CableProtocol::Uac1 {
            let recipient = setup.request_type & 0x1F;
            let low_byte = (setup.index & 0xFF) as u8;
            let le = |v: i16| v.to_le_bytes().to_vec();

            // 端点采样率（3 字节）
            if recipient == 0x02 && selector == CS_SAMPLING_FREQ && (low_byte == 0x01 || low_byte == 0x82)
            {
                let rate = s.sample_rate;
                let rate24 = |v: u32| {
                    vec![(v & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, ((v >> 16) & 0xFF) as u8]
                };
                return match setup.request {
                    UAC_SET_CUR => {
                        if out.len() >= 3 {
                            let got = u32::from_le_bytes([out[0], out[1], out[2], 0]);
                            if got != rate {
                                return (Vec::new(), STATUS_PIPE);
                            }
                        }
                        (Vec::new(), STATUS_OK)
                    }
                    UAC_GET_CUR | UAC_GET_MIN | UAC_GET_MAX => (rate24(rate), STATUS_OK),
                    UAC_GET_RES => (rate24(1), STATUS_OK),
                    _ => (Vec::new(), STATUS_PIPE),
                };
            }

            // 特性单元静音/音量（recipient=interface，wIndex 高字节 = 实体 ID）
            if recipient == 0x01
                && (entity == ID_FEATURE_PLAY_ENTITY || entity == ID_FEATURE_CAPTURE_ENTITY)
            {
                let unit = if entity == ID_FEATURE_PLAY_ENTITY { 0 } else { 1 };
                return match (selector, setup.request) {
                    (CS_MUTE, UAC_SET_CUR) => {
                        if !out.is_empty() {
                            s.mute[unit] = out[0] != 0;
                        }
                        (Vec::new(), STATUS_OK)
                    }
                    (CS_MUTE, UAC_GET_CUR) => {
                        (vec![u8::from(s.mute[unit])], STATUS_OK)
                    }
                    (CS_VOLUME, UAC_SET_CUR) => {
                        if out.len() >= 2 {
                            s.volume[unit] = i16::from_le_bytes([out[0], out[1]]);
                        }
                        (Vec::new(), STATUS_OK)
                    }
                    (CS_VOLUME, UAC_GET_CUR) => (le(s.volume[unit]), STATUS_OK),
                    (CS_VOLUME, UAC_GET_MIN) => (le(VOLUME_MIN_DB_256), STATUS_OK),
                    (CS_VOLUME, UAC_GET_MAX) => (le(0), STATUS_OK),
                    (CS_VOLUME, UAC_GET_RES) => (le(VOLUME_RES_DB_256), STATUS_OK),
                    _ => (Vec::new(), STATUS_PIPE),
                };
            }
        }

        match entity {
            // Clock Source：采样率（描述符 bmControls=0x03：频率可读写）。
            // 播放/采集各一个时钟源，采样率一致
            ID_CLOCK_ENTITY | ID_CLOCK_CAP_ENTITY => match (selector, kind) {
                // —— 采样率（4 字节 LE）——
                (CS_SAMPLING_FREQ, UAC_KIND_CUR) if !get => {
                    // SET_CUR：只接受描述符宣告的采样率
                    if out.len() >= 4 {
                        let rate = u32::from_le_bytes([out[0], out[1], out[2], out[3]]);
                        if rate != s.sample_rate {
                            return (Vec::new(), STATUS_PIPE);
                        }
                    }
                    (Vec::new(), STATUS_OK)
                }
                (CS_SAMPLING_FREQ, UAC_KIND_CUR) => (s.sample_rate.to_le_bytes().to_vec(), STATUS_OK),
                (CS_SAMPLING_FREQ, UAC_KIND_RANGE) if get => {
                    // wNumSubRanges=1 + min + max + res（各 4 字节 LE）
                    let rate = s.sample_rate;
                    let mut data = 1u16.to_le_bytes().to_vec();
                    data.extend_from_slice(&rate.to_le_bytes());
                    data.extend_from_slice(&rate.to_le_bytes());
                    data.extend_from_slice(&0u32.to_le_bytes());
                    (data, STATUS_OK)
                }
                (CS_SAMPLING_FREQ, UAC_KIND_RANGE) => (Vec::new(), STATUS_OK),
                // —— 时钟有效性：1 字节，1 = 有效（设备内部时钟恒有效）——
                (CS_CLOCK_VALID, UAC_KIND_CUR) if !get => (Vec::new(), STATUS_OK),
                (CS_CLOCK_VALID, UAC_KIND_CUR) => (vec![1u8], STATUS_OK),
                _ => (Vec::new(), STATUS_PIPE),
            },
            // Feature Unit（播放=ID_FEATURE_PLAY, 采集=ID_FEATURE_CAPTURE）
            ID_FEATURE_PLAY_ENTITY | ID_FEATURE_CAPTURE_ENTITY => {
                let unit = if entity == ID_FEATURE_PLAY_ENTITY { 0 } else { 1 };
                match (selector, kind) {
                    (CS_MUTE, UAC_KIND_CUR) if !get => {
                        if !out.is_empty() {
                            s.mute[unit] = out[0] != 0;
                        }
                        (Vec::new(), STATUS_OK)
                    }
                    (CS_MUTE, UAC_KIND_CUR) => (
                        if s.mute[unit] { 1u8.to_le_bytes().to_vec() } else { 0u8.to_le_bytes().to_vec() },
                        STATUS_OK,
                    ),
                    (CS_VOLUME, UAC_KIND_CUR) if !get => {
                        if out.len() >= 2 {
                            s.volume[unit] = i16::from_le_bytes([out[0], out[1]]);
                        }
                        (Vec::new(), STATUS_OK)
                    }
                    (CS_VOLUME, UAC_KIND_CUR) => (
                        s.volume[unit].to_le_bytes().to_vec(),
                        STATUS_OK,
                    ),
                    (CS_VOLUME, UAC_KIND_RANGE) if get => {
                        // 1/256 dB，-60dB..0dB，步进 1dB（16 位音量控制 → 8 字节）
                        let mut data = 1u16.to_le_bytes().to_vec();
                        data.extend_from_slice(&(-60 * 256i16).to_le_bytes());
                        data.extend_from_slice(&(0i16).to_le_bytes());
                        data.extend_from_slice(&(256i16).to_le_bytes());
                        (data, STATUS_OK)
                    }
                    (CS_VOLUME, UAC_KIND_RANGE) => (Vec::new(), STATUS_OK),
                    _ => (Vec::new(), STATUS_PIPE),
                }
            }
            _ => (Vec::new(), STATUS_PIPE),
        }
    }
}

const ID_CLOCK_ENTITY: u8 = 10; // 播放侧时钟
const ID_CLOCK_CAP_ENTITY: u8 = 11; // 采集侧时钟
const ID_FEATURE_PLAY_ENTITY: u8 = 2;
const ID_FEATURE_CAPTURE_ENTITY: u8 = 5;

/// Setup 包（USB 规范小端）
#[derive(Debug, Clone, Copy)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl SetupPacket {
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 8 {
            return None;
        }
        Some(Self {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        })
    }
}

/// USB 音频流 PCM → f32 [-1, 1]。
///
/// **字节序必须是小端**：USB 音频数据格式规范（UAC1 §2.3.1 / UAC2 §2.3.1）规定
/// 音频流中的多字节样本一律 little-endian，Windows 侧 usbaudio/usbaudio2 也是按小端收发。
/// 早期实现误按大端解析（来回都大端，自己回环测试看不出问题），结果是：
/// 混音器从线缆读到的波形是「高低字节互换」的满幅噪声 —— 接主播放设备只有沙沙声，
/// 电平表还顶到 100%。
pub fn pcm_to_f32(pcm: &[u8], bits: u16, out: &mut Vec<f32>) {
    match bits {
        16 => out.extend(pcm.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)),
        24 => out.extend(pcm.chunks_exact(3).map(|c| {
            let v = ((c[2] as i32) << 24 | (c[1] as i32) << 16 | (c[0] as i32) << 8) >> 8;
            v as f32 / 8388608.0
        })),
        32 => out.extend(pcm.chunks_exact(4).map(|c| {
            i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2147483648.0
        })),
        _ => {}
    }
}

/// f32 → USB 音频流 PCM（小端；round + 满幅对称：v × 2^(bits-1)，解码侧为 ÷2^(bits-1)）
pub fn f32_to_pcm(samples: &[f32], bits: u16, out: &mut [u8]) {
    match bits {
        16 => {
            for (c, &s) in out.chunks_exact_mut(2).zip(samples) {
                let v = (s.clamp(-1.0, 1.0) * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
                c.copy_from_slice(&v.to_le_bytes());
            }
        }
        24 => {
            for (c, &s) in out.chunks_exact_mut(3).zip(samples) {
                let v = (s.clamp(-1.0, 1.0) * 8388608.0).round().clamp(-8388608.0, 8388607.0) as i32;
                c.copy_from_slice(&v.to_le_bytes()[0..3]);
            }
        }
        32 => {
            for (c, &s) in out.chunks_exact_mut(4).zip(samples) {
                let v = (s.clamp(-1.0, 1.0) * 2147483648.0).round().clamp(-2147483648.0, 2147483647.0) as i32;
                c.copy_from_slice(&v.to_le_bytes());
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usbip::protocol::{STATUS_OK, STATUS_PIPE};

    fn cfg(number: u8, rate: u32, bits: u16, mode: CableMode) -> CableConfig {
        CableConfig {
            number,
            name: String::new(),
            sample_rate: rate,
            bits,
            mode,
            buffer_ms: 250,
            protocol: CableProtocol::Uac2,
        }
    }

    #[test]
    fn self_test_loopback_16bit() {
        let c = Cable::new(cfg(1, 48_000, 16, CableMode::Loopback)).unwrap();
        // 枚举序列：设配置 → 激活两个接口
        let set_cfg = SetupPacket { request_type: 0x00, request: REQ_SET_CONFIGURATION, value: 1, index: 0, length: 0 };
        assert_eq!(c.handle_control(set_cfg, &[]).1, STATUS_OK);
        for iface in 1..=2u16 {
            let set_if = SetupPacket {
                request_type: 0x01,
                request: REQ_SET_INTERFACE,
                value: 1,
                index: iface,
                length: 0,
            };
            assert_eq!(c.handle_control(set_if, &[]).1, STATUS_OK);
        }
        assert!(c.playback_active());
        assert!(c.capture_active());

        // 回环：经过 PCM→f32→PCM 量化，按样本空间比较（误差 < 1 LSB）
        let samples: Vec<f32> = (0..100).map(|i| (i as f32 / 100.0) * 0.5 - 0.25).collect();
        let mut pcm = vec![0u8; 200];
        f32_to_pcm(&samples, 16, &mut pcm);
        assert_eq!(c.write_playback(&pcm), 200);
        let mut out = vec![0u8; 200];
        c.read_capture(&mut out);
        let mut roundtrip = Vec::with_capacity(100);
        pcm_to_f32(&out, 16, &mut roundtrip);
        for (a, b) in samples.iter().zip(&roundtrip) {
            assert!((a - b).abs() < 1.0 / 32767.0, "loopback 回环误差过大: {a} vs {b}");
        }
    }

    #[test]
    fn self_test_loopback_24_and_32bit() {
        for bits in [24u16, 32] {
            let c = Cable::new(cfg(2, 96_000, bits, CableMode::Loopback)).unwrap();
            let set_cfg = SetupPacket { request_type: 0x00, request: REQ_SET_CONFIGURATION, value: 1, index: 0, length: 0 };
            c.handle_control(set_cfg, &[]);
            for iface in 1..=2u16 {
                c.handle_control(SetupPacket { request_type: 0x01, request: REQ_SET_INTERFACE, value: 1, index: iface, length: 0 }, &[]);
            }
            let sub = (bits / 8) as usize;
            let samples = [-1.0f32, -0.5, 0.0, 0.25, 0.999];
            let mut pcm = vec![0u8; samples.len() * sub];
            f32_to_pcm(&samples, bits, &mut pcm);
            c.write_playback(&pcm);
            let mut out = vec![0u8; pcm.len()];
            c.read_capture(&mut out);
            // 按样本空间比较（量化误差 < 1 LSB）
            let mut roundtrip = Vec::with_capacity(samples.len());
            pcm_to_f32(&out, bits, &mut roundtrip);
            let tol = 1.5 / (1i64 << (bits - 1)) as f32;
            for (a, b) in samples.iter().zip(&roundtrip) {
                assert!((a - b).abs() < tol, "{bits}bit 回环误差过大: {a} vs {b}");
            }
        }
    }

    #[test]
    fn mixer_mode_no_echo() {
        let c = Cable::new(cfg(3, 48_000, 16, CableMode::Mixer)).unwrap();
        c.handle_control(SetupPacket { request_type: 0x00, request: REQ_SET_CONFIGURATION, value: 1, index: 0, length: 0 }, &[]);
        c.handle_control(SetupPacket { request_type: 0x01, request: REQ_SET_INTERFACE, value: 1, index: 2, length: 0 }, &[]);
        let pcm = vec![0x40u8; 16];
        c.write_playback(&pcm);
        let mut out = vec![0u8; 16];
        c.read_capture(&mut out);
        assert!(out.iter().all(|&b| b == 0), "mixer 模式无路由时麦克风端为静音");
    }

    /// **字节序自检**：线缆两端的 PCM 必须是小端。
    ///
    /// 这个测试刻意用「手工写死的字节」而不是走 pcm_to_f32/f32_to_pcm 自己对比 ——
    /// 大端实现下自回环测试也能通过（换出去再换回来，错的两处互相抵消），
    /// 只有跟外部（Windows 应用 / 混音引擎）对接时才会暴露成满幅噪声。
    #[test]
    fn pcm_stream_is_little_endian() {
        let c = Cable::new(cfg(7, 48_000, 16, CableMode::Mixer)).unwrap();
        c.handle_control(SetupPacket { request_type: 0x00, request: REQ_SET_CONFIGURATION, value: 1, index: 0, length: 0 }, &[]);
        for iface in 1..=2u16 {
            c.handle_control(SetupPacket { request_type: 0x01, request: REQ_SET_INTERFACE, value: 1, index: iface, length: 0 }, &[]);
        }

        // Windows 播放 0x1234 = 4660 → 小端字节是 [0x34, 0x12]
        c.write_playback(&[0x34, 0x12, 0x00, 0x80]);
        let mut play = vec![0f32; 2];
        c.play_ring.pop(&mut play);
        assert!((play[0] - 4660.0 / 32768.0).abs() < 1e-6, "ISO OUT 没按小端解析: {}", play[0]);
        assert!((play[1] + 1.0).abs() < 1e-6, "-32768 应解成 -1.0: {}", play[1]);

        // 反过来：0.5 → 16384 = 0x4000 → 录音端字节必须是 [0x00, 0x40]
        c.write_capture(&[0.5, -0.5]);
        let mut out = vec![0u8; 4];
        c.read_capture(&mut out);
        assert_eq!(out, vec![0x00, 0x40, 0x00, 0xC0], "ISO IN 没按小端打包");
    }

    /// 线路输出 → 拷贝到 → 线路输入（reverse）：引擎写进录音端的数据同时出现在播放端
    #[test]
    fn reverse_mode_copies_capture_to_playback() {
        let c = Cable::new(cfg(9, 48_000, 16, CableMode::Reverse)).unwrap();
        c.handle_control(SetupPacket { request_type: 0x00, request: REQ_SET_CONFIGURATION, value: 1, index: 0, length: 0 }, &[]);
        for iface in 1..=2u16 {
            c.handle_control(SetupPacket { request_type: 0x01, request: REQ_SET_INTERFACE, value: 1, index: iface, length: 0 }, &[]);
        }
        // 引擎写入录音端（线路输出）
        let samples: Vec<f32> = (0..64).map(|i| (i as f32 * 0.02).sin() * 0.5).collect();
        c.write_capture(&samples);
        // 录音端（ISO IN）拿到的就是这些数据
        let mut out = vec![0u8; samples.len() * 2];
        c.read_capture(&mut out);
        let mut roundtrip = Vec::new();
        pcm_to_f32(&out, 16, &mut roundtrip);
        for (a, b) in samples.iter().zip(&roundtrip) {
            assert!((a - b).abs() < 1.0 / 32767.0, "录音端数据不符: {a} vs {b}");
        }
        // 播放端（混音图的「线路输入」源）也应读到同一份数据
        let mut play = vec![0f32; samples.len()];
        c.play_ring.pop(&mut play);
        for (a, b) in samples.iter().zip(&play) {
            assert!((a - b).abs() < 1e-6, "reverse 模式未回灌到播放端: {a} vs {b}");
        }
    }

    /// 不拷贝模式（mixer）：写进录音端的数据不会跑到播放端
    #[test]
    fn mixer_mode_does_not_copy_capture_to_playback() {
        let c = Cable::new(cfg(10, 48_000, 16, CableMode::Mixer)).unwrap();
        c.write_capture(&[0.5f32; 32]);
        let mut play = vec![0f32; 32];
        c.play_ring.pop(&mut play);
        assert!(play.iter().all(|&v| v == 0.0), "mixer 模式不应回灌");
    }

    #[test]
    fn clock_source_rate_control() {
        let c = Cable::new(cfg(4, 192_000, 32, CableMode::Mixer)).unwrap();
        // GET_CUR（真实驱动发的 0x81：class IN，entity 10，CS 0x01）
        let get = SetupPacket { request_type: 0xA1, request: UAC_GET_CUR, value: 0x0100, index: 0x0A00, length: 4 };
        let (data, st) = c.handle_control(get, &[]);
        assert_eq!(st, STATUS_OK);
        assert_eq!(data, 192_000u32.to_le_bytes());
        // 兼容非标准实现把 GET 写成 0x01（方向位为 IN）
        let legacy = SetupPacket { request_type: 0xA1, request: 0x01, value: 0x0100, index: 0x0A00, length: 4 };
        assert_eq!(c.handle_control(legacy, &[]).0, 192_000u32.to_le_bytes());
        // SET_CUR 错误速率 → STALL
        let mut bad = 48_000u32.to_le_bytes().to_vec();
        let set = SetupPacket { request_type: 0x21, request: UAC_SET_CUR, value: 0x0100, index: 0x0A00, length: 4 };
        assert_eq!(c.handle_control(set, &bad).1, STATUS_PIPE);
        // SET_CUR 正确速率 → OK
        bad.clear();
        bad.extend_from_slice(&192_000u32.to_le_bytes());
        assert_eq!(c.handle_control(set, &bad).1, STATUS_OK);
        // GET_RANGE：14 字节
        let range = SetupPacket { request_type: 0xA1, request: UAC_GET_RANGE, value: 0x0100, index: 0x0A00, length: 14 };
        let (data, st) = c.handle_control(range, &[]);
        assert_eq!(st, STATUS_OK);
        assert_eq!(data.len(), 14);
        assert_eq!(&data[2..6], &192_000u32.to_le_bytes());
        // 未知选择器（Clock Multiplier 等）→ STALL
        let unknown = SetupPacket { request_type: 0xA1, request: UAC_GET_CUR, value: 0x0300, index: 0x0A00, length: 4 };
        assert_eq!(c.handle_control(unknown, &[]).1, STATUS_PIPE);
    }

    #[test]
    fn capture_clock_source_entity_is_wired() {
        // 采集侧时钟（实体 11，对应描述符里 Mic IT / USB streaming OT 的源）
        // 必须与播放时钟应答一致——两套时钟采样率相同，只是各管各的方向
        let c = Cable::new(cfg(5, 48_000, 16, CableMode::Mixer)).unwrap();
        let get = SetupPacket { request_type: 0xA1, request: UAC_GET_CUR, value: 0x0100, index: 0x0B00, length: 4 };
        let (data, st) = c.handle_control(get, &[]);
        assert_eq!(st, STATUS_OK, "采集时钟 GET_CUR 不能 STALL");
        assert_eq!(data, 48_000u32.to_le_bytes());
        // SET_CUR 同样按描述符速率校验
        let set = SetupPacket { request_type: 0x21, request: UAC_SET_CUR, value: 0x0100, index: 0x0B00, length: 4 };
        let mut rate = 48_000u32.to_le_bytes().to_vec();
        assert_eq!(c.handle_control(set, &rate).1, STATUS_OK);
        rate.clear();
        rate.extend_from_slice(&96_000u32.to_le_bytes());
        assert_eq!(c.handle_control(set, &rate).1, STATUS_PIPE);
        // GET_RANGE 与播放时钟同构（14 字节）
        let range = SetupPacket { request_type: 0xA1, request: UAC_GET_RANGE, value: 0x0100, index: 0x0B00, length: 14 };
        let (data, st) = c.handle_control(range, &[]);
        assert_eq!(st, STATUS_OK);
        assert_eq!(data.len(), 14);
        assert_eq!(&data[2..6], &48_000u32.to_le_bytes());
    }

    #[test]
    fn clock_source_validity_is_readable() {
        // 描述符 bmControls=0x03 只宣告了频率控制，但若主机仍问有效性
        // （selector 0x02），多答不亏——STALL 反而可能让枚举半途而废
        let c = Cable::new(cfg(7, 48_000, 16, CableMode::Mixer)).unwrap();
        let get = SetupPacket { request_type: 0xA1, request: UAC_GET_CUR, value: 0x0200, index: 0x0A00, length: 1 };
        let (data, st) = c.handle_control(get, &[]);
        assert_eq!(st, STATUS_OK, "时钟有效性不能 STALL");
        assert_eq!(data, vec![1u8], "内部时钟恒有效");
        let set = SetupPacket { request_type: 0x21, request: UAC_SET_CUR, value: 0x0200, index: 0x0A00, length: 1 };
        assert_eq!(c.handle_control(set, &[1]).1, STATUS_OK);
    }

    #[test]
    fn feature_unit_uses_get_cur() {
        let c = Cable::new(cfg(8, 48_000, 16, CableMode::Mixer)).unwrap();
        // 播放 Feature Unit（实体 2）静音 GET_CUR
        let get_mute = SetupPacket { request_type: 0xA1, request: UAC_GET_CUR, value: 0x0100, index: 0x0200, length: 1 };
        let (data, st) = c.handle_control(get_mute, &[]);
        assert_eq!(st, STATUS_OK);
        assert_eq!(data, vec![0u8]);
        // SET_CUR 静音
        let set_mute = SetupPacket { request_type: 0x21, request: UAC_SET_CUR, value: 0x0100, index: 0x0200, length: 1 };
        assert_eq!(c.handle_control(set_mute, &[1]).1, STATUS_OK);
        assert_eq!(c.handle_control(get_mute, &[]).0, vec![1u8]);
        // 音量 GET_RANGE（16 位控制 → 8 字节）
        let range = SetupPacket { request_type: 0xA1, request: UAC_GET_RANGE, value: 0x0200, index: 0x0200, length: 8 };
        let (data, st) = c.handle_control(range, &[]);
        assert_eq!(st, STATUS_OK);
        assert_eq!(data.len(), 8);
        assert_eq!(&data[0..2], &1u16.to_le_bytes(), "1 个子范围");
        assert_eq!(&data[2..4], &(-60i16 * 256).to_le_bytes());
        assert_eq!(&data[4..6], &0i16.to_le_bytes());
        // 音量 GET_CUR
        let cur = SetupPacket { request_type: 0xA1, request: UAC_GET_CUR, value: 0x0200, index: 0x0500, length: 2 };
        assert_eq!(c.handle_control(cur, &[]).0, 0i16.to_le_bytes().to_vec());
    }

    #[test]
    fn inactive_endpoints_reject_data() {
        let c = Cable::new(cfg(5, 48_000, 16, CableMode::Mixer)).unwrap();
        assert_eq!(c.write_playback(&[0u8; 16]), 0, "接口未激活时应丢弃");
        assert!(!c.playback_active());
        assert!(!c.capture_active());
    }

    #[test]
    fn get_descriptor_includes_qualifier() {
        let c = Cable::new(cfg(6, 48_000, 16, CableMode::Mixer)).unwrap();
        let get = SetupPacket { request_type: 0x80, request: REQ_GET_DESCRIPTOR, value: 0x0600, index: 0, length: 10 };
        let (data, st) = c.handle_control(get, &[]);
        assert_eq!(st, STATUS_OK);
        assert_eq!(data.len(), 10);
        assert_eq!(data[1], 0x06);
        let get = SetupPacket { request_type: 0x80, request: REQ_GET_DESCRIPTOR, value: 0x0300, index: 0, length: 255 };
        let (data, st) = c.handle_control(get, &[]);
        assert_eq!(st, STATUS_OK);
        assert_eq!(data, vec![4, 3, 0x09, 0x04]);
    }
}
