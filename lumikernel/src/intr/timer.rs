//! 时间基准（M4）：谁在喂 tick 不重要，重要的是全内核只有一个计数器。
//!
//! M2/M3 时计数器挂在 PIT 上，名字也叫 `pit::ticks()`。换到 APIC 定时器之后
//! 这个名字会误导人，所以把它独立出来：**tick 的来源可变（PIT / APIC 定时器），
//! 读计数的人不变**。

use core::sync::atomic::{AtomicU64, Ordering};

/// 系统 tick 频率（Hz）。100 Hz = 10 ms 一个时间片。
pub const HZ: u32 = 100;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// 由中断处理器调用（每核都会调用；BSP 的计数是全局时钟）。
#[inline]
pub fn on_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

#[inline]
pub fn millis() -> u64 {
    ticks() * 1000 / HZ as u64
}
