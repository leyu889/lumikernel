//! 早期控制台：COM1 串口 + VGA 文本模式双写。
//!
//! 约束：M0 阶段只有 BSP 一个执行流，且中断关闭，所以这里用最简单的
//! 原子游标而非自旋锁；等 kint/ksched 就位后本模块会换成带锁的 klog。

use core::cell::UnsafeCell;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/* ---------------- 串口 ---------------- */

const COM1: u16 = 0x3F8;

#[inline]
unsafe fn outb(port: u16, value: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") value,
        options(nomem, nostack, preserves_flags));
}

#[inline]
unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    core::arch::asm!("in al, dx", in("dx") port, out("al") v,
        options(nomem, nostack, preserves_flags));
    v
}

pub fn serial_init() {
    unsafe {
        outb(COM1 + 1, 0x00); // 关中断
        outb(COM1 + 3, 0x80); // DLAB
        outb(COM1 + 0, 0x01); // 115200 = 115200/1
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8N1
        outb(COM1 + 2, 0xC7); // FIFO
        outb(COM1 + 4, 0x0B); // IRQ 关闭，RTS/DSR
        let _ = inb(COM1 + 5);
    }
}

fn serial_put(byte: u8) {
    unsafe {
        // 等发送保持寄存器空
        let mut guard = 0u32;
        while inb(COM1 + 5) & 0x20 == 0 {
            guard += 1;
            if guard > 100_000_000 {
                return; // 没有串口时不要死等
            }
            core::hint::spin_loop();
        }
        outb(COM1, byte);
    }
}

/* ---------------- VGA 文本模式 ---------------- */

// 走直接映射区访问显存：既验证了映射，也是内核访问一切物理设备的正规姿势。
const VGA_BASE: usize = crate::mm::DIRECT_MAP_BASE as usize + 0xB8000;
const VGA_WIDTH: usize = 80;
const VGA_HEIGHT: usize = 25;

static ROW: AtomicUsize = AtomicUsize::new(0);
static COL: AtomicUsize = AtomicUsize::new(0);

fn vga_scroll() {
    unsafe {
        let base = VGA_BASE as *mut u16;
        // 整体上移一行
        for row in 1..VGA_HEIGHT {
            for col in 0..VGA_WIDTH {
                let src = *base.add(row * VGA_WIDTH + col);
                *base.add((row - 1) * VGA_WIDTH + col) = src;
            }
        }
        let blank = 0x0F20u16; // 黑底白字空格
        for col in 0..VGA_WIDTH {
            *base.add((VGA_HEIGHT - 1) * VGA_WIDTH + col) = blank;
        }
    }
    ROW.store(VGA_HEIGHT - 1, Ordering::Relaxed);
}

fn vga_put(byte: u8) {
    unsafe {
        let base = VGA_BASE as *mut u16;
        match byte {
            b'\n' => {
                COL.store(0, Ordering::Relaxed);
                let r = ROW.fetch_add(1, Ordering::Relaxed) + 1;
                if r >= VGA_HEIGHT {
                    vga_scroll();
                }
            }
            b'\r' => COL.store(0, Ordering::Relaxed),
            _ => {
                let r = ROW.load(Ordering::Relaxed);
                let c = COL.load(Ordering::Relaxed);
                *base.add(r * VGA_WIDTH + c) = 0x0F00u16 | byte as u16;
                let c = c + 1;
                if c >= VGA_WIDTH {
                    COL.store(0, Ordering::Relaxed);
                    let r = ROW.fetch_add(1, Ordering::Relaxed) + 1;
                    if r >= VGA_HEIGHT {
                        vga_scroll();
                    }
                } else {
                    COL.store(c, Ordering::Relaxed);
                }
            }
        }
    }
}

/* ---------------- 统一写出 ---------------- */

/// 清空 VGA 文本缓冲。内核启动必须自己接管屏幕，否则会看到上一轮的残留字符
/// （VirtualBox 热复位时 BIOS 不保证清屏，这个坑很能骗人）。
fn vga_clear() {
    // SAFETY: VGA_BASE 是直接映射区里的文本缓冲，80x25 个 u16 单元全部可写。
    unsafe {
        let base = VGA_BASE as *mut u16;
        for i in 0..(VGA_WIDTH * VGA_HEIGHT) {
            core::ptr::write_volatile(base.add(i), 0x0F20); // 黑底白字空格
        }
    }
    ROW.store(0, Ordering::Relaxed);
    COL.store(0, Ordering::Relaxed);
}

pub fn init() {
    serial_init();
    vga_clear();
}

/* ---------------- 输出捕获（自检用） ---------------- */

/// 捕获窗口内 put_byte 收到的字节，最多 256 个。
///
/// 为什么需要它：要证明"两个进程各自打印了自己的 OK"，光数输出字节数是不够的
/// （两个进程都打印时字节数一样）。得把输出真的截下来比对。
/// 用 fetch_add 取槽位，多核/抢占下每个字节都有独立位置，不会互相覆盖。
static CAP_ON: AtomicBool = AtomicBool::new(false);

/// 静音开关：为真时 `put_byte` **整条丢弃**（屏幕 + 串口都不写）。
///
/// 只影响走 `put_byte` 的路径（用户态 `write` 与控制台宏），
/// `kserialln!` 走的是 `SerialOnly`，不受影响——所以内核自己的日志
/// 不会因为静音而丢。长跑里用它把 `/bin/hello` 的输出咽掉：
/// 每秒 20 次 exec × 96 字节会把串口日志冲到十几 MB，采样行反而更难找。
static MUTE: AtomicBool = AtomicBool::new(false);

pub fn set_mute(on: bool) {
    MUTE.store(on, Ordering::Relaxed);
}
static CAP_LEN: AtomicUsize = AtomicUsize::new(0);

/// 捕获缓冲大小。M9 要截一整段 `busybox sh` 的输出（echo + ls + cat），
/// 256 字节不够，扩到 2 KiB。多出来的部分只计数不保存——`capture_len`
/// 仍然反映真实长度，但内容只留前 2 KiB。
const CAP_SIZE: usize = 2048;
struct CapBuf(UnsafeCell<[u8; CAP_SIZE]>);
// SAFETY: 每个字节写自己 fetch_add 出来的槽位，不会有两个执行流写同一格。
unsafe impl Sync for CapBuf {}
static CAP: CapBuf = CapBuf(UnsafeCell::new([0u8; CAP_SIZE]));

/// 开始捕获（清空缓冲）。
pub fn capture_start() {
    CAP_LEN.store(0, Ordering::SeqCst);
    CAP_ON.store(true, Ordering::SeqCst);
}

pub fn capture_stop() {
    CAP_ON.store(false, Ordering::SeqCst);
}

fn capture_push(b: u8) {
    let i = CAP_LEN.fetch_add(1, Ordering::SeqCst);
    if i < CAP_SIZE {
        // SAFETY: i 是本执行流独占的槽位。
        unsafe { (*CAP.0.get())[i] = b };
    }
}

/// 捕获内容里是否包含 `needle`。
pub fn capture_contains(needle: &[u8]) -> bool {
    let n = CAP_LEN.load(Ordering::SeqCst).min(CAP_SIZE);
    // SAFETY: 只读前 n 个已写入的字节；缓冲是静态内存，生命周期够长。
    let buf: &[u8] = unsafe { core::slice::from_raw_parts(CAP.0.get() as *const u8, n) };
    if needle.is_empty() || needle.len() > n {
        return false;
    }
    buf.windows(needle.len()).any(|w| w == needle)
}

/// 捕获到的字节数。
pub fn capture_len() -> usize {
    CAP_LEN.load(Ordering::SeqCst).min(CAP_SIZE)
}

/// 把捕获内容转成可打印文本（非 ASCII 换成 '.'）。
pub fn capture_text(out: &mut [u8; 256]) -> usize {
    let n = capture_len().min(out.len());
    // SAFETY: 同上，只读已写入部分。
    let src: &[u8] = unsafe { core::slice::from_raw_parts(CAP.0.get() as *const u8, n) };
    for i in 0..n {
        let b = src[i];
        out[i] = if b >= 0x20 && b < 0x7F { b } else { b'.' };
    }
    n
}

/// 写一个字节到两条通道（串口拿原始字节，屏幕只收 ASCII）。
///
/// 给 `sys_write` 用：用户态写什么就照原样输出，不再经过格式化的中间层。
pub fn put_byte(b: u8) {
    if MUTE.load(Ordering::Relaxed) {
        return;
    }
    if CAP_ON.load(Ordering::Relaxed) {
        capture_push(b);
    }
    serial_put(b);
    if b < 0x80 {
        vga_put(b);
    }
}

/// 完全绕开 `core::fmt` 的十六进制打印。
///
/// 用途只有一个：**在异常处理器里**。如果崩溃路径本身还有 bug，
/// 任何依赖格式化机的东西都可能再崩一次，于是什么也看不到。
/// 这里只用循环 + 移位 + `put_byte`，没有栈上缓冲区、没有 trait 分派。
pub fn raw_u64(mark: u8, v: u64) {
    let digits = b"0123456789abcdef";
    put_byte(mark);
    put_byte(b'=');
    let mut started = false;
    let mut i = 16i32;
    while i > 0 {
        i -= 1;
        let d = ((v >> (i * 4)) & 0xF) as usize;
        if d != 0 || started || i == 0 {
            started = true;
            put_byte(digits[d]);
        }
    }
    put_byte(b'\n');
}

pub struct Console;

impl Console {
    pub const fn new() -> Self {
        Console
    }
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            // 串口拿完整的 UTF-8：中文日志是权威记录。
            serial_put(b);
            // VGA 文本模式没有中文字库，非 ASCII 字节直接丢弃，
            // 只在屏幕上留可读的 ASCII 骨架（M7 上 framebuffer 控制台 +
            // 内嵌字库后再统一）。
            if b < 0x80 {
                vga_put(b);
            }
        }
        Ok(())
    }
}

/// 只写串口、不写屏幕的写出器：中文细节走这里，VGA 只留 ASCII 骨架。
pub struct SerialOnly;

impl Write for SerialOnly {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            serial_put(b);
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::console::Console::new(), $($arg)*);
    }};
}

/// 只进串口日志（中文长句用；屏幕不显示中文）。
#[macro_export]
macro_rules! kserial {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::console::SerialOnly, $($arg)*);
    }};
}

#[macro_export]
macro_rules! kserialln {
    () => { $crate::kserial!("\n") };
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::console::SerialOnly, $($arg)*);
    }};
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::console::Console::new(), $($arg)*);
    }};
}
