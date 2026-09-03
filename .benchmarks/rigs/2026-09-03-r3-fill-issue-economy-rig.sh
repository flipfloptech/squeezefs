#!/usr/bin/env bash
# .benchmarks/rigs/2026-09-03-r3-fill-issue-economy-rig.sh
#
# E2E audit R-3 (read board #3 — fill-issue economy, in its zc-leg form).
# Rows = fio rand-4k / seq-1m reads on a cacheless mount, kern (kernel
# FUSE-over-io_uring) and il (LD_PRELOAD shim) modes; per row: pre/post
# `.stats` snapshots (the A1 exact histograms — every mean is
# sum_ns/count), fio JSON, an optional mid-row `.trace` drain (the A2 ring
# — `TRACE=1`; the drain is preceded by the dentry drop the attribution
# note's §8 item 1 requires), and the per-row delta table
# (`2026-09-03-r3-row-delta.py`): the zc bridge decomposition
# (`zc_bridge_phase_ns` msg_hop / sq_wait / device_cq / wake_hop / total),
# the funnel split, the worker's per-op enter/pass/wake economy, CPU by
# class, engagement + closure.
#
# Usage (root):
#   BIN=... IL=... MNT=/mnt/sqz-r3 RESULTS=/tmp/r3/rows $0 <row> <mode> <secs> [label]
#   rows: rr4k_qd8 rr4k_qd32 seq1m_qd8 ; modes: kern il ; secs: 30 / 60
#   TRACE=1 drains `.trace` at t = secs/2 into $RESULTS/$label.trace.json
#   and runs tests/op_trace_stitch.py against the row's stats pair.
#   The daemon must already be mounted at MNT (A/B binaries alternate at
#   the mount, not here). Files: $MNT/rr/f.{0..23} (FILE_MB each) — laid
#   out once by `$0 layout`.
set -euo pipefail

FIO="${FIO:-/nix/store/ii22gz4ajgiix2kpk8d5zraxpi11dk06-fio-3.42/bin/fio}"
MNT="${MNT:-/mnt/sqz-r3}"
RESULTS="${RESULTS:-/tmp/r3/rows}"
IL="${IL:-}"
NUMJOBS="${NUMJOBS:-24}"
FILE_MB="${FILE_MB:-256}"
TRACE="${TRACE:-0}"
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$RESULTS"

die() { echo "FATAL: $*" >&2; exit 1; }

layout() {
    mkdir -p "$MNT/rr"
    "$FIO" --name=layout --directory="$MNT/rr" --filename_format='f.$jobnum' \
        --rw=write --bs=1m --size="${FILE_MB}m" --numjobs="$NUMJOBS" --ioengine=libaio \
        --iodepth=16 --direct=1 --end_fsync=1 --group_reporting --output-format=terse >/dev/null
    sync
    ls -la "$MNT/rr" | head -3
}

# `cat`, not `cp`: the virtual inode's size changes between stat and read.
snap() { cat "$MNT/.stats" >"$1"; }

# The `.trace` drain (attribution note §8 item 1: a cached dentry makes the
# kernel's revalidation LOOKUP eat the payload — drop it first).
drain_trace() {
    sync
    echo 2 >/proc/sys/vm/drop_caches
    cat "$MNT/.trace" >"$1"
}

row() { # <row> <mode> <secs> [label]
    local rowname="$1" mode="$2" secs="$3" label="${4:-$1-$2-$3}"
    local rw bs qd
    case "$rowname" in
        rr4k_qd8) rw=randread; bs=4k; qd=8 ;;
        rr4k_qd32) rw=randread; bs=4k; qd=32 ;;
        seq1m_qd8) rw=read; bs=1m; qd=8 ;;
        *) die "unknown row $rowname" ;;
    esac
    local pre=()
    if [ "$mode" = il ]; then
        [ -n "$IL" ] || die "il mode needs IL=<libsqueezefs_il.so>"
        # A `-dirty` daemon/shim pair needs the dev override on BOTH ends
        # (KD-7 forgives -dirty degeneracy, never inequality) — the mount
        # carries SQUEEZEFS_IPC_ALLOW_DEV=1 too.
        pre=(env LD_PRELOAD="$IL" SQUEEZEFS_IPC_ALLOW_DEV=1)
    fi
    echo "quiet: loadavg $(cut -d' ' -f1-3 /proc/loadavg) rustc=$(pgrep -c -x rustc || true)" >"$RESULTS/$label.quiet"
    sync; sleep 1
    if [ "$TRACE" = 1 ]; then
        # Empty the rings so the mid-row drain is this row's population.
        drain_trace /dev/null
    fi
    snap "$RESULTS/$label.stats0"
    "${pre[@]}" "$FIO" --name="$label" --directory="$MNT/rr" --filename_format='f.$jobnum' \
        --rw="$rw" --bs="$bs" --size="${FILE_MB}m" --numjobs="$NUMJOBS" --ioengine=libaio \
        --iodepth="$qd" --direct=1 --norandommap=0 --time_based --runtime="$secs" --ramp_time=3 \
        --group_reporting --write_bw_log="$RESULTS/$label" --log_avg_msec=1000 \
        --output-format=json --output="$RESULTS/$label.fio.json" >"$RESULTS/$label.fio.out" 2>&1 &
    local fpid=$!
    if [ "$TRACE" = 1 ]; then
        sleep "$((secs / 2 + 3))"
        drain_trace "$RESULTS/$label.trace.json"
    fi
    wait "$fpid" || die "$label: fio failed ($(tail -3 "$RESULTS/$label.fio.out"))"
    snap "$RESULTS/$label.stats1"
    python3 "$(dirname "$0")/2026-09-03-r3-row-delta.py" "$RESULTS" "$label" | tee "$RESULTS/$label.row"
    if [ "$TRACE" = 1 ]; then
        python3 "$REPO/tests/op_trace_stitch.py" "$RESULTS/$label.trace.json" \
            --stats-pre "$RESULTS/$label.stats0" --stats-post "$RESULTS/$label.stats1" \
            --ops 2 --json "$RESULTS/$label.stitch.json" | tee "$RESULTS/$label.stitch" | tail -40 || true
    fi
}

case "${1:-}" in
    layout) layout ;;
    rr4k_qd8|rr4k_qd32|seq1m_qd8) row "$@" ;;
    *) die "usage: $0 layout | <row> <kern|il> <secs> [label]" ;;
esac
