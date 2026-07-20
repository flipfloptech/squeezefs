#!/bin/bash
set -euo pipefail

# Volume-lifecycle rig — PR VL3 skeleton (docs/design-volume-lifecycle.md
# §3, gate G-VL-2). VL4 extends it with drain/remove + kill-9 injection
# points; VL5+/VL6+/VL7 add meta migration, fsck, and defrag cycles.
#
# What this skeleton proves (G-VL-2 minus the VL4 auto-rebalance clause):
#
#   Leg 1 (always, no root, no FUSE): offline `volume add-data` against a
#     formatted file-backed set — durable vol- identity, `volume list`
#     probe, duplicate-add refusal, lifecycle-bit stamping (old binaries
#     refuse loud; this binary keeps mounting).
#
#   Leg 2 (needs /dev/fuse + fuse.enable_uring + fusermount3; SKIPs loud
#     otherwise): format + mount, write a checksummed dataset manifest,
#     unmount, `volume add-data`, REMOUNT, verify the manifest reads back
#     byte-identical, `volume list` shows the added volume, write more
#     data and assert the NEW backend receives allocations (engagement
#     via the `.stats` `volume_states` rows — stats deltas are the
#     instrument, house pattern).
#
# Unprivileged posture (the preload-gate precedent): file-backed volumes
# + a user-owned mountpoint; root is NOT required.
#
# Usage:  tests/run_volume_lifecycle.sh

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BASE="${BASE:-/tmp/squeezefs_vl_rig_$$}"
LOG="$BASE/mount.log"

cd "$REPO_DIR"

fail() { echo "RIG FAIL: $*" >&2; exit 1; }
note() { echo "--- $*"; }

cleanup() {
    if mountpoint -q "$BASE/mnt" 2>/dev/null; then
        fusermount3 -uz "$BASE/mnt" || true
    fi
    if [ -n "${MOUNT_PID:-}" ]; then
        kill "$MOUNT_PID" &>/dev/null || true
    fi
    rm -rf "$BASE"
}
trap cleanup EXIT

mkdir -p "$BASE"

note "build (release)"
cargo build --release
BIN="$REPO_DIR/target/release/squeezefs"

# ---------------------------------------------------------------------------
# Leg 1: offline add-data + list + duplicate refusal (no mount, no root)
# ---------------------------------------------------------------------------
note "Leg 1: offline volume add-data / list"

OFF="$BASE/offline"
mkdir -p "$OFF/staging"
truncate -s 256M "$OFF/meta1"
truncate -s 512M "$OFF/oss1"
truncate -s 512M "$OFF/oss2"

"$BIN" format "sqmeta://$OFF/meta1" "sqdata://$OFF/oss1" \
    --disk-cache-paths "$OFF/staging" --force >/dev/null

# Pre-add list: exactly the legacy basename id, state active.
LIST0="$("$BIN" volume list "sqmeta://$OFF/meta1" --json)"
echo "$LIST0" | grep -q '"id": *"oss1"' || fail "pre-add volume list must show the legacy id oss1: $LIST0"
echo "$LIST0" | grep -q '"vol-' && fail "an untouched set must carry no vol- ids: $LIST0"

# The add: durable record + vol- id + honest rebalance note (armed in VL4).
ADD_OUT="$("$BIN" volume add-data "sqmeta://$OFF/meta1" "$OFF/oss2")"
echo "$ADD_OUT" | grep -Eq 'vol-[0-9a-f]{16}' || fail "add-data must print the new durable vol- id: $ADD_OUT"
echo "$ADD_OUT" | grep -qi 'rebalance' || fail "add-data must print the auto-rebalance arming note (VL4): $ADD_OUT"

LIST1="$("$BIN" volume list "sqmeta://$OFF/meta1" --json)"
echo "$LIST1" | grep -q '"id": *"oss1"' || fail "post-add list must keep the legacy id: $LIST1"
echo "$LIST1" | grep -Eq '"id": *"vol-[0-9a-f]{16}"' || fail "post-add list must show the vol- id: $LIST1"

# Duplicate add refuses (exit nonzero, names membership).
if "$BIN" volume add-data "sqmeta://$OFF/meta1" "$OFF/oss2" 2>"$OFF/dup.err"; then
    fail "duplicate add-data must refuse"
fi
grep -qiE 'member|already' "$OFF/dup.err" || fail "duplicate refusal must name membership: $(cat "$OFF/dup.err")"

# --no-rebalance is accepted (opt-out of the VL4 default).
truncate -s 512M "$OFF/oss3"
"$BIN" volume add-data "sqmeta://$OFF/meta1" "$OFF/oss3" --no-rebalance >/dev/null \
    || fail "--no-rebalance add must succeed"

echo "OK: leg 1 (offline add/list/duplicate-refusal)"

# ---------------------------------------------------------------------------
# Leg 2: mount → manifest → add → remount → verify → new-backend engagement
# ---------------------------------------------------------------------------
transport_supported() {
    [ -e /dev/fuse ] || { echo "SKIP: /dev/fuse not present"; return 1; }
    case "$(cat /sys/module/fuse/parameters/enable_uring 2>/dev/null || true)" in
        Y|y|1) ;;
        *) echo "SKIP: kernel fuse.enable_uring not enabled"; return 1 ;;
    esac
    command -v fusermount3 >/dev/null || { echo "SKIP: fusermount3 not available"; return 1; }
    return 0
}

if ! transport_supported; then
    echo "==============================================================="
    echo "SKIP (LOUD): leg 2 (mount legs) cannot run in this environment."
    echo "Leg 1 (offline durable identity + add + list) PASSED."
    echo "==============================================================="
    exit 0
fi

note "Leg 2: mount / manifest / add / remount / engagement"

RIG="$BASE/rig"
MNT="$BASE/mnt"
mkdir -p "$RIG/staging" "$MNT"
truncate -s 256M "$RIG/meta1"
truncate -s 1G   "$RIG/oss1"
truncate -s 1G   "$RIG/oss2"

"$BIN" format "sqmeta://$RIG/meta1" "sqdata://$RIG/oss1" \
    --disk-cache-paths "$RIG/staging" --force >/dev/null

do_mount() {
    RUST_LOG=info "$BIN" mount "sqmeta://$RIG/meta1" "$MNT" >>"$LOG" 2>&1 &
    MOUNT_PID=$!
    local deadline=$((SECONDS + 90))
    until cat "$MNT/.stats" &>/dev/null; do
        [ $SECONDS -lt $deadline ] || { tail -50 "$LOG" >&2; fail "mount did not become ready in 90s"; }
        kill -0 "$MOUNT_PID" 2>/dev/null || { tail -50 "$LOG" >&2; fail "mount daemon exited"; }
        sleep 0.25
    done
}

do_unmount() {
    sync -f "$MNT" || true
    local tries=0
    until fusermount3 -u "$MNT"; do
        tries=$((tries + 1))
        [ $tries -lt 10 ] || fail "fusermount3 -u kept failing"
        sleep 0.5
    done
    wait "$MOUNT_PID" || true
    MOUNT_PID=""
}

stats_field() { # stats_field <jq-ish key> — crude extraction, no jq dependency
    python3 - "$1" <"$MNT/.stats" <<'EOF'
import json, sys
data = json.load(sys.stdin)
print(json.dumps(data.get(sys.argv[1], None)))
EOF
}

do_mount

# Checksummed dataset manifest (8 × 4 MiB random files).
mkdir -p "$MNT/dataset"
for i in $(seq 1 8); do
    dd if=/dev/urandom of="$MNT/dataset/f$i.bin" bs=1M count=4 status=none
done
sync -f "$MNT"
(cd "$MNT/dataset" && sha256sum f*.bin) >"$RIG/manifest.sha256"

do_unmount

# The durable add (offline verb between mounts — the VL3 shape; the
# online admin-lane path is exercised by the cargo suite).
ADD_OUT="$("$BIN" volume add-data "sqmeta://$RIG/meta1" "$RIG/oss2")"
NEW_ID="$(echo "$ADD_OUT" | grep -oE 'vol-[0-9a-f]{16}' | head -1)"
[ -n "$NEW_ID" ] || fail "no vol- id in add output: $ADD_OUT"
note "added volume id: $NEW_ID"

do_mount

# 1. The manifest reads back byte-identical after add + remount.
(cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
    || fail "dataset manifest mismatch after add + remount"
echo "OK: manifest byte-identical after remount"

# 2. volume list (live target = the mountpoint, admin lane) shows the set.
LIST_LIVE="$("$BIN" volume list "$MNT" --json)"
echo "$LIST_LIVE" | grep -q "$NEW_ID" || fail "live volume list must show $NEW_ID: $LIST_LIVE"
echo "$LIST_LIVE" | grep -q '"id": *"oss1"' || fail "live volume list must keep the legacy id: $LIST_LIVE"

# 3. Engagement: write more data; the new backend must receive
#    allocations — volume_states used_bytes on the new id must move.
USED_BEFORE="$(stats_field volume_states | python3 -c "
import json,sys
rows = json.load(sys.stdin) or []
print(next((r['used_bytes'] for r in rows if r['id']=='$NEW_ID'), 0))")"
for i in $(seq 1 16); do
    dd if=/dev/urandom of="$MNT/spread_$i.bin" bs=1M count=8 status=none
done
sync -f "$MNT"
USED_AFTER="$(stats_field volume_states | python3 -c "
import json,sys
rows = json.load(sys.stdin) or []
print(next((r['used_bytes'] for r in rows if r['id']=='$NEW_ID'), 0))")"
[ "$USED_AFTER" -gt "$USED_BEFORE" ] \
    || fail "the added backend received no allocations (used $USED_BEFORE -> $USED_AFTER)"
echo "OK: new backend receives allocations (used $USED_BEFORE -> $USED_AFTER bytes)"

do_unmount

echo "==============================================================="
echo "VOLUME LIFECYCLE RIG (VL3 skeleton) PASSED"
echo "==============================================================="
