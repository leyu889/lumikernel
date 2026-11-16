//! 路径规范化：把 `(cwd, 相对或绝对路径)` 变成一条"干净的绝对路径"
//!
//! 为什么必须先规整再解析：`kfs` 只认绝对路径，而且**遇到 `..` 会当成普通名字**
//! 去查目录——查不到就报不存在。用户态传 `../bin/ls`、`./a/../b` 这类路径
//! 是家常便饭（`sh` 的 PATH 搜索、`ls` 的相对路径），不规整就会莫名其妙地
//! "文件明明在，就是打不开"。
//!
//! 实现分两遍：第一遍只压组件（记下"来自 cwd 还是 path、起点、长度"），
//! 第二遍才往输出缓冲里拼。**不能边压边写**——组件长度不同的时候，
//! 后一个组件会覆盖前一个（这个坑我踩过一次，症状是路径变成半截）。

use crate::errno::{err, EINVAL, ENAMETOOLONG, ENOENT};

/// 组件栈的最大深度。超过就 `ENAMETOOLONG`——比无限套 `..` 把栈写爆安全。
const MAX_COMPONENTS: usize = 32;

/// 一个组件来自哪段输入。
#[derive(Clone, Copy)]
struct Comp {
    from_path: bool,
    start: u16,
    len: u16,
}

struct Stack {
    entries: [Comp; MAX_COMPONENTS],
    n: usize,
}

/// 规范化路径。
///
/// - `cwd` 必须是绝对路径；
/// - `path` 为空 → `ENOENT`（与 Linux 一致）；
/// - 结果的根是 `"/"`，不会带结尾斜杠（除非结果就是 `"/"`）。
pub fn normalize(cwd: &[u8], path: &[u8], out: &mut [u8]) -> Result<usize, i64> {
    if path.is_empty() {
        return Err(err(ENOENT));
    }

    let mut st = Stack {
        entries: [Comp {
            from_path: false,
            start: 0,
            len: 0,
        }; MAX_COMPONENTS],
        n: 0,
    };

    // 1) 相对路径：先把 cwd 的组件压进去
    if path[0] != b'/' {
        if cwd.first() != Some(&b'/') {
            return Err(err(EINVAL));
        }
        push_components(cwd, false, &mut st)?;
    }
    // 2) 再压路径自己的组件
    push_components(path, true, &mut st)?;

    // 3) 拼成 "/a/b"
    if out.is_empty() {
        return Err(err(ENAMETOOLONG));
    }
    out[0] = b'/';
    let mut pos = 1usize;
    for i in 0..st.n {
        let c = st.entries[i];
        let src = if c.from_path { path } else { cwd };
        let (s, l) = (c.start as usize, c.len as usize);
        let comp = match src.get(s..s + l) {
            Some(v) => v,
            None => return Err(err(EINVAL)),
        };
        if i > 0 {
            if pos >= out.len() {
                return Err(err(ENAMETOOLONG));
            }
            out[pos] = b'/';
            pos += 1;
        }
        let end = pos + l;
        if end > out.len() {
            return Err(err(ENAMETOOLONG));
        }
        out[pos..end].copy_from_slice(comp);
        pos = end;
    }
    Ok(pos)
}

/// 把 `p` 里以 `/` 分隔的组件压栈，处理 `.` 与 `..`。
///
/// 空组件（`//`）直接跳过；`..` 在栈为空时被忽略（等价于 `cd /..` = `/`）。
fn push_components(p: &[u8], from_path: bool, st: &mut Stack) -> Result<(), i64> {
    let mut i = 0usize;
    while i < p.len() {
        while i < p.len() && p[i] == b'/' {
            i += 1;
        }
        let start = i;
        while i < p.len() && p[i] != b'/' {
            i += 1;
        }
        let comp = &p[start..i];
        if comp.is_empty() || comp == b"." {
            continue;
        }
        if comp == b".." {
            if st.n > 0 {
                st.n -= 1;
            }
            continue;
        }
        if st.n >= MAX_COMPONENTS || start > u16::MAX as usize || comp.len() > u16::MAX as usize {
            return Err(err(ENAMETOOLONG));
        }
        st.entries[st.n] = Comp {
            from_path,
            start: start as u16,
            len: comp.len() as u16,
        };
        st.n += 1;
    }
    Ok(())
}

/// 取路径的最后一段（`basename` 的简化版）。`/` 返回 `None`。
pub fn basename(p: &[u8]) -> Option<&[u8]> {
    let mut end = p.len();
    while end > 0 && p[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let mut start = end;
    while start > 0 && p[start - 1] != b'/' {
        start -= 1;
    }
    Some(&p[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(cwd: &str, p: &str) -> Result<(usize, [u8; 128]), i64> {
        let mut out = [0u8; 128];
        let n = normalize(cwd.as_bytes(), p.as_bytes(), &mut out)?;
        Ok((n, out))
    }

    fn s(cwd: &str, p: &str) -> String {
        let (n, out) = norm(cwd, p).unwrap();
        core::str::from_utf8(&out[..n]).unwrap().into()
    }

    #[test]
    fn absolute_stays_absolute() {
        assert_eq!(s("/", "/bin/ls"), "/bin/ls");
        assert_eq!(s("/tmp", "/bin/ls"), "/bin/ls");
    }

    #[test]
    fn relative_joins_cwd() {
        assert_eq!(s("/", "hello.txt"), "/hello.txt");
        assert_eq!(s("/bin", "ls"), "/bin/ls");
        assert_eq!(s("/", "./bin/./ls"), "/bin/ls");
    }

    #[test]
    fn dotdot_pops() {
        assert_eq!(s("/a/b/c", ".."), "/a/b");
        assert_eq!(s("/a/b", "../../.."), "/");
        assert_eq!(s("/", ".."), "/");
        assert_eq!(s("/a/b", "../x"), "/a/x");
    }

    #[test]
    fn double_slashes_collapse() {
        assert_eq!(s("/", "//bin///ls"), "/bin/ls");
        assert_eq!(s("/", "/"), "/");
    }

    #[test]
    fn trailing_slash_dropped() {
        assert_eq!(s("/", "/bin/"), "/bin");
    }

    #[test]
    fn mixed_length_components_do_not_overwrite_each_other() {
        // 这条是防回归的：早期版本边压边往缓冲尾部写，
        // 短组件会被后一个长组件盖掉，结果路径变成 "/bin/usr/bin"
        assert_eq!(s("/", "/usr/lib/xyz"), "/usr/lib/xyz");
        assert_eq!(s("/a", "bbbbbb/c/..___/d"), "/a/bbbbbb/c/..___/d");
        assert_eq!(s("/", "/a/bbbbbbbbbb/c"), "/a/bbbbbbbbbb/c");
    }

    #[test]
    fn empty_path_is_error() {
        assert_eq!(norm("/", "").unwrap_err(), err(ENOENT));
    }

    #[test]
    fn too_many_components_is_not_a_crash() {
        let mut p = [0u8; 512];
        p[0] = b'/';
        let mut i = 1;
        while i + 2 < p.len() {
            p[i] = b'a';
            p[i + 1] = b'/';
            i += 2;
        }
        let mut out = [0u8; 64];
        assert!(normalize(b"/", &p[..i], &mut out).is_err());
    }

    #[test]
    fn output_buffer_too_small_is_error_not_overflow() {
        let mut out = [0u8; 4];
        assert!(normalize(b"/", b"/abcdefgh", &mut out).is_err());
    }

    #[test]
    fn relative_cwd_must_be_absolute() {
        assert!(norm("tmp", "x").is_err());
    }

    #[test]
    fn basename_works() {
        assert_eq!(basename(b"/bin/ls"), Some(&b"ls"[..]));
        assert_eq!(basename(b"/bin/ls///"), Some(&b"ls"[..]));
        assert_eq!(basename(b"/"), None);
    }
}
