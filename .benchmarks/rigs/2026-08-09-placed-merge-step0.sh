#!/usr/bin/env bash
# Approach A step 0 — the MANDATORY red gate (write-bandwidth program
# adjudication, rc-manifest §3f): confirm the extraction-vehicle term on
# the EXISTING binary before anything is built, and verify the write-lane
# SPREAD precondition (an unspread data_write_lane_submits is a
# regression that STOPS the campaign).
#
# Venue: LOCAL tcp devsub (vehicle-accounting gate — no latency arm
# needed). One armed 1 MiB streaming write row (fio libaio direct=1).
# All stats reads are python over the .stats JSON (NEVER grep — the
# colon-space law).
set -euo pipefail

BIN=${BIN:-/home/justin/Source/.sqz-placedmerge-target/release/squeezefs}
MNT=${MNT:-/run/sqz-placed/mnt}
META="sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1"
DATA="sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1"
OUT=${1:-/run/sqz-placed/step0-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$OUT" "$MNT" /run/sqz-placed/logs

fatal() { echo "FATAL: $*" >&2; exit 1; }

do_umount() {
  if mountpoint -q "$MNT"; then "$BIN" umount "$MNT" || umount "$MNT" || true; fi
  for _ in $(seq 1 150); do pgrep -x squeezefs >/dev/null || break; sleep 2; done
  pgrep -x squeezefs >/dev/null && fatal "daemon did not exit"
  return 0
}

echo "binary: $("$BIN" --version 2>&1 | head -1)"
do_umount
"$BIN" format "$META" "$DATA" --force >/dev/null || fatal "format failed"
ENV_EXTRA=${ENV_EXTRA:-}
env $ENV_EXTRA "$BIN" mount "$META" "$MNT" --daemon --uid 0 --gid 0 \
  --log-file /run/sqz-placed/logs/step0.log
for _ in $(seq 1 30); do
  mountpoint -q "$MNT" && cat "$MNT/.stats" >/dev/null 2>&1 && break
  sleep 2
done
cat "$MNT/.stats" >/dev/null 2>&1 || fatal "mount gate failed"
grep -q "+zero-copy" /run/sqz-placed/logs/step0.log || fatal "zc did not arm"

cat "$MNT/.stats" > "$OUT/before.json"
grep -E " (nvme[5-8]n1) " /proc/diskstats > "$OUT/disk0"

mkdir -p "$MNT/step0"
fio --name=seqwr --directory="$MNT/step0" --filename_format='sqzfio.$jobnum.$filenum' \
    --ioengine=libaio --direct=1 --rw=write --bs=1M --iodepth=8 --numjobs=8 \
    --nrfiles=4 --size=512m --time_based --runtime=45 --ramp_time=0 \
    --group_reporting --fallocate=none --output-format=json \
    --output="$OUT/fio.json" >/dev/null || fatal "fio row failed"

cat "$MNT/.stats" > "$OUT/after.json"
grep -E " (nvme[5-8]n1) " /proc/diskstats > "$OUT/disk1"

python3 - "$OUT" <<'EOF'
import json, sys
out = sys.argv[1]
b = json.load(open(f"{out}/before.json")); bm = b.get("metrics", b)
a = json.load(open(f"{out}/after.json")); am = a.get("metrics", a)
f = json.load(open(f"{out}/fio.json"))
d = lambda k: am.get(k, 0) - bm.get(k, 0)
io = sum(j["write"]["io_bytes"] for j in f["jobs"])
bw = sum(j["write"]["bw_bytes"] for j in f["jobs"])

def mid_us(lbl):
    if lbl == ">16s": return 16e6
    v = lbl[2:]
    if v.endswith("us"): c = float(v[:-2])
    elif v.endswith("ms"): c = float(v[:-2]) * 1e3
    else: c = float(v[:-1]) * 1e6
    return 0.5 if c <= 1 else 0.75 * c

def hist_mean(container_key, phase=None):
    hb, ha = bm.get(container_key), am.get(container_key)
    if phase is not None:
        hb = (hb or {}).get(phase); ha = (ha or {}).get(phase)
    if not isinstance(ha, dict): return "ABSENT"
    if "sum_ns" in ha:  # exact words (e2e audit A, 2026-09-02)
        hb = hb if isinstance(hb, dict) else {}
        n = ha["count"] - hb.get("count", 0)
        w = (ha["sum_ns"] - hb.get("sum_ns", 0)) / 1e3
        return f"{w/n:.1f}us(n={n})" if n else "0"
    n, w = 0, 0.0
    for lbl, av in ha.items():
        if not isinstance(av, (int, float)): continue
        dd = av - ((hb or {}).get(lbl, 0) if isinstance(hb, dict) else 0)
        if dd > 0: n += dd; w += dd * mid_us(lbl)
    return f"{w/n:.1f}us(n={n})" if n else "0"

ext_b = d("fuse3_zc_write_extract_bytes")
dir_b = d("fuse3_zc_write_direct_bytes")
nt_b  = d("nt_copy_bytes")
def dsec(p):
    return sum(int(line.split()[9]) for line in open(p)) * 512
def dwios(p):
    return sum(int(line.split()[7]) for line in open(p))
dev_w = dsec(f"{out}/disk1") - dsec(f"{out}/disk0")
w_ios = dwios(f"{out}/disk1") - dwios(f"{out}/disk0")
amp = dev_w / max(1, io)
wareq_kb = dev_w / 1024 / max(1, w_ios)

print(f"STEP0 row: io={io/1e9:.2f} GB  bw={bw/1e9:.3f} GB/s")
print(f"  extract_bytes = {ext_b}  ({ext_b/max(1,io)*100:.1f}% of user bytes)")
print(f"  direct_bytes  = {dir_b}  ({dir_b/max(1,io)*100:.1f}% of user bytes)")
print(f"  nt_copy_bytes = {nt_b}  ({nt_b/max(1,io)*100:.1f}% of user bytes)")
print(f"  placed: severs={d('ipc_placed_severs')} adoptions={d('placed_adoptions')} elides={d('placed_merge_elides')}")
print(f"  write_pipeline_phase_ns.dma mean = {hist_mean('write_pipeline_phase_ns','dma')}")
print(f"  write_pipeline_phase_ns.total mean = {hist_mean('write_pipeline_phase_ns','total')}")
print(f"  write_transport_phase_ns.transport_total mean = {hist_mean('write_transport_phase_ns','transport_total')}")
print(f"  write_transport_phase_ns.queue_wait mean = {hist_mean('write_transport_phase_ns','queue_wait')}")
print(f"  amplification = {amp:.3f}  wareq-sz = {wareq_kb:.0f} KiB")
print(f"  data_write_lanes = {am.get('data_write_lanes')}")
lanes = am.get("data_write_lane_submits", [])
spread_fail = False
for entry in lanes if isinstance(lanes, list) else []:
    dev, _, subs = entry.partition("=")
    vals = [int(x) for x in subs.split(",") if x]
    moving = sum(1 for v in vals if v > 0)
    tot = sum(vals)
    mx = max(vals) if vals else 0
    print(f"  lane_submits {dev}: lanes={len(vals)} moving={moving} total={tot} max_share={mx/max(1,tot)*100:.0f}%")
    if tot > 1000 and (moving < max(2, len(vals) // 2) or mx > 0.9 * tot):
        spread_fail = True

fails = []
if ext_b < 0.90 * io: fails.append(f"extract_bytes {ext_b} !~ user bytes {io} (term absent?)")
if dir_b > 0.05 * io: fails.append(f"direct_bytes {dir_b} unexpectedly high")
if nt_b < 0.90 * io: fails.append(f"nt_copy_bytes {nt_b} !~ user bytes {io}")
if amp > 1.05: fails.append(f"amplification {amp:.3f} > 1.05")
if wareq_kb < 256: fails.append(f"wareq-sz {wareq_kb:.0f} KiB collapsed")
if spread_fail: fails.append("WRITE LANES NOT SPREAD — the adjudication's STOP precondition")
if fails:
    print("STEP0 VERDICT: " + ("CAMPAIGN-STOP (lane spread)" if spread_fail else "TERM NOT CONFIRMED"))
    for x in fails: print("  FAIL: " + x)
    sys.exit(1)
print("STEP0 VERDICT: TERM CONFIRMED — extraction destination precedes the accumulation destination; lanes spread")
EOF
RC=$?
rm -rf "$MNT/step0"
do_umount
exit $RC
