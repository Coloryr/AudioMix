# AudioMix

Tauri 2 + Vue 3 音频混合软件：把多个音频源（物理输入设备 / 输出设备的系统 loopback / **自带虚拟声卡**）按路由规则混音，输出到任意输出设备；并内置一个 **USB/IP 虚拟声卡**，让任意 Windows 应用（播放器、会议软件、浏览器）都能把声音送进混音器、或从混音器录音。

当前支持 Windows（WASAPI 共享模式），核心引擎通过 trait 抽象，后续可扩展 Linux（PulseAudio/PipeWire）与 macOS（CoreAudio）。

## 功能

- **拖拽接线画布**：设备面板拖入生成节点，节点间连线即路由（严格单向：输出端子 → 输入端子）；点连线调混音音量/**实测路径延迟**/断开，点节点调采集开关与输出音量；**DSP 节点（开关/增益/延迟/EQ 等）的旁路开关直接放在节点上**，增益/延迟还有内嵌滑杆，详细参数走节点悬浮面板；布局自动持久化
- **延迟实测**：向源节点注入扫频脉冲 + 相关检测（含边缓冲/重采样/DSP 全程，非估算）。两种用法：点连线测单条路径；右键画布进入「延迟测试」模式选两个节点测整链——跨线缆（如 线路1 → 回灌 → 线路2 → 耳机）也能一次测出
- **USB/IP 虚拟声卡**（本项目的核心特性）：应用内置 USB/IP 服务器仿真一条 USB 声卡线缆，配合 usbip-win2 接入 Windows，系统自带驱动暴露成标准端点：
  - `扬声器 (Virtual Cable NN)` ＝ **线路输出**（别的软件往这里播放，声音进混音器）
  - `麦克风 (Virtual Cable NN)` ＝ **线路输入**（混音器写这里，别的软件从这里录）
  - 每条线缆可配置名称（作为 USB 产品名显示在 Windows 里）、格式（采样率/位深）与接线方式（loopback / mixer）
  - **内置线路是 UAC1**（系统自带 `usbaudio.sys`，USB 1.1 全速）：
    支持 **44.1–96kHz 的 16/24/32bit**，**176.4/192kHz 只支持 16bit**
    （上限来自 Microsoft 的 UDE 类扩展：iso 端点服务间隔不得短于 1ms ⇒ 每端点 ≤1024 B/ms，见 TASK P0）
  - ⚠️ 当前状态：设备创建、端点格式、播放/录音流打开、播放方向数据通路都已实测通过；
    **虚拟麦克风（回环出音）尚未通过**，见「已知事项」P1
  - 需要更高规格（192kHz/24bit 等）或更低延迟的线路，请自行安装第三方虚拟声卡（VB-CABLE、VoiceMeeter 等）——
    它们会作为普通 Windows 端点出现在混音页设备列表，直接拖进画布接线即可
- **Windows 默认设备管理**：混音页可直接把某个端点设为系统默认播放/录音设备；常驻守护会在虚拟线路接入时恢复用户偏好（不会被虚拟线缆抢走）
- **电平表与频谱**：源与输出的实时峰值电平（绿→黄→红分段 + 峰值保持），FFT 频谱可开关
- **日志面板**：设置页可查看并复制运行日志（同时落盘 `audiomix.log`）
- **无 GUI 后台运行**：关闭窗口 = 销毁 WebView 释放资源（引擎继续混音），点托盘重新打开时重建窗口；`--headless` 完全无窗口启动
- **单实例**：重复启动唤醒已有窗口
- **开机自启**：写入当前用户注册表 Run 键，可选自启即 headless
- **远程控制 API**：本地 REST + SSE（默认关闭，UI 中开启），headless 下也可控制

## 构建

依赖：Rust (MSVC)、Node 20+、pnpm、WebView2。

```bash
pnpm install        # 在 app/ 目录
pnpm tauri build    # 产出 NSIS 安装包 + 便携 exe（app/src-tauri/target/release/…）
pnpm tauri dev      # 开发调试
```

CI（`.github/workflows/release.yml`）：推送 `v*` 标签自动在 GitHub Actions 上构建
Windows 可执行文件并发布到 Release（产物 `AudioMix-版本-x64.exe`，USB/IP 驱动安装包已内嵌）。

只构建可执行文件（CI 使用同样命令）：

```bash
cd app && npm ci && npm run build   # 前端产物被编译期嵌入，必须先构建
cd .. && cargo build --release -p audiomix-app
```

## 架构

```
crates/audiomix-core            平台无关引擎库
  ├─ backend.rs    AudioBackend trait（枚举/采集/loopback/渲染）+ CompositeBackend
  ├─ engine.rs     图运行时、流生命周期、热更新（ArcSwap 原子发布，音频线程无锁）
  ├─ resample.rs   拉取式线性重采样器（欠载保持，时钟漂移可扩展）
  ├─ ring.rs       每路由一条 SPSC 无锁环形缓冲
  ├─ probe.rs      延迟测量（扫频脉冲注入 + 相关检测，跨采样率模板）
  └─ mixer.rs      混音纯函数（增益累加、软限幅、通道变换，含单元测试）
crates/audiomix-backend-windows WASAPI 实现 + Windows 策略 + USB/IP 虚拟声卡
  ├─ wasapi/       WASAPI 采集/渲染（事件驱动 + MMCSS「Pro Audio」）
  ├─ policy.rs     Windows 默认设备切换（IPolicyConfig）、端点音量/静音
  └─ usbip/        USB/IP 服务器与虚拟线缆
      ├─ protocol.rs    USB/IP v1.1.1 线格式
      ├─ descriptors.rs UAC1 描述符构建（按采样率/位深参数化）
      ├─ device.rs      线缆状态机：类请求 + PCM↔f32 + 播放/录音环形缓冲
      ├─ server.rs      tokio 服务：devlist/import 握手、EP0 串行、iso URB 节拍
      ├─ ring.rs        drop-oldest / silence-fill 环形缓冲
      ├─ backend.rs     线缆 ↔ 引擎设备（usbip://N/playback|capture）
      └─ attach.rs      usbip.exe 调用（install/attach/detach/port）
crates/audiomix-control-api     axum REST + SSE 控制服务
app/src-tauri                   Tauri 壳（托盘/关窗销毁 WebView/命令/配置/日志缓冲）
app/src                         Vue 3 + Naive UI 前端（混音 / 虚拟声卡 / 设置）
```

数据流：

```
Windows 应用 → 扬声器 (Virtual Cable NN) → usbip-win2 → ISO OUT PCM
  → play_ring →（a）混音器 Source →（路由/增益/混音）→ Sink → 物理输出设备
              →（b）loopback 模式：同份数据 → cap_ring → ISO IN → 麦克风 (Virtual Cable NN)
混音器 Sink（线路输出）→ cap_ring → ISO IN → 麦克风 (Virtual Cable NN) → 应用录音
```

引擎侧：每个源一条采集线程（WASAPI 事件驱动，虚拟线缆则由 ISO OUT 数据驱动）→ 推入其参与的每条路由的 SPSC 环形缓冲 → 每个输出一条渲染线程，按设备需要的帧数**拉取**：取样本 → 重采样到输出采样率 → 通道变换 → 路由增益累加 → 软限幅 → 写设备。

## 虚拟声卡（USB/IP）

不装任何内核驱动：应用自己仿真 USB 声卡，用 [usbip-win2](https://github.com/vadimgrn/usbip-win2)（微软签名、HVCI 兼容）把设备接入 Windows，由系统自带的 `usbaudio.sys`（UAC1）暴露成标准音频端点。

1. 「虚拟声卡」页 → **一键安装 USB/IP 驱动**（安装包已内嵌进 exe，无需单独下载；需要一次 UAC）
2. 添加/配置线路（名称、采样率、位深、接线方式）→ **保存并应用**
3. **附加全部**（需要一次 UAC；此后 attach/detach 都不再需要提权）
4. 系统声音设置里即出现 `扬声器/麦克风 (Virtual Cable NN)`，可参与混音路由

USB/IP 服务器默认监听 `127.0.0.1:3240`，端口可在「设置」页修改（服务器运行中不可改，输入时自动检测端口占用）。

格式上限：**44.1–96kHz 的 16/24/32bit，176.4/192kHz 只 16bit**（界面上超出规格的组合会被禁用/拦截）。
更高规格请自装第三方虚拟声卡，它们同样能拖进混音画布当节点用。

> **验证状态**：上述格式矩阵已实测到「Windows 里设备出现 + 端点设备格式正确 + 播放/录音流都能打开
> + 播放方向数据确实流到设备」；但**虚拟麦克风的回环出音尚未通过**（见「已知事项」P1）。

> 建议只保留一处附加：每次 `usbip attach` 都会新占一个 vhci 端口，重复附加会在 Windows 里堆出
> `(2- Virtual Cable NN)`、`(3- …)` 之类的重复端点；`断开全部`（`usbip detach --all`）即可清理，无需管理员。

## 控制 API

设置页开启后监听 `127.0.0.1:17643`（可改端口）：

| 端点 | 说明 |
|---|---|
| `GET /api/health` | 健康检查 |
| `GET /api/devices` | 刷新并返回设备列表 |
| `GET /api/graph` | 当前混音图 |
| `PUT /api/graph` | 整体应用混音图（body 为 GraphConfig JSON） |
| `GET /api/status` | 后端名、各节点电平、欠载/丢帧统计 |
| `GET /api/events` | SSE：`graph_applied` / `devices_changed` / `underrun` |

## 配置

`%APPDATA%\com.audiomix.app\config.json`：混音图（源/汇/路由/画布布局）+ 设置（控制 API、USB/IP 线缆、默认设备偏好、托盘与自启）。修改混音图即时持久化；运行日志在同目录 `audiomix.log`。
