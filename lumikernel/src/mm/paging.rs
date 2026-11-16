//! 页表与地址空间（M1）
//!
//! `kpt` 负责 PTE 的读写细节，这里负责把内核的物理内存/帧分配器接进去，
//! 并做一次**真机验证**：新建页表、搬内核高半区、切 CR3、写读、切回。
//!
//! 为什么这步必须真机做：页表写错在宿主机单测里查不出来（模型测的是逻辑，
//! 不是 CPU 的翻译行为）。切 CR3 那几行如果错了，唯一的现象就是三重故障重启。

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use kmem::PAGE_SIZE;
use kpt::{Flags, FrameSource, PageTable, PhysMem};

use super::{frames, DIRECT_MAP_BASE, KERNEL_VA_BASE, VMALLOC_BASE};

/// 内核视角的物理内存访问：全部走直接映射区。
pub struct KernelMem;

impl PhysMem for KernelMem {
    fn at(&self, phys: u64) -> *mut u8 {
        (DIRECT_MAP_BASE + phys) as *mut u8
    }
}

/// 把 `frames` 适配成 `kpt` 需要的帧来源。
pub struct KernelFrames;

impl FrameSource for KernelFrames {
    fn alloc_frame(&mut self) -> Option<u64> {
        frames::alloc_frame()
    }
    fn free_frame(&mut self, phys: u64) {
        frames::free_frame(phys)
    }
}

/// 内核自己的 PML4（进程切换时要切回它）。
///
/// 不能在需要时现读 CR3：跑在某个进程的地址空间里时，CR3 指向的是**那个进程**的表，
/// 拿它当"内核模板"去克隆，会把上一个用户的低半区也带进新进程——隔离就没了。
static KERNEL_CR3: AtomicU64 = AtomicU64::new(0);

/// 启动时记下内核 PML4（在任何进程创建之前调用一次）。
pub fn remember_kernel_cr3() {
    KERNEL_CR3.store(read_cr3(), Ordering::Release);
}

pub fn kernel_cr3() -> u64 {
    KERNEL_CR3.load(Ordering::Acquire)
}

/// 新建一张**用户地址空间**：低半区（PML4[0..255]）全空，高半区共享内核。
///
/// 这就是 M6 的核心：每个进程拿一张自己的表，于是同一个虚拟地址
/// （比如 ELF 里的 0x600000）在不同进程里指向不同的物理页。
/// 内核部分靠共享 PML4 项实现——这也是宏内核的常规做法：
/// 系统调用时不需要切页表，因为内核映射在每个进程里都在。
pub fn new_user_table() -> Option<PageTable> {
    let mut pt = PageTable::create(&KernelMem, &mut KernelFrames)?;
    let k = kernel_cr3();
    if k == 0 {
        return None;
    }
    // 256..512 是内核高半区（直接映射 0xFFFF8000...、内核 0xFFFFFFFF8000...、vmalloc）
    pt.copy_entries_from(&KernelMem, k, 256, 512);
    Some(pt)
}

/// 在**指定**页表里建立映射（进程地址空间用；不需要切 CR3）。
pub fn map_in(
    pt: &mut PageTable,
    vaddr: u64,
    paddr: u64,
    flags: Flags,
) -> Result<(), kpt::PtError> {
    pt.map(&KernelMem, &mut KernelFrames, vaddr, paddr, flags)
}

#[inline]
pub fn phys_to_virt(phys: u64) -> u64 {
    DIRECT_MAP_BASE + phys
}

/// 直接映射区的逆变换。M1 的自检只走正向，这里先留接口。
#[allow(dead_code)]
#[inline]
pub fn virt_to_phys(virt: u64) -> u64 {
    virt - DIRECT_MAP_BASE
}

pub fn read_cr3() -> u64 {
    let v: u64;
    // SAFETY: 读控制寄存器无副作用。
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

pub fn write_cr3(v: u64) {
    // SAFETY: 调用方保证 v 是有效 PML4 物理地址，且新表映射了当前代码与栈。
    unsafe { asm!("mov cr3, {}", in(reg) v, options(nostack)) };
}

pub fn read_rip() -> u64 {
    let v: u64;
    // SAFETY: 读指令指针无副作用。
    unsafe { asm!("lea {}, [rip]", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// 让 TLB 丢掉某一页的缓存翻译。
///
/// 改页表之后必须刷新，否则 CPU 可能继续用旧的翻译——这是"改了页表却
/// 没生效"的唯一原因（另一个原因是改错了表）。
#[inline]
pub fn flush_tlb(vaddr: u64) {
    // SAFETY: invlpg 只影响 TLB，不访问内存。
    unsafe { asm!("invlpg [{}]", in(reg) vaddr, options(nostack, preserves_flags)) };
}

/// 在**当前**地址空间里建立一页映射（vmalloc 区用），并刷新 TLB。
pub fn map_in_current(vaddr: u64, paddr: u64, flags: Flags) -> Result<(), kpt::PtError> {
    // SAFETY: 当前 CR3 一定是有效 PML4；我们是内核，有权改自己的地址空间。
    let mut pt = unsafe { PageTable::from_root(read_cr3()) };
    pt.map(&KernelMem, &mut KernelFrames, vaddr, paddr, flags)?;
    flush_tlb(vaddr);
    Ok(())
}

/// 拆除当前地址空间里的一页映射，返回原物理地址。
pub fn unmap_in_current(vaddr: u64) -> Result<u64, kpt::PtError> {
    // **硬护栏**：高半区属于内核（直接映射 / 内核映像 / vmalloc）。
    // `kpt::unmap` 会在表变空时回收上一级页表页——若放行一个高半区地址，
    // 它会把**内核与所有进程共享的 PDPT/PD 页**还给帧分配器，
    // 于是整机所有地址空间的直接映射同时崩掉（M11 实测踩到）。
    // 只禁"直接映射区"和"内核映像"这两段：它们的页表页与全系统共享。
    // vmalloc 区（`VMALLOC_BASE`）不放行——那是内核自己按需建的，拆它是合法的
    // （M2 的 guard page 自检就在那儿拆）。
    const DM_BASE: u64 = 0xFFFF_8000_0000_0000;
    if (vaddr >= DM_BASE && vaddr < crate::mm::VMALLOC_BASE)
        || vaddr >= crate::mm::KERNEL_VA_BASE
    {
        return Err(kpt::PtError::NotMapped);
    }
    // SAFETY: 同 map_in_current。
    let mut pt = unsafe { PageTable::from_root(read_cr3()) };
    let p = pt.unmap(&KernelMem, &mut KernelFrames, vaddr)?;
    flush_tlb(vaddr);
    Ok(p)
}

pub fn read_cr0() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

pub fn read_cr4() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// 读一个页表项（在直接映射区里读那个物理页的第 `idx` 个 8 字节）。
#[inline]
fn pte_at(table_phys: u64, idx: usize) -> u64 {
    // SAFETY: 调用方只传本进程页表里已验证 present 的表页物理地址；
    // 直接映射区把每个物理页都映射在案，读一个 8 字节不会越界。
    unsafe { core::ptr::read_volatile((phys_to_virt(table_phys) as *const u64).add(idx)) }
}

/// 销毁一个用户地址空间：低半区的所有用户页 + 所有中间页表页 + PML4 本身。
///
/// 返回 `(释放的数据页数, 释放的页表页数)`。
///
/// 为什么要自己走一遍：帧分配器只认"物理帧号"，它不知道哪些帧属于哪个进程。
/// 进程退出时如果只释放任务槽位、不释放这些帧，每跑一个进程就漏掉
/// 1 个 PML4 + 若干中间表 + 若干数据页——几百个进程就能把 512 MiB 吃干净。
/// 这类泄漏**不会报错、不会崩**，只会让系统越来越慢最后分配失败。
///
/// 只走低半区（PML4[0..256]）：高半区是内核自己的映射，属于所有进程共享，
/// 释放它等于把内核拆了。
pub fn destroy_user_table(pml4_phys: u64) -> (usize, usize) {
    if pml4_phys == 0 || pml4_phys == kernel_cr3() {
        // 内核自己的表、或者"用内核表"的任务（0 号）：不能拆
        return (0, 0);
    }
    let mut pages = 0usize;
    let mut tables = 0usize;
    for i4 in 0..256 {
        let e4 = pte_at(pml4_phys, i4);
        if e4 & 1 == 0 {
            continue;
        }
        let pdpt = e4 & kpt::ADDR_MASK;
        for i3 in 0..512 {
            let e3 = pte_at(pdpt, i3);
            if e3 & 1 == 0 {
                continue;
            }
            if e3 & 0x80 != 0 {
                // 1 GiB 大页：用户区不该出现（我们只按 4 KiB 建映射）。
                // 真出现了也不能只退首页——那会漏掉 262143 页，宁可留着让人发现。
                continue;
            }
            let pd = e3 & kpt::ADDR_MASK;
            for i2 in 0..512 {
                let e2 = pte_at(pd, i2);
                if e2 & 1 == 0 {
                    continue;
                }
                if e2 & 0x80 != 0 {
                    // 2 MiB 大页：同上，只退首页会漏 511 页，明确跳过。
                    continue;
                }
                let pt = e2 & kpt::ADDR_MASK;
                for i1 in 0..512 {
                    let e1 = pte_at(pt, i1);
                    if e1 & 1 == 0 {
                        continue;
                    }
                    frames::free_frame(e1 & kpt::ADDR_MASK);
                    pages += 1;
                }
                frames::free_frame(pt);
                tables += 1;
            }
            frames::free_frame(pd);
            tables += 1;
        }
        frames::free_frame(pdpt);
        tables += 1;
    }
    frames::free_frame(pml4_phys);
    tables += 1;
    (pages, tables)
}

/// 从页表项还原权限位。
///
/// `fork` 复制页时要保持原来的权限：把可执行页复制成不可执行，
/// 子进程一进代码段就 `#PF`（而且看起来像"fork 坏了"）。
fn flags_from_pte(e: u64) -> Flags {
    Flags {
        present: true,
        writable: e & 2 != 0,
        user: e & 4 != 0,
        executable: e & (1 << 63) == 0,
        cache_disable: e & (1 << 4) != 0,
    }
}

/// 复制一个用户地址空间（`fork` 用）。
///
/// **不是** COW（写时复制）：每一页都真复制一份。理由很实际——
/// COW 需要"缺页时判断是复制还是真错"的整套逻辑，而 M9 要的是
/// "先跑通别人的程序"。复制 1.1 MiB 的 busybox 大约 280 页，代价可接受。
/// 这条限制写在 README 里，不能假装它是 COW。
///
/// 失败时把已经建好的部分全部拆掉再返回 `None`，不留半张表。
pub fn clone_user_table(src_phys: u64) -> Option<(PageTable, usize)> {
    if src_phys == 0 {
        return None;
    }
    let mut dst = new_user_table()?;
    let mut pages = 0usize;

    for i4 in 0..256u64 {
        let e4 = pte_at(src_phys, i4 as usize);
        if e4 & 1 == 0 {
            continue;
        }
        let pdpt = e4 & kpt::ADDR_MASK;
        for i3 in 0..512u64 {
            let e3 = pte_at(pdpt, i3 as usize);
            if e3 & 1 == 0 || e3 & 0x80 != 0 {
                continue;
            }
            let pd = e3 & kpt::ADDR_MASK;
            for i2 in 0..512u64 {
                let e2 = pte_at(pd, i2 as usize);
                if e2 & 1 == 0 || e2 & 0x80 != 0 {
                    continue;
                }
                let pt = e2 & kpt::ADDR_MASK;
                for i1 in 0..512u64 {
                    let e1 = pte_at(pt, i1 as usize);
                    if e1 & 1 == 0 {
                        continue;
                    }
                    let va = (i4 << 39) | (i3 << 30) | (i2 << 21) | (i1 << 12);
                    let src_page = e1 & kpt::ADDR_MASK;
                    let newp = match frames::alloc_frame() {
                        Some(p) => p,
                        None => {
                            destroy_user_table(dst.root_phys());
                            return None;
                        }
                    };
                    // SAFETY: 两个物理帧都独占：src_page 属于调用者的地址空间，
                    // newp 是刚分配的；直接映射区能同时访问两者。
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            phys_to_virt(src_page) as *const u8,
                            phys_to_virt(newp) as *mut u8,
                            PAGE_SIZE,
                        );
                    }
                    if map_in(&mut dst, va, newp, flags_from_pte(e1)).is_err() {
                        frames::free_frame(newp);
                        destroy_user_table(dst.root_phys());
                        return None;
                    }
                    pages += 1;
                }
            }
        }
    }
    Some((dst, pages))
}

/// 在指定页表里把 `va` 翻译成物理地址（不切 CR3）。
pub fn translate_in(pml4: u64, va: u64) -> Option<u64> {
    if pml4 == 0 {
        return None;
    }
    let i4 = ((va >> 39) & 0x1FF) as usize;
    let e4 = pte_at(pml4, i4);
    if e4 & 1 == 0 {
        return None;
    }
    let i3 = ((va >> 30) & 0x1FF) as usize;
    let e3 = pte_at(e4 & kpt::ADDR_MASK, i3);
    if e3 & 1 == 0 {
        return None;
    }
    if e3 & 0x80 != 0 {
        return Some((e3 & kpt::ADDR_MASK) + (va & 0x3FFF_FFFF));
    }
    let i2 = ((va >> 21) & 0x1FF) as usize;
    let e2 = pte_at(e3 & kpt::ADDR_MASK, i2);
    if e2 & 1 == 0 {
        return None;
    }
    if e2 & 0x80 != 0 {
        return Some((e2 & kpt::ADDR_MASK) + (va & 0x1F_FFFF));
    }
    let i1 = ((va >> 12) & 0x1FF) as usize;
    let e1 = pte_at(e2 & kpt::ADDR_MASK, i1);
    if e1 & 1 == 0 {
        return None;
    }
    Some((e1 & kpt::ADDR_MASK) + (va & 0xFFF))
}

/// 往**指定**地址空间里写一段数据（不切 CR3）。用于给新进程铺初始栈。
///
/// 逐页翻译：目标页必须已经映射；跨页时按页边界切分。
pub fn write_in_table(pml4: u64, va: u64, src: &[u8]) -> Result<(), ()> {
    let mut done = 0usize;
    while done < src.len() {
        let cur = va.checked_add(done as u64).ok_or(())?;
        let phys = translate_in(pml4, cur).ok_or(())?;
        let page_left = 0x1000 - (cur & 0xFFF) as usize;
        let n = core::cmp::min(page_left, src.len() - done);
        // SAFETY: phys 是当前地址空间里这一页的物理地址，n 不越过页边界。
        unsafe {
            core::ptr::copy_nonoverlapping(
                src.as_ptr().add(done),
                phys_to_virt(phys) as *mut u8,
                n,
            );
        }
        done += n;
    }
    Ok(())
}

/// 从**指定**地址空间读一段数据（不切 CR3）。诊断用。
pub fn read_in_table(pml4: u64, va: u64, out: &mut [u8]) -> Result<(), ()> {
    let mut done = 0usize;
    while done < out.len() {
        let cur = va.checked_add(done as u64).ok_or(())?;
        let phys = translate_in(pml4, cur).ok_or(())?;
        let page_left = 0x1000 - (cur & 0xFFF) as usize;
        let n = core::cmp::min(page_left, out.len() - done);
        // SAFETY: phys 是这一页的物理地址，n 不越页。
        unsafe {
            core::ptr::copy_nonoverlapping(
                phys_to_virt(phys) as *const u8,
                out.as_mut_ptr().add(done),
                n,
            );
        }
        done += n;
    }
    Ok(())
}

/// 这一页在 `pml4` 里是否**存在且可写**？
///
/// 这是 `copy_to_user` 想要的答案。早先的实现是"探针真写一个字节再恢复"——
/// 能用，但它会在用户页上留下一次写操作，而且恢复那一步一旦出问题
/// （比如页刚好只读）就会留下脏数据。直接读 PTE 的 RW 位干净得多，
/// 也不会有任何副作用。
pub fn page_writable_in(pml4: u64, va: u64) -> bool {
    if pml4 == 0 {
        return false;
    }
    let i4 = ((va >> 39) & 0x1FF) as usize;
    let e4 = pte_at(pml4, i4);
    if e4 & 1 == 0 || e4 & 2 == 0 {
        return false;
    }
    let i3 = ((va >> 30) & 0x1FF) as usize;
    let e3 = pte_at(e4 & kpt::ADDR_MASK, i3);
    if e3 & 1 == 0 || e3 & 2 == 0 {
        return false;
    }
    if e3 & 0x80 != 0 {
        return true;
    }
    let i2 = ((va >> 21) & 0x1FF) as usize;
    let e2 = pte_at(e3 & kpt::ADDR_MASK, i2);
    if e2 & 1 == 0 || e2 & 2 == 0 {
        return false;
    }
    if e2 & 0x80 != 0 {
        return true;
    }
    let i1 = ((va >> 12) & 0x1FF) as usize;
    let e1 = pte_at(e2 & kpt::ADDR_MASK, i1);
    e1 & 1 != 0 && e1 & 2 != 0
}

/// 这一帧是否已经映射在 `pml4` 的低半区里？（防"一帧两用"的硬检查）
///
/// 一帧被两个虚拟地址同时映射是内存管理最隐蔽的 bug：两个用途互相覆盖，
/// 现象是"用户态数据莫名其妙被改"。这里只走已存在的项，代价与映射数量成正比。
pub fn frame_mapped_in(pml4: u64, phys: u64) -> Option<u64> {
    if pml4 == 0 {
        return None;
    }
    for i4 in 0..256u64 {
        let e4 = pte_at(pml4, i4 as usize);
        if e4 & 1 == 0 {
            continue;
        }
        let pdpt = e4 & kpt::ADDR_MASK;
        for i3 in 0..512u64 {
            let e3 = pte_at(pdpt, i3 as usize);
            if e3 & 1 == 0 || e3 & 0x80 != 0 {
                continue;
            }
            let pd = e3 & kpt::ADDR_MASK;
            for i2 in 0..512u64 {
                let e2 = pte_at(pd, i2 as usize);
                if e2 & 1 == 0 || e2 & 0x80 != 0 {
                    continue;
                }
                let pt = e2 & kpt::ADDR_MASK;
                for i1 in 0..512u64 {
                    let e1 = pte_at(pt, i1 as usize);
                    if e1 & 1 != 0 && (e1 & kpt::ADDR_MASK) == phys {
                        return Some((i4 << 39) | (i3 << 30) | (i2 << 21) | (i1 << 12));
                    }
                }
            }
        }
    }
    None
}

/// 页表自检结果细项，供 kmain 打印。
pub struct Report {
    pub pml4_pages: usize,
    pub pattern: u64,
}

/// 页表端到端自检：
/// 1. 内核镜像在直接映射区与高半区读到同一份内容；
/// 2. 当前 CR3 里能翻译出内核高半区的正确物理地址；
/// 3. 新建页表 + 共享高半区 + 在 vmalloc 区映射一页新帧；
/// 4. **切换 CR3** 后向新映射写入图案并读回（真正让 CPU 走一遍页表）；
/// 5. 在新地址空间里直接映射区仍然有效（证明内核高半区共享正确）；
/// 6. 切回原 CR3，拆映射、退帧，帧数回到起点。
pub fn selftest() -> Result<Report, &'static str> {
    let mem = KernelMem;
    let mut fs = KernelFrames;
    let old_root = read_cr3();
    let free_before = frames::stats().map(|s| s.free_frames).ok_or("帧分配器未初始化")?;

    // ---- 1. 直接映射 vs 高半区 ----
    let (kstart, _kend) = frames::kernel_phys_range();
    let kva = KERNEL_VA_BASE + kstart;
    // SAFETY: 内核映像首 8 字节一定可读，两个地址都指向同一物理页。
    let via_direct = unsafe { core::ptr::read_volatile(phys_to_virt(kstart) as *const u64) };
    let via_high = unsafe { core::ptr::read_volatile(kva as *const u64) };
    if via_direct != via_high {
        return Err("直接映射区与高半区内容不一致");
    }

    // ---- 2. 现有 CR3 的翻译结果 ----
    // SAFETY: old_root 来自 CR3，一定是有效 PML4。
    let current = unsafe { PageTable::from_root(old_root) };
    match current.translate(&mem, kva) {
        Some((pa, _)) if pa == kstart => {}
        _ => return Err("内核高半区映射缺失或物理地址不对"),
    }

    // ---- 3. 新建地址空间 ----
    let mut pt = PageTable::create(&mem, &mut fs).ok_or("分配 PML4 失败")?;
    pt.copy_entries_from(&mem, old_root, 256, 512);
    let data = frames::alloc_frame().ok_or("分配数据帧失败")?;
    const PATTERN: u64 = 0x4C75_6D69_4B65_726E; // "LumiKern"
    pt.map(&mem, &mut fs, VMALLOC_BASE, data, Flags::KERNEL_RW)
        .map_err(|_| "建立 vmalloc 映射失败")?;
    match pt.translate(&mem, VMALLOC_BASE) {
        Some((pa, _)) if pa == data => {}
        _ => return Err("新页表翻译结果不对"),
    }

    // 新映射页先写成干净值，避免读到上一任使用者的数据
    // SAFETY: data 是刚分配、当前无人使用的帧，通过直接映射访问。
    unsafe { core::ptr::write_bytes(phys_to_virt(data) as *mut u8, 0, PAGE_SIZE) };

    // ---- 4. 真正切换地址空间 ----
    write_cr3(pt.root_phys());
    // SAFETY: 新表已复制内核高半区（含当前代码与栈）并映射了 VMALLOC_BASE。
    let observed = unsafe {
        core::ptr::write_volatile(VMALLOC_BASE as *mut u64, PATTERN);
        core::ptr::read_volatile(VMALLOC_BASE as *const u64)
    };
    if observed != PATTERN {
        write_cr3(old_root);
        return Err("切换 CR3 后写入/读回不一致");
    }
    // ---- 5. 直接映射在新地址空间里仍然有效 ----
    // SAFETY: 同一个物理帧，走直接映射读。
    let via_direct_again = unsafe { core::ptr::read_volatile(phys_to_virt(data) as *const u64) };
    if via_direct_again != PATTERN {
        write_cr3(old_root);
        return Err("新地址空间里直接映射区失效");
    }
    write_cr3(old_root);

    // ---- 6. 收尾：拆映射、退帧 ----
    let freed_data = pt
        .unmap(&mem, &mut fs, VMALLOC_BASE)
        .map_err(|_| "解除 vmalloc 映射失败")?;
    if freed_data != data {
        return Err("unmap 返回的物理地址不对");
    }
    frames::free_frame(data);
    frames::free_frame(pt.root_phys());

    let free_after = frames::stats().map(|s| s.free_frames).ok_or("帧分配器读数失败")?;
    if free_after != free_before {
        return Err("自检结束后帧数没有回到起点（有泄漏）");
    }

    Ok(Report {
        pml4_pages: 1,
        pattern: PATTERN,
    })
}
