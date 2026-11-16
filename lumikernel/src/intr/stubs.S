/*
 * 中断/异常入口桩（M2）
 *
 * 统一栈帧布局（地址从低到高，也就是从 %rsp 往上）：
 *
 *   r15 r14 r13 r12 r11 r10 r9 r8 rbp rdi rsi rdx rcx rbx rax
 *   vector  error  rip  cs  rflags  rsp  ss
 *
 * 与 Rust 侧 `intr::frame::InterruptFrame` 的字段顺序**逐字节对应**，
 * 改这里必须同步改那边，否则打印出来的寄存器全是错位的（而且不会报错，
 * 只会显示一堆看似合理的垃圾值——这种 bug 最难查）。
 *
 * 两条铁律：
 * 1. 没带错误码的向量必须由桩压一个 0 占位，否则帧错位 8 字节；
 * 2. 调用 Rust 之前必须把 %rsp 对齐到 16 字节（SysV ABI），
 *    否则 Rust 里的 movaps 会 #GP —— 在处理 #GP 的路径上再 #GP 就是三重故障。
 */

.text
.code64

/* ---------------- 入口桩 ---------------- */

.macro ISR_NOERR n
.global isr\n
.type isr\n, @function
isr\n:
    pushq $0                    /* 占位错误码 */
    pushq $\n                   /* 向量号 */
    jmp isr_common
.endm

.macro ISR_ERR n
.global isr\n
.type isr\n, @function
isr\n:
    pushq $\n                   /* 错误码由 CPU 已经压好 */
    jmp isr_common
.endm

ISR_NOERR 0                     /* #DE 除零 */
ISR_NOERR 1                     /* #DB 调试 */
ISR_NOERR 2                     /* NMI */
ISR_NOERR 3                     /* #BP 断点 */
ISR_NOERR 4                     /* #OF 溢出 */
ISR_NOERR 5                     /* #BR 越界 */
ISR_NOERR 6                     /* #UD 非法指令 */
ISR_NOERR 7                     /* #NM 设备不可用 */
ISR_ERR   8                     /* #DF 双重故障 */
ISR_NOERR 9
ISR_ERR   10                    /* #TS 非法 TSS */
ISR_ERR   11                    /* #NP 段不存在 */
ISR_ERR   12                    /* #SS 栈段错误 */
ISR_ERR   13                    /* #GP 通用保护 */
ISR_ERR   14                    /* #PF 页错误 */
ISR_NOERR 15
ISR_NOERR 16                    /* #MF x87 */
ISR_ERR   17                    /* #AC 对齐检查 */
ISR_NOERR 18                    /* #MC 机器检查 */
ISR_NOERR 19                    /* #XM SIMD */
ISR_NOERR 20                    /* #VE 虚拟化 */
ISR_ERR   21                    /* #CP 控制保护 */
ISR_NOERR 22
ISR_NOERR 23
ISR_NOERR 24
ISR_NOERR 25
ISR_NOERR 26
ISR_NOERR 27
ISR_NOERR 28
ISR_NOERR 29
ISR_NOERR 30
ISR_NOERR 31

/* IRQ0..IRQ15 → 向量 32..47（8259A 重映射之后） */
ISR_NOERR 32
ISR_NOERR 33
ISR_NOERR 34
ISR_NOERR 35
ISR_NOERR 36
ISR_NOERR 37
ISR_NOERR 38
ISR_NOERR 39
ISR_NOERR 40
ISR_NOERR 41
ISR_NOERR 42
ISR_NOERR 43
ISR_NOERR 44
ISR_NOERR 45
ISR_NOERR 46
ISR_NOERR 47

/* 核间中断（IPI）向量：BSP 用它戳 AP 做 reschedule / TLB shootdown。
   放 0xF1 是为了离 0x20..0x2F 的 IRQ 段远一点，出问题好辨认。 */
.global IPI_VECTOR_NUM
.set IPI_VECTOR_NUM, 241
ISR_NOERR 241

/* 系统调用向量：int 0x80（M3）。门在 idt.rs 里单独设成 DPL=3。 */
ISR_NOERR 128

/* ---------------- 公共返回路径 ---------------- */

.type isr_common, @function
isr_common:
    pushq %rax
    pushq %rbx
    pushq %rcx
    pushq %rdx
    pushq %rsi
    pushq %rdi
    pushq %rbp
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15

    cld                         /* 内核内部约定 DF=0，别让用户态的 DF 传进来 */
    movq %rsp, %rdi             /* 参数：&mut InterruptFrame */
    movq %rsp, %rbp             /* 存一份原始帧顶，%rbp 的值随后从帧里 pop 恢复 */
    andq $-16, %rsp             /* 对齐到 16，给 Rust 的 SSE 指令留活路 */
    call isr_dispatch
    movq %rbp, %rsp             /* 回到帧顶 */
    /* 故意 fallthrough 到 trap_return */

/*
 * trap_return：从 InterruptFrame 恢复现场并 iretq。
 *
 * 它是**两条路径的汇合点**：
 *   1. 正常中断/异常返回（isr_common 直接落下来）；
 *   2. 新任务第一次被调度（switch_to 的 ret 直接跳到这里）。
 * 第 2 条能成立的前提是 `task::build_stack` 把栈拼成了
 * [6 个 callee-saved][trap_return][InterruptFrame] 的形状。
 */
.global trap_return
.type trap_return, @function
trap_return:
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rbp
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax
    addq $16, %rsp              /* 丢掉 vector 与 error */
    iretq

/* ---------------- 入口地址表（Rust 侧按数组读） ---------------- */

.section .rodata
.align 8
.global isr_table
isr_table:
    .quad isr0,  isr1,  isr2,  isr3,  isr4,  isr5,  isr6,  isr7
    .quad isr8,  isr9,  isr10, isr11, isr12, isr13, isr14, isr15
    .quad isr16, isr17, isr18, isr19, isr20, isr21, isr22, isr23
    .quad isr24, isr25, isr26, isr27, isr28, isr29, isr30, isr31
    .quad isr32, isr33, isr34, isr35, isr36, isr37, isr38, isr39
    .quad isr40, isr41, isr42, isr43, isr44, isr45, isr46, isr47

/* ------------------------------------------------------------------
 * __probe：受控故障探针
 *
 *   rdi = *mut ProbeCtx   （rsp/rip/addr/expect/armed/hits）
 *   rsi = *const ProbeAction
 *   返回 0 = 正常执行完；1 = 从异常中恢复
 *
 * 思路跟 Linux 的 exception fixup 表同源：先登记"出事就跳回这里"，
 * 再执行可能出错的指令，异常处理器验证确实是预期的那类故障后，
 * 把 iretq 的目标改成登记点。M5 的 copy_from_user 会复用同一套机制。
 * ------------------------------------------------------------------ */
.text
.global __probe
.type __probe, @function
__probe:
    pushq %rbp
    pushq %rbx
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    subq $8, %rsp               /* 6 次 push（48）+ 8 = 56，配合入口 8 → 16 对齐 */

    movq %rdi, %r14             /* %r14 保存上下文指针（被调用者保存，跨 call 有效） */
    leaq 2f(%rip), %rax         /* 恢复点 */
    movq %rsp, 0(%rdi)          /* ctx->rsp */
    movq %rax, 8(%rdi)          /* ctx->rip */
    movq $1, 32(%rdi)           /* ctx->armed = 1 */
    movq %rsi, %rdi             /* 参数：ProbeAction */
    call probe_action
    movq $0, 32(%r14)           /* 正常完成，解除武装 */
    movq %rax, %rbx
    jmp 3f

2:                              /* 由 iretq 回到这里，%rsp 已由处理器恢复成 ctx->rsp */
    movq $0, 32(%r14)
    movq $1, %rbx

3:
    movq %rbx, %rax
    addq $8, %rsp
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbx
    popq %rbp
    ret
