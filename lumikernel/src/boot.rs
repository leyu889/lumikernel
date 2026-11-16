//! 引导入口：把 boot.S 编进内核镜像。
//! `_start` → 32 位保护模式建表 → 长模式 → `kmain`。

core::arch::global_asm!(include_str!("boot.S"), options(att_syntax));
