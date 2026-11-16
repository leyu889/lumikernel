//! kbcache —— 写回（write-back）LRU 块缓存
//!
//! 把任意 [`lumifs::Disk`] 包一层，得到一个同样实现 `Disk` 的缓存设备，
//! 用法：`LumiFs::mount(Cached::new(ata_disk, 8))`。
//!
//! ## 为什么写回不会毁掉日志文件系统
//!
//! lumifs 的正确性建立在 `flush()` 是**真屏障**上：日志先落盘、提交记录再落盘、
//! 最后才就地写。写回缓存把这个顺序推迟了，但**没有**破坏它——
//! 因为 lumifs 在每个屏障点都会调 `flush()`，而本 crate 的 `flush()` 会：
//!
//! 1. 把当前所有脏块按 LRU 顺序（最久未用的先写）逐个 `write` 到底层；
//! 2. 等全部成功后再调用底层的 `flush()`，把设备自己的写缓存也推下去。
//!
//! 只有两步都返回 `Ok`，`flush()` 才返回 `Ok`。所以"`flush()` 返回后，之前
//! 所有 `write` 的内容都能从底层设备读到"这条契约依然成立。
//! 反过来说：**中途没有屏障时，本缓存有权任意重排/合并写**，调用方不能依赖
//! 未 `flush` 时的落盘顺序——这本来就是 `Disk` 接口的约定。
//!
//! ## 设计选择（都写在这里，避免"猜"）
//!
//! - **写未命中不先读底层**。`Disk::write` 收到的就是整块内容，是完整的覆盖写，
//!   旧内容没有保留价值。所以写未命中时直接把整块装进空槽/淘汰槽并标脏，
//!   省掉一次读 IO。对应地，`read` 未命中才需要读底层。
//! - **写命中会合并**：同一块在缓存里连续写 N 次，只有 `flush` 时的一次底层写。
//! - **不做写穿**：`write` 永远不碰底层（直通模式除外），这是写回的定义。
//! - **`flush` 失败语义**：某一块写回失败时，该块**保留脏标记**并立刻返回
//!   `Err(())`；已经写成功的块保持干净（它们确实已经交给设备了）。
//! - **`flush` 之后 `dirty() == 0`**（除非中途出错）。
//! - **越界 `lba`**（`>= block_count()`）一律 `Err(())`，不碰缓存、不碰设备、
//!   不改任何统计。
//! - **块大小不是 `BLOCK` 的设备自动退化为直通**：槽位是 `[u8; BLOCK]` 的固定
//!   数组，块大小对不上就没法安全地搬数据，宁可不缓存也不出错。
//! - **缓冲区长度**：`read`/`write` 的 `buf` 至少要有 `BLOCK` 字节，短了返回
//!   `Err(())`；长于 `BLOCK` 时只用前 `BLOCK` 字节（`read` 不会写尾巴）。
//!
//! ## 内存与栈
//!
//! 不用 alloc，槽位是编译期固定数组：`MAX_SLOTS * (BLOCK + 24)` 字节。
//!
//! **`MAX_SLOTS` 必须小**：`Cached::new(...)` 是按值返回的，整个结构体先在
//! 栈上构造再搬走；`LumiFs::mount()` 也按值返回。槽位设成 64（≈66 KiB）时，
//! 在 64 KiB 的内核栈上会直接把栈踩穿——现象是屏幕上冒出一串乱码然后卡死，
//! 和"文件系统写坏了"看起来一模一样。8 个槽位（≈8 KiB）够覆盖一次事务的
//! 全部块，也留得下栈空间。下面有编译期断言把这个上界钉住。
//!
//! 全 crate 不使用 `unsafe`（`#![forbid(unsafe_code)]`），公开 API 不 panic。
//! 唯一的 `panic` 来源是切片长度检查，而所有切片操作前都做了长度校验。

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use lumifs::{Disk, BLOCK};

/// 最大槽位数（编译期上界，避免用堆）。
pub const MAX_SLOTS: usize = 8;

/// 编译期把结构体上界钉死：槽位数组不许超过 16 KiB。
/// 调大 `MAX_SLOTS` 之前先想清楚内核栈够不够（M3 的教训）。
const _: () = assert!(MAX_SLOTS * (BLOCK + 32) <= 16 * 1024);

/// 缓存统计（只增不减，`invalidate()` 也不清零）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// 命中次数（读命中 + 写命中）。
    pub hits: u64,
    /// 未命中次数（读未命中 + 写未命中）。
    pub misses: u64,
    /// 穿过缓存直接写到底层的次数（capacity == 0 或块号越界时）
    pub passthrough: u64,
    /// 因缓存命中被合并掉的写次数
    pub write_hits: u64,
    /// 淘汰并写回的脏块数
    pub writebacks: u64,
    pub evictions: u64,
}

/// 一个缓存槽位。固定 `BLOCK` 字节的数据区，不指向外部内存。
#[derive(Clone, Copy)]
struct Slot {
    valid: bool,
    dirty: bool,
    lba: u64,
    /// 最近一次访问的逻辑时间戳，越大越"新"。
    stamp: u64,
    data: [u8; BLOCK],
}

impl Slot {
    const EMPTY: Slot = Slot {
        valid: false,
        dirty: false,
        lba: 0,
        stamp: 0,
        data: [0u8; BLOCK],
    };
}

/// 写回 LRU 块缓存，自身实现 [`lumifs::Disk`]。
pub struct Cached<D: Disk> {
    dev: D,
    slots: [Slot; MAX_SLOTS],
    /// 实际生效的槽位数，0 表示直通模式。
    capacity: usize,
    /// 逻辑时钟，每次访问自增，用来排 LRU。
    clock: u64,
    dirty_count: usize,
    stats: Stats,
}

impl<D: Disk> Cached<D> {
    /// 新建缓存。
    ///
    /// `capacity` 会被夹到 `1..=MAX_SLOTS`；传 0 表示"完全不缓存"（直通模式，
    /// 用来对比缓存前后行为是否一致——这是最重要的一个测试配置）。
    /// 若设备块大小不是 [`lumifs::BLOCK`]，同样退化为直通模式。
    pub fn new(dev: D, capacity: usize) -> Self {
        let cap = if dev.block_size() != BLOCK {
            0
        } else if capacity == 0 {
            0
        } else {
            capacity.min(MAX_SLOTS)
        };
        Cached {
            dev,
            slots: [Slot::EMPTY; MAX_SLOTS],
            capacity: cap,
            clock: 0,
            dirty_count: 0,
            stats: Stats::default(),
        }
    }

    /// 统计快照。
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// 当前脏块数（`flush()` 之后必须是 0）。
    pub fn dirty(&self) -> usize {
        self.dirty_count
    }

    /// 实际生效的槽位数，0 表示直通（`capacity` 传 0 或块大小不匹配）。
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 丢弃所有缓存（**不写回**）。只在"设备已经换了内容"时用：
    /// 比如整盘重格式化之后，缓存里那份旧数据已经毫无意义。
    pub fn invalidate(&mut self) {
        for s in self.slots.iter_mut() {
            *s = Slot::EMPTY;
        }
        self.dirty_count = 0;
    }

    /// 取回底层设备。内部先 `flush()`，**忽略其错误**之后照样把设备交出去：
    /// 拿回设备说明调用方要直接操作它，留在缓存里的脏块此时已经没有归宿。
    /// 需要保证数据落盘的调用方应当先自己 `flush()` 并检查返回值。
    pub fn into_inner(mut self) -> D {
        let _ = self.flush();
        self.dev
    }

    /* ---------------- 内部辅助 ---------------- */

    fn find(&self, lba: u64) -> Option<usize> {
        self.slots
            .iter()
            .take(self.capacity)
            .position(|s| s.valid && s.lba == lba)
    }

    fn free_slot(&self) -> Option<usize> {
        self.slots
            .iter()
            .take(self.capacity)
            .position(|s| !s.valid)
    }

    /// 最久未用（stamp 最小）的有效槽位。stamp 相同则取下标小的，保证确定性。
    fn lru_victim(&self) -> Option<usize> {
        let mut best: Option<(usize, u64)> = None;
        for (i, s) in self.slots.iter().take(self.capacity).enumerate() {
            if !s.valid {
                continue;
            }
            if best.map_or(true, |(_, st)| s.stamp < st) {
                best = Some((i, s.stamp));
            }
        }
        best.map(|(i, _)| i)
    }

    /// 淘汰一个槽位；脏块先写回。写回失败时保留脏标记并返回 `Err`。
    fn evict(&mut self, idx: usize) -> Result<(), ()> {
        let Some(s) = self.slots.get_mut(idx) else {
            return Err(());
        };
        if !s.valid {
            return Ok(());
        }
        if s.dirty {
            let lba = s.lba;
            self.dev.write(lba, &s.data)?;
            s.dirty = false;
            self.stats.writebacks = self.stats.writebacks.saturating_add(1);
            self.dirty_count = self.dirty_count.saturating_sub(1);
        }
        s.valid = false;
        self.stats.evictions = self.stats.evictions.saturating_add(1);
        Ok(())
    }

    /// 找一个可用的槽位：优先空槽，满了就淘汰 LRU。
    fn alloc_slot(&mut self) -> Result<usize, ()> {
        if let Some(i) = self.free_slot() {
            return Ok(i);
        }
        let Some(idx) = self.lru_victim() else {
            return Err(());
        };
        self.evict(idx)?;
        Ok(idx)
    }

    fn tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }
}

impl<D: Disk> Disk for Cached<D> {
    fn block_size(&self) -> usize {
        self.dev.block_size()
    }

    fn block_count(&self) -> u64 {
        self.dev.block_count()
    }

    /// 命中直接拷贝；未命中先读到栈上临时块，**成功之后**才选槽装入。
    ///
    /// 这个顺序是有意的：底层读失败时缓存一点都没动——既不会留下"半装入"
    /// 的槽，也不会为了一个读不出来的块白白淘汰掉一个脏块。
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        if buf.len() < BLOCK {
            return Err(());
        }
        if lba >= self.dev.block_count() {
            return Err(());
        }
        if self.capacity == 0 {
            self.stats.passthrough = self.stats.passthrough.saturating_add(1);
            return self.dev.read(lba, buf);
        }
        if let Some(idx) = self.find(lba) {
            self.stats.hits = self.stats.hits.saturating_add(1);
            let stamp = self.tick();
            if let Some(s) = self.slots.get_mut(idx) {
                s.stamp = stamp;
                // buf.len() >= BLOCK 已校验，切片不会 panic。
                buf[..BLOCK].copy_from_slice(&s.data);
            }
            return Ok(());
        }

        self.stats.misses = self.stats.misses.saturating_add(1);
        let mut tmp = [0u8; BLOCK];
        self.dev.read(lba, &mut tmp)?;
        let idx = self.alloc_slot()?;
        let stamp = self.tick();
        if let Some(s) = self.slots.get_mut(idx) {
            s.valid = true;
            s.dirty = false;
            s.lba = lba;
            s.stamp = stamp;
            s.data = tmp;
        }
        buf[..BLOCK].copy_from_slice(&tmp);
        Ok(())
    }

    /// 写命中：改槽位内容并标脏；写未命中：整块装入并标脏（**不先读底层**，
    /// 因为整块覆盖写不需要旧内容）。
    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
        if buf.len() < BLOCK {
            return Err(());
        }
        if lba >= self.dev.block_count() {
            return Err(());
        }
        if self.capacity == 0 {
            self.stats.passthrough = self.stats.passthrough.saturating_add(1);
            return self.dev.write(lba, buf);
        }
        let stamp = self.tick();
        if let Some(idx) = self.find(lba) {
            self.stats.hits = self.stats.hits.saturating_add(1);
            self.stats.write_hits = self.stats.write_hits.saturating_add(1);
            if let Some(s) = self.slots.get_mut(idx) {
                s.data.copy_from_slice(&buf[..BLOCK]);
                s.stamp = stamp;
                if !s.dirty {
                    s.dirty = true;
                    self.dirty_count = self.dirty_count.saturating_add(1);
                }
            }
            return Ok(());
        }

        self.stats.misses = self.stats.misses.saturating_add(1);
        let idx = self.alloc_slot()?;
        if let Some(s) = self.slots.get_mut(idx) {
            s.valid = true;
            s.dirty = true;
            s.lba = lba;
            s.stamp = stamp;
            s.data.copy_from_slice(&buf[..BLOCK]);
            self.dirty_count = self.dirty_count.saturating_add(1);
        }
        Ok(())
    }

    /// 屏障：所有脏块按 LRU 顺序写回，全部成功后再调设备自己的 `flush()`。
    ///
    /// 某块写回失败会立刻返回 `Err`，该块保留脏标记（下次还能再试），
    /// 之前写成功的块保持干净。
    fn flush(&mut self) -> Result<(), ()> {
        if self.capacity != 0 {
            // 没有 heap、也不想在栈上开一个"已写过"的位图（64 位够了），
            // 干脆按 (stamp, idx) 字典序一轮一轮挑最小的未写块——天然就是 LRU 顺序。
            let mut last: Option<(u64, usize)> = None;
            loop {
                let mut best: Option<(usize, (u64, usize))> = None;
                for (i, s) in self.slots.iter().take(self.capacity).enumerate() {
                    if !s.valid || !s.dirty {
                        continue;
                    }
                    let key = (s.stamp, i);
                    if let Some(l) = last {
                        if key <= l {
                            continue;
                        }
                    }
                    if best.map_or(true, |(_, k)| key < k) {
                        best = Some((i, key));
                    }
                }
                let Some((idx, key)) = best else { break };
                let Some(s) = self.slots.get_mut(idx) else { break };
                let lba = s.lba;
                self.dev.write(lba, &s.data)?;
                s.dirty = false;
                self.stats.writebacks = self.stats.writebacks.saturating_add(1);
                self.dirty_count = self.dirty_count.saturating_sub(1);
                last = Some(key);
            }
        }
        self.dev.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    /// 内存盘 mock：数据与计数都放在 `Rc` 里，测试可以在缓存外部偷看。
    #[derive(Clone)]
    struct Dev {
        data: Rc<RefCell<Vec<u8>>>,
        reads: Rc<Cell<usize>>,
        writes: Rc<Cell<usize>>,
        flushes: Rc<Cell<usize>>,
        /// 写到底层的 lba 顺序，用来验证 flush 的 LRU 写回顺序。
        log: Rc<RefCell<Vec<u64>>>,
        fail_read: Rc<Cell<Option<u64>>>,
        fail_write: Rc<Cell<Option<u64>>>,
    }

    impl Dev {
        fn new(blocks: usize) -> Self {
            Dev {
                data: Rc::new(RefCell::new(vec![0u8; blocks * BLOCK])),
                reads: Rc::new(Cell::new(0)),
                writes: Rc::new(Cell::new(0)),
                flushes: Rc::new(Cell::new(0)),
                log: Rc::new(RefCell::new(Vec::new())),
                fail_read: Rc::new(Cell::new(None)),
                fail_write: Rc::new(Cell::new(None)),
            }
        }

        /// 取出某块内容（只读，不计入统计）。
        fn block(&self, lba: u64) -> [u8; BLOCK] {
            let mut out = [0u8; BLOCK];
            let d = self.data.borrow();
            let off = (lba as usize) * BLOCK;
            if off + BLOCK <= d.len() {
                out.copy_from_slice(&d[off..off + BLOCK]);
            }
            out
        }

        /// 绕过统计直接改一块（用来预置初始内容）。
        fn poke(&self, lba: u64, val: u8) {
            let mut d = self.data.borrow_mut();
            let off = (lba as usize) * BLOCK;
            for b in d[off..off + BLOCK].iter_mut() {
                *b = val;
            }
        }
    }

    impl Disk for Dev {
        fn block_size(&self) -> usize {
            BLOCK
        }
        fn block_count(&self) -> u64 {
            (self.data.borrow().len() / BLOCK) as u64
        }
        fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
            if lba >= self.block_count() || buf.len() < BLOCK {
                return Err(());
            }
            self.reads.set(self.reads.get() + 1);
            if self.fail_read.get() == Some(lba) {
                return Err(());
            }
            let d = self.data.borrow();
            let off = (lba as usize) * BLOCK;
            buf[..BLOCK].copy_from_slice(&d[off..off + BLOCK]);
            Ok(())
        }
        fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
            if lba >= self.block_count() || buf.len() < BLOCK {
                return Err(());
            }
            self.writes.set(self.writes.get() + 1);
            if self.fail_write.get() == Some(lba) {
                return Err(());
            }
            self.log.borrow_mut().push(lba);
            let mut d = self.data.borrow_mut();
            let off = (lba as usize) * BLOCK;
            d[off..off + BLOCK].copy_from_slice(&buf[..BLOCK]);
            Ok(())
        }
        fn flush(&mut self) -> Result<(), ()> {
            self.flushes.set(self.flushes.get() + 1);
            Ok(())
        }
    }

    fn pattern(seed: u8) -> [u8; BLOCK] {
        let mut b = [0u8; BLOCK];
        for (k, v) in b.iter_mut().enumerate() {
            *v = seed.wrapping_add((k as u8).wrapping_mul(3));
        }
        b
    }

    #[test]
    fn read_your_own_writes() {
        let dev = Dev::new(8);
        let mut c = Cached::new(dev.clone(), 2);
        let p = pattern(0xA1);
        c.write(1, &p).unwrap();
        assert_eq!(dev.writes.get(), 0);
        let mut buf = [0u8; BLOCK];
        c.read(1, &mut buf).unwrap();
        assert_eq!(buf, p);
        // 读己之写完全由缓存满足，没有碰底层
        assert_eq!(dev.reads.get(), 0);
    }

    #[test]
    fn flush_persists_to_device() {
        let dev = Dev::new(8);
        let mut c = Cached::new(dev.clone(), 2);
        let p = pattern(0x5C);
        c.write(3, &p).unwrap();
        assert_eq!(dev.block(3), [0u8; BLOCK]);
        c.flush().unwrap();
        assert_eq!(dev.block(3), p);
        assert_eq!(dev.writes.get(), 1);
        assert_eq!(dev.flushes.get(), 1);
    }

    #[test]
    fn write_back_defers_device_write() {
        let dev = Dev::new(8);
        let mut c = Cached::new(dev.clone(), 4);
        let p = pattern(0x33);
        c.write(2, &p).unwrap();
        // 写回语义的分水岭：没 flush 之前底层一个字节都没动
        assert_eq!(dev.writes.get(), 0);
        assert_eq!(dev.block(2), [0u8; BLOCK]);
        assert_eq!(c.dirty(), 1);
        c.flush().unwrap();
        assert_eq!(dev.writes.get(), 1);
        assert_eq!(dev.block(2), p);
    }

    #[test]
    fn hits_and_misses_are_counted() {
        let dev = Dev::new(8);
        let mut c = Cached::new(dev.clone(), 2);
        let mut buf = [0u8; BLOCK];
        for _ in 0..3 {
            c.read(1, &mut buf).unwrap();
        }
        let s = c.stats();
        assert_eq!((s.hits, s.misses), (2, 1));
        assert_eq!(dev.reads.get(), 1);
    }

    #[test]
    fn lru_evicts_least_recently_used() {
        let dev = Dev::new(8);
        let mut c = Cached::new(dev.clone(), 2);
        let mut buf = [0u8; BLOCK];
        // 访问 A, B, A, C —— B 成了最久未用，应被 C 顶掉
        c.read(0, &mut buf).unwrap();
        c.read(1, &mut buf).unwrap();
        c.read(0, &mut buf).unwrap();
        assert_eq!(c.stats().hits, 1);
        c.read(2, &mut buf).unwrap();
        assert_eq!(c.stats().evictions, 1);
        // A 还在（刚用过），B 不在了
        c.read(0, &mut buf).unwrap();
        assert_eq!(c.stats().hits, 2);
        let before = c.stats().misses;
        c.read(1, &mut buf).unwrap();
        assert_eq!(c.stats().misses, before + 1, "B 应该已经被淘汰");
        assert_eq!(dev.reads.get(), 4);
    }

    #[test]
    fn dirty_eviction_writes_back() {
        let dev = Dev::new(8);
        let mut c = Cached::new(dev.clone(), 1);
        let p = pattern(0x77);
        c.write(5, &p).unwrap();
        assert_eq!(dev.writes.get(), 0);
        let mut buf = [0u8; BLOCK];
        c.read(6, &mut buf).unwrap(); // 容量 1，必然淘汰 5
        let s = c.stats();
        assert_eq!((s.writebacks, s.evictions), (1, 1));
        assert_eq!(dev.block(5), p);
        assert_eq!(c.dirty(), 0);
    }

    #[test]
    fn zero_capacity_is_passthrough() {
        let dev = Dev::new(8);
        let mut c = Cached::new(dev.clone(), 0);
        assert_eq!(c.capacity(), 0);
        let p = pattern(0x11);
        c.write(2, &p).unwrap();
        assert_eq!(dev.writes.get(), 1, "直通模式下写必须立刻到设备");
        assert_eq!(dev.block(2), p);
        assert_eq!(c.dirty(), 0);
        let mut buf = [0u8; BLOCK];
        c.read(2, &mut buf).unwrap();
        assert_eq!(buf, p);
        assert_eq!(dev.reads.get(), 1);
        let s = c.stats();
        assert_eq!((s.passthrough, s.hits, s.misses, s.write_hits), (2, 0, 0, 0));
        c.flush().unwrap();

        // 与带缓存版本对比：最终设备内容与读出的内容必须完全一致
        let dev2 = Dev::new(8);
        let mut c2 = Cached::new(dev2.clone(), 2);
        c2.write(2, &p).unwrap();
        assert_eq!(dev2.writes.get(), 0);
        c2.flush().unwrap();
        let mut buf2 = [0u8; BLOCK];
        c2.read(2, &mut buf2).unwrap();
        assert_eq!(buf2, p);
        assert_eq!(dev2.block(2), p);
        assert_eq!(*dev.data.borrow(), *dev2.data.borrow());
    }

    #[test]
    fn capacity_is_clamped_to_max_slots() {
        let dev = Dev::new(160);
        let mut c = Cached::new(dev.clone(), 9999);
        assert_eq!(c.capacity(), MAX_SLOTS);
        // 连续写超过槽位数的块，不越界、不 panic，脏块数不超过上限
        for lba in 0..(MAX_SLOTS as u64 + 16) {
            c.write(lba, &pattern(lba as u8)).unwrap();
        }
        assert_eq!(c.dirty(), MAX_SLOTS);
        c.flush().unwrap();
        assert_eq!(c.dirty(), 0);
        // 被淘汰的块已经写回，所有块在设备上都应该是最后写入的内容
        for lba in 0..(MAX_SLOTS as u64 + 16) {
            assert_eq!(dev.block(lba), pattern(lba as u8));
        }
    }

    #[test]
    fn out_of_range_lba_is_rejected() {
        let dev = Dev::new(8);
        let mut c = Cached::new(dev.clone(), 4);
        let before = c.stats();
        let mut buf = [0u8; BLOCK];
        for lba in [8u64, 99, u64::MAX] {
            assert_eq!(c.read(lba, &mut buf), Err(()));
            assert_eq!(c.write(lba, &pattern(1)), Err(()));
        }
        assert_eq!(c.stats(), before, "越界不能改变统计");
        assert_eq!(dev.reads.get(), 0);
        assert_eq!(dev.writes.get(), 0);
        assert_eq!(c.dirty(), 0);
        assert_eq!(*dev.data.borrow(), vec![0u8; 8 * BLOCK]);

        // 直通模式同样要挡住越界
        let dev0 = Dev::new(8);
        let mut c0 = Cached::new(dev0.clone(), 0);
        let before0 = c0.stats();
        assert_eq!(c0.read(8, &mut buf), Err(()));
        assert_eq!(c0.write(8, &pattern(1)), Err(()));
        assert_eq!(c0.stats(), before0);
        assert_eq!(dev0.reads.get(), 0);
        assert_eq!(dev0.writes.get(), 0);
    }

    #[test]
    fn dirty_count_is_tracked() {
        let dev = Dev::new(8);
        let mut c = Cached::new(dev.clone(), 2);
        let a = pattern(0x01);
        let b = pattern(0x02);
        c.write(0, &a).unwrap();
        assert_eq!(c.dirty(), 1);
        c.write(0, &b).unwrap(); // 同一块重复写，脏块数不重复计
        assert_eq!(c.dirty(), 1);
        assert_eq!(c.stats().write_hits, 1);
        c.write(1, &a).unwrap();
        assert_eq!(c.dirty(), 2);
        c.flush().unwrap();
        assert_eq!(c.dirty(), 0);
        // flush 按 LRU 顺序写回：块 0 的最后一次写比块 1 早，所以先写 0 再写 1
        assert_eq!(*dev.log.borrow(), vec![0u64, 1u64]);
        assert_eq!(dev.block(0), b);
        assert_eq!(dev.block(1), a);
    }

    #[test]
    fn invalidate_forces_reload() {
        let dev = Dev::new(8);
        dev.poke(4, 0x42);
        let mut c = Cached::new(dev.clone(), 4);
        let mut buf = [0u8; BLOCK];
        c.read(4, &mut buf).unwrap();
        assert_eq!(c.stats().misses, 1);
        c.read(4, &mut buf).unwrap();
        assert_eq!(c.stats().hits, 1);
        c.invalidate();
        assert_eq!(c.dirty(), 0);
        c.read(4, &mut buf).unwrap();
        assert_eq!(c.stats().misses, 2, "invalidate 之后必须重新读");
        assert_eq!(dev.reads.get(), 2);
        assert_eq!(buf, pattern_byte(0x42));
    }

    fn pattern_byte(v: u8) -> [u8; BLOCK] {
        [v; BLOCK]
    }

    #[test]
    fn into_inner_flushes_dirty_blocks() {
        let dev = Dev::new(8);
        let p = pattern(0x9D);
        let mut c = Cached::new(dev.clone(), 2);
        c.write(2, &p).unwrap();
        assert_eq!(dev.block(2), [0u8; BLOCK]);
        let inner = c.into_inner();
        assert_eq!(inner.block(2), p, "into_inner 必须先把脏块落盘");
        assert_eq!(inner.writes.get(), 1);
        assert_eq!(inner.flushes.get(), 1);
    }

    #[test]
    fn read_error_leaves_no_half_installed_slot() {
        let dev = Dev::new(8);
        let mut c = Cached::new(dev.clone(), 2);
        let mut buf = [0u8; BLOCK];
        c.read(0, &mut buf).unwrap();
        c.read(1, &mut buf).unwrap();
        let p = pattern(0x66);
        c.write(0, &p).unwrap(); // 留一个脏块，验证失败读不会顺手淘汰它
        assert_eq!(c.dirty(), 1);

        dev.fail_read.set(Some(2));
        let mut buf = [0xAAu8; BLOCK];
        assert_eq!(c.read(2, &mut buf), Err(()));
        assert_eq!(buf, [0xAAu8; BLOCK], "失败的读不能改调用方的缓冲区");
        assert_eq!(c.dirty(), 1);
        assert_eq!(c.stats().writebacks, 0, "失败的读不该淘汰脏块");
        assert_eq!(dev.writes.get(), 0);

        // 故障消除后重读：仍然是一次 miss，且内容正确（说明没有半装入的坏槽）
        dev.fail_read.set(None);
        let misses = c.stats().misses;
        c.read(2, &mut buf).unwrap();
        assert_eq!(c.stats().misses, misses + 1);
        assert_eq!(buf, [0u8; BLOCK]);
        // 原来那两块仍然命中，没被坏槽顶掉
        assert_eq!(c.read(0, &mut buf), Ok(()));
        assert_eq!(buf, p);
        assert_eq!(c.read(1, &mut buf), Ok(()));
    }

    #[test]
    fn repeated_writes_are_coalesced() {
        let dev = Dev::new(8);
        let mut c = Cached::new(dev.clone(), 4);
        for i in 0..10u8 {
            c.write(3, &pattern(i)).unwrap();
        }
        assert_eq!(dev.writes.get(), 0);
        assert_eq!(dev.reads.get(), 0, "整块覆盖写不需要先读底层");
        assert_eq!(c.dirty(), 1);
        assert_eq!(c.stats().write_hits, 9);
        c.flush().unwrap();
        assert_eq!(dev.writes.get(), 1, "10 次写只应产生 1 次底层写");
        assert_eq!(dev.block(3), pattern(9));
    }

    #[test]
    fn random_differential_against_dumb_model() {
        const BLOCKS: usize = 64;
        let dev = Dev::new(BLOCKS);
        let mut c = Cached::new(dev.clone(), 4);
        let mut model: Vec<[u8; BLOCK]> = vec![[0u8; BLOCK]; BLOCKS];
        let mut rng: u64 = 0x1234_5678_9abc_def0;

        for step in 0..5000usize {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let lba = ((rng >> 33) as usize) % BLOCKS;
            if (rng >> 20) & 1 == 0 {
                let mut buf = [0u8; BLOCK];
                c.read(lba as u64, &mut buf).unwrap();
                assert_eq!(buf, model[lba], "step {} 读 lba {}", step, lba);
            } else {
                let mut buf = [0u8; BLOCK];
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = (lba as u8)
                        .wrapping_mul(31)
                        .wrapping_add((step as u8).wrapping_mul(17))
                        .wrapping_add(i as u8);
                }
                c.write(lba as u64, &buf).unwrap();
                model[lba] = buf;
            }
            if step % 100 == 99 {
                c.flush().unwrap();
                assert_eq!(c.dirty(), 0);
            }
        }
        c.flush().unwrap();
        assert_eq!(c.dirty(), 0);

        // 缓存视图 vs 笨模型
        for (lba, want) in model.iter().enumerate() {
            let mut buf = [0u8; BLOCK];
            c.read(lba as u64, &mut buf).unwrap();
            assert_eq!(&buf, want, "缓存视图 lba {}", lba);
        }
        // 底层设备 vs 笨模型（flush 之后必须完全一致）
        let inner = c.into_inner();
        for (lba, want) in model.iter().enumerate() {
            assert_eq!(&inner.block(lba as u64), want, "设备内容 lba {}", lba);
        }
    }

    #[test]
    fn works_as_a_lumifs_backend() {
        // 真挂一次文件系统：format -> 通过缓存 mount -> 写文件 -> 重新挂载读回。
        let dev = Dev::new(64);
        lumifs::LumiFs::format(dev.clone()).unwrap();
        let mut fs = lumifs::LumiFs::mount(Cached::new(dev.clone(), 4)).unwrap().0;
        let ino = fs.create(b"hello").unwrap().ino;
        let data = [0x5Au8; 64];
        assert_eq!(fs.write_at(ino, 0, &data).unwrap(), 64);
        fs.sync().unwrap();

        let mut out = [0u8; 64];
        assert_eq!(fs.read_at(ino, 0, &mut out).unwrap(), 64);
        assert_eq!(out, data);
        drop(fs);

        // 换一个全新的缓存实例重新挂载，读到的必须是一模一样的内容
        let mut fs2 = lumifs::LumiFs::mount(Cached::new(dev.clone(), 2)).unwrap().0;
        let mut out2 = [0u8; 64];
        assert_eq!(fs2.read_at(ino, 0, &mut out2).unwrap(), 64);
        assert_eq!(out2, data);
    }
}
