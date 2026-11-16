; ============================================================================
; LumiOS 引导第一阶段（stage1）· 512 字节引导扇区
;
; 职责（刻意保持极简，把复杂度全部推给 stage2）：
;   1. 初始化段寄存器与栈，进入文本模式清屏
;   2. 用 int 13h 把 stage2（LBA 1..32）搬到 0x8000
;   3. 两段式把内核映像搬到 0x10000 区域（0x9C00 以上是 EBDA，BIOS 工作区，
;           绝不能写穿——实测写穿后 int 13h 会在 BIOS 内部死锁）:
;        b) 尾部扇区进 unreal mode 后直写物理 0x18C000（=0x100000+0x8C000）
;       stage2 会在保护模式把 0x10000→0x100000 搬 576 KiB，与尾部拼接成完整映像
;   4. 远跳转入 stage2（0x8000），由它负责保护模式与长模式
;
; 软盘布局（512B 扇区）：
;   LBA 0        stage1（本文件）
;   LBA 1..32    stage2（16KiB @0x8000）
;   LBA 33..63   预留
;   LBA 64..     64 位 Rust 内核映像（@0x10000，再由 stage2 搬到 @0x100000 运行）
;
; 读盘采用「每次 1 扇区 + 手动推进 CHS」，规避跨磁道读取的 BIOS 兼容性问题，
; 也避免在 512 字节里塞除法。
; ============================================================================
org 0x7C00
bits 16

start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7C00
    sti
    mov [drive], dl                 ; BIOS 在 DL 里传来启动盘号

    mov ax, 0x0003                  ; 80x25 文本模式并清屏
    int 0x10

    mov si, msg_title
    call puts

    xor ah, ah                      ; 磁盘系统复位
    mov dl, [drive]
    int 0x13

    ; ---------------------------------------- stage2：LBA1（= C0/H0/S2）起 32 扇区
    mov byte [cur_cyl], 0
    mov byte [cur_head], 0
    mov byte [cur_sec], 2
    mov word [dest_seg], 0x0800     ; 0x8000
    mov cx, 32
s2_load:
    cmp cx, 0
    je s2_done
    call read_one
    mov ax, [dest_seg]
    add ax, 0x20
    mov [dest_seg], ax
    dec cx
    jmp s2_load
s2_done:

    mov si, msg_ok1
    call puts

    ; ---------------------------------------- 内核头部：LBA64（= C1/H1/S11）起 1152 扇区
    mov byte [cur_cyl], 1
    mov byte [cur_head], 1
    mov byte [cur_sec], 11
    mov word [dest_seg], 0x1000     ; 0x10000
    mov cx, 1120
k_load:
    cmp cx, 0
    je k_done
    call read_one
    mov ax, [dest_seg]
    add ax, 0x20
    mov [dest_seg], ax
    dec cx
    jmp k_load
k_done:

    ; -------------------- 尾部装载：unreal mode 直写物理 0x18C000 --------------------
    ; 开 A20：优先 BIOS int 15h AX=2401（VBox BIOS 最可靠）；失败再用 0x92 fast gate。
    ; 坑（实测）：只用 0x92 时 A20 偶发不生效——尾部写 0x18C000 回卷到 0x8C000
    ; 撕坏头部映像，表现为「时好时坏」的随机挂死（读改写先清 bit0 防快速复位）。
    mov ax, 0x2401
    int 0x15
    jnc a20_done
    in al, 0x92
    and al, 0xFE
    or al, 2
    out 0x92, al
a20_done:

    ; 进 unreal mode：加载 GDT → 开 PE → 远跳清流水 → 给 fs 装上 4GB 数据段 → 关 PE
    cli
    lgdt [unreal_gdt_ptr]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp 0x08:pm_unreal
pm_unreal:
    mov bx, 0x10
    mov fs, bx                     ; fs 隐藏缓存锁住 4GB 上限
    mov eax, cr0
    and eax, 0xFFFFFFFE
    mov cr0, eax
    jmp 0x0000:rm_unreal
rm_unreal:
    sti

    ; 尾部扇区：读 1 扇到 0xA0000（VGA gfx，文本模式下空闲），再 movsd 直写物理 0x190000。
    ; MOVS 语义：源 = DS:ESI（可用段前缀改），目的 = ES:EDI（**硬连 ES，不能改前缀**）。
    ; 布局：DS=0、ESI=0xA0000、FS 的隐藏描述符（unreal 4GB，base=0）作源前缀 → 源线性 0xA0000；
    ;      ES=0x10（同款 4GB 描述符）、EDI=0x190000 → 目的线性 0x190000。
    ; 坑（实测）：实模式下 mov fs,bx 会把 FS 隐藏基址重载成 0x10<<4=0x100，毁掉 unreal 映射；
    ;      无 66 前缀的 A5 是 16 位字拷贝——必须 67 66 F3 A5（a32 o32 rep movsd）。
    mov si, msg_khead
    call puts                       ; 进度标记：k_load 完成
    mov word [dest_seg], 0xA000
    mov bp, @KERNEL_SECTORS@ - 1120
    mov edi, 0x18C000
tail_loop:
    cmp bp, 0
    je tail_done
    call read_one                  ; 读 1 扇到 0xA0000，推进 CHS
    mov ax, 0xA000
    mov ds, ax                     ; 源段：实模式直达 0xA0000（限长 64K 够用）
    xor si, si
copy_dwords:
    db 0x66, 0x8B, 0x04           ; mov eax, [si]     源：DS:SI = 0xA0000
    db 0x66, 0x64, 0x89, 0x07     ; mov fs:[edi], eax 目的：FS unreal 4GB → 线性 0x190000
    add si, 4
    add edi, 4
    cmp si, 512
    jb copy_dwords
    mov ax, 0
    mov ds, ax
    dec bp
    jmp tail_loop
tail_done:
    mov si, msg_ok2
    call puts
    mov si, msg_tail
    call puts

    mov dl, [drive]
    jmp 0x0000:0x8000               ; 交棒给 stage2

; ---------------------------------------------------------------- 读 1 个扇区
; 把 1 个扇区读到 es=[dest_seg]:0，并推进 cur_cyl/cur_head/cur_sec。
; 不碰 [count]，由调用方控制循环次数与 dest_seg 推进。
read_one:
    push ax
    push bx
    push cx
    push dx
    mov dl, [drive]
    mov dh, [cur_head]
    mov ch, [cur_cyl]
    mov cl, [cur_sec]
    mov ax, [dest_seg]
    mov es, ax
    xor bx, bx
    mov ax, 0x0201                  ; AH=02 读盘，AL=1 个扇区
    int 0x13
    jc near rs_err
    inc byte [cur_sec]              ; 推进 CHS（每磁道 18 扇区，2 磁头）
    cmp byte [cur_sec], 19
    jb ro_next
    mov byte [cur_sec], 1
    inc byte [cur_head]
    cmp byte [cur_head], 2
    jb ro_next
    mov byte [cur_head], 0
    inc byte [cur_cyl]
ro_next:
    pop dx
    pop cx
    pop bx
    pop ax
    ret

; ---------------------------------------------------------------- 字符串输出
puts:
    push ax
    push bx
    push di
    push es
    mov ax, 0xB800
    mov es, ax
    mov di, [cursor]
p_loop:
    lodsb
    cmp al, 0
    je p_done
    cmp al, 13
    je p_loop
    cmp al, 10
    je p_nl
    mov bh, 0x0F
    mov es:[di], al
    inc di
    mov es:[di], bh
    inc di
    jmp p_loop
p_nl:
    add di, 160                    ; 进下一文本行（不回绕列，启动信息足够短）
    jmp p_loop
p_done:
    mov [cursor], di
    pop es
    pop di
    pop bx
    pop ax
    ret

; ---------------------------------------------------------------- 变量与文案
cursor:   dw 0
drive:    db 0
dest_seg: dw 0
cur_cyl:  db 0
cur_head: db 0
cur_sec:  db 0

msg_title: db 'LumiOS', 13, 10, 0
msg_ok1:   db 's2', 13, 10, 0
msg_khead: db 'k', 13, 10, 0
msg_ok2:   db 'kk', 13, 10, 0
msg_err:   db '!', 13, 10, 0
msg_tail:  db 't', 13, 10, 0

; unreal mode GDT（仅装载尾部用：null + 16 位代码 + 4GB 数据）
unreal_gdt_ptr:
    dw unreal_gdt_end - unreal_gdt - 1
    dd unreal_gdt
unreal_gdt:
    dd 0, 0                        ; 0x00 空描述符
    dw 0xFFFF, 0x0000             ; 0x08 16 位代码段：base 0, limit 0xFFFF
    db 0x00, 0x9A, 0x00, 0x00
    dw 0xFFFF, 0x0000             ; 0x10 4GB 数据段：base 0, G=1 -> 4GB
    db 0x00, 0x92, 0xCF, 0x00
unreal_gdt_end:

; ---------------------------------------------------------------- 读盘失败
rs_err:
    mov si, msg_err
    call puts
    hlt
    jmp rs_err

times 510-($-$$) db 0
dw 0xAA55
