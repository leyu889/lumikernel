#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""把 LumiKernel 源码推到 GitHub（leyu889/lumikernel）。

为什么要有这个脚本：手工推送要记四件事——重建快照、同步、提交、凭据，
每一样忘了都会得到一条看不懂的报错。这里把四步固定下来。

流程
    1. 重建 dist/lumikernel（从当前源码，保证推的是最新代码）
    2. 同步进缓存克隆 .ghcache（**不在 dist 里**，否则会被 make-repo 一起清掉）
    3. 有变更才提交；没变更直接说"没有新变更"，退出码 0
    4. 推送

缓存克隆只放我们自己的提交，所以远端有新提交时 `pull --ff-only` 是安全的。
真的出现分叉（分支被别处改过），脚本会停下并原样保留现场，不会强推。

用法
    python tools/push_github.py                  # 常规：重建 + 推送
    python tools/push_github.py --no-rebuild     # 跳过重建，推 dist 里现有的
    python tools/push_github.py -m "fix: ..."    # 自定义提交说明
    python tools/push_github.py --dry-run        # 只同步和报告，不提交不推送
"""

import argparse
import datetime
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

REPO_URL = "https://github.com/leyu889/lumikernel.git"
BRANCH = "main"
SNAPSHOT = os.path.join(ROOT, "dist", "lumikernel")
CACHE = os.path.join(ROOT, ".ghcache")

# 出网走本地代理并做 TLS 中间人，系统默认信任链里没有它的 CA。
# git 的 openssl 后端必须显式指到这个包，否则报 "unable to get local issuer
# certificate"；schannel 后端则在代理隧道下报吊销检查失败。
CA_BUNDLE = os.path.join(
    os.path.expanduser("~"), ".workbuddy", "system-ca-bundle.pem"
)


def log(msg):
    print("[push] %s" % msg, flush=True)


def git(args, cwd=CACHE, check=True, capture=False):
    """调用 git，自动带上本机需要的 TLS 与凭据配置。"""
    cmd = ["git"]
    if os.path.isfile(CA_BUNDLE):
        cmd += ["-c", "http.sslBackend=openssl",
                "-c", "http.sslCAInfo=%s" % CA_BUNDLE]
    # 凭据：Windows 凭据管理器里已有 git:https://github.com 的条目
    cmd += ["-c", "credential.helper=wincred"]
    # 中文路径不转义：默认 `ls-files` 会把非 ASCII 名字打成 "\350\247..."
    # 的八进制串，拿它去拼真实路径必然找不到文件，删旧文件的逻辑就失效了。
    cmd += ["-c", "core.quotepath=false"]
    cmd += args
    r = subprocess.run(
        cmd, cwd=cwd,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.STDOUT if capture else None,
        text=True,
    )
    if check and r.returncode != 0:
        if capture and r.stdout:
            print(r.stdout.strip()[-1500:], file=sys.stderr)
        raise SystemExit("[push] git %s 失败（退出码 %d）" % (" ".join(args), r.returncode))
    return r


def rebuild_snapshot():
    log("重建源码快照 dist/lumikernel …")
    r = subprocess.run(
        [sys.executable, os.path.join(HERE, "make-repo.py"), "--zip"],
        cwd=ROOT, text=True,
    )
    if r.returncode != 0:
        raise SystemExit("[push] make-repo.py 失败，先修构建再推送")
    if not os.path.isdir(SNAPSHOT):
        raise SystemExit("[push] 没生成 %s" % SNAPSHOT)


def ensure_cache():
    """缓存克隆不存在就克隆一个；存在就先快进到远端最新。"""
    if not os.path.isdir(os.path.join(CACHE, ".git")):
        log("首次使用，克隆远端到 .ghcache …")
        os.makedirs(os.path.dirname(CACHE) or ".", exist_ok=True)
        git(["clone", "--branch", BRANCH, REPO_URL, CACHE], cwd=ROOT)
        return
    log("同步远端引用 …")
    git(["fetch", "--prune", "origin"])
    r = git(["pull", "--ff-only", "origin", BRANCH], check=False, capture=True)
    if r.returncode != 0:
        print(r.stdout.strip()[-1200:], file=sys.stderr)
        raise SystemExit(
            "[push] 缓存克隆无法快进——分支在别处被改过。\n"
            "        现场在 %s，确认后手工处理（脚本不会强推）。" % CACHE
        )


def sync_files():
    """把快照铺进缓存克隆。源里没有的文件，缓存里也删掉。

    排除 .git：快照目录里那个 .git 是 make-repo 的产物，不是我们要推的历史。
    """
    src_files = set()
    for root, dirs, files in os.walk(SNAPSHOT):
        dirs[:] = [d for d in dirs if d != ".git"]
        rel_dir = os.path.relpath(root, SNAPSHOT)
        for f in files:
            rel = f if rel_dir == "." else os.path.join(rel_dir, f)
            src_files.add(rel.replace("\\", "/"))

    # 缓存里被跟踪、但快照里已经没有的文件 → 删除（重命名/删文件时不会残留）
    tracked = [l for l in git(["ls-files"], capture=True).stdout.splitlines() if l.strip()]
    removed = 0
    for rel in tracked:
        if rel not in src_files:
            p = os.path.join(CACHE, rel)
            if os.path.isfile(p):
                os.remove(p)
                removed += 1

    import shutil
    copied = 0
    for rel in sorted(src_files):
        s = os.path.join(SNAPSHOT, rel.replace("/", os.sep))
        d = os.path.join(CACHE, rel.replace("/", os.sep))
        os.makedirs(os.path.dirname(d) or CACHE, exist_ok=True)
        shutil.copy2(s, d)
        copied += 1

    log("同步完成：写入 %d 个文件，删除 %d 个" % (copied, removed))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--no-rebuild", action="store_true", help="跳过重建快照")
    ap.add_argument("-m", "--message", default="", help="提交说明")
    ap.add_argument("--dry-run", action="store_true", help="只同步，不提交不推送")
    a = ap.parse_args()

    if not a.no_rebuild:
        rebuild_snapshot()
    elif not os.path.isdir(SNAPSHOT):
        raise SystemExit("[push] 没有 %s，去掉 --no-rebuild 先重建一次" % SNAPSHOT)

    ensure_cache()
    sync_files()

    git(["add", "-A"])
    changed = git(["status", "--porcelain"], capture=True).stdout.strip()
    if not changed:
        log("没有新变更，远端已是最新（%s）" % BRANCH)
        return 0

    n = len([l for l in changed.splitlines() if l.strip()])
    log("检测到 %d 处变更" % n)

    if a.dry_run:
        print(changed[:3000])
        log("--dry-run：到此为止，未提交未推送")
        return 0

    msg = a.message.strip()
    if not msg:
        stamp = datetime.datetime.now().strftime("%Y-%m-%d %H:%M")
        msg = "chore: sync source snapshot (%s)" % stamp
    git(["commit", "-q", "-m", msg])
    log("已提交：%s" % msg.splitlines()[0])

    log("推送到 %s …" % REPO_URL)
    r = git(["push", "origin", BRANCH], check=False, capture=True)
    if r.returncode != 0:
        print(r.stdout.strip()[-1500:], file=sys.stderr)
        raise SystemExit("[push] 推送被拒。若是非快进，说明远端有别人的提交——"
                         "先在 .ghcache 里合并，脚本不做强推。")
    head = git(["rev-parse", "--short", "HEAD"], capture=True).stdout.strip()
    log("完成，远端 %s = %s" % (BRANCH, head))
    log("查看：https://github.com/leyu889/lumikernel")
    return 0


if __name__ == "__main__":
    sys.exit(main())
