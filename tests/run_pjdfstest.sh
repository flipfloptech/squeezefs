#!/bin/bash
set -euo pipefail

# Squeezefs POSIX compliance test script using pjdfstest.
#
# MUST be run as root (or via sudo). The vast majority of pjdfstest cases use
# setuid (-u/-g), chown, mknod block/char, and permission-denial checks that
# silently cascade-fail when the harness is not root (chown → EPERM, -u → empty
# result). That looks like a "mass FS failure" but is an environment issue.
#
# Usage:
#   sudo ./tests/run_pjdfstest.sh

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: pjdfstest must run as root (sudo $0)." >&2
    echo "Non-root runs fail chown/setuid cases and cascade into thousands of false failures." >&2
    exit 1
fi

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MOUNT_DIR="${MOUNT_DIR:-/tmp/squeezefs_pjdfs_mount}"
STAGING_DIR="${STAGING_DIR:-/tmp/squeezefs_pjdfs_staging}"
# Run as the original invoking user for cargo when under sudo, if available.
RUNUSER="${SUDO_USER:-root}"

echo "=== Squeezefs POSIX Compliance Verification (root required) ==="
echo "Repo: $REPO_DIR  Mount: $MOUNT_DIR"

# 1. Check/Install dependencies
for cmd in make gcc autoreconf; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "Installing missing dependency: $cmd..."
        apt-get update && apt-get install -y build-essential autoconf
    fi
done
PROVE_CMD=""
for p in prove /usr/bin/prove /usr/bin/core_perl/prove; do
    if command -v "$p" &>/dev/null || [ -x "$p" ]; then
        PROVE_CMD=$(command -v "$p" 2>/dev/null || echo "$p")
        break
    fi
done
if [ -z "$PROVE_CMD" ] || [ ! -x "$PROVE_CMD" ]; then
    echo "Installing prove (Test::Harness)..."
    apt-get update && apt-get install -y libtest-harness-perl
    PROVE_CMD=$(command -v prove || echo /usr/bin/core_perl/prove)
fi

# 2. Build Squeezefs release binary (prefer non-root cargo home when possible)
cd "$REPO_DIR"
if [ "$RUNUSER" != "root" ] && id "$RUNUSER" &>/dev/null; then
    su -s /bin/bash "$RUNUSER" -c "cd '$REPO_DIR' && cargo build --release"
else
    cargo build --release
fi
SQUEEZEFS_BIN="$REPO_DIR/target/release/squeezefs"
if [ ! -x "$SQUEEZEFS_BIN" ]; then
    echo "ERROR: missing $SQUEEZEFS_BIN" >&2
    exit 1
fi

# 3. Clone and compile pjdfstest
PJDFSTEST_DIR="${PJDFSTEST_DIR:-/tmp/pjdfstest}"
if [ ! -d "$PJDFSTEST_DIR" ]; then
    echo "Cloning pjdfstest..."
    git clone https://github.com/pjd/pjdfstest.git "$PJDFSTEST_DIR"
fi

cd "$PJDFSTEST_DIR"
if [ ! -f "pjdfstest" ]; then
    echo "Compiling pjdfstest..."
    autoreconf -ifs
    ./configure
    make pjdfstest
fi

# 4. Prepare mounts
killall -9 squeezefs &>/dev/null || true
sleep 1
umount -l "$MOUNT_DIR" &>/dev/null || true
mkdir -p "$MOUNT_DIR" "$STAGING_DIR"
truncate -s 128M /dev/shm/squeezefs_pjdfs_meta || true
truncate -s 1G /dev/shm/squeezefs_default_backend || true

# Clean volume format
echo "Formatting volume..."
cd "$REPO_DIR"
"$SQUEEZEFS_BIN" format \
    sqmeta:///dev/shm/squeezefs_pjdfs_meta \
    sqdata:///dev/shm/squeezefs_default_backend \
    --disk-cache-paths "$STAGING_DIR" \
    --force

# Mount squeezefs (daemon must be root for chown/mknod tests; --allow-other for harness)
echo "Mounting squeezefs..."
SQUEEZEFS_TIMEOUT=15 RUST_LOG=debug "$SQUEEZEFS_BIN" mount \
    sqmeta:///dev/shm/squeezefs_pjdfs_meta \
    "$MOUNT_DIR" \
    --daemon \
    --disk-cache-paths "$STAGING_DIR" \
    --log-file /tmp/squeezefs_pjdfs.log \
    --allow-other

# Ensure mounted
sleep 3
if ! mountpoint -q "$MOUNT_DIR"; then
    echo "ERROR: Failed to mount Squeezefs!" >&2
    cat /tmp/squeezefs_pjdfs.log 2>/dev/null || true
    exit 1
fi

# 5. Run tests
TESTS_PATH="${1:-$PJDFSTEST_DIR/tests}"
echo "Running pjdfstest suite (as root on $MOUNT_DIR targeting $TESTS_PATH)..."
cd "$MOUNT_DIR"
if ! "$PROVE_CMD" -r "$TESTS_PATH"; then
    echo "=== POSIX Verification Failed (prove exit=1) ==="
    exit 1
fi

# 6. Cleanup
echo "Cleaning up..."
cd "$REPO_DIR"
umount "$MOUNT_DIR" || umount -l "$MOUNT_DIR" || true
rm -rf "$MOUNT_DIR" "$STAGING_DIR"
rm -f /dev/shm/squeezefs_pjdfs_meta /dev/shm/squeezefs_default_backend
echo "=== POSIX Verification Completed (prove exit=0) ==="
exit 0
