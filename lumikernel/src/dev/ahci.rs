//! AHCI（SATA）驱动（M10）
//!
//! 为什么非写不可：真机上找不到 IDE 那种"读 0x1F0 端口"的老控制器。
//! 2010 年之后的机器基本都是 **AHCI**（SATA），而 M10 的验收是"真机启动到 shell"，
//! 所以磁盘这条路必须自己走 AHCI——不然内核起来了也读不到 ext2。
//!
//! 实现范围（够用为止，都写清楚）：
//! - 支持一个 AHCI 控制器，最多两块盘（系统盘 + 数据盘），**不用中断**（轮询 PxCI）；
//! - 只走 DMA（`READ/WRITE DMA EXT`），不做 PIO——PIO 在 AHCI 里又慢又没必要；
//! - 不做热插拔、不做 NCQ（队列深度固定 1）、不做 64 位地址以上的 PRDT 拆分
//!   （我们的缓冲区都是 1 页、物理地址在 4 GiB 内）。
//!
//! 关键结构（都是规范里定死的偏移，写错不会报错、只会"命令发出去没反应"）：
//!
//! ```text
//! ABAR + 0x000       通用寄存器：CAP / GHC / IS / PI / VS
//! ABAR + 0x100 + 0x80*p   端口 p 的寄存器：CLB / FB / IS / CMD / TFD / SIG / SSTS / CI
//! ```
//!
//! 一次读写的流程：
//!   命令列表项(32B) → 命令表(CFIS 20B + PRDT 16B*n) → 置 PxCI 对应位 → 轮询到清零

use core::sync::atomic::{AtomicBool, Ordering};

use kvm::Flags;

use crate::dev::pci;
use crate::mm::{frames, paging};

/// MMIO 映射到 vmalloc 区的哪一段（避开内核映像与堆）。
const MMIO_VA: u64 = crate::mm::VMALLOC_BASE + 0x2000_0000;
/// ABAR 一般 8 KiB，这里按 64 KiB 映射，省得算大小。
const MMIO_PAGES: u64 = 16;

const REG_CAP: u64 = 0x00;
const REG_GHC: u64 = 0x04;
const REG_IS: u64 = 0x08;
const REG_PI: u64 = 0x0C;

const PORT_BASE: u64 = 0x100;
const PORT_STEP: u64 = 0x80;
const P_CLB: u64 = 0x00;
const P_FB: u64 = 0x08;
const P_IS: u64 = 0x10;
const P_CMD: u64 = 0x18;
const P_TFD: u64 = 0x20;
const P_SIG: u64 = 0x24;
const P_SSTS: u64 = 0x28;
const P_SERR: u64 = 0x30;
const P_CI: u64 = 0x38;

const CMD_ST: u32 = 1 << 0;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;

const GHC_HR: u32 = 1 << 0;
const GHC_AE: u32 = 1 << 31;

const IS_TFES: u32 = 1 << 30;
const IS_ERR: u32 = 1 << 11;

/// ATA 命令：读/写 DMA（48 位 LBA）/ FLUSH CACHE EXT。
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH_EXT: u8 = 0xEA;
const ATA_IDENTIFY: u8 = 0xEC;

/// 轮询上限。10 万次空转在虚拟机里约几十毫秒，足够一次 4 KiB DMA；
/// 真机上更慢一点也没关系——超时是"明确报错"，不是死等。
const SPIN_LIMIT: u32 = 200_000;

#[derive(Clone, Copy)]
struct Slot {
    used: bool,
    /// 端口寄存器基址（已映射的虚拟地址）。
    sx: u64,
    /// 命令列表 / FIS 接收区 / 命令表 / DMA 数据页（虚拟地址）。
    clb: u64,
    fb: u64,
    ct: u64,
    buf: u64,
    buf_phys: u64,
    sectors: u64,
    /// 弹跳缓存：`buf` 里现在装的是从 `c_lba` 起 `c_count` 个扇区。
    ///
    /// 为什么需要：`ata` 层按 512 字节扇区向上暴露，而底层一次 DMA
    /// 完全可以搬 4 KiB。不加缓存的话，读 1 MiB 要发 2048 条命令——
    /// 实测只有 **1 MiB/s**（M11 的性能自检把这个数字摆出来了）。
    /// 加上之后顺序读的 7/8 命中，命令数降到 1/8。
    c_valid: bool,
    c_lba: u64,
    c_count: u16,
}

const EMPTY_SLOT: Slot = Slot {
    used: false,
    sx: 0,
    clb: 0,
    fb: 0,
    ct: 0,
    buf: 0,
    buf_phys: 0,
    sectors: 0,
    c_valid: false,
    c_lba: 0,
    c_count: 0,
};

/// 一次 DMA 最多搬几个 512B 扇区（**上限就是 1 页 = 8 个**）。
///
/// 试过提到 16/64 个（8/32 KiB，跨页缓冲）：VirtualBox 的 AHCI 会让那条命令
/// 永远不完成（PxCI 不清零，也没有置错误位），整机静默卡死。
/// 详见 `docs/04-工程规约.md` #69。单页是当前环境的上限，
/// 在真机上跑多页 DMA 需要重新验证。
const BOUNCE_SECTORS: u64 = 8;
const BOUNCE_PAGES: usize = (BOUNCE_SECTORS as usize * 512) / 4096;

fn set_cache(slot: usize, lba: u64, count: u16) {
    // SAFETY: 单核；磁盘操作由 fs 层的 `IrqSpinlock` 串行化。
    unsafe {
        if slot < 2 {
            SLOTS[slot].c_valid = true;
            SLOTS[slot].c_lba = lba;
            SLOTS[slot].c_count = count;
        }
    }
}

fn drop_cache(slot: usize) {
    // SAFETY: 同上。
    unsafe {
        if slot < 2 {
            SLOTS[slot].c_valid = false;
            SLOTS[slot].c_count = 0;
        }
    }
}

static mut SLOTS: [Slot; 2] = [EMPTY_SLOT; 2];
static READY: AtomicBool = AtomicBool::new(false);

/// 是否已经接管了磁盘（`ata` 层据此决定走 AHCI 还是 PIO）。
pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// 这个槽位有没有盘？（单盘分区布局下，数据盘是 0 号端口上的另一个分区，
/// 此时 1 号槽位是空的。）
pub fn slot_used(slot: usize) -> bool {
    // SAFETY: 单核；SLOTS 初始化后只读。
    unsafe { slot < 2 && SLOTS[slot].used }
}

pub fn sectors(slot: usize) -> u64 {
    // SAFETY: 单核；SLOTS 只在 init 期间写。
    unsafe {
        if slot < 2 {
            SLOTS[slot].sectors
        } else {
            0
        }
    }
}

pub struct Report {
    pub abar: u64,
    pub ports_found: usize,
    pub system_sectors: u64,
    pub data_sectors: u64,
}

#[inline]
fn rd(sx: u64, off: u64) -> u32 {
    // SAFETY: sx 是映射过的 MMIO 区间，off 都在端口寄存器范围内。
    unsafe { core::ptr::read_volatile((sx + off) as *const u32) }
}

#[inline]
fn wr(sx: u64, off: u64, v: u32) {
    // SAFETY: 同上，写的是设备寄存器。
    unsafe { core::ptr::write_volatile((sx + off) as *mut u32, v) }
}

/// 找控制器、映射 ABAR、复位、扫描端口。
pub fn init() -> Result<Report, &'static str> {
    let mut devs = [pci::Device::EMPTY; 32];
    let rep = pci::enumerate(&mut devs);
    let mut found: Option<pci::Device> = None;
    for d in devs.iter().take(rep.devices) {
        // 0x01/0x06 = SATA 控制器；prog_if 0x01 = AHCI（0x00 才是老 IDE）
        if d.class == 0x01 && d.subclass == 0x06 && d.prog_if == 0x01 {
            found = Some(*d);
            break;
        }
    }
    let dev = found.ok_or("没有 AHCI 控制器")?;

    let (abar, is_io) = pci::bar64(&dev, 5).ok_or("AHCI 的 ABAR（BAR5）读不出来")?;
    if is_io {
        return Err("ABAR 竟然是 IO 端口，不合规范");
    }

    // 开总线主控（DMA 必须），再映射 ABAR（MMIO 要关 cache）
    pci::set_command_bit(&dev, 2, true);

    let mmio_flags = Flags {
        present: true,
        writable: true,
        user: false,
        executable: false,
        cache_disable: true,
    };
    for i in 0..MMIO_PAGES {
        paging::map_in_current(MMIO_VA + i * 4096, abar + i * 4096, mmio_flags)
            .map_err(|_| "映射 ABAR 失败")?;
    }

    let cap = rd(MMIO_VA, REG_CAP);
    let mut ghc = rd(MMIO_VA, REG_GHC);
    if ghc & GHC_HR != 0 {
        // 复位还没结束：等它清掉（最多 ~0.5 s）
        let mut n = 0u32;
        while rd(MMIO_VA, REG_GHC) & GHC_HR != 0 {
            n += 1;
            if n > 5_000_000 {
                return Err("HBA 复位超时");
            }
        }
        ghc = rd(MMIO_VA, REG_GHC);
    }
    wr(MMIO_VA, REG_GHC, ghc | GHC_AE);
    // 给控制器一点时间把 AHCI 模式打开
    for _ in 0..1000 {
        core::hint::spin_loop();
    }

    let pi = rd(MMIO_VA, REG_PI);
    let np = (cap & 0x1F) + 1;
    let mut ports_found = 0usize;
    let mut sys_sectors = 0u64;
    let mut data_sectors = 0u64;

    for p in 0..np.min(32) {
        if pi & (1 << p) == 0 {
            continue;
        }
        let sx = MMIO_VA + PORT_BASE + p as u64 * PORT_STEP;
        // SSTS.DET == 3：设备在位且已建立通信
        if rd(sx, P_SSTS) & 0xF != 3 {
            continue;
        }
        let idx = ports_found;
        if idx >= 2 {
            break;
        }
        match bring_up(sx) {
            Ok((sec, mut slot)) => {
                slot.sectors = sec;
                // SAFETY: 单核、初始化阶段独占写 SLOTS。
                unsafe { SLOTS[idx] = slot };
                ports_found += 1;
                if idx == 0 {
                    sys_sectors = sec;
                } else {
                    data_sectors = sec;
                }
            }
            Err(_) => continue,
        }
    }

    if ports_found == 0 {
        return Err("AHCI 控制器在，但没有可用端口");
    }
    READY.store(true, Ordering::Release);
    Ok(Report {
        abar,
        ports_found,
        system_sectors: sys_sectors,
        data_sectors,
    })
}

/// 给一个端口分配内存、启动命令引擎、IDENTIFY 拿容量。
/// 返回 `(扇区数, 就绪的 Slot)`，由 `init` 负责登记。
fn bring_up(sx: u64) -> Result<(u64, Slot), &'static str> {
    // 停引擎：先 ST=0/FRE=0，等 FR/CR 归零
    let cmd = rd(sx, P_CMD);
    wr(sx, P_CMD, cmd & !(CMD_ST | CMD_FRE));
    let mut n = 0u32;
    while rd(sx, P_CMD) & (CMD_FR | CMD_CR) != 0 {
        n += 1;
        if n > SPIN_LIMIT {
            return Err("端口命令引擎停不下来");
        }
    }

    let clb_phys = frames::alloc_frame().ok_or("CLB 分配失败")?;
    let fb_phys = frames::alloc_frame().ok_or("FIS 分配失败")?;
    let ct_phys = frames::alloc_frame().ok_or("命令表分配失败")?;
    let buf_phys = frames::alloc_frame().ok_or("DMA 缓冲分配失败")?;
    for p in [clb_phys, fb_phys, ct_phys, buf_phys] {
        // SAFETY: 刚分配的独占帧，经直接映射区清零。
        unsafe { core::ptr::write_bytes(paging::phys_to_virt(p) as *mut u8, 0, 4096) };
    }

    wr(sx, P_CLB, clb_phys as u32);
    wr(sx, P_CLB + 4, (clb_phys >> 32) as u32);
    wr(sx, P_FB, fb_phys as u32);
    wr(sx, P_FB + 4, (fb_phys >> 32) as u32);

    wr(sx, P_SERR, 0xFFFF_FFFF);
    wr(sx, P_IS, 0xFFFF_FFFF);

    // 起引擎：FRE 先于 ST（规范要求）
    let cmd = rd(sx, P_CMD);
    wr(sx, P_CMD, cmd | CMD_FRE);
    let cmd = rd(sx, P_CMD);
    wr(sx, P_CMD, cmd | CMD_ST);
    let mut n = 0u32;
    while rd(sx, P_CMD) & CMD_CR == 0 {
        n += 1;
        if n > SPIN_LIMIT {
            return Err("端口命令引擎起不来");
        }
    }

    let slot = Slot {
        used: true,
        sx,
        clb: paging::phys_to_virt(clb_phys),
        fb: paging::phys_to_virt(fb_phys),
        ct: paging::phys_to_virt(ct_phys),
        buf: paging::phys_to_virt(buf_phys),
        buf_phys,
        sectors: 0,
        c_valid: false,
        c_lba: 0,
        c_count: 0,
    };

    issue(&slot, ATA_IDENTIFY, 0, 1, buf_phys, false)?;

    // IDENTIFY 回 256 个 16 位字；word 100..103 是 LBA48 容量
    let w = |i: usize| -> u64 {
        // SAFETY: buf 是一整页，i 远小于 512 个字。
        unsafe { core::ptr::read_volatile((slot.buf as *const u16).add(i)) as u64 }
    };
    let lba48 = w(100) | (w(101) << 16) | (w(102) << 32) | (w(103) << 48);
    let lba28 = w(60) | (w(61) << 16);
    let sec = if lba48 != 0 { lba48 } else { lba28 };
    if sec == 0 {
        return Err("IDENTIFY 拿到的容量是 0");
    }
    Ok((sec, slot))
}

/// 发一条 ATA 命令并等它完成。
fn issue(s: &Slot, cmd: u8, lba: u64, count: u16, buf_phys: u64, write: bool) -> Result<(), &'static str> {
    let ct = s.ct as *mut u8;
    // SAFETY: ct 是一页命令表，偏移都在 0x80+16 以内。
    unsafe {
        // CFIS（Host to Device，20 字节）
        core::ptr::write_bytes(ct, 0, 0x90);
        *ct = 0x27; // FIS 类型：H2D
        *ct.add(1) = 0x80; // C=1
        *ct.add(2) = cmd;
        *ct.add(4) = (lba & 0xFF) as u8;
        *ct.add(5) = ((lba >> 8) & 0xFF) as u8;
        *ct.add(6) = ((lba >> 16) & 0xFF) as u8;
        *ct.add(7) = 0x40; // LBA 模式
        *ct.add(8) = ((lba >> 24) & 0xFF) as u8;
        *ct.add(9) = ((lba >> 32) & 0xFF) as u8;
        *ct.add(10) = ((lba >> 40) & 0xFF) as u8;
        core::ptr::write_unaligned(ct.add(12) as *mut u16, count);
    }

    // PRDT：一条就够（缓冲区 ≤ 4 KiB）
    let prdtl = if count == 0 { 0u32 } else { 1u32 };
    if prdtl == 1 {
        // SAFETY: 命令表的 PRDT 从 0x80 开始，一条 16 字节。
        unsafe {
            let p = ct.add(0x80);
            core::ptr::write_unaligned(p as *mut u32, buf_phys as u32);
            core::ptr::write_unaligned(p.add(4) as *mut u32, (buf_phys >> 32) as u32);
            core::ptr::write_unaligned(p.add(8) as *mut u32, 0);
            let bytes = count as u32 * 512;
            core::ptr::write_unaligned(p.add(12) as *mut u32, (bytes - 1) | (1 << 31));
        }
    }

    // 命令头（32 字节）：CFL 是 FIS 长度（dword 数，20/4 = 5），
    // CTBA 必须是**物理地址**——设备不认虚拟地址。
    let ct_phys = paging::virt_to_phys(s.ct);
    let clb = s.clb as *mut u32;
    // SAFETY: 命令列表一页，头 0 在 32 字节内。
    unsafe {
        core::ptr::write_volatile(clb, 5 | ((write as u32) << 6) | (prdtl << 16));
        core::ptr::write_volatile(clb.add(1), 0);
        core::ptr::write_volatile(clb.add(2), ct_phys as u32);
        core::ptr::write_volatile(clb.add(3), (ct_phys >> 32) as u32);
    }

    wr(s.sx, P_IS, 0xFFFF_FFFF);
    wr(s.sx, P_SERR, 0xFFFF_FFFF);
    wr(s.sx, P_CI, 1); // 端口 0 的命令槽

    let mut n = 0u32;
    loop {
        let ci = rd(s.sx, P_CI);
        if ci & 1 == 0 {
            break;
        }
        // 只在每 64 圈查一次 P_IS：每次 MMIO 读在虚拟机里都是一次 VM exit，
        // 每圈读两个寄存器等于把等待时间翻倍。
        if n % 64 == 0 {
            let is = rd(s.sx, P_IS);
            if is & (IS_TFES | IS_ERR) != 0 {
                return Err("AHCI 命令出错（TFD/IS 报错）");
            }
        }
        n += 1;
        if n > SPIN_LIMIT {
            // 命令超时是**设备级**故障：继续用它只会让每一次 I/O 都等满超时。
            crate::dev::health::mark_down(crate::dev::health::Driver::Disk, "AHCI 命令超时");
            return Err("AHCI 命令超时");
        }
        core::hint::spin_loop();
    }
    let is = rd(s.sx, P_IS);
    if is & (IS_TFES | IS_ERR) != 0 {
        return Err("AHCI 命令完成但状态报错");
    }
    Ok(())
}

fn get(slot: usize) -> Result<Slot, ()> {
    // SAFETY: 单核；SLOTS 在 init 之后只读。
    unsafe {
        let s = SLOTS[slot];
        if s.used {
            Ok(s)
        } else {
            Err(())
        }
    }
}

/// 读 2 个扇区（= 1 个内核块）。`lba` 是 1 KiB 块号。
pub fn read_block(slot: usize, lba: u64, out: &mut [u8]) -> Result<(), ()> {
    let s = get(slot)?;
    if out.len() < 1024 {
        return Err(());
    }
    issue(&s, ATA_READ_DMA_EXT, lba * 2, 2, s.buf_phys, false).map_err(|_| ())?;
    // SAFETY: DMA 缓冲是一整页，读 1024 字节安全。
    unsafe {
        core::ptr::copy_nonoverlapping(s.buf as *const u8, out.as_mut_ptr(), 1024);
    }
    Ok(())
}

/// 写 2 个扇区（= 1 个内核块）。
pub fn write_block(slot: usize, lba: u64, data: &[u8]) -> Result<(), ()> {
    let s = get(slot)?;
    if data.len() < 1024 {
        return Err(());
    }
    // SAFETY: 同上，先拷进 DMA 缓冲。
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), s.buf as *mut u8, 1024);
    }
    issue(&s, ATA_WRITE_DMA_EXT, lba * 2, 2, s.buf_phys, true).map_err(|_| ())
}

/// `FLUSH CACHE EXT`：文件系统的日志提交通道靠它落盘。
pub fn flush(slot: usize) -> Result<(), ()> {
    let s = get(slot)?;
    issue(&s, ATA_FLUSH_EXT, 0, 0, 0, false).map_err(|_| ())
}

/// 读一个 512 字节扇区（给沿用扇区接口的旧代码）。
pub fn read_sector(slot: usize, lba: u32, out: &mut [u8; 512]) -> Result<(), &'static str> {
    // 驱动健康门：控制器已经被判定失联时**立刻返回**，不再去碰 MMIO。
    // 卡死的设备上"每次都等超时"会把一次坏的读放大成整机卡顿。
    if !crate::dev::health::is_up(crate::dev::health::Driver::Disk) {
        return Err("磁盘控制器已降级");
    }
    if crate::qa::fault::deny(kqa::fault::Point::AhciDown) {
        crate::dev::health::mark_down(crate::dev::health::Driver::Disk, "注入：控制器失联");
        return Err("故障注入：控制器失联");
    }
    let s = get(slot).map_err(|_| "该槽位没有 AHCI 盘")?;
    let lba64 = lba as u64;

    // 命中：条令都不发，直接从弹跳缓存拷
    if s.c_valid && lba64 >= s.c_lba && lba64 < s.c_lba + s.c_count as u64 {
        let off = ((lba64 - s.c_lba) as usize) * 512;
        // SAFETY: `buf` 是一整页，`off + 512 <= 4096`（count ≤ 8）。
        unsafe {
            core::ptr::copy_nonoverlapping((s.buf as *const u8).add(off), out.as_mut_ptr(), 512)
        };
        return Ok(());
    }

    // 未命中：一次读满一整页（顺序读时后面 7 次都会命中）
    let left = s.sectors.saturating_sub(lba64);
    let n = core::cmp::min(BOUNCE_SECTORS, left) as u16;
    if n == 0 {
        return Err("读越过磁盘末尾");
    }
    issue(&s, ATA_READ_DMA_EXT, lba64, n, s.buf_phys, false)?;
    set_cache(slot, lba64, n);
    // SAFETY: 同上。
    unsafe { core::ptr::copy_nonoverlapping(s.buf as *const u8, out.as_mut_ptr(), 512) };
    Ok(())
}

pub fn write_sector(slot: usize, lba: u32, data: &[u8; 512]) -> Result<(), &'static str> {
    // 写之前先失效：`buf` 马上要被写数据占用，而且原来的缓存内容已经过时。
    drop_cache(slot);
    let s = get(slot).map_err(|_| "该槽位没有 AHCI 盘")?;
    // SAFETY: 缓冲一整页。
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), s.buf as *mut u8, 512) };
    issue(&s, ATA_WRITE_DMA_EXT, lba as u64, 1, s.buf_phys, true)
}
