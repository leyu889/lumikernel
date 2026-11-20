//! Intel 82540EM（e1000）网卡驱动——VBox 的 `--nictype1 82540EM`。
//!
//! ## 为什么选轮询而不是中断
//!
//! RX 中断要新申请一条向量、接进 IDT、再做中断与 socket 的唤醒耦合——
//! 每一步都能把稳定性搞砸。轮询方案：**每次有 syscall 进内核 +
//! 每个时钟 tick，顺手查一次 RX 环**。NAT 内网延迟多几百微秒，
//! 但换来"收包路径不会在任何锁的临界区里"。
//!
//! ## DMA 描述符
//!
//! 82540 用 legacy descriptor：16 字节一个，`addr + length + status`。
//! TX 环 32 项、RX 环 32 项，各占一页连续物理内存。
//! `buf` 都是独立帧（不用连续大块，1 个描述符对应 1 个 2 KiB 缓冲）。
//!
//! ## 与稳定性纪律的呼应
//!
//! 所有 MMIO 读写在锁内；`net_rx_poll` 允许从任意 syscall 上下文调用，
//! 所以锁用 `IrqSpinlock`。

use crate::arch::sync::IrqSpinlock;
use knet;
use crate::mm::{frames, paging};

/// 82540EM 的 PCI 设备号（VBox 82540EM）。
pub const VENDOR_INTEL: u16 = 0x8086;
pub const DEVICE_82540EM: u16 = 0x100E;

// 常用寄存器偏移（MMIO，DWORD 寻址）。
const REG_CTRL: usize = 0x0000 / 4;
const REG_STATUS: usize = 0x0008 / 4;
const REG_RCTL: usize = 0x0100 / 4;
const REG_TCTL: usize = 0x0400 / 4;
const REG_RDBAL: usize = 0x2800 / 4;
const REG_RDBAH: usize = 0x2804 / 4;
const REG_RDLEN: usize = 0x2808 / 4;
const REG_RDH: usize = 0x2810 / 4;
const REG_RDT: usize = 0x2818 / 4;
const REG_TDBAL: usize = 0x3800 / 4;
const REG_TDBAH: usize = 0x3804 / 4;
const REG_TDLEN: usize = 0x3808 / 4;
const REG_TDH: usize = 0x3810 / 4;
const REG_TDT: usize = 0x3818 / 4;
const REG_RAL_LO: usize = 0x5400 / 4;
const REG_RAL_HI: usize = 0x5404 / 4;

const RCTL_EN: u32 = 1 << 1;
const RCTL_SBP: u32 = 1 << 2;
const RCTL_UPE: u32 = 1 << 3;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_BAM: u32 = 1 << 15; // 接受广播（ARP 请求是广播）
const RCTL_BSIZE_2048: u32 = 0;
const RCTL_BSEX_0: u32 = 0;

const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;

const STATUS_ASDV_1000: u32 = 3 << 9;
const STATUS_LU: u32 = 1 << 1;

/// 环大小（描述符数）。
const RING_SIZE: usize = 32;
/// RX 缓冲大小（2 KiB，82540 对齐要求的最小单位）。
const RX_BUF: u16 = 2048;

/// legacy RX 描述符。
#[repr(C)]
struct RxDesc {
    addr: u64,
    length: u16,
    csum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

/// legacy TX 描述符。
#[repr(C)]
struct TxDesc {
    addr: u64,
    length: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u16,
}

const TX_CMD_EOP: u8 = 1 << 0;
const TX_CMD_IFCS: u8 = 1 << 1;
const TX_CMD_RS: u8 = 1 << 3;

/// 每个环的缓冲（帧内存），独立分配的帧。
struct RingBufs {
    rx_desc_phys: u64,
    tx_desc_phys: u64,
    rx_buf_phys: [u64; RING_SIZE],
    tx_buf_phys: [u64; RING_SIZE],
}

struct Dev {
    mmio: usize, // 虚拟地址（DWORD 寻址单位在访问处换算）
    rx_tail: usize,
    tx_tail: usize,
    tx_head_done: usize,
    mac: [u8; 6],
}

static DEV_LOCK: IrqSpinlock<Option<Dev>> = IrqSpinlock::new(None);
static RBUFS: IrqSpinlock<Option<RingBufs>> = IrqSpinlock::new(None);

fn mmio_read(base: usize, reg: usize) -> u32 {
    // SAFETY: base 是 PCI BAR0 映射的 MMIO 区（pages::phys_to_virt），
    // 大小 128 KiB，寄存器偏移都在范围内。
    unsafe { core::ptr::read_volatile((base + reg * 4) as *const u32) }
}

fn mmio_write(base: usize, reg: usize, v: u32) {
    // SAFETY: 同上。
    unsafe {
        core::ptr::write_volatile((base + reg * 4) as *mut u32, v);
    }
}

/// 从 PCI 枚举结果里找 82540EM 并初始化。
///
/// 返回 `Err(字符串)` 的地方都带上下文——网卡初始化失败的每一环
/// （没找到设备 / BAR 不是 MMIO / DMA 内存失败）都要能单独辨认。
pub fn init() -> Result<(), &'static str> {
    let mut devs = [crate::dev::pci::Device::EMPTY; 16];
    let rep = crate::dev::pci::enumerate(&mut devs);
    let dev = (0..rep.devices)
        .map(|i| &devs[i])
        .find(|d| d.vendor == VENDOR_INTEL && d.device == DEVICE_82540EM)
        .ok_or("PCI 上没有 82540EM（检查 --nictype1 82540EM --nic1 nat）")?;

    // 82540 有多个 BAR：VBox 的排布里 MMIO 不一定在 BAR0（实测 BAR0 是
    // I/O 端口）。扫全部 6 个 BAR 拿第一个 MMIO 的——拿错了映射到 I/O
    // 空间，MMIO 读出来全是垃圾，网卡"找到但读不动"。
    let mut bar0: Option<u32> = None;
    let mut skip_next = false;
    for i in 0..6u8 {
        if skip_next {
            skip_next = false;
            continue;
        }
        let raw = crate::dev::pci::config_read32(dev, 0x10 + i * 4);
        if raw == 0 || raw == 0xFFFF_FFFF {
            continue;
        }
        // 64 位 BAR：低 2 位 = 00（32 位）或 10（64 位）。
        // 64 位 BAR 占两个 BAR 槽，第二个 DWORD 是地址高 32 位——
        // **必须跳过**，否则会把"地址的高半段"当成一个独立 BAR
        // （实测拿到 0xc180 这种未对齐值，MMIO 全是垃圾）。
        if raw & 0x6 == 0x4 {
            skip_next = true;
        }
        let is_mmio = raw & 1 == 0;
        if is_mmio && bar0.is_none() {
            bar0 = Some(raw & !0xF);
        }
    }
    let bar0 = bar0.ok_or("网卡没有 MMIO BAR")?;

    // **必须先使能 MEM 空间与总线主控**：command bit2=MEM、bit4=BUS_MASTER。
    // 没使能时 MMIO 读回全 1（0xFFFF），CTRL.RST 看起来永远不清零，
    // 表现是"网卡复位超时"——现场完全看不出是 command 寄存器的问题。
    crate::dev::pci::set_command_bit(dev, 2, true);
    crate::dev::pci::set_command_bit(dev, 4, true);

    // DMA 描述符环：TX/RX 各一个 4 KiB 页放描述符 + 每描述符一个缓冲帧
    let rx_desc_phys = frames::alloc_frame().ok_or("RX 描述符页分配失败")?;
    let tx_desc_phys = frames::alloc_frame().ok_or("TX 描述符页分配失败")?;
    let mut rx_buf = [0u64; RING_SIZE];
    let mut tx_buf = [0u64; RING_SIZE];
    for i in 0..RING_SIZE {
        rx_buf[i] = frames::alloc_frame().ok_or("RX 缓冲分配失败")?;
        tx_buf[i] = frames::alloc_frame().ok_or("TX 缓冲分配失败")?;
    }

    // **BAR 可能落在 1 GiB 直接映射之外**（VBox 把 82540 的 MMIO 放在
    // 2 GiB 附近）：`phys_to_virt` 对那种地址返回的虚拟地址没有页表 backing，
    // 第一次读就是 #PF（实测 cr2=0xffff800081400000，直接映射未覆盖）。
    // 用当前页表给这一片单独建 4 KiB 映射（KERNEL_RW），128 KiB 覆盖满。
    let base = if bar0 as u64 + 0x20000 < (1 << 30) {
        paging::phys_to_virt(bar0 as u64) as usize
    } else {
        let mut va = 0xFFFF_A000_0000_0000u64;
        while va < 0xFFFF_A000_0004_0000 {
            if paging::translate_in(paging::read_cr3(), va).is_none() {
                break;
            }
            va += 0x1000;
        }
        for off in 0..0x80u64 {
            let _ = paging::map_in_current(va + off * 0x1000, bar0 as u64 + off * 0x1000,
                kvm::Flags::KERNEL_RW);
        }
        va as usize
    };
    // 诊断：BAR 地址、使能后 CTRL/RAL 原始值——网卡链路的每一步都要可见
    let raw_bars: [u32; 6] = [
        crate::dev::pci::config_read32(dev, 0x10),
        crate::dev::pci::config_read32(dev, 0x14),
        crate::dev::pci::config_read32(dev, 0x18),
        crate::dev::pci::config_read32(dev, 0x1c),
        crate::dev::pci::config_read32(dev, 0x20),
        crate::dev::pci::config_read32(dev, 0x24),
    ];
    kserialln!(
        "[e1000] bars={:#x},{:#x},{:#x}->bar={:#x} ctrl={:#x} mac={:x}:{:x}:{:x}:{:x}:{:x}:{:x} rctl={:#x}",
        raw_bars[0], raw_bars[1], raw_bars[2], bar0,
        mmio_read(base, REG_CTRL),
        (mmio_read(base, REG_RAL_LO) & 0xFF) as u8,
        ((mmio_read(base, REG_RAL_LO) >> 8) & 0xFF) as u8,
        ((mmio_read(base, REG_RAL_LO) >> 16) & 0xFF) as u8,
        ((mmio_read(base, REG_RAL_LO) >> 24) & 0xFF) as u8,
        (mmio_read(base, REG_RAL_HI) & 0xFF) as u8,
        ((mmio_read(base, REG_RAL_HI) >> 8) & 0xFF) as u8,
        mmio_read(base, REG_RCTL)
    );

    // 复位后 MAC 在 RAL 里（VBox 会写入虚拟机的 MAC）
    let ral_lo = mmio_read(base, REG_RAL_LO);
    let ral_hi = mmio_read(base, REG_RAL_HI);
    let mac = [
        (ral_lo & 0xFF) as u8,
        ((ral_lo >> 8) & 0xFF) as u8,
        ((ral_lo >> 16) & 0xFF) as u8,
        ((ral_lo >> 24) & 0xFF) as u8,
        (ral_hi & 0xFF) as u8,
        ((ral_hi >> 8) & 0xFF) as u8,
    ];

    // 描述符环清零
    for off in 0..4096usize {
        // SAFETY: 刚分配的独占页，经直接映射区写。
        unsafe {
            core::ptr::write_bytes(
                (paging::phys_to_virt(rx_desc_phys) as *mut u8).add(off),
                0,
                1,
            );
            core::ptr::write_bytes(
                (paging::phys_to_virt(tx_desc_phys) as *mut u8).add(off),
                0,
                1,
            );
        }
    }
    // RX 描述符填缓冲地址
    let rx_desc_va = paging::phys_to_virt(rx_desc_phys) as *mut RxDesc;
    for i in 0..RING_SIZE {
        // SAFETY: 描述符页独占，下标在环内。
        unsafe {
            (*rx_desc_va.add(i)).addr = rx_buf[i];
            (*rx_desc_va.add(i)).status = 0;
        }
    }

    // 通用寄存器：复位 + 使能
    mmio_write(base, REG_CTRL, 0x0400_0040 | (1 << 26)); // SLU + RST
    // 等复位完成（RST 位自清零）
    let mut spins = 0;
    while mmio_read(base, REG_CTRL) & (1 << 26) != 0 {
        spins += 1;
        if spins > 1_000_000 {
            return Err("网卡复位超时");
        }
    }
    // **读-改-写置 SLU**，不能写死值：CTRL 里 FD/SPEED 是硬件自己协商
    // 出来的，写死一个常量会把它们清掉，于是 ctrl 读回 0xa09（SLU 丢失）、
    // STATUS.LU 恒 0——链路永远起不来，网卡"初始化成功但一帧都收不到"。
    // **同时必须清 CTRL.LR（bit3，Link Reset）**：它是"链路复位"，
    // 硬件置起后应自清，但如果被读-改-写原样写回，就会一直保持置位——
    // PHY 被反复复位，STATUS.LU 恒为 0，TX 描述符永不被消费
    // （实测 tdt=1 tdh=0 desc_status=0，ARP 发得出去但收不到任何东西）。
    // 等 EEPROM 自动读取完成（EECD bit4）：Linux 驱动在动 CTRL 之前
    // 都会等这一位。没等就置 SLU，PHY 拿到的还是未加载的配置。
    let mut ee = 0u32;
    while mmio_read(base, 0x0010 / 4) & 0x10 == 0 && ee < 100_000 {
        ee += 1;
        core::hint::spin_loop();
    }

    // CTRL：置 SLU + 强制 100M 全双工（虚拟环境里 PHY 自动协商常常不工作，
    // 强制速率是 Linux e1000 驱动也会做的兜底）。位：
    // bit4 FRCSPD、bit5 FRC_DPX、bit8 SPEED_SEL(1=100M)、bit9 SPEED(1=1000M)
    let ctrl = mmio_read(base, REG_CTRL);
    mmio_write(
        base,
        REG_CTRL,
        (ctrl | 0x40 | 0x10 | 0x20 | 0x100) & !(1 << 26) & !0x8 & !0x200,
    ); // SLU|FRCSPD|FRC_DPX|100M，清 RST/LR/1000M
    kserialln!("[e1000] eeprom 等待 {}，ctrl 写入 {:#x}", ee,
        (ctrl | 0x40 | 0x10 | 0x20 | 0x100) & !(1 << 26) & !0x8 & !0x200);

    // 等 PHY 协商完成。VBox 里链路协商可能上百毫秒，用长延时轮询
    // （每次读 STATUS 是一次 MMIO/VM exit，本身就在耗时间）。
    let mut wait = 0u32;
    while mmio_read(base, REG_STATUS) & STATUS_LU == 0 && wait < 200_000 {
        wait += 1;
        core::hint::spin_loop();
    }
    kserialln!("[e1000] 链路等待 {} 次，status={:#x}", wait, mmio_read(base, REG_STATUS));

    // RX 环
    mmio_write(base, REG_RDBAL, rx_desc_phys as u32);
    mmio_write(base, REG_RDBAH, (rx_desc_phys >> 32) as u32);
    mmio_write(base, REG_RDLEN, (RING_SIZE * 16) as u32);
    mmio_write(base, REG_RDH, 0);
    mmio_write(base, REG_RDT, RING_SIZE as u32 - 1);
    mmio_write(
        base,
        REG_RCTL,
        RCTL_EN | RCTL_SBP | RCTL_UPE | RCTL_MPE | RCTL_BAM | RCTL_BSIZE_2048 | RCTL_BSEX_0,
    );

    // TX 环
    mmio_write(base, REG_TDBAL, tx_desc_phys as u32);
    mmio_write(base, REG_TDBAH, (tx_desc_phys >> 32) as u32);
    mmio_write(base, REG_TDLEN, (RING_SIZE * 16) as u32);
    mmio_write(base, REG_TDH, 0);
    mmio_write(base, REG_TDT, 0);
    mmio_write(base, REG_TCTL, TCTL_EN | TCTL_PSP);

    let link = mmio_read(base, REG_STATUS) & STATUS_LU != 0;
    kserialln!(
        "[e1000] init done: ctrl={:#x} status={:#x} link={} rctl={:#x} rdh={} rdt={} tctl={:#x}",
        mmio_read(base, REG_CTRL),
        mmio_read(base, REG_STATUS),
        link as u8,
        mmio_read(base, REG_RCTL),
        mmio_read(base, REG_RDH),
        mmio_read(base, REG_RDT),
        mmio_read(base, REG_TCTL)
    );

    *RBUFS.lock() = Some(RingBufs { rx_desc_phys, tx_desc_phys, rx_buf_phys: rx_buf, tx_buf_phys: tx_buf });
    *DEV_LOCK.lock() = Some(Dev { mmio: base, rx_tail: 0, tx_tail: 0, tx_head_done: 0, mac });

    Ok(())
}

pub fn mac() -> [u8; 6] {
    match DEV_LOCK.lock().as_ref() {
        Some(d) => d.mac,
        None => [0; 6],
    }
}

pub fn up() -> bool {
    DEV_LOCK.lock().is_some()
}

/// 发一帧。失败返回 Err（环满/驱动未就绪）。
pub fn send(frame: &[u8]) -> Result<(), &'static str> {
    let mut dev_guard = DEV_LOCK.lock();
    let d = dev_guard.as_mut().ok_or("网卡未初始化")?;
    let rbs = RBUFS.lock();
    let rbs = rbs.as_ref().ok_or("网卡缓冲未就绪")?;

    if frame.len() > knet::MTU {
        return Err("帧超过 MTU");
    }

    // 等"下一项"空出来。
    //
    // **判据是 `(tail+1) % N != TDH`，不是 `TDH == tail`**：TDH 是硬件
    // 消费游标，初始 TDH=0、tail=0——按"相等即满"判，第一次发包就会被
    // 误判成环满（实测 tx=0、ping 无回复，且现场只能看到"ARP 解析失败"）。
    // 环形队列必须留一个空位来区分"空"和"满"。
    let tx_desc_va = paging::phys_to_virt(rbs.tx_desc_phys) as *mut TxDesc;
    let mut spins = 0;
    let next = (d.tx_tail + 1) % RING_SIZE;
    // SAFETY: MMIO 区与描述符页都已映射。
    unsafe {
        while (mmio_read(d.mmio, REG_TDH) as usize) == next && spins < 100_000 {
            spins += 1;
            core::hint::spin_loop();
        }
        if spins >= 100_000 {
            return Err("TX 环满（硬件没消费）");
        }

        let i = d.tx_tail;
        // 帧拷进本描述符的缓冲
        let buf_va = paging::phys_to_virt(rbs.tx_buf_phys[i]) as *mut u8;
        core::ptr::copy_nonoverlapping(frame.as_ptr(), buf_va, frame.len());

        let desc = tx_desc_va.add(i);
        (*desc).addr = rbs.tx_buf_phys[i];
        (*desc).length = frame.len() as u16;
        (*desc).cso = 0;
        (*desc).cmd = TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS;
        (*desc).status = 0;

        d.tx_tail = (d.tx_tail + 1) % RING_SIZE;
        mmio_write(d.mmio, REG_TDT, d.tx_tail as u32);
        {
            use core::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            if N.fetch_add(1, Ordering::Relaxed) < 3 {
                let td = (*desc).status;
                kserialln!("[e1000] tx: len={} tdt={} tdh={} desc_status={:#x}",
                    frame.len(), d.tx_tail, mmio_read(d.mmio, REG_TDH), td);
            }
        }
    }
    Ok(())
}

/// 轮询一次 RX：把收到的帧逐个交给 `f`。
///
/// 在两个地方被调：每个时钟 tick（BSP）+ 每次 syscall 进内核。
/// NAT 下包不多，一次最多清空一个环（32 帧）。
pub fn poll(mut f: impl FnMut(&[u8])) -> usize {
    let mut dev_guard = DEV_LOCK.lock();
    let d = match dev_guard.as_mut() {
        Some(x) => x,
        None => return 0,
    };
    let rbs = RBUFS.lock();
    let rbs = match rbs.as_ref() {
        Some(x) => x,
        None => return 0,
    };

    let mut got = 0;
    let rx_desc_va = paging::phys_to_virt(rbs.rx_desc_phys) as *mut RxDesc;
    // SAFETY: 描述符页/缓冲帧/MMIO 都已映射，锁内独占。
    unsafe {
        for _ in 0..RING_SIZE {
            let i = d.rx_tail;
            let desc = rx_desc_va.add(i);
            if (*desc).status & 0x01 == 0 {
                break; // 没有新帧
            }
            let len = (*desc).length as usize;
            // 硬件写描述符时要清状态位，表示这一项已经被软件消费
            (*desc).status = 0;
            d.rx_tail = (d.rx_tail + 1) % RING_SIZE;
            // 通知硬件"这一项可以复用了"：RDT 指向最后一个可用描述符
            mmio_write(d.mmio, REG_RDT, d.rx_tail as u32);

            let buf_va = paging::phys_to_virt(rbs.rx_buf_phys[i]) as *const u8;
            let slice = core::slice::from_raw_parts(buf_va, len.min(RX_BUF as usize));
            f(slice);
            got += 1;
        }
    }
    got
}

/// 网卡健康检查（自检用）：`(已就绪, 链路通)`。
pub fn status() -> (bool, bool) {
    let g = DEV_LOCK.lock();
    match g.as_ref() {
        Some(d) => (true, mmio_read(d.mmio, REG_STATUS) & (STATUS_LU | STATUS_ASDV_1000) != 0),
        None => (false, false),
    }
}
