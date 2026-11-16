//! PCI 总线枚举（M8）
//!
//! 用 legacy 配置空间（0xCF8 选地址 / 0xCFC 读数据）扫总线。ECAM/MMCONFIG 更快，
//! 但要知道 MCFG 表的物理地址并映射它；legacy 口在任何 x86 机器上都有，
//! 是枚举的"最小可用起点"。
//!
//! 为什么要枚举：M8 之后要写真正的设备驱动（virtio、AHCI、网卡），
//! 而驱动第一步永远是"在总线上找到它"。现在先把这层做出来并验证，
//! 下一步接 virtio 时就不用一边调驱动一边怀疑枚举写错了。

const CONFIG_ADDR: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// 一个 PCI 功能（function）。多 function 设备每个 function 单独算一项。
#[derive(Clone, Copy)]
pub struct Device {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    /// 基类：0x01 = 存储控制器，0x02 = 网络，0x03 = 显示，0x06 = 桥。
    pub class: u8,
    pub subclass: u8,
    /// 编程接口：0x80 = 传统 IDE，0x85 = AHCI（SATA），0x00 = virtio 等。
    /// 光看 class/subclass 分不出 IDE 和 AHCI，必须看这一位。
    pub prog_if: u8,
}

impl Device {
    pub const EMPTY: Device = Device {
        bus: 0,
        slot: 0,
        func: 0,
        vendor: 0,
        device: 0,
        class: 0,
        subclass: 0,
        prog_if: 0,
    };

    /// 是不是 PCI-to-PCI 桥（要顺着它去扫次级总线）。
    pub fn is_bridge(&self) -> bool {
        self.class == 0x06 && self.subclass == 0x04
    }

    /// 大类的中文名（日志里看得懂比看得全重要）。
    pub fn class_name(&self) -> &'static str {
        match self.class {
            0x00 => "未分类",
            0x01 => "存储控制器",
            0x02 => "网络控制器",
            0x03 => "显示控制器",
            0x04 => "多媒体",
            0x05 => "内存控制器",
            0x06 => "桥",
            0x07 => "串行总线",
            0x08 => "系统外设",
            0x0C => "串行总线控制器",
            0xFF => "其他",
            _ => "未知类",
        }
    }
}

fn cfg_addr(bus: u8, slot: u8, func: u8, off: u8) -> u32 {
    ((bus as u32) << 16)
        | ((slot as u32) << 11)
        | ((func as u32) << 8)
        | ((off as u32) & 0xFC)
        | 0x8000_0000
}

fn cfg_read32(bus: u8, slot: u8, func: u8, off: u8) -> u32 {
    let a = cfg_addr(bus, slot, func, off);
    // 用 32 位端口写地址 / 读数据
    // SAFETY: 同上。
    unsafe {
        core::arch::asm!("out dx, eax", in("dx") CONFIG_ADDR, in("eax") a,
            options(nomem, nostack, preserves_flags));
        let v: u32;
        core::arch::asm!("in eax, dx", in("dx") CONFIG_DATA, out("eax") v,
            options(nomem, nostack, preserves_flags));
        v
    }
}

fn cfg_read8(bus: u8, slot: u8, func: u8, off: u8) -> u8 {
    let v = cfg_read32(bus, slot, func, off & 0xFC);
    ((v >> ((off & 3) * 8)) & 0xFF) as u8
}

/// 读一个 function 的基本信息。设备不存在返回 None。
fn read_device(bus: u8, slot: u8, func: u8) -> Option<Device> {
    let id = cfg_read32(bus, slot, func, 0);
    let vendor = (id & 0xFFFF) as u16;
    if vendor == 0xFFFF || vendor == 0 {
        return None;
    }
    Some(Device {
        bus,
        slot,
        func,
        vendor,
        device: (id >> 16) as u16,
        class: cfg_read8(bus, slot, func, 0x0B),
        subclass: cfg_read8(bus, slot, func, 0x0A),
        prog_if: cfg_read8(bus, slot, func, 0x09),
    })
}

/// 读 6 个 BAR 里的第 `i` 个（i < 6）。只处理 32 位内存/IO BAR。
///
/// 返回 `(值, 是否是 IO 空间)`。这里**不**做 BAR 探测大小的写回操作：
/// 那会把设备的 BAR 改成全 1，万一后面忘了恢复，驱动就再也找不到寄存器了。
pub fn bar(dev: &Device, i: u8) -> Option<(u32, bool)> {
    if i >= 6 {
        return None;
    }
    let v = cfg_read32(dev.bus, dev.slot, dev.func, 0x10 + i * 4);
    if v == 0 || v == 0xFFFF_FFFF {
        return None;
    }
    Some((v & !0xF, v & 1 != 0))
}

/// 读一个 BAR，**支持 64 位地址**。
///
/// `bar()` 只返回低 32 位且把类型位抹掉了；AHCI 的 ABAR 通常是 64 位 BAR，
/// 真机上有时候会落在 4 GiB 以上——只看低 32 位会映射到一片空地，
/// 表现是"控制器找到了，读 CAP 全是 0xFFFFFFFF"。
pub fn bar64(dev: &Device, i: u8) -> Option<(u64, bool)> {
    if i >= 6 {
        return None;
    }
    let v = cfg_read32(dev.bus, dev.slot, dev.func, 0x10 + i * 4);
    if v == 0 || v == 0xFFFF_FFFF {
        return None;
    }
    if v & 1 != 0 {
        // IO 空间 BAR
        return Some(((v & !3) as u64, true));
    }
    let kind = (v >> 1) & 3;
    let hi = if kind == 2 && i < 5 {
        cfg_read32(dev.bus, dev.slot, dev.func, 0x10 + (i + 1) * 4) as u64
    } else {
        0
    };
    Some((((hi << 32) | (v as u64 & !0xF)) as u64, false))
}

/// 枚举用的统计。
pub struct Report {
    pub devices: usize,
    /// 扫了几条总线（含桥后面的）。
    pub buses: u8,
    /// 存储控制器数量（IDE / AHCI / virtio-blk 都算）。
    pub storage: usize,
    pub bridges: usize,
}

/// 扫总线（从 0 号开始，遇到 PCI-PCI 桥就顺着次级总线往下走一层）。
///
/// 结果写进 `out`，最多 `out.len()` 项，返回报告。
pub fn enumerate(out: &mut [Device]) -> Report {
    let mut n = 0usize;
    let mut storage = 0usize;
    let mut bridges = 0usize;
    let mut max_bus = 0u8;
    // 只展开一层：真实机器上桥可以套很多层，但一层足够找到
    // IDE/AHCI/virtio 这些我们真正要驱动的设备；套太多层会把枚举时间拖长。
    let mut secondary: [u8; 8] = [0; 8];
    let mut nsecondary = 0usize;

    for bus in 0..=0u8 {
        for slot in 0..32u8 {
            let funcs = {
                let ht = cfg_read8(bus, slot, 0, 0x0E);
                if ht & 0x80 != 0 {
                    8
                } else {
                    1
                }
            };
            for func in 0..funcs {
                let Some(d) = read_device(bus, slot, func) else {
                    continue;
                };
                if d.class == 0x01 {
                    storage += 1;
                }
                if d.is_bridge() {
                    bridges += 1;
                    if nsecondary < secondary.len() {
                        let sec = cfg_read8(d.bus, d.slot, d.func, 0x19);
                        if sec != 0 && sec != bus {
                            secondary[nsecondary] = sec;
                            nsecondary += 1;
                        }
                    }
                }
                if n < out.len() {
                    out[n] = d;
                    n += 1;
                }
                max_bus = max_bus.max(bus);
            }
        }
    }

    for k in 0..nsecondary {
        let bus = secondary[k];
        max_bus = max_bus.max(bus);
        for slot in 0..32u8 {
            for func in 0..8u8 {
                let Some(d) = read_device(bus, slot, func) else {
                    continue;
                };
                if d.func != 0 && cfg_read8(bus, slot, 0, 0x0E) & 0x80 == 0 {
                    continue;
                }
                if d.class == 0x01 {
                    storage += 1;
                }
                if n < out.len() {
                    out[n] = d;
                    n += 1;
                }
            }
        }
    }

    Report {
        devices: n,
        buses: max_bus.saturating_add(1),
        storage,
        bridges,
    }
}

// 下面三个是留给驱动的访问口。M8 的枚举自己用的是内部函数，
// 所以这里暂时没人调——但它们是 M9 写 virtio 驱动时要用的第一批接口，
// 逻辑与已通过验证的枚举路径完全一致，不是"猜着写的"。
#[allow(dead_code)]
pub fn config_read32(dev: &Device, off: u8) -> u32 {
    cfg_read32(dev.bus, dev.slot, dev.func, off)
}

/// 读 8 位。
#[allow(dead_code)]
pub fn config_read8(dev: &Device, off: u8) -> u8 {
    cfg_read8(dev.bus, dev.slot, dev.func, off)
}

/// 改 PCI 命令行寄存器里的某一位（比如开 bus-master）。
#[allow(dead_code)]
pub fn set_command_bit(dev: &Device, bit: u8, on: bool) {
    let off = 0x04;
    let old = cfg_read32(dev.bus, dev.slot, dev.func, off);
    let mask = 1u32 << bit;
    let new = if on { old | mask } else { old & !mask };
    let a = cfg_addr(dev.bus, dev.slot, dev.func, off);
    // SAFETY: 标准 PCI 命令寄存器写，语义固定。
    unsafe {
        core::arch::asm!("out dx, eax", in("dx") CONFIG_ADDR, in("eax") a,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("out dx, eax", in("dx") CONFIG_DATA, in("eax") new,
            options(nomem, nostack, preserves_flags));
    }
}
