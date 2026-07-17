#!/usr/bin/env bash
# spdkscope: clean re-measure of the contaminated row (spdk-tcp rand4k-read-qd32)
# after verifying no stray load. Restart-from-zero per the multi-run discipline:
# the contaminated rows are dropped from the CSV (kept in summary.csv.contaminated
# for the record) and three fresh runs are appended.
set -euo pipefail
STATE=/tmp/spdkscope/state
RESULTS=/tmp/spdkscope/results
. "$STATE/devices"
SPDK_PID=$(cat "$STATE/spdk_tgt.pid")

if pgrep -f 'fio --name' >/dev/null; then
    echo "FATAL: stray fio detected — refusing to measure"; exit 1
fi
kill -CONT "$SPDK_PID" 2>/dev/null || true

CSV="$RESULTS/summary.csv"
grep -E '^spdk-tcp,(rand4k-read-qd32|seq128k-write-qd8,1,(1520|1462|1461),)' "$CSV" \
    > "$RESULTS/summary.csv.contaminated" || true
grep -v -E '^spdk-tcp,(rand4k-read-qd32|seq128k-write-qd8,1,(1520|1462|1461),)' "$CSV" \
    > "$CSV.clean"
mv "$CSV.clean" "$CSV"

tctl() { sensors 2>/dev/null | awk '/Tctl/{gsub(/[+°C]/,"",$2); print $2; exit}'; }
cpu_busy() { awk '/^cpu /{print $2+$3+$4+$6+$7+$8}' /proc/stat; }
proc_cpu() { awk '{print $14+$15}' "/proc/$SPDK_PID/stat"; }

for i in 1 2 3; do
    while :; do
        load=$(awk '{print $1}' /proc/loadavg); t=$(tctl)
        awk -v l="$load" -v t="$t" 'BEGIN{exit !(l < 3.0 && t < 80)}' && break
        echo "quiet gate wait (load=$load tctl=$t)"; sleep 10
    done
    json="$RESULTS/spdk-tcp-rand4k-read-qd32-clean-r${i}.json"
    c0=$(cpu_busy); p0=$(proc_cpu); t0=$(date +%s.%N)
    taskset -c 0-7 fio --name=rand4k-read-qd32 --filename="$DEV_SPDK" --rw=randread \
        --bs=4k --iodepth=32 --numjobs=1 --direct=1 --ioengine=io_uring \
        --time_based --runtime=10 --ramp_time=2 --norandommap --randrepeat=0 \
        --output-format=json --output="$json" >/dev/null
    t1=$(date +%s.%N); c1=$(cpu_busy); p1=$(proc_cpu)
    hz=$(getconf CLK_TCK)
    wall=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.1f", b-a}')
    sysb=$(awk -v a="$c0" -v b="$c1" -v h="$hz" 'BEGIN{printf "%.1f", (b-a)/h}')
    spdkb=$(awk -v a="$p0" -v b="$p1" -v h="$hz" 'BEGIN{printf "%.1f", (b-a)/h}')
    iops=$(jq -r '.jobs[0].read.iops | floor' "$json")
    bw=$(jq -r '.jobs[0].read.bw_bytes / 1048576 | floor' "$json")
    p50=$(jq -r '.jobs[0].read.clat_ns.percentile."50.000000" / 1000 | floor' "$json")
    p99=$(jq -r '.jobs[0].read.clat_ns.percentile."99.000000" / 1000 | floor' "$json")
    echo "spdk-tcp,rand4k-read-qd32,$i,$iops,$bw,$p50,$p99,$wall,$sysb,$spdkb,$(tctl)" >> "$CSV"
    echo "clean r$i: iops=$iops p50=${p50}us p99=${p99}us sys=${sysb}s spdk=${spdkb}s"
done
