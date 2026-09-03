#!/usr/bin/env bash
# .benchmarks/rigs/2026-09-03-r2-read-fast-dispatch-rig.sh
#
# E2E audit R-2 (READ fast-dispatch from the reap thread) — the field rig
# on squeeze-test. Rows = the field job files verbatim (`randread_iops.job`:
# 24 × qd8 4 KiB randread on each file's first 1 GiB, 30 s + 10 s ramp;
# `read_BW.job`: 24 × qd16 1 MiB seq read — the regression row), kern mode
# (kernel FUSE-over-io_uring; the il rows are untouched by R-2), on the
# cache-less cluster_reset_v4 fabric. Per row: pre/post `.stats` (the A1
# exact histograms — every mean is Δsum_ns/Δcount), fio JSON, a 1 s bw log
# (the sustained first/last-third check), and the per-row delta table
# (`row_delta.py` beside this script): fast-dispatch engagement
# (`transport_fast_dispatch_{serves,demotes}` vs the row's READs), the
# transport ingress means (`queue_wait`/`dispatch_lag`/`transport_total`),
# the reap-gap family (`transport_reap_gap_ns` blind/blind_cqe/park), daemon
# CPU per op by class, tripwires, the read-copy closure.
#
# Usage (root, on the box; ALL artifacts under /scratch/tmp/sqz-agent):
#   $0 mount <bin> <label> [ENV=VAL ...]   # mounts the cluster with <bin>
#   $0 umount
#   $0 layout                               # write_BW.job once (24 × 8 GiB)
#   $0 row <rr4k|seq1m> <label> [secs]      # secs overrides the job's 30
#   $0 trace_row <label>                    # rr4k + mid-row .trace drains
set -euo pipefail

AGENT=/scratch/tmp/sqz-agent
RESULTS="${RESULTS:-$AGENT/rows}"
JOBS=/scratch/tmp/fio_jobs
MNT=/scratch/tmp/test
META="sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1"
FIO="${FIO:-/usr/bin/fio}"
mkdir -p "$RESULTS"

die() { echo "FATAL: $*" >&2; exit 1; }

wait_ready() {
    for _ in $(seq 1 240); do
        if cat "$MNT/.stats" >/dev/null 2>&1; then return 0; fi
        sleep 0.5
    done
    die "mount did not become ready"
}

do_mount() { # <bin> <label> [ENV=VAL ...]
    local bin="$1" label="$2"; shift 2
    pgrep -f 'squeezefs moun[t]' >/dev/null && die "a daemon is already up"
    sudo -n env "$@" "$bin" mount "$META" "$MNT" --daemon --interception --allow-other \
        --log-file "$AGENT/$label.log"
    wait_ready
    "$bin" --version
    python3 - "$MNT" <<'EOF'
import json, sys
m = json.load(open(sys.argv[1] + "/.stats"))["metrics"]
print("armed: zc=%s kmbuf=%s queues=%s depth=%s fast_dispatch_serves=%s demotes=%s" % (
    m.get("fuse3_zc_negotiated"), m.get("fuse3_kmbuf_negotiated"), m.get("transport_queues"),
    m.get("transport_q_depth"), m.get("transport_fast_dispatch_serves"), m.get("transport_fast_dispatch_demotes")))
EOF
}

do_umount() {
    # Plain umount, retried (a fio straggler can hold the tree for a
    # moment); the lazy detach is the last resort — a lazy-detached
    # daemon lingering into the NEXT mount's arm is what tore leg B3
    # down at its own ready line (both at 02:51:34).
    local ok=0
    for _ in $(seq 1 20); do
        if sudo -n umount "$MNT" 2>/dev/null; then ok=1; break; fi
        sleep 1
    done
    [ "$ok" = 1 ] || sudo -n umount -l "$MNT" || true
    for _ in $(seq 1 240); do
        pgrep -f 'squeezefs moun[t]' >/dev/null || break
        sleep 0.5
    done
    pgrep -f 'squeezefs moun[t]' >/dev/null && { sudo -n pkill -f 'squeezefs moun[t]' || true; sleep 2; }
    mountpoint -q "$MNT" && die "$MNT still mounted after umount"
    sleep 2
}

snap() { cat "$MNT/.stats" >"$1"; }

layout() {
    mkdir -p "$MNT/client_validation"
    "$FIO" "$JOBS/write_BW.job" --output-format=json --output="$RESULTS/layout.fio.json" >/dev/null
    sync
    python3 - "$RESULTS/layout.fio.json" <<'EOF'
import json, sys
j = json.load(open(sys.argv[1]))["jobs"]
print("layout: write bw %.1f GiB/s" % (sum(x["write"]["bw_bytes"] for x in j) / 2**30))
EOF
    ls -la "$MNT/client_validation" | head -3
}

row() { # <rr4k|seq1m> <label> [secs]
    local kind="$1" label="$2" secs="${3:-30}"
    local job
    case "$kind" in
        rr4k) job="$JOBS/randread_iops.job" ;;
        seq1m) job="$JOBS/read_BW.job" ;;
        *) die "unknown row kind $kind" ;;
    esac
    echo "quiet: loadavg $(cut -d' ' -f1-3 /proc/loadavg)" >"$RESULTS/$label.quiet"
    sync; sleep 2
    snap "$RESULTS/$label.stats0"
    # A job-section `runtime=` beats any command-line value (before OR
    # after the file), so a non-default duration is a derived job file:
    # the field job verbatim with only `runtime=` rewritten.
    if [ "$secs" != 30 ]; then
        sed "s/^runtime=.*/runtime=$secs/" "$job" >"$RESULTS/$label.job"
        job="$RESULTS/$label.job"
    fi
    "$FIO" --write_bw_log="$RESULTS/$label" --log_avg_msec=1000 \
        --output-format=json --output="$RESULTS/$label.fio.json" "$job" >"$RESULTS/$label.fio.out" 2>&1 ||
        die "$label: fio failed ($(tail -3 "$RESULTS/$label.fio.out"))"
    snap "$RESULTS/$label.stats1"
    python3 "$(dirname "$0")/row_delta.py" "$RESULTS" "$label" | tee "$RESULTS/$label.row"
}

# The attribution pass's drain recipe (§2/§8 of the 2026-09-03 note): one
# DISCARD drain at t=15 s, then ten capture drains 0.5 s apart, each
# preceded by the dentry drop that keeps the `.trace` LOOKUP from losing
# its payload to the kernel's revalidation.
trace_row() { # <label>
    local label="$1"
    echo "quiet: loadavg $(cut -d' ' -f1-3 /proc/loadavg)" >"$RESULTS/$label.quiet"
    sync; sleep 2
    snap "$RESULTS/$label.stats0"
    "$FIO" --write_bw_log="$RESULTS/$label" --log_avg_msec=1000 \
        --output-format=json --output="$RESULTS/$label.fio.json" "$JOBS/randread_iops.job" >"$RESULTS/$label.fio.out" 2>&1 &
    local fpid=$!
    sleep 25   # 10 s ramp + 15 s
    echo 2 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null; cat "$MNT/.trace" >/dev/null
    snap "$RESULTS/$label.stats_win0"
    for i in $(seq 0 9); do
        sleep 0.5
        echo 2 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null
        cat "$MNT/.trace" >"$RESULTS/$label.trace.$i.json"
    done
    snap "$RESULTS/$label.stats_win1"
    wait "$fpid" || die "$label: fio failed ($(tail -3 "$RESULTS/$label.fio.out"))"
    snap "$RESULTS/$label.stats1"
    python3 "$(dirname "$0")/row_delta.py" "$RESULTS" "$label" | tee "$RESULTS/$label.row"
}

case "${1:-}" in
    mount) shift; do_mount "$@" ;;
    umount) do_umount ;;
    layout) layout ;;
    row) shift; row "$@" ;;
    trace_row) shift; trace_row "$@" ;;
    *) die "usage: $0 mount <bin> <label> [ENV=VAL..] | umount | layout | row <rr4k|seq1m> <label> [secs] | trace_row <label>" ;;
esac
