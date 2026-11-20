//! 故障注入策略（S2 的核心）。
//!
//! 目标：**不靠运气**地证明"分配失败时内核不会崩"。
//!
//! 做法是把"第几次调用失败"抽成一个纯状态机 [`Plan`]，内核只负责在
//! 分配路径上问一句 `should_fail(point)`。这样：
//!
//! - 判定本身不分配内存、不加锁（内核在 OOM 路径上调用它是安全的）；
//! - 同一个 `Plan` 在宿主机上可以跑满全部用例，不用起虚拟机；
//! - "注入 1000 次、失败 1000 次、系统仍然可服务"是可数的，不是感觉。
//!
//! 语义：**第 `skip + k * period` 次调用失败**（`k = 1, 2, …`），
//! 最多失败 `budget` 次（`budget == 0` 表示不限）。
//! `skip = 0, period = 1` 就是"每次都失败"。

/// 故障注入点。
///
/// 只列**能让内核优雅失败**的位置。`PageFault` 之类由硬件产生的故障
/// 不在这里——那是 [`crate::soak`] 与 fuzz 的活儿。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Point {
    /// 物理帧分配（`mm::frames`）。
    Frame = 1,
    /// 内核堆分配（`#[global_allocator]`）。
    Heap = 2,
    /// slab 页来源。
    Slab = 3,
    /// 块设备读。
    DiskRead = 4,
    /// 块设备写。
    DiskWrite = 5,
    /// 管道 / IPC 缓冲。
    Ipc = 6,
    /// 磁盘控制器（AHCI）命令超时 / 端口失联。
    ///
    /// 与 `DiskRead`/`DiskWrite` 的区别：那两个是"这一块读坏了"（数据层），
    /// 这个是"整个控制器没反应了"（设备层）——上层要能把设备标记为 down，
    /// 而不是每一块都超时一遍。
    AhciDown = 7,
    /// 网卡失联（virtio/e1000 发送后设备不推进 used 环）。
    NicDown = 8,
    /// 显示 / 控制台设备失联（串口写不出去）。
    ConsoleDown = 9,
}

impl Point {
    pub const ALL: [Point; 9] = [
        Point::Frame,
        Point::Heap,
        Point::Slab,
        Point::DiskRead,
        Point::DiskWrite,
        Point::Ipc,
        Point::AhciDown,
        Point::NicDown,
        Point::ConsoleDown,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Point::Frame => "frame",
            Point::Heap => "heap",
            Point::Slab => "slab",
            Point::DiskRead => "disk-read",
            Point::DiskWrite => "disk-write",
            Point::Ipc => "ipc",
            Point::AhciDown => "ahci-down",
            Point::NicDown => "nic-down",
            Point::ConsoleDown => "console-down",
        }
    }

    /// 从编号还原（内核用原子变量存注入点时用）。
    pub const fn from_u8(v: u8) -> Option<Point> {
        Some(match v {
            1 => Point::Frame,
            2 => Point::Heap,
            3 => Point::Slab,
            4 => Point::DiskRead,
            5 => Point::DiskWrite,
            6 => Point::Ipc,
            7 => Point::AhciDown,
            8 => Point::NicDown,
            9 => Point::ConsoleDown,
            _ => return None,
        })
    }
}

/// 一次注入实验的计划。
#[derive(Clone, Copy, Debug)]
pub struct Plan {
    point: Point,
    skip: u32,
    period: u32,
    budget: u32,
    seen: u32,
    fired: u32,
}

impl Plan {
    /// 建一个计划：从第 `skip + period` 次调用开始，每隔 `period` 次失败一次，
    /// 最多失败 `budget` 次（0 = 不限）。
    pub fn new(point: Point, skip: u32, period: u32, budget: u32) -> Self {
        Plan {
            point,
            skip,
            period: period.max(1),
            budget,
            seen: 0,
            fired: 0,
        }
    }

    /// "每 2 次失败 1 次、最先失败立刻发生"——最常用的粗暴档位。
    pub fn every(point: Point, period: u32) -> Self {
        Plan::new(point, 0, period, 0)
    }

    /// "只失败一次"。
    pub fn once(point: Point) -> Self {
        Plan::new(point, 0, 1, 1)
    }

    pub fn point(&self) -> Point {
        self.point
    }

    pub fn fired(&self) -> u32 {
        self.fired
    }

    pub fn seen(&self) -> u32 {
        self.seen
    }

    /// 预算是否已用完（用完之后 `should_fail` 恒为 `false`）。
    pub fn exhausted(&self) -> bool {
        self.budget != 0 && self.fired >= self.budget
    }

    /// 该不该在这次调用上失败。
    ///
    /// 只对 `point` 匹配的调用计数；别的注入点来问不会影响本计划的进度。
    pub fn should_fail(&mut self, point: Point) -> bool {
        if point != self.point || self.exhausted() {
            return false;
        }
        self.seen += 1;
        if self.seen <= self.skip {
            return false;
        }
        let k = self.seen - self.skip;
        if k % self.period != 0 {
            return false;
        }
        self.fired += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_fires_exactly_once_on_first_call() {
        let mut p = Plan::once(Point::Frame);
        assert!(p.should_fail(Point::Frame));
        for _ in 0..100 {
            assert!(!p.should_fail(Point::Frame));
        }
        assert_eq!(p.fired(), 1);
        assert!(p.exhausted());
    }

    #[test]
    fn every_two_fires_on_even_calls() {
        let mut p = Plan::every(Point::Frame, 2);
        let got: [bool; 8] = core::array::from_fn(|_| p.should_fail(Point::Frame));
        // 第 2、4、6、8 次失败
        assert_eq!(got, [false, true, false, true, false, true, false, true]);
        assert_eq!(p.fired(), 4);
    }

    #[test]
    fn skip_leaves_the_first_calls_alone() {
        let mut p = Plan::new(Point::Heap, 3, 2, 0);
        let got: [bool; 9] = core::array::from_fn(|_| p.should_fail(Point::Heap));
        // 前 3 次放过；之后 k = seen - skip，k 为偶数时失败 →
        // 第 5、7、9 次失败（k = 2、4、6）
        assert_eq!(
            got,
            [false, false, false, false, true, false, true, false, true]
        );
    }

    #[test]
    fn other_points_do_not_consume_the_plan() {
        let mut p = Plan::once(Point::DiskRead);
        for _ in 0..10 {
            assert!(!p.should_fail(Point::Frame));
            assert!(!p.should_fail(Point::Heap));
        }
        assert_eq!(p.seen(), 0, "别的注入点不该推进本计划的计数");
        assert!(p.should_fail(Point::DiskRead));
    }

    #[test]
    fn budget_stops_injection() {
        let mut p = Plan::new(Point::Frame, 0, 1, 3);
        assert!(p.should_fail(Point::Frame));
        assert!(p.should_fail(Point::Frame));
        assert!(p.should_fail(Point::Frame));
        for _ in 0..10 {
            assert!(!p.should_fail(Point::Frame));
        }
        assert_eq!(p.fired(), 3);
    }

    #[test]
    fn budget_zero_means_unlimited() {
        let mut p = Plan::new(Point::Frame, 0, 1, 0);
        for _ in 0..1000 {
            assert!(p.should_fail(Point::Frame));
        }
        assert!(!p.exhausted());
    }

    #[test]
    fn period_zero_is_normalised_to_one() {
        let mut p = Plan::new(Point::Frame, 0, 0, 0);
        assert!(p.should_fail(Point::Frame));
        assert!(p.should_fail(Point::Frame));
    }

    #[test]
    fn point_roundtrip() {
        for p in Point::ALL {
            assert_eq!(Point::from_u8(p as u8), Some(p));
            assert!(!p.name().is_empty());
        }
        assert_eq!(Point::from_u8(0), None);
        assert_eq!(Point::from_u8(200), None);
    }

    #[test]
    fn rate_matches_period() {
        // 1000 次调用、period=10 → 恰好 100 次失败
        let mut p = Plan::every(Point::Ipc, 10);
        let mut fails = 0;
        for _ in 0..1000 {
            if p.should_fail(Point::Ipc) {
                fails += 1;
            }
        }
        assert_eq!(fails, 100);
    }
}
