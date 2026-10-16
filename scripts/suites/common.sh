#!/usr/bin/env bash
# The ONE harness for the external conformance suites (charter §11 "Gates":
# release gate = pjdfstest + full LTP + full fstests `-g auto` all passing,
# adjudicated by-design exceptions only). Sourced by `pjdfstest.sh`,
# `ltp.sh`, `fstests.sh`; runs inside the Linux venue (`scripts/venue`) as
# root. Nothing here is a product path: the product is the `squeezefs`
# binary this harness formats, mounts, drives, unmounts and fscks.
#
#   suite_prepare <suite>        packages (lockfile-checked) + results dir
#   suite_mount <tag>            format a set and mount it with the release posture
#   suite_umount <tag>           orderly `squeezefs umount` (the daemon exits)
#   suite_fsck <tag>             `fsck --offline --scrub`: 0 findings is part of the pass
#   suite_judge <suite> <file>   the adjudication ledger over the run's failure ids
#   suite_run_count <n>          the multi-run discipline's run index (log only)
#
# Posture (charter §1.6 "never a silent fallback to a weaker posture"): the
# suites mount EXACTLY what production mounts. `-o default_permissions`
# (the kernel enforces mode/owner/sticky semantics, the `.stats` 0400, the
# sgid-on-chmod clearing law — the daemon never re-implements permission
# arithmetic) and `-o allow_other` (pjdfstest, LTP and fstests exercise
# every op as unprivileged uids through `-u/-g`; the kernel refuses a
# foreign uid's access to a FUSE mount without it). No `noatime` (the
# suites' atime rows are ADJUDICATED, not hidden: the charter names noatime
# an accepted exception class), no `nosuid`, no TTL change, no writeback.
#
# Scratch is bounded: one metadata volume (1 GiB sparse) + two data volumes
# (8 GiB sparse each) + a staging/cache path per set, all under
# `/build/tmp/suites-<suite>-<date>-<pid>/` on the venue's ext4 (sparse files,
# real block semantics; never /build/ext — AGENTS.md "Test data and disk").
# `scripts/venue trim` returns the blocks after a run.
set -euo pipefail

SUITES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SUITES_DIR/../.." && pwd)"
# `--bin PATH` (the build graph's emitted binary) wins over SQZ_BIN, which
# defaults to the venue's install prefix.
if [ "${1:-}" = --bin ]; then SQZ_BIN=$2; shift 2; fi
SQZ_BIN=${SQZ_BIN:-$REPO/zig-out-linux/bin/squeezefs}
# The harness's own variable, never the daemon's environment (the knob
# parser warns about every unknown SQZ*-prefixed name it inherits).
export -n SQZ_BIN
# Clones and builds persist on the cache volume between runs; their build
# artefacts are deleted when a suite's run is over (`suite_clean_build`).
SUITES_ROOT=${SUITES_ROOT:-${SQZ_BUILD_ROOT:-/build}/suites}
# Volumes, mountpoints, results: bounded, trimmed.
SUITES_TMP=${SUITES_TMP:-${SQZ_BUILD_ROOT:-/build}/tmp}
SUITE_META_MIB=${SUITE_META_MIB:-1024}
SUITE_DATA_GIB=${SUITE_DATA_GIB:-8}
# The release posture; see the module comment for each option's WHY.
SUITE_MOUNT_OPTS=${SUITE_MOUNT_OPTS:-default_permissions,allow_other}
# One lock per distro release (`apt.<ID>-<VERSION_ID>.lock`): each venue's
# release ships its own package versions (the VM's Ubuntu 24.04, kamrui's
# 26.04), so a shared lock is a drift on every venue but the one that pinned.
APT_LOCK="$SUITES_DIR/apt.$(. /etc/os-release && echo "$ID-$VERSION_ID").lock"

# apt packages the three suites need to build and run; the release's lock pins the
# versions this harness verified (a drift is a loud refusal, never a silent
# upgrade: the suites' own behaviour is part of the gate's evidence).
#   build: autoconf automake libtool pkg-config gcc make (pjdfstest, LTP, xfstests)
#   LTP:   libaio-dev libacl1-dev libattr1-dev libcap-dev libnuma-dev libtirpc-dev
#          libkeyutils-dev libssl-dev libmnl-dev linux-headers (none: syscalls only)
#   xfstests: xfsprogs e2fsprogs attr acl quota bc fio dbench(absent on noble)
#          libgdbm-dev uuid-dev gettext libtool-bin indent psmisc sed xfslibs-dev
#          liburing-dev python3 (the `check` driver), gawk, duperemove(absent)
#   tools: attr acl (setfattr/getfattr/setfacl), psmisc (fuser/killall), bc
SUITE_PACKAGES=(
  autoconf automake libtool libtool-bin pkg-config gcc make bison flex gettext indent
  attr acl bc fio psmisc gawk sed uuid-runtime xfsprogs e2fsprogs quota
  libaio-dev libacl1-dev libattr1-dev libcap-dev libnuma-dev libtirpc-dev libkeyutils-dev
  libssl-dev libmnl-dev libgdbm-dev uuid-dev xfslibs-dev liburing-dev libicu-dev
  python3 perl
)

suite_log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "${RESULTS:-/dev/null}/harness.log" >&2; }
suite_die() { suite_log "FATAL: $*"; exit 2; }

suite_require_venue() {
  [ "$(uname -s)" = Linux ] || suite_die "the suites run only in the Linux venue (scripts/venue …)"
  [ "$(id -u)" = 0 ] || suite_die "the suites need root (mount(2), the suites' -u/-g legs)"
  [ -x "$SQZ_BIN" ] || suite_die "no binary at $SQZ_BIN (scripts/venue zig build)"
  [ -c /dev/fuse ] || suite_die "/dev/fuse absent"
}

# Install the package set once; verify the lockfile every time. A missing
# lockfile is written from the installed versions (the first run pins);
# `SUITE_REFRESH_LOCK=1` re-pins after a deliberate upgrade.
suite_packages() {
  local missing=() p
  for p in "${SUITE_PACKAGES[@]}"; do
    dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q 'install ok installed' || missing+=("$p")
  done
  if [ ${#missing[@]} -gt 0 ]; then
    suite_log "apt-get install: ${missing[*]}"
    DEBIAN_FRONTEND=noninteractive apt-get -qq update >/dev/null
    DEBIAN_FRONTEND=noninteractive apt-get -qq install -y --no-install-recommends "${missing[@]}" >/dev/null
    apt-get -qq clean
  fi
  local current
  # Byte order: the caller's locale reaches here through ssh and sudo, and en_US.UTF-8
  # sorts `libtool=` before `libtool-bin=` while C sorts it after — a lock pinned under
  # one refused every run under the other (kamrui, the gate train's LC_ALL=C).
  current=$(for p in "${SUITE_PACKAGES[@]}"; do dpkg-query -W -f='${Package}=${Version}\n' "$p"; done | LC_ALL=C sort)
  if [ ! -s "$APT_LOCK" ] || [ "${SUITE_REFRESH_LOCK:-0}" = 1 ]; then
    printf '%s\n' "$current" > "$APT_LOCK"
    suite_log "$(basename "$APT_LOCK") written (${#SUITE_PACKAGES[@]} packages)"
    return 0
  fi
  if ! diff -u "$APT_LOCK" <(printf '%s\n' "$current") >"${RESULTS:-/tmp}/apt-lock.diff"; then
    cat "${RESULTS:-/tmp}/apt-lock.diff" >&2
    suite_die "installed package versions drift from $APT_LOCK (SUITE_REFRESH_LOCK=1 re-pins deliberately)"
  fi
}

# Results dir: /build/tmp/suites-<suite>-<date>-<pid>/ (the daemon logs,
# every run's raw output, the fsck reports, the verdicts; the sets and their
# mountpoints live under it). The pid: two worktrees' gates that start a
# suite in the same second would otherwise format, mount and judge ONE set
# (docs/evidence/rung-09-gates.md §11). `SUITE_RESULTS` overrides for a
# resumed multi-run count.
suite_prepare() {
  local suite=$1
  suite_require_venue
  mkdir -p "$SUITES_TMP" "$SUITES_ROOT"
  RESULTS=${SUITE_RESULTS:-"$SUITES_TMP/suites-$suite-$(date +%Y%m%d-%H%M%S)-$$"}
  mkdir -p "$RESULTS"
  export RESULTS
  suite_log "results: $RESULTS; binary: $("$SQZ_BIN" --version)"
  suite_packages
  trap suite_cleanup EXIT
}

# ---- the set: format + mount with the release posture ----------------------

declare -A SUITE_MNT SUITE_META SUITE_PID SUITE_SETDIR

suite_set_paths() {
  local tag=$1
  SUITE_SETDIR[$tag]="$RESULTS/set-$tag"
  SUITE_MNT[$tag]="$RESULTS/mnt-$tag"
  SUITE_META[$tag]="sqmeta://${SUITE_SETDIR[$tag]}/meta.img"
}

suite_format() {
  local tag=$1; shift
  suite_set_paths "$tag"
  local d=${SUITE_SETDIR[$tag]}
  rm -rf "$d"; mkdir -p "$d/cache"; chmod 0700 "$d/cache"
  truncate -s "${SUITE_META_MIB}M" "$d/meta.img"
  truncate -s "${SUITE_DATA_GIB}G" "$d/d0.img" "$d/d1.img"
  # `--single-writer`: the suites are one mount on one host; the symmetric
  # join ladder is the N-daemon matrix's business. The cache path turns the
  # staging ring and the NVMe tier ON (the production shape of a client).
  "$SQZ_BIN" format --single-writer --disk-cache-paths "$d/cache" "$@" "${SUITE_META[$tag]}" "sqdata://$d/d0.img,$d/d1.img" \
    >"$d/format.out" 2>&1 || { cat "$d/format.out" >&2; suite_die "format failed ($tag)"; }
  suite_log "formatted $tag: $(head -1 "$d/format.out")"
}

# Mount `tag` (formatting it unless SUITE_KEEP_SET=1 and it exists).
# Extra args after the tag go to `mount` verbatim (e.g. -o ro).
suite_mount() {
  local tag=$1; shift
  suite_set_paths "$tag"
  [ -f "${SUITE_SETDIR[$tag]}/meta.img" ] && [ "${SUITE_KEEP_SET:-0}" = 1 ] || suite_format "$tag"
  local mnt=${SUITE_MNT[$tag]} log="$RESULTS/daemon-$tag.log"
  mkdir -p "$mnt"
  # The kmbuf ring wants order-8 contiguous pages; a fragmented venue
  # (other agents' daemons, a hot page cache) answers ENOMEM. Compact and
  # retry — never a weaker buffer mode to pass (charter §1.6/§1.8).
  # `--supervise` IS the release posture (charter §4 "an external mount
  # supervisor"): a daemon that dies is named with its wait status in the
  # log (two silent deaths under venue OOM pressure left no trace without
  # it), and a daemon that stops answering is dumped (every task's kernel
  # stack) and aborted — a wedge becomes evidence, never a hung suite.
  local attempt
  for attempt in 1 2 3; do
    "$SQZ_BIN" mount --daemon --supervise --log-file "$log" -o "$SUITE_MOUNT_OPTS" "$@" "${SUITE_META[$tag]}" "$mnt" && break
    if [ "$attempt" -lt 3 ] && grep -q 'ENOMEM (order-8' "$log"; then
      suite_log "mount $tag: kmbuf ENOMEM (fragmented venue); compacting memory and retrying ($attempt/3)"
      sync -f "${SUITE_SETDIR[$tag]}"; { echo 3 > /proc/sys/vm/drop_caches; echo 1 > /proc/sys/vm/compact_memory; } 2>/dev/null || true; sleep 2
      continue
    fi
    tail -40 "$log" >&2; suite_die "mount failed ($tag)"
  done
  grep -q " $mnt fuse.squeezefs " /proc/mounts || suite_die "$mnt is not in /proc/mounts after a ready mount"
  SUITE_PID[$tag]=$(grep -o '"pid":[0-9]*' "$mnt/.stats" | head -1 | cut -d: -f2)
  suite_log "mounted $tag at $mnt (daemon pid ${SUITE_PID[$tag]}, -o $SUITE_MOUNT_OPTS $*)"
}

suite_umount() {
  local tag=$1
  local mnt=${SUITE_MNT[$tag]:-}
  [ -n "$mnt" ] || return 0
  if grep -q " $mnt " /proc/mounts; then
    # Anything a suite left behind with its cwd on the mount keeps umount EBUSY.
    fuser -k -M -m "$mnt" >/dev/null 2>&1 || true
    "$SQZ_BIN" umount "$mnt" >>"$RESULTS/harness.log" 2>&1 || { suite_log "squeezefs umount $tag refused; detaching"; umount -l "$mnt" || true; }
  fi
  local pid=${SUITE_PID[$tag]:-}
  if [ -n "$pid" ]; then
    local i; for i in $(seq 1 300); do suite_alive "$pid" || break; sleep 0.1; done
    suite_alive "$pid" && { suite_log "daemon $pid still alive after umount; SIGKILL"; kill -9 "$pid" || true; }
  fi
  SUITE_MNT[$tag]=""
  suite_log "unmounted $tag"
}

# Liveness by /proc state: the venue's init (`sleep infinity`) reaps
# nothing, so an exited `--daemon` child stays a zombie that `kill -0`
# would still report alive.
suite_alive() {
  local st
  st=$(awk '{print $3}' "/proc/$1/stat" 2>/dev/null) || return 1
  [ -n "$st" ] && [ "$st" != Z ] && [ "$st" != X ]
}

# Live (non-zombie) squeezefs pids.
suite_live_daemons() {
  local p
  for p in $(pgrep -x squeezefs); do suite_alive "$p" && echo "$p"; done
  return 0
}

# The daemon log's loud lines (errors, warnings, tripwires): printed and
# counted; a tripwire line is a gate failure on its own.
suite_daemon_alerts() {
  local tag=$1
  local log="$RESULTS/daemon-$tag.log"
  [ -f "$log" ] || return 0
  grep -E '^(error|warn)\(|TRIPWIRE' "$log" > "$RESULTS/daemon-$tag.alerts" || true
  local n; n=$(wc -l < "$RESULTS/daemon-$tag.alerts")
  [ "$n" = 0 ] || { suite_log "daemon $tag: $n alert line(s) (see daemon-$tag.alerts)"; head -20 "$RESULTS/daemon-$tag.alerts" >&2; }
  if grep -q TRIPWIRE "$RESULTS/daemon-$tag.alerts"; then suite_log "TRIPWIRE fired in daemon $tag"; return 1; fi
  # A daemon the kernel OOM-killed (the venue's 16 GiB shared with other
  # agents' daemons) is a run failure with its cause named, never a silent
  # "every later test failed".
  local pid=${SUITE_PID[$tag]:-}
  if [ -n "$pid" ] && dmesg 2>/dev/null | grep -q "Killed process $pid (squeezefs)"; then
    dmesg | grep "Killed process $pid (squeezefs)" | tail -1 | tee -a "$RESULTS/daemon-$tag.alerts" >&2
    suite_log "daemon $tag (pid $pid) was OOM-KILLED by the venue — the run is void (co-tenant daemons: $(suite_live_daemons | tr '\n' ' '))"
    return 1
  fi
}

# `fsck --offline --scrub` over the unmounted set: 0 findings (exit 0) is
# part of every suite's pass. The JSON report is kept.
suite_fsck() {
  local tag=$1 rc=0
  local out="$RESULTS/fsck-$tag-$(date +%H%M%S).json"
  suite_set_paths "$tag"
  "$SQZ_BIN" fsck --offline --scrub --json "${SUITE_META[$tag]}" > "$out" 2>"$out.err" || rc=$?
  if [ "$rc" = 0 ]; then
    suite_log "fsck --offline --scrub $tag: 0 findings"
    return 0
  fi
  suite_log "fsck --offline --scrub $tag: exit $rc — $out"
  head -c 2000 "$out" >&2; echo >&2
  grep -v '^info\|^debug' "$out.err" | head -20 >&2
  return 1
}

suite_cleanup() {
  local tag
  for tag in "${!SUITE_MNT[@]}"; do [ -n "${SUITE_MNT[$tag]}" ] && suite_umount "$tag" || true; done
  # Never leave OUR daemons or mounts behind on the shared venue (other
  # agents' daemons are theirs: listed, never signalled).
  local pid
  for pid in "${SUITE_PID[@]}"; do [ -n "$pid" ] && suite_alive "$pid" && { suite_log "daemon $pid (ours) still alive; SIGKILL"; kill -9 "$pid" || true; }; done
  for m in $(awk -v r="$RESULTS" '$3 == "fuse.squeezefs" && index($2, r "/") == 1 {print $2}' /proc/mounts); do umount -l "$m" || true; done
  local live; live=$(suite_live_daemons | tr '\n' ' ')
  [ -z "$live" ] || suite_log "note: live squeezefs daemons not ours: $live"
  # The data volumes are the bulk: drop them, keep the logs and reports.
  if [ -n "${RESULTS:-}" ] && [ "${SUITE_KEEP_SET:-0}" != 1 ]; then rm -f "$RESULTS"/set-*/d?.img "$RESULTS"/set-*/meta.img; rm -rf "$RESULTS"/set-*/cache; fi
  # The run's own filesystem only: a host-wide sync waits on every FUSE
  # mount on the venue, another run's frozen daemon included (0003 §Z).
  if [ -n "${RESULTS:-}" ] && [ -d "$RESULTS" ]; then sync -f "$RESULTS"; fi
}

# Delete a clone's build artefacts once a suite run is over (`make clean`
# class — the sources stay, the next run rebuilds in minutes).
suite_clean_build() {
  local dir=$1
  [ -d "$dir" ] || return 0
  (cd "$dir" && { make -s clean >/dev/null 2>&1 || true; git clean -qfdX >/dev/null 2>&1 || true; })
}

# ---- the adjudication ledger ------------------------------------------------
#
# `docs/gates/<suite>-exceptions.md` lists ONE table:
#
#   | test-id | reason-class | evidence |
#
# `reason-class` ∈ {noatime, thin-provisioning, i64-timestamp-range,
# by-design-<name>} (charter §11: the three named classes plus a NAMED
# by-design call recorded in decision 0002). The reader is strict: a
# failure id not in the ledger FAILS the gate (exit 1); a ledger row whose
# id did not fail in this run is STALE (exit 3) — the ledger never rots:
# a fixed defect's row goes, or the suite stopped covering it and the row
# says so. Ids are the suite's own: pjdfstest `chmod/12.t#3`, LTP the test
# tag (`utimensat01`), fstests `generic/003`.
suite_ledger_rows() {
  local suite=$1
  local f="$REPO/docs/gates/$suite-exceptions.md"
  [ -f "$f" ] || return 0
  # Table body rows only: `| id | class | evidence |`, skipping the header and the rule.
  awk -F'|' '/^\|/ && $2 !~ /^[ -]*test-id[ -]*$/ && $2 !~ /^[ -]+$/ { gsub(/^[ \t]+|[ \t]+$/, "", $2); gsub(/^[ \t]+|[ \t]+$/, "", $3); if ($2 != "") print $2 "\t" $3 }' "$f"
}

# $1 suite, $2 file with one failure id per line (empty = clean run), $3
# an optional id prefix scoping the STALE check to the rows this run could
# have exercised (LTP's `fs/` vs `syscalls/`; fstests' `generic/`).
# Prints the verdict; returns 0 pass, 1 unadjudicated failures, 3 stale rows.
suite_judge() {
  local suite=$1 failures=$2 scope=${3:-}
  local ledger="$RESULTS/ledger-$suite.tsv" unadj="$RESULTS/unadjudicated.txt" stale="$RESULTS/stale-rows.txt" adj="$RESULTS/adjudicated.txt"
  suite_ledger_rows "$suite" | { if [ -n "$scope" ]; then grep "^$scope" || true; else cat; fi; } > "$ledger"
  : > "$unadj"; : > "$stale"; : > "$adj"
  local bad_class=0 id cls
  while IFS=$'\t' read -r id cls; do
    case "$cls" in
      noatime|thin-provisioning|i64-timestamp-range|by-design-*) ;;
      *) suite_log "ledger row '$id' has reason-class '$cls' outside {noatime, thin-provisioning, i64-timestamp-range, by-design-<name>}"; bad_class=1 ;;
    esac
  done < "$ledger"
  sort -u "$failures" | while read -r id; do
    [ -n "$id" ] || continue
    if cls=$(awk -F'\t' -v id="$id" '$1 == id {print $2; exit}' "$ledger") && [ -n "$cls" ]; then echo "$id	$cls" >> "$adj"; else echo "$id" >> "$unadj"; fi
  done
  cut -f1 "$ledger" | sort -u | while read -r id; do
    [ -n "$id" ] || continue
    grep -qxF "$id" "$failures" || echo "$id" >> "$stale"
  done
  local nf nu ns na
  nf=$(sort -u "$failures" | grep -c . || true); nu=$(grep -c . "$unadj" || true); ns=$(grep -c . "$stale" || true); na=$(grep -c . "$adj" || true)
  suite_log "ledger verdict ($suite): $nf failure id(s): $na adjudicated, $nu UNADJUDICATED; $ns stale ledger row(s)"
  [ "$nu" = 0 ] || { suite_log "unadjudicated failures (each is a defect to repro-port and fix, or a by-design row with evidence):"; sed 's/^/    /' "$unadj" >&2; }
  [ "$ns" = 0 ] || { suite_log "stale ledger rows (no failure this run; remove or re-justify):"; sed 's/^/    /' "$stale" >&2; }
  [ "$bad_class" = 0 ] || return 1
  [ "$nu" = 0 ] || return 1
  [ "$ns" = 0 ] || return 3
  return 0
}

# The multi-run discipline (charter §11): a failure on any early run aborts
# the count; acceptance counts restart from zero on the fixed binary. The
# driver loops `SUITE_RUNS` (default 3) times over a FRESH set each time and
# stops at the first non-pass.
SUITE_RUNS=${SUITE_RUNS:-3}
suite_run_count() { suite_log "run $1 of $SUITE_RUNS on $("$SQZ_BIN" --version)"; }

# Wall time helper: `suite_timed <label> cmd…` logs elapsed seconds.
suite_timed() {
  local label=$1; shift
  local t0 t1 rc=0
  t0=$(date +%s)
  "$@" || rc=$?
  t1=$(date +%s)
  suite_log "$label: ${rc} after $((t1 - t0)) s"
  return $rc
}
