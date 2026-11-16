//! 设备层（M5）
//!
//! 现在只有 ATA。放一个模块目录是为了让后面 PCI 枚举、virtio、键盘
//! 有地方归位，而不是继续往内核根目录堆文件。

pub mod ahci;
pub mod ata;
pub mod pci;
