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

**本轮进展（2026-09-14，逐字对照 Linux `f_uac2.c` 后的结论与改动）**：

- **步骤 2 的假设可以排除**：读 `get_max_bw_for_bint()` 源码，playback 与 async capture 走的是
  同一分支——速率先按 `fb_max=5` 膨胀 0.5%（`srate*1005/1000`）再 `DIV_ROUND_UP` 到服务间隔，
  **恒比标称多 1 帧**，两个方向都加；「+1 帧只加在 capture」的说法不成立（那是 sync capture 分支）。
  48k/96k/192k 下我们的 `wMaxPacketSize` 与 f_uac2 **逐字节相同**（如 48k/16 → 196B@bInterval=4）；
  仅 44.1k 系比 f_uac2 多 1 帧（我们更大，不太可能导致失败）。
- 描述符已对齐 f_uac2 拓扑（`descriptors.rs`）：**两个 Clock Source**（播放=10 / 采集=11，
  对应 MS 文档「limited support for devices using a shared clock」），
  `bmAttributes=0x03`（internal fixed，f_uac2 的 `INT_FIXED`），
  `packet_bytes_at()` 改为 f_uac2 同款公式；`device.rs` 的 EP0 处理本来就同时支持实体 10/11。
- 步骤 1 的工具已就位：`device.rs::handle_control` 对每条类请求打 trace（含 wLength 与应答长度）、
  STALL 打 debug。

**2026-09-14 下午 trace 实测（`RUST_LOG=audiomix_backend_windows=trace`，48k/16/uac2，步骤 1 已执行）**：

- **EP0 完全干净**：枚举/字符串/SET_CONFIG/GET_STATUS/时钟 GET_RANGE（驱动以 `wLength=256`
  请求、答 14 字节属正常读法）/Feature Unit 音量 GET_RANGE/GET_CUR 全部 wLength 吻合、零 STALL
  —— **步骤 4 的描述符字节级 diff 不必做了，描述符已排除**。
- **死循环抓到**：`SET_INTERFACE(if1/if2, alt1) → 立即 alt0 → SET_CUR(clock 实体)` 每 ~1.5ms 一轮，
  **零 ISO URB 到达服务器**（server 的 URB trace 只有 EP3 中断轮询）——失败发生在 Windows 内部，
  驱动反复尝试激活 iso 管道、瞬间被否决。界面症状与之一致：声音设置里端点属性只有空的「级别」栏
  （usbaudio2 没建出任何可用格式/KS pin）。
- **根因定位（usbip-win2 issue #35 同款，官方也只修了全速）**：usbip-win2 0.9.8 的 UDE 虚拟控制器里，
  `ucx01000!UrbHandler_USBPORTStyle_Legacy_IsochTransfer` 会把 iso URB 以 USBD_STATUS_INVALID_PARAMETER
  直接否决（URB 不下发到服务器）。UDE 对 bInterval 永远按 0.125ms 微帧解释；官方修复
  （commit 2cee7e0）只在**返回的配置描述符里把「iso OUT 且 bInterval==1」补丁成 4**——即 UDE 只验证过
  「全速设备 + 1ms 包」形态（UAC1 即此形态）。原生高速（SPEED_HIGH + 高速 bInterval 语义）iso
  没有可用先例（Xbox 手柄 iso 设备同类失败：#170/#125）。
- **决策（用户拍板，2026-09-14）：全部改用高速模式（High-Speed，480 Mbps）**——UAC2 保持原生高速
  描述符（bInterval=4、包长按 1ms 服务间隔）与 `SPEED_HIGH` 上报，**不降级全速**（全速会失去
  192k/24bit、192k/32bit，违背 UAC2 的存在意义）。本地暂无绕过手段；后续方向：跟进/推动
  usbip-win2 修复 UDE 高速 iso（issue #35 思路：其 filter 驱动可在 ucx 校验前拦截/改造 iso URB，
  或等待上游把高速 iso 补齐），必要时评估替换虚拟主机控制器方案。本机装的是
  `C:\Program Files\USBip`（usbip-win2 0.9.8.0，Cloudyne 签名）。

### P1 · 虚拟麦克风（capture 方向）应用侧几乎收不到声音

- **设备侧完全正常**：`ISO IN` 统计 100 URB/s、187 KB/s、峰值 0.25、**零样本仅 0.1%**
  —— 我们交给主机的 PCM 是满幅、连续的。
- **应用侧却是 −57 dB 的近似静音**（`GetMixFormat` 正常、端点音量 1.0、未静音、无精确零缺口）；
  偶尔某次能录到完美的 1kHz（倍率 1.0000、RMS 0.015）。
- 已修的**真问题**：loopback 拷贝只在「主机真的在读麦克风（录音接口 alt1）」时才做，
  否则 `cap_ring` 会被灌满、之后每次 push 都丢掉**还没被读走**的音频（实测丢接近 100%）。
- 下一步：统计 `AUDCLNT_BUFFERFLAGS_SILENT` 占比；用另一种宿主（独占采集 / KS 直读）交叉验证；
  对比 ISO IN 完成时刻与驱动采集缓冲的对齐关系。

### P2 · 界面 / 功能清单

已完成的界面小修（2026-09-14，均已随本轮改动落地）：设备面板与画布等高（grid stretch 取代
clamp 估算）、线路卡片两行布局、节点缩放保位（measure() 里按「像素位置不变」重新归一化，
停稳 500ms 后写盘）、节点副标题两行（NODE_H 96→108）、箭头停在端子圆边（路径两端各缩 10px +
marker refX=10）、拉线虚线不显示（**根因：`wirePath` 里 `to.dir * d`，预览线终点没有 dir →
undefined×数=NaN，整条 path 画不出来**；dir 缺省按 0 处理即修复）、节点电平条分段变色
（<−18dBFS 绿 / −18…−6 黄 / >−6 红，`components/MeterBar.vue` 峰值保持 + 每拍回落 2%）。

1. **降低端到端延迟**：已把 WASAPI 共享缓冲 200ms→50ms、引擎喂数余量 50ms→20ms（≈ −180ms）；
   若仍偏高，下一档是线缆缓冲占用与 ISO 完成提前量，实测口径用「播测试音看回环起点」。
2. **新增功能节点（DSP 节点链）**：开关 / 延迟 / 强度 / 均衡器 / 高通 / 低通 / 带通。
   用户已拍板的决策：
   - **均衡器三种形式都做**：3 段架式（低架/峰值/高架）、单段峰式、图形 EQ；高通/低通/带通作为独立节点。
   - **参数在「连线选中面板」编辑**：点选一条连线，面板里显示该路由的 DSP 节点链
     （列表 + 添加/删除/排序 + 各节点参数），不做画布上的独立 DSP 节点。
   实现路径（探索结论）：`Route` 增 `nodes: Vec<DspNode>`（`#[serde(default)]` 向后兼容）；
   Biquad/延迟线在 `audiomix-core/src/mixer.rs` 做纯函数 + 单测；处理点在 `engine.rs` render 回调的
   per-edge 循环（重采样 + 声道转换之后、`mix_into` 之前），DSP 状态放 `SinkEdgeState`，
   参数经 `GraphRuntime`/`EdgeRef` 的 ArcSwap 快照下发（沿用 gain/muted 的免锁模式）；
   Tauri 侧沿用 `apply_graph` 全量下发或加 `set_route_nodes` 命令。**本轮最大功能项，单独排期。**
3. **接线图自动排布**（按信号流向分层整理节点）。
4. **FFT 频谱展示（foobar2000 风格柱状），可开关**：对每条 source/sink 的最近 1024 点做 Hann 窗 FFT，
   聚合成 ~32 段对数频段（dB）随 `EngineStats` 上报；前端画柱状，开关放混音页标题栏/设置，
   **默认关闭**（有 CPU 开销）。需要新增「最近样本环形缓冲 + FFT」模块并配单测。

### P3 · 产品化 / 部署

1. **开机自启（无窗口）**：现在只写了当前用户注册表 Run 键 + `--headless`，要把它做扎实：
   - 开机自启时**不加载 WebView、不弹任何窗口**，只起「托盘 + 引擎 + USB/IP 服务器」；
   - ~~自启后自动恢复上次的线缆附加~~ 已做一半（2026-09-14）：应用**每次启动**都会检查 vhci 端口
     （`commands.rs::auto_attach_if_needed`），端口还挂着上次的设备就不动（不弹 UAC）；
     端口空了才自动走「附加全部」弹一次 UAC。遗留：**开机自启**场景端口总是空的，
     每次登录都要弹 UAC —— 可选做法：用 usbip-win2 自带的"登录/开机自动附加"机制；
     或把应用注册成登录时运行的提权计划任务。
2. **无 WebView 运行模式**：让「引擎 + 托盘 + USB/IP 服务器」能在**不依赖 WebView2** 的情况下运行
   （独立 headless 可执行，或用 Cargo feature 把 WebView / 前端资源变成可选依赖），
   用于低配机器、没装 WebView2 的系统、以及长期后台驻留。
3. **单实例**：重复启动时唤醒已有窗口（而不是起第二个引擎/第二份 USB/IP 服务器）。
4. **便携版（绿色）**：配置与日志跟随可执行文件目录，而不是 `%APPDATA%`。
5. 安装包体积（已捆绑 usbip 安装包约 26MB）与首次安装体验（驱动安装 + 一条线缆的引导）。

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
- UAC2 描述符按 `f_uac2.c` 对齐：播放/采集**各一个** Clock Source（ID 10/11），
  `bmAttributes=0x03`（internal fixed）、`bmControls=0x03`（频率可读写）；
  高速 `wMaxPacketSize` 用 `get_max_bw_for_bint` 同款公式——`srate*1005/1000` 向上取整到服务间隔
  × 帧长（fb_max=5，恒比标称多 1 帧），见 `descriptors.rs::packet_bytes_at`。
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

- **提权 broker（2026-09-14，参考 Virtual-Cables 的 broker 架构）**：`usbip.exe` 的
  attach/detach 每次都要管理员权限，逐次提权 = 逐次 UAC。改为 `usbip/broker.rs`：
  主进程开本地 TCP listener + 一次性 64 位 token，`Start-Process -Verb RunAs -WindowStyle Hidden`
  把**自己**再启动一份（`--usbip-broker <addr> <token>`，main.rs 里分派，GUI 子系统无窗口）
  ——用户只确认**一次** UAC，broker 常驻提权，之后 attach/detach/重复端口清理全走
  `ATTACH/DETACH_ALL/DETACH_PORTS/QUIT` 命令，**零 UAC 零窗口**。应用退出（连接断开）
  broker 自杀。所有提权操作 broker 优先，`run_elevated_sequence` 一次性脚本只作回退。
  曾试过「attach 后 90s 观察期在同一次 UAC 里清理重复端口」，因提权 PowerShell 窗口
  常驻 90s 被否，broker 方案彻底替代（顺带删掉了 `--once`，改用持久 attach 自动重连）。
- **vhci 会自发重复 import**（2026-09-14 实测多次，**attach 后 ~20-30s 内高发**，与流启动/重启相关）：
  服务端看到第二个 import 会话（应用侧没有任何 attach 日志），同一 busid 占两个 vhci 端口。
  守护（每 15s 自检）**只拆多余端口、保留最早附加/在流式的那个**（走 broker 清理零 UAC），
  bus_id 失效端口一并拆；解析不出 bus_id 且总数对不上才退回 `repair_cables` 全断重接。
  另：`usbip port` 会把空闲端口也列出来，比较时只能数 `in_use` 的；
  `usbip attach --once` = 「连不上时不要自动重试」（现已不用 --once）。
- **启动自动附加**：`commands.rs::auto_attach_if_needed`（main.rs 启动后 800ms 后台执行）——
  vhci 附加状态独立于应用进程，端口还挂着上次的设备就不动（不弹 UAC）；端口空了才走
  「附加全部」弹一次 UAC。遗留：开机自启场景端口总是空的，每次登录都要弹 UAC（见 P3）。

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

- **每次改动先写 plan 给用户过目**（改哪些文件、怎么改、为什么），确认后再动手；
  多步任务分阶段给出 plan，完成后简要汇报改动点。
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
