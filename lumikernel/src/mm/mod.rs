//! 内存管理（M1）
//!
//! 分工：
//! - [`frames`]：物理帧分配（E820 → 位图 → 可分配/可回收），全局唯一实例；
//! - [`heap`]：内核堆，从帧分配器取一段连续物理内存，挂上 `kheap` 做全局分配器；
//! - [`paging`]：页表对象与地址空间布局，负责把物理页映射到虚拟地址。
//!
//! 地址空间布局（与 `boot.S`、`linker.ld` 严格一致，改一处必须改另外两处）：

/// 内核映像装载的物理地址。
pub const PHYS_LOAD: u64 = 0x0010_0000;

/// 内核高半区基址：虚拟地址 = 物理地址 + 这个偏移。
pub const KERNEL_VA_BASE: u64 = 0xFFFF_FFFF_8000_0000;

/// 直接映射区基址：虚拟地址 = 物理地址 + 这个基址。
pub const DIRECT_MAP_BASE: u64 = 0xFFFF_8000_0000_0000;

/// 直接映射区大小（M1 由 boot.S 用 2 MiB 大页铺满 1 GiB）。
pub const DIRECT_MAP_SIZE: u64 = 1 << 30;

/// vmalloc 区基址：按需建立 4 KiB 映射，用来验证页表对象。
pub const VMALLOC_BASE: u64 = 0xFFFF_A000_0000_0000;

/// 内核堆默认大小。
pub const HEAP_BYTES: usize = 4 << 20;

pub mod cow;
pub mod fault;
pub mod frames;
pub mod heap;
pub mod paging;
