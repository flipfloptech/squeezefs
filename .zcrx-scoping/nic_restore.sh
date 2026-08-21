#!/usr/bin/env bash
# nic_restore.sh — undo every nic_prep.sh change on the client (to recorded state).
set -u
IF=ens1f0np0
ET=/scratch/tmp/kernel-sqz/ethtool-sqz
# delete any bench rules (locs 1..8)
for loc in $(seq 1 8); do $ET -N $IF delete $loc 2>/dev/null; done
$ET -X $IF default
$ET -K $IF ntuple off
echo "-- verify --"
$ET -k $IF | grep ntuple
$ET -n $IF
$ET -x $IF | head -6
echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) zcrx-lane agent: NIC-RESTORE $IF rules deleted, rss default, ntuple off (matches recorded prior state)" >> /scratch/tmp/agent_runs.log
