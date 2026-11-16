//! kpt —— x86_64 硬件页表操作（真实 PTE 读写 + 中间层按需分配）
//!
//! 与 `kvm` 的分工：
//! - `kvm` 是**纯模型**，用来在不碰硬件的条件下推演映射语义、做随机对拍；
//! - `kpt` 是**真实页表**，直接改内存里的 PTE、装 CR3 生效。
//!
//! 两者共享 [`kvm::Flags`] 的位定义，避免"模型说可写、硬件说只读"这种错位。
//!
//! 为了能在宿主机上测试，物理内存与帧分配都通过 trait 注入：
//! - [`PhysMem`]：物理地址 → 可读写指针（内核里就是直接映射区）；
//! - [`FrameSource`]：分配/释放 4 KiB 页帧（内核里就是 `kmem` 的位图分配器）。
//!
//! 这样 `cargo test -p kpt` 就能在普通进程里跑真实的四级页表逻辑，
//! 包括"中间表按需创建"和"unmap 后回收空表"这些最容易写错的部分。

#![cfg_attr(not(test), no_std)]

pub use kvm::{Flags, PAGE_SIZE};

/// PTE 里的物理地址掩码（bit 12..51）。
pub const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// 1 GiB 大页基址掩码（bit 30..51）。
const MASK_1G: u64 = 0x000F_FFFF_C000_0000;
/// 2 MiB 大页基址掩码（bit 21..51）。
const MASK_2M: u64 = 0x000F_FFFF_FFE0_0000;

const P: u64 = 1 << 0;
const RW: u64 = 1 << 1;
const US: u64 = 1 << 2;
const PS: u64 = 1 << 7;

/// 物理内存访问接口。
pub trait PhysMem {
    /// 返回 `phys` 处可读写的指针。
    ///
    /// 调用方保证 `phys` 是有效的 RAM 地址；越界属于调用方 bug。
    fn at(&self, phys: u64) -> *mut u8;
}

/// 页帧来源。
pub trait FrameSource {
    fn alloc_frame(&mut self) -> Option<u64>;
    fn free_frame(&mut self, phys: u64);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PtError {
    /// 页表页分配失败。
    OutOfMemory,
    /// 地址未按页对齐。
    NotAligned,
    /// 非规范地址。
    NonCanonical,
    /// 该虚拟地址已有映射。
    AlreadyMapped,
    /// 该虚拟地址没有映射。
    NotMapped,
    /// 撞上大页，本层不支持（需要先拆分，M3 再做）。
    HugePage,
    /// 权限位里 present 为 false，无法建立映射。
    NotPresent,
}

#[inline]
fn index_of(vaddr: u64, level: u32) -> usize {
    ((vaddr >> (12 + 9 * level)) & 0x1FF) as usize
}

#[inline]
unsafe fn read_entry(mem: &impl PhysMem, table: u64, i: usize) -> u64 {
    // SAFETY: table 是页表物理基址，i < 512，页表页 4 KiB 且已知有效。
    core::ptr::read_volatile((mem.at(table) as *const u64).add(i))
}

#[inline]
unsafe fn write_entry(mem: &impl PhysMem, table: u64, i: usize, v: u64) {
    // SAFETY: 同 read_entry。volatile 防止编译器把页表写入优化掉/乱序。
    core::ptr::write_volatile((mem.at(table) as *mut u64).add(i), v);
}

fn zero_frame(mem: &impl PhysMem, phys: u64) {
    // SAFETY: phys 是刚分配、未被使用的 4 KiB 帧。
    unsafe { core::ptr::write_bytes(mem.at(phys), 0, PAGE_SIZE) }
}

fn table_empty(mem: &impl PhysMem, table: u64) -> bool {
    for i in 0..512 {
        // SAFETY: table 是有效页表页。
        if unsafe { read_entry(mem, table, i) } != 0 {
            return false;
        }
    }
    true
}

/// 一棵四级页表。
pub struct PageTable {
    root: u64,
}

impl PageTable {
    /// 新建一棵空页表（只分配 PML4 一页并清零）。
    pub fn create(mem: &impl PhysMem, frames: &mut impl FrameSource) -> Option<Self> {
        let root = frames.alloc_frame()?;
        zero_frame(mem, root);
        Some(PageTable { root })
    }

    /// 包裹一个已存在的 PML4 物理地址（例如当前 CR3 指向的表）。
    ///
    /// # Safety
    /// `root` 必须是一个有效的 PML4 物理地址，且调用方有权修改它。
    pub unsafe fn from_root(root: u64) -> Self {
        PageTable { root }
    }

    pub fn root_phys(&self) -> u64 {
        self.root
    }

    /// 建立 4 KiB 页映射，中间层按需分配。
    pub fn map(
        &mut self,
        mem: &impl PhysMem,
        frames: &mut impl FrameSource,
        vaddr: u64,
        paddr: u64,
        flags: Flags,
    ) -> Result<(), PtError> {
        if !kvm::is_canonical(vaddr) {
            return Err(PtError::NonCanonical);
        }
        if vaddr & (PAGE_SIZE as u64 - 1) != 0 || paddr & (PAGE_SIZE as u64 - 1) != 0 {
            return Err(PtError::NotAligned);
        }
        if !flags.present {
            return Err(PtError::NotPresent);
        }

        let mut table = self.root;
        // level 3 = PML4, 2 = PDPT, 1 = PD；逐层往下，缺哪层补哪层。
        for level in (1..=3u32).rev() {
            let i = index_of(vaddr, level);
            // SAFETY: table 一路都是有效页表页。
            let e = unsafe { read_entry(mem, table, i) };
            if e & P == 0 {
                let frame = frames.alloc_frame().ok_or(PtError::OutOfMemory)?;
                zero_frame(mem, frame);
                // 中间层必须可写；用户页要求整条链路都带 US，否则 #PF。
                let mut ne = frame | P | RW;
                if flags.user {
                    ne |= US;
                }
                // SAFETY: 同上。
                unsafe { write_entry(mem, table, i, ne) };
                table = frame;
            } else {
                if e & PS != 0 {
                    return Err(PtError::HugePage);
                }
                if flags.user && e & US == 0 {
                    // SAFETY: 同上。
                    unsafe { write_entry(mem, table, i, e | US) };
                }
                table = e & ADDR_MASK;
            }
        }

        let i = index_of(vaddr, 0);
        // SAFETY: table 是最后一级页表页。
        let old = unsafe { read_entry(mem, table, i) };
        if old & P != 0 {
            return Err(PtError::AlreadyMapped);
        }
        // SAFETY: 同上。
        unsafe { write_entry(mem, table, i, (paddr & ADDR_MASK) | flags.bits()) };
        Ok(())
    }

    /// 映射一段连续区间（`vaddr`/`paddr` 必须页对齐，`len` 向上取整到页）。
    pub fn map_range(
        &mut self,
        mem: &impl PhysMem,
        frames: &mut impl FrameSource,
        vaddr: u64,
        paddr: u64,
        len: usize,
        flags: Flags,
    ) -> Result<usize, PtError> {
        if len == 0 {
            return Ok(0);
        }
        let pages = (len + PAGE_SIZE - 1) / PAGE_SIZE;
        for n in 0..pages {
            let off = (n * PAGE_SIZE) as u64;
            self.map(mem, frames, vaddr + off, paddr + off, flags)?;
        }
        Ok(pages)
    }

    /// 翻译虚拟地址。支持 1 GiB / 2 MiB 大页。
    pub fn translate(&self, mem: &impl PhysMem, vaddr: u64) -> Option<(u64, Flags)> {
        if !kvm::is_canonical(vaddr) {
            return None;
        }
        let mut table = self.root;
        for level in (1..=3u32).rev() {
            // SAFETY: table 是有效页表页。
            let e = unsafe { read_entry(mem, table, index_of(vaddr, level)) };
            if e & P == 0 {
                return None;
            }
            if e & PS != 0 {
                let (base, size) = match level {
                    2 => (e & MASK_1G, 1u64 << 30),
                    1 => (e & MASK_2M, 1u64 << 21),
                    _ => return None,
                };
                return Some((base + (vaddr & (size - 1)), Flags::from_bits(e)));
            }
            table = e & ADDR_MASK;
        }
        // SAFETY: table 是最后一级页表页。
        let e = unsafe { read_entry(mem, table, index_of(vaddr, 0)) };
        if e & P == 0 {
            return None;
        }
        Some(((e & ADDR_MASK) | (vaddr & (PAGE_SIZE as u64 - 1)), Flags::from_bits(e)))
    }

    /// 拆除单个 4 KiB 映射，并回收因此变空的中间页表页。
    /// 返回原映射的物理地址。
    pub fn unmap(
        &mut self,
        mem: &impl PhysMem,
        frames: &mut impl FrameSource,
        vaddr: u64,
    ) -> Result<u64, PtError> {
        if !kvm::is_canonical(vaddr) {
            return Err(PtError::NonCanonical);
        }
        if vaddr & (PAGE_SIZE as u64 - 1) != 0 {
            return Err(PtError::NotAligned);
        }

        let mut tables = [0u64; 4];
        tables[0] = self.root;
        let mut table = self.root;
        for level in (1..=3u32).rev() {
            // SAFETY: table 是有效页表页。
            let e = unsafe { read_entry(mem, table, index_of(vaddr, level)) };
            if e & P == 0 {
                return Err(PtError::NotMapped);
            }
            if e & PS != 0 {
                return Err(PtError::HugePage);
            }
            table = e & ADDR_MASK;
            // level 3 → tables[1], 2 → [2], 1 → [3]
            tables[(4 - level) as usize] = table;
        }

        let i = index_of(vaddr, 0);
        // SAFETY: table 是最后一级页表页。
        let e = unsafe { read_entry(mem, table, i) };
        if e & P == 0 {
            return Err(PtError::NotMapped);
        }
        // SAFETY: 同上。
        unsafe { write_entry(mem, table, i, 0) };
        let paddr = e & ADDR_MASK;

        // 自底向上回收空表：PT → PD → PDPT
        for k in (1..=3usize).rev() {
            if tables[k] == 0 || !table_empty(mem, tables[k]) {
                break;
            }
            frames.free_frame(tables[k]);
            let level = 4 - k as u32; // k=3 → 1(PD), 2 → 2(PDPT), 1 → 3(PML4)
            // SAFETY: tables[k-1] 是 tables[k] 的父表，entry 存在。
            unsafe { write_entry(mem, tables[k - 1], index_of(vaddr, level), 0) };
        }
        Ok(paddr)
    }

    /// 读取某一级的原始 PTE（调试与测试用）。
    pub fn raw_entry(&self, mem: &impl PhysMem, vaddr: u64, level: u32) -> Option<u64> {
        if !kvm::is_canonical(vaddr) || level > 3 {
            return None;
        }
        let mut table = self.root;
        for l in (level..=3u32).rev() {
            // SAFETY: table 是有效页表页。
            let e = unsafe { read_entry(mem, table, index_of(vaddr, l)) };
            if e & P == 0 {
                return None;
            }
            if l == level {
                return Some(e);
            }
            if e & PS != 0 {
                return None;
            }
            table = e & ADDR_MASK;
        }
        None
    }

    /// 从另一棵页表拷贝 PML4 区间 `[from, to)` 的表项。
    ///
    /// 用途：新建页表时把内核高半区共享过来，再切换到新表（内核里做
    /// "进程地址空间" 就是这个动作的前身）。注意共享的是**下一级页表**，
    /// 因此新表在这段区间内的映射改动会同步影响源表——这正是内核高半区想要的。
    pub fn copy_entries_from(&mut self, mem: &impl PhysMem, src_root: u64, from: usize, to: usize) {
        let to = to.min(512);
        for i in from.min(to)..to {
            // SAFETY: 两张表都是有效 PML4 页。
            let e = unsafe { read_entry(mem, src_root, i) };
            unsafe { write_entry(mem, self.root, i, e) };
        }
    }
}

/* ------------------------------------------------------------------ */
/* 测试                                                                */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    const MEM_SIZE: usize = 8 << 20;
    const POOL_START: u64 = 0x10000;

    struct Mem {
        base: *mut u8,
    }

    impl Mem {
        fn new() -> Self {
            let b = vec![0u8; MEM_SIZE].into_boxed_slice();
            Mem {
                base: Box::leak(b).as_mut_ptr(),
            }
        }
    }

    impl PhysMem for Mem {
        fn at(&self, phys: u64) -> *mut u8 {
            assert!(phys < MEM_SIZE as u64, "物理地址越界: {:#x}", phys);
            // SAFETY: 上面已确认 phys 在本次分配的内存范围内。
            unsafe { self.base.add(phys as usize) }
        }
    }

    struct Frames {
        next: u64,
        end: u64,
        allocs: usize,
        frees: usize,
    }

    impl Frames {
        fn new() -> Self {
            Frames {
                next: POOL_START,
                end: MEM_SIZE as u64 - 0x1000,
                allocs: 0,
                frees: 0,
            }
        }
        fn live(&self) -> usize {
            self.allocs - self.frees
        }
    }

    impl FrameSource for Frames {
        fn alloc_frame(&mut self) -> Option<u64> {
            if self.next + 0x1000 > self.end {
                return None;
            }
            let f = self.next;
            self.next += 0x1000;
            self.allocs += 1;
            Some(f)
        }
        fn free_frame(&mut self, phys: u64) {
            assert!(phys >= POOL_START && phys < self.end, "释放了池外帧 {:#x}", phys);
            self.frees += 1;
        }
    }

    struct Fix {
        mem: Mem,
        frames: Frames,
    }

    impl Fix {
        fn new() -> Self {
            Fix {
                mem: Mem::new(),
                frames: Frames::new(),
            }
        }
        fn pt(&mut self) -> PageTable {
            PageTable::create(&self.mem, &mut self.frames).unwrap()
        }
    }

    const VA: u64 = 0xFFFF_A000_0000_0000;

    #[test]
    fn create_zeroes_root() {
        let mut f = Fix::new();
        let pt = f.pt();
        assert_eq!(pt.root_phys() % 4096, 0);
        assert!(pt.translate(&f.mem, VA).is_none());
        assert_eq!(f.frames.live(), 1, "只应分配 PML4 一页");
    }

    #[test]
    fn map_translate_roundtrip() {
        let mut f = Fix::new();
        let mut pt = f.pt();
        pt.map(&f.mem, &mut f.frames, VA, 0x200000, Flags::KERNEL_RW)
            .unwrap();
        let (pa, fl) = pt.translate(&f.mem, VA).unwrap();
        assert_eq!(pa, 0x200000);
        assert!(fl.present && fl.writable && !fl.user && !fl.executable);
        // 页内偏移必须保留
        assert_eq!(pt.translate(&f.mem, VA + 0xabc).unwrap().0, 0x200abc);
        // 相邻页未映射
        assert!(pt.translate(&f.mem, VA + 0x1000).is_none());
        assert_eq!(f.frames.live(), 4, "PML4+PDPT+PD+PT");
    }

    #[test]
    fn mapping_512_pages_reuses_tables() {
        let mut f = Fix::new();
        let mut pt = f.pt();
        for i in 0..512u64 {
            pt.map(&f.mem, &mut f.frames, VA + i * 4096, 0x1000000 + i * 4096, Flags::KERNEL_RW)
                .unwrap();
        }
        for i in 0..512u64 {
            assert_eq!(
                pt.translate(&f.mem, VA + i * 4096).unwrap().0,
                0x1000000 + i * 4096
            );
        }
        assert_eq!(f.frames.live(), 4, "512 页共用一套中间表");
    }

    #[test]
    fn map_twice_is_already_mapped() {
        let mut f = Fix::new();
        let mut pt = f.pt();
        pt.map(&f.mem, &mut f.frames, VA, 0x200000, Flags::KERNEL_RW)
            .unwrap();
        assert_eq!(
            pt.map(&f.mem, &mut f.frames, VA, 0x300000, Flags::KERNEL_RW),
            Err(PtError::AlreadyMapped)
        );
        assert_eq!(pt.translate(&f.mem, VA).unwrap().0, 0x200000, "原映射不能被改");
    }

    #[test]
    fn unmap_reclaims_empty_tables() {
        let mut f = Fix::new();
        let mut pt = f.pt();
        pt.map(&f.mem, &mut f.frames, VA, 0x200000, Flags::KERNEL_RW)
            .unwrap();
        assert_eq!(f.frames.live(), 4);
        let pa = pt.unmap(&f.mem, &mut f.frames, VA).unwrap();
        assert_eq!(pa, 0x200000);
        assert!(pt.translate(&f.mem, VA).is_none());
        assert_eq!(f.frames.live(), 1, "空表必须回收，否则每 unmap 一次漏 3 页");
    }

    #[test]
    fn unmap_keeps_shared_tables() {
        let mut f = Fix::new();
        let mut pt = f.pt();
        // 同 2MiB 区间内两页，unmap 一页后另一页还在，表不能回收
        pt.map(&f.mem, &mut f.frames, VA, 0x200000, Flags::KERNEL_RW).unwrap();
        pt.map(&f.mem, &mut f.frames, VA + 4096, 0x201000, Flags::KERNEL_RW).unwrap();
        pt.unmap(&f.mem, &mut f.frames, VA).unwrap();
        assert_eq!(f.frames.live(), 4);
        assert_eq!(pt.translate(&f.mem, VA + 4096).unwrap().0, 0x201000);
    }

    #[test]
    fn unmap_unmapped_is_error() {
        let mut f = Fix::new();
        let mut pt = f.pt();
        assert_eq!(pt.unmap(&f.mem, &mut f.frames, VA), Err(PtError::NotMapped));
    }

    #[test]
    fn remap_after_unmap_works() {
        let mut f = Fix::new();
        let mut pt = f.pt();
        pt.map(&f.mem, &mut f.frames, VA, 0x200000, Flags::KERNEL_RW).unwrap();
        pt.unmap(&f.mem, &mut f.frames, VA).unwrap();
        pt.map(&f.mem, &mut f.frames, VA, 0x400000, Flags::KERNEL_RX).unwrap();
        let (pa, fl) = pt.translate(&f.mem, VA).unwrap();
        assert_eq!(pa, 0x400000);
        assert!(fl.executable && !fl.writable);
    }

    #[test]
    fn rejects_misaligned_and_noncanonical() {
        let mut f = Fix::new();
        let mut pt = f.pt();
        assert_eq!(
            pt.map(&f.mem, &mut f.frames, VA + 1, 0x200000, Flags::KERNEL_RW),
            Err(PtError::NotAligned)
        );
        assert_eq!(
            pt.map(&f.mem, &mut f.frames, VA, 0x200001, Flags::KERNEL_RW),
            Err(PtError::NotAligned)
        );
        // 非规范地址
        assert_eq!(
            pt.map(&f.mem, &mut f.frames, 0x0000_8000_0000_0000, 0x200000, Flags::KERNEL_RW),
            Err(PtError::NonCanonical)
        );
        assert!(pt.translate(&f.mem, 0x0000_8000_0000_0000).is_none());
    }

    #[test]
    fn user_flag_propagates_to_all_levels() {
        let mut f = Fix::new();
        let mut pt = f.pt();
        pt.map(&f.mem, &mut f.frames, VA, 0x200000, Flags::USER_RW).unwrap();
        for level in 1..=3 {
            let e = pt.raw_entry(&f.mem, VA, level).unwrap();
            assert!(e & US != 0, "第 {} 级缺 US 位 → 用户态访问会 #PF", level);
        }
        let leaf = pt.raw_entry(&f.mem, VA, 0).unwrap();
        assert!(leaf & US != 0);
    }

    #[test]
    fn nx_bit_set_for_data_pages() {
        let mut f = Fix::new();
        let mut pt = f.pt();
        pt.map(&f.mem, &mut f.frames, VA, 0x200000, Flags::KERNEL_RW).unwrap();
        let leaf = pt.raw_entry(&f.mem, VA, 0).unwrap();
        assert!(leaf & (1 << 63) != 0, "数据页必须 NX，这是 W^X 的基础");
        pt.map(&f.mem, &mut f.frames, VA + 0x1000, 0x201000, Flags::KERNEL_RX).unwrap();
        let code = pt.raw_entry(&f.mem, VA + 0x1000, 0).unwrap();
        assert_eq!(code & (1 << 63), 0, "可执行页不能带 NX");
    }

    #[test]
    fn huge_page_is_detected() {
        let mut f = Fix::new();
        let mut pt = f.pt();
        // 手工把 PD 的第 0 项写成 2MiB 大页
        pt.map(&f.mem, &mut f.frames, VA, 0x200000, Flags::KERNEL_RW).unwrap();
        // PD 的地址 = PDPT 表项里的物理地址；要改的是 VA 对应的那一项
        let pd = pt.raw_entry(&f.mem, VA, 2).unwrap() & ADDR_MASK;
        let slot = index_of(VA, 1);
        // SAFETY: 直接用 PhysMem 改测试自己的页表。
        unsafe { write_entry(&f.mem, pd, slot, 0x200000 | P | RW | PS) };
        assert_eq!(pt.translate(&f.mem, VA + 0x1234).unwrap().0, 0x201234);
        assert_eq!(
            pt.map(&f.mem, &mut f.frames, VA + 0x1000, 0x300000, Flags::KERNEL_RW),
            Err(PtError::HugePage)
        );
    }

    #[test]
    fn copy_entries_shares_kernel_half() {
        let mut f = Fix::new();
        let mut kernel = f.pt();
        // 内核高半区某一项
        let high = 0xFFFF_FFFF_8000_0000u64;
        kernel.map(&f.mem, &mut f.frames, high, 0x100000, Flags::KERNEL_RX).unwrap();
        let mut task = f.pt();
        task.copy_entries_from(&f.mem, kernel.root_phys(), 256, 512);
        assert_eq!(task.translate(&f.mem, high).unwrap().0, 0x100000);
        // 低半区不共享
        let low = 0x0000_4000_0000u64;
        kernel.map(&f.mem, &mut f.frames, low, 0x500000, Flags::USER_RW).unwrap();
        assert!(task.translate(&f.mem, low).is_none());
    }

    #[test]
    fn oom_is_reported_not_panicked() {
        let mut f = Fix::new();
        // 把帧池榨干
        f.frames.end = POOL_START + 4096; // 只够 PML4
        let mut pt = PageTable::create(&f.mem, &mut f.frames).unwrap();
        assert_eq!(
            pt.map(&f.mem, &mut f.frames, VA, 0x200000, Flags::KERNEL_RW),
            Err(PtError::OutOfMemory)
        );
    }

    #[test]
    fn random_map_unmap_matches_model() {
        let mut f = Fix::new();
        let mut pt = f.pt();
        let mut model: Vec<(u64, u64)> = Vec::new(); // (va, pa)
        let mut seed = 0x1234_5678_9ABC_DEF0u64;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            seed >> 33
        };
        for _ in 0..3000 {
            let slot = (rnd() % 64) as u64;
            let va = VA + slot * 4096;
            let mapped = model.iter().any(|(v, _)| *v == va);
            if !mapped || rnd() % 4 == 0 {
                let pa = 0x200000 + (rnd() % 512) * 4096;
                match pt.map(&f.mem, &mut f.frames, va, pa, Flags::KERNEL_RW) {
                    Ok(()) => {
                        model.retain(|(v, _)| *v != va);
                        model.push((va, pa));
                    }
                    Err(PtError::AlreadyMapped) => assert!(mapped),
                    Err(e) => panic!("意外错误: {:?}", e),
                }
            } else if let Some(i) = model.iter().position(|(v, _)| *v == va) {
                let (_, expect) = model.remove(i);
                assert_eq!(pt.unmap(&f.mem, &mut f.frames, va).unwrap(), expect);
            }
            // 抽查
            for &(v, p) in model.iter().take(4) {
                assert_eq!(pt.translate(&f.mem, v).unwrap().0, p);
            }
        }
        for (v, _) in model.clone() {
            pt.unmap(&f.mem, &mut f.frames, v).unwrap();
        }
        assert_eq!(f.frames.live(), 1, "全部拆完后只剩 PML4");
    }
}
