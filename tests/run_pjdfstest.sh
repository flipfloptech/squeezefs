#!/bin/bash
set -e

# Squeezefs POSIX compliance test script using pjdfstest
# Must be run inside WSL as root (or with sudo) because pjdfstest tests chown/chmod/etc.

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
mkdir -p "$MOUNT_DIR"
mkdir -p "$STAGING_DIR"

# Clean Garnet volume format
echo "Formatting volume..."
GARNET_URL="$GARNET_URL" cargo run --release -- format pjdfsvol --disk-cache-paths "$STAGING_DIR"

# Mount squeezefs
echo "Mounting squeezefs..."
# Start in background / daemon mode
GARNET_URL="$GARNET_URL" cargo run --release -- mount "$MOUNT_DIR" --daemon --disk-cache-paths "$STAGING_DIR" --log-file /tmp/squeezefs_pjdfs.log

# Ensure mounted
sleep 2
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
sudo prove -r tests/ || true

# 6. Cleanup
echo "Cleaning up..."
sudo umount "$MOUNT_DIR" || true
rm -rf "$MOUNT_DIR" "$STAGING_DIR"
echo "=== POSIX Verification Completed ==="
