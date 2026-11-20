//! 文件与目录类系统调用
//!
//! 这一层的职责只有三件：**把用户指针变成合法参数**、**调 `vfs`**、
//! **把结果写回用户内存**。所有真实逻辑都在 `vfs` 里——这样 `fstat` 看到的
//! 和 `ls` 看到的必然是同一份数据，不会出现"两处各算一遍 stat"的偏差。

use alloc::vec;

use klinux::errno as E;
use klinux::flags as F;
use klinux::structs;

use super::uaccess;
use crate::fd;
use crate::fs2;
use crate::task;
use crate::vfs;

/// 单次系统调用最多搬运多少字节。
///
/// 8 KiB 是个折中：内核栈只有 16 KiB，不能开大数组；而 8 KiB 的堆分配
/// 对忙碌的 `cat` 也够快了（短读/短写对调用方是合法的，busybox 会自己循环）。
const IO_CHUNK: usize = 8192;

/* ---------------- read / write ---------------- */

pub fn read(fdno: u64, uptr: u64, len: u64) -> i64 {
    if len == 0 {
        return 0;
    }
    let want = core::cmp::min(len as usize, IO_CHUNK);
    // 先确认用户缓冲区可写，再真读：反过来的话，读到管道里的数据却没人接收，
    // 数据就永久丢了（M7 的注释里写过这一点）。
    if !uaccess::check_readable(uptr, want) || !uaccess::check_writable(uptr, want) {
        // 限流诊断：懒分配上线后，"该成功却 EFAULT"的现场必须能看到
        // 具体是哪一页、页表/VMA 各是什么状态。
        {
            use core::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            if N.fetch_add(1, Ordering::Relaxed) < 4 {
                let pml4 = crate::mm::paging::read_cr3();
                kserialln!(
                    "[read] EFAULT uptr={:#x} len={} pte={:?} vma={:?}",
                    uptr,
                    want,
                    crate::mm::paging::pte_of(pml4, uptr & !0xFFF),
                    crate::task::vma_lookup(uptr & !0xFFF)
                );
            }
        }
        return E::err(E::EFAULT);
    }
    let mut buf = vec![0u8; want];
    match fd::read(fdno as usize, &mut buf) {
        // 入口处已整段验证过，这里不再重复查一遍（省一半页表遍历）
        Ok(n) => match uaccess::unchecked_copy_out(uptr, &buf[..n]) {
            Ok(()) => n as i64,
            Err(e) => e,
        },
        Err(e) => e,
    }
}

pub fn write(fdno: u64, uptr: u64, len: u64) -> i64 {
    if len == 0 {
        return 0;
    }
    let want = core::cmp::min(len as usize, IO_CHUNK);
    let mut buf = vec![0u8; want];
    if let Err(e) = uaccess::unchecked_copy_in(uptr, &mut buf) {
        return e;
    }
    match fd::write(fdno as usize, &buf) {
        Ok(n) => n as i64,
        Err(e) => e,
    }
}

/// `writev(2)`：分散写。
///
/// **musl 的 stdio 就靠它**：`printf` 最后走 `__stdio_write` → `writev`。
/// 没有它，所有通过 stdio 的输出都会静默丢掉（程序还正常退出 0），
/// 表现是"命令跑了但屏幕上什么都没有"。
pub fn writev(fdno: u64, iov: u64, iovcnt: u64) -> i64 {
    if iovcnt == 0 {
        return 0;
    }
    let cnt = core::cmp::min(iovcnt as usize, 16);
    let mut total = 0i64;
    for i in 0..cnt {
        // struct iovec { void *base; size_t len; }
        let ent = iov + (i as u64) * 16;
        let base = match uaccess::get_u64(ent) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let len = match uaccess::get_u64(ent + 8) {
            Ok(v) => v,
            Err(e) => return e,
        };
        if len == 0 {
            continue;
        }
        let n = write(fdno, base, len);
        if n < 0 {
            return if total > 0 { total } else { n };
        }
        total += n;
        if (n as u64) < len {
            break; // 短写：剩下的留给调用方重试
        }
    }
    total
}

/// `readv(2)`：分散读。
pub fn readv(fdno: u64, iov: u64, iovcnt: u64) -> i64 {
    if iovcnt == 0 {
        return 0;
    }
    let cnt = core::cmp::min(iovcnt as usize, 16);
    let mut total = 0i64;
    for i in 0..cnt {
        let ent = iov + (i as u64) * 16;
        let base = match uaccess::get_u64(ent) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let len = match uaccess::get_u64(ent + 8) {
            Ok(v) => v,
            Err(e) => return e,
        };
        if len == 0 {
            continue;
        }
        let n = read(fdno, base, len);
        if n < 0 {
            return if total > 0 { total } else { n };
        }
        total += n;
        if (n as u64) < len {
            break; // 短读：EOF 或没有更多数据
        }
    }
    total
}

/// `pread64(2)`：带偏移读，不动文件游标。
pub fn pread64(fdno: u64, uptr: u64, len: u64, off: u64) -> i64 {
    let f = match fd::get(fdno as usize) {
        Ok(f) => f,
        Err(e) => return e,
    };
    if len == 0 {
        return 0;
    }
    let want = core::cmp::min(len as usize, IO_CHUNK);
    let o = vfs::Obj {
        kind: match f.kind {
            fd::Kind::LumiFile => vfs::Kind::Lumi,
            _ => vfs::Kind::Ext,
        },
        ino: f.id,
        size: f.size,
        mode: f.mode,
    };
    let mut buf = vec![0u8; want];
    match vfs::read(&o, off, &mut buf) {
        Ok(n) => match uaccess::copy_out(uptr, &buf[..n]) {
            Ok(()) => n as i64,
            Err(e) => e,
        },
        Err(e) => e,
    }
}

/* ---------------- open ---------------- */

pub fn openat(dirfd: u64, pathptr: u64, flags: u32, _mode: u32) -> i64 {
    let (p, n) = match super::read_path(dirfd, pathptr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match vfs::resolve(&p[..n], true) {
        Ok(o) => {
            if o.is_dir() {
                // 想以写方式打开目录 → EISDIR（Linux 也就这个反应）
                if flags & F::O_ACCMODE != F::O_RDONLY {
                    return E::err(E::EISDIR);
                }
                let slot = match vfs::dir_open(&p[..n], &o) {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                match fd::install(&o, flags, slot) {
                    Ok(fdno) => fdno as i64,
                    Err(e) => {
                        vfs::dir_close(slot);
                        e
                    }
                }
            } else {
                if flags & F::O_DIRECTORY != 0 {
                    return E::err(E::ENOTDIR);
                }
                match fd::install(&o, flags, 0) {
                    Ok(fdno) => fdno as i64,
                    Err(e) => e,
                }
            }
        }
        Err(e) => {
            // 只有 LumiFS 能创建（ext2 是只读挂载）
            if flags & F::O_CREAT != 0 && p[..n].starts_with(b"/data/") {
                return create_lumi(&p[6..n], flags);
            }
            e
        }
    }
}

/// 在 LumiFS 里创建文件。`name` 不含 `/data/` 前缀。
fn create_lumi(name: &[u8], flags: u32) -> i64 {
    let s = match core::str::from_utf8(name) {
        Ok(s) => s,
        Err(_) => return E::err(E::EINVAL),
    };
    if s.is_empty() || s.contains('/') {
        // LumiFS 是扁平命名空间：`/data/a/b` 没有意义，明确拒绝
        return E::err(E::ENOENT);
    }
    let ino = match fs2::open_create(s) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let o = vfs::Obj {
        kind: vfs::Kind::Lumi,
        ino: ino.ino,
        size: ino.size,
        mode: F::S_IFREG | 0o644,
    };
    match fd::install(&o, flags, 0) {
        Ok(fdno) => fdno as i64,
        Err(e) => e,
    }
}

pub fn close(fdno: u64) -> i64 {
    match fd::close(fdno as usize) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

/* ---------------- stat ---------------- */

pub fn fstat(fdno: u64, statbuf: u64) -> i64 {
    match fd::stat(fdno as usize) {
        Ok(st) => write_stat(statbuf, &st),
        Err(e) => e,
    }
}

pub fn newfstatat(dirfd: u64, pathptr: u64, statbuf: u64, flags: u32) -> i64 {
    // `fstatat(fd, "", &st, AT_EMPTY_PATH)` 等价于 fstat
    let mut raw = [0u8; vfs::PATH_MAX];
    let n = match uaccess::cstr(pathptr, &mut raw) {
        Ok(n) => n,
        Err(e) => return e,
    };
    if n == 0 {
        if flags & F::AT_EMPTY_PATH != 0 {
            return fstat(dirfd, statbuf);
        }
        return E::err(E::ENOENT);
    }
    let (p, n2) = match super::read_path(dirfd, pathptr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let follow = flags & F::AT_SYMLINK_NOFOLLOW == 0;
    match vfs::resolve(&p[..n2], follow) {
        Ok(o) => write_stat(statbuf, &vfs::stat_of(&o)),
        Err(e) => e,
    }
}

fn write_stat(uptr: u64, st: &structs::Stat) -> i64 {
    let mut b = [0u8; structs::STAT_BYTES];
    if !st.encode(&mut b) {
        return E::err(E::EINVAL);
    }
    match uaccess::copy_out(uptr, &b) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

/* ---------------- 目录遍历 ---------------- */

pub fn getdents64(fdno: u64, uptr: u64, len: u64) -> i64 {
    if len == 0 {
        return E::err(E::EINVAL);
    }
    let f = match fd::get(fdno as usize) {
        Ok(f) => f,
        Err(e) => return e,
    };
    if f.kind != fd::Kind::Dir {
        return E::err(E::ENOTDIR);
    }
    let total = vfs::dir_len(f.id);
    let mut off = f.off as usize;
    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();

    while off < total {
        let (name, ino, dtype) = match vfs::dir_at(f.id, off) {
            Some(v) => v,
            None => break,
        };
        let reclen = structs::dirent64_reclen(name.len());
        if out.len() + reclen > len as usize {
            break; // 装不下这一条：就停在这，下次接着读
        }
        let mut cell = vec![0u8; reclen];
        if structs::write_dirent64(&mut cell, ino, (off + 1) as i64, dtype, &name).is_none() {
            return E::err(E::EINVAL);
        }
        out.extend_from_slice(&cell);
        off += 1;
    }

    if out.is_empty() && off < total {
        // 一条都装不下：Linux 对这种情况返回 EINVAL
        return E::err(E::EINVAL);
    }
    // 推进游标（写回 fd 表）。注意不能走 `lseek`：对目录它是 EINVAL。
    fd::set_off(fdno as usize, off as u32);
    match uaccess::copy_out(uptr, &out) {
        Ok(()) => out.len() as i64,
        Err(e) => e,
    }
}

/* ---------------- 其它 ---------------- */

pub fn lseek(fdno: u64, off: i64, whence: u32) -> i64 {
    match fd::seek(fdno as usize, off, whence) {
        Ok(v) => v as i64,
        Err(e) => e,
    }
}

pub fn dup(old: u64, new: Option<usize>) -> i64 {
    match fd::dup_to(old as usize, new) {
        Ok(fdno) => fdno as i64,
        Err(e) => e,
    }
}

pub fn pipe(uptr: u64) -> i64 {
    match fd::pipe_pair() {
        Ok((r, w)) => {
            // 用户态缓冲区里放两个 int（32 位）
            let mut b = [0u8; 8];
            b[0..4].copy_from_slice(&(r as i32).to_le_bytes());
            b[4..8].copy_from_slice(&(w as i32).to_le_bytes());
            match uaccess::copy_out(uptr, &b) {
                Ok(()) => 0,
                Err(e) => {
                    let _ = fd::close(r);
                    let _ = fd::close(w);
                    e
                }
            }
        }
        Err(e) => e,
    }
}

pub fn access(pathptr: u64) -> i64 {
    let (p, n) = match super::read_path(F::AT_FDCWD as u64, pathptr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match vfs::resolve(&p[..n], true) {
        Ok(_) => 0,
        Err(e) => e,
    }
}

pub fn readlink(dirfd: u64, pathptr: u64, buf: u64, bufsz: usize) -> i64 {
    let (p, n) = match super::read_path(dirfd, pathptr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let s = match super::path_str(&p, n) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let mut tgt = [0u8; 256];
    match crate::fs::read_link(s, &mut tgt) {
        Ok(tl) => {
            // readlink **不加** NUL 结尾；截断到 bufsz
            let m = core::cmp::min(tl, bufsz);
            match uaccess::copy_out(buf, &tgt[..m]) {
                Ok(()) => m as i64,
                Err(e) => e,
            }
        }
        Err(e) => e,
    }
}

pub fn chdir(pathptr: u64) -> i64 {
    let (p, n) = match super::read_path(F::AT_FDCWD as u64, pathptr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match vfs::resolve(&p[..n], true) {
        Ok(o) if o.is_dir() => match task::set_cwd(&p[..n]) {
            Ok(()) => 0,
            Err(e) => e,
        },
        Ok(_) => E::err(E::ENOTDIR),
        Err(e) => e,
    }
}

pub fn getcwd(bufptr: u64, size: usize) -> i64 {
    let mut c = [0u8; task::CWD_MAX];
    let n = task::get_cwd(&mut c);
    if size < n + 1 {
        return E::err(E::ERANGE);
    }
    if let Err(e) = uaccess::copy_out(bufptr, &c[..n]) {
        return e;
    }
    if let Err(e) = uaccess::copy_out(bufptr + n as u64, &[0u8]) {
        return e;
    }
    n as i64
}

pub fn unlinkat(dirfd: u64, pathptr: u64, _flags: u32) -> i64 {
    let (p, n) = match super::read_path(dirfd, pathptr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if !p[..n].starts_with(b"/data/") {
        // 根文件系统是只读挂载
        return E::err(E::EROFS);
    }
    let s = match core::str::from_utf8(&p[6..n]) {
        Ok(s) => s,
        Err(_) => return E::err(E::EINVAL),
    };
    match fs2::unlink(s) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub fn fcntl(fdno: u64, cmd: u64, arg: u64) -> i64 {
    let f = match fd::get(fdno as usize) {
        Ok(f) => f,
        Err(e) => return e,
    };
    match cmd {
        1 => 0,                     // F_GETFD
        2 => 0,                     // F_SETFD
        3 => f.flags as i64,        // F_GETFL
        4 => 0,                     // F_SETFL
        7 | 8 => 0,                 // F_GETLK / F_SETLK：没有锁，直接说"可以"
        _ => {
            let _ = arg;
            E::err(E::EINVAL)
        }
    }
}

/// `ioctl`：只认终端那一组。musl 的 `isatty()` 就靠 `TCGETS`。
pub fn ioctl(fdno: u64, req: u64, arg: u64) -> i64 {
    let _ = (req, arg);
    let _ = match fd::get(fdno as usize) {
        Ok(f) => f,
        Err(e) => return e,
    };
    // 我们的控制台没有终端语义（没有键盘、没有行规程），所以**如实**返回
    // `ENOTTY`——`isatty()` 于是为假。这一条是踩出来的：早先为了"看起来像终端"
    // 回了合法的 termios，结果 `ls` 判定 stdout 是终端、给每个名字套上 ANSI 颜色，
    // 屏幕上出现一堆 `ESC[1;34m`。宁可诚实。
    E::err(E::ENOTTY)
}
