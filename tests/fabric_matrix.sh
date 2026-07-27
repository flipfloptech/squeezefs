#!/usr/bin/env bash
# tests/fabric_matrix.sh — fabric-divergence reproduction matrix
# (.benchmarks/2026-07-27-fabric-divergence.md): the three field-divergent
# rows (relaxed seq write 1m / seq read 1m / rand read 4k IOPS), kernel vs
# shim, on a REAL-fabric substrate (nvmet-tcp or the nvmet-loop rig),
# with the two mechanism instruments the loop rig cannot see:
#
#   (a) device-I/O fragmentation — per-row /proc/diskstats deltas on the
#       DATA namespace (ops, sectors, avg op size) + the daemon's own
#       ranged/write-through counters;
#   (b) client CPU contention — per-row daemon CPU (utime+stime delta,
#       per-thread-comm breakdown), elbencho CPU, and /proc/softirqs +
#       aggregate mpstat deltas (nvme-tcp softirq work shares the cores).
#
# Instrument (stated): elbencho (dynamic, shim-loadable), --direct rows
# exactly as the scoreboard battery spells them (seq: -t 16 -b 1m; rand:
# -t 8 -b 4k --timelimit). Engagement per row from .stats deltas; a shim
# row is INVALID unless ipc_ops_* accounts for its ops.
#
# Usage:
#   sudo SQZ_META_DEV=/dev/nvme4n1 SQZ_DATA_DEV=/dev/nvme3n1 \
#        SQZ_FM_RESULTS=/tmp/fm tests/fabric_matrix.sh [row-filter-regex]
set -u

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
META_DEV="${SQZ_META_DEV:-/dev/nvme4n1}"
DATA_DEV="${SQZ_DATA_DEV:-/dev/nvme3n1}"
MOUNT_DIR="${MOUNT_DIR:-/mnt/sqz_fabric_matrix}"
RESULTS="${SQZ_FM_RESULTS:-/tmp/fabric_matrix_$(date +%Y%m%d_%H%M%S)}"
ROW_FILTER="${1:-.}"
REPS="${SQZ_FM_REPS:-3}"
THREADS="${SQZ_FM_THREADS:-16}"
RAND_T="${SQZ_FM_RAND_T:-8}"
TIMELIMIT="${SQZ_FM_TIMELIMIT:-20}"
FILES="${SQZ_FM_FILES:-16}"
FILE_MB="${SQZ_FM_FILE_MB:-1024}"
SQUEEZEFS_BIN="${SQUEEZEFS_BIN:-$REPO_DIR/target/release/squeezefs}"
SO="${SQZ_FM_SO:-$REPO_DIR/target/preload-release/libsqueezefs_il.so}"
ELBENCHO_BIN="${ELBENCHO_BIN:-$(command -v elbencho)}"
LOG="$RESULTS/daemon.log"
CSV="$RESULTS/matrix.csv"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }
[ -x "$SQUEEZEFS_BIN" ] || { echo "missing $SQUEEZEFS_BIN"; exit 1; }
[ -f "$SO" ] || { echo "missing shim $SO"; exit 1; }
[ -b "$META_DEV" ] && [ -b "$DATA_DEV" ] || { echo "missing devices"; exit 1; }
mkdir -p "$RESULTS" "$MOUNT_DIR"

DATA_BASE="$(basename "$DATA_DEV")"

KEYS="ipc_ops_read ipc_ops_write ipc_bytes_in ipc_bytes_out \
ipc_fast_path_serves ipc_async_handoffs ipc_direct_drive_serves \
ipc_direct_ineligible_policy ranged_reads ranged_read_bytes \
ranged_read_ghost_escalations read_tier_admissions write_through_blocks \
write_path_seed_read_bytes patch_writes prefetch_issued prefetch_completed \
hot_block_hits hot_block_misses ipc_descriptor_rejects ipc_sessions_poisoned"

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
diff_disk() { # <before-str> <after-str> — prints r_ops r_MiB r_avg_kib w_ops w_MiB w_avg_kib
    python3 - "$1" "$2" <<'EOF'
import sys
b = [int(x) for x in sys.argv[1].split()]
a = [int(x) for x in sys.argv[2].split()]
r_ops, r_sec = a[0]-b[0], a[1]-b[1]
w_ops, w_sec = a[2]-b[2], a[3]-b[3]
def avg(ops, sec): return (sec*512/ops/1024) if ops else 0
print(f"  device: r_ops={r_ops} r_MiB={r_sec*512//1048576} r_avg={avg(r_ops,r_sec):.0f}KiB "
      f"w_ops={w_ops} w_MiB={w_sec*512//1048576} w_avg={avg(w_ops,w_sec):.0f}KiB")
EOF
}

snap_softirq() { grep -E "NET_RX|NET_TX|TASKLET|BLOCK" /proc/softirqs | awk '{s=0; for(i=2;i<=NF;i++) s+=$i; print $1, s}'; }
diff_softirq() { # <before-file> <after-file>
    paste "$1" "$2" | awk '{printf "%s%s ", $1, $4-$2} END {print ""}' | sed 's/^/  softirq: /'
}

cpu_of() { # <pid> — utime+stime in ticks
    awk '{print $14+$15}' "/proc/$1/stat" 2>/dev/null || echo 0
}

thread_cpu() { # <pid> <outfile> — per-comm tick totals
    local pid="$1"
    for t in /proc/"$pid"/task/*/; do
        awk -F'[()]' '{split($3,f," "); print $2, f[12]+f[13]}' "$t/stat" 2>/dev/null
    done | awk '{s[$1]+=$2} END {for (c in s) print c, s[c]}' | sort > "$2"
}

diff_thread_cpu() { # <before> <after> <elapsed-s> — top comms by CPU
    python3 - "$1" "$2" "$3" <<'EOF'
import sys
hz = 100
def load(p):
    d = {}
    for line in open(p):
        parts = line.rsplit(None, 1)
        if len(parts) == 2:
            d[parts[0]] = d.get(parts[0], 0) + int(parts[1])
    return d
b, a, el = load(sys.argv[1]), load(sys.argv[2]), float(sys.argv[3])
rows = sorted(((a.get(k,0)-b.get(k,0), k) for k in a), reverse=True)
tot = sum(r[0] for r in rows)
print(f"  daemon-cpu: total={tot/hz/el*100:.0f}%cores  " +
      " ".join(f"{k}={d/hz/el*100:.0f}%" for d, k in rows[:8] if d > 0))
EOF
}

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
    sleep 1 # flock settle: format's writer claim must drop before mount
}

mount_fs() {
    rm -f "$LOG"
    RUST_LOG=info SQUEEZEFS_IPC_SERVICE_THREADS="${SQZ_FM_SERVICE_THREADS:-8}" \
        ${SQZ_FM_DAEMON_ENV:-} "$SQUEEZEFS_BIN" mount \
        "sqmeta://$META_DEV" "$MOUNT_DIR" --daemon --allow-other --interception \
        --mem-cache-size "${SQZ_FM_CACHE:-1GB}" --log-file "$LOG" || fail "mount"
    for _ in $(seq 20); do mountpoint -q "$MOUNT_DIR" && break; sleep 0.5; done
    mountpoint -q "$MOUNT_DIR" || { tail -20 "$LOG"; fail "mount not up"; }
    chmod 1777 "$MOUNT_DIR"
    SQZ_PID="$(pidof squeezefs | awk '{print $1}')"
}

el_value() { # <logfile> <col: MiB/s|IOPS> — LAST/total value
    awk -v want="$2" '
        $1=="Throughput" && $2=="MiB/s" && want=="MiB/s" {v=$NF}
        $1=="IOPS" && want=="IOPS" {v=$NF}
        END {print v+0}' "$1"
}

dataset_files() {
    DATA_FILES=()
    for i in $(seq 1 "$FILES"); do DATA_FILES+=("$MOUNT_DIR/sbdata/f$i"); done
    mkdir -p "$MOUNT_DIR/sbdata"
}

run_one() { # <row> <side kernel|shim> <rep> -- <elbencho args...>
    local row="$1" side="$2" rep="$3"; shift 4
    local tag="$row.$side.r$rep"
    local envp=()
    [ "$side" = shim ] && envp=(env "LD_PRELOAD=$SO" "SQUEEZEFS_IPC_ALLOW_DEV=1")
    snap_stats "$RESULTS/$tag.stats.before"
    local disk_b; disk_b="$(snap_disk)"
    snap_softirq > "$RESULTS/$tag.sirq.before"
    thread_cpu "$SQZ_PID" "$RESULTS/$tag.tcpu.before"
    local cpu_b t0 t1
    cpu_b=$(cpu_of "$SQZ_PID")
    t0=$(date +%s.%N)
    "${envp[@]}" "$ELBENCHO_BIN" "$@" > "$RESULTS/$tag.elbencho" 2>&1 \
        || { cat "$RESULTS/$tag.elbencho"; fail "elbencho $tag"; }
    t1=$(date +%s.%N)
    local elapsed; elapsed=$(echo "$t1 $t0" | awk '{print $1-$2}')
    local cpu_a; cpu_a=$(cpu_of "$SQZ_PID")
    thread_cpu "$SQZ_PID" "$RESULTS/$tag.tcpu.after"
    snap_softirq > "$RESULTS/$tag.sirq.after"
    local disk_a; disk_a="$(snap_disk)"
    snap_stats "$RESULTS/$tag.stats.after"
    local val unit
    case "$row" in
        rand_read_4k) val=$(el_value "$RESULTS/$tag.elbencho" IOPS); unit=IOPS ;;
        *) val=$(el_value "$RESULTS/$tag.elbencho" "MiB/s"); unit="MiB/s" ;;
    esac
    echo "--- $tag: $val $unit (elapsed ${elapsed}s)"
    diff_disk "$disk_b" "$disk_a"
    diff_softirq "$RESULTS/$tag.sirq.before" "$RESULTS/$tag.sirq.after"
    diff_thread_cpu "$RESULTS/$tag.tcpu.before" "$RESULTS/$tag.tcpu.after" "$elapsed"
    diff_stats "$RESULTS/$tag.stats.before" "$RESULTS/$tag.stats.after" "$RESULTS/$tag.stats.delta"
    echo "$row,$side,$rep,$val,$unit,$elapsed" >> "$CSV"
    # Engagement (charter rule 4): shim rows account ops; kernel rows Δ=0.
    python3 - "$RESULTS/$tag.stats.delta" "$side" "$row" <<'EOF' || INVALID=1
import json, sys
d = json.load(open(sys.argv[1])); side, row = sys.argv[2], sys.argv[3]
ops = d.get("ipc_ops_read", 0) + d.get("ipc_ops_write", 0)
if side == "shim" and ops == 0:
    print(f"  ENGAGEMENT INVALID: shim row {row} served 0 ring ops"); sys.exit(1)
if side == "kernel" and ops != 0:
    print(f"  ENGAGEMENT INVALID: kernel row {row} shows ring ops {ops}"); sys.exit(1)
print(f"  engagement ok ({ops} ring ops)")
EOF
}

median_of() { # <row> <side>
    awk -F, -v r="$1" -v s="$2" '$1==r && $2==s {print $4}' "$CSV" | sort -n | \
        awk '{a[NR]=$1} END {print (NR%2) ? a[(NR+1)/2] : (a[NR/2]+a[NR/2+1])/2}'
}

run_row_matrix() { # <row> -- builds per-side reps
    local row="$1"
    echo "$row" | grep -Eq "$ROW_FILTER" || return 0
    dataset_files
    for side in kernel shim; do
        for rep in $(seq 1 "$REPS"); do
            case "$row" in
                seq_write_1m)
                    rm -f "${DATA_FILES[@]}"
                    run_one "$row" "$side" "$rep" -- \
                        -w -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct "${DATA_FILES[@]}"
                    ;;
                seq_read_1m)
                    run_one "$row" "$side" "$rep" -- \
                        -r -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct "${DATA_FILES[@]}"
                    ;;
                rand_read_4k)
                    run_one "$row" "$side" "$rep" -- \
                        -r --rand -t "$RAND_T" -b 4k --direct \
                        --timelimit "$TIMELIMIT" "${DATA_FILES[@]}"
                    ;;
            esac
        done
    done
    local mk ms
    mk=$(median_of "$row" kernel); ms=$(median_of "$row" shim)
    python3 -c "print(f'=== $row median: kernel=$mk shim=$ms ratio={$ms/$mk if $mk else 0:.2f}x')"
}

echo "fabric_matrix: data=$DATA_DEV meta=$META_DEV results=$RESULTS"
echo "row,side,rep,value,unit,elapsed" > "$CSV"
kill_daemon
format_fs
mount_fs

# Dataset prep (untimed) when only read rows are selected: cold striped
# dataset written through the KERNEL path.
if ! echo "seq_write_1m" | grep -Eq "$ROW_FILTER"; then
    dataset_files
    "$ELBENCHO_BIN" -w -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct \
        "${DATA_FILES[@]}" > "$RESULTS/prep.elbencho" 2>&1 || fail "dataset prep"
fi

run_row_matrix seq_write_1m
run_row_matrix seq_read_1m
run_row_matrix rand_read_4k

echo "results in $RESULTS"
[ "$INVALID" -eq 0 ] || fail "one or more rows INVALID (engagement)"
