#!/usr/bin/env bash
# .benchmarks/rigs/2026-09-03-zc-ahead-yield-field-abba.sh — the
# perf/zc-ahead-yield field A-B-B-A on squeeze-test.
#
# Lever: `pipeline_touch(..., dest_leaseable && !zc_geometry)` — a 1 MiB
# FUSE-zc serve no longer stamps the dest-lease 2 s stream-wide yield, so
# the read-ahead lane / R2 pipeline may fetch the NEXT block into the hold
# (control: `prefetch_issued` = `read_lane_fetches` = 0 on the kern 1 MiB
# row). A = the box's control pair (/scratch/tmp/squeezefs +
# libsqueezefs_il.so, c985fa8c, `release`); B = the rebased branch's rocky8
# `release` pair (same-commit shim, KD-7). Legs A B B A, EACH on a fresh
# cluster_reset_v4 + its own write_BW.job layout pass (24 × 8 GiB); rows per
# leg: read_BW kern, read_BW il (LD_PRELOAD the arm's shim), randread_iops
# kern (the no-regression row); the SUSTAINED legs (B1, A2) add 60 s
# read_BW kern + il. A job-section `runtime=` beats `--runtime` on the
# command line (R-3's lesson), so the 60 s rows are a sed'ed job copy.
#
# Writes ONLY under /scratch/tmp/sqz-agent/. Run as root on the box:
#   bash /scratch/tmp/sqz-agent/2026-09-03-zc-ahead-yield-field-abba.sh
set -eu
D=/scratch/tmp/sqz-agent
OUT="${OUT:-$D/run-$(date +%Y%m%d-%H%M%S)}"
META="sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1"
MNT=/scratch/tmp/test
JOBS=/scratch/tmp/fio_jobs
A_BIN=/scratch/tmp/squeezefs
A_IL=/scratch/tmp/libsqueezefs_il.so
B_BIN=$D/squeezefs.zcay
B_IL=$D/libsqueezefs_il.so.zcay
DELTA=$D/2026-09-03-zc-ahead-yield-row-delta.py
mkdir -p "$OUT"
exec > >(tee -a "$OUT/driver.log") 2>&1
echo "== zc-ahead-yield field A-B-B-A: $(date -u +%FT%TZ) out=$OUT kernel=$(uname -r)"
echo "A: $("$A_BIN" --version)"
echo "B: $("$B_BIN" --version)"

cur_bin=""
reset_cluster() {
  echo "== cluster reset ($1) $(date -u +%T)"
  echo YES | /scratch/tmp/cluster_reset_v4.sh > "$OUT/reset-$1.log" 2>&1 || { tail -5 "$OUT/reset-$1.log"; exit 1; }
  # the reset script leaves nothing mounted (it only echoes the mount line)
  if ps -eo args | grep -q "[s]queezefs mount"; then echo "daemon still up after reset" >&2; exit 2; fi
}

mount_arm() {  # $1 = A|B, $2 = tag
  local arm="$1" tag="$2" bin
  case "$arm" in A) bin=$A_BIN ;; B) bin=$B_BIN ;; esac
  cur_bin=$bin
  "$bin" mount "$META" "$MNT" --daemon --interception --allow-other --log-file "$OUT/mount-$tag.log"
  for _ in $(seq 1 120); do
    [ -r "$MNT/.stats" ] && grep -q '"fuse3_zc_replies"' "$MNT/.stats" && break
    sleep 1
  done
  mkdir -p "$MNT/client_validation"
  python3 - "$MNT/.stats" <<'EOF'
import json,sys
j=json.load(open(sys.argv[1])); m=j["metrics"]
print("   mount:", j.get("build_commit"), j.get("build_profile"), {k:m.get(k) for k in ["fuse3_zc_negotiated","fuse3_kmbuf_negotiated","transport_queues","transport_q_depth","transport_max_write","data_read_lanes","read_lane_armed","read_lane_depth_target","prefetch_window_hwm"]})
EOF
}

umount_arm() {
  "$cur_bin" umount "$MNT" || umount "$MNT" || true
  for _ in $(seq 1 60); do
    ps -eo args | grep -q "[s]queezefs mount" || break
    sleep 1
  done
  sleep 2
}

row() {  # $1 = tag, $2 = job (read_BW|randread_iops|write_BW), $3 = mode (kern|il), $4 = runtime, $5 = il so
  local tag="$1" job="$2" mode="$3" rt="$4" il="${5:-}"
  local jobfile="$JOBS/$job.job"
  if [ "$rt" != 30 ]; then
    sed "s/^runtime=.*/runtime=$rt/" "$jobfile" > "$OUT/$tag.job"
    jobfile="$OUT/$tag.job"
  fi
  local pre=()
  [ "$mode" = il ] && pre=(env LD_PRELOAD="$il")
  echo "-- row $tag: $job $mode ${rt}s (+10 ramp) loadavg=$(cut -d' ' -f1-3 /proc/loadavg) $(date -u +%T)"
  sync; sleep 2
  cat "$MNT/.stats" > "$OUT/$tag.stats0"
  "${pre[@]}" fio "$jobfile" --output-format=json --output="$OUT/$tag.fio.json" \
    --write_bw_log="$OUT/$tag" --log_avg_msec=1000 > "$OUT/$tag.fio.txt" 2>&1 ||
    { echo "fio failed: $(tail -3 "$OUT/$tag.fio.txt")"; return 1; }
  cat "$MNT/.stats" > "$OUT/$tag.stats1"
  python3 "$DELTA" "$OUT" "$tag" | tee "$OUT/$tag.row" || true
  if [ "$mode" = il ]; then
    python3 - "$OUT/$tag.stats0" "$OUT/$tag.stats1" <<'EOF' || { echo "IL ROW INVALID: shim did not engage" >&2; exit 3; }
import json,sys
a=json.load(open(sys.argv[1]))["metrics"]; b=json.load(open(sys.argv[2]))["metrics"]
ops=int(b.get("ipc_ops_read",0))-int(a.get("ipc_ops_read",0))
print("   il engagement: ipc_ops_read delta =", ops)
sys.exit(0 if ops>1000 else 1)
EOF
  fi
}

leg() {  # $1 = A|B, $2 = leg tag, $3 = sustained (0|1)
  local arm="$1" tag="$2" sus="$3"
  reset_cluster "$tag"
  mount_arm "$arm" "$tag"
  local il; case "$arm" in A) il=$A_IL ;; B) il=$B_IL ;; esac
  row "$tag-layout-write" write_BW kern 30 ""
  row "$tag-seq1m-kern" read_BW kern 30 ""
  row "$tag-seq1m-il" read_BW il 30 "$il"
  row "$tag-rr4k-kern" randread_iops kern 30 ""
  if [ "$sus" = 1 ]; then
    row "$tag-seq1m-kern-60" read_BW kern 60 ""
    row "$tag-seq1m-il-60" read_BW il 60 "$il"
  fi
  umount_arm
}

if ps -eo args | grep -q "[s]queezefs mount"; then
  echo "a daemon is already up — refusing (the reset would tear it down)" >&2; exit 2
fi

leg A A1 0
leg B B1 1
leg B B2 0
leg A A2 1
echo "== done $(date -u +%FT%TZ)"
grep -h "^ROW" "$OUT"/*.row
