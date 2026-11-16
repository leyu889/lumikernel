//! kelf —— ELF64 可执行文件解析（纯逻辑）
//!
//! 内核在 exec 路径上拿到的是一段"不可信"的文件字节：镜像可能被截断、
//! 头字段可能是垃圾、段表可能指向文件外。这段代码必须做到以下几点：
//!
//! - **只看字节切片**：不解引用任何指针，不碰页表，不分配内存（`no_std` + 零依赖）；
//! - **绝不 panic**：所有读字段都走 `get()`，越界只会变成 [`ElfError`]，
//!   因为在这个位置 panic 等于用一个用户可控的镜像把内核打死；
//! - **明确拒绝**：PIE（`ET_DYN`）与动态链接（`PT_INTERP`）当前不支持，
//!   与其半懂不懂地装进去跑飞，不如在解析阶段就返回 [`ElfError::Pie`] /
//!   [`ElfError::NeedsInterp`]，让调用方给出干净的报错。
//!
//! 解析结果 [`ElfImage`] 是 `Copy` 的定长结构（最多 [`MAX_SEGMENTS`] 个段），
//! 因此可以直接放在栈上或塞进进程控制块，生命周期上不依赖输入缓冲区。
//! 注意它**不持有**文件字节，调用方需要保留原缓冲区，
//! 靠 [`ElfImage::segment_data`] 把段内容拷进用户地址空间。

#![cfg_attr(not(test), no_std)]
// 纯字节运算，本 crate 不需要任何 unsafe；显式禁掉以免后续被随手加入。
#![forbid(unsafe_code)]

/* ---------------- 常量 ---------------- */

/// 单次装载最多接受的 `PT_LOAD` 段数量。
///
/// 定长数组而非 `Vec`，是为了让 [`ElfImage`] 保持 `Copy`、可放在栈上。
pub const MAX_SEGMENTS: usize = 8;

/// ELF64 文件头固定长度（字节）。
pub const EHDR_SIZE: usize = 64;

/// ELF64 程序头表项固定长度（字节），即 `e_phentsize` 的合法值。
pub const PHDR_SIZE: usize = 56;

/// `p_type`：可装载段。
pub const PT_LOAD: u32 = 1;

/// `p_type`：动态链接器路径（出现即说明是动态可执行文件）。
pub const PT_INTERP: u32 = 3;

/// 可执行镜像虚拟地址跨度的上限（512 MiB）。
///
/// `ElfImage::parse` 没有 limit 参数，这里给出一个固定的保守上限：
/// 超过它的镜像在 64 位地址空间里也基本是恶意构造，直接 [`ElfError::TooLarge`]。
pub const MAX_IMAGE_BYTES: u64 = 512 * 1024 * 1024;

/* ---------------- 错误类型 ---------------- */

/// 解析失败的**全部**可能原因。
///
/// 刻意做成 `Copy` 的扁平枚举而不是带字符串的错误：
/// 内核里打印一个枚举判别值就够了，且不涉及任何分配。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    /// 连一个完整的 ELF64 文件头（64 字节）都不够。
    TooSmall,
    /// 偏移 0 不是 `\x7fELF`。
    BadMagic,
    /// `EI_CLASS != 2`（不是 64 位 ELF）。
    Not64Bit,
    /// `EI_DATA != 1`（不是小端）。
    NotLittleEndian,
    /// `e_machine != 62`（不是 x86-64）。
    NotX86_64,
    /// `e_type` 既不是 `ET_EXEC` 也不是 `ET_DYN`。
    NotExecutable,
    /// `e_type == ET_DYN`：位置无关可执行文件，内核暂不支持，明确拒绝。
    Pie,
    /// 存在 `PT_INTERP`：需要动态链接器，内核暂不支持，明确拒绝。
    NeedsInterp,
    /// 头部/程序头表本身不合法：`e_phentsize != 56`、`e_phnum == 0`、
    /// 段数超过 [`MAX_SEGMENTS`]，或程序头表整体越出文件。
    BadHeader,
    /// 段内容不合法：`p_offset + p_filesz` 越界、`p_filesz > p_memsz`，
    /// 或一个 `PT_LOAD` 都没有。
    BadSegment,
    /// 段跨度超过 [`MAX_IMAGE_BYTES`]，或 `p_vaddr + p_memsz` 溢出。
    TooLarge,
}

/* ---------------- 段 ---------------- */

/// 一个已通过校验的 `PT_LOAD` 段（只保留装载需要的信息）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// 段的虚拟起始地址。
    pub vaddr: u64,
    /// 段在内存中的长度（含 `.bss` 部分，`memsz` 可以大于 `filesz`）。
    pub memsz: u64,
    /// 段在文件中的长度。
    pub filesz: u64,
    /// 段内容在文件中的偏移。
    pub offset: u64,
    /// `p_flags`：1 = X，2 = W，4 = R。
    pub flags: u32,
}

impl Segment {
    /// 是否可执行（`PF_X`）。
    pub fn is_exec(&self) -> bool {
        self.flags & 1 != 0
    }

    /// 是否可写（`PF_W`）。
    pub fn is_write(&self) -> bool {
        self.flags & 2 != 0
    }

    /// 段的虚拟结束地址（不含）；`vaddr + memsz` 溢出时返回 `None`。
    pub fn end_vaddr(&self) -> Option<u64> {
        self.vaddr.checked_add(self.memsz)
    }
}

/* ---------------- 镜像 ---------------- */

/// 解析后的可执行镜像：入口点 + 至多 [`MAX_SEGMENTS`] 个 `PT_LOAD` 段。
///
/// 内部使用定长数组 + 有效计数，多余槽位恒为零段；对外只暴露有效切片。
#[derive(Clone, Copy, Debug)]
pub struct ElfImage {
    /// `e_entry`。
    entry: u64,
    /// 段数组（前 `count` 项有效）。
    segs: [Segment; MAX_SEGMENTS],
    /// 有效段数量，恒 `>= 1`。
    count: usize,
    /// 所有段 `vaddr` 的最小值。
    min_vaddr: u64,
    /// 所有段 `end_vaddr` 的最大值。
    max_vaddr: u64,
    /// 程序头表在文件里的偏移（`e_phoff`）。构造 `AT_PHDR` 要用。
    phoff: u64,
    /// 程序头表项数（`e_phnum`）。
    phnum: usize,
}

impl ElfImage {
    /// 只解析文件头与程序头表，拷贝出最多 [`MAX_SEGMENTS`] 个 `PT_LOAD` 段。
    ///
    /// 不读取段内容，只做边界校验；段内容留给 [`ElfImage::segment_data`]。
    /// 任何越界/畸形输入都返回错误，函数本身不会 panic。
    pub fn parse(bytes: &[u8]) -> Result<Self, ElfError> {
        // ---- 1. 文件头基础字段 ----
        if bytes.len() < EHDR_SIZE {
            return Err(ElfError::TooSmall);
        }
        match bytes.get(0..4) {
            Some([0x7f, b'E', b'L', b'F']) => {}
            _ => return Err(ElfError::BadMagic),
        }
        match bytes.get(4).copied() {
            Some(2) => {}
            _ => return Err(ElfError::Not64Bit),
        }
        match bytes.get(5).copied() {
            Some(1) => {}
            _ => return Err(ElfError::NotLittleEndian),
        }

        // 走到这里说明前 64 字节都在，下面 rdXX 理论上不会失败；
        // 仍然统一按 BadHeader 处理，避免任何隐式 panic。
        let e_type = rd16(bytes, 16).ok_or(ElfError::BadHeader)?;
        match e_type {
            2 => {}                    // ET_EXEC
            3 => return Err(ElfError::Pie), // ET_DYN：暂不支持
            _ => return Err(ElfError::NotExecutable),
        }
        let e_machine = rd16(bytes, 18).ok_or(ElfError::BadHeader)?;
        if e_machine != 62 {
            return Err(ElfError::NotX86_64);
        }

        let entry = rd64(bytes, 24).ok_or(ElfError::BadHeader)?;
        let phoff = rd64(bytes, 32).ok_or(ElfError::BadHeader)?;
        let phentsize = rd16(bytes, 54).ok_or(ElfError::BadHeader)?;
        let phnum = rd16(bytes, 56).ok_or(ElfError::BadHeader)? as usize;

        // ---- 2. 程序头表自身的合法性 ----
        if phentsize != PHDR_SIZE as u16 {
            return Err(ElfError::BadHeader);
        }
        if phnum == 0 || phnum as usize > MAX_SEGMENTS {
            return Err(ElfError::BadHeader);
        }
        let phnum = phnum as usize;
        // 整张表必须在文件内，否则后面按表项读字段必然越界。
        let table_end = phoff
            .checked_add((phnum as u64) * (PHDR_SIZE as u64))
            .ok_or(ElfError::BadHeader)?;
        if table_end > bytes.len() as u64 {
            return Err(ElfError::BadHeader);
        }

        // ---- 3. 逐项扫描程序头 ----
        let mut segs = [Segment {
            vaddr: 0,
            memsz: 0,
            filesz: 0,
            offset: 0,
            flags: 0,
        }; MAX_SEGMENTS];
        let mut count = 0usize;
        let mut min_vaddr = u64::MAX;
        let mut max_vaddr = 0u64;

        let mut i = 0usize;
        while i < phnum {
            let base = (phoff as usize) + i * PHDR_SIZE;
            let p_type = rd32(bytes, base).ok_or(ElfError::BadHeader)?;

            if p_type == PT_INTERP {
                // 动态链接的可执行文件，内核目前没有加载 .so 的能力。
                return Err(ElfError::NeedsInterp);
            }

            if p_type == PT_LOAD {
                let p_flags = rd32(bytes, base + 4).ok_or(ElfError::BadHeader)?;
                let p_offset = rd64(bytes, base + 8).ok_or(ElfError::BadHeader)?;
                let p_vaddr = rd64(bytes, base + 16).ok_or(ElfError::BadHeader)?;
                let p_filesz = rd64(bytes, base + 32).ok_or(ElfError::BadHeader)?;
                let p_memsz = rd64(bytes, base + 40).ok_or(ElfError::BadHeader)?;

                // 内存像必须能装下文件中那段（否则是恶意构造）。
                if p_filesz > p_memsz {
                    return Err(ElfError::BadSegment);
                }
                // 文件视角的边界：p_offset + p_filesz 必须落在切片内。
                // filesz == 0 时（纯 .bss 段）不检查 offset，按规范允许任意值。
                if p_filesz != 0 {
                    let file_end = p_offset
                        .checked_add(p_filesz)
                        .ok_or(ElfError::BadSegment)?;
                    if file_end > bytes.len() as u64 {
                        return Err(ElfError::BadSegment);
                    }
                }
                // 虚拟地址视角：溢出说明这根本不是个能给用户用的镜像。
                let end_v = p_vaddr.checked_add(p_memsz).ok_or(ElfError::TooLarge)?;

                // phnum <= MAX_SEGMENTS 已保证 slot 存在；仍然用 get_mut 兜底。
                if let Some(slot) = segs.get_mut(count) {
                    *slot = Segment {
                        vaddr: p_vaddr,
                        memsz: p_memsz,
                        filesz: p_filesz,
                        offset: p_offset,
                        flags: p_flags,
                    };
                    count += 1;
                    if p_vaddr < min_vaddr {
                        min_vaddr = p_vaddr;
                    }
                    if end_v > max_vaddr {
                        max_vaddr = end_v;
                    }
                }
            }

            i += 1;
        }

        // ---- 4. 必须至少有一个可装载段 ----
        // 只有 PT_NOTE / PT_GNU_STACK 之类的文件"合法"但装不进内存。
        if count == 0 {
            return Err(ElfError::BadSegment);
        }
        if max_vaddr - min_vaddr > MAX_IMAGE_BYTES {
            return Err(ElfError::TooLarge);
        }

        Ok(Self {
            entry,
            segs,
            count,
            min_vaddr,
            max_vaddr,
            phoff,
            phnum,
        })
    }

    /// 程序入口虚拟地址（`e_entry`，未做任何重定位）。
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// 程序头表在**文件**里的偏移（`e_phoff`）。
    pub fn phoff(&self) -> u64 {
        self.phoff
    }

    /// 程序头表项数（`e_phnum`）。
    pub fn phnum(&self) -> usize {
        self.phnum
    }

    /// `AT_PHDR`：程序头表被装载到的**虚拟地址**。
    ///
    /// 对 `ET_EXEC` 来说，程序头表几乎总是落在第一个可装载段的文件起点，
    /// 所以等于 `min_vaddr + (e_phoff - 首个段的 p_offset)`。
    /// 这里用"第一个 `p_offset == 0` 的段"来对齐——没有的话返回 `None`，
    /// 调用方就不填 `AT_PHDR`（静态 musl 程序并不强制要求它）。
    pub fn phdr_vaddr(&self) -> Option<u64> {
        for seg in self.segments() {
            if seg.offset == 0 && self.phoff < seg.filesz {
                return Some(seg.vaddr + self.phoff);
            }
        }
        None
    }

    /// 有效的 `PT_LOAD` 段切片（按程序头表顺序，至少一个）。
    pub fn segments(&self) -> &[Segment] {
        match self.segs.get(..self.count) {
            Some(s) => s,
            None => &[],
        }
    }

    /// 所有段中最小的虚拟地址（装载基点）。
    pub fn min_vaddr(&self) -> u64 {
        self.min_vaddr
    }

    /// 所有段 `end_vaddr` 的最大值（即镜像末尾，不含）。
    pub fn max_vaddr(&self) -> u64 {
        self.max_vaddr
    }

    /// 取第 `idx` 个段在文件里的原始字节（用于拷贝到用户内存）。
    ///
    /// `idx` 越界、或段范围越出 `bytes` 时返回 `None`；
    /// 返回长度等于该段的 `filesz`（`memsz` 超出部分需调用方自己清零）。
    pub fn segment_data<'a>(&self, bytes: &'a [u8], idx: usize) -> Option<&'a [u8]> {
        if idx >= self.count {
            return None;
        }
        let seg = self.segs.get(idx)?;
        let start = usize::try_from(seg.offset).ok()?;
        let end = usize::try_from(seg.offset.checked_add(seg.filesz)?).ok()?;
        bytes.get(start..end)
    }
}

/* ---------------- 小端读数（全部返回 Option，绝不 panic） ---------------- */

/// 读 `off` 处的 u16（小端）；越界返回 `None`。
#[inline]
fn rd16(b: &[u8], off: usize) -> Option<u16> {
    match (b.get(off).copied(), b.get(off + 1).copied()) {
        (Some(a), Some(c)) => Some(u16::from_le_bytes([a, c])),
        _ => None,
    }
}

/// 读 `off` 处的 u32（小端）；越界返回 `None`。
#[inline]
fn rd32(b: &[u8], off: usize) -> Option<u32> {
    match (
        b.get(off).copied(),
        b.get(off + 1).copied(),
        b.get(off + 2).copied(),
        b.get(off + 3).copied(),
    ) {
        (Some(a), Some(c), Some(d), Some(e)) => Some(u32::from_le_bytes([a, c, d, e])),
        _ => None,
    }
}

/// 读 `off` 处的 u64（小端）；越界返回 `None`。
#[inline]
fn rd64(b: &[u8], off: usize) -> Option<u64> {
    match (
        b.get(off).copied(),
        b.get(off + 1).copied(),
        b.get(off + 2).copied(),
        b.get(off + 3).copied(),
        b.get(off + 4).copied(),
        b.get(off + 5).copied(),
        b.get(off + 6).copied(),
        b.get(off + 7).copied(),
    ) {
        (Some(a), Some(c), Some(d), Some(e), Some(f), Some(g), Some(h), Some(k)) => {
            Some(u64::from_le_bytes([a, c, d, e, f, g, h, k]))
        }
        _ => None,
    }
}

/* ================= 单元测试 ================= */

#[cfg(test)]
mod tests {
    use super::*;

    /* ---- 构造工具 ---- */

    /// 测试用程序头描述（只填解析关心的字段）。
    struct Ph {
        ptype: u32,
        flags: u32,
        offset: u64,
        vaddr: u64,
        filesz: u64,
        memsz: u64,
    }

    fn put16(b: &mut [u8], off: usize, v: u16) {
        let x = v.to_le_bytes();
        b[off] = x[0];
        b[off + 1] = x[1];
    }

    fn put32(b: &mut [u8], off: usize, v: u32) {
        let x = v.to_le_bytes();
        let mut i = 0;
        while i < 4 {
            b[off + i] = x[i];
            i += 1;
        }
    }

    fn put64(b: &mut [u8], off: usize, v: u64) {
        let x = v.to_le_bytes();
        let mut i = 0;
        while i < 8 {
            b[off + i] = x[i];
            i += 1;
        }
    }

    /// 文件正文长度：0x300 —— 头部 0..0x40，程序头表 0x40..0x40+56n，
    /// 数据区放在 0x100 之后。
    const FILE_LEN: usize = 0x300;

    /// 拼一个合法的 ET_EXEC / x86-64 / 小端 ELF64。
    ///
    /// 整个缓冲区先按字节下标填 `(i & 0xff)` 作为花纹，
    /// 这样 `segment_data` 返回的字节可以逐字节核对是否为"文件里那一块"。
    fn build(phdrs: &[Ph]) -> Vec<u8> {
        let mut b = vec![0u8; FILE_LEN];
        let mut i = 0;
        while i < b.len() {
            b[i] = (i & 0xff) as u8;
            i += 1;
        }

        // e_ident
        b[0] = 0x7f;
        b[1] = b'E';
        b[2] = b'L';
        b[3] = b'F';
        b[4] = 2; // ELFCLASS64
        b[5] = 1; // ELFDATA2LSB
        b[6] = 1; // EV_CURRENT
        // 头字段
        put16(&mut b, 16, 2); // ET_EXEC
        put16(&mut b, 18, 62); // EM_X86_64
        put32(&mut b, 20, 1); // e_version
        put64(&mut b, 24, 0x40_1000); // e_entry
        put64(&mut b, 32, 64); // e_phoff
        put16(&mut b, 52, 64); // e_ehsize
        put16(&mut b, 54, 56); // e_phentsize
        put16(&mut b, 56, phdrs.len() as u16);
        put16(&mut b, 58, 56); // e_shentsize
        put16(&mut b, 60, 0); // e_shnum

        let mut k = 0;
        while k < phdrs.len() {
            let p = &phdrs[k];
            let base = 64 + k * 56;
            put32(&mut b, base, p.ptype);
            put32(&mut b, base + 4, p.flags);
            put64(&mut b, base + 8, p.offset);
            put64(&mut b, base + 16, p.vaddr);
            put64(&mut b, base + 24, p.vaddr); // p_paddr
            put64(&mut b, base + 32, p.filesz);
            put64(&mut b, base + 40, p.memsz);
            put64(&mut b, base + 48, 0x1000); // p_align
            k += 1;
        }
        b
    }

    /// 标准双段镜像：代码段（R|X）+ 数据段（R|W）。
    fn two_loads() -> Vec<u8> {
        build(&[
            Ph {
                ptype: PT_LOAD,
                flags: 5, // R|X
                offset: 0x100,
                vaddr: 0x40_0000,
                filesz: 0x80,
                memsz: 0xa0,
            },
            Ph {
                ptype: PT_LOAD,
                flags: 6, // R|W
                offset: 0x200,
                vaddr: 0x40_1000,
                filesz: 0x40,
                memsz: 0x100,
            },
        ])
    }

    /* ---- 1. 合法双段镜像 ---- */

    #[test]
    fn parse_two_loads_ok() {
        let img = ElfImage::parse(&two_loads()).expect("合法镜像应当解析成功");
        assert_eq!(img.entry(), 0x40_1000);
        let segs = img.segments();
        assert_eq!(segs.len(), 2);

        assert_eq!(segs[0].vaddr, 0x40_0000);
        assert_eq!(segs[0].offset, 0x100);
        assert_eq!(segs[0].filesz, 0x80);
        assert_eq!(segs[0].memsz, 0xa0);
        assert_eq!(segs[0].flags, 5);

        assert_eq!(segs[1].vaddr, 0x40_1000);
        assert_eq!(segs[1].offset, 0x200);
        assert_eq!(segs[1].filesz, 0x40);
        assert_eq!(segs[1].memsz, 0x100);
        assert_eq!(segs[1].flags, 6);
    }

    /* ---- 2. min/max vaddr ---- */

    #[test]
    fn min_and_max_vaddr() {
        let img = ElfImage::parse(&two_loads()).expect("解析失败");
        assert_eq!(img.min_vaddr(), 0x40_0000);
        // max(0x400000+0xa0, 0x401000+0x100) = 0x401100
        assert_eq!(img.max_vaddr(), 0x40_1100);
    }

    /* ---- 3. segment_data ---- */

    #[test]
    fn segment_data_matches_file_bytes() {
        let file = two_loads();
        let img = ElfImage::parse(&file).expect("解析失败");

        let d0 = img.segment_data(&file, 0).expect("段 0 应可取到");
        assert_eq!(d0.len(), 0x80);
        // 文件里 0x100..0x180 的花纹是 (i & 0xff)
        let mut k = 0;
        while k < d0.len() {
            assert_eq!(d0[k], ((0x100 + k) & 0xff) as u8);
            k += 1;
        }
        assert_eq!(d0, &file[0x100..0x180]);

        let d1 = img.segment_data(&file, 1).expect("段 1 应可取到");
        assert_eq!(d1.len(), 0x40);
        assert_eq!(d1, &file[0x200..0x240]);
    }

    #[test]
    fn segment_data_index_out_of_range() {
        let file = two_loads();
        let img = ElfImage::parse(&file).expect("解析失败");
        assert!(img.segment_data(&file, 2).is_none());
        assert!(img.segment_data(&file, 7).is_none());
        assert!(img.segment_data(&file, usize::MAX).is_none());

        // 段有效但缓冲区被截短：越界同样只能是 None，不能 panic
        let short = &file[..0x140];
        assert!(img.segment_data(short, 0).is_none());
    }

    /* ---- 4. 太短 ---- */

    #[test]
    fn too_small() {
        assert_eq!(ElfImage::parse(&[]).err(), Some(ElfError::TooSmall));
        assert_eq!(
            ElfImage::parse(&[0x7f, b'E', b'L', b'F']).err(),
            Some(ElfError::TooSmall)
        );
        let file = two_loads();
        assert_eq!(ElfImage::parse(&file[..63]).err(), Some(ElfError::TooSmall));
    }

    /* ---- 5. 魔数 ---- */

    #[test]
    fn bad_magic() {
        let mut file = two_loads();
        file[1] = b'X';
        assert_eq!(ElfImage::parse(&file).err(), Some(ElfError::BadMagic));

        let mut file2 = two_loads();
        file2[0] = 0x00;
        assert_eq!(ElfImage::parse(&file2).err(), Some(ElfError::BadMagic));
    }

    /* ---- 6/7/8. 类别、字节序、机器 ---- */

    #[test]
    fn not_64_bit() {
        let mut file = two_loads();
        file[4] = 1; // ELFCLASS32
        assert_eq!(ElfImage::parse(&file).err(), Some(ElfError::Not64Bit));
    }

    #[test]
    fn not_little_endian() {
        let mut file = two_loads();
        file[5] = 2; // ELFDATA2MSB
        assert_eq!(
            ElfImage::parse(&file).err(),
            Some(ElfError::NotLittleEndian)
        );
    }

    #[test]
    fn not_x86_64() {
        let mut file = two_loads();
        put16(&mut file, 18, 3); // EM_386
        assert_eq!(ElfImage::parse(&file).err(), Some(ElfError::NotX86_64));
    }

    /* ---- 9. e_type ---- */

    #[test]
    fn not_executable_and_pie() {
        let mut rel = two_loads();
        put16(&mut rel, 16, 1); // ET_REL
        assert_eq!(ElfImage::parse(&rel).err(), Some(ElfError::NotExecutable));

        let mut dyn_elf = two_loads();
        put16(&mut dyn_elf, 16, 3); // ET_DYN
        assert_eq!(ElfImage::parse(&dyn_elf).err(), Some(ElfError::Pie));
    }

    /* ---- 10. PT_INTERP ---- */

    #[test]
    fn needs_interp() {
        let file = build(&[
            Ph {
                ptype: PT_LOAD,
                flags: 5,
                offset: 0x100,
                vaddr: 0x40_0000,
                filesz: 0x80,
                memsz: 0x80,
            },
            Ph {
                ptype: PT_INTERP,
                flags: 4,
                offset: 0x180,
                vaddr: 0x40_0080,
                filesz: 0x10,
                memsz: 0x10,
            },
        ]);
        assert_eq!(ElfImage::parse(&file).err(), Some(ElfError::NeedsInterp));
    }

    /* ---- 11. 头部字段非法 ---- */

    #[test]
    fn bad_header_fields() {
        let mut bad_entsize = two_loads();
        put16(&mut bad_entsize, 54, 32);
        assert_eq!(
            ElfImage::parse(&bad_entsize).err(),
            Some(ElfError::BadHeader)
        );

        let mut zero_phnum = two_loads();
        put16(&mut zero_phnum, 56, 0);
        assert_eq!(
            ElfImage::parse(&zero_phnum).err(),
            Some(ElfError::BadHeader)
        );

        let mut too_many = two_loads();
        put16(&mut too_many, 56, (MAX_SEGMENTS + 1) as u16);
        assert_eq!(ElfImage::parse(&too_many).err(), Some(ElfError::BadHeader));

        // 程序头表整体越出文件
        let mut table_out = two_loads();
        put64(&mut table_out, 32, 0x1000);
        assert_eq!(
            ElfImage::parse(&table_out).err(),
            Some(ElfError::BadHeader)
        );
    }

    /* ---- 12/13. 段越界、filesz > memsz ---- */

    #[test]
    fn segment_beyond_file() {
        let file = build(&[Ph {
            ptype: PT_LOAD,
            flags: 5,
            offset: 0x2c0, // 0x2c0 + 0x100 = 0x3c0 > 0x300
            vaddr: 0x40_0000,
            filesz: 0x100,
            memsz: 0x100,
        }]);
        assert_eq!(ElfImage::parse(&file).err(), Some(ElfError::BadSegment));
    }

    #[test]
    fn filesz_greater_than_memsz() {
        let file = build(&[Ph {
            ptype: PT_LOAD,
            flags: 5,
            offset: 0x100,
            vaddr: 0x40_0000,
            filesz: 0x100,
            memsz: 0x80,
        }]);
        assert_eq!(ElfImage::parse(&file).err(), Some(ElfError::BadSegment));
    }

    /* ---- 14. 没有 PT_LOAD ---- */

    #[test]
    fn no_load_segment() {
        let file = build(&[
            Ph {
                ptype: 4, // PT_NOTE
                flags: 4,
                offset: 0x100,
                vaddr: 0x40_0000,
                filesz: 0x20,
                memsz: 0x20,
            },
            Ph {
                ptype: 0x6474_e551, // PT_GNU_STACK
                flags: 6,
                offset: 0,
                vaddr: 0,
                filesz: 0,
                memsz: 0,
            },
        ]);
        assert_eq!(ElfImage::parse(&file).err(), Some(ElfError::BadSegment));
    }

    /* ---- 15. end_vaddr 溢出 ---- */

    #[test]
    fn end_vaddr_overflow() {
        let seg = Segment {
            vaddr: u64::MAX - 1,
            memsz: 10,
            filesz: 0,
            offset: 0,
            flags: 4,
        };
        assert_eq!(seg.end_vaddr(), None);

        let ok = Segment {
            vaddr: 0x40_0000,
            memsz: 0x1000,
            filesz: 0x1000,
            offset: 0,
            flags: 4,
        };
        assert_eq!(ok.end_vaddr(), Some(0x40_1000));
    }

    /* ---- 16. 权限位 ---- */

    #[test]
    fn permission_flags() {
        let file = two_loads();
        let img = ElfImage::parse(&file).expect("解析失败");
        let segs = img.segments();
        assert!(segs[0].is_exec());
        assert!(!segs[0].is_write());
        assert!(!segs[1].is_exec());
        assert!(segs[1].is_write());

        let ro = Segment {
            vaddr: 0,
            memsz: 1,
            filesz: 1,
            offset: 0,
            flags: 4,
        };
        assert!(!ro.is_exec() && !ro.is_write());
    }

    /* ---- 附加：镜像跨度超限 / vaddr 溢出 ---- */

    #[test]
    fn span_too_large() {
        let file = build(&[
            Ph {
                ptype: PT_LOAD,
                flags: 5,
                offset: 0x100,
                vaddr: 0,
                filesz: 0x10,
                memsz: 0x10,
            },
            Ph {
                ptype: PT_LOAD,
                flags: 6,
                offset: 0x200,
                vaddr: MAX_IMAGE_BYTES + 0x1000,
                filesz: 0x10,
                memsz: 0x10,
            },
        ]);
        assert_eq!(ElfImage::parse(&file).err(), Some(ElfError::TooLarge));
    }

    #[test]
    fn vaddr_overflow_is_too_large() {
        let file = build(&[Ph {
            ptype: PT_LOAD,
            flags: 5,
            offset: 0x100,
            vaddr: u64::MAX - 4,
            filesz: 0x10,
            memsz: 0x10,
        }]);
        assert_eq!(ElfImage::parse(&file).err(), Some(ElfError::TooLarge));
    }
    // ---------------------------------------------------------- fuzz（M11）

    /// 随机字节喂给解析器：**允许返回任何错误，但不许 panic、不许越界读**。
    ///
    /// 1500 轮 × 平均 300 字节，加上"有效头 + 随机改字段"的另一组，
    /// 覆盖的是"文件头合法但自相矛盾"这一类最容易崩的输入。
    #[test]
    fn fuzz_random_bytes_never_panic() {
        use kqa::rng::Rng;
        let mut rng = Rng::new(0xE1F0_0001);
        let mut buf = [0u8; 512];
        for round in 0..1500 {
            let n = (rng.below(512) as usize).max(1);
            rng.fill(&mut buf[..n]);
            // 一半的轮次保留 ELF 魔数，让解析器继续往深处走
            if round % 2 == 0 && n >= 4 {
                buf[..4].copy_from_slice(b"\x7fELF");
            }
            let _ = ElfImage::parse(&buf[..n]);
        }
    }

    /// 有效镜像 + 随机改字段：更容易命中"长度/偏移互相矛盾"的分支。
    #[test]
    fn fuzz_mutated_valid_header_never_panic() {
        use kqa::rng::Rng;
        let mut rng = Rng::new(0xE1F0_0002);
        let base = build(&[Ph {
            ptype: 1,
            flags: 5,
            offset: 0,
            vaddr: 0x400000,
            filesz: 0x100,
            memsz: 0x200,
        }]);
        for _ in 0..1500 {
            let mut d = base.clone();
            let rounds = 1 + rng.below(4) as usize;
            kqa::fuzz::mutate(&mut d, &mut rng, rounds);
            let _ = ElfImage::parse(&d);
        }
    }

}
