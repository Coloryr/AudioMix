# TASK：USB/IP 虚拟声卡（usbip-win2 + UAC1）

> 更新时间：2026-09-18。只留**待办**与**仍然有效的结论**；已完成的过程细节看 git 历史。
> 优先级：**P0 传输层天花板（已定论）→ P1 虚拟麦克风 → P2 界面/功能 → P3 产品化**。

## 目标

应用内置 **USB/IP 服务器**（TCP 127.0.0.1:3240，协议 v1.1.1）仿真 USB 声卡，
用 **usbip-win2**（微软签名、HVCI 兼容）接进 Windows，系统自带 **UAC1** 驱动
`usbaudio.sys` 暴露成标准播放/录音端点。参考实现：`tarekwasfy01/Virtual-Cables`
（Go，BSD-2；本地 `H:\Temp\vc-src`）。

## 数据流

```
Windows 应用播放 → 扬声器 (Virtual Cable NN)  [usbaudio.sys]
  → usbip-win2 vhci → TCP:3240 → ISO OUT PCM（小端）→ pcm_to_f32 → play_ring
      a) 引擎 Source tap（start_capture）→ 混音图路由 → Sink → 物理输出设备
      b) loopback 模式：同份数据写 cap_ring → ISO IN → 麦克风端
混音图 Sink(线路输出) → start_render → cap_ring → ISO IN（小端）→ 麦克风 (Virtual Cable NN)
```

- 设备 id `usbip://{n}/playback`（= 线路输出 = 系统播放端）、`usbip://{n}/capture`
  （= 线路输入 = 系统录音端）。
- **线路端点的「Windows 系统音量」没有声效（已定论）**：UAC Feature Unit 的音量/静音只存在于
  DevState 控制请求里，虚拟线路的数据通路（play_ring/cap_ring 的样本）完全不经过它 ⇒
  调了不改声音。线路节点的音量一律用混音图的「混音」（UI 已隐藏线路端点的系统音量条；
  节点面板里 `usbip://` 开头的 id 也不再解析 Windows 端点音量）。

## 待办

### P2 · 界面 / 功能

1. **降低端到端延迟**：下一档是线缆缓冲占用与 ISO 完成提前量。口径：混音页点连线「测量延迟」
   （注入扫频脉冲 + 相关检测，实测 source → DSP → sink 全程，不含两端设备缓冲）。

## 工程约定

- 改源码只用 read/write/edit 工具；**绝不用 PowerShell 文本 cmdlet 改仓库文件**
  （中文 Windows 上按 GBK 解码 UTF-8 会把中文变乱码，已出过一次事故，`git checkout --` 可恢复）。
- 临时文件/脚本/抓包放 `H:\Temp\AudioMix`，仓库只留最终产物；诊断用 example 用完即删。
- `python` 用 `E:\environment\python\python.exe`（`python3` 是商店占位符）；抓网页用
  `H:\Temp\tools\fetch.py`（curl 对 raw.githubusercontent 会 reset）。
- 不跑系统服务重启 / `pnputil /restart-device`；`cargo test` 与运行应用不同时进行（锁 exe）。
- 诊断命令：`usbip.exe`（`C:\Program Files\USBip\usbip.exe`）、`Get-PnpDeviceProperty`
  （ProblemCode/Service/DriverInfPath）、Kernel-PnP 事件日志 411。
- 应用日志：`%APPDATA%\com.audiomix.app\audiomix.log`；配置 `config.json` 同目录。
