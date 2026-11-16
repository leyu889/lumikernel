/*
 * `syscall` 指令入口（M9）
 *
 * 和 `int 0x80` 的区别（这三条决定了这里必须自己写）：
 *   1. `syscall` **不换栈**：它把返回地址放进 %rcx、RFLAGS 放进 %r11，
 *      然后直接跳到 LSTAR 指的地址，%rsp 仍然是用户栈。
 *      所以第一件事是把 %rsp 换成内核栈——而且必须先把用户 %rsp 存下来。
 *   2. `syscall` **不压栈帧**：没有 vector/error/rip/cs/rflags/rsp/ss。
 *      我们要手工拼一个和 `intr::frame::InterruptFrame` 一模一样的帧，
 *      这样后面的 `trap_return`、`task::exit_current`、抢占一整条路都能复用。
 *   3. 换栈的方式只能是"读一个全局变量"。`syscall` 不读 TSS，
 *      `swapgs` 又要配 GS base——我们没建那套基础设施，
 *      而调度只在 BSP 上跑（M4 起就这样，见 README 的已知限制），
 *      所以一个全局 u64 就是当前任务的内核栈顶，语义足够准确。
 *
 * 帧布局与 `stubs.S` 的 `isr_common` 完全一致：
 *   r15 r14 r13 r12 r11 r10 r9 r8 rbp rdi rsi rdx rcx rbx rax
 *   vector error rip cs rflags rsp ss
 *   共 176 字节。
 */

.text
.code64

.global syscall_entry
.type syscall_entry, @function
syscall_entry:
    /* 用户的 %rsp 先落盘，再换栈。这两步之间不能被任何东西打断：
       `syscall` 已经按 FMASK 把 IF 清掉了，所以安全。 */
    movq %rsp, SC_USER_RSP(%rip)
    movq SC_KSTACK(%rip), %rsp
    subq $176, %rsp

    /* 15 个通用寄存器。%rcx/%r11 里此刻是"返回地址 / 用户 RFLAGS"，
       它们同时也要进帧里的 rcx/r11 槽位——`syscall` 本来就把这两个
       寄存器定义为被破坏的，用户态不会指望它们保值。 */
    movq %r15, 0(%rsp)
    movq %r14, 8(%rsp)
    movq %r13, 16(%rsp)
    movq %r12, 24(%rsp)
    movq %r11, 32(%rsp)
    movq %r10, 40(%rsp)
    movq %r9,  48(%rsp)
    movq %r8,  56(%rsp)
    movq %rbp, 64(%rsp)
    movq %rdi, 72(%rsp)
    movq %rsi, 80(%rsp)
    movq %rdx, 88(%rsp)
    movq %rcx, 96(%rsp)
    movq %rbx, 104(%rsp)
    movq %rax, 112(%rsp)

    /* vector / error：填成和 int 0x80 一样，方便日志里两种入口混着看 */
    movq $128, 120(%rsp)
    movq $0,   128(%rsp)

    /* CPU 本该压的那五项，自己补齐 */
    movq %rcx, 136(%rsp)          /* rip = 返回地址 */
    movq $0x33, 144(%rsp)         /* cs = 用户代码段（DPL3） */
    movq %r11, 152(%rsp)          /* rflags */
    movq SC_USER_RSP(%rip), %rax
    movq %rax, 160(%rsp)          /* rsp = 用户栈 */
    movq $0x2b, 168(%rsp)         /* ss = 用户数据段（DPL3） */

    cld                           /* 别让用户态的 DF 传进内核 */

    movq %rsp, %rdi               /* 参数：&mut InterruptFrame */
    call linux_syscall_entry

    /* 和中断路径回到同一个出口：trap_return 会 pop 15 个寄存器、
       丢掉 vector/error、然后 iretq 回 ring3。 */
    jmp trap_return

.size syscall_entry, . - syscall_entry
