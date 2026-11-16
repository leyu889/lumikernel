//! Linux 系统调用层（M9）
//!
//! 入口有两条：`syscall` 指令（musl 程序用，`arch::syscall`）和 `int 0x80`
//! （M3 起的内核自检用，`crate::syscall`）。两条入口最后都拿到同一个
//! `InterruptFrame`，所以这里只关心"帧里的寄存器"。
//!
//! 参数约定（x86_64 Linux）：
//!
//! ```text
//! rax = 调用号   rdi rsi rdx r10 r8 r9 = 参数 1..6
//! 返回值放回 rax；失败是 -errno
//! ```
//!
//! 注意参数 4 是 `r10` 而不是 `rcx`——`syscall` 指令自己要用 `rcx` 存返回地址，
//! 所以内核 ABI 把第 4 个参数挪到了 `r10`。这在 `openat`/`newfstatat` 上最容易写错。
//!
//! 分文件：
//! - `fs`   —— 文件与目录
//! - `mem`  —— `brk` / `mmap`
//! - `proc` —— `fork` / `execve` / `wait4` / 信号占位

pub mod fs;
pub mod mem;
pub mod proc;
pub mod trace;
pub mod uaccess;

use klinux::errno as E;
use klinux::sysno as NR;

use crate::intr::frame::InterruptFrame;
use crate::task;
use crate::vfs;

/// 系统调用总入口。
pub fn dispatch(f: &mut InterruptFrame) {
    let nr = f.rax;
    let a0 = f.rdi;
    let a1 = f.rsi;
    let a2 = f.rdx;
    let a3 = f.r10;
    let a4 = f.r8;
    let a5 = f.r9;

    let ret: i64 = match nr {
        NR::READ => fs::read(a0, a1, a2),
        NR::WRITE => fs::write(a0, a1, a2),
        NR::OPEN => fs::openat(klinux::flags::AT_FDCWD, a0, a1 as u32, 0),
        NR::OPENAT => fs::openat(a0, a1, a2 as u32, a3 as u32),
        NR::CLOSE => fs::close(a0),
        NR::STAT | NR::LSTAT => fs::newfstatat(
            klinux::flags::AT_FDCWD,
            a0,
            a1,
            if nr == NR::LSTAT {
                klinux::flags::AT_SYMLINK_NOFOLLOW
            } else {
                0
            },
        ),
        NR::FSTAT => fs::fstat(a0, a1),
        NR::NEWFSTATAT => fs::newfstatat(a0, a1, a2, a3 as u32),
        NR::LSEEK => fs::lseek(a0, a1 as i64, a2 as u32),
        NR::GETDENTS64 => fs::getdents64(a0, a1, a2),
        NR::DUP => fs::dup(a0, None),
        NR::DUP2 => fs::dup(a0, Some(a1 as usize)),
        NR::WRITEV => fs::writev(a0, a1, a2),
        NR::READV => fs::readv(a0, a1, a2),
        NR::PREAD64 => fs::pread64(a0, a1, a2, a3),
        NR::PIPE => fs::pipe(a0),
        NR::ACCESS => fs::access(a0),
        NR::READLINK | NR::READLINKAT => {
            if nr == NR::READLINK {
                fs::readlink(klinux::flags::AT_FDCWD, a0, a1, a2 as usize)
            } else {
                fs::readlink(a0, a1, a2, a3 as usize)
            }
        }
        NR::CHDIR => fs::chdir(a0),
        NR::GETCWD => fs::getcwd(a0, a1 as usize),
        NR::IOCTL => fs::ioctl(a0, a1, a2),
        NR::UNLINK | NR::UNLINKAT => {
            if nr == NR::UNLINK {
                fs::unlinkat(klinux::flags::AT_FDCWD, a0, 0)
            } else {
                fs::unlinkat(a0, a1, a2 as u32)
            }
        }
        NR::FCNTL => fs::fcntl(a0, a1, a2),

        NR::MMAP => mem::mmap(a0, a1 as usize, a2 as u32, a3 as u32, a4 as i64, a5 as u64),
        NR::MUNMAP => mem::munmap(a0, a1 as usize),
        NR::MPROTECT => mem::mprotect(a0, a1 as usize, a2 as u32),
        NR::MREMAP => mem::mremap(a0, a1 as usize, a2 as usize, a3 as u32),
        NR::MADVISE => 0,
        NR::BRK => mem::brk(a0),

        NR::EXIT => task::exit_current(a0 as i32 as i64),
        NR::EXIT_GROUP => task::exit_current(a0 as i32 as i64),
        NR::FORK | NR::VFORK => proc::fork(f),
        NR::EXECVE => proc::execve(f, a0, a1, a2),
        NR::WAIT4 => proc::wait4(a0 as i64, a1, a2 as u32, a3),
        NR::GETPID => task::current_tid() as i64,
        NR::GETTID => task::current_tid() as i64,
        NR::GETPPID => task::parent_of(task::current_tid()) as i64,
        NR::KILL => 0,
        NR::SCHED_YIELD => {
            task::yield_now();
            0
        }
        NR::ARCH_PRCTL => proc::arch_prctl(a0, a1, a2),
        NR::CLOCK_GETTIME => proc::clock_gettime(a0, a1),
        NR::GETRANDOM => proc::getrandom(a0, a1 as usize),
        NR::PRLIMIT64 => proc::rlimit(a2, a3),
        NR::GETRLIMIT => proc::rlimit(0, a1),
        NR::SET_TID_ADDRESS => 0,
        NR::SET_ROBUST_LIST => 0,
        NR::RT_SIGACTION => 0,
        NR::RT_SIGPROCMASK => 0,
        NR::SIGALTSTACK => 0,
        NR::GETUID | NR::GETEUID => 0,
        NR::GETGID | NR::GETEGID => 0,
        NR::STATX => E::err(E::ENOSYS),
        _ => E::err(E::ENOSYS),
    };

    // 留痕：出问题时（用户态异常）由 `trace::dump` 打出来。
    // 正常路径一声不响，所以不会污染日志。
    trace::record(nr, a0, a1, a2, a3, ret);

    f.rax = ret as u64;
}

/// 读一个用户态路径并规范化成绝对路径。
///
/// `dirfd` 是 `AT_FDCWD` 时用 cwd；否则必须是本进程打开着的目录 fd，
/// 用它自己的路径当基准（`ls` 的 `fstatat(dirfd, name)` 就走这条路）。
///
/// 返回 `(缓冲区, 有效长度)`。
pub fn read_path(dirfd: u64, uptr: u64) -> Result<([u8; vfs::PATH_MAX], usize), i64> {
    let mut raw = [0u8; vfs::PATH_MAX];
    let n = uaccess::cstr(uptr, &mut raw)?;

    let mut basedir = [0u8; vfs::PATH_MAX];
    let mut blen = 0usize;
    if dirfd == klinux::flags::AT_FDCWD {
        let mut c = [0u8; task::CWD_MAX];
        let cn = task::get_cwd(&mut c);
        basedir[..cn].copy_from_slice(&c[..cn]);
        blen = cn;
    } else {
        let f = crate::fd::get(dirfd as usize)?;
        if f.kind != crate::fd::Kind::Dir {
            return Err(E::err(E::ENOTDIR));
        }
        blen = vfs::dir_path(f.id, &mut basedir).ok_or(E::err(E::EBADF))?;
    }

    let mut out = [0u8; vfs::PATH_MAX];
    let n2 = klinux::path::normalize(&basedir[..blen], &raw[..n], &mut out)?;
    if let Some(slot) = out.get_mut(n2) {
        *slot = 0;
    }
    Ok((out, n2))
}

/// 把 `[u8]` 路径转成 `&str`（非 UTF-8 一律当"不存在"，不给用户态任何别的信号）。
pub fn path_str(p: &[u8], n: usize) -> Result<&str, i64> {
    core::str::from_utf8(&p[..n]).map_err(|_| E::err(E::ENOENT))
}
