//! 以太网 II 帧。
//!
//! 帧布局：`[目的 MAC 6][源 MAC 6][EtherType 2][载荷...]`
//!
//! 为什么放最底层的一层不做校验：链路层已经由网卡校验过 FCS，
//! 能从 RX 环里拿出来的帧都是硬件认为完好的。我们只做**长度与类型**检查。

use crate::{NetError, NetResult, ETH_HDR, MTU};

/// 我们关心的 EtherType。
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV4: u16 = 0x0800;

/// 6 字节 MAC 地址（新类型只为让函数签名自解释）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mac(pub [u8; 6]);

impl Mac {
    pub const ZERO: Mac = Mac([0; 6]);
    /// 广播地址：ARP 请求用它，别的包不应该用。
    pub const BROADCAST: Mac = Mac([0xFF; 6]);

    pub fn is_broadcast(&self) -> bool {
        self.0 == [0xFF; 6]
    }

    pub fn format(&self, out: &mut [u8]) {
        // "xx:xx:xx:xx:xx:xx" 17 字节
        let hex = b"0123456789abcdef";
        for i in 0..6 {
            out[i * 3] = hex[(self.0[i] >> 4) as usize];
            out[i * 3 + 1] = hex[(self.0[i] & 0xF) as usize];
            if i < 5 {
                out[i * 3 + 2] = b':';
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Frame<'a> {
    pub dst: Mac,
    pub src: Mac,
    pub ethertype: u16,
    /// 载荷（不含以太网头）。
    pub payload: &'a [u8],
}

/// 解析一个以太网帧。
pub fn parse(buf: &[u8]) -> NetResult<Frame<'_>> {
    if buf.len() < ETH_HDR {
        return Err(NetError::TooShort);
    }
    let ethertype = u16::from_be_bytes([buf[12], buf[13]]);
    Ok(Frame {
        dst: Mac([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5]]),
        src: Mac([buf[6], buf[7], buf[8], buf[9], buf[10], buf[11]]),
        ethertype,
        payload: &buf[ETH_HDR..],
    })
}

/// 组帧。`out` 至少要 `ETH_HDR + payload.len()` 大。
pub fn build(dst: Mac, src: Mac, ethertype: u16, payload: &[u8], out: &mut [u8]) -> NetResult<usize> {
    let total = ETH_HDR + payload.len();
    if total > MTU {
        return Err(NetError::TooBig);
    }
    if out.len() < total {
        return Err(NetError::NoSpace);
    }
    out[0..6].copy_from_slice(&dst.0);
    out[6..12].copy_from_slice(&src.0);
    out[12..14].copy_from_slice(&ethertype.to_be_bytes());
    out[ETH_HDR..total].copy_from_slice(payload);
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let payload = [1u8, 2, 3, 4, 5, 6, 7];
        let mut buf = [0u8; 64];
        let n = build(
            Mac::BROADCAST,
            Mac([0x52, 0x54, 0, 0, 0, 1]),
            ETHERTYPE_ARP,
            &payload,
            &mut buf,
        )
        .unwrap();
        assert_eq!(n, ETH_HDR + 7);
        let f = parse(&buf[..n]).unwrap();
        assert_eq!(f.dst, Mac::BROADCAST);
        assert_eq!(f.ethertype, ETHERTYPE_ARP);
        assert_eq!(f.payload, &payload);
    }

    #[test]
    fn too_short_is_rejected() {
        assert_eq!(parse(&[0u8; 10]), Err(NetError::TooShort));
    }
}
