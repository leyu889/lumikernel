#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
checkiso.py — 自研 ISO9660 / El Torito 结构校验器（零依赖）

用途：在把 ISO 交给虚拟机或真机之前，先静态校验引导结构是否合法，
      避免「黑屏才发现盘做坏了」。校验项：
        1. PVD 主卷描述符（类型/标识/版本/卷大小/逻辑块）
        2. Boot Record VD（El Torito 标识 + 引导目录指针）
        3. 卷描述符结束符
        4. 引导目录：校验和必须为 0、键字节 55 AA、默认引导项合法
        5. 引导镜像首扇区内容与 RBA 指向是否一致
用法: python checkiso.py <file.iso> [--expect-boot <boot.bin>]
"""
import argparse
import struct
import sys

S = 2048


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('iso')
    ap.add_argument('--expect-boot', default=None)
    a = ap.parse_args()

    data = open(a.iso, 'rb').read()
    if len(data) < S * 20:
        print('[失败] 文件太小，不是有效 ISO')
        return 1

    def sec(n):
        return data[n * S:(n + 1) * S]

    ok = True

    def check(name, cond, detail=''):
        nonlocal ok
        mark = 'PASS' if cond else 'FAIL'
        if not cond:
            ok = False
        print('  [%s] %s %s' % (mark, name, detail))

    print('== ISO 引导结构校验: %s ==' % a.iso)
    print('总大小: %d 字节 (%.2f MB)' % (len(data), len(data) / 1048576.0))

    # 1) PVD
    pvd = sec(16)
    print('\n[1] 主卷描述符 (PVD)')
    check('类型=1', pvd[0] == 1, '(实际 %d)' % pvd[0])
    check('标准标识 CD001', pvd[1:6] == b'CD001', '(实际 %r)' % pvd[1:6])
    check('版本=1', pvd[6] == 1)
    vol_space = struct.unpack('<I', pvd[80:84])[0]
    check('卷大小字段', vol_space == len(data) // S,
          '(%d 扇区, 文件 %d 扇区)' % (vol_space, len(data) // S))
    check('逻辑块大小=2048', struct.unpack('<H', pvd[128:130])[0] == 2048)
    root_lba = struct.unpack('<I', pvd[158:162])[0]
    check('根目录记录存在', pvd[156] == 34, '(记录长 %d, 根目录 LBA=%d)' % (pvd[156], root_lba))

    # 2) Boot Record VD
    brvd = sec(17)
    print('\n[2] El Torito 引导记录 (Boot Record VD)')
    check('类型=0', brvd[0] == 0)
    check('标识 CD001', brvd[1:6] == b'CD001')
    check('EL TORITO SPECIFICATION', brvd[7:39].startswith(b'EL TORITO SPECIFICATION'),
          '(%r)' % brvd[7:39].rstrip(b'\x00'))
    cat_lba = struct.unpack('<I', brvd[71:75])[0]
    check('引导目录指针有效', 0 < cat_lba < len(data) // S, '(LBA=%d)' % cat_lba)

    # 3) 结束符
    print('\n[3] 卷描述符结束符')
    check('类型=255', sec(18)[0] == 255)

    # 4) 引导目录
    print('\n[4] 引导目录 (Boot Catalog @ LBA %d)' % cat_lba)
    cat = sec(cat_lba)
    check('校验项头标识=1', cat[0] == 1)
    check('平台 x86', cat[1] == 0)
    check('键字节 55 AA', cat[30] == 0x55 and cat[31] == 0xAA,
          '(%02x %02x)' % (cat[30], cat[31]))
    # 校验和定义：0..29 字节按 16 位小端求和后，对 65536 取模必须为 0
    total = sum(cat[i] | (cat[i + 1] << 8) for i in range(0, 30, 2))
    check('校验和归零', (total & 0xFFFF) == 0,
          '(16位和=%d, 取模后=%d)' % (total, total & 0xFFFF))

    e = 32
    media = cat[e + 1]
    media_name = {0x00: '无仿真', 0x01: '1.2MB软盘', 0x02: '1.44MB软盘',
                  0x03: '2.88MB软盘', 0x04: '硬盘仿真'}.get(media, '未知')
    check('默认引导项可引导 (0x88)', cat[e] == 0x88, '(实际 %02x)' % cat[e])
    check('媒体类型可识别', media in (0, 1, 2, 3, 4), '(%s)' % media_name)
    img_lba = struct.unpack('<I', cat[e + 8:e + 12])[0]
    check('引导镜像 RBA 有效', 0 < img_lba < len(data) // S, '(LBA=%d)' % img_lba)

    # 5) 引导镜像
    print('\n[5] 引导镜像内容 (RBA=%d)' % img_lba)
    img = sec(img_lba)
    check('含 0xAA55 引导签名', img[510:512] == b'\x55\xAA',
          '(%s)' % img[510:512].hex(' '))
    check('首字节非空(有代码)', img[0] != 0, '(%02x)' % img[0])
    if a.expect_boot:
        ref = open(a.expect_boot, 'rb').read()
        check('与汇编产物一致', img[:len(ref)] == ref)

    print('\n== 结论: %s ==' % ('结构合法，可进入引导测试' if ok else '存在问题，需修复'))
    return 0 if ok else 2


if __name__ == '__main__':
    sys.exit(main())
