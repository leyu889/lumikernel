/*
 * 磁盘上的用户程序（M5）
 *
 * 和 M3 的 `user.S` 的区别：这一个**不随内核链接**，而是被编成独立的 ELF64
 * 文件放进 ext2 镜像里，由内核在运行时从磁盘读出来再加载。
 *
 * 它做的事：
 *   1. write(1, "hello from ext2 elf\n")        —— 证明自己在 ring3 跑起来了
 *   2. open("/hello.txt")                        —— 证明 VFS + ext2 能查目录
 *   3. read(fd, buf, 128)                        —— 证明能读文件内容
 *   4. write(1, buf, n)                          —— 把文件内容打出来
 *   5. close(fd); exit(9)                        —— 退出码 9 = 全流程成功
 *
 * 硬约束：**只用 RIP 相对寻址**。内核加载器会把这个段整体搬到自己的用户基址
 * （不一定是 0x400000），任何绝对地址在搬运后都会失效。
 * 失败时 exit(10)，让内核能区分"跑完了"和"跑一半出错"。
 */

.section .text
.globl _start
_start:
    /* write(1, msg, len) */
    lea msg(%rip), %rsi
    movl $1, %edi
    movl $(msg_end - msg), %edx
    movl $1, %eax
    int $0x80

    /* open("/hello.txt", 11) */
    lea path(%rip), %rdi
    movl $(path_end - path), %esi
    movl $5, %eax
    int $0x80
    movslq %eax, %rbx
    testq %rbx, %rbx
    js fail

    /* 缓冲区放在**用户栈**上：代码段是 R+X（不可写），
       把 buf 写在 .text 里的话 read() 会撞上写保护 —— 这正说明 W^X 是生效的。 */
    movq %rsp, %r13
    subq $128, %r13
    andq $-16, %r13

    /* read(fd, buf, 128) */
    movl %ebx, %edi
    movq %r13, %rsi
    movl $128, %edx
    movl $6, %eax
    int $0x80
    movslq %eax, %r12
    testq %r12, %r12
    jle fail

    /* write(1, buf, n) */
    movl $1, %edi
    movq %r13, %rsi
    movl %r12d, %edx
    movl $1, %eax
    int $0x80

    /* close(fd) */
    movl %ebx, %edi
    movl $7, %eax
    int $0x80

    /* exit(9)：全流程走完 */
    movl $9, %edi
    movl $2, %eax
    int $0x80

fail:
    movl $10, %edi
    movl $2, %eax
    int $0x80

1:  jmp 1b

msg:
    .ascii "hello from ext2 elf\n"
msg_end:

path:
    .ascii "/hello.txt"
path_end:

