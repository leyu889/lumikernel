//! 确定性随机数（PCG32）。
//!
//! 为什么不用"时间当种子"：fuzz 报出来一个崩溃，第一件事是**重放**。
//! 种子一固定，输入序列就完全可复现——这不是讲究，是能不能定位的分界线。
//!
//! 选 PCG 而不是 xorshift：PCG 的状态转移是乘加（周期 2^64），
//! 输出经过 XSH-RR 置换，低位质量远好于裸 xorshift（后者低位近似线性，
//! 拿来做 `below(3)` 会明显偏）。

/// PCG32 的乘数（标准值，来自 pcg-random.org 的参考实现）。
const MUL: u64 = 6_364_136_223_846_793_005;

/// 确定性伪随机数发生器。
#[derive(Clone)]
pub struct Rng {
    state: u64,
    inc: u64,
}

impl Rng {
    /// 用给定种子建一个发生器（流号固定为 1，保证同种子同序列）。
    pub fn new(seed: u64) -> Self {
        let mut r = Rng {
            state: 0,
            inc: (1u64 << 1) | 1,
        };
        r.next_u32();
        r.state = r.state.wrapping_add(seed);
        r.next_u32();
        r
    }

    /// 换一条独立流（同种子不同流 → 两条互不相关的序列）。
    pub fn with_stream(seed: u64, stream: u64) -> Self {
        let mut r = Rng {
            state: 0,
            inc: (stream << 1) | 1,
        };
        r.next_u32();
        r.state = r.state.wrapping_add(seed);
        r.next_u32();
        r
    }

    pub fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old.wrapping_mul(MUL).wrapping_add(self.inc);
        // XSH-RR：先做 xorshift 右移，再按高位做循环右移
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }

    pub fn next_u64(&mut self) -> u64 {
        ((self.next_u32() as u64) << 32) | self.next_u32() as u64
    }

    /// `[0, n)` 上的均匀分布。`n == 0` 返回 0。
    ///
    /// 用"高位乘法"而不是取模：取模会让小的余数出现得更频繁（低位偏差），
    /// 在 n 不整除 2^32 时最明显。这里用 64 位乘法取高位，
    /// 偏差上界是 n / 2^32（对 n < 2^16 可以忽略），且没有拒绝循环。
    pub fn below(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        (((self.next_u32() as u64) * (n as u64)) >> 32) as u32
    }

    /// `[lo, hi)` 上的均匀分布。`hi <= lo` 时返回 `lo`。
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        let span = hi - lo;
        lo + (self.next_u64() % span)
    }

    /// 以 `permille / 1000` 的概率为真。
    pub fn chance(&mut self, permille: u32) -> bool {
        if permille >= 1000 {
            return true;
        }
        self.below(1000) < permille
    }

    /// 填满一段字节（用于生成 fuzz 输入）。
    pub fn fill(&mut self, out: &mut [u8]) {
        let mut i = 0;
        while i < out.len() {
            let v = self.next_u32().to_le_bytes();
            let n = core::cmp::min(4, out.len() - i);
            out[i..i + n].copy_from_slice(&v[..n]);
            i += n;
        }
    }

    /// 从切片里挑一个。
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            return None;
        }
        items.get(self.below(items.len() as u32) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Rng::new(0xC0FFEE);
        let mut b = Rng::new(0xC0FFEE);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seed_different_sequence() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        let mut same = 0;
        for _ in 0..64 {
            if a.next_u64() == b.next_u64() {
                same += 1;
            }
        }
        assert_eq!(same, 0, "两个不同种子不该产生相同序列");
    }

    #[test]
    fn different_stream_is_decorrelated() {
        let mut a = Rng::with_stream(7, 1);
        let mut b = Rng::with_stream(7, 2);
        let mut same = 0;
        for _ in 0..128 {
            if a.next_u64() == b.next_u64() {
                same += 1;
            }
        }
        assert_eq!(same, 0);
    }

    #[test]
    fn below_is_in_range_and_roughly_uniform() {
        let mut r = Rng::new(42);
        let mut buckets = [0u32; 10];
        for _ in 0..100_000 {
            let v = r.below(10);
            assert!(v < 10);
            buckets[v as usize] += 1;
        }
        // 每桶期望 10000；容差 10% 足够宽松，又能抓住"低位偏斜"这类真问题
        for (i, &c) in buckets.iter().enumerate() {
            assert!(
                c > 9000 && c < 11000,
                "桶 {i} 计数 {c} 偏离期望 10000 太多"
            );
        }
    }

    #[test]
    fn below_zero_is_zero() {
        let mut r = Rng::new(1);
        assert_eq!(r.below(0), 0);
    }

    #[test]
    fn below_one_is_always_zero() {
        let mut r = Rng::new(1);
        for _ in 0..100 {
            assert_eq!(r.below(1), 0);
        }
    }

    #[test]
    fn range_covers_both_ends() {
        let mut r = Rng::new(7);
        let mut lo_hit = false;
        let mut hi_hit = false;
        for _ in 0..10_000 {
            let v = r.range(5, 8);
            assert!((5..8).contains(&v));
            lo_hit |= v == 5;
            hi_hit |= v == 7;
        }
        assert!(lo_hit && hi_hit);
    }

    #[test]
    fn range_with_empty_span_returns_lo() {
        let mut r = Rng::new(3);
        assert_eq!(r.range(9, 9), 9);
        assert_eq!(r.range(9, 3), 9);
    }

    #[test]
    fn chance_extremes_are_exact() {
        let mut r = Rng::new(5);
        for _ in 0..1000 {
            assert!(!r.chance(0));
            assert!(r.chance(1000));
        }
    }

    #[test]
    fn chance_half_is_about_half() {
        let mut r = Rng::new(9);
        let mut yes = 0;
        for _ in 0..20_000 {
            if r.chance(500) {
                yes += 1;
            }
        }
        assert!(yes > 9000 && yes < 11000, "命中 {yes} 次，偏离一半太多");
    }

    #[test]
    fn fill_writes_every_byte() {
        let mut r = Rng::new(11);
        let mut buf = [0u8; 37];
        r.fill(&mut buf);
        // 37 字节里全零的概率是 2^-296，出现就说明 fill 没写满
        assert!(buf.iter().any(|&b| b != 0));
        // 再填一次应当不同
        let mut buf2 = [0u8; 37];
        r.fill(&mut buf2);
        assert_ne!(buf, buf2);
    }

    #[test]
    fn fill_empty_is_noop() {
        let mut r = Rng::new(11);
        r.fill(&mut []);
    }

    #[test]
    fn pick_from_empty_is_none() {
        let mut r = Rng::new(1);
        let empty: [u8; 0] = [];
        assert!(r.pick(&empty).is_none());
    }

    #[test]
    fn pick_never_panics_and_hits_all_items() {
        let mut r = Rng::new(13);
        let items = [10u8, 20, 30];
        let mut seen = [false; 3];
        for _ in 0..1000 {
            let v = *r.pick(&items).unwrap();
            let idx = items.iter().position(|&x| x == v).unwrap();
            seen[idx] = true;
        }
        assert_eq!(seen, [true; 3]);
    }
}
