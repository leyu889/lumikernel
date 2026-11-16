//! 受控故障探针（exception fixup）。
//!
//! 用途：**允许内核主动去踩一脚本该致命的异常**，然后安全地回来。
//!
//! 这不是玩具。后面 `copy_from_user` 要向用户态指针读写，而用户态指针
//! 几乎总是坏的；内核必须能"试一下，坏了就报错"，而不是让一个坏指针
//! 把整个系统带走。Linux 的做法是查 fixup 表，这里用同样思路但更直接：
//! 进入前把"出事就跳回这里"写进全局上下文，异常处理器验证**确实是预期的
//! 那类故障、确实是预期地址**之后，改掉 iretq 的目标。
//!
//! 三条安全规则（缺一不可）：
//! 1. 只认 `expect` 指定的向量，别的异常照常走崩溃路径；
//! 2. `#PF` 必须额外校验 CR2 落在预期页，否则说明是别的地方坏了；
//! 3. 恢复后 `armed` 立即清零，一次故障只换一次恢复。

use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use kfault;

use super::frame::InterruptFrame;

/// 与 `stubs.S::__probe` 约定的上下文布局（5 个 u64，顺序固定）。
#[repr(C)]
pub struct ProbeCtx {
    pub rsp: u64,
    pub rip: u64,
    pub addr: u64,
    pub expect: u64,
    pub armed: u64,
}

struct CtxCell(UnsafeCell<ProbeCtx>);

// SAFETY: 单核、中断关闭期间使用；处理器与发起方是同一执行流，
// 不存在并发写（异常是在同一条线程上同步发生的）。
unsafe impl Sync for CtxCell {}

static CTX: CtxCell = CtxCell(UnsafeCell::new(ProbeCtx {
    rsp: 0,
    rip: 0,
    addr: 0,
    expect: 0,
    armed: 0,
}));

static HITS: AtomicU64 = AtomicU64::new(0);
static LAST_VEC: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_ERR: AtomicU64 = AtomicU64::new(0);
static LAST_ADDR: AtomicU64 = AtomicU64::new(0);

/// 探针要执行的动作。
///
/// `repr(C)` 不是为了布局（汇编只把它当不透明指针传），
/// 而是为了让 `extern "C"` 声明通过 FFI 安全检查。
#[derive(Clone, Copy)]
#[repr(C)]
pub enum ProbeAction {
    /// 整数除零（`div` 指令，真正的硬件 #DE，不是 Rust 的 panic 检查）。
    DivideByZero,
    /// 从某地址读 8 字节。
    Read(u64),
    /// 往某地址写 8 字节。
    Write(u64, u64),
    /// 执行 `int3`。
    Int3,
}

extern "C" {
    /// 由 `stubs.S` 提供：执行 `action`，出预期故障就回到登记点。
    /// 返回 0 = 正常完成，1 = 从故障恢复。
    fn __probe(ctx: *mut ProbeCtx, action: *const ProbeAction) -> u64;
}

/// 被 `__probe` 调用的实际执行体。必须是 `extern "C"`：
/// 汇编里直接 `call` 它，链接器需要稳定的符号与调用约定。
#[no_mangle]
pub extern "C" fn probe_action(action: *const ProbeAction) -> u64 {
    // SAFETY: 指针来自 `run` 里的栈上局部变量，在 `__probe` 返回前一直有效。
    let a = unsafe { *action };
    match a {
        ProbeAction::DivideByZero => {
            let zero: u64 = 0;
            // SAFETY: 故意让 `div` 除零触发 #DE；不会返回，
            // 由异常处理器把控制流改到恢复点。
            unsafe {
                asm!(
                    "xor edx, edx",
                    "mov eax, 1",
                    "div {d}",
                    d = in(reg) zero,
                    out("rax") _,
                    out("rdx") _,
                    options(nostack)
                );
            }
            0
        }
        ProbeAction::Read(p) => {
            // SAFETY: 故意读可能未映射的地址；由探针机制兜底。
            unsafe { core::ptr::read_volatile(p as *const u64) }
        }
        ProbeAction::Write(p, v) => {
            // SAFETY: 同 Read。
            unsafe { core::ptr::write_volatile(p as *mut u64, v) };
            v
        }
        ProbeAction::Int3 => {
            // SAFETY: 单字节 int3，处理器会推进 rip 后返回。
            unsafe { asm!("int3", options(nostack)) };
            0
        }
    }
}

/// 探针结果。
#[derive(Clone, Copy, Debug)]
pub struct Outcome {
    /// 是否从异常中恢复（false 表示指令正常执行完了）。
    pub recovered: bool,
    /// `probe_action` 的返回值（恢复时为 0）。
    pub value: u64,
}

/// 执行一次受控故障。
///
/// - `expect`：预期的异常向量（[`kfault::VEC_DE`]、[`kfault::VEC_PF`] 等）；
/// - `addr`：预期发生故障的地址（只对 `#PF` 校验，比页粒度）；
/// - `action`：实际动作。
pub fn run(expect: u64, addr: u64, action: ProbeAction) -> Outcome {
    // 探针上下文是全局单例：被抢占后另一个任务也来探针就会互相踩，
    // 于是恢复点指向别人的栈——症状是随机的野跳转。
    let _g = crate::task::PreemptGuard::new();
    // SAFETY: 单核执行；`CTX` 在调用期间独占。
    let ctx = unsafe { &mut *CTX.0.get() };
    ctx.addr = addr;
    ctx.expect = expect;
    ctx.armed = 0;
    let cp = ctx as *mut ProbeCtx;
    let ap = core::ptr::addr_of!(action);
    // SAFETY: 两个指针都指向本栈帧上的有效对象，生命周期覆盖整个调用。
    let r = unsafe { __probe(cp, ap) };
    Outcome {
        recovered: r == 1,
        value: r,
    }
}

/// 异常处理器调用：能否把这次故障"接住"。
///
/// 返回 true 表示现场已被改写成"从恢复点继续"，处理器必须直接返回。
pub fn try_recover(f: &mut InterruptFrame) -> bool {
    // SAFETY: 与 `run` 同一执行流，且此时 `run` 尚未返回（栈上有它的帧）。
    let ctx = unsafe { &mut *CTX.0.get() };
    if ctx.armed != 1 {
        return false;
    }
    if f.vector != ctx.expect {
        return false;
    }
    let cr2 = super::frame::read_cr2();
    if ctx.expect == kfault::VEC_PF {
        // 只接受"预期的那一页"上的缺页。地址不符说明是别的东西坏了，
        // 这时恢复只会把真正的 bug 藏起来。
        if cr2 & !0xFFF != ctx.addr & !0xFFF {
            return false;
        }
    }
    ctx.armed = 0;
    HITS.fetch_add(1, Ordering::Relaxed);
    LAST_VEC.store(f.vector, Ordering::Relaxed);
    LAST_ERR.store(f.error, Ordering::Relaxed);
    LAST_ADDR.store(cr2, Ordering::Relaxed);

    // 改现场：iretq 会跳到登记点，并把栈指针换回登记时的值。
    f.rip = ctx.rip;
    f.rsp = ctx.rsp;
    true
}

/// 探针命中次数。
pub fn hits() -> u64 {
    HITS.load(Ordering::Relaxed)
}

/// 最近一次被接住的故障 `(vector, error, cr2)`。
pub fn last_fault() -> (u64, u64, u64) {
    (
        LAST_VEC.load(Ordering::Relaxed),
        LAST_ERR.load(Ordering::Relaxed),
        LAST_ADDR.load(Ordering::Relaxed),
    )
}
