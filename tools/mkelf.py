#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""把一段平铺机器码包成 ELF64 可执行文件（仅 PT_LOAD，无动态链接）。

为什么需要它：内核 M5 要从磁盘加载 ELF 来跑用户程序，而我们在 Windows 上
没有现成的 x86_64 链接器能直接产出一个"裸" ELF64 可执行文件（w64devkit 的 ld
只会产 PE/COFF）。所以走这条路：汇编 → objcopy -O binary 拿平铺机器码 →
本脚本补一个合法的 ELF64 头 + 一个 PT_LOAD 程序头。

硬约束：被包的代码必须是**位置无关**的（只用 RIP 相对寻址），
因为内核加载器会把段整体搬到自己的用户基址上（见 exec.rs 的说明）。
"""

import argparse
import struct
import sys

PT_LOAD = 1
PAGE = 0x1000


def build(code: bytes, base: int, bss: int, flags: int, entry_off: int,
          data_vaddr: int = 0, data_memsz: int = 0) -> bytes:
    phoff = 64
    phentsize = 56
    phnum = 2 if data_memsz > 0 else 1
    data_off = PAGE  # 代码放在第一个页边界，便于 p_offset % p_align == p_vaddr % p_align

    ehdr = bytearray(64)
    ehdr[0:4] = b"\x7fELF"
    ehdr[4] = 2          # EI_CLASS = 64 位
    ehdr[5] = 1          # EI_DATA = 小端
    ehdr[6] = 1          # EI_VERSION
    ehdr[7] = 0          # EI_OSABI = System V
    struct.pack_into("<H", ehdr, 16, 2)          # e_type = ET_EXEC
    struct.pack_into("<H", ehdr, 18, 62)         # e_machine = EM_X86_64
    struct.pack_into("<I", ehdr, 20, 1)          # e_version
    struct.pack_into("<Q", ehdr, 24, base + entry_off)   # e_entry
    struct.pack_into("<Q", ehdr, 32, phoff)      # e_phoff
    struct.pack_into("<Q", ehdr, 40, 0)          # e_shoff
    struct.pack_into("<I", ehdr, 48, 0)          # e_flags
    struct.pack_into("<H", ehdr, 52, 64)         # e_ehsize
    struct.pack_into("<H", ehdr, 54, phentsize)  # e_phentsize
    struct.pack_into("<H", ehdr, 56, phnum)      # e_phnum
    struct.pack_into("<H", ehdr, 58, 64)         # e_shentsize
    struct.pack_into("<H", ehdr, 60, 0)          # e_shnum
    struct.pack_into("<H", ehdr, 62, 0)          # e_shstrndx

    phdr = bytearray(phentsize * phnum)
    struct.pack_into("<I", phdr, 0, PT_LOAD)
    struct.pack_into("<I", phdr, 4, flags)       # 5=R|X, 7=R|W|X
    struct.pack_into("<Q", phdr, 8, data_off)
    struct.pack_into("<Q", phdr, 16, base)
    struct.pack_into("<Q", phdr, 24, base)
    struct.pack_into("<Q", phdr, 32, len(code))          # p_filesz
    struct.pack_into("<Q", phdr, 40, len(code) + bss)    # p_memsz（含 .bss）
    struct.pack_into("<Q", phdr, 48, PAGE)

    if phnum == 2:
        # 纯 .bss 段：文件里没有内容，只声明"这段要占多少内存"。
        # 用来给进程一块私有的可写数据页（每进程物理页不同 —— 这就是隔离的直接体现）。
        o = phentsize
        struct.pack_into("<I", phdr, o + 0, PT_LOAD)
        struct.pack_into("<I", phdr, o + 4, 6)       # R|W，不可执行
        struct.pack_into("<Q", phdr, o + 8, data_off)
        struct.pack_into("<Q", phdr, o + 16, data_vaddr)
        struct.pack_into("<Q", phdr, o + 24, data_vaddr)
        struct.pack_into("<Q", phdr, o + 32, 0)      # p_filesz = 0
        struct.pack_into("<Q", phdr, o + 40, data_memsz)
        struct.pack_into("<Q", phdr, o + 48, PAGE)

    out = bytearray(data_off + len(code))
    out[0:64] = ehdr
    out[phoff:phoff + phentsize * phnum] = phdr
    out[data_off:data_off + len(code)] = code
    return bytes(out)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--input", required=True, help="平铺机器码")
    ap.add_argument("--output", required=True, help="输出 ELF64")
    ap.add_argument("--base", default="0x400000", help="加载虚拟地址（默认 0x400000）")
    ap.add_argument("--bss", type=int, default=256, help="bss 字节数（只进 p_memsz）")
    ap.add_argument("--flags", type=int, default=7, help="段权限位 1=X 2=W 4=R")
    ap.add_argument("--entry-off", type=int, default=0, help="入口相对段首的偏移")
    ap.add_argument("--data-vaddr", default="0", help="额外的 .bss 段虚拟地址（0 = 不加）")
    ap.add_argument("--data-memsz", type=int, default=0, help="额外的 .bss 段大小")
    a = ap.parse_args()

    code = open(a.input, "rb").read()
    if not code:
        print("[错误] 输入为空")
        return 1
    if len(code) > PAGE - 64 - 56:
        print("[错误] 机器码超过一页，请扩展本脚本（当前只支持单页）")
        return 1
    phnum = 2 if a.data_memsz > 0 else 1
    if phnum == 2 and len(code) + 64 + 56 * phnum > PAGE:
        print("[错误] 代码 + 程序头超过第一页")
        return 1
    blob = build(code, int(a.base, 0), a.bss, a.flags, a.entry_off,
                 int(a.data_vaddr, 0), a.data_memsz)
    open(a.output, "wb").write(blob)
    print("[ELF] %s  code=%d B  vaddr=%s  entry=%#x  memsz=%d  segs=%d"
          % (a.output, len(code), a.base, int(a.base, 0) + a.entry_off,
             len(code) + a.bss, phnum))
    if a.data_memsz > 0:
        print("[ELF]   额外 .bss 段: vaddr=%s memsz=%d (R+W)"
              % (a.data_vaddr, a.data_memsz))
    return 0


if __name__ == "__main__":
    sys.exit(main())
