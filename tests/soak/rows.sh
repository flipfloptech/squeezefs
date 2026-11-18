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

# zig build test is units only. The smoke family is `zig build smoke`.
# -Dtest-filter is a substring, repeatable (OR), not a regex.
# The historical "27 rows" are the KILL / eviction / re-join family.
SMOKE_FILTERS=(
  "THE KILL ROWS"
  "CLEAN UNMOUNT AND REJOIN"
  "MEMBER JOINS A SET WHOSE MANAGER"
  "LIVE MEMBER EVICTED"
  "RE-JOIN"
  "A DEAD MANAGER HOLDING A STRIPED HOT DIRECTORY"
  "A LIVE MEMBER'S SLOW FOLLOW-UP"
  "THE N-DAEMON POSTURE, the manager half"
  "A SECOND MOUNT AT A LIVE JOINER"
  "A SECOND MOUNT WHOSE CENSUS"
  "THE SHARED HOT DIRECTORY ACROSS DAEMONS"
)
CHURN_FILTER='the owner under churn (decision 0003 §DM)'

filter_args=()
for f in "${SMOKE_FILTERS[@]}"; do
  filter_args+=(-Dtest-filter="$f")
done

echo "pr6 rows: $RUNS smoke passes plus the §DM churn unit, log $LOG"

echo "=== §DM churn unit ==="
zig build --cache-dir "$CACHE" --prefix "$PREFIX" test --summary all -Dtest-filter="$CHURN_FILTER"

n=1
while [ "$n" -le "$RUNS" ]; do
  echo "=== smoke eviction/re-join/KILL family run $n/$RUNS ==="
  zig build --cache-dir "$CACHE" --prefix "$PREFIX" smoke --summary all "${filter_args[@]}"
  n=$((n + 1))
done

echo "pr6 rows: all $RUNS passes succeeded"
echo "DONE"
