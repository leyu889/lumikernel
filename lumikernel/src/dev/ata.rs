//! ATA（IDE）PIO 块设备驱动
//!
//! M5 只有"主通道主盘 + 只读"；M8 加了**从盘 + 写 + FLUSH CACHE**，
//! 因为 LumiFS 需要一块可写的盘，而系统盘（ext2）是不该被动的东西。
//!
//! 为什么还是 PIO 不用 DMA：DMA 要配 bus-master PRDT、处理 64 位地址与
//! 页表约束、还要处理中断；PIO 只用 8 个端口。一个扇区约 20 µs，
//! 对 KiB 级的文件操作完全够。等真要做大吞吐再上 DMA。
//!
//! 几个必须记住的时序细节：
//! 1. 每条命令后要读 **alt status（0x3F6）4 次**当 400ns 延时——主状态口每读一次
//!    就清一次中断标志，读多了会把 IRQ 吃掉；
//! 2. 等 BSY 清之后还要看 ERR/DF，否则会把"设备报错"当成"数据就绪"，读出一堆 0xFF；
//! 3. 数据口必须用 16 位读写：按字节读会得到重复的高/低字节；
//! 4. **写完扇区必须发 FLUSH CACHE（0xE7）才算真的落盘**——
//!    设备内部有写缓存，不发这条命令，"已写入"可能只是"已进设备缓存"。
//!    日志文件系统的正确性直接依赖这一点，所以 `flush()` 不是可选项。

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::intr::frame::{inb, inw, outb};

const PORT_DATA: u16 = 0x1F0;
const PORT_ERR: u16 = 0x1F1;
const PORT_SECCOUNT: u16 = 0x1F2;
const PORT_LBA0: u16 = 0x1F3;
const PORT_LBA1: u16 = 0x1F4;
const PORT_LBA2: u16 = 0x1F5;
const PORT_DRIVE: u16 = 0x1F6;
const PORT_STATUS: u16 = 0x1F7;
const PORT_CMD: u16 = 0x1F7;
const PORT_ALT: u16 = 0x3F6;

const ST_BSY: u8 = 0x80;
const ST_DRDY: u8 = 0x40;
const ST_DF: u8 = 0x20;
const ST_DRQ: u8 = 0x08;
const ST_ERR: u8 = 0x01;

/// 文件系统块大小：1 KiB。ATA 物理扇区是 512 B，所以一次块读写 = 两个扇区。
pub const BLOCK_SIZE: usize = 1024;

/// 一次 ATA 延时：读 alt status 四次（约定俗成的 400ns）。
#[inline]
fn delay_400ns() {
    // SAFETY: 只读状态口，无副作用。
    unsafe {
        for _ in 0..4 {
            let _ = inb(PORT_ALT);
        }
    }
}

fn read_status() -> u8 {
    // SAFETY: 只读状态口。
    unsafe { inb(PORT_STATUS) }
}

fn wait_not_busy(timeout: u32) -> Result<u8, &'static str> {
    let mut st = read_status();
    let mut n = 0u32;
    while st & ST_BSY != 0 {
        n += 1;
        if n > timeout {
            return Err("ATA 一直忙（设备不存在或已挂起）");
        }
        st = read_status();
    }
    Ok(st)
}

/// 16 位端口写（ATA 写数据口用）。
#[inline]
unsafe fn outw(port: u16, v: u16) {
    core::arch::asm!("out dx, ax", in("dx") port, in("ax") v,
        options(nomem, nostack, preserves_flags));
}

/// 一块盘。`slave = false` 是主盘（系统盘），`true` 是从盘（LumiFS 用）。
#[derive(Clone, Copy)]
pub struct Drive {
    pub slave: bool,
}

impl Drive {
    pub const MASTER: Drive = Drive { slave: false };
    pub const SLAVE: Drive = Drive { slave: true };

    /// 选盘用的 0x1F6 基值：bit4=1 选从盘，bit6=1 用 LBA 模式。
    #[inline]
    fn sel(&self) -> u8 {
        (if self.slave { 0xB0 } else { 0xA0 }) | 0x40
    }

    /// 探测这块盘在不在，返回扇区总数。
    ///
    /// 判据是"读状态"：**没有设备的槽位会读到 0x00 或 0xFF**。
    /// 不要靠 IDENTIFY 是否超时判断——那在虚拟机上会等很久。
    pub fn probe(&self) -> Option<u64> {
        // SAFETY: 选盘 + 读状态，不涉及内存安全。
        unsafe {
            outb(PORT_DRIVE, self.sel());
            delay_400ns();
        }
        let st = read_status();
        if st == 0x00 || st == 0xFF {
            return None;
        }
        if wait_not_busy(1_000_000).is_err() {
            return None;
        }

        // IDENTIFY：扇区数在 words[60] | words[61] << 16
        // SAFETY: 端口序列，无内存安全影响。
        unsafe {
            outb(PORT_SECCOUNT, 0);
            outb(PORT_LBA0, 0);
            outb(PORT_LBA1, 0);
            outb(PORT_LBA2, 0);
            outb(PORT_CMD, 0xEC);
        }
        delay_400ns();
        let st = wait_not_busy(1_000_000).ok()?;
        if st & ST_ERR != 0 || st & ST_DRQ == 0 {
            return None;
        }
        let mut words = [0u16; 256];
        for w in words.iter_mut() {
            // SAFETY: DRQ 已置位，数据口可读。
            *w = unsafe { inw(PORT_DATA) };
        }
        let lo = words[60] as u64;
        let hi = words[61] as u64;
        let sectors = lo | (hi << 16);
        if sectors == 0 {
            None
        } else {
            Some(sectors)
        }
    }

    /// 读一个 512 字节扇区（LBA28）。
    pub fn read_sector(&self, lba: u32, out: &mut [u8; 512]) -> Result<(), &'static str> {
        // M10：真机是 SATA/AHCI，没有 0x1F0 那一套。只要 AHCI 接管了，
        // 所有扇区读写都转过去；PIO 代码原封不动留着，作为老机器的兜底。
        // 故障注入点：模拟"盘读坏了"。文件系统必须把它变成 `Err` 逐层上传，
        // 而不是 panic 或者返回一段垃圾数据（后者更危险：上层会当成合法内容解析）。
        if crate::qa::fault::deny(kqa::fault::Point::DiskRead) {
            return Err("故障注入：读扇区失败");
        }
        let lba = lba + lba_off(self.slave);
        if crate::dev::ahci::ready() {
            // 单盘分区布局：数据盘没有独立端口，落在 0 号端口的另一个分区上
            let slot = if self.slave && !crate::dev::ahci::slot_used(1) { 0 } else { self.slave as usize };
            return crate::dev::ahci::read_sector(slot, lba, out);
        }
        let st = wait_not_busy(1_000_000)?;
        if st & ST_DRDY == 0 {
            return Err("ATA 未就绪（DRDY=0）");
        }
        // SAFETY: 端口序列，无内存安全影响。
        unsafe {
            outb(PORT_DRIVE, self.sel() | (((lba >> 24) & 0x0F) as u8));
            outb(PORT_SECCOUNT, 1);
            outb(PORT_LBA0, (lba & 0xFF) as u8);
            outb(PORT_LBA1, ((lba >> 8) & 0xFF) as u8);
            outb(PORT_LBA2, ((lba >> 16) & 0xFF) as u8);
            outb(PORT_CMD, 0x20); // READ SECTORS
        }
        delay_400ns();

        let st = wait_not_busy(1_000_000)?;
        if st & ST_ERR != 0 {
            // SAFETY: 读错误寄存器（读它同时清中断标志）。
            let e = unsafe { inb(PORT_ERR) };
            return Err(if e == 0 {
                "ATA 读失败（ERR 置位但错误码为 0）"
            } else {
                "ATA 读失败（错误寄存器非 0）"
            });
        }
        if st & ST_DF != 0 {
            return Err("ATA 设备故障（DF=1）");
        }
        if st & ST_DRQ == 0 {
            return Err("ATA 数据未就绪（DRQ=0）");
        }
        for i in 0..256 {
            // SAFETY: DRQ 已置位；256 个字 = 512 字节。
            let w = unsafe { inw(PORT_DATA) };
            out[i * 2] = (w & 0xFF) as u8;
            out[i * 2 + 1] = (w >> 8) as u8;
        }
        Ok(())
    }

    /// 写一个 512 字节扇区（LBA28）。
    pub fn write_sector(&self, lba: u32, data: &[u8; 512]) -> Result<(), &'static str> {
        if crate::qa::fault::deny(kqa::fault::Point::DiskWrite) {
            return Err("故障注入：写扇区失败");
        }
        let lba = lba + lba_off(self.slave);
        if crate::dev::ahci::ready() {
            let slot = if self.slave && !crate::dev::ahci::slot_used(1) { 0 } else { self.slave as usize };
            return crate::dev::ahci::write_sector(slot, lba, data);
        }
        let st = wait_not_busy(1_000_000)?;
        if st & ST_DRDY == 0 {
            return Err("ATA 未就绪（DRDY=0）");
        }
        // SAFETY: 端口序列，无内存安全影响。
        unsafe {
            outb(PORT_DRIVE, self.sel() | (((lba >> 24) & 0x0F) as u8));
            outb(PORT_SECCOUNT, 1);
            outb(PORT_LBA0, (lba & 0xFF) as u8);
            outb(PORT_LBA1, ((lba >> 8) & 0xFF) as u8);
            outb(PORT_LBA2, ((lba >> 16) & 0xFF) as u8);
            outb(PORT_CMD, 0x30); // WRITE SECTORS
        }
        delay_400ns();

        let st = wait_not_busy(1_000_000)?;
        if st & ST_ERR != 0 || st & ST_DF != 0 {
            return Err("ATA 写失败（ERR/DF 置位）");
        }
        if st & ST_DRQ == 0 {
            return Err("ATA 写数据未就绪（DRQ=0）");
        }
        for i in 0..256 {
            let w = (data[i * 2] as u16) | ((data[i * 2 + 1] as u16) << 8);
            // SAFETY: DRQ 已置位，数据口可写。
            unsafe { outw(PORT_DATA, w) };
        }
        // 等设备把这一扇区接收完
        let st = wait_not_busy(1_000_000)?;
        if st & ST_ERR != 0 || st & ST_DF != 0 {
            return Err("ATA 写完成时出错");
        }
        Ok(())
    }

    /// FLUSH CACHE：让设备把内部写缓存落到介质上。
    ///
    /// 日志文件系统的顺序完全依赖这条命令：
    /// "先写日志 → flush → 再写数据"里的 flush 如果是假的，
    /// 掉电后数据和日志的顺序就可能反过来，日志等于没写。
    pub fn flush_cache(&self) -> Result<(), &'static str> {
        if crate::dev::ahci::ready() {
            let slot = if self.slave && !crate::dev::ahci::slot_used(1) { 0 } else { self.slave as usize };
            return crate::dev::ahci::flush(slot).map_err(|_| "AHCI FLUSH 失败");
        }
        // SAFETY: 端口序列，无内存安全影响。
        unsafe {
            outb(PORT_DRIVE, self.sel());
            outb(PORT_CMD, 0xE7); // FLUSH CACHE
        }
        delay_400ns();
        let st = wait_not_busy(4_000_000)?;
        if st & ST_ERR != 0 {
            return Err("ATA FLUSH CACHE 失败");
        }
        Ok(())
    }

    fn check_lba(&self, sectors: u64, lba: u64) -> Result<u32, ()> {
        let base = match lba.checked_mul(2) {
            Some(v) if v + 1 < sectors => v,
            _ => return Err(()),
        };
        Ok(base as u32)
    }

    /// 读一个 1 KiB 块（两个相邻扇区）。
    pub fn read_block(&self, sectors: u64, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        if buf.len() < BLOCK_SIZE {
            return Err(());
        }
        let base = self.check_lba(sectors, lba)?;
        let mut sec = [0u8; 512];
        self.read_sector(base, &mut sec).map_err(|_| ())?;
        buf[..512].copy_from_slice(&sec);
        self.read_sector(base + 1, &mut sec).map_err(|_| ())?;
        buf[512..1024].copy_from_slice(&sec);
        Ok(())
    }

    /// 写一个 1 KiB 块。
    ///
    /// 这里**不加** flush：什么时候需要屏障由上层（文件系统）决定。
    /// 驱动自己每写一块就同步全盘，性能会直接崩掉。
    pub fn write_block(&self, sectors: u64, lba: u64, buf: &[u8]) -> Result<(), ()> {
        if buf.len() < BLOCK_SIZE {
            return Err(());
        }
        let base = self.check_lba(sectors, lba)?;
        let mut sec = [0u8; 512];
        sec.copy_from_slice(&buf[..512]);
        self.write_sector(base, &sec).map_err(|_| ())?;
        sec.copy_from_slice(&buf[512..1024]);
        self.write_sector(base + 1, &sec).map_err(|_| ())
    }
}

/* ---------------- 与既有接口的兼容层（主盘只读 + 从盘可写） ---------------- */

/// 系统盘 / 数据盘在物理盘上的起始 LBA（同盘分区时非 0）。
/// BIOS 链为 0（ext2 独占一块盘），UEFI 链从 E820 头部的偏移字段读到。
static SYS_OFF: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static DATA_OFF: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

pub fn set_part_offsets(sys: u32, data: u32) {
    SYS_OFF.store(sys, Ordering::Release);
    DATA_OFF.store(data, Ordering::Release);
}

pub fn part_offsets() -> (u32, u32) {
    (SYS_OFF.load(Ordering::Acquire), DATA_OFF.load(Ordering::Acquire))
}

fn lba_off(slave: bool) -> u32 {
    if slave {
        DATA_OFF.load(Ordering::Acquire)
    } else {
        SYS_OFF.load(Ordering::Acquire)
    }
}

static PRESENT: AtomicBool = AtomicBool::new(false);
static SECTORS: AtomicU64 = AtomicU64::new(0);
static SLAVE_SECTORS: AtomicU64 = AtomicU64::new(0);
static SLAVE_PRESENT: AtomicBool = AtomicBool::new(false);

/// 探测主通道两块盘（主盘 = 系统盘，从盘 = LumiFS 数据盘）。
pub fn probe_all() -> (Option<u64>, Option<u64>) {
    // AHCI 先试：成了就不必再碰 IDE 端口（真机上那些端口根本不存在）
    if crate::dev::ahci::ready() {
        let m = crate::dev::ahci::sectors(0);
        // 数据盘：优先独立端口；没有的话看它是不是 0 号端口上的另一个分区
        let sl = if crate::dev::ahci::slot_used(1) {
            crate::dev::ahci::sectors(1)
        } else if DATA_OFF.load(Ordering::Acquire) != 0 {
            crate::dev::ahci::sectors(0)
        } else {
            0
        };
        if m > 0 {
            PRESENT.store(true, Ordering::Release);
            SECTORS.store(m, Ordering::Release);
        }
        if sl > 0 {
            SLAVE_PRESENT.store(true, Ordering::Release);
            SLAVE_SECTORS.store(sl, Ordering::Release);
        }
        return (if m > 0 { Some(m) } else { None }, if sl > 0 { Some(sl) } else { None });
    }
    let m = Drive::MASTER.probe();
    if let Some(s) = m {
        PRESENT.store(true, Ordering::Release);
        SECTORS.store(s, Ordering::Release);
    }
    let sl = Drive::SLAVE.probe();
    if let Some(s) = sl {
        SLAVE_PRESENT.store(true, Ordering::Release);
        SLAVE_SECTORS.store(s, Ordering::Release);
    }
    (m, sl)
}

/// 探测主盘（保留 M5 的入口）。
pub fn probe() -> Option<u64> {
    if crate::dev::ahci::ready() {
        let s = crate::dev::ahci::sectors(0);
        if s > 0 {
            PRESENT.store(true, Ordering::Release);
            SECTORS.store(s, Ordering::Release);
            return Some(s);
        }
        return None;
    }
    let s = Drive::MASTER.probe()?;
    PRESENT.store(true, Ordering::Release);
    SECTORS.store(s, Ordering::Release);
    Some(s)
}

pub fn present() -> bool {
    PRESENT.load(Ordering::Acquire)
}

pub fn sectors() -> u64 {
    SECTORS.load(Ordering::Acquire)
}

pub fn slave_present() -> bool {
    SLAVE_PRESENT.load(Ordering::Acquire)
}

pub fn slave_sectors() -> u64 {
    SLAVE_SECTORS.load(Ordering::Acquire)
}

/// 读主盘一个 512 字节扇区。
pub fn read_sector(lba: u32, out: &mut [u8; 512]) -> Result<(), &'static str> {
    // **磁盘设备门**（与具体控制器无关）：IDE PIO 和 AHCI 都要先过这里。
    // 之前只把门放在 `ahci::read_sector` 里，BIOS 路径走的是 PIO，
    // 于是"控制器失联"的注入完全不生效——门必须挂在设备级，不是控制器级。
    if !crate::dev::health::is_up(crate::dev::health::Driver::Disk) {
        return Err("磁盘设备已降级");
    }
    if crate::qa::fault::deny(kqa::fault::Point::AhciDown) {
        crate::dev::health::mark_down(
            crate::dev::health::Driver::Disk,
            "注入：磁盘设备失联",
        );
        return Err("故障注入：磁盘设备失联");
    }
    if !present() {
        return Err("没有探测到 ATA 盘");
    }
    Drive::MASTER.read_sector(lba, out)
}

/// 一个"以 1 KiB 为块"的主盘设备，实现 `kfs::BlockDevice`（只读 ext2）。
pub struct AtaDisk;

impl AtaDisk {
    pub const fn new() -> Self {
        AtaDisk
    }
}

impl Default for AtaDisk {
    fn default() -> Self {
        Self::new()
    }
}

impl kfs::BlockDevice for AtaDisk {
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), kfs::IoError> {
        Drive::MASTER
            .read_block(sectors(), lba, buf)
            .map_err(|_| kfs::IoError::DeviceError)
    }

    fn block_count(&self) -> u64 {
        sectors() / 2
    }
}

/// 从盘（LumiFS 用）：实现 `lumifs::Disk`，可读可写。
///
/// 这就是"两块盘各管一件事"的落点：系统盘只读、数据盘可写。
/// 格式化与掉电实验再怎么折腾，也不会把内核自己依赖的那块盘弄坏。
pub struct DataDisk;

impl DataDisk {
    pub const fn new() -> Self {
        DataDisk
    }
}

impl Default for DataDisk {
    fn default() -> Self {
        Self::new()
    }
}

impl lumifs::Disk for DataDisk {
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn block_count(&self) -> u64 {
        slave_sectors() / 2
    }

    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        Drive::SLAVE.read_block(slave_sectors(), lba, buf)
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
        Drive::SLAVE.write_block(slave_sectors(), lba, buf)
    }

    fn flush(&mut self) -> Result<(), ()> {
        Drive::SLAVE.flush_cache().map_err(|_| ())
    }
}
