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
- **删除线缆必须显式拆它的 vhci 端口（已定论）**：Windows 里的扬声器/麦克风是 usbip attach
  建出来的 devnode，删配置/停服务器都不会让它消失，必须 `usbip detach -p N`。所以
  `usbip_set_cables_sync` 保存后会把「已删除 / 已停用」的 busid 所占端口拆掉（broker 在线零 UAC，
  否则一次性提权；失败只告警不回滚配置）；15 秒周期的守护也会清理线缆表里已不存在的占用端口
  （**线缆表为空时同样要清理**，早前在空表处提前返回，导致删光线路后设备永久残留）。
  纯删除不再触发「detach --all + 重新附加」（那只用于新增/改格式），免得白弹 UAC 并掐断其它线路。

## 待办

### P2 · 界面 / 功能

1. **降低端到端延迟**：下一档是线缆缓冲占用与 ISO 完成提前量。口径：混音页点连线「测量延迟」
   （注入扫频脉冲 + 相关检测，实测 source → DSP → sink 全程，不含两端设备缓冲）。

### P3 · 远程 API（已完成一版，待真机走一遍）

- 已完成：`crates/audiomix-control-api` 重写为 40+ 端点的完整 REST
  （设备/混音图节点级增删改/电平/事件与电平 SSE/延迟实测/设置/虚拟声卡/日志/`GET /api` 索引）；
  统一错误体 + 状态码；可选令牌鉴权（Bearer / X-Api-Token / ?token=）；可选 CORS（默认关）；
  App 侧能力经 `ApiHost` 钩子注入（`app/src-tauri/src/api_host.rs`，未注入 → 501）；
  API 改动混音图后落盘并广播 `graph-changed`，界面自动重拉（否则界面旧副本会覆盖）。
  测试：`cargo test -p audiomix-control-api`（13 个集成测试，真 TCP + 手写 HTTP）。
- 文档：`docs/API.md`（鉴权/错误/全部端点字段与示例/SSE/配方/客户端代码/注意事项），
  README「控制 API」章节只留摘要 + 链接，避免两处漂移；运行时权威清单仍是 `GET /api`。
- 待办（需真机）：**用实际运行的应用走一遍**——设置页生成令牌 → 重启应用 →
  `curl -H "Authorization: Bearer <令牌>" http://127.0.0.1:17643/api`，
  以及 API 加节点后界面画布是否同步出现。

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
