<script setup lang="ts">
import { onMounted, onUnmounted, ref, watch } from "vue";
import {
  NConfigProvider,
  NMessageProvider,
  NTabs,
  NTabPane,
  NSpace,
  NTag,
  NButton,
  darkTheme,
  zhCN,
  dateZhCN,
} from "naive-ui";
import MixerView from "./views/MixerView.vue";
import DriverView from "./views/DriverView.vue";
import SettingsView from "./views/SettingsView.vue";
import { useApp } from "./store";

const app = useApp();
const activeTab = ref("mixer");

/** 主题：深色＝naive-ui darkTheme + 默认变量；亮色＝naive 默认浅色 + html.light 变量覆盖。选择存 localStorage */
const themeMode = ref<"dark" | "light">(localStorage.getItem("audiomix-theme") === "light" ? "light" : "dark");
function toggleTheme() {
  themeMode.value = themeMode.value === "dark" ? "light" : "dark";
}
watch(
  themeMode,
  (m) => {
    localStorage.setItem("audiomix-theme", m);
    document.documentElement.classList.toggle("light", m === "light");
  },
  { immediate: true },
);

/** 电平流调度：混音页可见 → 订阅后端 Channel 推送；切走页签/窗口隐藏 → 退订（后端跳过读取与序列化） */
async function syncLevelsStream() {
  if (document.hidden || activeTab.value !== "mixer") await app.unwatchLevels();
  else await app.watchLevels();
}
watch(activeTab, syncLevelsStream);

/** 设备列表改用 Channel 推送：热插拔/看门狗枚举有变化才推；窗口隐藏时退订 */
async function syncDevicesStream() {
  if (document.hidden) await app.unwatchDevices();
  else await app.watchDevices();
}

onMounted(async () => {
  document.addEventListener("visibilitychange", syncDevicesStream);
  await app.loadAll();
  syncLevelsStream();
  syncDevicesStream();
});
onUnmounted(() => {
  document.removeEventListener("visibilitychange", syncDevicesStream);
  app.unwatchLevels();
  app.unwatchDevices();
});
</script>

<template>
  <!-- height:100% 必须挂上：n-config-provider 渲染的包裹 div 默认没有高度，
       断了 #app → 页签 pane 的高度传递链（pane 靠 style.css 的 flex 规则撑满剩余空间） -->
  <n-config-provider style="height: 100%" :theme="themeMode === 'dark' ? darkTheme : null" :locale="zhCN" :date-locale="dateZhCN">
    <n-message-provider>
      <div style="height: 100%; display: flex; flex-direction: column">
        <div style="display: flex; align-items: center; gap: 10px; padding: 10px 16px 0">
          <span style="font-size: 17px; font-weight: 800; letter-spacing: 0.5px">
            AudioMix 虚拟调音台
          </span>
          <n-tag size="small" :bordered="false" type="info">WASAPI</n-tag>
          <n-tag size="small" :bordered="false" :type="app.apiStatus.running ? 'success' : 'default'">
            控制 API {{ app.apiStatus.running ? "运行中" : "关闭" }}
          </n-tag>
          <n-button quaternary size="small" style="margin-left: auto" @click="toggleTheme">
            {{ themeMode === "dark" ? "☀ 亮色模式" : "🌙 暗色模式" }}
          </n-button>
        </div>
        <!-- 页签占满整行、横向 padding 收进 pane 内层：pane 的滚动条才会贴着窗口右缘。
             pane 高度由 style.css 的 flex 规则撑满剩余空间（不写死「头部+页签栏」常数） -->
        <n-tabs v-model:value="activeTab" type="line" style="flex: 1; min-height: 0; padding: 0 16px"
          pane-style="overflow: auto; margin: 0 -16px; padding: 0 16px;">
          <n-tab-pane name="mixer" tab="混音">
            <MixerView />
          </n-tab-pane>
          <n-tab-pane name="devices" tab="虚拟声卡">
            <DriverView />
          </n-tab-pane>
          <n-tab-pane name="settings" tab="设置">
            <SettingsView />
          </n-tab-pane>
        </n-tabs>
      </div>
    </n-message-provider>
  </n-config-provider>
</template>
