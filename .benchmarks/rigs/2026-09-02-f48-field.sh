#!/usr/bin/env bash
# Finding 48 field row (squeeze-test): the promotion-first block's warm vs
# durable bytes across a clean unmount. $1 = binary label
# (/scratch/tmp/squeezefs.$1), $2 = mode: `ab` (the f46 A/B fingerprint
# venue — 24 x 8 GiB, crc32c headers, NOT time_based) or `bw`
# (/scratch/tmp/fio_jobs/write_BW.job once). Fresh cluster reset per run.
#
# Evidence per run: .stats pre/post-write/post-warm/post-close; warm md5 of
# three files + a dd of block 0 of EVERY file; overlay_open after close;
# umount; remount; cold md5 + block-0 dd + per-file cmp; a REAL crc32c
# verification (a READ job with verify=crc32c — `--verify_only` on the
# write job is a no-op: it reports err=0 on a corrupted local file).
set -uo pipefail
LABEL=$1; MODE=${2:-ab}
SQZ=/scratch/tmp/squeezefs.$LABEL
MP=/scratch/tmp/test
META="sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1"
TS=$(date +%Y%m%d-%H%M%S)
OUT=/scratch/tmp/f48-$LABEL-$MODE-$TS; LOG=/scratch/tmp/logs/sqz-f48-$LABEL-$MODE-$TS.log
mkdir -p "$OUT/warm" "$OUT/cold"
echo "== $LABEL/$MODE: $($SQZ --version | head -1)"
echo "-- reset"
echo YES | sudo -n /scratch/tmp/cluster_reset_v4.sh >"$OUT/reset.out" 2>&1; echo "reset exit=$?"
mountpoint -q "$MP" && { sudo -n "$SQZ" umount "$MP" >/dev/null 2>&1; sleep 2; }
mount_fs() {
  sudo -n "$SQZ" mount "$META" "$MP" --daemon --interception --allow-other --log-file "$1" 2>&1 | tail -1
  for i in $(seq 1 60); do mountpoint -q "$MP" && grep -q "transport enabled" "$1" 2>/dev/null && break; sleep 1; done
  mountpoint -q "$MP" || { echo "MOUNT FAILED"; exit 1; }
}
umount_fs() {
  sudo -n "$SQZ" umount "$MP" 2>&1 | tail -1
  for i in $(seq 1 120); do mountpoint -q "$MP" || break; sleep 1; done
  mountpoint -q "$MP" && sudo -n umount "$MP"; sleep 2
}
stats() { cat "$MP/.stats" > "$OUT/$1.json"; }
delta() {
  python3 - "$OUT/$1.json" "$OUT/$2.json" "${@:3}" <<'EOF'
import json,sys
a=json.load(open(sys.argv[1]))['metrics']; b=json.load(open(sys.argv[2]))['metrics']
for k in sys.argv[3:]:
    print(f"  {k}: {a.get(k)} -> {b.get(k)}")
EOF
}
mount_fs "$LOG"
if [ "$MODE" = ab ]; then
  DIR=$MP/ab; mkdir -p "$DIR"; FMT='ab.$jobnum.$filenum.root'; F0=ab.0.0.root; F1=ab.1.0.root; F2=ab.2.0.root
  cat > "$OUT/w.job" <<EOJ
[ab]
group_reporting=1
ioengine=libaio
readwrite=write
direct=1
bs=1M
size=8g
iodepth=16
fallocate=none
verify=crc32c
do_verify=0
filename_format=$FMT
numjobs=24
directory=$DIR/
EOJ
else
  DIR=$MP/client_validation; mkdir -p "$DIR"; FMT='test.$jobnum.$filenum.root'; F0=test.0.0.root; F1=test.1.0.root; F2=test.2.0.root
  cp /scratch/tmp/fio_jobs/write_BW.job "$OUT/w.job"
fi
stats pre
fio "$OUT/w.job" --output-format=json --output="$OUT/w.fio.json" >/dev/null 2>&1; echo "fio write exit=$?"
sleep 2
stats post_write
echo "-- after write+close:"
delta pre post_write overlay_open overlay_stores overlay_overwrite_installs overlay_publishes overlay_epoch_feeds rewrite_shadow_open_epochs write_lock_scope_entire layout_striped_writes
echo "-- warm: md5 of 3 files, block-0 dd of every file"
for f in $F0 $F1 $F2; do md5sum "$DIR/$f"; done | tee "$OUT/md5.warm"
for f in "$DIR"/*.root; do dd if="$f" of="$OUT/warm/$(basename "$f").b0" bs=4M count=1 status=none; done
stats post_warm
echo "-- warm window deltas:"
delta post_write post_warm overlay_read_serves overlay_read_gap_serves overlay_read_drains overlay_gap_seeds overlay_epoch_feeds overlay_open rewrite_shadow_open_epochs rewrite_shadow_swaps
umount_fs
grep -E "dismount closed|Dismount clean|epoch close.*failed" "$LOG" | tail -3
mount_fs "$LOG-s2"
echo "-- cold: md5 of 3 files, block-0 dd of every file"
for f in $F0 $F1 $F2; do md5sum "$DIR/$f"; done | tee "$OUT/md5.cold"
for f in "$DIR"/*.root; do dd if="$f" of="$OUT/cold/$(basename "$f").b0" bs=4M count=1 status=none; done
if diff <(cut -d' ' -f1 "$OUT/md5.warm") <(cut -d' ' -f1 "$OUT/md5.cold") >/dev/null; then echo "RESULT $LABEL/$MODE: MD5 IDENTICAL warm vs cold (3 files)"; else echo "RESULT $LABEL/$MODE: MD5 MISMATCH warm vs cold"; fi
bad=0
for w in "$OUT"/warm/*.b0; do
  c="$OUT/cold/$(basename "$w")"
  if ! cmp -s "$w" "$c"; then bad=$((bad+1)); echo "  block-0 DIFF $(basename "$w" .b0): $(cmp "$w" "$c" 2>&1 | head -1); cold zero bytes in [1M,4M): $(tail -c +1048577 "$c" | tr -d '\0' | wc -c | awk '{print 3145728-$1}')"; fi
done
echo "RESULT $LABEL/$MODE: block-0 warm-vs-cold diffs: $bad of $(ls "$OUT"/warm/*.b0 | wc -l) files"
if [ "$MODE" = ab ]; then
  sed 's/readwrite=write/readwrite=read/; s/do_verify=0/do_verify=1/' "$OUT/w.job" > "$OUT/v.job"
  fio "$OUT/v.job" --output="$OUT/verify.txt" >/dev/null 2>&1; echo "cold crc32c READ-verify exit=$? failures=$(grep -c 'verify failed' "$OUT/verify.txt")"
fi
umount_fs
echo "== DONE $OUT"
