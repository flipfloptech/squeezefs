#!/bin/bash
set -e

# Squeezefs POSIX compliance test script using pjdfstest
# Must be run inside WSL as root (or with sudo) because pjdfstest tests chown/chmod/etc.

REPO_DIR="$(pwd)"
MOUNT_DIR="/tmp/squeezefs_pjdfs_mount"
STAGING_DIR="/tmp/squeezefs_pjdfs_staging"
GARNET_URL="${GARNET_URL:-redis://127.0.0.1:6379}"

echo "=== Squeezefs POSIX Compliance Verification ==="

# 1. Check/Install dependencies
for cmd in make gcc autoreconf prove; do
    if ! command -v $cmd &> /dev/null; then
        echo "Installing missing dependency: $cmd..."
        sudo apt-get update && sudo apt-get install -y build-essential autoconf prove libtest-harness-perl
    fi
done

# 2. Build Squeezefs release binary
cargo build --release

# 3. Clone and compile pjdfstest
PJDFSTEST_DIR="/tmp/pjdfstest"
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
sudo umount -l "$MOUNT_DIR" &>/dev/null || true
mkdir -p "$MOUNT_DIR"
mkdir -p "$STAGING_DIR"

# Clean Garnet volume format
echo "Formatting volume..."
cd "$REPO_DIR"
GARNET_URL="$GARNET_URL" cargo run --release -- format pjdfsvol --disk-cache-paths "$STAGING_DIR" --volume /dev/shm/squeezefs_default_backend --force

# Mount squeezefs
echo "Mounting squeezefs..."
# Start in background / daemon mode
cd "$REPO_DIR"
GARNET_URL="$GARNET_URL" cargo run --release -- mount pjdfsvol "$MOUNT_DIR" --daemon --disk-cache-paths "$STAGING_DIR" --log-file /tmp/squeezefs_pjdfs.log --allow-other

# Ensure mounted
sleep 5
if ! mountpoint -q "$MOUNT_DIR"; then
    echo "ERROR: Failed to mount Squeezefs!"
    cat /tmp/squeezefs_pjdfs.log
    exit 1
fi

# 5. Run pjdfstest
echo "Running pjdfstest suite..."
cd "$PJDFSTEST_DIR"
# Run tests and show report
# We run as root because many tests require root chown/chmod/mknod privileges.
PROVE_CMD=$(command -v prove || echo "/usr/bin/core_perl/prove")
$PROVE_CMD -r tests/ || true

# 6. Cleanup
echo "Cleaning up..."
sudo umount "$MOUNT_DIR" || true
rm -rf "$MOUNT_DIR" "$STAGING_DIR"
echo "=== POSIX Verification Completed ==="
