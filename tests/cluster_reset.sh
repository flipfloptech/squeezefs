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
#   kind = mds (memory-backed null_blk) | oss (backing per OSS_BACKING below)
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

# Data-node backing. The choice defines what the testbed measures:
#   zram    — compressed RAM disk. CPU-priced writes (throughput depends on the
#             benchmark's DATA PATTERN — label rows with it) but native discard,
#             so it is the overwrite-tax / reclaim venue. RAM cost ≈ compressed.
#   nullblk — memory-backed null_blk, no compression: the clean THROUGHPUT-
#             CEILING venue (a raw fio bracket measured 6.7 GB/s/node vs zram's
#             1.65 on the same fabric). RAM cost is 1:1 with device size —
#             size OSS_NULLB_MB to the node's free RAM. Discard support on old
#             kernels is absent/spotty: the daemon counts skipped reclaims and
#             moves on, but overwrite-tax A/Bs are muted here.
OSS_BACKING="zram"          # zram | nullblk
OSS_ZRAM_SIZE="64G"         # zram disksize per data namespace
OSS_ZRAM_ALGO="auto"        # auto = best available on the target (zstd>lz4>lzo-rle>lzo);
                            # or name one explicitly to A/B compression cost
OSS_NULLB_MB=65536          # nullblk size (MiB) per data namespace
MOUNT_EXTRA=(--interception --allow-other --log-file /tmp/sqz.log)
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=5)
# ============================================================================

log()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
die()  { printf 'FATAL: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
[ -x "$SQZ" ] || die "client binary not executable: $SQZ"
case "$OSS_BACKING" in zram|nullblk) ;; *) die "OSS_BACKING must be zram or nullblk (got: $OSS_BACKING)";; esac

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

# disconnect with verification: survivors auto-reconnect to rebuilt shares
# and then poison step 3 with duplicate-connect failures, so this must be
# LOUD, not best-effort.
for attempt in 1 2 3; do
  for h in "${HOSTS[@]}"; do
    IFS=: read -r name _ _ _ <<<"$h"
    "$SQZ" nvmeof disconnect --subnqn "$NQN_PREFIX:$name" 2>&1 | sed 's/^/    /' || true
  done
  gone=1
  for _ in $(seq 1 15); do
    if grep -lq "$NQN_PREFIX" /sys/class/nvme/nvme*/subsysnqn 2>/dev/null; then
      gone=0; sleep 1
    else
      gone=1; break
    fi
  done
  [ "$gone" = 1 ] && break
  echo "  connections still present after disconnect (attempt $attempt) — retrying"
done
[ "$gone" = 1 ] || die "could not disconnect all $NQN_PREFIX connections; check 'nvme list-subsys'"

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
  elif [ "$OSS_BACKING" = "nullblk" ]; then
    # Same recipe as the mds nodes: memory-backed null_blk, no compression.
    "${SSH[@]}" "root@$ip1" REMOTE_SQZ="$REMOTE_SQZ" NAME="$name" NQN="$NQN_PREFIX:$name" \
        IP1="$ip1" IP2="$ip2" SIZE_MB="$OSS_NULLB_MB" 'bash -s' <<'EOS'
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
free_mb=$(awk '/MemAvailable/ {print int($2/1024)}' /proc/meminfo)
[ "$free_mb" -gt "$SIZE_MB" ] || \
  echo "WARNING: nullblk size ${SIZE_MB} MiB exceeds MemAvailable ${free_mb} MiB — writes can OOM this node" >&2
"$REMOTE_SQZ" nvmeof share "$dev" --target-stack nvmet --ip "$IP1,$IP2" --subnqn "$NQN"
EOS
  else
    "${SSH[@]}" "root@$ip1" REMOTE_SQZ="$REMOTE_SQZ" NQN="$NQN_PREFIX:$name" \
        IP1="$ip1" IP2="$ip2" ZSIZE="$OSS_ZRAM_SIZE" ZALGO="$OSS_ZRAM_ALGO" 'bash -s' <<'EOS'
set -euo pipefail
"$REMOTE_SQZ" nvmeof unshare "$NQN" 2>/dev/null || true
modprobe zram 2>/dev/null || true
# Crypto modules are NOT auto-loaded on RHEL8-family kernels, so the
# comp_algorithm listing hides algorithms the kernel actually ships (lz4,
# sometimes zstd). Load them first, then pick by WRITE-AND-VERIFY — the
# listing alone under-reports.
modprobe lz4  2>/dev/null || true
modprobe zstd 2>/dev/null || true
if [ -b /dev/zram0 ]; then
  echo 1 > /sys/block/zram0/reset            # full wipe — the fresh store
else
  cat /sys/class/zram-control/hot_add >/dev/null
fi
try_algo() {
  echo "$1" > /sys/block/zram0/comp_algorithm 2>/dev/null \
    && grep -q "\[$1\]" /sys/block/zram0/comp_algorithm
}
pick=""
if [ "$ZALGO" != "auto" ]; then
  if try_algo "$ZALGO"; then
    pick="$ZALGO"
  else
    echo "note: '$ZALGO' not usable on this kernel (offered: $(cat /sys/block/zram0/comp_algorithm)) — auto-selecting" >&2
  fi
fi
if [ -z "$pick" ]; then
  for cand in zstd lz4 lzo-rle lzo; do
    try_algo "$cand" && pick="$cand" && break
  done
fi
[ -n "$pick" ] || { echo "no usable zram compression algorithm (offered: $(cat /sys/block/zram0/comp_algorithm))" >&2; exit 1; }
echo "zram comp_algorithm: $pick"
echo "$ZSIZE" > /sys/block/zram0/disksize
sz=$(blockdev --getsize64 /dev/zram0)
[ "$sz" -gt 0 ] || { echo "zram0 has zero size after setup" >&2; exit 1; }
"$REMOTE_SQZ" nvmeof share /dev/zram0 --target-stack nvmet --ip "$IP1,$IP2" --subnqn "$NQN"
EOS
  fi
done

# ---------- 3. Client: reconnect (both paths), multipath policy -------------
log "3/5 client: connect both paths per subsystem"
path_live() {  # path_live <nqn> <traddr> — is this exact path already connected?
  local c
  for c in /sys/class/nvme/nvme*; do
    [ -e "$c/subsysnqn" ] || continue
    [ "$(cat "$c/subsysnqn")" = "$1" ] || continue
    grep -q "traddr=$2," "$c/address" 2>/dev/null && return 0
  done
  return 1
}
for h in "${HOSTS[@]}"; do
  IFS=: read -r name ip1 ip2 _ <<<"$h"
  for ip in "$ip1" "$ip2"; do
    if path_live "$NQN_PREFIX:$name" "$ip"; then
      echo "  $name via $ip: already connected — keeping"
    else
      "$SQZ" nvmeof connect --ip "$ip" --subnqn "$NQN_PREFIX:$name"
    fi
  done
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
