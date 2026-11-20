//! socket 系统调用（AF_INET，UDP + 最小 TCP）。
//!
//! ## fd 与 socket 的关系
//!
//! 我们的 fd 表存的是 `File`（文件/管道/设备）。socket 用**独立的表**
//! （`net::Sockets`），fd 只存一个"socket 索引 + 类型"。
//! 之所以不把 socket 塞进 fd 表：fd 的读写语义（offset、目录、块设备）
//! 与 socket 完全不同，硬塞会让每条 I/O 路径都多一个分支。
//!
//! ## 与 Linux 的差异（诚实标注）
//!
//! - `SOCK_STREAM` 只实现了一套"单连接"语义：`listen` 后每个新连接
//!   占据同一个 socket（没有独立的 accept queue）。`nc -l` / HTTP GET
//!   这种一次一个连接的场景可用。
//! - `accept` 返回的 fd 就是 listen socket 本身（Linux 会给新 fd）。
//! - `sendto`/`recvfrom` 只支持 AF_INET。
//!
//! 这些差异在 `docs/08-规模对标.md` 的"诚实边界"里登记。

use super::uaccess;
use knet;
use alloc::vec::Vec;
use klinux::{errno as E, flags as F, structs};
use crate::net;

/// socket 的 fd 表示：`Kind::SocketUdp/SocketTcp` + `id` = `net` 表下标。
///
/// 不用"藏在 inode 字段"的花招：`Kind` 已经是类型系统，
/// 借位存储只会让每条 fd 路径多一个隐藏分支。
fn alloc_sock_fd(kind: u8, idx: usize) -> i64 {
    match crate::fd::alloc_socket_fd(kind, idx) {
        Some(fd) => fd as i64,
        None => E::err(E::ENFILE),
    }
}

fn sock_of(fd: i64) -> Result<(u8, usize), i64> {
    let f = crate::fd::get(fd as usize).map_err(|e| e)?;
    let k = match f.kind {
        crate::fd::Kind::SocketUdp => 1u8,
        crate::fd::Kind::SocketTcp => 2u8,
        _ => return Err(E::err(E::ENOTSOCK)),
    };
    Ok((k, f.id as usize))
}

/// `socket(domain, type, protocol)`。
pub fn socket(domain: u64, ty: u64, _proto: u64) -> i64 {
    if domain != 2 {
        // AF_INET = 2。AF_UNIX/AF_INET6 明确拒绝。
        return E::err(E::EAFNOSUPPORT);
    }
    let ty = ty & 0xFF;
    match ty {
        2 => net::udp_alloc(0).map(|idx| alloc_sock_fd(1, idx)).unwrap_or_else(|| E::err(E::ENOMEM)),
        1 => net::tcp_alloc(0, [0, 0, 0, 0], 0, false)
            .map(|idx| alloc_sock_fd(2, idx))
            .unwrap_or_else(|| E::err(E::ENOMEM)),
        _ => E::err(E::ESOCKTNOSUPPORT),
    }
}

/// sockaddr_in（16 字节）：family(2) port(2) ip(4) zero(8)。
fn read_sockaddr_in(ptr: u64) -> Result<([u8; 4], u16), i64> {
    let mut buf = [0u8; 16];
    uaccess::copy_in(ptr, &mut buf).map_err(|e| e)?;
    if u16::from_le_bytes([buf[0], buf[1]]) != 2 {
        return Err(E::err(E::EAFNOSUPPORT));
    }
    Ok(([buf[4], buf[5], buf[6], buf[7]], u16::from_be_bytes([buf[2], buf[3]])))
}

/// `bind(fd, addr, len)`。
pub fn bind(fd: i64, addr: u64, _len: u64) -> i64 {
    let (kind, idx) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let (ip, port) = match read_sockaddr_in(addr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if !ip.iter().all(|&b| b == 0) {
        return E::err(E::EINVAL);
    }
    if port == 0 {
        return E::err(E::EINVAL);
    }
    match kind {
        SOCK_UDP => {
            net::udp_rebind(idx, port);
            0
        }
        SOCK_TCP => {
            net::tcp_rebind(idx, port);
            0
        }
        _ => E::err(E::ENOTSOCK),
    }
}

/// `listen(fd, backlog)`。
pub fn listen(fd: i64, _backlog: u64) -> i64 {
    let (kind, idx) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if kind != 2 {
        return E::err(E::EOPNOTSUPP);
    }
    match net::tcp_listen(idx) {
        Ok(()) => 0,
        Err(_) => E::err(E::EINVAL),
    }
}

/// `connect(fd, addr, len)`：发 SYN 并阻塞等待握手完成。
pub fn connect(fd: i64, addr: u64, _len: u64) -> i64 {
    let (kind, idx) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if kind != 2 {
        return E::err(E::EOPNOTSUPP);
    }
    let (ip, port) = match read_sockaddr_in(addr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match net::tcp_connect_to(idx, ip, port) {
        Ok(()) => 0,
        Err(_) => E::err(E::ECONNREFUSED),
    }
}

/// `sendto(fd, buf, len, flags, addr, addrlen)`。
pub fn sendto(fd: i64, buf: u64, len: u64, _flags: u64, addr: u64, _alen: u64) -> i64 {
    let (kind, idx) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let want = (len as usize).min(knet::MTU);
    let mut data = alloc_vec(want);
    if let Err(e) = uaccess::copy_in(buf, &mut data[..want]) {
        return e;
    }
    match kind {
        SOCK_UDP => {
            // 有地址用地址；没地址用 connect 过的（简化：没地址就 EINVAL）
            let (ip, port) = if addr != 0 {
                match read_sockaddr_in(addr) {
                    Ok(v) => v,
                    Err(e) => return e,
                }
            } else {
                return E::err(E::EDESTADDRREQ);
            };
            match net::udp_send(idx, ip, port, &data[..want]) {
                Ok(_) => want as i64,
                Err(_) => E::err(E::EIO),
            }
        }
        SOCK_TCP => match net::tcp_send(idx, &data[..want]) {
            Ok(n) => n as i64,
            Err(_) => E::err(E::EIO),
        },
        _ => E::err(E::ENOTSOCK),
    }
}

/// `recvfrom(fd, buf, len, flags, addr, addrlen)`。
pub fn recvfrom(fd: i64, buf: u64, len: u64, _flags: u64, addr: u64, alen: u64) -> i64 {
    let (kind, idx) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let want = (len as usize).min(knet::MTU);
    let mut out = alloc_vec(want);

    match kind {
        SOCK_UDP => {
            let (ip, port, n) = match net::udp_recv(idx, &mut out[..want]) {
                Some(v) => v,
                None => return E::err(E::EAGAIN), // WNOHANG 风格：没数据就回
            };
            if addr != 0 && alen != 0 {
                let mut sa = [0u8; 16];
                sa[0..2].copy_from_slice(&2u16.to_le_bytes());
                sa[2..4].copy_from_slice(&port.to_be_bytes());
                sa[4..8].copy_from_slice(&ip);
                let _ = uaccess::unchecked_copy_out(addr, &sa);
                let l = 16u32;
                let _ = uaccess::unchecked_copy_out(alen, &l.to_le_bytes());
            }
            if let Err(e) = uaccess::unchecked_copy_out(buf, &out[..n]) {
                return e;
            }
            n as i64
        }
        SOCK_TCP => {
            let n = net::tcp_read(idx, &mut out[..want]);
            if n == 0 {
                return match net::tcp_state(idx) {
                    Some((_, true)) => 0, // EOF
                    _ => E::err(E::EAGAIN),
                };
            }
            if let Err(e) = uaccess::unchecked_copy_out(buf, &out[..n]) {
                return e;
            }
            n as i64
        }
        _ => E::err(E::ENOTSOCK),
    }
}

/// `accept(fd, addr, addrlen)`：返回 listen socket 自身（见顶部说明）。
pub fn accept(fd: i64, _addr: u64, _alen: u64) -> i64 {
    let (_, idx) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    // 等 established
    for _ in 0..300 {
        if let Some((true, _)) = net::tcp_state(idx) {
            return fd; // 就返回 listen fd 本身（诚实边界）
        }
        crate::intr::enable();
        crate::intr::frame::halt_once();
        crate::intr::disable();
    }
    E::err(E::EAGAIN)
}

/// `close(fd)` 里的 socket 分支：按 Kind 清理 socket 表。
pub fn close_if_socket(kind_id: (u8, u32)) -> bool {
    let (kind, id) = kind_id;
    match kind {
        1 => {
            net::close_udp(id as usize);
            true
        }
        2 => {
            net::close_tcp(id as usize);
            true
        }
        _ => false,
    }
}

fn alloc_vec(n: usize) -> Vec<u8> {
    let mut v = Vec::new();
    v.resize(n, 0);
    v
}
