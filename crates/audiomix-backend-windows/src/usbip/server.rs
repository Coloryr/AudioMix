//! USB/IP TCP 服务器：devlist/import 握手 + URB 处理循环。
//!
//! 关键点（移植自 Virtual-Cables internal/usbip/server.go）：
//! - EP0 控制请求串行处理，保证 USB 枚举顺序；
//! - iso SUBMIT 派发独立任务，并按"每端点 1ms 帧预算"预留完成时刻——
//!   Windows 会一次性排入多个 10ms URB，若各任务独立 sleep 会整批同时完成，
//!   导致采集端 10 倍速跑完；
//! - 写半边加锁串行（多任务并发应答同一 TCP 连接）；
//! - UNLINK 取消排队中的 iso 任务。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;

use super::device::{Cable, SetupPacket};
use super::protocol::{
    self, BasicHeader, IsoPacket, SubmitRequest, DIRECTION_IN, DIRECTION_OUT,
    STATUS_CONN_RESET, STATUS_INVALID, STATUS_OK, STATUS_PIPE,
};

/// 管理连接握手超时
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// 单批 iso 完成预算上限（毫秒）
const MAX_BATCH_MS: u32 = 250;
/// iso 完成相对节拍的提前量：给驱动/音频引擎留缓冲余量。
///
/// 实测线缆环形缓冲的稳态占用只有 2–20ms（约等于这个提前量 + 主机自身队列），
/// 抖动一大就取空 → 补静音 → 听感是「咔」一下。20ms 足以吸收 10ms 级的抖动。
/// 它只是固定相位偏移（相邻两批仍严格相隔一个服务间隔），**不影响播放速度**。
const LEAD_MS: u64 = 50;
/// 落后超过这个量就**重新对齐**节拍，而不是让主机一次性追平。
///
/// 反面教材：原来落后只做封顶、追平时每批都"立刻完成"，主机随即按 CPU 速度灌数据 ——
/// 实测播放速度变快，而且线缆环缓冲一次丢掉 **562868 个样本（≈5.9 秒音频）**。
/// 现在落后超过 30ms 就丢弃这段欠账、以当前时刻重新起拍：宁可有一次性小跳，
/// 也不要变速 + 灌爆缓冲。
const RESYNC_MS: u64 = 30;

pub async fn serve(listener: TcpListener, registry: Arc<super::CableRegistry>) {
    // 连接任务集中登记在 JoinSet 里：accept 循环一旦结束（服务器被 abort / drop），
    // JoinSet 被丢弃时会把这些会话一起 abort 掉。
    //
    // 这一步很关键：usbip-win2 的内核端会把 import 出来的 TCP 连接长期攥在手里，
    // 如果会话任务活过服务器本身，本地 127.0.0.1:3240 就被那条连接占着，
    // 重启服务器 bind 会直接失败（用户看到的「端口已被占用」）。
    let mut conns = tokio::task::JoinSet::new();
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("USB/IP accept 失败: {e}");
                continue;
            }
        };
        tracing::debug!("USB/IP 连接来自 {peer}");
        // 顺手回收已结束的会话，别让 JoinSet 里堆着历史结果
        while conns.try_join_next().is_some() {}
        conns.spawn(handle_connection(stream, registry.clone()));
    }
}

async fn handle_connection(stream: TcpStream, registry: Arc<super::CableRegistry>) {
    let _ = stream.set_nodelay(true);

    let (mut reader, mut writer) = stream.into_split();
    let header = tokio::time::timeout(HANDSHAKE_TIMEOUT, protocol::read_op_header(&mut reader))
        .await
        .ok()
        .and_then(|r| r.ok());

    let Some(header) = header else {
        tracing::debug!("USB/IP 握手失败或超时");
        return;
    };
    if header.version != protocol::PROTOCOL_VERSION {
        tracing::warn!("USB/IP 版本不支持: 0x{:04x}", header.version);
        return;
    }

    match header.code {
        protocol::OP_REQ_DEVLIST => {
            let cables = registry.read().clone();
            tracing::info!("USB/IP 设备列表请求（{} 条线缆）", cables.len());
            if write_devlist(&mut writer, &cables).await.is_err() {
                tracing::warn!("USB/IP devlist 应答失败");
            }
        }
        protocol::OP_REQ_IMPORT => {
            let mut bus_raw = [0u8; 32];
            if reader.read_exact(&mut bus_raw).await.is_err() {
                return;
            }
            let bus_id = String::from_utf8_lossy(&bus_raw)
                .trim_end_matches('\0')
                .to_string();
            let cable = registry.read().iter().find(|c| c.bus_id == bus_id).cloned();
            let Some(cable) = cable else {
                tracing::warn!("USB/IP import 请求未知 busid {bus_id:?}");
                let _ = protocol::write_op_header(&mut writer, protocol::OP_REP_IMPORT, 1).await;
                return;
            };
            if protocol::write_op_header(&mut writer, protocol::OP_REP_IMPORT, 0)
                .await
                .is_err()
            {
                return;
            }
            if write_usb_device(&mut writer, &cable).await.is_err() {
                return;
            }
            tracing::info!("USB/IP import 接受: {} ({})", cable.bus_id, cable.product_name());
            handle_urbs(reader, writer, cable).await;
        }
        _ => {
            tracing::debug!("USB/IP 不支持的管理操作码 0x{:04x}", header.code);
        }
    }
}

async fn write_devlist(w: &mut tokio::net::tcp::OwnedWriteHalf, cables: &[Arc<Cable>]) -> std::io::Result<()> {
    protocol::write_op_header(w, protocol::OP_REP_DEVLIST, 0).await?;
    w.write_all(&(cables.len() as u32).to_be_bytes()).await?;
    for c in cables {
        write_usb_device(w, c).await?;
        // devlist 应答为活动配置中的每个接口写一条 class/subclass/proto 记录：
        // 接口 0 = AudioControl，接口 1/2 = AudioStreaming
        // （UAC2 的 bInterfaceProtocol = 0x20；UAC1 = 0x00）
        let iproto = match c.descriptors.protocol() {
            crate::usbip::descriptors::CableProtocol::Uac1 => 0x00u8,
            crate::usbip::descriptors::CableProtocol::Uac2 => 0x20,
        };
        for (class, subclass) in [(0x01u8, 0x01u8), (0x01, 0x02), (0x01, 0x02)] {
            w.write_all(&[class, subclass, iproto, 0]).await?;
        }
    }
    Ok(())
}

async fn write_usb_device(w: &mut tokio::net::tcp::OwnedWriteHalf, c: &Cable) -> std::io::Result<()> {
    let mut path = [0u8; 256];
    protocol::fixed_string(&mut path, &format!("/sys/devices/platform/audiomix/{}", c.bus_id));
    let mut bus = [0u8; 32];
    protocol::fixed_string(&mut bus, &c.bus_id);
    w.write_all(&path).await?;
    w.write_all(&bus).await?;
    let dev = &c.descriptors.get(0x01, 0).unwrap_or_default();
    let mut frame = Vec::with_capacity(32);
    frame.extend_from_slice(&1u32.to_be_bytes()); // busnum
    frame.extend_from_slice(&(c.cfg.number as u32).to_be_bytes()); // devnum
    // UAC1（USB 1.1 全速）必须按全速上报，否则主机按高速的包/帧语义解析描述符会失败
    let speed = match c.descriptors.protocol() {
        crate::usbip::descriptors::CableProtocol::Uac1 => protocol::SPEED_FULL,
        crate::usbip::descriptors::CableProtocol::Uac2 => protocol::SPEED_HIGH,
    };
    frame.extend_from_slice(&speed.to_be_bytes());
    // 描述符里的多字节字段本身是小端，按协议要求转大端写出
    for o in [8usize, 10, 12] {
        let v = u16::from_le_bytes([dev[o], dev[o + 1]]);
        frame.extend_from_slice(&v.to_be_bytes());
    }
    frame.push(dev[4]); // bDeviceClass
    frame.push(dev[5]); // bDeviceSubClass
    frame.push(dev[6]); // bDeviceProtocol
    frame.push(1); // bConfigurationValue
    frame.push(dev[17]); // bNumConfigurations
    frame.push(3); // bNumInterfaces
    w.write_all(&frame).await
}

/// 每端点的 iso 完成时间线：按端点的真实服务间隔（bInterval 决定，0.125–1ms）预留
struct IsoTimeline {
    next: Mutex<HashMap<u32, Instant>>,
    /// 一个 iso 包代表的服务间隔（微秒）
    packet_micros: u64,
}

impl IsoTimeline {
    fn new(packet_micros: u64) -> Self {
        Self { next: Mutex::new(HashMap::new()), packet_micros }
    }

    /// 为一个 iso 批次预留完成时刻。每包 = 一个服务间隔
    /// （bInterval=4 → 1ms、3 → 0.5ms、2 → 0.25ms、1 → 0.125ms），
    /// 一批的时长封顶 MAX_BATCH_MS。
    ///
    /// **绝对节拍**：基准取「上一次排定的完成时刻」，即使它已经过去也照用，
    /// 这样主机只会在需要时追平，不会因为我们的完成迟到而永久掉速率。
    ///
    /// 反面教材（曾经的写法 `max(上次, now) + duration`）：tokio 定时器 + 调度
    /// 每条完成都会迟到约 1ms，于是 URB 周期变成 10ms+1ms —— 主机（队列深度 1 时）
    /// 每秒只送得出 ~88 条 10ms 的 URB = **每秒少 12% 音频**，实测正是 880 包/秒而不是
    /// 1000，表现为声音变慢约 12% 且一卡一卡（Windows 侧还会不停
    /// `CLEAR_FEATURE(ENDPOINT_HALT)` 试图恢复播放端点）。
    ///
    /// 再提前 `LEAD_MS` 完成：驱动/音频引擎的缓冲因此总有一点余量，
    /// 我们晚个几百微秒也不至于让它欠载（提前量是固定相位偏移，
    /// 相邻两批仍严格相隔一个服务间隔，所以**不影响播放速度**）。
    ///
    /// 落后超过 `RESYNC_MS` 就丢弃欠账、以当前时刻重新起拍（见常量注释）。
    fn reserve(&self, endpoint: u32, packets: u32, now: Instant) -> Instant {
        if packets == 0 {
            return now;
        }
        let micros = packets as u64 * self.packet_micros;
        let duration = Duration::from_micros(micros.min(MAX_BATCH_MS as u64 * 1000));
        let lead = Duration::from_millis(LEAD_MS.min((duration.as_millis() as u64) / 2 + 1));
        let mut next = self.next.lock();
        let base = next.get(&endpoint).copied().unwrap_or(now);
        // 时间线里存的是**未偏移**的节拍，提前量只在返回时减一次
        // （存偏移后的值会让提前量逐批累积，步长变成 duration − lead → 播放变快）。
        let mut schedule = base + duration;
        let deadline = schedule.checked_sub(lead).unwrap_or(now);
        let behind = now.saturating_duration_since(deadline);
        let deadline = if behind > Duration::from_millis(RESYNC_MS) {
            schedule = now + duration;
            schedule - lead
        } else {
            deadline
        };
        next.insert(endpoint, schedule);
        deadline
    }
}

/// 排队中的 iso 任务（seq → abort），供 UNLINK / 断连取消
#[derive(Default)]
struct PendingMap {
    inner: Mutex<HashMap<u32, AbortHandle>>,
}

impl PendingMap {
    fn insert(&self, seq: u32, handle: AbortHandle) {
        self.inner.lock().insert(seq, handle);
    }

    /// 取消指定 seq 的任务；返回是否存在
    fn cancel(&self, seq: u32) -> bool {
        self.inner.lock().remove(&seq).map(|h| h.abort()).is_some()
    }

    fn cancel_all(&self) {
        for (_, h) in self.inner.lock().drain() {
            h.abort();
        }
    }
}

struct ConnState {
    /// 写半边串行锁（iso 任务并发应答同一连接）；异步锁，guard 可跨 await 持有
    write: tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>,
    timeline: IsoTimeline,
    pending: PendingMap,
    /// ISO OUT 到达统计（诊断卡顿：数据"没来"还是"来的是静音"）
    out_stats: OutStats,
    /// ISO IN（麦克风）读取统计
    in_stats: InStats,
}

/// ISO OUT 到达统计。用来区分两种"卡"：
/// - 最大到达间隔出现几百毫秒 → 主机这一段**没送数据**（驱动/引擎缓冲被抽干）；
/// - 到达间隔正常但**全零字节接近 100%** → 主机送的是**数字静音**（audiodg/播放器侧欠载）。
#[derive(Default)]
struct OutStats {
    inner: Mutex<OutStatsInner>,
}

#[derive(Default)]
struct OutStatsInner {
    urbs: u64,
    bytes: u64,
    zero_bytes: u64,
    max_gap_ms: u128,
    /// 与上一批**完全相同**的批次数（内容重复 = 听感上的"卡"，电平/速率都看不出来）
    repeat_urbs: u64,
    /// 包描述符 offset 不连续的批次数。
    /// 我们一直把整块 payload 当连续 PCM 读写；若主机给的 offset 有间隙/错位，
    /// 送进去（和读回来）的音频就会被打碎 —— 实测正是麦克风端丢/重样百万级。
    noncontig_urbs: u64,
    /// 序列自检（每帧 +1 LSB 的测试信号）：相邻样本差值异常的次数
    seq_bad: u64,
    seq_total: u64,
    seq_prev: [i16; 2],
    seq_has_prev: bool,
    /// 首个包的 offset（不为 0 说明 payload 开头有偏移，我们一直读错了位置）
    first_offset: u32,
    /// 样本级"保持"（同一通道连续 ≥4 个完全相同的样本）。
    /// 音乐里这种情况几乎不可能自然出现，出现就是驱动/引擎在欠载时**保持上一帧**
    /// ——听感是持续发毛发涩（"一直不流畅"），而整批比对、电平、速率全都看不出来。
    hold_samples: u64,
    total_samples: u64,
    hold_last: [i16; 2],
    hold_run: [u32; 2],
    last_hash: u64,
    has_last: bool,
    last: Option<Instant>,
    since: Option<Instant>,
}

/// 麦克风方向（ISO IN）的到达统计：主机到底读了多少、读得稳不稳。
/// 与 OUT 侧对比就能看出"录音端被持续丢样"的速率失配。
#[derive(Default)]
struct InStats {
    inner: Mutex<InStatsInner>,
}

#[derive(Default)]
struct InStatsInner {
    urbs: u64,
    bytes: u64,
    max_gap_ms: u128,
    /// 本区间内我们交给主机的 PCM 峰值（0 = 我们发出去的就是静音）
    peak: f32,
    /// 序列自检：相邻样本差值不等于期望步长的次数
    seq_bad: u64,
    seq_total: u64,
    /// 零样本数（占比高说明我们发出的就是"很多静音"）
    zero_samples: u64,
    samples_total: u64,
    prev: [i16; 2],
    has_prev: bool,
    last: Option<Instant>,
    since: Option<Instant>,
}

impl InStats {
    fn note(&self, bus: &str, payload: &[u8], now: Instant) {
        let mut s = self.inner.lock();
        let since = *s.since.get_or_insert(now);
        if let Some(last) = s.last {
            let gap = now.saturating_duration_since(last).as_millis();
            if gap > s.max_gap_ms {
                s.max_gap_ms = gap;
            }
        }
        s.last = Some(now);
        s.urbs += 1;
        s.bytes += payload.len() as u64;
        // 序列与峰值自检（按通道分别看）
        for (i, pair) in payload.chunks_exact(2).enumerate() {
            let v = i16::from_le_bytes([pair[0], pair[1]]);
            let c = i & 1;
            let f = v as f32 / 32768.0;
            if f.abs() > s.peak {
                s.peak = f.abs();
            }
            s.samples_total += 1;
            if v == 0 {
                s.zero_samples += 1;
            }
            if s.has_prev {
                let d = v as i32 - s.prev[c] as i32;
                // 测试信号每帧 +1（-1 是回绕）；只统计"明显异常"的
                if d != 1 && d != -6553 && d != -6554 && v != 0 && s.prev[c] != 0 {
                    s.seq_bad += 1;
                }
                s.seq_total += 1;
            }
            s.prev[c] = v;
            s.has_prev = true;
        }
        let elapsed = now.saturating_duration_since(since);
        if elapsed >= Duration::from_secs(2) {
            let secs = elapsed.as_secs_f64();
            tracing::debug!(
                "{bus} ISO IN（麦克风）：{:.0} URB/s、{:.1} KB/s、峰值 {:.4}、零样本 {:.1}%、最大到达间隔 {}ms",
                s.urbs as f64 / secs,
                s.bytes as f64 / 1024.0 / secs,
                s.peak,
                100.0 * s.zero_samples as f64 / (s.samples_total.max(1)) as f64,
                s.max_gap_ms
            );
            let prev = s.prev;
            let has_prev = s.has_prev;
            *s = InStatsInner {
                since: Some(now),
                prev,
                has_prev,
                ..Default::default()
            };
        }
    }
}

/// FNV-1a：只用来判断两批数据是否逐字节相同
fn payload_hash(payload: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in payload {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl OutStats {
    fn note(&self, bus: &str, payload: &[u8], packets: &[IsoPacket], now: Instant) {
        let mut s = self.inner.lock();
        let since = *s.since.get_or_insert(now);
        // 包 offset 是否连续（不连续 ⇒ 不能把整块 payload 当连续 PCM）
        if !packets.is_empty() {
            let mut expect = packets[0].offset;
            let mut contig = true;
            for p in packets {
                if p.offset != expect {
                    contig = false;
                    break;
                }
                expect = p.offset + p.length;
            }
            if !contig {
                s.noncontig_urbs += 1;
            }
            if s.urbs == 0 {
                s.first_offset = packets[0].offset;
            }
        }
        // 序列自检：测试信号每帧 +1 LSB，相邻样本差值异常即说明数据被打碎
        for (i, pair) in payload.chunks_exact(2).enumerate() {
            let v = i16::from_le_bytes([pair[0], pair[1]]);
            let c = i & 1;
            if s.seq_has_prev {
                let d = v as i32 - s.seq_prev[c] as i32;
                if d != 1 && d != -6553 && d != -6554 && v != 0 && s.seq_prev[c] != 0 {
                    s.seq_bad += 1;
                }
                s.seq_total += 1;
            }
            s.seq_prev[c] = v;
            s.seq_has_prev = true;
        }
        if let Some(last) = s.last {
            let gap = now.saturating_duration_since(last).as_millis();
            if gap > s.max_gap_ms {
                s.max_gap_ms = gap;
            }
        }
        s.last = Some(now);
        s.urbs += 1;
        s.bytes += payload.len() as u64;
        s.zero_bytes += payload.iter().filter(|b| **b == 0).count() as u64;
        // 样本级保持检测：16bit 小端 PCM，按**通道**分别看连续相同样本
        // （不能混着看：单声道素材左右相同，会全部误判成保持）
        let mut prev = [0i16; 2];
        let mut run = [0u32; 2];
        let mut first = true;
        for (i, pair) in payload.chunks_exact(2).enumerate() {
            let v = i16::from_le_bytes([pair[0], pair[1]]);
            let c = i & 1;
            // 0 不算：数字静音里"连续相同"是正常的，不能当成保持
            if v == 0 {
                if run[c] >= 3 {
                    s.hold_samples += (run[c] + 1) as u64;
                }
                run[c] = 0;
                prev[c] = v;
                first = false;
                s.total_samples += 1;
                continue;
            }
            if !first && v != 0 {
                if v == prev[c] {
                    run[c] += 1;
                } else {
                    if run[c] >= 3 {
                        // run 是"额外重复"的个数，≥3 表示连续 4 个相同
                        s.hold_samples += (run[c] + 1) as u64;
                    }
                    run[c] = 0;
                }
            }
            prev[c] = v;
            first = false;
            s.total_samples += 1;
        }
        // 把跨批的连续状态接上（payload 内的 run 已统计，这里只保留收尾状态）
        for c in 0..2 {
            if run[c] >= 3 {
                s.hold_samples += (run[c] + 1) as u64;
            }
            s.hold_last[c] = prev[c];
            s.hold_run[c] = run[c];
        }
        let h = payload_hash(payload);
        if s.has_last && h == s.last_hash {
            s.repeat_urbs += 1;
        }
        s.last_hash = h;
        s.has_last = true;
        let elapsed = now.saturating_duration_since(since);
        if elapsed >= Duration::from_secs(2) {
            let secs = elapsed.as_secs_f64();
            let hold_ms = s.hold_samples as f64 / 48.0; // 48 样本/ms（48k 立体声）
            tracing::debug!(
                "{bus} ISO OUT：{:.0} URB/s、{:.1} KB/s、全零 {:.1}%、重复批 {:.1}%、保持 {:.1}ms/s（{:.2}%）、包 offset 不连续 {:.1}%（首包 offset={}）、**序列异常 {:.1}%**（{}/{}）、最大到达间隔 {}ms",
                s.urbs as f64 / secs,
                s.bytes as f64 / 1024.0 / secs,
                100.0 * s.zero_bytes as f64 / (s.bytes.max(1)) as f64,
                100.0 * s.repeat_urbs as f64 / s.urbs.max(1) as f64,
                hold_ms / secs,
                100.0 * s.hold_samples as f64 / (s.total_samples.max(1)) as f64,
                100.0 * s.noncontig_urbs as f64 / s.urbs.max(1) as f64,
                s.first_offset,
                100.0 * s.seq_bad as f64 / (s.seq_total.max(1)) as f64,
                s.seq_bad,
                s.seq_total,
                s.max_gap_ms
            );
            let last_hash = s.last_hash;
            let has_last = s.has_last;
            let hold_last = s.hold_last;
            let hold_run = s.hold_run;
            *s = OutStatsInner {
                since: Some(now),
                last_hash,
                has_last,
                hold_last,
                hold_run,
                ..Default::default()
            };
        }
    }
}

async fn handle_urbs(
    mut reader: tokio::net::tcp::OwnedReadHalf,
    writer: tokio::net::tcp::OwnedWriteHalf,
    cable: Arc<Cable>,
) {
    // 服务间隔由描述符的 bInterval 决定（高码率会自动缩短），节拍必须跟着它走，
    // 否则采集端会按错误的速率出数据（bInterval=3 时快一倍）。
    let packet_micros = match cable.descriptors.protocol() {
        // UAC1 全速：bInterval=1 → 1 帧 = 1ms
        crate::usbip::descriptors::CableProtocol::Uac1 => 1000u64,
        crate::usbip::descriptors::CableProtocol::Uac2 => {
            super::descriptors::CableFormat::service_interval_micros(
                cable.format().iso_b_interval(),
            ) as u64
        }
    };
    let state = Arc::new(ConnState {
        write: tokio::sync::Mutex::new(writer),
        timeline: IsoTimeline::new(packet_micros),
        pending: PendingMap::default(),
        out_stats: OutStats::default(),
        in_stats: InStats::default(),
    });

    loop {
        let basic = match protocol::read_basic_header(&mut reader).await {
            Ok(h) => h,
            Err(_) => break,
        };
        match basic.command {
            protocol::CMD_SUBMIT => {
                let submit = match read_submit(&mut reader, basic).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("{} SUBMIT 解析失败: {e}", cable.bus_id);
                        break;
                    }
                };
                // EP0 与非 iso 请求串行处理（保证枚举顺序）
                if basic.endpoint == 0 || !submit.req.is_isochronous() {
                    if process_submit(&state, &cable, submit, None).await.is_err() {
                        tracing::warn!("{} SUBMIT 处理失败，断开", cable.bus_id);
                        break;
                    }
                    continue;
                }
                // iso OUT：**立刻**把数据写进环形缓冲，只把"回复"推迟到节拍点。
                //
                // 这是参考实现（Virtual-Cables）的关键顺序：先 WritePlayback，再 waitIsoCompletion，
                // 最后才 writeSubmit。我们原来是"等节拍到点后才写数据"，于是 ring 里的音频
                // 永远比主机晚一个节拍；而录音方向是"节拍到点才读 ring"，结果读麦克风时
                // ring 经常是空的/过期的 —— 表现为虚拟麦克风几乎是静音。
                let prewritten = if basic.endpoint == 1 && basic.direction == DIRECTION_OUT {
                    Some(if cable.playback_active() {
                        cable.write_playback(&submit.out)
                    } else {
                        0
                    })
                } else {
                    None
                };
                // iso：预留完成时刻后派发独立任务（节拍到点才回复）
                let complete_at =
                    state.timeline.reserve(basic.endpoint, submit.req.number_of_packets, Instant::now());
                let task_state = state.clone();
                let task_cable = cable.clone();
                let seq = basic.sequence;
                let handle = tokio::spawn(async move {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(complete_at)).await;
                    let _ = process_submit(&task_state, &task_cable, submit, prewritten).await;
                    task_state.pending.inner.lock().remove(&seq);
                });
                state.pending.insert(seq, handle.abort_handle());
            }
            protocol::CMD_UNLINK => {
                let mut buf = [0u8; 28];
                if reader.read_exact(&mut buf).await.is_err() {
                    break;
                }
                let target = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
                // 协议规定：成功 unlink 回 -ECONNRESET
                let status = if state.pending.cancel(target) { STATUS_CONN_RESET } else { STATUS_OK };
                let mut w = state.write.lock().await;
                if protocol::write_ret_unlink(&mut *w, &basic, status).await.is_err() {
                    break;
                }
            }
            _ => {
                tracing::warn!("{} 不支持的 URB 命令 0x{:08x}", cable.bus_id, basic.command);
                break;
            }
        }
    }
    state.pending.cancel_all();
    tracing::info!("USB/IP 会话关闭: {}", cable.bus_id);
}

struct ParsedSubmit {
    req: SubmitRequest,
    out: Vec<u8>,
    packets: Vec<IsoPacket>,
}

async fn read_submit(
    r: &mut tokio::net::tcp::OwnedReadHalf,
    basic: BasicHeader,
) -> std::io::Result<ParsedSubmit> {
    let req = protocol::read_submit_body(r, basic).await?;
    if req.transfer_buffer_length > protocol::MAX_TRANSFER_LENGTH {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("传输长度 {} 超限", req.transfer_buffer_length),
        ));
    }
    let mut out = Vec::new();
    if basic.direction == DIRECTION_OUT && req.transfer_buffer_length > 0 {
        out.resize(req.transfer_buffer_length as usize, 0);
        r.read_exact(&mut out).await?;
    }
    let packets = protocol::read_iso_packets(r, req.number_of_packets).await?;
    Ok(ParsedSubmit { req, out, packets })
}

/// 处理一个 SUBMIT 并写应答
async fn process_submit(
    state: &ConnState,
    cable: &Arc<Cable>,
    submit: ParsedSubmit,
    prewritten: Option<usize>,
) -> std::io::Result<()> {
    let req = &submit.req;
    let basic = &req.basic;

    // EP0 控制传输
    if basic.endpoint == 0 {
        let (data, status) = match SetupPacket::parse(&req.setup) {
            Some(setup) => cable.handle_control(setup, &submit.out),
            None => (Vec::new(), STATUS_INVALID),
        };
        let data = if data.len() > req.transfer_buffer_length as usize {
            data[..req.transfer_buffer_length as usize].to_vec()
        } else {
            data
        };
        let mut w = state.write.lock().await;
        return protocol::write_ret_submit(&mut *w, req, status, data.len() as u32, &data, &[], 0).await;
    }

    let mut w = state.write.lock().await;
    tracing::trace!(
        "URB ep={} dir={} iso={} len={} packets={}",
        basic.endpoint,
        basic.direction,
        req.is_isochronous(),
        req.transfer_buffer_length,
        req.number_of_packets
    );
    match (basic.endpoint, basic.direction) {
        // 播放：EP1 OUT
        (1, DIRECTION_OUT) => {
            if req.is_isochronous() {
                state
                    .out_stats
                    .note(&cable.bus_id, &submit.out, &submit.packets, Instant::now());
            }
            // 数据在派发前就已经写进 ring（只把回复推迟到节拍点，见 handle_urbs）
            let actual = prewritten.unwrap_or_else(|| {
                if cable.playback_active() {
                    cable.write_playback(&submit.out)
                } else {
                    0 // SET_INTERFACE 前后的无害竞态：接受并丢弃
                }
            });
            let (packets, error_count) = if req.is_isochronous() {
                (mark_iso_packets(&submit.packets, actual), 0)
            } else {
                (submit.packets.clone(), 0)
            };
            protocol::write_ret_submit(&mut *w, req, STATUS_OK, actual as u32, &[], &packets, error_count)
                .await
        }
        // 录音：EP2 IN
        (2, DIRECTION_IN) => {
            let payload = response_payload_length(req, &submit.packets);
            let mut data = vec![0u8; payload];
            let got = cable.read_capture(&mut data);
            if req.is_isochronous() {
                state.in_stats.note(&cable.bus_id, &data[..got.min(data.len())], Instant::now());
            }
            let (packets, error_count) = if req.is_isochronous() {
                (mark_iso_packets(&submit.packets, payload), 0)
            } else {
                (submit.packets.clone(), 0)
            };
            protocol::write_ret_submit(&mut *w, req, STATUS_OK, payload as u32, &data, &packets, error_count)
                .await
        }
        // 控制变化通知：EP3 IN（AC 接口的中断端点）
        //
        // 没有待上报事件时**必须不回复**（USB 语义 = NAK，主机的传输保持 pending）。
        // 回一个 0/2 字节的包会被当成「发生了一次控制变化」的事件，而且主机收到短包
        // 会立刻重发 —— 实测会变成每秒上千次的中断轮询，把音频栈搅乱。
        (3, DIRECTION_IN) => {
            tracing::trace!("EP3 IN 无中断事件 → 不回复（NAK）");
            Ok(())
        }
        _ => {
            let packets = mark_iso_error(&submit.packets, STATUS_PIPE);
            protocol::write_ret_submit(&mut *w, req, STATUS_PIPE, 0, &[], &packets, 0).await
        }
    }
}

/// 应答负载长度：iso = 各包长度之和（不超过传输缓冲上限）
fn response_payload_length(req: &SubmitRequest, packets: &[IsoPacket]) -> usize {
    if packets.is_empty() {
        return req.transfer_buffer_length as usize;
    }
    let mut total: u64 = packets.iter().map(|p| p.length as u64).sum();
    if req.transfer_buffer_length > 0 && total > req.transfer_buffer_length as u64 {
        total = req.transfer_buffer_length as u64;
    }
    total.min(protocol::MAX_TRANSFER_LENGTH as u64) as usize
}

/// 把实际字节数按包顺序分配到 actual_length
fn mark_iso_packets(packets: &[IsoPacket], actual_total: usize) -> Vec<IsoPacket> {
    let mut remaining = actual_total;
    packets
        .iter()
        .map(|p| {
            let want = (p.length as usize).min(remaining);
            remaining -= want;
            IsoPacket { status: STATUS_OK, actual_length: want as u32, ..*p }
        })
        .collect()
}

fn mark_iso_error(packets: &[IsoPacket], status: i32) -> Vec<IsoPacket> {
    packets
        .iter()
        .map(|p| IsoPacket { status, actual_length: 0, ..*p })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::protocol::NO_ISO_PACKETS;

    /// 主机队列深度 1（拿到完成才提交下一条）且我们的完成固定迟到 1ms 时，
    /// 节拍**不能**变慢：100 条 10ms 的 URB 仍应覆盖约 1 秒音频。
    /// 旧写法（base = max(上次, now)）会变成 11ms/条 → 每秒少 10% 音频。
    #[test]
    fn slow_completions_do_not_lose_iso_rate() {
        let timeline = IsoTimeline::new(1000); // 1ms/包
        let t0 = Instant::now();
        let mut now = t0;
        let mut last = t0;
        for _ in 0..100 {
            let deadline = timeline.reserve(1, 10, now); // 10 包 = 10ms 音频
            last = deadline;
            now = deadline + Duration::from_millis(1); // 完成迟到 1ms，主机随即再提交
        }
        let span = (last - t0).as_millis();
        assert!(
            (950..=1060).contains(&span),
            "100 条 10ms URB 的排程跨度应≈1s（不能因迟到而变成 1.1s），实际 {span}ms"
        );
    }

    /// 线程被长时间抢占后**不许**让主机一次性追平（实测会一次丢掉 5.9 秒音频、
    /// 并且播放速度变快）：应当丢弃欠账、以当前时刻重新起拍，之后恢复正常节拍。
    #[test]
    fn long_stall_resyncs_instead_of_flooding() {
        let timeline = IsoTimeline::new(1000);
        let now = Instant::now();
        timeline.reserve(1, 10, now);
        let later = now + Duration::from_secs(5);
        let d = timeline.reserve(1, 10, later);
        // 重新起拍：完成时刻落在「现在」之后的一个服务间隔内，而不是过去
        assert!(d >= later, "不应把完成时刻排在过去（会造成一次性追平）");
        assert!(d <= later + Duration::from_millis(10), "应在重新起拍后的一个服务间隔内完成");
        // 之后每批仍严格相隔 10ms（速率精确）
        let d2 = timeline.reserve(1, 10, later + Duration::from_millis(10));
        assert_eq!((d2 - d).as_millis(), 10);
    }

    /// OUT 与 IN 都必须按节拍完成（主机收到完成才提交下一批，节拍 = 流速率）；
    /// 每批的完成时刻 = 上一批完成时刻 + 本批时长 − 5ms 提前量。
    #[test]
    fn iso_completions_are_paced_with_a_lead() {
        let timeline = IsoTimeline::new(1000);
        let now = Instant::now();
        let d1 = timeline.reserve(1, 10, now);
        let d2 = timeline.reserve(1, 10, now + Duration::from_millis(10));
        let d3 = timeline.reserve(1, 10, now + Duration::from_millis(20));
        // 提前量：第一批在 10ms 时长 − 提前量（上限为半个服务间隔）之后完成
        let lead = LEAD_MS.min(10 / 2 + 1);
        assert_eq!((d1 - now).as_millis(), (10 - lead) as u128);
        // 相邻两批仍严格相隔一个服务间隔（10ms）→ 长期速率精确等于实时
        assert_eq!((d2 - d1).as_millis(), 10);
        assert_eq!((d3 - d2).as_millis(), 10);
    }

    #[test]
    fn iso_budget_reservation_is_sequential() {
        let timeline = IsoTimeline::new(1000); // 1ms 服务间隔（bInterval=4）
        let now = Instant::now();
        // Windows 深队列：一次排 4 个 10ms URB → 完成时刻依次错开
        let d1 = timeline.reserve(1, 10, now);
        let d2 = timeline.reserve(1, 10, now);
        let d3 = timeline.reserve(1, 10, now);
        assert!(d2 > d1 && d3 > d2, "批次完成时刻必须顺序错开");
        assert_eq!((d3 - d1).as_millis(), 20);
        // 不同端点互不影响
        let e1 = timeline.reserve(2, 10, now);
        assert!(e1 <= d1 + Duration::from_millis(10));
        // 0 包 → 立即
        assert_eq!(timeline.reserve(3, 0, now), now);
    }

    #[test]
    fn half_millisecond_service_interval_paces_correctly() {
        // 192k/32bit/2ch 会退到 bInterval=3（0.5ms），节拍必须按 0.5ms/包
        let timeline = IsoTimeline::new(500);
        let now = Instant::now();
        let d1 = timeline.reserve(1, 10, now);
        let d2 = timeline.reserve(1, 10, now);
        assert_eq!((d2 - d1).as_millis(), 5, "10 包 × 0.5ms = 5ms");
    }

    #[test]
    fn timeline_recovers_after_gap() {
        let timeline = IsoTimeline::new(1000);
        let now = Instant::now();
        let d1 = timeline.reserve(1, 10, now);
        // 长时间空闲后重新起拍：完成时刻不能排在过去（否则主机会一次性灌爆缓冲）
        let later = now + Duration::from_secs(5);
        let d2 = timeline.reserve(1, 10, later);
        assert!(
            d2 >= later && d2 <= later + Duration::from_millis(10),
            "空闲后应重新起拍（完成时刻在现在之后一个服务间隔内），实际 {:?}",
            d2.saturating_duration_since(later)
        );
        assert!(d1 < d2);
        // 之后保持正常节拍
        let mut d = d2;
        for _ in 0..5 {
            let t = later + Duration::from_millis(10);
            d = timeline.reserve(1, 10, t);
        }
        assert!(d >= later, "应回到实时节拍，实际落后 {:?}", later - d);
    }

    #[test]
    fn mark_packets_distributes_actual() {
        let packets = vec![
            IsoPacket { offset: 0, length: 192, actual_length: 0, status: 1 },
            IsoPacket { offset: 192, length: 192, actual_length: 0, status: 1 },
        ];
        let marked = mark_iso_packets(&packets, 300);
        assert_eq!(marked[0].actual_length, 192);
        assert_eq!(marked[1].actual_length, 108);
        assert!(marked.iter().all(|p| p.status == STATUS_OK));
        let marked = mark_iso_packets(&packets, 0);
        assert!(marked.iter().all(|p| p.actual_length == 0 && p.status == STATUS_OK));
    }

    #[test]
    fn payload_length_sums_packets() {
        let req = SubmitRequest {
            basic: BasicHeader { command: 1, sequence: 1, device_id: 0, direction: DIRECTION_IN, endpoint: 2 },
            transfer_flags: 0,
            transfer_buffer_length: 5000,
            start_frame: 0,
            number_of_packets: NO_ISO_PACKETS,
            interval: 1,
            setup: [0; 8],
        };
        let packets = vec![
            IsoPacket { offset: 0, length: 192, actual_length: 0, status: 0 },
            IsoPacket { offset: 192, length: 192, actual_length: 0, status: 0 },
        ];
        assert_eq!(response_payload_length(&req, &packets), 384);
        assert_eq!(response_payload_length(&req, &[]), 5000);
    }
}
