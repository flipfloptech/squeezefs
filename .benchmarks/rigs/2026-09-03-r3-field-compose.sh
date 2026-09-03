#!/usr/bin/env bash
# .benchmarks/rigs/2026-09-03-r3-field-compose.sh — the COMPOSITION field
# A-B-B-A on squeeze-test (R-2 ⊕ R-3 vs R-2 alone).
#
# A = the R-2-only dev tip (2b011d91: fast dispatch + lane homing + D-2)
#     rocky8 build `squeezefs.r2` + its shim;
# B = the composed R-3 tip (55a7bdbb: R-2 ⊕ READ fusion + the funnel wake
#     + the zc-leg instrument) rocky8 build `squeezefs.r3c` + its shim.
# Both clean, same `release` profile; KD-7 same-commit shim pairing, so no
# dev override is needed (kept harmless). One fresh cluster + one
# write_BW.job pass (arm A) mints the 24 × 8 GiB files; then A B B A
# remounts, each running kern randread_iops (30 s + 10 s ramp), il
# randread_iops (LD_PRELOAD), kern read_BW (1 MiB qd16); the second B and
# the last A mount with SQUEEZEFS_OP_TRACE=1 and drain `.trace` mid-row in
# their kern rand-4k leg (the compounding re-attribution). The 60 s
# sustained legs run a sed'ed `runtime=60` job on B1 and A2 (the job
# file's runtime= wins over --runtime).
#
# Writes ONLY under /scratch/tmp/sqz-agent/r3/. Run as root on the box:
#   bash /scratch/tmp/sqz-agent/r3/2026-09-03-r3-field-compose.sh
set -eu
D=/scratch/tmp/sqz-agent/r3
OUT="${OUT:-$D/run-$(date +%Y%m%d-%H%M%S)}"
META="${META:-sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1}"
MNT=/scratch/tmp/test
JOBS=/scratch/tmp/fio_jobs
A_BIN=${A_BIN:-$D/squeezefs.r2}
A_IL=${A_IL:-$D/libsqueezefs_il.so.r2}
B_BIN=${B_BIN:-$D/squeezefs.r3c}
B_IL=${B_IL:-$D/libsqueezefs_il.so.r3c}
mkdir -p "$OUT"
JOB60="$D/randread_iops_60.job"
sed "s/^runtime=30$/runtime=60/" "$JOBS/randread_iops.job" > "$JOB60"
grep -q "^runtime=60$" "$JOB60" || { echo "job rewrite failed" >&2; exit 1; }
exec > >(tee -a "$OUT/driver.log") 2>&1
echo "== R-3 COMPOSITION field A-B-B-A (A = R-2 dev tip, B = R-2 ⊕ R-3): $(date -u +%FT%TZ) out=$OUT kernel=$(uname -r)"
"$A_BIN" --version; "$B_BIN" --version

if ps -eo args | grep -q "[s]queezefs mount"; then
  echo "a daemon is already up — refusing" >&2; exit 2
fi

cur_bin=""
mount_arm() {  # $1 = A|B, $2 = tag, $3 = extra env (space-separated K=V, may be empty)
  local arm="$1" tag="$2" extra="${3:-}" bin
  case "$arm" in A) bin=$A_BIN ;; B) bin=$B_BIN ;; esac
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
print("   mount:", {k:m.get(k) for k in ["fuse3_zc_negotiated","fuse3_kmbuf_negotiated","transport_queues","transport_q_depth","transport_max_write","data_read_lanes","op_trace_armed","op_trace_divisor"]})
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

drain_trace() {  # $1 = out file
  sync; echo 2 > /proc/sys/vm/drop_caches
  cat "$MNT/.trace" > "$1"
}

row() {  # $1 = tag, $2 = job (randread_iops|read_BW|write_BW), $3 = mode (kern|il), $4 = runtime, $5 = il so, $6 = trace (0|1)
  local tag="$1" job="$2" mode="$3" rt="$4" il="${5:-}" trace="${6:-0}"
  local pre=()
  [ "$mode" = il ] && pre=(env LD_PRELOAD="$il" SQUEEZEFS_IPC_ALLOW_DEV=1)
  echo "-- row $tag: $job $mode ${rt}s (+10 ramp) trace=$trace loadavg=$(cut -d' ' -f1-3 /proc/loadavg)"
  [ "$trace" = 1 ] && drain_trace /dev/null
  sync; sleep 1
  cat "$MNT/.stats" > "$OUT/$tag.stats0"
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
  python3 "$D/2026-09-03-r3-row-delta.py" "$OUT" "$tag" | tee "$OUT/$tag.row" || true
  if [ "$trace" = 1 ]; then
    python3 "$D/op_trace_stitch.py" "$OUT/$tag.trace.json" --stats-pre "$OUT/$tag.stats0" \
      --stats-post "$OUT/$tag.stats1" --ops 0 --json "$OUT/$tag.stitch.json" > "$OUT/$tag.stitch" 2>&1 || true
    grep -A 40 "phase spans" "$OUT/$tag.stitch" | grep "zc_bridge\|read_transport\|read_serve" || true
  fi
}

# ---- fresh cluster + the file set (arm A mints it) ----------------------
echo "== cluster reset"
echo YES | /scratch/tmp/cluster_reset_v4.sh > "$OUT/reset.log" 2>&1 || { tail -5 "$OUT/reset.log"; exit 1; }
mount_arm A prep
row prep-write write_BW kern 30 "" 0
umount_arm

# ---- A B B A --------------------------------------------------------------
if [ "${SHORT:-0}" = 1 ]; then
  mount_arm A A1; row A1-rr4k-kern randread_iops kern 30 "" 0; row A1-seq1m-kern read_BW kern 30 "" 0; umount_arm
  mount_arm B B1; row B1-rr4k-kern randread_iops kern 30 "" 0; row B1-seq1m-kern read_BW kern 30 "" 0; row B1-rr4k-kern-60 randread_iops_60 kern 60 "" 0; umount_arm
  mount_arm B B2; row B2-rr4k-kern randread_iops kern 30 "" 0; row B2-rr4k-il randread_iops il 30 "$B_IL" 0; umount_arm
  mount_arm A A2; row A2-rr4k-kern randread_iops kern 30 "" 0; row A2-rr4k-kern-60 randread_iops_60 kern 60 "" 0; umount_arm
  echo "== done $(date -u +%FT%TZ)"; grep -h "^ROW" "$OUT"/*.row; exit 0
fi
# A1
mount_arm A A1
row A1-rr4k-kern randread_iops kern 30 "" 0
row A1-rr4k-il randread_iops il 30 "$A_IL" 0
row A1-seq1m-kern read_BW kern 30 "" 0
umount_arm
# B1 (+ sustained 60 s legs)
mount_arm B B1
row B1-rr4k-kern randread_iops kern 30 "" 0
row B1-rr4k-il randread_iops il 30 "$B_IL" 0
row B1-seq1m-kern read_BW kern 30 "" 0
row B1-rr4k-kern-60 randread_iops_60 kern 60 "" 0
row B1-rr4k-il-60 randread_iops_60 il 60 "$B_IL" 0
umount_arm
# B2 (traced)
mount_arm B B2 "SQUEEZEFS_OP_TRACE=1"
row B2-rr4k-kern-traced randread_iops kern 30 "" 1
row B2-rr4k-il randread_iops il 30 "$B_IL" 0
row B2-seq1m-kern read_BW kern 30 "" 0
umount_arm
# A2 (traced + sustained 60 s legs)
mount_arm A A2 "SQUEEZEFS_OP_TRACE=1"
row A2-rr4k-kern-traced randread_iops kern 30 "" 1
row A2-rr4k-il randread_iops il 30 "$A_IL" 0
row A2-seq1m-kern read_BW kern 30 "" 0
row A2-rr4k-kern-60 randread_iops_60 kern 60 "" 0
row A2-rr4k-il-60 randread_iops_60 il 60 "$A_IL" 0
umount_arm
echo "== done $(date -u +%FT%TZ)"
grep -h "^ROW" "$OUT"/*.row
