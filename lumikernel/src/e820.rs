//! E820 物理内存表解析（由 stage2 在实模式下探测并固化到 0x9000）
//!
//! 布局（与 boot/stage2.asm 严格对齐）：
//!   0x9000 : u16  条目数
//!   0x9008 : 每条 24 字节 { base: u64, len: u64, type: u32, acpi: u32 }
//!
//! 用固定物理地址而不是 multiboot 结构体，是因为引导链是自己的：
//! stage2 探测完直接放在这里，内核早期就能读到，不依赖任何外部协议。

#[allow(dead_code)] // 布局文档常量：外部工具与测试按它对齐
pub const E820_ADDR: usize = 0x9000;
#[allow(dead_code)]
pub const E820_ENTRIES: usize = 0x9008;
#[allow(dead_code)]
pub const ENTRY_SIZE: usize = 24;
/// 条目上限。BIOS 链（stage2）给的表很小，但 UEFI 的内存图动辄几十条，
/// 装载器会合并同类项后传过来；64 条足够覆盖 1 GiB 以内的真实布局。
const MAX_ENTRIES: usize = 64;

#[derive(Clone, Copy)]
#[repr(C)]
pub struct E820Entry {
    pub base: u64,
    pub len: u64,
    pub typ: u32,
    pub acpi: u32,
}

pub const TYPE_AVAILABLE: u32 = 1;

/// 引导来源标记（放在 count 与条目数组之间的空档，偏移 +6）。
/// 0xEF = UEFI 装载器写的，其它 = 传统 BIOS 链。
pub const SOURCE_MARK: u16 = 6;
pub const SOURCE_UEFI: u8 = 0xEF;

/// 系统盘（ext2）在**本盘**上的起始 LBA。
///
/// 真机只有一块 U 盘：ESP（装载器）+ ext2（根文件系统）+ LumiFS 都在同一块盘上。
/// UEFI 装载器把 ext2 分区的起始 LBA 写在这里；BIOS 链（stage2）不写，
/// 保持 0——那种情况下 ext2 就是"另一块盘从 0 开始"，语义自洽。
///
/// # Safety
/// 同 [`table`]。
pub unsafe fn part_lba(addr: usize) -> u32 {
    core::ptr::read_volatile((addr + 2) as *const u32)
}

/// "uefi" / "bios"：日志里一眼看出这次是怎么起来的。
///
/// # Safety
/// 同 [`table`]：调用时必须在恒等映射下，且表已被写好。
pub unsafe fn boot_source(addr: usize) -> &'static str {
    if *(addr as *const u8).add(SOURCE_MARK as usize) == SOURCE_UEFI {
        "uefi"
    } else {
        "bios"
    }
}

/// 返回 (条目数, 指向条目数组的指针)。
///
/// # Safety
/// 调用方必须保证运行在引导早期的恒等映射环境下，且 `stage2` 确实写入了这张表。
pub unsafe fn table(addr: usize) -> (usize, *const E820Entry) {
    let count = *(addr as *const u16) as usize;
    let count = count.min(MAX_ENTRIES);
    let ptr = (addr + 8) as *const E820Entry;
    (count, ptr)
}

pub fn entry_at(ptr: *const E820Entry, i: usize) -> E820Entry {
    // SAFETY: 调用方传入的 ptr 指向 stage2 写入的连续数组，i 已由 table() 的条目数约束。
    unsafe { *ptr.add(i) }
}

/// 统计可用内存字节数
pub fn usable_bytes(count: usize, ptr: *const E820Entry) -> u64 {
    let mut total = 0u64;
    for i in 0..count {
        let e = entry_at(ptr, i);
        if e.typ == TYPE_AVAILABLE {
            total += e.len;
        }
    }
    total
}

pub fn type_name(t: u32) -> &'static str {
    match t {
        1 => "可用",
        2 => "保留",
        3 => "ACPI 可回收",
        4 => "ACPI NVS",
        5 => "坏页",
        _ => "未知",
    }
}

pub const _ENTRY_SIZE_CHECK: () = assert!(core::mem::size_of::<E820Entry>() == ENTRY_SIZE);
