#!/usr/bin/env bash
# spdkscope teardown — removes ONLY manifest-recorded objects, restores
# hugepages to the recorded prior value. Safe to re-run.
set -uo pipefail
STATE=/tmp/spdkscope/state
NVMET_CFS=/sys/kernel/config/nvmet
log() { echo "[teardown $(date +%H:%M:%S)] $*"; }
[ "$(id -u)" = 0 ] || { echo "run as root"; exit 1; }
[ -f "$STATE/manifest" ] || { log "no manifest — nothing to tear down"; exit 0; }

# 0. unmount any spdkscope squeezefs mount
if mountpoint -q /tmp/spdkscope/mnt 2>/dev/null; then
    umount /tmp/spdkscope/mnt || fusermount3 -u /tmp/spdkscope/mnt || true
    sleep 1
fi
pkill -f "squeezefs mount sqmeta:///dev/nvme.*spdkscope" 2>/dev/null

# 1. disconnect our nvme controllers (by nqn, ours only)
grep '^connected=' "$STATE/manifest" | cut -d= -f2 | sort -u | while read -r nqn; do
    nvme disconnect -n "$nqn" >/dev/null 2>&1 && log "disconnected $nqn"
done
sleep 1

# 2. kill spdk_tgt by recorded pid
if [ -f "$STATE/spdk_tgt.pid" ]; then
    SPDK_PID=$(cat "$STATE/spdk_tgt.pid")
    if kill -0 "$SPDK_PID" 2>/dev/null; then
        kill "$SPDK_PID"; for i in $(seq 1 20); do kill -0 "$SPDK_PID" 2>/dev/null || break; sleep 0.5; done
        kill -9 "$SPDK_PID" 2>/dev/null
        log "spdk_tgt pid=$SPDK_PID stopped"
    fi
fi

# 3. kernel nvmet objects (ours only, from manifest)
grep '^nvmet_port=' "$STATE/manifest" | cut -d= -f2 | while read -r pid_; do
    P="$NVMET_CFS/ports/$pid_"
    [ -d "$P" ] || continue
    rm -f "$P"/subsystems/* 2>/dev/null
    rmdir "$P" 2>/dev/null && log "removed nvmet port $pid_"
done
grep '^nvmet_subsystem=' "$STATE/manifest" | cut -d= -f2 | while read -r nqn; do
    S="$NVMET_CFS/subsystems/$nqn"
    [ -d "$S" ] || continue
    for ns in "$S"/namespaces/*; do
        [ -d "$ns" ] || continue
        echo 0 > "$ns/enable" 2>/dev/null
        rmdir "$ns" 2>/dev/null
    done
    rmdir "$S" 2>/dev/null && log "removed nvmet subsystem $nqn"
done

# 4. zram devices (ours only; never index 0)
grep '^zram=' "$STATE/manifest" | sed 's/^zram=//' | awk '{print $1}' | while read -r idx; do
    [ "$idx" != "0" ] || { log "refusing zram0"; continue; }
    [ -b "/dev/zram$idx" ] || continue
    echo 1 > "/sys/block/zram$idx/reset" 2>/dev/null
    echo "$idx" > /sys/class/zram-control/hot_remove 2>/dev/null && log "removed zram$idx"
done

# 5. modules we loaded (best-effort, only if now unused)
grep '^module_loaded=' "$STATE/manifest" | cut -d= -f2 | tac | while read -r m; do
    modprobe -r "${m//_/-}" 2>/dev/null && log "unloaded module $m"
done

# 6. hugepages back to prior
if [ -f "$STATE/hugepages-prior" ]; then
    PRIOR=$(cat "$STATE/hugepages-prior")
    echo "$PRIOR" > /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
    log "hugepages restored to $PRIOR (now: $(cat /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages))"
fi

log "teardown complete"
