//! kslab —— 定长对象 slab 分配器（纯逻辑，页来源由 trait 注入）
//!
//! ## 为什么是 slab，而不是直接复用 [`kheap`]
//!
//! 内核里绝大多数分配请求集中在 16..=2048 这一小撮尺寸上（进程控制块扩展、
//! inode/dentry 缓存、bio、页表页……）。这些请求用通用堆处理有两个毛病：
//! 一是会产生外部碎片，二是每次分配都要走一遍空闲链表查找。
//! 定长槽位把这两个问题一次性去掉：**每页只服务一个尺寸**，分配永远是
//! "摘空闲链表头" 的 O(1) 操作，并且永远不会产生外部碎片（只有页内尾部
//! 的固定浪费）。
//!
//! ## 设计取舍
//!
//! 1. **页头放在页首（64 字节），不做独立元数据区。**
//!    反查一个对象属于哪个 slab 页，只需要 `addr & !0xFFF` 取页基址，
//!    然后读页头里的 `magic` / `obj_size` / `first_obj_off` / `class`。
//!    不需要任何"地址 → 页"的辅助映射表，省掉一张可能比数据本身还大的
//!    区间树。代价是：`free()` 必须直接解引用页基址，因此**要求对象地址落在
//!    内核直接映射区**（任何物理页都有对应的虚拟地址且已映射）。这是 x86_64
//!    宏内核的常规前提，测试里用一块真实 arena 内存来满足同一前提。
//!
//! 2. **空闲链表是侵入式的**：下一个空闲对象的地址直接写进空闲对象自己的
//!    头 8 字节。槽位最小 16 字节 ≥ 8 字节指针，所以不需要额外空间；
//!    代价是被释放的对象内容会被覆盖（这是所有侵入式 slab 的共同语义，
//!    调用方不得在 free 之后读取对象）。
//!
//! 3. **双重释放用页内线性查找检测**。每页对象数 ≤ 252（见下），在空闲
//!    链表里扫一遍的代价可以接受，而且**不改动任何状态**就能返回错误。
//!    换成"每对象一个状态位"的做法需要在页头外再找地方放位图，反而更复杂。
//!
//! 4. **每类两条页链**：
//!    - `next_all`：该类所有页（全量审计 [`Slab::audit`] 和 `free` 的归属
//!      校验要靠它，满页也必须能查到）；
//!    - `next_partial`：该类"还有空闲槽位"的页，`alloc` 的快路径。
//!      页满了就摘出去，`free` 之后又有空位就挂回来。
//!
//! 5. **暂不做整页回收（页还给 [`PageSource`]）**。理由：要安全归还一页，
//!    必须先能证明"该页所有对象都已释放 且 没有任何在途引用（例如某个 CPU
//!    的本地缓存、或者刚被摘下来还没初始化完的对象）"。M2 阶段没有 RCU /
//!    无锁读取的同步原语，贸然回收会被 UAF 咬。因此这里明确把整页回收
//!    推迟到 M3，届时配合"页级引用计数 + 全局回收批次"一起做。
//!    当前 `free` 只把槽位挂回页内空闲链表，`stats().pages` 只增不减。
//!
//! 6. **所有指针访问走 `read_volatile` / `write_volatile`**。对象的头 8 字节
//!    在不同时刻被当作"用户数据"和"链表指针"两种身份使用，属于典型的
//!    类型双关（type punning）；用 volatile 访问把这类别名读写钉死在
//!    具体的内存操作上，避免优化器基于别名假设做重排或删除。
//!
//! ## 页内布局
//!
//! ```text
//! 页基址 + 0                                              + 4096
//! +--------------------------+---------+---------+-----+--------
//! | 页头 64B                 | 对象 0  | 对象 1  | ... | 页尾余量
//! +--------------------------+---------+---------+-----+--------
//!                            ^ first_obj_off = 64（16 字节对齐）
//! ```
//!
//! 页头 64 字节布局（全部小端，偏移单位：字节）：
//!
//! ```text
//!  0  magic:u32            4  obj_size:u32
//!  8  next_all:u64        16  next_partial:u64
//! 24  free_head:u64       32  free_count:u32
//! 36  total_objects:u32   40  used_count:u32
//! 44  first_obj_off:u32   48  class:u32
//! 52  保留:u32            56  保留:u64
//! ```
//!
//! 每一档每页的对象数（`(4096 - 64) / obj_size`）：
//! 16→252、32→126、64→63、128→31、256→15、512→7、1024→3、2048→1。
//! 最大 252 < 256，所以 `audit` 用 `[u64; 4]` 的栈上位图就能做重复检测。
//!
//! ## 硬性约束（与工程规约一致）
//!
//! - 不 panic：公开 API 里没有 `unwrap`/`expect`/`assert!`，数组一律用
//!   `get`/`get_mut` 访问，算术一律饱和或先判后减。
//! - 不隐式扩容：页从哪来由调用方注入的 [`PageSource`] 决定。
//! - 每个 `unsafe` 块上方都写明 `// SAFETY:`。

#![cfg_attr(not(test), no_std)]

use core::ptr;

/// 页大小。slab 页必须与物理帧同尺寸，才能一页一帧地换。
pub const PAGE_SIZE: usize = 4096;

/// 尺寸档位数量。
pub const NCLASSES: usize = 8;

/// 尺寸档位表。每档都能被 16 整除，保证槽位 16 字节对齐。
pub const CLASS_SIZES: [usize; NCLASSES] = [16, 32, 64, 128, 256, 512, 1024, 2048];

/// 所有槽位的对齐粒度。
const ALIGN: usize = 16;

/// 页头字节数。取 64（一个 cache line）：既让 `first_obj_off` 天然 16 对齐，
/// 又避免对象头和页头共享同一行导致的伪共享。
const HDR_SIZE: usize = 64;

/// 页头魔数："SLAB"。
const MAGIC: u32 = 0x534C_4142;

/// 页头字段偏移。
const OFF_MAGIC: u64 = 0;
const OFF_OBJ_SIZE: u64 = 4;
const OFF_NEXT_ALL: u64 = 8;
const OFF_NEXT_PARTIAL: u64 = 16;
const OFF_FREE_HEAD: u64 = 24;
const OFF_FREE_COUNT: u64 = 32;
const OFF_TOTAL: u64 = 36;
const OFF_USED_COUNT: u64 = 40;
const OFF_FIRST_OFF: u64 = 44;
const OFF_CLASS: u64 = 48;

/// 页掩码，用来从对象地址反推页基址。
const PAGE_MASK: u64 = !(PAGE_SIZE as u64 - 1);

/// 空指针哨兵。0 号页永远不会被分给 slab（低地址在内核里是保留的）。
const NIL: u64 = 0;

// ---------------------------------------------------------------------------
// 底层内存访问
//
// 下面四个函数是整个 crate 里唯一的 unsafe 入口。它们之所以可以是安全函数，
// 是因为调用方（`alloc`/`free`/`audit`）在调用前已经完成了地址合法性校验：
// 只对 `PageSource` 交付过、或从页头里读出来的页内地址做访问。这与 `free()`
// 依赖直接映射区的设计前提是一致的，详见模块文档第 1 条。
// ---------------------------------------------------------------------------

/// 读 4 字节。
#[inline]
fn ld_u32(addr: u64) -> u32 {
    // SAFETY: 调用方保证 addr 位于已映射、可读、4 字节对齐的内存中。本 crate
    // 只对页来源交付过的页（页对齐，故 4 对齐）及其页内 4 字节倍数偏移使用它。
    unsafe { ptr::read_volatile(addr as *const u32) }
}

/// 写 4 字节。
#[inline]
fn wr_u32(addr: u64, val: u32) {
    // SAFETY: 同 `ld_u32`；另外调用方保证该地址归本 slab 独占（页来源契约），
    // 不会写穿到别人的内存。
    unsafe { ptr::write_volatile(addr as *mut u32, val) }
}

/// 读 8 字节。
#[inline]
fn ld_u64(addr: u64) -> u64 {
    // SAFETY: 同 `ld_u32`；偏移均为 8 的倍数，对齐成立。
    unsafe { ptr::read_volatile(addr as *const u64) }
}

/// 写 8 字节。
#[inline]
fn wr_u64(addr: u64, val: u64) {
    // SAFETY: 同 `ld_u32`；偏移均为 8 的倍数，对齐成立。
    unsafe { ptr::write_volatile(addr as *mut u64, val) }
}

// ---------------------------------------------------------------------------
// 公共类型
// ---------------------------------------------------------------------------

/// 页来源：内核里是"分配一帧 + 直接映射区地址"，测试里是一块 Vec 支撑的 arena。
///
/// 契约：
/// - `alloc_page` 返回的地址必须是 [`PAGE_SIZE`] 对齐的，且该页整页归调用方独占；
/// - 返回 `None` 表示内存耗尽，分配器必须优雅地放弃本次分配；
/// - `free_page` 只接受先前由同一个 `PageSource` 交付、且尚未归还的地址。
pub trait PageSource {
    /// 分配一页，返回页对齐的虚拟地址；该页归调用方独占。
    fn alloc_page(&mut self) -> Option<u64>;
    /// 归还一页。
    fn free_page(&mut self, addr: u64);
}

/// slab 错误码。
///
/// `TooLarge` / `ZeroSize` 目前不由 [`Slab::alloc`] 返回（它返回 `Option`，
/// 越界直接给 `None`），保留给 M3 的 `try_alloc` / 统计上报使用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlabError {
    /// 请求尺寸超过最大档位（2048）。
    TooLarge,
    /// 请求尺寸为 0。
    ZeroSize,
    /// 地址不在任何 slab 页内 / 不是对象起点 / 页头 magic 被破坏。
    BadPointer,
    /// 该对象已经在空闲链表里。
    DoubleFree,
    /// 内部不变量被破坏（链表环、计数不符）。
    Corrupt,
    /// 页来源拒绝供页。
    OutOfMemory,
}

/// 分配器统计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// 已从 [`PageSource`] 拿到的页数（当前只增不减，见模块文档第 5 条）。
    pub pages: usize,
    /// 已分配出去、尚未归还的对象数。
    pub used_objects: usize,
    /// 所有页里当前空闲的槽位数。
    pub free_objects: usize,
    /// 所有页里对象槽位的总字节数（不含页头，也不含页尾对齐余量）。
    pub bytes_capacity: usize,
}

// ---------------------------------------------------------------------------
// Slab
// ---------------------------------------------------------------------------

/// 定长对象 slab 分配器。
///
/// 结构上只有 8 个类 × 2 条页链 + 4 个计数器，没有任何动态分配：
/// 每个 `Slab` 实例的元数据都是固定大小的，可以直接作为静态变量嵌进内核。
pub struct Slab {
    /// 每类"所有页"链头（审计 + `free` 归属校验）。
    heads: [u64; NCLASSES],
    /// 每类"还有空闲槽位"的页链头（`alloc` 快路径）。
    partial: [u64; NCLASSES],
    /// 已供页数。
    pages: usize,
    /// 已用槽位总数。
    used: usize,
    /// 空闲槽位总数。
    free_objects: usize,
    /// 槽位总字节数。
    capacity_bytes: usize,
}

impl Slab {
    /// 创建一个空的分配器。`const`，可以放进静态变量。
    pub const fn new() -> Self {
        Slab {
            heads: [NIL; NCLASSES],
            partial: [NIL; NCLASSES],
            pages: 0,
            used: 0,
            free_objects: 0,
            capacity_bytes: 0,
        }
    }

    /// size → 档位下标（16 对齐向上取整）。
    ///
    /// `size == 0` 返回 `None`（零尺寸请求通常是调用方 bug，这里不静默接受）；
    /// `size > 2048` 返回 `None`（超大请求应当走页分配器或专用 arena）。
    pub fn class_of(size: usize) -> Option<usize> {
        if size == 0 {
            return None;
        }
        let mut i = 0;
        while i < NCLASSES {
            // 用 get 而不是下标，保证即使以后 CLASS_SIZES 变短也不会 panic。
            let cap = *CLASS_SIZES.get(i)?;
            if size <= cap {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// 分配 size 字节；返回的对象地址至少 16 字节对齐，且已按 size 分档。
    ///
    /// 失败返回 `None`（不做任何状态变更）。成功返回的槽位前 8 字节归本分配器
    /// 用作空闲链表指针，调用方可以立刻覆盖。
    pub fn alloc(&mut self, size: usize, src: &mut impl PageSource) -> Option<u64> {
        let class = Self::class_of(size)?;

        // 快路径：从"还有空位"的页链上取。
        // 计数器守卫防止页链被人为破坏成环时死循环。
        let mut guard = 0usize;
        loop {
            let page = *self.partial.get(class)?;
            if page == NIL {
                break;
            }
            guard += 1;
            if guard > self.pages + 1 {
                // 页链异常（成环或远超页数），放弃 partial 链，直接开新页，
                // 保证 alloc 一定会终止。
                break;
            }
            let next = ld_u64(page + OFF_NEXT_PARTIAL);
            wr_u64(page + OFF_NEXT_PARTIAL, NIL);
            let slot = match self.partial.get_mut(class) {
                Some(h) => {
                    *h = next;
                    true
                }
                None => false,
            };
            if !slot {
                break;
            }
            if let Some(obj) = self.take_slot(page) {
                // 还有空位就挂回链头，否则留在链外（满了）。
                if ld_u32(page + OFF_FREE_COUNT) > 0 {
                    self.push_partial(class, page);
                }
                return Some(obj);
            }
            // 页自相矛盾（free_count > 0 但链表为空）：本次不修，先跳过该页，
            // 继续找下一页。它仍然留在 heads 链里，free 时还能被找到。
        }

        // 慢路径：向页来源要一页。
        let base = match src.alloc_page() {
            Some(a) => a,
            None => return None,
        };
        // 页来源违约（返回 0 或未对齐）时把页还回去，返回 None 而不是 panic。
        if base == NIL || (base & PAGE_MASK) != base {
            src.free_page(base);
            return None;
        }
        let obj_size = *CLASS_SIZES.get(class)?;
        let total = (PAGE_SIZE - HDR_SIZE) / obj_size;
        if total == 0 {
            // 理论上不会发生（最大档 2048 < 4096-64），防御性归还。
            src.free_page(base);
            return None;
        }
        self.init_page(base, class, obj_size, total);

        let obj = self.take_slot(base);
        if ld_u32(base + OFF_FREE_COUNT) > 0 {
            self.push_partial(class, base);
        }
        obj
    }

    /// 归还。必须能识别：野指针、不是对象起点、重复释放。
    ///
    /// 出错时**不改动任何状态**（包括重复释放检测），调用方可以放心地把
    /// 错误当成"这次 free 被拒绝了"来处理。
    pub fn free(&mut self, addr: u64) -> Result<(), SlabError> {
        if addr == NIL {
            return Err(SlabError::BadPointer);
        }
        let base = addr & PAGE_MASK;
        if base == NIL {
            return Err(SlabError::BadPointer);
        }
        // 页头魔数：不对就说明这不是我们发出的页（野指针）。
        if ld_u32(base + OFF_MAGIC) != MAGIC {
            return Err(SlabError::BadPointer);
        }
        let class = ld_u32(base + OFF_CLASS) as usize;
        let obj_size = match CLASS_SIZES.get(class) {
            Some(v) => *v,
            None => return Err(SlabError::BadPointer),
        };
        if ld_u32(base + OFF_OBJ_SIZE) as usize != obj_size {
            return Err(SlabError::BadPointer);
        }
        let first = ld_u32(base + OFF_FIRST_OFF) as usize;
        if first < HDR_SIZE || first >= PAGE_SIZE || first % ALIGN != 0 {
            return Err(SlabError::BadPointer);
        }
        let total = ld_u32(base + OFF_TOTAL) as usize;
        if total == 0 || total != (PAGE_SIZE - first) / obj_size {
            return Err(SlabError::BadPointer);
        }
        // 归属校验：这一页必须真在本 slab 该类的页链里。
        // （页头可以被伪造，页链不能——链在 Slab 自己的内存里。）
        if !self.page_in_all(class, base) {
            return Err(SlabError::BadPointer);
        }

        // 对象起点校验：落在对象网格上才算数。
        let obj_start = base + first as u64;
        if addr < obj_start || addr >= base + PAGE_SIZE as u64 {
            return Err(SlabError::BadPointer);
        }
        if (addr - obj_start) % obj_size as u64 != 0 {
            return Err(SlabError::BadPointer);
        }

        // 双重释放检测：在该页的空闲链表里线性查找（≤ 252 个节点）。
        let mut cur = ld_u64(base + OFF_FREE_HEAD);
        let mut n = 0usize;
        while cur != NIL {
            if cur == addr {
                return Err(SlabError::DoubleFree);
            }
            n += 1;
            if n > total {
                // 链表比对象总数还长 → 有环，状态已不可信。
                return Err(SlabError::Corrupt);
            }
            cur = ld_u64(cur);
        }

        let used_count = ld_u32(base + OFF_USED_COUNT) as usize;
        let free_count = ld_u32(base + OFF_FREE_COUNT) as usize;
        // used_count == 0 说明这页没有任何已分配对象，而 addr 又不在空闲链表里，
        // 两个事实互相矛盾 —— 页头被人动过，拒绝操作。
        if used_count == 0 || free_count >= total {
            return Err(SlabError::Corrupt);
        }

        // 至此才动状态：头插空闲链表。
        let head = ld_u64(base + OFF_FREE_HEAD);
        wr_u64(addr, head);
        wr_u64(base + OFF_FREE_HEAD, addr);
        wr_u32(base + OFF_FREE_COUNT, (free_count + 1) as u32);
        wr_u32(base + OFF_USED_COUNT, (used_count - 1) as u32);

        // 页重新有空位 → 挂回 partial 链（幂等：先查再插）。
        if !self.page_in_partial(class, base) {
            self.push_partial(class, base);
        }
        self.free_objects = self.free_objects.saturating_add(1);
        self.used = self.used.saturating_sub(1);
        Ok(())
    }

    /// 当前统计快照。
    pub fn stats(&self) -> Stats {
        Stats {
            pages: self.pages,
            used_objects: self.used,
            free_objects: self.free_objects,
            bytes_capacity: self.capacity_bytes,
        }
    }

    /// 全量审计。检查项：
    ///
    /// 1. 每类的页链无环（长度不超过 `pages`）；
    /// 2. 每页 magic / class / obj_size / first_obj_off / total_objects 自洽；
    /// 3. `free_count + used_count == total_objects`；
    /// 4. 空闲链表长度 == `free_count`，节点都落在本页对象网格上、无重复、无环；
    /// 5. 全量累加的 pages/used/free/capacity 与 [`Slab::stats`] 一致。
    ///
    /// 这是唯一一个会遍历所有页的操作，只在调试构建 / 内核 panic 前自检时调用。
    pub fn audit(&self) -> Result<(), SlabError> {
        let mut pages = 0usize;
        let mut used = 0usize;
        let mut free = 0usize;
        let mut capacity = 0usize;

        let mut class = 0usize;
        while class < NCLASSES {
            let obj_size = match CLASS_SIZES.get(class) {
                Some(v) => *v,
                None => return Err(SlabError::Corrupt),
            };
            let mut page = match self.heads.get(class) {
                Some(v) => *v,
                None => return Err(SlabError::Corrupt),
            };
            let mut guard = 0usize;
            while page != NIL {
                guard += 1;
                if guard > self.pages + 1 {
                    // 页链成环，或者链长度与计数器不符。
                    return Err(SlabError::Corrupt);
                }

                if ld_u32(page + OFF_MAGIC) != MAGIC {
                    return Err(SlabError::BadPointer);
                }
                if ld_u32(page + OFF_CLASS) as usize != class {
                    return Err(SlabError::Corrupt);
                }
                if ld_u32(page + OFF_OBJ_SIZE) as usize != obj_size {
                    return Err(SlabError::Corrupt);
                }
                let first = ld_u32(page + OFF_FIRST_OFF) as usize;
                if first < HDR_SIZE || first >= PAGE_SIZE || first % ALIGN != 0 {
                    return Err(SlabError::Corrupt);
                }
                let total = (PAGE_SIZE - first) / obj_size;
                if ld_u32(page + OFF_TOTAL) as usize != total {
                    return Err(SlabError::Corrupt);
                }
                let free_count = ld_u32(page + OFF_FREE_COUNT) as usize;
                let used_count = ld_u32(page + OFF_USED_COUNT) as usize;
                if free_count > total
                    || used_count > total
                    || free_count + used_count != total
                {
                    return Err(SlabError::Corrupt);
                }

                // 空闲链表校验：位图去重（total ≤ 252 < 256，4×u64 足够）。
                let mut seen = [0u64; 4];
                let mut cur = ld_u64(page + OFF_FREE_HEAD);
                let mut n = 0usize;
                while cur != NIL {
                    n += 1;
                    if n > total {
                        // 比对象总数还长 → 必有环。
                        return Err(SlabError::Corrupt);
                    }
                    let start = page + first as u64;
                    if cur < start || cur >= page + PAGE_SIZE as u64 {
                        return Err(SlabError::Corrupt);
                    }
                    let off = (cur - start) as usize;
                    if off % obj_size != 0 {
                        return Err(SlabError::Corrupt);
                    }
                    let idx = off / obj_size;
                    if idx >= total {
                        return Err(SlabError::Corrupt);
                    }
                    let (word, bit) = (idx / 64, idx % 64);
                    let mask = 1u64 << bit;
                    match seen.get_mut(word) {
                        Some(w) => {
                            if *w & mask != 0 {
                                // 同一个槽位出现两次：重复或环。
                                return Err(SlabError::Corrupt);
                            }
                            *w |= mask;
                        }
                        None => return Err(SlabError::Corrupt),
                    }
                    cur = ld_u64(cur);
                }
                if n != free_count {
                    // 链表长度与计数器不符。
                    return Err(SlabError::Corrupt);
                }

                pages += 1;
                used += used_count;
                free += free_count;
                capacity += total * obj_size;
                page = ld_u64(page + OFF_NEXT_ALL);
            }
            class += 1;
        }

        if pages != self.pages
            || used != self.used
            || free != self.free_objects
            || capacity != self.capacity_bytes
        {
            return Err(SlabError::Corrupt);
        }
        Ok(())
    }

    // -- 内部辅助 ----------------------------------------------------------

    /// 初始化一页：写页头 + 串好空闲链表 + 挂到 heads 链头。
    fn init_page(&mut self, base: u64, class: usize, obj_size: usize, total: usize) {
        wr_u32(base + OFF_MAGIC, MAGIC);
        wr_u32(base + OFF_OBJ_SIZE, obj_size as u32);
        wr_u64(base + OFF_NEXT_ALL, match self.heads.get(class) {
            Some(v) => *v,
            None => NIL,
        });
        wr_u64(base + OFF_NEXT_PARTIAL, NIL);
        wr_u64(base + OFF_FREE_HEAD, NIL);
        wr_u32(base + OFF_FREE_COUNT, 0);
        wr_u32(base + OFF_TOTAL, total as u32);
        wr_u32(base + OFF_USED_COUNT, 0);
        wr_u32(base + OFF_FIRST_OFF, HDR_SIZE as u32);
        wr_u32(base + OFF_CLASS, class as u32);
        // 保留字段清零：页可能来自刚释放的帧，不假设内容干净。
        wr_u32(base + 52, 0);
        wr_u64(base + 56, 0);

        if let Some(h) = self.heads.get_mut(class) {
            *h = base;
        }

        // 串空闲链表：按地址升序，最后一个指向 NIL。
        let start = base + HDR_SIZE as u64;
        let step = obj_size as u64;
        let mut i = 0usize;
        while i < total {
            let obj = start + (i as u64) * step;
            let next = if i + 1 < total { obj + step } else { NIL };
            wr_u64(obj, next);
            i += 1;
        }
        wr_u64(base + OFF_FREE_HEAD, start);
        wr_u32(base + OFF_FREE_COUNT, total as u32);

        self.pages += 1;
        self.free_objects = self.free_objects.saturating_add(total);
        self.capacity_bytes = self.capacity_bytes.saturating_add(total * obj_size);
    }

    /// 从指定页摘一个槽位。页内状态自相矛盾时返回 `None` 且不改状态。
    fn take_slot(&mut self, page: u64) -> Option<u64> {
        let free_count = ld_u32(page + OFF_FREE_COUNT) as usize;
        if free_count == 0 {
            return None;
        }
        let head = ld_u64(page + OFF_FREE_HEAD);
        if head == NIL {
            return None;
        }
        // 头节点必须在页内对象网格上，否则视为损坏，拒绝摘取。
        let first = ld_u32(page + OFF_FIRST_OFF) as usize;
        let obj_size = ld_u32(page + OFF_OBJ_SIZE) as usize;
        if obj_size == 0 || first >= PAGE_SIZE {
            return None;
        }
        let start = page + first as u64;
        if head < start || head >= page + PAGE_SIZE as u64 {
            return None;
        }
        if (head - start) % obj_size as u64 != 0 {
            return None;
        }

        let next = ld_u64(head);
        wr_u64(page + OFF_FREE_HEAD, next);
        wr_u32(page + OFF_FREE_COUNT, (free_count - 1) as u32);
        let used_count = ld_u32(page + OFF_USED_COUNT) as usize;
        wr_u32(page + OFF_USED_COUNT, (used_count + 1) as u32);

        self.free_objects = self.free_objects.saturating_sub(1);
        self.used = self.used.saturating_add(1);
        Some(head)
    }

    /// 把页挂到某类的 partial 链头。
    fn push_partial(&mut self, class: usize, page: u64) {
        let head = match self.partial.get(class) {
            Some(v) => *v,
            None => return,
        };
        wr_u64(page + OFF_NEXT_PARTIAL, head);
        if let Some(h) = self.partial.get_mut(class) {
            *h = page;
        }
    }

    /// 页是否在该类的 heads 链上。
    fn page_in_all(&self, class: usize, page: u64) -> bool {
        let head = match self.heads.get(class) {
            Some(v) => *v,
            None => return false,
        };
        self.list_contains(head, page, OFF_NEXT_ALL)
    }

    /// 页是否在该类的 partial 链上。
    fn page_in_partial(&self, class: usize, page: u64) -> bool {
        let head = match self.partial.get(class) {
            Some(v) => *v,
            None => return false,
        };
        self.list_contains(head, page, OFF_NEXT_PARTIAL)
    }

    /// 在页链里线性查找，带步数守卫（防环）。
    ///
    /// `next_off` 指定该链用页头里哪个字段作 next 指针 —— 两条链的 next 字段
    /// 不同，混用会把 partial 链走成 heads 链（早期版本就踩过这个坑）。
    fn list_contains(&self, mut head: u64, target: u64, next_off: u64) -> bool {
        let mut guard = 0usize;
        while head != NIL {
            if head == target {
                return true;
            }
            guard += 1;
            if guard > self.pages + 1 {
                return false;
            }
            head = ld_u64(head + next_off);
        }
        false
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// 每页实际对象数（按当前页头尺寸算，测试里不写死 252）。
    fn objs_per_page(class: usize) -> usize {
        (PAGE_SIZE - HDR_SIZE) / CLASS_SIZES[class]
    }

    /// 由一块真实内存支撑的页来源。
    ///
    /// `buf` 多留 2 页：
    /// - 1 页用于把 `as_ptr()` 向上对齐到页边界（Vec 的对齐只保证 1 字节）；
    /// - 1 页用于制造"已映射但从不发给 slab"的野指针靶子 —— 直接拿
    ///   `0xdead_beef` 之类未映射地址去 free 会先段错误在 `read_volatile` 上，
    ///   根本走不到 `BadPointer` 分支。这正好对应内核对直接映射区的前提假设。
    struct Arena {
        buf: Vec<u8>,
        next: usize,
        freed: Vec<usize>,
        max_pages: usize,
    }

    impl Arena {
        /// 默认 254 页可用（buf 正好 1 MiB）。
        fn new() -> Self {
            Self::with_pages(254)
        }

        fn with_pages(n: usize) -> Self {
            Arena {
                buf: vec![0u8; (n + 2) * PAGE_SIZE],
                next: 0,
                freed: Vec::new(),
                max_pages: n,
            }
        }

        fn base(&self) -> usize {
            (self.buf.as_ptr() as usize + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
        }

        /// 一块已映射、但永远不会交给 slab 的页 —— 用它当"野指针"。
        fn wild_addr(&self) -> u64 {
            (self.base() + self.max_pages * PAGE_SIZE) as u64
        }
    }

    impl PageSource for Arena {
        fn alloc_page(&mut self) -> Option<u64> {
            if let Some(p) = self.freed.pop() {
                return Some(p as u64);
            }
            if self.next >= self.max_pages {
                return None;
            }
            let addr = self.base() + self.next * PAGE_SIZE;
            self.next += 1;
            Some(addr as u64)
        }

        fn free_page(&mut self, addr: u64) {
            self.freed.push(addr as usize);
        }
    }

    /// 自写 LCG，固定种子保证可复现。
    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Lcg(seed)
        }
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            // 取高位，低位的线性同余低位周期很短。
            self.0 >> 33
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() as usize) % n
        }
    }

    // 1. class_of 边界
    #[test]
    fn class_of_boundaries() {
        assert_eq!(Slab::class_of(0), None);
        assert_eq!(Slab::class_of(1), Some(0));
        assert_eq!(Slab::class_of(16), Some(0));
        assert_eq!(Slab::class_of(17), Some(1));
        assert_eq!(Slab::class_of(31), Some(1));
        assert_eq!(Slab::class_of(32), Some(1));
        assert_eq!(Slab::class_of(33), Some(2));
        assert_eq!(Slab::class_of(2048), Some(7));
        assert_eq!(Slab::class_of(2049), None);
        assert_eq!(Slab::class_of(usize::MAX), None);
        // 每一档的下边界都必须正好落在自己身上（档位间距 ≥ 16，sz-1 仍属于本档）。
        for (i, sz) in CLASS_SIZES.iter().enumerate() {
            assert_eq!(Slab::class_of(*sz), Some(i));
            assert_eq!(Slab::class_of(sz - 1), Some(i));
        }
    }

    // 2. 单次 alloc/free 往返
    #[test]
    fn single_roundtrip() {
        let mut a = Arena::new();
        let mut s = Slab::new();

        let p = s.alloc(24, &mut a).expect("alloc");
        assert_eq!(p % 16, 0, "对象必须 16 字节对齐");
        assert_eq!(s.stats().used_objects, 1);

        s.free(p).expect("free");
        assert_eq!(s.stats().used_objects, 0);
        assert_eq!(s.stats().free_objects, s.stats().bytes_capacity / 32);
        assert!(s.audit().is_ok());
    }

    // 3. 同一档位连续分配 200 个：地址互不相同、全部 16 字节对齐
    #[test]
    fn many_allocations_are_unique_and_aligned() {
        let mut a = Arena::new();
        let mut s = Slab::new();

        let mut addrs = Vec::new();
        for _ in 0..200 {
            let p = s.alloc(9, &mut a).expect("alloc");
            assert_eq!(p % 16, 0);
            addrs.push(p);
        }
        let mut sorted = addrs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 200, "200 个对象地址必须两两不同");
        // 200 个 16 字节对象塞得进一页（252 个槽位）。
        assert_eq!(s.stats().pages, 1);
        assert_eq!(s.stats().used_objects, 200);
        assert!(s.audit().is_ok());
    }

    // 4. 释放后再分配能复用（不新开页）
    #[test]
    fn free_then_alloc_reuses_without_new_page() {
        let mut a = Arena::new();
        let mut s = Slab::new();

        let p1 = s.alloc(64, &mut a).expect("alloc");
        s.free(p1).expect("free");
        let pages_after_free = s.stats().pages;

        let p2 = s.alloc(64, &mut a).expect("alloc");
        assert_eq!(p1, p2, "刚释放的槽位应当被优先复用");
        assert_eq!(s.stats().pages, pages_after_free, "复用不应新开页");
        assert_eq!(a.next, 1, "页来源只应被调用过一次");
    }

    // 5. 跨档位互不干扰
    #[test]
    fn classes_are_isolated() {
        let mut a = Arena::new();
        let mut s = Slab::new();

        for _ in 0..10 {
            let small = s.alloc(16, &mut a).expect("alloc 16");
            let big = s.alloc(128, &mut a).expect("alloc 128");
            assert_eq!(Slab::class_of(16), Some(0));
            assert_eq!(Slab::class_of(128), Some(3));
            // 两个档位来自不同的页。
            assert_ne!(small & PAGE_MASK, big & PAGE_MASK);
            s.free(big).expect("free 128");
            s.free(small).expect("free 16");
        }
        assert_eq!(s.stats().pages, 2, "两个档位各一页");
        assert_eq!(s.stats().used_objects, 0);
        let expect_free = objs_per_page(0) + objs_per_page(3);
        let expect_cap = objs_per_page(0) * 16 + objs_per_page(3) * 128;
        assert_eq!(s.stats().free_objects, expect_free);
        assert_eq!(s.stats().bytes_capacity, expect_cap);
        assert!(s.audit().is_ok());
    }

    // 6. 大量分配触发多页
    #[test]
    fn exhaustion_spills_to_more_pages() {
        let mut a = Arena::new();
        let mut s = Slab::new();

        let cap = objs_per_page(0);
        assert!(cap < 300);
        for _ in 0..300 {
            s.alloc(16, &mut a).expect("alloc");
        }
        assert!(s.stats().pages > 1, "300 个 16 字节对象必须占用多于一页");
        assert_eq!(s.stats().pages, 2);
        assert_eq!(s.stats().used_objects, 300);
        assert!(s.audit().is_ok());
    }

    // 7. 野指针 free → BadPointer
    #[test]
    fn wild_pointer_is_rejected() {
        let mut a = Arena::new();
        let mut s = Slab::new();
        let p = s.alloc(32, &mut a).expect("alloc");

        // 已映射但从未交给 slab 的页：magic == 0。
        let wild = a.wild_addr();
        assert_eq!(s.free(wild), Err(SlabError::BadPointer));
        // 已经交付的页，但偏移落在页头里（不是任何对象）。
        let base = p & PAGE_MASK;
        assert_eq!(s.free(base), Err(SlabError::BadPointer));
        assert_eq!(s.free(base + 8), Err(SlabError::BadPointer));
        // 0 也是非法地址。
        assert_eq!(s.free(0), Err(SlabError::BadPointer));

        // 失败的 free 不能破坏状态。
        assert_eq!(s.stats().used_objects, 1);
        assert!(s.audit().is_ok());
        s.free(p).expect("free");
    }

    // 8. 页内非对象起点 → BadPointer
    #[test]
    fn misaligned_interior_pointer_is_rejected() {
        let mut a = Arena::new();
        let mut s = Slab::new();
        let p = s.alloc(16, &mut a).expect("alloc");

        assert_eq!(s.free(p + 8), Err(SlabError::BadPointer));
        assert_eq!(s.free(p + 1), Err(SlabError::BadPointer));
        assert_eq!(s.stats().used_objects, 1);

        // 大档位同样：128 档的 +16 也不是起点。
        let q = s.alloc(128, &mut a).expect("alloc");
        assert_eq!(s.free(q + 16), Err(SlabError::BadPointer));
        assert_eq!(s.free(q + 127), Err(SlabError::BadPointer));
        s.free(q).expect("free");
        s.free(p).expect("free");
        assert!(s.audit().is_ok());
    }

    // 9. 双重释放 → DoubleFree，且再 free 一次仍是 DoubleFree 而不是破坏状态
    #[test]
    fn double_free_is_detected_and_harmless() {
        let mut a = Arena::new();
        let mut s = Slab::new();
        let p = s.alloc(32, &mut a).expect("alloc");
        let before = s.stats();

        s.free(p).expect("first free");
        let after_first = s.stats();
        assert_eq!(s.free(p), Err(SlabError::DoubleFree));
        assert_eq!(s.free(p), Err(SlabError::DoubleFree));
        assert_eq!(s.free(p), Err(SlabError::DoubleFree));
        assert_eq!(s.stats(), after_first, "重复释放不得改动任何状态");
        assert_ne!(before, after_first);
        assert!(s.audit().is_ok());
    }

    // 10. 每个档位都跑一遍 alloc/free 往返
    #[test]
    fn every_class_roundtrips() {
        let mut a = Arena::new();
        let mut s = Slab::new();

        for (i, sz) in CLASS_SIZES.iter().enumerate() {
            let mut v = Vec::new();
            for _ in 0..3 {
                let p = s.alloc(*sz, &mut a).expect("alloc");
                assert_eq!(p % 16, 0);
                // 对象必须落在属于自己的页上，且档位正确。
                assert_eq!(Slab::class_of(*sz), Some(i));
                v.push(p);
            }
            // 逆序释放
            while let Some(p) = v.pop() {
                s.free(p).expect("free");
            }
            assert_eq!(s.stats().used_objects, 0, "档位 {sz} 未完全回收");
            assert!(s.audit().is_ok(), "档位 {sz} 审计失败");
        }
        // 各档位 3 个对象所需的页数：1 个 2048 槽位/页的最费页（1024 档 3 槽/页）。
        let expect_pages: usize = CLASS_SIZES
            .iter()
            .enumerate()
            .map(|(i, _)| (3 + objs_per_page(i) - 1) / objs_per_page(i))
            .sum();
        assert_eq!(s.stats().pages, expect_pages);
        assert_eq!(s.stats().pages, 10, "8 档里有 3 页被 2048 档吃掉");
    }

    // 11. OutOfMemory：只给 1 页，把 2048 档位分配到满后再分配返回 None
    #[test]
    fn out_of_memory_returns_none() {
        let mut a = Arena::with_pages(1);
        let mut s = Slab::new();

        // 2048 档每页只有 1 个槽位。
        assert_eq!(objs_per_page(7), 1);
        let p = s.alloc(2048, &mut a).expect("alloc");
        assert_eq!(s.stats().pages, 1);
        assert_eq!(s.stats().free_objects, 0);

        assert_eq!(s.alloc(2048, &mut a), None, "页来源耗尽时应返回 None");
        assert_eq!(s.alloc(16, &mut a), None, "别的档位同样拿不到页");
        assert_eq!(s.stats().pages, 1);
        assert_eq!(s.stats().used_objects, 1);

        s.free(p).expect("free");
        assert!(s.audit().is_ok());
    }

    // 12. stats 在 alloc/free 后 used/free 计数正确
    #[test]
    fn stats_track_alloc_and_free() {
        let mut a = Arena::new();
        let mut s = Slab::new();
        let total = objs_per_page(1); // 32 字节档

        assert_eq!(s.stats(), Stats::default());

        let p1 = s.alloc(32, &mut a).expect("alloc");
        let st = s.stats();
        assert_eq!(st.pages, 1);
        assert_eq!(st.used_objects, 1);
        assert_eq!(st.free_objects, total - 1);
        assert_eq!(st.bytes_capacity, total * 32);

        let p2 = s.alloc(32, &mut a).expect("alloc");
        assert_eq!(s.stats().used_objects, 2);
        assert_eq!(s.stats().free_objects, total - 2);

        s.free(p1).expect("free");
        assert_eq!(s.stats().used_objects, 1);
        assert_eq!(s.stats().free_objects, total - 1);

        s.free(p2).expect("free");
        assert_eq!(s.stats().used_objects, 0);
        assert_eq!(s.stats().free_objects, total);
        assert_eq!(s.stats().bytes_capacity, total * 32);
        assert_eq!(s.stats().pages, 1, "未实现整页回收，页数不下降");
    }

    // 13. stats.pages 与实际拿到的页数一致
    #[test]
    fn stats_pages_match_page_source() {
        let mut a = Arena::new();
        let mut s = Slab::new();

        for sz in [16usize, 16, 16, 64, 1024, 64, 2048, 128] {
            s.alloc(sz, &mut a).expect("alloc");
        }
        // 16 档 3 个对象 → 1 页；64 档 2 个 → 1 页；1024 档 1 个 → 1 页；
        // 2048 档 1 个 → 1 页；128 档 1 个 → 1 页。共 5 页。
        assert_eq!(s.stats().pages, 5);
        assert_eq!(s.stats().pages, a.next, "stats.pages 应等于页来源实际交付的页数");
        assert_eq!(a.freed.len(), 0);
        assert!(s.audit().is_ok());
    }

    // 14. audit 干净时 Ok
    #[test]
    fn audit_is_clean_when_consistent() {
        let mut a = Arena::new();
        let mut s = Slab::new();

        // 每个档位都留一些已分配 + 一些空闲，覆盖 partial 链的两端。
        let mut live = Vec::new();
        for sz in CLASS_SIZES {
            for i in 0..5 {
                let p = s.alloc(sz, &mut a).expect("alloc");
                if i % 2 == 0 {
                    live.push(p);
                } else {
                    s.free(p).expect("free");
                }
            }
        }
        assert!(s.audit().is_ok());
        for p in live {
            s.free(p).expect("free");
        }
        assert!(s.audit().is_ok());
    }

    // 15. 人为破坏页头 magic → audit 返回 BadPointer
    #[test]
    fn audit_detects_broken_magic() {
        let mut a = Arena::new();
        let mut s = Slab::new();
        let p = s.alloc(16, &mut a).expect("alloc");
        let base = p & PAGE_MASK;

        assert!(s.audit().is_ok());
        // SAFETY: base 是页来源交付过的、已映射的页基址，写 4 字节在其头部合法。
        unsafe { ptr::write_volatile(base as *mut u32, 0xDEAD_BEEF) };

        assert_eq!(s.audit(), Err(SlabError::BadPointer));
        // 被破坏后 free 也必须拒绝。
        assert_eq!(s.free(p), Err(SlabError::BadPointer));
    }

    // 16. 人为让 free_count 与链表长度不符 → audit 返回 Corrupt
    #[test]
    fn audit_detects_free_count_mismatch() {
        let mut a = Arena::new();
        let mut s = Slab::new();
        let p = s.alloc(32, &mut a).expect("alloc");
        let base = p & PAGE_MASK;
        assert!(s.audit().is_ok());

        let fc = ld_u32(base + OFF_FREE_COUNT);
        // SAFETY: base 是已交付页的基址，OFF_FREE_COUNT 是页头内 4 字节字段。
        unsafe { ptr::write_volatile((base + OFF_FREE_COUNT) as *mut u32, fc + 1) };

        assert_eq!(s.audit(), Err(SlabError::Corrupt));

        // 复原后必须重新干净（证明破坏是唯一原因）。
        // SAFETY: 同上，把字段写回原值。
        unsafe { ptr::write_volatile((base + OFF_FREE_COUNT) as *mut u32, fc) };
        assert!(s.audit().is_ok());
    }

    // 17. 随机操作对拍
    #[test]
    fn randomized_alloc_free() {
        // 300 个存活对象最坏情况占 300 页（2048 档），1024 页留足余量。
        let mut a = Arena::with_pages(1024);
        let mut s = Slab::new();
        let mut rng = Lcg::new(0x1234_5678_9ABC_DEF0);
        let mut live: Vec<u64> = Vec::new();
        let mut allocs = 0usize;
        let mut frees = 0usize;

        for _ in 0..10_000 {
            let want_alloc = live.is_empty() || (rng.below(100) < 55 && live.len() < 300);
            if want_alloc {
                let size = rng.below(2048) + 1;
                let p = s.alloc(size, &mut a).expect("alloc");
                assert_eq!(p % 16, 0);
                live.push(p);
                allocs += 1;
            } else if !live.is_empty() {
                let i = rng.below(live.len());
                let p = live.swap_remove(i);
                s.free(p).expect("free");
                frees += 1;
            }
            // 每隔一段做一次全量审计，尽早抓到局部破坏。
            if (allocs + frees) % 1000 == 0 {
                assert!(s.audit().is_ok(), "第 {} 步审计失败", allocs + frees);
            }
        }

        for p in live.drain(..) {
            s.free(p).expect("free");
        }
        assert_eq!(s.stats().used_objects, 0);
        assert!(s.audit().is_ok());
        assert!(allocs > 1000 && frees > 1000, "随机用例覆盖面太窄");
    }

    // 18. 释放顺序打乱（LIFO / FIFO / 随机）都能全部回收
    #[test]
    fn release_order_does_not_matter() {
        let n = 100usize;

        // LIFO
        {
            let mut a = Arena::new();
            let mut s = Slab::new();
            let mut v = Vec::new();
            for _ in 0..n {
                v.push(s.alloc(128, &mut a).expect("alloc"));
            }
            while let Some(p) = v.pop() {
                s.free(p).expect("free");
            }
            assert_eq!(s.stats().used_objects, 0);
            assert!(s.audit().is_ok());
        }

        // FIFO
        {
            let mut a = Arena::new();
            let mut s = Slab::new();
            let mut v: Vec<u64> = Vec::new();
            for _ in 0..n {
                v.push(s.alloc(128, &mut a).expect("alloc"));
            }
            let mut head = 0;
            while head < v.len() {
                s.free(v[head]).expect("free");
                head += 1;
            }
            assert_eq!(s.stats().used_objects, 0);
            assert!(s.audit().is_ok());
        }

        // 随机
        {
            let mut a = Arena::new();
            let mut s = Slab::new();
            let mut v: Vec<u64> = Vec::new();
            for _ in 0..n {
                v.push(s.alloc(128, &mut a).expect("alloc"));
            }
            let mut rng = Lcg::new(0x00C0_FFEE_0000_0001);
            while !v.is_empty() {
                let i = rng.below(v.len());
                let p = v.swap_remove(i);
                s.free(p).expect("free");
            }
            assert_eq!(s.stats().used_objects, 0);
            assert_eq!(s.stats().free_objects, s.stats().bytes_capacity / 128);
            assert!(s.audit().is_ok());
        }
    }
}
