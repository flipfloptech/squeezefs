#!/usr/bin/env bash
# tests/fio/perf_session.sh — the post-wave performance session (2026-08-05,
# user sequencing: "run performance / merge everything / see our numbers /
# fix bugs on the way / then run gates"). One recorded sequence on one
# deployed pair; targets graded against SAME-DAY raw ceilings.
#
# Rows, in order:
#  0. raw ceilings (libaio direct on the data namespaces — no FS):
#     seq-read bs=4m qd16 ×10ns, rand-4k qd32 ×10ns — the grading rows
#  1. headline battery (kernel): seq read / seq write / randread /
#     randwrite on the exa_perf fileset shape, 60 s sustained each
#  2. ingress ladder (lever-2 field grading): 16x8 32x8 8x32 +
#     transport_drain_groups/width + commit-batch/wake deltas
#  3. libaio randread 32x8 il vs kernel (direct-drive sharding verdict:
#     shim >= kernel; ipc_direct_shards engagement)
#  4. durable fleet parity (convoy-fix verdict: il ~>= kernel durable)
#
# FIO ENGINE POLICY (user ruling 2026-08-07;
# `.benchmarks/2026-08-07-fio-engine-policy.md`): throughput/IOPS rows =
# ioengine=libaio + direct=1 + stated iodepth, BOTH lanes (il libaio rides
# the v1.1 aio interposers; engagement gates check the counters each lane
# actually moves); A/Bs use the SAME engine both sides; psync only as
# labeled sync-lane coverage rows; io_uring = labeled kernel-lane extra.
# This rig's own rows are libaio-compliant; row 4 delegates to
# fleet_parity_row.sh, whose rows are labeled SYNC-LANE COVERAGE there.
#
# OUTPUT CONVENTION (shared across tests/fio/ — user ruling 2026-08-07):
# default = the clean summary only (section headers + one result line per
# row); SQZ_DEBUG=1 adds the diagnostic stream (raw gauge/counter dumps)
# on stderr; failures are ALWAYS verbose regardless of mode (require-mount
# trips print mount diagnostics, fio/parse failures dump the captured fio
# stderr). Gate semantics/exit codes are unchanged by the mode.
#
# usage: perf_session.sh --mount <mnt> --meta <uri> [--out DIR]
set -u

SQZ_DEBUG="${SQZ_DEBUG:-0}"
dbg() { [ "$SQZ_DEBUG" = "1" ] && echo "$@" >&2 || true; }
MNT="" META="" OUT="/scratch/tmp/logs/perf_session_$(date +%Y%m%d_%H%M%S)"
SQZ="${SQZ:-/scratch/tmp/squeezefs}"
SHIM="${SHIM:-/scratch/tmp/libsqueezefs_il.so}"
RIG="${RIG:-/scratch/tmp/fio-e1076bfc}"
while [ $# -gt 0 ]; do
    case "$1" in
        --mount) MNT="$2"; shift 2 ;;
        --meta) META="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        *) echo "unknown arg $1" >&2; exit 2 ;;
    esac
done
[ -n "$MNT" ] && [ -n "$META" ] || { echo "--mount and --meta required" >&2; exit 2; }
mkdir -p "$OUT"

# Failure diagnostics are ALWAYS verbose (regardless of SQZ_DEBUG).
mount_diag() {
    {
        echo "---- mount diagnostic ($MNT) ----"
        findmnt "$MNT" 2>/dev/null || echo "  (findmnt: $MNT not mounted)"
        mount | grep -iE "squeezefs|fuse" || echo "  (no fuse/squeezefs mounts)"
        ls -la "$MNT" 2>&1 | head -5
        echo "---- end diagnostic ----"
    } >&2
}
require_mount() {
    mountpoint -q "$MNT" || { echo "FATAL: $MNT not a mountpoint" >&2; mount_diag; exit 1; }
    [ -e "$MNT/.stats" ] || { echo "FATAL: no .stats" >&2; mount_diag; exit 1; }
}

echo "== 0. raw ceilings (same-day grading rows) =="
# Data namespaces: /dev/nvme{10,12,14,16,18,20,22,24,26,28}n1. Raw device
# access needs root, and each device gets its own job (the first-session
# row printed 4.19 GB/s: unprivileged opens + one job over a colon set —
# instrument-junk, labeled so). If sudo is unavailable, label and skip —
# grading falls back to the standing 41.8 GB/s / 3.36 M rows.
DEVS=$(for i in 10 12 14 16 18 20 22 24 26 28; do printf "/dev/nvme%dn1:" $i; done | sed 's/:$//')
if sudo -n true 2>/dev/null; then
    sudo fio --name=rawread --filename="$DEVS" --rw=read --bs=4m --iodepth=16 \
        --numjobs=10 --ioengine=libaio --direct=1 --time_based --runtime=30 \
        --group_reporting --output-format=json 2>"$OUT/rawread.fio.err" | python3 -c "
import json,sys
raw=sys.stdin.buffer.read();d=json.loads(raw[raw.find(b'{'):])
print('  raw seq-read: %.2f GB/s' % (sum(x['read']['bw_bytes'] for x in d['jobs'])/1e9))" \
        || { echo "  raw seq-read: fio/parse FAILED — fio stderr follows" >&2; cat "$OUT/rawread.fio.err" >&2; }
    sudo fio --name=rawrand --filename="$DEVS" --rw=randread --bs=4k --iodepth=32 \
        --numjobs=32 --ioengine=libaio --direct=1 --time_based --runtime=30 \
        --group_reporting --output-format=json 2>"$OUT/rawrand.fio.err" | python3 -c "
import json,sys
raw=sys.stdin.buffer.read();d=json.loads(raw[raw.find(b'{'):])
r=[j['read'] for j in d['jobs']]
print('  raw rand-4k: %s IOPS' % format(int(sum(x['iops'] for x in r)),','))" \
        || { echo "  raw rand-4k: fio/parse FAILED — fio stderr follows" >&2; cat "$OUT/rawrand.fio.err" >&2; }
else
    echo "  (no passwordless sudo — raw rows SKIPPED; grade against standing 41.8 GB/s / 3.36M)"
fi

require_mount
echo "== 1. headline battery (kernel path, 60 s sustained each) =="
DIR="$MNT/exa_perf"
[ -d "$DIR" ] || { echo "FATAL: $DIR missing (battery fileset)" >&2; exit 1; }
for row in "read 1m 16 8 seqread" "randread 4k 32 8 randread"; do
    set -- $row; rw=$1 bs=$2 nj=$3 qd=$4 tag=$5
    require_mount
    fio --name=$tag --directory="$DIR" --filename_format='sqzfio.$jobnum.0' \
        --rw=$rw --bs=$bs --size=1g --numjobs=$nj --iodepth=$qd \
        --ioengine=libaio --direct=1 --time_based --runtime=60 \
        --group_reporting --output-format=json 2>"$OUT/$tag.fio.err" | python3 -c "
import json,sys
raw=sys.stdin.buffer.read();d=json.loads(raw[raw.find(b'{'):])
r=[j['read'] for j in d['jobs']]
bw=sum(x['bw_bytes'] for x in r)/1e9; iops=sum(x['iops'] for x in r)
print('  $tag: %.2f GB/s %s IOPS' % (bw, format(int(iops),',')))" \
        || { echo "  $tag: fio/parse FAILED — fio stderr follows" >&2; cat "$OUT/$tag.fio.err" >&2; }
done
WDIR="$MNT/perf_w"; rm -rf "$WDIR"; mkdir -p "$WDIR"
for row in "write 1m 16 8 seqwrite" "randwrite 4k 32 8 randwrite"; do
    set -- $row; rw=$1 bs=$2 nj=$3 qd=$4 tag=$5
    require_mount
    fio --name=$tag --directory="$WDIR" --filename_format='w.$jobnum' \
        --rw=$rw --bs=$bs --size=2g --numjobs=$nj --iodepth=$qd \
        --ioengine=libaio --direct=1 --time_based --runtime=60 \
        --group_reporting --output-format=json 2>"$OUT/$tag.fio.err" | python3 -c "
import json,sys
raw=sys.stdin.buffer.read();d=json.loads(raw[raw.find(b'{'):])
r=[j['write'] for j in d['jobs']]
bw=sum(x['bw_bytes'] for x in r)/1e9; iops=sum(x['iops'] for x in r)
print('  $tag: %.2f GB/s %s IOPS' % (bw, format(int(iops),',')))" \
        || { echo "  $tag: fio/parse FAILED — fio stderr follows" >&2; cat "$OUT/$tag.fio.err" >&2; }
    rm -rf "$WDIR"/*
done
rm -rf "$WDIR"

echo "== 2. ingress ladder (lever-2 grading; drain groups live) =="
# Raw gauge dump: debug stream only (the sweep's own summary lines govern).
dbg "$(python3 -c "
import json
m=json.load(open('$MNT/.stats'));m=m.get('metrics',m)
print('  transport_drain_groups=%s width=%s' % (m.get('transport_drain_groups'), m.get('transport_drain_group_width')))")"
"$RIG/transport_ingress_sweep.sh" --mount "$MNT" --points "16x8 32x8 8x32" \
    --runtime 30 --label lever2 2>&1 | tee "$OUT/ingress_sweep.log" | tail -6

echo "== 3. libaio randread il vs kernel (sharding verdict) =="
for arm in kern il; do
    require_mount
    envp=""; [ $arm = il ] && envp="LD_PRELOAD=$SHIM"
    B=$(python3 -c "
import json;m=json.load(open('$MNT/.stats'));m=m.get('metrics',m)
print(m.get('ipc_ops_read',0), m.get('ipc_direct_shards',0))")
    env $envp fio --name=rr --directory="$DIR" --filename_format='sqzfio.$jobnum.0' \
        --rw=randread --bs=4k --size=1g --numjobs=32 --iodepth=8 \
        --ioengine=libaio --direct=1 --time_based --runtime=30 \
        --group_reporting --output-format=json 2>"$OUT/rr_$arm.fio.err" | python3 -c "
import json,sys
raw=sys.stdin.buffer.read();d=json.loads(raw[raw.find(b'{'):])
r=[j['read'] for j in d['jobs']]
print('  $arm: %s IOPS' % format(int(sum(x['iops'] for x in r)),','))" \
        || { echo "  $arm: fio/parse FAILED — fio stderr follows" >&2; cat "$OUT/rr_$arm.fio.err" >&2; }
    A=$(python3 -c "
import json;m=json.load(open('$MNT/.stats'));m=m.get('metrics',m)
print(m.get('ipc_ops_read',0), m.get('ipc_direct_shards',0))")
    # Engagement counter dump (raw before/after tuples): debug stream only.
    dbg "    ops/shards before=($B) after=($A)"
done

echo "== 4. durable fleet parity (convoy verdict) =="
"$RIG/fleet_parity_row.sh" --mount "$MNT" --size 512m --out "$OUT/parity" 2>&1 \
    | tee "$OUT/parity_row.log" | grep -E "^  w\.|^  r\."
echo "artifacts: $OUT"
