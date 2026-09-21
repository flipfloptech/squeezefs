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
# residue-free) + the symmetric PR 3 legs (`pr-registrants`: ≥ 64 real
# registrants on one namespace REPORTED through the product's REGCTL-sized
# read — the shipped 4 KiB-buffer truncation repro-ported;
# `sym-manager-failover`: a bit-17 volume's manager holds WERO rtype 3,
# dies by kill -9, the successor wins inside `manager_failover_bound_ms`,
# acked data intact, zero residue; `sym-join-ladder` (PR 12): `format
# --symmetric` + the ARMED mount through the join ladder — the device
# reporting rtype 3 on BOTH namespaces, the retired posture knob refused at
# the startup gate, 200 creates at dlm_rpcs=0, a `--read-only` token reader
# seeing a foreign create exactly at its next resolve, the successor
# re-walking the ladder, zero residue on both namespaces) + ONE guard kill-9
# cycle.
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
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "SQUEEZEFS_NVMEOF_TARGET_STACK" && echo "$out" | grep -q "RETIRED" &&
        echo "$out" | grep -q "nvmeof unshare" && echo "$out" | grep -q -- "--target-stack nvmet"; then
        ok "SQUEEZEFS_NVMEOF_TARGET_STACK=spdk refuses at startup as a RETIRED value naming nvmet + the re-share sequence"
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
# Symmetric PR 3 — the registrant ceiling (KD-SYM-18/23; quick + full)
# ===========================================================================
# The shipped `reservation.rs` read the Reservation Report into a FIXED
# 4 KiB buffer: 64 B header + 63 × 64 B extended registrants fit, the 64th
# was silently dropped — S7's rung-5 check would mis-report the 64th
# co-writer. PR 3 sizes the read by REGCTL (header first, then
# `64 + 64 × REGCTL`). This leg puts FIDELI_PR_REGISTRANTS (default 128)
# real registrants on one nvmet namespace — one host identity each,
# through the PRODUCT connect path (`nvmeof connect --hostnqn/--hostid`),
# registered per controller through its char device (the association
# pin on native-multipath kernels) — and reads the report back through
# the product's own read (`squeezefs nvmeof resv-report`): every one must
# be REPORTED and the transfer must be exactly `64 + 64 × N` bytes. nvmet
# has no registrant cap, so `registrant_cap` reads unbounded (0) and the
# join refusal is unreachable here by construction.
leg_pr_registrants() {
    local out="$STATE/legs/pr-registrants.txt" nqn zb head n want i ctrl uuid hostnqn registered
    local report regctl bytes cap json_regctl cli_regctl
    want="${FIDELI_PR_REGISTRANTS:-128}"
    : > "$out"
    nqn="nqn.2026-07.io.squeezefs:fideli-prreg-nvmet"
    zb=$("$SUBSTRATE" mkzram $((256 * 1024 * 1024)) prreg-nvmet)
    "$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port 4556 --subnqn "$nqn" \
        --target-stack nvmet >> "$out" 2>&1 || { bad "PRREG: share failed"; return; }
    record "share=$nqn"
    if [ "$(cat "$NVMET_CFS/subsystems/$nqn/namespaces/1/resv_enable" 2>/dev/null)" = "1" ]; then
        ok "PRREG: share live with resv_enable=1"
    else
        bad "PRREG: resv_enable is not 1"
    fi

    # N identities, N controllers (one I/O queue each — the leg measures
    # the reservation table, not the fabric), N registrations.
    local t0 t1
    t0=$(date +%s%3N)
    n=0
    for i in $(seq 1 "$want"); do
        uuid=$(printf '7c7c7c7c-%04x-4%03x-8%03x-c3c3c3c3%04x' "$i" "$((i % 4096))" "$((i % 4096))" "$i")
        hostnqn="nqn.2014-08.org.nvmexpress:uuid:$uuid"
        if ! "$FIDELI_BIN" nvmeof connect --ip 127.0.0.1 --port 4556 --subnqn "$nqn" \
            --hostnqn "$hostnqn" --hostid "$uuid" --nr-io-queues 1 >> "$out" 2>&1; then
            bad "PRREG: connect $i failed"
            break
        fi
        # The controller of THIS identity, LIVE (its char node exists before
        # the association is up — a register on a connecting controller is
        # ENOTTY), then its namespace attached.
        ctrl=""
        for _ in $(seq 1 100); do
            for c in /sys/class/nvme/nvme*; do
                [ -e "$c/subsysnqn" ] || continue
                [ "$(cat "$c/subsysnqn" 2>/dev/null)" = "$nqn" ] || continue
                [ "$(cat "$c/hostnqn" 2>/dev/null)" = "$hostnqn" ] || continue
                [ "$(cat "$c/state" 2>/dev/null)" = "live" ] || continue
                ctrl=$(basename "$c")
                break
            done
            # nsid 1 attached on THIS controller: `nvme<subsys>c<ctrl>n1`
            # (native multipath) or `nvme<ctrl>n1`.
            [ -n "$ctrl" ] && [ -c "/dev/$ctrl" ] && ls "/sys/class/nvme/$ctrl/" | grep -q 'n1$' && break
            ctrl=""
            sleep 0.1
        done
        [ -n "$ctrl" ] || { bad "PRREG: no live controller for identity $i"; break; }
        registered=0
        for _ in $(seq 1 20); do
            if nvme resv-register "/dev/$ctrl" -n 1 --nrkey="$((0xC0DE0000 + i))" --rrega=0 --cptpl=0 >> "$out" 2>&1; then
                registered=1
                break
            fi
            sleep 0.1
        done
        if [ "$registered" = 1 ]; then
            n=$((n + 1))
        else
            bad "PRREG: register $i failed on /dev/$ctrl"
            break
        fi
    done
    record "connected=$nqn"
    t1=$(date +%s%3N)
    log "PRREG: $n registrant(s) established in $((t1 - t0)) ms ($(( (t1 - t0) / (n > 0 ? n : 1) )) ms each)"
    if [ "$n" -ge 64 ]; then
        ok "PRREG: $n registrants on one namespace (≥ 64 — past the shipped 4 KiB buffer's 63)"
    else
        bad "PRREG: only $n registrants established (need ≥ 64 to exercise the truncation)"
    fi

    head=$(finddev "$nqn") || { bad "PRREG: no head node"; return; }
    cli_regctl=$(nvme resv-report "$head" --eds -o json 2>/dev/null | jq -r .regctl)
    report=$("$FIDELI_BIN" nvmeof resv-report "$head" --json 2>> "$out") || { bad "PRREG: product resv-report failed"; return; }
    echo "$report" >> "$out"
    regctl=$(echo "$report" | jq -r .regctl)
    bytes=$(echo "$report" | jq -r .report_bytes)
    cap=$(echo "$report" | jq -r .registrant_cap)
    json_regctl=$(echo "$report" | jq -r '.registrants | length')
    if [ "$regctl" = "$n" ] && [ "$json_regctl" = "$n" ]; then
        ok "PRREG: the product's REGCTL-sized read REPORTS all $n registrants (nvme-cli regctl=$cli_regctl)"
    else
        bad "PRREG: product read regctl=$regctl (decoded $json_regctl) vs $n established (nvme-cli $cli_regctl) — the truncation"
    fi
    if [ "$bytes" = "$((64 + 64 * n))" ]; then
        ok "PRREG: report transfer = 64 + 64 × $n = $bytes B (pr_report_bytes)"
    else
        bad "PRREG: report transfer $bytes B ≠ $((64 + 64 * n))"
    fi
    if [ "$cap" = "0" ]; then
        ok "PRREG: registrant cap in force = unbounded (nvmet has none — refusals unreachable by construction)"
    else
        bad "PRREG: registrant cap $cap on nvmet"
    fi

    # Drain: every registrant's own controller unregisters its own key (a
    # controller may only drop what its association registered — the
    # unregister names the key, the device matches it to the host id), then
    # disconnect and unshare.
    i=0
    for c in /sys/class/nvme/nvme*; do
        [ -e "$c/subsysnqn" ] || continue
        [ "$(cat "$c/subsysnqn" 2>/dev/null)" = "$nqn" ] || continue
        ctrl=$(basename "$c")
        hostnqn=$(cat "$c/hostnqn" 2>/dev/null)
        i=$(printf '%s' "$hostnqn" | sed -n 's/.*uuid:7c7c7c7c-\([0-9a-f]*\)-.*/\1/p')
        [ -n "$i" ] || continue
        nvme resv-register "/dev/$ctrl" -n 1 --crkey="$((0xC0DE0000 + 0x$i))" --rrega=1 >> "$out" 2>&1 || true
    done
    cli_regctl=$(nvme resv-report "$head" --eds -o json 2>/dev/null | jq -r .regctl)
    if [ "$cli_regctl" = "0" ]; then
        ok "PRREG: drained to regctl=0"
    else
        bad "PRREG: residue regctl=$cli_regctl after the drain"
    fi
    "$FIDELI_BIN" nvmeof disconnect "$nqn" >> "$out" 2>&1
    for _ in $(seq 1 60); do
        ls /sys/class/nvme/nvme*/subsysnqn 2>/dev/null | xargs -r grep -l "^$nqn$" >/dev/null 2>&1 || break
        sleep 0.5
    done
    if "$FIDELI_BIN" nvmeof unshare "$nqn" >> "$out" 2>&1; then
        ok "PRREG: unshare"
    else
        bad "PRREG: unshare"
    fi
}

# ===========================================================================
# Symmetric PR 3 — the manager lease on a REAL PR target (quick + full)
# ===========================================================================
# design-symmetric-metadata §5.8.1 / §5.9: a bit-17 volume (stamped through
# the seam) mounted with the symmetric plane ARMED (a declared partition —
# PR 4's lease gate stands in) on the fidelity guard namespaces: the writer
# that wins the D0 ladder is the manager (`manager_lease` = held), holds the
# metadata namespace under WERO rtype 3 (`meta_pr_wero` = 1; the DEVICE
# reports the type), the must-stay-0 set is 0; kill -9 the manager, the
# same-host successor wins the ladder inside `manager_failover_bound_ms`
# (the register ladder reclaims the dead incarnation's key on the strict
# nvmet target), reads `held` again, and acked data is byte-intact; a clean
# unmount leaves regctl=0 and page 0 Free.
leg_sym_manager_failover() {
    local out="$STATE/legs/sym-manager-failover.txt" dlog="$STATE/sym-manager-daemon.log"
    local mnt="$STATE/mnt-sym-manager" meta data rt lease wero mode md5 md5b t0 t1 bound pid
    local refusals stalls kv lv ev rtype2 reg acked refused listed
    : > "$out"
    : > "$dlog"
    # shellcheck disable=SC1091 # generated by nvmeof_target_substrate.sh create
    . "$STATE/devices.env"
    meta=$(finddev "$NQN_GMETA_NVMET") || { bad "SYMMGR: no meta device"; return; }
    data=$(finddev "$NQN_GDATA_NVMET") || { bad "SYMMGR: no data device"; return; }
    mkdir -p "$mnt"
    pkill -f "squeezefs.*mount sqmeta://$meta" && sleep 2
    umount -l "$mnt" 2>/dev/null
    for k in $(nvme resv-report "$meta" --eds -o json 2>/dev/null | jq -r '.regctlext[]?.rkey'); do
        nvme resv-register "$meta" --crkey="$k" --rrega=1 >> "$out" 2>&1
    done
    dd if=/dev/zero of="$meta" bs=1M count=16 oflag=direct status=none
    dd if=/dev/zero of="$data" bs=1M count=16 oflag=direct status=none

    # A bit-17 volume: the seam at format (PR 11's `format --symmetric` is
    # not built), the partition seam at mount (PR 4's lease gate).
    if SQUEEZEFS_TEST_STAMP_SYMMETRIC=1 "$FIDELI_BIN" format "sqmeta://$meta" "sqdata://$data" --force >> "$out" 2>&1; then
        ok "SYMMGR: stamped format (bit 17 through the seam)"
    else
        bad "SYMMGR: format failed"
        return
    fi
    mount_sym() {
        udevadm settle --timeout=10 2>/dev/null || true
        SQUEEZEFS_TEST_SYM_APPENDER_SLOTS="1:4" RUST_LOG=info \
            "$FIDELI_BIN" --log-file "$dlog" mount "sqmeta://$meta" "$mnt" --daemon --allow-other >> "$out" 2>&1
    }
    wait_sym_mounted() {
        local i
        for i in $(seq 1 60); do
            awk -v m="$mnt" '$2==m{f=1} END{exit !f}' /proc/mounts && return 0
            sleep 0.5
        done
        return 1
    }
    t0=$(date +%s%3N)
    mount_sym
    wait_sym_mounted || { bad "SYMMGR: initial mount did not appear — $(tail -5 "$out")"; return; }
    t1=$(date +%s%3N)
    sleep 2
    mode=$(mnt_stat "$mnt" writer_guard_mode)
    lease=$(mnt_stat "$mnt" manager_lease)
    wero=$(mnt_stat "$mnt" meta_pr_wero)
    bound=$(mnt_stat "$mnt" manager_failover_bound_ms)
    log "SYMMGR: mount wall $((t1 - t0)) ms; writer_guard_mode=$mode manager_lease=$lease meta_pr_wero=$wero manager_failover_bound_ms=$bound"
    [ "$mode" = "flock+pr" ] && ok "SYMMGR: writer_guard_mode=flock+pr" || bad "SYMMGR: writer_guard_mode=$mode"
    [ "$lease" = "held" ] && ok "SYMMGR: manager_lease=held (the D0 winner is the manager)" || bad "SYMMGR: manager_lease=$lease"
    [ "$wero" = "1" ] && ok "SYMMGR: meta_pr_wero=1 (rtype 3 on the metadata namespace)" || bad "SYMMGR: meta_pr_wero=$wero"
    rtype2=$(nvme resv-report "$meta" --eds -o json 2>/dev/null | jq -r .rtype)
    if [ "$rtype2" = "3" ]; then
        ok "SYMMGR: the DEVICE reports rtype 3 (Write Exclusive – Registrants Only) held"
    else
        bad "SYMMGR: device rtype=$rtype2 (want 3)"
    fi
    rt=$("$FIDELI_BIN" nvmeof resv-report "$meta" --json 2>> "$out") && echo "$rt" >> "$out"
    if [ "$(echo "$rt" | jq -r .wero)" = "true" ]; then
        ok "SYMMGR: the product's report reads WERO held ($(echo "$rt" | jq -r .regctl) registrant(s))"
    else
        bad "SYMMGR: product report wero=$(echo "$rt" | jq -r .wero)"
    fi
    [ "${bound:-0}" -gt 45000 ] && ok "SYMMGR: manager_failover_bound_ms=$bound (> the 45 s stale TTL — DERIVED)" || bad "SYMMGR: bound=$bound"

    dd if=/dev/urandom of="$mnt/manager.bin" bs=1M count=8 status=none
    sync
    md5=$(md5sum "$mnt/manager.bin" | cut -d' ' -f1)
    # 200 creates. Under the DECLARED partition seam a create whose minted
    # ino the rotor routes to appender 1's slot spans two appenders (the
    # dentry is the parent's slot, the inode the child's) and is REFUSED
    # loud as the PR-6 cross-owner class — the seam's documented shape
    # (PR 4's lease gate and PR 6's cross-owner op replace it). Only the
    # ACKED creates are the successor's contract; the refused count is the
    # row (the rotor spreads ≈ 1/64 of the mints onto the leased slot).
    acked=0
    refused=0
    for i in $(seq 1 200); do
        if echo "sym $i" > "$mnt/f$i" 2>> "$out"; then
            acked=$((acked + 1))
        else
            refused=$((refused + 1))
            rm -f "$mnt/f$i" 2>/dev/null
        fi
    done
    sync
    log "SYMMGR: creates acked=$acked refused=$refused (the partition seam's PR-6 cross-owner class)"
    [ "$acked" -ge 150 ] && ok "SYMMGR: $acked of 200 creates acked under the seam ($refused refused loud — the rotor's share of the leased slot)" || bad "SYMMGR: only $acked creates acked"
    if grep -q "cross-appender mutation is a cross-owner operation" "$dlog"; then
        ok "SYMMGR: the refusals name the PR-6 cross-owner class"
    elif [ "$refused" -gt 0 ]; then
        bad "SYMMGR: $refused creates refused without the cross-owner class named"
    fi
    refusals=$(mnt_stat "$mnt" manager_verb_refusals)
    stalls=$(mnt_stat "$mnt" manager_dependency_stalls)
    kv=$(mnt_stat "$mnt" meta_kv_replay_key_violations)
    lv=$(mnt_stat "$mnt" meta_kv_replay_lease_violations)
    ev=$(mnt_stat "$mnt" meta_kv_replay_extent_violations)
    if [ "$refusals" = "0" ] && [ "$stalls" = "0" ] && [ "$kv" = "0" ] && [ "$lv" = "0" ] && [ "$ev" = "0" ]; then
        ok "SYMMGR: must-stay-0 set clean under load (refusals/stalls/key/lease/extent = 0)"
    else
        bad "SYMMGR: refusals=$refusals stalls=$stalls key=$kv lease=$lv extent=$ev"
    fi

    # The manager dies. The successor is this host (the D0 same-host arm:
    # flock reclaims instantly, the strict target's stale key rides the
    # register ladder), so its wall is far inside the cross-host bound —
    # both numbers are the row.
    pid=$(pgrep -f "squeezefs.*mount sqmeta://$meta $mnt " | head -1)
    [ -n "$pid" ] || { bad "SYMMGR: no daemon pid"; return; }
    t0=$(date +%s%3N)
    kill -9 "$pid"
    sleep 1
    umount -l "$mnt" 2>/dev/null
    mount_sym
    if wait_sym_mounted; then
        t1=$(date +%s%3N)
        sleep 2
        lease=$(mnt_stat "$mnt" manager_lease)
        md5b=$(md5sum "$mnt/manager.bin" 2>/dev/null | cut -d' ' -f1)
        log "SYMMGR: successor wall (kill → mounted) $((t1 - t0)) ms against bound $bound ms"
        if [ "$lease" = "held" ]; then
            ok "SYMMGR: the successor holds the manager lease"
        else
            bad "SYMMGR: successor manager_lease=$lease"
        fi
        if [ "$((t1 - t0))" -lt "${bound:-1}" ]; then
            ok "SYMMGR: successor inside manager_failover_bound_ms ($((t1 - t0)) < $bound)"
        else
            bad "SYMMGR: successor took $((t1 - t0)) ms ≥ bound $bound"
        fi
        [ "$md5b" = "$md5" ] && ok "SYMMGR: acked data byte-intact across the manager's death" || bad "SYMMGR: md5 $md5b != $md5"
        listed=$(ls "$mnt" | grep -c '^f')
        [ "$listed" = "$acked" ] && ok "SYMMGR: every acked name ($acked) served by the successor" || bad "SYMMGR: names lost — $listed listed of $acked acked"
        [ "$(mnt_stat "$mnt" appender_self_recoveries)" -ge 1 ] && ok "SYMMGR: own-residue recovery counted (appender_self_recoveries ≥ 1)" || bad "SYMMGR: no self recovery counted"
        [ "$(mnt_stat "$mnt" meta_pr_wero)" = "1" ] && ok "SYMMGR: WERO re-held by the successor" || bad "SYMMGR: successor meta_pr_wero != 1"
    else
        bad "SYMMGR: successor mount did not appear — $(tail -20 "$dlog")"
        return
    fi

    umount "$mnt" >> "$out" 2>&1
    sleep 1
    reg=$(nvme resv-report "$meta" --eds -o json 2>/dev/null | jq -r .regctl)
    [ "${reg:-x}" = "0" ] && ok "SYMMGR: zero PR residue after the clean unmount (regctl=0)" || bad "SYMMGR: residue regctl=$reg"
    if "$FIDELI_BIN" appenders "sqmeta://$meta" --json >> "$out" 2>&1; then
        ok "SYMMGR: appenders probe lists the directory after the clean unmount"
    else
        bad "SYMMGR: appenders probe failed"
    fi
}

# ---------------------------------------------------------------------------
# Symmetric PR 12 — the join ladder on the real PR target
# (docs/design-symmetric-metadata.md §7.3 / §5.1.6 / §5.8.1; the guarantee
# rows "Symmetric WRITER" and "`-o ro` under READ TOKENS" in operations.md).
# The in-process contracts (tests/sym_mount_posture_tests.rs) drive the
# registrant rung through reservation FAKES; this leg is the one venue where
# the ladder registers on REAL namespaces — `format --symmetric` (PR 11's
# product arm, no seam), the ARMED mount through every rung, the device
# reporting rtype 3 on BOTH the metadata and the data namespace, the retired
# posture knob refused at the startup gate through the real binary, every
# create at `dlm_rpcs == 0` (no partition seam — the writer's own slots), a
# `--read-only` member-reader + token client beside it seeing a foreign
# create EXACTLY at its next resolve (the live-FUSE token mount PR 5 named
# unreachable on a non-PR laptop — this substrate IS PR), the manager's
# kill -9 with the successor re-walking the ladder inside the derived bound,
# and the clean leave's ZERO residue on both namespaces.
# ---------------------------------------------------------------------------
# PR 12b — the N-daemon leg inside `sym-join-ladder`: joiners 2 and 3
# mount at their own mount points while the manager (mounted at
# $STATE/mnt-sym-join by the caller) serves. Every assertion here is a
# `bad` that names the daemon; the manager's mount is left as found.
sym_n_daemon_leg() { # meta data out
    local meta="$1" data="$2" out="$3"
    local mnt="$STATE/mnt-sym-join" j2="$STATE/mnt-sym-join-j2" j3="$STATE/mnt-sym-join-j3"
    local j2log="$STATE/sym-join-j2.log" j3log="$STATE/sym-join-j3.log"
    local reg_m0 reg_d0 reg_m reg_d v id2 id3 t0 t1 pid n
    mkdir -p "$j2" "$j3"
    : > "$j2log"
    : > "$j3log"
    reg_m0=$(nvme resv-report "$meta" --eds -o json 2>/dev/null | jq -r .regctl)
    reg_d0=$(nvme resv-report "$data" --eds -o json 2>/dev/null | jq -r .regctl)
    jstat() { # mnt field
        mnt_stat "$1" "$2"
    }
    mount_joiner() { # log mnt
        udevadm settle --timeout=10 2>/dev/null || true
        SQUEEZEFS_SYMMETRIC_META=1 RUST_LOG=info \
            "$FIDELI_BIN" --log-file "$1" mount "sqmeta://$meta" "$2" --daemon --allow-other >> "$out" 2>&1
    }
    wait_mnt() { # mnt
        local i
        for i in $(seq 1 60); do
            awk -v m="$1" '$2==m{f=1} END{exit !f}' /proc/mounts && return 0
            sleep 0.5
        done
        return 1
    }
    joiner_asserts() { # mnt name
        local m="$1" who="$2" posture symm lease jid jpost held mmode
        posture=$(jstat "$m" mount_posture)
        symm=$(jstat "$m" symmetric_meta)
        lease=$(jstat "$m" manager_lease)
        jid=$(jstat "$m" joined_appender_id)
        jpost=$(jstat "$m" joined_registrant_posture)
        held=$(jstat "$m" slot_leases_held)
        mmode=$(jstat "$m" membership_mode)
        log "SYMJOIN/N: $who mount_posture=$posture symmetric_meta=$symm manager_lease=$lease joined_appender_id=$jid joined_registrant_posture=$jpost slot_leases_held=$held membership_mode=$mmode"
        [ "$posture" = "writer" ] && ok "SYMJOIN/N: $who is a WRITER (every RW mount of an armed set)" || bad "SYMJOIN/N: $who mount_posture=$posture"
        [ "$symm" = "1" ] || bad "SYMJOIN/N: $who symmetric_meta=$symm"
        case "$lease" in
        peer:*) ok "SYMJOIN/N: $who holds no manager lease (manager_lease=$lease) — a joined appender" ;;
        *) bad "SYMJOIN/N: $who manager_lease=$lease (want peer:…)" ;;
        esac
        [ -n "$jid" ] && [ "$jid" != "null" ] && [ "$jid" -ge 1 ] 2>/dev/null && ok "SYMJOIN/N: $who joined as appender $jid (its own page + ring)" || bad "SYMJOIN/N: $who joined_appender_id=$jid"
        [ "$jpost" = "adopted" ] && ok "SYMJOIN/N: $who rung 4 ADOPTED the co-located manager's holds (KD-SYM-22: one host, one registrant)" || bad "SYMJOIN/N: $who joined_registrant_posture=$jpost (want adopted on one host)"
        [ "${held:-0}" -ge 1 ] 2>/dev/null && ok "SYMJOIN/N: $who slot_leases_held=$held (its rotor over the wire)" || bad "SYMJOIN/N: $who slot_leases_held=$held"
        [ "$mmode" = "member" ] && ok "SYMJOIN/N: $who is a MEMBER of the manager's shard (membership_mode=member)" || bad "SYMJOIN/N: $who membership_mode=$mmode"
    }
    t0=$(date +%s%3N)
    mount_joiner "$j2log" "$j2"
    if ! wait_mnt "$j2"; then
        bad "SYMJOIN/N: joiner 2 did not mount — $(tail -12 "$j2log"; tail -3 "$out")"
        return
    fi
    mount_joiner "$j3log" "$j3"
    if ! wait_mnt "$j3"; then
        bad "SYMJOIN/N: joiner 3 did not mount — $(tail -12 "$j3log"; tail -3 "$out")"
        umount "$j2" 2>/dev/null
        return
    fi
    t1=$(date +%s%3N)
    sleep 2
    log "SYMJOIN/N: two joiners mounted in $((t1 - t0)) ms; registrants — meta [$(regkeys "$meta")] data [$(regkeys "$data")]"
    joiner_asserts "$j2" "joiner 2"
    joiner_asserts "$j3" "joiner 3"
    id2=$(jstat "$j2" joined_appender_id)
    id3=$(jstat "$j3" joined_appender_id)
    [ "$id2" != "$id3" ] && ok "SYMJOIN/N: disjoint appender ids ($id2, $id3)" || bad "SYMJOIN/N: both joiners got appender id $id2"
    v=$(jstat "$mnt" appenders_known)
    [ "${v:-0}" -ge 3 ] 2>/dev/null && ok "SYMJOIN/N: the manager's directory names $v Live appenders (itself + 2 joiners — appenders_known)" || bad "SYMJOIN/N: manager appenders_known=$v (want ≥ 3)"
    reg_m=$(nvme resv-report "$meta" --eds -o json 2>/dev/null | jq -r .regctl)
    reg_d=$(nvme resv-report "$data" --eds -o json 2>/dev/null | jq -r .regctl)
    [ "$reg_m" = "$reg_m0" ] && [ "$reg_d" = "$reg_d0" ] && ok "SYMJOIN/N: the joins registered NOTHING (regctl meta $reg_m0→$reg_m, data $reg_d0→$reg_d — adoption)" || bad "SYMJOIN/N: the joins moved the registrant count (meta $reg_m0→$reg_m, data $reg_d0→$reg_d)"
    [ "$(nvme resv-report "$meta" --eds -o json 2>/dev/null | jq -r .rtype)" = "3" ] && ok "SYMJOIN/N: the manager's rtype-3 hold on the METADATA namespace stands under the joiners" || bad "SYMJOIN/N: meta rtype moved under the joiners"

    # Every daemon writes its own directory: 100 creates each, every one
    # acked, in each joiner's OWN ring (the manager's appenders_live says
    # who; `joined_wire_failures` must stay 0).
    local acked2 acked3 i
    mkdir -p "$j2/w2-dir" "$j3/w3-dir"
    acked2=0
    acked3=0
    for i in $(seq 1 100); do
        echo "j2 $i" > "$j2/w2-dir/f$i" 2>> "$out" && acked2=$((acked2 + 1))
        echo "j3 $i" > "$j3/w3-dir/f$i" 2>> "$out" && acked3=$((acked3 + 1))
    done
    dd if=/dev/urandom of="$j2/w2-dir/big.bin" bs=1M count=4 status=none 2>> "$out"
    sync
    [ "$acked2" = "100" ] && [ "$acked3" = "100" ] && ok "SYMJOIN/N: 100 + 100 creates acked on the joiners" || bad "SYMJOIN/N: acked j2=$acked2 j3=$acked3 of 100 each"
    for m in "$j2" "$j3"; do
        v=$(jstat "$m" joined_wire_failures)
        [ "${v:-x}" = "0" ] || bad "SYMJOIN/N: $m joined_wire_failures=$v"
        v=$(jstat "$m" joined_control_refusals)
        [ "${v:-x}" = "0" ] || bad "SYMJOIN/N: $m joined_control_refusals=$v (a control write reached a joiner)"
    done
    # Cross-daemon reads: exact at the next resolve — the manager reads a
    # joiner's directory through its token, joiner 2 reads joiner 3's and
    # the manager's, the reader would too. The count and one content.
    n=$(ls "$mnt/w2-dir" 2>> "$out" | grep -c '^f')
    [ "$n" = "100" ] && ok "SYMJOIN/N: the manager lists joiner 2's 100 names (a foreign slot through its holder's token)" || bad "SYMJOIN/N: manager lists $n of joiner 2's 100"
    [ "$(cat "$mnt/w3-dir/f7" 2>> "$out")" = "j3 7" ] && ok "SYMJOIN/N: the manager reads joiner 3's content exact" || bad "SYMJOIN/N: manager read of joiner 3's f7: '$(cat "$mnt/w3-dir/f7" 2>&1)'"
    n=$(ls "$j2/w3-dir" 2>> "$out" | grep -c '^f')
    [ "$n" = "100" ] && ok "SYMJOIN/N: joiner 2 lists joiner 3's 100 names (a joiner reading a joiner)" || bad "SYMJOIN/N: joiner 2 lists $n of joiner 3's 100"
    n=$(ls "$j3" 2>> "$out" | grep -c '^f')
    [ "$n" = "200" ] && ok "SYMJOIN/N: joiner 3 lists the manager's 200 names" || bad "SYMJOIN/N: joiner 3 lists $n of the manager's 200"
    [ "$(md5sum "$j3/w2-dir/big.bin" 2>/dev/null | cut -d' ' -f1)" = "$(md5sum "$j2/w2-dir/big.bin" | cut -d' ' -f1)" ] && ok "SYMJOIN/N: joiner 3 reads joiner 2's 4 MiB byte-exact (its data through the holder's grants)" || bad "SYMJOIN/N: joiner 3's read of joiner 2's big.bin differs"
    # A create INTO a foreign directory (PR 6's shipped step served by the
    # joiner): the manager creates under joiner 2's directory.
    echo "from manager" > "$mnt/w2-dir/by-manager" 2>> "$out" && ok "SYMJOIN/N: the manager created into joiner 2's directory (a cross-owner op — one intent, the step served by the joiner)" || bad "SYMJOIN/N: the manager's create into joiner 2's directory failed"
    [ "$(cat "$j2/w2-dir/by-manager" 2>> "$out")" = "from manager" ] && ok "SYMJOIN/N: joiner 2 reads the manager's create in its own directory" || bad "SYMJOIN/N: joiner 2 does not see the manager's create"
    # PR 13 §4.4z's INTERIM refusal on a REAL mount (review round 2, Issue
    # 22): a FOREIGN-slot FILE's mutation from a mount that does not lease
    # its slot refuses at the OPEN for write — where the shell checks —
    # with `EREMOTE` ("Object is remote") naming PR 13b (the default
    # mount's writeback cache would otherwise ack `write(2)` and report the
    # refusal only at close/fsync, so a `>>` printed rc 0 with its bytes
    # gone); `chmod` refuses the same way AND ITS EXIT STATUS SAYS SO —
    # §4.4ai: this contract's first run read `fchmodat = -1 EOPNOTSUPP`
    # with `chmod` exiting 0 and printing nothing, because coreutils ≥ 9.6
    # treats ENOTSUP from the mode syscall as "not applied"; the errno
    # class moved to one no tool swallows. The mode stays 644 at every
    # daemon; a read-only open serves; the holder's bytes stand.
    local xo_err xo_rc
    xo_err="$( (echo appended >> "$j2/w3-dir/f7") 2>&1 )"; xo_rc=$?
    if [ "$xo_rc" != "0" ] && echo "$xo_err" | grep -qi "object is remote"; then
        ok "SYMJOIN/N: joiner 2's \`>>\` into joiner 3's file failed AT THE OPEN with EREMOTE (rc=$xo_rc — the shell saw it; PR 13b owns the ship)"
    else
        bad "SYMJOIN/N: joiner 2's \`>>\` into joiner 3's file: rc=$xo_rc '$xo_err' (want a failed open, EREMOTE)"
    fi
    xo_err="$(chmod 600 "$j2/w3-dir/f7" 2>&1)"; xo_rc=$?
    if [ "$xo_rc" != "0" ] && echo "$xo_err" | grep -qi "object is remote"; then
        ok "SYMJOIN/N: joiner 2's chmod of joiner 3's file refused EREMOTE and chmod(1) EXITED NONZERO (never ENOENT for a file that exists; never an errno coreutils swallows)"
    else
        bad "SYMJOIN/N: joiner 2's chmod of joiner 3's file: rc=$xo_rc '$xo_err' (want EREMOTE, rc != 0)"
    fi
    [ "$(stat -c %a "$j2/w3-dir/f7" 2>> "$out")" = "644" ] && [ "$(stat -c %a "$j3/w3-dir/f7" 2>> "$out")" = "644" ] && [ "$(stat -c %a "$mnt/w3-dir/f7" 2>> "$out")" = "644" ] && ok "SYMJOIN/N: the refused chmod moved nothing (mode 644 at joiner 2, joiner 3 and the manager)" || bad "SYMJOIN/N: f7's mode after the refused chmod — j2 $(stat -c %a "$j2/w3-dir/f7" 2>&1) j3 $(stat -c %a "$j3/w3-dir/f7" 2>&1) mgr $(stat -c %a "$mnt/w3-dir/f7" 2>&1) (want 644 everywhere)"
    [ "$(cat "$j2/w3-dir/f7" 2>> "$out")" = "j3 7" ] && ok "SYMJOIN/N: the refused file still reads exact at joiner 2 (its bytes stand at the holder)" || bad "SYMJOIN/N: joiner 3's f7 read '$(cat "$j2/w3-dir/f7" 2>&1)' after the refusals"
    [ "$(cat "$j3/w3-dir/f7" 2>> "$out")" = "j3 7" ] && ok "SYMJOIN/N: the holder's bytes are untouched" || bad "SYMJOIN/N: the holder's f7 read '$(cat "$j3/w3-dir/f7" 2>&1)'"
    v=$(jstat "$j2" foreign_file_mutation_refusals)
    [ "${v:-0}" -ge 2 ] 2>/dev/null && ok "SYMJOIN/N: joiner 2 counted the refusals (foreign_file_mutation_refusals=$v)" || bad "SYMJOIN/N: joiner 2 foreign_file_mutation_refusals=$v (want ≥ 2)"
    # A joiner's TERMINAL FREE ships to the allocation-lease holder (PR 8's
    # law on a real second daemon): joiner 2 truncates its 4 MiB file, the
    # displaced block's free travels as `FreeBlocks` to the manager, whose
    # executor runs the ladder against the bitmap (Freed — never refused,
    # never abandoned: round 5 of this leg found the executor uninstalled
    # under the plane, every joiner free ABANDONED after 3 attempts).
    : > "$j2/w2-dir/big.bin"
    sync
    for i in $(seq 1 40); do
        v=$(jstat "$j2" meta_ship_publish.free_shipped_blocks)
        [ "${v:-0}" -ge 1 ] 2>/dev/null && break
        sleep 0.25
    done
    v=$(jstat "$j2" meta_ship_publish.free_shipped_blocks)
    [ "${v:-0}" -ge 1 ] 2>/dev/null && ok "SYMJOIN/N: joiner 2's displaced block's terminal free SHIPPED to the holder (free_shipped_blocks=$v)" || bad "SYMJOIN/N: joiner 2 shipped no free (free_shipped_blocks=$v)"
    v=$(jstat "$j2" meta_ship_publish.free_ship_failures)
    [ "${v:-x}" = "0" ] && ok "SYMJOIN/N: no shipped free abandoned at joiner 2 (free_ship_failures=0)" || bad "SYMJOIN/N: joiner 2 free_ship_failures=$v (a leaked block per failure)"
    v=$(jstat "$mnt" meta_ship_publish.free_served_blocks)
    [ "${v:-0}" -ge 1 ] 2>/dev/null && ok "SYMJOIN/N: the manager's executor SERVED the joiner's free against its bitmap (free_served_blocks=$v)" || bad "SYMJOIN/N: manager free_served_blocks=$v"
    v=$(jstat "$mnt" meta_ship_publish.free_refused_blocks)
    [ "${v:-x}" = "0" ] && ok "SYMJOIN/N: the manager refused no shipped free (free_refused_blocks=0)" || bad "SYMJOIN/N: manager free_refused_blocks=$v"
    local ms0 k
    ms0=""
    for m in "$mnt" "$j2" "$j3"; do
        for k in manager_verb_refusals meta_kv_replay_key_violations meta_kv_replay_lease_violations meta_kv_replay_extent_violations meta_kv_leaf_lease_refusals slot_lease_conflicts data_dma_fence_refusals invariant_tripwires dlm_token_recall_timeouts_live appender_fence_breach xv_cross_owner_intents_stuck; do
            v=$(stats "$m" "$k")
            [ "${v:-0}" = "0" ] || ms0="$ms0 $(basename "$m"):$k=$v"
        done
    done
    [ -z "$ms0" ] && ok "SYMJOIN/N: the must-stay-0 set is 0 on all three daemons" || bad "SYMJOIN/N: must-stay-0 moved:$ms0"

    # Joiner 3 dies (kill -9) and its successor remounts at the SAME mount
    # point — the same `(node, mount slot)` identity — so the manager's
    # `JoinAppender` answers `already` and the successor replays its dead
    # incarnation's ring as OWN RESIDUE: every acked name back, nothing
    # the manager had to recover.
    pid=$(pgrep -f "squeezefs.*mount sqmeta://$meta $j3" | head -1)
    if [ -n "$pid" ]; then
        kill -9 "$pid"
        sleep 1
        umount -l "$j3" 2>/dev/null
        cp "$j3log" "$STATE/sym-join-j3-predecessor.log" 2>/dev/null
        : > "$j3log"
        t0=$(date +%s%3N)
        mount_joiner "$j3log" "$j3"
        if wait_mnt "$j3"; then
            t1=$(date +%s%3N)
            sleep 2
            v=$(jstat "$j3" appender_self_recoveries)
            [ "${v:-0}" -ge 1 ] 2>/dev/null && ok "SYMJOIN/N: joiner 3's successor recovered its dead incarnation's ring as OWN RESIDUE (appender_self_recoveries=$v, $((t1 - t0)) ms)" || bad "SYMJOIN/N: joiner 3's successor appender_self_recoveries=$v"
            [ "$(jstat "$j3" joined_appender_id)" = "$id3" ] && ok "SYMJOIN/N: the successor rejoined its own region (appender $id3, `already`)" || bad "SYMJOIN/N: successor appender id $(jstat "$j3" joined_appender_id) != $id3"
            n=$(ls "$j3/w3-dir" 2>> "$out" | grep -c '^f')
            [ "$n" = "100" ] && ok "SYMJOIN/N: every acked name (100) served by joiner 3's successor" || bad "SYMJOIN/N: joiner 3's successor lists $n of 100"
            v=$(jstat "$mnt" appender_recoveries)
            [ "${v:-0}" = "0" ] && ok "SYMJOIN/N: the manager recovered nothing (a same-identity rejoin is its own residue)" || bad "SYMJOIN/N: manager appender_recoveries=$v"
        else
            bad "SYMJOIN/N: joiner 3's successor did not mount — $(tail -12 "$j3log")"
        fi
    else
        bad "SYMJOIN/N: no pid for joiner 3"
    fi

    # Clean leaves: pages Free, rings returned, nothing registered to
    # unregister — the registrant counts as before the joins, the
    # manager's hold standing, the manager alone in the directory.
    umount "$j3" >> "$out" 2>&1
    umount "$j2" >> "$out" 2>&1
    # The FUSE unmount returns before the daemon's leave (65 ReleaseSlot +
    # LeaveAppender over the wire — and, when a grant this holder issued
    # is held by the DEAD joiner-3 incarnation, PR 9's recall-at-leave
    # waits the S9 sweep out: T_owner + one renewal). Wait for both
    # PROCESSES to exit, bounded past that law, before judging the
    # directory — a lingering joiner is what round 5's guard leg killed
    # instead of its own daemon (`pgrep | head -1`).
    t0=$(date +%s)
    for i in $(seq 1 240); do
        pgrep -f "squeezefs.*mount sqmeta://$meta $j2 " > /dev/null 2>&1 || pgrep -f "squeezefs.*mount sqmeta://$meta $j3 " > /dev/null 2>&1 || break
        sleep 0.5
    done
    if pgrep -f "squeezefs.*mount sqmeta://$meta $j2 " > /dev/null 2>&1 || pgrep -f "squeezefs.*mount sqmeta://$meta $j3 " > /dev/null 2>&1; then
        bad "SYMJOIN/N: a joiner daemon is still alive $(( $(date +%s) - t0 )) s after its umount (the leave never completed)"
    else
        ok "SYMJOIN/N: both joiner daemons exited after their umount ($(( $(date +%s) - t0 )) s; PR 9's dead-grant recall bounds the leave at T_owner + renew)"
    fi
    for i in $(seq 1 40); do
        [ "$(jstat "$mnt" appenders_known)" = "1" ] && break
        sleep 0.5
    done
    reg_m=$(nvme resv-report "$meta" --eds -o json 2>/dev/null | jq -r .regctl)
    reg_d=$(nvme resv-report "$data" --eds -o json 2>/dev/null | jq -r .regctl)
    [ "$reg_m" = "$reg_m0" ] && [ "$reg_d" = "$reg_d0" ] && ok "SYMJOIN/N: the leaves left the registrant counts as found (meta $reg_m, data $reg_d)" || bad "SYMJOIN/N: the leaves moved the registrant count (meta $reg_m0→$reg_m, data $reg_d0→$reg_d)"
    [ "$(nvme resv-report "$meta" --eds -o json 2>/dev/null | jq -r .rtype)" = "3" ] && ok "SYMJOIN/N: the manager's rtype-3 hold survived two joiners' leaves" || bad "SYMJOIN/N: meta rtype after the leaves != 3"
    v=$(jstat "$mnt" appenders_known)
    [ "${v:-0}" = "1" ] && ok "SYMJOIN/N: appenders_known back to 1 (both pages Free)" || bad "SYMJOIN/N: appenders_known=$v after the leaves"
    n=$(ls "$mnt/w2-dir" 2>> "$out" | grep -c '^f')
    [ "$n" = "100" ] && ok "SYMJOIN/N: joiner 2's 100 names served by the manager after its leave (the released slot is the manager's to maintain)" || bad "SYMJOIN/N: after the leave the manager lists $n of joiner 2's 100"
    [ "$(cat "$mnt/w3-dir/f42" 2>> "$out")" = "j3 42" ] && ok "SYMJOIN/N: joiner 3's content served by the manager after its leave" || bad "SYMJOIN/N: manager read of joiner 3's f42 after the leave: '$(cat "$mnt/w3-dir/f42" 2>&1)'"
    if "$FIDELI_BIN" fsck "$mnt" > "$STATE/legs/sym-n-fsck.txt" 2>&1 && grep -q "findings: 0" "$STATE/legs/sym-n-fsck.txt"; then
        ok "SYMJOIN/N: online fsck clean after three daemons wrote (findings: 0)"
    else
        bad "SYMJOIN/N: online fsck after the N-daemon leg — $(tail -5 "$STATE/legs/sym-n-fsck.txt")"
    fi
}

leg_sym_join_ladder() {
    local out="$STATE/legs/sym-join-ladder.txt" dlog="$STATE/sym-join-daemon.log" rlog="$STATE/sym-join-reader.log"
    local mnt="$STATE/mnt-sym-join" rmnt="$STATE/mnt-sym-join-ro" meta data dev k rc t0 t1 pid
    local posture symm lease wero fence rungs grade regs endpoint mmode rpcs held bound
    local md5 md5b acked shipped reg_m reg_d rtype_m rtype_d listed rposture rmode rbound rgrants rrefusals
    : > "$out"
    : > "$dlog"
    : > "$rlog"
    # shellcheck disable=SC1091 # generated by nvmeof_target_substrate.sh create
    . "$STATE/devices.env"
    meta=$(finddev "$NQN_GMETA_NVMET") || { bad "SYMJOIN: no meta device"; return; }
    data=$(finddev "$NQN_GDATA_NVMET") || { bad "SYMJOIN: no data device"; return; }
    mkdir -p "$mnt" "$rmnt"
    pkill -f "squeezefs.*mount sqmeta://$meta" && sleep 2
    umount -l "$rmnt" 2>/dev/null
    umount -l "$mnt" 2>/dev/null
    for dev in "$meta" "$data"; do
        for k in $(nvme resv-report "$dev" --eds -o json 2>/dev/null | jq -r '.regctlext[]?.rkey'); do
            nvme resv-register "$dev" --crkey="$k" --rrega=1 >> "$out" 2>&1
        done
        dd if=/dev/zero of="$dev" bs=1M count=16 oflag=direct status=none
    done

    # PR 11's product arm: bit 17 + the appender directory + the multi-writer
    # class, no seam.
    if "$FIDELI_BIN" format "sqmeta://$meta" "sqdata://$data" --symmetric --force >> "$out" 2>&1; then
        ok "SYMJOIN: format --symmetric (bit 17 through the product arm)"
    else
        bad "SYMJOIN: format --symmetric failed — $(tail -3 "$out")"
        return
    fi

    # Rung 1 through the real binary: a retired posture knob beside the plane
    # refuses at the startup gate, before anything is opened (no --daemon —
    # the refusal is synchronous; the timeout is the belt against a mount
    # that wrongly proceeds).
    SQUEEZEFS_SYMMETRIC_META=1 SQUEEZEFS_MULTI_WRITER=1 \
        timeout 30 "$FIDELI_BIN" mount "sqmeta://$meta" "$mnt" --allow-other > "$STATE/legs/sym-join-rung1.txt" 2>&1
    rc=$?
    if [ "$rc" -ne 0 ] && grep -q "RETIRED on a symmetric mount" "$STATE/legs/sym-join-rung1.txt"; then
        ok "SYMJOIN: rung 1 — SQUEEZEFS_MULTI_WRITER beside the plane refused at the startup gate naming the ladder (rc=$rc)"
    else
        bad "SYMJOIN: rung 1 — rc=$rc; $(head -3 "$STATE/legs/sym-join-rung1.txt")"
        umount -l "$mnt" 2>/dev/null
        pkill -f "squeezefs.*mount sqmeta://$meta"
    fi
    if awk -v m="$mnt" '$2==m{f=1} END{exit !f}' /proc/mounts; then
        bad "SYMJOIN: rung 1 — the refused mount APPEARED"
        umount -l "$mnt" 2>/dev/null
    fi

    mount_join() { # log-file
        udevadm settle --timeout=10 2>/dev/null || true
        SQUEEZEFS_SYMMETRIC_META=1 RUST_LOG=info \
            "$FIDELI_BIN" --log-file "$1" mount "sqmeta://$meta" "$mnt" --daemon --allow-other >> "$out" 2>&1
    }
    wait_mounted() { # mnt
        local i
        for i in $(seq 1 60); do
            awk -v m="$1" '$2==m{f=1} END{exit !f}' /proc/mounts && return 0
            sleep 0.5
        done
        return 1
    }
    stats() { # mnt jq-path (under .metrics or the root; a per-volume array
        # answers its first element; `false`/`0` are VALUES here, never the
        # `//` operator's "absent" — the fallback is on null alone)
        jq -r "[.metrics.$2, .$2] | map(select(. != null)) | .[0] | if type == \"array\" then .[0] else . end" "$1/.stats" 2>/dev/null
    }
    regkeys() { # dev -> the namespace's registrant keys, space-joined
        nvme resv-report "$1" --eds -o json 2>/dev/null | jq -r '[.regctlext[]?.rkey] | join(" ")'
    }
    t0=$(date +%s%3N)
    mount_join "$dlog"
    wait_mounted "$mnt" || { bad "SYMJOIN: the armed mount did not appear — $(tail -8 "$dlog")"; return; }
    t1=$(date +%s%3N)
    sleep 2
    posture=$(mnt_stat "$mnt" mount_posture)
    symm=$(mnt_stat "$mnt" symmetric_meta)
    lease=$(mnt_stat "$mnt" manager_lease)
    wero=$(mnt_stat "$mnt" meta_pr_wero)
    fence=$(mnt_stat "$mnt" data_plane_fence_mode)
    mmode=$(mnt_stat "$mnt" membership_mode)
    rpcs=$(mnt_stat "$mnt" dlm_rpcs)
    held=$(mnt_stat "$mnt" slot_leases_held)
    bound=$(mnt_stat "$mnt" manager_failover_bound_ms)
    join_report() { # jq-expr over the symmetric_join object
        jq -r "(.metrics.symmetric_join // .symmetric_join) | $1" "$mnt/.stats" 2>/dev/null
    }
    rungs=$(join_report '.rungs | join(" > ")')
    grade=$(join_report '.detection_grade')
    regs=$(join_report '.data_namespaces_registered')
    endpoint=$(join_report '.endpoint')
    log "SYMJOIN: registrants after the armed mount — meta [$(regkeys "$meta")] data [$(regkeys "$data")]"
    log "SYMJOIN: mount wall $((t1 - t0)) ms; mount_posture=$posture symmetric_meta=$symm manager_lease=$lease meta_pr_wero=$wero data_plane_fence_mode=$fence membership_mode=$mmode dlm_rpcs=$rpcs slot_leases_held=$held"
    log "SYMJOIN: symmetric_join rungs=[$rungs] detection_grade=$grade data_namespaces_registered=$regs endpoint=$endpoint"
    [ "$posture" = "writer" ] && ok "SYMJOIN: mount_posture=writer (every RW mount of an armed set is a writer)" || bad "SYMJOIN: mount_posture=$posture"
    [ "$symm" = "1" ] && ok "SYMJOIN: symmetric_meta=1 (the plane is armed)" || bad "SYMJOIN: symmetric_meta=$symm"
    [ "$lease" = "held" ] && ok "SYMJOIN: manager_lease=held (the D0 winner IS the manager)" || bad "SYMJOIN: manager_lease=$lease"
    [ "$wero" = "1" ] && ok "SYMJOIN: meta_pr_wero=1 (rtype 3 on the metadata namespace)" || bad "SYMJOIN: meta_pr_wero=$wero"
    [ "$fence" = "1" ] && ok "SYMJOIN: data_plane_fence_mode=1 (the registrant rung held WERO on the data namespace)" || bad "SYMJOIN: data_plane_fence_mode=$fence"
    [ "$mmode" = "owner" ] && ok "SYMJOIN: membership_mode=owner (rung 3 armed the home shard's plane itself)" || bad "SYMJOIN: membership_mode=$mmode"
    [ "$grade" = "false" ] && ok "SYMJOIN: the ladder is DEVICE-ENFORCED on this substrate (detection_grade=false)" || bad "SYMJOIN: detection_grade=$grade on a PR substrate"
    [ "${regs:-0}" = "1" ] && ok "SYMJOIN: symmetric_join.data_namespaces_registered=1" || bad "SYMJOIN: data_namespaces_registered=$regs (want 1)"
    case "$rungs" in
    "declaration > bits > membership > registrant > join_appender > acquire_slots > planes") ok "SYMJOIN: the report names every required rung (the design's numbering — a checklist, not a trace) [$rungs]" ;;
    *) bad "SYMJOIN: rungs=[$rungs]" ;;
    esac
    [ -n "$endpoint" ] && [ "$endpoint" != "null" ] && ok "SYMJOIN: the writer published its S8 listener ($endpoint) — the binding half of §5.1.6" || bad "SYMJOIN: no endpoint published"
    [ "${held:-0}" -ge 1 ] && ok "SYMJOIN: slot_leases_held=$held (the native slot + the rotor)" || bad "SYMJOIN: slot_leases_held=$held"
    rtype_m=$(nvme resv-report "$meta" --eds -o json 2>/dev/null | jq -r .rtype)
    rtype_d=$(nvme resv-report "$data" --eds -o json 2>/dev/null | jq -r .rtype)
    reg_d=$(nvme resv-report "$data" --eds -o json 2>/dev/null | jq -r .regctl)
    [ "$rtype_m" = "3" ] && ok "SYMJOIN: the DEVICE reports rtype 3 on the METADATA namespace" || bad "SYMJOIN: meta rtype=$rtype_m (want 3)"
    [ "$rtype_d" = "3" ] && [ "${reg_d:-0}" -ge 1 ] && ok "SYMJOIN: the DEVICE reports rtype 3 on the DATA namespace ($reg_d registrant(s)) — the guarantee row's device-enforced class" || bad "SYMJOIN: data rtype=$rtype_d regctl=$reg_d (want 3 / ≥ 1)"

    dd if=/dev/urandom of="$mnt/join.bin" bs=1M count=8 status=none
    sync
    md5=$(md5sum "$mnt/join.bin" | cut -d' ' -f1)
    # 200 creates: no declared partition, so every mint lands in a slot this
    # writer leases — 200 of 200 acked, nothing shipped, dlm_rpcs still 0
    # (gate 1's law on a solo armed mount).
    acked=0
    for i in $(seq 1 200); do
        echo "join $i" > "$mnt/f$i" 2>> "$out" && acked=$((acked + 1))
    done
    sync
    shipped=$(mnt_stat "$mnt" xv_cross_owner_steps_shipped)
    rpcs=$(mnt_stat "$mnt" dlm_rpcs)
    [ "$acked" = "200" ] && ok "SYMJOIN: 200 of 200 creates acked (no partition seam under the ladder)" || bad "SYMJOIN: only $acked of 200 creates acked"
    [ "${shipped:-0}" = "0" ] && [ "${rpcs:-x}" = "0" ] && ok "SYMJOIN: dlm_rpcs=0 and xv_cross_owner_steps_shipped=0 after 200 creates (the solo armed mount ships nothing)" || bad "SYMJOIN: dlm_rpcs=$rpcs shipped=$shipped"
    local ms0 v
    ms0=""
    for k in manager_verb_refusals manager_dependency_stalls meta_kv_replay_key_violations meta_kv_replay_lease_violations meta_kv_replay_extent_violations meta_kv_leaf_lease_refusals slot_lease_conflicts data_dma_fence_refusals cowriter.accounting_refusals cowriter.local_commit_refusals; do
        v=$(stats "$mnt" "$k")
        [ "${v:-0}" = "0" ] || ms0="$ms0 $k=$v"
    done
    [ -z "$ms0" ] && ok "SYMJOIN: the must-stay-0 set is 0 under load (verbs/stalls/replay classes/lease gate/DMA fence/accounting/local-commit)" || bad "SYMJOIN: must-stay-0 moved:$ms0"

    # The reader: `--read-only` under the plane = member-reader + token
    # client (§5.7.2). It joins the membership plane off the rendezvous
    # record, resolves the manager's listener off tree 0 + the published
    # claim-set endpoint (PR 12's binding — no declared authority), and sees a
    # foreign create at its NEXT resolve: exact, bound 0.
    SQUEEZEFS_SYMMETRIC_META=1 RUST_LOG=info \
        "$FIDELI_BIN" --log-file "$rlog" mount "sqmeta://$meta" "$rmnt" --read-only --daemon --allow-other >> "$out" 2>&1
    if wait_mounted "$rmnt"; then
        sleep 2
        rposture=$(mnt_stat "$rmnt" mount_posture)
        rmode=$(mnt_stat "$rmnt" membership_mode)
        rbound=$(mnt_stat "$rmnt" reader_staleness_bound_ms)
        [ "$rposture" = "reader" ] && ok "SYMJOIN: reader mount_posture=reader" || bad "SYMJOIN: reader mount_posture=$rposture"
        [ "$rmode" = "member" ] && ok "SYMJOIN: the reader JOINED the membership plane (membership_mode=member)" || bad "SYMJOIN: reader membership_mode=$rmode"
        [ "$rbound" = "0" ] && ok "SYMJOIN: reader_staleness_bound_ms=0 (READ TOKENS — exact at the next resolve, never bounded)" || bad "SYMJOIN: reader_staleness_bound_ms=$rbound"
        echo "foreign create" > "$mnt/after-reader-joined"
        if [ "$(cat "$rmnt/after-reader-joined" 2>> "$out")" = "foreign create" ]; then
            ok "SYMJOIN: a create on the writer is visible at the reader's NEXT resolve, content exact (gate 5)"
        else
            bad "SYMJOIN: the reader did not see the writer's create — $(tail -8 "$rlog")"
        fi
        listed=$(ls "$rmnt" | grep -c '^f')
        [ "$listed" = "200" ] && ok "SYMJOIN: the reader lists every acked name (200) off the carried dentry set" || bad "SYMJOIN: reader lists $listed of 200"
        rgrants=$(mnt_stat "$rmnt" dlm_token_grants)
        rrefusals=$(mnt_stat "$rmnt" dlm_token_serve_refusals)
        [ "${rgrants:-0}" -ge 1 ] && ok "SYMJOIN: dlm_token_grants=$rgrants at the reader (the grants rode the manager's published listener)" || bad "SYMJOIN: reader dlm_token_grants=$rgrants"
        [ "${rrefusals:-x}" = "0" ] && ok "SYMJOIN: dlm_token_serve_refusals=0 at the reader" || bad "SYMJOIN: reader dlm_token_serve_refusals=$rrefusals"
        v=$(mnt_stat "$mnt" dlm_token_grants_served)
        [ "${v:-0}" -ge 1 ] && ok "SYMJOIN: dlm_token_grants_served=$v at the writer (the holder served the reader)" || bad "SYMJOIN: writer dlm_token_grants_served=$v"
        umount "$rmnt" >> "$out" 2>&1
        sleep 1
    else
        bad "SYMJOIN: the --read-only token reader did not mount — $(tail -12 "$rlog"; tail -3 "$out")"
    fi

    # PR 12b — N = 3 REAL daemons on the volume: two more RW mounts of the
    # armed set at other mount points (their own processes, their own
    # `(node, mount slot)` identities) JOIN the live manager through the
    # ladder — no knob but the plane — as full writers: their own rings,
    # pages and slot leases, tree 0 a projection, tokens for each other's
    # slots. One host = one PR association, so rung 4 ADOPTS the manager's
    # holds (KD-SYM-22): the registrant counts on BOTH namespaces are
    # unchanged by the joins and by the leaves.
    sym_n_daemon_leg "$meta" "$data" "$out"

    # The manager dies; the successor re-walks the ladder (same host: the
    # flock reclaims instantly, the strict target's stale keys ride the
    # register ladder on BOTH namespaces).
    pid=$(pgrep -f "squeezefs.*mount sqmeta://$meta $mnt " | head -1)
    [ -n "$pid" ] || { bad "SYMJOIN: no daemon pid"; return; }
    log "SYMJOIN: registrants before the kill — meta [$(regkeys "$meta")] data [$(regkeys "$data")]"
    t0=$(date +%s%3N)
    kill -9 "$pid"
    sleep 1
    umount -l "$mnt" 2>/dev/null
    log "SYMJOIN: registrants after the kill (the dead incarnation's residue) — meta [$(regkeys "$meta")] data [$(regkeys "$data")]"
    cp "$dlog" "$STATE/sym-join-daemon-predecessor.log" 2>/dev/null
    : > "$dlog"
    mount_join "$dlog"
    if wait_mounted "$mnt"; then
        t1=$(date +%s%3N)
        sleep 2
        log "SYMJOIN: registrants under the successor — meta [$(regkeys "$meta")] data [$(regkeys "$data")]"
        lease=$(mnt_stat "$mnt" manager_lease)
        md5b=$(md5sum "$mnt/join.bin" 2>/dev/null | cut -d' ' -f1)
        log "SYMJOIN: successor wall (kill → mounted) $((t1 - t0)) ms against bound $bound ms"
        [ "$lease" = "held" ] && ok "SYMJOIN: the successor holds the manager lease" || bad "SYMJOIN: successor manager_lease=$lease"
        [ "$((t1 - t0))" -lt "${bound:-1}" ] && ok "SYMJOIN: successor inside manager_failover_bound_ms ($((t1 - t0)) < $bound)" || bad "SYMJOIN: successor took $((t1 - t0)) ms ≥ bound $bound"
        [ "$md5b" = "$md5" ] && ok "SYMJOIN: acked data byte-intact across the manager's death" || bad "SYMJOIN: md5 $md5b != $md5"
        listed=$(ls "$mnt" | grep -c '^f')
        [ "$listed" = "200" ] && ok "SYMJOIN: every acked name (200) served by the successor" || bad "SYMJOIN: names lost — $listed listed of 200"
        [ "$(mnt_stat "$mnt" appender_self_recoveries)" -ge 1 ] && ok "SYMJOIN: own-residue recovery counted (appender_self_recoveries ≥ 1)" || bad "SYMJOIN: no self recovery counted"
        [ "$(mnt_stat "$mnt" mount_posture)" = "writer" ] && [ "$(mnt_stat "$mnt" data_plane_fence_mode)" = "1" ] && ok "SYMJOIN: the successor re-walked the ladder (writer, data_plane_fence_mode=1)" || bad "SYMJOIN: successor posture=$(mnt_stat "$mnt" mount_posture) fence=$(mnt_stat "$mnt" data_plane_fence_mode)"
        [ "$(nvme resv-report "$data" --eds -o json 2>/dev/null | jq -r .rtype)" = "3" ] && ok "SYMJOIN: WERO re-held on the DATA namespace by the successor" || bad "SYMJOIN: successor data rtype != 3"
    else
        bad "SYMJOIN: successor mount did not appear — $(tail -20 "$dlog")"
        return
    fi

    umount "$mnt" >> "$out" 2>&1
    sleep 1
    reg_m=$(nvme resv-report "$meta" --eds -o json 2>/dev/null | jq -r .regctl)
    reg_d=$(nvme resv-report "$data" --eds -o json 2>/dev/null | jq -r .regctl)
    log "SYMJOIN: registrants after the clean leave — meta [$(regkeys "$meta")] data [$(regkeys "$data")]"
    [ "${reg_m:-x}" = "0" ] && [ "${reg_d:-x}" = "0" ] && ok "SYMJOIN: zero PR residue on BOTH namespaces after the clean leave (regctl meta=0 data=0)" || bad "SYMJOIN: residue regctl meta=$reg_m data=$reg_d"
    if "$FIDELI_BIN" appenders "sqmeta://$meta" --json >> "$out" 2>&1; then
        ok "SYMJOIN: appenders probe lists the directory after the clean leave"
    else
        bad "SYMJOIN: appenders probe failed"
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
    # Symmetric PR 3 (both tiers): the registrant ceiling and the manager
    # lease on the real PR target.
    run_leg pr-registrants leg_pr_registrants
    run_leg sym-manager-failover leg_sym_manager_failover
    # Symmetric PR 12 (both tiers): the join ladder registering on REAL
    # namespaces, the token reader beside it, the successor re-walking it.
    run_leg sym-join-ladder leg_sym_join_ladder

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
