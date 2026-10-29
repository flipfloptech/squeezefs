#!/usr/bin/env bash
# The ONE harness for the external conformance suites (charter §11 "Gates":
# release gate = pjdfstest + full LTP + full fstests `-g auto` all passing,
# adjudicated by-design exceptions only) and the two release rows beside
# them (owner, 2026-10-22): `mdtest.sh` (an `mpirun` of mdtest) and
# `tarx.sh` (the reference's `sym-tarx` gate 2 — a joined writer's `tar -x`
# against the manager's own — plus the parallel single-mount form). Sourced
# by `pjdfstest.sh`, `ltp.sh`, `fstests.sh`, `mdtest.sh`, `tarx.sh`; runs
# inside the Linux venue (`scripts/venue`) as root. Nothing here is a
# product path: the product is the `squeezefs` binary this harness formats,
# mounts, drives, unmounts and fscks.
#
# FAIL EARLY, FAIL HARD (owner ruling 2026-10-17): a run stops at the first
# failing test that is not an adjudicated ledger row (LTP: nor the venue's,
# judged by its control run on the spot) — that failure is the work, and the
# hours of tests behind it add nothing to it. A by-design row fails on.
#
#   suite_prepare <suite>        packages (lockfile-checked) + results dir
#   suite_mount <tag>            format a set and mount it with the release posture
#   suite_mount_pair <tag> <peer> format a SYMMETRIC set, mount its manager at <tag>
#                                and a second daemon of the same set, JOINED, at <peer>
#   suite_umount <tag>           orderly `squeezefs umount` (the daemon exits)
#   suite_fsck <tag>             `fsck --offline --scrub`: 0 findings is part of the pass
#   suite_stats_snapshot <tag> <label>   the mount's `.stats` document to the results dir
#   suite_stat <file> <key>      one counter off a snapshot (by leaf name)
#   suite_stats_delta <before> <after> <key…>   Σ of the keys' deltas
#   suite_judge_mount <tag> <label>  the live mount's must-stay-0 set + the watchdog's log
#   suite_judge <suite> <file>   the adjudication ledger over the run's failure ids
#   suite_run_count <n>          the multi-run discipline's run index (log only)
#   suite_host_posture           the host's clock posture (mains, governor, boost, cap) in one line
#   suite_pin_clock / suite_unpin_clock   boost off for a timing row's run, restored after
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
# (8 GiB sparse each) per set, all under
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
# parser warns about every unknown SQZ*-prefixed name it inherits); the
# venue's exports (`scripts/venue`: the zig cache root, the slot) likewise —
# the warning line in a mount's stderr fails every xfstests golden output.
export -n SQZ_BIN SQZ_BUILD_ROOT SQZ_VENUE SQZ_VENUE_SLOT SQZ_VENUE_SLOTS 2>/dev/null || true
# Clones and builds persist on the cache volume between runs; their build
# artefacts are deleted when a suite's run is over (`suite_clean_build`).
SUITES_ROOT=${SUITES_ROOT:-${SQZ_BUILD_ROOT:-/build}/suites}
# Volumes, mountpoints, results: bounded, trimmed.
SUITES_TMP=${SUITES_TMP:-${SQZ_BUILD_ROOT:-/build}/tmp}
SUITE_META_MIB=${SUITE_META_MIB:-1024}
SUITE_DATA_GIB=${SUITE_DATA_GIB:-8}
# The release posture; see the module comment for each option's WHY.
SUITE_MOUNT_OPTS=${SUITE_MOUNT_OPTS:-default_permissions,allow_other}
# A daemon log's loud lines (`suite_daemon_alerts`, fstests.sh's per-set
# census): a `--log-file` line is `<epoch s>.<ms> <level>(<scope>): …`
# (`cli/log_sink.zig`), a stderr line `<level>(<scope>): …`, and the warn
# level's text is `warning` (`std.log.Level.asText`). The level token counts
# at the line's start or after a blank (the prefix's), never mid-word; a
# TRIPWIRE token counts anywhere. (Until 2026-10-22 the pattern was
# `^(error|warn)\(`: anchored at column 0 under a prefixed log and naming a
# level text that never occurs, it counted 1,407 `error(` lines as 0 alerts
# and matched the TRIPWIRE token alone.)
SUITE_ALERT_RE='(^|[[:space:]])(error|warning)\(|TRIPWIRE'
# The venue's package family, from /etc/os-release (a derivative names its parent in
# ID_LIKE: Omarchy says ID=omarchy ID_LIKE=arch):
#   apt     Debian/Ubuntu. One lock per distro release (`apt.<ID>-<VERSION_ID>.lock`): each
#           release ships its own package versions (the VM's Ubuntu 24.04, kamrui's 26.04),
#           so a shared lock is a drift on every venue but the one that pinned; a drift is a
#           loud refusal, never a silent upgrade (the suites' own behaviour is part of the
#           gate's evidence).
#   pacman  Arch and its derivatives. NO lock (owner ruling 2026-10-16: a rolling release,
#           bleeding-edge packages for a bleeding-edge filesystem): the suites install what
#           the mirrors carry today; the versions a run used go into its results dir
#           (packages.txt) as evidence for reading the run, never as a gate.
read -r SUITE_PKG_FAMILY SUITE_RELEASE < <(. /etc/os-release && case " $ID ${ID_LIKE:-} " in
  *" arch "*) echo "pacman $ID-${VERSION_ID:-rolling}" ;;
  *) echo "apt $ID-$VERSION_ID" ;;
esac)
APT_LOCK="$SUITES_DIR/apt.$SUITE_RELEASE.lock"

# The packages the three suites need to build and run, per family.
#   build: autoconf automake libtool pkg-config gcc make (pjdfstest, LTP, xfstests)
#   LTP:   libaio-dev libacl1-dev libattr1-dev libcap-dev libnuma-dev libtirpc-dev
#          libkeyutils-dev libssl-dev libmnl-dev linux-headers (none: syscalls only)
#   xfstests: xfsprogs e2fsprogs attr acl quota bc fio dbench(absent on noble)
#          libgdbm-dev uuid-dev gettext libtool-bin indent psmisc sed xfslibs-dev
#          liburing-dev python3 (the `check` driver), gawk, duperemove(absent)
#   tools: attr acl (setfattr/getfattr/setfacl), psmisc (fuser/killall), bc
# Arch ships headers inside the library's package (acl, attr, xfsprogs, openssl, …), so
# the -dev rows collapse onto them; uuid-runtime/uuid-dev are util-linux(-libs), quota is
# quota-tools, libnuma is numactl, python3/pkg-config are python/pkgconf.
case $SUITE_PKG_FAMILY in
  apt) SUITE_PACKAGES=(
    autoconf automake libtool libtool-bin pkg-config gcc make bison flex gettext indent
    attr acl bc fio psmisc gawk sed uuid-runtime xfsprogs e2fsprogs quota
    libaio-dev libacl1-dev libattr1-dev libcap-dev libnuma-dev libtirpc-dev libkeyutils-dev
    libssl-dev libmnl-dev libgdbm-dev uuid-dev xfslibs-dev liburing-dev libicu-dev
    python3 perl
  ) ;;
  pacman) SUITE_PACKAGES=(
    autoconf automake libtool pkgconf gcc make bison flex gettext indent
    attr acl bc fio psmisc gawk sed util-linux util-linux-libs xfsprogs e2fsprogs quota-tools
    libaio libcap numactl libtirpc keyutils openssl libmnl gdbm liburing icu
    python perl
  ) ;;
esac

suite_log() {
  # Before `suite_begin` there is no results dir: stderr only.
  if [ -n "${RESULTS:-}" ]; then printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$RESULTS/harness.log" >&2
  else printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" >&2; fi
}
suite_die() { suite_log "FATAL: $*"; exit 2; }

suite_require_venue() {
  [ "$(uname -s)" = Linux ] || suite_die "the suites run only in the Linux venue (scripts/venue …)"
  [ "$(id -u)" = 0 ] || suite_die "the suites need root (mount(2), the suites' -u/-g legs)"
  [ -x "$SQZ_BIN" ] || suite_die "no binary at $SQZ_BIN (scripts/venue zig build)"
  [ -c /dev/fuse ] || suite_die "/dev/fuse absent"
  # A desktop's udisks auto-mounts the loop devices the suites format (LTP's
  # .all_filesystems tests, xfstests' devices): the next mkfs sees "is mounted".
  # provision-local-venue's udev rule keeps it off loop devices backed under the build root.
  if { [ -x /usr/lib/udisks2/udisksd ] || [ -x /usr/libexec/udisks2/udisksd ]; } && [ ! -f /etc/udev/rules.d/99-sqz-loop-ignore.rules ]; then
    suite_die "udisks is installed and no /etc/udev/rules.d/99-sqz-loop-ignore.rules keeps it off the suites' loop devices (scripts/dev/provision-local-venue --apply)"
  fi
}

# Install the package set once. apt: verify the lockfile every time — a missing lockfile
# is written from the installed versions (the first run pins); `SUITE_REFRESH_LOCK=1`
# re-pins after a deliberate upgrade. pacman: no lock; the run records its versions.
suite_pkg_installed() {
  case $SUITE_PKG_FAMILY in
    apt) dpkg-query -W -f='${Status}' "$1" 2>/dev/null | grep -q 'install ok installed' ;;
    pacman) pacman -Q "$1" >/dev/null 2>&1 ;;
  esac
}
# `name=version` per package. Byte order: the caller's locale reaches here through ssh
# and sudo, and en_US.UTF-8 sorts `libtool=` before `libtool-bin=` while C sorts it after
# — a lock pinned under one refused every run under the other (kamrui, the gate train's
# LC_ALL=C).
suite_pkg_versions() {
  local p
  case $SUITE_PKG_FAMILY in
    apt) for p in "${SUITE_PACKAGES[@]}"; do dpkg-query -W -f='${Package}=${Version}\n' "$p"; done ;;
    pacman) pacman -Q "${SUITE_PACKAGES[@]}" | sed 's/ /=/' ;;
  esac | LC_ALL=C sort
}
suite_packages() {
  local missing=() p
  for p in "${SUITE_PACKAGES[@]}"; do suite_pkg_installed "$p" || missing+=("$p"); done
  if [ ${#missing[@]} -gt 0 ]; then
    case $SUITE_PKG_FAMILY in
      apt)
        suite_log "apt-get install: ${missing[*]}"
        DEBIAN_FRONTEND=noninteractive apt-get -qq update >/dev/null
        DEBIAN_FRONTEND=noninteractive apt-get -qq install -y --no-install-recommends "${missing[@]}" >/dev/null
        apt-get -qq clean ;;
      pacman)
        # From the sync db as it is: the harness never refreshes it alone (`-Sy` + an
        # install is a partial upgrade on Arch). A stale db means `pacman -Syu` first.
        suite_log "pacman -S --needed: ${missing[*]}"
        pacman -S --needed --noconfirm "${missing[@]}" >/dev/null \
          || suite_die "pacman -S failed for ${missing[*]} (a stale sync db? run pacman -Syu, then the suite)" ;;
    esac
  fi
  local current; current=$(suite_pkg_versions)
  if [ "$SUITE_PKG_FAMILY" = pacman ]; then
    printf '%s\n' "$current" > "${RESULTS:-/tmp}/packages.txt"
    suite_log "${#SUITE_PACKAGES[@]} packages, rolling (no lock); versions in packages.txt"
    return 0
  fi
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
  # A stop (the venue's timeout, an operator's kill) exits through the
  # EXIT trap: bash runs no trap when a signal simply kills it.
  trap 'exit 143' TERM INT HUP
}

# ---- one run per host: the suites whose state is the HOST's -----------------
# fstests re-points host state for its whole run — the /sbin helpers
# util-linux dispatches to, /run/squeezefs-suites.env, the shared clone's
# local.config — so two harnesses that both passed a process-list check
# re-pointed each other's (a void pass, decision 0003 §CS). The run holds
# one flock from before its first host write to after its restore: the fd
# lives until the harness exits, after its EXIT trap; a child that may
# outlive a step closes it (`{SUITE_HOST_LOCK_FD}>&-`).
SUITE_HOST_LOCK_FD=""
suite_host_lock() {
  local name=$1 wait_s=$2
  local lock="/run/lock/squeezefs-suite-$name.lock" owner="/run/lock/squeezefs-suite-$name.owner"
  exec {SUITE_HOST_LOCK_FD}>>"$lock"
  if ! flock -n "$SUITE_HOST_LOCK_FD"; then
    suite_log "waiting for suite run by $(cat "$owner" 2>/dev/null || echo "an unnamed holder") — one $name run per host (up to ${wait_s} s)"
    flock -w "$wait_s" "$SUITE_HOST_LOCK_FD" || suite_die "the $name suite run by $(cat "$owner" 2>/dev/null) still holds the host after ${wait_s} s"
  fi
  echo "pid $$ (owner $REPO, results $RESULTS)" > "$owner"
  suite_log "holding the host's $name lock ($lock)"
}

# ---- the set: format + mount with the release posture ----------------------

declare -A SUITE_MNT SUITE_META SUITE_PID SUITE_SETDIR
# peer tag → the set's tag (`suite_mount_peer`): a peer leaves before its manager.
declare -A SUITE_PEER_OF

suite_set_paths() {
  local tag=$1
  SUITE_SETDIR[$tag]="$RESULTS/set-$tag"
  SUITE_MNT[$tag]="$RESULTS/mnt-$tag"
  SUITE_META[$tag]="sqmeta://${SUITE_SETDIR[$tag]}/meta.img"
}

# The format class: the SHIPPED default — symmetric (charter §5.7: the D0
# winner is the manager, a later RW mount of the same set JOINS it), the
# posture every suite judges (owner ruling 2026-10-23; until then the
# suites formatted `--single-writer`, a 2026-10-10 harness choice under
# which xfstests' `mount` group only ever confirmed the guard's BUSY
# refusal of a second mount). `SUITE_SINGLE_WRITER=1` formats the
# `--single-writer` class instead (one writer, no wire) for a run that
# wants it. `suite_mount_pair`'s set (tarx.sh's A-B-B-A row) is symmetric
# whatever the variable says. The write cache is every mount's (RAM; the
# production shape of a client).
SUITE_SINGLE_WRITER=${SUITE_SINGLE_WRITER:-0}

suite_format() {
  local tag=$1; shift
  suite_set_paths "$tag"
  local d=${SUITE_SETDIR[$tag]}
  rm -rf "$d"; mkdir -p "$d"
  truncate -s "${SUITE_META_MIB}M" "$d/meta.img"
  truncate -s "${SUITE_DATA_GIB}G" "$d/d0.img" "$d/d1.img"
  local writers=()
  [ "$SUITE_SINGLE_WRITER" = 1 ] && writers=(--single-writer)
  "$SQZ_BIN" format "${writers[@]}" "$@" "${SUITE_META[$tag]}" "sqdata://$d/d0.img,$d/d1.img" \
    >"$d/format.out" 2>&1 || { cat "$d/format.out" >&2; suite_die "format failed ($tag)"; }
  suite_log "formatted $tag${writers[0]:+ (single writer)}: $(head -1 "$d/format.out")"
}

# One daemon of set `set_tag`'s metadata, mounted at `mnt`, logging to
# `log`; extra args go to `mount` verbatim. Records the daemon's pid under
# `tag` (its `.stats`).
suite_spawn_daemon() {
  local tag=$1 set_tag=$2 mnt=$3 log=$4; shift 4
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
    "$SQZ_BIN" mount --daemon --supervise --log-file "$log" -o "$SUITE_MOUNT_OPTS" "$@" "${SUITE_META[$set_tag]}" "$mnt" && break
    if [ "$attempt" -lt 3 ] && grep -q 'ENOMEM (order-8' "$log"; then
      suite_log "mount $tag: kmbuf ENOMEM (fragmented venue); compacting memory and retrying ($attempt/3)"
      sync -f "${SUITE_SETDIR[$set_tag]}"; { echo 3 > /proc/sys/vm/drop_caches; echo 1 > /proc/sys/vm/compact_memory; } 2>/dev/null || true; sleep 2
      continue
    fi
    tail -40 "$log" >&2; suite_die "mount failed ($tag)"
  done
  grep -q " $mnt fuse.squeezefs " /proc/mounts || suite_die "$mnt is not in /proc/mounts after a ready mount"
  SUITE_PID[$tag]=$(grep -o '"pid":[0-9]*' "$mnt/.stats" | head -1 | cut -d: -f2)
  suite_log "mounted $tag at $mnt (daemon pid ${SUITE_PID[$tag]}, -o $SUITE_MOUNT_OPTS $*)"
}

# Mount `tag` (formatting it unless SUITE_KEEP_SET=1 and it exists).
# Extra args after the tag go to `mount` verbatim (e.g. -o ro).
suite_mount() {
  local tag=$1; shift
  suite_set_paths "$tag"
  [ -f "${SUITE_SETDIR[$tag]}/meta.img" ] && [ "${SUITE_KEEP_SET:-0}" = 1 ] || suite_format "$tag"
  suite_spawn_daemon "$tag" "$tag" "${SUITE_MNT[$tag]}" "$RESULTS/daemon-$tag.log" "$@"
}

# ---- two daemons of one set on one host (tarx.sh's A-B-B-A row) ------------
#
# The set is formatted in the SYMMETRIC posture whatever SUITE_SINGLE_WRITER
# says; the first daemon wins D0 and is the MANAGER, the second
# finds the manager's listener through the page and JOINS as appender 1
# (charter §5.7; the smoke's TWO MOUNTS OF ONE SET ON ONE HOST row). Both
# members live on this host, so the ONE listener binds loopback
# (`SQUEEZEFS_MW_BIND`, a registry knob; `auto` would advertise the
# default route's address to a wire no second host is on). `suite_umount`,
# `suite_daemon_alerts` and `suite_judge_mount` take either tag; the set's
# fsck is `suite_fsck <tag>` after BOTH are unmounted (the peer first: the
# manager's clean close is the set's).
suite_mount_pair() {
  local tag=$1 peer=$2; shift 2
  export SQUEEZEFS_MW_BIND=${SUITE_MW_BIND:-127.0.0.1:0}
  local SUITE_SINGLE_WRITER=0
  suite_mount "$tag" "$@"
  suite_mount_peer "$tag" "$peer" "$@"
}

# A second daemon of `tag`'s (mounted, symmetric) set at `mnt-<peer>`.
suite_mount_peer() {
  local tag=$1 peer=$2; shift 2
  SUITE_SETDIR[$peer]=${SUITE_SETDIR[$tag]}
  SUITE_META[$peer]=${SUITE_META[$tag]}
  SUITE_MNT[$peer]="$RESULTS/mnt-$peer"
  SUITE_PEER_OF[$peer]=$tag
  local log="$RESULTS/daemon-$peer.log"
  suite_spawn_daemon "$peer" "$tag" "${SUITE_MNT[$peer]}" "$log" "$@"
  grep -q 'mounted JOINED as appender' "$log" || { tail -20 "$log" >&2; suite_die "$peer did not JOIN $tag's set (no 'mounted JOINED as appender' line)"; }
  suite_log "$peer JOINED $tag's set: $(grep -o 'mounted JOINED as appender [0-9]*' "$log" | head -1)"
}

suite_umount() {
  local tag=$1
  local mnt=${SUITE_MNT[$tag]:-}
  [ -n "$mnt" ] || return 0
  if grep -q " $mnt " /proc/mounts; then
    # Anything a suite left behind with its cwd on the mount keeps umount EBUSY.
    fuser -k -M -m "$mnt" >/dev/null 2>&1 || true
    # A test killed mid-run (the fail-early stop) leaves its mounts stacked
    # inside ours and its loop devices on files of ours: holders no process
    # census sees (2026-10-22: landlock04's loop0 kept the mount EBUSY after a
    # "no holder" census and forced the lazy detach). Deepest mount first.
    local m d
    for m in $(awk -v p="$mnt/" 'index($5, p) == 1 { print $5 }' /proc/self/mountinfo | sort -r); do umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null || true; done
    for d in $(losetup -ln -O NAME,BACK-FILE 2>/dev/null | awk -v p="$mnt/" 'index($2, p) == 1 { print $1 }'); do losetup -d "$d" 2>/dev/null || suite_log "loop device $d (backed under $mnt) would not detach"; done
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

# The daemon log's loud lines (errors, warnings, tripwires; `SUITE_ALERT_RE`):
# counted on every run (a 0 is part of the pass's evidence), printed when
# there are any; a tripwire line is a gate failure on its own.
suite_daemon_alerts() {
  local tag=$1
  local log="$RESULTS/daemon-$tag.log"
  [ -f "$log" ] || return 0
  grep -E "$SUITE_ALERT_RE" "$log" > "$RESULTS/daemon-$tag.alerts" || true
  local n; n=$(wc -l < "$RESULTS/daemon-$tag.alerts")
  suite_log "daemon $tag: $n alert line(s)$([ "$n" = 0 ] || echo " (see daemon-$tag.alerts)")"
  [ "$n" = 0 ] || head -20 "$RESULTS/daemon-$tag.alerts" >&2
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

# ---- the mount's `.stats` document ----------------------------------------
#
# A snapshot is the whole document (`cat`, never `cp`: the virtual file has
# no size for copy_file_range to trust) to
# `$RESULTS/stats-<tag>-<label>.json`; a counter is read by its LEAF name,
# first match in document order (every name below is unique in the
# document; `scripts/bracket/brk.py`'s `flatten` reads it the same way).
suite_stats_snapshot() {
  local tag=$1 label=$2 out="$RESULTS/stats-$1-$2.json"
  cat "${SUITE_MNT[$tag]}/.stats" > "$out" || suite_die "cannot read ${SUITE_MNT[$tag]}/.stats ($label)"
  echo "$out"
}
suite_stat() {   # file key → the value (0 when the key is absent, said on stderr)
  python3 - "$1" "$2" <<'PY'
import json, sys
def leaves(o, out):
    if isinstance(o, dict):
        for k, v in o.items():
            if not isinstance(v, (dict, list)) and k not in out: out[k] = v
        for v in o.values(): leaves(v, out)
    elif isinstance(o, list):
        for v in o: leaves(v, out)
    return out
d = leaves(json.load(open(sys.argv[1])), {})
if sys.argv[2] not in d: print("suite_stat: no leaf named %s in %s" % (sys.argv[2], sys.argv[1]), file=sys.stderr)
print(d.get(sys.argv[2], 0))
PY
}
suite_stats_delta() {   # before after key… → Σ (after − before) over the keys
  local before=$1 after=$2; shift 2
  python3 - "$before" "$after" "$@" <<'PY'
import json, sys
def leaves(o, out):
    if isinstance(o, dict):
        for k, v in o.items():
            if not isinstance(v, (dict, list)) and k not in out: out[k] = v
        for v in o.values(): leaves(v, out)
    elif isinstance(o, list):
        for v in o: leaves(v, out)
    return out
b = leaves(json.load(open(sys.argv[1])), {}); a = leaves(json.load(open(sys.argv[2])), {})
missing = [k for k in sys.argv[3:] if k not in a]
if missing: print("suite_stats_delta: no leaf named %s" % ", ".join(missing), file=sys.stderr)
print(sum(int(a.get(k, 0)) - int(b.get(k, 0)) for k in sys.argv[3:]))
PY
}

# The live mount's health after a workload (owner, 2026-10-22: every
# tripwire judged, the FUSE op watchdog never overdue): `.stats` readable,
# `invariant_tripwires` 0, `fuse_op_watchdog_overdue` 0, and no `FUSE op
# watchdog:` line in the daemon's log (the watchdog's report of an op past
# its class's threshold — logged, never cancelled; charter §4). The
# snapshot is kept as `stats-<tag>-<label>.json`. Returns 1 on a finding.
suite_judge_mount() {
  local tag=$1 label=$2 rc=0
  local snap; snap=$(suite_stats_snapshot "$tag" "$label") || return 1
  local tw od
  tw=$(suite_stat "$snap" invariant_tripwires); od=$(suite_stat "$snap" fuse_op_watchdog_overdue)
  [ "$tw" = 0 ] || { suite_log "$tag ($label): invariant_tripwires=$tw (must stay 0)"; rc=1; }
  [ "$od" = 0 ] || { suite_log "$tag ($label): fuse_op_watchdog_overdue=$od (must stay 0)"; rc=1; }
  local n; n=$(grep -c 'FUSE op watchdog:' "$RESULTS/daemon-$tag.log" 2>/dev/null || true)
  [ "${n:-0}" = 0 ] || { suite_log "$tag ($label): the FUSE op watchdog logged $n overdue op(s):"; grep 'FUSE op watchdog:' "$RESULTS/daemon-$tag.log" | head -5 | cut -c1-240 | sed 's/^/    /' >&2; rc=1; }
  [ "$rc" = 0 ] && suite_log "$tag ($label): .stats readable, invariant_tripwires 0, fuse_op_watchdog_overdue 0, no watchdog line"
  return $rc
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
  # A suite's own first step (fstests: its `check` process group, its sets).
  if [ -n "${SUITE_CLEANUP_HOOK:-}" ]; then "$SUITE_CLEANUP_HOOK" || true; fi
  suite_unpin_clock || true
  # Joined peers before their managers (a manager's clean close is the set's).
  for tag in "${!SUITE_PEER_OF[@]}"; do [ -n "${SUITE_MNT[$tag]:-}" ] && suite_umount "$tag" || true; done
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
# tag (`utimensat01`), fstests `generic/003`, mdtest `np32/<check>`
# (`np32/phase:File creation`), tarx `abba/<check>` and `parallel/<check>`
# (`abba/wall`, `abba/tree:b1@a`).
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
  # A pass stopped early (fail early) never reached most rows: no stale verdict.
  [ "${SUITE_JUDGE_NO_STALE:-0}" = 1 ] || cut -f1 "$ledger" | sort -u | while read -r id; do
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

# ---- the host's clock (a timing row's venue) --------------------------------
#
# A wall law divides one measurement by another, so the host's clock must
# hold still across both. It does not by itself (2026-10-22, this
# workstation): a laptop on battery runs its SoC in the firmware's battery
# power regime — tarx read 1.3–2.2 s per extraction on battery against
# 0.65–0.9 s on mains, arms minutes apart 2.5× apart as the boost budget
# came and went, every op's latency and the daemon's CPU per op shifted
# uniformly; and on mains a thermal guard's cap (this box's
# sqz-thermal-governor: `scaling_max_freq` 5.19 → 4.0 GHz at 84 °C, back
# 30 s later) flips inside nearly every 40 s phase and moves a
# single-threaded extraction 1.2–1.3×, while a co-tenant's burst shares the
# SoC's power budget and does the same with nothing in sysfs to show it.
# The posture line records it; the pin takes the boost out for the run.
SUITE_BOOST_CTL=/sys/devices/system/cpu/cpufreq/boost
SUITE_BOOST_SAVED=""
SUITE_MAX_KHZ_SAVED=""
# A thermal guard's record of the cap it currently intends (this box's
# sqz-thermal-governor writes it on every step): the unpin restores THAT
# when it exists, since the guard's steps during the run were clamped away.
SUITE_CLOCK_CAP_FILE=${SUITE_CLOCK_CAP_FILE:-/run/sqz-thermal-governor/cap_khz}

# `mains=<on|off|none> gov=<governor> epp=<preference> boost=<0|1>
# max_khz=<scaling_max_freq>` (cpu0's policy; a field a host lacks is
# omitted; `mains=none` is a host without a mains supply, i.e. not a laptop).
suite_host_posture() {
  local mains=none ps t on
  for ps in /sys/class/power_supply/*; do
    [ -r "$ps/type" ] && t=$(cat "$ps/type") || continue
    [ "$t" = Mains ] || continue
    on=$(cat "$ps/online" 2>/dev/null || echo 0)
    if [ "$on" = 1 ]; then mains=on; break; else mains=off; fi
  done
  local cf=/sys/devices/system/cpu/cpu0/cpufreq out="mains=$mains"
  [ -r $cf/scaling_governor ] && out="$out gov=$(cat $cf/scaling_governor)"
  [ -r $cf/energy_performance_preference ] && out="$out epp=$(cat $cf/energy_performance_preference)"
  [ -r $SUITE_BOOST_CTL ] && out="$out boost=$(cat $SUITE_BOOST_CTL)"
  [ -r $cf/scaling_max_freq ] && out="$out max_khz=$(cat $cf/scaling_max_freq)"
  echo "$out"
}

# Pin the clock for a timing row: boost off (`cpufreq/boost`), so every
# core's ceiling is the nominal frequency (3.0 GHz here against 5.19 GHz
# boosted) — the thermal guard's cap writes above it clamp to it and
# change nothing, the SoC stays inside its sustained power budget, and the
# ceiling is the same for every arm. Restored by `suite_unpin_clock` (the
# suite's cleanup calls it): boost back, then `scaling_max_freq` written
# back to every policy — the guard's current cap (SUITE_CLOCK_CAP_FILE)
# when there is one, else the value found at pin time (re-enabling boost
# lifts it to the hardware maximum, which would drop a guard's cap until
# its next step). A host without the control (a VM, an intel_pstate box:
# `no_turbo` is not handled) is said and left alone.
suite_pin_clock() {
  if [ ! -w "$SUITE_BOOST_CTL" ]; then suite_log "clock pin: no $SUITE_BOOST_CTL on this host — the clock is not pinned; the posture rules alone judge the measurement"; return 0; fi
  SUITE_BOOST_SAVED=$(cat "$SUITE_BOOST_CTL")
  SUITE_MAX_KHZ_SAVED=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq)
  echo 0 > "$SUITE_BOOST_CTL"
  suite_log "clock pinned: boost $SUITE_BOOST_SAVED -> 0 (ceiling $(cat /sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq) kHz; scaling_max_freq was $SUITE_MAX_KHZ_SAVED) for the run; restored after"
}
suite_unpin_clock() {
  [ -n "$SUITE_BOOST_SAVED" ] || return 0
  echo "$SUITE_BOOST_SAVED" > "$SUITE_BOOST_CTL" || true
  local p khz=$SUITE_MAX_KHZ_SAVED from="pin time"
  if [ -r "$SUITE_CLOCK_CAP_FILE" ]; then khz=$(cat "$SUITE_CLOCK_CAP_FILE"); from="the guard's $SUITE_CLOCK_CAP_FILE"; fi
  for p in /sys/devices/system/cpu/cpufreq/policy*; do echo "$khz" > "$p/scaling_max_freq" 2>/dev/null || true; done
  suite_log "clock unpinned: boost restored to $SUITE_BOOST_SAVED, scaling_max_freq to $khz ($from)"
  SUITE_BOOST_SAVED=""; SUITE_MAX_KHZ_SAVED=""
}

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
