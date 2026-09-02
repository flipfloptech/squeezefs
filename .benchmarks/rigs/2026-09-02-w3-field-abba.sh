#!/usr/bin/env bash
# W-3 field A-B-B-A on squeeze-test: the overlay depth governor
# (SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR=1, the shipped default) vs the pre-W-3
# open-loop overlay store (=0) on the w_rewrite row (seq 1 MiB O_DIRECT
# overwrite of 24 x 8 GiB kvmap files, fio libaio qd16 x24, 30 s + 10 s
# ramp — the field job /scratch/tmp/fio_jobs/write_BW.job re-run over
# already-written files). One binary, one knob, four legs; the cluster is
# freshly reset before the fresh pre-pass that mints the files.
#
# Usage (on the box): SQZ=/scratch/tmp/sqz-agent/w3/squeezefs.w3 \
#   bash 2026-09-02-w3-field-abba.sh
# Writes ONLY under /scratch/tmp/sqz-agent/w3/.
set -eu
SQZ="${SQZ:-/scratch/tmp/sqz-agent/w3/squeezefs.w3}"
OUT="${OUT:-/scratch/tmp/sqz-agent/w3/run-$(date +%Y%m%d-%H%M%S)}"
META="${META:-sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1}"
MNT=/scratch/tmp/test
JOB=/scratch/tmp/fio_jobs/write_BW.job
RUNTIME="${RUNTIME:-30}"
mkdir -p "$OUT"

if ps -eo args | grep -q "[s]queezefs mount"; then
  echo "a daemon is already up — refusing" >&2; exit 2
fi

data_devs() {
  # The data namespaces = every 48G Linux nvme namespace on the box.
  lsblk -d -n -o NAME,SIZE | awk '$2=="48G"{print $1}'
}

diskstats_written_bytes() {
  local sum=0 d
  for d in $(data_devs); do
    # field 10 = sectors written (512 B)
    local s; s=$(awk -v d="$d" '$3==d{print $10}' /proc/diskstats)
    sum=$((sum + s * 512))
  done
  echo "$sum"
}

mount_leg() {  # $1 = governor value (1|0), $2 = leg tag
  local ts; ts=$(date +%s)
  SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR="$1" sudo -n --preserve-env=SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR \
    "$SQZ" mount "$META" "$MNT" --daemon --allow-other \
    --log-file "/scratch/tmp/sqz-agent/w3/sqz-w3-$2-$ts.log"
  for _ in $(seq 1 60); do
    [ -r "$MNT/.stats" ] && grep -q '"overlay_governed_stores"' "$MNT/.stats" && break
    sleep 1
  done
  mkdir -p "$MNT/client_validation"
}

umount_leg() {
  sudo -n "$SQZ" umount "$MNT" || sudo -n umount "$MNT"
  for _ in $(seq 1 60); do
    ps -eo args | grep -q "[s]queezefs mount" || break
    sleep 1
  done
}

run_row() {  # $1 = leg tag
  local tag="$1"
  cat "$MNT/.stats" > "$OUT/$tag.pre.json"
  local d0; d0=$(diskstats_written_bytes)
  fio "$JOB" --runtime="$RUNTIME" --output-format=json+ --output="$OUT/$tag.fio.json" \
    --write_bw_log="$OUT/$tag" --log_avg_msec=1000 > "$OUT/$tag.fio.txt" 2>&1
  local d1; d1=$(diskstats_written_bytes)
  cat "$MNT/.stats" > "$OUT/$tag.post.json"
  echo "$((d1 - d0))" > "$OUT/$tag.devbytes"
  python3 - "$OUT" "$tag" <<'EOF'
import json, sys
out, tag = sys.argv[1], sys.argv[2]
f = json.load(open(f"{out}/{tag}.fio.json"))
j = f["jobs"][0]["write"]
user = j["io_bytes"]
bw = j["bw_bytes"] / 2**30
clat = j["clat_ns"]["mean"] / 1e6
p99 = j["clat_ns"]["percentile"].get("99.000000", 0) / 1e6
dev = int(open(f"{out}/{tag}.devbytes").read())
pre = json.load(open(f"{out}/{tag}.pre.json"))
post = json.load(open(f"{out}/{tag}.post.json"))
def d(k):
    a = pre.get(k, 0); b = post.get(k, 0)
    try: return b - a
    except Exception: return f"{a}->{b}"
keys = ["overlay_governed_stores", "overlay_stores", "overlay_ack_early_stores",
        "overlay_overwrite_installs", "write_pipeline_admission_waits",
        "write_pipeline_depth_probe_ups", "write_pipeline_depth_probe_backoffs",
        "rewrite_blocks", "write_through_blocks", "invariant_tripwires",
        "write_pipeline_fence_drops", "overlay_fence_drops"]
row = {k: d(k) for k in keys}
row["depth_target_post"] = post.get("write_pipeline_depth_target")
row["depth_target_base_post"] = post.get("write_pipeline_depth_target_base")
print(f"ROW {tag}: {bw:.2f} GiB/s clat {clat:.2f} ms p99 {p99:.1f} ms  dev/user {dev/user if user else 0:.4f}  {json.dumps(row)}")
EOF
}

echo "== fresh pre-pass (mints the 24 files; label-only) =="
mount_leg 1 fresh
run_row fresh
umount_leg

echo "== A1: governor ON =="
mount_leg 1 A1; run_row A1; umount_leg
echo "== B1: governor OFF (pre-W-3 open-loop) =="
mount_leg 0 B1; run_row B1; umount_leg
echo "== B2: governor OFF =="
mount_leg 0 B2; run_row B2; umount_leg
echo "== A2: governor ON =="
mount_leg 1 A2; run_row A2; umount_leg
echo "== done: $OUT =="
grep -h "^ROW" "$OUT"/*.fio.txt 2>/dev/null || true
