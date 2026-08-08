#!/usr/bin/env bash
# fused-lane PREDICATE bracket (2026-08-08 — perf/fused-lane-predicate).
# FIO ENGINE POLICY: libaio + direct=1 + stated iodepth, same engine all legs.
#
# Venue: LOCAL tcp devsub with a netem fabric-RTT emulation on lo
# (NETEM_US each way; 150 ⇒ measured ~0.36 ms RTT — the field's ~300 µs
# class), 7.1.6-1-cachyos-sqz, 32 CPUs, payload pinned 1 MiB. Root.
#
# Legs are (zc, fusion) postures; rows per leg (RUNTIME s sustained):
#   rand4k_hole : randwrite bs=4k numjobs=32 iodepth=8 size=128m fresh
#                 fileset — the FIELD discriminator shape (32×qd8).
#                 Ineligible-majority early (holes/growth), drifts
#                 toward eligible as coverage publishes mappings.
#   grow4k      : rw=write bs=4k (O_DIRECT append stream) — the PURE
#                 ineligible shape (extending + stream-adjacent: the
#                 W1 ladder refuses it twice over). fusions must be ≈0
#                 post-fix; extractions ≈ ops.
#   rand4kow    : prewritten durably-published fileset, randwrite bs=4k
#                 — the W1-eligible shape (direct-DMA population).
#   seqwr       : 1 MiB no-regression sentinel.
# FATAL gates: require-mount; arm proof per posture; P0 smoke; the
# STATS-PIN columns (team directive) printed per row and gated:
#   fuse3_zc_negotiated, fuse3_zc_write_fusions/_bytes/_demotions,
#   fuse3_zc_write_extractions vs directs (+ the both-vehicles
#   double-pay ratio (fusions∩extractions)/ops), lazy (late) extraction
#   split where exported, ipc_session_owners / ipc_direct_shards /
#   ipc_ingress_ns mean / ipc_drain_pass_ns ops-per-pass + µs/op (il
#   families — 0/absent on these un-shimmed kernel-lane rows: printed,
#   not gated nonzero).
set -euo pipefail

BIN=${BIN:-/home/justin/Source/.sqz-fusedrtt-target/release/squeezefs}
MNT=${MNT:-/run/sqz-fusedrtt/mnt}
META="sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1"
DATA="sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1"
OUT=${1:-/run/sqz-fusedrtt/out-$(date +%Y%m%d-%H%M%S)}
LEGS=${LEGS:-"A:1:1 B:1:0 C:1:0 D:1:1"}   # tag:zc:fusion — default A-B-B-A on the fusion lever
ROWS=${ROWS:-"rand4k_hole grow4k rand4kow seqwr"}
NETEM_US=${NETEM_US:-150}
mkdir -p "$OUT" "$MNT" /run/sqz-fusedrtt/logs

fatal() { echo "FATAL: $*" >&2; exit 1; }
jget() { python3 -c "import json,sys;d=json.load(open(sys.argv[1]));d=d.get('metrics',d);print(d.get(sys.argv[2],0))" "$1" "$2"; }

netem_on() {
  # Never stack on a foreign qdisc: refuse unless lo carries the default.
  local q; q=$(tc qdisc show dev lo | head -1)
  case "$q" in
    *noqueue*) ;; *) fatal "lo carries a foreign qdisc ($q) — refusing to touch it";;
  esac
  tc qdisc add dev lo root netem delay "${NETEM_US}us" limit 10000
  echo "netem: lo delay ${NETEM_US}us each way ($(ping -c 10 -i 0.05 -q 127.0.0.1 | tail -1 | cut -d= -f2))"
}
netem_off() { tc qdisc del dev lo root 2>/dev/null || true; }

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

do_mount() { # $1 = zc, $2 = fusion, $3 = leg tag
  SQUEEZEFS_FUSE_ZC=$1 SQUEEZEFS_FUSE_ZC_WRITE_FUSION=$2 \
    "$BIN" mount "$META" "$MNT" --daemon --uid 0 --gid 0 \
    --log-file "/run/sqz-fusedrtt/logs/sqz-$3-zc$1-f$2.log"
  local ok=0
  for _ in $(seq 1 30); do
    if mountpoint -q "$MNT" && cat "$MNT/.stats" >/dev/null 2>&1; then ok=1; break; fi
    sleep 2
  done
  [ "$ok" = 1 ] || fatal "mount gate failed (leg $3)"
  cat "$MNT/.stats" > "$OUT/$3-arm.json"
  local neg
  neg=$(jget "$OUT/$3-arm.json" fuse3_zc_negotiated)
  [ "$neg" = "$1" ] || fatal "leg $3: fuse3_zc_negotiated=$neg, expected $1"
  echo "leg $3: mounted (zc=$1 fusion=$2, negotiated=$neg)"
}

smoke() { # $1 = leg tag
  require_mount "smoke $1"
  local d="$MNT/fp_smoke_$1"
  mkdir -p "$d"
  dd if=/dev/urandom of=/tmp/fp_smoke.src bs=1M count=32 status=none
  local a b c i p
  a=$(md5sum < /tmp/fp_smoke.src | cut -d' ' -f1)
  dd if=/tmp/fp_smoke.src of="$d/blob" bs=1M oflag=direct conv=fsync status=none
  b=$(dd if="$d/blob" bs=1M iflag=direct status=none | md5sum | cut -d' ' -f1)
  c=$(md5sum < "$d/blob" | cut -d' ' -f1)
  [ "$a" = "$b" ] && [ "$a" = "$c" ] || fatal "leg $1: md5 mismatch — CORRUPTION"
  for i in 1 2 3; do
    dd if=/dev/urandom of=/tmp/fp_p0.src bs=1M count=16 status=none
    a=$(md5sum < /tmp/fp_p0.src | cut -d' ' -f1)
    cp /tmp/fp_p0.src "$d/p0_$i" && sync "$d/p0_$i"
    p=$(md5sum < "$d/p0_$i" | cut -d' ' -f1)
    [ "$a" = "$p" ] || fatal "leg $1 P0 trial $i: md5 mismatch"
  done
  rm -rf "$d" /tmp/fp_smoke.src /tmp/fp_p0.src
  echo "leg $1: correctness smoke OK"
}

settle() {
  local need_kb=$((16 * 1024 * 1024)) t=0
  while :; do
    local avail qb
    avail=$(df -k --output=avail "$MNT" | tail -1 | tr -d ' ')
    qb=$(cat "$MNT/.stats" | python3 -c "import json,sys;m=json.load(sys.stdin);m=m.get('metrics',m);print(m.get('block_free_reclaim_queue_bytes',0))")
    if [ "$avail" -ge "$need_kb" ] && [ "$qb" = "0" ]; then break; fi
    t=$((t+5)); [ "$t" -ge 900 ] && fatal "settle: space/reclaim did not converge"
    sleep 5
  done
  sleep 3
}

daemon_cpu() { awk '{print $14+$15}' "/proc/$(pgrep -n squeezefs)/stat" 2>/dev/null || echo 0; }

run_row() { # $1 = leg tag, $2 = row name, $3... = fio args
  local leg=$1 row=$2; shift 2
  require_mount "$leg/$row"
  cat "$MNT/.stats" > "$OUT/$leg-$row-before.json"
  daemon_cpu > "$OUT/$leg-$row-dcpu0"
  fio "$@" --output-format=json --output="$OUT/$leg-$row-fio.json" >/dev/null \
    || fatal "$leg/$row: fio exited nonzero — row INVALID"
  daemon_cpu > "$OUT/$leg-$row-dcpu1"
  cat "$MNT/.stats" > "$OUT/$leg-$row-after.json"
}

# The STATS-PIN verdict (team directive): per-row engagement + the
# double-pay signature, FATAL where the posture defines an expectation.
verdict() { # $1 leg, $2 row, $3 zc, $4 fusion, $5 shape-class (hole|grow|ow|seq)
  python3 - "$OUT" "$1" "$2" "$3" "$4" "$5" <<'EOF'
import json, sys
out, leg, row, zc, fusion, klass = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5]), sys.argv[6]
b = json.load(open(f"{out}/{leg}-{row}-before.json")); b = b.get("metrics", b)
a = json.load(open(f"{out}/{leg}-{row}-after.json")); a = a.get("metrics", a)
f = json.load(open(f"{out}/{leg}-{row}-fio.json"))
d = lambda k: a.get(k, 0) - b.get(k, 0)
io = sum(j["write"]["io_bytes"] for j in f["jobs"])
bw = sum(j["write"]["bw_bytes"] for j in f["jobs"])
iops = sum(j["write"]["iops"] for j in f["jobs"])
lat = f["jobs"][0]["write"].get("clat_ns", {}).get("percentile", {})
p50, p99 = lat.get("50.000000", 0)/1e6, lat.get("99.000000", 0)/1e6
ops = io // 4096 if klass in ("hole", "grow", "ow") else max(1, io // (1 << 20))
wx, wxb = d("fuse3_zc_write_extractions"), d("fuse3_zc_write_extract_bytes")
wd, wdb = d("fuse3_zc_write_directs"), d("fuse3_zc_write_direct_bytes")
fu, fub = d("fuse3_zc_write_fusions"), d("fuse3_zc_write_fusion_bytes")
dem = d("fuse3_zc_write_fusion_demotions")
lazy = d("fuse3_zc_write_lazy_extractions")  # 0 pre-split builds
fb, sk = d("fuse3_zc_fallbacks"), d("fuse3_zc_slot_payload_skips")
# Vehicle accounting caveat: the stats counters span the WHOLE fio run
# (layout + ramp + window) while fio's io_bytes is the timed window
# only, so cross-domain arithmetic ((directs+extractions) - ops) is an
# ESTIMATE that over-counts on laid-out shapes. The EXACT per-op
# double-pay instrument post-fix is fuse3_zc_write_lazy_extractions (a
# held op that ALSO extracted); pre-fix binaries lack it, and there the
# team signature (fusions ~= ops AND extractions ~= ops) is the read.
double = max(0, (wd + wx) - ops)
# il stats-pin columns (team list) — printed; 0/absent legal on
# un-shimmed kernel-lane rows.
owners = a.get("ipc_session_owners", "ABSENT")
shards = a.get("ipc_direct_shards", "ABSENT")
def mid_us(lbl):
    # "<=1us" .. "<=1024us", "<=2ms" .. "<=1024ms", "<=2s".."<=16s", ">16s"
    if lbl == ">16s": return 16e6
    v = lbl[2:]
    if v.endswith("us"): c = float(v[:-2])
    elif v.endswith("ms"): c = float(v[:-2]) * 1e3
    else: c = float(v[:-1]) * 1e6
    return 0.5 if c <= 1 else 0.75 * c
def hist_delta(key):
    hb, ha = b.get(key), a.get(key)
    if not isinstance(ha, dict): return None
    n, w = 0, 0.0
    for lbl, av in ha.items():
        dd = av - (hb.get(lbl, 0) if isinstance(hb, dict) else 0)
        if dd > 0:
            n += dd; w += dd * mid_us(lbl)
    return (n, (w / n) if n else 0.0)
ing_h = hist_delta("ipc_ingress_ns")
ing = f"{ing_h[1]:.1f}us(n={ing_h[0]})" if ing_h and ing_h[0] else "0"
dp_h = hist_delta("ipc_drain_pass_ns")
ipc_ops = d("ipc_ops_read") + d("ipc_ops_write")
if dp_h and dp_h[0]:
    opp = ipc_ops / dp_h[0]
    dpline = f"passes={dp_h[0]} mean={dp_h[1]:.1f}us ops/pass={opp:.1f} us/op={dp_h[1]/max(opp,1e-9):.2f}"
else:
    dpline = "0"
print(f"{leg}/{row} zc={zc} f={fusion}: io={io/1e9:.2f}GB bw={bw/1e9:.3f}GB/s iops={iops:.0f} p50={p50:.2f} p99={p99:.2f}")
print(f"  PIN neg={a.get('fuse3_zc_negotiated')} fusions={fu} fu_bytes={fub} dem={dem} extractions={wx} directs={wd} lazy={lazy} est_excess={double} ({100*double/max(1,ops):.1f}% of {ops} window ops)")
print(f"  PIN ipc_session_owners={owners} ipc_direct_shards={shards} ipc_ingress_ns_mean={ing} ipc_drain_pass[{dpline}]")
if zc == 1:
    if (wdb + wxb) < 0.95 * io:
        sys.exit(f"FATAL: {leg}/{row} armed vehicle closure < 95% of window")
    if fb != 0 or sk != 0:
        sys.exit(f"FATAL: {leg}/{row} fallbacks={fb} slot_skips={sk}")
else:
    if wx or wd or fu:
        sys.exit(f"FATAL: control {leg}/{row} shows zc vehicle engagement")
if fusion == 0 and fu != 0:
    sys.exit(f"FATAL: fusion-off {leg}/{row} shows fused dispatches ({fu})")
# Post-fix laws (opt-in via FP_FIXED=1 in the env: the red repro must
# SEE the bug, the acceptance must gate it).
import os
if os.environ.get("FP_FIXED") == "1" and zc == 1:
    # COUNTER-DOMAIN gates (both sides of each ratio span the same
    # layout+ramp+window population):
    # (a) the exact double-pay: a held op that also extracted = lazy.
    if lazy > 0.05 * max(1, fu + wx):
        sys.exit(f"FATAL: {leg}/{row} lazy (late) extractions {lazy} > 5% of vehicle ops — hold-gate staleness/drift")
    # (b) pure-growth shape: nothing holds/fuses (extractions carry it).
    if klass == "grow" and fu > 0.01 * max(1, wx):
        sys.exit(f"FATAL: {leg}/{row} pure-growth shape fused {fu} ops vs {wx} extractions")
    # (c) W1-eligible shape: the fused/held population direct-consumes;
    # extraction stays the small residue.
    # The eligible-shape extraction RESIDUE is honestly-ineligible ops
    # (a norandommap revisit landing while a prior extraction's extent
    # overlay is still parked on the block makes the block W1-ineligible
    # until the fold — a self-seeding class that scales with the row's
    # IOPS, ~6% at 55k IOPS emulated, ~11% at 157k un-emulated). The
    # EXACT hint-accuracy instrument is `lazy` (gated above at 5%);
    # this bound only catches a gate that broadly refuses the shape.
    if klass == "ow" and wx > 0.25 * max(1, wd):
        sys.exit(f"FATAL: {leg}/{row} eligible shape extracted {wx} vs directs {wd} — the hold gate is refusing eligible ops")
    if klass == "ow" and fusion == 1 and fu < 0.90 * max(1, wd):
        sys.exit(f"FATAL: {leg}/{row} eligible shape fused {fu} of {wd} direct-consumed ops")
    if klass == "seq" and fu > 0.01 * max(1, wx):
        sys.exit(f"FATAL: {leg}/{row} seq row fused {fu} ops")
EOF
  echo "$1/$2: verdict PASS"
}

FIO_COMMON=(--ioengine=libaio --direct=1 --group_reporting --fallocate=none
            --time_based --runtime="${RUNTIME:-60}" --ramp_time="${RAMP:-10}"
            --filename_format='sqzfio.$jobnum.$filenum')

leg_run() { # $1 tag, $2 zc, $3 fusion
  local leg=$1 zc=$2 fusion=$3
  echo "=== $leg (zc=$zc fusion=$fusion) ==="
  do_umount
  do_mount "$zc" "$fusion" "$leg"
  rm -rf "$MNT"/fp_* 2>/dev/null || true
  settle
  smoke "$leg"
  local d
  for row in $ROWS; do
    case "$row" in
      rand4k_field)
        # The FIELD discriminator job shape (tests/fio/exa_randwrite_iops.job):
        # randommap ON (each 4 KiB slot written once per pass — the row
        # stays first-touch/ineligible-majority), fresh fileset.
        d="$MNT/fp_field"; mkdir -p "$d"
        run_row "$leg" rand4k_field "${FIO_COMMON[@]}" --name=rand4k_field --directory="$d" \
          --rw=randwrite --bs=4k --iodepth=8 --numjobs=32 --size=256m
        verdict "$leg" rand4k_field "$zc" "$fusion" hole
        rm -rf "$d"; settle;;
      rand4k_hole)
        d="$MNT/fp_hole"; mkdir -p "$d"
        run_row "$leg" rand4k_hole "${FIO_COMMON[@]}" --name=rand4k_hole --directory="$d" \
          --rw=randwrite --bs=4k --iodepth=8 --numjobs=32 --size=128m --norandommap
        verdict "$leg" rand4k_hole "$zc" "$fusion" hole
        rm -rf "$d"; settle;;
      grow4k)
        d="$MNT/fp_grow"; mkdir -p "$d"
        run_row "$leg" grow4k "${FIO_COMMON[@]}" --name=grow4k --directory="$d" \
          --rw=write --bs=4k --iodepth=8 --numjobs=32 --size=192m
        verdict "$leg" grow4k "$zc" "$fusion" grow
        rm -rf "$d"; settle;;
      rand4kow)
        d="$MNT/fp_ow"; mkdir -p "$d"
        fio --ioengine=libaio --direct=1 --group_reporting --fallocate=none \
            --filename_format='sqzfio.$jobnum.$filenum' --name=prewrite \
            --directory="$d" --rw=write --bs=1M --iodepth=8 --numjobs=32 \
            --size=128m --fsync_on_close=1 --output-format=json \
            --output="$OUT/$leg-rand4kow-prewrite-fio.json" >/dev/null
        sync
        run_row "$leg" rand4kow "${FIO_COMMON[@]}" --name=rand4kow --directory="$d" \
          --rw=randwrite --bs=4k --iodepth=8 --numjobs=32 --size=128m --norandommap \
          --overwrite=1
        verdict "$leg" rand4kow "$zc" "$fusion" ow
        rm -rf "$d"; settle;;
      seqwr)
        d="$MNT/fp_seq"; mkdir -p "$d"
        run_row "$leg" seqwr "${FIO_COMMON[@]}" --name=seqwr --directory="$d" \
          --rw=write --bs=1M --iodepth=8 --numjobs=4 --nrfiles=4 --size=512m
        verdict "$leg" seqwr "$zc" "$fusion" seq
        rm -rf "$d"; settle;;
    esac
  done
  # Bridge tripwires flat per leg.
  cat "$MNT/.stats" > "$OUT/$leg-end.json"
  local c l
  c=$(jget "$OUT/$leg-end.json" fuse3_zc_bridge_cancels)
  l=$(jget "$OUT/$leg-end.json" fuse3_zc_bridge_lost)
  [ "$c" = 0 ] && [ "$l" = 0 ] || fatal "leg $leg: bridge tripwires cancels=$c lost=$l"
  echo "leg $leg: tripwires flat"
}

trap netem_off EXIT
echo "binary: $("$BIN" --version 2>&1 | head -1)"
[ "${NETEM_US}" = 0 ] || netem_on
"$BIN" format "$META" "$DATA" --force >/dev/null || fatal "format failed"

for spec in $LEGS; do
  IFS=: read -r tag zc fusion <<< "$spec"
  leg_run "$tag" "$zc" "$fusion"
done
do_umount
netem_off

echo "ALL LEGS PASSED — artifacts: $OUT"
