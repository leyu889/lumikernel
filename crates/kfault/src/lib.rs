//! kfault —— x86_64 异常/中断向量的语义解码
//!
//! 内核的异常处理器必须做到"只做算术和查表，不做任何可能失败的事"：
//! 它运行时内核状态已经不可信（可能栈坏了、可能堆锁被持有、可能正在缺页）。
//! 所以这个 crate 刻意保持**零依赖、零分配、无 panic**，只提供：
//!
//! - 向量号 → 名字 / 中文说明（查表，无分支预测灾难）；
//! - 向量号 → 是否带 CPU 压入的错误码（[`has_error_code`]）——
//!   这一条错了，栈帧整体错位 8 字节，打印出来的 rip 会是垃圾；
//! - 页错误错误码 → 可读标志串（[`describe_pf`]），写进调用方给的缓冲区。
//!
//! 为什么单独成 crate 而不是塞进内核：这些表和解码逻辑是**可以被单测覆盖**的，
//! 而内核里的处理器只有真机跑到才验证得到。把能测的部分挪出来，
//! 真机上要赌的就只剩"栈帧布局"这一件事。

#![cfg_attr(not(test), no_std)]

/* ---------------- 向量号常量 ---------------- */

pub const VEC_DE: u64 = 0; // 除零
pub const VEC_DB: u64 = 1; // 调试
pub const VEC_NMI: u64 = 2;
pub const VEC_BP: u64 = 3; // 断点
pub const VEC_OF: u64 = 4; // 溢出
pub const VEC_BR: u64 = 5; // 越界
pub const VEC_UD: u64 = 6; // 非法指令
pub const VEC_NM: u64 = 7; // 设备不可用
pub const VEC_DF: u64 = 8; // 双重故障
pub const VEC_TS: u64 = 10; // 非法 TSS
pub const VEC_NP: u64 = 11; // 段不存在
pub const VEC_SS: u64 = 12; // 栈段错误
pub const VEC_GP: u64 = 13; // 通用保护
pub const VEC_PF: u64 = 14; // 页错误
pub const VEC_MF: u64 = 16; // x87 浮点
pub const VEC_AC: u64 = 17; // 对齐检查
pub const VEC_MC: u64 = 18; // 机器检查
pub const VEC_XM: u64 = 19; // SIMD 浮点
pub const VEC_VE: u64 = 20; // 虚拟化
pub const VEC_CP: u64 = 21; // 控制保护

/// 第一个外部中断（8259A 重映射后的 IRQ0）。
pub const IRQ_BASE: u64 = 32;
/// 最后一个外部中断向量（IRQ15）。
pub const IRQ_LAST: u64 = 47;

/* ---------------- 查表 ---------------- */

/// 向量的英文短名（日志里的 `tag`，纯 ASCII，屏幕上也能读）。
pub fn name(vector: u64) -> &'static str {
    match vector {
        0 => "divide-error",
        1 => "debug",
        2 => "nmi",
        3 => "breakpoint",
        4 => "overflow",
        5 => "bound-range",
        6 => "invalid-opcode",
        7 => "device-na",
        8 => "double-fault",
        9 => "coprocessor",
        10 => "invalid-tss",
        11 => "segment-np",
        12 => "stack-segment",
        13 => "general-protection",
        14 => "page-fault",
        15 => "reserved",
        16 => "x87-fp",
        17 => "alignment-check",
        18 => "machine-check",
        19 => "simd-fp",
        20 => "virtualization",
        21 => "control-protection",
        22..=31 => "reserved",
        32..=47 => "irq",
        _ => "unknown",
    }
}

/// 向量的中文说明，给串口日志用。
pub fn desc_zh(vector: u64) -> &'static str {
    match vector {
        0 => "除零：除数寄存器为 0（整数除法不检查，硬件直接报错）",
        1 => "调试异常：断点/单步/数据断点",
        2 => "不可屏蔽中断：硬件级告警，通常意味着内存或总线出错",
        3 => "断点：执行了 INT3 指令",
        4 => "溢出：执行 INTO 且 OF=1",
        5 => "越界：BOUND 指令的索引超出范围",
        6 => "非法指令：CPU 不认识的编码，常见于函数指针被踩坏",
        7 => "设备不可用：CR0.TS/EM 与 x87 状态不一致",
        8 => "双重故障：处理一个异常时又抛出异常，CPU 处于危险状态",
        9 => "协处理器段越界（486 之后不再产生）",
        10 => "非法 TSS：任务状态段描述符有问题",
        11 => "段不存在：引用的段描述符 P=0",
        12 => "栈段错误：SS 相关访问越界或不可写",
        13 => "通用保护：特权级不足、非规范地址、写只读段等",
        14 => "页错误：页表里没有这个映射，或权限不允许",
        15 => "保留向量，不应被触发",
        16 => "x87 浮点错误",
        17 => "对齐检查：AC=1 时访问了未对齐的内存",
        18 => "机器检查：硬件报告的不可纠正错误",
        19 => "SIMD 浮点错误",
        20 => "虚拟化异常（EPT 违规等）",
        21 => "控制保护：CET 影子栈相关",
        22..=31 => "保留向量，不应被触发",
        32..=47 => "外部中断（8259A PIC 或本地 APIC）",
        _ => "未知向量",
    }
}

/// 该向量由 CPU 压入错误码吗？
///
/// **这条错了，整个栈帧错位 8 字节**，恢复出来的 rip/rsp 全是垃圾。
/// 带错误码的向量在 Intel SDM Vol.3 表 6-1 里是固定的 8/10/11/12/13/14/17/21
/// （外加 29/30 的 VMM 通信类异常，本内核不用）。
pub fn has_error_code(vector: u64) -> bool {
    matches!(vector, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21)
}

/// 是 CPU 异常（0..=31）还是外部中断（32..=47）。
pub fn is_exception(vector: u64) -> bool {
    vector < 32
}

/// 不修改指令流就无法「跳过」的异常是致命异常。
///
/// 断点（3）和调试（1）可以由处理器自己推进 rip 后返回，其余异常
/// 意味着"这条指令根本执行失败"，盲目返回会无限重入 → 直接停机更安全。
pub fn is_fatal(vector: u64) -> bool {
    !matches!(vector, 1 | 3)
}

/* ---------------- 页错误错误码 ---------------- */

/// 页错误错误码（`#PF` 压栈的那个值）的位解码。
///
/// 只做位运算，不做 IO；构造后可以随意复制。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PfError(pub u64);

impl PfError {
    /// P：0 = 页不存在（真的没映射），1 = 页存在但权限不允许。
    #[inline]
    pub fn present(&self) -> bool {
        self.0 & 1 != 0
    }
    /// W：0 = 读或取指触发，1 = 写触发。
    #[inline]
    pub fn write(&self) -> bool {
        self.0 & 2 != 0
    }
    /// U：0 = 内核态触发，1 = 用户态触发。
    #[inline]
    pub fn user(&self) -> bool {
        self.0 & 4 != 0
    }
    /// RSVD：页表项里用了保留位（通常是 NX 未启用却写了 bit63）。
    #[inline]
    pub fn reserved(&self) -> bool {
        self.0 & 8 != 0
    }
    /// I/D：取指令时触发。
    #[inline]
    pub fn instr_fetch(&self) -> bool {
        self.0 & 0x10 != 0
    }
    /// PK：保护键拦截。
    #[inline]
    pub fn protection_key(&self) -> bool {
        self.0 & 0x20 != 0
    }
    /// SS：影子栈（CET）违规。
    #[inline]
    pub fn shadow_stack(&self) -> bool {
        self.0 & 0x40 != 0
    }
}

/// 把 `#PF` 错误码格式化成定长 ASCII 串，返回写入长度。
///
/// 输出形如 `P=0 W=0 U=0 RSVD=0 ID=0 PK=0 SS=0`。缓冲区不够时截断
/// （返回的是**需要的**长度，调用方可以据此判断是否被截断）。
pub fn describe_pf(err: u64, out: &mut [u8]) -> usize {
    let e = PfError(err);
    let mut pos = 0usize;
    let mut first = true;
    for (name, set) in [
        (&b"P"[..], e.present()),
        (&b"W"[..], e.write()),
        (&b"U"[..], e.user()),
        (&b"RSVD"[..], e.reserved()),
        (&b"ID"[..], e.instr_fetch()),
        (&b"PK"[..], e.protection_key()),
        (&b"SS"[..], e.shadow_stack()),
    ] {
        if !first {
            put(out, &mut pos, b" ");
        }
        first = false;
        put(out, &mut pos, name);
        put(out, &mut pos, if set { &b"=1"[..] } else { &b"=0"[..] });
    }
    pos
}

fn put(out: &mut [u8], pos: &mut usize, s: &[u8]) {
    for &b in s {
        if let Some(slot) = out.get_mut(*pos) {
            *slot = b;
        }
        *pos += 1;
    }
}

/* ---------------- 中断向量的异常溯源 ---------------- */

/// 异常发生后内核应当采取的动作。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// 可以安全返回（断点/调试），处理器会推进 rip。
    Resume,
    /// 试图恢复现场；失败就停机。
    RecoverOrHalt,
    /// 直接停机（内核态不可恢复的编程错误）。
    Halt,
}

/// 给定向量决定处理策略。M2 只有 `#BP` 允许直接返回。
pub fn action(vector: u64) -> Action {
    match vector {
        1 | 3 => Action::Resume,
        0 | 4 | 5 | 6 | 7 | 10..=14 | 16..=21 => Action::RecoverOrHalt,
        _ => Action::Halt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_vectors_match_sdm() {
        for v in 0..=31u64 {
            let expect = matches!(v, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21);
            assert_eq!(has_error_code(v), expect, "向量 {} 的错误码判断错了", v);
        }
    }

    #[test]
    fn irq_vectors_never_have_error_code() {
        for v in IRQ_BASE..=IRQ_LAST {
            assert!(!has_error_code(v));
            assert!(!is_exception(v));
        }
    }

    #[test]
    fn names_are_ascii_and_unique_enough() {
        for v in 0..=47u64 {
            let n = name(v);
            assert!(n.is_ascii(), "向量 {} 的名字含非 ASCII", v);
            assert!(!n.is_empty());
        }
        assert_eq!(name(14), "page-fault");
        assert_eq!(name(8), "double-fault");
        assert_eq!(name(32), "irq");
    }

    #[test]
    fn desc_zh_is_not_empty() {
        for v in 0..=47u64 {
            assert!(!desc_zh(v).is_empty(), "向量 {} 没有中文说明", v);
        }
    }

    #[test]
    fn pf_all_zero() {
        let e = PfError(0);
        assert!(!e.present() && !e.write() && !e.user());
        assert!(!e.reserved() && !e.instr_fetch());
    }

    #[test]
    fn pf_bit_by_bit() {
        assert!(PfError(1 << 0).present());
        assert!(PfError(1 << 1).write());
        assert!(PfError(1 << 2).user());
        assert!(PfError(1 << 3).reserved());
        assert!(PfError(1 << 4).instr_fetch());
        assert!(PfError(1 << 5).protection_key());
        assert!(PfError(1 << 6).shadow_stack());
    }

    #[test]
    fn pf_no_cross_talk() {
        // 只置 W 位，其它位必须都是 0
        let e = PfError(0b10);
        assert!(!e.present());
        assert!(e.write());
        assert!(!e.user() && !e.reserved() && !e.instr_fetch());
    }

    #[test]
    fn describe_read_not_present() {
        let mut buf = [0u8; 64];
        let n = describe_pf(0, &mut buf);
        assert_eq!(&buf[..n], b"P=0 W=0 U=0 RSVD=0 ID=0 PK=0 SS=0");
    }

    #[test]
    fn describe_write_user_present() {
        let mut buf = [0u8; 64];
        let n = describe_pf(0b111, &mut buf);
        assert_eq!(&buf[..n], b"P=1 W=1 U=1 RSVD=0 ID=0 PK=0 SS=0");
    }

    #[test]
    fn describe_truncates_but_reports_need() {
        let mut small = [0u8; 8];
        let need = describe_pf(0, &mut small);
        assert!(need > small.len(), "应当报告被截断");
        assert_eq!(&small[..], b"P=0 W=0 ");
    }

    #[test]
    fn describe_ignores_high_bits() {
        // bit 15（SGX）等本 crate 不解码的位不应影响输出
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        let na = describe_pf(0, &mut a);
        let nb = describe_pf(1 << 15, &mut b);
        assert_eq!(&a[..na], &b[..nb]);
    }

    #[test]
    fn fatal_classification() {
        assert!(!is_fatal(3), "断点可以返回");
        assert!(!is_fatal(1), "调试可以返回");
        assert!(is_fatal(0));
        assert!(is_fatal(14));
        assert!(is_fatal(13));
    }

    #[test]
    fn action_policy() {
        assert_eq!(action(3), Action::Resume);
        assert_eq!(action(14), Action::RecoverOrHalt);
        assert_eq!(action(0), Action::RecoverOrHalt);
        assert_eq!(action(2), Action::Halt);
        assert_eq!(action(8), Action::Halt);
    }

    #[test]
    fn irq_range_bounds() {
        assert_eq!(IRQ_BASE, 32);
        assert_eq!(IRQ_LAST, 47);
        // 32..47 一共 16 个 IRQ，对应 8259A 的两片
        assert_eq!(IRQ_LAST - IRQ_BASE + 1, 16);
    }
}
