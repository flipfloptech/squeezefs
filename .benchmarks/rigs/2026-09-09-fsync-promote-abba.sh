#!/usr/bin/env bash
# Pricing A/B for "fsync promotes a staged-layout file" (step 3 of the
# dismount-staged-residue program, .benchmarks/2026-09-09-dismount-staged-residue.md
# §7 item 3): ONE binary, the lever SQUEEZEFS_FSYNC_PROMOTE_STAGED as the arm
# (A = 0, the shipped default; B = 1), A-B-B-A. The venue MUST format WITH a
# staging dir (a cache-less set has no staged layout to promote) — on
# squeeze-test the local disk is SATA (slower than the fabric, the reason the
# fleet is cache-less), so the staging root is RAM-backed (/dev/shm) to
# isolate the promotion's own cost; the staging budget is sized so the rows
# stay under the 75 % high-water arm (pressure promotion would confound the
# lever). Rows per arm:
#   fsync-storm   24 × 256 KiB files, fsync per write (each file staged-layout
#                 → with the lever every fsync promotes) — THE pricing row
#   smallf-fsync  24 jobs × 64 KiB files, one fsync per file (create+fsync
#                 small files — the create/fsync contention the high-water
#                 arm was written to avoid)
#   wdur-kern     write_BW 1 MiB × 24 × qd16 + end_fsync (striped files —
#                 lever-neutral control)
#   rr4k-kern     randread 4 KiB (lever-neutral control)
# Every row: .stats / /proc/stat / thermal snapshots, fio JSON + bw log,
# the fsync ledger (fsync_phase_ns incl. the promotion phase,
# fsync_promoted_*). Reduce with 2026-09-08-campaign-rows-reduce.py <OUT>
# (arm letter = tag's first char).
#
#   sudo env BIN=/scratch/tmp/squeezefs-fsp [SEQ="A B B A"] [RT=30] \
#            [STAGING=/dev/shm/sqz-staging] [STAGING_SIZE=64GB] \
#        bash 2026-09-09-fsync-promote-abba.sh
set -u
BIN=${BIN:?BIN (the one binary) is required}
TS=$(date -u +%Y%m%d-%H%M%S)
OUT=${OUT:-/scratch/tmp/campaign/fsync-promote-$TS}
MNT=/scratch/tmp/test
JOBS=/scratch/tmp/fio_jobs
META=""   # per arm, from the reset's printed mount line
SEQ=${SEQ:-"A B B A"}
RT=${RT:-30}
STAGING=${STAGING:-/dev/shm/sqz-staging}
STAGING_SIZE=${STAGING_SIZE:-64GB}
RESET=/scratch/tmp/cluster_reset_v4.sh
mkdir -p "$OUT"
echo "== fsync-promote A/B $TS host $(hostname) kernel $(uname -r) seq [$SEQ] staging $STAGING ($STAGING_SIZE) out $OUT"
echo "   BIN: $("$BIN" --version | head -1)"

thermal() { for h in /sys/class/hwmon/hwmon*/temp*_input; do [ -r "$h" ] && echo "hwmon $(basename "$(dirname "$h")")/$(basename "$h")=$(cat "$h")"; done > "$1" 2>/dev/null; }

STORM="$OUT/fsync_storm.job"
cat > "$STORM" <<EOF
[fsync_storm]
group_reporting=1
ioengine=libaio
readwrite=write
direct=0
bs=256k
size=256m
iodepth=1
fsync=1
fallocate=none
time_based
runtime=$RT
ramp_time=5
filename_format=storm.\$jobnum.\$filenum
numjobs=24
directory=$MNT/client_validation/
EOF

# Small files: each job creates 64 KiB files one after another (nrfiles),
# fsync at the end of each file (fsync_on_close), time-bounded.
SMALLF="$OUT/smallf_fsync.job"
cat > "$SMALLF" <<EOF
[smallf_fsync]
group_reporting=1
ioengine=psync
readwrite=write
direct=0
bs=64k
filesize=64k
nrfiles=4000
file_service_type=sequential
fsync_on_close=1
fallocate=none
create_on_open=1
time_based
runtime=$RT
ramp_time=5
filename_format=smallf.\$jobnum.\$filenum
numjobs=24
directory=$MNT/client_validation/
EOF

row() {  # $1 = tag, $2 = jobfile, $3.. = extra fio args
  local tag="$1" jobfile="$2"; shift 2
  echo "-- row $tag: $(basename "$jobfile") $* loadavg=$(cut -d' ' -f1-3 /proc/loadavg)"
  thermal "$OUT/$tag.thermal0"
  sync; sleep 1
  cat "$MNT/.stats" > "$OUT/$tag.stats0"
  head -1 /proc/stat > "$OUT/$tag.procstat0"
  fio "$jobfile" "$@" --output-format=json --output="$OUT/$tag.fio.json" \
    --write_bw_log="$OUT/$tag" --log_avg_msec=1000 > "$OUT/$tag.fio.txt" 2>&1 \
    || { echo "fio failed: $(tail -3 "$OUT/$tag.fio.txt")"; return 1; }
  cat "$MNT/.stats" > "$OUT/$tag.stats1"
  head -1 /proc/stat > "$OUT/$tag.procstat1"
  thermal "$OUT/$tag.thermal1"
  python3 - "$OUT/$tag.stats0" "$OUT/$tag.stats1" <<'PY' || true
import json,sys
a=json.load(open(sys.argv[1]))["metrics"]; b=json.load(open(sys.argv[2]))["metrics"]
def d(k): return (b.get(k) or 0)-(a.get(k) or 0)
calls=d("fsync_calls")
fam=b.get("fsync_phase_ns") or {}; fam0=a.get("fsync_phase_ns") or {}
means={ph:((h["sum_ns"]-(fam0.get(ph) or {}).get("sum_ns",0))/max(h["count"]-(fam0.get(ph) or {}).get("count",0),1)/1e6) for ph,h in fam.items() if isinstance(h,dict) and "sum_ns" in h}
print(f"   fsync: calls {calls:,} promoted files {d('fsync_promoted_files'):,} bytes {d('fsync_promoted_bytes'):,} failures {d('fsync_promote_failures'):,} noops {d('fsync_promote_noops'):,} | staged resident now {b.get('nvme_staged_write_file_count', b.get('metrics',{}).get('nvme_staged_write_file_count'))} | phase means ms: "+" ".join(f"{k}={v:.2f}" for k,v in sorted(means.items())))
print(f"   staging: layout_staged_writes {d('layout_staged_writes'):,} durable_upload_bytes_writeback {d('durable_upload_bytes_writeback'):,} (the pressure/merge promotions' bytes) staged_payload_lost_reads {d('staged_payload_lost_reads')} | tripwires inv={d('invariant_tripwires')} overdue={d('transport_slots_overdue')} rescues={d('transport_park_tick_commit_rescues')}")
PY
  dmesg -T 2>/dev/null | grep -iE "fuse|WARN|lockdep|BUG" | tail -5 > "$OUT/$tag.dmesg" || true
  [ -s "$OUT/$tag.dmesg" ] && { echo "   dmesg tail:"; sed 's/^/     /' "$OUT/$tag.dmesg"; }
}

mount_arm() {  # $1 = arm letter (A → lever 0, B → lever 1)
  local lever=0; [ "$1" = B ] && lever=1
  pkill -x "$(basename "$BIN")" 2>/dev/null; pkill -x squeezefs 2>/dev/null; sleep 2
  umount -l "$MNT" 2>/dev/null
  rm -rf "$STAGING"; mkdir -p "$STAGING"
  # The reset script formats cache-less; re-run its CONFIG with a staging
  # dir (it honors CACHE_DIR → --disk-cache-paths) and the staging budget.
  sed -e "s#^CACHE_DIR=\"\"#CACHE_DIR=\"$STAGING\"#" "$RESET" > "$OUT/reset-staging.sh"
  grep -q "^CACHE_DIR=\"$STAGING\"" "$OUT/reset-staging.sh" || { echo "reset variant did not take CACHE_DIR"; exit 1; }
  local rlog="$OUT/reset-$1-$(date +%s).log"
  echo YES | bash "$OUT/reset-staging.sh" > "$rlog" 2>&1 || { echo "RESET FAILED ($1)"; exit 1; }
  grep -q "format: staging at" "$rlog" || { echo "RESET did not format with a staging dir ($1)"; exit 1; }
  # The meta URI comes from THIS reset's printed mount line: a re-attach can
  # renumber the namespaces (n1 → n2 while the previous connections drain),
  # so a hard-coded URI points at a zeroed superblock on the second arm.
  META="$(grep -oE 'sqmeta://[^ ]+' "$rlog" | tail -n 1)"
  [ -n "$META" ] || { echo "RESET printed no sqmeta URI ($1)"; exit 1; }
  # Every named namespace must be a BLOCK device: a mount that races the
  # re-attach can leave a regular FILE at the path (format then writes a
  # superblock into it and every later format refuses "already formatted").
  for d in $(echo "$META" | sed 's#sqmeta://##; s#,# #g'); do
    [ -b "$d" ] || { echo "META device $d is not a block device ($(stat -c %F "$d" 2>&1)) — aborting ($1)"; exit 1; }
  done
  echo "   meta: $META"
  # The mountpoint must be EMPTY (the rig's client_validation dir lands on
  # the underlying directory after an unmount and the daemon refuses a
  # non-empty mountpoint).
  mountpoint -q "$MNT" && umount -l "$MNT"; rm -rf "$MNT"/client_validation "$MNT"/.probe* 2>/dev/null
  [ -z "$(ls -A "$MNT" 2>/dev/null)" ] || { echo "MOUNTPOINT NOT EMPTY: $(ls -A "$MNT" | head -n 3 | tr '\n' ' ')"; exit 1; }
  SQUEEZEFS_FSYNC_PROMOTE_STAGED=$lever "$BIN" mount "$META" "$MNT" --daemon --allow-other \
    --disk-cache-size "$STAGING_SIZE" --log-file "$OUT/mount-$1-$(date +%s).log" > "$OUT/mount-$1.out" 2>&1
  tail -n 2 "$OUT/mount-$1.out"
  sleep 3
  [ -e "$MNT/.stats" ] || { echo "MOUNT FAILED ($1): no .stats at $MNT"; exit 1; }
  mkdir -p "$MNT/client_validation"
  echo "   arm $1: SQUEEZEFS_FSYNC_PROMOTE_STAGED=$lever; staging $(df -h "$STAGING" | tail -1 | awk '{print $2" total, "$3" used"}')"
  fio "$JOBS/write_BW.job" --runtime=40 --ramp_time=0 --output-format=json --output="$OUT/prep-$1.fio.json" > /dev/null 2>&1
}

i=0
for arm in $SEQ; do
  i=$((i+1))
  echo "##### arm $arm (position $i) $(date -u +%FT%TZ)"
  mount_arm "$arm"
  row "${arm}${i}-fsync-storm"  "$STORM"
  row "${arm}${i}-smallf-fsync" "$SMALLF"
  row "${arm}${i}-wdur-kern"    "$JOBS/write_BW.job"      --runtime="$RT" --end_fsync=1
  row "${arm}${i}-rr4k-kern"    "$JOBS/randread_iops.job" --runtime="$RT"
  echo "   staging after the arm: $(du -sh "$STAGING" 2>/dev/null | cut -f1) resident=$(python3 -c "import json;print(json.load(open('$MNT/.stats')).get('nvme_staged_write_file_count'))" 2>/dev/null)"
done
pkill -x "$(basename "$BIN")" 2>/dev/null; sleep 2; umount -l "$MNT" 2>/dev/null
echo "== done $(date -u +%FT%TZ): $OUT"
