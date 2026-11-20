#!/usr/bin/env bash
# UEFI 引导测试（M10）
#   用法: scripts/vmtest-efi.sh <iso 或 .img> [等待秒数] [截图名] [sata]
#   第一个参数以 .img 结尾 → 当作 GPT 启动盘挂成硬盘（真机 UEFI 的真实路径）
#   否则                    → 当作 ISO 挂成光驱
#   sata=1 → 所有盘都挂 SATA(AHCI)，用来验证自有 AHCI 驱动
set -u

# VBoxManage：优先用环境变量，其次 PATH，最后才是 Windows 的默认安装路径
VBM="${LUMI_VBM:-}"
if [ -z "$VBM" ]; then
  if command -v VBoxManage >/dev/null 2>&1; then VBM="VBoxManage"
  else VBM="/d/Program Files/VBoxManage.exe"; fi
fi
VM="${LUMI_VM:-LumiKernelUEFI}"

# LUMI_VM_FRESH=1：每次删掉重建。
# 用于 VBox 的 UEFI NVRAM 启动项在 poweroff 后丢失（表现为下一轮从 PXE 启动）的场合——
# 实测重建后第一轮总能启动，比猜 EFI 变量可靠。
if [ "${LUMI_VM_FRESH:-0}" = "1" ]; then
  "$VBM" unregistervm "$VM" --delete >/dev/null 2>&1 || true
fi

# 上一轮如果被超时打断，VM 会残留在 Running/Saved/stopping。
# 这里先强制关掉并丢弃状态——否则本轮所有 VBoxManage 都会报 "already locked"。
if "$VBM" showvminfo "$VM" --machinereadable >/dev/null 2>&1; then
  state=$("$VBM" showvminfo "$VM" --machinereadable 2>/dev/null | sed -n 's/^VMState="\(.*\)"/\1/p')
  if [ "$state" != "poweroff" ] && [ -n "$state" ]; then
    "$VBM" controlvm "$VM" poweroff >/dev/null 2>&1
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      sleep 1
      st=$("$VBM" showvminfo "$VM" --machinereadable 2>/dev/null | sed -n 's/^VMState="\(.*\)"/\1/p')
      [ "$st" = "poweroff" ] || [ -z "$st" ] && break
    done
    [ "$st" = "saved" ] && "$VBM" discardstate "$VM" >/dev/null 2>&1
  fi
  "$VBM" discardstate "$VM" >/dev/null 2>&1 || true
fi
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) ROOT="$(cygpath -m "$ROOT")" ;; esac
MEDIA="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
WAIT="${2:-8}"
SHOT="${3:-uefi}"
USE_SATA="${4:-0}"
SER="$ROOT/build/serial-efi.log"

case "$MEDIA" in
  *.img) BOOT_KIND="disk" ;;
  *)     BOOT_KIND="dvd" ;;
esac

: > "$SER"

if ! "$VBM" showvminfo "$VM" >/dev/null 2>&1; then
  echo "[vm] 首次创建 $VM（EFI 固件）"
  "$VBM" createvm --name "$VM" --ostype Other_64 --register >/dev/null
  "$VBM" modifyvm "$VM" --memory 1024 --vram 16 --firmware efi \
      --boot1 disk --boot2 dvd --boot3 none --boot4 none \
      --audio-driver none --nic1 nat --nictype1 virtio --cableconnected1 on --macaddress1 525400123456 --usb off \
      --triple-fault-reset on --largepages off --nested-paging off \
      --uart1 0x3F8 4 --uartmode1 file "$SER" >/dev/null
fi
"$VBM" modifyvm "$VM" --firmware efi --boot1 disk --boot2 dvd --cpus 4 \
    --uart1 0x3F8 4 --uartmode1 file "$SER" >/dev/null

"$VBM" controlvm "$VM" poweroff >/dev/null 2>&1
sleep 2

CTL="IDE"
if [ "$USE_SATA" = "1" ]; then
  CTL="SATA"
  if ! "$VBM" showvminfo "$VM" --machinereadable | grep -q 'SATA'; then
    "$VBM" storagectl "$VM" --name SATA --add sata --controller IntelAhci --portcount 4 >/dev/null
  fi
else
  if ! "$VBM" showvminfo "$VM" --machinereadable | grep -q '"IDE"'; then
    "$VBM" storagectl "$VM" --name IDE --add ide --controller PIIX4 >/dev/null
  fi
fi
echo "[vm] 存储控制器: $CTL"

# 两个控制器都清一遍：上一次可能挂在 IDE 上，这次要挂 SATA（或反过来）。
# 上一轮的 VDI 可能还挂着（VM 卡死时删不掉）：先用 VBox 自己的命令关掉并删除
# 只清一个的话，旧介质还挂在另一条总线上，convertfromraw 出来的新 VDI 会报
# "already attached to this virtual machine"。
for c in IDE SATA; do
  for p in 0 1 2 3; do
    for d in 0 1; do
      "$VBM" storageattach "$VM" --storagectl $c --port $p --device $d --type hdd --medium none >/dev/null 2>&1
      "$VBM" storageattach "$VM" --storagectl $c --port $p --device $d --type dvddrive --medium none >/dev/null 2>&1
    done
  done
done

# 注意：VDI 文件名必须和 BIOS 那台虚拟机不同。VBox 的介质注册表按 UUID 记账，
# 两个 VM 共用同一个 .vdi 会在 attach 时报 "UUID 不匹配"（踩过）。
mkvdi() {
  "$VBM" closemedium disk "$2" --delete >/dev/null 2>&1
  python -c "import os,sys; os.path.exists(sys.argv[1]) and os.remove(sys.argv[1])" "$2" 2>/dev/null || true
  "$VBM" convertfromraw "$1" "$2" --format VDI >/dev/null
}

attach_hdd() { # <port> <device> <raw>
  # VDI 名带上 VM 名：卡死的旧 VM 会锁住同名介质（实测 10 分钟都不释放）
  local vdi="${3%.img}-${VM}.vdi"
  if [ -f "$3" ]; then
    mkvdi "$3" "$vdi"
    "$VBM" storageattach "$VM" --storagectl "$CTL" --port "$1" --device "$2" \
        --type hdd --medium "$vdi" >/dev/null
    echo "[vm] $(basename "$3") -> $CTL:$1:$2"
  fi
}

# 两种布局：
#   boot=dvd  → 光驱起（BIOS 风格），ext2/LumiFS 各自一块盘，偏移为 0
#   boot=disk → **单盘**：UEFI 盘里已经装了 ESP + ext2 + LumiFS 三个分区，
#               这正是真机插一块 U 盘的情形
if [ "$BOOT_KIND" = "disk" ]; then
  attach_hdd 0 0 "$MEDIA"
else
  attach_hdd 0 0 "$ROOT/build/disk.img"
  if [ "$USE_SATA" = "1" ]; then
    attach_hdd 1 0 "$ROOT/build/disk2.img"
  else
    attach_hdd 0 1 "$ROOT/build/disk2.img"
  fi
fi

if [ "$BOOT_KIND" = "dvd" ]; then
  if [ "$USE_SATA" = "1" ]; then
    "$VBM" storageattach "$VM" --storagectl "$CTL" --port 2 --device 0 --type dvddrive --medium "$MEDIA" >/dev/null
  else
    "$VBM" storageattach "$VM" --storagectl "$CTL" --port 1 --device 1 --type dvddrive --medium "$MEDIA" >/dev/null
  fi
  echo "[vm] ISO -> 光驱"
fi

echo "[vm] 启动 $VM（EFI, boot=$BOOT_KIND）"
"$VBM" startvm "$VM" --type headless >/dev/null
sleep "$WAIT"
"$VBM" controlvm "$VM" screenshotpng "$ROOT/build/$SHOT.png" >/dev/null 2>&1 || true
# 关机前先抓一份串口快照：VBox 的 file 后端在 poweroff 时会丢掉还没刷盘的尾巴
cp "$SER" "$SER.snapshot" 2>/dev/null || true

echo "[vm] ACPI 软关机"
"$VBM" controlvm "$VM" acpipowerbutton >/dev/null 2>&1
for _ in $(seq 1 15); do
  st=$("$VBM" showvminfo "$VM" --machinereadable 2>/dev/null | grep '^VMState=' | cut -d'"' -f2)
  [ "$st" = "poweroff" ] && break
  sleep 1
done
st=$("$VBM" showvminfo "$VM" --machinereadable 2>/dev/null | grep '^VMState=' | cut -d'"' -f2)
[ "$st" != "poweroff" ] && "$VBM" controlvm "$VM" poweroff >/dev/null 2>&1

echo "=========== 串口日志 ==========="
cat "$SER.snapshot" 2>/dev/null || cat "$SER"
echo "================================"