# Directory walk (decision 0003 §K row 12): `find -type f` over a tree of
# FR_FILES files (FR_DIRS directories, laid down once) — READDIRPLUS +
# LOOKUP/GETATTR per entry. The warm-up's first walk after a fresh mount is
# the COLD pass (its node-read ledger rides the metrics as `cold_*`); the
# measured span loops the walk WARM. Bounds: cold ≤ one leaf node per
# stripe per readdir page; warm ≈ 0 device reads per record.
ROW_KIND=metadata
ROW_NEEDS_SHIM=0
FR_FILES=${FR_FILES:-100000}
FR_DIRS=${FR_DIRS:-1000}
ROW_INSTRUMENT="find -type f over $FR_FILES files in $FR_DIRS dirs (python3 lays the tree down once), looped for the span"

row_prepare() {   # mnt
  local dir="$1/find-tree"
  if [ ! -f "$dir/.prepared" ]; then
    python3 - "$dir" "$FR_FILES" "$FR_DIRS" <<'PYEOF'
import os, sys
root, files, dirs = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
os.makedirs(root, exist_ok=True)
for d in range(dirs):
    p = os.path.join(root, "d%04d" % d)
    os.makedirs(p, exist_ok=True)
    for f in range(files // dirs):
        fd = os.open(os.path.join(p, "f%05d" % f), os.O_CREAT | os.O_WRONLY, 0o644)
        os.close(fd)
PYEOF
    sync -f "$1"
    touch "$dir/.prepared"
  fi
}

row_run() {   # mnt secs outprefix phase
  local mnt=$1 secs=$2 out=$3 phase=$4 dir="$1/find-tree"
  cat "$mnt/.stats" > "$out.stats-before.json"
  local t0 t1 n=0 walks=0
  t0=$(date +%s%N)
  if [ "$phase" = warmup ]; then
    n=$(find "$dir" -type f | wc -l); walks=1
  else
    while [ $(( ($(date +%s%N) - t0) / 1000000000 )) -lt "$secs" ]; do
      n=$(( n + $(find "$dir" -type f | wc -l) )); walks=$((walks + 1))
    done
  fi
  t1=$(date +%s%N)
  cat "$mnt/.stats" > "$out.stats-after.json"
  printf '{"entries": %d, "walks": %d, "elapsed_ns": %d}\n' "$n" "$walks" $((t1 - t0)) > "$out.find.json"
  [ "$phase" = warmup ] && cp "$out.stats-before.json" "$BRK_LEG_DIR/cold-before.json" && cp "$out.stats-after.json" "$BRK_LEG_DIR/cold-after.json" && cp "$out.find.json" "$BRK_LEG_DIR/cold.find.json"
  return 0
}

row_metrics() {   # outprefix → JSON on stdout
  python3 - "$1.find.json" "$BRK_LEG_DIR" <<'PYEOF'
import json, sys, os
sys.path.insert(0, os.environ["BRK_HERE"])
import brk
f = json.load(open(sys.argv[1]))
m = {"instrument": "find", "ops": f["entries"], "bytes": 0, "elapsed_s": f["elapsed_ns"] / 1e9, "walks": f["walks"]}
m["headline"] = {"name": "entries/s", "value": (f["entries"] / m["elapsed_s"]) if m["elapsed_s"] else 0}
leg = sys.argv[2]
if os.path.exists(os.path.join(leg, "cold-before.json")):
    b = brk.terms(json.load(open(os.path.join(leg, "cold-before.json"))))
    a = brk.terms(json.load(open(os.path.join(leg, "cold-after.json"))))
    d = brk.delta(a, b)
    oc = d["fuse_op_counts"]
    recs = oc["lookup"] + oc["getattr"] + d["fuse_readdir_entries"]
    cf = json.load(open(os.path.join(leg, "cold.find.json")))
    m["cold"] = {"node_read_bytes": d["node_read_bytes"], "node_reads": d["node_reads"], "lookups": oc["lookup"], "getattrs": oc["getattr"],
                 "dirents": d["fuse_readdir_entries"], "readdirs": oc["readdir"], "records": recs,
                 "node_bytes_per_record": (d["node_read_bytes"] / recs) if recs else None,
                 "node_reads_per_record": (d["node_reads"] / recs) if recs else None,
                 "node_reads_per_readdir": (d["node_reads"] / oc["readdir"]) if oc["readdir"] else None,
                 "cache_hits": d["meta_kv_node_cache_hits"], "cache_misses": d["meta_kv_node_cache_misses"],
                 "entries_per_s": cf["entries"] / (cf["elapsed_ns"] / 1e9) if cf["elapsed_ns"] else 0}
    m["notes"] = "cold walk (the warm-up): %.0f entries/s, %d node reads / %d B over %d records (%d dirents, %d readdirs, %d lookups, %d getattrs) = %s B/record, %s reads/record, %s reads/readdir" % (
        m["cold"]["entries_per_s"], d["node_reads"], d["node_read_bytes"], recs, d["fuse_readdir_entries"], oc["readdir"], oc["lookup"], oc["getattr"],
        brk.fmt(m["cold"]["node_bytes_per_record"]), brk.fmt(m["cold"]["node_reads_per_record"]), brk.fmt(m["cold"]["node_reads_per_readdir"]))
json.dump(m, sys.stdout)
PYEOF
}

row_cleanup() { :; }
