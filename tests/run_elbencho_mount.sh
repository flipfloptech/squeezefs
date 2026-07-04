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
if ! command -v elbencho &> /dev/null; then
    echo "ERROR: elbencho is not installed. Please install it first."
    exit 1
fi

# 1. Format the SqueezeFS volume
echo "Formatting SqueezeFS volume..."
./target/release/squeezefs format squeeze://127.0.0.1:6379/squeezefs-vol --force --capacity 100G --inodes 1000000 --volume /dev/shm/squeezefs_elbencho_backend

# 2. Mount SqueezeFS as a daemon
# Note: We configure the cache-dir explicitly to /tmp/squeezefs_staging
echo "Mounting SqueezeFS at $MOUNT_DIR..."
./target/release/squeezefs mount squeeze://127.0.0.1:6379/squeezefs-vol "$MOUNT_DIR" --daemon --cache-dir "$CACHE_DIR" --cache-size 10G

# 3. Wait for the FUSE mount to be ready
echo "Waiting for mount to become ready..."
until mountpoint -q "$MOUNT_DIR"; do
    sleep 0.2
done
echo "SqueezeFS is mounted and ready."

# 4. Run elbencho write and read tests
echo "Running elbencho tests..."
elbencho -w -r -t 4 -s 1G -b 4M "$MOUNT_DIR/file"

# 5. Clean up mount
echo "Unmounting SqueezeFS..."
sudo umount -l "$MOUNT_DIR"
rm -rf "$MOUNT_DIR"
rm -rf "$CACHE_DIR"

echo "Benchmark execution completed successfully!"
