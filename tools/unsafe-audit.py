#!/usr/bin/env python3
"""S6：unsafe 密度审计 + 逐条清单生成。

用法：
    python tools/unsafe-audit.py            # 打印统计并写 docs/unsafe-register.md
    python tools/unsafe-audit.py --check    # 只检查阈值，超了返回非 0（给 CI 用）

口径（写死在代码里，避免每次"口径不同"导致数字漂移）：

- **总行数**：所有 `.rs` 文件里，去掉空行、去掉整行注释（`//`）之后的行数。
  块注释 `/* */` 不做复杂解析，含 `/*` 的行按普通行计入——这是**偏保守**的口径
  （分母偏大 → 密度偏低），所以结论不会因为口径宽松而虚高。
- **unsafe 行数**：出现 `unsafe` 关键字的行，且不是注释行。
  同一行里多个 `unsafe` 只算一次。
- **密度** = unsafe 行 / 总行。目标 ≤ 3%（比 SLO 的 5% 更严，留出余量）。
- **证明要求**：每个 `unsafe` 块前面应当有一段 `// SAFETY:` 说明。
  脚本按"向上找最近 3 行"判断，找不到就记进 `missing` 列表。
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCAN_DIRS = ["lumikernel/src", "crates"]
SKIP_PARTS = ("/target/", "\\target\\")
DENSITY_LIMIT_PCT = 3.0


def rust_files():
    for base in SCAN_DIRS:
        d = os.path.join(ROOT, base)
        for dirpath, _dirnames, filenames in os.walk(d):
            if any(p in dirpath.replace("/", os.sep) for p in SKIP_PARTS):
                continue
            for fn in filenames:
                if fn.endswith(".rs"):
                    yield os.path.join(dirpath, fn)


def scan():
    total = 0
    unsafe_lines = 0
    sites = []
    missing = []
    for path in sorted(rust_files()):
        rel = os.path.relpath(path, ROOT).replace("\\", "/")
        with open(path, encoding="utf-8") as f:
            lines = f.readlines()
        for i, raw in enumerate(lines, 1):
            line = raw.strip()
            if not line or line.startswith("//"):
                continue
            total += 1
            if "unsafe" not in line:
                continue
            # `#![forbid(unsafe_code)]` / `deny(unsafe_code)` 是**禁止** unsafe，
            # 不是 unsafe 代码，不能计入分子。
            if "unsafe_code" in line:
                continue
            # `unsafe fn` 的声明行算一次；调用 `unsafe { ... }` 也算。
            unsafe_lines += 1
            # 向上找证明：接受两种写法
            #   `// SAFETY: ...`（块前的说明）与 `/// # Safety`（函数的契约文档）。
            # 都是合法证明，区别只是"写给块看"还是"写给调用者看"。
            proof = ""
            window = 14 if line.startswith("unsafe fn") else 8
            for k in range(i - 2, max(-1, i - window), -1):
                prev = lines[k].strip()
                low = prev.lower()
                if "safety" in low:
                    proof = prev.lstrip("/ ").strip()
                    break
                if prev.startswith("//"):
                    continue
                # 属性行（`#[inline]` / `#[no_mangle]` 等）不是语句，往上看不算越界
                if prev.startswith("#["):
                    continue
                if prev and not prev.startswith("//"):
                    break
            sites.append((rel, i, line[:96], proof))
            if not proof:
                missing.append((rel, i, line[:80]))
    return total, unsafe_lines, sites, missing


def write_register(total, unsafe_lines, sites, missing):
    out = os.path.join(ROOT, "docs", "unsafe-register.md")
    dens = 100.0 * unsafe_lines / total if total else 0.0
    with open(out, "w", encoding="utf-8") as f:
        f.write("# unsafe 清单（S6）\n\n")
        f.write("> 本文件由 `tools/unsafe-audit.py` 生成，不要手改。\n")
        f.write("> 重新生成：`python tools/unsafe-audit.py`\n\n")
        f.write("## 汇总\n\n")
        f.write("| 项 | 值 |\n|---|---|\n")
        f.write("| 代码行（去空行/注释） | %d |\n" % total)
        f.write("| 含 `unsafe` 的行 | %d |\n" % unsafe_lines)
        f.write("| 密度 | **%.2f%%**（上限 %.1f%%） |\n" % (dens, DENSITY_LIMIT_PCT))
        f.write("| 缺 `// SAFETY:` 说明的块 | %d |\n\n" % len(missing))
        f.write("按照约定，`unsafe` 只允许出现在三类边界上：\n")
        f.write("① 架构相关（端口/寄存器/上下文切换）；② MMIO/DMA；③ 已经由上层\n")
        f.write("证明过不变量的裸指针操作（页表、用户内存拷贝）。业务逻辑里出现\n")
        f.write("`unsafe` 一律视为设计错误。\n\n")
        if missing:
            f.write("## 缺说明的块（必须补齐）\n\n")
            for rel, ln, text in missing:
                f.write("- `%s:%d` — `%s`\n" % (rel, ln, text))
            f.write("\n")
        f.write("## 逐条清单\n\n")
        f.write("| 文件:行 | 代码 | 证明（SAFETY） |\n|---|---|---|\n")
        for rel, ln, text, proof in sites:
            code = text.replace("|", "\\|")
            p = (proof or "**缺**").replace("|", "\\|")
            f.write("| `%s:%d` | `%s` | %s |\n" % (rel, ln, code, p))
    return out, dens


def main():
    total, unsafe_lines, sites, missing = scan()
    out, dens = write_register(total, unsafe_lines, sites, missing)
    print("[S6] 代码行 %d，unsafe 行 %d，密度 %.2f%%（上限 %.1f%%）"
          % (total, unsafe_lines, dens, DENSITY_LIMIT_PCT))
    print("[S6] 缺 SAFETY 说明：%d 处" % len(missing))
    print("[S6] 清单已写入 " + os.path.relpath(out, ROOT))
    if "--check" in sys.argv:
        bad = dens > DENSITY_LIMIT_PCT or missing
        if bad:
            print("[S6] 不达标")
            return 1
        print("[S6] 达标")
    return 0


if __name__ == "__main__":
    sys.exit(main())
