<script setup lang="ts">
// 画布节点卡片：标签 + 标题/副标题 + 电平条 + （DSP）状态小字 + （输出）系统音量条 + 两端端子。
// 拖动/拉线/右键/移除等交互全部上抛给 MixerView（它持有拖拽引擎与画布几何）。
import { NTag, NSlider, NSwitch } from "naive-ui";
import MeterBar from "../MeterBar.vue";
import { NODE_W, NODE_H, KIND_META, termStyle, termTitle, type GNode } from "./types";
import type { DspNode } from "../../api";

defineProps<{
  node: GNode;
  selected: boolean;
  /** 正在拉线（上下文）：能接的端子亮黄圈、不能接的变暗 */
  wiring: boolean;
  /** 电平条数值（父组件按 源/汇/处理器 取最大） */
  meter: number;
  /** 20 段频谱 dB（频谱分析开启时传入，取代电平条显示；关闭时为 undefined） */
  spectrum?: number[];
  /** DSP 方块的状态小字（非 DSP 传 null 不显示） */
  badge: { text: string; cls: string } | null;
  /** 方块对应的 DSP 处理器（所有类型都在方块上放旁路开关；详细参数走悬浮窗） */
  dsp?: DspNode | null;
  /** 系统音量条对应的设备 id（Windows 端点音量；线路端点/无 sink 为 undefined 不显示） */
  volumeId?: string;
  /** 该设备的系统音量（0..1） */
  volume: number;
  /** 拉线时该端子是否可接："" | "compat" | "incompat"（父组件的 termState） */
  termStateOf: (side: "in" | "out") => string;
}>();

const emit = defineEmits<{
  (e: "drag-start", ev: PointerEvent): void;
  (e: "activate"): void;
  (e: "menu", ev: MouseEvent): void;
  (e: "remove"): void;
  (e: "wire-start", ev: PointerEvent, side: "in" | "out"): void;
  (e: "volume-change", v: number): void;
  (e: "dsp-param", key: string, v: number): void;
  (e: "dsp-toggle", v: boolean): void;
}>();

// ---------- 频谱条高度 ----------
// 整像素取整：百分比高度（如 4.3px）会被浏览器逐柱取整，底边出现 1px 抖动/错位
const SPEC_ROW_H = 15; // 与 .node-spectrum 的 height 保持一致
function specPx(db: number): number {
  const pct = Math.min(100, Math.max(0, ((db + 60) / 60) * 100));
  return Math.max(2, Math.round((pct / 100) * SPEC_ROW_H));
}
</script>

<template>
  <div class="node" :class="[`node-${node.kind}`, { selected, wiring }]" :data-node-key="node.key" :style="{
    left: Math.round(node.x) + 'px',
    top: Math.round(node.y) + 'px',
    width: NODE_W + 'px',
    height: NODE_H + 'px',
  }" @pointerdown="emit('drag-start', $event)" @click.stop="emit('activate')"
    @contextmenu.prevent.stop="emit('menu', $event)">
    <div class="node-head">
      <n-tag size="tiny" :type="KIND_META[node.kind].type" :bordered="false"
        :class="{ 'tag-dsp': node.kind === 'dsp' }">
        {{ KIND_META[node.kind].tag }}
      </n-tag>
      <span class="node-title">{{ node.title }}</span>
      <span class="node-close" @pointerdown.stop @click.stop="emit('remove')">×</span>
    </div>
    <div class="node-sub">{{ node.subtitle }}</div>
    <!-- 频谱分析开启：20 段 dB 频谱条取代电平条；关闭时照常显示电平条 -->
    <div v-if="spectrum" class="node-meter node-spectrum" @pointerdown.stop @click.stop>
      <i v-for="(db, i) in spectrum" :key="i" class="spec-bar" :style="{ height: specPx(db) + 'px' }" />
    </div>
    <MeterBar v-else class="node-meter" :level="meter" />
    <!-- DSP 方块状态行：所有 DSP 类型都直接在方块上放旁路开关（状态点 + 状态小字 + 开关） -->
    <div v-if="dsp" class="node-state" @pointerdown.stop @click.stop>
      <i class="state-dot" :class="badge?.cls" />
      <span>{{ badge?.text }}</span>
      <n-switch :value="dsp.enabled" size="small" style="margin-left: auto"
        @update:value="(v: boolean) => emit('dsp-toggle', v)" />
    </div>

    <!-- 增益 / 延迟：参数滑杆直接在方块上（改动即下发引擎） -->
    <div v-if="dsp && dsp.type === 'gain'" class="node-ctl" @pointerdown.stop @click.stop>
      <n-slider :value="dsp.db" :min="-60" :max="12" :step="0.5"
        :format-tooltip="(v: number) => v.toFixed(1) + ' dB'"
        @update:value="(v: number) => emit('dsp-param', 'db', v)" />
      <span class="node-ctl-val">{{ dsp.db > 0 ? "+" : "" }}{{ dsp.db }}dB</span>
    </div>
    <div v-if="dsp && dsp.type === 'delay'" class="node-ctl" @pointerdown.stop @click.stop>
      <n-slider :value="dsp.ms" :min="0" :max="1000" :step="1"
        :format-tooltip="(v: number) => Math.round(v) + ' ms'"
        @update:value="(v: number) => emit('dsp-param', 'ms', Math.round(v))" />
      <span class="node-ctl-val">{{ Math.round(dsp.ms) }}ms</span>
    </div>

    <!-- 输出/线路输入节点：调的是该设备的 **Windows 系统音量**（线路端点没有音量条） -->
    <div v-if="volumeId" class="node-vol" @pointerdown.stop @click.stop>
      <span class="node-vol-icon">🔊</span>
      <n-slider :value="volume" :min="0" :max="1" :step="0.01" :tooltip="false" size="small"
        @update:value="(v: number) => emit('volume-change', v)" />
      <span class="node-vol-pct">{{ Math.round(volume * 100) }}%</span>
    </div>

    <!-- 端子：小圆点只是视觉，命中由更宽的 term-zone 负责（整条边都能拖出连线） -->
    <span v-if="node.hasIn" class="terminal t-in" :class="termStateOf('in')" :style="termStyle(node, 'in')" />
    <div v-if="node.hasIn" class="term-zone zone-in" :data-term="`${node.key}|in`" :title="termTitle(node, 'in')"
      @pointerdown.stop="emit('wire-start', $event, 'in')" @click.stop />
    <span v-if="node.hasOut" class="terminal t-out" :class="termStateOf('out')" :style="termStyle(node, 'out')" />
    <div v-if="node.hasOut" class="term-zone zone-out" :data-term="`${node.key}|out`" :title="termTitle(node, 'out')"
      @pointerdown.stop="emit('wire-start', $event, 'out')" @click.stop />
  </div>
</template>

<style scoped>
.node {
  position: absolute;
  /* 必须建立层叠上下文：否则端子圈(z-index:2)参与全局层叠，节点重叠时圈会浮到别的节点主体上面 */
  z-index: 1;
  border: 1px solid var(--border);
  border-radius: 8px;
  /* 节点配色走主题变量（style.css），深/亮色模式自动切换 */
  background: var(--surface);
  color: var(--text);
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.35);
  /* 左右各留 18px：端子会伸进盒子 8px，文字不会被压住 */
  padding: 8px 18px;
  box-sizing: border-box;
  cursor: move;
  user-select: none;
  touch-action: none;
  font-size: 12px;
}

.node:hover {
  border-color: var(--border-strong);
}

.node.selected {
  z-index: 2;
  /* 重叠时选中的节点浮到上面，避免被压住 */
  border-color: #f0a020;
  box-shadow: 0 0 0 2px rgba(240, 160, 32, 0.25), 0 2px 8px rgba(0, 0, 0, 0.35);
}

/* DSP 处理方块：紫色调，和普通设备节点一眼区分开 */
.node.node-dsp {
  border-color: rgba(156, 108, 236, 0.55);
  background: var(--surface-dsp);
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
  /* 最多两行：副标题常是「系统播放端（扬声器）\n拷贝：… · 状态」两行文本，一行省略号看不全 */
  display: -webkit-box;
  -webkit-box-orient: vertical;
  -webkit-line-clamp: 2;
  overflow: hidden;
  /* pre-line：保留副标题里的 \n 分行，其余照常折行 */
  white-space: pre-line;
  word-break: break-all;
}

.node-vol {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 6px;
}

.node-meter {
  margin-top: 4px;
}

/* 频谱条：比电平条（8px）高近一倍，20 根细柱按 dB 高低起伏 */
.node-spectrum {
  height: 15px;
  display: flex;
  align-items: flex-end;
  gap: 1px;
}

.spec-bar {
  flex: 1;
  min-width: 1px;
  background: var(--accent);
  border-radius: 1px;
}

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

/* 增益/延迟的方块内参数滑杆（值右对齐，滑杆吃剩余宽度） */
.node-ctl {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 4px;
}

.node-ctl :deep(.n-slider) {
  flex: 1;
}

.node-ctl-val {
  font-size: 11px;
  opacity: 0.75;
  min-width: 44px;
  text-align: right;
  font-variant-numeric: tabular-nums;
}

/* DSP 的标签统一紫色（n-tag 的 warning 橙和方块配色不搭） */
.tag-dsp {
  background-color: rgba(156, 108, 236, 0.3) !important;
  color: var(--purple-text) !important;
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
  border: 2px solid var(--accent);
  /* 实心：空心环会把从环下经过的连线和箭头尖「漏」出来，看起来像线穿过了圆环 */
  background: var(--accent);
  pointer-events: none;
  /* 命中交给 .term-zone */
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
  border-color: var(--accent);
  /* 输出＝蓝 */
  background: var(--accent);
}

.terminal.t-in {
  border-color: #18a058;
  /* 输入＝绿 */
  background: #18a058;
}

/* 透明命中环：视觉上还是个 16px 的小点，实际点按范围约 32px */
.terminal::after {
  content: "";
  position: absolute;
  inset: -8px;
  border-radius: 50%;
}

/* 拉线时：能接的端子亮黄圈，不能接的（同类型）变暗。
   （原来挂在祖先 .canvas.wiring 上；拆组件后 scoped 样式不跨组件，改挂在节点自身） */
.node.wiring .terminal.compat {
  box-shadow: 0 0 0 5px rgba(240, 160, 32, 0.45);
}

.node.wiring .terminal.incompat {
  opacity: 0.18;
  cursor: not-allowed;
}
</style>
