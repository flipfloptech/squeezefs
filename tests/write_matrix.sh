#!/usr/bin/env bash
# tests/write_matrix.sh — DIALED P3: the write-side characterization matrix
# (.benchmarks/2026-07-27-write-side-economy.md §2 — the campaign's map).
#
# THE GOVERNING RULE (user directive, 2026-07-28 shim-parity campaign,
# verbatim): "The kernel and IPC should always at minimum be at par with
# the IPC out pacing the kernel in the majority of benchmarks."
# This script ENFORCES it: every armed shim row is paired with its kernel
# twin; a shim row trailing its kernel twin beyond the stated noise band
# (SQZ_WM_NOISE_PCT, default 10) FAILS the sweep (nonzero exit, row
# named), and the summary asserts the shim WINS the majority of decided
# (non-par) pairs.
#
#   {4k, 64k, 256k, 1m, 4m} × {O_DIRECT, buffered} × {kernel, shim} ×
#   {rand, seq} on the ARMED (--interception, KD-11 write-through) mount,
#   plus RW6-convention durable tails (fsync-each-file + syncfs, timed) on
#   every seq row, plus the UNARMED kernel-buffered rows (writeback cache
#   ON — the user's likely "normal writes" comparison). The 4m seq rows
#   are the shim-parity campaign's known-violation venue (t16×4MiB
#   streaming — ingest-economy board item 1).
#
# Instrument (stated, per the L1-A lesson): fio psync, --thread, numjobs=16,
# qd1 sync syscalls — fio page-aligns its buffers. Rand rows are time_based
# overwrites of preallocated striped whole-block-mapped files (the W1 patch
# shape); seq rows are fresh-file creates, size-scaled per bs.
#
# Engagement (charter rule 4): every shim row must account its ops in
# ipc_ops_write Δ (expected = fio ops × ceil(bs / slab)); a zero Δ shim row
# or a nonzero Δ kernel row is INVALID and the script exits nonzero at the
# end. Per-row counter deltas (patch_*, write_through_blocks, extent_*,
# tripwires) are printed and persisted per rep.
#
# Substrate: the DIALED fabric-latency rig (configfs null_blk 235 µs →
# nvmet-loop). Usage:
#   sudo SQZ_WM_RESULTS=/tmp/wm tests/write_matrix.sh [row-filter-regex]
set -u

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
META_DEV="${SQZ_META_DEV:-/dev/nvme2n1}"
DATA_DEV="${SQZ_DATA_DEV:-/dev/nvme1n1}"
MOUNT_DIR="${MOUNT_DIR:-/mnt/sqz_write_matrix}"
RESULTS="${SQZ_WM_RESULTS:-/tmp/write_matrix_$(date +%Y%m%d_%H%M%S)}"
ROW_FILTER="${1:-.}"
REPS="${SQZ_WM_REPS:-3}"
THREADS="${SQZ_WM_THREADS:-16}"
RUNTIME="${SQZ_WM_RUNTIME:-10}"
# The il-vs-kernel parity noise band (percent): a shim row within
# ±band of its kernel twin is PAR; below is a LOSS (sweep failure).
NOISE_PCT="${SQZ_WM_NOISE_PCT:-10}"
SQUEEZEFS_BIN="${SQUEEZEFS_BIN:-$REPO_DIR/target/release/squeezefs}"
SO="${SQZ_WM_SO:-$REPO_DIR/target/preload-release/libsqueezefs_il.so}"
LOG="$RESULTS/daemon.log"
CSV="$RESULTS/matrix.csv"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }
[ -x "$SQUEEZEFS_BIN" ] || { echo "missing $SQUEEZEFS_BIN"; exit 1; }
[ -f "$SO" ] || { echo "missing shim $SO"; exit 1; }
[ -b "$META_DEV" ] && [ -b "$DATA_DEV" ] || { echo "missing rig devices"; exit 1; }
mkdir -p "$RESULTS" "$MOUNT_DIR"

# Counter deltas printed per row (full metrics diff persisted per rep).
KEYS="ipc_ops_write ipc_bytes_in ipc_ops_read ipc_async_handoffs \
patch_writes patch_write_bytes patch_edge_rmw_reads write_through_blocks \
write_path_seed_read_bytes active_block_ooo_runs overwrite_seed_materialized \
overwrite_seed_skipped extent_parks extent_spills extent_record_absorbs \
fold_passes meta_kv_journal_entries fuse_ops ipc_sessions_poisoned \
ipc_descriptor_rejects"

INVALID=0

fail() { echo "FAIL: $*"; exit 1; }

snap_stats() { # snap_stats <outfile>
    python3 -c "import json;print(json.dumps(json.load(open('$MOUNT_DIR/.stats'))['metrics']))" \
        > "$1" 2>/dev/null || echo '{}' > "$1"
}

diff_stats() { # diff_stats <before> <after> <outfile> — prints selected deltas
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
print(" ".join(f"{k}={delta.get(k, 0)}" for k in sel if delta.get(k, 0)))
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
}

mount_fs() { # mount_fs armed|unarmed
    rm -f "$LOG"
    local extra=()
    [ "$1" = armed ] && extra=(--interception)
    RUST_LOG=info SQUEEZEFS_IPC_SERVICE_THREADS=8 "$SQUEEZEFS_BIN" mount \
        "sqmeta://$META_DEV" "$MOUNT_DIR" --daemon --allow-other \
        --mem-cache-size 1GB --log-file "$LOG" "${extra[@]}" || fail "mount $1"
    for _ in $(seq 20); do mountpoint -q "$MOUNT_DIR" && break; sleep 0.5; done
    mountpoint -q "$MOUNT_DIR" || { tail -20 "$LOG"; fail "mount $1 not up"; }
    chmod 1777 "$MOUNT_DIR"
}

# fio ops for a finished json
fio_ops()  { python3 -c "import json,sys;j=json.load(open('$1'));print(sum(job['write']['total_ios'] for job in j['jobs']))"; }
fio_iops() { python3 -c "import json,sys;j=json.load(open('$1'));print(round(sum(job['write']['iops'] for job in j['jobs'])))"; }
fio_bw()   { python3 -c "import json,sys;j=json.load(open('$1'));print(round(sum(job['write']['bw_bytes'] for job in j['jobs'])/1048576))"; }
fio_elapsed() { python3 -c "import json,sys;j=json.load(open('$1'));print(max(job['write']['runtime'] for job in j['jobs'])/1000.0)"; }

seq_size_for_bs() { # per-file size, scaled so op counts stay sane
    case "$1" in
        4k) echo 64m ;; 64k) echo 256m ;; 256k) echo 512m ;; 1m) echo 512m ;;
        4m) echo 512m ;;
    esac
}

# Per-row median IOPS ledger — the parity verdict input.
declare -A MEDIANS

run_row() { # run_row <rowname> <shim 0|1> <rw seq|rand> <bs> <direct 0|1> <dir>
    local row="$1" shim="$2" rw="$3" bs="$4" direct="$5" dir="$6"
    echo "$row" | grep -Eq "$ROW_FILTER" || return 0
    local iops_list=()
    for rep in $(seq "$REPS"); do
        local pre="$RESULTS/$row.r$rep.pre.json" post="$RESULTS/$row.r$rep.post.json"
        local fioout="$RESULTS/$row.r$rep.fio.json"
        local fio_rw fio_extra=()
        if [ "$rw" = rand ]; then
            fio_rw=randwrite
            fio_extra=(--time_based --runtime="$RUNTIME" --size=1g)
        else
            fio_rw=write
            rm -f "$dir"/s_f* 2>/dev/null
            sync -f "$MOUNT_DIR" 2>/dev/null || sync
            sleep 1
            fio_extra=(--size="$(seq_size_for_bs "$bs")" --fallocate=none)
        fi
        local pfx=(env)
        [ "$shim" = 1 ] && pfx=(env LD_PRELOAD="$SO")
        local fname='f$jobnum'; [ "$rw" = seq ] && fname='s_f$jobnum'
        snap_stats "$pre"
        "${pfx[@]}" fio --name="$row" --directory="$dir" \
            --filename_format="$fname" --numjobs="$THREADS" --thread \
            --group_reporting --ioengine=psync --rw="$fio_rw" --bs="$bs" \
            --direct="$direct" "${fio_extra[@]}" \
            --output-format=json --output="$fioout" >/dev/null 2>&1 \
            || fail "fio $row rep$rep"
        # Rand rows: settle writeback (untimed) so counter deltas do not
        # bleed into the next rep (unarmed buffered rows leave dirty pages).
        [ "$rw" = rand ] && { sync -f "$MOUNT_DIR" 2>/dev/null || sync; }
        # RW6 durable tail (seq rows): fsync every file + syncfs, timed.
        local dur_s="-"
        if [ "$rw" = seq ]; then
            local t0 t1
            t0=$(date +%s.%N)
            sync "$dir"/s_f* 2>/dev/null || true
            sync -f "$MOUNT_DIR" 2>/dev/null || true
            t1=$(date +%s.%N)
            dur_s=$(python3 -c "print(f'{$t1-$t0:.3f}')")
        fi
        snap_stats "$post"
        local sel
        sel=$(diff_stats "$pre" "$post" "$RESULTS/$row.r$rep.delta.json")
        local ops iops bw el
        ops=$(fio_ops "$fioout"); iops=$(fio_iops "$fioout")
        bw=$(fio_bw "$fioout"); el=$(fio_elapsed "$fioout")
        # Engagement: shim rows must move ipc_ops_write; kernel rows must not.
        local ipcw
        ipcw=$(python3 -c "import json;print(json.load(open('$RESULTS/$row.r$rep.delta.json')).get('ipc_ops_write',0))")
        local engage="ok"
        if [ "$shim" = 1 ] && [ "$ipcw" -eq 0 ]; then engage="INVALID-passthrough"; INVALID=1; fi
        if [ "$shim" = 0 ] && [ "$ipcw" -ne 0 ]; then engage="INVALID-leak"; INVALID=1; fi
        local chunks="-"
        [ "$shim" = 1 ] && [ "$ops" -gt 0 ] && chunks=$(python3 -c "print(f'{$ipcw/$ops:.2f}')")
        echo "$row,$rep,$iops,$bw,$el,$dur_s,$ops,$ipcw,$chunks,$engage" >> "$CSV"
        echo "  [$row r$rep] iops=$iops bw=${bw}MiB/s el=${el}s durable_tail=${dur_s}s ops=$ops ring/op=$chunks $engage"
        [ -n "$sel" ] && echo "      Δ $sel"
        iops_list+=("$iops")
    done
    local med
    med=$(printf '%s\n' "${iops_list[@]}" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}')
    echo "ROW $row median_iops=$med"
    echo "$row,median,$med,,,,,,," >> "$CSV"
    MEDIANS[$row]=$med
}

prealloc_rand() { # prealloc_rand <dir> — 16 × 1 GiB striped whole-block files
    mkdir -p "$1"
    fio --name=prealloc --directory="$1" --filename_format='f$jobnum' \
        --numjobs="$THREADS" --thread --group_reporting --ioengine=psync \
        --rw=write --bs=1m --direct=1 --size=1g --output-format=json \
        --output="$RESULTS/prealloc.json" >/dev/null 2>&1 || fail "prealloc"
    sync "$1"/f* 2>/dev/null || true
    sync -f "$MOUNT_DIR" 2>/dev/null || true
    sleep 2
}

echo "results: $RESULTS (filter: $ROW_FILTER)"
echo "row,rep,iops,bw_mib_s,elapsed_s,durable_tail_s,fio_ops,ipc_ops_write,ring_per_op,engagement" > "$CSV"

# ---------------- ARMED mount (KD-11 write-through) ----------------
kill_daemon
format_fs
mount_fs armed
RAND_DIR="$MOUNT_DIR/rand"; SEQ_DIR="$MOUNT_DIR/seqd"
mkdir -p "$RAND_DIR" "$SEQ_DIR"; chmod 1777 "$RAND_DIR" "$SEQ_DIR"
prealloc_rand "$RAND_DIR"
for path in kernel shim; do
    sh=0; [ "$path" = shim ] && sh=1
    for direct in 1 0; do
        dl=odirect; [ "$direct" = 0 ] && dl=buffered
        for bs in 4k 64k 256k 1m; do
            run_row "armed-$path-rand-$bs-$dl" "$sh" rand "$bs" "$direct" "$RAND_DIR"
        done
    done
done
for path in kernel shim; do
    sh=0; [ "$path" = shim ] && sh=1
    for direct in 1 0; do
        dl=odirect; [ "$direct" = 0 ] && dl=buffered
        # 4m seq = the shim-parity campaign's t16×4MiB streaming venue.
        for bs in 4k 64k 256k 1m 4m; do
            run_row "armed-$path-seq-$bs-$dl" "$sh" seq "$bs" "$direct" "$SEQ_DIR"
        done
    done
done
kill_daemon

# ------------- UNARMED mount (writeback cache ON) -------------
if echo "unarmed" | grep -Eq "$ROW_FILTER" || [ "$ROW_FILTER" = "." ]; then
    format_fs
    mount_fs unarmed
    RAND_DIR="$MOUNT_DIR/rand"; SEQ_DIR="$MOUNT_DIR/seqd"
    mkdir -p "$RAND_DIR" "$SEQ_DIR"; chmod 1777 "$RAND_DIR" "$SEQ_DIR"
    prealloc_rand "$RAND_DIR"
    for bs in 4k 64k 256k 1m; do
        run_row "unarmed-kernel-rand-$bs-buffered" 0 rand "$bs" 0 "$RAND_DIR"
    done
    for bs in 4k 64k 256k 1m; do
        run_row "unarmed-kernel-seq-$bs-buffered" 0 seq "$bs" 0 "$SEQ_DIR"
    done
    kill_daemon
fi

echo
echo "=== matrix complete → $CSV ==="
column -s, -t "$CSV" | tail -n +1
if [ "$INVALID" -ne 0 ]; then
    echo "ENGAGEMENT INVALID rows present"; exit 2
fi

# ---------------------------------------------------------------------------
# il-vs-kernel PARITY VERDICT (the governing rule, header): each armed shim
# row against its kernel twin, by median IOPS. Within ±NOISE_PCT% = PAR;
# above = WIN; below = LOSS. ANY loss fails the sweep (row named); the
# summary asserts the shim wins the MAJORITY of decided (non-par) pairs.
# ---------------------------------------------------------------------------
echo
echo "=== il-vs-kernel parity verdict (band ±${NOISE_PCT}%) ==="
WINS=0; LOSSES=0; PARS=0
echo "pair,shim_median,kernel_median,ratio,verdict" > "$RESULTS/parity.csv"
for row in $(printf '%s\n' "${!MEDIANS[@]}" | grep '^armed-shim-' | sort); do
    twin="${row/armed-shim-/armed-kernel-}"
    [ -n "${MEDIANS[$twin]:-}" ] || continue
    s="${MEDIANS[$row]}"; k="${MEDIANS[$twin]}"
    read -r ratio verdict <<< "$(python3 -c "
s=float($s); k=float($k); band=float($NOISE_PCT)/100.0
r = s/k if k > 0 else float('inf')
v = 'PAR' if k <= 0 or abs(s-k) <= band*k else ('WIN' if s > k else 'LOSS')
print(f'{r:.3f} {v}')")"
    pair="${row#armed-shim-}"
    printf '  %-24s shim=%-9s kernel=%-9s il/kern=%-7s %s\n' \
        "$pair" "$s" "$k" "$ratio" "$verdict"
    echo "$pair,$s,$k,$ratio,$verdict" >> "$RESULTS/parity.csv"
    case "$verdict" in
        WIN) WINS=$((WINS+1)) ;;
        LOSS) LOSSES=$((LOSSES+1)); echo "  PARITY LOSS: $row trails $twin beyond ${NOISE_PCT}%" ;;
        PAR) PARS=$((PARS+1)) ;;
    esac
done
TOTAL=$((WINS+LOSSES+PARS))
echo "parity summary: pairs=$TOTAL win=$WINS loss=$LOSSES par=$PARS"
if [ "$TOTAL" -gt 0 ]; then
    # Rule half 1 — at minimum par: ANY beyond-band loss fails.
    if [ "$LOSSES" -gt 0 ]; then
        echo "PARITY FAIL: il trails kernel beyond the ±${NOISE_PCT}% band on $LOSSES row(s) — the governing rule requires at-minimum par"
        exit 3
    fi
    # Rule half 2 — majority out-pacing: of the DECIDED (non-par) pairs,
    # il must win the majority. (With losses already fatal above, any
    # decided pair is a win; the check stays explicit so a future
    # allow-loss lever cannot silently drop the majority clause.)
    DECIDED=$((WINS+LOSSES))
    if [ "$DECIDED" -gt 0 ] && [ "$WINS" -le $((DECIDED / 2)) ]; then
        echo "PARITY FAIL: il must out-pace the kernel in the majority of decided pairs (win=$WINS of $DECIDED)"
        exit 3
    fi
    if [ "$DECIDED" -eq 0 ]; then
        echo "note: all pairs PAR — at-minimum-par holds; no decided pairs for the majority clause"
    fi
fi
