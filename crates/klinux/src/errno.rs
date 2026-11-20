//! errno 与"负返回值"约定
//!
//! Linux 的系统调用**不返回错误码**：成功返回非负值，失败返回 `-errno`。
//! 内核里如果混进"直接返回正数 errno"的路径，用户态会把它当成有效结果，
//! 于是错误被静默吞掉——所以这里提供 [`err`]，强制带上负号。

pub type Errno = i64;

pub const EPERM: Errno = 1;
pub const ENOENT: Errno = 2;
pub const ESRCH: Errno = 3;
pub const EINTR: Errno = 4;
pub const EIO: Errno = 5;
pub const E2BIG: Errno = 7;
pub const ENOEXEC: Errno = 8;
pub const EBADF: Errno = 9;
pub const ECHILD: Errno = 10;
pub const EAGAIN: Errno = 11;
pub const ENOMEM: Errno = 12;
pub const EACCES: Errno = 13;
pub const EFAULT: Errno = 14;
pub const EBUSY: Errno = 16;
pub const EEXIST: Errno = 17;
pub const ENODEV: Errno = 19;
pub const ENOTDIR: Errno = 20;
pub const EISDIR: Errno = 21;
pub const EINVAL: Errno = 22;
pub const ENFILE: Errno = 23;
pub const EMFILE: Errno = 24;
pub const ENOTTY: Errno = 25;
pub const EFBIG: Errno = 27;
pub const ENOSPC: Errno = 28;
pub const ESPIPE: Errno = 29;
pub const EROFS: Errno = 30;
pub const EPIPE: Errno = 32;
pub const ERANGE: Errno = 34;
pub const ENAMETOOLONG: Errno = 36;
pub const ENOSYS: Errno = 38;
pub const ENOTEMPTY: Errno = 39;
pub const ELOOP: Errno = 40;
pub const EOVERFLOW: Errno = 75;
pub const ENOTSUP: Errno = 95;

/// 把 errno 变成系统调用的返回值。
#[inline]
pub const fn err(e: Errno) -> i64 {
    -e
}

/// 从系统调用返回值里取出 errno（0 表示成功）。
#[inline]
pub const fn of(ret: i64) -> Errno {
    if ret < 0 && ret > -4096 {
        -ret
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn err_is_always_negative() {
        assert_eq!(err(ENOENT), -2);
        assert_eq!(err(ENOSYS), -38);
    }

    #[test]
    fn of_roundtrip() {
        assert_eq!(of(err(EINVAL)), EINVAL);
        assert_eq!(of(0), 0);
        // 合法的"大正数"返回值不能被误判成错误
        assert_eq!(of(1_000_000), 0);
    }
}

// ---- 网络（M12）----
pub const ENOTSOCK: Errno = 88;
pub const EDESTADDRREQ: Errno = 89;
pub const EMSGSIZE: Errno = 90;
pub const EPROTOTYPE: Errno = 91;
pub const ENOPROTOOPT: Errno = 92;
pub const EPROTONOSUPPORT: Errno = 93;
pub const ESOCKTNOSUPPORT: Errno = 94;
pub const EOPNOTSUPP: Errno = 95;
pub const EAFNOSUPPORT: Errno = 97;
pub const EADDRINUSE: Errno = 98;
pub const EADDRNOTAVAIL: Errno = 99;
pub const ENETDOWN: Errno = 100;
pub const ENETUNREACH: Errno = 101;
pub const ECONNABORTED: Errno = 103;
pub const ECONNRESET: Errno = 104;
pub const EISCONN: Errno = 106;
pub const ENOTCONN: Errno = 107;
pub const ESHUTDOWN: Errno = 108;
pub const ETIMEDOUT: Errno = 110;
pub const ECONNREFUSED: Errno = 111;
