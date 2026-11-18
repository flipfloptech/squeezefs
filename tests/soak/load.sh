#!/bin/bash
# F1, F2, F4, F7a and F7b ×3 under cpu+smoke load (decision 0004 §8.3).
# Run as: SQZ_VENUE_SLOT=1 scripts/venue tests/soak/load.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

LOG_DIR=${SQZ_SOAK_LOG_DIR:-$HOME/tmp/pr6-soak}
CACHE=${SQZ_BUILD_ROOT:-/build}/zig-cache-v2
PREFIX=${PREFIX:-zig-out-linux}
RUNS=${RUNS:-3}
CPU_LOAD=${CPU_LOAD:-16}
mkdir -p "$LOG_DIR"
LOG=$LOG_DIR/load.log
: > "$LOG"
exec > >(stdbuf -oL tee -a "$LOG") 2>&1

echo "pr6 load: F1 F2 F4 F7a F7b ×$RUNS under $CPU_LOAD busy loops, log $LOG"

LOAD_PIDS=()
cleanup() {
  local p
  for p in "${LOAD_PIDS[@]+"${LOAD_PIDS[@]}"}"; do
    kill -TERM "$p" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT

i=0
while [ "$i" -lt "$CPU_LOAD" ]; do
  yes >/dev/null &
  LOAD_PIDS+=($!)
  i=$((i + 1))
done

run_filter() {
  local name=$1
  local filter=$2
  local n=$3
  echo "=== $name run $n/$RUNS filter=$filter ==="
  zig build --cache-dir "$CACHE" --prefix "$PREFIX" nvmet-smoke --summary all -Dnvmet-filter="$filter"
}

# Unique substrings: a bare "pr5 F1" also matches F11–F16.
FILTERS=(
  "F1:pr5 F1 —"
  "F2:pr5 F2 —"
  "F4:pr5 F4 —"
  "F7a:pr5 F7a —"
  "F7b:pr5 F7b —"
)

# Optional smoke-half of the load: a looping smoke filter, if the caller
# already built a test binary and pointed SQZ_SOAK_SMOKE_BIN at it.
if [ -n "${SQZ_SOAK_SMOKE_BIN:-}" ] && [ -x "$SQZ_SOAK_SMOKE_BIN" ]; then
  (
    while true; do
      "$SQZ_SOAK_SMOKE_BIN" 2>/dev/null || true
      sleep 1
    done
  ) &
  LOAD_PIDS+=($!)
  echo "pr6 load: smoke binary looping as load ($SQZ_SOAK_SMOKE_BIN)"
else
  echo "pr6 load: cpu load is yes ×$CPU_LOAD (set SQZ_SOAK_SMOKE_BIN for a smoke loop)"
fi

echo "load average: $(cut -d' ' -f1-3 /proc/loadavg)"

n=1
while [ "$n" -le "$RUNS" ]; do
  for entry in "${FILTERS[@]}"; do
    name=${entry%%:*}
    filter=${entry#*:}
    run_filter "$name" "$filter" "$n"
  done
  n=$((n + 1))
done

echo "pr6 load: all $RUNS passes of F1 F2 F4 F7a F7b succeeded"
echo "DONE"
