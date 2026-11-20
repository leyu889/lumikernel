//! 网络协议栈：以太网 / ARP / IPv4 / ICMP / UDP / TCP。
//!
//! ## 与内核的边界
//!
//! 本 crate 是**纯逻辑**：不碰 MMIO、不分配内存（`#![no_std]` + 不用 alloc），
//! 只做"字节进来 → 解析/组装 → 字节出去"。内核侧（`dev/e1000`）负责硬件，
//! `linux/net` 负责系统调用语义。这样协议解析的 bug 可以在宿主机上
//! 用单元测试抓，不用每次都启动虚拟机。
//!
//! ## 诚实边界
//!
//! - IPv4 **不做分片与重组**：MTU 内的包直接发，超 MTU 的包丢弃（返回错误）。
//!   真实网络里几乎只有 DHCP 初始包会超，我们用静态配置（VBox NAT 是 10.0.2.15/24），
//!   不需要 DHCP。
//! - TCP 是**单连接规模**的最小实现：无拥塞控制、窗口固定 64 KiB、
//!   无乱序重组（乱序段直接丢弃）。够跑 `nc`/HTTP GET 这类顺序数据。
//! - 所有解析器对坏包的态度都是**丢掉并计数**，绝不 panic——
//!   网络输入是敌意的，这一点和 syscall fuzz 的结论一致。

#![no_std]
#![deny(unsafe_code)]

pub mod arp;
pub mod eth;
pub mod icmp;
pub mod ipv4;
pub mod tcp;
pub mod udp;

/// 以太网 MTU（不含 4 字节 FCS——硬件会校验并剥掉）。
pub const MTU: usize = 1500;

/// 以太网头长度：6 目的 + 6 源 + 2 类型。
pub const ETH_HDR: usize = 14;

/// IPv4 头最小长度（无选项）。
pub const IP_HDR_MIN: usize = 20;

/// UDP 头长度。
pub const UDP_HDR: usize = 8;

/// TCP 头最小长度。
pub const TCP_HDR_MIN: usize = 20;

/// 解析结果的统一错误。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum NetError {
    /// 包比协议头还短。
    TooShort,
    /// 版本号 / 校验和 / 类型等字段的值不对。
    BadField,
    /// 包超过 MTU，发不出去。
    TooBig,
    /// 该协议路径没实现（比如分片）。
    Unimplemented,
    /// 没有对应的连接/端口。
    NotFound,
    /// 缓冲区装不下。
    NoSpace,
}

pub type NetResult<T> = Result<T, NetError>;

/// 互联网校验和（RFC 1071）：按 16 位反码求和再取反码。
///
/// **这是全栈唯一必须逐位正确的算法**——校验和错一个位，
/// 对端就把整个包丢了，而且没有错误信息。
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    // 奇数长度：最后一字节当高字节补零
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    // 折叠高 16 位进低 16 位（两次足够：0xFFFF + 0xFFFF 最多进位一次）
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_rfc1071_example() {
        // RFC 1071 的例子：00 01 f2 03 f4 f5 f6 f7 → 220d
        assert_eq!(checksum(&[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7]), 0x220d);
    }

    #[test]
    fn checksum_odd_length() {
        // 00 01 02 → 奇数长度最后一字节补高位
        let c = checksum(&[0x00, 0x01, 0x02]);
        // sum = 0x0001 + 0x0200 = 0x0201 → ~0x0201 = 0xFDFE
        assert_eq!(c, 0xfdfe);
    }
}
