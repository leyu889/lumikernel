//! 驱动健康状态：**坏掉的驱动要能被摘掉，而不是把内核一起拖下去**。
//!
//! ## 为什么需要这一层
//!
//! 驱动是内核里唯一直接碰硬件的代码：MMIO 写错一个偏移就是踩别人的地址，
//! DMA 描述符指错一页就是写穿内核内存。现在的现实是——
//!
//! - 真机上，一个坏掉的 SATA 控制器会让每次读盘都超时 5 秒，
//!   而调用方拿到的还是"普通的 I/O 错误"，根本分不清是"这一块坏了"
//!   还是"整个控制器死了"；
//! - 虚拟机上，设备热拔/复位之后驱动还拿着旧的 MMIO 指针。
//!
//! 所以给每个驱动一个**显式状态**：`Up` / `Down(原因)`。
//! 一旦某次操作失败到"设备级"（命令超时、设备不推进队列），
//! 驱动自己把自己标成 `Down`——之后所有调用**立刻返回 `Err`**，
//! 不再去碰硬件。这换来的东西很实在：
//!
//! 1. **失败变快**：卡死的设备从"每次都等 5 秒"变成"立刻 Err"；
//! 2. **故障可归因**：自检和日志能说清"是磁盘坏了，不是文件系统坏了"；
//! 3. **可以恢复**：`recover()` 重新初始化，成功就把状态改回 `Up`。
//!
//! ## 边界（诚实标注）
//!
//! 这**不是**能力隔离：驱动仍然跑在内核态，一次越界写照样能毁掉内核。
//! 真正的隔离需要把驱动搬进独立地址空间（微内核），那是另一个量级的工程。
//! 这一层解决的是"设备失灵"这一类故障——它在真机上比内存越界常见得多。

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::arch::sync::IrqSpinlock;

/// 受管理的驱动。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Driver {
    /// 磁盘控制器（AHCI / ATA）。
    Disk = 0,
    /// 网卡（virtio / e1000）。
    Net = 1,
    /// 控制台输出。
    Console = 2,
}

impl Driver {
    pub const ALL: [Driver; 3] = [Driver::Disk, Driver::Net, Driver::Console];

    pub fn name(self) -> &'static str {
        match self {
            Driver::Disk => "disk",
            Driver::Net => "net",
            Driver::Console => "console",
        }
    }
}

/// 一个驱动的健康记录。
#[derive(Clone, Copy)]
pub struct Health {
    up: bool,
    /// 最近一次失败原因（`&'static str`，不分配内存）。
    reason: &'static str,
    /// 累计失败次数。
    failures: u32,
    /// 累计"降级"次数（从 Up 变 Down 的次数）。
    downs: u32,
}

const HEALTHY: Health = Health {
    up: true,
    reason: "",
    failures: 0,
    downs: 0,
};

static STATE: IrqSpinlock<[Health; 3]> = IrqSpinlock::new([HEALTHY; 3]);
/// 因驱动 Down 而被快速拒绝的调用次数（诊断用）。
static FAST_REJECTS: AtomicU64 = AtomicU64::new(0);
/// 恢复尝试次数。
static RECOVERIES: AtomicU32 = AtomicU32::new(0);

/// 这个驱动当前可用吗？驱动入口第一句就该问它。
pub fn is_up(d: Driver) -> bool {
    let s = STATE.lock();
    let up = s[d as usize].up;
    drop(s);
    if !up {
        FAST_REJECTS.fetch_add(1, Ordering::Relaxed);
    }
    up
}

/// 不看状态直接查（自检用，不会计入 FAST_REJECTS）。
pub fn status_of(d: Driver) -> (bool, &'static str, u32) {
    let s = STATE.lock();
    let h = s[d as usize];
    (h.up, h.reason, h.failures)
}

/// 记一次**非致命**失败（比如某一块读坏了）：只加计数，不降级。
pub fn note_soft_failure(d: Driver) {
    let mut s = STATE.lock();
    s[d as usize].failures = s[d as usize].failures.saturating_add(1);
}

/// 记一次**设备级**失败：把驱动标成 Down，之后所有调用立刻 Err。
///
/// 只在"继续用这个设备已经没有意义"的时候调：命令超时、设备不响应、
/// 队列卡死。单块数据读错不该走这里。
pub fn mark_down(d: Driver, reason: &'static str) {
    let mut s = STATE.lock();
    let h = &mut s[d as usize];
    h.failures = h.failures.saturating_add(1);
    if h.up {
        h.up = false;
        h.downs = h.downs.saturating_add(1);
        h.reason = reason;
    }
    drop(s);
    kserialln!("[drv] {} 降级：{}（之后所有调用立刻失败）", d.name(), reason);
}

/// 标记为可用（初始化成功 / 恢复成功）。
pub fn mark_up(d: Driver) {
    let mut s = STATE.lock();
    let h = &mut s[d as usize];
    h.up = true;
    h.reason = "";
    drop(s);
}

/// 尝试恢复所有 Down 掉的驱动。返回成功恢复的个数。
///
/// 恢复 = "重新走一遍初始化"。重新初始化会重新分配 DMA 缓冲、
/// 重新装载队列——如果设备真的还在，它就能回来。
pub fn recover_all() -> usize {
    let mut n = 0;
    for d in Driver::ALL {
        if status_of(d).0 {
            continue;
        }
        RECOVERIES.fetch_add(1, Ordering::Relaxed);
        let ok = match d {
            Driver::Disk => crate::dev::ahci::init().is_ok() || crate::dev::ata::probe().is_some(),
            Driver::Net => crate::dev::nic::init().is_ok(),
            // 控制台没有"重新初始化"的概念：串口一直在，静音只是软件状态
            Driver::Console => true,
        };
        if ok {
            mark_up(d);
            kserialln!("[drv] {} 恢复成功", d.name());
            n += 1;
        }
    }
    n
}

/// `(快速拒绝次数, 恢复尝试次数)`
pub fn stats() -> (u64, u32) {
    (
        FAST_REJECTS.load(Ordering::Relaxed),
        RECOVERIES.load(Ordering::Relaxed),
    )
}

/// 自检用：把状态机复位回全 Up（避免自检之间互相影响）。
pub fn reset_for_selftest() {
    let mut s = STATE.lock();
    for h in s.iter_mut() {
        *h = HEALTHY;
    }
}
