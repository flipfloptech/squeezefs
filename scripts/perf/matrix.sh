#!/usr/bin/env bash
# matrix.sh <target> <label> — the perf baseline's fio matrix (docs/evidence/rung-10-perf-baseline.md)
# against a raw block device or a mounted directory. One lay-down (1 MiB sequential O_DIRECT writes of
# 4 regions × MX_SIZE: per-job files in a directory, offset_increment regions on a device), then
# every row time-based for MX_RUNTIME s, io_uring, O_DIRECT. One line per row (also appended to
# $OUT/results.tsv): IOPS, MiB/s, clat p50/p99 µs, fio usr+sys %, host cores busy (/proc/stat), device
# bytes ÷ user bytes (diskstats over the row and its settle), and on a SqueezeFS mount the daemon's cores (/proc/<pid>/stat) and
# its busiest thread classes (cores, the busiest thread's %). .stats snapshots, the fio JSON and
# <row>.sched.txt (per thread class: cores, voluntary context switches — sleeps — per second and
# per op, CPU µs per op) land in $OUT/<label>/.
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
#   MX_ENGINE   the rows' fio ioengine (io_uring, the default). The client bypass (charter §7,
#               docs/evidence/rung-10-client-bypass-rows.md) is measured with the libc engines the
#               LD_PRELOAD shim interposes — io_uring never enters libc:
#                 libaio  the rows exactly as written (qd × nj): the shim decomposes each io_submit
#                         batch into ring ops, so QD32 × 4 jobs is QD32 × 4 jobs on the ring
#                 psync   pread/pwrite have no depth: a row runs min(qd × nj, MX_PSYNC_MAX_JOBS)
#                         jobs at depth 1 (default cap: nproc); the row NAME stays so the columns
#                         align across engines, the label says the engine. The lay-down writes
#                         MX_PSYNC_MAX_JOBS per-job files of 4 × MX_SIZE ÷ MX_PSYNC_MAX_JOBS each
#                         (the io_uring rows' working set, one file — one fd, one shim session —
#                         per job: a process bound on N fds takes up to N sessions, and 32 jobs ×
#                         4 shared files overran the host's derived admission cap, 2026-10-19).
#   MX_PRELOAD  a shared object LD_PRELOADed into the ROW fio (never the lay-down):
#               <tree>/zig-out-perf/lib/libsqueezefs_il.so is the bypass. On a SqueezeFS mount the
#               row then prints its ENGAGEMENT line — the daemon's Δ ipc_ops_{read,write} plus the
#               direct arm's Δ ipc_direct_ops (a `-o interception=direct` mount: the client's own
#               device reads) against the fio ops and the FUSE READ/WRITE deltas (charter §7: a
#               bypass row whose ring and direct ops do not account for its I/O is INVALID; the
#               FUSE counters must stay flat) — and writes <row>.ipc.txt. results.tsv's columns
#               are the same in every mode.
# A block device must be unmounted and carry no signature (blkid): the lay-down overwrites it.
set -euo pipefail
T=$1 L=$2
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROWS=${MX_ROWS:-.} S=${MX_SIZE:-3g} RT=${MX_RUNTIME:-30}
ENGINE=${MX_ENGINE:-io_uring} PRELOAD=${MX_PRELOAD:-} PSYNC_CAP=${MX_PSYNC_MAX_JOBS:-$(nproc)}
OUT=${MX_OUT:-${SQZ_BUILD_ROOT:-/build}/perf/mx}; O=$OUT/$L; mkdir -p "$O"
die() { echo "matrix: $*" >&2; exit 1; }
case $ENGINE in io_uring|libaio|psync) ;; *) die "MX_ENGINE must be io_uring, libaio or psync (got '$ENGINE')" ;; esac
[ -z "$PRELOAD" ] || [ -r "$PRELOAD" ] || die "MX_PRELOAD=$PRELOAD is not readable"

# The lay-down is always io_uring; the rows take MX_ENGINE.
common=(--direct=1 --group_reporting --output-format=json)
pid="" devs=() LAY_JOBS=4
if [ -b "$T" ]; then
  findmnt -rno TARGET -S "$T" >/dev/null 2>&1 && die "$T is mounted"
  sig=$(blkid -p -o value -s TYPE "$T" 2>/dev/null || true)
  [ -z "$sig" ] || die "$T carries a '$sig' signature; refusing to overwrite it (wipefs it if it is scratch)"
  where=(--filename="$T" --size="$S" --offset_increment="$S")
  [ "$ENGINE" = io_uring ] || die "MX_ENGINE=$ENGINE is a mounted-directory mode (the regions of a raw device are laid out for 4 jobs)"
  devs=("$T")
elif [ -d "$T" ]; then
  if [ "$ENGINE" = psync ]; then
    LAY_JOBS=$PSYNC_CAP
    S=$(python3 -c 'import sys; s=sys.argv[1]; u={"k":1<<10,"m":1<<20,"g":1<<30,"t":1<<40}; n=int(s[:-1])*u[s[-1].lower()] if s[-1].lower() in u else int(s); print(4*n//int(sys.argv[2])//4096*4096)' "$S" "$PSYNC_CAP")
  fi
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
# MX_MIX=1 appends the mixed row (randrw 50/50, 4 KiB: one file read AND written by every job — the
# direct arm's lease turnover case, rung-10-client-direct-writes.md); not in the default set.
[ "${MX_MIX:-0}" = 1 ] && ROWTAB="$ROWTAB
rwmix4k-q32j1 randrw 4k 32 1"

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
        # Voluntary switches = parks that slept (the thread's sleeps-per-op instrument).
        vcs = 0
        try:
            for ln in open("/proc/%d/task/%s/status" % (pid, tid)):
                if ln.startswith("voluntary_ctxt_switches:"):
                    vcs = int(ln.split()[1]); break
        except OSError:
            pass
        s["tasks"][tid] = [comm, int(f[11]) + int(f[12]), vcs]
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
    cls = collections.defaultdict(lambda: [0.0, 0.0, 0, 0])
    tot_t = 0
    for tid, (comm, t1, v1) in b["tasks"].items():
        t0, v0 = a["tasks"].get(tid, [comm, 0, 0])[1:3]
        c = (t1 - t0) / hz / wall; tot_t += c
        k = re.sub(r"\d+$", "", comm); cls[k][0] += c; cls[k][1] = max(cls[k][1], c); cls[k][2] += v1 - v0; cls[k][3] += 1
    dcores = "%.2f" % tot_t
    top = " ".join("%s:%.2f(%d%%)" % (k, v[0], 100 * v[1]) for k, v in sorted(cls.items(), key=lambda kv: -kv[1][0])[:5])
    # The per-class side file: cores, threads, and the sleeps (voluntary switches) per second and per op.
    ops = iops if iops else 1
    with open(fj[:-5] + ".sched.txt", "w") as sf:
        sf.write("class\tthreads\tcores\tsleeps/s\tsleeps/op\tus_cpu/op\n")
        for k, v in sorted(cls.items(), key=lambda kv: -kv[1][0]):
            if v[0] < 0.005 and v[2] == 0: continue
            sf.write("%s\t%d\t%.2f\t%.0f\t%.3f\t%.2f\n" % (k, v[3], v[0], v[2] / wall, v[2] / wall / ops, v[0] * 1e6 / ops))
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

TCTL=$(for h in /sys/class/hwmon/hwmon*; do if [ "$(cat "$h/name" 2>/dev/null)" = k10temp ]; then echo "$h/temp1_input"; fi; done | head -1; true)
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

# The engagement line of a preloaded row on a SqueezeFS mount (header): ring ops vs fio ops, the
# FUSE data ops that still crossed the kernel, and how the ring served (fast path / handoff / dd).
engagement() {
  [ -n "$PRELOAD" ] && [ -n "$pid" ] || return 0
  # (the heredoc binds to python, not to tee: the row loop's stdin is the row table)
  python3 - "$O/$1.stats0.json" "$O/$1.stats1.json" "$O/$1.json" "$L" "$1" <<'EOF' | tee "$O/$1.ipc.txt"
import json, sys
a, b, fj, label, row = sys.argv[1:]
a, b = json.load(open(a)), json.load(open(b))
j = json.load(open(fj))["jobs"][0]
def flat(d, p=""):
    out = {}
    for k, v in d.items():
        if isinstance(v, dict): out.update(flat(v, p + k + "."))
        elif isinstance(v, (int, float)): out[p + k] = v
    return out
fa, fb = flat(a), flat(b)
def d(suffix):
    ks = [k for k in fb if k.endswith("." + suffix) or k == suffix]
    return sum(fb[k] - fa.get(k, 0) for k in ks)
ops = j["read"]["total_ios"] + j["write"]["total_ios"]
# The direct arm's reads (the clients' pages folded at teardown) and writes (their notes) are the
# engagement law's third and fourth terms.
direct = d("ipc_direct_ops") + d("ipc_direct_writes")
ring = d("ipc_ops_read") + d("ipc_ops_write") + direct
# FUSE READs are counted at the worker (inline serves never reach a handler); WRITEs at the handler,
# where a ring write's handoff runs too — those are ring ops, not kernel ones.
fuse = d("transport_ring_reads") + max(0, d("fuse_op_latency_ns.write.count") - d("ipc_write_handoffs"))
verdict = "ENGAGED" if ops and ring >= 0.99 * ops and fuse <= 0.01 * ops else "NOT ENGAGED (row INVALID as a bypass row)"
print("matrix: %s %s engagement: ring+direct %d of %d fio ops (%.1f%%; direct reads %d writes %d, leases %d granted / %d write / rechecks refused %d / recalls %d forced %d), FUSE READ+WRITE %d — %s; reads: fast_path %d handoffs %d dd %d; writes: dd_inline %d handoffs %d severs %d; sessions %d svc_threads %d dd_shards %d; wakes %d park_timeouts %d empty_passes %d cqe_wakes %d/%d collapsed; lane_gate_routes %d; rejects %d poisoned %d" % (
    label, row, ring, ops, 100.0 * ring / ops if ops else 0, d("ipc_direct_ops"), d("ipc_direct_writes"), d("ipc_leases_granted"), d("ipc_write_leases_granted"), d("ipc_direct_rechecks_failed"), d("ipc_write_lease_recalls"), d("ipc_write_leases_forced"), fuse, verdict,
    d("ipc_fast_path_serves"), d("ipc_read_handoffs"), d("ipc_dd_read_serves"),
    d("ipc_dd_inline_serves"), d("ipc_write_handoffs"), d("ipc_severs"),
    fb.get("ipc.ipc_sessions_active", fb.get("ipc_host.ipc_sessions_active", -1)), fb.get("ipc_host.ipc_service_threads", -1), fb.get("ipc_host.ipc_dd_shards", -1),
    d("ipc_service_wakes"), d("ipc_service_park_timeouts"), d("ipc_drain_empty_passes"), d("ipc_cqe_wake_writes"), d("ipc_cqe_wake_collapsed"),
    d("ipc_lane_gate_kernel_routes"), d("ipc_descriptor_rejects"), d("ipc_sessions_poisoned")))
EOF
}

if [ "${MX_LAYDOWN:-1}" = 1 ]; then
  cool "the lay-down"
  fio --name=mx --rw=write --bs=1m --iodepth=16 --numjobs="$LAY_JOBS" --ioengine=io_uring "${common[@]}" "${where[@]}" --output="$O/laydown.json"
  settle
  echo "matrix: $L lay-down $LAY_JOBS × $S done ($(python3 -c 'import json,sys; j=json.load(open(sys.argv[1]))["jobs"][0]; print("%.0f MiB/s" % (j["write"]["bw_bytes"] / 2**20))' "$O/laydown.json"))"
fi
[ "$ENGINE" = io_uring ] || echo "matrix: $L rows run on fio's $ENGINE engine${PRELOAD:+ under LD_PRELOAD=$PRELOAD}$([ "$ENGINE" = psync ] && echo " (depth 1; jobs = min(qd × nj, $PSYNC_CAP))")"
echo -e "label\trow\tIOPS\tMiB/s\tp50us\tp99us\tfio%\thost_cores\tdaemon_cores\tdev/user\tthreads\ttctl_c"
while read -r name rw bs qd nj; do
  [[ $name =~ $ROWS ]] || continue
  extra=(--randrepeat=0 --norandommap)
  case $rw in *write|randrw) extra+=(--end_fsync=1) ;; esac
  [ "$rw" = randrw ] && extra+=(--rwmixread=50)
  if [ "$ENGINE" = psync ]; then
    nj=$((qd * nj)); [ "$nj" -le "$PSYNC_CAP" ] || nj=$PSYNC_CAP; qd=1
  fi
  cool "$name"; tc0=$(tctl)
  # (cat, not cp: the virtual .stats has no size for cp's copy_file_range path to trust.)
  [ -n "$pid" ] && cat "$T/.stats" > "$O/$name.stats0.json"
  snap "$O/$name.s0"
  env ${PRELOAD:+LD_PRELOAD="$PRELOAD"} fio --name=mx --rw="$rw" --bs="$bs" --iodepth="$qd" --numjobs="$nj" --time_based --runtime="$RT" \
    --ioengine="$ENGINE" "${extra[@]}" "${common[@]}" "${where[@]}" --output="$O/$name.json" &
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
  engagement "$name"
done <<< "$ROWTAB"
