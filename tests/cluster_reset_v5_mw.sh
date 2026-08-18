#!/usr/bin/env bash
#
# cluster_reset_v5_mw.sh — cluster_reset_v4.sh extended to the MULTI-WRITER
# fleet shape (v4 LINEAGE: the CONFIG block, HOSTS map, enumeration-based
# teardown, both-path connect, cache-less format core are v4's VERBATIM —
# v4 itself stays untouched as the single-mount reset).
#
# What v5-mw adds on top of v4 (the FIELD MPI-IO campaign,
# .benchmarks/2026-08-18-mw-field-mpiio-prep.md):
#   * PR ASSERTION on every storage node: the product `nvmeof share` verb
#     already writes resv_enable=1 BEFORE enable when the kernel offers the
#     knob (src/nvmeof/nvmet.rs — v4's targets were never "missing" it on a
#     capable kernel), but on a kernel WITHOUT nvmet PR support the verb
#     only prints a note and serves DETECTION-grade. The S9 multi-writer
#     arm REFUSES on a non-PR substrate, so v5 asserts resv_enable=1 per
#     namespace at target-build time and dies loud naming the node-kernel
#     remedy (nvmet PR = mainline v6.13+, some distros backport it; the sqz kernel RPMs ship it).
#   * Client-side PR verify after connect: `nvme resv-report` must succeed
#     on every DATA namespace (end-to-end proof the fabric transports PR).
#   * After the format: mounts the AUTHORITY at $MOUNTPOINT with the
#     multi-writer arm and $COWRITERS co-writer mounts at $MOUNTPOINT-cw1..K
#     with the EXACT tests/mw_fleet.sh recipe (FLEET_SHARE divisor,
#     membership bind, stable MW port, roster harvested from each
#     co-writer's own rung-3 refusal, authority re-arm with the roster,
#     RANGE_CUSTODY on every co-writer), readiness-gated the way mw_fleet
#     gates (WERO/ADMITTED log lines + posture greps on each mount's own
#     .stats).
#   * Prints the ready-to-paste SQZ_MWMATRIX_MOUNTS line for the s11-mpiio
#     MPI-IO row (tests/run_mw_matrix.sh external-mounts mode).
#   * --dry-run: prints every ssh/format/mount command without executing —
#     the field preflight.
#
# CO-LOCATED shape (docs/operations.md §Multi-writer co-writer mounts): the
# authority and every co-writer mount on THIS client and share the box's
# default NVMe host identity (/etc/nvme/hostnqn) — no explicit per-mount
# hostnqn/hostid, no fabric_endpoint records, no host-scoped-subsystem
# kernel needed (patch 0030 is multi-identity-only). This keeps v4's
# both-path connect + round-robin multipath intact (an explicit identity
# would demand daemon-owned single-path connects from fabric_endpoint
# records — the mw_fleet rig arms that surface to EXERCISE it; the field
# recipe keeps the proven co-located default-identity shape instead, and
# the delta is stated in the prep note).
#
# Run FROM THE CLIENT as root. Uses passwordless root ssh to the storage
# nodes. DESTROYS ALL FILESYSTEM DATA (fresh backings, fresh format).
#
# Edit the CONFIG block, then:  sudo tests/cluster_reset_v5_mw.sh
# Preflight print-only:         tests/cluster_reset_v5_mw.sh --dry-run
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
                            # posture when local disk is slower than the fabric —
                            # the SATA-cache lesson)
MDS_SIZE_MB=8192            # null_blk size per metadata namespace (one per node)

# Data-plane width. Every data namespace is its own subsystem (own nvme-tcp
# queue set), named "$NQN_PREFIX:<node>-d<i>".
OSS_NAMESPACES=2            # data namespaces per node (converged epoch: 2)

# Data backing (v4's table verbatim — zram = overwrite/reclaim venue,
# nullblk = clean throughput ceiling).
OSS_BACKING="nullblk"       # zram | nullblk
OSS_ZRAM_SIZE="64G"         # zram disksize per data namespace
OSS_ZRAM_ALGO="auto"
OSS_NULLB_MB=49152          # nullblk size (MiB) per data namespace
LOG_DIR="/scratch/tmp/logs" # per-mount --log-file home (the 2026-08-02
                            # convention; /tmp is banned for agent artifacts)

# ---- the multi-writer fleet (v5-mw additions) -------------------------------
COWRITERS=8                 # co-writer mounts at $MOUNTPOINT-cw1..K (the §9.5
                            # design shape is 8; 8 mounts x 4 procs = 32 ranks
                            # on the 32-CPU client, the local verdict geometry)
MW_PORT=45999               # the authority's custody/publish bind — a STABLE
                            # port (the mw_fleet rung-10 finding: `auto` mints
                            # a fresh ephemeral port per incarnation, so a
                            # remounted authority would be undialable by the
                            # recorded endpoint forever)
MEMBERSHIP_BIND="auto"      # SQUEEZEFS_MEMBERSHIP_BIND for the authority (the
                            # S9 arm's rung 4 requires the membership plane)
RANGE_CUSTODY=1             # SQUEEZEFS_RANGE_CUSTODY on every co-writer — the
                            # s11-mpiio row REQUIRES it (product default stays
                            # OFF; the row's engagement gate convicts an
                            # unarmed fleet)
FLEET_SHARE=""              # SQUEEZEFS_FLEET_SHARE per daemon; EMPTY = derived
                            # 1+COWRITERS (KD-MW-14: N co-located daemons
                            # divide the machine — the honest posture for 9
                            # daemons on one client; the local verdict rig ran
                            # share=1 by its N=1 quirk, priced in the prep note)
IOR_PROCS=4                 # --procs for the printed paste line (ranks =
                            # COWRITERS x IOR_PROCS)
# Mount flags for EVERY fleet mount. NOTE the delta from v4: no
# --interception — the proven mw_fleet multi-writer recipe mounts with
# --allow-other + --log-file only, and the MPI-IO row drives the kernel
# FUSE path (ior POSIX, no shim). Re-add interception on a later reset if a
# shim row needs it; keep the MW row on the proven shape first.
MOUNT_EXTRA=(--allow-other)
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=5)
# ============================================================================

DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    *) printf 'FATAL: unknown argument %s (only --dry-run)\n' "$arg" >&2; exit 1 ;;
  esac
done

log()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
die()  { printf 'FATAL: %s\n' "$*" >&2; exit 1; }

# runv: execute, or print the exact command under --dry-run.
runv() {
  if [ "$DRY_RUN" = 1 ]; then printf 'DRY: %s\n' "$*"; else "$@"; fi
}
# sshrun <target> <env-prefix-string>: remote body on stdin; printed
# verbatim under --dry-run.
sshrun() {
  local tgt="$1" envp="$2"
  if [ "$DRY_RUN" = 1 ]; then
    printf 'DRY: ssh %s "%s bash -s" <<'\''EOS'\''\n' "$tgt" "$envp"
    sed 's/^/  | /'
    printf 'DRY: EOS\n'
    return 0
  fi
  "${SSH[@]}" "$tgt" "$envp bash -s"
}

if [ "$DRY_RUN" = 0 ]; then
  [ "$(id -u)" -eq 0 ] || die "run as root (sudo); --dry-run works unprivileged"
  [ -x "$SQZ" ] || die "client binary not executable: $SQZ"
  command -v nvme >/dev/null 2>&1 || die "nvme-cli is required (PR verify: nvme resv-report)"
  command -v python3 >/dev/null 2>&1 || die "python3 is required (stats-inode readiness gates)"
  # FUSE-over-io_uring is REQUIRED by every mount (the transport is not
  # optional); fail here with the remedy instead of K+1 mount failures.
  [ -e /sys/module/fuse/parameters/enable_uring ] ||
    die "client kernel exposes no fuse.enable_uring — FUSE-over-io_uring is required (see docs/field-mpiio-runbook.md preflight 1)"
fi
case "$OSS_BACKING" in zram|nullblk) ;; *) die "OSS_BACKING must be zram or nullblk (got: $OSS_BACKING)";; esac
[[ "$OSS_NAMESPACES" =~ ^[1-9][0-9]*$ ]] || die "OSS_NAMESPACES must be a positive integer (got: $OSS_NAMESPACES)"
[[ "$COWRITERS" =~ ^[1-9][0-9]*$ ]] && [ "$COWRITERS" -ge 2 ] || die "COWRITERS must be >= 2 (the s11-mpiio leg's floor; got: $COWRITERS)"
FLEET_N=$(( 1 + COWRITERS ))
[ -n "$FLEET_SHARE" ] || FLEET_SHARE="$FLEET_N"

# ---------- venue-epoch banner (loud, before the YES gate) ------------------
printf '\033[1m'
cat <<EOB
=============================================================================
 RESET-V5-MW — THE MULTI-WRITER FLEET EPOCH (v4 core + the S9 arm)
   1 meta + ${OSS_NAMESPACES} data namespaces per node x ${#HOSTS[@]} nodes
   = ${#HOSTS[@]} meta volumes + $(( ${#HOSTS[@]} * OSS_NAMESPACES )) data namespaces, every node symmetric
   + 1 AUTHORITY mount ($MOUNTPOINT) + $COWRITERS co-writer mounts
     ($MOUNTPOINT-cw1..cw$COWRITERS), SQUEEZEFS_FLEET_SHARE=$FLEET_SHARE per daemon
 Node map (name -> fabric paths):
EOB
for h in "${HOSTS[@]}"; do
  IFS=: read -r name ip1 ip2 <<<"$h"
  printf '   %-6s -> %s / %s   (%s-{m0' "$name" "$ip1" "$ip2" "$name"
  for i in $(seq 0 $((OSS_NAMESPACES - 1))); do printf ',d%s' "$i"; done
  printf '})\n'
done
cat <<EOB
 PR posture: resv_enable=1 ASSERTED per namespace (S9 refuses non-PR);
   client verify: nvme resv-report per data namespace.
 Meta routing: DYNAMIC (bit 6); format: default = multi-writer-capable
   (the rung-10b flip); data backing: $OSS_BACKING; $( [ -n "$CACHE_DIR" ] && echo "staging at $CACHE_DIR" || echo cache-less ).
=============================================================================
EOB
printf '\033[0m'

if [ "$DRY_RUN" = 1 ]; then
  echo "DRY-RUN: printing every ssh/format/mount command; nothing executes."
else
  echo "This DESTROYS all data on the cluster volumes and rebuilds from scratch."
  read -r -p "Type YES to continue: " ans
  [ "$ans" = "YES" ] || die "aborted"
fi

# ---------- 1. Client teardown ----------------------------------------------
log "1/6 client: unmount fleet + disconnect"
# Co-writers first (they dial the authority), authority last — then any
# stray v4 single mount at $MOUNTPOINT is the same path.
for ((i = COWRITERS; i >= 1; i--)); do
  m="$MOUNTPOINT-cw$i"
  if [ "$DRY_RUN" = 1 ]; then
    runv umount "$m"
  elif mountpoint -q "$m" 2>/dev/null; then
    umount "$m" || umount -l "$m" || true
  fi
done
if [ "$DRY_RUN" = 1 ]; then
  runv umount "$MOUNTPOINT"
  runv pkill -f "squeezefs mount"
elif mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
  umount "$MOUNTPOINT" || umount -l "$MOUNTPOINT" || true
  sleep 2
fi
if [ "$DRY_RUN" = 0 ]; then
  pkill -f 'squeezefs moun[t]' 2>/dev/null || true
  sleep 1
fi

# disconnect with verification (v4 verbatim): survivors auto-reconnect to
# rebuilt shares and poison step 3 with duplicate-connect failures.
if [ "$DRY_RUN" = 1 ]; then
  runv "$SQZ" nvmeof disconnect "<every live $NQN_PREFIX* subnqn, enumerated from /sys/class/nvme/*/subsysnqn; retried x3 until none remain>"
else
  gone=0
  for attempt in 1 2 3; do
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
fi

# ---------- 2. Storage nodes: enumerate-teardown, recreate, re-share --------
for h in "${HOSTS[@]}"; do
  IFS=: read -r name ip1 ip2 <<<"$h"
  log "2/6 $name ($ip1): enumeration teardown + rebuild backings + re-share + PR assert"

  # LEDGER teardown first, via the product verb (v4 verbatim — the configfs
  # sweep removes live nvmet state but the product SHARE LEDGER survives it,
  # and `nvmeof share` refuses an NQN the ledger already carries).
  sshrun "root@$ip1" "REMOTE_SQZ='$REMOTE_SQZ' NQN_PREFIX='$NQN_PREFIX'" <<'EOS'
set -u
[ -x "$REMOTE_SQZ" ] || exit 0
"$REMOTE_SQZ" nvmeof list 2>/dev/null \
  | awk -v p="$NQN_PREFIX" '$1 == "NQN:" && index($2, p ":") == 1 { print $2 }' \
  | while read -r nqn; do
      echo "  ledger unshare: $nqn"
      "$REMOTE_SQZ" nvmeof unshare "$nqn" || echo "WARN: unshare $nqn failed (share will refuse loudly if the row survives)" >&2
    done
exit 0
EOS

  # ENUMERATION-BASED configfs teardown (v4 verbatim — the oss2 lesson:
  # ports whose traddr matches this node's fabric IPs FIRST, then every
  # subsystem under our prefix; never expectation-named teardown).
  sshrun "root@$ip1" "NQN_PREFIX='$NQN_PREFIX' IPS='$ip1 $ip2'" <<'EOS'
set -u
cfg=/sys/kernel/config/nvmet
[ -d "$cfg" ] || exit 0   # nvmet not loaded => nothing exported
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
left=$(ls -d "$cfg"/subsystems/"$NQN_PREFIX"* 2>/dev/null | wc -l)
[ "$left" -eq 0 ] || { echo "FATAL: $left prefixed subsystems survived teardown" >&2; exit 1; }
exit 0
EOS

  # meta namespace: memory-backed null_blk, one per node (v4 verbatim).
  sshrun "root@$ip1" "REMOTE_SQZ='$REMOTE_SQZ' NAME='$name' NQN='$NQN_PREFIX:$name-m0' IP1='$ip1' IP2='$ip2' SIZE_MB='$MDS_SIZE_MB'" <<'EOS'
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

  # data namespaces: OSS_NAMESPACES per node, one subsystem each (v4 verbatim).
  if [ "$OSS_BACKING" = "nullblk" ]; then
    sshrun "root@$ip1" "REMOTE_SQZ='$REMOTE_SQZ' NAME='$name' BASE='$NQN_PREFIX:$name' IP1='$ip1' IP2='$ip2' SIZE_MB='$OSS_NULLB_MB' COUNT='$OSS_NAMESPACES' META_MB='$MDS_SIZE_MB'" <<'EOS'
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
    sshrun "root@$ip1" "REMOTE_SQZ='$REMOTE_SQZ' BASE='$NQN_PREFIX:$name' IP1='$ip1' IP2='$ip2' ZSIZE='$OSS_ZRAM_SIZE' ZALGO='$OSS_ZRAM_ALGO' COUNT='$OSS_NAMESPACES'" <<'EOS'
set -euo pipefail
modprobe zram 2>/dev/null || true
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

  # v5-mw: the PR ASSERTION. The product share verb writes resv_enable=1
  # before enable WHEN THE KERNEL OFFERS THE KNOB and only prints a note
  # when it does not (detection-grade). The S9 arm refuses non-PR at mount
  # time; fail HERE at build time instead, naming the node and the remedy.
  sshrun "root@$ip1" "NQN_PREFIX='$NQN_PREFIX' NODE='$name'" <<'EOS' \
    || die "storage node $name: PR assertion failed (see above) — the S9 multi-writer arm would refuse this substrate"
set -u
cfg=/sys/kernel/config/nvmet
fail=0; n=0
for s in "$cfg"/subsystems/"$NQN_PREFIX"*; do
  [ -d "$s" ] || continue
  n=$((n + 1))
  r="$s/namespaces/1/resv_enable"
  if [ ! -f "$r" ]; then
    echo "FATAL[$NODE]: $(basename "$s"): nvmet exposes NO resv_enable knob — this node's kernel lacks nvmet Persistent Reservations (mainline v6.13+; the sqz kernel series ships it). Update the NODE kernel/nvmet module." >&2
    fail=1
  elif [ "$(cat "$r")" != "1" ]; then
    echo "FATAL[$NODE]: $(basename "$s"): resv_enable=$(cat "$r") (want 1 — the product share verb writes it before enable when the knob exists)" >&2
    fail=1
  fi
done
[ "$n" -gt 0 ] || { echo "FATAL[$NODE]: no prefixed subsystems found after share" >&2; exit 1; }
[ "$fail" -eq 0 ] && echo "  PR assert: $n namespace(s) resv_enable=1 on $NODE"
exit "$fail"
EOS
done

# ---------- 3. Client: reconnect (both paths), multipath, PR verify ---------
log "3/6 client: connect both paths per subsystem ($(( ${#HOSTS[@]} * (1 + OSS_NAMESPACES) )) subsystems x 2 paths) + PR verify"
path_live() {  # path_live <nqn> <traddr> — is this exact path already connected?
  local c
  for c in /sys/class/nvme/nvme*; do
    [ -e "$c/subsysnqn" ] || continue
    [ "$(cat "$c/subsysnqn")" = "$1" ] || continue
    grep -q "traddr=$2," "$c/address" 2>/dev/null && return 0
  done
  return 1
}
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

host_ips() { # host_ips <suffix> -> "ip1 ip2" of the node that serves it
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
    if [ "$DRY_RUN" = 1 ]; then
      runv "$SQZ" nvmeof connect --ip "$ip" --subnqn "$NQN_PREFIX:$suffix"
    elif path_live "$NQN_PREFIX:$suffix" "$ip"; then
      echo "  $suffix via $ip: already connected — keeping"
    else
      "$SQZ" nvmeof connect --ip "$ip" --subnqn "$NQN_PREFIX:$suffix"
    fi
  done
done

declare -A DEV
if [ "$DRY_RUN" = 1 ]; then
  for suffix in "${META_SUFFIXES[@]}" "${DATA_SUFFIXES[@]}"; do DEV[$suffix]="<dev:$suffix>"; done
  runv nvme resv-report "<dev:each-data-namespace>"
else
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
  # v5-mw: end-to-end PR verify — resv-report must SUCCEED on every DATA
  # namespace (the report may be empty; the command failing means the
  # fabric does not transport PR and the S9 arm will refuse).
  for suffix in "${DATA_SUFFIXES[@]}"; do
    nvme resv-report "${DEV[$suffix]}" >/dev/null 2>&1 ||
      die "nvme resv-report ${DEV[$suffix]} ($suffix) FAILED — the fabric does not transport Persistent Reservations end-to-end (node kernel nvmet PR = mainline v6.13+; see docs/field-mpiio-runbook.md preflight 2)"
  done
  echo "  PR verify: resv-report OK on all ${#DATA_SUFFIXES[@]} data namespaces"
fi

# ---------- 4. Format --------------------------------------------------------
log "4/6 format (derived meta routing width; ${#META_SUFFIXES[@]} meta volumes, ${#DATA_SUFFIXES[@]} data namespaces; default = multi-writer-capable)"
runv mkdir -p "$MOUNTPOINT"
meta_devs=""; for s in "${META_SUFFIXES[@]}"; do meta_devs+="${meta_devs:+,}${DEV[$s]}"; done
data_devs=""; for s in "${DATA_SUFFIXES[@]}"; do data_devs+="${data_devs:+,}${DEV[$s]}"; done
META_URI="sqmeta://$meta_devs"
DATA_URI="sqdata://$data_devs"
FORMAT_ARGS=("$META_URI" "$DATA_URI")
if [ -n "$CACHE_DIR" ]; then
  runv mkdir -p "$CACHE_DIR"
  FORMAT_ARGS+=(--disk-cache-paths "$CACHE_DIR")
else
  echo "  cache-less format (CACHE_DIR empty)"
fi
runv "$SQZ" format "${FORMAT_ARGS[@]}"

# ---------- 5. Mount the multi-writer fleet ----------------------------------
# The tests/mw_fleet.sh recipe over the real fabric (CO-LOCATED shape — see
# the header note): authority phase 1 (arm, no roster) -> harvest each
# co-writer's durable enrollment id from its rung-3 refusal -> re-arm the
# authority with the roster (enrollment is the AUTHORITY's durable act, a
# new era) -> mount the admitted co-writers. SUDO_* is scrubbed from every
# daemon launch so the daemon posture (mount ownership, admin-lane
# identity) is root-deterministic regardless of sudo-vs-root-shell (the
# admin lane admits peercred uid 0 — src/ipc_host.rs).
log "5/6 mount the fleet: authority + $COWRITERS co-writers (FLEET_SHARE=$FLEET_SHARE, MW port $MW_PORT)"
runv mkdir -p "$LOG_DIR"

AUTH_LOG="$LOG_DIR/sqz-mw-authority.log"
MW_ENDPOINT=""

# One flattened stats-inode field (the JSON nests under "metrics") — the
# mw_fleet stat_field helper verbatim.
stat_field() { # mountpoint key -> value
  cat "$1/.stats" | python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(sys.stdin)
print(flat(root.get("metrics", root), {}).get(sys.argv[1], ""))' "$2"
}

wait_for() { # description tries cmd...
  local what="$1" tries="$2" i
  shift 2
  for ((i = 0; i < tries; i++)); do
    "$@" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  die "timed out waiting for $what"
}

poll_stat() { # mountpoint key want tries what
  local mnt="$1" key="$2" want="$3" tries="$4" what="$5" v i
  v=""
  for ((i = 0; i < tries; i++)); do
    v="$(stat_field "$mnt" "$key" 2>/dev/null || true)"
    [ "$v" = "$want" ] && return 0
    sleep 0.5
  done
  die "$what: $key='$v' (want $want)"
}

unmount_and_reap() { # mountpoint
  local mnt="$1" pid t
  pid="$(pgrep -f "squeezefs.*mount.*$mnt" | head -1 || true)"
  if awk -v m="$mnt" '$2==m {f=1} END {exit !f}' /proc/mounts; then
    env -u SUDO_UID -u SUDO_GID -u SUDO_USER "$SQZ" umount "$mnt" >/dev/null 2>&1 || true
  fi
  if awk -v m="$mnt" '$2==m {f=1} END {exit !f}' /proc/mounts; then
    umount -l "$mnt" 2>/dev/null || true
  fi
  wait_for "unmount of $mnt" 60 bash -c "! awk -v m='$mnt' '\$2==m {f=1} END {exit !f}' /proc/mounts"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    for ((t = 0; t < 20; t++)); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.5
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
}

mount_authority() { # [roster]
  local roster="${1:-}"
  local env_args=(env -u SUDO_UID -u SUDO_GID -u SUDO_USER
    "SQUEEZEFS_FLEET_SHARE=$FLEET_SHARE"
    "SQUEEZEFS_IPC_ALLOW_DEV=1"
    "SQUEEZEFS_MEMBERSHIP_BIND=$MEMBERSHIP_BIND"
    "SQUEEZEFS_MULTI_WRITER=1"
    "SQUEEZEFS_MW_BIND=0.0.0.0:$MW_PORT")
  [ -n "$roster" ] && env_args+=("SQUEEZEFS_MW_MEMBERS=$roster")
  if [ "$DRY_RUN" = 1 ]; then
    runv "${env_args[@]}" "$SQZ" mount "$META_URI" "$MOUNTPOINT" --daemon "${MOUNT_EXTRA[@]}" --log-file "$AUTH_LOG"
    MW_ENDPOINT="<MW_ENDPOINT-from-authority-log>"
    return 0
  fi
  "${env_args[@]}" "$SQZ" mount "$META_URI" "$MOUNTPOINT" \
    --daemon "${MOUNT_EXTRA[@]}" --log-file "$AUTH_LOG" \
    >"$LOG_DIR/sqz-mw-authority.mount.out" 2>&1 ||
    die "authority mount failed: $(cat "$LOG_DIR/sqz-mw-authority.mount.out")"
  wait_for "authority mountpoint" 120 mountpoint -q "$MOUNTPOINT"
  wait_for "authority stats inode" 120 test -s "$MOUNTPOINT/.stats"
  # Rung-8 engagement gates (mw_fleet mount_member verbatim): the WERO hold
  # must stand (fence-mode gauge + the acquire log line) and the S6 plane
  # must own — a silently-degraded arm is contractually impossible, so a
  # miss here is a refusal we somehow did not see; die loud either way.
  poll_stat "$MOUNTPOINT" data_plane_fence_mode 1 120 \
    "authority: the S7 WERO hold did not engage (log: $AUTH_LOG)"
  grep -q "data-plane WERO (rtype 3) acquired" "$AUTH_LOG" ||
    die "authority log carries no 'data-plane WERO (rtype 3) acquired' line (log: $AUTH_LOG)"
  poll_stat "$MOUNTPOINT" membership_mode owner 120 \
    "authority: the S6 membership plane did not engage (log: $AUTH_LOG)"
  MW_ENDPOINT="$(sed -n 's/.*MULTI-WRITER ARMED (DLM S9) on \(.*\): era.*/\1/p' "$AUTH_LOG" | tail -1)"
  [ -n "$MW_ENDPOINT" ] ||
    die "authority log carries no 'MULTI-WRITER ARMED (DLM S9) on <endpoint>' line (log: $AUTH_LOG)"
  echo "  authority up at $MOUNTPOINT (WERO held, membership owner, MW endpoint $MW_ENDPOINT)"
}

cowriter_env() { # -> the co-writer daemon env (mw_fleet lines 779-784 verbatim)
  echo env -u SUDO_UID -u SUDO_GID -u SUDO_USER \
    "SQUEEZEFS_FLEET_SHARE=$FLEET_SHARE" \
    "SQUEEZEFS_IPC_ALLOW_DEV=1" \
    "SQUEEZEFS_MULTI_WRITER=1" \
    "SQUEEZEFS_MW_ROLE=co-writer" \
    "SQUEEZEFS_MW_AUTHORITY=$MW_ENDPOINT" \
    "$([ "$RANGE_CUSTODY" = "1" ] && echo SQUEEZEFS_RANGE_CUSTODY=1 || echo SQUEEZEFS_RANGE_CUSTODY=0)"
}

probe_cowriter_id() { # mountpoint probe-out probe-log -> echoes the enrollment id
  local mnt="$1" out="$2" plog="$3" id
  mkdir -p "$mnt"
  # A co-writer mount against a roster that does not name it is REFUSED at
  # rung 3, and the refusal prints this mountpoint's durable enrollment id
  # (KD-MW-2 `node_{16 hex}.m{8 hex}`). gather_admission mutates nothing
  # before rung 5 — the probe is side-effect-free (mw_fleet verbatim).
  # shellcheck disable=SC2046 # cowriter_env is a deliberate word list
  if $(cowriter_env) "$SQZ" mount "$META_URI" "$mnt" \
    --daemon "${MOUNT_EXTRA[@]}" --log-file "$plog" >"$out" 2>&1; then
    die "co-writer PROBE mount at $mnt was ADMITTED against an empty roster — rung 3 did not engage (out: $out)"
  fi
  id="$(grep -o "Add 'node_[0-9a-f.m]*'" "$out" | head -1 | sed "s/^Add '//; s/'$//")"
  [ -n "$id" ] || die "co-writer probe refusal at $mnt carries no enrollment id (want the rung-3 \"Add 'node_…'\" remedy; out: $out)"
  echo "$id"
}

mount_cowriter() { # idx
  local i="$1" mnt clog
  mnt="$MOUNTPOINT-cw$i"
  clog="$LOG_DIR/sqz-mw-cw$i.log"
  mkdir -p "$mnt"
  # shellcheck disable=SC2046 # cowriter_env is a deliberate word list
  $(cowriter_env) "$SQZ" mount "$META_URI" "$mnt" \
    --daemon "${MOUNT_EXTRA[@]}" --log-file "$clog" \
    >"$LOG_DIR/sqz-mw-cw$i.mount.out" 2>&1 ||
    die "co-writer $i mount failed: $(cat "$LOG_DIR/sqz-mw-cw$i.mount.out")"
  wait_for "co-writer $i mountpoint" 120 mountpoint -q "$mnt"
  wait_for "co-writer $i stats inode" 120 test -s "$mnt/.stats"
  # Rung-9 engagement (mw_fleet verbatim): the five-rung ladder ADMITTED
  # and the mount is the co-writer posture, never a silently-degraded one.
  grep -q "CO-WRITER ADMITTED" "$clog" ||
    die "co-writer $i log carries no 'CO-WRITER ADMITTED' line — the admission ladder did not engage (log: $clog)"
  poll_stat "$mnt" mount_posture co-writer 40 "co-writer $i posture (log: $clog)"
  poll_stat "$mnt" membership_mode member 120 "co-writer $i: the S6 join did not engage (log: $clog)"
  echo "  co-writer $i up at $mnt (ADMITTED, posture=co-writer, membership=member)"
}

if [ "$DRY_RUN" = 1 ]; then
  mount_authority
  echo "DRY: # phase 2 — per-co-writer enrollment-id probes (each mount attempt is REFUSED"
  echo "DRY: #           at rung 3; the refusal prints the durable id the roster needs):"
  for ((i = 1; i <= COWRITERS; i++)); do
    # shellcheck disable=SC2046
    runv $(cowriter_env) "$SQZ" mount "$META_URI" "$MOUNTPOINT-cw$i" --daemon "${MOUNT_EXTRA[@]}" --log-file "$LOG_DIR/sqz-mw-cw$i.probe.log"
  done
  echo "DRY: # phase 3 — re-arm the authority with the harvested roster (a new era):"
  runv "$SQZ" umount "$MOUNTPOINT"
  runv env -u SUDO_UID -u SUDO_GID -u SUDO_USER \
    "SQUEEZEFS_FLEET_SHARE=$FLEET_SHARE" "SQUEEZEFS_IPC_ALLOW_DEV=1" \
    "SQUEEZEFS_MEMBERSHIP_BIND=$MEMBERSHIP_BIND" "SQUEEZEFS_MULTI_WRITER=1" \
    "SQUEEZEFS_MW_BIND=0.0.0.0:$MW_PORT" "SQUEEZEFS_MW_MEMBERS=<id1>,<id2>,...,<id$COWRITERS>" \
    "$SQZ" mount "$META_URI" "$MOUNTPOINT" --daemon "${MOUNT_EXTRA[@]}" --log-file "$AUTH_LOG"
  echo "DRY: # phase 4 — mount the admitted co-writers:"
  for ((i = 1; i <= COWRITERS; i++)); do
    # shellcheck disable=SC2046
    runv $(cowriter_env) "$SQZ" mount "$META_URI" "$MOUNTPOINT-cw$i" --daemon "${MOUNT_EXTRA[@]}" --log-file "$LOG_DIR/sqz-mw-cw$i.log"
  done
else
  mount_authority
  ROSTER=""
  for ((i = 1; i <= COWRITERS; i++)); do
    id="$(probe_cowriter_id "$MOUNTPOINT-cw$i" "$LOG_DIR/sqz-mw-cw$i.probe.out" "$LOG_DIR/sqz-mw-cw$i.probe.log")"
    echo "  co-writer $i enrollment id harvested: $id"
    ROSTER="${ROSTER:+$ROSTER,}$id"
  done
  echo "  re-arming the authority with the roster (a new era): $ROSTER"
  unmount_and_reap "$MOUNTPOINT"
  mount_authority "$ROSTER"
  for ((i = 1; i <= COWRITERS; i++)); do
    mount_cowriter "$i"
  done
fi

# ---------- 6. Ready ----------------------------------------------------------
log "6/6 fleet ready — the MPI-IO row's paste line"
MOUNT_LIST="$MOUNTPOINT"
for ((i = 1; i <= COWRITERS; i++)); do MOUNT_LIST="$MOUNT_LIST,$MOUNTPOINT-cw$i"; done
ROWDIR="$(dirname "$MOUNTPOINT")/mwmatrix-rows"
if [ "$DRY_RUN" = 0 ]; then
  commit=$(grep -oE '"build_commit": *"[0-9a-f]*"' "$MOUNTPOINT/.stats" | head -1 || true)
  echo "authority: $MOUNTPOINT  daemon ${commit:-build_commit: n/a}  MW endpoint $MW_ENDPOINT"
fi
cat <<EOB

reset-v5-mw fleet ready: 1 authority + $COWRITERS co-writers over
${#META_SUFFIXES[@]} meta + ${#DATA_SUFFIXES[@]} data namespaces (PR-armed, both paths, round-robin).
Run the MPI-IO row FROM THE REPO CHECKOUT on this client (rows land under
$ROWDIR):

  sudo SQZ_BIN=$SQZ \\
       SQZ_MWMATRIX_MOUNTS=$MOUNT_LIST \\
       SQZ_MWMATRIX_ROWDIR=$ROWDIR \\
       tests/run_mw_matrix.sh s11-mpiio --procs=$IOR_PROCS

Repeat = re-run this reset (fresh backings + fresh format per counted run),
then the paste line again. Compare against the LOCAL verdict row
(.benchmarks/2026-08-18-s11-mpiio-row.md + the rung-19 verdict in
.benchmarks/2026-08-18-s11-widthn-refs-fix.md) with the tier stated: this
fleet is a REAL nvme-tcp fabric (measured-real substrate class); the local
rows are measured-simulated (one box, nvmet-tcp localhost).
EOB
