<script setup lang="ts">
// 节点电平条：按 dBFS 阈值分段变色（绿→黄→红）+ 峰值保持缓慢回落。
// 电平来自 App.vue 的 250ms 全局轮询（store.levels），组件本身不开定时器——
// 所有节点共用同一次轮询，回落也只在这条共享数据流上推进。
import { computed, ref, watch } from "vue";

const props = defineProps<{ /** 线性峰值 0..1（1.0 = 0 dBFS） */ level: number }>();

// TASK 口径：< -4dBFS 绿、-4…-6dBFS 黄、> -2dBFS 红
const TH_YELLOW = Math.pow(10, -4 / 20);
const TH_RED = Math.pow(10, -2 / 20);

const peak = ref(0);
watch(
  () => props.level,
  (v) => {
    // 超过峰值立刻抬上去，否则每拍回落 5% 满刻度（250ms 一拍 ≈ 8%/s，肉眼是缓慢回落）
    peak.value = Math.max(v, peak.value - 0.05);
  },
);

const pct = computed(() => `${Math.min(1, Math.max(0, props.level)) * 100}%`);
const peakPct = computed(() => `${Math.min(1, Math.max(0, peak.value)) * 100}%`);
const seg = computed(() => (props.level > TH_RED ? "hot" : props.level > TH_YELLOW ? "warm" : "safe"));
</script>

<template>
  <div class="meter-track">
    <div class="meter-fill" :class="seg" :style="{ width: pct }" />
    <div v-if="peak > 0.005" class="meter-peak" :style="{ left: peakPct }" />
  </div>
</template>
