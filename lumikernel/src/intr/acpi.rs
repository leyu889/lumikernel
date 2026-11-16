//! ACPI 表定位（M4）：找出 MADT，从而知道"这台机器有几个 CPU、I/O APIC 在哪"。
//!
//! 在 ACPI 之前，PC 的硬件信息靠 BIOS 中断和猜测；有 ACPI 之后，硬件拓扑写在
//! 内存里的表里，谁都能读。MADT（Multiple APIC Description Table）就是"中断控制器清单"：
//! 每个 CPU 一个处理器条目，每个 I/O APIC 一个条目，还有 ISA 中断线的改写规则。
//!
//! 找表的三步：
//! 1. 在 BIOS 区（0xE0000–0xFFFFF）按 16 字节对齐扫 `RSD PTR `（RSDP）；
//! 2. RSDP 指向 RSDT（或 XSDT，64 位），里面是一串"表地址"；
//! 3. 逐个读表头的 4 字节签名，找到 `APIC` 就是 MADT。
//!
//! 解析本身交给 `kacpi`（纯字节切片、可离线单测），这里只负责"把物理内存读出来"。

use kacpi::{Madt, Rsdp};

use crate::mm::paging;

/// 支持的 CPU 上限（静态数组，避免在中断初始化早期依赖堆）。
pub const MAX_CPUS: usize = 8;

/// MADT 里找到的硬件拓扑。
#[derive(Clone, Copy)]
pub struct Info {
    pub lapic_addr: u32,
    pub ioapic_addr: u32,
    pub ioapic_gsi_base: u32,
    /// 已启用 CPU 的 APIC ID 列表（第 0 个是 BSP）。
    pub apic_ids: [u8; MAX_CPUS],
    pub cpu_count: usize,
    /// ISA IRQ0（传统定时器）对应的 GSI —— 有中断源改写时可能不是 0。
    pub timer_gsi: u32,
    pub timer_level: bool,
    pub timer_active_low: bool,
    /// MADT 自身所在物理地址（诊断用）。
    pub madt_phys: u64,
}

impl Info {
    const fn empty() -> Self {
        Info {
            lapic_addr: 0,
            ioapic_addr: 0,
            ioapic_gsi_base: 0,
            apic_ids: [0; MAX_CPUS],
            cpu_count: 0,
            timer_gsi: 0,
            timer_level: false,
            timer_active_low: false,
            madt_phys: 0,
        }
    }
}

/// 把物理地址包成只读切片。长度由调用方保证不越过可用内存（表都很小）。
///
/// # Safety
/// `phys` 必须是有效的物理地址，且 `len` 字节可读。
unsafe fn phys_slice<'a>(phys: u64, len: usize) -> &'a [u8] {
    // SAFETY: 直接映射区里物理地址与虚拟地址一一对应；由调用方保证范围有效。
    core::slice::from_raw_parts(paging::phys_to_virt(phys) as *const u8, len)
}

/// 扫 BIOS 区找 RSDP。
fn find_rsdp() -> Option<Rsdp> {
    // ACPI 规定 RSDP 在 0xE0000–0xFFFFF 之间，16 字节对齐
    let mut p = 0xE_0000u64;
    while p < 0x10_0000 {
        // SAFETY: 这段是 BIOS 区，恒等/直接映射都覆盖；每次只读 36 字节。
        let head = unsafe { phys_slice(p, 36) };
        if head.get(..8) == Some(&kacpi::RSDP_SIG[..]) {
            if let Some(r) = kacpi::parse_rsdp(head) {
                return Some(r);
            }
        }
        p += 16;
    }
    None
}

/// 在 RSDT/XSDT 里找 MADT 的物理地址。
///
/// 注意：标准里 RSDT 的条目就是"表地址"本身，签名要去那张表里读——
/// 所以这里必须逐个解引用，不能只看条目内容（这点跟 `kacpi::find_table`
/// 那个"目录视图"的辅助函数不一样，那个函数本内核不用）。
fn find_madt(rsdp: &Rsdp) -> Option<u64> {
    let (root_phys, is_64) = if rsdp.xsdt_addr != 0 {
        (rsdp.xsdt_addr, true)
    } else {
        (rsdp.rsdt_addr as u64, false)
    };

    // SAFETY: RSDT 头固定 36 字节，是所有 ACPI 表的共同头。
    let head = unsafe { phys_slice(root_phys, 36) };
    let len = u32::from_le_bytes(head.get(4..8)?.try_into().ok()?) as usize;
    if !(36..=65536).contains(&len) {
        return None;
    }
    // SAFETY: 长度已判界。
    let table = unsafe { phys_slice(root_phys, len) };
    let entries = table.get(36..)?;
    let step = if is_64 { 8 } else { 4 };

    let mut off = 0usize;
    while off + step <= entries.len() {
        let addr = if is_64 {
            u64::from_le_bytes(entries.get(off..off + 8)?.try_into().ok()?)
        } else {
            u32::from_le_bytes(entries.get(off..off + 4)?.try_into().ok()?) as u64
        };
        // SAFETY: 指向另一张 ACPI 表的头，36 字节总是可读的。
        let h = unsafe { phys_slice(addr, 36) };
        if h.get(..4) == Some(&kacpi::MADT_SIG[..]) {
            return Some(addr);
        }
        off += step;
    }
    None
}

/// 发现硬件拓扑。找不到 ACPI 时返回 `None`（调用方退回 PIC/PIT）。
pub fn discover() -> Option<Info> {
    let rsdp = find_rsdp()?;
    let madt_phys = find_madt(&rsdp)?;

    // SAFETY: 先读 36 字节头拿长度，再按长度取整张表。
    let head = unsafe { phys_slice(madt_phys, 36) };
    let len = u32::from_le_bytes(head.get(4..8)?.try_into().ok()?) as usize;
    if !(44..=65536).contains(&len) {
        return None;
    }
    // SAFETY: 长度已判界。
    let raw = unsafe { phys_slice(madt_phys, len) };
    if !kacpi::checksum_ok(raw) {
        return None;
    }

    let madt = Madt::parse(raw)?;
    let mut info = Info {
        lapic_addr: madt.lapic_addr(),
        cpu_count: 0,
        madt_phys,
        ..Info::empty()
    };

    for c in madt.cpus() {
        if !c.enabled || info.cpu_count >= MAX_CPUS {
            continue;
        }
        info.apic_ids[info.cpu_count] = c.apic_id;
        info.cpu_count += 1;
    }
    if info.cpu_count == 0 {
        // 有些固件把唯一一个 CPU 的 enabled 位写错，至少当成单核用
        info.cpu_count = 1;
        info.apic_ids[0] = 0;
    }

    if let Some(io) = madt.ioapics().next() {
        info.ioapic_addr = io.addr;
        info.ioapic_gsi_base = io.gsi_base;
    }

    // ISA IRQ0 → GSI 的改写（VirtualBox 常见是 GSI 2）
    info.timer_gsi = 0;
    for iso in madt.overrides() {
        if iso.bus == 0 && iso.source == 0 {
            info.timer_gsi = iso.gsi;
            info.timer_level = iso.is_level_triggered();
            info.timer_active_low = iso.is_active_low();
        }
    }

    Some(info)
}
