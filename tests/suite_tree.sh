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
