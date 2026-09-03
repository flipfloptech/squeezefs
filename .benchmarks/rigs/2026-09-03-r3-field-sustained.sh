#!/usr/bin/env bash
# .benchmarks/rigs/2026-09-03-r3-field-sustained.sh — the R-3 field
# SUSTAINED legs (the landing law's ≥ 60 s rows): B then A, kern and il
# rand-4k at 24×8 for 60 s + 10 s ramp each, on the file set the A-B-B-A
# driver minted. The job file's own `runtime=30` wins over any
# command-line `--runtime` (either side of the file), so the 60 s legs
# run a sed'ed copy of the field job with `runtime=60` — otherwise
# identical.
# Run as root on the box after 2026-09-03-r3-field-abba.sh.
set -eu
D=/scratch/tmp/sqz-agent/r3
OUT="${OUT:-$D/sustained-$(date +%Y%m%d-%H%M%S)}"
META="${META:-sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1}"
MNT=/scratch/tmp/test
JOBS=/scratch/tmp/fio_jobs
A_BIN=/scratch/tmp/squeezefs.kvmap
A_IL=/scratch/tmp/libsqueezefs_il.so.kvmap
B_BIN=$D/squeezefs.r3
B_IL=$D/libsqueezefs_il.so.r3
RT="${RT:-60}"
mkdir -p "$OUT"
JOB60="$D/randread_iops_${RT}.job"
sed "s/^runtime=30$/runtime=$RT/" "$JOBS/randread_iops.job" > "$JOB60"
grep -q "^runtime=$RT$" "$JOB60" || { echo "job rewrite failed" >&2; exit 1; }
exec > >(tee -a "$OUT/driver.log") 2>&1
echo "== R-3 field sustained legs: $(date -u +%FT%TZ) out=$OUT rt=$RT"
if ps -eo args | grep -q "[s]queezefs mount"; then echo "a daemon is already up — refusing" >&2; exit 2; fi

cur_bin=""
mount_arm() {
  local bin="$1" tag="$2"
  cur_bin=$bin
  env SQUEEZEFS_IPC_ALLOW_DEV=1 "$bin" mount "$META" "$MNT" --daemon --interception --allow-other \
    --log-file "$OUT/mount-$tag-$(date +%s).log"
  for _ in $(seq 1 90); do [ -r "$MNT/.stats" ] && grep -q '"fuse3_zc_replies"' "$MNT/.stats" && break; sleep 1; done
}
umount_arm() {
  "$cur_bin" umount "$MNT" || umount "$MNT" || true
  for _ in $(seq 1 60); do ps -eo args | grep -q "[s]queezefs mount" || break; sleep 1; done
  sleep 2
}
row() {  # tag mode il
  local tag="$1" mode="$2" il="${3:-}" pre=()
  [ "$mode" = il ] && pre=(env LD_PRELOAD="$il" SQUEEZEFS_IPC_ALLOW_DEV=1)
  echo "-- row $tag: randread_iops $mode ${RT}s (+10 ramp) loadavg=$(cut -d' ' -f1-3 /proc/loadavg)"
  sync; sleep 1
  cat "$MNT/.stats" > "$OUT/$tag.stats0"
  "${pre[@]}" fio "$JOB60" --output-format=json --output="$OUT/$tag.fio.json" \
    --write_bw_log="$OUT/$tag" --log_avg_msec=1000 > "$OUT/$tag.fio.txt" 2>&1 || { tail -3 "$OUT/$tag.fio.txt"; return 1; }
  cat "$MNT/.stats" > "$OUT/$tag.stats1"
  python3 "$D/2026-09-03-r3-row-delta.py" "$OUT" "$tag" | tee "$OUT/$tag.row" || true
}

mount_arm "$B_BIN" B
row B-rr4k-kern-60 kern
row B-rr4k-il-60 il "$B_IL"
umount_arm
mount_arm "$A_BIN" A
row A-rr4k-kern-60 kern
row A-rr4k-il-60 il "$A_IL"
umount_arm
echo "== done $(date -u +%FT%TZ)"
grep -h "^ROW" "$OUT"/*.row
