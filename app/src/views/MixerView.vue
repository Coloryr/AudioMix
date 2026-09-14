<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { listen } from "@tauri-apps/api/event";
import {
  NAlert,
  NButton,
  NCard,
  NDivider,
  NSelect,
  NSlider,
  NSwitch,
  NTag,
  NText,
  useMessage,
} from "naive-ui";
import {
  api,
  dbToGain,
  gainToDb,
  nodeKey,
  type DeviceInfo,
  type NodePos,
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
// 画布高度原来是硬算的（100vh - 常数），底部提示条（没有线路 / 服务器没跑）一出现
// 就会把整页顶出一屏、提示被挤到滚动条下面；常数本身也不准，会留下一块空白。
// 改成实测：先按经验公式给初值，再量「整页高度 vs 页签可用高度」，把差值补到画布上，
// 这样有没有提示条都刚好占满一屏，既不用滚动也不留空档。
const pageEl = ref<HTMLElement | null>(null);
const clampCanvas = (h: number) => Math.max(240, Math.min(Math.round(h), 1600));
/** 画布高度（px） */
const canvasH = ref(clampCanvas(window.innerHeight - 392));
let fitting = false;
/** 上一次「设成多少画布高 / 当时页面多高」，用来发现画布已经不是高度的决定因素 */
let lastFit = { canvas: -1, page: -1 };
let fitStalled = false;

/** 从某元素往上找页签那个滚动容器 */
function findScroller(start: HTMLElement | null): HTMLElement | null {
  let el = start?.parentElement ?? null;
  while (el) {
    const oy = getComputedStyle(el).overflowY;
    if (oy === "auto" || oy === "scroll") return el;
    el = el.parentElement;
  }
  return null;
}

function fitCanvas() {
  const page = pageEl.value;
  const canvas = canvasEl.value;
  if (!page || !canvas || fitting) return;
  const scroller = findScroller(page);
  if (!scroller) return;
  const cs = getComputedStyle(scroller);
  const avail =
    scroller.clientHeight - (parseFloat(cs.paddingTop) || 0) - (parseFloat(cs.paddingBottom) || 0);
  if (avail < 200) return; // 页签没显示或窗口太矮，别乱动
  const pageH = page.offsetHeight;
  const delta = pageH - avail; // >0 溢出、<0 还空着（页面自带 padding-bottom，底部间距也算在里面）
  if (Math.abs(delta) < 2) {
    fitStalled = false;
    return;
  }
  // 上一次调整没让页面高度变化 → 页面高度不由画布决定（设备面板那一列更高），再缩也没用
  if (lastFit.canvas === canvasH.value && lastFit.page === pageH) fitStalled = true;
  else fitStalled = false;
  if (fitStalled) return;
  const next = clampCanvas(canvas.clientHeight - delta);
  if (next === canvasH.value) return;
  lastFit = { canvas: next, page: pageH };
  fitting = true;
  canvasH.value = next;
  requestAnimationFrame(() => {
    fitting = false;
  });
}

const canvasStyle = computed(() => ({ height: `${canvasH.value}px` }));

// ---------- 节点模型 ----------
type NodeKind = "input" | "loopback" | "output" | "cable_play" | "cable_rec";

interface GNode {
  key: string;
  kind: NodeKind;
  title: string;
  subtitle: string;
  /** 右端子（信号流出）对应的 Source */
  sourceId?: string;
  /** 左端子（信号流入）对应的 Sink */
  sinkId?: string;
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
};

const DEFAULT_COL: Record<NodeKind, number> = {
  input: 0.03,
  loopback: 0.03,
  cable_play: 0.36,
  cable_rec: 0.36,
  output: 0.68,
};

function defaultPos(kind: NodeKind, index: number): NodePos {
  const rowsPerCol = Math.max(1, Math.floor((canvasSize.value.h - 30) / (NODE_H + 24)));
  const row = index % rowsPerCol;
  const extraCol = Math.floor(index / rowsPerCol) * 0.1;
  return [
    Math.min(0.76, DEFAULT_COL[kind] + extraCol),
    0.05 + (row * (NODE_H + 24)) / canvasSize.value.h,
  ];
}

const nodes = computed<GNode[]>(() => {
  const list: GNode[] = [];
  const counters: Record<string, number> = { input: 0, loopback: 0, output: 0, cable_play: 0, cable_rec: 0 };
  const pos = (key: string, kind: NodeKind): NodePos => {
    const stored = layout.value[key];
    // 坏数据（null/短数组/NaN）当作没有，回落到默认排布
    if (Array.isArray(stored) && stored.length >= 2 && stored.every((v) => Number.isFinite(v))) {
      return [clamp01(stored[0]), clamp01(stored[1])];
    }
    return defaultPos(kind, counters[kind]++);
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
  return list;
});

const nodeByKey = computed(() => new Map(nodes.value.map((n) => [n.key, n])));
const nodeBySource = computed(() => new Map(nodes.value.filter((n) => n.sourceId).map((n) => [n.sourceId!, n])));
const nodeBySink = computed(() => new Map(nodes.value.filter((n) => n.sinkId).map((n) => [n.sinkId!, n])));

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
    const a = nodeBySource.value.get(r.source_id);
    const b = nodeBySink.value.get(r.sink_id);
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
 * 连线：控制点朝两端子的**出线方向**外扩（端子朝左就往左出线、朝右就往右），
 * 并夹在画布内，避免曲线被 overflow 裁掉。
 */
function wirePath(from: { x: number; y: number; dir: number }, to: { x: number; y: number; dir?: number }) {
  const { w } = canvasSize.value;
  const d = Math.min(90, Math.max(36, Math.abs(to.x - from.x) * 0.4));
  const c1 = Math.max(6, Math.min(w - 6, from.x + from.dir * d));
  // 拉线预览的终点是鼠标位置、没有方向，按 0 处理（否则 undefined * d = NaN，路径直接画不出来）
  const td = to.dir ?? 0;
  const c2 = Math.max(6, Math.min(w - 6, to.x + td * d));
  // 箭头停在端子圆边上（圆半径 8 + 2px 间隙），不再戳进圆里
  const sx = from.x + from.dir * 10;
  const ex = to.x + td * 10;
  return `M ${sx} ${from.y} C ${c1} ${from.y}, ${c2} ${to.y}, ${ex} ${to.y}`;
}

// ---------- 指针拖拽（不依赖 HTML5 DnD：Tauri 在 Windows 上会拦截它） ----------
type DragState =
  | { kind: "node"; key: string; dx: number; dy: number }
  | { kind: "wire"; key: string; side: "in" | "out"; x: number; y: number; sx: number; sy: number; moved: boolean }
  | { kind: "palette"; item: PaletteItem; sx: number; sy: number; cx: number; cy: number; moved: boolean };

const drag = ref<DragState | null>(null);

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
  return { x: e.clientX - rect.left, y: e.clientY - rect.top };
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
  } else if (d.kind === "wire") {
    const p = canvasPoint(e);
    const moved = d.moved || Math.hypot(e.clientX - d.sx, e.clientY - d.sy) > 4;
    drag.value = { ...d, x: p.x, y: p.y, moved };
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
  if (d.kind === "node") {
    await persistLayout();
    return;
  }
  if (d.kind === "wire") {
    // 先精确命中端子；不然放宽为「落在某个节点上」→ 自动用它对侧的那个端子
    const term = terminalAt(e.clientX, e.clientY) ?? inferredTerminal(e.clientX, e.clientY, d.side);
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
  let ok = src.side !== which && (which === "out" ? !!node.sourceId : !!node.sinkId);
  const srcNode = nodeByKey.value.get(src.key);
  if (ok && srcNode) ok = !pairForbidden(srcNode, src.side, node, which);
  return ok ? "compat" : "incompat";
}

/** 松手落在节点（而不是那个小圆点）上时，自动选对侧端子 —— 大幅放宽连线的手感 */
function inferredTerminal(
  clientX: number,
  clientY: number,
  fromSide: "in" | "out",
): { key: string; side: "in" | "out" } | null {
  const el = document.elementFromPoint(clientX, clientY) as HTMLElement | null;
  const key = el?.closest("[data-node-key]")?.getAttribute("data-node-key");
  if (!key) return null;
  const node = nodeByKey.value.get(key);
  if (!node || key === drag.value?.key) return null;
  const want: "in" | "out" = fromSide === "out" ? "in" : "out";
  const src = drag.value?.key ? nodeByKey.value.get(drag.value.key) : undefined;
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
  const p = canvasPoint(e);
  drag.value = {
    kind: "node",
    key: node.key,
    dx: p.x - node.x * canvasSize.value.w,
    dy: p.y - node.y * canvasSize.value.h,
  };
  attachWindowDrag();
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
    // 只发有限数值，坏项丢掉（后端 layout 是 f64，收到 null 会报 invalid args）
    await api.setMixerLayout(sanitizeLayout(layout.value));
  } catch (e) {
    message.error(String(e));
  }
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
  if (!outNode?.sourceId || !inNode?.sinkId) {
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
  if (app.graph.routes.some((r) => r.source_id === outNode.sourceId && r.sink_id === inNode.sinkId)) {
    message.info("这条线已经接好了");
    return;
  }
  app.graph.routes.push({
    id: genId("route"),
    source_id: outNode.sourceId,
    sink_id: inNode.sinkId,
    gain: 1.0,
    muted: false,
  });
  await save();
}

async function disconnect(routeId: string) {
  app.graph.routes = app.graph.routes.filter((r) => r.id !== routeId);
  if (selectedRoute.value === routeId) selectedRoute.value = null;
  await save();
}

// ---------- 设备面板 ----------
type PaletteKind = "input" | "loopback" | "output" | "cable_play" | "cable_rec";
interface PaletteItem {
  kind: PaletteKind;
  title: string;
  deviceId?: string;
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
  return items;
});

function paletteKey(item: PaletteItem): string | null {
  if (!item.deviceId) return null;
  if (item.kind === "cable_play") return nodeKey.cablePlay(cableNumberOf(item.deviceId)!);
  if (item.kind === "cable_rec") return nodeKey.cableRec(cableNumberOf(item.deviceId)!);
  if (item.kind === "input") return nodeKey.input(item.deviceId);
  if (item.kind === "loopback") return nodeKey.loopback(item.deviceId);
  return nodeKey.output(item.deviceId);
}

function paletteExisting(item: PaletteItem): boolean {
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

// ---------- 参数面板 ----------
const selectedRouteData = computed(() => app.graph.routes.find((r) => r.id === selectedRoute.value) ?? null);
const selectedNodeData = computed(() => nodes.value.find((n) => n.key === selectedNode.value) ?? null);

function onGain(routeId: string, db: number) {
  const r = app.graph.routes.find((x) => x.id === routeId);
  if (!r) return;
  r.gain = dbToGain(db);
  api.setRouteGain(routeId, r.gain).catch((e) => message.error(String(e)));
}

function onMute(routeId: string, muted: boolean) {
  const r = app.graph.routes.find((x) => x.id === routeId);
  if (!r) return;
  r.muted = muted;
  api.setRouteMuted(routeId, muted).catch((e) => message.error(String(e)));
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

async function menuToggleMute() {
  const m = ctxMenu.value;
  closeMenu();
  if (m?.kind !== "wire") return;
  const r = app.graph.routes.find((x) => x.id === m.id);
  if (r) onMute(r.id, !r.muted);
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
    layout.value = sanitizeLayout(await api.getMixerLayout());
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

  <div style="display: grid; grid-template-columns: 262px 1fr; gap: 14px">
    <!-- 设备面板：按住拖到画布上生成节点（也可直接点击 = 放到默认位置） -->
    <n-card size="small" title="设备" class="dev-card">
      <n-text depth="3" style="font-size: 12px; display: block; margin-bottom: 8px">
        按住拖到右侧画布；直接点击则放到默认位置。
      </n-text>
      <div class="palette">
        <div
          v-for="(item, i) in palette"
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
        <n-text v-if="!palette.length" depth="3" style="font-size: 12px">未检测到设备</n-text>
      </div>
    </n-card>

    <!-- 画布 -->
    <n-card size="small" title="接线画布">
      <template #header-extra>
        <div style="display: flex; align-items: center; gap: 12px; font-size: 12px">
          <span class="legend"><i class="legend-dot t-out" />输出端子（信号流出）</span>
          <span class="legend"><i class="legend-dot t-in" />输入端子（信号流入）</span>
          <span class="legend"><i class="legend-dot compat" />拖线时可接</span>
        </div>
      </template>
      <div
        ref="canvasEl"
        class="canvas"
        :class="{ wiring: !!wireSource }"
        :style="canvasStyle"
        @pointerdown.self="selectedRoute = null; selectedNode = null"
      >
        <svg class="wire-layer" :width="canvasSize.w" :height="canvasSize.h">
          <defs>
            <marker
              id="wire-arrow"
              viewBox="0 0 10 10"
              refX="10"
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
              refX="10"
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
              refX="10"
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
          <path v-if="pendingWire" :d="wirePath(pendingWire.from, pendingWire.to)" class="wire pending" />
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
          @click.stop="selectedNode = node.key; selectedRoute = null"
          @contextmenu.prevent.stop="openNodeMenuDeferred($event, node)"
        >
          <div class="node-head">
            <n-tag size="tiny" :type="KIND_META[node.kind].type" :bordered="false">
              {{ KIND_META[node.kind].tag }}
            </n-tag>
            <span class="node-title">{{ node.title }}</span>
            <span class="node-close" @pointerdown.stop @click.stop="removeNode(node)">×</span>
          </div>
          <div class="node-sub">{{ node.subtitle }}</div>
          <MeterBar
            class="node-meter"
            :level="Math.max(level(node.sourceId), level(node.sinkId))"
          />

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

        <n-text
          v-if="!nodes.length"
          depth="3"
          style="position: absolute; left: 50%; top: 46%; transform: translate(-50%, -50%); font-size: 13px"
        >
          从左侧「设备」拖入设备开始接线
        </n-text>
      </div>

      <n-divider style="margin: 12px 0" />

      <!-- 连线参数 -->
      <div v-if="selectedRouteData">
        <div class="item-row">
          <n-tag size="small" :bordered="false" type="info">连线</n-tag>
          <div style="flex: 1">
            {{ wires.find((w) => w.id === selectedRouteData!.id)?.fromTitle }} →
            {{ wires.find((w) => w.id === selectedRouteData!.id)?.toTitle }}
          </div>
          <n-switch
            :value="!selectedRouteData.muted"
            size="small"
            @update:value="(v: boolean) => onMute(selectedRouteData!.id, !v)"
          />
          <n-slider
            :value="gainToDb(selectedRouteData.gain)"
            :min="-60"
            :max="6"
            :step="1"
            :format-tooltip="(v: number) => v.toFixed(0) + ' dB'"
            style="width: 220px"
            @update:value="(v: number) => onGain(selectedRouteData!.id, v)"
          />
          <n-tag size="small" :bordered="false">{{ gainToDb(selectedRouteData.gain).toFixed(0) }} dB</n-tag>
          <n-button size="tiny" quaternary type="error" @click="disconnect(selectedRouteData.id)">断开</n-button>
        </div>
      </div>
      <div v-else-if="selectedNodeData">
        <div class="item-row">
          <n-tag size="small" :bordered="false" :type="KIND_META[selectedNodeData.kind].type">
            {{ KIND_META[selectedNodeData.kind].tag }}
          </n-tag>
          <div style="flex: 1">{{ selectedNodeData.title }}</div>
          <template v-if="selectedNodeData.sourceId">
            <span class="item-sub">采集</span>
            <n-switch
              :value="sourceEnabledOf(selectedNodeData.sourceId)"
              size="small"
              @update:value="(v: boolean) => onSourceEnabled(selectedNodeData!.sourceId!, v)"
            />
          </template>
          <template v-if="selectedNodeData.sinkId">
            <span class="item-sub" style="margin-left: 12px">混音音量</span>
            <n-slider
              :value="sinkVolumeOf(selectedNodeData.sinkId)"
              :min="0"
              :max="1"
              :step="0.01"
              :format-tooltip="(v: number) => Math.round(v * 100) + '%'"
              style="width: 140px"
              @update:value="(v: number) => onSinkVolume(selectedNodeData!.sinkId!, v)"
            />
            <n-switch
              :value="sinkEnabledOf(selectedNodeData.sinkId)"
              size="small"
              @update:value="(v: boolean) => onSinkEnabled(selectedNodeData!.sinkId!, v)"
            />
            <span class="item-sub" style="margin-left: 12px">系统音量</span>
            <n-slider
              :value="volumeOf(sinkDeviceId(selectedNodeData))"
              :min="0"
              :max="1"
              :step="0.01"
              :format-tooltip="(v: number) => Math.round(v * 100) + '%'"
              style="width: 140px"
              @update:value="(v: number) => onDeviceVolume(sinkDeviceId(selectedNodeData!)!, v)"
            />
          </template>
          <n-button size="tiny" quaternary type="error" @click="removeNode(selectedNodeData)">移除节点</n-button>
        </div>
      </div>
      <n-text v-else depth="3" style="font-size: 12px">
        连线：按住节点两侧的圆点（整条边都行）拖到目标节点，或点一下圆点再点目标节点；Esc 取消。
        点一条连线可调增益/静音，右键线/节点可删除。
      </n-text>
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
      <div class="ctx-item" @click="menuToggleMute">静音 / 取消静音</div>
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
/* 设备卡片与画布等高：网格行高由画布卡（固定 px 高）决定，左卡默认 stretch 拉满；
   卡片内部变成纵向 flex，设备列表吃掉剩余高度、超出自己滚动 */
.dev-card {
  display: flex;
  flex-direction: column;
}
.dev-card :deep(.n-card__content) {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
}
.palette {
  flex: 1;
  /* 兜底：画布高度还没量出来时也保持可用 */
  min-height: 160px;
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
.canvas {
  position: relative;
  /* 高度跟随窗口（宽度本来就自适应）；实际高度由 :style 绑定，这里只是兜底 */
  height: clamp(320px, calc(100vh - 392px), 1200px);
  border: 1px dashed rgba(128, 128, 128, 0.35);
  border-radius: 8px;
  background-image: radial-gradient(rgba(128, 128, 128, 0.16) 1px, transparent 1px);
  background-size: 20px 20px;
  overflow: hidden;
  touch-action: none;
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
  border-color: #f0a020;
  box-shadow: 0 0 0 2px rgba(240, 160, 32, 0.25), 0 2px 8px rgba(0, 0, 0, 0.35);
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
  background: #1f1f23;
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
/* 两种颜色区分输入/输出端子 */
.terminal.t-out {
  border-color: #4b9cd3; /* 输出＝蓝 */
}
.terminal.t-in {
  border-color: #18a058; /* 输入＝绿 */
}
.terminal.t-out:hover {
  background: #4b9cd3;
}
.terminal.t-in:hover {
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
}
.legend-dot.t-out {
  border-color: #4b9cd3;
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
