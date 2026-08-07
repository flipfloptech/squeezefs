#!/usr/bin/env bash
# tests/copy_census_rig.sh — the near-zero-copy campaign's COPY CENSUS
# instrument (2026-07-31, `.benchmarks/2026-07-31-near-zero-copy.md`).
#
# Produces, per path (kernel write, shim write, kernel read, shim read),
# the measured memory-traffic-per-payload-byte proxies that back the
# campaign's copy ledger, plus the engagement instruments for the two
# cost levers (NT-store DMA-destined copies → `nt_copy_bytes`; session
# arena THP → smaps_rollup ShmemPmdMapped / ShmemHugePages):
#
#   * fio bw/iops per row (sustained, time_based)
#   * per-row `perf stat` on the DAEMON (task-clock, cycles,
#     instructions, cache-references, cache-misses, dTLB-load-misses,
#     minor-faults) and the same wrapped around FIO (the client-side
#     copy for shim rows lives in the app threads)
#   * stats-inode deltas: nt_copy_bytes, ipc_placed_severs/adoptions/
#     merge_elides, ipc_bytes_in/out, write_through_bytes,
#     write_path_seed_read_bytes (must-stay-0 tripwire)
#   * /proc/diskstats data-namespace read/write bytes (amplification
#     columns per the standing write-row requirement)
#   * daemon smaps_rollup THP totals (ShmemPmdMapped etc.)
#
# INSTRUMENT HONESTY (stated): this host (AMD Strix Halo) exposes no
# uncore/IMC DRAM counters to perf; `cache-misses` is the core-PMU LLC
# miss proxy (hardware-prefetch traffic is undercounted). The ledger's
# per-byte traffic numbers are therefore CODE-DERIVED copy counts
# cross-checked against these proxies and against throughput ratios —
# never presented as measured DRAM bytes.
#
# FIO ENGINE POLICY (user ruling 2026-08-07 — the matched-instrument law;
# `.benchmarks/2026-08-07-fio-engine-policy.md`): throughput/IOPS rows
# are libaio+direct+stated-qd both lanes; A/Bs use the SAME engine both
# sides; psync survives only as labeled sync-lane coverage.
#
# SYNC-LANE COVERAGE RIG — psync by design, matched engine on every arm:
# the census's MEASURAND is the sync-lane copy machinery itself (the
# §5.5.2 ring-write sever + §5.5.1 arena completion serves — the two
# DMA-destined copy sites the NT-store lever rides), so every row runs
# fio psync 1m through the same client lane on both A/B arms. These are
# copy-ledger instrument rows, NEVER headline throughput numbers (the
# ledger's per-byte numbers are code-derived counts cross-checked
# against the proxies below).
#
# Substrate: the TCP devsub (`SQZ_DEVSUB_TRANSPORT=tcp
# tests/dev_substrate.sh create`) — the fabric-sensitive venue (two-
# substrate rule; write rows are INVALID on loop). Namespace discovery
# is by subsysnqn (devsubtcp-mds*/-oss*).
#
# Usage (one side per invocation; the A-B-B-A orchestration alternates
# invocations per the standing aging-store rule):
#   sudo SQZ_CC_LABEL=A SQZ_CC_BIN=target/release/squeezefs \
#        SQZ_CC_SHIM=target/preload-release/libsqueezefs_il.so \
#        [SQZ_CC_NT=1] [SQZ_CC_THP=1] [SQZ_CC_RESULTS=dir] \
#        tests/copy_census_rig.sh [rowfilter]
set -u

LABEL="${SQZ_CC_LABEL:?}"
BIN="${SQZ_CC_BIN:?}"
SHIM="${SQZ_CC_SHIM:?}"
NT="${SQZ_CC_NT:-1}"
THP="${SQZ_CC_THP:-1}"
MOUNT_DIR="${MOUNT_DIR:-/mnt/sqz_census}"
RESULTS="${SQZ_CC_RESULTS:-/tmp/copy_census_$(date +%Y%m%d_%H%M%S)}"
REPS="${SQZ_CC_REPS:-2}"
RUNTIME="${SQZ_CC_RUNTIME:-30}"
THREADS="${SQZ_CC_THREADS:-8}"
ROW_FILTER="${1:-.}"
PERF_EVENTS="task-clock,cycles,instructions,cache-references,cache-misses,dTLB-load-misses,minor-faults"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }
for f in "$BIN" "$SHIM"; do [ -e "$f" ] || { echo "missing $f"; exit 1; }; done
mkdir -p "$RESULTS" "$MOUNT_DIR"
CSV="$RESULTS/rows.csv"
[ -s "$CSV" ] || echo "label,row,rep,bw_mib_s,iops,user_bytes,dmn_ms,dmn_cyc,dmn_llcm,dmn_dtlbm,dmn_mflt,fio_ms,fio_cyc,fio_llcm,fio_dtlbm,nt_bytes,severs,elides,ipc_in,ipc_out,dev_w,dev_r,shmem_pmd_kb,engage" > "$CSV"

fail() { echo "FAIL: $*"; exit 1; }

# ---- substrate discovery (TCP devsub namespaces, by subsysnqn) -------
META_DEVS=(); DATA_DEVS=()
for c in /sys/class/nvme/nvme*; do
    nqn=$(cat "$c/subsysnqn" 2>/dev/null) || continue
    dev="/dev/$(basename "$c")n1"
    [ -b "$dev" ] || continue
    case "$nqn" in
        *devsubtcp*-mds*) META_DEVS+=("$dev") ;;
        *devsubtcp*-oss*) DATA_DEVS+=("$dev") ;;
    esac
done
[ "${#META_DEVS[@]}" -ge 1 ] && [ "${#DATA_DEVS[@]}" -ge 1 ] || \
    fail "TCP devsub not found — create it: sudo SQZ_DEVSUB_TRANSPORT=tcp tests/dev_substrate.sh create"
IFS=,; META_URI="sqmeta://${META_DEVS[*]}"; DATA_URI="sqdata://${DATA_DEVS[*]}"; unset IFS
echo "[census] meta=$META_URI data=$DATA_URI results=$RESULTS"

kill_daemon() {
    umount "$MOUNT_DIR" 2>/dev/null || umount -l "$MOUNT_DIR" 2>/dev/null || true
    for _ in $(seq 30); do pgrep -f "$BIN mount" >/dev/null || break; sleep 0.5; done
    pkill -9 -f "$BIN mount" 2>/dev/null || true
    sleep 1
}
trap kill_daemon EXIT

snap_stats() {
    python3 -c "import json;print(json.dumps(json.load(open('$MOUNT_DIR/.stats'))['metrics']))" \
        > "$1" 2>/dev/null || echo '{}' > "$1"
}

dev_bytes() { # dev_bytes w|r → summed bytes on the data namespaces
    local which="$1" total=0 f
    [ "$which" = w ] && f=10 || f=6
    for d in "${DATA_DEVS[@]}"; do
        local name=${d#/dev/} sect
        sect=$(awk -v n="$name" -v f="$f" '$3==n{print $f}' /proc/diskstats)
        total=$((total + ${sect:-0} * 512))
    done
    echo "$total"
}

daemon_pid() { pgrep -f "$BIN mount" | head -1; }

thp_kb_now() { # daemon shmem PMD-mapped KiB (arena THP engagement)
    local pid; pid=$(daemon_pid)
    awk '/^ShmemPmdMapped:/{print $2; found=1} END{if(!found)print 0}' \
        "/proc/$pid/smaps_rollup" 2>/dev/null | head -1
}

thp_sampler() { # max ShmemPmdMapped observed while a row runs (sessions
                # are reaped at client exit, so post-row reads see 0)
    local out="$1" max=0 v
    : > "$out"
    while :; do
        v=$(thp_kb_now); [ "${v:-0}" -gt "$max" ] && { max=$v; echo "$max" > "$out"; }
        sleep 1
    done
}

mount_side() { # fresh format + interception mount with the side's levers
    kill_daemon
    for d in "${DATA_DEVS[@]}" "${META_DEVS[@]}"; do blkdiscard -f "$d" 2>/dev/null || true; done
    "$BIN" format "$META_URI" "$DATA_URI" --force >> "$RESULTS/format.log" 2>&1 || fail "format"
    sleep 1
    for attempt in 1 2; do
        RUST_LOG=warn SQUEEZEFS_NT_COPY="$NT" SQUEEZEFS_IPC_ARENA_THP="$THP" \
            "$BIN" mount "$META_URI" "$MOUNT_DIR" --daemon --allow-other \
            --interception --log-file "$RESULTS/daemon-$LABEL.log" && break
        [ "$attempt" = 2 ] && fail "mount"
        sleep 3
    done
    for _ in $(seq 40); do mountpoint -q "$MOUNT_DIR" && break; sleep 0.5; done
    mountpoint -q "$MOUNT_DIR" || fail "mount not up"
    chmod 1777 "$MOUNT_DIR"
}

remount_only() { # tier-cold remount (keeps on-device data)
    umount "$MOUNT_DIR" 2>/dev/null || umount -l "$MOUNT_DIR" || true
    for _ in $(seq 30); do pgrep -f "$BIN mount" >/dev/null || break; sleep 0.5; done
    RUST_LOG=warn SQUEEZEFS_NT_COPY="$NT" SQUEEZEFS_IPC_ARENA_THP="$THP" \
        "$BIN" mount "$META_URI" "$MOUNT_DIR" --daemon --allow-other \
        --interception --log-file "$RESULTS/daemon-$LABEL.log" || fail "remount"
    for _ in $(seq 40); do mountpoint -q "$MOUNT_DIR" && break; sleep 0.5; done
    chmod 1777 "$MOUNT_DIR"
}

row() { # row <name> <shim01> <rw> <fresh|keep|cold> [extra fio args...]
    local name="$1" shim="$2" rw="$3" mode="$4"; shift 4
    echo "$name" | grep -Eq "$ROW_FILTER" || return 0
    local dir="$MOUNT_DIR/census"; mkdir -p "$dir"; chmod 1777 "$dir"
    for rep in $(seq "$REPS"); do
        [ "$mode" = fresh ] && rm -f "$dir"/f* 2>/dev/null
        [ "$mode" = cold ] && remount_only
        sync -f "$MOUNT_DIR" 2>/dev/null || sync; sleep 2
        local base="$RESULTS/$LABEL.$name.r$rep"
        local pfx=(env)
        [ "$shim" = 1 ] && pfx=(env LD_PRELOAD="$SHIM" SQUEEZEFS_IPC_ARENA_THP="$THP")
        local dw0 dr0 dpid
        dw0=$(dev_bytes w); dr0=$(dev_bytes r); dpid=$(daemon_pid)
        snap_stats "$base.pre.json"
        thp_sampler "$base.thp" & local thppid=$!
        # Daemon-attached perf for exactly the row window.
        perf stat -p "$dpid" -e "$PERF_EVENTS" -o "$base.dmn.perf" &
        local perfpid=$!
        sleep 0.2
        # SYNC-LANE COVERAGE ROW — psync by design (header): the §5.5.1/
        # §5.5.2 sync-lane copy sites are the measurand; NOT a headline.
        "${pfx[@]}" perf stat -e "$PERF_EVENTS" -o "$base.fio.perf" -- \
            fio --name="$name" --directory="$dir" --filename_format='f$jobnum' \
            --numjobs="$THREADS" --thread --group_reporting --ioengine=psync \
            --rw="$rw" --direct=1 --zero_buffers --size=1g \
            --time_based --runtime="$RUNTIME" \
            --output-format=json --output="$base.fio.json" "$@" >/dev/null 2>&1 \
            || { kill -INT "$perfpid" 2>/dev/null; kill "$thppid" 2>/dev/null; fail "fio $LABEL/$name r$rep"; }
        kill -INT "$perfpid" 2>/dev/null; wait "$perfpid" 2>/dev/null
        kill "$thppid" 2>/dev/null; wait "$thppid" 2>/dev/null
        snap_stats "$base.post.json"
        local dw1 dr1 tkb
        dw1=$(dev_bytes w); dr1=$(dev_bytes r); tkb=$(cat "$base.thp" 2>/dev/null || echo 0)
        tkb=${tkb:-0}
        python3 - "$LABEL" "$name" "$rep" "$base" "$shim" "$((dw1-dw0))" "$((dr1-dr0))" "$tkb" "$CSV" <<'EOF'
import json, re, sys
label, row, rep, base, shim, devw, devr, tkb, csv = sys.argv[1:10]
b = json.load(open(base + '.pre.json')); a = json.load(open(base + '.post.json'))
j = json.load(open(base + '.fio.json'))
d = lambda k: a.get(k, 0) - b.get(k, 0)
kind = 'read' if 'rd' in row else 'write'
agg = [job[kind] for job in j['jobs']]
iops = round(sum(x['iops'] for x in agg))
bw = round(sum(x['bw_bytes'] for x in agg) / 1048576)
user = sum(x['io_bytes'] for x in agg)
def perf(path):
    vals = {}
    for line in open(path, errors='replace'):
        m = re.match(r'\s*([\d,\.]+)\s+(msec)?\s*([\w-]+)', line)
        if m:
            v = m.group(1).replace(',', '')
            try: vals[m.group(3)] = float(v)
            except ValueError: pass
    return vals
dm = perf(base + '.dmn.perf'); fm = perf(base + '.fio.perf')
ipc_in = d('ipc_bytes_in'); ipc_out = d('ipc_bytes_out')
engage = 'ok'
if shim == '1' and kind == 'write' and ipc_in != user:
    engage = f'INVALID ipc_bytes_in {ipc_in} != user {user}'
if shim == '0' and (ipc_in or ipc_out):
    engage = f'INVALID kernel ipc leak {ipc_in}/{ipc_out}'
if d('write_path_seed_read_bytes'):
    engage = 'INVALID seed_read_bytes moved'
line = ','.join(str(x) for x in [
    label, row, rep, bw, iops, user,
    round(dm.get('task-clock', 0)), round(dm.get('cycles', 0)),
    round(dm.get('cache-misses', 0)), round(dm.get('dTLB-load-misses', 0)),
    round(dm.get('minor-faults', 0)),
    round(fm.get('task-clock', 0)), round(fm.get('cycles', 0)),
    round(fm.get('cache-misses', 0)), round(fm.get('dTLB-load-misses', 0)),
    d('nt_copy_bytes'), d('ipc_placed_severs'), d('placed_merge_elides'),
    ipc_in, ipc_out, devw, devr, tkb, engage])
open(csv, 'a').write(line + '\n')
missb = dm.get('cache-misses', 0) * 64
prox = (missb / user) if user else 0
print(f"  [{label} {row} r{rep}] bw={bw}MiB/s daemon-LLCmiss*64/user={prox:.2f}B/B "
      f"nt={d('nt_copy_bytes')} elides={d('placed_merge_elides')} thp={tkb}KiB {engage}")
if engage != 'ok':
    sys.exit(9)
EOF
        [ $? -eq 0 ] || fail "engagement $LABEL/$name r$rep"
    done
}

mount_side
# Write rows (sustained, time_based over a bounded file set).
row wr-il   1 write fresh --bs=1m --fallocate=none
row wr-kern 0 write fresh --bs=1m --fallocate=none
# Read rows: prefill once (shim path so the same file set serves both),
# then tier-cold remounted reads per rep (device-true-ish; hybrid serves
# labeled as such in the note).
if echo "rd" | grep -Eq "$ROW_FILTER"; then
    rm -rf "$MOUNT_DIR/census"; mkdir -p "$MOUNT_DIR/census"; chmod 1777 "$MOUNT_DIR/census"
    env LD_PRELOAD="$SHIM" fio --name=prefill --directory="$MOUNT_DIR/census" \
        --filename_format='f$jobnum' --numjobs="$THREADS" --thread --ioengine=psync \
        --rw=write --bs=1m --direct=1 --zero_buffers --size=1g --group_reporting \
        --output-format=json --output="$RESULTS/$LABEL.prefill.json" >/dev/null 2>&1 \
        || fail "prefill"
fi
row rd-il   1 read cold --bs=1m
row rd-kern 0 read cold --bs=1m
kill_daemon

echo "=== census side $LABEL complete → $CSV ==="
column -s, -t "$CSV" | tail -20
