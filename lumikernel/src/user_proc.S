/*
 * 进程示例（M6）：同一份程序跑两份，各自一份私有数据页
 *
 * 它存在的意义只有一个：**证明地址空间是隔开的**。
 *
 * 做法：
 *   1. 取自己的 pid，写进 0x600000（这是 ELF 里的第二个段，纯 .bss，R+W）；
 *   2. 忙等一会儿，让两个进程在时间片上真的交替执行；
 *   3. 在同一地址 0x600008 上自增 300 次；
 *   4. 读回校验：0x600000 还是不是自己的 pid？计数器是不是正好 300？
 *   5. 打印 "p0 ok" / "p1 ok"（最多 6 字节，一次 write，保证不被打断拼接）；
 *   6. exit(9) 表示全部通过，exit(10) 表示有校验失败。
 *
 * 为什么"计数器 == 300"是关键证据：
 * 如果两个进程共用一张页表，它们会往同一个物理页上累加，各加 300 次之后
 * 读到的会是 600 左右——于是两个进程都会报 bad。所以这条检查同时证明了
 * "各自有页"和"切换时 CR3 真的换了"。
 *
 * 约束：只用 RIP 相对寻址（段要能被搬到任意基址）+ 一个绝对地址 0x600000
 * （它由 ELF 程序头声明，M6 的加载器按 p_vaddr 原样映射）。
 */

.section .text
.globl _start
_start:
    /* pid = getpid() */
    movl $4, %eax
    int $0x80
    movq %rax, %rbx                 /* rbx = pid */

    movabsq $0x600000, %r13         /* 私有数据页 */

    /* 页首写自己的 pid，偏移 8 清计数器 */
    movq %rbx, 0(%r13)
    movq $0, 8(%r13)

    /* 忙等：必须**明显超过一个时间片**（10 ms），
       否则两份进程各自在一个片内跑完，内核根本来不及在它们之间切换，
       "并发"就成了一句空话。40M 次 ≈ 25 ms，稳跨 2~3 个片。 */
    movl $40000000, %ecx
1:  decl %ecx
    jnz 1b

    /* 在同一个虚拟地址上自增 300 次 */
    xorl %r14d, %r14d
2:  incq 8(%r13)
    incl %r14d
    cmpl $300, %r14d
    jb 2b

    /* 校验：页里还是自己的 pid、计数正好 300 */
    xorl %r12d, %r12d               /* r12 = ok */
    cmpq %rbx, 0(%r13)
    jne 3f
    cmpq $300, 8(%r13)
    jne 3f
    movl $1, %r12d
3:
    /* 选消息：p0/p1 × ok/bad，长度都是 6 */
    testb $1, %bl
    jnz 4f
    leaq msg_p0_ok(%rip), %rsi
    leaq msg_p0_bad(%rip), %rdi
    jmp 5f
4:  leaq msg_p1_ok(%rip), %rsi
    leaq msg_p1_bad(%rip), %rdi
5:  testl %r12d, %r12d
    cmovzq %rdi, %rsi               /* ok==0 → 用 bad 那条 */
    movl $1, %edi                   /* fd = 1 */
    movl $6, %edx                   /* 一次 write 6 字节，输出不会和其它进程拼在一起 */
    movl $1, %eax
    int $0x80

    /* exit(ok ? 9 : 10) */
    movl $10, %edi
    testl %r12d, %r12d
    jz 6f
    movl $9, %edi
6:  movl $2, %eax
    int $0x80

7:  jmp 7b

msg_p0_ok:  .ascii "p0 ok\n"
msg_p0_bad: .ascii "p0 bd\n"
msg_p1_ok:  .ascii "p1 ok\n"
msg_p1_bad: .ascii "p1 bd\n"
