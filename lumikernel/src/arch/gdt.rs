//! GDT + TSS（M3）
//!
//! 为什么 M3 必须自己建 GDT：M0–M2 沿用 stage2 的 4 项 GDT，只够内核自己跑。
//! 要进 ring3 至少还需要：
//! - 用户代码段 / 用户数据段（DPL=3）；
//! - **TSS**：从 ring3 陷入内核时，CPU 靠 `TSS.rsp0` 找到内核栈。
//!   没有 TSS（或 `TR` 没装载），第一次 `int 0x80` 就是三重故障——因为 CPU
//!   不知道该把栈切到哪，而它不能信任用户给的 rsp。
//!
//! 还有一件事：**#DF 必须有自己的栈（IST1）**。双重故障最常见的成因就是
//! "栈溢出导致压栈失败再触发异常"，此时原来的栈已经不可用，用同一张栈处理
//! 只会再失败一次 → 三重故障重启，现场全丢。IST 让 #DF 无条件换到一张已知可用的栈。
//!
//! 选择子布局（前三项与 stage2 保持一致，避免正在用的 CS/DS 失效）：
//!
//! ```text
//! 0x00 null
//! 0x08 64 位代码（保留，兼容）
//! 0x10 内核数据        ← 当前 DS/ES/SS
//! 0x18 64 位内核代码    ← 当前 CS（stage2 交过来就是它）
//! 0x20 内核数据（长模式，stage2 里的 0x20）
//! 0x28 用户数据  DPL=3 → 0x2B
//! 0x30 用户代码  DPL=3 → 0x33
//! 0x38 TSS（占两项，16 字节系统描述符）
//! ```

use core::arch::asm;
use core::cell::UnsafeCell;

pub const KERNEL_CS: u16 = 0x18;
pub const KERNEL_DS: u16 = 0x10;
pub const USER_DS: u16 = 0x28 | 3; // 0x2B
pub const USER_CS: u16 = 0x30 | 3; // 0x33
pub const TSS_SEL: u16 = 0x38;

/// TSS 的固定大小（64 位）。
pub const TSS_BYTES: usize = 104;

/// 64 位 TSS。**必须 packed**：字段 `rsp0` 落在偏移 4，是 4 字节对齐而非 8。
#[repr(C, packed)]
pub struct Tss {
    reserved0: u32,
    pub rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    reserved1: u64,
    /// IST1..IST7 的栈顶。
    pub ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    /// 没有 I/O 位图时等于 TSS 大小。
    pub iomap_base: u16,
}

impl Tss {
    pub const fn zero() -> Self {
        Tss {
            reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            iomap_base: TSS_BYTES as u16,
        }
    }
}

struct TssCell(UnsafeCell<Tss>);
// SAFETY: 启动阶段单核写入；运行期只有 CPU 自己读（切栈时读 rsp0/ist）。
unsafe impl Sync for TssCell {}
static TSS: TssCell = TssCell(UnsafeCell::new(Tss::zero()));

struct GdtCell(UnsafeCell<[u64; 9]>);
// SAFETY: 建好后只读（CPU 读，我们只在初始化时写）。
unsafe impl Sync for GdtCell {}
static GDT: GdtCell = GdtCell(UnsafeCell::new([0u64; 9]));

extern "C" {
    fn gdt_load(gdtr: *const u8);
    fn tss_load(sel: u16);
    pub static ist1_stack_top: u8;
    pub static kstack0_top: u8;
}

/// 64 位代码段描述符（基址 0，限 4 GiB，L=1）。
const fn code_desc(dpl: u8) -> u64 {
    let access = 0x9Au64 | ((dpl as u64) << 5);
    0xFFFF | (access << 40) | (0xF << 48) | (0xA << 52)
}

/// 数据段描述符（基址 0，限 4 GiB）。
const fn data_desc(dpl: u8) -> u64 {
    let access = 0x92u64 | ((dpl as u64) << 5);
    0xFFFF | (access << 40) | (0xF << 48) | (0xC << 52)
}

/// TSS 描述符的低 8 字节（类型 0x9 = 64 位可用 TSS，S=0，DPL=0，P=1）。
fn tss_desc(base: u64, limit: u32) -> u64 {
    let limit = limit as u64;
    (limit & 0xFFFF)
        | ((base & 0xFF_FFFF) << 16)
        | (0x89u64 << 40)
        | (((limit >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56)
}

/// `lgdt` / `sgdt` 的操作数（10 字节，必须 packed）。
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Gdtr {
    limit: u16,
    base: u64,
}

pub struct Report {
    pub gdt_base: u64,
    pub gdt_limit: u16,
    pub tr: u16,
    pub rsp0: u64,
    pub ist1: u64,
}

/// 建 GDT + TSS 并装载。
///
/// 必须在 `intr::init` **之前**调用：IDT 的 #DF 门要引用 IST1，
/// 而 IST 栈顶来自本函数。
pub fn init() -> Result<Report, &'static str> {
    // SAFETY: 启动阶段单核、中断关闭，以下全部是对本模块静态数据的独占写入。
    let (base, limit, rsp0, ist1) = unsafe {
        let tss = &mut *TSS.0.get();
        // SAFETY: extern 静态符号由 arch.S 定义，取地址不读写内存。
        let ist1_top = core::ptr::addr_of!(ist1_stack_top) as u64;
        let k0_top = core::ptr::addr_of!(kstack0_top) as u64;
        tss.rsp0 = k0_top;
        tss.ist[0] = ist1_top;
        tss.iomap_base = TSS_BYTES as u16;

        let gdt = &mut *GDT.0.get();
        gdt[0] = 0; // null
        gdt[1] = code_desc(0); // 0x08
        gdt[2] = data_desc(0); // 0x10
        gdt[3] = code_desc(0); // 0x18 ← 当前 CS
        gdt[4] = data_desc(0); // 0x20
        gdt[5] = data_desc(3); // 0x28 用户数据
        gdt[6] = code_desc(3); // 0x30 用户代码
        let tb = tss as *const Tss as u64;
        gdt[7] = tss_desc(tb, (TSS_BYTES - 1) as u32);
        gdt[8] = tb >> 32; // TSS 描述符高 8 字节
        (
            gdt.as_ptr() as u64,
            9 * 8 - 1,
            tss.rsp0,
            tss.ist[0],
        )
    };

    if limit as usize != 71 {
        return Err("GDT 界限异常");
    }

    let gdtr = Gdtr {
        limit: limit as u16,
        base,
    };
    // SAFETY: gdtr 指向本栈帧上的合法 10 字节操作数；GDT 已填好且是静态内存，
    // 在 lgdt 之后长期有效。远返回目标标签就在本函数内。
    unsafe {
        gdt_load(core::ptr::addr_of!(gdtr) as *const u8);   // 探针：GDT 已装载、远返回成功
        tss_load(TSS_SEL);   // 探针：TR 已装载
    }

    let (gb, gl) = read_gdtr();
    if gb != base || gl != limit as u16 {
        return Err("GDTR 回读不一致（lgdt 没生效）");
    }
    let tr = read_tr();
    if tr != TSS_SEL {
        return Err("TR 里的选择子不是 TSS");
    }

    Ok(Report {
        gdt_base: gb,
        gdt_limit: gl,
        tr,
        rsp0,
        ist1,
    })
}

/// 重新装载 GDT（每个 AP 都要做一次：GDTR 是每 CPU 的寄存器，不共享）。
///
/// 复用 `gdt_load`：它做 lgdt + 重载数据段 + 远返回重载 CS。
/// 对 AP 来说是安全的——CS 在临时跳板 GDT 和正式 GDT 里都正好是 0x18（64 位代码段）。
pub fn reload() {
    let base = unsafe { (*GDT.0.get()).as_ptr() as u64 };
    let gdtr = Gdtr {
        limit: 71,
        base,
    };
    // SAFETY: GDT 已初始化且是静态内存；栈由调用方保证有效。
    unsafe { gdt_load(core::ptr::addr_of!(gdtr) as *const u8) };
}

/// 当前 PML4 的物理地址（AP 要和 BSP 用同一套页表）。
pub fn pml4_phys() -> u64 {
    crate::mm::paging::read_cr3()
}

/// 设置 `TSS.rsp0`：从 ring3 陷入内核时用的栈顶。
///
/// 每次切换到用户任务都要改——每个任务必须有**自己的**内核栈，
/// 否则一个任务的内核栈会被另一个任务踩烂（而且查起来极难）。
pub fn set_rsp0(top: u64) {
    // SAFETY: packed 结构体字段写入，单核，调用点在调度器（中断关闭）。
    unsafe {
        (*TSS.0.get()).rsp0 = top;
    }
    // `syscall` 指令不读 TSS，只认这个全局量——两处必须永远同步，
    // 否则系统调用会用上一个任务的内核栈（症状：随机踩烂别人的栈）。
    crate::arch::syscall::set_kstack(top);
}

/// 当前的 `TSS.rsp0`（`syscall` 初始化时用它当初始内核栈）。
pub fn rsp0_or_zero() -> u64 {
    // SAFETY: 单核，只读。
    unsafe { (*TSS.0.get()).rsp0 }
}

/// 读回 GDTR。
pub fn read_gdtr() -> (u64, u16) {
    let mut g = Gdtr { limit: 0, base: 0 };
    // SAFETY: sgdt 向 10 字节 packed 结构写入。
    unsafe { asm!("sgdt [{}]", in(reg) core::ptr::addr_of_mut!(g), options(nostack)) };
    (g.base, g.limit)
}

/// 读回任务寄存器。
pub fn read_tr() -> u16 {
    let v: u16;
    // SAFETY: str 只读，无副作用。
    unsafe { asm!("str {0:x}", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}
