# `tar -x` of a synthetic tree into a directory this node created (charter
# §13: 0 wire verbs per entry in its own tree; the solo mount's wire census
# must be 0). Instrument: GNU tar extracting one archive (TAR_DIRS dirs ×
# TAR_FILES files of 1–4 KiB, built once in the scratch) repeatedly into fresh
# directories for the measured span; the number is the wire census Δ ÷
# entries extracted, with entries/s beside. Extracted trees are removed after
# the span (outside the record).
ROW_KIND=metadata
ROW_NEEDS_SHIM=0
TAR_DIRS=${TAR_DIRS:-500}
TAR_FILES=${TAR_FILES:-20}
TAR_PAR=${TAR_PAR:-$BRK_NUMJOBS}
TAR_ARCHIVE="$BRK_SCRATCH/tree.tar"
ROW_INSTRUMENT="tar -x of a ${TAR_DIRS}×${TAR_FILES} synthetic tree (1–4 KiB files), $TAR_PAR concurrent extractions, repeated for the span"

row_prepare() {   # mnt
  mkdir -p "$1/tar"
  if [ ! -f "$TAR_ARCHIVE" ]; then
    local src="$BRK_SCRATCH/tree" d f
    mkdir -p "$src"
    for d in $(seq 1 "$TAR_DIRS"); do
      mkdir -p "$src/d$d"
      for f in $(seq 1 "$TAR_FILES"); do
        head -c $(( 1024 + (d * 31 + f * 7) % 3072 )) /dev/urandom > "$src/d$d/f$f"
      done
    done
    tar -C "$src" -cf "$TAR_ARCHIVE" .
    tar -tf "$TAR_ARCHIVE" | wc -l > "$TAR_ARCHIVE.entries"
    rm -rf "$src"
  fi
}

tar_loop() {   # base secs out
  local base=$1 secs=$2 out=$3
  local deadline=$(( $(date +%s) + secs )) t0 reps=0
  t0=$(date +%s%N)
  # TAR_PAR extractions in flight at a time, each into its own fresh directory.
  while [ "$(date +%s)" -lt "$deadline" ]; do
    local pids=() k
    for k in $(seq 1 "$TAR_PAR"); do
      reps=$((reps + 1))
      mkdir -p "$base/x$reps"
      tar -C "$base/x$reps" -xf "$TAR_ARCHIVE" &
      pids+=($!)
    done
    wait "${pids[@]}"
  done
  local t1; t1=$(date +%s%N)
  python3 - "$out.tar.json" "$reps" "$(cat "$TAR_ARCHIVE.entries")" "$t0" "$t1" <<'EOF'
import json, sys
out, reps, entries, t0, t1 = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
el = (t1 - t0) / 1e9
json.dump({"instrument": "tar", "extractions": reps, "entries_per_archive": entries, "ops": reps * entries,
           "entries": reps * entries, "bytes": 0, "elapsed_s": el, "entries_per_s": reps * entries / el}, open(out, "w"))
EOF
}

row_run() {   # mnt secs outprefix phase
  local mnt=$1 secs=$2 out=$3 phase=$4
  local base="$mnt/tar/$phase-$BRK_LEG"
  mkdir -p "$base"
  echo "$base" >> "$BRK_LEG_DIR/tar-dirs"
  bounded_run "$secs" "$out" tar_loop "$base" "$secs" "$out"
}

row_metrics() {
  python3 - "$1.tar.json" "$BRK_LEG_DIR" <<'EOF'
import json, sys, os
sys.path.insert(0, os.environ["BRK_HERE"])
import brk
m = json.load(open(sys.argv[1]))
# The wire census over the leg's own before/after snapshots (the harness takes
# them around this very run).
b = brk.terms(brk.load_stats(os.path.join(sys.argv[2], "before.stats.json")))
a = brk.terms(brk.load_stats(os.path.join(sys.argv[2], "after.stats.json")))
wire = sum(a[k] - b[k] for k in brk.WIRE_RPCS)
m["wire_rpcs"] = wire
m["wire_verbs_per_entry"] = wire / m["entries"] if m["entries"] else 0.0
m["journal_entries_per_entry"] = (a["meta_kv_journal_entries"] - b["meta_kv_journal_entries"]) / m["entries"] if m["entries"] else 0.0
m["headline"] = {"name": "wire verbs/entry", "value": m["wire_verbs_per_entry"]}
m["notes"] = "entries/s %.0f; journal entries per tar entry %.3f (a file = create + write + utimes/chmod)" % (m["entries_per_s"], m["journal_entries_per_entry"])
json.dump(m, sys.stdout)
EOF
}

row_cleanup() {
  [ -f "$BRK_LEG_DIR/tar-dirs" ] || return 0
  while read -r d; do rm -rf "$d"; done < "$BRK_LEG_DIR/tar-dirs"
  rm -f "$BRK_LEG_DIR/tar-dirs"
}
