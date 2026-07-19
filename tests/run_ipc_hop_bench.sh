#!/bin/bash
# Two-process IPC-hop measurement runner — the G-L4-1 go/no-go surface
# (docs/design-preload-interception.md §5.8 / PR L4-2).
#
# Runs the three rig legs (echo / serve-shaped / tokio-handoff) under the
# §5.8.4 rail policy: the PR-2 rig rows are uncaged (0-31) per the design's
# core-budget table ("per-thread figures are cage-independent"), plus a
# labeled governed 0-15 companion so the matched-rails shape is on record.
# n=3 per point; the evidence note takes medians and states rails +
# instrument per row.
#
# Usage:
#   tests/run_ipc_hop_bench.sh            # full sweep (minutes)
#   tests/run_ipc_hop_bench.sh quick      # one short pass per leg
#   STRACE=1 tests/run_ipc_hop_bench.sh   # add a run-under strace -c -f
#                                         # syscall-count companion per leg
set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-full}"
RIG=target/release/ipc_hop_rig
RAIL_UNCAGED="0-31"
RAIL_GOVERNED="0-15"

echo "== build (release; house build rails) =="
taskset -c 0-15 env CARGO_BUILD_JOBS=12 \
    cargo build --release -p squeezefs-ipc --features rig

freq="$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq 2>/dev/null || echo '?')"
echo "== box: $(nproc) logical CPUs, scaling_max_freq=${freq} (3.5 GHz cap expected — do not change) =="

if [ "$MODE" = "quick" ]; then
    OPS=100000; WARM=10000; REPS=1
else
    OPS=1000000; WARM=100000; REPS=3
fi

run_leg() { # rail leg threads service extra...
    local rail="$1" leg="$2" threads="$3" service="$4"; shift 4
    for rep in $(seq 1 "$REPS"); do
        echo "--- rail=$rail leg=$leg t=$threads svc=$service rep=$rep ---"
        taskset -c "$rail" "$RIG" --leg "$leg" --threads "$threads" \
            --service-threads "$service" --ops "$OPS" --warmup "$WARM" "$@" \
            | grep -E '^(SUMMARY|SERVICE)' | sed "s/^/[$rail] /"
    done
    if [ "${STRACE:-0}" = "1" ]; then
        echo "--- strace -c -f companion (rail=$rail leg=$leg; counts include setup — use a long run so steady state dominates) ---"
        strace -c -f -o /tmp/ipc_hop_strace.$$ taskset -c "$rail" "$RIG" \
            --leg "$leg" --threads "$threads" --service-threads "$service" \
            --ops "$OPS" --warmup "$WARM" "$@" >/dev/null
        grep -E 'futex|total|syscall' /tmp/ipc_hop_strace.$$ | head -20
        rm -f /tmp/ipc_hop_strace.$$
    fi
}

echo "== leg (i): echo — RTT + syscalls/op (G-L4-1: p50 ≤ 3 µs, ≤ 0.1 syscalls/op both sides) =="
for t in 1 4 8; do
    run_leg "$RAIL_UNCAGED" echo "$t" 1
done
run_leg "$RAIL_GOVERNED" echo 4 1

echo "== leg (ii): serve-shaped — throughput/core (G-L4-1: ≥ 650k/core, ≥ 1.3M @ 2 svc) =="
for svc in 1 2; do
    for t in 4 8 12 16; do
        run_leg "$RAIL_UNCAGED" serve "$t" "$svc"
    done
done
run_leg "$RAIL_GOVERNED" serve 8 2

echo "== leg (iii): tokio-handoff — adder vs echo (G-L4-1: ≤ 3 µs/op) =="
run_leg "$RAIL_UNCAGED" echo 8 2
run_leg "$RAIL_UNCAGED" handoff 8 2 --runtime-workers 4
run_leg "$RAIL_GOVERNED" handoff 8 2 --runtime-workers 4

echo "== done — medians + adjudication belong in the .benchmarks evidence note =="
