<script setup lang="ts">
// DSP 方块参数面板：滑杆 / 图形均衡 10 段竖向滑杆 / 频响曲线 / 重置 + 启用开关。
// 参数改动直接改 store 里的 Processor 并下发后端（后端命令内部持久化，无需再 save）。
import { computed } from "vue";
import { NButton, NSlider, NSwitch, NText, useMessage } from "naive-ui";
import { api, type Processor } from "../../api";
import { useApp } from "../../store";
import {
  CURVE_W,
  CURVE_H,
  DSP_GEQ_BANDS,
  DSP_PARAMS,
  curveX,
  curveY,
  dspCurveOf,
  makeDspNode,
  sliderCommit,
  sliderTip,
  sliderVal,
} from "./types";

const props = defineProps<{ processor: Processor }>();

const app = useApp();
const message = useMessage();

const procStateText = computed(() => {
  const p = props.processor;
  if (p.type === "switch") return p.enabled ? "开：信号直通" : "关：输出静音";
  return p.enabled ? "处理中（接在信号路径上）" : "已旁路（信号直通）";
});

/** 参数有变化后下发引擎（后端 set_processor_params 命令内部持久化，无需再 save） */
function touchProcessor() {
  const p = app.graph.processors.find((x) => x.id === props.processor.id);
  if (!p) return;
  api.setProcessorParams(p.id, { ...p }).catch((e) => message.error(String(e)));
}

function onProcEnable(enabled: boolean) {
  const p = app.graph.processors.find((x) => x.id === props.processor.id);
  if (!p) return;
  p.enabled = enabled;
  touchProcessor();
}

/** key 为数字时表示 graph_eq 的第几段增益 */
function onProcParam(key: string | number, value: number) {
  const p = app.graph.processors.find((x) => x.id === props.processor.id);
  if (!p) return;
  if (typeof key === "number") {
    if (p.type === "graph_eq") p.gains_db[key] = value;
  } else {
    (p as unknown as Record<string, unknown>)[key] = value;
  }
  touchProcessor();
}

/** 重置为该类型的默认参数（保留启用状态与 id） */
function onProcReset() {
  const p = app.graph.processors.find((x) => x.id === props.processor.id);
  if (!p) return;
  const enabled = p.enabled;
  Object.assign(p, makeDspNode(p.type), { enabled });
  touchProcessor();
}

/** 频响曲线数据（高通/低通/带通/峰式均衡/三段均衡；Q 滑杆一动曲线即时跟着变） */
const dspCurve = computed(() => dspCurveOf(props.processor));
</script>

<template>
  <!-- DSP 方块参数编辑 -->
  <div class="dsp-section">
    <div class="dsp-head">
      <n-switch :value="processor.enabled" size="small" @update:value="(v: boolean) => onProcEnable(v)" />
      <n-text style="font-size: 12px">{{ procStateText }}</n-text>
      <div style="display: flex; align-items: center; gap: 4px">
        <n-button size="tiny" quaternary @click="onProcReset()">重置</n-button>
      </div>
    </div>
    <div v-if="processor.type !== 'graph_eq'" class="dsp-params">
      <div v-for="p in DSP_PARAMS[processor.type]" :key="p.key" class="dsp-param">
        <span class="item-sub" style="width: 48px">{{ p.label }}</span>
        <n-slider :value="sliderVal(p, (processor as unknown as Record<string, number>)[p.key])"
          :min="p.log ? Math.log10(p.min) : p.min" :max="p.log ? Math.log10(p.max) : p.max"
          :step="p.log ? 0.005 : p.step" :format-tooltip="(v: number) => sliderTip(p, v)" style="flex: 1"
          @update:value="(v: number) => onProcParam(p.key, sliderCommit(p, v))" />
      </div>
    </div>
    <div v-else class="dsp-geq">
      <div v-for="(band, b) in DSP_GEQ_BANDS" :key="b" class="dsp-geq-band">
        <!-- 竖向滑杆的高度必须给真实 CSS 高度（naive-ui 没有 height prop，写属性不生效就量不出轨道） -->
        <n-slider vertical :value="processor.gains_db[b]" :min="-24" :max="24" :step="1" style="height: 80px"
          @update:value="(v: number) => onProcParam(b, v)" />
        <span class="item-sub">{{ band }}</span>
      </div>
    </div>
    <!-- 高通/低通/带通频响曲线（对数频轴，虚线 = 频点，色块 = 通带） -->
    <svg v-if="dspCurve" class="dsp-curve" :viewBox="`0 0 ${CURVE_W} ${CURVE_H}`">
      <line :x1="0" :y1="curveY(0)" :x2="CURVE_W" :y2="curveY(0)" class="dsp-curve-zero" />
      <rect v-if="dspCurve.band" :x="curveX(dspCurve.band[0])" :y="0"
        :width="curveX(dspCurve.band[1]) - curveX(dspCurve.band[0])" :height="CURVE_H" class="dsp-curve-band" />
      <line v-for="(f, i) in dspCurve.marks" :key="i" :x1="curveX(f)" :y1="0" :x2="curveX(f)" :y2="CURVE_H"
        class="dsp-curve-mark" />
      <path :d="dspCurve.d" class="dsp-curve-line" />
    </svg>
  </div>
</template>

<style scoped>
/* DSP 参数面板（悬浮弹窗里的 DSP 参数编辑区） */
.dsp-section {
  margin-top: 10px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}

/* 三列 grid：两侧等宽（1fr），中间的状态文字在这行真正水平居中（space-between 会偏向开关一侧） */
.dsp-head {
  display: grid;
  grid-template-columns: 1fr auto 1fr;
  align-items: center;
  gap: 8px;
}

.dsp-head > :first-child {
  justify-self: start;
}

.dsp-head > :last-child {
  justify-self: end;
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
  stroke: var(--accent);
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
</style>
