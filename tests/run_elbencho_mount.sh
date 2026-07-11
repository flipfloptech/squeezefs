#!/bin/bash
# SqueezeFS: elbencho Integration Benchmark Script
#
# This script formats and mounts squeezefs, runs elbencho against it,
# and cleans up the mount cleanly.
#
# Usage:
#   chmod +x tests/run_elbencho_mount.sh
#   ./tests/run_elbencho_mount.sh
#

set -e

MOUNT_DIR="/tmp/squeezefs_mount"
CACHE_DIR="/tmp/squeezefs_staging"

# Clean up any stale mounts
echo "Cleaning up any stale mounts or directories..."
sudo umount -l "$MOUNT_DIR" 2>/dev/null || true
rm -rf "$MOUNT_DIR"
rm -rf "$CACHE_DIR"

mkdir -p "$MOUNT_DIR"
mkdir -p "$CACHE_DIR"

# Ensure we have a compiled squeezefs binary
if [ ! -f "./target/release/squeezefs" ]; then
    echo "Squeezefs binary not found. Compiling..."
    cargo build --release
fi

# Check if elbencho is installed
ELBENCHO_CMD="elbencho"
if ! command -v elbencho &> /dev/null; then
    if [ -x "/home/justin/.local/bin/elbencho" ]; then
        ELBENCHO_CMD="/home/justin/.local/bin/elbencho"
    else
        echo "ERROR: elbencho is not installed. Please install it first."
        exit 1
    fi
fi

# 1. Format the SqueezeFS volume
echo "Formatting SqueezeFS volume..."
truncate -s 128M /dev/shm/squeezefs_elbencho_meta || true
truncate -s 2G /dev/shm/squeezefs_elbencho_backend || true
./target/release/squeezefs format \
    sqmeta:///dev/shm/squeezefs_elbencho_meta \
    sqdata:///dev/shm/squeezefs_elbencho_backend \
    --disk-cache-paths "$CACHE_DIR" \
    --force

# 2. Mount SqueezeFS as a daemon (cache paths come from the format config;
#    mount rejects the flag — cache-path policy)
echo "Mounting SqueezeFS at $MOUNT_DIR..."
./target/release/squeezefs mount \
    sqmeta:///dev/shm/squeezefs_elbencho_meta \
    "$MOUNT_DIR" \
    --daemon \
    --allow-other

# 3. Wait for the FUSE mount to be ready
echo "Waiting for mount to become ready..."
until mountpoint -q "$MOUNT_DIR"; do
    sleep 0.2
done
echo "SqueezeFS is mounted and ready."

# 4. Run elbencho write and read tests
echo "Running elbencho tests..."
"$ELBENCHO_CMD" -w -r -t 4 -s 1G -b 4M "$MOUNT_DIR/file"

# 5. Clean up mount
echo "Unmounting SqueezeFS..."
sudo umount -l "$MOUNT_DIR" || true
rm -rf "$MOUNT_DIR"
rm -rf "$CACHE_DIR"
rm -f /dev/shm/squeezefs_elbencho_meta /dev/shm/squeezefs_elbencho_backend

echo "Benchmark execution completed successfully!"
