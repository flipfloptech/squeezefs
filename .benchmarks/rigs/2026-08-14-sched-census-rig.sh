#!/usr/bin/env bash
# Scheduler-truth census: per-thread-class runqueue-delay attribution during an il row.
# /proc/<tid>/schedstat: cputime_ns waittime_ns timeslices — waittime is THE run-delay term.
set -euo pipefail
BIN=$1; SHIM=$2; TAG=$3; NJ=${4:-32}; QD=${5:-8}
REPO=/home/justin/Source/squeezefs
META="sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1"
DATA="sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1"

umount -f /mnt/squeezefs 2>/dev/null || true
for p in $(pgrep -x squeezefs); do kill -9 "$p" 2>/dev/null || true; done
sleep 1
SQZ_DEVSUB_TRANSPORT=tcp "$REPO/tests/dev_substrate.sh" teardown >/dev/null 2>&1 || true
sleep 1
SQZ_DEVSUB_TRANSPORT=tcp "$REPO/tests/dev_substrate.sh" create >/dev/null 2>&1
sleep 3
rm -rf /mnt/squeezefs/* 2>/dev/null || true
"$BIN" format "$META" "$DATA" --force >/dev/null 2>&1
ok=""
for i in 1 2 3 4 5; do
  if "$BIN" mount "$META" /mnt/squeezefs --daemon --allow-other -o interception >/dev/null 2>&1; then
    sleep 2; mountpoint -q /mnt/squeezefs && { ok=1; break; }
  fi
  sleep 2
done
[ -n "$ok" ] || { echo "$TAG MOUNT-FAIL"; exit 1; }
DPID=$(pgrep -x "$(basename "$BIN")" | head -1 || true)
[ -n "$DPID" ] || { echo "$TAG NO-DAEMON-PID"; exit 1; }

size_mb=$(( 4096 / NJ )); [ "$size_mb" -lt 128 ] && size_mb=128
# Pre-fill
LD_PRELOAD="$SHIM" fio --name=w --directory=/mnt/squeezefs --size=${size_mb}m --bs=1M --rw=write \
  --numjobs="$NJ" --iodepth=4 --ioengine=libaio --direct=1 --group_reporting >/dev/null 2>&1 || true
sync; sleep 2

# Launch the measured row in background; census threads mid-row.
LD_PRELOAD="$SHIM" fio --name=w --directory=/mnt/squeezefs --size=${size_mb}m --bs=4k --rw=randwrite \
  --numjobs="$NJ" --iodepth="$QD" --ioengine=libaio --direct=1 --time_based --runtime=25 \
  --group_reporting > /tmp/${TAG}_fio.txt 2>/dev/null &
FIO_BG=$!
sleep 6

snap() {
  # class tid cputime wait slices comm
  for t in /proc/$DPID/task/*; do
    tid=${t##*/}
    read c w s < "$t/schedstat" 2>/dev/null || continue
    comm=$(cat "$t/comm" 2>/dev/null || echo "?")
    echo "daemon $tid $c $w $s $comm"
  done
  for fp in $(pgrep -x fio); do
    for t in /proc/$fp/task/*; do
      tid=${t##*/}
      read c w s < "$t/schedstat" 2>/dev/null || continue
      comm=$(cat "$t/comm" 2>/dev/null || echo "?")
      echo "fio $tid $c $w $s $comm"
    done
  done
}
snap > /tmp/${TAG}_s0.txt
sleep 10
snap > /tmp/${TAG}_s1.txt
wait $FIO_BG || true
grep -oE "IOPS=[0-9.k]+" /tmp/${TAG}_fio.txt | head -1

python3 - "$TAG" <<'EOF'
import sys, collections
tag=sys.argv[1]
def load(p):
    d={}
    for line in open(p):
        f=line.split()
        if len(f)<6: continue
        d[(f[0],f[1])]=(int(f[2]),int(f[3]),int(f[4])," ".join(f[5:]))
    return d
a=load(f"/tmp/{tag}_s0.txt"); b=load(f"/tmp/{tag}_s1.txt")
cls=collections.defaultdict(lambda:[0,0,0,0])  # cpu, wait, slices, nthreads
for k,(c1,w1,s1,comm) in b.items():
    if k not in a: continue
    c0,w0,s0,_=a[k]
    dc,dw,ds=c1-c0,w1-w0,s1-s0
    if dc==0 and dw==0: continue
    # classify daemon threads by comm prefix
    grp=k[0]
    if grp=="daemon":
        base=comm.split("/")[0].rstrip("0123456789")
        grp=f"daemon:{base}"
    e=cls[grp]; e[0]+=dc; e[1]+=dw; e[2]+=ds; e[3]+=1
print(f"== {tag}: per-class scheduler ledger over 10s (active threads only) ==")
print(f"{'class':28} {'thr':>4} {'cpu_ms':>9} {'runq_wait_ms':>12} {'slices':>9} {'wait/slice_us':>13}")
for g,(c,w,s,n) in sorted(cls.items(), key=lambda x:-x[1][1]):
    print(f"{g:28} {n:>4} {c/1e6:>9.0f} {w/1e6:>12.0f} {s:>9} {(w/s/1e3 if s else 0):>13.1f}")
EOF

umount -f /mnt/squeezefs 2>/dev/null || true
for p in $(pgrep -x squeezefs); do kill -9 "$p" 2>/dev/null || true; done
sleep 1
