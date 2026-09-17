// 混音画布的共享类型 / 常量 / 纯函数 —— MixerView 与 mixer/ 下的子组件共用。
// 这里只放不依赖组件状态的纯逻辑；拖拽引擎、布局持久化留在 MixerView。
import type { DspNode, NodePos, Sink } from "../../api";

export type { NodePos };

// ---------- 画布几何 ----------
export const NODE_W = 236;
// 96 → 108：副标题放开到两行后，内容（标题 + 两行副标题 + 电平条 + 音量条）需要更高一些
export const NODE_H = 116;
/** 端子环半径：连线两端收在环外缘，线不进环、箭头贴着环不被环线穿过 */
export const TERM_R = 8;

// ---------- 节点模型 ----------
export type NodeKind = "input" | "loopback" | "output" | "cable_play" | "cable_rec" | "dsp";

/** 画布节点（渲染视图模型，x/y 为相对画布左上角的像素坐标） */
export interface GNode {
  key: string;
  kind: NodeKind;
  title: string;
  subtitle: string;
  /** 右端子（信号流出）对应的 Source */
  sourceId?: string;
  /** 左端子（信号流入）对应的 Sink */
  sinkId?: string;
  /** DSP 处理方块（左入右出）对应的 Processor */
  processorId?: string;
  /** 设备 id（回声节点＝抓取的输出设备；输出汇＝目标输出设备），用于禁止回声回送同一设备 */
  deviceId?: string;
  cableNumber?: number;
  hasIn: boolean;
  hasOut: boolean;
  /** 端子贴在盒子的哪一侧（输入设备在右、输出设备在左、虚拟线路左进右出） */
  inSide: "left" | "right";
  outSide: "left" | "right";
  x: number;
  y: number;
}

export interface Wire {
  id: string;
  from: { x: number; y: number; dir: number };
  to: { x: number; y: number; dir: number };
  gain: number;
  muted: boolean;
  fromTitle: string;
  toTitle: string;
}

export type PaletteKind = "input" | "loopback" | "output" | "cable_play" | "cable_rec" | "dsp";
export interface PaletteItem {
  kind: PaletteKind;
  title: string;
  deviceId?: string;
  /** kind === "dsp" 时的节点类型 */
  dspType?: DspType;
}

export const KIND_META: Record<NodeKind, { tag: string; type: "default" | "info" | "success" | "warning" }> = {
  input: { tag: "输入设备", type: "info" },
  loopback: { tag: "系统回声", type: "default" },
  output: { tag: "输出设备", type: "success" },
  cable_play: { tag: "线路输出", type: "warning" },
  cable_rec: { tag: "线路输入", type: "warning" },
  dsp: { tag: "DSP", type: "warning" },
};

// ---------- 端子 ----------
/** 端子所在的边（按设备角色：输入设备在左、输出设备在右） */
export function termSide(node: GNode, which: "in" | "out"): "left" | "right" {
  return which === "in" ? node.inSide : node.outSide;
}

/** 端子坐标 + 出线方向（dir：-1 向左出线，+1 向右出线）；取整与节点的渲染位置对齐 */
export function termPos(node: GNode, which: "in" | "out") {
  const side = termSide(node, which);
  return {
    x: Math.round(side === "left" ? node.x : node.x + NODE_W),
    y: Math.round(node.y + NODE_H / 2),
    dir: side === "left" ? -1 : 1,
  };
}

export function termStyle(node: GNode, which: "in" | "out") {
  const side = termSide(node, which);
  return side === "left"
    ? { left: "-8px", right: "auto", top: "50%", transform: "translateY(-50%)" }
    : { right: "-8px", left: "auto", top: "50%", transform: "translateY(-50%)" };
}

/** 端子提示（线路节点按「线路输入＝Windows 录制端、线路输出＝Windows 播放端」称呼） */
export function termTitle(node: GNode, which: "in" | "out") {
  if (node.cableNumber !== undefined) {
    const line =
      node.kind === "cable_rec"
        ? "线路输入 · 系统录音端（麦克风）：混音器写这里，别的软件从这录"
        : "线路输出 · 系统播放端（扬声器）：别的软件播进这里，混音器从这读";
    return `${line}（${which === "in" ? "信号流入" : "信号流出"}端）`;
  }
  return which === "in" ? "输入端子（信号流入）" : "输出端子（信号流出）";
}

/** 取某个 sink 节点对应的设备 id（系统音量条用） */
export function sinkDeviceIdOf(node: GNode, sinks: Sink[]): string | undefined {
  if (!node.sinkId) return undefined;
  return sinks.find((s) => s.id === node.sinkId)?.device_id;
}

// ---------- 连线（贝塞尔） ----------
/**
 * 连线：控制点朝**目标所在的一侧**外扩 —— 端子虽然固定在盒子某一边，
 * 但目标在左时曲线就从节点左侧出线（起点藏到节点底下），不再绕到节点背后兜圈；
 * 终点方向保持端子的出线方向（箭头始终从外面戳向端子圆）。
 */
export function wirePath(from: { x: number; y: number; dir: number }, to: { x: number; y: number; dir?: number }) {
  const d = Math.min(90, Math.max(36, Math.abs(to.x - from.x) * 0.4));
  // 出发方向跟目标走（正下方时保持端子自身方向）；拉线预览的终点是鼠标位置、没有方向按 0 处理
  const td = to.dir ?? 0;
  const fd = Math.abs(to.x - from.x) < 1 ? from.dir : Math.sign(to.x - from.x);
  // 控制点**不做**画布内 clamp：贴边节点的 c2 被 clamp 后会跑到端点另一侧，
  // 曲线末段方向反转 → 箭头调头背对节点、整颗悬在端子环外面（贴画布左/右边的节点必现）。
  // 贝塞尔控制点在 viewBox 外是完全合法的，不影响渲染。
  const c1 = from.x + fd * d;
  const c2 = to.x + (td || fd) * d;
  // 两端都收在端子环外缘（沿各自朝向回退半径距离）：线不穿过环内部，箭头尖正好贴着环
  const sx = from.x + fd * TERM_R;
  const ex = (to.x + (td || fd) * TERM_R) - 10;
  return `M ${sx} ${from.y} C ${c1} ${from.y}, ${c2} ${to.y}, ${ex} ${to.y}`;
}

/** 拉线预览：起点沿端子自身朝向出线，终点顺「远离起点」的方向进线（跟手的 S 弯） */
export function previewWirePath(from: { x: number; y: number; dir: number }, to: { x: number; y: number }) {
  const d = Math.min(90, Math.max(36, Math.abs(to.x - from.x) * 0.4));
  const fd = from.dir;
  const ed = Math.abs(to.x - from.x) < 1 ? from.dir : Math.sign(to.x - from.x);
  const c1 = from.x + fd * d;
  const c2 = to.x - ed * d;
  // 起点从端子环外缘出发（终点是鼠标位置，直接到光标）
  const sx = from.x + fd * TERM_R;
  return `M ${sx} ${from.y} C ${c1} ${from.y}, ${c2} ${to.y}, ${to.x} ${to.y}`;
}

// ---------- DSP 元数据与参数定义 ----------
export type DspType = DspNode["type"];

export const DSP_META: Record<DspType, { label: string }> = {
  gain: { label: "增益" },
  delay: { label: "延迟" },
  eq3: { label: "三段均衡" },
  peak_eq: { label: "峰式均衡" },
  graph_eq: { label: "图形均衡 10 段" },
  highpass: { label: "高通" },
  lowpass: { label: "低通" },
  bandpass: { label: "带通" },
  switch: { label: "开关" },
  limiter: { label: "限幅器" },
};

export const DSP_ADD_OPTIONS = (Object.keys(DSP_META) as DspType[]).map((t) => ({
  label: DSP_META[t].label,
  value: t,
}));

/** 图形均衡中心频率标签（与 Rust GRAPH_EQ_BANDS 一致） */
export const DSP_GEQ_BANDS = ["31", "63", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"];

export interface ParamDef {
  key: string;
  label: string;
  min: number;
  max: number;
  step: number;
  unit?: string;
  /** 频率类参数：滑杆在 log10 域走（人耳对频率是对数感知，线性轴低频端根本调不开） */
  log?: boolean;
}

/** 各节点类型的参数滑杆定义（graph_eq 走专用 10 段竖向滑杆，不在此列） */
export const DSP_PARAMS: Record<Exclude<DspType, "graph_eq">, ParamDef[]> = {
  gain: [{ key: "db", label: "增益", min: -60, max: 12, step: 0.5, unit: " dB" }],
  delay: [{ key: "ms", label: "延迟", min: 0, max: 1000, step: 1, unit: " ms" }],
  eq3: [
    { key: "low_gain_db", label: "低增益", min: -24, max: 24, step: 0.5, unit: " dB" },
    { key: "low_freq", label: "低频点", min: 20, max: 20000, step: 10, unit: " Hz", log: true },
    { key: "mid_gain_db", label: "中增益", min: -24, max: 24, step: 0.5, unit: " dB" },
    { key: "mid_freq", label: "中频点", min: 20, max: 20000, step: 10, unit: " Hz", log: true },
    { key: "mid_q", label: "中 Q 值", min: 0.3, max: 10, step: 0.1 },
    { key: "high_gain_db", label: "高增益", min: -24, max: 24, step: 0.5, unit: " dB" },
    { key: "high_freq", label: "高频点", min: 20, max: 20000, step: 10, unit: " Hz", log: true },
  ],
  peak_eq: [
    { key: "freq", label: "频率", min: 20, max: 20000, step: 10, unit: " Hz", log: true },
    { key: "gain_db", label: "增益", min: -24, max: 24, step: 0.5, unit: " dB" },
    { key: "q", label: "Q 值", min: 0.3, max: 10, step: 0.1 },
  ],
  highpass: [
    { key: "freq", label: "频率", min: 20, max: 20000, step: 10, unit: " Hz", log: true },
    { key: "q", label: "Q 值", min: 0.3, max: 10, step: 0.1 },
  ],
  lowpass: [
    { key: "freq", label: "频率", min: 20, max: 20000, step: 10, unit: " Hz", log: true },
    { key: "q", label: "Q 值", min: 0.3, max: 10, step: 0.1 },
  ],
  bandpass: [
    { key: "low_freq", label: "起始", min: 20, max: 19000, step: 10, unit: " Hz", log: true },
    { key: "high_freq", label: "终止", min: 30, max: 20000, step: 10, unit: " Hz", log: true },
  ],
  switch: [],
  limiter: [
    { key: "threshold_db", label: "阈值", min: -24, max: 0, step: 0.5, unit: " dB" },
    { key: "release_ms", label: "释放", min: 10, max: 500, step: 5, unit: " ms" },
  ],
};

/** log 参数 → 滑杆值（log10 域，钳在参数范围内） */
export function sliderVal(p: ParamDef, v: number): number {
  if (!p.log) return v;
  return Math.log10(Math.min(p.max, Math.max(p.min, v || p.min)));
}

/** 滑杆值 → 实际参数：log 域取回线性并取整（≥1k 取 10 的倍数，避免 1503Hz 这种脏值） */
export function sliderCommit(p: ParamDef, v: number): number {
  if (!p.log) return v;
  const hz = 10 ** v;
  const r = hz >= 1000 ? Math.round(hz / 10) * 10 : Math.round(hz);
  return Math.min(p.max, Math.max(p.min, r));
}

/** 滑杆 tooltip 文案 */
export function sliderTip(p: ParamDef, v: number): string {
  if (p.log) return `${Math.round(10 ** v)} Hz`;
  return v.toFixed(p.step < 1 ? 1 : 0) + (p.unit ?? "");
}

/** DSP 方块副标题：关键参数摘要（随参数变化实时刷新） */
export function dspSubtitle(p: DspNode): string {
  const db = (v: number) => `${v > 0 ? "+" : ""}${v.toFixed(1).replace(/\.0$/, "")}dB`;
  switch (p.type) {
    case "gain":
      return db(p.db);
    case "delay":
      return `${p.ms.toFixed(0)} ms`;
    case "eq3":
      return `低 ${db(p.low_gain_db)} · 中 ${db(p.mid_gain_db)} · 高 ${db(p.high_gain_db)}`;
    case "peak_eq":
      return `${p.freq.toFixed(0)}Hz ${db(p.gain_db)}`;
    case "graph_eq":
      return p.gains_db.every((g) => g === 0) ? "10 段平直" : "10 段自定义";
    case "highpass":
      return `≥ ${p.freq.toFixed(0)} Hz`;
    case "lowpass":
      return `≤ ${p.freq.toFixed(0)} Hz`;
    case "bandpass":
      return `${p.low_freq.toFixed(0)}–${p.high_freq.toFixed(0)} Hz`;
    case "switch":
      return p.enabled ? "开 · 直通" : "关 · 静音";
    case "limiter":
      return `≤ ${p.threshold_db.toFixed(1)}dB 释放 ${p.release_ms.toFixed(0)}ms`;
  }
}

/** 各类型 DSP 方块的默认参数（enabled=true；重置时保留原 enabled） */
export function makeDspNode(t: DspType): DspNode {
  switch (t) {
    case "gain":
      return { type: "gain", enabled: true, db: 0 };
    case "delay":
      return { type: "delay", enabled: true, ms: 50 };
    case "eq3":
      return { type: "eq3", enabled: true, low_gain_db: 0, low_freq: 200, mid_gain_db: 0, mid_freq: 1000, mid_q: 1.0, high_gain_db: 0, high_freq: 4000 };
    case "peak_eq":
      return { type: "peak_eq", enabled: true, freq: 1000, gain_db: 0, q: 1.0 };
    case "graph_eq":
      return { type: "graph_eq", enabled: true, gains_db: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0] };
    case "highpass":
      return { type: "highpass", enabled: true, freq: 100, q: 0.707 };
    case "lowpass":
      return { type: "lowpass", enabled: true, freq: 10000, q: 0.707 };
    case "bandpass":
      return { type: "bandpass", enabled: true, low_freq: 300, high_freq: 3000 };
    case "switch":
      return { type: "switch", enabled: true };
    case "limiter":
      return { type: "limiter", enabled: true, threshold_db: -3, release_ms: 50 };
  }
}

/** DSP 方块上的状态小字（开关在悬浮弹窗里，方块只负责显示） */
export function procStateBadge(p: DspNode | undefined): { text: string; cls: string } {
  if (!p) return { text: "", cls: "off" };
  if (!p.enabled) return { text: p.type === "switch" ? "关 · 静音" : "已旁路", cls: "off" };
  return { text: p.type === "switch" ? "开 · 直通" : "已启用", cls: "on" };
}

// ---------- 频响曲线（RBJ 公式，与后端 biquad crate 同源） ----------
export const CURVE_W = 236;
export const CURVE_H = 84;
export const CURVE_FS = 48000;

/** dB → y 像素（+24 .. -24 dB 映射到 0 .. CURVE_H）——±24 才装得下 EQ 最大提升/衰减 */
export function curveY(db: number): number {
  return ((24 - Math.max(-24, Math.min(24, db))) / 48) * CURVE_H;
}

/** 频率 → x 像素（20Hz..20kHz 对数轴） */
export function curveX(f: number): number {
  return (Math.log10(Math.max(20, Math.min(20000, f)) / 20) / Math.log10(1000)) * CURVE_W;
}

/** RBJ 双二阶在某频率的幅度（dB） */
export function rbjMagnitudeDb(c: { b0: number; b1: number; b2: number; a1: number; a2: number }, f: number): number {
  const w = (2 * Math.PI * f) / CURVE_FS;
  const w2 = 2 * w;
  const c1 = Math.cos(w);
  const s1 = Math.sin(w);
  const c2 = Math.cos(w2);
  const s2 = Math.sin(w2);
  const numRe = c.b0 + c.b1 * c1 + c.b2 * c2;
  const numIm = -(c.b1 * s1 + c.b2 * s2);
  const denRe = 1 + c.a1 * c1 + c.a2 * c2;
  const denIm = -(c.a1 * s1 + c.a2 * s2);
  const mag =
    Math.sqrt((numRe * numRe + numIm * numIm) / (denRe * denRe + denIm * denIm + 1e-30));
  return 20 * Math.log10(mag + 1e-30);
}

export interface DspCurveData {
  d: string;
  marks: number[];
  band: [number, number] | null;
}

/**
 * DSP 方块的频响曲线数据。带 Q 值的节点（高通/低通/带通/峰式均衡/三段均衡）
 * 都画：Q 越大峰越尖、隆起越窄，调 Q 滑杆时曲线即时跟着变。
 */
export function dspCurveOf(p: DspNode): DspCurveData | null {
  // 单个双二阶的系数组（RBJ cookbook 公式，与后端 biquad crate 同源）
  type Coeffs = { b0: number; b1: number; b2: number; a1: number; a2: number };
  let coeffs: Coeffs[] = []; // 级联滤波器的幅度 dB 相加
  let marks: number[] = [];
  let band: [number, number] | null = null;

  // 峰式均衡（RBJ PeakingEQ；Q 即「Q 值 / 中 Q 值」滑杆）
  const peaking = (f0: number, gainDb: number, q: number): Coeffs => {
    const A = 10 ** (gainDb / 40);
    const w0 = (2 * Math.PI * f0) / CURVE_FS;
    const alpha = Math.sin(w0) / (2 * q);
    const a0 = 1 + alpha / A;
    return {
      b0: (1 + alpha * A) / a0,
      b1: (-2 * Math.cos(w0)) / a0,
      b2: (1 - alpha * A) / a0,
      a1: (-2 * Math.cos(w0)) / a0,
      a2: (1 - alpha / A) / a0,
    };
  };
  // 架式均衡（RBJ Low/HighShelf，S=1 时 alpha 与后端 Q_BUTTERWORTH 一致）
  const shelfAlpha = (w0: number) => (Math.sin(w0) / 2) * Math.SQRT2;
  const lowShelf = (f0: number, gainDb: number): Coeffs => {
    const A = 10 ** (gainDb / 40);
    const w0 = (2 * Math.PI * f0) / CURVE_FS;
    const alpha = shelfAlpha(w0);
    const sa = 2 * Math.sqrt(A) * alpha;
    const a0 = A + 1 + (A - 1) * Math.cos(w0) + sa;
    return {
      b0: (A * (A + 1 - (A - 1) * Math.cos(w0) + sa)) / a0,
      b1: (2 * A * (A - 1 - (A + 1) * Math.cos(w0))) / a0,
      b2: (A * (A + 1 - (A - 1) * Math.cos(w0) - sa)) / a0,
      a1: (-2 * (A - 1 + (A + 1) * Math.cos(w0))) / a0,
      a2: (A + 1 + (A - 1) * Math.cos(w0) - sa) / a0,
    };
  };
  const highShelf = (f0: number, gainDb: number): Coeffs => {
    const A = 10 ** (gainDb / 40);
    const w0 = (2 * Math.PI * f0) / CURVE_FS;
    const alpha = shelfAlpha(w0);
    const sa = 2 * Math.sqrt(A) * alpha;
    const a0 = A + 1 - (A - 1) * Math.cos(w0) + sa;
    return {
      b0: (A * (A + 1 + (A - 1) * Math.cos(w0) + sa)) / a0,
      b1: (-2 * A * (A - 1 + (A + 1) * Math.cos(w0))) / a0,
      b2: (A * (A + 1 + (A - 1) * Math.cos(w0) - sa)) / a0,
      a1: (2 * (A - 1 - (A + 1) * Math.cos(w0))) / a0,
      a2: (A + 1 - (A - 1) * Math.cos(w0) - sa) / a0,
    };
  };

  if (p.type === "highpass" || p.type === "lowpass") {
    const w0 = (2 * Math.PI * p.freq) / CURVE_FS;
    const cw = Math.cos(w0);
    const alpha = Math.sin(w0) / (2 * p.q);
    const a0 = 1 + alpha;
    coeffs = [
      p.type === "highpass"
        ? { b0: (1 + cw) / 2 / a0, b1: (-(1 + cw)) / a0, b2: (1 + cw) / 2 / a0, a1: (-2 * cw) / a0, a2: (1 - alpha) / a0 }
        : { b0: (1 - cw) / 2 / a0, b1: (1 - cw) / a0, b2: (1 - cw) / 2 / a0, a1: (-2 * cw) / a0, a2: (1 - alpha) / a0 },
    ];
    marks = [p.freq];
  } else if (p.type === "bandpass") {
    // 与后端一致：中心 = 几何平均，Q = 中心/带宽，恒裙增益补 1/Q
    const center = Math.sqrt(p.low_freq * p.high_freq);
    const q = Math.max(0.05, center / (p.high_freq - p.low_freq));
    const w0 = (2 * Math.PI * center) / CURVE_FS;
    const cw = Math.cos(w0);
    const alpha = Math.sin(w0) / (2 * q);
    const a0 = 1 + alpha;
    const g = 1 / q; // biquad 的 BandPass 是恒裙变体（中心增益 = Q），补偿回 0dB
    coeffs = [{ b0: (alpha * g) / a0, b1: 0, b2: (-alpha * g) / a0, a1: (-2 * cw) / a0, a2: (1 - alpha) / a0 }];
    marks = [p.low_freq, p.high_freq];
    band = [p.low_freq, p.high_freq];
  } else if (p.type === "peak_eq") {
    coeffs = [peaking(p.freq, p.gain_db, p.q)];
    marks = [p.freq];
  } else if (p.type === "eq3") {
    // 三段级联 = 幅度 dB 相加；曲线的中间隆起宽度随「中 Q 值」滑杆变化
    coeffs = [
      lowShelf(p.low_freq, p.low_gain_db),
      peaking(p.mid_freq, p.mid_gain_db, p.mid_q),
      highShelf(p.high_freq, p.high_gain_db),
    ];
    marks = [p.low_freq, p.mid_freq, p.high_freq];
  } else {
    return null;
  }
  const pts: string[] = [];
  for (let i = 0; i <= 120; i++) {
    const f = 20 * (1000 ** (i / 120));
    const db = coeffs.reduce((acc, c) => acc + rbjMagnitudeDb(c, f), 0);
    pts.push(`${curveX(f).toFixed(1)},${curveY(db).toFixed(1)}`);
  }
  return { d: `M ${pts.join(" L ")}`, marks, band };
}
