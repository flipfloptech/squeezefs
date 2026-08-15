#!/usr/bin/env bash
# tests/mw_two_registrants_leg.sh — two PR registrants from ONE box via the
# product connect path (design-full-multi-writer rung 2, KD-MW-3 / §5.2)
# =============================================================================
#
# The guard_smoke.sh family's rung-2 leg: proves that per-connection host
# identity (`squeezefs nvmeof connect --hostnqn/--hostid` — the same
# ConnectOptions identity the mount's daemon-owned connects present) makes
# two co-located processes TWO registrants on a real kernel nvmet-tcp
# target (resv_enable=1), and that registrant preemption is DEVICE-enforced:
# after preempting one registrant, the device rejects the preempted holder's
# I/O with the reservation-conflict class while the survivor keeps writing.
#
# Product verbs drive every fabric object (the nvmeof_target_substrate.sh
# discipline): `nvmeof share --target-stack nvmet` builds the tcp target
# (kernel nvmet writes resv_enable=1 by default — src/nvmeof/nvmet.rs),
# `nvmeof connect --hostnqn/--hostid` establishes the two identities,
# `nvmeof disconnect`/`unshare` tear down. nvme-cli reservation commands are
# the measurement instrument only (register/acquire/preempt/report), issued
# per-controller through the controller CHAR devices so the assertion holds
# on native-multipath kernels too (the head block node round-robins paths;
# the char device pins the association).
#
# Usage:  sudo tests/mw_two_registrants_leg.sh
#
# Requires: root, nvme-cli, kernel nvmet + nvmet-tcp modules, jq not needed.
# Port: 54142 — inside the tcp devsub service slice 54100–54199 (never the
# fidelity tier's 54000–54099).
#
# Legs (each asserted; exits nonzero on any FAIL):
#   1. product share (nvmet-tcp, 127.0.0.1:54142) + resv_enable=1 probe
#   2. two product connects with distinct --hostnqn/--hostid pairs ⇒ two
#      controllers, two ACTUAL identities in sysfs (rule 2's answer)
#   3. both register; A acquires WERO; report shows 2 registrants
#   4. B writes under WERO (registrants only ⇒ admitted)
#   5. B PREEMPTS the holder A (the product takeover shape) ⇒ report
#      shows 1 registrant; the DEVICE rejects the preempted holder A's
#      write (reservation conflict); B still writes
#   6. teardown to zero residue (product disconnect + unshare)
#
# Measured semantics this leg's shapes rest on (kernel nvmet, verified
# live 2026-08-15 on 7.1.6-sqz — drivers/nvme/target/pr.c):
#   * WERO is NVMe rtype **3** (Write Exclusive – Registrants Only).
#     rtype 2 is EXCLUSIVE ACCESS: under it a registered second host is
#     refused READS AND WRITES — NOTE this diverges from
#     `src/meta_backend/reservation.rs`'s
#     RTYPE_WRITE_EXCLUSIVE_REGISTRANTS_ONLY = 2 (reported with rung 2;
#     out of this rung's scope to change).
#   * A HOLDER-issued preempt naming another registrant's key is a
#     rtype-update no-op on kernel nvmet (pr.c's `holder == reg` arm
#     ignores PRKEY), so the leg preempts THE HOLDER — which is also the
#     product's actual takeover shape (a successor preempting a dead
#     writer's key).

set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SQZ="${SQZ_BIN:-$REPO/target/release/squeezefs}"
[ -x "$SQZ" ] || SQZ="$REPO/target/debug/squeezefs"

PORT=54142
NQN="nqn.2026-08.io.squeezefs:mw2reg-$$"
HOSTNQN_A="nqn.2014-08.org.nvmexpress:uuid:aaaaaaaa-2026-0815-0000-$(printf '%012d' $$)"
HOSTID_A="aaaaaaaa-2026-0815-0000-$(printf '%012d' $$)"
HOSTNQN_B="nqn.2014-08.org.nvmexpress:uuid:bbbbbbbb-2026-0815-0000-$(printf '%012d' $$)"
HOSTID_B="bbbbbbbb-2026-0815-0000-$(printf '%012d' $$)"
KEY_A=0x2026a
KEY_B=0x2026b
BACKING="$(mktemp -u /tmp/sqz-mw2reg-XXXXXX.img)"

if [ "$(id -u)" -ne 0 ]; then
    exec sudo env SQZ_BIN="$SQZ" bash "$0" "$@"
fi

pass=0
fail=0
ok() {
    echo "[mw2reg] OK  $*"
    pass=$((pass + 1))
}
bad() {
    echo "[mw2reg] FAIL $*" >&2
    fail=$((fail + 1))
}
die() {
    echo "[mw2reg] ERROR $*" >&2
    cleanup
    exit 2
}

nqn_controllers_live() { # 0 ⇔ some controller still serves $NQN
    local c
    for c in /sys/class/nvme/nvme*; do
        [ -d "$c" ] || continue
        [ "$(cat "$c/subsysnqn" 2>/dev/null)" = "$NQN" ] && return 0
    done
    return 1
}

cleanup() {
    "$SQZ" nvmeof disconnect "$NQN" >/dev/null 2>&1
    # Controllers take a beat to tear down before unshare sees zero conns.
    for _ in $(seq 1 20); do
        nqn_controllers_live || break
        sleep 0.25
    done
    # No --force: this NQN is nvmet-recorded, and the product refuses a
    # force flag it would have to silently ignore there (the
    # never-ignore-an-explicit-flag law — measured in this leg's bring-up).
    "$SQZ" nvmeof unshare "$NQN" >/dev/null 2>&1
    rm -f "$BACKING"
}
trap cleanup EXIT

command -v nvme >/dev/null 2>&1 || die "nvme-cli is required"
[ -x "$SQZ" ] || die "squeezefs binary not found (cargo build --release)"
modprobe nvmet 2>/dev/null
modprobe nvmet-tcp 2>/dev/null
modprobe nvme-tcp 2>/dev/null
[ -d /sys/kernel/config/nvmet ] || die "kernel nvmet (configfs) unavailable"

# --- 1. product share: nvmet-tcp on localhost, resv_enable=1 ---------------
"$SQZ" nvmeof share "$BACKING" --create-size 64M --subnqn "$NQN" \
    --ip 127.0.0.1 --port "$PORT" --target-stack nvmet ||
    die "product share failed"
RESV="/sys/kernel/config/nvmet/subsystems/$NQN/namespaces/1/resv_enable"
if [ "$(cat "$RESV" 2>/dev/null)" = "1" ]; then
    ok "share live on nvmet-tcp 127.0.0.1:$PORT with resv_enable=1"
else
    bad "resv_enable is not 1 (no enforcement-grade PR on this namespace)"
fi

# --- 2. two product connects, two identities --------------------------------
"$SQZ" nvmeof connect --ip 127.0.0.1 --port "$PORT" --subnqn "$NQN" \
    --hostnqn "$HOSTNQN_A" --hostid "$HOSTID_A" >/dev/null ||
    die "product connect (identity A) failed"
"$SQZ" nvmeof connect --ip 127.0.0.1 --port "$PORT" --subnqn "$NQN" \
    --hostnqn "$HOSTNQN_B" --hostid "$HOSTID_B" >/dev/null ||
    die "product connect (identity B) failed"

# Rule 2's answer: the ACTUAL identities in sysfs, per controller.
ctrl_for_hostnqn() { # hostnqn -> controller name (nvmeX)
    for c in /sys/class/nvme/nvme*; do
        [ -d "$c" ] || continue
        [ "$(cat "$c/subsysnqn" 2>/dev/null)" = "$NQN" ] || continue
        [ "$(cat "$c/hostnqn" 2>/dev/null)" = "$1" ] || continue
        basename "$c"
        return 0
    done
    return 1
}
CTRL_A=""
CTRL_B=""
for _ in $(seq 1 40); do
    CTRL_A="$(ctrl_for_hostnqn "$HOSTNQN_A")" && CTRL_B="$(ctrl_for_hostnqn "$HOSTNQN_B")" && break
    sleep 0.25
done
[ -n "$CTRL_A" ] && [ -n "$CTRL_B" ] && [ "$CTRL_A" != "$CTRL_B" ] ||
    die "expected two controllers with distinct hostnqn attrs (A='$CTRL_A' B='$CTRL_B')"
ok "two controllers, two ACTUAL identities: $CTRL_A (A) / $CTRL_B (B)"

DEV_A="/dev/$CTRL_A"
DEV_B="/dev/$CTRL_B"

# --- 3. both register; A acquires WERO; report shows 2 registrants ---------
# WERO = NVMe rtype 3 (Write Exclusive – Registrants Only). See the
# header note: rtype 2 is EXCLUSIVE ACCESS and refuses registrants.
WERO=3
nvme resv-register "$DEV_A" -n 1 --nrkey=$KEY_A --cptpl=0 >/dev/null ||
    die "register A failed"
nvme resv-register "$DEV_B" -n 1 --nrkey=$KEY_B --cptpl=0 >/dev/null ||
    die "register B failed"
nvme resv-acquire "$DEV_A" -n 1 --crkey=$KEY_A --rtype=$WERO --racqa=0 >/dev/null ||
    die "WERO acquire (A) failed"
REGCTL="$(nvme resv-report "$DEV_A" -n 1 -o json 2>/dev/null |
    tr -d ' \n' | sed -n 's/.*"regctl":\([0-9]*\).*/\1/p')"
if [ "$REGCTL" = "2" ]; then
    ok "reservation report: 2 registrants from one box (WERO held by A)"
else
    bad "expected regctl=2, got '$REGCTL'"
fi

wr() { # ctrl-char-dev -> 0 on admitted write, nonzero on rejection
    nvme write "$1" -n 1 --start-block=0 --block-count=0 --data-size=512 \
        --data=/dev/zero >/dev/null 2>&1
}

# --- 4. WERO admits the OTHER registrant ------------------------------------
if wr "$DEV_B"; then
    ok "registrant B writes under WERO (registrants-only admits)"
else
    bad "registrant B's write was rejected while still registered"
fi

# --- 5. B preempts the HOLDER A (the takeover shape); the device rejects
#        the preempted holder ------------------------------------------------
nvme resv-acquire "$DEV_B" -n 1 --crkey=$KEY_B --prkey=$KEY_A --rtype=$WERO \
    --racqa=1 >/dev/null || die "preempt of holder A's key failed"
REGCTL="$(nvme resv-report "$DEV_B" -n 1 -o json 2>/dev/null |
    tr -d ' \n' | sed -n 's/.*"regctl":\([0-9]*\).*/\1/p')"
if [ "$REGCTL" = "1" ]; then
    ok "preempt removed the holder A's registration (regctl=1)"
else
    bad "expected regctl=1 after preempt, got '$REGCTL'"
fi
if wr "$DEV_A"; then
    bad "the DEVICE admitted the preempted holder's write — fencing is not device-enforced"
else
    ok "the DEVICE rejects the preempted holder's write (reservation conflict)"
fi
if wr "$DEV_B"; then
    ok "the surviving registrant (new holder) still writes"
else
    bad "the survivor's write failed after preempting A"
fi

# --- 6. teardown to zero residue --------------------------------------------
nvme resv-release "$DEV_B" -n 1 --crkey=$KEY_B --rtype=$WERO >/dev/null 2>&1
nvme resv-register "$DEV_B" -n 1 --crkey=$KEY_B --rrega=1 >/dev/null 2>&1
cleanup
trap - EXIT
nqn_controllers_live && bad "a controller for $NQN survived teardown"
[ -d "/sys/kernel/config/nvmet/subsystems/$NQN" ] &&
    bad "nvmet subsystem survived teardown"

echo "[mw2reg] $pass ok, $fail failed"
[ "$fail" -eq 0 ] || exit 1
ok "two-registrants-from-one-box leg complete"
exit 0
