import { defineStore } from "pinia";
import { api, type DeviceInfo, type GraphConfig, type Settings, type ApiStatus } from "./api";

let idCounter = 0;
export const genId = (prefix: string) => `${prefix}-${Math.random().toString(36).slice(2, 10)}${idCounter++}`;

export const useApp = defineStore("app", {
  state: () => ({
    devices: [] as DeviceInfo[],
    graph: { sources: [], sinks: [], routes: [] } as GraphConfig,
    levels: {} as Record<string, number>,
    settings: {
      control_api: { enabled: false, bind: "127.0.0.1", port: 17643 },
      usbip: { enabled: false, bind: "127.0.0.1:3240", cables: [] },
      autostart_headless: true,
      close_to_tray: true,
    } as Settings,
    autostart: false,
    apiStatus: { running: false, addr: null } as ApiStatus,
    loading: false,
  }),
  getters: {
    inputDevices: (s) => s.devices.filter((d) => d.kind === "input"),
    outputDevices: (s) => s.devices.filter((d) => d.kind === "output"),
  },
  actions: {
    async loadAll() {
      this.loading = true;
      try {
        const [devices, graph, settings, autostart, apiStatus] = await Promise.all([
          // 用 refreshDevices：它顺带做「默认设备守护」（虚拟线路抢默认时恢复用户选择）
          api.refreshDevices(),
          api.getGraph(),
          api.getSettings(),
          api.getAutostart(),
          api.getControlApiStatus(),
        ]);
        this.devices = devices;
        this.graph = graph;
        this.settings = settings;
        this.autostart = autostart;
        this.apiStatus = apiStatus;
      } finally {
        this.loading = false;
      }
    },
    async refreshDevices() {
      this.devices = await api.refreshDevices();
    },
    async saveGraph() {
      this.graph = await api.applyGraph(this.graph);
    },
    async pollLevels() {
      this.levels = await api.getLevels();
    },
    deviceName(id: string): string {
      const d = this.devices.find((x) => x.id === id);
      return d ? d.name : id.slice(0, 16) + "…";
    },
  },
});
