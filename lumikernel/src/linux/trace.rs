//! 系统调用轨迹（M9 的可观测性）
//!
//! 为什么内核需要这个：跑别人的二进制时，"它死了"这句话没有任何信息量。
//! 需要知道的是"它死之前最后几次系统调用是什么、返回了什么"。
//! 这台机器上没有 gdb（也没法在 VirtualBox 里挂调试器），
//! 所以轨迹必须由内核自己留。
//!
//! 代价很小：每次系统调用多两次原子操作 + 四个 8 字节写入。
//! 环形缓冲 512 项 ≈ 16 KiB `.bss`，出问题时能回看最近几百次调用。
//!
//! 输出只在**用户态异常**时打印（正常路径一声不响），所以它不会污染日志。

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// 环形缓冲长度。
pub const TRACE_LEN: usize = 512;

#[derive(Clone, Copy)]
pub struct Entry {
    pub nr: u16,
    pub ret: i64,
    pub a0: u64,
    pub a1: u64,
    pub a2: u64,
    pub a3: u64,
}

const EMPTY: Entry = Entry {
    nr: 0,
    ret: 0,
    a0: 0,
    a1: 0,
    a2: 0,
    a3: 0,
};

struct TraceCell(UnsafeCell<[Entry; TRACE_LEN]>);
// SAFETY: 单核调度；索引用原子自增，每个执行流独占自己的槽位。
unsafe impl Sync for TraceCell {}

static TRACE: TraceCell = TraceCell(UnsafeCell::new([EMPTY; TRACE_LEN]));
static IDX: AtomicUsize = AtomicUsize::new(0);
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// 记一笔。`nr` 是系统调用号，`ret` 是返回值。
pub fn record(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, ret: i64) {
    let i = IDX.fetch_add(1, Ordering::Relaxed) % TRACE_LEN;
    // SAFETY: i 是本执行流独占的槽位（fetch_add 保证不重复）。
    unsafe {
        let t = &mut *TRACE.0.get();
        t[i] = Entry {
            nr: nr as u16,
            ret,
            a0,
            a1,
            a2,
            a3,
        };
    }
    COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn count() -> usize {
    COUNT.load(Ordering::Relaxed)
}

/// 打印最近 `n` 条（按发生顺序）。只在出错路径调用。
pub fn dump(n: usize) {
    let total = COUNT.load(Ordering::Relaxed);
    let n = core::cmp::min(n, core::cmp::min(total, TRACE_LEN));
    if n == 0 {
        kserialln!("[trace] 还没有任何系统调用");
        return;
    }
    kserialln!("[trace] 最近 {} 次系统调用（nr 参数1 参数2 -> 返回）：", n);
    let start = total - n;
    for k in start..total {
        let i = k % TRACE_LEN;
        // SAFETY: 只读已写入的槽位。
        let e = unsafe { (*TRACE.0.get())[i] };
        kserialln!(
            "        {:>4} {:#x} {:#x} {:#x} {:#x} -> {}",
            e.nr,
            e.a0,
            e.a1,
            e.a2,
            e.a3,
            e.ret
        );
    }
}
