#!/usr/bin/env python3
"""长跑日志分析（S3）：把 `SOAK` 采样行变成结论。

用法：
    python scripts/soak-report.py build/serial-soak.log
    python scripts/soak-report.py build/serial-soak.log --json
"""

import json
import re
import sys

# 一轮负载里"会碰资源"的操作数（帧 8 对、堆 2 次、文件 2、管道 4、目录 2，
# 以及按频率摊薄的 exec / LumiFS 写）。用来把"轮数"折算成"操作数"。
OPS_PER_ROUND = 18


def slope_milli(xs, ys):
    """最小二乘斜率 ×1000（整数运算，与内核里 `kqa::soak` 的口径一致）。"""
    n = len(xs)
    if n < 2:
        return 0
    sx = sum(xs)
    sy = sum(ys)
    sxy = sum(x * y for x, y in zip(xs, ys))
    sxx = sum(x * x for x in xs)
    den = n * sxx - sx * sx
    if den == 0:
        return 0
    return (n * sxy - sx * sy) * 1000 // den


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "build/serial-soak.log"
    as_json = "--json" in sys.argv
    txt = open(path, encoding="utf-8", errors="replace").read()

    samples = []
    for m in re.finditer(
        r"^SOAK r=(\d+) s=(\d+) frames=(\d+) heap=(\d+) tasks=(\d+) fds=(\d+) t=(\d+)",
        txt, re.M):
        samples.append(dict(
            rounds=int(m.group(1)), seq=int(m.group(2)),
            frames=int(m.group(3)), heap=int(m.group(4)),
            tasks=int(m.group(5)), fds=int(m.group(6)), ticks=int(m.group(7)),
        ))
    regs = [m.group(0) for m in re.finditer(r"^SOAK-REG .*$", txt, re.M)]

    if len(samples) < 3:
        print("[soak] 采样点不足（%d），无法判定" % len(samples))
        return 1

    xs = list(range(len(samples)))
    frames = [s["frames"] for s in samples]
    heap = [s["heap"] for s in samples]
    fds = [s["fds"] for s in samples]
    tasks = [s["tasks"] for s in samples]

    sf = slope_milli(xs, frames)      # 空闲帧：应为 0（负=在漏）
    sh = slope_milli(xs, heap)
    sfd = slope_milli(xs, fds)
    delta_f = frames[-1] - frames[0]
    delta_h = heap[-1] - heap[0]

    secs = samples[-1]["ticks"] / 100.0
    rounds = samples[-1]["rounds"]
    ops = rounds * OPS_PER_ROUND
    per_sec = ops / secs if secs else 0
    days = secs / 86400.0
    equivalent_ops_per_sec = ops / (30 * 86400)   # 若摊到 30 天，相当于每秒多少操作

    flat = abs(sf) <= 1 and abs(sh) <= 1 and abs(sfd) <= 1 and delta_f == 0 and delta_h == 0
    drift_regs = [r for r in regs if "verdict=drift" in r]

    if as_json:
        print(json.dumps(dict(
            samples=len(samples), rounds=rounds, ops=ops, seconds=secs,
            slope_frames=sf, slope_heap=sh, slope_fds=sfd,
            delta_frames=delta_f, delta_heap=delta_h,
            verdict="ok" if flat else "drift",
        ), ensure_ascii=False, indent=2))
        return 0 if flat else 1

    print("=" * 74)
    print("长跑结论（%s）" % path)
    print("-" * 74)
    print("采样点        %d（回归窗口 64）" % len(samples))
    print("轮数          %d" % rounds)
    print("操作数        %d（每轮 %d 次资源操作）" % (ops, OPS_PER_ROUND))
    print("连续运行      %.1f 秒（%.2f 小时 / %.3f 天）" % (secs, secs / 3600, days))
    print("吞吐          %.0f 操作/秒" % per_sec)
    print("-" * 74)
    print("空闲帧        %d → %d（差 %+d），范围 [%d, %d]"
          % (frames[0], frames[-1], delta_f, min(frames), max(frames)))
    print("堆已用        %d → %d（差 %+d）" % (heap[0], heap[-1], delta_h))
    print("任务数        %d → %d" % (tasks[0], tasks[-1]))
    print("打开 fd       %d → %d" % (fds[0], fds[-1]))
    print("-" * 74)
    print("回归斜率      空闲帧 %+d‰ / 堆 %+d‰ / fd %+d‰（每 1000 个采样点）"
          % (sf, sh, sfd))
    print("中途回归行    %d 条，其中 drift %d 条" % (len(regs), len(drift_regs)))
    print("-" * 74)
    print("判定          %s" % ("零漂移（通过）" if flat else "**检测到漂移**"))
    print("30 天等效    本次共 %d 次操作；若摊到 30 天，相当于每秒 %.0f 次操作的后台负载"
          % (ops, equivalent_ops_per_sec))
    print("=" * 74)
    return 0 if flat else 1


if __name__ == "__main__":
    sys.exit(main())
