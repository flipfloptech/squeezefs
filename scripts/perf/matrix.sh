#!/usr/bin/env bash
# matrix.sh <target> <label> — the perf baseline's fio matrix (docs/evidence/rung-10-perf-baseline.md)
# against a raw block device or a mounted directory. One lay-down (1 MiB sequential O_DIRECT writes of
# 4 regions × MX_SIZE: per-job files in a directory, offset_increment regions on a device), then
# every row time-based for MX_RUNTIME s, io_uring, O_DIRECT. One line per row (also appended to
# $OUT/results.tsv): IOPS, MiB/s, clat p50/p99 µs, fio usr+sys %, host cores busy (/proc/stat), device
# bytes ÷ user bytes (diskstats over the row and its settle), and on a SqueezeFS mount the daemon's cores (/proc/<pid>/stat) and
# its busiest thread classes (cores, the busiest thread's %). .stats snapshots and the fio JSON land
# in $OUT/<label>/.
#   MX_ROWS     regex over row names (default all)     MX_SIZE  per-job region (3g)
#   MX_COOL_C   wait before each row until the CPU package (k10temp Tctl) is at or below this
#               many °C (default 70), at most MX_COOL_MAX_S seconds (default 180): the owner found a
#               thermal throttle skewing rows (2026-10-19); each row records Tctl at its start and end
#   MX_RUNTIME  seconds per row (30)                   MX_OUT   results root (/build/perf/mx)
#   MX_DEVS     block devices whose diskstats count (default: the device, the mount's source, or
#                   a perf_mount.sh set's targets)
#   MX_LAYDOWN=0  skip the lay-down (the files/regions are already written)
#   MX_PROFILE=1  per row: perf record -a -g for 10 s mid-row → <row>.comm.txt (samples by thread)
#                     and <row>.incl-<who>.txt (scripts/perf/incl.py)
# A block device must be unmounted and carry no signature (blkid): the lay-down overwrites it.
set -euo pipefail
T=$1 L=$2
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROWS=${MX_ROWS:-.} S=${MX_SIZE:-3g} RT=${MX_RUNTIME:-30}
OUT=${MX_OUT:-${SQZ_BUILD_ROOT:-/build}/perf/mx}; O=$OUT/$L; mkdir -p "$O"
die() { echo "matrix: $*" >&2; exit 1; }

common=(--ioengine=io_uring --direct=1 --group_reporting --output-format=json)
pid="" devs=()
if [ -b "$T" ]; then
  findmnt -rno TARGET -S "$T" >/dev/null 2>&1 && die "$T is mounted"
  sig=$(blkid -p -o value -s TYPE "$T" 2>/dev/null || true)
  [ -z "$sig" ] || die "$T carries a '$sig' signature; refusing to overwrite it (wipefs it if it is scratch)"
  where=(--filename="$T" --size="$S" --offset_increment="$S")
  devs=("$T")
elif [ -d "$T" ]; then
  where=(--directory="$T" --filename_format='mx.$jobnum' --size="$S")
  if [ -r "$T/.stats" ]; then
    pid=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["mount"]["pid"])' "$T/.stats")
    for t in /var/lib/sqz-perf/targets/*; do devs+=("$(readlink -f "$t")"); done
  else
    devs=("$(findmnt -no SOURCE --target "$T")")
  fi
else
  die "$T is neither a block device nor a directory"
fi
[ -n "${MX_DEVS:-}" ] && read -r -a devs <<< "$MX_DEVS"
devnames=(); for d in "${devs[@]}"; do devnames+=("$(basename "$(readlink -f "$d")")"); done

# The row table: name rw bs iodepth numjobs.
ROWTAB="rr4k-q1j1 randread 4k 1 1
rr4k-q32j1 randread 4k 32 1
rr4k-q32j4 randread 4k 32 4
rw4k-q1j1 randwrite 4k 1 1
rw4k-q32j1 randwrite 4k 32 1
rw4k-q32j4 randwrite 4k 32 4
rr64k-q16j4 randread 64k 16 4
rw64k-q16j4 randwrite 64k 16 4
sr1m-q16j1 read 1m 16 1
sr1m-q16j4 read 1m 16 4
sw1m-q16j1 write 1m 16 1
sw1m-q16j4 write 1m 16 4
sw4m-q4j4 write 4m 4 4"

# A snapshot: wall clock, /proc/stat's cpu line, diskstats of $devnames, the daemon's threads.
snap() {
  python3 - "$1" "${pid:-0}" "${devnames[@]}" <<'EOF'
import json, os, sys, time
out, pid, devs = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
s = {"t": time.monotonic(), "cpu": [int(x) for x in open("/proc/stat").readline().split()[1:]], "disk": {}, "tasks": {}}
for d in devs:
    f = open("/sys/class/block/%s/stat" % d).read().split()
    s["disk"][d] = [int(f[2]), int(f[6])]  # sectors read, sectors written
if pid:
    for tid in os.listdir("/proc/%d/task" % pid):
        try:
            st = open("/proc/%d/task/%s/stat" % (pid, tid)).read()
        except OSError:
            continue
        comm = st[st.index("(") + 1:st.rindex(")")]
        f = st[st.rindex(")") + 2:].split()
        s["tasks"][tid] = [comm, int(f[11]) + int(f[12])]
json.dump(s, open(out, "w"))
EOF
}

# One summary line from the fio JSON, the CPU snapshots taken inside the steady state (m0 at +2 s, m1
# 2 s before the end) and the device snapshots around the whole row and its settle (s0, s1).
summarize() {
  python3 - "$O/$1.json" "$O/$1.m0" "$O/$1.m1" "$O/$1.s0" "$O/$1.s1" "$L" "$1" "$2" "$3" <<'EOF'
import json, os, re, sys, collections
fj, a, b, fa, fb, label, row, tc0, tc1 = sys.argv[1:]
j = json.load(open(fj))["jobs"][0]
a, b, fa, fb = (json.load(open(x)) for x in (a, b, fa, fb))
hz = os.sysconf("SC_CLK_TCK"); wall = b["t"] - a["t"]
iops = j["read"]["iops"] + j["write"]["iops"]
mib = (j["read"]["bw_bytes"] + j["write"]["bw_bytes"]) / 2**20
d = j["read"] if j["read"]["io_bytes"] else j["write"]
pc = d["clat_ns"].get("percentile", {})
p50 = pc.get("50.000000", 0) / 1e3; p99 = pc.get("99.000000", 0) / 1e3
ca, cb = a["cpu"], b["cpu"]; tot = sum(cb[:8]) - sum(ca[:8]); idle = (cb[3] + cb[4]) - (ca[3] + ca[4])
host = (tot - idle) / hz / wall
ur = j["read"]["io_bytes"]; uw = j["write"]["io_bytes"]
dr = sum(fb["disk"][k][0] - fa["disk"][k][0] for k in fb["disk"]) * 512
dw = sum(fb["disk"][k][1] - fa["disk"][k][1] for k in fb["disk"]) * 512
amp = "r%.2f/w%.2f" % (dr / ur if ur else 0, dw / uw if uw else 0)
dcores = "-"; top = "-"
if b["tasks"]:
    cls = collections.defaultdict(lambda: [0.0, 0.0])
    tot_t = 0
    for tid, (comm, t1) in b["tasks"].items():
        t0 = a["tasks"].get(tid, [comm, 0])[1]
        c = (t1 - t0) / hz / wall; tot_t += c
        k = re.sub(r"\d+$", "", comm); cls[k][0] += c; cls[k][1] = max(cls[k][1], c)
    dcores = "%.2f" % tot_t
    top = " ".join("%s:%.2f(%d%%)" % (k, v[0], 100 * v[1]) for k, v in sorted(cls.items(), key=lambda kv: -kv[1][0])[:5])
line = "\t".join([label, row, "%.0f" % iops, "%.1f" % mib, "%.1f" % p50, "%.1f" % p99,
                  "%.0f" % (j["usr_cpu"] + j["sys_cpu"]), "%.2f" % host, dcores, amp, top, "%s>%s" % (tc0, tc1)])
print(line)
open(os.path.join(os.path.dirname(os.path.dirname(fj)), "results.tsv"), "a").write(line + "\n")
EOF
}

settle() {   # a SqueezeFS mount: the write custody drained (owed and parked both 0), ≤ 10 s
  [ -n "$pid" ] || return 0
  local i=0
  while [ $i -lt 100 ]; do
    grep -q '"fuse_owed_drain_inos":0,' "$T/.stats" && grep -q '"parked_buffers":0,' "$T/.stats" && return 0
    sleep 0.1; i=$((i + 1))
  done
  echo "matrix: write custody not drained after 10 s" >&2
}

TCTL=$(for h in /sys/class/hwmon/hwmon*; do [ "$(cat "$h/name" 2>/dev/null)" = k10temp ] && echo "$h/temp1_input"; done | head -1)
tctl() { if [ -n "$TCTL" ]; then echo $(( $(cat "$TCTL") / 1000 )); else echo -; fi; }
cool() {   # thermal: a row starts only once the package is at or below MX_COOL_C (bounded by MX_COOL_MAX_S)
  [ -n "$TCTL" ] || return 0
  local lim=${MX_COOL_C:-70} max=${MX_COOL_MAX_S:-180} w=0
  while [ "$(tctl)" -gt "$lim" ] && [ $w -lt "$max" ]; do sleep 5; w=$((w + 5)); done
  if [ $w -gt 0 ]; then echo "matrix: $L waited $w s for the package to cool to $(tctl) °C before $1" >&2; fi
}

profile() {   # $1 row: 10 s of system-wide stacks mid-row
  local r=$1
  sleep $((RT / 3))
  perf record -a -g -F 499 -o "$O/$r.perf.data" -- sleep 10 >/dev/null 2>&1
  timeout 120 perf report -i "$O/$r.perf.data" --no-inline --sort comm --stdio 2>/dev/null | grep -v '^#' | grep -v '^$' | head -25 > "$O/$r.comm.txt" || true
  # --no-inline: resolving the fat-LTO binary's inlined frames is minutes per profile.
  timeout 300 perf script -i "$O/$r.perf.data" --no-inline 2>/dev/null > "$O/$r.script" || true
  python3 "$HERE/incl.py" "" 45 < "$O/$r.script" > "$O/$r.incl-all.txt"
  python3 "$HERE/incl.py" sqz 45 < "$O/$r.script" > "$O/$r.incl-sqz.txt"
  rm -f "$O/$r.script"
}

if [ "${MX_LAYDOWN:-1}" = 1 ]; then
  cool "the lay-down"
  fio --name=mx --rw=write --bs=1m --iodepth=16 --numjobs=4 "${common[@]}" "${where[@]}" --output="$O/laydown.json"
  settle
  echo "matrix: $L lay-down 4 × $S done ($(python3 -c 'import json,sys; j=json.load(open(sys.argv[1]))["jobs"][0]; print("%.0f MiB/s" % (j["write"]["bw_bytes"] / 2**20))' "$O/laydown.json"))"
fi
echo -e "label\trow\tIOPS\tMiB/s\tp50us\tp99us\tfio%\thost_cores\tdaemon_cores\tdev/user\tthreads\ttctl_c"
while read -r name rw bs qd nj; do
  [[ $name =~ $ROWS ]] || continue
  extra=(--randrepeat=0 --norandommap)
  case $rw in *write) extra+=(--end_fsync=1) ;; esac
  cool "$name"; tc0=$(tctl)
  # (cat, not cp: the virtual .stats has no size for cp's copy_file_range path to trust.)
  [ -n "$pid" ] && cat "$T/.stats" > "$O/$name.stats0.json"
  snap "$O/$name.s0"
  fio --name=mx --rw="$rw" --bs="$bs" --iodepth="$qd" --numjobs="$nj" --time_based --runtime="$RT" \
    "${extra[@]}" "${common[@]}" "${where[@]}" --output="$O/$name.json" &
  fpid=$!
  ( sleep 2; snap "$O/$name.m0"; sleep $((RT - 4)); snap "$O/$name.m1" ) &
  spid=$!
  [ "${MX_PROFILE:-0}" = 1 ] && profile "$name"
  wait $fpid || die "fio $name failed (see $O/$name.json)"
  wait $spid
  settle
  snap "$O/$name.s1"
  [ -n "$pid" ] && cat "$T/.stats" > "$O/$name.stats1.json"
  tc1=$(tctl)
  summarize "$name" "$tc0" "$tc1"
done <<< "$ROWTAB"
