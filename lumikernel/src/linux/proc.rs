//! 进程类系统调用：`fork` / `execve` / `wait4` / TLS / 时钟
//!
//! 这一段是整个 M9 里最容易出错的地方，因为 `execve` 要在**系统调用内部**
//! 换掉当前进程的地址空间，然后假装"什么都没发生"地返回到新的程序入口。
//! 关键在于三件事的顺序：
//!
//! 1. 先把用户传的 `argv`/`envp` **拷进内核**（切换地址空间之后就再也读不到旧内存了）；
//! 2. 建好新表、装好映像、铺好初始栈，然后才 `write_cr3`；
//! 3. 切完之后**才能**拆旧表——顺序反了就是在自己的脚下抽地板。
//!
//! 为什么不实现真正的信号：busybox 的 `sh` 在没有作业控制（非交互、没有 tty）时
//! 不会依赖信号投递。`rt_sigaction` 一律返回成功、不保存处理函数，
//! 这样 `kill`/`SIGCHLD` 的代码路径不会因为 `ENOSYS` 而走岔。
//! **这是明确的简化，不是"实现了信号"。**

use alloc::vec::Vec;

use klinux::errno as E;
use klinux::flags as F;
use klinux::structs;

use super::uaccess;
use crate::arch::syscall as msr;
use crate::exec;
use crate::fd;
use crate::intr::frame::InterruptFrame;
use crate::mm::paging;
use crate::task;
use crate::vfs;

/* ---------------- fork ---------------- */

/// `fork(2)`：复制地址空间 + 复制 fd 表，返回两次。
///
/// 父进程拿到子进程 tid；子进程拿到 0（`spawn_user_clone` 把 `rax` 设成 0，
/// 它第一次被调度时 `iretq` 直接回到用户态那条 `syscall` 的下一条指令）。
pub fn fork(f: &InterruptFrame) -> i64 {
    let parent_pml4 = task::current_pml4();
    if parent_pml4 == 0 {
        return E::err(E::EINVAL);
    }
    let (pt, _pages) = match paging::clone_user_table(parent_pml4) {
        Some(v) => v,
        None => return E::err(E::ENOMEM),
    };
    let pml4 = pt.root_phys();
    match task::spawn_user_clone(f, pml4) {
        Ok(tid) => tid as i64,
        Err(_) => {
            paging::destroy_user_table(pml4);
            E::err(E::EAGAIN)
        }
    }
}

/* ---------------- execve ---------------- */

pub fn execve(f: &mut InterruptFrame, pathptr: u64, argv_ptr: u64, envp_ptr: u64) -> i64 {
    // ---- 1. 路径 ----
    let (pathbuf, pathlen) = match super::read_path(F::AT_FDCWD as u64, pathptr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let obj = match vfs::resolve(&pathbuf[..pathlen], true) {
        Ok(o) => o,
        Err(e) => return e,
    };
    if obj.is_dir() {
        return E::err(E::EISDIR);
    }
    if obj.size == 0 {
        return E::err(E::ENOEXEC);
    }

    // ---- 2. argv / envp（必须在切地址空间之前拷出来）----
    let argv = match read_strvec(argv_ptr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let envp = match read_strvec(envp_ptr) {
        Ok(v) => v,
        Err(e) => return e,
    };

    // ---- 3. 把 ELF 整个读进内核 ----
    let bytes = match read_file_all(&obj) {
        Ok(b) => b,
        Err(e) => return e,
    };

    // ---- 4. 新地址空间 ----
    let mut pt = match paging::new_user_table() {
        Some(p) => p,
        None => return E::err(E::ENOMEM),
    };
    let loaded = match exec::load_image(&mut pt, &bytes) {
        Ok(l) => l,
        Err(_) => {
            paging::destroy_user_table(pt.root_phys());
            return E::err(E::ENOEXEC);
        }
    };
    drop(bytes); // 1.1 MiB 的内核缓冲尽早还给堆

    if exec::map_stack(&mut pt, exec::USER_STACK_PAGES_LINUX).is_err() {
        paging::destroy_user_table(pt.root_phys());
        return E::err(E::ENOMEM);
    }
    let pml4 = pt.root_phys();

    let rsp = match build_initial_stack(
        pml4,
        loaded.entry,
        loaded.phdr,
        loaded.phnum,
        &argv,
        &envp,
        &pathbuf[..pathlen],
    ) {
        Ok(r) => r,
        Err(e) => {
            paging::destroy_user_table(pml4);
            return e;
        }
    };

    // ---- 5. 提交：换表、换映像 ----
    let old = task::current_pml4();
    task::set_current_pml4(pml4);
    task::set_brk_range(loaded.brk_start, crate::linux::mem::initial_mmap_top());
    task::set_fs_base(0);
    msr::set_fs_msr(0);

    // 先切 CR3 再拆旧表：反过来的话，我们还在旧表上跑的时候它就没了。
    paging::write_cr3(pml4);
    if old != 0 && old != paging::kernel_cr3() && old != pml4 {
        paging::destroy_user_table(old);
    }

    // ---- 6. 让这一帧"返回"到新程序 ----
    f.rip = loaded.entry;
    f.rsp = rsp;
    f.rax = 0; // execve 成功后在新映像里返回 0
    f.rflags |= 0x200; // IF
    f.cs = crate::arch::gdt::USER_CS as u64;
    f.ss = crate::arch::gdt::USER_DS as u64;
    0
}

/// 读 `argv`/`envp`：一个以 NULL 结尾的字符串指针数组。
fn read_strvec(ptr: u64) -> Result<Vec<Vec<u8>>, i64> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    if ptr == 0 {
        return Ok(out);
    }
    // 上限 64 项：`sh -c '...'` 只有几项，给太多没有意义还会被恶意指针拖死
    for i in 0..64u64 {
        let p = uaccess::get_u64(ptr + i * 8)?;
        if p == 0 {
            return Ok(out);
        }
        let mut buf = [0u8; 1024];
        let n = uaccess::cstr(p, &mut buf)?;
        out.push(buf[..n].to_vec());
    }
    Ok(out)
}

/// 把一个文件整个读进内存。
fn read_file_all(o: &vfs::Obj) -> Result<Vec<u8>, i64> {
    let size = o.size as usize;
    if size == 0 || size > 32 * 1024 * 1024 {
        return Err(E::err(E::ENOEXEC));
    }
    let mut out: Vec<u8> = alloc::vec![0u8; size];
    let mut off = 0usize;
    while off < size {
        let n = core::cmp::min(8192, size - off);
        let got = vfs::read(o, off as u64, &mut out[off..off + n])?;
        if got == 0 {
            break;
        }
        off += got;
    }
    out.truncate(off);
    Ok(out)
}

/// 直接起一个 Linux 程序（内核自检用，等价于"没有父进程的 execve"）。
///
/// 为什么不复用 [`execve`]：`execve` 是"在**当前**进程身上换映像"，
/// 而自检需要一个**新**任务，这样主机内核线程还能继续跑着等它。
/// 两者的装载、铺栈逻辑是同一份（[`exec::load_image`] / [`build_initial_stack`]）。
pub fn spawn_program(
    path: &[u8],
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
) -> Result<u64, i64> {
    let obj = match vfs::resolve(path, true) {
        Ok(o) => o,
        Err(e) => return Err(e),
    };
    if obj.is_dir() {
        return Err(E::err(E::EISDIR));
    }
    let bytes = read_file_all(&obj)?;

    let mut pt = match paging::new_user_table() {
        Some(p) => p,
        None => return Err(E::err(E::ENOMEM)),
    };
    let loaded = match exec::load_image(&mut pt, &bytes) {
        Ok(l) => l,
        Err(_) => {
            paging::destroy_user_table(pt.root_phys());
            return Err(E::err(E::ENOEXEC));
        }
    };
    drop(bytes);

    if exec::map_stack(&mut pt, exec::USER_STACK_PAGES_LINUX).is_err() {
        paging::destroy_user_table(pt.root_phys());
        return Err(E::err(E::ENOMEM));
    }
    let pml4 = pt.root_phys();
    let rsp = match build_initial_stack(
        pml4,
        loaded.entry,
        loaded.phdr,
        loaded.phnum,
        argv,
        envp,
        path,
    ) {
        Ok(r) => r,
        Err(e) => {
            paging::destroy_user_table(pml4);
            return Err(e);
        }
    };

    match task::spawn_user_exact(loaded.entry, rsp, pml4) {
        Ok(tid) => {
            task::init_proc_state(tid, loaded.brk_start, crate::linux::mem::initial_mmap_top());
            Ok(tid)
        }
        Err(_) => {
            paging::destroy_user_table(pml4);
            Err(E::err(E::EAGAIN))
        }
    }
}

/* ---------------- 初始栈 ---------------- */

/// 铺好 `argc/argv/envp/auxv`，返回进入程序时的 `%rsp`。
///
/// 布局（地址从低到高）：
/// ```text
///   rsp → argc
///         argv[0..n-1], NULL
///         envp[0..m-1], NULL
///         auxv 键值对..., AT_NULL, 0
///         字符串区（argv/env 的内容、AT_RANDOM 的 16 字节、可执行文件路径）
/// ```
/// musl 的 `_start` 就是从这个形状里取 argc/argv/envp/auxv 的，错一个字段
/// 就会以"段错误"或"什么都没有输出就退出"的形式出现——非常难查，
/// 所以这里每个偏移都单独算，不图省事。
fn build_initial_stack(
    pml4: u64,
    entry: u64,
    phdr: Option<u64>,
    phnum: usize,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    path: &[u8],
) -> Result<u64, i64> {
    let mut sp = exec::USER_STACK_TOP;

    // ---- 字符串区（从高往低放）----
    let mut rnd = [0u8; 16];
    let seed = rdtsc();
    for (i, b) in rnd.iter_mut().enumerate() {
        *b = (seed >> ((i % 8) * 8)) as u8 ^ (i as u8).wrapping_mul(0x5D);
    }
    let random_addr = push_bytes(pml4, &mut sp, &rnd)?;
    let path_addr = push_cstr(pml4, &mut sp, path)?;

    let mut env_addrs = Vec::with_capacity(envp.len());
    for e in envp.iter().rev() {
        env_addrs.push(push_cstr(pml4, &mut sp, e)?);
    }
    env_addrs.reverse();

    let mut arg_addrs = Vec::with_capacity(argv.len());
    for a in argv.iter().rev() {
        arg_addrs.push(push_cstr(pml4, &mut sp, a)?);
    }
    arg_addrs.reverse();

    // ---- auxv 先算好，指针区大小必须按它的**实际**条目数预留 ----
    // 踩过的坑：早期版本把 auxv 项数写成常量 14，实际会写 15 对
    // （PHDR/PHENT/PHNUM + 11 个固定项 + AT_NULL），多出的两格
    // 正好越过预留区、把 argv[0] 字符串盖成 0——表现是 busybox 报
    // ": applet not found"（程序名是空串）。所以这里**先构造向量再算字节数**。
    let mut aux: Vec<(u64, u64)> = Vec::with_capacity(16);
    if let Some(p) = phdr {
        aux.push((F::AT_PHDR, p));
        aux.push((F::AT_PHENT, 56));
        aux.push((F::AT_PHNUM, phnum as u64));
    }
    aux.push((F::AT_PAGESZ, 4096));
    aux.push((F::AT_ENTRY, entry));
    aux.push((F::AT_UID, 0));
    aux.push((F::AT_EUID, 0));
    aux.push((F::AT_GID, 0));
    aux.push((F::AT_EGID, 0));
    aux.push((F::AT_SECURE, 0));
    aux.push((F::AT_RANDOM, random_addr));
    aux.push((F::AT_HWCAP, 0));
    aux.push((F::AT_CLKTCK, 100));
    aux.push((F::AT_EXECFN, path_addr));
    // AT_NULL 由下面的 put 补，不占 aux 的一格

    // ---- 指针区 ----
    let words = 1                 // argc
        + arg_addrs.len() + 1     // argv + NULL
        + env_addrs.len() + 1     // envp + NULL
        + aux.len() * 2 + 2;      // auxv + AT_NULL, 0
    let words_bytes = (words * 8) as u64;
    let stack_bottom = exec::USER_STACK_TOP - (exec::USER_STACK_PAGES_LINUX as u64) * 4096;
    if sp.saturating_sub(words_bytes) <= stack_bottom {
        // 栈放不下这么长的 argv/envp：这是 E2BIG 的标准含义
        return Err(E::err(E::E2BIG));
    }
    sp = sp.saturating_sub(words_bytes) & !0xF;
    let rsp = sp;

    let mut cur = rsp;
    let mut put = |v: u64| -> Result<(), i64> {
        let r = paging::write_in_table(pml4, cur, &v.to_le_bytes()).map_err(|_| E::err(E::EFAULT));
        cur += 8;
        r
    };

    put(argv.len() as u64)?;
    for a in &arg_addrs {
        put(*a)?;
    }
    put(0)?;
    for e in &env_addrs {
        put(*e)?;
    }
    put(0)?;
    for (k, v) in aux.iter() {
        put(*k)?;
        put(*v)?;
    }
    put(F::AT_NULL)?;
    put(0)?;

    // ---- 读回校验 ----
    // 这段只在自检里跑得着，但它值这个钱：初始栈一旦错位，
    // 用户程序的表现是"莫名其妙地什么都不做"，比崩溃难查十倍。
    let mut hdr = [0u8; 8];
    if paging::read_in_table(pml4, rsp, &mut hdr) != Ok(()) {
        return Err(E::err(E::EFAULT));
    }
    let argc_back = u64::from_le_bytes(hdr);
    if argc_back != argv.len() as u64 {
        kserialln!(
            "[dbg] 初始栈 argc 读回 {} != {} (rsp={:#x} pml4={:#x})",
            argc_back, argv.len(), rsp, pml4
        );
        return Err(E::err(E::EINVAL));
    }
    let mut a0 = [0u8; 8];
    if paging::read_in_table(pml4, rsp + 8, &mut a0) != Ok(()) {
        return Err(E::err(E::EFAULT));
    }
    let a0p = u64::from_le_bytes(a0);
    let mut s0 = [0u8; 32];
    if paging::read_in_table(pml4, a0p, &mut s0) != Ok(()) {
        return Err(E::err(E::EFAULT));
    }
    if !s0.starts_with(argv.first().map(|v| v.as_slice()).unwrap_or(b"")) {
        kserialln!(
            "[dbg] argv[0] 指针 {:#x} 指向 {:?}（期望 {:?}）",
            a0p,
            core::str::from_utf8(&s0[..4]).unwrap_or("?"),
            core::str::from_utf8(argv.first().map(|v| v.as_slice()).unwrap_or(b"")).unwrap_or("?")
        );
        // argv[0] 指针指错了地方：与其让用户程序表现奇怪，不如直接报错
        return Err(E::err(E::EINVAL));
    }

    Ok(rsp)
}

/// 把一个字节串（不加 NUL）压进用户栈，返回它的地址。
fn push_bytes(pml4: u64, sp: &mut u64, data: &[u8]) -> Result<u64, i64> {
    *sp = sp
        .checked_sub(data.len() as u64)
        .ok_or(E::err(E::ENOMEM))?;
    paging::write_in_table(pml4, *sp, data).map_err(|_| E::err(E::ENOMEM))?;
    Ok(*sp)
}

/// 压一个以 NUL 结尾的字符串，返回字符串首地址。
fn push_cstr(pml4: u64, sp: &mut u64, data: &[u8]) -> Result<u64, i64> {
    *sp = sp.checked_sub(data.len() as u64 + 1).ok_or(E::err(E::ENOMEM))?;
    paging::write_in_table(pml4, *sp, data).map_err(|_| E::err(E::ENOMEM))?;
    paging::write_in_table(pml4, *sp + data.len() as u64, &[0]).map_err(|_| E::err(E::ENOMEM))?;
    Ok(*sp)
}

fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: rdtsc 只读时间戳计数器。
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | lo as u64
}

/* ---------------- wait4 ---------------- */

pub fn wait4(pid: i64, status: u64, options: u32, rusage: u64) -> i64 {
    if options & F::WNOHANG != 0 {
        return match task::zombie_child(task::current_tid(), pid) {
            Some((tid, _)) => finish(tid, status, rusage),
            None => 0, // 没有已退出的子进程，但不是错误
        };
    }
    match task::wait_child(pid) {
        Some((tid, _)) => finish(tid, status, rusage),
        None => E::err(E::ECHILD),
    }
}

/// 收尾：写 status/rusage、回收资源、返回子进程号。
fn finish(tid: u64, status: u64, rusage: u64) -> i64 {
    let code = match task::query(tid) {
        Some((_, c, _)) => c,
        None => return E::err(E::ECHILD),
    };
    let (_, _, ok) = task::reap(tid);
    if !ok {
        return E::err(E::ECHILD);
    }
    if status != 0 {
        // 正常退出：低 8 位是退出码，放在第 8..16 位（和 Linux 一致）
        let st = ((code as i32) & 0xFF) << 8;
        if let Err(e) = uaccess::copy_out(status, &st.to_le_bytes()) {
            return e;
        }
    }
    if rusage != 0 {
        let zero = [0u8; structs::RUSAGE_BYTES];
        if let Err(e) = uaccess::copy_out(rusage, &zero) {
            return e;
        }
    }
    tid as i64
}

/* ---------------- 其它 ---------------- */

pub fn arch_prctl(code: u64, addr: u64, _extra: u64) -> i64 {
    match code {
        F::ARCH_SET_FS => {
            if addr != 0 && !kvm::is_canonical(addr) {
                return E::err(E::EPERM);
            }
            task::set_fs_base(addr);
            msr::set_fs_msr(addr);
            0
        }
        F::ARCH_GET_FS => match uaccess::put_u64(addr, task::fs_base()) {
            Ok(()) => 0,
            Err(e) => e,
        },
        F::ARCH_SET_GS | F::ARCH_GET_GS => E::err(E::ENOSYS),
        _ => E::err(E::EINVAL),
    }
}

pub fn clock_gettime(clk: u64, ts: u64) -> i64 {
    if clk != F::CLOCK_REALTIME && clk != F::CLOCK_MONOTONIC {
        return E::err(E::EINVAL);
    }
    let ms = crate::intr::timer::millis();
    let mut b = [0u8; 16];
    if !structs::write_timespec(&mut b, (ms / 1000) as i64, ((ms % 1000) * 1_000_000) as i64) {
        return E::err(E::EINVAL);
    }
    match uaccess::copy_out(ts, &b) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub fn getrandom(buf: u64, len: usize) -> i64 {
    let n = core::cmp::min(len, 256);
    let mut b = [0u8; 256];
    let mut s = rdtsc();
    for slot in b.iter_mut().take(n) {
        // xorshift：不追求密码学强度，只要"每次不一样"就够了
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        *slot = s as u8;
    }
    match uaccess::copy_out(buf, &b[..n]) {
        Ok(()) => n as i64,
        Err(e) => e,
    }
}

/// `getrlimit` / `prlimit64`：一律回 "无限"。
pub fn rlimit(_newp: u64, oldp: u64) -> i64 {
    if oldp == 0 {
        return 0;
    }
    let mut b = [0u8; 16];
    b[0..8].copy_from_slice(&u64::MAX.to_le_bytes()); // rlim_cur
    b[8..16].copy_from_slice(&u64::MAX.to_le_bytes()); // rlim_max
    match uaccess::copy_out(oldp, &b) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

/// 进程退出前的清理钩子：目录句柄是全局表，必须显式归还。
pub fn on_exit() {
    vfs::dir_close_all();
    let _ = fd::open_count();
}
