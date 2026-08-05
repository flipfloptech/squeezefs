#!/usr/bin/env bash
# tests/fio/fleet_parity_row.sh — D12 board item 2 field confirmation row
# (2026-08-05, perf/shim-fleet-parity f19df263): bs=1M seq write + read,
# fio psync PROCESS fleet (one shim session per process — the field shape),
# numjobs=256, qd1, il vs kernel in alternating pairs (K-I, I-K, K-I), with
# per-row engagement (ipc_ops_*, ipc_bytes_*) and the new arena-prep ledger
# (ipc_arena_prep_{queued,done}) off the stats inode. A shim row whose
# engagement deltas do not account for its ops is INVALID and printed so.
#
# usage: fleet_parity_row.sh --mount <mnt> [--njobs 256] [--size 512m]
#        [--shim /scratch/tmp/libsqueezefs_il.so] [--out DIR]
set -u
# SIZE default 256m: 3 write fleets + the read prefill must fit the
# store WITH reclaim headroom (512m hit ENOSPC at fleet 3, varying
# user bytes across rows -- an invalid comparison).
MNT="" NJOBS=256 SIZE="256m" SHIM="/scratch/tmp/libsqueezefs_il.so"
OUT="/tmp/fleet_parity_$(date +%Y%m%d_%H%M%S)"
while [ $# -gt 0 ]; do
    case "$1" in
        --mount) MNT="$2"; shift 2 ;;
        --njobs) NJOBS="$2"; shift 2 ;;
        --size) SIZE="$2"; shift 2 ;;
        --shim) SHIM="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        *) echo "unknown arg $1" >&2; exit 2 ;;
    esac
done
[ -n "$MNT" ] || { echo "--mount required" >&2; exit 2; }
mkdir -p "$OUT"

require_mount() { # a row against a bare directory is not a slow row — it
    # is NO row (the 2026-08-05 invalid-run lesson: fio happily "succeeds"
    # against local scratch and every number is fiction). Checked at start
    # and before EVERY row; failure is fatal, never a warning.
    mountpoint -q "$MNT" || { echo "FATAL: $MNT is not a mountpoint — refusing to fabricate rows" >&2; exit 1; }
    [ -e "$MNT/.stats" ] || { echo "FATAL: $MNT/.stats missing — not a squeezefs mount" >&2; exit 1; }
}
require_mount

snap() {
    for _ in 1 2 3 4 5; do
        dd if="$MNT/.stats" of="$1" bs=1M status=none 2>/dev/null
        python3 -c "import json;json.load(open('$1'))" 2>/dev/null && return 0
        sleep 0.3
    done
    return 1
}

settle() { # wait for the mount to quiesce between fleets: prep ledger
    # closed (queued == done + skipped_*), reclaim queue drained, and the
    # R5 level back to Green — so store aging/pressure from row N never
    # contaminates row N+1 (either arm).
    local t f="$OUT/.settle.json"
    for t in $(seq 1 60); do
        snap "$f" || { sleep 2; continue; }
        python3 - "$f" <<'EOF' && return 0
import json, sys
m = json.load(open(sys.argv[1])); m = m.get("metrics", m)
q = m.get("ipc_arena_prep_queued", 0)
closed = q == (m.get("ipc_arena_prep_done", 0)
               + m.get("ipc_arena_prep_skipped_dead", 0)
               + m.get("ipc_arena_prep_skipped_pressure", 0))
ok = (closed and m.get("block_free_reclaim_queue_bytes", 0) == 0
      and m.get("mem_budget_level", 0) == 0)
sys.exit(0 if ok else 1)
EOF
        sleep 2
    done
    echo "  (settle timed out after 120s — next row may be contaminated)" >&2
    return 1
}

row() { # $1=arm(kern|il) $2=rw(write|read) $3=tag
    local arm=$1 rw=$2 tag=$3 dir="$MNT/fleet_parity"
    local env_prefix=()
    [ "$arm" = il ] && env_prefix=(env LD_PRELOAD="$SHIM")
    if [ "$rw" = write ]; then rm -rf "$dir"; fi
    mkdir -p "$dir"
    # DURABLE=1 (default): write rows are fsync-inclusive — the RW6 law
    # (AGENTS.md scoreboard: durable rows GOVERN write verdicts). Without
    # it a 137 GB fleet vs 251 GB RAM measures page-cache/park absorption,
    # not the write path — the 2026-08-05 attribution timeline proved the
    # device plane sustains ~2.5 GB/s while relaxed rows print 17-26.
    local durable=""
    [ "${DURABLE:-1}" = 1 ] && [ "$rw" = write ] && durable="--end_fsync=1"
    snap "$OUT/$tag.before.json"
    "${env_prefix[@]}" fio --name=fp --directory="$dir" \
        --filename_format='fp.$jobnum' --rw="$rw" --bs=1M --size="$SIZE" \
        --numjobs="$NJOBS" --iodepth=1 --ioengine=psync $durable \
        --create_on_open=1 --output-format=json \
        --output="$OUT/$tag.fio.json" >/dev/null 2>&1
    snap "$OUT/$tag.after.json"
    python3 - "$OUT/$tag" "$arm" "$rw" <<'EOF'
import json, sys
p, arm, rw = sys.argv[1], sys.argv[2], sys.argv[3]
raw = open(f"{p}.fio.json", "rb").read()
fio = json.loads(raw[raw.find(b"{"):])
key = "read" if rw == "read" else "write"
r = [j[key] for j in fio["jobs"]]
user = sum(x["io_bytes"] for x in r)
# Durable law: fio bw_bytes excludes the end_fsync stall. The honest
# durable rate is user bytes / wall time. Precision ladder (both bugs
# were shipped once): group_reporting SUMS job_runtime (256x, run 9);
# "elapsed" is INTEGER seconds (run 10's identical 22.91x3 rows = 3 s
# quantization, ratio error bars 0.56-1.0). So write rows run WITHOUT
# group_reporting and wall = max per-job job_runtime (ms).
if len(fio["jobs"]) > 1:
    wall_ms = max(j.get("job_runtime", 0) for j in fio["jobs"])
else:  # grouped fallback (read rows keep group_reporting)
    wall_ms = max(j.get("elapsed", 0) for j in fio["jobs"]) * 1000
bw = user / (wall_ms / 1000) / 1e9 if wall_ms else 0.0
def m(f):
    d = json.load(open(f)); return d.get("metrics", d)
b, a = m(f"{p}.before.json"), m(f"{p}.after.json")
def d(k): return a.get(k, 0) - b.get(k, 0)
ops = d("ipc_ops_write") if rw == "write" else d("ipc_ops_read")
ib = d("ipc_bytes_in") if rw == "write" else d("ipc_bytes_out")
pq, pd = d("ipc_arena_prep_queued"), d("ipc_arena_prep_done")
if arm == "il":
    eng = ib / user if user else 0.0
    verdict = "ENGAGED" if eng > 0.9 else f"INVALID(eng={eng:.3f})"
else:
    verdict = "n/a(kern)" if ops == 0 else f"LEAK(ops={ops})"
print(f"  {p.split('/')[-1]}: {bw:.2f} GB/s user={user/1e9:.0f}GB "
      f"ipc_ops={ops} prep_q={pq} prep_done={pd} {verdict}")
EOF
    rm -rf "$dir"
}

echo "== fleet parity row: njobs=$NJOBS size=$SIZE (K-I, I-K, K-I pairs) =="
for pair in "kern il" "il kern" "kern il"; do
    set -- $pair
    for arm in $1 $2; do
        require_mount; row "$arm" write "w.$arm.$(date +%s)"
        settle
    done
done
echo "== read twin (prefill once, then K-I, I-K, K-I on the same set) =="
dir="$MNT/fleet_parity"; rm -rf "$dir"; mkdir -p "$dir"
fio --name=pre --directory="$dir" --filename_format='fp.$jobnum' --rw=write \
    --bs=1M --size="$SIZE" --numjobs="$NJOBS" --iodepth=1 --ioengine=psync \
    --create_on_open=1 --group_reporting >/dev/null 2>&1
read_row() { # $1=arm $2=tag  — reads never age the store: no rm
    local arm=$1 tag=$2 dir="$MNT/fleet_parity"
    local env_prefix=()
    [ "$arm" = il ] && env_prefix=(env LD_PRELOAD="$SHIM")
    snap "$OUT/$tag.before.json"
    "${env_prefix[@]}" fio --name=fp --directory="$dir" \
        --filename_format='fp.$jobnum' --rw=read --bs=1M --size="$SIZE" \
        --numjobs="$NJOBS" --iodepth=1 --ioengine=psync \
        --group_reporting --output-format=json \
        --output="$OUT/$tag.fio.json" >/dev/null 2>&1
    snap "$OUT/$tag.after.json"
    python3 - "$OUT/$tag" "$arm" read <<'EOF'
import json, sys
p, arm, rw = sys.argv[1], sys.argv[2], sys.argv[3]
raw = open(f"{p}.fio.json", "rb").read()
fio = json.loads(raw[raw.find(b"{"):])
r = [j["read"] for j in fio["jobs"]]
bw = sum(x["bw_bytes"] for x in r) / 1e9
user = sum(x["io_bytes"] for x in r)
def m(f):
    d = json.load(open(f)); return d.get("metrics", d)
b, a = m(f"{p}.before.json"), m(f"{p}.after.json")
def d(k): return a.get(k, 0) - b.get(k, 0)
ops, ob = d("ipc_ops_read"), d("ipc_bytes_out")
if arm == "il":
    eng = ob / user if user else 0.0
    verdict = "ENGAGED" if eng > 0.9 else f"INVALID(eng={eng:.3f})"
else:
    verdict = "n/a(kern)" if ops == 0 else f"LEAK(ops={ops})"
print(f"  {p.split('/')[-1]}: {bw:.2f} GB/s user={user/1e9:.0f}GB "
      f"ipc_ops={ops} {verdict}")
EOF
}
for pair in "kern il" "il kern" "kern il"; do
    set -- $pair
    for arm in $1 $2; do
        require_mount; read_row "$arm" "r.$arm.$(date +%s)"
    done
done
rm -rf "$MNT/fleet_parity"
echo "artifacts: $OUT"
