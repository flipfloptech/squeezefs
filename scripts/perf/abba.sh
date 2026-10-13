#!/usr/bin/env bash
# abba.sh <label> <A-binary> <B-binary> [phases] [posture] — the same-mount A-B-B-A: each leg reformats
# the raw targets (scripts/perf/reset.sh), mounts the leg's binary and runs scripts/perf/table.sh;
# one line per leg and metric. Environment as reset.sh (SQZ_TGT / SQZ_STATE for perf_mount.sh's set).
set -uo pipefail
H=$(cd "$(dirname "$0")" && pwd)
L=$1; A=$2; B=$3; P=${4:-mdtest,randread,randwrite}; POST=${5:-sym2}
i=0
for arm in A B B A; do
  i=$((i + 1)); bin=$A; [ $arm = B ] && bin=$B
  "$H/reset.sh" "$POST" "$bin" > /dev/null || { echo "leg $i: reset failed"; exit 1; }
  "$H/table.sh" /mnt/sqz "$L-leg$i-$arm" "$P" 2>&1 | sed "s/^/leg$i $arm /"
done
