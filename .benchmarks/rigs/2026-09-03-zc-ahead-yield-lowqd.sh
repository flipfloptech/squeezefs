#!/usr/bin/env bash
# .benchmarks/rigs/2026-09-03-zc-ahead-yield-lowqd.sh — the premise probe:
# the ahead lane's classic win is a SINGLE low-qd stream whose fabric RTT
# nothing else hides. The field job (24 × qd16) never exercises that
# shape, so this leg runs read_BW.job rewritten to numjobs=1 at iodepth 1
# and 4 (20 s + 5 s ramp) on both arms, fresh mount per arm, on the file
# set the A-B-B-A left behind. Kern mode (the shim routes 1 MiB to the
# kernel lane anyway). Writes ONLY under /scratch/tmp/sqz-agent/.
#   bash /scratch/tmp/sqz-agent/2026-09-03-zc-ahead-yield-lowqd.sh <out>
set -eu
D=/scratch/tmp/sqz-agent
OUT="$1"
META="sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1"
MNT=/scratch/tmp/test
JOBS=/scratch/tmp/fio_jobs
A_BIN=/scratch/tmp/squeezefs
B_BIN=$D/squeezefs.zcay
DELTA=$D/2026-09-03-zc-ahead-yield-row-delta.py
mkdir -p "$OUT"
exec > >(tee -a "$OUT/driver.log") 2>&1
echo "== zc-ahead-yield low-qd premise probe: $(date -u +%FT%TZ) out=$OUT"
if ps -eo args | grep -q "[s]queezefs mount"; then echo "a daemon is already up — refusing" >&2; exit 2; fi
for qd in 1 4; do
  sed -e "s/^numjobs=.*/numjobs=1/" -e "s/^iodepth=.*/iodepth=$qd/" -e "s/^runtime=.*/runtime=20/" \
      -e "s/^ramp_time=.*/ramp_time=5/" "$JOBS/read_BW.job" > "$OUT/seq1m-1j-qd$qd.job"
done

cur_bin=""
mount_arm() {
  local arm="$1" tag="$2"
  case "$arm" in A) cur_bin=$A_BIN ;; B) cur_bin=$B_BIN ;; esac
  "$cur_bin" mount "$META" "$MNT" --daemon --interception --allow-other --log-file "$OUT/mount-$tag.log"
  for _ in $(seq 1 120); do
    [ -r "$MNT/.stats" ] && grep -q '"fuse3_zc_replies"' "$MNT/.stats" && break
    sleep 1
  done
}
umount_arm() {
  "$cur_bin" umount "$MNT" || umount "$MNT" || true
  for _ in $(seq 1 60); do ps -eo args | grep -q "[s]queezefs mount" || break; sleep 1; done
  sleep 2
}
row() {  # $1 = tag, $2 = jobfile
  local tag="$1" jobfile="$2"
  echo "-- row $tag: $(basename "$jobfile") loadavg=$(cut -d' ' -f1-3 /proc/loadavg) $(date -u +%T)"
  sync; sleep 2
  cat "$MNT/.stats" > "$OUT/$tag.stats0"
  fio "$jobfile" --output-format=json --output="$OUT/$tag.fio.json" \
    --write_bw_log="$OUT/$tag" --log_avg_msec=1000 > "$OUT/$tag.fio.txt" 2>&1 ||
    { echo "fio failed: $(tail -3 "$OUT/$tag.fio.txt")"; return 1; }
  cat "$MNT/.stats" > "$OUT/$tag.stats1"
  python3 "$DELTA" "$OUT" "$tag" | tee "$OUT/$tag.row" || true
}

for arm in A B B A; do
  n=$(ls "$OUT" | grep -c "^mount-$arm" || true)
  tag="$arm$((n + 1))"
  mount_arm "$arm" "$tag"
  row "$tag-seq1m-1j-qd1" "$OUT/seq1m-1j-qd1.job"
  row "$tag-seq1m-1j-qd4" "$OUT/seq1m-1j-qd4.job"
  umount_arm
done
echo "== done $(date -u +%FT%TZ)"
grep -h "^ROW" "$OUT"/*.row
