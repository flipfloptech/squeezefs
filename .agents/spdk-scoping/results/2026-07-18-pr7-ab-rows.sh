#!/usr/bin/env bash
# PR 7 (N7) A/B rerun row runner — seeded verbatim from
# .agents/spdk-scoping/bench.sh (same rows, same fio shape, same CPU
# accounting, same quiet gate), driven against product-verb-shared
# namespaces on the fidelity substrate. Session-local harness; the
# committed record is .benchmarks/2026-07-18-nvmeof-dual-stack-ab.md.
#
# Usage: rows.sh <arm> <device> <spdk_pid|-> <runs> <row>...
#   row ∈ r32 w32 r1 w1 sr8 sw8   (or 'all')
set -euo pipefail
ARM=$1; DEV=$2; SPDK_PID=$3; RUNS=$4; shift 4
RESULTS=/tmp/sqz-pr7-ab/results
mkdir -p "$RESULTS"
CSV="$RESULTS/summary.csv"
[ -f "$CSV" ] || echo "arm,row,run,iops,bw_MBps,clat_p50_us,clat_p99_us,wall_s,sys_busy_cpu_s,spdk_cpu_s,tctl_c" > "$CSV"

log() { echo "[rows $ARM $(date +%H:%M:%S)] $*"; }
tctl() { sensors 2>/dev/null | awk '/Tctl/{gsub(/[+°C]/,"",$2); print $2; exit}'; }

quiet_gate() {
    local g t load
    for g in $(seq 1 30); do
        t=$(tctl); load=$(awk '{print $1}' /proc/loadavg)
        # spdk poller legitimately pins up to 4 cores (scaling rows); gate 6.0
        if awk -v l="$load" 'BEGIN{exit !(l < 6.0)}' && awk -v t="$t" 'BEGIN{exit !(t < 80)}'; then
            return 0
        fi
        log "quiet gate wait (load=$load tctl=$t)"; sleep 10
    done
    log "WARN: quiet gate not reached, proceeding (recorded)"
}

# CORRECTED from bench.sh (its comment said user+nice+sys+irq+softirq+steal
# but $6 is IOWAIT in /proc/stat's cpu line — idle CPU, not burn; on this
# box ~10 parked io_uring waiters keep nr_iowait/PSI-io high at true idle,
# which inflated the seed formula by ~8 phantom cores). Correct busy set:
# user($2)+nice($3)+system($4)+irq($7)+softirq($8)+steal($9).
cpu_busy() { awk '/^cpu /{print $2+$3+$4+$7+$8+$9}' /proc/stat; }
proc_cpu() { [ "$1" != "-" ] && [ -r "/proc/$1/stat" ] && awk '{print $14+$15}' "/proc/$1/stat" || echo 0; }

run_row() { # row rw bs qd
    local row=$1 rw=$2 bs=$3 qd=$4 i
    for i in $(seq 1 "$RUNS"); do
        quiet_gate
        local t0 c0 p0 t1 c1 p1 json
        json="$RESULTS/${ARM}-${row}-r${i}.json"
        c0=$(cpu_busy); p0=$(proc_cpu "$SPDK_PID"); t0=$(date +%s.%N)
        taskset -c 0-7 fio --name="$row" --filename="$DEV" --rw="$rw" --bs="$bs" \
            --iodepth="$qd" --numjobs=1 --direct=1 --ioengine=io_uring \
            --time_based --runtime=10 --ramp_time=2 --norandommap --randrepeat=0 \
            --output-format=json --output="$json" >/dev/null
        t1=$(date +%s.%N); c1=$(cpu_busy); p1=$(proc_cpu "$SPDK_PID")
        local hz wall sysb spdkb
        hz=$(getconf CLK_TCK); wall=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.1f", b-a}')
        sysb=$(awk -v a="$c0" -v b="$c1" -v h="$hz" 'BEGIN{printf "%.1f", (b-a)/h}')
        spdkb=$(awk -v a="$p0" -v b="$p1" -v h="$hz" 'BEGIN{printf "%.1f", (b-a)/h}')
        local iops bw p50 p99
        iops=$(jq -r '[.jobs[0].read.iops, .jobs[0].write.iops] | add | floor' "$json")
        bw=$(jq -r '([.jobs[0].read.bw_bytes, .jobs[0].write.bw_bytes] | add) / 1048576 | floor' "$json")
        p50=$(jq -r '([.jobs[0].read.clat_ns.percentile."50.000000" // 0, .jobs[0].write.clat_ns.percentile."50.000000" // 0] | max) / 1000 | floor' "$json")
        p99=$(jq -r '([.jobs[0].read.clat_ns.percentile."99.000000" // 0, .jobs[0].write.clat_ns.percentile."99.000000" // 0] | max) / 1000 | floor' "$json")
        echo "$ARM,$row,$i,$iops,$bw,$p50,$p99,$wall,$sysb,$spdkb,$(tctl)" >> "$CSV"
        log "$row r$i: iops=$iops bw=${bw}MB/s p50=${p50}us p99=${p99}us sys_cpu=${sysb}s spdk_cpu=${spdkb}s"
    done
}

for want in "$@"; do
    case "$want" in
    all) run_row rand4k-read-qd32 randread 4k 32
         run_row rand4k-write-qd32 randwrite 4k 32
         run_row rand4k-read-qd1 randread 4k 1
         run_row rand4k-write-qd1 randwrite 4k 1
         run_row seq128k-read-qd8 read 128k 8
         run_row seq128k-write-qd8 write 128k 8 ;;
    r32) run_row rand4k-read-qd32 randread 4k 32 ;;
    w32) run_row rand4k-write-qd32 randwrite 4k 32 ;;
    r1)  run_row rand4k-read-qd1 randread 4k 1 ;;
    w1)  run_row rand4k-write-qd1 randwrite 4k 1 ;;
    sr8) run_row seq128k-read-qd8 read 128k 8 ;;
    sw8) run_row seq128k-write-qd8 write 128k 8 ;;
    *) echo "unknown row $want" >&2; exit 2 ;;
    esac
done
log "arm $ARM rows done"
