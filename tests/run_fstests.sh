#!/bin/bash
set -euo pipefail

# Squeezefs integration script for fstests (xfstests).
# MUST be run as root.

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: This script must run as root (sudo $0)." >&2
    exit 1
fi

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUNUSER="${SUDO_USER:-root}"
# Ensure mount points are clean from any previous runs
umount -l /mnt/squeezefs_test /mnt/squeezefs_scratch &>/dev/null || true
rm -rf /mnt/squeezefs_test/.* /mnt/squeezefs_test/* /mnt/squeezefs_scratch/.* /mnt/squeezefs_scratch/* &>/dev/null || true

echo "=== Squeezefs fstests Integration ==="
echo "Repo: $REPO_DIR"

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

cat << 'EOF' > /sbin/mount.fuse.squeezefs
#!/bin/bash
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

SQUEEZEFS_BIN="/home/justin/Source/squeezefs/target/release/squeezefs"

if [[ "$MNT" == *"test"* ]]; then
    STAGING_DIR="/tmp/squeezefs_fstests_staging_test"
else
    STAGING_DIR="/tmp/squeezefs_fstests_staging_scratch"
fi
mkdir -p "$STAGING_DIR"

# Run mount daemon
exec "$SQUEEZEFS_BIN" mount \
    "sqmeta://$DEV" \
    "$MNT" \
    --daemon \
    --disk-cache-paths "$STAGING_DIR" \
    --disk-cache-size 500MB \
    --allow-other \
    -o "$ALL_OPTS" \
    --log-file "/tmp/squeezefs_fstests_$(basename "$MNT").log"
EOF
chmod +x /sbin/mount.fuse.squeezefs

cat << 'EOF' > /sbin/mkfs.fuse.squeezefs
#!/bin/bash
DEV="$1"
DATA_DEV="${DEV/_meta/_data}"

if [ ! -b "$DEV" ]; then
    truncate -s 128M "$DEV"
    truncate -s 1G "$DATA_DEV"
fi

SQUEEZEFS_BIN="/home/justin/Source/squeezefs/target/release/squeezefs"

# Format
exec "$SQUEEZEFS_BIN" format \
    "sqmeta://$DEV" \
    "sqdata://$DATA_DEV" \
    --force
EOF
chmod +x /sbin/mkfs.fuse.squeezefs

# 5. Create local.config
echo "Configuring xfstests local.config..."
cat << EOF > "$XFSTESTS_DIR/local.config"
export FSTYP=fuse
export FUSE_SUBTYP=.squeezefs

export TEST_DIR=${TEST_DIR:-/mnt/squeezefs_test}
export SCRATCH_MNT=${SCRATCH_MNT:-/mnt/squeezefs_scratch}

export TEST_DEV=${TEST_DEV:-/dev/shm/squeezefs_fstests_test_meta}
export SCRATCH_DEV=${SCRATCH_DEV:-/dev/shm/squeezefs_fstests_scratch_meta}
EOF

# Ensure mount points exist
mkdir -p "${TEST_DIR:-/mnt/squeezefs_test}" "${SCRATCH_MNT:-/mnt/squeezefs_scratch}"

# Pre-format TEST_DEV and SCRATCH_DEV once since xfstests doesn't mkfs them for FUSE
/sbin/mkfs.fuse.squeezefs "${TEST_DEV:-/dev/shm/squeezefs_fstests_test_meta}"
/sbin/mkfs.fuse.squeezefs "${SCRATCH_DEV:-/dev/shm/squeezefs_fstests_scratch_meta}"

# 6. Run fstests
if [ $# -eq 0 ]; then
    TEST_ARGS=("-g" "auto")
else
    TEST_ARGS=("${@}")
fi
echo "Running fstests with arguments: ${TEST_ARGS[*]}..."
cd "$XFSTESTS_DIR"

# We ignore non-zero exit code of check script for our cleanup
set +e
./check "${TEST_ARGS[@]}"
EXIT_CODE=$?
set -e

# 7. Cleanup
echo "Cleaning up..."
umount /mnt/squeezefs_test /mnt/squeezefs_scratch &>/dev/null || true
# rm -f /sbin/mount.fuse.squeezefs /sbin/mkfs.fuse.squeezefs
# rm -f /dev/shm/squeezefs_fstests_*

echo "=== fstests Completed (exit=$EXIT_CODE) ==="
exit $EXIT_CODE
