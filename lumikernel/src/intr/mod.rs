//! 中断与异常子系统（M4：APIC 取代 8259A/PIT）
//!
//! 组成：
//! - [`frame`]：栈帧布局 + 控制寄存器/端口裸操作 + 停机原语；
//! - [`idt`]：中断描述符表（含 DPL=3 的 `int 0x80` 门）
//! - [`acpi`]：定位 MADT，得到 CPU 个数与 I/O APIC 位置
//! - [`apic`]：本地 APIC / I/O APIC / APIC 定时器 / IPI
//! - [`pic`]、[`pit`]：8259A 与 PIT，现在只作为**退路**（找不到 ACPI 时）
//! - [`timer`]：全局 tick 计数（来源可变，读数的人不变）
//! - [`probe`]：受控故障探针（异常 fixup）
//! - [`handler`]：总调度——外部中断、异常转储、探针恢复、系统调用
//!
//! 为什么换 APIC：8259A 只有 15 条线且只连一个核；PIT 是全局单一定时器。
//! 多核需要"每个核自己的定时器 + 能给指定核投递中断"，这两件事只有 APIC 能给。

use core::arch::global_asm;
use core::sync::atomic::{AtomicU8, Ordering};

pub mod acpi;
pub mod apic;
pub mod frame;
pub mod handler;
pub mod idt;
pub mod pic;
pub mod pit;
pub mod rtc;
pub mod probe;
pub mod timer;

// 48 个入口桩 + `trap_return` + `__probe`。用 AT&T 语法（GAS 默认）。
global_asm!(include_str!("stubs.S"), options(att_syntax));

/// 中断控制器工作模式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 还没初始化。
    None,
    /// 本地 APIC + I/O APIC（M4 起的默认路径）。
    Apic,
    /// 8259A + PIT（找不到 ACPI 时的退路）。
    Pic,
}

static MODE: AtomicU8 = AtomicU8::new(0);

pub fn mode() -> Mode {
    match MODE.load(Ordering::Relaxed) {
        1 => Mode::Apic,
        2 => Mode::Pic,
        _ => Mode::None,
    }
}

fn set_mode(m: Mode) {
    MODE.store(
        match m {
            Mode::Apic => 1,
            Mode::Pic => 2,
            Mode::None => 0,
        },
        Ordering::Relaxed,
    );
}

/// 发 EOI。**必须按模式分派**：APIC 模式下写 8259A 的端口不会报错，
/// 但中断永远不会结束，表现为"只来一次中断，之后彻底静默"。
pub fn eoi(vector: u64) {
    match mode() {
        Mode::Apic => apic::eoi(),
        Mode::Pic => pic::eoi(vector),
        Mode::None => {}
    }
}

/// 早期初始化报告（只含 IDT 信息）。
pub struct Report {
    pub idt_base: u64,
    pub idt_vectors: usize,
}

/// APIC 阶段报告。
pub struct ApicReport {
    pub mode: Mode,
    pub apic_id: u8,
    pub cpu_count: usize,
    pub timer_gsi: u32,
    pub acpi_ok: bool,
    pub masks: (u8, u8),
    /// ACPI 发现的拓扑（找不到就是 None）。
    pub info: Option<acpi::Info>,
    /// APIC 初始化失败的原因（成功时为空串）。
    pub apic_err: &'static str,
}

/// **第一阶段**：屏蔽 8259A、装 IDT。必须在任何可能出异常的事情之前完成。
///
/// 这里**不碰 APIC**：APIC 的 MMIO 要建页表，建页表要帧分配器，
/// 而帧分配器这时候还没起来。早期就把两件事混在一起，
/// 结果是 APIC 永远启动失败、然后静默退回 PIC——查了半天才发现是顺序问题。
pub fn init() -> Result<Report, &'static str> {
    if frame::interrupts_enabled() {
        frame::disable();
    }
    // 先屏蔽 8259A：IDT 没装好之前任何外来中断都会落到未定义的门上。
    pic::mask_all();

    let idt_base = idt::init()?;
    idt::selftest(idt_base)?;

    Ok(Report {
        idt_base,
        idt_vectors: idt::LOADED,
    })
}

/// **第二阶段**：ACPI 发现 + APIC 接管。必须在帧分配器/页表就绪之后调用。
///
/// 找不到 ACPI 或 APIC 起不来时退回 8259A + PIT，并在报告里说明原因。
pub fn init_apic() -> ApicReport {
    let info = acpi::discover();
    let mut mode = Mode::Pic;
    let mut apic_id = 0u8;
    let mut cpu_count = 1usize;
    let mut timer_gsi = 0u32;
    let mut apic_err: &'static str = "";

    if let Some(i) = info {
        cpu_count = i.cpu_count;
        timer_gsi = i.timer_gsi;
        apic::set_gsi_base(i.ioapic_gsi_base);
        match apic::init(i.lapic_addr, i.ioapic_addr, i.cpu_count, i.timer_gsi) {
            Ok(rep) => {
                apic_id = rep.apic_id;
                mode = Mode::Apic;
                crate::kserialln!(
                    "       LAPIC 已使能：mmio={:#x} id={} 目标核数={} 定时器 GSI={}",
                    rep.lapic_addr,
                    rep.apic_id,
                    rep.cpu_count,
                    rep.timer_gsi
                );
            }
            Err(e) => {
                apic_err = e;
                pic::remap();
                pit::init();
            }
        }
    } else {
        apic_err = "找不到 ACPI 表";
        pic::remap();
        pit::init();
    }
    set_mode(mode);

    ApicReport {
        mode,
        apic_id,
        cpu_count,
        timer_gsi,
        acpi_ok: info.is_some(),
        masks: pic::read_masks(),
        info,
        apic_err,
    }
}

/// 打开中断。
pub fn enable() {
    frame::enable();
}

/// 关闭中断。
pub fn disable() {
    frame::disable();
}

/// 打开中断后在限定时间内等到 `n` 个 tick。返回实际等到的 tick 数。
///
/// 不用 `hlt`：定时器要是没接上，`hlt` 会永远睡下去，
/// 表现是"内核卡死"，看不出是定时器的问题。有界自旋超时后返回真实计数。
pub fn wait_ticks(n: u64, budget: u64) -> u64 {
    let start = timer::ticks();
    frame::enable();
    let mut spins = 0u64;
    while timer::ticks() - start < n {
        core::hint::spin_loop();
        spins += 1;
        if spins >= budget {
            break;
        }
    }
    frame::disable();
    timer::ticks() - start
}
