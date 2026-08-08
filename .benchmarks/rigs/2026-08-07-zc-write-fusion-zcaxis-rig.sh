#!/usr/bin/env bash
# zc-write-fusion — the CAVEAT AXIS bracket (2026-08-07): armed (zc=1,
# fusion default ON) vs UNARMED (zc=0), A-B-B-A. This is the axis the
# D16 caveat text and its sunset rule are written on ("un-shimmed
# kernel-lane rand-4k writes pay ~20% ... armed rand-4k write ≥ 0.97×
# unarmed"); the sibling rig (2026-08-07-zc-write-fusion-rig.sh) is the
# fusion-lever attribution bracket (both sides armed).
# FIO ENGINE POLICY: libaio + direct=1 + stated iodepth, same both sides.
# Venue: LOCAL tcp devsub, 7.1.6-1-cachyos-sqz, root; sizes as sibling.
#   Z1(zc=1) Z2(zc=0) Z3(zc=0) Z4(zc=1)
set -euo pipefail

BIN=${BIN:-/home/justin/Source/.sqz-fusion-target/release/squeezefs}
MNT=${MNT:-/run/sqz-fusion/mnt}
META="sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1"
DATA="sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1"
OUT=${1:-/run/sqz-fusion/zcaxis-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$OUT" "$MNT" /run/sqz-fusion/logs

fatal() { echo "FATAL: $*" >&2; exit 1; }
jget() { python3 -c "import json,sys;d=json.load(open(sys.argv[1]));d=d.get('metrics',d);print(d.get(sys.argv[2],0))" "$1" "$2"; }

require_mount() {
  mountpoint -q "$MNT" || fatal "require-mount: $MNT not mounted ($1)"
  cat "$MNT/.stats" >/dev/null 2>&1 || fatal "require-mount: .stats unreadable ($1)"
}

do_umount() {
  if mountpoint -q "$MNT"; then
    "$BIN" umount "$MNT" || umount "$MNT" || true
  fi
  for _ in $(seq 1 150); do
    pidof squeezefs >/dev/null 2>&1 || break
    sleep 2
  done
  pidof squeezefs >/dev/null 2>&1 && fatal "daemon did not exit after umount drain"
  mountpoint -q "$MNT" && fatal "mountpoint still mounted"
  return 0
}

do_mount() { # $1 = zc value, $2 = leg tag
  SQUEEZEFS_FUSE_ZC=$1 "$BIN" mount "$META" "$MNT" --daemon \
    --uid 0 --gid 0 \
    --log-file "/run/sqz-fusion/logs/sqz-$2-zc$1.log"
  local ok=0
  for _ in $(seq 1 30); do
    if mountpoint -q "$MNT" && cat "$MNT/.stats" >/dev/null 2>&1; then ok=1; break; fi
    sleep 2
  done
  [ "$ok" = 1 ] || fatal "mount gate failed (leg $2 zc=$1)"
  cat "$MNT/.stats" > "$OUT/$2-arm.json"
  local neg
  neg=$(jget "$OUT/$2-arm.json" fuse3_zc_negotiated)
  [ "$neg" = "$1" ] || fatal "leg $2: fuse3_zc_negotiated=$neg, expected $1 — silent degrade/arm is INVALID"
  echo "leg $2: mounted (zc_negotiated=$neg)"
}

smoke() { # $1 = leg tag
  require_mount "smoke $1"
  local d="$MNT/fus_smoke_$1"
  mkdir -p "$d"
  dd if=/dev/urandom of=/tmp/fus_smoke.src bs=1M count=32 status=none
  local a b c
  a=$(md5sum < /tmp/fus_smoke.src | cut -d' ' -f1)
  dd if=/tmp/fus_smoke.src of="$d/blob" bs=1M oflag=direct conv=fsync status=none
  b=$(dd if="$d/blob" bs=1M iflag=direct status=none | md5sum | cut -d' ' -f1)
  c=$(md5sum < "$d/blob" | cut -d' ' -f1)
  [ "$a" = "$b" ] || fatal "leg $1: O_DIRECT readback md5 mismatch — CORRUPTION"
  [ "$a" = "$c" ] || fatal "leg $1: buffered readback md5 mismatch — CORRUPTION"
  local i p
  for i in 1 2 3; do
    dd if=/dev/urandom of=/tmp/fus_p0.src bs=1M count=32 status=none
    a=$(md5sum < /tmp/fus_p0.src | cut -d' ' -f1)
    cp /tmp/fus_p0.src "$d/p0_$i" && sync "$d/p0_$i"
    p=$(md5sum < "$d/p0_$i" | cut -d' ' -f1)
    [ "$a" = "$p" ] || fatal "leg $1 P0 trial $i: readback md5 mismatch"
  done
  rm -rf "$d" /tmp/fus_smoke.src /tmp/fus_p0.src
  echo "leg $1: correctness smoke OK"
}

settle() {
  local need_kb=$((16 * 1024 * 1024)) t=0
  while :; do
    local avail qb
    avail=$(df -k --output=avail "$MNT" | tail -1 | tr -d ' ')
    qb=$(cat "$MNT/.stats" | python3 -c "import json,sys;m=json.load(sys.stdin);m=m.get('metrics',m);print(m.get('block_free_reclaim_queue_bytes',0))")
    if [ "$avail" -ge "$need_kb" ] && [ "$qb" = "0" ]; then break; fi
    t=$((t+5)); [ "$t" -ge 600 ] && fatal "settle: space/reclaim did not converge"
    sleep 5
  done
  sleep 3
}

cpu_snap() { head -1 /proc/stat; }
daemon_cpu() { awk '{print $14+$15}' "/proc/$(pidof squeezefs)/stat"; }
disk_snap() { grep -E " (nvme[5-8]n1) " /proc/diskstats; }

run_row() { # $1 = leg tag, $2 = row name, $3... = fio args
  local leg=$1 row=$2; shift 2
  require_mount "$leg/$row"
  cat "$MNT/.stats" > "$OUT/$leg-$row-before.json"
  cpu_snap > "$OUT/$leg-$row-cpu0"; daemon_cpu > "$OUT/$leg-$row-dcpu0"
  disk_snap > "$OUT/$leg-$row-disk0"
  fio "$@" --output-format=json --output="$OUT/$leg-$row-fio.json" >/dev/null \
    || fatal "$leg/$row: fio exited nonzero — row INVALID"
  daemon_cpu > "$OUT/$leg-$row-dcpu1"; cpu_snap > "$OUT/$leg-$row-cpu1"
  disk_snap > "$OUT/$leg-$row-disk1"
  cat "$MNT/.stats" > "$OUT/$leg-$row-after.json"
}

verdict_write() { # $1 = leg tag, $2 = row, $3 = zc
  python3 - "$OUT" "$1" "$2" "$3" <<'EOF'
import json, sys
out, leg, row, zc = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
b = json.load(open(f"{out}/{leg}-{row}-before.json")); b = b.get("metrics", b)
a = json.load(open(f"{out}/{leg}-{row}-after.json")); a = a.get("metrics", a)
f = json.load(open(f"{out}/{leg}-{row}-fio.json"))
d = lambda k: a.get(k, 0) - b.get(k, 0)
io = sum(j["write"]["io_bytes"] for j in f["jobs"])
bw = sum(j["write"]["bw_bytes"] for j in f["jobs"])
iops = sum(j["write"]["iops"] for j in f["jobs"])
wx, wxb = d("fuse3_zc_write_extractions"), d("fuse3_zc_write_extract_bytes")
wd, wdb = d("fuse3_zc_write_directs"), d("fuse3_zc_write_direct_bytes")
fu = d("fuse3_zc_write_fusions")
fb, sk = d("fuse3_zc_fallbacks"), d("fuse3_zc_slot_payload_skips")
def dsec(p):
    return sum(int(line.split()[9]) for line in open(p)) * 512
amp = (dsec(f"{out}/{leg}-{row}-disk1") - dsec(f"{out}/{leg}-{row}-disk0")) / max(1, io)
print(f"{leg}/{row} zc={zc}: io={io/1e9:.2f} GB bw={bw/1e9:.3f} GB/s iops={iops:.0f} amp={amp:.2f}")
print(f"  directs={wd} ({wdb/1e9:.2f} GB) extractions={wx} ({wxb/1e9:.2f} GB) fusions={fu}")
if zc == 1:
    if (wdb + wxb) < 0.95 * io:
        sys.exit(f"FATAL: {leg}/{row} armed vehicle closure < 95% — engagement NOT exact")
    if fb != 0 or sk != 0:
        sys.exit(f"FATAL: {leg}/{row} fallbacks={fb} slot_skips={sk}")
else:
    if wx or wxb or wd or wdb or fu:
        sys.exit(f"FATAL: control {leg}/{row} shows zc write-vehicle/fusion engagement")
EOF
  echo "$1/$2: engagement verdict PASS"
}

FIO_COMMON=(--ioengine=libaio --direct=1 --group_reporting --fallocate=none
            --time_based --runtime="${RUNTIME:-60}" --ramp_time="${RAMP:-10}"
            --filename_format='sqzfio.$jobnum.$filenum')

write_leg() { # $1 = leg tag, $2 = zc
  local leg=$1 zc=$2
  echo "=== $leg (SQUEEZEFS_FUSE_ZC=$zc) ==="
  do_umount
  do_mount "$zc" "$leg"
  rm -rf "$MNT"/fus_* 2>/dev/null || true
  settle
  smoke "$leg"

  local d="$MNT/fus_rand"; mkdir -p "$d"
  run_row "$leg" rand4k "${FIO_COMMON[@]}" --name=rand4k --directory="$d" \
    --rw=randwrite --bs=4k --iodepth=8 --numjobs=16 --size=256m --norandommap
  verdict_write "$leg" rand4k "$zc"
  rm -rf "$d"; settle

  d="$MNT/fus_rd_ow"; mkdir -p "$d"
  fio --ioengine=libaio --direct=1 --group_reporting --fallocate=none \
      --filename_format='sqzfio.$jobnum.$filenum' --name=prewrite \
      --directory="$d" --rw=write --bs=1M --iodepth=8 --numjobs=16 \
      --size=256m --fsync_on_close=1 --output-format=json \
      --output="$OUT/$leg-rand4kow-prewrite-fio.json" >/dev/null
  sync
  run_row "$leg" rand4kow "${FIO_COMMON[@]}" --name=rand4kow --directory="$d" \
    --rw=randwrite --bs=4k --iodepth=8 --numjobs=16 --size=256m --norandommap \
    --overwrite=1
  verdict_write "$leg" rand4kow "$zc"
  rm -rf "$d"; settle

  d="$MNT/fus_seq"; mkdir -p "$d"
  run_row "$leg" seqwr "${FIO_COMMON[@]}" --name=seqwr --directory="$d" \
    --rw=write --bs=1M --iodepth=8 --numjobs=4 --nrfiles=4 --size=512m
  verdict_write "$leg" seqwr "$zc"
  rm -rf "$d"; settle

  d="$MNT/fus_dur"; mkdir -p "$d"
  run_row "$leg" dur "${FIO_COMMON[@]}" --name=dur --directory="$d" \
    --rw=write --bs=1M --iodepth=8 --numjobs=4 --nrfiles=4 --size=256m --fsync_on_close=1
  verdict_write "$leg" dur "$zc"
  rm -rf "$d"; settle
}

echo "binary: $("$BIN" --version 2>&1 | head -1)"
"$BIN" format "$META" "$DATA" --force >/dev/null || fatal "format failed"

write_leg Z1 1
write_leg Z2 0
write_leg Z3 0
write_leg Z4 1
do_umount

echo "ALL LEGS PASSED — artifacts: $OUT"
