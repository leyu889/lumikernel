//! 各种标志位与常量（`fcntl.h` / `mmap.h` / `sys/stat.h` / `auxv.h`）

/* ---------------- open(2) ---------------- */

pub const O_RDONLY: u32 = 0;
pub const O_WRONLY: u32 = 1;
pub const O_RDWR: u32 = 2;
pub const O_ACCMODE: u32 = 3;
pub const O_CREAT: u32 = 0o100;
pub const O_EXCL: u32 = 0o200;
pub const O_TRUNC: u32 = 0o1000;
pub const O_APPEND: u32 = 0o2000;
pub const O_NONBLOCK: u32 = 0o4000;
pub const O_DIRECTORY: u32 = 0o200000;
pub const O_NOFOLLOW: u32 = 0o400000;
pub const O_CLOEXEC: u32 = 0o2000000;

/* ---------------- *at(2) ---------------- */

/// `AT_FDCWD` 在寄存器里是 `-100`，用 u64 表示就是它的补码。
pub const AT_FDCWD: u64 = (-100i64) as u64;
pub const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
pub const AT_REMOVEDIR: u32 = 0x200;
pub const AT_SYMLINK_FOLLOW: u32 = 0x400;
pub const AT_EMPTY_PATH: u32 = 0x1000;

/* ---------------- stat 模式位 ---------------- */

pub const S_IFMT: u32 = 0o170000;
pub const S_IFIFO: u32 = 0o010000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFSOCK: u32 = 0o140000;

/// 取出模式位里的"文件类型"部分。
#[inline]
pub const fn file_type(mode: u32) -> u32 {
    mode & S_IFMT
}

/// 权限位（rwxrwxrwx）。
#[inline]
pub const fn perms(mode: u32) -> u32 {
    mode & 0o7777
}

/* ---------------- dirent64 的 d_type ---------------- */

pub const DT_UNKNOWN: u8 = 0;
pub const DT_FIFO: u8 = 1;
pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_BLK: u8 = 6;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;
pub const DT_SOCK: u8 = 12;

/// 从 `st_mode` 推出 `d_type`。目录项里放类型只是省一次 stat，
/// 放错不会立刻出事，但 `ls` 会把目录画成文件——所以两种来源用同一个函数。
pub const fn dtype_of(mode: u32) -> u8 {
    match file_type(mode) {
        S_IFIFO => DT_FIFO,
        S_IFCHR => DT_CHR,
        S_IFDIR => DT_DIR,
        S_IFBLK => DT_BLK,
        S_IFREG => DT_REG,
        S_IFLNK => DT_LNK,
        S_IFSOCK => DT_SOCK,
        _ => DT_UNKNOWN,
    }
}

/* ---------------- lseek(2) ---------------- */

pub const SEEK_SET: u32 = 0;
pub const SEEK_CUR: u32 = 1;
pub const SEEK_END: u32 = 2;

/* ---------------- mmap(2) ---------------- */

pub const PROT_NONE: u32 = 0;
pub const PROT_READ: u32 = 1;
pub const PROT_WRITE: u32 = 2;
pub const PROT_EXEC: u32 = 4;

pub const MAP_SHARED: u32 = 1;
pub const MAP_PRIVATE: u32 = 2;
pub const MAP_FIXED: u32 = 0x10;
pub const MAP_ANONYMOUS: u32 = 0x20;
pub const MAP_NORESERVE: u32 = 0x4000;
pub const MAP_STACK: u32 = 0x2_0000;

/* ---------------- clock_gettime(2) ---------------- */

pub const CLOCK_REALTIME: u64 = 0;
pub const CLOCK_MONOTONIC: u64 = 1;

/* ---------------- arch_prctl(2) ---------------- */

pub const ARCH_SET_GS: u64 = 0x1001;
pub const ARCH_SET_FS: u64 = 0x1002;
pub const ARCH_GET_FS: u64 = 0x1003;
pub const ARCH_GET_GS: u64 = 0x1004;

/* ---------------- 初始栈上的 auxv 类型 ---------------- */

pub const AT_NULL: u64 = 0;
pub const AT_PHDR: u64 = 3;
pub const AT_PHENT: u64 = 4;
pub const AT_PHNUM: u64 = 5;
pub const AT_PAGESZ: u64 = 6;
pub const AT_BASE: u64 = 7;
pub const AT_ENTRY: u64 = 9;
pub const AT_UID: u64 = 11;
pub const AT_EUID: u64 = 12;
pub const AT_GID: u64 = 13;
pub const AT_EGID: u64 = 14;
pub const AT_HWCAP: u64 = 16;
pub const AT_CLKTCK: u64 = 17;
pub const AT_SECURE: u64 = 23;
pub const AT_RANDOM: u64 = 25;
pub const AT_EXECFN: u64 = 31;

/* ---------------- ioctl ---------------- */

/// `TCGETS`：musl 的 `isatty()` 就靠它。我们不支持终端语义，
/// 但**必须回一个像样的 termios**，否则 `ls` 会以为有颜色终端，
/// 或者 busybox 的 `cttyhack` 之类的路径直接失败。
pub const TCGETS: u64 = 0x5401;
pub const TCSETS: u64 = 0x5402;

/* ---------------- wait4 ---------------- */

pub const WNOHANG: u32 = 1;

/* ---------------- poll ---------------- */

pub const POLLIN: i16 = 0x1;
pub const POLLOUT: i16 = 0x4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_fdcwd_is_minus_100() {
        assert_eq!(AT_FDCWD as i64, -100);
    }

    #[test]
    fn dtype_matches_mode() {
        assert_eq!(dtype_of(S_IFDIR | 0o755), DT_DIR);
        assert_eq!(dtype_of(S_IFREG | 0o644), DT_REG);
        assert_eq!(dtype_of(S_IFLNK | 0o777), DT_LNK);
        assert_eq!(dtype_of(0), DT_UNKNOWN);
    }
}
