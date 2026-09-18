import { listen } from "@tauri-apps/api/event";
import { defineStore } from "pinia";
import { api, type DeviceInfo, type GraphConfig, type Settings, type ApiStatus, type LevelsPayload } from "./api";

let idCounter = 0;
export const genId = (prefix: string) => `${prefix}-${Math.random().toString(36).slice(2, 10)}${idCounter++}`;

export const useApp = defineStore("app", {
  state: () => ({
    devices: [] as DeviceInfo[],
    graph: { sources: [], sinks: [], routes: [], processors: [] } as GraphConfig,
    levels: {} as Record<string, number>,
    /** 频谱分析：source/sink id → 20 段 dB（fft 关闭时为空对象） */
    spectra: {} as Record<string, number[]>,
    /** 是否已向后端订阅电平推送 */
    levelsWatching: false,
    /** 是否已向后端订阅设备列表推送 */
    devicesWatching: false,
    /** 是否已监听「混音图被外部改动」（控制 API）事件 */
    graphWatching: false,
    settings: {
      control_api: {
        enabled: false,
        bind: "127.0.0.1",
        port: 17643,
        /** 访问令牌（空 = 不鉴权） */
        token: "",
        /** 允许浏览器跨域调用（默认关闭） */
        cors: false,
      },
      usbip: { enabled: false, bind: "127.0.0.1:3240", cables: [] },
      autostart_headless: true,
      close_to_tray: true,
      resample_quality: "sinc256",
      edge_buffer_ms: 250,
      levels_interval_ms: 50,
      fft_enabled: false,
      fft_size: 4096,
      fft_bands: [
        50, 69, 94, 129, 176, 241, 331, 453, 620, 850, 1200, 1600, 2200, 3000, 4100, 5600, 7700,
        11000, 14000, 20000,
      ],
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
    /** 订阅后端电平推送（Channel，后端 ~20fps 主动推）。重复调用会被忽略 */
    async watchLevels() {
      if (this.levelsWatching) return;
      this.levelsWatching = true;
      // 不能按「值没变就跳过」优化：MeterBar 的峰值回落靠 level 持续更新驱动，
      // 跳过更新会把峰值标记冻在半空
      await api.subscribeLevels((payload: LevelsPayload) => {
        this.levels = payload.levels;
        this.spectra = payload.spectra;
      });
    },
    /** 取消电平推送（切走页签/窗口隐藏时调用，省掉后端序列化） */
    async unwatchLevels() {
      if (!this.levelsWatching) return;
      this.levelsWatching = false;
      await api.unsubscribeLevels().catch(() => { });
    },
    /** 订阅设备列表推送（Channel，后端看门狗枚举有变化才推）。重复调用会被忽略 */
    async watchDevices() {
      if (this.devicesWatching) return;
      this.devicesWatching = true;
      await api.subscribeDevices((devices) => {
        this.devices = devices;
      });
    },
    /** 取消设备列表推送（窗口隐藏时调用） */
    async unwatchDevices() {
      if (!this.devicesWatching) return;
      this.devicesWatching = false;
      await api.unsubscribeDevices().catch(() => { });
    },
    /**
     * 监听「混音图被外部（控制 API / 脚本）改动」事件 → 重新拉取整图。
     *
     * 界面自己保存的图不会触发该事件（后端只在 API 改动后广播），因此不会
     * 出现「边编辑边被回灌」的抖动；但外部改动必须拉回来，否则界面的旧副本
     * 会在下次保存时把改动覆盖掉。
     */
    async watchGraphChanges() {
      if (this.graphWatching) return;
      this.graphWatching = true;
      await listen("graph-changed", async () => {
        try {
          this.graph = await api.getGraph();
        } catch (e) {
          console.error("拉取外部改动的混音图失败", e);
        }
      });
    },
    deviceName(id: string): string {
      const d = this.devices.find((x) => x.id === id);
      return d ? d.name : id.slice(0, 16) + "…";
    },
  },
});
