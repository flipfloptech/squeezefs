#!/usr/bin/env bash
# FIO ENGINE POLICY (2026-08-07, .benchmarks/2026-08-07-fio-engine-policy.md):
# libaio + direct=1 + stated iodepth on every row, same engine both A/B
# sides — this rig was already compliant; recorded here per the ruling.
# FUSE-zc A-B-B-A acceptance rig (K1 kill campaign, 2026-08-06).
# Runs ON the field host. Headline cold row: fio libaio direct=1 bs=1M
# numjobs=16 iodepth=8 nrfiles=8 size=8g time_based 60s ramp 10s over
# exa_perf. Legs alternate A-B-B-A (A = SQUEEZEFS_FUSE_ZC=1, B = =0),
# each leg on a FRESH mount (cold discipline). Every gate is FATAL:
# require-mount, arm-proof (fuse3_zc_negotiated), per-leg correctness
# smoke (write→readback md5 + readdir + readlink), and EXACT engagement
# (armed legs: read_zc_serve_bytes ≥ 95% of the fio measurement window,
# zc fallbacks/slot-skips flat; control legs: zc ledger identically 0).
set -euo pipefail

BIN=/scratch/tmp/squeezefs
MNT=/scratch/tmp/test
META="sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1"
OUT=${1:-/scratch/tmp/zc-abba-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$OUT" /scratch/tmp/logs
LEGS=(1 0 0 1)

fatal() { echo "FATAL: $*" >&2; exit 1; }

jget() { python3 -c "import json,sys;d=json.load(open(sys.argv[1]));d=d.get('metrics',d);print(d.get(sys.argv[2],0))" "$1" "$2"; }

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
    --allow-other --log-file "/scratch/tmp/logs/sqz-leg$2-zc$1.log"
  local ok=0
  for _ in $(seq 1 30); do
    if mountpoint -q "$MNT" && cat "$MNT/.stats" >/dev/null 2>&1; then ok=1; break; fi
    sleep 2
  done
  [ "$ok" = 1 ] || fatal "mount gate failed (leg $2 zc=$1)"
  local neg
  cat "$MNT/.stats" > "$OUT/leg$2-arm.json"
  neg=$(jget "$OUT/leg$2-arm.json" fuse3_zc_negotiated)
  [ "$neg" = "$1" ] || fatal "leg $2: fuse3_zc_negotiated=$neg, expected $1 — silent degrade is INVALID"
  echo "leg $2: mounted, zc_negotiated=$neg (expected $1)"
}

smoke() { # $1 = leg tag — correctness proof on THIS mount posture
  local d="$MNT/zc_smoke_$1"
  mkdir -p "$d"
  dd if=/dev/urandom of=/tmp/zc_smoke.src bs=1M count=32 status=none
  # O_DIRECT + fsync write shape: exercises the zc WRITE extraction
  # (fuse_direct_io in_pages) while avoiding the PRE-EXISTING
  # fsync-during-writeback last-block-zeros bug (present on base
  # 50ad803d, zc=0 and zc=1 alike — reported in the campaign note; not
  # this branch's).
  dd if=/tmp/zc_smoke.src of="$d/blob" bs=1M oflag=direct conv=fsync status=none
  ln -sf blob "$d/lnk"                       # READLINK bounce
  local a b l n
  a=$(md5sum < /tmp/zc_smoke.src | cut -d' ' -f1)
  b=$(dd if="$d/blob" bs=1M iflag=direct status=none | md5sum | cut -d' ' -f1)  # zc READ path (direct)
  c=$(cat "$d/blob" | md5sum | cut -d' ' -f1)                                    # zc READ path (buffered)
  [ "$a" = "$b" ] || fatal "leg $1: write→O_DIRECT-readback md5 mismatch ($a vs $b) — CORRUPTION"
  [ "$a" = "$c" ] || fatal "leg $1: write→buffered-readback md5 mismatch ($a vs $c) — CORRUPTION"
  l=$(readlink "$d/lnk")
  [ "$l" = "blob" ] || fatal "leg $1: readlink mismatch ($l)"
  n=$(ls "$d" | wc -l)                       # READDIR bounce
  [ "$n" = 2 ] || fatal "leg $1: readdir count $n != 2"
  rm -rf "$d" /tmp/zc_smoke.src
  echo "leg $1: correctness smoke OK (write/readback md5 + readdir + readlink)"
}

cpu_snap() { head -1 /proc/stat; }
daemon_cpu() { awk '{print $14+$15}' "/proc/$(pidof squeezefs)/stat"; }

run_fio() { # $1 = leg tag
  fio --name=zcrow --directory="$MNT/exa_perf" --filename_format='sqzfio.$jobnum.$filenum' \
      --ioengine=libaio --direct=1 --rw=read --bs=1M --iodepth=8 --numjobs=16 --nrfiles=8 \
      --size=8g --time_based --runtime=60 --ramp_time=10 --group_reporting --norandommap \
      --fallocate=none --output-format=json --output="$OUT/leg$1-fio.json" >/dev/null
}

leg=0
for zc in "${LEGS[@]}"; do
  leg=$((leg+1))
  echo "=== leg $leg (SQUEEZEFS_FUSE_ZC=$zc) ==="
  do_umount
  do_mount "$zc" "$leg"
  smoke "$leg"
  cat "$MNT/.stats" > "$OUT/leg$leg-before.json"
  cpu_snap > "$OUT/leg$leg-cpu0"
  daemon_cpu > "$OUT/leg$leg-dcpu0"
  run_fio "$leg"
  daemon_cpu > "$OUT/leg$leg-dcpu1"
  cpu_snap > "$OUT/leg$leg-cpu1"
  cat "$MNT/.stats" > "$OUT/leg$leg-after.json"

  # --- engagement verdict (FATAL) ---
  python3 - "$OUT" "$leg" "$zc" <<'EOF'
import json, sys
out, leg, zc = sys.argv[1], sys.argv[2], int(sys.argv[3])
b = json.load(open(f"{out}/leg{leg}-before.json")); b = b.get("metrics", b)
a = json.load(open(f"{out}/leg{leg}-after.json")); a = a.get("metrics", a)
f = json.load(open(f"{out}/leg{leg}-fio.json"))
def d(k): return a.get(k, 0) - b.get(k, 0)
io_bytes = sum(j["read"]["io_bytes"] for j in f["jobs"])
bw = sum(j["read"]["bw_bytes"] for j in f["jobs"])
zc_bytes = d("read_zc_serve_bytes")
zc_replies = d("fuse3_zc_replies")
fb = d("fuse3_zc_fallbacks"); sk = d("fuse3_zc_slot_payload_skips")
lease = d("read_dest_lease_bytes"); destdma = d("read_dest_dma_bytes")
destcp = d("read_copy_dest_bytes")
print(f"leg {leg} zc={zc}: fio io_bytes={io_bytes/1e9:.1f} GB bw={bw/1e9:.3f} GB/s")
print(f"  zc_serve_bytes={zc_bytes/1e9:.1f} GB zc_replies={zc_replies} fallbacks={fb} slot_skips={sk}")
print(f"  dest_lease_bytes={lease/1e9:.1f} GB dest_dma={destdma/1e9:.1f} GB dest_copy={destcp/1e9:.1f} GB")
if zc == 1:
    if zc_bytes < 0.95 * io_bytes:
        sys.exit(f"FATAL: leg {leg} armed but zc_serve_bytes {zc_bytes} < 95% of fio window {io_bytes} — engagement NOT exact; row INVALID")
    if zc_replies <= 0:
        sys.exit(f"FATAL: leg {leg} armed with zero zc replies")
    if fb != 0 or sk != 0:
        sys.exit(f"FATAL: leg {leg} zc fallbacks={fb} slot_skips={sk} (must be 0)")
else:
    if zc_bytes != 0 or zc_replies != 0:
        sys.exit(f"FATAL: control leg {leg} shows zc engagement ({zc_bytes} B, {zc_replies} replies)")
EOF
  echo "leg $leg: engagement verdict PASS"
done

# --- table ---
python3 - "$OUT" <<'EOF'
import json, sys
out = sys.argv[1]
legs = [(1,1),(2,0),(3,0),(4,1)]
print(f"\n{'leg':>3} {'zc':>3} {'GB/s':>8} {'box_busy%':>9} {'daemon_cpu_s':>12} {'d_cpu_ms/GB':>11} {'zc_GB':>8} {'lease_GB':>8} {'destcp_GB':>9}")
for leg, zc in legs:
    f = json.load(open(f"{out}/leg{leg}-fio.json"))
    bw = sum(j["read"]["bw_bytes"] for j in f["jobs"]) / 1e9
    io = sum(j["read"]["io_bytes"] for j in f["jobs"]) / 1e9
    c0 = list(map(int, open(f"{out}/leg{leg}-cpu0").read().split()[1:]))
    c1 = list(map(int, open(f"{out}/leg{leg}-cpu1").read().split()[1:]))
    tot = sum(c1) - sum(c0); idle = (c1[3]+c1[4]) - (c0[3]+c0[4])
    busy = 100.0 * (tot - idle) / tot if tot else 0
    dc = (int(open(f"{out}/leg{leg}-dcpu1").read()) - int(open(f"{out}/leg{leg}-dcpu0").read())) / 100.0
    b = json.load(open(f"{out}/leg{leg}-before.json")); b = b.get("metrics", b)
    a = json.load(open(f"{out}/leg{leg}-after.json")); a = a.get("metrics", a)
    d = lambda k: (a.get(k,0)-b.get(k,0))/1e9
    total_gb = io * 70/60  # ramp-inclusive estimate for per-GB CPU
    print(f"{leg:>3} {zc:>3} {bw:>8.3f} {busy:>9.1f} {dc:>12.1f} {1000*dc/total_gb if total_gb else 0:>11.1f} {d('read_zc_serve_bytes'):>8.1f} {d('read_dest_lease_bytes'):>8.1f} {d('read_copy_dest_bytes'):>9.1f}")
EOF
echo "artifacts: $OUT"
