<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { listen } from "@tauri-apps/api/event";
import {
  NAlert,
  NButton,
  NCard,
  NSelect,
  NSlider,
  NSwitch,
  NTag,
  NText,
  useMessage,
} from "naive-ui";
import {
  api,
  nodeKey,
  type DeviceInfo,
  type DspNode,
  type NodePos,
  type Processor,
  type Sink,
  type Source,
  type UsbIpCableStatus,
  type UsbIpStatus,
} from "../api";
import { useApp, genId } from "../store";
import MeterBar from "../components/MeterBar.vue";

const app = useApp();
const message = useMessage();

// ---------- 画布几何 ----------
const NODE_W = 236;
// 96 → 108：副标题放开到两行后，内容（标题 + 两行副标题 + 电平条 + 音量条）需要更高一些
const NODE_H = 108;
const canvasEl = ref<HTMLElement | null>(null);
const canvasSize = ref({ w: 900, h: 520 });
let resizeObserver: ResizeObserver | null = null;

function measure() {
  const el = canvasEl.value;
  if (!el) return;
  // 至少为 1：尺寸为 0 时（页签隐藏/尚未布局）算坐标会出现 0/0 = NaN
  const w = Math.max(1, el.clientWidth);
  const h = Math.max(1, el.clientHeight);
  const old = canvasSize.value;
  if (w === old.w && h === old.h) return;
  // 布局存的是归一化坐标，直接套新尺寸会让节点在像素上跟着画布缩放；
  // 这里按「像素位置不变」重新归一化（初始 900×520 是占位值、布局还没加载，不能算）
  if (layoutLoaded && old.w > 2 && old.h > 2) {
    const mapped: Record<string, NodePos> = {};
    for (const [key, p] of Object.entries(layout.value)) {
      if (!Array.isArray(p) || p.length < 2) continue;
      mapped[key] = [clamp01((p[0] * old.w) / w), clamp01((p[1] * old.h) / h)];
    }
    layout.value = mapped;
    schedulePersistLayout();
  }
  canvasSize.value = { w, h };
}

/** 画布是否已经量到可用尺寸（没量到就不做坐标换算） */
function canvasReady() {
  return canvasSize.value.w > 2 && canvasSize.value.h > 2;
}

// ---------- 整页高度自适应 ----------
// 画布高度 = 视口底边 − 画布顶边 − 底部留白 − 提示条高度（实测矩形，公式没有隐含假设，
// 提示条出现/消失、上方卡片高度变化都会被 ResizeObserver 捕捉后重新算，天然无反馈环）。
// 旧的「页面高度差值」启发式会卡死（画布一旦不是页面最高的元素就再也不跟随窗口），
// 连带把节点行距挤塌、看起来全叠在一起，所以整个换掉。
const pageEl = ref<HTMLElement | null>(null);
const noticeEl = ref<HTMLElement | null>(null);
const clampCanvas = (h: number) => Math.max(240, Math.min(Math.round(h), 1600));
/** 画布高度（px） */
const canvasH = ref(clampCanvas(window.innerHeight - 392));

function fitCanvas() {
  const canvas = canvasEl.value;
  const page = pageEl.value;
  if (!canvas || !canvas.isConnected || !page) return;
  // 滚动容器是页签面板（App.vue 给 pane 设了固定高 + overflow:auto），
  // 不是窗口 —— 参照窗口算会永远差出一条页签头的高度；向上找真正可滚动的那层
  let scroller: HTMLElement | null = page.parentElement;
  while (scroller && scroller !== document.body) {
    const oy = getComputedStyle(scroller).overflowY;
    if (oy === "auto" || oy === "scroll") break;
    scroller = scroller.parentElement;
  }
  if (!scroller || scroller === document.body) return;
  const sRect = scroller.getBoundingClientRect();
  const cRect = canvas.getBoundingClientRect();
  if (cRect.top < sRect.top) return; // 页签还没显示（画布不在可视区），别乱动
  // 画布在滚动容器内容里的偏移：加回 scrollTop，滚动时不改变测量结果（无反馈环）
  const offset = cRect.top - sRect.top + scroller.scrollTop;
  // 画布下方到页面底边的固定 chrome：沿途每层的 padding/border/margin-bottom 逐层实测。
  // 不能用「页面底边 − 画布底边」：左栏内容比画布高时会把网格行撑高，画布越压越矮（恶性循环）
  let below = 0;
  let el: HTMLElement | null = canvas;
  while (el && el !== page) {
    const cs = getComputedStyle(el);
    below +=
      (parseFloat(cs.paddingBottom) || 0) +
      (parseFloat(cs.borderBottomWidth) || 0) +
      (parseFloat(cs.marginBottom) || 0);
    el = el.parentElement;
  }
  below += parseFloat(getComputedStyle(page).paddingBottom) || 0;
  // 提示条占位 = 自身高度 + 外边距：只算 offsetHeight 会漏掉 margin-top 14px，
  // 页面正好高出这一截，页签面板右边出现滚动条
  let noticeH = 0;
  if (noticeEl.value) {
    const ncs = getComputedStyle(noticeEl.value);
    noticeH =
      noticeEl.value.offsetHeight + (parseFloat(ncs.marginTop) || 0) + (parseFloat(ncs.marginBottom) || 0);
  }
  const next = clampCanvas(scroller.clientHeight - offset - below - noticeH);
  if (next !== canvasH.value) canvasH.value = next;
}

const canvasStyle = computed(() => ({ height: `${canvasH.value}px` }));

// ---------- 节点模型 ----------
type NodeKind = "input" | "loopback" | "output" | "cable_play" | "cable_rec" | "dsp";

interface GNode {
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
  cableNumber?: number;
  hasIn: boolean;
  hasOut: boolean;
  /** 端子贴在盒子的哪一侧（输入设备在右、输出设备在左、虚拟线路左进右出） */
  inSide: "left" | "right";
  outSide: "left" | "right";
  x: number;
  y: number;
}

const layout = ref<Record<string, NodePos>>({});
/** 布局是否已从后端加载完（加载完之前画布尺寸变化不做像素换算） */
let layoutLoaded = false;
const cables = ref<UsbIpCableStatus[]>([]);
const usbipRunning = ref(false);

// ---------- 虚拟线路（usbip://N/playback|capture 设备本体即线路节点） ----------
/** 是否线路的播放端（系统播放 → 混音器采集） */
const cableIsPlay = (id: string) => /^usbip:\/\/\d+\/playback$/.test(id);
/** 是否线路的录音端（混音器写入 → 系统录音） */
const cableIsRec = (id: string) => /^usbip:\/\/\d+\/capture$/.test(id);
/** 从设备 id 解析线路号，非线路设备返回 null */
function cableNumberOf(deviceId: string): number | null {
  const m = deviceId.match(/^usbip:\/\/(\d+)\//);
  return m ? Number(m[1]) : null;
}
/** 线路运行态（usbip-status 未拉到 / 配置已删除时为 null） */
function cableStatusOf(number: number | null): UsbIpCableStatus | null {
  return number === null ? null : cables.value.find((c) => c.number === number) ?? null;
}
/** 节点标题用的线路名：优先配置显示名，回退设备名（去掉「 (输入)/(输出)」后缀） */
function lineName(deviceId: string): string {
  const c = cableStatusOf(cableNumberOf(deviceId));
  if (c?.display_name) return c.display_name;
  return app.deviceName(deviceId).replace(/\s*[(（](输入|输出)[)）]\s*$/, "");
}
/** 线路节点副标题里的接入状态 */
function lineStateOf(c: UsbIpCableStatus | null): string {
  if (!usbipRunning.value) return "服务器未运行";
  return c?.attached ? "已接入系统" : "未接入（需附加）";
}
/** 线路节点副标题里的内部拷贝方向 */
function lineCopyOf(c: UsbIpCableStatus | null): string {
  if (!c) return "—";
  return c.mode === "loopback" ? "输出→输入" : c.mode === "reverse" ? "输入→输出" : "不拷贝";
}
/**
 * 该设备是否是某条线路的 **Windows 真实端点**（接入后 usbaudio.sys 创建的扬声器/麦克风）。
 * 这些端点和 usbip:// 合成端点指向同一条线路，设备面板里由线路节点统一代表，不再单独出现。
 * usbip:// 合成端点名字相同但 id 不同，不算真实端点。
 */
function realCableDevice(d: DeviceInfo): UsbIpCableStatus | null {
  if (d.id.startsWith("usbip://")) return null;
  return cables.value.find((c) => c.display_name && d.name.includes(c.display_name)) ?? null;
}

/** 底部唯一的提示（同时只显示一条，高度用于给画布让位） */
const notice = computed(() => {
  if (!cables.value.length) {
    return {
      type: "info" as const,
      text: "还没有虚拟线路。先到「虚拟声卡」页添加线路并保存，再把「线路输出 / 线路输入」拖进画布接线。",
    };
  }
  if (!usbipRunning.value) {
    return {
      type: "warning" as const,
      text: "虚拟声卡服务器当前未运行 —— 画布上的线路节点不会有音频进出。到「虚拟声卡」页打开「启用虚拟声卡服务器」。",
    };
  }
  return null;
});

const KIND_META: Record<NodeKind, { tag: string; type: "default" | "info" | "success" | "warning" }> = {
  input: { tag: "输入设备", type: "info" },
  loopback: { tag: "系统回声", type: "default" },
  output: { tag: "输出设备", type: "success" },
  cable_play: { tag: "线路输出", type: "warning" },
  cable_rec: { tag: "线路输入", type: "warning" },
  dsp: { tag: "DSP", type: "warning" },
};

const DEFAULT_COL: Record<NodeKind, number> = {
  input: 0.03,
  loopback: 0.03,
  cable_play: 0.36,
  cable_rec: 0.36,
  dsp: 0.36,
  output: 0.68,
};

function defaultPos(col: number, index: number): NodePos {
  const rowsPerCol = Math.max(1, Math.floor((canvasSize.value.h - 30) / (NODE_H + 24)));
  const row = index % rowsPerCol;
  const extraCol = Math.floor(index / rowsPerCol) * 0.1;
  return [
    Math.min(0.76, col + extraCol),
    0.05 + (row * (NODE_H + 24)) / canvasSize.value.h,
  ];
}

const nodes = computed<GNode[]>(() => {
  const list: GNode[] = [];
  // 默认排布计数按「列」共享：同列的 input/loopback、cable_play/cable_rec/dsp
  // 各自独立计数会在同一坐标精确重叠，按列排队才能竖着错开
  const colCounters: Record<number, number> = {};
  const pos = (key: string, kind: NodeKind): NodePos => {
    const stored = layout.value[key];
    // 坏数据（null/短数组/NaN）当作没有，回落到默认排布
    if (Array.isArray(stored) && stored.length >= 2 && stored.every((v) => Number.isFinite(v))) {
      return [clamp01(stored[0]), clamp01(stored[1])];
    }
    const col = DEFAULT_COL[kind];
    const idx = colCounters[col] ?? 0;
    colCounters[col] = idx + 1;
    return defaultPos(col, idx);
  };

  // 输入源：物理设备 / 系统回声 / 虚拟线路的播放端（usbip://N/playback 本体即线路输出节点）
  for (const s of app.graph.sources) {
    const isCable = cableIsPlay(s.device_id);
    const cableNum = isCable ? cableNumberOf(s.device_id) : null;
    const kind: NodeKind = isCable ? "cable_play" : s.mode === "loopback" ? "loopback" : "input";
    const key =
      kind === "cable_play"
        ? nodeKey.cablePlay(cableNum!)
        : kind === "loopback"
          ? nodeKey.loopback(s.device_id)
          : nodeKey.input(s.device_id);
    const [x, y] = pos(key, kind);
    const cs = cableStatusOf(cableNum);
    list.push({
      key,
      kind,
      title: isCable ? `${lineName(s.device_id)} · 线路输出` : app.deviceName(s.device_id),
      subtitle: isCable
        ? `系统播放端（扬声器） · 拷贝：${lineCopyOf(cs)} · ${lineStateOf(cs)}`
        : s.enabled
          ? kind === "loopback"
            ? "该系统输出正在播放的声音"
            : "录入设备"
          : "已停用",
      sourceId: s.id,
      cableNumber: cableNum ?? undefined,
      hasIn: false,
      hasOut: true,
      // 输入设备 / 系统回声 / 线路播放端：端子贴在盒子**右**边（信号从它流向别处）
      inSide: "right",
      outSide: "right",
      x,
      y,
    });
  }
  // 输出汇：物理输出设备 / 虚拟线路的录音端（usbip://N/capture 本体即线路输入节点）
  for (const k of app.graph.sinks) {
    const isCable = cableIsRec(k.device_id);
    const cableNum = isCable ? cableNumberOf(k.device_id) : null;
    const kind: NodeKind = isCable ? "cable_rec" : "output";
    const key = isCable ? nodeKey.cableRec(cableNum!) : nodeKey.output(k.device_id);
    const [x, y] = pos(key, kind);
    const cs = cableStatusOf(cableNum);
    list.push({
      key,
      kind,
      title: isCable ? `${lineName(k.device_id)} · 线路输入` : app.deviceName(k.device_id),
      subtitle: isCable
        ? `系统录音端（麦克风） · 拷贝：${lineCopyOf(cs)} · ${lineStateOf(cs)}`
        : "输出设备",
      sinkId: k.id,
      cableNumber: cableNum ?? undefined,
      hasIn: true,
      hasOut: false,
      // 输出设备 / 线路录音端：端子贴在盒子**左**边（信号从别处流入它）
      inSide: "left",
      outSide: "left",
      x,
      y,
    });
  }
  // DSP 处理方块：左入右出，标题=类型名，副标题=参数摘要
  for (const p of app.graph.processors) {
    const key = `dsp:${p.id}`;
    const [x, y] = pos(key, "dsp");
    list.push({
      key,
      kind: "dsp",
      title: DSP_META[p.type].label,
      subtitle: dspSubtitle(p),
      processorId: p.id,
      hasIn: true,
      hasOut: true,
      inSide: "left",
      outSide: "right",
      x,
      y,
    });
  }
  return list;
});

/** DSP 方块副标题：关键参数摘要（随参数变化实时刷新） */
function dspSubtitle(p: DspNode): string {
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
  }
}

const nodeByKey = computed(() => new Map(nodes.value.map((n) => [n.key, n])));
const nodeBySource = computed(() => new Map(nodes.value.filter((n) => n.sourceId).map((n) => [n.sourceId!, n])));
const nodeBySink = computed(() => new Map(nodes.value.filter((n) => n.sinkId).map((n) => [n.sinkId!, n])));
const nodeByProcessor = computed(() => new Map(nodes.value.filter((n) => n.processorId).map((n) => [n.processorId!, n])));

/** 图实体 id（source/sink/processor）→ 画布节点 key */
function graphIdToKey(id: string): string | null {
  return nodeBySource.value.get(id)?.key ?? nodeBySink.value.get(id)?.key ?? nodeByProcessor.value.get(id)?.key ?? null;
}

/** source/sink/processor 三种 id 都能查到画布节点 */
function nodeByGraphId(id: string): GNode | null {
  return nodeBySource.value.get(id) ?? nodeBySink.value.get(id) ?? nodeByProcessor.value.get(id) ?? null;
}

/** 端子所在的边（按设备角色：输入设备在左、输出设备在右） */
function termSide(node: GNode, which: "in" | "out"): "left" | "right" {
  return which === "in" ? node.inSide : node.outSide;
}

/** 端子坐标 + 出线方向（dir：-1 向左出线，+1 向右出线） */
function termPos(node: GNode, which: "in" | "out") {
  const { w, h } = canvasSize.value;
  const side = termSide(node, which);
  return {
    x: side === "left" ? node.x * w : node.x * w + NODE_W,
    y: node.y * h + NODE_H / 2,
    dir: side === "left" ? -1 : 1,
  };
}

function termStyle(node: GNode, which: "in" | "out") {
  const side = termSide(node, which);
  return side === "left"
    ? { left: "-8px", right: "auto", top: "50%", transform: "translateY(-50%)" }
    : { right: "-8px", left: "auto", top: "50%", transform: "translateY(-50%)" };
}

/** 端子提示（线路节点按「线路输入＝Windows 录制端、线路输出＝Windows 播放端」称呼） */
function termTitle(node: GNode, which: "in" | "out") {
  if (node.cableNumber !== undefined) {
    const line =
      node.kind === "cable_rec"
        ? "线路输入 · 系统录音端（麦克风）：混音器写这里，别的软件从这录"
        : "线路输出 · 系统播放端（扬声器）：别的软件播进这里，混音器从这读";
    return `${line}（${which === "in" ? "信号流入" : "信号流出"}端）`;
  }
  return which === "in" ? "输入端子（信号流入）" : "输出端子（信号流出）";
}
const selectedRoute = ref<string | null>(null);
const selectedNode = ref<string | null>(null);

interface Wire {
  id: string;
  from: { x: number; y: number; dir: number };
  to: { x: number; y: number; dir: number };
  gain: number;
  muted: boolean;
  fromTitle: string;
  toTitle: string;
}

const wires = computed<Wire[]>(() => {
  const list: Wire[] = [];
  for (const r of app.graph.routes) {
    const a = nodeByGraphId(r.source_id);
    const b = nodeByGraphId(r.sink_id);
    if (!a || !b) continue;
    list.push({
      id: r.id,
      from: termPos(a, "out"),
      to: termPos(b, "in"),
      gain: r.gain,
      muted: r.muted,
      fromTitle: a.title,
      toTitle: b.title,
    });
  }
  return list;
});

/**
 * 连线：控制点朝**目标所在的一侧**外扩 —— 端子虽然固定在盒子某一边，
 * 但目标在左时曲线就从节点左侧出线（起点藏到节点底下），不再绕到节点背后兜圈；
 * 终点方向保持端子的出线方向（箭头始终从外面戳向端子圆）。
 */
/** 端子环半径：连线两端收在环外缘，线不进环、箭头贴着环不被环线穿过 */
const TERM_R = 8;
function wirePath(from: { x: number; y: number; dir: number }, to: { x: number; y: number; dir?: number }) {
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
function previewWirePath(from: { x: number; y: number; dir: number }, to: { x: number; y: number }) {
  const d = Math.min(90, Math.max(36, Math.abs(to.x - from.x) * 0.4));
  const fd = from.dir;
  const ed = Math.abs(to.x - from.x) < 1 ? from.dir : Math.sign(to.x - from.x);
  const c1 = from.x + fd * d;
  const c2 = to.x - ed * d;
  // 起点从端子环外缘出发（终点是鼠标位置，直接到光标）
  const sx = from.x + fd * TERM_R;
  return `M ${sx} ${from.y} C ${c1} ${from.y}, ${c2} ${to.y}, ${to.x} ${to.y}`;
}

// ---------- 指针拖拽（不依赖 HTML5 DnD：Tauri 在 Windows 上会拦截它） ----------
type DragState =
  | { kind: "node"; key: string; dx: number; dy: number; moved: boolean }
  | { kind: "wire"; key: string; side: "in" | "out"; x: number; y: number; sx: number; sy: number; moved: boolean }
  | { kind: "palette"; item: PaletteItem; sx: number; sy: number; cx: number; cy: number; moved: boolean }
  | { kind: "pan"; sx: number; sy: number; ox: number; oy: number; moved: boolean };

const drag = ref<DragState | null>(null);

/** 画布平移量（px，世界坐标 → 屏幕坐标的偏移）；拖空白处自由拖动 */
const pan = ref({ x: 0, y: 0 });

/** 平移范围：世界至少留 80px 在可视区里，别拖丢 */
function clampPan(v: number, dim: number) {
  const lim = Math.max(0, dim - 80);
  return Math.max(-lim, Math.min(lim, v));
}

function startCanvasPan(e: PointerEvent) {
  if (e.button !== 0) return;
  drag.value = { kind: "pan", sx: e.clientX, sy: e.clientY, ox: pan.value.x, oy: pan.value.y, moved: false };
  attachWindowDrag();
}

/** 把视图居中到所有节点的包围盒 */
function centerView() {
  const { w, h } = canvasSize.value;
  if (!nodes.value.length) {
    pan.value = { x: 0, y: 0 };
    return;
  }
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const n of nodes.value) {
    minX = Math.min(minX, n.x * w);
    minY = Math.min(minY, n.y * h);
    maxX = Math.max(maxX, n.x * w + NODE_W);
    maxY = Math.max(maxY, n.y * h + NODE_H);
  }
  pan.value = {
    x: clampPan(w / 2 - (minX + maxX) / 2, w),
    y: clampPan(h / 2 - (minY + maxY) / 2, h),
  };
}

/**
 * 自动排序：按信号流向分层（源 = 第 0 列，每经过一个 DSP 方块进一列，汇在最后一列），
 * 列内按现有上下顺序纵向均布；列放不下时压缩行距。连线成环在 connect() 已拦截，最长路径必收敛。
 */
function autoArrange() {
  if (!nodes.value.length) return;
  const layer = new Map<string, number>();
  for (const n of nodes.value) layer.set(n.key, 0);
  const edges: [string, string][] = [];
  for (const r of app.graph.routes) {
    const a = graphIdToKey(r.source_id);
    const b = graphIdToKey(r.sink_id);
    if (a && b && a !== b && layer.has(a) && layer.has(b)) edges.push([a, b]);
  }
  for (let i = 0; i < nodes.value.length; i++) {
    let changed = false;
    for (const [a, b] of edges) {
      const la = layer.get(a)!;
      if (la + 1 > layer.get(b)!) {
        layer.set(b, la + 1);
        changed = true;
      }
    }
    if (!changed) break;
  }
  const { w, h } = canvasSize.value;
  const cols = new Map<number, string[]>();
  for (const n of nodes.value) {
    const l = layer.get(n.key) ?? 0;
    if (!cols.has(l)) cols.set(l, []);
    cols.get(l)!.push(n.key);
  }
  const next = { ...layout.value };
  // 列距按最深层数均分整幅画布宽：固定列距在窄画布上会被 clamp 到同一条右边线，
  // 第 3 层以后的节点全部叠在一列，看起来就像「没按信号关系排」
  const maxLayer = Math.max(0, ...cols.keys());
  const colGap = maxLayer > 0 ? (w - 32 - NODE_W) / maxLayer : 0;
  for (const [l, keys] of cols) {
    keys.sort((a, b) => (layout.value[a]?.[1] ?? 0) - (layout.value[b]?.[1] ?? 0));
    const gapY = NODE_H + 36;
    // 放不下就压缩行距，保证最后一行也完整落在画布内
    const scale =
      16 + keys.length * NODE_H + (keys.length - 1) * gapY > h - 16
        ? (h - 32 - NODE_H) / Math.max(1, keys.length - 1)
        : gapY;
    keys.forEach((k, i) => {
      const px = Math.min(16 + l * colGap, w - NODE_W - 8);
      next[k] = [clamp01(px / w), clamp01((16 + i * scale) / h)];
    });
  }
  layout.value = next;
  void persistLayout();
  message.success("已按信号流向排序");
}

/** 「点一下端子 → 再点目标」的连线方式（不依赖拖拽，鼠标移动时同样有虚线跟随） */
const clickPending = ref<{ key: string; side: "in" | "out" } | null>(null);
const mousePos = ref<{ x: number; y: number } | null>(null);

function onGlobalMove(e: PointerEvent) {
  mousePos.value = canvasPoint(e);
}

function setClickPending(t: { key: string; side: "in" | "out" } | null) {
  clickPending.value = t;
  if (t) {
    window.addEventListener("pointermove", onGlobalMove);
  } else {
    window.removeEventListener("pointermove", onGlobalMove);
    mousePos.value = null;
  }
}

/** 当前正在拉线的端子（拖拽中或已点击选中） */
const wireSource = computed<{ key: string; side: "in" | "out" } | null>(() => {
  if (drag.value?.kind === "wire") return { key: drag.value.key, side: drag.value.side };
  return clickPending.value;
});

function canvasPoint(e: { clientX: number; clientY: number }) {
  const rect = canvasEl.value?.getBoundingClientRect();
  if (!rect) return { x: 0, y: 0 };
  // 画布内一切坐标都是**世界坐标**（节点/连线所在坐标系），要扣掉平移量
  return { x: e.clientX - rect.left - pan.value.x, y: e.clientY - rect.top - pan.value.y };
}

function clamp01(v: number) {
  // NaN/Infinity 一律挡掉：JSON.stringify(NaN) 会变成 null，后端的 f64 反序列化会直接报错
  if (!Number.isFinite(v)) return 0.5;
  return Math.max(0, Math.min(0.92, v));
}

/** 只保留「两个有限数字」的布局项；坏数据（null/短数组/NaN）直接丢掉 */
function sanitizeLayout(src: Record<string, unknown> | null | undefined): Record<string, NodePos> {
  const out: Record<string, NodePos> = {};
  for (const [key, value] of Object.entries(src ?? {})) {
    if (!Array.isArray(value) || value.length < 2) continue;
    const x = Number(value[0]);
    const y = Number(value[1]);
    if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
    out[key] = [clamp01(x), clamp01(y)];
  }
  return out;
}

function attachWindowDrag() {
  window.addEventListener("pointermove", onWindowMove);
  window.addEventListener("pointerup", onWindowUp, { once: true });
}

function detachWindowDrag() {
  window.removeEventListener("pointermove", onWindowMove);
  window.removeEventListener("pointerup", onWindowUp);
}

function onWindowMove(e: PointerEvent) {
  const d = drag.value;
  if (!d) return;
  if (d.kind === "node") {
    if (!canvasReady()) return; // 尺寸没量到就别更新坐标，避免 NaN
    const p = canvasPoint(e);
    layout.value = {
      ...layout.value,
      [d.key]: [clamp01((p.x - d.dx) / canvasSize.value.w), clamp01((p.y - d.dy) / canvasSize.value.h)],
    };
    drag.value = { ...d, moved: true };
  } else if (d.kind === "wire") {
    const p = canvasPoint(e);
    const moved = d.moved || Math.hypot(e.clientX - d.sx, e.clientY - d.sy) > 4;
    drag.value = { ...d, x: p.x, y: p.y, moved };
  } else if (d.kind === "pan") {
    const moved = d.moved || Math.hypot(e.clientX - d.sx, e.clientY - d.sy) > 4;
    const { w, h } = canvasSize.value;
    pan.value = {
      x: clampPan(d.ox + (e.clientX - d.sx), w),
      y: clampPan(d.oy + (e.clientY - d.sy), h),
    };
    drag.value = { ...d, moved };
  } else {
    const moved = d.moved || Math.hypot(e.clientX - d.sx, e.clientY - d.sy) > 4;
    drag.value = { ...d, cx: e.clientX, cy: e.clientY, moved };
  }
}

async function onWindowUp(e: PointerEvent) {
  const d = drag.value;
  detachWindowDrag();
  drag.value = null;
  if (!d) return;
  if (d.kind === "pan") {
    // 原地点击（没拖动）= 点空白：清掉选中
    if (!d.moved) {
      selectedRoute.value = null;
      selectedNode.value = null;
    }
    return;
  }
  if (d.kind === "node") {
    // 松手后浏览器还会补发一次 click（落在同一个节点上）——标记这次拖动，
    // 让 onNodeClick 把它吞掉，否则拖完节点总会弹出设置浮窗
    nodeDragMoved = d.moved;
    await persistLayout();
    return;
  }
  if (d.kind === "wire") {
    // 先精确命中端子；不然放宽为「落在某个节点上」→ 自动用它对侧的那个端子
    const term = terminalAt(e.clientX, e.clientY) ?? inferredTerminal(e.clientX, e.clientY, d.side, d.key);
    const same = term && term.key === d.key && term.side === d.side;

    // 没移动 = 单击端子：进入「再点一个端子就连线」模式（并让虚线跟着鼠标）
    if (!d.moved) {
      if (clickPending.value) {
        const pending = clickPending.value;
        setClickPending(null);
        if (!(pending.key === d.key && pending.side === d.side)) {
          await connect(pending.key, pending.side, d.key, d.side);
        }
        return;
      }
      if (term && !same) {
        await connect(d.key, d.side, term.key, term.side);
        return;
      }
      setClickPending({ key: d.key, side: d.side });
      message.info("已选中该端子：再点另一个端子即可连线（Esc 取消）");
      return;
    }

    setClickPending(null);
    if (term) await connect(d.key, d.side, term.key, term.side);
    else message.info("连线：请松手在目标节点上（或它的小圆点上）");
    return;
  }
  // 设备面板：落在画布内就放到落点，否则放到默认位置（点击即添加）
  const rect = canvasEl.value?.getBoundingClientRect();
  const inside =
    !!rect &&
    e.clientX >= rect.left &&
    e.clientX <= rect.right &&
    e.clientY >= rect.top &&
    e.clientY <= rect.bottom;
  await addFromPalette(d.item, inside && rect ? { x: e.clientX - rect.left, y: e.clientY - rect.top } : undefined);
}

function terminalAt(clientX: number, clientY: number): { key: string; side: "in" | "out" } | null {
  const el = document.elementFromPoint(clientX, clientY) as HTMLElement | null;
  const attr = el?.closest("[data-term]")?.getAttribute("data-term");
  if (!attr) return null;
  const [key, side] = attr.split("|");
  return { key, side: side as "in" | "out" };
}

/** 拖动连线时：判断某个端子能不能接（严格单向，输出→输入） */

/** 该线路是否已开启内部拷贝（输入→输出 或 输出→输入） */
function copyEnabledOf(number?: number): boolean {
  const c = cables.value.find((x) => x.number === number);
  return !!c && c.mode !== "mixer";
}

/**
 * 禁止的连接：
 * 1) 系统回声 → 输出设备：抓到的就是该输出设备正在播放的声音，送回输出端立刻回授啸叫；
 * 2) 同一条线路的「输入 → 输出」且该线路已开启拷贝：拷贝 + 再接一圈 = 自激。
 */
function pairForbidden(a: GNode, aSide: "in" | "out", b: GNode, bSide: "in" | "out"): boolean {
  const out = aSide === "out" ? a : b;
  const sink = aSide === "out" ? b : a;
  void bSide;
  if (out.kind === "loopback" && sink.kind === "output") return true;
  if (out.kind === "cable_play" && sink.kind === "cable_rec" && out.cableNumber === sink.cableNumber) {
    return copyEnabledOf(out.cableNumber);
  }
  return false;
}

function termState(node: GNode, which: "in" | "out") {
  const src = wireSource.value;
  if (!src) return "";
  // 自己那个正在拉线的端子不参与高亮
  if (src.key === node.key && src.side === which) return "";
  // 只能 输出 → 输入：同类型端子一律不可接
  let ok =
    src.side !== which &&
    (which === "out" ? !!(node.sourceId || node.processorId) : !!(node.sinkId || node.processorId));
  const srcNode = nodeByKey.value.get(src.key);
  if (ok && srcNode) ok = !pairForbidden(srcNode, src.side, node, which);
  return ok ? "compat" : "incompat";
}

/** 松手落在节点（而不是那个小圆点）上时，自动选对侧端子 —— 大幅放宽连线的手感 */
function inferredTerminal(
  clientX: number,
  clientY: number,
  fromSide: "in" | "out",
  dragKey: string,
): { key: string; side: "in" | "out" } | null {
  const el = document.elementFromPoint(clientX, clientY) as HTMLElement | null;
  const key = el?.closest("[data-node-key]")?.getAttribute("data-node-key");
  if (!key) return null;
  const node = nodeByKey.value.get(key);
  if (!node || key === dragKey) return null;
  const want: "in" | "out" = fromSide === "out" ? "in" : "out";
  const src = nodeByKey.value.get(dragKey);
  if (want === "in" && !node.hasIn) {
    message.warning(`「${node.title}」没有可接的输入端（输出不能接到输出）`);
    return null;
  }
  if (want === "out" && !node.hasOut) {
    message.warning(`「${node.title}」没有可接的输出端（输入不能接到输入）`);
    return null;
  }
  if (src && pairForbidden(src, fromSide, node, want)) {
    message.error(
      src.kind === "cable_play"
        ? "同一条线路的输入不能接回自己的输出（该线路已开启拷贝，会自激）"
        : "系统回声不能接到输出设备 —— 会形成回授啸叫",
    );
    return null;
  }
  return { key, side: want };
}

function startNodeDrag(e: PointerEvent, node: GNode) {
  if (e.button !== 0) return;
  e.preventDefault();
  nodeDragMoved = false; // 上次拖动若松手在节点外，click 不会补发，残留标记要在下次按下时清掉
  const p = canvasPoint(e);
  drag.value = {
    kind: "node",
    key: node.key,
    dx: p.x - node.x * canvasSize.value.w,
    dy: p.y - node.y * canvasSize.value.h,
    moved: false,
  };
  attachWindowDrag();
}

/** 上一次节点拖动是否真的挪动了位置：pointerup 之后紧跟着的 click 要据此吞掉 */
let nodeDragMoved = false;

/** 点击节点 = 选中并弹出设置浮窗；但拖拽松手补发的 click 不算点击 */
function onNodeClick(node: GNode) {
  if (nodeDragMoved) {
    nodeDragMoved = false;
    return;
  }
  selectedNode.value = node.key;
  selectedRoute.value = null;
}

function startWireDrag(e: PointerEvent, node: GNode, side: "in" | "out") {
  if (e.button !== 0) return;
  e.preventDefault();
  const start = termPos(node, side);
  drag.value = {
    kind: "wire",
    key: node.key,
    side,
    x: start.x,
    y: start.y,
    sx: e.clientX,
    sy: e.clientY,
    moved: false,
  };
  attachWindowDrag();
}

function startPaletteDrag(e: PointerEvent, item: PaletteItem) {
  if (e.button !== 0) return;
  e.preventDefault();
  drag.value = { kind: "palette", item, sx: e.clientX, sy: e.clientY, cx: e.clientX, cy: e.clientY, moved: false };
  attachWindowDrag();
}

const pendingWire = computed(() => {
  const d = drag.value;
  if (d && d.kind === "wire") {
    const n = nodeByKey.value.get(d.key);
    if (n) return { from: termPos(n, d.side), to: { x: d.x, y: d.y } };
  }
  // 点击选中端子后：虚线继续跟着鼠标走
  const p = clickPending.value;
  if (p && mousePos.value) {
    const n = nodeByKey.value.get(p.key);
    if (n) return { from: termPos(n, p.side), to: mousePos.value };
  }
  return null;
});

/** 从设备面板拖动时：跟随鼠标的虚线落位框（画布内才显示） */
const dropPreview = computed(() => {
  const d = drag.value;
  if (!d || d.kind !== "palette") return null;
  const p = canvasPoint({ clientX: d.cx, clientY: d.cy });
  const { w, h } = canvasSize.value;
  if (p.x < 0 || p.y < 0 || p.x > w || p.y > h) return null;
  return {
    x: Math.max(0, Math.min(w - NODE_W, p.x - NODE_W / 2)),
    y: Math.max(0, Math.min(h - NODE_H, p.y - NODE_H / 2)),
    title: d.item.title,
  };
});

/** 拖动节点时：穿过节点中心的虚线对齐辅助线 */
const nodeGuides = computed(() => {
  const d = drag.value;
  if (!d || d.kind !== "node") return null;
  const n = nodeByKey.value.get(d.key);
  if (!n) return null;
  return { x: n.x * canvasSize.value.w + NODE_W / 2, y: n.y * canvasSize.value.h + NODE_H / 2 };
});

const ghost = computed(() => (drag.value?.kind === "palette" ? drag.value : null));

async function persistLayout() {
  try {
    // 只发有限数值，坏项丢掉（后端 layout 是 f64，收到 null 会报 invalid args）。
    // 同时丢掉已删除节点的遗留坐标 —— 幽灵坐标会参与反重叠疏散、占掉能用格子，
    // 还永远留在配置文件里（画布小时直接把疏散挤成死循环）
    await api.setMixerLayout(pruneLayout(sanitizeLayout(layout.value)));
  } catch (e) {
    message.error(String(e));
  }
}

/** 丢掉画布上已不存在的节点坐标；图形还没加载时不剪（会把全部坐标误删） */
function pruneLayout(pos: Record<string, NodePos>): Record<string, NodePos> {
  if (!nodes.value.length) return pos;
  const valid = new Set(nodes.value.map((n) => n.key));
  const out: Record<string, NodePos> = {};
  for (const [k, v] of Object.entries(pos)) {
    if (valid.has(k)) out[k] = v;
  }
  return out;
}

let persistTimer: number | null = null;
/** 画布缩放期间布局连续变化，停稳 500ms 后再写盘 */
function schedulePersistLayout() {
  if (persistTimer !== null) clearTimeout(persistTimer);
  persistTimer = window.setTimeout(() => {
    persistTimer = null;
    void persistLayout();
  }, 500);
}

// ---------- 连线 ----------
async function connect(fromKey: string, fromSide: "in" | "out", toKey: string, toSide: "in" | "out") {
  // 严格单向：只能「输出 → 输入」
  if (fromSide === toSide) {
    message.warning(
      fromSide === "out" ? "输出不能接到输出 —— 要从输出端子拖到输入端子" : "输入不能接到输入 —— 要从输出端子拖到输入端子",
    );
    return;
  }
  const outKey = fromSide === "out" ? fromKey : toKey;
  const inKey = fromSide === "in" ? fromKey : toKey;
  const outNode = nodeByKey.value.get(outKey);
  const inNode = nodeByKey.value.get(inKey);
  // 出端必须是 source 或 DSP 方块；入端必须是 sink 或 DSP 方块
  const sourceId = outNode?.sourceId ?? outNode?.processorId;
  const sinkId = inNode?.sinkId ?? inNode?.processorId;
  if (!outNode || !inNode || !sourceId || !sinkId) {
    message.warning("该端子不可连接");
    return;
  }
  // 回授 / 自激路径直接拦掉
  if (pairForbidden(outNode, "out", inNode, "in")) {
    message.error(
      outNode.kind === "cable_play"
        ? "同一条线路的输入接回自己的输出会自激（该线路已开启拷贝）—— 要设备端回环请把该线路的拷贝设为「不拷贝」"
        : "系统回声不能接到输出设备 —— 它抓到的就是输出设备正在播放的声音，送回去会立刻啸叫",
    );
    return;
  }
  // 成环直接拦掉：从入端沿信号方向走，能回到出端就是环
  if (reachesFrom(inKey, outKey)) {
    message.error("这条线会构成环路 —— DSP 处理链不允许循环，信号会无限累加");
    return;
  }
  if (app.graph.routes.some((r) => r.source_id === sourceId && r.sink_id === sinkId)) {
    message.info("这条线已经接好了");
    return;
  }
  app.graph.routes.push({
    id: genId("route"),
    source_id: sourceId,
    sink_id: sinkId,
    gain: 1.0,
    muted: false,
    nodes: [],
  });
  await save();
}

/** 从 startKey 沿信号流向（出→入）能否到达 targetKey（防环） */
function reachesFrom(startKey: string, targetKey: string): boolean {
  const visited = new Set<string>();
  const stack = [startKey];
  while (stack.length) {
    const k = stack.pop()!;
    if (k === targetKey) return true;
    if (visited.has(k)) continue;
    visited.add(k);
    for (const r of app.graph.routes) {
      const from = graphIdToKey(r.source_id);
      if (from === k) {
        const to = graphIdToKey(r.sink_id);
        if (to) stack.push(to);
      }
    }
  }
  return false;
}

async function disconnect(routeId: string) {
  app.graph.routes = app.graph.routes.filter((r) => r.id !== routeId);
  if (selectedRoute.value === routeId) selectedRoute.value = null;
  await save();
}

// ---------- 设备面板 ----------
type PaletteKind = "input" | "loopback" | "output" | "cable_play" | "cable_rec" | "dsp";
interface PaletteItem {
  kind: PaletteKind;
  title: string;
  deviceId?: string;
  /** kind === "dsp" 时的节点类型 */
  dspType?: DspType;
}

const palette = computed<PaletteItem[]>(() => {
  const items: PaletteItem[] = [];
  // 所有声卡设备都能拖进来：物理设备、第三方虚拟声卡，
  // 以及本应用的 usbip 线路端点（设备本体即「线路输出 / 线路输入」节点）。
  // 线路接入后 Windows 创建的真实扬声器/麦克风端点由线路节点代表，跳过。
  for (const d of app.devices) {
    if (realCableDevice(d)) continue;
    if (d.kind === "input") {
      if (cableIsPlay(d.id)) {
        items.push({ kind: "cable_play", title: `${lineName(d.id)} · 线路输出`, deviceId: d.id });
      } else {
        items.push({ kind: "input", title: d.name, deviceId: d.id });
      }
    } else if (cableIsRec(d.id)) {
      items.push({ kind: "cable_rec", title: `${lineName(d.id)} · 线路输入`, deviceId: d.id });
    } else {
      items.push({ kind: "output", title: d.name, deviceId: d.id });
      items.push({ kind: "loopback", title: `${d.name}（系统回声）`, deviceId: d.id });
    }
  }
  // DSP 处理方块：静态清单，不限数量，可重复拖入
  for (const t of DSP_ADD_OPTIONS) {
    items.push({ kind: "dsp", title: t.label, dspType: t.value });
  }
  return items;
});

/** 设备面板分两组展示：真实/虚拟设备一组，DSP 处理方块单独一组 */
const paletteDevices = computed(() => palette.value.filter((i) => i.kind !== "dsp"));
const paletteDsp = computed(() => palette.value.filter((i) => i.kind === "dsp"));

function paletteKey(item: PaletteItem): string | null {
  if (!item.deviceId) return null;
  if (item.kind === "cable_play") return nodeKey.cablePlay(cableNumberOf(item.deviceId)!);
  if (item.kind === "cable_rec") return nodeKey.cableRec(cableNumberOf(item.deviceId)!);
  if (item.kind === "input") return nodeKey.input(item.deviceId);
  if (item.kind === "loopback") return nodeKey.loopback(item.deviceId);
  return nodeKey.output(item.deviceId);
}

function paletteExisting(item: PaletteItem): boolean {
  // DSP 方块不限数量，永远不标灰
  if (item.kind === "dsp") return false;
  if (!item.deviceId) return false;
  if (item.kind === "cable_play") {
    return !!app.graph.sources.find((s) => s.device_id === item.deviceId);
  }
  if (item.kind === "cable_rec") {
    return !!app.graph.sinks.find((s) => s.device_id === item.deviceId);
  }
  if (item.kind === "input") {
    return !!app.graph.sources.find((s) => s.device_id === item.deviceId && s.mode === "deviceinput");
  }
  if (item.kind === "loopback") {
    return !!app.graph.sources.find((s) => s.device_id === item.deviceId && s.mode === "loopback");
  }
  return !!app.graph.sinks.find((s) => s.device_id === item.deviceId);
}

function makeSource(deviceId: string, name: string, mode: Source["mode"]): Source {
  return { id: genId("src"), name, device_id: deviceId, mode, enabled: true };
}

function makeSink(deviceId: string, name: string): Sink {
  return { id: genId("sink"), name, device_id: deviceId, volume: 1.0, enabled: true };
}

async function addFromPalette(item: PaletteItem, dropPoint?: { x: number; y: number }) {
  // DSP 方块：不限数量，拖一次加一个
  if (item.kind === "dsp") {
    if (!item.dspType) return;
    const proc: Processor = { id: genId("dsp"), ...makeDspNode(item.dspType) };
    const key = `dsp:${proc.id}`;
    app.graph.processors.push(proc);
    if (dropPoint) {
      layout.value = {
        ...layout.value,
        [key]: [
          clamp01((dropPoint.x - NODE_W / 2) / canvasSize.value.w),
          clamp01((dropPoint.y - NODE_H / 2) / canvasSize.value.h),
        ],
      };
      await persistLayout();
    }
    await save();
    return;
  }
  const key = paletteKey(item);
  if (!key || !item.deviceId) return;
  if (nodes.value.some((n) => n.key === key)) {
    message.info("该设备已在画布上");
    return;
  }
  if (item.kind === "cable_play") {
    app.graph.sources.push(makeSource(item.deviceId, item.title, "deviceinput"));
  } else if (item.kind === "cable_rec") {
    app.graph.sinks.push(makeSink(item.deviceId, item.title));
  } else if (item.kind === "output") {
    app.graph.sinks.push(makeSink(item.deviceId, app.deviceName(item.deviceId)));
  } else {
    app.graph.sources.push(
      makeSource(item.deviceId, item.title, item.kind === "loopback" ? "loopback" : "deviceinput"),
    );
  }
  if (dropPoint) {
    layout.value = {
      ...layout.value,
      [key]: [
        clamp01((dropPoint.x - NODE_W / 2) / canvasSize.value.w),
        clamp01((dropPoint.y - NODE_H / 2) / canvasSize.value.h),
      ],
    };
    await persistLayout();
  }
  await save();
}

async function removeNode(node: GNode) {
  if (node.sourceId) {
    app.graph.sources = app.graph.sources.filter((s) => s.id !== node.sourceId);
    app.graph.routes = app.graph.routes.filter((r) => r.source_id !== node.sourceId);
  }
  if (node.sinkId) {
    app.graph.sinks = app.graph.sinks.filter((s) => s.id !== node.sinkId);
    app.graph.routes = app.graph.routes.filter((r) => r.sink_id !== node.sinkId);
  }
  if (node.processorId) {
    app.graph.processors = app.graph.processors.filter((p) => p.id !== node.processorId);
    app.graph.routes = app.graph.routes.filter(
      (r) => r.source_id !== node.processorId && r.sink_id !== node.processorId,
    );
  }
  const next = { ...layout.value };
  delete next[node.key];
  layout.value = next;
  await persistLayout();
  await save();
}

async function save() {
  try {
    await app.saveGraph();
  } catch (e) {
    message.error(String(e));
  }
}

// ---------- 参数面板（画布内悬浮弹窗，跟随选中的节点/连线） ----------
const selectedRouteData = computed(() => app.graph.routes.find((r) => r.id === selectedRoute.value) ?? null);
const selectedNodeData = computed(() => nodes.value.find((n) => n.key === selectedNode.value) ?? null);

const POP_W = 296;

/** 弹窗定位：节点在右就往右弹、放不下就往左弹；连线弹在中点旁；都夹在画布内 */
const popStyle = computed(() => {
  const { w, h } = canvasSize.value;
  let left = w / 2 - POP_W / 2;
  let top = 24;
  const nd = selectedNodeData.value;
  if (nd) {
    const fitsRight = nd.x * w + pan.value.x + NODE_W + 12 + POP_W <= w - 8;
    left = fitsRight ? nd.x * w + pan.value.x + NODE_W + 12 : Math.max(8, nd.x * w + pan.value.x - POP_W - 12);
    top = nd.y * h + pan.value.y;
  } else if (selectedRouteData.value) {
    const wr = wires.value.find((x) => x.id === selectedRouteData.value!.id);
    if (wr) {
      left = (wr.from.x + wr.to.x) / 2 + 16 + pan.value.x;
      top = (wr.from.y + wr.to.y) / 2 - 20 + pan.value.y;
    }
  }
  // 高度方向也夹在画布内：弹窗最高撑到画布底，装不下的内容在弹窗内部滚动
  top = Math.max(8, Math.min(top, h - 60));
  return {
    left: `${Math.max(8, Math.min(left, w - POP_W - 8))}px`,
    top: `${top}px`,
    width: `${POP_W}px`,
    maxHeight: `${h - top - 8}px`,
  };
});

const popRouteTitle = computed(() => {
  const w = wires.value.find((x) => x.id === selectedRouteData.value?.id);
  return w ? `${w.fromTitle} → ${w.toTitle}` : "";
});

const procStateText = computed(() => {
  const p = selectedProcessor.value;
  if (!p) return "";
  if (p.type === "switch") return p.enabled ? "开：信号直通" : "关：输出静音";
  return p.enabled ? "处理中（接在信号路径上）" : "已旁路（信号直通）";
});

/** DSP 方块上的状态小字（开关移进弹窗后，方块只负责显示） */
function procStateBadge(procId: string): { text: string; cls: string } {
  const p = app.graph.processors.find((x) => x.id === procId);
  if (!p) return { text: "", cls: "off" };
  if (!p.enabled) return { text: p.type === "switch" ? "关 · 静音" : "已旁路", cls: "off" };
  return { text: p.type === "switch" ? "开 · 直通" : "已启用", cls: "on" };
}

function closePop() {
  selectedNode.value = null;
  selectedRoute.value = null;
}

/** 浮窗打开期间，点击浮窗以外任意位置（左栏、画布空白、页头……）即关闭 */
function closePopOnOutside(e: PointerEvent) {
  const t = e.target as HTMLElement | null;
  if (t?.closest(".canvas-pop")) return;
  closePop();
}

let popOutsideOn = false;
watch(
  () => !!(selectedNode.value || selectedRoute.value),
  (open) => {
    if (open === popOutsideOn) return;
    popOutsideOn = open;
    if (open) window.addEventListener("pointerdown", closePopOnOutside);
    else window.removeEventListener("pointerdown", closePopOnOutside);
  },
);

/**
 * 节点位置完全由用户掌控：只在拖动/拖入/移除/点击「自动排序」时改变并写盘，
 * 不做任何自动疏散 —— 画布缩放时 measure() 只按「像素位置不变」重新归一化，
 * 也不会挪动节点的相对位置。
 */

// 图形数据（画布节点清单）比布局更晚到达 / 增删节点时：
// 剪掉已删除节点的遗留坐标，避免幽灵坐标永远留在配置文件里
watch(
  () =>
    nodes.value
      .map((n) => n.key)
      .join("|"),
  (keys) => {
    if (!layoutLoaded || !keys) return;
    const pruned = pruneLayout(layout.value);
    if (pruned !== layout.value) {
      layout.value = pruned;
      schedulePersistLayout();
    }
  },
);

// ---------- 连线 DSP 节点链 ----------

type DspType = DspNode["type"];

const DSP_META: Record<DspType, { label: string }> = {
  gain: { label: "增益" },
  delay: { label: "延迟" },
  eq3: { label: "三段均衡" },
  peak_eq: { label: "峰式均衡" },
  graph_eq: { label: "图形均衡 10 段" },
  highpass: { label: "高通" },
  lowpass: { label: "低通" },
  bandpass: { label: "带通" },
  switch: { label: "开关" },
};

const DSP_ADD_OPTIONS = (Object.keys(DSP_META) as DspType[]).map((t) => ({
  label: DSP_META[t].label,
  value: t,
}));

/** 图形均衡中心频率标签（与 Rust GRAPH_EQ_BANDS 一致） */
const DSP_GEQ_BANDS = ["31", "63", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"];

interface ParamDef {
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
const DSP_PARAMS: Record<Exclude<DspType, "graph_eq">, ParamDef[]> = {
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
};

/** log 参数 → 滑杆值（log10 域，钳在参数范围内） */
function sliderVal(p: ParamDef, v: number): number {
  if (!p.log) return v;
  return Math.log10(Math.min(p.max, Math.max(p.min, v || p.min)));
}

/** 滑杆值 → 实际参数：log 域取回线性并取整（≥1k 取 10 的倍数，避免 1503Hz 这种脏值） */
function sliderCommit(p: ParamDef, v: number): number {
  if (!p.log) return v;
  const hz = 10 ** v;
  const r = hz >= 1000 ? Math.round(hz / 10) * 10 : Math.round(hz);
  return Math.min(p.max, Math.max(p.min, r));
}

/** 滑杆 tooltip 文案 */
function sliderTip(p: ParamDef, v: number): string {
  if (p.log) return `${Math.round(10 ** v)} Hz`;
  return v.toFixed(p.step < 1 ? 1 : 0) + (p.unit ?? "");
}

// ---------- 高通/低通/带通频响曲线（RBJ 公式，与后端 biquad crate 同源） ----------
const CURVE_W = 236;
const CURVE_H = 84;
const CURVE_FS = 48000;

/** dB → y 像素（+6 .. -48 dB 映射到 0 .. CURVE_H） */
function curveY(db: number): number {
  return ((6 - Math.max(-48, Math.min(6, db))) / 54) * CURVE_H;
}

/** 频率 → x 像素（20Hz..20kHz 对数轴） */
function curveX(f: number): number {
  return (Math.log10(Math.max(20, Math.min(20000, f)) / 20) / Math.log10(1000)) * CURVE_W;
}

/** RBJ 双二阶在某频率的幅度（dB） */
function rbjMagnitudeDb(c: { b0: number; b1: number; b2: number; a1: number; a2: number }, f: number): number {
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

/** 选中 DSP 方块的频响曲线数据（仅高通/低通/带通），用于面板内 SVG */
const dspCurve = computed<{ d: string; marks: number[]; band: [number, number] | null } | null>(() => {
  const p = selectedProcessor.value;
  if (!p) return null;
  let c: { b0: number; b1: number; b2: number; a1: number; a2: number } | null = null;
  let marks: number[] = [];
  let band: [number, number] | null = null;
  if (p.type === "highpass" || p.type === "lowpass") {
    const w0 = (2 * Math.PI * p.freq) / CURVE_FS;
    const cw = Math.cos(w0);
    const alpha = Math.sin(w0) / (2 * p.q);
    const a0 = 1 + alpha;
    c =
      p.type === "highpass"
        ? { b0: (1 + cw) / 2 / a0, b1: (-(1 + cw)) / a0, b2: (1 + cw) / 2 / a0, a1: (-2 * cw) / a0, a2: (1 - alpha) / a0 }
        : { b0: (1 - cw) / 2 / a0, b1: (1 - cw) / a0, b2: (1 - cw) / 2 / a0, a1: (-2 * cw) / a0, a2: (1 - alpha) / a0 };
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
    c = { b0: (alpha * g) / a0, b1: 0, b2: (-alpha * g) / a0, a1: (-2 * cw) / a0, a2: (1 - alpha) / a0 };
    marks = [p.low_freq, p.high_freq];
    band = [p.low_freq, p.high_freq];
  } else {
    return null;
  }
  const pts: string[] = [];
  for (let i = 0; i <= 120; i++) {
    const f = 20 * (1000 ** (i / 120));
    pts.push(`${curveX(f).toFixed(1)},${curveY(rbjMagnitudeDb(c, f)).toFixed(1)}`);
  }
  return { d: `M ${pts.join(" L ")}`, marks, band };
});

function makeDspNode(t: DspType): DspNode {
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
  }
}

/** 选中画布上的 DSP 方块时，对应的 Processor */
const selectedProcessor = computed(() =>
  selectedNodeData.value?.processorId
    ? app.graph.processors.find((p) => p.id === selectedNodeData.value!.processorId) ?? null
    : null,
);

/** 方块参数有任何变化后下发引擎（后端命令内部持久化，无需再 save） */
function touchProcessor(procId: string) {
  const p = app.graph.processors.find((x) => x.id === procId);
  if (!p) return;
  api.setProcessorParams(procId, { ...p }).catch((e) => message.error(String(e)));
}

function onProcEnable(procId: string, enabled: boolean) {
  const p = app.graph.processors.find((x) => x.id === procId);
  if (!p) return;
  p.enabled = enabled;
  touchProcessor(procId);
}

/** key 为数字时表示 graph_eq 的第几段增益 */
function onProcParam(procId: string, key: string | number, value: number) {
  const p = app.graph.processors.find((x) => x.id === procId);
  if (!p) return;
  if (typeof key === "number") {
    p.gains_db[key] = value;
  } else {
    (p as Record<string, unknown>)[key] = value;
  }
  touchProcessor(procId);
}

function onSinkVolume(sinkId: string, volume: number) {
  const k = app.graph.sinks.find((x) => x.id === sinkId);
  if (k) k.volume = volume;
  api.setSinkVolume(sinkId, volume).catch((e) => message.error(String(e)));
}

function onSourceEnabled(sourceId: string, enabled: boolean) {
  const s = app.graph.sources.find((x) => x.id === sourceId);
  if (s) s.enabled = enabled;
  save();
}

function onSinkEnabled(sinkId: string, enabled: boolean) {
  const k = app.graph.sinks.find((x) => x.id === sinkId);
  if (k) k.enabled = enabled;
  save();
}

const level = (id?: string) => (id ? Math.min(1, app.levels[id] ?? 0) : 0);
const processorEnabledOf = (id?: string) =>
  id ? (app.graph.processors.find((p) => p.id === id)?.enabled ?? true) : true;
const sinkVolumeOf = (id?: string) => (id ? (app.graph.sinks.find((s) => s.id === id)?.volume ?? 1) : 1);
const sourceEnabledOf = (id?: string) => (id ? (app.graph.sources.find((s) => s.id === id)?.enabled ?? true) : true);
const sinkEnabledOf = (id?: string) => (id ? (app.graph.sinks.find((s) => s.id === id)?.enabled ?? true) : true);

// ---------- 系统音量（Windows 端点音量，影响该设备上所有声音） ----------
const deviceVolumes = ref<Record<string, number>>({});
/** 拖动中的待写队列：device_id -> 定时器 */
const pendingVolume = new Map<string, { timer: number; value: number }>();

/** 取某个 sink 节点对应的设备 id */
function sinkDeviceId(node: GNode): string | undefined {
  if (!node.sinkId) return undefined;
  return app.graph.sinks.find((s) => s.id === node.sinkId)?.device_id;
}

const volumeOf = (deviceId?: string) => (deviceId ? (deviceVolumes.value[deviceId] ?? 1) : 1);

async function refreshDeviceVolumes() {
  const ids = new Set<string>();
  for (const node of nodes.value) {
    const id = sinkDeviceId(node);
    if (id) ids.add(id);
  }
  for (const id of ids) {
    try {
      deviceVolumes.value[id] = await api.getDeviceVolume(id);
    } catch {
      // 设备可能已拔掉，忽略
    }
  }
}

async function onDeviceVolume(deviceId: string, value: number) {
  // 滑块先本地即时反馈，写入按设备节流，避免拖动时疯狂 COM/IPC 调用
  deviceVolumes.value[deviceId] = value;
  const prev = pendingVolume.get(deviceId);
  if (prev) window.clearTimeout(prev.timer);
  const timer = window.setTimeout(async () => {
    pendingVolume.delete(deviceId);
    const target = deviceVolumes.value[deviceId];
    try {
      await api.setDeviceVolume(deviceId, target);
    } catch (e) {
      message.error(String(e));
    }
  }, 60);
  pendingVolume.set(deviceId, { timer, value });
}

/** 组件销毁前把还没落盘的音量补写一次 */
function flushPendingVolumes() {
  for (const [id, p] of pendingVolume) {
    window.clearTimeout(p.timer);
    api.setDeviceVolume(id, p.value).catch(() => undefined);
  }
  pendingVolume.clear();
}

// ---------- 系统默认设备 ----------
const defaultOut = computed(() => app.devices.find((d) => d.kind === "output" && d.is_default)?.id ?? null);
const defaultIn = computed(() => app.devices.find((d) => d.kind === "input" && d.is_default)?.id ?? null);

/** 系统默认设备候选。排除 usbip:// 合成端点（不是真实 Windows 端点，设默认必定失败）；
 *  线路的真实扬声器/麦克风端点改标为「XX（虚拟线路）」——默认播放选它，系统声音才进得来。 */
function options(kind: "input" | "output") {
  return app.devices
    .filter((d) => d.kind === kind && !d.id.startsWith("usbip://"))
    .map((d: DeviceInfo) => {
      const c = realCableDevice(d);
      if (c) return { label: `${c.display_name}（虚拟线路）`, value: d.id };
      return { label: d.name + (d.is_virtual ? "（虚拟）" : ""), value: d.id };
    });
}

async function setDefault(deviceId: string) {
  try {
    app.devices = await api.setDefaultDevice(deviceId);
    const name = app.devices.find((d) => d.id === deviceId)?.name ?? deviceId;
    message.success(`已把系统默认设备切换为「${name}」`);
  } catch (e) {
    message.error(String(e));
  }
}

// ---------- 右键菜单 / 键盘删除 ----------
const ctxMenu = ref<{ x: number; y: number; kind: "wire" | "node"; id: string; title: string } | null>(null);

function openWireMenu(e: MouseEvent, w: Wire) {
  selectedRoute.value = w.id;
  selectedNode.value = null;
  ctxMenu.value = {
    x: Math.min(e.clientX, window.innerWidth - 220),
    y: Math.min(e.clientY, window.innerHeight - 130),
    kind: "wire",
    id: w.id,
    title: `${w.fromTitle} → ${w.toTitle}`,
  };
}

function openNodeMenu(e: MouseEvent, node: GNode) {
  selectedNode.value = node.key;
  selectedRoute.value = null;
  ctxMenu.value = {
    x: Math.min(e.clientX, window.innerWidth - 220),
    y: Math.min(e.clientY, window.innerHeight - 130),
    kind: "node",
    id: node.key,
    title: node.title,
  };
}

function closeMenu() {
  ctxMenu.value = null;
}

async function menuDisconnect() {
  const m = ctxMenu.value;
  closeMenu();
  if (m?.kind === "wire") await disconnect(m.id);
}

async function menuRemoveNode() {
  const m = ctxMenu.value;
  closeMenu();
  if (m?.kind !== "node") return;
  const node = nodeByKey.value.get(m.id);
  if (node) await removeNode(node);
}

/** 窗口尺寸变了：先重测画布，再重算整页高度分配 */
function onWindowResize() {
  measure();
  fitCanvas();
}

function onKeyDown(e: KeyboardEvent) {
  if (e.key === "Escape" && (clickPending.value || drag.value)) {
    setClickPending(null);
    drag.value = null;
    detachWindowDrag();
    return;
  }
  if (e.key !== "Delete" && e.key !== "Backspace") return;
  const t = e.target as HTMLElement | null;
  const tag = t?.tagName;
  if (tag === "INPUT" || tag === "TEXTAREA" || t?.isContentEditable) return;
  if (selectedRoute.value) {
    e.preventDefault();
    disconnect(selectedRoute.value);
  } else if (selectedNode.value) {
    const node = nodeByKey.value.get(selectedNode.value);
    if (node) {
      e.preventDefault();
      removeNode(node);
    }
  }
}

// ---------- 生命周期 ----------
let pageObserver: ResizeObserver | null = null;
let unlistenUsbip: (() => void) | undefined;

onMounted(async () => {
  measure();
  requestAnimationFrame(() => {
    measure();
    fitCanvas();
  });
  window.addEventListener("resize", onWindowResize);
  window.addEventListener("keydown", onKeyDown);
  // 画布尺寸随窗口/父容器变化（切页签也会变）
  if (typeof ResizeObserver !== "undefined" && canvasEl.value) {
    resizeObserver = new ResizeObserver(() => measure());
    resizeObserver.observe(canvasEl.value);
    // 整页高度变化（提示条出现/消失、上方内容变化）就重新分配画布高度
    if (pageEl.value) {
      pageObserver = new ResizeObserver(() => fitCanvas());
      pageObserver.observe(pageEl.value);
    }
  }
  try {
    const loaded = sanitizeLayout(await api.getMixerLayout());
    layout.value = loaded;
    layoutLoaded = true;
  } catch {
    layout.value = {};
    layoutLoaded = true;
  }
  try {
    const s = await api.usbipStatus();
    cables.value = s.cables;
    usbipRunning.value = s.running;
  } catch {
    cables.value = [];
    usbipRunning.value = false;
  }
  // 之后不再轮询：后端在附加/断开/自愈完成后广播 `usbip-status` 事件，
  // 这里只监听刷新（启动自动附加要等 UAC，vhci 也会自发重复 import，
  // 状态只拉一次会一直显示「未接入（需附加）」）
  unlistenUsbip = await listen<UsbIpStatus>("usbip-status", (e) => {
    cables.value = e.payload.cables;
    usbipRunning.value = e.payload.running;
  });
  await refreshDeviceVolumes();
  fitCanvas();
});

// 节点集合变化（拖入/移除设备、线路增减）时刷新一次各设备的系统音量
watch(
  () => nodes.value.map((n) => n.key).join("|"),
  () => {
    refreshDeviceVolumes();
  },
);

onUnmounted(() => {
  window.removeEventListener("resize", onWindowResize);
  window.removeEventListener("keydown", onKeyDown);
  resizeObserver?.disconnect();
  resizeObserver = null;
  pageObserver?.disconnect();
  pageObserver = null;
  detachWindowDrag();
  window.removeEventListener("pointerdown", closePopOnOutside);
  if (persistTimer !== null) clearTimeout(persistTimer);
  unlistenUsbip?.();
  flushPendingVolumes();
});

/** 打开右键菜单后，下一次点击任意位置即关闭 */
watch(ctxMenu, (m) => {
  if (m) window.addEventListener("pointerdown", closeMenu, { once: true });
});

function openWireMenuDeferred(e: MouseEvent, w: Wire) {
  // pointerdown（关闭旧菜单）先于 contextmenu 触发，这里用宏任务确保菜单不被立刻关掉
  setTimeout(() => openWireMenu(e, w), 0);
}

function openNodeMenuDeferred(e: MouseEvent, node: GNode) {
  setTimeout(() => openNodeMenu(e, node), 0);
}
</script>

<template>
  <div ref="pageEl" class="mixer-page">
  <!-- 系统默认设备（改的是 Windows 设置，不是本应用的混音图） -->
  <n-card size="small" title="系统默认设备" style="margin-bottom: 14px">
    <div style="display: grid; grid-template-columns: 1fr 1fr; gap: 16px">
      <div>
        <div class="item-sub" style="margin-bottom: 6px">
          默认播放（把系统声音送进虚拟线路：选 Virtual Cable NN）
        </div>
        <n-select
          :value="defaultOut"
          :options="options('output')"
          size="small"
          filterable
          placeholder="选择默认播放设备"
          @update:value="setDefault"
        />
      </div>
      <div>
        <div class="item-sub" style="margin-bottom: 6px">
          默认录音（让应用从虚拟线路录音：选 Virtual Cable NN）
        </div>
        <n-select
          :value="defaultIn"
          :options="options('input')"
          size="small"
          filterable
          placeholder="选择默认录音设备"
          @update:value="setDefault"
        />
      </div>
    </div>
  </n-card>

  <div class="canvas-grid">
    <!-- 左栏：设备 + DSP 两张独立卡片（绝对定位：不参与行高计算，永远不会撑开画布） -->
    <div class="sidebar">
      <!-- 设备面板：按住拖到画布上生成节点（也可直接点击 = 放到默认位置） -->
      <n-card size="small" title="设备" class="dev-card">
        <n-text depth="3" style="font-size: 12px; display: block; margin-bottom: 8px">
          按住拖到右侧画布；直接点击则放到默认位置。
        </n-text>
        <div class="palette">
          <div
            v-for="(item, i) in paletteDevices"
            :key="i"
            class="palette-item"
            :class="{ used: paletteExisting(item) }"
            @pointerdown="startPaletteDrag($event, item)"
          >
            <n-tag size="tiny" :type="KIND_META[item.kind].type" :bordered="false">
              {{ KIND_META[item.kind].tag }}
            </n-tag>
            <span class="palette-title">{{ item.title }}</span>
          </div>
          <n-text v-if="!paletteDevices.length" depth="3" style="font-size: 12px">未检测到设备</n-text>
        </div>
      </n-card>

      <!-- DSP 处理方块：独立卡片，不限数量 -->
      <n-card size="small" title="DSP 处理" class="dev-card dsp-card">
        <n-text depth="3" style="font-size: 12px; display: block; margin-bottom: 8px">
          拖到画布上串进线路，对路过的信号做处理；不限数量。
        </n-text>
        <div class="palette">
          <div
            v-for="(item, i) in paletteDsp"
            :key="'dsp' + i"
            class="palette-item palette-item-dsp"
            @pointerdown="startPaletteDrag($event, item)"
          >
            <n-tag size="tiny" :type="KIND_META[item.kind].type" :bordered="false" class="tag-dsp">
              {{ KIND_META[item.kind].tag }}
            </n-tag>
            <span class="palette-title">{{ item.title }}</span>
          </div>
        </div>
      </n-card>
    </div>

    <!-- 画布 -->
    <n-card size="small" title="接线画布" class="canvas-card">
      <template #header-extra>
        <div style="display: flex; align-items: center; gap: 12px; font-size: 12px">
          <span class="legend"><i class="legend-dot t-out" />输出端子（信号流出）</span>
          <span class="legend"><i class="legend-dot t-in" />输入端子（信号流入）</span>
          <span class="legend"><i class="legend-dot compat" />拖线时可接</span>
          <n-button size="tiny" quaternary @click="centerView" title="把所有节点居中到画布中间">居中</n-button>
          <n-button size="tiny" quaternary @click="autoArrange" title="按信号流向自动分层排列节点">自动排序</n-button>
        </div>
      </template>
      <div
        ref="canvasEl"
        class="canvas"
        :class="{ wiring: !!wireSource, panning: drag?.kind === 'pan' }"
        :style="canvasStyle"
        @pointerdown.self="startCanvasPan"
      >
        <!-- 世界层：节点/连线都在这个世界坐标系里，拖空白处平移整个世界。
             它 inset:0 铺满画布，空白处的 pointerdown 落在这层而不是 .canvas 上，
             平移入口必须挂在这里（挂 .canvas 上的 .self 永远不命中，画布就拖不动） -->
        <div
          class="canvas-world"
          :style="{ transform: `translate(${pan.x}px, ${pan.y}px)` }"
          @pointerdown.self="startCanvasPan"
        >
        <svg class="wire-layer" :width="canvasSize.w" :height="canvasSize.h">
          <defs>
            <marker
              id="wire-arrow"
              viewBox="0 0 10 10"
              refX="0"
              refY="5"
              markerWidth="6"
              markerHeight="6"
              orient="auto"
            >
              <path d="M 0 0 L 10 5 L 0 10 z" fill="#4b9cd3" />
            </marker>
            <marker
              id="wire-arrow-muted"
              viewBox="0 0 10 10"
              refX="0"
              refY="5"
              markerWidth="6"
              markerHeight="6"
              orient="auto"
            >
              <path d="M 0 0 L 10 5 L 0 10 z" fill="#8a8a8a" />
            </marker>
            <marker
              id="wire-arrow-sel"
              viewBox="0 0 10 10"
              refX="0"
              refY="5"
              markerWidth="6"
              markerHeight="6"
              orient="auto"
            >
              <path d="M 0 0 L 10 5 L 0 10 z" fill="#f0a020" />
            </marker>
          </defs>
          <path
            v-for="w in wires"
            :key="w.id"
            :d="wirePath(w.from, w.to)"
            class="wire"
            :class="{ muted: w.muted, selected: selectedRoute === w.id }"
            :marker-end="
              selectedRoute === w.id
                ? 'url(#wire-arrow-sel)'
                : w.muted
                  ? 'url(#wire-arrow-muted)'
                  : 'url(#wire-arrow)'
            "
            @pointerdown.stop="selectedRoute = w.id; selectedNode = null"
            @contextmenu.prevent.stop="openWireMenuDeferred($event, w)"
          />
          <path v-if="pendingWire" :d="previewWirePath(pendingWire.from, pendingWire.to)" class="wire pending" />
          <circle
            v-if="pendingWire"
            :cx="pendingWire.to.x"
            :cy="pendingWire.to.y"
            r="4.5"
            class="pending-dot"
          />
          <template v-if="nodeGuides">
            <line :x1="0" :y1="nodeGuides.y" :x2="canvasSize.w" :y2="nodeGuides.y" class="guide" />
            <line :x1="nodeGuides.x" :y1="0" :x2="nodeGuides.x" :y2="canvasSize.h" class="guide" />
          </template>
        </svg>

        <!-- 拖动设备时：虚线落位框 -->
        <div
          v-if="dropPreview"
          class="drop-preview"
          :style="{
            left: dropPreview.x + 'px',
            top: dropPreview.y + 'px',
            width: NODE_W + 'px',
            height: NODE_H + 'px',
          }"
        >
          <span>{{ dropPreview.title }}</span>
        </div>

        <div
          v-for="node in nodes"
          :key="node.key"
          class="node"
          :class="[`node-${node.kind}`, { selected: selectedNode === node.key }]"
          :data-node-key="node.key"
          :style="{
            left: node.x * canvasSize.w + 'px',
            top: node.y * canvasSize.h + 'px',
            width: NODE_W + 'px',
            height: NODE_H + 'px',
          }"
          @pointerdown="startNodeDrag($event, node)"
          @click.stop="onNodeClick(node)"
          @contextmenu.prevent.stop="openNodeMenuDeferred($event, node)"
        >
          <div class="node-head">
            <n-tag size="tiny" :type="KIND_META[node.kind].type" :bordered="false" :class="{ 'tag-dsp': node.kind === 'dsp' }">
              {{ KIND_META[node.kind].tag }}
            </n-tag>
            <span class="node-title">{{ node.title }}</span>
            <span class="node-close" @pointerdown.stop @click.stop="removeNode(node)">×</span>
          </div>
          <div class="node-sub">{{ node.subtitle }}</div>
          <MeterBar
            class="node-meter"
            :level="Math.max(level(node.sourceId), level(node.sinkId), level(node.processorId))"
          />
          <!-- DSP 方块：只显示启用状态（开关在悬浮弹窗里），开关节点关 = 静音、其它 = 旁路 -->
          <div v-if="node.processorId" class="node-state" @pointerdown.stop @click.stop>
            <i class="state-dot" :class="procStateBadge(node.processorId).cls" />
            {{ procStateBadge(node.processorId).text }}
          </div>

          <!-- 输出/线路输出节点：直接调该设备的 **Windows 系统音量** -->
          <div v-if="node.sinkId" class="node-vol" @pointerdown.stop @click.stop>
            <span class="node-vol-icon">🔊</span>
            <n-slider
              :value="volumeOf(sinkDeviceId(node))"
              :min="0"
              :max="1"
              :step="0.01"
              :tooltip="false"
              size="small"
              @update:value="(v: number) => onDeviceVolume(sinkDeviceId(node)!, v)"
            />
            <span class="node-vol-pct">{{ Math.round(volumeOf(sinkDeviceId(node)) * 100) }}%</span>
          </div>

          <!-- 端子：小圆点只是视觉，命中由更宽的 term-zone 负责（整条边都能拖出连线） -->
          <span
            v-if="node.hasIn"
            class="terminal t-in"
            :class="termState(node, 'in')"
            :style="termStyle(node, 'in')"
          />
          <div
            v-if="node.hasIn"
            class="term-zone zone-in"
            :data-term="`${node.key}|in`"
            :title="termTitle(node, 'in')"
            @pointerdown.stop="startWireDrag($event, node, 'in')"
            @click.stop
          />
          <span
            v-if="node.hasOut"
            class="terminal t-out"
            :class="termState(node, 'out')"
            :style="termStyle(node, 'out')"
          />
          <div
            v-if="node.hasOut"
            class="term-zone zone-out"
            :data-term="`${node.key}|out`"
            :title="termTitle(node, 'out')"
            @pointerdown.stop="startWireDrag($event, node, 'out')"
            @click.stop
          />
        </div>
        </div><!-- /canvas-world -->

        <n-text
          v-if="!nodes.length"
          depth="3"
          style="position: absolute; left: 50%; top: 46%; transform: translate(-50%, -50%); font-size: 13px"
        >
          从左侧「设备」拖入设备开始接线
        </n-text>

        <!-- 点击节点 / 连线后的悬浮属性面板（跟随目标定位，画布下方不再有固定参数栏） -->
        <div
          v-if="selectedRouteData || selectedNodeData"
          class="canvas-pop"
          :style="popStyle"
          @pointerdown.stop
          @click.stop
          @contextmenu.prevent.stop
        >
          <div class="canvas-pop-head">
            <n-tag
              size="small"
              :bordered="false"
              :type="selectedNodeData ? KIND_META[selectedNodeData.kind].type : 'info'"
              :class="{ 'tag-dsp': selectedNodeData?.kind === 'dsp' }"
            >
              {{ selectedNodeData ? KIND_META[selectedNodeData.kind].tag : "连线" }}
            </n-tag>
            <span class="canvas-pop-title">{{ selectedNodeData?.title ?? popRouteTitle }}</span>
            <span class="node-close" @pointerdown.stop @click.stop="closePop">×</span>
          </div>

          <!-- 连线：只代表链接关系，唯一操作是断开 -->
          <template v-if="selectedRouteData">
            <div class="canvas-pop-row">
              <n-button size="tiny" quaternary type="error" style="margin-left: auto" @click="disconnect(selectedRouteData.id)">
                断开
              </n-button>
            </div>
          </template>

          <!-- 节点：采集 / 混音音量 / 系统音量 / DSP 参数 -->
          <template v-else>
            <div v-if="selectedNodeData!.sourceId" class="canvas-pop-row">
              <span class="item-sub" style="width: 48px">采集</span>
              <n-switch
                :value="sourceEnabledOf(selectedNodeData!.sourceId)"
                size="small"
                @update:value="(v: boolean) => onSourceEnabled(selectedNodeData!.sourceId!, v)"
              />
            </div>
            <template v-if="selectedNodeData!.sinkId">
              <div class="canvas-pop-row">
                <span class="item-sub" style="width: 48px">混音</span>
                <n-slider
                  :value="sinkVolumeOf(selectedNodeData!.sinkId)"
                  :min="0"
                  :max="1"
                  :step="0.01"
                  :format-tooltip="(v: number) => Math.round(v * 100) + '%'"
                  style="flex: 1"
                  @update:value="(v: number) => onSinkVolume(selectedNodeData!.sinkId!, v)"
                />
                <n-switch
                  :value="sinkEnabledOf(selectedNodeData!.sinkId)"
                  size="small"
                  @update:value="(v: boolean) => onSinkEnabled(selectedNodeData!.sinkId!, v)"
                />
              </div>
              <div class="canvas-pop-row">
                <span class="item-sub" style="width: 48px">系统音量</span>
                <n-slider
                  :value="volumeOf(sinkDeviceId(selectedNodeData!))"
                  :min="0"
                  :max="1"
                  :step="0.01"
                  :format-tooltip="(v: number) => Math.round(v * 100) + '%'"
                  style="flex: 1"
                  @update:value="(v: number) => onDeviceVolume(sinkDeviceId(selectedNodeData!)!, v)"
                />
              </div>
            </template>

            <!-- DSP 方块参数编辑 -->
            <div v-if="selectedProcessor" class="dsp-section">
              <div class="dsp-head">
                <n-text depth="3" style="font-size: 12px">{{ procStateText }}</n-text>
                <n-switch
                  :value="selectedProcessor.enabled"
                  size="small"
                  @update:value="(v: boolean) => onProcEnable(selectedProcessor!.id, v)"
                />
              </div>
              <div v-if="selectedProcessor.type !== 'graph_eq'" class="dsp-params">
                <div v-for="p in DSP_PARAMS[selectedProcessor.type]" :key="p.key" class="dsp-param">
                  <span class="item-sub" style="width: 48px">{{ p.label }}</span>
                  <n-slider
                    :value="sliderVal(p, (selectedProcessor as Record<string, number>)[p.key])"
                    :min="p.log ? Math.log10(p.min) : p.min"
                    :max="p.log ? Math.log10(p.max) : p.max"
                    :step="p.log ? 0.005 : p.step"
                    :format-tooltip="(v: number) => sliderTip(p, v)"
                    style="flex: 1"
                    @update:value="(v: number) => onProcParam(selectedProcessor!.id, p.key, sliderCommit(p, v))"
                  />
                </div>
              </div>
              <div v-else class="dsp-geq">
                <div v-for="(band, b) in DSP_GEQ_BANDS" :key="b" class="dsp-geq-band">
                  <!-- 竖向滑杆的高度必须给真实 CSS 高度（naive-ui 没有 height prop，写属性不生效就量不出轨道） -->
                  <n-slider
                    vertical
                    :value="selectedProcessor.gains_db[b]"
                    :min="-24"
                    :max="24"
                    :step="1"
                    style="height: 80px"
                    @update:value="(v: number) => onProcParam(selectedProcessor!.id, b, v)"
                  />
                  <span class="item-sub">{{ band }}</span>
                </div>
              </div>
              <!-- 高通/低通/带通：实时频响曲线（对数频轴，虚线 = 频点，色块 = 通带） -->
              <svg v-if="dspCurve" class="dsp-curve" :viewBox="`0 0 ${CURVE_W} ${CURVE_H}`">
                <line :x1="0" :y1="curveY(0)" :x2="CURVE_W" :y2="curveY(0)" class="dsp-curve-zero" />
                <rect
                  v-if="dspCurve.band"
                  :x="curveX(dspCurve.band[0])"
                  :y="0"
                  :width="curveX(dspCurve.band[1]) - curveX(dspCurve.band[0])"
                  :height="CURVE_H"
                  class="dsp-curve-band"
                />
                <line
                  v-for="(f, i) in dspCurve.marks"
                  :key="i"
                  :x1="curveX(f)"
                  :y1="0"
                  :x2="curveX(f)"
                  :y2="CURVE_H"
                  class="dsp-curve-mark"
                />
                <path :d="dspCurve.d" class="dsp-curve-line" />
              </svg>
            </div>

            <div class="canvas-pop-row" style="margin-top: 4px">
              <n-button size="tiny" quaternary type="error" @click="removeNode(selectedNodeData!)">移除节点</n-button>
            </div>
          </template>
        </div>
      </div>
    </n-card>
  </div>

  <div v-if="notice" ref="noticeEl" class="notice-wrap">
    <n-alert :type="notice.type">{{ notice.text }}</n-alert>
  </div>

  <!-- 拖动设备时的跟随提示 -->
  <div
    v-if="ghost"
    class="drag-ghost"
    :style="{ left: ghost.cx + 12 + 'px', top: ghost.cy + 12 + 'px' }"
  >
    {{ ghost.item.title }}
  </div>

  <!-- 右键菜单：连线和节点都能在这里删 -->
  <div
    v-if="ctxMenu"
    class="ctx-menu"
    :style="{ left: ctxMenu.x + 'px', top: ctxMenu.y + 'px' }"
    @pointerdown.stop
    @contextmenu.prevent
  >
    <div class="ctx-title">{{ ctxMenu.title }}</div>
    <template v-if="ctxMenu.kind === 'wire'">
      <div class="ctx-item danger" @click="menuDisconnect">断开这条连线（Delete）</div>
    </template>
    <template v-else>
      <div class="ctx-item danger" @click="menuRemoveNode">移除这个节点（Delete）</div>
    </template>
  </div>
  </div>
</template>

<style scoped>
/* 整页容器：高度由 fitCanvas() 通过画布高度调平，刚好占满页签一屏；
   底部留出和左右一样的 16px（页签左右各 16px，见 App.vue 的 n-tabs padding） */
.mixer-page {
  min-height: 0;
  padding-bottom: 16px;
}
/* 连线 DSP 节点链 */
.dsp-section {
  margin-top: 10px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.dsp-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}
.dsp-node {
  border: 1px solid rgba(128, 128, 128, 0.25);
  border-radius: 6px;
  padding: 6px 8px;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.dsp-node-head {
  display: flex;
  align-items: center;
  gap: 6px;
}
.dsp-params {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.dsp-param {
  display: flex;
  align-items: center;
  gap: 8px;
}
.dsp-geq {
  display: flex;
  justify-content: space-between;
  gap: 4px;
}
.dsp-geq-band {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  font-size: 10px;
}
/* 频响曲线（高通/低通/带通面板内嵌） */
.dsp-curve {
  width: 236px;
  max-width: 100%;
  height: 84px;
  border: 1px solid rgba(128, 128, 128, 0.25);
  border-radius: 4px;
  background: rgba(128, 128, 128, 0.06);
}
.dsp-curve-line {
  fill: none;
  stroke: #4b9cd3;
  stroke-width: 1.5;
}
.dsp-curve-zero {
  stroke: rgba(128, 128, 128, 0.35);
  stroke-width: 1;
  stroke-dasharray: 3 3;
}
.dsp-curve-mark {
  stroke: rgba(240, 160, 32, 0.55);
  stroke-width: 1;
  stroke-dasharray: 2 3;
}
.dsp-curve-band {
  fill: rgba(75, 156, 211, 0.12);
}
/* 网格：左栏绝对定位挂在第一列上（不参与行高计算），行高只由画布卡决定 */
.canvas-grid {
  position: relative;
  display: grid;
  grid-template-columns: 262px 1fr;
  gap: 14px;
}
.sidebar {
  position: absolute;
  left: 0;
  top: 0;
  bottom: 0;
  width: 262px;
  display: flex;
  flex-direction: column;
  gap: 14px;
  min-height: 0;
}
.canvas-card {
  grid-column: 2;
}
/* 设备卡片与画布等高：卡片内部纵向 flex，设备列表吃掉剩余高度、超出自己滚动 */
.dev-card {
  display: flex;
  flex-direction: column;
  flex: 1 1 0;
  min-height: 0;
}
/* DSP 处理卡片：与设备卡平分左栏高度、列表内部滚动，紫色描边呼应方块配色 */
.dsp-card {
  flex: 1 1 0;
  min-height: 0;
  border: 1px solid rgba(156, 108, 236, 0.35);
}
.dsp-card :deep(.n-card-header__main) {
  color: #cfa9f9;
}
/* naive-ui 卡片内容层的类名是 n-card-content（单下划线）——写成 n-card__content 匹配不到，
   min-height:0 失效后列表会把卡片内容层撑高、溢出卡片盖到画布上 */
.dev-card :deep(.n-card-content) {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
}
.palette {
  flex: 1;
  /* min-height 必须为 0：列表是滚动容器，最小高度不为 0 会顶高左栏、进而顶高网格行 */
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: 6px;
  overflow-y: auto;
}
.palette-item {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 5px 7px;
  border: 1px solid rgba(128, 128, 128, 0.3);
  border-radius: 6px;
  cursor: grab;
  user-select: none;
  touch-action: none;
  font-size: 12px;
  flex-shrink: 0; /* 列表超长时保持条目完整高度交给滚动，不被压缩 */
}
/* WebView2 默认用 Fluent 悬浮滚动条（空闲时不可见），自定义后强制显示常驻滚动条 */
.palette::-webkit-scrollbar,
.canvas-pop::-webkit-scrollbar {
  width: 8px;
  height: 8px;
}
.palette::-webkit-scrollbar-thumb,
.canvas-pop::-webkit-scrollbar-thumb {
  background: rgba(255, 255, 255, 0.22);
  border-radius: 4px;
}
.palette::-webkit-scrollbar-thumb:hover,
.canvas-pop::-webkit-scrollbar-thumb:hover {
  background: rgba(255, 255, 255, 0.38);
}
.palette::-webkit-scrollbar-track,
.canvas-pop::-webkit-scrollbar-track {
  background: transparent;
}
.palette-item:hover {
  background: rgba(128, 128, 128, 0.12);
}
.palette-item.used {
  opacity: 0.5;
}
.palette-title {
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
/* DSP 面板项跟随方块配色（紫） */
.palette-item-dsp {
  border-color: rgba(156, 108, 236, 0.45);
}
.palette-item-dsp:hover {
  border-color: rgba(156, 108, 236, 0.8);
}
.canvas {
  position: relative;
  /* 高度跟随窗口（宽度本来就自适应）；实际高度由 :style 绑定，这里只是兜底 */
  height: clamp(320px, calc(100vh - 392px), 1200px);
  border: 1px dashed rgba(128, 128, 128, 0.35);
  border-radius: 8px;
  background-image: radial-gradient(rgba(128, 128, 128, 0.16) 1px, transparent 1px);
  background-size: 20px 20px;
  overflow: hidden;
  cursor: grab; /* 拖空白处平移画布 */
}
.canvas.panning {
  cursor: grabbing;
}
/* 世界层：节点/连线的坐标系，平移 = transform 整体挪 */
.canvas-world {
  position: absolute;
  inset: 0;
  will-change: transform;
}
/* 底部提示条：间距要和 NOTICE_GAP 一致（那段高度会从画布高度里扣掉） */
.notice-wrap {
  margin-top: 14px;
}
.wire-layer {
  position: absolute;
  left: 0;
  top: 0;
  pointer-events: none;
}
.wire {
  fill: none;
  stroke: #4b9cd3;
  stroke-width: 2.5;
  pointer-events: stroke;
  cursor: pointer;
}
.wire.muted {
  stroke: #8a8a8a;
  stroke-dasharray: 5 4;
}
.wire.selected {
  stroke: #f0a020;
  stroke-width: 3.5;
}
.wire.pending {
  stroke: #f0a020;
  stroke-width: 2.5;
  stroke-dasharray: 7 5;
  animation: dashmove 0.6s linear infinite;
}
.pending-dot {
  fill: #f0a020;
  stroke: none;
}
.guide {
  stroke: rgba(240, 160, 32, 0.45);
  stroke-width: 1;
  stroke-dasharray: 6 6;
}
@keyframes dashmove {
  to {
    stroke-dashoffset: -24;
  }
}
.drop-preview {
  position: absolute;
  z-index: 3;
  box-sizing: border-box;
  border: 2px dashed #f0a020;
  border-radius: 8px;
  background: rgba(240, 160, 32, 0.12);
  pointer-events: none;
  display: flex;
  align-items: center;
  justify-content: center;
  color: #f0a020;
  font-size: 12px;
  padding: 4px 8px;
  text-align: center;
  overflow: hidden;
}
.node {
  position: absolute;
  /* 必须建立层叠上下文：否则端子圈(z-index:2)参与根层叠，节点重叠时圈会浮到别的节点主体上面 */
  z-index: 1;
  border: 1px solid rgba(255, 255, 255, 0.18);
  border-radius: 8px;
  /* 应用是深色主题（naive-ui darkTheme + #101014 背景），节点跟随深色 */
  background: #1f1f23;
  color: #e8e8ea;
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.35);
  /* 左右留出 18px：端子会伸进盒子 8px，文字不会被压住 */
  padding: 8px 18px;
  box-sizing: border-box;
  cursor: move;
  user-select: none;
  touch-action: none;
  font-size: 12px;
}
.node:hover {
  border-color: rgba(255, 255, 255, 0.32);
}
.node.selected {
  z-index: 2; /* 重叠时选中的节点浮到上面，避免被压住 */
  border-color: #f0a020;
  box-shadow: 0 0 0 2px rgba(240, 160, 32, 0.25), 0 2px 8px rgba(0, 0, 0, 0.35);
}
/* DSP 处理方块：紫色调，和普通设备节点一眼区分开 */
.node.node-dsp {
  border-color: rgba(156, 108, 236, 0.55);
  background: #232030;
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.35), inset 0 0 0 1px rgba(156, 108, 236, 0.12);
}
.node.node-dsp.selected {
  border-color: rgba(156, 108, 236, 0.9);
}
.node-head {
  display: flex;
  align-items: center;
  gap: 6px;
}
.node-title {
  flex: 1;
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.node-sub {
  margin-top: 2px;
  opacity: 0.62;
  font-size: 11px;
  line-height: 1.3;
  /* 最多两行：副标题常是「系统播放端（扬声器） · 拷贝：… · 状态」长句，一行省略号看不全 */
  display: -webkit-box;
  -webkit-box-orient: vertical;
  -webkit-line-clamp: 2;
  overflow: hidden;
  white-space: normal;
  word-break: break-all;
}
.node-vol {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 2px;
}
.node-meter {
  margin-top: 4px;
}
/* DSP 方块上的开关行（开关小、贴左侧，不抢标题） */
/* DSP 方块的启用状态小字（开关在悬浮弹窗里，方块只显示状态） */
.node-state {
  margin-top: 2px;
  display: flex;
  align-items: center;
  gap: 5px;
  font-size: 11px;
  opacity: 0.85;
}
.state-dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  flex: none;
}
.state-dot.on {
  background: #18a058;
  box-shadow: 0 0 4px rgba(24, 160, 88, 0.7);
}
.state-dot.off {
  background: #8a8a8a;
}
/* DSP 的标签统一紫色（n-tag 的 warning 橙和方块配色不搭） */
.tag-dsp {
  background-color: rgba(156, 108, 236, 0.3) !important;
  color: #cfa9f9 !important;
}
/* 画布内悬浮属性面板：点击节点/连线时在目标旁弹出 */
.canvas-pop {
  position: absolute;
  z-index: 6;
  box-sizing: border-box;
  background: #232330;
  border: 1px solid rgba(156, 108, 236, 0.35);
  border-radius: 8px;
  overflow-y: auto; /* maxHeight 由 popStyle 按画布剩余高度给，装不下内部滚 */
  box-shadow: 0 6px 24px rgba(0, 0, 0, 0.5);
  padding: 10px 12px;
  font-size: 12px;
  max-height: calc(100% - 16px);
  overflow-y: auto;
}
.canvas-pop-head {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 8px;
}
.canvas-pop-title {
  flex: 1;
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.canvas-pop-row {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 6px;
}
.node-vol :deep(.n-slider) {
  flex: 1;
}
.node-vol-pct {
  font-size: 11px;
  opacity: 0.75;
  min-width: 34px;
  text-align: right;
}
.node-vol-icon {
  font-size: 11px;
  opacity: 0.8;
}
.node-close {
  opacity: 0.45;
  font-size: 14px;
  line-height: 1;
  cursor: pointer;
}
.node-close:hover {
  opacity: 1;
  color: #d03050;
}
.terminal {
  position: absolute;
  box-sizing: border-box;
  width: 16px;
  height: 16px;
  border-radius: 50%;
  border: 2px solid #4b9cd3;
  /* 实心：空心环会把从环下经过的连线和箭头尖「漏」出来，看起来像线穿过了圆环 */
  background: #4b9cd3;
  pointer-events: none; /* 命中交给 .term-zone */
  z-index: 2;
}
/* 节点悬停时端子整体亮一圈，提示这里可以拉线 */
.node:hover .terminal {
  box-shadow: 0 0 0 3px rgba(75, 156, 211, 0.25);
}
/* 端子命中区：整条边 22px 宽都能按下，不用瞄准小圆点 */
.term-zone {
  position: absolute;
  top: 2px;
  bottom: 2px;
  width: 22px;
  cursor: crosshair;
  touch-action: none;
  z-index: 3;
}
.term-zone.zone-in {
  left: -11px;
}
.term-zone.zone-out {
  right: -11px;
}
/* 两种颜色区分输入/输出端子（实心：边框和填充同色） */
.terminal.t-out {
  border-color: #4b9cd3; /* 输出＝蓝 */
  background: #4b9cd3;
}
.terminal.t-in {
  border-color: #18a058; /* 输入＝绿 */
  background: #18a058;
}
/* 透明命中环：视觉上还是个 16px 的小点，实际点按范围约 32px */
.terminal::after {
  content: "";
  position: absolute;
  inset: -8px;
  border-radius: 50%;
}
/* 拖线时：能接的端子亮黄圈，不能接的（同类型）变暗 */
.canvas.wiring .terminal.compat {
  box-shadow: 0 0 0 5px rgba(240, 160, 32, 0.45);
}
.canvas.wiring .terminal.incompat {
  opacity: 0.18;
  cursor: not-allowed;
}
.canvas.wiring {
  cursor: crosshair;
}
.legend {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  opacity: 0.75;
}
.legend-dot {
  width: 11px;
  height: 11px;
  border-radius: 50%;
  border: 2px solid #4b9cd3;
  display: inline-block;
}
.legend-dot.t-in {
  border-color: #18a058;
  background: #18a058; /* 端子已改实心，图例跟着实心 */
}
.legend-dot.t-out {
  border-color: #4b9cd3;
  background: #4b9cd3;
}
.legend-dot.compat {
  border-color: transparent;
  box-shadow: 0 0 0 4px rgba(240, 160, 32, 0.45);
}
.drag-ghost {
  position: fixed;
  z-index: 3000;
  pointer-events: none;
  padding: 3px 8px;
  border-radius: 6px;
  background: rgba(32, 128, 240, 0.9);
  color: #fff;
  font-size: 12px;
  white-space: nowrap;
}
.ctx-menu {
  position: fixed;
  z-index: 4000;
  min-width: 196px;
  padding: 4px;
  border: 1px solid rgba(255, 255, 255, 0.18);
  border-radius: 8px;
  background: #26262b;
  color: #e8e8ea;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.5);
  font-size: 12px;
}
.ctx-title {
  padding: 6px 10px 8px;
  opacity: 0.6;
  border-bottom: 1px solid rgba(255, 255, 255, 0.1);
  margin-bottom: 4px;
  max-width: 260px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ctx-item {
  padding: 6px 10px;
  border-radius: 6px;
  cursor: pointer;
}
.ctx-item:hover {
  background: rgba(255, 255, 255, 0.1);
}
.ctx-item.danger {
  color: #ff7875;
}
.ctx-item.danger:hover {
  background: rgba(208, 48, 80, 0.18);
}
</style>
