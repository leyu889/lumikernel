//! ICMP（RFC 792）：只做 Echo Request/Reply。
//!
//! 这是端到端连通性的最短验证路径：ARP → IP → ICMP 一个来回，
//! 每层协议都被真实地穿过一次。ping 不通，哪一层都有得查——
//! 它是"网络栈活着"的最低证明。

use crate::{checksum, NetError, NetResult};

/// 类型。
pub const TYPE_ECHO_REPLY: u8 = 0;
pub const TYPE_ECHO_REQUEST: u8 = 8;
/// 代码：Echo 全用 0。
pub const CODE_ECHO: u8 = 0;

pub const ICMP_ECHO_MIN: usize = 8;

/// 解析。返回 `(类型, 标识, 序号, 数据)`。
pub fn parse(buf: &[u8]) -> NetResult<(u8, u16, u16, &[u8])> {
    if buf.len() < ICMP_ECHO_MIN {
        return Err(NetError::TooShort);
    }
    if checksum(buf) != 0 {
        return Err(NetError::BadField);
    }
    let typ = buf[0];
    let id = u16::from_be_bytes([buf[4], buf[5]]);
    let seq = u16::from_be_bytes([buf[6], buf[7]]);
    Ok((typ, id, seq, &buf[ICMP_ECHO_MIN..]))
}

/// 组 Echo 包（请求或回复——回复就是把请求的类型换掉、校验和重算）。
pub fn build_echo(typ: u8, id: u16, seq: u16, data: &[u8], out: &mut [u8]) -> NetResult<usize> {
    let total = ICMP_ECHO_MIN + data.len();
    if out.len() < total {
        return Err(NetError::NoSpace);
    }
    out[0] = typ;
    out[1] = CODE_ECHO;
    out[2..4].copy_from_slice(&[0, 0]); // 校验和先清零
    out[4..6].copy_from_slice(&id.to_be_bytes());
    out[6..8].copy_from_slice(&seq.to_be_bytes());
    out[ICMP_ECHO_MIN..total].copy_from_slice(data);
    let c = checksum(&out[..total]);
    out[2..4].copy_from_slice(&c.to_be_bytes());
    Ok(total)
}

/// 从请求直接造回复（linux ping 的最短路径）。
pub fn reply_to(request: &[u8], out: &mut [u8]) -> NetResult<usize> {
    if request.len() < ICMP_ECHO_MIN {
        return Err(NetError::TooShort);
    }
    if out.len() < request.len() {
        return Err(NetError::NoSpace);
    }
    out[..request.len()].copy_from_slice(request);
    out[0] = TYPE_ECHO_REPLY;
    let c = checksum(&out[..request.len()]);
    out[2..4].copy_from_slice(&c.to_be_bytes());
    Ok(request.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_roundtrip() {
        let data = b"lumikernel-ping";
        let mut buf = [0u8; 64];
        let n = build_echo(TYPE_ECHO_REQUEST, 0x1234, 7, data, &mut buf).unwrap();
        let (typ, id, seq, got) = parse(&buf[..n]).unwrap();
        assert_eq!(typ, TYPE_ECHO_REQUEST);
        assert_eq!(id, 0x1234);
        assert_eq!(seq, 7);
        assert_eq!(got, data);
    }

    #[test]
    fn corrupted_icmp_rejected() {
        let mut buf = [0u8; 64];
        let n = build_echo(TYPE_ECHO_REQUEST, 1, 1, &[1, 2, 3], &mut buf).unwrap();
        buf[8] ^= 1;
        assert_eq!(parse(&buf[..n]), Err(NetError::BadField));
    }
}
