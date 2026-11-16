/*
 * AP（应用处理器）启动跳板（M4）
 *
 * 为什么需要它：SMP 里只有 BSP 是被 BIOS 引导起来的，其余核上电后停在
 * "等待 INIT-SIPI" 的状态。收到 SIPI 时，它进入 **实模式**，CS 基址 = vector<<12，
 * IP = 0 —— 也就是说 CPU 只会跑到 16 位、1 MiB 以内的地方。
 *
 * 所以 BSP 必须先把这段小代码拷到 0x8000，再用 SIPI(0x08) 把它叫醒。
 * 这段代码要做的事，和 stage2 当年做的一模一样：
 *   实模式 → 装 GDT → 保护模式 → 开 PAE → 装 CR3 → 开长模式 → 跳内核高半区。
 *
 * 两条硬约束：
 * 1. 代码被拷到 0x8000，但链接地址在内核映像里，所以**所有绝对地址都要写成
 *    `0x8000 + (label - ap_tramp_start)`**，不能直接引用 label。
 * 2. GDT 的段基址全是 0（平坦），因此保护模式/长模式里的跳转目标也得写成物理地址。
 *    身份映射覆盖了低 1 GiB，所以开分页之后立刻还能继续执行。
 *
 * 参数区固定在 0x7000（BSP 在发 SIPI 之前填好）：
 *   0x7000  u64  PML4 物理地址
 *   0x7008  u64  内核高半区入口
 *   0x7010  u64  该 AP 的内核栈顶
 *   0x7018  u32  APIC ID（仅用于调试）
 *
 * 遥测（BSP 读回后打印，见 smp.rs）：
 *   0x6000  u8   最后到达的阶段号（每个阶段一条 movb，不可能被"缓冲丢失"）
 *   0x6010  u32  被 SIPI 唤醒的次数（>1 说明上一次进来之后死了）
 *
 * 为什么用内存而不是串口：两个核同时往同一个 16550 写，字节会互相覆盖，
 * 而且 `outb` 不等发送寄存器空就继续写会丢字节。拿它做字节级探针，
 * 得到的是假数据——这个坑踩过一次，代价是半小时的错误结论。
 */

.section .ap_tramp, "ax"
.code16
.global ap_tramp_start
.global ap_tramp_end
ap_tramp_start:
    cli
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw $0x7C00, %sp           /* 给实模式一个合法栈，不必指望 SP 恰好是 0 */

    incl (0x6010)               /* 第几次被唤醒 */
    movb $1, (0x6000)           /* stage 1：真的从实模式进来了 */

    /* 装临时 GDT。
       规范写法只有 0F 01 /2 + **16 位绝对地址**，不要加 0x66/0x67 前缀：
       0x66 把操作数当 32 位、0x67 把地址当 32 位，两者叠在一起时
       读回来的 GDTR 是垃圾（实测 sgdt 得到 limit=21341 base=0x53f000ff）。
       GDT 描述符本身是 6 字节：16 位限长 + 32 位基址。 */
    .byte 0x0F, 0x01, 0x16
    .word 0x8000 + (ap_gdtr - ap_tramp_start)

    movb $2, (0x6000)           /* stage 2：GDT 已装载 */

    /* 开保护模式 */
    movl %cr0, %eax
    orl $0x01, %eax
    movl %eax, %cr0

    movb $3, (0x6000)           /* stage 3：CR0.PE=1 */

    /* 远跳转到 32 位段 */
    .byte 0x66, 0xEA
    .long 0x8000 + (ap_pm32 - ap_tramp_start)
    .word 0x08

.code32
ap_pm32:
    movb $4, (0x6000)           /* stage 4：保护模式 + 32 位代码段 */

    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs

    /* 开 PAE（64 位分页的前提） */
    movl %cr4, %eax
    orl $0x20, %eax
    movl %eax, %cr4

    /* 和 BSP 用同一套页表 */
    movl 0x7000, %eax
    movl %eax, %cr3

    movb $5, (0x6000)           /* stage 5：PAE + CR3 */

    /* EFER.LME | EFER.NXE —— NXE 必须开，否则数据页的 bit63 会被判保留位违规 */
    movl $0xC0000080, %ecx
    rdmsr
    orl $0x900, %eax
    wrmsr

    /* 开分页 */
    movl %cr0, %eax
    orl $0x80000001, %eax
    movl %eax, %cr0

    movb $6, (0x6000)           /* stage 6：分页已开，仍靠恒等映射跑在低地址 */

    .byte 0xEA
    .long 0x8000 + (ap_lm64 - ap_tramp_start)
    .word 0x18

.code64
ap_lm64:
    movb $7, (0x6000)           /* stage 7：长模式 */

    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs

    /* 开 SSE：x86_64 编译基线带 SSE2，CR4.OSFXSR=0 时一条 movdqu 就 #UD */
    movq %cr0, %rax
    andq $~0x4, %rax            /* 清 EM */
    orq $0x2, %rax              /* 置 MP */
    movq %rax, %cr0
    movq %cr4, %rax
    orq $0x600, %rax            /* OSFXSR | OSXMMEXCPT */
    movq %rax, %cr4

    movq 0x7010, %rsp           /* 各自的内核栈，别共用 BSP 的 */
    xorq %rbp, %rbp

    movb $8, (0x6000)           /* stage 8：栈已换，准备进 Rust */
    movq 0x7008, %rax           /* 高半区入口 */
    jmp *%rax

1:  cli
    hlt
    jmp 1b

/* ---------------- 临时 GDT ---------------- */
.align 8
ap_gdt:
    .quad 0x0000000000000000    /* 0x00 null */
    .quad 0x00CF9A000000FFFF    /* 0x08 32 位代码 */
    .quad 0x00CF92000000FFFF    /* 0x10 数据 */
    .quad 0x00AF9A000000FFFF    /* 0x18 64 位代码 */
    .quad 0x00CF92000000FFFF    /* 0x20 数据 */
ap_gdt_end:

ap_gdtr:
    .word ap_gdt_end - ap_gdt - 1
    .long 0x8000 + (ap_gdt - ap_tramp_start)

ap_tramp_end:
    .align 8
