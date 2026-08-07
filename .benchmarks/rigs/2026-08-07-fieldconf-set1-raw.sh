#!/usr/bin/env bash
# Set 1 same-day RAW calibration rows (destructive to the data namespaces —
# runs between cluster resets; the venue convention is fresh-reset-per-use).
# Grades the venue against the reference raw class (49.7 GB/s spread /
# per-connection wall single): 1 submitter/dev qd16 vs 6 submitters/dev qd3
# (approx same in-flight per device), bs=4M, 30 s sustained, libaio direct.
set -u
OUT="${OUT:-/scratch/tmp/fieldconf-0807/set1}"
DEVS=(/dev/nvme10n1 /dev/nvme12n1 /dev/nvme14n1 /dev/nvme16n1 /dev/nvme18n1)
fatal() { echo "FATAL: $*" >&2; exit 1; }
mountpoint -q /scratch/tmp/test && fatal "raw rows refused: FS still mounted"
for d in "${DEVS[@]}"; do
  fuser "$d" >/dev/null 2>&1 && fatal "raw leg refused: $d is held"
done
raw_row() { # tag numjobs-per-dev iodepth
  local args=() i=0
  for d in "${DEVS[@]}"; do
    args+=(--name="w$i" --filename="$d" --numjobs="$2" --iodepth="$3")
    i=$((i+1))
  done
  fio --ioengine=libaio --direct=1 --rw=write --bs=4M --size=40g \
    --time_based --runtime=30 --ramp_time=5 --group_reporting \
    --output-format=json --output="$OUT/raw-$1.json" "${args[@]}" >/dev/null 2>&1 \
    || fatal "raw row $1 failed"
  python3 -c "
import json
f = json.load(open('$OUT/raw-$1.json'))
bw = sum(j['write']['bw_bytes'] for j in f['jobs']) / 1e9
print('  raw $1: %.2f GB/s' % bw)"
}
echo "=== raw calibration (same-day venue grading; libaio direct bs=4M) ==="
raw_row 1sub-qd16 1 16
raw_row 6sub-qd3 6 3
