# 4-byte O_APPEND writes (fstests generic/069's `append_writer`): N writers each
# appending integers to its own file for the span — the staged arm's in-place
# append (decision 0003 §J.2). The row's laws: `staged_append_bound` (ring
# bytes moved ≤ 11 × bytes appended) and `journal_entries_per_append` (≤ 1.05);
# c36b531 re-staged the whole 1–2 MiB image per append (≈ 3 ms, 2–4 MiB moved).
# The readback (`append_reader`'s check: every integer in order) is the
# instrument's own correctness verdict.
ROW_KIND=data-append
ROW_NEEDS_SHIM=0
# The staged arm needs a staging root: the harness formats the set with one.
ROW_NEEDS_STAGING=1
AP_WRITERS=${AP_WRITERS:-$BRK_NUMJOBS}
ROW_INSTRUMENT="appends.py writers=$AP_WRITERS (4 B O_APPEND integers per writer, read back in order)"

row_prepare() { mkdir -p "$1/ap"; }

row_run() {   # mnt secs outprefix phase
  local mnt=$1 secs=$2 out=$3 phase=$4
  bounded_run "$secs" "$out" python3 "$BRK_HERE/appends.py" "$mnt/ap/$phase-$BRK_LEG" "$secs" "$AP_WRITERS" > "$out.appends.json"
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
