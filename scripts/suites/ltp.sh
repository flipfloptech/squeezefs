#!/usr/bin/env bash
# LTP against a real mount (charter §11 "Gates": the release gate runs the
# full LTP): clone + build + install once (/build/ltp), then kirk (LTP's
# runner since `runltp` was retired) per runtest file with the tests'
# TMPDIR ON THE MOUNT, the failing tags folded to ids, every failure CONTROLLED against the venue's ext4 (/build) — a test
# that fails identically with its TMPDIR on ext4 is the venue's
# (container limits: loop devices, swap, cgroup v1, seccomp…) and is
# listed as `venue`, never adjudicated as ours — then the adjudication
# ledger (`docs/gates/ltp-exceptions.md`) over what remains, `fsck
# --offline --scrub` after every file, the multi-run discipline.
#
#   scripts/venue scripts/suites/ltp.sh                 # fs then syscalls, SUITE_RUNS each
#   LTP_FILES=fs SUITE_RUNS=1 scripts/venue scripts/suites/ltp.sh
#   LTP_FILES="fs syscalls" LTP_TIMEOUT_S=14400 …        # bound per runtest file
#   LTP_FILES=fs LTP_PATTERN='^gf27$' SUITE_RUNS=1 …     # one test (triage; kirk's --run-pattern)
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

LTP_REPO=${LTP_REPO:-https://github.com/linux-test-project/ltp}
KIRK_REPO=${KIRK_REPO:-https://github.com/linux-test-project/kirk}
LTP_SRC="$SUITES_ROOT/ltp"
KIRK_DIR="$SUITES_ROOT/kirk"
LTP_PREFIX=${LTP_PREFIX:-${SQZ_BUILD_ROOT:-/build}/ltp}
LTP_FILES=${LTP_FILES:-"fs syscalls"}
# Wall bound per runtest file (kirk's --suite-timeout): `syscalls` is
# ~1,400 tests; `fs` ~90. A hung test is a defect to repro, never a reason
# to lose the run's report. Per-test bound: kirk's --exec-timeout.
LTP_TIMEOUT_S=${LTP_TIMEOUT_S:-10800}
LTP_EXEC_TIMEOUT_S=${LTP_EXEC_TIMEOUT_S:-900}
# Tags skipped in THIS venue only (kirk's --skip-file), each with the
# control run that proved it the venue's; kept beside the harness, read
# by the evidence note. Never a filesystem semantic.
LTP_VENUE_SKIP="$SUITES_DIR/ltp-venue-skip.txt"

ltp_build() {
  if [ ! -d "$LTP_SRC/.git" ]; then
    suite_log "cloning $LTP_REPO"
    git clone -q --depth 1 "$LTP_REPO" "$LTP_SRC"
  fi
  if [ ! -d "$KIRK_DIR/.git" ]; then
    # LTP retired `runltp` (2025): kirk is its runner.
    suite_log "cloning $KIRK_REPO"
    git clone -q --depth 1 "$KIRK_REPO" "$KIRK_DIR"
  fi
  if [ ! -d "$LTP_PREFIX/testcases/bin" ]; then
    suite_log "building LTP ($(cd "$LTP_SRC" && git log -1 --format=%h)) into $LTP_PREFIX"
    (cd "$LTP_SRC" && make -s autotools >"$RESULTS/ltp-build.log" 2>&1 && ./configure --prefix="$LTP_PREFIX" >>"$RESULTS/ltp-build.log" 2>&1 && make -s -j"$(nproc)" >>"$RESULTS/ltp-build.log" 2>&1 && make -s install >>"$RESULTS/ltp-build.log" 2>&1) \
      || { tail -30 "$RESULTS/ltp-build.log" >&2; suite_die "LTP build failed"; }
  fi
  { echo "ltp $(cd "$LTP_SRC" && git log -1 --format='%H %cd')"; echo "kirk $(cd "$KIRK_DIR" && git log -1 --format='%H %cd')"; } > "$RESULTS/ltp-version.txt"
}

# kirk's JSON report → one line per test: `tag<TAB>result<TAB>command`.
ltp_report_rows() {
  python3 - "$1" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
for t in r.get("results", []):
    tt = t["test"]
    cmd = " ".join([tt.get("command", "")] + list(tt.get("arguments", [])))
    print(f'{t["test_fqn"]}\t{t.get("status", tt.get("result", "?"))}\t{cmd}')
PY
}

# The control run: the failing tag's command with TMPDIR on ext4. Prints
# `venue` if it fails there too, `ours` otherwise. LTP exit codes: 0 pass,
# 32 TCONF (skipped), anything else a failure.
ltp_control() {
  local tag=$1 rows=$2 n=$3
  local cmd; cmd=$(awk -F'\t' -v t="$tag" '$1 == t { print $3; exit }' "$rows")
  [ -n "$cmd" ] || { echo ours; return; }
  local ctl="$SUITES_TMP/ltp-ctl-$n"; rm -rf "$ctl"; mkdir -p "$ctl"
  local rc=0
  (cd "$ctl" && PATH="$LTP_PREFIX/testcases/bin:$PATH" TMPDIR="$ctl" LTPROOT="$LTP_PREFIX" LTP_TIMEOUT_MUL=${LTP_TIMEOUT_MUL:-4} timeout -s KILL "$LTP_EXEC_TIMEOUT_S" sh -c "$cmd" > "$RESULTS/ctl-$tag-run$n.out" 2>&1) || rc=$?
  rm -rf "$ctl"
  if [ "$rc" != 0 ] && [ "$rc" != 32 ]; then echo venue; else echo ours; fi
}

# One runtest file on a fresh set. Ids of OUR failures (`file/tag`) to $3.
ltp_run() {
  local file=$1 n=$2 failures=$3
  local tag="$file-run$n"
  : > "$failures"
  suite_mount "$tag"
  local mnt=${SUITE_MNT[$tag]}
  local tmp="$mnt/ltp"; mkdir -p "$tmp"; chmod 1777 "$tmp"
  local report="$RESULTS/ltp-$tag.json" rows="$RESULTS/ltp-$tag.rows" ktmp="$RESULTS/kirk-$tag"
  mkdir -p "$ktmp"
  local skip=()
  if [ -s "$LTP_VENUE_SKIP" ]; then grep -v '^#' "$LTP_VENUE_SKIP" | awk '{print $1}' | grep . > "$RESULTS/venue-skip.list" || true; [ -s "$RESULTS/venue-skip.list" ] && skip=(--skip-file "$RESULTS/venue-skip.list"); fi
  local rc=0 t0 t1
  t0=$(date +%s)
  # TMPDIR ON THE MOUNT is the whole point: every test's tst_tmpdir() lands
  # there. LTP_TIMEOUT_MUL leaves LTP's own per-test timeouts in place,
  # ×4 for the FUSE round trip (fcntl12 opens one file getdtablesize()
  # times inside a 30 s budget — ≈ 50 µs per OPEN across the ring is the
  # transport's cost, not a semantic; LTP's own advice is this multiplier).
  (cd "$KIRK_DIR" && LTPROOT="$LTP_PREFIX" TMPDIR="$tmp" LTP_TIMEOUT_MUL=${LTP_TIMEOUT_MUL:-4} timeout -s KILL $((LTP_TIMEOUT_S + 120)) \
     ./kirk --no-colors --tmp-dir "$ktmp" --run-suite "$file" ${LTP_PATTERN:+--run-pattern "$LTP_PATTERN"} --json-report "$report" --suite-timeout "$LTP_TIMEOUT_S" --exec-timeout "$LTP_EXEC_TIMEOUT_S" "${skip[@]}" > "$RESULTS/kirk-$tag.console" 2>&1) || rc=$?
  t1=$(date +%s)
  [ -s "$report" ] || { tail -20 "$RESULTS/kirk-$tag.console" >&2; suite_log "$file run $n: kirk produced no report (rc $rc)"; suite_umount "$tag"; return 1; }
  ltp_report_rows "$report" > "$rows"
  local all="$RESULTS/ltp-$tag.allfail"
  awk -F'\t' '$2 == "fail" || $2 == "brok" || $2 == "broken" || $2 == "timeout" { print $1 }' "$rows" | sort -u > "$all"
  local total pass fail confn
  total=$(grep -c . "$rows" || true); pass=$(awk -F'\t' '$2 == "pass"' "$rows" | wc -l); fail=$(grep -c . "$all" || true); confn=$(awk -F'\t' '$2 == "conf" || $2 == "skip"' "$rows" | wc -l)
  suite_log "$file run $n: kirk rc=$rc, $total tests: $pass pass, $fail fail/brok/timeout, $confn conf, $((t1 - t0)) s wall"
  [ "$total" -gt 0 ] || { suite_log "$file run $n: the report lists no tests"; suite_umount "$tag"; return 1; }
  # Control every failure against ext4.
  : > "$failures"; : > "$RESULTS/ltp-$tag.venue"
  local t v
  while read -r t; do
    [ -n "$t" ] || continue
    v=$(ltp_control "$t" "$rows" "$n")
    if [ "$v" = venue ]; then echo "$t" >> "$RESULTS/ltp-$tag.venue"; else echo "$file/$t" >> "$failures"; fi
  done < "$all"
  local nv no; nv=$(grep -c . "$RESULTS/ltp-$tag.venue" || true); no=$(grep -c . "$failures" || true)
  suite_log "$file run $n: $no failure(s) OURS (pass on ext4), $nv the venue's (fail on ext4 too: ltp-$tag.venue)"
  if [ "$no" != 0 ]; then
    suite_log "our failures:"
    while read -r t; do
      t=${t#*/}
      awk -F'\t' -v t="$t" '$1 == t { print "    " $1 " [" $2 "]: " $3 }' "$rows" >&2
      python3 - "$report" "$t" <<'PY' >&2
import json, sys
r = json.load(open(sys.argv[1]))
for t in r.get("results", []):
    if t["test_fqn"] == sys.argv[2]:
        for line in t["test"].get("log", "").splitlines():
            if "TFAIL" in line or "TBROK" in line:
                print("      " + line[:220])
PY
    done < "$failures"
  fi
  rm -rf "$ktmp"
  suite_umount "$tag"
  suite_daemon_alerts "$tag" || return 1
  suite_fsck "$tag" || return 1
  [ "$rc" = 0 ] || { suite_log "kirk rc $rc: the run did not complete cleanly"; return 1; }
  return 0
}

main() {
  suite_prepare ltp
  ltp_build
  local verdict=0 file n
  for file in $LTP_FILES; do
    for n in $(seq 1 "$SUITE_RUNS"); do
      suite_run_count "$n"
      local failures="$RESULTS/ltp-$file-run$n.failures"
      local run_rc=0 jrc=0
      ltp_run "$file" "$n" "$failures" || run_rc=$?
      suite_judge ltp "$failures" "$file/" || jrc=$?
      if [ "$run_rc" != 0 ] || [ "$jrc" != 0 ]; then
        suite_log "$file run $n did NOT pass (run rc $run_rc, ledger rc $jrc): the count aborts"
        verdict=1; break 2
      fi
      suite_log "$file run $n PASSED"
    done
  done
  [ "$verdict" = 0 ] && suite_log "LTP ($LTP_FILES): $SUITE_RUNS consecutive clean run(s) per file on $("$SQZ_BIN" --version)"
  exit $verdict
}

main "$@"
