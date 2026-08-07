#!/usr/bin/env bash
# Field-confirmation Set 2 — the shim's HYBRID LANE GATE (2026-08-07,
# perf/field-confirmations-0807), per
# `.benchmarks/2026-08-07-shim-hybrid-lane-gate.md` §4 on the field host.
#
# GEOMETRY ADJUDICATION (first-run lesson, this campaign): the two read-first
# notes compose, they do not stack. `SQUEEZEFS_IPC_ARENA_MB=1024` (the
# fio-engine-policy arena-slab law) gives a 1 MiB slot slab — and the gate's
# derivation clamps the threshold into [slab, max_op] = [1 MiB, 1 MiB], so on
# that mount the DERIVED gate keeps bs=1M ops on the RING by the
# strictly-greater boundary law (first run measured it: threshold gauge
# 1,048,576, 17 GiB byte-exact through the ring). The gate note's own §3/§4
# rows ran on a DEFAULT-arena mount (threshold ~= 143-168 KiB there). So:
#
#   PHASE 1 (default arena, armed): the §4 spec verbatim — threshold publish,
#     libaio + psync 1M lane rows (kernel-lane engagement), rand-4k ring row,
#     dd sticky row, kernel reference row, sustained ON row.
#   PHASE 2 (the A/B): matched-instrument libaio 1M read, ON legs = default
#     arena + derived gate (kernel lane) vs OFF legs = ARENA_MB=1024 +
#     SQUEEZEFS_IL_KERNEL_LANE_MIN=0 (the only geometry where a 1M libaio op
#     IS a ring op — the §5 residual: a libaio 1M OFF row at the default slab
#     is a silent kernel row, the banned shape). Remount per leg, fresh dir +
#     prewrite per leg, ON OFF OFF ON ON OFF (medians of 3, both orders).
#
# Engagement gates FATAL on every row (silent-fallback exits nonzero).
set -u

BIN="${BIN:-/scratch/tmp/squeezefs}"
SHIM="${SHIM:-/scratch/tmp/libsqueezefs_il.so}"
MNT="${MNT:-/scratch/tmp/test}"
META="${META:-sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1}"
DATA="${DATA:-sqdata:///dev/nvme10n1,/dev/nvme12n1,/dev/nvme14n1,/dev/nvme16n1,/dev/nvme18n1}"
OUT="${OUT:-/scratch/tmp/fieldconf-0807/set2}"
LOGDIR=/scratch/tmp/logs
GATE="$OUT/../set2_gate.py"
RESET_FIRST="${RESET_FIRST:-1}"
RESET_SCRIPT="${RESET_SCRIPT:-/scratch/tmp/cluster_reset_v4.sh}"
NQN_PREFIX="nqn.2026-07.io.squeezefs"
META_SUFFIXES=(aqr37-m0 aqr38-m0 aqr39-m0 aqs38-m0 aqs39-m0)
DATA_SUFFIXES=(aqr37-d0 aqr37-d1 aqr38-d0 aqr38-d1 aqr39-d0)
DATA_BLK=(nvme10n1 nvme12n1 nvme14n1 nvme16n1 nvme18n1)

mkdir -p "$OUT" "$LOGDIR"
fatal() { echo "FATAL: $*" >&2; exit 1; }
snap() { cat "$MNT/.stats" > "$1"; }
ds() { grep -E "$(IFS='|'; echo "${DATA_BLK[*]}")" /proc/diskstats > "$1"; }

do_umount() {
  if mountpoint -q "$MNT"; then
    "$BIN" umount "$MNT" >/dev/null 2>&1 || umount "$MNT" 2>/dev/null || true
  fi
  for _ in $(seq 1 300); do mountpoint -q "$MNT" || break; sleep 2; done
  mountpoint -q "$MNT" && fatal "mountpoint still mounted — wedge class"
  for _ in $(seq 1 300); do pidof squeezefs >/dev/null 2>&1 || break; sleep 2; done
  pidof squeezefs >/dev/null && fatal "daemon still alive after umount drain"
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

do_mount() { # $1 = tag, $2 = default|bigarena
  local env=(SQUEEZEFS_FUSE_ZC=1)
  [ "$2" = bigarena ] && env+=(SQUEEZEFS_IPC_ARENA_MB=1024 SQUEEZEFS_IPC_MEM_MAX=49152)
  env "${env[@]}" "$BIN" mount "$META" "$MNT" --daemon --interception --allow-other \
    --log-file "$LOGDIR/sqz-fieldconf-set2-$1.log" >/dev/null 2>&1
  local ok=0
  for _ in $(seq 1 60); do
    if mountpoint -q "$MNT" && cat "$MNT/.stats" >/dev/null 2>&1; then ok=1; break; fi
    sleep 1
  done
  [ "$ok" = 1 ] || fatal "mount gate failed ($1)"
  grep -q "zero-copy" "$LOGDIR/sqz-fieldconf-set2-$1.log" || fatal "$1: zc did not arm"
}

check_tripwires() { # $1 = phase label — poisoned/descriptor rejects on the LIVE daemon
  python3 -c "
import json
m=json.load(open('$MNT/.stats')); m=m.get('metrics',m)
p=m.get('ipc_sessions_poisoned',0); r=m.get('ipc_descriptor_rejects',0)
lo=m.get('transport_lease_overlong',0)
print('  tripwires ($1): ipc_sessions_poisoned=%s ipc_descriptor_rejects=%s transport_lease_overlong=%s' % (p,r,lo))
import sys
sys.exit(0 if (p==0 and r==0) else 1)
" || fatal "tripwires nonzero ($1)"
}

run_fio() { # tag engine rw dir extra... (LD_PRELOAD unless KERN=1)
  local tag=$1 engine=$2 rw=$3 dir=$4; shift 4
  local pre=()
  [ "${KERN:-0}" = 1 ] || pre=(env LD_PRELOAD="$SHIM" ${CLIENT_ENV:-})
  "${pre[@]}" fio --name="$tag" --directory="$dir" --ioengine="$engine" \
    --rw="$rw" --direct=1 --group_reporting --fallocate=none \
    --output-format=json --output="$OUT/$tag-fio.json" "$@" >/dev/null 2>&1 \
    || fatal "$tag: fio row failed"
}

command -v fio >/dev/null || fatal "fio not installed"
[ -x "$BIN" ] && [ -f "$SHIM" ] || fatal "binary/shim missing"

echo "=== set2 phase 1: reset + fresh format + ARMED mount (DEFAULT arena) ==="
do_umount
if [ "$RESET_FIRST" = 1 ]; then
  echo "  cluster reset (venue stationarity) ..."
  echo YES | bash "$RESET_SCRIPT" >"$OUT/reset.log" 2>&1 || fatal "cluster reset failed — see $OUT/reset.log"
  do_umount
  resolve_devs
fi
printf '%s\n' "${DATA_BLK[@]}" > "$OUT/datadevs.txt"
"$BIN" format "$META" "$DATA" --force >"$OUT/format.log" 2>&1 || fatal "format failed"
udevadm settle 2>/dev/null || true
do_mount phase1 default
python3 -c "
import json
m=json.load(open('$MNT/.stats')); m=m.get('metrics',m)
assert m.get('fuse3_kmbuf_negotiated',0)==1, 'kmbuf not negotiated'
print('  armed: fuse3_kmbuf_negotiated=1, interception on')" || fatal "zc arm gauge check failed"

echo "=== T: threshold publish (live bound fd, DERIVED default geometry) ==="
mkdir -p "$MNT/lg_hold"
dd if=/dev/urandom of="$MNT/lg_hold/f" bs=1M count=4 status=none
env LD_PRELOAD="$SHIM" python3 -c "
f=open('$MNT/lg_hold/f','rb'); f.read(65536)
import time; time.sleep(10)" & HOLDPID=$!
sleep 5
snap "$OUT/T-hold.json"
THR=$(python3 -c "
import json
m=json.load(open('$OUT/T-hold.json')); m=m.get('metrics',m)
print(m.get('ipc_lane_gate_threshold_bytes',0))")
wait $HOLDPID 2>/dev/null
[ "$THR" -gt 0 ] || fatal "T: ipc_lane_gate_threshold_bytes=$THR not > 0"
echo "  T: ipc_lane_gate_threshold_bytes=$THR (live-session gauge, derived) OK"

echo "=== E1: libaio 1M write il (kernel lane via size-blind aio screen + gate) ==="
mkdir -p "$MNT/lg_e1"; snap "$OUT/E1-before.json"; ds "$OUT/E1-ds-before.txt"
run_fio E1 libaio write "$MNT/lg_e1" --bs=1M --iodepth=8 --numjobs=4 --size=4g --end_fsync=1
snap "$OUT/E1-after.json"; ds "$OUT/E1-ds-after.txt"
python3 "$GATE" "$OUT" E1 lane_write || fatal "E1 gate"

echo "=== E2: libaio 1M read il (E1 fileset) ==="
snap "$OUT/E2-before.json"
run_fio E2 libaio read "$MNT/lg_e1" --bs=1M --iodepth=8 --numjobs=4 --size=4g \
  --filename_format='E1.$jobnum.$filenum'
snap "$OUT/E2-after.json"
python3 "$GATE" "$OUT" E2 lane_read || fatal "E2 gate"

echo "=== E3: psync 1M write il (per-op positional gate) ==="
mkdir -p "$MNT/lg_e3"; snap "$OUT/E3-before.json"; ds "$OUT/E3-ds-before.txt"
run_fio E3 psync write "$MNT/lg_e3" --bs=1M --numjobs=4 --size=4g --end_fsync=1
snap "$OUT/E3-after.json"; ds "$OUT/E3-ds-after.txt"
python3 "$GATE" "$OUT" E3 lane_write || fatal "E3 gate"

echo "=== E4: psync 1M read il (E3 fileset) ==="
snap "$OUT/E4-before.json"
run_fio E4 psync read "$MNT/lg_e3" --bs=1M --numjobs=4 --size=4g \
  --filename_format='E3.$jobnum.$filenum'
snap "$OUT/E4-after.json"
python3 "$GATE" "$OUT" E4 lane_read || fatal "E4 gate"

echo "=== R: psync rand-4k read il (the ring/IOPS lane, untouched by the gate) ==="
mkdir -p "$MNT/lg_r4"
KERN=1 run_fio R-prep libaio write "$MNT/lg_r4" --bs=1M --iodepth=8 --numjobs=8 --size=1g --end_fsync=1
snap "$OUT/R-before.json"
run_fio R psync randread "$MNT/lg_r4" --bs=4k --numjobs=8 --size=1g \
  --filename_format='R-prep.$jobnum.$filenum' --time_based --runtime=20
snap "$OUT/R-after.json"
python3 "$GATE" "$OUT" R ring_rand_read || fatal "R gate"

echo "=== D: dd bs=1M sticky (offsetful write(2) stream) ==="
mkdir -p "$MNT/lg_dd"; snap "$OUT/D-before.json"; ds "$OUT/D-ds-before.txt"
env LD_PRELOAD="$SHIM" dd if=/dev/zero of="$MNT/lg_dd/f" bs=1M count=1024 conv=fsync 2>"$OUT/D-dd.log" \
  || fatal "D: dd failed"
snap "$OUT/D-after.json"; ds "$OUT/D-ds-after.txt"
python3 "$GATE" "$OUT" D dd_sticky 1024 || fatal "D gate"

echo "=== KREF: kernel-lane reference (no shim), libaio 1M read ==="
mkdir -p "$MNT/lg_kref"
KERN=1 run_fio KREF-prep libaio write "$MNT/lg_kref" --bs=1M --iodepth=8 --numjobs=4 --size=2g --end_fsync=1
KERN=1 run_fio KREF libaio read "$MNT/lg_kref" --bs=1M --iodepth=8 --numjobs=4 --size=2g \
  --filename_format='KREF-prep.$jobnum.$filenum' --time_based --runtime=20
python3 -c "
import json
f=json.load(open('$OUT/KREF-fio.json'))
print('  KREF (kernel lane, labeled reference): %.2f GB/s' % (sum(j['read']['bw_bytes'] for j in f['jobs'])/1e9))"

echo "=== S: sustained >=60 s ON 1M libaio read (flat thirds) ==="
mkdir -p "$MNT/lg_sus"
KERN=1 run_fio S-prep libaio write "$MNT/lg_sus" --bs=1M --iodepth=8 --numjobs=4 --size=2g --end_fsync=1
snap "$OUT/S-before.json"
env LD_PRELOAD="$SHIM" fio --name=S --directory="$MNT/lg_sus" --ioengine=libaio \
  --rw=read --bs=1M --iodepth=8 --numjobs=4 --size=2g --direct=1 \
  --filename_format='S-prep.$jobnum.$filenum' --time_based --runtime=60 \
  --group_reporting --write_bw_log="$OUT/S" --log_avg_msec=1000 \
  --output-format=json --output="$OUT/S-fio.json" >/dev/null 2>&1 || fatal "S row failed"
snap "$OUT/S-after.json"
python3 "$GATE" "$OUT" S sustained || fatal "S gate"

check_tripwires phase1

echo "=== phase 2: A/B matched-libaio 1M read — ON(default arena, derived gate) vs OFF(1 GiB arena + LANE_MIN=0, the true ring arm); remount + fresh dir + prewrite per leg ==="
i=0
for arm in ON OFF OFF ON ON OFF; do
  i=$((i+1)); tag="AB$i$arm"
  do_umount
  if [ "$arm" = OFF ]; then do_mount "$tag" bigarena; else do_mount "$tag" default; fi
  mkdir -p "$MNT/lg_ab$i"
  KERN=1 run_fio "$tag-prep" libaio write "$MNT/lg_ab$i" --bs=1M --iodepth=8 --numjobs=4 --size=2g --end_fsync=1
  CLIENT_ENV=""
  [ "$arm" = OFF ] && CLIENT_ENV="SQUEEZEFS_IL_KERNEL_LANE_MIN=0"
  snap "$OUT/$tag-before.json"
  run_fio "$tag" libaio read "$MNT/lg_ab$i" --bs=1M --iodepth=8 --numjobs=4 --size=2g \
    --filename_format="$tag-prep.\$jobnum.\$filenum" --time_based --runtime=20
  snap "$OUT/$tag-after.json"
  CLIENT_ENV=""
  if [ "$arm" = ON ]; then
    python3 "$GATE" "$OUT" "$tag" lane_read || fatal "$tag gate"
  else
    python3 "$GATE" "$OUT" "$tag" ring_seq_read || fatal "$tag gate"
  fi
  check_tripwires "$tag"
done
python3 "$GATE" "$OUT" AB medians || fatal "AB medians"

echo "set2 complete — artifacts in $OUT"
