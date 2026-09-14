# TASK：USB/IP 虚拟声卡（usbip-win2 + UAC1）

> 更新时间：2026-09-15。历史过程已删，**已完成的任务已从列表移除**，只留待办与仍然有效的结论。
> 优先级：**P0 传输层天花板与内置规格（已定论）→ P1 虚拟麦克风 → P2 界面/功能**。

## 目标

应用内置 **USB/IP 服务器**（TCP 127.0.0.1:3240，协议 v1.1.1）仿真 USB 声卡，
用 **usbip-win2**（微软签名、HVCI 兼容）把它接进 Windows，系统自带 **UAC1** 驱动
`usbaudio.sys` 暴露成标准播放/录音端点。

- **内置线路只做 UAC1（USB 1.1 全速）**：44.1–96kHz 的 16/24/32bit，176.4/192kHz 只 16bit；
  超出规格的线路由用户自装第三方虚拟声卡（VB-CABLE 等），见 P0。
- HVCI 开启，MTT 内核驱动报错 52 → 弃用自研内核驱动，改走 USB/IP。
- usbip 模块放在 `audiomix-backend-windows`，**不新建 crate**；安装包随应用捆绑。
- 参考实现：`tarekwasfy01/Virtual-Cables`（Go，BSD-2；本地 `H:\Temp\vc-src`）的 UAC1 描述符与
  USB/IP 数据通路。UAC1 已打通并在真机验证（见「关键结论」）。

## 数据流

```
Windows 应用播放 → 扬声器 (Virtual Cable NN)  [usbaudio.sys]
  → usbip-win2 vhci → TCP:3240 → ISO OUT PCM（小端）→ pcm_to_f32 → play_ring
      a) 引擎 Source tap（start_capture）→ 混音图路由 → Sink → 物理输出设备
      b) loopback 模式：同份数据写 cap_ring → ISO IN → 麦克风端
混音图 Sink(线路输出) → start_render → cap_ring → ISO IN（小端）→ 麦克风 (Virtual Cable NN)
```

- 设备 id `usbip://{n}/playback`（= 线路输出，播放端）、`usbip://{n}/capture`（= 线路输入，录音端）。
- 用户口径：**线路输出 = 系统播放端（扬声器）**，**线路输入 = 系统录音端（麦克风）**。

## 待办（按优先级）

### P0 · 传输层天花板与内置规格（已定论）

**结论：内置虚拟线路只做 UAC1（USB 1.1 全速）。超出规格的线路由用户自装第三方虚拟声卡。UAC2 已从代码中删除。**

#### 为什么不可能有 192k/24bit（上限来自 Microsoft 的 UDE 类扩展，不在我们手里）

- USB/IP 在 Windows 侧由 **UDE（USB Device Emulation）+ `ucx01000.sys`（USBHUB3）** 处理等时传输，
  它一律按 0.125ms 微帧解读 `bInterval`，因此 **iso 端点的 `bInterval` 必须 ≥ 4（服务间隔 ≥1ms）**。
  usbip-win2 源码里就是给不满足的端点打补丁：`drivers/ude/wsk_receive.cpp::patch_config()`
  `case UsbdPipeTypeIsochronous: e.bInterval = min(e.bInterval + 3, 16)`；
  上游 issue #35 第 29 条（Nefarius）把「虚拟 USB 音频在 UDE 上能工作的三件套」总结为
  ①以高速呈现 ②QueryBusTime 返回成功 ③**iso bInterval ≥ 4**；修复 commit `2cee7e0` 同义。
- ⇒ **每个 iso 端点每毫秒最多 1024 字节 ⇒ 立体声 24bit 上限约 170.6 kHz**；
  192k/24 = 1152 B/ms 必然超限。UAC1 与 UAC2 受同一条约束。
- 另有一道 `usbaudio.sys` 的限制：它按 `min(wMaxPacketSize, 1024)` 判断单包容量 ——
  所以「把 wMaxPacketSize 直接写成 1152」这类绕法（当时的 `rawbig` 变体）实测直接失败。
- 实测（每项都核对过注册表 `PKEY_AudioEngine_DeviceFormat` 并真的开流）：

  | 格式 | 结果 |
  |---|---|
  | 48k/16（192 B/ms）| ✓ 流式正常 |
  | 96k/24（576 B/ms）| ✓ |
  | 192k/16（768 B/ms）| ✓ |
  | 176.4k/24（1059）、192k/24（1152）| ✗ 无设备格式、无 ISO URB |

- UAC2（`usbaudio2.sys`）在这条通路上**功能上没有意义**：它唯一的优势是亚毫秒服务间隔，
  而这恰是 UDE 明令禁止的。多轮实验（9 组配置 + 逐字段复刻 TinyUSB / `f_uac2` / XMOS 量产固件描述符）
  都停在「设备能枚举、端点已建立、EP0 零 STALL，但 Windows 拿不到设备格式、一个 iso URB 都不发」。
- **换客户端也没用**：Windows 上真正的 USB/IP 客户端只有 usbip-win2（BSD-2，活跃）与
  cezanne/usbip-win（GPL-3，README 自述已被 usbip-win2 取代 —— 它的 `vhci(wdm)` 正是
  "不能完整支持 USB 应用" 才补了 `vhci(ude)`）；2026 年仍在更新的同类框架（VIIPER）
  在 Windows 上同样依赖 usbip-win2。任何 UDE 客户端共享同一条上限。
  （若将来必须更高规格，唯一出路是离开 USB/IP 走内核音频驱动，见 P3 备选方案。）

#### 内置规格（唯一支持矩阵）

| 位深 | 44.1 / 48 / 88.2 / 96 kHz | 176.4 / 192 kHz |
|---|---|---|
| 16 bit | ✓ | ✓ |
| 24 bit | ✓ | ✗ |
| 32 bit | ✓ | ✗ |

- 端点由系统自带 `usbaudio.sys` 驱动；描述符 = UAC1 全速（`bcdUSB=0x0110`、类字段全 0、无 IAD、
  无 device qualifier、iso `bInterval=1`、包长 = 每毫秒 PCM 字节数 ≤1023）。
- 校验在 `audiomix-core::UsbIpCableSettings::{validate,is_supported}` 与
  `usbip::descriptors::CableFormat::validate` 两处一致实现；旧配置里超规格的组合
  在载入时自动降级（`clamp_supported`，写一条 warning 日志），不会让应用起不来。
- **超出规格怎么办**：用户自行安装第三方虚拟声卡（VB-CABLE、VoiceMeeter 等）。
  它们作为普通 Windows 端点出现在混音页左侧设备列表，可直接拖进画布接线，我们不需要适配。

#### 端点包长必须按「整数个采样帧」取整（2026-09-15 真机发现并已修）

- 44.1k 系每毫秒是**小数帧**（44.1k/24bit/stereo = 264.6 字节 = 44.1 帧）。早期实现把
  标称字节数向上取整写进 `wMaxPacketSize`（265 = 44 帧 + 1 字节）——**不是合法音频包**：
  该线路的渲染端点在 Windows 里拿不到任何设备格式（`GetMixFormat` → `AUDCLNT_E_UNSUPPORTED_FORMAT`，
  注册表也没有 `PKEY_AudioEngine_DeviceFormat`），而 48k 系（192/576/768 恰好整帧）全部正常。
- 修法：`fs_wmax_packet() = ceil(rate/1000) × 帧长`（44.1k/24 → 45 帧 = 270 B，176.4k/16 → 708 B），
  核侧同规则（`UsbIpCableSettings::packet_bytes_per_ms`）；48k 系数值不变。
- 实测（`examples/uac1bench.rs` + `usbip.exe attach` + 注册表 `PKEY_AudioEngine_DeviceFormat`
  + `examples/vcfmt.rs` 真开流 + 服务端 `play_ring` 统计）：
  44.1k/16、44.1k/24、88.2k/24、176.4k/16、48k/16、96k/24、96k/32、48k/32、192k/16
  **全部「设备格式正确 + 开流成功 + 服务端确认在收数据」**。
  **注意这是单向（渲染 → 设备）的结论**；出去方向（虚拟麦克风）的回环测试尚未通过，见 P1。
- 排障注意：Windows 会按 **vhci 端口/设备实例**缓存音频端点属性，同一个线缆号在同一端口上
  换格式重新 attach 时，注册表里可能还是上一轮的旧格式（会误判成"没生效"）。
  验证时**换线缆号**（新 PID/序列号 → 新实例）最干净。

#### 保留备查的上游引用

- usbip-win2 issue #35（含 #29 Nefarius 的「unholy trinity」）、issue #181（0.9.7.8 上 ISO 音频可用）、
  commit `2cee7e0`「Fix isoch out transfers」、`drivers/ude/wsk_receive.cpp::patch_config()`。
- Windows 侧校验点：`ucx01000.sys::UrbHandler_USBPORTStyle_Legacy_IsochTransfer`
  → `USBD_STATUS_INVALID_PARAMETER`。
- 描述符实验（UAC2 的一整套变体、逐字段对齐参考实现）的记录保留在 git 历史与
  `H:\Temp\AudioMix\` 下的日志/脚本里，不再留在代码中。

### P1 · 虚拟麦克风（capture 方向）出不了声 —— **回环端到端尚未通过**

> **验证状态（2026-09-15，UAC1-only 重构后逐格式实测）**
>
> 已通过的部分：**创建**（Windows 出现设备）、**端点设备格式**（注册表
> `PKEY_AudioEngine_DeviceFormat` 与配置一致）、**打开**（渲染流 + 采集流都能开）、
> **渲染 → 设备**（`play_ring` 持续收到数据）。
>
> **未通过：回环（播进虚拟声卡 → 从虚拟麦克风录回）—— 6 种格式全部失败**
> （`examples/uac1bench.rs` + `usbip.exe attach` + `examples/vcmic.rs`，播 1kHz/0.25 共 8 秒）：
>
> | 线缆格式 | 渲染/采集端点格式 | 回环结果 |
> |---|---|---|
> | 48k/16 | 48000 / 48000 | ❌ 100ms RMS 中位 0.0000（−53.5 dB），频率读数乱 |
> | 44.1k/24 | 44100 / 48000 | ❌ 有信号但音高错（倍率 1.0695）、RMS 中位 0.0399 |
> | 96k/24 | 96000 / 48000 | ❌ 中位 0.0000（−48.6 dB） |
> | 96k/32 | 96000 / 48000 | ❌ 中位 0.0000（−47.2 dB） |
> | 176.4k/16 | 176400 / 48000 | ❌ 有信号但音高错（倍率 1.0154）、RMS 中位 0.0224 |
> | 192k/16 | 192000 / 48000 | ❌ 中位 0.0000（−29.5 dB） |
>
> 与 App 里那条 48k/16 线路现象一致（旧记录 −57 dB）⇒ **不是本轮 UAC1 重构引入的**
> （本次只改了描述符/协议选择，没碰 ISO IN、环形缓冲与控制时序），**也与格式无关**
> （48k/16 无需重采样，同样失败 ⇒ 排除"非 48k 重采样破坏数据"这一方向）。

- **设备侧确实在发**：回环测试期间 `play_ring` 持续收数据（48k/16 累计丢 74.6 万样本），
  `cap_ring` 也有数据、**欠载仅约 4%**（≈96% 的 ISO IN 包有内容），端点音量 1.0000、未静音
  ⇒ 数据是在**设备之外**（驱动 / 音频引擎 / 采集 API 这一段）掉的。
- 早期记录仍有效：`ISO IN` 侧 100 URB/s、187 KB/s、峰值 0.25、零样本 0.1%（我们交出去的 PCM
  是满幅连续的）；App 采集侧却是 −57 dB 近似静音，偶尔某次能录到完美的 1kHz（倍率 1.0000）。
- 已修的**真问题**：loopback 拷贝只在「主机真的在读麦克风（录音接口 alt1）」时才做，
  否则 `cap_ring` 会被灌满、之后每次 push 都丢掉**还没被读走**的音频（实测丢接近 100%）。
- **已撤回的线索（2026-09-14 晚）**：曾怀疑「采集端点永远报 48000Hz/2ch」是元凶，
  但注册表 `PKEY_AudioEngine_DeviceFormat` 显示**采集端点的设备格式是正确的**
  （192k/16 线缆 → 192000/16 ✓，96k/24 → 96000/24 ✓）；`GetMixFormat` 对采集端点返回的
  是**音频引擎的共享混音格式**（48k/32f），属正常现象。判定设备侧格式**一律以注册表为准**。
- **诊断环境噪声（重要）**：这台机器积累了大量历史遗留虚拟端点（`2- Virtual Cable xx`、
  `MTX1 uac1` 等），它们在 WASAPI 里仍可能显示为 active，**按名字筛设备的诊断工具会选到它们**；
  本次回环测试用**全新线缆号 27–32** 规避。同理，Windows 按 vhci 端口/设备实例缓存端点属性，
  换格式重测要换线缆号。
- **下一步（待排期，按信息量排序）**：
  1. `examples/vcmeter`：播 1kHz 时用 `IAudioMeterInformation` 读虚拟麦克风端点的**引擎电平** ——
     有电平 ⇒ 数据到了引擎、丢在采集回调；−∞ ⇒ 没进引擎，问题在驱动/端点格式层。**一步砍一半范围。**
  2. `examples/vcks`：**独占模式 / KS 直读**对比（绕过共享引擎）。共享失败、独占成功 ⇒
     共享混音/重采样路径问题。
  3. 服务端加**边界电平日志**：分别打 ISO OUT 解码后与 ISO IN 读出前的 1 秒 RMS，
     钉死"我们发出去的是不是那个正弦"。
  4. 核对 App 里 `usbip://1/capture` 解析到的是**本次 attach 的新实例**，不是历史同名端点。

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
6. **（备选，仅在需要突破 P0 天花板时）换传输层**：如果将来确实要 192k/24bit 或 <15ms 延迟，
   USB/IP 这条路走不通，只能自写**内核音频驱动**（Microsoft 的 `sysvad` 示例 MS-PL /
   ACX AudioCodec 示例为起点，纯用户态做不到创建音频端点）。代价：驱动开发数周、内核调试风险、
   **每个设备实例的增删都需要管理员**（除非连虚拟总线一起写）、以及签名
   （开发用测试签名需关 Secure Boot + `bcdedit /set testsigning on`；分发要 Microsoft
   attestation 或 WHQL）。当前 50–150ms 延迟与 ≤96k/24bit 规格够用，故只记录不排期。

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

### UAC1 类请求（照抄参考实现 Virtual-Cables，写错就起不来）

- 采样率控制挂在**端点**上（recipient=endpoint，wIndex 低字节 = 端点地址），值 **3 字节**：
  `GET_CUR(0x81)` / `GET_MIN(0x82)` / `GET_MAX(0x83)` / `GET_RES(0x84)`、`SET_CUR(0x01)`
  （`device.rs::handle_class`，实测 `usbaudio.sys` 会问 MIN/MAX/RES）。
- 音量/静音走**接口接收方 + Feature Unit 实体**（wIndex 高字节 = 实体 ID）：静音 1 字节，
  音量 2 字节（1/256 dB，-60..0dB），范围同样用分开的 GET_MIN/GET_MAX/GET_RES。
- **绝不能按 UAC2 的 `GET_RANGE` 回整块范围**：那是 8 字节，会被 `wLength=2` 截断 →
  `usbaudio.sys` StartDevice 失败（设备管理器代码 10）。
- 描述符：`bcdUSB=0x0110`、类/子类/协议**全 0**、**不提供 device qualifier**（请求即 STALL）、
  iso `bInterval=1`、包长 = 每毫秒字节数（48k/16 → 192，精确不加余量）、
  播放 `bmAttributes=0x09`、采集 `0x0D`；AC 块 wTotalLength=72；CS 端点 7 字节。
- USB/IP devlist 按**全速**上报（`SPEED_FULL=2`），接口记录的 proto 字节 = 0x00。
- 能力边界：`usbaudio.sys` 按「**每毫秒载荷 ≤ min(wMaxPacketSize, 1024)**（全速单包 1023）」校验格式，
  且 `wMaxPacketSize` **必须是整数个采样帧**（44.1k 系踩过，见 P0）；24bit 立体声上限约 170 kHz。

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
