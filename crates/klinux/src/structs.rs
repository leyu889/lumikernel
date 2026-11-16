//! 跨边界结构体的**字节级**布局
//!
//! 为什么不用 `#[repr(C)]` 结构体 + 直接 `copy_to_user`：
//! 1. 用不了——用户指针必须经过受控探针逐页校验，只能逐字节写；
//! 2. 结构体布局是 ABI 的一部分，写错不报错、只是"看起来有点不对"。
//!    写成显式偏移 + 单测断言，比让编译器决定更可控。
//!
//! 这里的所有 `put_*` 都用 `get_mut` 而不是裸指针：越界返回 `false`，
//! 不会 panic（内核里 panic 等于停机）。

/// x86_64 的 `sizeof(struct stat)`。
pub const STAT_BYTES: usize = 144;

/// `struct stat`（x86_64）。
///
/// 字段顺序与 `asm/stat.h` 一致；末尾 24 字节是 `__unused[3]`，
/// 序列化时保持为 0。
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: i64,
    pub blksize: i64,
    pub blocks: i64,
    pub atime: i64,
    pub atime_nsec: i64,
    pub mtime: i64,
    pub mtime_nsec: i64,
    pub ctime: i64,
    pub ctime_nsec: i64,
}

impl Stat {
    /// 普通文件/目录的常用默认值：1 个硬链接、512 字节块、块数按大小向上取整。
    pub const fn with_mode(ino: u64, mode: u32, size: i64) -> Self {
        Stat {
            dev: 1,
            ino,
            nlink: 1,
            mode,
            uid: 0,
            gid: 0,
            rdev: 0,
            size,
            blksize: 4096,
            // st_blocks 的单位是 512 字节扇区（POSIX 就这么怪）
            blocks: (size + 511) / 512,
            atime: 0,
            atime_nsec: 0,
            mtime: 0,
            mtime_nsec: 0,
            ctime: 0,
            ctime_nsec: 0,
        }
    }

    /// 序列化成 144 字节。`out` 不足 144 字节返回 `false`。
    pub fn encode(&self, out: &mut [u8]) -> bool {
        put_u64(out, 0, self.dev)
            && put_u64(out, 8, self.ino)
            && put_u64(out, 16, self.nlink)
            && put_u32(out, 24, self.mode)
            && put_u32(out, 28, self.uid)
            && put_u32(out, 32, self.gid)
            && put_u32(out, 36, 0) // __pad0
            && put_u64(out, 40, self.rdev)
            && put_i64(out, 48, self.size)
            && put_i64(out, 56, self.blksize)
            && put_i64(out, 64, self.blocks)
            && put_i64(out, 72, self.atime)
            && put_i64(out, 80, self.atime_nsec)
            && put_i64(out, 88, self.mtime)
            && put_i64(out, 96, self.mtime_nsec)
            && put_i64(out, 104, self.ctime)
            && put_i64(out, 112, self.ctime_nsec)
            && zero(out, 120, 24)
    }
}

/// `dirent64` 的固定头长度：`d_ino(8) + d_off(8) + d_reclen(2) + d_type(1)`。
pub const DIRENT64_HDR: usize = 19;

/// `dirent64` 的 `d_reclen`：8 字节对齐，且要放得下结尾的 NUL。
pub const fn dirent64_reclen(name_len: usize) -> usize {
    (DIRENT64_HDR + name_len + 1 + 7) & !7usize
}

/// 写一个 `dirent64`，返回它占用的字节数（已对齐）。空间不够返回 `None`。
pub fn write_dirent64(
    out: &mut [u8],
    ino: u64,
    next_off: i64,
    dtype: u8,
    name: &[u8],
) -> Option<usize> {
    if name.len() > 255 {
        return None;
    }
    let reclen = dirent64_reclen(name.len());
    let cell = out.get_mut(..reclen)?;
    for b in cell.iter_mut() {
        *b = 0;
    }
    if !put_u64(cell, 0, ino) || !put_i64(cell, 8, next_off) {
        return None;
    }
    // d_reclen 最大 65535；名字上限 255，不会溢出
    let r = reclen as u16;
    cell.get_mut(16..18)?.copy_from_slice(&r.to_le_bytes());
    *cell.get_mut(18)? = dtype;
    cell.get_mut(19..19 + name.len())?.copy_from_slice(name);
    // 结尾的 NUL 已经由上面的清零保证
    Some(reclen)
}

/// `struct utsname`：6 个 65 字节的字段。
pub const UTS_FIELD: usize = 65;
pub const UTS_BYTES: usize = UTS_FIELD * 6;

/// 写一个 `utsname`。任何字段超长会被截断（内核自己给的字符串都是常量，不会超）。
pub fn write_utsname(
    out: &mut [u8],
    sysname: &[u8],
    nodename: &[u8],
    release: &[u8],
    version: &[u8],
    machine: &[u8],
    domain: &[u8],
) -> bool {
    if out.len() < UTS_BYTES {
        return false;
    }
    let fields = [sysname, nodename, release, version, machine, domain];
    for (i, f) in fields.iter().enumerate() {
        let base = i * UTS_FIELD;
        let cell = match out.get_mut(base..base + UTS_FIELD) {
            Some(c) => c,
            None => return false,
        };
        for b in cell.iter_mut() {
            *b = 0;
        }
        let n = core::cmp::min(f.len(), UTS_FIELD - 1);
        cell[..n].copy_from_slice(&f[..n]);
    }
    true
}

/// `struct timespec`（16 字节）。
pub fn write_timespec(out: &mut [u8], sec: i64, nsec: i64) -> bool {
    put_i64(out, 0, sec) && put_i64(out, 8, nsec)
}

/// `struct timeval`（16 字节）。
pub fn write_timeval(out: &mut [u8], sec: i64, usec: i64) -> bool {
    put_i64(out, 0, sec) && put_i64(out, 8, usec)
}

/// `struct rusage` 的字节数（wait4 必须回填它，否则 busybox 的 `sh` 会读到栈上垃圾）。
pub const RUSAGE_BYTES: usize = 144;

/// `struct termios`（x86_64，36 字节）。
pub const TERMIOS_BYTES: usize = 36;

/// 一个"哑终端"的 termios：没有回显、没有行缓冲，但 `isatty()` 会返回真。
///
/// 为什么重要：musl 的 `isatty()` 只看 `ioctl(TCGETS)` 成不成功。
/// 返回 `ENOTTY` 会让 busybox 认为 stdout 是管道——`ls` 仍然能跑，
/// 但 `sh` 的作业控制与提示符行为会走另一条代码路径。给个合法值最省事。
pub fn write_termios(out: &mut [u8]) -> bool {
    if out.len() < TERMIOS_BYTES {
        return false;
    }
    for b in out[..TERMIOS_BYTES].iter_mut() {
        *b = 0;
    }
    // c_iflag = ICRNL(0x100)；c_oflag = OPOST(0x1)；c_cflag = B38400|CS8|CREAD
    put_u32(out, 0, 0x100) && put_u32(out, 4, 0x1) && put_u32(out, 8, 0xBF) && put_u32(out, 12, 0x1)
}

/* ---------------- 小工具 ---------------- */

pub fn put_u64(out: &mut [u8], off: usize, v: u64) -> bool {
    match out.get_mut(off..off + 8) {
        Some(s) => {
            s.copy_from_slice(&v.to_le_bytes());
            true
        }
        None => false,
    }
}

pub fn put_i64(out: &mut [u8], off: usize, v: i64) -> bool {
    put_u64(out, off, v as u64)
}

pub fn put_u32(out: &mut [u8], off: usize, v: u32) -> bool {
    match out.get_mut(off..off + 4) {
        Some(s) => {
            s.copy_from_slice(&v.to_le_bytes());
            true
        }
        None => false,
    }
}

pub fn put_u16(out: &mut [u8], off: usize, v: u16) -> bool {
    match out.get_mut(off..off + 2) {
        Some(s) => {
            s.copy_from_slice(&v.to_le_bytes());
            true
        }
        None => false,
    }
}

fn zero(out: &mut [u8], off: usize, len: usize) -> bool {
    match out.get_mut(off..off + len) {
        Some(s) => {
            for b in s.iter_mut() {
                *b = 0;
            }
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags::{S_IFDIR, S_IFREG};

    #[test]
    fn stat_is_144_bytes() {
        let mut b = [0u8; STAT_BYTES];
        let s = Stat::with_mode(7, S_IFREG | 0o644, 2500);
        assert!(s.encode(&mut b));
        assert_eq!(b.len(), 144);
        // 关键偏移逐个检查：这几个错了，`ls -l` 会显示乱七八糟的大小
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), S_IFREG | 0o644);
        assert_eq!(i64::from_le_bytes(b[48..56].try_into().unwrap()), 2500);
        assert_eq!(i64::from_le_bytes(b[64..72].try_into().unwrap()), 5); // 2500/512 向上取整
    }

    #[test]
    fn stat_encode_rejects_short_buffer() {
        let mut b = [0u8; 143];
        assert!(!Stat::with_mode(1, S_IFDIR, 0).encode(&mut b));
    }

    #[test]
    fn dirent64_is_8_byte_aligned() {
        assert_eq!(dirent64_reclen(1), 24); // "." → 19+1+1=21 → 24
        assert_eq!(dirent64_reclen(4), 24); // "bin" → 19+4+1=24 → 24
        assert_eq!(dirent64_reclen(8), 32); // "hello.txt" → 28 → 32
        for n in 0..256 {
            assert_eq!(dirent64_reclen(n) % 8, 0);
            assert!(dirent64_reclen(n) >= DIRENT64_HDR + n + 1);
        }
    }

    #[test]
    fn dirent64_fields_land_where_linux_expects() {
        let mut buf = [0u8; 64];
        let n = write_dirent64(&mut buf, 42, 3, 4, b"bin").unwrap();
        assert_eq!(n, 24);
        assert_eq!(u64::from_le_bytes(buf[0..8].try_into().unwrap()), 42);
        assert_eq!(i64::from_le_bytes(buf[8..16].try_into().unwrap()), 3);
        assert_eq!(u16::from_le_bytes(buf[16..18].try_into().unwrap()), 24);
        assert_eq!(buf[18], 4);
        assert_eq!(&buf[19..23], b"bin\0");
        // 对齐填充必须是 0，不能是上一轮的残留
        assert_eq!(&buf[23..24], &[0u8]);
    }

    #[test]
    fn dirent64_needs_room() {
        let mut buf = [0u8; 23];
        assert!(write_dirent64(&mut buf, 1, 0, 8, b"abc").is_none());
    }

    #[test]
    fn utsname_layout() {
        let mut b = [0u8; UTS_BYTES];
        assert!(write_utsname(&mut b, b"Linux", b"lumi", b"0.9.0", b"#1", b"x86_64", b""));
        assert_eq!(&b[0..6], b"Linux\0");
        assert_eq!(&b[65..69], b"lumi");
        assert_eq!(&b[65 * 4..65 * 4 + 6], b"x86_64");
        assert_eq!(b[UTS_BYTES - 1], 0);
    }

    #[test]
    fn utsname_truncates_instead_of_overflowing() {
        let mut name = [b'A'; 200];
        name[0] = b'X';
        let mut b = [0u8; UTS_BYTES];
        assert!(write_utsname(&mut b, &name, b"", b"", b"", b"", b""));
        assert_eq!(b[0], b'X');
        assert_eq!(b[64], 0, "第 65 个字节必须是 NUL");
        assert_eq!(b[65], 0, "不能溢出到下一个字段");
    }

    #[test]
    fn termios_is_not_all_zero() {
        let mut b = [0u8; TERMIOS_BYTES];
        assert!(write_termios(&mut b));
        assert_ne!(b, [0u8; TERMIOS_BYTES]);
    }
}
