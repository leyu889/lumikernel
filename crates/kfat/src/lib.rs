//! FAT32 只读驱动。
//!
//! ## 为什么需要它
//!
//! ext2 是我们的系统盘格式，LumiFS 是自研的可写盘——但**真实世界的 U 盘
//! 和 SD 卡几乎全是 FAT32**。没有 FAT32，插上 U 盘也读不出东西。
//!
//! ## 实现范围（诚实标注）
//!
//! - **只读**：写 FAT32 需要维护两份 FAT 副本 + 空闲簇链，风险远大于收益
//!   （换盘格式比写驱动简单）。要写东西用 LumiFS。
//! - 支持 **长文件名（LFN）**：不支持的后果是中文/长名字全变成 `??????.???`。
//! - 不支持 exFAT（不同的文件系统，不是 FAT32 的扩展）。
//! - 簇链走查带**防环上限**：损坏的 FAT 表里可能存在环，不设上限会死循环——
//!   而这是个只读驱动，"读到坏盘"是最可能遇到的情况。
//!
//! ## 纯逻辑
//!
//! 不碰硬件：所有块访问通过 `BlockDevice` trait，宿主机可以用内存镜像测试。

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// 块设备抽象（与 `kfs` 的约定一致：按块读）。
pub trait BlockDevice {
    /// 块大小（FAT 通常是 512）。
    fn block_size(&self) -> usize;
    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), &'static str>;
}

/// 目录项（解析后的形态）。
pub struct Entry {
    /// 8.3 短名（已转大写、去掉填充）。
    pub short: String,
    /// 长文件名（没有就是 `None`）。
    pub long: Option<String>,
    pub attr: u8,
    /// 首簇号（0 表示空文件）。
    pub cluster: u32,
    pub size: u32,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIRECTORY != 0
    }
    pub fn is_volume_label(&self) -> bool {
        self.attr & ATTR_VOLUME_ID != 0
    }
    /// 展示用的名字：有长名用长名。
    pub fn name(&self) -> &str {
        self.long.as_deref().unwrap_or(&self.short)
    }
}

/// 目录项解析结果（FAT16 固定根目录与簇链目录共用同一套解析）。
enum DirItem {
    End,
    Skip,
    Entry(Entry),
}

pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
pub const ATTR_LFN: u8 = 0x0F;

const ENTRY_FREE: u8 = 0xE5;
const ENTRY_END: u8 = 0x00;

/// 挂载后的卷。
pub struct Volume<D: BlockDevice> {
    dev: D,
    /// 每扇区字节数（512/1024/2048/4096）。
    bytes_per_sector: u16,
    sectors_per_cluster: u8,
    /// 保留扇区数（FAT 表起始位置）。
    reserved: u16,
    num_fats: u8,
    /// 每 FAT 扇区数。
    fat_size: u32,
    /// 根目录首簇（FAT32 恒为 2，但从 BPB 读更稳）。
    root_cluster: u32,
    /// 数据区起始扇区。
    data_start: u32,
    /// 总簇数（用于合法性检查）。
    total_clusters: u32,
    /* ---- FAT16 专有 ---- */
    /// 根目录项数（FAT16 固定根目录区；FAT32 为 0）。
    root_entries: u16,
    /// 根目录区的扇区数（FAT16）。
    root_dir_sectors: u32,
    /// FAT 表项字节数：FAT16 = 2，FAT32 = 4。
    fat_entry_bytes: usize,
}

impl<D: BlockDevice> Volume<D> {
    /// 挂载：读 BPB 并做一致性检查。
    pub fn mount(mut dev: D) -> Result<Self, &'static str> {
        let bs = dev.block_size();
        let mut bpb = [0u8; 512];
        dev.read_block(0, &mut bpb).map_err(|_| "读 BPB 失败")?;

        // 引导扇区特征：0x55AA。没有它多半不是 FAT 卷。
        if bpb[510] != 0x55 || bpb[511] != 0xAA {
            return Err("不是 FAT 卷（缺 0x55AA 引导签名）");
        }
        let bytes_per_sector = u16::from_le_bytes([bpb[11], bpb[12]]);
        if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
            return Err("BPB 里的扇区大小非法");
        }
        let sectors_per_cluster = bpb[13];
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
            return Err("每簇扇区数非法");
        }
        let reserved = u16::from_le_bytes([bpb[14], bpb[15]]);
        let num_fats = bpb[16];
        // FAT32 的 16 位 FAT 大小为 0，32 位在偏移 36
        let fat16 = u16::from_le_bytes([bpb[22], bpb[23]]);
        let root_entries = u16::from_le_bytes([bpb[17], bpb[18]]);
        // BPB 的 16 位 FAT 大小非 0 → FAT12/16；为 0 → 看 32 位字段（FAT32）
        let (fat_size, is_fat32) = if fat16 != 0 {
            (fat16 as u32, false)
        } else {
            (
                u32::from_le_bytes([bpb[36], bpb[37], bpb[38], bpb[39]]),
                true,
            )
        };
        if fat_size == 0 {
            return Err("FAT 大小为 0");
        }
        let root_cluster = if is_fat32 {
            let rc = u32::from_le_bytes([bpb[44], bpb[45], bpb[46], bpb[47]]);
            if rc < 2 {
                return Err("根目录簇号非法");
            }
            rc
        } else {
            0 // FAT16 的根目录不是簇链
        };
        let root_dir_sectors = if is_fat32 {
            0
        } else {
            (root_entries as u32 * 32 + bytes_per_sector as u32 - 1) / bytes_per_sector as u32
        };
        let data_start = reserved as u32 + num_fats as u32 * fat_size + root_dir_sectors;
        // 总扇区数有两个字段：32 位的（大卷）和 16 位的（小卷）。
        // **FAT16 常用 16 位那个**，只读 32 位会得到 0，于是簇数算成 0。
        let total32 = u32::from_le_bytes([bpb[32], bpb[33], bpb[34], bpb[35]]);
        let total_sectors = if total32 != 0 {
            total32
        } else {
            u16::from_le_bytes([bpb[19], bpb[20]]) as u32
        };
        let data_sectors = total_sectors.saturating_sub(data_start);
        let total_clusters = data_sectors / sectors_per_cluster as u32;
        // **按 BPB 而不是按簇数判定**：65525 是"格式化时选哪种 FAT"的依据，
        // 不是"读的时候怎么认"的依据。以簇数硬卡会把小容量 FAT32
        // （如 32 MB 的 SD 卡）误判成 FAT16 而拒绝挂载。
        // 只留一个明显不合理的下限，防止把随机数据当卷。
        if total_clusters < 16 {
            return Err("簇数过少，不像是 FAT 卷");
        }

        let _ = bs;
        Ok(Volume {
            dev,
            bytes_per_sector,
            sectors_per_cluster,
            reserved,
            num_fats,
            fat_size,
            root_cluster,
            data_start,
            total_clusters,
            root_entries,
            root_dir_sectors,
            fat_entry_bytes: if is_fat32 { 4 } else { 2 },
        })
    }

    /// 卷信息摘要（自检与日志用）。
    pub fn info(&self) -> (u16, u8, u32, u32, u8) {
        (
            self.bytes_per_sector,
            self.sectors_per_cluster,
            self.fat_size,
            self.total_clusters,
            self.num_fats,
        )
    }

    fn cluster_size(&self) -> usize {
        self.bytes_per_sector as usize * self.sectors_per_cluster as usize
    }

    /// 簇号 → 起始扇区。
    fn cluster_lba(&self, cluster: u32) -> u64 {
        self.data_start as u64 + (cluster as u64 - 2) * self.sectors_per_cluster as u64
    }

    /// 读一个簇到 `buf`。
    fn read_cluster(&mut self, cluster: u32, buf: &mut [u8]) -> Result<(), &'static str> {
        if cluster < 2 || cluster >= self.total_clusters + 2 {
            return Err("簇号越界");
        }
        let lba = self.cluster_lba(cluster);
        let spc = self.sectors_per_cluster as usize;
        let bs = self.bytes_per_sector as usize;
        if buf.len() < self.cluster_size() {
            return Err("缓冲太小");
        }
        for i in 0..spc {
            let off = i * bs;
            self.dev
                .read_block(lba + i as u64, &mut buf[off..off + bs])
                .map_err(|_| "读簇失败")?;
        }
        Ok(())
    }

    /// 查 FAT 表里 `cluster` 的下一项。
    fn next_cluster(&mut self, cluster: u32) -> Result<u32, &'static str> {
        let bs = self.bytes_per_sector as usize;
        let n = self.fat_entry_bytes;
        let entry_off = cluster as usize * n;
        let fat_lba = self.reserved as u64 + (entry_off / bs) as u64;
        let in_block = entry_off % bs;
        let mut buf = [0u8; 512];
        let mut buf = &mut buf[..bs];
        self.dev
            .read_block(fat_lba, &mut buf)
            .map_err(|_| "读 FAT 失败")?;
        let v = if n == 4 {
            u32::from_le_bytes([
                buf[in_block],
                buf[in_block + 1],
                buf[in_block + 2],
                buf[in_block + 3],
            ]) & 0x0FFF_FFFF // 只保留低 28 位（高 4 位是保留/脏标记）
        } else {
            u16::from_le_bytes([buf[in_block], buf[in_block + 1]]) as u32
        };
        Ok(v)
    }

    /// 遍历一条簇链，返回链上的簇号。
    ///
    /// **带防环上限**：坏 FAT 表里可能有环（A→B→A），不设上限就是死循环。
    /// 上限取"总簇数"，任何合法文件都不可能超过它。
    fn chain(&mut self, start: u32) -> Result<Vec<u32>, &'static str> {
        let mut out = Vec::new();
        let mut c = start;
        let limit = self.total_clusters.min(1 << 20) as usize;
        while c >= 2 && c < 0xFFF_FFF8 {
            out.push(c);
            if out.len() > limit {
                return Err("簇链成环（FAT 表损坏）");
            }
            c = self.next_cluster(c)?;
        }
        Ok(out)
    }

    /// 读一个文件/目录的全部内容（上限 8 MiB，防坏盘撑爆内存）。
    pub fn read_all(&mut self, first_cluster: u32, size: u32) -> Result<Vec<u8>, &'static str> {
        let cs = self.cluster_size();
        let want = size as usize;
        let mut out = Vec::new();
        if want == 0 {
            return Ok(out);
        }
        if want > 8 << 20 {
            return Err("文件过大（>8 MiB）");
        }
        out.reserve(want.min(cs * 4));
        let mut buf = alloc::vec![0u8; cs];
        let mut c = first_cluster;
        let mut guard = 0usize;
        let limit = self.total_clusters as usize;
        while out.len() < want {
            if c < 2 || c >= 0xFFF_FFF8 {
                break;
            }
            self.read_cluster(c, &mut buf)?;
            let take = (want - out.len()).min(cs);
            out.extend_from_slice(&buf[..take]);
            guard += 1;
            if guard > limit {
                return Err("簇链过长（FAT 表损坏）");
            }
            let next = self.next_cluster(c)?;
            if next == c {
                return Err("簇链自环（FAT 表损坏）");
            }
            c = next;
        }
        Ok(out)
    }

    /// 列一个目录（`cluster = 0` 表示根目录）。
    pub fn list(&mut self, cluster: u32) -> Result<Vec<Entry>, &'static str> {
        // FAT16 的根目录是**固定扇区**（不占簇链）；FAT32 的根目录是普通簇。
        if cluster == 0 && self.root_dir_sectors > 0 {
            return self.list_fixed_root();
        }
        let start = if cluster == 0 { self.root_cluster } else { cluster };
        let cs = self.cluster_size();
        let mut entries = Vec::new();
        // 长文件名缓冲（一个 LFN 由多个项拼成，按逆序出现）
        let mut lfn: Vec<u16> = Vec::new();
        let mut lfn_expected: u8 = 0;

        let mut buf = alloc::vec![0u8; cs];
        let mut c = start;
        let mut guard = 0usize;
        let limit = self.total_clusters as usize;

        'outer: while c >= 2 && c < 0xFFF_FFF8 {
            self.read_cluster(c, &mut buf)?;
            guard += 1;
            if guard > limit {
                return Err("目录簇链过长");
            }
            for i in 0..(cs / 32) {
                let e = &buf[i * 32..i * 32 + 32];
                if e[0] == ENTRY_END {
                    break 'outer;
                }
                if e[0] == ENTRY_FREE {
                    lfn.clear();
                    continue;
                }
                let attr = e[11];
                if attr == ATTR_LFN {
                    // LFN 项：序号在 bit0-5，bit6 = "最后一项"
                    let seq = e[0] & 0x1F;
                    if e[0] & 0x40 != 0 {
                        lfn_expected = seq;
                        lfn.clear();
                        lfn.resize(seq as usize * 13, 0u16);
                    }
                    if lfn_expected == 0 {
                        continue; // 孤立 LFN 项：丢掉
                    }
                    let base = (seq as usize - 1) * 13;
                    // 每个 LFN 项的 13 个 UTF-16 字符分布在三个不连续区间
                    let chunks: [(usize, usize); 3] = [(1, 5), (14, 12), (28, 2)];
                    let mut k = 0usize;
                    for (off, len) in chunks {
                        for j in 0..len {
                            if base + k < lfn.len() {
                                lfn[base + k] = u16::from_le_bytes([e[off + j * 2], e[off + j * 2 + 1]]);
                            }
                            k += 1;
                        }
                    }
                    continue;
                }
                // 普通项：拼名字
                let short = decode_short_name(e)?;
                let long = if lfn_expected > 0 {
                    let mut s = String::new();
                    for u in lfn.iter() {
                        if *u == 0 || *u == 0xFFFF {
                            continue;
                        }
                        // UTF-16 → 简化处理：BMP 内直接转 char，代理对丢弃
                        if let Some(ch) = char::from_u32(*u as u32) {
                            s.push(ch);
                        }
                    }
                    lfn.clear();
                    lfn_expected = 0;
                    if s.is_empty() {
                        None
                    } else {
                        Some(s)
                    }
                } else {
                    None
                };
                let cluster =
                    ((u16::from_le_bytes([e[20], e[21]]) as u32) << 16) | u16::from_le_bytes([e[26], e[27]]) as u32;
                let size = u32::from_le_bytes([e[28], e[29], e[30], e[31]]);
                let ent = Entry { short, long, attr, cluster, size };
                if !ent.is_volume_label() {
                    entries.push(ent);
                }
            }
            let next = self.next_cluster(c)?;
            // 自环 = 明确的表损坏。上限（6 万次）也能兜住，但会白跑一遍
            // 整个卷——这里早退，让"坏盘"立刻变成 Err。
            if next == c {
                return Err("目录簇链自环");
            }
            c = next;
        }
        Ok(entries)
    }

    /// FAT16 的固定根目录区。
    fn list_fixed_root(&mut self) -> Result<Vec<Entry>, &'static str> {
        let bs = self.bytes_per_sector as usize;
        let start_lba = self.reserved as u64 + self.num_fats as u64 * self.fat_size as u64;
        let mut entries = Vec::new();
        let mut lfn: Vec<u16> = Vec::new();
        let mut lfn_expected: u8 = 0;
        let mut buf = alloc::vec![0u8; bs];
        'outer: for sec in 0..self.root_dir_sectors as u64 {
            self.dev
                .read_block(start_lba + sec, &mut buf)
                .map_err(|_| "读根目录失败")?;
            for i in 0..(bs / 32) {
                let e = &buf[i * 32..i * 32 + 32];
                match self.parse_entry(e, &mut lfn, &mut lfn_expected)? {
                    DirItem::End => break 'outer,
                    DirItem::Skip => continue,
                    DirItem::Entry(ent) => entries.push(ent),
                }
            }
        }
        Ok(entries)
    }

    /// 解析一个 32 字节目录项（LFN 与普通项共用）。
    fn parse_entry(
        &mut self,
        e: &[u8],
        lfn: &mut Vec<u16>,
        lfn_expected: &mut u8,
    ) -> Result<DirItem, &'static str> {
        if e[0] == ENTRY_END {
            return Ok(DirItem::End);
        }
        if e[0] == ENTRY_FREE {
            lfn.clear();
            *lfn_expected = 0;
            return Ok(DirItem::Skip);
        }
        let attr = e[11];
        if attr == ATTR_LFN {
            let seq = e[0] & 0x1F;
            if e[0] & 0x40 != 0 {
                *lfn_expected = seq;
                lfn.clear();
                lfn.resize(seq as usize * 13, 0u16);
            }
            if *lfn_expected == 0 {
                return Ok(DirItem::Skip);
            }
            let base = (seq as usize - 1) * 13;
            let chunks: [(usize, usize); 3] = [(1, 5), (14, 6), (28, 2)];
            let mut k = 0usize;
            for (off, len) in chunks {
                for j in 0..len {
                    if base + k < lfn.len() {
                        lfn[base + k] = u16::from_le_bytes([e[off + j * 2], e[off + j * 2 + 1]]);
                    }
                    k += 1;
                }
            }
            return Ok(DirItem::Skip);
        }
        let short = decode_short_name(e)?;
        let long = if *lfn_expected > 0 {
            let mut s = String::new();
            for u in lfn.iter() {
                if *u == 0 || *u == 0xFFFF {
                    continue;
                }
                if let Some(ch) = char::from_u32(*u as u32) {
                    s.push(ch);
                }
            }
            lfn.clear();
            *lfn_expected = 0;
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        } else {
            None
        };
        let cluster =
            ((u16::from_le_bytes([e[20], e[21]]) as u32) << 16)
                | u16::from_le_bytes([e[26], e[27]]) as u32;
        let size = u32::from_le_bytes([e[28], e[29], e[30], e[31]]);
        let ent = Entry { short, long, attr, cluster, size };
        if ent.is_volume_label() {
            Ok(DirItem::Skip)
        } else {
            Ok(DirItem::Entry(ent))
        }
    }

    /// 按名字找（大小写不敏感，支持 `A/B/C` 这样的路径）。
    pub fn lookup(&mut self, path: &str) -> Result<Entry, &'static str> {
        let mut cluster = 0u32;
        let mut found: Option<Entry> = None;
        let mut depth = 0;
        for part in path.split('/').filter(|s| !s.is_empty()) {
            depth += 1;
            if depth > 16 {
                return Err("路径太深");
            }
            let list = self.list(cluster)?;
            let lower = part.to_ascii_lowercase();
            let mut hit = None;
            for e in list {
                if e.name().to_ascii_lowercase() == lower
                    || e.short.to_ascii_lowercase() == lower
                {
                    hit = Some(e);
                    break;
                }
            }
            let e = hit.ok_or("路径不存在")?;
            if e.is_dir() {
                cluster = e.cluster;
                found = Some(e);
            } else {
                return Ok(e);
            }
        }
        found.ok_or("路径为空")
    }
}

/// 解析 8.3 短名。
fn decode_short_name(e: &[u8]) -> Result<String, &'static str> {
    let mut base = String::new();
    for i in 0..8 {
        let b = e[i];
        // 空格与 NUL 都是填充（测试镜像里常见 NUL 填充）
        if b == b' ' || b == 0 {
            break;
        }
        base.push(b as char);
    }
    let mut ext = String::new();
    for i in 0..3 {
        let b = e[8 + i];
        if b == b' ' || b == 0 {
            break;
        }
        ext.push(b as char);
    }
    // 首字节 0x05 表示真正的 0xE5
    if e[0] == 0x05 {
        base.replace_range(0..1, "\u{e5}");
    }
    if ext.is_empty() {
        Ok(base)
    } else {
        Ok(alloc::format!("{}.{}", base, ext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内存块设备：测试用。
    struct Mem {
        data: Vec<u8>,
        bs: usize,
    }
    impl BlockDevice for Mem {
        fn block_size(&self) -> usize {
            self.bs
        }
        fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), &'static str> {
            let off = lba as usize * self.bs;
            if off + self.bs > self.data.len() {
                return Err("越界");
            }
            buf[..self.bs].copy_from_slice(&self.data[off..off + self.bs]);
            Ok(())
        }
    }

    /// 造一个最小合法 FAT32 卷：512B 扇区、1 扇区/簇、1 个 FAT。
    /// 簇数要 > 65525 才被认作 FAT32——用一个 4 MiB 的内存镜像造。
    fn build_fat32() -> Mem {
        let bs = 512usize;
        let total_sectors = 70000usize; // 约 34 MiB（簇数 > 65525，更接近真实 FAT32）
        let mut data = alloc::vec![0u8; bs * total_sectors];
        // BPB
        data[0] = 0xEB;
        data[1] = 0x58;
        data[2] = 0x90;
        data[11..13].copy_from_slice(&(bs as u16).to_le_bytes());
        data[13] = 1; // 每簇 1 扇区
        data[14..16].copy_from_slice(&32u16.to_le_bytes()); // 保留 32 扇区
        data[16] = 1; // 1 个 FAT
        data[22..24].copy_from_slice(&0u16.to_le_bytes()); // FAT16 大小 = 0 → FAT32
        data[32..36].copy_from_slice(&(total_sectors as u32).to_le_bytes());
        data[36..40].copy_from_slice(&600u32.to_le_bytes()); // 每 FAT 600 扇区（容 76800 项）
        data[44..48].copy_from_slice(&2u32.to_le_bytes()); // 根目录簇 = 2
        data[510] = 0x55;
        data[511] = 0xAA;

        let data_start = 32 + 600;
        // FAT[0]/FAT[1] 保留项
        let fat = 32 * bs;
        data[fat..fat + 4].copy_from_slice(&0x0FFF_FFF8u32.to_le_bytes());
        data[fat + 4..fat + 8].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());

        // 根目录（簇 2 = 数据区第一簇）放三个项：
        // ① "HELLO   TXT" 内容 "hi"
        // ② 子目录 "SUB     "（簇 3）
        // ③ 结束标记
        let root = data_start * bs;
        let mut e = [0u8; 32];
        e[..8].copy_from_slice(b"HELLO   ");
        e[8..11].copy_from_slice(b"TXT");
        e[11] = ATTR_ARCHIVE;
        e[26..28].copy_from_slice(&4u16.to_le_bytes());
        e[28..32].copy_from_slice(&2u32.to_le_bytes());
        data[root..root + 32].copy_from_slice(&e);

        // 簇 4 里放 "hi"
        let c4 = (data_start + 2) * bs; // 簇 4 = data_start + (4-2)
        data[c4..c4 + 2].copy_from_slice(b"hi");

        // 子目录项
        let mut e2 = [0u8; 32];
        e2[..8].copy_from_slice(b"SUB     ");
        e2[11] = ATTR_DIRECTORY;
        e2[26..28].copy_from_slice(&3u16.to_le_bytes()); // 子目录内容在簇 3
        data[root + 32..root + 64].copy_from_slice(&e2);

        // 簇 3（子目录）里放 "INNER   TXT" 指向簇 5
        let c3 = (data_start + 1) * bs;
        let mut e3 = [0u8; 32];
        e3[..8].copy_from_slice(b"INNER   ");
        e3[8..11].copy_from_slice(b"TXT");
        e3[11] = ATTR_ARCHIVE;
        e3[26..28].copy_from_slice(&5u16.to_le_bytes());
        e3[28..32].copy_from_slice(&3u32.to_le_bytes());
        data[c3..c3 + 32].copy_from_slice(&e3);

        let c5 = (data_start + 3) * bs;
        data[c5..c5 + 3].copy_from_slice(b"abc");

        // 簇链：2(根)→EOF, 3(sub)→EOF, 4(hello 数据)→EOF
        for cl in [2u32, 3, 4, 5] {
            let off = fat + cl as usize * 4;
            data[off..off + 4].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
        }
        Mem { data, bs }
    }

    #[test]
    fn mount_rejects_non_fat() {
        let mem = Mem { data: alloc::vec![0u8; 512 * 64], bs: 512 };
        assert!(Volume::mount(mem).is_err());
    }

    #[test]
    fn list_root_and_read_file() {
        let mut v = Volume::mount(build_fat32()).unwrap();
        let (bsz, spc, _, clusters, nfat) = v.info();
        assert_eq!((bsz, spc, nfat), (512, 1, 1));
        assert!(clusters > 0);

        let ents = v.list(0).unwrap();
        let names: Vec<&str> = ents.iter().map(|e| e.name()).collect();
        assert!(names.contains(&"HELLO.TXT"), "根目录应该有 HELLO.TXT：{:?}", names);
        assert!(names.contains(&"SUB"), "根目录应该有 SUB：{:?}", names);

        let hello = ents.iter().find(|e| e.name() == "HELLO.TXT").unwrap();
        assert_eq!(hello.size, 2);
        let data = v.read_all(hello.cluster, hello.size).unwrap();
        assert_eq!(&data[..], b"hi");
    }

    #[test]
    fn lookup_nested_path() {
        let mut v = Volume::mount(build_fat32()).unwrap();
        let e = v.lookup("/SUB/INNER.TXT").unwrap();
        assert_eq!(e.size, 3);
        let d = v.read_all(e.cluster, e.size).unwrap();
        assert_eq!(&d[..], b"abc");
    }

    /// 最小 FAT16 卷：512B 扇区、16 个根目录项、1 个 FAT。
    fn build_fat16() -> Mem {
        let bs = 512usize;
        let total_sectors = 512usize; // 256 KiB
        let fat_sectors = 16usize;
        let reserved = 1usize;
        let nfats = 1usize;
        let root_entries = 16usize;
        let root_sectors = 1usize;
        let mut data = alloc::vec![0u8; bs * total_sectors];

        data[0] = 0xEB;
        data[1] = 0x3C;
        data[2] = 0x90;
        data[11..13].copy_from_slice(&(bs as u16).to_le_bytes());
        data[13] = 1;
        data[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
        data[16] = nfats as u8;
        data[17..19].copy_from_slice(&(root_entries as u16).to_le_bytes());
        data[19..21].copy_from_slice(&(total_sectors as u16).to_le_bytes()); // FAT16 用 16 位总扇区
        data[22..24].copy_from_slice(&(fat_sectors as u16).to_le_bytes());
        data[510] = 0x55;
        data[511] = 0xAA;

        // FAT[0]=0xFFF8, FAT[1]=0xFFFF
        let fat = reserved * bs;
        data[fat..fat + 2].copy_from_slice(&0xFFF8u16.to_le_bytes());
        data[fat + 2..fat + 4].copy_from_slice(&0xFFFFu16.to_le_bytes());

        // 根目录（固定区，从 reserved + nfats*fat_sectors 起）
        let root_lba = reserved + nfats * fat_sectors;
        let root = root_lba * bs;
        let mut e = [0u8; 32];
        e[..8].copy_from_slice(b"TEST    ");
        e[8..11].copy_from_slice(b"TXT");
        e[11] = ATTR_ARCHIVE;
        e[26..28].copy_from_slice(&2u16.to_le_bytes()); // 首簇 2
        e[28..32].copy_from_slice(&5u32.to_le_bytes()); // 5 字节
        data[root..root + 32].copy_from_slice(&e);

        // 簇 2 内容 = "fat16"
        let data_start = root_lba + root_sectors;
        let c2 = data_start * bs;
        data[c2..c2 + 5].copy_from_slice(b"fat16");
        // 簇 2 在 FAT 里是 EOF
        data[fat + 2 * 2..fat + 2 * 2 + 2].copy_from_slice(&0xFFFFu16.to_le_bytes());

        Mem { data, bs }
    }

    #[test]
    fn fat16_mount_list_read() {
        let mut v = Volume::mount(build_fat16()).unwrap();
        let ents = v.list(0).unwrap();
        let names: Vec<&str> = ents.iter().map(|e| e.name()).collect();
        assert_eq!(names, alloc::vec!["TEST.TXT"]);
        let e = &ents[0];
        assert_eq!(e.size, 5);
        let d = v.read_all(e.cluster, e.size).unwrap();
        assert_eq!(&d[..], b"fat16");
        // 路径查找也要能走通
        assert!(v.lookup("/TEST.TXT").is_ok());
    }

    #[test]
    fn corrupt_chain_does_not_hang() {
        let mut mem = build_fat32();
        // 把簇 4（HELLO.TXT 的数据簇）的下一项改成指回自己（自环）。
        // 目录项读到 ENTRY_END 会提前结束，所以环要在**数据链**上测。
        let fat = 32 * 512;
        mem.data[fat + 4 * 4..fat + 4 * 4 + 4].copy_from_slice(&4u32.to_le_bytes());
        let mut v = Volume::mount(mem).unwrap();
        // 声明的 size 远大于一簇 → 必须走簇链 → 撞上自环 → Err（而不是死循环）
        assert!(v.read_all(4, 4096).is_err(), "环必须被检测出来，不能死循环");
    }
}
