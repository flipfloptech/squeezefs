#!/usr/bin/env bash
# dev_mount.sh — an interactive shell inside a small Linux container with a
# formatted, MOUNTED SqueezeFS at /mnt/sqz. For poking at the filesystem by
# hand; nothing here is a product path or a gate.
#
#   scripts/dev_mount.sh                 # enter (first use: create the container, format, mount)
#   scripts/dev_mount.sh --fresh         # reformat the set first (its contents are dropped)
#   scripts/dev_mount.sh --build         # (re)build the Linux binary inside the container first
#   scripts/dev_mount.sh --keep          # leave the container running (and the set mounted) on exit
#   scripts/dev_mount.sh --rm            # stop and delete the container and its volume
#   scripts/dev_mount.sh -- <command…>   # run one command against the mount instead of a shell
#
# The container matches the host's architecture: Apple's `container` CLI on
# macOS (arm64 on Apple Silicon — the same image and kernel as the agents'
# venue, but a SEPARATE container `sqz-dev` with its own volume, so a venue
# restart never kills your shell and your experiments never trip a gate);
# docker or podman on Linux with `--platform linux/<host arch>`. Either way
# the kernel must carry FUSE-over-io_uring (`fuse.enable_uring` present) —
# the mount has no classical fallback by law (charter §1.2); the preflight
# refuses loud otherwise.
#
# The set is small and shaped like a real one: TWO metadata members (the MDTs,
# 256 MiB each — a 2-member set on the symmetric plane, each member hosting
# half the slot keyspace) and TWO data volumes (the OSTs, 1 GiB each), plus a
# staging/cache directory. On macOS every target is a RAW virtio-blk partition:
# each is its own container volume (a virtio-blk disk in the VM) laid out as a
# 16 MiB ext4 at the front — the runtime refuses to start a container whose
# volume does not mount as ext4 — and an MBR partition 1 from 16 MiB to the end
# (the MBR lives in the 1 KiB boot block ext4 never touches), so SqueezeFS
# writes /dev/vdX1 directly with no guest filesystem under it. Stable paths
# /build/dev/targets/{mdt0,mdt1,ost0,ost1} are re-pointed at every start. On a
# Linux host (docker/podman volumes are directories) the targets are sparse
# image files. The set is mounted with the release posture
# (`default_permissions,allow_other`) under `--daemon --supervise` with its log
# beside the set. On exit the mount is unmounted ORDERLY (`squeezefs umount`:
# staged files promoted, the final checkpoint lands), the script waits for the
# daemon's exit, and `fsck --offline --scrub` prints its verdict; the set
# persists on the volume for the next visit.
#
# The image is `squeezefs-dev:ubuntu2404` (scripts/dev/Containerfile): the build
# image plus fio, OpenMPI and IOR/mdtest at io500's pinned hash, built on first
# use. A container created before the tools existed keeps its old image: --rm
# and enter again to get them.
set -euo pipefail

NAME=${SQZ_DEV_NAME:-sqz-dev}
IMAGE=${SQZ_DEV_IMAGE:-squeezefs-dev:ubuntu2404}
BASE_IMAGE=squeezefs-build:ubuntu2404
VOL=${SQZ_DEV_VOLUME:-squeezefs-zig-dev}
CPUS=${SQZ_DEV_CPUS:-4}
# 10 GiB: the ReleaseSafe + thin-LTO build of the daemon is OOM-killed in 6.
MEM=${SQZ_DEV_MEM:-10G}
# The raw targets (macOS): name and volume size = 16 MiB ext4 + the target.
TARGETS="mdt0:272M mdt1:272M ost0:1040M ost1:1040M"
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

fresh=0 build=0 keep=0 remove=0
cmd=()
while [ $# -gt 0 ]; do
  case "$1" in
    --fresh) fresh=1 ;;
    --build) build=1 ;;
    --keep) keep=1 ;;
    --rm) remove=1 ;;
    --) shift; cmd=("$@"); break ;;
    -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "dev_mount: unknown flag '$1' (see --help)" >&2; exit 2 ;;
  esac
  shift
done

die() { echo "dev_mount: $*" >&2; exit 1; }

# --- the runtime that matches the host -----------------------------------
host_arch=$(uname -m)
case "$(uname -s)" in
  Darwin)
    command -v container >/dev/null || die "Apple's 'container' CLI is not installed (https://github.com/apple/container)"
    container system status >/dev/null 2>&1 || die "container apiserver is not running: 'container system start'"
    RT=container
    ;;
  Linux)
    if command -v docker >/dev/null; then RT=docker; elif command -v podman >/dev/null; then RT=podman; else die "docker or podman is required on Linux"; fi
    case "$host_arch" in x86_64) platform=linux/amd64 ;; aarch64|arm64) platform=linux/arm64 ;; *) platform="linux/$host_arch" ;; esac
    ;;
  *) die "unsupported host $(uname -s)" ;;
esac

exists() { "$RT" inspect "$NAME" >/dev/null 2>&1; }
running() {
  case "$RT" in
    container) container inspect "$NAME" 2>/dev/null | python3 -c 'import json,sys; d=json.load(sys.stdin); d=d[0] if isinstance(d,list) else d; sys.exit(0 if d.get("status")=="running" else 1)' 2>/dev/null ;;
    *) [ "$("$RT" inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null)" = true ] ;;
  esac
}

if [ "$remove" = 1 ]; then
  if exists; then
    running && "$RT" exec "$NAME" bash -lc 'b=$(ls "$0"/zig-out-dev/bin/squeezefs "$0"/zig-out-linux/bin/squeezefs 2>/dev/null | head -1); [ -n "$b" ] && mountpoint -q /mnt/sqz && "$b" umount /mnt/sqz || true' "$REPO" || true
    "$RT" stop "$NAME" >/dev/null 2>&1 || true
    "$RT" rm "$NAME" >/dev/null 2>&1 || true
  fi
  "$RT" volume rm "$VOL" >/dev/null 2>&1 || true
  for t in $TARGETS; do "$RT" volume rm "$VOL-${t%%:*}" >/dev/null 2>&1 || true; done
  echo "dev_mount: removed $NAME, volume $VOL and its target volumes (the image stays)"
  exit 0
fi

# --- the image: the build image + the benchmark instruments ----------------
if ! "$RT" image inspect "$IMAGE" >/dev/null 2>&1; then
  [ "$IMAGE" = squeezefs-dev:ubuntu2404 ] || die "image $IMAGE is absent"
  "$RT" image inspect "$BASE_IMAGE" >/dev/null 2>&1 || die "the base image $BASE_IMAGE is absent (the venue image: Zig 0.17 + fuse3 on Ubuntu 24.04)"
  echo "dev_mount: building $IMAGE (fio, OpenMPI, IOR/mdtest) — once, about a minute …"
  case "$RT" in
    container) container build -t "$IMAGE" -f "$REPO/scripts/dev/Containerfile" "$REPO/scripts/dev" >/dev/null ;;
    *) "$RT" build --platform "$platform" -t "$IMAGE" -f "$REPO/scripts/dev/Containerfile" "$REPO/scripts/dev" >/dev/null ;;
  esac || die "building $IMAGE failed (rerun to see the build log: ${RT} build -f scripts/dev/Containerfile scripts/dev)"
fi

# --- create or start the container ----------------------------------------
if ! exists; then
  echo "dev_mount: creating $NAME ($RT, $host_arch, $CPUS cpus, $MEM) from $IMAGE"
  "$RT" volume create "$VOL" >/dev/null 2>&1 || true
  case "$RT" in
    container)
      # CAP_SYS_ADMIN: mount(2) and the fuse.enable_uring write. CAP_IPC_LOCK +
      # unlimited memlock: io_uring fixed buffers pin pages (REGISTER_BUFFERS).
      # --init: PID 1 reaps the exited supervisors (sleep infinity never does).
      # Each target is its own volume = its own virtio-blk disk in the VM.
      tvols=()
      for t in $TARGETS; do
        container volume create -s "${t#*:}" "$VOL-${t%%:*}" >/dev/null 2>&1 || true
        tvols+=(-v "$VOL-${t%%:*}:/mnt/vols/${t%%:*}")
      done
      container run -d --init --name "$NAME" -c "$CPUS" -m "$MEM" \
        --cap-add CAP_IPC_LOCK --cap-add CAP_SYS_ADMIN --ulimit memlock=-1:-1 \
        --mount "type=virtiofs,source=$REPO,target=$REPO" \
        -v "$VOL:/build" "${tvols[@]}" \
        "$IMAGE" sleep infinity >/dev/null
      ;;
    *)
      "$RT" run -d --init --name "$NAME" --platform "$platform" --cpus "$CPUS" -m "$MEM" \
        --device /dev/fuse --cap-add SYS_ADMIN --cap-add IPC_LOCK --ulimit memlock=-1:-1 \
        --security-opt apparmor=unconfined \
        -v "$REPO:$REPO" -v "$VOL:/build" \
        "$IMAGE" sleep infinity >/dev/null
      ;;
  esac
elif ! running; then
  "$RT" start "$NAME" >/dev/null
fi

# --- inside: build if asked, format once, mount, shell, orderly teardown ---
read -r -d '' INNER <<'EOF' || true
set -euo pipefail
REPO=$1; FRESH=$2; BUILD=$3; KEEP=$4; WANT_RAW=$5; shift 5
DEV=/build/dev; SET=$DEV/set; TGT=$DEV/targets; MNT=/mnt/sqz; LOG=$DEV/daemon.log
# Two MDTs (metadata members) and two OSTs (data volumes), by stable path.
META="sqmeta://$TGT/mdt0,$TGT/mdt1"
DATA="sqdata://$TGT/ost0,$TGT/ost1"
die() { echo "dev_mount: $*" >&2; exit 1; }
command -v fio >/dev/null && command -v ior >/dev/null \
  || echo "dev_mount: fio/ior/mdtest are absent — this container predates the tools image; 'scripts/dev_mount.sh --rm' and enter again" >&2

[ -c /dev/fuse ] || die "/dev/fuse is absent in this container"
[ -e /sys/module/fuse/parameters/enable_uring ] || die "this kernel ($(uname -r)) has no FUSE_IO_URING: fuse.enable_uring is absent and a squeezefs mount cannot arm (charter §1.2)"

# The binary is OURS: built into zig-out-dev/ from the checked-out tree, and
# rebuilt whenever its embedded commit is not HEAD's (a binary from another
# prefix can be weeks stale — the venue's zig-out-linux/ once formatted this
# set as format v4 with a long-fixed fsck). The first build takes minutes;
# later ones are incremental.
mkdir -p /build/zig-cache /build/zig-global-cache "$SET/cache"
export ZIG_GLOBAL_CACHE_DIR=/build/zig-global-cache
BIN="$REPO/zig-out-dev/bin/squeezefs"
head=$(git -C "$REPO" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)
have=$([ -x "$BIN" ] && "$BIN" --version 2>/dev/null | sed -n 's/.*(\([0-9a-f]*\)[^,]*,.*/\1/p' || true)
if [ "$BUILD" = 1 ] || [ ! -x "$BIN" ] || [ "$have" != "$head" ]; then
  echo "dev_mount: building squeezefs at $head (ReleaseSafe) into zig-out-dev/ … (binary was: ${have:-none})"
  ( cd "$REPO" && zig build --cache-dir /build/zig-cache --prefix zig-out-dev ) \
    || die "build failed (a compiler 'terminated with signal KILL' is the container's memory: recreate with SQZ_DEV_MEM=12G after --rm)"
fi
[ -x "$BIN" ] || die "no squeezefs binary at $BIN"

if [ "$FRESH" = 1 ]; then
  mountpoint -q "$MNT" && "$BIN" umount "$MNT"
  rm -rf "$SET" "$DEV/formatted"; mkdir -p "$SET/cache"
fi
# A set from before the 2 MDT + 2 OST layout is never reformatted silently.
[ -f "$SET/meta.img" ] && [ ! -f "$DEV/formatted" ] \
  && die "this set has the old layout (1 metadata volume); run with --fresh to reformat it as 2 MDTs + 2 OSTs (its contents are dropped)"
[ "$WANT_RAW" = 1 ] && [ ! -d /mnt/vols ] \
  && die "this container predates the raw-disk targets; 'scripts/dev_mount.sh --rm' and enter again (the set is reformatted)"

# One target: a raw partition of its own virtio-blk disk, else an image file.
prep_target() {
  local name=$1 size=$2 d
  if [ -d "/mnt/vols/$name" ] && d=$(findmnt -no SOURCE "/mnt/vols/$name") && [ -b "$d" ]; then
    if [ ! -b "${d}1" ]; then
      if sfdisk -d "$d" 2>/dev/null | grep -q 'start= *32768'; then
        partx -a "$d" 2>/dev/null || partx -u "$d"            # the table exists: just show it
      else
        # A new volume: the runtime's whole-disk ext4 becomes a 16 MiB ext4 the
        # runtime can still mount, then partition 1 from 16 MiB (32768 sectors).
        umount "/mnt/vols/$name"
        mkfs.ext4 -q -F "$d" 16M
        printf 'label: dos\nstart=32768, type=83\n' | sfdisk -q --wipe never "$d" >/dev/null 2>&1
        partx -a "$d" 2>/dev/null || partx -u "$d"
        mount "$d" "/mnt/vols/$name"
        # The old whole-disk ext4's metadata is inside the partition: clear the
        # head so format meets no stale signature.
        dd if=/dev/zero of="${d}1" bs=1M count=8 oflag=direct status=none
      fi
      [ -b "${d}1" ] || die "partition ${d}1 did not appear for target $name"
    fi
    ln -sfn "${d}1" "$TGT/$name"
  else
    [ -f "$SET/$name.img" ] || truncate -s "$size" "$SET/$name.img"   # sparse
    ln -sfn "$SET/$name.img" "$TGT/$name"
  fi
}
mkdir -p "$TGT"
prep_target mdt0 256M; prep_target mdt1 256M; prep_target ost0 1G; prep_target ost1 1G

if [ ! -f "$DEV/formatted" ]; then
  echo "dev_mount: formatting 2 MDTs + 2 OSTs ($(readlink "$TGT/mdt0" | sed 's|/dev/.*|raw virtio-blk partitions|; s|^/build.*|sparse image files|')) …"
  force=(); [ "$FRESH" = 1 ] && force=(-f)
  "$BIN" format ${force[@]+"${force[@]}"} --disk-cache-paths "$SET/cache" "$META" "$DATA" > "$DEV/format.log" 2>&1 \
    || { cat "$DEV/format.log" >&2; die "format failed"; }
  touch "$DEV/formatted"
fi

mkdir -p "$MNT"
if ! mountpoint -q "$MNT"; then
  "$BIN" mount --daemon --supervise --log-file "$LOG" \
    -o default_permissions,allow_other,fsname=squeezefs-dev \
    "$META" "$MNT" \
    || { tail -30 "$LOG" >&2 2>/dev/null; die "mount failed (log: $LOG)"; }
fi
DAEMON_PID=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["mount"]["pid"])' "$MNT/.stats" 2>/dev/null || true)

teardown() {
  rc=$?
  [ "$KEEP" = 1 ] && { echo "dev_mount: --keep — $MNT stays mounted in $(hostname)"; exit $rc; }
  # Leave the mount before unmounting it: a cwd inside makes umount2 EBUSY.
  cd /
  if mountpoint -q "$MNT"; then
    # (`; true`: a plain assignment takes its substitution's status, and the
    # last grep's miss would otherwise end the script under set -e.)
    holders=$({ for p in /proc/[0-9]*; do ls -l "$p/cwd" "$p/fd" 2>/dev/null | grep -q " $MNT" && echo "${p#/proc/}"; done; true; } | tr '\n' ' ')
    [ -n "$holders" ] && echo "dev_mount: processes still inside $MNT (pids $holders) — the orderly unmount needs them gone" >&2
    echo "dev_mount: unmounting $MNT (orderly) …"
    "$BIN" umount "$MNT" || echo "dev_mount: umount failed; see $LOG" >&2
  fi
  # `umount` returns when the mount is gone; the daemon still runs its final
  # checkpoint and releases the writer guard, and fsck refuses while it holds
  # it. Wait for the exit (bounded: a daemon that outlives this is reported).
  if [ -n "$DAEMON_PID" ]; then
    for _ in $(seq 1 600); do [ -d "/proc/$DAEMON_PID" ] || break; sleep 0.1; done
    [ -d "/proc/$DAEMON_PID" ] && echo "dev_mount: daemon $DAEMON_PID still running 60 s after umount; see $LOG" >&2
  fi
  # The verdict is fsck's own last non-log line (findings count, or the loud
  # failure); the daemon's info/debug trail goes to a file beside the set.
  echo -n "dev_mount: fsck --offline --scrub: "
  if "$BIN" fsck --offline --scrub "$META" > "$DEV/fsck.log" 2>&1; then
    grep -E '^findings:' "$DEV/fsck.log" | tail -1 || echo "passed (no findings line; see $DEV/fsck.log)"
  else
    echo "FAILED (rc $?) — $(grep -E '^squeezefs fsck:|^findings:' "$DEV/fsck.log" | tail -1); log: $DEV/fsck.log"
  fi
  exit $rc
}
trap teardown EXIT

cat <<BANNER

  squeezefs dev mount — $(uname -r), $(nproc) cpus, $(free -g | awk 'NR==2{print $2}') GiB
  mount      $MNT   ($(df -h "$MNT" | awk 'NR==2{print $2" total, "$4" free"}'))
  MDTs       $(readlink "$TGT/mdt0"), $(readlink "$TGT/mdt1")   (2 metadata members, 256 MiB each)
  OSTs       $(readlink "$TGT/ost0"), $(readlink "$TGT/ost1")   (2 data volumes, 1 GiB each; staging in $SET/cache)
  binary     $BIN   ($("$BIN" --version))
  daemon log $LOG
  tools      $(fio --version 2>/dev/null || echo "fio: absent"), ior + mdtest $(command -v ior >/dev/null && echo "(io500's 5528b36)" || echo ": absent"), $(mpirun --version 2>/dev/null | head -1 | sed 's/^mpirun (//; s/).*//' || echo "mpirun: absent")
  try        fio --name=r --directory=$MNT --rw=randread --bs=4k --size=256m --direct=1 --ioengine=io_uring --iodepth=32
             mpirun -np 4 ior -a POSIX -t 1m -b 64m -F -o $MNT/ior.dat
             mpirun -np 4 mdtest -n 1000 -d $MNT/md
  handy      python3 -m json.tool $MNT/.stats | less     (one JSON document)
             squeezefs status · squeezefs df · squeezefs volume list $META
             squeezefs umount $MNT   (the shell's exit does this orderly, then fsck)
  --keep leaves it mounted; --fresh reformats; --build rebuilds the binary.

BANNER
cd "$MNT"
if [ $# -gt 0 ]; then bash -c "$*"; else bash -i; fi
EOF

# A TTY only when we have one (a scripted `-- <command>` run has none); the
# empty-array expansion below is bash-3.2-safe (macOS's /bin/bash) under set -u.
tty_flags=(-i); [ -t 0 ] && tty_flags=(-it)
want_raw=0; [ "$RT" = container ] && want_raw=1
exec "$RT" exec "${tty_flags[@]}" "$NAME" bash -c "$INNER" dev_mount "$REPO" "$fresh" "$build" "$keep" "$want_raw" ${cmd[@]+"${cmd[@]}"}
