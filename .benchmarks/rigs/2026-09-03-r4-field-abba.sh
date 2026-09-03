#!/usr/bin/env bash
# .benchmarks/rigs/2026-09-03-r4-field-abba.sh — the R-4 field A-B-B-A on
# squeeze-test.
#
# A = the shipped dev tip (96156b15 — `/scratch/tmp/squeezefs.kvmap` +
#     `libsqueezefs_il.so.kvmap`, the R-3 §9.4 default binary);
# B = R-4 (`squeezefs.r4` + its shim): the worker CPU diet + the adaptive
#     spin-before-park at its derived default;
# C = the SAME R-4 binary with SQUEEZEFS_FUSE_IO_URING_SPIN_US=0 — the
#     diet-only control, so the two levers are separable.
# All rocky8 `release` (thin-LTO) builds; KD-7 same-commit shim pairing per
# arm. One fresh cluster + one write_BW.job pass (arm A) mints the 24 ×
# 8 GiB files; then A1 B1 C1 B2 A2 C2 remounts. Each arm runs kern
# randread_iops (30 s + 10 s ramp), il randread_iops (LD_PRELOAD), kern
# read_BW (1 MiB qd16); B1 and A2 add the 60 s sustained kern + il legs
# (a sed'ed runtime=60 job — the job file's runtime= wins over --runtime);
# B2 and A2 mount with SQUEEZEFS_OP_TRACE=1 and drain `.trace` mid-row in
# their kern rand-4k leg. Every row records the box-wide /proc/stat busy.
#
# Writes ONLY under /scratch/tmp/sqz-agent/r4/. Run as root on the box:
#   bash /scratch/tmp/sqz-agent/r4/2026-09-03-r4-field-abba.sh
set -eu
D=/scratch/tmp/sqz-agent/r4
OUT="${OUT:-$D/abba-$(date +%Y%m%d-%H%M%S)}"
META="${META:-sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1}"
MNT=/scratch/tmp/test
JOBS=/scratch/tmp/fio_jobs
A_BIN=${A_BIN:-/scratch/tmp/squeezefs.kvmap}
A_IL=${A_IL:-/scratch/tmp/libsqueezefs_il.so.kvmap}
B_BIN=${B_BIN:-$D/squeezefs.r4}
B_IL=${B_IL:-$D/libsqueezefs_il.so.r4}
RESET=${RESET:-1}
mkdir -p "$OUT"
JOB60="$D/randread_iops_60.job"
sed "s/^runtime=30$/runtime=60/" "$JOBS/randread_iops.job" > "$JOB60"
grep -q "^runtime=60$" "$JOB60" || { echo "job rewrite failed" >&2; exit 1; }
exec > >(tee -a "$OUT/driver.log") 2>&1
echo "== R-4 field A-B-B-A (A = 96156b15 kvmap, B = R-4 default, C = R-4 spin off): $(date -u +%FT%TZ) out=$OUT kernel=$(uname -r)"
"$A_BIN" --version; "$B_BIN" --version

if ps -eo args | grep -q "[s]queezefs[^ ]* mount"; then
  echo "a daemon is already up — refusing" >&2; exit 2
fi

cur_bin=""
mount_arm() {  # $1 = A|B|C, $2 = tag, $3 = extra env (space-separated K=V, may be empty)
  local arm="$1" tag="$2" extra="${3:-}" bin
  case "$arm" in A) bin=$A_BIN ;; B) bin=$B_BIN ;; C) bin=$B_BIN; extra="$extra SQUEEZEFS_FUSE_IO_URING_SPIN_US=0" ;; esac
  cur_bin=$bin
  local ts; ts=$(date +%s)
  env SQUEEZEFS_IPC_ALLOW_DEV=1 $extra "$bin" mount "$META" "$MNT" --daemon --interception --allow-other \
    --log-file "$OUT/mount-$tag-$ts.log"
  for _ in $(seq 1 90); do
    [ -r "$MNT/.stats" ] && grep -q '"fuse3_zc_replies"' "$MNT/.stats" && break
    sleep 1
  done
  mkdir -p "$MNT/client_validation"
  python3 - "$MNT/.stats" <<'EOF'
import json,sys
m=json.load(open(sys.argv[1]))["metrics"]
print("   mount:", {k:m.get(k) for k in ["fuse3_zc_negotiated","fuse3_kmbuf_negotiated","transport_queues","transport_q_depth","transport_max_write","data_read_lanes","op_trace_armed","transport_spin_window_us"]})
EOF
}

umount_arm() {
  "$cur_bin" umount "$MNT" || umount "$MNT" || true
  for _ in $(seq 1 60); do
    ps -eo args | grep -q "[s]queezefs[^ ]* mount" || break
    sleep 1
  done
  sleep 2
}

drain_trace() {  # $1 = out file
  sync; echo 2 > /proc/sys/vm/drop_caches
  cat "$MNT/.trace" > "$1"
}

row() {  # $1 = tag, $2 = job (randread_iops|randread_iops_60|read_BW|write_BW), $3 = mode (kern|il), $4 = runtime, $5 = il so, $6 = trace (0|1)
  local tag="$1" job="$2" mode="$3" rt="$4" il="${5:-}" trace="${6:-0}"
  local pre=()
  [ "$mode" = il ] && pre=(env LD_PRELOAD="$il" SQUEEZEFS_IPC_ALLOW_DEV=1)
  echo "-- row $tag: $job $mode ${rt}s (+10 ramp) trace=$trace loadavg=$(cut -d' ' -f1-3 /proc/loadavg)"
  [ "$trace" = 1 ] && drain_trace /dev/null
  sync; sleep 1
  cat "$MNT/.stats" > "$OUT/$tag.stats0"
  head -1 /proc/stat > "$OUT/$tag.procstat0"
  local jobfile="$JOBS/$job.job"
  [ "$job" = randread_iops_60 ] && jobfile="$JOB60"
  "${pre[@]}" fio "$jobfile" --output-format=json --output="$OUT/$tag.fio.json" \
    --write_bw_log="$OUT/$tag" --log_avg_msec=1000 > "$OUT/$tag.fio.txt" 2>&1 &
  local fpid=$!
  if [ "$trace" = 1 ]; then
    sleep $((rt / 2 + 10))
    drain_trace "$OUT/$tag.trace.json"
  fi
  wait "$fpid" || { echo "fio failed: $(tail -3 "$OUT/$tag.fio.txt")"; return 1; }
  cat "$MNT/.stats" > "$OUT/$tag.stats1"
  head -1 /proc/stat > "$OUT/$tag.procstat1"
  python3 "$D/2026-09-03-r4-row-delta.py" "$OUT" "$tag" | tee "$OUT/$tag.row" || true
  if [ "$trace" = 1 ]; then
    python3 "$D/op_trace_stitch.py" "$OUT/$tag.trace.json" --stats-pre "$OUT/$tag.stats0" \
      --stats-post "$OUT/$tag.stats1" --ops 0 --json "$OUT/$tag.stitch.json" > "$OUT/$tag.stitch" 2>&1 || true
    grep -A 40 "phase spans" "$OUT/$tag.stitch" | grep "zc_bridge\|read_transport\|read_serve" || true
  fi
}

# ---- fresh cluster + the file set (arm A mints it) ----------------------
if [ "$RESET" = 1 ]; then
  echo "== cluster reset"
  echo YES | /scratch/tmp/cluster_reset_v4.sh > "$OUT/reset.log" 2>&1 || { tail -5 "$OUT/reset.log"; exit 1; }
  mount_arm A prep
  row prep-write write_BW kern 30 "" 0
  umount_arm
fi

# ---- A1 B1 C1 B2 A2 C2 ----------------------------------------------------
mount_arm A A1
row A1-rr4k-kern randread_iops kern 30 "" 0
row A1-rr4k-il randread_iops il 30 "$A_IL" 0
row A1-seq1m-kern read_BW kern 30 "" 0
umount_arm
mount_arm B B1
row B1-rr4k-kern randread_iops kern 30 "" 0
row B1-rr4k-il randread_iops il 30 "$B_IL" 0
row B1-seq1m-kern read_BW kern 30 "" 0
row B1-rr4k-kern-60 randread_iops_60 kern 60 "" 0
row B1-rr4k-il-60 randread_iops_60 il 60 "$B_IL" 0
umount_arm
mount_arm C C1
row C1-rr4k-kern randread_iops kern 30 "" 0
row C1-rr4k-kern-60 randread_iops_60 kern 60 "" 0
umount_arm
mount_arm B B2 "SQUEEZEFS_OP_TRACE=1"
row B2-rr4k-kern-traced randread_iops kern 30 "" 1
row B2-rr4k-il randread_iops il 30 "$B_IL" 0
row B2-seq1m-kern read_BW kern 30 "" 0
umount_arm
mount_arm A A2 "SQUEEZEFS_OP_TRACE=1"
row A2-rr4k-kern-traced randread_iops kern 30 "" 1
row A2-rr4k-il randread_iops il 30 "$A_IL" 0
row A2-seq1m-kern read_BW kern 30 "" 0
row A2-rr4k-kern-60 randread_iops_60 kern 60 "" 0
row A2-rr4k-il-60 randread_iops_60 il 60 "$A_IL" 0
umount_arm
mount_arm C C2
row C2-rr4k-kern randread_iops kern 30 "" 0
umount_arm
echo "== done $(date -u +%FT%TZ)"
grep -h "^ROW" "$OUT"/*.row
