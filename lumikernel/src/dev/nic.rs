//! 网卡门面：把 virtio-net / e1000 藏在同一个接口后面。
//!
//! 为什么需要这一层：网络协议栈不该知道底下是半虚拟化还是模拟硬件。
//! 加了这层之后，**换网卡只需要动这一个文件**——e1000 那条路留着，
//! 因为真机上多半还是它（virtio 只存在于虚拟化环境）。

use crate::arch::sync::IrqSpinlock;

/// 当前生效的后端。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    None,
    Virtio,
    E1000,
}

static ACTIVE: IrqSpinlock<Backend> = IrqSpinlock::new(Backend::None);
/// 最近一次初始化失败的原因（自检和日志要能说清楚"为什么没网"）。
static LAST_ERR: IrqSpinlock<Option<&'static str>> = IrqSpinlock::new(None);

/// 初始化：**先 virtio（纯软件、无 PHY），再 e1000（真机兜底）**。
pub fn init() -> Result<(), &'static str> {
    let v = crate::dev::virtio::init();
    if v.is_ok() {
        *ACTIVE.lock() = Backend::Virtio;
        return Ok(());
    }
    let e = crate::dev::e1000::init();
    if e.is_ok() {
        *ACTIVE.lock() = Backend::E1000;
        return Ok(());
    }
    *LAST_ERR.lock() = Some(v.err().unwrap_or("未知"));
    Err("virtio 与 e1000 都不可用")
}

pub fn backend() -> Backend {
    *ACTIVE.lock()
}

pub fn last_error() -> Option<&'static str> {
    *LAST_ERR.lock()
}

pub fn mac() -> [u8; 6] {
    match backend() {
        Backend::Virtio => crate::dev::virtio::mac(),
        Backend::E1000 => crate::dev::e1000::mac(),
        Backend::None => [0; 6],
    }
}

pub fn up() -> bool {
    match backend() {
        Backend::Virtio => crate::dev::virtio::up(),
        Backend::E1000 => crate::dev::e1000::up(),
        Backend::None => false,
    }
}

pub fn send(frame: &[u8]) -> Result<(), &'static str> {
    if !crate::dev::health::is_up(crate::dev::health::Driver::Net) {
        return Err("网卡已降级");
    }
    if crate::qa::fault::deny(kqa::fault::Point::NicDown) {
        crate::dev::health::mark_down(crate::dev::health::Driver::Net, "注入：网卡失联");
        return Err("故障注入：网卡失联");
    }
    match backend() {
        Backend::Virtio => crate::dev::virtio::send(frame),
        Backend::E1000 => crate::dev::e1000::send(frame),
        Backend::None => Err("没有网卡"),
    }
}

pub fn poll(f: impl FnMut(&[u8])) -> usize {
    if !crate::dev::health::is_up(crate::dev::health::Driver::Net) {
        return 0;
    }
    match backend() {
        Backend::Virtio => crate::dev::virtio::poll(f),
        Backend::E1000 => crate::dev::e1000::poll(f),
        Backend::None => 0,
    }
}
