//! 可写文件系统 LumiFS 的挂载层（M8）
//!
//! 和 M5 的 ext2 那一层是**并存**关系，不是替换：
//! - 系统盘（主盘）是 ext2，**只读**，装着要执行的文件；
//! - 数据盘（从盘）是 LumiFS，**可写**，用来写文件、做掉电实验。
//!
//! 分开的好处很实在：格式化、写坏、故意造掉电现场，都只可能弄坏数据盘，
//! 内核自己依赖的系统盘永远干净。
//!
//! 中间夹了一层块缓存 `kbcache::Cached`：文件系统的日志正确性依赖 `flush()`
//! 是真屏障，所以缓存必须把"脏块写回 + 设备 flush"这两步老实做完，
//! 而不是把写攒在内存里。

use crate::arch::sync::IrqSpinlock;
use crate::dev::ata::DataDisk;
use kbcache::Cached;
use lumifs::Disk as _;

pub type DataFs = lumifs::LumiFs<Cached<DataDisk>>;

static FS: IrqSpinlock<Option<DataFs>> = IrqSpinlock::new(None);

/// 块缓存的槽位数。8 块 = 8 KiB，够覆盖一次事务的全部块
/// （超级块 + 日志头 + 载荷 + inode + 数据），再多就要靠淘汰了。
pub const CACHE_SLOTS: usize = 8;

pub struct Report {
    pub blocks: u64,
    pub free_blocks: u32,
    pub entries: usize,
    /// 这次挂载是否重放了日志（true = 上次是"写到一半掉电"）。
    pub replayed: bool,
    pub replayed_blocks: u32,
    pub seq: u64,
    /// 是不是这次刚格式化出来的空盘。
    pub formatted: bool,
    pub cache_hits: u64,
    pub cache_misses: u64,
}

/// 挂载数据盘。
///
/// 盘上没有 LumiFS 魔数时**自动格式化**，并如实标记 `formatted=true`。
/// 这不是"偷偷把用户的盘抹了"：数据盘本来就是这个内核自己的分区，
/// 而且日志里会明写"首次使用，已格式化"。真机上应当由安装器来做，
/// 我们还没有安装器。
pub fn init() -> Result<Report, &'static str> {
    if !crate::dev::ata::slave_present() {
        return Err("没有 ATA 从盘（数据盘未挂载）");
    }
    let mut formatted = false;
    let dev = Cached::new(DataDisk::new(), CACHE_SLOTS);
    let (mut fs, rep) = match lumifs::LumiFs::mount(dev) {
        Ok((fs, rep)) => (fs, rep),
        Err(lumifs::FsError::BadMagic) => {
            formatted = true;
            let d = Cached::new(DataDisk::new(), CACHE_SLOTS);
            let fs = lumifs::LumiFs::format(d).map_err(|_| "格式化数据盘失败")?;
            (fs, lumifs::Replay::NONE)
        }
        Err(lumifs::FsError::BadGeometry) => return Err("数据盘容量或块大小不对"),
        Err(lumifs::FsError::BadSuper) => return Err("LumiFS 超级块校验和不对（镜像损坏）"),
        Err(_) => return Err("挂载数据盘失败"),
    };

    // 列一遍根目录（扁平命名空间 = 所有文件）
    let mut ents = [lumifs::Entry::EMPTY; 8];
    let entries = fs.list(&mut ents).unwrap_or(0);
    let blocks = fs.block_count();
    let free_blocks = fs.free_blocks();
    let seq = fs.journal_seq();
    let st = fs.disk_mut().stats();

    *FS.lock() = Some(fs);
    Ok(Report {
        blocks,
        free_blocks,
        entries,
        replayed: rep.replayed,
        replayed_blocks: rep.blocks,
        seq,
        formatted,
        cache_hits: st.hits,
        cache_misses: st.misses,
    })
}

/// 卸载（再把缓存里的脏块刷一遍）。
pub fn unmount() {
    let mut g = FS.lock();
    if let Some(fs) = g.as_mut() {
        let _ = fs.sync();
        let _ = fs.disk_mut().flush();
    }
    *g = None;
}

/// 在已挂载的文件系统上做一次操作。
pub fn with<R>(f: impl FnOnce(&mut DataFs) -> R) -> Option<R> {
    let mut guard = FS.lock();
    guard.as_mut().map(f)
}

/// 打开（不存在就创建）。
pub fn open_create(name: &str) -> Result<lumifs::Inode, i64> {
    let r = with(|fs| match fs.lookup(name.as_bytes()) {
        Ok(i) => Ok(i),
        Err(lumifs::FsError::NotFound) => fs.create(name.as_bytes()),
        Err(e) => Err(e),
    })
    .ok_or(-5i64)?;
    r.map_err(map_err)
}

pub fn lookup(name: &str) -> Result<lumifs::Inode, i64> {
    let r = with(|fs| fs.lookup(name.as_bytes())).ok_or(-5i64)?;
    r.map_err(map_err)
}

pub fn read_at(ino: u32, off: u32, out: &mut [u8]) -> Result<usize, i64> {
    let r = with(|fs| fs.read_at(ino, off, out)).ok_or(-5i64)?;
    r.map_err(map_err)
}

/// 写文件。返回写入字节数（可能小于 `data.len()`：单次事务有块数上限）。
pub fn write_at(ino: u32, off: u32, data: &[u8]) -> Result<usize, i64> {
    let r = with(|fs| fs.write_at(ino, off, data)).ok_or(-5i64)?;
    r.map_err(map_err)
}

pub fn unlink(name: &str) -> Result<(), i64> {
    let r = with(|fs| fs.unlink(name.as_bytes())).ok_or(-5i64)?;
    r.map_err(map_err)
}

pub fn sync() -> Result<(), i64> {
    let r = with(|fs| fs.sync()).ok_or(-5i64)?;
    r.map_err(map_err)
}

/// 自检专用：把磁盘摆成"已提交未落地"的掉电现场。
pub fn arm_crash(ino: u32, new_data: &[u8]) -> Result<u64, i64> {
    let r = with(|fs| {
        // 先刷干净，保证现场里没有缓存带来的假象
        let _ = fs.sync();
        fs.arm_crash_after_commit(ino, new_data)
    })
    .ok_or(-5i64)?;
    r.map_err(map_err)
}

/// 列出 LumiFS 根目录下的所有条目 `(名字, inode, 大小)`。
///
/// LumiFS 是扁平命名空间（没有目录树），所以这个函数就是它的"readdir"。
pub fn list() -> Result<alloc::vec::Vec<(alloc::vec::Vec<u8>, u32, u32)>, i64> {
    use alloc::vec;
    use alloc::vec::Vec;
    with(|fs| {
        let mut buf = vec![lumifs::Entry::EMPTY; lumifs::INODE_COUNT as usize];
        let n = fs.list(&mut buf).map_err(|_| kl_err())?;
        let mut out: Vec<(Vec<u8>, u32, u32)> = Vec::with_capacity(n);
        for e in buf.iter().take(n) {
            let name = e.name_bytes();
            if name.is_empty() || name == b"." || name == b".." {
                continue;
            }
            let ino = fs.lookup(name).map_err(|_| kl_err())?;
            out.push((name.to_vec(), ino.ino, ino.size));
        }
        Ok(out)
    })
    .unwrap_or(Err(kl_err()))
}

/// 文件系统未挂载/出错时统一的返回值。
fn kl_err() -> i64 {
    klinux::errno::err(klinux::errno::EIO)
}

pub fn cache_dirty() -> usize {
    with(|fs| fs.disk_mut().dirty()).unwrap_or(0)
}

pub fn cache_stats() -> (u64, u64) {
    with(|fs| {
        let s = fs.disk_mut().stats();
        (s.hits, s.misses)
    })
    .unwrap_or((0, 0))
}

fn map_err(e: lumifs::FsError) -> i64 {
    match e {
        lumifs::FsError::NotFound => -2,   // ENOENT
        lumifs::FsError::Exists => -17,    // EEXIST
        lumifs::FsError::NoSpace => -28,   // ENOSPC
        lumifs::FsError::BadName => -36,   // ENAMETOOLONG
        lumifs::FsError::TooBig => -27,    // EFBIG
        lumifs::FsError::Io => -5,         // EIO
        _ => -5,
    }
}
