#!/usr/bin/env bash
# tests/fio/zcrx_field_rows.sh — the D5 gate chain's Z3 FIELD ROWS
# (docs/design-zcrx-read-lane.md: MEM-3 ✓ → TEST-6 → **Z3 field rows**).
#
# A-B-B-A alternating remounts (A = SQUEEZEFS_ZCRX_LANE=1, B = off) over
# ONE prefilled beyond-RAM fileset; per side: a sustained (>= 60 s)
# sequential cold-read row (the RX-copy CPU face) + a rand-4k row (the
# IOPS face), with mpstat softirq + daemon-CPU capture and the zcrx
# engagement snapshot (fill ≈ row bytes; gather ≡ fill; poisoned = 0;
# dest_gather = the fused subset). A lane side whose fill_bytes do not
# account for the row's cold bytes is INVALID (silent kernel-path serve),
# printed loudly, never presented as a lane number.
#
# FIO ENGINE POLICY (user ruling 2026-08-07;
# `.benchmarks/2026-08-07-fio-engine-policy.md`): throughput/IOPS rows =
# ioengine=libaio + direct=1 + stated iodepth, BOTH lanes; A/Bs use the
# SAME engine both sides (this rig's A/B is armed-vs-control — matched by
# construction); psync only as labeled sync-lane coverage rows; io_uring
# = labeled kernel-lane extra. This rig is libaio-compliant (all rows).
#
# usage: zcrx_field_rows.sh --mount <mnt> --meta <sqmeta-uri> [--dir <dir>]
#        [--runtime 60] [--njobs 16] [--out /tmp/zcrx_rows]
set -u
MNT="" META="" DIR="" RUNTIME=60 NJOBS=16 OUT="/tmp/zcrx_rows_$(date +%Y%m%d_%H%M%S)"
SQZ="${SQZ:-/scratch/tmp/squeezefs}"
while [ $# -gt 0 ]; do
    case "$1" in
        --mount) MNT="$2"; shift 2 ;;
        --meta) META="$2"; shift 2 ;;
        --dir) DIR="$2"; shift 2 ;;
        --runtime) RUNTIME="$2"; shift 2 ;;
        --njobs) NJOBS="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        *) echo "unknown arg $1" >&2; exit 2 ;;
    esac
done
[ -n "$MNT" ] && [ -n "$META" ] || { echo "--mount and --meta required" >&2; exit 2; }
DIR="${DIR:-$MNT/exa_perf}"
mkdir -p "$OUT"

snap() { # torn-tolerant stats snapshot
    for _ in 1 2 3 4 5; do
        dd if="$MNT/.stats" of="$1" bs=1M status=none 2>/dev/null
        python3 -c "import json;json.load(open('$1'))" 2>/dev/null && return 0
        sleep 0.3
    done
    return 1
}

remount() { # $1 = "on" | "off"
    "$SQZ" umount "$MNT" >/dev/null 2>&1 || true
    sleep 2
    if [ "$1" = on ]; then
        SQUEEZEFS_ZCRX_LANE=1 "$SQZ" mount "$META" "$MNT" --daemon --interception \
            --allow-other --log-file /scratch/tmp/logs/sqz.log >/dev/null 2>&1
    else
        "$SQZ" mount "$META" "$MNT" --daemon --interception \
            --allow-other --log-file /scratch/tmp/logs/sqz.log >/dev/null 2>&1
    fi
    sleep 4
    mountpoint -q "$MNT" || { echo "REMOUNT($1) FAILED" >&2; exit 1; }
}

run_side() { # $1 = side tag (A1/B1/B2/A2), $2 = on|off
    local tag=$1 lane=$2
    echo "== side $tag (lane=$lane) =="
    remount "$lane"
    for shape in seqread rand4k; do
        local label="$tag.$shape"
        snap "$OUT/$label.before.json"
        mpstat -P ALL 5 > "$OUT/$label.mpstat" 2>/dev/null &
        local MP=$!
        pidstat -p "$(pidof squeezefs)" 5 > "$OUT/$label.pidstat" 2>/dev/null &
        local PS=$!
        if [ "$shape" = seqread ]; then
            fio --name=sr --directory="$DIR" --filename_format='sqzfio.$jobnum.0' \
                --rw=read --bs=1M --size=1g --numjobs="$NJOBS" --iodepth=8 \
                --ioengine=libaio --direct=1 --time_based --runtime="$RUNTIME" \
                --group_reporting --output-format=json --output="$OUT/$label.fio.json" \
                >/dev/null 2>&1
        else
            fio --name=rr --directory="$DIR" --filename_format='sqzfio.$jobnum.0' \
                --rw=randread --bs=4k --size=1g --numjobs="$NJOBS" --iodepth=8 \
                --ioengine=libaio --direct=1 --time_based --runtime="$RUNTIME" \
                --group_reporting --output-format=json --output="$OUT/$label.fio.json" \
                >/dev/null 2>&1
        fi
        kill $MP $PS 2>/dev/null; wait $MP $PS 2>/dev/null
        snap "$OUT/$label.after.json"
        python3 - "$OUT/$label" "$lane" <<'EOF'
import json, sys
p, lane = sys.argv[1], sys.argv[2]
raw = open(f"{p}.fio.json", "rb").read()
i = raw.find(b"{"); fio = json.loads(raw[i:])
r = [j["read"] for j in fio["jobs"]]
bw = sum(x["bw_bytes"] for x in r) / 1e9
iops = sum(x["iops"] for x in r)
user = sum(x["io_bytes"] for x in r)
def m(f):
    d = json.load(open(f)); return d.get("metrics", d)
b, a = m(f"{p}.before.json"), m(f"{p}.after.json")
def delta(k): return a.get(k, 0) - b.get(k, 0)
fill, gath, dest = delta("zcrx_fill_bytes"), delta("zcrx_gather_bytes"), delta("zcrx_dest_gather_bytes")
armed, poi = a.get("zcrx_lane_armed", 0), delta("zcrx_lane_poisoned")
# Engagement-geometry instruments (Rev 4 §13): declines are the SIZING
# instrument (sustained growth at < 100 % engagement = an under-derived
# window — name the term); parks/failovers must stay bounded episodes.
# Round 2: the three volume gates are mutually-exclusive counters —
# bypasses (the pool/degraded term), waits (admission), fallbacks
# (per-op errors) — and starved_ms/structural name the no-harm arms.
waits = delta("zcrx_area_admission_waits")
parks, fails = delta("zcrx_recv_parks"), delta("zcrx_recv_failovers")
fbk, viol = delta("zcrx_fill_fallbacks"), delta("zcrx_frame_violations")
byp = delta("zcrx_degraded_bypasses")
# Round 4: the park errno-class split — closure parks == dry+rq+cq per
# row; WHICH class dominates names the starved term (pool arithmetic vs
# refill posting vs reap/CQ) without another source campaign.
pdry, prq, pcq = delta("zcrx_parks_pool_dry"), delta("zcrx_parks_rq_empty"), delta("zcrx_parks_cq_full")
starved_ms, structural = delta("zcrx_starved_ms"), delta("zcrx_structural_teardowns")
eng = fill / user if user else 0.0
closure = "exact" if fill == gath else f"TORN(fill={fill},gather={gath})"
verdict = "n/a(off)" if lane == "off" else (
    "ENGAGED" if eng > 0.5 and poi == 0 and viol == 0 else
    f"INVALID(eng={eng:.3f},poisoned={poi},viol={viol})")
print(f"  {p.split('/')[-1]}: {bw:.2f} GB/s {iops:,.0f} IOPS user={user/1e9:.1f}GB "
      f"fill={fill/1e9:.1f}GB gather={gath/1e9:.1f}GB dest={dest/1e9:.1f}GB "
      f"share={eng:.3f} closure={closure} waits={waits} bypasses={byp} "
      f"parks={parks}(dry={pdry},rq={prq},cq={pcq}) failovers={fails} fallbacks={fbk} "
      f"starved_ms={starved_ms} structural={structural} armed={armed} {verdict}")
EOF
    done
}

run_side A1 on
run_side B1 off
run_side B2 off
run_side A2 on
remount off
echo "artifacts: $OUT"
