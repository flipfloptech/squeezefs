# Metadata STAT storm (decision 0003 §K rows 10–11): mdtest `-T -F -u -z 2
# -b 8` over a tree the row lays down ONCE (`-C`, MD_ITEMS files per task
# across 73 dirs each); the warm-up's first pass after a fresh mount is the
# COLD pass (its node-read ledger is recorded beside the metrics as
# `cold_*`), the measured pass is WARM. Bounds: cold ≤ tree height nodes per
# record; warm ≈ 0 device reads per record (the node cache holds the tree).
ROW_KIND=metadata
ROW_NEEDS_SHIM=0
MDTEST=${MDTEST:-$BRK_TOOLS/mdtest}
MD_NP=${MD_NP:-$BRK_NUMJOBS}
MD_ITEMS=${MD_ITEMS:-25000}
ROW_INSTRUMENT="mdtest (ior $(cd /build/tools/src/ior-io500 2>/dev/null && git describe --tags --always 2>/dev/null || echo io500-sc19-605-g5528b36)) -T -F -u -z 2 -b 8 -n $MD_ITEMS over a tree created once, mpirun -np $MD_NP"

row_prepare() {   # mnt
  local mnt=$1 dir="$1/mdtest-stat"
  mkdir -p "$dir"
  if [ ! -f "$dir/.prepared" ]; then
    mpirun --allow-run-as-root --oversubscribe -np "$MD_NP" "$MDTEST" -C -F -u -z 2 -b 8 -n "$MD_ITEMS" -d "$dir" > "$BRK_LEG_DIR/prepare.mdtest.txt" 2>&1 || { cat "$BRK_LEG_DIR/prepare.mdtest.txt" >&2; return 1; }
    sync -f "$mnt"
    touch "$dir/.prepared"
  fi
}

row_run() {   # mnt secs outprefix phase
  local mnt=$1 secs=$2 out=$3 phase=$4 dir="$1/mdtest-stat"
  cat "$mnt/.stats" > "$out.stats-before.json"
  bounded_run "$secs" "$out" mpirun --allow-run-as-root --oversubscribe -np "$MD_NP" "$MDTEST" -T -F -u -z 2 -b 8 -n "$MD_ITEMS" -d "$dir" > "$out.mdtest.txt" 2>&1 || { cat "$out.mdtest.txt" >&2; return 1; }
  cat "$mnt/.stats" > "$out.stats-after.json"
  # The warm-up IS the cold pass: keep its ledger for the metrics.
  [ "$phase" = warmup ] && cp "$out.stats-before.json" "$BRK_LEG_DIR/cold-before.json" && cp "$out.stats-after.json" "$BRK_LEG_DIR/cold-after.json" && cp "$out.mdtest.txt" "$BRK_LEG_DIR/cold.mdtest.txt"
  return 0
}

row_metrics() {   # outprefix → JSON on stdout
  python3 - "$1.mdtest.txt" "$MD_NP" "$MD_ITEMS" "$BRK_LEG_DIR" <<'PYEOF'
import json, sys, os, re
sys.path.insert(0, os.environ["BRK_HERE"])
import brk
m = brk.mdtest_metrics(sys.argv[1])
np_, items = int(sys.argv[2]), int(sys.argv[3])
st = m.get("file_stat") or {"mean": 0.0}
m["ops"] = items * np_
m["bytes"] = 0
m["elapsed_s"] = (m["ops"] / st["mean"]) if st["mean"] else 0.0
m["headline"] = {"name": "stats/s", "value": st["mean"]}
leg = sys.argv[4]
if os.path.exists(os.path.join(leg, "cold-before.json")):
    b = brk.terms(json.load(open(os.path.join(leg, "cold-before.json"))))
    a = brk.terms(json.load(open(os.path.join(leg, "cold-after.json"))))
    d = brk.delta(a, b)
    oc = d["fuse_op_counts"]
    recs = oc["lookup"] + oc["getattr"] + d["fuse_readdir_entries"]
    m["cold"] = {"node_read_bytes": d["node_read_bytes"], "node_reads": d["node_reads"], "lookups": oc["lookup"], "getattrs": oc["getattr"],
                 "records": recs, "node_bytes_per_record": (d["node_read_bytes"] / recs) if recs else None,
                 "node_reads_per_record": (d["node_reads"] / recs) if recs else None,
                 "cache_hits": d["meta_kv_node_cache_hits"], "cache_misses": d["meta_kv_node_cache_misses"]}
    cm = brk.mdtest_metrics(os.path.join(leg, "cold.mdtest.txt")).get("file_stat") or {}
    m["cold"]["stats_per_s"] = cm.get("mean", 0.0)
    m["notes"] = "cold pass (the warm-up): %.0f stats/s, %d node reads / %d B over %d records = %s B/record, %s reads/record" % (
        m["cold"]["stats_per_s"], d["node_reads"], d["node_read_bytes"], recs,
        brk.fmt(m["cold"]["node_bytes_per_record"]), brk.fmt(m["cold"]["node_reads_per_record"]))
json.dump(m, sys.stdout)
PYEOF
}

row_cleanup() { :; }
