//! 任务、上下文切换与调度（M3）
//!
//! 核心设计只有一条：**统一走中断返回路径**。
//!
//! - 每个任务有自己的一张内核栈；
//! - 一个任务"被切走"时，它的内核栈上留着从 `schedule()` 一路到中断帧的完整链路；
//! - 一个新任务的栈由 [`build_stack`] 手工拼成 `[6 个 callee-saved][trap_return][InterruptFrame]`，
//!   于是它第一次被调度时，`switch_to` 的 `ret` 直接跳到 `trap_return`，
//!   和"被抢占后恢复"走的是同一条 `pop / iretq` 路径。
//!
//! 这样做的好处：内核线程（落回 ring0）和用户任务（`iretq` 到 ring3）用同一套代码，
//! 不需要两套切换逻辑，也就少了一整类"只在一半路径上才出错"的 bug。
//!
//! 抢占的边界（重要）：内核里有几处不能被打断——页帧分配、堆分配、故障探针。
//! 它们都用 [`preempt_disable`] 包住。单核上没有"另一个 CPU 同时在改"，只有
//! "切走之后再也不回来拿锁"，所以计数器就够了，不需要真自旋锁。

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use ksched::Scheduler;

use crate::arch::gdt;
use crate::arch::switch_to;
use crate::intr::frame::InterruptFrame;
use crate::intr::timer;
use crate::mm::{frames, paging};

// 用户态小程序：随内核一起编译，`setup_user_image` 再把它拷到用户页里。
core::arch::global_asm!(include_str!("../user.S"), options(att_syntax));

/// 任务表容量。够 M3 用；扩到 4K 只是改个常量，但 .bss 会变大。
pub const MAX_TASKS: usize = 32;

/// 时间片（ticks）。100 Hz 下 1 个 tick = 10 ms。
///
/// 用 1 而不是 2：M3 的自检要证明"时间片耗尽会强制切换"，
/// 时间片越短，观察窗口里发生的抢占次数越多，证据越硬、越不容易受计时抖动影响。
pub const QUANTUM: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Free,
    Ready,
    Running,
    /// 在某个等待队列上睡觉（M7）。它**不在**运行队列里，不会被挑中，
    /// 也不会消耗时间片——只有 [`wake`] 能把它叫醒。
    Blocked,
    Zombie,
}

#[derive(Clone, Copy)]
pub struct Task {
    pub state: State,
    pub tid: u64,
    /// 内核栈顶（`TSS.rsp0` 指向它）。
    pub kstack_top: u64,
    /// 内核栈的物理基址（回收时要整块还给帧分配器）。0 = 静态栈，不能还。
    pub kstack_base: u64,
    /// 保存的栈指针（指向 `switch_to` 压下的 6 个 callee-saved）。
    pub kstack: u64,
    /// 是否是用户任务（决定 `iretq` 返回哪个环）。
    pub user: bool,
    pub exit_code: i64,
    /// 累计被时钟中断记到的 tick 数。
    pub ticks: u64,
    /// 该任务的地址空间（PML4 物理地址）。0 表示"用内核那张表"。
    pub pml4: u64,
    /// 每进程文件描述符表（M7）。M5/M6 时它是全局的，注释里就写了迟早要挪。
    pub fds: [crate::fd::File; crate::fd::MAX_FD],

    /* ---- M9：Linux 进程语义 ---- */
    /// `arch_prctl(ARCH_SET_FS)` 设的 TLS 基址。上下文切换时要跟着换 MSR。
    pub fs_base: u64,
    /// 当前工作目录（绝对路径，不带结尾斜杠）。`getcwd` / 相对路径解析用它。
    pub cwd: [u8; CWD_MAX],
    pub cwd_len: u8,
    /// `brk(2)` 的当前值与起点（起点 = ELF 装载结束处）。
    pub brk: u64,
    pub brk_start: u64,
    /// 匿名 `mmap` 的分配指针，从高往低长。
    pub mmap_top: u64,
    /// 父进程 tid（`getppid` 用）。0 表示没有父进程。
    pub parent: u64,
    /// 任务归属的调度核。wake 时按它回原队列（而不是全局抢 CPU0）。
    pub sched_cpu: u8,

    /* ---- M12：匿名 mmap 的区域表（懒分配的判据） ---- */
    /// 已登记的匿名映射区间。页错误发生时靠它区分
    /// "合法的按需分配"和"用户程序的野指针"。
    ///
    /// 为什么不用类似 Linux 的红黑树：匿名 mmap 的区间个数在真实程序里
    /// 是个位数（musl 的 malloc 一次要 32 MiB，glibc 要几个 arena），
    /// 线性扫描比维护树更省事，也更容易审计。
    pub vmas: [Vma; VMA_MAX],
    pub vma_count: u8,
}

/// 一段匿名映射。
#[derive(Clone, Copy)]
pub struct Vma {
    pub start: u64,
    /// 不含（开区间右端）。
    pub end: u64,
    /// `PROT_READ=1` / `PROT_WRITE=2` / `PROT_EXEC=4`，与 Linux 同值。
    pub prot: u32,
    pub used: bool,
}

impl Vma {
    pub const FREE: Vma = Vma { start: 0, end: 0, prot: 0, used: false };
}

/// 每个进程最多记多少段匿名映射。
///
/// 24 段 × 24 字节 = 576 字节/任务。任务表 64 项，总共 36 KiB（BSS）。
/// 真不够时 `mmap` 会退化成"立即分配"，不会失败——见 `linux/mem.rs`。
pub const VMA_MAX: usize = 24;

/// 工作目录缓冲长度。Linux 的 `PATH_MAX` 是 4096，但对一个只会
/// `cd /` 或 `cd /bin` 的 shell，128 字节足够，且不会把 `Task` 撑得太大。
pub const CWD_MAX: usize = 128;

impl Task {
    pub const fn empty() -> Self {
        Task {
            state: State::Free,
            tid: 0,
            kstack_top: 0,
            kstack_base: 0,
            kstack: 0,
            user: false,
            exit_code: 0,
            ticks: 0,
            pml4: 0,
            fds: [crate::fd::FREE; crate::fd::MAX_FD],
            fs_base: 0,
            cwd: [0u8; CWD_MAX],
            cwd_len: 1,
            brk: 0,
            brk_start: 0,
            mmap_top: 0,
            parent: 0,
            vmas: [Vma::FREE; VMA_MAX],
            vma_count: 0,
            sched_cpu: 0,
        }
    }
}

struct TableCell(UnsafeCell<[Task; MAX_TASKS]>);
// SAFETY: 单核，所有访问都在中断关闭或 `preempt_disable` 区间内。
unsafe impl Sync for TableCell {}
static TASKS: TableCell = TableCell(UnsafeCell::new([Task::empty(); MAX_TASKS]));

/// 调度器放在静态区里**就地**初始化。
///
/// 不能用 `Scheduler::new()` 再赋值：那会先在栈上造一份 ~128 KiB 的临时值，
/// 内核栈只有 64 KiB，直接爆栈（踩过一次，症状是 slab 检查通过之后
/// 莫名其妙地写到一个野地址）。`.bss` 本来就全零，而 `Scheduler` 全是整数，
/// 全零位型合法，所以拿 `MaybeUninit` 原地初始化是安全的。
struct SchedCell(UnsafeCell<MaybeUninit<Scheduler>>);
// SAFETY: 同 `TASKS`。
unsafe impl Sync for SchedCell {}
static SCHED: SchedCell = SchedCell(UnsafeCell::new(MaybeUninit::uninit()));

/// 取调度器。只有 `init` 成功之后才允许调用。
fn sched() -> &'static mut Scheduler {
    // SAFETY: `init` 已用 `init_in_place` 把每个字段写了一遍；
    // 单核，调用点都在关中断或 `preempt_disable` 区间内。
    unsafe { (*SCHED.0.get()).assume_init_mut() }
}

/// 每个 CPU 当前运行的任务槽位。
///
/// 之前是单个 `AtomicUsize`——那是"所有任务都在 BSP 上跑"时代的产物。
/// AP 开始跑调度循环后，两核各自有不同的"当前任务"，一个全局量会让
/// AP 上的内核线程 `exit_current` 去改 BSP 正在用的槽位（灾难）。
///
/// 索引用 `arch::smp::me()`：AP 进调度循环前 LAPIC 已初始化，能拿到自己的号。
static CUR: [AtomicUsize; crate::intr::acpi::MAX_CPUS] =
    [const { AtomicUsize::new(0) }; crate::intr::acpi::MAX_CPUS];
static NEXT_TID: AtomicU64 = AtomicU64::new(1);
static SWITCHES: AtomicU64 = AtomicU64::new(0);
static PREEMPT_OFF: AtomicU32 = AtomicU32::new(0);
static QUANTUM_LEFT: AtomicU32 = AtomicU32::new(QUANTUM);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// 保护调度器内部结构的跨核自旋锁。
///
/// 临界区纪律：**`switch_to` 之前必须释放**。切换本身不在锁内——
/// 被切出的任务恢复执行时会从 `switch_to` 返回，续跑本函数的剩余部分，
/// 那时锁早已放开；两核交替拿锁也不会互相困死。
///
/// `pick` 之后任务已被摘出队列并标记 RUNNING，所以释放锁到
/// 真正切换之间不存在"别人也选中它"的窗口。
static SCHED_LOCK: crate::arch::sync::IrqSpinlock<()> =
    crate::arch::sync::IrqSpinlock::new(());

/// 保护 `EXIT_WAIT`（谁在等谁退出）。同样不得在临界区内切换任务。
static WAIT_LOCK: crate::arch::sync::IrqSpinlock<()> =
    crate::arch::sync::IrqSpinlock::new(());
static EXITED: AtomicU64 = AtomicU64::new(0);
/// 因**时间片耗尽**而发生的切换次数。这是抢占的直接证据：
/// `switches` 里还混着"任务自己退出导致的切换"，不能用来证明抢占。
static PREEMPTIONS: AtomicU64 = AtomicU64::new(0);
/// 被唤醒的次数（阻塞 → 就绪）。
static WAKED: AtomicU64 = AtomicU64::new(0);
/// 被回收的任务数。
static REAPED: AtomicU64 = AtomicU64::new(0);
/// 内核栈保留的帧数（与 `alloc_kstack` 保持一致）。
const KSTACK_FRAMES: usize = 4;

extern "C" {
    fn trap_return();
    static kstack0_top: u8;
    static __user_start: u8;
    static __user_end: u8;
}

/* ---------------- 抢占开关 ---------------- */

/// 关抢占。与 [`preempt_enable`] 成对使用，允许嵌套。
#[inline]
pub fn preempt_disable() {
    PREEMPT_OFF.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn preempt_enable() {
    // 不用 fetch_sub 的返回值做断言：宁可少报一次，也不要在内核里 panic
    let v = PREEMPT_OFF.load(Ordering::Relaxed);
    if v > 0 {
        PREEMPT_OFF.store(v - 1, Ordering::Relaxed);
    }
}

/// 关抢占的守卫：作用域结束自动恢复。
pub struct PreemptGuard(());

impl PreemptGuard {
    pub fn new() -> Self {
        preempt_disable();
        PreemptGuard(())
    }
}

impl Drop for PreemptGuard {
    fn drop(&mut self) {
        preempt_enable();
    }
}

/* ---------------- 初始化 ---------------- */

/// 把"当前正在执行的这段代码"变成 0 号任务。
pub fn init() -> Result<(), &'static str> {
    // SAFETY: 启动阶段单核、中断关闭；对任务表与调度器独占写入。
    let s = sched();
    // 队列数 = 实际核数：AP 的 idle/内核线程会落到自己的队列里。
    // 比起"8 个队列只用 1 个"，让调度器一开始就知道真实拓扑，
    // 后面把用户任务分布到多核时不需要再动这里。
    s.init_in_place(crate::arch::smp::count().max(1));
    // 0 号任务先入队再取出标为 running，这样 `pick` 的 retire 逻辑能正常把它放回队列。
    s.spawn(0, 20, 0, 0).map_err(|_| "调度器初始化失败")?;
    s.set_running(0, Some(0));

    // SAFETY: 启动阶段单核、中断关闭；对任务表独占写入。
    unsafe {
        let t = &mut *TASKS.0.get();
        for slot in t.iter_mut() {
            *slot = Task::empty();
        }
        t[0] = Task {
            state: State::Running,
            tid: 0,
            kstack_top: core::ptr::addr_of!(kstack0_top) as u64,
            kstack_base: 0, // 静态栈，回收时不能还
            kstack: 0, // 第一次 switch_to 时才会填
            user: false,
            exit_code: 0,
            ticks: 0,
            pml4: paging::kernel_cr3(),
            fds: crate::fd::new_table(),
            fs_base: 0,
            cwd: cwd_root(),
            cwd_len: 1,
            brk: 0,
            brk_start: 0,
            mmap_top: 0,
            parent: 0,
            vmas: [Vma::FREE; VMA_MAX],
            vma_count: 0,
            sched_cpu: 0,
        };
    }
    for c in CUR.iter() {
        c.store(0, Ordering::Relaxed);
    }
    INITIALIZED.store(true, Ordering::Relaxed);
    Ok(())
}

/* ---------------- 栈构造 ---------------- */

/// 拼一个"刚出生"的任务栈，返回要写进 `Task.kstack` 的值。
///
/// 栈从高到低：
/// ```text
///   top-176 .. top      InterruptFrame（22 words）
///   top-184             trap_return
///   top-232 .. top-184  6 个 callee-saved 占位
///   ← 返回这里的地址
/// ```
fn build_stack(top: u64, rip: u64, rsp: u64, cs: u16, ss: u16, rflags: u64) -> u64 {
    let frame_bytes = core::mem::size_of::<InterruptFrame>() as u64;
    let frame_addr = top - frame_bytes;
    // SAFETY: 这段内存是刚分配、当前无人使用的内核栈；下面写入的字段全部有明确含义。
    unsafe {
        core::ptr::write_bytes(frame_addr as *mut u8, 0, frame_bytes as usize);
        let f = frame_addr as *mut InterruptFrame;
        (*f).rip = rip;
        (*f).cs = cs as u64;
        (*f).rflags = rflags;
        (*f).rsp = rsp;
        (*f).ss = ss as u64;

        let ret_addr = frame_addr - 8;
        *(ret_addr as *mut u64) = trap_return as *const () as u64;

        let regs = ret_addr - 48;
        core::ptr::write_bytes(regs as *mut u8, 0, 48);
        regs
    }
}

fn alloc_slot() -> Option<usize> {
    // AP 的调度循环也会建 idle 任务，BSP 可能同时在 spawn——
    // 找空槽必须互斥，否则两核拿到同一个槽位（表现是任务互相覆盖，
    // 随机崩在"task 字段被改"上，现场毫无线索）。
    let g = SCHED_LOCK.lock();
    // SAFETY: 已持锁独占。
    let t = unsafe { &mut *TASKS.0.get() };
    let r = t.iter().position(|s| s.state == State::Free);
    drop(g);
    r
}

fn alloc_kstack() -> Option<(u64, u64)> {
    let phys = frames::alloc_frames(4)?;
    let top = paging::phys_to_virt(phys + 4 * 4096);
    Some((phys, top))
}

/// 创建一个内核线程。`entry` 应当是永不返回的函数（结束时调 [`exit_current`]）。
/// 内核线程轮流落到的 CPU（BSP=0 之外还有多少可用核就轮到多少号）。
static KT_CPU: AtomicUsize = AtomicUsize::new(0);

/// AP 的调度入口：为本核建一个常驻 idle 任务，然后进标准调度循环。
///
/// **为什么 idle 要是一个真任务**：调度器的 retire/时间片逻辑都围绕
/// "当前运行任务"工作。AP 直接裸跑 `pick` 循环的话，`running[cpu]`
/// 永远是空，时间片记账无从谈起。给每核一个永不退出的 idle 任务，
/// AP 就和一个普通任务没有区别——调度代码一行不用改。
pub fn ap_scheduler_start() -> ! {
    let cpu = crate::arch::smp::me().expect("AP 调度循环必须能拿到自己的核号");
    let slot = alloc_slot().expect("AP idle 任务分配失败");
    let (kstack_phys, top) = alloc_kstack().expect("AP idle 内核栈分配失败");
    let tid = NEXT_TID.fetch_add(1, Ordering::Relaxed);

    let kstack = build_stack(
        top,
        ap_idle as usize as u64,
        top - 1024,
        crate::arch::gdt::KERNEL_CS,
        crate::arch::gdt::KERNEL_DS,
        0x202,
    );
    // SAFETY: slot 刚分配独占；启动阶段 AP 尚未接收任务。
    unsafe {
        let t = &mut *TASKS.0.get();
        t[slot] = Task {
            state: State::Running,
            tid,
            kstack_top: top,
            kstack_base: kstack_phys,
            kstack,
            user: false,
            exit_code: 0,
            ticks: 0,
            pml4: paging::kernel_cr3(),
            fds: crate::fd::new_table(),
            fs_base: 0,
            cwd: cwd_root(),
            cwd_len: 1,
            brk: 0,
            brk_start: 0,
            mmap_top: 0,
            parent: 0,
            sched_cpu: cpu as u8,
            vmas: [Vma::FREE; VMA_MAX],
            vma_count: 0,
        };
    }
    CUR[cpu].store(slot, Ordering::Relaxed);
    {
        let g = SCHED_LOCK.lock();
        let _ = sched().spawn(tid, 0, 0, cpu); // prio 0：最低权重，有活让活干
        // running 标记必须是**自己的 tid**：写死 0 会让 AP 的 retire 把
        // BSP 主任务（tid0）放回 AP 队列——同一个任务出现在两个核的
        // 队列里，unlink 一半就是调度器结构损坏（静默卡死的元凶）。
        let _ = sched().set_running(cpu, Some(tid));
        drop(g);
    }
    INITIALIZED.store(true, Ordering::Release);
    crate::intr::enable();
    ap_idle()
}

/// 每核的 idle：有活就切过去跑，没活就睡。
extern "C" fn ap_idle() -> ! {
    loop {
        if !schedule_on_me() {
            // SAFETY: 没有可跑任务时等中断（定时器 tick 或未来的 IPI）。
            unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
        }
    }
}

/// 在**本核**跑一轮调度：pick 自己队列的任务并切过去，任务让出后返回 true。
/// 没有可跑任务返回 false（调用方 hlt）。
fn schedule_on_me() -> bool {
    if !INITIALIZED.load(Ordering::Relaxed) {
        return false;
    }
    let cpu = crate::arch::smp::me().unwrap_or(0);
    let g = SCHED_LOCK.lock();
    let now = timer::ticks();
    let next = sched().pick(cpu, now);
    let picked = match next {
        Some(tid) => {
            // SAFETY: 持锁独占。
            let base = TASKS.0.get();
            let idx = (0..MAX_TASKS)
                .find(|i| unsafe { (*base)[*i].tid == tid && (*base)[*i].state != State::Free });
            match idx {
                Some(i) if i != CUR[cpu].load(Ordering::Relaxed) => {
                    unsafe {
                        let cur_p = (*base).as_mut_ptr().add(CUR[cpu].load(Ordering::Relaxed));
                        if (*cur_p).state == State::Running {
                            (*cur_p).state = State::Ready;
                        }
                        (*(*base).as_mut_ptr().add(i)).state = State::Running;
                    }
                    CUR[cpu].store(i, Ordering::Relaxed);
                    Some(i)
                }
                _ => None,
            }
        }
        None => None,
    };
    drop(g);
    match picked {
        Some(_) => true, // 任务切走又切回（时间片/让出），本轮完成
        None => false,
    }
}

pub fn spawn_kthread(entry: extern "C" fn()) -> Result<u64, &'static str> {
    let _g = PreemptGuard::new();
    let slot = alloc_slot().ok_or("任务表已满")?;
    let (kstack_phys, top) = alloc_kstack().ok_or("内核栈分配失败")?;
    let tid = NEXT_TID.fetch_add(1, Ordering::Relaxed);
    // 多核调度未启用（见 smp.rs 注）：所有任务落 CPU0。
    //
    // **这里必须写死 0，不能用 `count()` 轮转**：`count()` 在 AP 上线后
    // 会变成 4/8，而调度器的队列数在 `task::init`（AP 尚未上线）就定型为
    // 当时的 `count()`——两个数在时间上永远错开，轮转出来的 cpu 号
    // 必然 `BadCpu` 被调度器拒绝，内核线程无声消失。
    // 启用多核调度时：init_in_place 用 MADT 的核总数，这里恢复轮转。
    let _ = KT_CPU.fetch_add(1, Ordering::Relaxed);
    let cpu = 0;

    let kstack = build_stack(
        top,
        entry as usize as u64,
        top - 1024, // 给初始帧留出空间，栈从这里往下长
        gdt::KERNEL_CS,
        gdt::KERNEL_DS,
        0x202, // IF=1
    );
    let _ = cpu;

    // SAFETY: 单核 + 抢占已关闭；slot 与 tid 都是本函数独占。
    unsafe {
        let t = &mut *TASKS.0.get();
        t[slot] = Task {
            state: State::Ready,
            tid,
            kstack_top: top,
            kstack_base: kstack_phys,
            kstack,
            user: false,
            exit_code: 0,
            ticks: 0,
            pml4: paging::kernel_cr3(),
            fds: crate::fd::new_table(),
            fs_base: 0,
            cwd: cwd_root(),
            cwd_len: 1,
            brk: 0,
            brk_start: 0,
            mmap_top: 0,
            parent: 0,
            vmas: [Vma::FREE; VMA_MAX],
            vma_count: 0,
            sched_cpu: 0,
        };
        if let Err(e) = sched().spawn(tid, 20, timer::ticks(), cpu) {
            kserialln!("[kt] spawn 被拒绝 tid={} err={:?} cpu={}", tid, e, cpu);
            return Err("调度器拒绝该任务");
        }
    }
    Ok(tid)
}

/// 全新的进程默认工作目录就是 `/`。
pub const fn cwd_root() -> [u8; CWD_MAX] {
    let mut c = [0u8; CWD_MAX];
    c[0] = b'/';
    c
}

/// user 映射的固定虚拟地址（都在 PML4[0] 覆盖范围内，但避开了恒等映射的 1 GiB）。
pub const USER_CODE_VA: u64 = 0x0000_0040_0000_0000; // 64 GiB
pub const USER_STACK_TOP: u64 = 0x0000_0040_0100_0000; // 64 GiB + 16 MiB

/// 准备好用户态代码页与栈页，返回用户入口地址。
pub fn setup_user_image() -> Result<u64, &'static str> {
    let _g = PreemptGuard::new();
    // 两个符号由 linker.ld 圈定 `.user` 段；`addr_of!` 只取地址，不读取内存。
    let (src, len) = (
        core::ptr::addr_of!(__user_start) as u64,
        core::ptr::addr_of!(__user_end) as u64 - core::ptr::addr_of!(__user_start) as u64,
    );
    if len == 0 || len > 4096 {
        return Err("用户映像大小异常");
    }

    let code_phys = frames::alloc_frame().ok_or("用户代码页分配失败")?;
    // SAFETY: code_phys 是刚摘下的帧；通过直接映射区拷入用户映像（长度已判界）。
    unsafe {
        core::ptr::copy_nonoverlapping(
            src as *const u8,
            paging::phys_to_virt(code_phys) as *mut u8,
            len as usize,
        );
    }
    paging::map_in_current(USER_CODE_VA, code_phys, kvm::Flags::USER_RX)
        .map_err(|_| "用户代码页映射失败")?;

    let stack_phys = frames::alloc_frame().ok_or("用户栈分配失败")?;
    // SAFETY: 同上，栈页清零即可。
    unsafe { core::ptr::write_bytes(paging::phys_to_virt(stack_phys) as *mut u8, 0, 4096) };
    paging::map_in_current(USER_STACK_TOP - 4096, stack_phys, kvm::Flags::USER_RW)
        .map_err(|_| "用户栈映射失败")?;

    Ok(USER_CODE_VA)
}

/// 创建一个用户任务。
/// 用固定映像的默认栈创建用户任务（M3 的自检用）。
pub fn spawn_user(entry: u64) -> Result<u64, &'static str> {
    // M3 的固定映像跑在内核地址空间里（它自己的页已经在 kernel CR3 里映射好了）
    spawn_user_at(entry, USER_STACK_TOP, paging::kernel_cr3())
}

/// 用指定用户栈顶创建用户任务（M5 的 ELF 装载器要用自己的栈）。
pub fn spawn_user_at(entry: u64, user_sp: u64, pml4: u64) -> Result<u64, &'static str> {
    // 栈顶往下留一点余量：M5 的老程序不读 argc，对齐更稳
    spawn_user_exact(entry, user_sp.saturating_sub(16), pml4)
}

/// 用**精确**的 `%rsp` 创建用户任务（M9：初始栈是按 SysV 布局拼好的，
/// 少一个字节都会让 musl 读错 argc）。
pub fn spawn_user_exact(entry: u64, user_sp: u64, pml4: u64) -> Result<u64, &'static str> {
    let _g = PreemptGuard::new();
    let slot = alloc_slot().ok_or("任务表已满")?;
    let (kstack_phys, top) = alloc_kstack().ok_or("内核栈分配失败")?;
    let tid = NEXT_TID.fetch_add(1, Ordering::Relaxed);

    let kstack = build_stack(
        top,
        entry,
        user_sp,
        gdt::USER_CS,
        gdt::USER_DS,
        0x202,
    );

    // SAFETY: 同 spawn_kthread。
    unsafe {
        let t = &mut *TASKS.0.get();
        t[slot] = Task {
            state: State::Ready,
            tid,
            kstack_top: top,
            kstack_base: kstack_phys,
            kstack,
            user: true,
            exit_code: 0,
            ticks: 0,
            pml4,
            fds: crate::fd::new_table(),
            fs_base: 0,
            cwd: cwd_root(),
            cwd_len: 1,
            brk: 0,
            brk_start: 0,
            mmap_top: 0,
            parent: 0,
            vmas: [Vma::FREE; VMA_MAX],
            vma_count: 0,
            sched_cpu: 0,
        };
        sched().spawn(tid, 20, timer::ticks(), 0).map_err(|_| "调度器拒绝该任务")?;
    }
    Ok(tid)
}

/// 从当前任务的用户帧复制出一个子任务（`fork` 用）。
///
/// 关键点：**子进程不复制内核栈**。它拿一张全新的内核栈，栈顶直接拼成
/// "从系统调用返回"的形状——`trap_return` → `iretq` → 用户态那条
/// `syscall` 的下一条指令，`rax = 0`。这样避免了一整类
/// "复制过来的内核栈里 RBP 还指向父进程栈"的隐性错误。
pub fn spawn_user_clone(frame: &InterruptFrame, pml4: u64) -> Result<u64, &'static str> {
    let _g = PreemptGuard::new();
    let slot = alloc_slot().ok_or("任务表已满")?;
    let (kstack_phys, top) = alloc_kstack().ok_or("内核栈分配失败")?;
    let tid = NEXT_TID.fetch_add(1, Ordering::Relaxed);

    let kstack = build_stack_from_frame(top, frame, 0);

    // SAFETY: 单核 + 抢占已关闭；slot 由本函数独占。
    unsafe {
        let t = &mut *TASKS.0.get();
        let parent = t[current_index()];
        t[slot] = Task {
            state: State::Ready,
            tid,
            kstack_top: top,
            kstack_base: kstack_phys,
            kstack,
            user: true,
            exit_code: 0,
            ticks: 0,
            pml4,
            fds: parent.fds,
            fs_base: parent.fs_base,
            cwd: parent.cwd,
            cwd_len: parent.cwd_len,
            brk: parent.brk,
            brk_start: parent.brk_start,
            mmap_top: parent.mmap_top,
            parent: parent.tid,
            // 子进程继承父进程的匿名映射区间：地址空间是 COW 复制过来的，
            // 区间表必须跟着复制，否则子进程一碰共享页就找不到"这页该有映射"。
            vmas: parent.vmas,
            vma_count: parent.vma_count,
            sched_cpu: parent.sched_cpu,
        };
        sched().spawn(tid, 20, timer::ticks(), 0).map_err(|_| "调度器拒绝该任务")?;
    }
    Ok(tid)
}

/// 用现有的用户帧拼一张新内核栈：子进程第一次被调度时直接 `iretq` 回用户态。
fn build_stack_from_frame(top: u64, src: &InterruptFrame, ret_rax: u64) -> u64 {
    let frame_bytes = core::mem::size_of::<InterruptFrame>() as u64;
    let frame_addr = top - frame_bytes;
    // SAFETY: frame_addr 在本函数独占的新内核栈内；src 是当前栈上的合法帧。
    unsafe {
        core::ptr::copy_nonoverlapping(
            src as *const InterruptFrame as *const u8,
            frame_addr as *mut u8,
            frame_bytes as usize,
        );
        let f = frame_addr as *mut InterruptFrame;
        (*f).rax = ret_rax; // fork 在子进程里返回 0
        (*f).vector = 128;
        (*f).error = 0;

        let ret_addr = frame_addr - 8;
        *(ret_addr as *mut u64) = trap_return as *const () as u64;
        let regs = ret_addr - 48;
        core::ptr::write_bytes(regs as *mut u8, 0, 48);
        regs
    }
}

/* ---------------- 调度 ---------------- */

fn current_index() -> usize {
    // **必须读 CUR[0]**：`me()` 是一次 APIC MMIO 读（VBox 里 = VM exit，
    // 数千周期），它现在在 syscall 热路径上——每次 getpid/getpid 都读
    // 一次 MMIO，实测 syscall 从 408 周期涨到 44.7K（110 倍）。
    // 多核调度未启用，所有任务恒在核 0；启用多核时恢复 me() 索引
    // （配合 per-CPU APIC id 缓存，见 docs/08 的 SMP 待办）。
    CUR[0].load(Ordering::Relaxed)
}

#[allow(dead_code)]
fn current_index_on(cpu: usize) -> usize {
    CUR[cpu].load(Ordering::Relaxed)
}

/// 某个任务的地址空间（诊断用）。
pub fn pml4_of(tid: u64) -> Option<u64> {
    // SAFETY: 单核，只读。
    unsafe {
        (*TASKS.0.get())
            .iter()
            .find(|t| t.tid == tid && t.state != State::Free)
            .map(|t| t.pml4)
    }
}

/// 当前任务号。
pub fn current_tid() -> u64 {
    // SAFETY: 单核，读单个字段。
    unsafe { (*TASKS.0.get()).get(current_index()).map(|t| t.tid).unwrap_or(0) }
}

/// 切到下一个任务。返回 true 表示确实发生了切换。
///
/// 调用前提：中断已关闭（或处于 `preempt_disable` 区间），
/// 因为整个函数在改调度器与任务表的共享状态。
fn schedule_inner() -> bool {
    if !INITIALIZED.load(Ordering::Relaxed) {
        return false;
    }
    let cur = current_index();
    // SAFETY: 单核 + 中断关闭，独占访问任务表与调度器。
    unsafe {
        let base = TASKS.0.get() as *mut Task;
        let cur_p = base.add(cur);

        let now = timer::ticks();
        let s = sched();

        // pick 会把当前 running 任务退役回就绪队列，再挑 vruntime 最小的。
        let next_tid = match s.pick(0, now) {
            Some(t) => t,
            None => return false,
        };

        let next_idx = match (0..MAX_TASKS).find(|i| (*base.add(*i)).tid == next_tid && (*base.add(*i)).state != State::Free) {
            Some(i) => i,
            None => return false,
        };
        if next_idx == cur {
            return false;
        }

        // 只有"还在运行"的任务才需要退回就绪队列。
        // 正在退出的任务已经被标成 Zombie，这里若无条件改回 Ready，
        // 父进程永远等不到它退出（M3 自检里就踩到了这个）。
        if (*cur_p).state == State::Running {
            (*cur_p).state = State::Ready;
        }
        let next_p = base.add(next_idx);
        (*next_p).state = State::Running;
        CUR[crate::arch::smp::me().unwrap_or(0)].store(next_idx, Ordering::Relaxed);
        // 用户任务的内核栈必须自己带，否则会踩到上一个用户任务的栈
        gdt::set_rsp0((*next_p).kstack_top);
        QUANTUM_LEFT.store(QUANTUM, Ordering::Relaxed);
        SWITCHES.fetch_add(1, Ordering::Relaxed);

        // 切地址空间。必须在 switch_to **之前**做：切完之后我们已经在对方的
        // 内核栈上，而内核栈走直接映射，两张表里都有效，所以这个顺序是安全的。
        let want = if (*next_p).pml4 != 0 {
            (*next_p).pml4
        } else {
            paging::kernel_cr3()
        };
        if want != 0 && want != paging::read_cr3() {
            paging::write_cr3(want);
        }
        // TLS 基址是每进程的：不换它，两个进程的 `%fs:` 会读到同一份数据。
        crate::arch::syscall::set_fs_msr((*next_p).fs_base);

        let prev_slot = core::ptr::addr_of_mut!((*cur_p).kstack);
        let next_kstack = (*next_p).kstack;
        switch_to(prev_slot, next_kstack);

        // 被切回来时，别人已经把我们的状态写成 Running
        if (*cur_p).state == State::Ready {
            (*cur_p).state = State::Running;
        }
        true
    }
}

/// 主动让出 CPU。
///
/// **必须给当前任务记一笔 vruntime**，否则主动让出是"免费"的：
/// 一个 `while ... { yield_now() }` 空转的任务 vruntime 永远不涨，
/// 调度器每次都挑它，另一个就绪任务被无限期饿死。
/// M7 自检里就撞上了这个：读者线程用 yield 轮询等标志位，
/// 主线程在 `hlt` 里等它退出——两边互相等，整机卡死。
///
/// Linux 的做法也是同一件事：任务让出前先 `update_curr()` 把本段时间记进去。
/// 这里简化成"记一个时间片的量"，够用且可解释。
pub fn yield_now() {
    if INITIALIZED.load(Ordering::Relaxed) {
        // SAFETY: 单核；只给当前任务累加 vruntime。
        unsafe {
            let p = (TASKS.0.get() as *mut Task).add(current_index());
            if (*p).state == State::Running {
                sched().tick(0, timer::ticks(), 1);
            }
        }
    }
    let _ = schedule_inner();
}

/// 时钟 tick 驱动的抢占。
pub fn on_tick() {
    if !INITIALIZED.load(Ordering::Relaxed) {
        return;
    }
    if PREEMPT_OFF.load(Ordering::Relaxed) > 0 {
        return;
    }
    let cur = current_index();
    // SAFETY: 单核；这里只读任务状态与累加计数。
    unsafe {
        let base = TASKS.0.get() as *mut Task;
        let p = base.add(cur);
        if (*p).state != State::Running {
            return;
        }
        (*p).ticks += 1;
        sched().tick(0, timer::ticks(), 1);
    }
    let left = QUANTUM_LEFT.load(Ordering::Relaxed);
    if left <= 1 {
        QUANTUM_LEFT.store(QUANTUM, Ordering::Relaxed);
        if schedule_inner() {
            PREEMPTIONS.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        QUANTUM_LEFT.store(left - 1, Ordering::Relaxed);
    }
}

/// 结束当前任务。**不返回**。
pub fn exit_current(code: i64) -> ! {
    let cur = current_index();
    // SAFETY: 单核；读自己的 tid（下面要拿它当"被等的对象"）。
    let tid_of = |_c: usize| -> u64 { unsafe { (*TASKS.0.get())[_c].tid } };
    // SAFETY: 单核；下面是自己的收尾。
    unsafe {
        let base = TASKS.0.get() as *mut Task;
        let p = base.add(cur);
        (*p).state = State::Zombie;
        (*p).exit_code = code;
        let tid = (*p).tid;

        let s = sched();
        // **必须用 detach**：运行态任务不在就绪队列里，`dequeue` 会静默
        // 失败（它只摘 ST_READY），槽位带着 ST_RUNNING 永远不回 free list。
        // 泄漏累积到 ksched 槽位上限后，`spawn` 全部返回 Duplicate/Full——
        // 内核线程从此没被调度过，fd-table/pipe/perf 三个自检连锁失败。
        // 这是同一根因第三次咬人（前两次：block_current、wake）。
        s.detach(tid);
    }
    EXITED.fetch_add(1, Ordering::Relaxed);

    // 退出前把 fd 关掉：管道端点必须关，否则对端会永远等一个不会来的写者。
    // 这一步要在标完 Zombie 之后做——关 fd 可能唤醒别人，别人可能立刻
    // 就来查我们的退出码，那时状态必须已经是 Zombie。
    crate::fd::close_all();

    // 叫醒所有在等我们退出的人。
    // 锁内只取快照，wake 放到锁外——wake 会动调度器，那是另一把锁，
    // 两把锁嵌套容易把加锁顺序搞错，一旦反序就是跨核死锁。
    let waiters = {
        let w = WAIT_LOCK.lock();
        exit_waiters(tid_of(cur))
    };
    for i in 0..waiters.1 {
        wake(waiters.0[i]);
    }

    loop {
        if !schedule_inner() {
            // 没有别的任务可切了：内核使命完成，停机
            kprintln!("[LIVE] no runnable task left, halt");
            crate::intr::frame::halt();
        }
    }
}

/// 当前任务的可变引用（fd 表要用）。
///
/// # Safety 约定
/// 调用方必须处在"单核独占"的上下文：中断关闭或 `preempt_disable` 区间。
/// 现在所有调用点（系统调用分发、退出路径）都满足这一条。
pub fn current_task_mut() -> Option<&'static mut Task> {
    // SAFETY: 见上；任务表是静态内存，生命周期够长。
    unsafe {
        let base = TASKS.0.get() as *mut Task;
        let p = base.add(current_index());
        if (*p).state == State::Free {
            None
        } else {
            Some(&mut *p)
        }
    }
}

/// 把当前任务挂起，直到有人 [`wake`] 它。返回是否真的切走了。
///
/// 调用前提：**中断已关闭**，且调用前已经把自己登记到某个等待队列里。
/// 中断必须关着，否则"登记完 → 还没切走"这段窗口里唤醒可能落空
/// （见 `pipe.rs` 顶部关于 lost wakeup 的说明）。
///
/// 返回 `false` 表示没有别的任务可运行，调用方应当撤销登记。
/// 从任务表查某任务的调度核归属（wake 时用；不依赖 SCHED_LOCK）。
fn sched_cpu_of(tid: u64) -> usize {
    // SAFETY: 只读遍历；tid 对应的槽位由任务生命周期保证存在。
    unsafe {
        let base = TASKS.0.get();
        (0..MAX_TASKS)
            .map(|i| &(*base)[i])
            .find(|t| t.tid == tid && t.state != State::Free)
            .map(|t| t.sched_cpu as usize)
            .unwrap_or(0)
    }
}

pub fn block_current() -> bool {
    if !INITIALIZED.load(Ordering::Relaxed) {
        return false;
    }
    let cur = current_index();
    // SAFETY: 单核 + 中断关闭；下面是自己的状态迁移。
    unsafe {
        let base = TASKS.0.get() as *mut Task;
        let p = base.add(cur);
        if (*p).state == State::Zombie {
            return false;
        }
        let tid = (*p).tid;
        // 从调度器里彻底摘掉再标状态：反过来会出现"状态是 Blocked 但还在
        // 运行队列里"，pick 会挑中一个不该跑的任务。
        //
        // **必须用 `detach` 而不是 `dequeue`**：运行态任务不在队列里，
        // `dequeue` 会静默失败，槽位带着 ST_RUNNING 泄漏——之后 wake 里的
        // `spawn` 因 `Duplicate` 一直失败，被阻塞的任务只能等 tick 兜底
        // （实测 200 次 wait 有 194 次退化成 10 ms 级延迟）。
        sched().detach(tid);
        (*p).state = State::Blocked;
    }
    let switched = schedule_inner();
    if !switched {
        // 没人可切：撤销阻塞，别把自己困死
        // SAFETY: 同上。
        unsafe {
            let base = TASKS.0.get() as *mut Task;
            let p = base.add(cur);
            if (*p).state == State::Blocked {
                (*p).state = State::Running;
                let tid = (*p).tid;
                let _ = sched().spawn(tid, 20, timer::ticks(), 0);
            }
        }
    }
    switched
}

/// 唤醒一个阻塞的任务。返回是否真的唤醒了（对不在阻塞态的任务返回 `false`）。
pub fn wake(tid: u64) -> bool {
    if !INITIALIZED.load(Ordering::Relaxed) {
        return false;
    }
    // SAFETY: 单核；只改目标任务的状态并把它塞回运行队列。
    unsafe {
        let base = TASKS.0.get() as *mut Task;
        let idx = match (0..MAX_TASKS)
            .find(|i| (*base.add(*i)).tid == tid && (*base.add(*i)).state != State::Free)
        {
            Some(i) => i,
            None => return false,
        };
        let p = base.add(idx);
        if (*p).state != State::Blocked {
            // 已经就绪/正在跑/已退出：唤醒是幂等的，不做第二次入队
            return false;
        }
        (*p).state = State::Ready;
        // 多核调度未启用（见 smp.rs 注）：任务全在 CPU0 的队列。
        // sched_cpu_of() 的逻辑保留在函数里，启用多核时恢复。
        if sched().spawn(tid, 20, timer::ticks(), 0).is_err() {
            (*p).state = State::Blocked;
            return false;
        }
        WAKED.fetch_add(1, Ordering::Relaxed);
        true
    }
}

/// 谁在等谁退出：`(等待者 tid, 被等的 tid)`。
///
/// 只支持"父等子"这一种关系，所以一张小表就够；真做 `wait4` 得按 pid 索引。
struct ExitWaitCell(UnsafeCell<[(u64, u64); 8]>);
// SAFETY: 单核；只在关中断区间内读写（`wait_zombie` / `exit_current`）。
unsafe impl Sync for ExitWaitCell {}
static EXIT_WAIT: ExitWaitCell = ExitWaitCell(UnsafeCell::new([(0, 0); 8]));

fn exit_wait_add(me: u64, target: u64) -> bool {
    // SAFETY: 单核 + 中断关闭（调用方 `wait_zombie` 保证）。
    unsafe {
        let t = &mut *EXIT_WAIT.0.get();
        for e in t.iter_mut() {
            if e.0 == 0 {
                *e = (me, target);
                return true;
            }
        }
        false
    }
}

fn exit_wait_del(me: u64) {
    // SAFETY: 同上。
    unsafe {
        let t = &mut *EXIT_WAIT.0.get();
        for e in t.iter_mut() {
            if e.0 == me {
                *e = (0, 0);
                return;
            }
        }
    }
}

/// 等待表里的"任意子进程"通配符（`wait4(-1, ...)` 用它）。
const WAIT_ANY: u64 = u64::MAX;

/// 谁在等 `target` 退出（退出路径用它叫醒父进程）。
fn exit_waiters(target: u64) -> ([u64; 8], usize) {
    let mut out = [0u64; 8];
    let mut n = 0;
    // SAFETY: 单核，只读。
    unsafe {
        for e in (*EXIT_WAIT.0.get()).iter() {
            if e.0 != 0 && (e.1 == target || e.1 == WAIT_ANY) {
                out[n] = e.0;
                n += 1;
            }
        }
    }
    (out, n)
}

/// 阻塞等待某个任务退出，返回它的退出码。
///
/// 这就是原来 `main.rs` 里那个 `wait_task`（靠 `hlt` 轮询）的替代品：
/// 真的睡眠，不烧 CPU。任务已经退出时立刻返回，不睡。
pub fn wait_zombie(tid: u64) -> Option<i64> {
    let me = current_tid();
    loop {
        match query(tid) {
            Some((State::Zombie, code, _)) => return Some(code),
            Some(_) => {}
            None => return None,
        }
        if me == 0 || !exit_wait_add(me, tid) {
            // 0 号任务（内核主线程）或等待表满了：退化成"关中断停一下"。
            // 宁可多耗一点 CPU，也不能在这里死等——那就真卡住了。
            crate::intr::enable();
            crate::intr::frame::halt_once();
            crate::intr::disable();
            continue;
        }
        // 关中断：从"检查状态"到"睡下去"之间不能有窗口，
        // 否则子进程可能正好在这中间退出，唤醒落空 → 永远等不到。
        let irq = crate::arch::sync::disable_irq_save();
        let code = match query(tid) {
            Some((State::Zombie, code, _)) => {
                exit_wait_del(me);
                Some(code)
            }
            Some(_) => {
                if block_current() {
                    None
                } else {
                    exit_wait_del(me);
                    Some(i64::MIN) // 没人可跑：交给调用方当超时处理
                }
            }
            None => {
                exit_wait_del(me);
                None
            }
        };
        crate::arch::sync::restore_irq(irq);
        if let Some(c) = code {
            if c != i64::MIN {
                return Some(c);
            }
            // 谁都不在跑：让时钟推进一下再继续等
            crate::intr::enable();
            crate::intr::frame::halt_once();
            crate::intr::disable();
        }
    }
}

/* ---------------- M9：子进程 ---------------- */

/// 找已退出的子进程。`pid > 0` 时只看那一个；`pid <= 0` 看所有子进程。
pub fn zombie_child(parent: u64, pid: i64) -> Option<(u64, i64)> {
    // SAFETY: 单核，只读。
    unsafe {
        (*TASKS.0.get())
            .iter()
            .find(|t| {
                t.state == State::Zombie
                    && t.parent == parent
                    && (pid <= 0 || t.tid == pid as u64)
            })
            .map(|t| (t.tid, t.exit_code))
    }
}

/// 还有没有活着的（或已退出但没被收的）子进程。
fn has_child(parent: u64, pid: i64) -> bool {
    // SAFETY: 单核，只读。
    unsafe {
        (*TASKS.0.get()).iter().any(|t| {
            t.state != State::Free && t.parent == parent && (pid <= 0 || t.tid == pid as u64)
        })
    }
}

/// 等一个子进程退出（`wait4` 的阻塞部分）。
///
/// 和 [`wait_zombie`] 的区别：这里的等待对象是"任意子进程"，
/// 所以登记时用 [`WAIT_ANY`] 通配；退出路径会连这个一起叫醒。
/// `wait_child` 里被迫用 `halt` 等时钟的次数。
///
/// 正常路径下父进程会被子进程的 `exit` 直接叫醒，这个数**必须是 0**；
/// 不为 0 说明唤醒链路漏了人，等待退化成"等下一个时钟 tick"（10 ms）。
pub static WAIT_HALTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn wait_child(pid: i64) -> Option<(u64, i64)> {
    let me = current_tid();
    // 无论从哪个出口离开，都先把自己的登记清掉。
    //
    // 这张表只有 8 项，而"登记成功 → 被唤醒 → reap"的主路径**没有任何人删它**
    // ——`exit_wait_del` 只在"block 失败"的分支被调。于是连续 wait 的进程
    // 第 9 次起 `exit_wait_add` 全部失败，永远走不上阻塞唤醒，
    // 退化成每次 10 ms 的 halt 轮询（实测 200 次 fork 里 195 次）。
    struct Cleanup(u64);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            exit_wait_del(self.0);
        }
    }
    let _cleanup = Cleanup(me);
    loop {
        if let Some(z) = zombie_child(me, pid) {
            return Some(z);
        }
        if !has_child(me, pid) {
            return None;
        }

        // 关中断 + WAIT_LOCK：EXIT_WAIT 现在被多个核并发读写
        // （exit 在子进程所在的核，wait 在父进程所在的核），
        // 只关本核中断挡不住另一核。锁内只做表操作，不切换。
        let irq = crate::arch::sync::disable_irq_save();
        let w = WAIT_LOCK.lock();
        // 复查一次：从上面的检查到拿锁之间，子进程可能刚好退出
        if let Some(z) = zombie_child(me, pid) {
            drop(w);
            crate::arch::sync::restore_irq(irq);
            return Some(z);
        }
        let target = if pid > 0 { pid as u64 } else { WAIT_ANY };
        let added = exit_wait_add(me, target);
        drop(w);
        if added && !block_current() {
            let w = WAIT_LOCK.lock();
            exit_wait_del(me);
            drop(w);
        }
        crate::arch::sync::restore_irq(irq);

        // 没人可跑时，让时钟推进一下再看（否则这一轮等于空转）
        if zombie_child(me, pid).is_none() {
            WAIT_HALTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // 限流打印队列现场：谁在就绪队列里、自己是什么状态。
            // 这是定位"唤醒链路漏洞"的唯一途径——代码推演推不出来时，
            // 让硬件自己说话。
            {
                use core::sync::atomic::Ordering as O;
                static DIAG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
                if DIAG.fetch_add(1, O::Relaxed) < 3 {
                    // SAFETY: 中断当前是关的（上面 disable 过），独占任务表。
                    let (n, list) = unsafe {
                        let base = TASKS.0.get();
                        let mut buf = [0u64; 8];
                        let mut cnt = 0usize;
                        for (i, t) in (*base).iter().enumerate() {
                            if t.state == State::Ready && cnt < 8 {
                                buf[cnt] = t.tid;
                                cnt += 1;
                            }
                        }
                        let me_t = &(*base)[current_index()];
                        (cnt, (buf, me_t.tid, me_t.state as u8))
                    };
                    kserialln!(
                        "[wait] halt 现场：me=tid{} state={} ready 列表 n={} {:?}",
                        list.1, list.2, n, list.0
                    );
                }
            }
            crate::intr::enable();
            crate::intr::frame::halt_once();
            crate::intr::disable();
        }
    }
}

/// 回收一个已退出任务的资源：用户地址空间、fd、内核栈、任务槽位。
///
/// 返回 `(释放的数据页, 释放的页表页, 是否成功)`。
///
/// **为什么必须单独有这一步**：僵尸任务只是"不再被调度"，它占着的帧一点没少。
/// M6 的注释里就写了"跑几百个进程就会把 512 MiB 吃光"——这里就是修它的地方。
/// 回收不能放在任务自己身上做：它还在自己的内核栈上跑，栈一释放就当场踩空。
pub fn reap(tid: u64) -> (usize, usize, bool) {
    // SAFETY: 单核 + 中断关闭；只动目标任务（它已不运行）。
    unsafe {
        let base = TASKS.0.get() as *mut Task;
        let idx = match (0..MAX_TASKS)
            .find(|i| (*base.add(*i)).tid == tid && (*base.add(*i)).state == State::Zombie)
        {
            Some(i) => i,
            None => return (0, 0, false),
        };
        let p = base.add(idx);
        let pml4 = (*p).pml4;
        let kstack = (*p).kstack_base;

        // 1) 地址空间：走一遍低半区，退掉每一页和每一张中间表
        let (pages, tables) = paging::destroy_user_table(pml4);

        // 2) fd 表：管道端点必须关，否则对端等一个不会来的写者
        for f in (*p).fds.iter_mut() {
            match f.kind {
                crate::fd::Kind::PipeRead => crate::pipe::close_read(f.id as usize),
                crate::fd::Kind::PipeWrite => crate::pipe::close_write(f.id as usize),
                _ => {}
            }
            *f = crate::fd::FREE;
        }

        // 3) 内核栈
        if kstack != 0 {
            frames::free_frames(kstack, KSTACK_FRAMES);
        }

        // 4) 槽位
        *p = Task::empty();
        REAPED.fetch_add(1, Ordering::Relaxed);
        (pages, tables, true)
    }
}

/* ---------------- 查询接口（自检用） ---------------- */

pub fn switches() -> u64 {
    SWITCHES.load(Ordering::Relaxed)
}

/// 时间片耗尽导致的切换次数（抢占的直接证据）。
/// 当前抢占禁用深度（诊断用）。
pub fn preempt_depth() -> u32 {
    PREEMPT_OFF.load(Ordering::Relaxed)
}

pub fn preemptions() -> u64 {
    PREEMPTIONS.load(Ordering::Relaxed)
}

pub fn exited() -> u64 {
    EXITED.load(Ordering::Relaxed)
}

/// 被唤醒次数（阻塞唤醒路径的证据）。
pub fn waked() -> u64 {
    WAKED.load(Ordering::Relaxed)
}

/// 被回收的任务数。
pub fn reaped() -> u64 {
    REAPED.load(Ordering::Relaxed)
}

/// 某任务的状态与退出码。
pub fn query(tid: u64) -> Option<(State, i64, u64)> {
    // SAFETY: 单核，只读。
    unsafe {
        (*TASKS.0.get())
            .iter()
            .find(|t| t.tid == tid && t.state != State::Free)
            .map(|t| (t.state, t.exit_code, t.ticks))
    }
}

/// 统计处于某状态的任务数。
/// 已占用的任务槽数（Free 之外都算）。
///
/// 长跑采样用它：僵尸任务没被回收的话，这个数会一直涨。
pub fn count_used() -> usize {
    // SAFETY: 单核，只读。
    unsafe { (*TASKS.0.get()).iter().filter(|t| t.state != State::Free).count() }
}

pub fn count(state: State) -> usize {
    // SAFETY: 单核，只读。
    unsafe { (*TASKS.0.get()).iter().filter(|t| t.state == state).count() }
}

/* ---------------- M9：进程属性 ---------------- */

/// 设置工作目录。路径必须是规范化后的绝对路径。
pub fn set_cwd(path: &[u8]) -> Result<(), i64> {
    if path.first() != Some(&b'/') || path.len() >= CWD_MAX {
        return Err(klinux::errno::err(klinux::errno::ENAMETOOLONG));
    }
    match current_task_mut() {
        Some(t) => {
            t.cwd = cwd_root();
            t.cwd[..path.len()].copy_from_slice(path);
            t.cwd_len = path.len() as u8;
            Ok(())
        }
        None => Err(klinux::errno::err(klinux::errno::ESRCH)),
    }
}

/// 读工作目录，返回长度。
pub fn get_cwd(out: &mut [u8; CWD_MAX]) -> usize {
    match current_task_mut() {
        Some(t) => {
            let n = (t.cwd_len as usize).min(CWD_MAX);
            out[..n].copy_from_slice(&t.cwd[..n]);
            n
        }
        None => {
            out[0] = b'/';
            1
        }
    }
}

pub fn set_fs_base(v: u64) {
    if let Some(t) = current_task_mut() {
        t.fs_base = v;
    }
}

pub fn fs_base() -> u64 {
    current_task_mut().map(|t| t.fs_base).unwrap_or(0)
}

/// 初始化 `brk` 区域（ELF 装载完成后调用）。
/// 设置 `brk` 起点与 `mmap` 起点。
///
/// **只在换成新地址空间时调用**（`execve`、初始装载），所以这里顺手把
/// VMA 表清空——旧程序的匿名映射不该被新程序继承。漏掉这一步的表现是：
/// `execve` 之后新程序访问某个"已经不存在"的区间，页错误被旧 VMA 认领，
/// 于是静默分配了一页，而不是报段错误。
pub fn set_brk_range(start: u64, mmap_top: u64) {
    if let Some(t) = current_task_mut() {
        t.brk_start = start;
        t.brk = start;
        t.mmap_top = mmap_top;
        for i in 0..VMA_MAX {
            t.vmas[i].used = false;
        }
        t.vma_count = 0;
    }
}

pub fn brk() -> u64 {
    current_task_mut().map(|t| t.brk).unwrap_or(0)
}

pub fn set_brk(v: u64) {
    if let Some(t) = current_task_mut() {
        t.brk = v;
    }
}

pub fn brk_start() -> u64 {
    current_task_mut().map(|t| t.brk_start).unwrap_or(0)
}

pub fn mmap_top() -> u64 {
    current_task_mut().map(|t| t.mmap_top).unwrap_or(0)
}

pub fn set_mmap_top(v: u64) {
    if let Some(t) = current_task_mut() {
        t.mmap_top = v;
    }
}

/// 登记一段匿名映射。返回 `false` 表示表满。
///
/// 表满时调用方应退回"立即分配"——绝不能返回错误，
/// 那会让一个本来能跑的程序突然 `mmap` 失败。
pub fn vma_add(start: u64, end: u64, prot: u32) -> bool {
    let t = match current_task_mut() {
        Some(t) => t,
        None => return false,
    };
    for i in 0..VMA_MAX {
        if !t.vmas[i].used {
            t.vmas[i] = Vma { start, end, prot, used: true };
            t.vma_count = t.vma_count.saturating_add(1);
            return true;
        }
    }
    false
}

/// 查一个地址属于哪段匿名映射，返回 `PROT_*`。
pub fn vma_lookup(va: u64) -> Option<u32> {
    let t = current_task_mut()?;
    for i in 0..VMA_MAX {
        let v = t.vmas[i];
        if v.used && va >= v.start && va < v.end {
            return Some(v.prot);
        }
    }
    None
}

/// 拆掉与 `[start, end)` 重叠的登记项（`munmap` 用）。
///
/// 只做"整段丢弃"：区间被部分拆掉时把剩余部分重新登记。
/// 这是近似的，但对"malloc 申请 64 KiB 又整段还回来"这种真实用法足够。
pub fn vma_remove(start: u64, end: u64) {
    let t = match current_task_mut() {
        Some(t) => t,
        None => return,
    };
    for i in 0..VMA_MAX {
        let v = t.vmas[i];
        if !v.used || v.end <= start || v.start >= end {
            continue;
        }
        if v.start >= start && v.end <= end {
            t.vmas[i].used = false;
            t.vma_count = t.vma_count.saturating_sub(1);
        } else if v.start < start && v.end > end {
            // 从中间挖个洞：左半段保留，右半段挪到别的槽位。
            // 这里**不能**调 `vma_add`——它内部会再拿一次 `&mut Task`，
            // 而 `t` 已经借着了（两个可变引用同时存在是 UB，即使编译能过）。
            t.vmas[i].end = start;
            let mut placed = false;
            for j in 0..VMA_MAX {
                if !t.vmas[j].used {
                    t.vmas[j] = Vma { start: end, end: v.end, prot: v.prot, used: true };
                    placed = true;
                    break;
                }
            }
            if !placed {
                // 表满：右半段也丢掉。这会让那段的访问变成"野指针"而不是
                // "按需分配"——是可见的降级，不是静默错误。
                t.vmas[i].end = v.end;
            }
        } else if v.start < start {
            t.vmas[i].end = start;
        } else {
            t.vmas[i].start = end;
        }
    }
}

/// 当前任务还剩几段可登记（诊断用）。
pub fn vma_free_slots() -> usize {
    match current_task_mut() {
        Some(t) => (0..VMA_MAX).filter(|&i| !t.vmas[i].used).count(),
        None => 0,
    }
}

pub fn parent_of(tid: u64) -> u64 {
    // SAFETY: 单核，只读。
    unsafe {
        (*TASKS.0.get())
            .iter()
            .find(|t| t.tid == tid && t.state != State::Free)
            .map(|t| t.parent)
            .unwrap_or(0)
    }
}

/// 当前任务的地址空间（`fork` / `execve` 用）。
pub fn current_pml4() -> u64 {
    current_task_mut().map(|t| t.pml4).unwrap_or(0)
}

/// 给刚创建的任务补上 `brk`/`mmap` 初值（按 tid 找，因为它还没跑起来）。
pub fn init_proc_state(tid: u64, brk_start: u64, mmap_top: u64) {
    // SAFETY: 单核；目标任务还没被调度，不会被并发访问。
    unsafe {
        let t = &mut *TASKS.0.get();
        for slot in t.iter_mut() {
            if slot.tid == tid && slot.state != State::Free {
                slot.brk_start = brk_start;
                slot.brk = brk_start;
                slot.mmap_top = mmap_top;
                return;
            }
        }
    }
}

pub fn set_current_pml4(v: u64) {
    if let Some(t) = current_task_mut() {
        t.pml4 = v;
    }
}

/// 某个槽位里装的是不是用户任务（`fork` 前检查用）。
pub fn is_user_task(tid: u64) -> bool {
    // SAFETY: 单核，只读。
    unsafe {
        (*TASKS.0.get())
            .iter()
            .find(|t| t.tid == tid && t.state != State::Free)
            .map(|t| t.user)
            .unwrap_or(false)
    }
}

/// 当前任务是否运行在 ring3（诊断用）。
pub fn current_is_user() -> bool {
    // SAFETY: 单核，只读。
    unsafe {
        (*TASKS.0.get())
            .get(current_index())
            .map(|t| t.user)
            .unwrap_or(false)
    }
}
