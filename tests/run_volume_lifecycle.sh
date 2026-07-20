#!/bin/bash
set -euo pipefail

# Volume-lifecycle rig — PR VL3 skeleton + PR VL4 drain/remove legs
# (docs/design-volume-lifecycle.md §3, gates G-VL-2 + G-VL-3).
# VL5+/VL6+/VL7 add meta migration, fsck, and defrag cycles.
#
# Legs:
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
#   Leg 3 (always, VL4): offline drain surface — preflight refusal with
#     the honest §5.2 numbers (needed/avail/transient/headroom), offline
#     `volume remove-data` of an empty member (drains to `retired`
#     in-process, §5.8 coordinator shape), retired-id permanence, undrain
#     refusal on retired.
#
#   Leg 4 (mount, VL4): drain with a live dataset — reads served
#     MID-DRAIN (G-VL-3 f), convergence to retired, checksummed manifest
#     byte-identical after remount, `evacuate_bytes_moved` engagement.
#
#   Leg 5 (mount, VL4): undrain — a throttled in-flight drain is
#     cancelled, the volume returns to active, the manifest is intact.
#
#   Leg 6 (mount, VL4): clone leg — copy_file_range whole-file clone,
#     drain, shared blocks move ONCE (`evacuate_shared_blocks_moved`),
#     both clones byte-identical (G-VL-3 c).
#
#   Leg 7 (mount, VL4, LOOPS=${LOOPS:-3}): kill-9 injection — start a
#     drain, kill -9 the coordinator daemon at a randomized point
#     (mid-copy/mid-publish/mid-retire sampled by the random delay),
#     remount (the fabric adopts the durable job and re-plans, KD-6),
#     drain converges, manifest byte-identical (G-VL-3 a; the ×10 run is
#     the closing gate, LOOPS=10).
#
# Unprivileged posture (the preload-gate precedent): file-backed volumes
# + a user-owned mountpoint; root is NOT required.
#
# Usage:  tests/run_volume_lifecycle.sh          # all legs, LOOPS=3
#         LOOPS=10 tests/run_volume_lifecycle.sh # the closing-gate soak

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
# Leg 3 (VL4, always): offline drain surface — preflight refusal, empty
# drain to retired, retired-id permanence
# ---------------------------------------------------------------------------
note "Leg 3: offline preflight refusal + empty drain to retired"

DR="$BASE/offdrain"
mkdir -p "$DR/staging"
truncate -s 256M "$DR/meta1"
truncate -s 4G   "$DR/oss1"
truncate -s 4G   "$DR/oss2"
"$BIN" format "sqmeta://$DR/meta1" "sqdata://$DR/oss1,$DR/oss2" \
    --disk-cache-paths "$DR/staging" --force >/dev/null

# 3a. Preflight refusal with the honest numbers: a survivor set whose
# free space is under the 1 GiB headroom floor must refuse and print
# every §5.2 term.
SMALL="$BASE/offdrain_small"
mkdir -p "$SMALL/staging"
truncate -s 256M "$SMALL/meta1"
truncate -s 256M "$SMALL/oss1"
truncate -s 256M "$SMALL/oss2"
"$BIN" format "sqmeta://$SMALL/meta1" "sqdata://$SMALL/oss1,$SMALL/oss2" \
    --disk-cache-paths "$SMALL/staging" --force >/dev/null
if "$BIN" volume remove-data "sqmeta://$SMALL/meta1" oss2 2>"$SMALL/refuse.err"; then
    fail "remove-data into a too-small survivor set must refuse"
fi
for term in needed avail transient headroom; do
    grep -qi "$term" "$SMALL/refuse.err" \
        || fail "preflight refusal must name the '$term' term: $(cat "$SMALL/refuse.err")"
done
LIST_SMALL="$("$BIN" volume list "sqmeta://$SMALL/meta1" --json)"
echo "$LIST_SMALL" | grep -q '"state": *"active"' \
    || fail "a refused remove must leave states untouched: $LIST_SMALL"
echo "OK: leg 3a (honest preflight refusal)"

# 3b. Offline drain of an empty member runs the §5.8 in-process
# coordinator to completion: census 0 ⇒ retired.
"$BIN" volume remove-data "sqmeta://$DR/meta1" oss2 \
    || fail "offline remove-data of an empty member must drain to retired"
LIST_DR="$("$BIN" volume list "sqmeta://$DR/meta1" --json)"
echo "$LIST_DR" | grep -q '"state": *"retired"' \
    || fail "the drained volume must list as retired: $LIST_DR"

# 3c. Retired is terminal: undrain refuses; the id is never reused (a
# re-add of the same device mints a FRESH vol- id).
if "$BIN" volume undrain "sqmeta://$DR/meta1" oss2 2>"$DR/undrain.err"; then
    fail "undrain of a retired volume must refuse"
fi
READD_OUT="$("$BIN" volume add-data "sqmeta://$DR/meta1" "$DR/oss2" --no-rebalance)"
echo "$READD_OUT" | grep -Eq 'vol-[0-9a-f]{16}' \
    || fail "re-adding a retired device must mint a fresh vol- id: $READD_OUT"
LIST_READD="$("$BIN" volume list "sqmeta://$DR/meta1" --json)"
echo "$LIST_READD" | grep -q '"state": *"retired"' \
    || fail "the retired record must be kept forever (KD-5): $LIST_READD"
echo "OK: leg 3 (offline drain surface)"

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
    echo "SKIP (LOUD): legs 2/4/5/6/7 (mount legs) cannot run here."
    echo "Legs 1+3 (offline identity/add/list + drain surface) PASSED."
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

# The guarded offline verbs refuse while any mount-registration heartbeat
# is fresh (the ONE staleness law, CLIENT_STALE_TTL_SECS = 45 s): an
# externally-unmounted daemon's records linger to the TTL, exactly like a
# kill -9'd one. Retry the guarded verb until the records go stale.
retry_guarded() { # retry_guarded <deadline-secs> <cmd...>
    local deadline=$((SECONDS + $1)); shift
    local out
    while true; do
        if out="$("$@" 2>&1)"; then
            echo "$out"
            return 0
        fi
        if ! echo "$out" | grep -q "actively mounted by clients"; then
            echo "$out" >&2
            return 1
        fi
        [ $SECONDS -lt $deadline ] || { echo "$out" >&2; return 1; }
        echo "    (waiting for the unmounted daemon's heartbeat records to go stale...)" >&2
        sleep 5
    done
}

vol_used_bytes() { # vol_used_bytes <volume-id> — from the .stats volume_states rows
    python3 -c '
import json, sys
data = json.load(open(sys.argv[1]))
rows = data.get("volume_states") or []
print(next((r["used_bytes"] for r in rows if r["id"] == sys.argv[2]), 0))
' "$MNT/.stats" "$1"
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
# online admin-lane path is exercised by the cargo suite). Retries
# through the post-unmount heartbeat-staleness window (TTL law).
ADD_OUT="$(retry_guarded 90 "$BIN" volume add-data "sqmeta://$RIG/meta1" "$RIG/oss2")" \
    || fail "volume add-data kept refusing after the staleness TTL"
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
USED_BEFORE="$(vol_used_bytes "$NEW_ID")"
for i in $(seq 1 16); do
    dd if=/dev/urandom of="$MNT/spread_$i.bin" bs=1M count=8 status=none
done
sync -f "$MNT"
USED_AFTER="$(vol_used_bytes "$NEW_ID")"
[ "$USED_AFTER" -gt "$USED_BEFORE" ] \
    || fail "the added backend received no allocations (used $USED_BEFORE -> $USED_AFTER)"
echo "OK: new backend receives allocations (used $USED_BEFORE -> $USED_AFTER bytes)"

do_unmount

# ---------------------------------------------------------------------------
# VL4 mount-leg helpers
# ---------------------------------------------------------------------------
LOOPS="${LOOPS:-3}"

stats_field() { # stats_field <metrics-counter-name> — from the .stats "metrics" object
    python3 -c '
import json, sys
data = json.load(open(sys.argv[1]))
print(int((data.get("metrics") or {}).get(sys.argv[2]) or 0))
' "$MNT/.stats" "$1"
}

vol_state_live() { # vol_state_live <volume-id> — from `volume list <mnt>`
    "$BIN" volume list "$MNT" --json | python3 -c '
import json, sys
rows = json.load(sys.stdin)
print(next((r["state"] for r in rows if r["id"] == sys.argv[1]), "absent"))
' "$1"
}

wait_vol_state_live() { # wait_vol_state_live <volume-id> <state> <deadline-secs>
    local deadline=$((SECONDS + $3))
    while [ "$(vol_state_live "$1")" != "$2" ]; do
        [ $SECONDS -lt $deadline ] \
            || fail "volume $1 did not reach state '$2' in $3s (now: $(vol_state_live "$1"))"
        sleep 0.5
    done
}

# Fresh 2-volume rig for one VL4 mount leg: format + mount + dataset.
# Sets RIG/MNT/LOG; the dataset manifest lands at $RIG/manifest.sha256.
fresh_drain_rig() { # fresh_drain_rig <name> <n-files> <mib-per-file>
    RIG="$BASE/$1"
    MNT="$BASE/$1_mnt"
    LOG="$RIG/mount.log"
    mkdir -p "$RIG/staging" "$MNT"
    truncate -s 256M "$RIG/meta1"
    truncate -s 8G   "$RIG/oss1"
    truncate -s 8G   "$RIG/oss2"
    "$BIN" format "sqmeta://$RIG/meta1" "sqdata://$RIG/oss1,$RIG/oss2" \
        --disk-cache-paths "$RIG/staging" --force >/dev/null
    do_mount
    mkdir -p "$MNT/dataset"
    for i in $(seq 1 "$2"); do
        dd if=/dev/urandom of="$MNT/dataset/f$i.bin" bs=1M count="$3" status=none
    done
    sync -f "$MNT"
    (cd "$MNT/dataset" && sha256sum f*.bin) >"$RIG/manifest.sha256"
}

# ---------------------------------------------------------------------------
# Leg 4 (VL4): live drain — reads mid-drain, retire, manifest, engagement
# ---------------------------------------------------------------------------
note "Leg 4: live drain — mid-drain reads + retire + manifest"

fresh_drain_rig "drain" 8 16

BYTES_BEFORE="$(stats_field evacuate_bytes_moved)"
# Throttled so the draining window is observable for the mid-drain reads.
"$BIN" volume remove-data "$MNT" oss2 --throttle 25 \
    || fail "live remove-data must admit (8G survivor)"
STATE="$(vol_state_live oss2)"
[ "$STATE" = "draining" ] || [ "$STATE" = "retired" ] \
    || fail "remove-data must flip oss2 to draining (got $STATE)"

# G-VL-3 f: reads are served WHILE the volume drains.
MID_READS=0
while [ "$(vol_state_live oss2)" = "draining" ] && [ $MID_READS -lt 3 ]; do
    (cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
        || fail "mid-drain read mismatch (draining must serve reads)"
    MID_READS=$((MID_READS + 1))
done
echo "OK: $MID_READS full-manifest read pass(es) served mid-drain"

# Unthrottle for convergence: rethrottle the evacuation job live.
EVAC_ID="$("$BIN" job list "$MNT" | python3 -c '
import json, sys
rows = json.loads(sys.stdin.read())
live = [r for r in rows if r["state"] in ("queued", "running", "paused")]
print(live[0]["job_id"] if live else "")
')"
[ -n "$EVAC_ID" ] && "$BIN" job throttle "$MNT" "$EVAC_ID" 100 >/dev/null || true

wait_vol_state_live oss2 retired 180
BYTES_AFTER="$(stats_field evacuate_bytes_moved)"
[ "$BYTES_AFTER" -gt "$BYTES_BEFORE" ] \
    || fail "engagement: evacuate_bytes_moved did not move ($BYTES_BEFORE -> $BYTES_AFTER)"
(cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
    || fail "post-drain manifest mismatch"
do_unmount

# Remount on the survivor set: manifest byte-identical, retired preserved.
do_mount
(cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
    || fail "post-drain post-remount manifest mismatch"
[ "$(vol_state_live oss2)" = "retired" ] || fail "retired state must survive remount"
do_unmount
echo "OK: leg 4 (live drain + mid-drain reads + manifest + engagement)"

# ---------------------------------------------------------------------------
# Leg 5 (VL4): undrain — cancel an in-flight drain, volume back to active
# ---------------------------------------------------------------------------
note "Leg 5: undrain"

fresh_drain_rig "undrain" 6 16
"$BIN" volume remove-data "$MNT" oss2 --throttle 10 \
    || fail "live remove-data must admit"
[ "$(vol_state_live oss2)" = "draining" ] || fail "oss2 must be draining"
"$BIN" volume undrain "$MNT" oss2 || fail "undrain of a draining volume must succeed"
[ "$(vol_state_live oss2)" = "active" ] || fail "undrain must restore active"
(cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
    || fail "post-undrain manifest mismatch"
do_unmount
echo "OK: leg 5 (undrain)"

# ---------------------------------------------------------------------------
# Leg 6 (VL4): clone leg — shared blocks move once, both clones intact
# ---------------------------------------------------------------------------
note "Leg 6: clone drain (shared blocks move once)"

fresh_drain_rig "clone" 2 16
# Whole-file clone through copy_file_range (block-sharing refcount path).
python3 - "$MNT/dataset/f1.bin" "$MNT/dataset/f1_clone.bin" <<'EOF'
import os, sys
src = os.open(sys.argv[1], os.O_RDONLY)
size = os.fstat(src).st_size
dst = os.open(sys.argv[2], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
copied = 0
while copied < size:
    n = os.copy_file_range(src, dst, size - copied, copied, copied)
    if n <= 0:
        raise SystemExit(f"copy_file_range stalled at {copied}/{size}")
    copied += n
os.close(src); os.close(dst)
EOF
sync -f "$MNT"
(cd "$MNT/dataset" && sha256sum f1.bin f1_clone.bin) >"$RIG/clones.sha256"

SHARED_BEFORE="$(stats_field evacuate_shared_blocks_moved)"
"$BIN" volume remove-data "$MNT" oss2 || fail "clone-leg remove-data must admit"
wait_vol_state_live oss2 retired 180
SHARED_AFTER="$(stats_field evacuate_shared_blocks_moved)"
[ "$SHARED_AFTER" -gt "$SHARED_BEFORE" ] \
    || fail "clone leg: evacuate_shared_blocks_moved did not move ($SHARED_BEFORE -> $SHARED_AFTER)"
(cd "$MNT/dataset" && sha256sum -c "$RIG/clones.sha256" --quiet) \
    || fail "clone leg: clones not byte-identical after the drain"
(cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
    || fail "clone leg: dataset manifest mismatch"
do_unmount
echo "OK: leg 6 (clone move-once, shared delta $SHARED_BEFORE -> $SHARED_AFTER)"

# ---------------------------------------------------------------------------
# Leg 7 (VL4): kill-9 injection soak — LOOPS randomized points (G-VL-3 a)
# ---------------------------------------------------------------------------
note "Leg 7: kill-9 injection soak (LOOPS=$LOOPS)"

for loop in $(seq 1 "$LOOPS"); do
    note "  kill-9 loop $loop/$LOOPS"
    fresh_drain_rig "kill9_$loop" 6 16

    "$BIN" volume remove-data "$MNT" oss2 --throttle 50 \
        || fail "kill-9 loop $loop: remove-data must admit"

    # Randomized injection point: mid-copy / mid-publish / mid-retire are
    # sampled by the delay (0–3 s across a multi-second drain).
    DELAY="0.$((RANDOM % 9))"
    [ $((RANDOM % 3)) -eq 0 ] && DELAY=$((RANDOM % 3))
    sleep "$DELAY"
    kill -9 "$MOUNT_PID" || true
    wait "$MOUNT_PID" 2>/dev/null || true
    MOUNT_PID=""
    fusermount3 -uz "$MNT" 2>/dev/null || true
    echo "    killed coordinator after ${DELAY}s"

    # Remount: the fabric adopts the durable evacuation job (KD-6
    # re-plan) and the drain converges without operator input.
    do_mount
    wait_vol_state_live oss2 retired 240
    (cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
        || fail "kill-9 loop $loop: manifest mismatch after resume (ZERO-LOSS violated)"
    do_unmount
    rm -rf "$RIG" "$MNT"
    echo "    OK: loop $loop converged, manifest byte-identical"
done
echo "OK: leg 7 (kill-9 soak ×$LOOPS)"

echo "==============================================================="
echo "VOLUME LIFECYCLE RIG (VL3 + VL4 legs) PASSED (kill-9 LOOPS=$LOOPS)"
echo "==============================================================="
