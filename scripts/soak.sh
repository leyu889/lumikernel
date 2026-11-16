#!/usr/bin/env bash
# 长跑（S3）：重启式采样，跨轮比较空闲帧 / 堆占用 / 判定斜率。
#
# 用法：
#   scripts/soak.sh --rounds 4320 --interval 600      # 30 天 × 每 10 分钟一轮
#   scripts/soak.sh --rounds 5 --interval 0           # 冒烟
#
# 为什么是"重启式"：内核自检跑完就进入 [LIVE] 空转，guest 里没有常驻 workload，
# 一次启动采不到多个点。所以这里**每轮重新启动一次**做完整自检，
# 把每轮的"空闲帧 / 堆已用 / 僵尸数"记下来，跨轮做回归。
# 真正的"不重启连续 30 天"需要 guest 侧常驻压测循环（M12 的第一项）。
#
# 输出：build/soak.csv（每轮一行）+ 末尾的回归结论。
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# VBoxManage 是 Windows 程序，Git Bash 的 `/e/...` 它不认，转成 `E:/...`
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) ROOT="$(cygpath -m "$ROOT")" ;; esac
IMG="out/LumiKernel-0.1-efi.img"
ROUNDS=5
INTERVAL=0
CSV="$ROOT/build/soak.csv"

while [ $# -gt 0 ]; do
  case "$1" in
    --rounds) ROUNDS="$2"; shift 2 ;;
    --interval) INTERVAL="$2"; shift 2 ;;
    --image) IMG="$2"; shift 2 ;;
    *) echo "未知参数 $1"; exit 2 ;;
  esac
done

cd "$ROOT" || exit 1
[ -f "$CSV" ] || echo "round,free_frames,heap_used,slope_frames,slope_heap,verdict" > "$CSV"

echo "[soak] 计划 $ROUNDS 轮，间隔 ${INTERVAL}s；镜像 $IMG"
i=1
while [ "$i" -le "$ROUNDS" ]; do
  echo "[soak] === 第 $i/$ROUNDS 轮 ==="
  out=$(timeout 420 bash scripts/vmtest-efi.sh "$IMG" 80 "soak-$i" 1 2>&1)
  log="$ROOT/build/serial-efi.log"

  frames=$(grep -oE "reap: frames [0-9]+ -> [0-9]+" "$log" | tail -1 | grep -oE "[0-9]+$")
  heap=$(grep -oE "heap_used=[0-9]+" "$log" | tail -1 | grep -oE "[0-9]+$")
  slope=$(grep -oE "帧斜率 -?[0-9]+‰ 堆斜率 -?[0-9]+‰" "$log" | tail -1)
  sf=$(echo "$slope" | grep -oE "\-?[0-9]+" | head -1)
  sh=$(echo "$slope" | grep -oE "\-?[0-9]+" | tail -1)
  verdict=$(echo "$out" | grep -oE "M11 self-test [0-9]+/[0-9]+ passed" | tail -1)

  [ -z "${frames:-}" ] && frames=0
  [ -z "${heap:-}" ] && heap=0
  [ -z "${sf:-}" ] && sf=0
  [ -z "${sh:-}" ] && sh=0
  echo "$i,$frames,$heap,$sf,$sh,\"${verdict:-无汇总}\"" >> "$CSV"
  cp "$log" "$ROOT/build/soak-round$i.log" 2>/dev/null

  i=$((i + 1))
  [ "$i" -le "$ROUNDS" ] && [ "$INTERVAL" -gt 0 ] && sleep "$INTERVAL"
done

echo
echo "[soak] 各轮空闲帧："
awk -F, 'NR>1 {printf "  第 %s 轮: %s 帧\n", $1, $2}' "$CSV"
echo "[soak] 明细见 build/soak.csv"
python - "$CSV" <<'PY'
import sys
rows = [l.strip().split(',') for l in open(sys.argv[1]) if l.strip()][1:]
vals = [int(r[1]) for r in rows if r[1].isdigit() and int(r[1]) > 0]
if len(vals) < 2:
    print('[soak] 采样点不足，无法判定'); raise SystemExit(0)
delta = vals[-1] - vals[0]
print('[soak] 首轮 %d → 末轮 %d（差 %+d 帧）' % (vals[0], vals[-1], delta))
print('[soak] 判定：%s' % ('零漂移' if delta == 0 else '有漂移，需要查'))
PY
