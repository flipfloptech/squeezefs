#!/usr/bin/env bash
# FUSE-zc WRITE-side pricing bracket (fuse-zc-serve §6 follow-up,
# 2026-08-06). Runs ON the field host (squeeze-test: EL8, 6.19.14-sqz,
# 32 CPUs). Six legs, each on a FRESH mount:
#   W1(zc=1) W2(zc=0) W3(zc=0) W4(zc=1)   — write legs (A-B-B-A)
#   R5(zc=1) R6(zc=0)                     — cold seq-read sentinel legs
# Write rows per write leg (60 s sustained, ramp 10 s, fio libaio
# direct=1, group_reporting):
#   seqwr : rw=write bs=1M numjobs=16 iodepth=8 nrfiles=8 size=4g
#           (SHAPE NOTE: the charter said size=8g = 128 GiB fileset; the
#           volume holds 112 GiB beside exa_perf's 128 GiB, so the
#           fileset is halved to 64 GiB — every other parameter and the
#           steady-state overwrite regime are unchanged)
#   dur   : seqwr + fsync_on_close=1 (the RW6 durability-leveled class)
#   rand4k: rw=randwrite bs=4k numjobs=32 iodepth=8 size=1g norandommap
# Every gate is FATAL: require-mount before each row, arm-proof
# (fuse3_zc_negotiated == expected), per-leg correctness (O_DIRECT+fsync
# write→readback md5 ×(direct+buffered) AND the P0 fsync-writeback shape
# `cp 32MiB f && sync f` → md5 ×3 — must stay clean on the ebf3c3f8
# fix), and EXACT engagement:
#   armed write rows  : Δfuse3_zc_write_extract_bytes ≥ 95 % of the fio
#                       window's write io_bytes, Δextractions > 0,
#                       fallbacks/slot-skips flat 0
#   control write rows: zc write-extraction ledger identically 0
#   read legs         : the zc-serve rig's gates verbatim
# Instruments per row: fio GB/s+IOPS+clat, .stats before/after (zc
# ledger, write_pipeline_phase_ns, write-amp counters), /proc/stat +
# daemon utime+stime, /proc/diskstats deltas on the DATA namespaces
# (nvme{10,12,14,16,18}n1), mpstat+pidstat tapes.
set -euo pipefail

BIN=/scratch/tmp/squeezefs
MNT=/scratch/tmp/test
META="sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1"
OUT=${1:-/scratch/tmp/zcw-bracket-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$OUT" /scratch/tmp/logs
DATA_DEVS="nvme10n1 nvme12n1 nvme14n1 nvme16n1 nvme18n1"

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
  for _ in $(seq 1 300); do
    pidof squeezefs >/dev/null 2>&1 || break
    sleep 2
  done
  pidof squeezefs >/dev/null 2>&1 && fatal "daemon did not exit after umount drain"
  mountpoint -q "$MNT" && fatal "mountpoint still mounted"
  return 0
}

do_mount() { # $1 = zc value, $2 = leg tag
  SQUEEZEFS_FUSE_ZC=$1 "$BIN" mount "$META" "$MNT" --daemon --interception \
    --allow-other --log-file "/scratch/tmp/logs/sqz-$2-zc$1.log"
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
  echo "leg $2: mounted, zc_negotiated=$neg (expected $1)"
}

smoke() { # $1 = leg tag
  require_mount "smoke $1"
  local d="$MNT/zcw_smoke_$1"
  mkdir -p "$d"
  dd if=/dev/urandom of=/tmp/zcw_smoke.src bs=1M count=32 status=none
  local a b c
  a=$(md5sum < /tmp/zcw_smoke.src | cut -d' ' -f1)
  # O_DIRECT + fsync write shape (the zc WRITE extraction lives here).
  dd if=/tmp/zcw_smoke.src of="$d/blob" bs=1M oflag=direct conv=fsync status=none
  b=$(dd if="$d/blob" bs=1M iflag=direct status=none | md5sum | cut -d' ' -f1)
  c=$(md5sum < "$d/blob" | cut -d' ' -f1)
  [ "$a" = "$b" ] || fatal "leg $1: O_DIRECT write→direct-readback md5 mismatch — CORRUPTION"
  [ "$a" = "$c" ] || fatal "leg $1: O_DIRECT write→buffered-readback md5 mismatch — CORRUPTION"
  # The P0 fsync-during-writeback shape (fixed by ebf3c3f8): buffered cp
  # + fsync(file) ×3 — must be 3/3 clean on this binary.
  local i p q
  for i in 1 2 3; do
    dd if=/dev/urandom of=/tmp/zcw_p0.src bs=1M count=32 status=none
    a=$(md5sum < /tmp/zcw_p0.src | cut -d' ' -f1)
    cp /tmp/zcw_p0.src "$d/p0_$i" && sync "$d/p0_$i"
    p=$(md5sum < "$d/p0_$i" | cut -d' ' -f1)
    q=$(dd if="$d/p0_$i" bs=1M iflag=direct status=none | md5sum | cut -d' ' -f1)
    [ "$a" = "$p" ] || fatal "leg $1 P0 trial $i: buffered readback md5 mismatch — the tail-loss shape REGRESSED"
    [ "$a" = "$q" ] || fatal "leg $1 P0 trial $i: O_DIRECT readback md5 mismatch — the tail-loss shape REGRESSED"
  done
  rm -rf "$d" /tmp/zcw_smoke.src /tmp/zcw_p0.src
  echo "leg $1: correctness smoke OK (O_DIRECT+fsync md5 ×2 + P0 cp+sync-file ×3)"
}

settle() { # reclaim/space settle between rows: rm'd fileset must return
  local need_kb=$((70 * 1024 * 1024)) t=0
  while :; do
    local avail qb
    avail=$(df -k --output=avail "$MNT" | tail -1 | tr -d ' ')
    qb=$(cat "$MNT/.stats" | python3 -c "import json,sys;m=json.load(sys.stdin);m=m.get('metrics',m);print(m.get('block_free_reclaim_queue_bytes',0))")
    if [ "$avail" -ge "$need_kb" ] && [ "$qb" = "0" ]; then break; fi
    t=$((t+5)); [ "$t" -ge 600 ] && fatal "settle: space/reclaim did not converge (avail=${avail}K queue=${qb}B)"
    sleep 5
  done
  sleep 5
}

cpu_snap() { head -1 /proc/stat; }
daemon_cpu() { awk '{print $14+$15}' "/proc/$(pidof squeezefs)/stat"; }
disk_snap() { grep -E " (nvme1[02468]n1) " /proc/diskstats; }

run_row() { # $1 = leg tag, $2 = row name, $3... = fio args
  local leg=$1 row=$2; shift 2
  require_mount "$leg/$row"
  cat "$MNT/.stats" > "$OUT/$leg-$row-before.json"
  cpu_snap > "$OUT/$leg-$row-cpu0"; daemon_cpu > "$OUT/$leg-$row-dcpu0"
  disk_snap > "$OUT/$leg-$row-disk0"
  mpstat 5 > "$OUT/$leg-$row-mpstat.log" 2>&1 &
  local mp=$!
  pidstat -u -p "$(pidof squeezefs)" 5 > "$OUT/$leg-$row-pidstat.log" 2>&1 &
  local ps=$!
  fio "$@" --output-format=json --output="$OUT/$leg-$row-fio.json" >/dev/null
  kill "$mp" "$ps" 2>/dev/null || true; wait "$mp" "$ps" 2>/dev/null || true
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
fb, sk = d("fuse3_zc_fallbacks"), d("fuse3_zc_slot_payload_skips")
zr = d("fuse3_zc_replies")
print(f"{leg}/{row} zc={zc}: window io={io/1e9:.1f} GB bw={bw/1e9:.3f} GB/s iops={iops:.0f}")
print(f"  zc_write_extractions={wx} extract_bytes={wxb/1e9:.1f} GB fallbacks={fb} slot_skips={sk} zc_replies={zr}")
if zc == 1:
    if wxb < 0.95 * io:
        sys.exit(f"FATAL: {leg}/{row} armed but extract_bytes {wxb} < 95% of window {io} — engagement NOT exact; row INVALID")
    if wx <= 0:
        sys.exit(f"FATAL: {leg}/{row} armed with zero write extractions")
    if fb != 0 or sk != 0:
        sys.exit(f"FATAL: {leg}/{row} fallbacks={fb} slot_skips={sk} (must be 0)")
else:
    if wx != 0 or wxb != 0:
        sys.exit(f"FATAL: control {leg}/{row} shows write-extraction engagement ({wx} ops, {wxb} B)")
EOF
  echo "$1/$2: engagement verdict PASS"
}

FIO_COMMON=(--ioengine=libaio --direct=1 --group_reporting --fallocate=none
            --time_based --runtime=60 --ramp_time=10
            --filename_format='sqzfio.$jobnum.$filenum')

write_leg() { # $1 = leg tag, $2 = zc
  local leg=$1 zc=$2
  echo "=== $leg (SQUEEZEFS_FUSE_ZC=$zc) ==="
  do_umount
  do_mount "$zc" "$leg"
  smoke "$leg"

  # row 1: seq write 1M
  local d="$MNT/zcw_seq"; mkdir -p "$d"
  run_row "$leg" seqwr "${FIO_COMMON[@]}" --name=seqwr --directory="$d" \
    --rw=write --bs=1M --iodepth=8 --numjobs=16 --nrfiles=8 --size=4g
  verdict_write "$leg" seqwr "$zc"
  rm -rf "$d"; settle

  # row 2: durable seq write (fsync_on_close)
  d="$MNT/zcw_dur"; mkdir -p "$d"
  run_row "$leg" dur "${FIO_COMMON[@]}" --name=dur --directory="$d" \
    --rw=write --bs=1M --iodepth=8 --numjobs=16 --nrfiles=8 --size=4g --fsync_on_close=1
  verdict_write "$leg" dur "$zc"
  rm -rf "$d"; settle

  # row 3: rand-4k write
  d="$MNT/zcw_rand"; mkdir -p "$d"
  run_row "$leg" rand4k "${FIO_COMMON[@]}" --name=rand4k --directory="$d" \
    --rw=randwrite --bs=4k --iodepth=8 --numjobs=32 --size=1g --norandommap
  verdict_write "$leg" rand4k "$zc"
  rm -rf "$d"; settle
}

read_leg() { # $1 = leg tag, $2 = zc — the standing cold seq-read recipe
  local leg=$1 zc=$2
  echo "=== $leg read sentinel (SQUEEZEFS_FUSE_ZC=$zc) ==="
  do_umount
  do_mount "$zc" "$leg"
  require_mount "$leg/read"
  cat "$MNT/.stats" > "$OUT/$leg-read-before.json"
  cpu_snap > "$OUT/$leg-read-cpu0"; daemon_cpu > "$OUT/$leg-read-dcpu0"
  fio --name=zcrow --directory="$MNT/exa_perf" --filename_format='sqzfio.$jobnum.$filenum' \
      --ioengine=libaio --direct=1 --rw=read --bs=1M --iodepth=8 --numjobs=16 --nrfiles=8 \
      --size=8g --time_based --runtime=60 --ramp_time=10 --group_reporting --norandommap \
      --fallocate=none --output-format=json --output="$OUT/$leg-read-fio.json" >/dev/null
  daemon_cpu > "$OUT/$leg-read-dcpu1"; cpu_snap > "$OUT/$leg-read-cpu1"
  cat "$MNT/.stats" > "$OUT/$leg-read-after.json"
  python3 - "$OUT" "$leg" "$zc" <<'EOF'
import json, sys
out, leg, zc = sys.argv[1], sys.argv[2], int(sys.argv[3])
b = json.load(open(f"{out}/{leg}-read-before.json")); b = b.get("metrics", b)
a = json.load(open(f"{out}/{leg}-read-after.json")); a = a.get("metrics", a)
f = json.load(open(f"{out}/{leg}-read-fio.json"))
d = lambda k: a.get(k, 0) - b.get(k, 0)
io = sum(j["read"]["io_bytes"] for j in f["jobs"])
bw = sum(j["read"]["bw_bytes"] for j in f["jobs"])
zb, zr = d("read_zc_serve_bytes"), d("fuse3_zc_replies")
fb, sk = d("fuse3_zc_fallbacks"), d("fuse3_zc_slot_payload_skips")
print(f"{leg}/read zc={zc}: io={io/1e9:.1f} GB bw={bw/1e9:.3f} GB/s zc_bytes={zb/1e9:.1f} GB replies={zr} fb={fb} sk={sk}")
if zc == 1:
    if zb < 0.95 * io: sys.exit(f"FATAL: {leg}/read armed but zc_serve_bytes < 95% of window — INVALID")
    if zr <= 0 or fb != 0 or sk != 0: sys.exit(f"FATAL: {leg}/read replies={zr} fb={fb} sk={sk}")
else:
    if zb != 0 or zr != 0: sys.exit(f"FATAL: control {leg}/read shows zc engagement")
EOF
  echo "$leg/read: engagement verdict PASS"
}

write_leg W1 1
write_leg W2 0
write_leg W3 0
write_leg W4 1
read_leg  R5 1
read_leg  R6 0

echo "ALL LEGS PASSED — artifacts: $OUT"
