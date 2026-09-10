#!/usr/bin/env bash
# Small-file PACKING rows — the PK7 rig. LOCAL run = SCOPING (dev box; the
# venue rule says the throughput/latency bracket runs on squeeze-test — this
# rig is what runs there too, with META/DATA/MNT pointed at the box). ONE
# binary; two arms per row (A = SQUEEZEFS_SMALL_FILE_PACKING=0, the shipped
# one-block-per-file promotion; B = 1, packing); fresh format WITH a staging
# dir per arm on the tcp dev substrate.
#
# Rows (design-small-file-packing §7 / PK7):
#   dismount  — FILES small files (8/16/32/64 KiB, deterministic content, NO
#               fsync), syncfs, `squeezefs umount` (the dismount pass promotes
#               them): blocks minted (statvfs on the remount), dismount wall,
#               the daemon's dismount summary; then a SECOND mount point reads
#               every file byte-exact with the C8 oracle armed (drift must be
#               0) and online fsck must report 0 findings. A: FILES blocks.
#               B: ≈ Σ slots / 4 MiB blocks.
#   fsyncrow  — the rejected fsync-promotion lever's row re-run under packing
#               (SQUEEZEFS_FSYNC_PROMOTE_STAGED=1 on BOTH arms): fio create +
#               fsync-on-close, 16 KiB, bounded by FILES (the A arm's space law
#               would otherwise fill the volume): files/s, fsync clat p50 /
#               p99.9, fsync_phase_ns, blocks minted, and the standing
#               write-amplification columns — DATA-namespace device bytes ÷
#               user bytes and wareq-sz from /proc/diskstats deltas.
#   compact   — on the A arm's dismount population (the LEGACY
#               one-block-per-file volume) remount with the lever ON:
#               `defrag --report-only` (the D1 pack-occupancy gauges), then
#               `defrag --pack`: blocks freed, every survivor byte-exact,
#               oracle 0, fsck 0. This is how existing volumes recover the
#               64× law without a reformat. On the B arm: delete every other
#               tenant, then `defrag --pack` compacts the half-empty packs.
#
#   sudo OUT=target/packing-rows bash .benchmarks/rigs/2026-09-10-packing-rows-local.sh
#   (optional) FILES=2000 ARMS="A B" ROWS="dismount compact fsyncrow" JOBS=8
# Reduce: 2026-09-10-packing-rows-reduce.py <OUT>
set -euo pipefail
SQZ="${SQZ:-$PWD/target/release/squeezefs}"
OUT="${OUT:-$PWD/target/packing-rows}"
META="${META:-sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1}"
DATA="${DATA:-sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1}"
DATA_DEVS="${DATA_DEVS:-nvme5n1 nvme6n1 nvme7n1 nvme8n1}"   # /proc/diskstats names of the DATA namespaces
MNT="${MNT:-/mnt/sqz-packing-rows}"
MNT2="${MNT2:-/mnt/sqz-packing-rows-b}"
STAGING="${STAGING:-/dev/shm/sqz-packing-staging}"
STAGING_SIZE="${STAGING_SIZE:-16GB}"
FILES="${FILES:-2000}"
ARMS="${ARMS:-A B}"
ROWS="${ROWS:-dismount compact fsyncrow}"   # compact reads the dismount row's population — keep that order
RT="${RT:-20}"
JOBS="${JOBS:-8}"
mkdir -p "$OUT" "$MNT" "$MNT2"
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 2; }
FIO="${FIO:-$(command -v fio || true)}"; [ -x "$FIO" ] || { echo "fio missing" >&2; exit 2; }
if ps -eo args | grep -q "[s]queezefs mount $META"; then echo "a daemon is up on $META — refusing" >&2; exit 2; fi
export SQUEEZEFS_IPC_ALLOW_DEV=1
log() { echo "[$(date +%T)] $*" | tee -a "$OUT/driver.log"; }
log "== packing rows ($(hostname) $(uname -r)): $($SQZ --version | head -1); files $FILES arms [$ARMS] rows [$ROWS]"

lever() { case "$1" in A) echo 0;; B) echo 1;; esac; }

fmt() {  # fresh format with the staging dir
  rm -rf "$STAGING"; mkdir -p "$STAGING"
  "$SQZ" format "$META" "$DATA" --force --disk-cache-paths "$STAGING" > "$OUT/$1.format.log" 2>&1
}
mnt() {  # $1 tag, $2 mountpoint, $3 lever, extra env after
  local tag="$1" mp="$2" lv="$3"; shift 3
  env SQUEEZEFS_SMALL_FILE_PACKING="$lv" "$@" "$SQZ" mount "$META" "$mp" --daemon --allow-other \
    --disk-cache-size "$STAGING_SIZE" --log-file "$OUT/$tag.daemon.log" > "$OUT/$tag.mount.log" 2>&1
  for _ in $(seq 1 100); do [ -e "$mp/.stats" ] && break; sleep 0.1; done
  [ -e "$mp/.stats" ] || { log "MOUNT FAILED ($tag): $(tail -2 "$OUT/$tag.mount.log")"; exit 1; }
}
umnt() {  # $1 tag, $2 mountpoint → prints the wall seconds
  local t0 t1; t0=$(date +%s.%N)
  "$SQZ" umount "$2" > "$OUT/$1.umount.log" 2>&1 || umount -l "$2"
  t1=$(date +%s.%N); python3 -c "print(f'{$t1-$t0:.2f}')"
}
stat_u64() { python3 -c "import json,sys;d=json.load(open('$1/.stats'));m=d['metrics'];k='$2';print(m.get(k) if k in m else d.get(k))"; }
used_blocks() { python3 -c "import os;s=os.statvfs('$1');print((s.f_blocks-s.f_bavail)*s.f_frsize//(4<<20))"; }
diskstats() { awk -v devs="$DATA_DEVS" 'BEGIN{n=split(devs,a," ");for(i=1;i<=n;i++)w[a[i]]=1} ($3 in w){wr+=$8; sec+=$10} END{print wr, sec*512}' /proc/diskstats; }

populate() {  # $1 mountpoint dir — FILES deterministic files, no fsync
  python3 - "$1" "$FILES" <<'PY'
import os,sys
d=sys.argv[1]; n=int(sys.argv[2]); os.makedirs(d, exist_ok=True)
sizes=[8<<10,16<<10,32<<10,64<<10]
for i in range(n):
    ln=sizes[i%4]; b=bytes(((i*131+j*7)&0xff) for j in range(4096))
    with open(f"{d}/f{i:05d}.bin","wb") as f:
        f.write((b*(ln//4096))[:ln])
PY
}
verify() {  # $1 dir, $2 stride (1 = all files, 2 = the survivors of a half-delete) → prints mismatches
  python3 - "$1" "$FILES" "$2" <<'PY'
import sys
d=sys.argv[1]; n=int(sys.argv[2]); stride=int(sys.argv[3]); bad=0
sizes=[8<<10,16<<10,32<<10,64<<10]
for i in range(0,n,stride):
    ln=sizes[i%4]; b=bytes(((i*131+j*7)&0xff) for j in range(4096)); want=(b*(ln//4096))[:ln]
    try: got=open(f"{d}/f{i:05d}.bin","rb").read()
    except Exception as e: got=repr(e).encode()
    if got!=want: bad+=1
print(bad)
PY
}
fsck_findings() { "$SQZ" fsck "$1" --json 2>"$OUT/$2.fsck.err" | python3 -c "import json,sys;r=json.load(sys.stdin);print(r['counters']['findings'])"; }

row_dismount() {  # $1 arm
  local arm="$1" lv; lv=$(lever "$arm"); local tag="dismount-$arm"
  log "-- $tag: format, mount lever=$lv, populate $FILES files (no fsync), syncfs, umount"
  fmt "$tag"; mnt "$tag" "$MNT" "$lv"
  local t0 t1; t0=$(date +%s.%N); populate "$MNT/small"; sync -f "$MNT"; t1=$(date +%s.%N)
  local resident; resident=$(stat_u64 "$MNT" nvme_staged_write_file_count)
  local used0; used0=$(used_blocks "$MNT")
  local wall; wall=$(umnt "$tag" "$MNT")
  local summary; summary=$(grep -oE "dismount promoted [0-9]+ staged-layout file\(s\) \([0-9]+ B\) to the shared backend: [0-9]+ inline, [0-9]+ packed, [0-9]+ to blocks" "$OUT/$tag.daemon.log" | tail -1)
  # The remount: the oracle armed, a DIFFERENT mount point (a different
  # staging scope — every read is a data-plane read).
  mnt "$tag-verify" "$MNT2" "$lv" SQUEEZEFS_BLOCK_REFS_VERIFY=1
  local used1 drift bad findings
  used1=$(used_blocks "$MNT2"); drift=$(stat_u64 "$MNT2" meta_kv_block_refs_drift)
  bad=$(verify "$MNT2/small" 1); findings=$(fsck_findings "$MNT2" "$tag-verify")
  log "   $tag: populate $(python3 -c "print(f'{$t1-$t0:.1f}')") s, resident before umount $resident, blocks before $used0; UMOUNT WALL ${wall}s; $summary"
  log "   $tag: on the second mount point — blocks $used1, oracle drift $drift, byte-mismatches $bad / $FILES, fsck findings $findings"
  echo "{\"row\":\"dismount\",\"arm\":\"$arm\",\"files\":$FILES,\"umount_wall_s\":$wall,\"blocks_before\":$used0,\"blocks_after\":$used1,\"drift\":$drift,\"mismatches\":$bad,\"fsck_findings\":$findings,\"summary\":\"$summary\"}" >> "$OUT/rows.jsonl"
  umnt "$tag-verify" "$MNT2" >/dev/null
}

row_fsync() {  # $1 arm
  local arm="$1" lv; lv=$(lever "$arm"); local tag="fsyncrow-$arm"
  log "-- $tag: format, mount lever=$lv + SQUEEZEFS_FSYNC_PROMOTE_STAGED=1, fio create+fsync 16 KiB × $FILES"
  fmt "$tag"; mnt "$tag" "$MNT" "$lv" SQUEEZEFS_FSYNC_PROMOTE_STAGED=1
  mkdir -p "$MNT/fs"
  local per; per=$((FILES / JOBS))
  cat > "$OUT/$tag.job" <<EOF
[smallf]
group_reporting=1
ioengine=psync
readwrite=write
direct=0
bs=16k
filesize=16k
nrfiles=$per
file_service_type=sequential
fsync_on_close=1
fallocate=none
create_on_open=1
filename_format=f.\$jobnum.\$filenum
numjobs=$JOBS
directory=$MNT/fs/
EOF
  sync; sleep 1
  cat "$MNT/.stats" > "$OUT/$tag.stats0"; read -r wr0 by0 <<<"$(diskstats)"
  "$FIO" "$OUT/$tag.job" --output-format=json --output="$OUT/$tag.fio.json" > "$OUT/$tag.fio.txt" 2>&1 \
    || { log "fio failed ($tag): $(tail -2 "$OUT/$tag.fio.txt")"; return 1; }
  sync -f "$MNT"; sleep 1
  cat "$MNT/.stats" > "$OUT/$tag.stats1"; read -r wr1 by1 <<<"$(diskstats)"
  local used drift; used=$(used_blocks "$MNT")
  python3 - "$OUT/$tag" "$arm" "$FILES" "$used" "$((wr1-wr0))" "$((by1-by0))" <<'PY' | tee -a "$OUT/driver.log"
import json,sys
p,arm,files,used,writes,dbytes=sys.argv[1],sys.argv[2],int(sys.argv[3]),int(sys.argv[4]),int(sys.argv[5]),int(sys.argv[6])
a=json.load(open(p+".stats0"))["metrics"]; b=json.load(open(p+".stats1"))["metrics"]
J=json.load(open(p+".fio.json"))["jobs"][0]; j=J["write"]
# group_reporting: job_runtime is the SUM over jobs; the aggregate rate is bw_bytes/filesize.
files_per_s=j["bw_bytes"]/16384
def d(k): return (b.get(k) or 0)-(a.get(k) or 0)
def pm(fam,ph):
    f0=a.get(fam) or {}; f1=b.get(fam) or {}
    c=(f1.get(ph) or {}).get("count",0)-(f0.get(ph) or {}).get("count",0); s=(f1.get(ph) or {}).get("sum_ns",0)-(f0.get(ph) or {}).get("sum_ns",0)
    return s/c/1e3 if c else 0.0
n=j["total_ios"]; user=n*16384
row=dict(row="fsyncrow",arm=arm,files=n,files_per_s=files_per_s,clat_p50_us=j["clat_ns"]["percentile"]["50.000000"]/1e3,
  clat_p999_us=j["clat_ns"]["percentile"]["99.900000"]/1e3,fsync_total_us=pm("fsync_phase_ns","total"),
  fsync_staged_promote_us=pm("fsync_phase_ns","staged_promote"),promoted=d("fsync_promoted_files"),
  promoted_packed=d("layout_promoted_packed"),blocks=used,dev_write_bytes=dbytes,user_bytes=user,
  amp=dbytes/max(user,1),wareq_kib=(dbytes/max(writes,1))/1024,pack_blocks_opened=d("pack_blocks_opened"),
  daemon_cpu_us_per_file=d("daemon_cpu_ns")/max(n,1)/1e3,tripwires=d("invariant_tripwires"))
print(f"   fsyncrow-{arm}: files {n:,} ({row['files_per_s']:,.0f}/s) clat p50 {row['clat_p50_us']:.0f} p99.9 {row['clat_p999_us']:.0f} us | fsync total {row['fsync_total_us']:.0f} us (staged_promote {row['fsync_staged_promote_us']:.0f}) | promoted {row['promoted']:,} packed {row['promoted_packed']:,} | BLOCKS {used:,} | dev bytes/user bytes {row['amp']:.2f}x wareq-sz {row['wareq_kib']:.0f} KiB | cpu us/file {row['daemon_cpu_us_per_file']:.0f} | tripwires {row['tripwires']}")
open(p.rsplit('/',1)[0]+"/rows.jsonl","a").write(json.dumps(row)+"\n")
PY
  # Oracle + fsck on the same mount (promotion is durable here — fsync'd).
  drift=$(stat_u64 "$MNT" meta_kv_block_refs_drift); local findings; findings=$(fsck_findings "$MNT" "$tag")
  log "   $tag: fsck findings $findings (oracle runs at the next mount)"
  umnt "$tag" "$MNT" >/dev/null
  mnt "$tag-verify" "$MNT2" "$lv" SQUEEZEFS_BLOCK_REFS_VERIFY=1
  drift=$(stat_u64 "$MNT2" meta_kv_block_refs_drift); log "   $tag: remount oracle drift $drift, blocks $(used_blocks "$MNT2")"
  echo "{\"row\":\"fsyncrow-verify\",\"arm\":\"$arm\",\"drift\":$drift}" >> "$OUT/rows.jsonl"
  umnt "$tag-verify" "$MNT2" >/dev/null
}

row_compact() {  # $1 arm — the population the dismount row left on the volumes
  local arm="$1" tag="compact-$arm"
  log "-- $tag: remount the dismount-$arm population with the lever ON; defrag --report-only, then --pack"
  mnt "$tag" "$MNT" 1 SQUEEZEFS_BLOCK_REFS_VERIFY=1
  local used0 drift0; used0=$(used_blocks "$MNT"); drift0=$(stat_u64 "$MNT" meta_kv_block_refs_drift)
  if [ "$arm" = B ]; then
    # Half the tenants leave: every other file — the packs drop below half.
    python3 -c "import os,sys; d='$MNT/small'; [os.unlink(f'{d}/f{i:05d}.bin') for i in range(1,$FILES,2)]"
    sync -f "$MNT"
  fi
  "$SQZ" defrag "$MNT" --report-only --json > "$OUT/$tag.report.json" 2>"$OUT/$tag.report.err" || true
  local occ; occ=$(python3 -c "import json;r=json.load(open('$OUT/$tag.report.json'));p=r.get('pack') or {};print(json.dumps(p))" 2>/dev/null || echo '{}')
  log "   $tag: blocks before $used0, oracle drift $drift0, report pack=$occ"
  local t0 t1; t0=$(date +%s.%N)
  "$SQZ" defrag "$MNT" --pack > "$OUT/$tag.pack.log" 2>&1 || log "   $tag: defrag --pack exit $? — $(tail -2 "$OUT/$tag.pack.log")"
  t1=$(date +%s.%N); sync -f "$MNT"; sleep 2
  local used1 freed moved bad findings stride
  used1=$(used_blocks "$MNT"); freed=$(stat_u64 "$MNT" pack_compaction_blocks_freed); moved=$(stat_u64 "$MNT" pack_compaction_tenants_moved)
  stride=1; [ "$arm" = B ] && stride=2
  bad=$(verify "$MNT/small" $stride); findings=$(fsck_findings "$MNT" "$tag")
  log "   $tag: defrag --pack wall $(python3 -c "print(f'{$t1-$t0:.1f}')") s — blocks $used0 → $used1, compaction freed $freed, tenants moved $moved, byte-mismatches $bad, fsck findings $findings"
  umnt "$tag" "$MNT" >/dev/null
  mnt "$tag-verify" "$MNT2" 1 SQUEEZEFS_BLOCK_REFS_VERIFY=1
  local drift1; drift1=$(stat_u64 "$MNT2" meta_kv_block_refs_drift); bad=$(verify "$MNT2/small" $stride)
  log "   $tag: remount oracle drift $drift1, blocks $(used_blocks "$MNT2"), byte-mismatches $bad"
  echo "{\"row\":\"compact\",\"arm\":\"$arm\",\"blocks_before\":$used0,\"blocks_after\":$used1,\"freed\":$freed,\"moved\":$moved,\"mismatches\":$bad,\"fsck_findings\":$findings,\"drift_after\":$drift1,\"report_pack\":$occ}" >> "$OUT/rows.jsonl"
  umnt "$tag-verify" "$MNT2" >/dev/null
}

for arm in $ARMS; do
  for r in $ROWS; do
    case "$r" in
      dismount) row_dismount "$arm" ;;
      compact)  row_compact "$arm" ;;   # needs the dismount row's population — run dismount first in ROWS
      fsyncrow) row_fsync "$arm" ;;
    esac
  done
done
log "== done: $OUT (rows.jsonl)"
