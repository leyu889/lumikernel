//! 网络收发的"控制平面"：IP 配置、ARP 缓存、包分发、socket 表。
//!
//! 与 `knet`（纯逻辑）的分工：
//! - `knet` 负责"字节 ↔ 协议结构"；
//! - 这里负责"哪个包给谁 + 状态放在哪 + 怎么发出去"。
//!
//! ## 配置
//!
//! VBox NAT 的固定拓扑：客户机 10.0.2.15/24，网关 10.0.2.2（它会应 ARP
//! 并转发 ICMP/UDP/TCP 到外网）。**不做 DHCP**——那要 68 端口 + UDP 分片，
//! 在"先把端到端跑通"的阶段是纯负担。边界写在 docs 里。

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use knet::{arp, eth, icmp, ipv4, tcp, udp, NetError};
use crate::{kserialln, kprintln};

/// 本机 IP（VBox NAT 的标准客户机地址）。
pub const OUR_IP: ipv4::Ipv4 = [10, 0, 2, 15];
/// 子网掩码（NAT 内网 /24）。
pub const NETMASK: ipv4::Ipv4 = [255, 255, 255, 0];
/// 网关（VBox NAT 路由器）。ping 它 = 端到端连通性测试。
pub const GATEWAY: ipv4::Ipv4 = [10, 0, 2, 2];

/// ARP 缓存（协议状态，锁内访问）。
static ARP_CACHE: crate::arch::sync::IrqSpinlock<arp::Cache> =
    crate::arch::sync::IrqSpinlock::new(arp::Cache::new());

/// 统计：(rx_frames, tx_frames, rx_drops, arp_resolved)。
static RX_FRAMES: AtomicU64 = AtomicU64::new(0);
static TX_FRAMES: AtomicU64 = AtomicU64::new(0);
static RX_DROPS: AtomicU64 = AtomicU64::new(0);
static ARP_OK: AtomicU64 = AtomicU64::new(0);

pub fn stats() -> (u64, u64, u64, u64) {
    (
        RX_FRAMES.load(Ordering::Relaxed),
        TX_FRAMES.load(Ordering::Relaxed),
        RX_DROPS.load(Ordering::Relaxed),
        ARP_OK.load(Ordering::Relaxed),
    )
}

/// 三元组：一条 TCP 连接的标识。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TcpKey {
    pub remote_ip: [u8; 4],
    pub remote_port: u16,
    pub local_port: u16,
}

/// socket 层的一条 TCP 连接。
pub struct TcpSock {
    pub key: TcpKey,
    pub conn: tcp::Conn,
    /// 收到的数据（内核侧 `read` 取走后清空）。
    pub rxbuf: Vec<u8>,
    /// 对端关了（读到 EOF）。
    pub eof: bool,
    /// 状态变化标记（connect 等握手完成）。
    pub established: bool,
}

/// UDP socket：端口 → 收到的数据报队列。
pub struct UdpSock {
    pub local_port: u16,
    /// `(源IP, 源端口, 数据)`。
    pub rxbuf: Vec<([u8; 4], u16, Vec<u8>)>,
}

/// 所有打开的 socket。**条目数上限 16**：一个 NAT 内网里跑不出这么多连接。
pub struct Sockets {
    pub tcp: [Option<TcpSock>; 16],
    pub udp: [Option<UdpSock>; 16],
    /// 端口分配（主动打开时用）。
    pub next_ephemeral: u16,
}

static SOCKETS: crate::arch::sync::IrqSpinlock<Sockets> = crate::arch::sync::IrqSpinlock::new(Sockets {
    tcp: [const { None }; 16],
    udp: [const { None }; 16],
    next_ephemeral: 40000,
});

/// 给 `socks.tcp[i]` 找下标（不持锁的辅助：先在锁内 copy key，再查）。
pub fn tcp_alloc(local_port: u16, remote_ip: [u8; 4], remote_port: u16, listen: bool) -> Option<usize> {
    let mut g = SOCKETS.lock();
    let isn = 0x2FAB_C000u32.wrapping_add(crate::intr::timer::ticks() as u32);
    let conn = if listen {
        tcp::Conn::listen(local_port)
    } else {
        tcp::Conn::open(local_port, remote_ip, remote_port, isn)
    };
    let slot = g.tcp.iter_mut().position(|s| s.is_none())?;
    g.tcp[slot] = Some(TcpSock {
        key: TcpKey { remote_ip, remote_port, local_port },
        conn,
        rxbuf: Vec::new(),
        eof: false,
        established: false,
    });
    Some(slot)
}

pub fn udp_alloc(local_port: u16) -> Option<usize> {
    let mut g = SOCKETS.lock();
    let slot = g.udp.iter_mut().position(|s| s.is_none())?;
    g.udp[slot] = Some(UdpSock { local_port, rxbuf: Vec::new() });
    Some(slot)
}

pub fn close_tcp(idx: usize) {
    let mut g = SOCKETS.lock();
    if idx < g.tcp.len() {
        g.tcp[idx] = None;
    }
}

pub fn close_udp(idx: usize) {
    let mut g = SOCKETS.lock();
    if idx < g.udp.len() {
        g.udp[idx] = None;
    }
}

/// 发一个以太网帧（ARP 或 IP）。
fn send_eth(dst_mac: eth::Mac, ethertype: u16, payload: &[u8]) -> Result<(), &'static str> {
    let mut out = [0u8; 1514];
    let n = eth::build(dst_mac, eth::Mac(crate::dev::nic::mac()), ethertype, payload, &mut out)
        .map_err(|_| "组帧失败")?;
    crate::dev::nic::send(&out[..n])?;
    TX_FRAMES.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// 解析目的 MAC：缓存有就直接用，没有先发 ARP 请求并失败一次
/// （调用方下次重试时缓存已经热了——这是内核网络栈的常规做法）。
fn resolve(ip: &[u8; 4]) -> Option<eth::Mac> {
    {
        let c = ARP_CACHE.lock();
        if let Some(m) = c.lookup(ip) {
            return Some(m);
        }
    }
    // 广播 ARP 请求
    let mut payload = [0u8; 28];
    let n = arp::build(
        arp::OP_REQUEST,
        eth::Mac(crate::dev::nic::mac()),
        OUR_IP,
        eth::Mac::ZERO,
        *ip,
        &mut payload,
    )
    .ok()?;
    let _ = send_eth(eth::Mac::BROADCAST, eth::ETHERTYPE_ARP, &payload[..n]);
    None
}

/// 立即解析（阻塞等 ARP 回应，最多 ~50 ms）。给 ICMP ping 用，
/// 因为"第一次 ping"必须真的能发出去。
fn resolve_wait(ip: &[u8; 4]) -> Option<eth::Mac> {
    if let Some(m) = resolve(ip) {
        return Some(m);
    }
    // 等回包：poll RX 直到缓存有或超时
    for _ in 0..50 {
        crate::dev::nic::poll(|frame| {
            handle_frame(frame);
        });
        let c = ARP_CACHE.lock();
        if let Some(m) = c.lookup(ip) {
            return Some(m);
        }
        // 100 µs 级别的等待：这里不能 hlt（会等满 10ms tick）
        for _ in 0..50_000 {
            core::hint::spin_loop();
        }
    }
    None
}

/// 处理一帧（从 e1000 poll 进来）。
pub fn handle_frame(frame: &[u8]) {
    let f = match eth::parse(frame) {
        Ok(f) => f,
        Err(_) => {
            RX_DROPS.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    // 目的 MAC 不是我们/广播/组播的帧：网卡混杂前不该出现，出现就丢
    let our = eth::Mac(crate::dev::nic::mac());
    if f.dst != our && !f.dst.is_broadcast() {
        RX_DROPS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    RX_FRAMES.fetch_add(1, Ordering::Relaxed);

    match f.ethertype {
        eth::ETHERTYPE_ARP => handle_arp(f.payload),
        eth::ETHERTYPE_IPV4 => handle_ipv4(f.payload),
        _ => {
            RX_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// ARP：请求要回答，回复要进缓存。
fn handle_arp(buf: &[u8]) {
    let (op, spa, sha, tpa) = match arp::parse(buf) {
        Ok(v) => v,
        Err(_) => {
            RX_DROPS.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    // **先写缓存再回答**：请求里也带着发送者信息（RFC 826 的要求），
    // 只存"请求我们的人"会导致"我们主动问的人"永远进不了缓存。
    if !sha.is_broadcast() {
        ARP_CACHE.lock().insert(&spa, sha);
    }
    match op {
        arp::OP_REQUEST => {
            if arp::asking_us(&tpa, &OUR_IP) {
                let mut payload = [0u8; 28];
                if let Ok(n) = arp::build(
                    arp::OP_REPLY,
                    eth::Mac(crate::dev::nic::mac()),
                    OUR_IP,
                    sha,
                    spa,
                    &mut payload,
                ) {
                    let _ = send_eth(sha, eth::ETHERTYPE_ARP, &payload[..n]);
                }
            }
        }
        arp::OP_REPLY => {
            ARP_OK.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

/// IPv4：按协议分发。
fn handle_ipv4(buf: &[u8]) {
    let (h, payload) = match ipv4::parse(buf) {
        Ok(v) => v,
        Err(_) => {
            RX_DROPS.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    // 不收分片：没有重组缓冲，重组一半的包比直接丢更危险。
    if h.more_fragments {
        RX_DROPS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    match h.proto {
        ipv4::PROTO_ICMP => handle_icmp(&h, payload),
        ipv4::PROTO_UDP => handle_udp(&h, payload),
        ipv4::PROTO_TCP => handle_tcp(&h, payload),
        _ => {
            RX_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// ICMP：echo 请求直接回；echo 回复交给等待 ping 结果的人。
static PING_REPLY: crate::arch::sync::IrqSpinlock<PingWait> =
    crate::arch::sync::IrqSpinlock::new(PingWait { waiting: false, seq: 0, got: false });

struct PingWait {
    waiting: bool,
    seq: u16,
    got: bool,
}

fn handle_icmp(h: &ipv4::Header, payload: &[u8]) {
    match icmp::parse(payload) {
        Ok((icmp::TYPE_ECHO_REQUEST, id, seq, data)) => {
            // 回 Echo Reply：从请求原样构造（类型改 0，校验和重算）
            let mut r = [0u8; 1500];
            if let Ok(n) = icmp::reply_to(payload, &mut r[..payload.len() + 8]) {
                send_ip(h.src, ipv4::PROTO_ICMP, &r[..n]);
            }
            let _ = id;
        }
        Ok((icmp::TYPE_ECHO_REPLY, _id, seq, data)) => {
            let mut g = PING_REPLY.lock();
            if g.waiting && g.seq == seq {
                g.got = true;
                PING_DATA.lock().len = data.len();
            }
        }
        _ => {}
    }
}

static PING_DATA: crate::arch::sync::IrqSpinlock<PingData> =
    crate::arch::sync::IrqSpinlock::new(PingData { len: 0 });

struct PingData {
    len: usize,
}

/// 发 IP 包（自动处理 ARP/网关）。
pub fn send_ip(dst: ipv4::Ipv4, proto: u8, payload: &[u8]) -> Result<(), &'static str> {
    // 同网段直接发，否则发网关（NAT 下几乎全是网关）
    let hop = if dst.iter().zip(NETMASK.iter()).all(|(a, m)| a & m == OUR_IP[0] & m) {
        dst
    } else {
        GATEWAY
    };
    let mac = match resolve_wait(&hop) {
        Some(m) => m,
        None => return Err("ARP 解析失败"),
    };
    let mut pkt = [0u8; 1500];
    let n = ipv4::build(OUR_IP, dst, proto, payload, &mut pkt).map_err(|_| "IP 组包失败")?;
    send_eth(mac, eth::ETHERTYPE_IPV4, &pkt[..n])
}

/// UDP 收包：按端口投递。
fn handle_udp(h: &ipv4::Header, payload: &[u8]) {
    let (sport, dport, data) = match udp::parse(payload) {
        Ok(v) => v,
        Err(_) => {
            RX_DROPS.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let mut g = SOCKETS.lock();
    for slot in g.udp.iter_mut().flatten() {
        if slot.local_port == dport {
            slot.rxbuf.push((h.src, sport, Vec::from(data)));
            // 上限保护：不读就丢老的，别让缓冲无限涨
            if slot.rxbuf.len() > 32 {
                slot.rxbuf.remove(0);
            }
            return;
        }
    }
    RX_DROPS.fetch_add(1, Ordering::Relaxed);
}

/// UDP 发包。
pub fn udp_send(idx: usize, dst_ip: ipv4::Ipv4, dst_port: u16, payload: &[u8]) -> Result<(), &'static str> {
    let (local_port, udp_len) = {
        let g = SOCKETS.lock();
        let slot = g.udp.get(idx).and_then(|s| s.as_ref()).ok_or("socket 不存在")?;
        (slot.local_port, (knet::UDP_HDR + payload.len()) as u16)
    };
    let pseudo = udp::pseudo(OUR_IP, dst_ip, udp_len);
    let mut seg = [0u8; 1500];
    let n = udp::build(local_port, dst_port, &pseudo, payload, &mut seg).map_err(|_| "UDP 组包失败")?;
    send_ip(dst_ip, ipv4::PROTO_UDP, &seg[..n])
}

/// TCP 收包：按 (remote, local) 四元组匹配，被动打开时按端口匹配 LISTEN。
fn handle_tcp(h: &ipv4::Header, payload: &[u8]) {
    let (th, data) = match tcp::parse(payload) {
        Ok(v) => v,
        Err(_) => {
            RX_DROPS.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let mut g = SOCKETS.lock();

    // 先找精确匹配（已建立的连接）
    let idx = g.tcp.iter().position(|s| {
        if let Some(s) = s {
            s.key.local_port == th.dport
                && (s.key.remote_ip == h.src && s.key.remote_port == th.sport
                    || s.conn.state == tcp::State::SynSent
                    || s.conn.state == tcp::State::Listen && th.dport == s.key.local_port)
        } else {
            false
        }
    });

    let idx = match idx {
        Some(i) => i,
        None => {
            // 没人要它：回 RST（礼貌的拒绝——对端能立刻知道连接被拒）
            RX_DROPS.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let s = match g.tcp[idx].as_mut() {
        Some(s) => s,
        None => return,
    };

    match s.conn.state {
        tcp::State::Listen => {
            if th.flags & tcp::FLAG_SYN != 0 {
                // 被动打开：记住对端，回 SYN+ACK
                s.key.remote_ip = h.src;
                s.key.remote_port = th.sport;
                s.conn.remote_ip = h.src;
                s.conn.remote_port = th.sport;
                s.conn.rcv_nxt = th.seq.wrapping_add(1);
                let isn = s.conn.snd_nxt;
                s.conn.state = tcp::State::SynReceived;
                drop(g);
                let _ = tcp_send_segment(idx, tcp::FLAG_SYN | tcp::FLAG_ACK, isn, th.seq.wrapping_add(1), &[]);
                return;
            }
        }
        tcp::State::SynSent => {
            if th.flags & (tcp::FLAG_SYN | tcp::FLAG_ACK) == (tcp::FLAG_SYN | tcp::FLAG_ACK) {
                s.conn.rcv_nxt = th.seq.wrapping_add(1);
                s.conn.snd_una = th.ack;
                s.conn.state = tcp::State::Established;
                s.established = true;
                let (snd, rcv) = (s.conn.snd_nxt, s.conn.rcv_nxt);
                drop(g);
                let _ = tcp_send_segment(idx, tcp::FLAG_ACK, snd, rcv, &[]);
                return;
            }
        }
        tcp::State::SynReceived => {
            if th.flags & tcp::FLAG_ACK != 0 {
                s.conn.state = tcp::State::Established;
                s.established = true;
                s.conn.snd_una = th.ack;
                return;
            }
        }
        tcp::State::Established => {
            // 数据段：按序就收，乱序就丢（不做重组）
            if th.seq == s.conn.rcv_nxt {
                if !data.is_empty() {
                    s.rxbuf.extend_from_slice(data);
                    if s.rxbuf.len() > 64 * 1024 {
                        let excess = s.rxbuf.len() - 64 * 1024;
                        s.rxbuf.drain(..excess);
                    }
                    s.conn.rcv_nxt = s.conn.rcv_nxt.wrapping_add(data.len() as u32);
                }
                if th.flags & tcp::FLAG_FIN != 0 {
                    s.conn.rcv_nxt = s.conn.rcv_nxt.wrapping_add(1);
                    s.eof = true;
                    let rcv = s.conn.rcv_nxt;
                    let snd = s.conn.snd_nxt;
                    drop(g);
                    let _ = tcp_send_segment(idx, tcp::FLAG_ACK, snd, rcv, &[]);
                    return;
                }
                if th.flags & tcp::FLAG_ACK != 0 {
                    s.conn.snd_una = th.ack;
                }
                // 有数据就 ACK 一次
                if !data.is_empty() {
                    let rcv = s.conn.rcv_nxt;
                    let snd = s.conn.snd_nxt;
                    drop(g);
                    let _ = tcp_send_segment(idx, tcp::FLAG_ACK, snd, rcv, &[]);
                }
                return;
            }
            if th.flags & tcp::FLAG_RST != 0 {
                s.conn.state = tcp::State::Closed;
                s.eof = true;
                return;
            }
            // 乱序/重复：回当前 ACK（对端会重传正确的序号）
            let rcv = s.conn.rcv_nxt;
            let snd = s.conn.snd_nxt;
            drop(g);
            let _ = tcp_send_segment(idx, tcp::FLAG_ACK, snd, rcv, &[]);
            return;
        }
        tcp::State::FinWait => {
            if th.flags & tcp::FLAG_ACK != 0 {
                s.conn.state = tcp::State::Closed;
                s.eof = true;
                return;
            }
        }
        tcp::State::Closed => {}
    }
}

/// 发一个 TCP 段（绕过 sock 状态锁，供 handle_tcp 内部用）。
fn tcp_send_segment(idx: usize, flags: u8, seq: u32, ack: u32, payload: &[u8]) -> Result<(), &'static str> {
    let (conn_copy, remote_ip) = {
        let g = SOCKETS.lock();
        let s = g.tcp.get(idx).and_then(|x| x.as_ref()).ok_or("连接不存在")?;
        (s.conn, s.key.remote_ip)
    };
    let pseudo = udp::pseudo(OUR_IP, remote_ip, (knet::TCP_HDR_MIN + payload.len()) as u16);
    let mut seg = [0u8; 1500];
    let n = tcp::build(&conn_copy, flags, seq, ack, payload, &pseudo, &mut seg).map_err(|_| "TCP 组包失败")?;
    send_ip(remote_ip, ipv4::PROTO_TCP, &seg[..n])
}

/// 主动打开：发 SYN（connect 用）。
pub fn tcp_connect(idx: usize) -> Result<(), &'static str> {
    let (conn_copy, remote_ip) = {
        let g = SOCKETS.lock();
        let s = g.tcp.get(idx).and_then(|x| x.as_ref()).ok_or("连接不存在")?;
        (s.conn, s.key.remote_ip)
    };
    let pseudo = udp::pseudo(OUR_IP, remote_ip, knet::TCP_HDR_MIN as u16);
    let mut seg = [0u8; 64];
    let n = tcp::build(&conn_copy, tcp::FLAG_SYN, conn_copy.snd_nxt, 0, &[], &pseudo, &mut seg)
        .map_err(|_| "SYN 组包失败")?;
    send_ip(remote_ip, ipv4::PROTO_TCP, &seg[..n])
}

/// 发数据（连接必须已建立）。`snd_nxt` 推进由这里做。
pub fn tcp_send(idx: usize, payload: &[u8]) -> Result<usize, &'static str> {
    let (conn_copy, remote_ip, new_nxt) = {
        let g = SOCKETS.lock();
        let s = g.tcp.get(idx).and_then(|x| x.as_ref()).ok_or("连接不存在")?;
        if s.conn.state != tcp::State::Established {
            return Err("连接未建立");
        }
        let nxt = s.conn.snd_nxt.wrapping_add(payload.len() as u32);
        (s.conn, s.key.remote_ip, nxt)
    };
    let pseudo = udp::pseudo(OUR_IP, remote_ip, (knet::TCP_HDR_MIN + payload.len()) as u16);
    let mut seg = [0u8; 1500];
    let n = tcp::build(&conn_copy, tcp::FLAG_ACK, conn_copy.snd_nxt, conn_copy.rcv_nxt, payload, &pseudo, &mut seg)
        .map_err(|_| "组包失败")?;
    send_ip(remote_ip, ipv4::PROTO_TCP, &seg[..n])?;
    let mut g = SOCKETS.lock();
    if let Some(s) = g.tcp.get_mut(idx).and_then(|x| x.as_mut()) {
        s.conn.snd_nxt = new_nxt;
    }
    Ok(payload.len())
}

/// 关闭：发 FIN。
pub fn tcp_close(idx: usize) -> Result<(), &'static str> {
    let (conn_copy, remote_ip) = {
        let g = SOCKETS.lock();
        let s = g.tcp.get(idx).and_then(|x| x.as_ref()).ok_or("连接不存在")?;
        (s.conn, s.key.remote_ip)
    };
    let pseudo = udp::pseudo(OUR_IP, remote_ip, knet::TCP_HDR_MIN as u16);
    let mut seg = [0u8; 64];
    let n = tcp::build(&conn_copy, tcp::FLAG_FIN | tcp::FLAG_ACK, conn_copy.snd_nxt, conn_copy.rcv_nxt, &[], &pseudo, &mut seg)
        .map_err(|_| "FIN 组包失败")?;
    send_ip(remote_ip, ipv4::PROTO_TCP, &seg[..n])?;
    let mut g = SOCKETS.lock();
    if let Some(s) = g.tcp.get_mut(idx).and_then(|x| x.as_mut()) {
        s.conn.state = tcp::State::FinWait;
        s.conn.snd_nxt = s.conn.snd_nxt.wrapping_add(1);
    }
    Ok(())
}

/// 被动打开：进入 LISTEN。
pub fn tcp_listen(idx: usize) -> Result<(), &'static str> {
    let mut g = SOCKETS.lock();
    let s = g.tcp.get_mut(idx).and_then(|x| x.as_mut()).ok_or("连接不存在")?;
    s.conn.state = tcp::State::Listen;
    Ok(())
}

/// 从 TCP 连接读数据（内核侧）。
pub fn tcp_read(idx: usize, out: &mut [u8]) -> usize {
    let mut g = SOCKETS.lock();
    match g.tcp.get_mut(idx).and_then(|x| x.as_mut()) {
        Some(s) => {
            let n = s.rxbuf.len().min(out.len());
            out[..n].copy_from_slice(&s.rxbuf[..n]);
            s.rxbuf.drain(..n);
            n
        }
        None => 0,
    }
}

/// TCP 连接的状态（syscall 层查询用）。
pub fn tcp_state(idx: usize) -> Option<(bool, bool)> {
    let g = SOCKETS.lock();
    g.tcp.get(idx).and_then(|x| x.as_ref()).map(|s| (s.established, s.eof))
}

/// UDP 收包（内核侧）。
pub fn udp_recv(idx: usize, out: &mut [u8]) -> Option<([u8; 4], u16, usize)> {
    let mut g = SOCKETS.lock();
    let s = g.udp.get_mut(idx).and_then(|x| x.as_mut())?;
    let (ip, port, data) = s.rxbuf.first()?.clone();
    let n = data.len().min(out.len());
    out[..n].copy_from_slice(&data[..n]);
    Some((ip, port, n))
}

/// ICMP ping（自检用）：等回复，返回 `(是否收到, 回包字节数)`。
pub fn ping(seq: u16) -> (bool, usize) {
    {
        let mut g = PING_REPLY.lock();
        g.waiting = true;
        g.seq = seq;
        g.got = false;
        PING_DATA.lock().len = 0;
    }
    let mut icmp_pkt = [0u8; 40];
    let n = icmp::build_echo(icmp::TYPE_ECHO_REQUEST, 0x4C4D, seq, b"lumi", &mut icmp_pkt)
        .map_err(|_| "ICMP 组包失败")
        .ok();
    let mut sent = false;
    if let Some(n) = n {
        sent = send_ip(GATEWAY, ipv4::PROTO_ICMP, &icmp_pkt[..n]).is_ok();
    }
    if !sent {
        let mut g = PING_REPLY.lock();
        g.waiting = false;
        return (false, 0);
    }
    // 等回复：最多 200 ms（20 个 10ms tick）
    let start = crate::intr::timer::ticks();
    loop {
        crate::dev::nic::poll(|frame| handle_frame(frame));
        let g = PING_REPLY.lock();
        if g.got {
            let len = PING_DATA.lock().len;
            drop(g);
            PING_REPLY.lock().waiting = false;
            return (true, len);
        }
        drop(g);
        if crate::intr::timer::ticks().wrapping_sub(start) > 20 {
            break;
        }
        // 让出 CPU，等下一轮 tick（tick 里也会 poll RX）
        crate::intr::enable();
        crate::intr::frame::halt_once();
        crate::intr::disable();
    }
    PING_REPLY.lock().waiting = false;
    (false, 0)
}

/// 轮询 RX（tick / syscall 时调用）。
pub fn poll_rx() {
    if crate::dev::nic::up() {
        crate::dev::nic::poll(|frame| handle_frame(frame));
    }
}


/// UDP socket 绑定端口（bind 用）。
pub fn udp_rebind(idx: usize, port: u16) {
    let mut g = SOCKETS.lock();
    if let Some(s) = g.udp.get_mut(idx).and_then(|x| x.as_mut()) {
        s.local_port = port;
    }
}

/// TCP socket 绑定本地端口（bind 用）。
pub fn tcp_rebind(idx: usize, port: u16) {
    let mut g = SOCKETS.lock();
    if let Some(s) = g.tcp.get_mut(idx).and_then(|x| x.as_mut()) {
        s.conn.local_port = port;
        s.key.local_port = port;
    }
}

/// connect：改目标并发 SYN，然后等握手（阻塞，最多 ~600 ms）。
pub fn tcp_connect_to(idx: usize, remote_ip: ipv4::Ipv4, remote_port: u16) -> Result<(), &'static str> {
    {
        let mut g = SOCKETS.lock();
        let s = g.tcp.get_mut(idx).and_then(|x| x.as_mut()).ok_or("socket 不存在")?;
        s.key.remote_ip = remote_ip;
        s.key.remote_port = remote_port;
        s.conn.remote_ip = remote_ip;
        s.conn.remote_port = remote_port;
    }
    tcp_connect(idx)?;
    // 等握手：poll RX 驱动回包
    for _ in 0..600 {
        {
            let mut g = SOCKETS.lock();
            if let Some(s) = g.tcp.get_mut(idx).and_then(|x| x.as_mut()) {
                if s.established {
                    return Ok(());
                }
            }
        }
        crate::intr::enable();
        crate::intr::frame::halt_once();
        crate::intr::disable();
        crate::dev::nic::poll(|frame| handle_frame(frame));
    }
    Err("TCP 握手超时")
}

/// 自检用的原始地址输出。
pub fn config_str() -> ([u8; 4], [u8; 4]) {
    (OUR_IP, GATEWAY)
}
