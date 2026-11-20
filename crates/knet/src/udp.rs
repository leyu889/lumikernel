//! UDP（RFC 768）。
//!
//! 头 8 字节：源端口/目的端口/长度/校验和。校验和的伪头要用 IP 头的
//! 源目地址——这层是唯一知道"校验和算完放哪"的地方，所以组包在这里做。

use crate::{checksum, NetError, NetResult, UDP_HDR};

pub fn parse(buf: &[u8]) -> NetResult<(u16, u16, &[u8])> {
    if buf.len() < UDP_HDR {
        return Err(NetError::TooShort);
    }
    let sport = u16::from_be_bytes([buf[0], buf[1]]);
    let dport = u16::from_be_bytes([buf[2], buf[3]]);
    let len = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    if len < UDP_HDR || len > buf.len() {
        return Err(NetError::BadField);
    }
    Ok((sport, dport, &buf[UDP_HDR..len]))
}

/// 组 UDP 包。**校验和可选**：IPv4 里 UDP 校验和为 0 表示"没算"，
/// 本地 NAT 场景直接写 0 是合法的——但为了对得起"协议正确"四个字，还是真算。
///
/// `pseudo` 是 IP 伪头：源地址(4) + 目的地址(4) + 零(1) + 协议号(1) + UDP长度(2)。
pub fn build(
    sport: u16,
    dport: u16,
    pseudo: &[u8; 12],
    payload: &[u8],
    out: &mut [u8],
) -> NetResult<usize> {
    let total = UDP_HDR + payload.len();
    if out.len() < total {
        return Err(NetError::NoSpace);
    }
    out[0..2].copy_from_slice(&sport.to_be_bytes());
    out[2..4].copy_from_slice(&dport.to_be_bytes());
    out[4..6].copy_from_slice(&(total as u16).to_be_bytes());
    out[6..8].copy_from_slice(&[0, 0]);
    out[UDP_HDR..total].copy_from_slice(payload);
    // 校验和 = 伪头 + 整个 UDP 包
    let mut tmp = [0u8; 12 + 1500];
    let n = 12 + total;
    if n > tmp.len() {
        return Err(NetError::TooBig);
    }
    tmp[..12].copy_from_slice(pseudo);
    tmp[12..n].copy_from_slice(&out[..total]);
    let c = checksum(&tmp[..n]);
    out[6..8].copy_from_slice(&c.to_be_bytes());
    Ok(total)
}

/// 构造 IP 伪头。
pub fn pseudo(src: crate::ipv4::Ipv4, dst: crate::ipv4::Ipv4, udp_len: u16) -> [u8; 12] {
    let mut p = [0u8; 12];
    p[0..4].copy_from_slice(&src);
    p[4..8].copy_from_slice(&dst);
    p[9] = crate::ipv4::PROTO_UDP;
    p[10..12].copy_from_slice(&udp_len.to_be_bytes());
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let data = b"hello udp";
        let mut buf = [0u8; 64];
        let n = build(
            1234,
            5678,
            &pseudo([10, 0, 2, 15], [10, 0, 2, 2], (UDP_HDR + data.len()) as u16),
            data,
            &mut buf,
        )
        .unwrap();
        let (sp, dp, got) = parse(&buf[..n]).unwrap();
        assert_eq!((sp, dp), (1234, 5678));
        assert_eq!(got, data);
    }
}
