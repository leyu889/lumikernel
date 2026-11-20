//! SMP：启动应用处理器（AP）（M4）
//!
//! 上电时只有 BSP 在跑，其余核停在"等 INIT-SIPI"的状态。唤醒它们需要三步：
//! 1. 把一段 16 位跳板拷到 0x8000（SIPI 只能从实模式、低 1 MiB 开始执行）；
//! 2. 给目标核发 INIT，等一会儿，再连发两次 SIPI(0x08)；
//! 3. 跳板把核带进长模式，跳到本文件的 [`ap_entry`]。
//!
//! 为什么每个核都要自己的栈、自己的 IDT/GDT、自己的定时器：
//! 这些全是**每 CPU** 的状态（GDTR/IDTR/TR/栈指针都是寄存器级的，不共享）。
//! 共用一张栈会让两个核互相踩栈，症状是随机时刻的诡异崩溃——
//! 而且往往在"跟多核毫无关系"的代码里炸。
//!
//! 数据竞争纪律：一旦有多个核在跑，`ticks`/`online` 这类每核状态**必须**是原子。
//! 用普通 u64 让四个核一起 `+= 1`，在 x86 上大概率"看起来正常"，
//! 但那只是编译器的好意，换个优化等级就丢更新。

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::arch;
use crate::intr::acpi::{Info, MAX_CPUS};
use crate::intr::{apic, idt};
use crate::mm::{frames, paging};

/// 跳板的装载物理地址（必须 4 KiB 对齐，SIPI 的向量就是地址的高位）。
const TRAMP_PHYS: u64 = 0x8000;
/// SIPI 向量 = 地址 >> 12。
const TRAMP_VECTOR: u8 = (TRAMP_PHYS >> 12) as u8;
/// 参数区（跳板从这里读 PML4/入口/栈顶）。
const PARAM_PHYS: u64 = 0x7000;
/// 跳板遥测单元（阶段号 / 被唤醒次数），见 `ap_tramp.S`。
const TELEM_STAGE: u64 = 0x6000;
const TELEM_WAKEUPS: u64 = 0x6010;

/// 每个 AP 的内核栈大小（4 页）。
const AP_STACK_FRAMES: usize = 4;

/// 每核的静态信息。启动后不再变化，因此可以放在普通内存里。
#[derive(Clone, Copy)]
pub struct Cpu {
    pub apic_id: u8,
    pub stack_top: u64,
}

impl Cpu {
    const fn empty() -> Self {
        Cpu {
            apic_id: 0xFF,
            stack_top: 0,
        }
    }
}

struct CpusCell(UnsafeCell<[Cpu; MAX_CPUS]>);
// SAFETY: 写入只发生在 `start_aps`（串行唤醒，每项写完后用 Release 发布 NCPU），
// AP 侧只读；`ticks`/`online`/`work` 全部走原子量，不经过这个结构。
unsafe impl Sync for CpusCell {}
static CPUS: CpusCell = CpusCell(UnsafeCell::new([Cpu::empty(); MAX_CPUS]));

static NCPU: AtomicUsize = AtomicUsize::new(0);
static BSP: AtomicU8 = AtomicU8::new(0);
static TIMER_INIT: AtomicU32 = AtomicU32::new(0);
static AP_ENTERED: AtomicU64 = AtomicU64::new(0);
/// 把每核的计数器各自占满一条 64 字节缓存行。
///
/// 不加这个，`WORK[1..3]` 会落在同一条缓存行上，三个核每加一次就把
/// 这条线在核间来回搬（cache-line ping-pong）——实测差距是 6100 万 vs 20 亿。
/// 那不是"核慢"，是假共享（false sharing）：性能杀手，而且很难从现象上看出来。
#[repr(align(64))]
struct Padded<T>(T);

/// 每核已上线标志（AP 自己置位）。
static ONLINE: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// 每核被自己的定时器打断的次数。
static TICKS: [Padded<AtomicU64>; MAX_CPUS] = [const { Padded(AtomicU64::new(0)) }; MAX_CPUS];
/// 每核在并行度窗口里干的活（自己数）。
static WORK: [Padded<AtomicU64>; MAX_CPUS] = [const { Padded(AtomicU64::new(0)) }; MAX_CPUS];
/// BSP 置位期间，各 AP 全速做无副作用的活。
static AP_BUSY: AtomicBool = AtomicBool::new(false);
/// 收到的核间中断次数。
static IPI_ACKS: AtomicU64 = AtomicU64::new(0);

extern "C" {
    static ap_tramp_start: u8;
    static ap_tramp_end: u8;
}

/// 极简自旋延时（约 10 ms 量级）。只用于 INIT→SIPI 之间的间隔要求，不追求精度。
#[inline]
fn spin_delay() -> bool {
    // 千万别在这里用 `pause`（core::hint::spin_loop）：虚拟机里 pause 可能触发
    // VM exit，一次几微秒，循环几百万次就是几十秒——看起来完全像死机。
    // 用带 black_box 的普通算术循环，既不会被优化掉，也不会陷入。
    let mut x: u32 = 0x9E37_79B9;
    for i in 0..3_000_000u32 {
        x = x.wrapping_mul(1664525).wrapping_add(i ^ x);
    }
    core::hint::black_box(x);
    true
}

fn cpus() -> &'static [Cpu; MAX_CPUS] {
    // SAFETY: 只读访问；每一项在 `NCPU` 发布之前就已写好（Release/Acquire 配对）。
    unsafe { &*CPUS.0.get() }
}

/// 读取跳板写下的遥测：`(最后阶段号, 被唤醒次数)`。
///
/// 走内存而不是串口：AP 和 BSP 共用一个 16550 时字节会互相覆盖，
/// 拿它做字节级探针只会得到假数据（踩过，代价是半小时的错误结论）。
fn telemetry() -> (u8, u32) {
    // SAFETY: 两个约定单元，经直接映射区读取。
    unsafe {
        (
            core::ptr::read_volatile(paging::phys_to_virt(TELEM_STAGE) as *const u8),
            core::ptr::read_volatile(paging::phys_to_virt(TELEM_WAKEUPS) as *const u32),
        )
    }
}

fn telemetry_reset() {
    // SAFETY: 同上；只在串行启动 AP 时使用。
    unsafe {
        core::ptr::write_volatile(paging::phys_to_virt(TELEM_STAGE) as *mut u8, 0);
        core::ptr::write_volatile(paging::phys_to_virt(TELEM_WAKEUPS) as *mut u32, 0);
    }
}

/// 按 APIC ID 找 CPU 下标。
pub fn index_of(apic_id: u8) -> Option<usize> {
    let n = NCPU.load(Ordering::Acquire);
    cpus().iter().take(n).position(|c| c.apic_id == apic_id)
}

/// 当前核的下标。
pub fn me() -> Option<usize> {
    index_of(apic::local_id())
}

/// 当前核是不是 BSP。
pub fn is_bsp() -> bool {
    apic::local_id() == BSP.load(Ordering::Relaxed)
}

/// 每核 tick 计数（由定时器中断调用，四个核都会进来）。
pub fn note_tick() {
    if let Some(i) = me() {
        TICKS[i].0.fetch_add(1, Ordering::Relaxed);
    }
}

/// 收到一次核间中断（由 `intr::handler` 分发）。
pub fn note_ipi() {
    IPI_ACKS.fetch_add(1, Ordering::SeqCst);
}

/// 已上线的核数。
pub fn online() -> usize {
    let n = NCPU.load(Ordering::Acquire);
    (0..n).filter(|&i| ONLINE[i].load(Ordering::Acquire)).count()
}

/// 第 i 个核的 tick 数。
pub fn ticks_of(i: usize) -> u64 {
    TICKS.get(i).map(|t| t.0.load(Ordering::Relaxed)).unwrap_or(0)
}

/// 第 i 个核在并行度窗口里干的活。
pub fn work_of(i: usize) -> u64 {
    WORK.get(i).map(|w| w.0.load(Ordering::Relaxed)).unwrap_or(0)
}

/// 第 i 个核的 APIC ID。
pub fn apic_id_of(i: usize) -> u8 {
    cpus().get(i).map(|c| c.apic_id).unwrap_or(0xFF)
}

/// 第 i 个核的内核栈顶（诊断用）。
pub fn stack_top_of(i: usize) -> u64 {
    cpus().get(i).map(|c| c.stack_top).unwrap_or(0)
}

/// 已登记的核数（含 BSP）。
pub fn count() -> usize {
    NCPU.load(Ordering::Acquire)
}

/// 开/关 AP 的干活窗口。
///
/// 打开时顺便广播一次 IPI：AP 平时停在 `hlt` 里，只能等自己 10 ms 一次的
/// 定时中断醒来——如果它刚在 BUSY 置位之前又睡下去，就要再等一个 tick 才动。
/// 实测这会偶发"窗口内某个核一次活都没干"。这也正是 reschedule IPI 的用途：
/// **有活要干，就戳它一下**，别指望它自己醒。
pub fn set_busy(on: bool) {
    AP_BUSY.store(on, Ordering::SeqCst);
    if on && count() > 1 {
        apic::send_ipi_all_but_self(idt::IPI_VEC as u8);
    }
}

/// 广播一次核间中断给所有其他核，并等所有 AP 都应答；返回实际应答数。
///
/// 这是多核调度（reschedule IPI）与 TLB shootdown 的底座：
/// 没有 IPI，一个核永远不知道另一个核改了页表、或者往它的运行队列里塞了任务。
pub fn broadcast_ipi_and_wait() -> u64 {
    let before = IPI_ACKS.load(Ordering::SeqCst);
    apic::send_ipi_all_but_self(idt::IPI_VEC as u8);
    let want = count().saturating_sub(1) as u64;
    let mut spins = 0u64;
    while IPI_ACKS.load(Ordering::SeqCst) - before < want {
        let mut x = spins as u32;
        x = x.wrapping_mul(1664525).wrapping_add(1);
        core::hint::black_box(x);
        spins += 1;
        if spins > 50_000_000 {
            break;
        }
    }
    IPI_ACKS.load(Ordering::SeqCst) - before
}

/// 登记 BSP，并把 APIC 定时器的初值记下来给 AP 复用。
pub fn init_bsp(timer_init: u32) {
    let id = apic::local_id();
    BSP.store(id, Ordering::Relaxed);
    TIMER_INIT.store(timer_init, Ordering::Relaxed);
    // SAFETY: 启动阶段只有 BSP 在跑，独占写入。
    unsafe {
        (*CPUS.0.get())[0] = Cpu {
            apic_id: id,
            stack_top: 0,
        };
    }
    ONLINE[0].store(true, Ordering::Release);
    NCPU.store(1, Ordering::Release);
}

/// 把跳板拷到 0x8000，并为每个 AP 发 INIT-SIPI。返回成功上线的 AP 数。
pub fn start_aps(info: &Info) -> usize {
    if info.cpu_count <= 1 {
        return 0;
    }

    let src = core::ptr::addr_of!(ap_tramp_start) as u64;
    let dst = core::ptr::addr_of!(ap_tramp_end) as u64;
    let len = dst - src;
    if len == 0 || len > 4096 {
        return 0;
    }
    // SAFETY: 源是链接器圈定的 .ap_tramp 段；目标是约定的实模式落点 0x8000，
    // 经直接映射区写入，长度已判界，两块内存不重叠。
    unsafe {
        core::ptr::copy_nonoverlapping(
            src as *const u8,
            paging::phys_to_virt(TRAMP_PHYS) as *mut u8,
            len as usize,
        );
    }

    let mut started = 0usize;
    for k in 1..info.cpu_count.min(MAX_CPUS) {
        let apic_id = info.apic_ids[k];
        if apic_id == 0xFF || apic_id == BSP.load(Ordering::Relaxed) {
            continue;
        }
        if index_of(apic_id).is_some() {
            continue;
        }

        let slot = NCPU.load(Ordering::Acquire);
        if slot >= MAX_CPUS {
            break;
        }
        // 给这个 AP 一张自己的栈
        let Some(phys) = frames::alloc_frames(AP_STACK_FRAMES) else {
            break;
        };
        let stack_top = paging::phys_to_virt(phys + (AP_STACK_FRAMES * 4096) as u64);

        // 预登记（online=false），等 AP 自己置位
        // SAFETY: 串行启动，此时只有 BSP 在写这一项。
        unsafe {
            (*CPUS.0.get())[slot] = Cpu { apic_id, stack_top };
        }
        NCPU.store(slot + 1, Ordering::Release);

        // 填参数区
        // SAFETY: 0x7000 是约定的参数块；每次只服务一个 AP（串行启动）。
        unsafe {
            let p = paging::phys_to_virt(PARAM_PHYS) as *mut u64;
            core::ptr::write_volatile(p, arch::gdt::pml4_phys());
            core::ptr::write_volatile(p.add(1), ap_entry as *const () as u64);
            core::ptr::write_volatile(p.add(2), stack_top);
            core::ptr::write_volatile(p.add(3) as *mut u32, apic_id as u32);
        }

        telemetry_reset();
        apic::send_init(apic_id);
        // INIT 之后必须等至少 10 ms 再发 SIPI（ACPI 规范要求）
        spin_delay();
        apic::send_startup(apic_id, TRAMP_VECTOR);
        spin_delay();
        // 规范建议连发两次 SIPI：第一次可能丢
        apic::send_startup(apic_id, TRAMP_VECTOR);

        // 等它上线（最多 ~200 ms）
        let mut ok = false;
        for _ in 0..20 {
            if ONLINE[slot].load(Ordering::Acquire) {
                ok = true;
                break;
            }
            spin_delay();
        }
        // stage 8 = 跳板全程走完、已经跳进 Rust；wakeups > 1 说明它中途死过
        let (stage, wakeups) = telemetry();
        crate::kserialln!(
            "       AP#{} apic={} stack={:#x} online={} stage={} wakeups={}",
            k,
            apic_id,
            stack_top,
            ok,
            stage,
            wakeups
        );
        if ok {
            started += 1;
        }
    }
    started
}

/// AP 的 C 入口。运行在长模式、共享页表、自有栈上。
///
/// 这里**不能返回**：返回等于跳到垃圾地址。做完初始化就进 idle 循环。
#[no_mangle]
pub extern "C" fn ap_entry() -> ! {
    AP_ENTERED.fetch_add(1, Ordering::Relaxed);

    // 每 CPU 的寄存器状态必须自己装：GDTR/IDTR 都不是共享的
    arch::gdt::reload();
    idt::reload();

    // 自己的 LAPIC + 定时器（BSP 已经校准过初值，直接复用）
    apic::ap_start(TIMER_INIT.load(Ordering::Relaxed));

    let idx = me();
    if let Some(i) = idx {
        ONLINE[i].store(true, Ordering::Release);
    }

    // 开中断，进 idle：只有中断会把它叫醒
    // SAFETY: sti 只影响本核的标志寄存器。
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
    }
    loop {
        // BSP 打开窗口时全速干活——这是"多个核真的在同时执行"的直接证据
        if let Some(i) = idx {
            while AP_BUSY.load(Ordering::Acquire) {
                WORK[i].0.fetch_add(1, Ordering::Relaxed);
            }
        }
        // SAFETY: hlt 等下一个中断（定时器或 IPI）。
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

// `task::ap_scheduler_start`（AP 跑真调度循环）已实现但**未启用**：
// 8 核并发时在 execve 密集路径静默卡死，并发点不止一处（每核 TSS/GDT、
// TASKS 表跨核访问）。等 per-CPU TSS 完成后按核打开。
