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
/// ARP 协议类型：IPv4。**这是 16 位字段**。
const PR_IPV4: u16 = 0x0800;
/// 硬件地址长度（以太网 MAC 6 字节）。
const HLEN_ETHERNET: u8 = 6;
/// 协议地址长度（IPv4 4 字节）。
const PLEN_IPV4: u8 = 4;
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
    // ARP 头布局（RFC 826）：htype(2) ptype(2) hlen(1) plen(1) op(2) …
    //
    // 之前把 ptype 当成 4 字节读（[buf[2]..buf[6]]），把 hlen/plen 一起
    // 吞了进去，于是 pr 变成 0x08000604，永远不等于 0x0800 ——
    // **收到的每一个 ARP 回复都被判成坏包**。组包那边犯的是同一个错，
    // 所以"自己组包再自己解包"的单测一直是绿的。这类错误只有真跟设备
    // 对话才会暴露。
    let hw = u16::from_be_bytes([buf[0], buf[1]]);
    let pr = u16::from_be_bytes([buf[2], buf[3]]);
    let hlen = buf[4];
    let plen = buf[5];
    let op = u16::from_be_bytes([buf[6], buf[7]]);
    if hw != HW_ETHERNET || pr != PR_IPV4 || hlen != HLEN_ETHERNET || plen != PLEN_IPV4 {
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
    out[2..4].copy_from_slice(&PR_IPV4.to_be_bytes());
    out[4] = HLEN_ETHERNET;
    out[5] = PLEN_IPV4;
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

    /// 按 RFC 826 的字节布局逐位断言。
    ///
    /// 只做 roundtrip 是不够的：组包与解包用同一个错误偏移，
    /// 两边一起错，测试照样过（这个 bug 就是这样活到真机上的）。
    #[test]
    fn wire_bytes_match_rfc826() {
        let mut buf = [0u8; 64];
        let n = build(
            OP_REPLY,
            Mac([0x52, 0x54, 0x00, 0x12, 0x35, 0x00]),
            [10, 0, 2, 2],
            Mac([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]),
            [10, 0, 2, 15],
            &mut buf,
        )
        .unwrap();
        assert_eq!(n, ARP_LEN);
        assert_eq!(&buf[0..2], &[0x00, 0x01], "htype 必须是 1（以太网）");
        assert_eq!(&buf[2..4], &[0x08, 0x00], "ptype 必须是 0x0800（2 字节）");
        assert_eq!(buf[4], 6, "hlen 必须是 6");
        assert_eq!(buf[5], 4, "plen 必须是 4");
        assert_eq!(&buf[6..8], &[0x00, 0x02], "op 必须是 REPLY");
        assert_eq!(&buf[8..14], &[0x52, 0x54, 0x00, 0x12, 0x35, 0x00]);
        assert_eq!(&buf[14..18], &[10, 0, 2, 2]);
        assert_eq!(&buf[18..24], &[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        assert_eq!(&buf[24..28], &[10, 0, 2, 15]);
        // 解包必须能吃下带以太网填充的长包（真实回复是 64 字节）
        let (op, spa, _sha, tpa) = parse(&buf[..64]).unwrap();
        assert_eq!(op, OP_REPLY);
        assert_eq!(spa, [10, 0, 2, 2]);
        assert_eq!(tpa, [10, 0, 2, 15]);
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
