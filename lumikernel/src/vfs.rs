//! 虚拟文件系统视图（M9）
//!
//! M5–M8 里"文件"是散的：`fs::lookup` 只认 ext2 的绝对路径、`fs2` 只管
//! LumiFS 的扁平名字、控制台干脆没有名字。M9 要跑别人的程序，
//! 就必须有一套**统一命名空间**：一条路径进来，谁负责由挂载表决定。
//!
//! 挂载表（写死在代码里，没有 `mount` 系统调用）：
//!
//! ```text
//! /            ext2 系统盘（只读）        ← 里面放着 /bin/busybox
//! /data        LumiFS 数据盘（可写，扁平）
//! /dev         合成设备                   ← null / zero / console
//! ```
//!
//! 为什么 `/data` 是**扁平**的：LumiFS 本来就是扁平 inode 表（M8 的设计），
//! 硬造出目录层级只会让两套语义互相打架。它当"可写的临时盘"够用了。
//!
//! 目录读取（`getdents64`）需要"一次打开、多次读"的游标，所以这里有一张
//! 全局目录句柄表：每个句柄持有一份**打开时的快照**（`Vec<DirItem>`）。
//! 快照语义和 Linux 的 `readdir` 不完全一样（Linux 是边读边取），
//! 但对 `ls` 这种用法等价，而且省掉了"游标必须对应磁盘布局"的麻烦。

use alloc::vec;
use alloc::vec::Vec;

use klinux::errno as E;
use klinux::flags as F;

use crate::arch::sync::IrqSpinlock;
use crate::fs;
use crate::fs2;

/// 路径缓冲上限（含结尾 NUL 的余量）。Linux 的 `PATH_MAX` 是 4096，
/// 这里取 256：够 busybox 用，也保证不会在内核栈上开大数组。
pub const PATH_MAX: usize = 256;

/// 名字上限（ext2 是 255，LumiFS 是 24）。
pub const NAME_MAX: usize = 255;

/// 合成的字符设备。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dev {
    /// 读永远 EOF，写永远"成功"。
    Null,
    /// 读出来全是 0。
    Zero,
    /// 写进内核控制台（也就是串口 + 屏幕）。
    Console,
}

/// 一个已解析的对象。
#[derive(Clone, Copy, Debug)]
pub struct Obj {
    pub kind: Kind,
    /// ext2 inode 号或 LumiFS inode 号（设备用 0）。
    pub ino: u32,
    pub size: u32,
    /// 完整 `st_mode`（含文件类型位）。
    pub mode: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Ext,
    Lumi,
    Dev(Dev),
    /// `/dev` 这个目录本身。
    DirDev,
    /// `/data` 这个目录本身。
    DirLumi,
}

impl Obj {
    pub fn is_dir(&self) -> bool {
        klinux::flags::file_type(self.mode) == F::S_IFDIR
    }
    pub fn is_chr(&self) -> bool {
        klinux::flags::file_type(self.mode) == F::S_IFCHR
    }
    pub fn is_link(&self) -> bool {
        klinux::flags::file_type(self.mode) == F::S_IFLNK
    }
}

/// 目录句柄：一次 `opendir` 的快照 + 它自己的路径（`openat` 的 dirfd 要用）。
struct DirHandle {
    path: [u8; PATH_MAX],
    path_len: usize,
    items: Vec<DirItem>,
}

#[derive(Clone)]
struct DirItem {
    name: Vec<u8>,
    ino: u64,
    dtype: u8,
}

/// 目录句柄表。容量小：同时打开的目录不会有几个（busybox 的 `ls` 只开一个）。
const DIR_SLOTS: usize = 8;

static DIRS: IrqSpinlock<Vec<Option<DirHandle>>> = IrqSpinlock::new(Vec::new());

/* ---------------- 解析 ---------------- */

/// 把规范化后的绝对路径解析成对象。
///
/// `follow = false` 时**最后一段**若是符号链接，返回链接本身（`lstat` 语义）。
pub fn resolve(path: &[u8], follow: bool) -> Result<Obj, i64> {
    if path.is_empty() || path[0] != b'/' {
        return Err(E::err(E::ENOENT));
    }

    // ---- /dev ----
    if path == b"/dev" {
        return Ok(Obj {
            kind: Kind::DirDev,
            ino: 1,
            size: 0,
            mode: F::S_IFDIR | 0o755,
        });
    }
    if let Some(rest) = strip(path, b"/dev/") {
        let name = one_component(rest)?;
        let kind = match name {
            b"null" => Dev::Null,
            b"zero" => Dev::Zero,
            b"console" | b"tty" => Dev::Console,
            _ => return Err(E::err(E::ENOENT)),
        };
        return Ok(Obj {
            kind: Kind::Dev(kind),
            ino: 2,
            size: 0,
            mode: F::S_IFCHR | 0o666,
        });
    }

    // ---- /data（LumiFS，扁平）----
    if path == b"/data" {
        return Ok(Obj {
            kind: Kind::DirLumi,
            ino: 1,
            size: 0,
            mode: F::S_IFDIR | 0o755,
        });
    }
    if let Some(rest) = strip(path, b"/data/") {
        let name = one_component(rest)?;
        let ino = fs2::lookup(core::str::from_utf8(name).map_err(|_| E::err(E::ENOENT))?)?;
        return Ok(Obj {
            kind: Kind::Lumi,
            ino: ino.ino,
            size: ino.size,
            mode: F::S_IFREG | 0o644,
        });
    }

    // ---- 其余都归 ext2 ----
    let s = core::str::from_utf8(path).map_err(|_| E::err(E::ENOENT))?;
    let inode = fs::lookup_opt(s, follow)?;
    Ok(Obj {
        kind: Kind::Ext,
        ino: inode.number,
        size: inode.size,
        mode: inode.mode as u32,
    })
}

fn strip<'a>(path: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if path.len() > prefix.len() && &path[..prefix.len()] == prefix {
        Some(&path[prefix.len()..])
    } else {
        None
    }
}

/// 确认 `rest` 只剩一段（`/dev/null/foo` 这种要报 `ENOTDIR`）。
fn one_component(rest: &[u8]) -> Result<&[u8], i64> {
    if rest.is_empty() {
        return Err(E::err(E::ENOENT));
    }
    if rest.contains(&b'/') {
        return Err(E::err(E::ENOTDIR));
    }
    Ok(rest)
}

/* ---------------- 读 / 写 ---------------- */

/// 从对象读。返回读到的字节数（0 = EOF）。
pub fn read(o: &Obj, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
    if buf.is_empty() {
        return Ok(0);
    }
    if o.is_dir() {
        return Err(E::err(E::EISDIR));
    }
    if off > u32::MAX as u64 {
        return Ok(0);
    }
    let off = off as u32;
    match o.kind {
        Kind::Ext => {
            let inode = kfs::Inode {
                number: o.ino,
                mode: o.mode as u16,
                size: o.size,
                blocks: 0,
            };
            fs::read_at(inode, off, buf).map_err(|_| E::err(E::EIO))
        }
        Kind::Lumi => fs2::read_at(o.ino, off, buf),
        Kind::Dev(Dev::Null) => Ok(0),
        Kind::Dev(Dev::Zero) => {
            for b in buf.iter_mut() {
                *b = 0;
            }
            Ok(buf.len())
        }
        // 控制台没有键盘驱动，读它就是 EINVAL（和 M5 的 fd 层保持一致）
        Kind::Dev(Dev::Console) => Err(E::err(E::EINVAL)),
        Kind::DirDev | Kind::DirLumi => Err(E::err(E::EISDIR)),
    }
}

/// 往对象写。返回写入字节数。
pub fn write(o: &Obj, off: u64, buf: &[u8]) -> Result<usize, i64> {
    match o.kind {
        Kind::Dev(Dev::Null) | Kind::Dev(Dev::Zero) => Ok(buf.len()),
        Kind::Dev(Dev::Console) => {
            for &b in buf {
                crate::console::put_byte(b);
            }
            Ok(buf.len())
        }
        Kind::Lumi => {
            if off > u32::MAX as u64 {
                return Err(E::err(E::EFBIG));
            }
            fs2::write_at(o.ino, off as u32, buf)
        }
        // ext2 是只读挂载
        Kind::Ext => Err(E::err(E::EROFS)),
        Kind::DirDev | Kind::DirLumi => Err(E::err(E::EISDIR)),
    }
}

/* ---------------- 目录 ---------------- */

/// 打开一个目录，返回句柄槽位。
pub fn dir_open(path: &[u8], obj: &Obj) -> Result<u32, i64> {
    let mut items: Vec<DirItem> = Vec::new();

    match obj.kind {
        Kind::DirDev => {
            let names: [&[u8]; 3] = [b"null", b"zero", b"console"];
            for n in names.iter() {
                items.push(DirItem {
                    name: n.to_vec(),
                    ino: 2,
                    dtype: F::DT_CHR,
                });
            }
        }
        Kind::DirLumi => {
            for (name, ino, _size) in fs2::list()? {
                items.push(DirItem {
                    name,
                    ino: ino as u64,
                    dtype: F::DT_REG,
                });
            }
        }
        Kind::Ext => {
            let inode = kfs::Inode {
                number: obj.ino,
                mode: obj.mode as u16,
                size: obj.size,
                blocks: 0,
            };
            for e in fs::list_alloc(inode)? {
                let name = e.name();
                if name.is_empty() {
                    continue;
                }
                // 需要每个条目的类型：`d_type` 只是省一次 stat，但 ls 会用它
                // 决定显示颜色/加斜杠，所以还是查一下（目录项少，代价可忽略）。
                let child = resolve_child(path, name);
                let (dtype, ino) = match child {
                    Some(o) => (F::dtype_of(o.mode), o.ino as u64),
                    None => (F::DT_UNKNOWN, e.inode as u64),
                };
                items.push(DirItem {
                    name: name.to_vec(),
                    ino,
                    dtype,
                });
            }
        }
        _ => return Err(E::err(E::ENOTDIR)),
    }

    let mut p = [0u8; PATH_MAX];
    let n = path.len().min(PATH_MAX - 1);
    p[..n].copy_from_slice(&path[..n]);

    let mut dirs = DIRS.lock();
    // 找一个空槽位；没有就扩容（上限 DIR_SLOTS）
    let slot = match dirs.iter().position(|d| d.is_none()) {
        Some(i) => i,
        None => {
            if dirs.len() >= DIR_SLOTS {
                return Err(E::err(E::EMFILE));
            }
            dirs.push(None);
            dirs.len() - 1
        }
    };
    dirs[slot] = Some(DirHandle {
        path: p,
        path_len: n,
        items,
    });
    Ok(slot as u32)
}

/// 解析 `dir/name`，只用来取类型；失败就退回 `DT_UNKNOWN`。
fn resolve_child(dir: &[u8], name: &[u8]) -> Option<Obj> {
    if name == b"." || name == b".." {
        return None;
    }
    let mut buf = [0u8; PATH_MAX];
    let mut n = 0usize;
    for &b in dir.iter() {
        if n + 1 >= buf.len() {
            return None;
        }
        buf[n] = b;
        n += 1;
    }
    if !dir.ends_with(b"/") {
        if n + 1 >= buf.len() {
            return None;
        }
        buf[n] = b'/';
        n += 1;
    }
    for &b in name.iter() {
        if n + 1 >= buf.len() {
            return None;
        }
        buf[n] = b;
        n += 1;
    }
    resolve(&buf[..n], false).ok()
}

/// 目录句柄的第 `idx` 项。
pub fn dir_at(slot: u32, idx: usize) -> Option<(Vec<u8>, u64, u8)> {
    let dirs = DIRS.lock();
    let h = dirs.get(slot as usize)?.as_ref()?;
    let it = h.items.get(idx)?;
    Some((it.name.clone(), it.ino, it.dtype))
}

pub fn dir_len(slot: u32) -> usize {
    let dirs = DIRS.lock();
    match dirs.get(slot as usize).and_then(|d| d.as_ref()) {
        Some(h) => h.items.len(),
        None => 0,
    }
}

/// 目录自己的路径（`openat(dirfd, ...)` 用它当基准）。拷进调用方的缓冲。
pub fn dir_path(slot: u32, out: &mut [u8; PATH_MAX]) -> Option<usize> {
    let dirs = DIRS.lock();
    let h = dirs.get(slot as usize)?.as_ref()?;
    out[..h.path_len].copy_from_slice(&h.path[..h.path_len]);
    Some(h.path_len)
}

pub fn dir_close(slot: u32) {
    let mut dirs = DIRS.lock();
    if let Some(s) = dirs.get_mut(slot as usize) {
        *s = None;
    }
}

/// 当前打开的目录数（诊断/自检用）。
pub fn dir_open_count() -> usize {
    DIRS.lock().iter().filter(|d| d.is_some()).count()
}

/// 释放所有目录句柄（进程退出时用）。
pub fn dir_close_all() {
    let mut dirs = DIRS.lock();
    for d in dirs.iter_mut() {
        *d = None;
    }
}

/* ---------------- stat ---------------- */

/// 组装 `struct stat`。
pub fn stat_of(o: &Obj) -> klinux::structs::Stat {
    klinux::structs::Stat::with_mode(o.ino as u64, o.mode, o.size as i64)
}

/// 写一个 `dirent64` 需要的对齐长度（给 `getdents64` 预算空间用）。
pub fn dirent_reclen(name_len: usize) -> usize {
    klinux::structs::dirent64_reclen(name_len)
}
