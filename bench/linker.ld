ENTRY(_start)
SECTIONS
{
  /* 0x400000：和 tools/mkelf.py 生成的用户程序同一装载地址。 */
  . = 0x400000;

  /* `_start` 必须落在映像最开头：mkelf.py 用"入口 = 段首 + 偏移"来生成 ELF，
     偏移 0 是最省事的做法（也省掉一次符号解析）。 */
  .text : {
    *(.text.start)
    *(.text .text.*)
  }
  .rodata : { *(.rodata .rodata.*) }
  .data : { *(.data .data.*) }
  .got  : { *(.got .got.*) *(.got.plt) }
  .bss : { *(.bss .bss.*) *(COMMON) }

  /DISCARD/ : { *(.eh_frame) *(.comment) *(.note*) }
}
