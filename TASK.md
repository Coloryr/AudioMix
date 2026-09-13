# TASK：USB/IP 虚拟声卡（usbip-win2 + UAC）

> 更新时间：2026-09-14。历史过程已删，**已完成的任务已从列表移除**，只留待办与仍然有效的结论。
> 优先级：**P0 UAC2 → P1 虚拟麦克风 → P2 界面/功能**。

## 目标

应用内置 **USB/IP 服务器**（TCP 127.0.0.1:3240，协议 v1.1.1）仿真 USB 声卡，
用 **usbip-win2**（微软签名、HVCI 兼容）把它接进 Windows，系统自带驱动
（UAC1 → `usbaudio.sys`，UAC2 → `usbaudio2.sys`）暴露成标准播放/录音端点。
每条线缆采样率 44.1–192kHz、位深 16/24/32bit 可配。

- HVCI 开启，MTT 内核驱动报错 52 → 弃用自研内核驱动，改走 USB/IP。
- usbip 模块放在 `audiomix-backend-windows`，**不新建 crate**；安装包随应用捆绑。
- 参考实现：`tarekwasfy01/Virtual-Cables`（Go，BSD-2；本地 `H:\Temp\vc-src`）
  与 Linux `f_uac2.c`。**UAC1 已打通并在真机验证**（见「关键结论」）。
- UAC1 全速带宽上限 1023 B/ms → `192k/24bit`、`176.4k/24bit` 在 UAC1 下直接报错提示改用 UAC2。

## 数据流

```
Windows 应用播放 → 扬声器 (Virtual Cable NN)  [usbaudio/usbaudio2]
  → usbip-win2 vhci → TCP:3240 → ISO OUT PCM（小端）→ pcm_to_f32 → play_ring
      a) 引擎 Source tap（start_capture）→ 混音图路由 → Sink → 物理输出设备
      b) loopback 模式：同份数据写 cap_ring → ISO IN → 麦克风端
混音图 Sink(线路输出) → start_render → cap_ring → ISO IN（小端）→ 麦克风 (Virtual Cable NN)
```

- 设备 id `usbip://{n}/playback`（= 线路输出，播放端）、`usbip://{n}/capture`（= 线路输入，录音端）。
- 用户口径：**线路输出 = 系统播放端（扬声器）**，**线路输入 = 系统录音端（麦克风）**。

## 待办（按优先级）

### P0 · UAC2 仍未打通（最高优先级）

设备能枚举（`usbaudio2.sys`，Problem 0），但 Windows 拿不到「设备格式」：
`GetMixFormat` → `0x88890008 (AUDCLNT_E_UNSUPPORTED_FORMAT)`；KS 独占下 48k/16 PCM 仍被接受，
驱动却不停 `SET_CUR(采样率)` + `SET_INTERFACE(alt1→alt0)` 死循环、从不发起 iso 传输。

已排除：包长余量、bInterval(1/3/4)、单/双 clock、clock 类型与 bmControls、有无 Feature Unit、
端点同步类型、AC 中断端点（有/回包/NAK/无）、bCategory、终端拷贝控制位、
iso 包 `offset` 是否连续（实测连续）、采样率/位深（换 48k/16 照样失败）。

**下一步（按优先级）**：

1. `RUST_LOG=audiomix_backend_windows=trace` 抓 UAC2 **枚举期全部 EP0 请求**，
   把每条类请求的 `wLength` 与我们的应答长度**逐条对比** —— UAC1 就是死在这个不匹配上
   （`GET_MIN/MAX/RES` 各 2 字节 vs 我们回 8 字节的 `GET_RANGE` 结构 → 被截断 → StartDevice 失败）。
2. 按 Linux `f_uac2.c` 核对 **`wMaxPacketSize` 的"+1 帧给 Win10"**：`get_max_bw_for_bint()` 里
   这个余量**只加在 capture（USB IN）方向**；我们现在两个方向都加，UAC2 下可能让 pin 的格式判定失败。
3. 用 usbzh 的 UAC2 描述符/请求系列文章逐条核对 AC 头 / Clock Source / AS General / Format Type I
   （本地参考：`H:\Temp\AudioMix\usbzh-174.html`、`xmos_descriptors.h`、`ms-usbaudio2.md`、`f_uac2.c`）。
4. 备选：把一个真实能在 usbaudio2 下工作的 UAC2 设备描述符逐字节 diff。

### P1 · 虚拟麦克风（capture 方向）应用侧几乎收不到声音

- **设备侧完全正常**：`ISO IN` 统计 100 URB/s、187 KB/s、峰值 0.25、**零样本仅 0.1%**
  —— 我们交给主机的 PCM 是满幅、连续的。
- **应用侧却是 −57 dB 的近似静音**（`GetMixFormat` 正常、端点音量 1.0、未静音、无精确零缺口）；
  偶尔某次能录到完美的 1kHz（倍率 1.0000、RMS 0.015）。
- 已修的**真问题**：loopback 拷贝只在「主机真的在读麦克风（录音接口 alt1）」时才做，
  否则 `cap_ring` 会被灌满、之后每次 push 都丢掉**还没被读走**的音频（实测丢接近 100%）。
- 下一步：统计 `AUDCLNT_BUFFERFLAGS_SILENT` 占比；用另一种宿主（独占采集 / KS 直读）交叉验证；
  对比 ISO IN 完成时刻与驱动采集缓冲的对齐关系。

### P2 · 界面 / 功能清单（用户提出，尚未开工）

1. 左侧「设备」面板与接线画布**等高**（`MixerView.vue` 的 `.palette` 现在用 `clamp()` 估算，
   应改为绑定已存在的 `canvasH`）。
2. 虚拟声卡页**一条线路一行放不下** → 改成两行布局（`DriverView.vue` 线路卡片）。
3. **降低端到端延迟**：已把 WASAPI 共享缓冲 200ms→50ms、引擎喂数余量 50ms→20ms（≈ −180ms）；
   若仍偏高，下一档是线缆缓冲占用与 ISO 完成提前量，实测口径用「播测试音看回环起点」。
4. **新增功能节点**：开关 / 延迟 / 强度 / 均衡器 / 高通 / 低通 / 带通。
   需要引擎侧 DSP（现在每条路由只有 `gain` + `muted`）：建议 `Route` 增 `nodes: Vec<DspNode>`
   （Biquad + 延迟线 + 增益 + 开关），在 `audiomix-core/src/mixer.rs` 做纯函数实现 + 单测，
   前端在节点选中面板改参数。**本轮最大功能项，单独排期。**
5. **接线图自动排布**（按信号流向分层整理节点）。
6. **界面缩放后画布里的节点会跟着动**：节点位置是归一化存、按画布尺寸还原，缩放时应保持像素位置
   （resize 时按旧尺寸换算，而不是直接套归一化坐标）。
7. **节点中间那行字太长被省略** → 允许两行/换行（`.node-sub` 加 `white-space: normal` + 行数限制）。
8. **连线箭头戳进端子圆圈里** → 调整路径终点 / `marker` 的 `refX`，让箭头停在圆边。
9. **拉线跟随鼠标的虚线不显示** → 检查 `wireSource` 存在时的临时 `<path>`（样式类已存在，
   可能是节流里没更新，或 SVG 层级 / `pointer-events` 问题）。
10. **节点音量指示条（绿→黄→红，分段变色）**：左→右增长，颜色按**阈值分段**
    （例如 < −18dBFS 绿、−18…−6dBFS 黄、> −6dBFS 红），带峰值保持 + 缓慢回落。
    数据用引擎现有的 `levels`（`EngineStats`），前端改成事件推送或 `requestAnimationFrame` 轮询，
    避免每条节点各自 setInterval。
11. **FFT 频谱展示（foobar2000 风格柱状），可开关**：对每条 source/sink 的最近 1024 点做 Hann 窗 FFT，
    聚合成 ~32 段对数频段（dB）随 `EngineStats` 上报；前端画柱状，开关放混音页标题栏/设置，
    **默认关闭**（有 CPU 开销）。需要新增「最近样本环形缓冲 + FFT」模块并配单测。

## 关键结论（仍然有效的坑）

### 虚拟线缆数据通路（照抄参考实现 Virtual-Cables 的 UAC1）

- **参考实现只有一个 ring**：播放端 `WritePlayback` 写、采集端 `ReadCapture` 从**同一个** ring 读
  —— loopback 是天然 FIFO 直通。我们为让混音器也能读播放端用了两份 ring，
  多出来的拷贝必须**只在有读者（录音接口 alt1）时才做**（见 P1）。
- **顺序**：iso OUT **立刻**把数据写进 ring，只把"回复"推迟到节拍点；iso IN 是**节拍到点才读 ring**。
  我们原来写成"节拍到点后才写数据"，ring 里永远比主机晚一个节拍，采集方向因此读到空/过期数据。
- 节拍必须是**绝对排程 + 小提前量**（`server.rs::IsoTimeline::reserve`）：主机"收到完成才提交下一批"，
  节拍=流速率；落后过多要**重新起拍**，而不是让主机一次性追平
  （后者让播放变快，并一次丢掉数秒音频：实测 `丢=562868` 样本 ≈ 5.9 秒）。

### UAC1 vs UAC2 的类请求差异（UAC1 曾因此起不来）

- UAC1 采样率控制挂在**端点**上（recipient=endpoint，wIndex 低字节 = 端点地址），值 **3 字节**；
  UAC2 挂在 **Clock Source 实体**上，值 **4 字节**，且有 `GET_RANGE(0x82)`。
- UAC1 查音量范围用**分开的三条**：`GET_MIN(0x82)`/`GET_MAX(0x83)`/`GET_RES(0x84)`，各 **2 字节**；
  UAC2 用 `GET_RANGE` 一次返回「子范围数(2) + MIN/MAX/RES(各 2 或 4 字节）」。
  按 UAC2 回 8 字节会被 `wLength=2` 截断 → `usbaudio.sys` StartDevice 失败（设备代码 10）。
- UAC2 的 GET 请求码必须置 bit7（`GET_CUR=0x81`、`GET_RANGE=0x82`）；解析按 `request & 0x7F` 归一化。
- Clock Source 若宣告了有效性控制（bmControls bit pair1），必须应答 `CS=0x02` 的 GET_CUR（1 字节）。
- UAC1 描述符：`bcdUSB=0x0110`、类/子类/协议**全 0**、**不提供 device qualifier**、`bInterval=1`、
  包长 = 每毫秒字节数（48k/16 → 192，精确不加余量）、播放 `bmAttributes=0x09`、采集 `0x0D`；
  USB/IP devlist 按全速上报（`SPEED_FULL=2`）。

### 音频数据面

- **喂数节奏按单调时钟累计**：Windows 上 `thread::sleep(10ms)` 实测 10.384ms（定时器粒度），
  固定每拍 480 帧只有 96.3% 速率 → 引擎重采样器欠载、保持上一帧 → 变慢 + 卡顿。
  见 `usbip/backend.rs::frames_due`（留 20ms 余量给引擎）。
- **PCM 字节序是小端**（USB 音频规范 §2.3.1）。曾误用大端 → 播放进线缆的正常样本被解析成满幅噪声
  （接主播放设备只有沙沙声、电平顶满）。线缆内部 loopback 会「解一遍再编一遍」把错误抵消，
  **只有跨出线缆才暴露** —— 自检必须用写死的字节（`pcm_stream_is_little_endian`）。
- **16bit 路径会加抖动**：用精细测试信号（如每帧 +1 LSB 的锯齿）验证数据完整性时，
  Windows 的 float→16bit 转换抖动会造成大量假阳性 —— 步长要远大于 1 LSB（用 16 LSB 以上）。
- 引擎侧：source 回调把数据推进 250ms 边环；sink 拉取时 `drain` 全部可用样本交给重采样器
  （重采样器内部 VecDeque 不丢数据）；`soft_clip` + 电平统计在最后。
- 诊断手段：控制 API（`settings.control_api.enabled=true`，`http://127.0.0.1:17643/api/status`）
  可直接读每条 source/sink 的**电平**与 `source_dropped` / `sink_underruns`；
  线缆环的 `欠/丢`、`ISO OUT/IN` 到达统计在 `usbip/backend.rs`、`usbip/server.rs` 的 debug 日志里
  （正常启动用 `RUST_LOG=info`，不打印）。
  测速率要用**已知频率测试音 + 过零计数**，不能只看峰值。

### 设备 / 系统层

- 虚拟线路接入后 Windows 会**抢走默认播放和录音设备** → 常驻「默认设备守护」在接入后 30s 窗口内
  恢复用户偏好，窗口外不干预（用户主动选线路会被尊重）。
- **每次 `usbip attach` 都新占一个 vhci 端口** → Windows 里多出一对
  `扬声器/麦克风 (N- Virtual Cable NN)`。清理只需 `usbip detach --all`（**不需要管理员**），
  守护线程里加了「端口数 > 线缆数就断开」的自检。attach/detach/音量/默认设备都无需提权。
- **应用启动早于线缆 attach** 时，混音图里的 source/sink 会因「设备不存在」被跳过；
  必须在设备表变化时重新对齐流（`Engine::refresh_devices` 会 `sync_streams`，
  守护线程发现新虚拟端点时也刷新），否则路由看着接好了却完全没声音。
- 保存线缆配置时**不要重绑端口**：`server::serve` 的会话任务挂进 `JoinSet`，`start()` 已在运行
  就只替换线缆表；真需要重绑时先 `abort` 再等 `is_finished()` + 重试。
- 中断端点（AC 的 EP3 IN）没有事件时**必须不回复**（= NAK）；回 0/2 字节会被当成控制变化事件，
  变成每秒上千次轮询。
- 渲染/采集回调线程要注册 **MMCSS「Pro Audio」**（`wasapi::ProAudio`），否则系统一忙就被抢占，
  输出会被补静音（实测出现过 645ms 静音空洞）。
- 高速 iso `wMaxPacketSize`：bits 10:0 = 单事务字节数（≤1024），bits 12:11 = 额外事务机会。

## 工程约定

- 改源码只用 read/write/edit 工具；**绝不用 PowerShell 文本 cmdlet 改仓库文件**
  （中文 Windows 上 `Get-Content -Raw`/`Set-Content` 按 GBK 解码 UTF-8，会把中文变乱码）。
  已经出过一次事故，用 python 显式 `encoding=` 才救回来（仓库有 git，`git checkout -- <file>` 可整体恢复）。
- 临时文件/脚本/抓包放 `H:\Temp\AudioMix`，仓库里只留最终产物；诊断用 example 用完即删。
- `python` 用 `E:\environment\python\python.exe`（`python3` 是商店占位符）；抓网页用
  `H:\Temp\tools\fetch.py`（curl 对 raw.githubusercontent 会 reset）。
- 不跑系统服务重启 / `pnputil /restart-device`；测试（`cargo test`）与运行应用不要同时进行（会锁 exe）。
- 诊断命令：`usbip.exe`（`C:\Program Files\USBip\usbip.exe`）、
  `Get-PnpDeviceProperty` 看 `DEVPKEY_Device_ProblemCode/Service/DriverInfPath`、
  事件日志 `Microsoft-Windows-Kernel-PnP/Configuration` 事件 411。
- 应用日志：`%APPDATA%\com.audiomix.app\audiomix.log`；配置 `config.json` 在同目录。
