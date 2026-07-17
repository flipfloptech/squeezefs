#!/usr/bin/env bash
# spdkscope: run all three bench arms serialized. spdk_tgt is SIGSTOPped
# during the kernel arms so its idle busy-poller can't pollute them, and
# SIGCONTed for its own arm.
set -euo pipefail
cd "$(dirname "$0")"
STATE=/tmp/spdkscope/state
. "$STATE/devices"
: > /tmp/spdkscope/results/summary.csv.tmp
rm -f /tmp/spdkscope/results/summary.csv

echo "=== arm 1: spdk-tcp ($DEV_SPDK) ==="
kill -CONT "$SPDK_PID" 2>/dev/null || true
./bench.sh spdk-tcp "$DEV_SPDK" 3

echo "=== arm 2: nvmet-tcp ($DEV_NVMET) — spdk_tgt SIGSTOPped ==="
kill -STOP "$SPDK_PID"
./bench.sh nvmet-tcp "$DEV_NVMET" 3
kill -CONT "$SPDK_PID"

echo "=== arm 3: nvmet-loop reference ($DEV_LOOP) — spdk_tgt SIGSTOPped ==="
kill -STOP "$SPDK_PID"
./bench.sh nvmet-loop "$DEV_LOOP" 3
kill -CONT "$SPDK_PID"

echo "=== all arms done ==="
cat /tmp/spdkscope/results/summary.csv
