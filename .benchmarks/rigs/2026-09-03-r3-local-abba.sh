#!/usr/bin/env bash
# .benchmarks/rigs/2026-09-03-r3-local-abba.sh — the R-3 local A-B-B-A
# driver (tcp devsub): remount per arm with the SAME binary, the lever
# alternating through the env (A = SQUEEZEFS_FUSE_ZC_READ_FUSION=0, the
# classic handler-lane dispatch; B = default ON), one row per arm.
#
# Usage (root): BIN=... MNT=/mnt/sqz-r3 RESULTS=/tmp/r3/abba $0 <row> <secs> [A B B A]
#   META/DATA default to the tcp devsub's namespaces; the volume must be
#   formatted and laid out already (`…-rig.sh layout`).
set -euo pipefail
BIN="${BIN:?}"
MNT="${MNT:-/mnt/sqz-r3}"
RESULTS="${RESULTS:-/tmp/r3/abba}"
META="${META:-sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1}"
ROW="${1:-rr4k_qd8}"
SECS="${2:-30}"
shift 2 || true
ARMS=("${@:-A B B A}")
[ ${#ARMS[@]} -gt 0 ] || ARMS=(A B B A)
RIG="$(dirname "$0")/2026-09-03-r3-fill-issue-economy-rig.sh"
mkdir -p "$RESULTS"

mount_arm() { # <arm>
    local arm="$1" env=()
    case "$arm" in
        A) env=(SQUEEZEFS_FUSE_ZC_READ_FUSION=0) ;;
        B) env=() ;;
        *) echo "unknown arm $arm" >&2; exit 1 ;;
    esac
    "$BIN" umount "$MNT" >/dev/null 2>&1 || true
    sleep 1
    env "${env[@]}" ${TRACE_ENV:-} "$BIN" mount "$META" "$MNT" --daemon --interception --allow-other \
        --log-file "$RESULTS/mount-$arm-$(date +%s).log" >/dev/null
    sleep 2
    cat "$MNT/.stats" >/dev/null
}

i=0
for arm in "${ARMS[@]}"; do
    i=$((i + 1))
    mount_arm "$arm"
    env MNT="$MNT" RESULTS="$RESULTS" TRACE="${TRACE:-0}" "$RIG" "$ROW" kern "$SECS" "$ROW-$arm$i" | head -12
done
"$BIN" umount "$MNT" >/dev/null 2>&1 || true
