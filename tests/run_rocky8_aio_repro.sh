#!/bin/bash
set -euo pipefail

# Rocky 8 libaio-ABI repro rig — the 2026-07-25 field segfault
# (elbencho libaio t32 qd32 under LD_PRELOAD, Rocky 8.10: glibc 2.28,
# libaio 0.3.112; backtrace frames 0xF00/0xF1B in libaio.so.1 under the
# shim's io_getevents).
#
# Root cause this rig pins: glibc < 2.36 `dlsym(RTLD_NEXT, ...)` returns
# the BASE version of a multi-versioned symbol — for libaio that is
# `io_getevents@LIBAIO_0.1`, a 4-argument compat wrapper (EL8 offsets
# 0xEE0..: `movdqu (%rcx)` at +0x20 = the field frame 0xF00; its
# internal PLT call's return address = 0xF1B). Called with the shim's
# 5-argument LIBAIO_0.4 convention it shuffles argument registers and
# re-enters the interposer through its PLT until an integer lands in
# the timeout register — SIGSEGV inside libaio. Fires in BOTH modes
# (passthrough fallback and armed merge path).
#
# Legs (all inside the rocky8 test container; artifacts = dist/rocky8):
#   A. plain-dir passthrough: lifecycle harness (3 orderings) + elbencho
#      (the reporter's t32 qd32 line) on a tmpfs dir.
#   B. armed mount (--interception; needs --privileged + /dev/fuse):
#      the field-exact merge-path shape, engagement-checked.
#   C. refused mount (no --interception): the refusal-line shape.
#
# Prereqs: podman or docker; `task build:rocky8` artifacts in
# dist/rocky8 (built here when missing); the squeezefs-test:rocky8
# image (built here when missing — docker/Dockerfile.rocky8-test).

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_DIR"

CT="${CONTAINER_TOOL:-}"
if [ -z "$CT" ]; then
    if command -v podman &>/dev/null; then CT=podman
    elif command -v docker &>/dev/null; then CT=docker
    else echo "FAIL: no container runtime (podman/docker)" >&2; exit 1; fi
fi

fail() { echo "ROCKY8 AIO REPRO FAIL: $*" >&2; exit 1; }

# Artifacts (daemon + shim + harness). The harness rides the build's
# target volume; rebuilds happen via the sanctioned task target.
if [ ! -x dist/rocky8/squeezefs ] || [ ! -x dist/rocky8/libsqueezefs_il.so ]; then
    command -v go-task &>/dev/null && go-task build:rocky8 || task build:rocky8
fi
[ -x dist/rocky8/squeezefs ] || fail "dist/rocky8 artifacts missing (task build:rocky8)"

if ! $CT image exists squeezefs-test:rocky8 2>/dev/null; then
    $CT build -t squeezefs-test:rocky8 -f docker/Dockerfile.rocky8-test docker
fi

# -i: the leg script rides stdin (`bash -s`); without it the container
# shell sees EOF and exits 0 silently — a false green.
$CT run --rm -i \
    --privileged --device /dev/fuse \
    -v "$REPO_DIR/dist/rocky8:/art:ro" \
    -v "squeezefs-target-rocky8:/build" \
    squeezefs-test:rocky8 bash -s <<'INNER'
set -euo pipefail
ulimit -c 0
SO=/art/libsqueezefs_il.so
BIN=/art/squeezefs
H=/build/target/preload-release/aio_lifecycle_harness
[ -x "$H" ] || { echo "FAIL: harness missing in target volume" >&2; exit 1; }
fail() { echo "ROCKY8 AIO REPRO FAIL: $*" >&2; exit 1; }

rpm -q glibc libaio

echo "=== Leg A: plain-dir passthrough ==="
mkdir -p /tmp/plain
for mode in setup-first open-first mixed; do
    LD_PRELOAD=$SO $H /tmp/plain $mode 8 3 16 \
        || fail "harness ($mode, plain dir) rc=$? — the libaio ABI/versioning contract is broken"
done
LD_PRELOAD=$SO elbencho -w -r -t 32 --iodepth 32 -b 4k -s 2m --direct --nolive /tmp/plain/f{1..32} >/dev/null \
    || fail "elbencho t32 qd32 (plain dir) rc=$?"
echo "OK: leg A"

echo "=== Leg B: armed mount (the field-exact merge-path shape) ==="
mkdir -p /mnt/sqz /var/sqz_staging
truncate -s 1G /dev/shm/meta /dev/shm/data
$BIN format sqmeta:///dev/shm/meta sqdata:///dev/shm/data \
    --disk-cache-paths /var/sqz_staging --force >/dev/null
export SQUEEZEFS_IPC_ALLOW_DEV=1
RUST_LOG=warn $BIN mount sqmeta:///dev/shm/meta /mnt/sqz \
    --daemon --interception --disk-cache-size 200MB \
    --log-file /tmp/sqz.log --allow-other >/dev/null
sleep 3
mountpoint -q /mnt/sqz || { tail -20 /tmp/sqz.log >&2; fail "armed mount failed"; }
chmod 1777 /mnt/sqz
stats() { python3 -c "import json;print(json.load(open('/mnt/sqz/.stats'))['metrics'].get('$1',0))"; }
R0=$(stats ipc_ops_read); W0=$(stats ipc_ops_write)
for mode in setup-first open-first mixed; do
    LD_PRELOAD=$SO $H /mnt/sqz $mode 8 3 16 \
        || fail "harness ($mode, armed mount) rc=$?"
done
LD_PRELOAD=$SO elbencho -w -r -t 32 --iodepth 32 -b 4k -s 2m --direct --nolive /mnt/sqz/f{1..32} >/dev/null \
    || fail "elbencho t32 qd32 (armed mount) rc=$?"
R1=$(stats ipc_ops_read); W1=$(stats ipc_ops_write)
[ "$((R1 - R0))" -gt 0 ] && [ "$((W1 - W0))" -gt 0 ] \
    || fail "armed leg never engaged the ring (reads Δ$((R1-R0)), writes Δ$((W1-W0))) — it would not cover the field's merge path"
echo "OK: leg B (ring reads Δ$((R1-R0)), writes Δ$((W1-W0)))"

echo "=== Leg C: refused mount (no --interception) ==="
umount /mnt/sqz 2>/dev/null || umount -l /mnt/sqz || true
for i in $(seq 1 20); do pgrep -x squeezefs >/dev/null || break; sleep 0.5; done
pgrep -x squeezefs >/dev/null && { kill -9 $(pgrep -x squeezefs) || true; sleep 1; }
RUST_LOG=warn $BIN mount sqmeta:///dev/shm/meta /mnt/sqz \
    --daemon --disk-cache-size 200MB \
    --log-file /tmp/sqz.log --allow-other >/dev/null
sleep 3
mountpoint -q /mnt/sqz || { tail -20 /tmp/sqz.log >&2; fail "refused-mode mount failed"; }
chmod 1777 /mnt/sqz
LD_PRELOAD=$SO $H /mnt/sqz mixed 8 3 16 2>/tmp/refused.err \
    || { cat /tmp/refused.err >&2; fail "harness (refused mount) rc=$?"; }
grep -q "session refused" /tmp/refused.err || fail "refusal line missing on the refused mount"
echo "OK: leg C"

echo "ROCKY8 AIO REPRO PASSED"
INNER
