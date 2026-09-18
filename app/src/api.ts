// Tauri invoke 封装 + 与 Rust 结构对应的类型（snake_case JSON）
import { Channel, invoke } from "@tauri-apps/api/core";

export type DeviceKind = "input" | "output";
export type SourceMode = "deviceinput" | "loopback";

export interface DeviceInfo {
  id: string;
  name: string;
  kind: DeviceKind;
  is_default: boolean;
  is_virtual: boolean;
  channels: number;
  sample_rate: number;
}

export interface Source {
  id: string;
  name: string;
  device_id: string;
  mode: SourceMode;
  enabled: boolean;
}

export interface Sink {
  id: string;
  name: string;
  device_id: string;
  volume: number;
  enabled: boolean;
}

/** DSP 节点参数（type 字段区分节点类型，与 Rust serde tag="type" 对应） */
export type DspNode =
  | { type: "gain"; enabled: boolean; db: number }
  | { type: "delay"; enabled: boolean; ms: number }
  | {
    type: "eq3";
    enabled: boolean;
    low_gain_db: number;
    low_freq: number;
    mid_gain_db: number;
    mid_freq: number;
    mid_q: number;
    high_gain_db: number;
    high_freq: number;
  }
  | { type: "peak_eq"; enabled: boolean; freq: number; gain_db: number; q: number }
  | { type: "graph_eq"; enabled: boolean; gains_db: number[] }
  | { type: "highpass"; enabled: boolean; freq: number; q: number }
  | { type: "lowpass"; enabled: boolean; freq: number; q: number }
  /** 带通：起始/终止频率（中心 = 几何平均，Q = 中心/带宽） */
  | { type: "bandpass"; enabled: boolean; low_freq: number; high_freq: number }
  /** 开关节点：开 = 直通，关 = 静音 */
  | { type: "switch"; enabled: boolean }
  /** 限幅器：峰值包络压低超阈信号，防削波 */
  | { type: "limiter"; enabled: boolean; threshold_db: number; release_ms: number };

/** 画布上的 DSP 处理方块：id + 节点类型/参数（与 Rust serde flatten 对应） */
export type Processor = { id: string } & DspNode;

export interface Route {
  id: string;
  source_id: string;
  sink_id: string;
  gain: number;
  muted: boolean;
  /** 已废弃：被画布 DSP 方块取代，保留字段只为旧配置兼容 */
  nodes: DspNode[];
}

export interface GraphConfig {
  sources: Source[];
  sinks: Sink[];
  routes: Route[];
  /** 画布上的 DSP 处理方块（源 → 方块… → 汇，按路径串联） */
  processors: Processor[];
}

export interface Settings {
  control_api: {
    enabled: boolean;
    bind: string;
    port: number;
    /** 访问令牌：非空时 /api/* 需带 `Authorization: Bearer <令牌>`（空 = 不鉴权） */
    token: string;
    /** 允许浏览器跨域调用（默认关闭：开了之后任意网页都能操控本机混音器） */
    cors: boolean;
  };
  usbip: { enabled: boolean; bind: string; cables: UsbIpCable[] };
  autostart_headless: boolean;
  close_to_tray: boolean;
  /** 重采样质量：sinc256（默认，高质量）/ sinc128（低延迟）/ linear（零延迟） */
  resample_quality: "sinc256" | "sinc128" | "linear";
  /** 边缓冲容量（ms，50..=1000，加大更抗卡顿，不影响日常延迟） */
  edge_buffer_ms: number;
  /** 电平推送间隔（ms，20..=500，越小电平条越顺滑、CPU 略高） */
  levels_interval_ms: number;
  /** 频谱分析开关（默认关闭：关闭时音频线程不采样、无频谱数据） */
  fft_enabled: boolean;
  /** FFT 窗口点数（1024/2048/4096，须为 2 的幂） */
  fft_size: number;
  /** 频段边界频率（Hz，升序；段数 = 边界数，band 0 含第一边界以下） */
  fft_bands: number[];
}

export interface ApiStatus {
  running: boolean;
  addr: string | null;
  /** 可直接拼端点用，如 `${base_url}/api/graph` */
  base_url: string | null;
  /** 运行中的服务是否启用了令牌鉴权 */
  auth_enabled: boolean;
  /** 设置里是否有令牌（保存后生效） */
  token_set: boolean;
  cors: boolean;
}

/** 电平推送载荷：节点电平 + 频段 dB（fft 关闭时 spectra 为空对象） */
export interface LevelsPayload {
  levels: Record<string, number>;
  spectra: Record<string, number[]>;
}

// ---------- USB/IP 虚拟声卡（usbip-win2 + UAC2）----------

export type UsbIpCableMode = "loopback" | "mixer" | "reverse";

/**
 * 内置虚拟线路只有 **UAC1**（USB Audio 1.0 / USB 1.1 全速 → Windows 自带 usbaudio.sys）。
 *
 * 每 1ms 一个包、单包上限 1023 字节，且 OUT+IN 两个方向共享全速帧预算，所以支持矩阵是：
 * 88.2kHz 及以下 16/24/32bit，96kHz 只 16bit（双向带宽限制）。
 * 需要更高规格（192k/24bit 等）的线路请自行安装第三方虚拟声卡（VB-CABLE 等），
 * 它们会作为普通 Windows 端点出现在混音画布里。
 */

/** 一条虚拟线缆的配置（后端 UsbIpCableSettings） */
export interface UsbIpCable {
  /** 线缆号 1..=32，对应 busid 1-N 与名称 Virtual Cable NN */
  number: number;
  /** 显示名（USB 产品字符串）；空则显示 Virtual Cable NN */
  name: string;
  /** 44100 / 48000 / 88200 / 96000（内置线路 UAC1 全速实测上限） */
  sample_rate: number;
  /** 16 / 24 / 32（88.2k 以上只支持 16bit：全速 USB 双向带宽限制） */
  bits: number;
  mode: UsbIpCableMode;
  buffer_ms: number;
}

/** 一条线缆的运行态 */
export interface UsbIpCableStatus {
  number: number;
  /** 自定义名（用户输入原值，可能为空） */
  name: string;
  /** 实际显示名（空名回退 Virtual Cable NN） */
  display_name: string;
  sample_rate: number;
  bits: number;
  mode: string;
  buffer_ms: number;
  device_id_playback: string;
  device_id_capture: string;
  attached: boolean;
  port: number | null;
}

/** usbip.exe / 传输驱动状态 */
export interface UsbIpDriverInfo {
  installed: boolean;
  usbip_path: string | null;
  installer_path: string | null;
  /** installer_path 是内嵌副本（单文件分发，安装时才释放到磁盘） */
  installer_embedded: boolean;
  test_signing: boolean | null;
  hvci_enabled: boolean | null;
}

/** `usbip port` 里的一条记录 */
export interface AttachedPort {
  port: number;
  in_use: boolean;
  bus_id: string | null;
  detail: string;
}

export interface UsbIpStatus {
  enabled: boolean;
  running: boolean;
  bind: string;
  local_addr: string | null;
  cables: UsbIpCableStatus[];
  driver: UsbIpDriverInfo;
  ports: AttachedPort[];
  /** `usbip port` 失败原因（多为需要管理员权限） */
  ports_error: string | null;
}

/** 「附加全部」的结果 */
export interface AttachReport {
  host: string;
  tcp_port: number;
  attached: [string, number | null][];
  failed: [string, string][];
  detached: number[];
  log: string;
}

/** 混音画布节点位置（归一化 [x, y]）：node key → 坐标 */
export type NodePos = [number, number];

/** 一条运行日志 */
export interface LogLine {
  seq: number;
  text: string;
}

/** 画布 node key：与设备/线路稳定对应（源与 sink 的随机 id 不适合做布局键） */
export const nodeKey = {
  input: (deviceId: string) => `in:${deviceId}`,
  loopback: (deviceId: string) => `loop:${deviceId}`,
  output: (deviceId: string) => `out:${deviceId}`,
  /** 虚拟线路拆成两个节点：输入＝系统播放端（混音图的源）、输出＝系统录音端（混音图的汇） */
  cablePlay: (number: number) => `cable:${number}:play`,
  cableRec: (number: number) => `cable:${number}:rec`,
};

export const api = {
  listDevices: () => invoke<DeviceInfo[]>("list_devices"),  refreshDevices: () => invoke<DeviceInfo[]>("refresh_devices"),
  /** 设为 Windows 默认设备（播放/录音均可），返回刷新后的设备列表 */
  setDefaultDevice: (deviceId: string) =>
    invoke<DeviceInfo[]>("set_default_device", { deviceId }),
  /** 读取端点的 Windows 系统音量（0..1） */
  getDeviceVolume: (deviceId: string) => invoke<number>("get_device_volume", { deviceId }),
  /** 设置端点的 Windows 系统音量（影响该设备上所有声音） */
  setDeviceVolume: (deviceId: string, level: number) =>
    invoke<void>("set_device_volume", { deviceId, level }),
  getDeviceMute: (deviceId: string) => invoke<boolean>("get_device_mute", { deviceId }),
  setDeviceMute: (deviceId: string, mute: boolean) =>
    invoke<void>("set_device_mute", { deviceId, mute }),
  getGraph: () => invoke<GraphConfig>("get_graph"),
  applyGraph: (graph: GraphConfig) => invoke<GraphConfig>("apply_graph", { graph }),
  getMixerLayout: () => invoke<Record<string, NodePos>>("get_mixer_layout"),
  setMixerLayout: (layout: Record<string, NodePos>) =>
    invoke<void>("set_mixer_layout", { layout }),
  setRouteGain: (routeId: string, gain: number) =>
    invoke<void>("set_route_gain", { routeId, gain }),
  setRouteMuted: (routeId: string, muted: boolean) =>
    invoke<void>("set_route_muted", { routeId, muted }),
  /** 实测一条连线路径的延迟（ms）：注入扫频脉冲 + 相关检测，阻塞约 2.5~4s */
  measureRouteLatency: (routeId: string) =>
    invoke<number>("measure_route_latency", { routeId }),
  /** 实测「源节点 → 输出节点」之间路径的延迟（ms） */
  measureNodesLatency: (sourceId: string, sinkId: string) =>
    invoke<number>("measure_nodes_latency", { sourceId, sinkId }),
  setProcessorParams: (processorId: string, node: DspNode) =>
    invoke<void>("set_processor_params", { processorId, node }),
  setSinkVolume: (sinkId: string, volume: number) =>
    invoke<void>("set_sink_volume", { sinkId, volume }),
  /** 电平推送：后端线程 ~20fps 主动推（取代轮询），返回 Promise 在订阅完成后 resolve */
  subscribeLevels(onLevels: (payload: LevelsPayload) => void): Promise<void> {
    const channel = new Channel<LevelsPayload>();
    channel.onmessage = onLevels;
    return invoke("subscribe_levels", { channel });
  },
  unsubscribeLevels: () => invoke<void>("unsubscribe_levels"),
  /** 设备列表推送：热插拔/看门狗枚举有变化才推（取代固定周期轮询） */
  subscribeDevices(onDevices: (devices: DeviceInfo[]) => void): Promise<void> {
    const channel = new Channel<DeviceInfo[]>();
    channel.onmessage = onDevices;
    return invoke("subscribe_devices", { channel });
  },
  unsubscribeDevices: () => invoke<void>("unsubscribe_devices"),
  getSettings: () => invoke<Settings>("get_settings"),
  updateSettings: (settings: Settings) => invoke<void>("update_settings", { settings }),
  getControlApiStatus: () => invoke<ApiStatus>("get_control_api_status"),
  setControlApiEnabled: (enabled: boolean) =>
    invoke<void>("set_control_api_enabled", { enabled }),
  getAutostart: () => invoke<boolean>("get_autostart"),
  setAutostart: (enabled: boolean) => invoke<void>("set_autostart", { enabled }),
  // USB/IP 虚拟声卡
  usbipStatus: () => invoke<UsbIpStatus>("usbip_status"),
  usbipSetCables: (enabled: boolean, cables: UsbIpCable[]) =>
    invoke<UsbIpStatus>("usbip_set_cables", { enabled, cables }),
  /** 检测本机端口是否空闲（false = 已被占用） */
  usbipPortAvailable: (port: number) => invoke<boolean>("usbip_port_available", { port }),
  /** 修改虚拟声卡服务器监听端口（服务器运行中会被拒绝） */
  usbipSetPort: (port: number) => invoke<UsbIpStatus>("usbip_set_port", { port }),
  usbipAttachAll: () => invoke<AttachReport>("usbip_attach_all"),
  usbipDetachAll: () => invoke<string>("usbip_detach_all"),
  usbipInstallDriver: () => invoke<string>("usbip_install_driver"),
  // 运行日志
  getLogs: (since: number) => invoke<{ lines: LogLine[]; next: number }>("get_logs", { since }),
  clearLogs: () => invoke<void>("clear_logs"),
  quitApp: () => invoke<void>("quit_app"),
};

// 增益换算：dB ↔ 线性
export const dbToGain = (db: number) => Math.pow(10, db / 20);
export const gainToDb = (g: number) => (g <= 0 ? -60 : 20 * Math.log10(g));
