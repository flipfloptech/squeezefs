#!/bin/bash
set -euo pipefail

# PR L4-7 A/B rig — registered-buffer / direct-to-arena DMA on the
# device-true interception row (docs/design-preload-interception.md
# §5.5.3). The card's law: the deepening must show ≥ 10 % on this row
# or the code self-deletes.
#
# One invocation = one leg. The leg under test is selected by the
# daemon-side env the implementation reads (SQUEEZEFS_IL_ARENA_DMA=0/1);
# the harness records IOPS + the engagement/device-true proof columns
# for the evidence note. EVERY measurement states its instrument
# (house law): elbencho, psync engine equivalent (sync positional
# reads through the shim), thread count stated per row.
#
# Usage: sudo tests/run_ipc_dma_ab.sh <legname> [threads] [secs]

LEG="${1:?leg name (e.g. baseline, arena-dma)}"
THREADS="${2:-8}"
SECS="${3:-15}"
BS="${4:-4k}"          # 4k = IOPS shape (randread); larger = bandwidth shape (seq read)
if [ "$BS" = "4k" ]; then RW=randread; else RW=read; fi

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUNUSER="${SUDO_USER:-$(id -un)}"
MOUNT_DIR="${MOUNT_DIR:-/tmp/squeezefs_dma_mount}"
STAGING_DIR="${STAGING_DIR:-/tmp/squeezefs_dma_staging}"
LOG=/tmp/squeezefs_dma_ab.log
SO="$REPO_DIR/target/preload-release/libsqueezefs_il.so"
OUT="/tmp/ipc_dma_ab_${LEG}.txt"

[ "$(id -u)" -eq 0 ] || { echo "ERROR: run as root (sudo $0 ...)" >&2; exit 1; }
cd "$REPO_DIR"

run_as_user() {
    if [ "$RUNUSER" != "root" ] && id "$RUNUSER" &>/dev/null; then
        su -s /bin/bash "$RUNUSER" -c "cd '$REPO_DIR' && $*"
    else
        bash -c "cd '$REPO_DIR' && $*"
    fi
}

run_as_user "cargo build --release"
run_as_user "cargo build -p squeezefs-preload --profile preload-release --features interposers"
SQUEEZEFS_BIN="$REPO_DIR/target/release/squeezefs"

export SQUEEZEFS_IPC_ALLOW_DEV=1

killall -9 squeezefs &>/dev/null || true
sleep 1
umount -l "$MOUNT_DIR" &>/dev/null || true
rm -f "$LOG"
mkdir -p "$MOUNT_DIR" "$STAGING_DIR"
if [ -n "${SQUEEZEFS_DMA_AB_DEVSUB:-}" ]; then
    # The house pseudo-NVMe substrate (tests/dev_substrate.sh create):
    # real /dev/nvmeXnY namespaces via nvmet-loop — the honest surface
    # for a device-true adjudication.
    META_URI="sqmeta:///dev/nvme1n1"
    DATA_URI="sqdata:///dev/nvme5n1,/dev/nvme6n1"
    SUBSTRATE_NOTE="devsub nvmet-loop (null_blk meta + zram data)"
else
    truncate -s 4G /dev/shm/squeezefs_dma_meta
    truncate -s 8G /dev/shm/squeezefs_dma_backend
    META_URI="sqmeta:///dev/shm/squeezefs_dma_meta"
    DATA_URI="sqdata:///dev/shm/squeezefs_dma_backend"
    SUBSTRATE_NOTE="/dev/shm file-backed (A/B-relative only)"
fi

"$SQUEEZEFS_BIN" format \
    "$META_URI" \
    "$DATA_URI" \
    --disk-cache-paths "$STAGING_DIR" \
    --force >/dev/null

RUST_LOG=warn "$SQUEEZEFS_BIN" mount \
    "$META_URI" \
    "$MOUNT_DIR" \
    --daemon \
    --interception \
    -o direct_device_true \
    --disk-cache-size 2GB \
    --log-file "$LOG" \
    --allow-other

sleep 3
mountpoint -q "$MOUNT_DIR" || { cat "$LOG" || true; echo "mount failed" >&2; exit 1; }
chmod 1777 "$MOUNT_DIR"

cleanup() {
    umount "$MOUNT_DIR" 2>/dev/null || umount -l "$MOUNT_DIR" 2>/dev/null || true
    rm -f /dev/shm/squeezefs_dma_meta /dev/shm/squeezefs_dma_backend
    rm -rf "$STAGING_DIR"
}
trap cleanup EXIT

stats() {
    python3 -c "import json; print(json.load(open('$MOUNT_DIR/.stats'))['metrics'].get('$1', 0))"
}

# Layout: one 2 GiB striped file, written through the ring.
LD_PRELOAD="$SO" dd if=/dev/urandom of="$MOUNT_DIR/ab.bin" bs=1M count=2048 status=none
sync

R0=$(stats ipc_ops_read)

# The measured pass: fio psync rand-4k O_DIRECT reads through the shim.
# INSTRUMENT NOTE (house law — every measurement states its instrument):
# elbencho is DISQUALIFIED for il rows on this box: the installed binary
# is STATICALLY LINKED, so LD_PRELOAD never loads and the row silently
# measures kernel FUSE (charter rule 4 — the engagement check below
# exists to make that unpublishable). fio is dynamic; psync = positional
# pread, the v1 ring's exact shape.
LD_PRELOAD="$SO" fio --name="il-$LEG" --filename="$MOUNT_DIR/ab.bin" \
    --rw="$RW" --bs="$BS" --size=2g --ioengine=psync --direct=1 \
    --numjobs="$THREADS" --group_reporting --runtime="$SECS" --time_based \
    --output-format=terse > "$OUT.raw"

R1=$(stats ipc_ops_read)
OPS=$((R1 - R0))
IOPS=$(cut -d';' -f8 "$OUT.raw" | head -1)
BW_KBS=$(cut -d';' -f7 "$OUT.raw" | head -1)

# Charter rule 4: the row is INVALID unless the ring served ≈ the ops.
# (bs > slab chunks into multiple ring ops per fio op — ring ops ≥ fio
# ops always holds, which is the direction the check needs.)
FIO_OPS=$((IOPS * SECS))
[ "$OPS" -gt $((FIO_OPS / 2)) ] || {
    echo "INVALID ROW: ring served $OPS of ~$FIO_OPS ops — shim not engaged" >&2
    exit 1
}

{
    echo "leg: $LEG"
    echo "instrument: fio psync $RW bs=$BS direct=1 numjobs=$THREADS runtime=${SECS}s time_based (via LD_PRELOAD shim; elbencho disqualified: static binary)"
    echo "substrate: $SUBSTRATE_NOTE"
    echo "mount: --interception -o direct_device_true"
    echo "arena_dma_env: SQUEEZEFS_IL_ARENA_DMA=${SQUEEZEFS_IL_ARENA_DMA:-unset}"
    echo "read_iops: $IOPS"
    echo "read_bw_kbs: $BW_KBS"
    echo "ring_ops_read_delta: $OPS (engagement OK)"
    echo "stats: fast=$(stats ipc_fast_path_serves) handoffs=$(stats ipc_async_handoffs) dma_reads=$(stats ipc_arena_dma_reads)"
} | tee "$OUT"

echo "WROTE $OUT"
