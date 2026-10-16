#!/usr/bin/env bash
# clockreads.sh <mount> <label> — fine clock reads per op by call site on a SqueezeFS mount
# (docs/evidence/rung-10-clockreads.md). On a host whose clocksource has no vDSO mode (HPET, acpi_pm)
# every fine clock_gettime is a syscall, so the tracepoint counts every read the daemon makes; coarse
# clocks stay in the vDSO and are not counted. Per workload (fio QD1, io_uring, O_DIRECT, a fixed op
# count) and an idle window of the same length: the reads by call site (inline chain of the user IP,
# addr2line on the dist build's split debug info) ÷ fio ops. Build the dist binary with frame pointers
# first (zig build -Dprofile=dist -Dframe-pointers=true --prefix zig-out-perf dist shim), then run it
# inside perf_mount.sh --ram -- ….
#   CR_OPS (4000) ops per 4 KiB row; the 1 MiB row runs CR_OPS/16.
set -euo pipefail
M=$1 L=$2
N=${CR_OPS:-4000}
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DBG=$HERE/../../zig-out-perf/bin/squeezefs.debug
O=/build/perf/cr/$L; mkdir -p "$O"
pid=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["mount"]["pid"])' "$M/.stats")
f=(--ioengine=io_uring --direct=1 --filename="$M/cr.0" --size=256m --iodepth=1 --output-format=terse)
fio --name=lay --rw=write --bs=1m "${f[@]}" >/dev/null
sleep 2
row() {   # name ops fio-args…
  local r=$1 n=$2; shift 2
  local t0 t1
  t0=$(date +%s.%N)
  perf record -q -e syscalls:sys_enter_clock_gettime --call-graph fp -p "$pid" -o "$O/$r.data" -- \
    fio --name="$r" --number_ios="$n" "${f[@]}" "$@" >/dev/null 2>&1
  t1=$(date +%s.%N)
  echo "$r $n $(echo "$t1 - $t0" | bc)" >> "$O/rows"
  perf script -i "$O/$r.data" -F comm,ip,dso --no-inline 2>/dev/null > "$O/$r.script"
}
: > "$O/rows"
row rr4k "$N" --rw=randread --bs=4k
row rw4k "$N" --rw=randwrite --bs=4k
row sw1m "$((N / 16))" --rw=write --bs=1m
# idle: as long as the rw4k row took
idle=$(awk '$1 == "rw4k" { print $3 }' "$O/rows")
perf record -q -e syscalls:sys_enter_clock_gettime --call-graph fp -p "$pid" -o "$O/idle.data" -- sleep "$idle" 2>/dev/null
echo "idle 1 $idle" >> "$O/rows"
perf script -i "$O/idle.data" -F comm,ip,dso --no-inline 2>/dev/null > "$O/idle.script"
python3 - "$O" "$DBG" <<'EOF'
import sys, subprocess, collections, os
O, dbg = sys.argv[1], sys.argv[2]
rows = [l.split() for l in open(os.path.join(O, "rows"))]
idle_s = float([r for r in rows if r[0] == "idle"][0][2])
def stacks(name):
    out = []; cur = None; frames = []
    for line in open(os.path.join(O, name + ".script")):
        if not line.strip():
            if cur is not None: out.append((cur, frames))
            cur = None; frames = []; continue
        if not line.startswith((' ', '\t')):
            cur = line.split()[0]; frames = []; continue
        p = line.split()
        if len(p) >= 2 and 'squeezefs' in p[-1]: frames.append(int(p[0], 16))
    if cur is not None: out.append((cur, frames))
    return out
data = {r[0]: stacks(r[0]) for r in rows}
addrs = sorted({a for st in data.values() for _, fr in st for a in fr[:3]})
names = {}
if addrs:
    out = subprocess.run(["addr2line", "-a", "-i", "-f", "-e", dbg], input="".join("%x\n" % (a - 1) for a in addrs), capture_output=True, text=True).stdout.splitlines()
    chains = []; i = 0
    while i < len(out):
        if out[i].startswith("0x"): chains.append([]); i += 1; continue
        chains[-1].append((out[i], out[i + 1].split("/src/")[-1].split(" ")[0])); i += 2
    for a, chain in zip(addrs, chains):
        keep = [c for c in chain if not any(s in c[1] for s in ("core/clock.zig", "std/", "lib/std"))]
        names[a] = ["%s %s" % (fn.split(".")[-1], fl) for fn, fl in keep]
def site(fr):
    chain = [n for a in fr[:3] for n in names.get(a, [])]
    return " < ".join(chain[:3]) if chain else "(no squeezefs frame)"
idle = collections.Counter(site(fr) for _, fr in data["idle"])
for r in rows:
    if r[0] == "idle": continue
    ops = int(r[1]); secs = float(r[2])
    c = collections.Counter(); th = collections.Counter()
    for comm, fr in data[r[0]]:
        c[site(fr)] += 1; th[comm.rstrip("0123456789")] += 1
    tot = sum(c.values()); bg = sum(idle.values()) * secs / idle_s
    print("== %s: %d ops in %.2f s: %d reads = %.2f/op (idle-rate background %.0f = %.2f/op)" % (r[0], ops, secs, tot, tot / ops, bg, bg / ops))
    print("   by thread: " + ", ".join("%s %.2f" % (k, v / ops) for k, v in th.most_common(8)))
    for s, v in c.most_common(30):
        b = idle.get(s, 0) * secs / idle_s
        print("   %7.3f/op (idle %6.3f)  %s" % (v / ops, b / ops, s))
EOF
