# 1 KiB O_APPEND records, each its own open-write-close (a shell's `echo … >>
# log` loop; rung-10 dev perf §10.4): N writers each appending to its own file
# for the span. On a set without staging every close drains the append — the
# row's law `append_drain_bound` (decision 0003 §AK): device bytes ≤ (pages(1
# KiB) × 4 KiB / 1 KiB + 1) × appended + files × 4 KiB = ×5 — the touched page
# in place per drain plus each block's one whole landing — where the whole-
# block drain wrote 4 MiB per close (1,602 MB for 36 KB of `echo >> f`). The
# readback (every record in order) is the instrument's own correctness verdict.
ROW_KIND=data-append
ROW_NEEDS_SHIM=0
AP_WRITERS=${AP_WRITERS:-$BRK_NUMJOBS}
AP_RECORD=1024
ROW_INSTRUMENT="appends.py writers=$AP_WRITERS ($AP_RECORD B O_APPEND records, open-write-close each, read back in order)"

row_prepare() { mkdir -p "$1/ap"; }

row_run() {   # mnt secs outprefix phase
  local mnt=$1 secs=$2 out=$3 phase=$4
  bounded_run "$secs" "$out" python3 "$BRK_HERE/appends.py" "$mnt/ap/$phase-$BRK_LEG" "$secs" "$AP_WRITERS" "$AP_RECORD" > "$out.appends.json"
}

row_metrics() {
  python3 - "$1.appends.json" <<'PYEOF'
import json, sys
m = json.load(open(sys.argv[1]))
m["headline"] = {"name": "appends/s", "value": m["ops_per_s"]}
json.dump(m, sys.stdout)
PYEOF
}

row_cleanup() { rm -rf "$1/ap/warmup-$BRK_LEG" "$1/ap/measure-$BRK_LEG" 2>/dev/null || true; }
