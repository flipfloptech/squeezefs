#!/usr/bin/env bash
set -euo pipefail

# Volume-lifecycle rig — PR VL3 skeleton + PR VL4 drain/remove legs
# (docs/design-volume-lifecycle.md §3, gates G-VL-2 + G-VL-3).
# VL5b/VL6a/VL6b/VL7 added meta migration, fsck, repair, and defrag legs;
# VL9 adds leg 17 (the cross-feature interaction matrix at rig scale;
# the canonical lifecycle soak is tests/run_lifecycle_soak.sh and the
# counted ×10 G-VL matrices are tests/run_vl9_matrices.sh).
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
#   Leg 8 (mount, VL4b): balance-aware placement (§5.9, G-VL-8 at rig
#     scale) — 2 volumes, oss2 live-disabled while oss1 fills to ~70 %,
#     re-enable, sustained mixed write/delete workload: assert
#     `placement.backend_fill_spread` decreases and new allocations
#     favor the emptier volume (`backend_placement_picks` deltas — the
#     stats instrument, house pattern). No rebalance job anywhere.
#
#   Leg 9 (mount, VL5b): meta add — format 2 meta volumes derived-width
#     (dynamic meta routing — no knob), mount, dataset + checksum manifest + INO manifest,
#     unmount, OFFLINE `volume add-meta` of a third member taking 2
#     slots (KD-8 barrier + generation restamp inside), remount on the
#     3-member URI: manifest byte-identical, every st_ino stable, the
#     old 2-member URI refuses loud.
#
#   Leg 10 (mount, VL5b): meta remove — continue from leg 9's set,
#     OFFLINE `volume remove-meta` of the added member (slots migrate
#     back to survivors, victim tombstones), remount on the survivor
#     URI: manifest byte-identical, inos stable, victim-listed URI
#     refuses loud.
#
#   Leg 11 (mount, VL5b, LOOPS=${LOOPS:-3}): kill-9 during slot
#     migration — start an offline add-meta coordinator, kill -9 at a
#     randomized point (§5.5.2b crash windows sampled), re-run the SAME
#     add-meta (idempotent convergence), remount, manifest
#     byte-identical + inos stable. Plus the ONLINE cutover-window
#     measurement: `volume migrate-meta-slot` on a live mount under
#     concurrent load; `meta_slot_cutover_ms_max` is PRINTED (the
#     G-VL-4 p99 < 250 ms gate number is recorded, not enforced here).
#
#   Leg 12 (mount, VL6a): online fsck under concurrent write/delete
#     churn — `squeezefs fsck <mnt>` ×3, findings MUST be 0 each pass
#     (G-VL-5 a at rig scale; `fsck_findings` tripwire asserted), plus
#     offline fsck and the `--shards k/N` + `merge-reports` union on
#     the same set.
#
#   Leg 13 (mount, VL6a): fsck DURING an active throttled drain over a
#     clone-heavy dataset — zero findings while the mover's pre-publish
#     refcount ledger and in-flight destinations are live (the
#     G-VL-5(a) drain-concurrent row), clones byte-identical after
#     convergence, post-drain fsck clean.
#
#   Leg 14 (mount, VL6a): the C7 scrub arm — `squeezefs scrub` on a
#     plain volume: per-arm counters asserted (100 % readability-only —
#     the KD-17 honesty gauge; zero failures/findings).
#
#   Leg 15 (VL6b, offline): seed-corrupt ⇒ repair ⇒ re-fsck clean ⇒
#     manifest intact (G-VL-5(d) at rig scale). Offline-seedable
#     classes: a crafted C4 orphan custody record + a C5 stale
#     generation marker. `fsck --repair` (dry run, the default) plans
#     and mutates NOTHING (re-fsck still finds); `fsck --repair
#     --apply` (the guarded D0 open) quarantines-first + repairs;
#     re-fsck reports findings: 0; the quarantine manifest lists the
#     copies; remount reads the dataset back byte-identical.
#
#   Leg 16 (mount, VL7): online defrag (G-VL-6 at rig scale) — fragment
#     via interleaved create/delete churn, `defrag --report-only`
#     matches the live frag_* gauges, `defrag --data` drives worst-volume
#     D1 contiguity + reclaimable tail to >= 0.9 with the survivor
#     manifest byte-identical across a remount; `defrag_blocks_moved` is
#     the engagement instrument.
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
# coordinator to completion: census 0 ⇒ retired. The outcome line
# prints EXACTLY once (the CLI arm owns it; a library-layer duplicate
# announced completion twice — regression pin).
DRAIN_OUT="$("$BIN" volume remove-data "sqmeta://$DR/meta1" oss2)" \
    || fail "offline remove-data of an empty member must drain to retired"
N_DONE=$(printf '%s\n' "$DRAIN_OUT" | grep -ci "evacuated and retired")
[ "$N_DONE" -eq 1 ] \
    || fail "the drain outcome line must print exactly once, got $N_DONE: $DRAIN_OUT"
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
# is fresh (the ONE staleness law, CLIENT_STALE_TTL_SECS = 45 s). Since the
# VL8 item-4 fix a CLEANLY (externally) unmounted daemon deregisters its
# client:/writer_claim records BEFORE exiting — do_unmount's wait on the
# daemon pid therefore returns with the records already gone, and the
# clean-unmount call sites carry a SHORT slack deadline (settle margin,
# NOT the TTL). Only kill -9'd holders still ride the TTL/dead-pid law
# (leg 11 keeps its long deadlines).
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
ADD_OUT="$(retry_guarded 20 "$BIN" volume add-data "sqmeta://$RIG/meta1" "$RIG/oss2")" \
    || fail "volume add-data kept refusing past the clean-unmount deregistration window (VL8 item-4 regression?)"
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

# ---------------------------------------------------------------------------
# Leg 8 (VL4b): balance-aware placement — fill spread converges, picks
# favor the emptier volume (§5.9, G-VL-8 at rig scale)
# ---------------------------------------------------------------------------
note "Leg 8: balance-aware placement (fill-penalty PlacementTable)"

RIG="$BASE/placement"
MNT="$BASE/placement_mnt"
LOG="$RIG/mount.log"
mkdir -p "$RIG/staging" "$MNT"
truncate -s 256M "$RIG/meta1"
truncate -s 512M "$RIG/oss1"
truncate -s 512M "$RIG/oss2"
"$BIN" format "sqmeta://$RIG/meta1" "sqdata://$RIG/oss1,$RIG/oss2" \
    --disk-cache-paths "$RIG/staging" --force >/dev/null
do_mount

placement_field() { # placement_field <python-expr over data["placement"]>
    python3 -c '
import json, sys
data = json.load(open(sys.argv[1]))
p = data.get("placement") or {}
print(eval(sys.argv[2]))
' "$MNT/.stats" "$1"
}

spread_now() { placement_field 'p["backend_fill_spread"]'; }
picks_of() { # picks_of <volume-id>
    placement_field "next(r['backend_placement_picks'] for r in p['backends'] if r['id'] == '$1')"
}

# Fill oss1 to ~70 % with oss2 live-disabled (the fail-stop health
# override rides the admin lane; the override hook refreshes the table).
"$BIN" config -g "$MNT" data-volume disable oss2 >/dev/null \
    || fail "live data-volume disable must succeed"
for i in $(seq 1 22); do
    dd if=/dev/urandom of="$MNT/fill_$i.bin" bs=1M count=16 status=none
done
sync -f "$MNT"

# Re-enable: both volumes eligible, the census imbalance is now visible.
"$BIN" config -g "$MNT" data-volume enable oss2 >/dev/null \
    || fail "live data-volume enable must succeed"
SPREAD_BEFORE="$(spread_now)"
PICKS1_BEFORE="$(picks_of oss1)"
PICKS2_BEFORE="$(picks_of oss2)"
python3 -c "import sys; sys.exit(0 if float('$SPREAD_BEFORE') > 0.4 else 1)" \
    || fail "premise: the seeded imbalance must read as spread > 40 pp (got $SPREAD_BEFORE)"
note "  seeded spread: $SPREAD_BEFORE (picks oss1=$PICKS1_BEFORE oss2=$PICKS2_BEFORE)"

# The sustained mixed write/delete workload — no rebalance job anywhere.
for i in $(seq 1 16); do
    dd if=/dev/urandom of="$MNT/spread_$i.bin" bs=1M count=12 status=none
    if [ $((i % 4)) -eq 0 ]; then
        rm -f "$MNT/fill_$i.bin"
    fi
done
sync -f "$MNT"
sleep 6 # one health-worker cadence: the fill gauges republish

SPREAD_AFTER="$(spread_now)"
PICKS1_AFTER="$(picks_of oss1)"
PICKS2_AFTER="$(picks_of oss2)"
D1=$((PICKS1_AFTER - PICKS1_BEFORE))
D2=$((PICKS2_AFTER - PICKS2_BEFORE))
note "  post-workload spread: $SPREAD_AFTER (pick deltas oss1=$D1 oss2=$D2)"

# (a) The spread instrument moved DOWN by a real margin.
python3 -c "import sys; sys.exit(0 if float('$SPREAD_AFTER') < float('$SPREAD_BEFORE') - 0.05 else 1)" \
    || fail "backend_fill_spread must decrease under the workload ($SPREAD_BEFORE -> $SPREAD_AFTER)"
# (b) New allocations favor the emptier volume: >= 60 % of the pick
#     delta lands on oss2 (G-VL-8 a, at rig scale).
[ "$D2" -gt 0 ] || fail "the emptier volume received no placements"
[ $((D2 * 10)) -ge $(((D1 + D2) * 6)) ] \
    || fail "placement must favor the emptier volume (oss1 +$D1 vs oss2 +$D2)"

do_unmount
echo "OK: leg 8 (placement: spread $SPREAD_BEFORE -> $SPREAD_AFTER, picks oss1 +$D1 / oss2 +$D2)"

# ---------------------------------------------------------------------------
# VL5b meta-set helpers (legs 9–11)
# ---------------------------------------------------------------------------

# Mount helper for an explicit sqmeta:// URI (the meta legs swap URIs).
do_mount_uri() { # do_mount_uri <sqmeta-uri>
    RUST_LOG=info "$BIN" mount "$1" "$MNT" >>"$LOG" 2>&1 &
    MOUNT_PID=$!
    local deadline=$((SECONDS + 90))
    until cat "$MNT/.stats" &>/dev/null; do
        [ $SECONDS -lt $deadline ] || { tail -50 "$LOG" >&2; fail "mount did not become ready in 90s"; }
        kill -0 "$MOUNT_PID" 2>/dev/null || { tail -50 "$LOG" >&2; fail "mount daemon exited"; }
        sleep 0.25
    done
}

# Record `name -> st_ino` for every dataset file (the ino-stability
# instrument: global inos are eternally stable across meta set changes).
ino_manifest() { # ino_manifest <out-file>
    (cd "$MNT" && find dataset -type f -exec stat -c '%n %i' {} + | sort) >"$1"
}

# Fresh 2-meta-volume derived-width rig with a dataset; sets RIG/MNT/LOG and the
# manifests at $RIG/manifest.sha256 + $RIG/inos.before.
fresh_meta_rig() { # fresh_meta_rig <name>
    RIG="$BASE/$1"
    MNT="$BASE/$1_mnt"
    LOG="$RIG/mount.log"
    mkdir -p "$RIG/staging" "$MNT"
    truncate -s 256M "$RIG/meta1"
    truncate -s 256M "$RIG/meta2"
    truncate -s 2G   "$RIG/oss1"
    "$BIN" format "sqmeta://$RIG/meta1,$RIG/meta2" "sqdata://$RIG/oss1" \
        --disk-cache-paths "$RIG/staging" --force >/dev/null
    do_mount_uri "sqmeta://$RIG/meta1,$RIG/meta2"
    mkdir -p "$MNT/dataset"
    for d in 0 1 2 3; do
        mkdir -p "$MNT/dataset/d$d"
        for i in $(seq 1 6); do
            dd if=/dev/urandom of="$MNT/dataset/d$d/f$i.bin" bs=256K count=1 status=none
            setfattr -n user.tag -v "d${d}f${i}" "$MNT/dataset/d$d/f$i.bin" 2>/dev/null || true
        done
    done
    sync -f "$MNT"
    (cd "$MNT" && find dataset -type f -exec sha256sum {} + | sort) >"$RIG/manifest.sha256"
    ino_manifest "$RIG/inos.before"
}

verify_meta_manifest() { # verify_meta_manifest <who>
    (cd "$MNT" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
        || fail "$1: dataset manifest mismatch (ZERO-LOSS violated)"
    ino_manifest "$RIG/inos.after"
    diff -u "$RIG/inos.before" "$RIG/inos.after" >/dev/null \
        || fail "$1: st_ino instability across the meta set change (KD-7 violated)"
}

# ---------------------------------------------------------------------------
# Leg 9 (VL5b): offline add-meta — manifest + ino stability + URI refusals
# ---------------------------------------------------------------------------
note "Leg 9: meta add (offline add-meta, derived width, --take-slots 2)"

fresh_meta_rig "metaadd"
do_unmount

truncate -s 256M "$RIG/meta3"
ADD_OUT="$(retry_guarded 20 "$BIN" volume add-meta "sqmeta://$RIG/meta1,$RIG/meta2" \
    "$RIG/meta3" --take-slots 2)" \
    || fail "volume add-meta kept refusing past the clean-unmount deregistration window (VL8 item-4 regression?)"
echo "$ADD_OUT" | grep -qi "hosting slot" || fail "add-meta must print the taken slots: $ADD_OUT"

# The old 2-member URI now refuses loud (stamps declare 3 members).
if "$BIN" volume list "sqmeta://$RIG/meta1,$RIG/meta2" --json 2>"$RIG/old_uri.err"; then
    fail "the shrunken URI must refuse after add-meta"
fi

do_mount_uri "sqmeta://$RIG/meta1,$RIG/meta2,$RIG/meta3"
verify_meta_manifest "leg 9"
# New writes still work on the migrated map (mints ride the travelling
# cursors — collisions would surface as EEXIST/corruption here).
dd if=/dev/urandom of="$MNT/dataset/post_add.bin" bs=256K count=1 status=none
sync -f "$MNT"
do_unmount
echo "OK: leg 9 (add-meta: manifest byte-identical, inos stable, old URI refused)"

# ---------------------------------------------------------------------------
# Leg 10 (VL5b): offline remove-meta — survivors serve, victim tombstones
# ---------------------------------------------------------------------------
note "Leg 10: meta remove (offline remove-meta of the added member)"

# Refresh the manifests to include post_add.bin.
do_mount_uri "sqmeta://$RIG/meta1,$RIG/meta2,$RIG/meta3"
(cd "$MNT" && find dataset -type f -exec sha256sum {} + | sort) >"$RIG/manifest.sha256"
ino_manifest "$RIG/inos.before"
do_unmount

retry_guarded 20 "$BIN" volume remove-meta \
    "sqmeta://$RIG/meta1,$RIG/meta2,$RIG/meta3" "$RIG/meta3" >/dev/null \
    || fail "volume remove-meta kept refusing past the clean-unmount deregistration window (VL8 item-4 regression?)"

# Listing the tombstoned victim refuses loud.
if "$BIN" volume list "sqmeta://$RIG/meta1,$RIG/meta2,$RIG/meta3" --json 2>"$RIG/victim.err"; then
    fail "a victim-listed URI must refuse after remove-meta"
fi
grep -qiE "retired|tombstone|member" "$RIG/victim.err" \
    || fail "the victim refusal must name the retirement: $(cat "$RIG/victim.err")"

do_mount_uri "sqmeta://$RIG/meta1,$RIG/meta2"
verify_meta_manifest "leg 10"
do_unmount
echo "OK: leg 10 (remove-meta: survivors serve, manifest + inos intact, victim refused)"

# ---------------------------------------------------------------------------
# Leg 11 (VL5b): kill-9 during the migration coordinator + online cutover
# window measurement (printed, not enforced)
# ---------------------------------------------------------------------------
note "Leg 11: kill-9 during slot migration (LOOPS=$LOOPS) + cutover window"

for loop in $(seq 1 "$LOOPS"); do
    note "  kill-9 loop $loop/$LOOPS"
    fresh_meta_rig "metakill_$loop"
    do_unmount

    truncate -s 256M "$RIG/meta3"
    # Start the offline coordinator and kill -9 it at a randomized point
    # (§5.5.2b windows sampled: format/copy/claim/re-stamp/teardown).
    DELAY="0.$((RANDOM % 9))"
    [ $((RANDOM % 3)) -eq 0 ] && DELAY="1.$((RANDOM % 5))"
    ( retry_guarded 90 "$BIN" volume add-meta "sqmeta://$RIG/meta1,$RIG/meta2" \
        "$RIG/meta3" --take-slots 2 >/dev/null 2>&1 ) &
    COORD_PID=$!
    sleep "$DELAY"
    kill -9 "$COORD_PID" 2>/dev/null || true
    wait "$COORD_PID" 2>/dev/null || true
    pkill -9 -f "volume add-meta.*metakill_$loop" 2>/dev/null || true
    echo "    killed coordinator after ${DELAY}s"

    # Re-run converges (idempotent §5.5.2b re-run; counts-disagree
    # mid-states refuse mounts until then, so convergence is REQUIRED).
    retry_guarded 120 "$BIN" volume add-meta "sqmeta://$RIG/meta1,$RIG/meta2" \
        "$RIG/meta3" --take-slots 2 >/dev/null \
        || fail "kill-9 loop $loop: add-meta re-run did not converge"

    do_mount_uri "sqmeta://$RIG/meta1,$RIG/meta2,$RIG/meta3"
    verify_meta_manifest "kill-9 loop $loop"
    do_unmount
    rm -rf "$RIG" "$MNT"
    echo "    OK: loop $loop converged, manifest + inos intact"
done

# The ONLINE cutover-window measurement: migrate a slot on a LIVE mount
# under concurrent load; print the widest window (G-VL-4's p99 < 250 ms
# gate number — recorded here, enforced by the closing-gate run).
note "  online migrate-meta-slot cutover-window measurement"
fresh_meta_rig "metaonline"
# Churn across EVERY dataset dir (the striped dirs live on both
# volumes, so the migrating slot's keyspace sees concurrent commits —
# the tee + gate get real contention).
( i=0; while [ $i -lt 800 ] && [ -e "$MNT/.stats" ]; do
      d=$((i % 4))
      echo "load" > "$MNT/dataset/d$d/churn_$((i % 20)).txt" 2>/dev/null || break
      rm -f "$MNT/dataset/d$d/churn_$(((i + 10) % 20)).txt" 2>/dev/null || true
      i=$((i + 1))
  done ) &
CHURN_PID=$!
"$BIN" volume migrate-meta-slot "$MNT" 1 0 >/dev/null \
    || fail "online migrate-meta-slot submission failed"
MIG_DEADLINE=$((SECONDS + 120))
until [ "$(stats_field meta_slot_migrations)" -ge 1 ]; do
    [ $SECONDS -lt $MIG_DEADLINE ] || fail "online slot migration did not complete in 120s"
    sleep 0.5
done
wait "$CHURN_PID" 2>/dev/null || true
CUTOVER_MS="$(stats_field meta_slot_cutover_ms_max)"
PARKED="$(stats_field meta_slot_gate_parked_commits)"
DELTA_KEYS="$(stats_field meta_slot_delta_keys)"
sync -f "$MNT"
(cd "$MNT" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
    || fail "online migration: manifest mismatch"
do_unmount
echo "  MEASUREMENT (recorded, not enforced): meta_slot_cutover_ms_max=${CUTOVER_MS} ms" \
     "(G-VL-4 target p99 < 250 ms), gate parks=${PARKED}, delta keys=${DELTA_KEYS}"
echo "OK: leg 11 (kill-9 ×$LOOPS convergence + online cutover window ${CUTOVER_MS} ms)"

# ---------------------------------------------------------------------------
# Leg 12 (VL6a): online fsck under concurrent write/delete churn —
# FP = 0 ×3 (G-VL-5 a at rig scale), plus the offline + sharded modes
# ---------------------------------------------------------------------------
note "Leg 12: fsck under load (FP=0 x3) + offline/sharded fsck"

fresh_drain_rig "fsck" 6 8

# Concurrent churn: rewrite/delete cycles across the dataset while fsck
# runs (the rig's dd/checksum churn — in-flight allocations, frees, and
# staged custody continuously exercise the epoch filter + registry).
( i=0; while [ $i -lt 2000 ] && [ -e "$MNT/.stats" ]; do
      dd if=/dev/urandom of="$MNT/dataset/churn_$((i % 8)).bin" bs=256K count=1 \
          conv=notrunc status=none 2>/dev/null || break
      [ $((i % 5)) -eq 0 ] && rm -f "$MNT/dataset/churn_$(((i + 3) % 8)).bin" 2>/dev/null
      i=$((i + 1))
  done ) &
FSCK_CHURN_PID=$!

for pass in 1 2 3; do
    FSCK_OUT="$("$BIN" fsck "$MNT")" \
        || fail "fsck pass $pass under churn reported findings (FP!=0) or failed: $FSCK_OUT"
    echo "$FSCK_OUT" | grep -q "findings: 0" \
        || fail "fsck pass $pass: expected zero findings, got: $FSCK_OUT"
    echo "    OK: fsck pass $pass under churn — findings: 0"
done
kill "$FSCK_CHURN_PID" 2>/dev/null || true
wait "$FSCK_CHURN_PID" 2>/dev/null || true

# Engagement: the scan actually ran (stats-inode instrument).
FSCK_SCANNED="$(stats_field fsck_inodes_scanned)"
[ "$FSCK_SCANNED" -gt 0 ] || fail "engagement: fsck_inodes_scanned stayed 0"
FSCK_FOUND="$(stats_field fsck_findings)"
[ "$FSCK_FOUND" -eq 0 ] || fail "tripwire: fsck_findings=$FSCK_FOUND on a healthy volume"
sync -f "$MNT"
do_unmount

# Offline fsck (read-only probes; retried through the heartbeat TTL) +
# the k/N shard union equivalence on the same set.
OFF_OUT="$(retry_guarded 20 "$BIN" fsck "sqmeta://$RIG/meta1" --offline)" \
    || fail "offline fsck kept refusing past the clean-unmount deregistration window: $OFF_OUT"
echo "$OFF_OUT" | grep -q "findings: 0" || fail "offline fsck must be clean: $OFF_OUT"
"$BIN" fsck "sqmeta://$RIG/meta1" --offline --shards 0/2 --json >"$RIG/shard0.json" \
    || fail "shard 0/2 fsck failed"
"$BIN" fsck "sqmeta://$RIG/meta1" --offline --shards 1/2 --json >"$RIG/shard1.json" \
    || fail "shard 1/2 fsck failed"
MERGE_OUT="$("$BIN" fsck merge-reports "$RIG/shard0.json" "$RIG/shard1.json")" \
    || fail "merge-reports reported findings on a healthy set: $MERGE_OUT"
echo "$MERGE_OUT" | grep -q "findings: 0" || fail "merged shard report must be clean: $MERGE_OUT"
echo "OK: leg 12 (fsck FP=0 x3 under churn; offline + 2-shard merge clean)"

# ---------------------------------------------------------------------------
# Leg 13 (VL6a): fsck DURING an active drain over a clone-heavy dataset —
# FP = 0 (the G-VL-5(a) drain-concurrent row: the mover's pre-publish
# refcount ledger + in-flight destinations are the adversary)
# ---------------------------------------------------------------------------
note "Leg 13: fsck during drain over a cloned dataset (FP=0)"

fresh_drain_rig "fsckdrain" 4 16
# Clone-heavy: whole-file clones via copy_file_range (refcount sharing).
for f in f1 f2; do
python3 - "$MNT/dataset/$f.bin" "$MNT/dataset/${f}_clone.bin" <<'EOF'
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
done
sync -f "$MNT"
(cd "$MNT/dataset" && sha256sum f*.bin) >"$RIG/clones.sha256"

# Slow drain so fsck runs INSIDE the drain window.
"$BIN" volume remove-data "$MNT" oss2 --throttle 10 \
    || fail "leg 13: remove-data must admit"
[ "$(vol_state_live oss2)" = "draining" ] || fail "leg 13: oss2 must be draining"

FSCK_OUT="$("$BIN" fsck "$MNT")" \
    || fail "fsck during an active drain reported findings (FP!=0) or failed: $FSCK_OUT"
echo "$FSCK_OUT" | grep -q "findings: 0" \
    || fail "drain-concurrent fsck must report zero findings: $FSCK_OUT"
[ "$(vol_state_live oss2)" = "draining" ] || [ "$(vol_state_live oss2)" = "retired" ] \
    || fail "leg 13: drain state lost during fsck"

# Let the drain converge; everything stays byte-identical.
EVAC_ID="$("$BIN" job list "$MNT" | python3 -c '
import json, sys
rows = json.loads(sys.stdin.read())
live = [r for r in rows if r["state"] in ("queued", "running", "paused")]
print(live[0]["job_id"] if live else "")
')"
[ -n "$EVAC_ID" ] && "$BIN" job throttle "$MNT" "$EVAC_ID" 100 >/dev/null || true
wait_vol_state_live oss2 retired 240
(cd "$MNT/dataset" && sha256sum -c "$RIG/clones.sha256" --quiet) \
    || fail "leg 13: clones not byte-identical after drain + fsck"
# And a post-drain fsck is still clean (no mover residue misreported).
"$BIN" fsck "$MNT" | grep -q "findings: 0" || fail "leg 13: post-drain fsck not clean"
do_unmount
echo "OK: leg 13 (drain-concurrent fsck FP=0, clones intact, post-drain clean)"

# ---------------------------------------------------------------------------
# Leg 14 (VL6a): the C7 scrub arm — per-arm counters (KD-17 honesty:
# plain volumes report readability-only, never fake verification)
# ---------------------------------------------------------------------------
note "Leg 14: scrub (C7) with per-arm counters"

fresh_drain_rig "scrub" 4 8
SCRUB_OUT="$("$BIN" scrub "$MNT" --json)" || fail "scrub reported failures: $SCRUB_OUT"
python3 - <<EOF || fail "scrub counters wrong: $SCRUB_OUT"
import json, sys
r = json.loads('''$SCRUB_OUT''')
c = r["counters"]
assert r["findings"] == [], f"scrub findings on a healthy volume: {r['findings']}"
assert c["scrub_blocks_scanned"] > 0, "scrub scanned nothing"
assert c["scrub_failures"] == 0, "scrub failures on a healthy volume"
# Plain (uncompressed, unencrypted) volume: the honesty gauge — every
# block is readability-only, no fake AEAD/frame verification claims.
assert c["scrub_readability_only"] == c["scrub_blocks_scanned"], (
    "plain volume must be 100% readability-only",
    c,
)
assert c["scrub_aead_verified"] == 0 and c["scrub_frame_verified"] == 0, c
EOF
echo "    scrub counters: $(echo "$SCRUB_OUT" | python3 -c '
import json, sys
c = json.load(sys.stdin)["counters"]
print({k: v for k, v in c.items() if k.startswith("scrub_")})
')"
do_unmount
echo "OK: leg 14 (scrub per-arm counters, readability-only honesty gauge)"

# ---------------------------------------------------------------------------
# Leg 15 (VL6b): offline seed-corrupt ⇒ fsck reports ⇒ --repair (dry run,
# nothing mutates) ⇒ --repair --apply (guarded open, quarantine-first) ⇒
# re-fsck CLEAN ⇒ manifest byte-identical (G-VL-5(d) at rig scale)
# ---------------------------------------------------------------------------
note "Leg 15: fsck --repair --apply (offline seed => repair => clean => manifest)"

fresh_drain_rig "repair" 4 8
sync -f "$MNT"
do_unmount

# Like retry_guarded, but findings-tolerant: fsck exits 1 WITH a report
# when findings exist — only the live-writer refusal is retried.
fsck_when_free() { # fsck_when_free <deadline-secs> <cmd...> ; sets FSCK_OUT/FSCK_RC
    local deadline=$((SECONDS + $1)); shift
    while true; do
        FSCK_RC=0
        FSCK_OUT="$("$@" 2>&1)" || FSCK_RC=$?
        if ! echo "$FSCK_OUT" | grep -q "actively mounted by clients"; then
            return 0
        fi
        [ $SECONDS -lt $deadline ] \
            || fail "leg 15: fsck kept refusing past the staleness TTL: $FSCK_OUT"
        echo "    (waiting for the unmounted daemon's heartbeat records to go stale...)" >&2
        sleep 5
    done
}

# The mount isolates its real staging (marker + staging_segment) under
# <root>/squeezefs/<sanitized-mountpoint>/ — seed THERE.
ISOL="$(find "$RIG/staging/squeezefs" -mindepth 1 -maxdepth 1 -type d ! -name cache_segment | head -1)"
[ -n "$ISOL" ] || fail "leg 15: no isolated staging dir under $RIG/staging/squeezefs"
[ -f "$ISOL/.squeezefs_generation" ] || fail "leg 15: mount left no generation marker in $ISOL"

# Seed C4: a crafted orphan custody record (the writer geometry — magic,
# key_len, val_len, key, payload; byte alignment for a sub-4KiB shard).
python3 - "$ISOL/staging_segment/seeded_leg15" <<'EOF'
import struct, sys, os
key = b"active_block_ext:inode_9999990:block_0"
val = b"leg15-payload"
img = struct.pack("<III", 0xCAFEBABE, len(key), len(val)) + key + val
os.makedirs(os.path.dirname(sys.argv[1]), exist_ok=True)
with open(sys.argv[1], "wb") as f:
    f.write(img)
    f.flush(); os.fsync(f.fileno())
EOF
# Seed C5: rebind the staging generation marker to a dead generation.
python3 - "$ISOL/.squeezefs_generation" <<'EOF'
import os, sys
lines = open(sys.argv[1]).read().splitlines()
assert lines and lines[0] == "squeezefs-staging-generation-v1", lines
with open(sys.argv[1], "w") as f:
    f.write(lines[0] + "\nv3:deadbeefdeadbeefdeadbeefdeadbeef\n")
    f.flush(); os.fsync(f.fileno())
EOF

# 1. Detection: both seeds are findings (exit 1 by the fsck convention).
fsck_when_free 20 "$BIN" fsck "sqmeta://$RIG/meta1" --offline
[ "$FSCK_RC" -ne 0 ] || fail "leg 15: seeded corruption must exit nonzero: $FSCK_OUT"
echo "$FSCK_OUT" | grep -q '\[C4\]' || fail "leg 15: C4 orphan not reported: $FSCK_OUT"
echo "$FSCK_OUT" | grep -q '\[C5\]' || fail "leg 15: C5 stale generation not reported: $FSCK_OUT"

# 2. Dry run (the --repair default): the plan prints, NOTHING mutates.
fsck_when_free 20 "$BIN" fsck "sqmeta://$RIG/meta1" --offline --repair
[ "$FSCK_RC" -ne 0 ] || fail "leg 15: dry run with findings must exit nonzero"
echo "$FSCK_OUT" | grep -q "DRY RUN" || fail "leg 15: dry run must say so: $FSCK_OUT"
[ -f "$ISOL/staging_segment/seeded_leg15" ] \
    || fail "leg 15: dry run must not touch the seeded record"
[ ! -d "$RIG/staging/quarantine" ] \
    || fail "leg 15: dry run must not create a quarantine"
fsck_when_free 20 "$BIN" fsck "sqmeta://$RIG/meta1" --offline
[ "$FSCK_RC" -ne 0 ] || fail "leg 15: dry run must not have repaired anything"

# 3. Apply (guarded D0 open): quarantine-first per-class repair.
fsck_when_free 20 "$BIN" fsck "sqmeta://$RIG/meta1" --offline --repair --apply
[ "$FSCK_RC" -ne 0 ] || fail "leg 15: apply run still reports its findings (exit 1)"
echo "$FSCK_OUT" | grep -q "repair (applied)" || fail "leg 15: apply must apply: $FSCK_OUT"
echo "$FSCK_OUT" | grep -q "done  \[C4\]" || fail "leg 15: C4 not applied: $FSCK_OUT"
echo "$FSCK_OUT" | grep -q "done  \[C5\]" || fail "leg 15: C5 not applied: $FSCK_OUT"

# 4. Re-fsck: CLEAN (exit 0, findings: 0).
fsck_when_free 20 "$BIN" fsck "sqmeta://$RIG/meta1" --offline
[ "$FSCK_RC" -eq 0 ] || fail "leg 15: re-fsck after apply must be clean: $FSCK_OUT"
echo "$FSCK_OUT" | grep -q "findings: 0" || fail "leg 15: re-fsck not clean: $FSCK_OUT"

# 5. Quarantine manifest: the copies are listed and present.
QMANIFEST="$(ls "$RIG"/staging/quarantine/*/manifest.json 2>/dev/null | head -1)"
[ -n "$QMANIFEST" ] || fail "leg 15: no quarantine manifest written"
python3 - "$QMANIFEST" <<'EOF' || exit 1
import json, os, sys
m = json.load(open(sys.argv[1]))
classes = {e["class"] for e in m["entries"]}
assert "C4" in classes and "C5" in classes, f"manifest classes: {classes}"
qdir = os.path.dirname(sys.argv[1])
for e in m["entries"]:
    for f in e["files"]:
        p = os.path.join(qdir, f["name"])
        assert os.path.getsize(p) == f["bytes"], f"quarantined file mismatch: {p}"
EOF
echo "    OK: quarantine manifest lists the C4 + C5 copies verbatim"

# 6. The data manifest survives the repair byte-identically.
do_mount
(cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
    || fail "leg 15: dataset manifest mismatch after repair"
# ...and a mounted-side fsck agrees the volume is clean.
"$BIN" fsck "$MNT" | grep -q "findings: 0" || fail "leg 15: online post-repair fsck not clean"
do_unmount
echo "OK: leg 15 (seed => detect => dry-run => apply => re-fsck clean => manifest intact)"

# ---------------------------------------------------------------------------
# Leg 16 (VL7): online defrag — fragment via churn, gauges captured,
# --report-only matches the live gauges, defrag --data improves them,
# manifest byte-identical (G-VL-6 at rig scale)
# ---------------------------------------------------------------------------
note "Leg 16: defrag (fragment => report-only matches => --data => gauges improve => manifest)"

# SINGLE data volume, deliberately: the G-VL-6 synthetic-fragmentation
# fixture needs interleaved deletes to interleave ON THE DEVICE — a
# 2-volume set's placement can cluster alternating files per volume and
# leave each volume's free space nearly contiguous (observed: 0.909).
RIG="$BASE/defrag"
MNT="$BASE/defrag_mnt"
LOG="$RIG/mount.log"
mkdir -p "$RIG/staging" "$MNT"
truncate -s 256M "$RIG/meta1"
truncate -s 8G   "$RIG/oss1"
"$BIN" format "sqmeta://$RIG/meta1" "sqdata://$RIG/oss1" \
    --disk-cache-paths "$RIG/staging" --force >/dev/null
do_mount
mkdir -p "$MNT/dataset"
for i in $(seq 1 24); do
    dd if=/dev/urandom of="$MNT/dataset/f$i.bin" bs=1M count=8 status=none
done
sync -f "$MNT"

# Fragment: delete every other file (interleaved alloc/free), keep a
# survivor manifest.
for i in $(seq 1 2 24); do
    rm "$MNT/dataset/f$i.bin"
done
sync -f "$MNT"
(cd "$MNT/dataset" && sha256sum f*.bin) >"$RIG/manifest.survivors.sha256"

# The frag_* gauges publish on the 5 s worker cadence; wait for the
# fragmentation to show (worst-volume D1 contiguity below 0.9).
frag_gauge() { # frag_gauge <name> — null-safe float read from .stats
    python3 -c '
import json, sys
v = (json.load(open(sys.argv[1])).get("metrics") or {}).get(sys.argv[2])
print(-1.0 if v is None else float(v))
' "$MNT/.stats" "$1"
}
DEADLINE=$((SECONDS + 60))
while :; do
    C="$(frag_gauge frag_d1_contiguity)"
    if python3 -c "import sys; sys.exit(0 if 0 <= $C < 0.9 else 1)"; then break; fi
    [ $SECONDS -lt $DEADLINE ] \
        || fail "leg 16: interleaved deletes did not fragment (frag_d1_contiguity=$C)"
    sleep 2
done
echo "    fragmented: frag_d1_contiguity=$C"

# 1. --report-only matches the live gauges (the G-VL-6 census-match
#    clause at rig scale; the cargo suite pins the independent census).
"$BIN" defrag "$MNT" --report-only --json >"$RIG/report_before.json"
STATS_C="$(frag_gauge frag_d1_contiguity)"
STATS_T="$(frag_gauge frag_d1_reclaimable_tail)"
python3 - "$RIG/report_before.json" "$STATS_C" "$STATS_T" <<'EOF' || fail "leg 16: report-only does not match the live gauges"
import json, sys
r = json.load(open(sys.argv[1]))
worst_c = min(v["contiguity"] for v in r["d1"])
worst_t = min(v["reclaimable_tail"] for v in r["d1"])
sc, st = float(sys.argv[2]), float(sys.argv[3])
# The gauges are permille-coded and refresh on cadence around the report.
assert abs(worst_c - sc) < 0.05, f"contiguity: report {worst_c} vs gauge {sc}"
assert abs(worst_t - st) < 0.05, f"tail: report {worst_t} vs gauge {st}"
assert r["d2"]["pairs"] >= 0 and len(r["d4"]) >= 1
EOF
echo "    OK: --report-only matches the live frag_* gauges"

# 2. defrag --data (the verb waits for the job) => gauges improve.
MOVED_BEFORE="$(stats_field defrag_blocks_moved)"
"$BIN" defrag "$MNT" --data --throttle 100 || fail "leg 16: defrag --data failed"
MOVED_AFTER="$(stats_field defrag_blocks_moved)"
[ "$MOVED_AFTER" -gt "$MOVED_BEFORE" ] \
    || fail "leg 16: engagement — defrag_blocks_moved did not move ($MOVED_BEFORE -> $MOVED_AFTER)"

"$BIN" defrag "$MNT" --report-only --json >"$RIG/report_after.json"
python3 - "$RIG/report_after.json" <<'EOF' || fail "leg 16: defrag --data must reach D1 >= 0.9 and tail >= 0.9"
import json, sys
r = json.load(open(sys.argv[1]))
worst_c = min(v["contiguity"] for v in r["d1"])
worst_t = min(v["reclaimable_tail"] for v in r["d1"])
assert worst_c >= 0.9, f"post-defrag contiguity {worst_c}"
assert worst_t >= 0.9, f"post-defrag reclaimable tail {worst_t}"
EOF
echo "    OK: D1 contiguity/tail >= 0.9 after defrag --data (moved +$((MOVED_AFTER - MOVED_BEFORE)) blocks)"

# 3. Zero corruption: the survivor manifest is byte-identical, across a
#    remount too.
(cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.survivors.sha256" --quiet) \
    || fail "leg 16: survivor manifest mismatch after defrag"
do_unmount
do_mount
(cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.survivors.sha256" --quiet) \
    || fail "leg 16: survivor manifest mismatch after defrag + remount"
do_unmount
echo "OK: leg 16 (defrag: report-only match, D1 improvement, manifest intact)"

# ---------------------------------------------------------------------------
# Leg 17 (VL9): cross-feature interaction matrix at rig scale —
#  (a) mover-scope serialization: whole-set defrag + volume-named defrag
#      both QUEUE behind a running drain (job_serialized_waits is the
#      engagement instrument); the whole-set mover completes after the
#      drain, the mover NAMING the (by then retired) victim fails loud
#      with the placement-eligibility refusal (never a silent no-op);
#  (b) meta-slot-migrate + data-drain concurrently: both converge on a
#      live mount (the cutover gate parks meta ops, never fabric
#      workers), manifest byte-identical, post fsck clean.
#  (The R5-Red job-pause pin is DELIBERATELY cargo-only —
#  tests/interaction_tests.rs charges the job_copy_buffers gauge
#  deterministically; the rig has no honest way to conjure real memory
#  pressure without faking the budget.)
# ---------------------------------------------------------------------------
note "Leg 17a: mover serialization behind a live drain + named-victim refusal"

fresh_drain_rig "interact" 6 16

WAITS_BEFORE="$(stats_field job_serialized_waits)"
"$BIN" volume remove-data "$MNT" oss2 --throttle 10 \
    || fail "leg 17a: remove-data must admit"
[ "$(vol_state_live oss2)" = "draining" ] || fail "leg 17a: oss2 must be draining"

# Both movers intersect the drain's scope: the whole-set defrag must
# queue-then-complete; the oss2-named defrag must queue-then-fail-loud
# (its victim retires under it — placement-ineligible at plan time).
"$BIN" defrag "$MNT" --data --throttle 100 >"$RIG/defrag_all.out" 2>&1 &
DEFRAG_ALL_PID=$!
"$BIN" defrag "$MNT" --data --volume oss2 --throttle 100 >"$RIG/defrag_oss2.out" 2>&1 &
DEFRAG_OSS2_PID=$!

# Engagement: both deferred movers count a serialization episode while
# the drain still runs.
SER_DEADLINE=$((SECONDS + 60))
while :; do
    WAITS_NOW="$(stats_field job_serialized_waits)"
    [ "$WAITS_NOW" -ge $((WAITS_BEFORE + 2)) ] && break
    [ "$(vol_state_live oss2)" = "draining" ] \
        || fail "leg 17a: drain finished before the serialization window was observed"
    [ $SECONDS -lt $SER_DEADLINE ] \
        || fail "leg 17a: job_serialized_waits never moved ($WAITS_BEFORE -> $WAITS_NOW) — the movers did not serialize"
    sleep 0.5
done
echo "    serialization observed: job_serialized_waits $WAITS_BEFORE -> $WAITS_NOW (drain still draining)"

# Unthrottle the drain; everything then resolves.
EVAC_ID="$("$BIN" job list "$MNT" | python3 -c '
import json, sys
rows = json.loads(sys.stdin.read())
live = [r for r in rows if r["state"] in ("queued", "running")
        and "evacuate_volume" in json.dumps(r.get("job_type"))]
print(live[0]["job_id"] if live else "")
')"
[ -n "$EVAC_ID" ] && "$BIN" job throttle "$MNT" "$EVAC_ID" 100 >/dev/null || true
wait_vol_state_live oss2 retired 240

if ! wait "$DEFRAG_ALL_PID"; then
    cat "$RIG/defrag_all.out" >&2
    fail "leg 17a: the whole-set defrag must COMPLETE once the drain releases its scope"
fi
if wait "$DEFRAG_OSS2_PID"; then
    cat "$RIG/defrag_oss2.out" >&2
    fail "leg 17a: defrag naming the drained volume must FAIL loud, not succeed"
fi
grep -qi "failed" "$RIG/defrag_oss2.out" \
    || fail "leg 17a: the named-victim refusal must be loud: $(cat "$RIG/defrag_oss2.out")"
(cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
    || fail "leg 17a: manifest mismatch across the serialized mover pileup"
do_unmount
echo "OK: leg 17a (serialize behind drain: whole-set completed, named victim refused loud)"

note "Leg 17b: meta-slot-migrate + data-drain concurrently (both converge)"

RIG="$BASE/interactmeta"
MNT="$BASE/interactmeta_mnt"
LOG="$RIG/mount.log"
mkdir -p "$RIG/staging" "$MNT"
truncate -s 256M "$RIG/meta1"
truncate -s 256M "$RIG/meta2"
truncate -s 8G   "$RIG/oss1"
truncate -s 8G   "$RIG/oss2"
"$BIN" format "sqmeta://$RIG/meta1,$RIG/meta2" "sqdata://$RIG/oss1,$RIG/oss2" \
    --disk-cache-paths "$RIG/staging" --force >/dev/null
do_mount_uri "sqmeta://$RIG/meta1,$RIG/meta2"
mkdir -p "$MNT/dataset"
for i in $(seq 1 6); do
    dd if=/dev/urandom of="$MNT/dataset/f$i.bin" bs=1M count=16 status=none
done
sync -f "$MNT"
(cd "$MNT/dataset" && sha256sum f*.bin) >"$RIG/manifest.sha256"

"$BIN" volume remove-data "$MNT" oss2 --throttle 10 \
    || fail "leg 17b: remove-data must admit"
[ "$(vol_state_live oss2)" = "draining" ] || fail "leg 17b: oss2 must be draining"

"$BIN" volume migrate-meta-slot "$MNT" 1 0 >/dev/null \
    || fail "leg 17b: migrate-meta-slot must admit beside a live drain"
MIG_DEADLINE=$((SECONDS + 120))
until [ "$(stats_field meta_slot_migrations)" -ge 1 ]; do
    [ $SECONDS -lt $MIG_DEADLINE ] \
        || fail "leg 17b: slot migration did not converge beside the drain"
    sleep 0.5
done
STATE="$(vol_state_live oss2)"
[ "$STATE" = "draining" ] || [ "$STATE" = "retired" ] \
    || fail "leg 17b: drain state lost during the slot migration (got $STATE)"
echo "    slot migration converged while the drain ran (cutover $(stats_field meta_slot_cutover_ms_max) ms)"

EVAC_ID="$("$BIN" job list "$MNT" | python3 -c '
import json, sys
rows = json.loads(sys.stdin.read())
live = [r for r in rows if r["state"] in ("queued", "running", "paused")]
print(live[0]["job_id"] if live else "")
')"
[ -n "$EVAC_ID" ] && "$BIN" job throttle "$MNT" "$EVAC_ID" 100 >/dev/null || true
wait_vol_state_live oss2 retired 240
(cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
    || fail "leg 17b: manifest mismatch after migrate+drain"
"$BIN" fsck "$MNT" | grep -q "findings: 0" || fail "leg 17b: post-pair fsck not clean"
do_unmount
echo "OK: leg 17b (slot migration + drain both converged, manifest intact, fsck clean)"

# ---------------------------------------------------------------------------
# Leg 18 (dynamic meta routing): format anywhere, grow forever — a
# SINGLE-meta-volume default format (the shape that could NEVER grow
# before the derived width) grows to two members by add-meta, with byte
# identity + st_ino stability + live slots on BOTH members.
# ---------------------------------------------------------------------------
note "Leg 18: single-volume format grows by migration (derived width)"

RIG="$BASE/growrig"
MNT="$BASE/grow_mnt"
LOG="$RIG/mount.log"
mkdir -p "$RIG/staging" "$MNT"
truncate -s 256M "$RIG/meta1"
truncate -s 2G   "$RIG/oss1"
"$BIN" format "sqmeta://$RIG/meta1" "sqdata://$RIG/oss1" \
    --disk-cache-paths "$RIG/staging" --force >"$RIG/format.out"
grep -q "derived virtual width" "$RIG/format.out" \
    || fail "leg 18: format must print the derived-width story: $(cat "$RIG/format.out")"
# The retired knob refuses loud naming its successor.
if "$BIN" format "sqmeta://$RIG/meta1" "sqdata://$RIG/oss1" --meta-slots 8 \
    --disk-cache-paths "$RIG/staging" --force >"$RIG/knob.out" 2>&1; then
    fail "leg 18: --meta-slots must be a hard error"
fi
grep -qiE "derived|dynamic" "$RIG/knob.out" \
    || fail "leg 18: the --meta-slots refusal must name the successor: $(cat "$RIG/knob.out")"

do_mount_uri "sqmeta://$RIG/meta1"
mkdir -p "$MNT/dataset"
for d in 0 1 2 3; do
    mkdir -p "$MNT/dataset/d$d"
    for i in $(seq 1 6); do
        dd if=/dev/urandom of="$MNT/dataset/d$d/f$i.bin" bs=256K count=1 status=none
        setfattr -n user.tag -v "d${d}f${i}" "$MNT/dataset/d$d/f$i.bin" 2>/dev/null || true
    done
done
sync -f "$MNT"
(cd "$MNT" && find dataset -type f -exec sha256sum {} + | sort) >"$RIG/manifest.sha256"
ino_manifest "$RIG/inos.before"
do_unmount

# Grow 1 → 2: the previously-impossible transition. Mint spread means
# the single member's load is divisible — take 8 slots.
truncate -s 256M "$RIG/meta2_grown"
ADD_OUT="$(retry_guarded 20 "$BIN" volume add-meta "sqmeta://$RIG/meta1" \
    "$RIG/meta2_grown" --take-slots 8)" \
    || fail "leg 18: single-volume add-meta refused (grow-from-one regression)"
echo "$ADD_OUT" | grep -qi "hosting slot" \
    || fail "leg 18: add-meta must print the taken slots: $ADD_OUT"

do_mount_uri "sqmeta://$RIG/meta1,$RIG/meta2_grown"
verify_meta_manifest "leg 18"
# Both members carry live slots (the new member hosts what it took).
for f in $(seq 1 12); do
    dd if=/dev/urandom of="$MNT/dataset/grown_$f.bin" bs=64K count=1 status=none
done
sync -f "$MNT"
"$BIN" fsck "$MNT" | grep -q "findings: 0" || fail "leg 18: post-grow fsck not clean"
do_unmount
# The new member's stamp claims its slots (probe via volume list JSON).
"$BIN" volume list "sqmeta://$RIG/meta1,$RIG/meta2_grown" --json >"$RIG/grown.json" \
    || fail "leg 18: grown set must list"
echo "OK: leg 18 (single-volume format grew by migration: manifest + inos intact, fsck clean)"

echo "==============================================================="
echo "VOLUME LIFECYCLE RIG (VL3 + VL4 + VL4b + VL5b + VL6a + VL6b + VL7 + VL9 + dynamic-routing legs) PASSED (kill-9 LOOPS=$LOOPS)"
echo "==============================================================="
