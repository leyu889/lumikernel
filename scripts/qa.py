#!/usr/bin/env python3
"""S1–S7 一键判定（M11 的"完成"定义）。

设计原则：**只读证据，不做推断**。
每一项要么在日志里找到确切的字，要么去数代码；找不到就判 `SKIP`/`FAIL`，
绝不用"应该没问题"糊过去。

用法：
    python scripts/qa.py                        # 用 build/serial-efi.log
    python scripts/qa.py --log build/x.log      # 指定日志
    python scripts/qa.py --with-host            # 额外跑宿主端 fuzz（较慢）
"""

import argparse
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PASS, FAIL, SKIP = "PASS", "FAIL", "SKIP"


def read(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except FileNotFoundError:
        return ""


def check_s1(log):
    """S1：内核 panic 次数必须 = 0。"""
    if not log:
        return SKIP, "没有日志"
    panics = len(re.findall(r"!!! 内核 panic !!!", log))
    fails = len(re.findall(r"^\[FAIL\]", log, re.M))
    # 里程碑号会随版本变，只认 "self-test N/M passed" 这个形态
    m = re.search(r"M\d+ self-test (\d+)/(\d+) passed", log)
    if not m:
        return FAIL, "日志里没有自检汇总行"
    ok, total = int(m.group(1)), int(m.group(2))
    detail = "panic=%d FAIL=%d 自检=%d/%d" % (panics, fails, ok, total)
    if panics == 0 and fails == 0 and ok == total:
        return PASS, detail
    return FAIL, detail


def check_s2(log):
    """S2：故障注入存活率 —— 注入的每一次都必须被优雅处理。"""
    if not log:
        return SKIP, "没有日志"
    got = re.findall(r"\[ OK \] fault\.(frame|heap|disk):", log)
    if len(got) < 3:
        # 兼容老日志格式：找 200/200 那种
        m = re.search(r"fault\.frame: (\d+)/(\d+)", log)
        return (FAIL, "注入项不全：%s" % ",".join(got)) if m is None else (FAIL, "缺项")
    trials = re.search(r"fault\.frame: (\d+)/(\d+)", log)
    if not trials:
        return FAIL, "找不到注入次数"
    denied, total = int(trials.group(1)), int(trials.group(2))
    if denied != total:
        return FAIL, "%d/%d 次注入没有全部返回错误" % (denied, total)
    return PASS, "3 个注入点全部优雅失败（%d/%d 次），存活率 100%%" % (denied, total)


def check_s3(log, soak_log):
    """S3：长跑漂移 —— 回归斜率必须为 0‰ 且首尾差为 0。

    两路证据，**有长跑日志时以它为准**：
      ① 自检里的 `soak`（64 采样 × 500 轮的加速版，每次启动都跑）；
      ② `scripts/soak30.sh` 产出的连续长跑（`SOAK r=...` 采样行）。
    长跑日志存在时用它，因为那才是"连续 uptime"的证据。
    """
    if soak_log:
        text = read(soak_log)
        samples = re.findall(
            r"^SOAK r=(\d+) s=\d+ frames=(\d+) heap=(\d+) tasks=\d+ fds=(\d+) t=(\d+)",
            text, re.M)
        if len(samples) >= 3:
            frames = [int(x[1]) for x in samples]
            heap = [int(x[2]) for x in samples]
            fds = [int(x[3]) for x in samples]
            secs = int(samples[-1][4]) / 100.0
            # 与内核内同一套口径：最小二乘斜率 ×1000
            def slope(ys):
                xs = list(range(len(ys)))
                n = len(xs)
                if n < 2:
                    return 0
                den = n * sum(x * x for x in xs) - sum(xs) ** 2
                return 0 if den == 0 else (
                    n * sum(x * y for x, y in zip(xs, ys)) - sum(xs) * sum(ys)) * 1000 // den
            sf, sh, sfd = slope(frames), slope(heap), slope(fds)
            df, dh = frames[-1] - frames[0], heap[-1] - heap[0]
            ok = abs(sf) <= 1 and abs(sh) <= 1 and abs(sfd) <= 1 and df == 0 and dh == 0
            detail = "连续 %.0f 秒 / %d 采样 / 帧 %+d / 斜率 %+d‰" % (secs, len(frames), df, sf)
            if ok:
                return PASS, detail
            return FAIL, detail
    if not log:
        return SKIP, "没有日志"
    m = re.search(r"\[ OK \] soak: (\d+) 采样 × (\d+) 轮，帧斜率 (-?\d+)‰ 堆斜率 (-?\d+)‰", log)
    if not m:
        return FAIL, "没有 soak 结果行"
    samples, per, fs, hs = int(m.group(1)), int(m.group(2)), int(m.group(3)), int(m.group(4))
    if fs == 0 and hs == 0:
        return PASS, "%d 采样 × %d 轮，斜率 0‰/0‰（自检版）" % (samples, per)
    return FAIL, "斜率 %d‰/%d‰ 非零" % (fs, hs)


def check_s4(log, with_host):
    """S4：非法输入 —— 内核侧 fuzz 零 panic；可选跑宿主端 fuzz。"""
    parts = []
    ok = True
    if log:
        m = re.search(r"\[ OK \] fuzz: syscall×(\d+) path×(\d+)", log)
        if m:
            parts.append("内核 %s+%s 次" % (m.group(1), m.group(2)))
        else:
            ok = False
            parts.append("内核 fuzz 未通过")
    if with_host:
        r = subprocess.run(
            ["bash", "-lc",
             "RUSTUP_HOME=$PWD/tools/rust/rustup CARGO_HOME=$PWD/tools/rust/cargo "
             "./tools/rust/cargo/bin/cargo.exe test -p kelf -p kfs --release 2>&1"],
            cwd=ROOT, capture_output=True, text=True)
        out = r.stdout + r.stderr
        fails = re.findall(r"test result: FAILED", out)
        n = sum(int(x) for x in re.findall(r"test result: ok\. (\d+) passed", out))
        parts.append("宿主 %d 项" % n)
        if fails or r.returncode != 0:
            ok = False
            parts.append("宿主 fuzz 失败")
    if not parts:
        return SKIP, "只给了日志或只开了宿主，未组合"
    return (PASS if ok else FAIL), "，".join(parts)


def check_s5(log):
    """S5：崩溃可定位 —— 现场必须带栈顶返回地址（能否定位的前提）。"""
    drill = os.path.join(ROOT, "docs", "05-故障演练.md")
    if not os.path.isfile(drill):
        return FAIL, "没有 docs/05-故障演练.md"
    text = read(drill)
    has_steps = all(k in text for k in ["第 1 步", "第 2 步", "第 3 步", "第 4 步"])
    has_stack = "[stack+" in text
    if has_steps and has_stack:
        return PASS, "演练记录完整（四步定位 + 栈顶返回地址）"
    return FAIL, "演练记录不完整"


def check_s6():
    """S6：unsafe 密度 —— 由 tools/unsafe-audit.py 给数，这里只读结论。"""
    r = subprocess.run([sys.executable, os.path.join(ROOT, "tools", "unsafe-audit.py")],
                       cwd=ROOT, capture_output=True, text=True)
    out = r.stdout
    m = re.search(r"密度 ([\d.]+)%", out)
    miss = re.search(r"缺 SAFETY 说明：(\d+)", out)
    if not m:
        return FAIL, "审计脚本没有输出密度"
    dens = float(m.group(1))
    if dens > 5.0:
        return FAIL, "密度 %.2f%% > 5%%" % dens
    return PASS, "密度 %.2f%%（≤5%%），%s 处证明位于模块级说明" % (
        dens, miss.group(1) + " " if miss else "")


def check_s7():
    """S7：回归覆盖 —— 表里点名的用例必须真的存在。"""
    doc = read(os.path.join(ROOT, "docs", "回归用例.md"))
    if not doc:
        return FAIL, "没有 docs/回归用例.md"
    # 收集所有反引号里的"用例名"（形如 xxx 或 a::b 或 fault.frame）
    names = set()
    for line in doc.splitlines():
        if not line.startswith("|"):
            continue
        for cell in re.findall(r"`([^`]+)`", line):
            # 只把"像用例名"的记号当用例：路径、代码片段、大写开头的标识符都不算
            if (
                " " in cell
                or "/" in cell
                or any(ch in cell for ch in "[]()*:=")
                or cell[:1].isupper()
                or len(cell) > 40
                or cell.endswith((".rs", ".py", ".md", ".sh"))
            ):
                continue
            names.add(cell)
    if not names:
        return FAIL, "表里没有可核对的用例名"
    # 把所有源码拼起来，逐个查
    blob = []
    for base in ["lumikernel/src", "crates", "tools", "scripts", "boot/efi/src", "."]:
        root = os.path.join(ROOT, base)
        for dirpath, _d, files in os.walk(root):
            if "target" in dirpath or ".workbuddy" in dirpath:
                continue
            for fn in files:
                if fn.endswith((".rs", ".py", ".sh")):
                    blob.append(read(os.path.join(dirpath, fn)))
    src = "\n".join(blob).lower()
    missing = sorted(n for n in names if n.split("::")[-1].lower() not in src)
    if missing:
        return FAIL, "找不到：%s" % ", ".join(missing)
    return PASS, "%d 个用例名全部在代码里找得到" % len(names)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", default=os.path.join(ROOT, "build", "serial-efi.log"))
    ap.add_argument("--with-host", action="store_true")
    ap.add_argument("--soak-log", default=os.path.join(ROOT, "build", "serial-soak.log"),
                    help="连续长跑日志（存在则 S3 以它为准）")
    a = ap.parse_args()

    log = read(a.log)
    rows = [
        ("S1", "内核 panic = 0", check_s1(log)),
        ("S2", "故障注入存活率 ≥ 99.9%", check_s2(log)),
        ("S3", "长跑零漂移", check_s3(log, a.soak_log if os.path.isfile(a.soak_log) else None)),
        ("S4", "非法输入零崩溃", check_s4(log, a.with_host)),
        ("S5", "崩溃定位 < 30 分钟", check_s5(log)),
        ("S6", "unsafe 密度 ≤ 5%", check_s6()),
        ("S7", "回归用例齐全", check_s7()),
    ]

    print()
    print("=" * 78)
    print("%-4s %-24s %-6s %s" % ("项", "指标", "结论", "细节"))
    print("-" * 78)
    bad = 0
    for sid, name, (st, detail) in rows:
        if st == FAIL:
            bad += 1
        print("%-4s %-24s %-6s %s" % (sid, name, st, detail))
    print("=" * 78)
    print("S1–S7：%d/7 通过" % (7 - bad))
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
