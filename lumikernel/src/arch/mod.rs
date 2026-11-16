//! 架构相关：GDT/TSS、上下文切换、静态内核栈。
//!
//! `arch.S` 里是必须用汇编写的那几段（远返回改 CS、`switch_to`、双故障栈），
//! `gdt.rs` 负责构造描述符并按规范装载。

use core::arch::global_asm;

pub mod gdt;
pub mod smp;
pub mod syscall;
pub mod sync;

global_asm!(include_str!("arch.S"), options(att_syntax));
global_asm!(include_str!("ap_tramp.S"), options(att_syntax));
// M9：`syscall` 指令入口。必须和 stubs.S 的帧布局严丝合缝，所以单独一个文件，
// 出问题时只要盯这两个文件。
global_asm!(include_str!("syscall_entry.S"), options(att_syntax));

extern "C" {
    /// `switch_to(prev_rsp: *mut u64, next_rsp: u64)`
    pub fn switch_to(prev: *mut u64, next: u64);
}
