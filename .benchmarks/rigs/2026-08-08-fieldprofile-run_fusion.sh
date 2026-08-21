#!/usr/bin/env bash
# fusion field-confirmation leg: kernel-lane rand-4k WRITE 32xqd8
set -u
LEG=$1
OUT=/scratch/tmp/profile-0808
MNT=/scratch/tmp/test
cat $MNT/.stats > $OUT/fus-$LEG-pre.json
cat /proc/diskstats > $OUT/fus-$LEG-dsk-pre.txt
fio --name=fus --directory=$MNT/iops --nrfiles=1 --filesize=1g --numjobs=32 \
  --filename_format="sqzfio.\$jobnum.\$filenum" \
  --rw=randwrite --bs=4k --direct=1 --norandommap --randrepeat=0 \
  --ioengine=libaio --iodepth=8 --time_based --runtime=60 --ramp_time=5 \
  --group_reporting --output-format=json --output=$OUT/fus-$LEG-fio.json \
  > /dev/null 2>&1
echo "fio rc=$?"
cat $MNT/.stats > $OUT/fus-$LEG-post.json
cat /proc/diskstats > $OUT/fus-$LEG-dsk-post.txt
