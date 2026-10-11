# Sequential read with an EARLY STOP (decision 0003 §K row 5): N workers cycle
# over ES_FILES files of ES_FILE_MB MiB, each open reading the first ES_HEAD_MB
# MiB in 1 MiB O_DIRECT reads then closing — the `head`/thumbnail/sampler
# shape. The device cost beyond the user's bytes is the prefetch pipeline's
# overshoot past the stop; the bound is ONE start window per stream (the AIMD
# start of 2 blocks = 8 MiB at the 4 MiB block): 1 + 8 MiB ÷ ES_HEAD_MB. The
# file set (default 96 × 64 MiB = 6 GiB, 768 MiB of heads per cycle) exceeds
# the venue's RAM tiers so every cycle misses.
ROW_KIND=data-read
ROW_NEEDS_SHIM=0
ES_FILES=${ES_FILES:-96}
ES_FILE_MB=${ES_FILE_MB:-64}
ES_HEAD_MB=${ES_HEAD_MB:-8}
ES_BS=$((1 << 20))
ES_WORKERS=${ES_WORKERS:-$BRK_NUMJOBS}
ROW_INSTRUMENT="early_stop.py: ${ES_WORKERS} workers, ${ES_FILES} files × ${ES_FILE_MB} MiB, first ${ES_HEAD_MB} MiB per open in 1 MiB O_DIRECT reads"
# bound = 1 + (R2_WINDOW_START × block) ÷ head, × (1 + f)
ES_BOUND=$(python3 -c "print(round((1 + (2 * 4) / $ES_HEAD_MB) * 1.05, 4))")

row_prepare() {   # mnt
  local mnt=$1
  mkdir -p "$mnt/bracket"
  if [ ! -f "$mnt/bracket/early.0" ]; then
    DIRECTORY="$mnt/bracket" PREFIX=early NRFILES=$ES_FILES FILESIZE=${ES_FILE_MB}m \
      fio --output-format=json --output="$BRK_LEG_DIR/prepare.fio.json" "$BRK_JOBS/prepare-files.fio"
  fi
}

row_run() {   # mnt secs outprefix phase
  local mnt=$1 secs=$2 out=$3
  bounded_run "$secs" "$out" python3 "$BRK_HERE/early_stop.py" --dir "$mnt/bracket" --prefix early --files "$ES_FILES" \
    --head $((ES_HEAD_MB << 20)) --bs "$ES_BS" --workers "$ES_WORKERS" --runtime "$secs" --output "$out.fio.json"
}

row_metrics() {   # outprefix → JSON on stdout
  python3 - "$1.fio.json" "$ES_BOUND" "$ES_HEAD_MB" <<'PYEOF'
import json, sys, os
sys.path.insert(0, os.environ["BRK_HERE"])
import brk
m = brk.fio_metrics(sys.argv[1])
m["bs"] = 1 << 20
m["direct"] = 1
m["headline"] = {"name": "MiB/s", "value": m["read"]["mib_per_s"]}
m["read_amp_bound"] = float(sys.argv[2])
m["read_amp_bound_why"] = "1 + R2_WINDOW_START (2) × block (4 MiB) ÷ head (%s MiB): one start window of speculation per stream, × (1 + f)" % sys.argv[3]
m["read_amp_exclude"] = []
json.dump(m, sys.stdout)
PYEOF
}

row_cleanup() { :; }
