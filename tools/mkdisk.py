#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""一条命令产出可挂载的系统盘：汇编用户程序 → 包成 ELF64 → 塞进 ext2 镜像。

为什么要串起来：这三步单独都能手工跑，但它们是**同一件事的三段**——
用户程序改了，ELF 和镜像都得跟着变；漏掉一步就会出现"内核跑的是旧程序"
这种最难查的问题（现象是改了代码没效果）。

产物：
  build/user_disk.bin   平铺机器码
  build/hello.elf       ELF64 可执行文件
  build/disk.img        ext2 镜像（含 /bin/hello、/bin/proc、/hello.txt，
                        以及 M9 的 musl 静态 busybox 与它的 applet 链接）

M9 之后镜像里还要放 busybox（1.1 MiB），所以总块数从 4096 提到 16384
（16 MiB）——文件本身要 1105 个数据块 + 二级间接块，4 MiB 装不下。
"""

import argparse
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

TEXT = (
    "this line came from an ext2 file on disk\n"
    "second line: 0123456789 abcdefghij\n"
)

# 两个用户程序：
#   /bin/hello —— M5 的 IO 演示（write/open/read/close/exit）
#   /bin/proc  —— M6 的进程演示（同一份程序跑两份，各有一份私有数据页）
PROGS = [
    ("user_disk", "bin/hello", ["--flags", "5", "--bss", "0"]),
    ("user_proc", "bin/proc", ["--flags", "5", "--bss", "0",
                               "--data-vaddr", "0x600000", "--data-memsz", "4096"]),
]

# M9：musl 静态 busybox + 四个 applet 链接。
# 用符号链接而不是复制四份：镜像省 3.4 MiB，而且顺带把内核的符号链接解析
# 与 busybox 的 argv[0] 分发一起验证了。
BUSYBOX = os.path.join(HERE, "assets", "busybox")
# M11 的跨系统微基准（build.py 里先构建好）
BENCH = os.path.join(HERE, "..", "build", "bench.elf")
APPLETS = ["sh", "ls", "cat", "echo"]


def find_tool(name):
    p = shutil.which(name)
    if p:
        return p
    for c in (r"D:\mingw64\mingw64\bin", r"C:\Users\Admin\devtools\w64devkit\bin"):
        cand = os.path.join(c, name + ".exe")
        if os.path.exists(cand):
            return cand
    return None


def run(cmd):
    print("[执行] " + " ".join(cmd))
    r = subprocess.run(cmd)
    if r.returncode != 0:
        raise SystemExit("[错误] 命令失败: " + " ".join(cmd))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=os.path.join(ROOT, "build"))
    ap.add_argument("--blocks", type=int, default=16384, help="系统盘块数（1 KiB/块）")
    ap.add_argument("--data-blocks", type=int, default=2048, help="数据盘块数（1 KiB/块）")
    a = ap.parse_args()

    os.makedirs(a.out, exist_ok=True)
    img = os.path.join(a.out, "disk.img")

    gcc_as = find_tool("as")
    objcopy = find_tool("objcopy")
    if not gcc_as or not objcopy:
        print("[错误] 找不到 as / objcopy（需要 MinGW-w64 工具链）")
        return 1

    files = []
    for base, guest, elf_args in PROGS:
        asm = os.path.join(ROOT, "lumikernel", "src", base + ".S")
        obj = os.path.join(a.out, base + ".o")
        flat = os.path.join(a.out, base + ".bin")
        elf = os.path.join(a.out, guest.split("/")[-1] + ".elf")
        run([gcc_as, "--64", "-o", obj, asm])
        run([objcopy, "-O", "binary", "-j", ".text", obj, flat])
        run([sys.executable, os.path.join(HERE, "mkelf.py"),
             "--input", flat, "--output", elf] + elf_args)
        files += ["--file", guest + "=" + elf]

    extra = ["--text", "hello.txt=" + TEXT]
    if os.path.isfile(BENCH):
        extra += ["--file", "bin/bench=" + os.path.abspath(BENCH)]
    else:
        print("[警告] 没有 build/bench.elf，M11 的跨系统对比会缺数据")
    if os.path.isfile(BUSYBOX):
        extra += ["--file", "bin/busybox=" + BUSYBOX]
        for ap_ in APPLETS:
            extra += ["--link", "bin/%s=/bin/busybox" % ap_]
    else:
        print("[警告] 没有 tools/assets/busybox，M9 的 busybox 自检会失败")
        print("       下载：https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox")

    env = os.environ.copy()
    # Git-Bash 会把 `/bin/busybox` 这种参数改写成 Windows 路径，
    # 链接目标就全错了。这条环境变量让它原样传递。
    env["MSYS2_ARG_CONV_EXCL"] = "*"
    r = subprocess.run([sys.executable, os.path.join(HERE, "mkext2.py"),
                        "--output", img, "--blocks", str(a.blocks)] + files + extra,
                       env=env)
    if r.returncode != 0:
        raise SystemExit("[错误] mkext2.py 失败")
    # 数据盘：全零即可。LumiFS 的格式化由内核在首次挂载时做
    # （格式化逻辑只有一份，避免工具与内核各写一遍格式定义而悄悄走样）。
    data = os.path.join(a.out, "disk2.img")
    with open(data, "wb") as f:
        f.write(b"\x00" * (a.data_blocks * 1024))
    print("[数据盘] %s 就绪（%d 块 = %d KiB，未格式化）"
          % (data, a.data_blocks, a.data_blocks))
    print("[磁盘] %s 就绪" % img)
    return 0


if __name__ == "__main__":
    sys.exit(main())
