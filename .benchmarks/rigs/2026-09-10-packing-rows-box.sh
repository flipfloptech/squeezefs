#!/usr/bin/env bash
# Small-file packing — the PK7 ACCEPTANCE bracket on squeeze-test. ONE
# binary, the lever SQUEEZEFS_SMALL_FILE_PACKING as the arm (A = 0, the
# shipped one-block-per-file promotion; B = 1, packing), A-B-B-A. Every
# position: fabric reset WITH a RAM-backed staging dir (the fleet is
# cache-less; the local disk is SATA — the same posture as
# 2026-09-09-fsync-promote-abba.sh), then:
#   fsyncrow   the PRICING row — fio psync create + fsync-on-close, 16 KiB,
#              JOBS × PER files (count-bounded: the A arm's one-block-per-
#              file law would fill the set on a time-bounded row), with
#              SQUEEZEFS_FSYNC_PROMOTE_STAGED=1 on both arms; files/s, clat
#              p50/p99.9, fsync_phase_ns, blocks minted (statvfs), the
#              write-amplification columns from the DATA namespaces'
#              /proc/diskstats deltas
#   dismount   FILES small files (8/16/32/64 KiB, deterministic, no fsync),
#              syncfs, `squeezefs umount` → blocks minted, dismount wall, the
#              daemon's summary; remount with the oracle → byte-exact, drift
#              0, fsck 0 (deterministic — one position per arm is enough;
#              runs on every position for the count)
#   compact    (A positions only) the legacy population from the dismount
#              row, remounted with the lever ON: report-only aggregates,
#              `defrag --pack`, blocks after, byte-exact, oracle, fsck
# Reduce: 2026-09-10-packing-rows-reduce.py <OUT> (it merges positions).
#
#   sudo env BIN=/scratch/tmp/squeezefs-pk7 [SEQ="A B B A"] [FILES=20000] \
#            [JOBS=24] [PER=4000] [STAGING=/dev/shm/sqz-staging] [STAGING_SIZE=64GB] \
#        bash /scratch/tmp/rigs/2026-09-10-packing-rows-box.sh
#
# EVERYTHING THIS RIG CREATES ON THE BOX (the manifest is also printed at
# the end, for manual cleanup):
#   $OUT/                       (default /scratch/tmp/campaign/packing-rows-<ts>/)
#   $STAGING/                   (default /dev/shm/sqz-staging — removed at exit)
#   the fabric's volumes are re-formatted by the reset (the standing test set)
#   /scratch/tmp/test           the standing mount point (left EMPTY + unmounted)
#   /scratch/tmp/test2          a second mount point for the verify mounts (removed at exit)
#   /scratch/tmp/logs/          the reset script's own mount log home (its standing convention)
set -u
BIN=${BIN:?BIN (the one binary) is required}
TS=$(date -u +%Y%m%d-%H%M%S)
OUT=${OUT:-/scratch/tmp/campaign/packing-rows-$TS}
MNT=/scratch/tmp/test
MNT2=/scratch/tmp/test2
META=""   # per position, from the reset's printed mount line
SEQ=${SEQ:-"A B B A"}
FILES=${FILES:-20000}
JOBS=${JOBS:-24}
PER=${PER:-4000}
STAGING=${STAGING:-/dev/shm/sqz-staging}
STAGING_SIZE=${STAGING_SIZE:-64GB}
RESET=/scratch/tmp/cluster_reset_v4.sh
mkdir -p "$OUT" "$MNT2"
export SQUEEZEFS_IPC_ALLOW_DEV=1
log() { echo "[$(date +%T)] $*" | tee -a "$OUT/driver.log"; }
log "== packing rows ($(hostname) $(uname -r)): $("$BIN" --version | head -1); files $FILES fsyncrow ${JOBS}x${PER} seq [$SEQ] staging $STAGING ($STAGING_SIZE) out $OUT"

thermal() { for h in /sys/class/hwmon/hwmon*/temp*_input; do [ -r "$h" ] && echo "hwmon $(basename "$(dirname "$h")")/$(basename "$h")=$(cat "$h")"; done > "$1" 2>/dev/null; }
stat_u64() { python3 -c "import json;d=json.load(open('$1/.stats'));m=d['metrics'];k='$2';print(m.get(k) if k in m else d.get(k))"; }
used_blocks() { python3 -c "import os;s=os.statvfs('$1');print((s.f_blocks-s.f_bavail)*s.f_frsize//(4<<20))"; }
# The DATA namespaces = every nvme device the mount's data URIs name (read
# off the reset log's sqdata line); their /proc/diskstats deltas are the
# device-byte side of the amplification column.
DATA_DEVS=""
diskstats() { awk -v devs="$DATA_DEVS" 'BEGIN{n=split(devs,a," ");for(i=1;i<=n;i++)w[a[i]]=1} ($3 in w){wr+=$8; sec+=$10} END{print wr+0, (sec+0)*512}' /proc/diskstats; }
fsck_findings() { "$BIN" fsck "$1" --json 2>"$OUT/$2.fsck.err" | python3 -c "import json,sys;r=json.load(sys.stdin);print(r['counters']['findings'])" 2>/dev/null || echo "?"; }

populate() {  # $1 dir
  python3 - "$1" "$FILES" <<'PY'
import os,sys
d=sys.argv[1]; n=int(sys.argv[2]); os.makedirs(d, exist_ok=True)
sizes=[8<<10,16<<10,32<<10,64<<10]
for i in range(n):
    ln=sizes[i%4]; b=bytes(((i*131+j*7)&0xff) for j in range(4096))
    with open(f"{d}/f{i:05d}.bin","wb") as f: f.write((b*(ln//4096))[:ln])
PY
}
verify() {  # $1 dir → mismatches
  python3 - "$1" "$FILES" <<'PY'
import sys
d=sys.argv[1]; n=int(sys.argv[2]); bad=0
sizes=[8<<10,16<<10,32<<10,64<<10]
for i in range(n):
    ln=sizes[i%4]; b=bytes(((i*131+j*7)&0xff) for j in range(4096)); want=(b*(ln//4096))[:ln]
    try: got=open(f"{d}/f{i:05d}.bin","rb").read()
    except Exception as e: got=repr(e).encode()
    if got!=want: bad+=1
print(bad)
PY
}

reset_fabric() {  # $1 tag — fresh format WITH the staging dir; sets META + DATA_DEVS
  pkill -x "$(basename "$BIN")" 2>/dev/null; pkill -x squeezefs 2>/dev/null; sleep 2
  umount -l "$MNT" 2>/dev/null; umount -l "$MNT2" 2>/dev/null
  rm -rf "$STAGING"; mkdir -p "$STAGING"
  # The reset's CONFIG, re-pointed: the staging dir (CACHE_DIR →
  # --disk-cache-paths) and THIS binary (the script's SQZ names
  # /scratch/tmp/squeezefs, which the 2026-09-09 cleanup removed).
  sed -e "s#^CACHE_DIR=\"\"#CACHE_DIR=\"$STAGING\"#" -e "s#^SQZ=.*#SQZ=\"$BIN\"#" "$RESET" > "$OUT/reset-staging.sh"
  grep -q "^CACHE_DIR=\"$STAGING\"" "$OUT/reset-staging.sh" || { log "reset variant did not take CACHE_DIR"; exit 1; }
  grep -q "^SQZ=\"$BIN\"" "$OUT/reset-staging.sh" || { log "reset variant did not take SQZ"; exit 1; }
  local rlog="$OUT/reset-$1.log"
  echo YES | bash "$OUT/reset-staging.sh" > "$rlog" 2>&1 || { log "RESET FAILED ($1)"; exit 1; }
  grep -q "format: staging at" "$rlog" || { log "RESET did not format with a staging dir ($1)"; exit 1; }
  META="$(grep -oE 'sqmeta://[^ ]+' "$rlog" | tail -n 1)"
  [ -n "$META" ] || { log "RESET printed no sqmeta URI ($1)"; exit 1; }
  for d in $(echo "$META" | sed 's#sqmeta://##; s#,# #g'); do
    [ -b "$d" ] || { log "META device $d is not a block device — aborting ($1)"; exit 1; }
  done
  log "   meta: $META"
  mountpoint -q "$MNT" && umount -l "$MNT"; rm -rf "$MNT"/client_validation "$MNT"/.probe* 2>/dev/null
  [ -z "$(ls -A "$MNT" 2>/dev/null)" ] || { log "MOUNTPOINT NOT EMPTY: $(ls -A "$MNT" | head -n 3 | tr '\n' ' ')"; exit 1; }
}
mnt() {  # $1 tag, $2 mountpoint, $3 lever, extra env after
  local tag="$1" mp="$2" lv="$3"; shift 3
  env SQUEEZEFS_SMALL_FILE_PACKING="$lv" "$@" "$BIN" mount "$META" "$mp" --daemon --allow-other \
    --disk-cache-size "$STAGING_SIZE" --log-file "$OUT/$tag.daemon.log" > "$OUT/$tag.mount.out" 2>&1
  for _ in $(seq 1 200); do [ -e "$mp/.stats" ] && break; sleep 0.1; done
  [ -e "$mp/.stats" ] || { log "MOUNT FAILED ($tag): $(tail -2 "$OUT/$tag.mount.out")"; exit 1; }
  # The DATA namespaces, off the mount's own stats (`data_volume_write_cache`
  # names every distinct data device) — the diskstats side of the
  # amplification column.
  DATA_DEVS="$(python3 -c "import json,re;d=json.load(open('$mp/.stats'));v=d['metrics'].get('data_volume_write_cache', d.get('data_volume_write_cache'));print(' '.join(sorted(set(re.findall(r'nvme[0-9]+n[0-9]+', json.dumps(v))))))")"
  [ -n "$DATA_DEVS" ] || log "   WARN: no data devices parsed from .stats (amplification column will read 0)"
}
umnt() { local t0 t1; t0=$(date +%s.%N); "$BIN" umount "$2" > "$OUT/$1.umount.log" 2>&1 || umount -l "$2"; t1=$(date +%s.%N); python3 -c "print(f'{$t1-$t0:.2f}')"; }

row_fsync() {  # $1 arm, $2 position
  local arm="$1" pos="$2" lv=0; [ "$arm" = B ] && lv=1; local tag="fsyncrow-$arm$pos"
  log "-- $tag: mount lever=$lv + SQUEEZEFS_FSYNC_PROMOTE_STAGED=1, fio create+fsync 16 KiB × $((JOBS*PER))"
  mnt "$tag" "$MNT" "$lv" SQUEEZEFS_FSYNC_PROMOTE_STAGED=1
  mkdir -p "$MNT/fs"
  cat > "$OUT/$tag.job" <<EOF
[smallf]
group_reporting=1
ioengine=psync
readwrite=write
direct=0
bs=16k
filesize=16k
nrfiles=$PER
file_service_type=sequential
fsync_on_close=1
fallocate=none
create_on_open=1
filename_format=f.\$jobnum.\$filenum
numjobs=$JOBS
directory=$MNT/fs/
EOF
  thermal "$OUT/$tag.thermal0"; sync; sleep 1
  cat "$MNT/.stats" > "$OUT/$tag.stats0"; head -1 /proc/stat > "$OUT/$tag.procstat0"; read -r wr0 by0 <<<"$(diskstats)"
  fio "$OUT/$tag.job" --output-format=json --output="$OUT/$tag.fio.json" > "$OUT/$tag.fio.txt" 2>&1 \
    || { log "fio failed ($tag): $(tail -2 "$OUT/$tag.fio.txt")"; return 1; }
  sync -f "$MNT"; sleep 1
  cat "$MNT/.stats" > "$OUT/$tag.stats1"; head -1 /proc/stat > "$OUT/$tag.procstat1"; read -r wr1 by1 <<<"$(diskstats)"; thermal "$OUT/$tag.thermal1"
  local used; used=$(used_blocks "$MNT")
  python3 - "$OUT/$tag" "$arm" "$used" "$((wr1-wr0))" "$((by1-by0))" <<'PY' | tee -a "$OUT/driver.log"
import json,sys
p,arm,used,writes,dbytes=sys.argv[1],sys.argv[2],int(sys.argv[3]),int(sys.argv[4]),int(sys.argv[5])
a=json.load(open(p+".stats0"))["metrics"]; b=json.load(open(p+".stats1"))["metrics"]
J=json.load(open(p+".fio.json"))["jobs"][0]; j=J["write"]; secs=J["job_runtime"]/1000
def d(k): return (b.get(k) or 0)-(a.get(k) or 0)
def pm(fam,ph):
    f0=a.get(fam) or {}; f1=b.get(fam) or {}
    c=(f1.get(ph) or {}).get("count",0)-(f0.get(ph) or {}).get("count",0); s=(f1.get(ph) or {}).get("sum_ns",0)-(f0.get(ph) or {}).get("sum_ns",0)
    return s/c/1e3 if c else 0.0
n=j["total_ios"]; user=n*16384
row=dict(row="fsyncrow",arm=arm,files=n,files_per_s=n/secs,clat_p50_us=j["clat_ns"]["percentile"]["50.000000"]/1e3,
  clat_p999_us=j["clat_ns"]["percentile"]["99.900000"]/1e3,fsync_total_us=pm("fsync_phase_ns","total"),
  fsync_staged_promote_us=pm("fsync_phase_ns","staged_promote"),promoted=d("fsync_promoted_files"),
  promoted_packed=d("layout_promoted_packed"),blocks=used,dev_write_bytes=dbytes,user_bytes=user,
  amp=dbytes/max(user,1),wareq_kib=(dbytes/max(writes,1))/1024,pack_blocks_opened=d("pack_blocks_opened"),
  daemon_cpu_us_per_file=d("daemon_cpu_ns")/max(n,1)/1e3,tripwires=d("invariant_tripwires"),
  overdue=d("transport_slots_overdue"),rescues=d("transport_park_tick_commit_rescues"))
print(f"   {p.rsplit('/',1)[1]}: files {n:,} ({row['files_per_s']:,.0f}/s) clat p50 {row['clat_p50_us']:.0f} p99.9 {row['clat_p999_us']:.0f} us | fsync total {row['fsync_total_us']:.0f} us (staged_promote {row['fsync_staged_promote_us']:.0f}) | promoted {row['promoted']:,} packed {row['promoted_packed']:,} | BLOCKS {used:,} | dev/user bytes {row['amp']:.2f}x wareq-sz {row['wareq_kib']:.0f} KiB | cpu us/file {row['daemon_cpu_us_per_file']:.0f} | tripwires {row['tripwires']} overdue {row['overdue']} rescues {row['rescues']}")
open(p.rsplit('/',1)[0]+"/rows.jsonl","a").write(json.dumps(row)+"\n")
PY
  local findings; findings=$(fsck_findings "$MNT" "$tag"); log "   $tag: fsck findings $findings"
  umnt "$tag" "$MNT" >/dev/null
  mnt "$tag-verify" "$MNT2" "$lv" SQUEEZEFS_BLOCK_REFS_VERIFY=1
  local drift; drift=$(stat_u64 "$MNT2" meta_kv_block_refs_drift); log "   $tag: remount oracle drift $drift, blocks $(used_blocks "$MNT2")"
  echo "{\"row\":\"fsyncrow-verify\",\"arm\":\"$arm\",\"drift\":$drift}" >> "$OUT/rows.jsonl"
  umnt "$tag-verify" "$MNT2" >/dev/null
}

row_dismount() {  # $1 arm, $2 position — needs a FRESH format (reset) before it
  local arm="$1" pos="$2" lv=0; [ "$arm" = B ] && lv=1; local tag="dismount-$arm$pos"
  log "-- $tag: mount lever=$lv, populate $FILES files (no fsync), syncfs, umount"
  mnt "$tag" "$MNT" "$lv"
  local t0 t1; t0=$(date +%s.%N); populate "$MNT/small"; sync -f "$MNT"; t1=$(date +%s.%N)
  local resident; resident=$(stat_u64 "$MNT" nvme_staged_write_file_count)
  local wall; wall=$(umnt "$tag" "$MNT")
  local summary; summary=$(grep -oE "dismount promoted [0-9]+ staged-layout file\(s\) \([0-9]+ B\) to the shared backend: [0-9]+ inline, [0-9]+ packed, [0-9]+ to blocks" "$OUT/$tag.daemon.log" | tail -1)
  mnt "$tag-verify" "$MNT2" "$lv" SQUEEZEFS_BLOCK_REFS_VERIFY=1
  local used1 drift bad findings
  used1=$(used_blocks "$MNT2"); drift=$(stat_u64 "$MNT2" meta_kv_block_refs_drift)
  bad=$(verify "$MNT2/small"); findings=$(fsck_findings "$MNT2" "$tag-verify")
  log "   $tag: populate $(python3 -c "print(f'{$t1-$t0:.1f}')") s, resident $resident; UMOUNT WALL ${wall}s; $summary"
  log "   $tag: second mount point — blocks $used1, oracle drift $drift, byte-mismatches $bad / $FILES, fsck findings $findings"
  echo "{\"row\":\"dismount\",\"arm\":\"$arm\",\"files\":$FILES,\"umount_wall_s\":$wall,\"blocks_before\":0,\"blocks_after\":$used1,\"drift\":$drift,\"mismatches\":$bad,\"fsck_findings\":$findings,\"summary\":\"$summary\"}" >> "$OUT/rows.jsonl"
  umnt "$tag-verify" "$MNT2" >/dev/null
}

row_compact_legacy() {  # $1 position — the A dismount population, lever ON
  local tag="compact-A$1"
  log "-- $tag: remount the legacy population with the lever ON; defrag --report-only, then --pack"
  mnt "$tag" "$MNT" 1 SQUEEZEFS_BLOCK_REFS_VERIFY=1
  local used0; used0=$(used_blocks "$MNT")
  "$BIN" defrag "$MNT" --report-only --json > "$OUT/$tag.report.json" 2>"$OUT/$tag.report.err" || true
  local occ; occ=$(python3 -c "import json;r=json.load(open('$OUT/$tag.report.json'));p=dict(r.get('pack') or {});p.pop('rows',None);print(json.dumps(p))" 2>/dev/null || echo '{}')
  log "   $tag: blocks before $used0, report pack=$occ"
  local t0 t1; t0=$(date +%s.%N)
  "$BIN" defrag "$MNT" --pack > "$OUT/$tag.pack.log" 2>&1 || log "   $tag: defrag --pack exit $? — $(tail -2 "$OUT/$tag.pack.log")"
  t1=$(date +%s.%N); sync -f "$MNT"; sleep 2
  local used1 freed moved bad findings
  used1=$(used_blocks "$MNT"); freed=$(stat_u64 "$MNT" pack_compaction_blocks_freed); moved=$(stat_u64 "$MNT" pack_compaction_tenants_moved)
  bad=$(verify "$MNT/small"); findings=$(fsck_findings "$MNT" "$tag")
  log "   $tag: defrag --pack wall $(python3 -c "print(f'{$t1-$t0:.1f}')") s — blocks $used0 → $used1, freed $freed, tenants moved $moved, byte-mismatches $bad, fsck findings $findings"
  umnt "$tag" "$MNT" >/dev/null
  mnt "$tag-verify" "$MNT2" 1 SQUEEZEFS_BLOCK_REFS_VERIFY=1
  local drift1; drift1=$(stat_u64 "$MNT2" meta_kv_block_refs_drift); bad=$(verify "$MNT2/small")
  log "   $tag: remount oracle drift $drift1, blocks $(used_blocks "$MNT2"), byte-mismatches $bad"
  echo "{\"row\":\"compact\",\"arm\":\"A\",\"blocks_before\":$used0,\"blocks_after\":$used1,\"freed\":$freed,\"moved\":$moved,\"mismatches\":$bad,\"fsck_findings\":$findings,\"drift_after\":$drift1,\"report_pack\":$occ}" >> "$OUT/rows.jsonl"
  umnt "$tag-verify" "$MNT2" >/dev/null
}

i=0
for arm in $SEQ; do
  i=$((i+1))
  log "##### arm $arm (position $i) $(date -u +%FT%TZ) loadavg=$(cut -d' ' -f1-3 /proc/loadavg)"
  reset_fabric "$arm$i-fsync"
  row_fsync "$arm" "$i"
  reset_fabric "$arm$i-dismount"
  row_dismount "$arm" "$i"
  [ "$arm" = A ] && row_compact_legacy "$i"
done
pkill -x "$(basename "$BIN")" 2>/dev/null; sleep 2
umount -l "$MNT" 2>/dev/null; umount -l "$MNT2" 2>/dev/null
rm -rf "$STAGING"; rmdir "$MNT2" 2>/dev/null
log "== done $(date -u +%FT%TZ): $OUT (rows.jsonl)"
log "== CREATED ON THIS BOX: $OUT/ (results) | $BIN (the binary) | $(dirname "$0")/$(basename "$0") (this rig) | staging $STAGING (removed) | $MNT2 (removed) | the fabric's test set was re-formatted by the reset"
