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

const RATE_OPTIONS = [44100, 48000, 88200, 96000, 176400, 192000].map((v) => ({
  label: `${v / 1000} kHz`,
  value: v,
}));
const BIT_OPTIONS = [16, 24, 32].map((v) => ({ label: `${v} bit`, value: v }));

/** 内置线路（UAC1 全速）的规格上限：每毫秒 ≤1023 字节，且 >96kHz 只给 16bit */
const MAX_BYTES_PER_MS = 1023;
const MULTIBIT_MAX_RATE = 96000;

/** 每毫秒 PCM 字节数：按整数个采样帧向上取整（与后端端点包长同规则；UAC1 全速上限 1023） */
function bytesPerMs(c: UsbIpCable): number {
  return Math.ceil(c.sample_rate / 1000) * 2 * (c.bits / 8);
}

/** 该格式内置线路是否支持（与后端 UsbIpCableSettings::is_supported 同规则） */
function formatSupported(c: UsbIpCable): boolean {
  return !(c.sample_rate > MULTIBIT_MAX_RATE && c.bits !== 16) && bytesPerMs(c) <= MAX_BYTES_PER_MS;
}

/** 采样率下拉：当前位深为 24/32bit 时，>96kHz 的档位不可选 */
function rateOptionsFor(c: UsbIpCable) {
  return RATE_OPTIONS.map((o) => ({
    ...o,
    disabled: c.bits !== 16 && o.value > MULTIBIT_MAX_RATE,
    title: c.bits !== 16 && o.value > MULTIBIT_MAX_RATE ? "内置线路在 96kHz 以上只提供 16bit" : undefined,
  }));
}

/** 位深下拉：当前采样率 >96kHz 时，24/32bit 不可选 */
function bitOptionsFor(c: UsbIpCable) {
  return BIT_OPTIONS.map((o) => ({
    ...o,
    disabled: c.sample_rate > MULTIBIT_MAX_RATE && o.value !== 16,
    title: c.sample_rate > MULTIBIT_MAX_RATE && o.value !== 16 ? "内置线路在 96kHz 以上只提供 16bit" : undefined,
  }));
}

/** 格式不支持时的提示 */
function formatHint(c: UsbIpCable): string {
  if (c.sample_rate > MULTIBIT_MAX_RATE && c.bits !== 16) {
    return `内置线路在 ${MULTIBIT_MAX_RATE / 1000}kHz 以上只提供 16bit；需要 ${c.sample_rate / 1000}kHz/${c.bits}bit 请自装第三方虚拟声卡（如 VB-CABLE）`;
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
  return s;
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
  <n-card title="虚拟声卡（usbip-win2 + UAC1）" size="small">
    <!-- 传输驱动安装状态 -->
    <n-alert
      v-if="status && !driverReady"
      type="warning"
      title="未检测到 usbip.exe（USB/IP 传输驱动）"
      style="margin-bottom: 12px"
    >
      虚拟声卡由 <b>usbip-win2</b>（BSD-2，微软签名驱动，内存完整性 HVCI 兼容）把应用内置的
      USB/IP 服务器仿真的设备接入 Windows，再由系统自带的 usbaudio.sys（UAC1）暴露为标准播放/录音端点。
      <template v-if="status.driver.installer_path">
        已随包提供安装包，点击下面按钮一键安装（需要管理员权限，安装过程会短暂重启 USB 集线器）。
      </template>
      <template v-else>未找到随包安装包（resources/drivers/usbip/），请确认安装包完整。</template>
      <div style="margin-top: 8px; display: flex; gap: 10px; align-items: center">
        <n-button
          type="primary"
          size="small"
          :loading="busy"
          :disabled="!status.driver.installer_path"
          @click="installDriver"
        >
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

    <n-alert v-else-if="status" type="success" style="margin-bottom: 12px">
      传输驱动已就绪：<code>{{ status.driver.usbip_path }}</code>
    </n-alert>

    <!-- 服务器开关/状态 -->
    <div class="item-row" style="margin-bottom: 10px">
      <div style="flex: 1">
        <div class="item-title">启用虚拟声卡服务器</div>
        <n-text depth="3" style="font-size: 12px">
          监听 {{ status?.bind ?? "127.0.0.1:3240" }}（仅本机）；保存线路/格式/接线时服务器保持监听（不断开已接入设备），改完会自动重新附加。
        </n-text>
      </div>
      <n-switch :value="enabled" :loading="busy" @update:value="toggleEnabled" />
    </div>
    <div style="display: flex; align-items: center; gap: 10px; margin-bottom: 16px">
      <n-tag size="small" :type="status?.running ? 'success' : 'default'" :bordered="false">
        {{ status?.running ? `运行中 ${status.local_addr ?? ""}` : "未运行" }}
      </n-tag>
      <n-tag size="small" :type="status?.cables.length ? 'info' : 'default'" :bordered="false">
        线路 {{ status?.cables.length ?? 0 }} 条 / 已接入 {{ attachedCount }}
      </n-tag>
      <n-button size="tiny" @click="load">刷新状态</n-button>
    </div>

    <n-alert v-if="needsReattach" type="info" style="margin-bottom: 12px">
      线路配置已更改，系统里旧的虚拟设备需要重新接入 —— 请点「附加全部」。
    </n-alert>

    <n-divider style="margin: 4px 0 12px" />

    <!-- 线路列表：名称 / 格式 / 接线 -->
    <div class="section-title">虚拟线路（1–32 条，格式独立可配）</div>
    <n-text depth="3" style="font-size: 12px; display: block; margin-bottom: 10px; line-height: 1.8">
      线路两端对应 Windows 里的两个端点：<br />
      · <b>线路输入</b> ＝ 系统<b>录音</b>端（麦克风）——混音器写到这里，别的软件从「Virtual Cable NN 麦克风」录；<br />
      · <b>线路输出</b> ＝ 系统<b>播放</b>端（扬声器）——别的软件选「Virtual Cable NN 扬声器」播放，声音从这进混音器。<br />
      内置线路是 <b>UAC1（USB 1.1 全速，系统自带 usbaudio.sys）</b>，格式上限：
      <b>44.1–96 kHz 的 16/24/32bit</b>，<b>176.4/192 kHz 只支持 16bit</b>。<br />
      需要更高规格（例如 192kHz/24bit）或更低延迟的线路，请自行安装第三方虚拟声卡（VB-CABLE、VoiceMeeter 等）——
      它们会作为普通 Windows 端点出现在混音画布左侧设备列表里，直接拖进来接线即可。<br />
      内部拷贝方向：<b>直接点接线图上的箭头</b>选（点中间那条线也可以循环切换；再点一次箭头即取消）——
      <b>线路输出 → 拷贝到 → 线路输入</b>（播放端的声音原样出现在录音端）、
      <b>线路输入 → 拷贝到 → 线路输出</b>（写进录音端的数据回灌到播放端）、或都不点＝<b>不拷贝</b>（只走混音图）。
      改完点「保存」即生效。
    </n-text>

    <n-list v-if="cables.length" :show-divider="false" style="margin-bottom: 10px">
      <n-list-item v-for="c in cables" :key="c.number">
        <div class="cable-row">
          <!-- 第一行：身份与格式（名称/采样率/位深/带宽告警/接入状态/删除） -->
          <div class="cable-row-line">
            <n-tag size="small" type="warning" :bordered="false">{{ String(c.number).padStart(2, "0") }}</n-tag>
            <n-input
              v-model:value="c.name"
              size="small"
              style="width: 190px"
              :placeholder="`Virtual Cable ${String(c.number).padStart(2, '0')}`"
              maxlength="64"
              clearable
              @update:value="dirty = true"
            />
            <n-select
              v-model:value="c.sample_rate"
              :options="rateOptionsFor(c)"
              size="small"
              style="width: 112px"
              @update:value="dirty = true"
            />
            <n-select
              v-model:value="c.bits"
              :options="bitOptionsFor(c)"
              size="small"
              style="width: 92px"
              @update:value="dirty = true"
            />
            <n-tag
              v-if="!formatSupported(c)"
              size="small"
              type="error"
              :bordered="false"
              :title="formatHint(c)"
            >
              超出内置线路规格
            </n-tag>
            <n-tag
              size="small"
              :type="attachedOf(c.number)?.attached ? 'success' : 'default'"
              :bordered="false"
            >
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
            <div
              class="patch"
              :title="
                copyMode(c) === 'in2out'
                  ? '线路输入 → 拷贝到 → 线路输出：混音器写进录音端的数据同时回灌到播放端'
                  : copyMode(c) === 'out2in'
                    ? '线路输出 → 拷贝到 → 线路输入：系统播放进这条线路的声音原样出现在系统录音端（混音图的「线路输出」能读到）'
                    : '不拷贝：线路输入只输出混音图路由过来的信号'
              "
            >
            <div class="patch-box">
              <div class="patch-label">线路输入</div>
              <div class="patch-sub">系统录音端（麦克风）</div>
            </div>

            <div class="patch-mid">
              <!-- 两条虚线箭头：同一 x 范围、同一虚线相位（对齐）；点右向＝输入→输出，点左向＝输出→输入 -->
              <svg class="patch-arrows" width="160" height="46" viewBox="0 0 160 46">
                <defs>
                  <marker id="pa-r" viewBox="0 0 10 10" refX="10" refY="5" markerWidth="7" markerHeight="7" orient="auto">
                    <path d="M 0 0 L 10 5 L 0 10 z" fill="#4b9cd3" />
                  </marker>
                  <marker
                    id="pa-l"
                    viewBox="0 0 10 10"
                    refX="10"
                    refY="5"
                    markerWidth="7"
                    markerHeight="7"
                    orient="auto-start-reverse"
                  >
                    <path d="M 0 0 L 10 5 L 0 10 z" fill="#4b9cd3" />
                  </marker>
                  <marker id="pa-dim" viewBox="0 0 10 10" refX="10" refY="5" markerWidth="7" markerHeight="7" orient="auto">
                    <path d="M 0 0 L 10 5 L 0 10 z" fill="rgba(255,255,255,0.32)" />
                  </marker>
                  <marker
                    id="pa-dim-l"
                    viewBox="0 0 10 10"
                    refX="10"
                    refY="5"
                    markerWidth="7"
                    markerHeight="7"
                    orient="auto-start-reverse"
                  >
                    <path d="M 0 0 L 10 5 L 0 10 z" fill="rgba(255,255,255,0.32)" />
                  </marker>
                </defs>

                <!-- 右向：线路输入 → 线路输出（＝ 录音端回灌到播放端） -->
                <g class="arrow-g" @click.stop="setCopy(c, 'in2out')">
                  <title>线路输入 → 拷贝到 → 线路输出（再点一次取消）</title>
                  <line class="hit" x1="16" y1="13" x2="144" y2="13" />
                  <line
                    class="arrow-line"
                    :class="{ active: copyMode(c) === 'in2out' }"
                    x1="16"
                    y1="13"
                    x2="144"
                    y2="13"
                    :marker-end="copyMode(c) === 'in2out' ? 'url(#pa-r)' : 'url(#pa-dim)'"
                  />
                </g>

                <!-- 左向：线路输出 → 线路输入（同样的 x 起止，箭头画在左端） -->
                <g class="arrow-g" @click.stop="setCopy(c, 'out2in')">
                  <title>线路输出 → 拷贝到 → 线路输入（再点一次取消）</title>
                  <line class="hit" x1="16" y1="33" x2="144" y2="33" />
                  <line
                    class="arrow-line"
                    :class="{ active: copyMode(c) === 'out2in' }"
                    x1="16"
                    y1="33"
                    x2="144"
                    y2="33"
                    :marker-start="copyMode(c) === 'out2in' ? 'url(#pa-l)' : 'url(#pa-dim-l)'"
                  />
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
    <n-text v-else depth="3" style="display: block; font-size: 13px; margin-bottom: 10px">
      尚未添加线路。线路名会写进 USB 产品字符串——Windows 声音设置里显示的就是它。
    </n-text>

    <div style="display: flex; gap: 10px; align-items: center; margin-bottom: 16px; flex-wrap: wrap">
      <n-button size="small" :disabled="cables.length >= 32" @click="addCable">＋ 添加线路</n-button>
      <n-button size="small" type="primary" :loading="busy" :disabled="!dirty" @click="saveCables">
        保存
      </n-button>
      <n-text v-if="dirty" depth="3" style="font-size: 12px">
        有未保存的改动 —— 点「保存」后立即生效（会自动重新附加，弹一次 UAC）
      </n-text>
      <n-text v-else depth="3" style="font-size: 12px">
        当前：{{ cables.map((c) => cableLabel(c)).join("、") || "无线路" }}
      </n-text>
    </div>

    <n-divider style="margin: 8px 0 14px" />

    <!-- 接入系统 -->
    <div class="section-title">接入系统（usbip attach，需管理员权限）</div>
    <n-text depth="3" style="display: block; font-size: 12px; margin-bottom: 8px">
      「附加全部」先断开所有已接入端口，再按当前线路逐条附加（**只弹一次 UAC**）。
      附加后设备出现在系统声音设置与本应用设备列表（可能需要几秒枚举）。
    </n-text>
    <div style="display: flex; gap: 10px; margin-bottom: 12px">
      <n-button
        type="primary"
        size="small"
        :loading="busy"
        :disabled="!driverReady || !status?.running || !cables.length"
        @click="attachAll"
      >
        附加全部（需管理员）
      </n-button>
      <n-button size="small" :loading="busy" :disabled="!driverReady || !attachedCount" @click="detachAll">
        断开全部
      </n-button>
    </div>

    <n-alert v-if="status?.ports_error" type="default" style="margin-bottom: 12px">
      端口状态不可用：{{ status.ports_error }}
    </n-alert>
    <n-list v-if="status?.ports.length" :show-divider="false" style="margin-bottom: 12px">
      <n-list-item v-for="p in status.ports" :key="p.port">
        <div class="item-row">
          <n-tag size="small" type="success" :bordered="false">端口 {{ p.port }}</n-tag>
          <div style="flex: 1; white-space: pre-wrap; font-size: 12px; opacity: 0.85">{{ p.detail }}</div>
        </div>
      </n-list-item>
    </n-list>

    <n-alert v-if="report && report.failed.length" type="error" style="margin-bottom: 12px">
      以下线路附加失败：<span v-for="[bus, why] in report.failed" :key="bus">{{ bus }}（{{ why }}） </span>
    </n-alert>
    <n-text
      v-if="lastLog"
      depth="3"
      style="display: block; font-size: 12px; white-space: pre-wrap; margin-bottom: 8px"
    >
      {{ lastLog }}
    </n-text>

    <n-text depth="3" style="display: block; font-size: 12px; line-height: 1.8; margin-top: 8px">
      混音页用法：设备面板里每条线路拆成两个可分别拖入的节点 ——
      <b>线路输入</b>（系统录音端，混音图写到这）和 <b>线路输出</b>（系统播放端，混音图当源用）。
      所有声卡设备（含第三方虚拟声卡）也都能直接拖进画布：自装的虚拟声卡（VB-CABLE 等）就是走这条路接入混音的。
    </n-text>
  </n-card>
</template>

<style scoped>
/* 线路卡片两行布局：上面一行身份/格式/状态，下面一行拷贝方向接线图 */
.cable-row {
  display: flex;
  flex-direction: column;
  gap: 8px;
  width: 100%;
}
.cable-row-line {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
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
/* 细虚线箭头：选中（active）变实线蓝；未选中是灰色虚线 */
.arrow-line {
  stroke: rgba(255, 255, 255, 0.3);
  stroke-width: 1.6;
  stroke-dasharray: 6 5;
  fill: none;
}
.arrow-line.active {
  stroke: #4b9cd3;
  stroke-width: 2.2;
  stroke-dasharray: none;
}
.arrow-g:hover .arrow-line {
  stroke: #7cc0ea;
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
  border: 1px solid rgba(255, 255, 255, 0.18);
  border-radius: 6px;
  background: #1f1f23;
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
.patch-wire {
  display: block;
  cursor: pointer;
}
.wire {
  fill: none;
  stroke: #4b9cd3;
  stroke-width: 2.5;
}
.wire.off {
  stroke: rgba(255, 255, 255, 0.25);
  stroke-dasharray: 5 4;
}
.wire-hint {
  font-size: 10px;
  fill: rgba(255, 255, 255, 0.55);
}
</style>
