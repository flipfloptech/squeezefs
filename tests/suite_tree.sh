#!/usr/bin/env bash
# The external suites' source trees (xfstests-dev, LTP, pjdfstest) — ONE
# home, ONE liveness check, ONE toolchain shim. Sourced by the three
# runners; never run directly.
#
# Why this exists (the 1.2.3 release chain, 2026-09-11): the trees lived
# under /tmp, where systemd-tmpfiles' 10-day age cleaner removed every
# FILE of the LTP checkout while leaving its DIRECTORIES — the runner's
# `[ -d "$DIR" ]` guard saw the hollow tree, skipped its clone, and
# `make autotools` had nothing to build; the chain stopped in its LTP leg
# on a venue failure and had to be resumed by hand. The same guard shape
# protected all three runners.
#
# Rules:
#   1. Home = $SQUEEZEFS_SUITE_CACHE if set, else $XDG_CACHE_HOME/squeezefs-suites,
#      else /var/cache/squeezefs-suites when writable as root, else the
#      invoking user's ~/.cache/squeezefs-suites. Durable — outside the age
#      cleaner's reach. A tree already present at the legacy /tmp path is
#      ADOPTED (moved) once when it is live, so no re-clone is paid for it.
#   2. Liveness = the tree's MARKER file exists (a real source file the clone
#      always produces), never `-d`: a hollow tree is refreshed (moved aside
#      with a timestamp, re-cloned), loud.
#   3. Autotools for the configure steps come from PATH or, on nix hosts,
#      from the store (automake / autoconf / m4 / libtool / pkg-config's
#      aclocal dir) — under `sudo` PATH is sanitized and none of them are
#      visible, which is the second half of the LTP venue failure.
set -u

suite_tree_home() {
    if [ -n "${SQUEEZEFS_SUITE_CACHE:-}" ]; then
        echo "$SQUEEZEFS_SUITE_CACHE"; return
    fi
    if [ -n "${XDG_CACHE_HOME:-}" ]; then
        echo "$XDG_CACHE_HOME/squeezefs-suites"; return
    fi
    if [ "$(id -u)" = 0 ]; then
        if mkdir -p /var/cache/squeezefs-suites 2>/dev/null && [ -w /var/cache/squeezefs-suites ]; then
            echo /var/cache/squeezefs-suites; return
        fi
        local u="${SUDO_USER:-}"
        if [ -n "$u" ]; then
            local h; h=$(getent passwd "$u" | cut -d: -f6)
            [ -n "$h" ] && { echo "$h/.cache/squeezefs-suites"; return; }
        fi
    fi
    echo "${HOME:-/root}/.cache/squeezefs-suites"
}

# The realtime-CPU budget (the 1.3.0 release chain, attempt 5 — fstests
# generic/631, 2026-09-27): a desktop launcher (Omarchy's quickshell) runs
# with RLIMIT_RTTIME hard = 0 and every process it spawns inherits it — the
# terminal, the shell, the runner, every test. The kernel's RCU-boost
# kthreads (CONFIG_RCU_BOOST) priority-inherit an ordinary task caught in a
# preempted RCU read section into the realtime class for one tick, and at a
# zero budget that tick trips the RT watchdog: the kernel SIGKILLs the task
# (`posix_cpu_timers_work`, SI_KERNEL — no log line, no OOM report). rm /
# touch / mv / bash died mid-test; a suite that spawns thousands of short
# processes meets it within minutes. A VENUE defect — the runner lifts the
# budget (the hard limit is root's to raise) and refuses a zero it cannot.
suite_tree_lift_rttime() {
    local was hard
    was=$(awk '/Max realtime timeout/{print $5}' /proc/$$/limits 2>/dev/null || echo unlimited)
    [ "$was" = "unlimited" ] && return 0
    prlimit --pid $$ --rttime=unlimited:unlimited 2>/dev/null || true
    hard=$(awk '/Max realtime timeout/{print $5}' /proc/$$/limits 2>/dev/null || echo unlimited)
    if [ "$hard" != "unlimited" ]; then
        echo "suite_tree: RLIMIT_RTTIME hard limit is ${hard} µs and could not be lifted — a zero realtime budget lets the kernel SIGKILL RCU-boosted test processes (fstests generic/631); the runners are root-only and root can raise it (sudo -n prlimit --pid \$\$ --rttime=unlimited:unlimited before invoking), or run from a session outside the desktop launcher (a TTY or ssh login inherits systemd's unlimited default)" >&2
        return 1
    fi
    echo "suite_tree: RLIMIT_RTTIME lifted to unlimited (was ${was} µs — the desktop launcher's inherited zero budget)" >&2
}
suite_tree_lift_rttime || exit 2

# suite_tree_ensure NAME REPO_URL MARKER [LEGACY_PATH]
# Prints the tree's path on stdout; clones (or adopts / refreshes) as needed.
suite_tree_ensure() {
    local name="$1" repo="$2" marker="$3" legacy="${4:-}"
    local home; home=$(suite_tree_home)
    local dir="$home/$name"
    mkdir -p "$home"
    if [ ! -f "$dir/$marker" ] && [ -n "$legacy" ] && [ -f "$legacy/$marker" ]; then
        echo "suite_tree: adopting live legacy checkout $legacy -> $dir" >&2
        rm -rf "$dir"
        mv "$legacy" "$dir"
    fi
    if [ -d "$dir" ] && [ ! -f "$dir/$marker" ]; then
        local aside="$dir.hollow.$(date +%s)"
        echo "suite_tree: $dir is HOLLOW (no $marker — age-cleaned or a failed clone); moving aside to $aside and re-cloning" >&2
        mv "$dir" "$aside"
    fi
    if [ ! -d "$dir" ]; then
        echo "suite_tree: cloning $name into $dir" >&2
        git clone --depth 1 "$repo" "$dir" >&2
    fi
    [ -f "$dir/$marker" ] || { echo "suite_tree: $dir still lacks $marker after clone — refusing" >&2; return 1; }
    # Keep the age cleaner off a tree that lives under /tmp after all
    # (SQUEEZEFS_SUITE_CACHE pointed there): touching every file resets
    # its clock for another cycle.
    case "$dir" in /tmp/*) find "$dir" -xdev -exec touch -a {} + 2>/dev/null ;; esac
    echo "$dir"
}

# suite_tree_autotools_env — call DIRECTLY (not in a subshell): sets
# SUITE_AT_PATH to a PATH prefix (possibly empty) that makes
# aclocal/automake/autoconf/autoreconf/m4/libtoolize resolvable, and
# exports ACLOCAL_PATH for pkg.m4 when it has to come from the store.
suite_tree_autotools_env() {
    SUITE_AT_PATH=""
    local need="" t
    for t in aclocal automake autoconf autoreconf m4 libtoolize; do
        command -v "$t" >/dev/null 2>&1 || need="$need $t"
    done
    [ -z "$need" ] && return 0
    local d pat
    for pat in automake autoconf gnum4 libtool; do
        d=$(ls -d /nix/store/*-"$pat"-[0-9]*/bin 2>/dev/null | sort -V | tail -n 1)
        [ -n "$d" ] && SUITE_AT_PATH="${SUITE_AT_PATH:+$SUITE_AT_PATH:}$d"
    done
    if [ -z "${ACLOCAL_PATH:-}" ]; then
        local m4dir; m4dir=$(ls -d /nix/store/*-pkg-config*/share/aclocal 2>/dev/null | head -n 1)
        [ -n "$m4dir" ] && export ACLOCAL_PATH="$m4dir"
    fi
    if [ -z "$SUITE_AT_PATH" ]; then
        echo "suite_tree: autotools missing ($need) and no nix-store copies found — configure will fail; install automake/autoconf/m4/libtool" >&2
    fi
    return 0
}
