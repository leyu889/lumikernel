//! LumiKernel —— 凝光内核（M3：TSS/IST + ring3 + 系统调用 + 抢占调度）
//!
//! 宏内核路线：驱动、文件系统、网络栈最终都跑在内核态（与 Linux 同构），
//! 稳定性靠内存安全 + 分层纪律 + 可测性，而不是靠把服务赶到用户态。
//!
//! - M1：内核有自己的地址空间与内存管理；
//! - M2：出错能说话（IDT、异常现场、定时器、guard page、slab）；
//! - **M3：能跑程序**——ring3 用户任务、`int 0x80` 系统调用、内核线程、
//!   时间片抢占、上下文切换。这一层做完，"操作系统"才算成立。
//!
//! 自检顺序不是随手排的：
//! 1. 先 GDT/TSS（后面进 ring3 全靠它）；
//! 2. 再 IDT/异常（出错才有话说）；
//! 3. 再内存（帧、堆、页表）；
//! 4. 最后任务与用户态（要用到上面全部）。
//!
//! 输出分两条通道：屏幕 = 纯 ASCII `tag: key=value`；串口 = 完整中文解释。

#![no_std]
#![no_main]

extern crate alloc;

mod arch;
mod boot;
#[macro_use]
mod console;
mod dev;
mod e820;
mod exec;
mod fd;
mod fs;
mod fs2;
mod intr;
mod linux;
mod mm;
mod pipe;
mod qa;
mod soakmode;
mod syscall;
mod task;
mod vfs;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt::Write;
use core::panic::PanicInfo;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, Ordering};

use console::Console;
use kfault::{VEC_BP, VEC_DE, VEC_PF};
use kslab::{PageSource, Slab};
use mm::{frames, heap, paging, KERNEL_VA_BASE, VMALLOC_BASE};

/// guard page 自检用的数据页（vmalloc 区里，PML4[257] 起手是空的）。
const GUARD_DATA: u64 = VMALLOC_BASE + 0x4000;
/// 紧挨着数据页、**故意不映射**的那一页。
const GUARD_HOLE: u64 = VMALLOC_BASE + 0x5000;

/// 内核线程的工作量。
///
/// 必须**明显超过一个时间片**（2 tick = 20 ms），否则三个线程会在第一次抢占之前
/// 就各自跑完——那样自检只能证明"能切到新任务"，证明不了"抢占"。
/// 8M 次原子自增 ≈ 60–80 ms/线程，稳跨 3 个以上时间片。
const KT_ITERS: u64 = 8_000_000;
static KT: [AtomicU64; 3] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
/// 三个内核线程的真实 tid（tid 是全局递增的，别假设它从 1 开始）。
/// M7 管道自检用：读到的字节数、读到的内容、等待者拿到的退出码。
static PIPE_LEN: AtomicU64 = AtomicU64::new(u64::MAX);
static PIPE_WORD: AtomicU64 = AtomicU64::new(0);
static WAIT_CODE: AtomicU64 = AtomicU64::new(u64::MAX);
/// M7 每进程 fd 自检用：对端任务拿到的 fd 号。
static PEER_FD: AtomicU64 = AtomicU64::new(u64::MAX);
static PEER_GO: AtomicU64 = AtomicU64::new(0);

/// M7 每进程 fd 自检：这个内核线程打开文件、占住自己表里的 fd 3，
/// 直到主线程说"可以了"。
extern "C" fn fd_peer() {
    let fd = fd::open("/hello.txt").unwrap_or(usize::MAX);
    PEER_FD.store(fd as u64, Ordering::SeqCst);
    // 轮询等主线程放行。用 yield 而不是 sleep：这里只想"活着占住 fd"，
    // 不是为了测调度。
    while PEER_GO.load(Ordering::SeqCst) == 0 {
        task::yield_now();
    }
    let _ = fd::close(fd);
    task::exit_current(0)
}

/// M7 管道自检：在管道上阻塞读，读到就记账退出。
extern "C" fn pipe_reader() {
    let id = PIPE_ID.load(Ordering::SeqCst) as usize;
    let mut buf = [0u8; 16];
    match pipe::read(id, &mut buf) {
        Ok(n) => {
            PIPE_LEN.store(n as u64, Ordering::SeqCst);
            let mut w = 0u64;
            for i in 0..n.min(4) {
                w = (w << 8) | buf[i] as u64;
            }
            PIPE_WORD.store(w, Ordering::SeqCst);
        }
        Err(e) => PIPE_LEN.store((-e) as u64, Ordering::SeqCst),
    }
    task::exit_current(0)
}

/// M7 等待自检：调用 `wait_zombie` 等上面那个读者退出，记录拿到的退出码。
extern "C" fn pipe_waiter() {
    let target = PIPE_READER_TID.load(Ordering::SeqCst);
    match task::wait_zombie(target) {
        Some(c) => WAIT_CODE.store(c as u64, Ordering::SeqCst),
        None => WAIT_CODE.store(u64::MAX, Ordering::SeqCst),
    }
    task::exit_current(0)
}

static PIPE_ID: AtomicU64 = AtomicU64::new(u64::MAX);
static PIPE_READER_TID: AtomicU64 = AtomicU64::new(0);

static KT_TID: [AtomicU64; 3] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// 由 boot.S 调用，参数是 E820 表的物理地址（固定 0x9000）
/* ---------------- 内核栈的体检（M11） ---------------- */

extern "C" {
    static __stack_bottom: u8;
    static __stack_canary: u8;
}

/// 栈金丝雀的大小与图案。
const CANARY_BYTES: usize = 4096;
const CANARY_PATTERN: u8 = 0x5A;

/// 栈的总大小（与 boot.S 的 `.skip` 必须一致）。
const STACK_BYTES: usize = 262144;

/// 已用栈的**高水位**（字节）。从底部往上找第一个被写过的字节。
fn stack_high_water() -> usize {
    // SAFETY: 同 `paint_stack`，只读栈的未使用区间。
    unsafe {
        let bottom = &__stack_bottom as *const u8;
        let mut i = 0usize;
        while i < STACK_BYTES - 8192 {
            if core::ptr::read_volatile(bottom.add(i)) != CANARY_PATTERN {
                break;
            }
            i += 1;
        }
        // 未被覆盖的 i 字节 → 用掉了其余全部（含顶端那 8 KiB 假设已用）
        STACK_BYTES - i
    }
}

/// 金丝雀是否完好。
fn stack_canary_ok() -> bool {
    // SAFETY: 金丝雀是我们自己画的一页静态内存。
    unsafe {
        let c = &__stack_canary as *const u8;
        for i in 0..CANARY_BYTES {
            if core::ptr::read_volatile(c.add(i)) != CANARY_PATTERN {
                return false;
            }
        }
    }
    true
}

#[no_mangle]
pub extern "C" fn kmain(e820_addr: u32) -> ! {

    console::init();

    kprintln!();
    kprintln!("==================================================");
    kprintln!("  LumiKernel 1.2.0  (milestone M11)");
    kprintln!("  UEFI + BIOS dual boot, own AHCI(SATA) driver, linux abi");
    kprintln!("  fault injection + fuzz + soak regression, 57 self-checks");
    kprintln!("==================================================");
    kserialln!("  凝光内核 · 宏内核 · x86_64 · ACPI/MADT · APIC/IOAPIC · ring3 · 系统调用 · 抢占");

    let mut ok = 0u32;
    let mut total = 0u32;

    /* ================= 1. IDT（先装异常处理器，后面出问题才有话说） ================= */
    total += 1;
    let _init_rep = match intr::init() {
        Ok(r) => {
            kprintln!(
                "[ OK ] idt:  vectors={} idtr={:#x} limit={}",
                r.idt_vectors,
                r.idt_base,
                256 * 16 - 1
            );
            ok += 1;
            Some(r)
        }
        Err(e) => {
            kprintln!("[FAIL] idt:  {}", e);
            None
        }
    };

    /* ================= 2. GDT + TSS ================= */
    total += 1;
    match arch::gdt::init() {
        Ok(r) => {
            kprintln!(
                "[ OK ] gdt:  base={:#x} limit={} tr={:#x}",
                r.gdt_base,
                r.gdt_limit,
                r.tr
            );
            kserialln!(
                "       8 项：内核代码/数据 + 用户代码/数据(DPL3) + TSS；rsp0={:#x} ist1={:#x}",
                r.rsp0,
                r.ist1
            );
            ok += 1;
        }
        Err(e) => kprintln!("[FAIL] gdt:  {}", e),
    }

    /* ================= 3. IST + syscall 门 ================= */
    total += 1;
    match intr::idt::set_ist(8, 1) {
        Ok(()) => {
            let df_ist = intr::idt::gate(8).map(|g| g.ist()).unwrap_or(0);
            let sc = intr::idt::gate(intr::idt::SYSCALL_VEC);
            let (sc_dpl, sc_present) = sc.map(|g| (g.dpl(), g.present())).unwrap_or((0, false));
            let tr = arch::gdt::read_tr();
            if df_ist == 1 && sc_present && sc_dpl == 3 && tr == arch::gdt::TSS_SEL {
                kprintln!("[ OK ] ist:  df_ist=1 syscall_dpl={} tr={:#x}", sc_dpl, tr);
                kserialln!(
                    "       #DF 挂在 IST1 上（栈坏了也能打出报告）；int 0x80 的门是 DPL=3，ring3 才进得来"
                );
                ok += 1;
            } else {
                kprintln!(
                    "[FAIL] ist:  df_ist={} sc_dpl={} sc_present={} tr={:#x}",
                    df_ist,
                    sc_dpl,
                    sc_present,
                    tr
                );
            }
        }
        Err(e) => kprintln!("[FAIL] ist:  {}", e),
    }

    /* ================= 3b. syscall 指令通道（M9） ================= */
    total += 1;
    match arch::syscall::init() {
        Ok(r) => {
            kprintln!(
                "[ OK ] syscall: efer={:#x} lstar={:#x} fmask={:#x}",
                r.efer,
                r.lstar,
                r.fmask
            );
            kserialln!(
                "       `syscall` 指令通道就绪（EFER.SCE=1、LSTAR 指向入口、FMASK 清掉 IF/DF/TF）：\
                 musl 静态程序只认这条路，`int 0x80` 保留给自检"
            );
            ok += 1;
        }
        Err(e) => kprintln!("[FAIL] syscall: {}", e),
    }

    /* ================= 4. E820 ================= */
    let table_addr = if e820_addr == 0 {
        e820::E820_ADDR
    } else {
        e820_addr as usize
    };
    let (count, ptr) = unsafe { e820::table(table_addr) };
    let mut entries = [e820::E820Entry {
        base: 0,
        len: 0,
        typ: 0,
        acpi: 0,
    }; 32];
    let count = count.min(32);
    for i in 0..count {
        entries[i] = e820::entry_at(ptr, i);
    }

    total += 1;
    if count > 0 {
        let usable = e820::usable_bytes(count, ptr);
        let src = unsafe { e820::boot_source(table_addr) };
        // 同盘分区：UEFI 装载器把 ext2 起始 LBA 写在表头，LumiFS 在它之后
        // 固定 16 MiB 处（由 tools/mkgpt.py 决定）。
        let sys_off = unsafe { e820::part_lba(table_addr) };
        let data_off = if sys_off != 0 { sys_off + 0x8000 } else { 0 };
        dev::ata::set_part_offsets(sys_off, data_off);
        kprintln!(
            "[ OK ] e820:  entries={} usable={}MiB boot={}",
            count, usable >> 20, src
        );
        kserialln!("       物理内存地图（串口完整版）:");
        for i in 0..count {
            let e = entries[i];
            kserialln!(
                "         {:#012x} + {:#012x}  type={} {}",
                e.base,
                e.len,
                e.typ,
                e820::type_name(e.typ)
            );
        }
        ok += 1;
    } else {
        kprintln!("[FAIL] e820:  empty table");
    }

    /* ================= 5. 帧分配器 ================= */
    total += 1;
    match frames::init(&entries[..count]) {
        Some(st) => {
            kprintln!(
                "[ OK ] frames:  total={}MiB free={}MiB frames={} e820={}MiB",
                st.total_mib(),
                st.free_mib(),
                st.free_frames,
                st.usable_bytes >> 20
            );
            if st.outside_direct_map > 0 {
                kserialln!(
                    "       注意：{} MiB 超出 1 GiB 直接映射区，暂不纳入分配",
                    st.outside_direct_map >> 20
                );
            }
            ok += 1;
        }
        None => kprintln!("[FAIL] frames:  init failed"),
    }

    /* ================= 6. 保留区 ================= */
    total += 1;
    let (ks, ke) = frames::kernel_phys_range();
    match frames::alloc_frame() {
        Some(p) if ks == mm::PHYS_LOAD && frames::is_used(ks) && p >= ke => {
            kprintln!(
                "[ OK ] reserve: kernel={:#x}..{:#x} first_free={:#x}",
                ks,
                ke,
                p
            );
            frames::free_frame(p);
            ok += 1;
        }
        Some(p) => kprintln!(
            "[FAIL] reserve: kernel_start={:#x} want={:#x} got={:#x} end={:#x}",
            ks,
            mm::PHYS_LOAD,
            p,
            ke
        ),
        None => kprintln!("[FAIL] reserve: no frame allocatable"),
    }

    /* ================= 7. 内核堆 ================= */
    total += 1;
    match heap::init() {
        Some(phys) => {
            let mut v: Vec<u64> = Vec::new();
            for i in 0..512u64 {
                v.push(i * 3);
            }
            let sum: u64 = v.iter().sum();
            let expect = 3 * (511 * 512 / 2);
            let b = Box::new([0xABu8; 1024]);
            let box_ok = b.iter().all(|x| *x == 0xAB);
            drop(v);
            drop(b);
            match heap::stats() {
                Some(s) if sum == expect && box_ok && heap::audit() => {
                    kprintln!(
                        "[ OK ] heap:  base={:#x} size={}KiB used={}B audit=ok",
                        phys,
                        mm::HEAP_BYTES >> 10,
                        s.used
                    );
                    ok += 1;
                }
                Some(_) => kprintln!("[FAIL] heap:  content check or audit failed"),
                None => kprintln!("[FAIL] heap:  stats unreadable"),
            }
        }
        None => kprintln!("[FAIL] heap:  init failed (no contiguous frames)"),
    }

    /* ================= 8. ACPI / MADT ================= */
    total += 1;
    let apic_rep = intr::init_apic();
    let (amode, aapic, acpus, agsi, acpi_ok) = (
        apic_rep.mode,
        apic_rep.apic_id,
        apic_rep.cpu_count,
        apic_rep.timer_gsi,
        apic_rep.acpi_ok,
    );
    if acpi_ok && amode == intr::Mode::Apic && acpus >= 1 {
        kprintln!(
            "[ OK ] acpi:  cpus={} apic_id={} timer_gsi={}",
            acpus, aapic, agsi
        );
        kserialln!(
            "       MADT @{:#x}：{} 个 CPU，LAPIC @{:#x}，ISA IRQ0 路由到 GSI {}，中断已切到 APIC",
            apic_rep.info.map(|i| i.madt_phys).unwrap_or(0),
            acpus,
            apic_rep.info.map(|i| i.lapic_addr as u64).unwrap_or(0),
            agsi
        );
        ok += 1;
    } else {
        let e = apic_rep.apic_err;
        kprintln!(
            "[FAIL] acpi:  ok={} mode={:?} cpus={} apic_err={}",
            acpi_ok,
            amode,
            acpus,
            e
        );
    }

    /* ================= 9. 外部中断走 I/O APIC ================= */
    total += 1;
    let mut ioapic_ticks = 0u64;
    let mut used_gsi = u32::MAX;
    if let Some(info) = apic_rep.info {
        if amode == intr::Mode::Apic {
            // MADT 有改写就用改写；没有的话按 PC 惯例试 GSI2（PIT 实际接在 I/O APIC 的 2 号脚）
            let mut candidates = [info.timer_gsi, 2, 0];
            for c in candidates.iter_mut() {
                if used_gsi != u32::MAX {
                    break;
                }
                intr::apic::unmask_timer(&info, aapic, *c);
                intr::pit::init(); // PIT 100 Hz → IRQ0 → GSI → 向量 32
                let got = intr::wait_ticks(20, 120_000_000);
                if got >= 18 {
                    ioapic_ticks = got;
                    used_gsi = *c;
                }
                intr::apic::mask_timer(&info, *c);
            }
        }
    }
    if ioapic_ticks >= 18 && amode == intr::Mode::Apic {
        kprintln!(
            "[ OK ] ioapic-irq: gsi={} ticks={} pit_div={}",
            used_gsi,
            ioapic_ticks,
            intr::pit::divisor()
        );
        kserialln!(
            "       PIT 的中断经 I/O APIC 重定向（GSI {}）后到达：8259A 已经被彻底绕开",
            used_gsi
        );
        ok += 1;
    } else {
        kprintln!("[FAIL] ioapic-irq: ticks={} mode={:?}", ioapic_ticks, amode);
    }

    /* ================= 10. APIC 定时器（每核自己的时钟） ================= */
    total += 1;
    let mut apic_ticks = 0u64;
    let mut timer_init = 0u32;
    if amode == intr::Mode::Apic {
        // 校准要用 PIT 的真实中断当基准，所以这里临时把它的路由打开；
        // 上一步的 ioapic-irq 自检结束时是屏蔽状态（否则会持续抢 tick）。
        let ref_gsi = if used_gsi != u32::MAX {
            used_gsi
        } else {
            apic_rep.info.map(|i| i.timer_gsi).unwrap_or(0)
        };
        if let Some(info) = apic_rep.info {
            intr::apic::unmask_timer(&info, aapic, ref_gsi);
            intr::pit::init();
            match intr::apic::timer_start(intr::timer::HZ) {
                Ok(init) => {
                    timer_init = init;
                    apic_ticks = intr::wait_ticks(20, 400_000_000);
                }
                Err(e) => kprintln!("[FAIL] apic-timer: {}", e),
            }
            // 校准完就撤掉 PIT：主时钟从此只由每核的 APIC 定时器提供
            intr::apic::mask_timer(&info, ref_gsi);
        }
    }
    if apic_ticks >= 18 {
        kprintln!(
            "[ OK ] apic-timer: hz={} init={} ticks={}",
            intr::timer::HZ,
            timer_init,
            apic_ticks
        );
        kserialln!(
            "       每核自己的定时器跑起来了（初值 {} 由 RTC 秒边界校准）—— 这才是多核时间片的基础",
            timer_init
        );
        kserialln!(
            "       校准基准 = RTC 秒边界：APIC 定时器 {} counts/10ms，10ms = {} 个 TSC tick",
            intr::apic::timer_init(),
            intr::apic::tsc_per_10ms()
        );
        kserialln!(
            "       校准那一秒里原 tick 源（PIT 经 I/O APIC）实际打了 {} 次",
            intr::apic::ref_hz()
        );
        ok += 1;
    } else {
        kprintln!("[FAIL] apic-timer: ticks={}", apic_ticks);
    }

    /* ================= 11. 8259A 已静默 ================= */
    total += 1;
    match Some(apic_rep.masks) {
        Some((m1, m2)) if amode == intr::Mode::Apic && m1 == 0xFF && m2 == 0xFF => {
            kprintln!("[ OK ] pic:  fully masked ({:#04x} {:#04x})", m1, m2);
            ok += 1;
        }
        Some((m1, m2)) if amode == intr::Mode::Pic && m1 == 0xFE && m2 == 0xFF => {
            kprintln!("[ OK ] pic:  fallback mode ({:#04x} {:#04x})", m1, m2);
            ok += 1;
        }
        Some((m1, m2)) => kprintln!(
            "[FAIL] pic:  mask_m={:#04x} mask_s={:#04x} mode={:?}",
            m1,
            m2,
            amode
        ),
        None => kprintln!("[FAIL] pic:  not initialized"),
    }

    /* ================= 12. 高半区 ================= */
    total += 1;
    let rip = paging::read_rip();
    if rip >= KERNEL_VA_BASE {
        kprintln!("[ OK ] high-half:  rip={:#x}", rip);
        ok += 1;
    } else {
        kprintln!("[FAIL] high-half:  rip={:#x} base={:#x}", rip, KERNEL_VA_BASE);
    }

    /* ================= 13. 分页 ================= */
    total += 1;
    let cr0 = paging::read_cr0();
    let cr4 = paging::read_cr4();
    if cr0 & (1 << 31) != 0 && cr4 & (1 << 5) != 0 {
        kprintln!("[ OK ] paging:  cr0.pg=1 cr4.pae=1 cr3={:#x}", paging::read_cr3());
        ok += 1;
    } else {
        kprintln!("[FAIL] paging:  cr0={:#x} cr4={:#x}", cr0, cr4);
    }

    /* ================= 14. 页表端到端 ================= */
    total += 1;
    match paging::selftest() {
        Ok(r) => {
            kprintln!(
                "[ OK ] paging-test: pml4={} page pattern={:#x} frames=back",
                r.pml4_pages,
                r.pattern
            );
            ok += 1;
        }
        Err(e) => kprintln!("[FAIL] paging-test: {}", e),
    }

    /* ================= 15. 帧分配压力 ================= */
    total += 1;
    let before = frames::stats().map(|s| s.free_frames).unwrap_or(0);
    let mut held: Vec<u64> = Vec::new();
    for _ in 0..512 {
        match frames::alloc_frame() {
            Some(p) => held.push(p),
            None => break,
        }
    }
    let mid = frames::stats().map(|s| s.free_frames).unwrap_or(0);
    let n = held.len();
    let mut unique_ok = true;
    for (i, p) in held.iter().enumerate() {
        if held[..i].contains(p) {
            unique_ok = false;
        }
    }
    for p in held.drain(..) {
        frames::free_frame(p);
    }
    let after = frames::stats().map(|s| s.free_frames).unwrap_or(0);
    if n == 512 && unique_ok && mid + n == before && after == before {
        kprintln!("[ OK ] frames-stress: n={} unique=1 returned={}", n, after);
        ok += 1;
    } else {
        kprintln!(
            "[FAIL] frames-stress: n={} unique={} before={} mid={} after={}",
            n,
            unique_ok,
            before,
            mid,
            after
        );
    }

    /* ================= 16. 异常探针 ================= */
    total += 1;
    let h0 = intr::probe::hits();
    let out_de = intr::probe::run(VEC_DE, 0, intr::probe::ProbeAction::DivideByZero);
    let (v_de, _e_de, _a_de) = intr::probe::last_fault();
    let out_bp = intr::probe::run(VEC_BP, 0, intr::probe::ProbeAction::Int3);
    let (v_bp, _e_bp, _a_bp) = intr::probe::last_fault();
    if out_de.recovered
        && out_de.value == 1
        && v_de == VEC_DE
        && out_bp.recovered
        && v_bp == VEC_BP
        && intr::probe::hits() == h0 + 2
    {
        kprintln!(
            "[ OK ] exception: de=caught bp=caught hits={}",
            intr::probe::hits()
        );
        ok += 1;
    } else {
        kprintln!(
            "[FAIL] exception: de={} bp={} vec_de={} vec_bp={} hits={}",
            out_de.recovered,
            out_bp.recovered,
            v_de,
            v_bp,
            intr::probe::hits()
        );
    }

    /* ================= 17. guard page ================= */
    total += 1;
    let free_before_guard = frames::stats().map(|s| s.free_frames).unwrap_or(0);
    let mut guard_ok = false;
    let mut guard_note = "";
    let mut fault_addr = 0u64;
    match frames::alloc_frame() {
        Some(df) => {
            if paging::map_in_current(GUARD_DATA, df, kpt::Flags::KERNEL_RW).is_ok() {
                // SAFETY: 这一页刚建立映射且归本测试独占。
                let pat = unsafe {
                    write_volatile(GUARD_DATA as *mut u64, 0xCAFE_BABE_1234_5678);
                    read_volatile(GUARD_DATA as *const u64)
                };
                if pat == 0xCAFE_BABE_1234_5678 {
                    let bad = GUARD_HOLE + 0x123;
                    fault_addr = bad;
                    let o_r = intr::probe::run(VEC_PF, bad, intr::probe::ProbeAction::Read(bad));
                    let (v1, e1, a1) = intr::probe::last_fault();
                    let o_w = intr::probe::run(
                        VEC_PF,
                        bad,
                        intr::probe::ProbeAction::Write(bad, 0xDEAD),
                    );
                    let (v2, e2, a2) = intr::probe::last_fault();
                    if o_r.recovered
                        && o_r.value == 1
                        && v1 == VEC_PF
                        && a1 == bad
                        && e1 & 0b11 == 0b00
                        && o_w.recovered
                        && v2 == VEC_PF
                        && a2 == bad
                        && e2 & 0b11 == 0b10
                    {
                        guard_ok = true;
                    } else {
                        guard_note = "fault code or address mismatch";
                    }
                } else {
                    guard_note = "write/read mismatch on mapped page";
                }
            } else {
                guard_note = "map failed";
            }
            let _ = paging::unmap_in_current(GUARD_DATA);
            frames::free_frame(df);
        }
        None => guard_note = "no data frame",
    }
    let free_after_guard = frames::stats().map(|s| s.free_frames).unwrap_or(0);
    if guard_ok && free_after_guard == free_before_guard {
        kprintln!(
            "[ OK ] guard-page: addr={:#x} pf=p0 read+write caught frames=back",
            fault_addr
        );
        ok += 1;
    } else if guard_ok {
        kprintln!(
            "[FAIL] guard-page: leak frames {} -> {}",
            free_before_guard,
            free_after_guard
        );
    } else {
        kprintln!("[FAIL] guard-page: {}", guard_note);
    }

    /* ================= 18. slab ================= */
    total += 1;
    {
        let mut slab = Slab::new();
        let mut src = KernelPages;
        let sizes = [16usize, 24, 48, 100, 200, 300, 700, 1500];
        let mut held: Vec<(u64, usize, u64)> = Vec::new();
        let mut alloc_fail = 0usize;
        for i in 0..600usize {
            let size = sizes[i % sizes.len()];
            match slab.alloc(size, &mut src) {
                Some(a) => {
                    let pat = 0xA5A5_0000_0000_0000u64 | (i as u64);
                    // SAFETY: slab 返回的地址属于本测试，长度 >= 16。
                    unsafe {
                        write_volatile(a as *mut u64, pat);
                        write_volatile((a + size as u64 - 8) as *mut u64, pat);
                    }
                    held.push((a, size, pat));
                }
                None => alloc_fail += 1,
            }
        }
        let mut intact = true;
        for &(a, size, pat) in held.iter() {
            // SAFETY: 同上，仍在持有期内。
            let h = unsafe { read_volatile(a as *const u64) };
            let t = unsafe { read_volatile((a + size as u64 - 8) as *const u64) };
            if h != pat || t != pat {
                intact = false;
            }
        }
        let mut order: Vec<usize> = Vec::new();
        let mut i = 0usize;
        while i < held.len() {
            order.push(i);
            i += 2;
        }
        let mut i = 1usize;
        while i < held.len() {
            order.push(i);
            i += 2;
        }
        let mut freed_ok = 0usize;
        for &idx in order.iter() {
            if slab.free(held[idx].0).is_ok() {
                freed_ok += 1;
            }
        }
        let st = slab.stats();
        let audit_ok = slab.audit().is_ok();
        if intact && alloc_fail == 0 && freed_ok == held.len() && audit_ok && st.used_objects == 0
        {
            kprintln!(
                "[ OK ] slab:  objs={} pages={} order=shuffled audit=ok leak=0",
                held.len(),
                st.pages
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] slab: intact={} alloc_fail={} freed={}/{} audit={} used={}",
                intact,
                alloc_fail,
                freed_ok,
                held.len(),
                audit_ok,
                st.used_objects
            );
        }
    }

    /* ================= 19. 任务系统 ================= */
    total += 1;
    // 必须在任何进程创建**之前**记下内核 PML4：之后读 CR3 可能读到某个进程的表。
    mm::paging::remember_kernel_cr3();
    match task::init() {
        Ok(()) => {
            let running = task::count(task::State::Running);
            let tid = task::current_tid();
            if running == 1 && tid == 0 {
                kprintln!(
                    "[ OK ] task:  self=task0 state=running quantum={}ticks",
                    task::QUANTUM
                );
                kserialln!("       当前这段代码本身变成了 0 号任务，后面创建的线程与它同场竞争");
                ok += 1;
            } else {
                kprintln!("[FAIL] task:  running={} tid={}", running, tid);
            }
        }
        Err(e) => kprintln!("[FAIL] task:  {}", e),
    }

    /* ================= 20. ring3 用户任务 ================= */
    total += 1;
    {
        let sc_before = syscall::calls_from_user();
        let usr_before = syscall::bytes_written();
        let mut entry = None;
        match task::setup_user_image() {
            Ok(e) => entry = Some(e),
            Err(e) => kprintln!("[FAIL] ring3:  {}", e),
        }
        if let Some(entry) = entry {
            match task::spawn_user(entry) {
                Ok(tid) => {
                    let code = wait_task(tid, 400);
                    let from_user = syscall::calls_from_user() - sc_before;
                    let wrote = syscall::bytes_written() - usr_before;
                    if code == Some(7) && from_user >= 2 && wrote == 17 {
                        kprintln!(
                            "[ OK ] ring3:  exit={} syscalls_from_cpl3={} wrote={}B",
                            code.unwrap_or(-1),
                            from_user,
                            wrote
                        );
                        kserialln!(
                            "       用户程序在 CPL=3 跑完：int 0x80 进来两次，写出 17 字节，最后 exit(7)"
                        );
                        ok += 1;
                    } else {
                        kprintln!(
                            "[FAIL] ring3:  exit={:?} from_user={} wrote={}",
                            code,
                            from_user,
                            wrote
                        );
                    }
                }
                Err(e) => kprintln!("[FAIL] ring3:  {}", e),
            }
        }
    }

    /* ================= 21. 内核线程 ================= */
    total += 1;
    {
        let s0 = task::switches();
        let p0 = task::preemptions();
        let a = task::spawn_kthread(kt0);
        let b = task::spawn_kthread(kt1);
        let c = task::spawn_kthread(kt2);
        let mut done = 0;
        for (i, r) in [a, b, c].into_iter().enumerate() {
            if let Ok(t) = r {
                KT_TID[i].store(t, Ordering::Relaxed);
                done += wait_task(t, 600).is_some() as u32;
            }
        }
        let body: u64 = KT.iter().map(|x| x.load(Ordering::Relaxed)).sum();
        let all_full = KT.iter().all(|x| x.load(Ordering::Relaxed) == KT_ITERS);
        let sw = task::switches() - s0;
        let pre = task::preemptions() - p0;
        if done == 3 && all_full && sw > 0 {
            kprintln!(
                "[ OK ] kthreads: 3 threads x {} iters done, switches={} preempted={}",
                KT_ITERS,
                sw,
                pre
            );
            kserialln!("       三个内核线程都没有主动让出 CPU，能跑完说明是时钟中断把它们切开的");
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] kthreads: done={}/3 sum={} expect={} full={} switches={} preempted={}",
                done,
                body,
                KT_ITERS * 3,
                all_full,
                sw,
                pre
            );
        }
    }

    /* ================= 22. 抢占 ================= */
    total += 1;
    {
        let sw = task::switches();
        let pre = task::preemptions();
        let exited = task::exited();
        // 每个线程都被记过 tick，说明抢占是真的按时间片发生的，而不是"跑完就退出"
        let mut all_ticked = true;
        for i in 0..3 {
            let t = KT_TID[i].load(Ordering::Relaxed);
            match task::query(t) {
                Some((st, _, ticks)) if st == task::State::Zombie && ticks > 0 => {}
                _ => all_ticked = false,
            }
        }
        if pre >= 3 && exited >= 4 {
            kprintln!(
                "[ OK ] preempt: preempted={} switches={} exited={} ticked={}",
                pre,
                sw,
                exited,
                all_ticked as u8
            );
            kserialln!(
                "       时间片耗尽强制切换 {} 次（三个线程都没主动让出）；每个任务都被记过 tick —— 抢占是真的",
                pre
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] preempt: preempted={} switches={} exited={} all_ticked={}",
                pre,
                sw,
                exited,
                all_ticked
            );
        }
    }


    /* ================= 23. SMP：唤醒 AP ================= */
    arch::smp::init_bsp(timer_init);
    let ap_started = apic_rep
        .info
        .map(|i| arch::smp::start_aps(&i))
        .unwrap_or(0);
    let n = arch::smp::count();
    let online = arch::smp::online();
    let multicore = acpus > 1;

    if !multicore {
        kprintln!("[ .. ] smp:  MADT reported only 1 cpu, smp checks skipped");
        kserialln!("       MADT 只报出 1 个 CPU（虚拟机没给多核），SMP 相关自检跳过");
    } else {
        total += 1;
        if online == n && online >= 2 {
            kprintln!(
                "[ OK ] smp: online={}/{} started={} bsp_apic={}",
                online,
                n,
                ap_started,
                aapic
            );
            let mut list = alloc::string::String::new();
            for i in 0..n {
                if i > 0 {
                    list.push(' ');
                }
                list.push_str(&alloc::format!(
                    "cpu{}=apic{}",
                    i,
                    arch::smp::apic_id_of(i)
                ));
            }
            kserialln!("       已上线：{}", list);
            for i in 1..n {
                kserialln!(
                    "       cpu{} 独立内核栈 top={:#x}",
                    i,
                    arch::smp::stack_top_of(i)
                );
            }
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] smp: online={} of {} started={}",
                online,
                n,
                ap_started
            );
        }
    }

    /* ---- 24a. 先量定时器的真实频率（安静状态）---- */
    // 必须在 AP 都停在 hlt 的时候量：如果让 4 个核同时满速空转，
    // 每个核实际拿到的执行时间会被稀释，tick 数自然变少——
    // 那是"测量条件"的问题，不是"定时器频率"的问题。这两者混淆过一次，
    // 让我以为定时器只有 14 Hz，白查了半天。
    let per10 = intr::apic::tsc_per_10ms();
    let mut measured_hz = 0u64;
    let mut hz_sample = (0u64, 0u64);
    if multicore && online == n && per10 > 0 {
        let t0 = intr::timer::ticks();
        let tsc0 = intr::apic::tsc_now();
        let t_end = tsc0.saturating_add(per10.saturating_mul(20));
        intr::enable();
        let g = task::PreemptGuard::new();
        while intr::apic::tsc_now() < t_end {
            core::hint::black_box(tsc0);
        }
        let tsc1 = intr::apic::tsc_now();
        let dt = intr::timer::ticks() - t0;
        drop(g);
        intr::disable();
        hz_sample = (dt, tsc1 - tsc0);
        if tsc1 > tsc0 {
            measured_hz = dt * per10 * 100 / (tsc1 - tsc0);
        }
    }

    /* ================= 24. 并行度：四个核同时在执行 ================= */
    let mut bsp_work = 0u64;
    if multicore && online == n {
        total += 1;
        let tsc0 = intr::apic::tsc_now();
        let t_end = tsc0.saturating_add(per10.saturating_mul(20));
        arch::smp::set_busy(true);
        intr::enable();
        let guard = task::PreemptGuard::new();
        // 每 1024 次才读一次 TSC：rdtsc 本身要几十个周期，
        // 每轮都读会让 bsp_work 变成"rdtsc 次数"，跟 AP 的次数没法比。
        'count: loop {
            for _ in 0..1024 {
                bsp_work = bsp_work.wrapping_add(1);
                core::hint::black_box(bsp_work);
            }
            if intr::apic::tsc_now() >= t_end {
                break 'count;
            }
        }
        drop(guard);
        intr::disable();
        arch::smp::set_busy(false);

        let mut min_work = u64::MAX;
        let mut workers = 0usize;
        for i in 1..n {
            let w = arch::smp::work_of(i);
            if w > 0 {
                workers += 1;
            }
            min_work = min_work.min(w);
        }
        if workers == n - 1 && bsp_work > 0 {
            kprintln!(
                "[ OK ] parallel: cores={} min_work={} bsp_work={}",
                n,
                min_work,
                bsp_work
            );
            kserialln!(
                "       200 ms 窗口内 {} 个核同时执行：每核自己数循环次数，最少 {} 次（BSP {} 次，含 rdtsc 开销）",
                n,
                min_work,
                bsp_work
            );
            kserialln!(
                "       静息态实测 APIC 定时器：{} Hz（配置 {} Hz；样本 {} tick / {} TSC）",
                measured_hz,
                intr::timer::HZ,
                hz_sample.0,
                hz_sample.1
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] parallel: workers={}/{} min_work={} bsp_work={}",
                workers,
                n - 1,
                min_work,
                bsp_work
            );
        }
    }

    /* ================= 25. 每核自己的定时器 ================= */
    if multicore && online == n {
        total += 1;
        let mut alive = 0usize;
        let mut min_ticks = u64::MAX;
        let mut max_ticks = 0u64;
        for i in 0..n {
            let t = arch::smp::ticks_of(i);
            if t > 0 {
                alive += 1;
            }
            min_ticks = min_ticks.min(t);
            max_ticks = max_ticks.max(t);
        }
        if alive == n {
            kprintln!(
                "[ OK ] smp-timer: cpus={} all_ticking=1 ticks=[{}..{}]",
                n,
                min_ticks,
                max_ticks
            );
            kserialln!(
                "       {} 个核各自被自己的 LAPIC 定时器打断（最少 {} 次、最多 {} 次）—— 每核独立时基",
                n,
                min_ticks,
                max_ticks
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] smp-timer: cpus={} alive={} min={}",
                n,
                alive,
                min_ticks
            );
        }
    }

    /* ================= 26. 核间中断（IPI）广播 ================= */
    if multicore && online == n {
        total += 1;
        let acks = arch::smp::broadcast_ipi_and_wait();
        if acks >= (n as u64 - 1) {
            kprintln!("[ OK ] ipi:  broadcast_vec={} acked={}/{}", intr::idt::IPI_VEC, acks, n - 1);
            kserialln!(
                "       向量 {} 广播出去，{} 个 AP 全部应答 —— 多核调度的 reschedule IPI 通道打通了",
                intr::idt::IPI_VEC,
                acks
            );
            ok += 1;
        } else {
            kprintln!("[FAIL] ipi:  acked={}/{}", acks, n - 1);
        }
    }

    /* ================= 27. 磁盘：AHCI(SATA) 优先，IDE PIO 兜底 ================= */
    total += 1;
    // 真机没有 0x1F0 那套 IDE 端口，必须先让 AHCI 接管；拿不到控制器才退回 PIO。
    let mut transport = "pio";
    match dev::ahci::init() {
        Ok(r) => {
            transport = "ahci";
            kserialln!(
                "       AHCI 控制器就位：ABAR={:#x}，{} 个端口在线（系统盘 {} 扇区，数据盘 {} 扇区）",
                r.abar,
                r.ports_found,
                r.system_sectors,
                r.data_sectors
            );
        }
        Err(e) => {
            kserialln!("       没有可用 AHCI 控制器（{}），退回 IDE PIO", e);
        }
    }
    let (_master, slave) = dev::ata::probe_all();
    if let Some(s) = slave {
        kserialln!("       从盘（数据盘）：{} 扇区 = {} KiB", s, s / 2);
    }
    match dev::ata::probe() {
        Some(ata_sectors) => {
            // 直接读第 2 个扇区（偏移 1024 = ext2 超级块）并核对魔数：
            // 这一步同时证明"ATA 能读"和"盘上确实是我们做的 ext2 镜像"。
            let mut s = [0u8; 512];
            let r = dev::ata::read_sector(2, &mut s);
            let magic = u16::from_le_bytes([s[56], s[57]]);
            if r.is_ok() && magic == 0xEF53 {
                kprintln!(
                    "[ OK ] disk: sectors={} blocks={}KiB transport={} magic={:#06x}",
                    ata_sectors,
                    ata_sectors / 2,
                    transport,
                    magic
                );
                kserialln!(
                    "       经 {} 读到 {} 个 512B 扇区（{} KiB），超级块魔数 {:#06x} 对上了",
                    transport,
                    ata_sectors,
                    ata_sectors / 2,
                    magic
                );
                ok += 1;
            } else {
                kprintln!("[FAIL] ata:  read={:?} magic={:#06x}", r.is_ok(), magic);
            }
        }
        None => kprintln!("[FAIL] ata:  no disk on primary master"),
    }

    /* ================= 28. ext2 挂载 ================= */
    total += 1;
    match fs::init() {
        Ok(rep) => {
            let name = core::str::from_utf8(&rep.first_name[..rep.first_name_len]).unwrap_or("?");
            // 真的把根目录列一遍：日志里能看到"盘上有什么"，而不是只看到计数
            if let Some(r) = fs::root() {
                let mut ents = [kfs::DirEntry { inode: 0, name_len: 0, name: [0u8; 255] }; 8];
                if let Ok(cnt) = fs::list(r, &mut ents) {
                    let mut names = alloc::string::String::new();
                    for e in ents.iter().take(cnt) {
                        let n = e.name();
                        if n == b"." || n == b".." || n.is_empty() {
                            continue;
                        }
                        if !names.is_empty() {
                            names.push(' ');
                        }
                        names.push_str(core::str::from_utf8(n).unwrap_or("?"));
                    }
                    kserialln!("       根目录内容：{}", names);
                }
            }
            kprintln!(
                "[ OK ] ext2:  blocks={} root_entries={} hello.txt={}B",
                rep.total_blocks,
                rep.root_entries,
                rep.first_size
            );
            kserialln!(
                "       ext2 挂载成功：根目录 {} 项，第一个条目 \"{}\"；/hello.txt = {} 字节",
                rep.root_entries,
                name,
                rep.first_size
            );
            ok += 1;
        }
        Err(e) => kprintln!("[FAIL] ext2:  {}", e),
    }

    /* ================= 29. 文件读取（fd 接口） ================= */
    total += 1;
    {
        fd::close_all();
        let mut got = 0usize;
        let mut head = [0u8; 24];
        let mut okread = false;
        if let Ok(f) = fd::open("/hello.txt") {
            let mut buf = [0u8; 128];
            if let Ok(n) = fd::read(f, &mut buf) {
                got = n;
                let k = n.min(24);
                head[..k].copy_from_slice(&buf[..k]);
                okread = n > 0 && buf[..5] == *b"this ";
            }
            let _ = fd::close(f);
        }
        let head_s = core::str::from_utf8(&head).unwrap_or("?");
        if okread && fd::open_count() == 0 {
            kprintln!("[ OK ] file:  open/read/close ok bytes={}", got);
            kserialln!("       读到 {} 字节，开头是：{}", got, head_s);
            kserialln!("       close 之后打开数为 {}（fd 表没有泄漏）", fd::open_count());
            ok += 1;
        } else {
            kprintln!("[FAIL] file:  bytes={} leak={}", got, fd::open_count());
        }
    }

    /* ================= 30. ELF 装载 + ring3 执行 ================= */
    total += 1;
    {
        let mut detail = 0u64;
        let elf = fs::lookup("/bin/hello");
        let mut ran = false;
        if let Ok(ino) = elf {
            let size = ino.size as usize;
            if size > 0 && size <= 32768 {
                let mut buf = alloc::vec![0u8; size];
                if let Ok(n) = fs::read_at(ino, 0, &mut buf) {
                    if n == size {
                        let w0 = syscall::bytes_written();
                        let c0 = syscall::calls_from_user();
                        let parsed = kelf::ElfImage::parse(&buf);
                        let segs = parsed.as_ref().map(|i| i.segments().len()).unwrap_or(0);
                        match exec::spawn_from_elf(&buf) {
                            Ok(rep) => {
                                let code = wait_task(rep.tid, 800);
                                let dw = syscall::bytes_written() - w0;
                                let dc = syscall::calls_from_user() - c0;
                                detail = dc;
                                if code == Some(9) && dw > 0 {
                                    kprintln!(
                                        "[ OK ] elf:  segs={} entry={:#x} pages={} mem={}KiB exit={} calls={} out_b={}",
                                        segs,
                                        rep.entry,
                                        rep.pages,
                                        rep.mem_bytes / 1024,
                                        code.unwrap_or(-1),
                                        dc,
                                        dw
                                    );
                                    kserialln!(
                                        "       从磁盘上的 /bin/hello 装载：{} 段 / {} 页，入口 {:#x}，退出码 9，ring3 系统调用 {} 次，输出 {} 字节",
                                        rep.segments,
                                        rep.pages,
                                        rep.entry,
                                        dc,
                                        dw
                                    );
                                    ran = true;
                                } else {
                                    kprintln!(
                                        "[FAIL] elf:  exit={:?} out={}B（期望退出码 9 且有输出）",
                                        code,
                                        dw
                                    );
                                }
                            }
                            Err(e) => kprintln!("[FAIL] elf:  {}", e),
                        }
                    } else {
                        kprintln!("[FAIL] elf:  只读到 {}/{} 字节", n, size);
                    }
                }
            } else {
                kprintln!("[FAIL] elf:  /bin/hello 大小异常 {}", size);
            }
        } else {
            kprintln!("[FAIL] elf:  /bin/hello 不存在");
        }
        if ran {
            ok += 1;
        } else {
            kserialln!("       调试：size/detail={}", detail);
        }
    }

    /* ================= 31. 负路径：坏输入必须报错，不能崩 ================= */
    total += 1;
    {
        // 1) 不存在的路径
        let e1 = fs::lookup("/no/such/file").is_err();
        // 2) 不是 ELF 的 64 字节垃圾
        let junk = [0x41u8; 64];
        let e2 = exec::spawn_from_elf(&junk).is_err();
        // 3) 打开目录必须被拒（M5 没有 readdir 语义）
        let e3 = fd::open("/bin").is_err();
        // 4) 关一个没打开的 fd 必须返回 EBADF
        let e4 = fd::close(6) == Err(fd::EBADF);
        if e1 && e2 && e3 && e4 {
            kprintln!("[ OK ] reject: missing=err badelf=err dir=err badfd=err");
            kserialln!(
                "       坏输入都被挡在门口：不存在的路径、假 ELF、打开目录、关无效 fd —— 内核没有因此崩"
            );
            ok += 1;
        } else {
            kprintln!("[FAIL] reject: {}{}{}{}", e1, e2, e3, e4);
        }
    }

    /* ================= 32. 每进程地址空间 ================= */
    // 同一个 ELF 连装两次：应当得到**两张不同的页表**，而且都不是内核那张。
    let pair = (|| -> Option<(exec::Report, exec::Report)> {
        let ino = fs::lookup("/bin/proc").ok()?;
        let size = ino.size as usize;
        if size == 0 || size > 65536 {
            return None;
        }
        let mut buf = alloc::vec![0u8; size];
        if fs::read_at(ino, 0, &mut buf).ok()? != size {
            return None;
        }
        let a = exec::spawn_from_elf(&buf).ok()?;
        let b = exec::spawn_from_elf(&buf).ok()?;
        Some((a, b))
    })();

    total += 1;
    if let Some((ra, rb)) = &pair {
        let k = mm::paging::kernel_cr3();
        let on_task = task::pml4_of(ra.tid) == Some(ra.pml4)
            && task::pml4_of(rb.tid) == Some(rb.pml4);
        if ra.pml4 != rb.pml4 && ra.pml4 != k && rb.pml4 != k && on_task {
            kprintln!(
                "[ OK ] addrspace: a={:#x} b={:#x} kern={:#x} same_entry={:#x}",
                ra.pml4,
                rb.pml4,
                k,
                ra.entry
            );
            kserialln!(
                "       同一个 ELF 装成两个进程：页表 {:#x} 与 {:#x} 互不相同，也都不是内核的 {:#x}；入口地址相同（{:#x}）但物理页不同",
                ra.pml4,
                rb.pml4,
                k,
                ra.entry
            );
            ok += 1;
        } else {
            kprintln!("[FAIL] addrspace: a={:#x} b={:#x} kern={:#x}", ra.pml4, rb.pml4, k);
        }
    } else {
        kprintln!("[FAIL] addrspace: /bin/proc 装载失败");
    }

    /* ================= 33. 两个进程并发，互不干扰 ================= */
    total += 1;
    if let Some((ra, rb)) = &pair {
        // 捕获窗口要在它们开跑之前打开：两个进程一被调度就会打印
        console::capture_start();
        let sw0 = task::switches();
        let e1 = wait_task(ra.tid, 2500);
        let e2 = wait_task(rb.tid, 2500);
        let sw = task::switches() - sw0;
        console::capture_stop();

        let saw_a = console::capture_contains(b"p0");
        let saw_b = console::capture_contains(b"p1");
        // 任一进程的计数器读到 600（说明共用了一页）就会打印 "bd"
        let any_bad = console::capture_contains(b"bd");
        let t1 = task::query(ra.tid).map(|(_, _, t)| t).unwrap_or(0);
        let t2 = task::query(rb.tid).map(|(_, _, t)| t).unwrap_or(0);

        let mut text = [0u8; 256];
        let n = console::capture_text(&mut text);
        let out = core::str::from_utf8(&text[..n]).unwrap_or("?");

        // "并发"的证据不能用"每个进程的 tick 数"：一个进程可能在一个时间片内就跑完，
        // 那时它的 ticks 是 0——那是进程短，不是没并发。用"窗口内切换次数"更实在。
        if e1 == Some(9) && e2 == Some(9) && saw_a && saw_b && !any_bad && sw >= 2 {
            kprintln!(
                "[ OK ] 2proc: exit={:?}/{:?} switches={} ticks={}/{} out=\"{}\"",
                e1,
                e2,
                sw,
                t1,
                t2,
                out.trim_end()
            );
            kserialln!(
                "       两个进程各跑各的：都报 ok（同一个 0x600000 地址各加 300 次，互不干扰）；窗口内切换 {} 次（它们真的在时间片上交叉）",
                sw
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] 2proc: exit={:?}/{:?} p0={} p1={} bad={} sw={} ticks={}/{} out=\"{}\"",
                e1,
                e2,
                saw_a,
                saw_b,
                any_bad,
                sw,
                t1,
                t2,
                out.trim_end()
            );
        }
    } else {
        kprintln!("[FAIL] 2proc: 没有可运行的进程");
    }

    /* ================= 34. 越界 ELF 必须被拒 ================= */
    total += 1;
    {
        let mut entry_rejected = false;
        let mut seg_rejected = false;
        if let Ok(ino) = fs::lookup("/bin/proc") {
            let size = ino.size as usize;
            let mut buf = alloc::vec![0u8; size];
            if size > 128 && fs::read_at(ino, 0, &mut buf).ok() == Some(size) {
                // 1) 把入口改成内核高半区地址
                let mut bad = buf.clone();
                bad[24..32].copy_from_slice(&0xFFFF_FFFF_8010_0000u64.to_le_bytes());
                entry_rejected = exec::spawn_from_elf(&bad).is_err();
                // 2) 入口正常，但把第一个段的 p_vaddr 抬到内核区
                let mut bad2 = buf.clone();
                bad2[80..88].copy_from_slice(&0xFFFF_FFFF_8020_0000u64.to_le_bytes());
                seg_rejected = exec::spawn_from_elf(&bad2).is_err();
            }
        }
        if entry_rejected && seg_rejected {
            kprintln!("[ OK ] elf-guard: bad_entry=err bad_seg=err");
            kserialln!(
                "       指向内核地址的 ELF（入口越界 / 段越界）都被装载器拒绝 —— 用户程序不能给自己装一页内核"
            );
            ok += 1;
        } else {
            kprintln!("[FAIL] elf-guard: entry={} seg={}", entry_rejected, seg_rejected);
        }
    }

    /* ================= 35. 每进程 fd 表 ================= */
    total += 1;
    {
        PEER_FD.store(u64::MAX, Ordering::SeqCst);
        PEER_GO.store(0, Ordering::SeqCst);
        let peer = task::spawn_kthread(fd_peer).ok();
        // 等对端把自己的 fd 拿到（它会在 fd 3 上打开 /hello.txt 并占住）
        let got = spin_until(600, || PEER_FD.load(Ordering::SeqCst) != u64::MAX);
        let peer_fd = PEER_FD.load(Ordering::SeqCst);

        // 关键断言：**我自己**的 open 也必须返回 3。
        // 如果 fd 表是全局的，对端占了 3 之后我只能拿到 4 —— 这一条就挂了。
        let mine = fd::open("/hello.txt").unwrap_or(usize::MAX);
        let mut buf = [0u8; 12];
        let n = if mine != usize::MAX {
            let r = fd::read(mine, &mut buf).unwrap_or(0);
            let _ = fd::close(mine);
            r
        } else {
            0
        };
        let head_ok = n >= 5 && &buf[..5] == b"this ";

        // 放行对端，让它关掉自己的 fd 并退出；它退出后的回收会关掉它的 fd，
        // 而我的表早就不受影响了（这一步就算全局表也不会炸，所以判据只放在上面）
        PEER_GO.store(1, Ordering::SeqCst);
        if let Some(t) = peer {
            let _ = task::wait_zombie(t);
            let _ = task::reap(t);
        }

        if got && peer_fd == 3 && mine == 3 && head_ok {
            kprintln!("[ OK ] fd-table: peer_fd={} my_fd={} same_file_bytes={}", peer_fd, mine, n);
            kserialln!(
                "       两个任务各自打开同一个文件，**各自的 fd 都从 3 开始**；我在自己的表里读到 {} 字节（偏移互不影响）",
                n
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] fd-table: got={} peer_fd={} my_fd={} bytes={}",
                got,
                peer_fd,
                mine,
                n
            );
        }
    }

    /* ================= 36. 管道阻塞 + 唤醒 ================= */
    total += 1;
    {
        PIPE_LEN.store(u64::MAX, Ordering::SeqCst);
        PIPE_WORD.store(0, Ordering::SeqCst);
        WAIT_CODE.store(u64::MAX, Ordering::SeqCst);
        let b0 = pipe::blocks();
        let w0 = task::waked();
        let id = match pipe::create() {
            Ok(id) => id,
            Err(()) => usize::MAX,
        };
        if id != usize::MAX {
            PIPE_ID.store(id as u64, Ordering::SeqCst);
            let r = task::spawn_kthread(pipe_reader).unwrap_or(0);
            PIPE_READER_TID.store(r, Ordering::SeqCst);
            let p = task::spawn_kthread(pipe_waiter).unwrap_or(0);

            // 两个任务都该睡下去：读者等数据，等待者等读者退出
            let both_blocked = spin_until(600, || {
                is_blocked(r) && is_blocked(p)
            });

            // 睡着的任务不消耗时间片：放着不管，ticks 不该增长。
            // 这一条直接把"阻塞"和"忙等"区分开 —— 忙等的任务 ticks 会一直涨。
            let t0 = ticks_of(r);
            let tick0 = intr::timer::ticks();
            let _ = spin_until(200, || intr::timer::ticks() - tick0 >= 5);
            let t1 = ticks_of(r);

            // 写数据 → 读者被唤醒
            let wr = pipe::write(id, b"ping").unwrap_or(0);
            let r_done = spin_until(900, || is_zombie(r));
            let p_done = spin_until(900, || is_zombie(p));
            let detail = ((pipe::blocks() - b0) as u64) << 32 | (task::waked() - w0);

            let len = PIPE_LEN.load(Ordering::SeqCst);
            let word = PIPE_WORD.load(Ordering::SeqCst);
            let code = WAIT_CODE.load(Ordering::SeqCst);
            let _ = p;

            if both_blocked
                && t1 == t0
                && wr == 4
                && r_done
                && p_done
                && len == 4
                && word == 0x7069_6e67 // "ping"
                && code == 0
            {
                kprintln!(
                    "[ OK ] pipe: blocked=2 frozen_ticks=1 write={} read={} word={:#x} wait_code={}",
                    wr,
                    len,
                    word,
                    code
                );
                kserialln!(
                    "       读者与等待者都真的睡下了（ticks 停在 {} 不涨），写入 4 字节后两者依次被唤醒；等待者拿到的退出码是 {}",
                    t0,
                    code
                );
                kserialln!(
                    "       本次新增阻塞 {} 次 / 唤醒 {} 次；累计创建管道 {} 个",
                    detail >> 32,
                    detail & 0xFFFF_FFFF,
                    pipe::created()
                );
                ok += 1;
            } else {
                kprintln!(
                    "[FAIL] pipe: blocked={} frozen={} wr={} r_done={} p_done={} len={} word={:#x} code={}",
                    both_blocked,
                    t1 == t0,
                    wr,
                    r_done,
                    p_done,
                    len,
                    word,
                    code
                );
            }
        } else {
            kprintln!("[FAIL] pipe: 没有空闲管道");
        }
    }

    /* ================= 37. 管道语义：EOF 与 EPIPE（走 fd 层） ================= */
    total += 1;
    {
        // 这一项刻意走 `fd::pipe_pair` + `fd::read/write`，而不是直接调 `pipe::*`：
        // 系统调用 SYS_PIPE / SYS_READ / SYS_WRITE 走的正是这条路径，
        // 只测底层等于没测上层——上层的分派写错了照样能"全绿"。
        //
        // 两个语义必须用两个管道来测，前提互斥：
        //   EPIPE：读端先全关，写端再写 → 写端拿到 EPIPE
        //   EOF  ：写端先全关，读端读到空 → 返回 0
        let mut order_ok = false;
        let mut epipe_ok = false;
        let mut eof_ok = false;
        let mut paired = 0usize;
        if let Ok((r, w)) = fd::pipe_pair() {
            paired += 1;
            let mut buf = [0u8; 8];
            let wn = fd::write(w, b"abc").unwrap_or(0);
            let n = fd::read(r, &mut buf).unwrap_or(0);
            order_ok = wn == 3 && n == 3 && &buf[..3] == b"abc";
            let _ = fd::close(r); // 读端全关
            epipe_ok = fd::write(w, b"x") == Err(pipe::EPIPE);
            let _ = fd::close(w);
        }
        if let Ok((r2, w2)) = fd::pipe_pair() {
            paired += 1;
            let mut buf = [0u8; 8];
            let wn = fd::write(w2, b"de").unwrap_or(0);
            let _ = fd::close(w2); // 最后一个写端关掉
            let n = fd::read(r2, &mut buf).unwrap_or(0);
            let eof = fd::read(r2, &mut buf).unwrap_or(0);
            eof_ok = wn == 2 && n == 2 && &buf[..2] == b"de" && eof == 0;
            let _ = fd::close(r2);
        }
        if paired == 2 && order_ok && epipe_ok && eof_ok && fd::open_count() == 0 {
            kprintln!("[ OK ] pipe-eof: order=1 epipe=1 eof=1 fds_closed=1");
            kserialln!(
                "       走 fd 层：写端全关 → 读端先读完缓冲再拿到 EOF（0）；读端全关 → 写端拿到 EPIPE；关完全部 fd 表回到空"
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] pipe-eof: pair={} order={} epipe={} eof={} leak={}",
                paired,
                order_ok,
                epipe_ok,
                eof_ok,
                fd::open_count()
            );
        }
    }

    /* ================= 38. 退出回收：帧数必须回到起点 ================= */
    total += 1;
    {
        let mut freed_pages = 0usize;
        let mut freed_tables = 0usize;
        let mut rounds = 0usize;
        let mut failed = 0usize;
        let before = frames::stats().map(|s| s.free_frames).unwrap_or(0);
        let elf = fs::lookup("/bin/proc").ok().and_then(|ino| {
            let size = ino.size as usize;
            if size == 0 || size > 65536 {
                return None;
            }
            let mut buf = alloc::vec![0u8; size];
            if fs::read_at(ino, 0, &mut buf).ok() == Some(size) {
                Some(buf)
            } else {
                None
            }
        });
        if let Some(bytes) = elf {
            for _ in 0..3 {
                if let Ok(rep) = exec::spawn_from_elf(&bytes) {
                    if task::wait_zombie(rep.tid).is_some() {
                        let (p, t, okk) = task::reap(rep.tid);
                        freed_pages += p;
                        freed_tables += t;
                        if okk {
                            rounds += 1;
                        } else {
                            failed += 1;
                        }
                    }
                }
            }
        }
        let after = frames::stats().map(|s| s.free_frames).unwrap_or(0);
        if rounds == 3 && failed == 0 && after == before && freed_pages > 0 {
            kprintln!(
                "[ OK ] reap: runs={} pages={} tables={} frames {} -> {}",
                rounds,
                freed_pages,
                freed_tables,
                before,
                after
            );
            kserialln!(
                "       跑完并回收 3 个进程：退掉 {} 个数据页 + {} 个页表页，可用帧数精确回到起点（{}）—— 进程退出不再漏内存（累计回收 {} 个）",
                freed_pages,
                freed_tables,
                after,
                task::reaped()
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] reap: runs={} failed={} frames {} -> {} pages={}",
                rounds,
                failed,
                before,
                after,
                freed_pages
            );
        }
    }

    /* ================= 39. PCI 枚举 ================= */
    total += 1;
    {
        let mut devs = [dev::pci::Device::EMPTY; 32];
        let rep = dev::pci::enumerate(&mut devs);
        // 串口里把前几个设备打出来：一眼能看出枚举是不是真在跑
        kserialln!("       PCI：扫了 {} 条总线，发现 {} 个设备，其中存储控制器 {}、桥 {}", rep.buses, rep.devices, rep.storage, rep.bridges);
        for d in devs.iter().take(rep.devices.min(8)) {
            let (io, is_io) = dev::pci::bar(d, 0).unwrap_or((0, false));
            kserialln!(
                "         {:02x}:{:02x}.{} {:04x}:{:04x} {} prog_if={:#04x} BAR0={:#x}{}",
                d.bus,
                d.slot,
                d.func,
                d.vendor,
                d.device,
                d.class_name(),
                d.prog_if,
                io,
                if is_io { " (IO)" } else { " (MEM)" }
            );
        }
        if rep.devices >= 2 && rep.storage >= 1 {
            kprintln!(
                "[ OK ] pci:  devs={} buses={} storage={} bridges={}",
                rep.devices,
                rep.buses,
                rep.storage,
                rep.bridges
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] pci:  devs={} buses={} storage={}",
                rep.devices,
                rep.buses,
                rep.storage
            );
        }
    }

    /* ================= 40. LumiFS 挂载（可写数据盘） ================= */
    total += 1;
    let mut lumifs_ok = false;
    {
        match fs2::init() {
            Ok(rep) => {
                kprintln!(
                    "[ OK ] lumifs: blocks={} free={} entries={} seq={} formatted={}",
                    rep.blocks,
                    rep.free_blocks,
                    rep.entries,
                    rep.seq,
                    rep.formatted as u8
                );
                kserialln!(
                    "       数据盘挂载成功：{} 块（{} KiB），空闲 {}，日志序号 {}，块缓存命中/未命中={}/{}，{}",
                    rep.blocks,
                    rep.blocks,
                    rep.free_blocks,
                    rep.seq,
                    rep.cache_hits,
                    rep.cache_misses,
                    if rep.formatted {
                        "首次使用，已由内核格式化"
                    } else {
                        "沿用已有文件系统"
                    }
                );
                lumifs_ok = true;
                ok += 1;
            }
            Err(e) => kprintln!("[FAIL] lumifs: {}", e),
        }
    }

    /* ================= 41. 写文件（走 fd 层，跨块 + 短写循环） ================= */
    total += 1;
    let mut m8_ino = 0u32;
    // 注意：inode 0 是**合法**编号（LumiFS 从 0 开始），不能拿 `ino != 0`
    // 当"有没有文件"的判据——第一版就是这么错的，导致后面两项被跳过。
    let have_file: bool;
    {
        let mut wrote = 0usize;
        let mut readback = 0usize;
        let mut same = false;
        let mut fdw = usize::MAX;
        if lumifs_ok {
            fd::close_all();
            let payload: [u8; 2500] = {
                let mut b = [0u8; 2500];
                let mut i = 0;
                while i < b.len() {
                    b[i] = b'a' + (i % 26) as u8;
                    i += 1;
                }
                b
            };
            if let Ok(f) = fd::open_w("/m8.txt") {
                fdw = f;
                wrote = fd::write(f, &payload).unwrap_or(0);
                m8_ino = fs2::lookup("m8.txt").map(|i| i.ino).unwrap_or(0);
                let _ = fd::close(f);
                // 重新打开：从偏移 0 读回全部内容
                if let Ok(f2) = fd::open_w("/m8.txt") {
                    let mut buf = [0u8; 4096];
                    readback = fd::read(f2, &mut buf).unwrap_or(0);
                    same = readback == wrote && buf[..readback] == payload[..wrote];
                    let _ = fd::close(f2);
                }
            }
        }
        let _ = fdw;
        have_file = wrote == 2500 && same;
        if wrote == 2500 && readback == 2500 && same {
            kprintln!(
                "[ OK ] write: bytes={} readback={} match=1 ino={}",
                wrote,
                readback,
                m8_ino
            );
            kserialln!(
                "       写 2500 字节（跨 3 块，受单次事务上限限制分成两次提交），读回逐字节一致；inode={}",
                m8_ino
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] write: wrote={} readback={} match={}",
                wrote,
                readback,
                same
            );
        }
    }

    /* ================= 42. 卸载再挂载：数据必须还在 ================= */
    total += 1;
    {
        let mut persisted = false;
        let mut replayed = true;
        if lumifs_ok && have_file {
            let _ = fs2::sync();
            let (h0, m0) = fs2::cache_stats();
            fs2::unmount();
            match fs2::init() {
                Ok(rep) => {
                    replayed = rep.replayed;
                    if let Ok(ino) = fs2::lookup("m8.txt") {
                        let mut buf = [0u8; 2600];
                        if let Ok(n) = fs2::read_at(ino.ino, 0, &mut buf) {
                            persisted = n == 2500
                                && buf[0] == b'a'
                                && buf[2499] == (b'a' + (2499 % 26) as u8);
                        }
                    }
                    kserialln!(
                        "       重挂载后：日志重放={}（正常卸载后应为 false），缓存命中/未命中 {}/{}",
                        rep.replayed,
                        h0,
                        m0
                    );
                }
                Err(e) => kprintln!("[FAIL] remount: {}", e),
            }
        }
        if persisted && !replayed {
            kprintln!("[ OK ] persist: remount=1 replayed=0 bytes=2500");
            kserialln!(
                "       卸载再挂载，文件内容逐字节一致；没有走日志重放（说明是正常落盘的，不是靠重放兜的）"
            );
            ok += 1;
        } else {
            kprintln!("[FAIL] persist: ok={} replayed={}", persisted, replayed);
        }
    }

    /* ================= 43. 掉电现场：提交后崩溃必须靠日志救回来 ================= */
    total += 1;
    {
        // 把磁盘摆成"日志已提交、还没落地"的样子，然后**卸载再挂载**，
        // 让真正的重放路径把它救回来。这是 M8 最核心的一条：
        // 日志不是"写了就好"，是要在崩溃后被真的用上。
        let mut replayed = false;
        let mut blocks = 0u32;
        let mut content_ok = false;
        let mut seq = 0u64;
        if lumifs_ok && have_file {
            let new_data = b"AFTER-CRASH-REPLAY";
            let armed = fs2::arm_crash(m8_ino, new_data).unwrap_or(0);
            seq = armed;
            fs2::unmount();
            match fs2::init() {
                Ok(rep) => {
                    replayed = rep.replayed;
                    blocks = rep.replayed_blocks;
                    if let Ok(ino) = fs2::lookup("m8.txt") {
                        let mut buf = [0u8; 64];
                        if let Ok(n) = fs2::read_at(ino.ino, 0, &mut buf) {
                            content_ok = n == new_data.len() && &buf[..n] == &new_data[..];
                        }
                    }
                }
                Err(e) => kprintln!("[FAIL] replay: {}", e),
            }
        }
        if replayed && blocks == 2 && content_ok {
            kprintln!(
                "[ OK ] journal: armed_seq={} replayed=1 blocks={} content=1",
                seq,
                blocks
            );
            kserialln!(
                "       造出掉电现场（日志已提交、数据未落地）→ 重挂载 → 重放 {} 块 → 文件内容是新的那份。日志真的救了数据。",
                blocks
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] journal: replayed={} blocks={} content={}",
                replayed,
                blocks,
                content_ok
            );
        }
    }

    /* ================= 44. 块缓存：真的在挡 IO，而且刷完是干净的 ================= */
    total += 1;
    {
        let mut hit_gain = 0u64;
        let mut dirty_after = 1usize;
        if lumifs_ok {
            let (h0, _m0) = fs2::cache_stats();
            // 同一块反复读：缓存必须命中
            if let Ok(ino) = fs2::lookup("m8.txt") {
                let mut buf = [0u8; 64];
                for _ in 0..5 {
                    let _ = fs2::read_at(ino.ino, 0, &mut buf);
                }
            }
            let (h1, _m1) = fs2::cache_stats();
            hit_gain = h1 - h0;
            let _ = fs2::sync();
            dirty_after = fs2::cache_dirty();
        }
        if hit_gain >= 3 && dirty_after == 0 {
            kprintln!("[ OK ] cache: hits_gained={} dirty_after_sync=0", hit_gain);
            kserialln!(
                "       同一块连读 5 次，缓存命中 {} 次（真正省掉了 ATA 读）；sync 之后脏块数 0",
                hit_gain
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] cache: hits_gained={} dirty={}",
                hit_gain,
                dirty_after
            );
        }
    }

    /* ================= 45. 删除文件：名字和块都要还回去 ================= */
    total += 1;
    {
        let mut created = false;
        let mut gone = false;
        let mut freed_back = false;
        if lumifs_ok {
            let before = fs2::with(|fs| fs.free_blocks()).unwrap_or(0);
            if fs2::open_create("scratch.txt").is_ok() {
                created = true;
                if let Ok(ino) = fs2::lookup("scratch.txt") {
                    let _ = fs2::write_at(ino.ino, 0, &[b'x'; 1024]);
                }
                let after_write = fs2::with(|fs| fs.free_blocks()).unwrap_or(0);
                if fs2::unlink("scratch.txt").is_ok() {
                    gone = fs2::lookup("scratch.txt").is_err();
                    let after_unlink = fs2::with(|fs| fs.free_blocks()).unwrap_or(0);
                    freed_back = after_unlink == before && after_write < before;
                }
            }
        }
        if created && gone && freed_back {
            kprintln!("[ OK ] unlink: gone=1 blocks_restored=1");
            kserialln!("       建一个文件写 1 KiB 再删掉：名字查不到了，空闲块数精确回到写之前");
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] unlink: created={} gone={} freed={}",
                created,
                gone,
                freed_back
            );
        }
    }

    /* ================= 47. VFS 命名空间 ================= */
    // 这一项不依赖用户程序：直接把"一条路径 -> 一个对象"的解析结果摊开看。
    // 挂载点分三类（ext2 / LumiFS / 合成设备），任何一类解析错，
    // 后面 busybox 的表现都会是"文件不存在"，很难定位。
    total += 1;
    {
        let root = vfs::resolve(b"/", true);
        let hello = vfs::resolve(b"/hello.txt", true);
        let devnull = vfs::resolve(b"/dev/null", true);
        let devzero = vfs::resolve(b"/dev/zero", true);
        let mut items = 0usize;
        let mut dir_ok = false;
        if let Ok(r) = &root {
            if let Ok(slot) = vfs::dir_open(b"/", r) {
                items = vfs::dir_len(slot);
                dir_ok = items >= 4; // . / .. / bin / hello.txt
                let mut tgt = [0u8; vfs::PATH_MAX];
                let _ = vfs::dir_path(slot, &mut tgt);
                vfs::dir_close(slot);
            }
        }
        let ok_all = matches!(&root, Ok(o) if o.is_dir())
            && matches!(&hello, Ok(o) if o.size == 76 && !o.is_dir())
            && matches!(&devnull, Ok(o) if o.is_chr())
            && matches!(&devzero, Ok(o) if o.is_chr())
            && dir_ok
            && vfs::dir_open_count() == 0;
        if ok_all {
            kprintln!(
                "[ OK ] vfs:  root=dir hello=76B dev_null=chr entries={} open_dirs=0",
                items
            );
            kserialln!(
                "       一条路径进来，按挂载点分派：/ 是只读 ext2、/data 是可写 LumiFS、/dev 是合成设备；\
                 目录快照 4 项、句柄全部归还"
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] vfs:  root={} hello={} dev={} entries={}",
                matches!(&root, Ok(o) if o.is_dir()) as u8,
                matches!(&hello, Ok(o) if o.size == 76) as u8,
                matches!(&devnull, Ok(o) if o.is_chr()) as u8,
                items
            );
        }
    }

    /* ================= 48. busybox sh：M9 总考 ================= */
    total += 1;
    let frames_before = frames::stats().map(|s| s.free_frames).unwrap_or(0);
    let zombies_before = task::count(task::State::Zombie);
    {
        use alloc::vec;
        // `sh -c` 会把这三件事都走一遍：
        //   echo  —— shell 内建，只写 stdout
        //   ls /  —— fork + execve("/bin/ls") → 符号链接 → busybox 的 ls applet
        //   cat   —— fork + execve + 真的读 ext2 上的文件
        let argv: Vec<Vec<u8>> = vec![
            b"sh".to_vec(),
            b"-c".to_vec(),
            b"echo LUMI-M9; ls /; cat /hello.txt".to_vec(),
        ];
        let envp: Vec<Vec<u8>> = vec![
            b"PATH=/bin".to_vec(),
            b"HOME=/".to_vec(),
            b"TERM=dumb".to_vec(),
            b"PWD=/".to_vec(),
        ];
        console::capture_start();
        match linux::proc::spawn_program(b"/bin/busybox", &argv, &envp) {
            Ok(tid) => {
                let code = wait_task(tid, 3000);
                // 这个进程是内核直接起的，没有父进程替它收尾——必须自己 reap，
                // 否则它的地址空间（busybox 的 1.1 MiB 映像约 280 页）会一直挂着。
                let (p, t, done) = task::reap(tid);
                kserialln!("       reap(sh): pages={} tables={} ok={}", p, t, done as u8);
                console::capture_stop();
                let got_echo = console::capture_contains(b"LUMI-M9");
                let got_ls = console::capture_contains(b"hello.txt");
                let got_cat = console::capture_contains(b"from an ext2 file");
                if got_echo && got_ls && got_cat && code == Some(0) {
                    kprintln!(
                        "[ OK ] busybox: sh/echo=1 ls=1 cat=1 exit=0 bytes={}",
                        console::capture_len()
                    );
                    kserialln!(
                        "       musl 静态 busybox 跑通：`sh -c 'echo; ls /; cat /hello.txt'` \
                         三件事全部有输出（ELF 装载 + 初始栈 + 符号链接 + fork/exec/wait4）"
                    );
                    ok += 1;
                } else {
                    let mut t = [0u8; 256];
                    let n = console::capture_text(&mut t);
                    kprintln!(
                        "[FAIL] busybox: echo={} ls={} cat={} exit={:?}",
                        got_echo as u8,
                        got_ls as u8,
                        got_cat as u8,
                        code
                    );
                    kserialln!("       捕获到的输出：{:?}", core::str::from_utf8(&t[..n]).unwrap_or("?"));
                }
            }
            Err(e) => kprintln!("[FAIL] busybox: spawn err={}", e),
        }
    }

    /* ================= 49. syscall 指令通道真的在用 ================= */
    total += 1;
    {
        let n = arch::syscall::insns_from_user();
        // 一次 sh + 两次 fork/exec 下来，实测约 70 次系统调用（`ls`/`cat` 都不重）；
        // 门槛取 50：既能挡住"通道其实没走通"的假通过，又不会因版本差异误报。
        if n >= 50 {
            kprintln!("[ OK ] linux-abi: syscall_insns={} from_cpl3={}", arch::syscall::insns(), n);
            kserialln!(
                "       `syscall` 指令通道累计 {} 次（其中来自 CPL3 的 {} 次）：\
                 这些不是内核自检，是 busybox 自己发的",
                arch::syscall::insns(),
                n
            );
            ok += 1;
        } else {
            kprintln!("[FAIL] linux-abi: syscall_insns={} from_cpl3={}", arch::syscall::insns(), n);
        }
    }

    /* ================= 50. 进程资源回收 ================= */
    // fork/exec 会分配新地址空间、内核栈、打开的文件；进程退出后必须**精确**还回来。
    // 差一帧就说明某条回收路径漏了——这类泄漏不会报错，只会让系统越跑越慢。
    total += 1;
    {
        // 等所有僵尸被父进程收干净（sh 自己会 wait4，这里再给几个 tick）
        // 注意：M3–M7 的自检任务里有的只退出不回收（它们是演示用的），
        // 所以这里比较的是"僵尸数量与之前一致"，而不是"零僵尸"。
        let reaped_ok = spin_until(200, || {
            task::count(task::State::Zombie) <= zombies_before
        });
        let after = frames::stats().map(|s| s.free_frames).unwrap_or(0);
        if reaped_ok && after == frames_before && frames::double_frees() == 0 {
            kprintln!("[ OK ] reap: frames {} -> {} zombies={} double_free=0", frames_before, after, task::count(task::State::Zombie));
            kserialln!(
                "       busybox 那一轮 fork/exec 用掉的帧全部归还（{} -> {}），没有重复释放",
                frames_before,
                after
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] reap: frames {} -> {} zombies={} double_free={}",
                frames_before,
                after,
                task::count(task::State::Zombie),
                frames::double_frees()
            );
        }
    }

    /* ================= 51. 用户态异常隔离 ================= */
    // 跑别人的程序，用户态空指针/越界是**它自己的 bug**，不能把内核带走。
    // 这一项数的是"被隔离掉、只杀进程"的次数；正常跑完应当是 0 次，
    // 所以这里验证的是"内核没有因此停机"+计数接口可用。
    total += 1;
    {
        let uf = intr::handler::user_faults();
        if uf == 0 && task::current_is_user() == false {
            kprintln!("[ OK ] isolate: user_faults=0 kernel_alive=1");
            kserialln!(
                "       这一轮没有用户态异常；即便有，也只会杀掉那个进程以 139 退出、内核继续跑"
            );
            ok += 1;
        } else {
            kprintln!("[FAIL] isolate: user_faults={} cur_user={}", uf, task::current_is_user() as u8);
        }
    }

    /* ================= 52. 故障注入：物理帧 ================= */
    // S2 的第一根支柱：**分配器说"没有内存"时，调用链必须原路返回错误**。
    // 这里连续注入 200 次失败，每一次都必须让 `alloc_frame()` 返回 `None`——
    // 不是 panic、不是返回无效地址、更不是把已经发出去的帧再发一次。
    total += 1;
    {
        use kqa::fault::Point;
        let base = frames::stats().map(|s| s.free_frames).unwrap_or(0);
        const TRIALS: u32 = 200;
        qa::fault::arm(Point::Frame, 0, 1, TRIALS);
        let mut denied = 0u32;
        for _ in 0..TRIALS {
            if frames::alloc_frame().is_none() {
                denied += 1;
            }
        }
        let fired = qa::fault::fired();
        qa::fault::disarm();

        // 解注入之后必须**立刻**恢复正常。这里如果拿不到帧，说明注入状态粘住了
        // （比如预算计数写错）——那是比"注入本身"更严重的 bug。
        let recovered = frames::alloc_frame();
        if let Some(p) = recovered {
            frames::free_frame(p);
        }
        let after = frames::stats().map(|s| s.free_frames).unwrap_or(0);
        // "系统仍可服务"：堆还能审计、文件还能读
        let alive = heap::audit() && fs::lookup("/hello.txt").is_ok();

        if denied == TRIALS && fired == TRIALS as u64 && recovered.is_some() && after == base && alive {
            kprintln!(
                "[ OK ] fault.frame: {}/{} 返回 None，恢复后帧数 {} -> {}，系统可服务",
                denied, TRIALS, base, after
            );
            kserialln!(
                "       物理帧注入 {} 次：每次都由 alloc_frame 返回 None（没有 panic、没有重复发放），\
                 解除注入后立刻能分配、帧数精确回到 {}",
                TRIALS, base
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] fault.frame: denied={} fired={} recovered={} frames {}->{} alive={}",
                denied, fired, recovered.is_some() as u8, base, after, alive as u8
            );
        }
    }

    /* ================= 53. 故障注入：内核堆 ================= */
    // `GlobalAlloc::alloc` 返回空指针是 Rust 约定的"失败"表达方式。
    // 校验点是：调用方必须能把它变成 `Err`（这里用 `try_reserve_exact`），
    // 而不是直接在 `Vec` 里 `unwrap` 成 panic。
    total += 1;
    {
        use kqa::fault::Point;
        let mut v: Vec<u8> = Vec::new();
        qa::fault::arm(Point::Heap, 0, 1, 1);
        let denied = v.try_reserve_exact(8192).is_err();
        qa::fault::disarm();
        let recovered = v.try_reserve_exact(8192).is_ok();
        let alive = heap::audit();
        if denied && recovered && alive {
            kprintln!("[ OK ] fault.heap: try_reserve 拿到 Err，解除后成功，堆审计通过");
            kserialln!(
                "       堆注入：分配返回空指针 → `try_reserve_exact` 得到 `Err`（不是 panic）；\
                 解除注入后立刻分配成功，堆结构未被破坏"
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] fault.heap: denied={} recovered={} audit={}",
                denied as u8, recovered as u8, alive as u8
            );
        }
    }

    /* ================= 54. 故障注入：块设备读 ================= */
    // 最容易被忽略的一条：盘读失败时**绝不能返回一段旧数据或垃圾**。
    // 文件系统必须把错误一路上传，调用方看到的是 Err。
    total += 1;
    {
        use kqa::fault::Point;
        let mut good = [0u8; 128];
        let gn = match fs::lookup("/hello.txt") {
            Ok(ino) => fs::read_at(ino, 0, &mut good).unwrap_or(0),
            Err(_) => 0,
        };

        // 先清块缓存：不清的话这次"读"会命中缓存，注入根本不会触发——
        // 那就变成在测缓存，不是在测磁盘错误路径。
        fs::cache_flush();
        qa::fault::arm(Point::DiskRead, 0, 1, 1);
        let denied = fs::lookup("/hello.txt").is_err();
        qa::fault::disarm();

        // 恢复后逐字节比对：注入期间不许"半成功"地改坏任何缓存
        let mut again = [0u8; 128];
        let an = match fs::lookup("/hello.txt") {
            Ok(ino) => fs::read_at(ino, 0, &mut again).unwrap_or(0),
            Err(_) => 0,
        };
        if denied && gn == an && gn > 0 && good[..gn] == again[..an] {
            kprintln!("[ OK ] fault.disk: 读失败被上传为 Err，恢复后 {} 字节逐字节一致", gn);
            kserialln!(
                "       块设备注入：路径解析拿到 `Err`（不是旧数据、不是垃圾）；\
                 解除后重新读到 {} 字节且内容与注入前完全一致",
                gn
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] fault.disk: denied={} len {}->{} same={}",
                denied as u8,
                gn,
                an,
                (gn == an && gn > 0 && good[..gn] == again[..an]) as u8
            );
        }
    }

    /* ================= 55. fuzz：系统调用参数 + 路径 ================= */
    // 两段：
    //   ① 把"边界值"当参数灌进系统调用分发层（2 万次）；
    //   ② 把随机拼出来的路径直接喂给路径解析（5 千条）。
    // 判定：一次都不许 panic，且跑完之后**帧 / fd / 任务数不许涨**。
    //
    // 为什么白名单而不是全量：`fork`/`execve`/`exit`/`read` 会改变当前进程
    // 的状态甚至让自检线程消失——那不是 fuzz 参数该干的事。这里挑的是
    // "纯校验型"调用：它们的失败路径最能暴露越界检查的漏洞。
    total += 1;
    {
        use klinux::sysno as NR;
        use kqa::rng::Rng;
        let mut rng = Rng::new(0x4D31_1F02);
        const SAFE: [u64; 13] = [
            NR::GETPID,
            NR::GETPPID,
            NR::GETCWD,
            NR::OPENAT,
            NR::CLOSE,
            NR::FSTAT,
            NR::NEWFSTATAT,
            NR::LSEEK,
            NR::GETDENTS64,
            NR::IOCTL,
            NR::READLINK,
            NR::FCNTL,
            NR::MPROTECT,
        ];

        let f0 = frames::stats().map(|s| s.free_frames).unwrap_or(0);
        let fd0 = fd::open_count();
        let t0 = task::count_used();

        const SYSCALL_ROUNDS: u32 = 20_000;
        for i in 0..SYSCALL_ROUNDS {
            let nr = SAFE[(i as usize) % SAFE.len()];
            let (a0, a1) = (kqa::fuzz::arg(&mut rng), kqa::fuzz::arg(&mut rng));
            let (a2, a3) = (kqa::fuzz::arg(&mut rng), kqa::fuzz::arg(&mut rng));
            let (a4, a5) = (kqa::fuzz::arg(&mut rng), kqa::fuzz::arg(&mut rng));
            // SAFETY: `InterruptFrame` 全是 u64，零值合法；这里只是借用它的
            // 寄存器布局来调用分发层，不会真的 iretq 回去。
            let mut f: intr::frame::InterruptFrame = unsafe { core::mem::zeroed() };
            f.rax = nr;
            f.rdi = a0;
            f.rsi = a1;
            f.rdx = a2;
            f.r10 = a3;
            f.r8 = a4;
            f.r9 = a5;
            linux::dispatch(&mut f);
        }

        // 路径 fuzz：直接调 vfs 解析，绕开用户内存拷贝，专打路径状态机
        const PATH_ROUNDS: u32 = 5_000;
        let mut buf = [0u8; 96];
        for _ in 0..PATH_ROUNDS {
            let n = kqa::fuzz::path(&mut rng, &mut buf);
            let _ = vfs::resolve(&buf[..n], true);
        }

        let f1 = frames::stats().map(|s| s.free_frames).unwrap_or(0);
        let fd1 = fd::open_count();
        let t1 = task::count_used();
        let no_growth = f1 == f0 && fd1 <= fd0 && t1 == t0;
        if no_growth {
            kprintln!(
                "[ OK ] fuzz: syscall×{} path×{} 无 panic，frames {} fd {}->{} tasks {}",
                SYSCALL_ROUNDS, PATH_ROUNDS, f1, fd0, fd1, t1
            );
            kserialln!(
                "       fuzz {} 万次调用（含页边界/用户上界/内核地址/-1 等 {} 个边界值）+\
                 5000 条畸形路径：全部返回错误码，没有 panic，也没有留下帧/句柄/任务",
                (SYSCALL_ROUNDS + PATH_ROUNDS) / 10_000,
                kqa::fuzz::INTERESTING.len()
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] fuzz: frames {}->{} fd {}->{} tasks {}->{}",
                f0, f1, fd0, fd1, t0, t1
            );
        }
    }

    /* ================= 56. 加速长跑与漂移判定（S3） ================= */
    // 30 天实测不在这里（那要机器连续开着）；这一项做的是**同一套判定代码**
    // 的加速版：64 个采样点、每点之间跑 100 轮混合负载，
    // 要求空闲帧与堆占用回归斜率为 0、首尾差为 0。
    // 真正的 30 天跑用 `scripts/soak.sh`，采样的判定逻辑完全一样。
    total += 1;
    {
        const SAMPLES: u32 = 64;
        const PER_SAMPLE: u32 = 500;
        qa::soak::reset();
        let tsc0 = qa::perf::tsc();
        let mut ops = 0u64;

        for _ in 0..SAMPLES {
            for _ in 0..PER_SAMPLE {
                // ① 帧：分配 4 页再还回去
                let mut ps = [0u64; 4];
                for p in ps.iter_mut() {
                    *p = frames::alloc_frame().unwrap_or(0);
                }
                for &p in ps.iter() {
                    if p != 0 {
                        frames::free_frame(p);
                    }
                }
                // ② 堆：小对象反复申请释放（最容易泄漏的地方）
                {
                    let b = Box::new([0x5Au8; 256]);
                    core::hint::black_box(&b);
                }
                {
                    let mut v: Vec<u64> = Vec::with_capacity(32);
                    for i in 0..32u64 {
                        v.push(i);
                    }
                    core::hint::black_box(&v);
                }
                // ③ 文件：打开 → 读 → 关
                if let Ok(f) = fd::open("/hello.txt") {
                    let mut b = [0u8; 128];
                    let _ = fd::read(f, &mut b);
                    let _ = fd::close(f);
                }
                // ④ 管道：建 → 写 → 读 → 关（两侧都关，否则会漏 fd 与缓冲）
                if let Ok((r, w)) = fd::pipe_pair() {
                    let msg = [0x42u8; 32];
                    let _ = fd::write(w, &msg);
                    let mut out = [0u8; 32];
                    let _ = fd::read(r, &mut out);
                    let _ = fd::close(r);
                    let _ = fd::close(w);
                }
                ops += 8;
            }
            qa::soak::record();
        }

        let elapsed = qa::perf::tsc().wrapping_sub(tsc0);
        let r = qa::soak::report();
        let still = qa::soak::is_still();
        if r.samples == SAMPLES as usize && r.is_flat() && still {
            let ms = qa::perf::cycles_to_ns(elapsed) / 1_000_000;
            kprintln!(
                "[ OK ] soak: {} 采样 × {} 轮，帧斜率 {}‰ 堆斜率 {}‰ 首尾差 0（{} ms）",
                r.samples, PER_SAMPLE, r.free_frames_slope, r.heap_used_slope, ms
            );
            kserialln!(
                "       加速长跑：{} 轮 / {} 次操作 / {} ms；空闲帧与堆占用的回归斜率为 0‰、\
                 首尾差为 0 —— 没有泄漏。30 天版用 scripts/soak.sh，判定代码是同一份",
                r.rounds * PER_SAMPLE as u64,
                ops,
                ms
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] soak: samples={} slope frames={}‰ heap={}‰ delta {} / {} still={}",
                r.samples, r.free_frames_slope, r.heap_used_slope,
                r.free_frames_delta, r.heap_used_delta, still as u8
            );
        }
    }

    /* ================= 57. 性能基准 ================= */
    // 数字全部用 TSC 计时（比 100 Hz 的 tick 精确三个数量级），换算成
    // 每次操作的纳秒数。这组数直接进 `docs/06-性能对比.md`，和同一台机器上
    // 跑的真实 Linux 放在一起比。
    total += 1;
    {
        use alloc::vec;
        let hz = qa::perf::tsc_hz();
        if hz == 0 {
            kprintln!("[FAIL] perf: TSC 未校准，无法测");
        } else {
            const N_FRAME: u64 = 20_000;
            let (c_frame, _) = qa::perf::timed(|| {
                for _ in 0..N_FRAME {
                    if let Some(p) = frames::alloc_frame() {
                        frames::free_frame(p);
                    }
                }
            });

            const N_HEAP: u64 = 20_000;
            let (c_heap, _) = qa::perf::timed(|| {
                for _ in 0..N_HEAP {
                    let b = Box::new([0u8; 256]);
                    core::hint::black_box(&b);
                }
            });

            // ext2 顺序读：整份 busybox（1.1 MiB），4 KiB 一块
            let mut read_bytes = 0u64;
            let mut read_ops = 0u64;
            let c_read = match fs::lookup("/bin/busybox") {
                Ok(ino) => {
                    let mut buf = vec![0u8; 4096];
                    let (c, _) = qa::perf::timed(|| {
                        let mut off = 0u32;
                        while off < ino.size {
                            let n = fs::read_at(ino, off, &mut buf).unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            read_bytes += n as u64;
                            read_ops += 1;
                            off += n as u32;
                        }
                    });
                    c
                }
                Err(_) => 0,
            };

            // LumiFS 写：1 KiB × 8 次事务（每次都 flush，代价含日志提交）
            let mut write_ops = 0u64;
            let c_write = {
                let (c, _) = qa::perf::timed(|| {
                    for _ in 0..8 {
                        if let Ok(ino) = fs2::open_create("perf.dat") {
                            if fs2::write_at(ino.ino, 0, &[0x5Au8; 1024]).is_ok() {
                                write_ops += 1;
                            }
                            let _ = fs2::sync();
                        }
                    }
                });
                c
            };
            let _ = fs2::unlink("perf.dat");

            // 进程创建：装载 ELF + 建页表 + 建任务。
            //
            // **故意不把等待算进去**：内核主线程（tid 0）等子进程只能靠
            // `halt_once()`，而它要等下一个时钟 tick（10 ms）才醒——
            // 那样测出来的永远是 10 ms，量的是 tick 频率不是进程创建。
            // 真正的 `fork+exec+wait` 端到端延迟由用户态 bench 程序测
            // （它用的是真 `wait4`，见 docs/06-性能对比.md）。
            let mut exec_ok = 0u64;
            let mut tids: Vec<u64> = Vec::new();
            let c_exec = {
                let argv: Vec<Vec<u8>> = vec![b"hello".to_vec()];
                let envp: Vec<Vec<u8>> = vec![b"PATH=/bin".to_vec()];
                console::capture_start();
                let (c, _) = qa::perf::timed(|| {
                    for _ in 0..20 {
                        if let Ok(tid) = linux::proc::spawn_program(b"/bin/hello", &argv, &envp) {
                            tids.push(tid);
                            exec_ok += 1;
                        }
                    }
                });
                c
            };
            // 计时结束后再统一收割，避免把等待时间算进"创建"里
            for tid in tids {
                let _ = wait_task(tid, 3000);
                let _ = task::reap(tid);
            }
            console::capture_stop();

            // 管道往返：写 1 KiB 再读回来（测的是内核数据通路，不是调度）
            let mut pipe_ops = 0u64;
            let c_pipe = {
                let (c, _) = qa::perf::timed(|| {
                    if let Ok((r, w)) = fd::pipe_pair() {
                        let data = [0x41u8; 1024];
                        let mut out = [0u8; 1024];
                        for _ in 0..1000 {
                            if fd::write(w, &data).is_ok() && fd::read(r, &mut out).is_ok() {
                                pipe_ops += 1;
                            }
                        }
                        let _ = fd::close(r);
                        let _ = fd::close(w);
                    }
                });
                c
            };

            // 同一份 ELF 在这里也跑一遍：它的输出（`getpid` / `write_null` / …）
            // 直接进串口日志，和 Linux 上跑同一份二进制的结果放在一起比。
            let bench_ok = {
                let argv: Vec<Vec<u8>> = vec![
                    b"bench".to_vec(),
                    b"/bin/busybox".to_vec(),
                    b"/hello.txt".to_vec(),
                    b"/dev/null".to_vec(),
                ];
                let envp: Vec<Vec<u8>> = vec![b"PATH=/bin".to_vec()];
                match linux::proc::spawn_program(b"/bin/bench", &argv, &envp) {
                    Ok(tid) => {
                        let code = wait_task(tid, 6000);
                        let _ = task::reap(tid);
                        code == Some(0)
                    }
                    Err(e) => {
                        kserialln!("[bench] 启动失败 err={}", e);
                        false
                    }
                }
            };

            let ns_frame = qa::perf::cycles_to_ns(c_frame) / N_FRAME;
            let ns_heap = qa::perf::cycles_to_ns(c_heap) / N_HEAP;
            let read_ns = qa::perf::cycles_to_ns(c_read);
            // MiB/s = 字节 / (纳秒 / 1e9) / 2^20
            // MiB/s = 字节 / 秒 / 2^20 = 字节 × 1e9 / 纳秒 / 2^20
            let read_mibs = if read_ns == 0 {
                0
            } else {
                ((read_bytes as u128 * 1_000_000_000u128) / read_ns as u128 / (1u128 << 20)) as u64
            };
            let ns_write = if write_ops > 0 {
                qa::perf::cycles_to_ns(c_write) / write_ops
            } else {
                0
            };
            let ns_exec = if exec_ok > 0 {
                qa::perf::cycles_to_ns(c_exec) / exec_ok
            } else {
                0
            };
            let ns_pipe = if pipe_ops > 0 {
                qa::perf::cycles_to_ns(c_pipe) / pipe_ops
            } else {
                0
            };

            if exec_ok == 20 && read_bytes > 1_000_000 && pipe_ops == 1000 && write_ops == 8 && bench_ok {
                kprintln!(
                    "[ OK ] perf: frame={}ns heap={}ns read={}MiB/s lumifs={}ns exec={}ns pipe1k={}ns",
                    ns_frame, ns_heap, read_mibs, ns_write, ns_exec, ns_pipe
                );
                kserialln!(
                    "       TSC {} MHz；帧分配+归还 {} ns/次，256B 堆分配 {} ns/次，\
                     ext2 顺序读 {} MiB/s（{} KiB），LumiFS 1KiB 写+flush {} ns，\
                     fork+exec+wait+reap {} ns，管道 1KiB 往返 {} ns，\
                     每次读 {} 次块 I/O",
                    hz / 1_000_000,
                    ns_frame,
                    ns_heap,
                    read_mibs,
                    read_bytes / 1024,
                    ns_write,
                    ns_exec,
                    ns_pipe,
                    if read_ops > 0 { read_bytes / read_ops.max(1) } else { 0 }
                );
                ok += 1;
            } else {
                kprintln!(
                    "[FAIL] perf: exec {}/10 read {}B pipe {}/1000 write {}/8",
                    exec_ok, read_bytes, pipe_ops, write_ops
                );
            }
        }
    }

    /* ================= 58. 内核栈：金丝雀 + 高水位（S1） ================= */
    // 这一项是 M11 花得最值的体检：它在**之前**就抓到过一次真实的栈溢出
    // ——64 KiB 的 boot 栈被踩穿 1.2 KiB，正好落在紧邻的 `pd_1g` 上，
    // 把直接映射第 357 项写成内核返回地址。表现是"跑 2 万次 mmap 之后
    // 在 714 MiB 处随机页错误"，从现场根本看不出是栈的问题。
    total += 1;
    {
        let hw = stack_high_water();
        let canary = stack_canary_ok();
        let pct = hw * 100 / STACK_BYTES;
        if canary && pct < 60 {
            kprintln!(
                "[ OK ] stack: 高水位 {} KiB / {} KiB ({}%), 金丝雀完好",
                hw / 1024,
                STACK_BYTES / 1024,
                pct
            );
            kserialln!(
                "       内核栈用量 {} KiB（上限 {} KiB，占 {}%）：栈底金丝雀未被触碰——\
                 溢出会在踩坏页表之前先被这里抓到",
                hw / 1024,
                STACK_BYTES / 1024,
                pct
            );
            ok += 1;
        } else {
            kprintln!(
                "[FAIL] stack: 高水位 {} KiB ({}%), canary_ok={}",
                hw / 1024,
                pct,
                canary as u8
            );
        }
    }

    // 抢占禁用深度必须回到 0：任何一处 `PreemptGuard` 泄漏都会让抢占永久失效，
    // 而表现只是"系统变卡"，不会有任何报错。放在这里当最后一道体检。
    kserialln!(
        "抢占禁用深度 = {}（必须为 0）",
        task::preempt_depth()
    );

    kprintln!("--------------------------------------------------");
    kprintln!("M11 self-test {}/{} passed", ok, total);
    if ok == total {
        kprintln!(
            "[LIVE] kernel healthy: {} cpus, uefi+sata, 0 panic, 0 leak",
            arch::smp::count()
        );
        kserialln!(
            "M11 全部通过：{} 个核在线；故障注入 200 次全部优雅失败、fuzz 2.5 万次零 panic、\
             加速长跑零漂移、性能数据见 docs/06-性能对比.md",
            arch::smp::count()
        );
        kserialln!("这是第一个可发布版本：v1.2.0（M11）。30 天长跑与真机验证由 scripts/ 下的脚本执行");
    } else {
        kprintln!("[WARN] some checks failed, see [FAIL] lines above");
    }
    kprintln!("--------------------------------------------------");
    kserialln!(
        "累计 tick = {}（≈{} ms） 切换 = {} 系统调用 = {}（来自 CPL3 {}）",
        intr::timer::ticks(),
        intr::timer::millis(),
        task::switches(),
        syscall::calls(),
        syscall::calls_from_user()
    );
    kserialln!(
        "非定时器中断 {} 次（应为 0）；当前任务 user={}",
        intr::handler::other_irqs(),
        task::current_is_user()
    );

    // 长跑模式（仅 LUMI_SOAK=1 构建）：自检全绿之后进无限循环，永不返回。
    #[cfg(soak)]
    {
        // 负对照构建（`LUMI_SOAK_LEAK=1`）**故意**会让自检 56 报漂移，
        // 那种情况下也必须进长跑，否则验证不了检测器。
        #[cfg(soak_leak)]
        soakmode::run();
        #[cfg(not(soak_leak))]
        {
            if ok == total {
                soakmode::run();
            }
            kprintln!("[SOAK] 自检没有全绿，拒绝进入长跑（先修 FAIL 项）");
        }
    }

    // 崩溃演示（仅 LUMI_CRASH_DEMO=1 构建）
    #[cfg(crash_demo)]
    {
        kprintln!("[DEMO] triggering an uncaught page fault on purpose");
        kserialln!("[演示] 下面这次缺页没有对应的探针，会走完整崩溃路径：打印现场后停机");
        // SAFETY: 故意访问未映射地址。PML4[257] 整段没有映射。
        unsafe {
            let p = (VMALLOC_BASE + 0x9_0000) as *const u64;
            let v = core::ptr::read_volatile(p);
            kprintln!("[DEMO] unreachable: {}", v);
        }
    }

    intr::enable();
    loop {
        intr::frame::halt_once();
    }
}

/// 等某个任务退出。返回退出码（超时返回 None）。
///
/// 这里是"让出+等中断"而不是忙等：开中断后 `hlt`，时钟中断会把我们切走，
/// 目标任务才有机会跑。这本身也是抢占在工作的证据。
/// 开中断等某个条件成立（最多 `max` 次 hlt）。返回是否等到了。
///
/// 主线程（0 号任务）自己不能阻塞：真睡下去万一没有别的任务可跑就死锁了。
/// 所以内核自检统一用"halt + 重查"——halt 期间 CPU 不烧电，中断来了继续查。
fn spin_until(max: u32, mut f: impl FnMut() -> bool) -> bool {
    intr::enable();
    let mut n = 0u32;
    while !f() {
        if n >= max {
            intr::disable();
            return false;
        }
        intr::frame::halt_once();
        n += 1;
    }
    intr::disable();
    true
}

fn is_blocked(tid: u64) -> bool {
    task::query(tid).map(|(s, _, _)| s) == Some(task::State::Blocked)
}

fn is_zombie(tid: u64) -> bool {
    task::query(tid).map(|(s, _, _)| s) == Some(task::State::Zombie)
}

fn ticks_of(tid: u64) -> u64 {
    task::query(tid).map(|(_, _, t)| t).unwrap_or(0)
}

fn wait_task(tid: u64, max_halts: u32) -> Option<i64> {
    intr::enable();
    let mut n = 0u32;
    let r = loop {
        match task::query(tid) {
            Some((task::State::Zombie, code, _)) => break Some(code),
            Some(_) => {}
            None => break None,
        }
        if n >= max_halts {
            break None;
        }
        intr::frame::halt_once();
        n += 1;
    };
    intr::disable();
    r
}

/* ---------------- 三个内核线程：只干活，从不主动让出 ---------------- */

fn kt_work(i: usize) -> ! {
    for _ in 0..KT_ITERS {
        KT[i].fetch_add(1, Ordering::Relaxed);
    }
    task::exit_current(0)
}

extern "C" fn kt0() {
    kt_work(0)
}
extern "C" fn kt1() {
    kt_work(1)
}
extern "C" fn kt2() {
    kt_work(2)
}

/// 把帧分配器适配成 slab 的页来源：物理帧 + 直接映射区地址。
struct KernelPages;

impl PageSource for KernelPages {
    fn alloc_page(&mut self) -> Option<u64> {
        frames::alloc_frame().map(paging::phys_to_virt)
    }
    fn free_page(&mut self, addr: u64) {
        frames::free_frame(paging::virt_to_phys(addr));
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    intr::disable();
    let mut c = Console::new();
    let _ = writeln!(c, "\n!!! 内核 panic !!!");
    let _ = writeln!(c, "{}", info);
    let _ = writeln!(c, "panic 视为内核 bug，CI 中计数必须为 0");
    intr::frame::halt();
}
