#!/usr/bin/env fish
# zero_len_readfixed_matrix — run the zero-length READ_FIXED probe across
# filesystems on loopback images, one filesystem per invocation.
#
# WHY one at a time: on an affected filesystem the probe OOPSES the kernel
# (taint G D; only the probe task dies, the box survives). Sweeping every
# filesystem in one boot stacks oopses and muddies attribution — run one,
# read dmesg, decide, reboot if you want a clean taint state.
#
#   sudo fish zero_len_readfixed_matrix.fish btrfs
#   sudo fish zero_len_readfixed_matrix.fish ext4
#   sudo fish zero_len_readfixed_matrix.fish xfs        # expected negative control
#   fish zero_len_readfixed_matrix.fish --list          # no root needed
#
# Requires root (loop mount) and the mkfs tool for the target filesystem.
# On NixOS, pull the tools into the root shell for the run:
#   sudo nix-shell -p liburing gcc e2fsprogs xfsprogs btrfs-progs \
#        f2fs-tools exfatprogs util-linux fish \
#        --run 'fish docker/kernel-sqz/probes/zero_len_readfixed_matrix.fish ext4'

set -g FS_LIST btrfs ext4 ext2 xfs f2fs exfat
set -g IMG_MB 512
set -g HERE (path dirname (status filename))
set -g PROBE_SRC "$HERE/zero_len_readfixed_oops.c"
set -q STATE; or set -g STATE /var/tmp/zerolen-probe

function log
    set_color --bold
    echo "[zerolen] $argv"
    set_color normal
end

function die
    echo "FATAL: $argv" >&2
    exit 1
end

# Fish has no `trap`; the exit handler is an event function. It runs on
# normal exit AND on `exit` from die, which is what the bash trap covered.
function _zerolen_cleanup --on-event fish_exit
    set -q MNT; and umount $MNT 2>/dev/null
    set -q MNT; and rmdir $MNT 2>/dev/null
    set -q IMG; and rm -f $IMG
end

if test "$argv[1]" = --list
    echo $FS_LIST
    exit 0
end

set -g FS $argv[1]
test -n "$FS"; or die "usage: "(status basename)" <"(string join '|' $FS_LIST)"> | --list"
contains -- $FS $FS_LIST; or die "unknown filesystem '$FS' (known: $FS_LIST)"
test (id -u) -eq 0; or die "must run as root (loop mount)"
command -q mkfs.$FS; or die "mkfs.$FS not found — install its tools (see the header)"

# The probe binary: built into $STATE so a read-only tree still works.
# gcc + the liburing dev headers must be present.
set -g PROBE "$STATE/zero_len_readfixed_oops"
mkdir -p $STATE
log "building the probe"
gcc -O2 -Wall -o $PROBE $PROBE_SRC -luring
or die "build failed (need liburing dev headers)"

set -g IMG "$STATE/$FS.img"
set -g MNT "$STATE/mnt-$FS"

log "creating a $IMG_MB MiB $FS image"
rm -f $IMG
truncate -s {$IMG_MB}M $IMG
switch $FS
    case btrfs
        mkfs.btrfs -q -f $IMG >/dev/null
    case ext4 ext2
        mkfs.$FS -q -F $IMG >/dev/null
    case xfs
        mkfs.xfs -q -f $IMG >/dev/null
    case f2fs
        mkfs.f2fs -q -f $IMG >/dev/null
    case exfat
        mkfs.exfat $IMG >/dev/null
end
or die "mkfs.$FS failed"

mkdir -p $MNT
mount -o loop $IMG $MNT; or die "mount failed"
log "mounted $FS at $MNT"

# Mark dmesg so the operator can attribute any oops to THIS run.
set -g MARK "zerolen-probe-$FS-"(date +%s)
echo $MARK >/dev/kmsg 2>/dev/null

log "running the probe on $FS (an affected pair oopses HERE)"
$PROBE "$MNT/probe.bin"
set -l rc $status

echo
if test $rc -eq 0
    log "RESULT: $FS SURVIVED (not affected on this kernel)"
else
    log "RESULT: $FS probe exited rc=$rc (137/killed = the oops path)"
end

echo "kernel messages since the marker:"
set -l splat (dmesg | sed -n "/$MARK/,\$p" | grep -iE 'BUG:|RIP:|Call Trace|iov_iter|io_uring|Oops|Tainted' | head -20)
if test -n "$splat"
    printf '%s\n' $splat
else
    echo "  (none — no oops recorded for this run)"
end

echo
echo "taint word: "(cat /proc/sys/kernel/tainted)"  (non-zero after an oops)"
