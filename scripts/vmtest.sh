#!/usr/bin/env bash
# 在 VirtualBox 里无头跑一次内核 ISO，抓串口日志 + 截图。
# 用法: scripts/vmtest.sh <iso> [等待秒数] [截图名]
set -u

# VBoxManage：优先用环境变量，其次 PATH，最后才是 Windows 的默认安装路径
VBM="${LUMI_VBM:-}"
if [ -z "$VBM" ]; then
  if command -v VBoxManage >/dev/null 2>&1; then VBM="VBoxManage"
  else VBM="/d/Program Files/VBoxManage.exe"; fi
fi
VM="LumiKernelDev"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) ROOT="$(cygpath -m "$ROOT")" ;; esac
ISO="$1"
WAIT="${2:-6}"
SHOT="${3:-run}"
SER="$ROOT/build/serial.log"

: > "$SER"   # 必须先建空文件，否则 VBox 的 file 后端不落盘

if ! "$VBM" showvminfo "$VM" >/dev/null 2>&1; then
  echo "[vm] 首次创建 $VM"
  "$VBM" createvm --name "$VM" --ostype Other_64 --register >/dev/null
  "$VBM" modifyvm "$VM" --memory 512 --vram 16 --firmware bios \
      --boot1 dvd --boot2 none --boot3 none --boot4 none \
      --audio none --nic1 nat --nictype1 virtio --cableconnected1 on --macaddress1 525400123456 --usb off \
      --triple-fault-reset on --largepages off --nested-paging off \
      --uart1 0x3F8 4 --uartmode1 file "$SER" >/dev/null
  "$VBM" storagectl "$VM" --name IDE --add ide --controller PIIX4 >/dev/null
fi

"$VBM" storageattach "$VM" --storagectl IDE --port 1 --device 0 \
    --type dvddrive --medium none >/dev/null 2>&1
"$VBM" storageattach "$VM" --storagectl IDE --port 1 --device 0 \
    --type dvddrive --medium "$ISO" >/dev/null
# 系统盘（ext2 镜像）：先转成 VDI，再挂到主通道主盘。
# 内核 M5 要读它来挂载文件系统、加载 ELF；没有它 ata/ext2/elf 三项会明确报 FAIL。
DISK_IMG="$ROOT/build/disk.img"
DISK_VDI="$ROOT/build/disk.vdi"
if [ -f "$DISK_IMG" ]; then
  # 先确保虚拟机是关的：运行中 storageattach 会失败
  "$VBM" controlvm "$VM" poweroff >/dev/null 2>&1
  sleep 2
  "$VBM" storageattach "$VM" --storagectl IDE --port 0 --device 0 --type hdd --medium none >/dev/null 2>&1
  # 从介质注册表里注销旧的 VDI 再删文件：直接在文件系统上 rm，
  # VBox 的注册表会留下悬空条目，下次 convertfromraw 到同一路径直接报 E_FAIL。
  "$VBM" closemedium disk "$DISK_VDI" --delete >/dev/null 2>&1
  rm -f "$DISK_VDI"
  "$VBM" convertfromraw "$DISK_IMG" "$DISK_VDI" --format VDI >/dev/null
  "$VBM" storageattach "$VM" --storagectl IDE --port 0 --device 0 --type hdd --medium "$DISK_VDI" >/dev/null
  echo "[vm] 系统盘已挂载: $(basename "$DISK_IMG")"
else
  echo "[vm] 警告: 没有 build/disk.img，ext2/elf 自检会失败"
fi

# 数据盘（LumiFS，可写）：挂到主通道从盘。没有它 M8 的 lumifs 自检会明确报 FAIL。
DATA_IMG="$ROOT/build/disk2.img"
DATA_VDI="$ROOT/build/disk2.vdi"
if [ -f "$DATA_IMG" ]; then
  "$VBM" controlvm "$VM" poweroff >/dev/null 2>&1
  sleep 2
  "$VBM" storageattach "$VM" --storagectl IDE --port 0 --device 1 --type hdd --medium none >/dev/null 2>&1
  "$VBM" closemedium disk "$DATA_VDI" --delete >/dev/null 2>&1
  rm -f "$DATA_VDI"
  "$VBM" convertfromraw "$DATA_IMG" "$DATA_VDI" --format VDI >/dev/null
  "$VBM" storageattach "$VM" --storagectl IDE --port 0 --device 1 --type hdd --medium "$DATA_VDI" >/dev/null
  echo "[vm] 数据盘已挂载: $(basename "$DATA_IMG")（未格式化，首次挂载由内核格式化）"
else
  echo "[vm] 警告: 没有 build/disk2.img，lumifs 自检会失败"
fi

"$VBM" modifyvm "$VM" --uartmode1 file "$SER" >/dev/null

echo "[vm] 启动 $VM"
"$VBM" startvm "$VM" --type headless >/dev/null
sleep "$WAIT"
"$VBM" controlvm "$VM" screenshotpng "$ROOT/build/$SHOT.png" >/dev/null 2>&1 || true

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
cat "$SER"
echo "================================"
