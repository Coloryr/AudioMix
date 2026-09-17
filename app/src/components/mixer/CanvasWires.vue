<script setup lang="ts">
// 画布连线层：SVG 比画布外扩一圈（-1..2 的世界范围），节点拖到画布矩形外时
// viewBox 平移到负区，路径里的世界坐标不用换算。选中/静音/拉线预览/对齐辅助线都在这层。
import { wirePath, previewWirePath, type Wire } from "./types";

defineProps<{
  wires: Wire[];
  /** 选中的连线 id（route id） */
  selectedId: string | null;
  /** 拉线预览（拖拽中或点选端子后跟随鼠标） */
  pending: { from: { x: number; y: number; dir: number }; to: { x: number; y: number } } | null;
  /** 拖动节点时穿过节点中心的辅助线 */
  guides: { x: number; y: number } | null;
  /** 画布尺寸（px），SVG 尺寸 = 3 倍外扩 */
  size: { w: number; h: number };
}>();

const emit = defineEmits<{
  (e: "select", id: string): void;
  (e: "menu", ev: MouseEvent, wire: Wire): void;
}>();
</script>

<template>
  <svg class="wire-layer" :width="size.w * 3" :height="size.h * 3"
    :viewBox="`${-size.w} ${-size.h} ${size.w * 3} ${size.h * 3}`"
    :style="{ left: -size.w + 'px', top: -size.h + 'px' }">
    <defs>
      <marker id="wire-arrow" viewBox="0 0 10 10" refX="0" refY="5" markerWidth="6" markerHeight="6" orient="auto">
        <path d="M 0 0 L 10 5 L 0 10 z" style="fill: var(--accent)" />
      </marker>
      <marker id="wire-arrow-muted" viewBox="0 0 10 10" refX="0" refY="5" markerWidth="6" markerHeight="6"
        orient="auto">
        <path d="M 0 0 L 10 5 L 0 10 z" fill="#8a8a8a" />
      </marker>
      <marker id="wire-arrow-sel" viewBox="0 0 10 10" refX="0" refY="5" markerWidth="6" markerHeight="6" orient="auto">
        <path d="M 0 0 L 10 5 L 0 10 z" fill="#f0a020" />
      </marker>
    </defs>
    <path v-for="wire in wires" :key="wire.id" :d="wirePath(wire.from, wire.to)" class="wire"
      :class="{ muted: wire.muted, selected: selectedId === wire.id }" :marker-end="selectedId === wire.id
          ? 'url(#wire-arrow-sel)'
          : wire.muted
            ? 'url(#wire-arrow-muted)'
            : 'url(#wire-arrow)'
        " @pointerdown.stop="emit('select', wire.id)" @contextmenu.prevent.stop="emit('menu', $event, wire)" />
    <path v-if="pending" :d="previewWirePath(pending.from, pending.to)" class="wire pending" />
    <circle v-if="pending" :cx="pending.to.x" :cy="pending.to.y" r="4.5" class="pending-dot" />
    <template v-if="guides">
      <line :x1="-size.w" :y1="guides.y" :x2="size.w * 2" :y2="guides.y" class="guide" />
      <line :x1="guides.x" :y1="-size.h" :x2="guides.x" :y2="size.h * 2" class="guide" />
    </template>
  </svg>
</template>

<style scoped>
.wire-layer {
  position: absolute;
  left: 0;
  top: 0;
  pointer-events: none;
}

.wire {
  fill: none;
  stroke: var(--accent);
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
</style>
