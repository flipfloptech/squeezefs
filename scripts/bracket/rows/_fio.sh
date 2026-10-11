# The fio-driven rows' common body. A row sets FIO_JOB, FIO_RW, FIO_DIRECT,
# FIO_BS, FIO_PREFIX, FIO_HEADLINE (mib_per_s | iops) and optionally
# FIO_IOENGINE / FIO_SIZE / FIO_BLOCKALIGN (the offset grain of a random
# row; default = bs) / FIO_WARMUP_JOB + FIO_WARMUP_RW + FIO_WARMUP_BS (a
# different job for the warm-up span: the read-after-small-write shape
# writes in the warm-up and reads in the measured span) / FIO_READ_AMP_BOUND
# + FIO_READ_AMP_WHY (the row's derived `device ÷ user` bound, decision 0003
# §K — the leg's `read_amplification_bound` verdict) / FIO_READ_AMP_EXCLUDE
# (device-read arms excluded from the bounded ratio, space-separated; still
# reported), then sources this file. The dataset (`<mnt>/bracket/<prefix>.
# <job>`) is laid down ONCE and never recreated: it IS the aging store; a
# write leg overwrites the same inodes, a read leg reads the same bytes.
FIO_IOENGINE=${FIO_IOENGINE:-${BRK_IOENGINE:-libaio}}
FIO_SIZE=${FIO_SIZE:-${BRK_FILE_SIZE:-1g}}
FIO_BLOCKALIGN=${FIO_BLOCKALIGN:-$FIO_BS}
if [ "$FIO_IOENGINE" = psync ]; then
  # psync is synchronous: the queue depth IS the job count.
  ROW_INSTRUMENT="$(fio --version 2>/dev/null) psync ${FIO_RW} bs=${FIO_BS} blockalign=${FIO_BLOCKALIGN} direct=${FIO_DIRECT} QD1×numjobs=${BRK_NUMJOBS} size=${FIO_SIZE}/job"
else
  ROW_INSTRUMENT="$(fio --version 2>/dev/null) $FIO_IOENGINE ${FIO_RW} bs=${FIO_BS} blockalign=${FIO_BLOCKALIGN} direct=${FIO_DIRECT} iodepth=${BRK_IODEPTH} numjobs=${BRK_NUMJOBS} size=${FIO_SIZE}/job"
fi
if [ -n "${FIO_WARMUP_JOB:-}" ]; then
  ROW_INSTRUMENT="$ROW_INSTRUMENT; warm-up ${FIO_WARMUP_RW} bs=${FIO_WARMUP_BS} (${FIO_WARMUP_JOB})"
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
  local mnt=$1 secs=$2 out=$3 phase=$4
  local job=$FIO_JOB rw=$FIO_RW bs=$FIO_BS
  if [ "$phase" = warmup ] && [ -n "${FIO_WARMUP_JOB:-}" ]; then
    job=$FIO_WARMUP_JOB; rw=$FIO_WARMUP_RW; bs=$FIO_WARMUP_BS
  fi
  DIRECTORY="$mnt/bracket" RW=$rw BS=$bs BLOCKALIGN=$FIO_BLOCKALIGN DIRECT=$FIO_DIRECT IOENGINE=$FIO_IOENGINE SIZE=$FIO_SIZE \
  RUNTIME=$secs NUMJOBS=$BRK_NUMJOBS IODEPTH=$BRK_IODEPTH PREFIX=$FIO_PREFIX \
    bounded_run "$secs" "$out" with_preload fio --output-format=json --output="$out.fio.json" "$BRK_JOBS/$job"
}

row_metrics() {   # outprefix → JSON on stdout
  python3 - "$1.fio.json" "$FIO_BS" "$FIO_DIRECT" "$FIO_HEADLINE" "${FIO_READ_AMP_BOUND:-}" "${FIO_READ_AMP_WHY:-}" "${FIO_READ_AMP_EXCLUDE:-}" <<'EOF'
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
if sys.argv[5]:
    m["read_amp_bound"] = float(sys.argv[5])
    m["read_amp_bound_why"] = sys.argv[6]
    m["read_amp_exclude"] = sys.argv[7].split()
json.dump(m, sys.stdout)
EOF
}

row_cleanup() { :; }
