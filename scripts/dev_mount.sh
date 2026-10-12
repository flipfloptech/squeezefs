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
# The set is small: a 256 MiB metadata image, two 1 GiB data images (sparse)
# and a staging/cache directory, formatted `--single-writer`, mounted with the
# release posture (`default_permissions,allow_other`) under `--daemon
# --supervise` with its log beside the set. On exit the mount is unmounted
# ORDERLY (`squeezefs umount`: staged files promoted, the final checkpoint
# lands) and `fsck --offline --scrub` prints its verdict; the set persists on
# the volume for the next visit.
set -euo pipefail

NAME=${SQZ_DEV_NAME:-sqz-dev}
IMAGE=${SQZ_DEV_IMAGE:-squeezefs-build:ubuntu2404}
VOL=${SQZ_DEV_VOLUME:-squeezefs-zig-dev}
CPUS=${SQZ_DEV_CPUS:-4}
# 10 GiB: the ReleaseSafe + thin-LTO build of the daemon is OOM-killed in 6.
MEM=${SQZ_DEV_MEM:-10G}
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
  echo "dev_mount: removed $NAME and volume $VOL"
  exit 0
fi

# --- create or start the container ----------------------------------------
if ! exists; then
  echo "dev_mount: creating $NAME ($RT, $host_arch, $CPUS cpus, $MEM) from $IMAGE"
  "$RT" volume create "$VOL" >/dev/null 2>&1 || true
  case "$RT" in
    container)
      # CAP_SYS_ADMIN: mount(2) and the fuse.enable_uring write. CAP_IPC_LOCK +
      # unlimited memlock: io_uring fixed buffers pin pages (REGISTER_BUFFERS).
      container run -d --name "$NAME" -c "$CPUS" -m "$MEM" \
        --cap-add CAP_IPC_LOCK --cap-add CAP_SYS_ADMIN --ulimit memlock=-1:-1 \
        --mount "type=virtiofs,source=$REPO,target=$REPO" \
        -v "$VOL:/build" \
        "$IMAGE" sleep infinity >/dev/null
      ;;
    *)
      "$RT" image inspect "$IMAGE" >/dev/null 2>&1 || die "image $IMAGE is absent; build it for $platform (the venue image: Zig 0.17 + fuse3 on Ubuntu 24.04) or set SQZ_DEV_IMAGE"
      "$RT" run -d --name "$NAME" --platform "$platform" --cpus "$CPUS" -m "$MEM" \
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
REPO=$1; FRESH=$2; BUILD=$3; KEEP=$4; shift 4
DEV=/build/dev; SET=$DEV/set; MNT=/mnt/sqz; LOG=$DEV/daemon.log
die() { echo "dev_mount: $*" >&2; exit 1; }

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
  rm -rf "$SET"; mkdir -p "$SET/cache"
fi
if [ ! -f "$SET/meta.img" ]; then
  # Small by design: sparse images cost nothing until written.
  truncate -s 256M "$SET/meta.img"
  truncate -s 1G "$SET/d0.img" "$SET/d1.img"
  echo "dev_mount: formatting the set under $SET …"
  "$BIN" format --single-writer --disk-cache-paths "$SET/cache" \
    "sqmeta://$SET/meta.img" "sqdata://$SET/d0.img,$SET/d1.img" > "$DEV/format.log" 2>&1 \
    || { cat "$DEV/format.log" >&2; die "format failed"; }
fi

mkdir -p "$MNT"
if ! mountpoint -q "$MNT"; then
  "$BIN" mount --daemon --supervise --log-file "$LOG" \
    -o default_permissions,allow_other,fsname=squeezefs-dev \
    "sqmeta://$SET/meta.img" "$MNT" \
    || { tail -30 "$LOG" >&2 2>/dev/null; die "mount failed (log: $LOG)"; }
fi

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
  # The verdict is fsck's own last non-log line (findings count, or the loud
  # failure); the daemon's info/debug trail goes to a file beside the set.
  echo -n "dev_mount: fsck --offline --scrub: "
  if "$BIN" fsck --offline --scrub "sqmeta://$SET/meta.img" > "$DEV/fsck.log" 2>&1; then
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
  set        $SET   (meta 256 MiB, data 2 × 1 GiB sparse, cache/)
  binary     $BIN   ($("$BIN" --version))
  daemon log $LOG
  handy      python3 -m json.tool $MNT/.stats | less     (one JSON document)
             squeezefs status · squeezefs df · tail -f $LOG
             squeezefs umount $MNT   (the shell's exit does this orderly, then fsck)
  --keep leaves it mounted; --fresh reformats; --build rebuilds the binary.

BANNER
cd "$MNT"
if [ $# -gt 0 ]; then bash -c "$*"; else bash -i; fi
EOF

# A TTY only when we have one (a scripted `-- <command>` run has none); the
# empty-array expansion below is bash-3.2-safe (macOS's /bin/bash) under set -u.
tty_flags=(-i); [ -t 0 ] && tty_flags=(-it)
exec "$RT" exec "${tty_flags[@]}" "$NAME" bash -c "$INNER" dev_mount "$REPO" "$fresh" "$build" "$keep" ${cmd[@]+"${cmd[@]}"}
