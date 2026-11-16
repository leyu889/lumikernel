//! kfs —— 只读 ext2 文件系统解析器
//!
//! 设计约束（与 docs/04-工程规约.md 一致）：
//!
//! 1. `no_std`、零依赖、**不使用堆**。所有输出写进调用方提供的 `&mut [u8]`，
//!    解析过程的中间结果放在 `Ext2Reader` 自带的 4 KiB 定长块缓冲区里，
//!    因此可以跑在没有分配器的内核早期阶段。
//! 2. **绝不 panic**。任何畸形镜像（魔数错、块号越界、inode 号越界、
//!    目录项 `rec_len` 为 0 或未 4 字节对齐、名字长度非法、目录成环）
//!    都只会得到 [`FsError`]，不会出现越界读或算术溢出。
//! 3. 只读：不实现写入、不实现日志（journal）、不实现扩展属性，
//!    **不实现符号链接解析**——遇到符号链接一律返回 [`FsError::Corrupt`]，
//!    交由上层未来实现。
//!
//! 典型的镜像布局（1 KiB 块、1 个块组）：
//!
//! ```text
//! 块 0        引导块（不用）
//! 块 1        超级块（字节偏移 1024）
//! 块 2        块组描述符表
//! 块 3/4      块位图 / inode 位图（本 crate 只读，不校验位图）
//! 块 5..      inode 表
//! ```
//!
//! 本文件不含任何 `unsafe`。

#![cfg_attr(not(test), no_std)]

/// ext2 超级块魔数（位于超级块偏移 +56，u16 小端）。
pub const EXT2_MAGIC: u16 = 0xEF53;

/// 本 crate 支持的最大块大小。块缓冲区按此长度内嵌在 [`Ext2Reader`] 里。
const MAX_BLOCK_SIZE: usize = 4096;

/// inode 结构中本 crate 关心的核心部分长度（v0 固定 128 字节）。
const CORE_INODE_SIZE: usize = 128;

/// inode 内 15 个块指针：12 个直接 + 1 一级间接 + 1 二级间接 + 1 三级间接。
const INODE_BLOCK_SLOTS: usize = 15;

/// 直接块指针个数。
const DIRECT_BLOCKS: u64 = 12;

/// 根目录 inode 号固定为 2（ext2 保留）。
const ROOT_INODE: u32 = 2;

/// `mode` 字段中「文件类型」的掩码。
const S_IFMT: u16 = 0xF000;
/// 目录。
const S_IFDIR: u16 = 0x4000;
/// 符号链接（本 crate 不解析）。
const S_IFLNK: u16 = 0xA000;

/// 块设备访问错误。
#[derive(Debug, PartialEq)]
pub enum IoError {
    /// 请求的块号超出设备容量。
    OutOfRange,
    /// 设备驱动自身报错（介质错误、超时等）。
    DeviceError,
}

/// 文件系统层错误。
#[derive(Debug, PartialEq)]
pub enum FsError {
    /// 超级块魔数不是 [`EXT2_MAGIC`]。
    BadMagic,
    /// 超级块的其它字段不合法（块大小、块数、inode 大小等），或与设备不符。
    BadSuperblock,
    /// 路径中某一级不存在。
    NotFound,
    /// 需要目录的地方给了普通文件。
    NotADirectory,
    /// 需要普通文件的地方给了目录。
    IsADirectory,
    /// 调用方给的输出缓冲区（或 `list` 的输出数组）装不下结果。
    BufferTooSmall,
    /// 镜像结构损坏：块号/inode 号越界、目录项非法、存在环、遇到未实现的符号链接等。
    Corrupt,
    /// 符号链接层数超过上限（`ELOOP`）。
    Loop,
    /// 路径或文件名超出本实现能处理的上限。
    NameTooLong,
    /// 对非符号链接调用 `read_link`。
    NotASymlink,
    /// 底层块设备错误。
    Io(IoError),
}

/// 路径解析用的暂存缓冲大小。
const PATH_SCRATCH: usize = 512;

/// 符号链接最多跟几层。Linux 也是这个量级（`MAXSYMLINKS = 40`），
/// 这里取小一点够用，同时让"环"更快暴露。
pub const MAX_SYMLINK_DEPTH: u32 = 8;

/// 块设备抽象。由上层（IDE / ATA / virtio / 内存 mock）注入。
///
/// 约定：`block_size()` 只能是 1024 或 4096，且必须与超级块里的
/// `1024 << s_log_block_size` 一致，否则 [`Ext2Reader::mount`] 返回
/// [`FsError::BadSuperblock`]。
pub trait BlockDevice {
    /// 块大小（字节）。
    fn block_size(&self) -> usize;

    /// 读取第 `lba` 块到 `buf`。`buf.len()` 必须不小于 `block_size()`。
    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), IoError>;

    /// 设备总块数（`lba` 的合法上界是 `block_count() - 1`）。
    fn block_count(&self) -> u64;
}

/// inode 的对外摘要信息。块指针不对外暴露，`read_file` 会按 `number` 重读。
#[derive(Clone, Copy, Debug)]
pub struct Inode {
    /// inode 号，从 1 开始。
    pub number: u32,
    /// 原始 `mode`（含文件类型位与权限位）。
    pub mode: u16,
    /// 文件字节数。
    pub size: u32,
    /// `i_blocks`：占用的 512 字节扇区数（非块数）。
    pub blocks: u32,
}

/// 目录项。
#[derive(Clone, Copy, Debug)]
pub struct DirEntry {
    /// 指向的 inode 号。
    pub inode: u32,
    /// 名字长度（1..=255）。
    pub name_len: u8,
    /// 名字（定长数组，仅前 `name_len` 字节有效）。
    pub name: [u8; 255],
}

impl DirEntry {
    /// 有效名字切片。
    pub fn name(&self) -> &[u8] {
        let n = self.name_len as usize;
        if n > self.name.len() {
            return &[];
        }
        match self.name.get(..n) {
            Some(s) => s,
            None => &[],
        }
    }
}

/// inode 里本 crate 需要的字段（比对外 [`Inode`] 多出 15 个块指针）。
#[derive(Clone, Copy)]
struct RawInode {
    mode: u16,
    size: u32,
    blocks: u32,
    ptr: [u32; INODE_BLOCK_SLOTS],
}

/// 只读 ext2 读取器。
///
/// 内部持有一块 [`MAX_BLOCK_SIZE`] 字节的解析缓冲区，因此**不是** `Sync`，
/// 也不可重入：同一时刻只允许一个操作在跑（这正好匹配内核里单线程挂载场景）。
pub struct Ext2Reader<D: BlockDevice> {
    dev: D,
    block_size: usize,
    /// 文件系统块数（来自超级块，作为块号合法上界）。
    block_count: u64,
    inodes_count: u32,
    inodes_per_group: u32,
    inode_size: usize,
    /// 块组描述符表起始块号（1024 块大小时为第 2 块，否则第 1 块）。
    gdt_start: u64,
    /// 块组总数。
    group_count: u32,
    /// 挂载时读好并缓存的根 inode，保证 [`Ext2Reader::root`] 不会失败。
    root: Inode,
    buf: [u8; MAX_BLOCK_SIZE],
}

/// 从 `buf` 的 `off` 处读小端 u16；越界返回 `None`（调用方转成错误，绝不 panic）。
fn get16(buf: &[u8], off: usize) -> Option<u16> {
    let b = buf.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

/// 从 `buf` 的 `off` 处读小端 u32；越界返回 `None`。
fn get32(buf: &[u8], off: usize) -> Option<u32> {
    let b = buf.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

impl<D: BlockDevice> Ext2Reader<D> {
    /// 挂载：读超级块、校验参数、定位块组描述符表，并预读根 inode。
    ///
    /// 失败时设备被丢弃（`dev` 按值传入），调用方可以重新构造。
    pub fn mount(mut dev: D) -> Result<Self, FsError> {
        let block_size = dev.block_size();
        // 只支持 1 KiB / 4 KiB 两种块大小（内核页大小与之对齐）。
        if block_size != 1024 && block_size != MAX_BLOCK_SIZE {
            return Err(FsError::BadSuperblock);
        }
        let dev_blocks = dev.block_count();
        if dev_blocks == 0 {
            return Err(FsError::BadSuperblock);
        }

        // 超级块固定在字节偏移 1024：按块大小换算成「块号 + 块内偏移」。
        let sb_lba = 1024u64 / block_size as u64;
        let sb_off = 1024usize % block_size;
        if sb_lba >= dev_blocks {
            return Err(FsError::BadSuperblock);
        }
        let mut sector = [0u8; MAX_BLOCK_SIZE];
        dev.read_block(sb_lba, &mut sector[..block_size])
            .map_err(FsError::Io)?;

        let sb_end = sb_off.checked_add(1024).ok_or(FsError::BadSuperblock)?;
        let sb = sector.get(sb_off..sb_end).ok_or(FsError::BadSuperblock)?;

        if get16(sb, 56).ok_or(FsError::BadSuperblock)? != EXT2_MAGIC {
            return Err(FsError::BadMagic);
        }

        let inodes_count = get32(sb, 0).ok_or(FsError::BadSuperblock)?;
        let blocks_count = get32(sb, 4).ok_or(FsError::BadSuperblock)?;
        let first_data_block = get32(sb, 20).ok_or(FsError::BadSuperblock)?;
        let log_block_size = get32(sb, 24).ok_or(FsError::BadSuperblock)?;
        let blocks_per_group = get32(sb, 32).ok_or(FsError::BadSuperblock)?;
        let inodes_per_group = get32(sb, 40).ok_or(FsError::BadSuperblock)?;
        let rev_level = get32(sb, 76).ok_or(FsError::BadSuperblock)?;
        let raw_inode_size = get16(sb, 88).ok_or(FsError::BadSuperblock)?;

        // 块大小必须与设备一致（也顺带挡住 log_block_size 离谱的镜像）。
        if log_block_size > 6 {
            return Err(FsError::BadSuperblock);
        }
        if (1024u64 << log_block_size) != block_size as u64 {
            return Err(FsError::BadSuperblock);
        }
        // 首数据块：1 KiB 块时为 1，否则为 0。
        let expect_first = if block_size == 1024 { 1 } else { 0 };
        if first_data_block != expect_first {
            return Err(FsError::BadSuperblock);
        }
        if inodes_count == 0 || blocks_count == 0 {
            return Err(FsError::BadSuperblock);
        }
        if blocks_count as u64 > dev_blocks {
            return Err(FsError::BadSuperblock);
        }
        if blocks_per_group == 0 || inodes_per_group == 0 {
            return Err(FsError::BadSuperblock);
        }

        // rev 0 的 inode 固定 128 字节；更高版本才读 s_inode_size。
        let inode_size = if rev_level >= 1 {
            raw_inode_size as usize
        } else {
            CORE_INODE_SIZE
        };
        if inode_size < CORE_INODE_SIZE || inode_size > block_size || inode_size % 4 != 0 {
            return Err(FsError::BadSuperblock);
        }

        // 块组数（向上取整），用于挡住越界的组号 / inode 号。
        let group_count = (blocks_count as u64)
            .checked_add(blocks_per_group as u64 - 1)
            .ok_or(FsError::BadSuperblock)?
            / blocks_per_group as u64;
        if group_count == 0 || group_count > u32::MAX as u64 {
            return Err(FsError::BadSuperblock);
        }

        // 块组描述符表紧跟在超级块之后：1 KiB 块 → 第 2 块，其它 → 第 1 块。
        let gdt_start = first_data_block as u64 + 1;
        if gdt_start >= blocks_count as u64 {
            return Err(FsError::BadSuperblock);
        }

        let mut fs = Self {
            dev,
            block_size,
            block_count: blocks_count as u64,
            inodes_count,
            inodes_per_group,
            inode_size,
            gdt_start,
            group_count: group_count as u32,
            // 先占位，下面读出来覆盖；保证 `root()` 永不失败。
            root: Inode { number: ROOT_INODE, mode: 0, size: 0, blocks: 0 },
            buf: [0u8; MAX_BLOCK_SIZE],
        };

        let root = fs.read_inode(ROOT_INODE)?;
        if (root.mode & S_IFMT) != S_IFDIR {
            // 根不是目录 → 镜像不可用。
            return Err(FsError::Corrupt);
        }
        fs.root = root;
        Ok(fs)
    }

    /// 根 inode（挂载时已缓存，不会失败）。
    pub fn root(&self) -> Inode {
        self.root
    }

    /// 是否是目录。
    pub fn is_dir(&self, inode: Inode) -> bool {
        (inode.mode & S_IFMT) == S_IFDIR
    }

    /// 解析绝对路径（**跟随**符号链接）。
    ///
    /// - `"/"` 返回根 inode；`"//a//b/"` 这类多余斜杠会被忽略。
    /// - 相对路径（不以 `/` 开头）返回 [`FsError::NotFound`]。
    /// - 路径中间某级不是目录 → [`FsError::NotADirectory`]。
    /// - 符号链接跟随到 [`MAX_SYMLINK_DEPTH`] 层为止，超过返回 [`FsError::Loop`]。
    /// - 组件长度超过 255 字节 → [`FsError::NameTooLong`]。
    pub fn lookup(&mut self, path: &str) -> Result<Inode, FsError> {
        self.lookup_opt(path, true)
    }

    /// 解析绝对路径但**不跟随最后一段**的符号链接（`lstat` / `readlink` 语义）。
    pub fn lookup_nofollow(&mut self, path: &str) -> Result<Inode, FsError> {
        self.lookup_opt(path, false)
    }

    /// 路径解析主体。
    ///
    /// 符号链接的处理是"把目标拼到剩余路径前面，然后接着解析"——和 Linux 的
    /// `link_path_walk` 同一个思路。拼出来的路径放另一个缓冲区，
    /// 这样游标不用回退，也不会出现"边解析边覆盖自己"的问题。
    pub fn lookup_opt(&mut self, path: &str, follow_last: bool) -> Result<Inode, FsError> {
        if !path.starts_with('/') {
            return Err(FsError::NotFound);
        }
        if path.len() >= PATH_SCRATCH {
            return Err(FsError::NameTooLong);
        }
        let mut work = [0u8; PATH_SCRATCH];
        work[..path.len()].copy_from_slice(path.as_bytes());
        let mut wlen = path.len();
        let mut pos = 0usize;
        let mut cur = self.root;
        let mut depth = 0u32;

        loop {
            while pos < wlen && work[pos] == b'/' {
                pos += 1;
            }
            if pos >= wlen {
                return Ok(cur);
            }
            let start = pos;
            while pos < wlen && work[pos] != b'/' {
                pos += 1;
            }
            let name_len = pos - start;
            if name_len == 0 {
                continue;
            }
            if name_len > 255 {
                return Err(FsError::NameTooLong);
            }
            if !self.is_dir(cur) {
                return Err(FsError::NotADirectory);
            }
            let raw = self.read_inode_raw(cur.number)?;
            let mut found: Option<u32> = None;
            {
                let name = &work[start..start + name_len];
                self.walk_dir(&raw, |e| {
                    if e.name_len as usize == name_len
                        && e.name.get(..name_len).map_or(false, |s| s == name)
                    {
                        found = Some(e.inode);
                        Ok(false)
                    } else {
                        Ok(true)
                    }
                })?;
            }
            let ino = found.ok_or(FsError::NotFound)?;
            let next = self.read_inode(ino)?;

            // 这一段之后还有组件吗？最后一段的符号链接是否跟随，由 `follow_last` 决定。
            let mut peek = pos;
            while peek < wlen && work[peek] == b'/' {
                peek += 1;
            }
            let is_last = peek >= wlen;

            if (next.mode & S_IFMT) == S_IFLNK && (!is_last || follow_last) {
                let mut tgt = [0u8; 256];
                let tl = self.read_link(next, &mut tgt)?;
                depth += 1;
                if depth > MAX_SYMLINK_DEPTH {
                    return Err(FsError::Loop);
                }
                let rest_len = wlen - pos;
                if tl + rest_len >= PATH_SCRATCH {
                    return Err(FsError::NameTooLong);
                }
                let mut nw = [0u8; PATH_SCRATCH];
                nw[..tl].copy_from_slice(&tgt[..tl]);
                if rest_len > 0 {
                    nw[tl..tl + rest_len].copy_from_slice(&work[pos..wlen]);
                }
                // 绝对目标从根重新开始；相对目标相对"符号链接所在目录"。
                if tl > 0 && tgt[0] == b'/' {
                    cur = self.root;
                }
                work = nw;
                wlen = tl + rest_len;
                pos = 0;
                continue;
            }

            cur = next;
        }
    }

    /// 读符号链接的目标（fast symlink 直接存在 inode 里，慢链接读数据块）。
    ///
    /// 目标长度上限 255（`out` 至少要有这么大）；本实现不支持超过 255 字节的目标。
    pub fn read_link(&mut self, inode: Inode, out: &mut [u8]) -> Result<usize, FsError> {
        if (inode.mode & S_IFMT) != S_IFLNK {
            return Err(FsError::NotASymlink);
        }
        let raw = self.read_inode_raw(inode.number)?;
        let size = raw.size as usize;
        if size == 0 {
            return Err(FsError::Corrupt);
        }
        if size > out.len() || size > 255 {
            return Err(FsError::BufferTooSmall);
        }
        if size < 60 {
            // ext2 的约定：目标短于 60 字节时不占数据块，直接写在 i_block 里。
            // `ptr` 是按 u32 读出来的，转回原始字节序即可。
            let mut tmp = [0u8; 60];
            for (i, w) in raw.ptr.iter().enumerate() {
                let b = w.to_le_bytes();
                if let Some(dst) = tmp.get_mut(i * 4..i * 4 + 4) {
                    dst.copy_from_slice(&b);
                }
            }
            out[..size].copy_from_slice(&tmp[..size]);
            Ok(size)
        } else {
            self.read_data(&raw, 0, &mut out[..size])
        }
    }

    /// 从 `offset` 开始读最多 `out.len()` 字节，返回实际读到的字节数。
    ///
    /// - `offset >= size` 或 `out` 为空 → 返回 `Ok(0)`（明确的「读到尾」语义）。
    /// - 传入目录 → [`FsError::IsADirectory`]；传入符号链接 → [`FsError::Corrupt`]。
    /// - 稀疏文件中的空洞（块指针为 0）读出全 0。
    /// - 镜像中的块指针越界 → [`FsError::Corrupt`]。
    pub fn read_file(&mut self, inode: Inode, offset: u32, out: &mut [u8]) -> Result<usize, FsError> {
        match inode.mode & S_IFMT {
            S_IFDIR => return Err(FsError::IsADirectory),
            S_IFLNK => return Err(FsError::Corrupt),
            _ => {}
        }
        let raw = self.read_inode_raw(inode.number)?;
        self.read_data(&raw, offset, out)
    }

    /// 从原始 inode 读数据：不做类型检查（符号链接的数据块也走这里）。
    fn read_data(&mut self, raw: &RawInode, offset: u32, out: &mut [u8]) -> Result<usize, FsError> {
        let size = raw.size as u64;
        let start = offset as u64;
        if start >= size || out.is_empty() {
            return Ok(0);
        }
        let want = core::cmp::min(out.len() as u64, size - start) as usize;
        let bs = self.block_size as u64;
        let mut done = 0usize;
        while done < want {
            let file_off = start + done as u64;
            let logical = file_off / bs;
            if logical > u32::MAX as u64 {
                return Err(FsError::Corrupt);
            }
            let in_block = (file_off % bs) as usize;
            let chunk = core::cmp::min(bs as usize - in_block, want - done);
            let phys = self.bmap(raw, logical as u32)?;
            if phys == 0 {
                // 空洞：补 0。
                if let Some(dst) = out.get_mut(done..done + chunk) {
                    for b in dst.iter_mut() {
                        *b = 0;
                    }
                }
            } else {
                self.load_block(phys)?;
                let src = self
                    .buf
                    .get(in_block..in_block + chunk)
                    .ok_or(FsError::Corrupt)?;
                let dst = out.get_mut(done..done + chunk).ok_or(FsError::BufferTooSmall)?;
                dst.copy_from_slice(src);
            }
            done += chunk;
        }
        Ok(done)
    }

    /// 列出目录项（包含 `.` 与 `..`），返回写入 `out` 的条目数。
    ///
    /// 输出数组装不下全部条目时返回 [`FsError::BufferTooSmall`]（此时 `out`
    /// 内容未定义）；这需要预先数一遍条目，所以会扫描两遍目录。
    pub fn list(&mut self, dir: Inode, out: &mut [DirEntry]) -> Result<usize, FsError> {
        match dir.mode & S_IFMT {
            S_IFDIR => {}
            S_IFLNK => return Err(FsError::Corrupt),
            _ => return Err(FsError::NotADirectory),
        }
        let raw = self.read_inode_raw(dir.number)?;

        let mut need = 0usize;
        self.walk_dir(&raw, |_e| {
            need = need.saturating_add(1);
            Ok(true)
        })?;
        if need > out.len() {
            return Err(FsError::BufferTooSmall);
        }

        let mut n = 0usize;
        self.walk_dir(&raw, |e| {
            if let Some(slot) = out.get_mut(n) {
                *slot = *e;
                n += 1;
            }
            Ok(true)
        })?;
        Ok(n)
    }

    // ---------------------------------------------------------------- 内部实现

    /// 读一个块到内部缓冲区，同时做块号越界检查。
    fn load_block(&mut self, lba: u64) -> Result<(), FsError> {
        if lba >= self.block_count {
            return Err(FsError::Corrupt);
        }
        let bs = self.block_size;
        let buf = self.buf.get_mut(..bs).ok_or(FsError::Corrupt)?;
        self.dev.read_block(lba, buf).map_err(FsError::Io)
    }

    /// 读 inode 号 `number` 的原始结构。号越界 → [`FsError::Corrupt`]（不 panic）。
    fn read_inode_raw(&mut self, number: u32) -> Result<RawInode, FsError> {
        if number == 0 || number > self.inodes_count {
            return Err(FsError::Corrupt);
        }
        let idx = number - 1;
        let group = idx / self.inodes_per_group;
        let local = idx % self.inodes_per_group;
        if group >= self.group_count {
            return Err(FsError::Corrupt);
        }

        // 块组描述符：32 字节一项，紧跟在 gdt_start 之后连续排列。
        let gd_bytes = group as u64 * 32;
        let gd_lba = self
            .gdt_start
            .checked_add(gd_bytes / self.block_size as u64)
            .ok_or(FsError::Corrupt)?;
        self.load_block(gd_lba)?;
        let gd_off = (gd_bytes % self.block_size as u64) as usize;
        let inode_table = get32(&self.buf, gd_off + 8).ok_or(FsError::Corrupt)? as u64;
        if inode_table == 0 || inode_table >= self.block_count {
            return Err(FsError::Corrupt);
        }

        // inode 表内的字节偏移（inode 不会跨块：inode_size 整除块大小，
        // 但仍做一次显式检查以防镜像不规整）。
        let byte_off = local as u64 * self.inode_size as u64;
        let tbl_lba = inode_table
            .checked_add(byte_off / self.block_size as u64)
            .ok_or(FsError::Corrupt)?;
        let off = (byte_off % self.block_size as u64) as usize;
        if off + CORE_INODE_SIZE > self.block_size {
            return Err(FsError::Corrupt);
        }
        self.load_block(tbl_lba)?;

        let mode = get16(&self.buf, off).ok_or(FsError::Corrupt)?;
        let size = get32(&self.buf, off + 4).ok_or(FsError::Corrupt)?;
        let blocks = get32(&self.buf, off + 28).ok_or(FsError::Corrupt)?;
        let mut ptr = [0u32; INODE_BLOCK_SLOTS];
        for (i, slot) in ptr.iter_mut().enumerate() {
            *slot = get32(&self.buf, off + 40 + i * 4).ok_or(FsError::Corrupt)?;
        }

        Ok(RawInode { mode, size, blocks, ptr })
    }

    /// 读 inode 摘要。
    fn read_inode(&mut self, number: u32) -> Result<Inode, FsError> {
        let raw = self.read_inode_raw(number)?;
        Ok(Inode { number, mode: raw.mode, size: raw.size, blocks: raw.blocks })
    }

    /// 逻辑块号 → 物理块号。返回 0 表示空洞；索引超出三级间接覆盖范围 → [`FsError::Corrupt`]。
    fn bmap(&mut self, inode: &RawInode, logical: u32) -> Result<u64, FsError> {
        let ppb = (self.block_size / 4) as u64; // 每块可容纳的块指针数
        let l = logical as u64;
        if l < DIRECT_BLOCKS {
            return Ok(inode.ptr[logical as usize] as u64);
        }

        let mut idx = l - DIRECT_BLOCKS;
        let ppb2 = ppb.saturating_mul(ppb);
        let ppb3 = ppb2.saturating_mul(ppb);
        let (root, depth) = if idx < ppb {
            (inode.ptr[12], 1u32)
        } else if idx < ppb + ppb2 {
            idx -= ppb;
            (inode.ptr[13], 2)
        } else if idx < ppb + ppb2 + ppb3 {
            idx -= ppb + ppb2;
            (inode.ptr[14], 3)
        } else {
            return Err(FsError::Corrupt);
        };
        if root == 0 {
            return Ok(0);
        }

        let mut cur = root as u64;
        let mut div = 1u64;
        for _ in 1..depth {
            div *= ppb;
        }
        let mut rem = idx;
        for _ in 0..depth {
            if cur == 0 {
                return Ok(0);
            }
            self.load_block(cur)?;
            let slot = (rem / div) as usize;
            rem %= div;
            cur = get32(&self.buf, slot * 4).ok_or(FsError::Corrupt)? as u64;
            if div > 1 {
                div /= ppb;
            }
        }
        Ok(cur)
    }

    /// 遍历目录数据块，对每个有效目录项调用 `visit`；`visit` 返回 `false` 表示提前结束。
    ///
    /// 所有结构检查都在这里完成：`rec_len == 0`（环）、未 4 字节对齐、
    /// 越出本块、`name_len` 大于 `rec_len - 8`、指向越界 inode，一律 [`FsError::Corrupt`]。
    fn walk_dir<F>(&mut self, dir: &RawInode, mut visit: F) -> Result<(), FsError>
    where
        F: FnMut(&DirEntry) -> Result<bool, FsError>,
    {
        let bs = self.block_size;
        let size = dir.size as u64;
        if size == 0 {
            return Ok(());
        }
        let nblocks = (size + bs as u64 - 1) / bs as u64;
        for logical in 0..nblocks {
            if logical > u32::MAX as u64 {
                return Err(FsError::Corrupt);
            }
            let phys = self.bmap(dir, logical as u32)?;
            if phys == 0 {
                // 目录数据不允许是空洞（也无法解释为合法目录项）。
                return Err(FsError::Corrupt);
            }
            self.load_block(phys)?;

            let block_start = logical * bs as u64;
            let remain = core::cmp::min(bs as u64, size - block_start) as usize;
            let mut off = 0usize;
            while off + 8 <= remain {
                let inode = get32(&self.buf, off).ok_or(FsError::Corrupt)?;
                let rec_len = get16(&self.buf, off + 4).ok_or(FsError::Corrupt)? as usize;
                let name_len = *self.buf.get(off + 6).ok_or(FsError::Corrupt)? as usize;

                if rec_len == 0 {
                    // rec_len 为 0 → 线性遍历会原地打转（环）。
                    return Err(FsError::Corrupt);
                }
                if rec_len < 8 || rec_len % 4 != 0 {
                    return Err(FsError::Corrupt);
                }
                if off + rec_len > remain {
                    return Err(FsError::Corrupt);
                }
                // name_len 是 u8（≤255），这里挡住「名字伸进下一条目」的情况。
                if name_len > 255 || name_len > rec_len - 8 {
                    return Err(FsError::Corrupt);
                }

                if inode != 0 {
                    if inode > self.inodes_count {
                        return Err(FsError::Corrupt);
                    }
                    let mut entry = DirEntry {
                        inode,
                        name_len: name_len as u8,
                        name: [0u8; 255],
                    };
                    let src = self
                        .buf
                        .get(off + 8..off + 8 + name_len)
                        .ok_or(FsError::Corrupt)?;
                    if let Some(dst) = entry.name.get_mut(..name_len) {
                        dst.copy_from_slice(src);
                    }
                    if !visit(&entry)? {
                        return Ok(());
                    }
                }
                off += rec_len;
            }
        }
        Ok(())
    }
}

// ===================================================================== 测试
#[cfg(test)]
mod tests {
    use super::*;

    const BS: usize = 1024;
    const TOTAL_BLOCKS: usize = 32;
    const INODE_TABLE_BLOCK: usize = 5;
    const ROOT_DIR_BLOCK: usize = 7;
    const SUB_DIR_BLOCK: usize = 8;
    const HELLO_BLOCK: usize = 9;
    const X_BLOCK: usize = 10;
    const INODE_TABLE_OFF: usize = INODE_TABLE_BLOCK * BS;

    const MODE_DIR: u16 = 0x41ED; // 0o040755
    const MODE_FILE: u16 = 0x81A4; // 0o100644

    const HELLO: &[u8] = b"hello ext2";
    const XDATA: &[u8] = b"deep";

    // ------------------------------------------------------------ 测试辅助

    fn put16(img: &mut [u8], off: usize, v: u16) {
        img[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn put32(img: &mut [u8], off: usize, v: u32) {
        img[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn write_inode(img: &mut [u8], ino: u32, mode: u16, size: u32, blocks: &[u32]) {
        let off = INODE_TABLE_OFF + (ino as usize - 1) * 128;
        put16(img, off, mode);
        put16(img, off + 26, 1); // i_links_count
        put32(img, off + 4, size);
        put32(img, off + 28, 2); // i_blocks（512B 扇区数）
        for (i, b) in blocks.iter().enumerate() {
            put32(img, off + 40 + i * 4, *b);
        }
    }

    /// 按 ext2 规范写一个目录块：每条目 8 字节头 + 名字，`rec_len` 4 字节对齐，
    /// 最后一条的 `rec_len` 一直延伸到块尾。
    fn write_dir(img: &mut [u8], block: usize, entries: &[(u32, &[u8])]) {
        let base = block * BS;
        let mut off = 0usize;
        for (i, (ino, name)) in entries.iter().enumerate() {
            let is_last = i + 1 == entries.len();
            let need = 8 + name.len();
            let rec = if is_last { BS - off } else { (need + 3) & !3 };
            assert!(rec >= need, "目录块放不下");
            put32(img, base + off, *ino);
            put16(img, base + off + 4, rec as u16);
            img[base + off + 6] = name.len() as u8;
            img[base + off + 7] = if *ino == 2 || *ino == 3 { 2 } else { 1 };
            img[base + off + 8..base + off + 8 + name.len()].copy_from_slice(name);
            off += rec;
        }
    }

    /// 在内存里拼一个最小可用 ext2 镜像：
    /// 1 KiB 块、1 个块组、根目录含 `hello.txt` 与子目录 `sub/`（含 `x.txt`）。
    fn build_minimal_ext2() -> Vec<u8> {
        let mut img = vec![0u8; TOTAL_BLOCKS * BS];

        // ---- 超级块（字节偏移 1024）----
        let sb = 1024;
        put32(&mut img, sb + 0, 16); // s_inodes_count
        put32(&mut img, sb + 4, TOTAL_BLOCKS as u32); // s_blocks_count
        put32(&mut img, sb + 12, 20); // s_free_blocks_count
        put32(&mut img, sb + 16, 12); // s_free_inodes_count
        put32(&mut img, sb + 20, 1); // s_first_data_block
        put32(&mut img, sb + 24, 0); // s_log_block_size → 1024
        put32(&mut img, sb + 32, 8192); // s_blocks_per_group
        put32(&mut img, sb + 40, 16); // s_inodes_per_group
        put16(&mut img, sb + 56, EXT2_MAGIC); // s_magic
        put32(&mut img, sb + 76, 1); // s_rev_level
        put32(&mut img, sb + 84, 11); // s_first_ino
        put16(&mut img, sb + 88, 128); // s_inode_size

        // ---- 块组描述符表（第 2 块）----
        let gd = 2 * BS;
        put32(&mut img, gd + 0, 3); // bg_block_bitmap
        put32(&mut img, gd + 4, 4); // bg_inode_bitmap
        put32(&mut img, gd + 8, INODE_TABLE_BLOCK as u32); // bg_inode_table
        put16(&mut img, gd + 12, 20); // bg_free_blocks_count
        put16(&mut img, gd + 14, 12); // bg_free_inodes_count

        // ---- inode：根(2)、sub(3)、hello.txt(4)、x.txt(5) ----
        write_inode(&mut img, 2, MODE_DIR, BS as u32, &[ROOT_DIR_BLOCK as u32]);
        write_inode(&mut img, 3, MODE_DIR, BS as u32, &[SUB_DIR_BLOCK as u32]);
        write_inode(
            &mut img,
            4,
            MODE_FILE,
            HELLO.len() as u32,
            &[HELLO_BLOCK as u32],
        );
        write_inode(&mut img, 5, MODE_FILE, XDATA.len() as u32, &[X_BLOCK as u32]);

        // ---- 目录内容 ----
        write_dir(
            &mut img,
            ROOT_DIR_BLOCK,
            &[(2, b"."), (2, b".."), (4, b"hello.txt"), (3, b"sub")],
        );
        write_dir(
            &mut img,
            SUB_DIR_BLOCK,
            &[(3, b"."), (2, b".."), (5, b"x.txt")],
        );

        // ---- 文件内容 ----
        let hb = HELLO_BLOCK * BS;
        img[hb..hb + HELLO.len()].copy_from_slice(HELLO);
        let xb = X_BLOCK * BS;
        img[xb..xb + XDATA.len()].copy_from_slice(XDATA);

        img
    }

    /// 内存块设备：直接在一块 `Vec<u8>` 上做块读。
    struct MockDevice {
        data: Vec<u8>,
        block_size: usize,
    }

    impl MockDevice {
        fn new(data: Vec<u8>, block_size: usize) -> Self {
            Self { data, block_size }
        }
    }

    impl BlockDevice for MockDevice {
        fn block_size(&self) -> usize {
            self.block_size
        }

        fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), IoError> {
            let bs = self.block_size;
            if buf.len() < bs {
                return Err(IoError::OutOfRange);
            }
            let start = (lba as usize).checked_mul(bs).ok_or(IoError::OutOfRange)?;
            let end = start.checked_add(bs).ok_or(IoError::OutOfRange)?;
            let src = self.data.get(start..end).ok_or(IoError::OutOfRange)?;
            buf[..bs].copy_from_slice(src);
            Ok(())
        }

        fn block_count(&self) -> u64 {
            (self.data.len() / self.block_size) as u64
        }
    }

    /// 指定块号读失败，用于验证错误向上传播。
    struct FailingDevice {
        inner: MockDevice,
        fail_lba: u64,
    }

    impl BlockDevice for FailingDevice {
        fn block_size(&self) -> usize {
            self.inner.block_size()
        }

        fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), IoError> {
            if lba == self.fail_lba {
                return Err(IoError::DeviceError);
            }
            self.inner.read_block(lba, buf)
        }

        fn block_count(&self) -> u64 {
            self.inner.block_count()
        }
    }

    fn reader(img: Vec<u8>) -> Ext2Reader<MockDevice> {
        Ext2Reader::mount(MockDevice::new(img, BS)).expect("挂载应成功")
    }

    // ------------------------------------------------------------ 测试用例

    #[test]
    fn mount_ok_and_root_is_inode_2() {
        let fs = reader(build_minimal_ext2());
        assert_eq!(fs.root().number, 2);
        assert_eq!(fs.root().mode & S_IFMT, S_IFDIR);
        assert!(fs.is_dir(fs.root()));
        assert_eq!(fs.root().size as usize, BS);
    }

    #[test]
    fn lookup_and_read_hello() {
        let mut fs = reader(build_minimal_ext2());
        let ino = fs.lookup("/hello.txt").expect("应找到 hello.txt");
        assert_eq!(ino.number, 4);
        assert!(!fs.is_dir(ino));
        assert_eq!(ino.size as usize, HELLO.len());

        let mut buf = [0u8; 64];
        let n = fs.read_file(ino, 0, &mut buf).expect("读文件应成功");
        assert_eq!(n, HELLO.len());
        assert_eq!(&buf[..n], HELLO);
    }

    #[test]
    fn lookup_nested_two_levels() {
        let mut fs = reader(build_minimal_ext2());
        let sub = fs.lookup("/sub").expect("应找到 sub");
        assert!(fs.is_dir(sub));
        assert_eq!(sub.number, 3);

        let x = fs.lookup("/sub/x.txt").expect("应找到 /sub/x.txt");
        assert_eq!(x.number, 5);
        let mut buf = [0u8; 16];
        let n = fs.read_file(x, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], XDATA);
    }

    #[test]
    fn lookup_missing_is_not_found() {
        let mut fs = reader(build_minimal_ext2());
        assert_eq!(fs.lookup("/nope.txt").unwrap_err(), FsError::NotFound);
        assert_eq!(fs.lookup("/sub/nope").unwrap_err(), FsError::NotFound);
        assert_eq!(fs.lookup("/sub/x.txt/deeper").unwrap_err(), FsError::NotADirectory);
        assert_eq!(fs.lookup("relative").unwrap_err(), FsError::NotFound);
    }

    #[test]
    fn list_on_regular_file_is_not_a_directory() {
        let mut fs = reader(build_minimal_ext2());
        let ino = fs.lookup("/hello.txt").unwrap();
        let mut out = [DirEntry { inode: 0, name_len: 0, name: [0u8; 255] }; 4];
        assert_eq!(fs.list(ino, &mut out).unwrap_err(), FsError::NotADirectory);
    }

    #[test]
    fn list_root_entries() {
        let mut fs = reader(build_minimal_ext2());
        let root = fs.root();
        let mut out = [DirEntry { inode: 0, name_len: 0, name: [0u8; 255] }; 8];
        let n = fs.list(root, &mut out).unwrap();
        assert_eq!(n, 4);
        let names: Vec<&[u8]> = out[..n].iter().map(|e| e.name()).collect();
        assert_eq!(names, vec![&b"."[..], &b".."[..], &b"hello.txt"[..], &b"sub"[..]]);
        assert_eq!(out[2].inode, 4);
        assert_eq!(out[3].inode, 3);
    }

    #[test]
    fn list_buffer_too_small() {
        let mut fs = reader(build_minimal_ext2());
        let root = fs.root();
        let mut out = [DirEntry { inode: 0, name_len: 0, name: [0u8; 255] }; 2];
        assert_eq!(fs.list(root, &mut out).unwrap_err(), FsError::BufferTooSmall);
    }

    #[test]
    fn read_file_in_four_byte_chunks() {
        let mut fs = reader(build_minimal_ext2());
        let ino = fs.lookup("/hello.txt").unwrap();
        let mut all: Vec<u8> = Vec::new();
        let mut off = 0u32;
        loop {
            let mut chunk = [0u8; 4];
            let n = fs.read_file(ino, off, &mut chunk).unwrap();
            assert!(n <= 4);
            if n == 0 {
                break;
            }
            all.extend_from_slice(&chunk[..n]);
            off += n as u32;
            assert!(off as usize <= HELLO.len());
        }
        assert_eq!(off as usize, HELLO.len());
        assert_eq!(&all[..], HELLO);
        // 4 字节一读 → 4 + 4 + 2
        assert_eq!(all.len(), 10);
    }

    #[test]
    fn read_file_offset_past_eof_returns_zero() {
        let mut fs = reader(build_minimal_ext2());
        let ino = fs.lookup("/hello.txt").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(fs.read_file(ino, HELLO.len() as u32, &mut buf).unwrap(), 0);
        assert_eq!(fs.read_file(ino, HELLO.len() as u32 + 5, &mut buf).unwrap(), 0);
        assert_eq!(fs.read_file(ino, u32::MAX, &mut buf).unwrap(), 0);
        // 部分越界 → 截断到文件尾
        let mut tail = [0u8; 8];
        let n = fs.read_file(ino, 6, &mut tail).unwrap();
        assert_eq!(&tail[..n], b"ext2");
        // 目录不能当文件读
        let root = fs.root();
        assert_eq!(fs.read_file(root, 0, &mut buf).unwrap_err(), FsError::IsADirectory);
    }

    #[test]
    fn damaged_magic_is_bad_magic() {
        let mut img = build_minimal_ext2();
        put16(&mut img, 1024 + 56, 0xDEAD);
        let err = Ext2Reader::mount(MockDevice::new(img, BS)).err().expect("应挂载失败");
        assert_eq!(err, FsError::BadMagic);
    }

    #[test]
    fn damaged_inode_number_is_corrupt() {
        let mut img = build_minimal_ext2();
        // 根目录里 hello.txt 条目在块内偏移 24，把它的 inode 号改成越界值。
        let off = ROOT_DIR_BLOCK * BS + 24;
        put32(&mut img, off, 9999);
        let mut fs = reader(img);
        assert_eq!(fs.lookup("/hello.txt").unwrap_err(), FsError::Corrupt);
        // 整个目录的 list 同样应报错而不是 panic
        let root = fs.root();
        let mut out = [DirEntry { inode: 0, name_len: 0, name: [0u8; 255] }; 8];
        assert_eq!(fs.list(root, &mut out).unwrap_err(), FsError::Corrupt);
    }

    #[test]
    fn dir_rec_len_zero_is_corrupt() {
        let mut img = build_minimal_ext2();
        // 根目录第一条（"."）rec_len = 0 → 遍历会成环。
        put16(&mut img, ROOT_DIR_BLOCK * BS + 4, 0);
        let mut fs = reader(img);
        assert_eq!(fs.lookup("/hello.txt").unwrap_err(), FsError::Corrupt);
        let root = fs.root();
        let mut out = [DirEntry { inode: 0, name_len: 0, name: [0u8; 255] }; 8];
        assert_eq!(fs.list(root, &mut out).unwrap_err(), FsError::Corrupt);
    }

    #[test]
    fn dir_rec_len_misaligned_is_corrupt() {
        let mut img = build_minimal_ext2();
        // 根目录第二项 ".." 的 rec_len 改成 13（未 4 字节对齐）。
        put16(&mut img, ROOT_DIR_BLOCK * BS + 12 + 4, 13);
        let mut fs = reader(img);
        assert_eq!(fs.lookup("/sub").unwrap_err(), FsError::Corrupt);
    }

    #[test]
    fn dir_name_len_too_long_is_corrupt() {
        let mut img = build_minimal_ext2();
        // "hello.txt" 条目在根目录块内偏移 24，rec_len 为 20（名字最多 12 字节），
        // 把 name_len 改成 200 → 名字伸进下一条目，必须判 Corrupt。
        let off = ROOT_DIR_BLOCK * BS + 24;
        img[off + 6] = 200;
        let mut fs = reader(img);
        assert_eq!(fs.lookup("/sub").unwrap_err(), FsError::Corrupt);

        // name_len 取 u8 上限 255、rec_len 只有 12 → 同样 Corrupt。
        let mut img = build_minimal_ext2();
        let off = ROOT_DIR_BLOCK * BS + 12; // ".." 条目
        img[off + 6] = 255;
        let mut fs = reader(img);
        assert_eq!(fs.lookup("/sub").unwrap_err(), FsError::Corrupt);
    }

    #[test]
    fn device_error_propagates() {
        let img = build_minimal_ext2();
        // 挂载时要读 inode 表（第 5 块）拿根 inode。
        let dev = FailingDevice { inner: MockDevice::new(img, BS), fail_lba: 5 };
        let err = Ext2Reader::mount(dev).err().expect("应挂载失败");
        assert_eq!(err, FsError::Io(IoError::DeviceError));
    }

    #[test]
    fn block_size_mismatch_is_bad_superblock() {
        let img = build_minimal_ext2();
        // 镜像里写的是 1 KiB 块，设备却宣称 4 KiB。
        let err = Ext2Reader::mount(MockDevice::new(img, 4096))
            .err()
            .expect("应挂载失败");
        assert_eq!(err, FsError::BadSuperblock);
    }

    #[test]
    fn read_file_across_block_boundary() {
        let mut img = build_minimal_ext2();
        // 把 hello.txt 改成 1028 字节、占两块：第 9 块全是 0xAA，第 11 块写 "tail"。
        let ino_off = INODE_TABLE_OFF + 3 * 128; // inode 4
        put32(&mut img, ino_off + 4, (BS + XDATA.len()) as u32);
        put32(&mut img, ino_off + 44, 11); // block[1]

        let b0 = HELLO_BLOCK * BS;
        for b in img[b0..b0 + BS].iter_mut() {
            *b = 0xAA;
        }
        let b1 = 11 * BS;
        img[b1..b1 + XDATA.len()].copy_from_slice(XDATA);

        let mut fs = reader(img);
        let ino = fs.lookup("/hello.txt").unwrap();
        assert_eq!(ino.size as usize, BS + XDATA.len());

        let mut out = [0u8; 8];
        let n = fs.read_file(ino, (BS - 4) as u32, &mut out).unwrap();
        assert_eq!(n, 8);
        assert_eq!(&out[..4], &[0xAA; 4]);
        assert_eq!(&out[4..], XDATA);

        // 一次读完整文件（1028 字节）也要对得上。
        let mut whole = [0u8; 1028];
        let n = fs.read_file(ino, 0, &mut whole).unwrap();
        assert_eq!(n, 1028);
        assert_eq!(&whole[..BS], &[0xAA; BS][..]);
        assert_eq!(&whole[BS..], XDATA);
    }

    #[test]
    fn path_with_extra_slashes() {
        let mut fs = reader(build_minimal_ext2());
        let a = fs.lookup("//sub///x.txt/").unwrap();
        let b = fs.lookup("/sub/x.txt").unwrap();
        assert_eq!(a.number, b.number);
        assert_eq!(fs.lookup("/").unwrap().number, 2);
    }

    // ------------------------------------------------------------ 符号链接

    const MODE_LNK: u16 = 0xA1FF; // 0o120777

    /// 把一个 inode 写成 ext2 的 "fast symlink"（目标直接放在 i_block 里）。
    fn write_fast_symlink(img: &mut [u8], ino: u32, target: &[u8]) {
        assert!(target.len() < 60, "fast symlink 上限 59 字节");
        let off = INODE_TABLE_OFF + (ino as usize - 1) * 128;
        put16(img, off, MODE_LNK);
        put16(img, off + 26, 1);
        put32(img, off + 4, target.len() as u32);
        put32(img, off + 28, 0); // 不占数据块
        for (i, b) in target.iter().enumerate() {
            img[off + 40 + i] = *b;
        }
    }

    /// 在最小镜像上挂两个符号链接：
    ///   /link   → "/sub/x.txt"（绝对目标）
    ///   /rel    → "sub/x.txt" （相对目标，相对根目录）
    fn build_with_links() -> Vec<u8> {
        let mut img = build_minimal_ext2();
        write_fast_symlink(&mut img, 6, b"/sub/x.txt");
        write_fast_symlink(&mut img, 7, b"sub/x.txt");
        write_dir(
            &mut img,
            ROOT_DIR_BLOCK,
            &[
                (2, b"."),
                (2, b".."),
                (4, b"hello.txt"),
                (3, b"sub"),
                (6, b"link"),
                (7, b"rel"),
            ],
        );
        img
    }

    #[test]
    fn symlink_is_followed_to_the_target() {
        let mut fs = reader(build_with_links());
        let ino = fs.lookup("/link").unwrap();
        assert_eq!(ino.number, 5, "应解析到 x.txt");
        let mut buf = [0u8; 16];
        let n = fs.read_file(ino, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], XDATA);
    }

    #[test]
    fn relative_symlink_resolves_from_its_own_directory() {
        let mut fs = reader(build_with_links());
        assert_eq!(fs.lookup("/rel").unwrap().number, 5);
    }

    #[test]
    fn nofollow_returns_the_link_itself() {
        let mut fs = reader(build_with_links());
        let link = fs.lookup_nofollow("/link").unwrap();
        assert_eq!(link.mode & S_IFMT, S_IFLNK);
        let mut tgt = [0u8; 64];
        let n = fs.read_link(link, &mut tgt).unwrap();
        assert_eq!(&tgt[..n], b"/sub/x.txt");
    }

    #[test]
    fn read_link_on_regular_file_is_rejected() {
        let mut fs = reader(build_with_links());
        let f = fs.lookup_nofollow("/hello.txt").unwrap();
        let mut tgt = [0u8; 64];
        assert_eq!(fs.read_link(f, &mut tgt).unwrap_err(), FsError::NotASymlink);
    }

    #[test]
    fn symlink_loop_is_detected_not_hung() {
        let mut img = build_with_links();
        // /loop → /loop：跟下去就是一个环
        write_fast_symlink(&mut img, 6, b"/loop");
        write_dir(
            &mut img,
            ROOT_DIR_BLOCK,
            &[(2, b"."), (2, b".."), (4, b"hello.txt"), (3, b"sub"), (6, b"loop")],
        );
        let mut fs = reader(img);
        assert_eq!(fs.lookup("/loop").unwrap_err(), FsError::Loop);
    }

    #[test]
    fn out_of_range_data_block_is_corrupt() {
        let mut img = build_minimal_ext2();
        // 把 hello.txt 的第一个块指针指到镜像之外。
        let off = INODE_TABLE_OFF + (4 - 1) * 128 + 40;
        put32(&mut img, off, 4096);
        let mut fs = reader(img);
        let ino = fs.lookup("/hello.txt").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(fs.read_file(ino, 0, &mut buf).unwrap_err(), FsError::Corrupt);
    }
    // ------------------------------------------------------------ fuzz（M11）

    /// 把最小镜像随机改几字节，然后**照常使用**它。
    ///
    /// 判定只有一条：不许 panic、不许越界读。返回 `FsError` 是**正确**行为——
    /// 文件系统遇到坏数据就该明确报错，而不是崩掉或者返回垃圾。
    ///
    /// 为什么这条比"随机字节"更有价值：随机字节连魔数都过不了，
    /// 解析器在第一个判断就返回了，跑不到危险的地方。改坏**合法**镜像的
    /// 某几个字节，才会把执行推到位图、间接块、目录项这些真正容易越界的地方。
    #[test]
    fn fuzz_corrupted_images_never_panic() {
        use kqa::rng::Rng;
        let mut rng = Rng::new(0x4B5F_0001);
        let empty = DirEntry {
            inode: 0,
            name_len: 0,
            name: [0u8; 255],
        };
        let mut mounted = 0usize;
        for _ in 0..800 {
            let mut img = build_minimal_ext2();
            let rounds = 1 + rng.below(16) as usize;
            kqa::fuzz::mutate(&mut img, &mut rng, rounds);
            let mut fs = match Ext2Reader::mount(MockDevice::new(img, BS)) {
                Ok(f) => f,
                Err(_) => continue, // 超级块被改坏：挂不上是正确行为
            };
            mounted += 1;

            let root = fs.root();
            let mut ents = [empty; 32];
            let _ = fs.list(root, &mut ents);

            let long = "/".to_string() + &"a".repeat(300);
            for path in [
                "/", "/hello.txt", "/sub", "/sub/x.txt", "/link", "/rel", "//a//b",
                "/\u{0}", long.as_str(),
            ] {
                if let Ok(ino) = fs.lookup(path) {
                    let mut buf = [0u8; 256];
                    let _ = fs.read_file(ino, 0, &mut buf);
                    let _ = fs.read_file(ino, 0xFFFF_F000, &mut buf);
                    let _ = fs.list(ino, &mut ents);
                }
                let _ = fs.lookup_nofollow(path);
            }
        }
        // 至少有一部分变异体还挂着，否则说明 mutate 把魔数打得太狠、这轮白跑
        assert!(mounted > 100, "只有 {mounted}/800 个变异体挂载成功，fuzz 覆盖不足");
    }

    /// 随机字节当镜像：解析器必须在**第一层校验**就拒绝，绝不允许越界。
    #[test]
    fn fuzz_random_images_never_panic() {
        use kqa::rng::Rng;
        let mut rng = Rng::new(0x4B5F_0002);
        for _ in 0..400 {
            let n = 1 + rng.below(4096) as usize;
            let mut img = Vec::with_capacity(n);
            img.resize(n, 0u8);
            rng.fill(&mut img);
            let _ = Ext2Reader::mount(MockDevice::new(img, BS));
        }
    }

}
