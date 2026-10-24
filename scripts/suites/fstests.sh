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
#   FSTESTS_AFTER=generic/683 …                                   # resume discovery after a failure (never the release verdict)
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
source "$SUITES_DIR/setpaths.sh"

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
  # `src/locktest` is among the last targets: a build that died midway
  # (the glibc guard below) left fsstress and fsx behind and must re-run.
  if [ ! -x "$FSTESTS_DIR/ltp/fsstress" ] || [ ! -x "$FSTESTS_DIR/ltp/fsx" ] || [ ! -x "$FSTESTS_DIR/src/locktest" ]; then
    suite_log "building xfstests ($(cd "$FSTESTS_DIR" && git log -1 --format=%h))"
    fstests_patch_locktest
    (cd "$FSTESTS_DIR" && make -s -j"$(nproc)" >"$RESULTS/fstests-build.log" 2>&1) || { tail -30 "$RESULTS/fstests-build.log" >&2; suite_die "xfstests build failed"; }
  fi
  echo "xfstests $(cd "$FSTESTS_DIR" && git log -1 --format='%H %cd')" > "$RESULTS/fstests-version.txt"
  # The unprivileged users xfstests' `_require_user` wants.
  local u; for u in fsgqa 123456-fsgqa fsgqa2; do id "$u" >/dev/null 2>&1 || useradd -m "$u"; done
  # ONE fstests harness per venue: the helper symlinks below and the
  # venue-wide env file are re-pointed here, under any `check` already
  # running (2026-10-22: a second harness's mkfs re-pointed the first's
  # scratch slices during its post-test fsck). Wait for it, bounded.
  local other waited=0
  while other=$(pgrep -f 'check -R xunit' | grep -vx "$$" | head -1) && [ -n "$other" ]; do
    [ "$waited" = 0 ] && suite_log "another fstests check (pid $other) is running on this venue; waiting for it (the helpers are venue-wide)"
    [ "$waited" -lt "${FSTESTS_VENUE_WAIT_S:-900}" ] || suite_die "another fstests check (pid $other) still runs after ${waited}s: one fstests harness per venue"
    sleep 5; waited=$((waited + 5))
  done
  # The mount/umount/mkfs/fsck helpers util-linux dispatches to.
  ln -sf "$SUITES_DIR/mount.squeezefs" /sbin/mount.fuse.squeezefs
  # umount(8) names its helper by the type's PREFIX: `umount.fuse -t fuse.squeezefs`.
  ln -sf "$SUITES_DIR/mount.squeezefs" /sbin/umount.fuse
  rm -f /sbin/umount.fuse.squeezefs
  ln -sf "$SUITES_DIR/mkfs.squeezefs" /sbin/mkfs.fuse.squeezefs
  ln -sf "$SUITES_DIR/fsck.squeezefs" /sbin/fsck.fuse.squeezefs
}

# The two sets: TEST (long-lived across the run) and SCRATCH (every test
# re-mkfses it). Both formatted fresh per run; each set ONE sparse image
# behind a loop device (detached by `fstests_detach_loops`) that the helpers
# carve into the metadata volume and the two data volumes (`setpaths.sh`):
# every byte of a set's state under the device the test cuts, errors or
# logs. A test that hands the helpers a device of its own (dmhugedisk's
# 17 TB snapshot, a thin volume) gets a set keyed by the device's name,
# its logs under `$SETS/sets/<name>` — the helpers learn that directory
# from their env file, written here per run.
fstests_sets() {
  local n=$1
  SETS="$RESULTS/run$n"
  mkdir -p "$SETS/set-test" "$SETS/set-scratch" "$SETS/mnt-test" "$SETS/mnt-scratch" "$SETS/results" "$SETS/sets" "$SETS/bin"
  # check's mount(8): a bare mount of an untypable block device is ours
  # (`mount.typed`; generic/620 mounts its hugedisk without `-t`).
  ln -sf "$SUITES_DIR/mount.typed" "$SETS/bin/mount"
  # The run's env beside its sets (the helpers find it from a set's path:
  # `suite_run_env`) and the venue-wide copy for a set with no path to a run.
  cat > "$SETS/suites.env" <<ENV
SQZ_BIN=$SQZ_BIN
SUITE_MOUNT_OPTS=$SUITE_MOUNT_OPTS
SUITE_META_MIB=$SUITE_META_MIB
SUITE_DATA_GIB=$SUITE_DATA_GIB
SUITE_META_MIN_MIB=$SUITE_META_MIN_MIB
SUITE_DATA_MIN_MIB=$SUITE_DATA_MIN_MIB
SUITE_SINGLE_WRITER=$SUITE_SINGLE_WRITER
SUITE_SETS_DIR=$SETS/sets
ENV
  cp "$SETS/suites.env" /run/squeezefs-suites.env
  truncate -s "$(suite_set_bytes)" "$SETS/set-test/meta.img" "$SETS/set-scratch/meta.img"
  TEST_LOOP=$(losetup -f --show "$SETS/set-test/meta.img") || suite_die "losetup (test)"
  SCRATCH_LOOP=$(losetup -f --show "$SETS/set-scratch/meta.img") || suite_die "losetup (scratch)"
  "$SUITES_DIR/mkfs.squeezefs" -f "$TEST_LOOP" > "$SETS/set-test/format.out" 2>&1 || { cat "$SETS/set-test/format.out" >&2; suite_die "mkfs test set"; }
  "$SUITES_DIR/mkfs.squeezefs" -f "$SCRATCH_LOOP" > "$SETS/set-scratch/format.out" 2>&1 || { cat "$SETS/set-scratch/format.out" >&2; suite_die "mkfs scratch set"; }
  # MIN_FSSIZE: xfstests' own floor hook (xfs sets 600 MB, btrfs 512 MB) — a
  # test's small device request (`_small_fs_size_mb`) is rounded up to the
  # smallest set the product formats (setpaths.sh's floors); nothing is
  # skipped by it: our FSTYP has no `_scratch_mkfs_sized` arm, the one path
  # that would.
  cat > "$SETS/local.config" <<CFG
export FSTYP=fuse.squeezefs
# _scratch_mkfs reformats a set the previous test formatted: mkfs(8)'s -f.
export MKFS_OPTIONS="-f"
export TEST_DEV=$TEST_LOOP
export TEST_DIR=$SETS/mnt-test
export SCRATCH_DEV=$SCRATCH_LOOP
export SCRATCH_MNT=$SETS/mnt-scratch
export RESULT_BASE=$SETS/results
export LOGWRITES_DEV=
export MIN_FSSIZE=$(suite_set_min_bytes)
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
  # A set's slices a test left standing go first: the loop is busy under
  # them. Every set of this run — the two loop sets and any raw-device set
  # a test brought (its `.dir` names a directory under this run's).
  local id f
  for f in /run/squeezefs-suites/*.dir; do
    [ -f "$f" ] || continue
    id=$(basename "$f" .dir)
    case "$(cat "$f")" in "$SETS"/*) ;; *) continue ;; esac
    SET_ID=$id; suite_slice_paths; suite_slices_down
    rm -f "$f"
  done
  rm -f /run/squeezefs-suites/mnt-* 2>/dev/null
  [ -n "${TEST_LOOP:-}" ] && losetup -d "$TEST_LOOP" 2>/dev/null; TEST_LOOP=""
  [ -n "${SCRATCH_LOOP:-}" ] && losetup -d "$SCRATCH_LOOP" 2>/dev/null; SCRATCH_LOOP=""
  return 0
}

fstests_fsck_set() {
  local which=$1 n=$2 rc=0
  local out="$RESULTS/fsck-$which-run$n.json"
  local dev; dev=$([ "$which" = test ] && echo "$TEST_LOOP" || echo "$SCRATCH_LOOP")
  "$SUITES_DIR/fsck.squeezefs" "$dev" > "$out" 2>"$out.err" || rc=$?
  if [ "$rc" = 0 ]; then suite_log "fsck --offline --scrub $which (run $n): 0 findings"; return 0; fi
  # The SCRATCH device at the run's end is the LAST test's to have left as
  # it liked: a `_require_scratch_nocheck` test leaves a foreign mkfs
  # (generic/740), a dm-snapshot's COW chunks (generic/620), another set's
  # volumes over its first 64 MiB (generic/563) — and every test that checks
  # its scratch had it fscked by `_check_scratch_fs` before the next test
  # touched it. So this fsck is the record, never the verdict: a scratch
  # finding here was either that test's (already red) or no test's.
  if [ "$which" = scratch ]; then
    suite_log "fsck $which (run $n): exit $rc, for the record (the last test's leftover; the per-test fsck is the oracle): $(grep -v '^info\|^debug' "$out" | grep -E 'findings|cannot open|discovery|refuse' | head -1 | cut -c1-200)"
    return 0
  fi
  suite_log "fsck --offline --scrub $which (run $n): exit $rc — $out"; head -c 1500 "$out" >&2; echo >&2; grep -v '^info\|^debug' "$out.err" | head -10 >&2
  return 1
}

# The host sysctls tests set for their own run and restore in their
# `_cleanup` — generic/460 (`vm.dirty_*ratio`), 597 and 598
# (`fs.protected_*`), 750 and 795 (one-shot writes). Two ways that restore
# is lost: a fail-early KILL of `check` ends a test before its cleanup,
# and xfstests restores a saved value only when it is non-zero — on a
# host tuned through `vm.dirty_bytes` (this workstation: 256 MiB / 64 MiB)
# the ratios read 0, so 460's `echo 100` cleared the bytes and its cleanup
# put nothing back (2026-10-22: the host ran at dirty_ratio 100 until a
# sysctl reload). The harness snapshots them before `check` and writes
# them back after: bytes first where the host uses bytes (writing a bytes
# knob zeroes its ratio twin), ratios otherwise.
FSTESTS_SYSCTLS=(vm.dirty_bytes vm.dirty_background_bytes vm.dirty_ratio vm.dirty_background_ratio fs.protected_symlinks fs.protected_hardlinks fs.protected_regular fs.protected_fifos)
declare -A FSTESTS_SYSCTL_SAVED
fstests_sysctl_save() {
  local k
  for k in "${FSTESTS_SYSCTLS[@]}"; do FSTESTS_SYSCTL_SAVED[$k]=$(sysctl -n "$k" 2>/dev/null || echo ""); done
}
fstests_sysctl_restore() {
  [ -n "${FSTESTS_SYSCTL_SAVED[vm.dirty_bytes]:-}" ] || return 0
  local k v changed=()
  for k in "${FSTESTS_SYSCTLS[@]}"; do [ "$(sysctl -n "$k" 2>/dev/null)" = "${FSTESTS_SYSCTL_SAVED[$k]}" ] || changed+=("$k"); done
  [ ${#changed[@]} -gt 0 ] || return 0
  for k in fs.protected_symlinks fs.protected_hardlinks fs.protected_regular fs.protected_fifos; do
    v=${FSTESTS_SYSCTL_SAVED[$k]}; [ -n "$v" ] && sysctl -qw "$k=$v" 2>/dev/null || true
  done
  if [ "${FSTESTS_SYSCTL_SAVED[vm.dirty_bytes]}" != 0 ]; then sysctl -qw "vm.dirty_bytes=${FSTESTS_SYSCTL_SAVED[vm.dirty_bytes]}" 2>/dev/null || true
  else sysctl -qw "vm.dirty_ratio=${FSTESTS_SYSCTL_SAVED[vm.dirty_ratio]}" 2>/dev/null || true; fi
  if [ "${FSTESTS_SYSCTL_SAVED[vm.dirty_background_bytes]}" != 0 ]; then sysctl -qw "vm.dirty_background_bytes=${FSTESTS_SYSCTL_SAVED[vm.dirty_background_bytes]}" 2>/dev/null || true
  else sysctl -qw "vm.dirty_background_ratio=${FSTESTS_SYSCTL_SAVED[vm.dirty_background_ratio]}" 2>/dev/null || true; fi
  suite_log "host sysctls a test left changed were restored: ${changed[*]}"
  FSTESTS_SYSCTL_SAVED=()
}

# One `check` pass. Failure ids (`generic/NNN`) to $2.
fstests_run() {
  local n=$1 failures=$2
  : > "$failures"
  fstests_sets "$n"
  fstests_sysctl_save
  local exclude=()
  if [ -s "$FSTESTS_VENUE_EXCLUDE" ]; then grep -v '^#' "$FSTESTS_VENUE_EXCLUDE" | awk '{print $1}' | grep . > "$RESULTS/venue-exclude.list" || true; [ -s "$RESULTS/venue-exclude.list" ] && exclude=(-E "$RESULTS/venue-exclude.list"); fi
  local what=()
  if [ -n "${FSTESTS_TESTS:-}" ]; then read -ra what <<< "$FSTESTS_TESTS"; else read -ra what <<< "$FSTESTS_GROUPS"; fi
  local rc=0 t0 t1
  t0=$(date +%s)
  # `check` leads a process group of its own and the harness remembers it:
  # a stopped harness (its EXIT trap) takes the whole group down — a `check`
  # that outlived one stop kept running its list against the loop devices
  # the next run re-created, and the two runs fought over one set.
  cp "$SETS/local.config" "$FSTESTS_DIR/local.config"
  # FSTESTS_AFTER=generic/NNN: resume DISCOVERY after a failure while its fix
  # is made — the pass runs the tests the list would run after NNN, in
  # check's order. A discovery pass is never the release verdict: that is one
  # clean run from the top on the final head.
  if [ -n "${FSTESTS_AFTER:-}" ]; then
    # The list from the generated group files (`check -n` is no dry run: it
    # formats SCRATCH). Groups are FSTESTS_GROUPS' `-g` words; this venue runs
    # tests/generic only (fuse.squeezefs has no fs-specific directory).
    local groups listed=()
    groups=$(sed -E 's/-g[[:space:]]+//g' <<< "$FSTESTS_GROUPS")
    mapfile -t listed < <(grep -v '^#' "$FSTESTS_DIR/tests/generic/group.list" | awk -v want="$groups" 'BEGIN {n = split(want, w, /[ ,]+/)} {for (i = 2; i <= NF; i++) for (j = 1; j <= n; j++) if ($i == w[j]) {print "generic/" $1; next}}' | sort -u | awk -v after="$FSTESTS_AFTER" 'seen {print} $1 == after {seen = 1}')
    [ "${#listed[@]}" -gt 0 ] || suite_die "FSTESTS_AFTER=$FSTESTS_AFTER: no test of the groups ($groups) follows it"
    suite_log "run $n: resuming discovery after $FSTESTS_AFTER — ${#listed[@]} test(s) left in the list"
    what=("${listed[@]}")
  fi
  # util-linux 2.39+ remounts through the new mount API, and FUSE refuses
  # every option on a reconfigure there ("No changes allowed in
  # reconfigure": generic/294, 306, 452's `-o remount,ro`); the classic
  # mount(2) applies the flag and ignores the options, as the daemon's
  # contract says. Helpers run the same under either API.
  (cd "$FSTESTS_DIR" && exec setsid env LIBMOUNT_FORCE_MOUNT2=always PATH="$SETS/bin:$PATH" timeout -s KILL "$FSTESTS_TIMEOUT_S" ./check -R xunit "${exclude[@]}" "${what[@]}") > "$RESULTS/check-run$n.out" 2>&1 &
  CHECK_PGID=$!
  # FAIL EARLY (owner ruling 2026-10-17): the first failing test that is not
  # a ledger row ends the pass — that failure is the work; the hours of
  # tests behind it add nothing to it. A by-design row fails on, as it must.
  local first_unadjudicated=""
  suite_ledger_rows fstests | cut -f1 > "$RESULTS/ledger-ids.txt"
  while kill -0 "$CHECK_PGID" 2>/dev/null; do
    local bad
    for bad in "$SETS"/results/*/*.out.bad; do
      [ -f "$bad" ] || continue
      local id; id=$(basename "$(dirname "$bad")")/$(basename "$bad" .out.bad)
      grep -qxF "$id" "$RESULTS/ledger-ids.txt" && continue
      first_unadjudicated=$id
      break 2
    done
    sleep 2
  done
  if [ -n "$first_unadjudicated" ]; then
    suite_log "run $n: $first_unadjudicated failed and is no ledger row — stopping the pass here (fail early; the diff follows)"
    kill -KILL -- "-$CHECK_PGID" 2>/dev/null || true
    SUITE_JUDGE_NO_STALE=1
  fi
  wait "$CHECK_PGID" || rc=$?
  CHECK_PGID=""
  fstests_sysctl_restore
  t1=$(date +%s)
  # The summary block: `Ran: …`, `Not run: …`, `Failures: …`, `Failed N of M tests`.
  # A pass stopped early has none: its counts are the results dir's.
  local ran notrun failed
  if [ -n "$first_unadjudicated" ]; then
    ran=$(ls "$SETS"/results/*/*.full 2>/dev/null | wc -l)
    notrun=$(ls "$SETS"/results/*/*.notrun 2>/dev/null | wc -l)
    (cd "$SETS/results" && ls */*.out.bad 2>/dev/null | sed 's/\.out\.bad$//' | sort -u) > "$failures" || true
    rc=1
  else
    ran=$(grep -E '^Ran: ' "$RESULTS/check-run$n.out" | tail -1 | sed 's/^Ran: //' | wc -w)
    notrun=$(grep -E '^Not run: ' "$RESULTS/check-run$n.out" | tail -1 | sed 's/^Not run: //' | wc -w)
    grep -E '^Failures: ' "$RESULTS/check-run$n.out" | tail -1 | sed 's/^Failures: //' | tr ' ' '\n' | grep . | sort -u > "$failures" || true
  fi
  failed=$(grep -c . "$failures" || true)
  suite_log "run $n: check rc=$rc, ran $ran, not run $notrun, failed $failed, $((t1 - t0)) s wall ($(grep -E '^(Passed all|Failed [0-9]+ of)' "$RESULTS/check-run$n.out" | tail -1)${first_unadjudicated:+stopped early at $first_unadjudicated})"
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
  # Every set's daemons: the two loop sets and the raw-device sets tests brought.
  local sets=("$SETS/set-test" "$SETS/set-scratch")
  for m in "$SETS"/sets/*/; do [ -d "$m" ] && sets+=("${m%/}"); done
  for m in "${sets[@]}"; do
    local logs; logs=$(ls "$m"/daemon-*.log 2>/dev/null | wc -l)
    cat "$m"/daemon-*.log 2>/dev/null | grep -E "$SUITE_ALERT_RE" > "$RESULTS/daemon-$(basename "$m")-run$n.alerts" || true
    local na; na=$(wc -l < "$RESULTS/daemon-$(basename "$m")-run$n.alerts")
    suite_log "$(basename "$m"): $logs daemon log(s), $na alert line(s)"
    # A tripwire fails the run, on every daemon — a dm-error daemon's too
    # (owner ruling 2026-10-19): a fence taken while the volume is FAILED
    # is a counted outcome (`*_fence_drops_failed_volume`,
    # `meta_conveyor_divergences_failed_volume`, one info line each), so a
    # tripwire there is a fence on a HEALTHY volume like anywhere else.
    # The FAILED-volume fence lines are histogrammed for the record.
    local l tw
    for l in "$m"/daemon-*.log; do
      [ -f "$l" ] || continue
      tw=$(grep -o "TRIPWIRE '[a-z_]*'" "$l" | sort -u | sed "s/TRIPWIRE '//; s/'//" | tr '\n' ' ')
      [ -n "$tw" ] || continue
      suite_log "TRIPWIRE fired in $(basename "$m") ($(basename "$l")): $tw"; ok=1
    done
    local fv
    fv=$(cat "$m"/daemon-*.log 2>/dev/null | grep -c 'on a FAILED volume\|on a FAILED/fenced volume' || true)
    [ "${fv:-0}" = 0 ] || suite_log "$(basename "$m"): $fv fence event(s) on a FAILED volume (for the record: counted, never a tripwire)"
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
  [ "${SUITE_KEEP_SET:-0}" = 1 ] || rm -f "$SETS"/set-*/meta.img
  [ "$rc" = 0 ] || [ "$rc" = 1 ] || { suite_log "check rc $rc (137 = the wall bound): the pass did not complete"; return 1; }
  [ -z "$first_unadjudicated" ] || return 1
  return $ok
}

# The harness's exit (a pass's end, a stop): `check`'s whole process group
# first, then this run's mounts and loop devices — nothing of a run
# outlives it to meet the next run's devices.
CHECK_PGID=""
fstests_cleanup() {
  if [ -n "$CHECK_PGID" ]; then kill -KILL -- "-$CHECK_PGID" 2>/dev/null || true; CHECK_PGID=""; fi
  fstests_sysctl_restore
  [ -n "${SETS:-}" ] || return 0
  fstests_unmount_all
  fstests_detach_loops
}

main() {
  SUITE_CLEANUP_HOOK=fstests_cleanup
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
