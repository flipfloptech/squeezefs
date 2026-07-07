#!/bin/bash
# Ad-hoc LTP data-path reproduction harness (investigation only; not part of gate).
# Mounts a fresh squeezefs as the current user and runs the specified LTP test
# binaries against it, capturing full stdout+stderr per test.
#
# Usage: tests/ltp_repro.sh [SQUEEZEFS_META_SECTOR_LOCKS=0|1] test1 test2 ...
set -uo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO_DIR/target/release/squeezefs"
TAG="${TAG:-sqrepro}"
MOUNT_DIR="/tmp/${TAG}_mount"
STAGING_DIR="/tmp/${TAG}_staging"
META="/dev/shm/${TAG}_meta"
BACKEND="/dev/shm/${TAG}_backend"
LOG="/tmp/${TAG}.log"
LTP_BIN="/tmp/ltp_install/testcases/bin"

cleanup() {
    fusermount3 -u "$MOUNT_DIR" 2>/dev/null || umount -l "$MOUNT_DIR" 2>/dev/null || true
    pkill -9 -f "squeezefs mount .*${TAG}" 2>/dev/null || true
    sleep 0.5
    rm -rf "$MOUNT_DIR" "$STAGING_DIR" "$META" "$BACKEND"
}
trap cleanup EXIT

cleanup
mkdir -p "$MOUNT_DIR" "$STAGING_DIR"
truncate -s 1G "$META"
truncate -s 1G "$BACKEND"

echo "== format =="
"$BIN" format "sqmeta://$META" "sqdata://$BACKEND" --disk-cache-paths "$STAGING_DIR" --force >/dev/null 2>&1

echo "== mount (SQUEEZEFS_META_SECTOR_LOCKS=${SQUEEZEFS_META_SECTOR_LOCKS:-unset}) =="
RUST_LOG=warn "$BIN" mount "sqmeta://$META" "$MOUNT_DIR" \
    --daemon --disk-cache-paths "$STAGING_DIR" --disk-cache-size 500MB \
    --log-file "$LOG" >/dev/null 2>&1

for i in $(seq 1 20); do mountpoint -q "$MOUNT_DIR" && break; sleep 0.5; done
if ! mountpoint -q "$MOUNT_DIR"; then
    echo "MOUNT FAILED"; tail -30 "$LOG" 2>/dev/null; exit 1
fi
echo "mounted OK"

WORK="$MOUNT_DIR/work"
mkdir -p "$WORK"
FAILED=()
for t in "$@"; do
    echo "================= $t ================="
    if env TMPDIR="$WORK" "$LTP_BIN/$t" 2>&1; then
        echo ">>> $t: PASS (exit 0)"
    else
        rc=$?
        echo ">>> $t: NONZERO exit=$rc"
        FAILED+=("$t")
    fi
done
echo "===================================="
echo "Nonzero-exit tests: ${FAILED[*]:-none}"
