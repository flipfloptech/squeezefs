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
CACHE_DIR=""                # staging/cache dir; EMPTY = cache-less format (the right
                            # posture when local disk is slower than the fabric — the
                            # SATA-cache lesson: same throughput, tails 238ms -> 58ms)
MDS_SIZE_MB=8192            # null_blk size per metadata namespace

# Data-plane width. Every data namespace is its own subsystem (own nvme-tcp
# queue set), named "$NQN_PREFIX:<node>-d<i>".
OSS_NAMESPACES=1            # data namespaces per data-serving node
                            # (per-node RAM cost = OSS_NAMESPACES x backing size)
DATA_ON_MDS=0               # 1 = mds nodes ALSO serve OSS_NAMESPACES data namespaces
                            # each (the 4-wide data plane: 2-target raw ceiling was
                            # ~16.6 GB/s; 4 targets ~= double the headroom)

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
[[ "$OSS_NAMESPACES" =~ ^[1-9][0-9]*$ ]] || die "OSS_NAMESPACES must be a positive integer (got: $OSS_NAMESPACES)"
case "$DATA_ON_MDS" in 0|1) ;; *) die "DATA_ON_MDS must be 0 or 1 (got: $DATA_ON_MDS)";; esac

# serves_data <kind> — does this node export data namespaces?
serves_data() { [ "$1" = "oss" ] || { [ "$1" = "mds" ] && [ "$DATA_ON_MDS" = 1 ]; }; }

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
  # enumerate LIVE prefixed NQNs (robust across topology changes — old
  # single-namespace layouts, wide layouts, drift) and disconnect each.
  for f in /sys/class/nvme/nvme*/subsysnqn; do
    [ -e "$f" ] || continue
    nqn=$(cat "$f")
    case "$nqn" in "$NQN_PREFIX"*) "$SQZ" nvmeof disconnect "$nqn" 2>&1 | sed 's/^/    /' || true;; esac
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
  log "2/5 $name ($ip1): rebuild backings + re-share"

  # Stale-share sweep first: the legacy single-namespace NQN plus a generous
  # -dN range, so topology changes (1-wide <-> N-wide, data-on-mds on/off)
  # never leave orphan exports for the client to trip over.
  "${SSH[@]}" "root@$ip1" REMOTE_SQZ="$REMOTE_SQZ" BASE="$NQN_PREFIX:$name" 'bash -s' <<'EOS'
set -u
"$REMOTE_SQZ" nvmeof unshare "$BASE" 2>/dev/null || true
for i in $(seq 0 15); do "$REMOTE_SQZ" nvmeof unshare "$BASE-d$i" 2>/dev/null || true; done
exit 0
EOS

  if [ "$kind" = "mds" ]; then
    "${SSH[@]}" "root@$ip1" REMOTE_SQZ="$REMOTE_SQZ" NAME="$name" NQN="$NQN_PREFIX:$name" \
        IP1="$ip1" IP2="$ip2" SIZE_MB="$MDS_SIZE_MB" 'bash -s' <<'EOS'
set -euo pipefail
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
  fi

  if serves_data "$kind"; then
    if [ "$OSS_BACKING" = "nullblk" ]; then
      # Memory-backed null_blk data namespaces, one subsystem each.
      "${SSH[@]}" "root@$ip1" REMOTE_SQZ="$REMOTE_SQZ" NAME="$name" BASE="$NQN_PREFIX:$name" \
          IP1="$ip1" IP2="$ip2" SIZE_MB="$OSS_NULLB_MB" COUNT="$OSS_NAMESPACES" 'bash -s' <<'EOS'
set -euo pipefail
modprobe null_blk nr_devices=0 2>/dev/null || true
free_mb=$(awk '/MemAvailable/ {print int($2/1024)}' /proc/meminfo)
total_mb=$((SIZE_MB * COUNT))
[ "$free_mb" -gt "$total_mb" ] || \
  echo "WARNING: $COUNT x ${SIZE_MB} MiB nullblk = ${total_mb} MiB exceeds MemAvailable ${free_mb} MiB — writes can OOM this node" >&2
for i in $(seq 0 $((COUNT - 1))); do
  d="/sys/kernel/config/nullb/$NAME-d$i"
  if [ -d "$d" ]; then echo 0 > "$d/power" 2>/dev/null || true; rmdir "$d"; fi
  mkdir "$d"
  echo "$SIZE_MB" > "$d/size"
  echo 4096       > "$d/blocksize"
  echo 1          > "$d/memory_backed"
  echo 1          > "$d/power"
  idx=$(cat "$d/index"); dev="/dev/nullb$idx"
  [ -b "$dev" ] || { echo "null_blk device missing: $dev" >&2; exit 1; }
  "$REMOTE_SQZ" nvmeof share "$dev" --target-stack nvmet --ip "$IP1,$IP2" --subnqn "$BASE-d$i"
done
EOS
    else
      # zram data namespaces, one subsystem each.
      "${SSH[@]}" "root@$ip1" REMOTE_SQZ="$REMOTE_SQZ" BASE="$NQN_PREFIX:$name" \
          IP1="$ip1" IP2="$ip2" ZSIZE="$OSS_ZRAM_SIZE" ZALGO="$OSS_ZRAM_ALGO" COUNT="$OSS_NAMESPACES" 'bash -s' <<'EOS'
set -euo pipefail
modprobe zram 2>/dev/null || true
# Crypto modules are NOT auto-loaded on RHEL8-family kernels, so the
# comp_algorithm listing hides algorithms the kernel actually ships (lz4,
# sometimes zstd). Load them first, then pick by WRITE-AND-VERIFY — the
# listing alone under-reports.
modprobe lz4  2>/dev/null || true
modprobe zstd 2>/dev/null || true
try_algo() {  # try_algo <dev-index> <algo>
  echo "$2" > "/sys/block/zram$1/comp_algorithm" 2>/dev/null \
    && grep -q "\[$2\]" "/sys/block/zram$1/comp_algorithm"
}
for i in $(seq 0 $((COUNT - 1))); do
  while [ ! -b "/dev/zram$i" ]; do cat /sys/class/zram-control/hot_add >/dev/null; done
  echo 1 > "/sys/block/zram$i/reset"           # full wipe — the fresh store
  pick=""
  if [ "$ZALGO" != "auto" ]; then
    if try_algo "$i" "$ZALGO"; then
      pick="$ZALGO"
    else
      echo "note: '$ZALGO' not usable on this kernel (offered: $(cat /sys/block/zram$i/comp_algorithm)) — auto-selecting" >&2
    fi
  fi
  if [ -z "$pick" ]; then
    for cand in zstd lz4 lzo-rle lzo; do
      try_algo "$i" "$cand" && pick="$cand" && break
    done
  fi
  [ -n "$pick" ] || { echo "no usable zram compression algorithm (offered: $(cat /sys/block/zram$i/comp_algorithm))" >&2; exit 1; }
  echo "zram$i comp_algorithm: $pick"
  echo "$ZSIZE" > "/sys/block/zram$i/disksize"
  sz=$(blockdev --getsize64 "/dev/zram$i")
  [ "$sz" -gt 0 ] || { echo "zram$i has zero size after setup" >&2; exit 1; }
  "$REMOTE_SQZ" nvmeof share "/dev/zram$i" --target-stack nvmet --ip "$IP1,$IP2" --subnqn "$BASE-d$i"
done
EOS
    fi
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
# Build the full subsystem list: meta per mds node, data per data-serving node.
# SUFFIXES orders meta first, then data in HOSTS order — the format URIs below
# reproduce this order deterministically.
META_SUFFIXES=(); DATA_SUFFIXES=()
for h in "${HOSTS[@]}"; do
  IFS=: read -r name _ _ kind <<<"$h"
  [ "$kind" = "mds" ] && META_SUFFIXES+=("$name")
done
for h in "${HOSTS[@]}"; do
  IFS=: read -r name _ _ kind <<<"$h"
  if serves_data "$kind"; then
    for i in $(seq 0 $((OSS_NAMESPACES - 1))); do DATA_SUFFIXES+=("$name-d$i"); done
  fi
done
[ "${#META_SUFFIXES[@]}" -ge 1 ] || die "no mds hosts configured"
[ "${#DATA_SUFFIXES[@]}" -ge 1 ] || die "no data namespaces configured"

# host_ips <suffix> -> "ip1 ip2" of the node that serves it
host_ips() {
  local h name ip1 ip2 _
  for h in "${HOSTS[@]}"; do
    IFS=: read -r name ip1 ip2 _ <<<"$h"
    case "$1" in "$name"|"$name"-d*) echo "$ip1 $ip2"; return 0;; esac
  done
  return 1
}

for suffix in "${META_SUFFIXES[@]}" "${DATA_SUFFIXES[@]}"; do
  read -r ip1 ip2 <<<"$(host_ips "$suffix")"
  for ip in "$ip1" "$ip2"; do
    if path_live "$NQN_PREFIX:$suffix" "$ip"; then
      echo "  $suffix via $ip: already connected — keeping"
    else
      "$SQZ" nvmeof connect --ip "$ip" --subnqn "$NQN_PREFIX:$suffix"
    fi
  done
done

# wait for every namespace head to appear, then resolve NQN -> /dev node
declare -A DEV
for suffix in "${META_SUFFIXES[@]}" "${DATA_SUFFIXES[@]}"; do
  want="$NQN_PREFIX:$suffix"; found=""
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
  DEV[$suffix]="$found"
  echo "  $want -> $found"
done
for s in /sys/class/nvme-subsystem/nvme-subsys*/iopolicy; do
  echo round-robin > "$s" 2>/dev/null || true
done

# ---------- 4. Format --------------------------------------------------------
log "4/5 format (derived meta routing width, data plane: ${#DATA_SUFFIXES[@]} namespaces)"
mkdir -p "$MOUNTPOINT"
meta_devs=""; for s in "${META_SUFFIXES[@]}"; do meta_devs+="${meta_devs:+,}${DEV[$s]}"; done
data_devs=""; for s in "${DATA_SUFFIXES[@]}"; do data_devs+="${data_devs:+,}${DEV[$s]}"; done
META_URI="sqmeta://$meta_devs"
DATA_URI="sqdata://$data_devs"
FORMAT_ARGS=("$META_URI" "$DATA_URI")
if [ -n "$CACHE_DIR" ]; then
  mkdir -p "$CACHE_DIR"
  FORMAT_ARGS+=(--disk-cache-paths "$CACHE_DIR")
else
  echo "  cache-less format (CACHE_DIR empty)"
fi
"$SQZ" format "${FORMAT_ARGS[@]}"

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
