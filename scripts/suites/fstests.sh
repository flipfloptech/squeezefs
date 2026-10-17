#!/usr/bin/env bash
# xfstests (fstests) `check -g auto` against real mounts (charter §11
# "Gates": the release gate runs the full fstests `-g auto` with
# adjudicated by-design exceptions only; `_notrun` for xfs/ext4-specific
# groups is expected — the generic tests are the signal).
#
# Shape: FSTYP=fuse.squeezefs — xfstests' GENERIC arms (an fstype it has no
# special case for): TEST_DEV and SCRATCH_DEV are LOOP DEVICES over two
# sets' metadata images (the suite's `_require_scratch` wants a block
# device), `_scratch_mkfs` runs `mkfs -t fuse.squeezefs` = our
# `mkfs.fuse.squeezefs` (a fresh set per scratch test), mounts go through
# `mount.fuse.squeezefs` (the release posture + the caller's options,
# `fsname=$dev` so findmnt finds the mount by its device), unmounts through
# `umount.fuse.squeezefs` (the orderly `squeezefs umount`), and — the whole
# point of this shape — `_check_test_fs`/`_check_scratch_fs` run `fsck -t
# fuse.squeezefs` = our `fsck.fuse.squeezefs` (`fsck --offline --scrub`)
# over the UNMOUNTED set after EVERY test: C1–C17 + the scrub are the
# per-test oracle; a finding fails that test. (The `fuse` FSTYP class
# would have skipped mkfs and fsck and judged the mount type `fuse`,
# which a subtype-bearing daemon never is.) Failures → the ledger
# (`docs/gates/fstests-exceptions.md`).
#
#   scripts/venue scripts/suites/fstests.sh                     # -g auto, SUITE_RUNS
#   SUITE_RUNS=1 FSTESTS_GROUPS="-g quick" scripts/venue scripts/suites/fstests.sh
#   FSTESTS_TESTS="generic/001 generic/003" …                   # a subset (triage)
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

FSTESTS_REPO=${FSTESTS_REPO:-https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git}
FSTESTS_DIR="$SUITES_ROOT/xfstests-dev"
FSTESTS_GROUPS=${FSTESTS_GROUPS:-"-g auto"}
# Wall bound for one `check` pass: measured ~2 h for -g auto on the venue.
FSTESTS_TIMEOUT_S=${FSTESTS_TIMEOUT_S:-14400}
# Tests excluded in THIS venue only (`check -X`), each with its reason;
# never a filesystem semantic (those are the ledger's).
FSTESTS_VENUE_EXCLUDE="$SUITES_DIR/fstests-venue-exclude.txt"

# xfstests master defines `struct delegation` (the file-delegation tests'
# fcntl argument) only `#ifndef F_GETDELEG`; glibc 2.43+ defines the
# constants without the struct, so on a rolling venue locktest.c does not
# compile ("storage size of 'deleg' isn't known"). The maintainer's fix
# (https://lore.kernel.org/fstests/20260813150846.280498-1-zlang@kernel.org/)
# takes the struct out of that guard; until it lands, the same edit here,
# only where the guard still wraps the struct and glibc lacks it.
fstests_patch_locktest() {
  local f="$FSTESTS_DIR/src/locktest.c"
  # A libc that completes the struct: nothing to do.
  printf '#include <fcntl.h>\nstruct delegation d;\n' | cc -x c -fsyntax-only - 2>/dev/null && return 0
  # The struct still inside the guard (the clone predates the fix)?
  grep -Pzoq 'struct delegation \{[^}]*\};\n#endif\n' "$f" || return 0
  # Close the guard after the constants; drop the `#endif` after the struct.
  sed -i -e '/^#define F_SETDELEG[[:space:]]*(1024 + 16)$/a #endif' -e '/^struct delegation {/,/^#endif$/{/^#endif$/d}' "$f"
  suite_log "locktest.c: struct delegation defined outside the F_GETDELEG guard (this glibc defines the constants, not the struct; upstream fix pending)"
}

fstests_build() {
  if [ ! -d "$FSTESTS_DIR/.git" ]; then
    suite_log "cloning $FSTESTS_REPO"
    git clone -q --depth 1 "$FSTESTS_REPO" "$FSTESTS_DIR"
  fi
  if [ ! -x "$FSTESTS_DIR/ltp/fsstress" ] || [ ! -x "$FSTESTS_DIR/ltp/fsx" ]; then
    suite_log "building xfstests ($(cd "$FSTESTS_DIR" && git log -1 --format=%h))"
    fstests_patch_locktest
    (cd "$FSTESTS_DIR" && make -s -j"$(nproc)" >"$RESULTS/fstests-build.log" 2>&1) || { tail -30 "$RESULTS/fstests-build.log" >&2; suite_die "xfstests build failed"; }
  fi
  echo "xfstests $(cd "$FSTESTS_DIR" && git log -1 --format='%H %cd')" > "$RESULTS/fstests-version.txt"
  # The unprivileged users xfstests' `_require_user` wants.
  local u; for u in fsgqa 123456-fsgqa fsgqa2; do id "$u" >/dev/null 2>&1 || useradd -m "$u"; done
  # The mount/umount/mkfs/fsck helpers util-linux dispatches to.
  cat > /run/squeezefs-suites.env <<ENV
SQZ_BIN=$SQZ_BIN
SUITE_MOUNT_OPTS=$SUITE_MOUNT_OPTS
SUITE_META_MIB=$SUITE_META_MIB
SUITE_DATA_GIB=$SUITE_DATA_GIB
ENV
  ln -sf "$SUITES_DIR/mount.squeezefs" /sbin/mount.fuse.squeezefs
  ln -sf "$SUITES_DIR/mount.squeezefs" /sbin/umount.fuse.squeezefs
  ln -sf "$SUITES_DIR/mkfs.squeezefs" /sbin/mkfs.fuse.squeezefs
  ln -sf "$SUITES_DIR/fsck.squeezefs" /sbin/fsck.fuse.squeezefs
}

# The two sets: TEST (long-lived across the run) and SCRATCH (every test
# re-mkfses it). Both formatted fresh per run; each metadata image behind
# a loop device (detached by `fstests_detach_loops`).
fstests_sets() {
  local n=$1
  SETS="$RESULTS/run$n"
  mkdir -p "$SETS/set-test" "$SETS/set-scratch" "$SETS/mnt-test" "$SETS/mnt-scratch" "$SETS/results"
  truncate -s "${SUITE_META_MIB}M" "$SETS/set-test/meta.img" "$SETS/set-scratch/meta.img"
  TEST_LOOP=$(losetup -f --show "$SETS/set-test/meta.img") || suite_die "losetup (test)"
  SCRATCH_LOOP=$(losetup -f --show "$SETS/set-scratch/meta.img") || suite_die "losetup (scratch)"
  "$SUITES_DIR/mkfs.squeezefs" -f "$TEST_LOOP" > "$SETS/set-test/format.out" 2>&1 || { cat "$SETS/set-test/format.out" >&2; suite_die "mkfs test set"; }
  "$SUITES_DIR/mkfs.squeezefs" -f "$SCRATCH_LOOP" > "$SETS/set-scratch/format.out" 2>&1 || { cat "$SETS/set-scratch/format.out" >&2; suite_die "mkfs scratch set"; }
  cat > "$SETS/local.config" <<CFG
export FSTYP=fuse.squeezefs
# `_scratch_mkfs` reformats a set the previous test formatted: mkfs(8)'s -f.
export MKFS_OPTIONS="-f"
export TEST_DEV=$TEST_LOOP
export TEST_DIR=$SETS/mnt-test
export SCRATCH_DEV=$SCRATCH_LOOP
export SCRATCH_MNT=$SETS/mnt-scratch
export RESULT_BASE=$SETS/results
export LOGWRITES_DEV=
CFG
  suite_log "run $n sets formatted: TEST $TEST_LOOP ($SETS/set-test), SCRATCH $SCRATCH_LOOP ($SETS/set-scratch)"
}

# Every daemon the helpers spawned for this run's sets: pids from the
# mounts' .stats are gone with the mounts, so the set ledgers' log paths
# are the record; liveness by /proc/mounts + the squeezefs daemons whose
# cmdline names our sets.
fstests_daemons() {
  local p
  for p in $(pgrep -x squeezefs); do
    suite_alive "$p" || continue
    if tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null | grep -q -- "$SETS\|${TEST_LOOP:-/nonesuch}\|${SCRATCH_LOOP:-/nonesuch}"; then echo "$p"; fi
  done
  return 0
}

fstests_unmount_all() {
  local m
  for m in "$SETS/mnt-test" "$SETS/mnt-scratch"; do
    grep -q " $m " /proc/mounts && { "$SQZ_BIN" umount "$m" >>"$RESULTS/harness.log" 2>&1 || umount -l "$m" || true; }
  done
  local i p
  for i in $(seq 1 300); do [ -z "$(fstests_daemons)" ] && break; sleep 0.1; done
  for p in $(fstests_daemons); do suite_alive "$p" && { suite_log "daemon $p (ours) still alive; SIGKILL"; kill -9 "$p" || true; }; done
}

fstests_detach_loops() {
  [ -n "${TEST_LOOP:-}" ] && losetup -d "$TEST_LOOP" 2>/dev/null; TEST_LOOP=""
  [ -n "${SCRATCH_LOOP:-}" ] && losetup -d "$SCRATCH_LOOP" 2>/dev/null; SCRATCH_LOOP=""
  return 0
}

fstests_fsck_set() {
  local which=$1 n=$2 rc=0
  local out="$RESULTS/fsck-$which-run$n.json"
  local dev; dev=$([ "$which" = test ] && echo "$TEST_LOOP" || echo "$SCRATCH_LOOP")
  # The SCRATCH device is any test's to leave as it likes (`_require_scratch_nocheck`:
  # generic/740 ends with a foreign mkfs on it); only a squeezefs volume is scrubbed.
  if [ "$which" = scratch ]; then
    local st; st=$("$SQZ_BIN" status "sqmeta://$dev" 2>/dev/null | grep -v '^info\|^debug' | head -1)
    case "$st" in *"not formatted"*) suite_log "fsck $which (run $n): skipped — the last test left the scratch device $(echo "$st" | sed 's/.*not formatted/not formatted/' | cut -c1-120)"; return 0 ;; esac
  fi
  "$SQZ_BIN" fsck --offline --scrub --json "sqmeta://$dev" > "$out" 2>"$out.err" || rc=$?
  if [ "$rc" = 0 ]; then suite_log "fsck --offline --scrub $which (run $n): 0 findings"; return 0; fi
  suite_log "fsck --offline --scrub $which (run $n): exit $rc — $out"; head -c 1500 "$out" >&2; echo >&2; grep -v '^info\|^debug' "$out.err" | head -10 >&2
  return 1
}

# One `check` pass. Failure ids (`generic/NNN`) to $2.
fstests_run() {
  local n=$1 failures=$2
  : > "$failures"
  fstests_sets "$n"
  local exclude=()
  if [ -s "$FSTESTS_VENUE_EXCLUDE" ]; then grep -v '^#' "$FSTESTS_VENUE_EXCLUDE" | awk '{print $1}' | grep . > "$RESULTS/venue-exclude.list" || true; [ -s "$RESULTS/venue-exclude.list" ] && exclude=(-E "$RESULTS/venue-exclude.list"); fi
  local what=()
  if [ -n "${FSTESTS_TESTS:-}" ]; then read -ra what <<< "$FSTESTS_TESTS"; else read -ra what <<< "$FSTESTS_GROUPS"; fi
  local rc=0 t0 t1
  t0=$(date +%s)
  (cd "$FSTESTS_DIR" && cp "$SETS/local.config" local.config && timeout -s KILL "$FSTESTS_TIMEOUT_S" ./check -R xunit "${exclude[@]}" "${what[@]}" > "$RESULTS/check-run$n.out" 2>&1) || rc=$?
  t1=$(date +%s)
  # The summary block: `Ran: …`, `Not run: …`, `Failures: …`, `Failed N of M tests`.
  local ran notrun failed
  ran=$(grep -E '^Ran: ' "$RESULTS/check-run$n.out" | tail -1 | sed 's/^Ran: //' | wc -w)
  notrun=$(grep -E '^Not run: ' "$RESULTS/check-run$n.out" | tail -1 | sed 's/^Not run: //' | wc -w)
  grep -E '^Failures: ' "$RESULTS/check-run$n.out" | tail -1 | sed 's/^Failures: //' | tr ' ' '\n' | grep . | sort -u > "$failures" || true
  failed=$(grep -c . "$failures" || true)
  suite_log "run $n: check rc=$rc, ran $ran, not run $notrun, failed $failed, $((t1 - t0)) s wall ($(grep -E '^(Passed all|Failed [0-9]+ of)' "$RESULTS/check-run$n.out" | tail -1))"
  [ "$ran" -gt 0 ] || { tail -20 "$RESULTS/check-run$n.out" >&2; suite_log "run $n: check ran nothing"; fstests_unmount_all; fstests_detach_loops; return 1; }
  # Per-failure: the first lines of the .out.bad diff and the dmesg flag.
  if [ "$failed" != 0 ]; then
    suite_log "failures (first diff lines; full results under $SETS/results):"
    local t
    while read -r t; do
      echo "    $t: $(grep -E "^$t " "$RESULTS/check-run$n.out" | head -1 | cut -c1-160)" >&2
      [ -f "$SETS/results/$t.out.bad" ] && diff "$FSTESTS_DIR/tests/$t.out" "$SETS/results/$t.out.bad" 2>/dev/null | grep '^[<>]' | head -4 | cut -c1-180 | sed 's/^/        /' >&2
    done < "$failures"
  fi
  # The not-run reasons, histogrammed (the evidence note's table).
  (cd "$SETS/results" && for f in */*.notrun; do [ -f "$f" ] && cat "$f"; echo; done 2>/dev/null | grep . | sort | uniq -c | sort -rn | head -40) > "$RESULTS/notrun-reasons-run$n.txt" || true
  fstests_unmount_all
  local ok=0
  for m in "$SETS/set-test" "$SETS/set-scratch"; do
    local logs; logs=$(ls "$m"/daemon-*.log 2>/dev/null | wc -l)
    cat "$m"/daemon-*.log 2>/dev/null | grep -E '^(error|warn)\(|TRIPWIRE' > "$RESULTS/daemon-$(basename "$m")-run$n.alerts" || true
    local na; na=$(wc -l < "$RESULTS/daemon-$(basename "$m")-run$n.alerts")
    suite_log "$(basename "$m"): $logs daemon log(s), $na alert line(s)"
    grep -q TRIPWIRE "$RESULTS/daemon-$(basename "$m")-run$n.alerts" && { suite_log "TRIPWIRE fired in $(basename "$m")"; ok=1; }
    # A daemon that died mid-test (the mount helper cleared its stale
    # claim so the pass could go on): the run is VOID whatever `check` says.
    if [ -s "$m/daemon-died" ]; then
      suite_log "$(basename "$m"): a daemon DIED during the run ($(grep -c . "$m/daemon-died") time(s)) — the run is void:"
      sed 's/^/    /' "$m/daemon-died" >&2
      cp "$m/daemon-died" "$RESULTS/daemon-died-$(basename "$m")-run$n"
      ok=1
    fi
  done
  fstests_fsck_set test "$n" || ok=1
  fstests_fsck_set scratch "$n" || ok=1
  fstests_detach_loops
  # The data volumes are the bulk; the logs and results stay.
  [ "${SUITE_KEEP_SET:-0}" = 1 ] || rm -f "$SETS"/set-*/d?.img "$SETS"/set-*/meta.img
  [ "$rc" = 0 ] || [ "$rc" = 1 ] || { suite_log "check rc $rc (137 = the wall bound): the pass did not complete"; return 1; }
  return $ok
}

main() {
  suite_prepare fstests
  fstests_build
  local verdict=0 n
  for n in $(seq 1 "$SUITE_RUNS"); do
    suite_run_count "$n"
    local failures="$RESULTS/run$n.failures"
    local run_rc=0 jrc=0
    fstests_run "$n" "$failures" || run_rc=$?
    suite_judge fstests "$failures" || jrc=$?
    if [ "$run_rc" != 0 ] || [ "$jrc" != 0 ]; then
      suite_log "run $n did NOT pass (run rc $run_rc, ledger rc $jrc): the count aborts"
      verdict=1; break
    fi
    suite_log "run $n PASSED"
  done
  [ "$verdict" = 0 ] && suite_log "fstests ($FSTESTS_GROUPS): $SUITE_RUNS consecutive clean run(s) on $("$SQZ_BIN" --version)"
  exit $verdict
}

main "$@"
