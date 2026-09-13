# AudioMix

Tauri 2 + Vue 3 音频混合软件：把多个音频源（物理输入设备 / 输出设备的系统 loopback / **自带虚拟声卡**）按路由规则混音，输出到任意输出设备；并内置一个 **USB/IP 虚拟声卡**，让任意 Windows 应用（播放器、会议软件、浏览器）都能把声音送进混音器、或从混音器录音。

当前支持 Windows（WASAPI 共享模式），核心引擎通过 trait 抽象，后续可扩展 Linux（PulseAudio/PipeWire）与 macOS（CoreAudio）。

## 功能

- **拖拽接线画布**：设备面板拖入生成节点，节点间连线即路由（严格单向：输出端子 → 输入端子）；点连线调增益/静音/断开，点节点调采集开关与输出音量；布局自动持久化
- **USB/IP 虚拟声卡**（本项目的核心特性）：应用内置 USB/IP 服务器仿真一条 USB 声卡线缆，配合 usbip-win2 接入 Windows，系统自带驱动暴露成标准端点：
  - `扬声器 (Virtual Cable NN)` ＝ **线路输出**（别的软件往这里播放，声音进混音器）
  - `麦克风 (Virtual Cable NN)` ＝ **线路输入**（混音器写这里，别的软件从这里录）
  - 每条线缆可配置名称（作为 USB 产品名显示在 Windows 里）、采样率（44.1–192kHz）、位深（16/24/32）、协议（UAC1/UAC2）与接线方式（loopback / mixer）
  - **UAC1（`usbaudio.sys`，全速）已打通并真机验证**；UAC2（`usbaudio2.sys`，高速 192K）仍在开发，见「后续路线」
- **Windows 默认设备管理**：混音页可直接把某个端点设为系统默认播放/录音设备；常驻守护会在虚拟线路接入时恢复用户偏好（不会被虚拟线缆抢走）
- **电平表**：源与输出的实时峰值电平
- **日志面板**：设置页可查看并复制运行日志（同时落盘 `audiomix.log`）
- **无 GUI 后台运行**：关闭窗口 = 隐藏到托盘，引擎继续混音；`--headless` 完全无窗口启动
- **开机自启**：写入当前用户注册表 Run 键，可选自启即 headless
- **远程控制 API**：本地 REST + SSE（默认关闭，UI 中开启），headless 下也可控制

## 构建

依赖：Rust (MSVC)、Node 20+、pnpm、WebView2。

```bash
pnpm install        # 在 app/ 目录
pnpm tauri build    # 产出 NSIS 安装包 + 便携 exe（app/src-tauri/target/release/…）
pnpm tauri dev      # 开发调试
```

纯后端构建（不带 GUI）：

```bash
cargo build --release -p audiomix-app
```

## 架构

```
crates/audiomix-core            平台无关引擎库
  ├─ backend.rs    AudioBackend trait（枚举/采集/loopback/渲染）+ CompositeBackend
  ├─ engine.rs     图运行时、流生命周期、热更新（ArcSwap 原子发布，音频线程无锁）
  ├─ resample.rs   拉取式线性重采样器（欠载保持，时钟漂移可扩展）
  ├─ ring.rs       每路由一条 SPSC 无锁环形缓冲
  └─ mixer.rs      混音纯函数（增益累加、软限幅、通道变换，含单元测试）
crates/audiomix-backend-windows WASAPI 实现 + Windows 策略 + USB/IP 虚拟声卡
  ├─ wasapi/       WASAPI 采集/渲染（事件驱动 + MMCSS「Pro Audio」）
  ├─ policy.rs     Windows 默认设备切换（IPolicyConfig）、端点音量/静音
  └─ usbip/        USB/IP 服务器与虚拟线缆
      ├─ protocol.rs    USB/IP v1.1.1 线格式
      ├─ descriptors.rs UAC1/UAC2 描述符构建（按采样率/位深参数化）
      ├─ device.rs      线缆状态机：类请求 + PCM↔f32 + 播放/录音环形缓冲
      ├─ server.rs      tokio 服务：devlist/import 握手、EP0 串行、iso URB 节拍
      ├─ ring.rs        drop-oldest / silence-fill 环形缓冲
      ├─ backend.rs     线缆 ↔ 引擎设备（usbip://N/playback|capture）
      └─ attach.rs      usbip.exe 调用（install/attach/detach/port）
crates/audiomix-control-api     axum REST + SSE 控制服务
app/src-tauri                   Tauri 壳（托盘/关窗隐藏/命令/配置/日志缓冲）
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

不装任何内核驱动：应用自己仿真 USB 声卡，用 [usbip-win2](https://github.com/vadimgrn/usbip-win2)（微软签名、HVCI 兼容）把设备接入 Windows，由系统自带的 `usbaudio.sys` / `usbaudio2.sys` 暴露成标准音频端点。

1. 「虚拟声卡」页 → **一键安装 USB/IP 驱动**（安装包随应用捆绑，需要一次 UAC）
2. 添加/配置线路（名称、采样率、位深、协议、接线方式）→ **保存并应用**
3. **附加全部**（需要一次 UAC；此后 attach/detach 都不再需要提权）
4. 系统声音设置里即出现 `扬声器/麦克风 (Virtual Cable NN)`，可参与混音路由

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

## 已知事项 / 后续路线

按优先级（详细排查记录与结论见 `TASK.md`）：

- [ ] **P0 · UAC2 打通**：设备能枚举（`usbaudio2.sys`，Problem 0），但 Windows 拿不到「设备格式」
      （`GetMixFormat` → `AUDCLNT_E_UNSUPPORTED_FORMAT`），驱动在 `SET_CUR(采样率)` 与
      `SET_INTERFACE(alt0/alt1)` 之间死循环。UAC1（`usbaudio.sys`）正常，192K/24bit 需要 UAC2
- [ ] **P1 · 虚拟麦克风应用侧收不到声音**：设备侧实测完美（100 URB/s、187 KB/s、峰值 0.25、零样本 0.1%），
      但应用录音得到 ≈ −57 dB 的近似静音，怀疑驱动/音频引擎把采集流判为静音
- [ ] **P2 · 功能节点**：开关 / 延迟 / 强度 / 均衡器 / 高通 / 低通 / 带通（需要引擎侧 DSP 节点）
- [ ] **P2 · 节点音量指示条**（绿→黄→红分段阈值 + 峰值保持）与 **FFT 频谱展示（可开关）**
- [ ] **P2 · 界面**：设备面板与画布等高、虚拟声卡线路两行布局、画布自动排布、缩放时节点位置保持、
      节点文字换行、连线箭头不出圈、拉线虚线跟随
- [ ] **P3 · 产品化**：开机自启做到「无窗口、无 WebView、自动恢复线缆附加」；提供**不依赖 WebView2** 的
      headless 运行模式；单实例；便携版（配置/日志跟随 exe）
- [ ] Linux / macOS 后端（`AudioBackend` trait 已就位）
- [ ] 高质量 sinc 重采样、时钟漂移自适应比率微调
- [ ] 驱动 attestation 签名（免去测试签名与 UAC 提示）
