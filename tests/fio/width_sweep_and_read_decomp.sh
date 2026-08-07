#!/usr/bin/env bash
# tests/fio/width_sweep_and_read_decomp.sh — 2026-08-06 field session:
# (A) the direct-drive width sweep — the MEASUREMENT rows for a derived
#     class slope (the explicit knobs are levers; the landing is a
#     derivation + tie test, never a constant — user ruling);
#     bracket W8(start) W12 W16 W24 W8(end), 3×30 s rows each, rand-4k
#     32×32, engagement + owners/shards + per-thread CPU on row 2.
# (B) seq-read decomposition rows on the current binary: kernel bs=1M
#     16×8 60 s with read_{serve,fill,transport}_phase_ns + copy-ledger
#     deltas + mpstat — the fresh evidence for the read-throughput
#     campaign (reads vs the 41.8 raw ceiling).
#
# FIO ENGINE POLICY (user ruling 2026-08-07;
# `.benchmarks/2026-08-07-fio-engine-policy.md`): throughput/IOPS rows =
# ioengine=libaio + direct=1 + stated iodepth, BOTH lanes; A/Bs use the
# SAME engine both sides; psync only as labeled sync-lane coverage rows;
# io_uring = labeled kernel-lane extra. This rig is libaio-compliant
# (all rows: il rand-4k 32x32, kernel seq-read 16x8).
set -u
MNT="/scratch/tmp/test"
META="sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1"
SQZ="/scratch/tmp/squeezefs"
SHIM="/scratch/tmp/libsqueezefs_il.so"
OUT="/scratch/tmp/logs/wsweep_$(date +%Y%m%d_%H%M%S)"
mkdir -p "$OUT"

require_mount() {
    mountpoint -q "$MNT" || { echo "FATAL: not a mountpoint" >&2; exit 1; }
    [ -e "$MNT/.stats" ] || { echo "FATAL: no .stats" >&2; exit 1; }
}

remount_width() { # $1 = width ("" = derived default)
    "$SQZ" umount "$MNT" >/dev/null 2>&1 || true
    for i in $(seq 1 60); do pidof squeezefs >/dev/null || break; sleep 5; done
    local envs=""
    [ -n "$1" ] && envs="SQUEEZEFS_IPC_SERVICE_THREADS=$1 SQUEEZEFS_IPC_DD_SHARDS=$1"
    env $envs "$SQZ" mount "$META" "$MNT" --daemon --interception \
        --allow-other --log-file /scratch/tmp/logs/sqz.log >/dev/null 2>&1
    for i in $(seq 1 30); do mountpoint -q "$MNT" && [ -e "$MNT/.stats" ] && return 0; sleep 2; done
    echo "REMOUNT FAILED (width=$1)"; exit 1
}

snap() {
    for _ in 1 2 3 4 5; do
        dd if="$MNT/.stats" of="$1" bs=1M status=none 2>/dev/null
        python3 -c "import json;json.load(open('$1'))" 2>/dev/null && return 0
        sleep 0.3
    done
    return 1
}

rand_row() { # $1=tag $2=capture_cpu(0/1)
    require_mount
    local pidst=""
    if [ "$2" = 1 ]; then
        pidstat -t -p "$(pidof squeezefs)" 5 6 > "$OUT/$1.pidstat" 2>/dev/null &
        pidst=$!
    fi
    LD_PRELOAD="$SHIM" fio --name=rr --directory="$MNT/exa_perf" \
        --filename_format='sqzfio.$jobnum.0' --rw=randread --bs=4k --size=1g \
        --numjobs=32 --iodepth=32 --ioengine=libaio --direct=1 --time_based \
        --runtime=30 --group_reporting --output-format=json 2>/dev/null | python3 -c "
import json,sys
raw=sys.stdin.buffer.read();d=json.loads(raw[raw.find(b'{'):])
r=[j['read'] for j in d['jobs']]
print('  $1: %s IOPS clat=%dus' % (format(int(sum(x['iops'] for x in r)),','), sum(x['clat_ns']['mean'] for x in r)/len(r)/1000))"
    python3 -c "
import json;m=json.load(open('$MNT/.stats'));m=m.get('metrics',m)
print('    owners=%s shards=%s svc=%s' % (m.get('ipc_session_owners'), m.get('ipc_direct_shards'), m.get('ipc_service_threads')))"
    [ -n "$pidst" ] && wait $pidst 2>/dev/null
    sleep 8
}

echo "== A. width sweep (rand-4k 32x32, 3 rows/width, W8 brackets) =="
for w in "" 12 16 24 ""; do
    label="W${w:-8def}"
    remount_width "$w"
    echo " -- $label --"
    rand_row "$label.r1" 0
    rand_row "$label.r2" 1
    rand_row "$label.r3" 0
done

echo "== B. seq-read decomposition (kernel bs=1M 16x8, 60 s + phases) =="
remount_width ""
require_mount
snap "$OUT/seqread.before.json"
mpstat -P ALL 10 6 > "$OUT/seqread.mpstat" 2>/dev/null &
MP=$!
pidstat -t -p "$(pidof squeezefs)" 10 6 > "$OUT/seqread.pidstat" 2>/dev/null &
PS=$!
fio --name=sr --directory="$MNT/exa_perf" --filename_format='sqzfio.$jobnum.0' \
    --rw=read --bs=1M --size=1g --numjobs=16 --iodepth=8 --ioengine=libaio \
    --direct=1 --time_based --runtime=60 --group_reporting \
    --output-format=json --output="$OUT/seqread.fio.json" >/dev/null 2>&1
wait $MP $PS 2>/dev/null
snap "$OUT/seqread.after.json"
python3 -c "
import json
raw=open('$OUT/seqread.fio.json','rb').read();d=json.loads(raw[raw.find(b'{'):])
r=[j['read'] for j in d['jobs']]
print('  kernel seqread: %.2f GB/s' % (sum(x['bw_bytes'] for x in r)/1e9))"
echo "artifacts: $OUT"
