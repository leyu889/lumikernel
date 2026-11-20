//! virtio-net 驱动（legacy 0.9.5 接口）。
//!
//! ## 为什么用 virtio 而不是继续调 e1000
//!
//! 82540EM 的 PHY 是模拟出来的：链路状态（`STATUS.LU`）依赖"厂商固件
//! 行为"，VBox 的模拟连 EECD 的 Auto-Read-Done 位都不实现——我们的
//! 初始化序列看起来完全正确（MAC 读对、CTRL.SLU 置上、RCTL 写成功），
//! 却始终 `LU=0`，TX 描述符永远不被消费。
//!
//! virtio 是**半虚拟化约定**：没有 PHY、没有协商、没有厂商固件时序。
//! 驱动与设备通过一小段共享内存 + 一个门铃寄存器通信，语义在规范里
//! 写死。同一份代码在 QEMU/KVM/VBox 上都能跑。
//!
//! ## 队列模型（legacy）
//!
//! 一个 virtqueue = 三段连续内存：
//! - **descriptor table**：16 字节/项，`addr/len/flags/next`
//! - **available ring**：驱动告诉设备"这些描述符可用"
//! - **used ring**：设备告诉驱动"这些处理完了"
//!
//! 三段必须物理连续，`QUEUE_PFN` 写起始页号。
//!
//! ## 三个环的偏移**由设备的队列长度决定**（这里踩过大坑）
//!
//! `desc` 在偏移 0，`avail` 在 `16 * QueueNum`，`used` 在
//! `align_up(16*QueueNum + 6 + 2*QueueNum, 4096)`。
//!
//! 之前按固定的 64 项布局，而设备的队列长度是它自己定的（VBox 给 256）。
//! 于是设备按 256 算，去偏移 4096 找 avail 环——那正好是我们的 used 环：
//! 设备把 used 当 avail 读，看到 `idx=0`，**一条请求都不取**。
//! 现场表现是 `tx` 计数涨、`used.idx` 恒为 0、`rx` 恒为 0、ping 不通，
//! 而所有寄存器读回来都"正常"——最难查的一类。
//!
//! 配套的坑：**`QueueNum` 只在 `QueuePFN == 0` 时可读**。先写 PFN 再读，
//! 读回来是 0，于是"我们以为设备队列是 0 项"这个错误会被自己掩盖掉。

use crate::arch::sync::IrqSpinlock;
use crate::mm::{frames, paging};

/// virtio-net 的 legacy PCI ID（transitional device）。
pub const VENDOR_VIRTIO: u16 = 0x1AF4;
pub const DEVICE_NET: u16 = 0x1000;

// legacy virtio PCI 寄存器（相对 BAR0 的 IO 端口）。
const R_HOST_FEATURES: u16 = 0x00;
const R_GUEST_FEATURES: u16 = 0x04;
const R_QUEUE_PFN: u16 = 0x08;
const R_QUEUE_NUM: u16 = 0x0C;
const R_QUEUE_SEL: u16 = 0x0E;
const R_QUEUE_NOTIFY: u16 = 0x10;
const R_STATUS: u16 = 0x12;
const R_ISR: u16 = 0x13;
const R_MAC: u16 = 0x14;

const STATUS_ACK: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_FAILED: u8 = 0x80;

/// 我们自己最多用多少个描述符。
///
/// 设备队列一般给 256 项，我们只用前 64 个——够跑一个回环 ping，
/// 也省下 3/4 的 RX 缓冲。**布局仍然按设备给的 `QueueNum` 算**，
/// 这一点不能省。
const DESC_MAX: usize = 64;
/// RX 缓冲大小：每帧 2 KiB（MTU 1500 + 头）。
const RX_BUF: usize = 2048;

/// 每个 virtio-net 缓冲前面必须有的 `virtio_net_hdr`（10 字节）。
///
/// legacy virtio-net 的约定：**缓冲 = 头 + 数据**，不是裸以太网帧。
/// 头里放 offload 请求（flags）、GSO 参数和校验和偏移。我们不使用任何
/// offload，所以发的时候整块填 0；收的时候跳过这 10 字节，否则以太网
/// 头部会整体错位 10 字节——ARP 的目的 MAC、类型字段全对不上，
/// 表现是"包收到了但协议栈一帧都不认识"。
const VNET_HDR: usize = 10;
/// 两个队列：0 = RX，1 = TX。
const Q_RX: u16 = 0;
const Q_TX: u16 = 1;

#[repr(C, align(16))]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

const VIRTQ_DESC_F_WRITE: u16 = 2; // 设备可写（RX 用）

struct Dev {
    io_base: u16,
    mac: [u8; 6],
}

static DEV: IrqSpinlock<Option<Dev>> = IrqSpinlock::new(None);

/// 队列运行状态。
struct QState {
    /// 队列内存的**物理**地址（写进 QUEUE_PFN 的那个）。
    mem_phys: u64,
    /// 设备上报的队列长度，布局偏移全部由它决定。
    num: u16,
    avail_idx: u16,
    last_used: u16,
}

static RXQ: IrqSpinlock<Option<QState>> = IrqSpinlock::new(None);
static TXQ: IrqSpinlock<Option<QState>> = IrqSpinlock::new(None);

/// RX 与 TX 各自独立的缓冲池。
///
/// 共用一个池是错的：TX 要往缓冲里写以太网帧，而 RX 的同一页还挂在
/// 设备上等收包——两边互相覆盖，收到的帧和发出的帧都变成垃圾。
static RX_BUF_PHYS: IrqSpinlock<[u64; DESC_MAX]> = IrqSpinlock::new([0; DESC_MAX]);
static TX_BUF_PHYS: IrqSpinlock<[u64; DESC_MAX]> = IrqSpinlock::new([0; DESC_MAX]);

fn outb(port: u16, v: u8) {
    // SAFETY: port 是 PCI BAR0 给我们的 IO 窗口。
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags));
    }
}

fn inb(port: u16) -> u8 {
    let v: u8;
    // SAFETY: 同上。
    unsafe {
        core::arch::asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    v
}

fn outw(port: u16, v: u16) {
    // SAFETY: 同上（IO 端口）。
    unsafe {
        core::arch::asm!("out dx, ax", in("dx") port, in("ax") v, options(nomem, nostack, preserves_flags));
    }
}

/// 16 位端口读。`QUEUE_NUM` 就是一个 16 位寄存器，**必须按 2 字节读**。
///
/// 用 32 位 `inl` 读 0x0C 会同时覆盖 0x0E 的 `QUEUE_SEL`——跨了寄存器边界，
/// 设备（VBox 与 QEMU 都是）直接返回 0。于是"设备队列长度 = 0"，
/// 再往下所有布局计算全是错的，而现场只看到一句"队列长度非法"。
fn inw(port: u16) -> u16 {
    let v: u16;
    // SAFETY: port 是 PCI BAR0 的 IO 窗口。
    unsafe {
        core::arch::asm!("in ax, dx", out("ax") v, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    v
}

fn outl(port: u16, v: u32) {
    // SAFETY: 同上。
    unsafe {
        core::arch::asm!("out dx, eax", in("dx") port, in("eax") v, options(nomem, nostack, preserves_flags));
    }
}

fn inl(port: u16) -> u32 {
    let v: u32;
    // SAFETY: 同上。
    unsafe {
        core::arch::asm!("in eax, dx", out("eax") v, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    v
}

/* ---------------- 队列内存布局 ---------------- */

#[inline]
fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

/// avail 环的字节偏移：描述符表占 `16 * num`。
#[inline]
fn avail_off(num: u16) -> usize {
    16 * num as usize
}

/// used 环的字节偏移：avail 之后按 4 KiB 对齐（规范要求）。
#[inline]
fn used_off(num: u16) -> usize {
    // avail = flags(2) + idx(2) + ring[2*num] + used_event(2) = 6 + 2*num
    align_up(avail_off(num) + 6 + 2 * num as usize, 4096)
}

/// 整块队列内存的字节数。
#[inline]
fn queue_bytes(num: u16) -> usize {
    // used = flags(2) + idx(2) + ring[8*num]
    used_off(num) + 4 + 8 * num as usize
}

#[inline]
unsafe fn desc_at(base: *mut u8, i: usize) -> *mut Desc {
    base.add(i * 16) as *mut Desc
}

#[inline]
unsafe fn avail_idx_slot(base: *mut u8, num: u16) -> *mut u16 {
    base.add(avail_off(num) + 2) as *mut u16
}

#[inline]
unsafe fn avail_ring_slot(base: *mut u8, num: u16, slot: usize) -> *mut u16 {
    base.add(avail_off(num) + 4 + slot * 2) as *mut u16
}

/// 设备写的 `used.idx`。用 volatile 读：这一页是设备在改，
/// 编译器不知道有别人写过它。
#[inline]
unsafe fn used_idx_of(base: *mut u8, num: u16) -> u16 {
    core::ptr::read_volatile(base.add(used_off(num) + 2) as *const u16)
}

#[inline]
unsafe fn used_id_of(base: *mut u8, num: u16, slot: usize) -> u32 {
    core::ptr::read_volatile(base.add(used_off(num) + 4 + slot * 8) as *const u32)
}

#[inline]
unsafe fn used_len_of(base: *mut u8, num: u16, slot: usize) -> u32 {
    core::ptr::read_volatile(base.add(used_off(num) + 8 + slot * 8) as *const u32)
}

/* ---------------- 初始化 ---------------- */

/// 初始化。返回 `Err` 时网络层会跳过自检（无网卡环境不算失败）。
pub fn init() -> Result<(), &'static str> {
    let mut devs = [crate::dev::pci::Device::EMPTY; 16];
    let rep = crate::dev::pci::enumerate(&mut devs);
    let dev = (0..rep.devices)
        .map(|i| &devs[i])
        .find(|d| d.vendor == VENDOR_VIRTIO && (d.device == DEVICE_NET || d.device == 0x1041))
        .ok_or("PCI 上没有 virtio-net（检查 --nictype1 virtio）")?;

    // legacy virtio：BAR0 是 IO 窗口。同时使能 IO 空间与总线主控。
    crate::dev::pci::set_command_bit(dev, 0, true); // IO space
    crate::dev::pci::set_command_bit(dev, 2, true); // MEM
    crate::dev::pci::set_command_bit(dev, 4, true); // BUS_MASTER

    let (bar0, is_io) = crate::dev::pci::bar(dev, 0).ok_or("virtio 没有 BAR0")?;
    if !is_io {
        return Err("virtio BAR0 不是 IO（modern 模式，本驱动只支持 legacy）");
    }
    let io = bar0 as u16;

    // 复位：写 STATUS=0，等它归零（复位会把所有 QUEUE_PFN 清 0）
    outb(io + R_STATUS, 0);
    let mut spins = 0;
    while inb(io + R_STATUS) != 0 && spins < 100_000 {
        spins += 1;
        core::hint::spin_loop();
    }
    if inb(io + R_STATUS) != 0 {
        return Err("virtio 复位超时");
    }

    // ACK + DRIVER
    outb(io + R_STATUS, STATUS_ACK);
    outb(io + R_STATUS, STATUS_ACK | STATUS_DRIVER);

    // 特性协商：只保留 MAC（bit5），其它一律不要——
    // 尤其不能要 MRG_RXBUF，那会改变 RX 缓冲的语义（我们按"一帧一缓冲"实现）。
    let host_feat = inl(io + R_HOST_FEATURES);
    let want = host_feat & (1 << 5);
    outl(io + R_GUEST_FEATURES, want);

    outb(io + R_STATUS, STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK);
    if inb(io + R_STATUS) & STATUS_FEATURES_OK == 0 {
        outb(io + R_STATUS, STATUS_FAILED);
        return Err("virtio 拒绝特性协商（队列 64 项 + MAC 都不被接受）");
    }

    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = inb(io + R_MAC + i as u16);
    }

    // ---- 队列 0（RX）：先读长度 → 按长度分配 → 填描述符 → 最后写 PFN ----
    let (rx_mem, rx_num) = queue_alloc(io, Q_RX)?;
    let rx_n = core::cmp::min(rx_num as usize, DESC_MAX);

    // RX 缓冲：每个描述符一页，提前全部挂上去（设备随时可能收包）
    {
        let mut bufs = RX_BUF_PHYS.lock();
        for b in bufs.iter_mut().take(rx_n) {
            if *b == 0 {
                *b = frames::alloc_frame().ok_or("virtio RX 缓冲分配失败")?;
                // SAFETY: 刚分配的独占页。
                unsafe {
                    core::ptr::write_bytes(paging::phys_to_virt(*b) as *mut u8, 0, 4096);
                }
            }
        }
    }
    // SAFETY: 队列内存独占且已清零。
    unsafe {
        let base = paging::phys_to_virt(rx_mem) as *mut u8;
        let bufs = RX_BUF_PHYS.lock();
        for i in 0..rx_n {
            let d = desc_at(base, i);
            (*d).addr = bufs[i];
            (*d).len = RX_BUF as u32;
            (*d).flags = VIRTQ_DESC_F_WRITE; // 设备往里写
            (*d).next = 0;
            core::ptr::write_volatile(avail_ring_slot(base, rx_num, i), i as u16);
        }
        core::ptr::write_volatile(avail_idx_slot(base, rx_num), rx_n as u16);
    }
    queue_activate(io, Q_RX, rx_mem);

    // ---- 队列 1（TX）----
    let (tx_mem, tx_num) = queue_alloc(io, Q_TX)?;
    let tx_n = core::cmp::min(tx_num as usize, DESC_MAX);
    {
        let mut bufs = TX_BUF_PHYS.lock();
        for b in bufs.iter_mut().take(tx_n) {
            if *b == 0 {
                *b = frames::alloc_frame().ok_or("virtio TX 缓冲分配失败")?;
                // SAFETY: 刚分配的独占页。
                unsafe {
                    core::ptr::write_bytes(paging::phys_to_virt(*b) as *mut u8, 0, 4096);
                }
            }
        }
    }
    queue_activate(io, Q_TX, tx_mem);

    *RXQ.lock() = Some(QState {
        mem_phys: rx_mem,
        num: rx_num,
        avail_idx: rx_n as u16,
        last_used: 0,
    });
    *TXQ.lock() = Some(QState {
        mem_phys: tx_mem,
        num: tx_num,
        avail_idx: 0,
        last_used: 0,
    });
    *DEV.lock() = Some(Dev { io_base: io, mac });

    // 驱动就绪 —— 这一位之前设备不会碰任何队列
    outb(
        io + R_STATUS,
        STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
    );
    // 现在告诉设备 RX 队列有新缓冲
    outw(io + R_QUEUE_NOTIFY, Q_RX);

    let st = inb(io + R_STATUS);
    if st & STATUS_DRIVER_OK == 0 {
        return Err("virtio 未进入 DRIVER_OK");
    }
    kserialln!(
        "[virtio] 就绪：io={:#x} mac={:x}:{:x}:{:x}:{:x}:{:x}:{:x} status={:#x} \
         rx(num={} avail={}) tx(num={}) used_off(rx)={} 页数={}",
        io, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], st,
        rx_num, rx_n, tx_num, used_off(rx_num),
        (queue_bytes(rx_num) + 4095) / 4096 + (queue_bytes(tx_num) + 4095) / 4096
    );
    Ok(())
}

/// 读取队列长度并分配内存。**不写 PFN**——PFN 一写，`QueueNum` 就读不到了。
fn queue_alloc(io: u16, sel: u16) -> Result<(u64, u16), &'static str> {
    outw(io + R_QUEUE_SEL, sel);
    // **必须在写 PFN 之前读**：legacy 规定 QueueNum 只在 QueuePFN==0 时可读。
    // 16 位读（见 `inw` 的注释：32 位读会跨进 QUEUE_SEL，设备返回 0）
    let num = inw(io + R_QUEUE_NUM);
    if num < 2 || !num.is_power_of_two() {
        return Err("virtio 队列长度非法（不是 2 的幂）");
    }
    let bytes = queue_bytes(num);
    let pages = (bytes + 4095) / 4096;
    let phys = frames::alloc_frames(pages).ok_or("virtio 队列页分配失败")?;
    // SAFETY: 刚分配的独占连续页，整块清零（三个环的初始状态必须全是 0）。
    unsafe {
        core::ptr::write_bytes(paging::phys_to_virt(phys) as *mut u8, 0, pages * 4096);
    }
    Ok((phys, num))
}

/// 把队列内存的页号交给设备（此后再读 `QueueNum` 会得到 0）。
fn queue_activate(io: u16, sel: u16, phys: u64) {
    outw(io + R_QUEUE_SEL, sel);
    outl(io + R_QUEUE_PFN, (phys / 4096) as u32);
}

/* ---------------- 收发 ---------------- */

pub fn mac() -> [u8; 6] {
    match DEV.lock().as_ref() {
        Some(d) => d.mac,
        None => [0; 6],
    }
}

pub fn up() -> bool {
    DEV.lock().is_some()
}

/// 发一帧。设备处理很快，环满才报错（NAT 下不会积压）。
pub fn send(frame: &[u8]) -> Result<(), &'static str> {
    if frame.is_empty() || frame.len() > RX_BUF {
        return Err("帧长度非法");
    }
    let dev = DEV.lock();
    let d = dev.as_ref().ok_or("virtio 未初始化")?;
    let bufs = TX_BUF_PHYS.lock();
    let mut txq = TXQ.lock();
    let qs = txq.as_mut().ok_or("TX 队列未就绪")?;
    let base = paging::phys_to_virt(qs.mem_phys) as *mut u8;
    let n = core::cmp::min(qs.num as usize, DESC_MAX);

    // SAFETY: 队列与缓冲都由本驱动独占；索引已取模到 n 以内。
    unsafe {
        // 未完成数 = avail.idx - used.idx（u16 回绕是 virtio 的常规做法）
        let used = used_idx_of(base, qs.num);
        if qs.avail_idx.wrapping_sub(used) as usize >= n {
            return Err("TX 队列满");
        }
        let slot = qs.avail_idx as usize % n;
        let buf = paging::phys_to_virt(bufs[slot]) as *mut u8;
        // 前 10 字节是 virtio_net_hdr：全 0 = 不需要任何 offload，
        // 校验和由我们自己算好了（knet 里算的）。
        core::ptr::write_bytes(buf, 0, VNET_HDR);
        core::ptr::copy_nonoverlapping(frame.as_ptr(), buf.add(VNET_HDR), frame.len());
        let dsc = desc_at(base, slot);
        (*dsc).addr = bufs[slot];
        (*dsc).len = (VNET_HDR + frame.len()) as u32;
        (*dsc).flags = 0; // 设备只读
        (*dsc).next = 0;

        // 环槽位按**设备队列长度**取模，描述符号按我们自己的 n 取模——两者不同
        let rslot = qs.avail_idx as usize % qs.num as usize;
        core::ptr::write_volatile(avail_ring_slot(base, qs.num, rslot), slot as u16);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        qs.avail_idx = qs.avail_idx.wrapping_add(1);
        // avail.idx 必须最后写：设备看到 idx 更新就会立刻去读上面的环槽
        core::ptr::write_volatile(avail_idx_slot(base, qs.num), qs.avail_idx);
        outw(d.io_base + R_QUEUE_NOTIFY, Q_TX);
    }
    Ok(())
}

/// 轮询 RX：把收到的帧逐个交给 `f`，返回处理了几帧。
pub fn poll(mut f: impl FnMut(&[u8])) -> usize {
    if DEV.lock().is_none() {
        return 0;
    }
    let bufs = RX_BUF_PHYS.lock();
    let mut rxq = RXQ.lock();
    let qs = match rxq.as_mut() {
        Some(x) => x,
        None => return 0,
    };
    let base = paging::phys_to_virt(qs.mem_phys) as *mut u8;
    let n = core::cmp::min(qs.num as usize, DESC_MAX);
    let mut got = 0;

    // SAFETY: 队列内存独占；used 环由设备写、我们只读。
    unsafe {
        let used = used_idx_of(base, qs.num);
        while qs.last_used != used && got < qs.num as usize {
            let rslot = qs.last_used as usize % qs.num as usize;
            let id = used_id_of(base, qs.num, rslot) as usize;
            let len = used_len_of(base, qs.num, rslot) as usize;
            // 去掉开头的 virtio_net_hdr，只把以太网帧交给协议栈
            if id < n && len > VNET_HDR {
                let slice = core::slice::from_raw_parts(
                    (paging::phys_to_virt(bufs[id]) as *const u8).add(VNET_HDR),
                    (len - VNET_HDR).min(RX_BUF - VNET_HDR),
                );
                f(slice);
                got += 1;
            }
            qs.last_used = qs.last_used.wrapping_add(1);
            // 把描述符重新放回 avail 环（设备才能继续往里写）。
            // **len==0 也要放**：否则这个描述符就永久丢失了。
            if id < n {
                let aslot = qs.avail_idx as usize % qs.num as usize;
                core::ptr::write_volatile(avail_ring_slot(base, qs.num, aslot), id as u16);
                qs.avail_idx = qs.avail_idx.wrapping_add(1);
                core::ptr::write_volatile(avail_idx_slot(base, qs.num), qs.avail_idx);
            }
        }
        if qs.last_used != used_idx_of(base, qs.num) {
            if let Some(d) = DEV.lock().as_ref() {
                outw(d.io_base + R_QUEUE_NOTIFY, Q_RX);
            }
        }
    }
    got
}

/// 读一次 ISR（清中断/状态），自检用来确认设备活着。
pub fn isr() -> u8 {
    match DEV.lock().as_ref() {
        Some(d) => inb(d.io_base + R_ISR),
        None => 0,
    }
}
