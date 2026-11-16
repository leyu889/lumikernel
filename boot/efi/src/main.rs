//! LumiKernel 的 UEFI 装载器（M10）
//!
//! 为什么需要它：真机几乎都只提供 UEFI 固件，我们那套 BIOS 引导链
//! （stage1 512 字节 + 实模式 stage2 + int 13h）在现代机器上根本跑不到。
//!
//! 它做四件事，一件不多：
//! 1. 把**嵌在自己映像里**的内核（`mkefi.py` 追加成 PE 段）拷到物理 1 MiB；
//! 2. 把 UEFI 内存描述符翻成内核认识的 E820 表，放在**物理 1 GiB 以内**的
//!    一块池内存里（内核的早期恒等映射只覆盖 0..1 GiB）；
//! 3. `ExitBootServices`——之后固件的定时器/中断就再也不会跳进来；
//! 4. 装一张最小 GDT，跳到内核入口 0x100000，并以 `%edi` 传 E820 表地址。
//!
//! **不做**的事：不建页表。内核入口 `boot.S` 第一件事就是建自己的页表并切
//! CR3；它按"物理地址运行 + 恒等映射"设计，所以这里只要保证跳过去那一刻
//! 0x100000 可读可执行即可（UEFI 固件对常规内存本来就是恒等映射）。
//! 少建一套页表 = 少一处"两套映射不一致"的坑。

#![no_std]
#![no_main]
#![allow(clippy::missing_safety_doc)]

use core::arch::asm;
use core::panic::PanicInfo;

type Status = usize;
type Handle = *mut core::ffi::c_void;

const EFI_SUCCESS: Status = 0;
const EFI_BUF_TOO_SMALL: Status = 5;

const KERNEL_PA: u64 = 0x0010_0000;
/// ext2 根分区在本盘上的起始 LBA（由 `tools/mkgpt.py` 决定，两边必须一致）。
const EXT2_PART_LBA: u32 = 16384;
/// E820 表必须放在内核早期恒等映射（0..1 GiB）能够到的地方。
const E820_LIMIT: u64 = 0x3FFF_FFFF;
/// 内核的 E820 解析上限（见 `crates/…/e820.rs` 的 `MAX_ENTRIES`）。
const MAX_E820: usize = 64;

/* ================= 嵌在映像里的内核 ================= */

/// `mkefi.py` 会在这个结构后面追加一个 `.blob` 段，并回填 `delta` / `len`。
///
/// 用"标记 + 相对偏移"而不是绝对地址：PE 映像由固件决定加载到哪个基址，
/// 只有**同映像内的相对距离**是编译期就能定死的。
#[repr(C)]
pub struct BlobRef {
    pub mark: [u8; 16],
    /// 内核数据地址 - 本结构地址（RVA 差）
    pub delta: u64,
    pub len: u64,
}

/// 自己的栈。**必须的**：`ExitBootServices` 会把固件栈（EfiBootServicesData）
/// 释放掉，之后任何 push/call 都在踩空——症状是"串口打出 C 之后立刻重启"。
#[repr(align(16))]
struct EflStack([u8; 32768]);
static mut EFL_STACK: EflStack = EflStack([0u8; 32768]);

/// GDT 必须放在**静态区**，不能放栈上。
/// `ExitBootServices` 会把固件栈（EfiBootServicesData）释放掉；GDT/GDTR 若还在
/// 旧栈上，`lgdt` 读到的就是已释放内存——而且之后 CS 指向的描述符也没了，
/// 第一次远跳就 #GP。症状：串口打完 'C' 立刻重启。
static GDT: [u64; 4] = [
    0,
    0x00AF_9A00_0000_FFFF, // 0x08 64 位代码
    0x00CF_9200_0000_FFFF, // 0x10 数据
    0x00AF_9A00_0000_FFFF, // 0x18 64 位代码（内核入口用的选择子）
];

/// 退出引导服务/跳内核要用的参数，统一放静态区。
///
/// 为什么不用寄存器传：`call ExitBootServices` 之后只有"被调用者保存"的寄存器
/// 还可靠，而 rbx/rbp 在 Rust 内联汇编里根本不让当操作数（LLVM 自用）。
/// 干脆全部落到静态区，汇编里用 `[sym]` 读——既不受调用约定影响，
/// 也不受"旧栈已被释放"影响。
static mut BOOT_STACK_TOP: u64 = 0;
static mut BOOT_EXIT: u64 = 0;
static mut BOOT_ENTRY: u64 = 0;
static mut BOOT_E820: u64 = 0;
static mut BOOT_IMAGE: u64 = 0;
static mut BOOT_KEY: u64 = 0;
/// GDTR 操作数（2 字节 limit + 8 字节 base）
static mut BOOT_GDTR: [u8; 16] = [0u8; 16];

#[no_mangle]
#[used]
pub static BLOB_REF: BlobRef = BlobRef {
    mark: *b"LUMIKRNL-BLOB-v1",
    delta: 0,
    len: 0,
};

/* ================= UEFI 最小结构 ================= */

#[repr(C)]
struct TableHeader {
    signature: u64,
    revision: u32,
    header_size: u32,
    crc32: u32,
    reserved: u32,
}

#[repr(C)]
struct SimpleTextOutput {
    reset: usize,
    output_string: Option<unsafe extern "efiapi" fn(*mut SimpleTextOutput, *const u16) -> Status>,
    // 后面的字段用不到，但**必须占位**，否则结构体大小不对（我们只按指针用，问题不大）
    _rest: [usize; 8],
}

#[repr(C)]
struct BootServices {
    hdr: TableHeader,

    raise_tpl: usize,
    restore_tpl: usize,
    allocate_pages: Option<unsafe extern "efiapi" fn(u32, u32, u64, *mut u64) -> Status>,
    free_pages: usize,
    /// 参数顺序是 **(MapSize, Map, MapKey, DescriptorSize, DescriptorVersion)**。
    /// 把 Map 写成第二位以外的地方，固件就会把整张内存图写进你给的"MapKey"指针，
    /// 直接把栈砸掉——症状是 #UD + 一个莫名其妙的 RIP（踩过一次）。
    get_memory_map: Option<
        unsafe extern "efiapi" fn(*mut usize, *mut u8, *mut usize, *mut usize, *mut u32) -> Status,
    >,
    allocate_pool: Option<unsafe extern "efiapi" fn(u32, usize, *mut *mut u8) -> Status>,
    free_pool: usize,

    // 8..26：这一段本装载器一个都不用，但**必须逐个占位**——
    // 少写一个，后面 ExitBootServices 的偏移就整体错一位，
    // 结果就是"调用了一个随便什么函数指针"，现场极难看清。
    create_event: usize,
    set_timer: usize,
    wait_for_event: usize,
    signal_event: usize,
    close_event: usize,
    check_event: usize,
    install_protocol_interface: usize,
    reinstall_protocol_interface: usize,
    uninstall_protocol_interface: usize,
    handle_protocol: usize,
    reserved: usize,
    register_protocol_notify: usize,
    locate_handle: usize,
    locate_device_path: usize,
    install_configuration_table: usize,
    load_image: usize,
    start_image: usize,
    exit: usize,
    unload_image: usize,
    exit_boot_services: Option<unsafe extern "efiapi" fn(Handle, usize) -> Status>,
}

#[repr(C)]
struct SystemTable {
    hdr: TableHeader,
    firmware_vendor: *const u16,
    firmware_revision: u32,
    _pad: u32,
    console_in_handle: usize,
    con_in: usize,
    console_out_handle: usize,
    con_out: *mut SimpleTextOutput,
    std_err_handle: usize,
    std_err: usize,
    runtime_services: usize,
    boot_services: *mut BootServices,
    num_table_entries: usize,
    config_table: usize,
}

#[repr(C)]
struct MemoryDescriptor {
    typ: u32,
    _pad: u32,
    physical_start: u64,
    virtual_start: u64,
    number_of_pages: u64,
    attribute: u64,
}

/* ================= 输出 ================= */

static mut CON_OUT: *mut SimpleTextOutput = core::ptr::null_mut();

/// 直接写 COM1。`ExitBootServices` 之后固件控制台就没了，
/// 这几颗字节是"跳到内核之前到底走到哪一步"的唯一证据。
unsafe fn serial(b: u8) {
    asm!("out dx, al", in("dx") 0x3F8u16, in("al") b, options(nomem, nostack, preserves_flags));
}

fn puthex(v: u64) {
    let mut buf = [b'0'; 16];
    let hex = b"0123456789abcdef";
    for i in 0..16 {
        buf[15 - i] = hex[((v >> (4 * i)) & 0xF) as usize];
    }
    let mut s = [0u16; 20];
    for i in 0..16 {
        s[i] = buf[i] as u16;
    }
    s[16] = b'\r' as u16;
    s[17] = b'\n' as u16;
    s[18] = 0;
    unsafe {
        if let Some(f) = (*CON_OUT).output_string {
            f(CON_OUT, s.as_ptr());
        }
    }
}

fn puts(s: &str) {
    let mut buf = [0u16; 128];
    let mut n = 0;
    for c in s.chars() {
        if n + 2 >= buf.len() {
            break;
        }
        buf[n] = c as u16;
        n += 1;
    }
    buf[n] = b'\r' as u16;
    buf[n + 1] = b'\n' as u16;
    n += 2;
    buf[n] = 0;
    // SAFETY: CON_OUT 在 efi_main 早期赋值；固件保证该协议在
    // ExitBootServices 之前一直有效。
    unsafe {
        if let Some(f) = (*CON_OUT).output_string {
            f(CON_OUT, buf.as_ptr());
        }
    }
}

/* ================= e820 转换 ================= */

/// 把 UEFI 内存类型翻成 E820 类型。只把"常规内存"当可用：
/// BootServices/Loader 那几类理论上在 ExitBootServices 之后也能用，
/// 但我们马上要把内核放进去、还要让内核自己管内存，混进这些区只会
/// 让"哪块能用"变得不可推理。
fn map_type(t: u32) -> u32 {
    match t {
        7 => 1,  // EfiConventionalMemory -> usable
        9 => 3,  // EfiACPIReclaimMemory
        10 => 4, // EfiACPIMemoryNVS
        _ => 2,  // 其余一律 reserved
    }
}

/// 生成 E820 表（u16 条目数 @0，条目从 +8 开始，每条 24 字节）。
/// 返回条目数。
unsafe fn build_e820(desc: *const u8, size: usize, dsize: usize, out: *mut u8) -> usize {
    let mut buf: [(u64, u64, u32); MAX_E820] = [(0, 0, 0); MAX_E820];
    let mut n = 0usize;

    let mut i = 0usize;
    while i + dsize <= size {
        let d = desc.add(i) as *const MemoryDescriptor;
        i += dsize;

        let typ = (*d).typ;
        let start = (*d).physical_start;
        let pages = (*d).number_of_pages;
        if pages == 0 {
            continue;
        }
        let len = pages.saturating_mul(4096);
        // 内核只直接映射前 1 GiB，超出部分报了也用不上；这里裁掉，
        // 免得把宝贵的条目名额（64 条）浪费在 4 GiB 以上。
        if start >= 0x4000_0000 || len == 0 {
            continue;
        }
        let end = start.saturating_add(len).min(0x4000_0000);
        if end <= start {
            continue;
        }
        let t = map_type(typ);

        // 与上一条同类型且相接 → 合并
        if n > 0 && buf[n - 1].2 == t {
            let (b, l, _) = buf[n - 1];
            if b + l == start {
                buf[n - 1].1 = l + (end - start);
                continue;
            }
        }
        if n >= MAX_E820 {
            break;
        }
        buf[n] = (start, end - start, t);
        n += 1;
    }

    *(out as *mut u16) = n as u16;
    let mut p = out.add(8);
    for k in 0..n {
        let (base, len, t) = buf[k];
        core::ptr::write_unaligned(p as *mut u64, base);
        core::ptr::write_unaligned(p.add(8) as *mut u64, len);
        core::ptr::write_unaligned(p.add(16) as *mut u32, t);
        core::ptr::write_unaligned(p.add(20) as *mut u32, 0);
        p = p.add(24);
    }
    n
}

/* ================= 入口 ================= */

#[no_mangle]
pub unsafe extern "efiapi" fn efi_main(_image: Handle, systab: *mut SystemTable) -> Status {
    CON_OUT = (*systab).con_out;
    let bs = (*systab).boot_services;

    puts("[LumiEFI] UEFI loader up");

    // ---- 1. 读自己的 blob 描述 ----
    let r = core::ptr::read_volatile(&BLOB_REF as *const BlobRef);
    if &r.mark != b"LUMIKRNL-BLOB-v1" || r.len == 0 {
        puts("[LumiEFI] FAIL: kernel blob missing (mkefi.py not run?)");
        return 1;
    }
    let blob = (&BLOB_REF as *const BlobRef as u64).wrapping_add(r.delta) as *const u8;
    let klen = r.len as usize;
    puts("[LumiEFI] kernel blob found");

    // ---- 2. 把内核放到物理 1 MiB（和 BIOS 链完全一致的位置）----
    let pages = (klen as u64 + 0xFFF) / 0x1000;
    let mut target = KERNEL_PA;
    let alloc = (*bs).allocate_pages.unwrap();
    // AllocateAddress = 2；内存类型用 **EfiLoaderCode = 1**。
    //
    // 这一条是整个 UEFI 引导的关键：用 EfiLoaderData 申请的话，固件的页表
    // 会把这块内存标成**不可执行**（NX），我们在 0x100000 取第一条指令就 #PF。
    // EfiLoaderCode 才是"要执行的代码"的正确类型，固件会给它可执行权限。
    // （试过自己建页表 + 切 CR3，踩到 CR4.PCIDE 不刷 TLB 的坑，反而更绕。）
    if alloc(2, 1, pages, &mut target) != EFI_SUCCESS || target != KERNEL_PA {
        puts("[LumiEFI] FAIL: cannot reserve 0x100000");
        return 2;
    }
    core::ptr::copy_nonoverlapping(blob, KERNEL_PA as *mut u8, klen);
    puts("[LumiEFI] kernel copied to 0x100000");
    // 校验：内核首 8 字节应当是 fa fc 66 b8 …（cli; cld; mov ax,0x10）
    puts("[LumiEFI] kernel[0..8]=");
    puthex(core::ptr::read_volatile(KERNEL_PA as *const u64));
    puts("[LumiEFI] blob[0..8]=");
    puthex(core::ptr::read_volatile(blob as *const u64));

    // ---- 3. E820 表：**固定放在物理 0x9000** ----
    // 为什么不另找一块池内存：地址要传给内核，而内核入口 boot.S 在第一屏里
    // 就把 RDI 拿去清零 .bss 了（`rep stosb`），寄存器传参根本活不到 kmain。
    // boot.S 本来就有"E820 在 0x9000"的约定（BIOS 链如此），沿用最省事：
    // 内核也把低 1 MiB 整个保留，不会有人回头踩它。
    let e820_pa: u64 = 0x9000;

    // ---- 4. 取内存图 ----
    let mut map_size = 0usize;
    let mut map_key = 0usize;
    let mut dsize = 0usize;
    let mut dver = 0u32;
    let gmm = (*bs).get_memory_map.unwrap();
    gmm(
        &mut map_size,
        core::ptr::null_mut(),
        &mut map_key,
        &mut dsize,
        &mut dver,
    );
    map_size += 4096; // 留出"取完到用之前又多了几条"的余量
    // 记住**缓冲区容量**。后面重试时要传容量，不能传"上次用了多少"——
    // 传后者必然 EFI_BUFFER_TOO_SMALL（踩过）。
    let map_capacity = map_size;
    let mut map_buf: *mut u8 = core::ptr::null_mut();
    // EfiLoaderData = 2
    if (*bs).allocate_pool.unwrap()(2, map_size, &mut map_buf) != EFI_SUCCESS {
        puts("[LumiEFI] FAIL: cannot allocate memory map");
        return 4;
    }
    if gmm(&mut map_size, map_buf, &mut map_key, &mut dsize, &mut dver) != EFI_SUCCESS {
        puts("[LumiEFI] FAIL: GetMemoryMap");
        return 5;
    }

    let count = build_e820(map_buf, map_size, dsize, e820_pa as *mut u8);
    // 在 count 与条目之间的空档里打一个"来源"标记，内核据此在日志里
    // 区分 UEFI / BIOS。这个位置 stage2（BIOS 链）从来不动，恒为 0。
    core::ptr::write_volatile((e820_pa + 6) as *mut u8, 0xEF);
    // 同盘分区布局：ext2 根文件系统在 ESP 之后的固定 LBA。
    // 内核拿这个偏移去读系统盘——一块 U 盘上装三个分区，真机才可能到 shell。
    core::ptr::write_volatile((e820_pa + 2) as *mut u32, EXT2_PART_LBA);
    puts("[LumiEFI] e820 entries=");
    puthex(count as u64);
    puts("[LumiEFI] readback@0x9000=");
    puthex(core::ptr::read_volatile(0x9000 as *const u64));
    puts("[LumiEFI] dsize=");
    puthex(dsize as u64);



    // ---- 5. 先切自己的页表，再退出引导服务 ----
    // 顺序很关键：EDK2 在 ExitBootServices 里会**重建页表**，把 BootServices /
    // Loader 那几段从映射里摘掉。如果那时还跑在固件页表上，函数一返回、
    // 取下一条指令就 #PF——实测现象是串口只打出 'A'，'B' 永远不出现。
    // 先切到自己的恒等映射，固件之后再怎么改它自己的表都影响不到我们。
    let stack_top = unsafe { core::ptr::addr_of_mut!(EFL_STACK.0) as u64 + 32768 - 64 };
    let exit_bs = (*bs).exit_boot_services.unwrap();

    // 参数落盘到静态区（含 GDTR）
    unsafe {
        let p = core::ptr::addr_of_mut!(BOOT_GDTR) as *mut u8;
        core::ptr::write_unaligned(p as *mut u16, (core::mem::size_of::<[u64; 4]>() - 1) as u16);
        core::ptr::write_unaligned(p.add(2) as *mut u64, core::ptr::addr_of!(GDT) as u64);
        BOOT_STACK_TOP = stack_top;
        BOOT_EXIT = exit_bs as usize as u64;
        BOOT_ENTRY = KERNEL_PA;
        BOOT_E820 = e820_pa;
        BOOT_IMAGE = _image as u64;
        BOOT_KEY = map_key as u64;
    }

    unsafe { serial(b'A') };
    asm!("cli", options(nomem, nostack));

    // ---- 退出引导服务 ----
    // 必须带重试：EDK2 的 GetMemoryMap 内部会自己分配缓冲区，于是"取图"这个动作
    // 本身就可能让刚拿到的 key 失效。标准做法就是"退出失败 → 重取图 → 再退出"，
    // 通常第二次就成（第一次失败是预期内的）。
    let mut exited = false;
    for _ in 0..4 {
        let st = exit_bs(_image, map_key);
        if st == EFI_SUCCESS {
            exited = true;
            break;
        }
        let mut sz = map_capacity;
        let mut g = gmm(&mut sz, map_buf, &mut map_key, &mut dsize, &mut dver);
        if g == EFI_BUF_TOO_SMALL {
            map_size = sz + 8192;
            g = gmm(&mut map_size, map_buf, &mut map_key, &mut dsize, &mut dver);
        }
        if g != EFI_SUCCESS {
            unsafe { serial(b'?') };
            return 7;
        }
    }
    if !exited {
        unsafe { serial(b'!') };
        return 6;
    }

    // 退出成功：从这里开始绝不能再依赖固件栈/固件服务。
    // 第一件事就是换到自己的栈，然后装 GDT、远跳。
    asm!(
        "mov rsp, [{stack}]",
        // 注意 `lea` 而不是 `mov`：需要的是 GDTR 结构体的**地址**，
        // 用 mov 读到的是它头 8 个字节的内容（limit+base 前 6 字节），
        // 拿它去 lgdt 必然 #PF。
        "lea rbx, [{gdtr}]",
        "lgdt [rbx]",
        "mov ax, 0x10",
        "mov ds, ax",
        "mov es, ax",
        "mov ss, ax",
        "mov fs, ax",
        "mov gs, ax",
        "mov edi, [{e820}]",
        "mov dx, 0x3F8",
        "mov al, 0x43",
        "out dx, al",
        // 进内核：用 **iretq** 而不是 retf。
        // iretq 一次性把 rip/cs/rflags/rsp/ss 全设好，是长模式下换 CS 最稳的做法；
        // 而且它明确要求这几项都在栈上按逆序摆，语义没有歧义。
        "push 0x10",              // ss
        "push rsp",               // rsp（内核第一条指令就会换自己的栈，这里只要合法）
        "push 0x202",             // rflags（IF=1）
        "push 0x18",              // cs
        "mov rax, [{entry}]",
        "push rax",               // rip = 0x100000
        "iretq",
        stack = sym BOOT_STACK_TOP,
        gdtr = sym BOOT_GDTR,
        e820 = sym BOOT_E820,
        entry = sym BOOT_ENTRY,
        options(noreturn),
    )
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    puts("[LumiEFI] PANIC");
    loop {
        unsafe { asm!("hlt", options(nomem, nostack)) };
    }
}
