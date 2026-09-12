#!/usr/bin/env bash
# The 2026-09-08 campaign board's field rows on squeeze-test — an A-B-B-A
# whose arms are BINARIES (one boot, the same fabric set, the same fio
# jobs): E = the pre-campaign tip, F = the campaign tip (W-6 write-handler
# economy, R-5 read-handler economy, W-5 fsync economy, D-5 owner hop).
# Rows per arm: kern rand-4k read (R-5: rr_4k CPU/op), kern rand-4k write
# (W-6: rw_4k CPU/op), kern write_BW --end_fsync (W-5: w_durable), and the
# small-file FSYNC STORM (W-5's own shape: 24 jobs × 256 KiB files, fsync
# per write, 30 s). The il control row pins attribution (the shim path
# takes neither handler). Every row: thermal / .stats / /proc/stat
# snapshots around fio, the row-delta analyzer, dmesg tail.
#
#   sudo env BIN_E=/scratch/tmp/sqz-agent/day2/squeezefs-E \
#            BIN_F=/scratch/tmp/sqz-agent/day2/squeezefs-F \
#            [IL_E=... IL_F=...] [SEQ="E F F E"] [RT=30] \
#        bash 2026-09-08-campaign-rows-abba.sh
#
# Artifacts: $OUT/<arm><n>-<row>.{stats0,stats1,procstat0,procstat1,
# thermal0,thermal1,fio.json,row,dmesg}; reduce with
# 2026-09-08-campaign-rows-reduce.py <OUT> (the arm letter is the tag's
# first char, the A-B-B-A position its second; the write and fsync rows
# carry their own columns — the R-4 per-row analyzer invoked below knows
# read jobs only, so its .row files are empty for them).
set -u
TS=$(date -u +%Y%m%d-%H%M%S)
OUT=${OUT:-/scratch/tmp/campaign/rows-$TS}
MNT=/scratch/tmp/test
JOBS=/scratch/tmp/fio_jobs
META="sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1"
DELTA=${DELTA:-/scratch/tmp/rigs/2026-09-03-r4-row-delta.py}
SEQ=${SEQ:-"E F F E"}
RT=${RT:-30}
mkdir -p "$OUT"
echo "== campaign rows $TS host $(hostname) kernel $(uname -r) seq [$SEQ] out $OUT"
for a in E F; do
  v="BIN_$a"; [ -x "${!v:-}" ] || { echo "missing $v"; exit 2; }
  echo "   $a: $("${!v}" --version 2>/dev/null | head -1)"
done

thermal() { for h in /sys/class/hwmon/hwmon*/temp*_input; do [ -r "$h" ] && echo "hwmon $(basename "$(dirname "$h")")/$(basename "$h")=$(cat "$h")"; done > "$1" 2>/dev/null; }

# The fsync-storm job: small files, one fsync per 256 KiB write, 24 jobs.
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

row() {  # $1 = tag, $2 = jobfile, $3 = mode (kern|il), $4 = il shim, $5.. = extra fio args
  local tag="$1" jobfile="$2" mode="$3" il="$4"; shift 4
  local pre=()
  [ "$mode" = il ] && pre=(env LD_PRELOAD="$il" SQUEEZEFS_IPC_ALLOW_DEV=1)
  echo "-- row $tag: $(basename "$jobfile") $mode $* loadavg=$(cut -d' ' -f1-3 /proc/loadavg)"
  thermal "$OUT/$tag.thermal0"
  sync; sleep 1
  cat "$MNT/.stats" > "$OUT/$tag.stats0"
  head -1 /proc/stat > "$OUT/$tag.procstat0"
  "${pre[@]}" fio "$jobfile" "$@" --output-format=json --output="$OUT/$tag.fio.json" \
    --write_bw_log="$OUT/$tag" --log_avg_msec=1000 > "$OUT/$tag.fio.txt" 2>&1 \
    || { echo "fio failed: $(tail -3 "$OUT/$tag.fio.txt")"; return 1; }
  cat "$MNT/.stats" > "$OUT/$tag.stats1"
  head -1 /proc/stat > "$OUT/$tag.procstat1"
  thermal "$OUT/$tag.thermal1"
  python3 - "$OUT/$tag.procstat0" "$OUT/$tag.procstat1" <<'PY'
import sys
a=[int(x) for x in open(sys.argv[1]).read().split()[1:]]; b=[int(x) for x in open(sys.argv[2]).read().split()[1:]]
d=[y-x for x,y in zip(a,b)]; tot=sum(d); idle=d[3]+d[4]
print(f"   box cpu: busy {100*(tot-idle)/tot:.1f}% (user {100*d[0]/tot:.1f} sys {100*d[2]/tot:.1f} irq {100*d[5]/tot:.1f} softirq {100*d[6]/tot:.1f} iowait {100*d[4]/tot:.1f})")
PY
  echo "   thermal: $(grep -h hwmon "$OUT/$tag.thermal0" | sort -t= -k2 -n | tail -1) -> $(grep -h hwmon "$OUT/$tag.thermal1" | sort -t= -k2 -n | tail -1)"
  python3 "$DELTA" "$OUT" "$tag" 2>/dev/null | tee "$OUT/$tag.row" || true
  # the fsync ledger (W-5) beside the row: the phase means and the namespace ratio
  python3 - "$OUT/$tag.stats0" "$OUT/$tag.stats1" <<'PY' || true
import json,sys
a=json.load(open(sys.argv[1]))["metrics"]; b=json.load(open(sys.argv[2]))["metrics"]
def d(k): return (b.get(k) or 0)-(a.get(k) or 0)
calls=d("fsync_calls")
if calls:
    fam=b.get("fsync_phase_ns") or {}; fam0=a.get("fsync_phase_ns") or {}
    means={ph:((h["sum_ns"]-(fam0.get(ph) or {}).get("sum_ns",0))/max(h["count"]-(fam0.get(ph) or {}).get("count",0),1)/1e6) for ph,h in fam.items() if isinstance(h,dict) and "sum_ns" in h}
    print(f"   fsync: calls {calls:,} noop {d('fsync_noop_clean'):,} namespaces touched/flushed {d('fsync_data_namespaces_touched'):,}/{d('fsync_data_namespaces_flushed'):,} wt-skips {d('fsync_write_through_skips'):,} | phase means ms: "+" ".join(f"{k}={v:.2f}" for k,v in sorted(means.items())))
PY
  dmesg -T 2>/dev/null | grep -iE "fuse|WARN|lockdep|BUG" | tail -5 > "$OUT/$tag.dmesg" || true
  [ -s "$OUT/$tag.dmesg" ] && { echo "   dmesg tail:"; sed 's/^/     /' "$OUT/$tag.dmesg"; }
}

mount_arm() {  # $1 = arm letter
  local v="BIN_$1"; local bin="${!v}"
  pkill -x squeezefs 2>/dev/null; sleep 2; pkill -9 -x squeezefs 2>/dev/null
  umount -l "$MNT" 2>/dev/null
  echo YES | /scratch/tmp/cluster_reset_v4.sh > "$OUT/reset-$1.log" 2>&1 || { echo "RESET FAILED ($1)"; exit 1; }
  # The meta URI from THIS reset's printed mount line — namespace numbering
  # can move across resets (the 2026-09-09 n1→n2 lesson); the reset also
  # leaves the set mounted by its own SQZ, which the arm replaces.
  META="$(grep -oE 'sqmeta://[^ ]+' "$OUT/reset-$1.log" | tail -n 1)"; [ -n "$META" ] || { echo "RESET printed no sqmeta URI ($1)"; exit 1; }
  for d in $(echo "$META" | sed 's#sqmeta://##; s#,# #g'); do [ -b "$d" ] || { echo "META device $d is not a block device ($1)"; exit 1; }; done
  pkill -x squeezefs 2>/dev/null; sleep 2; umount -l "$MNT" 2>/dev/null; rm -rf "$MNT"/client_validation 2>/dev/null
  [ -z "$(ls -A "$MNT" 2>/dev/null)" ] || { echo "MOUNTPOINT NOT EMPTY ($1)"; exit 1; }
  "$bin" mount "$META" "$MNT" --daemon --interception --allow-other --log-file "$OUT/mount-$1-$(date +%s).log" 2>&1 | tail -1
  sleep 3
  mkdir -p "$MNT/client_validation"
  # prep: the data set the rand rows read/write (write_BW's 24 × 8 GiB files)
  fio "$JOBS/write_BW.job" --runtime=40 --ramp_time=0 --output-format=json --output="$OUT/prep-$1.fio.json" > /dev/null 2>&1
}

i=0
for arm in $SEQ; do
  i=$((i+1)); ilv="IL_$arm"; il="${!ilv:-}"
  echo "##### arm $arm (position $i) $(date -u +%FT%TZ)"
  mount_arm "$arm"
  # ROWS selects the rows (default all); a re-run of one contested row
  # names just it, e.g. ROWS="rw4k-kern rr4k-il".
  for r in ${ROWS:-rr4k-kern rw4k-kern wdur-kern fsync-storm rr4k-il}; do
    case "$r" in
      rr4k-kern)   row "${arm}${i}-rr4k-kern" "$JOBS/randread_iops.job"  kern "$il" --runtime="$RT" ;;
      rw4k-kern)   row "${arm}${i}-rw4k-kern" "$JOBS/randwrite_iops.job" kern "$il" --runtime="$RT" ;;
      wdur-kern)   row "${arm}${i}-wdur-kern" "$JOBS/write_BW.job"        kern "$il" --runtime="$RT" --end_fsync=1 ;;
      fsync-storm) row "${arm}${i}-fsync-storm" "$STORM"                  kern "$il" ;;
      rr4k-il)     [ -n "$il" ] && row "${arm}${i}-rr4k-il" "$JOBS/randread_iops.job" il "$il" --runtime="$RT" ;;
    esac
  done
  rm -rf "$MNT/client_validation/storm."* 2>/dev/null
done
pkill -x squeezefs 2>/dev/null; sleep 2; umount -l "$MNT" 2>/dev/null
echo "== done $(date -u +%FT%TZ) $OUT"
grep -h "^ROW" "$OUT"/*.row 2>/dev/null
