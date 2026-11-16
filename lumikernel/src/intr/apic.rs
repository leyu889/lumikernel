//! 本地 APIC + I/O APIC + APIC 定时器（M4）
//!
//! 为什么要换掉 8259A/PIT：
//! - 8259A 只有 15 条线、只连一个核，多核根本没得谈；
//! - PIT 是全局单一定时器，四个核抢一个中断源，做不出"每个核自己的时间片"。
//!
//! 换成 APIC 之后：每个核有自己的 LAPIC（有自己的定时器、自己的中断入口、能发 IPI
//! 打断别的核），I/O APIC 负责把外部设备的中断路由到指定核上。
//!
//! 地址：LAPIC/IOAPIC 的 MMIO 在 4 GiB 附近（0xFEE00000 / 0xFEC00000），
//! 不在恒等映射的 1 GiB 里，所以这里先把它们映射进 vmalloc 区，并且**必须**带
//! cache_disable —— 设备寄存器被 CPU 缓存住的话，读写会"看起来没生效"。

use core::ptr::{read_volatile, write_volatile};

use kvm::Flags;

use crate::mm::paging;
use crate::mm::VMALLOC_BASE;

const LAPIC_VA: u64 = VMALLOC_BASE + 0x20_0000;
const IOAPIC_VA: u64 = VMALLOC_BASE + 0x21_0000;

const LAPIC_PHYS: u64 = 0xFEE0_0000;

/// MMIO 用的页标志：可写 + 不缓存。
const MMIO: Flags = Flags {
    present: true,
    writable: true,
    user: false,
    executable: false,
    cache_disable: true,
};

/* ---------------- LAPIC 寄存器偏移 ---------------- */

const REG_ID: u64 = 0x020;
const REG_EOI: u64 = 0x0B0;
const REG_SVR: u64 = 0x0F0;
const REG_ICR_LOW: u64 = 0x300;
const REG_ICR_HIGH: u64 = 0x310;
const REG_LVT_TIMER: u64 = 0x320;
const REG_TIMER_INIT: u64 = 0x380;
const REG_TIMER_CUR: u64 = 0x390;
const REG_TIMER_DIV: u64 = 0x3E0;

/// 伪中断向量（SVR 低 4 位必须非零）。
const SPURIOUS_VEC: u32 = 0xFF;
/// 定时器向量：和传统 IRQ0 共用 32，处理路径一致。
pub const TIMER_VEC: u64 = 32;

struct Lapic {
    base: u64,
}

impl Lapic {
    #[inline]
    fn read(&self, off: u64) -> u32 {
        // SAFETY: MMIO 页已映射且不可缓存；偏移都是 16 字节对齐的寄存器。
        unsafe { read_volatile((self.base + off) as *const u32) }
    }
    #[inline]
    fn write(&self, off: u64, v: u32) {
        // SAFETY: 同上。
        unsafe { write_volatile((self.base + off) as *mut u32, v) }
    }
}

static mut LAPIC_BASE: u64 = 0;
/// BSP 校准出来的定时器初值（一个周期 = 10 ms）。
static TIMER_INIT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// 校准那一秒里，原来的 tick 源实际打了多少次（用来核对它是不是 100 Hz）。
static REF_HZ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// 10 ms 对应多少个 TSC tick（校准 APIC 定时器时顺手量出来的）。
static TSC_PER_10MS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// 读时间戳计数器。
#[inline]
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: rdtsc 只读计数器，无副作用。lfence 保证不乱序。
    unsafe {
        core::arch::asm!("lfence", "rdtsc", out("eax") lo, out("edx") hi, options(nostack, preserves_flags));
    }
    ((hi as u64) << 32) | lo as u64
}

fn lapic() -> Lapic {
    // SAFETY: 单次写入的常量，读回来是 u64，没有别名问题。
    Lapic {
        base: unsafe { LAPIC_BASE },
    }
}

/// 当前核的 APIC ID。
pub fn local_id() -> u8 {
    (lapic().read(REG_ID) >> 24) as u8
}

/// 发 EOI。APIC 模式下必须写 LAPIC 的 EOI 寄存器，写 8259A 的端口是没用的。
pub fn eoi() {
    lapic().write(REG_EOI, 0);
}

/* ---------------- I/O APIC ---------------- */

struct IoApic {
    base: u64,
}

impl IoApic {
    fn read(&self, reg: u32) -> u32 {
        // SAFETY: IOREGSEL/IOWIN 是标准两寄存器窗口。
        unsafe {
            write_volatile(self.base as *mut u32, reg);
            read_volatile((self.base + 0x10) as *const u32)
        }
    }
    fn write(&self, reg: u32, v: u32) {
        // SAFETY: 同上。
        unsafe {
            write_volatile(self.base as *mut u32, reg);
            write_volatile((self.base + 0x10) as *mut u32, v);
        }
    }
}

static mut IOAPIC_BASE: u64 = 0;
/// I/O APIC 的 GSI 基址（重定向表第 0 项对应的全局中断号）。
static mut GSI_BASE: u32 = 0;

/// 设置 GSI 基址（初始化时调一次）。
pub fn set_gsi_base(v: u32) {
    // SAFETY: 启动阶段单核写常量。
    unsafe { GSI_BASE = v };
}

fn ioapic() -> IoApic {
    IoApic {
        base: unsafe { IOAPIC_BASE },
    }
}

/// 配一条重定向表项。`gsi` 是**全局**中断号，要减掉 I/O APIC 的 GSI 基址。
pub fn route_gsi(gsi: u32, vector: u8, dest: u8, level: bool, active_low: bool, masked: bool) {
    // SAFETY: 启动阶段已写入，之后只读。
    let base_gsi = unsafe { GSI_BASE };
    let idx = gsi.saturating_sub(base_gsi);
    let io = ioapic();
    let mut lo = vector as u32;
    if level {
        lo |= 1 << 15; // 电平触发
    }
    if active_low {
        lo |= 1 << 13; // 低电平有效
    }
    if masked {
        lo |= 1 << 16;
    }
    io.write(0x10 + idx * 2, lo);
    io.write(0x10 + idx * 2 + 1, (dest as u32) << 24);
}

/* ---------------- 初始化 ---------------- */

pub struct Report {
    pub apic_id: u8,
    pub lapic_addr: u64,
    pub ioapic_addr: u64,
    pub cpu_count: usize,
    pub timer_gsi: u32,
}

/// 映射并启用 LAPIC，配好 I/O APIC（定时器那条先屏蔽，等自检时再放行）。
pub fn init(lapic_phys: u32, ioapic_phys: u32, cpu_count: usize, timer_gsi: u32) -> Result<Report, &'static str> {
    let lp = if lapic_phys == 0 {
        LAPIC_PHYS
    } else {
        lapic_phys as u64
    };
    // 诊断：把此刻的帧余量记下来，失败时能分清"没帧"还是"页表出错"
    let free_before = crate::mm::frames::stats().map(|s| s.free_frames).unwrap_or(999_999);
    paging::map_in_current(LAPIC_VA, lp, MMIO).map_err(move |e| match e {
        kpt::PtError::OutOfMemory if free_before < 1000 => "LAPIC 映射: 帧不够",
        kpt::PtError::OutOfMemory => "LAPIC 映射: alloc_frame 返回 None（帧明明够）",
        kpt::PtError::AlreadyMapped => "LAPIC 映射: 地址已被映射",
        kpt::PtError::NonCanonical => "LAPIC 映射: 非规范地址",
        kpt::PtError::NotAligned => "LAPIC 映射: 地址未对齐",
        kpt::PtError::HugePage => "LAPIC 映射: 撞上大页",
        kpt::PtError::NotPresent => "LAPIC 映射: present=0",
        kpt::PtError::NotMapped => "LAPIC 映射: 未映射",
    })?;
    // SAFETY: 单核启动阶段写这个全局。
    unsafe { LAPIC_BASE = LAPIC_VA };

    // 硬件使能位在 IA32_APIC_BASE 的 bit11（有些固件默认关掉）
    let mut lo: u32;
    let mut hi: u32;
    // SAFETY: rdmsr/wrmsr 是特权指令，参数固定。
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") 0x1Bu32, out("eax") lo, out("edx") hi);
    }
    if lo & (1 << 11) == 0 {
        lo |= 1 << 11;
        // SAFETY: 同上。
        unsafe {
            core::arch::asm!("wrmsr", in("ecx") 0x1Bu32, in("eax") lo, in("edx") hi);
        }
    }

    // SVR：bit8 = 软件使能，低 4 位是伪中断向量（必须非零）
    lapic().write(REG_SVR, 0x100 | SPURIOUS_VEC);

    // 分频 16（校准和运行用同一个分频，数值才有可比性）
    lapic().write(REG_TIMER_DIV, 0b1011);

    let mut rep = Report {
        apic_id: local_id(),
        lapic_addr: LAPIC_VA,
        ioapic_addr: 0,
        cpu_count,
        timer_gsi,
    };

    if ioapic_phys != 0 {
        paging::map_in_current(IOAPIC_VA, ioapic_phys as u64, MMIO)
            .map_err(|_| "IOAPIC 映射失败")?;
        // SAFETY: 同上。
        unsafe { IOAPIC_BASE = IOAPIC_VA };
        // 先全部屏蔽，避免上电遗留的配置突然打进来
        let max = ((ioapic().read(0x01) >> 16) & 0xFF) + 1;
        for i in 0..max {
            ioapic().write(0x10 + i * 2, (1 << 16) | 0xFF);
        }
        rep.ioapic_addr = IOAPIC_VA;
    }

    Ok(rep)
}

/// 放行定时器那条外部中断（走 I/O APIC）。
pub fn unmask_timer(info: &super::acpi::Info, dest: u8, gsi: u32) {
    route_gsi(
        gsi,
        TIMER_VEC as u8,
        dest,
        info.timer_level,
        info.timer_active_low,
        false,
    );
}

/// 屏蔽定时器那条外部中断。
pub fn mask_timer(info: &super::acpi::Info, gsi: u32) {
    route_gsi(
        gsi,
        TIMER_VEC as u8,
        local_id(),
        info.timer_level,
        info.timer_active_low,
        true,
    );
}

/* ---------------- APIC 定时器 ---------------- */

/// 校准并启动 APIC 定时器，目标是 [`super::timer::HZ`]。
///
/// 原理：把 APIC 定时器设成最大计数开始往下跑，同时用 PIT 精确等 10 ms，
/// 看它掉了多少 —— 这个数就是"10 ms 内的 APIC tick 数"，也就是 100 Hz 的初值。
pub fn timer_start(hz: u32) -> Result<u32, &'static str> {
    // 基准必须独立于被测时钟。踩过两次循环依赖之后，这里用 CMOS RTC 的秒边界：
    //   1) 用 PIT 通道 2 轮询当 10 ms —— 实际 71 ms，定时器被校成 14 Hz；
    //   2) 用"PIT 的 10 个中断"当 100 ms —— 实测仍只有 24 Hz，
    //      说明 PIT 这条链路的真实频率也不是 100 Hz。
    // RTC 是另一块晶振，跟 APIC 定时器没有共享假设，拿它当尺子才不是自证。
    let l = lapic();
    l.write(REG_TIMER_INIT, 0xFFFF_FFFF); // 先自由下跑

    if !super::rtc::wait_edge() {
        return Err("RTC 秒边界等不到（CMOS 不可用）");
    }
    let before = l.read(REG_TIMER_CUR);
    let tsc0 = rdtsc();
    let ticks0 = super::timer::ticks();
    // 校准期间必须有中断在走（此时 tick 来自 PIT），否则无法记录参考源频率
    super::enable();
    let ok = super::rtc::wait_edge();
    super::disable();
    let after = l.read(REG_TIMER_CUR);
    let tsc1 = rdtsc();
    let ticks1 = super::timer::ticks();
    if !ok {
        return Err("RTC 秒边界等不到（第二次）");
    }

    let lapic_per_s = before.wrapping_sub(after) as u64;
    // 一秒至少上百万个计数，否则说明 APIC 定时器根本没在跑
    if lapic_per_s < 1_000_000 {
        return Err("APIC 定时器未计数");
    }
    let per_10ms = (lapic_per_s / 100) as u32;
    if tsc1 > tsc0 {
        TSC_PER_10MS.store((tsc1 - tsc0) / 100, core::sync::atomic::Ordering::Relaxed);
    }
    REF_HZ.store(ticks1 - ticks0, core::sync::atomic::Ordering::Relaxed);

    let init = per_10ms.saturating_mul(100 / hz.max(1));
    if init < 1_000 {
        return Err("APIC 定时器频率异常");
    }
    TIMER_INIT.store(init, core::sync::atomic::Ordering::Relaxed);

    // 周期模式：LVT = 向量 | mode=01（periodic）
    l.write(REG_LVT_TIMER, TIMER_VEC as u32 | (1 << 17));
    l.write(REG_TIMER_INIT, init);
    Ok(init)
}

/// 读时间戳计数器。用它做"固定物理时长"的窗口，比用 tick 可靠：
/// tick 的频率本身就是要被验证的量，拿它当尺子等于用被测物量自己。
#[inline]
pub fn tsc_now() -> u64 {
    rdtsc()
}

/// 校准得到的 10 ms TSC 数（诊断用）。
pub fn tsc_per_10ms() -> u64 {
    TSC_PER_10MS.load(core::sync::atomic::Ordering::Relaxed)
}

/// 校准那一秒内原 tick 源的实际频率（诊断用）。
pub fn ref_hz() -> u64 {
    REF_HZ.load(core::sync::atomic::Ordering::Relaxed)
}

/// 校准得到的定时器初值（一个周期）。
pub fn timer_init() -> u32 {
    TIMER_INIT.load(core::sync::atomic::Ordering::Relaxed)
}

/// AP 侧的启动：只开自己的 LAPIC 和定时器（不复用 BSP 的校准流程）。
///
/// `init` 是 BSP 校准出来的初值 —— APIC 定时器的频率在同一个平台上是一致的，
/// 让每个核都去跑一遍 PIT 校准既慢又没必要。
pub fn ap_start(init: u32) {
    let l = lapic();
    l.write(REG_SVR, 0x100 | SPURIOUS_VEC);
    l.write(REG_TIMER_DIV, 0b1011);
    if init > 0 {
        l.write(REG_LVT_TIMER, TIMER_VEC as u32 | (1 << 17));
        l.write(REG_TIMER_INIT, init);
    }
}


/* ---------------- IPI ---------------- */

fn wait_icr() {
    let l = lapic();
    let mut guard = 0u32;
    while l.read(REG_ICR_LOW) & (1 << 12) != 0 {
        guard += 1;
        // 投递在正常情况下是微秒级；这个上限只是防止硬件彻底卡死。
        // 注意：虚拟机的 MMIO 读可能慢到微秒量级，上限设太大会让"等"变成"卡"。
        if guard > 20_000 {
            return;
        }
        core::hint::spin_loop();
    }
}

fn send_ipi(dest: u8, low: u32) {
    let l = lapic();
    wait_icr();
    l.write(REG_ICR_HIGH, (dest as u32) << 24);
    l.write(REG_ICR_LOW, low);
    wait_icr();
}

/// 给目标核发 INIT。
pub fn send_init(dest: u8) {
    // 000 模式 = 固定投递? 不：101 = INIT，level assert(bit14) + trigger(bit15)=1
    send_ipi(dest, 0b101 << 8 | (1 << 14) | (1 << 15));
}

/// 给目标核发 STARTUP（SIPI），`vector` 即目标物理页（vector << 12）。
///
/// 注意：SIPI **不能**置 level（bit14）。INIT 要求 level assert，
/// 但 SIPI 是边沿触发的；带了 level 位某些实现会把 ICR 的投递状态一直挂着。
pub fn send_startup(dest: u8, vector: u8) {
    send_ipi(dest, (0b110 << 8) | (vector as u32));
}

/// 给所有其他核发一个固定向量中断（M4 只用来做 liveness 探针）。
pub fn send_ipi_all_but_self(vector: u8) {
    let l = lapic();
    wait_icr();
    // 目的简写 11 = all excluding self
    l.write(REG_ICR_LOW, (0b11 << 18) | (vector as u32));
    wait_icr();
}
