M9 的验收对象：busybox 1.35.0（x86_64, musl 静态）

来源：https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox
许可：GPLv2（busybox 项目）
用途：仅作为"跑别人的程序"这一验收项的测试素材放进 ext2 系统盘，
     内核本身不链接、不包含它的任何代码。

为什么必须是静态 musl：
- x86_64 Linux 的用户态程序用 `syscall` 指令进内核（不是 int 0x80），
  这正是 M9 要实现的通道；
- 静态链接意味着不需要动态装载器与 libc.so，镜像里放一个文件就能跑；
- 1.1 MiB / 1105 个块，正好把 ext2 的二级间接块路径也走一遍。
