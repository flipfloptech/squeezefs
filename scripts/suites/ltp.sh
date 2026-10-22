#!/usr/bin/env bash
# LTP against a real mount (charter §11 "Gates": the release gate runs the
# full LTP): clone + build + install once (/build/ltp), then kirk (LTP's
# runner since `runltp` was retired) per runtest file with the tests'
# TMPDIR ON THE MOUNT, the failing tags folded to ids, every failure CONTROLLED against the venue's ext4 (/build) — a test
# that fails ON THE SAME ASSERTIONS with its TMPDIR on ext4, run from
# kirk's cwd, is the venue's (the kernel's, the host's: a sysctl, a
# missing feature, a filesystem LTP formats on its loop device) and is
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
  ltp_rebuild_modules
  { echo "ltp $(cd "$LTP_SRC" && git log -1 --format='%H %cd')"; echo "kirk $(cd "$KIRK_DIR" && git log -1 --format='%H %cd')"; echo "modules for $(awk '/^LINUX_VERSION[[:space:]]*:=/ { print $3 }' "$LTP_SRC/include/mk/config.mk")"; } > "$RESULTS/ltp-version.txt"
}

# LTP's `configure` bakes the RUNNING kernel into `include/mk/config.mk`
# (`LINUX_DIR`, `LINUX_VERSION`) and its module testcases (`init_module`,
# `finit_module`, `delete_module`, the device-driver and insmod/lsmod rows)
# build against it, so an install outlives the kernel it was built on: on
# 2026-10-22 the modules carried `vermagic 7.2.8-7-omarchy-sqz` under
# 7.3.0-1.7-mainline-sqz and seven tests failed ENOEXEC on both sides of
# the control — classified "the venue's" every run. A kernel change
# reconfigures (5 s) and rebuilds + reinstalls every module directory
# (17 s); the rest of the install is kernel-independent.
ltp_rebuild_modules() {
  local built; built=$(awk '/^LINUX_VERSION[[:space:]]*:=/ { print $3 }' "$LTP_SRC/include/mk/config.mk" 2>/dev/null)
  local running; running=$(uname -r)
  [ "$built" != "$running" ] || return 0
  [ -d "/lib/modules/$running/build" ] || suite_die "LTP's modules were built for kernel $built and /lib/modules/$running/build is absent: the module testcases cannot be rebuilt for this kernel"
  suite_log "LTP's modules were built for kernel $built; reconfiguring for $running and rebuilding the module testcases"
  local log="$RESULTS/ltp-modules.log" d
  (cd "$LTP_SRC" && ./configure --prefix="$LTP_PREFIX" > "$log" 2>&1) || { tail -20 "$log" >&2; suite_die "LTP configure failed"; }
  for d in $(cd "$LTP_SRC" && grep -rl 'include $(top_srcdir)/include/mk/module.mk' --include=Makefile testcases | xargs -n1 dirname); do
    (cd "$LTP_SRC/$d" && { make -s clean >> "$log" 2>&1 || true; } && make -s >> "$log" 2>&1 && make -s install >> "$log" 2>&1) \
      || { tail -20 "$log" >&2; suite_die "LTP module testcases in $d failed to rebuild for $running"; }
  done
  built=$(awk '/^LINUX_VERSION[[:space:]]*:=/ { print $3 }' "$LTP_SRC/include/mk/config.mk")
  [ "$built" = "$running" ] || suite_die "LTP's config.mk still names kernel $built after configure"
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
# `venue` if it fails there too ON THE SAME ASSERTIONS, `ours` otherwise.
# LTP exit codes: 0 pass, 32 TCONF (skipped), anything else a failure. The
# test's own failing lines come from kirk's JSON report.
ltp_control() {
  local tag=$1 rows=$2 n=$3 report=$4
  local cmd; cmd=$(awk -F'\t' -v t="$tag" '$1 == t { print $3; exit }' "$rows")
  local ours="$RESULTS/ctl-$tag-run$n.ours"
  if [ -s "$report" ]; then ltp_fail_lines report "$report" "$tag" > "$ours" 2>/dev/null || : > "$ours"; else ours=""; fi
  ltp_control_cmd "$tag" "$cmd" "$n" "$ours"
}

# The failing assertions (TFAIL/TBROK lines) of one test, normalized so the
# two sides compare: kirk's colours stripped, the tmpdir path, hex words and
# numbers (pids, sizes, line numbers) folded. `report <json> <tag>` reads
# kirk's report; `debug <debug.log> <tag>` the per-command stdout kirk logs
# as it runs (the fail-early watcher, before the report exists); `out
# <file>` a control's captured stdout.
ltp_fail_lines() {
  python3 - "$@" <<'PY'
import ast, json, re, sys
kind, src = sys.argv[1], sys.argv[2]
tag = sys.argv[3] if len(sys.argv) > 3 else None
text = None
if kind == "report":
    for t in json.load(open(src)).get("results", []):
        if t["test_fqn"] == tag:
            text = t["test"].get("log", "")
elif kind == "debug":
    for line in open(src, errors="replace"):
        i = line.find(" - DEBUG - return data=")
        if i < 0:
            continue
        try:
            d = ast.literal_eval(line[i + len(" - DEBUG - return data="):].strip())
        except (ValueError, SyntaxError):
            continue
        if isinstance(d, dict) and d.get("command") == tag:
            text = d.get("stdout", "")
else:
    text = open(src, errors="replace").read()
if text is None:
    sys.exit(1)
out = []
for l in text.splitlines():
    l = re.sub(r"\x1b\[[0-9;]*m", "", l)
    if not re.search(r"\bT(FAIL|BROK)\b", l):
        continue
    l = re.sub(r"/build/tmp/\S+", "<tmp>", l)
    l = re.sub(r"0x[0-9a-fA-F]+", "<hex>", l)
    l = re.sub(r"\d+", "<n>", l)
    out.append(l.strip())
print("\n".join(sorted(out)))
PY
}

# The control run of one command (`ltp_control` with the command in hand:
# the fail-early watcher reads it off the runtest file before the report
# exists). kirk runs every test from `testcases/bin` (`libkirk/ltp.py`
# `_tc_folder`), so the control does too: a test that opens a sibling by a
# relative name (`init_module01`: `init_module.ko`) failed ENOENT from any
# other cwd and the control said "venue" for the wrong reason. `$4` names a
# file of the test's own failing lines (`ltp_fail_lines`); the verdict is
# `venue` only when the ext4 run fails on exactly those — a control that
# fails differently is no control (owner ruling 2026-10-22: a skip needs
# certainty), and the run is judged ours.
ltp_control_cmd() {
  local tag=$1 cmd=$2 n=$3 ours=${4:-}
  [ -n "$cmd" ] || { echo ours; return; }
  local ctl="$SUITES_TMP/ltp-ctl-$n-$tag"; rm -rf "$ctl"; mkdir -p "$ctl"
  local rc=0 out="$RESULTS/ctl-$tag-run$n.out"
  (cd "$LTP_PREFIX/testcases/bin" && PATH="$LTP_PREFIX/testcases/bin:$PATH" TMPDIR="$ctl" LTPROOT="$LTP_PREFIX" LTP_TIMEOUT_MUL=${LTP_TIMEOUT_MUL:-4} timeout -s KILL "$LTP_EXEC_TIMEOUT_S" sh -c "$cmd" > "$out" 2>&1) || rc=$?
  rm -rf "$ctl"
  if [ "$rc" = 0 ] || [ "$rc" = 32 ]; then echo ours; return; fi
  [ -n "$ours" ] && [ -f "$ours" ] || { echo venue; return; }
  ltp_fail_lines out "$out" > "$out.lines" 2>/dev/null || : > "$out.lines"
  if cmp -s "$ours" "$out.lines"; then echo venue; return; fi
  suite_log "$tag fails on ext4 too, but on different assertions (ours vs ext4: ctl-$tag-run$n.ours vs ctl-$tag-run$n.out.lines) — judged ours"
  echo ours
}

# The tests must start with SIGINT (2) and SIGQUIT (3) at their defaults:
# `kill11` sends both to a child and waits. Prints the leader's SigIgn
# mask; the exit code is the verdict.
ltp_check_dispositions() {
  local mask
  mask=$(awk '/^SigIgn/{print $2}' "/proc/$1/status" 2>/dev/null)
  echo "${mask:-unreadable}"
  [ -n "$mask" ] && [ $(( 0x$mask & 0x6 )) -eq 0 ]
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
  local console="$RESULTS/kirk-$tag.console"
  # kirk is an asynchronous list of this (non-interactive, no job control)
  # shell, which bash starts with SIGINT and SIGQUIT IGNORED; SIG_IGN
  # survives exec, so without `--default-signal` every test inherits it
  # through setsid/timeout/kirk/sh. LTP's library resets SIGINT in the test
  # process and never SIGQUIT: kill11's SIGQUIT child could not die and the
  # test was "broken (2m 0s)" on every run since eea8892 (2026-10-18,
  # 2026-10-22) — while its ext4 control, run in the foreground, passed.
  (cd "$KIRK_DIR" && exec setsid env --default-signal=INT,QUIT LTPROOT="$LTP_PREFIX" TMPDIR="$tmp" LTP_TIMEOUT_MUL=${LTP_TIMEOUT_MUL:-4} timeout -s KILL $((LTP_TIMEOUT_S + 120)) \
     ./kirk --no-colors --tmp-dir "$ktmp" --run-suite "$file" ${LTP_PATTERN:+--run-pattern "$LTP_PATTERN"} --json-report "$report" --suite-timeout "$LTP_TIMEOUT_S" --exec-timeout "$LTP_EXEC_TIMEOUT_S" "${skip[@]}") > "$console" 2>&1 &
  KIRK_PGID=$!
  # FAIL EARLY (owner ruling 2026-10-17): the first failing tag that is
  # neither a ledger row nor the venue's (its control run on ext4 fails
  # too — judged right here, off the runtest file's command) ends the file.
  local first_ours="" seen="$RESULTS/ltp-$tag.seen" t cmd v ours dispositions_checked="" mask
  : > "$seen"
  suite_ledger_rows ltp | cut -f1 > "$RESULTS/ledger-ids.txt"
  while kill -0 "$KIRK_PGID" 2>/dev/null; do
    # The group leader is `timeout` once the exec chain has run: its
    # dispositions are what every test gets.
    if [ -z "$dispositions_checked" ] && [ "$(cat "/proc/$KIRK_PGID/comm" 2>/dev/null)" = timeout ]; then
      dispositions_checked=1
      if ! mask=$(ltp_check_dispositions "$KIRK_PGID"); then
        kill -KILL -- "-$KIRK_PGID" 2>/dev/null || true
        suite_die "kirk's tests would run with SIGINT/SIGQUIT ignored (SigIgn $mask): LTP's kill11 cannot pass that way"
      fi
    fi
    for t in $(grep -oE '^[A-Za-z0-9_.+-]+: (fail|broken|timeout)' "$console" 2>/dev/null | cut -d: -f1); do
      grep -qxF "$t" "$seen" && continue
      echo "$t" >> "$seen"
      grep -qxF "$file/$t" "$RESULTS/ledger-ids.txt" && continue
      cmd=$(awk -v t="$t" '$1 == t { sub(/^[^ \t]+[ \t]+/, ""); print; exit }' "$LTP_PREFIX/runtest/$file")
      # The test's own failing lines off kirk's debug log (its stdout is
      # logged at completion, before the console row); absent, the control
      # is judged by its exit code here and re-judged on the report after.
      ours="$RESULTS/ctl-$t-run$n.ours"
      ltp_fail_lines debug "$ktmp/kirk.root/latest/debug.log" "$t" > "$ours" 2>/dev/null || ours=""
      v=$(ltp_control_cmd "$t" "$cmd" "$n" "$ours")
      [ "$v" = venue ] && continue
      first_ours=$t
      break 2
    done
    sleep 2
  done
  if [ -n "$first_ours" ]; then
    suite_log "$file run $n: $first_ours failed, passes on ext4 and is no ledger row — stopping the file here (fail early)"
    # A stop must not wait on the running test (its exec-timeout is 900 s):
    # the group is killed and the rows below come off kirk's console (the
    # report is never written). What the killed test leaves on the mount
    # (a loop device, a stacked mount) is `suite_umount`'s to clear.
    kill -KILL -- "-$KIRK_PGID" 2>/dev/null || true
    SUITE_JUDGE_NO_STALE=1
  fi
  wait "$KIRK_PGID" || rc=$?
  KIRK_PGID=""
  t1=$(date +%s)
  if [ -n "$first_ours" ]; then
    grep -oE '^[A-Za-z0-9_.+-]+: (pass|fail|broken|timeout|conf|skip)' "$console" | awk -F': ' -v rt="$LTP_PREFIX/runtest/$file" '
      BEGIN { while ((getline l < rt) > 0) { split(l, a, /[ \t]+/); c = l; sub(/^[^ \t]+[ \t]+/, "", c); cmd[a[1]] = c } }
      { print $1 "\t" $2 "\t" cmd[$1] }' > "$rows"
    rc=1
  else
    [ -s "$report" ] || { tail -20 "$console" >&2; suite_log "$file run $n: kirk produced no report (rc $rc)"; suite_umount "$tag"; return 1; }
    ltp_report_rows "$report" > "$rows"
  fi
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
    v=$(ltp_control "$t" "$rows" "$n" "$report")
    if [ "$v" = venue ]; then echo "$t" >> "$RESULTS/ltp-$tag.venue"; else echo "$file/$t" >> "$failures"; fi
  done < "$all"
  local nv no; nv=$(grep -c . "$RESULTS/ltp-$tag.venue" || true); no=$(grep -c . "$failures" || true)
  suite_log "$file run $n: $no failure(s) OURS (pass on ext4), $nv the venue's (fail on ext4 too: ltp-$tag.venue)"
  if [ "$no" != 0 ]; then
    suite_log "our failures:"
    while read -r t; do
      t=${t#*/}
      awk -F'\t' -v t="$t" '$1 == t { print "    " $1 " [" $2 "]: " $3 }' "$rows" >&2
      [ -s "$report" ] || continue
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
  # kirk's debug log holds every test's stdout: on a run that did not pass
  # it is the only record of WHERE a test hung (a fail-early kill writes no
  # report; 2026-10-18 kill11 "broken 2m 0s" left nothing to read).
  [ "$rc" = 0 ] || cp "$ktmp/kirk.root/latest/debug.log" "$RESULTS/kirk-$tag.debug.log" 2>/dev/null || true
  rm -rf "$ktmp"
  suite_umount "$tag"
  suite_daemon_alerts "$tag" || return 1
  suite_fsck "$tag" || return 1
  [ "$rc" = 0 ] || { suite_log "kirk rc $rc: the run did not complete cleanly"; return 1; }
  return 0
}

# A stop of the harness takes kirk's process group with it (its tests hold
# files on the mount the cleanup unmounts next).
KIRK_PGID=""
ltp_cleanup() {
  if [ -n "$KIRK_PGID" ]; then kill -KILL -- "-$KIRK_PGID" 2>/dev/null || true; KIRK_PGID=""; fi
}

main() {
  SUITE_CLEANUP_HOOK=ltp_cleanup
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
