//! 8253/8254 可编程间隔定时器（PIT）
//!
//! M4 起它的角色变了：
//! - **主时钟换成 APIC 定时器**（每个核一个），PIT 不再是 tick 来源；
//! - PIT 还剩两个用途：
//!   1. 找不到 ACPI/APIC 时的退路（仍然能跑，只是单核）；
//!   2. **校准 APIC 定时器的基准**——它的 1.193182 MHz 是硬件常量，
//!      比任何软件延时都可靠。前提是这条中断链路真的通（自检里单独验过）。
//!
//! 历史教训：曾经用"通道 2 + 端口 0x61 轮询 OUT"做 10 ms 延时，
//! 实测那段延时要 ~71 ms，直接把 APIC 定时器校准成了 14 Hz。
//! 轮询硬件状态位看起来"精确"，实际受门控、模式、上次残留状态影响；
//! **用真实中断计数才可靠**。那段代码已经删掉了。

use core::sync::atomic::{AtomicU32, Ordering};

use super::frame::outb;

const PIT_CH0: u16 = 0x40;
const PIT_CMD: u16 = 0x43;

/// 输入时钟：1.193182 MHz（NTSC 彩电色副载波的 4 倍，历史遗留值）。
pub const PIT_INPUT_HZ: u32 = 1_193_182;

/// tick 频率。
pub const HZ: u32 = 100;

static DIVISOR: AtomicU32 = AtomicU32::new(0);

/// 让通道 0 以 [`HZ`] 频率产生中断（IRQ0）。
pub fn init() {
    let divisor = (PIT_INPUT_HZ / HZ) as u16;
    DIVISOR.store(divisor as u32, Ordering::Relaxed);
    // SAFETY: 写 PIT 命令口/数据口，不涉及任何内存安全。
    unsafe {
        outb(PIT_CMD, 0x36); // 通道0 | 模式3（方波）| 先低后高 | 二进制
        outb(PIT_CH0, (divisor & 0xFF) as u8);
        outb(PIT_CH0, (divisor >> 8) as u8);
    }
}

/// 当前分频值（诊断用）。
pub fn divisor() -> u32 {
    DIVISOR.load(Ordering::Relaxed)
}
