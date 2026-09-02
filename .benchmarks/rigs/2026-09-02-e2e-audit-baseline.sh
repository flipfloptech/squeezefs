#!/usr/bin/env bash
# End-to-end performance audit — BASELINE matrix on squeeze-test.
# Every row: stats-inode snapshot → fio (JSON + bw log) → snapshot →
# client-side diskstats delta (the nvme-tcp namespaces ARE local block
# devices here, so device bytes ÷ user bytes is exact per row).
# Modes: kernel FUSE, then LD_PRELOAD interception (same-commit pair).
set -u
TS=$(date +%Y%m%d-%H%M%S)
OUT=/scratch/tmp/e2e-baseline-$TS
MNT=/scratch/tmp/test
SQZ=/scratch/tmp/squeezefs.kvmap
SHIM=/scratch/tmp/libsqueezefs_il.so.kvmap
JOBS=/scratch/tmp/fio_jobs
META="sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1"
LOG=/scratch/tmp/logs/sqz-e2e-$TS.log
mkdir -p "$OUT"
echo "artifacts: $OUT"

snap() { cat "$MNT/.stats" > "$OUT/$1.json" 2>/dev/null; grep -E ' nvme[0-9]+n1 ' /proc/diskstats > "$OUT/$1.diskstats"; date +%s.%N > "$OUT/$1.ts"; }

# row <name> <mode:kern|il> <fio args...>
row() {
  local name=$1 mode=$2; shift 2
  echo "=== ROW $name ($mode) $(date +%T)"
  snap "$name.pre"
  local pre=""
  [ "$mode" = il ] && pre="env LD_PRELOAD=$SHIM"
  $pre fio "$@" --output-format=json --output="$OUT/$name.fio.json" \
      --write_bw_log="$OUT/$name" --log_avg_msec=1000 >/dev/null 2>&1
  echo "fio rc=$?"
  snap "$name.post"
  python3 - "$OUT/$name.fio.json" <<'EOF' 2>/dev/null
import json,sys
d=json.load(open(sys.argv[1]))
for j in d.get("jobs",[]):
    for k in ("read","write"):
        v=j.get(k,{})
        if v.get("io_bytes",0):
            bw=v["bw_bytes"]/2**30; iops=v["iops"]
            lat=v.get("clat_ns",{}); p=lat.get("percentile",{})
            print(f"  {k}: {bw:.2f} GiB/s  {iops:,.0f} IOPS  clat mean {lat.get('mean',0)/1e6:.2f} ms  p99 {p.get('99.000000',0)/1e6:.2f} ms")
EOF
}

# ---- session ----
pkill -x squeezefs 2>/dev/null; sleep 2; pkill -9 -x squeezefs 2>/dev/null
umount -l "$MNT" 2>/dev/null
echo YES | /scratch/tmp/cluster_reset_v4.sh >/dev/null 2>&1 || { echo "RESET FAILED"; exit 1; }
$SQZ mount "$META" "$MNT" --daemon --interception --allow-other --log-file "$LOG" 2>&1 | tail -1
sleep 3
mkdir -p "$MNT/client_validation"
$SQZ --version | head -1 > "$OUT/binary.txt"
cp "$JOBS"/*.job "$OUT/" 2>/dev/null

for mode in kern il; do
  # Fresh write (creates the 24x8g set), then cold/warm sequential reads.
  row "${mode}.w_fresh"   $mode "$JOBS/write_BW.job"
  row "${mode}.r_cold"    $mode "$JOBS/read_BW.job"
  row "${mode}.r_repeat"  $mode "$JOBS/read_BW.job"
  row "${mode}.rr_4k"     $mode "$JOBS/randread_iops.job"
  # Rewrite face (same files), then random 4k writes.
  row "${mode}.w_rewrite" $mode "$JOBS/write_BW.job"
  row "${mode}.rw_4k"     $mode "$JOBS/randwrite_iops.job"
  # Mixed.
  mwbwmixread=70 row "${mode}.mix_bw"   $mode "$JOBS/mixed_workload_BW.job"
  mwiopsmixread=70 row "${mode}.mix_4k" $mode "$JOBS/mixed_workload_rand_iops.job"
  # Durable rewrite (fsync-inclusive) — the RW6 durability-leveled row.
  row "${mode}.w_durable" $mode "$JOBS/write_BW.job" --end_fsync=1
  # File creation (metadata plane).
  rm -f "$MNT"/client_validation/test.*.root
  row "${mode}.f_create"  $mode "$JOBS/file_creation.job"
  rm -f "$MNT"/client_validation/test.*.root
done

snap "final"
echo "=== kvmap engagement + tripwires:"
python3 - "$OUT/final.json" <<'EOF'
import json,sys
d=json.load(open(sys.argv[1])); d=d.get("metrics",d)
for k in ("map_migrate_inos","kvmap_partial_inos","meta_kv_block_refs_drift","invariant_tripwires",
          "transport_lease_overlong","fuse_op_watchdog_overdue","write_pipeline_fence_drops",
          "ipc_ops_read","ipc_ops_write","overlay_ack_early_stores","patch_writes","write_through_blocks"):
    print(f"  {k}: {d.get(k)}")
EOF
grep -cE "ERROR|divergent|corrupt" "$LOG"
echo "DONE $OUT"
