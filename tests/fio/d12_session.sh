#!/usr/bin/env bash
# tests/fio/d12_session.sh — the D12 performance session (2026-08-04):
# ruling "no gates until all performance is where we want it"; targets
# "reads closer to writes (36-38 GB/s class), iops closer to 1M".
# Ceilings (measured, this box): raw seq-read 41.8 GB/s; raw rand-4k
# 3.36 M IOPS @ 372 us. Binary: the integrate/zcrx-wave tip.
#
# Rows, in order (one script so remount count and store aging are one
# recorded sequence; A-B-B-A where a comparison ages the store):
#  0a. cache posture: config get-cache-paths (is the disk tier even armed?)
#  0b. CACHE-HYPOTHESIS A/B (user, verbatim: "could it be the caching
#      mechanism?"): default vs -o direct_device_true mounts,
#      seqread-1M + randread-4k per side, A-B-B-A.
#  1.  Default battery — the population-derived arena acceptance
#      (engagement >= 0.90 on the bs=1M shim rows with NO env lever).
#  2.  Battery under SQUEEZEFS_IPC_SERVICE_THREADS=32 — the thread-slope
#      bracket for the -33% full-fleet finding.
#  3.  zcrx Z3 field rows retry (the Connect fix's first live shot).
#
# FIO ENGINE POLICY (user ruling 2026-08-07;
# `.benchmarks/2026-08-07-fio-engine-policy.md`): throughput/IOPS rows =
# ioengine=libaio + direct=1 + stated iodepth, BOTH lanes; A/Bs use the
# SAME engine both sides; psync only as labeled sync-lane coverage rows;
# io_uring = labeled kernel-lane extra. This rig is libaio-compliant
# (all rows); the delegated exa_client_perf battery is all-libaio too.
set -u
SQZ=/scratch/tmp/squeezefs
MNT=/scratch/tmp/test
META="sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1"
RIG=/scratch/tmp/fio-e1076bfc
OUT=/scratch/tmp/logs/d12_session
DIR=$MNT/exa_perf
mkdir -p "$OUT"

log() { echo "[d12 $(date +%H:%M:%S)] $*"; }

remount() { # $1 = extra mount args (may be empty), $2.. = env pairs
    local args=$1; shift
    local envs=("$@")
    "$SQZ" umount "$MNT" >/dev/null 2>&1 || true
    sleep 2
    env "${envs[@]}" "$SQZ" mount "$META" "$MNT" --daemon --interception \
        --allow-other $args --log-file /scratch/tmp/logs/sqz.log >/dev/null 2>&1
    sleep 4
    mountpoint -q "$MNT" || { log "REMOUNT FAILED (args=$args)"; exit 1; }
}

fio_row() { # $1 label, $2 rw, $3 bs, $4 runtime
    local label=$1 rw=$2 bs=$3 rt=$4
    fio --name=r --directory="$DIR" --filename_format='sqzfio.$jobnum.0' \
        --rw="$rw" --bs="$bs" --size=1g --numjobs=16 --iodepth=8 \
        --ioengine=libaio --direct=1 --time_based --runtime="$rt" \
        --group_reporting --output-format=json --output="$OUT/$label.fio.json" \
        >/dev/null 2>&1
    python3 - "$OUT/$label.fio.json" "$label" <<'EOF'
import json, sys
raw = open(sys.argv[1], "rb").read()
d = json.loads(raw[raw.find(b"{"):])
r = [j["read"] for j in d["jobs"]]
ios = sum(x["total_ios"] for x in r)
if ios:
    bw = sum(x["bw_bytes"] for x in r) / 1e9
    iops = sum(x["iops"] for x in r)
    clat = sum(x["clat_ns"]["mean"] * x["total_ios"] for x in r) / ios / 1000
    print(f"  {sys.argv[2]}: {bw:.2f} GB/s {iops:,.0f} IOPS clat {clat:.0f}us")
EOF
}

log "row 0a: cache posture"
"$SQZ" config get-cache-paths "$META" 2>&1 | head -3 | sed 's/^/  /'

log "row 0b: cache-hypothesis A-B-B-A (default vs direct_device_true)"
for side in DEF1 DDT1 DDT2 DEF2; do
    case $side in
        DEF*) remount "" ;;
        DDT*) remount "-o direct_device_true" ;;
    esac
    log " side $side"
    fio_row "$side.seq" read 1m 45
    fio_row "$side.rand" randread 4k 45
done

log "row 1: default battery (arena-derivation acceptance)"
remount ""
rm -f "$OUT/bat_default.rc"
"$RIG/exa_client_perf.sh" --mount "$MNT" --shim /scratch/tmp/libsqueezefs_il.so \
    > "$OUT/bat_default.log" 2>&1
echo $? > "$OUT/bat_default.rc"
grep -A 18 "^row " "$OUT/bat_default.log" | head -6 | sed 's/^/  /'

log "row 2: battery @ SERVICE_THREADS=32 (thread-slope bracket)"
remount "" SQUEEZEFS_IPC_SERVICE_THREADS=32
"$RIG/exa_client_perf.sh" --mount "$MNT" --shim /scratch/tmp/libsqueezefs_il.so \
    > "$OUT/bat_threads32.log" 2>&1
echo $? > "$OUT/bat_threads32.rc"
grep -A 18 "^row " "$OUT/bat_threads32.log" | head -6 | sed 's/^/  /'

log "row 3: zcrx Z3 field rows (Connect fix live)"
SQZ="$SQZ" "$RIG/zcrx_field_rows.sh" --mount "$MNT" --meta "$META" \
    --out "$OUT/zcrx" > "$OUT/zcrx_rows.log" 2>&1
grep -E "^==|ENGAGED|INVALID|GB/s" "$OUT/zcrx_rows.log" | tail -16 | sed 's/^/  /'

remount ""
log "session complete; artifacts $OUT"
