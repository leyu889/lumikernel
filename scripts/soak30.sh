#!/usr/bin/env bash
# 连续长跑（S3）：把 soak 内核跑起来，让它一直跑，期间定期落盘心跳。
#
# 用法：
#   scripts/soak30.sh --minutes 30          # 冒烟 / 本次验收
#   scripts/soak30.sh --minutes 43200       # 真正的 30 天（= 30 × 24 × 60）
#
# 前提：先用长跑模式构建镜像
#   LUMI_SOAK=1 python build.py             # 采样间隔 10 分钟（正式）
#   LUMI_SOAK=1 LUMI_SOAK_FAST=1 python build.py   # 采样间隔 1 秒（演示/验收）
#
# 为什么单独一台虚拟机：长跑要"不被打断"，而 `vmtest-efi.sh` 每次都会
# 关机重启。两者共用一台机器会互相踩（VBox 的介质注册表按 UUID 记账）。
set -u

# VBoxManage：优先用环境变量，其次 PATH，最后才是 Windows 的默认安装路径
VBM="${LUMI_VBM:-}"
if [ -z "$VBM" ]; then
  if command -v VBoxManage >/dev/null 2>&1; then VBM="VBoxManage"
  else VBM="/d/Program Files/VBoxManage.exe"; fi
fi
VM="${LUMI_SOAK_VM:-LumiKernelSoak}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) ROOT="$(cygpath -m "$ROOT")" ;; esac
IMG="$ROOT/out/LumiKernel-0.1-efi.img"
MINUTES=30
VDI="$ROOT/build/soak-disk.vdi"
SER="$ROOT/build/serial-soak.log"

while [ $# -gt 0 ]; do
  case "$1" in
    --minutes) MINUTES="$2"; shift 2 ;;
    --image) IMG="$2"; shift 2 ;;
    *) echo "未知参数 $1"; exit 2 ;;
  esac
done

[ -f "$IMG" ] || { echo "[soak] 找不到 $IMG"; exit 1; }

# ---- 虚拟机 ----
if ! "$VBM" showvminfo "$VM" >/dev/null 2>&1; then
  echo "[soak] 创建 $VM"
  "$VBM" createvm --name "$VM" --ostype Other_64 --register >/dev/null
  "$VBM" modifyvm "$VM" --memory 1024 --vram 16 --firmware efi \
      --boot1 disk --boot2 none --boot3 none --boot4 none \
      --audio-driver none --nic1 none --usb off --cpus 4 \
      --uart1 0x3F8 4 --uartmode1 file "$SER" >/dev/null
  "$VBM" storagectl "$VM" --name SATA --add sata --controller IntelAhci --portcount 4 >/dev/null
fi
"$VBM" modifyvm "$VM" --firmware efi --boot1 disk --cpus 4 \
    --uart1 0x3F8 4 --uartmode1 file "$SER" >/dev/null

"$VBM" controlvm "$VM" poweroff >/dev/null 2>&1
sleep 2
"$VBM" storageattach "$VM" --storagectl SATA --port 0 --device 0 --type hdd --medium none >/dev/null 2>&1
"$VBM" closemedium disk "$VDI" --delete >/dev/null 2>&1
rm -f "$VDI"
"$VBM" convertfromraw "$IMG" "$VDI" --format VDI >/dev/null
"$VBM" storageattach "$VM" --storagectl SATA --port 0 --device 0 --type hdd --medium "$VDI" >/dev/null

: > "$SER"
echo "[soak] 启动 $VM（$MINUTES 分钟）"
"$VBM" startvm "$VM" --type headless >/dev/null

# ---- 心跳：每分钟记一次行数与最后一采样 ----
END=$(( $(date +%s) + MINUTES * 60 ))
while [ "$(date +%s)" -lt "$END" ]; do
  sleep 60
  lines=$(wc -l < "$SER" 2>/dev/null || echo 0)
  last=$(grep "^SOAK r=" "$SER" 2>/dev/null | tail -1)
  echo "[soak] $(date +%H:%M:%S) 行数=$lines ${last}"
  # 每 10 分钟留一份快照，防止最后关机丢尾巴
  if [ $(( $(date +%s) % 600 )) -lt 60 ]; then
    cp "$SER" "$SER.snapshot" 2>/dev/null || true
  fi
done

echo "[soak] 时间到，取最后一份日志并关机"
cp "$SER" "$SER.snapshot" 2>/dev/null || true
"$VBM" controlvm "$VM" poweroff >/dev/null 2>&1
sleep 3

echo "[soak] 统计"
grep -c "^SOAK r=" "$SER.snapshot" 2>/dev/null || echo 0
python "$ROOT/scripts/soak-report.py" "$SER.snapshot"
