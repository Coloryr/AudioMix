<script setup lang="ts">
import { nextTick, onMounted, onUnmounted, ref, watch } from "vue";
import {
  NButton,
  NCard,
  NDivider,
  NSwitch,
  NInputNumber,
  NFormItem,
  NSelect,
  NTag,
  NText,
  NPopconfirm,
  useMessage,
} from "naive-ui";
import { api, type LogLine } from "../api";
import { useApp } from "../store";

const app = useApp();
const message = useMessage();
const busy = ref(false);

async function save() {
  busy.value = true;
  try {
    await api.updateSettings(app.settings);
    app.apiStatus = await api.getControlApiStatus();
    message.success("设置已保存");
  } catch (e) {
    message.error(String(e));
  } finally {
    busy.value = false;
  }
}

async function toggleAutostart(enabled: boolean) {
  try {
    await api.setAutostart(enabled);
    app.autostart = enabled;
  } catch (e) {
    message.error(String(e));
  }
}

function copyAddr() {
  if (app.apiStatus.addr) navigator.clipboard?.writeText(app.apiStatus.addr);
}

// ---------- 运行日志 ----------
const logs = ref<LogLine[]>([]);
const lastSeq = ref(0);
const follow = ref(true);
const logBox = ref<HTMLElement | null>(null);
let timer: number | undefined;

const MAX_SHOWN = 1500;

async function pollLogs() {
  try {
    const { lines, next } = await api.getLogs(lastSeq.value);
    if (lines.length) {
      lastSeq.value = next;
      logs.value.push(...lines);
      if (logs.value.length > MAX_SHOWN) logs.value.splice(0, logs.value.length - MAX_SHOWN);
      if (follow.value) {
        await nextTick();
        const el = logBox.value;
        if (el) el.scrollTop = el.scrollHeight;
      }
    }
  } catch {
    // 日志拉取失败不打扰用户
  }
}

async function clearLogs() {
  await api.clearLogs();
  logs.value = [];
  lastSeq.value = 0;
  message.success("日志已清空");
}

/** 复制：先用同步的 execCommand（保留用户手势），失败再退回剪贴板 API */
async function copyText(text: string) {
  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.top = "-1000px";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    ta.setSelectionRange(0, text.length);
    const ok = document.execCommand("copy");
    document.body.removeChild(ta);
    if (ok) return true;
  } catch {
    // 继续尝试剪贴板 API
  }
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // 两种方式都失败
  }
  return false;
}

async function copyLogs() {
  if (!logs.value.length) {
    message.info("暂无日志可复制");
    return;
  }
  const text = logs.value.map((l) => `[${l.seq}] ${l.text}`).join("\r\n");
  const ok = await copyText(text);
  if (ok) message.success(`已复制 ${logs.value.length} 行日志`);
  else message.error("复制失败，请手动选择文本后 Ctrl+C");
}

/** 按级别上色 */
function logClass(text: string) {
  if (text.includes("ERROR")) return "log-error";
  if (text.includes("WARN")) return "log-warn";
  if (text.includes("DEBUG")) return "log-debug";
  return "";
}

/** 用户往上滚就暂停自动跟随 */
function onLogScroll() {
  const el = logBox.value;
  if (!el) return;
  follow.value = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
}

/** 一旦开始选中文本就暂停跟随，否则新日志会把选区顶走 */
function onLogMouseUp() {
  if ((window.getSelection()?.toString() ?? "").length > 0) follow.value = false;
}

watch(follow, async (v) => {
  if (v) {
    await nextTick();
    const el = logBox.value;
    if (el) el.scrollTop = el.scrollHeight;
  }
});

onMounted(() => {
  pollLogs();
  timer = window.setInterval(pollLogs, 700);
});
onUnmounted(() => window.clearInterval(timer));
</script>

<template>
  <div class="settings-grid">
    <!-- 左：设置 -->
    <div style="display: flex; flex-direction: column; gap: 14px; min-width: 0">
      <n-card title="音频引擎" size="small">
        <div class="item-row">
          <div style="flex: 1">
            <div class="item-title">重采样质量</div>
            <n-text depth="3" style="font-size: 12px">
              高质量与低延迟的权衡，切换立即生效。
              sinc 通带平坦、抗混叠好；linear 高频失真较大但零延迟（耳返场景选它）。
            </n-text>
          </div>
          <n-select
            v-model:value="app.settings.resample_quality"
            :options="[
              { label: '高质量 sinc（延迟 ~7ms）', value: 'sinc256' },
              { label: '轻量 sinc（延迟 ~3.5ms）', value: 'sinc128' },
              { label: 'linear（零延迟，耳返用）', value: 'linear' },
            ]"
            style="width: 230px"
            @update:value="save"
          />
        </div>
        <n-divider />
        <div class="item-row">
          <div style="flex: 1">
            <div class="item-title">边缓冲容量</div>
            <n-text depth="3" style="font-size: 12px">
              50–1000ms。加大更抗卡顿（引擎/系统卡住时积缓冲而不丢音频），
              不影响日常延迟；调小则卡顿上限更低。修改后路由边缓冲立即重建。
            </n-text>
          </div>
          <n-input-number
            v-model:value="app.settings.edge_buffer_ms"
            :min="50"
            :max="1000"
            :step="50"
            style="width: 130px"
            @update:value="save"
          >
            <template #suffix>ms</template>
          </n-input-number>
        </div>
      </n-card>

      <n-card title="后台运行" size="small">
        <div class="item-row">
          <div style="flex: 1">
            <div class="item-title">关闭窗口 = 最小化到托盘</div>
            <n-text depth="3" style="font-size: 12px">
              引擎继续在后台混音，托盘左键单击可重新打开窗口。
            </n-text>
          </div>
          <n-switch v-model:value="app.settings.close_to_tray" @update:value="save" />
        </div>
        <n-divider />
        <div class="item-row">
          <div style="flex: 1">
            <div class="item-title">开机自启</div>
            <n-text depth="3" style="font-size: 12px">写入当前用户注册表 Run 键，无需管理员权限。</n-text>
          </div>
          <n-switch :value="app.autostart" @update:value="toggleAutostart" />
        </div>
        <n-divider />
        <div class="item-row">
          <div style="flex: 1">
            <div class="item-title">自启时进入无窗口模式（--headless）</div>
            <n-text depth="3" style="font-size: 12px">
              开机后仅驻留托盘后台混音，不显示主窗口。
            </n-text>
          </div>
          <n-switch v-model:value="app.settings.autostart_headless" @update:value="save" />
        </div>
      </n-card>

      <n-card title="远程控制 API" size="small">
        <div class="item-row">
          <div style="flex: 1">
            <div class="item-title">启用本地控制 API</div>
            <n-text depth="3" style="font-size: 12px">
              REST + SSE 接口，可在 headless 模式下查询设备、修改路由、读取电平。
            </n-text>
          </div>
          <n-switch v-model:value="app.settings.control_api.enabled" @update:value="save" />
        </div>
        <n-form-item label="端口" label-placement="left" style="margin-top: 12px; max-width: 220px">
          <n-input-number
            v-model:value="app.settings.control_api.port"
            :min="1024"
            :max="65535"
            @update:value="save"
          />
        </n-form-item>
        <div class="item-row">
          <n-tag size="small" :type="app.apiStatus.running ? 'success' : 'default'" :bordered="false">
            {{ app.apiStatus.running ? `运行中 ${app.apiStatus.addr}` : "未运行" }}
          </n-tag>
          <n-button v-if="app.apiStatus.addr" size="tiny" @click="copyAddr">复制地址</n-button>
        </div>
        <n-divider />
        <n-text depth="3" style="font-size: 12px; line-height: 1.9">
          示例：
          <code>GET /api/devices</code>、<code>GET /api/graph</code>、<code>PUT /api/graph</code>、
          <code>GET /api/status</code>、<code>GET /api/events</code>（SSE）
        </n-text>
      </n-card>

      <n-card title="关于" size="small">
        <n-text depth="3" style="font-size: 12px; line-height: 1.9">
          AudioMix v0.1.0 — Windows WASAPI 混音引擎 + Vue 前端。<br />
          虚拟声卡：usbip-win2（BSD-2）+ UAC2 描述符 → Windows 自带的 usbaudio2.sys 端点。
        </n-text>
        <n-divider />
        <n-popconfirm @positive-click="api.quitApp()">
          <template #trigger>
            <n-button size="small" type="error" ghost>退出 AudioMix（停止引擎）</n-button>
          </template>
          将停止所有混音流并退出进程，确定？
        </n-popconfirm>
      </n-card>
    </div>

    <!-- 右：运行日志（引擎 / 虚拟声卡 / 提权命令输出） -->
    <n-card size="small" class="log-card">
      <template #header>
        <div class="item-row">
          <span class="item-title">运行日志</span>
          <n-tag size="tiny" :bordered="false">{{ logs.length }} 行</n-tag>
        </div>
      </template>
      <template #header-extra>
        <div class="item-row">
          <n-switch v-model:value="follow" size="small">
            <template #checked>跟随</template>
            <template #unchecked>暂停</template>
          </n-switch>
          <n-button size="tiny" @click="copyLogs">复制</n-button>
          <n-button size="tiny" @click="clearLogs">清空</n-button>
        </div>
      </template>
      <div ref="logBox" class="log-box" @scroll="onLogScroll" @mouseup="onLogMouseUp">
        <div v-for="l in logs" :key="l.seq" class="log-line" :class="logClass(l.text)">
          <span class="log-seq">{{ l.seq }}</span>
          <span class="log-text">{{ l.text }}</span>
        </div>
        <div v-if="!logs.length" class="log-empty">
          暂无日志（引擎启动、设备变化、USB/IP 附加、提权命令输出都会记在这里）
        </div>
      </div>
    </n-card>
  </div>
</template>

<style scoped>
.settings-grid {
  display: grid;
  grid-template-columns: minmax(340px, 1fr) minmax(420px, 1.15fr);
  gap: 14px;
  align-items: start;
}
.log-card {
  min-width: 0;
}
.log-box {
  height: calc(100vh - 236px);
  min-height: 260px;
  overflow: auto;
  padding: 6px 8px;
  border: 1px solid rgba(255, 255, 255, 0.12);
  border-radius: 8px;
  background: #131317;
  font-family: "Cascadia Mono", Consolas, "Courier New", monospace;
  font-size: 11.5px;
  line-height: 1.55;
  user-select: text;
  cursor: text;
}
.log-line {
  display: flex;
  gap: 8px;
  white-space: pre-wrap;
  word-break: break-all;
}
.log-seq {
  flex: 0 0 44px;
  text-align: right;
  opacity: 0.35;
}
.log-text {
  flex: 1;
  min-width: 0;
}
.log-error .log-text {
  color: #ff7875;
}
.log-warn .log-text {
  color: #f0a020;
}
.log-debug .log-text {
  opacity: 0.55;
}
.log-empty {
  opacity: 0.5;
  padding: 8px;
}
</style>
