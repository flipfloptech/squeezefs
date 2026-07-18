#!/usr/bin/env bash
# [SUPERSEDED 2026-07-18, PR 5/N5] productized as tests/nvmeof_target_substrate.sh snapshot (stable-sections residue witness) — kept as evidence lineage; do not extend.
# spdkscope box-state snapshot — run before AND after the A/B rig to prove
# zero residue. Read-only. Usage: sudo ./snapshot.sh <label> (writes
# /tmp/spdkscope/snapshot-<label>.txt)
set -u
LABEL="${1:-unlabeled}"
OUT="/tmp/spdkscope/snapshot-${LABEL}.txt"
mkdir -p /tmp/spdkscope
{
    echo "=== spdkscope snapshot: ${LABEL} @ $(date -Is) ==="
    echo "--- kernel/cpu ---"
    uname -r
    grep -m1 'model name' /proc/cpuinfo
    nproc
    cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null
    cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq 2>/dev/null
    echo "--- thermal (Tctl) ---"
    sensors 2>/dev/null | grep -i -E 'tctl|tdie' || echo "sensors n/a"
    echo "--- memory ---"
    free -m | head -3
    echo "--- hugepages ---"
    cat /proc/sys/vm/nr_hugepages
    for d in /sys/kernel/mm/hugepages/hugepages-*; do
        echo "$d: nr=$(cat "$d/nr_hugepages") free=$(cat "$d/free_hugepages")"
    done
    grep -E 'Huge' /proc/meminfo
    echo "--- hugetlbfs mounts ---"
    mount | grep -i huge || echo "(none)"
    echo "--- block devices ---"
    lsblk -o NAME,SIZE,TYPE,MOUNTPOINTS -e 7 2>/dev/null
    echo "--- nvme namespaces ---"
    ls -1 /dev/nvme* 2>/dev/null || echo "(none)"
    echo "--- nvme ctrl list (subsysnqn) ---"
    for c in /sys/class/nvme/nvme*; do
        [ -e "$c" ] || continue
        echo "$(basename "$c"): $(cat "$c/subsysnqn" 2>/dev/null) [$(cat "$c/transport" 2>/dev/null)] addr=$(cat "$c/address" 2>/dev/null)"
    done
    echo "--- zram ---"
    zramctl 2>/dev/null || echo "(zramctl n/a)"
    ls -1 /dev/zram* 2>/dev/null || echo "(no zram nodes)"
    echo "--- null_blk configfs ---"
    ls /sys/kernel/config/nullb/ 2>/dev/null || echo "(none)"
    echo "--- loop ---"
    losetup -a 2>/dev/null || true
    echo "--- nvmet configfs subsystems ---"
    ls /sys/kernel/config/nvmet/subsystems/ 2>/dev/null || echo "(nvmet configfs absent)"
    echo "--- nvmet configfs ports ---"
    for p in /sys/kernel/config/nvmet/ports/*; do
        [ -e "$p" ] || continue
        echo "port $(basename "$p"): trtype=$(cat "$p/addr_trtype" 2>/dev/null) traddr=$(cat "$p/addr_traddr" 2>/dev/null) trsvcid=$(cat "$p/addr_trsvcid" 2>/dev/null) subsystems=[$(ls "$p/subsystems" 2>/dev/null | tr '\n' ' ')]"
    done
    echo "--- spdk processes ---"
    pgrep -af 'spdk|nvmf_tgt|spdk_tgt' || echo "(none)"
    echo "--- squeezefs/juicefs mounts+procs ---"
    mount | grep -E 'squeezefs|juicefs' || echo "(no such mounts)"
    pgrep -af 'squeezefs|juicefs' || echo "(no such procs)"
    echo "--- docker containers (do-not-touch inventory) ---"
    docker ps --format '{{.Names}}: {{.Image}}' 2>/dev/null || echo "(docker n/a)"
    echo "--- listening tcp 4420-4699 ---"
    ss -ltn 2>/dev/null | awk 'NR==1 || $4 ~ /:(44[0-9][0-9]|45[0-9][0-9]|46[0-9][0-9])$/' 
    echo "=== end snapshot ${LABEL} ==="
} > "$OUT" 2>&1
echo "wrote $OUT"
