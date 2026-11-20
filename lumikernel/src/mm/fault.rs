//! 用户态页错误的**可恢复**两类：写时复制（COW）与懒分配。
//!
//! 这两件事共用一个入口，因为它们在硬件眼里长得一模一样——
//! 都是 `#PF`，区别只在错误码：
//!
//! | 场景 | P (bit0) | W (bit1) | 判据 | 动作 |
//! |---|---|---|---|---|
//! | COW | 1（页在） | 1（写） | 页被标 COW 且引用数 > 1 | 复制一份，改指向 |
//! | 懒分配 | 0（页不在） | 任意 | 地址落在某个 VMA 里 | 分配 + 清零 + 建映射 |
//!
//! 两条都失败时返回 `false`，交给上层按"用户程序自己的 bug"处理（杀进程）。
//! **绝不在状态不明时假装恢复成功**——那会把一次干净的崩溃变成一堆脏数据。
//!
//! ## 为什么判据必须严格
//!
//! 一次错误的"恢复"比一次崩溃更糟：崩溃留下现场，误恢复留下的是
//! "程序跑着跑着结果不对"。所以两处判据都收得很紧：
//!
//! - COW 要求 **PTE 里带着 COW 标记**（不是"看起来像只读"），
//!   所以写代码段仍然会触发合法的 `#PF` → 杀进程，语义与 Linux 一致；
//! - 懒分配要求 **该地址被登记在 VMA 表里**，野指针不在表里，照样杀进程。

use core::sync::atomic::{AtomicU64, Ordering};

use kpt::PageTable;
use kvm::Flags;

use super::{cow, frames, paging};

/// 一页的字节数。
const PAGE: u64 = 4096;

/// COW 触发的复制次数。
static COW_FAULTS: AtomicU64 = AtomicU64::new(0);
/// 懒分配次数。
static LAZY_FAULTS: AtomicU64 = AtomicU64::new(0);
/// 走到这里但判定为"真的不该恢复"的次数（正常路径下就是用户程序崩溃的次数）。
static REFUSED: AtomicU64 = AtomicU64::new(0);
/// 分配失败导致没能恢复的次数。
static NOMEM: AtomicU64 = AtomicU64::new(0);
/// 诊断打印计数（限流，避免刷屏）。
static DIAG: AtomicU64 = AtomicU64::new(0);

/// `(cow, lazy, refused, nomem)`
pub fn stats() -> (u64, u64, u64, u64) {
    (
        COW_FAULTS.load(Ordering::Relaxed),
        LAZY_FAULTS.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
        NOMEM.load(Ordering::Relaxed),
    )
}

/// 把 `PROT_*`（Linux 同值）翻译成页表权限位。
fn prot_to_flags(prot: u32) -> Flags {
    Flags {
        present: true,
        writable: prot & 2 != 0,
        user: true,
        executable: prot & 4 != 0,
        cache_disable: false,
    }
}

/// 这一页"读一次会成功"吗？（给 `uaccess` 的指针校验用）
///
/// 三种情况返回 `true`：
/// ① 已映射（读映射页总是可以的，NX 只挡取指不挡读）；
/// ② 没映射但落在带 `PROT_READ` 的 VMA 里 → 懒分配页，读了就分配；
/// ③ 其余一律 `false`。
pub fn can_read(va: u64) -> bool {
    let cr3 = paging::read_cr3();
    if let Some(pte) = paging::pte_of(cr3, va & !(PAGE - 1)) {
        // **必须检查 present 位**：`munmap` 把 PTE 清成"表项还在、P=0"。
        // 不查的话，"mmap → munmap → 再 mmap 同一地址"的程序
        // （malloc 反复复用地址就是这种模式）会在第二次 mmap 后
        // 被误判成坏指针 —— `cat` 第二轮 read 的 EFAULT 就是它。
        if pte & 1 != 0 {
            return true;
        }
    }
    match crate::task::vma_lookup(va) {
        Some(prot) => prot & 1 != 0,
        None => false,
    }
}

/// 这一页"写一次会成功"吗？
///
/// 两种可写来源：
/// ① PTE 本身带 W 位；
/// ② **COW 页**——写会触发复制，复制完就是可写的；
/// ③ 懒分配页带 `PROT_WRITE`——写会触发分配。
pub fn can_write(va: u64) -> bool {
    let cr3 = paging::read_cr3();
    if let Some(pte) = paging::pte_of(cr3, va & !(PAGE - 1)) {
        if pte & 1 != 0 {
            if pte & 2 != 0 {
                return true;
            }
            // 已映射但只读：COW 页可以写（触发复制），真只读（代码段）不行
            return cow::is_cow(pte & kpt::ADDR_MASK);
        }
        // P=0：munmap 的残骸，等同"没映射"，往下走 VMA 判断
    }
    match crate::task::vma_lookup(va) {
        Some(prot) => prot & 2 != 0,
        None => false,
    }
}

/// 处理一次用户页错误。返回 `true` 表示已经修好，可以 `iretq` 重试那条指令。
///
/// 调用方必须保证：这条错误来自 ring3，或者来自内核访问**用户页**。
pub fn handle(vector_ok: bool, error: u64) -> bool {
    if !vector_ok {
        return false;
    }
    let cr2 = crate::intr::frame::read_cr2();

    // 判据是**地址属于哪个区**，不是"这次访问来自哪个特权级"。
    //
    // 之前按 `error.U` 判，后果是：内核代表用户访问用户内存时
    // （`fd::read` 直接往用户缓冲 memcpy），#PF 的 U 位是 0——
    // `cat` 读一个还没分配的懒页就直接把内核带崩了。
    //
    // 高半区的缺页（直接映射 / 内核映像 / vmalloc）在这里修不了，
    // 也不该修——那说明页表被踩坏，必须留下完整现场。
    if cr2 >= 0x0000_8000_0000_0000 {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    // RSVD 位 = 页表项里有保留位被置起 = 页表本身被写坏。
    // 这不是"缺一页"能解决的问题。
    if error & 8 != 0 {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    let present = error & 1 != 0;
    let write = error & 2 != 0;

    if present && write {
        if try_cow(cr2) {
            COW_FAULTS.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    if !present {
        if try_lazy(cr2) {
            LAZY_FAULTS.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    // 页在、但读就出错（比如执行了 NX 页，或权限不足读）：
    // 这些都不是"能靠分配解决的"缺页，交给上层。
    REFUSED.fetch_add(1, Ordering::Relaxed);
    false
}

/// 写时复制。
fn try_cow(cr2: u64) -> bool {
    let pml4 = paging::read_cr3();
    let va = cr2 & !(PAGE - 1);

    let pte = match paging::pte_of(pml4, va) {
        Some(e) if e & 1 != 0 => e,
        _ => return false,
    };
    let old = pte & kpt::ADDR_MASK;

    // 判据：必须是 COW 页。单看"只读"会把代码段也算进来。
    if !cow::is_cow(old) {
        if DIAG.load(Ordering::Relaxed) < 6 {
            DIAG.fetch_add(1, Ordering::Relaxed);
            kserialln!(
                "  [cow] 拒绝 cr2={:#x} pte={:#018x} phys={:#x} is_cow=0 refs={}",
                cr2, pte, old, cow::refcount(old)
            );
        }
        return false;
    }

    let refs = cow::refcount(old);

    // `refs` 是总引用数：1 = 只剩自己（独占），≥2 = 还有人在用。
    // 判断必须是 `<= 1`：写成 `== 0` 的话，fork 后父子各持一份引用，
    // 一方退出后另一方（refs=1）会被误判成"还有人共享"，白白复制一次。
    if refs <= 1 {
        // 只剩自己一个引用了：不需要复制，直接把权限改回可写。
        // 这条分支很关键——fork 之后父子都只读，若两边都不写，
        // 一旦一方退出，另一方就能省掉一次 4 KiB 复制。
        cow::clear_cow(old);
        if !paging::set_writable_in(pml4, va, true) {
            return false;
        }
        return true;
    }

    // 真的共享：复制一份。
    let newp = match frames::alloc_frame() {
        Some(p) => p,
        None => {
            NOMEM.fetch_add(1, Ordering::Relaxed);
            return false;
        }
    };
    // SAFETY: `old` 是当前地址空间里已映射的可读页，`newp` 是刚分配独占的页；
    // 两者都能经直接映射区访问。`writable` 为真时我们会把 PTE 改成可写，
    // 但复制这一刻读的仍是旧内容，不存在写冲突。
    unsafe {
        core::ptr::copy_nonoverlapping(
            paging::phys_to_virt(old) as *const u8,
            paging::phys_to_virt(newp) as *mut u8,
            PAGE as usize,
        );
    }

    let fl = Flags {
        present: true,
        writable: true,
        user: pte & 4 != 0,
        executable: pte & (1 << 63) == 0,
        cache_disable: pte & (1 << 4) != 0,
    };
    if paging::remap_in(pml4, va, newp, fl).is_err() {
        frames::free_frame(newp);
        NOMEM.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    // 本进程放弃对旧页的引用（旧页仍归其它进程）。
    cow::decref(old);
    // 新页是**独占**的：计数保持 0，并清掉可能残留的标记。
    // 之前这里写的是 `incref(newp)`（想借它的 0→1 分支清标记），
    // 结果把新页标成了"还有一个人在引用"，于是下一次写它又要白复制一次。
    cow::clear_all(newp);
    paging::flush_tlb(va);
    true
}

/// 懒分配：给落在 VMA 里的匿名地址按需铺一页。
fn try_lazy(cr2: u64) -> bool {
    let prot = match crate::task::vma_lookup(cr2) {
        Some(p) => p,
        None => return false,
    };
    let pml4 = paging::read_cr3();
    let va = cr2 & !(PAGE - 1);

    // 已经有映射（多核下可能出现竞争，或上一次失败留下的半成品）：视作成功。
    if paging::translate_in(pml4, va).is_some() {
        return true;
    }

    let phys = match frames::alloc_frame() {
        Some(p) => p,
        None => {
            NOMEM.fetch_add(1, Ordering::Relaxed);
            return false;
        }
    };
    // 匿名映射必须清零：不复零等于把上一个进程的内存泄露给新进程，
    // 这是最经典的信息泄露漏洞之一。
    // SAFETY: `phys` 是刚分配独占的帧，经直接映射区可写。
    unsafe {
        core::ptr::write_bytes(paging::phys_to_virt(phys) as *mut u8, 0, PAGE as usize);
    }

    // SAFETY: `pml4` 是当前 CR3 指向的根，`from_root` 只借用不拥有，
    // 不会在 drop 时释放它（`PageTable` 没有 Drop 实现）。
    let mut pt = unsafe { PageTable::from_root(pml4) };
    if paging::map_in(&mut pt, va, phys, prot_to_flags(prot)).is_err() {
        frames::free_frame(phys);
        NOMEM.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    // 新映射：TLB 里不可能有这一项，不需要刷。
    true
}

/// 页错误处理的摘要行（给自检和日志用）。
pub fn summary() -> (u64, u64, u64, u64) {
    stats()
}
