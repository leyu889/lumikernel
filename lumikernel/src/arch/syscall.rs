//! `syscall` / `sysret` 的 MSR 配置与入口（M9）
//!
//! 为什么 M9 必须换成 `syscall`：musl 的静态程序在 x86_64 上**只会**
//! 用 `syscall` 指令，不会用 `int 0x80`（后者是 32 位 ABI 的历史包袱）。
//! M3 造的 `int 0x80` 通道继续保留给内核自检用，两条路共用同一套
//! `InterruptFrame` 与返回路径——多一套独立路径就多一类"只在一半路上才出错"的 bug。
//!
//! 需要配的四个 MSR：
//!
//! | MSR | 名字 | 这里填什么 |
//! |---|---|---|
//! | `0xC0000080` | EFER | bit0 `SCE=1`，开启 `syscall` |
//! | `0xC0000081` | STAR | 低 32 位=内核 CS(0x18)，高 32 位=用户基址(0x23) |
//! | `0xC0000082` | LSTAR | 入口地址（`syscall_entry`） |
//! | `0xC0000084` | FMASK | 进内核时强制清掉 IF/DF/TF/AC/NT/IOPL |
//!
//! STAR 的用户基址为什么是 `0x23` 而不是我们 GDT 里的 `0x28`：
//! `sysret` 的约定是 `CS = base + 16`、`SS = base + 8`。
//! 我们的用户数据段在 `0x28`（选择子 `0x2B`）、代码段在 `0x30`（`0x33`），
//! 所以 `base = 0x23` 正好让 `sysret` 算出 `0x33/0x2B`。
//! 我们不真的用 `sysret`（返回走 `iretq`，因为它能顺便恢复 rflags 和 rsp），
//! 但把 STAR 填对是零成本的，将来要提速时直接换就行。

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::gdt;
use crate::intr::frame::InterruptFrame;

/// `syscall` 进入时用的内核栈顶。由 `gdt::set_rsp0` 同步维护（每次任务切换）。
///
/// 用 `AtomicU64` 只是为了让 Rust 侧写入是单条 `mov`；
/// 汇编侧按普通 8 字节内存读，不涉及锁。
#[no_mangle]
pub static SC_KSTACK: AtomicU64 = AtomicU64::new(0);

/// 用户的 `%rsp` 暂存位（`syscall` 不换栈，只能自己存）。
#[no_mangle]
pub static SC_USER_RSP: AtomicU64 = AtomicU64::new(0);

const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_FMASK: u32 = 0xC000_0084;
/// `IA32_FS_BASE`：x86_64 的 TLS 基址。musl 用 `arch_prctl(ARCH_SET_FS)` 设它。
const MSR_FS_BASE: u32 = 0xC000_0100;

/// `EFER.SCE`：允许 `syscall` 指令。
const EFER_SCE: u64 = 1 << 0;

/// 进内核时要清掉的标志：TF(0x100) IF(0x200) DF(0x400) IOPL(0x3000)
/// NT(0x4000) AC(0x40000)。和 Linux 用的是同一个值。
const FMASK_VALUE: u64 = 0x4_7700;

/// STAR：`[47:32]` 内核 CS、`[63:48]` 用户基址。
const STAR_VALUE: u64 = ((gdt::KERNEL_CS as u64) << 32) | (0x23u64 << 48);

static SC_INSNS: AtomicU64 = AtomicU64::new(0);
static SC_INSNS_FROM_USER: AtomicU64 = AtomicU64::new(0);

extern "C" {
    fn syscall_entry();
}

#[inline]
fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: rdmsr 只读 MSR，无内存副作用；msr 全是本模块的常量。
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi,
             options(nomem, nostack, preserves_flags));
    }
    ((hi as u64) << 32) | lo as u64
}

#[inline]
fn wrmsr(msr: u32, v: u64) {
    // SAFETY: 写的是本架构定义的四个 MSR，值由上面的常量拼出。
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32,
             options(nomem, nostack, preserves_flags));
    }
}

pub struct Report {
    pub efer: u64,
    pub star: u64,
    pub lstar: u64,
    pub fmask: u64,
}

/// 打开 `syscall` 通道。必须在 GDT/TSS 装载之后调用（不然进内核没栈）。
pub fn init() -> Result<Report, &'static str> {
    // 1) EFER.SCE
    let efer = rdmsr(MSR_EFER);
    wrmsr(MSR_EFER, efer | EFER_SCE);

    // 2) STAR / LSTAR / FMASK
    wrmsr(MSR_STAR, STAR_VALUE);
    wrmsr(MSR_LSTAR, syscall_entry as usize as u64);
    wrmsr(MSR_FMASK, FMASK_VALUE);

    // 3) 回读校验：wrmsr 写错不会报错，只会"看起来没生效"
    let efer2 = rdmsr(MSR_EFER);
    if efer2 & EFER_SCE == 0 {
        return Err("EFER.SCE 没写进去（CPU 可能不支持 syscall）");
    }
    let star2 = rdmsr(MSR_STAR);
    if star2 != STAR_VALUE {
        return Err("STAR 回读不一致");
    }
    let lstar2 = rdmsr(MSR_LSTAR);
    if lstar2 == 0 || lstar2 != syscall_entry as usize as u64 {
        return Err("LSTAR 回读不一致");
    }
    let fmask2 = rdmsr(MSR_FMASK);
    if fmask2 & 0x200 == 0 {
        return Err("FMASK 没有清 IF（进内核会带着用户的中断开跑）");
    }

    // 初始内核栈：先用 0 号任务的静态栈，任务切换时会跟着更新
    set_kstack(crate::arch::gdt::rsp0_or_zero());

    Ok(Report {
        efer: efer2,
        star: star2,
        lstar: lstar2,
        fmask: fmask2,
    })
}

/// 更新"下一个 `syscall` 要用哪张内核栈"。
///
/// 和 `TSS.rsp0` 永远保持同一个值：`syscall` 用它、中断用它，
/// 两者不一致的话会出现"系统调用走 A 栈、中断走 B 栈"，
/// 一个任务的栈被另一个任务踩烂（而且现象极难定位）。
pub fn set_kstack(top: u64) {
    SC_KSTACK.store(top, Ordering::Release);
}

/// 写 `IA32_FS_BASE`（TLS 基址）。
///
/// 任务切换时必须跟着换：否则 A 进程的 `%fs:` 访问会读到 B 进程的 TLS，
/// 表现是"全局变量串了"——这是多进程下最难查的一类问题。
pub fn set_fs_msr(v: u64) {
    wrmsr(MSR_FS_BASE, v);
}

pub fn get_fs_msr() -> u64 {
    rdmsr(MSR_FS_BASE)
}

/// 记录一次通过 `syscall` 指令进来的调用（给自检看）。
pub fn note_insn(cpl: u64) {
    SC_INSNS.fetch_add(1, Ordering::Relaxed);
    if cpl & 3 == 3 {
        SC_INSNS_FROM_USER.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn insns() -> u64 {
    SC_INSNS.load(Ordering::Relaxed)
}

pub fn insns_from_user() -> u64 {
    SC_INSNS_FROM_USER.load(Ordering::Relaxed)
}

/// 汇编入口调这里。签名固定：`extern "C" fn(*mut InterruptFrame)`。
#[no_mangle]
pub extern "C" fn linux_syscall_entry(frame: *mut InterruptFrame) {
    if frame.is_null() {
        // 不可能发生（入口自己传的 %rsp），但绝不能在这里解引用空指针
        crate::intr::frame::halt();
    }
    // SAFETY: 指针指向 `syscall_entry` 刚在任务内核栈上拼好的 176 字节帧，
    // 布局与 `InterruptFrame` 逐字节一致（见 syscall_entry.S 顶部注释）。
    let f = unsafe { &mut *frame };
    note_insn(f.cs);
    crate::linux::dispatch(f);
}
