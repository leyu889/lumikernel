//! 内核堆（M1）
//!
//! 从帧分配器取一段连续物理内存，经直接映射区交给 `kheap`，
//! 再注册成 `#[global_allocator]`——这样内核里就能用 `Box`/`Vec`/`String`。
//!
//! 为什么不在 `kheap` 里做"按需扩堆"：扩容要能失败、要能统计、要在
//! 内存紧张时有明确策略。把它留在 `mm` 层（M2 接上回收）比塞进分配器干净。

use core::alloc::{GlobalAlloc, Layout};

use kheap::{LockedHeap, Stats};
use kmem::PAGE_SIZE;

use super::{frames, paging, HEAP_BYTES};

/// 真正的堆实现。未初始化时任何分配都会返回空指针（随后 OOM → panic）。
pub static HEAP: LockedHeap = LockedHeap::empty();

/// 全局分配器的抢占守卫包装。
///
/// 为什么必须包一层：`LockedHeap` 内部有自旋锁。任务 A 拿着锁被时钟中断抢占，
/// 任务 B 再去分配就会原地自旋等一把永远不会释放的锁——单核上这就是死锁，
/// 而且不报错、不 panic，表现是"系统突然不动了"。
struct PreemptSafe;

// SAFETY: 只是把调用转给 HEAP，额外做的只有一对原子加减。
unsafe impl GlobalAlloc for PreemptSafe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // 故障注入点：`Point::Heap`。返回空指针是 `GlobalAlloc` 约定的
        // "分配失败"表达方式，上层（`try_reserve` / `Box::try_new` 风格代码）
        // 必须把它变成 `Err` 而不是 panic。
        if crate::qa::fault::deny(kqa::fault::Point::Heap) {
            return core::ptr::null_mut();
        }
        let _g = crate::task::PreemptGuard::new();
        HEAP.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _g = crate::task::PreemptGuard::new();
        HEAP.dealloc(ptr, layout)
    }
}

#[global_allocator]
static GLOBAL_HEAP: PreemptSafe = PreemptSafe;

/// 初始化内核堆，返回堆的物理起始地址。
pub fn init() -> Option<u64> {
    let phys = frames::alloc_frames(HEAP_BYTES / PAGE_SIZE)?;
    let va = paging::phys_to_virt(phys);
    // SAFETY: 这段物理内存刚从帧分配器摘走，之前没有任何代码持有它；
    // 直接映射区是内核访问它的唯一通道，生命周期到内核结束。
    let region = unsafe { core::slice::from_raw_parts_mut(va as *mut u8, HEAP_BYTES) };
    // SAFETY: 同上，region 独占。
    if !(unsafe { HEAP.init(region) }) {
        // 初始化失败就把帧还回去，不留半个堆
        frames::free_frame(phys);
        return None;
    }
    Some(phys)
}

pub fn stats() -> Option<Stats> {
    HEAP.stats()
}

/// 全量审计（内核自检用）。
pub fn audit() -> bool {
    matches!(HEAP.audit(), Some(Ok(_)))
}
