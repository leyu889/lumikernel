//! 跨内核 / 跨操作系统的微基准（M11）
//!
//! 目标只有一个：**同一份二进制、同一台物理机、同一层虚拟化**，在 LumiKernel
//! 和 Linux 上跑同一段代码，把数字放在一起比。所以这里：
//!
//! - 不用 libc（`no_std` + 裸 `syscall`），两个系统都没有"libc 实现差异"这个变量；
//! - 不用 `clock_gettime`（我们的内核只有最粗的实现），统一用 `rdtsc`：
//!   两个系统跑在同一颗物理 CPU 上，TSC 是**同一个晶振**，周期数可直接比；
//! - 只挑两边都实现了的系统调用：`getpid / write / openat / read / close /
//!   mmap / munmap / fork / wait4 / pipe / exit_group`。
//!
//! 用法：`bench <读测试用的文件> <开关测试用的文件>`
//!   例：LumiKernel `bench /bin/busybox /hello.txt`
//!       Linux      `bench /bin/sh /etc/hostname`
#![no_std]
#![no_main]

use core::arch::asm;

/* ==================== 系统调用 ==================== */

#[inline(always)]
fn sys6(n: u64, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> i64 {
    let ret: i64;
    // SAFETY: `syscall` 只读写约定的寄存器；rcx/r11 由 CPU 破坏，这里显式声明。
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n as i64 => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            in("r10") d,
            in("r8") e,
            in("r9") f,
            out("rcx") _,
            out("r11") _,
            options(nostack)
        );
    }
    ret
}

const NR_WRITE: u64 = 1;
const NR_CLOSE: u64 = 3;
const NR_MMAP: u64 = 9;
const NR_MUNMAP: u64 = 11;
const NR_PIPE: u64 = 22;
const NR_GETPID: u64 = 39;
const NR_FORK: u64 = 57;
const NR_EXIT_GROUP: u64 = 231;
const NR_OPENAT: u64 = 257;
const NR_READ: u64 = 0;
const NR_WAIT4: u64 = 61;
const NR_LSEEK: u64 = 8;

const AT_FDCWD: u64 = (-100i64) as u64;
const O_RDONLY: u64 = 0;
const O_WRONLY: u64 = 1;
const PROT_RW: u64 = 3;
const MAP_PRIVATE_ANON: u64 = 0x02 | 0x20;

fn write(fd: u64, p: &[u8]) -> i64 {
    sys6(NR_WRITE, fd, p.as_ptr() as u64, p.len() as u64, 0, 0, 0)
}
fn openat(path: &[u8], flags: u64) -> i64 {
    sys6(NR_OPENAT, AT_FDCWD, path.as_ptr() as u64, flags, 0, 0, 0)
}
fn read(fd: u64, p: &mut [u8]) -> i64 {
    sys6(NR_READ, fd, p.as_mut_ptr() as u64, p.len() as u64, 0, 0, 0)
}
fn lseek0(fd: u64) -> i64 {
    sys6(NR_LSEEK, fd, 0, 0, 0, 0, 0)
}
fn close(fd: u64) -> i64 {
    sys6(NR_CLOSE, fd, 0, 0, 0, 0, 0)
}
fn getpid() -> i64 {
    sys6(NR_GETPID, 0, 0, 0, 0, 0, 0)
}
fn mmap(len: u64) -> i64 {
    sys6(NR_MMAP, 0, len, PROT_RW, MAP_PRIVATE_ANON, u64::MAX, 0)
}
fn munmap(addr: u64, len: u64) -> i64 {
    sys6(NR_MUNMAP, addr, len, 0, 0, 0, 0)
}
fn fork() -> i64 {
    sys6(NR_FORK, 0, 0, 0, 0, 0, 0)
}
fn wait4(pid: i64) -> i64 {
    sys6(NR_WAIT4, pid as u64, 0, 0, 0, 0, 0)
}
fn exit_group(code: u64) -> ! {
    sys6(NR_EXIT_GROUP, code, 0, 0, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

/* ==================== 计时与输出 ==================== */

#[inline(always)]
fn tsc() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: rdtsc 只读计数器；lfence 防止它被乱序到测量区间之前。
    unsafe {
        asm!("lfence", "rdtsc", out("eax") lo, out("edx") hi, options(nostack, preserves_flags))
    };
    ((hi as u64) << 32) | lo as u64
}

/// 输出一行：`<name> <value>`。
fn emit(name: &[u8], value: u64) {
    write(1, name);
    write(1, b" ");
    let mut buf = [0u8; 24];
    let n = fmt_u64(&mut buf, value);
    write(1, &buf[..n]);
    write(1, b"\n");
}

fn fmt_u64(buf: &mut [u8; 24], mut v: u64) -> usize {
    if v == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 24];
    let mut n = 0;
    while v > 0 {
        tmp[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    for i in 0..n {
        buf[i] = tmp[n - 1 - i];
    }
    n
}

fn cstr_len(p: *const u8) -> usize {
    let mut n = 0;
    // SAFETY: 调用方保证是 NUL 结尾的字符串。
    unsafe {
        while *p.add(n) != 0 {
            n += 1;
        }
    }
    n
}

/* ==================== 各项基准 ==================== */

fn bench_getpid(n: u64) -> u64 {
    let t0 = tsc();
    // min/max：平均值被"偶发 tick 打断"污染时看不出方向——
    // min 是纯 syscall 成本，max 与 min 的差就是调度/中断开销。
    let mut mn = u64::MAX;
    let mut mx = 0u64;
    let mut acc = 0i64;
    let mut prev = tsc();
    for _ in 0..n {
        acc = acc.wrapping_add(getpid());
        let now = tsc();
        let d = now - prev;
        prev = now;
        if d < mn {
            mn = d;
        }
        if d > mx {
            mx = d;
        }
    }
    emit(b"getpid_min", mn);
    emit(b"getpid_max", mx);
    core::hint::black_box(acc);
    (tsc() - t0) / n
}

fn bench_write_null(path: *const u8, n: u64) -> u64 {
    let fd = openat(unsafe { core::slice::from_raw_parts(path, cstr_len(path)) }, O_WRONLY);
    if fd < 0 {
        return 0;
    }
    let buf = [0x5Au8; 64];
    let t0 = tsc();
    for _ in 0..n {
        write(fd as u64, &buf);
    }
    let t1 = tsc();
    close(fd as u64);
    (t1 - t0) / n
}

fn bench_open_close(path: *const u8, n: u64) -> u64 {
    let p = unsafe { core::slice::from_raw_parts(path, cstr_len(path)) };
    let t0 = tsc();
    for _ in 0..n {
        let fd = openat(p, O_RDONLY);
        if fd >= 0 {
            close(fd as u64);
        }
    }
    let t1 = tsc();
    (t1 - t0) / n
}

fn bench_read(path: *const u8, rounds: u64) -> (u64, u64) {
    let p = unsafe { core::slice::from_raw_parts(path, cstr_len(path)) };
    let fd = openat(p, O_RDONLY);
    if fd < 0 {
        return (0, 0);
    }
    let mut buf = [0u8; 4096];
    // 先读一遍把文件读进各自的缓存，再计时——这里比的是"热读开销"，
    // 不是冷盘 IO（冷读在两边都受虚拟磁盘影响，变量太多）。
    let mut total = 0u64;
    loop {
        let n = read(fd as u64, &mut buf);
        if n <= 0 {
            break;
        }
        total += n as u64;
    }
    let mut ops = 0u64;
    let t0 = tsc();
    for _ in 0..rounds {
        // 每轮先回到文件开头：否则读到 EOF 就退出了。`lseek` 两边都是常数级，
        // 计进"一次 4 KiB 热读"的代价里可以接受（报告里会注明）。
        lseek0(fd as u64);
        let n = read(fd as u64, &mut buf);
        if n <= 0 {
            break;
        }
        ops += 1;
    }
    let t1 = tsc();
    close(fd as u64);
    (if ops > 0 { (t1 - t0) / ops } else { 0 }, total)
}

fn bench_mmap(n: u64) -> u64 {
    let t0 = tsc();
    for _ in 0..n {
        let a = mmap(65536);
        if a >= 0 {
            munmap(a as u64, 65536);
        }
    }
    let t1 = tsc();
    (t1 - t0) / n
}

fn bench_fork_wait(n: u64) -> u64 {
    // 同时报 min/max：平均值被"偶发多等一个 tick"污染时看不出根因，
    // min 代表真实开销，max 与 min 的差距就是调度延迟。
    let mut mn = u64::MAX;
    let mut mx = 0u64;
    let mut total = 0u64;
    for _ in 0..n {
        let t0 = tsc();
        let pid = fork();
        if pid == 0 {
            exit_group(0);
        }
        if pid > 0 {
            wait4(pid);
        }
        let d = tsc() - t0;
        total += d;
        if d < mn {
            mn = d;
        }
        if d > mx {
            mx = d;
        }
    }
    emit(b"fork_min", mn);
    emit(b"fork_max", mx);
    total / n
}

fn bench_pipe_rtt(n: u64) -> u64 {
    let mut p2c = [0i32; 2];
    let mut c2p = [0i32; 2];
    if sys6(NR_PIPE, p2c.as_mut_ptr() as u64, 0, 0, 0, 0, 0) != 0 {
        return 0;
    }
    if sys6(NR_PIPE, c2p.as_mut_ptr() as u64, 0, 0, 0, 0, 0) != 0 {
        return 0;
    }
    let pid = fork();
    if pid == 0 {
        // 子进程：先关掉自己用不到的两端。
        // 不关的话，父进程关写端之后子进程仍持有那份写端副本，
        // 管道永远不会 EOF —— 父进程在 `wait4` 上永久等待。
        // （在 LumiKernel 上没暴露是因为它的引用计数只按"当前表里的 fd"算，
        //   这份差异本身就是两边语义不同的证据。）
        close(c2p[0] as u64);
        close(p2c[1] as u64);
        // 子进程：读一个字节，回一个字节
        let mut b = [0u8; 1];
        loop {
            let r = read(p2c[0] as u64, &mut b);
            if r <= 0 {
                exit_group(0);
            }
            write(c2p[1] as u64, &b);
        }
    }
    // 父进程也关掉自己用不到的两端（对称）
    close(c2p[0] as u64);
    close(p2c[1] as u64);
    let one = [0x41u8; 1];
    let mut inb = [0u8; 1];
    let t0 = tsc();
    for _ in 0..n {
        write(p2c[1] as u64, &one);
        read(c2p[0] as u64, &mut inb);
    }
    let t1 = tsc();
    // 收尾：关掉父进程剩下的两端，子进程读到 EOF 后退出
    close(c2p[1] as u64);
    wait4(pid);
    (t1 - t0) / n
}

/* ==================== 入口 ==================== */

#[no_mangle]
pub extern "C" fn bench_main(argc: u64, argv: *const *const u8) -> u64 {
    let mut read_file: *const u8 = b"/bin/busybox\0".as_ptr();
    let mut null_path: *const u8 = b"/dev/null\0".as_ptr();
    let mut open_file: *const u8 = b"/hello.txt\0".as_ptr();
    if argc >= 2 {
        // SAFETY: 内核按 SysV 布局铺的初始栈，argv[0..argc] 都是合法指针。
        read_file = unsafe { *argv.add(1) };
    }
    if argc >= 3 {
        open_file = unsafe { *argv.add(2) };
    }
    if argc >= 4 {
        null_path = unsafe { *argv.add(3) };
    }

    emit(b"# bench (rdtsc cycles/op)", 0);
    emit(b"getpid", bench_getpid(200_000));
    emit(b"write_null", bench_write_null(null_path, 100_000));
    emit(b"open_close", bench_open_close(open_file, 20_000));
    let (rc, bytes) = bench_read(read_file, 2_000);
    emit(b"read_4k", rc);
    emit(b"read_total_bytes", bytes);
    emit(b"mmap_unmap_64k", bench_mmap(20_000));
    emit(b"fork_wait", bench_fork_wait(200));
    emit(b"pipe_rtt", bench_pipe_rtt(5_000));
    emit(b"# done", 0);
    0
}

core::arch::global_asm!(
    r#"
    .section .text.start,"ax",@progbits
    .global _start
_start:
    xor %rbp, %rbp
    mov (%rsp), %rdi          /* argc */
    lea 8(%rsp), %rsi         /* argv */
    and $-16, %rsp
    call bench_main
    mov %rax, %rdi
    mov $231, %rax            /* exit_group */
    syscall
    hlt
"#,
    options(att_syntax)
);

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    exit_group(70)
}
