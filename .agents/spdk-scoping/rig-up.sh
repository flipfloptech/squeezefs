#!/usr/bin/env bash
# spdkscope A/B rig — SPDK NVMe/TCP target vs kernel nvmet-tcp vs nvmet-loop
# (reference), all zram-backed, everything namespaced "spdkscope".
#
# Ownership conventions (dev_substrate style): every object this script
# creates carries the spdkscope prefix or is recorded by exact id in the
# manifest ($STATE/manifest). Teardown removes ONLY manifest entries.
# NEVER touches: zram0 (user swap), nullb0, foreign nvmet trees, user mounts.
#
# Hugepages: records the prior 2M-page count, reserves $HUGEPAGES_2M (1024
# = 2 GiB <= the 4 GiB cap), teardown restores the recorded value.
set -euo pipefail

SPDK_DIR=/var/tmp/spdk-scoping/spdk
STATE=/tmp/spdkscope/state
RESULTS=/tmp/spdkscope/results
RPC="$SPDK_DIR/scripts/rpc.py -s /tmp/spdkscope/spdk.sock"
RPCSOCK=/tmp/spdkscope/spdk.sock
HUGEPAGES_2M=1024          # 2 GiB
SPDK_CORE_MASK=0x1000000   # core 24 (1 reactor core; per-core honesty baseline)
SPDK_MEM_MB=1024
PORT_SPDK=4460
PORT_NVMET=4461
NVMET_PORT_ID_TCP=52470
NVMET_PORT_ID_LOOP=52471
NQN_SPDK="nqn.2026-07.io.spdkscope:bench-spdk"
NQN_SPDK_GUARD="nqn.2026-07.io.spdkscope:guard-spdk"
NQN_NVMET="nqn.2026-07.io.spdkscope:bench-nvmet"
NQN_LOOP="nqn.2026-07.io.spdkscope:bench-loop"
NVMET_CFS=/sys/kernel/config/nvmet

log() { echo "[rig-up $(date +%H:%M:%S)] $*" >&2; }
die() { echo "[rig-up FATAL] $*" >&2; exit 1; }
manifest() { echo "$1" >> "$STATE/manifest"; }

[ "$(id -u)" = 0 ] || die "run as root"
[ -x "$SPDK_DIR/build/bin/spdk_tgt" ] || die "spdk_tgt not built at $SPDK_DIR"
mkdir -p "$STATE" "$RESULTS" /tmp/spdkscope/mnt
: > "$STATE/manifest"

# --- hugepages (record prior, reserve, verify) -----------------------------
PRIOR_HP=$(cat /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages)
echo "$PRIOR_HP" > "$STATE/hugepages-prior"
manifest "hugepages_prior=$PRIOR_HP"
echo "$HUGEPAGES_2M" > /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
GOT=$(cat /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages)
[ "$GOT" -ge "$HUGEPAGES_2M" ] || die "hugepage reservation failed (wanted $HUGEPAGES_2M got $GOT)"
log "hugepages: prior=$PRIOR_HP now=$GOT (2M pages)"

# --- modules (record which we newly load) ----------------------------------
for m in nvmet nvmet_tcp nvme_tcp nvme_loop; do
    if ! lsmod | awk '{print $1}' | grep -qx "$m"; then
        modprobe "${m//_/-}" && manifest "module_loaded=$m" && log "loaded module $m"
    fi
done
[ -d "$NVMET_CFS" ] || die "nvmet configfs missing"

# --- zram devices (hot_add; refuse index 0; record every index) ------------
mkzram() { # size-bytes label -> echoes /dev/zramN
    local size=$1 label=$2 idx
    idx=$(cat /sys/class/zram-control/hot_add)
    [ "$idx" != "0" ] || die "hot_add returned zram0 (user swap slot!) — abort"
    echo "$size" > "/sys/block/zram$idx/disksize"
    manifest "zram=$idx label=$label"
    log "zram$idx = $label ($size bytes)"
    echo "/dev/zram$idx"
}
ZR_SPDK=$(mkzram $((4*1024*1024*1024)) bench-spdk)
ZR_NVMET=$(mkzram $((4*1024*1024*1024)) bench-nvmet)
ZR_LOOP=$(mkzram $((4*1024*1024*1024)) bench-loop)
ZR_GMETA=$(mkzram $((2*1024*1024*1024)) guard-meta)
ZR_GDATA=$(mkzram $((8*1024*1024*1024)) guard-data)

# --- pre-fill bench devices (identical incompressible fill, all arms) ------
for d in "$ZR_SPDK" "$ZR_NVMET" "$ZR_LOOP"; do
    log "prefill $d"
    fio --name=fill --filename="$d" --rw=write --bs=1M --iodepth=8 \
        --ioengine=io_uring --direct=1 --randrepeat=0 --size=100% \
        --cpus_allowed=0-15 --output=/dev/null
done

# --- SPDK target ------------------------------------------------------------
log "starting spdk_tgt (mask $SPDK_CORE_MASK, ${SPDK_MEM_MB}MB hugemem)"
"$SPDK_DIR/build/bin/spdk_tgt" -m "$SPDK_CORE_MASK" -s "$SPDK_MEM_MB" \
    -r "$RPCSOCK" > /tmp/spdkscope/spdk_tgt.log 2>&1 &
SPDK_PID=$!
echo "$SPDK_PID" > "$STATE/spdk_tgt.pid"
manifest "spdk_pid=$SPDK_PID"
for i in $(seq 1 50); do
    $RPC spdk_get_version >/dev/null 2>&1 && break
    kill -0 "$SPDK_PID" 2>/dev/null || { tail -20 /tmp/spdkscope/spdk_tgt.log; die "spdk_tgt died at startup"; }
    sleep 0.2
done
$RPC spdk_get_version >/dev/null || die "spdk_tgt RPC not answering"
log "spdk_tgt up pid=$SPDK_PID ($($RPC spdk_get_version | jq -r .version))"

$RPC nvmf_create_transport -t TCP
# bench subsystem: aio bdev on zram (same backing class as nvmet device_path)
$RPC bdev_aio_create "$ZR_SPDK" aio_bench 4096
$RPC nvmf_create_subsystem "$NQN_SPDK" -a -s SPDKSCOPE01
$RPC nvmf_subsystem_add_ns "$NQN_SPDK" aio_bench
$RPC nvmf_subsystem_add_listener "$NQN_SPDK" -t tcp -a 127.0.0.1 -s $PORT_SPDK -f ipv4
# guard subsystem: 2 namespaces (meta+data) WITH PTPL files (reservation persistence)
$RPC bdev_aio_create "$ZR_GMETA" aio_gmeta 4096
$RPC bdev_aio_create "$ZR_GDATA" aio_gdata 4096
$RPC nvmf_create_subsystem "$NQN_SPDK_GUARD" -a -s SPDKSCOPE02
$RPC nvmf_subsystem_add_ns "$NQN_SPDK_GUARD" aio_gmeta --ptpl-file /tmp/spdkscope/ptpl-gmeta.json \
    || $RPC nvmf_subsystem_add_ns "$NQN_SPDK_GUARD" aio_gmeta -p /tmp/spdkscope/ptpl-gmeta.json \
    || { manifest "note=ptpl_file_unsupported"; $RPC nvmf_subsystem_add_ns "$NQN_SPDK_GUARD" aio_gmeta; }
$RPC nvmf_subsystem_add_ns "$NQN_SPDK_GUARD" aio_gdata --ptpl-file /tmp/spdkscope/ptpl-gdata.json \
    || $RPC nvmf_subsystem_add_ns "$NQN_SPDK_GUARD" aio_gdata -p /tmp/spdkscope/ptpl-gdata.json \
    || $RPC nvmf_subsystem_add_ns "$NQN_SPDK_GUARD" aio_gdata
$RPC nvmf_subsystem_add_listener "$NQN_SPDK_GUARD" -t tcp -a 127.0.0.1 -s $PORT_SPDK -f ipv4
manifest "spdk_subsystem=$NQN_SPDK"
manifest "spdk_subsystem=$NQN_SPDK_GUARD"
log "spdk subsystems configured"

# --- kernel nvmet-tcp target -------------------------------------------------
mknvmet_sub() { # nqn device
    local nqn=$1 dev=$2
    local sd="$NVMET_CFS/subsystems/$nqn"
    [ ! -d "$sd" ] || die "nvmet subsystem $nqn already exists (foreign?)"
    mkdir -p "$sd"
    echo 1 > "$sd/attr_allow_any_host"
    mkdir -p "$sd/namespaces/1"
    echo "$dev" > "$sd/namespaces/1/device_path"
    echo 1 > "$sd/namespaces/1/enable"
    manifest "nvmet_subsystem=$nqn"
}
mknvmet_sub "$NQN_NVMET" "$ZR_NVMET"
PD="$NVMET_CFS/ports/$NVMET_PORT_ID_TCP"
[ ! -d "$PD" ] || die "nvmet port $NVMET_PORT_ID_TCP exists (foreign?)"
mkdir -p "$PD"
echo 127.0.0.1 > "$PD/addr_traddr"
echo tcp       > "$PD/addr_trtype"
echo $PORT_NVMET > "$PD/addr_trsvcid"
echo ipv4      > "$PD/addr_adrfam"
ln -s "$NVMET_CFS/subsystems/$NQN_NVMET" "$PD/subsystems/$NQN_NVMET"
manifest "nvmet_port=$NVMET_PORT_ID_TCP"
log "kernel nvmet-tcp target up (port $PORT_NVMET)"

# --- kernel nvmet-loop reference ----------------------------------------------
mknvmet_sub "$NQN_LOOP" "$ZR_LOOP"
PL="$NVMET_CFS/ports/$NVMET_PORT_ID_LOOP"
[ ! -d "$PL" ] || die "nvmet port $NVMET_PORT_ID_LOOP exists (foreign?)"
mkdir -p "$PL"
echo loop > "$PL/addr_trtype"
ln -s "$NVMET_CFS/subsystems/$NQN_LOOP" "$PL/subsystems/$NQN_LOOP"
manifest "nvmet_port=$NVMET_PORT_ID_LOOP"
log "kernel nvmet-loop reference up"

# --- kernel initiator connects -------------------------------------------------
finddev() { # nqn nsid -> /dev/nvmeXnN HEAD node (not the cXnY channel node)
    local nqn=$1 nsid=${2:-1} c cname
    for i in $(seq 1 40); do
        for c in /sys/class/nvme/nvme*; do
            [ -e "$c/subsysnqn" ] || continue
            if [ "$(cat "$c/subsysnqn")" = "$nqn" ]; then
                cname=$(basename "$c")           # nvme1
                # native multipath: block head is nvmeXnN even when sysfs
                # lists the channel node nvmeXcYnN
                if [ -b "/dev/${cname}n${nsid}" ]; then
                    echo "/dev/${cname}n${nsid}"; return 0
                fi
            fi
        done
        sleep 0.25
    done
    return 1
}
nvme connect -t tcp -a 127.0.0.1 -s $PORT_SPDK -n "$NQN_SPDK"
manifest "connected=$NQN_SPDK"
nvme connect -t tcp -a 127.0.0.1 -s $PORT_NVMET -n "$NQN_NVMET"
manifest "connected=$NQN_NVMET"
nvme connect -t loop -n "$NQN_LOOP"
manifest "connected=$NQN_LOOP"
nvme connect -t tcp -a 127.0.0.1 -s $PORT_SPDK -n "$NQN_SPDK_GUARD"
manifest "connected=$NQN_SPDK_GUARD"

DEV_SPDK=$(finddev "$NQN_SPDK" 1)   || die "no dev for $NQN_SPDK"
DEV_NVMET=$(finddev "$NQN_NVMET" 1) || die "no dev for $NQN_NVMET"
DEV_LOOP=$(finddev "$NQN_LOOP" 1)   || die "no dev for $NQN_LOOP"
DEV_GMETA=$(finddev "$NQN_SPDK_GUARD" 1) || die "no dev for $NQN_SPDK_GUARD (ns1)"
DEV_GDATA=$(finddev "$NQN_SPDK_GUARD" 2) || die "no dev for $NQN_SPDK_GUARD (ns2)"
{
    echo "DEV_SPDK=$DEV_SPDK"
    echo "DEV_NVMET=$DEV_NVMET"
    echo "DEV_LOOP=$DEV_LOOP"
    echo "DEV_GMETA=$DEV_GMETA"
    echo "DEV_GDATA=$DEV_GDATA"
    echo "SPDK_PID=$SPDK_PID"
} > "$STATE/devices"
log "devices: spdk=$DEV_SPDK nvmet=$DEV_NVMET loop=$DEV_LOOP guard=$DEV_GMETA,$DEV_GDATA"
log "rig up OK"
