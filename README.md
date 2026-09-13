# AudioMix

Tauri 2 + Vue 3 音频混合软件：把多个音频源（物理输入设备 / 输出设备的系统 loopback / 虚拟声卡）按路由规则混音，输出到任意输出设备。当前支持 Windows（WASAPI 共享模式），核心引擎通过 trait 抽象，后续可扩展 Linux（PulseAudio/PipeWire）与 macOS（CoreAudio）。

## 功能

- **混音路由**：多源 → 多输出的多对多路由矩阵，每条路由独立增益（dB）/静音，每个输出独立主音量；修改即时生效，不重启音频流
- **电平表**：源与输出的实时峰值电平
- **无 GUI 后台运行**：关闭窗口 = 隐藏到托盘，引擎继续混音；`--headless` 参数完全无窗口启动
- **开机自启**：写入当前用户注册表 Run 键，可选自启即 headless
- **远程控制 API**：本地 REST + SSE（默认关闭，UI 中开启），headless 下也可控制
- **虚拟声卡管理**：检测已装虚拟声卡（VB-Cable、Voicemeeter 等），支持安装开源驱动（VirtualDrivers/Virtual-Audio-Driver，MIT）

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
  ├─ backend.rs    AudioBackend trait（枚举/采集/loopback/渲染）
  ├─ engine.rs     图运行时、流生命周期、热更新（ArcSwap 原子发布，音频线程无锁）
  ├─ resample.rs   拉取式线性重采样器（欠载保持，时钟漂移可扩展）
  ├─ ring.rs       每路由一条 SPSC 无锁环形缓冲
  └─ mixer.rs      混音纯函数（增益累加、软限幅、通道变换，含单元测试）
crates/audiomix-backend-windows WASAPI 实现 + 驱动管理（pnputil/INF）
crates/audiomix-control-api     axum REST + SSE 控制服务
app/src-tauri                   Tauri 壳（托盘/关窗隐藏/命令/配置）
app/src                         Vue 3 + Naive UI 前端
```

数据流：每个源一条采集线程（WASAPI 事件驱动）→ 推入其参与的每条路由的 SPSC 环形缓冲 → 每个输出一条渲染线程，按设备需要的帧数**拉取**：取样本 → 线性重采样到输出采样率 → 通道变换 → 路由增益累加 → 软限幅 → 写设备。

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

## 虚拟声卡

Windows 创建虚拟音频设备需要内核驱动。方案：

1. **第三方驱动**（推荐，最省事）：安装 [VB-Cable](https://vb-audio.com/Cable/) 等，应用自动检测其虚拟设备
2. **开源驱动**：将 [VirtualDrivers/Virtual-Audio-Driver](https://github.com/VirtualDrivers/Virtual-Audio-Driver)（MIT）的 INF/cat 放入安装目录 `drivers/`，在「虚拟声卡」页一键安装（需 UAC；未签名驱动需先 `bcdedit /set testsigning on` 并重启）

安装后虚拟扬声器/麦克风即出现在设备列表，可参与混音路由（如：系统声音 → 虚拟扬声器 → 会议软件虚拟麦克风）。

## 配置

`%APPDATA%\com.audiomix.app\config.json`：混音图 + 设置（控制 API、托盘行为、自启参数）。修改混音图即时持久化。

## 已知事项 / 后续路线

- [ ] Linux / macOS 后端（`AudioBackend` trait 已就位）
- [ ] 时钟漂移自适应比率微调（当前线性重采样 + 欠载保持/溢出丢弃）
- [ ] 设备热插拔自动重建（当前用「刷新」手动重建）
- [ ] 高质量 sinc 重采样
- [ ] 虚拟声卡 INF 多实例（创建多对虚拟设备）
- [ ] 驱动 attestation 签名（免去测试签名）
