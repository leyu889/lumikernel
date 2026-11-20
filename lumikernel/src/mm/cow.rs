//! 物理页引用计数（写时复制的地基）。
//!
//! ## 为什么需要它
//!
//! `fork` 之后父子进程共享同一批物理页。这时"释放一个地址空间"不再等于
//! "释放这些页"——如果 `destroy_user_table` 照旧把每一页还给分配器，
//! 父进程会突然发现自己的一半内存被别的东西占用了。
//! 表现是父进程随机崩在 malloc 里，而崩溃现场看不出任何和 fork 有关的痕迹。
//!
//! 所以每个共享页都要有一个计数：**最后一个引用消失时才真释放**。
//!
//! ## 为什么用 u16 而不是 u32
//!
//! 1 GiB 物理内存 = 262,144 页。u16 表是 512 KiB，u32 表是 1 MiB。
//! 后者会把内核映像多撑大 512 KiB，而一个页被 32767 个地址空间同时共享
//! 这件事在那之前早就撞上进程数上限了。
//!
//! ## COW 标记藏在哪
//!
//! u16 的最高位是"这是 COW 页"的标记，低 15 位是计数。
//! 为什么不放进 PTE 的软件可用位（bit 9-11）：那样要改 `kvm::Flags` 的模型，
//! 会连带动到页表 crate 的 47 个单元测试。而这个标记只在**页错误的瞬间**
//! 需要，和页表项的生命周期完全同步，放在这里更省事，也更容易审计。
//!
//! **注意**：表是按物理地址索引的固定数组，超过 1 GiB 的页不做跟踪——
//! 那种页 `incref` 会被拒绝，`clone_user_table` 会自动退化成整页复制
//! （也就是 M9 的老行为）。宁可慢，不可错。

use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};

/// 最多跟踪多少页（1 GiB / 4 KiB）。超出的页退化成"不可共享"。
pub const MAX_TRACKED: usize = 262_144;

/// 计数饱和值（低 15 位）。
const COUNT_MAX: u16 = 0x7FFF;
/// "这一页是 COW 页"的标记位。
const COW_BIT: u16 = 0x8000;

static REFS: [AtomicU16; MAX_TRACKED] = [const { AtomicU16::new(0) }; MAX_TRACKED];

/// 因超出跟踪范围而退化成复制页的次数（正常应为 0）。
static UNTRACKED: AtomicU64 = AtomicU64::new(0);

fn idx(phys: u64) -> Option<usize> {
    let i = (phys >> 12) as usize;
    if phys & 0xFFF != 0 || i >= MAX_TRACKED {
        None
    } else {
        Some(i)
    }
}

/// 增加引用。返回 `false` 表示这页超出跟踪范围（调用方应按"独占"处理）。
///
/// **计数记录的是"总引用数"**：0 表示"这页还没被共享过"（分配者独占），
/// 第一次 `incref` 直接跳到 2——因为隐含的"分配者引用"必须一起算进去。
///
/// 之前这里写的是 0→1，后果是：fork 标记后子进程退出、`decref` 把
/// 计数减到 0 就把**父进程还在用的页**真的释放了——分配器转头把它
/// 发给别人，父进程下一次写栈就是踩别人的内存。这类 bug 的现场
/// （随机、无痕迹、复现概率与调度顺序相关）在 M11 长跑里见过近亲。
pub fn incref(phys: u64) -> bool {
    match idx(phys) {
        Some(i) => {
            let mut cur = REFS[i].load(Ordering::Acquire);
            loop {
                let c = cur & COUNT_MAX;
                let next = if c == 0 {
                    // 0 → 2：隐含的分配者引用 + 本次新引用
                    2u16
                } else {
                    (c.saturating_add(1)).min(COUNT_MAX)
                } | (cur & COW_BIT);
                match REFS[i].compare_exchange(cur, next, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => return true,
                    Err(v) => cur = v,
                }
            }
        }
        None => {
            UNTRACKED.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

/// 减少引用。返回 `true` 表示**引用归零，调用方可以真正释放这一页**。
///
/// 从未登记过的页（计数 0）也返回 `true`：那是独占页，语义与
/// 老代码里的 `free_frame` 一致。
pub fn decref(phys: u64) -> bool {
    match idx(phys) {
        Some(i) => {
            let mut cur = REFS[i].load(Ordering::Acquire);
            loop {
                let c = cur & COUNT_MAX;
                if c <= 1 {
                    match REFS[i].compare_exchange(cur, 0, Ordering::AcqRel, Ordering::Acquire) {
                        Ok(_) => return true,
                        Err(v) => {
                            cur = v;
                            continue;
                        }
                    }
                }
                match REFS[i].compare_exchange(
                    cur,
                    (c - 1) | (cur & COW_BIT),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return false,
                    Err(v) => cur = v,
                }
            }
        }
        None => true,
    }
}

/// 把一页标记为 COW 页（写时复制）。
pub fn mark_cow(phys: u64) {
    if let Some(i) = idx(phys) {
        REFS[i].fetch_or(COW_BIT, Ordering::AcqRel);
    }
}

pub fn is_cow(phys: u64) -> bool {
    match idx(phys) {
        Some(i) => REFS[i].load(Ordering::Acquire) & COW_BIT != 0,
        None => false,
    }
}

pub fn clear_cow(phys: u64) {
    if let Some(i) = idx(phys) {
        REFS[i].fetch_and(!COW_BIT, Ordering::AcqRel);
    }
}

/// 把一页彻底从表里抹掉（释放时调用）。
pub fn clear_all(phys: u64) {
    if let Some(i) = idx(phys) {
        REFS[i].store(0, Ordering::Release);
    }
}

/// 当前引用数（0 = 没有登记过，通常意味着独占页）。
pub fn refcount(phys: u64) -> u16 {
    match idx(phys) {
        Some(i) => REFS[i].load(Ordering::Acquire) & COUNT_MAX,
        None => 0,
    }
}

/// 超过跟踪范围而退化成复制的次数。
pub fn untracked() -> u64 {
    UNTRACKED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    // 宿主机上跑不了（静态表 + 原子够大，但 `kvm` 的 Flags 用不上），
    // 所以这里只测索引边界这个纯逻辑。
    use super::*;

    #[test]
    fn index_rejects_unaligned_and_out_of_range() {
        assert!(idx(0).is_some());
        assert!(idx(0x1000).is_some());
        assert!(idx(0x800).is_none(), "未对齐的地址不该被跟踪");
        assert!(idx((MAX_TRACKED as u64) << 12).is_none(), "超范围要拒绝");
    }
}
