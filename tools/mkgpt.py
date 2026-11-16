#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""mkgpt.py — 把 FAT 映像包成「GPT 磁盘 + EFI 系统分区」（M10）

为什么走这条路而不是 El Torito：
真机（以及 VirtualBox 的 EFI 固件）启动 UEFI 程序的**标准**路径是
"读 GPT → 找到 ESP（EFI System Partition，类型 GUID C12A7328-…）→
在它的 FAT 文件系统里找 \\EFI\\BOOT\\BOOTX64.EFI"。
实测 VBox 的 EFI 对 El Torito 的 EFI 引导项不认账（报 Not Found），
而 GPT+ESP 一次就过——所以以它为准，顺带这份 .img 就是"可 dd 到 U 盘"的产物。

用法:
    python mkgpt.py --esp efi.img --output disk.img [--mb 64]
"""

import argparse
import struct
import sys
import uuid
import zlib

SECTOR = 512
ESP_TYPE = 'C12A7328-F81F-11D2-BA4B-00A0C93EC93B'
ESP_START_LBA = 2048          # 1 MiB 对齐：绝大多数固件都习惯这个位置
# 系统盘（ext2）与数据盘（LumiFS）在**同一块盘**上的固定位置。
# 真机只有一块 U 盘，所以三样东西必须装在一起：
#   ESP（装载器）→ ext2（/bin/busybox）→ LumiFS（可写数据）
EXT2_START_LBA = 16384        # 8 MiB
LUMI_START_LBA = 49152        # 24 MiB
LINUX_FS = "0FC63DAF-8483-4772-8E79-3D69D8477DE4"


def guid_bytes(s):
    """GUID 文本 -> GPT 里那 16 字节。

    GPT 用的是微端序（前三段小端、后两段大端）——`uuid.bytes_le` 正好是这个布局，
    手写一遍只会写错（第一版把 clock_seq 弄反了）。
    """
    return uuid.UUID(s).bytes_le


def entry(type_guid, part_guid, first, last, name):
    e = bytearray(128)
    e[0:16] = guid_bytes(type_guid)
    e[16:32] = guid_bytes(part_guid)
    struct.pack_into('<QQQ', e, 32, first, last, 0)
    nm = name.encode('utf-16-le')[:70]
    e[56:56 + len(nm)] = nm
    return bytes(e)


def header(cur, backup, first_usable, last_usable, disk_guid, ent_lba, ent_crc):
    h = bytearray(92)
    h[0:8] = b'EFI PART'
    struct.pack_into('<IIII', h, 8, 0x00010000, 92, 0, 0)
    struct.pack_into('<QQQQ', h, 24, cur, backup, first_usable, last_usable)
    h[56:72] = guid_bytes(disk_guid)
    struct.pack_into('<QII', h, 72, ent_lba, 128, 128)
    struct.pack_into('<I', h, 88, ent_crc)
    struct.pack_into('<I', h, 16, zlib.crc32(bytes(h)) & 0xFFFFFFFF)
    return bytes(h)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--esp', required=True)
    ap.add_argument('--ext2', default=None, help='系统盘镜像（放进第二个分区）')
    ap.add_argument('--lumi', default=None, help='LumiFS 数据盘镜像（第三个分区）')
    ap.add_argument('--output', required=True)
    ap.add_argument('--mb', type=int, default=64)
    a = ap.parse_args()

    esp = open(a.esp, 'rb').read()
    ext2 = open(a.ext2, 'rb').read() if a.ext2 else None
    lumi = open(a.lumi, 'rb').read() if a.lumi else None
    if len(esp) % SECTOR:
        print('[错误] ESP 映像不是 512 的整数倍')
        return 1

    total = a.mb * 1024 * 1024 // SECTOR
    esp_sectors = len(esp) // SECTOR
    if ESP_START_LBA + esp_sectors + 33 > total:
        print('[错误] 磁盘太小：ESP 需要 %d 扇区' % (ESP_START_LBA + esp_sectors + 33))
        return 1

    disk_guid = str(uuid.uuid4())
    part_guid = str(uuid.uuid4())

    img = bytearray(total * SECTOR)
    img[ESP_START_LBA * SECTOR:ESP_START_LBA * SECTOR + len(esp)] = esp
    if ext2:
        img[EXT2_START_LBA * SECTOR:EXT2_START_LBA * SECTOR + len(ext2)] = ext2
    if lumi:
        img[LUMI_START_LBA * SECTOR:LUMI_START_LBA * SECTOR + len(lumi)] = lumi

    # ---- 保护性 MBR ----
    mbr = bytearray(SECTOR)
    e = 446
    mbr[e + 0] = 0x00                    # 非活动
    mbr[e + 1:e + 4] = b'\x00\x02\x00'   # 起始 CHS（GPT 里没意义）
    mbr[e + 4] = 0xEE                    # 类型：GPT 保护分区
    mbr[e + 5:e + 8] = b'\xFF\xFF\xFF'
    struct.pack_into('<I', mbr, e + 8, 1)
    struct.pack_into('<I', mbr, e + 12, min(total - 1, 0xFFFFFFFF))
    mbr[510] = 0x55
    mbr[511] = 0xAA
    img[0:SECTOR] = mbr

    # ---- 分区表 ----
    ents = bytearray(128 * 128)
    ents[0:128] = entry(ESP_TYPE, part_guid, ESP_START_LBA,
                        ESP_START_LBA + esp_sectors - 1, 'LumiOS ESP')
    parts = [(ESP_START_LBA, esp_sectors)]
    if ext2:
        n = len(ext2) // SECTOR
        ents[128:256] = entry(LINUX_FS, str(uuid.uuid4()),
                              EXT2_START_LBA, EXT2_START_LBA + n - 1, 'LumiOS ROOT')
        parts.append((EXT2_START_LBA, n))
    if lumi:
        n = len(lumi) // SECTOR
        ents[256:384] = entry(LINUX_FS, str(uuid.uuid4()),
                              LUMI_START_LBA, LUMI_START_LBA + n - 1, 'LumiOS DATA')
        parts.append((LUMI_START_LBA, n))
    ent_crc = zlib.crc32(bytes(ents)) & 0xFFFFFFFF
    img[2 * SECTOR:2 * SECTOR + len(ents)] = ents

    last = total - 1
    img[1 * SECTOR:1 * SECTOR + 92] = header(1, last, 34, last - 33, disk_guid, 2, ent_crc)
    # 备份：末尾是头，前 32 扇区是分区表副本
    img[(last - 32) * SECTOR:(last - 32) * SECTOR + len(ents)] = ents
    img[last * SECTOR:last * SECTOR + 92] = header(last, 1, 34, last - 33, disk_guid, last - 32, ent_crc)

    open(a.output, 'wb').write(bytes(img))
    print('[gpt] %s（%d MiB）分区: %s'
          % (a.output, a.mb, ', '.join('LBA%d+%d' % p for p in parts)))
    print('[gpt] 这就是可以 dd 到 U 盘的原始镜像（真机 UEFI 直接认）')
    return 0


if __name__ == '__main__':
    sys.exit(main())
