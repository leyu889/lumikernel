#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
xasm16.py — 自研极简 x86 汇编器（16 / 32 位 · LumiOS 内核引导链专用 · 零依赖）

背景：本机无网络、无任何现成编译器/汇编器（gcc/nasm/rustc 全缺），
因此自研这一枚汇编器，作为 LumiOS 引导层（boot sector / stage2 / 保护模式引导）的构建工具。
只实现引导链真正用得到的指令子集，够用、可控、可读。

用法:
    python xasm16.py <input.asm> <output.bin>

语法要点:
    org 0x7C00        ; 起始地址（影响 $ 与标签）
    bits 16 / bits 32 ; 切换代码位宽（可同一文件内切换）
    label:            ; 标签
    mov ax, 0x0013
    int 0x10
    mov dword [0xC000], 0x0000D003   ; 32 位绝对寻址
    mov [edi], eax                   ; 32 位寄存器间接
    lgdt [gdtr]
    jmp dword 0x08:pm_entry          ; 16 位态下跳入 32 位代码段
    jmp 0x18:0x100000                ; 32 位态下跳入 64 位代码段
    times 510-($-$$) db 0
    dw 0xAA55
    16 位内存操作数: [0x1234] [bx] [si] [di] [bx+si] [bx+di] [bp+si] [bp+di]
    32 位内存操作数: [0x1234] [eax] [edi+8] [ebx-4]
    段前缀:          es:[di]  ds:[bx]  fs:[eax] ...
"""
import struct
import sys
import re


class AsmError(Exception):
    pass


REG16 = {'ax': 0, 'cx': 1, 'dx': 2, 'bx': 3, 'sp': 4, 'bp': 5, 'si': 6, 'di': 7}
REG32 = {'eax': 0, 'ecx': 1, 'edx': 2, 'ebx': 3, 'esp': 4, 'ebp': 5, 'esi': 6, 'edi': 7}
REG8 = {'al': 0, 'cl': 1, 'dl': 2, 'bl': 3, 'ah': 4, 'ch': 5, 'dh': 6, 'bh': 7}
SREG = {'es': 0, 'cs': 1, 'ss': 2, 'ds': 3, 'fs': 4, 'gs': 5}
SEGOV = {'es': 0x26, 'cs': 0x2E, 'ss': 0x36, 'ds': 0x3E, 'fs': 0x64, 'gs': 0x65}
CREG = {'cr0': 0, 'cr2': 2, 'cr3': 3, 'cr4': 4, 'cr8': 8}
DREG = {'dr0': 0, 'dr1': 1, 'dr2': 2, 'dr3': 3, 'dr6': 6, 'dr7': 7}

# mod=00 的 16 位内存寻址 rm 码（注意：rm=6 在 mod=00 时表示 disp16 绝对地址）
MEM_RM = {
    '[bx+si]': 0, '[bx+di]': 1, '[bp+si]': 2, '[bp+di]': 3,
    '[si]': 4, '[di]': 5, '[bp]': 6, '[bx]': 7,
}

ALU_DIGIT = {'add': 0, 'or': 1, 'adc': 2, 'sbb': 3, 'and': 4, 'sub': 5, 'xor': 6, 'cmp': 7}
ALU_RM_R = {'add': 0x01, 'or': 0x09, 'adc': 0x11, 'sbb': 0x19,
            'and': 0x21, 'sub': 0x29, 'xor': 0x31, 'cmp': 0x39}
ALU_R_RM = {'add': 0x03, 'or': 0x0B, 'adc': 0x13, 'sbb': 0x1B,
            'and': 0x23, 'sub': 0x2B, 'xor': 0x33, 'cmp': 0x3B}
ALU_RM_R8 = {'add': 0x00, 'or': 0x08, 'adc': 0x10, 'sbb': 0x18,
             'and': 0x20, 'sub': 0x28, 'xor': 0x30, 'cmp': 0x38}
ALU_R_RM8 = {'add': 0x02, 'or': 0x0A, 'adc': 0x12, 'sbb': 0x1A,
             'and': 0x22, 'sub': 0x2A, 'xor': 0x32, 'cmp': 0x3A}
ALU_AX_I = {'add': 0x05, 'or': 0x0D, 'adc': 0x15, 'sbb': 0x1D,
            'and': 0x25, 'sub': 0x2D, 'xor': 0x35, 'cmp': 0x3D}
ALU_AL_I = {'add': 0x04, 'or': 0x0C, 'adc': 0x14, 'sbb': 0x1C,
            'and': 0x24, 'sub': 0x2C, 'xor': 0x34, 'cmp': 0x3C}

CC = {'o': 0x0, 'no': 0x1, 'b': 0x2, 'c': 0x2, 'nae': 0x2, 'ae': 0x3, 'nb': 0x3,
      'nc': 0x3, 'e': 0x4, 'z': 0x4, 'ne': 0x5, 'nz': 0x5, 'be': 0x6, 'na': 0x6,
      'a': 0x7, 'nbe': 0x7, 's': 0x8, 'ns': 0x9, 'p': 0xA, 'pe': 0xA,
      'np': 0xB, 'po': 0xB, 'l': 0xC, 'nge': 0xC, 'ge': 0xD, 'nl': 0xD,
      'le': 0xE, 'ng': 0xE, 'g': 0xF, 'nle': 0xF}

SIMPLE = {'cli': 0xFA, 'sti': 0xFB, 'hlt': 0xF4, 'nop': 0x90, 'cld': 0xFC,
          'std': 0xFD, 'ret': 0xC3, 'iret': 0xCF, 'int3': 0xCC, 'into': 0xCE,
          'pushf': 0x9C, 'popf': 0x9D, 'movsb': 0xA4, 'stosb': 0xAA,
          'lodsb': 0xAC, 'cbw': 0x98, 'cwd': 0x99, 'leave': 0xC9,
          'rdmsr': 0x32, 'wrmsr': 0x30, 'rdtsc': 0x31, 'cpuid': 0xA2,
          'invd': 0x08, 'wbinvd': 0x09}
TWO_BYTE_SIMPLE = {'rdmsr', 'wrmsr', 'rdtsc', 'cpuid', 'invd', 'wbinvd'}


def parse_int(tok):
    t = tok.strip()
    neg = False
    if t.startswith('-'):
        neg = True
        t = t[1:].strip()
    elif t.startswith('+'):
        t = t[1:].strip()
    if len(t) >= 3 and t[0] == "'" and t[-1] == "'":
        v = ord(t[1:-1])
    elif t.lower().startswith('0x'):
        v = int(t, 16)
    elif t.lower().startswith('0b'):
        v = int(t, 2)
    elif re.fullmatch(r'[0-9A-Fa-f]+h', t):
        v = int(t[:-1], 16)
    else:
        v = int(t, 10)
    return -v if neg else v


class Operand:
    """kind: reg8 / reg16 / reg32 / sreg / creg / dreg / imm / mem / mem32 / memabs32

    mem     —— 16 位寻址：val = rm 码，disp = 绝对地址（None 表示寄存器间接）
    mem32   —— 32/64 位寄存器寻址：val = rm 码，disp = 位移，disp32 = 位移宽度
    memabs32—— 32/64 位绝对寻址：disp = 绝对地址
    """
    __slots__ = ('kind', 'val', 'seg', 'disp', 'disp32')

    def __init__(self, kind, val=None, seg=None, disp=None, disp32=False):
        self.kind = kind
        self.val = val
        self.seg = seg          # 段前缀字节 或 None
        self.disp = disp
        self.disp32 = disp32


MEM_KINDS = ('mem', 'mem32', 'memabs32', 'mem16abs')


class Assembler:
    def __init__(self):
        self.labels = {}
        self.org = 0
        self.bits = 16
        self.dflt = 2           # 当前位宽下的默认操作数宽度（字节）
        self.out = bytearray()
        self.fixups = []        # (offset_in_out, kind, label, addend)
        self._pfx = b''
        self._opsz = None

    # ---------- 工具 ----------
    def here(self):
        return self.org + len(self.out)

    def emit(self, *bs):
        for b in bs:
            if isinstance(b, (bytes, bytearray)):
                self.out.extend(b)
            else:
                self.out.append(b & 0xFF)

    def emit16(self, v):
        self.out.append(v & 0xFF)
        self.out.append((v >> 8) & 0xFF)

    def emit32(self, v):
        for i in range(4):
            self.out.append((v >> (8 * i)) & 0xFF)

    def modrm(self, mod, reg, rm):
        return ((mod & 3) << 6) | ((reg & 7) << 3) | (rm & 7)

    def szpfx(self, w):
        """操作数宽度与当前默认宽度不一致时，需要 0x66 前缀"""
        if w and w != self.dflt and w in (2, 4):
            return b'\x66'
        return b''

    # ---------- 操作数解析 ----------
    def parse_operand(self, s):
        s = s.strip()
        seg = None
        m = re.match(r'^(es|cs|ss|ds|fs|gs)\s*:\s*(.+)$', s, re.I)
        if m:
            seg = SEGOV[m.group(1).lower()]
            s = m.group(2).strip()
        low = s.lower()

        if low in REG32:
            return Operand('reg32', REG32[low])
        if low in REG16:
            return Operand('reg16', REG16[low])
        if low in REG8:
            return Operand('reg8', REG8[low])
        if low in CREG:
            return Operand('creg', CREG[low])
        if low in DREG:
            return Operand('dreg', DREG[low])
        if low in SREG:
            return Operand('sreg', SREG[low])

        if s.startswith('[') and s.endswith(']'):
            raw = s[1:-1].strip()
            if self.bits >= 32:
                return self.parse_mem32(raw, seg)
            return self.parse_mem16(raw, seg)

        # 立即数 / 表达式
        return Operand('imm', self.eval_expr(s))

    # 16 位寻址的合法基址组合（长的排前面，保证贪婪匹配到 bx+si 而不是 bx）
    MEM16_BASES = ('bx+si', 'bx+di', 'bp+si', 'bp+di', 'bx', 'bp', 'si', 'di')

    def parse_mem16(self, raw, seg):
        """解析 16 位内存操作数：[bx+si] / [di+4] / [bp-8] / [0x1234]"""
        s = raw.lower().replace(' ', '')
        for base in self.MEM16_BASES:
            if s == base:
                return Operand('mem', MEM_RM['[' + base + ']'], seg, None)
            if s.startswith(base):
                rest = s[len(base):]
                if rest[:1] in ('+', '-'):
                    disp = self.eval_expr(rest)
                    return Operand('mem', MEM_RM['[' + base + ']'], seg, disp)
        # 绝对地址 [disp16]：与 [bp+disp] 共用 rm=6，
        # 靠 mod 位区分（mod=00 → disp16 绝对；mod=01/10 → bp 基址）。
        # 因此必须用独立的 kind 标记，否则 [bp-8] 会被错编成绝对地址。
        return Operand('mem16abs', 6, seg, self.eval_expr(s))

    def parse_mem32(self, raw, seg):
        """32 位寻址：[disp32] / [reg32] / [reg32 ± disp]"""
        t = raw.lower().replace(' ', '')
        m = re.fullmatch(r'([a-z0-9_$]+)([+-].+)?', t)
        if m and m.group(1) in REG32:
            reg = REG32[m.group(1)]
            disp = None
            if m.group(2):
                disp = self.eval_expr(m.group(2))
            wide = disp is not None and not (-128 <= disp <= 127)
            return Operand('mem32', reg, seg, disp, wide)
        # 绝对地址（含标签）
        return Operand('memabs32', None, seg, self.eval_expr(raw))

    def eval_expr(self, expr):
        expr = expr.strip()
        # 1) 先消掉十六进制 / 字符字面量，避免被标识符正则吞掉
        tmp = re.sub(r"0[xX][0-9A-Fa-f]+|'[^']'", lambda m: str(parse_int(m.group(0))), expr)
        tmp = re.sub(r"\b([0-9][0-9A-Fa-f]*)h\b", lambda m: str(int(m.group(1), 16)), tmp)
        # 2) 位置计数器：$$ 必须先于 $ 替换（否则 $$ 会被当成两个 $）
        tmp = tmp.replace('$$', str(self.org))
        tmp = tmp.replace('$', str(self.here()))

        # 3) 最后替换标签名
        def repl(mm):
            name = mm.group(0)
            if name in self.labels:
                return str(self.labels[name])
            # 第一遍（dry）时前向标签尚未定义，按 0 处理（不影响指令长度）
            if getattr(self, 'dry', False):
                return '0'
            raise AsmError('未定义符号: %s' % name)

        tmp = re.sub(r"[A-Za-z_][A-Za-z0-9_]*", repl, tmp)
        tmp = tmp.replace('/', '//')
        try:
            return int(eval(tmp, {'__builtins__': {}}, {}))
        except AsmError:
            raise
        except Exception as e:
            raise AsmError('无法求值的表达式: %r (%s)' % (expr, e))

    # ---------- 指令编码 ----------
    @staticmethod
    def seg_of(op):
        """操作数的段前缀字节。x86 规定段前缀必须位于操作码之前，故单独取出。"""
        if op.kind in MEM_KINDS and op.seg:
            return bytes([op.seg])
        return b''

    def enc_mem(self, op, regfield):
        """只返回 ModRM(+位移)，段前缀与 0x66 交由调用方最先发出"""
        if op.kind == 'memabs32':
            return bytes([self.modrm(0, regfield, 5)]) + struct.pack('<I', op.disp & 0xFFFFFFFF)
        if op.kind == 'mem32':
            rm = op.val
            if rm == 4:
                raise AsmError('[esp] 需要 SIB 字节，暂不支持')
            if op.disp is None:
                if rm == 5:                      # [ebp] 必须带 disp8=0
                    return bytes([self.modrm(1, regfield, 5), 0])
                return bytes([self.modrm(0, regfield, rm)])
            if op.disp32:
                return bytes([self.modrm(2, regfield, rm)]) + struct.pack('<I', op.disp & 0xFFFFFFFF)
            return bytes([self.modrm(1, regfield, rm), op.disp & 0xFF])
        # 16 位寻址：绝对地址 [disp16]
        if op.kind == 'mem16abs':
            return bytes([self.modrm(0, regfield, 6),
                          op.disp & 0xFF, (op.disp >> 8) & 0xFF])
        # 16 位寻址：基址寄存器 [bx+si] / [bp+disp] ...
        if op.disp is None:
            if op.val == 6:                      # [bp] 必须写成 mod=01 disp8=0
                return bytes([self.modrm(1, regfield, 6), 0])
            return bytes([self.modrm(0, regfield, op.val)])
        if -128 <= op.disp <= 127:
            return bytes([self.modrm(1, regfield, op.val), op.disp & 0xFF])
        return bytes([self.modrm(2, regfield, op.val),
                      op.disp & 0xFF, (op.disp >> 8) & 0xFF])

    def is_rm(self, op):
        return op.kind in ('reg16', 'reg32', 'reg8') or op.kind in MEM_KINDS

    def rm_modrm(self, op, regfield):
        if op.kind in MEM_KINDS:
            return self.enc_mem(op, regfield)
        if op.kind in ('reg16', 'reg32', 'reg8'):
            return bytes([self.modrm(3, regfield, op.val)])
        raise AsmError('非 r/m 操作数')

    def emit_rm(self, opcode, regfield, rmop, tail=b''):
        """按正确顺序发出：0x66 + 段前缀 + 操作码 + ModRM + 尾部立即数"""
        self.emit(self._pfx, self.seg_of(rmop), opcode,
                  self.rm_modrm(rmop, regfield), tail)

    # ----- 操作数宽度推断 -----
    def _width_of(self, *ops):
        w = self._opsz
        for op in ops:
            if op is None:
                continue
            if op.kind == 'reg16' or op.kind == 'sreg':
                w = 2
            elif op.kind == 'reg32' or op.kind in ('creg', 'dreg'):
                w = 4
        return w

    def _imm_bytes(self, val, w):
        if w == 1:
            return bytes([val & 0xFF])
        if w == 2:
            return bytes([val & 0xFF, (val >> 8) & 0xFF])
        return struct.pack('<I', val & 0xFFFFFFFF)

    # ----- mov -----
    def do_mov(self, dst, src):
        self._pfx = self.szpfx(self._width_of(dst, src))

        # 控制/调试寄存器（16/32 位态下通用寄存器编码一致，允许 ax/eax）
        GPR = ('reg16', 'reg32')
        if dst.kind == 'creg':
            if src.kind not in GPR:
                raise AsmError('mov crN, r 需要通用寄存器')
            self.emit(0x0F, 0x22, self.modrm(3, dst.val & 7, src.val))
            return
        if src.kind == 'creg':
            if dst.kind not in GPR:
                raise AsmError('mov r, crN 需要通用寄存器')
            self.emit(0x0F, 0x20, self.modrm(3, src.val & 7, dst.val))
            return
        if dst.kind == 'dreg':
            if src.kind not in GPR:
                raise AsmError('mov drN, r 需要通用寄存器')
            self.emit(0x0F, 0x23, self.modrm(3, dst.val & 7, src.val))
            return
        if src.kind == 'dreg':
            if dst.kind not in GPR:
                raise AsmError('mov r, drN 需要通用寄存器')
            self.emit(0x0F, 0x21, self.modrm(3, src.val & 7, dst.val))
            return

        if dst.kind == 'sreg':
            self._pfx = b''          # MOV Sreg 的操作数恒定 16 位，不需要 0x66
            if self.is_rm(src) or src.kind in ('reg16', 'reg32'):
                self.emit_rm(0x8E, dst.val, src)
            else:
                raise AsmError('mov sreg, ? 不支持')
            return
        if dst.kind in ('reg16', 'reg32') and src.kind == 'sreg':
            self._pfx = b''
            self.emit_rm(0x8C, src.val, dst)
            return

        if dst.kind in ('reg16', 'reg32') and src.kind == 'imm':
            self.emit(self._pfx, 0xB8 + dst.val)
            self.emit(self._imm_bytes(src.val, 4 if dst.kind == 'reg32' else 2))
            return
        if dst.kind == 'reg8' and src.kind == 'imm':
            self.emit(0xB0 + dst.val)
            self.emit(src.val)
            return
        if dst.kind in ('reg16', 'reg32') or dst.kind in MEM_KINDS:
            if src.kind in ('reg16', 'reg32'):
                self.emit_rm(0x89, src.val, dst)
                return
        if dst.kind in ('reg16', 'reg32'):
            if src.kind in MEM_KINDS:
                self.emit_rm(0x8B, dst.val, src)
                return
        if dst.kind in ('reg8',) + MEM_KINDS and src.kind == 'reg8':
            self.emit_rm(0x88, src.val, dst)
            return
        if dst.kind == 'reg8' and src.kind in MEM_KINDS:
            self.emit_rm(0x8A, dst.val, src)
            return
        if dst.kind in MEM_KINDS and src.kind == 'imm':
            # 依据尺寸修饰选择宽度（默认 byte；mov word / mov dword 显式指定）
            sz = self._opsz or 1
            if sz == 1:
                self.emit_rm(0xC6, 0, dst, bytes([src.val & 0xFF]))
            elif sz == 2:
                self.emit_rm(0xC7, 0, dst, bytes([src.val & 0xFF, (src.val >> 8) & 0xFF]))
            else:
                self.emit_rm(0xC7, 0, dst, struct.pack('<I', src.val & 0xFFFFFFFF))
            return
        raise AsmError('不支持的 mov 组合')

    # ----- alu -----
    def do_alu(self, name, dst, src):
        digit = ALU_DIGIT[name]
        if dst.kind == 'imm':
            raise AsmError('%s 的目标不能是立即数' % name)
        w = self._width_of(dst, src)
        self._pfx = self.szpfx(w)
        is32 = (w == 4)

        # 立即数形式
        if src.kind == 'imm':
            if dst.kind in ('reg16', 'reg32'):
                if dst.val == 0:
                    self.emit(self._pfx, ALU_AX_I[name])
                    self.emit(self._imm_bytes(src.val, 4 if is32 else 2))
                else:
                    self.emit(self._pfx, 0x81)
                    self.emit(self.modrm(3, digit, dst.val))
                    self.emit(self._imm_bytes(src.val, 4 if is32 else 2))
                return
            if dst.kind == 'reg8':
                if dst.val == 0:
                    self.emit(ALU_AL_I[name])
                    self.emit(src.val)
                else:
                    self.emit(0x80)
                    self.emit(self.modrm(3, digit, dst.val))
                    self.emit(src.val)
                return
            if dst.kind in MEM_KINDS:
                sz = self._opsz or 1
                if sz == 1:
                    self.emit_rm(0x80, digit, dst, bytes([src.val & 0xFF]))
                else:
                    self.emit_rm(0x81, digit, dst, self._imm_bytes(src.val, sz))
                return
        # 寄存器/内存形式
        if (dst.kind in ('reg16', 'reg32') or dst.kind in MEM_KINDS) \
                and src.kind in ('reg16', 'reg32'):
            self.emit_rm(ALU_RM_R[name], src.val, dst)
            return
        if dst.kind in ('reg16', 'reg32') and src.kind in MEM_KINDS:
            self.emit_rm(ALU_R_RM[name], dst.val, src)
            return
        if (dst.kind in ('reg8', 'mem')) and src.kind == 'reg8':
            self.emit_rm(ALU_RM_R8[name], src.val, dst)
            return
        if dst.kind == 'reg8' and src.kind == 'mem':
            self.emit_rm(ALU_R_RM8[name], dst.val, src)
            return
        raise AsmError('不支持的 %s 组合' % name)

    # ----- test（只影响标志位，不写回结果）-----
    def do_test(self, dst, src):
        if dst.kind == 'imm':
            raise AsmError('test 的目标不能是立即数')
        w = self._width_of(dst, src)
        self._pfx = self.szpfx(w)
        is32 = (w == 4)

        if src.kind == 'imm':
            if dst.kind == 'reg8':
                self.emit(0xF6)
                self.emit(self.modrm(3, 0, dst.val))
                self.emit(src.val)
                return
            if dst.kind in ('reg16', 'reg32'):
                self.emit(self._pfx, 0xF7)
                self.emit(self.modrm(3, 0, dst.val))
                self.emit(self._imm_bytes(src.val, 4 if is32 else 2))
                return
            if dst.kind in MEM_KINDS:
                sz = self._opsz or (4 if is32 else 2)
                if sz == 1:
                    self.emit_rm(0xF6, 0, dst, bytes([src.val & 0xFF]))
                else:
                    self.emit_rm(0xF7, 0, dst, self._imm_bytes(src.val, sz))
                return
            raise AsmError('test 不支持的立即数组合')

        # 寄存器形式
        if dst.kind in ('reg8', 'mem') and src.kind == 'reg8':
            self.emit_rm(0x84, src.val, dst)
            return
        if dst.kind == 'reg8' and src.kind in MEM_KINDS:
            self.emit_rm(0x84, dst.val, src)
            return
        if (dst.kind in ('reg16', 'reg32') or dst.kind in MEM_KINDS) \
                and src.kind in ('reg16', 'reg32'):
            self.emit_rm(0x85, src.val, dst)
            return
        if dst.kind in ('reg16', 'reg32') and src.kind in MEM_KINDS:
            self.emit_rm(0x85, dst.val, src)
            return
        raise AsmError('不支持的 test 组合')

    # ----- 主入口 -----
    def assemble(self, text):
        lines = text.splitlines()
        # 第一遍：确定标签地址与 org
        self._pass(lines, dry=True)
        # 第二遍：真正生成
        org_saved = self.org
        bits_saved = self.bits
        self.out = bytearray()
        self.org = org_saved
        self.bits = bits_saved
        self._pass(lines, dry=False)
        # 回填
        for off, kind, label, addend in self.fixups:
            if label not in self.labels:
                raise AsmError('未定义标签: %s' % label)
            target = self.labels[label] + addend
            if kind == 'rel8':
                base = self.org + off + 1
                rel = target - base
                if not (-128 <= rel <= 127):
                    raise AsmError('rel8 跳转越界: %s (%d)' % (label, rel))
                self.out[off] = rel & 0xFF
            elif kind == 'rel16':
                base = self.org + off + 2
                rel = target - base
                if not (-32768 <= rel <= 32767):
                    raise AsmError('rel16 跳转越界: %s (%d)' % (label, rel))
                self.out[off] = rel & 0xFF
                self.out[off + 1] = (rel >> 8) & 0xFF
            elif kind == 'rel32':
                base = self.org + off + 4
                rel = target - base
                if not (-0x80000000 <= rel <= 0x7FFFFFFF):
                    raise AsmError('rel32 跳转越界: %s (%d)' % (label, rel))
                self.out[off:off + 4] = struct.pack('<i', rel)
            else:
                raise AsmError('未知回填类型')
        return bytes(self.out)

    def _pass(self, lines, dry):
        self.out = bytearray()
        self.fixups = []
        self.dry = dry
        if dry:
            self.labels = {}
        for lineno, raw in enumerate(lines, 1):
            line = raw.split(';')[0].rstrip()
            if not line.strip():
                continue
            # 标签
            while True:
                m = re.match(r'^([A-Za-z_.$][A-Za-z0-9_.$]*)\s*:\s*(.*)$', line)
                if not m:
                    break
                name = m.group(1)
                self.labels[name] = self.here()
                line = m.group(2).strip()
                if not line:
                    break
            if not line:
                continue
            try:
                self._one(line, dry)
            except AsmError as e:
                raise AsmError('第 %d 行: %s  |  %s' % (lineno, e, raw.strip()))

    def _one(self, line, dry):
        prefix = b''
        word = False
        self._opsz = None
        toks = line.split(None, 1)
        mnem = toks[0].lower()
        rest = toks[1] if len(toks) > 1 else ''

        # rep / word / short / near 修饰
        while True:
            mnem2 = mnem
            if mnem2 in ('rep', 'repe', 'repz'):
                prefix += b'\xF3'
            elif mnem2 in ('repne', 'repnz'):
                prefix += b'\xF2'
            elif mnem2 == 'word':
                word = True
            else:
                break
            toks2 = rest.split(None, 1)
            mnem = toks2[0].lower() if toks2 else ''
            rest = toks2[1] if len(toks2) > 1 else ''
            if not mnem:
                break

        # Intel 语法的尺寸修饰位于助记符之后（mov byte [di], 200 / mov dword [0x3000], 0x83）
        while True:
            msz = re.match(r'^(byte|word|dword)\s+(.*)$', rest, re.I)
            if not msz:
                break
            self._opsz = {'byte': 1, 'word': 2, 'dword': 4}[msz.group(1).lower()]
            rest = msz.group(2).strip()

        if word and self._opsz is None:
            self._opsz = 2
        self._word = (self._opsz == 2)
        self._pfx = self.szpfx(self._opsz)

        # 伪指令
        if mnem in ('org',):
            self.org = self.eval_expr(rest)
            return
        if mnem in ('bits',):
            self.bits = int(rest)
            self.dflt = 2 if self.bits == 16 else 4
            return
        if mnem in ('db', 'dw', 'dd'):
            unit = {'db': 1, 'dw': 2, 'dd': 4}[mnem]
            for part in self._split_args(rest):
                p = part.strip()
                if p[:1] in ('"', "'"):
                    for b in self._str_bytes(p):
                        self.emit(b)
                else:
                    v = self.eval_expr(part)
                    if unit == 1:
                        self.emit(v)
                    elif unit == 2:
                        self.emit16(v)
                    else:
                        self.emit32(v)
            return
        if mnem == 'times':
            m = re.match(r'^(.+?)\s+(db|dw|dd)\s+(.+)$', rest)
            if not m:
                raise AsmError('times 语法错误')
            count = self.eval_expr(m.group(1))
            unit = {'db': 1, 'dw': 2, 'dd': 4}[m.group(2)]
            arg = m.group(3)
            for _ in range(count):
                p = arg.strip()
                if p[:1] in ('"', "'"):
                    for b in self._str_bytes(p):
                        self.emit(b)
                else:
                    v = self.eval_expr(arg)
                    if unit == 1:
                        self.emit(v)
                    elif unit == 2:
                        self.emit16(v)
                    else:
                        self.emit32(v)
            return

        # 简单指令
        if mnem in SIMPLE and not rest.strip():
            self.emit(prefix)
            if mnem in TWO_BYTE_SIMPLE:
                self.emit(0x0F)
            self.emit(SIMPLE[mnem])
            return

        args = [a for a in self._split_args(rest)] if rest.strip() else []

        # int
        if mnem == 'int':
            self.emit(prefix)
            self.emit(0xCD)
            self.emit(self.eval_expr(args[0]))
            return

        # jmp / call / jcc / loop
        if mnem == 'jmp':
            target = args[0].strip()
            if ':' in target:                       # 远跳转 seg:off（进代码段）
                seg_s, off_s = target.split(':', 1)
                seg_v = self.eval_expr(seg_s)
                off_v = self.eval_expr(off_s)
                if self._opsz == 4 or self.bits >= 32:
                    self.emit(prefix, b'\x66' if self.bits == 16 else b'', 0xEA)
                    self.emit32(off_v)
                else:
                    self.emit(prefix, 0xEA)
                    self.emit16(off_v)
                self.emit16(seg_v)
                return
            short = False
            if target.lower().startswith('short'):
                short = True
                target = target[5:].strip()
            elif target.lower().startswith('near'):
                target = target[4:].strip()
            if short:
                self.emit(prefix, 0xEB)
                self.fixups.append((len(self.out), 'rel8', target, 0))
                self.emit(0)
            elif self.bits >= 32:
                self.emit(prefix, 0xE9)
                self.fixups.append((len(self.out), 'rel32', target, 0))
                self.emit32(0)
            else:
                self.emit(prefix, 0xE9)
                self.fixups.append((len(self.out), 'rel16', target, 0))
                self.emit16(0)
            return
        if mnem == 'call':
            target = args[0].strip()
            if self.bits >= 32:
                self.emit(prefix, 0xE8)
                self.fixups.append((len(self.out), 'rel32', target, 0))
                self.emit32(0)
            else:
                self.emit(prefix, 0xE8)
                self.fixups.append((len(self.out), 'rel16', target, 0))
                self.emit16(0)
            return
        if mnem in ('loop', 'loope', 'loopne', 'jcxz', 'jecxz'):
            opc = {'loop': 0xE2, 'loope': 0xE1, 'loopne': 0xE0,
                   'jcxz': 0xE3, 'jecxz': 0xE3}[mnem]
            if mnem == 'jecxz' and self.bits == 16:
                prefix += b'\x67'
            self.emit(prefix, opc)
            self.fixups.append((len(self.out), 'rel8', args[0].strip(), 0))
            self.emit(0)
            return
        if mnem.startswith('j') and mnem[1:] in CC:
            cc = CC[mnem[1:]]
            target = args[0].strip()
            near = False
            if target.lower().startswith('near'):
                near = True
                target = target[4:].strip()
            if near and self.bits >= 32:
                self.emit(prefix, 0x0F, 0x80 + cc)
                self.fixups.append((len(self.out), 'rel32', target, 0))
                self.emit32(0)
            elif near:
                self.emit(prefix, 0x0F, 0x80 + cc)
                self.fixups.append((len(self.out), 'rel16', target, 0))
                self.emit16(0)
            else:
                self.emit(prefix, 0x70 + cc)
                self.fixups.append((len(self.out), 'rel8', target, 0))
                self.emit(0)
            return

        # mov
        if mnem == 'mov':
            self.emit(prefix)
            d = self.parse_operand(args[0])
            s = self.parse_operand(args[1])
            self.do_mov(d, s)
            return

        # test（只置标志）
        if mnem == 'test':
            self.emit(prefix)
            d = self.parse_operand(args[0])
            s = self.parse_operand(args[1])
            self.do_test(d, s)
            return

        # alu
        if mnem in ALU_DIGIT:
            self.emit(prefix)
            d = self.parse_operand(args[0])
            s = self.parse_operand(args[1])
            self.do_alu(mnem, d, s)
            return

        # lgdt / lidt
        if mnem in ('lgdt', 'lidt'):
            op = self.parse_operand(args[0])
            if op.kind not in MEM_KINDS:
                raise AsmError('%s 需要内存操作数' % mnem)
            self.emit(prefix, self._pfx, self.seg_of(op), 0x0F, 0x01,
                      self.enc_mem(op, 2 if mnem == 'lgdt' else 3))
            return

        # 字符串传送 32 位形式
        if mnem in ('stosw', 'stosd', 'lodsw', 'lodsd', 'movsw', 'movsd', 'scasw', 'scasd'):
            tbl = {'movsw': 0xA5, 'movsd': 0xA5, 'stosw': 0xAB, 'stosd': 0xAB,
                   'lodsw': 0xAD, 'lodsd': 0xAD, 'scasw': 0xAF, 'scasd': 0xAF}
            w = 4 if mnem.endswith('d') else 2
            self.emit(prefix, self.szpfx(w), tbl[mnem])
            return
        if mnem in ('movsb', 'stosb', 'lodsb', 'scasb'):
            tbl = {'movsb': 0xA4, 'stosb': 0xAA, 'lodsb': 0xAC, 'scasb': 0xAE}
            self.emit(prefix, tbl[mnem])
            return

        # inc/dec
        if mnem in ('inc', 'dec'):
            op = self.parse_operand(args[0])
            self.emit(prefix)
            if op.kind in ('reg16', 'reg32'):
                self.emit(self.szpfx(2 if op.kind == 'reg16' else 4),
                          (0x40 if mnem == 'inc' else 0x48) + op.val)
            elif op.kind == 'reg8':
                self.emit(0xFE)
                self.emit(self.modrm(3, 0 if mnem == 'inc' else 1, op.val))
            elif op.kind in MEM_KINDS:
                digit = 0 if mnem == 'inc' else 1
                if (self._opsz or 1) == 1:
                    self.emit_rm(0xFE, digit, op)
                else:
                    self.emit_rm(0xFF, digit, op)
            else:
                raise AsmError('inc/dec 非法操作数')
            return

        # push/pop
        if mnem == 'push':
            op = self.parse_operand(args[0])
            self.emit(prefix)
            if op.kind in ('reg16', 'reg32'):
                self.emit(self.szpfx(2 if op.kind == 'reg16' else 4), 0x50 + op.val)
            elif op.kind == 'imm':
                self.emit(0x68)
                if (self._opsz or self.dflt) == 2:
                    self.emit16(op.val)
                else:
                    self.emit32(op.val)
            elif op.kind == 'sreg':
                self.emit(0x06 + op.val * 8)
            elif op.kind in MEM_KINDS:
                self.emit_rm(0xFF, 6, op)
            else:
                raise AsmError('push 非法操作数')
            return
        if mnem == 'pop':
            op = self.parse_operand(args[0])
            self.emit(prefix)
            if op.kind in ('reg16', 'reg32'):
                self.emit(self.szpfx(2 if op.kind == 'reg16' else 4), 0x58 + op.val)
            elif op.kind == 'sreg':
                self.emit(0x07 + op.val * 8)
            elif op.kind in MEM_KINDS:
                self.emit_rm(0x8F, 0, op)
            else:
                raise AsmError('pop 非法操作数')
            return

        # 移位
        if mnem in ('shl', 'sal', 'shr', 'sar', 'rol', 'ror', 'rcl', 'rcr'):
            digit = {'rol': 0, 'ror': 1, 'rcl': 2, 'rcr': 3,
                     'shl': 4, 'sal': 4, 'shr': 5, 'sar': 7}[mnem]
            op = self.parse_operand(args[0])
            cnt = 1 if len(args) == 1 else self.eval_expr(args[1])
            self.emit(prefix)
            if op.kind in ('reg16', 'reg32'):
                self.emit(self.szpfx(2 if op.kind == 'reg16' else 4), 0xC1,
                          self.modrm(3, digit, op.val), cnt & 0xFF)
            elif op.kind == 'reg8':
                self.emit(0xC0, self.modrm(3, digit, op.val), cnt & 0xFF)
            elif op.kind in MEM_KINDS:
                self.emit_rm(0xC1, digit, op, bytes([cnt & 0xFF]))
            else:
                raise AsmError('移位非法操作数')
            return

        # mul / imul / div / idiv / neg / not（单操作数形式）
        if mnem in ('mul', 'imul', 'div', 'idiv', 'neg', 'not'):
            digit = {'mul': 4, 'imul': 5, 'div': 6, 'idiv': 7, 'neg': 3, 'not': 2}[mnem]
            op = self.parse_operand(args[0])
            self.emit(prefix)
            if op.kind in ('reg16', 'reg32'):
                self.emit(self.szpfx(2 if op.kind == 'reg16' else 4), 0xF7,
                          self.modrm(3, digit, op.val))
            elif op.kind == 'reg8':
                self.emit(0xF6, self.modrm(3, digit, op.val))
            elif op.kind in MEM_KINDS:
                self.emit_rm(0xF7, digit, op)
            else:
                raise AsmError('%s 非法操作数' % mnem)
            return

        # in / out
        if mnem == 'out':
            d = args[0].strip().lower()
            s = args[1].strip().lower()
            self.emit(prefix)
            if d == 'dx' and s == 'al':
                self.emit(0xEE)
            elif d == 'dx' and s == 'ax':
                self.emit(0xEF)
            elif s == 'al':
                self.emit(0xE6, self.eval_expr(args[0]))
            elif s == 'ax':
                self.emit(0xE7, self.eval_expr(args[0]))
            else:
                raise AsmError('out 仅支持 dx/imm8, al/ax')
            return
        if mnem == 'in':
            d = args[0].strip().lower()
            s = args[1].strip().lower()
            self.emit(prefix)
            if d == 'al' and s == 'dx':
                self.emit(0xEC)
            elif d == 'ax' and s == 'dx':
                self.emit(0xED)
            elif d == 'al':
                self.emit(0xE4, self.eval_expr(args[1]))
            elif d == 'ax':
                self.emit(0xE5, self.eval_expr(args[1]))
            else:
                raise AsmError('in 仅支持 al/ax, dx/imm8')
            return

        raise AsmError('未知指令: %s' % mnem)

    @staticmethod
    def _str_bytes(tok):
        """把引号字符串字面量转成字节序列，支持 \\n \\r \\t \\0 \\\\ \\' \\" \\xHH 转义。"""
        q = tok[0]
        body = tok[1:]
        if body.endswith(q):
            body = body[:-1]
        out = bytearray()
        i = 0
        esc = {'n': 10, 'r': 13, 't': 9, '0': 0, '\\': 92, "'": 39, '"': 34, 'e': 27}
        while i < len(body):
            ch = body[i]
            if ch == '\\' and i + 1 < len(body):
                nxt = body[i + 1]
                if nxt == 'x' and i + 3 < len(body):
                    out.append(int(body[i + 2:i + 4], 16) & 0xFF)
                    i += 4
                    continue
                out.append(esc.get(nxt, ord(nxt)) & 0xFF)
                i += 2
                continue
            out.append(ord(ch) & 0xFF)
            i += 1
        return bytes(out)

    @staticmethod
    def _split_args(s):
        """按逗号切分参数：跳过 [] 内的逗号，且**不切分字符串字面量内部的逗号**。

        曾因 db '... , ...' 里含逗号被误切而报"unterminated string literal"。
        """
        out = []
        depth = 0
        quote = None
        cur = ''
        for ch in s:
            if quote:
                cur += ch
                if ch == quote:
                    quote = None
                continue
            if ch in ('"', "'"):
                quote = ch
                cur += ch
                continue
            if ch == '[':
                depth += 1
            elif ch == ']':
                depth -= 1
            if ch == ',' and depth == 0:
                out.append(cur)
                cur = ''
            else:
                cur += ch
        if cur.strip() or out:
            out.append(cur)
        return out


def main():
    if len(sys.argv) < 3:
        print('用法: python xasm16.py <input.asm> <output.bin>')
        return 1
    src = open(sys.argv[1], 'r', encoding='utf-8').read()
    asm = Assembler()
    try:
        data = asm.assemble(src)
    except AsmError as e:
        print('[汇编失败] %s' % e)
        return 2
    open(sys.argv[2], 'wb').write(data)
    print('[OK] %s -> %s  (%d 字节)' % (sys.argv[1], sys.argv[2], len(data)))
    return 0


if __name__ == '__main__':
    sys.exit(main())
