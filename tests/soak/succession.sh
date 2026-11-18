#!/bin/bash
# 20-succession soak at the derived ring or at --meta-journal-mb 1024
# (decision 0004 §8.3). Run as:
#   SQZ_VENUE_SLOT=1 scripts/venue tests/soak/succession.sh
#   SQZ_VENUE_SLOT=1 scripts/venue tests/soak/succession.sh --ring 1024
#
# Alternates SIGKILL, SIGSTOP past the claim TTL, and a clean manager
# unmount. Two joiners on two hosts write and fsync throughout; one data
# writer runs with them. Asserts:
#   0 member fail-stops; appender_park_expiries 0
#   writer_guard_succession_settles 20 (≥ 18 early)
#   every fsynced byte back; fsck 0
#   meta_pr_preempt_kept 0; data_pr_reconcile_in_window 0
# The large-ring run also asserts that no member's park reached its deadline.
# Records bring-up phases, meta_pr_park_extensions, and window lengths as
# counter deltas (no timing benchmarks).
set -euo pipefail
cd "$(dirname "$0")/../.."

RING=derived
CYCLES=${CYCLES:-20}
LOG_DIR=${SQZ_SOAK_LOG_DIR:-$HOME/tmp/pr6-soak}
while [ $# -gt 0 ]; do
  case $1 in
    --ring)
      RING=${2:?}
      shift 2
      ;;
    --ring=*)
      RING=${1#--ring=}
      shift
      ;;
    --cycles)
      CYCLES=${2:?}
      shift 2
      ;;
    --cycles=*)
      CYCLES=${1#--cycles=}
      shift
      ;;
    -h|--help)
      sed -n '2,20p' "$0"
      exit 0
      ;;
    *)
      echo "usage: $0 [--ring derived|1024] [--cycles N]" >&2
      exit 2
      ;;
  esac
done

case $RING in
  derived|1024) ;;
  *)
    echo "unknown --ring $RING (derived or 1024)" >&2
    exit 2
    ;;
esac

mkdir -p "$LOG_DIR"
LOG=$LOG_DIR/succession-$RING.log
: > "$LOG"
exec > >(stdbuf -oL tee -a "$LOG") 2>&1

echo "pr6 succession soak: ring=$RING cycles=$CYCLES log=$LOG"

CACHE=${SQZ_BUILD_ROOT:-/build}/zig-cache-v2
PREFIX=${PREFIX:-zig-out-linux}
export SQUEEZEFS_TEST_SOAK_CYCLES=$CYCLES
if [ "$RING" = 1024 ]; then
  export SQUEEZEFS_TEST_SOAK_JOURNAL_MB=1024
else
  unset SQUEEZEFS_TEST_SOAK_JOURNAL_MB
  export SQUEEZEFS_TEST_SOAK_JOURNAL_MB=0
fi

set +e
zig build --cache-dir "$CACHE" --prefix "$PREFIX" succession-soak --summary all
ec=$?
set -e
if [ "$ec" -ne 0 ] || grep -E "steps succeeded \\(.* failed\\)|error: '.+' failed|HUNG TEST:" "$LOG" >/dev/null; then
  echo "pr6 succession soak: FAILED ring=$RING cycles=$CYCLES exit=$ec"
  echo "$ec" > "$LOG_DIR/succession-$RING.exit"
  exit "${ec:-1}"
fi
echo "pr6 succession soak: ring=$RING cycles=$CYCLES done"
echo "DONE"
echo 0 > "$LOG_DIR/succession-$RING.exit"
