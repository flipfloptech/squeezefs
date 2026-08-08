#!/usr/bin/env bash
# Drain-funnel Phase-1 profiler leg: R0 shape (defaults, 32×32) with a
# 10 s perf record scoped to the sqz-ipc-svc threads mid-row (+ a 10 s
# off-CPU sample via perf sched or /proc wchan fallback). One-shot.
set -u
PAIR_T="${PAIR_T:-/home/justin/Source/.sqz-drainfunnel-target}"
META="${META:-sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1}"
DATA="${DATA:-sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1}"
MNT="${MNT:-/mnt/sqz-drainfunnel}"
OUT="${OUT:-/tmp/drainfunnel-0808/perf}"
BIN="$PAIR_T/release/squeezefs"
SHIM="$PAIR_T/preload-release/libsqueezefs_il.so"
mkdir -p "$OUT"
fatal() { echo "FATAL: $*" >&2; exit 1; }
my_daemon() { pgrep -f "squeezefs mount.*$MNT" 2>/dev/null; }

mountpoint -q "$MNT" && fatal "$MNT already mounted"
"$BIN" format "$META" "$DATA" --force >"$OUT/format.log" 2>&1 || fatal format
mkdir -p "$MNT"
SQUEEZEFS_DIRECT_DEVICE_TRUE=1 "$BIN" mount "$META" "$MNT" --daemon \
  --allow-other --interception --log-file "$OUT/mount.log" >/dev/null 2>&1
for _ in $(seq 1 60); do mountpoint -q "$MNT" && cat "$MNT/.stats" >/dev/null 2>&1 && break; sleep 1; done
mkdir -p "$MNT/d"
fio --name=pf --directory="$MNT/d" --nrfiles=1 --filesize=128m --numjobs=32 \
  --rw=write --bs=1M --direct=1 --ioengine=libaio --iodepth=8 \
  --group_reporting --fallocate=none >/dev/null 2>&1 || fatal prefill

( sleep 15
  # Resolve the LIVE daemon + its svc tids mid-row (spawn-on-bind: the
  # threads exist only after the client binds). Thread-name scan over
  # /proc — comm == "squeezefs" processes whose cmdline names OUR mount.
  PID=""
  TIDS=""
  for p in $(ps -eo pid=,comm= | awk '$2=="squeezefs"{print $1}'); do
    tr '\0' ' ' < /proc/$p/cmdline 2>/dev/null | grep -q "$MNT" || continue
    t=$(ps -T -p "$p" -o spid=,comm= 2>/dev/null | awk '$2 ~ /sqz-ipc-svc/ {printf "%s,", $1}' | sed 's/,$//')
    if [ -n "$t" ]; then PID=$p; TIDS=$t; break; fi
  done
  { echo "PROF: pid=${PID:-none} tids=${TIDS:-none}";
    ps -eo pid=,comm= | awk '$2=="squeezefs"'; } > "$OUT/prof-resolve.txt"
  [ -n "$TIDS" ] || { echo "PROF: no svc tids" > "$OUT/svc-wchan.txt"; exit 0; }
  perf record -g --call-graph dwarf -o "$OUT/svc.perf" -t "$TIDS" -- sleep 10
  # off-CPU attribution: sample state+wchan of svc threads for 10 s
  for i in $(seq 1 100); do
    for t in ${TIDS//,/ }; do
      s=$(awk '{print $3}' /proc/$PID/task/$t/stat 2>/dev/null)
      w=$(cat /proc/$PID/task/$t/wchan 2>/dev/null)
      echo "$t $s $w"
    done
    sleep 0.1
  done > "$OUT/svc-wchan.txt"
) & PROF=$!

LD_PRELOAD="$SHIM" fio --name=pf --directory="$MNT/d" --nrfiles=1 \
  --filesize=128m --numjobs=32 --rw=randread --bs=4k --direct=1 \
  --randrepeat=0 --ioengine=libaio --iodepth=32 --time_based \
  --runtime=45 --ramp_time=5 --group_reporting \
  --output-format=json --output="$OUT/fio.json" >/dev/null 2>&1 || fatal fio
wait $PROF 2>/dev/null || true

# Engagement gate (the skewed-pair lesson: a silent-passthrough profile
# row profiles NOTHING): the row must have moved ring ops.
python3 - "$MNT/.stats" <<'PY' || { "$BIN" umount "$MNT" >/dev/null 2>&1; fatal "perf leg ran PASSTHROUGH (0 ring ops) — pair skew?"; }
import json, sys
m = json.load(open(sys.argv[1])).get("metrics", {})
ops = int(m.get("ipc_ops_read", 0))
print(f"perf leg ipc_ops_read={ops}")
sys.exit(0 if ops > 1_000_000 else 1)
PY
"$BIN" umount "$MNT" >/dev/null 2>&1 || true
for _ in $(seq 1 60); do mountpoint -q "$MNT" || break; sleep 1; done
echo "PERF LEG DONE: $OUT/svc.perf"
