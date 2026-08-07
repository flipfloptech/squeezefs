#!/usr/bin/env bash
# zc-write-fusion local acceptance bracket (2026-08-07 — D16 fast-track).
# FIO ENGINE POLICY (.benchmarks/2026-08-07-fio-engine-policy.md): libaio +
# direct=1 + stated iodepth on every row, SAME engine both A/B sides.
#
# Venue: LOCAL tcp devsub (nvmet-tcp on 127.0.0.1 — the fabric-sensitive
# substrate the two-substrate rule mandates for write rows), 7.1.6-1-
# cachyos-sqz, 32 CPUs, fs.fuse.max_pages_limit=256 (payload 1 MiB ⇒
# derived fusion ceiling 128 KiB). Root (the zc arm needs CAP_SYS_ADMIN).
#
# Both sides ARMED (SQUEEZEFS_FUSE_ZC default ON since d10e5a22); the A/B
# lever is the fusion knob:
#   F1(fusion=1) F2(fusion=0) F3(fusion=0) F4(fusion=1)   — A-B-B-A
# Rows per leg (60 s sustained, ramp 10 s, sizes scaled to the 32 GiB
# devsub — deviations from the field rig stated in the evidence note):
#   rand4k  : randwrite bs=4k numjobs=16 iodepth=8 size=256m norandommap
#             (the caveat's headline shape — fresh fileset)
#   rand4kow: the same randwrite over a PREWRITTEN durably-published
#             fileset (the W1/D14 direct-DMA regime)
#   seqwr   : write bs=1M numjobs=4 iodepth=8 nrfiles=4 size=512m
#             (no-regression sentinel — fusion must never engage here)
#   dur     : seqwr shape + fsync_on_close=1 size=256m (durable sentinel)
# FATAL gates per leg/row: require-mount; arm proof (fuse3_zc_negotiated=1
# BOTH sides); P0 smoke (O_DIRECT+fsync md5 + cp+sync-file ×3); EXACT
# engagement — armed vehicle closure (direct+extract ≥ 95 % of window
# bytes) on every row, fusion legs' rand rows fusions ≥ 95 % of ops,
# control legs fusions ≡ 0, seq/dur rows fusions ≈ 0 on BOTH sides;
# bridge tripwires (cancels/lost) flat 0 per leg. Write-amp columns per
# the standing law (device÷user bytes on the DATA namespaces).
set -euo pipefail

BIN=${BIN:-/home/justin/Source/.sqz-fusion-target/release/squeezefs}
MNT=${MNT:-/run/sqz-fusion/mnt}
META="sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1"
DATA="sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1"
DATA_DEVS="nvme5n1 nvme6n1 nvme7n1 nvme8n1"
OUT=${1:-/run/sqz-fusion/out-$(date +%Y%m%d-%H%M%S)}
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

do_mount() { # $1 = fusion value, $2 = leg tag
  # --uid/--gid 0 explicitly: under sudo the daemon derives the FUSE
  # user_id from SUDO_UID, which locks the root rig out of its own
  # mount (no allow_other here by design).
  SQUEEZEFS_FUSE_ZC_WRITE_FUSION=$1 "$BIN" mount "$META" "$MNT" --daemon \
    --uid 0 --gid 0 \
    --log-file "/run/sqz-fusion/logs/sqz-$2-fusion$1.log"
  local ok=0
  for _ in $(seq 1 30); do
    if mountpoint -q "$MNT" && cat "$MNT/.stats" >/dev/null 2>&1; then ok=1; break; fi
    sleep 2
  done
  [ "$ok" = 1 ] || fatal "mount gate failed (leg $2 fusion=$1)"
  cat "$MNT/.stats" > "$OUT/$2-arm.json"
  local neg
  neg=$(jget "$OUT/$2-arm.json" fuse3_zc_negotiated)
  [ "$neg" = 1 ] || fatal "leg $2: fuse3_zc_negotiated=$neg, expected 1 — BOTH bracket sides run armed; a declined arm is INVALID"
  echo "leg $2: mounted armed (zc_negotiated=1, fusion=$1)"
}

smoke() { # $1 = leg tag — P0 per the standing rig shape
  require_mount "smoke $1"
  local d="$MNT/fus_smoke_$1"
  mkdir -p "$d"
  dd if=/dev/urandom of=/tmp/fus_smoke.src bs=1M count=32 status=none
  local a b c
  a=$(md5sum < /tmp/fus_smoke.src | cut -d' ' -f1)
  dd if=/tmp/fus_smoke.src of="$d/blob" bs=1M oflag=direct conv=fsync status=none
  b=$(dd if="$d/blob" bs=1M iflag=direct status=none | md5sum | cut -d' ' -f1)
  c=$(md5sum < "$d/blob" | cut -d' ' -f1)
  [ "$a" = "$b" ] || fatal "leg $1: O_DIRECT write→direct-readback md5 mismatch — CORRUPTION"
  [ "$a" = "$c" ] || fatal "leg $1: O_DIRECT write→buffered-readback md5 mismatch — CORRUPTION"
  local i p q
  for i in 1 2 3; do
    dd if=/dev/urandom of=/tmp/fus_p0.src bs=1M count=32 status=none
    a=$(md5sum < /tmp/fus_p0.src | cut -d' ' -f1)
    cp /tmp/fus_p0.src "$d/p0_$i" && sync "$d/p0_$i"
    p=$(md5sum < "$d/p0_$i" | cut -d' ' -f1)
    q=$(dd if="$d/p0_$i" bs=1M iflag=direct status=none | md5sum | cut -d' ' -f1)
    [ "$a" = "$p" ] || fatal "leg $1 P0 trial $i: buffered readback md5 mismatch"
    [ "$a" = "$q" ] || fatal "leg $1 P0 trial $i: O_DIRECT readback md5 mismatch"
  done
  rm -rf "$d" /tmp/fus_smoke.src /tmp/fus_p0.src
  echo "leg $1: correctness smoke OK (O_DIRECT+fsync md5 + P0 cp+sync-file ×3)"
}

tripwires() { # $1 = leg tag — bridge tripwires flat 0 at leg end
  cat "$MNT/.stats" > "$OUT/$1-end.json"
  local c l
  c=$(jget "$OUT/$1-end.json" fuse3_zc_bridge_cancels)
  l=$(jget "$OUT/$1-end.json" fuse3_zc_bridge_lost)
  [ "$c" = 0 ] && [ "$l" = 0 ] || fatal "leg $1: bridge tripwires cancels=$c lost=$l (must stay 0)"
  echo "leg $1: tripwires flat (cancels=0 lost=0)"
}

settle() {
  local need_kb=$((16 * 1024 * 1024)) t=0
  while :; do
    local avail qb
    avail=$(df -k --output=avail "$MNT" | tail -1 | tr -d ' ')
    qb=$(cat "$MNT/.stats" | python3 -c "import json,sys;m=json.load(sys.stdin);m=m.get('metrics',m);print(m.get('block_free_reclaim_queue_bytes',0))")
    if [ "$avail" -ge "$need_kb" ] && [ "$qb" = "0" ]; then break; fi
    t=$((t+5)); [ "$t" -ge 600 ] && fatal "settle: space/reclaim did not converge (avail=${avail}K queue=${qb}B)"
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

verdict_write() { # $1 = leg tag, $2 = row, $3 = fusion, $4 = rand|seq
  python3 - "$OUT" "$1" "$2" "$3" "$4" <<'EOF'
import json, sys
out, leg, row, fusion, klass = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5]
b = json.load(open(f"{out}/{leg}-{row}-before.json")); b = b.get("metrics", b)
a = json.load(open(f"{out}/{leg}-{row}-after.json")); a = a.get("metrics", a)
f = json.load(open(f"{out}/{leg}-{row}-fio.json"))
d = lambda k: a.get(k, 0) - b.get(k, 0)
io = sum(j["write"]["io_bytes"] for j in f["jobs"])
bw = sum(j["write"]["bw_bytes"] for j in f["jobs"])
iops = sum(j["write"]["iops"] for j in f["jobs"])
lat = f["jobs"][0]["write"].get("clat_ns", {}).get("percentile", {})
p50, p99 = lat.get("50.000000", 0)/1e6, lat.get("99.000000", 0)/1e6
wx, wxb = d("fuse3_zc_write_extractions"), d("fuse3_zc_write_extract_bytes")
wd, wdb = d("fuse3_zc_write_directs"), d("fuse3_zc_write_direct_bytes")
fu, fub = d("fuse3_zc_write_fusions"), d("fuse3_zc_write_fusion_bytes")
dem = d("fuse3_zc_write_fusion_demotions")
fb, sk = d("fuse3_zc_fallbacks"), d("fuse3_zc_slot_payload_skips")
pw = d("patch_writes")
ops = io // 4096 if row.startswith("rand") else max(1, io // (1 << 20))
# Write amp: device sectors written on the DATA namespaces ÷ user bytes.
def dsec(p):
    t = 0
    for line in open(p):
        t += int(line.split()[9])  # sectors written
    return t * 512
amp = (dsec(f"{out}/{leg}-{row}-disk1") - dsec(f"{out}/{leg}-{row}-disk0")) / max(1, io)
print(f"{leg}/{row} fusion={fusion}: io={io/1e9:.2f} GB bw={bw/1e9:.3f} GB/s iops={iops:.0f} p50={p50:.2f}ms p99={p99:.2f}ms amp={amp:.2f}")
print(f"  directs={wd} ({wdb/1e9:.2f} GB) extractions={wx} ({wxb/1e9:.2f} GB) fusions={fu} ({fub/1e9:.2f} GB) demotions={dem}")
print(f"  fallbacks={fb} slot_skips={sk} patch_writes={pw}")
if (wdb + wxb) < 0.95 * io:
    sys.exit(f"FATAL: {leg}/{row} vehicle closure {wdb+wxb} < 95% of window {io} — engagement NOT exact")
if fb != 0 or sk != 0:
    sys.exit(f"FATAL: {leg}/{row} fallbacks={fb} slot_skips={sk} (must be 0)")
if fusion == 1 and klass == "rand" and fu < 0.95 * ops:
    sys.exit(f"FATAL: {leg}/{row} fusion leg but fusions {fu} < 95% of {ops} ops — the fused lane is not carrying the row")
if fusion == 0 and fu != 0:
    sys.exit(f"FATAL: control {leg}/{row} shows fused dispatches ({fu})")
if klass == "seq" and fu > 0.01 * ops:
    sys.exit(f"FATAL: {leg}/{row} seq row fused {fu} ops (> 1% of {ops}) — the ceiling is not bounding the lane")
EOF
  echo "$1/$2: engagement verdict PASS"
}

# RUNTIME/RAMP overridable for rig plumbing smokes ONLY — a counted
# bracket runs the 60/10 default (the sustained-state law).
FIO_COMMON=(--ioengine=libaio --direct=1 --group_reporting --fallocate=none
            --time_based --runtime="${RUNTIME:-60}" --ramp_time="${RAMP:-10}"
            --filename_format='sqzfio.$jobnum.$filenum')

write_leg() { # $1 = leg tag, $2 = fusion
  local leg=$1 fusion=$2
  echo "=== $leg (SQUEEZEFS_FUSE_ZC_WRITE_FUSION=$fusion, zc armed) ==="
  do_umount
  do_mount "$fusion" "$leg"
  rm -rf "$MNT"/fus_* 2>/dev/null || true
  settle
  smoke "$leg"

  # row 1: rand-4k, fresh fileset (the caveat's headline shape)
  local d="$MNT/fus_rand"; mkdir -p "$d"
  run_row "$leg" rand4k "${FIO_COMMON[@]}" --name=rand4k --directory="$d" \
    --rw=randwrite --bs=4k --iodepth=8 --numjobs=16 --size=256m --norandommap
  verdict_write "$leg" rand4k "$fusion" rand
  rm -rf "$d"; settle

  # row 2: rand-4k OVERWRITE regime (prewrite untimed, durably published)
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
  verdict_write "$leg" rand4kow "$fusion" rand
  rm -rf "$d"; settle

  # row 3: seq-write sentinel (fusion must NOT engage — the ceiling law)
  d="$MNT/fus_seq"; mkdir -p "$d"
  run_row "$leg" seqwr "${FIO_COMMON[@]}" --name=seqwr --directory="$d" \
    --rw=write --bs=1M --iodepth=8 --numjobs=4 --nrfiles=4 --size=512m
  verdict_write "$leg" seqwr "$fusion" seq
  rm -rf "$d"; settle

  # row 4: durable sentinel (fsync_on_close)
  d="$MNT/fus_dur"; mkdir -p "$d"
  run_row "$leg" dur "${FIO_COMMON[@]}" --name=dur --directory="$d" \
    --rw=write --bs=1M --iodepth=8 --numjobs=4 --nrfiles=4 --size=256m --fsync_on_close=1
  verdict_write "$leg" dur "$fusion" seq
  rm -rf "$d"; settle

  tripwires "$leg"
}

echo "binary: $("$BIN" --version 2>&1 | head -1)"
"$BIN" format "$META" "$DATA" --force >/dev/null || fatal "format failed"

write_leg F1 1
write_leg F2 0
write_leg F3 0
write_leg F4 1
do_umount

echo "ALL LEGS PASSED — artifacts: $OUT"
