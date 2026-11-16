//! 8259A 可编程中断控制器（PIC）重映射。
//!
//! 为什么必须重映射：上电后 IRQ0..7 落在向量 0x08..0x0F，正好压在 CPU 异常上
//! （IRQ0 盖住 #DF，IRQ1 盖住 #GP…）。不重映射的话，一次定时器中断会被当成
//! 双重故障处理。这是 PC 架构遗留的历史包袱，所有 OS 都挪到 0x20 起。
//!
//! M2 只用 IRQ0（PIT）。本地 APIC 在 M3 与多核一起接，那时 8259A 会被屏蔽。

use super::frame::{inb, outb};

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// 主片偏移：IRQ0 → 向量 0x20。
pub const MASTER_OFFSET: u8 = 0x20;
/// 从片偏移：IRQ8 → 向量 0x28。
pub const SLAVE_OFFSET: u8 = 0x28;

/// 向端口 0x80 写一个字节，给老式 PIC 一点"反应时间"。
///
/// 8259A 在一条命令与下一条之间需要几个总线周期；现代 CPU 太快，
/// 中间不插延迟会把两条命令挤在一起，表现为偶发的初始化失败。
#[inline]
fn io_wait() {
    // SAFETY: 0x80 是 POST 诊断端口，写任意值无副作用。
    unsafe { outb(0x80, 0) };
}

/// 重映射两片 PIC 到 0x20 / 0x28，并只放行 IRQ0。
pub fn remap() {
    // SAFETY: 以下全部是对 8259A 标准端口的初始化序列（ICW1..ICW4），
    // 参数含义见 8259A 数据手册；启动阶段中断关闭，不会被中断打断。
    unsafe {
        // ICW1：边沿触发 + 需要 ICW4
        outb(PIC1_CMD, 0x11);
        io_wait();
        outb(PIC2_CMD, 0x11);
        io_wait();
        // ICW2：向量基址
        outb(PIC1_DATA, MASTER_OFFSET);
        io_wait();
        outb(PIC2_DATA, SLAVE_OFFSET);
        io_wait();
        // ICW3：主片 IRQ2 接从片；从片自己是 2 号
        outb(PIC1_DATA, 0x04);
        io_wait();
        outb(PIC2_DATA, 0x02);
        io_wait();
        // ICW4：8086/88 模式，普通 EOI
        outb(PIC1_DATA, 0x01);
        io_wait();
        outb(PIC2_DATA, 0x01);
        io_wait();

        // 掩码：只放行 IRQ0（PIT），其余全关。
        // 关掉不等于不会响：未掩码的 IRQ 若悬空会持续触发（IRQ7 假中断）。
        outb(PIC1_DATA, 0xFE);
        outb(PIC2_DATA, 0xFF);
    }
}

/// 屏蔽全部中断源（停机前用）。
pub fn mask_all() {
    // SAFETY: 写 IMR 是 PIC 的标准操作。
    unsafe {
        outb(PIC1_DATA, 0xFF);
        outb(PIC2_DATA, 0xFF);
    }
}

/// 读回两片的掩码寄存器（自检用）。
pub fn read_masks() -> (u8, u8) {
    // SAFETY: 读 IMR 只是读端口。
    unsafe { (inb(PIC1_DATA), inb(PIC2_DATA)) }
}

/// 发送 EOI（中断结束）。
///
/// IRQ8..15 必须**先**给从片发，再给主片发；顺序反了从片会一直挂着
/// ISR 位，那个 IRQ 从此再也不来。
pub fn eoi(vector: u64) {
    // SAFETY: 往命令口写 0x20 是标准 EOI。
    unsafe {
        if vector >= SLAVE_OFFSET as u64 {
            outb(PIC2_CMD, 0x20);
        }
        outb(PIC1_CMD, 0x20);
    }
}

/// 向量 → IRQ 号（0..15）。不是 IRQ 向量时返回 None。
pub fn irq_of(vector: u64) -> Option<u8> {
    let v = vector as u8;
    if (MASTER_OFFSET..MASTER_OFFSET + 8).contains(&v) {
        Some(v - MASTER_OFFSET)
    } else if (SLAVE_OFFSET..SLAVE_OFFSET + 8).contains(&v) {
        Some(v - SLAVE_OFFSET + 8)
    } else {
        None
    }
}
