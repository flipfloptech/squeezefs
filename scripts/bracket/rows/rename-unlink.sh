# rename / unlink journal entries per op (charter §13: ≤ 1.05 entries/op).
# Instrument: `renames.py` — rounds of (create a pool of M files, UNTIMED) →
# (rename every file, then unlink every file, TIMED, bracketed by `.stats`
# snapshots) until the timed spans sum to the measured span. The number is
# Δ`meta_kv_journal_entries` ÷ (renames + unlinks) over the timed spans alone
# (PAD entries are the sector-pad law's framing, reported beside, not counted;
# the writer heartbeat and checkpoints add ≤ a handful of entries a minute and
# are stated as the bias). Throughput (ops/s) is reported, not judged.
ROW_KIND=metadata
ROW_NEEDS_SHIM=0
RU_POOL=${RU_POOL:-20000}
RU_THREADS=${RU_THREADS:-$BRK_NUMJOBS}
ROW_INSTRUMENT="renames.py pool=$RU_POOL threads=$RU_THREADS (rename→unlink rounds; journal entries ÷ ops over the timed spans)"

row_prepare() { mkdir -p "$1/ru"; }

row_run() {   # mnt secs outprefix phase
  local mnt=$1 secs=$2 out=$3 phase=$4
  bounded_run "$secs" "$out" python3 "$BRK_HERE/renames.py" "$mnt" "$mnt/ru/$phase-$BRK_LEG" "$secs" "$RU_POOL" "$RU_THREADS" > "$out.renames.json"
}

row_metrics() {
  python3 - "$1.renames.json" <<'EOF'
import json, sys
m = json.load(open(sys.argv[1]))
m["headline"] = {"name": "journal entries/op", "value": m["entries_per_op"]}
json.dump(m, sys.stdout)
EOF
}

row_cleanup() { rm -rf "$1/ru/warmup-$BRK_LEG" "$1/ru/measure-$BRK_LEG" 2>/dev/null || true; }
