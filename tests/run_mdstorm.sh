#!/usr/bin/env bash
# run_mdstorm.sh — the packaged mdstorm instrument (reconstructed 2026-08-16
# from the 2026-07-14 baseline's ad-hoc harness, committed this time).
#
# One LEG = fresh format + mount + the canonical phase sequence at --scale:
#   mkdir 20k → create 100k → stat 100k → rename 100k → unlink 100k →
#   manydirs create+unlink 100k → rmdir 20k         (100% scale)
# Rows report ops/s per phase. The MW §6.3 S4 residual runs it A-B-B-A:
#   sudo tests/run_mdstorm.sh abba          # stamped→unstamped→unstamped→stamped
#   sudo tests/run_mdstorm.sh leg [--format-args=--single-writer] [--tag=X]
# (Since the rung-10b Phase-B flip the DEFAULT format is the stamped class;
# the interesting non-default --format-args posture is --single-writer.)
# Env: SQZ_MDSTORM_SCALE (pct, default 100), SQZ_MDSTORM_THREADS (default 8),
#      SQZ_MDSTORM_DIR (substrate dir, default /dev/shm/sqz_mdstorm).
# Quiet gate: refuses when 1-min load ≥ SQZ_MDSTORM_MAX_LOAD (default 2.0)
# or foreign cargo/rustc work is running; every leg re-checks after its
# phases and marks the row DIRTY if load returned mid-leg.
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
SQZ_BIN="${SQZ_BIN:-$REPO_DIR/target/release/squeezefs}"
DIR="${SQZ_MDSTORM_DIR:-/dev/shm/sqz_mdstorm}"
THREADS="${SQZ_MDSTORM_THREADS:-8}"
SCALE="${SQZ_MDSTORM_SCALE:-100}"
MAX_LOAD="${SQZ_MDSTORM_MAX_LOAD:-2.0}"
MNT="$DIR/mnt"
STORM="$DIR/mdstorm"

die() { echo "[mdstorm] FATAL: $*" >&2; exit 1; }
log() { echo "[mdstorm] $*"; }

[ "$(id -u)" -eq 0 ] || die "run as root (mount)"
[ -x "$SQZ_BIN" ] || die "missing $SQZ_BIN (cargo build --release)"

foreign_work() { # comm-exact, the baseline's protocol (pgrep -f false-positives)
    pgrep -x cargo >/dev/null || pgrep -x rustc >/dev/null ||
        pgrep -x fio >/dev/null || pgrep -x elbencho >/dev/null ||
        pgrep -x fsstress >/dev/null || pgrep -x fsx >/dev/null
}

quiet_or_die() {
    # WAIT for quiet rather than refuse outright: the previous leg's own
    # storm load takes minutes to decay out of load1 — only FOREIGN work
    # is a refusal, our own echo just needs draining (bounded 10 min).
    foreign_work && die "foreign cargo/rustc/fio work running — refuse the measured row"
    local load
    for _ in $(seq 1 60); do
        load="$(cut -d' ' -f1 /proc/loadavg)"
        awk -v l="$load" -v m="$MAX_LOAD" 'BEGIN{exit !(l<m)}' && return 0
        sleep 10
    done
    die "box never went quiet (load1=$load >= $MAX_LOAD after 10 min)"
}

quiet_flag() { # -> "clean" | "DIRTY(foreign)" — the storm's OWN load is not dirt
    if foreign_work; then echo "DIRTY(foreign)"; else echo clean; fi
}

build_storm() {
    cc -O2 -pthread -o "$STORM" "$REPO_DIR/tests/mdstorm.c" || die "cc mdstorm.c"
}

mount_up() { # <format-args...>
    mkdir -p "$MNT" "$DIR/stage"
    rm -f "$DIR/meta.img" "$DIR/data.img"
    rm -rf "$DIR/stage" && mkdir -p "$DIR/stage"
    truncate -s 2G "$DIR/meta.img"
    truncate -s 8G "$DIR/data.img"
    # Word-splitting of "$@" is the point (extra format args).
    "$SQZ_BIN" format "sqmeta://$DIR/meta.img" "sqdata://$DIR/data.img" \
        --disk-cache-paths "$DIR/stage" --force "$@" \
        >"$DIR/format.log" 2>&1 || die "format (see $DIR/format.log)"
    # shellcheck disable=SC2094 # --log-file is a path arg, not a read
    "$SQZ_BIN" mount "sqmeta://$DIR/meta.img" "$MNT" --daemon --allow-other \
        --log-file "$DIR/mount.log" >>"$DIR/mount.log" 2>&1 || die "mount"

    for _ in $(seq 1 100); do
        mountpoint -q "$MNT" && break
        sleep 0.2
    done
    mountpoint -q "$MNT" || die "mount never appeared (see $DIR/mount.log)"
}

unmount_down() {
    "$SQZ_BIN" umount "$MNT" >/dev/null 2>&1 || true
    for _ in $(seq 1 100); do
        mountpoint -q "$MNT" || break
        sleep 0.2
    done
    mountpoint -q "$MNT" && fusermount3 -uz "$MNT"
    # The daemon's staging flock releases at process exit; wait it out so
    # back-to-back legs never race it (the generic/003 lesson).
    for _ in $(seq 1 100); do
        pgrep -f "squeezefs mount sqmeta://$DIR/meta.img" >/dev/null || break
        sleep 0.2
    done
}

leg() { # <tag> <format-args...>
    local tag="$1"
    shift
    quiet_or_die
    mount_up "$@"
    local work="$MNT/storm" out="$DIR/row_$tag.txt"
    mkdir -p "$work"
    local n_small=$((20000 * SCALE / 100)) n_big=$((100000 * SCALE / 100))
    cat "$MNT/.stats" >"$DIR/stats_${tag}_pre.json" 2>/dev/null || true
    local mwork="$MNT/manydirs"
    mkdir -p "$mwork"
    {
        "$STORM" "$work" "$THREADS" "$n_small" mkdir
        "$STORM" "$work" "$THREADS" "$n_big" create
        "$STORM" "$work" "$THREADS" "$n_big" stat
        "$STORM" "$work" "$THREADS" "$n_big" rename
        "$STORM" "$work" "$THREADS" "$n_big" unlink
        "$STORM" "$mwork" "$THREADS" "$n_big" manydirs
        "$STORM" "$work" "$THREADS" "$n_small" rmdir
    } >"$out"
    cat "$MNT/.stats" >"$DIR/stats_${tag}_post.json" 2>/dev/null || true
    local flag
    flag="$(quiet_flag)"
    unmount_down
    log "leg $tag ($flag):"
    sed "s/^/  [$tag] /" "$out"
    [ "$flag" = clean ] || log "WARN: leg $tag is $flag — row is provisional"
}

cmd="${1:-abba}"
shift || true
mkdir -p "$DIR"
build_storm
case "$cmd" in
leg)
    tag=leg
    fmt_args=()
    for a in "$@"; do
        case "$a" in
        --tag=*) tag="${a#--tag=}" ;;
        --format-args=*) read -r -a fmt_args <<<"${a#--format-args=}" ;;
        *) die "unknown arg $a" ;;
        esac
    done
    leg "$tag" "${fmt_args[@]}"
    ;;
abba)
    # A-B-B-A: stamped → unstamped → unstamped → stamped (aging-order rule).
    # Since the rung-10b Phase-B flip the DEFAULT format IS the stamped
    # class, so the stamped legs run bare and the unstamped legs carry the
    # explicit --single-writer opt-out.
    leg stamped_1
    leg unstamped_1 --single-writer
    leg unstamped_2 --single-writer
    leg stamped_2
    log "A-B-B-A complete — rows in $DIR/row_*.txt, stats snapshots beside them"
    ;;
*) die "usage: run_mdstorm.sh [leg|abba] …" ;;
esac
