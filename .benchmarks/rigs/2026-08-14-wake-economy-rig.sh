#!/bin/bash
# Wake-economy campaign rig (docs/design-il-wake-economy.md, PR 1).
#
# One row = one clean cell: fresh TCP devsub substrate (zram ages across
# format), format, -o interception mount, PRE-FILL (the 2026-08-14 rule:
# a random-overwrite row measures the patch path only on a WRITTEN
# fileset — a row containing its own layout pass is INVALID), then the
# measured fio window with per-row columns:
#
#   IOPS | p99 | wake_writes/s (the daemon cqe FUTEX_WAKE rate) |
#   wake_gauge = writes/(writes+elided+collapsed) | collapsed |
#   pass_flushes | PSI-some% (CPU pressure share of the window) |
#   engagement (ipc_w or ipc_r delta == fio ops) | tick_delta (must be 0)
#
# The `il_slot_reroutes` column is CHARTERED here and lands with PR 2
# (it is a ClientStatsPage field; PR 1 is bump-free) — until then the
# column prints "n/a".
#
# Grid (the design's baseline set): 32x8 16x16 8x32 4x64 1x32 1x1 32x32
# randwrite + 32x8 randread. Usage:
#   sudo .benchmarks/rigs/2026-08-14-wake-economy-rig.sh <daemon> <shim> [tag]
# Env: SQZ_RIG_RUNTIME (s, default 25), SQZ_RIG_ROWS (override grid,
# space-separated NJxQD[:r] entries, :r = randread), DAEMON_ENV/CLIENT_ENV
# pass-throughs (A/B levers).
set -euo pipefail
BIN=${1:?daemon binary}; SHIM=${2:?shim .so}; TAG=${3:-base}
RUNTIME=${SQZ_RIG_RUNTIME:-25}
ROWS=${SQZ_RIG_ROWS:-"32x8 16x16 8x32 4x64 1x32 1x1 32x32 32x8:r"}
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
META="sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1"
DATA="sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1"
MNT=/mnt/squeezefs

snap() { python3 -c "
import json; m=json.load(open('$MNT/.stats'))['metrics']
print(m.get('ipc_cqe_wake_writes',0), m.get('ipc_cqe_wake_elided',0),
      m.get('ipc_cqe_wake_collapsed',0), m.get('ipc_cqe_pass_wake_flushes',0),
      m.get('ipc_ops_write',0), m.get('ipc_ops_read',0),
      m.get('lock_ticked_reregisters',0))"; }

psi() { awk '/^some/ {print $5}' /proc/pressure/cpu | cut -d= -f2; }

cell() {
  local nj=$1 qd=$2 rw=$3 label=$4
  # File sizing: fixed 4 GiB total fileset so process-count rows compare
  # (the 2026-08-14 clean-sweep rule), floor 128m.
  local size_mb=$(( 4096 / nj )); [ "$size_mb" -lt 128 ] && size_mb=128
  umount -f "$MNT" 2>/dev/null || true
  for p in $(pgrep -x squeezefs); do kill -9 "$p" 2>/dev/null || true; done
  sleep 1
  SQZ_DEVSUB_TRANSPORT=tcp "$REPO/tests/dev_substrate.sh" teardown >/dev/null 2>&1 || true
  sleep 1
  SQZ_DEVSUB_TRANSPORT=tcp "$REPO/tests/dev_substrate.sh" create >/dev/null 2>&1
  sleep 3
  rm -rf "${MNT:?}"/* 2>/dev/null || true
  "$BIN" format "$META" "$DATA" --force >/dev/null 2>&1 || { echo "$label FORMAT-FAIL"; return 1; }
  local ok=""
  for _ in 1 2 3 4 5; do
    if env ${DAEMON_ENV:-} "$BIN" mount "$META" "$MNT" --daemon --allow-other -o interception >/dev/null 2>&1; then
      sleep 2; mountpoint -q "$MNT" && { ok=1; break; }
    fi
    sleep 2
  done
  [ -n "$ok" ] || { echo "$label MOUNT-FAIL"; return 1; }

  # Pre-fill: sequential 1M write of the whole fileset, then settle.
  env ${CLIENT_ENV:-} LD_PRELOAD="$SHIM" fio --name=w --directory="$MNT" \
    --size=${size_mb}m --bs=1M --rw=write --numjobs="$nj" --iodepth=4 \
    --ioengine=libaio --direct=1 --group_reporting >/dev/null 2>&1 || true
  sync; sleep 2

  read W0 E0 C0 F0 OW0 OR0 T0 <<<"$(snap)"
  local P0; P0=$(psi)
  local out
  out=$(env ${CLIENT_ENV:-} LD_PRELOAD="$SHIM" fio --name=w --directory="$MNT" \
    --size=${size_mb}m --bs=4k --rw="$rw" --numjobs="$nj" --iodepth="$qd" \
    --ioengine=libaio --direct=1 --time_based --runtime="$RUNTIME" \
    --group_reporting 2>/dev/null)
  read W1 E1 C1 F1 OW1 OR1 T1 <<<"$(snap)"
  local P1; P1=$(psi)

  local iops p99 fio_ops
  iops=$(echo "$out" | grep -oE "IOPS=[0-9.k]+" | head -1)
  p99=$(echo "$out" | grep -E "99.00th" | head -1 | grep -oE "99.00th=\[ *[0-9]+\]" | grep -oE "[0-9]+" | tail -1)
  if [ "$rw" = randread ]; then
    fio_ops=$(echo "$out" | grep -oE "issued rwts: total=[0-9]+" | head -1 | grep -oE "[0-9]+$")
    ops_delta=$((OR1-OR0))
  else
    fio_ops=$(echo "$out" | grep -oE "issued rwts: total=[0-9]+,[0-9]+" | head -1 | sed 's/.*total=//' | cut -d, -f2)
    ops_delta=$((OW1-OW0))
  fi
  local dw=$((W1-W0)) de=$((E1-E0)) dc=$((C1-C0)) df=$((F1-F0))
  local gauge="n/a"
  if [ $((dw+de+dc)) -gt 0 ]; then
    gauge=$(python3 -c "print(f'{$dw/($dw+$de+$dc):.3f}')")
  fi
  local psi_pct
  psi_pct=$(python3 -c "print(f'{($P1-$P0)/(${RUNTIME}*1e6)*100:.1f}')")
  local engage=INVALID
  [ -n "$fio_ops" ] && [ "$ops_delta" = "$fio_ops" ] && engage=exact
  echo "$label: $iops p99=${p99}us wakes/s=$((dw/RUNTIME)) gauge=$gauge collapsed=$dc pass_flushes=$df psi_some=${psi_pct}% reroutes=n/a engage=$engage tick_delta=$((T1-T0))"

  umount -f "$MNT" 2>/dev/null || true
  for p in $(pgrep -x squeezefs); do kill -9 "$p" 2>/dev/null || true; done
  sleep 1
}

echo "== wake-economy rig: tag=$TAG runtime=${RUNTIME}s bin=$BIN shim=$SHIM =="
for row in $ROWS; do
  rw=randwrite; base=${row%:r}
  [ "$row" != "$base" ] && rw=randread
  nj=${base%x*}; qd=${base#*x}
  cell "$nj" "$qd" "$rw" "$TAG-${base}$([ "$rw" = randread ] && echo -r || true)" || true
done
