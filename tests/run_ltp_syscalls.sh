#!/usr/bin/env bash
set -euo pipefail

# Squeezefs filesystem syscall tests runner using LTP.
# MUST be run as root.

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: This script must run as root (sudo $0)." >&2
    exit 1
fi

# Set a standard file descriptor limit so that tests expecting EMFILE do not run out of inodes first
ulimit -n 2048 || true

# Set a standard umask so that files and directories created by tests have proper group/world permissions
umask 0022

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MOUNT_DIR="${MOUNT_DIR:-/tmp/squeezefs_ltp_mount}"
STAGING_DIR="${STAGING_DIR:-/tmp/squeezefs_ltp_staging}"
RUNUSER="${SUDO_USER:-root}"

echo "=== Squeezefs LTP Filesystem Syscalls Verification ==="
echo "Repo: $REPO_DIR  Mount: $MOUNT_DIR"

# 1. Build Squeezefs release binary
cd "$REPO_DIR"
if [ "$RUNUSER" != "root" ] && id "$RUNUSER" &>/dev/null; then
    su -s /bin/bash "$RUNUSER" -c "cd '$REPO_DIR' && cargo build --release"
else
    cargo build --release
fi
SQUEEZEFS_BIN="$REPO_DIR/target/release/squeezefs"

# 2. Clone and build LTP kernel/syscalls if not already done
LTP_DIR="/tmp/ltp_build"
LTP_INSTALL_DIR="/tmp/ltp_install"

if [ ! -d "$LTP_DIR" ]; then
    echo "Cloning LTP..."
    git clone --depth 1 https://github.com/linux-test-project/ltp.git "$LTP_DIR"
fi

cd "$LTP_DIR"
if [ ! -f "config.status" ]; then
    echo "Configuring LTP..."
    make autotools
    ./configure --prefix="$LTP_INSTALL_DIR" --with-open-posix-testsuite
fi

echo "Compiling and installing LTP syscall test binaries..."
make -C testcases/kernel/syscalls -j$(nproc)
make -C testcases/kernel/syscalls install

# 3. Format and mount squeezefs volume (skip if USE_EXISTING_MOUNT is set)
if [ -z "${USE_EXISTING_MOUNT:-}" ]; then
    killall -9 squeezefs &>/dev/null || true
    sleep 1
    umount -l "$MOUNT_DIR" &>/dev/null || true
    rm -f /tmp/squeezefs_ltp.log || true
    mkdir -p "$MOUNT_DIR" "$STAGING_DIR"
    truncate -s 1G /dev/shm/squeezefs_ltp_meta || true
    truncate -s 1G /dev/shm/squeezefs_ltp_backend || true

    echo "Formatting squeezefs volume..."
    "$SQUEEZEFS_BIN" format \
        sqmeta:///dev/shm/squeezefs_ltp_meta \
        sqdata:///dev/shm/squeezefs_ltp_backend \
        --disk-cache-paths "$STAGING_DIR" \
        --force

    # Cache-path policy: staging dirs were declared at format above and are
    # read from the format config — mount rejects the flag.
    echo "Mounting squeezefs..."
    RUST_LOG=info "$SQUEEZEFS_BIN" mount \
        sqmeta:///dev/shm/squeezefs_ltp_meta \
        "$MOUNT_DIR" \
        --daemon \
        --disk-cache-size 500MB \
        --log-file /tmp/squeezefs_ltp.log \
        --allow-other

    sleep 3
    if ! mountpoint -q "$MOUNT_DIR"; then
        echo "ERROR: Failed to mount Squeezefs!" >&2
        cat /tmp/squeezefs_ltp.log 2>/dev/null || true
        exit 1
    fi
    chmod 1777 "$MOUNT_DIR"
    TARGET_DIR="$MOUNT_DIR"
else
    TARGET_DIR="$USE_EXISTING_MOUNT"
    chmod 1777 "$TARGET_DIR" || true
fi

# Define test scenarios
export PATH="/tmp/ltp_install/testcases/bin:$PATH"

if [ $# -gt 0 ]; then
    declare -a TESTS=("$@")
    declare -A CUSTOM_TESTS=()
else
    declare -a TESTS=(
        "access01" "access02" "access03" "access04"
        "chmod01" "chmod03" "chmod05" "chmod06" "chmod07" "chmod08" "chmod09"
        "chown01" "chown01_16" "chown02" "chown02_16" "chown03" "chown03_16" "chown04" "chown04_16" "chown05" "chown05_16"
        "fdatasync01" "fdatasync02" "fdatasync03"
        "fsync01" "fsync02" "fsync03" "fsync04"
        "link02" "link04" "link05" "link08" "linkat01" "linkat02"
        "mkdir02" "mkdir03" "mkdir04" "mkdir05" "mkdir09" "mkdirat01" "mkdirat02"
        "mmap01" "mmap02" "mmap03" "mmap04" "mmap05" "mmap06" "mmap08" "mmap09" "mmap12" "mmap13" "mmap14" "mmap15" "mmap16" "mmap17" "mmap18" "mmap19" "mmap20" "mmap22"
        "open01" "open02" "open03" "open04" "open06" "open07" "open08" "open09" "open10" "open11" "open12" "open13" "open14" "open15"
        "openat01" "openat02" "openat03" "openat04" "openat201" "openat202" "openat203"
        "open_by_handle_at01" "open_by_handle_at02" "open_tree01" "open_tree02"
        "read01" "read02" "read03" "read04" "readahead01" "readahead02" "readdir01" "readdir21"
        "readlink01" "readlink03" "readlinkat01" "readlinkat02" "readv01" "readv02"
        "rename01" "rename03" "rename04" "rename05" "rename06" "rename07" "rename08" "rename09" "rename10" "rename11" "rename12" "rename13" "rename14" "rename15"
        "renameat01" "renameat201" "rmdir01" "rmdir02" "rmdir03"
        "stat01" "stat01_64" "stat02" "stat02_64" "stat03" "stat03_64" "stat04" "stat04_64"
        "statmount01" "statmount02" "statmount03" "statmount04" "statmount05" "statmount06" "statmount07" "statmount08" "statmount09"
        "statfs01" "statfs01_64" "statfs02" "statfs02_64" "statfs03" "statfs03_64" "statvfs01" "statvfs02"
        "symlink02" "symlink03" "symlink04" "symlinkat01"
        "truncate02" "truncate02_64" "truncate03" "truncate03_64"
        "unlink05" "unlink07" "unlink08" "unlink09" "unlink10" "unlinkat01"
        "write01" "write02" "write03" "write04" "write05" "write06" "writev01" "writev02" "writev03" "writev05" "writev06" "writev07"
        "statx01" "statx02" "statx03" "statx04" "statx05" "statx06" "statx07" "statx08" "statx09" "statx10" "statx11" "statx12"
    )

    # Custom test runners requiring specific parameters
    declare -A CUSTOM_TESTS=(
        ["mmap21_01"]="/tmp/ltp_install/testcases/bin/mmap21 -m 1"
        ["mmap21_02"]="/tmp/ltp_install/testcases/bin/mmap21"
        ["renameat202"]="/tmp/ltp_install/testcases/bin/renameat202 -i 10"
    )
fi

PASS=0
FAIL=0
BROK=0
CONF=0
# FAIL FAST (standing user rule, 2026-07-22): the first unexpected
# result (FAILED or BROKEN — TCONF skips are the suite's own expected
# environment class) aborts the run immediately; fix red-first, then
# restart the whole run (counted-restart discipline).
ABORT=""

fail_fast() {
    echo "==================================================================" >&2
    echo "FAIL FAST: $1 — aborting the LTP run (fix red-first, restart)" >&2
    echo "==================================================================" >&2
    ABORT=1
}

echo "Running LTP filesystem tests on squeezefs mount (fail-fast)..."
for test in "${TESTS[@]}"; do
    binary="/tmp/ltp_install/testcases/bin/$test"
    if [ -x "$binary" ]; then
        echo "--------------------------------------------------"
        echo "Running: $test"
        if env TMPDIR="$TARGET_DIR" "$binary"; then
            PASS=$((PASS+1))
        else
            ret=$?
            # LTP returns 32 for TCONF (skipped) and others for failure/broken
            if [ $ret -eq 32 ]; then
                CONF=$((CONF+1))
                echo "$test: SKIPPED (TCONF)"
            elif [ $ret -eq 2 ]; then
                BROK=$((BROK+1))
                echo "$test: BROKEN (TBROK)"
                fail_fast "$test BROKEN (exit $ret)"
            else
                FAIL=$((FAIL+1))
                echo "$test: FAILED"
                fail_fast "$test FAILED (exit $ret)"
            fi
        fi
    else
        echo "Warning: Test binary not found: $test"
    fi
    if [ -n "$ABORT" ]; then break; fi
done

for name in "${!CUSTOM_TESTS[@]}"; do
    if [ -n "$ABORT" ]; then break; fi
    cmd=${CUSTOM_TESTS[$name]}
    echo "--------------------------------------------------"
    echo "Running custom test: $name ($cmd)"
    if env TMPDIR="$TARGET_DIR" $cmd; then
        PASS=$((PASS+1))
    else
        ret=$?
        if [ $ret -eq 32 ]; then
            CONF=$((CONF+1))
            echo "$name: SKIPPED (TCONF)"
        elif [ $ret -eq 2 ]; then
            BROK=$((BROK+1))
            echo "$name: BROKEN (TBROK)"
            fail_fast "$name BROKEN (exit $ret)"
        else
            FAIL=$((FAIL+1))
            echo "$name: FAILED"
            fail_fast "$name FAILED (exit $ret)"
        fi
    fi
done

echo "=================================================="
echo "LTP Filesystem Syscalls Test Results Summary:"
echo "--------------------------------------------------"
echo "PASS:    $PASS"
echo "FAIL:    $FAIL"
echo "BROKEN:  $BROK"
echo "SKIPPED: $CONF"
echo "=================================================="

# Cleanup
if [ -z "${USE_EXISTING_MOUNT:-}" ]; then
    echo "Cleaning up..."
    cd "$REPO_DIR"
    # 1. Unmount any active submounts inside MOUNT_DIR
    for submount in $(findmnt -n -o TARGET -R "$MOUNT_DIR" 2>/dev/null | grep -v "^$MOUNT_DIR$" | sort -r); do
        echo "Unmounting sub-mount: $submount"
        umount "$submount" || umount -l "$submount" || true
    done
    # 2. Detach loop devices backed by the squeezefs mount
    for loop_dev in $(losetup -a 2>/dev/null | grep "$MOUNT_DIR" | cut -d: -f1); do
        echo "Detaching loop device: $loop_dev"
        losetup -d "$loop_dev" || true
    done
    # 3. Unmount mountpoint
    umount "$MOUNT_DIR" || umount -l "$MOUNT_DIR" || true
    # 3. Terminate FUSE daemon
    killall -9 squeezefs &>/dev/null || true
    sleep 1
    # 4. Remove directories and backing storage files
    rm -rf "$MOUNT_DIR" "$STAGING_DIR"
    rm -f /dev/shm/squeezefs_ltp_meta /dev/shm/squeezefs_ltp_backend
fi

if [ $FAIL -gt 0 ] || [ $BROK -gt 0 ]; then
    echo "=== LTP Syscalls Verification Failed ==="
    exit 1
fi

echo "=== LTP Syscalls Verification Completed Successfully ==="
exit 0
