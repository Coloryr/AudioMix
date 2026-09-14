<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import {
  NConfigProvider,
  NMessageProvider,
  NTabs,
  NTabPane,
  NSpace,
  NTag,
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
let levelTimer: number | undefined;
let deviceTimer: number | undefined;

onMounted(async () => {
  await app.loadAll();
  levelTimer = window.setInterval(() => app.pollLevels().catch(() => {}), 250);
  // 设备列表轻量轮询：后端看门狗（3s）负责枚举热插拔，这里只同步 UI 展示
  deviceTimer = window.setInterval(() => app.pollDevices().catch(() => {}), 2000);
});
onUnmounted(() => {
  window.clearInterval(levelTimer);
  window.clearInterval(deviceTimer);
});
</script>

<template>
  <n-config-provider :theme="darkTheme" :locale="zhCN" :date-locale="dateZhCN">
    <n-message-provider>
      <div style="height: 100%; display: flex; flex-direction: column">
        <div style="display: flex; align-items: center; gap: 10px; padding: 10px 16px 0">
          <span style="font-size: 17px; font-weight: 800; letter-spacing: 0.5px">
            🎚️ AudioMix
          </span>
          <n-tag size="small" :bordered="false" type="info">WASAPI</n-tag>
          <n-tag size="small" :bordered="false" :type="app.apiStatus.running ? 'success' : 'default'">
            控制 API {{ app.apiStatus.running ? "运行中" : "关闭" }}
          </n-tag>
        </div>
        <n-tabs v-model:value="activeTab" type="line" style="flex: 1; padding: 0 16px" pane-style="height: calc(100vh - 96px); overflow: auto;">
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
