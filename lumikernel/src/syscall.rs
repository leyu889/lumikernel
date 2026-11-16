//! 系统调用（M3）
//!
//! 约定：`rax` = 调用号，`rdi/rsi/rdx` = 参数，返回值写回 `rax`。
//! 入口是 `int 0x80`，门在 `idt.rs` 里设成 **DPL=3**——不然 ring3 执行它是 `#GP`。
//!
//! 这个文件里最重要的一行不是分发，而是 [`write`] 里的用户指针校验：
//! 用户给的指针永远是可疑的。内核不能"假设它是对的然后 memcpy"，
//! 那样一个坏指针就能把内核带走。这里用 M2 造的故障探针逐页试读，
//! 读不通就返回 `-EFAULT`——这正是 Linux 的 `copy_from_user` 在做的事。

use core::sync::atomic::{AtomicU64, Ordering};

use kfault::VEC_PF;

use crate::intr::frame::InterruptFrame;
use crate::intr::probe::{self, ProbeAction};
use crate::task::{self, PreemptGuard};

pub const SYS_PING: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_EXIT: u64 = 2;
pub const SYS_YIELD: u64 = 3;
pub const SYS_GETPID: u64 = 4;
/// M5：文件相关
pub const SYS_OPEN: u64 = 5;
pub const SYS_READ: u64 = 6;
pub const SYS_CLOSE: u64 = 7;
/// M7：创建管道。返回值低 32 位是读端 fd，高 32 位是写端 fd
/// （两个返回值塞一个寄存器：还没做多返回值，这样省一次系统调用）。
pub const SYS_PIPE: u64 = 8;

/// 负返回值（-errno 风格）。
const EFAULT: i64 = -14;
const EINVAL: i64 = -22;
const ENOSYS: i64 = -38;

static CALLS: AtomicU64 = AtomicU64::new(0);
static CALLS_FROM_USER: AtomicU64 = AtomicU64::new(0);
static BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);

pub fn calls() -> u64 {
    CALLS.load(Ordering::Relaxed)
}

/// 来自 CPL3 的调用次数。自检靠它证明"用户态真的进场了"。
pub fn calls_from_user() -> u64 {
    CALLS_FROM_USER.load(Ordering::Relaxed)
}

pub fn bytes_written() -> u64 {
    BYTES_WRITTEN.load(Ordering::Relaxed)
}

/// 系统调用总入口（由 `intr::handler` 在向量 128 时调用）。
pub fn handle(f: &mut InterruptFrame) {
    CALLS.fetch_add(1, Ordering::Relaxed);
    if f.cs & 3 == 3 {
        CALLS_FROM_USER.fetch_add(1, Ordering::Relaxed);
    }

    match f.rax {
        // exit 不返回：直接把自己标成僵尸并切走
        SYS_EXIT => task::exit_current(f.rdi as i64),
        SYS_WRITE => {
            f.rax = write(f.rdi, f.rsi, f.rdx as usize) as u64;
        }
        SYS_OPEN => f.rax = open(f.rdi, f.rsi as usize) as u64,
        SYS_READ => f.rax = read(f.rdi as usize, f.rsi, f.rdx as usize) as u64,
        SYS_CLOSE => {
            f.rax = match crate::fd::close(f.rdi as usize) {
                Ok(()) => 0,
                Err(e) => e as u64,
            }
        }
        SYS_PIPE => f.rax = pipe_pair() as u64,
        SYS_PING => f.rax = 0x1234,
        SYS_GETPID => f.rax = task::current_tid(),
        SYS_YIELD => {
            task::yield_now();
            f.rax = 0;
        }
        _ => f.rax = ENOSYS as u64,
    }
}

/// 校验 `[ptr, ptr+len)` 这段**用户**地址在内核视角下确实可读。
///
/// 只按页校验：页是映射的最小单位，一页可读就意味着整页可读
/// （M3 没有部分映射的页）。每页试读 8 字节，读不通立刻返回 false。
fn user_range_ok(ptr: u64, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let end = match ptr.checked_add(len as u64) {
        Some(e) => e,
        None => return false,
    };
    // 非规范地址（比如 0xdead_beef_dead_beef）会让 CPU 报 #GP 而不是 #PF，
    // 探针只接 #PF，所以这类地址必须在进探针之前就挡掉。
    if !kvm::is_canonical(ptr) || !kvm::is_canonical(end - 1) {
        return false;
    }
    let mut page = ptr & !0xFFF;
    while page < end {
        // 探针返回 recovered=true 说明这一页读不通（缺页）
        let o = probe::run(VEC_PF, page, ProbeAction::Read(page));
        if o.recovered {
            return false;
        }
        page = match page.checked_add(0x1000) {
            Some(p) => p,
            None => return false,
        };
    }
    true
}

/// `write(fd, buf, len)`：写控制台或管道。返回写入字节数或负错误码。
///
/// 先把用户数据拷进内核缓冲，再交给 fd 层。**顺序很重要**：
/// 管道写满时会阻塞，阻塞期间用户内存的状态没人保证；
/// 如果边阻塞边读用户指针，就得在恢复后重新校验——不如一次拷完。
fn write(fd: u64, ptr: u64, len: usize) -> i64 {
    if len == 0 {
        return 0;
    }
    if len > 1024 {
        return EINVAL;
    }
    let mut buf = [0u8; 1024];
    match copy_from_user(ptr, len, &mut buf) {
        Ok(n) if n == len => {}
        Ok(_) => return EFAULT,
        Err(e) => return e,
    }
    match crate::fd::write(fd as usize, &buf[..len]) {
        Ok(n) => {
            if fd <= 2 {
                BYTES_WRITTEN.fetch_add(n as u64, Ordering::Relaxed);
            }
            n as i64
        }
        Err(e) => e,
    }
}

/// 创建管道，返回打包的两个 fd。
fn pipe_pair() -> i64 {
    match crate::fd::pipe_pair() {
        Ok((r, w)) => ((w as i64) << 32) | (r as i64 & 0xFFFF_FFFF),
        Err(e) => e,
    }
}

/// 校验 `[ptr, ptr+len)` 对**用户**可写。
///
/// 手头没有"查页表项权限"的接口，所以用探针真写一次再恢复：
/// 先读出原字节（可读性已由 [`user_range_ok`] 保证），探针写一个标记值，
/// 恢复原值。探针恢复（recovered=true）说明这一页只读或没映射。
///
/// 注意：**只有写成功才恢复**。写失败还去写回去，会在同一个坑上再摔一次，
/// 这次没有探针兜着，就是内核崩溃。
fn user_range_writable(ptr: u64, len: usize) -> bool {
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
        // SAFETY: 调用点已先用 user_range_ok 确认整段可读。
        let old = unsafe { core::ptr::read_volatile(page as *const u8) };
        let o = probe::run(VEC_PF, page, ProbeAction::Write(page, 0xA5));
        if o.recovered {
            return false;
        }
        // SAFETY: 刚才那次写成功了，这一页确实可写。
        unsafe { core::ptr::write_volatile(page as *mut u8, old) };
        page = match page.checked_add(0x1000) {
            Some(p) => p,
            None => return false,
        };
    }
    true
}

/// 把用户内存里的 `len` 字节拷进内核缓冲。失败返回 `EFAULT`。
fn copy_from_user(ptr: u64, len: usize, out: &mut [u8]) -> Result<usize, i64> {
    if len > out.len() {
        return Err(EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    let _g = PreemptGuard::new();
    if !user_range_ok(ptr, len) {
        return Err(EFAULT);
    }
    for i in 0..len {
        // SAFETY: 上面逐页确认过可读，且抢占已关。
        out[i] = unsafe { core::ptr::read_volatile((ptr + i as u64) as *const u8) };
    }
    Ok(len)
}

/// `open(path, len)`：返回 fd（≥3）或负错误码。
fn open(ptr: u64, len: usize) -> i64 {
    if len == 0 || len > 255 {
        return EINVAL;
    }
    let mut buf = [0u8; 256];
    let n = match copy_from_user(ptr, len, &mut buf) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let path = match core::str::from_utf8(&buf[..n]) {
        Ok(s) => s,
        Err(_) => return EINVAL,
    };
    match crate::fd::open(path) {
        Ok(fd) => fd as i64,
        Err(e) => e,
    }
}

/// `read(fd, buf, len)`：返回读到的字节数（0 = EOF）或负错误码。
///
/// 管道读会**阻塞**，所以这里绝不能带着 `PreemptGuard` 调用 fd 层：
/// 抢占计数器是全局的，一个任务带着它睡下去，其他任务就再也抢不动了
/// （表现是系统看着在跑，实际上只在时钟中断里打转）。
/// 校验用户页 → 放开守卫 → 真正读（可能睡）→ 再拿守卫拷回用户内存。
fn read(fd: usize, ptr: u64, len: usize) -> i64 {
    if len == 0 {
        return 0;
    }
    if len > 1024 {
        return EINVAL;
    }
    {
        let _g = PreemptGuard::new();
        // 先确认可写，再读文件/管道：反过来的话，用户给个坏指针就白读一次磁盘，
        // 更糟的是从管道读走的数据没人接收——数据就这么丢了。
        if !copy_precheck_writable(ptr, len) {
            return EFAULT;
        }
    }
    let mut buf = [0u8; 1024];
    let got = match crate::fd::read(fd, &mut buf[..len]) {
        Ok(n) => n,
        Err(e) => return e,
    };
    if got > 0 {
        let _g = PreemptGuard::new();
        for i in 0..got {
            // SAFETY: 上面已确认这段用户内存可写；抢占已关，期间不会被换出。
            unsafe { core::ptr::write_volatile((ptr + i as u64) as *mut u8, buf[i]) }
        }
    }
    got as i64
}

/// 只做可读 + 可写校验，不搬数据。
fn copy_precheck_writable(ptr: u64, len: usize) -> bool {
    if !user_range_ok(ptr, len) {
        return false;
    }
    user_range_writable(ptr, len)
}
