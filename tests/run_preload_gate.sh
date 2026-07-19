#!/bin/bash
set -euo pipefail

# L4-5 preload-shim gate (docs/design-preload-interception.md §5.4, PR
# L4-5; the G-L4-4 transparency matrix starts here).
#
# Two legs:
#
#   Leg 1 (always, no root): the sanctioned cdylib build
#     (--profile preload-release --features interposers), the crate's own
#     clippy/fmt/tests, the Issue-4 wrong-profile guard proof, and the
#     plain-file LD_PRELOAD passthrough battery (cp/dd/cat/tar byte
#     parity + exit codes — the shim on foreign filesystems must be
#     invisible).
#
#   Leg 2 (root only): format + mount with --interception, then the
#     bound-fd battery ON the mount: cp/dd parity, the §3 rule-4
#     ENGAGEMENT proof (.stats ipc_ops_* must move — silent passthrough
#     published as interception is the exact fraud the charter forbids),
#     dup transparency, close_range-then-socket() fd reuse, the
#     lseek-SEEK_CUR-is-FUSE-free pin, and fio/elbencho when installed.
#
# Usage:
#   tests/run_preload_gate.sh          # leg 1 only (unprivileged)
#   sudo tests/run_preload_gate.sh     # both legs

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUNUSER="${SUDO_USER:-$(id -un)}"
MOUNT_DIR="${MOUNT_DIR:-/tmp/squeezefs_il_mount}"
STAGING_DIR="${STAGING_DIR:-/tmp/squeezefs_il_staging}"
LOG=/tmp/squeezefs_il_gate.log
SO="$REPO_DIR/target/preload-release/libsqueezefs_il.so"

cd "$REPO_DIR"

run_as_user() {
    if [ "$(id -u)" -eq 0 ] && [ "$RUNUSER" != "root" ] && id "$RUNUSER" &>/dev/null; then
        su -s /bin/bash "$RUNUSER" -c "cd '$REPO_DIR' && $*"
    else
        bash -c "cd '$REPO_DIR' && $*"
    fi
}

fail() { echo "GATE FAIL: $*" >&2; exit 1; }

echo "=== Leg 1: build + crate gate + passthrough transparency ==="

# 1a. The sanctioned build; clippy/fmt/tests under the same profile.
run_as_user "cargo build -p squeezefs-preload --profile preload-release --features interposers"
run_as_user "cargo clippy -p squeezefs-preload --profile preload-release --features interposers --all-targets -- -D warnings"
run_as_user "cargo fmt -p squeezefs-preload --check"
run_as_user "cargo test -p squeezefs-preload --profile preload-release --features interposers"
[ -f "$SO" ] || fail "cdylib missing at $SO"

# 1b. Issue-4 guard proof: a plain --release build MUST refuse (the root
# profile's panic=abort would void catch_unwind → host-app aborts).
if run_as_user "cargo build -p squeezefs-preload --release --features interposers" 2>/dev/null; then
    fail "wrong-profile build (--release, panic=abort) must be refused by the compile_error! guard"
fi
echo "OK: wrong-profile build refused (Issue-4 guard live)"

# 1c. Plain-file passthrough battery: the shim on a foreign filesystem
# must be byte- and exit-code-invisible.
T="$(mktemp -d /tmp/il_gate_plain.XXXXXX)"
trap 'rm -rf "$T"' EXIT
head -c 4M /dev/urandom > "$T/src.bin"

LD_PRELOAD="$SO" cp "$T/src.bin" "$T/cp.bin"
cmp "$T/src.bin" "$T/cp.bin" || fail "cp parity under LD_PRELOAD (plain fs)"

LD_PRELOAD="$SO" dd if="$T/src.bin" of="$T/dd.bin" bs=7k status=none
cmp "$T/src.bin" "$T/dd.bin" || fail "dd parity under LD_PRELOAD (plain fs)"

(cd "$T" && LD_PRELOAD="$SO" tar cf t.tar src.bin && LD_PRELOAD="$SO" tar xf t.tar -O > untar.bin)
cmp "$T/src.bin" "$T/untar.bin" || fail "tar roundtrip under LD_PRELOAD (plain fs)"

LD_PRELOAD="$SO" bash -c 'set -e; echo x | grep -q x; ls / >/dev/null; true' || fail "shell battery under LD_PRELOAD"
echo "OK: plain-file passthrough battery"

if [ "$(id -u)" -ne 0 ]; then
    echo "=== Leg 2 skipped (not root) — run: sudo $0 ==="
    echo "PRELOAD GATE (leg 1) PASSED"
    exit 0
fi

echo "=== Leg 2: interception mount battery ==="

# Dev-box identity posture: a dirty tree stamps a `-dirty` build commit,
# which is a DEGENERATE KD-7 identity — both ends refuse to establish
# unless the counted dev override is set (the first sudo run of this
# gate proved that refusal fires for real). The override still requires
# commit-string EQUALITY: a shim and daemon built from different tree
# states refuse regardless, which is exactly what a gate wants.
export SQUEEZEFS_IPC_ALLOW_DEV=1
echo "note: SQUEEZEFS_IPC_ALLOW_DEV=1 (dev-tree identities; counted in ipc_binds_dev_override)"

# 2a. Build the daemon and mount with --interception.
run_as_user "cargo build --release"
SQUEEZEFS_BIN="$REPO_DIR/target/release/squeezefs"

killall -9 squeezefs &>/dev/null || true
sleep 1
umount -l "$MOUNT_DIR" &>/dev/null || true
rm -f "$LOG"
mkdir -p "$MOUNT_DIR" "$STAGING_DIR"
truncate -s 1G /dev/shm/squeezefs_il_meta
truncate -s 1G /dev/shm/squeezefs_il_backend

"$SQUEEZEFS_BIN" format \
    sqmeta:///dev/shm/squeezefs_il_meta \
    sqdata:///dev/shm/squeezefs_il_backend \
    --disk-cache-paths "$STAGING_DIR" \
    --force

RUST_LOG=info "$SQUEEZEFS_BIN" mount \
    sqmeta:///dev/shm/squeezefs_il_meta \
    "$MOUNT_DIR" \
    --daemon \
    --interception \
    --disk-cache-size 500MB \
    --log-file "$LOG" \
    --allow-other

sleep 3
mountpoint -q "$MOUNT_DIR" || { cat "$LOG" || true; fail "mount failed"; }
chmod 1777 "$MOUNT_DIR"

cleanup_mount() {
    umount "$MOUNT_DIR" 2>/dev/null || umount -l "$MOUNT_DIR" 2>/dev/null || true
    rm -f /dev/shm/squeezefs_il_meta /dev/shm/squeezefs_il_backend
    rm -rf "$T" "$STAGING_DIR"
}
trap cleanup_mount EXIT

stats() { # stats <key>
    python3 -c "import json,sys; print(json.load(open('$MOUNT_DIR/.stats'))['metrics'].get('$1', 0))"
}

# The shim runs as the invoking (sudo) user's processes here — root is
# fine: --allow-other admits any uid, and root's opens pass the screen.
ILP() { LD_PRELOAD="$SO" "$@"; }

# Ring writes grow files invisibly to the KERNEL's attr cache (size TTL
# 1 s): an unbound reader (cmp here) inside that window sees the stale
# size — the documented §5.6.2 W1-class bound. PR L4-6's
# notify_inval_inode mitigation shrinks it; until then the parity rows
# wait out the TTL. Reads THROUGH the shim (ring reads) never see the
# window — daemon state is the authority — which the dd read-back row
# proves by running with no settle at all.
ATTR_TTL_SETTLE=1.2
settle() { sleep "$ATTR_TTL_SETTLE"; }

# 2b. cp/dd parity ON the mount + the §3 rule-4 engagement proof.
OPS_R0=$(stats ipc_ops_read); OPS_W0=$(stats ipc_ops_write); SESS0=$(stats ipc_sessions_total)

ILP cp "$T/src.bin" "$MOUNT_DIR/cp.bin"
ILP dd if="$MOUNT_DIR/cp.bin" of="$T/back.bin" bs=64k status=none
cmp "$T/src.bin" "$T/back.bin" || fail "ring read-back parity (no settle: daemon state is the authority)"
settle
cmp "$T/src.bin" "$MOUNT_DIR/cp.bin" || fail "cp parity onto the mount (kernel reader, post-TTL)"
ILP dd if="$T/src.bin" of="$MOUNT_DIR/dd.bin" bs=1M oflag=direct status=none 2>/dev/null \
    || ILP dd if="$T/src.bin" of="$MOUNT_DIR/dd.bin" bs=1M status=none
settle
cmp "$T/src.bin" "$MOUNT_DIR/dd.bin" || fail "dd write parity onto the mount (kernel reader, post-TTL)"

OPS_R1=$(stats ipc_ops_read); OPS_W1=$(stats ipc_ops_write); SESS1=$(stats ipc_sessions_total)
[ "$SESS1" -gt "$SESS0" ] || fail "no IPC session was ever established (engagement, §3 rule 4)"
[ "$OPS_W1" -gt "$OPS_W0" ] || fail "ipc_ops_write did not move — writes ran over kernel FUSE (engagement)"
[ "$OPS_R1" -gt "$OPS_R0" ] || fail "ipc_ops_read did not move — reads ran over kernel FUSE (engagement)"
echo "OK: mount parity + engagement (sessions +$((SESS1-SESS0)), reads +$((OPS_R1-OPS_R0)), writes +$((OPS_W1-OPS_W0)))"

# 2c. dup transparency (G-L4-4): dup, close the original, the dup keeps
# serving over the ring.
OPS_R2=$(stats ipc_ops_read)
ILP python3 - "$MOUNT_DIR/cp.bin" <<'EOF'
import os, sys
fd = os.open(sys.argv[1], os.O_RDONLY)
d = os.dup(fd)
os.close(fd)
data = os.pread(d, 65536, 0)
assert len(data) == 65536, f"dup read short: {len(data)}"
os.close(d)
EOF
OPS_R3=$(stats ipc_ops_read)
[ "$OPS_R3" -gt "$OPS_R2" ] || fail "dup-then-close-original must stay intercepted (Issue-14)"
echo "OK: dup transparency"

# 2d. close_range-then-socket() fd reuse (G-L4-4): no stale serve, no
# crash — the socket op must behave as a socket.
ILP python3 - "$MOUNT_DIR/cp.bin" <<'EOF'
import os, socket, sys
fd = os.open(sys.argv[1], os.O_RDONLY)
os.pread(fd, 4096, 0)
os.closerange(fd, fd + 1)          # close_range over the bound fd
s = socket.socket()                # commonly reuses the number
try:
    os.pread(s.fileno(), 16, 0)    # must be ESPIPE from the KERNEL,
    raise AssertionError("pread on a socket must fail")
except OSError:
    pass                           # never stale file bytes
s.close()
EOF
echo "OK: close_range-then-socket reuse"

# 2e. lseek-SEEK_CUR-is-FUSE-free pin (§5.4.3): a SEEK_CUR storm on a
# bound fd must not generate FUSE traffic (f_pos is kernel-local).
FUSE0=$(stats fuse_ops)
ILP python3 - "$MOUNT_DIR/cp.bin" <<'EOF'
import os, sys
fd = os.open(sys.argv[1], os.O_RDONLY)
for _ in range(10000):
    os.lseek(fd, 0, os.SEEK_CUR)
os.close(fd)
EOF
FUSE1=$(stats fuse_ops)
DELTA=$((FUSE1 - FUSE0))
[ "$DELTA" -lt 100 ] || fail "10k SEEK_CURs generated $DELTA FUSE ops — lseek is not FUSE-free"
echo "OK: lseek SEEK_CUR pin ($DELTA FUSE ops for 10k seeks)"

# 2f. Offsetful read()/write() discipline: sequential cat through the
# shim, byte parity (kernel f_pos advanced by the shim's SEEK_SET).
ILP cat "$MOUNT_DIR/cp.bin" > "$T/cat.bin"
cmp "$T/src.bin" "$T/cat.bin" || fail "sequential read() parity (offset discipline)"
echo "OK: offsetful read() parity"

# 2g. fio / elbencho when present (parity-checked workloads).
if command -v fio &>/dev/null; then
    ILP fio --name=il --directory="$MOUNT_DIR" --size=32M --bs=4k --rw=randwrite \
        --ioengine=psync --verify=crc32c --do_verify=1 --output-format=terse >/dev/null \
        || fail "fio randwrite+verify under LD_PRELOAD"
    echo "OK: fio verify"
else
    echo "SKIP: fio not installed"
fi
if command -v elbencho &>/dev/null; then
    ILP elbencho -w -r -t 4 -b 64k -s 16m --verify 1 --nodelerr \
        "$MOUNT_DIR/elb" >/dev/null 2>&1 \
        || fail "elbencho write+read+verify under LD_PRELOAD"
    echo "OK: elbencho verify"
else
    echo "SKIP: elbencho not installed"
fi

echo "PRELOAD GATE (both legs) PASSED"
