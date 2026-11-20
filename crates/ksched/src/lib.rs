//! ksched：CFS 风格的就绪队列调度模型（纯逻辑，不碰硬件、不切换上下文）
//!
//! 本 crate 只描述"该让谁跑"这一层策略：每个 CPU 一个就绪队列，
//! 以 `vruntime`（虚拟运行时间）为唯一排序依据，取最小者运行。
//! 真正的上下文切换、时钟中断、IPI 由上层 crate 负责。
//!
//! # 核心规则
//! - 一个任务任意时刻只处于三种状态之一：空闲槽位 / 就绪（在某个 CPU 的队列里）/ 运行（不在任何队列里）。
//! - `vruntime` 按权重反向缩放：`增量 = delta * 1024 / weight(prio)`，
//!   优先级数值越小（prio 0 最高）权重越大，`vruntime` 涨得越慢。
//! - `tick` 只给"本 CPU 正在运行的任务"记账，就绪任务不受影响。
//! - 出队顺序完全确定：先比 `vruntime`，相等再比 `tid` 小的优先。
//!
//! # 硬性约束
//! 1. 公开 API 全部是"不 panic"的：容量满返回 [`SchedError::Full`]，
//!    未知 tid 返回 [`SchedError::NotFound`]，非法 CPU 返回 [`SchedError::BadCpu`]。
//! 2. `[dependencies]` 为空，零第三方依赖；不使用堆分配，全部数据放在定长数组里。
//! 3. 不使用 `unsafe`（因此没有 `// SAFETY:` 注释），也不做隐式失败。
//! 4. 每个公开接口都有单元测试，跑在宿主机上（`cargo test -p ksched`）。
//!
//! # 不变量（实现与测试共同保证）
//! - 每个槽位最多出现在一个就绪队列里；各队列长度之和 == 就绪任务数。
//! - 任务在就绪队列里时状态必须是就绪，且其 `cpu` 字段与所在队列一致。
//! - 任务处于运行态时 `running[cpu]` 一定指向它，且它不在任何就绪队列里。
//! - 单个任务的 `vruntime` 单调不减。

#![cfg_attr(not(test), no_std)]

/// 任务标识符。
pub type Tid = u64;

/// 任务表容量（同时也就是系统可容纳的最大任务数）。
pub const MAX_TASKS: usize = 4096;

/// 最大 CPU 数。
pub const MAX_CPUS: usize = 64;

/// 支持的优先级档位数：prio 取值 `0..40`，0 最高。
const MAX_PRIO: u8 = 40;

/// 调度器可返回的错误。所有会失败的操作都只返回错误，绝不 panic。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedError {
    /// 任务表已满（活跃任务数达到 [`MAX_TASKS`]）。
    Full,
    /// 找不到这个 tid（未创建或已被 `dequeue` 摘除）。
    NotFound,
    /// CPU 编号越界（`>= cpu_count`）。
    BadCpu,
    /// tid 已存在，或该任务已经在就绪队列里（重复入队）。
    Duplicate,
}

/// 任务对外可见的统计快照。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskStat {
    /// 任务标识。
    pub tid: Tid,
    /// 优先级（0 最高，39 最低）；越界的入参会夹到 39。
    pub prio: u8,
    /// 虚拟运行时间，越小越该跑。
    pub vruntime: u64,
    /// 最近一次进入就绪队列的时刻；运行中的任务保留原值。
    pub waiting_since: u64,
}

/// 槽位空闲标记。
const ST_FREE: u8 = 0;
/// 槽位处于就绪态（在某个 CPU 的就绪队列里）。
const ST_READY: u8 = 1;
/// 槽位处于运行态（不在任何就绪队列里）。
const ST_RUNNING: u8 = 2;

/// 链表/游标的空标记。
const SLOT_NONE: u32 = u32::MAX;

/// 优先级 → 权重表（prio 0..39，递减）。
///
/// 沿用"每降一档约除以 1.25"的形状，但写成整数常量表，避免浮点与运行时计算。
/// 权重越大，同样的时间片带来的 `vruntime` 增量越小，任务就越"占便宜"。
static WEIGHTS: [u64; MAX_PRIO as usize] = [
    88761, 71755, 56483, 46273, 36291, // prio  0.. 4
    29154, 23254, 18705, 14949, 11916, // prio  5.. 9
    9548, 7620, 6100, 4904, 3906, // prio 10..14
    3121, 2501, 1991, 1586, 1277, // prio 15..19
    1024, 820, 655, 526, 423, // prio 20..24
    335, 272, 215, 172, 137, // prio 25..29
    110, 87, 70, 56, 45, // prio 30..34
    36, 29, 23, 18, 15, // prio 35..39
];

/// 单个任务槽位。用 `#[derive(Clone, Copy)]` + 定长数组实现零分配任务表。
#[derive(Clone, Copy)]
struct Slot {
    /// 任务标识；空闲槽位为 0。
    tid: Tid,
    /// 虚拟运行时间。
    vruntime: u64,
    /// 进入就绪队列的时刻。
    waiting_since: u64,
    /// 就绪队列内的下一个槽位下标；空闲时用来串空闲链表。
    next: u32,
    /// 优先级（已夹到 `0..MAX_PRIO`）。
    prio: u8,
    /// 状态：`ST_FREE` / `ST_READY` / `ST_RUNNING`。
    state: u8,
    /// 任务当前归属的 CPU。
    cpu: u8,
}

impl Slot {
    /// 空槽位常量，用于初始化整个任务表。
    const EMPTY: Slot = Slot {
        tid: 0,
        vruntime: 0,
        waiting_since: 0,
        next: SLOT_NONE,
        prio: 0,
        state: ST_FREE,
        cpu: 0,
    };
}

/// 每个 CPU 一个的就绪队列，用槽位下标串成单向链表（零分配）。
///
/// 链表插入是 O(1) 的尾插，`pick`/`dequeue` 需要 O(n) 遍历，
/// 对本模型的规模（≤ [`MAX_TASKS`]）完全够用。
#[derive(Clone, Copy)]
struct RunQueue {
    head: u32,
    tail: u32,
    len: usize,
}

impl RunQueue {
    const EMPTY: RunQueue = RunQueue {
        head: SLOT_NONE,
        tail: SLOT_NONE,
        len: 0,
    };
}

/// CFS 风格调度器：每 CPU 一个就绪队列 + 一张定长任务表。
///
/// 创建后任务表全空、所有队列为空。整个结构不含指针、不含堆内存，
/// 可以安全地放在内核静态区里。
pub struct Scheduler {
    slots: [Slot; MAX_TASKS],
    rq: [RunQueue; MAX_CPUS],
    /// 每个 CPU 当前正在运行的任务槽位（`SLOT_NONE` 表示空闲）。
    running: [u32; MAX_CPUS],
    cpu_count: usize,
    /// 空闲槽位链表头，保证任务摘除后槽位可以复用。
    free_head: u32,
}

impl Scheduler {
    /// 构造一个覆盖 `cpu_count` 个 CPU 的调度器。
    ///
    /// `cpu_count` 为 0 时按 1 处理，超过 [`MAX_CPUS`] 时夹到 [`MAX_CPUS`]，
    /// 这样调用方不用先做校验，也不会出现"零队列"这种退化配置。
    pub fn new(cpu_count: usize) -> Self {
        let cpu_count = if cpu_count == 0 {
            1
        } else if cpu_count > MAX_CPUS {
            MAX_CPUS
        } else {
            cpu_count
        };

        let mut me = Scheduler {
            slots: [Slot::EMPTY; MAX_TASKS],
            rq: [RunQueue::EMPTY; MAX_CPUS],
            running: [SLOT_NONE; MAX_CPUS],
            cpu_count,
            free_head: SLOT_NONE,
        };

        // 反向串空闲链表：0 → 1 → 2 → ... → MAX_TASKS-1
        let mut i = MAX_TASKS;
        while i > 0 {
            i -= 1;
            me.slots[i].next = me.free_head;
            me.free_head = i as u32;
        }
        me
    }

    /// **就地**初始化，不产生整块临时值。
    ///
    /// 为什么需要它：`new()` 会先在调用方的栈上构造一个完整的 `Scheduler`
    /// （4096 个槽位 ≈ 128 KiB）再移动出来。内核栈通常只有几十 KiB，
    /// 一调用就爆栈——症状是随即发生的、看起来毫无关联的缺页。
    ///
    /// 要求：`self` 指向一段**已清零**的内存（静态区最合适），
    /// 且本函数会把每个字段都重新写一遍。`Scheduler` 全部是整数，
    /// 全零位型本来就是合法的，所以这种用法没有未定义行为。
    pub fn init_in_place(&mut self, cpu_count: usize) {
        let cpu_count = if cpu_count == 0 {
            1
        } else if cpu_count > MAX_CPUS {
            MAX_CPUS
        } else {
            cpu_count
        };
        for s in self.slots.iter_mut() {
            *s = Slot::EMPTY;
        }
        for q in self.rq.iter_mut() {
            *q = RunQueue::EMPTY;
        }
        for r in self.running.iter_mut() {
            *r = SLOT_NONE;
        }
        self.cpu_count = cpu_count;
        self.free_head = SLOT_NONE;
        let mut i = MAX_TASKS;
        while i > 0 {
            i -= 1;
            self.slots[i].next = self.free_head;
            self.free_head = i as u32;
        }
    }

    /// 当前管理的 CPU 数（`new` 已经做过夹取）。
    pub fn cpu_count(&self) -> usize {
        self.cpu_count
    }

    /// 创建任务并直接放入 `cpu` 的就绪队列。
    ///
    /// `vruntime` 从 0 开始，`waiting_since` 记为 `now`。
    /// 失败时返回 [`SchedError::BadCpu`] / [`SchedError::Duplicate`] / [`SchedError::Full`]，
    /// 且不做任何状态改动（满载失败不会破坏已有任务）。
    pub fn spawn(&mut self, tid: Tid, prio: u8, now: u64, cpu: usize) -> Result<(), SchedError> {
        if cpu >= self.cpu_count {
            return Err(SchedError::BadCpu);
        }
        if self.find(tid).is_some() {
            return Err(SchedError::Duplicate);
        }
        let slot = match self.alloc_slot() {
            Some(s) => s,
            None => return Err(SchedError::Full),
        };

        let idx = slot as usize;
        self.slots[idx] = Slot {
            tid,
            vruntime: 0,
            waiting_since: now,
            next: SLOT_NONE,
            prio: if prio >= MAX_PRIO { MAX_PRIO - 1 } else { prio },
            state: ST_READY,
            cpu: cpu as u8,
        };
        self.push_back(cpu, slot);
        Ok(())
    }

    /// 把已存在的任务重新放回它所属 CPU 的就绪队列。
    ///
    /// 典型用法是"时间片用完，把正在运行的任务放回队列"。
    /// 已经就绪的任务再入队返回 [`SchedError::Duplicate`]，未知 tid 返回 [`SchedError::NotFound`]。
    pub fn enqueue(&mut self, tid: Tid, now: u64) -> Result<(), SchedError> {
        let slot = match self.find(tid) {
            Some(s) => s,
            None => return Err(SchedError::NotFound),
        };
        let idx = slot as usize;

        if self.slots[idx].state == ST_READY {
            return Err(SchedError::Duplicate);
        }
        let cpu = self.slots[idx].cpu as usize;
        if cpu >= self.cpu_count {
            return Err(SchedError::BadCpu);
        }
        // 运行中的任务回到队列：先注销 running 标记，避免"既在跑又在排队"。
        if self.slots[idx].state == ST_RUNNING && self.running[cpu] == slot {
            self.running[cpu] = SLOT_NONE;
        }

        self.slots[idx].state = ST_READY;
        self.slots[idx].waiting_since = now;
        self.push_back(cpu, slot);
        Ok(())
    }

    /// 从 `cpu` 的就绪队列里挑一个任务运行，同时把它标记为该 CPU 的运行任务。
    ///
    /// 选择规则：`vruntime` 最小者；相同则 `tid` 小者优先（完全确定，便于复现）。
    /// 若该 CPU 上已有运行任务，先把它放回队列（`waiting_since = now`），这是"时间片到期"的自然语义。
    /// 队列为空时返回 `None`，且不会动已有运行任务。
    pub fn pick(&mut self, cpu: usize, now: u64) -> Option<Tid> {
        if cpu >= self.cpu_count {
            return None;
        }
        if self.rq[cpu].head == SLOT_NONE {
            return None;
        }
        // 时间片到期：上一个运行任务回到队尾。
        self.retire(cpu, Some(now));
        self.select_and_dispatch(cpu)
    }

    /// 从就绪队列里摘除任务并释放槽位。
    ///
    /// 返回 `false` 的场景：tid 不存在、任务正在运行（运行态不在队列里）、
    /// 或内部链表游标异常（不可达分支，仍按 `false` 上报，不 panic）。
    pub fn dequeue(&mut self, tid: Tid) -> bool {
        let slot = match self.find(tid) {
            Some(s) => s,
            None => return false,
        };
        let idx = slot as usize;
        if self.slots[idx].state != ST_READY {
            return false;
        }
        let cpu = self.slots[idx].cpu as usize;
        if cpu >= self.cpu_count || !self.unlink(cpu, slot) {
            return false;
        }

        self.slots[idx] = Slot::EMPTY;
        self.slots[idx].next = self.free_head;
        self.free_head = slot;
        true
    }

    /// 把一个任务从调度器里**彻底摘除**（无论它是就绪还是运行中）。
    ///
    /// `block` 场景专用。之前 `task::block_current` 用 `dequeue` 摘自己，
    /// 但运行态任务不在队列里，`dequeue` 会静默失败——槽位带着 ST_RUNNING
    /// 泄漏，之后 `wake` 里的 `spawn` 因 `Duplicate` 一直失败，被阻塞的
    /// 任务只能等时钟 tick 兜底才能醒（实测 200 次 wait 有 194 次退化）。
    ///
    /// 返回 `false` 表示 tid 未知（调用方可安全忽略）。
    pub fn detach(&mut self, tid: Tid) -> bool {
        let slot = match self.find(tid) {
            Some(s) => s,
            None => return false,
        };
        let idx = slot as usize;
        let cpu = self.slots[idx].cpu as usize;
        // 运行标记先清掉：不管是 READY 还是 RUNNING 都不该再被 pick 到
        if cpu < MAX_CPUS && self.running[cpu] == slot {
            self.running[cpu] = SLOT_NONE;
        }
        if self.slots[idx].state == ST_READY && !self.unlink(cpu, slot) {
            return false;
        }
        self.slots[idx] = Slot::EMPTY;
        self.slots[idx].next = self.free_head;
        self.free_head = slot;
        true
    }

    /// 给 `cpu` 上正在运行的任务记一笔时间账：`vruntime += delta * 1024 / weight(prio)`。
    ///
    /// `now` 目前不参与计算（本模型只按 `delta` 记账），保留参数以便上层对齐时间语义。
    /// 该 CPU 没有运行任务时是空操作；加法与乘法都做饱和处理，不会溢出也不会 panic。
    pub fn tick(&mut self, cpu: usize, now: u64, delta: u64) {
        let _ = now;
        if cpu >= self.cpu_count {
            return;
        }
        let slot = self.running[cpu];
        if slot == SLOT_NONE {
            return;
        }
        let idx = slot as usize;
        let weight = Self::weight(self.slots[idx].prio);
        // 权重表里没有 0，这里的判空只是"除法前先确认"，避免除零路径。
        let inc = if weight == 0 {
            0
        } else {
            delta.saturating_mul(1024) / weight
        };
        self.slots[idx].vruntime = self.slots[idx].vruntime.saturating_add(inc);
    }

    /// 直接指定某个 CPU 上的运行任务（上下文切换时由上层调用）。
    ///
    /// - `Some(tid)`：把该任务标为运行；若它本来在就绪队列里则先摘出来，
    ///   若它正在别的 CPU 上跑则先注销那边的标记。该 CPU 原有的运行任务会被放回队尾。
    /// - `None`：把当前运行任务放回队尾，CPU 变为空闲。
    ///
    /// 由于签名为 `()`，非法 CPU 或未知 tid 一律静默忽略（不做任何改动），不会 panic。
    pub fn set_running(&mut self, cpu: usize, tid: Option<Tid>) {
        if cpu >= self.cpu_count {
            return;
        }
        match tid {
            None => self.retire(cpu, None),
            Some(t) => {
                let slot = match self.find(t) {
                    Some(s) => s,
                    None => return,
                };
                let idx = slot as usize;
                match self.slots[idx].state {
                    ST_READY => {
                        let old = self.slots[idx].cpu as usize;
                        if old < MAX_CPUS {
                            let _ = self.unlink(old, slot);
                        }
                    }
                    ST_RUNNING => {
                        let old = self.slots[idx].cpu as usize;
                        if old < MAX_CPUS && self.running[old] == slot {
                            self.running[old] = SLOT_NONE;
                        }
                    }
                    _ => return,
                }
                self.retire(cpu, None);
                self.slots[idx].state = ST_RUNNING;
                self.slots[idx].cpu = cpu as u8;
                self.running[cpu] = slot;
            }
        }
    }

    /// 把任务迁移到 `to_cpu`，`vruntime` 原样保留（迁移不影响公平账本）。
    ///
    /// 就绪任务：从源队列摘除后插入目标队列；运行任务：把运行标记一并搬过去。
    /// 目标 CPU 非法返回 [`SchedError::BadCpu`]，未知 tid 返回 [`SchedError::NotFound`]。
    pub fn migrate(&mut self, tid: Tid, to_cpu: usize) -> Result<(), SchedError> {
        if to_cpu >= self.cpu_count {
            return Err(SchedError::BadCpu);
        }
        let slot = match self.find(tid) {
            Some(s) => s,
            None => return Err(SchedError::NotFound),
        };
        let idx = slot as usize;
        let from = self.slots[idx].cpu as usize;
        if from == to_cpu {
            return Ok(());
        }

        match self.slots[idx].state {
            ST_READY => {
                // 摘不下来说明内部状态坏了，宁可报错也不重复入队。
                if from >= MAX_CPUS || !self.unlink(from, slot) {
                    return Err(SchedError::NotFound);
                }
                self.slots[idx].cpu = to_cpu as u8;
                self.push_back(to_cpu, slot);
                Ok(())
            }
            ST_RUNNING => {
                if from < MAX_CPUS && self.running[from] == slot {
                    self.running[from] = SLOT_NONE;
                }
                // 目标 CPU 原有的运行任务让位（回到队尾）。
                self.retire(to_cpu, None);
                self.slots[idx].cpu = to_cpu as u8;
                self.running[to_cpu] = slot;
                Ok(())
            }
            // find() 已排除空闲槽位，此分支不可达。
            _ => Err(SchedError::NotFound),
        }
    }

    /// 查询任务统计快照；未知 tid 返回 `None`（任务被 `dequeue` 后也返回 `None`）。
    pub fn stats(&self, tid: Tid) -> Option<TaskStat> {
        let slot = self.find(tid)?;
        let s = &self.slots[slot as usize];
        Some(TaskStat {
            tid: s.tid,
            prio: s.prio,
            vruntime: s.vruntime,
            waiting_since: s.waiting_since,
        })
    }

    /// `cpu` 就绪队列的长度（不含正在运行的那一个任务）。越界 CPU 返回 0。
    pub fn load(&self, cpu: usize) -> usize {
        if cpu >= self.cpu_count {
            0
        } else {
            self.rq[cpu].len
        }
    }

    /// 所有就绪任务里"最久等待时间"，即 `max(now - waiting_since)`。
    ///
    /// `now` 早于 `waiting_since` 时按 0 计算（不做无符号下溢）；没有就绪任务返回 0。
    pub fn max_wait(&self, now: u64) -> u64 {
        let mut worst = 0u64;
        for i in 0..MAX_TASKS {
            let s = &self.slots[i];
            if s.state == ST_READY {
                let w = now.saturating_sub(s.waiting_since);
                if w > worst {
                    worst = w;
                }
            }
        }
        worst
    }

    // ---- 内部辅助（全部不 panic，越界一律走安全分支） ----

    /// 优先级 → 权重；`prio` 已保证 `< MAX_PRIO`，索引仍做一次夹取以防越界。
    #[inline]
    fn weight(prio: u8) -> u64 {
        let i = if prio >= MAX_PRIO {
            (MAX_PRIO - 1) as usize
        } else {
            prio as usize
        };
        WEIGHTS[i]
    }

    /// 线性查找活跃任务（空闲槽位不参与匹配）。
    fn find(&self, tid: Tid) -> Option<u32> {
        for i in 0..MAX_TASKS {
            let s = &self.slots[i];
            if s.state != ST_FREE && s.tid == tid {
                return Some(i as u32);
            }
        }
        None
    }

    /// 从空闲链表取一个槽位。
    fn alloc_slot(&mut self) -> Option<u32> {
        let slot = self.free_head;
        if slot == SLOT_NONE {
            return None;
        }
        self.free_head = self.slots[slot as usize].next;
        Some(slot)
    }

    /// 尾插入队。调用方必须保证该槽位当前不在任何队列里。
    fn push_back(&mut self, cpu: usize, slot: u32) {
        if cpu >= MAX_CPUS {
            return;
        }
        self.slots[slot as usize].next = SLOT_NONE;
        let tail = self.rq[cpu].tail;
        if tail == SLOT_NONE {
            self.rq[cpu].head = slot;
        } else {
            self.slots[tail as usize].next = slot;
        }
        self.rq[cpu].tail = slot;
        self.rq[cpu].len += 1;
    }

    /// 从 `cpu` 的队列中摘除 `slot`；成功返回 `true`。
    fn unlink(&mut self, cpu: usize, slot: u32) -> bool {
        if cpu >= MAX_CPUS {
            return false;
        }
        let mut prev = SLOT_NONE;
        let mut cur = self.rq[cpu].head;
        while cur != SLOT_NONE {
            if cur == slot {
                let nxt = self.slots[cur as usize].next;
                if prev == SLOT_NONE {
                    self.rq[cpu].head = nxt;
                } else {
                    self.slots[prev as usize].next = nxt;
                }
                if self.rq[cpu].tail == cur {
                    self.rq[cpu].tail = prev;
                }
                self.slots[cur as usize].next = SLOT_NONE;
                self.rq[cpu].len = self.rq[cpu].len.saturating_sub(1);
                return true;
            }
            prev = cur;
            cur = self.slots[cur as usize].next;
        }
        false
    }

    /// 把 `cpu` 上的运行任务放回队尾；`now` 为 `Some` 时刷新等待起点。
    fn retire(&mut self, cpu: usize, now: Option<u64>) {
        if cpu >= MAX_CPUS {
            return;
        }
        let slot = self.running[cpu];
        if slot == SLOT_NONE {
            return;
        }
        self.running[cpu] = SLOT_NONE;
        let idx = slot as usize;
        if let Some(t) = now {
            self.slots[idx].waiting_since = t;
        }
        self.slots[idx].state = ST_READY;
        self.slots[idx].cpu = cpu as u8;
        self.push_back(cpu, slot);
    }

    /// 在 `cpu` 的队列里选 `vruntime` 最小（并列取 tid 最小）的任务并派发。
    fn select_and_dispatch(&mut self, cpu: usize) -> Option<Tid> {
        let mut best = SLOT_NONE;
        let mut cur = self.rq[cpu].head;
        while cur != SLOT_NONE {
            let better = if best == SLOT_NONE {
                true
            } else {
                let a = &self.slots[cur as usize];
                let b = &self.slots[best as usize];
                a.vruntime < b.vruntime || (a.vruntime == b.vruntime && a.tid < b.tid)
            };
            if better {
                best = cur;
            }
            cur = self.slots[cur as usize].next;
        }

        if best == SLOT_NONE {
            return None;
        }
        self.unlink(cpu, best);
        let idx = best as usize;
        self.slots[idx].state = ST_RUNNING;
        self.slots[idx].cpu = cpu as u8;
        self.running[cpu] = best;
        Some(self.slots[idx].tid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 固定种子的线性同余发生器（Numerical Recipes 参数），保证随机测试可复现。
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state >> 33
    }

    /// 全面自检：队列结构、状态一致性、tid 唯一性、vruntime 单调性。
    fn audit(s: &Scheduler, last_vruntime: &mut HashMap<Tid, u64>) {
        let cpus = s.cpu_count();
        let mut visited = vec![false; MAX_TASKS];
        let mut queued = 0usize;

        for c in 0..cpus {
            let mut n = 0usize;
            let mut cur = s.rq[c].head;
            while cur != SLOT_NONE {
                let idx = cur as usize;
                assert!(idx < MAX_TASKS, "队列游标越界: {}", idx);
                assert!(!visited[idx], "槽位 {} 同时出现在多个队列中", idx);
                assert!(n <= MAX_TASKS, "队列 {} 成环", c);
                visited[idx] = true;
                assert_eq!(s.slots[idx].state, ST_READY, "队列里的任务必须是就绪态");
                assert_eq!(s.slots[idx].cpu as usize, c, "任务归属 CPU 与队列不一致");
                n += 1;
                cur = s.slots[idx].next;
            }
            assert_eq!(n, s.rq[c].len, "CPU {} 队列长度字段与实际长度不符", c);
            assert_eq!(
                s.rq[c].head == SLOT_NONE,
                s.rq[c].len == 0,
                "CPU {} 空队列的头指针与长度必须同时为空",
                c
            );
            queued += n;
        }

        let mut ready_tasks = 0usize;
        let mut running_tasks = 0usize;
        let mut tids: Vec<Tid> = Vec::new();

        for i in 0..MAX_TASKS {
            let t = &s.slots[i];
            match t.state {
                ST_READY => {
                    ready_tasks += 1;
                    assert!(visited[i], "就绪任务 {} 不在任何队列里", t.tid);
                }
                ST_RUNNING => {
                    running_tasks += 1;
                    let c = t.cpu as usize;
                    assert!(c < cpus, "运行任务的 CPU 越界");
                    assert_eq!(s.running[c], i as u32, "running[] 必须指向运行任务");
                    assert!(!visited[i], "运行任务不应出现在就绪队列里");
                }
                ST_FREE => {}
                _ => panic!("非法状态字节 {}", t.state),
            }
            if t.state != ST_FREE {
                assert!(!tids.contains(&t.tid), "tid {} 重复", t.tid);
                assert!(t.prio < MAX_PRIO, "prio 越界");
                tids.push(t.tid);
                if let Some(prev) = last_vruntime.get(&t.tid).copied() {
                    assert!(t.vruntime >= prev, "tid {} 的 vruntime 回退", t.tid);
                }
                last_vruntime.insert(t.tid, t.vruntime);
            }
        }

        assert_eq!(queued, ready_tasks, "各队列长度之和应等于就绪任务数");
        assert!(running_tasks <= cpus, "运行任务数不超过 CPU 数");
    }

    #[test]
    fn spawn_pick_dequeue_roundtrip() {
        let mut s = Scheduler::new(1);
        assert!(s.spawn(7, 20, 0, 0).is_ok());
        assert_eq!(s.load(0), 1);
        assert_eq!(s.pick(0, 1), Some(7));
        assert_eq!(s.load(0), 0, "被选中的任务不在就绪队列里");
        assert!(!s.dequeue(7), "运行中的任务不能被摘出队列");
        assert!(s.enqueue(7, 2).is_ok(), "时间片到期应能放回队列");
        assert_eq!(s.load(0), 1);
        assert!(s.dequeue(7));
        assert_eq!(s.load(0), 0);
        assert_eq!(s.stats(7), None, "摘除后任务不再可见");
        assert_eq!(s.pick(0, 3), None);
    }

    #[test]
    fn pick_prefers_smaller_vruntime_then_smaller_tid() {
        let mut s = Scheduler::new(1);
        assert!(s.spawn(1, 20, 0, 0).is_ok());
        assert!(s.spawn(2, 20, 0, 0).is_ok());
        assert!(s.spawn(3, 20, 0, 0).is_ok());

        // vruntime 全为 0，按 tid 决胜。
        assert_eq!(s.pick(0, 0), Some(1));
        s.tick(0, 0, 50); // tid 1: 0 -> 50
        assert_eq!(s.stats(1).map(|t| t.vruntime), Some(50));

        // 1 的 vruntime 变大，下一次轮到 2。
        assert_eq!(s.pick(0, 1), Some(2));
        s.tick(0, 1, 50);
        assert_eq!(s.pick(0, 2), Some(3));
    }

    #[test]
    fn tick_only_charges_running_task() {
        let mut s = Scheduler::new(1);
        assert!(s.spawn(1, 20, 0, 0).is_ok());
        assert!(s.spawn(2, 20, 0, 0).is_ok());

        s.tick(0, 0, 100); // 还没人运行，空操作
        assert_eq!(s.stats(1).map(|t| t.vruntime), Some(0));
        assert_eq!(s.stats(2).map(|t| t.vruntime), Some(0));

        assert_eq!(s.pick(0, 0), Some(1));
        s.tick(0, 0, 100);
        assert_eq!(s.stats(1).map(|t| t.vruntime), Some(100), "prio 20 权重 1024，增量等于 delta");
        assert_eq!(s.stats(2).map(|t| t.vruntime), Some(0), "就绪任务不被记账");

        s.tick(9, 0, 100); // 非法 CPU 静默忽略
        assert_eq!(s.stats(1).map(|t| t.vruntime), Some(100));
    }

    #[test]
    fn higher_priority_grows_vruntime_slower() {
        let mut s = Scheduler::new(2);
        assert!(s.spawn(1, 0, 0, 0).is_ok());
        assert!(s.spawn(2, 39, 0, 1).is_ok());
        assert_eq!(s.pick(0, 0), Some(1));
        assert_eq!(s.pick(1, 0), Some(2));

        s.tick(0, 0, 1000);
        s.tick(1, 0, 1000);

        let hi = s.stats(1).map(|t| t.vruntime).unwrap_or(u64::MAX);
        let lo = s.stats(2).map(|t| t.vruntime).unwrap_or(0);
        assert!(hi < lo, "高优先级增长应更慢: {} vs {}", hi, lo);
        assert_eq!(hi, 1000 * 1024 / 88761);
        assert_eq!(lo, 1000 * 1024 / 15);
    }

    #[test]
    fn dequeue_running_returns_false() {
        let mut s = Scheduler::new(1);
        assert!(s.spawn(1, 20, 0, 0).is_ok());
        assert_eq!(s.pick(0, 0), Some(1));
        assert!(!s.dequeue(1));
        assert_eq!(s.stats(1).map(|t| t.vruntime), Some(0), "失败不能破坏状态");
    }

    #[test]
    fn empty_queue_pick_is_none() {
        let mut s = Scheduler::new(2);
        assert_eq!(s.pick(0, 0), None);
        assert_eq!(s.pick(1, 0), None);
        assert_eq!(s.pick(2, 0), None, "非法 CPU 也返回 None");
        assert_eq!(s.load(0), 0);
        assert_eq!(s.load(9), 0);
    }

    #[test]
    fn unknown_tid_is_not_found() {
        let mut s = Scheduler::new(2);
        assert_eq!(s.enqueue(42, 0), Err(SchedError::NotFound));
        assert_eq!(s.migrate(42, 1), Err(SchedError::NotFound));
        assert_eq!(s.stats(42), None);
        assert!(!s.dequeue(42));
        // 未知 tid 的 set_running 静默忽略，不产生运行任务。
        s.set_running(0, Some(42));
        assert_eq!(s.pick(0, 0), None);
    }

    #[test]
    fn duplicate_spawn_is_rejected() {
        let mut s = Scheduler::new(1);
        assert!(s.spawn(5, 20, 0, 0).is_ok());
        assert_eq!(s.spawn(5, 10, 0, 0), Err(SchedError::Duplicate));
        assert_eq!(s.load(0), 1, "重复 spawn 不能多占槽位");
        assert_eq!(s.stats(5).map(|t| t.prio), Some(20), "旧任务属性不能被覆盖");
        // 已在队列里的任务不能重复入队。
        assert_eq!(s.enqueue(5, 0), Err(SchedError::Duplicate));
        assert_eq!(s.load(0), 1);
    }

    #[test]
    fn spawn_on_bad_cpu_is_rejected() {
        let mut s = Scheduler::new(1);
        assert_eq!(s.spawn(1, 20, 0, 1), Err(SchedError::BadCpu));
        assert_eq!(s.stats(1), None);
        assert_eq!(s.load(0), 0);
    }

    #[test]
    fn migrate_keeps_vruntime_and_selection() {
        let mut s = Scheduler::new(2);
        assert!(s.spawn(1, 20, 0, 0).is_ok());
        assert!(s.spawn(2, 20, 0, 0).is_ok());

        assert_eq!(s.pick(0, 0), Some(1));
        s.tick(0, 0, 500); // tid 1: vruntime 500
        assert!(s.enqueue(1, 10).is_ok());

        assert!(s.migrate(1, 1).is_ok());
        assert_eq!(s.load(0), 1);
        assert_eq!(s.load(1), 1);
        assert_eq!(s.stats(1).map(|t| t.vruntime), Some(500), "迁移保留 vruntime");

        // 目标 CPU 上只有它，立刻被选中，且 vruntime 未变。
        assert_eq!(s.pick(1, 20), Some(1));
        assert_eq!(s.stats(1).map(|t| t.vruntime), Some(500));
        assert_eq!(s.pick(0, 20), Some(2));
    }

    #[test]
    fn migrate_to_bad_cpu_is_rejected() {
        let mut s = Scheduler::new(2);
        assert!(s.spawn(1, 20, 0, 0).is_ok());
        assert_eq!(s.migrate(1, 2), Err(SchedError::BadCpu));
        assert_eq!(s.load(0), 1, "失败迁移必须保持原状");
        assert_eq!(s.load(1), 0);
        // 迁移到同一个 CPU 是合法的空操作。
        assert!(s.migrate(1, 0).is_ok());
        assert_eq!(s.load(0), 1);
    }

    #[test]
    fn full_capacity_returns_full_without_damage() {
        let mut s = Scheduler::new(1);
        for i in 1..=MAX_TASKS {
            assert!(s.spawn(i as Tid, 20, 0, 0).is_ok(), "第 {} 个任务应成功", i);
        }
        assert_eq!(s.load(0), MAX_TASKS);
        assert_eq!(s.spawn(MAX_TASKS as Tid + 1, 20, 0, 0), Err(SchedError::Full));
        assert_eq!(s.load(0), MAX_TASKS, "满载失败不能破坏队列");
        assert_eq!(s.stats(1).map(|t| t.tid), Some(1));
        assert_eq!(s.stats(MAX_TASKS as Tid).map(|t| t.tid), Some(MAX_TASKS as Tid));

        // 腾出槽位后又能接受新任务，说明失败路径没有污染空闲链表。
        assert!(s.dequeue(1));
        assert!(s.spawn(MAX_TASKS as Tid + 1, 20, 0, 0).is_ok());
        assert_eq!(s.load(0), MAX_TASKS);
    }

    #[test]
    fn max_wait_reports_longest_waiter() {
        let mut s = Scheduler::new(1);
        assert_eq!(s.max_wait(100), 0, "空队列没有等待");
        assert!(s.spawn(1, 20, 10, 0).is_ok());
        assert!(s.spawn(2, 20, 50, 0).is_ok());
        assert_eq!(s.max_wait(100), 90);
        assert_eq!(s.max_wait(5), 0, "now 早于入队时间按 0 计");

        assert_eq!(s.pick(0, 100), Some(1), "同时入队时按 tid 决胜");
        assert_eq!(s.max_wait(100), 50, "运行中的任务不计入等待");
        assert!(s.dequeue(2));
        assert_eq!(s.max_wait(100), 0);
    }

    #[test]
    fn set_running_none_restores_load() {
        let mut s = Scheduler::new(1);
        assert!(s.spawn(1, 20, 0, 0).is_ok());
        assert!(s.spawn(2, 20, 0, 0).is_ok());
        assert_eq!(s.load(0), 2);

        assert_eq!(s.pick(0, 0), Some(1));
        assert_eq!(s.load(0), 1);

        s.set_running(0, None);
        assert_eq!(s.load(0), 2, "运行任务被放回队列");
        assert!(s.stats(1).is_some());

        // 重新派发后只有运行中的任务被记账。
        s.tick(0, 0, 10);
        let total: u64 = s.stats(1).map(|t| t.vruntime).unwrap_or(0);
        assert_eq!(total, 0, "没有运行任务时 tick 是空操作");
    }

    #[test]
    fn set_running_explicit_switches_task() {
        let mut s = Scheduler::new(1);
        assert!(s.spawn(1, 20, 0, 0).is_ok());
        assert!(s.spawn(2, 20, 0, 0).is_ok());

        s.set_running(0, Some(2));
        assert_eq!(s.load(0), 1, "被指定运行的任务从队列里摘出");
        s.tick(0, 0, 64);
        assert_eq!(s.stats(2).map(|t| t.vruntime), Some(64));

        s.set_running(0, Some(1));
        // 任务 2 让位回到队列，任务 1 从队列里出来接管 CPU，队列长度不变。
        assert_eq!(s.load(0), 1, "一进一出，队列长度不变");
        assert!(s.dequeue(2), "让位的任务应该回到就绪队列");
        s.tick(0, 0, 64);
        assert_eq!(s.stats(1).map(|t| t.vruntime), Some(64));
    }

    #[test]
    fn fair_alternation_between_equal_tasks() {
        let mut s = Scheduler::new(1);
        assert!(s.spawn(1, 20, 0, 0).is_ok());
        assert!(s.spawn(2, 20, 0, 0).is_ok());

        let mut counts = [0usize; 3];
        let mut now = 0u64;
        for _ in 0..1000 {
            if let Some(t) = s.pick(0, now) {
                counts[(t as usize).min(2)] += 1;
                s.tick(0, now, 1);
            }
            now += 1;
        }

        let v1 = s.stats(1).map(|t| t.vruntime).unwrap_or(0);
        let v2 = s.stats(2).map(|t| t.vruntime).unwrap_or(0);
        assert_eq!(v1 + v2, 1000, "总记账量应等于总 tick 数");
        assert!(
            v1.abs_diff(v2) <= 1,
            "同优先级任务的 vruntime 偏差应不超过一个时间片: {} vs {}",
            v1,
            v2
        );
        assert!(counts[1] >= 450 && counts[2] >= 450, "两个任务应大致平分: {:?}", counts);
    }

    #[test]
    fn random_operations_keep_invariants() {
        let cpus = 4usize;
        let mut s = Scheduler::new(cpus);
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut last_vruntime: HashMap<Tid, u64> = HashMap::new();
        let mut now = 0u64;

        for step in 0..10_000u64 {
            now += 1;
            let op = lcg(&mut seed) % 10;
            let tid = 1 + lcg(&mut seed) % 40;
            let cpu = (lcg(&mut seed) as usize) % cpus;

            match op {
                0 | 1 => {
                    let prio = (lcg(&mut seed) % 40) as u8;
                    if s.spawn(tid, prio, now, cpu).is_ok() {
                        // 新任务（可能是同 tid 的新一轮生命周期）从头记账。
                        last_vruntime.insert(tid, 0);
                    }
                }
                2 => {
                    let _ = s.enqueue(tid, now);
                }
                3 => {
                    let _ = s.pick(cpu, now);
                }
                4 => {
                    // 任务被摘除后生命周期结束，vruntime 的单调性只在一个生命周期内成立。
                    if s.dequeue(tid) {
                        last_vruntime.remove(&tid);
                    }
                }
                5 => {
                    let delta = lcg(&mut seed) % 64;
                    s.tick(cpu, now, delta);
                }
                6 => s.set_running(cpu, Some(tid)),
                7 => s.set_running(cpu, None),
                8 => {
                    let _ = s.migrate(tid, cpu);
                }
                _ => {
                    let _ = s.stats(tid);
                    let _ = s.load(cpu);
                    let _ = s.max_wait(now);
                }
            }

            if step % 100 == 0 {
                audit(&s, &mut last_vruntime);
            }
        }
        audit(&s, &mut last_vruntime);
    }

    #[test]
    fn init_in_place_behaves_like_new() {
        // 就地初始化与 new 必须等价（内核用的是前者，测试用的是后者）
        let mut a = Scheduler::new(2);
        let mut b = Box::new(Scheduler::new(1));
        b.init_in_place(2);
        assert_eq!(a.cpu_count(), b.cpu_count());
        assert_eq!(a.load(0), b.load(0));
        assert!(a.spawn(7, 20, 0, 0).is_ok());
        assert!(b.spawn(7, 20, 0, 0).is_ok());
        assert_eq!(a.pick(0, 0), b.pick(0, 0));
    }

    #[test]
    fn init_in_place_is_idempotent() {
        let mut s = Box::new(Scheduler::new(1));
        s.init_in_place(1);
        assert!(s.spawn(1, 10, 0, 0).is_ok());
        s.init_in_place(1); // 再初始化一次应当回到"全空"
        assert_eq!(s.load(0), 0);
        assert!(s.spawn(1, 10, 0, 0).is_ok());
        assert_eq!(s.load(0), 1);
    }
}
