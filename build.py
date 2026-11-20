#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
LumiKernel 构建：内核 → stage2 → stage1 → 软盘镜像 → 可启动 ISO

设计跟既有 LumiKernel 工程保持一致（引导链与 ISO 打包器已实测可用）：
    LBA 0        stage1（512 B，含 @KERNEL_SECTORS@ 注入）
    LBA 1..32    stage2 @0x8000（实模式 → 保护模式 → 长模式）
    LBA 64..     内核扁平映像（stage1 读到 0x10000，stage2 搬到 0x100000）

用法：
    python build.py            # 出 ISO 到 out/
    python build.py --no-build # 跳过 cargo，直接重打包（改引导层时用）
"""
import argparse
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SECTOR = 512
FLOPPY_SIZE = 1474560
STAGE2_LBA = 1
STAGE2_SECTORS = 32
KERNEL_LBA = 64
KERNEL_MAX = 100 * 1024 * 1024

# ---- 工具链 ----
# 本仓库可以自带一套 Rust（`tools/rust/`，离线可用）；没有就用系统里的。
# 这样同一份源码既能在"没网没工具链"的机器上跑，也能在 CI / 别人的机器上跑。
TOOLCHAIN = os.path.join(HERE, "tools", "rust")
TOOLCHAIN_CARGO = os.path.join(TOOLCHAIN, "cargo")


def cargo_exe():
    name = "cargo.exe" if os.name == "nt" else "cargo"
    local = os.path.join(TOOLCHAIN_CARGO, "bin", name)
    return local if os.path.isfile(local) else "cargo"


def toolchain_env():
    """把自带工具链与（可选）MinGW 目录加进环境；都不存在就原样返回系统环境。"""
    env = {}
    if os.path.isdir(os.path.join(TOOLCHAIN, "rustup")):
        env["RUSTUP_HOME"] = os.path.join(TOOLCHAIN, "rustup")
        env["CARGO_HOME"] = TOOLCHAIN_CARGO
    extra = []
    if os.path.isdir(os.path.join(TOOLCHAIN_CARGO, "bin")):
        extra.append(os.path.join(TOOLCHAIN_CARGO, "bin"))
    mingw = os.environ.get("MINGW_BIN", r"D://mingw64//mingw64//bin")
    if os.path.isdir(mingw):
        extra.append(mingw)
    if extra:
        env["PATH"] = os.pathsep.join(extra + [os.environ.get("PATH", "")])
    return env


def run(cmd, **kw):
    print("[执行] " + " ".join(os.path.basename(c) if os.path.isabs(c) else c for c in cmd))
    env = os.environ.copy()
    env.update(toolchain_env())
    env["PATH"] = os.path.join(HERE, "tools", "rust", "cargo", "bin") + os.pathsep + os.path.join("D:\\", "mingw64", "mingw64", "bin") + os.pathsep + env.get("PATH", "")
    r = subprocess.run(cmd, cwd=HERE, env=env, **kw)
    if r.returncode != 0:
        raise SystemExit("[失败] 上一步退出码 %d" % r.returncode)


def build_uefi_loader(kdata):
    """构建 UEFI 装载器并把内核嵌进去；失败不致命（BIOS 盘仍可用）。"""
    efi_dir = os.path.join(HERE, "boot", "efi")
    if not os.path.isdir(efi_dir):
        return None
    venv = os.environ.copy()
    venv.update(toolchain_env())
    cargo = cargo_exe()
    print("[执行] cargo build (x86_64-unknown-uefi) in boot/efi")
    r = subprocess.run([cargo, "build", "--release", "--target", "x86_64-unknown-uefi"],
                       cwd=efi_dir, env=venv)
    if r.returncode != 0:
        print("[警告] UEFI 装载器构建失败，本次 ISO 只有 BIOS 引导")
        return None
    loader = os.path.join(efi_dir, "target", "x86_64-unknown-uefi", "release", "lumi-efi.efi")
    if not os.path.isfile(loader):
        print("[警告] 没找到 " + loader)
        return None
    out = os.path.join(HERE, "build", "BOOTX64.EFI")
    kbin = os.path.join(HERE, "build", "kernel.bin")
    r = subprocess.run([sys.executable, os.path.join(HERE, "tools", "mkefi.py"),
                        "--loader", loader, "--kernel", kbin, "--output", out])
    if r.returncode != 0:
        print("[警告] 内核嵌入失败，本次 ISO 只有 BIOS 引导")
        return None
    # UEFI 固件不加载裸 PE：它把 El Torito 的映像当磁盘，挂文件系统找
    # \EFI\BOOT\BOOTX64.EFI。所以再包一层 FAT16。
    fat = os.path.join(HERE, "build", "efi.img")
    r = subprocess.run([sys.executable, os.path.join(HERE, "tools", "mkfat.py"),
                        "--file", "EFI/BOOT/BOOTX64.EFI=" + out,
                        "--output", fat, "--mb", "4"])
    if r.returncode != 0:
        print("[警告] FAT 映像生成失败，本次 ISO 只有 BIOS 引导")
        return None
    # 真机 UEFI 的实际启动路径：GPT → ESP(FAT) → \EFI\BOOT\BOOTX64.EFI。
    # 产出的 .img 可以直接 dd 到 U 盘。
    disk = os.path.join(HERE, "build", "efi-disk.img")
    gpt = [sys.executable, os.path.join(HERE, "tools", "mkgpt.py"),
           "--esp", fat, "--output", disk, "--mb", "64"]
    # 真机只有一块 U 盘：把 ext2 根文件系统与 LumiFS 数据盘一起装进去。
    sysimg = os.path.join(HERE, "build", "disk.img")
    dataimg = os.path.join(HERE, "build", "disk2.img")
    if os.path.isfile(sysimg) and os.path.isfile(dataimg):
        gpt += ["--ext2", sysimg, "--lumi", dataimg]
    r = subprocess.run(gpt)
    if r.returncode != 0:
        print("[警告] GPT 磁盘生成失败")
        return None
    return fat, disk


def build_bench():
    """构建跨系统微基准（M11）：裸 ELF，内核与 Linux 都能跑。

    先 `cargo build` 出**扁平映像**（x86_64-unknown-none 的默认输出就是 binary），
    再用 `tools/mkelf.py` 包成 ET_EXEC 的 ELF —— 内核的装载器只认这种形态。
    """
    d = os.path.join(HERE, "bench")
    if not os.path.isdir(d):
        return None
    venv = os.environ.copy()
    venv.update(toolchain_env())
    cargo = cargo_exe()
    print("[执行] cargo build (bench, x86_64-unknown-none)")
    r = subprocess.run([cargo, "build", "--release"], cwd=d, env=venv,
                       stdout=subprocess.DEVNULL)
    flat = os.path.join(d, "target", "x86_64-unknown-none", "release", "bench")
    if r.returncode != 0 or not os.path.isfile(flat):
        print("[警告] bench 构建失败，本次镜像不含 /bin/bench")
        return None
    out = os.path.join(HERE, "build", "bench.elf")
    r = subprocess.run([sys.executable, os.path.join(HERE, "tools", "mkelf.py"),
                        "--input", flat, "--output", out,
                        "--base", "0x400000", "--flags", "5"])
    if r.returncode != 0:
        print("[警告] bench 打包 ELF 失败")
        return None
    return out


def build_kernel():
    cargo = cargo_exe()
    run([cargo, "build", "-p", "lumikernel", "--target", "x86_64-unknown-none", "--release"])
    src = os.path.join(HERE, "target", "x86_64-unknown-none", "release", "lumikernel")
    if not os.path.isfile(src):
        raise SystemExit("[错误] 没找到内核映像: " + src)
    os.makedirs(os.path.join(HERE, "build"), exist_ok=True)
    dst = os.path.join(HERE, "build", "kernel.bin")
    shutil.copyfile(src, dst)
    return dst


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ver", default="0.1")
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--no-iso", action="store_true", help="只出软盘镜像，便于快速验证")
    a = ap.parse_args()

    build = os.path.join(HERE, "build")
    outd = os.path.join(HERE, "out")
    os.makedirs(build, exist_ok=True)
    os.makedirs(outd, exist_ok=True)

    kernel_bin = os.path.join(build, "kernel.bin")
    if not a.no_build:
        kernel_bin = build_kernel()
    kdata = open(kernel_bin, "rb").read()
    if not kdata:
        raise SystemExit("[错误] 内核映像为空")
    if len(kdata) > KERNEL_MAX:
        raise SystemExit("[错误] 内核 %d 字节超过装载上限 %d" % (len(kdata), KERNEL_MAX))
    ksectors = (len(kdata) + SECTOR - 1) // SECTOR
    if ksectors > 2880 - KERNEL_LBA:
        raise SystemExit("[错误] 内核 %d 扇区超过软盘仿真容量" % ksectors)

    # stage1 的头部固定读 1120 扇区（573 KiB），尾部装载数是 KERNEL_SECTORS-1120。
    # 映像比这个窗口小时必须按 1120 上报，否则差值下溢成 6 万多扇区的死循环
    # ——症状是 stage2 打完 'k' 就再也没动静。
    load_sectors = max(ksectors, 1120)

    # stage2
    stage2_bin = os.path.join(build, "stage2.bin")
    run([sys.executable, os.path.join("tools", "xasm16.py"), os.path.join("boot", "stage2.asm"), stage2_bin])
    s2 = open(stage2_bin, "rb").read()
    if len(s2) > STAGE2_SECTORS * SECTOR:
        raise SystemExit("[错误] stage2 %d 字节超过 %d 容量" % (len(s2), STAGE2_SECTORS * SECTOR))

    # stage1：注入内核扇区数
    src_path = os.path.join(HERE, "boot", "boot.asm")
    src = open(src_path, "r", encoding="utf-8").read()
    if "@KERNEL_SECTORS@" not in src:
        raise SystemExit("[错误] boot.asm 缺少 @KERNEL_SECTORS@ 占位符")
    gen = os.path.join(build, "stage1.gen.asm")
    open(gen, "w", encoding="utf-8").write(src.replace("@KERNEL_SECTORS@", str(load_sectors)))
    boot_bin = os.path.join(build, "boot.bin")
    run([sys.executable, os.path.join("tools", "xasm16.py"), gen, boot_bin])
    boot = open(boot_bin, "rb").read()
    if len(boot) != 512 or boot[-2:] != b"\x55\xAA":
        raise SystemExit("[错误] stage1 不合法：%d 字节" % len(boot))

    # 软盘镜像
    img = bytearray(FLOPPY_SIZE)
    img[0:SECTOR] = boot
    off2 = STAGE2_LBA * SECTOR
    img[off2:off2 + len(s2)] = s2
    offk = KERNEL_LBA * SECTOR
    img[offk:offk + len(kdata)] = kdata
    floppy = os.path.join(build, "floppy.img")
    open(floppy, "wb").write(bytes(img))

    print()
    print("-" * 62)
    print("[布局] stage1  LBA 0    %6d B" % len(boot))
    print("[布局] stage2  LBA 1    %6d B (@0x8000)" % len(s2))
    print("[布局] kernel  LBA 64   %6d B (@0x100000, %d 扇区)" % (len(kdata), ksectors))
    print("-" * 62)

    if a.no_iso:
        print("[完成] 软盘镜像: " + floppy)
        return 0

    # 跨系统微基准（M11）：必须在 mkdisk 之前产出，镜像里才会有 /bin/bench
    build_bench()

    # FAT 卷（M12 fat-for-kfat）：给 kfat 驱动做端到端验证。
    # 用 hello.txt 的同样内容当文件，自检里读回来逐字节比对。
    _fat_out = os.path.join(build, "fat.img")
    _hello = os.path.join(HERE, "build", "hello-for-fat.txt")
    with open(_hello, "wb") as _f:
        _f.write(b"this line came from an ext2 file on disk.\n"
                 b"second line: 0123456789 abcdefghij\n")
    if subprocess.run([sys.executable, os.path.join(HERE, "tools", "mkfat.py"),
                       "--output", _fat_out, "--mb", "8",
                       "--file", "FAT.TXT=" + _hello]).returncode != 0:
        print("[警告] mkfat 失败，FAT 自检会跳过")

    # 系统盘：用户程序 → ELF → ext2 镜像。跟内核一起出，避免"内核跑的是旧程序"。
    run([sys.executable, os.path.join("tools", "mkdisk.py"), "--out", build])

    # UEFI 装载器：真机只提供 UEFI 固件，BIOS 那条链在真机上根本跑不到。
    # 单独一个 crate（目标 x86_64-unknown-uefi），产物再被打包器塞进内核。
    efi_bin = build_uefi_loader(kdata)

    iso = os.path.join(outd, "LumiKernel-%s.iso" % a.ver)
    mkiso_args = [sys.executable, os.path.join("tools", "mkiso.py"),
                  "--boot-image", floppy, "--output", iso, "--volume-id", "LUMIKERNEL"]
    if efi_bin:
        mkiso_args += ["--efi-image", efi_bin[0]]
    run(mkiso_args)
    run([sys.executable, os.path.join("tools", "checkiso.py"), iso])
    print("[交付] " + iso)
    if efi_bin:
        # UEFI 启动盘（GPT + ESP）。真机把它写进 U 盘即可；虚拟机里当硬盘挂。
        dst = os.path.join(outd, "LumiKernel-%s-efi.img" % a.ver)
        shutil.copyfile(efi_bin[1], dst)
        print("[交付] " + dst + "（GPT+ESP，dd 到 U 盘或当硬盘挂都能 UEFI 启动）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
