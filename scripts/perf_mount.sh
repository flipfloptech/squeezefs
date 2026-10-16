#!/usr/bin/env bash
# perf_mount.sh — a shell with a mounted SqueezeFS built for BENCHMARKING: the
# dist profile (ReleaseSafe + fat LTO, charter §11, decision 0003 Q8), the
# release mount posture, targets sized for datasets that no RAM tier holds,
# fio/IOR/mdtest at hand, and a run record beside every session so a number
# can always be traced to the binary, the kernel, the disks and the load it
# was measured under.
#
#   scripts/perf_mount.sh                      # enter (first use: build dist, create, format, mount)
#   scripts/perf_mount.sh --fresh              # reformat the set first (its contents are dropped)
#   scripts/perf_mount.sh --rebuild            # rebuild the dist binary even if it names HEAD
#   scripts/perf_mount.sh --keep               # leave the set mounted on exit
#   scripts/perf_mount.sh --scrub              # fsck --offline --scrub on exit (default: no scrub)
#   scripts/perf_mount.sh --rm                 # macOS: delete the bench container and its volumes
#   scripts/perf_mount.sh -- <command…>        # run one command against the mount instead of a shell
#
#   macOS (Apple `container`, a microVM): a container `sqz-bench` of its own
#   (all host cores and 16 GiB by default — SQZ_PERF_CPUS / SQZ_PERF_MEM),
#   the image squeezefs-dev:ubuntu2404 (scripts/dev/Containerfile), each target
#   its own virtio-blk disk with SqueezeFS on a raw partition (the layout
#   dev_mount.sh uses: a 16 MiB ext4 the runtime can mount + MBR partition 1).
#     --mdts N (1|2, default 2)   --osts N (1..4, default 2)
#     --mdt-size SIZE (default 2G)   --ost-size SIZE (default 16G; sparse)
#   The VM's disks are image files on APFS: these numbers are SCOPING, never
#   the charter's bars (docs/gates/brackets.md).
#
#   Linux (native, the reference box): no container; root; real devices.
#     scripts/perf_mount.sh --mdt /dev/nvme0n1 [--mdt /dev/nvme1n1] \
#                           --ost /dev/nvme2n1 [--ost …] [--cache-dir DIR]
#     scripts/perf_mount.sh --ram …          # the RAM namespaces of scripts/perf/ramnvme.sh up
#   Format refuses a device carrying another filesystem unless --fresh (which
#   formats with --force). The staging ring lives under --cache-dir (default
#   /var/tmp/sqz-perf-cache — put it on NVMe). fio/ior/mdtest come from PATH.
#
# One mount at a time per host: macOS `/mnt/sqz` inside sqz-bench; Linux
# `/mnt/sqz-perf`. Exit = orderly `squeezefs umount`, wait for the daemon's
# exit, `fsck --offline` (with --scrub on request).
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ORIG_ARGS=("$@")
NAME=${SQZ_PERF_NAME:-sqz-bench}
VOL=${SQZ_PERF_VOLUME:-squeezefs-zig-bench}
IMAGE=${SQZ_PERF_IMAGE:-squeezefs-dev:ubuntu2404}
BASE_IMAGE=squeezefs-build:ubuntu2404
MEM=${SQZ_PERF_MEM:-16G}

fresh=0 rebuild=0 keep=0 remove=0 scrub=0
mdts=2 osts=2 mdt_size=2G ost_size=16G cache_dir=/var/tmp/sqz-perf-cache
mdt_devs=() ost_devs=() cmd=()
die() { echo "perf_mount: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --fresh) fresh=1 ;;
    --rebuild) rebuild=1 ;;
    --keep) keep=1 ;;
    --scrub) scrub=1 ;;
    --rm) remove=1 ;;
    --mdts) mdts=$2; shift ;;
    --osts) osts=$2; shift ;;
    --mdt-size) mdt_size=$2; shift ;;
    --ost-size) ost_size=$2; shift ;;
    --mdt) mdt_devs+=("$2"); shift ;;
    --ost) ost_devs+=("$2"); shift ;;
    --cache-dir) cache_dir=$2; shift ;;
    --ram) [ -r /run/sqz-ramnvme/devs ] || die "--ram: run scripts/perf/ramnvme.sh up first"
      while IFS== read -r r d; do case "$r" in mdt*) mdt_devs+=("$d") ;; ost*) ost_devs+=("$d") ;; esac; done < /run/sqz-ramnvme/devs ;;
    --) shift; cmd=("$@"); break ;;
    -h|--help) sed -n '2,37p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "perf_mount: unknown flag '$1' (see --help)" >&2; exit 2 ;;
  esac
  shift
done

case "$mdts" in 1|2) ;; *) die "--mdts must be 1 or 2" ;; esac
case "$osts" in 1|2|3|4) ;; *) die "--osts must be 1..4" ;; esac
# (`|| true`: a worktree mirrored to an ssh venue has a `.git` file naming a gitdir only the
# Mac has; the failed git ended the script silently under pipefail.)
dirty=$(git -C "$REPO" status --porcelain --untracked-files=no 2>/dev/null | head -1) || true
[ -n "$dirty" ] && echo "perf_mount: WARNING — the tree has uncommitted changes; the binary will say -dirty and its numbers cannot be attributed to a commit" >&2

# ---------------------------------------------------------------------------
# The session, run inside the container (macOS) or on the host (Linux).
# Arguments: REPO FRESH REBUILD KEEP SCRUB MODE MNT CACHE HOSTINFO TARGETS… -- CMD…
# TARGETS are role:name:source — source is `vol` (a container volume to lay
# out as a raw partition) or a device path (native).
# ---------------------------------------------------------------------------
read -r -d '' INNER <<'EOF' || true
set -euo pipefail
REPO=$1 FRESH=$2 REBUILD=$3 KEEP=$4 SCRUB=$5 MODE=$6 MNT=$7 CACHE=$8 HOSTINFO=$9; shift 9
targets=(); while [ $# -gt 0 ] && [ "$1" != -- ]; do targets+=("$1"); shift; done; [ $# -gt 0 ] && shift
die() { echo "perf_mount: $*" >&2; exit 1; }
STATE=/var/lib/sqz-perf; [ "$MODE" = container ] && STATE=/build/perf
TGT=$STATE/targets; LOG=$STATE/daemon.log
mkdir -p "$TGT" "$CACHE" "$STATE/runs"

[ "$(id -u)" = 0 ] || die "root is required (mount(2), block devices, fuse.enable_uring)"
[ -c /dev/fuse ] || die "/dev/fuse is absent"
[ -e /sys/module/fuse/parameters/enable_uring ] || die "this kernel ($(uname -r)) has no FUSE_IO_URING; a squeezefs mount cannot arm (charter §1.2)"

# --- the dist binary, ours, at HEAD ---------------------------------------
BIN=$REPO/zig-out-perf/bin/squeezefs
head=$(git -C "$REPO" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)
ver=$([ -x "$BIN" ] && "$BIN" --version 2>/dev/null || true)
have=$(echo "$ver" | sed -n 's/.*(\([0-9a-f]*\)[^,]*,.*/\1/p')
if [ "$REBUILD" = 1 ] || [ "$have" != "$head" ] || ! echo "$ver" | grep -q "dist profile"; then
  echo "perf_mount: building the DIST profile at $head (ReleaseSafe + fat LTO; several minutes) …"
  # The container's /build is its own volume; natively, the venue's build root when there is one.
  croot=/build; [ "$MODE" = container ] || croot=${SQZ_BUILD_ROOT:-/build}
  if [ -d "$croot" ]; then export ZIG_GLOBAL_CACHE_DIR=$croot/zig-global-cache; cachearg=(--cache-dir "$croot/zig-cache-perf"); else cachearg=(); fi
  ( cd "$REPO" && zig build ${cachearg[@]+"${cachearg[@]}"} -Dprofile=dist --prefix zig-out-perf dist shim ) \
    || die "the dist build failed (in the container a compiler killed by signal KILL is memory: SQZ_PERF_MEM)"
  ver=$("$BIN" --version)
fi
echo "$ver" | grep -q "dist profile" || die "$BIN is not a dist build: $ver"

# --- targets ---------------------------------------------------------------
prep_volume_target() {   # a container volume → its raw partition 1
  local name=$1 d
  d=$(findmnt -no SOURCE "/mnt/vols/$name") && [ -b "$d" ] || die "target volume $name is not attached (recreate: --rm)"
  if [ ! -b "${d}1" ]; then
    if sfdisk -d "$d" 2>/dev/null | grep -q 'start= *32768'; then
      partx -a "$d" 2>/dev/null || partx -u "$d"
    else
      umount "/mnt/vols/$name"
      mkfs.ext4 -q -F "$d" 16M
      printf 'label: dos\nstart=32768, type=83\n' | sfdisk -q --wipe never "$d" >/dev/null 2>&1
      partx -a "$d" 2>/dev/null || partx -u "$d"
      mount "$d" "/mnt/vols/$name"
      dd if=/dev/zero of="${d}1" bs=1M count=8 oflag=direct status=none
    fi
    [ -b "${d}1" ] || die "partition ${d}1 did not appear for target $name"
  fi
  ln -sfn "${d}1" "$TGT/$name"
}
metas=() datas=()
for t in "${targets[@]}"; do
  role=${t%%:*}; rest=${t#*:}; name=${rest%%:*}; src=${rest#*:}
  if [ "$src" = vol ]; then prep_volume_target "$name"
  else
    [ -b "$src" ] || die "$src is not a block device"
    findmnt -rno TARGET -S "$src" >/dev/null 2>&1 && die "$src is mounted (at $(findmnt -rno TARGET -S "$src"))"
    ln -sfn "$src" "$TGT/$name"
  fi
  if [ "$role" = mdt ]; then metas+=("$TGT/$name"); else datas+=("$TGT/$name"); fi
done
META="sqmeta://$(IFS=,; echo "${metas[*]}")"
DATA="sqdata://$(IFS=,; echo "${datas[*]}")"
layout="${#metas[@]} MDT + ${#datas[@]} OST"

# --- format once (or on --fresh), mount ------------------------------------
mountpoint -q "$MNT" 2>/dev/null && [ "$FRESH" = 1 ] && "$BIN" umount "$MNT"
# By role and size: /dev/vdX names may shift across container restarts.
want="$layout $(for m in "${metas[@]}" "${datas[@]}"; do printf '%s=%s ' "$(basename "$m")" "$(blockdev --getsize64 "$m")"; done)"
if [ "$FRESH" = 1 ] || [ ! -f "$STATE/formatted" ] || [ "$(cat "$STATE/formatted")" != "$want" ]; then
  [ -f "$STATE/formatted" ] && [ "$FRESH" != 1 ] && die "the targets changed since the set was formatted ($(cat "$STATE/formatted") → $want); run with --fresh to reformat"
  force=(); [ "$FRESH" = 1 ] && force=(--force)
  [ "${#metas[@]}" = 1 ] && force+=(--single-writer)
  echo "perf_mount: formatting $layout …"
  "$BIN" format ${force[@]+"${force[@]}"} --disk-cache-paths "$CACHE" "$META" "$DATA" > "$STATE/format.log" 2>&1 \
    || { cat "$STATE/format.log" >&2; die "format failed"; }
  echo "$want" > "$STATE/formatted"
fi
mkdir -p "$MNT"
if ! mountpoint -q "$MNT"; then
  "$BIN" mount --daemon --supervise --log-file "$LOG" \
    -o default_permissions,allow_other,fsname=squeezefs-perf "$META" "$MNT" \
    || { tail -30 "$LOG" >&2 2>/dev/null; die "mount failed (log: $LOG)"; }
fi
DAEMON_PID=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["mount"]["pid"])' "$MNT/.stats" 2>/dev/null || true)

# --- the run record ---------------------------------------------------------
RUN=$STATE/runs/$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$RUN"
{
  echo "binary      $ver"
  echo "kernel      $(uname -r) $(uname -m)"
  echo "cpus        $(nproc)   memory $(free -g | awk 'NR==2{print $2}') GiB"
  echo "mode        $MODE   host: $HOSTINFO"
  echo "layout      $layout"
  for m in "${metas[@]}" "${datas[@]}"; do printf '  %-6s -> %s  %s B\n' "$(basename "$m")" "$(readlink "$m")" "$(blockdev --getsize64 "$m" 2>/dev/null)"; done
  echo "cache       $CACHE"
  echo "mount       $MNT  -o default_permissions,allow_other  (daemon pid ${DAEMON_PID:-?})"
  echo "env         $(env | grep '^SQUEEZEFS_' | tr '\n' ' ')"
  echo "load        $(cut -d' ' -f1-3 /proc/loadavg) at mount"
  echo "fuse        enable_uring=$(cat /sys/module/fuse/parameters/enable_uring)"
} > "$RUN/env.txt"
cp "$STATE/format.log" "$RUN/" 2>/dev/null || true

teardown() {
  rc=$?
  cp "$MNT/.stats" "$RUN/stats-at-exit.json" 2>/dev/null || true
  [ "$KEEP" = 1 ] && { echo "perf_mount: --keep — $MNT stays mounted; run record $RUN"; exit $rc; }
  cd /
  if mountpoint -q "$MNT"; then
    echo "perf_mount: unmounting $MNT (orderly) …"
    "$BIN" umount "$MNT" || echo "perf_mount: umount failed; see $LOG" >&2
  fi
  if [ -n "${DAEMON_PID:-}" ]; then
    for _ in $(seq 1 600); do [ -d "/proc/$DAEMON_PID" ] || break; sleep 0.1; done
  fi
  scrubarg=(); [ "$SCRUB" = 1 ] && scrubarg=(--scrub)
  echo -n "perf_mount: fsck --offline ${scrubarg[*]:-}: "
  if "$BIN" fsck --offline ${scrubarg[@]+"${scrubarg[@]}"} "$META" > "$RUN/fsck.log" 2>&1; then
    grep -E '^findings:' "$RUN/fsck.log" | tail -1 || echo "passed"
  else
    echo "FAILED — $(grep -E '^squeezefs fsck:|^findings:' "$RUN/fsck.log" | tail -1); log: $RUN/fsck.log"
  fi
  echo "perf_mount: run record $RUN"
  exit $rc
}
trap teardown EXIT

# drop_caches: the kernel's page cache and dentries, after flushing the mount.
mkdir -p "$STATE/bin"
# The container runtime mounts /proc/sys read-only; the container is the whole
# VM, so the helper remounts it rw for the one write and restores it.
cat > "$STATE/bin/sqz-drop-caches" <<DROP
#!/bin/sh
set -e
sync -f "$MNT"
ro=0; grep -q " /proc/sys proc ro" /proc/mounts && { mount -o remount,rw /proc/sys; ro=1; }
echo 3 > /proc/sys/vm/drop_caches
[ \$ro = 1 ] && mount -o remount,ro /proc/sys
echo "dropped the page/dentry caches (after sync -f $MNT)"
DROP
chmod +x "$STATE/bin/sqz-drop-caches"
export PATH="$STATE/bin:$PATH"

cat <<BANNER

  squeezefs PERF mount — $ver
  $(sed -n '2,3p;5p' "$RUN/env.txt" | sed '2,$s/^/  /')
  mount      $MNT   ($(df -h "$MNT" | awk 'NR==2{print $2" total, "$4" free"}'))
  targets    $(for m in "${metas[@]}" "${datas[@]}"; do printf '%s=%s ' "$(basename "$m")" "$(readlink "$m")"; done)
  tools      $(fio --version 2>/dev/null || echo "fio absent"), ior $(command -v ior >/dev/null && echo present || echo absent), mdtest $(command -v mdtest >/dev/null && echo present || echo absent), mpirun $(command -v mpirun >/dev/null && echo present || echo absent)
  record     $RUN   (env.txt now; stats-at-exit.json + fsck.log on exit)
  between runs: sqz-drop-caches   (sync -f the mount, then drop the page/dentry caches)
  try        fio --name=r --directory=$MNT --rw=randread --bs=4k --size=4g --direct=1 --ioengine=io_uring --iodepth=32 --numjobs=4 --time_based --runtime=60 --group_reporting
             mpirun -np $(nproc) ior -a POSIX -t 1m -b 1g -F -e -o $MNT/ior.dat
             mpirun -np $(nproc) mdtest -n 10000 -u -d $MNT/md
             the bracket harness: scripts/bracket/bracket.sh (docs/gates/brackets.md)
  library    LD_PRELOAD=$REPO/zig-out-perf/lib/libsqueezefs_il.so  (the interception layer of this build)

BANNER
cd "$MNT"
if [ $# -gt 0 ]; then bash -c "$*"; else bash -i; fi
EOF

# ---------------------------------------------------------------------------
# Native: Linux with real devices.
# ---------------------------------------------------------------------------
if [ "$(uname -s)" = Linux ]; then
  [ "$remove" = 1 ] && die "--rm is a macOS container verb; natively, unmount and reuse or reformat the devices"
  [ "${#mdt_devs[@]}" -ge 1 ] && [ "${#ost_devs[@]}" -ge 1 ] \
    || die "natively, name the devices: --mdt /dev/X [--mdt /dev/Y] --ost /dev/Z [--ost …] (benchmarking on image files would measure the host filesystem)"
  [ "${#mdt_devs[@]}" -le 2 ] || die "at most 2 --mdt"
  targets=(); i=0
  for d in "${mdt_devs[@]}"; do targets+=("mdt:mdt$i:$d"); i=$((i + 1)); done
  i=0
  for d in "${ost_devs[@]}"; do targets+=("ost:ost$i:$d"); i=$((i + 1)); done
  hostinfo="native $(uname -n) $(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2 | sed 's/^ //')"
  [ "$(id -u)" = 0 ] || exec sudo -E "$0" ${ORIG_ARGS[@]+"${ORIG_ARGS[@]}"}
  exec bash -c "$INNER" perf_mount "$REPO" "$fresh" "$rebuild" "$keep" "$scrub" native /mnt/sqz-perf "$cache_dir" "$hostinfo" "${targets[@]}" -- ${cmd[@]+"${cmd[@]}"}
fi

# ---------------------------------------------------------------------------
# macOS: the bench container.
# ---------------------------------------------------------------------------
[ "$(uname -s)" = Darwin ] || die "unsupported host $(uname -s)"
command -v container >/dev/null || die "Apple's 'container' CLI is not installed"
container system status >/dev/null 2>&1 || die "container apiserver is not running: 'container system start'"
[ "${#mdt_devs[@]}" = 0 ] && [ "${#ost_devs[@]}" = 0 ] || die "--mdt/--ost name native devices (Linux); on macOS use --mdts/--osts/--mdt-size/--ost-size"
CPUS=${SQZ_PERF_CPUS:-$(sysctl -n hw.ncpu)}

tnames=(); for i in $(seq 0 $((mdts - 1))); do tnames+=("mdt$i:mdt:$mdt_size"); done
for i in $(seq 0 $((osts - 1))); do tnames+=("ost$i:ost:$ost_size"); done

exists() { container inspect "$NAME" >/dev/null 2>&1; }
running() { container inspect "$NAME" 2>/dev/null | python3 -c 'import json,sys; d=json.load(sys.stdin); d=d[0] if isinstance(d,list) else d; sys.exit(0 if d.get("status")=="running" else 1)' 2>/dev/null; }

if [ "$remove" = 1 ]; then
  if exists; then
    running && container exec "$NAME" bash -c 'mountpoint -q /mnt/sqz && "$0"/zig-out-perf/bin/squeezefs umount /mnt/sqz || true' "$REPO" || true
    container stop "$NAME" >/dev/null 2>&1 || true
    container rm "$NAME" >/dev/null 2>&1 || true
  fi
  container volume rm "$VOL" >/dev/null 2>&1 || true
  for v in $(container volume list 2>/dev/null | awk 'NR>1 {print $1}' | grep "^$VOL-" || true); do container volume rm "$v" >/dev/null 2>&1 || true; done
  echo "perf_mount: removed $NAME, volume $VOL and its target volumes (the image stays)"
  exit 0
fi

if ! container image inspect "$IMAGE" >/dev/null 2>&1; then
  container image inspect "$BASE_IMAGE" >/dev/null 2>&1 || die "the base image $BASE_IMAGE is absent"
  echo "perf_mount: building $IMAGE (fio, OpenMPI, IOR/mdtest) — once …"
  container build -t "$IMAGE" -f "$REPO/scripts/dev/Containerfile" "$REPO/scripts/dev" >/dev/null || die "building $IMAGE failed"
fi

if ! exists; then
  echo "perf_mount: creating $NAME ($CPUS cpus, $MEM; $mdts MDT × $mdt_size + $osts OST × $ost_size, sparse)"
  container volume create "$VOL" >/dev/null 2>&1 || true
  tvols=()
  for t in "${tnames[@]}"; do
    n=${t%%:*}; size=${t##*:}
    # 16 MiB in front of the target for the runtime's ext4 (see dev_mount.sh).
    bytes=$(python3 -c 'import sys; u={"K":1<<10,"M":1<<20,"G":1<<30,"T":1<<40}; s=sys.argv[1]; print(int(s[:-1])*u[s[-1].upper()] + (16<<20) if s[-1].upper() in u else int(s) + (16<<20))' "$size")
    container volume create -s "$bytes" "$VOL-$n" >/dev/null 2>&1 || true
    tvols+=(-v "$VOL-$n:/mnt/vols/$n")
  done
  container run -d --init --name "$NAME" -c "$CPUS" -m "$MEM" \
    --cap-add CAP_IPC_LOCK --cap-add CAP_SYS_ADMIN --ulimit memlock=-1:-1 \
    --mount "type=virtiofs,source=$REPO,target=$REPO" \
    -v "$VOL:/build" "${tvols[@]}" \
    "$IMAGE" sleep infinity >/dev/null
elif ! running; then
  container start "$NAME" >/dev/null
fi

# The attached targets are fixed at creation: a different shape needs --rm.
have_t=$(container exec "$NAME" bash -c 'ls /mnt/vols 2>/dev/null | tr "\n" " "')
want_t=$(for t in "${tnames[@]}"; do printf '%s ' "${t%%:*}"; done)
[ "$have_t" = "$want_t" ] || die "$NAME has targets [$have_t] but this run asks for [$want_t]; 'scripts/perf_mount.sh --rm' first"

# Quiet check: other containers share the host's cores.
busy=$(container list 2>/dev/null | awk 'NR>1 && $1 != "'"$NAME"'" && /running/ {print $1}' | tr '\n' ' ')
hostload=$(sysctl -n vm.loadavg | tr -d '{}' | awk '{print $1, $2, $3}')
[ -n "$busy" ] && echo "perf_mount: NOTE — other containers are running ($busy) and share the host's cores; host load $hostload. Quiet the host for numbers you will compare." >&2
hostinfo="macOS $(sw_vers -productVersion) $(sysctl -n machdep.cpu.brand_string) $(sysctl -n hw.ncpu) cores; container ${CPUS} vCPU $MEM; host load $hostload; other containers: ${busy:-none}"

targets=(); for t in "${tnames[@]}"; do n=${t%%:*}; r=${t#*:}; targets+=("${r%%:*}:$n:vol"); done
tty_flags=(-i); [ -t 0 ] && tty_flags=(-it)
exec container exec "${tty_flags[@]}" "$NAME" bash -c "$INNER" perf_mount "$REPO" "$fresh" "$rebuild" "$keep" "$scrub" container /mnt/sqz /build/perf/cache "$hostinfo" "${targets[@]}" -- ${cmd[@]+"${cmd[@]}"}
