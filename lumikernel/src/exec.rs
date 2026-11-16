//! ELF64 装载器（M5 起，M6 改为**每进程独立地址空间**，M9 支持往已有地址空间里装）
//!
//! 两个用法：
//! - [`spawn_from_elf`]：造一张新表 + 装载 + 建任务（M5/M6 自检用）；
//! - [`load_image`] / [`map_stack`]：往**调用方给的**表里装载（M9 的 `execve` 用，
//!   因为 `execve` 要保留进程身份、fd 表，只换映像）。
//!
//! 一条仍然存在的限制：内核页表项在进程表里是**共享**的（不是每进程复制内核数据）。
//! 这是宏内核的常规做法；`user=false` 的映射，CPL3 访问会直接 `#PF`。

use kelf::ElfImage;
use kvm::Flags;
use kpt::PageTable;

use crate::mm::{frames, paging};
use crate::task;

/// 用户地址空间的上界（低半区里留给用户的最高地址）。
///
/// 越过这条线就撞到内核高半区了。用户段必须整个落在它下面，
/// 否则一个恶意 ELF 能把段"装"到内核映像上——页表项会覆盖内核自己的映射，
/// 等于用户程序自己给自己开了后门。
pub const USER_LIMIT: u64 = 0x0000_7FFF_FFFF_F000;

/// 用户栈顶（2 GiB）。每个进程都有自己的表，所以这个地址是"每进程私有"的。
pub const USER_STACK_TOP: u64 = 0x0000_0000_8000_0000;
/// 自检用的小栈（16 KiB）。
pub const USER_STACK_PAGES: usize = 4;
/// 给真正的 Linux 程序用的栈（32 KiB）。busybox 的 `sh` 递归解析语法时会用掉不少。
pub const USER_STACK_PAGES_LINUX: usize = 8;

const PAGE: u64 = 4096;

/// 装载结果。
pub struct Report {
    pub entry: u64,
    pub segments: usize,
    pub pages: usize,
    pub tid: u64,
    pub pml4: u64,
    pub mem_bytes: u64,
}

/// 一次装载的摘要（`execve` 用）。
pub struct Loaded {
    pub entry: u64,
    /// 映像末尾（按页对齐）——`brk` 就从这里开始。
    pub brk_start: u64,
    pub pages: usize,
    /// 装载了多少个 `PT_LOAD` 段（自检要打印）。
    pub segments: usize,
    pub phdr: Option<u64>,
    pub phnum: usize,
}

/// 代码段：可读可执行；数据段：可读可写（NX）。W^X 在页表层面就是分开的。
fn flags_of(seg: &kelf::Segment) -> Flags {
    if seg.is_exec() {
        Flags {
            present: true,
            writable: false,
            user: true,
            executable: true,
            cache_disable: false,
        }
    } else {
        Flags {
            present: true,
            writable: seg.is_write(),
            user: true,
            executable: false,
            cache_disable: false,
        }
    }
}

fn elf_err(e: kelf::ElfError) -> &'static str {
    match e {
        kelf::ElfError::Pie => "ELF 是 PIE（ET_DYN），暂不支持",
        kelf::ElfError::NeedsInterp => "ELF 需要动态链接器，暂不支持",
        kelf::ElfError::NotX86_64 => "ELF 不是 x86_64",
        kelf::ElfError::NotExecutable => "ELF 不是可执行文件",
        kelf::ElfError::BadMagic => "不是 ELF 文件",
        _ => "ELF 头/程序头非法",
    }
}

/// 往**已存在**的页表里装一个 ELF，返回入口等信息。
///
/// 段内容按 `p_vaddr` 原样落位：低地址之所以能用，是因为每个进程有自己的页表。
pub fn load_image(pt: &mut PageTable, bytes: &[u8]) -> Result<Loaded, &'static str> {
    let img = ElfImage::parse(bytes).map_err(elf_err)?;

    // 入口和所有段都必须落在用户区：挡的是"把自己装到内核头上"的那类 ELF
    if img.entry() > USER_LIMIT || img.entry() == 0 {
        return Err("ELF 入口不在用户地址空间");
    }
    for seg in img.segments() {
        match seg.end_vaddr() {
            Some(e) if e <= USER_LIMIT && seg.vaddr >= PAGE => {}
            _ => return Err("ELF 段越过用户地址空间上界"),
        }
    }

    let mut pages = 0usize;
    for (idx, seg) in img.segments().iter().enumerate() {
        if seg.memsz == 0 {
            continue;
        }
        let data = img
            .segment_data(bytes, idx)
            .ok_or("段数据越界（文件比头里声明的短）")?;

        let first_page = seg.vaddr & !(PAGE - 1);
        let end = seg.vaddr.checked_add(seg.memsz).ok_or("段地址溢出")?;
        let last_page = (end + PAGE - 1) & !(PAGE - 1);

        let mut va = first_page;
        while va < last_page {
            let phys = frames::alloc_frame().ok_or("装载 ELF 时物理帧不足")?;
            // SAFETY: 刚分配的独占帧，经直接映射区清零。
            unsafe {
                core::ptr::write_bytes(paging::phys_to_virt(phys) as *mut u8, 0, PAGE as usize)
            };

            // 本页里属于文件内容的部分：[max(va,vaddr), min(va+PAGE, vaddr+filesz))
            let seg_end = seg.vaddr.saturating_add(seg.filesz);
            let lo = va.max(seg.vaddr);
            let hi = (va + PAGE).min(seg_end);
            if lo < hi {
                let src_off = (lo - seg.vaddr) as usize;
                let dst_off = (lo - va) as usize;
                let len = (hi - lo) as usize;
                // SAFETY: 目标在本帧内，源在 data 内（kelf 已校验 p_offset+p_filesz 不越界）。
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        data.as_ptr().add(src_off),
                        (paging::phys_to_virt(phys) as *mut u8).add(dst_off),
                        len,
                    );
                }
            }

            paging::map_in(pt, va, phys, flags_of(seg)).map_err(|_| "用户段映射失败")?;
            pages += 1;
            va += PAGE;
        }
    }

    let brk_start = (img.max_vaddr() + PAGE - 1) & !(PAGE - 1);
    Ok(Loaded {
        entry: img.entry(),
        brk_start,
        pages,
        segments: img.segments().len(),
        phdr: img.phdr_vaddr(),
        phnum: img.phnum(),
    })
}

/// 在给定页表里铺用户栈。返回（栈顶 - 1 页的）最低地址，仅供诊断。
pub fn map_stack(pt: &mut PageTable, pages: usize) -> Result<u64, &'static str> {
    for i in 0..pages {
        let va = USER_STACK_TOP - ((i as u64 + 1) * PAGE);
        let phys = frames::alloc_frame().ok_or("用户栈分配失败")?;
        // SAFETY: 刚分配的独占帧。
        unsafe {
            core::ptr::write_bytes(paging::phys_to_virt(phys) as *mut u8, 0, PAGE as usize)
        };
        paging::map_in(pt, va, phys, Flags::USER_RW).map_err(|_| "用户栈映射失败")?;
    }
    Ok(USER_STACK_TOP - (pages as u64) * PAGE)
}

/// 从 ELF 字节装载出一个**独立地址空间**的进程（M5/M6 的老路径）。
pub fn spawn_from_elf(bytes: &[u8]) -> Result<Report, &'static str> {
    let mut pt = paging::new_user_table().ok_or("用户页表分配失败")?;
    let loaded = load_image(&mut pt, bytes)?;
    map_stack(&mut pt, USER_STACK_PAGES)?;

    let pml4 = pt.root_phys();
    let mem_bytes = (loaded.pages + USER_STACK_PAGES) as u64 * PAGE;
    let tid = task::spawn_user_at(loaded.entry, USER_STACK_TOP, pml4)?;
    Ok(Report {
        entry: loaded.entry,
        segments: loaded.segments,
        pages: loaded.pages,
        tid,
        pml4,
        mem_bytes,
    })
}
