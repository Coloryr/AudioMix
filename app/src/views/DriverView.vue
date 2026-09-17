<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import {
  NAlert,
  NButton,
  NCard,
  NDivider,
  NInput,
  NList,
  NListItem,
  NPopconfirm,
  NSelect,
  NSwitch,
  NTag,
  NText,
  useMessage,
} from "naive-ui";
import {
  api,
  type AttachReport,
  type UsbIpCable,
  type UsbIpCableMode,
  type UsbIpStatus,
} from "../api";
import { useApp } from "../store";

const app = useApp();
const message = useMessage();

const status = ref<UsbIpStatus | null>(null);
const cables = ref<UsbIpCable[]>([]);
const enabled = ref(false);
const busy = ref(false);
/** 配置有未保存改动 */
const dirty = ref(false);
/** 线路配置变过但还没重新附加 */
const needsReattach = ref(false);
const report = ref<AttachReport | null>(null);
const lastLog = ref("");

const RATE_OPTIONS = [44100, 48000, 88200, 96000].map((v) => ({
  label: `${v / 1000} kHz`,
  value: v,
}));
const BIT_OPTIONS = [16, 24, 32].map((v) => ({ label: `${v} bit`, value: v }));

/** 内置线路（UAC1 全速）的规格上限：每毫秒 ≤1023 字节；采样率 ≤96kHz（实测更高时主机侧 ISO OUT 无法稳定承载） */
const MAX_BYTES_PER_MS = 1023;
const MAX_RATE = 96000;
/** 88.2k 以上只支持 16bit：loopback/reverse 的 OUT+IN 共享全速帧预算，96k/24 双向 = 1152 B/ms 会断流 */
const MULTIBIT_MAX_RATE = 88200;

/** 每毫秒 PCM 字节数：按整数个采样帧向上取整（与后端端点包长同规则；UAC1 全速上限 1023） */
function bytesPerMs(c: UsbIpCable): number {
  return Math.ceil(c.sample_rate / 1000) * 2 * (c.bits / 8);
}

/** 位深选项：88.2k 以上（96k 档）只有 16bit（与后端 MULTIBIT_MAX_RATE 同规则） */
function bitOptionsOf(c: UsbIpCable) {
  return c.sample_rate > MULTIBIT_MAX_RATE ? BIT_OPTIONS.filter((o) => o.value === 16) : BIT_OPTIONS;
}

/** 切采样率：跳到 96k 档时位深自动降为 16bit */
function onRateChange(c: UsbIpCable, rate: number) {
  c.sample_rate = rate;
  if (rate > MULTIBIT_MAX_RATE) c.bits = 16;
  dirty.value = true;
}

/** 该格式内置线路是否支持（与后端 UsbIpCableSettings::is_supported 同规则） */
function formatSupported(c: UsbIpCable): boolean {
  return (
    c.sample_rate <= MAX_RATE &&
    (c.sample_rate <= MULTIBIT_MAX_RATE || c.bits === 16) &&
    bytesPerMs(c) <= MAX_BYTES_PER_MS
  );
}

/** 格式不支持时的提示 */
function formatHint(c: UsbIpCable): string {
  if (c.sample_rate > MAX_RATE) {
    return `内置线路最高 ${MAX_RATE / 1000}kHz；需要 ${c.sample_rate / 1000}kHz 请自装第三方虚拟声卡（如 VB-CABLE）`;
  }
  if (c.sample_rate > MULTIBIT_MAX_RATE && c.bits !== 16) {
    return `${c.sample_rate / 1000}kHz 只支持 16bit（全速 USB 双向带宽限制）`;
  }
  return `内置线路（UAC1 全速）每毫秒上限 ${MAX_BYTES_PER_MS} 字节，该格式需要 ${bytesPerMs(c)} 字节`;
}

const driverReady = computed(() => status.value?.driver.installed === true);
const attachedCount = computed(() => status.value?.cables.filter((c) => c.attached).length ?? 0);

async function load() {
  const s = await api.usbipStatus();
  status.value = s;
  enabled.value = s.enabled;
  cables.value = s.cables.map((c) => ({
    number: c.number,
    name: c.name,
    sample_rate: c.sample_rate,
    bits: c.bits,
    mode: c.mode as UsbIpCableMode,
    buffer_ms: c.buffer_ms,
  }));
  dirty.value = false;
}

async function guarded(fn: () => Promise<void>) {
  busy.value = true;
  try {
    await fn();
  } catch (e) {
    message.error(String(e));
  } finally {
    busy.value = false;
  }
}

function addCable() {
  if (cables.value.length >= 32) {
    message.warning("最多 32 条虚拟线路");
    return;
  }
  const used = new Set(cables.value.map((c) => c.number));
  let n = 1;
  while (used.has(n)) n++;
  cables.value.push({
    number: n,
    name: "",
    sample_rate: 48000,
    bits: 16,
    mode: "loopback",
    buffer_ms: 250,
  });
  dirty.value = true;
}

function removeCable(number: number) {
  cables.value = cables.value.filter((c) => c.number !== number);
  dirty.value = true;
}

/** 线路显示名（空名回退 Virtual Cable NN） */
function cableLabel(c: UsbIpCable): string {
  return c.name.trim() || `Virtual Cable ${String(c.number).padStart(2, "0")}`;
}

/**
 * 拷贝方向（三种）：none=不拷贝。
 * 语义（按用户的说法）：**线路输入 ＝ Windows 录制端（麦克风）**，**线路输出 ＝ Windows 播放端（扬声器）**。
 * 所以「线路输入 → 线路输出」= 录进来的数据回灌到播放端 = `reverse`；
 * 「线路输出 → 线路输入」= 系统播进这条线路的声音原样出现在录音端 = `loopback`。
 */
function copyMode(c: UsbIpCable): "none" | "in2out" | "out2in" {
  if (c.mode === "reverse") return "in2out";
  if (c.mode === "loopback") return "out2in";
  return "none";
}

function copyLabel(c: UsbIpCable): string {
  return copyMode(c) === "in2out" ? "输入→输出" : copyMode(c) === "out2in" ? "输出→输入" : "不拷贝";
}

/** 点箭头：选中该方向；再点一次取消（回到不拷贝） */
function setCopy(c: UsbIpCable, dir: "in2out" | "out2in") {
  c.mode = copyMode(c) === dir ? "mixer" : dir === "in2out" ? "reverse" : "loopback";
  dirty.value = true;
}

function attachedOf(number: number) {
  return status.value?.cables.find((s) => s.number === number) ?? null;
}

/**
 * 保存并生效：服务器已在运行时只换线缆表（不重绑端口），
 * 然后**自动重新附加**（只弹一次 UAC），让改名 / 改格式 / 改接线立刻反映到 Windows。
 */
async function saveCables() {
  await guarded(async () => {
    const bad = cables.value.filter((c) => !formatSupported(c));
    if (bad.length) {
      message.error(
        `线路 ${bad.map((c) => c.number).join("、")} 的格式超出内置线路规格：${bad
          .map((c) => formatHint(c))
          .join("；")}`,
      );
      return;
    }
    const s = await api.usbipSetCables(enabled.value, cables.value);
    status.value = s;
    enabled.value = s.enabled;
    dirty.value = false;
    app.settings.usbip = { enabled: s.enabled, bind: s.bind, cables: cables.value };
    await app.refreshDevices();

    const shouldAttach = s.enabled && cables.value.length > 0 && s.driver.installed;
    if (!shouldAttach) {
      needsReattach.value = s.enabled && cables.value.length > 0;
      message.success(
        s.enabled
          ? `已保存 ${cables.value.length} 条线路（安装驱动后点「附加全部」）`
          : "已保存（虚拟声卡服务器停止）",
      );
      return;
    }

    message.info("配置已保存，正在重新附加到系统（请确认 UAC）…");
    const r = await api.usbipAttachAll();
    report.value = r;
    lastLog.value = r.log;
    needsReattach.value = false;
    if (r.failed.length) message.error(`已保存，但 ${r.failed.length} 条线路附加失败`);
    else message.success(`已保存并附加 ${r.attached.length} 条线路，改动已生效`);
    await load();
    await app.refreshDevices();
  });
}

async function toggleEnabled(v: boolean) {
  enabled.value = v;
  await saveCables();
}

async function installDriver() {
  await guarded(async () => {
    const out = await api.usbipInstallDriver();
    lastLog.value = out;
    message.success("USB/IP 驱动安装完成");
    await load();
  });
}

async function attachAll() {
  await guarded(async () => {
    const r = await api.usbipAttachAll();
    report.value = r;
    lastLog.value = r.log;
    needsReattach.value = false;
    if (r.failed.length) message.error(`${r.failed.length} 条线路附加失败`);
    else message.success(`已附加 ${r.attached.length} 条线路`);
    await load();
    await app.refreshDevices();
  });
}

async function detachAll() {
  await guarded(async () => {
    lastLog.value = await api.usbipDetachAll();
    needsReattach.value = false;
    message.success("已断开全部虚拟线路");
    await load();
    await app.refreshDevices();
  });
}

onMounted(() => {
  guarded(load);
});
</script>

<template>
  <!-- 底部留白：滚到底时卡片不贴窗口底边（pane 本身不加 padding，只给内容根加） -->
  <n-card title="虚拟声卡（usbip-win2 + UAC1）" size="small" style="margin-bottom: 12px">
    <!-- 传输驱动安装状态 -->
    <n-alert v-if="status && !driverReady" type="warning" title="未检测到 usbip.exe（USB/IP 传输驱动）" class="block">
      虚拟声卡由 <b>usbip-win2</b>（BSD-2，微软签名驱动，内存完整性 HVCI 兼容）把应用内置的
      USB/IP 服务器仿真的设备接入 Windows，再由系统自带的 usbaudio.sys（UAC1）暴露为标准播放/录音端点。
      <template v-if="status.driver.installer_path">
        已随包提供安装包，点击下面按钮一键安装（需要管理员权限，安装过程会短暂重启 USB 集线器）。
      </template>
      <template v-else>未找到随包安装包（resources/drivers/usbip/），请确认安装包完整。</template>
      <div style="margin-top: 8px; display: flex; gap: 10px; align-items: center">
        <n-button type="primary" size="small" :loading="busy" :disabled="!status.driver.installer_path"
          @click="installDriver">
          安装 USB/IP 驱动（管理员）
        </n-button>
        <n-button size="small" @click="load">刷新</n-button>
      </div>
      <div v-if="status.driver.installer_path" style="margin-top: 6px; font-size: 12px; opacity: 0.75">
        安装包：{{ status.driver.installer_path }}
      </div>
      <div style="margin-top: 6px; font-size: 12px; opacity: 0.75">
        内存完整性（HVCI）：{{ status.driver.hvci_enabled === null ? "未知" : status.driver.hvci_enabled ? "已开启" : "已关闭" }} ·
        测试签名：{{ status.driver.test_signing === null ? "未知（读取需管理员）" : status.driver.test_signing ? "已开启" : "未开启" }}
      </div>
    </n-alert>

    <n-alert v-else-if="status" type="success" class="block">
      传输驱动已就绪：<code>{{ status.driver.usbip_path }}</code>
    </n-alert>

    <!-- 服务器开关/状态 -->
    <div class="item-row block">
      <div style="flex: 1">
        <div class="item-title">启用虚拟声卡服务器</div>
        <n-text depth="3" style="font-size: 12px">
          监听 {{ status?.bind ?? "127.0.0.1:3240" }}（仅本机）；保存线路/格式/接线时服务器保持监听（不断开已接入设备），改完会自动重新附加。
        </n-text>
      </div>
      <n-switch :value="enabled" :loading="busy" @update:value="toggleEnabled" />
    </div>
    <div class="item-row block">
      <n-tag size="small" :type="status?.running ? 'success' : 'default'" :bordered="false">
        {{ status?.running ? `运行中 ${status.local_addr ?? ""}` : "未运行" }}
      </n-tag>
      <n-tag size="small" :type="status?.cables.length ? 'info' : 'default'" :bordered="false">
        线路 {{ status?.cables.length ?? 0 }} 条 / 已接入 {{ attachedCount }}
      </n-tag>
      <n-button size="tiny" @click="load">刷新状态</n-button>
    </div>

    <n-alert v-if="needsReattach" type="info" class="block">
      线路配置已更改，系统里旧的虚拟设备需要重新接入 —— 请点「附加全部」。
    </n-alert>

    <n-divider class="divider" />

    <!-- 线路列表：名称 / 格式 / 接线 -->
    <div class="section-title">虚拟线路（1–32 条，格式独立可配）</div>
    <n-text depth="3" style="font-size: 12px; display: block; margin-bottom: 10px; line-height: 1.8">
      每条线路对应 Windows 里的两个端点：<b>线路输入</b>＝录音端（麦克风，混音器写到这里）、<b>线路输出</b>＝播放端（扬声器，别的软件播放的声音从这进混音器）。<br />
      内置线路为 UAC1 全速：44.1/48/88.2 kHz 支持 16/24/32bit，96 kHz 只支持 16bit；
      更高规格或更低延迟请自装第三方虚拟声卡（VB-CABLE 等），同样能拖进混音画布。<br />
      内部拷贝方向直接点右侧接线图的箭头选择（再点一次取消）：
      输出→输入＝播放端的声音原样出现在录音端；输入→输出＝写进录音端的数据回灌到播放端。改完点「保存」即生效。
    </n-text>

    <n-list v-if="cables.length" :show-divider="false" class="block">
      <n-list-item v-for="c in cables" :key="c.number">
        <div class="cable-row">
          <!-- 第一行：身份与格式（名称/采样率/位深/带宽告警/接入状态/删除） -->
          <div class="cable-row-line">
            <n-tag size="small" type="warning" :bordered="false">{{ String(c.number).padStart(2, "0") }}</n-tag>
            <n-input v-model:value="c.name" size="small" style="width: 190px"
              :placeholder="`Virtual Cable ${String(c.number).padStart(2, '0')}`" maxlength="64" clearable
              @update:value="dirty = true" />
            <n-select v-model:value="c.sample_rate" :options="RATE_OPTIONS" size="small" style="width: 112px"
              @update:value="(v: number) => onRateChange(c, v)" />
            <n-select v-model:value="c.bits" :options="bitOptionsOf(c)" size="small" style="width: 92px"
              @update:value="dirty = true" />
            <n-tag v-if="!formatSupported(c)" size="small" type="error" :bordered="false" :title="formatHint(c)">
              超出内置线路规格
            </n-tag>
            <n-tag size="small" :type="attachedOf(c.number)?.attached ? 'success' : 'default'" :bordered="false">
              {{ attachedOf(c.number)?.attached ? `已接入（端口 ${attachedOf(c.number)?.port}）` : "未接入" }}
            </n-tag>
            <n-popconfirm @positive-click="removeCable(c.number)">
              <template #trigger>
                <n-button size="tiny" quaternary type="error">删除</n-button>
              </template>
              删除该线路后需要重新保存并附加，确定？
            </n-popconfirm>
          </div>

          <!-- 第二行：线路内部拷贝方向接线图（直接点箭头选，不用下拉框） -->
          <div class="cable-row-line">
            <div class="patch" :title="copyMode(c) === 'in2out'
              ? '线路输入 → 拷贝到 → 线路输出：混音器写进录音端的数据同时回灌到播放端'
              : copyMode(c) === 'out2in'
                ? '线路输出 → 拷贝到 → 线路输入：系统播放进这条线路的声音原样出现在系统录音端（混音图的「线路输出」能读到）'
                : '不拷贝：线路输入只输出混音图路由过来的信号'
              ">
              <div class="patch-box">
                <div class="patch-label">线路输入</div>
                <div class="patch-sub">系统录音端（麦克风）</div>
              </div>

              <div class="patch-mid">
                <!-- 两条虚线箭头：同一 x 范围、同一虚线相位（对齐）；点右向＝输入→输出，点左向＝输出→输入 -->
                <svg class="patch-arrows" width="160" height="46" viewBox="0 0 160 46">
                  <!-- 右向：线路输入 → 线路输出（＝ 录音端回灌到播放端）；三角用 polygon 画在线之后，保证盖在虚线上面 -->
                  <g class="arrow-g" @click.stop="setCopy(c, 'in2out')">
                    <title>线路输入 → 拷贝到 → 线路输出（再点一次取消）</title>
                    <line class="hit" x1="16" y1="13" x2="144" y2="13" />
                    <line class="arrow-line" :class="{ active: copyMode(c) === 'in2out' }" x1="16" y1="13" x2="133"
                      y2="13" />
                    <polygon class="arrow-head" :class="{ active: copyMode(c) === 'in2out' }"
                      points="144,13 133,8.5 133,17.5" />
                  </g>

                  <!-- 左向：线路输出 → 线路输入（与右向等长对称，箭头画在左端） -->
                  <g class="arrow-g" @click.stop="setCopy(c, 'out2in')">
                    <title>线路输出 → 拷贝到 → 线路输入（再点一次取消）</title>
                    <line class="hit" x1="16" y1="33" x2="144" y2="33" />
                    <line class="arrow-line" :class="{ active: copyMode(c) === 'out2in' }" x1="27" y1="33" x2="144"
                      y2="33" />
                    <polygon class="arrow-head" :class="{ active: copyMode(c) === 'out2in' }"
                      points="16,33 27,28.5 27,37.5" />
                  </g>
                </svg>
              </div>

              <div class="patch-box right">
                <div class="patch-label">线路输出</div>
                <div class="patch-sub">系统播放端（扬声器）</div>
              </div>
            </div>

            <n-tag size="small" :type="copyMode(c) === 'none' ? 'info' : 'success'" :bordered="false">
              {{ copyLabel(c) }}
            </n-tag>
          </div>
        </div>
      </n-list-item>
    </n-list>
    <n-text v-else depth="3" style="display: block; font-size: 13px" class="block">
      尚未添加线路。线路名会写进 USB 产品字符串——Windows 声音设置里显示的就是它。
    </n-text>

    <div class="item-row block">
      <n-button size="small" :disabled="cables.length >= 32" @click="addCable">＋ 添加线路</n-button>
      <n-button size="small" type="primary" :loading="busy" :disabled="!dirty" @click="saveCables">
        保存
      </n-button>
      <n-text v-if="dirty" depth="3" style="font-size: 12px">
        有未保存的改动 —— 点「保存」后立即生效（会自动重新附加，弹一次 UAC）
      </n-text>
      <n-text v-else depth="3" style="font-size: 12px">
        当前：{{cables.map((c) => cableLabel(c)).join("、") || "无线路"}}
      </n-text>
    </div>

    <n-divider class="divider" />

    <!-- 接入系统 -->
    <div class="section-title">接入系统（usbip attach，需管理员权限）</div>
    <n-text depth="3" style="display: block; font-size: 12px" class="block">
      「附加全部」先断开所有已接入端口，再按当前线路逐条附加（<b>只弹一次 UAC</b>）。
      附加后设备出现在系统声音设置与本应用设备列表（可能需要几秒枚举）。
    </n-text>
    <div class="item-row block">
      <n-button type="primary" size="small" :loading="busy"
        :disabled="!driverReady || !status?.running || !cables.length" @click="attachAll">
        附加全部（需管理员）
      </n-button>
      <n-button size="small" :loading="busy" :disabled="!driverReady || !attachedCount" @click="detachAll">
        断开全部
      </n-button>
    </div>

    <n-alert v-if="status?.ports_error" type="default" class="block">
      端口状态不可用：{{ status.ports_error }}
    </n-alert>
    <n-list v-if="status?.ports.length" :show-divider="false" class="block">
      <n-list-item v-for="p in status.ports" :key="p.port">
        <div class="item-row">
          <n-tag size="small" type="success" :bordered="false">端口 {{ p.port }}</n-tag>
          <div style="flex: 1; white-space: pre-wrap; font-size: 12px; opacity: 0.85">{{ p.detail }}</div>
        </div>
      </n-list-item>
    </n-list>

    <n-alert v-if="report && report.failed.length" type="error" class="block">
      以下线路附加失败：<span v-for="[bus, why] in report.failed" :key="bus">{{ bus }}（{{ why }}） </span>
    </n-alert>
    <!-- 附加/安装命令的原始输出：等宽字体、可选中复制 -->
    <div v-if="lastLog" class="log-box">{{ lastLog }}</div>

  </n-card>
</template>

<style scoped>
/* 垂直节奏：块与块之间统一 12px（以前 8/10/12/14/16 混用，视觉忽紧忽松） */
.block {
  margin-bottom: 12px;
}

/* 分隔线自身不再带上边距：上一块的 margin-bottom 已是 12px，避免叠出 20+ 的大空隙 */
.divider {
  margin: 0 0 12px;
}

/* 命令原始输出：等宽字体 + 内嵌底色，可选中复制 */
.log-box {
  padding: 6px 10px;
  border: 1px solid var(--border-weak);
  border-radius: 8px;
  background: var(--inset);
  font-family: "Cascadia Mono", Consolas, "Courier New", monospace;
  font-size: 11.5px;
  line-height: 1.55;
  white-space: pre-wrap;
  word-break: break-all;
  user-select: text;
  cursor: text;
}

/* 线路卡片两行布局：上面一行身份/格式/状态，下面一行拷贝方向接线图 */
.cable-row {
  display: flex;
  flex-direction: column;
  gap: 10px;
  width: 100%;
  /* 行间分隔：淡虚线比 n-list 默认分隔更轻 */
  padding: 10px 0;
  border-bottom: 1px dashed var(--border-weak);
}

.cable-row:last-child {
  border-bottom: none;
  padding-bottom: 2px;
}

.cable-row-line {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}

/* 接线图整行缩进一点，与上面的名称/格式行在视觉上分层 */
.cable-row-line:last-child {
  padding-left: 4px;
}

.patch {
  display: flex;
  align-items: center;
}

.patch-mid {
  display: flex;
  align-items: center;
}

.patch-arrows {
  display: block;
  overflow: visible;
}

.arrow-g {
  cursor: pointer;
}

/* 细虚线箭头：选中（active）变实线蓝；未选中是灰色虚线。
   箭头三角是同组内的 polygon，画在虚线之上，颜色也走 class，hover 时与线一起变色 */
.arrow-line {
  stroke: var(--wire-dim);
  stroke-width: 1.6;
  stroke-dasharray: 6 5;
  fill: none;
}

.arrow-line.active {
  stroke: var(--accent);
  stroke-width: 2.2;
  stroke-dasharray: none;
}

.arrow-head {
  fill: var(--wire-dim);
}

.arrow-head.active {
  fill: var(--accent);
}

.arrow-g:hover .arrow-line {
  stroke: var(--accent-hover);
}

.arrow-g:hover .arrow-head {
  fill: var(--accent-hover);
}

/* 透明加粗命中线：点起来更容易 */
.hit {
  stroke: transparent;
  stroke-width: 16;
  fill: none;
}

.patch-box {
  position: relative;
  padding: 3px 9px;
  border: 1px solid var(--border);
  border-radius: 6px;
  background: var(--surface);
  font-size: 11px;
  min-width: 104px;
  text-align: right;
}

.patch-box.right {
  text-align: left;
}

.patch-label {
  font-weight: 600;
}

.patch-sub {
  opacity: 0.6;
  font-size: 10px;
  white-space: nowrap;
}
</style>
