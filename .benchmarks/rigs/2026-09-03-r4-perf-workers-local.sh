#!/usr/bin/env bash
# .benchmarks/rigs/2026-09-03-r4-perf-workers-local.sh — the LOCAL twin of the field rig's perf_workers():
# Record the f3-ur* queue-worker threads of the running daemon: flat
# (cpu-clock, no call graph) for the per-symbol self-time ledger, then a
# DWARF call-graph capture for caller attribution.
# usage: perf_ur.sh <outdir> <flat_secs> <cg_secs> [perf]
set -eu
OUT="$1"; FLAT="$2"; CG="$3"; PERF="${4:-perf}"
mkdir -p "$OUT"
PID=$(ps -eo pid,args | awk '/[s]queezefs mount/ {print $1; exit}')
[ -n "$PID" ] || { echo "no daemon" >&2; exit 1; }
TIDS=$(for t in /proc/$PID/task/*; do n=$(cat "$t/comm"); case "$n" in f3-ur[0-9]*) basename "$t";; esac; done | paste -sd,)
echo "pid=$PID ur-tids=$(echo "$TIDS" | tr ',' '\n' | wc -l)"
echo "$TIDS" > "$OUT/ur.tids"
# per-thread CPU before/after (utime+stime jiffies) for the busy fraction
for t in $(echo "$TIDS" | tr ',' ' '); do awk '{print $14+$15}' /proc/$PID/task/$t/stat; done | paste -sd' ' > "$OUT/ur.cpu0"
date +%s.%N > "$OUT/ur.t0"
"$PERF" record -e cpu-clock -F 4999 -t "$TIDS" -o "$OUT/ur-flat.data" -- sleep "$FLAT" 2>"$OUT/perf-flat.err" || true
date +%s.%N > "$OUT/ur.t1"
for t in $(echo "$TIDS" | tr ',' ' '); do awk '{print $14+$15}' /proc/$PID/task/$t/stat; done | paste -sd' ' > "$OUT/ur.cpu1"
if [ "$CG" -gt 0 ]; then
  "$PERF" record -e cpu-clock -F 1999 --call-graph dwarf,16384 -t "$TIDS" -o "$OUT/ur-cg.data" -- sleep "$CG" 2>"$OUT/perf-cg.err" || true
fi
# syscalls per second on the workers (the enter count per op face)
"$PERF" stat -e 'syscalls:sys_enter_io_uring_enter,syscalls:sys_enter_read,syscalls:sys_enter_futex,syscalls:sys_enter_write,context-switches' -t "$TIDS" -- sleep 3 2>"$OUT/ur-stat.txt" || true
cat "$OUT/ur-stat.txt" | grep -E "io_uring|read|futex|write|context|seconds" || true
