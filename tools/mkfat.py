#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""mkfat.py — 生成一个最小 FAT16 映像，用来装 UEFI 装载器（M10）

为什么需要：UEFI 固件不直接加载 El Torito 里的那颗裸 PE。
EDK2（VBox / 真机）拿到 El Torito 的"无仿真"映像后，会把它当成一块**磁盘**，
挂上文件系统，然后去找 `\\EFI\\BOOT\\BOOTX64.EFI`。
所以引导映像必须是一个 FAT 文件系统，里面放着我们的 .efi。

grub-mkrescue 也是这么干的（它塞的是一张 EFI 专用的 FAT 映像）。

用法:
    python mkfat.py --file EFI/BOOT/BOOTX64.EFI=<path> --output efi.img [--mb 4]
"""

import argparse
import struct
import sys


class Fat16:
    def __init__(self, sectors):
        self.sectors = sectors
        self.bytes_per_sector = 512
        self.sec_per_cluster = 1
        self.reserved = 1
        self.nfats = 2
        self.root_entries = 512
        self.root_sectors = self.root_entries * 32 // self.bytes_per_sector
        # FAT 扇区数：要用簇数算，先估一个上界再回代
        clusters_est = sectors - self.reserved - self.root_sectors
        fat_bytes = (clusters_est + 2) * 2
        self.fat_sectors = (fat_bytes + self.bytes_per_sector - 1) // self.bytes_per_sector
        self.data_start = self.reserved + self.nfats * self.fat_sectors + self.root_sectors
        self.clusters = sectors - self.data_start
        if not (4085 <= self.clusters <= 65524):
            raise SystemExit('[错误] 簇数 %d 不在 FAT16 范围，请调 --mb' % self.clusters)
        self.img = bytearray(sectors * self.bytes_per_sector)
        self.fat = [0] * (self.clusters + 2)
        self.fat[0] = 0xFFF8
        self.fat[1] = 0xFFFF
        self.next_cluster = 2

    # ---------------- 底层 ----------------
    def cluster_off(self, cl):
        return (self.data_start + (cl - 2) * self.sec_per_cluster) * self.bytes_per_sector

    def alloc_chain(self, nbytes):
        n = max(1, (nbytes + self.bytes_per_sector - 1) // self.bytes_per_sector)
        first = self.next_cluster
        if first + n - 1 > self.clusters + 1:
            raise SystemExit('[错误] FAT16 映像空间不足')
        for i in range(n):
            c = first + i
            self.fat[c] = 0xFFFF if i == n - 1 else c + 1
        self.next_cluster += n
        return first

    def write_data(self, data):
        first = self.alloc_chain(len(data))
        off = self.cluster_off(first)
        self.img[off:off + len(data)] = data
        return first

    @staticmethod
    def short_name(name):
        base, _, ext = name.partition('.')
        base = base.upper()[:8].ljust(8)
        ext = ext.upper()[:3].ljust(3)
        return (base + ext).encode('ascii')

    @staticmethod
    def dirent(name, attr, cluster, size):
        e = bytearray(32)
        if name == '.':
            e[0:11] = b'.          '
        elif name == '..':
            e[0:11] = b'..         '
        else:
            # 注意：`partition('.')` 会把 ".." 拆成 base="" + ext="."，
            # 生成一个"8 空格 + 点"的非法名字。所以点目录必须单独写字面量。
            e[0:11] = Fat16.short_name(name)
        e[11] = attr
        # 时间字段给一个固定值，避免每次构建产物二进制不同（对比方便）
        struct.pack_into('<H', e, 24, (2026 - 1980) << 9 | (1 << 5) | 1)  # 日期
        struct.pack_into('<H', e, 22, 0)                                   # 时间
        struct.pack_into('<H', e, 26, cluster & 0xFFFF)
        struct.pack_into('<I', e, 28, size)
        return bytes(e)

    # ---------------- 目录树 ----------------
    def build_tree(self, spec):
        """spec: {'b': bytes} 文件 或 {'d': {name: spec}} 目录。递归写盘。"""
        # 分配根目录固定区
        root_off = (self.reserved + self.nfats * self.fat_sectors) * self.bytes_per_sector
        entries = []
        # 先为每个子目录分配一个簇，再写内容（"." 和 ".." 需要自己的簇号）
        names = sorted(spec.keys())
        dirs = {}
        for n in names:
            v = spec[n]
            if isinstance(v, dict):
                dirs[n] = self.alloc_chain(self.bytes_per_sector)
        for n in names:
            v = spec[n]
            if isinstance(v, dict):
                entries.append(self.dirent(n, 0x10, dirs[n], 0))
            else:
                entries.append(self.dirent(n, 0x20, self.write_data(v), len(v)))
        blob = b''.join(entries)
        self.img[root_off:root_off + len(blob)] = blob
        # 子目录内容
        for n, first in dirs.items():
            self.write_dir(first, 0, spec[n])

    def write_dir(self, first, parent, spec):
        ents = []
        ents.append(self.dirent('.', 0x10, first, 0))
        ents.append(self.dirent('..', 0x10, parent, 0))
        names = sorted(spec.keys())
        subdirs = {}
        for n in names:
            if isinstance(spec[n], dict):
                subdirs[n] = self.alloc_chain(self.bytes_per_sector)
        for n in names:
            if isinstance(spec[n], dict):
                ents.append(self.dirent(n, 0x10, subdirs[n], 0))
            else:
                ents.append(self.dirent(n, 0x20, self.write_data(spec[n]), len(spec[n])))
        blob = b''.join(ents)
        off = self.cluster_off(first)
        self.img[off:off + len(blob)] = blob
        for n, f in subdirs.items():
            self.write_dir(f, first, spec[n])

    # ---------------- 收尾 ----------------
    def finish(self):
        # FAT 表
        fat_bytes = bytearray(self.fat_sectors * self.bytes_per_sector)
        for i, v in enumerate(self.fat):
            struct.pack_into('<H', fat_bytes, i * 2, v)
        for k in range(self.nfats):
            off = (self.reserved + k * self.fat_sectors) * self.bytes_per_sector
            self.img[off:off + len(fat_bytes)] = fat_bytes

        # 引导扇区（BPB）
        b = bytearray(self.bytes_per_sector)
        b[0:3] = b'\xEB\x3C\x90'
        b[3:11] = b'LUMIFS16'
        struct.pack_into('<H', b, 11, self.bytes_per_sector)
        b[13] = self.sec_per_cluster
        struct.pack_into('<H', b, 14, self.reserved)
        b[16] = self.nfats
        struct.pack_into('<H', b, 17, self.root_entries)
        if self.sectors < 65536:
            struct.pack_into('<H', b, 19, self.sectors)
        else:
            struct.pack_into('<I', b, 32, self.sectors)
        b[21] = 0xF8                      # 介质描述符：固定盘
        struct.pack_into('<H', b, 22, self.fat_sectors)
        struct.pack_into('<H', b, 24, 32)     # 每道扇区
        struct.pack_into('<H', b, 26, 64)     # 磁头
        struct.pack_into('<I', b, 28, 0)      # 隐藏扇区
        b[36] = 0x80                      # 驱动器号
        b[38] = 0x29                      # 扩展引导签名
        struct.pack_into('<I', b, 39, 0x4C554D49)  # 卷序列号 "LUMI"
        b[43:54] = b'LUMIEFI    '
        b[54:62] = b'FAT16   '
        b[510] = 0x55
        b[511] = 0xAA
        self.img[0:self.bytes_per_sector] = b


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--file', action='append', default=[], help='/EFI/BOOT/BOOTX64.EFI=宿主文件')
    ap.add_argument('--output', required=True)
    ap.add_argument('--mb', type=int, default=4, help='映像大小（MiB）')
    a = ap.parse_args()

    sectors = a.mb * 1024 * 1024 // 512
    fs = Fat16(sectors)
    tree = {}
    for spec in a.file:
        if '=' not in spec:
            print('[错误] --file 需要 路径=文件')
            return 1
        path, host = spec.split('=', 1)
        data = open(host, 'rb').read()
        parts = [p for p in path.replace('\\', '/').split('/') if p]
        cur = tree
        for p in parts[:-1]:
            cur = cur.setdefault(p, {})
            if not isinstance(cur, dict):
                raise SystemExit('[错误] %s 既是文件又是目录' % p)
        cur[parts[-1]] = data
        print('[fat16] /%-22s <- %s (%d B)' % ('/'.join(parts), host.split('/')[-1].split('\\')[-1], len(data)))

    fs.build_tree(tree)
    fs.finish()
    open(a.output, 'wb').write(bytes(fs.img))
    print('[fat16] %s（%d MiB，%d 簇）' % (a.output, a.mb, fs.clusters))
    return 0


if __name__ == '__main__':
    sys.exit(main())
