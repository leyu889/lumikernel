//! IDT（中断描述符表）构建与装载。
//!
//! 只装载 0..47 这 48 个向量：0..31 是 CPU 异常，32..47 是 8259A 重映射后的 IRQ。
//! 其余 208 个门保持 `present = 0`——访问它们会得到 `#GP`，
//! 比跳到地址 0 然后执行随机数据安全得多。

use core::cell::UnsafeCell;
use core::arch::asm;


/// 一个 16 字节的中断门。
///
/// 直接按位拼，不拆成 7 个字段：拆开会引入 padding 与端序问题，
/// 而位布局在 Intel SDM 里是固定的。
#[derive(Clone, Copy)]
#[repr(C, align(8))]
pub struct IdtEntry {
    lo: u64,
    hi: u64,
}

impl IdtEntry {
    pub const fn missing() -> Self {
        IdtEntry { lo: 0, hi: 0 }
    }

    /// 填入中断门：`P=1, DPL=0, 类型=0xE`（64 位中断门），IST=0。
    pub fn set(&mut self, handler: u64, selector: u16) {
        self.set_full(handler, selector, 0, 0);
    }

    /// 带 IST 与 DPL 的完整版本。
    ///
    /// - `ist`：0 表示沿用当前栈；1..7 表示切到 TSS 里对应的 IST 栈。
    ///   `#DF` 必须用 IST1——它最常见的成因就是栈坏了，不能再指望栈。
    /// - `dpl`：3 用于系统调用门（用户态要能 `int 0x80`）。
    ///
    /// 用中断门（清 IF）而不是陷阱门：处理期间不允许被同级中断打断。
    pub fn set_full(&mut self, handler: u64, selector: u16, ist: u8, dpl: u8) {
        let attr = 0x8Eu64 | ((dpl as u64 & 3) << 5);
        self.lo = (handler & 0xFFFF)
            | ((selector as u64) << 16)
            | ((ist as u64 & 0x7) << 32)
            | (attr << 40)
            | (((handler >> 16) & 0xFFFF) << 48);
        self.hi = handler >> 32;
    }

    /// IST 索引（0 表示不用 IST）。
    pub fn ist(&self) -> u8 {
        ((self.lo >> 32) & 0x7) as u8
    }

    /// 特权级（门的 DPL）。
    pub fn dpl(&self) -> u8 {
        ((self.lo >> 45) & 0x3) as u8
    }

    pub fn handler(&self) -> u64 {
        (self.lo & 0xFFFF) | (((self.lo >> 48) & 0xFFFF) << 16) | (self.hi << 32)
    }

    pub fn present(&self) -> bool {
        (self.lo >> 40) & 0x80 != 0
    }

    /// 目标代码段选择子。
    pub fn selector(&self) -> u16 {
        ((self.lo >> 16) & 0xFFFF) as u16
    }
}

/// `lidt` / `sidt` 的操作数（10 字节，**必须 packed**）。
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Idtr {
    limit: u16,
    base: u64,
}

struct IdtCell(UnsafeCell<[IdtEntry; 256]>);

// SAFETY: 只在单核启动阶段由 `init` 写入一次；装载后 CPU 只读。
// 后续若要在运行时改门（比如 syscall），必须先进 `intr` 自旋锁。
unsafe impl Sync for IdtCell {}

static IDT: IdtCell = IdtCell(UnsafeCell::new([IdtEntry::missing(); 256]));

extern "C" {
    /// 由 `stubs.S` 提供：48 个入口桩地址。
    static isr_table: [u64; 48];
    /// 系统调用入口（`int 0x80`）。
    fn isr128();
    /// 核间中断入口。
    fn isr241();
}

/// 已装载的 IDT 基址（AP 要用同一个）。
static IDT_BASE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// 系统调用向量号。
pub const SYSCALL_VEC: u64 = 128;
/// 核间中断向量号（与 `stubs.S` 里的 `IPI_VECTOR_NUM` 保持一致）。
pub const IPI_VEC: u64 = 241;

/// 内核代码段选择子（stage2 的 GDT：0x18 是 64 位代码段）。
pub const KERNEL_CS: u16 = 0x18;

/// 已装载的向量个数。
pub const LOADED: usize = 48;

/// 构建并装载 IDT，返回 IDTR 的基址。
pub fn init() -> Result<u64, &'static str> {
    // SAFETY: 启动阶段单核、中断关闭，`IDT` 无并发访问者；
    // `isr_table` 是链接期固定的 48 个 u64，长度与声明一致。
    let base = unsafe {
        let table = &mut *IDT.0.get();
        let src = core::ptr::addr_of!(isr_table) as *const u64;
        for i in 0..LOADED {
            let h = core::ptr::read(src.add(i));
            if h < crate::mm::KERNEL_VA_BASE {
                // 入口桩必须落在内核映像里；落在别处说明链接或表坏了。
                return Err("ISR 入口地址不在内核高半区");
            }
            table[i].set(h, KERNEL_CS);
        }
        // 系统调用门：DPL=3，用户态才能 int 0x80
        table[SYSCALL_VEC as usize].set_full(isr128 as *const () as u64, KERNEL_CS, 0, 3);
        // 核间中断：DPL=0，只有内核能发
        table[IPI_VEC as usize].set_full(isr241 as *const () as u64, KERNEL_CS, 0, 0);

        // 剩下的门保持 present=0
        for i in LOADED..256 {
            if i != SYSCALL_VEC as usize && i != IPI_VEC as usize {
                table[i] = IdtEntry::missing();
            }
        }
        IDT.0.get() as u64
    };

    let idtr = Idtr {
        limit: (256 * 16 - 1) as u16,
        base,
    };
    let p = core::ptr::addr_of!(idtr);
    // SAFETY: idtr 是 10 字节的合法操作数，指向本函数栈上的局部变量，
    // 在 lidt 执行期间有效。
    unsafe { asm!("lidt [{}]", in(reg) p, options(readonly, nostack, preserves_flags)) };
    IDT_BASE.store(base, core::sync::atomic::Ordering::Relaxed);
    Ok(base)
}

/// 重新装载 IDT（每个 AP 都要做：IDTR 是每 CPU 的寄存器）。
pub fn reload() {
    let base = IDT_BASE.load(core::sync::atomic::Ordering::Relaxed);
    if base == 0 {
        return;
    }
    let idtr = Idtr {
        limit: (256 * 16 - 1) as u16,
        base,
    };
    let p = core::ptr::addr_of!(idtr);
    // SAFETY: base 是本模块建好的 IDT；idtr 是本栈帧上的合法 10 字节操作数。
    unsafe { asm!("lidt [{}]", in(reg) p, options(readonly, nostack, preserves_flags)) };
}

/// 给某个向量挂上 IST 栈。必须在 TSS 装载之后调用。
pub fn set_ist(vector: u64, ist: u8) -> Result<(), &'static str> {
    if vector >= 256 || ist == 0 || ist > 7 {
        return Err("IST 参数非法");
    }
    // SAFETY: IDT 已装载；中断关闭期间改写单个门不会有并发读取。
    unsafe {
        let table = &mut *IDT.0.get();
        let e = table.get_mut(vector as usize).ok_or("向量越界")?;
        let h = e.handler();
        let sel = e.selector();
        let dpl = e.dpl();
        e.set_full(h, sel, ist, dpl);
    }
    Ok(())
}

/// 读回 IDTR（用 `sidt`），用于确认 `lidt` 真的生效了。
///
/// 只相信"我们调用了 lidt"是自欺欺人——写错操作数大小、结构体不是 packed，
/// 都会静默失败，直到第一个中断到来才炸。
pub fn read_idtr() -> (u64, u16) {
    let mut idtr = Idtr { limit: 0, base: 0 };
    let p = core::ptr::addr_of_mut!(idtr);
    // SAFETY: sidt 向指向的 10 字节写入，局部变量足够大且对齐（packed）。
    unsafe { asm!("sidt [{}]", in(reg) p, options(nostack, preserves_flags)) };
    (idtr.base, idtr.limit)
}

/// 取某个门的副本（诊断用）。
pub fn gate(vector: u64) -> Option<IdtEntry> {
    if vector >= 256 {
        return None;
    }
    // SAFETY: `IDT` 装载后只读；下标已判界。
    Some(unsafe { *(*IDT.0.get()).get(vector as usize)? })
}

/// 自检：IDTR 指向我们建的表、界正确、前 48 个门都 present 且落点在内核里。
pub fn selftest(base: u64) -> Result<(), &'static str> {
    let (b, l) = read_idtr();
    if b != base {
        return Err("IDTR 基址与预期不一致（lidt 没生效？）");
    }
    if l != (256 * 16 - 1) as u16 {
        return Err("IDTR 界限不对（结构体可能没 packed）");
    }
    for v in 0..LOADED as u64 {
        let g = gate(v).ok_or("门不存在")?;
        if !g.present() {
            return Err("已装载的向量却有门是 present=0");
        }
        if g.handler() < crate::mm::KERNEL_VA_BASE {
            return Err("门的目标地址不在内核高半区");
        }
        if g.selector() != KERNEL_CS {
            return Err("门的选择子不是内核代码段");
        }
    }
    for v in LOADED..256 {
        if v as u64 == SYSCALL_VEC || v as u64 == IPI_VEC {
            continue;
        }
        if gate(v as u64).map(|g| g.present()).unwrap_or(false) {
            return Err("未装载的向量却是 present=1");
        }
    }
    // 系统调用门必须是 DPL=3，否则 ring3 执行 int 0x80 会 #GP
    let sc = gate(SYSCALL_VEC).ok_or("syscall 门不存在")?;
    if !sc.present() || sc.dpl() != 3 {
        return Err("syscall 门不是 DPL=3");
    }
    Ok(())
}
