# AudioMix 控制 API

AudioMix 内置的本地 REST + SSE 控制服务：脚本、第三方面板、会议/直播自动化都可以用它
读设备、改混音图、实测延迟、看电平、管虚拟声卡——**不需要 GUI，`--headless` 模式下照常可用**。

- 默认监听 `http://127.0.0.1:17643`（设置页可改地址与端口）
- 服务默认关闭：设置页「远程控制 API → 启用」后生效（也可用 API 自己打开，见下）
- `GET /api` 返回**运行时端点清单**（含本机实际启用的能力），本文档是它的完整版

```bash
BASE=http://127.0.0.1:17643

curl -s $BASE/api/health
# {"status":"ok","app":"audiomix","version":"0.1.0","backend":"wasapi","uptime_ms":12345}

curl -s $BASE/api | jq '.endpoints[] | "\(.method) \(.path)"'   # 端点清单
```

## 目录

- [鉴权](#鉴权)
- [错误](#错误)
- [通用约定](#通用约定)
- [端点参考](#端点参考)
  - [服务与状态](#服务与状态) · [设备](#设备) · [混音图](#混音图)
  - [DSP 方块](#dsp-方块) · [延迟实测](#延迟实测) · [设置](#设置)
  - [虚拟声卡](#虚拟声卡) · [日志](#日志)
- [事件流（SSE）](#事件流sse)
- [配方](#配方)
- [客户端示例](#客户端示例)
- [注意事项](#注意事项)

## 鉴权

设置页可生成/填入**访问令牌**。令牌非空时，所有 `/api/*` 请求都要带它，三种方式任选：

| 方式 | 示例 | 适用 |
|---|---|---|
| `Authorization` 头 | `Authorization: Bearer <令牌>` | 默认选择（curl / fetch / requests） |
| `X-Api-Token` 头 | `X-Api-Token: <令牌>` | 不方便用 Authorization 的客户端 |
| query 参数 | `?token=<令牌>` | **SSE**（浏览器 `EventSource` 不能自定义请求头） |

- `/` 与 `/api`（端点索引）**始终公开**，客户端可据此发现是否需要令牌（`auth_required` 字段）。
- 未设令牌 = 不鉴权，只在监听 `127.0.0.1` 时可接受；监听 `0.0.0.0` 且无令牌时设置页会红字警告，启动日志也会告警。
- 令牌与其他设置一起明文存在 `%APPDATA%\com.audiomix.app\config.json`。
- **CORS 默认关闭**：开启后任意网页都能调用本机 API，请同时设令牌。

## 错误

非 2xx 一律是这个结构（`message` 是给人看的中文说明，`kind` 稳定可用于分支）：

```json
{ "error": { "kind": "not_found", "message": "source src-3f2a1b9c 不存在" } }
```

| 状态码 | `kind` | 含义 |
|---|---|---|
| 400 | `bad_request` | 参数/请求体不合法，或混音图校验失败（悬空引用等） |
| 401 | `unauthorized` | 令牌缺失或错误 |
| 404 | `not_found` | 端点、节点 id 或设备不存在 |
| 405 | — | 方法不匹配（框架原生响应，带 `Allow` 头） |
| 409 | `conflict` | 幂等冲突：id 已存在、连线重复、前置条件未满足（服务器没开/没配线缆） |
| 500 | `internal` | 平台侧失败（WASAPI 打不开、提权被取消、USB/IP attach 失败等） |
| 501 | `unsupported` | 该能力需要 App 宿主（纯引擎模式下没有） |

## 通用约定

**设备 id**
- 物理设备：Windows MMDevice 端点 id，形如 `{0.0.0.0.00000000}.{a1b2…}`（含大括号/点号）
- 内置虚拟线路：合成 id `usbip://<线缆号>/playback`（线路输出 = 系统播放端）
  与 `usbip://<线缆号>/capture`（线路输入 = 系统录音端）

> id 里可能含 `/`，因此涉及设备的端点统一用 **`?device_id=` 查询参数**，而不是路径参数。

**节点 id**：`src-…` / `sink-…` / `route-…` / `dsp-…`（创建时自动生成，也可在创建请求里自带 `id`），
是 URL 安全的，可以直接放路径里。

**source.mode**：`"deviceinput"`（采集设备录入）| `"loopback"`（采集输出设备的系统回环）。

**局部更新**：`PATCH` 是「与当前值合并」，只发改动字段；`PUT` 是整体替换（未给字段回默认值）。

**数值钳制**：越界值不会报错，会被钳到合法区间（如 `gain` 0–4、`volume` 0–1、`db` -60–12）。
创建/更新后返回的对象即钳制后的实际值。

**时间**：单位一律毫秒（`uptime_ms`、`interval_ms`、`ms`、`buffer_ms`）。

## 端点参考

### 服务与状态

#### `GET /api` · `GET /`

端点索引 + 版本 + 鉴权状态：`{app, version, api_version, auth_required, auth, events, endpoints[]}`。
客户端可用它做能力探测（不同版本端点不同）。

#### `GET /api/health`

```json
{ "status": "ok", "app": "audiomix", "version": "0.1.0", "backend": "wasapi", "uptime_ms": 812345 }
```

#### `GET /api/status`

后端名 + 各节点实时电平 + 运行统计（电平条/健康检查用）：

```json
{
  "backend": "wasapi",
  "levels": { "src-3f2a1b9c": 0.42, "sink-1a2b3c4d": 0.0, "dsp-9f8e7d6c": 0.31 },
  "stats": {
    "source_dropped": { "src-3f2a1b9c": 0 },
    "sink_underruns": { "sink-1a2b3c4d": 2 },
    "spectra": { "src-3f2a1b9c": [-62.1, -58.4, "…"] }
  }
}
```

- `levels`：峰值 0–1+，键是 source/sink/DSP 节点 id；`levels` 里的值约等于最近一次回调的峰值（毫秒级刷新）。
- `stats.source_dropped`：采集侧因边缓冲满丢弃的**样本数**，持续增长 = 下游消费不及时。
- `stats.sink_underruns`：渲染欠载**次数**，增长 = 上游供数不及时（会听到断音）。
- `stats.spectra`：各节点频段 dB，只有设置里开了 `fft_enabled` 才有内容（否则空对象）。

#### `GET /api/levels`

`{"levels": {...}, "spectra": {...}}`——只要电平不要统计时更省。

#### `GET /api/events`（SSE）

见[事件流](#事件流sse)。

#### `GET /api/stream/levels`（SSE）

按 `?interval_ms=`（20–1000，默认 100）持续推送 `event: levels`，`data` 即 `/api/levels` 的结构：

```
event: levels
data: {"levels":{"src-3f2a1b9c":0.42},"spectra":{}}
```

### 设备

#### `GET /api/devices`

设备列表（读缓存，不重新枚举）：

```json
[{
  "id": "{0.0.0.0.00000000}.{a1b2c3d4-…}",
  "name": "扬声器 (Realtek(R) Audio)",
  "kind": "output",            // "input" | "output"
  "is_default": true,
  "is_virtual": false,
  "channels": 2,
  "sample_rate": 48000
}]
```

#### `POST /api/devices/refresh`

重新枚举设备（热插拔后；顺带跑「默认设备守护」——虚拟线路接入时 Windows 会抢默认设备，
这一步会把系统默认恢复成用户选择）。返回刷新后的完整列表。

#### `POST /api/devices/default`

设为系统默认设备。虚拟线路的合成 id 会自动映射到它在 Windows 里的真实端点。

```json
{ "device_id": "usbip://1/playback" }
```

返回刷新后的设备列表（与 `GET /api/devices` 同结构）。

#### `GET|PUT /api/devices/volume?device_id=<设备 id>`

读/写该端点的 **Windows 系统音量**（0.0–1.0，影响这台设备上的所有声音，不只是混音器）。

```bash
curl -s "$BASE/api/devices/volume?device_id=%7B0.0.0.0.…%7D"
# {"device_id":"{0.0.0.0.…}","level":0.8}

curl -s -X PUT "$BASE/api/devices/volume?device_id=%7B0.0.0.0.…%7D" -d '{"level":0.5}'
```

#### `GET|PUT /api/devices/mute?device_id=<设备 id>`

同上，静音开关（`{"mute": true}`）。

> ⚠️ 对**虚拟线路**（`usbip://N/...`）调音量/静音只写 Windows 端点的控制值，
> **不影响线路的数据通路**。要调线路音量请用路由的 `gain` 或 sink 的 `volume`。

### 混音图

混音图 = `sources`（采集源）+ `sinks`（输出）+ `routes`（连线）+ `processors`（DSP 方块）。
连线两端可以是 source/sink，也可以是 DSP 方块；引擎按「源 → …（途经方块按序串联）→ 汇」解析路径。

```json
{
  "sources": [{
    "id": "src-3f2a1b9c", "name": "线路1",
    "device_id": "usbip://1/playback", "mode": "deviceinput", "enabled": true
  }],
  "sinks": [{
    "id": "sink-1a2b3c4d", "name": "耳机",
    "device_id": "{0.0.0.0.…}", "volume": 0.8, "enabled": true
  }],
  "routes": [{
    "id": "route-7c6b5a49", "source_id": "src-3f2a1b9c", "sink_id": "sink-1a2b3c4d",
    "gain": 0.5, "muted": false, "nodes": []
  }],
  "processors": []
}
```

#### `GET /api/graph` · `PUT /api/graph`

整体读写。`PUT` 会做校验（悬空引用 → 400），并用 diff 应用：只有设备/模式/启停变化的流会被重启。
返回**应用后**的图（引擎可能跳过当前不存在的设备）。

#### `GET|POST /api/sources`

`POST` 加一个采集源：

```json
{ "id": "src-mic", "name": "麦克风", "device_id": "{0.0.0.0.…}", "mode": "deviceinput", "enabled": true }
```

- `id` 可省略（自动生成）
- `name` 省略时用 `device_id`
- `mode` 省略时 `deviceinput`
- 201 Created + 创建的 source 对象

#### `GET|PATCH|DELETE /api/sources/{id}`

`PATCH` 支持 `name` / `enabled` / `mode`；`DELETE` 会**连带删掉挂在它上面的路由**：

```json
{ "removed": "src-mic", "removed_routes": 2 }
```

#### `GET|POST /api/sinks` · `GET|PATCH|DELETE /api/sinks/{id}`

同 source，字段为 `{id?, name?, device_id, volume?, enabled?}`；`PATCH` 支持 `name` / `volume` / `enabled`。

#### `GET|POST /api/routes` · `GET|PATCH|DELETE /api/routes/{id}`

```json
{ "source_id": "src-mic", "sink_id": "dsp-eq", "gain": 0.5 }
```

- 两端必须已存在（source/sink/processor 任一）；不存在 → 404
- 同一对端点重复连线 → 409
- `PATCH` 支持 `gain`（0–4，线性增益）和 `muted`

### DSP 方块

`processor` = 画布上的一个处理节点，形状是 `{ "id": "...", "type": "...", <参数…>, "enabled": true }`。
`enabled: false` = 旁路（跳过该节点，与 `switch` 的「关 = 静音」不同）。

#### `GET|POST /api/processors` · `GET|PUT|DELETE /api/processors/{id}`

`POST` 的 body 就是节点本身（可带 `id`），`PUT` 换掉整个节点参数：

```bash
curl -s -X POST $BASE/api/processors -d '{"id":"dsp-eq","type":"gain","db":-6}'
curl -s -X PUT  $BASE/api/processors/dsp-eq -d '{"type":"eq3","low_gain_db":3,"low_freq":120,"mid_gain_db":-2,"mid_freq":1000,"mid_q":1.0,"high_gain_db":2,"high_freq":8000}'
```

可用类型与参数（超出范围会被钳制）：

| `type` | 参数 | 范围 |
|---|---|---|
| `gain` | `db` | -60 – 12 |
| `delay` | `ms` | 0 – 1000 |
| `eq3` | `low_gain_db` / `low_freq`、`mid_gain_db` / `mid_freq` / `mid_q`、`high_gain_db` / `high_freq` | 增益 -24 – 12；低频 40–500、中频 200–8000、高频 2000–16000；Q 0.3–10 |
| `peak_eq` | `freq` / `gain_db` / `q` | 20–20000 / -24 – 24 / 0.3–10 |
| `graph_eq` | `gains_db`（长度 10，31.25Hz–16kHz，1/3 倍频程） | 每段 -24 – 24 |
| `highpass` / `lowpass` | `freq` / `q` | 20–20000 / 0.3–10 |
| `bandpass` | `low_freq` / `high_freq` | 20–19000 / 30–20000（high ≥ low+10） |
| `switch` | —（开 = 直通，关 = 静音） | |
| `limiter` | `threshold_db` / `release_ms` | -24 – 0 / 10–500 |

删方块同样连带删除挂在它上面的连线。

### 延迟实测

#### `POST /api/latency`

向源注入扫频脉冲 + 相关检测，**实测** source → DSP → sink 全程（不含两端设备缓冲）。
请求二选一：

```json
{ "route_id": "route-7c6b5a49" }
{ "source_id": "src-line1", "sink_id": "sink-headphones" }
```

```json
{ "route_id": "route-7c6b5a49", "ms": 12.4 }
```

> ⏱ 这是**阻塞测量**，一次约 2.5–4 秒（多次注入取中位数）；请把客户端超时设到 10 秒以上。
> 跨线缆测整链（线路1 → 回灌 → 线路2 → 耳机）用第二种写法。

### 设置

#### `GET /api/settings` · `PATCH /api/settings` · `PUT /api/settings`

```json
{
  "control_api": { "enabled": true, "bind": "127.0.0.1", "port": 17643, "token": "", "cors": false },
  "usbip": { "enabled": true, "bind": "127.0.0.1:3240", "cables": [ "…" ] },
  "autostart_headless": true,
  "close_to_tray": true,
  "default_output": "{0.0.0.0.…}",
  "default_input": null,
  "default_output_virtual": false,
  "default_input_virtual": false,
  "resample_quality": "sinc256",
  "edge_buffer_ms": 250,
  "levels_interval_ms": 50,
  "fft_enabled": false,
  "fft_size": 4096,
  "fft_bands": [50, 69, "…", 20000]
}
```

`PATCH` 只发改动字段，例如只改均衡缓冲与频谱开关：

```bash
curl -s -X PATCH $BASE/api/settings -d '{"edge_buffer_ms":500,"fft_enabled":true}'
```

设置改动会即时生效（重采样质量、边缓冲、频谱参数、控制 API 自身、自启注册）。

### 虚拟声卡

#### `GET /api/usbip`

```json
{
  "enabled": true,
  "running": true,
  "bind": "127.0.0.1:3240",
  "local_addr": "127.0.0.1:3240",
  "cables": [{
    "number": 1, "name": "", "display_name": "Virtual Cable 01",
    "sample_rate": 48000, "bits": 16, "mode": "loopback", "buffer_ms": 250,
    "device_id_playback": "usbip://1/playback",
    "device_id_capture": "usbip://1/capture",
    "attached": true, "port": 3
  }],
  "driver": {
    "installed": true,
    "usbip_path": "C:\\Program Files\\USBip\\usbip.exe",
    "installer_path": "…\\USBip-0.9.8.0-x64.exe",
    "installer_embedded": false,
    "test_signing": null,
    "hvci_enabled": true
  },
  "ports": [{ "port": 3, "in_use": true, "bus_id": "1-1", "detail": "…" }],
  "ports_error": null
}
```

#### `PUT /api/usbip/cables`

保存线缆配置并（重）启服务器。`enabled` 省略时保持原开关。

```json
{
  "enabled": true,
  "cables": [
    { "number": 1, "name": "直播线路", "sample_rate": 48000, "bits": 24, "mode": "loopback", "buffer_ms": 250 }
  ]
}
```

| 字段 | 说明 |
|---|---|
| `number` | 线缆号 1–32（决定 busid `1-N` 与 USB PID），决定设备 id `usbip://N/...` |
| `name` | 显示名（作为 USB 产品名出现在 Windows 里），空 = `Virtual Cable NN`，≤64 字 |
| `sample_rate` | **44 100–96 000**；超过 88 200 只支持 16bit |
| `bits` | 16 / 24 / 32（双向共享全速帧预算，每毫秒 ≤1023 字节） |
| `mode` | 线缆两端的拷贝方向：`loopback`（播放进 → 录音出，回环）/ `reverse`（录音进 → 播放出）/ `mixer`（不拷贝：播放端只进混音图，录音端由混音图渲染） |
| `buffer_ms` | 线缆侧环形缓冲 20–5000（大 = 更抗卡顿） |

- 最多 32 条线缆；服务器在跑时**不重绑端口**，只替换线缆表；**改格式/增删线缆后必须在系统侧重 attach** 才生效。
- 启动失败会回滚到上一次的线缆配置，不会把在跑的服务停在半路。
- 非法组合（如 96k/24bit、线缆号重复）→ 400，`message` 会说明原因。

#### `POST /api/usbip/attach` · `POST /api/usbip/detach` · `POST /api/usbip/driver`

- `attach`：先 `detach --all` 再逐条接入系统，**需要管理员权限**（首次会弹 UAC；应用在线时会用已提权的常驻代理，零弹窗）。
  返回 `{host, tcp_port, attached: [["1-1", 3]], failed: [["1-2", "原因"]], detached: [2], log: "原始输出"}`
- `detach`：断开全部已接入线缆（不需管理员），返回 `{"output": "…"}`
- `driver`：安装随包捆绑的 usbip-win2 驱动（**需要管理员**，UAC），返回 `{"output": "…"}`

> 前置条件未满足（服务器没开 / 没配线缆）→ 409；提权被用户取消 → 500。
> 重复 attach 会在 Windows 里堆出 `(2- Virtual Cable 01)` 之类重复端点——先 `detach` 再 `attach` 即可清理。

### 日志

#### `GET /api/logs?since=<seq>&limit=<n>`

增量拉取运行日志（与设置页日志面板同源）：

```json
{
  "lines": [{ "seq": 8123, "text": "INFO audiomix: …" }],
  "next": 8123,
  "truncated": false
}
```

下次带上 `since=next` 即可续取。`limit` 默认 500、上限 5000；`truncated: true` 表示这一批被截断、
`next` 停在本批最后一行，接着取即可（日志同时落盘 `%APPDATA%\com.audiomix.app\audiomix.log`）。

## 事件流（SSE）

`GET /api/events` 推送引擎事件，`text/event-stream`，带 keep-alive 注释帧：

| 事件名 | 触发时机 | `data` |
|---|---|---|
| `graph_applied` | 混音图被应用（流有启停/重建） | `{"type":"graph_applied"}` |
| `devices_changed` | 设备列表变化（热插拔/线缆接入） | `{"type":"devices_changed"}` |
| `underrun` | 某 sink 渲染欠载 | `{"type":"underrun","sink_id":"sink-1a2b3c4d"}` |

```bash
curl -N "$BASE/api/events?token=$TOKEN"
```

- 界面（或别的客户端）改图也会广播 `graph_applied`，所以这是「图变了」的统一信号。
- 应用**不接受**从 API 反向触发界面动作：界面收到 `graph_changed`（内部事件）后自行重拉。
- 广播通道满时会丢旧事件——收到的是「发生过」而不是完整序列；重连后请用
  `GET /api/graph` + `GET /api/status` 重新对齐状态。
- 电平请用 `/api/stream/levels`，不要混用 `/api/events`。

## 配方

**1. 把「线路 1」接进耳机并测延迟**

```bash
BASE=http://127.0.0.1:17643

SRC=$(curl -s -X POST $BASE/api/sources -d '{"device_id":"usbip://1/playback","name":"线路1"}' | jq -r .id)
SNK=$(curl -s -X POST $BASE/api/sinks   -d '{"device_id":"{0.0.0.0.…}","name":"耳机"}'    | jq -r .id)
RT=$(curl  -s -X POST $BASE/api/routes  -d "{\"source_id\":\"$SRC\",\"sink_id\":\"$SNK\",\"gain\":0.5}" | jq -r .id)

curl -s -X POST $BASE/api/latency -d "{\"route_id\":\"$RT\"}"   # {"route_id":"…","ms":12.4}
```

**2. 插入一个 -6dB 增益方块并串进这条线（先删掉直连，再走方块）**

```bash
curl -s -X POST $BASE/api/processors -d '{"id":"dsp-gain","type":"gain","db":-6}'
curl -s -X DELETE $BASE/api/routes/$RT
curl -s -X POST $BASE/api/routes -d "{\"source_id\":\"$SRC\",\"sink_id\":\"dsp-gain\"}"
curl -s -X POST $BASE/api/routes -d "{\"source_id\":\"dsp-gain\",\"sink_id\":\"$SNK\"}"
```

**3. 静音 / 调音量 / 切换开关**

```bash
curl -s -X PATCH $BASE/api/routes/$RT -d '{"muted":true}'
curl -s -X PATCH $BASE/api/sinks/$SNK  -d '{"volume":0.3}'
curl -s -X PATCH $BASE/api/processors/dsp-gain -d '{"type":"gain","db":0,"enabled":false}'  # 旁路
```

**4. 远程电平表**

```bash
curl -N "$BASE/api/stream/levels?interval_ms=50"     # event: levels / data: {...}
```

**5. 无 GUI 脚本化启用虚拟线缆**

```bash
curl -s -X PUT $BASE/api/usbip/cables -d '{"enabled":true,"cables":[
  {"number":1,"name":"直播","sample_rate":48000,"bits":24,"mode":"loopback","buffer_ms":250}]}'
curl -s -X POST $BASE/api/usbip/attach      # 可能弹一次 UAC
curl -s $BASE/api/devices | jq '.[] | select(.is_virtual) | .name'
```

## 客户端示例

**JavaScript（浏览器需开启 CORS，或用同源页面/Node）**

```js
const BASE = "http://127.0.0.1:17643";
const TOKEN = "";                       // 没设令牌就留空
const H = TOKEN ? { Authorization: `Bearer ${TOKEN}` } : {};

// 加一条路由
const res = await fetch(`${BASE}/api/routes`, {
  method: "POST",
  headers: { ...H, "Content-Type": "application/json" },
  body: JSON.stringify({ source_id: "src-line1", sink_id: "sink-headphones", gain: 0.8 }),
});
if (!res.ok) throw new Error((await res.json()).error.message);

// 电平表（EventSource 不能带头，令牌放 query）
const es = new EventSource(`${BASE}/api/stream/levels?interval_ms=100${TOKEN ? `&token=${TOKEN}` : ""}`);
es.addEventListener("levels", (e) => console.log(JSON.parse(e.data).levels));
```

**Python**

```python
import json, urllib.request

BASE, TOKEN = "http://127.0.0.1:17643", ""

def call(method, path, body=None):
    req = urllib.request.Request(
        BASE + path, method=method,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"Content-Type": "application/json",
                 **({"Authorization": f"Bearer {TOKEN}"} if TOKEN else {})})
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        raise RuntimeError(json.loads(e.read())["error"]["message"])

call("PATCH", "/api/routes/route-7c6b5a49", {"muted": True})
print(call("GET", "/api/status")["levels"])
```

## 注意事项

- **哪些端点需要宿主**：`/api/settings`、`/api/devices/{volume,mute,default}`、`/api/usbip/*`、`/api/logs`
  由 App 侧实现（涉及配置读写 / Windows 策略 / 外部进程）。纯引擎模式（测试、嵌入式）下这些返回 501。
- **改 `control_api` 自身**（开关/端口/令牌/CORS）会重启服务：响应会先发出去、重启延后约 0.4 秒执行，
  但连接可能被掐断——客户端应重连后重试。
- **`attach` / `driver` 会弹 UAC**：脚本里请预留时间并处理失败（用户点「否」→ 500）。
  应用在线且常驻代理已提权时，`attach` 不再弹窗。
- **`/api/latency` 阻塞 2.5–4 秒**，别放在 UI 主线程或短超时的请求里。
- **虚拟线路的 Windows 音量不生效**（见[设备](#设备)小节），用混音图里的 `gain`/`volume`。
- **改线缆格式/增删线缆要重新 `attach`**，否则 Windows 侧还是旧描述符。
- **API 改图后界面会自动重拉**；反过来界面改图会广播 `graph_applied`。两边同时改仍以「后写」为准，
  但没有静默覆盖。
- 端到端加密/HTTPS 未提供：这是本机 API，跨机访问请自行套隧道（SSH 端口转发等）并务必设令牌。
