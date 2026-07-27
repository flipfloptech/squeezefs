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

# 1d. libaio lifecycle passthrough (2026-07-25 field crash class): the
# full io_setup → io_submit → io_getevents → io_destroy lifecycle on a
# PLAIN (non-SqueezeFS) directory under the shim, all orderings — a
# context never created through a live shim session must be forwarded
# UNTOUCHED to real libaio (fake-vs-real io_context_t confusion was the
# elbencho t32 qd32 segfault). Multi-phase per thread so destroyed ring
# addresses recycle (the confusion-class trigger surface). Exit 2 =
# no libaio.so.1 on this box (skip loudly).
AIO_HARNESS="$REPO_DIR/target/preload-release/aio_lifecycle_harness"
[ -x "$AIO_HARNESS" ] || fail "aio_lifecycle_harness missing at $AIO_HARNESS"
# Self-interposition tripwire: the harness must NOT carry the strong
# interposer symbols itself (an exe's symbols beat the preloaded .so —
# the harness would silently test a self-interposed double-shim
# topology; caught live by the refusal-line once-count on 2026-07-25).
if nm "$AIO_HARNESS" 2>/dev/null | grep -qE " T (open|read|write|io_setup|io_getevents)$"; then
    fail "aio_lifecycle_harness carries interposer symbols — it must not link the featured rlib"
fi
for mode in setup-first open-first mixed; do
    rc=0
    LD_PRELOAD="$SO" "$AIO_HARNESS" "$T" "$mode" 8 4 16 || rc=$?
    if [ "$rc" -eq 2 ]; then echo "SKIP: libaio.so.1 not present (aio passthrough leg)"; break; fi
    [ "$rc" -eq 0 ] || fail "aio lifecycle passthrough ($mode) rc=$rc"
done
echo "OK: libaio lifecycle passthrough (plain fs, 3 orderings)"

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

# §5.6.2 W1 + the L4-6 mitigation: ring writes grow files invisibly to
# the KERNEL's attr cache (size TTL 1 s) — the daemon now pushes
# FUSE_NOTIFY_INVAL_INODE on bind + rate-limited first write, so an
# unbound kernel reader converges WITHIN the TTL. settle_notify polls
# with a deadline strictly BELOW the 1 s TTL: passing proves the notify
# was DELIVERED on the armed over-uring session (the L4-6 card's
# delivery pin), not merely that the TTL expired. Reads THROUGH the
# shim never see the window at all — the no-settle dd read-back row.
settle_notify() { # settle_notify <a> <b> <what>
    for _ in $(seq 1 8); do
        cmp -s "$1" "$2" && return 0
        sleep 0.1
    done
    fail "$3 (kernel reader did not converge < 1 s TTL — notify NOT delivered)"
}

# 2b. cp/dd parity ON the mount + the §3 rule-4 engagement proof.
OPS_R0=$(stats ipc_ops_read); OPS_W0=$(stats ipc_ops_write); SESS0=$(stats ipc_sessions_total)

ILP cp "$T/src.bin" "$MOUNT_DIR/cp.bin"
ILP dd if="$MOUNT_DIR/cp.bin" of="$T/back.bin" bs=64k status=none
cmp "$T/src.bin" "$T/back.bin" || fail "ring read-back parity (no settle: daemon state is the authority)"
settle_notify "$T/src.bin" "$MOUNT_DIR/cp.bin" "cp parity via notify delivery"
ILP dd if="$T/src.bin" of="$MOUNT_DIR/dd.bin" bs=1M oflag=direct status=none 2>/dev/null \
    || ILP dd if="$T/src.bin" of="$MOUNT_DIR/dd.bin" bs=1M status=none
settle_notify "$T/src.bin" "$MOUNT_DIR/dd.bin" "dd write parity via notify delivery"
INVAL=$(stats ipc_inval_notifies)
[ "$INVAL" -gt 0 ] || fail "ipc_inval_notifies never moved — the W1 handoff is not firing"
echo "OK: notify delivery on the armed session (ipc_inval_notifies=$INVAL)"

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
    # Run from the scratch dir: fio drops *-verify.state files in CWD.
    (cd "$T" && ILP fio --name=il --directory="$MOUNT_DIR" --size=32M --bs=4k --rw=randwrite \
        --ioengine=psync --verify=crc32c --do_verify=1 --output-format=terse >/dev/null) \
        || fail "fio randwrite+verify under LD_PRELOAD"
    echo "OK: fio verify"
else
    echo "SKIP: fio not installed"
fi
if command -v elbencho &>/dev/null; then
    # Run from the scratch dir: elbencho drops verify-state files in CWD.
    (cd "$T" && ILP elbencho -w -r -t 4 -b 64k -s 16m --verify 1 --nodelerr \
        "$MOUNT_DIR/elb" >/dev/null 2>&1) \
        || fail "elbencho write+read+verify under LD_PRELOAD"
    echo "OK: elbencho verify"
else
    echo "SKIP: elbencho not installed"
fi

# 2g-aio. libaio interposers (v1.1 OQ-1): mixed-batch async I/O with
# data verify, plus RING engagement proof — the iodepth concurrency
# must ride the session slots, not kernel FUSE (charter §3 rule 4: an
# aio row without an ipc_ops delta is measurement fraud, exactly the
# static-elbencho lesson).
# (ldd, not `ldconfig | grep -q`: under pipefail grep -q's early exit
# SIGPIPEs ldconfig's large listing and false-skips the row.)
if command -v fio &>/dev/null && ldd "$(command -v fio)" 2>/dev/null | grep -q libaio.so.1; then
    W0=$(stats ipc_ops_write)
    R0=$(stats ipc_ops_read)
    (cd "$T" && ILP fio --name=ilaio --directory="$MOUNT_DIR" --size=32M --bs=4k --rw=randwrite \
        --ioengine=libaio --iodepth=16 --verify=crc32c --do_verify=1 \
        --output-format=terse >/dev/null) \
        || fail "fio libaio randwrite+verify under LD_PRELOAD"
    W1=$(stats ipc_ops_write)
    R1=$(stats ipc_ops_read)
    WD=$((W1 - W0)); RD=$((R1 - R0))
    # 32M / 4k = 8192 writes + 8192 verify reads; threshold est/2.
    [ "$WD" -ge 4096 ] || fail "libaio writes bypassed the ring (ipc_ops_write Δ$WD < 4096)"
    [ "$RD" -ge 4096 ] || fail "libaio verify reads bypassed the ring (ipc_ops_read Δ$RD < 4096)"
    echo "OK: fio libaio verify (ring writes Δ$WD, reads Δ$RD)"
else
    echo "SKIP: fio or libaio.so.1 not present (libaio row)"
fi

# 2g-netns. OQ-6 path-socket rendezvous: a client in a FOREIGN network
# namespace (abstract AF_UNIX names are per-netns — the pre-v1.1 shape
# silently degraded to kernel FUSE fleet-wide in containers). unshare -n
# shares our mount namespace (FUSE mount + runtime dir visible) but not
# the netns, so the abstract rung ECONNREFUSEDs and ONLY the path
# socket can rendezvous. Engagement-checked like every il surface.
if command -v unshare &>/dev/null; then
    NS_R0=$(stats ipc_ops_read)
    NS_S0=$(stats ipc_sessions_total)
    unshare -n bash -c "LD_PRELOAD='$SO' dd if='$MOUNT_DIR/cp.bin' of=/dev/null bs=64k status=none" \
        || fail "preload'd read inside unshare -n"
    NS_R1=$(stats ipc_ops_read)
    NS_S1=$(stats ipc_sessions_total)
    [ "$((NS_S1 - NS_S0))" -ge 1 ] || fail "foreign-netns client established no session (path socket dead?)"
    [ "$((NS_R1 - NS_R0))" -ge 32 ] || fail "foreign-netns reads bypassed the ring (Δ$((NS_R1 - NS_R0)))"
    echo "OK: foreign-netns path-socket rendezvous (sessions +$((NS_S1 - NS_S0)), ring reads +$((NS_R1 - NS_R0)))"
else
    echo "SKIP: unshare not installed (netns row)"
fi

# 2h. kill-9 soak (L4-6, G-L4-4 zero-residue): SIGKILL a preload'd
# writer mid-stream ×5; every cycle must drain sessions AND session-shm
# bytes to the pre-cycle baseline (multi-run discipline: any failure
# aborts the count). Odd cycles write bs=1M — multi-slab pipelined
# flights (DIALED P3 large-op economy: contiguous slot runs + arena-
# extension holds) are in flight at SIGKILL, so run teardown/GC is
# soaked, not just single-slab ops; even cycles keep the 64k shape.
SESS_BASE=$(stats ipc_sessions_active)
ARENA_BASE=$(stats ipc_arena_bytes)
for i in 1 2 3 4 5; do
    if [ $((i % 2)) -eq 1 ]; then KBS=1M; KCNT=6000; else KBS=64k; KCNT=100000; fi
    # Env-prefixed SIMPLE command: bash backgrounds the real process, so
    # $! is dd itself. (`ILP dd ... &` backgrounds a SUBSHELL running the
    # function — the first run of this soak killed the wrapper while dd
    # survived to finish normally, testing nothing.)
    LD_PRELOAD="$SO" SQUEEZEFS_IPC_ALLOW_DEV=1 \
        dd if=/dev/zero of="$MOUNT_DIR/kill$i.bin" bs="$KBS" count="$KCNT" status=none &
    KPID=$!
    sleep 0.3
    kill -9 "$KPID" 2>/dev/null || true
    wait "$KPID" 2>/dev/null || true
    deadline=$((SECONDS + 10))
    while :; do
        [ "$(stats ipc_sessions_active)" -le "$SESS_BASE" ] \
            && [ "$(stats ipc_arena_bytes)" -le "$ARENA_BASE" ] && break
        [ "$SECONDS" -lt "$deadline" ] || fail "kill-9 cycle $i: session/arena residue"
        sleep 0.2
    done
done
mountpoint -q "$MOUNT_DIR" || fail "daemon died during the kill-9 soak"
echo "OK: kill-9 soak (5 cycles, zero session/arena residue)"

# 2i. fork-then-kill-parent (§5.7 row 1): the atfork child CLOSED its
# inherited socket copy, so SIGKILL'ing the parent drops the last ref —
# EOF fires and the session tears down even though a forked child
# still runs (the child's inherited arena mapping is the bounded
# straggler, reclaimed at its exit).
LD_PRELOAD="$SO" SQUEEZEFS_IPC_ALLOW_DEV=1 python3 - "$MOUNT_DIR/forkp.bin" <<'EOF' &
import os, sys, time
fd = os.open(sys.argv[1], os.O_RDWR | os.O_CREAT, 0o644)
os.pwrite(fd, b"x" * 65536, 0)          # bind + session established
pid = os.fork()
if pid == 0:
    time.sleep(20)                        # child outlives the parent
    os._exit(0)
print(os.getpid(), flush=True)
time.sleep(30)                            # parent waits to be killed
EOF
FPID=$!
sleep 1.5                                  # session + fork established
kill -9 "$FPID" 2>/dev/null || true
wait "$FPID" 2>/dev/null || true
deadline=$((SECONDS + 10))
while :; do
    [ "$(stats ipc_sessions_active)" -le "$SESS_BASE" ] && break
    [ "$SECONDS" -lt "$deadline" ] \
        || fail "fork-kill-parent: session never tore down — the surviving child masked parent-death EOF"
    sleep 0.2
done
pkill -9 -f "$MOUNT_DIR/forkp.bin" 2>/dev/null || true
echo "OK: fork-then-kill-parent (EOF fired despite the surviving child)"

# 2j. libaio lifecycle on the ARMED mount (served merge path): the same
# harness with ring-lane engagement — mixed batches, per-phase ctx
# destroy/recreate (ring-address recycling against LIVE sessions).
AIO_HARNESS="$REPO_DIR/target/preload-release/aio_lifecycle_harness"
[ -x "$AIO_HARNESS" ] || fail "aio_lifecycle_harness missing at $AIO_HARNESS"
AIO_SKIP=0
for mode in setup-first open-first mixed; do
    rc=0
    ILP "$AIO_HARNESS" "$MOUNT_DIR" "$mode" 8 4 16 || rc=$?
    if [ "$rc" -eq 2 ]; then AIO_SKIP=1; echo "SKIP: libaio.so.1 not present (aio mount legs)"; break; fi
    [ "$rc" -eq 0 ] || fail "aio lifecycle on the armed mount ($mode) rc=$rc"
done
[ "$AIO_SKIP" -eq 1 ] || echo "OK: libaio lifecycle on the armed mount (3 orderings)"

# 2k. libaio lifecycle in the ESTABLISH-REFUSED shape — the 2026-07-25
# field crash environment, exactly: a mount WITHOUT -o interception
# still arms the VL2 control-plane host (bootstrap xattr serves, the
# listener answers) but refuses every data-plane HELLO, so the shim
# reports "session refused: interception not armed on this mount"
# (once per mount+reason) and the fds stay unbound. libaio through the
# shim must then be
# PERFECTLY passthrough: full lifecycle green, zero ipc data ops.
if [ "$AIO_SKIP" -eq 0 ]; then
    # Free the kill-soak ballast (the 1G backend is near-full) and let
    # the drained daemon release the D0 writer claim before remounting.
    rm -f "$MOUNT_DIR"/kill*.bin "$MOUNT_DIR"/dd.bin "$MOUNT_DIR"/elb* 2>/dev/null || true
    sleep 1
    umount "$MOUNT_DIR" 2>/dev/null || umount -l "$MOUNT_DIR" 2>/dev/null || true
    deadline=$((SECONDS + 20))
    while pgrep -x squeezefs >/dev/null 2>&1; do
        if [ "$SECONDS" -ge "$deadline" ]; then
            killall -9 squeezefs 2>/dev/null || true # D0 flock reclaim is instant
            sleep 1
            break
        fi
        sleep 0.5
    done
    RUST_LOG=info "$SQUEEZEFS_BIN" mount \
        sqmeta:///dev/shm/squeezefs_il_meta \
        "$MOUNT_DIR" \
        --daemon \
        --disk-cache-size 500MB \
        --log-file "$LOG" \
        --allow-other
    sleep 3
    mountpoint -q "$MOUNT_DIR" || { cat "$LOG" || true; fail "no-interception remount failed"; }
    chmod 1777 "$MOUNT_DIR"
    PT_R0=$(stats ipc_ops_read); PT_W0=$(stats ipc_ops_write)
    PT_ERR="$T/aio_pt_stderr.log"
    for mode in setup-first open-first mixed; do
        rc=0
        ILP "$AIO_HARNESS" "$MOUNT_DIR" "$mode" 8 4 16 2>"$PT_ERR" || rc=$?
        [ "$rc" -eq 0 ] || { cat "$PT_ERR"; fail "aio lifecycle in the establish-refused shape ($mode) rc=$rc"; }
    done
    # Reason-bearing refusal line (user directive 2026-07-25): the shim
    # must NAME the cause — this shape's cause is the unarmed data
    # plane — and print it once per (mount, reason) per process, never
    # per open/thread (the field report's 32-thread run printed a line
    # per open). $PT_ERR holds the LAST harness run (one process,
    # 8 threads x 3 phases of opens).
    grep -q "session refused: interception not armed on this mount" "$PT_ERR" \
        || { cat "$PT_ERR"; fail "refusal line must name the unarmed-interception cause"; }
    grep -q -- "--interception" "$PT_ERR" \
        || fail "refusal line must name the remedy (mount with --interception)"
    REFUSE_LINES=$(grep -c "session refused" "$PT_ERR" || true)
    [ "$REFUSE_LINES" -eq 1 ] \
        || fail "refusal line must print once per (mount, reason) per process — got $REFUSE_LINES lines"
    PT_R1=$(stats ipc_ops_read); PT_W1=$(stats ipc_ops_write)
    [ "$PT_R1" -eq "$PT_R0" ] && [ "$PT_W1" -eq "$PT_W0" ] \
        || fail "passthrough mount served ring ops (reads Δ$((PT_R1-PT_R0)), writes Δ$((PT_W1-PT_W0)))"
    if command -v elbencho &>/dev/null; then
        # The reporter's instrument shape (libaio engine, deep iodepth).
        (cd "$T" && ILP elbencho -w -r -t 8 --iodepth 16 -b 4k -s 2m --direct --nolive \
            "$MOUNT_DIR/aiopt"{1..8} >/dev/null 2>&1) \
            || fail "elbencho libaio on the establish-refused mount"
        echo "OK: elbencho libaio (establish-refused shape)"
    fi
    echo "OK: libaio lifecycle passthrough (establish-refused shape, engagement zero)"
fi

# 2l. direct-drive kill-9 soak (DIALED P1, perf/ipc-direct-drive): a
# device-true remount arms the governed miss shape — the service
# thread submits ranged device reads on the ipc-host uring with the
# session ARENA as the DMA destination. SIGKILL a preload'd O_DIRECT
# reader mid-stream x5: every cycle must drain sessions AND arena
# bytes to baseline (the in-flight CQE pins the mapping Arc — §5.3.1
# rule 4 — so teardown is safe but must still CONVERGE), the daemon
# must survive, and the soak is INVALID unless direct-drive actually
# engaged (ipc_direct_drive_serves delta > 0 — the engagement rule).
umount "$MOUNT_DIR" 2>/dev/null || umount -l "$MOUNT_DIR" 2>/dev/null || true
deadline=$((SECONDS + 20))
while pgrep -x squeezefs >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then
        killall -9 squeezefs 2>/dev/null || true
        sleep 1
        break
    fi
    sleep 0.5
done
RUST_LOG=info "$SQUEEZEFS_BIN" mount \
    sqmeta:///dev/shm/squeezefs_il_meta \
    "$MOUNT_DIR" \
    --daemon \
    --interception \
    --disk-cache-size 500MB \
    --log-file "$LOG" \
    --allow-other \
    -o direct_device_true
sleep 3
mountpoint -q "$MOUNT_DIR" || { cat "$LOG" || true; fail "direct-drive remount failed"; }
chmod 1777 "$MOUNT_DIR"
# A striped file (> one 4 MiB block) written through the kernel path.
dd if=/dev/urandom of="$MOUNT_DIR/ddsoak.bin" bs=1M count=12 status=none \
    || fail "direct-drive soak: striped fixture write"
sync "$MOUNT_DIR/ddsoak.bin" 2>/dev/null || true
DD_SERVES0=$(stats ipc_direct_drive_serves)
SESS_BASE=$(stats ipc_sessions_active)
ARENA_BASE=$(stats ipc_arena_bytes)
for i in 1 2 3 4 5; do
    LD_PRELOAD="$SO" SQUEEZEFS_IPC_ALLOW_DEV=1 \
        dd if="$MOUNT_DIR/ddsoak.bin" of=/dev/null iflag=direct bs=4k status=none &
    KPID=$!
    sleep 0.2
    kill -9 "$KPID" 2>/dev/null || true
    wait "$KPID" 2>/dev/null || true
    deadline=$((SECONDS + 10))
    while :; do
        [ "$(stats ipc_sessions_active)" -le "$SESS_BASE" ] \
            && [ "$(stats ipc_arena_bytes)" -le "$ARENA_BASE" ] && break
        [ "$SECONDS" -lt "$deadline" ] || fail "direct-drive kill-9 cycle $i: session/arena residue"
        sleep 0.2
    done
done
mountpoint -q "$MOUNT_DIR" || fail "daemon died during the direct-drive kill-9 soak"
DD_SERVES1=$(stats ipc_direct_drive_serves)
[ "$DD_SERVES1" -gt "$DD_SERVES0" ] \
    || fail "direct-drive kill-9 soak never engaged direct-drive (serves Δ0 — the leg tested nothing)"
[ "$(stats ipc_descriptor_rejects)" -eq 0 ] || fail "descriptor rejects after the direct-drive soak"
[ "$(stats ipc_sessions_poisoned)" -eq 0 ] || fail "poisoned sessions after the direct-drive soak"
echo "OK: direct-drive kill-9 soak (5 cycles, engaged +$((DD_SERVES1-DD_SERVES0)) serves, zero residue)"

echo "PRELOAD GATE (both legs) PASSED"
