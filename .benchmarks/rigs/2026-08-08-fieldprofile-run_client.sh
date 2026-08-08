#!/bin/bash
set -u
OUT=/scratch/tmp/profile-0808
MNT=/scratch/tmp/test
SHIM=/scratch/tmp/libsqueezefs_il.so
cat $MNT/.stats > $OUT/cli-pre.json
( sleep 12
  PIDS=$(pgrep -x fio | tr "\n" "," | sed "s/,\$//")
  echo "fio pids: $PIDS" > $OUT/cli-tids.txt
  perf record -g --call-graph dwarf -o $OUT/client.perf -p "$PIDS" -- sleep 10 2>> $OUT/cli-tids.txt
) &
PROF=$!
LD_PRELOAD=$SHIM fio --name=crow --directory=$MNT/iops --nrfiles=1 --filesize=1g --numjobs=32 \
  --filename_format="sqzfio.\$jobnum.\$filenum" \
  --rw=randread --bs=4k --direct=1 --norandommap --randrepeat=0 \
  --ioengine=libaio --iodepth=32 --time_based --runtime=30 --ramp_time=5 \
  --group_reporting --output-format=json --output=$OUT/cli-fio.json \
  > /dev/null 2>&1
RC=$?
wait $PROF 2>/dev/null
cat $MNT/.stats > $OUT/cli-post.json
echo "fio rc=$RC"
ls -la $OUT/client.perf 2>/dev/null
