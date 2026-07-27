#!/usr/bin/env bash
#
# cluster_reset.sh — tear down and freshly rebuild a SqueezeFS test cluster.
#
# Run FROM THE CLIENT as root. Uses passwordless root ssh to the storage
# nodes. DESTROYS ALL FILESYSTEM DATA (that is the point: fresh zram/null_blk
# backings, fresh format).
#
# Edit the CONFIG block, then:  sudo tests/cluster_reset.sh
#
set -euo pipefail

# ============================ CONFIG ========================================
# Path to the squeezefs binary ON THE CLIENT (this machine).
SQZ="/scratch/tmp/squeezefs"
# Path to the squeezefs binary ON THE STORAGE NODES.
REMOTE_SQZ="/scratch/tmp/squeezefs"

# Storage nodes: "name:primary_ip:secondary_ip:kind"
#   kind = mds (memory-backed null_blk) | oss (zram)
HOSTS=(
  "mds0:10.181.177.191:10.181.178.191:mds"
  "mds1:10.181.177.192:10.181.178.192:mds"
  "oss0:10.181.177.193:10.181.178.193:oss"
  "oss1:10.181.177.195:10.181.178.195:oss"
)

NQN_PREFIX="nqn.2026-07.io.squeezefs"
MOUNTPOINT="/scratch/tmp/test"
CACHE_DIR="/scratch/tmp/cache"
META_SLOTS=8
MDS_SIZE_MB=8192            # null_blk size per metadata namespace
OSS_ZRAM_SIZE="64G"         # zram disksize per data namespace
OSS_ZRAM_ALGO="zstd"        # try lz4 to A/B target compression cost
MOUNT_EXTRA=(--interception --allow-other --log-file /tmp/sqz.log)
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=5)
# ============================================================================

log()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
die()  { printf 'FATAL: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
[ -x "$SQZ" ] || die "client binary not executable: $SQZ"

echo "This DESTROYS all data on the cluster volumes and rebuilds from scratch."
read -r -p "Type YES to continue: " ans
[ "$ans" = "YES" ] || die "aborted"

# ---------- 1. Client teardown ----------------------------------------------
log "1/5 client: unmount + disconnect"
if mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
  umount "$MOUNTPOINT" || umount -l "$MOUNTPOINT" || true
  sleep 2
fi
pkill -f 'squeezefs moun[t]' 2>/dev/null || true
sleep 1

for h in "${HOSTS[@]}"; do
  IFS=: read -r name _ _ _ <<<"$h"
  "$SQZ" nvmeof disconnect --subnqn "$NQN_PREFIX:$name" 2>/dev/null || true
done
# wait until no squeezefs namespaces remain
for _ in $(seq 1 20); do
  grep -lq "$NQN_PREFIX" /sys/class/nvme/nvme*/subsysnqn 2>/dev/null || break
  sleep 1
done

# ---------- 2. Storage nodes: unshare, destroy, recreate, re-share ----------
for h in "${HOSTS[@]}"; do
  IFS=: read -r name ip1 ip2 kind <<<"$h"
  log "2/5 $name ($ip1): rebuild $kind backing + re-share"

  if [ "$kind" = "mds" ]; then
    "${SSH[@]}" "root@$ip1" REMOTE_SQZ="$REMOTE_SQZ" NAME="$name" NQN="$NQN_PREFIX:$name" \
        IP1="$ip1" IP2="$ip2" SIZE_MB="$MDS_SIZE_MB" 'bash -s' <<'EOS'
set -euo pipefail
"$REMOTE_SQZ" nvmeof unshare "$NQN" 2>/dev/null || true
modprobe null_blk nr_devices=0 2>/dev/null || true
d="/sys/kernel/config/nullb/$NAME"
if [ -d "$d" ]; then echo 0 > "$d/power" 2>/dev/null || true; rmdir "$d"; fi
mkdir "$d"
echo "$SIZE_MB" > "$d/size"
echo 4096       > "$d/blocksize"
echo 1          > "$d/memory_backed"
echo 1          > "$d/power"
idx=$(cat "$d/index"); dev="/dev/nullb$idx"
[ -b "$dev" ] || { echo "null_blk device missing: $dev" >&2; exit 1; }
"$REMOTE_SQZ" nvmeof share "$dev" --target-stack nvmet --ip "$IP1,$IP2" --subnqn "$NQN"
EOS
  else
    "${SSH[@]}" "root@$ip1" REMOTE_SQZ="$REMOTE_SQZ" NQN="$NQN_PREFIX:$name" \
        IP1="$ip1" IP2="$ip2" ZSIZE="$OSS_ZRAM_SIZE" ZALGO="$OSS_ZRAM_ALGO" 'bash -s' <<'EOS'
set -euo pipefail
"$REMOTE_SQZ" nvmeof unshare "$NQN" 2>/dev/null || true
modprobe zram 2>/dev/null || true
if [ -b /dev/zram0 ]; then
  echo 1 > /sys/block/zram0/reset            # full wipe — the fresh store
else
  cat /sys/class/zram-control/hot_add >/dev/null
fi
echo "$ZALGO" > /sys/block/zram0/comp_algorithm
echo "$ZSIZE" > /sys/block/zram0/disksize
sz=$(blockdev --getsize64 /dev/zram0)
[ "$sz" -gt 0 ] || { echo "zram0 has zero size after setup" >&2; exit 1; }
"$REMOTE_SQZ" nvmeof share /dev/zram0 --target-stack nvmet --ip "$IP1,$IP2" --subnqn "$NQN"
EOS
  fi
done

# ---------- 3. Client: reconnect (both paths), multipath policy -------------
log "3/5 client: connect both paths per subsystem"
for h in "${HOSTS[@]}"; do
  IFS=: read -r name ip1 ip2 _ <<<"$h"
  "$SQZ" nvmeof connect --ip "$ip1" --subnqn "$NQN_PREFIX:$name"
  "$SQZ" nvmeof connect --ip "$ip2" --subnqn "$NQN_PREFIX:$name"
done

# wait for every namespace head to appear, then resolve NQN -> /dev node
declare -A DEV
for h in "${HOSTS[@]}"; do
  IFS=: read -r name _ _ _ <<<"$h"
  want="$NQN_PREFIX:$name"; found=""
  for _ in $(seq 1 30); do
    for s in /sys/class/nvme-subsystem/nvme-subsys*; do
      [ -e "$s/subsysnqn" ] || continue
      [ "$(cat "$s/subsysnqn")" = "$want" ] || continue
      ns=""
      for c in "$s"/nvme*n*; do
        b=$(basename "$c")
        [[ "$b" =~ ^nvme[0-9]+n[0-9]+$ ]] && ns="$b" && break
      done
      [ -n "$ns" ] && found="/dev/$ns" && break
    done
    [ -n "$found" ] && break
    sleep 1
  done
  [ -n "$found" ] || die "namespace for $want never appeared"
  DEV[$name]="$found"
  echo "  $want -> $found"
done
for s in /sys/class/nvme-subsystem/nvme-subsys*/iopolicy; do
  echo round-robin > "$s" 2>/dev/null || true
done

# ---------- 4. Format --------------------------------------------------------
log "4/5 format (meta-slots=$META_SLOTS)"
mkdir -p "$CACHE_DIR" "$MOUNTPOINT"
META_URI="sqmeta://${DEV[mds0]},${DEV[mds1]}"
DATA_URI="sqdata://${DEV[oss0]},${DEV[oss1]}"
"$SQZ" format "$META_URI" "$DATA_URI" --meta-slots "$META_SLOTS" --disk-cache-paths "$CACHE_DIR"

# ---------- 5. Mount + verify -------------------------------------------------
log "5/5 mount + verify"
"$SQZ" mount "$META_URI" "$MOUNTPOINT" --daemon "${MOUNT_EXTRA[@]}"
sleep 2
mountpoint -q "$MOUNTPOINT" || die "mount did not come up"
commit=$(grep -oE '"build_commit": *"[0-9a-f]*"' "$MOUNTPOINT/.stats" | head -1)
echo "mounted: $MOUNTPOINT  daemon $commit"
echo
echo "Fresh cluster ready. First move: the same-day write bracket —"
echo "  ./elbencho -w -t 16 -b 1m -s 2g --direct --lat $MOUNTPOINT/f{1..16}   # kernel"
echo "  LD_PRELOAD=<shim> !!                                                  # shim"
