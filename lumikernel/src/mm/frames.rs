//! 物理帧分配（E820 → 位图）
//!
//! 初始化顺序是最容易出错的地方，这里固定成三步，顺序不可颠倒：
//! 1. **全部标记为占用**（`new_all_used`）——默认不信任任何物理内存；
//! 2. 只对 E820 里 `type == 1` 的区间 `mark_free`——只信 BIOS 给的可用区；
//! 3. 显式保留低 1 MiB 与内核映像（含 BSS）——把自己从可用池里摘掉。
//!
//! 反过来做（先全空闲再标占用）只要漏一个保留区，内核就会把自己分配出去，
//! 症状是随机时刻的三重故障，极难查。

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kmem::{Frame, FrameAllocator, PAGE_SIZE};

use super::{DIRECT_MAP_SIZE, KERNEL_VA_BASE};
use crate::e820::E820Entry;

/// 位图能管理的物理地址上限。16 GiB / 4 KiB / 8 = 512 KiB 静态位图。
pub const MAX_PHYS: u64 = 16 << 30;
const MAX_FRAMES: usize = (MAX_PHYS as usize) / PAGE_SIZE;
const BITMAP_BYTES: usize = MAX_FRAMES / 8;

struct Bitmap(UnsafeCell<[u8; BITMAP_BYTES]>);

// SAFETY: 位图只在 `GLOBAL` 的锁保护下通过 `&mut` 访问。
unsafe impl Sync for Bitmap {}

static BITMAP: Bitmap = Bitmap(UnsafeCell::new([0u8; BITMAP_BYTES]));

struct Global {
    lock: AtomicBool,
    alloc: UnsafeCell<Option<FrameAllocator<'static>>>,
}

// SAFETY: 同上，`alloc` 只在持锁期间访问。
unsafe impl Sync for Global {}

static GLOBAL: Global = Global {
    lock: AtomicBool::new(false),
    alloc: UnsafeCell::new(None),
};

extern "C" {
    static __kernel_start: u8;
    static __bss_end: u8;
}

/// 帧分配统计。
#[derive(Clone, Copy)]
pub struct Stats {
    /// 位图覆盖到的总帧数（按 E820 的物理上界算）。
    pub total_frames: usize,
    /// 当前可分配帧数。
    pub free_frames: usize,
    /// E820 报告的可用字节数。
    pub usable_bytes: u64,
    /// 超出直接映射区、暂时没纳入分配（M2 扩直接映射后放开）的字节数。
    pub outside_direct_map: u64,
}

impl Stats {
    pub fn free_mib(&self) -> u64 {
        (self.free_frames as u64 * PAGE_SIZE as u64) >> 20
    }
    pub fn total_mib(&self) -> u64 {
        (self.total_frames as u64 * PAGE_SIZE as u64) >> 20
    }
}

#[inline]
fn align_up(v: u64, a: u64) -> u64 {
    (v + a - 1) & !(a - 1)
}

fn reserve(a: &mut FrameAllocator<'_>, start: u64, end: u64) {
    let f0 = (start / PAGE_SIZE as u64) as usize;
    let f1 = ((end + PAGE_SIZE as u64 - 1) / PAGE_SIZE as u64) as usize;
    for f in f0..f1 {
        a.mark_used(Frame(f));
    }
}

/// 内核映像（物理）区间 `[起始, BSS 结尾)`。
pub fn kernel_phys_range() -> (u64, u64) {
    // SAFETY: __kernel_start / __bss_end 由 linker.ld 定义为内核映像的
    // 起止标记，取它们的地址不读写任何内存，也不会与任何代码竞争。
    let (s, e) = unsafe {
        (
            &__kernel_start as *const u8 as u64 - KERNEL_VA_BASE,
            &__bss_end as *const u8 as u64 - KERNEL_VA_BASE,
        )
    };
    (s, e)
}

/// 按 E820 表初始化帧分配器。返回 `None` 表示表不合法或位图不够大。
pub fn init(entries: &[E820Entry]) -> Option<Stats> {
    // ---- 1. 物理上界 ----
    let mut top = 0u64;
    let mut usable_bytes = 0u64;
    for e in entries {
        if e.typ != crate::e820::TYPE_AVAILABLE {
            continue;
        }
        usable_bytes += e.len;
        let end = e.base.saturating_add(e.len);
        if end > top {
            top = end;
        }
    }
    let top = top.min(MAX_PHYS);
    if top == 0 {
        return None;
    }
    let total_frames = ((top + PAGE_SIZE as u64 - 1) / PAGE_SIZE as u64) as usize;

    // SAFETY: 位图是这块静态内存的唯一所有者，长度与类型都是常量。
    let bitmap: &'static mut [u8] =
        unsafe { core::slice::from_raw_parts_mut(BITMAP.0.get() as *mut u8, BITMAP_BYTES) };

    // ---- 2. 默认全部占用 ----
    let mut a = FrameAllocator::new_all_used(bitmap, total_frames)?;

    // ---- 3. 只释放 E820 可用区（且不越过直接映射区）----
    let mut outside = 0u64;
    for e in entries {
        if e.typ != crate::e820::TYPE_AVAILABLE {
            continue;
        }
        let start = align_up(e.base, PAGE_SIZE as u64);
        let end = e.base.saturating_add(e.len) & !(PAGE_SIZE as u64 - 1);
        if end <= start {
            continue;
        }
        if end > DIRECT_MAP_SIZE {
            outside += end.saturating_sub(start.max(DIRECT_MAP_SIZE));
        }
        let hi = end.min(DIRECT_MAP_SIZE);
        if hi <= start {
            continue;
        }
        let f0 = (start / PAGE_SIZE as u64) as usize;
        let f1 = (hi / PAGE_SIZE as u64) as usize;
        for f in f0..f1 {
            a.mark_free(Frame(f));
        }
    }

    // ---- 4. 保留低 1 MiB（BIOS/实模式页表/E820 表/VGA）与内核自身 ----
    reserve(&mut a, 0, 0x10_0000);
    let (ks, ke) = kernel_phys_range();
    reserve(&mut a, ks, ke);

    let st = Stats {
        total_frames: a.total_frames(),
        free_frames: a.free_frames(),
        usable_bytes,
        outside_direct_map: outside,
    };

    // SAFETY: 持锁后独占写入。
    acquire();
    unsafe { *GLOBAL.alloc.get() = Some(a) };
    release();
    Some(st)
}

fn acquire() {
    while GLOBAL
        .lock
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

fn release() {
    GLOBAL.lock.store(false, Ordering::Release);
}

fn with<R>(f: impl FnOnce(&mut FrameAllocator<'static>) -> R) -> Option<R> {
    // 分配器内部有自旋锁：被抢占后另一个任务再来分配会死等一个永远不释放的锁。
    // 单核上"关抢占"就等价于"关临界区"。
    let _g = crate::task::PreemptGuard::new();
    acquire();
    // SAFETY: 持锁期间独占访问分配器。
    let r = unsafe { (*GLOBAL.alloc.get()).as_mut().map(f) };
    release();
    r
}

/// 分配一帧，返回物理地址。
///
/// 故障注入点：`Point::Frame`。**判定在拿锁之前做**——注入的意义就是
/// "分配器说没有内存了"，不该先占锁再假装失败（那会把锁的竞争也算进去）。
pub fn alloc_frame() -> Option<u64> {
    if crate::qa::fault::deny(kqa::fault::Point::Frame) {
        return None;
    }
    let f = with(|a| a.alloc()).flatten().map(|fr: Frame| fr.addr() as u64);
    if let Some(phys) = f {
        // 体检：这一帧经**直接映射**能不能访问？
        //
        // 正常情况下必须能——boot.S 把前 1 GiB 全铺成 2 MiB 大页。
        // 一旦访问不到，说明页表被踩坏了；这时继续用它只会把现场搅得更烂，
        // 所以立刻报出来（用原子计数限流，别刷屏）。
        let va = crate::mm::paging::phys_to_virt(phys);
        if crate::mm::paging::translate_in(crate::mm::paging::read_cr3(), va).is_none() {
            let n = BAD_ALLOC.fetch_add(1, Ordering::Relaxed);
            if n < 4 {
                kserialln!(
                    "[frames] 分配的帧 {:#x} 在直接映射里不可达（第 {} 次，cr3={:#x}）",
                    phys,
                    n + 1,
                    crate::mm::paging::read_cr3()
                );
            }
        }
    }
    f
}

/// 分配 `n` 个连续帧，返回起始物理地址。
pub fn alloc_frames(n: usize) -> Option<u64> {
    if crate::qa::fault::deny(kqa::fault::Point::Frame) {
        return None;
    }
    with(|a| a.alloc_contiguous(n))
        .flatten()
        .map(|f: Frame| f.addr() as u64)
}

/// 归还从 `phys` 起连续 `n` 帧（[`alloc_frames`] 的逆操作）。
///
/// 逐帧 `mark_free` 而不是"整块还"：帧分配器只按位图记账，
/// 连续块和单个帧在它眼里没有区别，循环还回去就是完整逆操作。
pub fn free_frames(phys: u64, n: usize) {
    for i in 0..n {
        free_frame(phys + (i * PAGE_SIZE) as u64);
    }
}

/// 归还一帧。
/// 释放一个已经在空闲位图里的帧的次数。
///
/// 这个计数**必须恒为 0**：一旦出现，说明某处把同一帧还了两次，
/// 于是它会被分配给两个不同的用途——表现是"两块内存互相覆盖"，
/// 用户态看起来就是随机数据被踩坏。
static DOUBLE_FREE: AtomicU64 = AtomicU64::new(0);

/// 分配到的帧在直接映射里不可达的次数（必须为 0）。
static BAD_ALLOC: AtomicU64 = AtomicU64::new(0);

pub fn bad_allocs() -> u64 {
    BAD_ALLOC.load(Ordering::Relaxed)
}

pub fn double_frees() -> u64 {
    DOUBLE_FREE.load(Ordering::Relaxed)
}

/// 释放一个"页引用"。
///
/// 与 [`free_frame`] 的区别：这是 **COW 之后所有释放路径都应该走**的入口。
/// 它先减引用计数，只有计数归零才真正把帧还给分配器。
///
/// 返回 `true` 表示这次调用真的释放了物理帧。
///
/// 为什么单独开一个函数而不是在 `free_frame` 里做：`free_frame` 还被
/// 页表页（PML4/PD/PT，它们从不共享）和 DMA 缓冲用着，那些地方必须
/// 无条件释放。让两者共用一个入口，等于给"页表页被误共享"留后门。
pub fn put_page(phys: u64) -> bool {
    if crate::mm::cow::decref(phys) {
        free_frame(phys);
        true
    } else {
        false
    }
}

pub fn free_frame(phys: u64) {
    // 清掉引用计数的残留。
    //
    // 不清的话，一个被释放又立刻被分配的页会带着上一轮的计数（甚至 COW 标记），
    // 于是新页在第一写入时被误判成"别人的共享页"——行为上不崩，
    // 但会多复制一次，而且让"引用数"这个诊断指标彻底失去意义。
    crate::mm::cow::clear_all(phys);
    if !is_used(phys) {
        DOUBLE_FREE.fetch_add(1, Ordering::Relaxed);
    }
    let idx = phys as usize / PAGE_SIZE;
    with(|a| a.mark_free(Frame(idx)));
}

/// 该物理地址所在帧是否已被占用（自检用）。
pub fn is_used(phys: u64) -> bool {
    let idx = phys as usize / PAGE_SIZE;
    with(|a| a.frame_is_used(Frame(idx))).unwrap_or(false)
}

/// 当前统计。
pub fn stats() -> Option<Stats> {
    with(|a| Stats {
        total_frames: a.total_frames(),
        free_frames: a.free_frames(),
        usable_bytes: 0,
        outside_direct_map: 0,
    })
}
