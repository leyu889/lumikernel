# LumiKernel · 凝光内核

> **English** — A from-scratch x86_64 monolithic kernel in Rust. Own boot chain
> (BIOS + UEFI), own AHCI/SATA driver, own journaled filesystem, own FAT16/32 reader,
> and a Linux syscall subset that runs static musl binaries (`busybox sh`).
> **61 kernel self-checks · 300+ host unit tests · fault injection, fuzz, soak,
> driver-failure isolation, W^X audit** — all reproducible from one command.
>
> **中文** — 从引导扇区开始、一行 Linux 代码都不继承的 x86_64 宏内核。
> 驱动、文件系统、协议栈全在内核态；目标是**在受支持范围内比 Linux 更不容易崩，并且崩了能定位**。
> 文档与代码注释均为中文，注释解释*为什么*这样写，而不是复述代码在做什么。

> 完整取舍见 [docs/00-定位与稳定性哲学.md](docs/00-定位与稳定性哲学.md)，
> 架构见 [docs/01-架构.md](docs/01-架构.md)，里程碑见 [docs/02-路线图.md](docs/02-路线图.md)，
> 十人分工见 [docs/03-十个工作包.md](docs/03-十个工作包.md)，规矩与坑见 [docs/04-工程规约.md](docs/04-工程规约.md)，
> **规模对标（为什么行数比不上 Linux 不是缺陷）见 [docs/08-规模对标.md](docs/08-规模对标.md)**。

## 当前状态

| 项 | 状态 |
|---|---|
| 工具链（Rust 1.99.1 + `x86_64-unknown-none` / `x86_64-unknown-uefi`） | ✅ 可用（自带一份离线工具链；也可以只装 rustup） |
| 自建引导链（stage1 → stage2 → 长模式 → 0x100000） | ✅ 已跑通 |
| **M0 地基**：64 位入口、SSE、清 BSS、串口 + VGA、E820 | ✅ 4/4 |
| **M1 内存**：高半区、帧分配器、内核堆、页表对象 | ✅ 8/8 |
| **M2 中断**：IDT、异常现场、PIC/PIT、guard page、slab | ✅ 14/14 |
| **M3 任务**：GDT/TSS/IST、ring3、`int 0x80`、内核线程、抢占 | ✅ 20/20 |
| **M4 多核**：ACPI/MADT、LAPIC/IOAPIC、APIC 定时器（RTC 校准）、AP 跳板与唤醒、IPI 广播 | ✅ **26/26**（2026-11-10，4 vCPU，连跑 4 次稳定） |
| **M5 文件与程序**：ATA PIO 驱动、ext2 挂载、fd 表、`open/read/close`、ELF64 装载 | ✅ **31/31**（2026-11-11，连跑 3 次稳定；单核 27/27） |
| **M6 进程与地址空间**：per-process 页表、CR3 切换、同一 ELF 跑两份、越界 ELF 拒绝 | ✅ **34/34**（2026-11-11，连跑 3 次；单核 30/30） |
| **M7 阻塞与进程资源**：等待队列（真 Blocked）、管道、每进程 fd 表、退出页回收 | ✅ **38/38**（2026-11-11，连跑 3 次；单核 34/34） |
| **M8 存储与设备**：块缓存、可写 LumiFS（redo 日志）、ATA 写 + FLUSH CACHE、PCI 枚举 | ✅ **45/45**（2026-11-12，连跑 3 次；单核 41/41） |
| **M9 Linux 兼容层**：`syscall` 指令通道、Linux 系统调用子集、musl 静态程序、VFS、`fork/execve/wait4` | ✅ **51/51**（2026-11-12，连跑 3 次；`busybox sh` 跑通 `echo`/`ls`/`cat`） |
| **M10 真机**：UEFI 装载器（PE32+）、GPT+ESP 启动盘、自有 **AHCI(SATA)** 驱动、同盘三分区 | ✅ **51/51**（2026-11-16，UEFI+SATA 单盘连跑 3 次） |
| **M11 稳定工程**：故障注入（帧/堆/磁盘）、系统调用与镜像 fuzz、长跑回归、跨系统微基准、S1–S7 判定、栈金丝雀 | ✅ **58/58**（2026-11-16，UEFI+SATA 连跑 3 次；S1–S7 全绿） |
| **M12 性能与安全**：fork COW、mmap 懒分配、wait 立即唤醒、网络协议栈（knet）、virtio-net/e1000 驱动、FAT16/32、**驱动故障隔离**、SMEP/W^X | ✅ **61/61**（2026-11-20） |
| crates 单测（14 个包，含 `klinux`/`kqa`/`lumifs`/`kbcache`） | ✅ **281 个全绿** |

证据：`docs/证据/M0~M11-串口日志.txt` + 屏幕截图 + 崩溃演练（`docs/05-故障演练.md`）。

### M12 做到了什么（性能对齐 + 故障隔离 + 安全）

```
[ OK ] driver:  降级→拒绝→恢复 ok，拒绝 1 次，恢复后内容一致
[ OK ] security: SMEP=0（cap=0）W^X 违规=0（审计 1540 页）
[ OK ] fat:    挂载成功，根目录 1 项，文件内容一致
M12 self-test 61/61 passed
```

| 做了什么 | 数据 |
|---|---|
| **fork 写时复制** | fork 真实开销 **12 µs**（M11 整页复制 ~90 µs）；289 页里 15 页 COW、274 页只读共享、0 页复制 |
| **mmap 懒分配** | 64 KiB mmap+munmap：**171 万 → 806 周期**（同一台机上 Linux 是 8426） |
| **wait4 立即唤醒** | `wait_halts` 195 → **0**；fork+wait 从 61M 降到 **51 K 周期** |
| **网络协议栈**（新 crate `knet`） | 以太网 / ARP / IPv4 / ICMP / UDP / TCP，**13 个宿主单测**；`knet::checksum` 对得上 RFC 1071 的样例 |
| **virtio-net 驱动** | legacy 0.9.5 队列（desc/avail/used），QEMU/KVM/VBox 通用；e1000 作为真机兜底 |
| **FAT16/32 只读驱动**（新 crate `kfat`） | 真实 U 盘格式；长文件名、变长簇链、**带防环上限**；5 个宿主单测 |
| **驱动故障隔离** | 注入"控制器失联"→ 立刻 `Err`（不等超时）+ 状态 `Down` + 后续调用被健康门快速拒绝 + `recover` 后同一块盘内容逐字节一致 |
| **安全** | SMEP 代码就位（VBox 不透传 CPUID，真机生效）；**W^X 审计 1540 页、0 违规**；SMAP 能力已探测，等所有用户访问统一走 `uaccess` 后再开 |

#### M12 抓到的真 bug（挑最值钱的三类）

1. **调度槽位泄漏**：`block_current` / `exit_current` / `wake` 三处用 `dequeue`
   摘运行态任务——运行态不在队列里，`dequeue` 静默失败，槽位带着 `RUNNING`
   永远不回 free list。泄漏到上限后 `spawn` 全部返回 `Duplicate`，
   **内核线程从此再没被调度过**（表现为三个自检连锁失败）。
   修法是新增 `ksched::detach()`，三处统一。
2. **`me()` 进了 syscall 热路径**：`current_index()` 每次都读 APIC MMIO
   （VBox 里一次 = 一个 VM exit），syscall 从 408 周期涨到 **44.7 K（110 倍）**。
   单核期间索引恒 0，改成直接读 `CUR[0]`。
3. **virtio used 环对齐**：队列只分配一页，而 used 环按规范要 4096 对齐——
   它落在页外，读回来是 `0xFFFFFFFF`，设备写的完成项一个都看不到。

#### 诚实的边界

| 项 | 现状 |
|---|---|
| SMP 多核调度 | 4/8 个 AP 全部上线、per-CPU `CUR`/调度器多队列/跨核锁已就位；**真并行调度未启用**——8 核并发在 execve 密集路径静默卡死，根因是全局 TSS（多核要 per-CPU TSS+GDT）。AP 目前是空转 idle，`parallel` 自检证明它们真的在并行执行 |
| 网络栈实机验证 | 协议栈与驱动代码完整、宿主单测全绿；VBox 的 virtio 环兼容性还没跑通（设备不消费 avail），**ping 未过**。这一项没有"过关"之前不写成支持 |
| ext2 写入 | **没做**。可写能力由 LumiFS（日志式、带崩溃恢复）覆盖；ext2 保持只读是设计选择——改系统盘的风险远大于收益。FAT16/32 也是只读 |
| SMEP | 代码就位但 VBox 不透传 `CPUID.07H:EBX[7]`，`cap=0`；真机上应能开启 |
| SMAP | 只探测不启用：内核里还有几处直接访问用户内存的历史路径，没全部收进 `uaccess` 之前开它是负收益 |

### M11 做到了什么（把"稳"变成可数的东西）

```
[ OK ] fault.frame: 200/200 返回 None，恢复后帧数 248098 -> 248098，系统可服务
[ OK ] fault.heap: try_reserve 拿到 Err，解除后成功，堆审计通过
[ OK ] fault.disk: 读失败被上传为 Err，恢复后 76 字节逐字节一致
[ OK ] fuzz: syscall×20000 path×5000 无 panic，frames 248098 fd 0->0 tasks 10
[ OK ] soak: 64 采样 × 500 轮，帧斜率 0‰ 堆斜率 0‰ 首尾差 0
[ OK ] perf: frame=41ns heap=63ns read=19MiB/s lumifs=12.5ms exec=0.35ms pipe1k=184ns
[ OK ] stack: 高水位 65 KiB / 256 KiB (25%), 金丝雀完好
M12 self-test 61/61 passed
```

这一版**没有加任何新功能**，只做一件事：把 `docs/00-定位与稳定性哲学.md` 里的
S1–S7 从"目标"变成"可以一键判定的结论"。

- **`crates/kqa`**（47 个单测）：确定性随机（PCG32）、故障注入计划（纯状态机）、
  长跑回归（整数最小二乘）、fuzz 输入生成（26 个边界值 + 6 种变异）。
  全部是纯逻辑，宿主机可测——**修一个判定 bug 不需要起虚拟机**。
- **故障注入真接在分配路径上**：帧分配、内核堆、块设备读写各一个注入点，
  用原子量判定（不加锁、不分配，OOM 现场也能安全调用）。
  自检里连续注入 200 次，每一次都必须让调用方拿到 `Err`/`None`。
- **fuzz**：2 万次随机系统调用参数（页边界 / 用户上界 / 内核地址 / `-1`）
  + 5000 条畸形路径，外加宿主端的镜像 fuzz（`kfs` 800 个变异体、`kelf` 3000 个随机镜像）。
- **长跑**：两种跑法，同一套判定。
  ① 每次启动都跑的加速版（64 采样 × 500 轮）；
  ② `LUMI_SOAK=1` 构建出的**长跑模式**——自检全绿后进无限循环，
     混合负载（帧/堆/文件/管道/目录/fork+exec/LumiFS 写）跑到底，
     每 10 分钟采一次样，可用 `scripts/soak30.sh --minutes 43200` 无人值守跑满 30 天。
  实测：连续 **1812 秒 / 3.5 亿轮 / 63 亿次操作**，空闲帧全程恒等
  `248098 → 248098`，1812 个采样点一个没抖。
  同时给了**负对照**（故意每 5000 轮漏一帧 → 127 秒内被判 `drift`），
  证明"零漂移"不是检测器瞎了。详见 [`docs/07-长跑报告.md`](docs/07-长跑报告.md)。
- **跨系统微基准**：`bench` 是一个 6928 字节、零 libc 的裸 ELF，
  **同一个文件**在 LumiKernel 和 Alpine Linux 上都能跑，结果见
  [`docs/06-性能对比.md`](docs/06-性能对比.md)。

#### M11 抓到的真 bug（这才是这一版的价值）

跑 `bench` 的 2 万次 `mmap+munmap` 时内核页错误。顺着崩溃转储查下去：
**boot 栈（64 KiB）溢出 1.2 KiB，把紧邻其下的 `pd_1g`（直接映射页表）踩了**，
第 357 项被写成一个内核返回地址，于是 714 MiB 处的直接映射静默失效。

修法三件事：栈提到 256 KiB、栈与页表之间插 4 KiB 金丝雀页、新增自检项 `stack`
打印**实测高水位**（65 KiB / 256 KiB —— 也就是原来正好差 1.2 KiB 就溢出）。
完整过程写在 [`docs/05-故障演练.md`](docs/05-故障演练.md)，从拿到转储到指出根因约 14 分钟。

#### 诚实的边界

| 限制 | 现状 |
|---|---|
| 30 天日历时间 | 长跑模式与 `scripts/soak30.sh` 都已就绪（`--minutes 43200` 即 30 天），但**本轮只连续跑了 1812 秒（guest 计时）**。日历时间压不动：报告给的是"工作量等效"（63 亿次操作 ≈ 30 天里每秒 2434 次），不做日历等效的声称 |
| fuzz 24 小时 | **没做**。做的是 2.5 万次内核侧 + 3000 次宿主侧，全部零 panic；脚本已具备拉长时间的能力 |
| 同配置对比 | vCPU 数不同（4 vs 2），且 `mmap`/`fork` 两项是策略差异（懒分配 / COW），不能直接当"快慢"读 |
| unsafe 逐条证明 | 密度 1.53%（SLO ≤5%），303 个 unsafe 行里 43 处的证明位于文件顶部的模块级说明，未就地标注 |

### M10 做到了什么（在 UEFI 固件 + SATA 盘上自己启动）

```
[LumiEFI] UEFI loader up
[LumiEFI] kernel blob found
[LumiEFI] kernel copied to 0x100000
ACSK
[ OK ] e820:  entries=19 usable=975MiB boot=uefi
       AHCI 控制器就位：ABAR=0x81404000，1 个端口在线（系统盘 131072 扇区）
[ OK ] disk: sectors=131072 blocks=65536KiB transport=ahci magic=0xef53
[ OK ] busybox: sh/echo=1 ls=1 cat=1 exit=0 bytes=98
M10 self-test 51/51 passed
```

这一条跑的是**一块盘 + UEFI 固件**，也就是真机插 U 盘的样子：

- **UEFI 启动链**：`tools/mkfat.py` 造 FAT16 → `tools/mkgpt.py` 包成 GPT 磁盘（ESP 分区）
  → 固件按标准路径 `\EFI\BOOT\BOOTX64.EFI` 加载我们的装载器。
  装载器（`boot/efi/`，Rust + `x86_64-unknown-uefi` 目标，产物是 PE32+）做四件事：
  把嵌在自己映像里的内核拷到物理 1 MiB、把 UEFI 内存描述符翻成内核认识的 E820、
  `ExitBootServices`、`iretq` 进内核。
- **自有 AHCI(SATA) 驱动**：真机没有 IDE 的 0x1F0 端口。`dev/ahci.rs` 找控制器、
  映射 ABAR（MMIO 关 cache）、复位 HBA、逐端口起命令引擎、`IDENTIFY` 拿容量，
  之后所有读写都走 **`READ/WRITE DMA EXT` + PRDT**，`FLUSH CACHE EXT` 作为日志屏障。
  自检里的 `transport=ahci` 就是它接管成功的证据（同一份代码在 BIOS 盘上是 `transport=pio`）。
- **同一块盘装三样东西**：ESP（装载器）+ ext2（`/bin/busybox`）+ LumiFS（可写数据）。
  装载器把 ext2 分区的起始 LBA 写在 E820 表头的空档里，内核据此加偏移——
  这样"插一块 U 盘就能到 shell"，不需要第二块盘。
- **BIOS 那条链没被牺牲**：同一张 ISO 仍然能用老的 stage1/stage2 引导
  （`boot=bios`、`transport=pio`、51/51）。真机走 UEFI，老机器/虚拟机走 BIOS，
  两条路都留证据。

> **证据说明**：VBox 的 EFI 固件把控制台重定向到串口，所以 UEFI 那条路的
> 屏幕截图只到装载器阶段（`M10-屏幕-uefi装载器.png`），内核自检与 shell 输出
> 全在 `docs/证据/M10-串口日志-uefi-sata.txt` 里。BIOS 那条路的截图
> （`M10-屏幕-内核自检.png`）能直接看到屏幕上的自检结果。

#### 交付物

| 文件 | 用途 |
|---|---|
| `out/LumiKernel-1.1-M10.iso` | 光盘/虚拟机用：**BIOS + UEFI 双引导**（El Torito 里放了 FAT 引导映像） |
| `out/LumiKernel-1.1-M10-efi.img` | **U 盘用**：GPT + ESP + ext2 + LumiFS，一块盘自包含 |
| `out/WRITE_USB.cmd` | 双击写盘（只允许写 USB 总线上的可移动盘，需管理员） |

### M9 做到了什么（别人的程序真的跑起来了）

```
[ OK ] vfs:  root=dir hello=76B dev_null=chr entries=4 open_dirs=0
LUMI-M9
bin
hello.txt
this line came from an ext2 file on disk
second line: 0123456789 abcdefghij
[ OK ] busybox: sh/echo=1 ls=1 cat=1 exit=0 bytes=98
[ OK ] linux-abi: syscall_insns=70 from_cpl3=70
[ OK ] reap: frames 129297 -> 129297 zombies=9 double_free=0
M9 self-test 51/51 passed
```

跑的是**别人编译好的二进制**：busybox 1.35.0（x86_64 musl 静态），
命令是 `sh -c 'echo LUMI-M9; ls /; cat /hello.txt'`。这一句把整条链路都串起来了：

- `sh` 是 musl 静态程序 → 需要 `syscall` 指令、`arch_prctl(SET_FS)` 设 TLS、
  按 SysV 布局的 `argc/argv/envp/auxv` 初始栈；
- `ls` / `cat` 不是 shell 内建 → `sh` 会 `fork` + `execve("/bin/ls")`，
  而 `/bin/ls` 是 **ext2 里的符号链接**（指向 `/bin/busybox`），
  所以还要"路径里跟符号链接 + PATH 搜索 + `wait4` 收子进程"；
- `ls` 要 `getdents64` 列目录、`lstat` 拿每个条目的类型；
  它的输出走 musl stdio 的 `writev`；
- `cat` 要 `openat/read/write`，把 ext2 上 76 字节的文件原样吐出来。

几个必须说清楚的点：

- **`syscall` 指令通道是必需的**：musl 只会用 `syscall`，不会用 `int 0x80`。
  M3 造的 `int 0x80` 保留给内核自检；两条路共用同一个 `InterruptFrame` 和
  同一个 `trap_return`，不搞两套返回路径。
- **初始栈是我自己在内核里拼的**：`argc/argv/envp/auxv` 逐项按 ABI 摆位，
  而且**摆完读回校验**。这里踩过一个很隐蔽的坑：auxv 预留的字节数写成了常量，
  实际多写一对，正好把 `argv[0]` 字符串盖成 0 —— busybox 报 `: applet not found`，
  看起来像 applet 表坏了。
- **`fork` 不是 COW**：整页复制（busybox 约 280 页）。COW 需要"缺页时判断是复制
  还是真错"的整套逻辑，M9 要的是先跑通；这条限制写在下面的"已知限制"里。
- **用户态异常只杀进程，不停内核**。`cs & 3 == 3` 的 `#PF`/`#GP` 一律按
  139（128+SIGSEGV）退出该进程，内核继续跑。跑别人的程序，这是底线。
- **可观测性**：内核留了一个 512 项的**系统调用环形轨迹**，用户态异常时把最近
  24 次调用（号/参数/返回值）打出来。M9 里两个致命 bug 都是靠它定位的
  （一个是 `mmap(MAP_FIXED)` 撞已有映射返回 ENOMEM，一个是 stdio 的 `writev` 没实现）。

#### M9 的已知限制（都是明确的取舍，不是"没做完"）

| 限制 | 后果 | 为什么先这样 |
|---|---|---|
| 没有信号投递 | 用户态异常直接按 139 退出；`rt_sigaction` 成功但不生效 | busybox 非交互场景不依赖信号 |
| `fork` 整页复制（无 COW） | fork 一个 1.1 MiB 的程序约 280 页拷贝 | 先把"能跑"做对，再谈性能 |
| 没有终端（`ioctl` 一律 `ENOTTY`） | `isatty()` 为假；`ls` 不套颜色 | 没有键盘驱动与行规程，不假装有 |
| 文件映射 `mmap` 未实现 | `mmap` 一个 fd 返回 `ENODEV` | 静态 busybox 不用它 |
| 调度仍只在 BSP 上跑 | 4 核里只有 1 个核跑任务 | 从 M4 起就写在限制里 |
| 没有 `pipe2`/`poll`/`futex` | 线程与事件循环类程序跑不了 | busybox 单线程够用 |
| 只支持 ET_EXEC | PIE（ET_DYN）仍被拒绝 | 静态 musl 默认就是 ET_EXEC |

### M8 做到了什么（可写 + 掉电不坏）

```
[ OK ] pci:  devs=6 buses=1 storage=1 bridges=0
[ OK ] lumifs: blocks=2048 free=2032 entries=0 seq=0 formatted=1
[ OK ] write: bytes=2500 readback=2500 match=1 ino=0
[ OK ] persist: remount=1 replayed=0 bytes=2500
[ OK ] journal: armed_seq=4 replayed=1 blocks=2 content=1
[ OK ] cache: hits_gained=12 dirty_after_sync=0
[ OK ] unlink: gone=1 blocks_restored=1
M8 self-test 45/45 passed
```

- **LumiFS 可写 + 带 redo 日志**：改一个文件要动数据块、inode 块、超级块；
  中途掉电就会得到"inode 说有 4 KiB、第三块还是别人的数据"这种结构性损坏。
  现在这些改动先写进日志区并提交，再落到原位；挂载时看到完整提交记录就重放。
  顺序是 **载荷 → 屏障 → 提交记录 → 屏障 → 就地写 → 屏障 → 清记录**，
  每一处 `flush()` 都是 `ATA FLUSH CACHE (0xE7)`，不是"我以为已经落盘了"。
- **掉电现场真被救回来**：自检主动把磁盘摆成"日志已提交、数据还没落地"的样子
  （`arm_crash_after_commit`），卸载再挂载 → 重放 2 块 → 文件内容是**新的**那份。
  这条不是"日志写好了"，是"日志真的被用上了"。
- **块缓存**：写回 + LRU，8 个槽位。同一块连读 5 次命中 12 次（读路径和元数据
  都省掉了 ATA 操作），`sync` 之后脏块数归 0。
- **ATA 可写**：加了从盘、`WRITE SECTORS`、`FLUSH CACHE`。系统盘（ext2）仍只读，
  数据盘（LumiFS）可写——**破坏性实验碰不到内核自己依赖的那块盘**。
- **PCI 枚举**：legacy 配置空间扫总线，读出 6 个设备（含 IDE 控制器 `8086:7111`、
  VBox 显卡 `80ee:beef`）与它们的 BAR。这是 M9 写 virtio 驱动的入口。

#### LumiFS 的格式（1 KiB 块）

```text
LBA 0        超级块（magic + 几何 + 空闲块 + 日志序号 + 校验和）
LBA 1        日志头（magic + seq + n + n 条 {目标LBA, 载荷槽} + 校验和）
LBA 2..8     日志载荷（7 个槽，单次事务最多 4 块）
LBA 9..12    inode 表（16 个/块 × 4 块 = 64 个，每个 64 B，名字存在 inode 里）
LBA 13..15   保留
LBA 16..     数据块
```

刻意的简化：扁平命名空间（没有目录）、每文件最多 8 个直接块（8 KiB）、
单次事务最多 4 块（更大的写入由调用方分多次提交，每次都是一个独立原子事务）。
换来的是一份 700 行、能在宿主机上跑完整掉电测试的文件系统。

#### 这轮踩的坑里最值钱的一个

把 `kbcache::MAX_SLOTS` 设成 64 时，`Cached` 结构体 ≈ 66 KiB。
`Cached::new()` 和 `LumiFs::mount()` 都是**按值返回**的，于是整块结构先在
64 KiB 的内核栈上构造——栈被踩穿，屏幕上冒出一串乱码然后卡死。
看起来像"文件系统写坏了"，其实是栈溢出。修法：槽位降到 8，并加一条
**编译期断言**把结构体上界钉死在 16 KiB。

### M7 做到了什么（任务会睡觉了，而且不再漏内存）

```
[ OK ] fd-table: peer_fd=3 my_fd=3 same_file_bytes=12
[ OK ] pipe: blocked=2 frozen_ticks=1 write=4 read=4 word=0x70696e67 wait_code=0
[ OK ] pipe-eof: order=1 epipe=1 eof=1 fds_closed=1
[ OK ] reap: runs=3 pages=18 tables=21 frames 129332 -> 129332
M7 self-test 38/38 passed
```

- **真阻塞**：新增 `State::Blocked`。睡下去的任务**不在运行队列里**，
  不消耗时间片、不被挑中，只有 `wake()` 能叫醒它。自检直接量了两件事：
  两个任务同时处于 `Blocked`；放 5 个 tick 过去，它们的 `ticks` 一次都没涨
  （忙等的话会一直涨——这一条把"阻塞"和"轮询"分得很干净）。
- **管道**：环形缓冲 + 双向等待队列。读空阻塞、写满阻塞、写端全关给 EOF、
  读端全关给 `EPIPE`。这四个语义都测了，而且**走 fd 层**
  （`fd::pipe_pair` → `fd::read/write`），因为系统调用正是走那条路。
- **每进程 fd 表**：fd 表从 `Task` 外面搬进 `Task` 里面。判据很硬：
  一个任务占住自己的 fd 3 之后，**我自己 open 也必须拿到 3**
  （全局表的话只能拿到 4）。
- **退出回收**：任务退出后由 `reap()` 走一遍自己的页表低半区，
  把每个用户页和每张中间页表都还给帧分配器。跑 3 个进程再回收，
  可用帧数**精确回到起点**（129332 → 129332）。
- **退出即关 fd**：进程死掉时自动关掉它的管道端点，否则对端会永远等一个
  不会来的写者。

#### 这轮修掉的一个真 bug：`yield` 是免费的

`while flag == 0 { yield_now() }` 这种空转，一开始真的把整机卡死了：
`yield_now()` 只做调度、**不记 vruntime**，于是空转任务的 vruntime 一直是 1，
调度器每次都挑它，另一个就绪任务被无限期饿死。而对方正在 `hlt` 里等它退出——
两边互相等，日志停在上一行，没有任何报错。

修法是把"让出"也记进账：`yield_now()` 里先给当前任务加一个时间片的 vruntime，
再调度。Linux 的 `update_curr()` 做的是同一件事。

### M6 做到了什么（两个进程，各有一张自己的页表）

```
[ OK ] addrspace: a=0x60c000 b=0x61d000 kern=0x125000 same_entry=0x400000
p1 ok
p0 ok
[ OK ] 2proc: exit=Some(9)/Some(9) switches=5 ticks=1/1 out="p1 ok.p0 ok."
[ OK ] elf-guard: bad_entry=err bad_seg=err
M6 self-test 34/34 passed
```

- **每进程一张 PML4**：低半区（PML4[0..255]）全空，高半区（256..511）从内核表复制。
  于是同一个虚拟地址 `0x600000` 在两个进程里指向**不同的物理页**。
- **上下文切换切 CR3**：在 `switch_to` 之前写 CR3。内核栈走直接映射，
  两张表里都有效，所以这个顺序是安全的；切完再 `iretq` 回用户态。
- **同一份 ELF 装两次**：`/bin/proc` 被装载成两个进程，页表不同、入口地址相同。
  两个进程都在 `0x600000` 上自增 300 次并读回校验——**各读到 300 而不是 600**，
  这就是隔离生效的直接证据（共用一页的话会互相累加到 600，两边都会报 bad）。
- **越界 ELF 被拒**：把入口改成 `0xFFFFFFFF80100000`、或把某个段的 `p_vaddr`
  抬到内核区，装载器都必须拒绝——否则用户程序能给自己装一页内核。

##### M5 的两处临时方案，M6 已经删掉

| M5 的临时做法 | M6 的做法 |
|---|---|
| ELF 段整体搬到固定基址 `0x4000010000` | **按 `p_vaddr` 原样加载**（低地址在进程表里是空的） |
| 用户栈固定在一个全局地址 | 栈在进程自己的表里（地址相同、物理页不同），每进程 4 页 |

仍然存在的限制（写在代码注释里）：
- **fd 表还是全局的**，不是每进程的。多进程并发读写同一个文件时，偏移会互相踩。
- **没有 readdir**：打开目录返回 `EBADF`，缺 getdents 语义。
- **进程退出的物理页不回收**：僵尸任务释放了槽位，但页表页和数据页泄漏；
  跑几百个进程就会把 512 MiB 吃光。回收要等引用计数（M7）。

#### M5 的两处临时方案，M6 已经删掉

| M5 的临时做法 | M6 的做法 |
|---|---|
| ELF 段整体搬到固定基址 `0x4000010000` | **按 `p_vaddr` 原样加载**（低地址在进程表里是空的） |
| 用户栈固定在一个全局地址 | 栈在进程自己的表里（地址相同、物理页不同），每进程 4 页 |

仍然存在的限制（写在代码注释里）：
- **fd 表还是全局的**，不是每进程的。多进程并发读写同一个文件时，偏移会互相踩。
- **没有 readdir**：打开目录返回 `EBADF`，缺 getdents 语义。
- **进程退出的物理页不回收**：僵尸任务释放了槽位，但页表页和数据页泄漏；
  跑几百个进程就会把 512 MiB 吃光。回收要等引用计数（M7）。

### M5 做到了什么（用户程序终于变成"磁盘上的一个文件"）

```
[ OK ] ata:  sectors=8192 blocks=4096KiB magic=0xef53
       根目录内容：bin hello.txt
[ OK ] ext2:  blocks=4096 root_entries=4 hello.txt=76B
[ OK ] file:  open/read/close ok bytes=76
       close 之后打开数为 0（fd 表没有泄漏）
hello from ext2 elf                      ← 下面三行都是 ring3 程序打的
this line came from an ext2 file on disk ← 后两行是从 ext2 文件里读出来的
second line: 0123456789 abcdefghij
[ OK ] elf:  segs=1 entry=0x4000010000 pages=1 mem=4KiB exit=9 calls=6 out_b=96
[ OK ] reject: missing=err badelf=err dir=err badfd=err
M5 self-test 31/31 passed
```

- **ATA PIO 驱动**：8 个端口，`IDENTIFY` 拿容量，LBA28 读扇区。
  不靠"DMA 以后再说"糊过去——小文件用 PIO 够快，先把它做对。
- **ext2 挂载**：`crates/kfs` 那个纯解析器直接接到了真实磁盘上（中间只加了一层
  `BlockDevice` 适配 + `IrqSpinlock`），两级目录 `/bin/hello` 能查。
- **文件描述符**：fd 0/1/2 留给控制台，`open` 从 3 开始；`read` 维护偏移、到 EOF 返回 0。
- **ELF64 装载**：按程序头逐段分配物理页 → 清零 → 拷文件内容 → 映射成用户页
  （代码段 R+X，数据段 R+W，**W^X 是分开的**）。
- **负路径也测**：不存在的路径、假 ELF、打开目录、关无效 fd —— 四种坏输入都必须
  返回错误而不是崩。只测成功路径的自检等于没测。

### M4 做到了什么

- **ACPI/MADT**：在 BIOS 区扫 RSDP → RSDT/XSDT → MADT，拿到 CPU 个数、
  I/O APIC 地址与 ISA 中断改写。解析器是独立 crate `kacpi`（19 个单测，纯字节切片）。
- **I/O APIC 真的在投递中断**：把 PIT 的 IRQ0 经 GSI 2 重定向到向量 32，
  实测收到 20 个 tick；随后屏蔽它，改由本地 APIC 定时器供时——**两条路径都验过**。
- **四个核全部上线**：16 位跳板 → 保护模式 → 长模式，每个 AP 拿到自己的内核栈、
  自己的 GDTR/IDTR、自己的 LAPIC 定时器。`online=4/4`，`stage=8 wakeups=1`
  （说明跳板一次走完、没有中途死掉重启）。
- **并行是真的**：200 ms 窗口里四个核同时干活，每个核**自己数自己的循环次数**
  （最少 3900 万次，BSP 3.7 亿次），不是"连着就算"。
- **每核独立时基**：四个核都被自己的 LAPIC 定时器打断，静息态实测 94 Hz（配置 100 Hz，
  VBox 的定时器粒度决定的抖动）。
- **IPI 广播可用**：向量 241 广播给 3 个 AP，3/3 应答——多核调度 reschedule 的通道打通了。
- **8259A 完全静默**：掩码 0xFF/0xFF，EOI 走 LAPIC 的 EOI 寄存器。

### M4 的坑（这次踩得最惨的一个）

上一版我判断"AP 唤醒卡在 SIPI 的 ICR 写"，**这个结论是错的**。
真相是：跳板里 `lgdt` 多带了 `0x66/0x67` 前缀，GDTR 被装成垃圾
（`sgdt` 读回 base=0x53f000ff），远跳转查描述符表时 `#GP` → 三重故障 →
VBox 复位那个核 → 它又收到待处理的 SIPI → 再跑一遍跳板 → 再死。
表现成"一串 `1\x0f` 洪水"，而我把它读成了"BSP 卡住"。

错判的直接原因：**两个核同时往同一个 16550 写**，字节互相覆盖；
而我没等发送寄存器空就连续写，还会丢字节。拿它做字节级探针，得到的是假数据。
现在跳板把阶段号写进内存（0x6000）、BSP 读回再打印，信号不再经过任何设备。

## 怎么跑

```
python build.py --ver 0.9      # 内核 + 引导链 + 系统盘(ext2) + 数据盘(全零) + ISO
bash scripts/vmtest.sh out/LumiKernel-0.9.iso 34  # VBox 无头启动（自动挂两块盘）并抓串口日志

# 崩溃转储演练（可选）：出一个会故意踩未映射页的镜像
LUMI_CRASH_DEMO=1 python build.py --ver 0.9-crash
```

`build.cmd` / `run.cmd` / `test.cmd` 是 Windows 下双击入口（QEMU 版；本机 QEMU 安装包需要管理员权限，
暂时走 VirtualBox 这条已验证的路）。

预期输出（串口，正式版，4 vCPU）：

```
  LumiKernel 0.9.0  (milestone M8)
  monolithic kernel, journaled fs + block cache
  lumifs(journaled) + bcache + pci + ata rw
[ OK ] idt / gdt / ist / e820 / frames / heap / acpi / ioapic-irq / apic-timer / pic
[ OK ] paging-test / frames-stress / exception / guard-page / slab
[ OK ] task / ring3 / kthreads / preempt / smp / parallel / smp-timer / ipi
[ OK ] ata / ext2 / file / elf / reject / addrspace / 2proc / elf-guard
[ OK ] fd-table / pipe / pipe-eof / reap
[ OK ] pci:  devs=6 buses=1 storage=1 bridges=0
[ OK ] lumifs: blocks=2048 free=2032 entries=0 seq=0 formatted=1
[ OK ] write: bytes=2500 readback=2500 match=1 ino=0
[ OK ] persist: remount=1 replayed=0 bytes=2500
[ OK ] journal: armed_seq=4 replayed=1 blocks=2 content=1
[ OK ] cache: hits_gained=12 dirty_after_sync=0
[ OK ] unlink: gone=1 blocks_restored=1
抢占禁用深度 = 0（必须为 0）
M8 self-test 45/45 passed
[LIVE] kernel healthy: 4 cpus, writable journaled fs, pci scanned
```

单核启动时 SMP 那 4 项标 `[ .. ] ... skipped`，总数变成 41/41。

### 系统盘怎么来的

`python build.py` 会自动跑 `tools/mkdisk.py`：

```
lumikernel/src/user_disk.S  --as--> user_disk.o --objcopy--> user_disk.bin
   --mkelf.py--> hello.elf（ELF64，PT_LOAD @0x400000，R+X）
   --mkext2.py--> build/disk.img（4 MiB ext2：/bin/hello + /hello.txt，自带回读校验）
```

`scripts/vmtest.sh` 会把它转成 VDI 挂到 IDE 主通道主盘——**这一步不能省**，
内核的 `ata/ext2/elf` 三项全靠它，没有盘会明确报 FAIL 而不是假装通过。

## 目录

```
kernel/
  lumikernel/      内核二进制
    src/boot.S     64 位入口 + 自建高半区页表
    src/arch/      GDT/TSS、switch_to、IrqSpinlock、ap_tramp.S + smp.rs（AP 唤醒 / 每核状态 / IPI）
    src/intr/      IDT / ACPI 发现 / LAPIC+IOAPIC / APIC 定时器 / RTC 基准 / 异常转储 / 故障探针
    src/task/      任务表 / 栈构造 / 调度 / 抢占
    src/syscall.rs int 0x80 分发（M3 起的内核自检通道）
    src/linux/     Linux 系统调用层：分发 / 文件 / 内存 / 进程 / 用户内存访问 / 调用轨迹
    src/vfs.rs     统一命名空间：/ ext2（只读）、/data LumiFS、/dev 合成设备
    src/arch/syscall.rs      syscall 指令的 MSR 配置 + 入口胶水
    src/arch/syscall_entry.S 手工拼 InterruptFrame 的汇编入口（不换栈、不压帧）
    src/user.S     ring3 示例程序（只允许 RIP 相对寻址）
    src/dev/ata.rs ATA PIO 块设备（实现 kfs::BlockDevice）+ AHCI 分发
    src/dev/ahci.rs AHCI(SATA) 驱动：HBA 复位 / 端口启动 / IDENTIFY / DMA 读写 / FLUSH
    src/fs/        ext2 挂载 + 名字查找 + 读文件 + 符号链接（IrqSpinlock 保护）
    src/fs2.rs     可写 LumiFS 挂载层（块缓存 + 首次格式化 + 掉电演练钩子）
    src/dev/pci.rs PCI 总线枚举（配置空间 + BAR + 驱动访问口）
    src/dev/ata.rs ATA PIO：主/从盘、读写扇区、FLUSH CACHE
    src/fd.rs      每进程文件描述符表（控制台/只读 ext2/管道/LumiFS 可写文件）
    src/pipe.rs    管道 + 等待队列（lost-wakeup 的原子窗口写在文件头）
    src/exec.rs    ELF64 装载：分段 → 用户页 → ring3 任务
    src/user_disk.S 放进磁盘的 ring3 程序（开/读/写/关/退出）
    src/mm/        帧分配 / 内核堆 / 页表
  boot/            自建引导链（stage1 512B / stage2 实模式→长模式）
    efi/           UEFI 装载器（Rust, x86_64-unknown-uefi → PE32+，内核以 PE 段嵌入）
  crates/          十个工作包，一个包一个 crate（纯逻辑，宿主机可单测）
    kmem/          物理帧位图分配
    kheap/         内核堆（边界标记 + 双向合并 + audit）
    kpt/           真实四级页表操作（PTE 读写 / 中间层按需分配 / 空表回收）
    kvm/           页表纯模型（随机对拍参考实现）
    ksched/        CFS 风格调度模型
    kfs/           只读 ext2 解析
    kslab/         slab 分配器（8 档 / 页头校验 / 双重释放检测 / 全量审计）
    kfault/        异常向量语义解码（错误码位解码，零分配无 panic）
    kacpi/         ACPI 表解析（RSDP / RSDT / MADT，纯字节切片）
    kelf/          ELF64 解析（只读字节切片，19 个单测）
    lumifs/        带 redo 日志的可写文件系统（20 个单测，含逐写边界掉电对拍）
    kbcache/       写回 LRU 块缓存（16 个单测，含与直通模式随机对拍）
    klinux/        Linux x86_64 ABI 纯描述层：调用号/errno/常量/stat 与 dirent64 字节布局/路径规范化（23 个单测）
  docs/            定位、架构、路线图、工作包、工程规约、证据
  tools/           自研 ISO 打包器（xasm16/mkiso/checkiso）+ 系统盘三件套
    assets/busybox 1.35.0 x86_64 musl 静态二进制（M9 的验收对象，1.1 MiB）
    mkefi.py       把内核追加成装载器的 PE 段并回填相对偏移
    mkfat.py       造 FAT16（UEFI 固件只从文件系统里找 \EFI\BOOT\BOOTX64.EFI）
    mkgpt.py       造 GPT 磁盘：ESP + ext2 + LumiFS 三个分区
                   mkelf.py（平铺码→ELF64）/ mkext2.py（造 ext2）/ mkdisk.py（串起来）
  build.py         一条命令出盘
  linker.ld        内核链接脚本（0x100000，.text.entry 在最前）
```

## 三条不妥协的规矩

1. **内核里不许 panic。** 内存不足、参数非法、设备超时，全部是正常返回值。
2. **`unsafe` 必须有证明。** 每个块上面一行 `// SAFETY:`；密度超过 5% 就重构。
3. **没有原始日志的"完成"不算完成。** 每个里程碑的串口日志留在 `docs/证据/`。
