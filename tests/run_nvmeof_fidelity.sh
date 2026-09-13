#!/usr/bin/env bash
# tests/run_nvmeof_fidelity.sh — the kernel-nvmet NVMe-oF fidelity tier
# ======================================================================
#
# The standing real-kernel, ZERO-MOCK acceptance suite for the NVMe-oF
# target-management program (docs/design-nvmeof-target-management.md §6.8):
# every leg drives the PRODUCT's own verbs against the real kernel nvmet
# target — THE target since SPDK was retired (owner ruling R-SYM-8,
# docs/design-symmetric-metadata.md §5.8.1; the SPDK legs, the stalling
# JSON-RPC crash-window proxy and the PTPL power-cycle leg left with it) —
# on the fidelity substrate (tests/nvmeof_target_substrate.sh). Supersedes
# the ad-hoc root gates that lived under `.agents/spdk-scoping/` (removed
# from the tree — git history at c615e3a).
#
# Usage
#   sudo tests/run_nvmeof_fidelity.sh quick     # per-PR tier (~10 min)
#   sudo tests/run_nvmeof_fidelity.sh full      # nightly tier (~45-70 min)
#
# quick (per-PR for changes touching src/nvmeof/, reservation.rs, or the
# guard gate): product verb round-trip (share -> connect -> IO -> unshare ->
# residue-free) + ONE guard kill-9 cycle.
#
# full (nightly / program & release gates) adds:
#   * loud-fail matrix (G3): missing backing, unledgered unshare, --nsid!=1,
#     the deleted SPDK-only flags, the R-SYM-8 retirement refusals
#     (--target-stack spdk, SQUEEZEFS_NVMEOF_TARGET_STACK=spdk,
#     SQUEEZEFS_SPDK_TGT_BIN, `target install`) naming nvmet + the re-share
#     sequence, idempotent target start, target stop refusal
#   * crash-window injection (§6.4 law 6): two REAL law-6 states produced by
#     kernel-refused mid-verb mutations (EADDRNOTAVAIL listener bind) —
#     pending finalized (live objects match), pending garbage-collected
#     after manual partial-residue wipe
#   * adopt legs (§6.10 pt 5): pre-rebuild-style configfs adopt (small-int
#     port id, zero target mutation, zero serving interruption),
#     adopt-after-simulated-ledger-loss, harness-owned refusal against the
#     fidelity NQN marker itself
#   * PR matrix (pr-matrix.sh productized): RESCAP, register/acquire,
#     cross-host fence (EBADE class), preempt, registration persistence
#     across an initiator disconnect (no PTPL claims — nvmet ptpls=0 by
#     design; the §6.7 errno contract is what this asserts)
#   * target-restart persistence (G2): configfs wipe -> `restore` -> the
#     same identity re-presented -> the connected initiator reattaches
#     without operator action
#   * soft-RoCE plumbing leg (rdma_rxe; user decision, Resolved Questions
#     #5): kernel-initiator NVMe/RDMA connect + IO round-trip against an
#     rxe listener on the PRODUCT-shared nvmet subsystem — plumbing
#     validation ONLY, explicitly NOT representative of real RNIC behavior;
#     no guard or perf claims ride it. The rdma listener is HARNESS-built:
#     the product's listener plumbing cannot express trtype=rdma yet (a
#     named residual for PR 7 — see the leg's RESIDUAL lines)
#   * A/B smoke row (recorded, NOT ordered): fio rand4k QD32 write+read on
#     the raw guard-data namespace — instrument: fio io_uring O_DIRECT
#     (per-release ordered rows belong to the bench rerun)
#   * guard matrix kill-9 x10 (S1 ladder; the multi-run discipline applies
#     — any fix restarts the count from zero) (tests/guard_smoke.sh)
#   * teardown-to-zero-residue proof (substrate before/after snapshot diff
#     empty — counted as a leg)
#
# Cadence mapping: AGENTS.md "Test tiering" table (quick = per-PR row,
# full = nightly row). Requires: root (re-execs via sudo), nvme-cli, jq,
# fio (A/B smoke; loud SKIP if absent).
# Artifacts: $FIDELI_STATE/legs/*.txt + fidelity-<mode>.log (kept until the
# next substrate create).

# shellcheck disable=SC2329 # every leg/helper below is invoked indirectly (run_leg, traps)
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
STATE="${FIDELI_STATE:-/tmp/squeezefs-fideli}"
export FIDELI_STATE="$STATE" # sub-invocations (substrate mkzram, guard_smoke) must agree
SUBSTRATE="$REPO/tests/nvmeof_target_substrate.sh"
GUARD_SMOKE="$REPO/tests/guard_smoke.sh"

MODE="${1:-}"
case "$MODE" in
quick | full) ;;
-h | --help | help | "")
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
    [ -n "$MODE" ]
    exit $?
    ;;
*)
    echo "usage: $0 quick|full" >&2
    exit 2
    ;;
esac

if [ "$(id -u)" -ne 0 ]; then
    KNOBS=()
    while IFS= read -r kv; do KNOBS+=("$kv"); done \
        < <(env | grep -E '^(FIDELI_|SQUEEZEFS_)' || true)
    exec sudo env "${KNOBS[@]}" bash "$0" "$MODE"
fi

NVMET_CFS=/sys/kernel/config/nvmet
LOG=""
PASS=0
FAIL=0
LEG_PASS=0
LEG_FAIL=0
SUMMARY=()

log() { echo "[fidelity] $*" | tee -a "$LOG"; }
ok() {
    PASS=$((PASS + 1))
    LEG_PASS=$((LEG_PASS + 1))
    log "PASS: $*"
}
bad() {
    FAIL=$((FAIL + 1))
    LEG_FAIL=$((LEG_FAIL + 1))
    log "FAIL: $*"
}
die() {
    log "FATAL: $*"
    exit 1
}
tctl() { sensors 2>/dev/null | awk '/Tctl/ {gsub(/[+°C]/, "", $2); print $2; exit}'; }

# Leg driver: per-leg pass/fail counts + wall duration for the summary table.
run_leg() { # name fn [args...]
    local name=$1 t0 t1 rc
    shift
    LEG_PASS=0
    LEG_FAIL=0
    log "===== leg: $name ====="
    t0=$(date +%s)
    "$@"
    rc=$?
    t1=$(date +%s)
    if [ "$rc" -ne 0 ] && [ "$LEG_FAIL" -eq 0 ]; then
        bad "$name: leg body exited rc=$rc"
    fi
    SUMMARY+=("$(printf '%-28s pass=%-3s fail=%-3s %4ss' "$name" "$LEG_PASS" "$LEG_FAIL" "$((t1 - t0))")")
}

record() { echo "$1" >> "$FIDELI_MANIFEST"; }

finddev() { # nqn -> /dev/nvmeXn1 head node
    local nqn=$1 c cname
    for _ in $(seq 1 60); do
        for c in /sys/class/nvme/nvme*; do
            [ -e "$c/subsysnqn" ] || continue
            if [ "$(cat "$c/subsysnqn" 2>/dev/null)" = "$nqn" ]; then
                cname=$(basename "$c")
                if [ -b "/dev/${cname}n1" ]; then
                    echo "/dev/${cname}n1"
                    return 0
                fi
            fi
        done
        sleep 0.5
    done
    return 1
}

wipe_marked_subsystem() { # nqn — manual configfs removal, OURS ONLY by marker
    local nqn=$1 p
    case "$nqn" in
    *fideli* | *fidadopt*) ;;
    *) die "wipe refused: $nqn is not a fidelity object" ;;
    esac
    for p in "$NVMET_CFS"/ports/*/subsystems/"$nqn"; do
        [ -L "$p" ] && rm -f "$p"
    done
    if [ -d "$NVMET_CFS/subsystems/$nqn" ]; then
        echo 0 > "$NVMET_CFS/subsystems/$nqn/namespaces/1/enable" 2>/dev/null
        rmdir "$NVMET_CFS/subsystems/$nqn/namespaces/1" 2>/dev/null
        rmdir "$NVMET_CFS/subsystems/$nqn" 2>/dev/null
    fi
}

ledger_state_of() { # nqn -> state string or empty
    jq -r ".shares[] | select(.subnqn==\"$1\") | .state" \
        "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json" 2>/dev/null
}

mnt_stat() { # mnt field -> first scalar off the .stats inode
    # `?` suppresses the "cannot index scalar" error so the alternatives
    # serve BOTH shapes: per-volume arrays (writer_guard_*) and daemon
    # scalars (the PR 6 fabric_* family).
    jq -r ".metrics.$2[0]? // .metrics.$2? // .$2[0]? // .$2? // empty" "$1/.stats" 2>/dev/null
}

io_roundtrip() { # dev mib -> 0 on md5 match (O_DIRECT both ways)
    local dev=$1 mib=$2 src="$STATE/io-src" m1 m2
    dd if=/dev/urandom of="$src" bs=1M count="$mib" status=none
    m1=$(md5sum "$src" | awk '{print $1}')
    dd if="$src" of="$dev" bs=1M oflag=direct status=none || return 1
    m2=$(dd if="$dev" bs=1M count="$mib" iflag=direct status=none | md5sum | awk '{print $1}')
    [ "$m1" = "$m2" ]
}

# ===========================================================================
# Round-trip legs (G1 shape — quick + full)
# ===========================================================================
leg_roundtrip_nvmet() {
    local out="$STATE/legs/rt-nvmet.txt" nqn_f nqn_b backing zb dev_f dev_b uuid_cfs uuid_led links
    nqn_f="nqn.2026-07.io.squeezefs:fideli-rt-file"
    nqn_b="nqn.2026-07.io.squeezefs:fideli-rt-block"
    backing="$STATE/backing-rt.img"
    # 4491 -> fnv1a64 first candidate 54065 (test slice).
    local port=4491 expect_id=54065

    if ! "$FIDELI_BIN" nvmeof share "$backing" --create-size 1G --ip 127.0.0.1 --port "$port" \
        --subnqn "$nqn_f" --target-stack nvmet > "$out" 2>&1; then
        bad "file share: $(tail -3 "$out")"
        return
    fi
    record "share=$nqn_f"
    if grep -qi "detection-grade" "$out"; then
        ok "file share: loop guarantee-class note printed"
    else
        bad "file share: loop note missing"
    fi
    if [ -f "$backing" ]; then
        ok "sparse backing created via explicit --create-size"
    else
        bad "backing missing"
    fi

    zb=$("$SUBSTRATE" mkzram $((2 * 1024 * 1024 * 1024)) rt-block)
    if ! "$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port "$port" \
        --subnqn "$nqn_b" --target-stack nvmet >> "$out" 2>&1; then
        bad "block share"
        return
    fi
    record "share=$nqn_b"
    if [ -d "$NVMET_CFS/subsystems/$nqn_f" ] && [ -d "$NVMET_CFS/subsystems/$nqn_b" ]; then
        ok "both subsystems live in configfs"
    else
        bad "subsystem missing"
    fi
    links=$(find "$NVMET_CFS/ports/$expect_id/subsystems" -mindepth 1 2>/dev/null | wc -l)
    if [ "$links" = 2 ]; then
        ok "deterministic slice port id $expect_id shared by both listeners"
    else
        bad "port $expect_id links=$links (want 2)"
    fi
    if [ "$(cat "$NVMET_CFS/subsystems/$nqn_b/namespaces/1/resv_enable" 2>/dev/null)" = 1 ]; then
        ok "resv_enable=1 (enforcement-grade PR)"
    else
        bad "resv_enable"
    fi
    uuid_cfs=$(cat "$NVMET_CFS/subsystems/$nqn_b/namespaces/1/device_uuid" 2>/dev/null)
    uuid_led=$(jq -r ".shares[] | select(.subnqn==\"$nqn_b\") | .ns_uuid" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json")
    if [ -n "$uuid_cfs" ] && [ "$uuid_cfs" = "$uuid_led" ]; then
        ok "device_uuid == ledger ns_uuid ($uuid_cfs)"
    else
        bad "uuid: cfs=$uuid_cfs ledger=$uuid_led"
    fi

    "$FIDELI_BIN" nvmeof connect --ip 127.0.0.1 --port "$port" --subnqn "$nqn_f" >> "$out" 2>&1
    record "connected=$nqn_f"
    "$FIDELI_BIN" nvmeof connect --ip 127.0.0.1 --port "$port" --subnqn "$nqn_b" >> "$out" 2>&1
    record "connected=$nqn_b"
    dev_f=$(finddev "$nqn_f") || { bad "no device for $nqn_f"; return; }
    dev_b=$(finddev "$nqn_b") || { bad "no device for $nqn_b"; return; }
    if io_roundtrip "$dev_f" 64; then
        ok "64 MiB O_DIRECT round-trip (file-backed)"
    else
        bad "file IO md5"
    fi
    if io_roundtrip "$dev_b" 64; then
        ok "64 MiB O_DIRECT round-trip (block-backed)"
    else
        bad "block IO md5"
    fi

    "$FIDELI_BIN" nvmeof list --json > "$STATE/legs/rt-nvmet-list.json" 2>/dev/null
    if jq -e "[.shares[] | select((.subnqn==\"$nqn_f\" or .subnqn==\"$nqn_b\")
            and .classification==\"managed\" and .live==true)] | length==2" \
        "$STATE/legs/rt-nvmet-list.json" >/dev/null; then
        ok "list: both shares managed+live"
    else
        bad "list reconciliation"
    fi

    nvme disconnect -n "$nqn_f" >/dev/null 2>&1
    nvme disconnect -n "$nqn_b" >/dev/null 2>&1
    sleep 1
    if "$FIDELI_BIN" nvmeof unshare "$nqn_f" >> "$out" 2>&1; then
        ok "unshare file share"
    else
        bad "unshare file"
    fi
    if "$FIDELI_BIN" nvmeof unshare "$nqn_b" >> "$out" 2>&1; then
        ok "unshare block share"
    else
        bad "unshare block"
    fi
    if [ -z "$(losetup -j "$backing" 2>/dev/null)" ]; then
        ok "loop detached"
    else
        bad "loop leaked"
    fi
    if [ ! -d "$NVMET_CFS/subsystems/$nqn_f" ] && [ ! -d "$NVMET_CFS/subsystems/$nqn_b" ]; then
        ok "subsystems gone"
    else
        bad "subsystem residue"
    fi
    if [ ! -d "$NVMET_CFS/ports/$expect_id" ]; then
        ok "port $expect_id removed (link-free last-out)"
    else
        bad "port $expect_id residue"
    fi
    if [ -z "$(ledger_state_of "$nqn_f")" ] && [ -z "$(ledger_state_of "$nqn_b")" ]; then
        ok "ledger clean of round-trip records"
    else
        bad "ledger residue"
    fi
}

# ===========================================================================
# Loud-fail matrix (G3 — full)
# ===========================================================================
leg_loudfail() {
    local out rc flag
    out=$("$FIDELI_BIN" nvmeof share "$STATE/definitely-missing.img" --ip 127.0.0.1 \
        --port 4650 --target-stack nvmet 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "does not exist" && echo "$out" | grep -q -- "--create-size"; then
        ok "missing backing refuses loud, names --create-size (explicit nvmet)"
    else
        bad "missing-backing refusal (nvmet): $out"
    fi
    if [ ! -e "$STATE/definitely-missing.img" ]; then
        ok "refusal conjured no file"
    else
        bad "refusal created the file"
    fi

    out=$("$FIDELI_BIN" nvmeof share "$STATE/definitely-missing.img" --ip 127.0.0.1 --port 4650 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "does not exist"; then
        ok "missing backing refuses loud (default stack = nvmet)"
    else
        bad "missing-backing refusal (default): $out"
    fi

    out=$("$FIDELI_BIN" nvmeof unshare nqn.2026-07.io.squeezefs:fideli-ghost 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "not in the share ledger"; then
        ok "unledgered unshare refuses (ownership = ledger membership)"
    else
        bad "unledgered unshare: $out"
    fi

    out=$("$FIDELI_BIN" nvmeof share /dev/null --ip 127.0.0.1 --target-stack nvmet --nsid 2 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "structurally fixed at 1"; then
        ok "--nsid 2 refuses loud (nvmet index structurally 1)"
    else
        bad "--nsid refusal: $out"
    fi

    # shellcheck disable=SC1091 # generated by nvmeof_target_substrate.sh create
    . "$STATE/devices.env"
    out=$("$FIDELI_BIN" nvmeof unshare "$NQN_GMETA_NVMET" --force 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "unexpected argument"; then
        ok "--force (the deleted SPDK-only flag) dies on clap, never a silent accept"
    else
        bad "unshare --force: $out"
    fi
    if [ "$(ledger_state_of "$NQN_GMETA_NVMET")" = "active" ]; then
        ok "refused unshare mutated nothing (guard record still active)"
    else
        bad "guard record disturbed"
    fi

    # --- R-SYM-8 retirement refusals: every SPDK-shaped surface refuses
    # loud naming nvmet + the re-share sequence; nothing falls back.
    out=$("$FIDELI_BIN" nvmeof share /dev/null --ip 127.0.0.1 --port 4650 --target-stack spdk 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "RETIRED" && echo "$out" | grep -q "nvmeof unshare" &&
        echo "$out" | grep -q -- "--target-stack nvmet"; then
        ok "--target-stack spdk refuses loud naming nvmet + the re-share sequence"
    else
        bad "--target-stack spdk refusal: $out"
    fi
    out=$(SQUEEZEFS_NVMEOF_TARGET_STACK=spdk "$FIDELI_BIN" nvmeof share /dev/null --ip 127.0.0.1 --port 4650 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "SQUEEZEFS_NVMEOF_TARGET_STACK" && echo "$out" | grep -q "nvmet"; then
        ok "SQUEEZEFS_NVMEOF_TARGET_STACK=spdk refuses at startup naming nvmet"
    else
        bad "env spdk refusal: $out"
    fi
    out=$(SQUEEZEFS_SPDK_TGT_BIN=/nonexistent/spdk_tgt "$FIDELI_BIN" nvmeof share /dev/null \
        --ip 127.0.0.1 --port 4650 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "SQUEEZEFS_SPDK_TGT_BIN" && echo "$out" | grep -q "RETIRED"; then
        ok "SQUEEZEFS_SPDK_TGT_BIN is a retired knob (refuses at startup)"
    else
        bad "retired-knob refusal: $out"
    fi
    out=$("$FIDELI_BIN" nvmeof target install 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "was removed" && echo "$out" | grep -q "target setup"; then
        ok "target install is a retired verb naming its successor"
    else
        bad "target install refusal: $out"
    fi
    for flag in "--core-mask 0x1" "--cores 2" "--dpdk-mem-mb 512" "--accept-version-drift"; do
        # shellcheck disable=SC2086 # the flag string is intentionally word-split
        out=$("$FIDELI_BIN" nvmeof target start $flag 2>&1)
        rc=$?
        if [ "$rc" -ne 0 ] && echo "$out" | grep -q "unexpected argument"; then
            ok "deleted SPDK-only flag '$flag' dies on clap"
        else
            bad "deleted flag $flag: $out"
        fi
    done

    # The kernel target is not a process: a second `target start` is the
    # idempotent readiness check + ledger replay (guard shares verified
    # no-op), never an "already running" refusal.
    out=$("$FIDELI_BIN" nvmeof target start 2>&1)
    rc=$?
    if [ "$rc" -eq 0 ] && echo "$out" | grep -q "verified no-op"; then
        ok "second target start is idempotent (ledger replay: verified no-ops)"
    else
        bad "idempotent target start: $out"
    fi
    out=$("$FIDELI_BIN" nvmeof target stop 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "not a process"; then
        ok "target stop refuses (the kernel target is not a process)"
    else
        bad "target stop: $out"
    fi

    if "$FIDELI_BIN" nvmeof target status --json 2>/dev/null |
        jq -e '.modules_present == true and .configfs_mounted == true and .subsystems >= 2' >/dev/null; then
        ok "target status reports modules + configfs + the guard subsystems"
    else
        bad "status shape"
    fi
}

# ===========================================================================
# Crash-window states — nvmet stack (§6.4 law 6; full)
#
# configfs writes cannot be interposed or slowed, so a SIGKILL cannot be
# landed deterministically between them. Both law-6 window STATES are
# produced for real instead, by a kernel-refused mid-verb mutation: an
# unassigned TEST-NET-1 listener address makes the port symlink fail
# EADDRNOTAVAIL mid-apply, abandoning the verb exactly as a crash would —
# pending intent + whatever objects the crash left.
# ===========================================================================
leg_crash_nvmet() {
    local nqn zb out rc
    # --- W-N1: pending + matching live objects -> restore FINALIZES.
    # 4744 -> ids 54090 (127.0.0.1) + 54015 (192.0.2.55), both in-slice.
    # NOTE: finalize flips the ledger record only (§6.4 law 6 — live
    # objects exist and MATCH on backing+uuid); the never-bound second
    # listener is NOT re-applied, and its port shell is removed by the
    # recorded-id teardown law at unshare.
    nqn="nqn.2026-07.io.squeezefs:fideli-wn1"
    zb=$("$SUBSTRATE" mkzram $((512 * 1024 * 1024)) crash-wn1)
    if ip -o addr show 2>/dev/null | grep -q "192\.0\.2\.55"; then
        bad "W-N1: TEST-NET-1 address 192.0.2.55 unexpectedly assigned on this box"
        return
    fi
    out=$("$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1,192.0.2.55 --port 4744 \
        --subnqn "$nqn" --target-stack nvmet 2>&1)
    rc=$?
    echo "$out" > "$STATE/legs/wn1-share.txt"
    if [ "$rc" -ne 0 ]; then
        ok "W-N1: share abandoned mid-apply (EADDRNOTAVAIL on the second listener bind)"
    else
        bad "W-N1: share unexpectedly succeeded"
        return
    fi
    if [ "$(ledger_state_of "$nqn")" = "pending" ]; then
        ok "W-N1: pending intent record claims the partial objects (law 6)"
    else
        bad "W-N1: ledger state"
    fi
    if [ -d "$NVMET_CFS/subsystems/$nqn" ]; then
        ok "W-N1: partial live objects exist (subsystem enabled)"
    else
        bad "W-N1: subsystem missing"
    fi
    out=$("$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port 4650 \
        --subnqn "${nqn}-dup" --target-stack nvmet 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "$nqn"; then
        ok "W-N1: duplicate guard refuses MID-WINDOW naming the pending holder"
    else
        bad "W-N1: mid-window dup guard: $out"
    fi
    out=$("$FIDELI_BIN" nvmeof restore --target-stack nvmet 2>&1)
    if echo "$out" | grep -q "$nqn: pending intent finalized"; then
        ok "W-N1: restore FINALIZED the crash-window intent (live objects match)"
    else
        bad "W-N1: restore: $out"
    fi
    if [ "$(ledger_state_of "$nqn")" = "active" ]; then
        ok "W-N1: record active"
    else
        bad "W-N1: record state"
    fi
    if "$FIDELI_BIN" nvmeof unshare "$nqn" > /dev/null 2>&1; then
        ok "W-N1: unshare accepted + clean"
    else
        bad "W-N1: unshare"
    fi
    if [ ! -d "$NVMET_CFS/ports/54090" ] && [ ! -d "$NVMET_CFS/ports/54015" ]; then
        ok "W-N1: BOTH recorded port ids removed (incl. the never-linked shell — teardown law)"
    else
        bad "W-N1: port residue (54090/54015)"
    fi

    # --- W-N2: pending + NO live objects -> restore GARBAGE-COLLECTS
    # (after the documented removal-first manual wipe of OUR partial
    # residue), sweeping the never-linked port shell with it.
    # 4540 @ 192.0.2.66 -> id 54007.
    nqn="nqn.2026-07.io.squeezefs:fideli-wn2"
    zb=$("$SUBSTRATE" mkzram $((512 * 1024 * 1024)) crash-wn2)
    if ip -o addr show 2>/dev/null | grep -q "192\.0\.2\.66"; then
        bad "W-N2: TEST-NET-1 address 192.0.2.66 unexpectedly assigned on this box"
        return
    fi
    out=$("$FIDELI_BIN" nvmeof share "$zb" --ip 192.0.2.66 --port 4540 \
        --subnqn "$nqn" --target-stack nvmet 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ]; then
        ok "W-N2: share abandoned mid-apply (bogus-only listener)"
    else
        bad "W-N2: share unexpectedly succeeded"
        return
    fi
    if [ "$(ledger_state_of "$nqn")" = "pending" ]; then
        ok "W-N2: pending intent survives"
    else
        bad "W-N2: ledger state"
    fi
    wipe_marked_subsystem "$nqn"
    if [ ! -d "$NVMET_CFS/subsystems/$nqn" ]; then
        ok "W-N2: partial subsystem wiped (removal-first runbook, ours by marker)"
    else
        bad "W-N2: manual wipe failed"
    fi
    out=$("$FIDELI_BIN" nvmeof restore --target-stack nvmet 2>&1)
    if echo "$out" | grep -q "$nqn: pending intent garbage-collected"; then
        ok "W-N2: restore GC'd the pending intent LOUDLY (no live objects)"
    else
        bad "W-N2: restore: $out"
    fi
    if [ -z "$(ledger_state_of "$nqn")" ]; then
        ok "W-N2: record gone"
    else
        bad "W-N2: record residue"
    fi
    if [ ! -d "$NVMET_CFS/ports/54007" ]; then
        ok "W-N2: never-linked port shell swept by the GC teardown"
    else
        bad "W-N2: port 54007 residue"
    fi
}

# ===========================================================================
# Adopt legs (§6.10 pt 5 — full)
# ===========================================================================
configfs_snap() {
    # Zero-target-mutation witness: shape + attr content + symlink targets.
    # Mtimes are deliberately EXCLUDED — configfs re-instantiates attribute
    # inodes lazily and bumps parent dir mtimes on READ walks (the
    # n4b-gate run-1/2 lesson: adopt's own probe moved mtimes without
    # writing anything). Every real mutation still shows: object
    # create/remove = path add/remove, attr write = content change, link
    # change = target change.
    (cd "$NVMET_CFS" && find . \( -type d -printf '%P|d\n' \) -o \
        \( -type l -printf '%P|l|%l\n' \) -o \
        \( -type f -exec sh -c 'printf "%s|f|%s\n" "${1#./}" "$(tr -d "\n" < "$1" 2>/dev/null)"' _ {} \; \) |
        sort) 2>/dev/null
}

leg_adopt() {
    local out rc nqn zb uuid rec port_id dev md5a md5b cfgmd5a cfgmd5b bdev cand

    # --- A1: hand-built PRE-REBUILD-STYLE configfs share (small-int port
    # id, namespaces/1, no ledger) -> adopt WHILE SERVING -> unshare clean
    # including the out-of-range port id.
    nqn="nqn.2026-06.io.squeezefs:subsystem-fidadopt-pre"
    port_id=""
    for cand in 1 2 3 4 5 6 7 8 9; do
        if [ ! -d "$NVMET_CFS/ports/$cand" ]; then
            port_id=$cand
            break
        fi
    done
    [ -n "$port_id" ] || { bad "A1: no free small-int port id"; return; }
    zb=$("$SUBSTRATE" mkzram $((512 * 1024 * 1024)) adopt-pre)
    uuid=$(uuidgen)
    mkdir -p "$NVMET_CFS/subsystems/$nqn/namespaces/1" || { bad "A1: hand-build failed"; return; }
    record "handbuilt_subsystem=$nqn"
    echo 1 > "$NVMET_CFS/subsystems/$nqn/attr_allow_any_host"
    echo "$zb" > "$NVMET_CFS/subsystems/$nqn/namespaces/1/device_path"
    echo "$uuid" > "$NVMET_CFS/subsystems/$nqn/namespaces/1/device_uuid"
    echo 1 > "$NVMET_CFS/subsystems/$nqn/namespaces/1/enable"
    mkdir -p "$NVMET_CFS/ports/$port_id"
    record "handbuilt_port=$port_id"
    echo tcp > "$NVMET_CFS/ports/$port_id/addr_trtype"
    echo ipv4 > "$NVMET_CFS/ports/$port_id/addr_adrfam"
    echo 127.0.0.1 > "$NVMET_CFS/ports/$port_id/addr_traddr"
    echo 4660 > "$NVMET_CFS/ports/$port_id/addr_trsvcid"
    ln -s "$NVMET_CFS/subsystems/$nqn" "$NVMET_CFS/ports/$port_id/subsystems/$nqn"
    ok "A1: pre-rebuild-style share hand-built (small-int port id $port_id)"

    "$FIDELI_BIN" nvmeof connect --ip 127.0.0.1 --port 4660 --subnqn "$nqn" >/dev/null 2>&1
    record "connected=$nqn"
    dev=$(finddev "$nqn") || { bad "A1: no initiator device"; return; }
    dd if=/dev/urandom of="$dev" bs=1M count=8 oflag=direct status=none
    md5a=$(dd if="$dev" bs=1M count=8 iflag=direct status=none | md5sum | awk '{print $1}')
    if "$FIDELI_BIN" nvmeof list --json 2>/dev/null | jq -e \
        ".foreign_live[] | select(.subnqn==\"$nqn\" and .stack==\"nvmet\")" >/dev/null; then
        ok "A1: list shows it foreign (stack=nvmet)"
    else
        bad "A1: foreign list"
    fi

    configfs_snap > "$STATE/legs/a1-cfs-before.txt"
    out=$("$FIDELI_BIN" nvmeof adopt "$nqn" 2>&1)
    rc=$?
    echo "$out" > "$STATE/legs/a1-adopt.txt"
    if [ "$rc" -eq 0 ] && echo "$out" | grep -q "pre-rebuild" && echo "$out" | grep -q "nvmet stack"; then
        ok "A1: adopt absorbed it (stack auto-detected, class pre-rebuild)"
    else
        bad "A1: adopt: $out"
    fi
    configfs_snap > "$STATE/legs/a1-cfs-after.txt"
    if diff -q "$STATE/legs/a1-cfs-before.txt" "$STATE/legs/a1-cfs-after.txt" >/dev/null; then
        ok "A1: ZERO target mutation (configfs shape+content+links identical)"
    else
        bad "A1: configfs mutated across adopt"
    fi
    rec=$(jq -c ".shares[] | select(.subnqn==\"$nqn\")" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json")
    if echo "$rec" | jq -e ".state==\"active\" and .stack==\"nvmet\" and .nsid==null \
        and (.ns_uuid|ascii_downcase)==(\"$uuid\"|ascii_downcase) \
        and .listeners[0].nvmet_port_id==$port_id \
        and .adopted_from.class==\"pre-rebuild\"" >/dev/null; then
        ok "A1: record carries live identity + out-of-range port id as-is + provenance"
    else
        bad "A1: adopted record: $rec"
    fi
    md5b=$(dd if="$dev" bs=1M count=8 iflag=direct status=none | md5sum | awk '{print $1}')
    if [ "$md5a" = "$md5b" ]; then
        ok "A1: zero serving interruption (initiator IO identical across adopt)"
    else
        bad "A1: IO across adopt"
    fi
    out=$("$FIDELI_BIN" nvmeof adopt "$nqn" 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "adopt_already_ledgered"; then
        ok "A1: re-adopt refuses [adopt_already_ledgered]"
    else
        bad "A1: re-adopt: $out"
    fi
    "$FIDELI_BIN" nvmeof disconnect "$nqn" >/dev/null 2>&1
    sleep 1
    "$FIDELI_BIN" nvmeof unshare "$nqn" >/dev/null 2>&1
    if [ ! -d "$NVMET_CFS/subsystems/$nqn" ] && [ ! -d "$NVMET_CFS/ports/$port_id" ]; then
        ok "A1: unshare tore down cleanly INCL. the out-of-range port id (link-free law)"
    else
        bad "A1: unshare residue"
    fi

    # --- A1b: harness-owned refusal against OUR OWN marker (':fideli-' is
    # in HARNESS_NQN_MARKERS — the fidelity fabric must refuse adoption).
    nqn="nqn.2026-07.io.squeezefs:fideli-orphan"
    zb=$("$SUBSTRATE" mkzram $((256 * 1024 * 1024)) adopt-orphan)
    mkdir -p "$NVMET_CFS/subsystems/$nqn/namespaces/1"
    record "handbuilt_subsystem=$nqn"
    echo "$zb" > "$NVMET_CFS/subsystems/$nqn/namespaces/1/device_path"
    echo 1 > "$NVMET_CFS/subsystems/$nqn/namespaces/1/enable"
    out=$("$FIDELI_BIN" nvmeof adopt "$nqn" 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "adopt_harness_owned" && echo "$out" | grep -q ":fideli-"; then
        ok "A1b: adopt of a fidelity-marked NQN refuses [adopt_harness_owned] naming ':fideli-'"
    else
        bad "A1b: harness-owned refusal: $out"
    fi
    wipe_marked_subsystem "$nqn"

    # --- A2: adopt-after-simulated-ledger-loss (§6.10 pt 5). The ledger
    # is snapshotted aside so the substrate's guard records survive the leg.
    cp "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json" "$STATE/legs/ledger-backup-a2.json" ||
        { bad "A2: ledger backup failed"; return; }
    nqn="nqn.2026-07.io.squeezefs:share-fidadopt-loss-nvmet"
    zb=$("$SUBSTRATE" mkzram $((1024 * 1024 * 1024)) adopt-loss-nvmet)
    # 4539 -> id 54060.
    "$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port 4539 --subnqn "$nqn" \
        --target-stack nvmet > "$STATE/legs/a2-share.txt" 2>&1 || { bad "A2: product share failed"; return; }
    uuid=$(jq -r ".shares[] | select(.subnqn==\"$nqn\") | .ns_uuid" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json")
    rm -f "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json"
    if "$FIDELI_BIN" nvmeof list --json 2>/dev/null | jq -e \
        ".foreign_live[] | select(.subnqn==\"$nqn\" and .stack==\"nvmet\")" >/dev/null; then
        ok "A2: orphaned nvmet share shows foreign"
    else
        bad "A2: foreign list"
    fi
    out=$("$FIDELI_BIN" nvmeof adopt "$nqn" 2>&1)
    rc=$?
    if [ "$rc" -eq 0 ] && echo "$out" | grep -q "ledger-loss" && echo "$out" | grep -q "nvmet stack"; then
        ok "A2: adopt absorbed the orphan (class ledger-loss, nvmet)"
    else
        bad "A2: adopt: $out"
    fi
    rec=$(jq -c ".shares[] | select(.subnqn==\"$nqn\")" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json")
    if echo "$rec" | jq -e ".state==\"active\" and .stack==\"nvmet\" \
        and (.ns_uuid|ascii_downcase)==(\"$uuid\"|ascii_downcase) \
        and .listeners[0].nvmet_port_id==54060 \
        and .adopted_from.class==\"ledger-loss\"" >/dev/null; then
        ok "A2: record re-binds identity + the actual serving port id (54060)"
    else
        bad "A2: adopted record: $rec"
    fi
    out=$("$FIDELI_BIN" nvmeof restore --target-stack nvmet 2>&1)
    if echo "$out" | grep -q "$nqn: already live — verified no-op"; then
        ok "A2: restore verified no-op"
    else
        bad "A2: restore: $out"
    fi
    if "$FIDELI_BIN" nvmeof unshare "$nqn" >/dev/null 2>&1; then
        ok "A2: unshare adopted share"
    else
        bad "A2: unshare"
    fi
    if [ ! -d "$NVMET_CFS/subsystems/$nqn" ] && [ ! -d "$NVMET_CFS/ports/54060" ]; then
        ok "A2: zero target residue (subsystem + port gone)"
    else
        bad "A2: residue"
    fi
    mv "$STATE/legs/ledger-backup-a2.json" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json"
}

# ===========================================================================
# PR matrix (pr-matrix.sh productized — full). No PTPL claims: kernel nvmet
# has ptpls=0 by design (reservations do not survive a TARGET restart —
# the heartbeat re-check law covers it); the §6.7 errno contract is what
# this asserts, on the ONE target.
# ===========================================================================
leg_pr_matrix() {
    local out="$STATE/legs/pr-matrix.txt" nqn zb dev dev2 rescap report key1=0xA11CE key2=0xB0B
    local h2uuid="7b7b7b7b-2222-4222-8222-b2b2b2b2b2b2"
    local h2nqn="nqn.2014-08.org.nvmexpress:uuid:$h2uuid"
    : > "$out"

    nqn="nqn.2026-07.io.squeezefs:fideli-prmx-nvmet"
    zb=$("$SUBSTRATE" mkzram $((1024 * 1024 * 1024)) prmx-nvmet)
    "$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port 4555 --subnqn "$nqn" \
        --target-stack nvmet >> "$out" 2>&1 || { bad "PRMX: share failed"; return; }
    record "share=$nqn"
    "$FIDELI_BIN" nvmeof connect --ip 127.0.0.1 --port 4555 --subnqn "$nqn" >> "$out" 2>&1
    record "connected=$nqn"
    dev=$(finddev "$nqn") || { bad "PRMX: no device"; return; }

    rescap=$(nvme id-ns "$dev" -o json | jq -r .rescap)
    if [ $(((rescap >> 1) & 1)) = 1 ]; then
        ok "PRMX: RESCAP carries the Write-Exclusive bit (0x$(printf '%02x' "$rescap"); PTPL bit $((rescap & 1)) — nvmet has none by design)"
    else
        bad "PRMX: rescap=$rescap"
    fi
    report=$(nvme resv-report "$dev" --eds -o json 2>/dev/null | jq -r .regctl)
    if [ "$report" = "0" ]; then
        ok "PRMX: baseline report regctl=0 (fresh namespace)"
    else
        bad "PRMX: baseline regctl=$report"
    fi

    if nvme resv-register "$dev" --nrkey="$key1" --rrega=0 --iekey --cptpl=3 >> "$out" 2>&1; then
        ok "PRMX: host1 register (IEKEY, CPTPL=11b — the guard's shape)"
    else
        bad "PRMX: register"
    fi
    if nvme resv-acquire "$dev" --crkey="$key1" --rtype=1 --racqa=0 >> "$out" 2>&1; then
        ok "PRMX: host1 acquire Write Exclusive"
    else
        bad "PRMX: acquire"
    fi
    if dd if=/dev/zero of="$dev" bs=4096 count=1 oflag=direct conv=notrunc >> "$out" 2>&1; then
        ok "PRMX: holder write lands"
    else
        bad "PRMX: holder write"
    fi

    nvme disconnect -n "$nqn" >> "$out" 2>&1
    sleep 1
    nvme connect -t tcp -a 127.0.0.1 -s 4555 -n "$nqn" --hostnqn="$h2nqn" --hostid="$h2uuid" >> "$out" 2>&1
    dev2=$(finddev "$nqn") || { bad "PRMX: host2 device"; return; }
    report=$(nvme resv-report "$dev2" --eds -o json 2>/dev/null | jq -r .regctl)
    if [ "$report" = "1" ]; then
        ok "PRMX: registration persisted across host1 disconnect (regctl=1)"
    else
        bad "PRMX: post-disconnect regctl=$report"
    fi
    if dd if=/dev/zero of="$dev2" bs=4096 count=1 oflag=direct conv=notrunc >> "$out" 2>&1; then
        bad "PRMX: NON-HOLDER write SUCCEEDED — Write-Exclusive enforcement broken"
    else
        ok "PRMX: non-holder write rejected (the fence — EBADE class)"
    fi
    nvme resv-register "$dev2" --nrkey="$key2" --rrega=0 --iekey --cptpl=3 >> "$out" 2>&1
    if nvme resv-acquire "$dev2" --crkey="$key2" --rtype=1 --racqa=1 --prkey="$key1" >> "$out" 2>&1; then
        ok "PRMX: host2 preempt of the stale holder"
    else
        bad "PRMX: preempt"
    fi
    if dd if=/dev/zero of="$dev2" bs=4096 count=1 oflag=direct conv=notrunc >> "$out" 2>&1; then
        ok "PRMX: preempting host writes (takeover path)"
    else
        bad "PRMX: post-preempt write"
    fi

    {
        nvme resv-release "$dev2" --crkey="$key2" --rtype=1 --rrela=0
        nvme resv-register "$dev2" --crkey="$key2" --rrega=1
    } >> "$out" 2>&1
    report=$(nvme resv-report "$dev2" --eds -o json 2>/dev/null | jq -r .regctl)
    if [ "$report" = "0" ]; then
        ok "PRMX: release + unregister drains to regctl=0"
    else
        bad "PRMX: drain regctl=$report"
    fi
    nvme disconnect -n "$nqn" >> "$out" 2>&1
    sleep 1
    if "$FIDELI_BIN" nvmeof unshare "$nqn" >> "$out" 2>&1; then
        ok "PRMX: unshare"
    else
        bad "PRMX: unshare"
    fi
}

# ===========================================================================
# Target-restart persistence (G2 — full)
# ===========================================================================
leg_g2_nvmet() {
    local out="$STATE/legs/g2-nvmet.txt" nqn zb dev uuid uuid2 md5 md5b io_ok
    nqn="nqn.2026-07.io.squeezefs:fideli-g2-nvmet"
    : > "$out"
    zb=$("$SUBSTRATE" mkzram $((1024 * 1024 * 1024)) g2-nvmet)
    # 4520 -> id 54038.
    "$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port 4520 --subnqn "$nqn" \
        --target-stack nvmet >> "$out" 2>&1 || { bad "G2N: share"; return; }
    record "share=$nqn"
    uuid=$(cat "$NVMET_CFS/subsystems/$nqn/namespaces/1/device_uuid")
    "$FIDELI_BIN" nvmeof connect --ip 127.0.0.1 --port 4520 --subnqn "$nqn" >> "$out" 2>&1
    record "connected=$nqn"
    dev=$(finddev "$nqn") || { bad "G2N: no device"; return; }
    dd if=/dev/urandom of="$STATE/io-src" bs=1M count=16 status=none
    md5=$(md5sum "$STATE/io-src" | awk '{print $1}')
    dd if="$STATE/io-src" of="$dev" bs=1M oflag=direct status=none || bad "G2N: pre-wipe write"

    # The nvmet target-restart simulation (G2): configfs wiped under a
    # LIVE initiator connection; `restore` must re-present the SAME
    # identity and the initiator must reattach without operator action.
    wipe_marked_subsystem "$nqn"
    if [ ! -d "$NVMET_CFS/subsystems/$nqn" ]; then
        ok "G2N: configfs wiped under a live connection (target power-cut analog)"
    else
        bad "G2N: wipe failed"
        return
    fi
    if "$FIDELI_BIN" nvmeof list --json 2>/dev/null | jq -e \
        ".shares[] | select(.subnqn==\"$nqn\") | .classification | test(\"down\")" >/dev/null; then
        ok "G2N: list shows the record down (restore candidate)"
    else
        bad "G2N: down classification"
    fi
    if "$FIDELI_BIN" nvmeof restore --target-stack nvmet > "$STATE/legs/g2n-restore.txt" 2>&1 &&
        grep -q "$nqn: restored" "$STATE/legs/g2n-restore.txt"; then
        ok "G2N: restore replayed the wiped share"
    else
        bad "G2N: restore: $(cat "$STATE/legs/g2n-restore.txt")"
    fi
    uuid2=$(cat "$NVMET_CFS/subsystems/$nqn/namespaces/1/device_uuid" 2>/dev/null)
    if [ "$uuid2" = "$uuid" ]; then
        ok "G2N: device_uuid re-presented VERBATIM ($uuid)"
    else
        bad "G2N: uuid changed across restore ($uuid2)"
    fi
    io_ok=""
    for _ in $(seq 1 60); do
        md5b=$(dd if="$dev" bs=1M count=16 iflag=direct status=none 2>/dev/null | md5sum | awk '{print $1}')
        [ "$md5b" = "$md5" ] && { io_ok=1; break; }
        sleep 2
    done
    if [ -n "$io_ok" ]; then
        ok "G2N: connected initiator REATTACHED without operator action (data intact)"
    else
        bad "G2N: reattach (md5=$md5b want $md5)"
    fi
    "$FIDELI_BIN" nvmeof restore --target-stack nvmet > "$STATE/legs/g2n-restore2.txt" 2>&1
    if grep -q "$nqn: already live — verified no-op" "$STATE/legs/g2n-restore2.txt"; then
        ok "G2N: second restore is a verified no-op (idempotent)"
    else
        bad "G2N: idempotent restore"
    fi
    "$FIDELI_BIN" nvmeof disconnect "$nqn" >> "$out" 2>&1
    sleep 1
    if "$FIDELI_BIN" nvmeof unshare "$nqn" >> "$out" 2>&1; then
        ok "G2N: unshare"
    else
        bad "G2N: unshare"
    fi
}

# ===========================================================================
# Soft-RoCE plumbing leg (rdma_rxe; Resolved Questions #5 — full)
#
# PLUMBING VALIDATION ONLY — soft-RoCE is explicitly NOT representative of
# real RNIC behavior; no guard or perf claims ride this leg.
#
# Branch taken (stated per the PR-5 tasking): the product's listener
# plumbing cannot express trtype=rdma yet — src/nvmeof/nvmet.rs pins
# addr_trtype="tcp" (write_attr at ensure_port) and `share` exposes no
# --trtype.
# The leg therefore validates kernel-initiator RDMA connect + IO against
# a HARNESS-built rxe listener on the PRODUCT-shared subsystem, and
# prints the product-verb gap as a named residual for PR 7.
#
# rxe attach point: OQ5 says "rdma_rxe over loopback", so the leg tries
# lo FIRST; where the kernel refuses it (predecessor probe on 7.1.3: the
# rxe device binds to lo and the nvmet rdma listener arms, but the
# initiator's rdma_resolve_route times out, -110), it falls back to the
# default-route netdev connecting to the host's OWN address — packets
# still never leave the box. The branch actually taken is logged.
# ===========================================================================
softroce_listener_up() { # port_id traddr -> 0 when the rdma listener armed
    local port_id=$1 traddr=$2 nqn=$3 out=$4
    mkdir "$NVMET_CFS/ports/$port_id" 2>>"$out" || return 1
    record "handbuilt_port=$port_id"
    echo rdma > "$NVMET_CFS/ports/$port_id/addr_trtype"
    echo ipv4 > "$NVMET_CFS/ports/$port_id/addr_adrfam"
    echo "$traddr" > "$NVMET_CFS/ports/$port_id/addr_traddr"
    echo 4544 > "$NVMET_CFS/ports/$port_id/addr_trsvcid"
    ln -s "$NVMET_CFS/subsystems/$nqn" "$NVMET_CFS/ports/$port_id/subsystems/$nqn" 2>>"$out"
}

softroce_listener_down() { # port_id nqn
    rm -f "$NVMET_CFS/ports/$1/subsystems/$2" 2>/dev/null
    rmdir "$NVMET_CFS/ports/$1" 2>/dev/null
}

leg_softroce() {
    local out="$STATE/legs/softroce.txt" nqn zb m netdev ipaddr dev md5s md5r ctrl
    local branch="" traddr="" port_id=""
    : > "$out"
    for m in rdma_rxe nvmet_rdma nvme_rdma; do
        if ! lsmod | awk '{print $1}' | grep -qx "$m"; then
            modprobe "$m" 2>>"$out" || { bad "RXE: modprobe $m failed"; return; }
            record "module_loaded=$m"
            log "RXE: loaded module $m (was not loaded; stays loaded by policy)"
        fi
    done

    # Product-shared subsystem (TCP listener via the verbs, 4543 -> 54013).
    nqn="nqn.2026-07.io.squeezefs:fideli-rxe"
    zb=$("$SUBSTRATE" mkzram $((1024 * 1024 * 1024)) rxe)
    "$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port 4543 --subnqn "$nqn" \
        --target-stack nvmet >> "$out" 2>&1 || { bad "RXE: product share failed"; return; }
    record "share=$nqn"

    # Attempt 1: rxe over loopback (the OQ5 spelling).
    if rdma link add fideli_rxe0 type rxe netdev lo 2>>"$out"; then
        record "rdma_link=fideli_rxe0"
        if softroce_listener_up 54099 127.0.0.1 "$nqn" "$out" &&
            timeout 30 nvme connect -t rdma -a 127.0.0.1 -s 4544 -n "$nqn" >> "$out" 2>&1; then
            branch="lo"
            traddr=127.0.0.1
            port_id=54099
        else
            log "RXE: rxe-on-lo attempt did not connect (expected on kernels where rdma_resolve_route refuses lo) — falling back to the default-route netdev"
            nvme disconnect -n "$nqn" >> "$out" 2>&1
            softroce_listener_down 54099 "$nqn"
            rdma link del fideli_rxe0 2>>"$out"
        fi
    else
        log "RXE: rdma link add on lo refused — falling back to the default-route netdev"
    fi

    # Attempt 2: rxe on the default-route netdev, connecting to the host's
    # OWN address (local-only; packets never leave the box).
    if [ -z "$branch" ]; then
        netdev=$(ip -o route get 1.1.1.1 2>/dev/null | sed -n 's/.* dev \([^ ]*\) .*/\1/p')
        ipaddr=$(ip -o -4 addr show dev "$netdev" scope global 2>/dev/null | head -1 | awk '{print $4}' | cut -d/ -f1)
        if [ -z "$netdev" ] || [ -z "$ipaddr" ]; then
            bad "RXE: no default-route IPv4 netdev — cannot arm the rxe leg on this box"
            return
        fi
        rdma link add fideli_rxe0 type rxe netdev "$netdev" 2>>"$out" ||
            { bad "RXE: rdma link add on $netdev failed"; return; }
        record "rdma_link=fideli_rxe0"
        if softroce_listener_up 54098 "$ipaddr" "$nqn" "$out" &&
            timeout 30 nvme connect -t rdma -a "$ipaddr" -s 4544 -n "$nqn" >> "$out" 2>&1; then
            branch="netdev:$netdev"
            traddr="$ipaddr"
            port_id=54098
        else
            bad "RXE: rdma connect failed on both lo and $netdev"
            softroce_listener_down 54098 "$nqn"
            rdma link del fideli_rxe0 2>>"$out"
            "$FIDELI_BIN" nvmeof unshare "$nqn" >> "$out" 2>&1
            return
        fi
    fi
    record "connected=$nqn"
    ok "RXE: soft-RoCE armed + kernel-initiator NVMe/RDMA connect (branch: $branch, traddr $traddr, harness port id $port_id)"

    dev=$(finddev "$nqn") || { bad "RXE: no rdma device"; return; }
    ctrl=$(basename "${dev%n1}")
    if [ "$(cat "/sys/class/nvme/$ctrl/transport" 2>/dev/null)" = "rdma" ]; then
        ok "RXE: controller transport is rdma ($ctrl)"
    else
        bad "RXE: transport of $ctrl"
    fi
    dd if=/dev/urandom of="$STATE/io-src" bs=1M count=32 status=none
    md5s=$(md5sum "$STATE/io-src" | awk '{print $1}')
    dd if="$STATE/io-src" of="$dev" bs=1M oflag=direct status=none || bad "RXE: rdma write"
    md5r=$(dd if="$dev" bs=1M count=32 iflag=direct status=none | md5sum | awk '{print $1}')
    if [ "$md5r" = "$md5s" ]; then
        ok "RXE: 32 MiB O_DIRECT round-trip over NVMe/RDMA (rxe)"
    else
        bad "RXE: rdma IO md5"
    fi

    nvme disconnect -n "$nqn" >> "$out" 2>&1
    sleep 1
    softroce_listener_down "$port_id" "$nqn"
    if [ ! -d "$NVMET_CFS/ports/$port_id" ]; then
        ok "RXE: harness rdma port removed"
    else
        bad "RXE: port residue"
    fi
    if rdma link del fideli_rxe0 2>>"$out"; then
        ok "RXE: rxe device removed (modules stay loaded by policy)"
    else
        bad "RXE: rdma link residue"
    fi
    if "$FIDELI_BIN" nvmeof unshare "$nqn" >> "$out" 2>&1; then
        ok "RXE: product unshare"
    else
        bad "RXE: unshare"
    fi

    log "RXE RESIDUAL (PR 7): product listener plumbing cannot express trtype=rdma — 'nvmeof share' has no --trtype, src/nvmeof/nvmet.rs pins addr_trtype=tcp; this leg's rdma listener is harness-built on the product-shared subsystem."
    log "RXE NOTE: not representative of real RNIC behavior (software RoCE over the local stack); no guard/perf claims ride this leg."
}

# ===========================================================================
# A/B smoke rows (recorded, NOT ordered — full)
# ===========================================================================
leg_ab_smoke() {
    local out="$STATE/legs/ab-smoke.txt"
    : > "$out"
    if ! command -v fio >/dev/null; then
        log "AB: fio not installed — rows SKIPPED loudly (install fio for the nightly A/B smoke)"
        ok "AB: skip recorded (fio absent)"
        return
    fi
    # shellcheck disable=SC1091 # generated by nvmeof_target_substrate.sh create
    . "$STATE/devices.env"
    # Rides the raw guard-data namespace BEFORE the guard leg (which
    # re-wipes + reformats it at its leg 0/1). Recorded rows only —
    # ordered rows belong to the bench rerun. Instrument: fio io_uring,
    # O_DIRECT, railed to cores 0-15. Engine policy note (2026-08-07,
    # `.benchmarks/2026-08-07-fio-engine-policy.md` rule 5): io_uring is
    # the sanctioned KERNEL-LANE raw-device instrument — these rows never
    # ride the shim (which cannot interpose io_uring) and are labeled
    # raw-ceiling rows, not FUSE-lane numbers.
    local arm=nvmet dev="$DEV_GDATA_NVMET" row iops
    for row in randwrite randread; do
        fio --name="ab-$arm-$row" --filename="$dev" --rw="$row" --bs=4k --iodepth=32 \
            --ioengine=io_uring --direct=1 --runtime=10 --time_based --size=1G \
            --cpus_allowed=0-15 --output-format=json > "$STATE/legs/ab-$arm-$row.json" 2>>"$out"
        if [ "$row" = randread ]; then
            iops=$(jq -r '.jobs[0].read.iops | floor' "$STATE/legs/ab-$arm-$row.json" 2>/dev/null)
        else
            iops=$(jq -r '.jobs[0].write.iops | floor' "$STATE/legs/ab-$arm-$row.json" 2>/dev/null)
        fi
        if [ -n "${iops:-}" ] && [ "${iops:-0}" -gt 0 ]; then
            ok "AB: $arm rand4k QD32 $row = $iops IOPS (recorded row, fio io_uring O_DIRECT)"
        else
            bad "AB: $arm $row produced no IO"
        fi
    done
}

# ===========================================================================
# Guard leg (tests/guard_smoke.sh — quick: 1 cycle; full: x10)
# ===========================================================================
leg_guard_nvmet() {
    local loops=$1
    if "$GUARD_SMOKE" --stack nvmet --loops "$loops" >> "$LOG" 2>&1; then
        ok "guard smoke nvmet (loops=$loops) GREEN — transcript $STATE/guard-smoke-nvmet.txt"
    else
        bad "guard smoke nvmet FAILED — see $STATE/guard-smoke-nvmet.txt"
    fi
}

# ===========================================================================
# Substrate legs
# ===========================================================================
leg_substrate_up() {
    if "$SUBSTRATE" create >> "$LOG" 2>&1; then
        ok "substrate up (product-verb-driven, kernel nvmet)"
    else
        bad "substrate create FAILED"
        exit 1
    fi
    # shellcheck disable=SC1091 # generated by nvmeof_target_substrate.sh create
    . "$STATE/env.sh"
    mkdir -p "$STATE/legs"
}

TEARDOWN_DONE=0
leg_substrate_down() {
    TEARDOWN_DONE=1
    if "$SUBSTRATE" teardown >> "$LOG" 2>&1; then
        ok "teardown to ZERO residue (before/after snapshot diff empty)"
    else
        bad "teardown reported residue — see $STATE/residue.diff"
    fi
}

emergency_teardown() {
    [ "$TEARDOWN_DONE" = 1 ] && return
    log "emergency teardown (run aborted)"
    "$SUBSTRATE" teardown >> "$LOG" 2>&1 || true
}

# ===========================================================================
# main
# ===========================================================================
main() {
    mkdir -p "$STATE"
    LOG="$STATE/fidelity-$MODE.log"
    : > "$LOG"
    trap emergency_teardown EXIT
    local t_start t_end line
    t_start=$(date +%s)
    log "=== NVMe-oF fidelity tier (kernel nvmet — THE target, R-SYM-8): $MODE @ $(date -Is) ==="
    log "binary: ${FIDELI_SQZ_BIN:-$REPO/target/release/squeezefs} ($(md5sum "${FIDELI_SQZ_BIN:-$REPO/target/release/squeezefs}" | awk '{print $1}'))"
    log "Tctl: $(tctl || echo n/a)°C"

    run_leg substrate-up leg_substrate_up

    run_leg roundtrip-nvmet leg_roundtrip_nvmet

    if [ "$MODE" = full ]; then
        run_leg loud-fail-matrix leg_loudfail
        run_leg crash-window-nvmet leg_crash_nvmet
        run_leg adopt leg_adopt
        run_leg pr-matrix leg_pr_matrix
        run_leg g2-persistence-nvmet leg_g2_nvmet
        run_leg soft-roce leg_softroce
        run_leg ab-smoke leg_ab_smoke
        run_leg guard-nvmet-x10 leg_guard_nvmet 10
    else
        run_leg guard-nvmet-x1 leg_guard_nvmet 1
    fi

    run_leg teardown-zero-residue leg_substrate_down

    t_end=$(date +%s)
    log "=== leg summary ==="
    for line in "${SUMMARY[@]}"; do log "  $line"; done
    log "Tctl: $(tctl || echo n/a)°C"
    log "=== fidelity $MODE result: PASS=$PASS FAIL=$FAIL in $(((t_end - t_start) / 60))m$(((t_end - t_start) % 60))s ==="
    [ "$FAIL" = 0 ] || exit 1
    exit 0
}

main
