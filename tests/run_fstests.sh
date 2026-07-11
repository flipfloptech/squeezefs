#!/bin/bash
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
# files are sparse on tmpfs.
META_SIZE="${SQUEEZEFS_FSTESTS_META_SIZE:-1G}"
DATA_SIZE="${SQUEEZEFS_FSTESTS_DATA_SIZE:-8G}"

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
    su -s /bin/bash "$RUNUSER" -c "cd '$REPO_DIR' && cargo build --release"
else
    cargo build --release
fi
SQUEEZEFS_BIN="$REPO_DIR/target/release/squeezefs"

# 3. Clone and compile xfstests-dev if not already done
XFSTESTS_DIR="/tmp/xfstests-dev"
if [ ! -d "$XFSTESTS_DIR" ]; then
    echo "Cloning xfstests-dev..."
    git clone --depth 1 https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git "$XFSTESTS_DIR"
fi

cd "$XFSTESTS_DIR"
if [ ! -f "src/open_by_handle" ]; then
    echo "Compiling xfstests..."
    # xfstests requires creating fsgqa user/group if they do not exist
    if ! getent group fsgqa >/dev/null; then
        groupadd fsgqa
    fi
    if ! getent passwd fsgqa >/dev/null; then
        useradd -g fsgqa fsgqa
    fi
    make
fi

# 4. Install mount and mkfs helpers
echo "Installing FUSE helpers in /sbin..."

cat << EOF > /sbin/mount.fuse.squeezefs
#!/bin/bash
# mount(8) helper for -t fuse.squeezefs. Contract: silent on success (stdout
# leaks into xfstests golden output), non-zero + stderr on failure, and the
# mount is usable when we return.
set -u
SQUEEZEFS_BIN="$SQUEEZEFS_BIN"
EOF
cat << 'EOF' >> /sbin/mount.fuse.squeezefs
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

ALL_OPTS="fsname=$DEV"
if [ -n "$OPTS" ]; then
    ALL_OPTS="$ALL_OPTS,$OPTS"
fi

TAG="$(basename "$MNT")"
LOG="/tmp/squeezefs_fstests_${TAG}.log"

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
chmod +x /sbin/mount.fuse.squeezefs

cat << EOF > /sbin/mkfs.fuse.squeezefs
#!/bin/bash
set -u
SQUEEZEFS_BIN="$SQUEEZEFS_BIN"
META_SIZE="$META_SIZE"
DATA_SIZE="$DATA_SIZE"
EOF
cat << 'EOF' >> /sbin/mkfs.fuse.squeezefs
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

# Format
exec "$SQUEEZEFS_BIN" format \
    "sqmeta://$DEV" \
    "sqdata://$DATA_DEV" \
    --disk-cache-paths "$STAGING_DIR" \
    --force
EOF
chmod +x /sbin/mkfs.fuse.squeezefs

# UMOUNT_PROG wrapper: a freshly armed FUSE-over-io_uring mount holds a
# kernel-side reference for up to ~100ms after mount(8) returns, so the
# zero-dwell umount xfstests issues in cycle-mount paths fails EBUSY with no
# userspace holder (deterministic in e.g. generic/003). Retry briefly; a real
# leak still fails after the 5s budget. Silent on eventual success so no
# noise reaches golden output.
UMOUNT_REAL="$(type -P umount)"
cat << EOF > /sbin/umount.squeezefs-fstests
#!/bin/bash
UMOUNT_REAL="$UMOUNT_REAL"
EOF
cat << 'EOF' >> /sbin/umount.squeezefs-fstests
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
chmod +x /sbin/umount.squeezefs-fstests

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
export UMOUNT_PROG=/sbin/umount.squeezefs-fstests
EOF

# Pre-format TEST_DEV and SCRATCH_DEV once since xfstests doesn't mkfs them for FUSE
/sbin/mkfs.fuse.squeezefs "$TEST_DEV"
/sbin/mkfs.fuse.squeezefs "$SCRATCH_DEV"

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
#   PASS (deterministic): 001 008 013 069 074 075 091 112 127 263 285 469
#                     616 617 618
#   NOTRUN (deterministic, platform): 009 316 — both _require xfs_io fiemap;
#                     FUSE has no FIEMAP ioctl. Kept as canaries: they start
#                     RUNNING (and their punch/prealloc coverage arms) the
#                     day a FIEMAP-capable kernel/fuse lands.
#   FAIL (deterministic, platform-class — expected-fail, kept as canaries):
#     003 = atime semantics: FUSE attr caching + writeback-cache mount keep
#           relatime/strictatime updates kernel-side and remount-unstable
#           (atime not updated on read; ctime jitter across remount). Not a
#           data-path bug; revisit only if we adopt FOPEN_KEEP/attr-timeout
#           rework. A DIFFERENT diff than the 10 known atime/ctime ERROR
#           lines = regression.
#     213 = thin provisioning: fallocate(mode=0) never reserves physical
#           blocks (sparse/dynamic backend by design; statfs is virtual),
#           so the "fallocate: No space left on device" golden line never
#           appears. All other 213 legs pass; exactly that one missing
#           line is the expected diff.
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
SQUEEZEFS_FSTESTS_QUICK=(
    generic/001 generic/003 generic/008 generic/009 generic/013
    generic/069 generic/074 generic/075 generic/091 generic/112
    generic/127 generic/213 generic/263 generic/285 generic/316
    generic/469 generic/616 generic/617 generic/618
)

if [ $# -gt 0 ]; then
    TEST_ARGS=("${@}")
elif [ "${FSTESTS_QUICK:-0}" = "1" ]; then
    TEST_ARGS=("${SQUEEZEFS_FSTESTS_QUICK[@]}")
else
    TEST_ARGS=("-g" "auto")
fi
echo "Running fstests with arguments: ${TEST_ARGS[*]}..."
cd "$XFSTESTS_DIR"

# We ignore non-zero exit code of check script for our cleanup
set +e
./check "${TEST_ARGS[@]}"
EXIT_CODE=$?
set -e

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
