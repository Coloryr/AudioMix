# TASK：USB/IP 虚拟声卡（usbip-win2 + UAC）

> 更新时间：2026-09-13。历史过程已删，只留**当前状态 + 仍然有效的结论**。

## 目标

应用内置一个 **USB/IP 服务器**（TCP 127.0.0.1:3240，协议 v1.1.1）仿真 USB 声卡，
用 **usbip-win2**（微软签名、HVCI 兼容）把它接进 Windows，系统自带驱动
（UAC1 → `usbaudio.sys`，UAC2 → `usbaudio2.sys`）把它暴露成标准播放/录音端点。
每条线缆采样率 44.1–192kHz、位深 16/24/32bit 可配。

- HVCI 开启，MTT 内核驱动报错 52 → 弃用自研内核驱动，改走 USB/IP。
- usbip 模块放在 `audiomix-backend-windows`，**不新建 crate**；安装包随应用捆绑。
- 参考实现：`tarekwasfy01/Virtual-Cables`（Go，BSD-2；我们的 USB/IP 服务端移植自它）
  与 Linux `f_uac2.c`（UAC2 描述符/包长依据）。

## 数据流

```
Windows 应用播放 → 扬声器 (Virtual Cable NN)  [usbaudio/usbaudio2]
  → usbip-win2 vhci → TCP:3240 → ISO OUT PCM（小端）→ pcm_to_f32 → play_ring
      a) 引擎 Source tap（start_capture）→ 混音图路由 → Sink → 物理输出设备
      b) loop_to_mic 模式：同份数据写 cap_ring → ISO IN → 麦克风端
混音图 Sink(线路输出) → start_render → cap_ring → ISO IN（小端）→ 麦克风 (Virtual Cable NN)
```

- 设备 id `usbip://{n}/playback`（= 线路输出，播放端）、`usbip://{n}/capture`（= 线路输入，录音端）。
- 用户口径：**线路输出 = 系统播放端（扬声器）**，**线路输入 = 系统录音端（麦克风）**。
- 每线缆两个 f32 环：`play_ring`（满则丢最旧）、`cap_ring`（空则补数字静音）。
- 引擎是**拉取式**：sink 线程按自己的节奏取数，重采样器（`core::resample::PullResampler`）
  输入不足时「保持上一帧」→ 听感是变慢 + 一卡一卡。所以喂数速率必须精确。

## 当前状态

### ✅ 已完成并能用

| 能力 | 状态 |
|---|---|
| USB/IP 协议/服务器/环缓冲/描述符 | ✅ 单测 + 独立客户端集成测试 |
| UAC1 线缆（全速、`usbaudio.sys`） | ✅ 真机能用（详见下） |
| UAC1 端到端音频 | ✅ 播放进扬声器端 → 从麦克风端录到（1kHz 正弦，峰值 0.506） |
| 线缆 → 混音器 → 物理输出 | ✅ 20kHz 测试音 Goertzel 量到 2500× 基线；6s/1kHz 测试音实测 **999.2Hz**、无掉音 |
| 虚拟线路接入后默认设备不被抢走 | ✅ 守护线程真机验证 |
| 混音页（系统设置 + 拖拽接线画布）、虚拟声卡页（名称/格式/协议/接线/附加） | ✅ |
| 日志落盘 + 设置页可选中复制 | ✅ |
| 无命令行窗口启动（`windows_subsystem="windows"`） | ✅ |

### ⚠️ 未完成 / 正在修

**UAC2 不通（核心待办）**：设备能枚举（`usbaudio2.sys`，Problem 0），但 Windows 拿不到
「设备格式」：`GetMixFormat` → `0x88890008 (AUDCLNT_E_UNSUPPORTED_FORMAT)`；KS 独占下
48k/16 PCM 仍被接受，驱动却不停 `SET_CUR(采样率)` + `SET_INTERFACE(alt1→alt0)` 死循环、
从不发起 iso 传输。已排除：包长余量、bInterval(1/3/4)、单/双 clock、clock 类型与 bmControls、
有无 Feature Unit、端点同步类型、AC 中断端点（有/回包/NAK/无）、bCategory、终端拷贝控制位。
**下一步**：把 UAC2 枚举期**所有类请求的 wLength 与我们的应答长度逐条对比**（UAC1 就死在这里），
再用 usbzh UAC2 系列文章逐条核对。

## 关键结论（仍然有效的坑）

### UAC1 vs UAC2 的类请求差异（UAC1 曾因此起不来）

- UAC1 的采样率控制挂在**端点**上（recipient=endpoint，wIndex 低字节 = 端点地址），值 **3 字节**；
  UAC2 挂在 **Clock Source 实体**上，值 **4 字节**，且有 `GET_RANGE(0x82)`。
- UAC1 查音量范围用**分开的三条**：`GET_MIN(0x82)`/`GET_MAX(0x83)`/`GET_RES(0x84)`，各 **2 字节**；
  UAC2 用 `GET_RANGE` 一次返回「子范围数(2) + MIN/MAX/RES(各 2 或 4 字节）」。
  我们当时按 UAC2 回 8 字节 → 被 `wLength=2` 截断 → `usbaudio.sys` StartDevice 失败（代码 10）。
- UAC2 的 GET 请求码必须置 bit7（`GET_CUR=0x81`、`GET_RANGE=0x82`）；解析按 `request & 0x7F` 归一化。
- Clock Source 若宣告了有效性控制（bmControls bit pair1），就必须应答 `CS=0x02` 的 GET_CUR（1 字节）。
- UAC1 描述符：`bcdUSB=0x0110`、类/子类/协议**全 0**、**不提供 device qualifier**、
  `bInterval=1`、包长 = 每毫秒字节数（48k/16 → 192，精确不加余量）、播放 `bmAttributes=0x09`、
  采集 `0x0D`；USB/IP devlist 按全速上报（`SPEED_FULL=2`）。
- UAC1 全速带宽上限 1023 B/ms → `192k/24bit`、`176.4k/24bit` 直接报错提示改用 UAC2。

### 音频数据面

- **iso 完成节拍必须是「绝对节拍」**（`server.rs::IsoTimeline::reserve`）：
  基准取上一次排定的完成时刻，即使它已过期也照用（允许最多落后 100ms 去追平）。
  曾经写成 `base = max(上次, now) + duration`：tokio 定时器 + 调度让每条完成迟到约 1ms，
  于是一条 10ms 的 URB 变成 11ms → 主机每秒只送得出 ~88 条 = **每秒少 12% 音频**，
  听感就是「变慢 + 一卡一卡」（实测 1000Hz 测试音放出来只有 881–943Hz），
  而且 Windows 侧会不停 `CLEAR_FEATURE(ENDPOINT_HALT)` 试图恢复播放端点（实测 148 次 → 修好后 0 次）。
  诊断口径：`RUST_LOG=…=trace` 数 URB 行，正常应 ≈100 条/秒 × 10 包。
- **喂数节奏按单调时钟累计**：Windows 上 `thread::sleep(10ms)` 实测 10.384ms（定时器粒度），
  固定每拍 480 帧只有 96.3% 速率 → 引擎重采样器欠载、保持上一帧 → 变慢 + 卡顿。
  见 `usbip/backend.rs::frames_due`（另留 50ms 余量给引擎）。
- **PCM 字节序是小端**（USB 音频规范 §2.3.1），Windows 侧也按小端收发。
  曾误用大端 → 播放进线缆的正常样本被解析成满幅噪声（接主播放设备只有沙沙声、电平顶满）。
  线缆内部 loopback 会「解一遍再编一遍」把错误抵消，**只有跨出线缆才暴露** ——
  自检必须用写死的字节（`pcm_stream_is_little_endian`）。
- 引擎侧：source 回调把数据推进 250ms 边环；sink 拉取时 `drain` 全部可用样本交给重采样器
  （重采样器内部 VecDeque 不丢数据）；`soft_clip` + 电平统计在最后。
- 诊断手段：控制 API（`settings.control_api.enabled=true`，`http://127.0.0.1:17643/api/status`）
  能直接读每条 source/sink 的**电平**与 `source_dropped` / `sink_underruns`，比自己录回环可靠；
  线缆环形缓冲的 `欠/丢` 另见 `usbip/backend.rs` 里每 2 秒一条的 debug 日志。
  测「跨线缆路径」的速率要用**已知频率的测试音 + 过零计数**（回环录到的波形），
  不能只看峰值——字节序错误在自回环里是会被抵消的。

### 设备 / 系统层

- 虚拟线路接入后 Windows 会**抢走默认播放和录音设备**（实测复现）→ 常驻「默认设备守护」
  在接入后的 30s 窗口内恢复用户偏好，窗口外不干预（用户主动选线路会被尊重）。
- **每次 `usbip attach` 都新占一个 vhci 端口** → Windows 里多出一对
  `扬声器/麦克风 (N- Virtual Cable NN)`。清理只需 `usbip detach --all`（**不需要管理员**），
  守护线程里加了「端口数 > 线缆数就断开」的自检。`attach`/`detach`/音量/默认设备都无需提权。
- **应用启动早于线缆 attach 时**，混音图里那条 source/sink 会因「设备不存在」被跳过；
  必须在设备表变化时重新对齐流（`Engine::refresh_devices` 现在会 `sync_streams`，
  守护线程发现新虚拟端点时也会刷新），否则路由看着接好了却完全没声音。
- 保存线缆配置时**不要重绑端口**：`server::serve` 的会话任务要挂进 `JoinSet`（随服务器任务一起
  结束），`start()` 已在运行就只替换线缆表；真需要重绑时先 `abort` 再等 `is_finished()` + 重试。
- 中断端点（AC 的 EP3 IN）没有事件时**必须不回复**（= NAK）；回 0/2 字节会被当成控制变化事件，
  变成每秒上千次轮询。
- 高速 iso `wMaxPacketSize`：bits 10:0 = 单事务字节数（≤1024），bits 12:11 = 额外事务机会。

## 工程约定

- 改源码只用 read/write/edit 工具；**绝不用 PowerShell 文本 cmdlet 改仓库文件**
  （中文 Windows 上 `Get-Content -Raw`/`Set-Content` 按 GBK 解码 UTF-8，会把中文变 `?`、吞掉换行）。
- 临时文件/脚本/抓包放 `H:\Temp\AudioMix`，仓库里只留最终产物；诊断用 example 用完即删。
- `python` 用 `E:\environment\python\python.exe`（`python3` 是商店占位符）；抓网页用
  `H:\Temp\tools\fetch.py`（curl 在本机对 raw.githubusercontent 会 reset）。
- 不跑系统服务重启 / `pnputil /restart-device`；测试与运行应用不要同时进行（会锁 exe）。
- 诊断命令：`usbip.exe`（`C:\Program Files\USBip\usbip.exe`）、
  `Get-PnpDeviceProperty` 看 `DEVPKEY_Device_ProblemCode/Service/DriverInfPath`、
  事件日志 `Microsoft-Windows-Kernel-PnP/Configuration` 事件 411。
- 应用日志：`%APPDATA%\com.audiomix.app\audiomix.log`；配置 `config.json` 在同目录。
  EP0 控制请求默认只记 STALL（全量用 `RUST_LOG=…=trace`，UAC2 死循环会刷爆日志）。
