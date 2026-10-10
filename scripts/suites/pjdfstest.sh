#!/usr/bin/env bash
# pjdfstest against a real mount (charter §11 "Gates"): clone + build once,
# `prove -rv tests/` run FROM INSIDE the mount (the suite judges the cwd's
# filesystem), every `not ok` folded to an id `<dir>/<NN>.t#<subtest>`,
# the adjudication ledger (`docs/gates/pjdfstest-exceptions.md`) judging
# the set, `fsck --offline --scrub` of the set after the run, and the
# multi-run discipline (SUITE_RUNS, default 3: a FRESH set per run, the
# count aborts at the first non-pass).
#
#   scripts/venue scripts/suites/pjdfstest.sh            # the 3-run gate
#   SUITE_RUNS=1 scripts/venue scripts/suites/pjdfstest.sh
#   PJD_TESTS="chmod utimensat" …                        # a subset (triage)
#   PJD_SMOKE=1 …                                        # the 2-minute gate subset
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

PJD_REPO=${PJD_REPO:-https://github.com/pjd/pjdfstest}
PJD_DIR="$SUITES_ROOT/pjdfstest"
# The gate-time subset (`zig build suites-smoke`): the handler families
# that carry semantics of our own (modes, ownership, the rename matrix,
# truncate, the unlink/open-count law, times) — 141 of the 238 files at the
# measured 0.77 s/file ≈ 110 s + format/mount/fsck ≈ 2 minutes. link,
# mkfifo, rmdir and symlink ride the full suite (the release gate).
PJD_SMOKE_TESTS="chmod chown mkdir mknod open rename truncate unlink utimensat"

pjd_build() {
  if [ ! -d "$PJD_DIR/.git" ]; then
    suite_log "cloning $PJD_REPO"
    git clone -q --depth 1 "$PJD_REPO" "$PJD_DIR"
  fi
  if [ ! -x "$PJD_DIR/pjdfstest" ]; then
    suite_log "building pjdfstest ($(cd "$PJD_DIR" && git log -1 --format=%h))"
    (cd "$PJD_DIR" && autoreconf -ifs >"$RESULTS/pjd-build.log" 2>&1 && ./configure >>"$RESULTS/pjd-build.log" 2>&1 && make -s >>"$RESULTS/pjd-build.log" 2>&1) \
      || { tail -30 "$RESULTS/pjd-build.log" >&2; suite_die "pjdfstest build failed"; }
  fi
  echo "pjdfstest $(cd "$PJD_DIR" && git log -1 --format='%H %cd')" > "$RESULTS/pjd-version.txt"
}

# One run: a fresh set, prove inside the mount, ids of every failing
# subtest to $2, the raw TAP to $RESULTS/run-$1.tap. Returns prove's rc.
pjd_run() {
  local n=$1 failures=$2
  local tag="run$n"
  : > "$failures"
  suite_mount "$tag"
  local mnt=${SUITE_MNT[$tag]}
  local targets=()
  if [ -n "${PJD_TESTS:-}" ]; then for t in $PJD_TESTS; do targets+=("$PJD_DIR/tests/$t"); done
  elif [ "${PJD_SMOKE:-0}" = 1 ]; then for t in $PJD_SMOKE_TESTS; do targets+=("$PJD_DIR/tests/$t"); done
  else targets=("$PJD_DIR/tests"); fi
  local rc=0 t0 t1
  t0=$(date +%s)
  # prove's -v prints each test file's TAP under a `tests/... ..` banner;
  # the suite's cwd must be ON the mount. `--merge` keeps stderr in order.
  (cd "$mnt" && prove -rv --merge "${targets[@]}") > "$RESULTS/run-$n.tap" 2>&1 || rc=$?
  t1=$(date +%s)
  # Fold `not ok N …` under the current file banner to `dir/NN.t#N`. A
  # `# TODO` not-ok is the SUITE's own declared expectation (TAP: counted
  # as a pass by prove — pjdfstest marks the rows where Linux VFS semantics
  # differ from the POSIX text, e.g. chown on a directory keeping its
  # sgid), listed separately, never a failure of ours.
  local fold='/^\/.*\.t \.\./ { f = $1; sub(root, "", f); next } /^not ok [0-9]+/ { if (($0 ~ /# TODO/) == todo) print f "#" $3 }'
  awk -v root="$PJD_DIR/tests/" -v todo=0 "$fold" "$RESULTS/run-$n.tap" | sort -u > "$failures"
  awk -v root="$PJD_DIR/tests/" -v todo=1 "$fold" "$RESULTS/run-$n.tap" | sort -u > "$RESULTS/run-$n.todo"
  local nf nt; nf=$(grep -c . "$failures" || true); nt=$(grep -c . "$RESULTS/run-$n.todo" || true)
  suite_log "run $n: prove rc=$rc, $nf failing subtest(s), $nt suite-declared TODO(s), $((t1 - t0)) s wall — $(grep -E '^(Files=|Result:)' "$RESULTS/run-$n.tap" | tr '\n' ' ')"
  [ "$nf" = 0 ] || { suite_log "failing subtests:"; while read -r id; do grep -h "^not ok ${id##*#} " "$RESULTS/run-$n.tap" | head -1 | cut -c1-200 | sed "s|^|    $id: |" >&2; done < <(head -60 "$failures"); }
  suite_umount "$tag"
  suite_daemon_alerts "$tag" || return 1
  suite_fsck "$tag" || return 1
  return 0
}

main() {
  suite_prepare pjdfstest
  pjd_build
  local n verdict=0
  for n in $(seq 1 "$SUITE_RUNS"); do
    suite_run_count "$n"
    local failures="$RESULTS/run-$n.failures"
    local run_rc=0
    pjd_run "$n" "$failures" || run_rc=$?
    local jrc=0
    suite_judge pjdfstest "$failures" || jrc=$?
    if [ "$run_rc" != 0 ] || [ "$jrc" != 0 ]; then
      suite_log "run $n did NOT pass (run rc $run_rc, ledger rc $jrc): the count aborts; fix, then restart from run 1"
      verdict=1; break
    fi
    suite_log "run $n PASSED (every failure adjudicated, fsck clean, no tripwire)"
  done
  [ "$verdict" = 0 ] && suite_log "pjdfstest: $SUITE_RUNS consecutive clean run(s) on $("$SQZ_BIN" --version)"
  if [ "${SUITE_CLEAN_BUILD:-0}" = 1 ]; then suite_clean_build "$PJD_DIR"; fi
  exit $verdict
}

main "$@"
