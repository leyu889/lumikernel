/*
 * 用户态示例程序（M3）
 *
 * 这不是"演示用的假程序"：它真的在 CPL=3 下执行，通过 int 0x80 陷入内核，
 * 由内核对用户指针做校验后把字节打到串口，最后 sys_exit(7) 结束自己。
 *
 * 两条硬约束：
 * 1. **只允许 RIP 相对寻址**。这段机器码在编译期被链接到内核高半区，
 *    运行期被拷到另一个虚拟地址（USER_CODE_VA）执行；任何绝对地址在拷贝后都是错的。
 *    所以取字符串用 `leaq sym(%rip)`，取长度用汇编期常量差。
 * 2. 不许直接碰任何 I/O 端口或内核地址——它没有权限，碰了就是 #GP，
 *    内核会打印现场。M3 的自检正是靠"它跑完了"证明保护是真的生效了。
 */

.section .user, "ax"
.code64

.global __user_start
__user_start:
    /* sys_write(1, msg, len) */
    leaq __umsg(%rip), %rsi
    movl $1, %edi               /* fd = 1（串口） */
    movl $(__umsg_end - __umsg), %edx
    movl $1, %eax               /* SYS_write */
    int $0x80
    movq %rax, %r12             /* 留着返回值，退出码里带出去 */

    /* sys_exit(7)：退出码固定 7，内核据此判断"确实走到最后一行" */
    movl $7, %edi
    movl $2, %eax               /* SYS_exit */
    int $0x80

1:  jmp 1b                      /* sys_exit 不该返回；万一返回了就自旋等抢占 */

__umsg:
    .ascii "hello from ring3\n"
__umsg_end:

.global __user_end
__user_end:
