#!/usr/bin/env python3
"""打一个"能直接推到 GitHub"的源码目录。

用法：
    python tools/make-repo.py                    # 输出到 dist/lumikernel/
    python tools/make-repo.py --dst D:/tmp/lk    # 指定目录
    python tools/make-repo.py --zip              # 顺手压成 zip

做三件事：
1. 按白名单拷贝源码（**不带** 编译产物、自带工具链、1 MB 的 busybox 二进制）；
2. 生成仓库必需的元文件（LICENSE / .gitignore / CI / CONTRIBUTING）；
3. 打印清单与体积，方便一眼看出"漏了什么"或"多了什么"。

为什么手工列白名单而不是 `cp -r` 再删：漏删一个 `out/*.img` 就是几十 MB，
而漏拷一个源文件会让仓库编译不过。白名单至少保证"多了什么"是显式的。
"""

import argparse
import os
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

INCLUDE_FILES = [
    # `.cargo/config.toml` 是**必需的**：裸机目标要靠它拿到
    # `-Tlinker.ld` / `--oformat=binary` / `code-model=kernel`，
    # 漏了它内核在别人机器上直接链接失败（踩过一次）。
    os.path.join(".cargo", "config.toml"),
    "build.py",
    "Cargo.toml",
    "Cargo.lock",
    "linker.ld",
    "README.md",
    "README.en.md",
    # Windows 上双击就能用的包装脚本（内部路径都是相对的）
    "build.cmd",
    "run.cmd",
    "test.cmd",
    # 一键推到 GitHub：重建快照 → 同步 → 有变更才提交 → 推送
    "PUSH_GITHUB.cmd",
]
INCLUDE_DIRS = [
    "lumikernel",
    "crates",
    "boot",
    "bench",
    "tools",
    "scripts",
    "docs",
]

# 目录/文件名黑名单（相对仓库根）
EXCLUDE_PARTS = {
    "target",
    "build",
    "out",
    ".git",
    ".workbuddy",
    "__pycache__",
    "node_modules",
}
EXCLUDE_SUFFIX = (".pyc", ".img", ".iso", ".vdi", ".log", ".o", ".rlib", ".efi")

# 大二进制 / 第三方资产：仓库里不放，提供下载说明
EXCLUDE_EXACT = {
    os.path.join("tools", "assets", "busybox"),
    os.path.join("tools", "assets", "busybox136"),
    # 第三方安装器（15 MB）：仓库里不放，文档里给 rustup.rs 的链接
    os.path.join("tools", "rustup-init.exe"),
}
# 自带工具链（1.2 GB）绝不上传
EXCLUDE_EXACT_DIRS = {os.path.join("tools", "rust")}

LICENSE_TEXT = """MIT License

Copyright (c) 2026 LumiOS / LumiKernel contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"""

GITIGNORE = """# 编译产物
target/
build/
out/
*.o
*.rlib
*.pyc
__pycache__/

# 虚拟机产物
*.img
*.iso
*.vdi
*.vbox
*.log

# 自带工具链（1.2 GB，自己装 rustup 即可）
tools/rust/

# 第三方二进制资产（见 tools/assets/README.txt 自行下载）
tools/assets/busybox
tools/assets/busybox136

# 编辑器
.vscode/
.idea/
*.swp
.DS_Store
"""

CI = """name: ci

on:
  push:
    branches: [main, master]
  pull_request:

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: 安装 Rust 与两个裸机目标
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: x86_64-unknown-none, x86_64-unknown-uefi

      # 所有"纯逻辑"的 crate 都能在宿主机上测（这是本项目的核心设计：
      # 判定逻辑与硬件分离，所以 CI 不需要虚拟机就能跑几百个单测）
      - name: 单元测试（宿主机）
        run: cargo test --workspace --exclude lumikernel

      - name: 编译内核（裸机目标）
        run: cargo build -p lumikernel --target x86_64-unknown-none --release

      - name: 编译 UEFI 装载器
        run: cargo build --release --target x86_64-unknown-uefi
        working-directory: boot/efi

      # S6：unsafe 密度审计（超过阈值或缺少证明会返回非 0）
      - name: unsafe 审计
        run: python3 tools/unsafe-audit.py
"""

CONTRIBUTING = """# 参与开发

这个项目的规矩很少，但每一条都是踩出来的（详见 `docs/04-工程规约.md`，目前 72 条）。

## 前置要求

- Rust（stable）。需要两个裸机目标：

```bash
rustup target add x86_64-unknown-none x86_64-unknown-uefi
```

- Python 3.8+（构建脚本 `build.py` / `tools/*.py` 全部是标准库，无第三方依赖）
- 想在虚拟机里跑自检的话再装 VirtualBox 7.x（脚本用 `VBoxManage`）

> 本仓库不携带 Rust 工具链（1.2 GB）。如果你的机器完全离线，
> 把一份 rustup 安装目录放到 `tools/rust/`（结构为 `tools/rust/cargo/bin/cargo`），
> `build.py` 会自动优先使用它。

## 提交前必须过的三关

```bash
cargo test --workspace --exclude lumikernel   # 单测全绿
python tools/unsafe-audit.py --check          # unsafe 密度 ≤ 5% 且逐条有证明
python build.py                               # 能出 ISO（不需要虚拟机）
```

涉及内核行为改动的，还要跑一次：

```bash
bash scripts/vmtest-efi.sh out/LumiKernel-0.1-efi.img 80 check 1
# 期望串口里出现 `M11 self-test 58/58 passed`，且没有 [FAIL] / panic
```

## 加功能的顺序

1. **先写用例**：纯逻辑放 crate 单测（宿主机可跑），涉及硬件放内核自检块。
2. **再写实现**：`unsafe` 只允许出现在三类边界（arch / MMIO-DMA / 已被证明不变量的裸指针），
   每处必须有 `// SAFETY:` 说明。
3. **最后写文档**：新踩的坑进 `docs/04-工程规约.md`，修掉的 bug 进 `docs/回归用例.md`。

## 不需要的东西

不需要你先有"完整的内核知识"——这个仓库里每一个文件开头都写了"为什么这样做"，
以及"不这样做会怎样"。按 `docs/02-路线图.md` 挑一个里程碑往下做就行。
"""


def should_skip(rel):
    parts = rel.replace("\\", "/").split("/")
    for p in parts:
        if p in EXCLUDE_PARTS:
            return True
    if rel in EXCLUDE_EXACT:
        return True
    for d in EXCLUDE_EXACT_DIRS:
        if rel == d or rel.startswith(d + os.sep):
            return True
    if rel.endswith(EXCLUDE_SUFFIX):
        return True
    return False


def copy_tree(dst):
    copied = 0
    total = 0
    for f in INCLUDE_FILES:
        src = os.path.join(ROOT, f)
        if not os.path.isfile(src):
            print("[跳过] 源文件不存在：%s" % f)
            continue
        os.makedirs(os.path.dirname(os.path.join(dst, f)) or dst, exist_ok=True)
        shutil.copy2(src, os.path.join(dst, f))
        copied += 1
        total += os.path.getsize(src)

    for d in INCLUDE_DIRS:
        base = os.path.join(ROOT, d)
        if not os.path.isdir(base):
            continue
        for dirpath, dirnames, filenames in os.walk(base):
            dirnames[:] = [x for x in dirnames if x not in EXCLUDE_PARTS]
            for fn in filenames:
                src = os.path.join(dirpath, fn)
                rel = os.path.relpath(src, ROOT)
                if should_skip(rel):
                    continue
                out = os.path.join(dst, rel)
                os.makedirs(os.path.dirname(out), exist_ok=True)
                shutil.copy2(src, out)
                copied += 1
                total += os.path.getsize(src)
    return copied, total


def write_meta(dst):
    meta = {
        "LICENSE": LICENSE_TEXT,
        ".gitignore": GITIGNORE,
        "CONTRIBUTING.md": CONTRIBUTING,
        os.path.join(".github", "workflows", "ci.yml"): CI,
    }
    for rel, text in meta.items():
        p = os.path.join(dst, rel)
        os.makedirs(os.path.dirname(p) or dst, exist_ok=True)
        with open(p, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
    return len(meta)


def rm_tree(path):
    """自己递归删，不用 shutil.rmtree —— 某些沙箱环境会把 rmtree 拦成
    "移到回收站"，在 dist/ 这种临时目录上反而会失败。"""
    if not os.path.isdir(path):
        return
    for ent in os.scandir(path):
        if ent.is_dir(follow_symlinks=False):
            rm_tree(ent.path)
        else:
            os.remove(ent.path)
    # 删空目录可能被沙箱拦（会被当成"移到回收站"），失败也没关系：
    # 我们要的是"目录里没东西"，空目录会被后续拷贝复用。
    try:
        os.rmdir(path)
    except OSError:
        pass


def rm_tree(path):
    """自己递归删，不用 shutil.rmtree —— 某些沙箱环境会把 rmtree 拦成
    "移到回收站"，在 dist/ 这种临时目录上反而会失败。"""
    if not os.path.isdir(path):
        return
    for ent in os.scandir(path):
        if ent.is_dir(follow_symlinks=False):
            rm_tree(ent.path)
        else:
            os.remove(ent.path)
    # 删空目录可能被沙箱拦（会被当成"移到回收站"），失败也没关系：
    # 我们要的是"目录里没东西"，空目录会被后续拷贝复用。
    try:
        os.rmdir(path)
    except OSError:
        pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dst", default=os.path.join(ROOT, "dist", "lumikernel"))
    ap.add_argument("--zip", action="store_true")
    a = ap.parse_args()

    dst = os.path.abspath(a.dst)
    if os.path.isdir(dst):
        rm_tree(dst)
    os.makedirs(dst, exist_ok=True)

    n, total = copy_tree(dst)
    m = write_meta(dst)
    print("[repo] 拷贝 %d 个文件，%.1f KiB → %s" % (n, total / 1024, dst))
    print("[repo] 生成 %d 个元文件（LICENSE / .gitignore / CI / CONTRIBUTING）" % m)

    # 顶层清点，防止漏拷目录
    print("[repo] 顶层内容：")
    for name in sorted(os.listdir(dst)):
        p = os.path.join(dst, name)
        if os.path.isdir(p):
            cnt = sum(len(f) for _, _, f in os.walk(p))
            print("        %-16s %4d 个文件" % (name + "/", cnt))
        else:
            print("        %-16s %6d B" % (name, os.path.getsize(p)))

    if a.zip:
        z = dst + ".zip"
        if os.path.isfile(z):
            os.remove(z)
        shutil.make_archive(dst, "zip", os.path.dirname(dst), os.path.basename(dst))
        print("[repo] 打包 " + z)
    return 0


if __name__ == "__main__":
    sys.exit(main())
