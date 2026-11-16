//! 中断帧布局与底层指令封装。
//!
//! **这个文件里的结构体字段顺序必须与 `stubs.S` 的压栈顺序逐字节一致。**
//! 两者不一致时不会编译报错，只会在异常打印时显示一堆看似合理的垃圾值——
//! 这是最难查的一类 bug，所以 `frame_layout_is_64_bytes_per_half` 之类的
//! 静态断言（见文件末尾测试）要一直保留。

use core::arch::asm;

/// 中断/异常发生瞬间的完整现场。
///
/// 地址从低到高（也就是 `%rsp` 指向的第一个字段是 `r15`）：
/// `r15..rax` 由 `isr_common` 压入，`vector`/`error` 由入口桩压入，
/// 其余五个由 CPU 自动压入。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InterruptFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    /// 向量号（入口桩压入）。
    pub vector: u64,
    /// 错误码（没带错误码的向量由入口桩压 0）。
    pub error: u64,
    /// 被中断的指令地址。
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    /// 被中断时的栈指针（同特权级中断时就是原 `%rsp`）。
    pub rsp: u64,
    pub ss: u64,
}

/// 现场的总字节数：15 个通用寄存器 + vector + error + 5 个 CPU 压入项。
pub const FRAME_BYTES: usize = 22 * 8;

/* ---------------- 控制寄存器 ---------------- */

#[inline]
pub fn read_cr2() -> u64 {
    let v: u64;
    // SAFETY: 读 CR2 只返回最近一次缺页的线性地址，无副作用。
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/* ---------------- 中断开关 ---------------- */

#[inline]
pub fn enable() {
    // SAFETY: sti 之后中断可能立刻到来，调用方必须保证 IDT/栈已就绪。
    unsafe { asm!("sti", options(nomem, nostack, preserves_flags)) };
}

#[inline]
pub fn disable() {
    // SAFETY: cli 只清 IF，无内存副作用。
    unsafe { asm!("cli", options(nomem, nostack, preserves_flags)) };
}

/// 中断当前是否开启（读 RFLAGS.IF）。
#[inline]
pub fn interrupts_enabled() -> bool {
    let f: u64;
    // SAFETY: pushfq 只把 RFLAGS 压栈再弹回，不改状态。
    unsafe { asm!("pushfq", "pop {}", out(reg) f, options(nomem, preserves_flags)) };
    f & (1 << 9) != 0
}

/// 挂起 CPU 直到下一个中断到来。要求 IF=1，否则永远醒不过来。
#[inline]
pub fn halt_once() {
    // SAFETY: hlt 是特权指令，ring0 可用；不会改内存。
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
}

/// 停机：关中断后无限 hlt。用于不可恢复的内核错误。
pub fn halt() -> ! {
    loop {
        // SAFETY: 同 halt_once。
        unsafe { asm!("cli; hlt", options(nomem, nostack, preserves_flags)) };
    }
}

/* ---------------- 端口的裸读写（给 PIC/PIT/串口用） ---------------- */

#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    asm!("out dx, al", in("dx") port, in("al") value,
        options(nomem, nostack, preserves_flags));
}

#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    asm!("in al, dx", in("dx") port, out("al") v,
        options(nomem, nostack, preserves_flags));
    v
}

/// 16 位端口读（ATA 的数据口是 16 位宽，一次搬一个字）。
#[inline]
pub unsafe fn inw(port: u16) -> u16 {
    let v: u16;
    asm!("in ax, dx", in("dx") port, out("ax") v,
        options(nomem, nostack, preserves_flags));
    v
}


// 编译期断言：帧大小与 `stubs.S` 的压栈数量必须一致。
// 改汇编忘了改结构体（或反过来）时，这一行会直接编译失败——
// 比真机上打出一堆错位寄存器强得多。
const _: () = assert!(core::mem::size_of::<InterruptFrame>() == FRAME_BYTES);
const _: () = assert!(core::mem::align_of::<InterruptFrame>() == 8);
