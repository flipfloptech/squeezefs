# Metadata creates, many-dirs, through the mount (charter §13: ≥ 32 k/s).
# Instrument: IOR's mdtest under MPI — `-C -F` (create files only), `-u` (one
# tree per task), `-z 2 -b 8` (a 73-directory tree per task: many dirs).
# mdtest's stonewall (`-W`) refuses a branching tree, so the sustained span
# is reached by SIZING: the warm-up runs a small fixed count and its rate
# sizes the measured count to ≥ 1.1 × the span (floor MD_MIN_ITEMS per task);
# the record carries the real elapsed time. Created trees are removed AFTER
# the measured span (outside the record) so the scratch stays bounded; the
# store still ages (journal, slot trees, inos). Also the posture row's
# workload (`--lever posture`): a solo symmetric mount must sit within noise
# of `--single-writer` with the wire census 0.
ROW_KIND=metadata
ROW_NEEDS_SHIM=0
MDTEST=${MDTEST:-$BRK_TOOLS/mdtest}
MD_NP=${MD_NP:-$BRK_NUMJOBS}
MD_MIN_ITEMS=${MD_MIN_ITEMS:-10000}
MD_WARM_ITEMS=${MD_WARM_ITEMS:-10000}
ROW_INSTRUMENT="mdtest (ior $(cd ${SQZ_BUILD_ROOT:-/build}/tools/src/ior-io500 2>/dev/null && git describe --tags --always 2>/dev/null || echo io500-sc19-605-g5528b36)) -C -F -u -z 2 -b 8 -n <sized from the warm-up rate>, mpirun -np $MD_NP"

row_prepare() { mkdir -p "$1/mdtest"; }

row_run() {   # mnt secs outprefix phase
  local mnt=$1 secs=$2 out=$3 phase=$4
  local dir="$mnt/mdtest/$phase-$BRK_LEG" n=$MD_WARM_ITEMS
  if [ "$phase" = measure ] && [ -f "$BRK_LEG_DIR/md-rate-per-task" ]; then
    n=$(python3 -c "import sys; r=float(open(sys.argv[1]).read()); print(max(int(sys.argv[3]), int(r * float(sys.argv[2]) * 1.1)))" "$BRK_LEG_DIR/md-rate-per-task" "$secs" "$MD_MIN_ITEMS")
  fi
  mkdir -p "$dir"
  bounded_run "$secs" "$out" mpirun --allow-run-as-root --oversubscribe -np "$MD_NP" "$MDTEST" -C -F -u -z 2 -b 8 -n "$n" -d "$dir" > "$out.mdtest.txt" 2>&1 || { cat "$out.mdtest.txt" >&2; return 1; }
  echo "$dir" >> "$BRK_LEG_DIR/mdtest-dirs"
  # The per-task rate sizes the next phase.
  python3 - "$out.mdtest.txt" "$MD_NP" > "$BRK_LEG_DIR/md-rate-per-task" <<'EOF'
import sys, os
sys.path.insert(0, os.environ["BRK_HERE"])
import brk
m = brk.mdtest_metrics(sys.argv[1])
print(max(1.0, m["creates_per_s"] / int(sys.argv[2])))
EOF
}

row_metrics() {
  python3 - "$1.mdtest.txt" <<'EOF'
import json, sys, os, re
sys.path.insert(0, os.environ["BRK_HERE"])
import brk
m = brk.mdtest_metrics(sys.argv[1])
text = open(sys.argv[1]).read()
n = re.search(r"'-n' '(\d+)'", text)
tasks = m.get("tasks", 0)
items = int(n.group(1)) * tasks if n else m.get("items", 0)
m["ops"] = items
m["items_created"] = items
m["elapsed_s"] = (items / m["creates_per_s"]) if m["creates_per_s"] else 0.0
m["headline"] = {"name": "creates/s", "value": m["creates_per_s"]}
m["notes"] = "%d files across %d tasks × 73 dirs; elapsed %.1f s (sized from the warm-up rate)" % (items, tasks, m["elapsed_s"])
json.dump(m, sys.stdout)
EOF
}

row_cleanup() {   # mnt — remove this leg's trees (outside the record)
  [ -f "$BRK_LEG_DIR/mdtest-dirs" ] || return 0
  while read -r d; do rm -rf "$d"; done < "$BRK_LEG_DIR/mdtest-dirs"
  rm -f "$BRK_LEG_DIR/mdtest-dirs"
}
