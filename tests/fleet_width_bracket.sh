#!/usr/bin/env bash
# tests/fleet_width_bracket.sh — D12 board item 2 (perf/shim-fleet-parity):
# the SESSION-FLEET width bracket. il-vs-kernel large-sequential parity as a
# function of PROCESS-fleet width (one shim session per client process — the
# field's 256-session shape), bs=1M psync qd1, per the shim-parity law.
#
# FIO ENGINE POLICY (user ruling 2026-08-07 — the matched-instrument law;
# `.benchmarks/2026-08-07-fio-engine-policy.md`): throughput/IOPS rows are
# libaio+direct=1+stated iodepth both lanes; A/Bs use the SAME engine both
# sides; psync survives only as labeled sync-lane coverage.
#
# SYNC-LANE COVERAGE RIG — psync by design, matched engine on BOTH arms;
# NOT headline throughput numbers. Adjudication (same as
# tests/fio/fleet_parity_row.sh): the measurand is the §5.5.1
# sync-fast-path SESSION FLEET as a function of width — the v1.1
# single-slot aio screen cannot express a bs=1M fleet (slab >= 1 MiB
# needs SQUEEZEFS_IPC_ARENA_MB>=1024, and width x sessions x 1 GiB
# arenas is an admission-budget impossibility at fleet widths), so a
# libaio il fleet would silently measure the kernel lane. The default
# rows are buffered (the field ingest shape) — psync is the explicit
# engine choice per rule 4 (libaio degrades to sync on buffered I/O).
#
# Instrument (stated per the standing rule): fio psync, numjobs=N PROCESSES
# (no --thread — each job is a fork, so each job establishes its OWN
# session; the write_matrix's --thread shape shares ONE session registry
# across 16 threads and never populates a fleet), qd1, bs=1M, seq write,
# fresh files per row, O_DIRECT per SQZ_FW_DIRECT (default 0 = buffered,
# the field's large-sequential ingest shape).
# Substrate: the TCP dev substrate (nvmet-tcp on localhost) — MANDATORY for
# fabric-sensitive write rows (two-substrate rule, 2026-07-27).
# Ordering: A-B-B-A per width (il, kernel, kernel, il) — the store ages
# (allocation cursor, reclaim), so both orders are cited (standing rule).
#
# Engagement: every il row must account its ops in ipc_ops_write Δ
# (expected = fio ops × ceil(bs/max_op_bytes)); a zero-Δ il row is INVALID.
#
# Usage: sudo SQZ_FW_WIDTHS="32 64 128 256" tests/fleet_width_bracket.sh
set -u

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_DIR/target}"
META_DEV="${SQZ_META_DEV:-/dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1}"
DATA_DEV="${SQZ_DATA_DEV:-/dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1}"
MOUNT_DIR="${MOUNT_DIR:-/mnt/sqz_fleet_width}"
RESULTS="${SQZ_FW_RESULTS:-/tmp/fleet_width_$(date +%Y%m%d_%H%M%S)}"
WIDTHS="${SQZ_FW_WIDTHS:-32 64 128 256}"
TOTAL_MB="${SQZ_FW_TOTAL_MB:-16384}"   # fleet-total bytes per row (capacity-bound: zram oss)
BS="${SQZ_FW_BS:-1m}"
DIRECT="${SQZ_FW_DIRECT:-0}"
# SQZ_FW_ZERO=1: fio --zero_buffers — the zram-zstd oss stores same-fill
# pages without compressing, lifting the DEVICE ceiling so the daemon-side
# per-op term is exposed (the volume is passthrough: squeezefs never
# compresses; NT-copy/sever/DMA costs per byte are content-blind).
# Stated per the instrument-alignment rule wherever rows are cited.
ZERO="${SQZ_FW_ZERO:-0}"
# SQZ_FW_RUNTIME=secs: sustained-state rows (the 2026-07-29 standing rule) —
# fio --time_based --runtime=S sequential overwrite of the fleet's files.
# Session establishment (256 × 64 MiB populate+collapse at fleet start)
# amortizes instead of dominating a 2.5 s create row; the write shape
# becomes whole-block overwrite (CoW rewrite + displaced free) — identical
# for both transports, so the il/kernel DELTA stays the instrument.
RUNTIME="${SQZ_FW_RUNTIME:-0}"
# SQZ_FW_DURABLE=1: durable rows — fio --end_fsync=1, per the RW6 law
# (durable rows GOVERN write verdicts; the 2026-08-05 field addendum:
# relaxed 137 GB fleets vs 251 GB RAM measured page-cache/park ABSORPTION,
# not the write path). The three fio-JSON traps documented in
# tests/fio/fleet_parity_row.sh apply verbatim: bw_bytes EXCLUDES the
# fsync stall, group_reporting SUMS job_runtime across jobs, and
# 'elapsed' is integer seconds. So durable rows run WITHOUT
# group_reporting and the honest rate is user_bytes / max per-job
# job_runtime (ms precision).
DURABLE="${SQZ_FW_DURABLE:-0}"
SQUEEZEFS_BIN="${SQUEEZEFS_BIN:-$TARGET_DIR/release/squeezefs}"
SO="${SQZ_FW_SO:-$TARGET_DIR/preload-release/libsqueezefs_il.so}"
MOUNT_ENV="${SQZ_FW_MOUNT_ENV:-}"      # extra daemon env, e.g. "SQUEEZEFS_IPC_SERVICE_THREADS=12"
LOG="$RESULTS/daemon.log"
CSV="$RESULTS/width.csv"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }
[ -x "$SQUEEZEFS_BIN" ] || { echo "missing $SQUEEZEFS_BIN"; exit 1; }
[ -f "$SO" ] || { echo "missing shim $SO"; exit 1; }
for d in ${META_DEV//,/ } ${DATA_DEV//,/ }; do
    [ -b "$d" ] || { echo "missing rig device $d (tcp devsub down?)"; exit 1; }
done
mkdir -p "$RESULTS" "$MOUNT_DIR"
INVALID=0
fail() { echo "FAIL: $*"; exit 1; }

snap_stats() {
    python3 -c "import json;print(json.dumps(json.load(open('$MOUNT_DIR/.stats'))['metrics']))" \
        > "$1" 2>/dev/null || echo '{}' > "$1"
}

KEYS="ipc_ops_write ipc_bytes_in ipc_async_handoffs ipc_service_parks \
ipc_inval_notifies ipc_inval_attrs_only ipc_inval_suppressed \
ipc_service_threads ipc_sessions_active ipc_sessions_total ipc_admission_refusals \
ipc_cqe_wake_writes ipc_cqe_wake_elided ipc_severed_pool_hits ipc_severed_pool_misses \
ipc_placed_severs placed_adoptions placed_merge_elides ipc_placed_sever_fallbacks \
placed_assembly_bytes nt_copy_bytes numa_local_bytes numa_remote_bytes \
write_through_blocks write_pipeline_admission_waits transport_wake_writes \
transport_wake_elided fuse_ops meta_kv_journal_entries ipc_descriptor_rejects \
ipc_sessions_poisoned"

diff_stats() { # before after out
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
    for _ in $(seq 30); do
        pgrep -f " mount .* $MOUNT_DIR" >/dev/null || break
        sleep 0.5
    done
    pkill -9 -f " mount .* $MOUNT_DIR" 2>/dev/null || true
    sleep 1
}
trap kill_daemon EXIT

format_fs() {
    "$SQUEEZEFS_BIN" format "sqmeta://$META_DEV" "sqdata://$DATA_DEV" --force \
        >> "$RESULTS/format.log" 2>&1 || fail "format"
    udevadm settle --timeout=10 2>/dev/null || true
}

mount_fs() {
    rm -f "$LOG"
    local m_ok=0
    for _ in 1 2 3 4 5; do
        if env RUST_LOG=info SQUEEZEFS_IPC_MEM_MAX=24576 $MOUNT_ENV \
            "$SQUEEZEFS_BIN" mount "sqmeta://$META_DEV" "$MOUNT_DIR" --daemon \
            --allow-other --mem-cache-size 2GB --log-file "$LOG" --interception; then
            m_ok=1; break
        fi
        sleep 1
    done
    [ "$m_ok" = 1 ] || fail "mount"
    for _ in $(seq 20); do mountpoint -q "$MOUNT_DIR" && break; sleep 0.5; done
    mountpoint -q "$MOUNT_DIR" || { tail -20 "$LOG"; fail "mount not up"; }
    chmod 1777 "$MOUNT_DIR"
}

fio_bw()   { python3 -c "import json;j=json.load(open('$1'));print(round(sum(job['write']['bw_bytes'] for job in j['jobs'])/1048576))"; }
# Durable wall-clock rate (MiB/s): user bytes / max per-job job_runtime —
# the fleet is done when its LAST job's fsync returns. Requires
# per-job reporting (no group_reporting on durable rows).
fio_bw_durable() { python3 -c "
import json
j = json.load(open('$1'))
user = sum(job['write']['io_bytes'] for job in j['jobs'])
wall_ms = max(job.get('job_runtime', 0) for job in j['jobs'])
print(round(user / (wall_ms / 1000) / 1048576) if wall_ms else 0)"; }
fio_ops()  { python3 -c "import json;j=json.load(open('$1'));print(sum(job['write']['total_ios'] for job in j['jobs']))"; }
fio_el()   { if [ "$DURABLE" = 1 ]; then
                 python3 -c "import json;j=json.load(open('$1'));print(max(job.get('job_runtime',0) for job in j['jobs'])/1000.0)"
             else
                 python3 -c "import json;j=json.load(open('$1'));print(max(job['write'].get('runtime',0) for job in j['jobs'])/1000.0)"
             fi; }

daemon_pid() { pgrep -f " mount .* $MOUNT_DIR" | head -1; }

run_row() { # run_row <label> <shim 0|1> <width> <rep>
    local label="$1" shim="$2" width="$3" rep="$4"
    local per_mb=$(( TOTAL_MB / width ))
    local dir="$MOUNT_DIR/seqd"
    mkdir -p "$dir"; chmod 1777 "$dir"
    rm -f "$dir"/s_f* 2>/dev/null
    sync -f "$MOUNT_DIR" 2>/dev/null || sync
    # Settle the async block reclaim (the rm's displaced frees) so the next
    # row never races the reclaimer into ENOSPC (capacity: 4×8G zram oss).
    # TWO gates: reclaim queue drained AND statvfs available covers the
    # row (+2 GiB margin) — the queue alone raced: the rm's frees may not
    # be ENQUEUED yet when it samples 0 (the run-1 r3 ENOSPC).
    python3 - "$MOUNT_DIR" "$TOTAL_MB" <<'EOF'
import json, os, sys, time
mnt, need_mb = sys.argv[1], int(sys.argv[2]) + 2048
for _ in range(120):
    try:
        m = json.load(open(f"{mnt}/.stats"))["metrics"]
        q = m.get("block_free_reclaim_queue_bytes", 0)
        st = os.statvfs(mnt)
        avail_mb = st.f_bavail * st.f_frsize // 1048576
        if q == 0 and avail_mb >= need_mb:
            break
    except Exception:
        pass
    time.sleep(0.5)
else:
    print(f"  (capacity settle timed out: avail={avail_mb}MB need={need_mb}MB)", file=sys.stderr)
EOF
    sleep 1
    local pre="$RESULTS/$label.r$rep.pre.json" post="$RESULTS/$label.r$rep.post.json"
    local fioout="$RESULTS/$label.r$rep.fio.json"
    local pfx=(env)
    [ "$shim" = 1 ] && pfx=(env LD_PRELOAD="$SO")
    local zflag=()
    [ "$ZERO" = 1 ] && zflag=(--zero_buffers)
    [ "$RUNTIME" != 0 ] && zflag+=(--time_based --runtime="$RUNTIME")
    snap_stats "$pre"
    # per-thread daemon CPU sampling during the row (decomposition input):
    # /proc/<pid>/task/*/stat utime+stime, summed per comm class.
    local dpid; dpid=$(daemon_pid)
    python3 - "$dpid" > "$RESULTS/$label.r$rep.threadcpu" 2>/dev/null <<'EOF' &
import os, sys, time, collections
pid = sys.argv[1]
def snap():
    acc = collections.Counter()
    base = f"/proc/{pid}/task"
    try:
        tids = os.listdir(base)
    except OSError:
        return acc
    for t in tids:
        try:
            st = open(f"{base}/{t}/stat").read()
        except OSError:
            continue
        comm = st[st.index("(")+1:st.rindex(")")]
        f = st[st.rindex(")")+2:].split()
        # utime = field 14, stime = 15 (1-indexed); after comm: index 11, 12
        acc[comm] += int(f[11]) + int(f[12])
    return acc
a = snap(); t0 = time.time()
try:
    while True:
        time.sleep(1)
        b = snap()
        print("---", round(time.time()-t0, 1), flush=True)
        agg = collections.Counter()
        for comm, v in b.items():
            d = v - a.get(comm, 0)
            if d:
                cls = comm
                for pfx_ in ("sqz-ipc-svc", "fuse3-tpc", "tokio-runtime-w"):
                    if comm.startswith(pfx_):
                        cls = pfx_ + "*"
                agg[cls] += d
        for cls, d in agg.most_common():
            print(cls, d, flush=True)
        a = b
except KeyboardInterrupt:
    pass
EOF
    local pstat=$!
    # Durable rows: --end_fsync=1 and NO group_reporting (the fio-JSON
    # traps — see the SQZ_FW_DURABLE block up top). Relaxed rows keep the
    # original grouped form verbatim.
    local grpflag=(--group_reporting)
    [ "$DURABLE" = 1 ] && { grpflag=(--end_fsync=1); }
    # Write-amp instrument (standing row requirement): per-row
    # /proc/diskstats deltas on the DATA namespaces.
    grep -E " nvme[0-9]+n[0-9]+ " /proc/diskstats > "$RESULTS/$label.r$rep.dsk.pre"
    "${pfx[@]}" fio --name="$label" --directory="$dir" \
        --filename_format='s_f$jobnum' --numjobs="$width" \
        "${grpflag[@]}" --ioengine=psync --rw=write --bs="$BS" \
        --direct="$DIRECT" --size="${per_mb}m" --fallocate=none "${zflag[@]}" \
        --output-format=json --output="$fioout" >/dev/null 2>&1 \
        || { kill $pstat 2>/dev/null; fail "fio $label rep$rep"; }
    kill $pstat 2>/dev/null; wait $pstat 2>/dev/null
    grep -E " nvme[0-9]+n[0-9]+ " /proc/diskstats > "$RESULTS/$label.r$rep.dsk.post"
    snap_stats "$post"
    local sel; sel=$(diff_stats "$pre" "$post" "$RESULTS/$label.r$rep.delta.json")
    local bw ops el ipcw
    if [ "$DURABLE" = 1 ]; then bw=$(fio_bw_durable "$fioout"); else bw=$(fio_bw "$fioout"); fi
    ops=$(fio_ops "$fioout"); el=$(fio_el "$fioout")
    ipcw=$(python3 -c "import json;print(json.load(open('$RESULTS/$label.r$rep.delta.json')).get('ipc_ops_write',0))")
    local engage="ok"
    if [ "$shim" = 1 ] && [ "$ipcw" -eq 0 ]; then engage="INVALID-passthrough"; INVALID=1; fi
    if [ "$shim" = 0 ] && [ "$ipcw" -ne 0 ]; then engage="INVALID-leak"; INVALID=1; fi
    echo "$label,$rep,$width,$bw,$el,$ops,$ipcw,$engage" >> "$CSV"
    echo "  [$label r$rep] w=$width bw=${bw}MiB/s el=${el}s ops=$ops ipc_ops_write=$ipcw $engage"
    [ -n "$sel" ] && echo "      Δ $sel"
}

echo "results: $RESULTS  widths: $WIDTHS  bs=$BS direct=$DIRECT total=${TOTAL_MB}MB"
echo "instrument: fio-$(fio --version 2>/dev/null | head -1) psync process-fleet qd1 (SYNC-LANE COVERAGE — psync by design, §5.5.1 session fleet; NOT headline); substrate: nvmet-tcp devsub (localhost)"
echo "label,rep,width,bw_mib_s,elapsed_s,fio_ops,ipc_ops_write,engagement" > "$CSV"

kill_daemon
for width in $WIDTHS; do
    # Fresh format + mount per width: kills cross-width store aging and
    # bounds capacity to one width's rows.
    format_fs
    mount_fs
    # Alternating-order pairs (A-B, B-A, A-B): 3 samples per side per
    # width, both orders cited (the aging-store rule).
    run_row "il-w$width"     1 "$width" 1
    run_row "kernel-w$width" 0 "$width" 1
    run_row "kernel-w$width" 0 "$width" 2
    run_row "il-w$width"     1 "$width" 2
    run_row "il-w$width"     1 "$width" 3
    run_row "kernel-w$width" 0 "$width" 3
    kill_daemon
done

echo
echo "=== width bracket complete → $CSV ==="
column -s, -t "$CSV"
python3 - "$CSV" <<'EOF'
import csv, sys, collections
rows = collections.defaultdict(list)
for r in csv.DictReader(open(sys.argv[1])):
    rows[(r["label"].split("-w")[0], int(r["width"]))].append(float(r["bw_mib_s"]))
def med(xs):
    xs = sorted(xs); return xs[len(xs)//2] if len(xs) % 2 else (xs[len(xs)//2-1]+xs[len(xs)//2])/2
widths = sorted({w for (_, w) in rows})
print(f"{'width':>6} {'il MiB/s':>24} {'kernel MiB/s':>24} {'med(il)/med(k)':>15}")
for w in widths:
    il = rows.get(("il", w), []); k = rows.get(("kernel", w), [])
    if not il or not k: continue
    ratio = med(il)/med(k) if med(k) else float("nan")
    print(f"{w:>6} {str([round(x) for x in il]):>24} {str([round(x) for x in k]):>24} {ratio:>15.3f}")
EOF
[ "$INVALID" -ne 0 ] && { echo "ENGAGEMENT INVALID rows present"; exit 2; }
exit 0
