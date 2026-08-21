#!/usr/bin/env bash
# nic_prep.sh — client-local NIC steering for the zcrx bracket (RECORD + apply).
#   nic_prep.sh <nports>   — steer bench dst-ports 5301..5300+n -> queues 24..23+n
# All state recorded to /scratch/tmp/zcrx/nic_state_before.txt; nic_restore.sh undoes.
set -eu
N=${1:-1}
IF=ens1f0np0
ET=/scratch/tmp/kernel-sqz/ethtool-sqz
S=/scratch/tmp/zcrx/nic_state_before.txt

if [ ! -f "$S" ]; then
  {
    echo "== recorded $(date -u +%Y-%m-%dT%H:%M:%SZ) =="
    echo "-- features --";   $ET -k $IF | grep -E "ntuple"
    echo "-- rxfh --";       $ET -x $IF
    echo "-- rules --";      $ET -n $IF
    echo "-- ring/hds --";   $ET -g $IF | grep -iE "data split|hds"
  } > "$S"
  echo "recorded prior state -> $S"
fi

$ET -K $IF ntuple on
# RSS off the ZC queues: restrict indirection to queues 0..23
$ET -X $IF equal 24
for loc in $(seq 1 8); do $ET -N $IF delete $loc 2>/dev/null || true; done
for i in $(seq 0 $((N - 1))); do
  $ET -N $IF flow-type tcp4 dst-port $((5301 + i)) action $((24 + i)) loc $((i + 1))
done
echo "applied: ntuple on, RSS equal 24, $N rule(s) 5301..$((5300 + N)) -> q24..$((23 + N))"
$ET -n $IF
echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) zcrx-lane agent: NIC-PREP $IF ntuple=on rss=equal24 rules=$N (ports 5301+ -> q24+); prior state recorded, restore via nic_restore.sh" >> /scratch/tmp/agent_runs.log
