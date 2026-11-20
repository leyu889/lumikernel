//! virtio-net 驱动（legacy 0.9.5 接口）。
//!
//! ## 为什么改用 virtio 而不是继续调 e1000
//!
//! 82540EM 的 PHY 是模拟出来的：链路状态（`STATUS.LU`）依赖"厂商固件
//! 行为"，VBox 的模拟连 EECD 的 Auto-Read-Done 位都不实现——我们的
//! 初始化序列看起来完全正确（MAC 读对、CTRL.SLU 置上、RCTL 写成功），
//! 却始终 `LU=0`，TX 描述符永远不被消费。
//!
//! virtio 是**半虚拟化约定**：没有 PHY、没有协商、没有厂商固件时序。
//! 驱动与设备通过一小段共享内存 + 一个门铃寄存器通信，语义在规范里
//! 写死。同一份代码在 QEMU/KVM/VBox 上都能跑——这也顺带完成了
//! "摆脱只支持 VBox AHCI"的目标。
//!
//! ## 队列模型（legacy）
//!
//! 一个 virtqueue = 三块连续内存：
//! - **descriptor table**：16 字节/项，`addr/len/flags/next`
//! - **available ring**：驱动告诉设备"这些描述符可用"
//! - **used ring**：设备告诉驱动"这些处理完了"
//!
//! 三个环必须物理连续，`QUEUE_PFN` 写的是起始页号。

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

/// 队列项数（必须是 2 的幂）。
const QSIZE: usize = 64;
/// RX 缓冲大小：每帧 2 KiB（MTU 1500 + 头）。
const RX_BUF: usize = 2048;
/// 我们用的两个队列：0 = RX，1 = TX。
const Q_RX: u16 = 0;
const Q_TX: u16 = 1;

#[repr(C, align(16))]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2; // 设备可写（RX 用）

/// 一个 queue 的共享内存布局。
#[repr(C, align(4096))]
struct QueueMem {
    desc: [Desc; QSIZE],
    /// avail: flags(u16) + idx(u16) + ring[QSIZE] + used_event(u16)
    avail: [u16; 2 + QSIZE + 1],
    /// 对齐到 4 KiB 边界是 legacy 的要求（QUEUE_PFN 以页为单位）
    pad: [u8; 4096 - ((QSIZE * 16 + (2 + QSIZE + 1) * 2) % 4096)],
    used: [u32; 2 + QSIZE * 2],
}

struct Dev {
    io_base: u16,
    mac: [u8; 6],
}

static DEV: IrqSpinlock<Option<Dev>> = IrqSpinlock::new(None);

/// 队列运行状态（物理地址 + 游标）。
struct QState {
    mem_phys: u64,
    avail_idx: u16,
    last_used: u16,
}

static RXQ: IrqSpinlock<Option<QState>> = IrqSpinlock::new(None);
static TXQ: IrqSpinlock<Option<QState>> = IrqSpinlock::new(None);

/// 每帧的物理缓冲（RX 需要，TX 复用同一个池）。
static BUF_PHYS: IrqSpinlock<[u64; QSIZE]> = IrqSpinlock::new([0; QSIZE]);

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
        // modern virtio 的 BAR0 是 MMIO（capability 列表）——本实现只覆盖
        // legacy。明确报出来，而不是"初始化成功但收发全无"。
        return Err("virtio BAR0 不是 IO（modern 模式，本驱动只支持 legacy）");
    }
    let io = bar0 as u16;

    // 复位设备：写 STATUS=0，等它归零
    outb(io + R_STATUS, 0);
    let mut spins = 0;
    while inb(io + R_STATUS) != 0 && spins < 100_000 {
        spins += 1;
        core::hint::spin_loop();
    }

    // ACK + DRIVER
    outb(io + R_STATUS, STATUS_ACK);
    outb(io + R_STATUS, STATUS_ACK | STATUS_DRIVER);

    // 特性协商：**必须清掉 VIRTIO_NET_F_MRG_RXBUF / MRG_CTRL 等我们没实现的**
    let host_feat = inl(io + R_HOST_FEATURES);
    // 只保留 MAC（bit5）——其它一律不要，避免设备按我们没实现的方式发包
    let want = host_feat & (1 << 5);
    outl(io + R_GUEST_FEATURES, want);

    outb(io + R_STATUS, STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK);
    if inb(io + R_STATUS) & STATUS_FEATURES_OK == 0 {
        outb(io + R_STATUS, STATUS_FAILED);
        return Err("virtio 拒绝特性协商");
    }

    // MAC 在 legacy 配置空间 R_MAC（6 字节）
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = inb(io + R_MAC + i as u16);
    }

    // 两个队列
    let rx_mem = alloc_queue(Q_RX, io)?;
    let tx_mem = alloc_queue(Q_TX, io)?;

    // RX：把所有缓冲提前挂上去（设备随时可能收包）
    {
        let bufs = BUF_PHYS.lock();
        // SAFETY: 队列内存独占且已清零；`rx_mem` 来自 alloc_queue。
        unsafe {
            let q = QueueMem::at(rx_mem);
            for i in 0..QSIZE {
                (*q).desc[i].addr = bufs[i];
                (*q).desc[i].len = RX_BUF as u32;
                (*q).desc[i].flags = VIRTQ_DESC_F_WRITE;
                (*q).desc[i].next = 0;
            }
            // avail.idx = QSIZE：告诉设备"这 64 个都能用"
            (*q).avail[1] = QSIZE as u16;
            for i in 0..QSIZE {
                (*q).avail[2 + i] = i as u16;
            }
        }
        *RXQ.lock() = Some(QState { mem_phys: rx_mem, avail_idx: QSIZE as u16, last_used: 0 });
        // 门铃：通知 RX 队列有可用缓冲
        outw(io + R_QUEUE_NOTIFY, Q_RX);
    }

    *TXQ.lock() = Some(QState { mem_phys: tx_mem, avail_idx: 0, last_used: 0 });
    *DEV.lock() = Some(Dev { io_base: io, mac });

    outb(io + R_STATUS, STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
    // 回读所有关键寄存器：设备到底接受了什么配置，只能从它嘴里问
    let st = inb(io + R_STATUS);
    outw(io + R_QUEUE_SEL, Q_RX);
    let qnum_rx = inl(io + R_QUEUE_NUM);
    let qpfn_rx = inl(io + R_QUEUE_PFN);
    outw(io + R_QUEUE_SEL, Q_TX);
    let qnum_tx = inl(io + R_QUEUE_NUM);
    let qpfn_tx = inl(io + R_QUEUE_PFN);
    kserialln!(
        "[virtio] 就绪：io={:#x} mac={:x}:{:x}:{:x}:{:x}:{:x}:{:x} status={:#x} \
         rx(num={} pfn={}) tx(num={} pfn={}) feat={:#x}",
        io, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], st,
        qnum_rx, qpfn_rx, qnum_tx, qpfn_tx, want
    );
    Ok(())
}

fn outw(port: u16, v: u16) {
    // SAFETY: 同上（IO 端口）。
    unsafe {
        core::arch::asm!("out dx, ax", in("dx") port, in("ax") v, options(nomem, nostack, preserves_flags));
    }
}

impl QueueMem {
    /// 把队列物理地址当成虚拟地址用。
    ///
    /// # Safety
    /// `phys` 必须是 `alloc_queue` 返回的、按 4 KiB 对齐的连续队列内存。
    unsafe fn at(phys: u64) -> *mut QueueMem {
        paging::phys_to_virt(phys) as *mut QueueMem
    }
}

/// 分配一个队列（desc + avail + used 连续一页）+ 64 个数据缓冲。
fn alloc_queue(sel: u16, io: u16) -> Result<u64, &'static str> {
    // **必须两页连续内存**：virtio 规范要求 used 环按 4 KiB 对齐，
    // 而 desc(1024) + avail(134) 之后要 pad 到 4096——used 于是落在
    // 第二页开头。只给一页的话 used 环在页外，读回来是 0xFFFFFFFF
    // （实测），设备写的完成项一个都看不到，RX 永远是 0。
    let phys = frames::alloc_frames(2).ok_or("virtio 队列页分配失败")?;
    // SAFETY: 刚分配的独占连续两页，整块清零。
    unsafe {
        core::ptr::write_bytes(paging::phys_to_virt(phys) as *mut u8, 0, 8192);
    }
    {
        let mut bufs = BUF_PHYS.lock();
        for i in 0..QSIZE {
            if bufs[i] == 0 {
                bufs[i] = frames::alloc_frame().ok_or("virtio 缓冲分配失败")?;
                // SAFETY: 刚分配独占页。
                unsafe {
                    core::ptr::write_bytes(paging::phys_to_virt(bufs[i]) as *mut u8, 0, 4096);
                }
            }
        }
    }
    outw(io + R_QUEUE_SEL, sel);
    outl(io + R_QUEUE_PFN, (phys / 4096) as u32);
    Ok(phys)
}

pub fn mac() -> [u8; 6] {
    match DEV.lock().as_ref() {
        Some(d) => d.mac,
        None => [0; 6],
    }
}

pub fn up() -> bool {
    DEV.lock().is_some()
}

/// 发一帧。等上一条发送完成（设备处理很快，NAT 下不会积压）。
pub fn send(frame: &[u8]) -> Result<(), &'static str> {
    let dev = DEV.lock();
    let d = dev.as_ref().ok_or("virtio 未初始化")?;
    let bufs = BUF_PHYS.lock();
    let mut txq = TXQ.lock();
    let qs = txq.as_mut().ok_or("TX 队列未就绪")?;
    // SAFETY: qs.mem_phys 来自 alloc_queue，是独占的队列页。
    let q = unsafe { QueueMem::at(qs.mem_phys) };

    let slot = qs.avail_idx as usize % QSIZE;
    // SAFETY: 队列与缓冲都由本驱动独占；索引已取模。
    unsafe {
        // 等这个槽位上一次的 used 回来（简单同步：used.idx 追平）
        // used[0] = flags(u16) | idx(u16)<<16 —— idx 在**高** 16 位。
        let used_idx = ((*q).used[0] >> 16) as u16;
        if qs.avail_idx.wrapping_sub(used_idx) as usize >= QSIZE {
            return Err("TX 队列满");
        }
        core::ptr::copy_nonoverlapping(
            frame.as_ptr(),
            paging::phys_to_virt(bufs[slot]) as *mut u8,
            frame.len(),
        );
        (*q).desc[slot].addr = bufs[slot];
        (*q).desc[slot].len = frame.len() as u32;
        (*q).desc[slot].flags = 0; // 只读，无 next
        (*q).desc[slot].next = 0;

        let ring_slot = 2 + (qs.avail_idx as usize % QSIZE);
        (*q).avail[ring_slot] = slot as u16;
        qs.avail_idx = qs.avail_idx.wrapping_add(1);
        (*q).avail[1] = qs.avail_idx; // avail.idx 必须最后写
        outw(d.io_base + R_QUEUE_NOTIFY, Q_TX);
        {
            use core::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            if N.fetch_add(1, Ordering::Relaxed) < 3 {
                kserialln!(
                    "[virtio] tx len={} avail_idx={} avail[0]={:#x} used[0]={:#x}",
                    frame.len(), qs.avail_idx, (*q).avail[0], (*q).used[0]
                );
            }
        }
    }
    Ok(())
}

/// 轮询 RX：把收到的帧逐个交给 `f`，返回处理了几帧。
pub fn poll(mut f: impl FnMut(&[u8])) -> usize {
    if DEV.lock().is_none() {
        return 0;
    }
    let bufs = BUF_PHYS.lock();
    let mut rxq = RXQ.lock();
    let qs = match rxq.as_mut() {
        Some(x) => x,
        None => return 0,
    };
    let q = unsafe { QueueMem::at(qs.mem_phys) };
    let mut got = 0;
    // SAFETY: 队列内存独占；used 环由设备写、我们只读。
    unsafe {
        // used[0] 高 16 位是 idx；环项从 offset 4 起：
        // u32 索引 1 = id、2 = len，之后每项 2 个 u32。
        let used_idx = ((*q).used[0] >> 16) as u16;
        {
            use core::sync::atomic::{AtomicU32, Ordering};
            static N2: AtomicU32 = AtomicU32::new(0);
            if N2.fetch_add(1, Ordering::Relaxed) < 3 {
                kserialln!(
                    "[virtio] poll: used[0]={:#x} idx={} last={} avail_idx={}",
                    (*q).used[0], used_idx, qs.last_used, qs.avail_idx
                );
            }
        }
        while qs.last_used != used_idx && got < QSIZE {
            let slot = (qs.last_used as usize) % QSIZE;
            let id = (*q).used[1 + slot * 2] as usize;
            let len = (*q).used[2 + slot * 2] as usize;
            if id < QSIZE && len > 0 {
                let buf_va = paging::phys_to_virt(bufs[id]) as *const u8;
                let slice = core::slice::from_raw_parts(buf_va, len.min(RX_BUF));
                f(slice);
                got += 1;
            }
            qs.last_used = qs.last_used.wrapping_add(1);
            // 重新把这个缓冲挂回 avail 环（设备才能继续往里写）
            let ring_slot = 2 + (qs.avail_idx as usize % QSIZE);
            (*q).avail[ring_slot] = id as u16;
            qs.avail_idx = qs.avail_idx.wrapping_add(1);
            (*q).avail[1] = qs.avail_idx;
        }
        if got > 0 {
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
