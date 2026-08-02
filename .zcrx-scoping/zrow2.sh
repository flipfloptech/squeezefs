#!/bin/bash
# zrow2.sh — dual-port row: one receiver process per NIC port, NUMA-matched pins.
#   zrow2.sh <label> <mode> <threads_per_port> <conns_per_thread> <secs>
# Port A: ens1f0np0 (node0, even cpus), sender .193 -> 10.181.177.194, ports 5301+
# Port B: ens2f0np0 (node1, odd cpus),  sender .195 -> 10.181.178.194, ports 5311+
set -u
LABEL=$1; MODE=$2; TPP=$3; CONNS=$4; SECS=$5
ET=/scratch/tmp/kernel-sqz/ethtool-sqz
D=/scratch/tmp/zcrx/rows/$LABEL
AREA_MB=${AREA_MB:-512}
mkdir -p "$D"
echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) zcrx-row2: START label=$LABEL mode=$MODE tpp=$TPP conns=$CONNS secs=$SECS" >> /scratch/tmp/agent_runs.log

$ET -S ens1f0np0 | grep -E "rx[0-9]+_bytes" > "$D/qbytes.A.before"
$ET -S ens2f0np0 | grep -E "rx[0-9]+_bytes" > "$D/qbytes.B.before"

EXTRA=""
[ "$MODE" = zcrx ] && EXTRA="--qid-base 24 --area-mb $AREA_MB"
/scratch/tmp/zcrx/zcrx_bench --mode "$MODE" --threads "$TPP" --conns "$CONNS" \
  --port 5301 --cpu-base 2 --ifname ens1f0np0 $EXTRA \
  --max-secs $((SECS + 60)) > "$D/recv.A.out" 2> "$D/recv.A.err" &
RA=$!
/scratch/tmp/zcrx/zcrx_bench --mode "$MODE" --threads "$TPP" --conns "$CONNS" \
  --port 5311 --cpu-base 3 --ifname ens2f0np0 $EXTRA \
  --max-secs $((SECS + 60)) > "$D/recv.B.out" 2> "$D/recv.B.err" &
RB=$!
sleep 2
for p in $RA $RB; do
  if ! kill -0 $p 2>/dev/null; then
    echo "receiver $p died:"; cat "$D"/recv.*.err
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) zcrx-row2: FAIL label=$LABEL receiver-startup" >> /scratch/tmp/agent_runs.log
    kill $RA $RB 2>/dev/null
    exit 2
  fi
done

mpstat -P ALL 1 "$SECS" > "$D/mpstat.out" 2>&1 &
MPID=$!
perf stat -a -e uncore_imc/cas_count_read/,uncore_imc/cas_count_write/ \
  -o "$D/uncore.out" -- sleep "$SECS" &
UPID=$!

ssh -o BatchMode=yes root@10.181.177.193 \
  "/tmp/zcrx_send --host 10.181.177.194 --port-base 5301 --ports $TPP --conns-per-port $CONNS --secs $SECS --bs-kb 1024" \
  > "$D/send.A.out" 2>&1 &
SA=$!
ssh -o BatchMode=yes root@10.181.177.195 \
  "/tmp/zcrx_send --host 10.181.178.194 --port-base 5311 --ports $TPP --conns-per-port $CONNS --secs $SECS --bs-kb 1024" \
  > "$D/send.B.out" 2>&1 &
SB=$!
wait $SA; wait $SB
wait $RA; RCA=$?
wait $RB; RCB=$?
wait $MPID 2>/dev/null
wait $UPID 2>/dev/null

$ET -S ens1f0np0 | grep -E "rx[0-9]+_bytes" > "$D/qbytes.A.after"
$ET -S ens2f0np0 | grep -E "rx[0-9]+_bytes" > "$D/qbytes.B.after"

echo "== $LABEL =="
grep -h ^RESULT "$D"/recv.*.out
grep -h ^SENDER-DONE "$D"/send.*.out
CPU=$(awk '/Average:/ && $2=="all" {printf "usr=%s sys=%s irq=%s soft=%s iow=%s idle=%s", $3, $5, $7, $8, $6, $NF}' "$D/mpstat.out")
echo "cpu(all): $CPU"
awk '/cas_count/ {printf "dram %s = %s MiB\n", $3, $1}' "$D/uncore.out"
for P in A B; do
  echo "port $P queue deltas:"
  paste "$D/qbytes.$P.before" "$D/qbytes.$P.after" | awk '{d=$4-$2; if (d>1e8) printf "  %s delta=%.2fGB\n", $1, d/1e9}'
done
GBPS=$(grep -h ^RESULT "$D"/recv.*.out | awk '{for(i=1;i<=NF;i++) if ($i ~ /^GBps=/) {sub("GBps=","",$i); s+=$i}} END {print s}')
echo "TOTAL GBps=$GBPS"
echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) zcrx-row2: DONE label=$LABEL rcA=$RCA rcB=$RCB total_GBps=$GBPS | cpu $CPU" >> /scratch/tmp/agent_runs.log
[ $RCA -eq 0 ] && [ $RCB -eq 0 ]
