<script setup lang="ts">
// 系统默认设备卡片：改的是 Windows 的默认播放/录音设备（不是本应用的混音图）。
// 线路的真实扬声器/麦克风端点在这里改名为「XX（虚拟线路）」——默认播放选它，系统声音才进得来。
import { computed } from "vue";
import { NCard, NSelect, useMessage } from "naive-ui";
import { api, type DeviceInfo, type UsbIpCableStatus } from "../../api";
import { useApp } from "../../store";

const props = defineProps<{
  /** 线路运行态：用于把线路的 Windows 真实端点标成「XX（虚拟线路）」 */
  cables: UsbIpCableStatus[];
}>();

const app = useApp();
const message = useMessage();

/**
 * 该设备是否是某条线路的 **Windows 真实端点**（接入后 usbaudio.sys 创建的扬声器/麦克风）。
 * 这些端点和 usbip:// 合成端点指向同一条线路，设备面板里由线路节点统一代表，不再单独出现。
 * usbip:// 合成端点名字相同但 id 不同，不算真实端点。
 */
function realCableDevice(d: DeviceInfo): UsbIpCableStatus | null {
  if (d.id.startsWith("usbip://")) return null;
  return props.cables.find((c) => c.display_name && d.name.includes(c.display_name)) ?? null;
}

const defaultOut = computed(() => app.devices.find((d) => d.kind === "output" && d.is_default)?.id ?? null);
const defaultIn = computed(() => app.devices.find((d) => d.kind === "input" && d.is_default)?.id ?? null);

function options(kind: "input" | "output") {
  return app.devices
    .filter((d) => d.kind === kind && !d.id.startsWith("usbip://"))
    .map((d: DeviceInfo) => {
      const c = realCableDevice(d);
      if (c) return { label: `${c.display_name}（虚拟线路）`, value: d.id };
      return { label: d.name + (d.is_virtual ? "（虚拟）" : ""), value: d.id };
    });
}

async function setDefault(deviceId: string) {
  try {
    app.devices = await api.setDefaultDevice(deviceId);
    const name = app.devices.find((d) => d.id === deviceId)?.name ?? deviceId;
    message.success(`已把系统默认设备切换为「${name}」`);
  } catch (e) {
    message.error(String(e));
  }
}
</script>

<template>
  <n-card size="small" title="系统默认设备" style="margin-bottom: 14px">
    <div style="display: grid; grid-template-columns: 1fr 1fr; gap: 16px">
      <div>
        <div class="item-sub" style="margin-bottom: 6px">
          默认播放（把系统声音送进虚拟线路：选 Virtual Cable NN）
        </div>
        <n-select :value="defaultOut" :options="options('output')" size="small" filterable placeholder="选择默认播放设备"
          @update:value="setDefault" />
      </div>
      <div>
        <div class="item-sub" style="margin-bottom: 6px">
          默认录音（让应用从虚拟线路录音：选 Virtual Cable NN）
        </div>
        <n-select :value="defaultIn" :options="options('input')" size="small" filterable placeholder="选择默认录音设备"
          @update:value="setDefault" />
      </div>
    </div>
  </n-card>
</template>
