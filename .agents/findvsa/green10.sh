#!/bin/bash
# FIND-VS-A acceptance: SIGBUS repro shape green xN on the given mask.
# Usage: green10.sh <rounds> [taskset-mask]
set -u
N="${1:-10}"
MASK="${2:-}"
REPO=/home/justin/Source/squeezefs
PASS=0
for i in $(seq 1 "$N"); do
    if [ -n "$MASK" ]; then
        out=$(taskset -c "$MASK" "$REPO/.agents/findvsa/repro.sh" 2>&1)
    else
        out=$("$REPO/.agents/findvsa/repro.sh" 2>&1)
    fi
    sig=$(echo "$out" | grep -o "SIGBUS-REPRO: .*")
    mis=$(echo "$out" | grep -o "missing files: .*")
    echo "round $i: $sig | $mis"
    echo "$out" | grep -q "SIGBUS-REPRO: NO" && echo "$out" | grep -q "missing files: 0" && PASS=$((PASS + 1))
done
echo "GREEN $PASS/$N (mask=${MASK:-full})"
[ "$PASS" -eq "$N" ]
