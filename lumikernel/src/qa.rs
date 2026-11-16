//! M11 · 质量工程：把"稳"变成可数的东西
//!
//! 三个部分：
//!
//! - [`fault`]：故障注入。在帧分配 / 堆分配 / 块设备读写上按计划制造失败，
//!   让"OOM 时会怎样"这类问题在自检里就能跑出来，而不是等真 OOM。
//! - [`soak`]：长跑采样。每轮循环记一次（空闲帧、堆占用、任务数），
//!   用回归斜率判定有没有泄漏。
//! - [`perf`]：性能基准。统一用 TSC 计时（比 tick 精确 100 倍以上），
//!   换算成纳秒输出，方便和 Linux 上的同类数据放在一起看。
//!
//! **判定逻辑全部在 `kqa` crate 里**（纯函数、宿主机可单测）；
//! 这里只负责"接到硬件上"——原子量、锁、TSC。这样修一个判定 bug
//! 不需要起虚拟机，跑一遍 `cargo test -p kqa` 就行。

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use kqa::fault::Point;

pub mod soak;

/* ==================== 故障注入 ==================== */

pub mod fault {
    use super::*;

    /// 未注入。
    const NONE: u8 = 0xFF;

    static POINT: AtomicU8 = AtomicU8::new(NONE);
    static SKIP: AtomicU32 = AtomicU32::new(0);
    static PERIOD: AtomicU32 = AtomicU32::new(1);
    static BUDGET: AtomicU32 = AtomicU32::new(0);
    static SEEN: AtomicU32 = AtomicU32::new(0);
    static FIRED: AtomicU64 = AtomicU64::new(0);

    /// 装机一次注入计划。
    ///
    /// 参数含义见 [`kqa::fault::Plan::new`]。判定用原子量而不是锁：
    /// 这条路径会在**分配失败的现场**被调用，那里绝不能再依赖
    /// "能拿到锁"这件事（锁的持有者可能正是上一个失败的分配）。
    pub fn arm(point: Point, skip: u32, period: u32, budget: u32) {
        SEEN.store(0, Ordering::Relaxed);
        FIRED.store(0, Ordering::Relaxed);
        SKIP.store(skip, Ordering::Relaxed);
        PERIOD.store(period.max(1), Ordering::Relaxed);
        BUDGET.store(budget, Ordering::Relaxed);
        // 最后写 POINT：前面几个值必须在"生效"之前就位
        POINT.store(point as u8, Ordering::Release);
    }

    pub fn disarm() {
        POINT.store(NONE, Ordering::Release);
    }

    pub fn armed() -> Option<Point> {
        Point::from_u8(POINT.load(Ordering::Acquire))
    }

    /// 已经注入了多少次。
    pub fn fired() -> u64 {
        FIRED.load(Ordering::Relaxed)
    }

    /// 被判定了多少次（含放过的）。
    pub fn seen() -> u32 {
        SEEN.load(Ordering::Relaxed)
    }

    /// 分配器调用它：`true` = 这一次应当失败。
    ///
    /// 语义与 [`kqa::fault::Plan::should_fail`] 完全一致，只是把状态搬到了原子量上。
    #[inline]
    pub fn deny(point: Point) -> bool {
        if POINT.load(Ordering::Acquire) != point as u8 {
            return false;
        }
        let budget = BUDGET.load(Ordering::Relaxed);
        if budget != 0 && FIRED.load(Ordering::Relaxed) >= budget as u64 {
            // 预算用完，自动解除——否则后面的正常路径会被一直干扰
            POINT.store(NONE, Ordering::Release);
            return false;
        }
        let seen = SEEN.fetch_add(1, Ordering::Relaxed) + 1;
        let skip = SKIP.load(Ordering::Relaxed);
        if seen <= skip {
            return false;
        }
        if (seen - skip) % PERIOD.load(Ordering::Relaxed) != 0 {
            return false;
        }
        FIRED.fetch_add(1, Ordering::Relaxed);
        true
    }
}

/* ==================== 性能计时 ==================== */

pub mod perf {
    /// 读时间戳计数器。
    #[inline]
    pub fn tsc() -> u64 {
        crate::intr::apic::tsc_now()
    }

    /// TSC 频率（Hz）。由 M4 的 RTC 校准得到；没校准成功时返回 0，
    /// 调用方必须处理（`cycles_to_ns` 会原样返回 0 表示"不知道"）。
    pub fn tsc_hz() -> u64 {
        crate::intr::apic::tsc_per_10ms() * 100
    }

    /// 循环数 → 纳秒。用 u128 中间量避免溢出（1e9 × 1e9 会爆 u64）。
    pub fn cycles_to_ns(c: u64) -> u64 {
        let hz = tsc_hz();
        if hz == 0 {
            return 0;
        }
        ((c as u128 * 1_000_000_000u128) / hz as u128) as u64
    }

    /// 纳秒 → 每千次操作的微秒数（打印时更稳，不会全是小数）。
    pub fn ns_per_kilo(ns: u64) -> u64 {
        ns
    }

    /// 跑一段闭包并返回 `(耗时 TSC, 闭包返回值)`。带 `lfence` 防止乱序。
    #[inline]
    pub fn timed<R>(f: impl FnOnce() -> R) -> (u64, R) {
        // SAFETY: rdtsc/lfence 无副作用；只影响计时精度。
        let t0 = tsc();
        let r = f();
        let t1 = tsc();
        (t1.wrapping_sub(t0), r)
    }
}

/* ==================== 统一入口 ==================== */

/// 自检开始时调用一次：把注入状态清干净。
pub fn reset() {
    fault::disarm();
}
