; ============================================================================
; LumiOS 引导第二阶段（stage2）· 实模式 → 保护模式 → 长模式
;
; 装载地址：0x8000（由 stage1 用 int 13h 从软盘搬入）
; 交接对象：0x10000 处的 64 位 Rust 内核
;
; 本阶段完成的四件事：
;   1. 打开 A20 地址线
;   2. 载入自建 GDT（含 32 位代码段、数据段、64 位代码段、长模式数据段）
;   3. 建立四级页表（PML4 → PDPT → PD，2MiB 大页恒等映射前 1GiB）
;   4. 依序开启 CR4.PAE → CR3 → EFER.LME → CR0.PG|PE，远跳进入 64 位段
;
; 由自研汇编器 tools/xasm16.py 汇编（16/32 位混编），零第三方工具。
; ============================================================================
org 0x8000
bits 16

stage2_entry:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7C00

    mov si, msg_a20
    call puts16

    ; ---------------------------------------------------------- 打开 A20
    ; 走 System Control Port A（0x92）的快速 A20 通道
    in al, 0x92
    or al, 0x02
    out 0x92, al

    ; ------------------------------------------------- 探测物理内存（E820）
    ; 结果布局（交给 64 位内核读取）：
    ;   0x9000 : u16  条目数
    ;   0x9008 : 每条 24 字节 {base u64, len u64, type u32, acpi u32}
    mov si, msg_e820
    call puts16
    xor ebx, ebx                    ; BIOS 要求的续查句柄
    mov di, 0x9008                  ; ES:DI 指向数据区
    mov word [0x9000], 0            ; 条目计数清零
e820_loop:
    mov eax, 0xE820
    mov edx, 0x534D4150             ; 'SMAP'
    mov ecx, 24
    mov dword [di + 20], 1          ; 强制要求 ACPI 3.0 扩展位有效
    int 0x15
    jc e820_done                    ; CF=1 表示枚举结束
    cmp eax, 0x534D4150             ; 校验返回签名
    jne e820_done
    cmp ecx, 20                     ; 至少要有 20 字节
    jb e820_done
    inc word [0x9000]
    cmp word [0x9000], 32           ; 最多 32 条，防止越界
    jae e820_done
    add di, 24
    test ebx, ebx                   ; ebx=0 表示结束
    jnz e820_loop
e820_done:

    ; ---------------------------------------------------------- 载入 GDT
    lgdt [gdtr]

    mov si, msg_gdt
    call puts16

    ; ---------------------------------------------------- 打开保护模式
    mov eax, cr0
    or eax, 0x00000001              ; CR0.PE = 1
    mov cr0, eax

    ; 16 位态下用 0x66 前缀做 32 位偏移的远跳转（66 EA off32 sel16）
    jmp dword 0x08:pm_entry

; ============================================================================
bits 32

pm_entry:
    ; 32 位平坦段
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    mov esp, 0x9F000                ; 临时栈（EBDA 之下，向下生长）

    ; ------------------------------------------------ 把内核搬到 1 MiB
    ; 实模式的 int 13h 只能把映像读到 0x10000，而 0xA0000 起就是 VGA 显存，
    ; 低区窗口总共只有 576 KiB —— M7 之后内核已经装不下了。
    ; 现在处于 32 位平坦段、A20 已开，可以直接整块拷到 1 MiB 之上，
    ; 长模式入口地址也随之改成 0x100000。
    ; 拷贝字节数固定为低区窗口大小：多拷的尾部是软盘上的旧数据，
    ; 没有任何代码会去读它（内核启动时自己清 BSS），无害。
    mov esi, 0x10000
    mov edi, 0x100000
    mov ecx, 0x23000                ; 0x90000 字节 / 4 = 147456 个双字
    rep movsd

    ; ------------------------------------------------ 清零页表区 12KiB
    mov edi, 0xC000                 ; PML4 @ 0xC000
    mov ecx, 0x0C00                 ; 3072 个双字 = 12KiB
    xor eax, eax
    rep stosd

    ; ------------------------------------------------ 搭四级页表骨架
    mov edi, 0xC000
    mov eax, 0xD003                 ; P 与 RW：指向 PDPT @0xD000
    mov [edi], eax

    mov edi, 0xD000
    mov eax, 0xE003                 ; P 与 RW：指向 PD @0xE000
    mov [edi], eax

    ; ------------------------------------------------ 填充 512 个 2MiB 大页
    mov edi, 0xE000
    mov eax, 0x83                   ; P + RW + PS（2MiB 页）
    mov ecx, 512
fill_pd:
    mov [edi], eax
    mov dword [edi+4], 0            ; 页表项是 8 字节，高 32 位清零
    add eax, 0x200000
    add edi, 8
    dec ecx
    jnz fill_pd

    ; ------------------------------------------------ 开启 PAE
    mov eax, cr4
    or eax, 0x20                    ; CR4.PAE = 1
    mov cr4, eax

    mov eax, 0xC000
    mov cr3, eax                    ; CR3 = PML4 物理基址

    ; ------------------------------------------------ 打开长模式位
    mov ecx, 0xC0000080             ; IA32_EFER
    rdmsr
    or eax, 0x100                   ; EFER.LME = 1
    wrmsr

    ; ------------------------------------------------ 开分页 + 进长模式
    mov eax, cr0
    or eax, 0x80000001              ; CR0.PG | CR0.PE
    mov cr0, eax

    ; 跳入 64 位代码段（选择子 0x18），交棒给 Rust 内核（已搬到 1 MiB 处）
    jmp 0x18:0x100000

; ---------------------------------------------------------------- 16 位输出
; 实模式下直接写 VGA 文本缓冲，避免依赖 BIOS 调用
; 注意：本段必须在 bits 16 下汇编（上面已切到 32 位），否则 [di] 会被当成 64/32 位寻址
bits 16
puts16:
    push ax
    push bx
    push di
    push es
    mov ax, 0xB800
    mov es, ax
    mov di, [cursor]
p16_loop:
    lodsb
    cmp al, 0
    je p16_done
    cmp al, 13
    je p16_loop
    cmp al, 10
    je p16_nl
    mov bh, 0x0B
    mov es:[di], al
    inc di
    mov es:[di], bh
    inc di
    inc word [col]
    jmp p16_loop
p16_nl:
    mov ax, [col]
    add ax, ax
    sub di, ax
    add di, 160
    mov word [col], 0
    jmp p16_loop
p16_done:
    mov [cursor], di
    pop es
    pop di
    pop bx
    pop ax
    ret

; ---------------------------------------------------------------- 数据
puts_err:
    mov si, msg_err

cursor:  dw 0
col:     dw 0
gdt:
    dd 0
    dd 0                            ; 0x00 空描述符
    dw 0xFFFF
    dw 0x0000
    db 0x00
    db 0x9A
    db 0xCF
    db 0x00                         ; 0x08 32 位代码段（基址 0，限 4G）
    dw 0xFFFF
    dw 0x0000
    db 0x00
    db 0x92
    db 0xCF
    db 0x00                         ; 0x10 数据段（基址 0，限 4G）
    dw 0xFFFF
    dw 0x0000
    db 0x00
    db 0x9A
    db 0xAF
    db 0x00                         ; 0x18 64 位代码段（L=1）
    dw 0xFFFF
    dw 0x0000
    db 0x00
    db 0x92
    db 0xCF
    db 0x00                         ; 0x20 长模式数据段
gdt_end:

gdtr:
    dw gdt_end - gdt - 1
    dd gdt

msg_a20: db '  [OK] A20 line opened', 13, 10, 0
msg_e820: db '  [OK] E820 memory map probed', 13, 10, 0
msg_gdt: db '  [OK] GDT loaded, entering protected mode', 13, 10, 0
msg_err: db '  [!!] stage2 halt', 13, 10, 0
