//! IPv4（RFC 791）。
//!
//! 只做"收发整机包"：无分片（发包侧拒绝超 MTU，收包侧丢弃 MF=1 的分片——
//! 没有重组缓冲，重组一半的包比直接丢更危险）。

use crate::{checksum, NetError, NetResult, IP_HDR_MIN, MTU};

/// 协议号。
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

/// IPv4 地址。
pub type Ipv4 = [u8; 4];

/// 解析出的头。
#[derive(Debug, PartialEq)]
pub struct Header {
    pub src: Ipv4,
    pub dst: Ipv4,
    pub proto: u8,
    /// 头长度（含选项，字节数）。
    pub ihl: usize,
    /// 是否还有分片（我们不支持重组）。
    pub more_fragments: bool,
}

pub fn parse(buf: &[u8]) -> NetResult<(Header, &[u8])> {
    if buf.len() < IP_HDR_MIN {
        return Err(NetError::TooShort);
    }
    if buf[0] >> 4 != 4 {
        return Err(NetError::BadField);
    }
    let ihl = ((buf[0] & 0xF) as usize) * 4;
    if ihl < IP_HDR_MIN || buf.len() < ihl {
        return Err(NetError::BadField);
    }
    // 头校验和必须过：错的头说明字节在传输中被改过，
    // 继续解析等于把不可信数据当可信数据用。
    if checksum(&buf[..ihl]) != 0 {
        return Err(NetError::BadField);
    }
    let total = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    if total < ihl || total > buf.len() {
        return Err(NetError::BadField);
    }
    let flags_frag = u16::from_be_bytes([buf[6], buf[7]]);
    Ok((
        Header {
            src: [buf[12], buf[13], buf[14], buf[15]],
            dst: [buf[16], buf[17], buf[18], buf[19]],
            proto: buf[9],
            ihl,
            more_fragments: flags_frag & 0x2000 != 0,
        },
        &buf[ihl..total],
    ))
}

/// 组包。返回写进 `out` 的总长度。
#[allow(clippy::too_many_arguments)]
pub fn build(
    src: Ipv4,
    dst: Ipv4,
    proto: u8,
    payload: &[u8],
    out: &mut [u8],
) -> NetResult<usize> {
    let total = IP_HDR_MIN + payload.len();
    if total > MTU {
        return Err(NetError::TooBig);
    }
    if out.len() < total {
        return Err(NetError::NoSpace);
    }
    out[0] = 0x45; // 版本 4，IHL 5（无选项）
    out[1] = 0; // DSCP/ECN 不用
    out[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    out[4..6].copy_from_slice(&0u16.to_be_bytes()); // id
    out[6..8].copy_from_slice(&0x4000u16.to_be_bytes()); // DF=1：不分片
    out[8] = 64; // TTL
    out[9] = proto;
    out[10..12].copy_from_slice(&[0, 0]); // 校验和先清零
    out[12..16].copy_from_slice(&src);
    out[16..20].copy_from_slice(&dst);
    let c = checksum(&out[..IP_HDR_MIN]);
    out[10..12].copy_from_slice(&c.to_be_bytes());
    out[IP_HDR_MIN..total].copy_from_slice(payload);
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_checksum() {
        let payload = [7u8; 24];
        let mut buf = [0u8; 100];
        let n = build([10, 0, 2, 15], [10, 0, 2, 2], PROTO_ICMP, &payload, &mut buf).unwrap();
        let (h, p) = parse(&buf[..n]).unwrap();
        assert_eq!(h.src, [10, 0, 2, 15]);
        assert_eq!(h.dst, [10, 0, 2, 2]);
        assert_eq!(h.proto, PROTO_ICMP);
        assert_eq!(p, &payload);
    }

    #[test]
    fn corrupted_checksum_is_rejected() {
        let payload = [7u8; 24];
        let mut buf = [0u8; 100];
        let n = build([10, 0, 2, 15], [10, 0, 2, 2], PROTO_ICMP, &payload, &mut buf).unwrap();
        buf[3] ^= 0xFF; // 破坏总长字段 → 校验和不再匹配
        assert_eq!(parse(&buf[..n]), Err(NetError::BadField));
    }
}
