#!/usr/bin/env bash
# The fstests host lock's test (decision 0003 §CS): two harnesses started
# together serialize — the second says whose run it waits for, the two
# runs' hold windows never overlap, each sees the helpers point at its own
# tree, and what stood before is back after both.
#
#   scripts/venue scripts/suites/test-host-lock.sh
#
# Each harness runs fstests.sh up to its host state only
# (`FSTESTS_TEST_HOLD_S`: no build, no check, no set).
set -euo pipefail
SUITES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOLD_S=${HOLD_S:-4}
work=$(mktemp -d "${SQZ_BUILD_ROOT:-/build}/tmp/host-lock-test-$$-XXXX")
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$work"
}
trap cleanup EXIT
fail() { echo "test-host-lock: FAIL: $*" >&2; exit 1; }

# A harness from a tree before the lock re-points the helpers without it.
if pgrep -f 'check -R xunit' >/dev/null; then fail "an fstests check runs on this host: run this test when none does"; fi
before=$(readlink /sbin/mount.fuse.squeezefs 2>/dev/null || echo "<absent>")
env_before=$(md5sum /run/squeezefs-suites.env 2>/dev/null | cut -d' ' -f1 || echo "<absent>")
declare -A pid_of
for who in a b; do
  SUITE_RESULTS="$work/results-$who" FSTESTS_TEST_HOLD_S=$HOLD_S FSTESTS_VENUE_WAIT_S=$((HOLD_S * 10)) \
    bash "$SUITES_DIR/fstests.sh" > "$work/$who.log" 2>&1 &
  pids+=($!)
  pid_of[$who]=$!
done
rcs=()
for p in "${pids[@]}"; do rc=0; wait "$p" || rc=$?; rcs+=("$rc"); done
pids=()
[ "${rcs[*]}" = "0 0" ] || { tail -n 8 "$work"/a.log "$work"/b.log >&2; fail "harness exits ${rcs[*]}"; }

window() {   # log → "from until" (ns)
  local from until
  from=$(grep -o 'hold probe: from [0-9]*' "$1" | awk '{print $4}')
  until=$(grep -o 'hold probe: until [0-9]*' "$1" | awk '{print $4}')
  [ -n "$from" ] && [ -n "$until" ] || fail "no hold window in $1"
  echo "$from $until"
}
read -r a_from a_until <<< "$(window "$work/a.log")"
read -r b_from b_until <<< "$(window "$work/b.log")"
if [ "$a_from" -lt "$b_from" ]; then first=a; second=b; f_until=$a_until; s_from=$b_from
else first=b; second=a; f_until=$b_until; s_from=$a_from; fi
[ "$s_from" -ge "$f_until" ] || fail "the hold windows overlap: $first until $f_until, $second from $s_from"

first_pid=${pid_of[$first]}
grep -q "waiting for suite run by pid $first_pid (owner " "$work/$second.log" || { cat "$work/$second.log" >&2; fail "the second harness never said whose run it waited for (pid $first_pid)"; }
grep -q 'waiting for suite run' "$work/$first.log" && fail "the first harness waited"
for who in a b; do
  grep -q "mount helper → $SUITES_DIR/mount.squeezefs" "$work/$who.log" || fail "$who's helpers did not point at this tree inside its hold"
done
after=$(readlink /sbin/mount.fuse.squeezefs 2>/dev/null || echo "<absent>")
env_after=$(md5sum /run/squeezefs-suites.env 2>/dev/null | cut -d' ' -f1 || echo "<absent>")
[ "$after" = "$before" ] || fail "the mount helper points at $after after both runs (stood at $before)"
[ "$env_after" = "$env_before" ] || fail "the venue env file changed across the runs"
grep -o 'waiting for suite run by .*' "$work/$second.log" | sed 's/^/test-host-lock: /'
echo "test-host-lock: PASS — $second waited for $first (pid $first_pid); windows [$a_from, $a_until] / [$b_from, $b_until] disjoint; helpers restored to $before"
