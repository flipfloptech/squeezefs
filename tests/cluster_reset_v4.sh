#!/usr/bin/env bash
#
# cluster_reset_v4.sh — tear down and freshly rebuild the SqueezeFS test
# cluster in the CONVERGED reset-v5 shape (USER DECISION 2026-08-02):
#
#   1 meta + 2 data namespaces PER NODE x 5 nodes
#     = 5 meta volumes + 10 data namespaces, every node symmetric.
#
# v4 supersedes the patched v3 (/scratch/tmp/cluster_reset_v3.sh — the
# reset-v4 epoch's script; the old repo copy tests/cluster_reset.sh was
# STALE against it). Deltas from v3:
#   * converged node map (no mds/oss kinds — every node serves m0,d0,d1;
#     NQNs "$NQN_PREFIX:<node>-{m0,d0,d1}")
#   * ENUMERATION-BASED storage-node teardown (the oss2 lesson, journaled
#     2026-08-01 — the pre-reset journal was rotated by the re-provision):
#     sweep every nvmet port whose addr_traddr matches the node's fabric
#     IPs and every subsystem under our NQN prefix, ports FIRST. Never
#     expectation-named teardown.
#   * no --meta-slots anywhere (dynamic meta routing, KV_DYNAMIC_ROUTING
#     bit 6: the width is DERIVED; the flag is a hard error now)
#   * mount logs to /scratch/tmp/logs/sqz.log (standing convention
#     2026-08-02: agents use /scratch/tmp exclusively — /tmp is banned)
#
# Run FROM THE CLIENT as root. Uses passwordless root ssh to the storage
# nodes. DESTROYS ALL FILESYSTEM DATA (that is the point: fresh backings,
# fresh format — the reset-v5 epoch).
#
# Edit the CONFIG block, then:  sudo tests/cluster_reset_v4.sh
#
set -euo pipefail

# ============================ CONFIG ========================================
# Path to the squeezefs binary ON THE CLIENT (this machine).
SQZ="/scratch/tmp/squeezefs"
# Path to the squeezefs binary ON THE STORAGE NODES.
REMOTE_SQZ="/scratch/tmp/squeezefs"

# Storage nodes: "name:primary_ip:secondary_ip". EVERY node serves one meta
# namespace (m0, mds-class memory-backed null_blk) and OSS_NAMESPACES data
# namespaces (d0..dN, backing per OSS_BACKING). Node order here is the meta
# volume order in the format URI — keep it stable across resets.
HOSTS=(
  "aqr37:10.181.177.191:10.181.178.191"   # memp-s3ds-aqr-37
  "aqr38:10.181.177.192:10.181.178.192"   # memp-s3ds-aqr-38
  "aqr39:10.181.177.193:10.181.178.193"   # memp-s3ds-aqr-39
  "aqs38:10.181.177.195:10.181.178.195"   # memp-s3ds-aqs-38
  "oss2:10.181.177.196:10.181.178.196"    # oss2
)

NQN_PREFIX="nqn.2026-07.io.squeezefs"
MOUNTPOINT="/scratch/tmp/test"
CACHE_DIR=""                # staging/cache dir; EMPTY = cache-less format (the right
                            # posture when local disk is slower than the fabric — the
                            # SATA-cache lesson: same throughput, tails 238ms -> 58ms)
MDS_SIZE_MB=8192            # null_blk size per metadata namespace (one per node)

# Data-plane width. Every data namespace is its own subsystem (own nvme-tcp
# queue set), named "$NQN_PREFIX:<node>-d<i>".
OSS_NAMESPACES=2            # data namespaces per node (converged epoch: 2)

# Data backing. The choice defines what the testbed measures:
#   zram    — compressed RAM disk. CPU-priced writes (throughput depends on the
#             benchmark's DATA PATTERN — label rows with it) but native discard,
#             so it is the overwrite-tax / reclaim venue. RAM cost ≈ compressed.
#   nullblk — memory-backed null_blk, no compression: the clean THROUGHPUT-
#             CEILING venue (a raw fio bracket measured 6.7 GB/s/node vs zram's
#             1.65 on the same fabric). RAM cost is 1:1 with device size —
#             size OSS_NULLB_MB against MDS_SIZE_MB + OSS_NAMESPACES x size
#             per node. Discard support on old kernels is absent/spotty: the
#             daemon counts skipped reclaims and moves on, but overwrite-tax
#             A/Bs are muted here.
OSS_BACKING="nullblk"       # zram | nullblk
OSS_ZRAM_SIZE="64G"         # zram disksize per data namespace
OSS_ZRAM_ALGO="auto"        # auto = best available on the target (zstd>lz4>lzo-rle>lzo);
                            # or name one explicitly to A/B compression cost
OSS_NULLB_MB=49152          # nullblk size (MiB) per data namespace
LOG_DIR="/scratch/tmp/logs" # mount --log-file home (the 2026-08-02 convention;
                            # /tmp is banned for agent artifacts)
MOUNT_EXTRA=(--interception --allow-other --log-file "$LOG_DIR/sqz.log")
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=5)
# ============================================================================

log()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
die()  { printf 'FATAL: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
[ -x "$SQZ" ] || die "client binary not executable: $SQZ"
case "$OSS_BACKING" in zram|nullblk) ;; *) die "OSS_BACKING must be zram or nullblk (got: $OSS_BACKING)";; esac
[[ "$OSS_NAMESPACES" =~ ^[1-9][0-9]*$ ]] || die "OSS_NAMESPACES must be a positive integer (got: $OSS_NAMESPACES)"

# ---------- venue-epoch banner (loud, before the YES gate) ------------------
printf '\033[1m'
cat <<EOB
=============================================================================
 RESET-V5 — THE CONVERGED EPOCH (user decision 2026-08-02)
   1 meta + ${OSS_NAMESPACES} data namespaces per node x ${#HOSTS[@]} nodes
   = ${#HOSTS[@]} meta volumes + $(( ${#HOSTS[@]} * OSS_NAMESPACES )) data namespaces, every node symmetric
 Node map (name -> fabric paths):
EOB
for h in "${HOSTS[@]}"; do
  IFS=: read -r name ip1 ip2 <<<"$h"
  printf '   %-6s -> %s / %s   (%s-{m0' "$name" "$ip1" "$ip2" "$name"
  for i in $(seq 0 $((OSS_NAMESPACES - 1))); do printf ',d%s' "$i"; done
  printf '})\n'
done
cat <<EOB
 Meta routing: DYNAMIC (KV_DYNAMIC_ROUTING bit 6) — width is DERIVED;
   --meta-slots does not exist (naming it is a hard error).
 Data backing: $OSS_BACKING; format: $( [ -n "$CACHE_DIR" ] && echo "staging at $CACHE_DIR" || echo cache-less )
=============================================================================
EOB
printf '\033[0m'

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
# LOUD, not best-effort. (`nvmeof disconnect` takes the SUBNQN positionally.)
gone=0
for attempt in 1 2 3; do
  # enumerate LIVE prefixed NQNs (robust across topology changes — the
  # reset-v4 4-wide layout, kind-split node names, drift) and disconnect
  # each.
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

# ---------- 2. Storage nodes: enumerate-teardown, recreate, re-share --------
for h in "${HOSTS[@]}"; do
  IFS=: read -r name ip1 ip2 <<<"$h"
  log "2/5 $name ($ip1): enumeration teardown + rebuild backings + re-share"

  # ENUMERATION-BASED teardown (the oss2 lesson, journaled 2026-08-01):
  # sweep EVERY nvmet port whose addr_traddr matches this node's fabric IPs
  # and EVERY subsystem under our NQN prefix — whatever a previous epoch
  # named them. Ports go FIRST (unlink subsystem links, then rmdir the
  # port: that releases the listeners), then namespaces -> subsystems.
  # Expectation-named teardown ("unshare $BASE-dN for N in 0..15") left
  # orphan exports+listeners on oss2 and poisoned the client reconnect.
  # NOTE: the env prefix must be quoted AS ONE REMOTE WORD-SEQUENCE: ssh
  # concatenates its argv with spaces and the REMOTE shell re-parses it, so
  # an unquoted-remotely IPS="$ip1 $ip2" splits — the remote runs
  # `IPS=ip1` and then tries to EXECUTE ip2 ("command not found", first-run
  # 2026-08-02). The single-word assignments below (NQN=..., NAME=...)
  # survive that re-parse; only multi-word values hit it.
  "${SSH[@]}" "root@$ip1" "NQN_PREFIX='$NQN_PREFIX' IPS='$ip1 $ip2' bash -s" <<'EOS'
set -u
cfg=/sys/kernel/config/nvmet
[ -d "$cfg" ] || exit 0   # nvmet not loaded => nothing exported
# 2a. ports whose traddr belongs to this node's fabric IPs: unlink, rmdir.
for p in "$cfg"/ports/*; do
  [ -d "$p" ] || continue
  tr=$(cat "$p/addr_traddr" 2>/dev/null || echo "")
  case " $IPS " in
    *" $tr "*)
      for l in "$p"/subsystems/*; do [ -L "$l" ] && rm -f "$l"; done
      for g in "$p"/ana_groups/*; do
        [ -d "$g" ] || continue
        case "$g" in */grp1) ;; *) rmdir "$g" 2>/dev/null || true;; esac
      done
      for r in "$p"/referrals/*; do [ -d "$r" ] && rmdir "$r" 2>/dev/null; done
      rmdir "$p" || echo "WARN: port $(basename "$p") (traddr $tr) not removable" >&2
      ;;
  esac
done
# 2b. subsystems under our prefix: disable+rmdir namespaces, unlink hosts,
#     rmdir the subsystem.
for s in "$cfg"/subsystems/"$NQN_PREFIX"*; do
  [ -d "$s" ] || continue
  for ns in "$s"/namespaces/*; do
    [ -d "$ns" ] || continue
    echo 0 > "$ns/enable" 2>/dev/null || true
    rmdir "$ns" || echo "WARN: namespace $ns not removable" >&2
  done
  for hl in "$s"/allowed_hosts/*; do [ -L "$hl" ] && rm -f "$hl"; done
  rmdir "$s" || echo "WARN: subsystem $(basename "$s") not removable" >&2
done
# loud residue check: anything left under the prefix or on our IPs is a bug
left=$(ls -d "$cfg"/subsystems/"$NQN_PREFIX"* 2>/dev/null | wc -l)
[ "$left" -eq 0 ] || { echo "FATAL: $left prefixed subsystems survived teardown" >&2; exit 1; }
exit 0
EOS

  # meta namespace: memory-backed null_blk, one per node.
  "${SSH[@]}" "root@$ip1" REMOTE_SQZ="$REMOTE_SQZ" NAME="$name" NQN="$NQN_PREFIX:$name-m0" \
      IP1="$ip1" IP2="$ip2" SIZE_MB="$MDS_SIZE_MB" 'bash -s' <<'EOS'
set -euo pipefail
modprobe null_blk nr_devices=0 2>/dev/null || true
d="/sys/kernel/config/nullb/$NAME-m0"
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

  # data namespaces: OSS_NAMESPACES per node, one subsystem each.
  if [ "$OSS_BACKING" = "nullblk" ]; then
    "${SSH[@]}" "root@$ip1" REMOTE_SQZ="$REMOTE_SQZ" NAME="$name" BASE="$NQN_PREFIX:$name" \
        IP1="$ip1" IP2="$ip2" SIZE_MB="$OSS_NULLB_MB" COUNT="$OSS_NAMESPACES" META_MB="$MDS_SIZE_MB" 'bash -s' <<'EOS'
set -euo pipefail
modprobe null_blk nr_devices=0 2>/dev/null || true
free_mb=$(awk '/MemAvailable/ {print int($2/1024)}' /proc/meminfo)
total_mb=$((SIZE_MB * COUNT + META_MB))
[ "$free_mb" -gt "$total_mb" ] || \
  echo "WARNING: $COUNT x ${SIZE_MB} MiB nullblk + ${META_MB} MiB meta = ${total_mb} MiB exceeds MemAvailable ${free_mb} MiB — writes can OOM this node" >&2
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
done

# ---------- 3. Client: reconnect (both paths), multipath policy -------------
log "3/5 client: connect both paths per subsystem ($(( ${#HOSTS[@]} * (1 + OSS_NAMESPACES) )) subsystems x 2 paths)"
path_live() {  # path_live <nqn> <traddr> — is this exact path already connected?
  local c
  for c in /sys/class/nvme/nvme*; do
    [ -e "$c/subsysnqn" ] || continue
    [ "$(cat "$c/subsysnqn")" = "$1" ] || continue
    grep -q "traddr=$2," "$c/address" 2>/dev/null && return 0
  done
  return 1
}
# Build the full subsystem list: meta (m0) per node in HOSTS order first,
# then data (d0..dN) node-major in HOSTS order — the format URIs below
# reproduce this order deterministically.
META_SUFFIXES=(); DATA_SUFFIXES=()
for h in "${HOSTS[@]}"; do
  IFS=: read -r name _ _ <<<"$h"
  META_SUFFIXES+=("$name-m0")
done
for h in "${HOSTS[@]}"; do
  IFS=: read -r name _ _ <<<"$h"
  for i in $(seq 0 $((OSS_NAMESPACES - 1))); do DATA_SUFFIXES+=("$name-d$i"); done
done
[ "${#META_SUFFIXES[@]}" -ge 1 ] || die "no meta namespaces configured"
[ "${#DATA_SUFFIXES[@]}" -ge 1 ] || die "no data namespaces configured"

# host_ips <suffix> -> "ip1 ip2" of the node that serves it
host_ips() {
  local h name ip1 ip2
  for h in "${HOSTS[@]}"; do
    IFS=: read -r name ip1 ip2 <<<"$h"
    case "$1" in "$name"-m*|"$name"-d*) echo "$ip1 $ip2"; return 0;; esac
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
log "4/5 format (derived meta routing width; ${#META_SUFFIXES[@]} meta volumes, ${#DATA_SUFFIXES[@]} data namespaces)"
mkdir -p "$MOUNTPOINT"
meta_devs=""; for s in "${META_SUFFIXES[@]}"; do meta_devs+="${meta_devs:+,}${DEV[$s]}"; done
data_devs=""; for s in "${DATA_SUFFIXES[@]}"; do data_devs+="${data_devs:+,}${DEV[$s]}"; done
META_URI="sqmeta://$meta_devs"
DATA_URI="sqdata://$data_devs"
# NO --meta-slots: the routing width is derived (bit 6); the flag is a hard
# error naming its successors.
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
mkdir -p "$LOG_DIR"
"$SQZ" mount "$META_URI" "$MOUNTPOINT" --daemon "${MOUNT_EXTRA[@]}"
sleep 2
mountpoint -q "$MOUNTPOINT" || die "mount did not come up"
commit=$(grep -oE '"build_commit": *"[0-9a-f]*"' "$MOUNTPOINT/.stats" | head -1)
width=$(grep -oE '"meta_routing_width": *[0-9]+' "$MOUNTPOINT/.stats" | head -1 || true)
echo "mounted: $MOUNTPOINT  daemon $commit  ${width:-meta_routing_width: n/a}"
echo
echo "reset-v5 converged cluster ready (${#META_SUFFIXES[@]} meta + ${#DATA_SUFFIXES[@]} data). First move: the post-reset"
echo "baseline bracket per docs/reset-v5-window-plan.md (fresh_write_pass fill,"
echo "then the canonical table's rows — label every row reset-v5)."
