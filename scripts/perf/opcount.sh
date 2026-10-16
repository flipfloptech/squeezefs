#!/usr/bin/env bash
# opcount.sh <label> <command…> — FUSE requests by opcode (kernel tracepoint fuse_request_send) and the
# mean kernel-side round trip per opcode (send → end) while <command> runs.
set -uo pipefail
L=$1; shift
TR=/sys/kernel/tracing
[ -d $TR/events/fuse ] || mount -t tracefs none $TR
echo 0 > $TR/tracing_on; echo > $TR/trace; echo 65536 > $TR/buffer_size_kb
echo 1 > $TR/events/fuse/fuse_request_send/enable; echo 1 > $TR/events/fuse/fuse_request_end/enable
echo 1 > $TR/tracing_on
"$@"
echo 0 > $TR/tracing_on
echo 0 > $TR/events/fuse/fuse_request_send/enable; echo 0 > $TR/events/fuse/fuse_request_end/enable
mkdir -p ${SQZ_BUILD_ROOT:-/build}/perf; cp $TR/trace ${SQZ_BUILD_ROOT:-/build}/perf/trace-$L.txt
python3 - ${SQZ_BUILD_ROOT:-/build}/perf/trace-$L.txt <<'EOF'
import re, sys, collections
send = {}; cnt = collections.Counter(); rtt = collections.defaultdict(list)
for line in open(sys.argv[1]):
    m = re.search(r'\s(\d+\.\d+): fuse_request_(send|end): connection \d+ req (\d+)(?: opcode \d+ \((\w+)\))?', line)
    if not m: continue
    t = float(m.group(1)); kind = m.group(2); req = m.group(3)
    if kind == 'send':
        op = m.group(4); cnt[op] += 1; send[req] = (t, op)
    elif req in send:
        t0, op = send.pop(req); rtt[op].append(t - t0)
for op, c in cnt.most_common():
    r = rtt.get(op, [])
    r.sort()
    print("%-14s %8d  rtt mean %7.1f us  p50 %7.1f us" % (op, c, 1e6 * sum(r) / max(len(r), 1), 1e6 * r[len(r) // 2] if r else 0))
EOF
