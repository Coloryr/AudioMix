<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { listen } from "@tauri-apps/api/event";
import { NAlert, NButton, NCard, NSwitch, NText, useMessage } from "naive-ui";
import {
  api,
  DeviceInfo,
  nodeKey,
  type DspNode,
  type NodePos,
  type Processor,
  type Sink,
  type Source,
  type UsbIpCableStatus,
  type UsbIpStatus,
} from "../api";
import { useApp, genId } from "../store";
import DefaultsCard from "../components/mixer/DefaultsCard.vue";
import DevicePalette from "../components/mixer/DevicePalette.vue";
import CanvasWires from "../components/mixer/CanvasWires.vue";
import CanvasNodeCard from "../components/mixer/CanvasNodeCard.vue";
import NodePopover from "../components/mixer/NodePopover.vue";
import {
  DSP_ADD_OPTIONS,
  DSP_META,
  GNode,
  NodeKind,
  PaletteItem,
  Wire,
  dspSubtitle,
  makeDspNode,
  termPos,
} from "../components/mixer/types";

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
  if (w === canvasSize.value.w && h === canvasSize.value.h) return;
  // 布局存的是归一化坐标，尺寸变化时**不做**重归一化：启动期画布会先量到
  // 兜底高度、再被 fitCanvas 定到真实高度，旧逻辑按「像素位置不变」换算并写回，
  // 每次开软件坐标都被缩一截、往左上漂（越开越漂）。归一化坐标直接按当前画布
  // 解释：同样窗口每次打开位置一致，窗口大小变化时布局等比跟随。
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
// floor 而不是 round：offset/below 都是实测小数，四舍五入会让「画布 + 上下留白」
// 比页签容器高出最多半像素，scrollHeight 向上取整后就有了 1px 滚动量 ——
// 右边于是常驻一条滚动条。向下取整保证内容总高永不超出可视区。
const clampCanvas = (h: number) => Math.max(240, Math.min(Math.floor(h), 1600));
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

// 白点背景挂在 .canvas 上、不随世界层平移；把平移量同步进 background-position，
// 点阵才跟世界一起动（否则平移时节点动了、背景点不动，观感是「背景在反方向滑」）
const canvasStyle = computed(() => ({
  height: `${canvasH.value}px`,
  backgroundPosition: `${pan.value.x}px ${pan.value.y}px`,
}));

// ---------- 节点模型 ----------
// NodeKind / GNode / PaletteItem / Wire / DSP 常量等移到 components/mixer/types.ts（与子组件共用）

const layout = ref<Record<string, NodePos>>({});
/** 布局是否已从后端加载完（加载完之前不剪掉未知节点的坐标） */
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

const DEFAULT_COL: Record<NodeKind, number> = {
  input: 0.03,
  loopback: 0.03,
  cable_play: 0.36,
  cable_rec: 0.36,
  dsp: 0.36,
  output: 0.68,
};

function defaultPos(col: number, index: number): NodePos {
  const { w, h } = canvasSize.value;
  const rowsPerCol = Math.max(1, Math.floor((h - 30) / (NODE_H + 24)));
  const row = index % rowsPerCol;
  const extraCol = Math.floor(index / rowsPerCol) * (NODE_W + 40);
  return [
    Math.min(w - NODE_W - 16, Math.max(8, col * w + extraCol)),
    Math.max(8, h * 0.05 + row * (NODE_H + 24)),
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
      return [clampPos(stored[0], true), clampPos(stored[1], false)];
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
        ? `系统播放端（扬声器）\n${lineCopyOf(cs)} · ${lineStateOf(cs)}`
        : s.enabled
          ? kind === "loopback"
            ? "该系统输出正在播放的声音"
            : "录入设备"
          : "已停用",
      sourceId: s.id,
      deviceId: kind === "loopback" ? s.device_id : undefined,
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
        ? `系统录音端（麦克风）\n${lineCopyOf(cs)} · ${lineStateOf(cs)}`
        : "输出设备",
      sinkId: k.id,
      deviceId: kind === "output" ? k.device_id : undefined,
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

// 端子函数（termPos / termStyle / termTitle）与连线曲线（wirePath / previewWirePath）已移到 components/mixer/types.ts
const selectedRoute = ref<string | null>(null);
const selectedNode = ref<string | null>(null);

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

// 连线曲线（wirePath / previewWirePath）已移到 components/mixer/types.ts

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
    minX = Math.min(minX, n.x);
    minY = Math.min(minY, n.y);
    maxX = Math.max(maxX, n.x + NODE_W);
    maxY = Math.max(maxY, n.y + NODE_H);
  }
  // 平移量取整：世界层带 will-change:transform 被提升为合成层，
  // 非整数 translate 会让整层文字落在半像素上，看起来发虚
  pan.value = {
    x: clampPan(Math.round(w / 2 - (minX + maxX) / 2), w),
    y: clampPan(Math.round(h / 2 - (minY + maxY) / 2), h),
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
      // 布局直接存像素：窗口缩放不影响节点位置
      next[k] = [px, 16 + i * scale];
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

/**
 * 节点坐标（像素）允许范围：比可视区外扩一圈（-1x..2x 尺寸）。平移画布后可视区
 * 对应的往往是画布矩形之外的负坐标区域，只允许画布内时节点一到边界就拖不动了。
 * 上限要与后端 set_mixer_layout 的 clamp 保持同一量级。
 */
function clampPos(v: number, isX: boolean) {
  if (!Number.isFinite(v)) return 0;
  const lim = (isX ? canvasSize.value.w : canvasSize.value.h) || 1;
  return Math.max(-lim, Math.min(2 * lim, v));
}

/** 只保留「两个有限数字」的布局项；坏数据（null/短数组/NaN）直接丢掉 */
function sanitizeLayout(src: Record<string, unknown> | null | undefined): Record<string, NodePos> {
  const out: Record<string, NodePos> = {};
  for (const [key, value] of Object.entries(src ?? {})) {
    if (!Array.isArray(value) || value.length < 2) continue;
    const x = Number(value[0]);
    const y = Number(value[1]);
    if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
    out[key] = [clampPos(x, true), clampPos(y, false)];
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
      [d.key]: [clampPos(p.x - d.dx, true), clampPos(p.y - d.dy, false)],
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
  // 设备面板：落在画布内就放到落点，否则放到默认位置（点击即添加）。
  // 落点要换算成世界坐标（扣掉平移量），否则平移后放置位置会偏一条平移量
  const rect = canvasEl.value?.getBoundingClientRect();
  const inside =
    !!rect &&
    e.clientX >= rect.left &&
    e.clientX <= rect.right &&
    e.clientY >= rect.top &&
    e.clientY <= rect.bottom;
  await addFromPalette(
    d.item,
    inside && rect ? { x: e.clientX - rect.left - pan.value.x, y: e.clientY - rect.top - pan.value.y } : undefined,
  );
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
 * 1) 系统回声 → 它**自己正在抓取的那个**输出设备：抓到的就是该设备正在播放的声音，送回去立刻回授啸叫；
 *    接到**其他**声卡的输出是允许的（两块设备之间没有回授路径）。
 * 2) 同一条线路的「输入 → 输出」且该线路已开启拷贝：拷贝 + 再接一圈 = 自激。
 */
function pairForbidden(a: GNode, aSide: "in" | "out", b: GNode, bSide: "in" | "out"): boolean {
  const out = aSide === "out" ? a : b;
  const sink = aSide === "out" ? b : a;
  void bSide;
  if (out.kind === "loopback" && sink.kind === "output") {
    return out.deviceId !== undefined && out.deviceId === sink.deviceId;
  }
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
        : "系统回声不能接回它正在抓取的那个输出设备 —— 会形成回授啸叫",
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
    dx: p.x - node.x,
    dy: p.y - node.y,
    moved: false,
  };
  attachWindowDrag();
}

/** 上一次节点拖动是否真的挪动了位置：pointerup 之后紧跟着的 click 要据此吞掉 */
let nodeDragMoved = false;

/** 点击节点 = 选中并弹出设置浮窗；但拖拽松手补发的 click 不算点击。
 *  增益/开关/延迟三种方块的控制直接在方块上，不弹浮窗 */
function onNodeClick(node: GNode) {
  if (nodeDragMoved) {
    nodeDragMoved = false;
    return;
  }
  // 延迟测试模式：点击 = 选起点/终点，不弹属性面板
  if (latencyMode.value) {
    pickLatencyNode(node);
    return;
  }
  if (hasInlineDspControl(node)) return;
  selectedNode.value = node.key;
  selectedRoute.value = null;
}

// ---------- 延迟测试模式：右键画布进入 → 依次点「源节点」「输出节点」→ 开始测试 ----------
const latencyMode = ref(false);
/** 已点选的节点 key（最多两个：一个带 sourceId 的源、一个带 sinkId 的输出） */
const latencyPicks = ref<string[]>([]);
const latencyMeasuring = ref(false);
/** 测量结果（null = 还没测，浮条显示选择引导） */
const latencyResult = ref<{ ok: boolean; text: string } | null>(null);

function enterLatencyMode() {
  latencyMode.value = true;
  latencyPicks.value = [];
  latencyMeasuring.value = false;
  latencyResult.value = null;
}

function exitLatencyMode() {
  latencyMode.value = false;
  latencyPicks.value = [];
  latencyMeasuring.value = false;
  latencyResult.value = null;
}

/** 测试模式下点节点：已选的再点取消；第三个点击替换终点 */
function pickLatencyNode(node: GNode) {
  if (latencyMeasuring.value) return;
  if (latencyPicks.value.includes(node.key)) {
    latencyPicks.value = latencyPicks.value.filter((k) => k !== node.key);
    return;
  }
  if (!node.sourceId && !node.sinkId) {
    message.warning("起点/终点需要设备节点（源或输出），DSP 方块不能选");
    return;
  }
  if (latencyPicks.value.length >= 2) latencyPicks.value = [latencyPicks.value[1]];
  latencyPicks.value = [...latencyPicks.value, node.key];
}

const latencyPickedNodes = computed(() =>
  latencyPicks.value.map((k) => nodeByKey.value.get(k)).filter((n): n is GNode => !!n),
);
/** 两端齐了：一个源节点 + 一个输出节点 */
const latencyReady = computed(
  () =>
    latencyPickedNodes.value.some((n) => n.sourceId) && latencyPickedNodes.value.some((n) => n.sinkId),
);
const latencyHint = computed(() => {
  const hasSrc = latencyPickedNodes.value.some((n) => n.sourceId);
  const hasDst = latencyPickedNodes.value.some((n) => n.sinkId);
  if (hasSrc && hasDst) return "已选好两端";
  if (!latencyPickedNodes.value.length || (!hasSrc && !hasDst))
    return "延迟测试：先点一个源节点（线路 / 输入 / 系统回声）";
  return hasSrc ? "再点一个输出节点（耳机 / 扬声器 / 线路输入）" : "先点一个源节点（线路 / 输入 / 系统回声）";
});

async function startLatencyTest() {
  const src = latencyPickedNodes.value.find((n) => n.sourceId);
  const dst = latencyPickedNodes.value.find((n) => n.sinkId);
  if (!src?.sourceId || !dst?.sinkId) return;
  latencyMeasuring.value = true;
  latencyResult.value = null;
  try {
    const ms = await api.measureNodesLatency(src.sourceId, dst.sinkId);
    latencyResult.value = { ok: true, text: `${src.title} → ${dst.title} ≈ ${ms.toFixed(1)} ms` };
  } catch (e) {
    latencyResult.value = { ok: false, text: String(e) };
  } finally {
    latencyMeasuring.value = false;
  }
}

/** 该节点的 DSP 控件是否已内联到方块上（悬浮窗不再弹出） */
function hasInlineDspControl(node: GNode): boolean {
  if (!node.processorId) return false;
  const p = app.graph.processors.find((x) => x.id === node.processorId);
  return !!p && (p.type === "gain" || p.type === "switch" || p.type === "delay");
}

/** 方块内联 DSP 控件对应的处理器 */
function dspOf(node: GNode): DspNode | null {
  if (!node.processorId) return null;
  return app.graph.processors.find((x) => x.id === node.processorId) ?? null;
}

/** 方块滑杆/开关改动后下发引擎（后端 set_processor_params 内部持久化，无需再 save） */
function touchProcessor(procId: string) {
  const p = app.graph.processors.find((x) => x.id === procId);
  if (!p) return;
  api.setProcessorParams(p.id, { ...p }).catch((e) => message.error(String(e)));
}

function onDspParam(node: GNode, key: string, v: number) {
  const p = dspOf(node);
  if (!p) return;
  (p as unknown as Record<string, unknown>)[key] = v;
  touchProcessor(p.id);
}

function onDspToggle(node: GNode, enabled: boolean) {
  const p = dspOf(node);
  if (!p) return;
  p.enabled = enabled;
  touchProcessor(p.id);
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
  return { x: n.x + NODE_W / 2, y: n.y + NODE_H / 2 };
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
        : "系统回声不能接回它正在抓取的那个输出设备 —— 送回去会立刻啸叫（接其他声卡的输出不受限）",
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
// PaletteKind / PaletteItem 已移到 components/mixer/types.ts

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
          clampPos(dropPoint.x - NODE_W / 2, true),
          clampPos(dropPoint.y - NODE_H / 2, false),
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
        clampPos(dropPoint.x - NODE_W / 2, true),
        clampPos(dropPoint.y - NODE_H / 2, false),
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
    const fitsRight = nd.x + pan.value.x + NODE_W + 12 + POP_W <= w - 8;
    left = fitsRight ? nd.x + pan.value.x + NODE_W + 12 : Math.max(8, nd.x + pan.value.x - POP_W - 12);
    top = nd.y + pan.value.y;
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
 * 节点位置完全由用户掌控：只在拖动/拖入/移除/点击「自动排序」时改变并写盘。
 * 布局直接存像素坐标（相对画布左上角），窗口/画布缩放不影响节点位置。
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

// DspType / DSP_META / DSP_ADD_OPTIONS / DSP_GEQ_BANDS / ParamDef / DSP_PARAMS /
// sliderVal / sliderCommit / sliderTip / makeDspNode / 频响曲线（dspCurveOf）已移到
// components/mixer/types.ts；DSP 参数面板整体在 components/mixer/DspParamPanel.vue，
// 悬浮弹窗在 components/mixer/NodePopover.vue —— 这里只保留画布几何与交互。

const level = (id?: string) => (id ? Math.min(1, app.levels[id] ?? 0) : 0);
/** 频谱分析关闭时为 undefined，节点不渲染频谱条 */
const spectrumOf = (id?: string) => (id ? app.spectra[id] : undefined);

/** 频谱开关：写设置（后端 set_fft_enabled 即时生效），失败回滚 */
async function toggleSpectrum(on: boolean) {
  const old = !on;
  try {
    await api.updateSettings(app.settings);
  } catch (e) {
    app.settings.fft_enabled = old;
    message.error(String(e));
  }
}

// ---------- 系统音量（Windows 端点音量，影响该设备上所有声音） ----------
const deviceVolumes = ref<Record<string, number>>({});
/** 拖动中的待写队列：device_id -> 定时器 */
const pendingVolume = new Map<string, { timer: number; value: number }>();

/** 取某个 sink 节点对应的设备 id */
function sinkDeviceId(node: GNode): string | undefined {
  if (!node.sinkId) return undefined;
  return app.graph.sinks.find((s) => s.id === node.sinkId)?.device_id;
}

/** 系统音量条对应的设备 id。线路端点（usbip://）的端点音量不经过虚拟线路的
 *  数据通路（调了没效果，调「混音」才有效），所以线路节点不显示系统音量条 */
function volumeIdOf(node: GNode): string | undefined {
  const id = sinkDeviceId(node);
  if (!id || id.startsWith("usbip://")) return undefined;
  return id;
}

const volumeOf = (deviceId?: string) => (deviceId ? (deviceVolumes.value[deviceId] ?? 1) : 1);

async function refreshDeviceVolumes() {
  const ids = new Set<string>();
  for (const node of nodes.value) {
    const id = volumeIdOf(node);
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

// ---------- 右键菜单 / 键盘删除 ----------
const ctxMenu = ref<{
  x: number;
  y: number;
  kind: "wire" | "node" | "canvas";
  id: string;
  title: string;
} | null>(null);

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
  // 右键只弹右键菜单，不选中节点 —— 否则会连带弹出设置浮窗
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

/** 右键画布空白：延迟测试入口 */
function openCanvasMenu(e: MouseEvent) {
  ctxMenu.value = {
    x: Math.min(e.clientX, window.innerWidth - 220),
    y: Math.min(e.clientY, window.innerHeight - 130),
    kind: "canvas",
    id: "",
    title: "接线画布",
  };
}

function openCanvasMenuDeferred(e: MouseEvent) {
  // pointerdown（关闭旧菜单）先于 contextmenu 触发，用宏任务确保菜单不被立刻关掉
  setTimeout(() => openCanvasMenu(e), 0);
}

function menuLatencyTest() {
  closeMenu();
  enterLatencyMode();
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
  if (e.key === "Escape" && latencyMode.value) {
    exitLatencyMode();
    return;
  }
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
    // 旧版配置存的是 0..1 归一化坐标（会随窗口缩放挪动），一次性换算成像素：
    // 判据是所有坐标都 ≤ 1.5 —— 像素布局下节点不可能全部挤在画布左上 1px 内
    const allNorm = Object.values(loaded).every(([x, y]) => x <= 1.5 && y <= 1.5);
    if (allNorm && loaded) {
      const { w, h } = canvasSize.value;
      for (const k of Object.keys(loaded)) {
        loaded[k] = [loaded[k][0] * w, loaded[k][1] * h];
      }
    }
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
    <DefaultsCard :cables="cables" />

    <div class="canvas-grid">
      <!-- 左栏：设备 + DSP 面板（拖到画布上生成节点） -->
      <DevicePalette :devices="paletteDevices" :dsp="paletteDsp" :is-existing="paletteExisting"
        @drag-start="(item, e) => startPaletteDrag(e, item)" />

      <!-- 画布 -->
      <n-card size="small" title="接线画布" class="canvas-card">
        <template #header-extra>
          <div style="display: flex; align-items: center; gap: 12px; font-size: 12px">
            <n-switch v-model:value="app.settings.fft_enabled" size="small" @update:value="toggleSpectrum">
              <template #checked>频谱 开</template>
<template #unchecked>频谱 关</template>
</n-switch>
            <span class="legend"><i class="legend-dot t-out" />输出端子（信号流出）</span>
            <span class="legend"><i class="legend-dot t-in" />输入端子（信号流入）</span>
            <span class="legend"><i class="legend-dot compat" />拖线时可接</span>
            <n-button size="tiny" quaternary @click="centerView" title="把所有节点居中到画布中间">居中</n-button>
            <n-button size="tiny" quaternary @click="autoArrange" title="按信号流向自动分层排列节点">自动排序</n-button>
          </div>
        </template>
        <div ref="canvasEl" class="canvas" :class="{ wiring: !!wireSource, panning: drag?.kind === 'pan' }"
          :style="canvasStyle" @pointerdown.self="startCanvasPan">
          <!-- 世界层：节点/连线都在这个世界坐标系里，拖空白处平移整个世界。
             它 inset:0 铺满画布，空白处的 pointerdown 落在这层而不是 .canvas 上，
             平移入口必须挂在这里（挂 .canvas 上的 .self 永远不命中，画布就拖不动） -->
          <div class="canvas-world" :style="{ transform: `translate(${pan.x}px, ${pan.y}px)` }"
            @pointerdown.self="startCanvasPan" @contextmenu.self.prevent="openCanvasMenuDeferred">
            <!-- 连线层：SVG 画在 CanvasWires 里（比画布外扩一圈，viewBox 平移到负区） -->
            <CanvasWires :wires="wires" :selected-id="selectedRoute" :pending="pendingWire" :guides="nodeGuides"
              :size="canvasSize" @select="(id) => { selectedRoute = id; selectedNode = null; }"
              @menu="openWireMenuDeferred" />

            <!-- 拖动设备时：虚线落位框 -->
            <div v-if="dropPreview" class="drop-preview" :style="{
              left: dropPreview.x + 'px',
              top: dropPreview.y + 'px',
              width: NODE_W + 'px',
              height: NODE_H + 'px',
            }">
              <span>{{ dropPreview.title }}</span>
            </div>

            <CanvasNodeCard v-for="node in nodes" :key="node.key" :node="node"
              :selected="selectedNode === node.key || latencyPicks.includes(node.key)" :wiring="!!wireSource"
              :meter="Math.max(level(node.sourceId), level(node.sinkId), level(node.processorId))"
              :spectrum="spectrumOf(node.sourceId ?? node.sinkId)"
              :badge="node.processorId ? procStateBadge(node.processorId) : null" :dsp="dspOf(node)"
              :volume-id="volumeIdOf(node)" :volume="volumeOf(volumeIdOf(node))"
              :term-state-of="(side) => termState(node, side)"
              @drag-start="(e) => startNodeDrag(e, node)" @activate="onNodeClick(node)"
              @menu="(e) => openNodeMenuDeferred(e, node)" @remove="removeNode(node)"
              @wire-start="(e, side) => startWireDrag(e, node, side)"
              @volume-change="(v) => { const id = volumeIdOf(node); if (id) void onDeviceVolume(id, v); }"
              @dsp-param="(key, v) => onDspParam(node, key, v)" @dsp-toggle="(v) => onDspToggle(node, v)" />
          </div><!-- /canvas-world -->

          <n-text v-if="!nodes.length" depth="3"
            style="position: absolute; left: 50%; top: 46%; transform: translate(-50%, -50%); font-size: 13px">
            从左侧「设备」拖入设备开始接线
          </n-text>

          <!-- 点击节点 / 连线后的悬浮属性面板（定位与关闭时机在父组件，内容在子组件） -->
          <NodePopover v-if="selectedRouteData || selectedNodeData" :node="selectedNodeData" :route="selectedRouteData"
            :route-title="popRouteTitle"
            :device-volume="selectedNodeData?.sinkId ? volumeOf(sinkDeviceId(selectedNodeData)) : 1" :style="popStyle"
            @close="closePop" @disconnect="disconnect" @volume-change="(id, v) => onDeviceVolume(id, v)"
            @remove="removeNode" />

          <!-- 延迟测试模式浮条：置顶居中、absolute 不参与布局（不影响 fitCanvas） -->
          <div v-if="latencyMode" class="latency-bar" @pointerdown.stop @click.stop @contextmenu.prevent.stop>
            <span v-if="latencyMeasuring">测量中，约 3 秒（会听到轻微脉冲声）…</span>
            <template v-else>
              <span>{{ latencyHint }}</span>
              <span v-for="n in latencyPickedNodes" :key="n.key" class="latency-pick">{{ n.title }}</span>
              <span v-if="latencyResult" :class="latencyResult.ok ? 'latency-val' : 'latency-err'"
                :title="latencyResult.text">{{ latencyResult.text }}</span>
            </template>
            <n-button size="tiny" type="primary" :disabled="!latencyReady || latencyMeasuring"
              @click="startLatencyTest">
              开始测试
            </n-button>
            <n-button size="tiny" quaternary :disabled="latencyMeasuring" @click="exitLatencyMode">退出</n-button>
          </div>
        </div>
      </n-card>
    </div>

    <div v-if="notice" ref="noticeEl" class="notice-wrap">
      <n-alert :type="notice.type">{{ notice.text }}</n-alert>
    </div>

    <!-- 拖动设备时的跟随提示 -->
    <div v-if="ghost" class="drag-ghost" :style="{ left: ghost.cx + 12 + 'px', top: ghost.cy + 12 + 'px' }">
      {{ ghost.item.title }}
    </div>

    <!-- 右键菜单：连线和节点都能在这里删 -->
    <div v-if="ctxMenu" class="ctx-menu" :style="{ left: ctxMenu.x + 'px', top: ctxMenu.y + 'px' }" @pointerdown.stop
      @contextmenu.prevent>
      <div class="ctx-title">{{ ctxMenu.title }}</div>
      <template v-if="ctxMenu.kind === 'canvas'">
        <div class="ctx-item" @click="menuLatencyTest">延迟测试（选两个节点）</div>
      </template>
      <template v-else-if="ctxMenu.kind === 'wire'">
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

/* 网格：左栏绝对定位挂在第一列上（不参与行高计算），行高只由画布卡决定 */
.canvas-grid {
  position: relative;
  display: grid;
  grid-template-columns: 262px 1fr;
  gap: 14px;
}

.canvas-card {
  grid-column: 2;
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
  cursor: grab;
  /* 拖空白处平移画布 */
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

/* 拉线中：光标提示（端子的高亮/变暗样式在 CanvasNodeCard 里） */
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
  border: 2px solid var(--accent);
  display: inline-block;
}

.legend-dot.t-in {
  border-color: #18a058;
  background: #18a058;
  /* 端子已改实心，图例跟着实心 */
}

.legend-dot.t-out {
  border-color: var(--accent);
  background: var(--accent);
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

/* 延迟测试模式浮条：画布内置顶居中；absolute 不参与布局，不影响 fitCanvas 的高度调平 */
.latency-bar {
  position: absolute;
  top: 10px;
  left: 50%;
  transform: translateX(-50%);
  z-index: 7;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 12px;
  border: 1px solid var(--border-strong);
  border-radius: 8px;
  background: var(--surface-2);
  color: var(--text);
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.4);
  font-size: 12px;
  white-space: nowrap;
}

.latency-pick {
  max-width: 180px;
  overflow: hidden;
  text-overflow: ellipsis;
  padding: 1px 8px;
  border-radius: 10px;
  background: var(--hover-chip);
}

.latency-val {
  color: var(--accent);
  font-variant-numeric: tabular-nums;
}

.latency-err {
  max-width: 300px;
  overflow: hidden;
  text-overflow: ellipsis;
  color: var(--err-text);
  font-size: 11px;
}

.ctx-menu {
  position: fixed;
  z-index: 4000;
  min-width: 196px;
  padding: 4px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--surface-2);
  color: var(--text);
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.5);
  font-size: 12px;
}

.ctx-title {
  padding: 6px 10px 8px;
  opacity: 0.6;
  border-bottom: 1px solid var(--border-weak);
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
  background: var(--hover-soft);
}

.ctx-item.danger {
  color: var(--err-text);
}

.ctx-item.danger:hover {
  background: rgba(208, 48, 80, 0.18);
}
</style>
