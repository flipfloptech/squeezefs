#!/usr/bin/env bash
# .benchmarks/rigs/2026-09-06-kernel-bg-per-queue-ab.sh — the kernel
# COMMIT-lock split A/B (sqz 7.2-track patch 0026, per-queue FUSE background
# accounting; docs/design-kernel-bg-per-queue.md §5).
#
# ONE boot per invocation. The arms are KERNELS, not daemons: the SAME
# daemon binary + shim run under kernel A (the sqz series 0001-0025 — or the
# running 6.19.14-sqz on the field box) and kernel B (the same series +
# 0026). A-B-B-A across reboots is impossible, so the schedule is A A B B:
#   boot A → ARM=A bash …-ab.sh   (fresh cluster + prep, then the rows x2)
#   boot A → ARM=A bash …-ab.sh   (RESET=1 again — same-state repeat)
#   boot B → ARM=B bash …-ab.sh
#   boot B → ARM=B bash …-ab.sh
# The rig refuses to run if `uname -r` does not contain KERNEL_TAG (a
# substring you set per arm, e.g. KERNEL_TAG=7.2.3-sqz-v2) so a row can never
# be filed under the wrong kernel. Every row records the box's THERMAL state
# (thermal zones + hwmon maxima), the box-wide /proc/stat busy and loadavg
# beside its .row — the cross-reboot bracket has no A-B-B-A to cancel drift,
# so the state is the control.
#
# Rows per boot (2026-09-03-r4-field-abba.sh's jobs verbatim): kern rand-4k
# 24 x qd8 30 s (+10 ramp) x2, the 60 s sustained kern rand-4k, kern read_BW
# (1 MiB qd16) x2, ONE il rand-4k control (the shim does not take these
# locks — it must NOT move), and ONE perf'd kern rand-4k row: flat cpu-clock
# on the f3-ur* workers + a 2 s DWARF call graph, then the two lock symbols'
# shares and callers extracted (the ledger column).
#
# Columns (2026-09-03-r4-row-delta.py, copied beside this rig on the box):
# daemon_cpu_ns_by_class[fuse3-ur] per op, read_transport_phase_ns exact
# means, IOPS, clat p50/p99, zc_bridge_phase_ns; plus <tag>.perf/lock.txt
# (the native_queued_spin_lock_slowpath + _raw_spin_lock shares and their
# callers) and <tag>.thermal.
#
# Writes ONLY under /scratch/tmp/sqz-agent/k26/. Run as root on the box:
#   ARM=A KERNEL_TAG=6.19.14-sqz  bash /scratch/tmp/sqz-agent/k26/2026-09-06-kernel-bg-per-queue-ab.sh
#   ARM=B KERNEL_TAG=sqz-v2       bash /scratch/tmp/sqz-agent/k26/2026-09-06-kernel-bg-per-queue-ab.sh
set -eu
D=/scratch/tmp/sqz-agent/k26
ARM=${ARM:?set ARM=A|B (the KERNEL under test)}
KERNEL_TAG=${KERNEL_TAG:?set KERNEL_TAG to a substring uname -r must contain for this arm}
OUT="${OUT:-$D/ab-$ARM-$(date +%Y%m%d-%H%M%S)}"
META="${META:-sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1}"
MNT=/scratch/tmp/test
JOBS=/scratch/tmp/fio_jobs
BIN=${BIN:-/scratch/tmp/squeezefs}
IL=${IL:-/scratch/tmp/libsqueezefs_il.so}
RESET=${RESET:-1}
DELTA=${DELTA:-$D/2026-09-03-r4-row-delta.py}

case "$ARM" in A|B) ;; *) echo "ARM must be A or B" >&2; exit 1 ;; esac
case "$(uname -r)" in *"$KERNEL_TAG"*) ;; *) echo "kernel $(uname -r) does not match KERNEL_TAG=$KERNEL_TAG for ARM=$ARM — refusing" >&2; exit 2 ;; esac
[ -x "$BIN" ] || { echo "BIN=$BIN not executable" >&2; exit 1; }
[ -r "$DELTA" ] || { echo "row-delta script missing: $DELTA (copy .benchmarks/rigs/2026-09-03-r4-row-delta.py beside this rig)" >&2; exit 1; }
mkdir -p "$OUT"
JOB60="$D/randread_iops_60.job"
sed "s/^runtime=30$/runtime=60/" "$JOBS/randread_iops.job" > "$JOB60"
grep -q "^runtime=60$" "$JOB60" || { echo "job rewrite failed" >&2; exit 1; }
exec > >(tee -a "$OUT/driver.log") 2>&1
echo "== kernel bg-per-queue A/B, ARM=$ARM: $(date -u +%FT%TZ) out=$OUT kernel=$(uname -r) bin=$BIN"
"$BIN" --version
uname -a > "$OUT/uname.txt"
# the kernel's own statement of which series it carries: the fuse module's
# symbol table has fuse_uring_bg_wait only with 0026
if grep -q " fuse_uring_bg_wait$" /proc/kallsyms 2>/dev/null; then echo "   kernel: fuse_uring_bg_wait PRESENT (0026 kernel)"; echo present > "$OUT/kernel-0026.txt";
else echo "   kernel: fuse_uring_bg_wait absent (pre-0026 kernel)"; echo absent > "$OUT/kernel-0026.txt"; fi
case "$ARM:$(cat "$OUT/kernel-0026.txt")" in
  A:present) echo "ARM=A but the running kernel carries 0026 — refusing" >&2; exit 2 ;;
  B:absent)  echo "ARM=B but the running kernel lacks 0026 (module not loaded yet? mount once and re-check) — continuing, re-verified after mount" ;;
esac

if ps -eo args | grep -q "[s]queezefs[^ ]* mount"; then
  echo "a daemon is already up — refusing" >&2; exit 2
fi

thermal() {  # $1 = out file — zones + hwmon maxima, loadavg, box busy snapshot
  {
    echo "ts=$(date -u +%FT%TZ) loadavg=$(cut -d' ' -f1-3 /proc/loadavg)"
    for z in /sys/class/thermal/thermal_zone*; do
      [ -r "$z/temp" ] && echo "zone $(cat "$z/type" 2>/dev/null) $(cat "$z/temp")"
    done
    for h in /sys/class/hwmon/hwmon*; do
      n=$(cat "$h/name" 2>/dev/null || echo "?")
      m=$(cat "$h"/temp*_input 2>/dev/null | sort -n | tail -1)
      [ -n "$m" ] && echo "hwmon $n max_mC=$m"
    done
    grep -h . /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null | sed 's/^/governor=/'
  } > "$1" 2>/dev/null || true
}

mount_it() {  # $1 = tag, $2 = extra env
  local tag="$1" extra="${2:-}"
  env SQUEEZEFS_IPC_ALLOW_DEV=1 $extra "$BIN" mount "$META" "$MNT" --daemon --interception --allow-other \
    --log-file "$OUT/mount-$tag.log"
  for _ in $(seq 1 90); do
    [ -r "$MNT/.stats" ] && grep -q '"fuse3_zc_replies"' "$MNT/.stats" && break
    sleep 1
  done
  mkdir -p "$MNT/client_validation"
  python3 - "$MNT/.stats" <<'EOF'
import json,sys
m=json.load(open(sys.argv[1]))["metrics"]
print("   mount:", {k:m.get(k) for k in ["fuse3_zc_negotiated","fuse3_kmbuf_negotiated","transport_queues","transport_q_depth","transport_max_background","transport_max_write","data_read_lanes","build_profile"]})
EOF
  # with the fuse module loaded, the 0026 symbol check is authoritative
  if grep -q " fuse_uring_bg_wait$" /proc/kallsyms; then echo present > "$OUT/kernel-0026.txt"; else echo absent > "$OUT/kernel-0026.txt"; fi
  echo "   kernel-0026: $(cat "$OUT/kernel-0026.txt") (ARM=$ARM)"
  if [ "$ARM" = B ] && [ "$(cat "$OUT/kernel-0026.txt")" != present ]; then echo "ARM=B without 0026 in the loaded fuse module — refusing" >&2; umount_it; exit 2; fi
  # the connection's max_background as the kernel sees it (fusectl) — the per-queue share is this / transport_queues
  for c in /sys/fs/fuse/connections/*/; do
    [ -r "$c/max_background" ] && echo "   fusectl $(basename "$c"): max_background=$(cat "$c/max_background") congestion_threshold=$(cat "$c/congestion_threshold")"
  done
}

umount_it() {
  "$BIN" umount "$MNT" || umount "$MNT" || true
  for _ in $(seq 1 60); do ps -eo args | grep -q "[s]queezefs[^ ]* mount" || break; sleep 1; done
  sleep 2
}

perf_workers() {  # $1 = outdir — flat 8 s + dwarf 2 s on the f3-ur* threads, then the lock ledger
  local o="$1"; mkdir -p "$o"
  local pid; pid=$(ps -eo pid,args | awk '/[s]queezefs[^ ]* mount/ {print $1; exit}')
  local tids; tids=$(for t in /proc/$pid/task/*; do n=$(cat "$t/comm"); case "$n" in f3-ur[0-9]*) basename "$t";; esac; done | paste -sd,)
  echo "$tids" > "$o/ur.tids"; echo "   perf: pid=$pid ur-tids=$(echo "$tids" | tr ',' '\n' | wc -l)"
  for t in $(echo "$tids" | tr ',' ' '); do awk '{print $14+$15}' /proc/$pid/task/$t/stat; done | paste -sd' ' > "$o/ur.cpu0"
  date +%s.%N > "$o/ur.t0"
  perf record -e cpu-clock -F 4999 -t "$tids" -o "$o/ur-flat.data" -- sleep 8 2>"$o/perf-flat.err" || true
  date +%s.%N > "$o/ur.t1"
  for t in $(echo "$tids" | tr ',' ' '); do awk '{print $14+$15}' /proc/$pid/task/$t/stat; done | paste -sd' ' > "$o/ur.cpu1"
  perf record -e cpu-clock -F 1999 --call-graph dwarf,16384 -t "$tids" -o "$o/ur-cg.data" -- sleep 2 2>"$o/perf-cg.err" || true
  perf report -i "$o/ur-flat.data" --stdio --no-children --sort dso,sym -g none --percent-limit 0.02 2>/dev/null | grep -v "^#" | grep -v "^$" > "$o/flat.txt" || true
  perf report -i "$o/ur-flat.data" --stdio --no-children --sort dso -g none 2>/dev/null | grep -v "^#" | grep -v "^$" | head -8 > "$o/flat-dso.txt" || true
  # THE ledger column: the two lock symbols' self-time shares + their callers
  {
    echo "# lock class shares (flat, self time) — the R-4 §2 ledger's 7.8 % + 3.1 %"
    grep -E "native_queued_spin_lock_slowpath|_raw_spin_lock\b|_raw_spin_lock_irq|fuse_uring_req_end|fuse_request_end|fuse_uring_queue_bq_req|fuse_uring_commit_fetch" "$o/flat.txt" || echo "(none above 0.02 %)"
    echo "# callers of native_queued_spin_lock_slowpath (dwarf, 2 s)"
    perf report -i "$o/ur-cg.data" --stdio --no-children -S native_queued_spin_lock_slowpath -g caller,0.5,callee --percent-limit 0.5 2>/dev/null | grep -v "^#" | grep -v "^$" | head -60 || true
    echo "# callers of _raw_spin_lock (dwarf, 2 s)"
    perf report -i "$o/ur-cg.data" --stdio --no-children -S _raw_spin_lock -g caller,0.5,callee --percent-limit 0.5 2>/dev/null | grep -v "^#" | grep -v "^$" | head -60 || true
  } > "$o/lock.txt"
  echo "   lock ledger:"; sed -n 1,6p "$o/lock.txt" | sed 's/^/     /'
  cat "$o/flat-dso.txt"
}

row() {  # $1 = tag, $2 = job (randread_iops|randread_iops_60|read_BW|write_BW), $3 = mode (kern|il), $4 = runtime, $5 = perf (0|1)
  local tag="$1" job="$2" mode="$3" rt="$4" doperf="${5:-0}"
  local pre=()
  [ "$mode" = il ] && pre=(env LD_PRELOAD="$IL" SQUEEZEFS_IPC_ALLOW_DEV=1)
  echo "-- row $tag: $job $mode ${rt}s (+10 ramp) perf=$doperf loadavg=$(cut -d' ' -f1-3 /proc/loadavg)"
  thermal "$OUT/$tag.thermal0"
  sync; sleep 1
  cat "$MNT/.stats" > "$OUT/$tag.stats0"
  head -1 /proc/stat > "$OUT/$tag.procstat0"
  local jobfile="$JOBS/$job.job"
  [ "$job" = randread_iops_60 ] && jobfile="$JOB60"
  "${pre[@]}" fio "$jobfile" --output-format=json --output="$OUT/$tag.fio.json" \
    --write_bw_log="$OUT/$tag" --log_avg_msec=1000 > "$OUT/$tag.fio.txt" 2>&1 &
  local fpid=$!
  if [ "$doperf" = 1 ]; then
    sleep 14
    perf_workers "$OUT/$tag.perf"
  fi
  wait "$fpid" || { echo "fio failed: $(tail -3 "$OUT/$tag.fio.txt")"; return 1; }
  cat "$MNT/.stats" > "$OUT/$tag.stats1"
  head -1 /proc/stat > "$OUT/$tag.procstat1"
  thermal "$OUT/$tag.thermal1"
  python3 - "$OUT/$tag.procstat0" "$OUT/$tag.procstat1" <<'PY'
import sys
a=[int(x) for x in open(sys.argv[1]).read().split()[1:]]; b=[int(x) for x in open(sys.argv[2]).read().split()[1:]]
d=[y-x for x,y in zip(a,b)]; tot=sum(d); idle=d[3]+d[4]
print(f"   box cpu: busy {100*(tot-idle)/tot:.1f}% (user {100*d[0]/tot:.1f} sys {100*d[2]/tot:.1f} irq {100*d[5]/tot:.1f} softirq {100*d[6]/tot:.1f} iowait {100*d[4]/tot:.1f})")
PY
  echo "   thermal: $(grep -h hwmon "$OUT/$tag.thermal0" | sort -t= -k2 -n | tail -1) -> $(grep -h hwmon "$OUT/$tag.thermal1" | sort -t= -k2 -n | tail -1)"
  python3 "$DELTA" "$OUT" "$tag" | tee "$OUT/$tag.row" || true
  # the kernel-side tripwires this patch adds nothing to but must not trip
  dmesg -T 2>/dev/null | grep -iE "fuse|WARN|lockdep|BUG" | tail -5 > "$OUT/$tag.dmesg" || true
  [ -s "$OUT/$tag.dmesg" ] && { echo "   dmesg (fuse/WARN/lockdep/BUG tail):"; sed 's/^/     /' "$OUT/$tag.dmesg"; }
}

# ---- fresh cluster + the file set ----------------------------------------
if [ "$RESET" = 1 ]; then
  echo "== cluster reset"
  echo YES | /scratch/tmp/cluster_reset_v4.sh > "$OUT/reset.log" 2>&1 || { tail -5 "$OUT/reset.log"; exit 1; }
  mount_it prep
  row prep-write write_BW kern 30 0
  umount_it
fi

# ---- the rows, one boot ---------------------------------------------------
T=$ARM
mount_it "$T"
row "$T-rr4k-kern-1"   randread_iops    kern 30 0
row "$T-seq1m-kern-1"  read_BW          kern 30 0
row "$T-rr4k-kern-2"   randread_iops    kern 30 0
row "$T-rr4k-il"       randread_iops    il   30 0
row "$T-seq1m-kern-2"  read_BW          kern 30 0
row "$T-rr4k-kern-60"  randread_iops_60 kern 60 0
row "$T-rr4k-kern-perf" randread_iops   kern 30 1
umount_it
echo "== done $(date -u +%FT%TZ) kernel=$(uname -r) ARM=$ARM kernel-0026=$(cat "$OUT/kernel-0026.txt")"
grep -h "^ROW" "$OUT"/*.row
echo "== compare across boots: python3 $D/2026-09-03-r4-row-delta.py is per row; pair A*/B* dirs by tag and read fuse3-ur us/op, lock.txt shares, IOPS, p50/p99, thermal"
