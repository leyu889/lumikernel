//! kacpi —— ACPI 表的纯逻辑解析
//!
//! 这个 crate 只做一件事：把引导阶段拿到的**字节切片**（RSDP / RSDT / XSDT / MADT）
//! 解析成内核可以直接使用的结构体。它刻意**不碰内存映射、不做 IO、不分配内存**：
//! 引导器把表从物理内存拷进缓冲区之后，剩下的判断全在这里完成，因此它可以在
//! 中断子系统、SMP 启动之前就跑起来。
//!
//! 三条底线：
//!
//! 1. `no_std`、零依赖、无堆：所有输出都是 `Copy` 的小结构体，或者借用输入切片的迭代器；
//! 2. **绝不 panic**：所有读取都走 `get()`，长度字段一律当作「不可信」处理，
//!    畸形表只会得到 `None`，或者被安全跳过，不会越界、也不会死循环；
//! 3. 本文件不含任何 `unsafe`。
//!
//! 关于 [`find_table`] 的约定（重要）：标准 RSDT/XSDT 的条目只是一个 4/8 字节的地址，
//! 签名放在目标表内部，要匹配签名就必须解引用物理地址——那超出了本 crate「纯字节解析」
//! 的边界。因此本函数接受的是一份**目录视图**：每个条目 = `sig(4) + addr(4|8)`。
//! 上层从物理内存里把子表的头拷出来时，顺手把签名排在地址前面即可。

#![cfg_attr(not(test), no_std)]

/* ---------------- 常量 ---------------- */

/// RSDP 签名，注意结尾是一个空格。
pub const RSDP_SIG: [u8; 8] = *b"RSD PTR ";
/// MADT 的签名（表头里的 `sig` 字段）。
pub const MADT_SIG: [u8; 4] = *b"APIC";

/// 标准 SDT 表头长度：sig(4) + length(4) + revision(1) + checksum(1) + oem(6)
/// + oem_table_id(8) + oem_revision(4) + creator(4) + creator_revision(4)。
const SDT_HEADER_LEN: usize = 36;

/// RSDP v1 的最小长度（没有 XSDT 地址与扩展校验和）。
const RSDP_V1_LEN: usize = 20;
/// RSDP v2 的长度（多了 length / XSDT 地址 / 扩展校验和 / 保留字段）。
const RSDP_V2_LEN: usize = 36;

/// MADT 固定部分长度：36 字节 SDT 头 + local APIC 地址(4) + flags(4)。
const MADT_FIXED_LEN: usize = 44;

/* ---------------- 小端读取辅助 ---------------- */

/// 读一个小端 `u16`。越界返回 `None`。
fn le_u16(bytes: &[u8], off: usize) -> Option<u16> {
    let end = off.checked_add(2)?;
    let chunk = bytes.get(off..end)?;
    Some(u16::from_le_bytes([*chunk.get(0)?, *chunk.get(1)?]))
}

/// 读一个小端 `u32`。越界返回 `None`。
fn le_u32(bytes: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let chunk = bytes.get(off..end)?;
    Some(u32::from_le_bytes([
        *chunk.get(0)?,
        *chunk.get(1)?,
        *chunk.get(2)?,
        *chunk.get(3)?,
    ]))
}

/// 读一个小端 `u64`。越界返回 `None`。
fn le_u64(bytes: &[u8], off: usize) -> Option<u64> {
    let end = off.checked_add(8)?;
    let chunk = bytes.get(off..end)?;
    Some(u64::from_le_bytes([
        *chunk.get(0)?,
        *chunk.get(1)?,
        *chunk.get(2)?,
        *chunk.get(3)?,
        *chunk.get(4)?,
        *chunk.get(5)?,
        *chunk.get(6)?,
        *chunk.get(7)?,
    ]))
}

/* ---------------- 校验和 ---------------- */

/// 对前 `len` 字节（也就是整个切片）求和，结果为 0 即校验通过。
///
/// ACPI 的约定：把校验和字段本身也算进去，整表按字节累加（u8 回绕）必须为 0。
pub fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |acc, b| acc.wrapping_add(*b)) == 0
}

/* ---------------- RSDP ---------------- */

/// RSDP（Root System Description Pointer）里本内核关心的字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rsdp {
    /// ACPI 版本。0/1 表示只有 RSDT，>= 2 才有 XSDT。
    pub revision: u8,
    /// RSDT 的物理地址（32 位，任何版本都有）。
    pub rsdt_addr: u32,
    /// XSDT 的物理地址（64 位）；`revision < 2` 时恒为 0。
    pub xsdt_addr: u64,
}

/// 解析 RSDP。v1（20 字节）与 v2（36 字节）都支持。
///
/// 返回 `None` 的情况：
/// - 长度不足 20 字节；
/// - 签名不是 [`RSDP_SIG`]；
/// - 前 20 字节的校验和为 0 不成立；
/// - `revision >= 2` 但长度不足 36 字节（读不到 XSDT 地址）；
/// - `revision >= 2` 时 36 字节的扩展校验和不成立。
pub fn parse_rsdp(bytes: &[u8]) -> Option<Rsdp> {
    let sig = bytes.get(0..8)?;
    if sig != RSDP_SIG.as_slice() {
        return None;
    }
    // 前 20 字节的校验和永远要过。
    if !checksum_ok(bytes.get(0..RSDP_V1_LEN)?) {
        return None;
    }

    let revision = *bytes.get(15)?;
    let rsdt_addr = le_u32(bytes, 16)?;

    if revision >= 2 {
        // v2 必须有完整的 36 字节，且扩展校验和（覆盖全部 36 字节）也要成立。
        if !checksum_ok(bytes.get(0..RSDP_V2_LEN)?) {
            return None;
        }
        let xsdt_addr = le_u64(bytes, 24)?;
        Some(Rsdp {
            revision,
            rsdt_addr,
            xsdt_addr,
        })
    } else {
        Some(Rsdp {
            revision,
            rsdt_addr,
            xsdt_addr: 0,
        })
    }
}

/* ---------------- RSDT / XSDT 目录查询 ---------------- */

/// 在一张 RSDT/XSDT 目录里找第 `index` 个签名为 `sig` 的表，返回它的物理地址。
///
/// 条目布局见模块文档：每条 = `sig(4) + addr`，地址宽度由 `entries_are_64bit`
/// 决定（RSDT 为 4 字节，XSDT 为 8 字节）。
///
/// 头部 `length` 字段不可信：超过实际切片长度时按切片长度截断；小于 36 字节
/// （连表头都放不下）时返回 `None`。找不到、或 `index` 超过匹配数量都返回 `None`。
pub fn find_table(table: &[u8], sig: &[u8; 4], index: usize, entries_are_64bit: bool) -> Option<u64> {
    let declared = le_u32(table, 4)? as usize;
    let end = if declared < table.len() {
        declared
    } else {
        table.len()
    };
    if end < SDT_HEADER_LEN {
        return None;
    }
    let entries = table.get(SDT_HEADER_LEN..end)?;

    let addr_len = if entries_are_64bit { 8 } else { 4 };
    let stride = 4 + addr_len;

    let mut pos = 0usize;
    let mut seen = 0usize;
    loop {
        let rec_end = match pos.checked_add(stride) {
            Some(v) => v,
            None => break,
        };
        let rec = match entries.get(pos..rec_end) {
            Some(r) => r,
            None => break,
        };
        let rec_sig = match rec.get(0..4) {
            Some(s) => s,
            None => break,
        };
        if rec_sig == sig.as_slice() {
            if seen == index {
                let addr = if entries_are_64bit {
                    le_u64(rec, 4)?
                } else {
                    le_u32(rec, 4)? as u64
                };
                return Some(addr);
            }
            seen += 1;
        }
        pos = rec_end;
    }
    None
}

/* ---------------- MADT ---------------- */

/// 一个逻辑 CPU（来自 type 0 或 type 9 条目）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuInfo {
    /// ACPI 处理器 ID（MADT 里给 OS 用的编号，不一定是物理 APIC ID）。
    pub acpi_id: u8,
    /// APIC ID。x2APIC 条目只取低 8 位（M4 只有 xAPIC）。
    pub apic_id: u8,
    /// 条目的 flags bit0：1 表示该处理器已启用。
    pub enabled: bool,
}

/// 一个 I/O APIC（来自 type 1 条目）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApicInfo {
    /// I/O APIC 编号。
    pub id: u8,
    /// 寄存器基址（MMIO 物理地址）。
    pub addr: u32,
    /// 该 I/O APIC 管理的第一个全局中断号（GSI）。
    pub gsi_base: u32,
}

/// 一条中断源改写（Interrupt Source Override，来自 type 2 条目）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Iso {
    /// 总线类型，0 表示 ISA。
    pub bus: u8,
    /// 总线上的源 IRQ 号。
    pub source: u8,
    /// 改写后映射到的全局中断号（GSI）。
    pub gsi: u32,
    /// 极性/触发方式标志（取条目 flags 的低 16 位）。
    pub flags: u16,
}

impl Iso {
    /// 这条改写是否适用于某条 ISA IRQ（即 `source` 是否等于 `irq`）。
    ///
    /// 注意：这不是 `flags` 的判断，`flags` 的判断见 [`Iso::is_level_triggered`]。
    pub fn is_level_triggered(&self) -> bool {
        // 按本 crate 的约定：flags 低 2 位 == 0b11 视为电平触发。
        self.flags & 0b11 == 0b11
    }

    /// 是否低电平有效（flags 低 2 位 == 0b01）。
    pub fn is_active_low(&self) -> bool {
        self.flags & 0b11 == 0b01
    }
}

/// 解析后的 MADT（Multiple APIC Description Table）。
///
/// 只借用原始切片，不复制、不分配；迭代器每次重新从头扫描条目区。
pub struct Madt<'a> {
    /// 36 字节 SDT 头之后、到声明长度为止的可变长条目区。
    entries: &'a [u8],
    /// local APIC 的 MMIO 物理地址（MADT 固定部分 offset 36 的 u32）。
    lapic_addr: u32,
}

impl<'a> Madt<'a> {
    /// 解析整张 MADT（含 36 字节 SDT 头）。
    ///
    /// 返回 `None` 的情况：
    /// - 切片不足 44 字节（36 字节头 + local APIC 地址 + flags）；
    /// - 签名不是 [`MADT_SIG`]；
    /// - 头部的 `length` 小于 44，或**大于**实际切片长度（此时不敢按它走，
    ///   直接拒绝；条目区内部的坏长度才会被安全跳过）。
    ///
    /// 本函数不校验表的 checksum，需要的话调用方自己用 [`checksum_ok`]。
    pub fn parse(table: &'a [u8]) -> Option<Self> {
        if table.len() < MADT_FIXED_LEN {
            return None;
        }
        let sig = table.get(0..4)?;
        if sig != MADT_SIG.as_slice() {
            return None;
        }
        let declared = le_u32(table, 4)? as usize;
        if declared < MADT_FIXED_LEN || declared > table.len() {
            return None;
        }
        let lapic_addr = le_u32(table, 36)?;
        let entries = table.get(MADT_FIXED_LEN..declared)?;
        Some(Madt {
            entries,
            lapic_addr,
        })
    }

    /// local APIC 的 MMIO 物理地址（通常是 `0xFEE0_0000`）。
    pub fn lapic_addr(&self) -> u32 {
        self.lapic_addr
    }

    /// 遍历所有 CPU 条目（type 0 与 type 9）。
    pub fn cpus(&self) -> Cpus<'a> {
        Cpus {
            raw: RawEntries::new(self.entries),
        }
    }

    /// 遍历所有 I/O APIC 条目（type 1）。
    pub fn ioapics(&self) -> IoApics<'a> {
        IoApics {
            raw: RawEntries::new(self.entries),
        }
    }

    /// 遍历所有中断源改写条目（type 2）。
    pub fn overrides(&self) -> Isos<'a> {
        Isos {
            raw: RawEntries::new(self.entries),
        }
    }

    /// 已启用的 CPU 数量（flags bit0 == 1）。未启用的条目不计入。
    pub fn cpu_count(&self) -> usize {
        self.cpus().filter(|c| c.enabled).count()
    }
}

/* ---------------- 条目游标与迭代器 ---------------- */

/// MADT 条目区的原始游标。
///
/// 每个条目：`type(1) + length(1) + payload`。长度字段不可信，任何异常都让迭代
/// 安全结束（返回 `None`），绝不越界也绝不死循环。
struct RawEntries<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> RawEntries<'a> {
    fn new(data: &'a [u8]) -> Self {
        RawEntries { data, pos: 0 }
    }

    /// 取下一整条条目，返回 `(type, 条目字节)`（含 type/length 两个字节）。
    ///
    /// 到尾部、长度 < 2、长度越过剩余空间都返回 `None`。
    fn next_entry(&mut self) -> Option<(u8, &'a [u8])> {
        let ty = *self.data.get(self.pos)?;
        let len = *self.data.get(self.pos.checked_add(1)?)? as usize;
        if len < 2 {
            return None;
        }
        let end = self.pos.checked_add(len)?;
        if end > self.data.len() {
            return None;
        }
        let entry = self.data.get(self.pos..end)?;
        self.pos = end;
        Some((ty, entry))
    }
}

/// type 0：Processor Local APIC。
fn parse_local_apic(entry: &[u8]) -> Option<CpuInfo> {
    // type(1) length(1) acpi_processor_id(1) apic_id(1) flags(4)
    if entry.len() < 8 {
        return None;
    }
    let flags = le_u32(entry, 4)?;
    Some(CpuInfo {
        acpi_id: *entry.get(2)?,
        apic_id: *entry.get(3)?,
        enabled: flags & 1 != 0,
    })
}

/// type 9：Processor Local x2APIC。apic_id 只取低 8 位（M4 只有 xAPIC）。
fn parse_x2apic(entry: &[u8]) -> Option<CpuInfo> {
    // type(1) length(1) reserved(2) x2apic_id(4) flags(4) acpi_uid(4)
    if entry.len() < 16 {
        return None;
    }
    let x2apic_id = le_u32(entry, 4)?;
    let flags = le_u32(entry, 8)?;
    let acpi_id = le_u32(entry, 12)?;
    Some(CpuInfo {
        acpi_id: acpi_id as u8,
        apic_id: x2apic_id as u8,
        enabled: flags & 1 != 0,
    })
}

/// type 1：I/O APIC。
fn parse_ioapic(entry: &[u8]) -> Option<IoApicInfo> {
    // type(1) length(1) id(1) reserved(1) addr(4) gsi_base(4)
    if entry.len() < 12 {
        return None;
    }
    Some(IoApicInfo {
        id: *entry.get(2)?,
        addr: le_u32(entry, 4)?,
        gsi_base: le_u32(entry, 8)?,
    })
}

/// type 2：Interrupt Source Override。
fn parse_iso(entry: &[u8]) -> Option<Iso> {
    // type(1) length(1) bus(1) source(1) gsi(4) flags(4) —— 共 12 字节
    if entry.len() < 12 {
        return None;
    }
    Some(Iso {
        bus: *entry.get(2)?,
        source: *entry.get(3)?,
        gsi: le_u32(entry, 4)?,
        // 字段是 u32，本结构只保留低 16 位（高 16 位保留未用）。
        flags: le_u16(entry, 8)?,
    })
}

/// CPU 迭代器，产出 type 0 / type 9 条目；其它类型透明跳过。
pub struct Cpus<'a> {
    raw: RawEntries<'a>,
}

impl<'a> Iterator for Cpus<'a> {
    type Item = CpuInfo;

    fn next(&mut self) -> Option<CpuInfo> {
        loop {
            let (ty, entry) = self.raw.next_entry()?;
            let parsed = match ty {
                0 => parse_local_apic(entry),
                9 => parse_x2apic(entry),
                _ => None,
            };
            if let Some(cpu) = parsed {
                return Some(cpu);
            }
        }
    }
}

/// I/O APIC 迭代器，只产出 type 1 条目。
pub struct IoApics<'a> {
    raw: RawEntries<'a>,
}

impl<'a> Iterator for IoApics<'a> {
    type Item = IoApicInfo;

    fn next(&mut self) -> Option<IoApicInfo> {
        loop {
            let (ty, entry) = self.raw.next_entry()?;
            if ty != 1 {
                continue;
            }
            if let Some(io) = parse_ioapic(entry) {
                return Some(io);
            }
        }
    }
}

/// 中断源改写迭代器，只产出 type 2 条目。
pub struct Isos<'a> {
    raw: RawEntries<'a>,
}

impl<'a> Iterator for Isos<'a> {
    type Item = Iso;

    fn next(&mut self) -> Option<Iso> {
        loop {
            let (ty, entry) = self.raw.next_entry()?;
            if ty != 2 {
                continue;
            }
            if let Some(iso) = parse_iso(entry) {
                return Some(iso);
            }
        }
    }
}

/* ---------------- 单元测试 ---------------- */

#[cfg(test)]
mod tests {
    use super::*;

    /* ---- 构造工具 ---- */

    /// 把 `buf[off]` 设成「让 `buf[0..end]` 字节和为 0」的值。
    fn fix_checksum(buf: &mut [u8], off: usize, end: usize) {
        let mut sum: u8 = 0;
        let mut i = 0usize;
        while i < end {
            if i != off {
                if let Some(b) = buf.get(i) {
                    sum = sum.wrapping_add(*b);
                }
            }
            i += 1;
        }
        if let Some(slot) = buf.get_mut(off) {
            *slot = 0u8.wrapping_sub(sum);
        }
    }

    /// 拼一张标准 SDT：36 字节头 + body，并把 checksum 修好。
    fn sdt(sig: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let total = SDT_HEADER_LEN + body.len();
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(sig);
        v.extend_from_slice(&(total as u32).to_le_bytes());
        v.push(1); // revision
        v.push(0); // checksum 占位
        v.extend_from_slice(b"TESTOE"); // OEM ID（6）
        v.extend_from_slice(b"TESTOEMT"); // OEM table id（8）
        v.extend_from_slice(&1u32.to_le_bytes()); // OEM revision
        v.extend_from_slice(b"TEST"); // creator（4）
        v.extend_from_slice(&1u32.to_le_bytes()); // creator revision
        v.extend_from_slice(body);
        let end = v.len();
        fix_checksum(&mut v, 9, end);
        v
    }

    /// 把所有条目拼起来。
    fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
        let mut v: Vec<u8> = Vec::new();
        for p in parts {
            v.extend_from_slice(p);
        }
        v
    }

    /// RSDP v1：20 字节。
    fn rsdp_v1() -> Vec<u8> {
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(b"RSD PTR ");
        v.push(0); // checksum 占位
        v.extend_from_slice(b"TEST  "); // OEM ID（6）
        v.push(0); // revision = 0
        v.extend_from_slice(&0x1234_5678u32.to_le_bytes()); // RSDT 地址
        fix_checksum(&mut v, 8, RSDP_V1_LEN);
        v
    }

    /// RSDP v2：36 字节，带 XSDT 地址与扩展校验和。
    fn rsdp_v2() -> Vec<u8> {
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(b"RSD PTR ");
        v.push(0); // checksum 占位（只覆盖前 20 字节）
        v.extend_from_slice(b"TEST  "); // OEM ID（6）
        v.push(2); // revision = 2
        v.extend_from_slice(&0x1234_5678u32.to_le_bytes()); // RSDT 地址
        v.extend_from_slice(&36u32.to_le_bytes()); // length
        v.extend_from_slice(&0x0000_0001_2345_6789u64.to_le_bytes()); // XSDT 地址
        v.push(0); // 扩展校验和占位
        v.extend_from_slice(&[0u8, 0, 0]); // 保留
        fix_checksum(&mut v, 8, RSDP_V1_LEN);
        fix_checksum(&mut v, 32, RSDP_V2_LEN);
        v
    }

    /// MADT 条目：Local APIC。
    fn e_cpu(acpi_id: u8, apic_id: u8, enabled: bool) -> Vec<u8> {
        let flags: u32 = if enabled { 1 } else { 0 };
        let mut v: Vec<u8> = vec![0, 8, acpi_id, apic_id];
        v.extend_from_slice(&flags.to_le_bytes());
        v
    }

    /// MADT 条目：I/O APIC。
    fn e_ioapic(id: u8, addr: u32, gsi_base: u32) -> Vec<u8> {
        let mut v: Vec<u8> = vec![1, 12, id, 0];
        v.extend_from_slice(&addr.to_le_bytes());
        v.extend_from_slice(&gsi_base.to_le_bytes());
        v
    }

    /// MADT 条目：Interrupt Source Override（12 字节）。
    fn e_iso(bus: u8, source: u8, gsi: u32, flags: u16) -> Vec<u8> {
        let mut v: Vec<u8> = vec![2, 12, bus, source];
        v.extend_from_slice(&gsi.to_le_bytes());
        v.extend_from_slice(&(flags as u32).to_le_bytes());
        v
    }

    /// MADT 条目：x2APIC。
    fn e_x2apic(x2apic_id: u32, flags: u32, acpi_id: u32) -> Vec<u8> {
        let mut v: Vec<u8> = vec![9, 16, 0, 0];
        v.extend_from_slice(&x2apic_id.to_le_bytes());
        v.extend_from_slice(&flags.to_le_bytes());
        v.extend_from_slice(&acpi_id.to_le_bytes());
        v
    }

    /// 拼一张 MADT（固定部分 + 条目）。
    fn madt(lapic: u32, entries: &[u8]) -> Vec<u8> {
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&lapic.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes()); // flags
        body.extend_from_slice(entries);
        sdt(&MADT_SIG, &body)
    }

    /* ---- 1/2: 校验和 ---- */

    #[test]
    fn checksum_ok_accepts_valid() {
        let b = rsdp_v1();
        assert_eq!(checksum_ok(&b), true);
    }

    #[test]
    fn checksum_ok_rejects_corrupted() {
        let mut b = rsdp_v1();
        if let Some(x) = b.get_mut(10) {
            *x = x.wrapping_add(1);
        }
        assert_eq!(checksum_ok(&b), false);
    }

    /* ---- 3/4/5/6: RSDP ---- */

    #[test]
    fn parse_rsdp_v1_minimal() {
        let b = rsdp_v1();
        assert_eq!(
            parse_rsdp(&b),
            Some(Rsdp {
                revision: 0,
                rsdt_addr: 0x1234_5678,
                xsdt_addr: 0,
            })
        );
    }

    #[test]
    fn parse_rsdp_v2_with_xsdt() {
        let b = rsdp_v2();
        assert_eq!(
            parse_rsdp(&b),
            Some(Rsdp {
                revision: 2,
                rsdt_addr: 0x1234_5678,
                xsdt_addr: 0x0000_0001_2345_6789,
            })
        );
        // v2 的前 20 字节校验和同样成立。
        assert_eq!(b.get(0..RSDP_V1_LEN).map(checksum_ok), Some(true));
    }

    #[test]
    fn parse_rsdp_bad_signature() {
        let mut b = rsdp_v1();
        if let Some(x) = b.get_mut(0) {
            *x = b'X';
        }
        assert_eq!(parse_rsdp(&b), None);
    }

    #[test]
    fn parse_rsdp_bad_checksum() {
        let mut b = rsdp_v1();
        if let Some(x) = b.get_mut(12) {
            *x = x.wrapping_add(1);
        }
        assert_eq!(parse_rsdp(&b), None);
    }

    /* ---- 7/8/9: find_table ---- */

    #[test]
    fn find_table_rsdt_32bit_entries() {
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(b"FACP");
        body.extend_from_slice(&0x0000_1000u32.to_le_bytes());
        body.extend_from_slice(b"APIC");
        body.extend_from_slice(&0x0000_2000u32.to_le_bytes());
        let rsdt = sdt(b"RSDT", &body);

        assert_eq!(find_table(&rsdt, b"APIC", 0, false), Some(0x0000_2000));
        assert_eq!(find_table(&rsdt, b"FACP", 0, false), Some(0x0000_1000));
    }

    #[test]
    fn find_table_xsdt_64bit_entries() {
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(b"FACP");
        body.extend_from_slice(&0x0000_0000_0000_3000u64.to_le_bytes());
        body.extend_from_slice(b"APIC");
        body.extend_from_slice(&0x0000_0001_0000_4000u64.to_le_bytes());
        let xsdt = sdt(b"XSDT", &body);

        assert_eq!(find_table(&xsdt, b"APIC", 0, true), Some(0x0000_0001_0000_4000));
        assert_eq!(find_table(&xsdt, b"FACP", 0, true), Some(0x0000_3000));
    }

    #[test]
    fn find_table_missing_and_out_of_range() {
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(b"APIC");
        body.extend_from_slice(&0x0000_2000u32.to_le_bytes());
        let rsdt = sdt(b"RSDT", &body);

        // 签名不存在。
        assert_eq!(find_table(&rsdt, b"SSDT", 0, false), None);
        // 只有 1 个匹配，index 1 越界。
        assert_eq!(find_table(&rsdt, b"APIC", 1, false), None);
        // 连表头都不完整。
        assert_eq!(
            rsdt.get(0..10).and_then(|s| find_table(s, b"APIC", 0, false)),
            None
        );
    }

    /* ---- 10/11/12: MADT 正常路径 ---- */

    #[test]
    fn madt_full_layout() {
        let entries = cat(&[
            e_cpu(0, 0, true),
            e_cpu(1, 1, true),
            e_cpu(2, 2, false),
            e_cpu(3, 3, true),
            e_ioapic(2, 0xFEC0_0000, 0),
            e_iso(0, 0, 2, 0x000F),
            e_iso(0, 5, 5, 0x0005),
        ]);
        let t = madt(0xFEE0_0000, &entries);

        let cpus = Madt::parse(&t).map(|m| m.cpus().collect::<Vec<CpuInfo>>());
        assert_eq!(
            cpus,
            Some(vec![
                CpuInfo { acpi_id: 0, apic_id: 0, enabled: true },
                CpuInfo { acpi_id: 1, apic_id: 1, enabled: true },
                CpuInfo { acpi_id: 2, apic_id: 2, enabled: false },
                CpuInfo { acpi_id: 3, apic_id: 3, enabled: true },
            ])
        );

        let ioapics = Madt::parse(&t).map(|m| m.ioapics().collect::<Vec<IoApicInfo>>());
        assert_eq!(
            ioapics,
            Some(vec![IoApicInfo {
                id: 2,
                addr: 0xFEC0_0000,
                gsi_base: 0,
            }])
        );

        let isos = Madt::parse(&t).map(|m| m.overrides().collect::<Vec<Iso>>());
        assert_eq!(
            isos,
            Some(vec![
                Iso { bus: 0, source: 0, gsi: 2, flags: 0x000F },
                Iso { bus: 0, source: 5, gsi: 5, flags: 0x0005 },
            ])
        );
    }

    #[test]
    fn madt_cpu_count_only_enabled() {
        let entries = cat(&[
            e_cpu(0, 0, true),
            e_cpu(1, 1, false),
            e_cpu(2, 2, true),
            e_cpu(3, 3, true),
        ]);
        let t = madt(0xFEE0_0000, &entries);
        assert_eq!(Madt::parse(&t).map(|m| m.cpu_count()), Some(3));
    }

    #[test]
    fn madt_lapic_addr() {
        let entries = cat(&[e_cpu(0, 0, true)]);
        let t = madt(0xFEE0_0000, &entries);
        assert_eq!(Madt::parse(&t).map(|m| m.lapic_addr()), Some(0xFEE0_0000));
    }

    #[test]
    fn madt_iso_irq0_maps_to_gsi2() {
        // 典型 VirtualBox：IRQ0 → GSI2，电平/低有效。
        let entries = cat(&[e_iso(0, 0, 2, 0x000F), e_iso(0, 9, 9, 0x0005)]);
        let t = madt(0xFEE0_0000, &entries);
        // 迭代结果一次性取出来比较，避免对 Option<Vec<_>> 反复解构。
        let got = Madt::parse(&t).map(|m| {
            let list: Vec<Iso> = m.overrides().collect();
            (
                list.len(),
                list.first().copied(),
                list.get(1).copied(),
                list.first().map(|i| i.is_level_triggered()),
                list.first().map(|i| i.is_active_low()),
                list.get(1).map(|i| i.is_level_triggered()),
                list.get(1).map(|i| i.is_active_low()),
            )
        });
        let irq0 = Iso { bus: 0, source: 0, gsi: 2, flags: 0x000F };
        let irq9 = Iso { bus: 0, source: 9, gsi: 9, flags: 0x0005 };
        assert_eq!(
            got,
            Some((2, Some(irq0), Some(irq9), Some(true), Some(false), Some(false), Some(true)))
        );
        // flags = 0x000F：低 2 位 0b11 → 电平触发；不是低有效（0b01）。
        assert_eq!(irq0.is_level_triggered(), true);
        assert_eq!(irq0.is_active_low(), false);
        // flags = 0x0005：低 2 位 0b01 → 低有效，非电平。
        assert_eq!(irq9.is_level_triggered(), false);
        assert_eq!(irq9.is_active_low(), true);
        assert_eq!(irq0.gsi, 2);
    }

    /* ---- 13/14/15/16/17: 畸形表 ---- */

    #[test]
    fn madt_entry_length_overruns_slice() {
        // 第一条合法，第二条声明的长度（0xF0）远超剩余字节 → 迭代安全停止。
        let mut entries = e_cpu(0, 0, true);
        entries.extend_from_slice(&[0u8, 0xF0, 1, 2, 3, 4, 5, 6]);
        let t = madt(0xFEE0_0000, &entries);
        assert_eq!(Madt::parse(&t).map(|m| m.cpus().count()), Some(1));
    }

    #[test]
    fn madt_entry_length_below_two_stops() {
        // 长度字段 1 < 2 → 立即停止，一个条目也解析不出来。
        let t = madt(0xFEE0_0000, &[0u8, 1]);
        assert_eq!(Madt::parse(&t).map(|m| m.cpus().count()), Some(0));
        assert_eq!(Madt::parse(&t).map(|m| m.cpu_count()), Some(0));
    }

    #[test]
    fn madt_unknown_type_skipped() {
        // 0x7F 是未知类型，应被透明跳过，后面的 CPU 仍能解析。
        let entries = cat(&[vec![0x7F, 4, 0xAA, 0xBB], e_cpu(7, 7, true)]);
        let t = madt(0xFEE0_0000, &entries);
        assert_eq!(
            Madt::parse(&t).map(|m| m.cpus().collect::<Vec<CpuInfo>>()),
            Some(vec![CpuInfo { acpi_id: 7, apic_id: 7, enabled: true }])
        );
    }

    #[test]
    fn madt_empty_entry_area() {
        let t = madt(0xFEE0_0000, &[]);
        assert_eq!(
            Madt::parse(&t).map(|m| (
                m.cpus().count(),
                m.cpu_count(),
                m.ioapics().count(),
                m.overrides().count(),
            )),
            Some((0, 0, 0, 0))
        );
    }

    #[test]
    fn madt_declared_length_too_big_is_none() {
        // 头部 length 声称 44+64，但切片只有 44 字节 → 拒绝解析。
        let mut t = madt(0xFEE0_0000, &[]);
        if let Some(x) = t.get_mut(4) {
            *x = 44u32.wrapping_add(64) as u8;
        }
        // 高位本来就为 0，改写低字节即可。
        assert_eq!(Madt::parse(&t).map(|m| m.cpu_count()), None);

        // 反过来：长度小于固定部分 44 → 也拒绝。
        let mut t2 = madt(0xFEE0_0000, &[]);
        if let Some(x) = t2.get_mut(4) {
            *x = 40;
        }
        assert_eq!(Madt::parse(&t2).map(|m| m.cpu_count()), None);
    }

    /* ---- 18: x2APIC ---- */

    #[test]
    fn madt_x2apic_counted() {
        let entries = cat(&[
            e_cpu(0, 0, true),
            e_x2apic(0x0000_0105, 1, 5),
        ]);
        let t = madt(0xFEE0_0000, &entries);
        let cpus = Madt::parse(&t).map(|m| m.cpus().collect::<Vec<CpuInfo>>());
        assert_eq!(
            cpus,
            Some(vec![
                CpuInfo { acpi_id: 0, apic_id: 0, enabled: true },
                CpuInfo { acpi_id: 5, apic_id: 0x05, enabled: true },
            ])
        );
        assert_eq!(Madt::parse(&t).map(|m| m.cpu_count()), Some(2));
    }
}
