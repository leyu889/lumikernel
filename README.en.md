# LumiKernel

**An x86_64 monolithic kernel written from scratch in Rust — not a single line inherited from Linux.**

62 self-checks green · 25,000 random syscalls with zero panics · drives survive driver failure · bootable ISO, no toolchain needed

[![self-checks](https://img.shields.io/badge/self--checks-62%2F62-brightgreen)](docs/%E8%AF%81%E6%8D%AE/M12-%E4%B8%B2%E5%8F%A3%E6%97%A5%E5%BF%97.txt)
[![Rust](https://img.shields.io/badge/Rust-no__std-orange?logo=rust&logoColor=white)](Cargo.toml)
[![target](https://img.shields.io/badge/target-x86__64__unknown__none-blue)](.cargo/config.toml)
[![license](https://img.shields.io/badge/license-MIT-green)](LICENSE)
[![release](https://img.shields.io/github/v/release/leyu889/lumikernel?color=blue)](https://github.com/leyu889/lumikernel/releases/latest)

![M12 self-check 62/62](docs/%E8%AF%81%E6%8D%AE/M12-%E5%B1%8F%E5%B9%95.png)

> **Try it before reading.** [Download the bootable ISO](https://github.com/leyu889/lumikernel/releases/latest)
> (5.5 MB), boot it in QEMU or VirtualBox, and about 30 seconds later your screen looks
> like the image above. No compiler, no toolchain, no host setup.

```bash
# QEMU
qemu-system-x86_64 -cdrom LumiKernel-1.3-M12.iso -m 512 -serial stdio

# VirtualBox (BIOS path; the ISO alone is enough, no hard disk required)
VBoxManage createvm --name LumiKernel --ostype Other_64 --register
VBoxManage modifyvm LumiKernel --memory 512 --firmware bios --nic1 none
VBoxManage storagectl LumiKernel --name IDE --add ide
VBoxManage storageattach LumiKernel --storagectl IDE --port 1 --device 0 \
    --type dvddrive --medium LumiKernel-1.3-M12.iso
VBoxManage startvm LumiKernel
```

---

## What it actually is

A monolithic kernel in the Linux sense — drivers, filesystems and the network stack all
live in kernel space — but every line is written here, starting from the boot sector.

| Layer | Status |
|---|---|
| Boot chain | Own stage1 → stage2 → long mode → `0x100000`; plus a UEFI loader |
| Memory | Higher-half kernel, frame allocator, kernel heap, page-table objects, COW, lazy mmap |
| Interrupts | IDT, exception dumps, PIC/PIT, guard pages, slab allocator |
| Tasks | GDT/TSS/IST, ring 3, `int 0x80`, kernel threads, preemption |
| SMP | ACPI/MADT, LAPIC/IOAPIC, APIC timer (RTC-calibrated), AP trampoline, IPI broadcast |
| Filesystems | AHCI/SATA + IDE PIO drivers, ext2 (read-only), **LumiFS** (own journaled, writable), **FAT16/32** (read-only) |
| Programs | fd table, ELF64 loader, per-process page tables, CR3 switching, Linux syscall subset — `busybox sh` runs |
| Network | `knet` crate: Ethernet / ARP / IPv4 / ICMP / UDP / TCP + virtio-net and e1000 drivers *(see limitations)* |
| Safety | Fault injection, random-syscall fuzz, soak runs, **driver fault isolation**, SMEP, W^X audit |

## Numbers (same machine, same hypervisor)

| Operation | Linux 6.12 | LumiKernel M12 |
|---|---:|---:|
| `getpid` | 734 cycles | **426** |
| `read` 4 KiB | 2,063 | 2,880 |
| `mmap` + `munmap` 64 KiB | 8,426 | **806** |
| `fork` + `wait` | 2,420,000 | **51,247** |
| pipe 1-byte round trip | 1,370 | **1,321** |

**Be honest about these numbers:** `fork` is 47× faster partly because the syscall
deliberately omits signal delivery, `rusage` accounting and process-group semantics.
`mmap` is faster because there is no VMA tree and no file-backed mapping path. It is not
an equivalent workload — what it does show is that the policy design is not what makes
a kernel slow.

## Design decisions worth arguing about

**Driver failure must not take the kernel down.** Every driver call passes a health gate
checked at the *device* layer, not the controller layer — a lesson learned when a BIOS
boot path (IDE PIO) sailed straight past a gate that lived in the AHCI driver. Inject
"controller vanished": the call returns `Err` immediately instead of blocking out the
timeout, the driver flips to `Down`, later calls are fast-rejected, and `recover()`
brings back byte-identical reads.

**The kernel must explain its own failures.** Every panic path leaves a register dump
with an address, and every silent-failure path (a `let _ = sched().dequeue(tid)` that
always returned `false`) got turned into either a hard error or a counter. Three
separate scheduling bugs shared that root cause before it was fixed properly.

**Comments explain why, not what.** The source is documented in Chinese — that is the
author's working language — and the comments record the trap, not the syntax. Example,
from `mm/cow.rs`: the reference count records *total* references, not extra ones,
because counting extras made a child process' exit free pages the parent was still using.

## What does not work (documented, not hidden)

| Item | State |
|---|---|
| TCP/IP beyond ICMP | ARP and ICMP are **verified end-to-end** (`ping 10.0.2.2` under VBox NAT returns 4 bytes, rx=2/tx=2/drops=0). UDP/TCP have a socket layer and host unit tests, but no end-to-end traffic yet — there is no TCP server reachable behind NAT to test against. On a machine with no NIC this check is skipped and the total reads 61. |
| Real SMP scheduling | 4 APs come online; per-CPU state, multi-queue scheduler and cross-core locks exist. Parallel scheduling is **disabled**: 8 cores deadlock silently in the execve path (root cause: one global TSS; needs per-CPU TSS + GDT). |
| ext2 writes | **Not implemented.** Writable storage is LumiFS (journaled, crash-recoverable, its own self-checks). Rewriting the boot disk is a risk with no upside here. |
| SMEP | Code is in place, but VirtualBox does not pass `CPUID.07H:EBX[7]`, so `cap=0`. Works on bare metal. |
| SMAP | Capability probed only. Enabling it before every user-memory access goes through `uaccess` would turn working programs into random `EFAULT`s. |

## Build from source

```bash
python build.py          # builds out/LumiKernel-0.1.iso and out/LumiKernel-0.1-efi.img
python test.cmd          # or: cargo test --workspace --exclude lumikernel
```

Requires Rust with the `x86_64-unknown-none` and `x86_64-unknown-uefi` targets.
A vendored offline toolchain lives in `tools/rust/` if you would rather not touch rustup.

## Documentation

All docs are Chinese, written for whoever maintains this next.

| File | Contents |
|---|---|
| `docs/00-定位与稳定性哲学.md` | What this is for, and what "stable" means here |
| `docs/01-架构.md` | Layering, address space, subsystems |
| `docs/02-路线图.md` | Milestones M0–M12 and what is next |
| `docs/04-工程规约.md` | 70+ rules, each one paid for by a real bug |
| `docs/06-性能对比.md` | Benchmark method and raw comparison against Linux |
| `docs/08-规模对标.md` | Why line count versus Linux is the wrong metric |
| `docs/09-驱动隔离与安全.md` | Driver isolation and the W^X audit |

## License

MIT. See [LICENSE](LICENSE).

---

[简体中文 README](README.md)
