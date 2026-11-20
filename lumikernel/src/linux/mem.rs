//! 用户态内存管理：`brk` / `mmap` / `munmap` / `mprotect`
//!
//! 地址空间布局（每进程独立）：
//!
//! ```text
//! 0x0000_0000_0040_0000   ELF 装载（busybox 就装在这里）
//!          ...            brk 从 ELF 末尾往上长
//! 0x0000_0000_8000_0000   用户栈顶（16 KiB，向下）
//! 0x0000_7F00_0000_0000   匿名 mmap 区（向下长）
//! 0x0000_7FFF_FFFF_F000   USER_LIMIT —— 越过就是内核
//! ```
//!
//! 为什么 `mmap` 要往下长：向上长会和栈撞。Linux 也是这么排的
//! （栈在低处、mmap 在高处、中间留着大洞）。这里为了省事，
//! 直接把 `mmap_top` 摆在 0x7F00_0000_0000，离栈有 127 TiB，
//! 永远撞不上，也就不需要"检查撞车"的逻辑。

use kmem::PAGE_SIZE;
use klinux::errno as E;
use klinux::flags as F;
use kvm::Flags;

use crate::mm::{frames, paging};
use crate::task;

/// `mmap` 区的初始顶。
const MMAP_TOP: u64 = 0x0000_0001_0000_0000;

const PAGE: u64 = PAGE_SIZE as u64;

/// 新进程匿名映射区的起始顶（`execve` 建完地址空间后写进任务）。
pub fn initial_mmap_top() -> u64 {
    MMAP_TOP
}

fn prot_flags(prot: u32) -> Flags {
    Flags {
        present: true,
        writable: prot & F::PROT_WRITE != 0,
        user: true,
        executable: prot & F::PROT_EXEC != 0,
        cache_disable: false,
    }
}

/// `brk(2)`：把堆的末尾挪到 `addr`。
///
/// 返回**新的** brk（可能是旧的，表示失败）。Linux 的约定是
/// "失败也返回当前 brk"，不返回错误码——所以这里绝不返回负数。
pub fn brk(addr: u64) -> i64 {
    let cur = task::brk();
    if addr == 0 {
        return cur as i64;
    }
    if cur == 0 {
        // 还没有初始化过 brk（理论上不该发生：exec 时设过）
        return 0;
    }
    if addr <= cur {
        // 收缩：不动页表，只把指针挪回去（Linux 允许，页留着下次复用）
        task::set_brk(addr.max(task::brk_start()));
        return addr.max(task::brk_start()) as i64;
    }
    if addr > crate::exec::USER_LIMIT || addr >= task::mmap_top().min(MMAP_TOP) {
        return cur as i64;
    }

    // 往上长：把 [align_up(cur), align_up(addr)) 之间缺的页都映射出来
    let from = (cur + PAGE - 1) & !(PAGE - 1);
    let to = (addr + PAGE - 1) & !(PAGE - 1);
    let mut va = from;
    while va < to {
        let phys = match frames::alloc_frame() {
            Some(p) => p,
            None => {
                // 内存不够：把这一次已经建好的页退回去，保持 brk 不变
                // （不留"半个堆"，否则用户态会以为自己有那么多内存）
                let mut back = from;
                while back < va {
                    if let Ok(p) = paging::unmap_in_current(back) {
                        frames::put_page(p);
                    }
                    back += PAGE;
                }
                return cur as i64;
            }
        };
        // SAFETY: 刚分配的独占帧，经直接映射区清零。
        unsafe { core::ptr::write_bytes(paging::phys_to_virt(phys) as *mut u8, 0, PAGE as usize) };
        if paging::map_in_current(va, phys, Flags::USER_RW).is_err() {
            frames::put_page(phys);
            return cur as i64;
        }
        va += PAGE;
    }
    task::set_brk(addr);
    addr as i64
}

/// `mmap(2)`。只支持匿名映射（`MAP_ANONYMOUS`）。
///
/// 文件映射（`mmap` 一个 fd）没有做：busybox 的静态版本不会用它，
/// 真遇到了明确返回 `ENODEV`，而不是"假装成功然后读到全 0"。
pub fn mmap(addr: u64, len: usize, prot: u32, flags: u32, fd: i64, off: u64) -> i64 {
    if len == 0 {
        return E::err(E::EINVAL);
    }
    if flags & F::MAP_ANONYMOUS == 0 {
        return E::err(E::ENODEV);
    }
    let _ = (fd, off);
    let bytes = match (len as u64).checked_add(PAGE - 1) {
        Some(v) => v & !(PAGE - 1),
        None => return E::err(E::EINVAL),
    };

    let base = if flags & F::MAP_FIXED != 0 {
        if addr & (PAGE - 1) != 0 || addr == 0 || addr + bytes > crate::exec::USER_LIMIT {
            return E::err(E::EINVAL);
        }
        // `MAP_FIXED` 的语义是**覆盖**：区间里原有的映射要先拆掉。
        // 不拆的话 `map_in_current` 会撞上已有 PTE 失败，返回 ENOMEM——
        // musl 的 malloc 于是拿到 NULL，接着在 `printf` 里空指针崩溃。
        // （这条是 trace 里看出来的：mmap(0x714000,0x1000) -> -12）
        let mut va = addr;
        while va < addr + bytes {
            if let Ok(p) = paging::unmap_in_current(va) {
                frames::put_page(p);
            }
            va += PAGE;
        }
        addr
    } else {
        let top = task::mmap_top();
        let top = if top == 0 { MMAP_TOP } else { top };
        let want = match top.checked_sub(bytes) {
            Some(v) => v & !(PAGE - 1),
            None => return E::err(E::ENOMEM),
        };
        // 和栈/brk 保持安全距离
        if want <= crate::exec::USER_STACK_TOP + 0x1000_0000 {
            return E::err(E::ENOMEM);
        }
        task::set_mmap_top(want);
        want
    };

    // ---- 懒分配（M12）----
    //
    // 只登记一段 VMA，一页物理内存都不给。真正的分配发生在用户第一次
    // 碰这段内存时的 #PF 里（`mm::fault::try_lazy`）。
    //
    // 为什么值得改：`mmap` 立刻分配的话，`mmap` + 只写头几十字节的模式
    // （几乎每个分配器都这么干）会白白零化整段。M11 量出来一次
    // 64 KiB 的 mmap+munmap 要 174 万周期，而 Linux 是 8426——差 200 倍
    // 全是"提前把没用的页铺好"的代价。
    //
    // 表满或者地址上已经有映射时退回立即分配（老路径），保证不会因为
    // 新机制而让原本能跑的程序变失败。
    let can_lazy = flags & F::MAP_FIXED == 0
        || paging::translate_in(paging::read_cr3(), base).is_none();
    if can_lazy {
        task::vma_remove(base, base + bytes);
        if task::vma_add(base, base + bytes, prot) {
            return base as i64;
        }
        kserialln!("[mmap] VMA 表满（{} 段），这一段退回立即分配", task::VMA_MAX);
    }

    let f = prot_flags(prot);
    let mut va = base;
    while va < base + bytes {
        // 防御性检查：这一页不该已经有映射。真撞上说明地址分配逻辑有 bug
        // （两个映射落在同一页 = 用户态看到"两块内存互相覆盖"，
        // 表现是 malloc 元数据莫名其妙被踩）。这里宁可报错也不覆盖。
        if paging::translate_in(paging::read_cr3(), va).is_some() {
            kserialln!("[mmap] 地址冲突：{:#x} 已有映射，拒绝", va);
            return E::err(E::EEXIST);
        }
        let phys = match frames::alloc_frame() {
            Some(p) => p,
            None => {
                let mut back = base;
                while back < va {
                    if let Ok(p) = paging::unmap_in_current(back) {
                        frames::put_page(p);
                    }
                    back += PAGE;
                }
                return E::err(E::ENOMEM);
            }
        };
        // SAFETY: 刚分配的独占帧。
        unsafe { core::ptr::write_bytes(paging::phys_to_virt(phys) as *mut u8, 0, PAGE as usize) };
        // 这里曾经每映射一页就跑一次 `frame_mapped_in()`（遍历整个地址空间，
        // O(映射数)）。它是 M9 用来抓"一帧两用"的调试检查，但留在热路径上
        // 让 mmap+munmap 一次 16 页要 174 万周期（M11 的 bench 量出来的）。
        // 检查本身保留在 `paging::frame_mapped_in`，由自检按需调用。
        if paging::map_in_current(va, phys, f).is_err() {
            frames::put_page(phys);
            return E::err(E::ENOMEM);
        }
        va += PAGE;
    }
    base as i64
}

/// `munmap(2)`：拆映射并把帧还回去。
pub fn munmap(addr: u64, len: usize) -> i64 {
    if addr & (PAGE - 1) != 0 {
        return E::err(E::EINVAL);
    }
    // 范围校验：用户只能拆用户区的映射。少了这一条，一个 rax 里的垃圾值
    // 就能让内核去拆自己的页表（`kpt::unmap` 会回收空的上级表）。
    let end = match addr.checked_add(len as u64) {
        Some(v) => v,
        None => return E::err(E::EINVAL),
    };
    if addr == 0 || end > crate::exec::USER_LIMIT {
        return E::err(E::EINVAL);
    }
    let bytes = match (len as u64).checked_add(PAGE - 1) {
        Some(v) => v & !(PAGE - 1),
        None => return E::err(E::EINVAL),
    };
    task::vma_remove(addr, addr + bytes);
    let mut va = addr;
    while va < addr + bytes {
        // 没映射的页直接跳过（POSIX 允许 munmap 一段里夹杂空洞）。
        // 懒分配之后"没映射"是常态，不是异常。
        if let Ok(phys) = paging::unmap_in_current(va) {
            // 引用计数：fork 出来的共享页可能还有别的进程在用。
            frames::put_page(phys);
        }
        va += PAGE;
    }
    // 全部拆光时把 mmap_top 抬回去，避免反复 mmap/munmap 让地址空间单调下降
    let top = task::mmap_top();
    if top != 0 && addr == top {
        task::set_mmap_top(addr + bytes);
    }
    0
}

/// `mremap(2)`。
///
/// musl 的 `realloc` 对"整页 mmap 出来的大块"会先试 `mremap`；不支持时它会
/// 退回 `malloc + memcpy + free`（正确但慢）。既然实现起来只有几十行，
/// 就给它一条直路：新开一段、把旧内容拷过去、拆掉旧的。
///
/// 暂不支持原地扩容与 `MREMAP_FIXED`——返回 `EINVAL` 让调用方走回退路径。
pub fn mremap(old_addr: u64, old_len: usize, new_len: usize, flags: u32) -> i64 {
    const MREMAP_MAYMOVE: u32 = 1;
    const MREMAP_FIXED: u32 = 2;
    if flags & MREMAP_FIXED != 0 || flags & MREMAP_MAYMOVE == 0 {
        return E::err(E::EINVAL);
    }
    let old_bytes = ((old_len as u64) + PAGE - 1) & !(PAGE - 1);
    let new_bytes = ((new_len as u64) + PAGE - 1) & !(PAGE - 1);
    if new_bytes == 0 {
        return E::err(E::EINVAL);
    }
    if new_bytes <= old_bytes {
        // 缩小：原地返回就够（多余的页留着，不影响正确性）
        return old_addr as i64;
    }
    let new = mmap(
        0,
        new_len,
        F::PROT_READ | F::PROT_WRITE,
        F::MAP_PRIVATE | F::MAP_ANONYMOUS,
        -1,
        0,
    );
    if new < 0 {
        return new;
    }
    let dst = new as u64;
    // SAFETY: 两个区间都在当前进程的地址空间里且已映射；系统调用期间
    // CR3 就是这个进程的表，所以内核可以直接按用户地址读写。
    unsafe {
        core::ptr::copy_nonoverlapping(
            old_addr as *const u8,
            dst as *mut u8,
            old_bytes as usize,
        );
    }
    let _ = munmap(old_addr, old_len);
    new
}

/// `mprotect(2)`：改已有页的权限位。
pub fn mprotect(addr: u64, len: usize, prot: u32) -> i64 {
    if addr & (PAGE - 1) != 0 {
        return E::err(E::EINVAL);
    }
    let bytes = match (len as u64).checked_add(PAGE - 1) {
        Some(v) => v & !(PAGE - 1),
        None => return E::err(E::EINVAL),
    };
    let f = prot_flags(prot);
    let mut va = addr;
    while va < addr + bytes {
        match paging::translate_in(paging::read_cr3(), va) {
            Some(phys) => {
                if paging::map_in_current(va, phys, f).is_err() {
                    return E::err(E::ENOMEM);
                }
            }
            None => return E::err(E::ENOMEM),
        }
        va += PAGE;
    }
    0
}
