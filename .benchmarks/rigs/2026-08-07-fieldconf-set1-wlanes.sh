#!/usr/bin/env bash
# Field-confirmation Set 1 — write-lane fan-out A/B (2026-08-07,
# perf/field-confirmations-0807). Runs ON the field host (squeeze-test,
# 32 CPUs, 5 data namespaces -> derived data_write_lanes = 6) per
# `.benchmarks/2026-08-07-write-lane-fanout.md` §6.
#
# Legs A-B-B-A (A = derived lanes, B = SQUEEZEFS_NVME_WRITE_LANES=1 — the
# shipped pre-fanout posture) + ONE labeled armed variant AZ
# (SQUEEZEFS_FUSE_ZC=1 + derived lanes — the compose row, separate label).
# Fresh format + fresh fio dir per leg (A-B-B-A aging rule).
#
# Row: fio libaio seq-write bs=1M numjobs=16 iodepth=8 nrfiles=8 size=8g
# time_based 60 s + 10 s ramp, end_fsync=1 (FIO ENGINE POLICY compliant —
# `.benchmarks/2026-08-07-fio-engine-policy.md`).
#
# Per-leg instruments (engagement gates FATAL):
#   * data_write_lanes gauge (A: 6 expected on this host; B: 1)
#   * data_write_lane_submits per-device per-lane deltas (A: >1 lane moves
#     on every data device; B: lane 0 only)
#   * hot nvme-tcp connections (ss -ti bytes_acked deltas mid-row, TX dir)
#   * write_pipeline_phase_ns.dma mode (delta histogram)
#   * governor: depth_target vs base, probe_ups/backoffs deltas
#   * amplification: data-namespace /proc/diskstats delta / fio io_bytes
#     (device window aligned to ramp end via the 5 s sampler) + wareq-sz
#   * thirds flatness (sustained-state law) from the diskstats sampler
#   * P0 smoke per leg: cp 32MiB && sync f, md5 x3
#   * tripwires: data_dma_fence_refusals flat 0
#
# VENUE STATIONARITY (first-run lesson, this campaign): the memory-backed
# null_blk data namespaces are a CONSUMABLE venue — page population across
# legs decays throughput monotonically (27 -> 19 GB/s over 5 legs on the
# first run, both brackets disagreeing = the ordering-artifact signature).
# RESET_PER_LEG=1 (default) runs the venue's own reset instrument
# (/scratch/tmp/cluster_reset_v4.sh) before EVERY leg, so each leg starts
# from the reference state the 35.31 GB/s baseline was measured on; device
# names are re-resolved from NQNs after each reset (names can shift).
set -u

BIN="${BIN:-/scratch/tmp/squeezefs}"
MNT="${MNT:-/scratch/tmp/test}"
META="${META:-sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1}"
DATA="${DATA:-sqdata:///dev/nvme10n1,/dev/nvme12n1,/dev/nvme14n1,/dev/nvme16n1,/dev/nvme18n1}"
DATA_BLK=(nvme10n1 nvme12n1 nvme14n1 nvme16n1 nvme18n1)
OUT="${OUT:-/scratch/tmp/fieldconf-0807/set1}"
LOGDIR=/scratch/tmp/logs
LEGS=(A B B A Z)   # Z = armed variant (labeled separately, not in the A/B medians)
[ -n "${LEGS_STR:-}" ] && read -r -a LEGS <<<"$LEGS_STR"   # e.g. LEGS_STR="B A A B"
LEG_BASE="${LEG_BASE:-0}"   # tag offset for continuation passes
RUNTIME="${RUNTIME:-60}"
RAMP="${RAMP:-10}"
RESET_PER_LEG="${RESET_PER_LEG:-1}"
RESET_SCRIPT="${RESET_SCRIPT:-/scratch/tmp/cluster_reset_v4.sh}"
NQN_PREFIX="nqn.2026-07.io.squeezefs"
META_SUFFIXES=(aqr37-m0 aqr38-m0 aqr39-m0 aqs38-m0 aqs39-m0)
DATA_SUFFIXES=(aqr37-d0 aqr37-d1 aqr38-d0 aqr38-d1 aqr39-d0)

mkdir -p "$OUT" "$LOGDIR"
fatal() { echo "FATAL: $*" >&2; exit 1; }

jget() { python3 -c "
import json,sys
d=json.load(open(sys.argv[1])); d=d.get('metrics',d)
print(d.get(sys.argv[2],0))" "$1" "$2"; }

do_umount() {
  if mountpoint -q "$MNT"; then
    "$BIN" umount "$MNT" >/dev/null 2>&1 || umount "$MNT" 2>/dev/null || true
  fi
  for _ in $(seq 1 300); do mountpoint -q "$MNT" || break; sleep 2; done
  mountpoint -q "$MNT" && fatal "mountpoint still mounted after 10 min — wedge class, capture tape"
  for _ in $(seq 1 300); do pidof squeezefs >/dev/null 2>&1 || break; sleep 2; done
  pidof squeezefs >/dev/null && fatal "daemon still alive after umount drain — wedge class"
  return 0
}

resolve_devs() { # NQN -> /dev node (names can shift across resets)
  local suffix want found s ns b
  local meta="" data=""
  DATA_BLK=()
  for suffix in "${META_SUFFIXES[@]}" "${DATA_SUFFIXES[@]}"; do
    want="$NQN_PREFIX:$suffix"; found=""
    for _ in $(seq 1 30); do
      for s in /sys/class/nvme-subsystem/nvme-subsys*; do
        [ -e "$s/subsysnqn" ] || continue
        [ "$(cat "$s/subsysnqn")" = "$want" ] || continue
        ns=""
        for c in "$s"/nvme*n*; do
          b=$(basename "$c")
          [[ "$b" =~ ^nvme[0-9]+n[0-9]+$ ]] && ns="$b" && break
        done
        [ -n "$ns" ] && found="$ns" && break
      done
      [ -n "$found" ] && break
      sleep 1
    done
    [ -n "$found" ] || fatal "namespace for $want never appeared"
    case "$suffix" in
      *-m0) meta+="${meta:+,}/dev/$found" ;;
      *)    data+="${data:+,}/dev/$found"; DATA_BLK+=("$found") ;;
    esac
  done
  META="sqmeta://$meta"; DATA="sqdata://$data"
  echo "  resolved meta=$META data=$DATA"
}

do_reset() { # $1 = leg tag — venue reset to the reference state
  echo "  cluster reset (venue stationarity) ..."
  echo YES | bash "$RESET_SCRIPT" >"$OUT/reset-$1.log" 2>&1 \
    || fatal "cluster reset failed (leg $1) — see $OUT/reset-$1.log"
  # the reset leaves its own default mount up — take it down for the leg
  do_umount
  resolve_devs
  printf '%s\n' "${DATA_BLK[@]}" > "$OUT/leg$1-datadevs.txt"
}

format_fs() {
  "$BIN" format "$META" "$DATA" --force >"$OUT/format-$1.log" 2>&1 \
    || fatal "format failed (leg $1) — see $OUT/format-$1.log"
  udevadm settle 2>/dev/null || true
}

do_mount() { # $1 = leg tag, $2 = mode A|B|Z
  local env=()
  case "$2" in
    B) env=(SQUEEZEFS_NVME_WRITE_LANES=1) ;;
    Z) env=(SQUEEZEFS_FUSE_ZC=1) ;;
  esac
  mkdir -p "$MNT"
  env "${env[@]}" "$BIN" mount "$META" "$MNT" --daemon --allow-other \
    --log-file "$LOGDIR/sqz-fieldconf-set1-$1.log" >/dev/null 2>&1
  local ok=0
  for _ in $(seq 1 60); do
    if mountpoint -q "$MNT" && cat "$MNT/.stats" >/dev/null 2>&1; then ok=1; break; fi
    sleep 1
  done
  [ "$ok" = 1 ] || fatal "mount gate failed (leg $1)"
  cat "$MNT/.stats" > "$OUT/leg$1-arm.json"
  local lanes
  lanes=$(jget "$OUT/leg$1-arm.json" data_write_lanes)
  if [ "$2" = B ]; then
    [ "$lanes" = 1 ] || fatal "leg $1: B leg data_write_lanes=$lanes != 1"
  else
    [ "$lanes" -gt 1 ] || fatal "leg $1: leg data_write_lanes=$lanes not >1 — fan-out did not arm"
  fi
  if [ "$2" = Z ]; then
    grep -q "zero-copy" "$LOGDIR/sqz-fieldconf-set1-$1.log" \
      || fatal "leg $1: Z leg did not arm FUSE zc (no zero-copy line in mount log)"
  fi
  echo "leg $1: mounted, data_write_lanes=$lanes"
}

p0_smoke() { # $1 = leg tag — 32 MiB cp && sync f, md5 x3 (task spec)
  local d="$MNT/smoke_$1" a b
  mkdir -p "$d"
  dd if=/dev/urandom of=/tmp/fc_smoke.src bs=1M count=32 status=none
  a=$(md5sum < /tmp/fc_smoke.src | cut -d' ' -f1)
  for i in 1 2 3; do
    cp /tmp/fc_smoke.src "$d/f$i" && sync "$d/f$i" || fatal "leg $1: cp && sync f$i failed"
    b=$(md5sum "$d/f$i" | cut -d' ' -f1)
    [ "$a" = "$b" ] || fatal "leg $1: md5 mismatch on f$i — CORRUPTION"
  done
  rm -rf "$d" /tmp/fc_smoke.src
  echo "leg $1: P0 smoke OK (cp 32MiB && sync x3, md5 clean)"
}

sample_diskstats() { # background: every 5 s until stopfile appears (+ final)
  local tag=$1
  : > "$OUT/leg$tag-diskstats.log"
  while [ ! -e "$OUT/.stop-$tag" ]; do
    { date +%s.%N; grep -E "$(IFS='|'; echo "${DATA_BLK[*]}")" /proc/diskstats; } \
      >> "$OUT/leg$tag-diskstats.log"
    sleep 5
  done
  { date +%s.%N; grep -E "$(IFS='|'; echo "${DATA_BLK[*]}")" /proc/diskstats; } \
    >> "$OUT/leg$tag-diskstats.log"
}

sample_ss() { # background: two ss -ti snapshots at t=25 and t=40 (mid-row)
  local tag=$1
  sleep 25; ss -tin state established '( dport = :4420 )' > "$OUT/leg$tag-ss1.txt" 2>/dev/null
  sleep 15; ss -tin state established '( dport = :4420 )' > "$OUT/leg$tag-ss2.txt" 2>/dev/null
}

run_row() { # $1 = leg tag
  mkdir -p "$MNT/wl_$1"
  fio --name=wl --directory="$MNT/wl_$1" --ioengine=libaio \
    --direct=1 --rw=write --bs=1M --iodepth=8 --numjobs=16 --nrfiles=8 \
    --size=8g --time_based --runtime="$RUNTIME" --ramp_time="$RAMP" \
    --group_reporting --fallocate=none --end_fsync=1 \
    --output-format=json --output="$OUT/leg$1-fio.json" >/dev/null 2>&1 \
    || fatal "leg $1: fio row failed"
}

analyze() { # $1 = leg tag, $2 = mode — engagement FATAL + row table line
  python3 "$OUT/../set1_analyze.py" "$OUT" "$1" "$2" "$RAMP" || fatal "leg $1: engagement/analysis verdict failed"
}

command -v fio >/dev/null || fatal "fio not installed"
[ -x "$BIN" ] || fatal "binary missing: $BIN"

leg="$LEG_BASE"
for mode in "${LEGS[@]}"; do
  leg=$((leg+1)); tag="$leg$mode"
  echo "=== leg $tag ($(date +%H:%M:%S)) ==="
  do_umount
  if [ "$RESET_PER_LEG" = 1 ]; then
    do_reset "$tag"
  else
    printf '%s\n' "${DATA_BLK[@]}" > "$OUT/leg$tag-datadevs.txt"
  fi
  format_fs "$tag"
  do_mount "$tag" "$mode"
  p0_smoke "$tag"
  cat "$MNT/.stats" > "$OUT/leg$tag-before.json"
  rm -f "$OUT/.stop-$tag"
  sample_diskstats "$tag" & DSPID=$!
  sample_ss "$tag" & SSPID=$!
  run_row "$tag"
  cat "$MNT/.stats" > "$OUT/leg$tag-after.json"
  touch "$OUT/.stop-$tag"; wait $DSPID 2>/dev/null; wait $SSPID 2>/dev/null
  analyze "$tag" "$mode"
  do_umount
done
echo "set1 complete — artifacts in $OUT"
