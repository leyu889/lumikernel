//! Linux x86_64 ABI 的**纯描述层**（M9）
//!
//! 这一层只做三件事：
//! 1. 把"系统调用号 / errno / 各种常量"写成常量表；
//! 2. 把 `struct stat`、`dirent64`、`utsname` 这类**跨边界结构**按字节序列化，
//!    并在宿主机上断言它们的大小与偏移——这是唯一能在没上真机前抓住
//!    "结构体布局写错"的手段；
//! 3. 路径规范化（`..`、`//`、相对路径）。
//!
//! 为什么不把这些塞进内核：这些全是纯逻辑，放 `no_std` crate 里就能
//! `cargo test` 跑到；粘在 `lumikernel` 里就只能靠虚拟机一次次试。
//!
//! 约定：`struct stat` 用的是 **x86_64** 布局（144 字节）。
//! 如果哪天要兼容 i386 的 32 位 ABI，那是另一套结构，不能混用。

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_code)]

pub mod errno;
pub mod flags;
pub mod path;
pub mod structs;
pub mod sysno;

pub use errno::{err, Errno};
