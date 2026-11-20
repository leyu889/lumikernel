//! TCP（RFC 793）的**最小可工作实现**。
//!
//! 范围：三次握手、顺序数据、FIN 关闭、RST 处理。
//! 不做：拥塞控制（窗口固定）、乱序重组（乱序段直接丢）、SACK、窗口缩放。
//! 定位是"能跟 busybox nc / HTTP GET 对话"——顺序数据的场景都覆盖。
//!
//! ## 状态机的裁剪
//!
//! 完整 TCP 有 11 个状态。这里保留 7 个：
//! `CLOSED → SYN_SENT → ESTABLISHED → FIN_WAIT → CLOSED`（主动打开/关闭）
//! `LISTEN → SYN_RECEIVED → ESTABLISHED → CLOSED`（被动打开）
//! 砍掉的是 CLOSE_WAIT/TIME_WAIT 等优雅关闭的精细状态——用"直接关"替代，
//! 对 NAT 内网的单连接场景够用，也容易审计。

use crate::{checksum, NetError, NetResult, TCP_HDR_MIN};

pub const FLAG_FIN: u8 = 0x01;
pub const FLAG_SYN: u8 = 0x02;
pub const FLAG_RST: u8 = 0x04;
pub const FLAG_ACK: u8 = 0x10;

/// 接收窗口（固定值，不做窗口缩放）。
pub const WINDOW: u16 = 0xFFFF;

/// 连接状态。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait,
}

/// 一条连接的全部状态。
#[derive(Clone, Copy)]
pub struct Conn {
    pub state: State,
    pub local_port: u16,
    pub remote_ip: crate::ipv4::Ipv4,
    pub remote_port: u16,
    /// 本机序号（下一个要发送的字节）。
    pub snd_nxt: u32,
    /// 对端已确认的序号（<= 它的数据可以丢弃了）。
    pub snd_una: u32,
    /// 期望收到的下一个字节（应答用）。
    pub rcv_nxt: u32,
    /// 收到的数据缓存在这里（由内核侧取走）。
    pub rxbuf_len: usize,
}

impl Conn {
    /// 主动打开：发 SYN。
    pub fn open(local_port: u16, remote_ip: crate::ipv4::Ipv4, remote_port: u16, isn: u32) -> Self {
        Conn {
            state: State::SynSent,
            local_port,
            remote_ip,
            remote_port,
            snd_nxt: isn,
            snd_una: isn,
            rcv_nxt: 0,
            rxbuf_len: 0,
        }
    }

    pub fn listen(local_port: u16) -> Self {
        Conn {
            state: State::Listen,
            local_port,
            remote_ip: [0; 4],
            remote_port: 0,
            snd_nxt: 0,
            snd_una: 0,
            rcv_nxt: 0,
            rxbuf_len: 0,
        }
    }
}

/// 解析 TCP 头。
pub struct Header {
    pub sport: u16,
    pub dport: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub data_offset: usize,
    pub window: u16,
}

pub fn parse(buf: &[u8]) -> NetResult<(Header, &[u8])> {
    if buf.len() < TCP_HDR_MIN {
        return Err(NetError::TooShort);
    }
    let data_offset = ((buf[12] >> 4) as usize) * 4;
    if data_offset < TCP_HDR_MIN || buf.len() < data_offset {
        return Err(NetError::BadField);
    }
    Ok((
        Header {
            sport: u16::from_be_bytes([buf[0], buf[1]]),
            dport: u16::from_be_bytes([buf[2], buf[3]]),
            seq: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            ack: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
            flags: buf[13],
            data_offset,
            window: u16::from_be_bytes([buf[14], buf[15]]),
        },
        &buf[data_offset..],
    ))
}

/// 组 TCP 段。`payload` 可为空（纯 ACK/SYN/FIN 段）。
#[allow(clippy::too_many_arguments)]
pub fn build(
    c: &Conn,
    flags: u8,
    seq: u32,
    ack: u32,
    payload: &[u8],
    pseudo: &[u8; 12],
    out: &mut [u8],
) -> NetResult<usize> {
    let total = TCP_HDR_MIN + payload.len();
    if out.len() < total {
        return Err(NetError::NoSpace);
    }
    out[0..2].copy_from_slice(&c.local_port.to_be_bytes());
    out[2..4].copy_from_slice(&c.remote_port.to_be_bytes());
    out[4..8].copy_from_slice(&seq.to_be_bytes());
    out[8..12].copy_from_slice(&ack.to_be_bytes());
    out[12] = 0x50; // 数据偏移 5（无选项）
    out[13] = flags;
    out[14..16].copy_from_slice(&WINDOW.to_be_bytes());
    out[16..18].copy_from_slice(&[0, 0]); // 紧急指针不用
    out[TCP_HDR_MIN..total].copy_from_slice(payload);

    let mut tmp = [0u8; 12 + 1500];
    let n = 12 + total;
    if n > tmp.len() {
        return Err(NetError::TooBig);
    }
    tmp[..12].copy_from_slice(pseudo);
    tmp[12..n].copy_from_slice(&out[..total]);
    // **SYN 段没有数据时校验和可以为 0 吗**？不行——TCP 校验和必填，
    // 算出来正好是 0 也要按原码线格式送（0x0000 会当"没校验"）。
    let c0 = checksum(&tmp[..n]);
    out[16..18].copy_from_slice(&if c0 == 0 { 0xFFFF } else { c0 }.to_be_bytes());
    Ok(total)
}

/// 推进序号比较（RFC 793 的循环比较，防序号回绕）。
pub fn seq_before(a: u32, b: u32) -> bool {
    // (int32)(a - b) < 0
    (a.wrapping_sub(b) as i32) < 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo() -> [u8; 12] {
        crate::udp::pseudo([10, 0, 2, 15], [10, 0, 2, 2], 0)
    }

    #[test]
    fn syn_roundtrip() {
        let c = Conn::open(40000, [10, 0, 2, 2], 80, 12345);
        let mut buf = [0u8; 64];
        let n = build(&c, FLAG_SYN, c.snd_nxt, 0, &[], &pseudo(), &mut buf).unwrap();
        let (h, payload) = parse(&buf[..n]).unwrap();
        assert_eq!(h.sport, 40000);
        assert_eq!(h.dport, 80);
        assert_eq!(h.flags, FLAG_SYN);
        assert_eq!(h.seq, 12345);
        assert!(payload.is_empty());
    }

    #[test]
    fn seq_wraparound() {
        // 序号从 u32::MAX 附近回绕：wrapping 比较才是对的
        assert!(seq_before(0xFFFF_FFF0, 0x10));
        assert!(!seq_before(0x10, 0xFFFF_FFF0));
    }
}
