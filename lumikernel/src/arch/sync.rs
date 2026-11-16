//! 中断安全的自旋锁（M4）
//!
//! 单核时代"关中断"就等于"互斥"，因为不存在另一个执行流同时改同一份数据。
//! 多核之后这条不成立了：CPU0 在临界区里，CPU1 照样能冲进来。
//!
//! 所以锁必须同时做两件事：
//! 1. **原子地抢锁**（挡住别的 CPU）；
//! 2. **关本核中断**（挡住本核的中断处理程序——否则中断在持锁期间进来，
//!    处理器再去拿同一把锁，就是自己等自己，永久死锁）。
//!
//! 两个都做了才叫"中断安全"。少一个都是死锁，而且表现都是"系统突然不动了"。

// 现在内核里还没有需要跨核互斥的共享结构（每核状态全走原子量），
// 所以这一整套暂时没人调用。多核调度一上来，运行队列和 fd 表就要用它，
// 那时删掉这行即可。不为了"看起来用上了"而硬塞一个假调用点。
#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub struct IrqSpinlock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: 互斥由 `locked` 保证；`T: Send` 才允许跨执行流共享。
unsafe impl<T: Send> Sync for IrqSpinlock<T> {}
// SAFETY: 同 `Sync`。
unsafe impl<T: Send> Send for IrqSpinlock<T> {}

impl<T> IrqSpinlock<T> {
    pub const fn new(v: T) -> Self {
        IrqSpinlock {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(v),
        }
    }

    /// 抢锁并关中断。返回的守卫在 drop 时放锁并恢复中断状态。
    pub fn lock(&self) -> IrqGuard<'_, T> {
        let if_was_on = disable_irq_save();
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // 自旋时也让出流水线，别把另一个核的总线事务压死
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        IrqGuard {
            lock: self,
            if_was_on,
        }
    }

    /// 不做任何同步，直接读内部值的副本（只在启动阶段、已知无并发时用）。
    ///
    /// # Safety
    /// 调用方必须保证此刻没有其他 CPU 或中断上下文正在改这个值。
    pub unsafe fn get_unchecked(&self) -> &T {
        &*self.data.get()
    }
}

pub struct IrqGuard<'a, T> {
    lock: &'a IrqSpinlock<T>,
    if_was_on: bool,
}

impl<T> Deref for IrqGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: 持有守卫期间独占访问。
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for IrqGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: 同上。
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for IrqGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        restore_irq(self.if_was_on);
    }
}

/// 关中断并返回原状态（true = 原来开着）。
#[inline]
pub fn disable_irq_save() -> bool {
    let flags: u64;
    // SAFETY: pushfq/pop 只读 RFLAGS；cli 清 IF。两者都不碰内存。
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {0}",
            "cli",
            out(reg) flags,
            options(nomem, preserves_flags)
        )
    };
    flags & (1 << 9) != 0
}

/// 按保存的状态恢复中断开关。
#[inline]
pub fn restore_irq(was_on: bool) {
    if was_on {
        // SAFETY: 只在原来开着时才重新开启，语义与 x86 的 sti 一致。
        unsafe { core::arch::asm!("sti", options(nomem, nostack, preserves_flags)) };
    }
}
