#!/usr/bin/env fish
# zero_len_readfixed_matrix — run the zero-length READ_FIXED probe across
# filesystems on loopback images, one filesystem per invocation.
#
# WHY one at a time: on an affected filesystem the probe OOPSES the kernel
# (taint G D; only the probe task dies, the box survives). Sweeping every
# filesystem in one boot stacks oopses and muddies attribution — run one,
# read dmesg, decide, reboot if you want a clean taint state.
#
# BUILD AS YOUR USER, RUN AS ROOT. `sudo` carries binaries through
# `env "PATH=$PATH"` but NOT NIX_CFLAGS_COMPILE, so a compile under sudo
# cannot find liburing.h — hence the two steps (the root run reuses the
# binary the first step left in $STATE):
#
#   fish zero_len_readfixed_matrix.fish --build              # in nix-shell
#   sudo env "PATH=$PATH" fish zero_len_readfixed_matrix.fish btrfs
#   sudo env "PATH=$PATH" fish zero_len_readfixed_matrix.fish xfs   # negative control
#   fish zero_len_readfixed_matrix.fish --list               # no root needed
#
# The repo's shell.nix carries gcc, liburing and every mkfs tool the sweep
# uses, so `nix-shell` (or direnv) + the two lines above is the whole
# story. PROBE_BIN=<path> overrides the binary if you built it elsewhere.

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

set -g PROBE "$STATE/zero_len_readfixed_oops"
set -q PROBE_BIN; and set -g PROBE $PROBE_BIN

# Build the probe: unprivileged, inside nix-shell, where the compiler
# wrapper still has its NIX_CFLAGS_COMPILE. Reused by the root run.
function build_probe
    set -l cc
    for candidate in $CC gcc cc clang
        if command -q $candidate
            set cc $candidate
            break
        end
    end
    test -n "$cc"; or die "no C compiler on PATH — run inside nix-shell (shell.nix carries gcc)"
    set -l dir (path dirname $PROBE)
    mkdir -p $dir 2>/dev/null
    # A previous ROOT run leaves $STATE root-owned, which then refuses the
    # unprivileged --build this workflow depends on. Name the remedy.
    set -l who (whoami)
    test -w $dir
    or die "state dir $dir is not writable by $who — a previous root run owns it:
    sudo rm -rf $dir      # then re-run --build as your user
  or set STATE=<path> to build somewhere else"
    log "building the probe with $cc"
    $cc -O2 -Wall -o $PROBE $PROBE_SRC -luring
    or die "build failed — run this step UNPRIVILEGED inside nix-shell (a compile under sudo loses NIX_CFLAGS_COMPILE and cannot find liburing.h)"
    log "built $PROBE"
end

if test "$argv[1]" = --list
    echo $FS_LIST
    exit 0
end

if test "$argv[1]" = --build
    test (id -u) -ne 0
    or log "note: building as root — prefer your own user so the binary is not root-owned"
    build_probe
    exit 0
end

set -g FS $argv[1]
test -n "$FS"; or die "usage: "(status basename)" <"(string join '|' $FS_LIST)"> | --list"
contains -- $FS $FS_LIST; or die "unknown filesystem '$FS' (known: $FS_LIST)"
test (id -u) -eq 0; or die "must run as root (loop mount)"
command -q mkfs.$FS; or die "mkfs.$FS not found — install its tools (see the header)"

# Reuse the binary the --build step left behind; rebuild only if it is
# missing or older than its source (and only if a compiler is reachable —
# under sudo it usually is not, which is exactly why --build exists).
if test -x $PROBE; and test $PROBE -nt $PROBE_SRC
    log "using the prebuilt probe $PROBE"
else if command -q gcc; or command -q cc
    build_probe
else
    die "no probe binary at $PROBE and no compiler on PATH — build it first:
    fish "(status basename)" --build     # unprivileged, inside nix-shell
  then re-run this command as root"
end

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

# The WHOLE splat, not a grep of interesting-looking lines: the frames
# that matter most for an upstream report (btrfs_direct_read, __io_read,
# io_read_fixed) match no obvious keyword, and a filtered trace is not
# reportable. Saved verbatim beside the image for pasting into the bug.
set -g SPLAT_OUT "$STATE/splat-$FS.txt"
set -l splat (dmesg | sed -n "/$MARK/,\$p" | sed -n '/BUG: kernel NULL/,/end trace\|^$/p' | head -60)
if test -z "$splat"
    # No NULL-deref block: fall back to everything since the marker, so a
    # different failure shape is still visible rather than silently empty.
    set splat (dmesg | sed -n "/$MARK/,\$p" | tail -40)
end
if test -n "$splat"
    printf '%s\n' $splat | tee $SPLAT_OUT
    echo
    log "full splat saved to $SPLAT_OUT (paste this into the report)"
else
    echo "  (none — no oops recorded for this run)"
end

echo
echo "taint word: "(cat /proc/sys/kernel/tainted)"  (non-zero after an oops)"
