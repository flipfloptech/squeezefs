#!/usr/bin/env bash
# Inline-raise threshold sweep — LOCAL SCOPING (dev box; the venue rule says
# acceptance rows run on squeeze-test, this picks the derivation to take
# there). ONE binary; per leg: fresh format WITH a staging dir on the tcp
# dev substrate, mount with SQUEEZEFS_INLINE_MAX_BYTES=T, then the
# small-file row at file size S (fio psync, 24 jobs x 1,500 files each — fio's
# smalloc file table caps the product; time_based wraps the set — one fsync
# per file, time-bounded) and a create-only row (no fsync). The grid is S x T:
#   S in SIZES (file sizes, default 4k 16k 32k 64k)
#   T in CEILINGS (inline ceilings, default 4096 = the shipped posture and
#                  the KV value cap 65536 = the maximum raise)
# so every S is measured STAGED (T < S) and INLINE (T >= S). Per row the
# .stats deltas give: files/s, fsync total + phases, metadata-plane bytes
# per file (meta_kv_journal_bytes / node_append_bytes), node-cache
# misses/evictions, layout_{inline,staged}_writes engagement, and the
# blocks the row allocated (inline must be 0).
#
#   sudo OUT=target/inline-sweep bash .benchmarks/rigs/2026-09-09-inline-raise-sweep-local.sh
#   (optional) SIZES="16k 64k" CEILINGS="4096 16384 65536" RT=20
# Reduce: 2026-09-09-inline-raise-sweep-reduce.py <OUT>
set -euo pipefail
SQZ="${SQZ:-$PWD/target/release/squeezefs}"
OUT="${OUT:-$PWD/target/inline-sweep}"
META="${META:-sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1}"
DATA="${DATA:-sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1}"
MNT="${MNT:-/mnt/sqz-inline-sweep}"
STAGING="${STAGING:-/dev/shm/sqz-inline-staging}"
STAGING_SIZE="${STAGING_SIZE:-16GB}"
SIZES="${SIZES:-4k 16k 32k 64k}"
CEILINGS="${CEILINGS:-4096 65536}"
RT="${RT:-20}"
JOBS="${JOBS:-24}"
mkdir -p "$OUT" "$MNT"
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 2; }
FIO="${FIO:-$(command -v fio || true)}"; [ -x "$FIO" ] || { echo "fio missing" >&2; exit 2; }
if ps -eo args | grep -q "[s]queezefs mount $META"; then echo "a daemon is up on $META — refusing" >&2; exit 2; fi
log() { echo "[$(date +%T)] $*" | tee -a "$OUT/driver.log"; }
log "== inline-raise sweep (LOCAL SCOPING, $(hostname) $(uname -r)): $($SQZ --version | head -1); sizes [$SIZES] ceilings [$CEILINGS] rt $RT"

job() {  # $1 = path, $2 = size, $3 = fsync (1|0)
  cat > "$1" <<EOF
[smallf]
group_reporting=1
ioengine=psync
readwrite=write
direct=0
bs=$2
filesize=$2
nrfiles=1500
file_service_type=sequential
fsync_on_close=$3
fallocate=none
create_on_open=1
time_based
runtime=$RT
ramp_time=3
filename_format=f.\$jobnum.\$filenum
numjobs=$JOBS
directory=$MNT/sweep/
EOF
}

row() {  # $1 = tag, $2 = jobfile
  local tag="$1" jobfile="$2"
  sync; sleep 1
  cat "$MNT/.stats" > "$OUT/$tag.stats0"; head -1 /proc/stat > "$OUT/$tag.procstat0"
  "$FIO" "$jobfile" --output-format=json --output="$OUT/$tag.fio.json" > "$OUT/$tag.fio.txt" 2>&1 \
    || { log "fio failed ($tag): $(tail -2 "$OUT/$tag.fio.txt")"; return 1; }
  cat "$MNT/.stats" > "$OUT/$tag.stats1"; head -1 /proc/stat > "$OUT/$tag.procstat1"
  python3 - "$OUT/$tag" <<'PY' | tee -a "$OUT/driver.log"
import json,sys
p=sys.argv[1]
a=json.load(open(p+".stats0")); b=json.load(open(p+".stats1")); am=a["metrics"]; bm=b["metrics"]
j=json.load(open(p+".fio.json"))["jobs"][0]["write"]
def d(k): return (bm.get(k) or 0)-(am.get(k) or 0)
files=j["total_ios"]; secs=json.load(open(p+".fio.json"))["jobs"][0]["job_runtime"]/1000
def pm(fam,ph):
    f0=am.get(fam) or {}; f1=bm.get(fam) or {}
    c=(f1.get(ph) or {}).get("count",0)-(f0.get(ph) or {}).get("count",0); s=(f1.get(ph) or {}).get("sum_ns",0)-(f0.get(ph) or {}).get("sum_ns",0)
    return s/c/1e3 if c else 0.0
print(f"   files {files:,} ({files/secs:,.0f}/s) clat p50 {j['clat_ns']['percentile']['50.000000']/1e3:.0f} p99.9 {j['clat_ns']['percentile']['99.900000']/1e3:.0f} us | layout inline {d('layout_inline_writes'):,} staged {d('layout_staged_writes'):,} | meta bytes/file journal {d('meta_kv_journal_bytes')/max(files,1):,.0f} appends {d('meta_kv_node_append_bytes')/max(files,1):,.0f} | node cache misses {d('meta_kv_node_cache_misses'):,} evict {d('meta_kv_node_cache_evictions'):,} | fsync total {pm('fsync_phase_ns','total'):.0f} us (meta_barrier {pm('fsync_phase_ns','meta_barrier'):.0f} meta_publish {pm('fsync_phase_ns','meta_publish'):.0f} data_flush {pm('fsync_phase_ns','data_flush'):.0f}) | promoted inline {d('fsync_promoted_inline_files') + d('dismount_promoted_inline_files'):,} | staged resident {b.get('nvme_staged_write_file_count')} | daemon cpu us/file {d('daemon_cpu_ns')/max(files,1)/1e3:.1f}")
PY
}

for T in $CEILINGS; do
  for S in $SIZES; do
    leg="T${T}-S${S}"
    log "-- leg $leg: format (staging $STAGING), mount SQUEEZEFS_INLINE_MAX_BYTES=$T"
    rm -rf "$STAGING"; mkdir -p "$STAGING"
    "$SQZ" format "$META" "$DATA" --force --disk-cache-paths "$STAGING" > "$OUT/$leg.format.log" 2>&1
    env SQUEEZEFS_INLINE_MAX_BYTES="$T" "$SQZ" mount "$META" "$MNT" --daemon --allow-other \
      --disk-cache-size "$STAGING_SIZE" --log-file "$OUT/$leg.daemon.log" > "$OUT/$leg.mount.log" 2>&1
    sleep 2; [ -e "$MNT/.stats" ] || { log "MOUNT FAILED ($leg): $(tail -2 "$OUT/$leg.mount.log")"; exit 1; }
    python3 -c "import json;d=json.load(open('$MNT/.stats'));print('   inline_max_bytes on the mount:', d.get('inline_max_bytes', d['metrics'].get('inline_max_bytes')))" | tee -a "$OUT/driver.log"
    mkdir -p "$MNT/sweep"
    job "$OUT/$leg.fsync.job" "$S" 1
    log "   row $leg-fsync (S=$S, fsync per file)"
    row "$leg-fsync" "$OUT/$leg.fsync.job"
    rm -rf "$MNT/sweep"; mkdir -p "$MNT/sweep"
    job "$OUT/$leg.create.job" "$S" 0
    log "   row $leg-create (S=$S, no fsync)"
    row "$leg-create" "$OUT/$leg.create.job"
    "$SQZ" umount "$MNT" > "$OUT/$leg.umount.log" 2>&1 || umount -l "$MNT"
    sleep 1
  done
done
log "== done: $OUT"
