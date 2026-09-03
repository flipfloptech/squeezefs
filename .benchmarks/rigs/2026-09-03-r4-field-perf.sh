#!/usr/bin/env bash
# .benchmarks/rigs/2026-09-03-r4-field-perf.sh — R-4 step 1 (the per-op
# CPU ledger of the reap worker): ONE kern rand-4k row on squeeze-test
# under the shipped binary, with `perf` on the `f3-ur*` queue-worker
# threads mid-row — a flat cpu-clock profile (the per-symbol self-time
# ledger), a short DWARF call-graph capture (caller attribution), a
# syscall/context-switch count per op (`perf stat`), and a `perf sched`
# capture for the parked worker's wake latency (the R-2 §5.3 instrument).
#
# Writes ONLY under /scratch/tmp/sqz-agent/r4/. Run as root on the box:
#   BIN=/scratch/tmp/squeezefs.kvmap bash /scratch/tmp/sqz-agent/r4/2026-09-03-r4-field-perf.sh
set -eu
D=/scratch/tmp/sqz-agent/r4
OUT="${OUT:-$D/perf-$(date +%Y%m%d-%H%M%S)}"
META="${META:-sqmeta:///dev/nvme0n1,/dev/nvme2n1,/dev/nvme4n1,/dev/nvme6n1,/dev/nvme8n1}"
MNT=/scratch/tmp/test
JOBS=/scratch/tmp/fio_jobs
BIN=${BIN:-/scratch/tmp/squeezefs.kvmap}
RESET=${RESET:-1}
mkdir -p "$OUT"
exec > >(tee -a "$OUT/driver.log") 2>&1
echo "== R-4 field perf row: $(date -u +%FT%TZ) out=$OUT kernel=$(uname -r) bin=$BIN"
"$BIN" --version
if ps -eo args | grep -q "[s]queezefs[^ ]* mount"; then
  echo "a daemon is already up — refusing" >&2; exit 2
fi

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
print("   mount:", {k:m.get(k) for k in ["fuse3_zc_negotiated","fuse3_kmbuf_negotiated","transport_queues","transport_q_depth","transport_max_write","data_read_lanes","build_profile"]})
EOF
}
umount_it() {
  "$BIN" umount "$MNT" || umount "$MNT" || true
  for _ in $(seq 1 60); do ps -eo args | grep -q "[s]queezefs[^ ]* mount" || break; sleep 1; done
  sleep 2
}

perf_workers() {  # $1 = outdir  (flat 8 s, dwarf 2 s, stat 3 s, sched 2 s)
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
  perf stat -e 'syscalls:sys_enter_io_uring_enter,syscalls:sys_enter_read,syscalls:sys_enter_futex,syscalls:sys_enter_write,context-switches,cpu-migrations' -t "$tids" -- sleep 3 2>"$o/ur-stat.txt" || true
  grep -E "io_uring|read|futex|write|context|migr|seconds" "$o/ur-stat.txt" || true
  perf sched record -o "$o/sched.data" -- sleep 2 2>"$o/perf-sched.err" || true
  perf report -i "$o/ur-flat.data" --stdio --no-children --sort dso,sym -g none --percent-limit 0.02 2>/dev/null | grep -v "^#" | grep -v "^$" > "$o/flat.txt" || true
  perf report -i "$o/ur-flat.data" --stdio --no-children --sort dso -g none 2>/dev/null | grep -v "^#" | grep -v "^$" | head -8 > "$o/flat-dso.txt" || true
  cat "$o/flat-dso.txt"
}

row() {  # $1 = tag, $2 = job, $3 = perf (0|1)
  local tag="$1" job="$2" doperf="${3:-0}"
  echo "-- row $tag: $job loadavg=$(cut -d' ' -f1-3 /proc/loadavg)"
  sync; sleep 1
  cat "$MNT/.stats" > "$OUT/$tag.stats0"
  # box-wide CPU: /proc/stat cpu line at row start/end (busy % over the row)
  head -1 /proc/stat > "$OUT/$tag.procstat0"
  fio "$JOBS/$job.job" --output-format=json --output="$OUT/$tag.fio.json" \
    --write_bw_log="$OUT/$tag" --log_avg_msec=1000 > "$OUT/$tag.fio.txt" 2>&1 &
  local fpid=$!
  if [ "$doperf" = 1 ]; then
    sleep 14
    perf_workers "$OUT/$tag.perf"
  fi
  wait "$fpid" || { echo "fio failed: $(tail -3 "$OUT/$tag.fio.txt")"; return 1; }
  cat "$MNT/.stats" > "$OUT/$tag.stats1"
  head -1 /proc/stat > "$OUT/$tag.procstat1"
  python3 - "$OUT/$tag.procstat0" "$OUT/$tag.procstat1" <<'PY'
import sys
a=[int(x) for x in open(sys.argv[1]).read().split()[1:]]; b=[int(x) for x in open(sys.argv[2]).read().split()[1:]]
d=[y-x for x,y in zip(a,b)]; tot=sum(d); idle=d[3]+d[4]
print(f"   box cpu: busy {100*(tot-idle)/tot:.1f}% (user {100*d[0]/tot:.1f} sys {100*d[2]/tot:.1f} irq {100*d[5]/tot:.1f} softirq {100*d[6]/tot:.1f} iowait {100*d[4]/tot:.1f}) over {tot/100/32:.0f}s x 32 cpus")
PY
  python3 "$D/2026-09-03-r3-row-delta.py" "$OUT" "$tag" | tee "$OUT/$tag.row" || true
}

if [ "$RESET" = 1 ]; then
  echo "== cluster reset"
  echo YES | /scratch/tmp/cluster_reset_v4.sh > "$OUT/reset.log" 2>&1 || { tail -5 "$OUT/reset.log"; exit 1; }
  mount_it prep
  row prep-write write_BW 0
  umount_it
fi
mount_it A0
row A0-rr4k-kern randread_iops 1
umount_it
echo "== done $(date -u +%FT%TZ)"
grep -h "^ROW" "$OUT"/*.row
