#!/usr/bin/env python3
"""统计 LumiKernel 的代码行数，按 Linux 的目录口径对齐。

为什么要这个脚本：`docs/08-规模对标.md` 里的每个数字都应该能被复算。
不接受"我觉得大概三万行"这种说法。

三个口径，三个用途：
  raw    含空行与注释 —— 用来和 Linux 的 `wc -l` 口径对话
  code   去掉空行与注释 —— 用来和 sloccount / cloc 对话（对标 Linux 时用这个）
  doc    注释行数 —— 用来解释"为什么我们的行数里有一部分是说明而不是逻辑"
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# 按 Linux 的目录名对齐。顺序无关紧要，输出会再排。
GROUPS = [
    # (显示名, [相对路径], Linux 对应的目录)
    ("调度 + 中断 + 时间", ["lumikernel/src/task", "lumikernel/src/intr",
                            "crates/ksched"], "kernel/"),
    ("内存管理", ["lumikernel/src/mm", "crates/kmem", "crates/kpt",
                  "crates/kheap", "crates/kslab"], "mm/"),
    ("文件系统", ["lumikernel/src/fs", "crates/kfs", "crates/lumifs",
                  "crates/kbcache"], "fs/"),
    ("驱动", ["lumikernel/src/dev"], "drivers/"),
    ("架构", ["lumikernel/src/arch", "crates/kacpi"], "arch/"),
    ("Linux ABI", ["lumikernel/src/linux", "crates/klinux"], "（Linux 没有对应物）"),
    ("IPC / fd / VFS", ["lumikernel/src/pipe.rs", "lumikernel/src/fd.rs",
                        "lumikernel/src/vfs.rs"], "ipc/ + fs/"),
    ("ELF / 装载", ["crates/kelf", "lumikernel/src/exec.rs"], "fs/binfmt_elf.c"),
    ("虚拟机 / sandbox", ["crates/kvm"], "virt/"),
    ("质量工程", ["crates/kqa", "crates/kfault", "lumikernel/src/qa.rs"], "（Linux 无对应物）"),
]


def count(path):
    """返回 (raw, code, doc) 三行数。"""
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            lines = f.read().splitlines()
    except OSError:
        return 0, 0, 0

    raw = len(lines)
    code = 0
    doc = 0
    in_block = False
    for ln in lines:
        s = ln.strip()
        if not s:
            continue
        if in_block:
            doc += 1
            if "*/" in s:
                in_block = False
            continue
        if s.startswith("/*"):
            doc += 1
            if "*/" not in s:
                in_block = True
            continue
        if s.startswith("//") or s.startswith("#!") or s.startswith("# "):
            doc += 1
            continue
        # 行尾注释不算进来：保守估计，宁可高估代码量也不虚报注释比例
        code += 1
    return raw, code, doc


def walk(p):
    """给定文件或目录，返回所有源文件。"""
    full = os.path.join(ROOT, p)
    if os.path.isfile(full):
        return [full]
    out = []
    for dirpath, dirnames, files in os.walk(full):
        dirnames[:] = [d for d in dirnames if d not in ("target", "node_modules")]
        for fn in files:
            if fn.endswith((".rs", ".S", ".s", ".asm", ".py", ".sh", ".ld")):
                out.append(os.path.join(dirpath, fn))
    return out


def main():
    only_code = "--code" in sys.argv

    print("LumiKernel 代码量（按 Linux 目录口径对齐）")
    print("=" * 78)
    fmt = "%-22s %8s %8s %8s  %s"
    print(fmt % ("分组", "raw", "code", "doc", "Linux 对应"))
    print("-" * 78)

    total = [0, 0, 0]
    seen = set()
    for name, paths, linux in GROUPS:
        r = c = d = 0
        for p in paths:
            for f in walk(p):
                if f in seen:
                    continue
                seen.add(f)
                a, b, e = count(f)
                r += a
                c += b
                d += e
        total[0] += r
        total[1] += c
        total[2] += d
        print(fmt % (name, r, c, d, linux))

    # 归不进上面任何一组的顶层文件（main.rs / console.rs 等）
    rest = [0, 0, 0]
    for f in walk("lumikernel/src"):
        if f in seen:
            continue
        if os.path.basename(f).startswith("main.") or True:
            a, b, e = count(f)
            rest[0] += a
            rest[1] += b
            rest[2] += e
    print(fmt % ("内核主体其他", rest[0], rest[1], rest[2], "init/ + lib/"))
    total = [x + y for x, y in zip(total, rest)]

    print("-" * 78)
    print(fmt % ("内核合计", total[0], total[1], total[2], ""))

    # 内核之外的东西：单列，因为它们不算"内核代码量"
    outside = [0, 0, 0]
    for p in ["bench/src", "tools", "scripts", "boot"]:
        for f in walk(p):
            if f.endswith(".py") or f.endswith(".rs") or f.endswith(".sh") or f.endswith(".S"):
                a, b, e = count(f)
                outside[0] += a
                outside[1] += b
                outside[2] += e
    print(fmt % ("工具 / 基准 / 引导", outside[0], outside[1], outside[2], "（不计入内核）"))

    print("=" * 78)
    grand = [x + y for x, y in zip(total, outside)]
    print(fmt % ("全部合计", grand[0], grand[1], grand[2], ""))

    linux = 26_407_571
    print()
    if only_code:
        print(f"与 Linux 6.12 对比：{grand[1]:,} / {linux:,} = 1 : {linux / grand[1]:.0f}")
    else:
        print(f"与 Linux 6.12 对比（code 口径）：{grand[1]:,} / {linux:,} = 1 : {linux / grand[1]:.0f}")

    # 每个 crates 的分布，方便看"最大的模块有多大规模"
    print("\n按 crate 分布（code 行）")
    print("-" * 40)
    rows = []
    cdir = os.path.join(ROOT, "crates")
    if os.path.isdir(cdir):
        for name in sorted(os.listdir(cdir)):
            src = os.path.join(cdir, name, "src")
            if not os.path.isdir(src):
                continue
            c = 0
            for f in walk(os.path.join("crates", name, "src")):
                c += count(f)[1]
            rows.append((c, name))
    for c, name in sorted(rows, reverse=True):
        print(f"  {name:<16} {c:>6}")


if __name__ == "__main__":
    main()
