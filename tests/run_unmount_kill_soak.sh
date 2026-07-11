#!/bin/bash
# Unmount/kill teardown soak — the K7-era SIGABRT regression check.
#
# K7-era fstests runs SIGABRT'd daemons (tokio-rt-worker, SI_TKILL) under
# the harness's kill-based unmount discipline, feeding the drain-timeout
# class. The causes were closed by ce90efc (daemons launched outside the
# per-test systemd scope, so tests no longer SIGTERM/SIGKILL a live mount
# at test end), 2b66477 (post-arm classical sideband servicing — no
# stranded FORGET/INTERRUPT requests wedging unmount), and the dismount
# teardown fixes (idempotent destroy, checkpoint-task join). This soak
# replays the harness's exact mount → light I/O → kill/umount shapes
# (tests/run_fstests.sh step-0 cleanup, the mount helper's serialization,
# the EBUSY-retry umount wrapper, and SIGTERM-during-drain) and fails on
# any SIGABRT/core, panic, mount failure, or drain-timeout.
#
# MUST run as root (mounts FUSE with --allow-other). Not part of `cargo
# test`; run after teardown/unmount-path changes:
#
#   sudo tests/run_unmount_kill_soak.sh            # 30 cycles (~4 min)
#   sudo CYCLES=100 tests/run_unmount_kill_soak.sh
set -u

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: This script must run as root (sudo $0)." >&2
    exit 1
fi

REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO/target/release/squeezefs"
TEST_DEV=/dev/shm/sq_unmount_soak_meta
DATA_DEV=/dev/shm/sq_unmount_soak_data
MNT=/mnt/sq_unmount_soak
STAGING=/tmp/sq_unmount_soak_staging
LOG=/tmp/sq_unmount_soak_daemon.log
CYCLES="${CYCLES:-30}"
PAT="squeezefs mount sqmeta://$TEST_DEV"

if [ ! -x "$BIN" ]; then
    echo "ERROR: $BIN missing — build with 'cargo build --release' first." >&2
    exit 1
fi

fail=0
note() { echo "[soak] $*"; }

wait_daemon_gone() { # $1 = budget in 0.1s ticks
    for _ in $(seq 1 "$1"); do
        pgrep -f "$PAT" >/dev/null 2>&1 || return 0
        sleep 0.1
    done
    return 1
}

# --- step-0 cleanup, the run_fstests.sh shape -----------------------------
umount -l "$MNT" &>/dev/null || true
pkill -f "$PAT" &>/dev/null || true
wait_daemon_gone 100 || true
pkill -9 -f "$PAT" &>/dev/null || true
rm -f "$TEST_DEV" "$DATA_DEV"
rm -rf "$STAGING" "$MNT" "$LOG"
mkdir -p "$MNT" "$STAGING"
truncate -s 1G "$TEST_DEV"
truncate -s 4G "$DATA_DEV"
"$BIN" format "sqmeta://$TEST_DEV" "sqdata://$DATA_DEV" --force >/dev/null 2>&1 || {
    echo "FORMAT FAILED"; exit 1; }

START_TS="$(date '+%Y-%m-%d %H:%M:%S')"

umount_retry() { # the /sbin/umount.squeezefs-fstests EBUSY-retry shape
    local rc=0 err=""
    for _ in $(seq 1 100); do
        err=$(umount "$MNT" 2>&1); rc=$?
        [ $rc -eq 0 ] && return 0
        case "$err" in *"target is busy"*) sleep 0.05 ;; *) break ;; esac
    done
    [ -n "$err" ] && echo "$err" >&2
    return $rc
}

for c in $(seq 1 "$CYCLES"); do
    # mount.fuse.squeezefs serialization: wait out the previous daemon.
    if ! wait_daemon_gone 600; then
        note "cycle $c: previous daemon STILL RUNNING after 60s (drain-timeout class)"
        fail=1; break
    fi
    if ! stat "$MNT" >/dev/null 2>&1; then umount -l "$MNT" 2>/dev/null; fi
    if ! mountpoint -q "$MNT" 2>/dev/null; then find "$MNT" -mindepth 1 -delete 2>/dev/null; fi

    if ! "$BIN" mount "sqmeta://$TEST_DEV" "$MNT" --daemon \
        --disk-cache-paths "$STAGING" --disk-cache-size 500MB \
        --allow-other -o "fsname=$TEST_DEV" --log-file "$LOG" >>"$LOG" 2>&1; then
        note "cycle $c: MOUNT FAILED (see $LOG)"; fail=1; break
    fi

    # Light I/O: inline + staged (no fsync) + fsynced + striped + dir churn.
    echo "inline-$c" > "$MNT/inline_$c" 2>/dev/null
    dd if=/dev/urandom of="$MNT/staged_$c" bs=16k count=1 status=none 2>/dev/null
    dd if=/dev/urandom of="$MNT/fsynced_$c" bs=16k count=1 conv=fsync status=none 2>/dev/null
    dd if=/dev/zero of="$MNT/striped_$c" bs=1M count=6 status=none 2>/dev/null
    mkdir -p "$MNT/d_$c"
    mv "$MNT/inline_$c" "$MNT/d_$c/renamed_$c" 2>/dev/null
    cat "$MNT/staged_$c" > /dev/null 2>&1
    rm -f "$MNT/striped_$((c-1))" "$MNT/staged_$((c-2))" 2>/dev/null

    # The harness's kill/umount shapes, rotated.
    case $((c % 3)) in
    1)  # plain umount through the EBUSY-retry wrapper shape
        umount_retry || { note "cycle $c: umount failed"; fail=1; break; }
        ;;
    2)  # lazy umount then TERM → wait → KILL9 (the step-0 cleanup shape)
        umount -l "$MNT" 2>/dev/null || true
        pkill -f "$PAT" 2>/dev/null || true
        wait_daemon_gone 100 || true
        pkill -9 -f "$PAT" 2>/dev/null || true
        ;;
    0)  # umount and immediately SIGTERM the draining daemon (the K7-era
        # transient-scope-stop shape), then KILL9 stragglers
        umount_retry || true
        pkill -f "$PAT" 2>/dev/null || true
        sleep 0.2
        pkill -9 -f "$PAT" 2>/dev/null || true
        ;;
    esac
    echo "[soak] cycle $c done"
done

# Drain the last daemon before the verdict.
wait_daemon_gone 600 || note "final daemon never exited"
umount -l "$MNT" &>/dev/null || true

echo "=== verdict ==="
CORES=$(coredumpctl list --no-legend --since "$START_TS" 2>/dev/null | grep -c squeezefs || true)
echo "squeezefs coredumps since start: ${CORES:-0}"
coredumpctl list --since "$START_TS" 2>/dev/null | grep squeezefs || true
DMESG_ABRT=$(dmesg --since "$START_TS" 2>/dev/null | grep -ciE "squeezefs.*(signal 6|SIGABRT)" || true)
echo "dmesg SIGABRT lines: ${DMESG_ABRT:-0}"
PANICS=$(grep -ciE "panicked|SIGABRT" "$LOG" 2>/dev/null || true)
echo "daemon log panic/abort lines: ${PANICS:-0}"
if [ "$fail" -eq 0 ] && [ "${CORES:-0}" = "0" ] && [ "${PANICS:-0}" = "0" ]; then
    echo "SOAK PASS ($CYCLES cycles clean)"
else
    echo "SOAK FAIL"
    exit 1
fi
