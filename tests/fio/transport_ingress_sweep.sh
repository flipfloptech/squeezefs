#!/usr/bin/env bash
# tests/fio/transport_ingress_sweep.sh — the transport-queueing campaign's
# baseline + bracket instrument (2026-08-04, the field randread-kernel
# venue named by .benchmarks/2026-08-01-transport-ingress.md §9.2).
#
# Field evidence being chased (squeeze-test, randread-kernel libaio bs=4k
# qd8 njobs=32 = 256 in-flight): read_transport_phase_ns queue_wait
# ≈293 µs + dispatch_lag ≈412 µs in front of a ≈363 µs serve — 66 % of
# clat is pre-handler transport queueing, and it scales SUPER-linearly
# with in-flight (local 64-in-flight: 42 µs + 35 µs).
#
# This sweep runs randread-kernel at in-flight 32/64/128/256 (njobs × qd
# combinations) against ONE prefilled fileset and prints, per point:
# IOPS, fio clat, and the est-mean queue_wait / dispatch_lag /
# serve-total / transport_total deltas off the stats inode — the
# scaling-curve instrument for every A/B lever (pin-scope, queues,
# depth, dispatch changes).
#
# Venue law: run on the TCP dev substrate (fabric-sensitive row —
# the two-substrate rule); state instrument + substrate on every use.
#
# FIO ENGINE POLICY (user ruling 2026-08-07;
# `.benchmarks/2026-08-07-fio-engine-policy.md`): throughput/IOPS rows =
# ioengine=libaio + direct=1 + stated iodepth, BOTH lanes; A/Bs use the
# SAME engine both sides; psync only as labeled sync-lane coverage rows;
# io_uring = labeled kernel-lane extra. This rig is libaio-compliant
# (all rows: prefill + every njobs x qd sweep point).
#
# usage: transport_ingress_sweep.sh --mount <mnt> [--dir <dir>]
#        [--points "4x8 8x8 16x8 32x8"]  (njobs x qd list)
#        [--size 512m] [--runtime 20] [--label <tag>]
set -u
MNT="" DIR="" POINTS="4x8 8x8 16x8 32x8" SIZE="512m" RUNTIME="20" LABEL="sweep"
while [ $# -gt 0 ]; do
    case "$1" in
        --mount) MNT="$2"; shift 2 ;;
        --dir) DIR="$2"; shift 2 ;;
        --points) POINTS="$2"; shift 2 ;;
        --size) SIZE="$2"; shift 2 ;;
        --runtime) RUNTIME="$2"; shift 2 ;;
        --label) LABEL="$2"; shift 2 ;;
        *) echo "unknown arg $1" >&2; exit 2 ;;
    esac
done
[ -n "$MNT" ] || { echo "--mount required" >&2; exit 2; }
DIR="${DIR:-$MNT/ingress_sweep}"
OUT="/tmp/ingress_sweep_$(date +%Y%m%d_%H%M%S)_$LABEL"

# A row against a bare directory is NO row (the 2026-08-05 fabricated-run
# lesson: fio "succeeds" against local scratch and every number is
# fiction). Fatal at start and re-checked before every point.
require_mount() {
    mountpoint -q "$MNT" || { echo "FATAL: $MNT is not a mountpoint — refusing to fabricate rows" >&2; exit 1; }
    [ -e "$MNT/.stats" ] || { echo "FATAL: $MNT/.stats missing — not a squeezefs mount" >&2; exit 1; }
}
require_mount
mkdir -p "$OUT" "$DIR"

# Snapshot with parse-retry: on busy mounts the kernel can clamp a
# buffered .stats read at a stale i_size (torn JSON — the 2026-08-04
# stats-tear finding, fix queued); dd-to-EOF + json validation retries
# until a coherent generation lands.
snap() {
    for _ in 1 2 3 4 5 6 7 8; do
        dd if="$MNT/.stats" of="$1" bs=1M status=none 2>/dev/null
        python3 -c "import json,sys; json.load(open('$1'))" 2>/dev/null && return 0
        sleep 0.3
    done
    echo "WARN: torn .stats snapshot after 8 attempts: $1" >&2
    return 1
}

phase_table() {
    python3 - "$1" "$2" <<'EOF'
import json, re, sys
def fams(d):
    # phase families live top-level on some generations, under "metrics"
    # on others — serve both.
    out = dict(d.get("metrics", {}))
    out.update({k: v for k, v in d.items() if k.endswith("_phase_ns")})
    return out
b, a = fams(json.load(open(sys.argv[1]))), fams(json.load(open(sys.argv[2])))
def wmean_us(delta):
    tot = n = 0.0
    for bk, c in delta.items():
        if c <= 0: continue
        m = re.match(r"<=(\d+)(us|ms|s)?$", bk)
        if not m: continue
        val = float(m.group(1)); unit = m.group(2) or "ns"
        tot += val * {"us":1.0,"ms":1000.0,"s":1e6}[unit] * 0.75 * c; n += c
    return (tot / n if n else 0.0, int(n))
cells = []
for fam, phase in (("read_transport_phase_ns","queue_wait"),
                   ("read_transport_phase_ns","dispatch_lag"),
                   ("read_transport_phase_ns","transport_total"),
                   ("read_serve_phase_ns","total"),
                   ("read_fill_phase_ns","dev_service")):
    delta = {}
    for bk, v in a.get(fam, {}).get(phase, {}).items():
        d = v - b.get(fam, {}).get(phase, {}).get(bk, 0)
        if d: delta[bk] = d
    est, n = wmean_us(delta)
    cells.append(f"{phase}={est:.0f}us(n={n})")
print(" ".join(cells))
EOF
}

echo "== transport ingress sweep: $LABEL (mount=$MNT size=$SIZE runtime=${RUNTIME}s) =="
echo "   pin_scope=${SQUEEZEFS_FUSE_PIN_SCOPE:-node(default)} queues=${SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES:-default} depth=${SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH:-default}"

# Prefill once at the widest job count so every point reads real bytes.
MAXJ=0
for p in $POINTS; do j=${p%%x*}; [ "$j" -gt "$MAXJ" ] && MAXJ=$j; done
fio --name=prefill --directory="$DIR" --filename_format='swp.$jobnum.$filenum' \
    --rw=write --bs=1M --size="$SIZE" --numjobs="$MAXJ" --iodepth=8 \
    --ioengine=libaio --direct=1 --fallocate=none --group_reporting \
    --minimal > /dev/null 2>&1 || { echo "PREFILL FAILED" >&2; exit 1; }

printf "%-8s %-10s %-10s %s\n" "point" "IOPS" "clat_us" "phases"
for p in $POINTS; do
    j=${p%%x*}; q=${p##*x}
    require_mount
    snap "$OUT/$p.before.json" || { echo "FATAL: no coherent .stats before $p — refusing the point" >&2; exit 1; }
    fio --name=rr --directory="$DIR" --filename_format='swp.$jobnum.$filenum' \
        --rw=randread --bs=4k --size="$SIZE" --numjobs="$j" --iodepth="$q" \
        --ioengine=libaio --direct=1 --time_based --runtime="$RUNTIME" \
        --group_reporting --output-format=json --output="$OUT/$p.fio.json" \
        > "$OUT/$p.stderr" 2>&1
    rc=$?
    snap "$OUT/$p.after.json"
    if [ $rc -ne 0 ]; then echo "$p: FIO FAILED rc=$rc (see $OUT/$p.stderr)"; exit 1; fi
    # strip fio 3.4x's advisory prelude (the run_fio_row.sh law)
    python3 -c "
import sys
p='$OUT/$p.fio.json'; raw=open(p,'rb').read(); i=raw.find(b'{')
open(p,'wb').write(raw[i:]) if i>0 else None"
    read -r IOPS CLAT <<< "$(python3 -c "
import json
d=json.load(open('$OUT/$p.fio.json'))
r=[j['read'] for j in d['jobs']]
iops=sum(x['iops'] for x in r)
ios=sum(x['total_ios'] for x in r)
clat=sum(x['clat_ns']['mean']*x['total_ios'] for x in r)/max(ios,1)/1000
print(f'{iops:.0f} {clat:.0f}')")"
    PH="$(phase_table "$OUT/$p.before.json" "$OUT/$p.after.json")"
    printf "%-8s %-10s %-10s %s\n" "$p" "$IOPS" "$CLAT" "$PH"
done
echo "artifacts: $OUT"
