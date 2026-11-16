//! 长跑采样（S3）。
//!
//! 每个采样点记录四个"只增不减才叫泄漏"的量：
//!
//! - **空闲帧数**：少一页就是少一页，最直接的泄漏指标；
//! - **堆已用字节**：内核里所有 `Box`/`Vec` 都从这里出；
//! - **任务槽使用数**：退出但没回收的任务会卡在这里；
//! - **打开的文件描述符数**：fd 泄漏比内存泄漏更快把系统拖死。
//!
//! 采样本身要轻：一次采样只读几个原子量 + 加锁推入环形缓冲。
//! 长跑里它被调用百万次也无所谓（相对于每轮的分配/释放，它便宜得多）。

use crate::arch::sync::IrqSpinlock;
use kqa::soak::Samples;

/// 采样窗口：保留最近 64 个点。
///
/// 为什么是 64 而不是 4096：判定用最小二乘，点越多对"最近的变化"越不敏感。
/// 64 个点、每点 1000 轮，相当于"最近 6.4 万轮"的窗口，
/// 既能看到慢漏，又不至于被早期的一次性增长带偏。
pub const WINDOW: usize = 64;

/// 一次采样。
#[derive(Clone, Copy, Default)]
pub struct Sample {
    /// 第几轮（从 1 开始）。
    pub round: u64,
    pub free_frames: u64,
    pub heap_used: u64,
    pub tasks_used: u64,
    pub fds_open: u64,
}

/// 采样序列（四路独立回归）。
struct Series {
    rounds: u64,
    free_frames: Samples<WINDOW>,
    heap_used: Samples<WINDOW>,
    tasks_used: Samples<WINDOW>,
    fds_open: Samples<WINDOW>,
}

static SERIES: IrqSpinlock<Series> = IrqSpinlock::new(Series {
    rounds: 0,
    free_frames: Samples::new(),
    heap_used: Samples::new(),
    tasks_used: Samples::new(),
    fds_open: Samples::new(),
});

/// 采一次样，返回采样点。
pub fn record() -> Sample {
    let free = crate::mm::frames::stats().map(|s| s.free_frames as u64).unwrap_or(0);
    let heap = crate::mm::heap::stats().map(|s| s.used as u64).unwrap_or(0);
    let tasks = crate::task::count_used() as u64;
    let fds = crate::fd::open_count() as u64;

    let mut g = SERIES.lock();
    g.rounds += 1;
    let s = Sample {
        round: g.rounds,
        free_frames: free,
        heap_used: heap,
        tasks_used: tasks,
        fds_open: fds,
    };
    g.free_frames.push(free);
    g.heap_used.push(heap);
    g.tasks_used.push(tasks);
    g.fds_open.push(fds);
    s
}

/// 长跑判定的结果。
#[derive(Clone, Copy, Default)]
pub struct Report {
    pub samples: usize,
    pub rounds: u64,
    /// 千分斜率：每 1000 个采样点变化多少单位。负值 = 空闲帧在减少。
    pub free_frames_slope: i64,
    pub heap_used_slope: i64,
    pub tasks_slope: i64,
    pub fds_slope: i64,
    /// 首尾差。
    pub free_frames_delta: i64,
    pub heap_used_delta: i64,
}

impl Report {
    /// 是否满足"零漂移"。
    ///
    /// 阈值取 1（千分之一单位/采样点）——比它更小的斜率在 64 点窗口里
    /// 连一个单位的台阶都构不成，属于量化噪声。
    pub fn is_flat(&self) -> bool {
        self.free_frames_slope.abs() <= 1
            && self.heap_used_slope.abs() <= 1
            && self.fds_slope.abs() <= 1
            && self.tasks_slope.abs() <= 1
            && self.free_frames_delta == 0
            && self.heap_used_delta == 0
    }
}

pub fn report() -> Report {
    let g = SERIES.lock();
    Report {
        samples: g.free_frames.len(),
        rounds: g.rounds,
        free_frames_slope: g.free_frames.slope_milli(),
        heap_used_slope: g.heap_used.slope_milli(),
        tasks_slope: g.tasks_used.slope_milli(),
        fds_slope: g.fds_open.slope_milli(),
        free_frames_delta: g.free_frames.delta(),
        heap_used_delta: g.heap_used.delta(),
    }
}

/// 清空序列（自检中途换场景时用）。
pub fn reset() {
    let mut g = SERIES.lock();
    g.rounds = 0;
    g.free_frames = Samples::new();
    g.heap_used = Samples::new();
    g.tasks_used = Samples::new();
    g.fds_open = Samples::new();
}

/// 当前是否"完全静止"：最近两个采样点一模一样。
///
/// 比回归更严格的一条断言：长跑里每轮结束都该回到同一个状态码，
/// 粘连不上就说明有东西没释放（哪怕只有一页）。
pub fn is_still() -> bool {
    let g = SERIES.lock();
    if g.free_frames.len() < 2 {
        return true;
    }
    g.free_frames.first() == g.free_frames.last()
        && g.heap_used.first() == g.heap_used.last()
        && g.fds_open.first() == g.fds_open.last()
}
