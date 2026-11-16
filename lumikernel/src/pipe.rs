//! 管道（M7）：内核态环形缓冲 + 真正的阻塞等待
//!
//! 这是内核第一次**主动把任务挂起**，而不是靠 `yield` 轮询。
//! 两者的区别不是性能，而是正确性：轮询在"没有别的任务可跑"时会空转烧时间，
//! 而阻塞会把 CPU 交出去，自己在等待队列里睡觉，直到有人叫它。
//!
//! ## 为什么关中断是必须的
//!
//! "看缓冲区 → 发现自己该等 → 登记为等待者 → 切走"这四步必须**原子**。
//! 中间只要被打断一次（时钟中断 → 抢占 → 写者跑完把数据写进来并发了一次唤醒），
//! 唤醒就会落空：数据已经在缓冲里，而我们正要睡下去等它。
//! 这就是经典的 **lost wakeup**，现象是"偶尔卡死，重跑又好了"。
//!
//! 做法：这四步全程 `cli`（用 [`disable_irq_save`] 保存原状态）。单核上关中断
//! 就等于"没有别人能插进来"；多核上还要加锁，两个都做。
//! 登记完、**锁放掉之后**才切走——拿着锁切走会死锁（醒来之前没人能放这把锁）。
//!
//! ## 关中断睡下去，为什么还能醒
//!
//! 因为"醒来"不靠本核的时钟中断：写者在**另一个任务**里调动
//! [`crate::task::wake`]，把它从阻塞态改成就绪态并塞回运行队列。
//! 调度器下次挑到它时 `switch_to` 返回，`iretq` 恢复的是它**自己**保存的
//! RFLAGS（IF=1），中断自然就重新开了。

use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch::sync::{disable_irq_save, restore_irq, IrqSpinlock};
use crate::task;

/// 系统里最多几个管道。
pub const MAX_PIPES: usize = 4;
/// 每个管道的容量（字节）。
pub const CAP: usize = 256;
/// 每条管道每侧最多几个等待者。
const MAX_WAITERS: usize = 4;

pub const EBADF: i64 = -9;
pub const EAGAIN: i64 = -11;
pub const EPIPE: i64 = -32;

struct Pipe {
    used: bool,
    buf: [u8; CAP],
    /// 下一个要读的位置。
    head: usize,
    /// 缓冲里已有多少字节。
    len: usize,
    /// 打开着的读端 / 写端数量（不是"创建过"，是"还开着"）。
    readers: usize,
    writers: usize,
    rwait: [u64; MAX_WAITERS],
    rn: usize,
    wwait: [u64; MAX_WAITERS],
    wn: usize,
}

const EMPTY: Pipe = Pipe {
    used: false,
    buf: [0u8; CAP],
    head: 0,
    len: 0,
    readers: 0,
    writers: 0,
    rwait: [0; MAX_WAITERS],
    rn: 0,
    wwait: [0; MAX_WAITERS],
    wn: 0,
};

static PIPES: IrqSpinlock<[Pipe; MAX_PIPES]> = IrqSpinlock::new([EMPTY; MAX_PIPES]);

static CREATED: AtomicU32 = AtomicU32::new(0);
static BLOCKS: AtomicU32 = AtomicU32::new(0);
static WAKES: AtomicU32 = AtomicU32::new(0);

fn push_waiter(list: &mut [u64; MAX_WAITERS], n: &mut usize, tid: u64) -> bool {
    if *n >= MAX_WAITERS {
        return false;
    }
    list[*n] = tid;
    *n += 1;
    true
}

fn drop_waiter(list: &mut [u64; MAX_WAITERS], n: &mut usize, tid: u64) {
    let mut i = 0;
    while i < *n {
        if list[i] == tid {
            list[i] = list[*n - 1];
            *n -= 1;
            return;
        }
        i += 1;
    }
}

/// 唤醒一批任务（必须在**放锁之后**调用：`task::wake` 要改调度器）。
fn wake_list(list: &[u64; MAX_WAITERS], n: usize) {
    for i in 0..n {
        if task::wake(list[i]) {
            WAKES.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// 一次操作的结果：要么干完了，要么该睡，要么出错。
enum Step {
    Done(usize),
    Block,
    Fail(i64),
}

/// 创建一个管道（读端/写端各算 1 个打开者）。
pub fn create() -> Result<usize, ()> {
    let mut g = PIPES.lock();
    for (i, p) in g.iter_mut().enumerate() {
        if !p.used {
            *p = EMPTY;
            p.used = true;
            p.readers = 1;
            p.writers = 1;
            CREATED.fetch_add(1, Ordering::Relaxed);
            return Ok(i);
        }
    }
    Err(())
}

/// 关掉一个读端。
pub fn close_read(id: usize) {
    let mut wl = [0u64; MAX_WAITERS];
    let mut wn = 0usize;
    {
        let mut g = PIPES.lock();
        let Some(p) = g.get_mut(id) else { return };
        if !p.used {
            return;
        }
        p.readers = p.readers.saturating_sub(1);
        if p.readers == 0 {
            // 最后一个读端没了：所有写者都该拿到 EPIPE，不能让他们继续等
            wl = p.wwait;
            wn = p.wn;
            p.wn = 0;
            if p.writers == 0 {
                p.used = false;
            }
        }
    }
    wake_list(&wl, wn);
}

/// 关掉一个写端。最后一个写端关掉时，读者会读到 EOF（0 字节）。
pub fn close_write(id: usize) {
    let mut rl = [0u64; MAX_WAITERS];
    let mut rn = 0usize;
    {
        let mut g = PIPES.lock();
        let Some(p) = g.get_mut(id) else { return };
        if !p.used {
            return;
        }
        p.writers = p.writers.saturating_sub(1);
        if p.writers == 0 {
            rl = p.rwait;
            rn = p.rn;
            p.rn = 0;
            if p.readers == 0 {
                p.used = false;
            }
        }
    }
    wake_list(&rl, rn);
}

/// 从管道读。没有数据时**阻塞**，直到有人写或写端全关。
pub fn read(id: usize, out: &mut [u8]) -> Result<usize, i64> {
    if out.is_empty() {
        return Ok(0);
    }
    if id >= MAX_PIPES {
        return Err(EBADF);
    }
    let me = task::current_tid();
    loop {
        // ---- 原子窗口：看状态、搬数据/登记等待，全程关中断 ----
        let irq = disable_irq_save();
        let mut wl = [0u64; MAX_WAITERS];
        let mut wn = 0usize;
        let step = {
            let mut g = PIPES.lock();
            match g.get_mut(id) {
                None => Step::Fail(EBADF),
                Some(p) if !p.used => Step::Fail(EBADF),
                Some(p) if p.len > 0 => {
                    let n = p.len.min(out.len());
                    for i in 0..n {
                        out[i] = p.buf[(p.head + i) % CAP];
                    }
                    p.head = (p.head + n) % CAP;
                    p.len -= n;
                    // 空出了位置：写者都能再写一点
                    wl = p.wwait;
                    wn = p.wn;
                    p.wn = 0;
                    Step::Done(n)
                }
                Some(p) if p.writers == 0 => Step::Done(0), // EOF
                Some(p) => {
                    if push_waiter(&mut p.rwait, &mut p.rn, me) {
                        Step::Block
                    } else {
                        Step::Fail(EAGAIN)
                    }
                }
            }
        };
        // 锁在这里已经放掉（上面的块作用域结束）
        match step {
            Step::Done(n) => {
                wake_list(&wl, wn);
                restore_irq(irq);
                return Ok(n);
            }
            Step::Fail(e) => {
                restore_irq(irq);
                return Err(e);
            }
            Step::Block => {
                BLOCKS.fetch_add(1, Ordering::Relaxed);
                // 中断仍关着 —— 此刻写者插不进来，登记不会失效
                let switched = task::block_current();
                restore_irq(irq);
                if !switched {
                    // 没有别的可运行任务：撤销登记，让调用方自己决定
                    let mut g = PIPES.lock();
                    if let Some(p) = g.get_mut(id) {
                        drop_waiter(&mut p.rwait, &mut p.rn, me);
                    }
                    return Err(EAGAIN);
                }
                // 被唤醒（或被 close 唤醒）后回到顶部重新看状态
            }
        }
    }
}

/// 往管道写。满了就**阻塞**，直到读者腾出空间。
pub fn write(id: usize, data: &[u8]) -> Result<usize, i64> {
    if data.is_empty() {
        return Ok(0);
    }
    if id >= MAX_PIPES {
        return Err(EBADF);
    }
    let me = task::current_tid();
    loop {
        let irq = disable_irq_save();
        let mut rl = [0u64; MAX_WAITERS];
        let mut rn = 0usize;
        let step = {
            let mut g = PIPES.lock();
            match g.get_mut(id) {
                None => Step::Fail(EBADF),
                Some(p) if !p.used => Step::Fail(EBADF),
                Some(p) if p.readers == 0 => Step::Fail(EPIPE),
                Some(p) if p.len < CAP => {
                    let space = CAP - p.len;
                    let n = space.min(data.len());
                    for i in 0..n {
                        p.buf[(p.head + p.len + i) % CAP] = data[i];
                    }
                    p.len += n;
                    rl = p.rwait;
                    rn = p.rn;
                    p.rn = 0;
                    Step::Done(n)
                }
                Some(p) => {
                    if push_waiter(&mut p.wwait, &mut p.wn, me) {
                        Step::Block
                    } else {
                        Step::Fail(EAGAIN)
                    }
                }
            }
        };
        match step {
            Step::Done(n) => {
                wake_list(&rl, rn);
                restore_irq(irq);
                return Ok(n);
            }
            Step::Fail(e) => {
                restore_irq(irq);
                return Err(e);
            }
            Step::Block => {
                BLOCKS.fetch_add(1, Ordering::Relaxed);
                let switched = task::block_current();
                restore_irq(irq);
                if !switched {
                    let mut g = PIPES.lock();
                    if let Some(p) = g.get_mut(id) {
                        drop_waiter(&mut p.wwait, &mut p.wn, me);
                    }
                    return Err(EAGAIN);
                }
            }
        }
    }
}

/* ---------------- 自检观测 ---------------- */

/// 已创建的管道数。
pub fn created() -> u32 {
    CREATED.load(Ordering::Relaxed)
}

/// 累计阻塞次数（每真的睡下去一次算一次）。
pub fn blocks() -> u32 {
    BLOCKS.load(Ordering::Relaxed)
}

/// 累计唤醒次数。
#[allow(dead_code)] // 自检里只用了 `created()`；这个留给以后排查漏唤醒
pub fn wakes() -> u32 {
    WAKES.load(Ordering::Relaxed)
}
