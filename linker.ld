/* LumiKernel 链接脚本（M1：高半区内核）
 *
 * 虚拟地址：0xFFFFFFFF80100000（Linux 同一套路，PML4[511] 区间）
 * 物理地址：0x100000（stage1 读到 0x10000，stage2 整块搬到 1 MiB）
 * 两者差值就是 KERNEL_VA_BASE = 0xFFFFFFFF80000000。
 *
 * boot.S 建好页表后先按物理地址跑，然后 jmp 到高半区；进入高半区后
 * 所有符号访问都走 0xFFFFFFFF80... 这套地址。
 */
ENTRY(_start)

SECTIONS
{
    . = 0xFFFFFFFF80100000;
    __kernel_start = .;

    .text : {
        KEEP(*(.text.entry))
        *(.text .text.*)
    }

    . = ALIGN(4096);
    .rodata : {
        *(.rodata .rodata.*)
    }

    /* 用户态小程序：编译期跟着内核走，运行时被拷到用户页里执行。
       只允许 RIP 相对寻址——绝对地址在拷贝后会失效。 */
    . = ALIGN(4096);
    .user : {
        __user_start = .;
        KEEP(*(.user))
        __user_end = .;
    }

    /* AP 启动跳板：必须真的进映像（否则链接器会把它当孤儿段丢掉） */
    . = ALIGN(4096);
    .ap_tramp : {
        __ap_tramp_start = .;
        KEEP(*(.ap_tramp))
        __ap_tramp_end = .;
    }

    . = ALIGN(4096);
    .data : {
        *(.data .data.*)
    }

    . = ALIGN(4096);
    .bss : {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(4096);
        __bss_end = .;
    }

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note .note.*)
    }
}
