//! USB/IP v1.1.1 传输协议（大端线格式）。
//!
//! 参考 Virtual-Cables internal/usbip/protocol.go（BSD-2-Clause）。
//! 所有头部/描述符字段按网络字节序编码；setup 包内部字段为 USB 规范定义的
//! 小端，由上层按原始字节解析。

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// USB/IP 协议版本（v1.1.1，usbip-win2 期望的版本）。
pub const PROTOCOL_VERSION: u16 = 0x0111;

/// OP 包（op_header）的请求/应答码：设备列表与 import（attach）握手。
pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;
pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;

/// URB 包（basic_header）的命令码：提交传输 / 取消传输 及其应答。
pub const CMD_SUBMIT: u32 = 0x0000_0001;
pub const CMD_UNLINK: u32 = 0x0000_0002;
pub const RET_SUBMIT: u32 = 0x0000_0003;
pub const RET_UNLINK: u32 = 0x0000_0004;

/// URB 传输方向（host 视角：IN = 设备 → 主机）。
pub const DIRECTION_OUT: u32 = 0;
pub const DIRECTION_IN: u32 = 1;

/// USB/IP speed 编码：1=low 2=full 3=high
/// （UAC2 = 高速；UAC1 是 USB 1.1 全速设备，必须按 full 上报）
pub const SPEED_HIGH: u32 = 3;
pub const SPEED_FULL: u32 = 2;

/// 非 iso URB 的 number_of_packets 哨兵值；不是错误，绝不能当负数解释。
pub const NO_ISO_PACKETS: u32 = 0xffff_ffff;

/// 单次传输长度上限（防恶意/异常客户端）
pub const MAX_TRANSFER_LENGTH: u32 = 16 * 1024 * 1024;
/// iso 包数上限
pub const MAX_ISO_PACKETS: u32 = 4096;

/// URB 应答状态码（沿 Linux 内核 errno 约定）。
pub const STATUS_OK: i32 = 0;
pub const STATUS_INVALID: i32 = -22; // -EINVAL
pub const STATUS_PIPE: i32 = -32; // -EPIPE / STALL
pub const STATUS_CONN_RESET: i32 = -104; // -ECONNRESET（成功 unlink 的规定状态码）

/// OP 层 8 字节头（设备列表 / import 握手）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpHeader {
    pub version: u16,
    pub code: u16,
    pub status: u32,
}

/// 读取并解析 OP 头（8 字节，大端）。
pub async fn read_op_header(r: &mut (impl AsyncRead + Unpin)) -> io::Result<OpHeader> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).await?;
    Ok(OpHeader {
        version: u16::from_be_bytes([buf[0], buf[1]]),
        code: u16::from_be_bytes([buf[2], buf[3]]),
        status: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
    })
}

pub async fn write_op_header(
    w: &mut (impl AsyncWrite + Unpin),
    code: u16,
    status: u32,
) -> io::Result<()> {
    let buf = [
        (PROTOCOL_VERSION >> 8) as u8,
        PROTOCOL_VERSION as u8,
        (code >> 8) as u8,
        code as u8,
    ];
    let mut frame = Vec::with_capacity(8);
    frame.extend_from_slice(&buf);
    frame.extend_from_slice(&status.to_be_bytes());
    w.write_all(&frame).await
}

/// URB 层 20 字节基本头，每条 URB 都以它开头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BasicHeader {
    pub command: u32,
    /// 请求序号（应答按它配对）
    pub sequence: u32,
    /// 目标设备（devlist 中的下标，本服务器只有一台设备恒为 0）
    pub device_id: u32,
    pub direction: u32,
    pub endpoint: u32,
}

/// 读取并解析 URB 基本头（20 字节，大端）。
pub async fn read_basic_header(r: &mut (impl AsyncRead + Unpin)) -> io::Result<BasicHeader> {
    let mut buf = [0u8; 20];
    r.read_exact(&mut buf).await?;
    let be = |o: usize| u32::from_be_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    Ok(BasicHeader {
        command: be(0),
        sequence: be(4),
        device_id: be(8),
        direction: be(12),
        endpoint: be(16),
    })
}

/// CMD_SUBMIT 请求：一次 USB 传输（控制 / 批量 / 等时）。
#[derive(Debug, Clone)]
pub struct SubmitRequest {
    pub basic: BasicHeader,
    pub transfer_flags: u32,
    /// 传输数据长度（上限 [`MAX_TRANSFER_LENGTH`]）
    pub transfer_buffer_length: u32,
    /// 仅等时传输有意义
    pub start_frame: u32,
    /// 等时包数；非等时为 [`NO_ISO_PACKETS`] 哨兵值
    pub number_of_packets: u32,
    /// 轮询间隔（中断端点为 ms，等时为 2^(bInterval-1) 帧）
    pub interval: u32,
    /// 控制传输的 8 字节 setup 包；非控制传输全 0
    pub setup: [u8; 8],
}

impl SubmitRequest {
    /// 是否为等时传输（音频数据走这里）。
    pub fn is_isochronous(&self) -> bool {
        self.number_of_packets != NO_ISO_PACKETS && self.number_of_packets != 0
    }
}

/// 读取 SUBMIT 请求体（basic 头之后的 24 字节：5×u32 + setup\[8\]）
pub async fn read_submit_body(
    r: &mut (impl AsyncRead + Unpin),
    basic: BasicHeader,
) -> io::Result<SubmitRequest> {
    let mut buf = [0u8; 28];
    r.read_exact(&mut buf).await?;
    let be = |o: usize| u32::from_be_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    let mut setup = [0u8; 8];
    setup.copy_from_slice(&buf[20..28]);
    Ok(SubmitRequest {
        basic,
        transfer_flags: be(0),
        transfer_buffer_length: be(4),
        start_frame: be(8),
        number_of_packets: be(12),
        interval: be(16),
        setup,
    })
}

/// 等时传输的单包描述符（16 字节，随 RET_SUBMIT 返回）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoPacket {
    /// 相对数据区的偏移
    pub offset: u32,
    pub length: u32,
    pub actual_length: u32,
    pub status: i32,
}

/// 读取 `count` 个等时包描述符（每个 16 字节，超限拒绝）。
pub async fn read_iso_packets(
    r: &mut (impl AsyncRead + Unpin),
    count: u32,
) -> io::Result<Vec<IsoPacket>> {
    if count == NO_ISO_PACKETS || count == 0 {
        return Ok(Vec::new());
    }
    if count > MAX_ISO_PACKETS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("iso packet count {count} 超限"),
        ));
    }
    let mut raw = vec![0u8; count as usize * 16];
    r.read_exact(&mut raw).await?;
    let mut packets = Vec::with_capacity(count as usize);
    for chunk in raw.chunks_exact(16) {
        let be32 = |o: usize| u32::from_be_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]]);
        packets.push(IsoPacket {
            offset: be32(0),
            length: be32(4),
            actual_length: be32(8),
            status: be32(12) as i32,
        });
    }
    Ok(packets)
}

/// 写一个完整的 USBIP_RET_SUBMIT 帧。规范要求应答中 devid/direction/endpoint
/// 必须为 0，仅 command 与 sequence 标识应答。
pub async fn write_ret_submit(
    w: &mut (impl AsyncWrite + Unpin),
    req: &SubmitRequest,
    status: i32,
    actual_length: u32,
    data: &[u8],
    packets: &[IsoPacket],
    error_count: u32,
) -> io::Result<()> {
    let (actual_length, data): (u32, &[u8]) = if status == STATUS_OK {
        (actual_length, data)
    } else {
        (0, &[])
    };
    let (number_of_packets, start_frame) = if req.is_isochronous() {
        (packets.len() as u32, req.start_frame)
    } else {
        (NO_ISO_PACKETS, 0)
    };

    let mut frame = Vec::with_capacity(48 + data.len() + packets.len() * 16);
    // basic header：command + sequence，其余为 0
    frame.extend_from_slice(&RET_SUBMIT.to_be_bytes());
    frame.extend_from_slice(&req.basic.sequence.to_be_bytes());
    frame.extend_from_slice(&[0u8; 12]);
    frame.extend_from_slice(&status.to_be_bytes());
    frame.extend_from_slice(&actual_length.to_be_bytes());
    frame.extend_from_slice(&start_frame.to_be_bytes());
    frame.extend_from_slice(&number_of_packets.to_be_bytes());
    frame.extend_from_slice(&error_count.to_be_bytes());
    frame.extend_from_slice(&0u64.to_be_bytes());
    frame.extend_from_slice(data);
    for p in packets {
        frame.extend_from_slice(&p.offset.to_be_bytes());
        frame.extend_from_slice(&p.length.to_be_bytes());
        frame.extend_from_slice(&p.actual_length.to_be_bytes());
        frame.extend_from_slice(&(p.status as u32).to_be_bytes());
    }
    w.write_all(&frame).await
}

/// 写 USBIP_RET_UNLINK 帧（basic header + status + 24 字节保留）。
pub async fn write_ret_unlink(
    w: &mut (impl AsyncWrite + Unpin),
    request: &BasicHeader,
    status: i32,
) -> io::Result<()> {
    let mut frame = Vec::with_capacity(48);
    frame.extend_from_slice(&RET_UNLINK.to_be_bytes());
    frame.extend_from_slice(&request.sequence.to_be_bytes());
    frame.extend_from_slice(&[0u8; 12]);
    frame.extend_from_slice(&status.to_be_bytes());
    frame.extend_from_slice(&[0u8; 24]);
    w.write_all(&frame).await
}

/// 把字符串写入定长 NUL 填充字段
pub fn fixed_string(dst: &mut [u8], value: &str) {
    dst.fill(0);
    let bytes = value.as_bytes();
    let n = bytes.len().min(dst.len());
    dst[..n].copy_from_slice(&bytes[..n]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn op_header_roundtrip() {
        let mut v = Vec::new();
        write_op_header(&mut v, OP_REP_DEVLIST, 0).await.unwrap();
        let h = read_op_header(&mut &v[..]).await.unwrap();
        assert_eq!(h.version, 0x0111);
        assert_eq!(h.code, OP_REP_DEVLIST);
        assert_eq!(h.status, 0);
    }

    #[tokio::test]
    async fn ret_submit_roundtrip() {
        let mut v = Vec::new();
        let req = SubmitRequest {
            basic: BasicHeader { command: CMD_SUBMIT, sequence: 42, device_id: 1, direction: DIRECTION_IN, endpoint: 2 },
            transfer_flags: 0,
            transfer_buffer_length: 192,
            start_frame: 7,
            number_of_packets: 10,
            interval: 1,
            setup: [0u8; 8],
        };
        let packets: Vec<IsoPacket> = (0..10)
            .map(|i| IsoPacket { offset: i * 192, length: 192, actual_length: 192, status: 0 })
            .collect();
        write_ret_submit(&mut v, &req, STATUS_OK, 192, &[0xAB; 192], &packets, 0)
            .await
            .unwrap();
        // 解析 basic 头
        let bh = read_basic_header(&mut &v[..]).await.unwrap();
        assert_eq!(bh.command, RET_SUBMIT);
        assert_eq!(bh.sequence, 42);
        assert_eq!(bh.device_id, 0, "应答中 devid 必须为 0");
        // status + actual
        assert_eq!(i32::from_be_bytes(v[20..24].try_into().unwrap()), STATUS_OK);
        assert_eq!(u32::from_be_bytes(v[24..28].try_into().unwrap()), 192);
        assert_eq!(u32::from_be_bytes(v[32..36].try_into().unwrap()), 10);
        // 数据在包描述符之前
        assert_eq!(v[48], 0xAB);
        assert_eq!(v[48 + 192], 0, "描述符区紧跟数据区");
    }

    #[test]
    fn fixed_string_pads() {
        let mut dst = [0u8; 32];
        fixed_string(&mut dst, "1-1");
        assert_eq!(&dst[..3], b"1-1");
        assert_eq!(dst[3], 0);
    }
}
