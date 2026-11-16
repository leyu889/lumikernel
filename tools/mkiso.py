#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
mkiso.py — 自研 ISO9660 + El Torito 可启动 ISO 打包器（零依赖，纯 Python）

背景：本机无 xorriso / mkisofs / grub-mkrescue（且无网络无法安装），
故自研此打包器，直接按 ISO9660 规范 + El Torito 规范手工构造可启动 ISO。
采用「1.44MB 软盘仿真」引导方式：BIOS 把引导镜像当成 A: 盘，
内核因此可用 int 13h 简单读取后续扇区，无需自带 ISO9660 驱动。

用法:
    python mkiso.py --boot-image <引导镜像> --output <输出.iso> [--volume-id ID]

磁盘布局（单位：2048 字节扇区）
    LBA 0..15   系统区（全 0，El Torito 与传统 ISO 均不使用）
    LBA 16      Primary Volume Descriptor
    LBA 17      Boot Record Volume Descriptor（El Torito 指针）
    LBA 18      Volume Descriptor Set Terminator
    LBA 19      Boot Catalog（引导目录：校验项 + 默认引导项）
    LBA 20      Path Table (Type L)
    LBA 21      Path Table (Type M)
    LBA 22      Root Directory
    LBA 23      README.TXT
    LBA 24..    引导镜像（1.44MB 软盘，含 512B 引导扇区）
"""
import argparse
import datetime
import struct
import sys

SECTOR = 2048
FLOPPY_1440 = 1474560          # 1.44MB 软盘镜像字节数
MEDIA_144MB = 0x02             # El Torito 媒体类型：1.44MB 软盘仿真


def both16(v):
    """16 位双端序（小端在前）"""
    return struct.pack('<H', v) + struct.pack('>H', v)


def both32(v):
    """32 位双端序"""
    return struct.pack('<I', v) + struct.pack('>I', v)


def dt7(when=None):
    """目录记录用：七字节日期时间（年自1900/月/日/时/分/秒/时区）"""
    when = when or datetime.datetime.now()
    return bytes([when.year - 1900, when.month, when.day,
                  when.hour, when.minute, when.second, 0])


def dt17(when=None):
    """卷描述符用：十七字节文本日期 YYYYMMDDHHMMSSCC + 时区字节"""
    when = when or datetime.datetime.now()
    s = '%04d%02d%02d%02d%02d%02d00' % (when.year, when.month, when.day,
                                        when.hour, when.minute, when.second)
    return s.encode('ascii') + b'\x00'


def dir_record(ident, extent_lba, size, is_dir):
    """构造一条 ISO9660 目录记录（长度自动补齐为偶数）"""
    rec = bytearray()
    rec.append(0)                                   # 记录长度（回填）
    rec.append(0)                                   # 扩展属性长度
    rec += both32(extent_lba)                       # 数据起始 LBA
    rec += both32(size)                             # 数据长度
    rec += dt7()                                    # 记录时间
    rec.append(0x02 if is_dir else 0x00)            # 文件标志
    rec.append(0)                                   # 文件单元大小
    rec.append(0)                                   # 交错间隔
    rec += both16(1)                                # 卷序数
    rec.append(len(ident))                          # 标识符长度
    rec += ident
    if len(rec) % 2:
        rec.append(0)
    rec[0] = len(rec)
    return bytes(rec)


def make_pvd(vol_space, path_table_size, pt_lba_l, pt_lba_m, root_rec,
             vol_id):
    """Primary Volume Descriptor"""
    d = bytearray(SECTOR)
    d[0] = 0x01
    d[1:6] = b'CD001'
    d[6] = 0x01
    # 以「偏移 -> 字段字节」的方式逐字段写入。
    # 注意：不能用 d[a:b] = 长度不同的数据（会静默改变 bytearray 长度导致整盘错位），
    # 因此这里统一用 d[off:off+len(blob)] 赋值并做越界校验。
    fields = [
        (8,   b'LUMIOS'.ljust(32, b' ')),                       # 系统标识
        (40,  vol_id.encode('ascii', 'replace')[:32].ljust(32, b' ')),  # 卷标识
        (80,  both32(vol_space)),                               # 卷总扇区数
        (120, both16(1)),                                       # 卷集大小
        (124, both16(1)),                                       # 卷序数
        (128, both16(SECTOR)),                                  # 逻辑块大小
        (132, both32(path_table_size)),                         # 路径表字节数
        (140, struct.pack('<I', pt_lba_l)),                      # Type-L 路径表
        (144, struct.pack('<I', 0)),
        (148, struct.pack('>I', pt_lba_m)),                      # Type-M 路径表
        (152, struct.pack('>I', 0)),
        (156, root_rec),                                        # 根目录记录(34B)
        (190, b' ' * 128),                                       # 卷集标识
        (318, b'LUMIOS PROJECT'.ljust(128, b' ')[:128]),        # 出版者
        (446, b' ' * 128),                                       # 数据准备者
        (574, b'LUMIKERNEL'.ljust(128, b' ')[:128]),            # 应用标识
        (702, b' ' * 37),                                        # 版权文件标识
        (739, b' ' * 37),                                        # 摘要文件标识
        (776, b' ' * 37),                                        # 书目文件标识
        (813, dt17()),                                           # 创建时间(17B)
        (830, dt17()),                                           # 修改时间(17B)
        (847, b'0' * 16 + b'\x00'),                              # 失效时间
        (864, b'0' * 16 + b'\x00'),                              # 生效时间
        (881, bytes([0x01])),                                    # 文件结构版本
    ]
    for off, blob in fields:
        if off + len(blob) > SECTOR:
            raise SystemExit('[错误] PVD 字段越界: 偏移 %d, 长度 %d' % (off, len(blob)))
        d[off:off + len(blob)] = blob
    if len(d) != SECTOR:
        raise SystemExit('[错误] PVD 长度异常: %d' % len(d))
    return bytes(d)


def make_boot_record_vd(catalog_lba):
    """El Torito 引导记录卷描述符（类型 0）"""
    d = bytearray(SECTOR)
    d[0] = 0x00
    d[1:6] = b'CD001'
    d[6] = 0x01
    d[7:39] = b'EL TORITO SPECIFICATION'.ljust(32, b'\x00')
    d[71:75] = struct.pack('<I', catalog_lba)
    return bytes(d)


def make_terminator():
    d = bytearray(SECTOR)
    d[0] = 0xFF
    d[1:6] = b'CD001'
    d[6] = 0x01
    return bytes(d)


def _validation_entry(platform, label):
    """El Torito 校验项（32 字节，带 16 位反码校验和）"""
    e = bytearray(32)
    e[0] = 0x01                                     # 头标识
    e[1] = platform                                 # 0x00=80x86, 0xEF=EFI
    e[4:28] = label.ljust(24, b'\x00')
    e[30] = 0x55
    e[31] = 0xAA
    total = 0
    for i in range(0, 30, 2):
        total = (total + (e[i] | (e[i + 1] << 8))) & 0xFFFF
    csum = (-total) & 0xFFFF
    e[28] = csum & 0xFF
    e[29] = (csum >> 8) & 0xFF
    return bytes(e)


def _default_entry(media, load_seg, sector_count, image_lba):
    e = bytearray(32)
    e[0] = 0x88                                     # 可引导
    e[1] = media                                    # 0=无仿真, 2=1.44MB 软盘仿真
    e[2:4] = struct.pack('<H', load_seg)
    e[4] = 0x00                                     # 系统类型
    e[5] = 0x00
    e[6:8] = struct.pack('<H', sector_count)
    e[8:12] = struct.pack('<I', image_lba)
    return bytes(e)


def make_boot_catalog(image_lba, load_seg=0x0000, sector_count=1,
                      efi_lba=None, efi_sectors=0):
    """El Torito 引导目录。

    一个目录里可以放**多个 section**，每个 section = 校验项 + 默认引导项(+扩展项)。
    固件按"平台 id 与自己匹配"挑一个：
      - 0x00 / 80x86 —— 老 BIOS 用，1.44MB 软盘仿真，即我们自建的 stage1 链；
      - 0xEF / EFI   —— UEFI 固件用，无仿真，直接把 .efi 映像读进内存并当 PE 加载。
    两个 section 并存，同一张盘 BIOS 和 UEFI 都能起。
    """
    cat = bytearray(SECTOR)
    cat[0:32] = _validation_entry(0x00, b'LumiOS-BIOS')
    cat[32:64] = _default_entry(MEDIA_144MB, load_seg, sector_count, image_lba)
    if efi_lba is not None:
        # 第二段必须是「节头项」而不是又一个校验项：
        #   0x90 = 最后一段，0x91 = 后面还有；
        #   byte1 = 平台 id（0xEF = EFI）；bytes2..3 = 本节条目数。
        # 写成校验项（0x01）的话，固件在找平台时会直接判定整张目录无效——
        # 现象就是 "failed to load ... CD-ROM: Not Found"。
        hdr = bytearray(32)
        hdr[0] = 0x90
        hdr[1] = 0xEF
        struct.pack_into('<H', hdr, 2, 1)
        cat[64:96] = bytes(hdr)
        # 无仿真（media=0）；sector_count 是 512 字节扇区数
        cat[96:128] = _default_entry(0x00, 0x0000, efi_sectors, efi_lba)
    return bytes(cat)


def make_path_table(root_lba, big_endian=False):
    """路径表（只有根目录一项）"""
    e = bytearray()
    e.append(0x01)                                  # 目录标识符长度
    e.append(0x00)                                  # 扩展属性长度
    e += struct.pack('>I' if big_endian else '<I', root_lba)
    e += struct.pack('>H' if big_endian else '<H', 1)
    e.append(0x00)                                  # 根目录标识符
    if len(e) % 2:
        e.append(0x00)
    return bytes(e)


def build_iso(boot_image_path, out_path, volume_id='LUMIOS_KERNEL', efi_path=None):
    raw = open(boot_image_path, 'rb').read()
    efi = open(efi_path, 'rb').read() if efi_path else None
    if len(raw) != FLOPPY_1440:
        raise SystemExit('[错误] 引导镜像必须是 1440KB(1474560 字节)，当前 %d' % len(raw))

    readme = (
        'LumiOS Kernel -- self-developed boot chain\r\n'
        '------------------------------------------\r\n'
        'This ISO boots a self-made bootloader/kernel.\r\n'
        'Boot method: El Torito 1.44MB floppy emulation.\r\n'
        'No Linux, no third-party kernel involved.\r\n'
    ).encode('ascii')

    # --- 布局 ---
    LBA_PVD = 16
    LBA_BRVD = 17
    LBA_TERM = 18
    LBA_CAT = 19
    LBA_PT_L = 20
    LBA_PT_M = 21
    LBA_ROOT = 22
    LBA_README = 23
    LBA_IMAGE = 24
    image_sectors = (len(raw) + SECTOR - 1) // SECTOR
    LBA_EFI = LBA_IMAGE + image_sectors
    efi_sectors = ((len(efi) + SECTOR - 1) // SECTOR) if efi else 0
    total_sectors = LBA_EFI + efi_sectors

    pt = make_path_table(LBA_ROOT, big_endian=False)
    pt_m = make_path_table(LBA_ROOT, big_endian=True)

    root_rec = dir_record(b'\x00', LBA_ROOT, SECTOR, True)
    pvd = make_pvd(total_sectors, len(pt), LBA_PT_L, LBA_PT_M, root_rec, volume_id)

    # --- 根目录内容 ---
    dirent = bytearray()
    dirent += dir_record(b'\x00', LBA_ROOT, SECTOR, True)          # .
    dirent += dir_record(b'\x01', LBA_ROOT, SECTOR, True)          # ..
    dirent += dir_record(b'README.TXT;1', LBA_README, len(readme), False)
    dirent += dir_record(b'BOOT.IMG;1', LBA_IMAGE, len(raw), False)
    if efi:
        dirent += dir_record(b'BOOTX64.EFI;1', LBA_EFI, len(efi), False)
    rootdir = bytes(dirent).ljust(SECTOR, b'\x00')

    # --- 防御性检查：定长描述符必须正好 2048 字节 ---
    # （Python 中 slice 赋值长度不匹配会静默改变 bytearray 长度，曾导致整盘扇区错位）
    fixed = [('PVD', pvd), ('BootRecordVD', make_boot_record_vd(LBA_CAT)),
             ('Terminator', make_terminator()),
             ('BootCatalog', make_boot_catalog(
                 LBA_IMAGE, sector_count=1,
                 efi_lba=(LBA_EFI if efi else None),
                 efi_sectors=((len(efi) + 511) // 512) if efi else 0))]
    for name, blob in fixed:
        if len(blob) != SECTOR:
            raise SystemExit('[错误] %s 长度应为 %d，实际 %d' % (name, SECTOR, len(blob)))
    for name, blob in (('PathTableL', pt), ('PathTableM', pt_m), ('RootDir', rootdir)):
        if len(blob) > SECTOR:
            raise SystemExit('[错误] %s 超过 %d 字节 (%d)' % (name, SECTOR, len(blob)))

    def sector_bytes(offset_lba):
        return b'\x00' * (SECTOR * offset_lba)

    out = bytearray()
    out += sector_bytes(LBA_PVD)                       # LBA 0..15 系统区
    out += pvd                                          # 16
    out += make_boot_record_vd(LBA_CAT)                 # 17
    out += make_terminator()                            # 18
    out += make_boot_catalog(LBA_IMAGE, sector_count=1,
                             efi_lba=(LBA_EFI if efi else None),
                             efi_sectors=((len(efi) + 511) // 512) if efi else 0)  # 19
    out += pt.ljust(SECTOR, b'\x00')                    # 20
    out += pt_m.ljust(SECTOR, b'\x00')                  # 21
    out += rootdir                                      # 22
    out += readme.ljust(SECTOR, b'\x00')                # 23
    out += raw                                          # 24.. 引导镜像
    if efi:
        out += efi                                       # .. EFI 装载器
    pad = total_sectors * SECTOR - len(out)
    if pad > 0:
        out += b'\x00' * pad

    open(out_path, 'wb').write(bytes(out))
    return total_sectors


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--boot-image', required=True)
    ap.add_argument('--output', required=True)
    ap.add_argument('--volume-id', default='LUMIOS_KERNEL')
    ap.add_argument('--efi-image', default=None, help='BOOTX64.EFI（可选；有它就能 UEFI 启动）')
    a = ap.parse_args()
    n = build_iso(a.boot_image, a.output, a.volume_id, a.efi_image)
    print('[OK] ISO 已生成: %s  (共 %d 扇区, %.2f MB)'
          % (a.output, n, n * SECTOR / 1048576.0))
    return 0


if __name__ == '__main__':
    sys.exit(main())
