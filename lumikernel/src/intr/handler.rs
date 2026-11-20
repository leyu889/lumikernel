//! 中断/异常总调度。
//!
//! 这个函数是整个内核里**最不能出错**的一段代码：它在栈可能已经损坏、
//! 堆锁可能被持有、页表可能不一致的状态下运行。所以这里：
//!
//! - 不分配内存（用不了堆）；
//! - 不 panic（panic 会再触发一次异常 → 双重故障 → 三重故障重启）；
//! - 不用 `format!`（走的是 `core::fmt`，本身可能需要栈空间）；
//! - 打印完就停机，绝不在状态不明时"继续跑"。
//!
//! 唯一的例外是**受控故障探针**（[`super::probe`]）：预期之内的故障
//! 会被精确接住并恢复，那是我们主动制造的，不算事故。

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kfault;

use super::frame::{InterruptFrame, halt, read_cr2};
use super::{idt, pic, probe, timer};

/// 防止在处理器内部再次出故障导致无限递归（那样会刷爆串口）。
static IN_HANDLER: AtomicBool = AtomicBool::new(false);

/// 非定时器 IRQ 的累计次数。
static IRQ_OTHER: AtomicU64 = AtomicU64::new(0);

/// 被"只杀进程"处理的用户态异常次数（M9）。内核崩溃计数必须为 0，
/// 但这个计数**允许**大于 0——它代表用户程序的 bug 被隔离住了。
static USER_FAULTS: AtomicU64 = AtomicU64::new(0);

/// 入口桩统一调用这里。名字必须与 `stubs.S` 里的 `call isr_dispatch` 一致。
#[no_mangle]
pub extern "C" fn isr_dispatch(frame: *mut InterruptFrame) {
    if frame.is_null() {
        halt();
    }
    // SAFETY: 指针来自 `isr_common` 压好的栈帧，布局与 `InterruptFrame` 一致
    // （由 `frame.rs` 末尾的编译期断言保证）。
    let f = unsafe { &mut *frame };
    let v = f.vector;

    // ---- 系统调用（int 0x80）----
    // 必须在"外部中断"分支之前拦掉：128 不是 IRQ 向量，落到那边会被误发 EOI。
    if v == idt::SYSCALL_VEC {
        crate::syscall::handle(f);
        return;
    }

    // ---- 核间中断 ----
    // APIC 模式下 EOI 必须写给本地 APIC，写 8259A 不会报错但中断永不结束。
    if v == idt::IPI_VEC {
        crate::arch::smp::note_ipi();
        super::eoi(v);
        return;
    }

    if !kfault::is_exception(v) {
        // ---- 外部中断 ----
        match pic::irq_of(v) {
            Some(0) => {
                // 先发 EOI 再记账：万一紧接着发生任务切换，8259A 已经放行了下一个中断。
                // 顺序反了的话，被切走的那个任务恢复前这段时间定时器是哑的。
                super::eoi(v);
                // 全局时间只由 BSP 推进：多个核各加一次会让"1 秒"变成 1/N 秒。
                if crate::arch::smp::is_bsp() {
                    timer::on_tick();
                    // 轮询网卡 RX：不接中断向量的代价就是 tick 里顺手查一次。
                    // **必须限频**：poll 里有 MMIO 读（VM exit 级别的开销），
                    // 每 tick 一次会把整个系统的 syscall 摊薄 200 倍
                    // （实测 getpid 426 → 145K 周期）。10 个 tick（100ms）
                    // 轮询一次，ping 的 200ms 等待窗口内仍有 2 次机会。
                    static NET_POLL_EVERY: core::sync::atomic::AtomicU32 =
                        core::sync::atomic::AtomicU32::new(0);
                    let n = NET_POLL_EVERY.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    if n % 10 == 0 {
                        crate::net::poll_rx();
                    }
                    // task::on_tick 只能在 BSP 跑：AP 回退到"空转 idle"后
                    // 没有自己的任务，CUR[AP]=0 会把 BSP 主任务错当成
                    // "AP 正在跑的任务"去切——任务状态被两核并发改乱，
                    // 表现是自检在 execve 附近静默卡死（实测）。
                    // AP 跑真调度循环后这里按核打开。
                    crate::task::on_tick(); // 可能在这里切走，不返回本轮
                }
                // 每核自己的 tick 计数：这是"这个核真的在跑"的直接证据
                crate::arch::smp::note_tick();
            }
            Some(_) => {
                IRQ_OTHER.fetch_add(1, Ordering::Relaxed);
                super::eoi(v);
            }
            None => {}
        }
        return;
    }

    // ---- CPU 异常 ----
    // 1) 主动制造、预期之内的故障：精确恢复
    if probe::try_recover(f) {
        return;
    }

    // 2) 用户页缺失/写保护：两类**可以修好**的页错误。
    //
    // 顺序很关键：必须在"杀进程"之前、在 crash 之前。
    // 这里修好的错误对上层完全透明——用户程序只看到自己的写入成功了，
    // 不知道内核刚替它复制了一页。
    if v == kfault::VEC_PF && crate::mm::fault::handle(true, f.error) {
        return;
    }

    // 3) 断点：单字节 int3，推进 rip 后可以安全返回（内核调试原语）
    if v == kfault::VEC_BP {
        f.rip = f.rip.wrapping_add(1);
        kprintln!("[ .. ] 断点命中（已被跳过） rip={:#x}", f.rip.wrapping_sub(1));
        return;
    }

    // 4) 来自 ring3 的异常：**杀掉这个进程**，内核继续跑。
    //
    // 这一条是 M9 的稳定性底线：跑别人的程序时，用户态空指针/越界是
    // 用户程序自己的 bug，不能把整个内核带走。Linux 的做法是投递 SIGSEGV；
    // 我们还没实现信号投递，所以直接以 139（128+11）退出，
    // shell 看到的 `$?` 与 Linux 一致。
    if f.cs & 3 == 3 {
        kprintln!(
            "[ .. ] 用户态异常：vec={} err={:#x} rip={:#x} -> 杀掉该进程，内核继续",
            v,
            f.error,
            f.rip
        );
        kserialln!(
            "       用户程序触发了 {}（错误码 {:#x}），rip={:#x}；只杀进程不停机",
            kfault::name(v),
            f.error,
            f.rip
        );
        if v == kfault::VEC_PF {
            kserialln!("       cr2={:#x}", read_cr2());
        }
        USER_FAULTS.fetch_add(1, Ordering::Relaxed);
        // 系统调用轨迹：用户程序死之前最后做了什么，比寄存器更有用。
        crate::linux::trace::dump(24);
        // 让后续异常还能正常报告（否则会被当成"处理器内部再次异常"）
        IN_HANDLER.store(false, Ordering::SeqCst);
        crate::task::exit_current(139);
    }

    // 5) 内核自己的异常：现场转储后停机
    crash(f);
}

fn crash(f: &InterruptFrame) -> ! {
    if IN_HANDLER.swap(true, Ordering::SeqCst) {
        // 处理器内部又炸了：只打一行，别刷屏
        kprintln!("!!! 异常处理过程中再次异常，放弃打印现场 !!!");
        halt();
    }

    // 先用不依赖 core::fmt 的原始探针把最关键的两个数打出来：
    // 崩溃路径自己也可能有 bug，格式化机正是它最脆弱的一环。
    crate::console::raw_u64(b'V', f.vector);
    crate::console::raw_u64(b'R', f.rip);

    kprintln!();
    kprintln!("=============== KERNEL EXCEPTION ===============");
    kprintln!(
        "vector={} ({})  error={:#x}",
        f.vector,
        kfault::name(f.vector),
        f.error
    );
    kserialln!("内核异常：{}", kfault::desc_zh(f.vector));

    if f.vector == kfault::VEC_PF {
        let cr2 = read_cr2();
        // 把这条地址的四级页表项原样打出来：RSVD/权限类故障只有看到表项值
        // 才知道是"表坏了"还是"标志位算错了"。
        {
            let cr3 = crate::mm::paging::read_cr3();
            let mut t = cr3 & !0xFFF;
            for lvl in (0..4).rev() {
                if t == 0 {
                    kserialln!("  [pt L{}] <上一级为空>", lvl);
                    break;
                }
                let idx = ((cr2 >> (12 + 9 * lvl)) & 0x1FF) as usize;
                // 表项地址是**物理**地址，经直接映射区读。
                let va = crate::mm::paging::phys_to_virt(t + (idx as u64) * 8);
                let ok = crate::mm::paging::translate_in(cr3, va).is_some();
                match if ok { Some(va) } else { None } {
                    Some(va) => {
                        // SAFETY: 上面确认过这个直接映射地址可读。
                        let e = unsafe { core::ptr::read_volatile(va as *const u64) };
                        kserialln!(
                            "  [pt L{}] idx={} entry={:#018x}",
                            lvl, idx, e
                        );
                        if e & 1 == 0 || e & 0x80 != 0 {
                            break;
                        }
                        t = e & 0x000f_ffff_ffff_f000;
                    }
                    None => {
                        kserialln!("  [pt L{}] idx={} <读不到表项>", lvl, idx);
                        break;
                    }
                }
            }
        }
        let mut buf = [0u8; 64];
        let n = kfault::describe_pf(f.error, &mut buf);
        let s = core::str::from_utf8(&buf[..n]).unwrap_or("?");
        kprintln!("  cr2={:#x}", cr2);
        kprintln!("  pf: {}", s);
        kserialln!(
            "  提示：P=0 表示这一页根本没映射；P=1 表示有映射但权限不够（写只读 / 用户访问内核页）"
        );
    }

    kprintln!("  rip={:#018x}  cs={:#x}", f.rip, f.cs);
    kprintln!("  rsp={:#018x}  ss={:#x}", f.rsp, f.ss);

    // 栈顶 12 个字：**返回地址就在这里**。没有它，现场只能定位到
    // "崩在 memcpy 里"；有了它，能直接看到是谁调用的（S5 的核心工具）。
    // 同时在后面给出"相对内核基址的偏移"，方便 objdump -b binary 直接查。
    {
        let cr3 = crate::mm::paging::read_cr3();
        let mut sp = f.rsp;
        for i in 0..12u64 {
            match crate::mm::paging::translate_in(cr3, sp) {
                Some(pa) => {
                    // SAFETY: translate_in 已确认这一页映射在 CR3 指向的地址空间里。
                    let v = unsafe {
                        core::ptr::read_volatile(crate::mm::paging::phys_to_virt(pa) as *const u64)
                    };
                    kserialln!(
                        "  [stack+{:2}] {:#018x}  kernel_off={:#x}",
                        i * 8,
                        v,
                        v.wrapping_sub(0xFFFF_FFFF_8010_0000)
                    );
                }
                None => {
                    kserialln!("  [stack+{:2}] <未映射>", i * 8);
                    break;
                }
            }
            sp = sp.wrapping_add(8);
        }
    }
    kprintln!("  rflags={:#018x}", f.rflags);
    // 每行两个：80 列屏幕上 4 个会折行，折行后最难读的恰恰是寄存器值
    kprintln!("  rax={:#018x} rbx={:#018x}", f.rax, f.rbx);
    kprintln!("  rcx={:#018x} rdx={:#018x}", f.rcx, f.rdx);
    kprintln!("  rsi={:#018x} rdi={:#018x}", f.rsi, f.rdi);
    kprintln!("  rbp={:#018x} r8 ={:#018x}", f.rbp, f.r8);
    kprintln!("  r9 ={:#018x} r10={:#018x}", f.r9, f.r10);
    kprintln!("  r11={:#018x} r12={:#018x}", f.r11, f.r12);
    kprintln!("  r13={:#018x} r14={:#018x}", f.r13, f.r14);
    kprintln!("  r15={:#018x}", f.r15);
    kprintln!("================================================");
    kserialln!("内核已停机（不可恢复的编程错误）。这条信息就是崩溃报告，请据此定位。");

    // 掩掉中断源，避免停机后还被中断反复唤醒
    pic::mask_all();
    halt();
}

/// 被隔离的用户态异常次数（诊断用）。
pub fn user_faults() -> u64 {
    USER_FAULTS.load(Ordering::Relaxed)
}

/// 非定时器 IRQ 计数（诊断用）。
pub fn other_irqs() -> u64 {
    IRQ_OTHER.load(Ordering::Relaxed)
}
