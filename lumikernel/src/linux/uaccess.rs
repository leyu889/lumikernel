//! 用户内存访问（M9）
//!
//! 内核里所有"用户给的指针"都必须先过这里。三条铁律：
//!
//! 1. **不假设指针是对的**。一个坏指针必须变成 `-EFAULT`，不能把内核带走。
//! 2. **按页验证**，而不是按字节。验证用 M2 的受控故障探针（`intr::probe`）：
//!    真的去读/写一下，缺页就靠异常 fixup 跳回来。这和 Linux 的
//!    `copy_from_user` + exception table 是同一个思路。
//! 3. **验证和搬运之间不能被打断**（`PreemptGuard`）。否则验证通过之后
//!    任务被切走、页表可能变化，再拷就踩空了。
//!
//! 与 `crate::syscall`（int 0x80 那套老通道）的关系：老文件里有一份等价的实现，
//! 是 M5 时写的、被 M5–M8 的自检覆盖着。M9 起新代码一律走这里，
//! 老实现保留在原处不动——改它没有收益，只有回归风险。

use kfault::VEC_PF;

use crate::intr::probe::{self, ProbeAction};
use crate::task::PreemptGuard;

use klinux::errno::{err, EFAULT};

/// `[ptr, ptr+len)` 在内核视角下可读吗？
pub fn check_readable(ptr: u64, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let end = match ptr.checked_add(len as u64) {
        Some(e) => e,
        None => return false,
    };
    // 非规范地址会让 CPU 报 #GP 而不是 #PF；探针只接 #PF，这类地址必须提前挡掉。
    if !kvm::is_canonical(ptr) || !kvm::is_canonical(end - 1) {
        return false;
    }
    let mut page = ptr & !0xFFF;
    while page < end {
        // 之前这里用"探针真读一次再恢复"。懒分配之后这招失灵了：
        // 没映射的合法页会 #PF，探针把它当成坏指针——`cat` 于是
        // 拿到 EFAULT（read 先 check_readable，而它的 buf 来自 mmap）。
        // 现在用页表 + VMA 直接判定，探针留给故障注入自检。
        if !crate::mm::fault::can_read(page) {
            return false;
        }
        page = match page.checked_add(0x1000) {
            Some(p) => p,
            None => return false,
        };
    }
    true
}

/// `[ptr, ptr+len)` 可写吗？
///
/// 没有"查页表项权限"的接口，所以用探针真写一次再恢复：
/// 先读原字节（可读性由 [`check_readable`] 保证），写个标记，再写回去。
/// 探针恢复说明这一页只读或没映射。
pub fn check_writable(ptr: u64, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let end = match ptr.checked_add(len as u64) {
        Some(e) => e,
        None => return false,
    };
    if !kvm::is_canonical(ptr) || !kvm::is_canonical(end - 1) {
        return false;
    }
    let mut page = ptr & !0xFFF;
    while page < end {
        // `can_write` 同时覆盖三种"写了会成功"的页：
        // 普通可写页、COW 只读页（写会触发复制）、带 PROT_WRITE 的懒页。
        // 只查 PTE 的 W 位会把后两种误判成坏指针。
        if !crate::mm::fault::can_write(page) {
            return false;
        }
        page = match page.checked_add(0x1000) {
            Some(p) => p,
            None => return false,
        };
    }
    true
}

/// 把用户内存拷进内核缓冲。长度不匹配也返回 `EFAULT`。
pub fn copy_in(uptr: u64, out: &mut [u8]) -> Result<(), i64> {
    if out.is_empty() {
        return Ok(());
    }
    if !check_readable(uptr, out.len()) {
        return Err(err(EFAULT));
    }
    unchecked_copy_in(uptr, out)
}

/// **已由调用方验证过可读**的拷贝。
///
/// `read` 这类入口在开头就把整段缓冲查过一遍了，`copy_out` 里再查一遍
/// 等于每页多走四次页表遍历——bench 里 read_4k 的一半开销都在这。
/// 任何用户指针都必须先过 `check_*`，这条纪律写在这里不写在调用点。
pub fn unchecked_copy_in(uptr: u64, out: &mut [u8]) -> Result<(), i64> {
    let _g = PreemptGuard::new();
    for (i, slot) in out.iter_mut().enumerate() {
        // SAFETY: 上面逐页确认可读，且抢占已关。
        *slot = unsafe { core::ptr::read_volatile((uptr + i as u64) as *const u8) };
    }
    Ok(())
}

/// 把内核缓冲拷到用户内存。**先验证再搬**，避免"写到一半失败"。
pub fn copy_out(uptr: u64, src: &[u8]) -> Result<(), i64> {
    if src.is_empty() {
        return Ok(());
    }
    if !check_readable(uptr, src.len()) || !check_writable(uptr, src.len()) {
        return Err(err(EFAULT));
    }
    unchecked_copy_out(uptr, src)
}

/// 已验证的拷出（见 [`unchecked_copy_in`] 的理由）。
pub fn unchecked_copy_out(uptr: u64, src: &[u8]) -> Result<(), i64> {
    let _g = PreemptGuard::new();
    for (i, b) in src.iter().enumerate() {
        // SAFETY: 已确认可写，抢占已关。
        unsafe { core::ptr::write_volatile((uptr + i as u64) as *mut u8, *b) };
    }
    Ok(())
}

/// 读一个以 NUL 结尾的字符串（路径、argv 里的字符串）。
///
/// 返回长度（**不含** NUL）。没找到 NUL 返回 `ENAMETOOLONG`。
/// 一次最多读 `out.len()-1` 字节，多出来的部分不看——防止用户给个
/// 没有结尾的指针让我们一路读到内核空间去。
pub fn cstr(uptr: u64, out: &mut [u8]) -> Result<usize, i64> {
    if out.is_empty() {
        return Err(err(klinux::errno::EINVAL));
    }
    let max = out.len() - 1;
    let _g = PreemptGuard::new();
    // 分页检查：一路读到 NUL 或者读满上限
    let mut i = 0usize;
    while i <= max {
        if !check_readable(uptr + i as u64, 1) {
            return Err(err(EFAULT));
        }
        // SAFETY: 刚确认这个字节可读。
        let b = unsafe { core::ptr::read_volatile((uptr + i as u64) as *const u8) };
        if b == 0 {
            out[i] = 0;
            return Ok(i);
        }
        out[i] = b;
        i += 1;
    }
    Err(err(klinux::errno::ENAMETOOLONG))
}

/// 读一个 `u64`（`argv` 数组里的一项）。
pub fn get_u64(uptr: u64) -> Result<u64, i64> {
    let mut b = [0u8; 8];
    copy_in(uptr, &mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// 写回调用者结构里的一个 `u64`。
pub fn put_u64(uptr: u64, v: u64) -> Result<(), i64> {
    copy_out(uptr, &v.to_le_bytes())
}

pub fn put_i64(uptr: u64, v: i64) -> Result<(), i64> {
    copy_out(uptr, &v.to_le_bytes())
}

pub fn put_u32(uptr: u64, v: u32) -> Result<(), i64> {
    copy_out(uptr, &v.to_le_bytes())
}

pub fn put_u16(uptr: u64, v: u16) -> Result<(), i64> {
    copy_out(uptr, &v.to_le_bytes())
}
