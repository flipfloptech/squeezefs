# The fio-driven rows' common body. A row sets FIO_JOB, FIO_RW, FIO_DIRECT,
# FIO_BS, FIO_PREFIX, FIO_HEADLINE (mib_per_s | iops) and optionally
# FIO_IOENGINE / FIO_SIZE, then sources this file. The dataset
# (`<mnt>/bracket/<prefix>.<job>`) is laid down ONCE and never recreated: it
# IS the aging store; a write leg overwrites the same inodes, a read leg reads
# the same bytes.
FIO_IOENGINE=${FIO_IOENGINE:-${BRK_IOENGINE:-libaio}}
FIO_SIZE=${FIO_SIZE:-${BRK_FILE_SIZE:-1g}}
if [ "$FIO_IOENGINE" = psync ]; then
  # psync is synchronous: the queue depth IS the job count.
  ROW_INSTRUMENT="$(fio --version 2>/dev/null) psync ${FIO_RW} bs=${FIO_BS} direct=${FIO_DIRECT} QD1×numjobs=${BRK_NUMJOBS} size=${FIO_SIZE}/job"
else
  ROW_INSTRUMENT="$(fio --version 2>/dev/null) $FIO_IOENGINE ${FIO_RW} bs=${FIO_BS} direct=${FIO_DIRECT} iodepth=${BRK_IODEPTH} numjobs=${BRK_NUMJOBS} size=${FIO_SIZE}/job"
fi

row_prepare() {   # mnt
  local mnt=$1
  mkdir -p "$mnt/bracket"
  if [ ! -f "$mnt/bracket/$FIO_PREFIX.0" ]; then
    DIRECTORY="$mnt/bracket" PREFIX=$FIO_PREFIX SIZE=$FIO_SIZE NUMJOBS=$BRK_NUMJOBS \
      fio --output-format=json --output="$BRK_LEG_DIR/prepare.fio.json" "$BRK_JOBS/prepare-seq.fio"
  fi
}

row_run() {   # mnt secs outprefix phase
  local mnt=$1 secs=$2 out=$3
  DIRECTORY="$mnt/bracket" RW=$FIO_RW DIRECT=$FIO_DIRECT IOENGINE=$FIO_IOENGINE SIZE=$FIO_SIZE \
  RUNTIME=$secs NUMJOBS=$BRK_NUMJOBS IODEPTH=$BRK_IODEPTH \
    bounded_run "$secs" "$out" with_preload fio --output-format=json --output="$out.fio.json" "$BRK_JOBS/$FIO_JOB"
}

row_metrics() {   # outprefix → JSON on stdout
  python3 - "$1.fio.json" "$FIO_BS" "$FIO_DIRECT" "$FIO_HEADLINE" <<'EOF'
import json, sys, os
sys.path.insert(0, os.environ["BRK_HERE"])
import brk
m = brk.fio_metrics(sys.argv[1])
bs = sys.argv[2]
units = {"k": 1 << 10, "m": 1 << 20, "g": 1 << 30}
m["bs"] = int(bs[:-1]) * units[bs[-1].lower()] if bs[-1].lower() in units else int(bs)
m["direct"] = int(sys.argv[3])
d = m[m["direction"]]
if sys.argv[4] == "iops":
    m["headline"] = {"name": "IOPS", "value": d["iops"]}
else:
    m["headline"] = {"name": "MiB/s", "value": d["mib_per_s"]}
json.dump(m, sys.stdout)
EOF
}

row_cleanup() { :; }
