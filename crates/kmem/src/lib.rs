//! 物理帧分配器（参考实现，供其余 crate 对齐代码风格）
//!
//! 硬性约束（写进 docs/04-工程规约.md，所有 crate 必须遵守）：
//! 1. 不允许隐式失败：分配不到就返回 `None`，不许 panic、不许阻塞。
//! 2. 每个 `unsafe` 必须写明"为什么这里安全"。
//! 3. 所有公开 API 都要有单元测试，且测试跑在宿主机上（`cargo test -p kmem`）。
//! 4. 分配器自身不得分配内存（bitmap 由调用方提供）。

#![cfg_attr(not(test), no_std)]

pub const PAGE_SIZE: usize = 4096;

/// 物理页帧。`0` 号帧永远不分配（空指针保护区）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Frame(pub usize);

impl Frame {
    pub const fn addr(self) -> usize {
        self.0 * PAGE_SIZE
    }
}

/// 位图物理帧分配器：1 bit = 1 帧，1 表示已占用。
///
/// 不变量：
/// - `bitmap.len() * 8 >= total_frames`
/// - `free == total_frames - 已置位帧数`
/// - 第 0 帧始终标记为已占用
pub struct FrameAllocator<'a> {
    bitmap: &'a mut [u8],
    total_frames: usize,
    hint: usize,
    free: usize,
}

impl<'a> FrameAllocator<'a> {
    /// 创建一个"全部可用"的分配器；第 0 帧会被占用。
    pub fn new(bitmap: &'a mut [u8], total_frames: usize) -> Option<Self> {
        if total_frames == 0 || bitmap.len() * 8 < total_frames {
            return None;
        }
        for b in bitmap.iter_mut() {
            *b = 0;
        }
        let mut me = Self {
            bitmap,
            total_frames,
            hint: 1,
            free: total_frames,
        };
        me.mark_used(Frame(0));
        Some(me)
    }

    /// 创建一个"全部已占用"的分配器，供 E820 初始化使用。
    ///
    /// 内核启动时的正确姿势是：先假定整片物理地址都不可用，再对 E820 中
    /// `type == 1` 的区间逐帧 [`FrameAllocator::mark_free`]。反过来做
    /// （先全空闲再标占用）只要漏掉一个保留区间，就会把内核自己分配出去。
    pub fn new_all_used(bitmap: &'a mut [u8], total_frames: usize) -> Option<Self> {
        if total_frames == 0 || bitmap.len() * 8 < total_frames {
            return None;
        }
        for b in bitmap.iter_mut() {
            *b = 0xFF;
        }
        Some(Self {
            bitmap,
            total_frames,
            hint: 1,
            free: 0,
        })
    }

    #[inline]
    fn is_used(&self, f: usize) -> bool {
        self.bitmap[f / 8] & (1 << (f % 8)) != 0
    }

    #[inline]
    fn set_bit(&mut self, f: usize, used: bool) {
        let byte = &mut self.bitmap[f / 8];
        let mask = 1u8 << (f % 8);
        let was = *byte & mask != 0;
        if used && !was {
            *byte |= mask;
            self.free -= 1;
        } else if !used && was {
            *byte &= !mask;
            self.free += 1;
        }
    }

    /// 查询某一帧是否已被占用（自检与诊断用）。
    pub fn frame_is_used(&self, f: Frame) -> bool {
        f.0 < self.total_frames && self.is_used(f.0)
    }

    pub fn total_frames(&self) -> usize {
        self.total_frames
    }

    pub fn free_frames(&self) -> usize {
        self.free
    }

    /// 标记一帧已占用（用于保留内核镜像、multiboot 结构、MMIO 等区域）。
    pub fn mark_used(&mut self, f: Frame) {
        if f.0 < self.total_frames {
            self.set_bit(f.0, true);
        }
    }

    pub fn mark_free(&mut self, f: Frame) {
        if f.0 != 0 && f.0 < self.total_frames {
            self.set_bit(f.0, false);
        }
    }

    /// 分配一帧：从 hint 开始向后找，找不到再从 0 开始（循环一次）。
    pub fn alloc(&mut self) -> Option<Frame> {
        if self.free == 0 {
            return None;
        }
        let start = self.hint.max(1);
        if let Some(f) = self.scan(start, self.total_frames) {
            self.hint = f + 1;
            return Some(Frame(f));
        }
        if let Some(f) = self.scan(1, start) {
            self.hint = f + 1;
            return Some(Frame(f));
        }
        None
    }

    fn scan(&mut self, from: usize, to: usize) -> Option<usize> {
        for f in from..to {
            if !self.is_used(f) {
                self.set_bit(f, true);
                return Some(f);
            }
        }
        None
    }

    /// 分配 `count` 个连续帧（DMA 缓冲常用）。失败时不做任何改动。
    pub fn alloc_contiguous(&mut self, count: usize) -> Option<Frame> {
        if count == 0 || count > self.free {
            return None;
        }
        let mut run = 0usize;
        let mut start = 0usize;
        for f in 1..self.total_frames {
            if self.is_used(f) {
                run = 0;
                continue;
            }
            if run == 0 {
                start = f;
            }
            run += 1;
            if run == count {
                for g in start..start + count {
                    self.set_bit(g, true);
                }
                self.hint = start + count;
                return Some(Frame(start));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allocator(frames: usize) -> (Vec<u8>, usize) {
        (vec![0u8; frames.div_ceil(8)], frames)
    }

    #[test]
    fn new_rejects_tiny_bitmap() {
        let mut bits = [0u8; 1];
        assert!(FrameAllocator::new(&mut bits, 9).is_none());
        assert!(FrameAllocator::new(&mut bits, 0).is_none());
    }

    #[test]
    fn all_used_then_free_ranges() {
        let (mut bits, n) = allocator(128);
        let mut a = FrameAllocator::new_all_used(&mut bits, n).unwrap();
        assert_eq!(a.free_frames(), 0);
        assert!(a.alloc().is_none());
        for f in 10..20 {
            a.mark_free(Frame(f));
        }
        assert_eq!(a.free_frames(), 10);
        let got: Vec<usize> = (0..10).filter_map(|_| a.alloc().map(|f| f.0)).collect();
        assert_eq!(got, (10..20).collect::<Vec<_>>());
        assert!(a.alloc().is_none());
    }

    #[test]
    fn frame_zero_is_reserved() {
        let (mut bits, n) = allocator(64);
        let a = FrameAllocator::new(&mut bits, n).unwrap();
        assert_eq!(a.free_frames(), n - 1);
        assert!(a.is_used(0));
    }

    #[test]
    fn allocates_everything_then_fails_cleanly() {
        let (mut bits, n) = allocator(64);
        let mut a = FrameAllocator::new(&mut bits, n).unwrap();
        let mut seen = Vec::new();
        while let Some(f) = a.alloc() {
            assert_ne!(f.0, 0);
            seen.push(f.0);
        }
        assert_eq!(seen.len(), n - 1);
        assert_eq!(a.free_frames(), 0);
        assert!(a.alloc().is_none());
        // 每条帧只出现一次
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), n - 1);
    }

    #[test]
    fn freed_frame_is_reused() {
        let (mut bits, n) = allocator(32);
        let mut a = FrameAllocator::new(&mut bits, n).unwrap();
        let f = a.alloc().unwrap();
        assert_eq!(a.free_frames(), n - 2);
        a.mark_free(f);
        assert_eq!(a.free_frames(), n - 1);
        // 分配器是 hint 优先的：刚释放的帧不保证立刻被选中，
        // 但继续分配一定会重新拿到它（不能出现"释放后永远漏掉"）。
        let mut reused = false;
        while let Some(g) = a.alloc() {
            if g == f {
                reused = true;
                break;
            }
        }
        assert!(reused, "释放的帧 {} 再也没被分配出来", f.0);
    }

    #[test]
    fn reserved_regions_are_never_handed_out() {
        let (mut bits, n) = allocator(128);
        let mut a = FrameAllocator::new(&mut bits, n).unwrap();
        for f in 10..20 {
            a.mark_used(Frame(f));
        }
        while let Some(f) = a.alloc() {
            assert!(!(10..20).contains(&f.0), "分配到了保留帧 {}", f.0);
        }
    }

    #[test]
    fn contiguous_runs() {
        let (mut bits, n) = allocator(128);
        let mut a = FrameAllocator::new(&mut bits, n).unwrap();
        a.mark_used(Frame(5));
        let f = a.alloc_contiguous(4).unwrap();
        assert_eq!(f.0, 1);
        for g in 1..5 {
            assert!(a.is_used(g));
        }
        // 0 号帧保留 + 手动占用的 5 号帧 + 连续 4 帧
        assert_eq!(a.free_frames(), n - 1 - 1 - 4);
    }

    #[test]
    fn contiguous_failure_does_not_mutate() {
        let (mut bits, n) = allocator(16);
        let mut a = FrameAllocator::new(&mut bits, n).unwrap();
        let before = a.free_frames();
        assert!(a.alloc_contiguous(n).is_none());
        assert_eq!(a.free_frames(), before);
    }
}
