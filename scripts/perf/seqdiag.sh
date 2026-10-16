#!/usr/bin/env bash
# seqdiag.sh <label> [rows] — the sequential-write decomposition (rung-10-seqwrite): RAM-backed NVMe,
# one fresh mount, a 4 × 2 GiB lay-down, then each row "<bs> <qd> <jobs>" time-based SD_RT s: .stats
# before/after, the daemon's per-thread CPU, the handler lanes' current syscall sampled every 10 ms, and
# (SD_OFFCPU=1) offcputime-bpfcc folded stacks mid-row. Output: /build/perf/seqdiag/<label>;
# scripts/perf/seqdiag.py <dir> prints the decomposition.
# Run holding the venue alone (the flock chain of baseline.sh).
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
L=$1 ROWS=${2:-"1m 16 4;1m 16 1;1m 4 4;1m 1 4;1m 1 1;4m 4 4"}
O=/build/perf/seqdiag/$L
M=/mnt/sqz-perf
if [ "${SD_INNER:-0}" = 1 ]; then
  pid=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["mount"]["pid"])' "$M/.stats")
  settle() { for _ in $(seq 100); do grep -q '"fuse_owed_drain_inos":0,' "$M/.stats" && grep -q '"parked_buffers":0,' "$M/.stats" && return; sleep 0.1; done; }
  tcpu() { local t s c f; for t in /proc/$pid/task/*; do s=$(cat "$t/stat" 2>/dev/null) || continue; c=${s#*(}; c=${c%%)*}; f=(${s##*) }); echo "${t##*/} $c $((f[11] + f[12]))"; done; }
  fio --name=ld --rw=write --bs=1m --iodepth=16 --numjobs=4 --ioengine=io_uring --direct=1 --group_reporting \
    --directory=$M --filename_format='sd.$jobnum' --size=2g --output-format=terse >/dev/null
  settle
  IFS=';' read -r -a rows <<< "$ROWS"
  for r in "${rows[@]}"; do
    read -r bs qd nj <<< "$r"; n=sw$bs-q${qd}j$nj
    cat "$M/.stats" > "$O/$n.stats0.json"
    fio --name=sd --rw=write --bs=$bs --iodepth=$qd --numjobs=$nj --ioengine=io_uring --direct=1 --group_reporting \
      --directory=$M --filename_format='sd.$jobnum' --size=2g --time_based --runtime=${SD_RT:-20} --end_fsync=1 \
      --output-format=json --output="$O/$n.json" &
    f=$!
    sleep 5; tcpu > "$O/$n.t0"; t0=$(date +%s.%N)
    [ "${SD_OFFCPU:-0}" = 1 ] && [ "$r" = "1m 16 4" ] && timeout 30 offcputime-bpfcc -f -p "$pid" 8 > "$O/$n.offcpu" 2>/dev/null
    # The lanes' current syscall every 10 ms for 5 s (202 futex, 426 io_uring_enter, running = on CPU).
    for _ in $(seq 500); do for t in /proc/$pid/task/*; do read -r c < "$t/comm"; case $c in sqz-fuse-ln*) read -r s _ < "$t/syscall" 2>/dev/null; echo "$c ${s:-x}";; esac; done; sleep 0.01; done | sort | uniq -c > "$O/$n.sys"
    sleep 3; tcpu > "$O/$n.t1"; echo "$(date +%s.%N) - $t0" | bc > "$O/$n.wall"
    wait $f; settle
    cat "$M/.stats" > "$O/$n.stats1.json"
    python3 -c 'import json,sys; j=json.load(open(sys.argv[1]))["jobs"][0]["write"]; print(sys.argv[2], "%.0f MiB/s" % (j["bw_bytes"]/2**20), "clat_mean %.0f us" % (j["clat_ns"]["mean"]/1e3))' "$O/$n.json" "$n"
  done
  exit 0
fi
mkdir -p "$O"; cd "$REPO"
zig build -Dprofile=dist --prefix zig-out-perf dist shim || exit 1
scripts/perf/ramnvme.sh up >/dev/null || exit 1
trap 'scripts/perf/ramnvme.sh down >/dev/null' EXIT
scripts/perf_mount.sh --ram --fresh -- "env SD_INNER=1 SD_RT=${SD_RT:-20} SD_OFFCPU=${SD_OFFCPU:-0} bash $HERE/seqdiag.sh $L '$ROWS'" 2>&1 | tail -n 40
