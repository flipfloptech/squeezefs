#!/usr/bin/env bash
# .benchmarks/rigs/2026-09-03-zc-ahead-yield-pinned.sh — the falsification
# leg for the governor's verdict: on the lever binary (B), pin the ahead
# lane's depth (`SQUEEZEFS_READ_LANE_DEPTH=<blocks>`) so the lane holds a
# fixed read-ahead depth instead of probe-adopt-retreat, and read the kern
# 1 MiB row against the same leg's un-pinned control. If the pinned depth
# beats the governed row, the governor is timid; if it does not, the
# fabric is the wall and the governor's retreat was correct. Reuses the
# A-B-B-A driver's functions on the file set the last leg left behind (no
# reset — reads do not age the store; the mount is fresh per pin).
#   bash /scratch/tmp/sqz-agent/2026-09-03-zc-ahead-yield-pinned.sh <out> <depth>...
set -eu
D=/scratch/tmp/sqz-agent
OUT="$1"; shift
META="sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1"
MNT=/scratch/tmp/test
JOBS=/scratch/tmp/fio_jobs
B_BIN=$D/squeezefs.zcay
DELTA=$D/2026-09-03-zc-ahead-yield-row-delta.py
mkdir -p "$OUT"
exec > >(tee -a "$OUT/driver.log") 2>&1
echo "== zc-ahead-yield pinned-depth legs: $(date -u +%FT%TZ) out=$OUT"
if ps -eo args | grep -q "[s]queezefs mount"; then echo "a daemon is already up — refusing" >&2; exit 2; fi

mount_b() {  # $1 = tag, $2.. = env
  local tag="$1"; shift
  env "$@" "$B_BIN" mount "$META" "$MNT" --daemon --interception --allow-other --log-file "$OUT/mount-$tag.log"
  for _ in $(seq 1 120); do
    [ -r "$MNT/.stats" ] && grep -q '"fuse3_zc_replies"' "$MNT/.stats" && break
    sleep 1
  done
}
umount_b() {
  "$B_BIN" umount "$MNT" || umount "$MNT" || true
  for _ in $(seq 1 60); do ps -eo args | grep -q "[s]queezefs mount" || break; sleep 1; done
  sleep 2
}
row() {  # $1 = tag
  local tag="$1"
  echo "-- row $tag: read_BW kern 30s loadavg=$(cut -d' ' -f1-3 /proc/loadavg) $(date -u +%T)"
  sync; sleep 2
  cat "$MNT/.stats" > "$OUT/$tag.stats0"
  fio "$JOBS/read_BW.job" --output-format=json --output="$OUT/$tag.fio.json" \
    --write_bw_log="$OUT/$tag" --log_avg_msec=1000 > "$OUT/$tag.fio.txt" 2>&1 ||
    { echo "fio failed: $(tail -3 "$OUT/$tag.fio.txt")"; return 1; }
  cat "$MNT/.stats" > "$OUT/$tag.stats1"
  python3 "$DELTA" "$OUT" "$tag" | tee "$OUT/$tag.row" || true
}

for depth in "$@"; do
  mount_b "pin$depth" SQUEEZEFS_READ_LANE_DEPTH="$depth"
  row "B-pin$depth-seq1m-kern"
  umount_b
done
mount_b governed
row "B-governed-seq1m-kern"
umount_b
echo "== done $(date -u +%FT%TZ)"
grep -h "^ROW" "$OUT"/*.row
