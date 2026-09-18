//! 数据模型：设备、混音图、应用设置。

use serde::{Deserialize, Serialize};

use crate::resample::ResamplerQuality;

/// 实体 id（source/sink/route 共用，形如 `src-a1b2c3d4` 的字符串）。
pub type Id = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    /// 录入设备（麦克风 / line-in / 虚拟输入端）
    Input,
    /// 输出设备（扬声器 / 虚拟输出端，可被 loopback 采集）
    Output,
}

/// 一个音频端点设备（来自后端枚举）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// 平台端点 id（Windows 为 MMDevice 端点 id，打开流也用它）
    pub id: String,
    /// 友好名（系统设置里显示的名字）
    pub name: String,
    pub kind: DeviceKind,
    /// 是否为系统默认设备（录入/播放各至多一个）
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

/// 混音图的音频来源（一台设备一种采集模式至多一条 source）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: Id,
    pub name: String,
    /// 后端设备 id
    pub device_id: String,
    pub mode: SourceMode,
    /// false 时引擎不启动其采集流
    pub enabled: bool,
}

/// 混音图的输出目标（一台输出设备至多一条 sink）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sink {
    pub id: Id,
    pub name: String,
    /// 后端设备 id
    pub device_id: String,
    /// 0.0 ..= 1.0
    pub volume: f32,
    /// false 时引擎不启动其渲染流
    pub enabled: bool,
}

/// DSP 节点：类型 + 启用开关（旁路时不进处理链）。
///
/// serde 上 `kind` 被 flatten、`DspKind` 又是内部 tag（`type`）枚举，
/// 因此 JSON 形状是**扁平**的 `{ "type": "gain", "db": -6, "enabled": true }`，
/// 与前端 TS 联合类型一一对应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DspNode {
    #[serde(flatten)]
    pub kind: DspKind,
    /// false = 旁路（重建链时直接跳过该节点）
    #[serde(default = "crate::model::default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// DSP 节点类型与参数。链式按顺序处理，处理点在重采样 + 声道转换之后、混入输出之前
/// （采样率/声道数为 sink 流的实际格式）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DspKind {
    /// 强度（增益）
    Gain {
        /// -60 ..= 12 dB
        db: f32,
    },
    /// 延迟（每声道独立延迟线）
    Delay {
        /// 0 ..= 1000 ms
        ms: f32,
    },
    /// 3 段架式均衡：低架 / 峰值 / 高架
    Eq3 {
        low_gain_db: f32,
        low_freq: f32,
        mid_gain_db: f32,
        mid_freq: f32,
        mid_q: f32,
        high_gain_db: f32,
        high_freq: f32,
    },
    /// 单段峰式均衡
    PeakEq {
        freq: f32,
        gain_db: f32,
        q: f32,
    },
    /// 图形 EQ：10 段固定中心频率（31.25Hz..16kHz，1/3 倍频程）各自增益
    GraphEq {
        /// 长度 10（低 → 高），-24 ..= 24 dB
        gains_db: [f32; 10],
    },
    Highpass {
        freq: f32,
        q: f32,
    },
    Lowpass {
        freq: f32,
        q: f32,
    },
    /// 带通：用「起始/终止频率」定义（中心 = 几何平均，Q = 中心/带宽）。
    /// 旧字段 freq/q（单频点+Q）不再使用；缺字段时按默认值补齐以兼容旧配置。
    Bandpass {
        #[serde(default = "default_band_low")]
        low_freq: f32,
        #[serde(default = "default_band_high")]
        high_freq: f32,
    },
    /// 开关节点：开 = 直通，关 = 静音（与其它节点的旁路不同，关掉是切信号不是跳过）
    Switch,
    /// 限幅器：峰值包络检测 + 增益衰减，防止信号超过阈值削波
    Limiter {
        /// -24 ..= 0 dB（线性阈值 = 10^(db/20)）
        threshold_db: f32,
        /// 增益包络的释放时间 10 ..= 500 ms（衰减太快泵感明显，太慢压住下一个峰）
        release_ms: f32,
    },
}

fn default_band_low() -> f32 {
    300.0
}

fn default_band_high() -> f32 {
    3000.0
}

impl DspKind {
    /// 图形 EQ 的 10 段中心频率（1/3 倍频程）
    pub const GRAPH_EQ_BANDS: [f32; 10] = [
        31.25, 62.5, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
    ];

    /// UI/加载共用的参数钳制：超范围值拉回边界（防 NaN/极端值进滤波器）。
    pub fn clamp_params(&mut self) {
        let cl = |v: f32, lo: f32, hi: f32| v.clamp(lo, hi);
        match self {
            DspKind::Gain { db } => *db = cl(*db, -60.0, 12.0),
            DspKind::Delay { ms } => *ms = cl(*ms, 0.0, 1000.0),
            DspKind::Eq3 {
                low_gain_db,
                low_freq,
                mid_gain_db,
                mid_freq,
                mid_q,
                high_gain_db,
                high_freq,
            } => {
                *low_gain_db = cl(*low_gain_db, -24.0, 12.0);
                *low_freq = cl(*low_freq, 40.0, 500.0);
                *mid_gain_db = cl(*mid_gain_db, -24.0, 12.0);
                *mid_freq = cl(*mid_freq, 200.0, 8000.0);
                *mid_q = cl(*mid_q, 0.3, 10.0);
                *high_gain_db = cl(*high_gain_db, -24.0, 12.0);
                *high_freq = cl(*high_freq, 2000.0, 16000.0);
            }
            DspKind::PeakEq { freq, gain_db, q } => {
                *freq = cl(*freq, 20.0, 20000.0);
                *gain_db = cl(*gain_db, -24.0, 24.0);
                *q = cl(*q, 0.3, 10.0);
            }
            DspKind::GraphEq { gains_db } => {
                for g in gains_db.iter_mut() {
                    *g = cl(*g, -24.0, 24.0);
                }
            }
            DspKind::Highpass { freq, q } | DspKind::Lowpass { freq, q } => {
                *freq = cl(*freq, 20.0, 20000.0);
                *q = cl(*q, 0.3, 10.0);
            }
            DspKind::Bandpass {
                low_freq,
                high_freq,
            } => {
                *low_freq = cl(*low_freq, 20.0, 19_000.0);
                *high_freq = cl(*high_freq, 30.0, 20_000.0);
                // 终止频率至少比起始高 10Hz（几何平均/带宽计算需要 high > low）
                if *high_freq < *low_freq + 10.0 {
                    *high_freq = (*low_freq + 10.0).min(20_000.0);
                }
            }
            DspKind::Switch => {}
            DspKind::Limiter {
                threshold_db,
                release_ms,
            } => {
                *threshold_db = cl(*threshold_db, -24.0, 0.0);
                *release_ms = cl(*release_ms, 10.0, 500.0);
            }
        }
    }
}

/// 一条路由边：把某 source（或 processor）的声音送进某 sink（或 processor）。
///
/// 画布上 DSP 处理方块是一等节点（`GraphConfig::processors`），连线两端可以是
/// source/processor 与 sink/processor 的任意「出 → 入」组合；引擎按**路径**
/// （源 → … → 汇，途经的 processor 按序串联成 DSP 链）解析信号流。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub id: Id,
    pub source_id: Id,
    pub sink_id: Id,
    /// 线性增益 0.0 ..= 2.0（1.0 = 0dB）；路径总增益 = 沿途各段相乘
    pub gain: f32,
    pub muted: bool,
    /// （已废弃）早期挂在连线上的 DSP 链；被画布 DSP 方块取代，字段仅为
    /// 旧配置兼容保留，引擎忽略。
    #[serde(default)]
    pub nodes: Vec<DspNode>,
}

/// 画布上的 DSP 处理方块：一个 id + 一个 DSP 节点（类型 + 参数 + 旁路开关）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Processor {
    pub id: Id,
    #[serde(flatten)]
    pub node: DspNode,
}

/// 混音图：多 source 经 route（可穿过 processor）混音进 sink。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphConfig {
    pub sources: Vec<Source>,
    pub sinks: Vec<Sink>,
    pub routes: Vec<Route>,
    /// DSP 处理方块（画布节点；`#[serde(default)]` 向后兼容）
    #[serde(default)]
    pub processors: Vec<Processor>,
}

impl GraphConfig {
    /// 按 id 查 source。
    pub fn source(&self, id: &str) -> Option<&Source> {
        self.sources.iter().find(|s| s.id == id)
    }
    /// 按 id 查 sink。
    pub fn sink(&self, id: &str) -> Option<&Sink> {
        self.sinks.iter().find(|s| s.id == id)
    }
    /// 按 id 查 DSP 处理方块。
    pub fn processor(&self, id: &str) -> Option<&Processor> {
        self.processors.iter().find(|p| p.id == id)
    }
    /// 按 (device_id, mode) 找源（画布节点键 ↔ 源 的唯一映射依据）
    pub fn source_of_device(&self, device_id: &str, mode: SourceMode) -> Option<&Source> {
        self.sources
            .iter()
            .find(|s| s.device_id == device_id && s.mode == mode)
    }
    /// 按设备 id 找 sink。
    pub fn sink_of_device(&self, device_id: &str) -> Option<&Sink> {
        self.sinks.iter().find(|s| s.device_id == device_id)
    }
    /// 校验图的一致性：每条 route 的两端必须存在（source **或** processor 出、
    /// sink **或** processor 入）；processor id 不得重复。
    pub fn validate(&self) -> Result<(), crate::Error> {
        let mut seen = std::collections::HashSet::new();
        for p in &self.processors {
            if !seen.insert(p.id.as_str()) {
                return Err(crate::Error::InvalidGraph(format!(
                    "processor id {} 重复",
                    p.id
                )));
            }
        }
        for r in &self.routes {
            if self.source(&r.source_id).is_none() && self.processor(&r.source_id).is_none() {
                return Err(crate::Error::InvalidGraph(format!(
                    "route {} 引用了不存在的 source/processor {}",
                    r.id, r.source_id
                )));
            }
            if self.sink(&r.sink_id).is_none() && self.processor(&r.sink_id).is_none() {
                return Err(crate::Error::InvalidGraph(format!(
                    "route {} 引用了不存在的 sink/processor {}",
                    r.id, r.sink_id
                )));
            }
        }
        Ok(())
    }
}

/// 本地控制 API（REST + SSE）开关与监听参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlApiSettings {
    pub enabled: bool,
    /// 监听地址（默认仅本机）
    pub bind: String,
    pub port: u16,
    /// 访问令牌：非空时 `/api/*` 全部要带令牌
    /// （`Authorization: Bearer <token>` / `X-Api-Token` / `?token=`）。
    /// 空 = 不鉴权（仅适合只监听回环地址的场景）。
    pub token: String,
    /// 允许浏览器跨域调用（CORS）。**默认关闭**：开启后任意网页都能读取/控制
    /// 本机混音器（CSRF 面），只在有前端页面作为客户端时才打开，且应同时设令牌。
    pub cors: bool,
}

impl Default for ControlApiSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1".into(),
            port: 17643,
            token: String::new(),
            cors: false,
        }
    }
}

impl ControlApiSettings {
    /// 生成一个新令牌（32 位十六进制）。界面上「生成」按钮用。
    pub fn generate_token() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    /// 监听地址是否只在回环（外网/局域网访问不到）
    pub fn loopback_only(&self) -> bool {
        matches!(self.bind.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]")
    }

    /// 启动前自检：非回环监听且没设令牌 → 返回提示文本（仅告警，不阻断）
    pub fn security_warning(&self) -> Option<String> {
        if self.enabled && !self.loopback_only() && self.token.trim().is_empty() {
            return Some(format!(
                "控制 API 监听 {}（非回环）但未设置访问令牌：同网段的任何人都能控制本机混音器",
                self.bind
            ));
        }
        None
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

/// 一条虚拟线缆的格式与模式
///
/// 内置虚拟线路**只有 UAC1**（Windows 自带的 `usbaudio.sys`，USB 1.1 全速，
/// 每 1ms 一个包、单包上限 1023 字节）。需要更高规格（例如 192k/24bit）的线路，
/// 由用户自行安装第三方虚拟声卡（VB-CABLE 等），在混音画布里当普通节点接线即可；
/// 见 `is_supported` / `MULTIBIT_MAX_RATE`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsbIpCableSettings {
    /// 线缆号 1..=32（决定 USB/IP busid "1-N" 与 USB PID）
    pub number: u8,
    /// 显示名（USB 产品字符串）；为空时用 "Virtual Cable NN"。
    /// 会作为 USB 产品字符串，Windows 里看到的就是这个名字。
    pub name: String,
    /// 44_100 ..= 96_000
    pub sample_rate: u32,
    /// 16 / 24 / 32
    pub bits: u16,
    pub mode: UsbIpCableMode,
    /// 设备侧环形缓冲容量（毫秒）
    pub buffer_ms: u32,
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
        }
    }
}

impl UsbIpCableSettings {
    pub const MIN_RATE: u32 = 44_100;
    /// 实测上限：UAC1 全速等时流在 usbip-win2/UDE 上 96 kHz 以上无法稳定承载
    /// （192k 时主机侧 ISO OUT 大面积保持/断流，见 TASK.md P1 格式矩阵）。
    pub const MAX_RATE: u32 = 96_000;
    /// 多位深（24/32bit）的采样率上限：88.2k 以上只支持 16bit。
    ///
    /// 依据（2026-09-15 实测）：loopback/reverse 模式 OUT+IN 两个 iso 端点**共享同一全速帧的
    /// ~1023 B/ms 预算**，96k/24 双向 = 1152 B/ms、96k/32 = 1536，都会像 192k 一样
    /// 主机侧 ISO OUT 慢性欠载、播放断流；96k/16 = 768 安全。
    pub const MULTIBIT_MAX_RATE: u32 = 88_200;
    pub const MAX_NUMBER: u8 = 32;
    /// 自定义名长度上限（USB 字符串描述符上限较宽，这里取一个稳妥值）
    pub const MAX_NAME_CHARS: usize = 64;
    /// 内置线路（UAC1 全速）每毫秒的字节上限：USB 1.1 全速等时端点单包 1023 字节
    pub const MAX_BYTES_PER_MS: u64 = 1023;

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
        if self.sample_rate > Self::MULTIBIT_MAX_RATE && self.bits != 16 {
            return Err(crate::Error::InvalidSettings(format!(
                "线缆 {} 的 {} Hz 只支持 16bit（全速 USB 双向带宽限制）",
                self.number, self.sample_rate
            )));
        }
        if !(20..=5000).contains(&self.buffer_ms) {
            return Err(crate::Error::InvalidSettings(format!(
                "线缆 {} 缓冲 {}ms 超出 20–5000ms",
                self.number, self.buffer_ms
            )));
        }
        // 内置线路只有 UAC1（USB 1.1 全速）：每 1ms 一个包，单包上限 1023 字节，
        // 且采样率上限 96 kHz。更高规格请用户自装第三方虚拟声卡。
        let bytes_per_ms = Self::packet_bytes_per_ms(self.sample_rate, self.bits);
        if bytes_per_ms > Self::MAX_BYTES_PER_MS {
            return Err(crate::Error::InvalidSettings(format!(
                "线缆 {} 的 {} Hz / {}-bit 每毫秒需要 {} 字节，超过内置线路（UAC1）上限 {} —— \
                 请降低采样率或位深，或自装第三方虚拟声卡来获得更高规格",
                self.number,
                self.sample_rate,
                self.bits,
                bytes_per_ms,
                Self::MAX_BYTES_PER_MS
            )));
        }
        Ok(())
    }

    /// 该采样率/位深在内置线路（UAC1 全速）下是否可用。
    pub fn is_supported(sample_rate: u32, bits: u16) -> bool {
        (Self::MIN_RATE..=Self::MAX_RATE).contains(&sample_rate)
            && matches!(bits, 16 | 24 | 32)
            && (sample_rate <= Self::MULTIBIT_MAX_RATE || bits == 16)
            && Self::packet_bytes_per_ms(sample_rate, bits) <= Self::MAX_BYTES_PER_MS
    }

    /// 把不受支持的格式降到最近的可用组合（载入旧配置时用）。
    /// 返回一句给用户看的说明；本来就是合法组合时返回 `None`。
    pub fn clamp_supported(&mut self) -> Option<String> {
        let original = (self.sample_rate, self.bits);
        if Self::is_supported(self.sample_rate, self.bits) {
            return None;
        }
        if !matches!(self.bits, 16 | 24 | 32) {
            self.bits = 16;
        }
        // 超上限降到 96k（保住能保的最高规格），低于下限回默认 48k
        if self.sample_rate > Self::MAX_RATE {
            self.sample_rate = Self::MAX_RATE;
        } else if self.sample_rate < Self::MIN_RATE {
            self.sample_rate = 48_000;
        }
        // 88.2k 以上只支持 16bit（双向带宽限制，见 MULTIBIT_MAX_RATE 注释）
        if self.sample_rate > Self::MULTIBIT_MAX_RATE && self.bits != 16 {
            self.bits = 16;
        }
        if Self::packet_bytes_per_ms(self.sample_rate, self.bits) > Self::MAX_BYTES_PER_MS {
            self.bits = 16;
        }
        if !Self::is_supported(self.sample_rate, self.bits) {
            self.sample_rate = 48_000;
            self.bits = 16;
        }
        Some(format!(
            "线缆 {}：{}/{}bit 超出内置线路（UAC1）规格，已改为 {}/{}bit",
            self.number, original.0, original.1, self.sample_rate, self.bits
        ))
    }

    /// 每毫秒的 PCM 字节数（2 通道，向上取整）——判断 UAC1 全速能否承载
    pub fn bytes_per_ms(sample_rate: u32, bits: u16) -> u64 {
        let total = sample_rate as u64 * 2 * (bits as u64 / 8);
        total.div_ceil(1000)
    }

    /// 端点描述符里实际写出的每毫秒包长：**按整数个采样帧向上取整**。
    ///
    /// 与后端 `usbip::descriptors::CableFormat::fs_wmax_packet` 同规则 ——
    /// 44.1k 系每毫秒是小数帧（44.1k/24bit = 264.6 字节 = 44.1 帧），标称向上取整得到的
    /// 265 字节不是合法音频包，真机上 Windows 会认不出该端点格式（见 TASK P0）。
    pub fn packet_bytes_per_ms(sample_rate: u32, bits: u16) -> u64 {
        let frame = 2 * (bits as u64 / 8);
        (sample_rate as u64).div_ceil(1000) * frame
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
    /// 虚拟线缆列表（每条是一个独立 USB 设备）
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

    /// 校验全部线缆配置（逐条校验 + 线缆号查重 + 数量上限）。
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
                return Err(crate::Error::InvalidSettings(format!(
                    "线缆号 {} 重复",
                    c.number
                )));
            }
        }
        Ok(())
    }

    /// 按线缆号查线缆。
    pub fn cable(&self, number: u8) -> Option<&UsbIpCableSettings> {
        self.cables.iter().find(|c| c.number == number)
    }

    /// 下一个可用线缆号（1..=32；满了返回 None）
    pub fn next_free_number(&self) -> Option<u8> {
        (1..=UsbIpCableSettings::MAX_NUMBER).find(|n| self.cable(*n).is_none())
    }

    /// 载入旧配置时把超出内置线路规格的线缆降到可用组合；
    /// 返回每条被改动线缆的说明，交给界面/日志提示用户。
    pub fn clamp_cables_to_supported(&mut self) -> Vec<String> {
        self.cables
            .iter_mut()
            .filter_map(|c| c.clamp_supported())
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub control_api: ControlApiSettings,
    /// 内置 USB/IP 虚拟声卡（UAC1）线缆
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
    /// 用户默认**输出**偏好是虚拟线路（意图标志：线路端点 id 每次重连都会变，记意图不记 id）
    #[serde(default)]
    pub default_output_virtual: bool,
    /// 用户默认**输入**偏好是虚拟线路
    #[serde(default)]
    pub default_input_virtual: bool,
    /// 重采样质量档位（sinc 默认 / linear 低延迟）
    pub resample_quality: ResamplerQuality,
    /// 边环形缓冲容量（ms，钳制 50..=1000；加大更抗卡顿，不影响日常延迟）
    pub edge_buffer_ms: u32,
    /// 电平推送间隔（ms，钳制 20..=500）：后端向前端推电平的频率，越小电平条越顺滑、CPU 略高
    pub levels_interval_ms: u64,
    /// 频谱分析开关（默认关闭：关闭时音频线程不采样、stats 不计算频段）
    pub fft_enabled: bool,
    /// FFT 窗口点数（1024/2048/4096，须为 2 的幂，非法值回退默认 4096）
    #[serde(default = "default_fft_size")]
    pub fft_size: u32,
    /// 频段边界频率（Hz，升序；段数 = 边界数，band 0 含第一边界以下，高于末边界不显示）
    #[serde(default = "default_fft_bands")]
    pub fft_bands: Vec<f32>,
}

fn default_fft_size() -> u32 {
    crate::fft::DEFAULT_FFT_SIZE as u32
}

fn default_fft_bands() -> Vec<f32> {
    crate::fft::DEFAULT_BAND_EDGES.to_vec()
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
            default_output_virtual: false,
            default_input_virtual: false,
            resample_quality: ResamplerQuality::default(),
            edge_buffer_ms: 250,
            levels_interval_ms: 50,
            fft_enabled: false,
            fft_size: crate::fft::DEFAULT_FFT_SIZE as u32,
            fft_bands: crate::fft::DEFAULT_BAND_EDGES.to_vec(),
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
                nodes: Vec::new(),
            }],
            processors: vec![Processor {
                id: "p1".into(),
                node: DspNode {
                    kind: DspKind::Gain { db: 0.0 },
                    enabled: true,
                },
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
        assert!(g.processor("p1").is_some());
        assert!(g.source("nope").is_none());
        assert!(g.processor("nope").is_none());
    }

    #[test]
    fn validate_accepts_processor_endpoints_and_rejects_duplicates() {
        // 连线两端是 processor 也合法
        let mut g = valid_graph();
        g.routes[0].source_id = "p1".into();
        g.routes[0].sink_id = "p1".into(); // 引擎层环检测兜底；模型只查引用
        assert!(g.validate().is_ok());

        // processor id 重复
        let mut g = valid_graph();
        g.processors.push(Processor {
            id: "p1".into(),
            node: DspNode {
                kind: DspKind::Gain { db: 0.0 },
                enabled: true,
            },
        });
        assert!(matches!(g.validate(), Err(crate::Error::InvalidGraph(_))));
    }

    #[test]
    fn dsp_node_json_is_flat_for_frontend() {
        // 前端 TS 联合类型的形状：扁平 { type, enabled, ...参数 }
        let n: DspNode = serde_json::from_str(r#"{"type":"gain","enabled":true,"db":-6}"#).unwrap();
        assert_eq!(n.kind, DspKind::Gain { db: -6.0 });
        assert!(n.enabled);
        let json = serde_json::to_string(&n).unwrap();
        assert!(json.contains(r#""type":"gain""#), "应为扁平形状：{json}");

        // Processor 整体（id + flatten 节点）也要能从前端形状解析
        let p: Processor = serde_json::from_str(
            r#"{"id":"dsp-x","type":"peak_eq","enabled":true,"freq":1000,"gain_db":3,"q":1}"#,
        )
        .unwrap();
        assert_eq!(p.id, "dsp-x");
        assert_eq!(
            p.node.kind,
            DspKind::PeakEq {
                freq: 1000.0,
                gain_db: 3.0,
                q: 1.0
            }
        );

        // enabled 缺省 = true
        let n: DspNode = serde_json::from_str(r#"{"type":"delay","ms":50}"#).unwrap();
        assert!(n.enabled);
    }

    #[test]
    fn legacy_graph_without_processors_still_parses() {
        // 旧配置没有 processors 字段：补默认空列表而不是解析失败
        let json = r#"{"sources":[],"sinks":[],"routes":[]}"#;
        let g: GraphConfig = serde_json::from_str(json).unwrap();
        assert!(g.processors.is_empty());
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
        let s: Settings =
            serde_json::from_str(r#"{"control_api":{"enabled":true,"bind":"0.0.0.0","port":1}}"#)
                .unwrap();
        assert!(s.control_api.enabled);
        assert_eq!(s.control_api.port, 1);
        assert!(s.close_to_tray, "未指定的字段应取默认值");
    }

    #[test]
    fn control_api_auth_defaults_and_warning() {
        // 老配置没有 token / cors 字段 → 空令牌 + 关 CORS
        let mut api: ControlApiSettings =
            serde_json::from_str(r#"{"enabled":true,"bind":"127.0.0.1","port":17643}"#).unwrap();
        assert!(api.token.is_empty());
        assert!(!api.cors);
        assert!(api.loopback_only());
        assert!(api.security_warning().is_none(), "只监听回环时不应告警");

        // 监听全网且无令牌 → 告警；设了令牌就不再告警
        api.bind = "0.0.0.0".into();
        assert!(api.security_warning().is_some());
        api.token = ControlApiSettings::generate_token();
        assert_eq!(api.token.len(), 32);
        assert!(api.security_warning().is_none());
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
            sample_rate: 96_000,
            bits: 24,
            mode: UsbIpCableMode::Mixer,
            buffer_ms: 120,
        });
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        let c = back.usbip.cable(3).unwrap();
        assert_eq!(c.sample_rate, 96_000);
        assert_eq!(c.bits, 24);
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
            let c = UsbIpCableSettings {
                number: 2,
                mode,
                ..Default::default()
            };
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
        let c = UsbIpCableSettings {
            number: 7,
            ..Default::default()
        };
        assert_eq!(c.display_name(), "Virtual Cable 07");
        let c = UsbIpCableSettings {
            number: 7,
            name: "    ".into(),
            ..Default::default()
        };
        assert_eq!(c.display_name(), "Virtual Cable 07", "空白名视为未命名");
        let c = UsbIpCableSettings {
            number: 7,
            name: "  直播线  ".into(),
            ..Default::default()
        };
        assert_eq!(c.display_name(), "直播线", "两端空白应去掉");
    }

    #[test]
    fn cable_name_is_validated() {
        let ok = UsbIpCableSettings {
            name: "あ".repeat(UsbIpCableSettings::MAX_NAME_CHARS),
            ..Default::default()
        };
        assert!(ok.validate().is_ok(), "上限内应通过（按字符而非字节计）");
        let too_long = UsbIpCableSettings {
            name: "x".repeat(UsbIpCableSettings::MAX_NAME_CHARS + 1),
            ..Default::default()
        };
        assert!(matches!(
            too_long.validate(),
            Err(crate::Error::InvalidSettings(_))
        ));
        let control = UsbIpCableSettings {
            name: "a\nb".into(),
            ..Default::default()
        };
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
    fn legacy_cable_json_with_protocol_still_parses() {
        // 旧配置里带 protocol 字段（UAC1/UAC2 选择）：已删除该字段，但必须仍能解析
        // （serde 默认忽略未知字段），否则老用户一升级就"配置损坏"。
        let legacy: UsbIpCableSettings =
            serde_json::from_str(r#"{"number":1,"sample_rate":48000,"bits":16,"protocol":"uac2"}"#)
                .unwrap();
        assert_eq!(
            (legacy.number, legacy.sample_rate, legacy.bits),
            (1, 48_000, 16)
        );
        assert!(legacy.validate().is_ok());
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
        // 内置线路 = UAC1 全速：88.2k 及以下 16/24/32bit 都行，96k 只 16bit
        for (rate, bits) in [
            (44_100u32, 16u16),
            (48_000, 16),
            (48_000, 24),
            (48_000, 32),
            (88_200, 24),
            (88_200, 32),
            (96_000, 16),
        ] {
            let cfg = UsbIpCableSettings {
                number: 1,
                sample_rate: rate,
                bits,
                ..Default::default()
            };
            assert!(cfg.validate().is_ok(), "{rate}/{bits} 应合法");
        }
        // 96 kHz 以上一律拒绝（实测主机侧 ISO OUT 无法稳定承载，见 TASK.md P1）
        for rate in [176_400u32, 192_000] {
            for bits in [16u16, 24, 32] {
                let cfg = UsbIpCableSettings {
                    number: 1,
                    sample_rate: rate,
                    bits,
                    ..Default::default()
                };
                assert!(
                    matches!(cfg.validate(), Err(crate::Error::InvalidSettings(_))),
                    "{rate}/{bits} 必须被拒绝（超出内置线路规格）"
                );
                assert!(!UsbIpCableSettings::is_supported(rate, bits));
            }
        }
        // 88.2k 以上只支持 16bit：双向（OUT+IN 同帧）96k/24 = 1152 B/ms 超全速帧预算
        for bits in [24u16, 32] {
            let cfg = UsbIpCableSettings {
                number: 1,
                sample_rate: 96_000,
                bits,
                ..Default::default()
            };
            assert!(
                matches!(cfg.validate(), Err(crate::Error::InvalidSettings(_))),
                "96k/{bits} 必须被拒绝（双向带宽超限）"
            );
            assert!(!UsbIpCableSettings::is_supported(96_000, bits));
        }
    }

    #[test]
    fn usbip_cable_clamps_unsupported_format() {
        // 旧配置里可能是 192k/24（当年 UAC2 规划的规格）：载入时应降级而不是报错
        let mut c = UsbIpCableSettings {
            number: 2,
            sample_rate: 192_000,
            bits: 24,
            ..Default::default()
        };
        let note = c.clamp_supported().expect("应给出降级说明");
        assert!(note.contains("192000/24bit"), "{note}");
        assert_eq!(
            (c.sample_rate, c.bits),
            (96_000, 16),
            "96k 只支持 16bit，位深一并降级"
        );
        assert!(c.validate().is_ok());
        // 96k/24 同样要降到 96k/16
        let mut hi = UsbIpCableSettings {
            number: 1,
            sample_rate: 96_000,
            bits: 24,
            ..Default::default()
        };
        assert!(hi.clamp_supported().is_some());
        assert_eq!((hi.sample_rate, hi.bits), (96_000, 16));
        // 合法组合不该被改动
        let mut ok = UsbIpCableSettings {
            number: 1,
            sample_rate: 96_000,
            bits: 16,
            ..Default::default()
        };
        assert!(ok.clamp_supported().is_none());
        assert_eq!((ok.sample_rate, ok.bits), (96_000, 16));
        // 垃圾值兜底
        let mut bad = UsbIpCableSettings {
            number: 1,
            sample_rate: 1,
            bits: 9,
            ..Default::default()
        };
        assert!(bad.clamp_supported().is_some());
        assert!(bad.validate().is_ok(), "{:?}", (bad.sample_rate, bad.bits));
    }

    #[test]
    fn usbip_validate_rejects_bad_format() {
        let bad_rate = UsbIpCableSettings {
            sample_rate: 8_000,
            ..Default::default()
        };
        assert!(matches!(
            bad_rate.validate(),
            Err(crate::Error::InvalidSettings(_))
        ));
        let bad_bits = UsbIpCableSettings {
            bits: 8,
            ..Default::default()
        };
        assert!(bad_bits.validate().is_err());
        let bad_number = UsbIpCableSettings {
            number: 0,
            ..Default::default()
        };
        assert!(bad_number.validate().is_err());
        let bad_buffer = UsbIpCableSettings {
            buffer_ms: 1,
            ..Default::default()
        };
        assert!(bad_buffer.validate().is_err());
    }

    #[test]
    fn usbip_validate_rejects_duplicates_and_overflow() {
        let cable = UsbIpCableSettings {
            number: 1,
            ..Default::default()
        };
        let mut s = UsbIpSettings {
            enabled: true,
            ..Default::default()
        };
        s.cables = vec![cable.clone(), cable];
        assert!(matches!(
            s.validate(),
            Err(crate::Error::InvalidSettings(_))
        ));

        let mut s = UsbIpSettings::default();
        s.cables = (1..=UsbIpCableSettings::MAX_NUMBER)
            .map(|n| UsbIpCableSettings {
                number: n,
                ..Default::default()
            })
            .collect();
        assert!(s.validate().is_ok());
        assert_eq!(s.next_free_number(), None);
        s.cables.push(UsbIpCableSettings {
            number: 9,
            ..Default::default()
        });
        assert!(s.validate().is_err(), "超过 32 条应被拒绝");
    }

    #[test]
    fn usbip_next_free_number_finds_gap() {
        let s = UsbIpSettings {
            enabled: true,
            bind: UsbIpSettings::default().bind,
            cables: vec![
                UsbIpCableSettings {
                    number: 1,
                    ..Default::default()
                },
                UsbIpCableSettings {
                    number: 3,
                    ..Default::default()
                },
            ],
        };
        assert_eq!(s.next_free_number(), Some(2));
    }
}
