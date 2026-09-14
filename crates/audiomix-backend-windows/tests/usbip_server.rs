//! USB/IP 服务器端到端集成测试。
//!
//! 用真实 TCP 连接模拟 usbip-win2 客户端（`usbip port` / vhci 的行为）：
//! `OP_REQ_DEVLIST` 握手 → `OP_REQ_IMPORT` 取出设备描述符 → EP0 控制传输完成
//! USB 枚举（SET_CONFIGURATION + SET_INTERFACE + UAC1 类请求）→ ISO OUT 写入
//! PCM → ISO IN 读回（Loopback 线缆应原样回环）。
//!
//! 客户端的编解码刻意**不复用** server 侧写函数，独立按 USB/IP v1.1.1
//! 线格式逐字节实现，避免"两边同时写错"的自洽假阳性。

use std::net::SocketAddr;
use std::time::Duration;

use audiomix_backend_windows::usbip::device::{CableConfig, CableMode};
use audiomix_backend_windows::usbip::UsbIpManager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// —— 线格式常量 ——
const VERSION: u16 = 0x0111;
const OP_REQ_DEVLIST: u16 = 0x8005;
const OP_REP_DEVLIST: u16 = 0x0005;
const OP_REQ_IMPORT: u16 = 0x8003;
const OP_REP_IMPORT: u16 = 0x0003;
const CMD_SUBMIT: u32 = 0x0000_0001;
const CMD_UNLINK: u32 = 0x0000_0002;
const RET_SUBMIT: u32 = 0x0000_0003;
const RET_UNLINK: u32 = 0x0000_0004;
const NO_ISO: u32 = 0xffff_ffff;
const DIR_OUT: u32 = 0;
const DIR_IN: u32 = 1;
/// USB/IP 头总长：basic(20) + 命令体(28)
const HEADER_LEN: usize = 48;
/// usbip_usb_device 线格式长度
const DEVICE_LEN: usize = 312;
const SPEED_FULL: u32 = 2;
const STATUS_OK: i32 = 0;
const STATUS_PIPE: i32 = -32;
const STATUS_CONN_RESET: i32 = -104;

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

#[derive(Debug)]
struct UsbDevice {
    path: String,
    busid: String,
    busnum: u32,
    devnum: u32,
    speed: u32,
    id_vendor: u16,
    id_product: u16,
    bcd_device: u16,
    class: u8,
    subclass: u8,
    protocol: u8,
    config_value: u8,
    num_configs: u8,
    num_interfaces: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IsoDesc {
    offset: u32,
    length: u32,
    actual_length: u32,
    status: i32,
}

#[derive(Debug)]
struct Reply {
    sequence: u32,
    status: i32,
    actual: u32,
    packets: Vec<IsoDesc>,
    data: Vec<u8>,
}

/// 一次 SUBMIT 请求
struct SubmitSpec {
    seq: u32,
    ep: u32,
    dir: u32,
    /// transfer_buffer_length：OUT 为数据长度，IN 为期望返回上限
    transfer_len: u32,
    /// 非 iso 用 NO_ISO
    packets: u32,
    setup: [u8; 8],
    payload: Vec<u8>,
    /// iso 包描述符 (offset, length)
    descs: Vec<(u32, u32)>,
}

struct Client {
    s: TcpStream,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        let s = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
            .await
            .expect("连接超时")
            .expect("连接服务器失败");
        s.set_nodelay(true).ok();
        Self { s }
    }

    async fn write(&mut self, b: &[u8]) {
        self.s.write_all(b).await.expect("写失败");
    }

    async fn read_n(&mut self, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        tokio::time::timeout(Duration::from_secs(5), self.s.read_exact(&mut buf))
            .await
            .expect("读超时")
            .expect("读失败");
        buf
    }

    async fn read_u32(&mut self) -> u32 {
        let b = self.read_n(4).await;
        be32(&b, 0)
    }

    /// 发管理操作请求头（version + code + status=0）
    async fn op_request(&mut self, code: u16) {
        let mut b = Vec::with_capacity(8);
        b.extend_from_slice(&VERSION.to_be_bytes());
        b.extend_from_slice(&code.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        self.write(&b).await;
    }

    async fn op_reply(&mut self) -> (u16, u32) {
        let b = self.read_n(8).await;
        (
            u16::from_be_bytes([b[2], b[3]]),
            be32(&b, 4),
        )
    }

    async fn read_device(&mut self) -> UsbDevice {
        let b = self.read_n(DEVICE_LEN).await;
        let cstr = |r: std::ops::Range<usize>| {
            String::from_utf8_lossy(&b[r]).trim_end_matches('\0').to_string()
        };
        UsbDevice {
            path: cstr(0..256),
            busid: cstr(256..288),
            busnum: be32(&b, 288),
            devnum: be32(&b, 292),
            speed: be32(&b, 296),
            id_vendor: u16::from_be_bytes([b[300], b[301]]),
            id_product: u16::from_be_bytes([b[302], b[303]]),
            bcd_device: u16::from_be_bytes([b[304], b[305]]),
            class: b[306],
            subclass: b[307],
            protocol: b[308],
            config_value: b[309],
            num_configs: b[310],
            num_interfaces: b[311],
        }
    }

    async fn submit(&mut self, spec: &SubmitSpec) {
        let mut h = Vec::with_capacity(HEADER_LEN);
        h.extend_from_slice(&CMD_SUBMIT.to_be_bytes());
        h.extend_from_slice(&spec.seq.to_be_bytes());
        h.extend_from_slice(&0u32.to_be_bytes()); // devid（客户端填 0）
        h.extend_from_slice(&spec.dir.to_be_bytes());
        h.extend_from_slice(&spec.ep.to_be_bytes());
        h.extend_from_slice(&0u32.to_be_bytes()); // transfer_flags
        h.extend_from_slice(&spec.transfer_len.to_be_bytes());
        h.extend_from_slice(&0u32.to_be_bytes()); // start_frame
        h.extend_from_slice(&spec.packets.to_be_bytes());
        h.extend_from_slice(&0u32.to_be_bytes()); // interval
        h.extend_from_slice(&spec.setup);
        assert_eq!(h.len(), HEADER_LEN);
        self.write(&h).await;
        // iso OUT：数据区在包描述符之前
        if spec.dir == DIR_OUT && !spec.payload.is_empty() {
            let p = spec.payload.clone();
            self.write(&p).await;
        }
        for (offset, length) in &spec.descs {
            let mut d = Vec::with_capacity(16);
            d.extend_from_slice(&offset.to_be_bytes());
            d.extend_from_slice(&length.to_be_bytes());
            d.extend_from_slice(&0u32.to_be_bytes()); // actual_length
            d.extend_from_slice(&0u32.to_be_bytes()); // status
            self.write(&d).await;
        }
    }

    /// 读 RET_SUBMIT；`expect_data` = 主机是否期待数据区（IN 传输）
    async fn read_reply(&mut self, expect_data: bool) -> Reply {
        let h = self.read_n(HEADER_LEN).await;
        assert_eq!(be32(&h, 0), RET_SUBMIT, "应答命令必须是 RET_SUBMIT");
        let sequence = be32(&h, 4);
        assert_eq!(be32(&h, 8), 0, "应答 devid 必须清零");
        assert_eq!(be32(&h, 12), 0, "应答 direction 必须清零");
        assert_eq!(be32(&h, 16), 0, "应答 endpoint 必须清零");
        let status = be32(&h, 20) as i32;
        let actual = be32(&h, 24);
        let n = be32(&h, 32);
        let data = if expect_data && status == STATUS_OK && actual > 0 {
            self.read_n(actual as usize).await
        } else {
            Vec::new()
        };
        let count = if n == NO_ISO { 0 } else { n };
        let mut packets = Vec::with_capacity(count as usize);
        if count > 0 {
            let raw = self.read_n(count as usize * 16).await;
            for chunk in raw.chunks_exact(16) {
                packets.push(IsoDesc {
                    offset: be32(chunk, 0),
                    length: be32(chunk, 4),
                    actual_length: be32(chunk, 8),
                    status: be32(chunk, 12) as i32,
                });
            }
        }
        Reply { sequence, status, actual, packets, data }
    }

    /// EP0 控制传输
    async fn control(&mut self, seq: u32, setup: [u8; 8], out: &[u8], in_len: u32) -> Reply {
        let dir = if setup[0] & 0x80 != 0 { DIR_IN } else { DIR_OUT };
        let transfer_len = if dir == DIR_OUT { out.len() as u32 } else { in_len };
        self.submit(&SubmitSpec {
            seq,
            ep: 0,
            dir,
            transfer_len,
            packets: NO_ISO,
            setup,
            payload: out.to_vec(),
            descs: Vec::new(),
        })
        .await;
        self.read_reply(dir == DIR_IN).await
    }

    /// CMD_UNLINK：28 字节体，前 4 字节 = 目标 seq
    async fn unlink(&mut self, seq: u32, target: u32) {
        let mut b = Vec::with_capacity(HEADER_LEN);
        b.extend_from_slice(&CMD_UNLINK.to_be_bytes());
        b.extend_from_slice(&seq.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&target.to_be_bytes());
        b.extend_from_slice(&[0u8; 24]);
        assert_eq!(b.len(), HEADER_LEN);
        self.write(&b).await;
    }

    async fn read_unlink_reply(&mut self) -> (u32, i32) {
        let h = self.read_n(HEADER_LEN).await;
        assert_eq!(be32(&h, 0), RET_UNLINK);
        (be32(&h, 4), be32(&h, 20) as i32)
    }
}

fn cable_config(number: u8, rate: u32, bits: u16) -> CableConfig {
    CableConfig { number, name: String::new(), sample_rate: rate, bits, mode: CableMode::Loopback, buffer_ms: 250 }
}

/// 探一个当前空闲的端口（绑定 :0 拿到端口号后立刻释放）
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("应能探测空闲端口");
    l.local_addr().expect("应能取到端口").port()
}

/// 模拟 usbip-win2 的 attach：import 一条线缆，并把这条长连接保持住
async fn import_cable(c: &mut Client, busid: &str) -> UsbDevice {
    c.op_request(OP_REQ_IMPORT).await;
    let mut b = [0u8; 32];
    b[..busid.len()].copy_from_slice(busid.as_bytes());
    c.write(&b).await;
    let (code, status) = c.op_reply().await;
    assert_eq!((code, status), (OP_REP_IMPORT, 0), "import {busid} 应成功");
    c.read_device().await
}

/// 启动服务器并返回管理器 + 实际监听地址
fn start_server(cfgs: Vec<CableConfig>) -> (UsbIpManager, SocketAddr) {
    let manager = UsbIpManager::new("127.0.0.1:0");
    manager
        .start(&tokio::runtime::Handle::current(), cfgs)
        .expect("服务器应能启动");
    let addr = manager.local_addr().expect("应能查到监听地址");
    (manager, addr)
}

/// 48k/16bit 立体声的 1ms 负载
const BYTES_PER_MS: usize = 192;

fn pcm_ramp(bytes: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(bytes);
    for i in 0..bytes / 2 {
        let s = ((i as i32 * 37) % 20000 - 10000) as i16;
        v.extend_from_slice(&s.to_be_bytes());
    }
    v
}

#[tokio::test]
async fn devlist_reports_full_speed_uac1_device() {
    let (_m, addr) = start_server(vec![cable_config(1, 48_000, 16), cable_config(2, 192_000, 16)]);

    let mut c = Client::connect(addr).await;
    c.op_request(OP_REQ_DEVLIST).await;
    let (code, status) = c.op_reply().await;
    assert_eq!((code, status), (OP_REP_DEVLIST, 0));
    assert_eq!(c.read_u32().await, 2, "两条线缆");

    let first = c.read_device().await;
    assert_eq!(first.busid, "1-1");
    assert_eq!(first.busnum, 1);
    assert_eq!(first.devnum, 1);
    assert_eq!(first.speed, SPEED_FULL, "内置线路是 UAC1 全速设备");
    assert_eq!(first.id_vendor, 0xFFFF);
    assert_eq!(first.id_product, 0xCA01, "PID = 0xCA00 + 线缆号");
    assert_eq!(first.bcd_device, 0x0100);
    assert_eq!(first.class, 0x00, "UAC1 设备级类字段全 0");
    assert_eq!(first.subclass, 0x00);
    assert_eq!(first.protocol, 0x00);
    assert_eq!(first.config_value, 1);
    assert_eq!(first.num_configs, 1);
    assert_eq!(first.num_interfaces, 3, "AC + 播放 AS + 录音 AS");
    assert!(first.path.contains("1-1"), "path 应包含 busid：{}", first.path);
    // 接口记录：AC + 2×AS（UAC1：class=AUDIO、subclass=AC/AS、proto=0）
    let expected = [[0x01u8, 0x01, 0x00], [0x01, 0x02, 0x00], [0x01, 0x02, 0x00]];
    for exp in expected.iter().take(first.num_interfaces as usize) {
        let iface = c.read_n(4).await;
        assert_eq!(&iface[..3], &exp[..], "接口记录 class/subclass/proto：{iface:02x?}");
        assert_eq!(iface[3], 0, "padding 必须为 0");
    }

    let second = c.read_device().await;
    assert_eq!(second.busid, "1-2");
    assert_eq!(second.devnum, 2);
    assert_eq!(second.id_product, 0xCA02);
    for _ in 0..second.num_interfaces {
        c.read_n(4).await;
    }
}

#[tokio::test]
async fn import_unknown_busid_is_rejected() {
    let (_m, addr) = start_server(vec![cable_config(1, 48_000, 16)]);

    let mut c = Client::connect(addr).await;
    c.op_request(OP_REQ_IMPORT).await;
    let mut busid = [0u8; 32];
    busid[..3].copy_from_slice(b"9-9");
    c.write(&busid).await;
    let (code, status) = c.op_reply().await;
    assert_eq!(code, OP_REP_IMPORT);
    assert_ne!(status, 0, "未知 busid 必须返回非 0 状态");
}

#[tokio::test]
async fn full_session_control_then_iso_roundtrip() {
    let (_m, addr) = start_server(vec![cable_config(1, 48_000, 16)]);

    // —— import：拿到设备描述符 ——
    let mut c = Client::connect(addr).await;
    c.op_request(OP_REQ_IMPORT).await;
    let mut busid = [0u8; 32];
    busid[..3].copy_from_slice(b"1-1");
    c.write(&busid).await;
    let (code, status) = c.op_reply().await;
    assert_eq!((code, status), (OP_REP_IMPORT, 0), "import 应成功");
    let dev = c.read_device().await;
    assert_eq!(dev.busid, "1-1");
    assert_eq!(dev.speed, SPEED_FULL);

    // —— EP0 枚举 ——
    let r = c.control(1, [0x00, 0x09, 1, 0, 0, 0, 0, 0], &[], 0).await;
    assert_eq!(r.status, STATUS_OK, "SET_CONFIGURATION(1)");
    for (seq, iface) in [(2u32, 1u8), (3, 2)] {
        let r = c.control(seq, [0x01, 0x0B, 1, 0, iface, 0, 0, 0], &[], 0).await;
        assert_eq!(r.status, STATUS_OK, "SET_INTERFACE({iface}, alt=1)");
    }
    // 设备描述符
    let r = c.control(4, [0x80, 0x06, 0x00, 0x01, 0, 0, 18, 0], &[], 18).await;
    assert_eq!(r.status, STATUS_OK);
    assert_eq!(r.data.len(), 18);
    assert_eq!(r.data[1], 0x01);
    assert_eq!(r.data[8], 0xFF, "idVendor = 0xFFFF");

    // UAC1 端点采样率：GET_CUR（真实 usbaudio.sys 发的 0xA2：class IN + endpoint recipient）
    let r = c.control(5, [0xA2, 0x81, 0x00, 0x01, 0x01, 0x00, 3, 0], &[], 3).await;
    assert_eq!(r.status, STATUS_OK, "GET_CUR(采样率) 不能 STALL");
    assert_eq!(r.data, vec![0x80, 0xBB, 0x00], "48000 = 0x00BB80 小端 3 字节");
    // GET_MIN / GET_MAX 也返回该离散值
    let r = c.control(6, [0xA2, 0x83, 0x00, 0x01, 0x01, 0x00, 3, 0], &[], 3).await;
    assert_eq!(r.status, STATUS_OK, "GET_MAX(采样率) 不能 STALL");
    assert_eq!(r.data, vec![0x80, 0xBB, 0x00]);
    // 不支持的采样率 SET_CUR → STALL（EPIPE）
    let r = c
        .control(7, [0x22, 0x01, 0x00, 0x01, 0x01, 0x00, 3, 0], &44_100u32.to_le_bytes()[0..3], 0)
        .await;
    assert_eq!(r.status, STATUS_PIPE, "未配置的采样率必须 STALL");

    // —— ISO OUT（播放端）——
    let frames = 10u32;
    let pcm = pcm_ramp(frames as usize * BYTES_PER_MS);
    let descs: Vec<(u32, u32)> = (0..frames)
        .map(|i| (i * BYTES_PER_MS as u32, BYTES_PER_MS as u32))
        .collect();
    c.submit(&SubmitSpec {
        seq: 10,
        ep: 1,
        dir: DIR_OUT,
        transfer_len: pcm.len() as u32,
        packets: frames,
        setup: [0; 8],
        payload: pcm.clone(),
        descs: descs.clone(),
    })
    .await;
    let r = c.read_reply(false).await;
    assert_eq!(r.sequence, 10);
    assert_eq!(r.status, STATUS_OK);
    assert_eq!(r.actual, pcm.len() as u32, "OUT 应接受全部字节");
    assert_eq!(r.packets.len(), frames as usize, "iso 应答包数 = 描述符数");
    assert!(
        r.packets.iter().all(|p| p.status == STATUS_OK && p.actual_length == BYTES_PER_MS as u32),
        "每包 actual_length 应等于请求长度：{:?}",
        r.packets
    );

    // —— ISO IN（录音端）：Loopback 线缆应原样回环 ——
    c.submit(&SubmitSpec {
        seq: 11,
        ep: 2,
        dir: DIR_IN,
        transfer_len: pcm.len() as u32,
        packets: frames,
        setup: [0; 8],
        payload: Vec::new(),
        descs: descs.clone(),
    })
    .await;
    let r = c.read_reply(true).await;
    assert_eq!(r.sequence, 11);
    assert_eq!(r.status, STATUS_OK);
    assert_eq!(r.actual, pcm.len() as u32);
    assert_eq!(r.data, pcm, "Loopback 模式：录音端应拿到播放端写入的 PCM");
    assert_eq!(r.packets.len(), frames as usize);

    // —— UNLINK：未知 seq 返回 OK ——
    c.unlink(12, 9999).await;
    let (seq, status) = c.read_unlink_reply().await;
    assert_eq!(seq, 12);
    assert_eq!(status, STATUS_OK);

    // —— UNLINK：取消排队中的 iso，返回 CONN_RESET 且不再有 RET_SUBMIT ——
    c.submit(&SubmitSpec {
        seq: 13,
        ep: 2,
        dir: DIR_IN,
        transfer_len: (200 * BYTES_PER_MS) as u32,
        packets: 200,
        setup: [0; 8],
        payload: Vec::new(),
        descs: (0..200u32).map(|i| (i * BYTES_PER_MS as u32, BYTES_PER_MS as u32)).collect(),
    })
    .await;
    c.unlink(14, 13).await;
    let (seq, status) = c.read_unlink_reply().await;
    assert_eq!(seq, 14);
    assert_eq!(status, STATUS_CONN_RESET, "成功取消排队 iso 必须回 -ECONNRESET");
    // 被取消的请求不应再有应答（其完成时刻在 200ms 之后）
    let mut probe = [0u8; HEADER_LEN];
    let early = tokio::time::timeout(Duration::from_millis(60), c.s.read_exact(&mut probe)).await;
    assert!(early.is_err(), "被 unlink 的 iso 不应再写回应答");
}

#[tokio::test]
async fn iso_out_is_dropped_before_interface_is_activated() {
    let (_m, addr) = start_server(vec![cable_config(1, 48_000, 16)]);

    let mut c = Client::connect(addr).await;
    c.op_request(OP_REQ_IMPORT).await;
    let mut busid = [0u8; 32];
    busid[..3].copy_from_slice(b"1-1");
    c.write(&busid).await;
    let (_, status) = c.op_reply().await;
    assert_eq!(status, 0);
    c.read_device().await;

    // 只设配置，不激活 AS 接口
    let r = c.control(1, [0x00, 0x09, 1, 0, 0, 0, 0, 0], &[], 0).await;
    assert_eq!(r.status, STATUS_OK);

    let pcm = pcm_ramp(BYTES_PER_MS);
    let descs = vec![(0u32, BYTES_PER_MS as u32)];
    c.submit(&SubmitSpec {
        seq: 2,
        ep: 1,
        dir: DIR_OUT,
        transfer_len: pcm.len() as u32,
        packets: 1,
        setup: [0; 8],
        payload: pcm.clone(),
        descs: descs.clone(),
    })
    .await;
    let r = c.read_reply(false).await;
    assert_eq!(r.status, STATUS_OK);
    assert_eq!(r.actual, 0, "接口未激活时应丢弃数据而不是写入环形缓冲");

    // 激活后再写一次：这次应该被接受
    let r = c.control(3, [0x01, 0x0B, 1, 0, 1, 0, 0, 0], &[], 0).await;
    assert_eq!(r.status, STATUS_OK);
    c.submit(&SubmitSpec {
        seq: 4,
        ep: 1,
        dir: DIR_OUT,
        transfer_len: pcm.len() as u32,
        packets: 1,
        setup: [0; 8],
        payload: pcm.clone(),
        descs,
    })
    .await;
    let r = c.read_reply(false).await;
    assert_eq!(r.actual, pcm.len() as u32, "激活后应接受数据");
}

#[tokio::test]
async fn trace_capture_path_feeds_iso_in_after_activation() {
    // Mixer 模式：麦克风端应输出引擎写入的混音结果（这里用后端适配器写入 cap_ring 模拟）
    let (_m, addr) = start_server(vec![CableConfig {
        number: 1,
        name: String::new(),
        sample_rate: 48_000,
        bits: 16,
        mode: CableMode::Mixer,
        buffer_ms: 250,
    }]);

    let mut c = Client::connect(addr).await;
    c.op_request(OP_REQ_IMPORT).await;
    let mut busid = [0u8; 32];
    busid[..3].copy_from_slice(b"1-1");
    c.write(&busid).await;
    let (_, status) = c.op_reply().await;
    assert_eq!(status, 0);
    c.read_device().await;
    c.control(1, [0x00, 0x09, 1, 0, 0, 0, 0, 0], &[], 0).await;
    c.control(2, [0x01, 0x0B, 1, 0, 1, 0, 0, 0], &[], 0).await;
    c.control(3, [0x01, 0x0B, 1, 0, 2, 0, 0, 0], &[], 0).await;

    // 未写入任何数据 → 麦克风端应为数字静音（而不是错误/STALL）
    let descs = vec![(0u32, BYTES_PER_MS as u32)];
    c.submit(&SubmitSpec {
        seq: 4,
        ep: 2,
        dir: DIR_IN,
        transfer_len: BYTES_PER_MS as u32,
        packets: 1,
        setup: [0; 8],
        payload: Vec::new(),
        descs,
    })
    .await;
    let r = c.read_reply(true).await;
    assert_eq!(r.status, STATUS_OK);
    assert_eq!(r.data.len(), BYTES_PER_MS);
    assert!(r.data.iter().all(|&b| b == 0), "无路由时应为静音填充");
}

/// 「保存」线缆配置时端口不能被自己占住。
///
/// 回归用例：旧实现 `start()` 先 `stop()`（只 abort accept 任务）再立刻 bind，
/// 而连接任务会活过服务器本身、内核侧的 usbip-win2 又长期攥着那条 TCP 连接，
/// 本地端口随即被占 → 用户点「保存」直接报「绑定 127.0.0.1:3240 失败（端口被占用？）」。
/// 现在服务器在跑就只换线缆表，不重绑端口，已接入的会话也保持可用。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_cables_while_attached_does_not_rebind_port() {
    let port = free_port();
    let manager = UsbIpManager::new(format!("127.0.0.1:{port}"));
    let rt = tokio::runtime::Handle::current();
    manager.start(&rt, vec![cable_config(1, 48_000, 16)]).expect("服务器应能启动");
    let addr = manager.local_addr().expect("应能查到监听地址");

    // 已接入的设备：长连接一直挂着
    let mut c = Client::connect(addr).await;
    assert_eq!(import_cable(&mut c, "1-1").await.busid, "1-1");

    // 改采样率后点「保存」：必须成功，且不能换端口
    manager
        .start(&rt, vec![cable_config(1, 96_000, 24)])
        .expect("保存线缆不应因端口被占用而失败");
    assert!(manager.running(), "保存后服务器仍应在运行");
    assert_eq!(manager.local_addr(), Some(addr), "保存不应更换监听地址");
    let cables = manager.cables();
    assert_eq!(cables.len(), 1);
    assert_eq!(cables[0].cfg.sample_rate, 96_000, "线缆表应换成新配置");
    assert_eq!(cables[0].cfg.bits, 24);

    // 已接入的会话不会被踢掉（改格式要重新 attach 才生效，UI 会自动重新附加）
    let r = c.control(1, [0x00, 0x09, 1, 0, 0, 0, 0, 0], &[], 0).await;
    assert_eq!(r.status, STATUS_OK, "保存后老会话应继续可用");

    // 新连接看到的是新线缆
    let mut c2 = Client::connect(addr).await;
    let dev = import_cable(&mut c2, "1-1").await;
    assert_eq!(dev.id_product, 0xCA01);
}

/// `stop()` 必须把端口真正释放（连已接入会话的套接字一起），之后才能重新启用。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_releases_port_with_live_session_then_restart_works() {
    let port = free_port();
    let manager = UsbIpManager::new(format!("127.0.0.1:{port}"));
    let rt = tokio::runtime::Handle::current();
    manager.start(&rt, vec![cable_config(1, 48_000, 16)]).expect("服务器应能启动");
    let addr = manager.local_addr().expect("应能查到监听地址");

    let mut c = Client::connect(addr).await;
    import_cable(&mut c, "1-1").await;

    manager.stop();
    assert!(!manager.running(), "stop 后不应再是运行中");
    assert!(manager.cables().is_empty(), "stop 后线缆表应清空");
    // 端口确实放开了：外部进程能立刻绑上同一个地址
    let probe = std::net::TcpListener::bind(addr).expect("stop 后监听套接字必须已释放");
    drop(probe);

    manager
        .start(&rt, vec![cable_config(1, 44_100, 16)])
        .expect("停掉后应能重新启动");
    assert!(manager.running());
    assert_eq!(manager.local_addr(), Some(addr), "同一端口应能重新监听");
    drop(c);
}
