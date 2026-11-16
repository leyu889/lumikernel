#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成一个最小可用的 ext2 文件系统镜像（1 KiB 块、单块组）。

为什么自己写：这台机器上没有 mke2fs（Windows），而内核的 ext2 解析器
（crates/kfs）已经在宿主机上跑过 24 个单测——缺的只是一个"真的能挂"的镜像。
按规范拼字节比去装一个 Linux 环境快得多，而且过程完全可控。

M9 起支持：
- **间接块**（一级/二级），所以能放得下 1 MiB 级的 musl 静态 busybox；
- **符号链接**（fast symlink，目标 < 60 字节时直接写在 i_block 里）。
  `/bin/ls -> /bin/busybox` 这类链接让 busybox 能以 applet 方式被调用，
  不需要复制四份 1.1 MiB 的程序。

限制（写清楚免得以后误用）：
- 只支持单块组、1 KiB 块；
- 目录只用单块（一个目录最多约 100 个条目）；
- 不写 journal、不写 xattr、不做块组对齐优化；
- 符号链接目标上限 59 字节（更长的慢链接会占数据块，本工具不生成）。

生成后会自己重新解析一遍（--check 默认开），用独立代码验证布局没写歪。
"""

import argparse
import os
import struct
import sys

BS = 1024
INODE_SIZE = 128
INODES_PER_GROUP = 256
FIRST_INO = 11
MODE_DIR = 0o040755
MODE_FILE = 0o100644
MODE_LNK = 0o120777
MAX_DIRECT = 12
PPB = BS // 4  # 每个间接块能放多少个块指针


def put16(b, off, v):
    struct.pack_into("<H", b, off, v & 0xFFFF)


def put32(b, off, v):
    struct.pack_into("<I", b, off, v & 0xFFFFFFFF)


class Link:
    """符号链接。`target` 是字节串。"""

    def __init__(self, target):
        self.target = target if isinstance(target, bytes) else target.encode()


class Image:
    def __init__(self, total_blocks):
        self.img = bytearray(total_blocks * BS)
        self.total = total_blocks
        self.inode_blocks = INODES_PER_GROUP * INODE_SIZE // BS
        self.inode_table = 5
        self.first_data = 5 + self.inode_blocks
        self.next_block = self.first_data
        self.next_ino = 2
        self.used_blocks = set(range(0, self.first_data))
        self.used_inos = set()

    def alloc_block(self):
        if self.next_block >= self.total:
            raise RuntimeError("镜像空间不足（需要 %d 块以上）" % (self.next_block + 1))
        b = self.next_block
        self.next_block += 1
        self.used_blocks.add(b)
        return b

    def alloc_inode(self):
        if self.next_ino > INODES_PER_GROUP:
            raise RuntimeError("inode 用尽")
        i = self.next_ino
        self.next_ino += 1
        self.used_inos.add(i)
        return i

    def inode_off(self, ino):
        return self.inode_table * BS + (ino - 1) * INODE_SIZE

    def write_inode(self, ino, mode, size, slots, links, nblk=None):
        """`slots` 是 15 个块指针（12 直接 + 3 间接），按 ext2 规范填。"""
        off = self.inode_off(ino)
        put16(self.img, off + 0, mode)
        put32(self.img, off + 4, size)
        put32(self.img, off + 8, 0)          # atime
        put32(self.img, off + 12, 0)         # ctime
        put32(self.img, off + 16, 0)         # mtime
        put32(self.img, off + 20, 0)         # dtime
        put16(self.img, off + 24, 0)         # gid
        put16(self.img, off + 26, links)     # links_count
        # i_blocks 的单位是 512 字节扇区；`nblk` 由调用方算好（含间接块本身）
        if nblk is None:
            nblk = sum(1 for s in slots if s)
        self.write_i_blocks(ino, nblk)
        for i, b in enumerate(slots[:15]):
            put32(self.img, off + 40 + i * 4, b)

    def write_i_blocks(self, ino, nblk):
        off = self.inode_off(ino) + 28
        put32(self.img, off, nblk * (BS // 512))

    def write_fast_symlink(self, ino, target):
        if len(target) >= 60:
            raise RuntimeError("链接目标 %r 太长（fast symlink 上限 59 字节）" % target)
        off = self.inode_off(ino)
        put16(self.img, off + 0, MODE_LNK)
        put32(self.img, off + 4, len(target))
        put16(self.img, off + 26, 1)
        put32(self.img, off + 28, 0)
        for i, ch in enumerate(target):
            self.img[off + 40 + i] = ch

    def write_dir(self, block, entries):
        base = block * BS
        off = 0
        for i, (ino, name) in enumerate(entries):
            need = 8 + len(name)
            last = i + 1 == len(entries)
            rec = (BS - off) if last else ((need + 3) & ~3)
            if rec < need:
                raise RuntimeError("目录块放不下（%d 项）" % len(entries))
            put32(self.img, base + off, ino)
            put16(self.img, base + off + 4, rec)
            self.img[base + off + 6] = len(name)
            self.img[base + off + 7] = 2 if (name in (b".", b"..")) else 1
            self.img[base + off + 8:base + off + 8 + len(name)] = name
            off += rec


def alloc_data(im, data):
    """把 `data` 写进新块，返回 `(15 个块指针, 实际占用的总块数)`。"""
    nblk = (len(data) + BS - 1) // BS
    if nblk == 0:
        return [0] * 15, 0
    total = nblk
    data_blocks = []
    for _ in range(nblk):
        data_blocks.append(im.alloc_block())
    for i, b in enumerate(data_blocks):
        chunk = data[i * BS:(i + 1) * BS]
        im.img[b * BS:b * BS + len(chunk)] = chunk

    slots = [0] * 15
    for i in range(min(MAX_DIRECT, nblk)):
        slots[i] = data_blocks[i]
    rest = data_blocks[MAX_DIRECT:]
    if rest:
        # 一级间接
        take = rest[:PPB]
        rest = rest[PPB:]
        ind = im.alloc_block()
        total += 1
        for i, b in enumerate(take):
            put32(im.img, ind * BS + i * 4, b)
        slots[12] = ind
        if rest:
            # 二级间接：第一层指向若干个"指针块"
            dbl = im.alloc_block()
            total += 1
            for j in range(0, len(rest), PPB):
                chunk = rest[j:j + PPB]
                sub = im.alloc_block()
                total += 1
                for i, b in enumerate(chunk):
                    put32(im.img, sub * BS + i * 4, b)
                put32(im.img, dbl * BS + (j // PPB) * 4, sub)
            slots[13] = dbl
            if len(rest) > PPB * PPB:
                raise RuntimeError("文件超过二级间接覆盖范围（约 64 MiB）")
    return slots, total


def emit(im, node, parent_ino):
    """递归写入一个节点，返回它的 inode 号。"""
    if isinstance(node, dict):
        ino = im.alloc_inode()
        kids = []
        for name in sorted(node):
            kids.append((name.encode(), emit(im, node[name], ino)))
        blk = im.alloc_block()
        im.write_dir(blk, [(ino, b"."), (parent_ino, b"..")] + [(k, n) for n, k in kids])
        im.write_inode(ino, MODE_DIR, BS, [blk], 2, 1)
        return ino
    if isinstance(node, Link):
        ino = im.alloc_inode()
        im.write_fast_symlink(ino, node.target)
        return ino
    ino = im.alloc_inode()
    slots, nblk = alloc_data(im, node)
    im.write_inode(ino, MODE_FILE, len(node), slots, 1, nblk)
    return ino


def build(files, total_blocks):
    im = Image(total_blocks)

    tree = {}
    for path, data in files.items():
        parts = [p for p in path.split("/") if p]
        if not parts:
            continue
        cur = tree
        for p in parts[:-1]:
            nxt = cur.setdefault(p, {})
            if not isinstance(nxt, dict):
                raise RuntimeError("/%s 既是文件又是目录" % p)
            cur = nxt
        cur[parts[-1]] = data

    # 根固定是 2 号 inode
    root_ino = im.alloc_inode()
    assert root_ino == 2
    kids = []
    for name in sorted(tree):
        kids.append((name.encode(), emit(im, tree[name], 2)))
    root_blk = im.alloc_block()
    im.write_dir(root_blk, [(2, b"."), (2, b"..")] + [(k, n) for n, k in kids])
    im.write_inode(2, MODE_DIR, BS, [root_blk], 2, 1)

    # ---- 位图 ----
    for b in im.used_blocks:
        im.img[3 * BS + b // 8] |= 1 << (b % 8)
    for i in im.used_inos:
        im.img[4 * BS + (i - 1) // 8] |= 1 << ((i - 1) % 8)

    # ---- 块组描述符 ----
    gd = 2 * BS
    put32(im.img, gd + 0, 3)                # bg_block_bitmap
    put32(im.img, gd + 4, 4)                # bg_inode_bitmap
    put32(im.img, gd + 8, im.inode_table)
    put16(im.img, gd + 12, min(0xFFFF, total_blocks - len(im.used_blocks)))
    put16(im.img, gd + 14, min(0xFFFF, INODES_PER_GROUP - len(im.used_inos)))

    # ---- 超级块 ----
    sb = 1024
    put32(im.img, sb + 0, INODES_PER_GROUP)
    put32(im.img, sb + 4, total_blocks)
    put32(im.img, sb + 8, 0)
    put32(im.img, sb + 12, total_blocks - len(im.used_blocks))
    put32(im.img, sb + 16, INODES_PER_GROUP - len(im.used_inos))
    put32(im.img, sb + 20, 1)               # s_first_data_block
    put32(im.img, sb + 24, 0)               # s_log_block_size（1024）
    put32(im.img, sb + 28, 0)
    put32(im.img, sb + 32, 8192)            # s_blocks_per_group
    put32(im.img, sb + 36, 8192)
    put32(im.img, sb + 40, INODES_PER_GROUP)
    put32(im.img, sb + 48, 0)
    put16(im.img, sb + 52, 0)
    put16(im.img, sb + 54, 0xFFFF)
    put16(im.img, sb + 56, 0xEF53)          # s_magic
    put16(im.img, sb + 58, 1)               # s_state = clean
    put16(im.img, sb + 60, 1)               # s_errors = continue
    put32(im.img, sb + 76, 1)               # s_rev_level = 1（动态）
    put32(im.img, sb + 84, FIRST_INO)
    put16(im.img, sb + 88, INODE_SIZE)
    return im, tree


def check(img, tree, total_blocks):
    """独立地把镜像读回来，验证 sb / inode / 目录 / 文件内容。"""
    sb = 1024
    assert struct.unpack_from("<H", img, sb + 56)[0] == 0xEF53, "魔数不对"
    blocks = struct.unpack_from("<I", img, sb + 4)[0]
    assert blocks == total_blocks, "块数不符"
    assert struct.unpack_from("<I", img, sb + 24)[0] == 0, "log_block_size 不是 0"
    itable = struct.unpack_from("<I", img, 2 * BS + 8)[0]

    def inode(ino):
        off = itable * BS + (ino - 1) * INODE_SIZE
        mode = struct.unpack_from("<H", img, off)[0]
        size = struct.unpack_from("<I", img, off + 4)[0]
        ptr = list(struct.unpack_from("<15I", img, off + 40))
        return mode, size, ptr

    def bmap(ptr, logical):
        if logical < MAX_DIRECT:
            return ptr[logical]
        idx = logical - MAX_DIRECT
        if idx < PPB:
            root, depth = ptr[12], 1
        elif idx < PPB + PPB * PPB:
            idx -= PPB
            root, depth = ptr[13], 2
        else:
            idx -= PPB + PPB * PPB
            root, depth = ptr[14], 3
        cur, div = root, PPB ** (depth - 1)
        for _ in range(depth):
            if cur == 0:
                return 0
            slot = (idx // div) % PPB
            cur = struct.unpack_from("<I", img, cur * BS + slot * 4)[0]
            div //= PPB
        return cur

    def file_bytes(ino):
        mode, size, ptr = inode(ino)
        out = bytearray()
        n = (size + BS - 1) // BS
        for i in range(n):
            b = bmap(ptr, i)
            if b == 0:
                out += b"\x00" * BS
            else:
                out += bytes(img[b * BS:b * BS + BS])
        return bytes(out[:size])

    mode, size, ptr = inode(2)
    assert mode & 0o170000 == 0o040000, "根不是目录"
    entries = {}
    off = 0
    while off < size:
        base = ptr[0] * BS + off
        ino, rec, nlen = struct.unpack_from("<IHB", img, base)
        assert rec >= 8, "rec_len 非法"
        name = bytes(img[base + 8:base + 8 + nlen])
        if name not in (b".", b".."):
            entries[name] = ino
        off += rec

    def entries_lookup(path):
        parts = [p for p in path.split("/") if p]
        cur = 2
        for p in parts:
            m, s, pp = inode(cur)
            off = 0
            found = None
            while off < s:
                base = pp[0] * BS + off
                ino, rec, nlen = struct.unpack_from("<IHB", img, base)
                nm = bytes(img[base + 8:base + 8 + nlen])
                if nm == p.encode():
                    found = ino
                    break
                off += rec
            assert found is not None, "找不到 %s" % p
            cur = found
        return cur

    nfiles = 0
    nlinks = 0

    def walk(node, prefix):
        nonlocal nfiles, nlinks
        for name in sorted(node):
            val = node[name]
            ino = entries_lookup(prefix + "/" + name)
            if isinstance(val, dict):
                walk(val, prefix + "/" + name)
            elif isinstance(val, Link):
                m, s, _ = inode(ino)
                assert m & 0o170000 == 0o120000, "%s 不是符号链接" % name
                got = bytes(img[itable * BS + (ino - 1) * INODE_SIZE + 40:
                                itable * BS + (ino - 1) * INODE_SIZE + 40 + s])
                assert got == val.target, "%s 链接目标不符 %r != %r" % (name, got, val.target)
                nlinks += 1
            else:
                m, s, _ = inode(ino)
                assert m & 0o170000 == 0o100000, "%s 不是普通文件" % name
                assert s == len(val), "%s 大小不符 %d != %d" % (name, s, len(val))
                got = file_bytes(ino)
                assert got == val, "%s 内容不符（%d 字节）" % (name, len(val))
                nfiles += 1

    walk(tree, "")
    print("[ext2] 自检：%d 个文件、%d 个链接逐字节一致" % (nfiles, nlinks))
    return len(entries)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--output", required=True)
    ap.add_argument("--blocks", type=int, default=16384, help="总块数（1 KiB/块）")
    ap.add_argument("--file", action="append", default=[], help="/路径=宿主文件")
    ap.add_argument("--text", action="append", default=[], help="/路径=文本内容")
    ap.add_argument("--link", action="append", default=[], help="/路径=链接目标")
    ap.add_argument("--no-check", action="store_true")
    a = ap.parse_args()

    files = {}

    # MSYS/Git-Bash 会把 `/bin/hello` 这种参数当 Windows 路径转换掉，
    # 所以这里同时接受 "bin/hello" 与 "/bin/hello" 两种写法。
    def norm(p):
        p = p.replace("\\", "/")
        if not p.startswith("/"):
            p = "/" + p
        return p

    for spec in a.file:
        if "=" not in spec:
            print("[错误] --file 需要 /路径=文件 形式")
            return 1
        path, host = spec.split("=", 1)
        path = norm(path)
        with open(host, "rb") as f:
            files[path] = f.read()
        print("[ext2] %-20s <- %s (%d B)" % (path, os.path.basename(host), len(files[path])))
    for spec in a.text:
        path, text = spec.split("=", 1)
        path = norm(path)
        files[path] = text.encode()
        print("[ext2] %-20s <- 文本 (%d B)" % (path, len(files[path])))
    for spec in a.link:
        path, target = spec.split("=", 1)
        path = norm(path)
        files[path] = Link(target)
        print("[ext2] %-20s -> %s" % (path, target))

    im, tree = build(files, a.blocks)
    if not a.no_check:
        n = check(im.img, tree, a.blocks)
        print("[ext2] 自检通过：根目录 %d 项，用 %d 块 / 共 %d 块"
              % (n, len(im.used_blocks), a.blocks))
    open(a.output, "wb").write(im.img)
    print("[ext2] 写出 %s（%d KiB）" % (a.output, len(im.img) // 1024))
    return 0


if __name__ == "__main__":
    sys.exit(main())
