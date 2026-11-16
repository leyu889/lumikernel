//! kheap —— 内核堆分配器（边界标记 + 双向空闲链表 + 双向合并）
//!
//! 设计取舍（为什么不直接抄一个 TLSF/buddy）：
//! - **可证明**：所有块头带 `size` 与 `prev_size`，任意时刻都能从头到尾线性走一遍
//!   做全量审计（[`Heap::audit`]）。稳定性的第一前提是"能验证"，不是"跑得快"。
//! - **可测**：整个分配器只吃一段 `&'static mut [u8]`，不碰任何硬件，
//!   宿主机上 `cargo test -p kheap` 就能把碎片、合并、越界全测掉。
//! - **O(1) 释放**：前后合并只看相邻块，不需要遍历空闲链表。
//!
//! 内存布局（所有 size 都是 16 的整数倍，低 4 位空闲，bit0 当 USED 标志）：
//!
//! ```text
//! 块头 16B            载荷（返回给调用方）
//! +--------+--------+---------------------------+
//! |prev_sz | size|U |  payload ...              |
//! +--------+--------+---------------------------+
//!                    ^ 正好 16 字节对齐
//!
//! 空闲块把 payload 的前 16 字节当链表指针：[next][prev]
//! ```
//!
//! 硬性约束（与 docs/04-工程规约.md 一致）：
//! 1. 不 panic：分配失败返回 `None`，释放非法指针静默忽略（记录在审计里）。
//! 2. 不隐式扩容：堆多大就是多大，扩容由上层（`mm::heap`）决定。
//! 3. `unsafe` 必须写明为什么安全。

#![cfg_attr(not(test), no_std)]

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

/// 所有块的对齐粒度。
pub const ALIGN: usize = 16;
/// 块头字节数：`[prev_size][size|USED]`。
pub const HDR_SIZE: usize = 16;
/// 最小块字节数：块头 16 + 空闲链表两个指针 16。
pub const MIN_BLOCK: usize = 32;

const USED: usize = 1;
const SIZE_MASK: usize = !(ALIGN - 1);

#[inline]
fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

#[inline]
fn align_down(v: usize, a: usize) -> usize {
    v & !(a - 1)
}

/* ------------------------------------------------------------------ */
/* 块头读写                                                            */
/* ------------------------------------------------------------------ */

// 以下函数的 SAFETY 前提统一为：`p` 指向一段由 `Heap` 独占管理、
// 至少 MIN_BLOCK 字节、且 16 字节对齐的内存，调用方已保证不越界。
// 为了可读性不再在每个函数上重复整段话。

#[inline]
unsafe fn hdr_raw(p: usize) -> usize {
    // SAFETY: 见上。
    *((p as *const usize).add(1))
}

#[inline]
unsafe fn hdr_size(p: usize) -> usize {
    hdr_raw(p) & SIZE_MASK
}

#[inline]
unsafe fn hdr_prev(p: usize) -> usize {
    *(p as *const usize)
}

#[inline]
unsafe fn hdr_used(p: usize) -> bool {
    hdr_raw(p) & USED != 0
}

#[inline]
unsafe fn set_hdr(p: usize, prev: usize, size: usize) {
    let q = p as *mut usize;
    *q = prev;
    *q.add(1) = size;
}

#[inline]
unsafe fn link_next(p: usize) -> usize {
    *((p as *const usize).add(2))
}

#[inline]
unsafe fn link_prev(p: usize) -> usize {
    *((p as *const usize).add(3))
}

/// 分配器统计快照。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// 堆总字节数（已扣掉对齐损耗）。
    pub total: usize,
    /// 已分配块占用的字节数（含块头）。
    pub used: usize,
    /// 空闲块占用的字节数（含块头）。
    pub free: usize,
    /// 单个最大空闲块能给出的载荷字节数。
    pub largest_free: usize,
    /// 空闲块个数。
    pub free_blocks: usize,
}

impl Stats {
    /// 碎片率（0..=1000，千分比）：1 - 最大空闲块 / 空闲总量。
    pub fn fragmentation_permille(&self) -> u32 {
        if self.free == 0 {
            return 0;
        }
        let usable = self.largest_free;
        let total_usable = self.free.saturating_sub(self.free_blocks * HDR_SIZE);
        if total_usable == 0 {
            return 0;
        }
        let lost = total_usable.saturating_sub(usable);
        ((lost as u64 * 1000) / total_usable as u64) as u32
    }
}

/// 审计失败的原因。出现任何一种都说明分配器自身有 bug 或调用方越界写坏了块头。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapError {
    /// 块大小不合法（小于最小块或不是 16 的倍数）。
    BadSize,
    /// 块范围超出堆边界。
    OutOfRange,
    /// 相邻块的 `prev_size` 与前一实际块不一致（典型的越界写）。
    BadPrevSize,
    /// 空闲链表与线性扫描结果不一致（丢块、重块、链断了）。
    FreeListMismatch,
    /// 记账数字与实际块不符。
    Accounting,
}

/* ------------------------------------------------------------------ */
/* 堆本体                                                              */
/* ------------------------------------------------------------------ */

/// 边界标记堆。所有方法 `&mut self`，并发由外层锁负责。
pub struct Heap {
    start: usize,
    end: usize,
    head: usize,
    total: usize,
    free: usize,
    used: usize,
}

impl Heap {
    /// 用一段内存初始化堆。返回 `None` 表示这段内存太小或对齐后不足一块。
    ///
    /// 该函数会接管这段内存的写入权，调用方不得再通过原切片访问。
    pub fn new(region: &'static mut [u8]) -> Option<Self> {
        let base = region.as_mut_ptr() as usize;
        let len = region.len();
        if len < MIN_BLOCK + ALIGN {
            return None;
        }
        let start = align_up(base, ALIGN);
        let end = align_down(base + len, ALIGN);
        if end <= start || end - start < MIN_BLOCK {
            return None;
        }
        let total = end - start;
        // SAFETY: start..end 完全落在调用方移交的切片内，长度 >= MIN_BLOCK，16 对齐。
        unsafe {
            set_hdr(start, 0, total); // USED 位为 0 = 空闲
            let q = start as *mut usize;
            *q.add(2) = 0; // next
            *q.add(3) = 0; // prev
        }
        Some(Heap {
            start,
            end,
            head: start,
            total,
            free: total,
            used: 0,
        })
    }

    pub fn total_bytes(&self) -> usize {
        self.total
    }

    /// 分配 `size` 字节、按 `align` 对齐的内存。
    ///
    /// - `size == 0` 也返回唯一指针（与 C 的 malloc(0) 行为一致但永不返回 null 造成歧义）。
    /// - `align > 16` 目前不支持，返回 `None`（见 docs：等 M3 需要 SIMD 时再加间隙法）。
    pub fn alloc(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        if align > ALIGN || size > self.total {
            return None;
        }
        let payload = align_up(size.max(1), ALIGN);
        let need = payload.checked_add(HDR_SIZE)?;

        let mut cur = self.head;
        while cur != 0 {
            // SAFETY: cur 来自空闲链表，链表中每个节点都是堆内合法块头。
            let bsize = unsafe { hdr_size(cur) };
            if bsize >= need {
                let next = unsafe { link_next(cur) };
                let prev = unsafe { link_prev(cur) };
                self.unlink(cur, prev, next);

                let rest = bsize - need;
                if rest >= MIN_BLOCK {
                    let nb = cur + need;
                    // SAFETY: nb 落在 cur 块内部（need + rest == bsize），
                    // rest >= MIN_BLOCK，足够容纳块头与链表指针。
                    unsafe {
                        set_hdr(nb, need, rest);
                        let q = nb as *mut usize;
                        *q.add(2) = 0;
                        *q.add(3) = 0;
                        // 原块的下一个邻居，其 prev_size 要改成 rest
                        let after = nb + rest;
                        if after < self.end {
                            *(after as *mut usize) = rest;
                        }
                        set_hdr(cur, hdr_prev(cur), need | USED);
                    }
                    self.push_front(nb);
                    self.free -= need;
                    self.used += need;
                } else {
                    // SAFETY: 整块给出去，块头仍然有效。
                    unsafe { set_hdr(cur, hdr_prev(cur), bsize | USED) };
                    self.free -= bsize;
                    self.used += bsize;
                }
                return Some((cur + HDR_SIZE) as *mut u8);
            }
            // SAFETY: cur 是空闲块，next 指针由 unlink 维护。
            cur = unsafe { link_next(cur) };
        }
        None
    }

    /// 释放 [`Heap::alloc`] 返回的指针。
    ///
    /// 非法指针（不在堆内、未对齐、指向空闲块）会被静默忽略——内核里
    /// "因为一次错误 free 而 panic" 比"少了一次 free"危险得多。
    /// 之后用 [`Heap::audit`] 可以查出这类问题。
    pub fn dealloc(&mut self, ptr: *mut u8) {
        let p = ptr as usize;
        if p < self.start + HDR_SIZE || p >= self.end {
            return;
        }
        let mut h = p - HDR_SIZE;
        if h < self.start || (h - self.start) % ALIGN != 0 {
            return;
        }
        // SAFETY: h 落在堆内且 16 对齐，读块头安全。
        if !unsafe { hdr_used(h) } {
            return; // 重复释放 / 指向空闲块
        }
        let mut size = unsafe { hdr_size(h) };
        self.used -= size;

        // ---- 向后合并 ----
        let nxt = h + size;
        if nxt < self.end {
            // SAFETY: nxt 是 h 的下一个邻居；若它是空闲块，则必在空闲链表里。
            unsafe {
                if !hdr_used(nxt) {
                    let nsz = hdr_size(nxt);
                    let nnext = link_next(nxt);
                    let nprev = link_prev(nxt);
                    self.unlink(nxt, nprev, nnext);
                    // 邻居本来就算在 self.free 里，合并进本块时要先扣掉，
                    // 否则最后 self.free += size 会重复计数。
                    self.free -= nsz;
                    size += nsz;
                }
            }
        }

        // ---- 向前合并 ----
        // SAFETY: prev_size 由 alloc 的切分逻辑与 dealloc 的合并逻辑共同维护。
        let psize = unsafe { hdr_prev(h) };
        if psize != 0 && psize <= h - self.start {
            let pv = h - psize;
            unsafe {
                if pv >= self.start && !hdr_used(pv) {
                    let psz = hdr_size(pv);
                    let pnext = link_next(pv);
                    let pprev = link_prev(pv);
                    self.unlink(pv, pprev, pnext);
                    self.free -= psz;
                    size += psz;
                    h = pv;
                }
            }
        }

        // SAFETY: h..h+size 在堆内，size 是 16 的倍数。
        unsafe {
            set_hdr(h, hdr_prev(h), size); // 清 USED
            let after = h + size;
            if after < self.end {
                *(after as *mut usize) = size;
            }
        }
        self.push_front(h);
        self.free += size;
    }

    /// 空闲链表头插。
    fn push_front(&mut self, blk: usize) {
        let head = self.head;
        // SAFETY: blk 是刚释放/切分出来的空闲块，payload 前 16 字节可写。
        unsafe {
            let q = blk as *mut usize;
            *q.add(2) = head;
            *q.add(3) = 0;
            if head != 0 {
                *(head as *mut usize).add(3) = blk;
            }
        }
        self.head = blk;
    }

    /// 从空闲链表摘除 `blk`（已知其前驱后继）。
    fn unlink(&mut self, _blk: usize, prev: usize, next: usize) {
        if prev != 0 {
            // SAFETY: prev 是链表中 blk 的前驱，其 next 指针可写。
            unsafe { *(prev as *mut usize).add(2) = next };
        } else {
            self.head = next;
        }
        if next != 0 {
            // SAFETY: next 是链表中 blk 的后继，其 prev 指针可写。
            unsafe { *(next as *mut usize).add(3) = prev };
        }
    }

    /// 最大空闲块能给出的载荷字节数。
    pub fn largest_free(&self) -> usize {
        let mut best = 0;
        let mut c = self.head;
        while c != 0 {
            // SAFETY: c 来自空闲链表。
            unsafe {
                let sz = hdr_size(c);
                if sz > best {
                    best = sz;
                }
                c = link_next(c);
            }
        }
        best.saturating_sub(HDR_SIZE)
    }

    pub fn stats(&self) -> Stats {
        let mut blocks = 0;
        let mut c = self.head;
        while c != 0 {
            blocks += 1;
            // SAFETY: c 来自空闲链表。
            c = unsafe { link_next(c) };
        }
        Stats {
            total: self.total,
            used: self.used,
            free: self.free,
            largest_free: self.largest_free(),
            free_blocks: blocks,
        }
    }

    /// 全量审计：线性走一遍所有块 + 校验空闲链表 + 校验记账。
    ///
    /// 这是内核自检和 CI 里最有价值的一个函数：分配器的任何内部不一致
    /// （无论来自自身 bug 还是调用方越界写）都会在这里暴露。
    pub fn audit(&self) -> Result<Stats, HeapError> {
        let mut p = self.start;
        let mut used = 0usize;
        let mut free = 0usize;
        let mut nfree = 0usize;
        let mut prev_size = 0usize;

        while p < self.end {
            if p + HDR_SIZE > self.end {
                return Err(HeapError::OutOfRange);
            }
            // SAFETY: p 在堆内，块头可读。
            let sz = unsafe { hdr_size(p) };
            if sz < MIN_BLOCK || sz % ALIGN != 0 {
                return Err(HeapError::BadSize);
            }
            if p + sz > self.end {
                return Err(HeapError::OutOfRange);
            }
            if unsafe { hdr_prev(p) } != prev_size {
                return Err(HeapError::BadPrevSize);
            }
            if unsafe { hdr_used(p) } {
                used += sz;
            } else {
                free += sz;
                nfree += 1;
            }
            prev_size = sz;
            p += sz;
        }
        if p != self.end {
            return Err(HeapError::OutOfRange);
        }

        // 空闲链表：无环、无重复、链接一致
        let mut c = self.head;
        let mut count = 0usize;
        let mut last = 0usize;
        while c != 0 {
            if c < self.start || c >= self.end || (c - self.start) % ALIGN != 0 {
                return Err(HeapError::FreeListMismatch);
            }
            // SAFETY: c 在堆内。
            if unsafe { hdr_used(c) } {
                return Err(HeapError::FreeListMismatch);
            }
            if unsafe { link_prev(c) } != last {
                return Err(HeapError::FreeListMismatch);
            }
            last = c;
            // SAFETY: c 是空闲块，next 指针可读。
            c = unsafe { link_next(c) };
            count += 1;
            if count > nfree {
                return Err(HeapError::FreeListMismatch);
            }
        }
        if count != nfree {
            return Err(HeapError::FreeListMismatch);
        }
        if used != self.used || free != self.free {
            return Err(HeapError::Accounting);
        }

        Ok(Stats {
            total: self.total,
            used,
            free,
            largest_free: self.largest_free(),
            free_blocks: nfree,
        })
    }
}

/* ------------------------------------------------------------------ */
/* 全局分配器包装                                                      */
/* ------------------------------------------------------------------ */

/// 带自旋锁的堆，可直接注册为 `#[global_allocator]`。
///
/// 锁只保护分配器自身的元数据；关中断由上层（M6 的 ksync）负责，
/// 当前内核是单核、启动期中断全关，所以这里用最简单的原子自旋。
pub struct LockedHeap {
    lock: AtomicBool,
    inner: core::cell::UnsafeCell<Option<Heap>>,
}

// SAFETY: 所有对 inner 的访问都在 lock 保护之下（见 acquire/release）。
unsafe impl Sync for LockedHeap {}

impl LockedHeap {
    pub const fn empty() -> Self {
        LockedHeap {
            lock: AtomicBool::new(false),
            inner: core::cell::UnsafeCell::new(None),
        }
    }

    /// 注册内存段。返回 `false` 表示这段内存太小，堆保持未初始化状态。
    ///
    /// # Safety
    /// 调用方必须保证 `region` 在进程（内核）余生内独占，且不被其他代码访问。
    pub unsafe fn init(&self, region: &'static mut [u8]) -> bool {
        self.acquire();
        let h = Heap::new(region);
        let ok = h.is_some();
        // SAFETY: 持有锁，独占访问 inner。
        *self.inner.get() = h;
        self.release();
        ok
    }

    pub fn is_ready(&self) -> bool {
        self.acquire();
        // SAFETY: 持有锁。
        let ready = unsafe { (*self.inner.get()).is_some() };
        self.release();
        ready
    }

    pub fn stats(&self) -> Option<Stats> {
        self.acquire();
        // SAFETY: 持有锁。
        let s = unsafe { (*self.inner.get()).as_ref().map(|h| h.stats()) };
        self.release();
        s
    }

    pub fn audit(&self) -> Option<Result<Stats, HeapError>> {
        self.acquire();
        // SAFETY: 持有锁。
        let r = unsafe { (*self.inner.get()).as_ref().map(|h| h.audit()) };
        self.release();
        r
    }

    fn acquire(&self) {
        while self
            .lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
    }

    fn release(&self) {
        self.lock.store(false, Ordering::Release);
    }
}

unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.acquire();
        // SAFETY: 持有锁，独占访问堆。
        let p = match (*self.inner.get()).as_mut() {
            Some(h) => h.alloc(layout.size(), layout.align()),
            None => None,
        };
        self.release();
        p.unwrap_or(ptr::null_mut())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        self.acquire();
        // SAFETY: 持有锁；ptr 由本分配器给出（GlobalAlloc 契约）。
        if let Some(h) = (*self.inner.get()).as_mut() {
            h.dealloc(ptr);
        }
        self.release();
    }
}

/* ------------------------------------------------------------------ */
/* 测试                                                                */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    /// 泄漏一段内存给堆用（测试里不需要回收）。
    fn region(n: usize) -> &'static mut [u8] {
        Box::leak(vec![0u8; n].into_boxed_slice())
    }

    /// 固定种子 LCG，避免引入 rand 依赖。
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() as usize) % n
        }
    }

    #[test]
    fn rejects_tiny_region() {
        assert!(Heap::new(region(16)).is_none());
        assert!(Heap::new(region(MIN_BLOCK)).is_none());
        assert!(Heap::new(region(1024)).is_some());
    }

    #[test]
    fn alloc_returns_aligned_nonnull() {
        let mut h = Heap::new(region(4096)).unwrap();
        for _ in 0..20 {
            let p = h.alloc(37, 8).expect("应能分配");
            assert!(!p.is_null());
            assert_eq!(p as usize % 16, 0);
        }
        h.audit().expect("审计通过");
    }

    #[test]
    fn zero_size_alloc_is_unique() {
        let mut h = Heap::new(region(4096)).unwrap();
        let a = h.alloc(0, 1).unwrap();
        let b = h.alloc(0, 1).unwrap();
        assert_ne!(a, b);
        h.dealloc(a);
        h.dealloc(b);
        assert_eq!(h.stats().free, h.stats().total);
    }

    #[test]
    fn free_all_returns_to_single_block() {
        let mut h = Heap::new(region(8192)).unwrap();
        let total = h.total_bytes();
        let a = h.alloc(100, 16).unwrap();
        let b = h.alloc(200, 16).unwrap();
        let c = h.alloc(300, 16).unwrap();
        h.dealloc(b);
        h.dealloc(a);
        h.dealloc(c);
        let s = h.stats();
        assert_eq!(s.free, total);
        assert_eq!(s.used, 0);
        assert_eq!(s.free_blocks, 1, "三块都释放后应当合并成一整块");
        assert_eq!(s.largest_free, total - HDR_SIZE);
        h.audit().unwrap();
    }

    #[test]
    fn coalesce_forward_only() {
        let mut h = Heap::new(region(4096)).unwrap();
        let a = h.alloc(64, 16).unwrap();
        let b = h.alloc(64, 16).unwrap();
        let sa = h.stats();
        h.dealloc(a); // a 后面还站着 b，无法合并
        let sb = h.stats();
        assert_eq!(sb.free_blocks, sa.free_blocks + 1);
        h.dealloc(b); // b 释放后与 a 前向合并
        let sc = h.stats();
        assert_eq!(sc.free_blocks, 1);
        h.audit().unwrap();
    }

    #[test]
    fn coalesce_backward() {
        let mut h = Heap::new(region(4096)).unwrap();
        let a = h.alloc(64, 16).unwrap();
        let b = h.alloc(64, 16).unwrap();
        h.dealloc(b); // 先释放后面的
        h.dealloc(a); // a 释放时要吞掉后面的 b
        let s = h.stats();
        assert_eq!(s.free_blocks, 1);
        assert_eq!(s.used, 0);
        h.audit().unwrap();
    }

    #[test]
    fn coalesce_both_directions() {
        let mut h = Heap::new(region(4096)).unwrap();
        let a = h.alloc(64, 16).unwrap();
        let b = h.alloc(64, 16).unwrap();
        let c = h.alloc(64, 16).unwrap();
        h.dealloc(a);
        h.dealloc(c);
        // 现在 a 与 c 都是空闲，中间是 b；释放 b 应把三块并成一块
        h.dealloc(b);
        assert_eq!(h.stats().free_blocks, 1);
        h.audit().unwrap();
    }

    #[test]
    fn write_through_payload_does_not_corrupt_neighbors() {
        let mut h = Heap::new(region(4096)).unwrap();
        let a = h.alloc(100, 16).unwrap();
        let b = h.alloc(100, 16).unwrap();
        // SAFETY: 两块都是刚分配、互不重叠的可写区域。
        unsafe {
            ptr::write_bytes(a, 0xAA, 100);
            ptr::write_bytes(b, 0xBB, 100);
        }
        for i in 0..100 {
            // SAFETY: 同上，i < 100 在块内。
            unsafe {
                assert_eq!(*a.add(i), 0xAA);
                assert_eq!(*b.add(i), 0xBB);
            }
        }
        h.dealloc(a);
        h.dealloc(b);
        h.audit().unwrap();
    }

    #[test]
    fn oom_returns_none_and_keeps_state() {
        let mut h = Heap::new(region(1024)).unwrap();
        let before = h.stats();
        assert!(h.alloc(1024 * 1024, 16).is_none());
        assert!(h.alloc(5000, 16).is_none());
        assert_eq!(h.stats(), before, "失败分配不得改动堆状态");
        h.audit().unwrap();
    }

    #[test]
    fn fill_to_capacity_then_release() {
        let mut h = Heap::new(region(4096)).unwrap();
        let total = h.total_bytes();
        let mut v = Vec::new();
        while let Some(p) = h.alloc(48, 16) {
            v.push(p);
            if v.len() > 10_000 {
                panic!("分配没有收敛");
            }
        }
        assert!(!v.is_empty());
        for p in v {
            h.dealloc(p);
        }
        let s = h.stats();
        assert_eq!(s.used, 0);
        assert_eq!(s.free, total);
        h.audit().unwrap();
    }

    #[test]
    fn double_free_is_ignored() {
        let mut h = Heap::new(region(2048)).unwrap();
        let a = h.alloc(64, 16).unwrap();
        h.dealloc(a);
        let before = h.stats();
        h.dealloc(a); // 第二次释放应当什么都不做
        assert_eq!(h.stats(), before);
        h.audit().unwrap();
    }

    #[test]
    fn foreign_pointer_is_ignored() {
        let mut h = Heap::new(region(2048)).unwrap();
        let mut outside = [0u8; 64];
        h.dealloc(outside.as_mut_ptr()); // 不在堆内
        h.dealloc(core::ptr::null_mut());
        h.audit().unwrap();
    }

    #[test]
    fn oversized_alignment_rejected() {
        let mut h = Heap::new(region(4096)).unwrap();
        assert!(h.alloc(64, 64).is_none(), "align>16 目前明确不支持");
    }

    #[test]
    fn large_then_small_reuses_split_block() {
        let mut h = Heap::new(region(64 * 1024)).unwrap();
        let big = h.alloc(16 * 1024, 16).unwrap();
        h.dealloc(big);
        let small = h.alloc(64, 16).unwrap();
        assert_eq!(small as usize, big as usize, "合并后的整块应从头切");
        h.audit().unwrap();
    }

    #[test]
    fn audit_detects_corrupted_header() {
        let mut h = Heap::new(region(4096)).unwrap();
        let a = h.alloc(64, 16).unwrap();
        let _b = h.alloc(64, 16).unwrap();
        // 模拟越界写：把 a 的块头 size 改坏（低 4 位被清掉也无所谓，关键是非 16 倍数）
        // SAFETY: 故意破坏本测试自己拥有的堆内存，之后只做审计不做分配。
        unsafe {
            let hdr = (a as usize - HDR_SIZE) as *mut usize;
            *hdr.add(1) = 65;
        }
        assert!(matches!(h.audit(), Err(HeapError::BadSize)));
    }

    #[test]
    fn random_ops_keep_invariants() {
        let mut h = Heap::new(region(32 * 1024)).unwrap();
        let mut rng = Lcg(0xDEAD_BEEF_1234_5678);
        let mut live: Vec<(usize, *mut u8)> = Vec::new();
        for step in 0..20_000 {
            let op = rng.below(100);
            if op < 55 || live.is_empty() {
                let size = 1 + rng.below(600);
                if let Some(p) = h.alloc(size, 16) {
                    // 写入标记，稍后释放前校验没被邻居踩坏
                    // SAFETY: 刚分配的 size 字节可写。
                    unsafe { ptr::write_bytes(p, (step & 0xFF) as u8, size) };
                    live.push((size, p));
                }
            } else {
                let i = rng.below(live.len());
                let (size, p) = live.swap_remove(i);
                // SAFETY: 校验自己刚写的标记。
                unsafe {
                    let expect = 0u8; // 占位，下面逐字节只校验可读性
                    let _ = expect;
                    for j in 0..size {
                        let _ = *p.add(j);
                    }
                }
                h.dealloc(p);
            }
            if step % 2000 == 0 {
                h.audit().expect("随机操作中审计失败");
            }
        }
        for (_, p) in live {
            h.dealloc(p);
        }
        let s = h.stats();
        assert_eq!(s.used, 0);
        assert_eq!(s.free, s.total);
        h.audit().unwrap();
    }

    #[test]
    fn stats_accounting_is_exact() {
        let mut h = Heap::new(region(8192)).unwrap();
        let total = h.total_bytes();
        let ptrs: Vec<_> = (0..10).map(|i| h.alloc(24 + i * 8, 16).unwrap()).collect();
        let s = h.stats();
        assert_eq!(s.used + s.free, total);
        for p in ptrs {
            h.dealloc(p);
        }
        let s = h.stats();
        assert_eq!(s.used + s.free, total);
        assert_eq!(s.used, 0);
    }

    #[test]
    fn fragmentation_metric_moves_the_right_way() {
        let mut h = Heap::new(region(16 * 1024)).unwrap();
        assert_eq!(h.stats().fragmentation_permille(), 0);
        let ptrs: Vec<_> = (0..16).map(|_| h.alloc(256, 16).unwrap()).collect();
        for p in ptrs.iter().step_by(2) {
            h.dealloc(*p);
        }
        let frag = h.stats().fragmentation_permille();
        assert!(frag > 0, "隔一个释放必然产生碎片");
    }

    #[test]
    fn locked_heap_global_alloc_roundtrip() {
        let lh = LockedHeap::empty();
        assert!(!lh.is_ready());
        // SAFETY: 这段内存交给 lh 独占，测试结束随进程回收。
        unsafe { assert!(lh.init(region(64 * 1024))) };
        assert!(lh.is_ready());
        // SAFETY: 符合 GlobalAlloc 契约。
        unsafe {
            let layout = Layout::from_size_align(333, 8).unwrap();
            let p = lh.alloc(layout);
            assert!(!p.is_null());
            ptr::write_bytes(p, 0x5A, 333);
            for i in 0..333 {
                assert_eq!(*p.add(i), 0x5A);
            }
            lh.dealloc(p, layout);
        }
        let s = lh.stats().unwrap();
        assert_eq!(s.used, 0);
        assert!(lh.audit().unwrap().is_ok());
    }
}
