/*
 * 架构相关的几小段汇编（M3）
 *
 * 为什么不写在 Rust 的 asm! 里：
 * - 加载 GDT 之后必须用远返回来重载 CS，`retfq` 在 asm! 的 Intel 语法下写法容易出错；
 * - `switch_to` 的寄存器压栈顺序必须与 Rust 侧期望的栈布局严格一致，
 *   写成独立文件便于加注释、也便于和 `stubs.S` 对照。
 */

.text
.code64

/* ------------------------------------------------------------------
 * gdt_load(gdtr: *const u8)
 *
 * lgdt 之后必须重载 CS——老 CS 指向的描述符还在，但缓存里的段信息不会自动刷新。
 * 用 push/lretfq 做远返回是改 CS 的规范姿势（直接 mov cs, ax 是非法的）。
 * ------------------------------------------------------------------ */
.global gdt_load
.type gdt_load, @function
gdt_load:
    lgdt (%rdi)

    movw $0x10, %ax             /* 内核数据段 */
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs

    pushq $0x18                 /* 目标 CS：64 位内核代码段 */
    leaq 1f(%rip), %rax
    pushq %rax                  /* 目标 RIP */
    lretq
1:
    ret

/* ------------------------------------------------------------------
 * tss_load(sel: u16)
 * 把选择子装进任务寄存器（TR）。不装载 TSS 的话，从 ring3 进内核时
 * CPU 找不到 rsp0，会直接三重故障。
 * ------------------------------------------------------------------ */
.global tss_load
.type tss_load, @function
tss_load:
    movw %di, %ax
    ltr %ax
    ret

/* ------------------------------------------------------------------
 * switch_to(prev_rsp: *mut u64, next_rsp: u64)
 *
 * 只保存被调用者保存寄存器（SysV：rbp rbx r12-r15）——其余寄存器
 * 编译器本来就认为会被破坏，不需要我们管。
 *
 * 关键点：这里用的是"协程式"切换，保存的 rsp 指向这 6 个寄存器；
 * 恢复时先 pop 再 ret，于是**从中断处理函数里调用它也能正确往返**
 * （ret 回到 schedule 里 switch_to 的下一条指令）。
 *
 * 新任务的栈由 `task::build_stack` 手工拼好，返回地址填 `trap_return`，
 * 于是"第一次被调度"和"被抢占后恢复"走的是同一条返回路径。
 * ------------------------------------------------------------------ */
.global switch_to
.type switch_to, @function
switch_to:
    pushq %rbp
    pushq %rbx
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    movq %rsp, (%rdi)           /* *prev_rsp = 当前栈指针 */
    movq %rsi, %rsp             /* 切到对方的栈 */
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbx
    popq %rbp
    ret

/* ------------------------------------------------------------------
 * 双故障栈与 0 号任务的内核栈（静态，避免"分配器还没起来就要用栈"的顺序问题）
 * ------------------------------------------------------------------ */
.section .bss, "aw", @nobits
.align 16

/* 栈向下生长：标号放在区域**低端**，"top" 放在高端 */
.global ist1_stack
ist1_stack:
    .skip 16384                 /* IST1：4 页，给 #DF 用 */
.global ist1_stack_top
ist1_stack_top:

.global kstack0
kstack0:
    .skip 16384                 /* 0 号任务的内核栈 */
.global kstack0_top
kstack0_top:
