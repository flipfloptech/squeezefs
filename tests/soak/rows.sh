#!/bin/bash
# The eviction / re-join / KILL smoke rows ×3, and the §DM churn unit
# (decision 0004 §8.3). Run as: SQZ_VENUE_SLOT=1 scripts/venue tests/soak/rows.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

LOG_DIR=${SQZ_SOAK_LOG_DIR:-$HOME/tmp/pr6-soak}
CACHE=${SQZ_BUILD_ROOT:-/build}/zig-cache-v2
PREFIX=${PREFIX:-zig-out-linux}
RUNS=${RUNS:-3}
mkdir -p "$LOG_DIR"
LOG=$LOG_DIR/rows.log
: > "$LOG"
exec > >(stdbuf -oL tee -a "$LOG") 2>&1

# Historical "27 rows" / 71 tests: the KILL, eviction, and re-join family.
# -Dtest-filter replaces the default smoke: prefix, so this must still
# match those names.
ROWS_FILTER='THE KILL ROWS|CLEAN UNMOUNT AND REJOIN|MEMBER JOINS A SET WHOSE MANAGER|LIVE MEMBER EVICTED|RE-JOIN|RE-JOINS'
CHURN_FILTER='the owner under churn (decision 0003 §DM)'

echo "pr6 rows: $RUNS passes, log $LOG"

n=1
while [ "$n" -le "$RUNS" ]; do
  echo "=== smoke eviction/re-join/KILL family run $n/$RUNS ==="
  zig build --cache-dir "$CACHE" --prefix "$PREFIX" test --summary all -Dtest-filter="$ROWS_FILTER"
  echo "=== §DM churn unit run $n/$RUNS ==="
  zig build --cache-dir "$CACHE" --prefix "$PREFIX" test --summary all -Dtest-filter="$CHURN_FILTER"
  n=$((n + 1))
done

echo "pr6 rows: all $RUNS passes succeeded"
echo "DONE"
