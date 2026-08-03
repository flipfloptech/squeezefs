#!/usr/bin/env bash
# tests/write_amp_rig.sh — shim streaming-write amplification instrument
# (.benchmarks/2026-07-27-shim-write-amplification.md).
#
# Field capture (6-node cluster, relaxed seq-write 1 MiB shim row): device
# 6.6 GB/s at 98% util serving user 3.6 GB/s = 1.85× write amplification,
# wareq-sz ≈ 2.8 MiB against 4 MiB blocks — blocks flushed at ~70%
# coverage then re-written when the remaining ring chunks arrive. The
# red/green here is the ratio `device write bytes / user bytes` on the
# DATA namespace (meta rides its own namespace, so the data-device delta
# is exact), visible on any rig via /proc/diskstats + the stats inode.
#
# Rows (seq_write_1m battery shape: elbencho -w -t 16 -b 1m --direct):
#   kernel     — unintercepted (the ~1.1× reference posture)
#   shim       — LD_PRELOAD ring path, default geometry (ring/op 1.00)
#   shim-frag  — SQUEEZEFS_IL_MAX_RUN_SLOTS=1: every ring op chunks to the
#                64 KiB slot slab and pipelines flights — the deterministic
#                form of the field's slot-fragmented fleet arrival
#   (+ seq_read_1m kernel/shim rows via SQZ_WA_ROWS for the read-side
#    sibling check: rareq-sz + device read bytes / user bytes)
#
# Rewrite-program SLO rows (Idea 17, docs/design-rewrite-program.md §2):
#   seq_overwrite_1m — untimed fileset prep, then the timed FULL
#       overwrite. Prints REWRITE_AMP (device write bytes / user
#       overwrite bytes) with the mid-row discard columns; under
#       SQZ_WA_REWRITE_GATE=1 (default) the charter gates are enforced:
#       REWRITE_AMP ≤ 1.05 AND d_ops == 0 during the row.
#   loop_rewrite    — SQZ_WA_LOOP_PASSES (default 5) full-overwrite
#       passes over a hot set (SQZ_WA_LOOP_FILES × SQZ_WA_LOOP_FILE_MB,
#       default 4 × 64 MiB) in ONE measured window. Prints the
#       latest-wins verdict: device writes ÷ unique-block bytes and the
#       coalesce factor (user ÷ device), plus the supersession
#       engagement delta. Report-only on the non-overlapping face (the
#       ≈-unique-blocks gate arms with the Idea 8 durability classes —
#       design §2.2).
#
# Instrument (stated): elbencho (DYNAMIC build — the pinned static one
# cannot load the shim), medians of SQZ_WA_REPS (default 3). Engagement
# per row from .stats deltas: a shim row is INVALID unless ipc_ops_write
# (or ipc_ops_read) accounts for the row's ops.
#
# Usage:
#   sudo SQZ_META_DEV=/dev/nvme4n1 SQZ_DATA_DEV=/dev/nvme3n1 \
#        SQZ_WA_RESULTS=/tmp/wamp tests/write_amp_rig.sh [row-filter-regex]
set -u

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
META_DEV="${SQZ_META_DEV:-/dev/nvme4n1}"
DATA_DEV="${SQZ_DATA_DEV:-/dev/nvme3n1}"
MOUNT_DIR="${MOUNT_DIR:-/mnt/sqz_write_amp}"
RESULTS="${SQZ_WA_RESULTS:-/tmp/write_amp_$(date +%Y%m%d_%H%M%S)}"
ROW_FILTER="${1:-seq_write}"
REPS="${SQZ_WA_REPS:-3}"
THREADS="${SQZ_WA_THREADS:-16}"
FILES="${SQZ_WA_FILES:-16}"
FILE_MB="${SQZ_WA_FILE_MB:-512}"
SQUEEZEFS_BIN="${SQUEEZEFS_BIN:-$REPO_DIR/target/release/squeezefs}"
SO="${SQZ_WA_SO:-$REPO_DIR/target/preload-release/libsqueezefs_il.so}"
ELBENCHO_BIN="${ELBENCHO_BIN:-$(command -v elbencho)}"
LOG="$RESULTS/daemon.log"
CSV="$RESULTS/amp.csv"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }
[ -x "$SQUEEZEFS_BIN" ] || { echo "missing $SQUEEZEFS_BIN"; exit 1; }
[ -f "$SO" ] || { echo "missing shim $SO"; exit 1; }
[ -b "$META_DEV" ] && [ -b "$DATA_DEV" ] || { echo "missing devices"; exit 1; }
[ -n "$ELBENCHO_BIN" ] || { echo "missing elbencho"; exit 1; }
mkdir -p "$RESULTS" "$MOUNT_DIR"

DATA_BASE="$(basename "$DATA_DEV")"

# Attribution keys printed per row (full delta persisted per rep).
KEYS="ipc_ops_write ipc_ops_read ipc_bytes_in ipc_bytes_out \
write_through_blocks write_through_bytes write_through_fallbacks \
durable_upload_bytes_escalation flush_seed_read_bytes \
overwrite_seed_materialized overwrite_seed_skipped \
spill_staging_puts spill_staging_put_bytes spill_seed_read_bytes \
staging_put_bytes_wt_fallback restage_churn_bytes \
extent_parks extent_spills extent_spill_bytes fold_passes fold_seed_reads \
patch_writes patch_write_bytes active_block_ooo_runs write_block_revisits \
parked_gate_waits parked_gate_self_flushes parked_gate_timeouts \
mem_budget_yellow_events mem_budget_red_events \
write_path_seed_read_bytes patch_edge_rmw_reads \
block_free_discards block_free_discard_bytes block_free_file_punches \
block_free_punch_bytes block_free_reclaim_skipped \
rewrite_blocks rewrite_user_bytes rewrite_device_write_bytes \
block_free_reclaim_elided block_free_elided_debt_bytes \
block_free_trim_discards block_free_debt_pressure_drains \
write_pipeline_supersessions write_pipeline_superseded_bytes \
rewrite_shadow_swaps rewrite_shadow_bytes rewrite_shadow_fallbacks \
write_through_inplace_overwrites \
ipc_descriptor_rejects ipc_sessions_poisoned"

INVALID=0
fail() { echo "FAIL: $*"; exit 1; }

snap_stats() { # <outfile>
    python3 -c "import json;print(json.dumps(json.load(open('$MOUNT_DIR/.stats'))['metrics']))" \
        > "$1" 2>/dev/null || echo '{}' > "$1"
}

diff_stats() { # <before> <after> <outfile>
    python3 - "$1" "$2" "$3" "$KEYS" <<'EOF'
import json, sys
before = json.load(open(sys.argv[1])); after = json.load(open(sys.argv[2]))
delta = {}
for k, v in after.items():
    if isinstance(v, (int, float)) and isinstance(before.get(k, 0), (int, float)):
        d = v - before.get(k, 0)
        if d:
            delta[k] = d
json.dump(delta, open(sys.argv[3], "w"), indent=1, sort_keys=True)
sel = sys.argv[4].split()
print("  stats: " + " ".join(f"{k}={delta.get(k, 0)}" for k in sel if delta.get(k, 0)))
EOF
}

# reads, rsect, writes, wsect, discards, dsect
snap_disk() { awk -v d="$DATA_BASE" '$3==d {print $4, $6, $8, $10, $15+0, $17+0}' /proc/diskstats; }

kill_daemon() {
    umount "$MOUNT_DIR" 2>/dev/null || umount -l "$MOUNT_DIR" 2>/dev/null || true
    for _ in $(seq 30); do pidof squeezefs >/dev/null || break; sleep 0.5; done
    killall -9 squeezefs 2>/dev/null || true
    sleep 1
}
trap kill_daemon EXIT

format_fs() {
    "$SQUEEZEFS_BIN" format "sqmeta://$META_DEV" "sqdata://$DATA_DEV" --force \
        >> "$RESULTS/format.log" 2>&1 || fail "format"
    sleep 1 # flock settle
}

mount_fs() {
    rm -f "$LOG"
    # SQUEEZEFS_IPC_ALLOW_DEV on the DAEMON side too: dev-tree (-dirty)
    # builds carry a degenerate KD-7 identity that both halves must
    # explicitly forgive (counted in ipc_binds_dev_override).
    RUST_LOG=info SQUEEZEFS_IPC_SERVICE_THREADS="${SQZ_WA_SERVICE_THREADS:-8}" \
        SQUEEZEFS_IPC_ALLOW_DEV=1 \
        ${SQZ_WA_DAEMON_ENV:-} "$SQUEEZEFS_BIN" mount \
        "sqmeta://$META_DEV" "$MOUNT_DIR" --daemon --allow-other --interception \
        --mem-cache-size "${SQZ_WA_CACHE:-1GB}" --log-file "$LOG" || fail "mount"
    for _ in $(seq 20); do mountpoint -q "$MOUNT_DIR" && break; sleep 0.5; done
    mountpoint -q "$MOUNT_DIR" || { tail -20 "$LOG"; fail "mount not up"; }
    chmod 1777 "$MOUNT_DIR"
}

el_mibs() { # <logfile> — LAST-DONE MiB/s
    awk '$1=="Throughput" && $2=="MiB/s" {v=$NF} END {print v+0}' "$1"
}

dataset_files() {
    DATA_FILES=()
    for i in $(seq 1 "$FILES"); do DATA_FILES+=("$MOUNT_DIR/wadata/f$i"); done
    mkdir -p "$MOUNT_DIR/wadata"
}

run_one() { # <row> <side> <rep> <mode w|r> -- <elbencho args...>
    local row="$1" side="$2" rep="$3" mode="$4"; shift 5
    local tag="$row.$side.r$rep"
    local envp=()
    case "$side" in
        shim) envp=(env "LD_PRELOAD=$SO" "SQUEEZEFS_IPC_ALLOW_DEV=1") ;;
        shim-frag) envp=(env "LD_PRELOAD=$SO" "SQUEEZEFS_IPC_ALLOW_DEV=1" \
            "SQUEEZEFS_IL_MAX_RUN_SLOTS=${SQZ_WA_FRAG_SLOTS:-1}") ;;
    esac
    snap_stats "$RESULTS/$tag.stats.before"
    local disk_b; disk_b="$(snap_disk)"
    local t0 t1
    t0=$(date +%s.%N)
    "${envp[@]}" "$ELBENCHO_BIN" "$@" > "$RESULTS/$tag.elbencho" 2>&1 \
        || { cat "$RESULTS/$tag.elbencho"; fail "elbencho $tag"; }
    t1=$(date +%s.%N)
    local disk_a; disk_a="$(snap_disk)"
    snap_stats "$RESULTS/$tag.stats.after"
    local mibs; mibs=$(el_mibs "$RESULTS/$tag.elbencho")
    local user_bytes=$((FILES * FILE_MB * 1024 * 1024))
    # amp + req-sz from the data-namespace diskstats delta
    local ampline
    ampline=$(python3 - "$disk_b" "$disk_a" "$user_bytes" "$mode" <<'EOF'
import sys
b = [int(x) for x in sys.argv[1].split()]
a = [int(x) for x in sys.argv[2].split()]
user = int(sys.argv[3]); mode = sys.argv[4]
r_ops, r_sec = a[0]-b[0], a[1]-b[1]
w_ops, w_sec = a[2]-b[2], a[3]-b[3]
def req(ops, sec): return (sec*512/ops/1048576) if ops else 0
d_ops, d_sec = a[4]-b[4], a[5]-b[5]
if mode == "w":
    amp = w_sec*512/user if user else 0
    print(f"AMP={amp:.3f} w_MiB={w_sec*512//1048576} wareq={req(w_ops,w_sec):.2f}MiB "
          f"w_ops={w_ops} r_MiB={r_sec*512//1048576} d_ops={d_ops} d_MiB={d_sec*512//1048576}")
else:
    amp = r_sec*512/user if user else 0
    print(f"AMP={amp:.3f} r_MiB={r_sec*512//1048576} rareq={req(r_ops,r_sec):.2f}MiB "
          f"r_ops={r_ops} w_MiB={w_sec*512//1048576} d_ops={d_ops} d_MiB={d_sec*512//1048576}")
EOF
)
    local elapsed; elapsed=$(echo "$t1 $t0" | awk '{print $1-$2}')
    local amp; amp=$(echo "$ampline" | grep -o 'AMP=[0-9.]*' | cut -d= -f2)
    echo "--- $tag: $mibs MiB/s (${elapsed}s)  $ampline"
    diff_stats "$RESULTS/$tag.stats.before" "$RESULTS/$tag.stats.after" "$RESULTS/$tag.stats.delta"
    echo "$row,$side,$rep,$mibs,$amp" >> "$CSV"
    # Rewrite-program SLO gate (Idea 17, design-rewrite-program §2.2):
    # a seq-overwrite row under the target classes must pay ≤1.05 device
    # amp AND zero mid-row discards. SQZ_WA_REWRITE_GATE=0 = measure only.
    if [ "$row" = seq_overwrite_1m ] && [ "${SQZ_WA_REWRITE_GATE:-1}" = 1 ]; then
        local d_ops_row; d_ops_row=$(echo "$ampline" | grep -o 'd_ops=[0-9]*' | cut -d= -f2)
        if awk -v a="$amp" 'BEGIN{exit !(a > 1.05)}'; then
            echo "  SLO GATE FAIL: $tag REWRITE_AMP=$amp > 1.05"; INVALID=1
        fi
        if [ "${d_ops_row:-0}" -ne 0 ]; then
            echo "  SLO GATE FAIL: $tag paid $d_ops_row mid-row discard ops (charter: 0)"; INVALID=1
        fi
    fi
    # Engagement (charter rule 4)
    python3 - "$RESULTS/$tag.stats.delta" "$side" "$row" "$mode" <<'EOF' || INVALID=1
import json, sys
d = json.load(open(sys.argv[1])); side, row, mode = sys.argv[2], sys.argv[3], sys.argv[4]
key = "ipc_ops_write" if mode == "w" else "ipc_ops_read"
ops = d.get(key, 0)
if side.startswith("shim") and ops == 0:
    print(f"  ENGAGEMENT INVALID: {side} row {row} served 0 ring {key}"); sys.exit(1)
if side == "kernel" and (d.get("ipc_ops_write", 0) + d.get("ipc_ops_read", 0)) != 0:
    print(f"  ENGAGEMENT INVALID: kernel row {row} shows ring ops"); sys.exit(1)
print(f"  engagement ok ({ops} ring ops)")
EOF
}

median_col() { # <row> <side> <col: 4=MiB/s 5=amp>
    awk -F, -v r="$1" -v s="$2" -v c="$3" '$1==r && $2==s {print $c}' "$CSV" | sort -n | \
        awk '{a[NR]=$1} END {if (NR) print (NR%2) ? a[(NR+1)/2] : (a[NR/2]+a[NR/2+1])/2}'
}

run_row_matrix() { # <row> <sides...>
    local row="$1"; shift
    echo "$row" | grep -Eq "$ROW_FILTER" || return 0
    dataset_files
    if [ "$row" = seq_read_1m ] || [ "$row" = seq_overwrite_1m ]; then
        # Cold striped dataset written through the KERNEL path (untimed),
        # then per-rep remount for a cold read (drops RAM tiers) /
        # per-rep full overwrite (every block displaces — the rewrite
        # steady state; settle first so prep frees never land mid-row).
        rm -f "${DATA_FILES[@]}"
        "$ELBENCHO_BIN" -w -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct \
            "${DATA_FILES[@]}" > "$RESULTS/prep.elbencho" 2>&1 || fail "dataset prep"
        settle_reclaim
    fi
    for side in "$@"; do
        for rep in $(seq 1 "$REPS"); do
            case "$row" in
                seq_write_1m)
                    rm -f "${DATA_FILES[@]}"
                    run_one "$row" "$side" "$rep" w -- \
                        -w -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct "${DATA_FILES[@]}"
                    ;;
                seq_overwrite_1m)
                    settle_reclaim
                    run_one "$row" "$side" "$rep" w -- \
                        -w -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct "${DATA_FILES[@]}"
                    ;;
                seq_read_1m)
                    kill_daemon; mount_fs; dataset_files
                    run_one "$row" "$side" "$rep" r -- \
                        -r -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct "${DATA_FILES[@]}"
                    ;;
            esac
        done
        echo "=== $row $side: median $(median_col "$row" "$side" 4) MiB/s, amp $(median_col "$row" "$side" 5)"
    done
}

# Row settle hygiene (rewrite rows): no reclaim backlog may drain into a
# measured window (the standing settle rule) — the queued reclaimer AND
# the elided-discard debt (Idea 4: the idle venue drains it between
# rows; a row must start with a zero debt gauge or the drain's tail
# lands mid-row).
settle_reclaim() {
    for _ in $(seq 90); do
        local qb
        qb=$(python3 -c "import json;m=json.load(open('$MOUNT_DIR/.stats'))['metrics'];print(int(m.get('block_free_reclaim_queue_bytes',0))+int(m.get('block_free_elided_debt_bytes',0)))" 2>/dev/null || echo 0)
        [ "${qb:-0}" -eq 0 ] && return 0
        sleep 1
    done
    echo "  WARN: reclaim queue/debt did not settle before the row"
}

# Rewrite-program loop_rewrite row (Idea 17, design §2.2 face 2):
# SQZ_WA_LOOP_PASSES full overwrites of a hot set in ONE measured window.
# Report-only latest-wins verdict: device write bytes ÷ unique-block
# bytes (the charter's ≈-unique-blocks target) + the coalesce factor and
# the supersession engagement delta.
run_loop_rewrite() { # <side>
    echo "loop_rewrite" | grep -Eq "$ROW_FILTER" || return 0
    local side="$1"
    local passes="${SQZ_WA_LOOP_PASSES:-5}"
    local files="${SQZ_WA_LOOP_FILES:-4}"
    local fmb="${SQZ_WA_LOOP_FILE_MB:-64}"
    local envp=()
    case "$side" in
        shim) envp=(env "LD_PRELOAD=$SO" "SQUEEZEFS_IPC_ALLOW_DEV=1") ;;
    esac
    mkdir -p "$MOUNT_DIR/waloop"
    local lfiles=()
    for i in $(seq 1 "$files"); do lfiles+=("$MOUNT_DIR/waloop/f$i"); done
    rm -f "${lfiles[@]}"
    # Untimed prep pass (fresh mint) + settle, so the measured window is
    # pure rewrite.
    "${envp[@]}" "$ELBENCHO_BIN" -w -t "$files" -s "${fmb}m" -b 1m --direct \
        "${lfiles[@]}" > "$RESULTS/loop_rewrite.$side.prep" 2>&1 || fail "loop prep"
    settle_reclaim
    local tag="loop_rewrite.$side"
    snap_stats "$RESULTS/$tag.stats.before"
    local disk_b; disk_b="$(snap_disk)"
    for p in $(seq 1 "$passes"); do
        "${envp[@]}" "$ELBENCHO_BIN" -w -t "$files" -s "${fmb}m" -b 1m --direct \
            "${lfiles[@]}" > "$RESULTS/$tag.pass$p.elbencho" 2>&1 || fail "loop pass $p"
    done
    local disk_a; disk_a="$(snap_disk)"
    snap_stats "$RESULTS/$tag.stats.after"
    diff_stats "$RESULTS/$tag.stats.before" "$RESULTS/$tag.stats.after" "$RESULTS/$tag.stats.delta"
    python3 - "$disk_b" "$disk_a" "$((files * fmb * 1024 * 1024))" "$passes" "$RESULTS/$tag.stats.delta" <<'EOF'
import json, sys
b = [int(x) for x in sys.argv[1].split()]; a = [int(x) for x in sys.argv[2].split()]
unique = int(sys.argv[3]); passes = int(sys.argv[4])
d = json.load(open(sys.argv[5]))
w_bytes = (a[3]-b[3])*512; d_ops = a[4]-b[4]
user = unique * passes
ratio = w_bytes/unique if unique else 0
coal = user/w_bytes if w_bytes else 0
sup = d.get("write_pipeline_supersessions", 0)
print(f"=== loop_rewrite: device_w={w_bytes//1048576}MiB over {passes} passes of "
      f"{unique//1048576}MiB unique; device/unique={ratio:.2f}x (charter target ~1), "
      f"coalesce={coal:.2f}x, mid-window d_ops={d_ops}, supersessions={sup}")
EOF
}

echo "write_amp_rig: data=$DATA_DEV meta=$META_DEV files=${FILES}x${FILE_MB}MiB results=$RESULTS"
echo "row,side,rep,mibs,amp" > "$CSV"
kill_daemon
format_fs
mount_fs

run_row_matrix seq_write_1m kernel shim shim-frag
run_row_matrix seq_overwrite_1m kernel shim
run_loop_rewrite kernel
run_loop_rewrite shim
run_row_matrix seq_read_1m kernel shim

[ "$INVALID" -eq 0 ] || fail "one or more rows INVALID (engagement/SLO gate)"
echo "write_amp_rig: done ($CSV)"
