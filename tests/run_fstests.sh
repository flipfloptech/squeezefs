#!/usr/bin/env bash
set -euo pipefail

# Squeezefs integration script for fstests (xfstests).
# MUST be run as root.
#
# Harness contract this script has to satisfy (xfstests common/rc + check):
#  - Tests unmount/remount TEST_DEV and SCRATCH_DEV between (and inside) tests
#    via `mount -t fuse.squeezefs $DEV $MNT` / `umount $DEV`. The mount helper
#    must be SILENT on success (its stdout leaks into golden output diffs) and
#    must not return until the mount is actually usable.
#  - `umount` returns before the squeezefs daemon finishes draining staged
#    writes. The next mount of the same device must wait for the previous
#    daemon to exit, or two daemons race on the same meta volume (fencing
#    discards -> phantom data loss) and a dead ENOTCONN mountpoint corrupts
#    $TEST_DIR inside common/config ("mount: bad usage" cascade).
#  - xfstests never re-mkfs a FUSE scratch fs (common/rc _scratch_mkfs just
#    rm -rf's it), so volumes must be big enough for a full `-g auto` run:
#    meta sized for the 20000-inode allocator cap (see
#    src/meta_backend/storage.rs: usable inodes = (size - 72 MiB) / 32 KiB).

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: This script must run as root (sudo $0)." >&2
    exit 1
fi

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUNUSER="${SUDO_USER:-root}"

TEST_DIR="${TEST_DIR:-/mnt/squeezefs_test}"
SCRATCH_MNT="${SCRATCH_MNT:-/mnt/squeezefs_scratch}"
TEST_DEV="${TEST_DEV:-/dev/shm/squeezefs_fstests_test_meta}"
SCRATCH_DEV="${SCRATCH_DEV:-/dev/shm/squeezefs_fstests_scratch_meta}"

# Sizes: meta >= 72 MiB + 20000 * 32 KiB (~697 MiB) reaches the inode
# allocator cap; below that "Inode table full" aborts long runs. Backing
# files are sparse on tmpfs, so the sizes below cost RAM only for bytes a
# test actually writes (the mkfs wrapper recreates them per re-format, so
# usage never accumulates across tests).
#
# Size-gate law (user directive 2026-08-10 — no size-based skips on a dev
# box): xfstests' _require_scratch_size reads the SCRATCH_DEV file — for
# us the META file — so META_SIZE is what clears the gate, while
# DATA_SIZE is what actually holds a test's bytes. The largest -g auto
# gate today is 16 GiB (generic/781, generic/793): META 17G clears it
# with margin, DATA 24G holds a 16 GiB write plus block rounding. With
# these defaults the residual [not run] population is capability gates
# only (block-device/zoned/reflink/dax) — never device size. Bigger
# needs ride the existing TEST_DEV/SCRATCH_DEV env overrides (flat files
# on disk).
META_SIZE="${SQUEEZEFS_FSTESTS_META_SIZE:-17G}"
DATA_SIZE="${SQUEEZEFS_FSTESTS_DATA_SIZE:-24G}"

echo "=== Squeezefs fstests Integration ==="
echo "Repo: $REPO_DIR"

# 0. Clean state from any previous runs: unmount, wait out old daemons,
#    wipe stale backing volumes / staging dirs / mountpoint underlay junk.
echo "Cleaning up state from previous runs..."
umount -l "$TEST_DIR" "$SCRATCH_MNT" &>/dev/null || true
pkill -f "squeezefs mount sqmeta://${TEST_DEV}" &>/dev/null || true
pkill -f "squeezefs mount sqmeta://${SCRATCH_DEV}" &>/dev/null || true
for _ in $(seq 1 100); do
    pgrep -f "squeezefs mount sqmeta://(${TEST_DEV}|${SCRATCH_DEV})" >/dev/null || break
    sleep 0.1
done
pkill -9 -f "squeezefs mount sqmeta://(${TEST_DEV}|${SCRATCH_DEV})" &>/dev/null || true
rm -f "$TEST_DEV" "${TEST_DEV/_meta/_data}" "$SCRATCH_DEV" "${SCRATCH_DEV/_meta/_data}"
rm -rf /tmp/squeezefs_fstests_staging_* /tmp/squeezefs_fstests_*.log
# Leaked files on the underlying mountpoint dirs make the daemon refuse to
# mount ("mount point is not empty"); recreate them empty.
rm -rf "$TEST_DIR" "$SCRATCH_MNT"
mkdir -p "$TEST_DIR" "$SCRATCH_MNT"

# 1. Install prerequisites
echo "Installing prerequisites..."
if command -v apt-get &>/dev/null; then
    apt-get update && apt-get install -y \
        xfsprogs libtool acl attr libacl1-dev libattr1-dev libaio-dev \
        bc dbench fio gawk gcc git indent libcap-dev libgdbm-dev make psmisc quota
elif command -v pacman &>/dev/null; then
    echo "Arch Linux detected, assuming prerequisites are installed."
else
    echo "Warning: package manager not supported, assuming prerequisites are installed."
fi

# 2. Build Squeezefs release binary
cd "$REPO_DIR"
if [ "$RUNUSER" != "root" ] && id "$RUNUSER" &>/dev/null; then
    su -s "$BASH" "$RUNUSER" -c "export PATH='$PATH'; cd '$REPO_DIR' && cargo build --release"
else
    cargo build --release
fi
SQUEEZEFS_BIN="$REPO_DIR/target/release/squeezefs"

# 3. Clone and compile xfstests-dev if not already done
XFSTESTS_DIR="/tmp/xfstests-dev"
# A cached checkout that never produced include/builddefs is a FAILED
# configure's residue — its config.cache pins the stale host environment
# and every retry dies with a misleading "make does not seem to be
# installed". Refresh it; a fully built checkout is still reused.
if [ -d "$XFSTESTS_DIR" ] && [ ! -f "$XFSTESTS_DIR/include/builddefs" ]; then
    echo "Refreshing stale xfstests checkout (configure residue, no builddefs)..."
    rm -rf "$XFSTESTS_DIR"
fi
if [ ! -d "$XFSTESTS_DIR" ]; then
    echo "Cloning xfstests-dev..."
    git clone --depth 1 https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git "$XFSTESTS_DIR"
fi

# Non-FHS hosts (NixOS): the suite's scripts, helpers and every test
# hardcode '#!/bin/bash' (and a few '#!/usr/bin/perl'), which do not
# exist outside FHS — group-list builds die 'bad interpreter' and every
# test would follow. Rewrite the shebang line to the resolved
# interpreter wherever the FHS path is absent; idempotent, and a plain
# FHS host never enters either arm.
if [ ! -x /bin/bash ]; then
    BASH_REAL="$(command -v bash)"
    # `|| true`: an already-rewritten checkout matches nothing and
    # grep's exit-1 must not kill the runner under `set -e`.
    { grep -rlIZ '^#!/bin/bash' "$XFSTESTS_DIR" 2>/dev/null || true; } |
        xargs -0 -r sed -i "1s|^#!/bin/bash|#!$BASH_REAL|"
fi
if [ ! -x /usr/bin/perl ] && command -v perl >/dev/null; then
    PERL_REAL="$(command -v perl)"
    { grep -rlIZ '^#!/usr/bin/perl' "$XFSTESTS_DIR" 2>/dev/null || true; } |
        xargs -0 -r sed -i "1s|^#!/usr/bin/perl|#!$PERL_REAL|"
fi

cd "$XFSTESTS_DIR"
# xfstests requires the fsgqa user/group; keep this OUTSIDE the
# compile-once guard so cached-suite runs repair it too. -m / the home
# repair: several tests `su - fsgqa`, and a missing home dir leaks a
# "cannot change directory" warning into golden output (generic/128's
# residual diff, VL10 release gate).
if ! getent group fsgqa >/dev/null; then
    groupadd fsgqa
fi
if ! getent passwd fsgqa >/dev/null; then
    useradd -m -g fsgqa fsgqa
fi
FSGQA_HOME="$(getent passwd fsgqa | cut -d: -f6)"
if [ -n "$FSGQA_HOME" ] && [ ! -d "$FSGQA_HOME" ]; then
    mkdir -p "$FSGQA_HOME"
    chown fsgqa:fsgqa "$FSGQA_HOME"
fi
# glibc >= 2.42 exports F_GETDELEG/F_SETDELEG from <fcntl.h>, so
# locktest.c's `#ifndef F_GETDELEG` fallback (which also declares
# struct delegation) never fires — but struct delegation itself lives
# only in <linux/fcntl.h>, which locktest.c does not include: the
# suite fails to COMPILE on bleeding-edge glibc (incomplete type).
# Force the local fallback — its values are identical to the kernel's
# (F_LINUX_SPECIFIC_BASE = 1024). Pure instrument header skew, same
# class as the generic/062 setfattr sed below; idempotent.
if ! grep -q 'squeezefs runner: force delegation fallback' src/locktest.c; then
    sed -i 's|^#ifndef F_GETDELEG$|#if 1 /* squeezefs runner: force delegation fallback — glibc 2.42+ defines F_GETDELEG in <fcntl.h> without struct delegation */\n#undef F_GETDELEG\n#undef F_SETDELEG|' \
        src/locktest.c
fi

if [ ! -f "src/open_by_handle" ]; then
    echo "Compiling xfstests..."
    # xfstests' m4/package_utilies.m4 resolves its build tools with
    # AC_PATH_PROG over HARDCODED FHS dirs (/bin:/usr/bin:...), which are
    # empty on non-FHS hosts (NixOS) — configure then dies "make does not
    # seem to be installed" with make plainly on PATH. AC_PATH_PROG
    # honors preset variables verbatim: pin every tool the macro file
    # names from the invoking PATH (absent ones stay unset — configure
    # keeps its own verdict for genuinely missing tools).
    for tool_var in AWK:awk ECHO:echo LIBTOOL:libtool MAKE:make \
        MSGFMT:msgfmt MSGMERGE:msgmerge SED:sed SORT:sort TAR:tar ZIP:gzip; do
        var="${tool_var%%:*}"
        bin="${tool_var##*:}"
        path="$(command -v "$bin" 2>/dev/null || true)"
        [ -n "$path" ] && export "$var"="$path"
    done
    make
fi

# attr >= 2.6 prints an UNCONDITIONAL warning on `setfattr --restore`
# whenever the dump contains any multi-component path ("unsafe without
# option -P") — reproduced on tmpfs with a plain nested dir, i.e. pure
# instrument noise the pre-2.6 golden output predates (VL10 release
# gate, generic/062's residual line). The dump paths are physical
# (getfattr -h walk), so -P is semantics-identical; idempotent sed.
sed -i 's/setfattr -h --restore=/setfattr -hP --restore=/' tests/generic/062

# 4. Install mount and mkfs helpers
# libmount resolves `mount -t fuse.squeezefs` through its COMPILED
# fs-search path — plain /sbin on FHS hosts, but e.g. NixOS builds it
# with /run/wrappers/bin:/run/current-system/sw/bin:/sbin and ships no
# /sbin at all. Install the helpers into the first root-writable
# member so mount(8) actually finds them; FHS hosts keep /sbin.
HELPER_DIR=/sbin
if [ ! -d "$HELPER_DIR" ]; then
    for d in /run/wrappers/bin /run/current-system/sw/bin; do
        if [ -d "$d" ] && [ -w "$d" ]; then
            HELPER_DIR="$d"
            break
        fi
    done
fi
if [ ! -d "$HELPER_DIR" ]; then
    echo "ERROR: no writable mount-helper dir (tried /sbin, /run/wrappers/bin)" >&2
    exit 1
fi
echo "Installing FUSE helpers in $HELPER_DIR..."

cat << EOF > $HELPER_DIR/mount.fuse.squeezefs
#!/usr/bin/env bash
# mount(8) helper for -t fuse.squeezefs. Contract: silent on success (stdout
# leaks into xfstests golden output), non-zero + stderr on failure, and the
# mount is usable when we return.
set -u
SQUEEZEFS_BIN="$SQUEEZEFS_BIN"
SCRATCH_DEV="$SCRATCH_DEV"
HELPER_DIR="$HELPER_DIR"
EOF
cat << 'EOF' >> $HELPER_DIR/mount.fuse.squeezefs
DEV="$1"
MNT="$2"
shift 2

OPTS=""
while [ $# -gt 0 ]; do
    if [ "$1" = "-o" ]; then
        OPTS="$2"
        shift 2
    else
        shift
    fi
done

# REMOUNT (mount -o remount,...) reaches subtype helpers too. It is a
# VFS-flag flip on the LIVE attachment (MS_REMOUNT — e.g. generic/294's
# remount,ro; 306/452 carry the same shape), NOT a new daemon: treating
# it as a fresh mount made the helper wait 60s for the (legitimately
# alive, mounted) daemon and fail the test. Hand it to the kernel
# directly (-i skips helper re-entry).
case ",$OPTS," in
*,remount,*)
    # LIBMOUNT_FORCE_MOUNT2: util-linux's new fsconfig() API re-submits
    # the existing fuse params on reconfigure, which kernel fuse refuses
    # ("No changes allowed in reconfigure"); the classic mount(2)
    # MS_REMOUNT path flips the VFS flags only.
    LIBMOUNT_FORCE_MOUNT2=always exec /usr/bin/mount -i -o "$OPTS" "$MNT"
    ;;
esac

ALL_OPTS="fsname=$DEV"
if [ -n "$OPTS" ]; then
    ALL_OPTS="$ALL_OPTS,$OPTS"
fi

TAG="$(basename "$MNT")"
LOG="/tmp/squeezefs_fstests_${TAG}.log"

# SECOND MOUNTPOINT of an ALREADY-MOUNTED device (fstests generic/732:
# same export at two mountpoints, cross-mountpoint renames): local
# filesystems share the superblock; the FUSE equivalent is a BIND of the
# live attachment — one daemon, one D0 writer claim, full coherence by
# construction. Only when the daemon is LIVE and its mountpoint is
# attached; a draining/dying daemon falls through to the serialize-wait.
LIVE_MNT="$(pgrep -af "squeezefs mount sqmeta://$DEV " 2>/dev/null | head -1 | \
    sed -nE "s#.*squeezefs mount sqmeta://$DEV ([^ ]+) .*#\1#p")"
if [ -n "$LIVE_MNT" ] && [ "$LIVE_MNT" != "$MNT" ] && mountpoint -q "$LIVE_MNT" 2>/dev/null; then
    exec /usr/bin/mount --bind "$LIVE_MNT" "$MNT"
fi

# xfstests' `umount` returns while the previous daemon is still draining
# staged writes to this same meta volume + staging dir. Mounting a second
# daemon on top of that races fencing/recovery and corrupts the volume, and a
# daemon dying mid-teardown leaves the mountpoint ENOTCONN. Serialize: wait
# (up to 60s) for any prior daemon on this device to exit before mounting.
for _ in $(seq 1 600); do
    pgrep -f "squeezefs mount sqmeta://$DEV " >/dev/null 2>&1 || break
    sleep 0.1
done
if pgrep -f "squeezefs mount sqmeta://$DEV " >/dev/null 2>&1; then
    echo "mount.fuse.squeezefs: previous daemon for $DEV still running after 60s" >&2
    exit 32
fi

# Clear stale mountpoint state: detach a dead FUSE attachment (stat fails
# with ENOTCONN), then remove any files leaked onto the underlying directory
# — the daemon refuses non-empty mountpoints.
if ! stat "$MNT" >/dev/null 2>&1; then
    umount -l "$MNT" 2>/dev/null
fi
if ! mountpoint -q "$MNT" 2>/dev/null; then
    find "$MNT" -mindepth 1 -delete 2>/dev/null
fi

# SCRATCH raw-clobber resilience (2026-07-13 release-gate provenance):
# xfstests treats SCRATCH_DEV as a raw block device it may scribble on —
# generic/515 pwrites 0x58 over [0, 300 MiB) BEFORE its _require bails
# notrun on FUSE (_scratch_mkfs_sized unsupported), and 250/252/399 carry
# the same shape behind other _requires. Real filesystems re-mkfs inside
# those tests; FUSE scratch is never re-mkfs'd (common/rc _scratch_mkfs
# just rm -rf's), so one raw writer destroyed the superblock and
# mountfailed all 84 later scratch tests of the first full sweep. The
# scratch volume is disposable between tests BY xfstests' own contract,
# so: if the SCRATCH meta device no longer carries a squeezefs
# superblock (magic "METALV01" at byte 0), re-mkfs it before mounting.
# The predicate is deliberately NARROW — a volume whose magic is intact
# but whose innards are corrupt (the Finding-A class) still fails the
# mount LOUD; only a foreign raw overwrite (X-splat, zero-splat) can
# strip the magic. TEST_DEV is exempt: its state persists across tests
# by design and auto-reformat there would mask real damage.
if [ "$DEV" = "$SCRATCH_DEV" ] && [ -e "$DEV" ]; then
    MAGIC=$(head -c 8 "$DEV" 2>/dev/null | LC_ALL=C tr -d '\0')
    if [ "$MAGIC" != "METALV01" ]; then
        {
            echo "mount.fuse.squeezefs: scratch superblock magic gone" \
                 "(raw-clobber class, e.g. generic/515) — re-mkfs $DEV"
        } >> "$LOG" 2>&1
        if ! $HELPER_DIR/mkfs.fuse.squeezefs "$DEV" >> "$LOG" 2>&1; then
            echo "mount.fuse.squeezefs: raw-clobber re-mkfs of $DEV failed; see $LOG" >&2
            exit 32
        fi
    fi
fi

# xfstests' check runs every test inside a transient systemd scope and stops
# that scope when the test exits ("kill all subprocesses of the test").
# A daemon mounted from inside a test inherits the test's cgroup and gets
# SIGTERM/SIGKILL from systemd at test end while still mounted — aborting the
# FUSE connection (ENOTCONN mountpoint, fencing races). Launch the daemon in
# its own scope so it only ever exits via unmount.
#
# check also bumps each test's oom_score_adj to 250 so the OOM killer
# sacrifices tests, not the framework; a daemon mounted from inside a test
# inherits that bias and becomes the preferred OOM victim while mounted,
# which kills the mount under the whole remaining run. Reset to neutral —
# a genuinely leaking daemon still gets picked on its own RSS.
echo 0 > /proc/self/oom_score_adj 2>/dev/null || true

LAUNCH=()
if [ -d /run/systemd/system ] && command -v systemd-run >/dev/null 2>&1; then
    LAUNCH=(systemd-run --quiet --collect --scope \
        --unit "squeezefs-fstests-${TAG}-$$-$(date +%s%N)")
    # Safety rail (opt-in): cap the daemon so an unbounded-allocation
    # regression (the generic/285 ~108 GB RSS family) OOM-kills the leaking
    # daemon's scope, never the box. Export SQUEEZEFS_FSTESTS_MEMMAX=8G on
    # runs chasing memory bugs; unset = unchanged behavior.
    if [ -n "${SQUEEZEFS_FSTESTS_MEMMAX:-}" ]; then
        LAUNCH+=(-p "MemoryMax=${SQUEEZEFS_FSTESTS_MEMMAX}" -p "MemorySwapMax=0")
    fi
fi

# Cache-path policy: staging dirs are declared at FORMAT (see the mkfs
# helper) and read from the format config — mount rejects the flag.
"${LAUNCH[@]}" "$SQUEEZEFS_BIN" mount \
    "sqmeta://$DEV" \
    "$MNT" \
    --daemon \
    --disk-cache-size 500MB \
    --allow-other \
    -o "$ALL_OPTS" \
    --log-file "$LOG" >> "$LOG" 2>&1
rc=$?
if [ $rc -ne 0 ]; then
    echo "mount.fuse.squeezefs: squeezefs mount $DEV -> $MNT failed (rc=$rc); see $LOG" >&2
    exit 32
fi
exit 0
EOF
chmod +x $HELPER_DIR/mount.fuse.squeezefs

cat << EOF > $HELPER_DIR/mkfs.fuse.squeezefs
#!/usr/bin/env bash
set -u
SQUEEZEFS_BIN="$SQUEEZEFS_BIN"
META_SIZE="$META_SIZE"
DATA_SIZE="$DATA_SIZE"
FORMAT_EXTRA_ARGS="${SQUEEZEFS_FSTESTS_FORMAT_ARGS:-}"
EOF
cat << 'EOF' >> $HELPER_DIR/mkfs.fuse.squeezefs
DEV="$1"
DATA_DEV="${DEV/_meta/_data}"

if [ ! -b "$DEV" ]; then
    # Recreate sparse backing files so a re-format drops old allocations.
    rm -f "$DEV" "$DATA_DEV"
    truncate -s "$META_SIZE" "$DEV"
    truncate -s "$DATA_SIZE" "$DATA_DEV"
fi

# Cache-path policy: staging dirs are DECLARED AT FORMAT (recorded in the
# format config; mount rejects the flag). Key the dir by device so both
# harness filesystems keep disjoint staging.
STAGING_DIR="/tmp/squeezefs_fstests_staging_$(basename "$DEV")"
mkdir -p "$STAGING_DIR"

# Format. FORMAT_EXTRA_ARGS is baked from SQUEEZEFS_FSTESTS_FORMAT_ARGS at
# runner start (e.g. "--multi-writer" for the stamped-solo posture — the MW
# S4 residual gate); word-splitting is intentional.
exec "$SQUEEZEFS_BIN" format \
    "sqmeta://$DEV" \
    "sqdata://$DATA_DEV" \
    --disk-cache-paths "$STAGING_DIR" \
    --force $FORMAT_EXTRA_ARGS
EOF
chmod +x $HELPER_DIR/mkfs.fuse.squeezefs

# UMOUNT_PROG wrapper: a freshly armed FUSE-over-io_uring mount holds a
# kernel-side reference for up to ~100ms after mount(8) returns, so the
# zero-dwell umount xfstests issues in cycle-mount paths fails EBUSY with no
# userspace holder (deterministic in e.g. generic/003). Retry briefly; a real
# leak still fails after the 5s budget. Silent on eventual success so no
# noise reaches golden output.
UMOUNT_REAL="$(type -P umount)"
cat << EOF > $HELPER_DIR/umount.squeezefs-fstests
#!/usr/bin/env bash
UMOUNT_REAL="$UMOUNT_REAL"
EOF
cat << 'EOF' >> $HELPER_DIR/umount.squeezefs-fstests
rc=0
for _ in $(seq 1 100); do
    ERR=$("$UMOUNT_REAL" "$@" 2>&1)
    rc=$?
    [ $rc -eq 0 ] && exit 0
    case "$ERR" in
    *"target is busy"*)
        sleep 0.05
        ;;
    *)
        break
        ;;
    esac
done
[ -n "$ERR" ] && echo "$ERR" >&2
exit $rc
EOF
chmod +x $HELPER_DIR/umount.squeezefs-fstests

# 5. Create local.config
echo "Configuring xfstests local.config..."
cat << EOF > "$XFSTESTS_DIR/local.config"
export FSTYP=fuse
export FUSE_SUBTYP=.squeezefs

export TEST_DIR=$TEST_DIR
export SCRATCH_MNT=$SCRATCH_MNT

export TEST_DEV=$TEST_DEV
export SCRATCH_DEV=$SCRATCH_DEV

# common/config sets UMOUNT_PROG before sourcing this file; override it so
# every harness unmount rides the EBUSY-retry wrapper installed by
# tests/run_fstests.sh.
export UMOUNT_PROG=$HELPER_DIR/umount.squeezefs-fstests
EOF

# Pre-format TEST_DEV and SCRATCH_DEV once since xfstests doesn't mkfs them for FUSE
$HELPER_DIR/mkfs.fuse.squeezefs "$TEST_DEV"
$HELPER_DIR/mkfs.fuse.squeezefs "$SCRATCH_DEV"

# 6. Run fstests
#
# Test tiers (see AGENTS.md "Test tiering"):
#   - explicit args    -> exactly those tests (targeted fix loop:
#                         `sudo tests/run_fstests.sh generic/616`)
#   - FSTESTS_QUICK=1   -> the curated squeezefs regression set below
#                         (per-PR data-path tier; minutes, not hours)
#   - no args           -> full `-g auto` inventory (nightly / release-gate
#                         tier; ~5 h — run once, never between fixes)
#
# SQUEEZEFS_FSTESTS_FORMAT_ARGS: extra `squeezefs format` args baked into the
# mkfs wrapper. Since the rung-10b Phase-B flip the DEFAULT format is the
# stamped (multi-writer-capable) class, so the interesting NON-default
# posture is "--single-writer" (the unstamped class); "--multi-writer"
# stays accepted as an announced-inert spelling of the default. (The MW
# §6.3 S4 residual gate ran the QUICK set stamped via this lever pre-flip.)
#
# SQUEEZEFS_FSTESTS_QUICK is the STANDING REGRESSION SET: every fstests case
# that has ever caught a real SqueezeFS bug, plus core fsx/fsstress data-path
# soakers, hole/punch/seek coverage, and mount-cycle basics. GROW THIS LIST
# whenever a new test surfaces a bug — that is the whole point of the tier.
# Provenance (2026-07-09/10 v3 bring-up):
#   112/616/617/618 = copy_file_range crawl (fix 97e2ed4)
#   616             = hole-read family: PUNCH_HOLE/truncate coherence
#                     (fixes 37fe5eb + 49286ee stale-size truncate)
#   075/091         = write-visibility family: fallocate size clobber under
#                     a lagging durable size, truncate leaving parked/staged
#                     active-block overlays alive, overlay-blind
#                     copy_file_range, unwritten uring read-dest regions,
#                     flush-merge-vs-prune reordering (layout-prune epoch),
#                     dual-overlay authority (fix 93dce89) — GREEN 3/3 each.
#                     The rare 075.2 soak residual (stale bytes one block
#                     over — the reused-key stale-fill / block-index→key
#                     binding ABA, 8e3995e follow-up) is closed by the
#                     binding-validated serve fix; regression pins live in
#                     tests/reused_key_stale_fill_tests.rs and averted
#                     serves surface as stale_binding_rebinds on .stats
#   008/009/285/316 = fallocate / zero-range / SEEK_HOLE / punch coverage
#   285             = daemon OOM (~108 GB RSS): staged->striped promotion
#                     materialized the whole logical span for a 64 KiB write
#                     at a ~8/16 TiB offset (seek_sanity huge_file_test).
#                     FIXED (sparse O(map) promotion; pins in
#                     tests/sparse_write_bounded_tests.rs) — GREEN. NOTE:
#                     SEEK_HOLE/SEEK_DATA are intentionally kernel-default
#                     (no FUSE_LSEEK): 285 runs in seek_sanity's accepted
#                     "default behavior" mode; a layout-granularity native
#                     lseek would FAIL its st_blksize-granularity probes.
#   617             = O_DIRECT short read below EOF ("uring read bad io
#                     length"): reads spanning a staged/inline implicit-zero
#                     hole tail returned only the physically-backed prefix
#                     (page cache masked it; fsx -Z exposed it). FIXED
#                     (full-length below-EOF zero-fill in
#                     read_file_range_zero_copy; pins in
#                     tests/read_full_length_tests.rs) — GREEN.
#   074/127/616 (+075) = staged-identity transient-ZEROS family (fixes
#                     f924085 atomic ring replace + 0a184f3 read-side
#                     identity revalidation; RSS-creep reclaim rides
#                     f924085) — see the FIXED rows in the expected-result
#                     table below; pins in
#                     tests/staged_identity_visibility_tests.rs. 074's
#                     second, distinct striped/mmap stale-fill bug
#                     (fstest.3 leg, budget-dependent) = FIXED by the
#                     put-ring geometry-complete eviction (f29520e) — see
#                     its FIXED row below; pins in
#                     tests/reused_key_stale_fill_tests.rs.
#   001/013/074/127/213/263 = mount-cycle + fsx/fsstress core soak
#
# DETERMINISTIC EXPECTED RESULT of this tier (2026-07-11, post 285/617 +
# staged-identity transient-zeros + 074 striped stale-fill fixes).
# Anything deviating from this table is a REGRESSION:
#   PASS (deterministic): 001 008 013 069 074 075 091 112 127 263 285 464
#                     469 616 617 618
#     ... 074's THIRD family (the fstest.4 -F/-mS stale-by-one-loop unit,
#           VL8 catalog item 1) = FIXED 2026-07-21
#           (fix/write-wedge-and-074: open() ignored O_TRUNC while fuse3
#           negotiates FUSE_ATOMIC_O_TRUNC — the kernel never sends the
#           SETATTR(0) fallback, so every fstest loop's truncate was a
#           daemon-side no-op and the previous generation's state
#           survived into the next; pins in
#           tests/mmap_writeback_staleness_tests.rs, counted x20 in
#           .benchmarks/2026-07-21-wedge-and-074-fixes.md). A recurrence
#           of the stale-by-one-loop signature on fstest.4 IS a
#           regression, as is any other 074 diff.
#   NOTRUN (deterministic, platform): 009 316 — both _require xfs_io fiemap;
#                     FUSE has no FIEMAP ioctl. Kept as canaries: they start
#                     RUNNING (and their punch/prealloc coverage arms) the
#                     day a FIEMAP-capable kernel/fuse lands.
#   FAIL (deterministic, platform-class — expected-fail, kept as canaries):
#     003 = noatime by design (user ruling, VL8 item-3 adjudication: no
#           read-path atime write exists — the JuiceFS reference-client
#           posture; relatime/strictatime unsupported) + kernel-TTL attr
#           observation. Expected shape re-pinned 2026-07-22 (VL10 release
#           gate): exactly SIX ERROR lines — 4 × "access time has not been
#           updated" (file1 first time / file2 / file3 second time / file3
#           third time) + "change time has changed for file1 after
#           remount" + "change time has changed after accessing file3
#           second time". (Was 10 lines; the create-parent-attr-refresh
#           fix 37f1c86 removed four ctime/mtime observation legs.) A
#           DIFFERENT diff than those 6 lines = regression.
#     192 = the SAME noatime-by-design class (adjudicated with 003,
#           2026-07-22 — generic/192 measures the atime delta after a
#           sleeping read; _require_atime notruns ceph/"atime not
#           maintained" filesystems upstream but knows no generic-fuse
#           spelling, so it runs here). Expected shape: exactly
#           "delta1 has value of 0" + "delta1 is NOT in range 5 .. 7"
#           replacing the golden "delta1 is in range" (delta2 = mtime
#           stays in range). Any other diff = regression.
#     213 = thin provisioning: fallocate(mode=0) never reserves physical
#           blocks (sparse/dynamic backend by design — statfs now reports
#           honest capacity/allocated numbers per fix/real-statfs, but
#           fallocate still reserves nothing), so the "fallocate: No
#           space left on device" golden line never appears. All other
#           213 legs pass; exactly that one missing line is the expected
#           diff (re-verified 2026-07-12 on the honest-statfs branch:
#           identical diff shape, 2x rolls).
#   464 EIO class (FIND-RW5-A, the staged-write-storm ring-pressure EIO)
#           = FIXED 2026-07-21 — the charter LANDED (fix/write-wedge-and-074;
#           counted x10 fully green in
#           .benchmarks/2026-07-21-wedge-and-074-fixes.md). 464 moved
#           from expected-FAIL to expected-PASS above. Was: 464's 16-proc
#           delalloc/append/sync_range storm over 200 files structurally
#           oversubscribes this harness's 500MB staging ring and user
#           writes surfaced EIO. Six convicted faces, all pinned in
#           tests/rw5a_never_lossy_tests.rs: (1) StorageFull propagation
#           from the fold-rider re-stage + clone staged arms (now durable
#           spills, counted staged_spill_escalations); (2) rebind
#           exhaustion — cohort fill inheritance defeated the stripe
#           escalation (now device-true + stripe-locked escalated
#           attempts, bound 24 w/ backoff); (3) RELEASE dropped the shared
#           op lease with other handles open (now last-close only + one
#           fresh-lease write retry); (4) duplicate reclaim (RELEASE+FORGET
#           both enqueued -> delete_file twice -> block double-free; now
#           reclaim_inflight single-drive guard); (5) merge RMW based on
#           the lagging backend over a DIRTY RAM layout (now dirty-
#           authority rule); (6) untracked frees freed unconditionally —
#           the second half of any double-release minted one offset to two
#           live owners (now refused-and-counted,
#           block_untracked_free_refusals; block_double_frees is a
#           must-stay-0 tripwire). ANY 464 diff (EIO line, wedge, or
#           other) IS now a regression — capture the daemon logs and the
#           DOUBLE FREE / REFUSED untracked / did-not-settle greps first.
#           The 464 WEDGE mode (VL8 item 2 capture 2, writes-only stuck
#           census) = FIXED 2026-07-21 (fix/write-wedge-and-074: the
#           staged-ledger scc bucket was a second Hang-1 lock population
#           — executor threads blocked in ledger *_sync ops while the
#           bucket holder waited the shard write lock behind a parked
#           §5.5 guard; ledger access is now scc *_async on executor
#           paths). Pin: tests/staging_shard_deadlock_tests.rs
#           (ledger_read_never_blocks_executor...); counted x10 in
#           .benchmarks/2026-07-21-wedge-and-074-fixes.md. The watchdog
#           additionally logs a named-holder lock-wait census whenever
#           overdue ops exist. ANY new wedge (op in flight > 30 s
#           forever) IS a regression — capture the census lines first.
#   127/616 (+074 fstest.2, 075) transient-ZEROS family = FIXED
#           (fix/staged-identity-transient-zeros; ring atomic same-key
#           replace f924085 + staged-identity read revalidation 0a184f3).
#           Was: transient zeros reads under buffered write+read churn
#           (fsx READ BAD DATA of zeros for recently-written ranges,
#           durably correct afterward; 074 children corrupt), .stats
#           staged_payload_lost_reads firing on a healthy mount. Root
#           causes: (1) reserve_and_write removed the ring index entry for
#           the whole replacement memcpy — every re-stage exposed a
#           key-absent window whose readers fell into the crash-recovery
#           zeros-degrade leg; (2) readers holding a pre-transition meta
#           snapshot missed moved identities (staged→inline/striped, spill
#           re-id, promote) — reads now re-resolve and re-dispatch
#           bounded, and the zeros leg is reachable only for a STABLE lost
#           identity (genuine crash loss). Acceptance 2026-07-11: seeded
#           shapes 6/6 each (616 golden line, 127 fsx_std_mmap seed
#           191110531, 074 fstest.2 -F children), lost_reads = 0 across
#           all 18 runs + a 15-min 3.06M-op fsx churn (RSS creep also
#           fixed: dead ring extents punched). Pins in
#           tests/staged_identity_visibility_tests.rs. A recurrence of
#           the ZEROS signature on these tests IS a regression.
#   074 striped/mmap stale-fill (the fstest.3 leg, -s 30M -b 512 -m under
#           the harness's 500 MB disk-cache budget) = FIXED
#           (fix/striped-stale-fill-074: 34c0871 red pins + f29520e).
#           Was: whole 512 B blocks reading back a NEARBY round's fill
#           (stale content, never zeros; all staged-identity counters
#           silent; durable state clean — a poisoned NVMe read-cache
#           entry probed live). Root cause: NvmeShard::evict_overlapping
#           walked only the FRONT RUN of active_keys assuming queue order
#           == ring-position order; same-key replaces + out-of-order
#           concurrent placements broke it, the sweep stopped early, and
#           a placement memcpy CLOBBERED a live indexed entry — served
#           under a valid key + incarnation + binding. Eviction is now
#           geometry-complete against the authoritative extent map;
#           defense in depth: terminal-free read-tier purge + incarnation-
#           validated non-owner publishes (dehydration, p2p). Acceptance
#           2026-07-11: isolated repro 12/12 (was 4-11/12 failing),
#           ./check generic/074 6/6, read-verify diagnostic 0 mismatches.
#           Pins in tests/reused_key_stale_fill_tests.rs. A recurrence of
#           the stale-fill signature on 074 IS a regression.
#   FLAKE (pre-existing capacity shape under SQUEEZEFS_FSTESTS_MEMMAX=8G):
#     tier-tail tests (observed on 618) can fail via _check_dmesg when the
#           TEST daemon's CUMULATIVE budgeted RSS over the 19-test roll
#           (2x1GB RAM LRUs + 512MB/vol KV node cache + segments +
#           jemalloc retention) crosses the 8G rail mid-test and the
#           cgroup OOM-kills the daemon. NOT a leak and NOT new: pristine
#           dev@74190d2 peaks HIGHER on the same roll (5.63 GB vs 5.06 GB
#           sampled 2026-07-11), fresh-mount 15-min fsx churn is
#           self-limiting (3.06M ops, negative last-10-min drift), and
#           618 standalone passes 3/3. An OOM on an EARLY test or under a
#           bigger cap IS a regression.
# VL10 release-gate additions (2026-07-22). Kernel-interface-only rows
# (the repro-port mandate's documented exception class — their semantics
# live in the kernel's own lock code once FUSE_POSIX_LOCKS/FLOCK_LOCKS
# stopped being advertised, unreachable from cargo tests):
#   131 = POSIX byte-range lock semantics (kernel-local; locktest)
#   478 = OFD locks (kernel-local)
#   504 = flock + /proc/locks visibility (kernel-local)
# Cargo-pinned rows added for their fstests faces: 020 (xattr value cap),
# 035 (rename dir-overwrite nlink), 062 (virtuals unlisted), 128
# (-o nosuid honored), 258 (pre-epoch timestamps), 426/467/477
# (EXPORT_SUPPORT '.'/'..' revival), 525 (EFBIG size cap), 533
# (removexattr ENODATA), 294/306/452 (the O(size) delete linger family —
# rides the sparse_write_bounded cargo pin), 451 (async-DIO write vs
# buffered-read page coherence — the post-write kernel invalidation law,
# rides the dio_write_page_coherence cargo pin).
SQUEEZEFS_FSTESTS_QUICK=(
    generic/001 generic/003 generic/008 generic/009 generic/013
    generic/020 generic/035 generic/062 generic/069 generic/074
    generic/075 generic/091 generic/112 generic/127 generic/128
    generic/131 generic/213 generic/258 generic/263 generic/285
    generic/294 generic/306 generic/316 generic/423 generic/426
    generic/451 generic/452
    generic/464 generic/467 generic/469 generic/477 generic/478
    generic/504 generic/525 generic/533 generic/551 generic/590
    generic/616 generic/617
    generic/618 generic/631 generic/683 generic/732 generic/795
)

# ---------------------------------------------------------------------------
# FAIL FAST (standing user rule, 2026-07-22): full and QUICK runs abort at
# the FIRST unexpected failure — nonzero exit, artifacts preserved, the
# failing test named loudly. The adjudicated by-design set (003/192 noatime,
# 213 thin provisioning) continues ONLY when its failure diff matches the
# pinned expected shape EXACTLY; any other diff on those tests aborts too.
# Single-test invocations (explicit args) keep the classic one-shot check.
# xfstests' check has no first-class fail-fast, so the full run expands
# `-g auto` (`check -n`) and drives the list per test.
# ---------------------------------------------------------------------------

# The pinned expected shapes, verbatim `diff tests/<t>.out results/<t>.out.bad`.
# generic/634 (adjudicated 2026-07-23): the on-disk timestamp word is i64
# NANOSECONDS — a deliberate ±292-year range (1677..2262), the same
# finite-range class as ext4 u34/xfs bigtime. The daemon half of 634's
# clamp-and-persist contract is exact (deterministic saturation, pinned in
# tests/attr_refresh_tests.rs::out_of_range_timestamps_saturate_*); the
# kernel half CANNOT be satisfied over FUSE — incore clamping needs
# sb->s_time_max and the FUSE protocol has no field to advertise it, so
# the kernel keeps huge dates incore while the daemon persists the clamp,
# and 634's before/after-remount diff shows exactly the six saturated
# rows below. Kernel-interface-only (the 131/478/504 exception class).
# Any OTHER diff on 634 = regression.
# generic/003 (re-adjudicated 2026-07-28, release-gate fix loop —
# .benchmarks/2026-07-28-release-gate-v1.1.md): the FOUR atime lines are
# the noatime-by-design core (mandatory, every run). The former
# deterministic ctime lines were a daemon bug — layout persistence
# fabricated a second ctime authority — fixed with its cargo repro
# (tests/write_times_durability_tests.rs). What remains is
# KERNEL-INTERFACE-ONLY (the 131/478/504/634 class): under the FUSE
# writeback cache the kernel authors regular-file m/ctime at write(2),
# overrides GETATTR times incore for the inode's lifetime, and sends the
# daemon its stamp ONLY on fsync — never on close (probed live: WRITE is
# delivered at FLUSH time, no flush-times SETATTR follows). The daemon's
# best durable estimate is its WRITE-arrival stamp, µs later — so about
# 1 run in 5 the two straddle a coarse tick and the remount legs show a
# PAIRED modify+change divergence for that file. The tolerated shapes are
# the ENUMERATED byte-exact set below (variant 1 = mandatory core; 2 =
# file1 pair; 3 = file3 pair; 4 = both). Unpaired time lines or any
# other line still abort.
expected_shape_diff() {
    case "$1" in
    generic/003) cat <<'EOF'
1a2,5
> ERROR: access time has not been updated after accessing file1 first time
> ERROR: access time has not been updated after accessing file2
> ERROR: access time has not been updated after accessing file3 second time
> ERROR: access time has not been updated after accessing file3 third time
EOF
        ;;
    generic/003@2) cat <<'EOF'
1a2,7
> ERROR: access time has not been updated after accessing file1 first time
> ERROR: modify time has changed for file1 after remount
> ERROR: change time has changed for file1 after remount
> ERROR: access time has not been updated after accessing file2
> ERROR: access time has not been updated after accessing file3 second time
> ERROR: access time has not been updated after accessing file3 third time
EOF
        ;;
    generic/003@3) cat <<'EOF'
1a2,7
> ERROR: access time has not been updated after accessing file1 first time
> ERROR: access time has not been updated after accessing file2
> ERROR: access time has not been updated after accessing file3 second time
> ERROR: modify time has changed after accessing file3 second time
> ERROR: change time has changed after accessing file3 second time
> ERROR: access time has not been updated after accessing file3 third time
EOF
        ;;
    generic/003@4) cat <<'EOF'
1a2,9
> ERROR: access time has not been updated after accessing file1 first time
> ERROR: modify time has changed for file1 after remount
> ERROR: change time has changed for file1 after remount
> ERROR: access time has not been updated after accessing file2
> ERROR: access time has not been updated after accessing file3 second time
> ERROR: modify time has changed after accessing file3 second time
> ERROR: change time has changed after accessing file3 second time
> ERROR: access time has not been updated after accessing file3 third time
EOF
        ;;
    generic/192) cat <<'EOF'
4c4,5
< delta1 is in range
---
> delta1 has value of 0
> delta1 is NOT in range 5 .. 7
EOF
        ;;
    generic/213) cat <<'EOF'
4d3
< fallocate: No space left on device
EOF
        ;;
    generic/634) cat <<'EOF'
1a2,41
> 2,4c2,4
> < 2147483647-12-31 23:59:59.000000000 +0000 67767976233532799 /mnt/squeezefs_scratch/t_abs_max_time
> < stat.mtime.tv_sec = 67767976233532799
> < stat.mtime.tv_nsec = 0
> ---
> > 2262-04-11 23:47:16.854775807 +0000 9223372036 /mnt/squeezefs_scratch/t_abs_max_time
> > stat.mtime.tv_sec = 9223372036
> > stat.mtime.tv_nsec = 854775807
> 6,8c6,8
> < 0000-01-01 00:00:00.000000000 +0000 -62167219200 /mnt/squeezefs_scratch/t_abs_min_time
> < stat.mtime.tv_sec = -62167219200
> < stat.mtime.tv_nsec = 0
> ---
> > 1677-09-21 00:12:43.145224192 +0000 -9223372037 /mnt/squeezefs_scratch/t_abs_min_time
> > stat.mtime.tv_sec = -9223372037
> > stat.mtime.tv_nsec = 145224192
> 22,24c22,24
> < 2446-05-10 22:38:55.000000000 +0000 15032385535 /mnt/squeezefs_scratch/t_u34_from_s32_min
> < stat.mtime.tv_sec = 15032385535
> < stat.mtime.tv_nsec = 0
> ---
> > 2262-04-11 23:47:16.854775807 +0000 9223372036 /mnt/squeezefs_scratch/t_u34_from_s32_min
> > stat.mtime.tv_sec = 9223372036
> > stat.mtime.tv_nsec = 854775807
> 26,28c26,28
> < 2514-05-30 01:53:03.000000000 +0000 17179869183 /mnt/squeezefs_scratch/t_u34_max
> < stat.mtime.tv_sec = 17179869183
> < stat.mtime.tv_nsec = 0
> ---
> > 2262-04-11 23:47:16.854775807 +0000 9223372036 /mnt/squeezefs_scratch/t_u34_max
> > stat.mtime.tv_sec = 9223372036
> > stat.mtime.tv_nsec = 854775807
> 30,32c30,32
> < 2486-07-02 20:20:24.000000000 +0000 16299260424 /mnt/squeezefs_scratch/t_u64ns_from_s32_min
> < stat.mtime.tv_sec = 16299260424
> < stat.mtime.tv_nsec = 0
> ---
> > 2262-04-11 23:47:16.854775807 +0000 9223372036 /mnt/squeezefs_scratch/t_u64ns_from_s32_min
> > stat.mtime.tv_sec = 9223372036
> > stat.mtime.tv_nsec = 854775807
EOF
        ;;
    *) return 1 ;;
    esac
}

# HOST-HARDWARE dmesg noise (documented instrument class, 2026-07-23):
# xfstests' _check_dmesg fails a test on ANY "WARNING:"-class kernel line
# since the test started — including this box's amdgpu display-driver
# idle-power WARN (dc_dmub_srv_apply_idle_power_optimizations, fired by
# desktop vblank activity, zero fs frames) and split-lock CPU traps from
# unrelated desktop processes. Those are not filesystem findings and
# recur randomly, so a DMESG-ONLY failure (golden output matched — no
# out.bad) continues IFF every triggering line is a named host-hardware
# frame. Anything naming fs/fuse/mm/block paths stays fatal; the flagged
# lines are printed either way. No cargo repro possible (host GPU
# driver) — the same instrument-noise class as the attr `--restore` sed.
dmesg_failure_is_host_noise() {
    local t="$1" f="results/${t}.dmesg"
    [ -f "$f" ] || return 1
    [ -f "results/${t}.out.bad" ] && return 1
    local hits
    hits=$(grep -E -e "kernel BUG at" -e "WARNING:" -e "\bBUG:" -e "Oops:" \
        -e "possible recursive locking detected" \
        -e "(INFO|ERR): suspicious RCU usage" \
        -e "INFO: possible circular locking dependency detected" \
        -e "general protection fault:" -e "BUG .* remaining" \
        -e "oom-kill" -e "UBSAN:" "$f" || true)
    [ -n "$hits" ] || return 1
    echo "--- dmesg trigger lines for $t ---"
    printf '%s\n' "$hits"
    # EVERY triggering line must be a known host-hardware frame.
    if printf '%s\n' "$hits" | grep -E -q -v \
        -e "drivers/gpu/drm" -e "amdgpu" -e "dc_dmub_srv" \
        -e "split lock" -e "bus_lock" -e "wireless extensions"; then
        return 1
    fi
    return 0
}

# A failed test continues only if its out.bad diff IS one of its pinned
# shapes (a test may pin an ENUMERATED variant set as "<t>@2", "<t>@3", …
# — generic/003's kernel-clock-race pairs; each variant is still matched
# byte-exact, so anything outside the enumeration aborts).
failure_is_expected_shape() {
    local t="$1"
    local golden="tests/${t}.out" bad="results/${t}.out.bad"
    local want got variant
    [ -f "$golden" ] && [ -f "$bad" ] || return 1
    got="$(diff "$golden" "$bad" 2>/dev/null || true)"
    for variant in "$t" "$t@2" "$t@3" "$t@4"; do
        want="$(expected_shape_diff "$variant")" || continue
        if [ "$got" = "$want" ]; then
            return 0
        fi
    done
    return 1
}

# Per-test driver: abort on the first unexpected failure.
run_check_failfast() {
    local ran=0 clean=0 shaped=0
    local t rc
    for t in "$@"; do
        ran=$((ran + 1))
        set +e
        ./check "$t"
        rc=$?
        set -e
        if [ $rc -eq 0 ]; then
            clean=$((clean + 1))
            continue
        fi
        if failure_is_expected_shape "$t"; then
            shaped=$((shaped + 1))
            echo "EXPECTED-SHAPE: $t failed with exactly its pinned by-design diff" \
                 "(003/192 = noatime class, 213 = thin provisioning) — continuing"
            continue
        fi
        if dmesg_failure_is_host_noise "$t"; then
            shaped=$((shaped + 1))
            echo "HOST-NOISE DMESG: $t failed only _check_dmesg and every trigger" \
                 "line is a named host-hardware frame (GPU/CPU-errata — lines" \
                 "above; results/${t}.dmesg preserved) — continuing"
            continue
        fi
        echo "==================================================================" >&2
        echo "FAIL FAST: $t FAILED UNEXPECTEDLY (test $ran of $#)" >&2
        echo "Artifacts preserved under $XFSTESTS_DIR/results/${t}*" >&2
        if [ -f "results/${t}.out.bad" ]; then
            echo "--- diff tests/${t}.out results/${t}.out.bad (head) ---" >&2
            diff "tests/${t}.out" "results/${t}.out.bad" 2>/dev/null | head -40 >&2 || true
        fi
        echo "Fix it (red cargo repro-port first), re-run the single test, then" >&2
        echo "RESUME with: sudo tests/run_fstests.sh --resume-from $t" >&2
        echo "(debugging efficiency only — final acceptance is still ONE complete" >&2
        echo "from-zero pass on the final binary)." >&2
        echo "==================================================================" >&2
        return 1
    done
    echo "FAIL-FAST SUMMARY: $ran ran, $clean clean, $shaped expected-shape, 0 unexpected"
}

cd "$XFSTESTS_DIR"
# The persisted from-zero expansion order — what makes a resume
# deterministic (the resume rule, user directive 2026-07-23).
ORDER_FILE="$XFSTESTS_DIR/vl10_full_list.order"

expand_full_list() {
    mapfile -t FULL_LIST < <(./check -n -g auto 2>/dev/null | grep -oE '^[a-z]+/[0-9]+')
    if [ "${#FULL_LIST[@]}" -lt 100 ]; then
        echo "ERROR: -g auto expansion produced only ${#FULL_LIST[@]} tests — refusing" >&2
        exit 1
    fi
}

EXIT_CODE=0
if [ "${1:-}" = "--resume-from" ]; then
    # RESUME rule (user directive 2026-07-23): after a fail-fast abort at
    # test T is fixed, re-run T then RESUME from T's position to the END
    # of the persisted from-zero order. Debugging efficiency ONLY — the
    # release-gate acceptance is still one complete from-zero pass on the
    # final binary; prior-position greens from the aborted pass are
    # provisional and the closing report cites only the from-zero pass.
    RESUME_AT="${2:?--resume-from needs a test name (e.g. generic/634)}"
    if [ -f "$ORDER_FILE" ]; then
        mapfile -t FULL_LIST < "$ORDER_FILE"
        echo "Resume order: persisted from-zero expansion ($ORDER_FILE, ${#FULL_LIST[@]} tests)."
    else
        echo "WARNING: no persisted order file — expanding fresh (check -n order is stable)." >&2
        expand_full_list
    fi
    SLICE=()
    seen=""
    for t in "${FULL_LIST[@]}"; do
        [ "$t" = "$RESUME_AT" ] && seen=1
        [ -n "$seen" ] && SLICE+=("$t")
    done
    if [ "${#SLICE[@]}" -eq 0 ]; then
        echo "ERROR: $RESUME_AT is not in the expanded list — nothing to resume" >&2
        exit 1
    fi
    echo "=== RESUME PASS (NOT acceptance): ${#SLICE[@]} tests from $RESUME_AT to end ==="
    run_check_failfast "${SLICE[@]}" || EXIT_CODE=1
elif [ $# -gt 0 ]; then
    # Targeted fix-loop mode: classic one-shot check, verbatim args.
    echo "Running fstests with arguments: ${*}..."
    set +e
    ./check "${@}"
    EXIT_CODE=$?
    set -e
elif [ "${FSTESTS_QUICK:-0}" = "1" ]; then
    echo "Running fstests QUICK set (${#SQUEEZEFS_FSTESTS_QUICK[@]} tests, fail-fast)..."
    run_check_failfast "${SQUEEZEFS_FSTESTS_QUICK[@]}" || EXIT_CODE=1
else
    echo "Expanding -g auto and running the full suite fail-fast..."
    expand_full_list
    printf '%s\n' "${FULL_LIST[@]}" > "$ORDER_FILE"
    echo "Full list: ${#FULL_LIST[@]} tests (order persisted to $ORDER_FILE)."
    run_check_failfast "${FULL_LIST[@]}" || EXIT_CODE=1
fi

# 7. Cleanup: unmount and wait for the daemons to finish draining so a
#    back-to-back invocation starts from a quiet state.
echo "Cleaning up..."
umount "$TEST_DIR" "$SCRATCH_MNT" &>/dev/null || true
for _ in $(seq 1 300); do
    pgrep -f "squeezefs mount sqmeta://(${TEST_DEV}|${SCRATCH_DEV})" >/dev/null || break
    sleep 0.1
done

echo "=== fstests Completed (exit=$EXIT_CODE) ==="
exit $EXIT_CODE
