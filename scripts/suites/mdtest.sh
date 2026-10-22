#!/usr/bin/env bash
# An `mpirun` of mdtest against a real mount — a release row beside LTP
# and fstests (owner, 2026-10-22; class C: the release window, the venue
# exclusive, the installed binary). The perf shape the owner means
# (`scripts/perf/matrix.sh`, `docs/evidence/rung-10-create-path-*.md`):
#
#   mpirun --allow-run-as-root --oversubscribe -np 32 mdtest -n 10000 -u -d <mnt>/mdtest
#
# — every rank creates, stats, reads (open/close: 0 bytes) and removes
# 10,000 files and 10,000 directories in a directory of its own (`-u`),
# the tree created and removed around them — then the same at -np 64.
#
# THE LAW (pass; the rates are evidence, never the law): per leg, mdtest
# exits 0 with its header naming every rank and item (`<np> tasks, <np×n>
# files/directories`), every phase in its SUMMARY table at a positive rate
# (a rank's error aborts mdtest — `ERROR`/`MPI_ABORT` lines — and a phase
# missing from the table is a phase that did not run), the tree empty
# after the removal phases, `df` back to its baseline (used bytes AND used
# inodes: the live-inode population is minted − destroyed, and a corpse is
# destroyed after the kernel's last FORGET — the suite waits for the
# observable, bounded), the mount healthy after (`suite_judge_mount`:
# `.stats` readable, `invariant_tripwires` 0, the FUSE op watchdog never
# overdue); per run, no tripwire in the daemon log and `fsck --offline
# --scrub` clean over the unmounted set; the multi-run discipline
# (SUITE_RUNS, default 3: a FRESH set per run, the count aborts at the first
# non-pass). FAIL EARLY (owner ruling 2026-10-17): the first failing check
# that is no ledger row (`docs/gates/mdtest-exceptions.md`, ids
# `np<N>/<check>`) ends the run — the -np 64 leg is not run behind a failed
# -np 32.
#
#   scripts/venue scripts/suites/mdtest.sh                 # the 3-run gate
#   SUITE_RUNS=1 scripts/venue scripts/suites/mdtest.sh
#   MDTEST_NPS="32" MDTEST_N=1000 SUITE_RUNS=1 …           # a short scoping run
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

MDTEST=${MDTEST:-/opt/sqz-tools/bin/mdtest}
MPIRUN=${MPIRUN:-mpirun}
MDTEST_NPS=${MDTEST_NPS:-"32 64"}
MDTEST_N=${MDTEST_N:-10000}
# The set is the harness's (a 1 GiB metadata volume): the -np 64 leg's
# 1.28 M inodes fit — measured 2026-10-22, the two legs end with 2,545 of
# the volume's 4,030 heap extents free, the rest held by 1,402 resident
# leaves (emptied leaves stay allocated: the heap does not return to its
# baseline the way `df` does).
# Wall bound per leg (`timeout -s KILL`): the -np 64 leg is ~2 min at the
# measured 100 k creates/s; a leg past this is a wedge to read, never a
# number to wait for.
MDTEST_TIMEOUT_S=${MDTEST_TIMEOUT_S:-1800}
# How long `df` is given to return to its baseline after the removal
# phases (the FORGET → destroy drain is asynchronous; measured seconds).
MDTEST_SETTLE_S=${MDTEST_SETTLE_S:-120}
# Every phase mdtest runs under these flags; a missing row is a failure.
MDTEST_PHASES=("Directory creation" "Directory stat" "Directory removal" "File creation" "File stat" "File read" "File removal" "Tree creation" "Tree removal")

mdtest_require() {
  [ -x "$MDTEST" ] || suite_die "no mdtest at $MDTEST (scripts/dev/provision-local-venue --apply installs IOR/mdtest; MDTEST=… names another)"
  command -v "$MPIRUN" >/dev/null || suite_die "no $MPIRUN on PATH (OpenMPI)"
  "$MPIRUN" --version 2>/dev/null | head -1 > "$RESULTS/mpirun-version.txt" || true
  { echo "mdtest $("$MDTEST" --help 2>&1 | grep -o 'mdtest-[0-9][^ ]*' | head -1)"; cat "$RESULTS/mpirun-version.txt"; } > "$RESULTS/mdtest-version.txt"
  suite_log "instrument: $(tr '\n' ' ' < "$RESULTS/mdtest-version.txt")"
}

# `df` of the mount as `<used bytes> <used inodes>`.
mdtest_df() {
  df -B1 --output=used,iused "$1" | tail -1 | awk '{ print $1, $2 }'
}

# The run's failure ids: appended, and the first that is no ledger row
# stops the run (fail early).
MDTEST_STOP=0
mdtest_fail() {   # id message
  echo "$1" >> "$FAILURES"
  if grep -qxF "$1" "$RESULTS/ledger-ids.txt"; then suite_log "FAIL (adjudicated ledger row) $1: $2"
  else suite_log "FAIL $1: $2"; MDTEST_STOP=1; fi
}

# One leg: -np $2 on run $1's mount. Its checks' failure ids go to $FAILURES.
mdtest_leg() {
  local n=$1 np=$2 tag=$3
  local mnt=${SUITE_MNT[$tag]} out="$RESULTS/run$n-np$np.txt" id="np$np"
  # The run's one `-d` directory (mdtest removes its tree, never the
  # directory): the baseline is the root + this directory. Nothing is
  # removed between legs — an rmdir'd directory is a corpse until the
  # kernel's FORGET and the next drain, and a baseline read over it counted
  # one inode the leg then rightly destroyed (the first scoping run).
  local dir="$mnt/mdtest"
  local base; base=$(mdtest_df "$mnt")
  local before; before=$(suite_stats_snapshot "$tag" "np$np-before")
  suite_log "run $n: $MPIRUN --allow-run-as-root --oversubscribe -np $np $MDTEST -n $MDTEST_N -u -d $dir (df baseline: used $base)"
  local rc=0 t0 t1
  t0=$(date +%s)
  (cd "$RESULTS" && timeout -s KILL "$MDTEST_TIMEOUT_S" "$MPIRUN" --allow-run-as-root --oversubscribe -np "$np" "$MDTEST" -n "$MDTEST_N" -u -d "$dir") > "$out" 2>&1 || rc=$?
  t1=$(date +%s)
  local items=$((np * MDTEST_N))
  suite_log "run $n -np $np: mdtest rc=$rc, $((t1 - t0)) s wall"
  # 1. The instrument itself: exit 0, no rank error.
  [ "$rc" = 0 ] || { tail -15 "$out" | cut -c1-200 | sed 's/^/    /' >&2; mdtest_fail "$id/exit" "mdtest exit $rc (137 = the $MDTEST_TIMEOUT_S s wall bound)"; }
  local errs; errs=$(grep -cE '^ERROR|MPI_ABORT|MPI_Abort|mpirun (noticed|detected)' "$out" || true)
  [ "${errs:-0}" = 0 ] || { grep -E '^ERROR|MPI_ABORT|MPI_Abort|mpirun (noticed|detected)' "$out" | head -5 | cut -c1-200 | sed 's/^/    /' >&2; mdtest_fail "$id/rank-error" "$errs rank error line(s)"; }
  # 2. Every rank and item: the header `<np> tasks, <np×n> files/directories`.
  grep -qE "^$np tasks, $items files/directories" "$out" || mdtest_fail "$id/header" "no '$np tasks, $items files/directories' header ($(grep -E '^[0-9]+ tasks,' "$out" | head -1))"
  # 3. Every phase in the SUMMARY rate table at a positive rate; the rates
  #    to the result file (evidence).
  local rates="$RESULTS/rates-run$n-np$np.tsv" ph
  awk -v np="$np" '/^SUMMARY rate/ { s = 1; next } /^SUMMARY time|^-- finished/ { s = 0 } s && /^   [A-Z]/ && $1 != "Operation" { name = $1 " " $2; printf "%s\t%s\t%s\t%s\t%s\n", np, name, $3, $4, $5 }' "$out" > "$rates"
  for ph in "${MDTEST_PHASES[@]}"; do
    local max; max=$(awk -F'\t' -v p="$ph" '$2 == p { print $3; exit }' "$rates")
    if [ -z "$max" ]; then mdtest_fail "$id/phase:$ph" "no '$ph' row in the SUMMARY rate table"
    elif ! awk -v m="$max" 'BEGIN { exit !(m > 0) }'; then mdtest_fail "$id/phase:$ph" "'$ph' rate $max (must be > 0)"; fi
  done
  suite_log "run $n -np $np rates (ops/s, max): $(awk -F'\t' '{ printf "%s %.0f; ", $2, $3 }' "$rates")"
  # 4. The tree is empty after the removal phases.
  local left; left=$(find "$dir" -mindepth 1 2>/dev/null | wc -l)
  [ "$left" = 0 ] || { find "$dir" -mindepth 1 | head -5 | sed 's/^/    /' >&2; mdtest_fail "$id/tree" "$left entries left under $dir after mdtest's removal phases"; }
  # 5. `df` back to the baseline — used bytes and used inodes — within the
  #    settle bound (the drain is asynchronous: wait for the observable).
  local now deadline=$((SECONDS + MDTEST_SETTLE_S)) ts0=$SECONDS
  while :; do
    now=$(mdtest_df "$mnt")
    [ "$now" = "$base" ] && break
    [ "$SECONDS" -lt "$deadline" ] || break
    sleep 1
  done
  if [ "$now" = "$base" ]; then suite_log "run $n -np $np: df back to the baseline (used $base) after $((SECONDS - ts0)) s"
  else mdtest_fail "$id/df" "df did not return to its baseline within $MDTEST_SETTLE_S s: used '$now' vs baseline '$base' (bytes inodes)"; fi
  # 6. The mount healthy after; the leg's counters for the record.
  suite_judge_mount "$tag" "np$np-after" || mdtest_fail "$id/mount" "the mount is not healthy after the leg (see above)"
  local after="$RESULTS/stats-$tag-np$np-after.json"
  [ -f "$after" ] && suite_log "run $n -np $np counters: fuse_ops +$(suite_stats_delta "$before" "$after" fuse_ops 2>/dev/null) destroys +$(suite_stats_delta "$before" "$after" fuse_reclaim_destroys 2>/dev/null) live_inodes $(suite_stat "$after" live_inodes 2>/dev/null) heap_free_extents $(suite_stat "$after" heap_free_extents 2>/dev/null) of $(suite_stat "$after" heap_total_extents 2>/dev/null)"
}

# One run: a fresh set, every leg in MDTEST_NPS (stopping at the first
# unadjudicated failure), the daemon's alerts, fsck.
mdtest_run() {
  local n=$1
  local tag="run$n"
  suite_mount "$tag"
  mkdir -p "${SUITE_MNT[$tag]}/mdtest"
  local np
  for np in $MDTEST_NPS; do
    mdtest_leg "$n" "$np" "$tag"
    if [ "$MDTEST_STOP" = 1 ]; then
      suite_log "run $n: a check failed and is no ledger row — stopping the run here (fail early)"
      SUITE_JUDGE_NO_STALE=1
      break
    fi
  done
  suite_umount "$tag"
  local ok=0
  suite_daemon_alerts "$tag" || { echo "run/tripwire" >> "$FAILURES"; ok=1; }
  suite_fsck "$tag" || { echo "run/fsck" >> "$FAILURES"; ok=1; }
  [ "$MDTEST_STOP" = 0 ] || return 1
  return $ok
}

main() {
  suite_prepare mdtest
  mdtest_require
  suite_ledger_rows mdtest | cut -f1 > "$RESULTS/ledger-ids.txt"
  local n verdict=0
  for n in $(seq 1 "$SUITE_RUNS"); do
    suite_run_count "$n"
    FAILURES="$RESULTS/run$n.failures"; : > "$FAILURES"
    MDTEST_STOP=0
    local run_rc=0 jrc=0
    mdtest_run "$n" || run_rc=$?
    suite_judge mdtest "$FAILURES" || jrc=$?
    if [ "$run_rc" != 0 ] || [ "$jrc" != 0 ]; then
      suite_log "run $n did NOT pass (run rc $run_rc, ledger rc $jrc): the count aborts; fix, then restart from run 1"
      verdict=1; break
    fi
    suite_log "run $n PASSED (every leg complete, df at its baseline, the mount healthy, fsck clean, no tripwire)"
  done
  [ "$verdict" = 0 ] && suite_log "mdtest (-np $MDTEST_NPS, -n $MDTEST_N -u): $SUITE_RUNS consecutive clean run(s) on $("$SQZ_BIN" --version)"
  exit $verdict
}

main "$@"
