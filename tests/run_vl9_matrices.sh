#!/bin/bash
set -euo pipefail

# PR VL9 — the counted G-VL closing matrices
# (docs/design-volume-lifecycle.md §12; the ×10 soaks the per-PR G-VL
# gates deferred to VL9), scripted with the AGENTS multi-run discipline:
#
#   A deterministic or attributable failure on ANY counted loop ABORTS
#   the whole run (loud banner, artifacts preserved, nonzero exit).
#   After the fix the count RESTARTS FROM ZERO on the fixed binary —
#   completing the remaining loops post-failure is declared
#   rate/signature gathering only, NEVER acceptance.
#
# Matrices:
#   m1  G-VL-3(a): drain kill-9 zero-loss ×LOOPS — kill -9 the
#       coordinator daemon at a randomized point mid-drain (mid-copy /
#       mid-publish / mid-retire sampled), remount adopts + re-plans
#       (KD-6), drain converges, checksum manifest byte-identical.
#   m2  G-VL-4: (a) §5.5.2b crash-window injection — the deterministic
#       cargo seams (kill after flip writes 0/1/2/3, 3 rounds per run ×
#       CARGO_RUNS runs = 12/window ≥ 10) + the torn-slot fallback test
#       ×LOOPS; (b) staging-barrier / offline add-meta coordinator
#       kill-9 ×LOOPS at randomized points — re-run converges, manifest
#       + inos intact (KD-8: the barrier is zero-loss).
#   m3  G-VL-5(a): online fsck false positives = 0 — (row 1) ×LOOPS
#       fsck passes under concurrent write/delete churn; (row 2) ×LOOPS
#       fresh drains over a clone-heavy dataset with fsck INSIDE the
#       drain window (the mover-ledger/pin adversary), post-drain fsck
#       clean, clones byte-identical.
#   m4  G-VL-7: fabric kill-9 ×LOOPS — kill -9 the coordinator mid
#       defrag --data, remount, the adopted durable job converges with
#       no operator input, manifest byte-identical, fsck clean.
#
# Usage:
#   tests/run_vl9_matrices.sh              # all matrices, LOOPS=10
#   tests/run_vl9_matrices.sh m1 m3        # a subset
#   LOOPS=3 tests/run_vl9_matrices.sh      # plumbing check (NOT acceptance)
#
# Unprivileged posture: file-backed volumes, user mountpoint (rig
# precedent); needs /dev/fuse + fuse.enable_uring + fusermount3.

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BASE="${BASE:-/tmp/squeezefs_vl9_mx_$$}"
LOOPS="${LOOPS:-10}"
CARGO_RUNS="${CARGO_RUNS:-4}"   # 3 rounds/window per run -> 12/window
MATRICES=("${@:-m1 m2 m3 m4}")
[ $# -eq 0 ] && MATRICES=(m1 m2 m3 m4)
OK=0

cd "$REPO_DIR"

banner() {
    echo "==============================================================="
    echo "$@"
    echo "==============================================================="
}

fail() {
    banner "MATRIX ABORT (${MATRIX:-setup} loop ${LOOP:-–}): $*"
    echo "Artifacts preserved at: $BASE" >&2
    echo "MULTI-RUN DISCIPLINE: fix (or catalog with A/B evidence), then" >&2
    echo "RESTART THE COUNT FROM ZERO on the fixed binary." >&2
    exit 1
}
note() { echo "--- $*"; }

cleanup() {
    if [ -n "${MNT:-}" ] && mountpoint -q "$MNT" 2>/dev/null; then
        fusermount3 -uz "$MNT" || true
    fi
    if [ -n "${MOUNT_PID:-}" ]; then
        kill "$MOUNT_PID" &>/dev/null || true
    fi
    if [ "$OK" = "1" ] && [ -z "${KEEP:-}" ]; then
        rm -rf "$BASE"
    fi
}
trap cleanup EXIT

mkdir -p "$BASE"

transport_supported() {
    [ -e /dev/fuse ] || { echo "SKIP: /dev/fuse not present"; return 1; }
    case "$(cat /sys/module/fuse/parameters/enable_uring 2>/dev/null || true)" in
        Y|y|1) ;;
        *) echo "SKIP: kernel fuse.enable_uring not enabled"; return 1 ;;
    esac
    command -v fusermount3 >/dev/null || { echo "SKIP: fusermount3 missing"; return 1; }
    return 0
}
transport_supported || fail "the counted matrices need a mount-capable box (no silent skip)"

note "build (release)"
cargo build --release
BIN="$REPO_DIR/target/release/squeezefs"

do_mount() { # do_mount <sqmeta-uri>
    RUST_LOG=info "$BIN" mount "$1" "$MNT" >>"$LOG" 2>&1 &
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

kill9_daemon() {
    kill -9 "$MOUNT_PID" || true
    wait "$MOUNT_PID" 2>/dev/null || true
    MOUNT_PID=""
    fusermount3 -uz "$MNT" 2>/dev/null || true
}

stats_field() {
    python3 -c '
import json, sys
data = json.load(open(sys.argv[1]))
print(int((data.get("metrics") or {}).get(sys.argv[2]) or 0))
' "$MNT/.stats" "$1"
}

vol_state_live() {
    "$BIN" volume list "$MNT" --json | python3 -c '
import json, sys
rows = json.load(sys.stdin)
print(next((r["state"] for r in rows if r["id"] == sys.argv[1]), "absent"))
' "$1"
}

wait_vol_state() { # <id> <state> <secs>
    local deadline=$((SECONDS + $3))
    while [ "$(vol_state_live "$1")" != "$2" ]; do
        [ $SECONDS -lt $deadline ] \
            || fail "volume $1 did not reach '$2' in $3s (now: $(vol_state_live "$1"))"
        sleep 0.5
    done
}

wait_jobs_idle() { # <secs>
    local deadline=$((SECONDS + $1))
    while :; do
        local live
        live="$("$BIN" job list "$MNT" | python3 -c '
import json, sys
rows = json.loads(sys.stdin.read())
print(sum(1 for r in rows if r["state"] in ("queued", "running", "paused-capacity")))
')"
        [ "$live" -eq 0 ] && return 0
        [ $SECONDS -lt $deadline ] || fail "jobs still live after $1s: $("$BIN" job list "$MNT")"
        sleep 1
    done
}

retry_guarded() { # <secs> <cmd...>
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
        echo "    (waiting for heartbeat records to go stale / dead-pid proof...)" >&2
        sleep 5
    done
}

random_delay() { # 0.0–2.9 s, occasionally whole seconds (samples phases)
    local d="0.$((RANDOM % 9))"
    [ $((RANDOM % 3)) -eq 0 ] && d=$((RANDOM % 3))
    echo "$d"
}

fresh_data_rig() { # fresh_data_rig <name> <n-files> <mib-per-file>
    RIG="$BASE/$1"
    MNT="$BASE/$1_mnt"
    LOG="$RIG/mount.log"
    rm -rf "$RIG" "$MNT"
    mkdir -p "$RIG/staging" "$MNT"
    truncate -s 256M "$RIG/meta1"
    truncate -s 8G   "$RIG/oss1"
    truncate -s 8G   "$RIG/oss2"
    "$BIN" format "sqmeta://$RIG/meta1" "sqdata://$RIG/oss1,$RIG/oss2" \
        --disk-cache-paths "$RIG/staging" --force >/dev/null
    do_mount "sqmeta://$RIG/meta1"
    mkdir -p "$MNT/dataset"
    for i in $(seq 1 "$2"); do
        dd if=/dev/urandom of="$MNT/dataset/f$i.bin" bs=1M count="$3" status=none
    done
    sync -f "$MNT"
    (cd "$MNT/dataset" && sha256sum f*.bin) >"$RIG/manifest.sha256"
}

cfr_clone() { # <src> <dst>
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

TALLY=()

# ---------------------------------------------------------------------------
# m1 — G-VL-3(a): drain kill-9 zero-loss ×LOOPS
# ---------------------------------------------------------------------------
run_m1() {
    MATRIX="m1 G-VL-3(a) drain-kill-9"
    banner "$MATRIX ×$LOOPS"
    for LOOP in $(seq 1 "$LOOPS"); do
        fresh_data_rig "m1_$LOOP" 6 16
        "$BIN" volume remove-data "$MNT" oss2 --throttle 50 \
            || fail "remove-data must admit"
        local d; d="$(random_delay)"
        sleep "$d"
        kill9_daemon
        echo "    killed coordinator after ${d}s"
        do_mount "sqmeta://$RIG/meta1"
        wait_vol_state oss2 retired 240
        (cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
            || fail "manifest mismatch after resume (ZERO-LOSS violated)"
        do_unmount
        rm -rf "$RIG" "$MNT"
        echo "OK: m1 loop $LOOP/$LOOPS (killed @${d}s, converged, byte-identical)"
    done
    TALLY+=("m1 G-VL-3(a) drain kill-9: $LOOPS/$LOOPS zero-loss")
}

# ---------------------------------------------------------------------------
# m2 — G-VL-4: crash windows (cargo seams) + staging-barrier kill-9
# ---------------------------------------------------------------------------
run_m2() {
    MATRIX="m2 G-VL-4 crash-windows"
    banner "$MATRIX: cargo seams ×$CARGO_RUNS (3 rounds/window each) + torn ×$LOOPS"
    for LOOP in $(seq 1 "$CARGO_RUNS"); do
        cargo test --all-features --test meta_slot_migration_tests \
            test_flip_crash_windows_resolve_and_rerun_converges -- --exact --test-threads=1 \
            >"$BASE/m2_windows_$LOOP.log" 2>&1 \
            || { tail -30 "$BASE/m2_windows_$LOOP.log" >&2; fail "crash-window cargo run $LOOP failed"; }
        echo "OK: m2 crash-window cargo run $LOOP/$CARGO_RUNS (windows 0-3 ×3 rounds)"
    done
    for LOOP in $(seq 1 "$LOOPS"); do
        cargo test --all-features --test meta_slot_migration_tests \
            test_flip_torn_claim_slot_falls_back_and_rerun_converges -- --exact --test-threads=1 \
            >"$BASE/m2_torn_$LOOP.log" 2>&1 \
            || { tail -30 "$BASE/m2_torn_$LOOP.log" >&2; fail "torn-slot cargo run $LOOP failed"; }
        echo "OK: m2 torn-slot cargo run $LOOP/$LOOPS"
    done

    MATRIX="m2 G-VL-4 staging-barrier/add-meta kill-9"
    banner "$MATRIX ×$LOOPS"
    for LOOP in $(seq 1 "$LOOPS"); do
        RIG="$BASE/m2k_$LOOP"
        MNT="$BASE/m2k_${LOOP}_mnt"
        LOG="$RIG/mount.log"
        rm -rf "$RIG" "$MNT"
        mkdir -p "$RIG/staging" "$MNT"
        truncate -s 256M "$RIG/meta1"
        truncate -s 256M "$RIG/meta2"
        truncate -s 2G   "$RIG/oss1"
        "$BIN" format "sqmeta://$RIG/meta1,$RIG/meta2" "sqdata://$RIG/oss1" \
            --disk-cache-paths "$RIG/staging" --force >/dev/null
        do_mount "sqmeta://$RIG/meta1,$RIG/meta2"
        mkdir -p "$MNT/dataset"
        for d in 0 1 2 3; do
            mkdir -p "$MNT/dataset/d$d"
            for i in $(seq 1 6); do
                dd if=/dev/urandom of="$MNT/dataset/d$d/f$i.bin" bs=256K count=1 status=none
            done
        done
        sync -f "$MNT"
        (cd "$MNT" && find dataset -type f -exec sha256sum {} + | sort) >"$RIG/manifest.sha256"
        (cd "$MNT" && find dataset -type f -exec stat -c '%n %i' {} + | sort) >"$RIG/inos.before"
        do_unmount

        truncate -s 256M "$RIG/meta3"
        local d; d="$(random_delay)"
        ( retry_guarded 90 "$BIN" volume add-meta "sqmeta://$RIG/meta1,$RIG/meta2" \
            "$RIG/meta3" --take-slots 2 >/dev/null 2>&1 ) &
        local coord=$!
        sleep "$d"
        kill -9 "$coord" 2>/dev/null || true
        wait "$coord" 2>/dev/null || true
        pkill -9 -f "volume add-meta.*m2k_$LOOP" 2>/dev/null || true
        echo "    killed add-meta coordinator after ${d}s"

        retry_guarded 120 "$BIN" volume add-meta "sqmeta://$RIG/meta1,$RIG/meta2" \
            "$RIG/meta3" --take-slots 2 >/dev/null \
            || fail "add-meta re-run did not converge"
        do_mount "sqmeta://$RIG/meta1,$RIG/meta2,$RIG/meta3"
        (cd "$MNT" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
            || fail "manifest mismatch after barrier kill-9 (ZERO-LOSS violated)"
        (cd "$MNT" && find dataset -type f -exec stat -c '%n %i' {} + | sort) >"$RIG/inos.after"
        diff -u "$RIG/inos.before" "$RIG/inos.after" >/dev/null \
            || fail "st_ino instability across the kill-9'd set change (KD-7 violated)"
        do_unmount
        rm -rf "$RIG" "$MNT"
        echo "OK: m2 barrier kill-9 loop $LOOP/$LOOPS (killed @${d}s, re-run converged)"
    done
    TALLY+=("m2 G-VL-4: windows ×$((CARGO_RUNS * 3))/window + torn ×$LOOPS + barrier kill-9 $LOOPS/$LOOPS")
}

# ---------------------------------------------------------------------------
# m3 — G-VL-5(a): fsck FP=0, both rows ×LOOPS
# ---------------------------------------------------------------------------
run_m3() {
    MATRIX="m3 G-VL-5(a) fsck-under-churn"
    banner "$MATRIX ×$LOOPS passes"
    fresh_data_rig "m3churn" 6 8
    ( i=0; while [ -e "$MNT/.stats" ]; do
          dd if=/dev/urandom of="$MNT/dataset/churn_$((i % 8)).bin" bs=256K count=1 \
              conv=notrunc status=none 2>/dev/null || break
          [ $((i % 5)) -eq 0 ] && rm -f "$MNT/dataset/churn_$(((i + 3) % 8)).bin" 2>/dev/null
          i=$((i + 1))
      done ) &
    local churn=$!
    for LOOP in $(seq 1 "$LOOPS"); do
        local out
        out="$("$BIN" fsck "$MNT")" \
            || fail "fsck pass $LOOP under churn failed or found: $out"
        echo "$out" | grep -q "findings: 0" || fail "fsck pass $LOOP FP!=0: $out"
        echo "OK: m3 row-1 fsck pass $LOOP/$LOOPS under churn — findings: 0"
    done
    kill "$churn" 2>/dev/null || true
    wait "$churn" 2>/dev/null || true
    [ "$(stats_field fsck_findings)" -eq 0 ] || fail "fsck_findings tripwire nonzero"
    [ "$(stats_field fsck_inodes_scanned)" -gt 0 ] || fail "engagement: no inodes scanned"
    do_unmount
    rm -rf "$RIG" "$MNT"

    MATRIX="m3 G-VL-5(a) fsck-during-drain (clone-heavy)"
    banner "$MATRIX ×$LOOPS"
    for LOOP in $(seq 1 "$LOOPS"); do
        fresh_data_rig "m3drain_$LOOP" 4 16
        for f in f1 f2; do
            cfr_clone "$MNT/dataset/$f.bin" "$MNT/dataset/${f}_clone.bin"
        done
        sync -f "$MNT"
        (cd "$MNT/dataset" && sha256sum f*.bin) >"$RIG/clones.sha256"
        "$BIN" volume remove-data "$MNT" oss2 --throttle 10 \
            || fail "remove-data must admit"
        [ "$(vol_state_live oss2)" = "draining" ] || fail "oss2 must be draining"
        local out
        out="$("$BIN" fsck "$MNT")" \
            || fail "drain-concurrent fsck failed or found: $out"
        echo "$out" | grep -q "findings: 0" || fail "drain-concurrent fsck FP!=0: $out"
        # Unthrottle, converge, verify.
        local evac
        evac="$("$BIN" job list "$MNT" | python3 -c '
import json, sys
rows = json.loads(sys.stdin.read())
live = [r for r in rows if r["state"] in ("queued", "running", "paused")]
print(live[0]["job_id"] if live else "")
')"
        [ -n "$evac" ] && "$BIN" job throttle "$MNT" "$evac" 100 >/dev/null || true
        wait_vol_state oss2 retired 240
        (cd "$MNT/dataset" && sha256sum -c "$RIG/clones.sha256" --quiet) \
            || fail "clones not byte-identical after drain + fsck"
        "$BIN" fsck "$MNT" | grep -q "findings: 0" || fail "post-drain fsck not clean"
        do_unmount
        rm -rf "$RIG" "$MNT"
        echo "OK: m3 row-2 loop $LOOP/$LOOPS (mid-drain FP=0, clones intact, post-drain clean)"
    done
    TALLY+=("m3 G-VL-5(a): churn ×$LOOPS FP=0 + drain-concurrent ×$LOOPS FP=0")
}

# ---------------------------------------------------------------------------
# m4 — G-VL-7: fabric kill-9 mid-job ⇒ remount ⇒ adopted job converges
# ---------------------------------------------------------------------------
run_m4() {
    MATRIX="m4 G-VL-7 fabric-kill-9 (defrag)"
    banner "$MATRIX ×$LOOPS"
    for LOOP in $(seq 1 "$LOOPS"); do
        fresh_data_rig "m4_$LOOP" 12 8
        # Fragment: delete every other file (survivor manifest).
        for i in $(seq 1 2 12); do rm "$MNT/dataset/f$i.bin"; done
        sync -f "$MNT"
        (cd "$MNT/dataset" && sha256sum f*.bin) >"$RIG/manifest.sha256"

        ( "$BIN" defrag "$MNT" --data --throttle 25 >/dev/null 2>&1 || true ) &
        local verb=$!
        local d; d="$(random_delay)"
        sleep "$d"
        kill9_daemon
        kill "$verb" 2>/dev/null || true
        wait "$verb" 2>/dev/null || true
        echo "    killed coordinator after ${d}s"

        do_mount "sqmeta://$RIG/meta1"
        # The durable job record is adopted and converges unattended.
        wait_jobs_idle 300
        (cd "$MNT/dataset" && sha256sum -c "$RIG/manifest.sha256" --quiet) \
            || fail "manifest mismatch after fabric kill-9 (ZERO-LOSS violated)"
        "$BIN" fsck "$MNT" | grep -q "findings: 0" || fail "post-resume fsck not clean"
        do_unmount
        rm -rf "$RIG" "$MNT"
        echo "OK: m4 loop $LOOP/$LOOPS (killed @${d}s, adopted job converged, byte-identical)"
    done
    TALLY+=("m4 G-VL-7 fabric kill-9: $LOOPS/$LOOPS converged")
}

# ---------------------------------------------------------------------------
banner "VL9 COUNTED MATRICES: ${MATRICES[*]} (LOOPS=$LOOPS)"
for m in ${MATRICES[*]}; do
    case "$m" in
        m1) run_m1 ;;
        m2) run_m2 ;;
        m3) run_m3 ;;
        m4) run_m4 ;;
        *) fail "unknown matrix '$m' (m1 m2 m3 m4)" ;;
    esac
done

banner "VL9 COUNTED MATRICES PASSED (LOOPS=$LOOPS)"
printf '%s\n' "${TALLY[@]}"
OK=1
