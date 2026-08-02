#!/bin/bash
# zrow.sh — one Phase-1 bracket row on the client (runs ON squeeze-test).
#   zrow.sh <label> <mode:copy|zcrx> <threads> <conns_per_thread> <secs> [sender_host]
# Receiver here, sender triggered over ssh on the storage node. Collects:
# fio-style GB/s (receiver RESULT line), mpstat whole-box CPU, perf uncore
# CAS DRAM bytes, per-queue rx byte deltas (engagement), rusage CPU.
set -u
LABEL=$1; MODE=$2; THREADS=$3; CONNS=$4; SECS=$5; SNODE=${6:-10.181.177.193}
IF=ens1f0np0
ET=/scratch/tmp/kernel-sqz/ethtool-sqz
D=/scratch/tmp/zcrx/rows/$LABEL
PORT_BASE=5301
QID_BASE=24
AREA_MB=${AREA_MB:-512}
BUF_KB=${BUF_KB:-1024}
BS_KB=${BS_KB:-1024}
mkdir -p "$D"

echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) zcrx-row: START label=$LABEL mode=$MODE threads=$THREADS conns=$CONNS secs=$SECS sender=$SNODE" >> /scratch/tmp/agent_runs.log

# engagement: per-queue rx bytes before
$ET -S $IF | grep -E "rx[0-9]+_bytes" > "$D/qbytes.before"
cat /proc/net/softnet_stat > "$D/softnet.before" || true

# receiver
EXTRA=""
if [ "$MODE" = zcrx ]; then EXTRA="--qid-base $QID_BASE --area-mb $AREA_MB"; fi
/scratch/tmp/zcrx/zcrx_bench --mode "$MODE" --threads "$THREADS" --conns "$CONNS" \
  --port $PORT_BASE --cpu-base 2 --ifname $IF --buf-kb "$BUF_KB" $EXTRA \
  --max-secs $((SECS + 60)) > "$D/recv.out" 2> "$D/recv.err" &
RPID=$!
sleep 2
if ! kill -0 $RPID 2>/dev/null; then
  echo "receiver died at startup:"; cat "$D/recv.err"
  echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) zcrx-row: FAIL label=$LABEL receiver-startup" >> /scratch/tmp/agent_runs.log
  exit 2
fi

# samplers (whole box)
mpstat -P ALL 1 "$SECS" > "$D/mpstat.out" 2>&1 &
MPID=$!
perf stat -a -e uncore_imc/cas_count_read/,uncore_imc/cas_count_write/ \
  -o "$D/uncore.out" -- sleep "$SECS" &
PPID_PERF=$!

# sender(s) (ephemeral processes on storage node(s); journaled by this row)
IFS=, read -ra NODES <<< "$SNODE"
NN=${#NODES[@]}
PPN=$(( THREADS / NN ))
SPIDS=()
for ni in $(seq 0 $((NN - 1))); do
  ssh -o BatchMode=yes root@"${NODES[$ni]}" \
    "/tmp/zcrx_send --host 10.181.177.194 --port-base $((PORT_BASE + ni * PPN)) --ports $PPN --conns-per-port $CONNS --secs $SECS --bs-kb $BS_KB" \
    > "$D/send.$ni.out" 2>&1 &
  SPIDS+=($!)
done
for sp in "${SPIDS[@]}"; do wait "$sp"; done
cat "$D"/send.*.out > "$D/send.out"
wait $RPID; RRC=$?
wait $MPID 2>/dev/null
wait $PPID_PERF 2>/dev/null

$ET -S $IF | grep -E "rx[0-9]+_bytes" > "$D/qbytes.after"

# summarize
RES=$(grep ^RESULT "$D/recv.out" || echo "RESULT missing")
SND=$(grep -h ^SENDER-DONE "$D/send.out" | awk {b+=+0} {print} END{} | tr "\n" ";")
CPU=$(awk '/Average:/ && $2=="all" {printf "usr=%s sys=%s soft=%s idle=%s", $3, $5, $8, $NF}' "$D/mpstat.out")
# engagement: top queue deltas
paste "$D/qbytes.before" "$D/qbytes.after" | awk '{d=$4-$2; if (d>1e8) printf "  %s delta=%.2fGB\n", $1, d/1e9}' > "$D/qdeltas.txt"
DRAM=$(awk '/cas_count/ {printf "%s=%sMiB ", $3, $1}' "$D/uncore.out" 2>/dev/null)

echo "== $LABEL =="
echo "$RES"
echo "$SND"
echo "cpu(all): $CPU"
echo "dram: $DRAM"
echo "queue deltas:"; cat "$D/qdeltas.txt"
echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) zcrx-row: DONE label=$LABEL rc=$RRC | $RES | cpu $CPU" >> /scratch/tmp/agent_runs.log
exit $RRC
