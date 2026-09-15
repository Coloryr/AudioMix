// Tauri invoke 封装 + 与 Rust 结构对应的类型（snake_case JSON）
import { invoke } from "@tauri-apps/api/core";

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
  | { type: "switch"; enabled: boolean };

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
  control_api: { enabled: boolean; bind: string; port: number };
  usbip: { enabled: boolean; bind: string; cables: UsbIpCable[] };
  autostart_headless: boolean;
  close_to_tray: boolean;
  /** 重采样质量：sinc256（默认，高质量）/ sinc128（低延迟）/ linear（零延迟） */
  resample_quality: "sinc256" | "sinc128" | "linear";
  /** 边缓冲容量（ms，50..=1000，加大更抗卡顿，不影响日常延迟） */
  edge_buffer_ms: number;
}

export interface ApiStatus {
  running: boolean;
  addr: string | null;
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
  listDevices: () => invoke<DeviceInfo[]>("list_devices"),
  refreshDevices: () => invoke<DeviceInfo[]>("refresh_devices"),
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
  setProcessorParams: (processorId: string, node: DspNode) =>
    invoke<void>("set_processor_params", { processorId, node }),
  setSinkVolume: (sinkId: string, volume: number) =>
    invoke<void>("set_sink_volume", { sinkId, volume }),
  getLevels: () => invoke<Record<string, number>>("get_levels"),
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
