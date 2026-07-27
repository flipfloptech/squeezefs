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
write_partial_flush_blocks write_partial_flush_bytes write_partial_reflush_blocks \
write_stream_flush_deferrals write_stream_flush_deferral_expiries \
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

snap_disk() { awk -v d="$DATA_BASE" '$3==d {print $4, $6, $8, $10}' /proc/diskstats; }

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
    RUST_LOG=info SQUEEZEFS_IPC_SERVICE_THREADS="${SQZ_WA_SERVICE_THREADS:-8}" \
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
if mode == "w":
    amp = w_sec*512/user if user else 0
    print(f"AMP={amp:.3f} w_MiB={w_sec*512//1048576} wareq={req(w_ops,w_sec):.2f}MiB "
          f"w_ops={w_ops} r_MiB={r_sec*512//1048576}")
else:
    amp = r_sec*512/user if user else 0
    print(f"AMP={amp:.3f} r_MiB={r_sec*512//1048576} rareq={req(r_ops,r_sec):.2f}MiB "
          f"r_ops={r_ops} w_MiB={w_sec*512//1048576}")
EOF
)
    local elapsed; elapsed=$(echo "$t1 $t0" | awk '{print $1-$2}')
    local amp; amp=$(echo "$ampline" | grep -o 'AMP=[0-9.]*' | cut -d= -f2)
    echo "--- $tag: $mibs MiB/s (${elapsed}s)  $ampline"
    diff_stats "$RESULTS/$tag.stats.before" "$RESULTS/$tag.stats.after" "$RESULTS/$tag.stats.delta"
    echo "$row,$side,$rep,$mibs,$amp" >> "$CSV"
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
    if [ "$row" = seq_read_1m ]; then
        # Cold striped dataset written through the KERNEL path (untimed),
        # then per-rep remount for a cold read (drops RAM tiers).
        rm -f "${DATA_FILES[@]}"
        "$ELBENCHO_BIN" -w -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct \
            "${DATA_FILES[@]}" > "$RESULTS/prep.elbencho" 2>&1 || fail "dataset prep"
    fi
    for side in "$@"; do
        for rep in $(seq 1 "$REPS"); do
            case "$row" in
                seq_write_1m)
                    rm -f "${DATA_FILES[@]}"
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

echo "write_amp_rig: data=$DATA_DEV meta=$META_DEV files=${FILES}x${FILE_MB}MiB results=$RESULTS"
echo "row,side,rep,mibs,amp" > "$CSV"
kill_daemon
format_fs
mount_fs

run_row_matrix seq_write_1m kernel shim shim-frag
run_row_matrix seq_read_1m kernel shim

[ "$INVALID" -eq 0 ] || fail "one or more rows INVALID (engagement)"
echo "write_amp_rig: done ($CSV)"
