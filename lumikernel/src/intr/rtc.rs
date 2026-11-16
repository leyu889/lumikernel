//! CMOS 实时时钟（RTC）：只用来当**独立时间基准**
//!
//! 为什么需要它：校准 APIC 定时器需要一个"与被测时钟无关"的参考。
//! 之前踩过的坑正好是循环依赖：
//!   - 用 PIT 通道 2 轮询当 10 ms → 实际 71 ms → 定时器被校成 14 Hz；
//!   - 改用"PIT 的 10 个中断"当 100 ms → 实测定时器仍只有 24 Hz，
//!     说明那条链路自己的频率也不是它声称的 100 Hz。
//! 拿被测对象（或依赖它的东西）当尺子，永远得不到真相。
//!
//! RTC 走独立的晶振，秒寄存器就是秒，误差对我们这个用途完全够用。

use super::frame::{inb, outb};

/// 读一个 CMOS 寄存器。
fn cmos(reg: u8) -> u8 {
    // SAFETY: 0x70 是索引口、0x71 是数据口；读 CMOS 无副作用。
    unsafe {
        outb(0x70, reg);
        inb(0x71)
    }
}

/// 寄存器 0x0B 的 bit2 表示"二进制模式"；否则是 BCD。
fn is_binary() -> bool {
    cmos(0x0B) & 0x04 != 0
}

fn to_dec(v: u8) -> u8 {
    if is_binary() {
        v
    } else {
        (v >> 4) * 10 + (v & 0x0F)
    }
}

/// 当前秒（0..59）。
pub fn seconds() -> u8 {
    to_dec(cmos(0x00))
}

/// 等到秒寄存器的下一次跳变（最多等 1 秒多一点）。
///
/// 返回 false 表示等超时（CMOS 不可用），调用方应当放弃这次校准而不是死等。
pub fn wait_edge() -> bool {
    let start = seconds();
    let mut guard = 0u32;
    while seconds() == start {
        guard += 1;
        // 每次循环都要读一次 I/O 口（微秒级），所以这里不需要额外延时；
        // 上限按"一秒内最多允许的轮询次数"给足余量。
        if guard > 50_000_000 {
            return false;
        }
    }
    true
}
