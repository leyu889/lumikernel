/*
 * LumiKernel 入口（M1：自建高半区页表）
 *
 * 交接约定（与 boot/stage2.asm 对齐，不可单方面修改）：
 *   - 进入时：CS=0x18，DS/ES/SS=0x10，长模式，stage2 的恒等映射已开，中断关闭
 *   - RIP = 0x100000（物理）。本文件负责：建自己的页表 → 跳高半区 → 调 kmain
 *   - E820 表固定在物理 0x9000：u16 条目数 + 从 0x9008 起每条 24 字节
 *
 * 最终地址空间（M1）：
 *   0x0000000000000000 + 0..1GiB   恒等映射（留给访问物理内存/MMIO 的兜底）
 *   0xFFFF800000000000 + 0..1GiB   直接映射区（内核访问物理页的正规通道）
 *   0xFFFFFFFF80000000 + 0..1GiB   内核映像（高半区）
 * 三者都指向物理 0..1GiB，用同一张 PD，省两页且不会不一致。
 *
 * 注意：本文件里所有 `leaq sym(%rip)` 在切页表**之前**得到的是物理地址
 * （RIP 还是物理值），这正是写页表项需要的；切完之后同样的写法得到高半区地址。
 */

.set KERNEL_VA_BASE, 0xFFFFFFFF80000000

.section .text.entry, "ax", @progbits
.code64
.global _start
_start:
    cli
    cld
    /* 串口探针 'S'：**内核第一条指令**就发一个字节。
       BIOS 链上它证明"stage2 交棒成功"；UEFI 链上它证明"装载器真的跳到了
       0x100000"——两种引导方式出了问题，第一个要分辨的就是这一件事。 */
    movw $0x3F8, %dx
    movb $0x53, %al
    outb %al, %dx

    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs

    /* 早期栈：仍在物理地址区（.bss 里的 stack，此时 RIP 是物理值） */
    leaq stack_top(%rip), %rsp
    xorq %rbp, %rbp


    /* 保住入参 %edi（E820 表地址）。下面清零 .bss 会用 RDI 当目标指针
       （`rep stosb` 结束时 RDI 变成 __bss_end），把装载器传来的地址冲掉——
       UEFI 链上表现是"表里有 17 项，内核却读到空表"。 */
    pushq %rdi

    /* 清零 .bss：stage1 只搬文件内容，.bss 在磁盘上不占空间。
       页表数组本身就在 .bss，必须先清再填。 */
    leaq __bss_start(%rip), %rdi
    leaq __bss_end(%rip), %rcx
    subq %rdi, %rcx
    xorl %eax, %eax
    rep stosb
    popq %rdi                   /* 恢复 E820 表地址 */

    /* 给栈（含紧邻下方的金丝雀页）画图案（M11）。
       位置有讲究：**必须在 .bss 清零之后**——栈本身就在 .bss 里，
       先画后清等于白画（踩过一次，表现是"高水位恒等于 100%"）。
       从 `__stack_canary` 填到 `stack_top-4KiB`：canary 是夹在
       栈与页表之间的一页，溢出时会先踩它，而不是踩坏 pd_1g。 */
    /* 注意寄存器选择：%rdi 里还躺着 E820 表地址，绝不能用它当游标
       （上一版就踩了这个：E820 变成空表，后面全线 FAIL）。 */
    leaq __stack_canary(%rip), %r8
    leaq stack_top(%rip), %r9
    subq $4096, %r9
    movabsq $0x5A5A5A5A5A5A5A5A, %rax
.Lpaint_stack:
    cmpq %r9, %r8
    jae .Lpaint_done
    movq %rax, (%r8)
    addq $8, %r8
    jmp .Lpaint_stack
.Lpaint_done:

    /* ------------------------------------------------ 填 512 个 2MiB 大页 */
    leaq pd_1g(%rip), %rbx
    movl $512, %ecx
    movq $0x83, %rax            /* P | RW | PS */
1:  movq %rax, (%rbx)
    addq $0x200000, %rax
    addq $8, %rbx
    decl %ecx
    jnz 1b

    /* ------------------------------------------------ 串三级表 */
    leaq pml4(%rip), %r12
    leaq pdpt_low(%rip), %r13
    leaq pdpt_kernel(%rip), %r14
    leaq pdpt_direct(%rip), %r15
    leaq pd_1g(%rip), %rbx

    movq %rbx, %rax
    orq $3, %rax                /* 指向 pd_1g，P|RW */
    movq %rax, (%r13)           /* pdpt_low[0]    = 物理 0       */
    movq %rax, 4080(%r14)       /* pdpt_kernel[510] = 0xFFFFFFFF80000000
                                   ★ 坑：高半区 [511] 覆盖的 512 GiB 从
                                   0xFFFFFF8000000000 起，内核基址在其中
                                   的第 510 个 1 GiB 槽，不是第 0 个。
                                   写错的表现是"读高半区地址就三重故障"。 */
    movq %rax, (%r15)           /* pdpt_direct[0] = 0xFFFF800000000000 */

    movq %r13, %rax
    orq $3, %rax
    movq %rax, (%r12)           /* pml4[0]   恒等映射 */

    movq %r15, %rax
    orq $3, %rax
    movq %rax, 2048(%r12)       /* pml4[256] 直接映射区 0xFFFF800000000000 */

    movq %r14, %rax
    orq $3, %rax
    movq %rax, 4088(%r12)       /* pml4[511] 内核高半区 0xFFFFFFFF80000000 */

    /* ------------------------------------------------ 切页表 */
    movq %r12, %cr3

    /* ------------------------------------------------ 跳高半区
       物理地址靠 pml4[0] 的恒等映射继续可执行；目标地址由链接器给成高半区值。 */
    movabsq $_high_entry, %rax
    jmp *%rax

_high_entry:
    /* 现在 RIP 在 0xFFFFFFFF80100xxx；栈指针也要换成高半区地址 */
    leaq stack_top(%rip), %rsp
    xorq %rbp, %rbp

    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs

    /* 开 NX：kpt 给所有数据页都写 NX(bit63)，若 EFER.NXE=0，
       页表项里的 bit63 会被判成"保留位违规"→ #PF → 没有 IDT → 三重故障。
       这个坑只在"自己写页表"时才会踩到。 */
    movl $0xC0000080, %ecx      /* IA32_EFER */
    rdmsr
    orl  $0x800, %eax           /* EFER.NXE = 1 */
    wrmsr

    /* 开 SSE：x86_64 编译基线带 SSE2，CR4.OSFXSR=0 时一条 movdqu 就 #UD */
    movq %cr0, %rax
    andq $~0x4, %rax            /* CR0.EM = 0 */
    orq  $0x2, %rax             /* CR0.MP = 1 */
    movq %rax, %cr0
    movq %cr4, %rax
    orq  $0x600, %rax           /* CR4.OSFXSR | CR4.OSXMMEXCPT */
    movq %rax, %cr4

    /* 串口探针 'K'：比屏幕更早、更可靠的证据 */
    movw $0x3F8, %dx
    movb $0x4B, %al
    outb %al, %dx

    /* 参数：E820 表物理地址。
       BIOS 链把它固化在 0x9000；UEFI 装载器拿到的内存图在固件分配的池里，
       地址不固定，所以由它先把 %edi 设好，这里只在为空时兜底。 */
    testl %edi, %edi
    jnz 1f
    movl $0x9000, %edi
1:  call kmain

2:  cli
    hlt
    jmp 2b

/* ------------------------------------------------------------------
 * 早期页表（BSS 里，启动时清零再填）
 * 每张表必须 4 KiB 对齐；物理地址 = 高半区地址 - 0xFFFFFFFF80000000
 * ------------------------------------------------------------------ */
.section .bss, "aw", @nobits
.align 4096
pml4:
    .skip 4096
pdpt_low:
    .skip 4096
pdpt_kernel:
    .skip 4096
pdpt_direct:
    .skip 4096
pd_1g:
    .skip 4096

/* 栈金丝雀：夹在页表和栈之间的一页。
   栈从 `stack_top` 向下长，溢出时**先**踩到这里，而不是直接踩 pd_1g。
   （M11 实测踩过一次：64 KiB 栈溢出 1.2 KiB，把 pd_1g 的第 357 项
   写成一个内核返回地址，直接映射在 714 MiB 处静默失效，
   表现为"跑了 2 万次 mmap 之后随机页错误"。） */
.align 4096
.global __stack_canary
__stack_canary:
    .skip 4096

.align 16
.global __stack_bottom
__stack_bottom:
stack:
    .skip 262144
stack_top:
