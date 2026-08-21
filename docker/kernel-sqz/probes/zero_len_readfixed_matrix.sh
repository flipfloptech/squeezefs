#!/bin/bash
# zero_len_readfixed_matrix — run the zero-length READ_FIXED probe across
# filesystems on loopback images, one filesystem per invocation.
#
# WHY one at a time: on an affected filesystem the probe OOPSES the kernel
# (taint G D; only the probe task dies, the box survives). Sweeping every
# filesystem in one boot stacks oopses and muddies attribution — run one,
# read dmesg, decide, reboot if you want a clean taint state.
#
#   sudo bash zero_len_readfixed_matrix.sh btrfs
#   sudo bash zero_len_readfixed_matrix.sh ext4
#   sudo bash zero_len_readfixed_matrix.sh xfs        # expected negative control
#   sudo bash zero_len_readfixed_matrix.sh --list
#
# Requires root (loop mount) and the mkfs tool for the target filesystem.
# On NixOS:
#   sudo nix-shell -p liburing gcc e2fsprogs xfsprogs btrfs-progs \
#        f2fs-tools exfatprogs util-linux --run 'bash zero_len_readfixed_matrix.sh ext4'
set -euo pipefail

FS_LIST="btrfs ext4 ext2 xfs f2fs exfat"
IMG_MB=512
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROBE_SRC="$HERE/zero_len_readfixed_oops.c"
STATE="${STATE:-/var/tmp/zerolen-probe}"

die() {
	echo "FATAL: $*" >&2
	exit 1
}
log() { echo -e "\e[1m[zerolen] $*\e[0m"; }

[ "${1:-}" = "--list" ] && {
	echo "$FS_LIST"
	exit 0
}
FS="${1:-}"
[ -n "$FS" ] || die "usage: $0 <${FS_LIST// /|}> | --list"
[ "$(id -u)" = 0 ] || die "must run as root (loop mount)"
command -v "mkfs.$FS" >/dev/null || die "mkfs.$FS not found — install its tools (see the header)"

# The probe binary: built beside the source, or in $STATE if the tree is
# read-only. gcc + liburing headers must be present.
PROBE="$STATE/zero_len_readfixed_oops"
mkdir -p "$STATE"
log "building the probe"
gcc -O2 -Wall -o "$PROBE" "$PROBE_SRC" -luring || die "build failed (need liburing dev headers)"

IMG="$STATE/$FS.img"
MNT="$STATE/mnt-$FS"
cleanup() {
	umount "$MNT" 2>/dev/null || true
	rmdir "$MNT" 2>/dev/null || true
	rm -f "$IMG"
}
trap cleanup EXIT

log "creating a ${IMG_MB} MiB $FS image"
rm -f "$IMG"
truncate -s "${IMG_MB}M" "$IMG"
case "$FS" in
btrfs) mkfs.btrfs -q -f "$IMG" >/dev/null ;;
ext4 | ext2) "mkfs.$FS" -q -F "$IMG" >/dev/null ;;
xfs) mkfs.xfs -q -f "$IMG" >/dev/null ;;
f2fs) mkfs.f2fs -q -f "$IMG" >/dev/null ;;
exfat) mkfs.exfat "$IMG" >/dev/null ;;
*) die "unknown filesystem '$FS'" ;;
esac

mkdir -p "$MNT"
mount -o loop "$IMG" "$MNT" || die "mount failed"
log "mounted $FS at $MNT"

# Mark dmesg so the operator can attribute any oops to THIS run.
MARK="zerolen-probe-$FS-$(date +%s)"
echo "$MARK" >/dev/kmsg 2>/dev/null || true

log "running the probe on $FS (an affected pair oopses HERE)"
set +e
"$PROBE" "$MNT/probe.bin"
RC=$?
set -e

echo
if [ "$RC" -eq 0 ]; then
	log "RESULT: $FS SURVIVED (not affected on this kernel)"
else
	log "RESULT: $FS probe exited rc=$RC (137/killed = the oops path)"
fi
echo "kernel messages since the marker:"
dmesg | sed -n "/$MARK/,\$p" | grep -iE "BUG:|RIP:|Call Trace|iov_iter|io_uring|Oops|Tainted" | head -20 ||
	echo "  (none — no oops recorded for this run)"
echo
echo "taint word: $(cat /proc/sys/kernel/tainted)  (non-zero after an oops)"
