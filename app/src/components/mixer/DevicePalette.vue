<script setup lang="ts">
// 左栏：设备 + DSP 处理两张卡片（绝对定位：不参与行高计算，永远不会撑开画布）。
// 面板条目按下即开始拖拽（拖到画布生成节点，直接点击 = 放到默认位置）。
import { NCard, NTag, NText } from "naive-ui";
import { KIND_META, type PaletteItem } from "./types";

defineProps<{
  /** 设备类条目（真实/虚拟设备 + 线路端点） */
  devices: PaletteItem[];
  /** DSP 处理方块条目 */
  dsp: PaletteItem[];
  /** 该条目是否已在画布上（标灰，DSP 方块不限数量永远不灰） */
  isExisting: (item: PaletteItem) => boolean;
}>();

const emit = defineEmits<{
  (e: "drag-start", item: PaletteItem, ev: PointerEvent): void;
}>();
</script>

<template>
  <div class="sidebar">
    <!-- 设备面板：按住拖到画布上生成节点（也可直接点击 = 放到默认位置） -->
    <n-card size="small" title="设备" class="dev-card">
      <n-text depth="3" style="font-size: 12px; display: block; margin-bottom: 8px">
        按住拖到右侧画布；直接点击则放到默认位置。
      </n-text>
      <div class="palette">
        <div v-for="(item, i) in devices" :key="i" class="palette-item" :class="{ used: isExisting(item) }"
          @pointerdown="emit('drag-start', item, $event)">
          <n-tag size="tiny" :type="KIND_META[item.kind].type" :bordered="false">
            {{ KIND_META[item.kind].tag }}
          </n-tag>
          <span class="palette-title">{{ item.title }}</span>
        </div>
        <n-text v-if="!devices.length" depth="3" style="font-size: 12px">未检测到设备</n-text>
      </div>
    </n-card>

    <!-- DSP 处理方块：独立卡片，不限数量 -->
    <n-card size="small" title="DSP 处理" class="dev-card dsp-card">
      <n-text depth="3" style="font-size: 12px; display: block; margin-bottom: 8px">
        拖到画布上串进线路，对路过的信号做处理；不限数量。
      </n-text>
      <div class="palette">
        <div v-for="(item, i) in dsp" :key="'dsp' + i" class="palette-item palette-item-dsp"
          @pointerdown="emit('drag-start', item, $event)">
          <n-tag size="tiny" :type="KIND_META[item.kind].type" :bordered="false" class="tag-dsp">
            {{ KIND_META[item.kind].tag }}
          </n-tag>
          <span class="palette-title">{{ item.title }}</span>
        </div>
      </div>
    </n-card>
  </div>
</template>

<style scoped>
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
  color: var(--purple-text);
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
  flex-shrink: 0;
  /* 列表超长时保持条目完整高度交给滚动，不被压缩 */
}

/* WebView2 默认用 Fluent 悬浮滚动条（空闲时不可见），自定义后强制显示常驻滚动条 */
.palette::-webkit-scrollbar {
  width: 8px;
  height: 8px;
}

.palette::-webkit-scrollbar-thumb {
  background: var(--hover-chip);
  border-radius: 4px;
}

.palette::-webkit-scrollbar-thumb:hover {
  background: var(--chip-strong);
}

.palette::-webkit-scrollbar-track {
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

/* DSP 的标签统一紫色（n-tag 的 warning 橙和方块配色不搭） */
.tag-dsp {
  background-color: rgba(156, 108, 236, 0.3) !important;
  color: var(--purple-text) !important;
}
</style>
