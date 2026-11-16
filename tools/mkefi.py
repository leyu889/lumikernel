#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""mkefi.py — 把内核塞进 UEFI 装载器的 PE 映像里（M10）

为什么用"追加 PE 段"而不是让装载器去读文件系统：
- 走文件系统就得依赖固件的 ISO9660/FAT 驱动与文件名大小写，多一层不可控；
- 追加段只改 PE 头，固件照样按正常 PE 加载，内核映像随 .efi 一起被映射好；
- 一次加载，没有任何运行时 IO —— 真机上少一个失败面。

怎么做：
1. 在装载器里找标记 `LUMIKRNL-BLOB-v1`（`BLOB_REF` 结构的前 16 字节）；
2. 把 kernel.bin 追加到文件尾，作为一个新的只读数据段 `.blob`；
3. 回填 `BLOB_REF.delta`（新段 RVA − 结构自身 RVA）与 `len`。

**只写相对偏移，不写绝对地址**：PE 映像由固件决定加载基址，绝对地址在
打包时根本不知道；相对距离才是编译期可定死的。
"""

import argparse
import struct
import sys

MAGIC = b'LUMIKRNL-BLOB-v1'
SCN_INIT_DATA = 0x40000040  # CNT_INITIALIZED_DATA | MEM_READ


def align_up(v, a):
    return (v + a - 1) // a * a


def find_sections(d, pe, nsec, optsz):
    sec = pe + 24 + optsz
    out = []
    for i in range(nsec):
        o = sec + i * 40
        name = d[o:o + 8].rstrip(b'\x00').decode('ascii', 'replace')
        vs, va, rs, rp = struct.unpack_from('<IIII', d, o + 8)
        out.append({'off': o, 'name': name, 'vs': vs, 'va': va, 'rs': rs, 'rp': rp})
    return out


def rva_of(sections, file_off):
    for s in sections:
        if s['rs'] and s['rp'] <= file_off < s['rp'] + s['rs']:
            return s['va'] + (file_off - s['rp'])
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--loader', required=True, help='lumi-efi.efi')
    ap.add_argument('--kernel', required=True, help='kernel.bin（平铺映像）')
    ap.add_argument('--output', required=True)
    a = ap.parse_args()

    d = bytearray(open(a.loader, 'rb').read())
    k = open(a.kernel, 'rb').read()
    if not k:
        print('[错误] kernel.bin 为空')
        return 1

    pe = struct.unpack_from('<I', d, 0x3c)[0]
    if d[pe:pe + 4] != b'PE\x00\x00':
        print('[错误] 不是 PE 文件')
        return 1
    machine, nsec, _tds, p_sym, n_sym, optsz, _chars = struct.unpack_from('<HHIIIHH', d, pe + 4)
    if machine != 0x8664:
        print('[错误] 不是 x86_64 PE')
        return 1
    opt = pe + 24
    if struct.unpack_from('<H', d, opt)[0] != 0x20b:
        print('[错误] 不是 PE32+（EFI 需要 PE32+）')
        return 1
    secalign, filealign = struct.unpack_from('<II', d, opt + 32)
    size_of_image = struct.unpack_from('<I', d, opt + 56)[0]
    hdr_size = struct.unpack_from('<I', d, opt + 60)[0]

    sections = find_sections(d, pe, nsec, optsz)

    # ---- 1. 找 BLOB_REF ----
    at = d.find(MAGIC)
    if at < 0:
        print('[错误] 装载器里找不到内核标记 %r' % MAGIC)
        return 1
    if d.find(MAGIC, at + 1) >= 0:
        print('[错误] 标记不唯一，无法确定要回填哪一处')
        return 1
    ref_rva = rva_of(sections, at)
    if ref_rva is None:
        print('[错误] 标记不在任何有文件内容的段里（段 raw=0 时无法回填）')
        return 1
    ref_end = at + 16 + 8 + 8
    for s in sections:
        if s['rp'] <= at < s['rp'] + s['rs']:
            if ref_end > s['rp'] + s['rs']:
                print('[错误] BLOB_REF 跨段了，打包器不支持')
                return 1
            break

    # ---- 2. 追加 .blob 段 ----
    sec_tbl_end = pe + 24 + optsz + nsec * 40
    if sec_tbl_end + 40 > hdr_size:
        print('[错误] PE 头没地方放新段表项（需要 %d，头只有 %d）'
              % (sec_tbl_end + 40, hdr_size))
        return 1

    blob_va = align_up(size_of_image, secalign)
    raw_ptr = align_up(len(d), filealign)
    raw_size = align_up(len(k), filealign)

    if len(d) < raw_ptr:
        d += b'\x00' * (raw_ptr - len(d))
    d += k
    if len(d) < raw_ptr + raw_size:
        d += b'\x00' * (raw_ptr + raw_size - len(d))

    o = sec_tbl_end
    d[o:o + 8] = b'.blob\x00\x00\x00'
    struct.pack_into('<IIII', d, o + 8, len(k), blob_va, raw_size, raw_ptr)
    struct.pack_into('<II', d, o + 24, 0, 0)          # 无重定位/行号
    struct.pack_into('<I', d, o + 36, SCN_INIT_DATA)  # Characteristics

    struct.pack_into('<H', d, pe + 6, nsec + 1)
    new_size = align_up(blob_va + len(k), secalign)
    struct.pack_into('<I', d, opt + 56, new_size)

    # ---- 3. 回填 delta / len ----
    delta = blob_va - ref_rva
    struct.pack_into('<Q', d, at + 16, delta)
    struct.pack_into('<Q', d, at + 24, len(k))

    # 符号表偏移失效（我们改了节表），把 COFF 符号/重定位清零最省事
    if n_sym == 0 and p_sym:
        struct.pack_into('<I', d, pe + 12, 0)

    open(a.output, 'wb').write(bytes(d))
    print('[efi] %-22s <- %s (%d B)  blob_rva=%#x delta=%#x' %
          (a.output.split('/')[-1].split('\\')[-1], a.kernel.split('/')[-1].split('\\')[-1],
           len(k), blob_va, delta))
    print('[efi] 段数 %d -> %d，SizeOfImage %#x' % (nsec, nsec + 1, new_size))
    return 0


if __name__ == '__main__':
    sys.exit(main())
