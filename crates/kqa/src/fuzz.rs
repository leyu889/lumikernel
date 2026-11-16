//! 非法输入生成（S4 的弹药库）。
//!
//! 纯随机的 64 位整数几乎永远命不中边界：随机值落在
//! `[0x1000, 0x1001]` 的概率是 2^-64。所以 fuzz 的核心不是"随机"，
//! 而是**把边界值按权重混进去**：页边界、用户地址上界、内核地址、
//! `-1`、`i64::MIN`、刚越过合法范围一格的值……
//!
//! 这套取值表来自 M5–M10 实际踩过的坑（见 `docs/04-工程规约.md`）：
//! 每个值都对应过至少一次真实故障。

use crate::rng::Rng;

/// 用户地址上界（与 `exec::USER_LIMIT` 一致）。越界的用户指针必须被拒。
pub const USER_LIMIT: u64 = 0x0000_8000_0000_0000;

/// 内核高半区基址（`KERNEL_VA_BASE`）。
pub const KERNEL_BASE: u64 = 0xFFFF_FFFF_8000_0000;

/// 直接映射区基址（`DIRECT_MAP_BASE`）。
pub const DIRECT_MAP_BASE: u64 = 0xFFFF_8000_0000_0000;

/// "有意思的" 64 位值。
///
/// 顺序即优先级：靠前的在 [`interesting_u64`] 里被抽中的概率更高
/// （用两段式取样：先在小索引段里挑，再偶尔挑整张表）。
pub const INTERESTING: [u64; 26] = [
    0,
    1,
    2,
    3,
    0xFFF,                        // 页尾
    0x1000,                       // 一页
    0x1001,                       // 刚过一页
    0x7FFF_FFFF,                  // i32 上界
    0x8000_0000,                  // i32 上界 +1（符号位）
    0xFFFF_FFFF,                  // u32 上界
    0x1_0000_0000,                // u32 上界 +1
    0x0000_7FFF_FFFF_F000,        // 用户区最后一页
    USER_LIMIT - 1,               // 用户上界 -1
    USER_LIMIT,                   // 用户上界（非法）
    USER_LIMIT + 1,               // 刚越界
    0x0000_0000_4000_0000,        // 1 GiB 处（典型 mmap 区）
    i64::MAX as u64,              // 有符号上界
    i64::MIN as u64,              // 有符号下界（负数当 size 是经典 fuzz 套路）
    u64::MAX,                     // -1
    u64::MAX - 0xFFF,             // 负数 + 页对齐
    KERNEL_BASE,                  // 内核基址（用户不该碰）
    DIRECT_MAP_BASE,              // 直接映射区
    0xFFFF_FFFF_FFFF_EFFF,        // 高半区最后一页之前
    0x0000_0000_F000_0000,        // 3.75 GiB 处（32 位地址空间的顶端）
    0xDEAD_BEEF_DEAD_BEEF,        // 经典魔数
    0xA5A5_A5A5_A5A5_A5A5,        // 另一个魔数（填充值常被当指针）
];

/// 是不是"有意思"的值（自检用）。
pub fn is_interesting(v: u64) -> bool {
    INTERESTING.contains(&v)
}

/// 生成一个用户态系统调用参数。
///
/// 分布：62.5% 从"小值 + 边界值"里挑，25% 从整张表里挑，12.5% 纯随机。
/// 纯随机那一档负责撞"没人想到的组合"，边界那一档负责**今天就能撞出问题**。
pub fn arg(rng: &mut Rng) -> u64 {
    match rng.below(8) {
        0 => rng.next_u64(),                       // 12.5% 纯随机
        1 => INTERESTING[rng.below(INTERESTING.len() as u32) as usize],
        2 | 3 => INTERESTING[rng.below(6) as usize], // 小值区（0/1/2/3/0xFFF/0x1000）
        _ => INTERESTING[rng.below(16) as usize],
    }
}

/// 生成一个"长度"参数：偏向 0、1、页边界，偶尔给个巨大的数。
pub fn len(rng: &mut Rng) -> u64 {
    match rng.below(8) {
        0 => rng.next_u64(),                 // 巨大值
        1 => 0x1000 + rng.below(4) as u64,   // 页边界附近
        2 => 0,
        3 => 1,
        _ => rng.below(0x2000) as u64,       // 0..8 KiB
    }
}

/// 生成一个文件描述符：把 0..8 与"明显非法"的值混在一起。
pub fn fd(rng: &mut Rng) -> u64 {
    match rng.below(4) {
        0 => 0xFFFF_FFFF,          // -1
        1 => u64::MAX - rng.below(3) as u64,
        _ => rng.below(12) as u64,
    }
}

/// 生成一条"看起来像路径"的字节串，长度 `1..=max`。
///
/// 里面会混入 `\0`、`/../`、超长组件、非 UTF-8 字节——这些正是
/// 路径解析最容易出错的地方（M9 的 `ELOOP` / `ENAMETOOLONG` 就是这么找出来的）。
pub fn path(rng: &mut Rng, out: &mut [u8]) -> usize {
    if out.is_empty() {
        return 0;
    }
    const ATOMS: [&[u8]; 8] = [
        b"/",
        b"..",
        b".",
        b"bin",
        b"busybox",
        b"hello.txt",
        b"\xff\xfe",
        b"a",
    ];
    let mut n = 0usize;
    // 起点：一半以 '/' 开头（绝对路径），一半不以（相对路径 → 应当被拒）
    if rng.chance(500) && n < out.len() {
        out[n] = b'/';
        n += 1;
    }
    while n < out.len() && rng.chance(850) {
        let atom = ATOMS[rng.below(ATOMS.len() as u32) as usize];
        let take = core::cmp::min(atom.len(), out.len() - n);
        out[n..n + take].copy_from_slice(&atom[..take]);
        n += take;
    }
    // 偶尔塞一个 NUL 在中间（C 字符串层面会截断，内核不该越界读）
    if n > 0 && rng.chance(80) {
        out[n / 2] = 0;
    }
    n
}

/// 在 `data` 上做 `rounds` 轮随机变异。
///
/// 六种变异，每种都在真实镜像 fuzz 里必用：翻位、置 0、置 0xFF、
/// 加减一、复制粘贴一段、整体截断。前三者撞"字段错位"，
/// 后三者撞"长度/偏移不一致"——文件系统崩得最多的就是这类。
pub fn mutate(data: &mut [u8], rng: &mut Rng, rounds: usize) {
    if data.is_empty() {
        return;
    }
    for _ in 0..rounds {
        let i = rng.below(data.len() as u32) as usize;
        match rng.below(6) {
            0 => data[i] ^= 1 << rng.below(8),
            1 => data[i] = 0,
            2 => data[i] = 0xFF,
            3 => data[i] = data[i].wrapping_add(1),
            4 => {
                // 复制粘贴：把另一处的一段搬到 i，制造"两份一模一样"的元数据
                let j = rng.below(data.len() as u32) as usize;
                let n = core::cmp::min(rng.below(8) as usize + 1, data.len() - i.min(j));
                let (s, d) = if j <= i { (j, i) } else { (i, j) };
                let n = n.min(data.len() - d).min(data.len() - s);
                for k in 0..n {
                    data[d + k] = data[s + k];
                }
            }
            _ => {
                // 截断：把尾部一段清零（模拟"扇区只写了一部分"）
                let n = core::cmp::min(rng.below(64) as usize, data.len() - i);
                for b in &mut data[i..i + n] {
                    *b = 0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arg_mostly_lands_on_interesting_values() {
        let mut rng = Rng::new(2026);
        let mut hits = 0;
        const N: usize = 10_000;
        for _ in 0..N {
            if is_interesting(arg(&mut rng)) {
                hits += 1;
            }
        }
        // 理论上 7/8 命中；留出余量，主要防"取值表和取样逻辑脱节"
        assert!(hits > N * 7 / 10, "只有 {hits}/{N} 命中边界值，分布写错了");
    }

    #[test]
    fn table_has_no_duplicates() {
        // 踩过一次：表里放了 `0xFFF` 和 `0x0000_0000_0000_0FFF`（同一个值），
        // 于是"每个值都被抽到过"永远不成立——重复项会被前一项吃掉。
        for i in 0..INTERESTING.len() {
            for j in (i + 1)..INTERESTING.len() {
                assert_ne!(
                    INTERESTING[i], INTERESTING[j],
                    "取值表第 {i} 项与第 {j} 项重复：{:#x}",
                    INTERESTING[i]
                );
            }
        }
    }

    #[test]
    fn arg_covers_the_whole_table() {
        let mut rng = Rng::new(1);
        let mut seen = [false; INTERESTING.len()];
        for _ in 0..200_000 {
            let v = arg(&mut rng);
            if let Some(i) = INTERESTING.iter().position(|&x| x == v) {
                seen[i] = true;
            }
        }
        let missing: usize = seen.iter().filter(|&&s| !s).count();
        assert_eq!(missing, 0, "取值表里有值从来没被抽到过");
    }

    #[test]
    fn user_limit_itself_is_in_the_table() {
        assert!(is_interesting(USER_LIMIT));
        assert!(is_interesting(USER_LIMIT + 1));
        assert!(is_interesting(KERNEL_BASE));
    }

    #[test]
    fn len_leans_small() {
        let mut rng = Rng::new(5);
        let mut small = 0;
        for _ in 0..10_000 {
            if len(&mut rng) <= 0x2000 {
                small += 1;
            }
        }
        assert!(small > 8000, "长度参数应当偏向小值，实际 {small}/10000");
    }

    #[test]
    fn fd_includes_invalid_values() {
        let mut rng = Rng::new(9);
        let mut invalid = 0;
        for _ in 0..1000 {
            if fd(&mut rng) > 100 {
                invalid += 1;
            }
        }
        assert!(invalid > 150, "非法 fd 比例太低：{invalid}/1000");
    }

    #[test]
    fn path_is_bounded_and_sometimes_relative() {
        let mut rng = Rng::new(77);
        let mut rel = 0;
        for _ in 0..1000 {
            let mut buf = [0u8; 64];
            let n = path(&mut rng, &mut buf);
            assert!(n <= buf.len(), "路径长度越过缓冲：{n}");
            if n == 0 || buf[0] != b'/' {
                rel += 1;
            }
        }
        assert!(rel > 100, "相对路径比例太低，{rel}/1000");
    }

    #[test]
    fn path_into_empty_buffer_is_zero() {
        let mut rng = Rng::new(1);
        let mut buf: [u8; 0] = [];
        assert_eq!(path(&mut rng, &mut buf), 0);
    }

    #[test]
    fn mutate_changes_bytes_but_keeps_length() {
        let mut rng = Rng::new(3);
        let mut data = [0xAAu8; 512];
        let before = data;
        mutate(&mut data, &mut rng, 32);
        assert_eq!(data.len(), 512);
        assert_ne!(data, before, "32 轮变异之后内容居然没变");
    }

    #[test]
    fn mutate_empty_is_noop() {
        let mut rng = Rng::new(1);
        let mut data: [u8; 0] = [];
        mutate(&mut data, &mut rng, 100);
    }

    #[test]
    fn mutate_never_panics_on_tiny_buffers() {
        let mut rng = Rng::new(4);
        for size in 1..40usize {
            let mut data = vec_of_ones(size);
            mutate(&mut data, &mut rng, 64);
        }
    }

    fn vec_of_ones(n: usize) -> std::vec::Vec<u8> {
        let mut v = std::vec::Vec::with_capacity(n);
        v.resize(n, 1u8);
        v
    }
}
