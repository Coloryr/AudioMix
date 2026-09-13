//! 数据模型：设备、混音图、应用设置。

use serde::{Deserialize, Serialize};

pub type Id = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    /// 录入设备（麦克风 / line-in / 虚拟输入端）
    Input,
    /// 输出设备（扬声器 / 虚拟输出端，可被 loopback 采集）
    Output,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub is_default: bool,
    /// 是否为虚拟声卡（驱动虚拟设备或已知第三方虚拟线路）
    pub is_virtual: bool,
    pub channels: u16,
    pub sample_rate: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceMode {
    /// 采集设备的录入信号
    DeviceInput,
    /// 采集输出设备的系统回环（loopback，即系统正在播放的声音）
    Loopback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: Id,
    pub name: String,
    /// 后端设备 id
    pub device_id: String,
    pub mode: SourceMode,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sink {
    pub id: Id,
    pub name: String,
    pub device_id: String,
    /// 0.0 ..= 1.0
    pub volume: f32,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub id: Id,
    pub source_id: Id,
    pub sink_id: Id,
    /// 线性增益 0.0 ..= 2.0（1.0 = 0dB）
    pub gain: f32,
    pub muted: bool,
}

/// 混音图：多 source 经 route 混音进 sink。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphConfig {
    pub sources: Vec<Source>,
    pub sinks: Vec<Sink>,
    pub routes: Vec<Route>,
}

impl GraphConfig {
    pub fn source(&self, id: &str) -> Option<&Source> {
        self.sources.iter().find(|s| s.id == id)
    }
    pub fn sink(&self, id: &str) -> Option<&Sink> {
        self.sinks.iter().find(|s| s.id == id)
    }
    /// 按 (device_id, mode) 找源（画布节点键 ↔ 源 的唯一映射依据）
    pub fn source_of_device(&self, device_id: &str, mode: SourceMode) -> Option<&Source> {
        self.sources
            .iter()
            .find(|s| s.device_id == device_id && s.mode == mode)
    }
    pub fn sink_of_device(&self, device_id: &str) -> Option<&Sink> {
        self.sinks.iter().find(|s| s.device_id == device_id)
    }
    pub fn validate(&self) -> Result<(), crate::Error> {
        for r in &self.routes {
            if self.source(&r.source_id).is_none() {
                return Err(crate::Error::InvalidGraph(format!(
                    "route {} 引用了不存在的 source {}",
                    r.id, r.source_id
                )));
            }
            if self.sink(&r.sink_id).is_none() {
                return Err(crate::Error::InvalidGraph(format!(
                    "route {} 引用了不存在的 sink {}",
                    r.id, r.sink_id
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlApiSettings {
    pub enabled: bool,
    pub bind: String,
    pub port: u16,
}

impl Default for ControlApiSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1".into(),
            port: 17643,
        }
    }
}

// —— USB/IP 虚拟声卡（UAC2）设置 ——

/// 虚拟线缆的拷贝方向（与后端 `CableMode` 一一对应，core 不依赖平台后端故独立定义）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsbIpCableMode {
    /// 线路输入 → 拷贝到 → 线路输出：系统播放进该线缆的声音原样出现在系统录音端
    Loopback,
    /// 线路输出 → 拷贝到 → 线路输入：写进系统录音端的声音同时回灌到系统播放端
    Reverse,
    /// 不拷贝：线路输出只输出混音图路由到该线的信号
    Mixer,
}

impl Default for UsbIpCableMode {
    fn default() -> Self {
        Self::Loopback
    }
}

/// 虚拟线缆对外暴露的 USB 音频类版本。
///
/// - `Uac1`：USB Audio 1.0（设备描述符 bDeviceClass/SubClass/Protocol = 0x00，
///   采样率控制挂在**端点**上，3 字节值）—— 由 Windows 自带的 `usbaudio.sys` 驱动。
/// - `Uac2`：USB Audio 2.0（复合设备 0xEF/0x02/0x01 + IAD，采样率挂在 **Clock Source**
///   上，4 字节值）—— 由 `usbaudio2.sys` 驱动，192kHz 需要它。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsbIpCableProtocol {
    Uac1,
    Uac2,
}

impl Default for UsbIpCableProtocol {
    fn default() -> Self {
        Self::Uac1
    }
}

/// 一条虚拟线缆的格式与模式
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsbIpCableSettings {
    /// 线缆号 1..=32（决定 USB/IP busid "1-N" 与 USB PID）
    pub number: u8,
    /// 显示名（USB 产品字符串）；为空时用 "Virtual Cable NN"。
    /// 会作为 USB 产品字符串，Windows 里看到的就是这个名字。
    pub name: String,
    /// 44_100 ..= 192_000
    pub sample_rate: u32,
    /// 16 / 24 / 32
    pub bits: u16,
    pub mode: UsbIpCableMode,
    /// 设备侧环形缓冲容量（毫秒）
    pub buffer_ms: u32,
    /// USB 音频类版本（UAC1 / UAC2）
    pub protocol: UsbIpCableProtocol,
}

impl Default for UsbIpCableSettings {
    fn default() -> Self {
        Self {
            number: 1,
            name: String::new(),
            sample_rate: 48_000,
            bits: 16,
            mode: UsbIpCableMode::Loopback,
            buffer_ms: 250,
            protocol: UsbIpCableProtocol::default(),
        }
    }
}

impl UsbIpCableSettings {
    pub const MIN_RATE: u32 = 44_100;
    pub const MAX_RATE: u32 = 192_000;
    pub const MAX_NUMBER: u8 = 32;
    /// 自定义名长度上限（USB 字符串描述符上限较宽，这里取一个稳妥值）
    pub const MAX_NAME_CHARS: usize = 64;

    /// 最终显示名：自定义名去空白，空则 `Virtual Cable NN`
    pub fn display_name(&self) -> String {
        let trimmed = self.name.trim();
        if trimmed.is_empty() {
            format!("Virtual Cable {:02}", self.number)
        } else {
            trimmed.to_string()
        }
    }

    pub fn validate(&self) -> Result<(), crate::Error> {
        if !(1..=Self::MAX_NUMBER).contains(&self.number) {
            return Err(crate::Error::InvalidSettings(format!(
                "线缆号 {} 超出 1..={}",
                self.number,
                Self::MAX_NUMBER
            )));
        }
        if self.name.chars().count() > Self::MAX_NAME_CHARS {
            return Err(crate::Error::InvalidSettings(format!(
                "线缆 {} 名称过长（上限 {} 字）",
                self.number,
                Self::MAX_NAME_CHARS
            )));
        }
        if self.name.chars().any(|c| c.is_control()) {
            return Err(crate::Error::InvalidSettings(format!(
                "线缆 {} 名称不能包含控制字符",
                self.number
            )));
        }
        if !(Self::MIN_RATE..=Self::MAX_RATE).contains(&self.sample_rate) {
            return Err(crate::Error::InvalidSettings(format!(
                "线缆 {} 采样率 {} 超出 {}–{} Hz",
                self.number,
                self.sample_rate,
                Self::MIN_RATE,
                Self::MAX_RATE
            )));
        }
        if !matches!(self.bits, 16 | 24 | 32) {
            return Err(crate::Error::InvalidSettings(format!(
                "线缆 {} 位深 {} 不受支持（16/24/32）",
                self.number, self.bits
            )));
        }
        if !(20..=5000).contains(&self.buffer_ms) {
            return Err(crate::Error::InvalidSettings(format!(
                "线缆 {} 缓冲 {}ms 超出 20–5000ms",
                self.number, self.buffer_ms
            )));
        }
        // UAC1 是 USB 1.1 全速设备：每 1ms 一个包，单包上限 1023 字节
        if self.protocol == UsbIpCableProtocol::Uac1 {
            let bytes_per_ms = Self::bytes_per_ms(self.sample_rate, self.bits);
            if bytes_per_ms > 1023 {
                return Err(crate::Error::InvalidSettings(format!(
                    "线缆 {} 在 UAC1（全速）下每毫秒需要 {} 字节，超过 1023 上限 —— \
                     请把采样率/位深调低，或把该线路改成 UAC2",
                    self.number, bytes_per_ms
                )));
            }
        }
        Ok(())
    }

    /// 每毫秒的 PCM 字节数（2 通道，向上取整）——判断 UAC1 全速能否承载
    pub fn bytes_per_ms(sample_rate: u32, bits: u16) -> u64 {
        let total = sample_rate as u64 * 2 * (bits as u64 / 8);
        total.div_ceil(1000)
    }
}

/// USB/IP 虚拟声卡设置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UsbIpSettings {
    /// 是否随应用启动内置 USB/IP 服务器
    pub enabled: bool,
    /// 监听地址（仅本机使用：usbip-win2 从本机 attach）
    pub bind: String,
    pub cables: Vec<UsbIpCableSettings>,
}

impl Default for UsbIpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1:3240".into(),
            cables: Vec::new(),
        }
    }
}

impl UsbIpSettings {
    pub const MAX_CABLES: usize = UsbIpCableSettings::MAX_NUMBER as usize;

    pub fn validate(&self) -> Result<(), crate::Error> {
        if self.cables.len() > Self::MAX_CABLES {
            return Err(crate::Error::InvalidSettings(format!(
                "线缆数 {} 超出上限 {}",
                self.cables.len(),
                Self::MAX_CABLES
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.cables {
            c.validate()?;
            if !seen.insert(c.number) {
                return Err(crate::Error::InvalidSettings(format!("线缆号 {} 重复", c.number)));
            }
        }
        Ok(())
    }

    pub fn cable(&self, number: u8) -> Option<&UsbIpCableSettings> {
        self.cables.iter().find(|c| c.number == number)
    }

    /// 下一个可用线缆号（1..=32；满了返回 None）
    pub fn next_free_number(&self) -> Option<u8> {
        (1..=UsbIpCableSettings::MAX_NUMBER).find(|n| self.cable(*n).is_none())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub control_api: ControlApiSettings,
    /// 内置 USB/IP 虚拟声卡（UAC2）线缆
    pub usbip: UsbIpSettings,
    /// 开机自启时以 headless 模式运行（不显示窗口）
    pub autostart_headless: bool,
    /// 记住"关闭窗口 = 隐藏到托盘"的偏好
    pub close_to_tray: bool,
    /// 用户选定的系统默认**播放**设备 id（MMDevice 端点 id）。
    /// 虚拟线路接入系统时 Windows 可能把默认设备抢过去，这里记住用户的选择用于恢复。
    pub default_output: Option<String>,
    /// 用户选定的系统默认**录音**设备 id
    pub default_input: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            control_api: ControlApiSettings::default(),
            usbip: UsbIpSettings::default(),
            autostart_headless: true,
            close_to_tray: true,
            default_output: None,
            default_input: None,
        }
    }
}

/// 混音画布上一个节点的位置（归一化 0..1，与画布尺寸无关）
pub type NodePos = [f64; 2];

/// 完整的应用配置（持久化到 app-data 目录）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphSettings {
    pub graph: GraphConfig,
    pub settings: Settings,
    /// 混音画布节点位置：node key → [x, y]。
    /// 纯前端布局元数据（引擎不关心），与图一起持久化；
    /// node key 形如 `in:<device_id>` / `loop:<device_id>` / `out:<device_id>` / `cable:<number>`。
    pub layout: std::collections::HashMap<String, NodePos>,
}

/// 已知虚拟声卡特征名（小写匹配）
pub const KNOWN_VIRTUAL_PATTERNS: &[&str] = &[
    "virtual audio",
    "virtual-audio",
    "virtual mic",
    "virtual cable",
    "cable input",
    "cable output",
    "vb-audio",
    "voicemeeter",
    "audiomirror",
    "vb-cable",
];

pub fn looks_virtual(name: &str) -> bool {
    let lower = name.to_lowercase();
    KNOWN_VIRTUAL_PATTERNS.iter().any(|p| lower.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_graph() -> GraphConfig {
        GraphConfig {
            sources: vec![Source {
                id: "s1".into(),
                name: "mic".into(),
                device_id: "d1".into(),
                mode: SourceMode::DeviceInput,
                enabled: true,
            }],
            sinks: vec![Sink {
                id: "k1".into(),
                name: "spk".into(),
                device_id: "d2".into(),
                volume: 1.0,
                enabled: true,
            }],
            routes: vec![Route {
                id: "r1".into(),
                source_id: "s1".into(),
                sink_id: "k1".into(),
                gain: 1.0,
                muted: false,
            }],
        }
    }

    #[test]
    fn validate_accepts_consistent_graph() {
        assert!(valid_graph().validate().is_ok());
    }

    #[test]
    fn validate_rejects_dangling_route_source() {
        let mut g = valid_graph();
        g.routes[0].source_id = "missing".into();
        assert!(matches!(g.validate(), Err(crate::Error::InvalidGraph(_))));
    }

    #[test]
    fn validate_rejects_dangling_route_sink() {
        let mut g = valid_graph();
        g.routes[0].sink_id = "missing".into();
        assert!(matches!(g.validate(), Err(crate::Error::InvalidGraph(_))));
    }

    #[test]
    fn helpers_find_nodes() {
        let g = valid_graph();
        assert!(g.source("s1").is_some());
        assert!(g.sink("k1").is_some());
        assert!(g.source("nope").is_none());
    }

    #[test]
    fn graph_json_roundtrip() {
        let g = valid_graph();
        let json = serde_json::to_string(&g).unwrap();
        let back: GraphConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sources.len(), 1);
        assert_eq!(back.routes[0].gain, 1.0);
    }

    #[test]
    fn settings_defaults_apply_for_partial_json() {
        // 旧版本配置缺字段时用默认值补齐（#[serde(default)]）
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.control_api.port, 17643);
        assert!(s.close_to_tray);
        let s: Settings = serde_json::from_str(r#"{"control_api":{"enabled":true,"bind":"0.0.0.0","port":1}}"#).unwrap();
        assert!(s.control_api.enabled);
        assert_eq!(s.control_api.port, 1);
        assert!(s.close_to_tray, "未指定的字段应取默认值");
    }

    #[test]
    fn virtual_name_detection() {
        assert!(looks_virtual("CABLE Input (VB-Audio)"));
        assert!(looks_virtual("Virtual Audio Driver Speaker"));
        assert!(looks_virtual("Virtual Mic Driver by MTT"));
        assert!(looks_virtual("Virtual Cable 01"));
        assert!(looks_virtual("Virtual Cable 12 Recording"));
        assert!(looks_virtual("Voicemeeter Input"));
        assert!(!looks_virtual("Realtek HD Audio"));
        assert!(!looks_virtual("Minifuse 1"));
    }

    #[test]
    fn usbip_settings_default_to_disabled_without_cables() {
        let s = Settings::default();
        assert!(!s.usbip.enabled);
        assert!(s.usbip.cables.is_empty());
        assert_eq!(s.usbip.bind, "127.0.0.1:3240");
        assert!(s.usbip.validate().is_ok());
    }

    #[test]
    fn usbip_settings_backfill_from_legacy_json() {
        // 旧配置没有 usbip 字段：应补默认值而不是解析失败
        let s: Settings = serde_json::from_str(r#"{"autostart_headless":false}"#).unwrap();
        assert!(!s.autostart_headless);
        assert!(!s.usbip.enabled);
        assert!(s.usbip.cables.is_empty());
    }

    #[test]
    fn usbip_settings_json_roundtrip() {
        let mut s = Settings::default();
        s.usbip.enabled = true;
        s.usbip.cables.push(UsbIpCableSettings {
            number: 3,
            name: "游戏耳返".into(),
            sample_rate: 192_000,
            bits: 32,
            mode: UsbIpCableMode::Mixer,
            buffer_ms: 120,
            protocol: UsbIpCableProtocol::Uac2,
        });
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        let c = back.usbip.cable(3).unwrap();
        assert_eq!(c.sample_rate, 192_000);
        assert_eq!(c.bits, 32);
        assert_eq!(c.mode, UsbIpCableMode::Mixer);
        assert_eq!(c.buffer_ms, 120);
        assert_eq!(c.name, "游戏耳返");
        assert_eq!(c.display_name(), "游戏耳返");
        // 序列化用 lowercase 枚举名，前端可直接比较
        assert!(json.contains("\"mixer\""), "{json}");
    }

    #[test]
    fn cable_copy_direction_variants_roundtrip() {
        // 两种拷贝方向 + 不拷贝，都要能落盘/读回，且前端能按小写名比较
        for (mode, text) in [
            (UsbIpCableMode::Loopback, "loopback"),
            (UsbIpCableMode::Reverse, "reverse"),
            (UsbIpCableMode::Mixer, "mixer"),
        ] {
            let c = UsbIpCableSettings { number: 2, mode, ..Default::default() };
            let json = serde_json::to_string(&c).unwrap();
            assert!(json.contains(&format!("\"{text}\"")), "{json}");
            let back: UsbIpCableSettings = serde_json::from_str(&json).unwrap();
            assert_eq!(back.mode, mode);
            // 旧配置（没有该字段）默认 loopback
        }
        let legacy: UsbIpCableSettings = serde_json::from_str(r#"{"number":2}"#).unwrap();
        assert_eq!(legacy.mode, UsbIpCableMode::Loopback);
    }

    #[test]
    fn cable_display_name_falls_back_to_number() {
        let c = UsbIpCableSettings { number: 7, ..Default::default() };
        assert_eq!(c.display_name(), "Virtual Cable 07");
        let c = UsbIpCableSettings { number: 7, name: "    ".into(), ..Default::default() };
        assert_eq!(c.display_name(), "Virtual Cable 07", "空白名视为未命名");
        let c = UsbIpCableSettings { number: 7, name: "  直播线  ".into(), ..Default::default() };
        assert_eq!(c.display_name(), "直播线", "两端空白应去掉");
    }

    #[test]
    fn cable_name_is_validated() {
        let ok = UsbIpCableSettings { name: "あ".repeat(UsbIpCableSettings::MAX_NAME_CHARS), ..Default::default() };
        assert!(ok.validate().is_ok(), "上限内应通过（按字符而非字节计）");
        let too_long = UsbIpCableSettings {
            name: "x".repeat(UsbIpCableSettings::MAX_NAME_CHARS + 1),
            ..Default::default()
        };
        assert!(matches!(too_long.validate(), Err(crate::Error::InvalidSettings(_))));
        let control = UsbIpCableSettings { name: "a\nb".into(), ..Default::default() };
        assert!(control.validate().is_err(), "控制字符应被拒绝");
    }

    #[test]
    fn legacy_graph_without_layout_still_parses() {
        // 旧配置里没有 layout 字段：必须补默认值而不是整体解析失败
        let json = r#"{"graph":{"sources":[],"sinks":[],"routes":[]},"settings":{}}"#;
        let gs: GraphSettings = serde_json::from_str(json).unwrap();
        assert!(gs.layout.is_empty());
        let with_layout: GraphSettings = serde_json::from_str(
            r#"{"graph":{"sources":[],"sinks":[],"routes":[]},"settings":{},"layout":{"cable:1":[0.25,0.5]}}"#,
        )
        .unwrap();
        assert_eq!(with_layout.layout.get("cable:1"), Some(&[0.25, 0.5]));
    }

    #[test]
    fn device_lookup_helpers() {
        let g = valid_graph();
        assert!(g.source_of_device("d1", SourceMode::DeviceInput).is_some());
        assert!(g.source_of_device("d1", SourceMode::Loopback).is_none());
        assert!(g.sink_of_device("d2").is_some());
        assert!(g.sink_of_device("nope").is_none());
    }

    #[test]
    fn usbip_cable_partial_json_uses_defaults() {
        let c: UsbIpCableSettings = serde_json::from_str(r#"{"number":2}"#).unwrap();
        assert_eq!(c.number, 2);
        assert_eq!(c.sample_rate, 48_000);
        assert_eq!(c.bits, 16);
        assert_eq!(c.mode, UsbIpCableMode::Loopback);
        assert_eq!(c.buffer_ms, 250);
    }

    #[test]
    fn usbip_validate_accepts_range_boundaries() {
        // UAC2（高速）允许到 192k/32bit
        for (rate, bits) in [(44_100u32, 16u16), (48_000, 24), (96_000, 32), (192_000, 32)] {
            let cfg = UsbIpCableSettings {
                number: 1,
                sample_rate: rate,
                bits,
                protocol: UsbIpCableProtocol::Uac2,
                ..Default::default()
            };
            assert!(cfg.validate().is_ok(), "UAC2 {rate}/{bits} 应合法");
        }
        // UAC1（全速）受 1023 字节/包 限制：48k/16、96k/32 可以，192k/32 不行
        for (rate, bits) in [(44_100u32, 16u16), (48_000, 24), (96_000, 32)] {
            let cfg = UsbIpCableSettings {
                number: 1,
                sample_rate: rate,
                bits,
                protocol: UsbIpCableProtocol::Uac1,
                ..Default::default()
            };
            assert!(cfg.validate().is_ok(), "UAC1 {rate}/{bits} 应合法");
        }
        let too_wide = UsbIpCableSettings {
            number: 1,
            sample_rate: 192_000,
            bits: 32,
            protocol: UsbIpCableProtocol::Uac1,
            ..Default::default()
        };
        assert!(
            matches!(too_wide.validate(), Err(crate::Error::InvalidSettings(_))),
            "UAC1 192k/32bit 必须被拒绝"
        );
    }

    #[test]
    fn usbip_validate_rejects_bad_format() {
        let bad_rate = UsbIpCableSettings { sample_rate: 8_000, ..Default::default() };
        assert!(matches!(bad_rate.validate(), Err(crate::Error::InvalidSettings(_))));
        let bad_bits = UsbIpCableSettings { bits: 8, ..Default::default() };
        assert!(bad_bits.validate().is_err());
        let bad_number = UsbIpCableSettings { number: 0, ..Default::default() };
        assert!(bad_number.validate().is_err());
        let bad_buffer = UsbIpCableSettings { buffer_ms: 1, ..Default::default() };
        assert!(bad_buffer.validate().is_err());
    }

    #[test]
    fn usbip_validate_rejects_duplicates_and_overflow() {
        let cable = UsbIpCableSettings { number: 1, ..Default::default() };
        let mut s = UsbIpSettings { enabled: true, ..Default::default() };
        s.cables = vec![cable.clone(), cable];
        assert!(matches!(s.validate(), Err(crate::Error::InvalidSettings(_))));

        let mut s = UsbIpSettings::default();
        s.cables = (1..=UsbIpCableSettings::MAX_NUMBER)
            .map(|n| UsbIpCableSettings { number: n, ..Default::default() })
            .collect();
        assert!(s.validate().is_ok());
        assert_eq!(s.next_free_number(), None);
        s.cables.push(UsbIpCableSettings { number: 9, ..Default::default() });
        assert!(s.validate().is_err(), "超过 32 条应被拒绝");
    }

    #[test]
    fn usbip_next_free_number_finds_gap() {
        let s = UsbIpSettings {
            enabled: true,
            bind: UsbIpSettings::default().bind,
            cables: vec![
                UsbIpCableSettings { number: 1, ..Default::default() },
                UsbIpCableSettings { number: 3, ..Default::default() },
            ],
        };
        assert_eq!(s.next_free_number(), Some(2));
    }
}
