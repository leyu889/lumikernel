//! ARP（RFC 826）：IPv4 地址 ↔ MAC 地址。
//!
//! 一个进程里唯一的"邻接状态"就是 ARP 缓存。没有它，每发一个 IP 包
//! 都要先广播一次 ARP——网关会被这种重复请求刷屏（Linux 有类似的
//! "unresolved neighbor" 限流），所以缓存是必须的。
//!
//! 缓存结构故意做成**定长直接映射**而不是哈希：条目数最多 32 个
//! （一台 NAT 网关 + 少量局域网主机），线性扫一遍比哈希更快也更省内存。

use crate::{eth::Mac, NetError, NetResult};

/// ARP 硬件类型：以太网。
const HW_ETHERNET: u16 = 1;
/// ARP 协议类型：IPv4。
const PR_IPV4: u32 = 0x0800;
/// 操作码。
pub const OP_REQUEST: u16 = 1;
pub const OP_REPLY: u16 = 2;

/// ARP 包定长 28 字节（以太网 + IPv4 组合下）。
pub const ARP_LEN: usize = 28;

/// 缓存条目数（2 的幂不必要，这里线性扫）。
const CACHE_SIZE: usize = 32;

#[derive(Clone, Copy)]
pub struct Entry {
    pub ip: [u8; 4],
    pub mac: Mac,
    pub used: bool,
}

/// 固定 ARP 缓存。满了就覆盖第一个（一台 NAT 路由器场景下根本到不了 32）。
pub struct Cache {
    pub entries: [Entry; CACHE_SIZE],
}

impl Cache {
    pub const fn new() -> Self {
        Cache {
            entries: [Entry { ip: [0; 4], mac: Mac::ZERO, used: false }; CACHE_SIZE],
        }
    }

    /// 查缓存。
    pub fn lookup(&self, ip: &[u8; 4]) -> Option<Mac> {
        self.entries
            .iter()
            .find(|e| e.used && e.ip == *ip)
            .map(|e| e.mac)
    }

    /// 写缓存（已存在则覆盖 MAC）。
    pub fn insert(&mut self, ip: &[u8; 4], mac: Mac) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.used && e.ip == *ip) {
            e.mac = mac;
            return;
        }
        if let Some(e) = self.entries.iter_mut().find(|e| !e.used) {
            e.ip = *ip;
            e.mac = mac;
            e.used = true;
            return;
        }
        self.entries[0] = Entry { ip: *ip, mac, used: true };
    }
}

/// 解析 ARP 包。返回 `(操作码, 发送者 IP, 发送者 MAC, 目标 IP)`。
pub fn parse(buf: &[u8]) -> NetResult<(u16, [u8; 4], Mac, [u8; 4])> {
    if buf.len() < ARP_LEN {
        return Err(NetError::TooShort);
    }
    let hw = u16::from_be_bytes([buf[0], buf[1]]);
    let pr = u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]);
    let op = u16::from_be_bytes([buf[6], buf[7]]);
    if hw != HW_ETHERNET || pr != PR_IPV4 {
        return Err(NetError::BadField);
    }
    let sha = Mac([buf[8], buf[9], buf[10], buf[11], buf[12], buf[13]]);
    let spa = [buf[14], buf[15], buf[16], buf[17]];
    let tpa = [buf[24], buf[25], buf[26], buf[27]];
    Ok((op, spa, sha, tpa))
}

/// 组 ARP 包（请求或回复）。
pub fn build(
    op: u16,
    sender_mac: Mac,
    sender_ip: [u8; 4],
    target_mac: Mac,
    target_ip: [u8; 4],
    out: &mut [u8],
) -> NetResult<usize> {
    if out.len() < ARP_LEN {
        return Err(NetError::NoSpace);
    }
    out[0..2].copy_from_slice(&HW_ETHERNET.to_be_bytes());
    out[2..6].copy_from_slice(&PR_IPV4.to_be_bytes());
    out[6..8].copy_from_slice(&op.to_be_bytes());
    out[8..14].copy_from_slice(&sender_mac.0);
    out[14..18].copy_from_slice(&sender_ip);
    out[18..24].copy_from_slice(&target_mac.0);
    out[24..28].copy_from_slice(&target_ip);
    Ok(ARP_LEN)
}

/// 要问的 IPv4 地址（帮 `lib.rs` 的分发层判断"这个 ARP 是不是在问我们"）。
pub fn asking_us(tpa: &[u8; 4], our_ip: &[u8; 4]) -> bool {
    tpa == our_ip
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut buf = [0u8; 64];
        let n = build(
            OP_REQUEST,
            Mac([1, 2, 3, 4, 5, 6]),
            [10, 0, 2, 15],
            Mac::ZERO,
            [10, 0, 2, 2],
            &mut buf,
        )
        .unwrap();
        let (op, spa, sha, tpa) = parse(&buf[..n]).unwrap();
        assert_eq!(op, OP_REQUEST);
        assert_eq!(spa, [10, 0, 2, 15]);
        assert_eq!(sha, Mac([1, 2, 3, 4, 5, 6]));
        assert_eq!(tpa, [10, 0, 2, 2]);
    }

    #[test]
    fn cache_insert_lookup_overwrite() {
        let mut c = Cache::new();
        assert_eq!(c.lookup(&[10, 0, 2, 2]), None);
        c.insert(&[10, 0, 2, 2], Mac([0xAA; 6]));
        assert_eq!(c.lookup(&[10, 0, 2, 2]), Some(Mac([0xAA; 6])));
        c.insert(&[10, 0, 2, 2], Mac([0xBB; 6]));
        assert_eq!(c.lookup(&[10, 0, 2, 2]), Some(Mac([0xBB; 6])), "覆盖旧 MAC");
    }
}
