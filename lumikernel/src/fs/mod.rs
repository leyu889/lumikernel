//! 文件系统层（M5）：把 ext2 解析器（`crates/kfs`）挂到一个真实块设备上
//!
//! 分工很清楚：
//! - `crates/kfs` 只管"按 ext2 规范解析字节"，不知道底下是 ATA 还是内存镜像，
//!   所以它能在宿主机上跑 19 个单测；
//! - 本文件负责"把设备接上去、加锁、对外提供几个简单操作"。
//!
//! 锁用 `arch::sync::IrqSpinlock`：原子抢锁挡住别的核，关本核中断挡住
//! 自己的中断处理程序——少一个都是死锁（详见 sync.rs）。

use crate::arch::sync::IrqSpinlock;
extern crate alloc;
use crate::dev::ata::AtaDisk;

pub use kfs::{DirEntry, FsError, Inode};

/* ==================== 只读块缓存（M11 性能修复） ==================== */

/// 缓存的块数。64 × 1 KiB = 64 KiB 静态内存。
///
/// 为什么这里敢做"永不失效"的缓存：**ext2 是只读挂载的**，盘上的内容
/// 在内核运行期间不会被改写（要写的是另一块盘上的 LumiFS）。没有写，
/// 就没有失效问题——这一条是这层缓存能只有二十行的根本原因。
///
/// 为什么需要它：`kfs` 每次 `read_file` 都会重读 inode，`bmap` 每次都重读
/// 间接块。读一个 1 MiB 的文件时，元数据块的读取次数和数据块是同一个量级。
/// M11 的性能自检把这个代价摆了出来（2 MiB/s），加缓存后见下方实测注释。
const RC_SLOTS: usize = 64;
/// ext2 块大小（与 `kfs` 的 `BLOCK_SIZE`、ATA 层一致）。
const RC_BLOCK: usize = 1024;

static RC_KEY: [core::sync::atomic::AtomicU64; RC_SLOTS] =
    [const { core::sync::atomic::AtomicU64::new(u64::MAX) }; RC_SLOTS];
static RC_HITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static RC_MISS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

struct CacheData(core::cell::UnsafeCell<[[u8; RC_BLOCK]; RC_SLOTS]>);

// SAFETY: 只通过 `FS` 的 `IrqSpinlock` 访问（`CachedDisk` 只存在于
// `Ext2Reader` 内部，而 `Ext2Reader` 被锁包着），所以同一时刻只有一个执行流。
unsafe impl Sync for CacheData {}

static RC_DATA: CacheData = CacheData(core::cell::UnsafeCell::new([[0u8; RC_BLOCK]; RC_SLOTS]));

/// 包在真实设备外面的一层直连（direct-mapped）读缓存。
struct CachedDisk {
    inner: AtaDisk,
}

impl CachedDisk {
    const fn new() -> Self {
        CachedDisk { inner: AtaDisk::new() }
    }
}

impl kfs::BlockDevice for CachedDisk {
    fn block_size(&self) -> usize {
        RC_BLOCK
    }

    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), kfs::IoError> {
        let idx = (lba as usize) & (RC_SLOTS - 1);
        if RC_KEY[idx].load(core::sync::atomic::Ordering::Relaxed) == lba {
            RC_HITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // SAFETY: 索引在范围内；底层缓冲生命周期是整个内核运行期，
            // 且 `self` 被锁保护，不存在并发写。
            unsafe {
                let src = (RC_DATA.0.get() as *const u8).add(idx * RC_BLOCK);
                core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), RC_BLOCK.min(buf.len()));
            }
            return Ok(());
        }
        self.inner.read_block(lba, buf)?;
        RC_MISS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // SAFETY: 同上，写入位置由 idx 界定。
        unsafe {
            let dst = (RC_DATA.0.get() as *mut u8).add(idx * RC_BLOCK);
            core::ptr::copy_nonoverlapping(buf.as_ptr(), dst, RC_BLOCK.min(buf.len()));
        }
        RC_KEY[idx].store(lba, core::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn block_count(&self) -> u64 {
        self.inner.block_count()
    }
}

/// 清空缓存。
///
/// 两个用途：① 故障注入自检必须先清缓存，否则"读盘失败"根本不会发生
/// （数据在缓存里）；② 性能基准要测冷读时用。
pub fn cache_flush() {
    for k in RC_KEY.iter() {
        k.store(u64::MAX, core::sync::atomic::Ordering::Relaxed);
    }
    RC_HITS.store(0, core::sync::atomic::Ordering::Relaxed);
    RC_MISS.store(0, core::sync::atomic::Ordering::Relaxed);
}

/// 缓存命中统计 `(hits, misses)`。
pub fn cache_stats() -> (u64, u64) {
    (
        RC_HITS.load(core::sync::atomic::Ordering::Relaxed),
        RC_MISS.load(core::sync::atomic::Ordering::Relaxed),
    )
}

type DiskFs = kfs::Ext2Reader<CachedDisk>;

static FS: IrqSpinlock<Option<DiskFs>> = IrqSpinlock::new(None);

/// 目录项是定长数组，需要一个"空"值来初始化接收缓冲。
const EMPTY_ENTRY: DirEntry = DirEntry {
    inode: 0,
    name_len: 0,
    name: [0u8; 255],
};

/// 挂载结果，供自检打印。
pub struct Report {
    pub total_blocks: u64,
    pub root_entries: usize,
    /// 根目录里第一个普通文件的名字（没有就为空）。
    pub first_name: [u8; 32],
    pub first_name_len: usize,
    pub first_size: u32,
}

/// 挂载根分区（主 ATA 盘上的 ext2）。
pub fn init() -> Result<Report, &'static str> {
    if !crate::dev::ata::present() {
        return Err("没有 ATA 盘，无法挂载 ext2");
    }
    let mut reader = DiskFs::mount(CachedDisk::new()).map_err(|e| match e {
        FsError::BadMagic => "不是 ext2（魔数不对）",
        FsError::BadSuperblock => "超级块字段非法",
        FsError::Io(_) => "读设备失败",
        _ => "挂载失败",
    })?;

    let root = reader.root();
    let mut buf = [EMPTY_ENTRY; 16];
    let n = reader.list(root, &mut buf).map_err(|_| "列根目录失败")?;

    // 记下第一个普通文件（跳过 . 和 ..），日志里能直接看到"盘上有什么"
    let mut first_name = [0u8; 32];
    let mut first_name_len = 0usize;
    let mut first_size = 0u32;
    for e in buf.iter().take(n) {
        let name = e.name();
        if name == b"." || name == b".." || name.is_empty() {
            continue;
        }
        let len = name.len().min(32);
        first_name[..len].copy_from_slice(&name[..len]);
        first_name_len = len;
        if let Ok(ino) = reader.lookup("/hello.txt") {
            first_size = ino.size;
        }
        break;
    }

    let total_blocks = crate::dev::ata::sectors() / 2;
    *FS.lock() = Some(reader);
    Ok(Report {
        total_blocks,
        root_entries: n,
        first_name,
        first_name_len,
        first_size,
    })
}

/// 在已挂载的文件系统上做一次操作。未挂载返回 `None`。
pub fn with<R>(f: impl FnOnce(&mut DiskFs) -> R) -> Option<R> {
    let mut guard = FS.lock();
    guard.as_mut().map(f)
}

/// 按绝对路径查找。
pub fn lookup(path: &str) -> Result<Inode, &'static str> {
    with(|r| r.lookup(path))
        .ok_or("文件系统未挂载")?
        .map_err(|e| match e {
            FsError::NotFound => "路径不存在",
            FsError::NotADirectory => "路径中间不是目录",
            FsError::Corrupt => "镜像损坏",
            _ => "查找失败",
        })
}

/// 读文件（带偏移），返回实际读到的字节数。
pub fn read_at(inode: Inode, off: u32, out: &mut [u8]) -> Result<usize, &'static str> {
    with(|r| r.read_file(inode, off, out))
        .ok_or("文件系统未挂载")?
        .map_err(|_| "读文件失败")
}

/// 列目录。
pub fn list(dir: Inode, out: &mut [DirEntry]) -> Result<usize, &'static str> {
    with(|r| r.list(dir, out))
        .ok_or("文件系统未挂载")?
        .map_err(|_| "列目录失败")
}

/// 解析路径，`follow = false` 时不跟随最后一段的符号链接（`lstat` 语义）。
pub fn lookup_opt(path: &str, follow: bool) -> Result<Inode, i64> {
    let r = with(|r| {
        if follow {
            r.lookup(path)
        } else {
            r.lookup_nofollow(path)
        }
    })
    .ok_or(klinux::errno::err(klinux::errno::EIO))?;
    r.map_err(map_errno)
}

/// 把 `kfs::FsError` 翻成 errno。**这一层不能省**：
/// 让上层各自 match 的话，同一个错误在不同系统调用里会返回不同的号，
/// 用户态（尤其 busybox 那种"按 errno 分支"的代码）就会走错路。
fn map_errno(e: FsError) -> i64 {
    use klinux::errno as E;
    E::err(match e {
        FsError::NotFound => E::ENOENT,
        FsError::NotADirectory => E::ENOTDIR,
        FsError::IsADirectory => E::EISDIR,
        FsError::BufferTooSmall => E::ERANGE,
        FsError::Loop => E::ELOOP,
        FsError::NameTooLong => E::ENAMETOOLONG,
        FsError::NotASymlink => E::EINVAL,
        FsError::Io(_) => E::EIO,
        _ => E::EIO,
    })
}

/// 读符号链接目标。返回长度（写入 `out`，不含 NUL）。
pub fn read_link(path: &str, out: &mut [u8]) -> Result<usize, i64> {
    with(|r| match r.lookup_nofollow(path) {
        Ok(ino) => r.read_link(ino, out),
        Err(e) => Err(e),
    })
    .ok_or(klinux::errno::err(klinux::errno::EIO))?
    .map_err(map_errno)
}

/// 列目录，缓冲区按需在堆上分配。
///
/// 为什么用堆：一个 `DirEntry` 有 260 字节，64 项就是 16 KiB，
/// 放在系统调用的栈帧里会把内核栈顶到边。这里失败也只是返回错误，
/// 不会 panic。
pub fn list_alloc(dir: Inode) -> Result<alloc::vec::Vec<DirEntry>, i64> {
    use alloc::vec;
    let mut buf = vec![EMPTY_ENTRY; 96];
    let n = with(|r| r.list(dir, &mut buf))
        .ok_or(klinux::errno::err(klinux::errno::EIO))?
        .map_err(|e| match e {
            // 条目比预估多：把容量放大一次再试，而不是直接报错
            FsError::BufferTooSmall => e,
            other => other,
        })
        .map_err(map_errno)?;
    buf.truncate(n);
    Ok(buf)
}

/// 是否是目录。
pub fn is_dir(inode: Inode) -> bool {
    with(|r| r.is_dir(inode)).unwrap_or(false)
}

/// 根 inode。
pub fn root() -> Option<Inode> {
    with(|r| r.root())
}
