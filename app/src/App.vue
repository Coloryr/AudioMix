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
let deviceTimer: number | undefined;

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

onMounted(async () => {
  document.addEventListener("visibilitychange", syncLevelsStream);
  await app.loadAll();
  syncLevelsStream();
  // 窗口隐藏时设备列表轮询也暂停；热插拔由后端看门狗（3s）兜底
  deviceTimer = window.setInterval(() => {
    if (document.hidden) return;
    app.pollDevices().catch(() => { });
  }, 4000);
});
onUnmounted(() => {
  document.removeEventListener("visibilitychange", syncLevelsStream);
  window.clearInterval(deviceTimer);
  app.unwatchLevels();
});
</script>

<template>
  <n-config-provider :theme="themeMode === 'dark' ? darkTheme : null" :locale="zhCN" :date-locale="dateZhCN">
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
        <n-tabs v-model:value="activeTab" type="line" style="flex: 1; padding: 0 16px"
          pane-style="height: calc(100vh - 96px); overflow: auto;">
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
