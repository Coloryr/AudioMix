<script setup lang="ts">
// 画布内悬浮属性面板（点击节点 / 连线后弹出）：连线的断开、采集开关、
// 混音音量、系统音量、DSP 参数（内嵌 DspParamPanel）、移除节点。
// 定位（popStyle）与关闭时机（点外部关闭）仍由 MixerView 控制 —— 这里只负责内容。
import { computed } from "vue";
import { NButton, NSlider, NSwitch, NTag, useMessage } from "naive-ui";
import { api, type Route } from "../../api";
import { useApp } from "../../store";
import DspParamPanel from "./DspParamPanel.vue";
import { KIND_META, sinkDeviceIdOf, type GNode } from "./types";

const props = defineProps<{
  /** 选中的节点（连线选中时为 null） */
  node: GNode | null;
  /** 选中的连线 */
  route: Route | null;
  /** 连线标题（from → to） */
  routeTitle: string;
  /** node.sinkId 对应设备的 Windows 系统音量（0..1） */
  deviceVolume: number;
}>();

const emit = defineEmits<{
  (e: "close"): void;
  (e: "disconnect", routeId: string): void;
  (e: "volume-change", deviceId: string, v: number): void;
  (e: "remove", node: GNode): void;
}>();

const app = useApp();
const message = useMessage();

const sourceEnabled = computed(() =>
  props.node?.sourceId ? (app.graph.sources.find((s) => s.id === props.node!.sourceId)?.enabled ?? true) : false,
);
const sinkVolume = computed(() =>
  props.node?.sinkId ? (app.graph.sinks.find((s) => s.id === props.node!.sinkId)?.volume ?? 1) : 1,
);
const sinkEnabled = computed(() =>
  props.node?.sinkId ? (app.graph.sinks.find((s) => s.id === props.node!.sinkId)?.enabled ?? true) : true,
);
const processor = computed(() =>
  props.node?.processorId
    ? (app.graph.processors.find((p) => p.id === props.node!.processorId) ?? null)
    : null,
);
/** 输出节点对应的 Windows 设备 id（系统音量行）。线路端点（usbip://）的端点音量
 *  不经过虚拟线路的数据通路（调了没效果），不显示系统音量行，用「混音」调 */
const deviceId = computed(() => (props.node ? (sinkDeviceIdOf(props.node, app.graph.sinks) ?? null) : null));
const showDeviceVolume = computed(() => !!deviceId.value && !deviceId.value.startsWith("usbip://"));

async function save() {
  try {
    await app.saveGraph();
  } catch (e) {
    message.error(String(e));
  }
}

function onSinkVolume(sinkId: string, volume: number) {
  const k = app.graph.sinks.find((x) => x.id === sinkId);
  if (k) k.volume = volume;
  api.setSinkVolume(sinkId, volume).catch((e) => message.error(String(e)));
}

function onSourceEnabled(sourceId: string, enabled: boolean) {
  const s = app.graph.sources.find((x) => x.id === sourceId);
  if (s) s.enabled = enabled;
  void save();
}

function onSinkEnabled(sinkId: string, enabled: boolean) {
  const k = app.graph.sinks.find((x) => x.id === sinkId);
  if (k) k.enabled = enabled;
  void save();
}
</script>

<template>
  <div class="canvas-pop" @pointerdown.stop @click.stop @contextmenu.prevent.stop>
    <div class="canvas-pop-head">
      <n-tag size="small" :bordered="false" :type="node ? KIND_META[node.kind].type : 'info'"
        :class="{ 'tag-dsp': node?.kind === 'dsp' }">
        {{ node ? KIND_META[node.kind].tag : "连线" }}
      </n-tag>
      <span class="canvas-pop-title">{{ node?.title ?? routeTitle }}</span>
      <span class="node-close" @pointerdown.stop @click.stop="emit('close')">×</span>
    </div>

    <!-- 连线：只代表连接关系，唯一操作是断开 -->
    <template v-if="route">
      <div class="canvas-pop-row">
        <n-button size="tiny" quaternary type="error" style="margin-left: auto" @click="emit('disconnect', route.id)">
          断开
        </n-button>
      </div>
    </template>

    <!-- 节点：采集 / 混音音量 / 系统音量 / DSP 参数 -->
    <template v-else>
      <div v-if="node!.sourceId" class="canvas-pop-row">
        <span class="item-sub" style="width: 48px">采集</span>
        <n-switch :value="sourceEnabled" size="small"
          @update:value="(v: boolean) => onSourceEnabled(node!.sourceId!, v)" />
      </div>
      <template v-if="node!.sinkId">
        <div class="canvas-pop-row">
          <span class="item-sub" style="width: 48px">混音</span>
          <n-slider :value="sinkVolume" :min="0" :max="1" :step="0.01"
            :format-tooltip="(v: number) => Math.round(v * 100) + '%'" style="flex: 1"
            @update:value="(v: number) => onSinkVolume(node!.sinkId!, v)" />
          <n-switch :value="sinkEnabled" size="small" @update:value="(v: boolean) => onSinkEnabled(node!.sinkId!, v)" />
        </div>
        <div v-if="showDeviceVolume" class="canvas-pop-row">
          <span class="item-sub" style="width: 48px">系统音量</span>
          <n-slider :value="deviceVolume" :min="0" :max="1" :step="0.01"
            :format-tooltip="(v: number) => Math.round(v * 100) + '%'" style="flex: 1"
            @update:value="(v: number) => deviceId && emit('volume-change', deviceId, v)" />
        </div>
      </template>

      <!-- DSP 方块参数编辑 -->
      <DspParamPanel v-if="processor" :processor="processor" />

      <div class="canvas-pop-row" style="margin-top: 4px">
        <n-button size="tiny" quaternary type="error" @click="node && emit('remove', node)">移除节点</n-button>
      </div>
    </template>
  </div>
</template>

<style scoped>
/* 画布内悬浮属性面板：点击节点/连线后在目标旁弹出 */
.canvas-pop {
  position: absolute;
  z-index: 6;
  box-sizing: border-box;
  background: var(--surface-pop);
  border: 1px solid rgba(156, 108, 236, 0.35);
  border-radius: 8px;
  /* maxHeight 由 popStyle 按画布剩余高度给（写在 :style 上），装不下时内部滚 */
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

/* DSP 的标签统一紫色（n-tag 的 warning 橙和方块配色不搭） */
.tag-dsp {
  background-color: rgba(156, 108, 236, 0.3) !important;
  color: var(--purple-text) !important;
}

/* WebView2 默认用 Fluent 悬浮滚动条（空闲时不可见），自定义后强制显示常驻滚动条 */
.canvas-pop::-webkit-scrollbar {
  width: 8px;
  height: 8px;
}

.canvas-pop::-webkit-scrollbar-thumb {
  background: var(--hover-chip);
  border-radius: 4px;
}

.canvas-pop::-webkit-scrollbar-thumb:hover {
  background: var(--chip-strong);
}

.canvas-pop::-webkit-scrollbar-track {
  background: transparent;
}
</style>
