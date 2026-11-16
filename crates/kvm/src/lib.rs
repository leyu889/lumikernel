//! 四级页表模型（纯数据结构，不碰硬件）
//!
//! 这个 crate 只做"地址空间账本"：记录哪一段虚拟地址映射到哪一段物理地址、
//! 带什么权限位，并回答翻译与遍历的问题。它**不**安装 CR3、**不**执行 `invlpg`、
//! **不**做任何 `in`/`out`，因此可以完整地在宿主机上跑单元测试。
//!
//! 硬性约束：
//! 1. `no_std`（仅测试时可用 std），`[dependencies]` 为空，零第三方依赖。
//! 2. 公开 API 不 panic：不使用 `unwrap` / `expect` / `assert!`，失败一律返回 [`VmError`]。
//! 3. 本 crate 完全不含 `unsafe`。
//! 4. 唯一的动态内存来自 `alloc::collections::BTreeMap`（标准库自带，非第三方依赖）。
//!
//! 模型粒度是**页**：一次 `map` 会展开成 `len / PAGE_SIZE` 条页级记录，
//! 因此部分取消映射（`unmap` 只覆盖区间的一部分）天然可以表达。

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::collections::BTreeMap;

/// 页大小：4 KiB。
pub const PAGE_SIZE: usize = 4096;

/// 页内偏移掩码（低 12 位）。
const PAGE_MASK: u64 = PAGE_SIZE as u64 - 1;

/// 下半个规范地址空间的最高地址。
pub const CANONICAL_LOW_END: u64 = 0x0000_7FFF_FFFF_FFFF;

/// 上半个规范地址空间的最低地址。
pub const CANONICAL_HIGH_START: u64 = 0xFFFF_8000_0000_0000;

/// 模型容量上限（页数）。1M 页 = 4 GiB 映射，超出即 [`VmError::TooLarge`]。
pub const MAX_PAGES: usize = 1 << 20;

// x86_64 PTE 位定义（只取本模型关心的位）。
const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_USER: u64 = 1 << 2;
const PTE_PCD: u64 = 1 << 4; // page-level cache disable
const PTE_NX: u64 = 1 << 63; // no-execute，与 `executable` 相反

/// 页表项权限位（语义化表示）。
///
/// 与 x86_64 PTE 的转换见 [`Flags::bits`] / [`Flags::from_bits`]：
/// - `present`  -> bit 0
/// - `writable` -> bit 1
/// - `user`     -> bit 2
/// - `cache_disable` -> bit 4 (PCD)
/// - `executable` -> NX(bit 63) 取反
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Flags {
    pub present: bool,
    pub writable: bool,
    pub user: bool,
    /// 可执行；写进 PTE 时以 NX 位取反表示。
    pub executable: bool,
    pub cache_disable: bool,
}

impl Flags {
    /// 内核可读写、用户不可访问、不可执行。
    pub const KERNEL_RW: Flags = Flags {
        present: true,
        writable: true,
        user: false,
        executable: false,
        cache_disable: false,
    };

    /// 内核可读可执行（代码段）、不可写、用户不可访问。
    pub const KERNEL_RX: Flags = Flags {
        present: true,
        writable: false,
        user: false,
        executable: true,
        cache_disable: false,
    };

    /// 用户可读写、不可执行。
    pub const USER_RW: Flags = Flags {
        present: true,
        writable: true,
        user: true,
        executable: false,
        cache_disable: false,
    };

    /// 用户可读可执行、不可写。
    pub const USER_RX: Flags = Flags {
        present: true,
        writable: false,
        user: true,
        executable: true,
        cache_disable: false,
    };

    /// 转成 x86_64 PTE 位图（只置本模型关心的位，其余位为 0）。
    pub fn bits(&self) -> u64 {
        let mut bits = 0u64;
        if self.present {
            bits |= PTE_PRESENT;
        }
        if self.writable {
            bits |= PTE_WRITABLE;
        }
        if self.user {
            bits |= PTE_USER;
        }
        if self.cache_disable {
            bits |= PTE_PCD;
        }
        if !self.executable {
            bits |= PTE_NX;
        }
        bits
    }

    /// 从 x86_64 PTE 位图还原权限位。
    ///
    /// 未定义的位（如 PS、G、dirty、accessed、物理页基址）被忽略；
    /// NX 位为 0 即视为可执行。
    pub fn from_bits(bits: u64) -> Flags {
        Flags {
            present: bits & PTE_PRESENT != 0,
            writable: bits & PTE_WRITABLE != 0,
            user: bits & PTE_USER != 0,
            executable: bits & PTE_NX == 0,
            cache_disable: bits & PTE_PCD != 0,
        }
    }
}

impl Default for Flags {
    fn default() -> Self {
        Flags {
            present: true,
            writable: false,
            user: false,
            executable: false,
            cache_disable: false,
        }
    }
}

/// 映射操作失败的原因。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VmError {
    /// `vaddr` 或 `len` 不是页对齐（`len == 0` 视为成功，不算错）。
    NotAligned,
    /// `vaddr + len` 溢出 `u64`。
    Overflow,
    /// 区间与已有映射重叠。
    AlreadyMapped,
    /// `unmap` 的区间并非全部已映射。
    NotMapped,
    /// 非规范地址（既高于 `0x0000_7FFF_FFFF_FFFF`、又低于 `0xFFFF_8000_0000_0000`）。
    NonCanonical,
    /// 超过模型容量（见 [`MAX_PAGES`]）。
    TooLarge,
}

/// 地址是否落在合法的规范区间内。
///
/// x86_64 只有 48 位有效虚拟地址：低半区到 `0x0000_7FFF_FFFF_FFFF`，
/// 高半区从 `0xFFFF_8000_0000_0000` 起，中间是非法空洞。
pub fn is_canonical(addr: u64) -> bool {
    addr <= CANONICAL_LOW_END || addr >= CANONICAL_HIGH_START
}

/// 四级页表模型：页级映射表。
///
/// 内部用 `BTreeMap<虚拟页号, (物理页号, 权限)>` 保存，每项恰好代表一个 4 KiB 页，
/// 因此 `translate` 是 O(log n)，`mapped_bytes` 是 O(1)。
pub struct PageTableModel {
    pages: BTreeMap<u64, (u64, Flags)>,
}

impl PageTableModel {
    /// 创建空页表。
    pub fn new() -> Self {
        PageTableModel {
            pages: BTreeMap::new(),
        }
    }

    /// 建立 `[vaddr, vaddr + len)` -> `[paddr, paddr + len)` 的映射。
    ///
    /// 语义：
    /// - `len == 0` 直接成功，不产生任何记录。
    /// - `vaddr`、`len` 必须是 [`PAGE_SIZE`] 的整数倍，否则 [`VmError::NotAligned`]。
    /// - `paddr` 作为"第一页的物理基址"使用，第 i 页映射到 `paddr + i * PAGE_SIZE`；
    ///   模型不对其做对齐校验（真实页表由硬件保证，这里保持接口最小）。
    /// - 整个区间必须落在同一个规范半区，否则 [`VmError::NonCanonical`]。
    /// - 区间与任何已有页重叠即整体失败（不会部分生效），返回 [`VmError::AlreadyMapped`]。
    pub fn map(&mut self, vaddr: u64, paddr: u64, len: usize, flags: Flags) -> Result<(), VmError> {
        if len == 0 {
            return Ok(());
        }
        let (start, end) = Self::checked_range(vaddr, len)?;

        let pages = len / PAGE_SIZE;
        if pages > MAX_PAGES.saturating_sub(self.pages.len()) {
            return Err(VmError::TooLarge);
        }
        if self.range_overlaps(start, end) {
            return Err(VmError::AlreadyMapped);
        }

        let mut v = start;
        let mut p = paddr;
        while v < end {
            self.pages.insert(v, (p, flags));
            v += PAGE_SIZE as u64;
            p = p.wrapping_add(PAGE_SIZE as u64);
        }
        Ok(())
    }

    /// 取消 `[vaddr, vaddr + len)` 的映射。
    ///
    /// 只有**整段**都已映射才生效，否则返回 [`VmError::NotMapped`] 且不做任何改动。
    /// `len == 0` 视为成功。
    pub fn unmap(&mut self, vaddr: u64, len: usize) -> Result<(), VmError> {
        if len == 0 {
            return Ok(());
        }
        let (start, end) = Self::checked_range(vaddr, len)?;

        // 先全量校验，保证失败时模型零改动。
        let mut v = start;
        while v < end {
            if !self.pages.contains_key(&v) {
                return Err(VmError::NotMapped);
            }
            v += PAGE_SIZE as u64;
        }

        let mut v = start;
        while v < end {
            self.pages.remove(&v);
            v += PAGE_SIZE as u64;
        }
        Ok(())
    }

    /// 查询 `vaddr` 对应的物理地址与权限；页内偏移保留。
    ///
    /// 未映射或非规范地址返回 `None`。
    pub fn translate(&self, vaddr: u64) -> Option<(u64, Flags)> {
        if !is_canonical(vaddr) {
            return None;
        }
        let page = vaddr & !PAGE_MASK;
        let offset = vaddr - page;
        match self.pages.get(&page) {
            Some(&(paddr, flags)) => Some((paddr + offset, flags)),
            None => None,
        }
    }

    /// 返回 `vaddr` 的 PML4 / PDPT / PD / PT 索引（各 9 位，`0..=511`）。
    ///
    /// 这是纯地址算术：只要地址规范就返回索引，与是否已映射无关；
    /// 想知道"有没有映射"请用 [`PageTableModel::translate`]。
    /// 非规范地址返回 `None`（它的索引没有意义）。
    pub fn walk(&self, vaddr: u64) -> Option<[u16; 4]> {
        if !is_canonical(vaddr) {
            return None;
        }
        Some([
            ((vaddr >> 39) & 0x1FF) as u16,
            ((vaddr >> 30) & 0x1FF) as u16,
            ((vaddr >> 21) & 0x1FF) as u16,
            ((vaddr >> 12) & 0x1FF) as u16,
        ])
    }

    /// 当前已映射的总字节数（页数 × [`PAGE_SIZE`]）。
    pub fn mapped_bytes(&self) -> usize {
        self.pages.len() * PAGE_SIZE
    }

    /// 校验对齐 / 溢出 / 规范性，返回 `[start, end)`。
    ///
    /// 顺序固定：NotAligned -> Overflow -> NonCanonical，保证错误可预测。
    fn checked_range(vaddr: u64, len: usize) -> Result<(u64, u64), VmError> {
        if vaddr & PAGE_MASK != 0 {
            return Err(VmError::NotAligned);
        }
        if len & (PAGE_SIZE - 1) != 0 {
            return Err(VmError::NotAligned);
        }
        let end = match vaddr.checked_add(len as u64) {
            Some(end) => end,
            None => return Err(VmError::Overflow),
        };
        // 末字节也要规范，防止一段区间跨进非规范空洞。
        if !is_canonical(vaddr) || !is_canonical(end - 1) {
            return Err(VmError::NonCanonical);
        }
        Ok((vaddr, end))
    }

    /// `[start, end)` 是否与任一已映射页相交。
    fn range_overlaps(&self, start: u64, end: u64) -> bool {
        // 是否存在 >= start 的已映射页，且该页仍落在区间内。
        if let Some((&page, _)) = self.pages.range(start..).next() {
            if page < end {
                return true;
            }
        }
        // 是否存在 < start 的已映射页，且该页的页尾越过 start。
        if start > 0 {
            if let Some((&page, _)) = self.pages.range(..start).next_back() {
                if page + PAGE_SIZE as u64 > start {
                    return true;
                }
            }
        }
        false
    }
}

impl Default for PageTableModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap as DumbMap;

    /// 固定种子的线性同余伪随机数（不做密码学用途，够随机即可）。
    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Lcg(seed)
        }

        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next_u64() % bound as u64) as usize
        }
    }

    // 1. 空模型
    #[test]
    fn empty_model_translates_to_none() {
        let pt = PageTableModel::new();
        assert_eq!(pt.translate(0), None);
        assert_eq!(pt.translate(0xFFFF_8000_0000_0000), None);
        assert_eq!(pt.mapped_bytes(), 0);
    }

    // 2. 单页 map/translate/unmap 往返
    #[test]
    fn single_page_roundtrip() {
        let mut pt = PageTableModel::new();
        assert_eq!(pt.map(0x4000, 0x9000, PAGE_SIZE, Flags::USER_RW), Ok(()));

        assert_eq!(pt.translate(0x4000), Some((0x9000, Flags::USER_RW)));
        // 页内偏移保留
        assert_eq!(pt.translate(0x4abc), Some((0x9abc, Flags::USER_RW)));
        assert_eq!(pt.mapped_bytes(), PAGE_SIZE);

        assert_eq!(pt.unmap(0x4000, PAGE_SIZE), Ok(()));
        assert_eq!(pt.translate(0x4000), None);
        assert_eq!(pt.translate(0x4abc), None);
        assert_eq!(pt.mapped_bytes(), 0);
    }

    // 3. 跨页区间 + 页内偏移
    #[test]
    fn cross_page_range_keeps_offset() {
        let mut pt = PageTableModel::new();
        assert_eq!(pt.map(0x1000, 0x2_0000, 2 * PAGE_SIZE, Flags::KERNEL_RW), Ok(()));

        // 0x1abc 在第二页之前的区间里：页基址 0x1000，偏移 0xabc
        assert_eq!(pt.translate(0x1abc), Some((0x2_0abc, Flags::KERNEL_RW)));
        // 0x2000 是第二页：物理页 0x2_1000，偏移 0 => 0x2_1000
        assert_eq!(pt.translate(0x2000), Some((0x2_1000, Flags::KERNEL_RW)));
        assert_eq!(pt.translate(0x2fff), Some((0x2_1fff, Flags::KERNEL_RW)));
        // 区间外
        assert_eq!(pt.translate(0x3000), None);
        assert_eq!(pt.mapped_bytes(), 2 * PAGE_SIZE);
    }

    // 4. 权限位与 PTE 位图互转
    #[test]
    fn flags_bits_roundtrip() {
        for mask in 0u64..32 {
            let f = Flags {
                present: mask & 1 != 0,
                writable: mask & 2 != 0,
                user: mask & 4 != 0,
                executable: mask & 8 != 0,
                cache_disable: mask & 16 != 0,
            };
            assert_eq!(Flags::from_bits(f.bits()), f, "mask = {}", mask);
        }
        // 具体位位置
        assert_eq!(Flags::KERNEL_RX.bits(), 1);
        assert_eq!(Flags::USER_RX.bits(), 1 | 4);
        assert_eq!(Flags::KERNEL_RW.bits(), 1 | 2 | (1 << 63));
        assert_eq!(Flags::USER_RW.bits(), 1 | 2 | 4 | (1 << 63));
        // 可执行与否就是 NX 位取反
        assert_eq!(Flags::KERNEL_RX.bits() & (1 << 63), 0);
        assert_ne!(Flags::KERNEL_RW.bits() & (1 << 63), 0);
        // 未定义位被忽略
        assert_eq!(Flags::from_bits(0x0000_0000_0000_0120), Flags::from_bits(0));
        assert_eq!(Flags::from_bits(1), Flags::KERNEL_RX);
    }

    // 5. 重叠映射
    #[test]
    fn overlapping_map_is_rejected() {
        let mut pt = PageTableModel::new();
        assert_eq!(pt.map(0x1000, 0x8000, 2 * PAGE_SIZE, Flags::KERNEL_RW), Ok(()));

        // 完全重叠
        assert_eq!(
            pt.map(0x1000, 0x9000, PAGE_SIZE, Flags::USER_RW),
            Err(VmError::AlreadyMapped)
        );
        // 前向部分重叠（区间头落在已有页内）
        assert_eq!(
            pt.map(0x2000, 0x9000, 2 * PAGE_SIZE, Flags::USER_RW),
            Err(VmError::AlreadyMapped)
        );
        // 后向部分重叠（区间尾压在已有页上）
        assert_eq!(
            pt.map(0x0, 0x9000, 2 * PAGE_SIZE, Flags::USER_RW),
            Err(VmError::AlreadyMapped)
        );
        // 完全包含
        assert_eq!(
            pt.map(0x0, 0x9000, 4 * PAGE_SIZE, Flags::USER_RW),
            Err(VmError::AlreadyMapped)
        );
        // 失败不得留下痕迹
        assert_eq!(pt.translate(0x0), None);
        assert_eq!(pt.translate(0x3000), None);
        assert_eq!(pt.mapped_bytes(), 2 * PAGE_SIZE);
        // 紧邻（不重叠）可以成功
        assert_eq!(pt.map(0x3000, 0x9000, PAGE_SIZE, Flags::USER_RW), Ok(()));
    }

    // 6. 未对齐
    #[test]
    fn unaligned_is_rejected() {
        let mut pt = PageTableModel::new();
        assert_eq!(
            pt.map(0x1001, 0x8000, PAGE_SIZE, Flags::KERNEL_RW),
            Err(VmError::NotAligned)
        );
        assert_eq!(
            pt.map(0x1000, 0x8000, PAGE_SIZE - 1, Flags::KERNEL_RW),
            Err(VmError::NotAligned)
        );
        assert_eq!(
            pt.map(0x1000, 0x8000, PAGE_SIZE + 1, Flags::KERNEL_RW),
            Err(VmError::NotAligned)
        );
        assert_eq!(pt.unmap(0x1001, PAGE_SIZE), Err(VmError::NotAligned));
        assert_eq!(pt.unmap(0x1000, 3), Err(VmError::NotAligned));
        assert_eq!(pt.mapped_bytes(), 0);
    }

    // 7. unmap 未映射区间 -> NotMapped，且不做任何改动
    #[test]
    fn unmap_partially_mapped_fails_without_changes() {
        let mut pt = PageTableModel::new();
        assert_eq!(pt.map(0x1000, 0x8000, PAGE_SIZE, Flags::KERNEL_RW), Ok(()));
        let before = pt.mapped_bytes();

        assert_eq!(
            pt.unmap(0x1000, 3 * PAGE_SIZE),
            Err(VmError::NotMapped)
        );
        assert_eq!(pt.translate(0x1000), Some((0x8000, Flags::KERNEL_RW)));
        assert_eq!(pt.mapped_bytes(), before);

        // 中间开洞的情况：两页已映射，取消三页必须整体失败
        assert_eq!(pt.map(0x3000, 0xa000, PAGE_SIZE, Flags::KERNEL_RW), Ok(()));
        let before = pt.mapped_bytes();
        assert_eq!(pt.unmap(0x2000, 2 * PAGE_SIZE), Err(VmError::NotMapped));
        assert_eq!(pt.mapped_bytes(), before);
        assert_eq!(pt.translate(0x3000), Some((0xa000, Flags::KERNEL_RW)));
    }

    // 8. 非规范地址
    #[test]
    fn non_canonical_addresses_are_rejected() {
        let mut pt = PageTableModel::new();
        // 空洞的起点与终点
        assert_eq!(
            pt.map(0x0000_8000_0000_0000, 0x1000, PAGE_SIZE, Flags::KERNEL_RW),
            Err(VmError::NonCanonical)
        );
        assert_eq!(
            pt.map(0xFFFF_7FFF_FFFF_F000, 0x1000, PAGE_SIZE, Flags::KERNEL_RW),
            Err(VmError::NonCanonical)
        );
        // 低半区最后一页、高半区第一页是合法的
        assert_eq!(
            pt.map(0x0000_7FFF_FFFF_F000, 0x1000, PAGE_SIZE, Flags::KERNEL_RW),
            Ok(())
        );
        assert_eq!(
            pt.map(0xFFFF_8000_0000_0000, 0x2000, PAGE_SIZE, Flags::KERNEL_RW),
            Ok(())
        );
        assert_eq!(pt.translate(0x0000_8000_0000_0000), None);
        assert_eq!(pt.unmap(0x0000_8000_0000_0000, PAGE_SIZE), Err(VmError::NonCanonical));
        assert_eq!(pt.walk(0x0000_8000_0000_0000), None);
    }

    // 9. 加法溢出
    #[test]
    fn address_plus_len_overflow() {
        let mut pt = PageTableModel::new();
        // 最后一页的页基址：vaddr + len 必定溢出 u64（end 是开区间上界）
        let top = u64::MAX - (PAGE_SIZE as u64 - 1); // 0xFFFF_FFFF_FFFF_F000
        assert_eq!(pt.map(top, 0x1000, PAGE_SIZE, Flags::KERNEL_RW), Err(VmError::Overflow));
        assert_eq!(pt.map(top, 0x1000, 2 * PAGE_SIZE, Flags::KERNEL_RW), Err(VmError::Overflow));
        assert_eq!(pt.unmap(top, PAGE_SIZE), Err(VmError::Overflow));
        // 倒数第二页可以正常映射，页尾正好是 u64::MAX
        let last_mappable = top - PAGE_SIZE as u64;
        assert_eq!(pt.map(last_mappable, 0x1000, PAGE_SIZE, Flags::KERNEL_RW), Ok(()));
        assert_eq!(pt.translate(last_mappable), Some((0x1000, Flags::KERNEL_RW)));
        assert_eq!(pt.translate(last_mappable + 0xfff), Some((0x1fff, Flags::KERNEL_RW)));
        // 最高页（0xFFFF_FFFF_FFFF_F000）本身无法映射：它的 end 会溢出
        assert_eq!(pt.translate(u64::MAX), None);
        // 非对齐的溢出场景优先报 NotAligned（错误顺序固定）
        assert_eq!(pt.map(u64::MAX, 0x1000, PAGE_SIZE, Flags::KERNEL_RW), Err(VmError::NotAligned));
    }

    // 10. 容量上限
    #[test]
    fn capacity_limit() {
        let mut pt = PageTableModel::new();
        let len = (MAX_PAGES + 1) * PAGE_SIZE;
        assert_eq!(pt.map(0x1000, 0, len, Flags::KERNEL_RW), Err(VmError::TooLarge));
        assert_eq!(pt.mapped_bytes(), 0);

        // 分块填满到上限，然后再多一页就 TooLarge
        for i in 0..(MAX_PAGES / 2) {
            let v = (i as u64 + 1) * PAGE_SIZE as u64;
            assert_eq!(pt.map(v, v, PAGE_SIZE, Flags::KERNEL_RW), Ok(()));
        }
        for i in (MAX_PAGES / 2)..MAX_PAGES {
            let v = (i as u64 + 1) * PAGE_SIZE as u64;
            assert_eq!(pt.map(v, v, PAGE_SIZE, Flags::KERNEL_RW), Ok(()));
        }
        assert_eq!(pt.mapped_bytes(), MAX_PAGES * PAGE_SIZE);
        let v = (MAX_PAGES as u64 + 1) * PAGE_SIZE as u64;
        assert_eq!(pt.map(v, v, PAGE_SIZE, Flags::KERNEL_RW), Err(VmError::TooLarge));
    }

    // 11. 零长度
    #[test]
    fn zero_length_is_noop() {
        let mut pt = PageTableModel::new();
        assert_eq!(pt.map(0x1234, 0x5678, 0, Flags::KERNEL_RW), Ok(()));
        assert_eq!(pt.unmap(0x1234, 0), Ok(()));
        assert_eq!(pt.mapped_bytes(), 0);
        assert_eq!(pt.translate(0x1234), None);
        // 零长度即使地址非规范也算成功（没有区间需要校验）
        assert_eq!(pt.map(0x0000_8000_0000_0000, 0, 0, Flags::KERNEL_RW), Ok(()));
    }

    // 12. walk 的四级索引
    #[test]
    fn walk_indices_match_hand_computation() {
        let pt = PageTableModel::new();

        // 0x0000_7FFF_FFFF_F000：下半个规范空间最后一页，四级索引都是最大值
        assert_eq!(pt.walk(0x0000_7FFF_FFFF_F000), Some([0xFF, 0x1FF, 0x1FF, 0x1FF]));

        // 0：全 0
        assert_eq!(pt.walk(0), Some([0, 0, 0, 0]));

        // 0xFFFF_8000_0000_0000：内核空间第一页
        // >>39 = 0x100（0xFFFF * 512 + 256，再取低 9 位）, >>30 = 0, >>21 = 0, >>12 = 0
        assert_eq!(pt.walk(0xFFFF_8000_0000_0000), Some([0x100, 0, 0, 0]));

        // 逐级单独置位：0x0000_0080_0000_0000 (bit39) / 0x4000_0000 (bit30)
        // / 0x20_0000 (bit21) / 0x1000 (bit12)
        assert_eq!(pt.walk(0x0000_0080_0000_0000), Some([1, 0, 0, 0]));
        assert_eq!(pt.walk(0x0000_0000_4000_0000), Some([0, 1, 0, 0]));
        assert_eq!(pt.walk(0x0000_0000_0020_0000), Some([0, 0, 1, 0]));
        assert_eq!(pt.walk(0x0000_0000_0000_1000), Some([0, 0, 0, 1]));

        // 页内偏移不影响索引
        assert_eq!(pt.walk(0x0000_0000_0000_1abc), pt.walk(0x0000_0000_0000_1000));

        // 与"手工移位"逐位对照
        for shift_pair in [(39u32, 0usize), (30, 1), (21, 2), (12, 3)] {
            let v = 0x0000_1234_5678_9000u64;
            let expected = ((v >> shift_pair.0) & 0x1FF) as u16;
            assert_eq!(pt.walk(v).map(|a| a[shift_pair.1]), Some(expected));
        }
    }

    // 13. mapped_bytes 随 map/unmap 变化
    #[test]
    fn mapped_bytes_accounting() {
        let mut pt = PageTableModel::new();
        assert_eq!(pt.mapped_bytes(), 0);

        assert_eq!(pt.map(0x10_0000, 0x20_0000, 4 * PAGE_SIZE, Flags::KERNEL_RW), Ok(()));
        assert_eq!(pt.mapped_bytes(), 4 * PAGE_SIZE);

        assert_eq!(pt.map(0x20_0000, 0x30_0000, PAGE_SIZE, Flags::USER_RX), Ok(()));
        assert_eq!(pt.mapped_bytes(), 5 * PAGE_SIZE);

        // 失败的操作不改变计数
        assert_eq!(
            pt.map(0x10_0000, 0, PAGE_SIZE, Flags::KERNEL_RW),
            Err(VmError::AlreadyMapped)
        );
        assert_eq!(pt.mapped_bytes(), 5 * PAGE_SIZE);

        assert_eq!(pt.unmap(0x10_0000, 2 * PAGE_SIZE), Ok(()));
        assert_eq!(pt.mapped_bytes(), 3 * PAGE_SIZE);
        // 前两页已经没了，再取消同一段必须失败
        assert_eq!(pt.unmap(0x10_0000, 2 * PAGE_SIZE), Err(VmError::NotMapped));
        assert_eq!(pt.mapped_bytes(), 3 * PAGE_SIZE);
        assert_eq!(pt.unmap(0x10_2000, 2 * PAGE_SIZE), Ok(()));
        assert_eq!(pt.mapped_bytes(), PAGE_SIZE);
    }

    // 14. unmap 后再 map 同一页（权限也可以换）
    #[test]
    fn remap_after_unmap_with_new_flags() {
        let mut pt = PageTableModel::new();
        assert_eq!(pt.map(0x5000, 0x9000, PAGE_SIZE, Flags::USER_RX), Ok(()));
        assert_eq!(pt.unmap(0x5000, PAGE_SIZE), Ok(()));
        assert_eq!(pt.translate(0x5000), None);
        assert_eq!(pt.map(0x5000, 0x7000, PAGE_SIZE, Flags::KERNEL_RW), Ok(()));
        assert_eq!(pt.translate(0x5000), Some((0x7000, Flags::KERNEL_RW)));
        assert_eq!(pt.mapped_bytes(), PAGE_SIZE);
    }

    // 15. 随机操作对拍"笨模型"（每页一条 BTreeMap 记录）
    #[test]
    fn random_operations_match_dumb_model() {
        const BASE: u64 = 0x10_0000;
        const PAGES: usize = 256;

        let mut pt = PageTableModel::new();
        let mut dumb: DumbMap<u64, (u64, Flags)> = DumbMap::new();
        let mut rng = Lcg::new(0x1234_5678_9ABC_DEF0);

        for round in 0..3000 {
            let op = rng.below(10);
            let pages = 1 + rng.below(4);
            let start_page = rng.below(PAGES - pages + 1);
            let vaddr = BASE + start_page as u64 * PAGE_SIZE as u64;
            let len = pages * PAGE_SIZE;
            let paddr = 0x1000_0000 + rng.below(PAGES) as u64 * PAGE_SIZE as u64;
            let flags = if rng.next_u64() & 1 == 0 {
                Flags::USER_RW
            } else {
                Flags::USER_RX
            };

            if op < 6 {
                // map：期望"有任何一页已存在"就 AlreadyMapped
                let mut overlap = false;
                for i in 0..pages {
                    if dumb.contains_key(&(vaddr + i as u64 * PAGE_SIZE as u64)) {
                        overlap = true;
                    }
                }
                let res = pt.map(vaddr, paddr, len, flags);
                if overlap {
                    assert_eq!(res, Err(VmError::AlreadyMapped), "round {} map overlap", round);
                } else {
                    assert_eq!(res, Ok(()), "round {} map", round);
                    for i in 0..pages {
                        dumb.insert(
                            vaddr + i as u64 * PAGE_SIZE as u64,
                            (paddr + i as u64 * PAGE_SIZE as u64, flags),
                        );
                    }
                }
            } else {
                // unmap：期望"全部已映射"才成功
                let mut all = true;
                for i in 0..pages {
                    if !dumb.contains_key(&(vaddr + i as u64 * PAGE_SIZE as u64)) {
                        all = false;
                    }
                }
                let res = pt.unmap(vaddr, len);
                if all {
                    assert_eq!(res, Ok(()), "round {} unmap", round);
                    for i in 0..pages {
                        dumb.remove(&(vaddr + i as u64 * PAGE_SIZE as u64));
                    }
                } else {
                    assert_eq!(res, Err(VmError::NotMapped), "round {} unmap", round);
                }
            }

            // 全量对拍：每一页的基址与带偏移的地址
            for i in 0..PAGES {
                let v = BASE + i as u64 * PAGE_SIZE as u64;
                let expected = dumb.get(&v).map(|&(p, f)| (p, f));
                assert_eq!(pt.translate(v), expected, "round {} page {:#x}", round, v);

                let expected_off = dumb.get(&v).map(|&(p, f)| (p + 0x1ab, f));
                assert_eq!(pt.translate(v + 0x1ab), expected_off, "round {} page {:#x}+off", round, v);
            }
            assert_eq!(pt.mapped_bytes(), dumb.len() * PAGE_SIZE, "round {}", round);
        }

        // 收尾：逐页清空（随机过程中可能留下空洞，不能整段 unmap）
        for i in 0..PAGES {
            let v = BASE + i as u64 * PAGE_SIZE as u64;
            if pt.translate(v).is_some() {
                assert_eq!(pt.unmap(v, PAGE_SIZE), Ok(()));
            }
        }
        assert_eq!(pt.mapped_bytes(), 0);
        for i in 0..PAGES {
            let v = BASE + i as u64 * PAGE_SIZE as u64;
            assert_eq!(pt.translate(v), None);
        }
    }

    // 16. 高位（内核半区）地址同样可用
    #[test]
    fn high_half_mapping_works() {
        let mut pt = PageTableModel::new();
        let vaddr = 0xFFFF_8000_0000_0000u64 + 0x40_0000;
        assert_eq!(pt.map(vaddr, 0x8000_0000, PAGE_SIZE, Flags::KERNEL_RX), Ok(()));
        assert_eq!(
            pt.translate(vaddr + 0x321),
            Some((0x8000_0321, Flags::KERNEL_RX))
        );
        assert_eq!(pt.walk(vaddr), Some([0x100, 0, 2, 0]));
        assert_eq!(pt.unmap(vaddr, PAGE_SIZE), Ok(()));
        assert_eq!(pt.mapped_bytes(), 0);
    }
}
