//! LumiFS —— 带 redo 日志的可写文件系统（M8）
//!
//! ## 为什么要有日志
//!
//! 改一个文件要动好几块：数据块、inode 块、超级块（空闲计数）。中途掉电，
//! 磁盘上就会出现"inode 说文件有 4 KiB、但第 3 块还是别人的数据"这种
//! **结构性损坏**——不是丢数据，是文件系统自己不自洽了。
//!
//! redo 日志的做法：先把"要写的每一块的新内容"写进日志区并提交（一条记录），
//! 再往真正的目标位置写。掉电后重新挂载时，只要看到一条**完整提交过**的日志，
//! 就把它的内容重放一遍。重放是幂等的（写整块），所以"重放了一半又掉电"
//! 也没关系。
//!
//! 这就是顺序：**日志 → 屏障 → 提交记录 → 屏障 → 就地写 → 屏障 → 清提交记录**。
//! 每一步之间的 `flush()` 都不能省：缓存没落盘时，"先写日志再写数据"在
//! 断电后可能变成反序，日志就等于没写。
//!
//! ## 格式（1 KiB 块，全部小端）
//!
//! ```text
//! LBA 0        超级块
//! LBA 1        日志头（magic + seq + n + 校验和 + n 条 {目标LBA, 载荷槽}）
//! LBA 2..8     日志载荷（7 个槽，单次事务最多 4 块）
//! LBA 9..12    inode 表（16 个/块 × 4 块 = 64 个 inode，每个 64 B）
//! LBA 13..15   保留
//! LBA 16..     数据块
//! ```
//!
//! ## 刻意的简化（都写在这里，别当成设计）
//!
//! - **扁平命名空间**：没有目录，文件名直接存在 inode 里（24 字节）。
//! - **每个文件最多 8 个直接块**（8 KiB），没有间接块。
//! - **单次事务最多 4 块**，所以单次写入有上限；更大的写入由调用方分多次提交，
//!   每次都是一个独立的原子事务（不会出现"半个文件"的中间态）。
//! - **没有权限、没有时间戳、没有硬链接**。
//!
//! 这些限制换来的是：整个文件系统 700 行、能在宿主机上跑完整的掉电测试，
//! 而不是一个"看起来像 ext2 但从来没在断电情况下验证过"的半成品。

#![cfg_attr(not(test), no_std)]

pub const BLOCK: usize = 1024;
pub const NAME_MAX: usize = 24;
pub const MAX_FILE_BLOCKS: usize = 8;
/// 单次事务最多几块。日志载荷槽是 7 个，这里留了余量。
pub const MAX_TXN: usize = 4;
/// 单次 `write_at` 最多写几块（要留块给超级块与 inode 块）。
pub const MAX_IO_BLOCKS: usize = 2;

pub const MAGIC: [u8; 8] = *b"LUMIFS01";
pub const J_MAGIC: [u8; 8] = *b"LUMIJRN1";

pub const SB_LBA: u64 = 0;
pub const J_HEADER_LBA: u64 = 1;
pub const J_PAYLOAD0: u64 = 2;
pub const J_PAYLOADS: usize = 7;
pub const INODE_LBA: u64 = 9;
pub const INODE_BLOCKS: u64 = 4;
pub const DATA_LBA: u64 = 16;
pub const INODE_COUNT: u32 = 64;
const INODE_SIZE: usize = 64;
const INODES_PER_BLOCK: usize = BLOCK / INODE_SIZE;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FsError {
    /// 设备读写失败（含"掉电"模拟）。
    Io,
    BadMagic,
    BadSuper,
    /// 块大小不是 1024，或容量太小放不下元数据。
    BadGeometry,
    /// 名字太长或为空。
    BadName,
    Exists,
    NotFound,
    /// 数据块用完了。
    NoSpace,
    /// 文件超过 8 个直接块。
    TooBig,
    /// 日志区不一致（提交记录 n 越界）。
    BadJournal,
}

/// 文件系统需要的块设备：**可写**（这是和 M5 的只读接口最大的区别）。
///
/// `flush` 有默认实现（什么都不做），但真实设备上**必须**实现它：
/// 日志的正确性依赖"日志先落盘、再写目标"这个顺序，没有屏障就没有顺序。
pub trait Disk {
    fn block_size(&self) -> usize;
    fn block_count(&self) -> u64;
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()>;
    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()>;
    fn flush(&mut self) -> Result<(), ()> {
        Ok(())
    }
}

/// 一个 inode 的快照（读出来后用这个，不要拿着块指针到处跑）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Inode {
    pub ino: u32,
    pub size: u32,
    pub used: bool,
    pub name_len: u8,
    pub name: [u8; NAME_MAX],
    pub blocks: [u32; MAX_FILE_BLOCKS],
}

impl Inode {
    pub const EMPTY: Inode = Inode {
        ino: 0,
        size: 0,
        used: false,
        name_len: 0,
        name: [0; NAME_MAX],
        blocks: [0; MAX_FILE_BLOCKS],
    };

    pub fn name_bytes(&self) -> &[u8] {
        let n = (self.name_len as usize).min(NAME_MAX);
        &self.name[..n]
    }
}

/// 目录项（`list` 的输出）。
#[derive(Clone, Copy)]
pub struct Entry {
    pub ino: u32,
    pub size: u32,
    pub name_len: u8,
    pub name: [u8; NAME_MAX],
}

impl Entry {
    pub const EMPTY: Entry = Entry {
        ino: 0,
        size: 0,
        name_len: 0,
        name: [0; NAME_MAX],
    };
    pub fn name_bytes(&self) -> &[u8] {
        let n = (self.name_len as usize).min(NAME_MAX);
        &self.name[..n]
    }
}

/// 挂载时的重放结果，供日志/自检打印。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Replay {
    /// 是否有日志被重放（true = 上次是"写到一半掉电"）。
    pub replayed: bool,
    /// 重放了几块。
    pub blocks: u32,
    /// 重放的事务序号。
    pub seq: u64,
}

impl Replay {
    pub const NONE: Replay = Replay {
        replayed: false,
        blocks: 0,
        seq: 0,
    };
}

#[derive(Clone, Copy)]
struct Sb {
    block_count: u64,
    inode_count: u32,
    free_blocks: u32,
    seq: u64,
}

/// 一次事务：待写的 (目标块, 新内容) 列表。
///
/// 内容留在栈上（4 KiB），不额外分配——但调用方要留神栈深度：
/// 内核栈 64 KiB，这个结构 4 KiB，放在 `mount` 附近没问题。
struct Txn {
    n: usize,
    targets: [u64; MAX_TXN],
    blocks: [[u8; BLOCK]; MAX_TXN],
}

impl Txn {
    fn new() -> Txn {
        Txn {
            n: 0,
            targets: [0; MAX_TXN],
            blocks: [[0u8; BLOCK]; MAX_TXN],
        }
    }

    fn push(&mut self, lba: u64, data: &[u8]) -> Result<(), FsError> {
        if self.n >= MAX_TXN || data.len() < BLOCK {
            return Err(FsError::NoSpace);
        }
        self.targets[self.n] = lba;
        self.blocks[self.n][..BLOCK].copy_from_slice(&data[..BLOCK]);
        self.n += 1;
        Ok(())
    }
}

fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn rd_u64(b: &[u8], off: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(v)
}

fn wr_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn wr_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// 整个块的字节和（校验和就是"再补一个值让总和为 0"，这里用直接比对）。
fn sum32(b: &[u8]) -> u32 {
    let mut s = 0u32;
    let mut i = 0;
    while i < b.len() {
        s = s.wrapping_add(b[i] as u32);
        i += 1;
    }
    s
}

pub struct LumiFs<D: Disk> {
    dev: D,
    sb: Sb,
}

impl<D: Disk> LumiFs<D> {
    /* ---------------- 挂载 / 格式化 ---------------- */

    /// 挂载。返回是否发生了日志重放。
    pub fn mount(mut dev: D) -> Result<(Self, Replay), FsError> {
        if dev.block_size() != BLOCK {
            return Err(FsError::BadGeometry);
        }
        let nblocks = dev.block_count();
        if nblocks < DATA_LBA + 1 {
            return Err(FsError::BadGeometry);
        }
        let mut buf = [0u8; BLOCK];
        dev.read(SB_LBA, &mut buf).map_err(|_| FsError::Io)?;
        let sb = Self::parse_sb(&buf)?;

        let mut fs = LumiFs { dev, sb };
        // 先重放日志，再返回：重放之前读到的任何元数据都可能是"写了一半"的。
        let rep = fs.replay()?;
        Ok((fs, rep))
    }

    /// 格式化：写超级块、清日志头、清 inode 表。
    ///
    /// 只给工具与自检用。真机上"格式化"是破坏性操作，不该藏在挂载路径里。
    pub fn format(mut dev: D) -> Result<Self, FsError> {
        if dev.block_size() != BLOCK {
            return Err(FsError::BadGeometry);
        }
        let nblocks = dev.block_count();
        if nblocks < DATA_LBA + 1 {
            return Err(FsError::BadGeometry);
        }
        let zero = [0u8; BLOCK];
        let mut sb = [0u8; BLOCK];
        sb[..8].copy_from_slice(&MAGIC);
        wr_u32(&mut sb, 8, 1); // version
        wr_u32(&mut sb, 12, BLOCK as u32);
        wr_u64(&mut sb, 16, nblocks);
        wr_u32(&mut sb, 24, INODE_COUNT);
        wr_u32(&mut sb, 28, INODE_LBA as u32);
        wr_u32(&mut sb, 32, DATA_LBA as u32);
        wr_u32(&mut sb, 36, J_HEADER_LBA as u32);
        wr_u32(&mut sb, 40, (1 + J_PAYLOADS) as u32);
        wr_u32(&mut sb, 44, (nblocks - DATA_LBA) as u32); // free_blocks
        wr_u64(&mut sb, 48, 0); // seq
        let c = sum32(&sb[..60]);
        wr_u32(&mut sb, 60, c);

        dev.write(SB_LBA, &sb).map_err(|_| FsError::Io)?;
        dev.write(J_HEADER_LBA, &zero).map_err(|_| FsError::Io)?;
        for i in 0..INODE_BLOCKS {
            dev.write(INODE_LBA + i, &zero).map_err(|_| FsError::Io)?;
        }
        dev.flush().map_err(|_| FsError::Io)?;

        Ok(LumiFs {
            dev,
            sb: Sb {
                block_count: nblocks,
                inode_count: INODE_COUNT,
                free_blocks: (nblocks - DATA_LBA) as u32,
                seq: 0,
            },
        })
    }

    fn parse_sb(b: &[u8]) -> Result<Sb, FsError> {
        if b[..8] != MAGIC {
            return Err(FsError::BadMagic);
        }
        if sum32(&b[..60]) != rd_u32(b, 60) {
            return Err(FsError::BadSuper);
        }
        let block_size = rd_u32(b, 12);
        if block_size as usize != BLOCK {
            return Err(FsError::BadGeometry);
        }
        let block_count = rd_u64(b, 16);
        let inode_count = rd_u32(b, 24);
        if rd_u32(b, 28) != INODE_LBA as u32
            || rd_u32(b, 32) != DATA_LBA as u32
            || rd_u32(b, 36) != J_HEADER_LBA as u32
            || inode_count != INODE_COUNT
            || block_count <= DATA_LBA
        {
            return Err(FsError::BadGeometry);
        }
        Ok(Sb {
            block_count,
            inode_count,
            free_blocks: rd_u32(b, 44),
            seq: rd_u64(b, 48),
        })
    }

    /* ---------------- 日志 ---------------- */

    fn journal_write_header(&mut self, seq: u64, n: usize, t: Option<&Txn>) -> Result<(), FsError> {
        let mut h = [0u8; BLOCK];
        if n > 0 {
            h[..8].copy_from_slice(&J_MAGIC);
            wr_u64(&mut h, 8, seq);
            wr_u32(&mut h, 16, n as u32);
            for i in 0..n {
                let off = 24 + i * 12;
                let tgt = match t {
                    Some(t) => t.targets[i],
                    None => return Err(FsError::BadJournal),
                };
                wr_u64(&mut h, off, tgt);
                wr_u32(&mut h, off + 8, (J_PAYLOAD0 as u32) + i as u32);
            }
        }
        // n == 0 时整块全零 = "没有待重放事务"
        let c = sum32(&h[..BLOCK - 4]);
        wr_u32(&mut h, BLOCK - 4, c);
        self.dev.write(J_HEADER_LBA, &h).map_err(|_| FsError::Io)
    }

    /// 提交一次事务。
    ///
    /// 顺序不能改：**载荷 → 屏障 → 提交记录 → 屏障 → 就地写 → 屏障 → 清记录**。
    /// 少了任何一个屏障，"先日志后数据"这个顺序在掉电后就可能反了。
    fn commit(&mut self, t: &mut Txn) -> Result<(), FsError> {
        if t.n == 0 {
            return Ok(());
        }
        // 1. 载荷写进日志区
        for i in 0..t.n {
            let lba = J_PAYLOAD0 + i as u64;
            let blk = t.blocks[i];
            self.dev.write(lba, &blk).map_err(|_| FsError::Io)?;
        }
        self.dev.flush().map_err(|_| FsError::Io)?;
        // 2. 提交记录（有它才算数）
        let seq = self.sb.seq.wrapping_add(1);
        self.journal_write_header(seq, t.n, Some(t))?;
        self.dev.flush().map_err(|_| FsError::Io)?;
        // 3. 落到真正的目标位置
        for i in 0..t.n {
            let lba = t.targets[i];
            let blk = t.blocks[i];
            self.dev.write(lba, &blk).map_err(|_| FsError::Io)?;
        }
        self.dev.flush().map_err(|_| FsError::Io)?;
        // 4. 清掉提交记录
        self.journal_write_header(0, 0, None)?;
        self.dev.flush().map_err(|_| FsError::Io)?;
        self.sb.seq = seq;
        t.n = 0;
        Ok(())
    }

    /// 挂载时重放。返回重放了几块。
    fn replay(&mut self) -> Result<Replay, FsError> {
        let mut h = [0u8; BLOCK];
        self.dev.read(J_HEADER_LBA, &mut h).map_err(|_| FsError::Io)?;
        if h[..8] != J_MAGIC {
            return Ok(Replay::NONE);
        }
        if sum32(&h[..BLOCK - 4]) != rd_u32(&h, BLOCK - 4) {
            // 提交记录本身被写坏了（撕裂）：按"没有事务"处理。
            // 这不是"丢了已提交的数据"——校验和不成立就说明这条记录没写完，
            // 那时还没开始就地写，磁盘还是旧状态。
            return Ok(Replay::NONE);
        }
        let seq = rd_u64(&h, 8);
        let n = rd_u32(&h, 16) as usize;
        if n == 0 || n > MAX_TXN {
            return Err(FsError::BadJournal);
        }
        for i in 0..n {
            let off = 24 + i * 12;
            let target = rd_u64(&h, off);
            let payload = rd_u32(&h, off + 8) as u64;
            if target >= self.sb.block_count || payload < J_PAYLOAD0 || payload >= J_PAYLOAD0 + J_PAYLOADS as u64 {
                return Err(FsError::BadJournal);
            }
            let mut blk = [0u8; BLOCK];
            self.dev.read(payload, &mut blk).map_err(|_| FsError::Io)?;
            self.dev.write(target, &blk).map_err(|_| FsError::Io)?;
        }
        self.dev.flush().map_err(|_| FsError::Io)?;
        self.journal_write_header(0, 0, None)?;
        self.dev.flush().map_err(|_| FsError::Io)?;
        // **必须重读超级块**：重放的内容里可能就包含新的超级块（空闲计数、seq）。
        // 只把块写到磁盘、却继续用内存里那份旧的，是最容易漏的一步——
        // 磁盘是对的，运行中的视图是错的，下一次写就把旧值又盖回去了。
        let mut sb_buf = [0u8; BLOCK];
        self.dev.read(SB_LBA, &mut sb_buf).map_err(|_| FsError::Io)?;
        self.sb = Self::parse_sb(&sb_buf)?;
        if seq > self.sb.seq {
            self.sb.seq = seq;
        }
        Ok(Replay {
            replayed: true,
            blocks: n as u32,
            seq,
        })
    }

    /* ---------------- inode ---------------- */

    fn inode_lba(ino: u32) -> u64 {
        INODE_LBA + (ino as u64) / INODES_PER_BLOCK as u64
    }

    fn inode_off(ino: u32) -> usize {
        (ino as usize % INODES_PER_BLOCK) * INODE_SIZE
    }

    fn decode_inode(ino: u32, b: &[u8], off: usize) -> Inode {
        let mut name = [0u8; NAME_MAX];
        let nl = b[off + 1].min(NAME_MAX as u8);
        name.copy_from_slice(&b[off + 2..off + 2 + NAME_MAX]);
        let mut blocks = [0u32; MAX_FILE_BLOCKS];
        for (i, slot) in blocks.iter_mut().enumerate() {
            *slot = rd_u32(b, off + 32 + i * 4);
        }
        Inode {
            ino,
            size: rd_u32(b, off + 28),
            used: b[off] != 0,
            name_len: nl,
            name,
            blocks,
        }
    }

    fn encode_inode(ino: Inode, b: &mut [u8], off: usize) {
        b[off] = if ino.used { 1 } else { 0 };
        b[off + 1] = ino.name_len;
        b[off + 2..off + 2 + NAME_MAX].copy_from_slice(&ino.name);
        wr_u32(b, off + 28, ino.size);
        for i in 0..MAX_FILE_BLOCKS {
            wr_u32(b, off + 32 + i * 4, ino.blocks[i]);
        }
    }

    /// 读一个 inode（要读它的整块）。
    fn get_inode(&mut self, ino: u32) -> Result<Inode, FsError> {
        if ino >= INODE_COUNT {
            return Err(FsError::NotFound);
        }
        let mut b = [0u8; BLOCK];
        self.dev.read(Self::inode_lba(ino), &mut b).map_err(|_| FsError::Io)?;
        let i = Self::decode_inode(ino, &b, Self::inode_off(ino));
        if i.used {
            Ok(i)
        } else {
            Err(FsError::NotFound)
        }
    }

    fn find_by_name(&mut self, name: &[u8]) -> Result<u32, FsError> {
        if name.is_empty() || name.len() > NAME_MAX {
            return Err(FsError::BadName);
        }
        let mut b = [0u8; BLOCK];
        for blk in 0..INODE_BLOCKS {
            self.dev.read(INODE_LBA + blk, &mut b).map_err(|_| FsError::Io)?;
            for k in 0..INODES_PER_BLOCK {
                let ino = (blk as u32) * INODES_PER_BLOCK as u32 + k as u32;
                let i = Self::decode_inode(ino, &b, k * INODE_SIZE);
                if i.used && i.name_bytes() == name {
                    return Ok(ino);
                }
            }
        }
        Err(FsError::NotFound)
    }

    /* ---------------- 对外操作 ---------------- */

    pub fn lookup(&mut self, name: &[u8]) -> Result<Inode, FsError> {
        let ino = self.find_by_name(name)?;
        self.get_inode(ino)
    }

    /// 创建一个空文件。已存在返回 `Exists`。
    pub fn create(&mut self, name: &[u8]) -> Result<Inode, FsError> {
        if name.is_empty() || name.len() > NAME_MAX {
            return Err(FsError::BadName);
        }
        if self.find_by_name(name).is_ok() {
            return Err(FsError::Exists);
        }
        // 找一个空 inode
        let mut b = [0u8; BLOCK];
        let mut free_ino = None;
        let mut free_off = 0usize;
        for blk in 0..INODE_BLOCKS {
            self.dev.read(INODE_LBA + blk, &mut b).map_err(|_| FsError::Io)?;
            for k in 0..INODES_PER_BLOCK {
                if b[k * INODE_SIZE] == 0 {
                    free_ino = Some((blk as u32) * INODES_PER_BLOCK as u32 + k as u32);
                    free_off = k * INODE_SIZE;
                    break;
                }
            }
            if free_ino.is_some() {
                break;
            }
        }
        let ino = free_ino.ok_or(FsError::NoSpace)?;

        let mut ni = Inode::EMPTY;
        ni.ino = ino;
        ni.used = true;
        ni.name_len = name.len() as u8;
        ni.name[..name.len()].copy_from_slice(name);
        Self::encode_inode(ni, &mut b, free_off);

        let mut t = Txn::new();
        t.push(Self::inode_lba(ino), &b)?;
        self.commit(&mut t)?;
        Ok(ni)
    }

    /// 写文件。返回实际写入的字节数（可能小于 `data.len()`：单次事务有上限）。
    ///
    /// 分配策略：按需从 `[DATA_LBA, block_count)` 里顺序找空块。
    /// 找空块靠"扫描 inode 表里已用的块"——64 个 inode × 8 块 = 512 次比较，
    /// 比维护一张位图简单，而且不可能与 inode 失去同步（位图和 inode 不一致
    /// 是文件系统最经典的一类 bug）。
    pub fn write_at(&mut self, ino: u32, off: u32, data: &[u8]) -> Result<usize, FsError> {
        if data.is_empty() {
            return Ok(0);
        }
        let mut node = self.get_inode(ino)?;
        let first = (off as usize) / BLOCK;
        let last = (off as usize + data.len() - 1) / BLOCK;
        if first >= MAX_FILE_BLOCKS {
            return Err(FsError::TooBig);
        }
        // 单次事务：超级块 + inode 块 + 数据块，所以数据块最多 MAX_IO_BLOCKS 个
        let want = (last - first + 1).min(MAX_IO_BLOCKS);
        let end_block = first + want;

        // 先把这一批数据块的**当前内容**读出来（不足的要补零），再打补丁，
        // 这样"往文件中间写"不会把同一块里别处的数据抹成 0。
        let mut blocks: [[u8; BLOCK]; MAX_IO_BLOCKS] = [[0u8; BLOCK]; MAX_IO_BLOCKS];
        let mut targets = [0u64; MAX_IO_BLOCKS];
        for (slot, bi) in (first..end_block).enumerate() {
            let lba = if node.blocks[bi] != 0 {
                node.blocks[bi] as u64
            } else {
                let fresh = self.alloc_block(&node)?;
                node.blocks[bi] = fresh as u32;
                self.sb.free_blocks = self.sb.free_blocks.saturating_sub(1);
                fresh
            };
            if node.size as usize > bi * BLOCK {
                self.dev.read(lba, &mut blocks[slot]).map_err(|_| FsError::Io)?;
            }
            targets[slot] = lba;
        }

        // 计算这次真正能写多少字节
        let block_end_limit = end_block * BLOCK;
        let mut n = data.len();
        if (off as usize) + n > block_end_limit {
            n = block_end_limit - off as usize;
        }
        for i in 0..n {
            let abs = off as usize + i;
            let bi = abs / BLOCK - first;
            let bo = abs % BLOCK;
            blocks[bi][bo] = data[i];
        }

        let new_size = (off as usize + n).max(node.size as usize);
        node.size = new_size as u32;

        let mut t = Txn::new();
        for slot in 0..(end_block - first) {
            t.push(targets[slot], &blocks[slot])?;
        }
        // inode 块：要基于**当前**内容改，避免把同一块里别的 inode 覆盖掉
        let mut ib = [0u8; BLOCK];
        self.dev.read(Self::inode_lba(ino), &mut ib).map_err(|_| FsError::Io)?;
        Self::encode_inode(node, &mut ib, Self::inode_off(ino));
        t.push(Self::inode_lba(ino), &ib)?;
        // 超级块（空闲块计数）
        let sbb = self.encode_sb();
        t.push(SB_LBA, &sbb)?;
        self.commit(&mut t)?;
        Ok(n)
    }

    /// 在文件末尾追加。
    pub fn append(&mut self, ino: u32, data: &[u8]) -> Result<usize, FsError> {
        let node = self.get_inode(ino)?;
        // 已经写满 8 个块：追加 0 字节，而不是报 TooBig。
        // 调用方按"短写"处理就够了（POSIX 里超限也是短写或 ENOSPC）。
        if node.size as usize >= MAX_FILE_BLOCKS * BLOCK {
            return Ok(0);
        }
        self.write_at(ino, node.size, data)
    }

    pub fn read_at(&mut self, ino: u32, off: u32, out: &mut [u8]) -> Result<usize, FsError> {
        let node = self.get_inode(ino)?;
        if off >= node.size {
            return Ok(0);
        }
        let avail = node.size as usize - off as usize;
        let n = avail.min(out.len());
        let mut done = 0usize;
        let mut blk = [0u8; BLOCK];
        while done < n {
            let abs = off as usize + done;
            let bi = abs / BLOCK;
            let bo = abs % BLOCK;
            if bi >= MAX_FILE_BLOCKS || node.blocks[bi] == 0 {
                // 稀疏区（不该出现，除非 inode 被外部改坏）：返回 0 填充
                let take = (BLOCK - bo).min(n - done);
                for i in 0..take {
                    out[done + i] = 0;
                }
                done += take;
                continue;
            }
            self.dev
                .read(node.blocks[bi] as u64, &mut blk)
                .map_err(|_| FsError::Io)?;
            let take = (BLOCK - bo).min(n - done);
            out[done..done + take].copy_from_slice(&blk[bo..bo + take]);
            done += take;
        }
        Ok(n)
    }

    /// 删除文件（同时归还它的数据块）。
    pub fn unlink(&mut self, name: &[u8]) -> Result<(), FsError> {
        let ino = self.find_by_name(name)?;
        let node = self.get_inode(ino)?;
        let mut ib = [0u8; BLOCK];
        self.dev.read(Self::inode_lba(ino), &mut ib).map_err(|_| FsError::Io)?;
        Self::encode_inode(Inode::EMPTY, &mut ib, Self::inode_off(ino));
        let freed = node.blocks.iter().filter(|b| **b != 0).count() as u32;
        self.sb.free_blocks = self.sb.free_blocks.saturating_add(freed);
        let sbb = self.encode_sb();
        let mut t = Txn::new();
        t.push(Self::inode_lba(ino), &ib)?;
        t.push(SB_LBA, &sbb)?;
        self.commit(&mut t)?;
        Ok(())
    }

    /// 列目录（扁平命名空间：就是所有已用的 inode）。
    pub fn list(&mut self, out: &mut [Entry]) -> Result<usize, FsError> {
        let mut n = 0usize;
        let mut b = [0u8; BLOCK];
        for blk in 0..INODE_BLOCKS {
            self.dev.read(INODE_LBA + blk, &mut b).map_err(|_| FsError::Io)?;
            for k in 0..INODES_PER_BLOCK {
                let ino = (blk as u32) * INODES_PER_BLOCK as u32 + k as u32;
                let i = Self::decode_inode(ino, &b, k * INODE_SIZE);
                if !i.used {
                    continue;
                }
                if n >= out.len() {
                    return Ok(n);
                }
                out[n] = Entry {
                    ino,
                    size: i.size,
                    name_len: i.name_len,
                    name: i.name,
                };
                n += 1;
            }
        }
        Ok(n)
    }

    fn encode_sb(&self) -> [u8; BLOCK] {
        let mut sb = [0u8; BLOCK];
        sb[..8].copy_from_slice(&MAGIC);
        wr_u32(&mut sb, 8, 1);
        wr_u32(&mut sb, 12, BLOCK as u32);
        wr_u64(&mut sb, 16, self.sb.block_count);
        wr_u32(&mut sb, 24, self.sb.inode_count);
        wr_u32(&mut sb, 28, INODE_LBA as u32);
        wr_u32(&mut sb, 32, DATA_LBA as u32);
        wr_u32(&mut sb, 36, J_HEADER_LBA as u32);
        wr_u32(&mut sb, 40, (1 + J_PAYLOADS) as u32);
        wr_u32(&mut sb, 44, self.sb.free_blocks);
        wr_u64(&mut sb, 48, self.sb.seq);
        let c = sum32(&sb[..60]);
        wr_u32(&mut sb, 60, c);
        sb
    }

    /// 找一块没人用的数据块。
    fn alloc_block(&mut self, skip: &Inode) -> Result<u64, FsError> {
        if self.sb.free_blocks == 0 {
            return Err(FsError::NoSpace);
        }
        let mut b = [0u8; BLOCK];
        let mut cand = DATA_LBA;
        while cand < self.sb.block_count {
            // 拿候选块和所有 inode 的块指针比一遍
            let mut used = false;
            for blk in 0..INODE_BLOCKS {
                self.dev.read(INODE_LBA + blk, &mut b).map_err(|_| FsError::Io)?;
                for k in 0..INODES_PER_BLOCK {
                    let i = Self::decode_inode(0, &b, k * INODE_SIZE);
                    if !i.used {
                        continue;
                    }
                    for p in i.blocks.iter() {
                        if *p as u64 == cand {
                            used = true;
                            break;
                        }
                    }
                    if used {
                        break;
                    }
                }
                if used {
                    break;
                }
            }
            if !used {
                // 同一批写入里刚分配的块还没落盘，inode 里的指针是内存里的副本，
                // 所以要排除本次已分配的那几个（skip）。
                if !skip.blocks.iter().any(|p| *p as u64 == cand && *p != 0) {
                    return Ok(cand);
                }
            }
            cand += 1;
        }
        Err(FsError::NoSpace)
    }

    /// **自检专用**：把一个"已经提交、但还没就地写"的事务留在日志里。
    ///
    /// 这就是掉电窗口里磁盘上的样子。只写日志载荷与提交记录，不碰目标块。
    /// 下次 `mount` 必须把它重放出来——否则"日志能救数据"这句话就没有证据。
    ///
    /// 真实掉电无法在单机上稳定复现，所以用这个函数把磁盘**摆成**那个状态，
    /// 剩下的交给真正的重放路径。除了这个函数本身，没有别的捷径。
    pub fn arm_crash_after_commit(&mut self, ino: u32, new_data: &[u8]) -> Result<u64, FsError> {
        if new_data.is_empty() || new_data.len() > BLOCK {
            return Err(FsError::TooBig);
        }
        let node = self.get_inode(ino)?;
        if node.blocks[0] == 0 {
            return Err(FsError::NotFound);
        }
        // 载荷 0：数据块（新的文件前 1 KiB）
        let mut db = [0u8; BLOCK];
        db[..new_data.len()].copy_from_slice(new_data);
        // 载荷 1：inode 块（大小改成新的）
        let mut ib = [0u8; BLOCK];
        self.dev.read(Self::inode_lba(ino), &mut ib).map_err(|_| FsError::Io)?;
        let mut ni = node;
        ni.size = new_data.len() as u32;
        Self::encode_inode(ni, &mut ib, Self::inode_off(ino));

        self.dev.write(J_PAYLOAD0, &db).map_err(|_| FsError::Io)?;
        self.dev.write(J_PAYLOAD0 + 1, &ib).map_err(|_| FsError::Io)?;
        self.dev.flush().map_err(|_| FsError::Io)?;

        let seq = self.sb.seq.wrapping_add(1);
        let mut h = [0u8; BLOCK];
        h[..8].copy_from_slice(&J_MAGIC);
        wr_u64(&mut h, 8, seq);
        wr_u32(&mut h, 16, 2);
        wr_u64(&mut h, 24, node.blocks[0] as u64);
        wr_u32(&mut h, 32, J_PAYLOAD0 as u32);
        wr_u64(&mut h, 36, Self::inode_lba(ino));
        wr_u32(&mut h, 44, (J_PAYLOAD0 + 1) as u32);
        let c = sum32(&h[..BLOCK - 4]);
        wr_u32(&mut h, BLOCK - 4, c);
        self.dev.write(J_HEADER_LBA, &h).map_err(|_| FsError::Io)?;
        self.dev.flush().map_err(|_| FsError::Io)?;
        self.sb.seq = seq;
        Ok(seq)
    }

    /// 落盘（把缓存里的脏块刷出去）。有块缓存时这一步是真动作。
    pub fn sync(&mut self) -> Result<(), FsError> {
        self.dev.flush().map_err(|_| FsError::Io)?;
        let sbb = self.encode_sb();
        self.dev.write(SB_LBA, &sbb).map_err(|_| FsError::Io)?;
        self.dev.flush().map_err(|_| FsError::Io)
    }

    pub fn free_blocks(&self) -> u32 {
        self.sb.free_blocks
    }

    pub fn journal_seq(&self) -> u64 {
        self.sb.seq
    }

    pub fn block_count(&self) -> u64 {
        self.sb.block_count
    }

    pub fn disk_mut(&mut self) -> &mut D {
        &mut self.dev
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 内存盘 + 掉电注入。
    ///
    /// `crash_at = Some(n)` 表示"第 n 次写调用开始时断电"：这一次写**不生效**，
    /// 之后的写也全部失败。用它把每个可能的写边界都当一次崩溃点来验证。
    struct Sim {
        data: Rc<RefCell<Vec<u8>>>,
        writes: Rc<RefCell<Vec<(u64, usize)>>>,
        crash_at: Option<usize>,
        crashed: bool,
        flushes: usize,
    }

    impl Sim {
        fn new(blocks: u64) -> Self {
            Sim {
                data: Rc::new(RefCell::new(vec![0u8; (blocks as usize) * BLOCK])),
                writes: Rc::new(RefCell::new(Vec::new())),
                crash_at: None,
                crashed: false,
                flushes: 0,
            }
        }
    }

    impl Disk for Sim {
        fn block_size(&self) -> usize {
            BLOCK
        }
        fn block_count(&self) -> u64 {
            (self.data.borrow().len() / BLOCK) as u64
        }
        fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
            let d = self.data.borrow();
            let off = (lba as usize) * BLOCK;
            if off + BLOCK > d.len() || buf.len() < BLOCK {
                return Err(());
            }
            buf[..BLOCK].copy_from_slice(&d[off..off + BLOCK]);
            Ok(())
        }
        fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
            let n = self.writes.borrow().len();
            if let Some(c) = self.crash_at {
                if n >= c {
                    self.crashed = true;
                    return Err(());
                }
            }
            self.writes.borrow_mut().push((lba, buf.len()));
            let mut d = self.data.borrow_mut();
            let off = (lba as usize) * BLOCK;
            if off + BLOCK > d.len() || buf.len() < BLOCK {
                return Err(());
            }
            d[off..off + BLOCK].copy_from_slice(&buf[..BLOCK]);
            Ok(())
        }
        fn flush(&mut self) -> Result<(), ()> {
            self.flushes += 1;
            Ok(())
        }
    }

    const N: u64 = 256;

    fn fs_new() -> (LumiFs<Sim>, Rc<RefCell<Vec<u8>>>) {
        let mut dev = Sim::new(N);
        // 直接调用 format 后再用同一份 data 造一个新的 Sim
        let _ = LumiFs::format(Sim {
            data: dev.data.clone(),
            writes: dev.writes.clone(),
            crash_at: None,
            crashed: false,
            flushes: 0,
        })
        .unwrap();
        dev.writes.borrow_mut().clear();
        let fs = LumiFs::mount(dev).unwrap().0;
        (fs, Rc::new(RefCell::new(Vec::new())))
    }

    #[test]
    fn format_writes_superblock_and_mounts() {
        let dev = Sim::new(N);
        let fs = LumiFs::format(dev).unwrap();
        assert_eq!(fs.block_count(), N);
        assert_eq!(fs.free_blocks(), (N - DATA_LBA) as u32);
    }

    #[test]
    fn empty_disk_is_not_formatted() {
        let dev = Sim::new(N);
        assert_eq!(LumiFs::mount(dev).err(), Some(FsError::BadMagic));
    }

    #[test]
    fn small_disk_is_rejected() {
        let dev = Sim::new(8);
        assert_eq!(LumiFs::format(dev).err(), Some(FsError::BadGeometry));
    }

    #[test]
    fn bad_checksum_is_rejected() {
        let mut dev = Sim::new(N);
        let _ = LumiFs::format(Sim {
            data: dev.data.clone(),
            writes: dev.writes.clone(),
            crash_at: None,
            crashed: false,
            flushes: 0,
        })
        .unwrap();
        dev.data.borrow_mut()[40] ^= 0xFF; // 改坏超级块里的一个字节
        assert_eq!(LumiFs::mount(dev).err(), Some(FsError::BadSuper));
    }

    #[test]
    fn create_lookup_and_list() {
        let (mut fs, _) = fs_new();
        let ino = fs.create(b"a.txt").unwrap();
        assert_eq!(ino.size, 0);
        assert_eq!(fs.lookup(b"a.txt").unwrap().ino, ino.ino);
        let mut ents = [Entry::EMPTY; 8];
        assert_eq!(fs.list(&mut ents).unwrap(), 1);
        assert_eq!(ents[0].name_bytes(), b"a.txt");
        assert_eq!(fs.create(b"a.txt").err(), Some(FsError::Exists));
        assert_eq!(fs.lookup(b"nope").err(), Some(FsError::NotFound));
    }

    #[test]
    fn name_limits() {
        let (mut fs, _) = fs_new();
        assert_eq!(fs.create(b"").err(), Some(FsError::BadName));
        let long = [b'x'; NAME_MAX + 1];
        assert_eq!(fs.create(&long).err(), Some(FsError::BadName));
        let exact = [b'y'; NAME_MAX];
        assert!(fs.create(&exact).is_ok());
    }

    #[test]
    fn write_then_read_roundtrip() {
        let (mut fs, _) = fs_new();
        let ino = fs.create(b"f").unwrap().ino;
        let data = *b"hello lumifs";
        assert_eq!(fs.write_at(ino, 0, &data).unwrap(), data.len());
        let mut buf = [0u8; 32];
        let n = fs.read_at(ino, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], &data);
        assert_eq!(fs.lookup(b"f").unwrap().size, data.len() as u32);
    }

    #[test]
    fn partial_write_in_middle_keeps_neighbours() {
        let (mut fs, _) = fs_new();
        let ino = fs.create(b"f").unwrap().ino;
        let block = [b'A'; BLOCK];
        fs.write_at(ino, 0, &block).unwrap();
        // 往中间盖 4 个字节，前后必须还是 'A'
        fs.write_at(ino, 100, b"ZZZZ").unwrap();
        let mut buf = [0u8; BLOCK];
        let n = fs.read_at(ino, 0, &mut buf).unwrap();
        assert_eq!(n, BLOCK);
        assert_eq!(&buf[..100], &[b'A'; 100]);
        assert_eq!(&buf[100..104], b"ZZZZ");
        assert_eq!(&buf[104..], &[b'A'; BLOCK - 104]);
    }

    #[test]
    fn read_past_eof_returns_zero() {
        let (mut fs, _) = fs_new();
        let ino = fs.create(b"f").unwrap().ino;
        fs.write_at(ino, 0, b"abc").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(fs.read_at(ino, 3, &mut buf).unwrap(), 0);
        assert_eq!(fs.read_at(ino, 100, &mut buf).unwrap(), 0);
    }

    #[test]
    fn read_short_buffer_is_clamped() {
        let (mut fs, _) = fs_new();
        let ino = fs.create(b"f").unwrap().ino;
        fs.write_at(ino, 0, &[b'q'; 200]).unwrap();
        let mut buf = [0u8; 10];
        assert_eq!(fs.read_at(ino, 0, &mut buf).unwrap(), 10);
        assert_eq!(&buf[..], &[b'q'; 10]);
    }

    #[test]
    fn append_grows_file_and_short_writes_are_reported() {
        let (mut fs, _) = fs_new();
        let ino = fs.create(b"f").unwrap().ino;
        let chunk = [b'1'; 512];
        let mut total = 0;
        loop {
            let n = fs.append(ino, &chunk).unwrap();
            if n == 0 {
                break;
            }
            total += n;
            if total > 8 * BLOCK {
                break;
            }
        }
        assert_eq!(total, MAX_FILE_BLOCKS * BLOCK);
        let e = fs.lookup(b"f").unwrap();
        assert_eq!(e.size as usize, MAX_FILE_BLOCKS * BLOCK);
        assert_eq!(e.blocks.iter().filter(|b| **b != 0).count(), MAX_FILE_BLOCKS);
    }

    #[test]
    fn file_cannot_exceed_eight_blocks() {
        let (mut fs, _) = fs_new();
        let ino = fs.create(b"f").unwrap().ino;
        let mut total = 0usize;
        loop {
            match fs.write_at(ino, total as u32, &[b'x'; BLOCK]) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(FsError::TooBig) => break,
                Err(e) => panic!("意外错误 {:?}", e),
            }
            if total > 64 * BLOCK {
                break;
            }
        }
        assert_eq!(total, MAX_FILE_BLOCKS * BLOCK);
    }

    #[test]
    fn unlink_frees_blocks_and_name() {
        let (mut fs, _) = fs_new();
        let free0 = fs.free_blocks();
        let ino = fs.create(b"gone").unwrap().ino;
        fs.write_at(ino, 0, &[b'z'; BLOCK]).unwrap();
        assert!(fs.free_blocks() < free0);
        fs.unlink(b"gone").unwrap();
        assert_eq!(fs.free_blocks(), free0);
        assert_eq!(fs.lookup(b"gone").err(), Some(FsError::NotFound));
        let mut ents = [Entry::EMPTY; 4];
        assert_eq!(fs.list(&mut ents).unwrap(), 0);
    }

    #[test]
    fn data_survives_remount() {
        let blocks = 64u64;
        let data = Rc::new(RefCell::new(vec![0u8; (blocks as usize) * BLOCK]));
        let mk = |data: &Rc<RefCell<Vec<u8>>>, crash: Option<usize>| Sim {
            data: data.clone(),
            writes: Rc::new(RefCell::new(Vec::new())),
            crash_at: crash,
            crashed: false,
            flushes: 0,
        };
        {
            let fs = LumiFs::format(mk(&data, None)).unwrap();
            drop(fs);
        }
        {
            let mut fs = LumiFs::mount(mk(&data, None)).unwrap().0;
            let ino = fs.create(b"persist").unwrap().ino;
            fs.write_at(ino, 0, b"written before remount").unwrap();
            fs.sync().unwrap();
        }
        {
            let (mut fs, rep) = LumiFs::mount(mk(&data, None)).unwrap();
            assert!(!rep.replayed, "正常卸载后不该有日志要重放");
            assert_eq!(fs.lookup(b"persist").unwrap().size, 22);
            let mut buf = [0u8; 32];
            let ino = fs.lookup(b"persist").unwrap().ino;
            let n = fs.read_at(ino, 0, &mut buf).unwrap();
            assert_eq!(&buf[..n], b"written before remount");
        }
    }

    /// 核心测试：把每一次写调用都当成一次掉电点，重放后状态必须是
    /// "旧状态" 或 "新状态"，绝不能是中间态。
    #[test]
    fn crash_at_every_write_is_atomic() {
        let blocks = 64u64;
        // 基准：整体成功的镜像 + 每一步后的期望状态
        let base = Rc::new(RefCell::new(vec![0u8; (blocks as usize) * BLOCK]));
        let mk = |data: &Rc<RefCell<Vec<u8>>>, crash: Option<usize>| Sim {
            data: data.clone(),
            writes: Rc::new(RefCell::new(Vec::new())),
            crash_at: crash,
            crashed: false,
            flushes: 0,
        };
        {
            let fs = LumiFs::format(mk(&base, None)).unwrap();
            drop(fs);
        }
        // 预置一个"上一个事务已完成"的状态
        {
            let mut fs = LumiFs::mount(mk(&base, None)).unwrap().0;
            fs.create(b"old").unwrap();
            let oi = fs.lookup(b"old").unwrap().ino;
            fs.write_at(oi, 0, b"OLD").unwrap();
            fs.sync().unwrap();
        }

        // 目标操作：新建 f 并写入 1500 字节（跨 2 块，一次事务上限）
        let payload = [b'N'; 1500];

        // 数一下完整执行一次操作需要多少次写
        let data2 = base.clone();
        let mut counter = mk(&data2, None);
        let w0 = counter.writes.clone();
        {
            let mut fs = LumiFs::mount(counter).unwrap().0;
            fs.create(b"f").unwrap();
            let fi = fs.lookup(b"f").unwrap().ino;
            fs.write_at(fi, 0, &payload).unwrap();
            fs.sync().unwrap();
        }
        let nwrites = w0.borrow().len();
        assert!(nwrites > 4, "一次操作至少要写日志+数据+inode+sb");

        for k in 0..nwrites {
            let d = base.clone();
            let mut sim = mk(&d, Some(k));
            let mounted = LumiFs::mount(sim);
            let (mut fs, rep) = match mounted {
                Ok(v) => v,
                Err(e) => panic!("第 {} 次写掉电后连挂载都失败：{:?}", k, e),
            };
            let _ = rep;
            // 状态必须是二选一：要么 f 不存在（旧），要么 f 完整（新）
            match fs.lookup(b"f") {
                Err(FsError::NotFound) => {}
                Ok(e) => {
                    assert_eq!(
                        e.size as usize,
                        payload.len(),
                        "第 {} 次写掉电后 f 存在但大小是 {}（中间态！）",
                        k,
                        e.size
                    );
                    let mut buf = [0u8; 1500];
                    let n = fs.read_at(e.ino, 0, &mut buf).unwrap();
                    assert_eq!(n, payload.len());
                    assert_eq!(&buf[..n], &payload[..], "第 {} 次写掉电后内容残缺", k);
                }
                Err(other) => panic!("第 {} 次写掉电后查 f 出错 {:?}", k, other),
            }
            // 上一轮的文件必须还在
            assert!(fs.lookup(b"old").is_ok(), "第 {} 次写掉电把旧文件弄丢了", k);
        }
    }

    #[test]
    fn journal_replay_is_idempotent() {
        let blocks = 64u64;
        let data = Rc::new(RefCell::new(vec![0u8; (blocks as usize) * BLOCK]));
        let mk = |data: &Rc<RefCell<Vec<u8>>>| Sim {
            data: data.clone(),
            writes: Rc::new(RefCell::new(Vec::new())),
            crash_at: None,
            crashed: false,
            flushes: 0,
        };
        let fs = LumiFs::format(mk(&data)).unwrap();
        drop(fs);
        // 手工造一条"已提交但没落地"的日志：把超级块的新内容塞进日志
        {
            let mut fs = LumiFs::mount(mk(&data)).unwrap().0;
            fs.create(b"x").unwrap();
            fs.sync().unwrap();
        }
        let mut raw = Sim {
            data: data.clone(),
            writes: Rc::new(RefCell::new(Vec::new())),
            crash_at: None,
            crashed: false,
            flushes: 0,
        };
        // 取当前超级块，改 free_blocks，写进日志载荷槽 0，再造一条提交记录
        let mut sb = [0u8; BLOCK];
        raw.read(SB_LBA, &mut sb).unwrap();
        let orig_free = rd_u32(&sb, 44);
        wr_u32(&mut sb, 44, orig_free + 7);
        let c = sum32(&sb[..60]);
        wr_u32(&mut sb, 60, c);
        raw.write(J_PAYLOAD0, &sb).unwrap();
        let mut h = [0u8; BLOCK];
        h[..8].copy_from_slice(&J_MAGIC);
        wr_u64(&mut h, 8, 99);
        wr_u32(&mut h, 16, 1);
        wr_u64(&mut h, 24, SB_LBA);
        wr_u32(&mut h, 32, J_PAYLOAD0 as u32);
        let hc = sum32(&h[..BLOCK - 4]);
        wr_u32(&mut h, BLOCK - 4, hc);
        raw.write(J_HEADER_LBA, &h).unwrap();

        let (fs1, rep1) = LumiFs::mount(raw).unwrap();
        assert!(rep1.replayed);
        assert_eq!(rep1.blocks, 1);
        assert_eq!(fs1.free_blocks(), orig_free + 7);
        // 重放完提交记录应被清掉，再挂一次不该重放
        let raw2 = Sim {
            data: data.clone(),
            writes: Rc::new(RefCell::new(Vec::new())),
            crash_at: None,
            crashed: false,
            flushes: 0,
        };
        let (fs2, rep2) = LumiFs::mount(raw2).unwrap();
        assert!(!rep2.replayed);
        assert_eq!(fs2.free_blocks(), orig_free + 7);
    }

    #[test]
    fn corrupt_journal_header_is_ignored_or_rejected() {
        let blocks = 64u64;
        let data = Rc::new(RefCell::new(vec![0u8; (blocks as usize) * BLOCK]));
        let mk = |data: &Rc<RefCell<Vec<u8>>>| Sim {
            data: data.clone(),
            writes: Rc::new(RefCell::new(Vec::new())),
            crash_at: None,
            crashed: false,
            flushes: 0,
        };
        let _ = LumiFs::format(mk(&data)).unwrap();
        // 校验和错 → 当作没有事务（还没开始就地写，磁盘是旧状态，安全）
        let mut raw = Sim {
            data: data.clone(),
            writes: Rc::new(RefCell::new(Vec::new())),
            crash_at: None,
            crashed: false,
            flushes: 0,
        };
        let mut h = [0u8; BLOCK];
        h[..8].copy_from_slice(&J_MAGIC);
        wr_u64(&mut h, 8, 5);
        wr_u32(&mut h, 16, 1);
        raw.write(J_HEADER_LBA, &h).unwrap(); // 故意不写校验和
        let (_fs, rep) = LumiFs::mount(raw).unwrap();
        assert!(!rep.replayed);
    }

    #[test]
    fn too_many_inodes_is_nospace() {
        let blocks = 64u64;
        let data = Rc::new(RefCell::new(vec![0u8; (blocks as usize) * BLOCK]));
        let mut raw = Sim {
            data: data.clone(),
            writes: Rc::new(RefCell::new(Vec::new())),
            crash_at: None,
            crashed: false,
            flushes: 0,
        };
        let mut fs = LumiFs::format(Sim {
            data: raw.data.clone(),
            writes: raw.writes.clone(),
            crash_at: None,
            crashed: false,
            flushes: 0,
        })
        .unwrap();
        for i in 0..INODE_COUNT {
            let mut name = [b'f'; 8];
            name[7] = b'0' + (i % 10) as u8;
            name[6] = b'0' + ((i / 10) % 10) as u8;
            fs.create(&name[..]).unwrap();
        }
        assert_eq!(fs.create(b"overflow").err(), Some(FsError::NoSpace));
        let _ = raw;
    }

    #[test]
    fn no_space_when_data_blocks_exhausted() {
        // 只有 20 块数据（DATA_LBA=16，共 36 块）。每个文件最多 8 块，
        // 所以两个文件之后第三个必然写不满——用这个来撞 NoSpace。
        let blocks = 36u64;
        let data = Rc::new(RefCell::new(vec![0u8; (blocks as usize) * BLOCK]));
        let mut fs = LumiFs::format(Sim {
            data: data.clone(),
            writes: Rc::new(RefCell::new(Vec::new())),
            crash_at: None,
            crashed: false,
            flushes: 0,
        })
        .unwrap();
        let mut names = [b'a', b'b', b'c'];
        let mut hit_nospace = false;
        for name in names.iter_mut() {
            let n = [*name];
            let ino = fs.create(&n).unwrap().ino;
            let mut total = 0usize;
            for _ in 0..MAX_FILE_BLOCKS {
                match fs.write_at(ino, total as u32, &[b'x'; BLOCK]) {
                    Ok(0) => break,
                    Ok(k) => total += k,
                    Err(FsError::NoSpace) => {
                        hit_nospace = true;
                        break;
                    }
                    Err(e) => panic!("意外错误 {:?}", e),
                }
            }
            if hit_nospace {
                break;
            }
        }
        assert!(hit_nospace, "20 块数据装不下三个 8 块文件，应该撞到 NoSpace");
    }

    #[test]
    fn flush_is_used_at_barriers() {
        // 一次事务至少要 4 次屏障（载荷/提交/就地/清记录）
        let blocks = 64u64;
        let data = Rc::new(RefCell::new(vec![0u8; (blocks as usize) * BLOCK]));
        let mut dev = Sim {
            data: data.clone(),
            writes: Rc::new(RefCell::new(Vec::new())),
            crash_at: None,
            crashed: false,
            flushes: 0,
        };
        {
            let _ = LumiFs::format(Sim {
                data: dev.data.clone(),
                writes: dev.writes.clone(),
                crash_at: None,
                crashed: false,
                flushes: 0,
            })
            .unwrap();
        }
        let mut fs = LumiFs::mount(Sim {
            data: dev.data.clone(),
            writes: dev.writes.clone(),
            crash_at: None,
            crashed: false,
            flushes: 0,
        })
        .unwrap()
        .0;
        let base = 0;
        let _ = base;
        let ino = fs.create(b"f").unwrap().ino;
        let before = fs.disk_mut().flushes;
        fs.write_at(ino, 0, b"x").unwrap();
        let after = fs.disk_mut().flushes;
        assert!(after - before >= 4, "屏障次数 {} 太少", after - before);
        dev.flushes = after;
    }
}
