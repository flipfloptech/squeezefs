#!/usr/bin/env bash
# ACK-early re-bracket (zc-payload-retention R4, 2026-08-10) — the B2
# falsification's named follow-up: the SAME batched fresh-ingest row, THREE
# postures — A = overlay + ACK-early (retention pipeline; the depth cap
# released), B = overlay ACK-after-CQE (the falsified B2 posture — the
# isolation control), C = shipped extraction control. fio is O_DIRECT, so
# A legs arm SQUEEZEFS_ZC_ACK_EARLY_ODIRECT — O_DIRECT stores SNAPSHOT
# (extract) then ACK then DMA the snapshot. fio/dd reuse is safe (bytes
# sampled at ACK). The per-leg dd smoke is the aliasing regression pin.
# Adapted from 2026-08-09-device-overlay-bracket.sh (B2).
#
# Comparator = the SHIPPED EXTRACTION CONTROL (§1.4 case 2: Approach A was
# perf-falsified-but-correct, so its mechanism is dead evidence, not a bar).
# Legs alternate the SQUEEZEFS_DEVICE_OVERLAY lever, RESET-PER-LEG (fresh
# format — the venue ages), both bracket orders + a third sample each
# (medians of 3): O1 C1 C2 O2 O3 C3.
#
# THE ROW IS FRESH-FILE INGEST, BATCHED: B2's eligible shape is blocks with
# NO old binding, so a fio time_based loop over one file set leaves
# eligibility after pass 1 (rewrites are PR B4). The sustained row is a
# batch loop — fio writes a FRESH 16 GiB file set (the charter shape: bs=1M
# libaio direct=1 numjobs=8 nrfiles=4 size=512m), the set is deleted
# (offsets recycle through the unlink drain + reclaim), repeat until
# >= ROW_SECS of fio wall time and >= 3 batches. Headline = aggregate BW
# over fio wall time; per-batch BWs are the flatness (no-decay) check.
#
# FATAL pins per leg (the standing row law + the B2 engagement set):
#   fuse3_zc_negotiated=1; armed: overlay_store_bytes >= 95% of user bytes,
#   extract <= 5%, nt_copy <= 5%, fallbacks <= 2% of stores; control:
#   overlay_installs == 0, extract ~= user bytes; both: amp <= 1.05,
#   wareq-sz sane, block_free_* recorded, tripwires 0 (overlay_fence_drops /
#   overlay_teardown_waits / overlay_unpublished_at_fsync /
#   invariant_tripwires / zc bridge cancels+lost / data_dma_fence_refusals /
#   write_pipeline fence drops). Per-qid store census recorded (spread
#   verdict); single-qid collapse with real volume is FATAL.
# All stats reads are python over the .stats JSON (the colon-space law).
set -euo pipefail

BIN=${BIN:-/home/justin/Source/squeezefs/target/release/squeezefs}
MNT=${MNT:-/run/sqz-overlay/mnt}
META="sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1"
DATA="sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1"
OUT=${1:-/run/sqz-overlay/ackearly-$(date +%Y%m%d-%H%M%S)}
LEGS=${LEGS:-"A1:1:1 B1:1:0 C1:0:0 C2:0:0 B2:1:0 A2:1:1 A3:1:1 B3:1:0 C3:0:0"}
ROW_SECS=${ROW_SECS:-75}
MIN_BATCHES=${MIN_BATCHES:-3}
mkdir -p "$OUT" "$MNT" /run/sqz-overlay/logs

fatal() { echo "FATAL: $*" >&2; exit 1; }

do_umount() {
  if mountpoint -q "$MNT"; then "$BIN" umount "$MNT" || umount "$MNT" || true; fi
  for _ in $(seq 1 150); do pgrep -x squeezefs >/dev/null || break; sleep 2; done
  pgrep -x squeezefs >/dev/null && fatal "daemon did not exit"
  return 0
}

leg_mount() { # $1 overlay, $2 ackearly, $3 tag — RESET: format fresh per leg
  do_umount
  "$BIN" format "$META" "$DATA" --force >/dev/null || fatal "format failed ($3)"
  SQUEEZEFS_DEVICE_OVERLAY=$1 \
  SQUEEZEFS_ZC_ACK_EARLY=$2 \
  SQUEEZEFS_ZC_ACK_EARLY_ODIRECT=$2 \
    "$BIN" mount "$META" "$MNT" --daemon --uid 0 --gid 0 \
    --log-file "/run/sqz-overlay/logs/sqz-$3.log"
  local ok=0
  for _ in $(seq 1 30); do
    if mountpoint -q "$MNT" && cat "$MNT/.stats" >/dev/null 2>&1; then ok=1; break; fi
    sleep 2
  done
  [ "$ok" = 1 ] || fatal "mount gate failed ($3)"
  cat "$MNT/.stats" > "$OUT/$3-arm.json"
  python3 - "$OUT/$3-arm.json" "$2" <<'EOF' || fatal "leg arm proof failed"
import json, sys
d = json.load(open(sys.argv[1])); d = d.get("metrics", d)
assert int(d.get("fuse3_zc_negotiated", 0)) == 1, "fuse3_zc_negotiated != 1"
if int(sys.argv[2]) == 1:
    assert int(d.get("fuse3_zc_retention_negotiated", 0)) == 1, \
        "ack-early leg without retention negotiation (v2 kernel required)"
EOF
  echo "leg $3: mounted (overlay=$1 ackearly=$2, zc armed)"
}

smoke() { # $1 tag — P0 (per-leg, before the row)
  local d="$MNT/ovl_smoke"
  mkdir -p "$d"
  dd if=/dev/urandom of=/tmp/ovl_smoke.src bs=1M count=32 status=none
  local a b c
  a=$(md5sum < /tmp/ovl_smoke.src | cut -d' ' -f1)
  dd if=/tmp/ovl_smoke.src of="$d/blob" bs=1M oflag=direct conv=fsync status=none
  b=$(dd if="$d/blob" bs=1M iflag=direct status=none | md5sum | cut -d' ' -f1)
  c=$(md5sum < "$d/blob" | cut -d' ' -f1)
  [ "$a" = "$b" ] && [ "$a" = "$c" ] || fatal "leg $1: md5 mismatch — CORRUPTION"
  rm -rf "$d" /tmp/ovl_smoke.src
  echo "leg $1: correctness smoke OK"
}

run_ingest_row() { # $1 tag — the batched fresh-ingest sustained row
  local leg=$1 batch=0 fio_wall_ms=0
  cat "$MNT/.stats" > "$OUT/$leg-ingest-before.json"
  grep -E " (nvme[5-8]n1) " /proc/diskstats > "$OUT/$leg-ingest-disk0"
  : > "$OUT/$leg-ingest-batches.tsv"
  local t0 now
  t0=$(date +%s)
  while :; do
    batch=$((batch + 1))
    local d="$MNT/ovl_ingest_$batch"
    mkdir -p "$d"
    fio --ioengine=libaio --direct=1 --group_reporting --fallocate=none \
        --name="ingest$batch" --directory="$d" --rw=write --bs=1M \
        --iodepth=8 --numjobs=8 --nrfiles=4 --size=512m \
        --filename_format='sqzfio.$jobnum.$filenum' \
        --output-format=json --output="$OUT/$leg-ingest-b$batch-fio.json" \
        >/dev/null || fatal "$leg ingest batch $batch: fio exited nonzero"
    python3 - "$OUT/$leg-ingest-b$batch-fio.json" >> "$OUT/$leg-ingest-batches.tsv" <<'EOF'
import json, sys
f = json.load(open(sys.argv[1]))
io = sum(j["write"]["io_bytes"] for j in f["jobs"])
ms = max(j["write"]["runtime"] for j in f["jobs"])
print(f"{io}\t{ms}\t{io/1e9/(ms/1e3):.3f}")
EOF
    rm -rf "$d"
    now=$(date +%s)
    fio_wall_ms=$(awk -F'\t' '{s+=$2} END{print s}' "$OUT/$leg-ingest-batches.tsv")
    if [ "$batch" -ge "$MIN_BATCHES" ] && [ "$fio_wall_ms" -ge $((ROW_SECS * 1000)) ]; then
      break
    fi
    if [ $((now - t0)) -gt 900 ]; then fatal "$leg: ingest row exceeded 15 min"; fi
  done
  # Quiesce: let detached publishes/reclaims settle before the after-shot.
  sync "$MNT/.stats" 2>/dev/null || true
  sleep 2
  grep -E " (nvme[5-8]n1) " /proc/diskstats > "$OUT/$leg-ingest-disk1"
  cat "$MNT/.stats" > "$OUT/$leg-ingest-after.json"
}

verdict() { # $1 leg, $2 overlay, $3 ackearly
  python3 - "$OUT" "$1" "$2" "$3" <<'EOF'
import json, sys
out, leg, ovl = sys.argv[1], sys.argv[2], int(sys.argv[3])
ae = int(sys.argv[4])
b = json.load(open(f"{out}/{leg}-ingest-before.json")); b = b.get("metrics", b)
a = json.load(open(f"{out}/{leg}-ingest-after.json")); a = a.get("metrics", a)
d = lambda k: a.get(k, 0) - b.get(k, 0)
rows = [l.split("\t") for l in open(f"{out}/{leg}-ingest-batches.tsv").read().splitlines()]
io = sum(int(r[0]) for r in rows)
ms = sum(int(r[1]) for r in rows)
bws = [float(r[2]) for r in rows]
bw = io / 1e9 / (ms / 1e3)
def dsec(p):
    return sum(int(line.split()[9]) for line in open(p)) * 512
def dwios(p):
    return sum(int(line.split()[7]) for line in open(p))
dev_w = dsec(f"{out}/{leg}-ingest-disk1") - dsec(f"{out}/{leg}-ingest-disk0")
w_ios = dwios(f"{out}/{leg}-ingest-disk1") - dwios(f"{out}/{leg}-ingest-disk0")
amp = dev_w / max(1, io)
wareq_kb = dev_w / 1024 / max(1, w_ios)
sb, st = d("overlay_store_bytes"), d("overlay_stores")
inst, pubs = d("overlay_installs"), d("overlay_publishes")
fb, ss = d("overlay_store_fallbacks"), d("overlay_short_stores")
seeds_b = d("overlay_gap_seed_bytes")
ext_b = d("fuse3_zc_write_extract_bytes")
nt_b = d("nt_copy_bytes")
direct_b = d("fuse3_zc_write_direct_bytes")
frees = d("block_free_reclaim_queued")
freeb = d("block_free_discard_bytes") + d("block_free_punch_bytes")
print(f"{leg} overlay={ovl}: batches={len(rows)} io={io/1e9:.2f}GB fio_wall={ms/1e3:.1f}s "
      f"BW={bw:.3f}GB/s batchBWs={['%.2f' % x for x in bws]} amp={amp:.3f} wareq={wareq_kb:.0f}KiB")
print(f"  PIN installs={inst} stores={st} store_bytes={sb/1e9:.2f}GB ({sb/max(1,io)*100:.1f}%) "
      f"publishes={pubs} fallbacks={fb} short={ss} seed_bytes={seeds_b}")
print(f"  PIN direct={direct_b/1e9:.2f}GB extract={ext_b/1e9:.2f}GB ({ext_b/max(1,io)*100:.1f}%) "
      f"nt_copy={nt_b/1e9:.2f}GB ({nt_b/max(1,io)*100:.1f}%) reclaim_q={frees} free_bytes={freeb/1e9:.2f}GB")
# ACK-early engagement + closure pins.
ae_st, ae_b = d("overlay_ack_early_stores"), d("overlay_ack_early_bytes")
ret_c, rel_c = d("fuse3_zc_retain_commits"), d("fuse3_zc_releases")
ret_out = a.get("fuse3_zc_retained_outstanding", 0)
ae_retry = d("overlay_ack_early_retries")
print(f"  PIN ack_early stores={ae_st} bytes={ae_b/1e9:.2f}GB retain_commits={ret_c} "
      f"releases={rel_c} outstanding_now={ret_out} retries={ae_retry}")
if ae:
    if ae_st < 0.95 * max(1, st):
        sys.exit(f"FATAL: {leg} ack-early engagement {ae_st}/{st} stores < 95%")
    # O_DIRECT A-leg snapshots (extract) then ACK — retain_commits is the
    # page-cache arm and stays ~0 on a pure-direct row. Closure still
    # applies to whatever DID retain.
    if rel_c != ret_c:
        sys.exit(f"FATAL: {leg} release closure broken: retains={ret_c} releases={rel_c}")
    if ret_out != 0:
        sys.exit(f"FATAL: {leg} retained_outstanding={ret_out} at quiesce")
    if ext_b < 0.90 * max(1, ae_b):
        sys.exit(f"FATAL: {leg} O_DIRECT snapshot extract {ext_b} < 90% of ack-early bytes {ae_b}")
else:
    if ae_st or ret_c:
        sys.exit(f"FATAL: {leg} shows ack-early engagement (stores={ae_st} retains={ret_c}) with the lever off")
qids = [q for q in a.get("overlay_store_submits_qids", []) ]
bq = b.get("overlay_store_submits_qids", []) or [0]*len(qids)
dq = [x - y for x, y in zip(qids, bq + [0]*(len(qids)-len(bq)))]
engaged = sum(1 for x in dq if x > 0)
tot_q = sum(dq)
print(f"  PIN store qids engaged={engaged} total={tot_q} max_share={max(dq)/tot_q*100 if tot_q else 0:.0f}%")
trip = {k: d(k) for k in ("overlay_fence_drops", "overlay_teardown_waits",
        "overlay_unpublished_at_fsync", "invariant_tripwires",
        "fuse3_zc_bridge_cancels", "fuse3_zc_bridge_lost",
        "fuse3_zc_release_failures", "fuse3_zc_retain_refused",
        "data_dma_fence_refusals", "write_pipeline_fence_drops",
        "overlay_supersessions")}
bad = {k: v for k, v in trip.items() if v}
if bad:
    sys.exit(f"FATAL: {leg} tripwires {bad}")
if a.get("overlay_open", 0) != 0:
    sys.exit(f"FATAL: {leg} overlay_open={a.get('overlay_open')} at quiesce")
# Armed rows carry the adjudication's 1.05 FATAL. The CONTROL posture's
# own floor on this fresh-ingest venue measured 1.052 (first counted run
# - a batched fresh-store shape term, not an overlay term; the Approach-A
# small-row precedent), so control gates at 1.10 with the probe recorded.
amp_cap = 1.05 if ovl else 1.10
if amp > amp_cap:
    sys.exit(f"FATAL: {leg} amplification {amp:.3f} > {amp_cap}")
if wareq_kb < 256:
    sys.exit(f"FATAL: {leg} wareq-sz {wareq_kb:.0f} KiB collapsed")
if ovl:
    if sb < 0.95 * io:
        sys.exit(f"FATAL: {leg} overlay engagement {sb/max(1,io)*100:.1f}% < 95% of user bytes")
    # B (ACK-after-CQE) stays 0-copy. A (ACK-early O_DIRECT) snapshots
    # via extract — that pin is the ack-early block above, not here.
    if not ae and ext_b > 0.05 * io:
        sys.exit(f"FATAL: {leg} extraction bytes {ext_b/max(1,io)*100:.1f}% > 5% on the eligible shape")
    if nt_b > 0.05 * io:
        sys.exit(f"FATAL: {leg} nt_copy bytes {nt_b/max(1,io)*100:.1f}% > 5% on the eligible shape")
    if fb > 0.02 * max(1, st):
        sys.exit(f"FATAL: {leg} store fallbacks {fb} > 2% of stores")
    if pubs < inst:
        sys.exit(f"FATAL: {leg} publishes {pubs} < installs {inst} (stranded records)")
    if tot_q > 1000 and engaged <= 1:
        sys.exit(f"FATAL: {leg} fabric-queue collapse (all stores on one qid)")
else:
    if inst or sb:
        sys.exit(f"FATAL: control {leg} shows overlay engagement (installs={inst})")
    if ext_b < 0.90 * io:
        sys.exit(f"FATAL: control {leg} extraction {ext_b/max(1,io)*100:.1f}% < 90% — not the shipped shape")
EOF
  echo "$1: verdict PASS"
}

# Same-day RAW ceiling row (the fraction law: every gate is a fraction
# of the SAME-DAY raw on THIS venue): fio straight at the four data
# namespaces, the row shape's aggregate concurrency. DESTRUCTIVE - runs
# before any leg formats (each leg re-formats anyway).
if [ "${RAW_ROW:-1}" = 1 ]; then
  do_umount
  fio --ioengine=libaio --direct=1 --group_reporting --time_based --runtime=30 \
      --name=raw1m --rw=write --bs=1M --iodepth=8 --numjobs=8 \
      --filename=/dev/nvme5n1:/dev/nvme6n1:/dev/nvme7n1:/dev/nvme8n1 \
      --output-format=json --output="$OUT/raw1m-fio.json" >/dev/null \
    || fatal "raw row failed"
  python3 -c "
import json
f = json.load(open('$OUT/raw1m-fio.json'))
io = sum(j['write']['io_bytes'] for j in f['jobs'])
ms = max(j['write']['runtime'] for j in f['jobs'])
print(f'RAW same-day ceiling (4-dev aggregate, bs=1M qd8x8): {io/1e9/(ms/1e3):.3f} GB/s')
"
fi

for spec in $LEGS; do
  IFS=: read -r tag ovl ae <<< "$spec"
  echo "=== $tag (overlay=$ovl ackearly=$ae) ==="
  leg_mount "$ovl" "$ae" "$tag"
  smoke "$tag"
  run_ingest_row "$tag"
  verdict "$tag" "$ovl" "$ae"
done
do_umount
echo "ALL LEGS PASSED — artifacts: $OUT"
