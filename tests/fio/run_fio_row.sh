#!/usr/bin/env bash
# tests/fio/run_fio_row.sh — the house fio row runner (fio is the house
# instrument for all field rows; user directive 2026-07-31).
#
# Takes a [global]-shape job file from tests/fio/ and makes it a labeled,
# reproducible ROW:
#   * TOPOLOGY-GENERAL NUMA fan-out: discovers the box's NUMA nodes at
#     runtime and appends one fio [section] per node with
#     numa_cpu_nodes/numa_mem_policy=bind — works on N domains, not 2;
#     a single-node box gets ONE section and NO numa lines (the law).
#     --njobs is the TOTAL job count, split as evenly as possible.
#   * RAW device mode (--devices a:b:c): one [section] per device with
#     filename=<dev> and numjobs=SQZ_FIO_NJOBS_PER_DEV (default 8).
#     A write-class raw job is DESTRUCTIVE and refuses to run without
#     --i-know-this-destroys-data.
#   * Shim rows (--shim <libsqueezefs_il.so>): LD_PRELOAD applied to the
#     fio process only; with --mount, engagement is VERIFIED from the
#     stats inode (ipc_ops_read+ipc_ops_write deltas must account for
#     >= SQZ_FIO_ENGAGE_MIN of the row's ios, default 0.90) — a
#     silent-passthrough row exits nonzero (charter rule 4).
#   * Venue labels: --label/--substrate/--fill/--order are printed in
#     the row line and persisted, so journal copies stay honest.
#   * Persists everything under --results: the GENERATED job file, fio
#     JSON, stats-inode before/after + delta, and meta.json.
#
# Examples:
#   # EXA write-BW shape through the kernel FUSE path:
#   tests/fio/run_fio_row.sh --job tests/fio/exa_write_bw.job \
#     --dir /mnt/sqz/fio --mount /mnt/sqz --label wrkern-exa \
#     --substrate "field 4-node nvme-tcp" --fill fresh --order r1
#
#   # Same shape via the interception shim, engagement-verified:
#   tests/fio/run_fio_row.sh --job tests/fio/exa_write_bw.job \
#     --dir /mnt/sqz/fio --mount /mnt/sqz \
#     --shim target/preload-release/libsqueezefs_il.so --label wril-exa
#
#   # Raw device read ceiling (per-device sections):
#   tests/fio/run_fio_row.sh --job tests/fio/raw_ceiling_read.job \
#     --devices /dev/nvme1n1:/dev/nvme2n1 --label raw-read
set -u

usage() {
    cat <<'EOF'
usage: run_fio_row.sh --job <file.job> (--dir <dir> | --devices <d1:d2:..>)
       [--mount <mountpoint>] [--shim <libsqueezefs_il.so>]
       [--njobs N] [--bs 1M] [--iodepth 8] [--size 1g]
       [--runtime 30] [--ramp 10] [--engine libaio]
       [--data-devs <nvme4n1:nvme6n1:..>]   # diskstats amp columns
       [--label S] [--substrate S] [--fill S] [--order S]
       [--results DIR] [--journal FILE] [--no-numa] [--emit-only]
       [--i-know-this-destroys-data]
EOF
    exit 1
}

fail() { echo "FAIL: $*" >&2; exit 1; }

JOB="" DIR="" DEVICES="" MOUNT="" SHIM="" DATA_DEVS=""
NJOBS="" BS="1M" IODEPTH="8" SIZE="1g" RUNTIME="30" RAMP="10"
ENGINE="libaio" LABEL="row" SUBSTRATE="unlabeled" FILL="unlabeled"
ORDER="unlabeled" RESULTS="" JOURNAL="" NO_NUMA=0 DESTROY_OK=0 EMIT_ONLY=0

while [ $# -gt 0 ]; do
    case "$1" in
        --job) JOB="$2"; shift 2 ;;
        --dir) DIR="$2"; shift 2 ;;
        --devices) DEVICES="$2"; shift 2 ;;
        --mount) MOUNT="$2"; shift 2 ;;
        --shim) SHIM="$2"; shift 2 ;;
        --data-devs) DATA_DEVS="$2"; shift 2 ;;
        --njobs) NJOBS="$2"; shift 2 ;;
        --bs) BS="$2"; shift 2 ;;
        --iodepth) IODEPTH="$2"; shift 2 ;;
        --size) SIZE="$2"; shift 2 ;;
        --runtime) RUNTIME="$2"; shift 2 ;;
        --ramp) RAMP="$2"; shift 2 ;;
        --engine) ENGINE="$2"; shift 2 ;;
        --label) LABEL="$2"; shift 2 ;;
        --substrate) SUBSTRATE="$2"; shift 2 ;;
        --fill) FILL="$2"; shift 2 ;;
        --order) ORDER="$2"; shift 2 ;;
        --results) RESULTS="$2"; shift 2 ;;
        --journal) JOURNAL="$2"; shift 2 ;;
        --no-numa) NO_NUMA=1; shift ;;
        --emit-only) EMIT_ONLY=1; shift ;;
        --i-know-this-destroys-data) DESTROY_OK=1; shift ;;
        -h|--help) usage ;;
        *) fail "unknown arg: $1" ;;
    esac
done

[ -n "$JOB" ] && [ -f "$JOB" ] || fail "--job <file.job> required (got '${JOB}')"
command -v fio >/dev/null || fail "fio not found"
command -v python3 >/dev/null || fail "python3 not found"
if [ -n "$DIR" ] && [ -n "$DEVICES" ]; then fail "--dir and --devices are exclusive"; fi
if [ -z "$DIR" ] && [ -z "$DEVICES" ]; then fail "one of --dir or --devices required"; fi
if [ -n "$SHIM" ] && [ ! -f "$SHIM" ]; then fail "shim not found: $SHIM"; fi

# ---- destructive guard (raw write-class jobs) --------------------------
JOB_RW="$(sed -n 's/^readwrite=//p' "$JOB" | head -1)"
if [ -n "$DEVICES" ]; then
    case "$JOB_RW" in
        write|randwrite|rw|randrw|trimwrite|trim)
            if [ "$DESTROY_OK" -ne 1 ]; then
                fail "job '$JOB' WRITES TO RAW DEVICES ($DEVICES) and destroys their contents; re-run with --i-know-this-destroys-data if you really mean it"
            fi ;;
    esac
fi

RESULTS="${RESULTS:-/tmp/fio_rows/$(date +%Y%m%d_%H%M%S)_${LABEL}}"
mkdir -p "$RESULTS"
GEN="$RESULTS/${LABEL}.job"
OUT="$RESULTS/${LABEL}.json"

# ---- topology discovery (N NUMA domains, not 2) ------------------------
# SQZ_FIO_NODE_SYSFS overrides the sysfs root (emission-path testing).
NODE_SYSFS="${SQZ_FIO_NODE_SYSFS:-/sys/devices/system/node}"
NODES=()
if [ "$NO_NUMA" -eq 0 ] && [ -d "$NODE_SYSFS" ]; then
    for n in "$NODE_SYSFS"/node[0-9]*; do
        [ -d "$n" ] || continue
        NODES+=("${n##*node}")
    done
fi
NNODES="${#NODES[@]}"
# Single node (or no sysfs, or --no-numa): one section, no numa lines.
USE_NUMA=0
[ "$NNODES" -gt 1 ] && USE_NUMA=1

# ---- generate the concrete job file ------------------------------------
BASE="$(basename "$JOB" .job)"
cp "$JOB" "$GEN"

if [ -n "$DEVICES" ]; then
    NJPD="${SQZ_FIO_NJOBS_PER_DEV:-8}"
    IFS=':' read -r -a DEVS <<< "$DEVICES"
    [ "${#DEVS[@]}" -gt 0 ] || fail "no devices parsed from '$DEVICES'"
    for dev in "${DEVS[@]}"; do
        [ -e "$dev" ] || fail "device not found: $dev"
        {
            echo ""
            echo "[${BASE}_$(basename "$dev")]"
            echo "filename=$dev"
            echo "numjobs=$NJPD"
            if [ -n "${SQZ_FIO_OFFSET_INC:-}" ]; then
                echo "offset_increment=${SQZ_FIO_OFFSET_INC}"
            fi
        } >> "$GEN"
    done
    NJOBS_TOTAL=$(( ${#DEVS[@]} * NJPD ))
else
    mkdir -p "$DIR" || fail "cannot create --dir $DIR"
    NJOBS="${NJOBS:-$(nproc)}"
    NJOBS_TOTAL="$NJOBS"
    if [ "$USE_NUMA" -eq 1 ]; then
        # Split TOTAL njobs across the discovered nodes as evenly as
        # possible; nodes that would get 0 jobs get no section.
        per=$(( NJOBS / NNODES ))
        rem=$(( NJOBS % NNODES ))
        idx=0
        for node in "${NODES[@]}"; do
            cnt=$per
            [ "$idx" -lt "$rem" ] && cnt=$(( cnt + 1 ))
            idx=$(( idx + 1 ))
            [ "$cnt" -gt 0 ] || continue
            {
                echo ""
                echo "[${BASE}_numa${node}]"
                echo "numjobs=$cnt"
                echo "numa_cpu_nodes=$node"
                echo "numa_mem_policy=bind:$node"
            } >> "$GEN"
        done
    else
        {
            echo ""
            echo "[${BASE}]"
            echo "numjobs=$NJOBS"
        } >> "$GEN"
    fi
fi

# ---- env the job files consume ------------------------------------------
export SQZ_FIO_ENGINE="$ENGINE"
export SQZ_FIO_BS="$BS"
export SQZ_FIO_IODEPTH="$IODEPTH"
export SQZ_FIO_SIZE="$SIZE"
export SQZ_FIO_RUNTIME="$RUNTIME"
export SQZ_FIO_RAMP="$RAMP"
export SQZ_FIO_DIR="$DIR"

if [ "$EMIT_ONLY" -eq 1 ]; then
    echo "EMIT-ONLY: generated job at $GEN (njobs_total=$NJOBS_TOTAL, numa_fanout across $NNODES node(s))"
    cat "$GEN"
    exit 0
fi

FIO_VERSION="$(fio --version)"
PATHKIND="kernel"
[ -n "$DEVICES" ] && PATHKIND="raw"
[ -n "$SHIM" ] && PATHKIND="shim"

# ---- stats snapshot (before) --------------------------------------------
snap_stats() { # <outfile>
    python3 -c "import json;print(json.dumps(json.load(open('$MOUNT/.stats'))['metrics']))" \
        > "$1" 2>/dev/null || echo '{}' > "$1"
}
if [ -n "$MOUNT" ]; then snap_stats "$RESULTS/${LABEL}.stats_before.json"; fi

# /proc/diskstats snapshot for the standing amplification columns:
# per --data-devs device, sectors-read ($6) and sectors-written ($10).
snap_disk() { # <outfile>
    if [ -n "$DATA_DEVS" ]; then
        awk -v devs="$DATA_DEVS" 'BEGIN{n=split(devs,a,":");for(i=1;i<=n;i++)want[a[i]]=1}
             $3 in want {print $3, $6, $10}' /proc/diskstats > "$1"
    fi
}
snap_disk "$RESULTS/${LABEL}.disk_before.txt"

journal() {
    [ -n "$JOURNAL" ] || return 0
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) fio-row: $*" >> "$JOURNAL" 2>/dev/null || true
}
journal "START label=$LABEL job=$(basename "$JOB") engine=$ENGINE bs=$BS qd=$IODEPTH njobs=$NJOBS_TOTAL runtime=${RUNTIME}s path=$PATHKIND fill=$FILL order=$ORDER"

# ---- run ----------------------------------------------------------------
set +e
if [ -n "$SHIM" ]; then
    LD_PRELOAD="$SHIM" fio --output-format=json --output="$OUT" "$GEN" \
        > "$RESULTS/${LABEL}.fio_stderr.log" 2>&1
else
    fio --output-format=json --output="$OUT" "$GEN" \
        > "$RESULTS/${LABEL}.fio_stderr.log" 2>&1
fi
FIO_RC=$?
if [ "$FIO_RC" -ne 0 ] || [ ! -s "$OUT" ]; then
    cat "$RESULTS/${LABEL}.fio_stderr.log" >&2 || true
    journal "FAIL label=$LABEL fio_rc=$FIO_RC"
    fail "fio exited $FIO_RC (log: $RESULTS/${LABEL}.fio_stderr.log)"
fi
# fio >= 3.4x prepends advisory "note: ..." lines to --output even in JSON
# mode (e.g. "note: both iodepth >= 1 and synchronous I/O engine are
# selected..." on the matched-inflight psync passes); fio 3.36 does not.
# Strip everything before the first '{' so every downstream json.load sees
# pure JSON regardless of the instrument's fio build.
python3 - "$OUT" <<'EOF'
import sys
p = sys.argv[1]
raw = open(p, "rb").read()
i = raw.find(b"{")
if i > 0:
    open(p, "wb").write(raw[i:])
EOF

# ---- stats snapshot (after) + delta --------------------------------------
if [ -n "$MOUNT" ]; then
    snap_stats "$RESULTS/${LABEL}.stats_after.json"
    python3 - "$RESULTS/${LABEL}.stats_before.json" \
        "$RESULTS/${LABEL}.stats_after.json" \
        "$RESULTS/${LABEL}.stats_delta.json" <<'EOF'
import json, sys
before = json.load(open(sys.argv[1])); after = json.load(open(sys.argv[2]))
delta = {}
for k, v in after.items():
    b = before.get(k, 0)
    if isinstance(v, (int, float)) and isinstance(b, (int, float)) and v - b:
        delta[k] = v - b
json.dump(delta, open(sys.argv[3], "w"), indent=1, sort_keys=True)
keys = ["ipc_ops_read", "ipc_ops_write", "ipc_bytes_in", "ipc_bytes_out",
        "ipc_fast_path_serves", "ipc_async_handoffs",
        "write_through_blocks", "write_pipeline_admission_waits",
        "placed_severs", "placed_merge_elides", "nt_copy_bytes",
        "block_free_reclaim_cap_parks", "prefetch_issued", "ranged_reads"]
print("  stats: " + " ".join(f"{k}={delta[k]}" for k in keys if delta.get(k)))
EOF
    # Must-stay-flat tripwires (the 2026-08-04 EXA corruption lesson —
    # invariant_tripwires and its escalation siblings growing ACROSS a
    # benchmark row means the daemon hit a designed-impossible concurrency
    # outcome mid-row; the row's numbers are invalid and the run must not
    # quietly continue to the next row).
    TRIP="$(python3 - "$RESULTS/${LABEL}.stats_delta.json" <<'EOF'
import json, sys
delta = json.load(open(sys.argv[1]))
watch = ["invariant_tripwires", "rewrite_shadow_fence_drops",
         "write_pipeline_fence_drops", "data_dma_fence_refusals",
         "detached_task_panics"]
hits = {k: delta[k] for k in watch if delta.get(k)}
print(" ".join(f"{k}=+{v}" for k, v in sorted(hits.items())))
EOF
)"
    if [ -n "$TRIP" ]; then
        journal "FAIL label=$LABEL tripwires: $TRIP"
        fail "must-stay-flat tripwires grew across the row: $TRIP (stats delta: $RESULTS/${LABEL}.stats_delta.json)"
    fi
fi

# ---- amplification columns (device bytes vs user bytes) -------------------
AMP_LINE=""
if [ -n "$DATA_DEVS" ]; then
    snap_disk "$RESULTS/${LABEL}.disk_after.txt"
    AMP_LINE="$(python3 - "$RESULTS/${LABEL}.disk_before.txt" \
        "$RESULTS/${LABEL}.disk_after.txt" "$OUT" <<'EOF'
import json, sys
def read(p):
    d = {}
    for line in open(p):
        f = line.split()
        if len(f) == 3:
            d[f[0]] = (int(f[1]), int(f[2]))
    return d
b, a = read(sys.argv[1]), read(sys.argv[2])
dr = sum((a[k][0] - b.get(k, (0, 0))[0]) * 512 for k in a)
dw = sum((a[k][1] - b.get(k, (0, 0))[1]) * 512 for k in a)
fio = json.load(open(sys.argv[3]))
ur = sum(j.get("read", {}).get("io_bytes", 0) for j in fio.get("jobs", []))
uw = sum(j.get("write", {}).get("io_bytes", 0) for j in fio.get("jobs", []))
parts = []
if uw:
    parts.append(f"write_amp {dw/uw:.3f} (dev {dw/1e9:.2f} GB / user {uw/1e9:.2f} GB)")
if ur:
    parts.append(f"read_amp {dr/ur:.3f} (dev {dr/1e9:.2f} GB / user {ur/1e9:.2f} GB)")
print("; ".join(parts) if parts else "no user bytes")
EOF
)"
fi

# ---- summarize + engagement verdict ---------------------------------------
SUMMARY="$(python3 - "$OUT" <<'EOF'
import json, sys
data = json.load(open(sys.argv[1]))
tot = {"read": [0, 0, 0.0, 0], "write": [0, 0, 0.0, 0]}  # bw_bytes, iops, clat_w, ios
p99 = {"read": 0.0, "write": 0.0}
for j in data.get("jobs", []):
    for d in ("read", "write"):
        s = j.get(d, {})
        ios = s.get("total_ios", 0)
        if not ios:
            continue
        tot[d][0] += s.get("bw_bytes", 0)
        tot[d][1] += s.get("iops", 0)
        tot[d][2] += s.get("clat_ns", {}).get("mean", 0.0) * ios
        tot[d][3] += ios
        pct = s.get("clat_ns", {}).get("percentile", {})
        p99[d] = max(p99[d], pct.get("99.000000", 0.0))
parts, all_ios = [], 0
for d in ("read", "write"):
    bw, iops, clw, ios = tot[d]
    all_ios += ios
    if ios:
        parts.append(
            f"{d}: {bw/1e9:.2f} GB/s ({bw/2**30:.2f} GiB/s) {iops:,.0f} IOPS "
            f"clat_mean {clw/ios/1e6:.3f} ms p99 {p99[d]/1e6:.3f} ms"
        )
print(" | ".join(parts) if parts else "no I/O recorded")
print(all_ios)
EOF
)"
ROWLINE="$(echo "$SUMMARY" | head -1)"
TOTAL_IOS="$(echo "$SUMMARY" | tail -1)"

ENGAGE="n/a"
RC=0
if [ -n "$SHIM" ] && [ -n "$MOUNT" ]; then
    ENGAGE="$(python3 - "$RESULTS/${LABEL}.stats_delta.json" "$TOTAL_IOS" <<'EOF'
import json, sys
delta = json.load(open(sys.argv[1]))
ios = int(sys.argv[2]) or 1
ops = delta.get("ipc_ops_read", 0) + delta.get("ipc_ops_write", 0)
print(f"{ops/ios:.3f}")
EOF
)"
    MIN="${SQZ_FIO_ENGAGE_MIN:-0.90}"
    if python3 -c "import sys; sys.exit(0 if float('$ENGAGE') >= float('$MIN') else 1)"; then
        ENGAGE="$ENGAGE (>=$MIN OK)"
    else
        ENGAGE="$ENGAGE (<$MIN INVALID)"
        RC=4
    fi
fi

# ---- persist row meta ------------------------------------------------------
python3 - "$RESULTS/${LABEL}.meta.json" <<EOF
import json, sys
json.dump({
    "label": "$LABEL", "job": "$JOB", "generated_job": "$GEN",
    "instrument": "$FIO_VERSION", "engine": "$ENGINE",
    "bs": "$BS", "iodepth": "$IODEPTH", "njobs_total": $NJOBS_TOTAL,
    "size": "$SIZE", "runtime_s": $RUNTIME, "ramp_s": $RAMP,
    "numa_nodes": $NNODES, "numa_fanout": $USE_NUMA,
    "dir": "$DIR", "devices": "$DEVICES", "shim": "$SHIM",
    "path_kind": "$PATHKIND",
    "substrate": "$SUBSTRATE", "fill": "$FILL", "order": "$ORDER",
    "engagement": "$ENGAGE", "amplification": "$AMP_LINE",
    "row": "$ROWLINE",
}, open(sys.argv[1], "w"), indent=1)
EOF

echo "ROW label=$LABEL | $ROWLINE"
echo "  shape: $(basename "$JOB") engine=$ENGINE bs=$BS qd=$IODEPTH njobs=$NJOBS_TOTAL (numa_fanout=$USE_NUMA/$NNODES nodes) runtime=${RUNTIME}s ramp=${RAMP}s size=$SIZE"
echo "  venue: instrument=\"$FIO_VERSION\" substrate=\"$SUBSTRATE\" fill=\"$FILL\" order=\"$ORDER\" path=$PATHKIND"
echo "  engagement: $ENGAGE"
[ -n "$AMP_LINE" ] && echo "  amplification: $AMP_LINE"
echo "  artifacts: $RESULTS"
journal "DONE label=$LABEL rc=$RC | $ROWLINE | engagement=$ENGAGE"
exit "$RC"
