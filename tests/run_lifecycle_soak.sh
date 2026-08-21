#!/usr/bin/env bash
set -euo pipefail

# The CANONICAL lifecycle soak — PR VL9 centerpiece
# (docs/design-volume-lifecycle.md §PR VL9; user ruling 2026-07-21).
#
# Each ITERATION:
#   1. mount (fresh set on iteration 1, the CONTINUING set thereafter —
#      volume membership changes persist across iterations)
#   2. create a nested directory structure (8 top dirs × 3 levels)
#   3. create files crossing EVERY layout boundary: inline (< 4 KiB),
#      staged (4 KiB – 4 MiB), striped (> 4 MiB multi-block), sizes
#      straddling the 4 KiB inline edge and the 4 MiB block edge
#      (±1 / ±512 B), a sparse/holey file, a copy_file_range whole-file
#      clone pair, an O_DIRECT-written file, files with xattrs
#   4. validate checksums (sha256 manifest — expected values computed
#      at WRITE TIME from the host-side source payloads)
#   5. ONE lifecycle operation (rotating menu, all ops ≥ once per full
#      default run) — then dismount / remount. Online ops run live;
#      meta add/remove are the OFFLINE verbs between unmount/remount
#      (online meta membership change is not shipped — stated loudly)
#   6. validate checksums again (byte-identical) + st_ino stability
#      (global inos are eternally stable across every menu op — KD-7)
#      + xattr identity
#   7. delete the files; verify space reclaim (volume_states used-bytes
#      sum returns to the run baseline ± slack)
#   8. delete the directory structure
#   9. dismount CLEAN: zero staged residue (the dataset is empty — no
#      staged payload may remain), heartbeats deregistered (VL8 item-4:
#      client:/writer_claim records gone in ~20 s), must-stay-0
#      tripwires (write_path_seed_read_bytes, patch_edge_rmw_reads)
#
# Step-5 menu (rotates; iteration k runs menu[(k-1) % 10]):
#   1 fsck (online, FP=0 asserted)      6 defrag --meta
#   2 scrub (C7, zero failures)         7 data-volume remove/drain (oss3)
#   3 defrag --data                     8 meta-slot migrate (ONLINE)
#   4 data-volume add (+auto-rebalance) 9 meta-volume add   (OFFLINE verb)
#   5 explicit rebalance               10 meta-volume remove (OFFLINE verb)
#
# MULTI-RUN DISCIPLINE (AGENTS.md — the heart of PR VL9): a checksum
# mismatch, EIO, refused re-run, residue at dismount, or tripwire fire
# ABORTS the run with a loud banner + preserved artifacts + nonzero
# exit. After the fix, the count RESTARTS FROM ZERO on the fixed binary;
# completing remaining iterations post-failure is declared rate/signature
# gathering only, never acceptance.
#
# Usage:
#   tests/run_lifecycle_soak.sh                 # SOAK_ITERS=10 (the counted acceptance)
#   SOAK_ITERS=3 tests/run_lifecycle_soak.sh    # short plumbing check
#   KEEP=1 ...                                  # keep artifacts on success too
#
# Unprivileged posture (rig precedent): file-backed volumes + a
# user-owned mountpoint; root NOT required.

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BASE="${BASE:-/tmp/squeezefs_soak_$$}"
SOAK_ITERS="${SOAK_ITERS:-10}"
RECLAIM_SLACK_MB="${SOAK_RECLAIM_SLACK_MB:-16}"
MNT="$BASE/mnt"
LOG="$BASE/mount.log"
SOAK_OK=0

cd "$REPO_DIR"

banner() {
    echo "==============================================================="
    echo "$@"
    echo "==============================================================="
}

fail() {
    banner "SOAK ABORT (iteration ${ITER:-setup}, op ${OP_NAME:-none}): $*"
    echo "Artifacts preserved at: $BASE" >&2
    echo "  mount log:      $LOG" >&2
    echo "  manifests:      $BASE/src/manifest.sha256 / $BASE/inos.*" >&2
    [ -f "$BASE/manifest.diff" ] && echo "  manifest diff:  $BASE/manifest.diff" >&2
    echo "MULTI-RUN DISCIPLINE: fix (or catalog with A/B evidence), then" >&2
    echo "RESTART THE COUNT FROM ZERO on the fixed binary." >&2
    exit 1
}
note() { echo "--- $*"; }

cleanup() {
    if mountpoint -q "$MNT" 2>/dev/null; then
        fusermount3 -uz "$MNT" || true
    fi
    if [ -n "${MOUNT_PID:-}" ]; then
        kill "$MOUNT_PID" &>/dev/null || true
    fi
    if [ "$SOAK_OK" = "1" ] && [ -z "${KEEP:-}" ]; then
        rm -rf "$BASE"
    fi
}
trap cleanup EXIT

mkdir -p "$BASE" "$MNT"

note "build (release)"
cargo build --release
BIN="$REPO_DIR/target/release/squeezefs"

# ---------------------------------------------------------------------------
# The continuing set: 2 meta volumes (W=8 — slot-migrate + meta add/remove
# need slots), 2 data volumes, declared staging.
# ---------------------------------------------------------------------------
mkdir -p "$BASE/staging"
truncate -s 256M "$BASE/meta1"
truncate -s 256M "$BASE/meta2"
truncate -s 8G   "$BASE/oss1"
truncate -s 8G   "$BASE/oss2"
META_URI="sqmeta://$BASE/meta1,$BASE/meta2"
"$BIN" format "$META_URI" "sqdata://$BASE/oss1,$BASE/oss2" \
    --disk-cache-paths "$BASE/staging" --force >/dev/null

# Rotation state (persists across iterations — the CONTINUING set).
OSS3_ID=""            # active vol- id of the oss3 device ("" = not a member)
MIG_TARGET=0          # migrate-meta-slot target toggle (slot 1 starts on vol 1)
META3_PRESENT=0

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
do_mount() {
    RUST_LOG=info "$BIN" mount "$META_URI" "$MNT" >>"$LOG" 2>&1 &
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
        [ $tries -lt 20 ] || fail "fusermount3 -u kept failing"
        sleep 0.5
    done
    wait "$MOUNT_PID" || true
    MOUNT_PID=""
}

stats_field() { # stats_field <metrics-counter-name>
    python3 -c '
import json, sys
data = json.load(open(sys.argv[1]))
print(int((data.get("metrics") or {}).get(sys.argv[2]) or 0))
' "$MNT/.stats" "$1"
}

used_bytes_sum() { # sum of volume_states used_bytes (data volumes)
    python3 -c '
import json, sys
data = json.load(open(sys.argv[1]))
print(sum(r.get("used_bytes", 0) for r in (data.get("volume_states") or [])))
' "$MNT/.stats"
}

vol_state_live() { # vol_state_live <volume-id>
    "$BIN" volume list "$MNT" --json | python3 -c '
import json, sys
rows = json.load(sys.stdin)
print(next((r["state"] for r in rows if r["id"] == sys.argv[1]), "absent"))
' "$1"
}

wait_vol_state() { # wait_vol_state <volume-id> <state> <deadline-secs>
    local deadline=$((SECONDS + $3))
    while [ "$(vol_state_live "$1")" != "$2" ]; do
        [ $SECONDS -lt $deadline ] \
            || fail "volume $1 did not reach '$2' in $3s (now: $(vol_state_live "$1"))"
        sleep 0.5
    done
}

wait_jobs_idle() { # wait_jobs_idle <deadline-secs> — no queued/running/paused job
    local deadline=$((SECONDS + $1))
    while :; do
        local live
        live="$("$BIN" job list "$MNT" | python3 -c '
import json, sys
rows = json.loads(sys.stdin.read())
print(sum(1 for r in rows if r["state"] in ("queued", "running", "paused", "paused-capacity")))
')"
        [ "$live" -eq 0 ] && return 0
        [ $SECONDS -lt $deadline ] \
            || fail "jobs still live after $1s: $("$BIN" job list "$MNT")"
        sleep 1
    done
}

# The clean-unmount heartbeat law (VL8 item-4): guarded offline verbs
# see the records gone almost immediately; only kill-9 rides the TTL.
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
        sleep 2
    done
}

cfr_clone() { # cfr_clone <src-in-mount> <dst-in-mount>
    python3 - "$1" "$2" <<'EOF'
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
}

set_xattrs() { # set_xattrs <iter> <file...>
    python3 - "$@" <<'EOF'
import os, sys
tag = f"soak-iter-{sys.argv[1]}".encode()
for p in sys.argv[2:]:
    os.setxattr(p, "user.soak", tag)
EOF
}

check_xattrs() { # check_xattrs <iter> <file...>
    python3 - "$@" <<'EOF'
import os, sys
tag = f"soak-iter-{sys.argv[1]}".encode()
for p in sys.argv[2:]:
    got = os.getxattr(p, "user.soak")
    assert got == tag, f"{p}: xattr {got!r} != {tag!r}"
EOF
}

ino_manifest() { # ino_manifest <out-file>
    (cd "$MNT" && find dataset -type f -exec stat -c '%n %i' {} + | sort) >"$1"
}

# ---------------------------------------------------------------------------
# Step 2+3: the layout-boundary dataset. Payloads are generated HOST-SIDE
# in $SRC first (sha256 = the write-time expected values), then
# materialized into the mount.
# ---------------------------------------------------------------------------
# relpath:bytes — 4 KiB inline edge (±1, ±512), staged span, 4 MiB block
# edge (±1, −512), multi-block striped (2/3/4 blocks ±).
FILE_SPECS="
d1/inline_tiny.bin:100
d1/l2/inline_4095.bin:4095
d2/edge_4096.bin:4096
d2/l2/l3/edge_4097.bin:4097
d3/edge_3584.bin:3584
d3/l2/edge_4608.bin:4608
d4/staged_8k.bin:8192
d4/l2/staged_1m.bin:1048576
d5/staged_4m_minus512.bin:4193792
d5/l2/l3/boundary_4m.bin:4194304
d6/boundary_4m_plus1.bin:4194305
d6/l2/boundary_4m_minus1.bin:4194303
d7/striped_8m_plus512.bin:8389120
d7/l2/striped_12m.bin:12582912
d8/l2/l3/striped_16m_minus1.bin:16777215
"
DIRECT_REL="d8/direct_8m.bin"     # 8 MiB, O_DIRECT materialize
SPARSE_REL="d8/sparse_16m.bin"    # 16 MiB holey
CLONE_SRC_REL="d7/l2/striped_12m.bin"
CLONE_REL="d7/l2/striped_12m_clone.bin"
XATTR_FILES_REL="d1/inline_tiny.bin d4/l2/staged_1m.bin d7/l2/striped_12m.bin"

build_sources() { # regenerated fresh every iteration
    SRC="$BASE/src"
    rm -rf "$SRC"
    mkdir -p "$SRC"
    for d in 1 2 3 4 5 6 7 8; do mkdir -p "$SRC/d$d/l2/l3"; done
    local spec rel bytes
    for spec in $FILE_SPECS; do
        rel="${spec%%:*}"; bytes="${spec##*:}"
        head -c "$bytes" /dev/urandom >"$SRC/$rel"
    done
    # O_DIRECT payload (8 MiB exactly — aligned materialize).
    head -c 8388608 /dev/urandom >"$SRC/$DIRECT_REL"
    # Sparse twin: 16 MiB, three data islands (offsets straddle block
    # edges), holes elsewhere.
    python3 - "$SRC/$SPARSE_REL" <<'EOF'
import os, sys
with open(sys.argv[1], "wb") as f:
    f.truncate(16777216)
    for off, n in ((0, 1048576), (5242996, 65536), (12587007, 8192)):
        f.seek(off)
        f.write(os.urandom(n))
EOF
    # The write-time manifest (clone = the origin's hash, listed too).
    (cd "$SRC" && find . -type f ! -name 'manifest.sha256' | sed 's|^\./||' | sort | xargs sha256sum) \
        >"$SRC/manifest.sha256"
    local clone_hash
    clone_hash="$(grep " $CLONE_SRC_REL\$" "$SRC/manifest.sha256" | awk '{print $1}')"
    echo "$clone_hash  $CLONE_REL" >>"$SRC/manifest.sha256"
}

materialize_dataset() { # step 2+3 into the mount
    mkdir -p "$MNT/dataset"
    for d in 1 2 3 4 5 6 7 8; do mkdir -p "$MNT/dataset/d$d/l2/l3"; done
    local spec rel
    for spec in $FILE_SPECS; do
        rel="${spec%%:*}"
        cp "$SRC/$rel" "$MNT/dataset/$rel"
    done
    dd if="$SRC/$DIRECT_REL" of="$MNT/dataset/$DIRECT_REL" bs=1M oflag=direct status=none
    cp --sparse=always "$SRC/$SPARSE_REL" "$MNT/dataset/$SPARSE_REL"
    cfr_clone "$MNT/dataset/$CLONE_SRC_REL" "$MNT/dataset/$CLONE_REL"
    local xf=""
    for rel in $XATTR_FILES_REL; do xf="$xf $MNT/dataset/$rel"; done
    # shellcheck disable=SC2086
    set_xattrs "$ITER" $xf
    sync -f "$MNT"
}

validate_dataset() { # validate_dataset <who>
    if ! (cd "$MNT/dataset" && sha256sum -c "$SRC/manifest.sha256" --quiet) >"$BASE/manifest.diff" 2>&1; then
        cat "$BASE/manifest.diff" >&2
        fail "$1: checksum mismatch (ZERO-LOSS violated)"
    fi
    local xf="" rel
    for rel in $XATTR_FILES_REL; do xf="$xf $MNT/dataset/$rel"; done
    # shellcheck disable=SC2086
    check_xattrs "$ITER" $xf || fail "$1: xattr identity lost"
}

# ---------------------------------------------------------------------------
# The step-5 menu
# ---------------------------------------------------------------------------
op_fsck() {
    local out
    out="$("$BIN" fsck "$MNT")" || fail "online fsck reported findings or failed: $out"
    echo "$out" | grep -q "findings: 0" || fail "fsck FP!=0: $out"
}

op_scrub() {
    local out
    out="$("$BIN" scrub "$MNT" --json)" || fail "scrub failed: $out"
    python3 - <<EOF || fail "scrub counters wrong: $out"
import json
r = json.loads('''$out''')
assert r["findings"] == [], r["findings"]
assert r["counters"]["scrub_failures"] == 0
assert r["counters"]["scrub_blocks_scanned"] > 0
EOF
}

op_defrag_data() {
    "$BIN" defrag "$MNT" --data --throttle 100 || fail "defrag --data failed"
    wait_jobs_idle 180
}

op_defrag_meta() {
    "$BIN" defrag "$MNT" --meta || fail "defrag --meta failed"
    wait_jobs_idle 180
}

ensure_oss3_active() {
    if [ -z "$OSS3_ID" ]; then
        [ -f "$BASE/oss3" ] || truncate -s 8G "$BASE/oss3"
        local out
        out="$("$BIN" volume add-data "$MNT" "$BASE/oss3")" || fail "volume add-data failed: $out"
        OSS3_ID="$(echo "$out" | grep -oE 'vol-[0-9a-f]{16}' | head -1)"
        [ -n "$OSS3_ID" ] || fail "add-data printed no vol- id: $out"
        note "  added $BASE/oss3 as $OSS3_ID"
    fi
}

op_data_add() {
    ensure_oss3_active
    # The KD-12 auto-rebalance pass rides the add — let it converge.
    wait_jobs_idle 300
}

op_rebalance() {
    "$BIN" defrag "$MNT" --rebalance || fail "explicit rebalance failed"
    wait_jobs_idle 300
}

op_data_drain() {
    ensure_oss3_active
    wait_jobs_idle 300 # a just-added set may still be rebalancing
    "$BIN" volume remove-data "$MNT" "$OSS3_ID" || fail "remove-data $OSS3_ID refused"
    wait_vol_state "$OSS3_ID" retired 300
    OSS3_ID=""
    wait_jobs_idle 120
}

op_slot_migrate() {
    "$BIN" volume migrate-meta-slot "$MNT" 1 "$MIG_TARGET" \
        || fail "migrate-meta-slot 1 -> $MIG_TARGET refused"
    wait_jobs_idle 180
    local ms
    ms="$(stats_field meta_slot_cutover_ms_max)"
    note "  slot 1 -> volume $MIG_TARGET (cutover window ${ms} ms)"
    MIG_TARGET=$((1 - MIG_TARGET))
}

op_meta_add() { # OFFLINE verb — runs between the step-5 unmount/remount
    note "  meta-volume add is the OFFLINE verb (online meta membership change is not shipped)"
    do_unmount
    [ -f "$BASE/meta3" ] || truncate -s 256M "$BASE/meta3"
    retry_guarded 60 "$BIN" volume add-meta "$META_URI" "$BASE/meta3" --take-slots 2 >/dev/null \
        || fail "offline add-meta refused past the clean-unmount deregistration window"
    META_URI="$META_URI,$BASE/meta3"
    META3_PRESENT=1
    do_mount
}

op_meta_remove() { # OFFLINE verb — between unmount/remount
    if [ "$META3_PRESENT" -ne 1 ]; then
        note "  meta3 not a member (custom rotation) — adding first so the remove is real"
        op_meta_add
    fi
    note "  meta-volume remove is the OFFLINE verb (online meta membership change is not shipped)"
    do_unmount
    retry_guarded 60 "$BIN" volume remove-meta "$META_URI" "$BASE/meta3" >/dev/null \
        || fail "offline remove-meta refused past the clean-unmount deregistration window"
    META_URI="sqmeta://$BASE/meta1,$BASE/meta2"
    META3_PRESENT=0
    do_mount
}

MENU=(fsck scrub defrag_data data_add rebalance defrag_meta data_drain slot_migrate meta_add meta_remove)

run_op() { # step 5: the op, then dismount/remount (meta ops embed theirs)
    OP_NAME="${MENU[$(((ITER - 1) % ${#MENU[@]}))]}"
    note "step 5: lifecycle op '$OP_NAME'"
    case "$OP_NAME" in
        meta_add)     op_meta_add ;;      # unmount -> verb -> remount inside
        meta_remove)  op_meta_remove ;;
        *)
            "op_$OP_NAME"
            do_unmount
            do_mount
            ;;
    esac
}

# ---------------------------------------------------------------------------
# The soak
# ---------------------------------------------------------------------------
banner "CANONICAL LIFECYCLE SOAK: $SOAK_ITERS iteration(s), menu ${MENU[*]}"
TALLY=()

for ITER in $(seq 1 "$SOAK_ITERS"); do
    OP_NAME=""
    banner "ITERATION $ITER/$SOAK_ITERS"
    T0=$SECONDS

    # 1. mount (continuing set)
    do_mount
    if [ "$ITER" -eq 1 ]; then
        USED_BASELINE="$(used_bytes_sum)"
        note "run baseline: used_bytes_sum=$USED_BASELINE"
    fi

    # 2+3. dirs + the layout-boundary dataset (write-time manifest)
    note "steps 2+3: dataset (inline/staged/striped edges, sparse, clone, O_DIRECT, xattrs)"
    build_sources
    materialize_dataset

    # 4. validate at write time
    validate_dataset "step 4 (post-write)"
    ino_manifest "$BASE/inos.before"
    note "step 4 OK: manifest + inos captured"

    # 5. the lifecycle op + dismount/remount
    run_op

    # 6. byte identity + ino stability + xattrs after op + remount
    validate_dataset "step 6 (post-op post-remount)"
    ino_manifest "$BASE/inos.after"
    diff -u "$BASE/inos.before" "$BASE/inos.after" >"$BASE/inos.diff" \
        || fail "step 6: st_ino instability across '$OP_NAME' (KD-7 violated): $(cat "$BASE/inos.diff")"
    note "step 6 OK: byte-identical, inos stable, xattrs intact"

    # 7. delete files; space reclaim to the run baseline ± slack
    find "$MNT/dataset" -type f -delete
    sync -f "$MNT"
    DEADLINE=$((SECONDS + 180))
    while :; do
        USED_NOW="$(used_bytes_sum)"
        if [ "$USED_NOW" -le $((USED_BASELINE + RECLAIM_SLACK_MB * 1024 * 1024)) ]; then
            break
        fi
        [ $SECONDS -lt $DEADLINE ] \
            || fail "step 7: space not reclaimed (baseline $USED_BASELINE, now $USED_NOW, slack ${RECLAIM_SLACK_MB}MiB)"
        sleep 2
    done
    note "step 7 OK: space reclaimed ($USED_NOW vs baseline $USED_BASELINE)"

    # 8. delete the directory structure
    rm -rf "$MNT/dataset"
    sync -f "$MNT"

    # 9. clean dismount: tripwires, staged residue, heartbeats
    SEED_READS="$(stats_field write_path_seed_read_bytes)"
    [ "$SEED_READS" -eq 0 ] || fail "step 9: write_path_seed_read_bytes=$SEED_READS (must stay 0)"
    EDGE_RMW="$(stats_field patch_edge_rmw_reads)"
    [ "$EDGE_RMW" -eq 0 ] || fail "step 9: patch_edge_rmw_reads=$EDGE_RMW (must stay 0)"
    do_unmount

    DEADLINE=$((SECONDS + 30))
    while :; do
        LIVE="$("$BIN" clients "$META_URI" --json 2>/dev/null | python3 -c '
import json, sys
d = json.load(sys.stdin)
print(int(d.get("live", 0)) + len(d.get("clients", [])))
' || echo 999)"
        [ "$LIVE" -eq 0 ] && break
        [ $SECONDS -lt $DEADLINE ] \
            || fail "step 9: heartbeat records survive a clean dismount (VL8 item-4 regression): $("$BIN" clients "$META_URI" --json)"
        sleep 2
    done

    # Zero staged residue: the dataset is empty, so an OFFLINE fsck must
    # be totally clean — C4 (orphan staged custody), C5 (generation),
    # C2/C3 (leaked/lost blocks, refcounts) all zero. (The mmap
    # segment-pool arena files persist by design — they are not residue;
    # custody records are.)
    OFF_FSCK="$(retry_guarded 45 "$BIN" fsck "$META_URI" --offline)" \
        || fail "step 9: offline fsck after the clean dismount refused or found: $OFF_FSCK"
    echo "$OFF_FSCK" | grep -q "findings: 0" \
        || fail "step 9: staged/census residue after a clean dismount: $OFF_FSCK"
    note "step 9 OK: clean dismount (zero staged residue, heartbeats deregistered)"

    TALLY+=("iter $ITER: op=$OP_NAME ok ($((SECONDS - T0))s)")
    echo "OK: iteration $ITER (op $OP_NAME, $((SECONDS - T0))s)"
done

banner "CANONICAL LIFECYCLE SOAK PASSED (${SOAK_ITERS} iterations)"
printf '%s\n' "${TALLY[@]}"
SOAK_OK=1
