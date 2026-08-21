#!/usr/bin/env bash
# r5 field ingress profile — the owed measurement (r5 note §6).
set -u
OUT=/scratch/tmp/profile-0808
MNT=/scratch/tmp/test
SHIM=/scratch/tmp/libsqueezefs_il.so
cat $MNT/.stats > $OUT/pre.json

# background samplers
( sleep 20; mpstat 1 15 > $OUT/mpstat.txt 2>&1 ) &
( sleep 20
  PID=$(pidof squeezefs | awk "{print \$1}")
  pidstat -t -p $PID 5 3 > $OUT/pidstat.txt 2>&1 ) &
( sleep 30; ss -tin state established "( dport = :4420 )" > $OUT/ss.txt 2>&1 ) &
( sleep 22
  PID=$(pidof squeezefs | awk "{print \$1}")
  TIDS=$(ps -T -p $PID -o spid=,comm= | awk "\$2 ~ /sqz-ipc-svc|sqz-ipc-dd/ {printf \"%s,\", \$1}" | sed "s/,\$//")
  echo "daemon tids: $TIDS" > $OUT/tids.txt
  perf record -g --call-graph dwarf -o $OUT/daemon.perf -t "$TIDS" -- sleep 18 2>> $OUT/tids.txt
  FPID=$(pgrep -x fio | head -1)
  [ -n "$FPID" ] && perf record -g --call-graph dwarf -o $OUT/fio.perf -p $FPID -- sleep 10 2>> $OUT/tids.txt
) &
PROF=$!

LD_PRELOAD=$SHIM fio --name=row --directory=$MNT/iops --nrfiles=1 --filesize=1g --numjobs=32 \
  --filename_format="sqzfio.\$jobnum.\$filenum" \
  --rw=randread --bs=4k --direct=1 --norandommap --randrepeat=0 \
  --ioengine=libaio --iodepth=32 --time_based --runtime=60 --ramp_time=10 \
  --group_reporting --output-format=json --output=$OUT/fio.json \
  > /dev/null 2>&1
RC=$?
wait $PROF 2>/dev/null
cat $MNT/.stats > $OUT/post.json
echo "fio rc=$RC"
ls -la $OUT/daemon.perf $OUT/fio.perf 2>/dev/null
