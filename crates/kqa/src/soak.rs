//! 长跑采样与漂移判定（S3）。
//!
//! S3 要求"30 天长跑内存漂移 = 0 页"。真要跑 30 天不现实，但**判定方法**
//! 必须现在就对：每 N 次循环采一次样，用最小二乘拟合"空闲页数 ~ 采样序号"，
//! 斜率必须近似 0。斜率是 0 还是 -0.5 页/千次，是**数**出来的，不是看出来的。
//!
//! 为什么不用浮点：内核里没有切换 FPU/SSE 上下文，随手用 `f64` 会污染
//! 用户态（尤其 musl 的浮点状态）。这里全部用 `i128` 整数运算，
//! 斜率放大 1000 倍保留精度。

/// 定长环形采样缓冲。
///
/// 满了就覆盖最旧的——长跑要的是"最近的趋势"，
/// 而不需要把 30 天的每次采样都留着（那是 259 万个点）。
#[derive(Clone)]
pub struct Samples<const N: usize> {
    v: [u64; N],
    /// 下一个写入位置。
    head: usize,
    /// 已有多少个（上限 N）。
    count: usize,
    /// 总共推入过多少个（不受容量影响）。
    total: u64,
}

impl<const N: usize> Default for Samples<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Samples<N> {
    pub const fn new() -> Self {
        Samples {
            v: [0; N],
            head: 0,
            count: 0,
            total: 0,
        }
    }

    pub fn push(&mut self, x: u64) {
        if N == 0 {
            return;
        }
        self.v[self.head] = x;
        self.head = (self.head + 1) % N;
        if self.count < N {
            self.count += 1;
        }
        self.total += 1;
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// 累计推入次数（含被覆盖的）。
    pub fn total(&self) -> u64 {
        self.total
    }

    /// 按时间顺序取第 `i` 个（0 = 最旧）。
    fn at(&self, i: usize) -> u64 {
        if self.count < N {
            self.v[i]
        } else {
            self.v[(self.head + i) % N]
        }
    }

    pub fn first(&self) -> Option<u64> {
        if self.count == 0 {
            None
        } else {
            Some(self.at(0))
        }
    }

    pub fn last(&self) -> Option<u64> {
        if self.count == 0 {
            None
        } else {
            Some(self.at(self.count - 1))
        }
    }

    pub fn min(&self) -> Option<u64> {
        (0..self.count).map(|i| self.at(i)).min()
    }

    pub fn max(&self) -> Option<u64> {
        (0..self.count).map(|i| self.at(i)).max()
    }

    /// `last - first`（可为负）。
    pub fn delta(&self) -> i64 {
        match (self.first(), self.last()) {
            (Some(a), Some(b)) => b as i64 - a as i64,
            _ => 0,
        }
    }

    /// 最小二乘斜率，单位：**每 1000 个采样点变化多少单位**（千分斜率）。
    ///
    /// 数据点少于 2 个返回 0；所有 x 相同（不可能）也返回 0。
    /// 用 `i128` 累加：N=256 且值到 2^32 时，Σxy 约 2^32 * 256^2 = 2^48，
    /// 远在 i128 之内，不会溢出。
    pub fn slope_milli(&self) -> i64 {
        let n = self.count as i128;
        if n < 2 {
            return 0;
        }
        let mut sx: i128 = 0;
        let mut sy: i128 = 0;
        let mut sxy: i128 = 0;
        let mut sxx: i128 = 0;
        for i in 0..self.count {
            let x = i as i128;
            let y = self.at(i) as i128;
            sx += x;
            sy += y;
            sxy += x * y;
            sxx += x * x;
        }
        let num = n * sxy - sx * sy;
        let den = n * sxx - sx * sx;
        if den == 0 {
            return 0;
        }
        // 放大 1000 倍再除，保留三位小数
        let v = num * 1000 / den;
        // 饱和到 i64（正常量级下根本用不到，防御性写法）
        if v > i64::MAX as i128 {
            i64::MAX
        } else if v < i64::MIN as i128 {
            i64::MIN
        } else {
            v as i64
        }
    }

    /// 漂移是否可接受：|斜率| < `limit_milli` 且没有单向增长。
    ///
    /// 两个条件都要：斜率小但首尾差很多，说明中途涨上去又跌回来
    /// （抖动也不算"零漂移"）；这里要求首尾差也在 `limit_milli * 采样数 / 1000` 以内。
    pub fn is_flat(&self, limit_milli: i64) -> bool {
        if self.count < 3 {
            return true;
        }
        let s = self.slope_milli();
        if s.abs() >= limit_milli {
            return false;
        }
        let allowed = (limit_milli * self.count as i64) / 1000;
        self.delta().abs() <= allowed.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_has_no_stats() {
        let s: Samples<8> = Samples::new();
        assert_eq!(s.len(), 0);
        assert!(s.is_empty());
        assert_eq!(s.first(), None);
        assert_eq!(s.last(), None);
        assert_eq!(s.min(), None);
        assert_eq!(s.slope_milli(), 0);
        assert_eq!(s.delta(), 0);
    }

    #[test]
    fn single_sample_slope_is_zero() {
        let mut s: Samples<8> = Samples::new();
        s.push(42);
        assert_eq!(s.slope_milli(), 0);
        assert_eq!(s.first(), Some(42));
        assert_eq!(s.last(), Some(42));
        assert!(s.is_flat(1));
    }

    #[test]
    fn constant_series_has_zero_slope_and_zero_delta() {
        let mut s: Samples<64> = Samples::new();
        for _ in 0..64 {
            s.push(129_297);
        }
        assert_eq!(s.slope_milli(), 0);
        assert_eq!(s.delta(), 0);
        assert!(s.is_flat(1), "恒定的序列必须判为不漂移");
    }

    #[test]
    fn perfect_line_slope_is_exact() {
        // y = 3x + 10 → 每点涨 3，千分斜率应是 3000
        let mut s: Samples<16> = Samples::new();
        for i in 0..16u64 {
            s.push(10 + 3 * i);
        }
        assert_eq!(s.slope_milli(), 3000);
        assert!(!s.is_flat(1));
    }

    #[test]
    fn decreasing_line_is_negative() {
        let mut s: Samples<16> = Samples::new();
        for i in 0..16u64 {
            s.push(100 - 2 * i);
        }
        assert_eq!(s.slope_milli(), -2000);
    }

    #[test]
    fn noisy_but_flat_passes() {
        // 在 1000 上下 ±3 抖动，没有单向趋势
        let mut s: Samples<32> = Samples::new();
        let pattern = [1000u64, 1003, 997, 1001, 999, 1002, 998, 1000];
        for i in 0..32 {
            s.push(pattern[i % pattern.len()]);
        }
        assert!(s.slope_milli().abs() < 100, "抖动不该被判成漂移");
        assert!(s.is_flat(1000));
    }

    #[test]
    fn slow_leak_is_detected() {
        // 每 8 个采样点少 1 页：30 天量级的隐蔽泄漏，绝对量看着微不足道，
        // 但斜率不为 0 —— 这正是长跑要抓的东西。
        let mut s: Samples<64> = Samples::new();
        for i in 0..64u64 {
            s.push(100_000 - i / 8);
        }
        assert!(s.slope_milli() < -100, "千分斜率 {} 太小", s.slope_milli());
        assert!(!s.is_flat(10));
    }

    #[test]
    fn ring_buffer_keeps_latest() {
        let mut s: Samples<4> = Samples::new();
        for i in 0..10u64 {
            s.push(i);
        }
        assert_eq!(s.len(), 4);
        assert_eq!(s.total(), 10);
        // 保留的是 6,7,8,9
        assert_eq!(s.first(), Some(6));
        assert_eq!(s.last(), Some(9));
        assert_eq!(s.min(), Some(6));
        assert_eq!(s.max(), Some(9));
    }

    #[test]
    fn slope_uses_chronological_order_after_wrap() {
        // 环绕之后仍应按时间顺序算斜率：0,1,2,3,... 递增
        let mut s: Samples<8> = Samples::new();
        for i in 0..20u64 {
            s.push(i);
        }
        // 最后 8 个是 12..19，斜率 1/点 = 1000 千分
        assert_eq!(s.slope_milli(), 1000);
    }

    #[test]
    fn delta_signs() {
        let mut s: Samples<8> = Samples::new();
        s.push(10);
        s.push(5);
        assert_eq!(s.delta(), -5);
    }

    #[test]
    fn single_step_up_is_detected() {
        // 真实泄漏的典型形态：跑着跑着多占一页，然后稳定在那儿。
        // 首尾只差 1，靠"首尾差"永远抓不住；斜率能。
        let mut s: Samples<16> = Samples::new();
        for _ in 0..8 {
            s.push(1000);
        }
        for _ in 0..8 {
            s.push(1001);
        }
        assert_eq!(s.delta(), 1);
        assert!(!s.is_flat(1), "阶跃式泄漏必须被判为漂移");
    }

    #[test]
    fn is_flat_requires_stillness_between_samples() {
        // 前提必须写清楚：`is_flat` 只适用于"每个采样点都处在同一个静止态"
        // 的序列。长跑里每轮结束都会把临时对象全部释放，
        // 所以采样点之间的抖动本身就该是 0——不是 0 就说明有东西没释放。
        let mut s: Samples<8> = Samples::new();
        for v in [1000u64, 1000, 1000, 2000, 1000, 1000, 1000, 1000] {
            s.push(v);
        }
        assert!(!s.is_flat(1), "中途波动说明有未归还的临时占用");
    }

    #[test]
    fn zero_capacity_is_safe() {
        let mut s: Samples<0> = Samples::new();
        s.push(1);
        assert_eq!(s.len(), 0);
        assert_eq!(s.slope_milli(), 0);
    }
}
