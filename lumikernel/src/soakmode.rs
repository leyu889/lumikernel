//! 长跑模式（M11 / S3）
//!
//! 自检跑完之后，如果内核是按 `LUMI_SOAK=1` 构建的，就进这里：
//! **无限循环地做混合负载，并按固定间隔采样**。只能关机退出。
//!
//! 为什么要单独一个模式，而不是把负载塞进自检里：
//! 自检是"一次性的断言集合"，跑完就结束了；长跑要的是**连续 uptime**——
//! 定时器一直走、调度器一直在切、每轮都在分配和释放。两者的失败模式不同：
//! 自检抓"算错了"，长跑抓"慢慢漏"。
//!
//! 采样间隔：
//! - 正式（`LUMI_SOAK=1`）：**60000 tick = 10 分钟**，30 天 = 4320 个采样点；
//! - 演示（再加 `LUMI_SOAK_FAST=1`）：100 tick = 1 秒，一份日志里能塞几千个点。
//!
//! 判定不在这里做——采样值交给 `qa::soak`，回归斜率由它算
//! （判定逻辑在 `kqa` 里，宿主机有单测）。

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::intr::{self, timer};
use crate::{fd, frames, fs2, qa, task, vfs};

/// 正式采样的间隔：10 分钟。
const SAMPLE_TICKS_SLOW: u64 = 60_000;

/// 演示采样的间隔：1 秒。
const SAMPLE_TICKS_FAST: u64 = 100;

/// 每多少轮做一次 fork/exec（重活，不能每轮都做——一次约 0.3 ms）。
const EXEC_EVERY: u64 = 2000;

/// 每多少轮做一次 LumiFS 写（含 5 次 FLUSH，约 12 ms）。
const WRITE_EVERY: u64 = 4000;

/// 负对照的泄漏频率：每多少轮漏一帧。
#[cfg(soak_leak)]
const LEAK_EVERY: u64 = 5_000;

/// 一轮混合负载。
///
/// 挑这五件事是有讲究的：它们分别覆盖**帧分配器、内核堆、VFS/文件系统、
/// IPC、进程生命周期**——这五条路径里任何一条漏一点点，长跑都能看出来。
fn round(n: u64) {
    // ① 帧：分配 8 页再还回去
    let mut ps = [0u64; 8];
    for p in ps.iter_mut() {
        *p = frames::alloc_frame().unwrap_or(0);
    }
    for &p in ps.iter() {
        if p != 0 {
            frames::free_frame(p);
        }
    }

    // ② 内核堆：小对象反复申请释放
    {
        let b = Box::new([0x5Au8; 256]);
        core::hint::black_box(&b);
    }
    {
        let mut v: Vec<u64> = Vec::with_capacity(64);
        for i in 0..64u64 {
            v.push(i);
        }
        core::hint::black_box(&v);
    }

    // ③ 文件：打开 → 读 → 关
    if let Ok(f) = fd::open("/hello.txt") {
        let mut buf = [0u8; 128];
        let _ = fd::read(f, &mut buf);
        let _ = fd::close(f);
    }

    // ④ 管道：建 → 写 → 读 → 两侧都关（少关一个就会漏 fd，长跑立刻能抓到）
    if let Ok((r, w)) = fd::pipe_pair() {
        let msg = [0x42u8; 64];
        let _ = fd::write(w, &msg);
        let mut out = [0u8; 64];
        let _ = fd::read(r, &mut out);
        let _ = fd::close(r);
        let _ = fd::close(w);
    }

    // ⑤ 目录：解析路径 + 打开目录快照 + 归还句柄
    if let Ok(o) = vfs::resolve(b"/", true) {
        if let Ok(slot) = vfs::dir_open(b"/", &o) {
            let _ = vfs::dir_len(slot);
            vfs::dir_close(slot);
        }
    }

    // ⑥ 偶尔做一次进程创建（覆盖页表复制、任务槽、退出回收）
    if n % EXEC_EVERY == 0 {
        let argv: Vec<Vec<u8>> = alloc::vec![b"hello".to_vec()];
        let envp: Vec<Vec<u8>> = alloc::vec![b"PATH=/bin".to_vec()];
        // /bin/hello 会往控制台写 96 字节，长跑里把它咽掉，别刷爆串口
        // （静音只挡 `put_byte`，内核自己的 `kserialln!` 照常输出）
        crate::console::set_mute(true);
        if let Ok(tid) = crate::linux::proc::spawn_program(b"/bin/hello", &argv, &envp) {
            let mut n = 0u32;
            loop {
                match task::query(tid) {
                    Some((task::State::Zombie, _, _)) => break,
                    Some(_) => {}
                    None => break,
                }
                n += 1;
                if n > 5000 {
                    break;
                }
                intr::enable();
                crate::intr::frame::halt_once();
                intr::disable();
            }
            let _ = task::reap(tid);
        }
        crate::console::set_mute(false);
    }

    // ⑦ 偶尔写一次可写文件系统（覆盖日志提交 + FLUSH 屏障）
    if n % WRITE_EVERY == 0 {
        if let Ok(ino) = fs2::open_create("soak.dat") {
            let _ = fs2::write_at(ino.ino, 0, &[0xA5u8; 1024]);
        }
        let _ = fs2::sync();
        let _ = fs2::unlink("soak.dat");
    }

    // ⑧ 负对照：故意漏一帧（只有 `LUMI_SOAK_LEAK=1` 的构建才有）
    #[cfg(soak_leak)]
    {
        if n % LEAK_EVERY == 0 {
            core::hint::black_box(frames::alloc_frame());
        }
    }
}

/// 进入长跑：永不返回。
pub fn run() -> ! {
    #[cfg(soak_fast)]
    let interval = SAMPLE_TICKS_FAST;
    #[cfg(not(soak_fast))]
    let interval = SAMPLE_TICKS_SLOW;

    kprintln!("--------------------------------------------------");
    kprintln!("[SOAK] 长跑模式：采样间隔 {} tick（{} s）", interval, interval / 100);
    kprintln!("[SOAK] 负载：帧 / 堆 / 文件 / 管道 / 目录 / fork+exec / LumiFS 写");
    #[cfg(soak_leak)]
    kprintln!("[SOAK] **负对照构建**：每 {} 轮故意漏 1 帧", LEAK_EVERY);
    kprintln!("[SOAK] 退出方式：关机（没有别的办法，这是故意的）");
    kserialln!(
        "[SOAK] start interval={} ticks load=frames+heap+file+pipe+dir+exec+lumifs",
        interval
    );

    qa::soak::reset();
    let mut next = timer::ticks().saturating_add(interval);
    let mut n = 0u64;
    let started = timer::ticks();

    loop {
        round(n);
        n += 1;

        let t = timer::ticks();
        if t >= next {
            let s = qa::soak::record();
            // 机器可解析的一行：`SOAK r=<轮> s=<采样序号> ...`
            kserialln!(
                "SOAK r={} s={} frames={} heap={} tasks={} fds={} t={}",
                n,
                s.round,
                s.free_frames,
                s.heap_used,
                s.tasks_used,
                s.fds_open,
                t - started
            );
            // 每 64 个采样点报一次回归（64 是 `qa::soak::WINDOW` 的窗口长度）
            if s.round % 64 == 0 {
                let r = qa::soak::report();
                let verdict = if r.is_flat() { "ok" } else { "drift" };
                kserialln!(
                    "SOAK-REG samples={} slope_frames={} slope_heap={} slope_fds={} delta_frames={} delta_heap={} rounds={} verdict={}",
                    r.samples,
                    r.free_frames_slope,
                    r.heap_used_slope,
                    r.fds_slope,
                    r.free_frames_delta,
                    r.heap_used_delta,
                    r.rounds,
                    verdict
                );
            }
            // 采样点落后太多（比如被中断拖慢）时不要一次补一串
            next = next.saturating_add(interval);
            if next <= t {
                next = t + interval;
            }
        }
    }
}
