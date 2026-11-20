//! 文件描述符表（M7 起每进程一张，M9 扩到 Linux 语义）
//!
//! 表里放的是"文件描述符"，不是 POSIX 意义上的"打开文件描述"。
//! 真实系统里两者是不同对象（`fork` 之后父子共享偏移，但各自有独立的 fd 位）；
//! M9 加了 `fork`，但**仍然是每个进程一份独立偏移**——这个简化必须写在代码里，
//! 否则以后会误以为"父子共享偏移"是本来就有的行为。
//! 对 busybox 这种"fork 完就 exec"的用法，这个差异观察不到。
//!
//! M9 新增的描述符类型：
//! - `Dir`：目录（`getdents64` 用，`id` 是 `vfs` 的目录句柄槽位）
//! - `DevNull` / `DevZero`：`/dev/null`、`/dev/zero`
//!
//! 读写的实际动作全部转给 `vfs`：这一层只负责"fd → 对象"和偏移。

use klinux::errno as E;
use klinux::flags as F;

use crate::pipe;
use crate::vfs;

/// 每个进程最多打开多少个 fd。busybox 的 `sh` 会同时开着脚本、目录和管道，
/// M7 的 8 个不够用（真的撞上了 `EMFILE`）。
pub const MAX_FD: usize = 16;
/// 控制台占用的 fd 数。
pub const FIRST_FREE_FD: usize = 3;

pub const EBADF: i64 = -9;
pub const ENOENT: i64 = -2;
pub const EMFILE: i64 = -24;
pub const EIO: i64 = -5;
pub const EINVAL: i64 = -22;

/// 一个 fd 指向什么。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// 空槽位。
    Free,
    /// 控制台：写进内核日志，读不可用。
    Console,
    /// ext2 上的普通文件（只读挂载）。
    File,
    /// 管道读端 / 写端：`id` 是管道下标。
    PipeRead,
    PipeWrite,
    /// LumiFS（可写数据盘）上的文件：`id` 是 inode 号。
    ///
    /// 和 `Kind::File` 分开，是因为它们的写语义完全不同：
    /// 一个是 `EROFS`，一个是真的能写。混成一个变体会让"能不能写"取决于
    /// 某个隐藏字段，读代码的人永远不会注意到。
    LumiFile,
    /// 目录：`id` 是 `vfs` 的目录句柄槽位，`off` 是遍历游标。
    Dir,
    DevNull,
    DevZero,
    /// UDP socket：`id` 低 28 位是 `net` 表下标。
    SocketUdp,
    /// TCP socket：同上。
    SocketTcp,
}

#[derive(Clone, Copy)]
pub struct File {
    pub kind: Kind,
    /// `File`/`LumiFile` 是 inode 号；`Pipe*` 是管道下标；`Dir` 是目录句柄。
    pub id: u32,
    /// 文件大小（打开时的快照）。
    pub size: u32,
    /// 读写偏移。
    pub off: u32,
    /// 打开时的权限位（`st_mode`），`fstat` 直接用它。
    pub mode: u32,
    /// `open(2)` 的 flags。
    pub flags: u32,
}

pub const FREE: File = File {
    kind: Kind::Free,
    id: 0,
    size: 0,
    off: 0,
    mode: 0,
    flags: 0,
};

pub const CONSOLE: File = File {
    kind: Kind::Console,
    id: 0,
    size: 0,
    off: 0,
    mode: F::S_IFCHR | 0o666,
    flags: 0,
};

/// 新建一张 fd 表：0/1/2 是控制台，其余空。
pub const fn new_table() -> [File; MAX_FD] {
    let mut t = [FREE; MAX_FD];
    t[0] = CONSOLE;
    t[1] = CONSOLE;
    t[2] = CONSOLE;
    t
}

/// 对当前任务的 fd 表做事。
fn with_table<R>(f: impl FnOnce(&mut [File; MAX_FD]) -> R) -> R {
    match crate::task::current_task_mut() {
        Some(t) => f(&mut t.fds),
        None => {
            let mut dummy = [FREE; MAX_FD];
            f(&mut dummy)
        }
    }
}

fn find_free(t: &[File; MAX_FD], from: usize) -> Option<usize> {
    (from..MAX_FD).find(|&i| t[i].kind == Kind::Free)
}

/// 装 socket 的 fd（`linux/net` 用）。
///
/// `kind`: 1=UDP，2=TCP（`linux/net` 的 SOCK_* 值）。
pub fn alloc_socket_fd(kind: u8, id: usize) -> Option<usize> {
    let k = match kind {
        1 => Kind::SocketUdp,
        2 => Kind::SocketTcp,
        _ => return None,
    };
    with_table(|t| {
        let slot = find_free(t, FIRST_FREE_FD)?;
        t[slot] = File {
            kind: k,
            id: id as u32,
            size: 0,
            off: 0,
            mode: 0,
            flags: 0,
        };
        Some(slot)
    })
}

/// 取某个 fd 的快照。
pub fn get(fd: usize) -> Result<File, i64> {
    let f = with_table(|t| t.get(fd).copied()).ok_or(EBADF)?;
    if f.kind == Kind::Free {
        return Err(EBADF);
    }
    Ok(f)
}

/// 把快照里的可变部分写回（偏移、大小）。
fn update(fd: usize, expect_id: u32, off: u32, size: u32) {
    with_table(|t| {
        if let Some(e) = t.get_mut(fd) {
            if e.id == expect_id && e.kind != Kind::Free {
                e.off = off;
                e.size = size;
            }
        }
    });
}

/* ---------------- 打开 ---------------- */

/// 装一个已解析的对象进 fd 表。
pub fn install(o: &vfs::Obj, flags: u32, dir_slot: u32) -> Result<usize, i64> {
    // 先判"是不是目录"再判来源：ext2 的目录在 `Kind::Ext` 里，
    // 只按 `Kind` 分派会把它装成普通文件，`getdents64` 就会报 ENOTDIR
    // （`ls /` 于是什么都不打印——踩过一次）。
    let (kind, id) = if o.is_dir() {
        (Kind::Dir, dir_slot)
    } else {
        match o.kind {
            vfs::Kind::Ext => (Kind::File, o.ino),
            vfs::Kind::Lumi => (Kind::LumiFile, o.ino),
            vfs::Kind::Dev(vfs::Dev::Null) => (Kind::DevNull, 0),
            vfs::Kind::Dev(vfs::Dev::Zero) => (Kind::DevZero, 0),
            vfs::Kind::Dev(vfs::Dev::Console) => (Kind::Console, 0),
            vfs::Kind::DirDev | vfs::Kind::DirLumi => (Kind::Dir, dir_slot),
        }
    };
    with_table(|t| match find_free(t, FIRST_FREE_FD) {
        Some(i) => {
            t[i] = File {
                kind,
                id,
                size: o.size,
                off: 0,
                mode: o.mode,
                flags,
            };
            Ok(i)
        }
        None => Err(E::err(E::EMFILE)),
    })
}

/// 打开 ext2 上的文件（M5 的老入口，走新 VFS）。
pub fn open(path: &str) -> Result<usize, i64> {
    let o = vfs::resolve(path.as_bytes(), true)?;
    if o.is_dir() {
        return Err(E::err(E::EISDIR));
    }
    install(&o, F::O_RDONLY, 0)
}

/// 打开 LumiFS 上的文件（不存在就创建），返回 fd。
pub fn open_w(path: &str) -> Result<usize, i64> {
    let name = path.trim_start_matches('/');
    if name.is_empty() || name.len() > lumifs::NAME_MAX {
        return Err(EINVAL);
    }
    let inode = crate::fs2::open_create(name)?;
    with_table(|t| match find_free(t, FIRST_FREE_FD) {
        Some(i) => {
            t[i] = File {
                kind: Kind::LumiFile,
                id: inode.ino,
                size: inode.size,
                off: 0,
                mode: F::S_IFREG | 0o644,
                flags: F::O_RDWR,
            };
            Ok(i)
        }
        None => Err(E::err(E::EMFILE)),
    })
}

/// 创建一对管道，返回 `(读端 fd, 写端 fd)`。
pub fn pipe_pair() -> Result<(usize, usize), i64> {
    let id = pipe::create().map_err(|_| E::err(E::EMFILE))?;
    with_table(|t| {
        let r = find_free(t, FIRST_FREE_FD).ok_or(E::err(E::EMFILE))?;
        t[r] = File {
            kind: Kind::PipeRead,
            id: id as u32,
            size: 0,
            off: 0,
            mode: F::S_IFIFO | 0o644,
            flags: F::O_RDONLY,
        };
        let w = match find_free(t, FIRST_FREE_FD) {
            Some(w) => w,
            None => {
                t[r] = FREE;
                return Err(E::err(E::EMFILE));
            }
        };
        t[w] = File {
            kind: Kind::PipeWrite,
            id: id as u32,
            size: 0,
            off: 0,
            mode: F::S_IFIFO | 0o644,
            flags: F::O_WRONLY,
        };
        Ok((r, w))
    })
}

/* ---------------- 读 / 写 ---------------- */

/// 从 fd 读。返回读到的字节数（0 = EOF）。
pub fn read(fd: usize, out: &mut [u8]) -> Result<usize, i64> {
    if out.is_empty() {
        return Ok(0);
    }
    let f = get(fd)?;
    match f.kind {
        Kind::Free => Err(EBADF),
        Kind::Console => Err(E::err(E::EINVAL)),
        Kind::PipeRead => pipe::read(f.id as usize, out),
        Kind::PipeWrite => Err(EBADF),
        Kind::Dir => Err(E::err(E::EISDIR)),
        Kind::DevNull => Ok(0),
        Kind::DevZero => {
            for b in out.iter_mut() {
                *b = 0;
            }
            Ok(out.len())
        }
        Kind::File => read_file(fd, &f, out),
        Kind::LumiFile => {
            if f.off >= f.size && f.size != 0 {
                return Ok(0);
            }
            let n = out.len().min(8192);
            let got = crate::fs2::read_at(f.id, f.off, &mut out[..n])?;
            update(fd, f.id, f.off.saturating_add(got as u32), f.size);
            Ok(got)
        }
        // socket 只能走 sendto/recvfrom（它们管阻塞与地址语义）；
        // fd 层的 read/write 不认识 socket，明确拒绝比静默错好。
        Kind::SocketUdp | Kind::SocketTcp => Err(E::err(E::EINVAL)),
    }
}

/// ext2 文件读取：走 `vfs`，顺便处理"读得比文件短"的情况。
fn read_file(fd: usize, f: &File, out: &mut [u8]) -> Result<usize, i64> {
    let o = vfs::Obj {
        kind: vfs::Kind::Ext,
        ino: f.id,
        size: f.size,
        mode: f.mode,
    };
    let got = vfs::read(&o, f.off as u64, out)?;
    update(fd, f.id, f.off.saturating_add(got as u32), f.size);
    Ok(got)
}

/// 往 fd 写。返回写入字节数。
pub fn write(fd: usize, data: &[u8]) -> Result<usize, i64> {
    let f = get(fd)?;
    match f.kind {
        Kind::Free => Err(EBADF),
        Kind::Console => {
            for &b in data {
                crate::console::put_byte(b);
            }
            Ok(data.len())
        }
        Kind::PipeWrite => pipe::write(f.id as usize, data),
        Kind::PipeRead => Err(EBADF),
        Kind::Dir => Err(E::err(E::EISDIR)),
        Kind::DevNull | Kind::DevZero => Ok(data.len()),
        Kind::SocketUdp | Kind::SocketTcp => Err(E::err(E::EINVAL)),
        Kind::LumiFile => {
            // LumiFS 单次事务最多几块，会短写；循环到写完为止，
            // 用户态看到的仍然是一次完整的 write。
            let mut done = 0usize;
            let mut off = f.off;
            while done < data.len() {
                let n = crate::fs2::write_at(f.id, off, &data[done..])?;
                if n == 0 {
                    break;
                }
                done += n;
                off = off.saturating_add(n as u32);
            }
            update(fd, f.id, off, f.size.max(off));
            Ok(done)
        }
        // ext2 是只读挂载
        Kind::File => Err(E::err(E::EROFS)),
    }
}

/* ---------------- lseek / stat / dup ---------------- */

/// `lseek(2)`。
pub fn seek(fd: usize, offset: i64, whence: u32) -> Result<u64, i64> {
    let f = get(fd)?;
    let base = match f.kind {
        Kind::PipeRead | Kind::PipeWrite => return Err(E::err(E::ESPIPE)),
        Kind::Dir => return Err(E::err(E::EINVAL)),
        _ => match whence {
            F::SEEK_SET => 0i64,
            F::SEEK_CUR => f.off as i64,
            F::SEEK_END => f.size as i64,
            _ => return Err(E::err(E::EINVAL)),
        },
    };
    let n = base.checked_add(offset).ok_or(E::err(E::EINVAL))?;
    if n < 0 {
        return Err(E::err(E::EINVAL));
    }
    if n > u32::MAX as i64 {
        return Err(E::err(E::EFBIG));
    }
    update(fd, f.id, n as u32, f.size);
    Ok(n as u64)
}

/// 直接设置偏移。
///
/// 只给**目录遍历**用：`lseek` 对目录是非法的（`EINVAL`），
/// 但 `getdents64` 内部必须能推进游标。早期版本这里误用了 `seek`，
/// 结果 seek 一直返回 `EINVAL`、游标永远不动，`ls` 就死循环了。
pub fn set_off(fdno: usize, off: u32) {
    with_table(|t| {
        if let Some(e) = t.get_mut(fdno) {
            if e.kind != Kind::Free {
                e.off = off;
            }
        }
    });
}

/// 组装 `fstat` 的结果。
pub fn stat(fd: usize) -> Result<klinux::structs::Stat, i64> {
    let f = get(fd)?;
    Ok(klinux::structs::Stat::with_mode(
        f.id as u64,
        f.mode,
        f.size as i64,
    ))
}

/// `dup(2)` / `dup2(2)` 的公共实现：把 `old` 的表项复制到 `new`（或最小空位）。
pub fn dup_to(old: usize, new: Option<usize>) -> Result<usize, i64> {
    let f = get(old)?;
    with_table(|t| match new {
        Some(n) => {
            if n >= MAX_FD {
                return Err(E::err(E::EBADF));
            }
            let old_entry = t[n];
            t[n] = f;
            release_entry(&old_entry, &f);
            Ok(n)
        }
        None => match find_free(t, FIRST_FREE_FD) {
            Some(i) => {
                t[i] = f;
                Ok(i)
            }
            None => Err(E::err(E::EMFILE)),
        },
    })
}

/// 被 `dup2` 覆盖掉的旧描述符要做的清理。
fn release_entry(old: &File, new: &File) {
    // 同一个底层对象就不用动（dup2(fd, fd) 是合法的空操作）
    if old.kind == new.kind && old.id == new.id {
        return;
    }
    match old.kind {
        Kind::Dir => vfs::dir_close(old.id),
        Kind::PipeRead => pipe::close_read(old.id as usize),
        Kind::PipeWrite => pipe::close_write(old.id as usize),
        _ => {}
    }
}

/* ---------------- 关闭 ---------------- */

/// 关闭 fd。管道端点关闭会唤醒对端（读端全关 → 写端拿到 `EPIPE`）。
pub fn close(fd: usize) -> Result<(), i64> {
    let f = with_table(|t| match t.get_mut(fd) {
        Some(e) if e.kind != Kind::Free => {
            let old = *e;
            *e = FREE;
            Some(old)
        }
        _ => None,
    })
    .ok_or(EBADF)?;

    match f.kind {
        Kind::PipeRead => pipe::close_read(f.id as usize),
        Kind::PipeWrite => pipe::close_write(f.id as usize),
        Kind::Dir => vfs::dir_close(f.id),
        // socket：同步清掉 `net` 表（否则端口/连接会一直挂着）
        Kind::SocketUdp => crate::net::close_udp(f.id as usize),
        Kind::SocketTcp => crate::net::close_tcp(f.id as usize),
        _ => {}
    }
    Ok(())
}

/// 当前任务打开的非控制台 fd 数量（自检用）。
pub fn open_count() -> usize {
    with_table(|t| {
        t.iter()
            .filter(|f| f.kind != Kind::Free && f.kind != Kind::Console)
            .count()
    })
}

/// 关掉当前任务的所有 fd（退出路径与自检之间互不影响）。
pub fn close_all() {
    let snapshot = with_table(|t| *t);
    for (i, f) in snapshot.iter().enumerate() {
        if f.kind == Kind::Free || f.kind == Kind::Console {
            continue;
        }
        let _ = close(i);
    }
}
