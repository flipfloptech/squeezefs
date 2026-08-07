#!/usr/bin/env bash
# tests/run_nvmeof_fidelity.sh — the dual-stack NVMe-oF fidelity tier
# ===================================================================
#
# The standing real-kernel, ZERO-MOCK acceptance suite for the NVMe-oF
# target-management program (docs/design-nvmeof-target-management.md §6.8):
# every leg drives the PRODUCT's own verbs against BOTH real target stacks
# (SPDK spdk_tgt + kernel nvmet) on the fidelity substrate
# (tests/nvmeof_target_substrate.sh). Supersedes the ad-hoc root gates that
# lived under `.agents/spdk-scoping/` (removed from the tree — git history
# at c615e3a).
#
# Usage
#   sudo tests/run_nvmeof_fidelity.sh quick     # per-PR tier (~10 min)
#   sudo tests/run_nvmeof_fidelity.sh full      # nightly tier (~45-70 min)
#
# quick (per-PR for changes touching src/nvmeof/, reservation.rs, or the
# guard gate): both-stack product verb round-trips (share -> connect -> IO
# -> unshare -> residue-free) + ONE guard kill-9 cycle per stack.
#
# full (nightly / program & release gates) adds:
#   * loud-fail matrix (G3): missing backing, unledgered unshare, --nsid!=1
#     on nvmet, nvmet --force refusal, broken SQUEEZEFS_SPDK_TGT_BIN
#     override, dead-RPC runbook message, double target start
#   * crash-window injection (§6.4 law 6) — SPDK stack: the CLI is SIGKILLED
#     mid-verb inside three deterministic windows held open by a stalling
#     JSON-RPC proxy standing in for spdk_tgt's socket (the §6.8-sanctioned
#     RUN_DIR relocation seam; the product carries NO test seam): pending
#     intent GC'd, pending intent finalized, removing teardown resumed —
#     plus the mid-window duplicate-guard refusals. nvmet stack: two REAL
#     law-6 states produced by kernel-refused mid-verb mutations
#     (EADDRNOTAVAIL listener bind) — pending finalized (live objects
#     match), pending garbage-collected after manual partial-residue wipe
#   * adopt legs (§6.10 pt 5): pre-rebuild-style configfs adopt (small-int
#     port id, zero target mutation, zero serving interruption),
#     adopt-after-simulated-ledger-loss on BOTH stacks, harness-owned
#     refusal against the fidelity NQN marker itself
#   * PR/PTPL matrix (pr-matrix.sh productized): RESCAP, register/acquire,
#     cross-host fence (EBADE class), preempt, PTPL across a product-verb
#     target power cycle (spdk); register/fence/preempt on nvmet
#   * target-restart persistence (G2), both stacks: SPDK share -> mount ->
#     SIGKILL spdk_tgt -> `target start` (load_config) -> IO resumes with
#     the reservation intact (PTPL; fenced=0, pr_reacquires=0); nvmet
#     configfs wipe -> `restore` -> same identity re-presented -> the
#     connected initiator reattaches without operator action
#   * soft-RoCE plumbing leg (rdma_rxe; user decision, Resolved Questions
#     #5): kernel-initiator NVMe/RDMA connect + IO round-trip against an
#     rxe listener on the PRODUCT-shared nvmet subsystem — plumbing
#     validation ONLY, explicitly NOT representative of real RNIC behavior;
#     no guard or perf claims ride it. The rdma listener is HARNESS-built:
#     the product's listener plumbing cannot express trtype=rdma yet (a
#     named residual for PR 7 — see the leg's RESIDUAL lines)
#   * A/B smoke rows (recorded, NOT ordered): fio rand4k QD32 write+read on
#     each stack's raw guard-data namespace — instrument: fio io_uring
#     O_DIRECT (per-release ordered A/B belongs to PR 7's bench rerun)
#   * guard matrix kill-9 x10 per stack (S1 ladder; the multi-run
#     discipline applies — any fix restarts the count from zero) + the
#     SPDK PTPL power-cycle leg (tests/guard_smoke.sh)
#   * teardown-to-zero-residue proof (substrate before/after snapshot diff
#     empty — counted as a leg)
#
# Cadence mapping: AGENTS.md "Test tiering" table (quick = per-PR row,
# full = nightly row). Requires: root (re-execs via sudo), nvme-cli, jq,
# python3 (crash-window proxy), fio (A/B smoke; loud SKIP if absent).
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

leg_roundtrip_spdk() {
    local out="$STATE/legs/rt-spdk.txt" nqn zb uuid rec dev port=4611
    nqn="nqn.2026-07.io.squeezefs:fideli-rt-spdk"
    zb=$("$SUBSTRATE" mkzram $((2 * 1024 * 1024 * 1024)) rt-spdk)

    if ! "$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port "$port" --subnqn "$nqn" > "$out" 2>&1; then
        bad "share (default stack): $(tail -3 "$out")"
        return
    fi
    record "share=$nqn"
    if grep -q "spdk stack" "$out"; then
        ok "share rode the DEFAULT spdk stack"
    else
        bad "default stack"
    fi
    uuid=$(jq -r ".shares[] | select(.subnqn==\"$nqn\") | .ns_uuid" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json")
    rec=$(jq -c ".shares[] | select(.subnqn==\"$nqn\")" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json")
    if echo "$rec" | jq -e ".state==\"active\" and .stack==\"spdk\" and .nsid==1 and .ns_uuid!=null \
        and .bdev_name!=null and .ptpl_file==\"spdk/ptpl/$uuid.json\" and .loop_device==null" >/dev/null; then
        ok "ledger record pins the §6.4 SPDK shape (nsid=1, uuid, ptpl, bdev)"
    else
        bad "ledger record shape: $rec"
    fi
    if grep -q "$nqn" "$SQUEEZEFS_NVMEOF_STATE_DIR/spdk/tgt-config.json" &&
        grep -q "$uuid" "$SQUEEZEFS_NVMEOF_STATE_DIR/spdk/tgt-config.json"; then
        ok "tgt-config.json (save_config) captured subsystem + pinned uuid"
    else
        bad "tgt-config after share"
    fi
    # Read-only live-identity assert (rpc.py is an assertion instrument;
    # every mutation stays product-verb).
    if [ -n "$FIDELI_RPCPY" ]; then
        "$FIDELI_RPCPY" -s "$SQUEEZEFS_NVMEOF_RUN_DIR/spdk.sock" nvmf_get_subsystems \
            > "$STATE/legs/rt-spdk-live.json" 2>/dev/null
        if jq -e ".[] | select(.nqn==\"$nqn\") | .namespaces[0] |
                (.nsid==1 and ((.uuid|ascii_downcase)==(\"$uuid\"|ascii_downcase)))" \
            "$STATE/legs/rt-spdk-live.json" >/dev/null; then
            ok "live target serves nsid 1 under the pinned ns UUID"
        else
            bad "live namespace identity"
        fi
    fi

    "$FIDELI_BIN" nvmeof connect --ip 127.0.0.1 --port "$port" --subnqn "$nqn" >> "$out" 2>&1
    record "connected=$nqn"
    dev=$(finddev "$nqn") || { bad "no initiator device"; return; }
    if io_roundtrip "$dev" 64; then
        ok "64 MiB O_DIRECT round-trip (spdk)"
    else
        bad "spdk IO md5"
    fi

    "$FIDELI_BIN" nvmeof disconnect "$nqn" >> "$out" 2>&1
    sleep 1
    if "$FIDELI_BIN" nvmeof unshare "$nqn" >> "$out" 2>&1; then
        ok "unshare"
    else
        bad "unshare"
    fi
    if grep -q "$nqn" "$SQUEEZEFS_NVMEOF_STATE_DIR/spdk/tgt-config.json"; then
        bad "tgt-config still carries the unshared subsystem (resurrection hazard)"
    else
        ok "tgt-config no longer describes the share (resurrection law)"
    fi
    if [ -z "$(ledger_state_of "$nqn")" ]; then
        ok "ledger clean"
    else
        bad "ledger residue"
    fi
}

# ===========================================================================
# Loud-fail matrix (G3 — full)
# ===========================================================================
leg_loudfail() {
    local out rc fake_run
    out=$("$FIDELI_BIN" nvmeof share "$STATE/definitely-missing.img" --ip 127.0.0.1 \
        --port 4650 --target-stack nvmet 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "does not exist" && echo "$out" | grep -q -- "--create-size"; then
        ok "missing backing refuses loud, names --create-size (nvmet)"
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
        ok "missing backing refuses loud (spdk default)"
    else
        bad "missing-backing refusal (spdk): $out"
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
        ok "--nsid 2 with nvmet refuses loud"
    else
        bad "--nsid refusal: $out"
    fi

    # shellcheck disable=SC1091 # generated by nvmeof_target_substrate.sh create
    . "$STATE/devices.env"
    out=$("$FIDELI_BIN" nvmeof unshare "$NQN_GMETA_NVMET" --force 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "never silently ignores"; then
        ok "--force on an nvmet record refuses (no silent flag-ignore)"
    else
        bad "nvmet --force: $out"
    fi
    if [ "$(ledger_state_of "$NQN_GMETA_NVMET")" = "active" ]; then
        ok "refused unshare mutated nothing (guard record still active)"
    else
        bad "guard record disturbed"
    fi

    out=$(SQUEEZEFS_SPDK_TGT_BIN=/nonexistent/spdk_tgt "$FIDELI_BIN" nvmeof share /dev/null \
        --ip 127.0.0.1 --port 4650 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "does not exist" &&
        echo "$out" | grep -q "never falls through" &&
        echo "$out" | grep -q "never falls back between target stacks"; then
        ok "broken SQUEEZEFS_SPDK_TGT_BIN override refuses loud + no-fallback law"
    else
        bad "broken-override refusal: $out"
    fi

    # Dead-RPC runbook: a fake RUN_DIR (relocation seam) with the REAL
    # alive pidfile but no socket — rung 3 fires with the §6.2 message.
    fake_run="$STATE/fake-run-deadrpc"
    mkdir -p "$fake_run"
    cp "$SQUEEZEFS_NVMEOF_RUN_DIR/spdk_tgt.pid" "$fake_run/" 2>/dev/null
    out=$(SQUEEZEFS_NVMEOF_RUN_DIR="$fake_run" "$FIDELI_BIN" nvmeof share /dev/null \
        --ip 127.0.0.1 --port 4650 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "target status" &&
        echo "$out" | grep -q "target start" &&
        echo "$out" | grep -q "never falls back between target stacks"; then
        ok "dead-RPC refusal carries the check/start runbook + no-fallback law"
    else
        bad "dead-RPC refusal: $out"
    fi
    rm -rf "$fake_run"

    out=$("$FIDELI_BIN" nvmeof target start 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "already running"; then
        ok "second target start refuses (already running)"
    else
        bad "already-running: $out"
    fi

    if "$FIDELI_BIN" nvmeof target status --json 2>/dev/null |
        jq -e '.rpc.live == true and .rpc.drift == false' >/dev/null; then
        ok "target status reports rpc.live + drift=false (report-only rung)"
    else
        bad "status drift shape"
    fi
}

# ===========================================================================
# Crash-window injection — SPDK stack (§6.4 law 6; full)
#
# Mechanism (stated per the design's env-seam line, §6.8): the product has
# NO test seam. SQUEEZEFS_NVMEOF_RUN_DIR — a sanctioned relocation seam —
# points ONE injected CLI invocation at a stalling JSON-RPC proxy that
# forwards every call verbatim to the real spdk_tgt socket but withholds
# ONE configured method, holding the verb parked mid-window until the
# harness SIGKILLs it (a real crash, deterministic by construction: the
# marker file appears only after the withheld request was received). The
# RPC client v2 opens ONE connection per call (src/nvmeof/spdk/rpc.rs),
# so per-connection first-read inspection sees every method.
# ===========================================================================
write_stall_proxy() {
    cat > "$STATE/fideli-rpc-stall.py" <<'EOF'
#!/usr/bin/env python3
# fideli-rpc-stall — stalling JSON-RPC unix-socket proxy (crash-window rig).
# argv: LISTEN_SOCK REAL_SOCK STALL_METHOD MARKER_FILE
import socket
import sys
import threading

listen, real, stall, marker = sys.argv[1:5]
needle = ('"method":"%s"' % stall).encode()
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(listen)
srv.listen(8)


def relay(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d:
                break
            b.sendall(d)
    except OSError:
        pass
    finally:
        try:
            b.shutdown(socket.SHUT_WR)
        except OSError:
            pass


while True:
    c, _ = srv.accept()
    c.settimeout(60)
    try:
        first = c.recv(65536)
    except OSError:
        c.close()
        continue
    if not first:
        c.close()
        continue
    if needle in first:
        # The window is open: the verb sits parked in its response read.
        open(marker, "w").close()
        threading.Event().wait()  # hold forever; the harness kills us
    r = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    r.settimeout(60)
    r.connect(real)
    r.sendall(first)
    threading.Thread(target=relay, args=(c, r), daemon=True).start()
    relay(r, c)
    for s in (c, r):
        try:
            s.close()
        except OSError:
            pass
EOF
}

# run_windowed_verb STALL_METHOD OUT_FILE VERB-ARGS...
# Runs `squeezefs nvmeof VERB-ARGS` against the stalling proxy, SIGKILLs it
# once the window opens. Returns 0 when the kill landed inside the window.
run_windowed_verb() {
    local stall=$1 out=$2 fake_run marker proxy_pid cli_pid
    shift 2
    fake_run="$STATE/fake-run-window"
    marker="$fake_run/window-entered"
    rm -rf "$fake_run"
    mkdir -p "$fake_run"
    cp "$SQUEEZEFS_NVMEOF_RUN_DIR/spdk_tgt.pid" "$fake_run/" || return 1
    python3 "$STATE/fideli-rpc-stall.py" "$fake_run/spdk.sock" \
        "$SQUEEZEFS_NVMEOF_RUN_DIR/spdk.sock" "$stall" "$marker" &
    proxy_pid=$!
    for _ in $(seq 1 50); do
        [ -S "$fake_run/spdk.sock" ] && break
        sleep 0.1
    done
    SQUEEZEFS_NVMEOF_RUN_DIR="$fake_run" "$FIDELI_BIN" nvmeof "$@" > "$out" 2>&1 &
    cli_pid=$!
    for _ in $(seq 1 150); do
        [ -f "$marker" ] && break
        kill -0 "$cli_pid" 2>/dev/null || break
        sleep 0.1
    done
    if [ ! -f "$marker" ]; then
        kill -9 "$cli_pid" 2>/dev/null
        kill -9 "$proxy_pid" 2>/dev/null
        wait "$cli_pid" 2>/dev/null
        rm -rf "$fake_run"
        return 1
    fi
    kill -9 "$cli_pid" 2>/dev/null # the crash, inside the held window
    wait "$cli_pid" 2>/dev/null
    kill -9 "$proxy_pid" 2>/dev/null
    wait "$proxy_pid" 2>/dev/null
    rm -rf "$fake_run"
    return 0
}

leg_crash_spdk() {
    local nqn zb out state rc
    write_stall_proxy

    # --- W-S1: killed between intent-record and the first target mutation
    # (stall bdev_aio_create) -> pending intent, ZERO live objects -> GC'd.
    nqn="nqn.2026-07.io.squeezefs:fideli-ws1"
    zb=$("$SUBSTRATE" mkzram $((512 * 1024 * 1024)) crash-ws1)
    if run_windowed_verb bdev_aio_create "$STATE/legs/ws1-share.txt" \
        share "$zb" --ip 127.0.0.1 --port 4631 --subnqn "$nqn"; then
        ok "W-S1: share SIGKILLed inside the pre-mutation window (bdev_aio_create withheld)"
    else
        bad "W-S1: window never opened: $(tail -3 "$STATE/legs/ws1-share.txt")"
        return
    fi
    state=$(ledger_state_of "$nqn")
    if [ "$state" = "pending" ]; then
        ok "W-S1: pending intent record survives the crash"
    else
        bad "W-S1: ledger state '$state' (want pending)"
    fi
    "$FIDELI_BIN" nvmeof list --json 2>/dev/null > "$STATE/legs/ws1-list.json"
    if jq -e ".shares[] | select(.subnqn==\"$nqn\") | .classification | test(\"pending\")" \
        "$STATE/legs/ws1-list.json" >/dev/null; then
        ok "W-S1: list shows the pending intent with the reconciliation action named"
    else
        bad "W-S1: list classification"
    fi
    out=$("$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port 4650 \
        --subnqn "${nqn}-dup" 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "$nqn"; then
        ok "W-S1: duplicate guard still refuses MID-WINDOW naming the pending holder"
    else
        bad "W-S1: mid-window duplicate guard: $out"
    fi
    out=$("$FIDELI_BIN" nvmeof restore --target-stack spdk 2>&1)
    if echo "$out" | grep -q "$nqn: pending intent garbage-collected"; then
        ok "W-S1: restore GC'd the pending intent LOUDLY (no live objects)"
    else
        bad "W-S1: restore: $out"
    fi
    if [ -z "$(ledger_state_of "$nqn")" ]; then
        ok "W-S1: record gone after GC"
    else
        bad "W-S1: record residue"
    fi

    # --- W-S2: killed between the last mutation and finalize. The stall
    # method is framework_get_subsystems — the FIRST wire call of the
    # product's save_config aggregation (src/nvmeof/spdk/lifecycle.rs:
    # save_config = framework_get_subsystems + framework_get_config +
    # local atomic write; "save_config" itself is never on the wire) —
    # so the kill lands after apply_share's last mutation, before the
    # ledger finalize. Pending intent + live objects MATCH -> finalized.
    nqn="nqn.2026-07.io.squeezefs:fideli-ws2"
    zb=$("$SUBSTRATE" mkzram $((512 * 1024 * 1024)) crash-ws2)
    if run_windowed_verb framework_get_subsystems "$STATE/legs/ws2-share.txt" \
        share "$zb" --ip 127.0.0.1 --port 4632 --subnqn "$nqn"; then
        ok "W-S2: share SIGKILLed inside the pre-finalize window (framework_get_subsystems withheld)"
    else
        bad "W-S2: window never opened"
        return
    fi
    if [ "$(ledger_state_of "$nqn")" = "pending" ]; then
        ok "W-S2: pending intent survives"
    else
        bad "W-S2: ledger state"
    fi
    if [ -n "$FIDELI_RPCPY" ]; then
        if "$FIDELI_RPCPY" -s "$SQUEEZEFS_NVMEOF_RUN_DIR/spdk.sock" nvmf_get_subsystems 2>/dev/null |
            jq -e ".[] | select(.nqn==\"$nqn\")" >/dev/null; then
            ok "W-S2: live objects exist (mutations landed before the crash)"
        else
            bad "W-S2: live objects missing"
        fi
    fi
    out=$("$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port 4650 --subnqn "${nqn}-dup" 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ]; then
        ok "W-S2: duplicate guard refuses mid-window (live+ledger sources)"
    else
        bad "W-S2: mid-window dup guard accepted: $out"
    fi
    out=$("$FIDELI_BIN" nvmeof restore --target-stack spdk 2>&1)
    if echo "$out" | grep -q "$nqn: pending intent finalized" &&
        echo "$out" | grep -q "config saved"; then
        ok "W-S2: restore finalized the pending intent AND re-saved the config (§6.4 persistence law)"
    else
        bad "W-S2: restore: $out"
    fi
    if [ "$(ledger_state_of "$nqn")" = "active" ]; then
        ok "W-S2: record active after finalize"
    else
        bad "W-S2: record state after finalize"
    fi
    if grep -q "$nqn" "$SQUEEZEFS_NVMEOF_STATE_DIR/spdk/tgt-config.json"; then
        ok "W-S2: tgt-config now carries the finalized share"
    else
        bad "W-S2: tgt-config"
    fi
    if "$FIDELI_BIN" nvmeof unshare "$nqn" > /dev/null 2>&1; then
        ok "W-S2: unshare clean"
    else
        bad "W-S2: unshare"
    fi

    # --- W-S3: unshare killed between the removing-intent and teardown
    # (stall the first teardown RPC) -> removing record, objects live ->
    # restore RESUMES the teardown.
    nqn="nqn.2026-07.io.squeezefs:fideli-ws3"
    zb=$("$SUBSTRATE" mkzram $((512 * 1024 * 1024)) crash-ws3)
    "$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port 4633 --subnqn "$nqn" \
        > "$STATE/legs/ws3-share.txt" 2>&1 || { bad "W-S3: setup share failed"; return; }
    if run_windowed_verb nvmf_subsystem_remove_listener "$STATE/legs/ws3-unshare.txt" \
        unshare "$nqn"; then
        ok "W-S3: unshare SIGKILLed inside the teardown window (remove_listener withheld)"
    else
        bad "W-S3: window never opened"
        return
    fi
    if [ "$(ledger_state_of "$nqn")" = "removing" ]; then
        ok "W-S3: removing intent survives the crash"
    else
        bad "W-S3: ledger state"
    fi
    "$FIDELI_BIN" nvmeof list --json 2>/dev/null > "$STATE/legs/ws3-list.json"
    if jq -e ".shares[] | select(.subnqn==\"$nqn\") | .classification | test(\"removing\")" \
        "$STATE/legs/ws3-list.json" >/dev/null; then
        ok "W-S3: list shows the removing intent with the action named"
    else
        bad "W-S3: list"
    fi
    out=$("$FIDELI_BIN" nvmeof restore --target-stack spdk 2>&1)
    if echo "$out" | grep -q "$nqn: interrupted teardown resumed" &&
        echo "$out" | grep -q "config saved"; then
        ok "W-S3: restore RESUMED the interrupted teardown + re-saved the config"
    else
        bad "W-S3: restore: $out"
    fi
    if [ -z "$(ledger_state_of "$nqn")" ]; then
        ok "W-S3: record gone"
    else
        bad "W-S3: record residue"
    fi
    if grep -q "$nqn" "$SQUEEZEFS_NVMEOF_STATE_DIR/spdk/tgt-config.json"; then
        bad "W-S3: tgt-config resurrection hazard (still carries the NQN)"
    else
        ok "W-S3: tgt-config no longer describes the share (resurrection law)"
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
# pending intent + whatever objects the crash left. (The literal
# kill-mid-verb coverage lives in leg_crash_spdk.)
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

    # --- A2: adopt-after-simulated-ledger-loss, SPDK stack. The ledger is
    # snapshotted aside so the substrate's guard records survive the leg.
    cp "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json" "$STATE/legs/ledger-backup-a2.json" ||
        { bad "A2: ledger backup failed"; return; }
    nqn="nqn.2026-07.io.squeezefs:share-fidadopt-loss-spdk"
    zb=$("$SUBSTRATE" mkzram $((1024 * 1024 * 1024)) adopt-loss-spdk)
    "$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port 4605 --subnqn "$nqn" \
        > "$STATE/legs/a2-share.txt" 2>&1 || { bad "A2: product share failed"; return; }
    uuid=$(jq -r ".shares[] | select(.subnqn==\"$nqn\") | .ns_uuid" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json")
    bdev=$(jq -r ".shares[] | select(.subnqn==\"$nqn\") | .bdev_name" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json")
    rm -f "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json"
    ok "A2: ledger loss simulated (shares.json deleted; target keeps serving)"
    if "$FIDELI_BIN" nvmeof list --json 2>/dev/null | jq -e \
        ".foreign_live[] | select(.subnqn==\"$nqn\" and .stack==\"spdk\")" >/dev/null; then
        ok "A2: orphaned share shows foreign (stack=spdk)"
    else
        bad "A2: foreign list"
    fi
    if [ -n "$FIDELI_RPCPY" ]; then
        "$FIDELI_RPCPY" -s "$SQUEEZEFS_NVMEOF_RUN_DIR/spdk.sock" nvmf_get_subsystems \
            > "$STATE/legs/a2-live-before.json" 2>/dev/null
    fi
    out=$("$FIDELI_BIN" nvmeof adopt "$nqn" 2>&1)
    rc=$?
    echo "$out" > "$STATE/legs/a2-adopt.txt"
    if [ "$rc" -eq 0 ] && echo "$out" | grep -q "ledger-loss" && echo "$out" | grep -q "spdk stack"; then
        ok "A2: adopt absorbed the orphan (class ledger-loss)"
    else
        bad "A2: adopt: $out"
    fi
    if [ -n "$FIDELI_RPCPY" ]; then
        "$FIDELI_RPCPY" -s "$SQUEEZEFS_NVMEOF_RUN_DIR/spdk.sock" nvmf_get_subsystems \
            > "$STATE/legs/a2-live-after.json" 2>/dev/null
        if diff -q "$STATE/legs/a2-live-before.json" "$STATE/legs/a2-live-after.json" >/dev/null; then
            ok "A2: ZERO target mutation (live inventory identical across adopt)"
        else
            bad "A2: live inventory changed"
        fi
    fi
    rec=$(jq -c ".shares[] | select(.subnqn==\"$nqn\")" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json")
    if echo "$rec" | jq -e ".state==\"active\" and .stack==\"spdk\" and .nsid==1 \
        and (.ns_uuid|ascii_downcase)==(\"$uuid\"|ascii_downcase) and .bdev_name==\"$bdev\" \
        and .adopted_from.class==\"ledger-loss\"" >/dev/null; then
        ok "A2: record re-binds the live identity (nsid/uuid/bdev + provenance)"
    else
        bad "A2: adopted record: $rec"
    fi
    if [ -f "$SQUEEZEFS_NVMEOF_STATE_DIR/spdk/ptpl/$uuid.json" ]; then
        if echo "$rec" | jq -e ".ptpl_file==\"spdk/ptpl/$uuid.json\"" >/dev/null; then
            ok "A2: surviving state-dir ptpl file re-bound"
        else
            bad "A2: ptpl not re-bound"
        fi
    else
        if echo "$rec" | jq -e ".ptpl_file==\"spdk/ptpl/$uuid.json\" or .ptpl_file==null" >/dev/null; then
            ok "A2: ptpl shape consistent (no PR activity yet)"
        else
            bad "A2: ptpl shape: $rec"
        fi
    fi
    if grep -q "$nqn" "$SQUEEZEFS_NVMEOF_STATE_DIR/spdk/tgt-config.json"; then
        ok "A2: save_config truth capture (tgt-config describes the adopted share)"
    else
        bad "A2: tgt-config misses the adopted share"
    fi
    cfgmd5a=$(md5sum "$SQUEEZEFS_NVMEOF_STATE_DIR/spdk/tgt-config.json" | awk '{print $1}')
    out=$("$FIDELI_BIN" nvmeof restore --target-stack spdk 2>&1)
    cfgmd5b=$(md5sum "$SQUEEZEFS_NVMEOF_STATE_DIR/spdk/tgt-config.json" | awk '{print $1}')
    if echo "$out" | grep -q "$nqn: already live — verified no-op" && [ "$cfgmd5a" = "$cfgmd5b" ]; then
        ok "A2: restore verified no-op, tgt-config untouched (no-op skips save)"
    else
        bad "A2: restore after adopt: $out"
    fi
    if "$FIDELI_BIN" nvmeof unshare "$nqn" >/dev/null 2>&1; then
        ok "A2: unshare adopted share"
    else
        bad "A2: unshare"
    fi
    mv "$STATE/legs/ledger-backup-a2.json" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json"
    if "$FIDELI_BIN" nvmeof list --json 2>/dev/null | jq -e \
        "[.shares[] | select(.classification==\"managed\" and .live==true)] | length >= 4" >/dev/null; then
        ok "A2: guard records restored from the ledger backup (still live)"
    else
        bad "A2: ledger restore"
    fi

    # --- A3: adopt-after-simulated-ledger-loss, nvmet stack (§6.10 pt 5
    # names BOTH stacks for this scenario).
    cp "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json" "$STATE/legs/ledger-backup-a3.json" ||
        { bad "A3: ledger backup failed"; return; }
    nqn="nqn.2026-07.io.squeezefs:share-fidadopt-loss-nvmet"
    zb=$("$SUBSTRATE" mkzram $((1024 * 1024 * 1024)) adopt-loss-nvmet)
    # 4539 -> id 54060.
    "$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port 4539 --subnqn "$nqn" \
        --target-stack nvmet > "$STATE/legs/a3-share.txt" 2>&1 || { bad "A3: product share failed"; return; }
    uuid=$(jq -r ".shares[] | select(.subnqn==\"$nqn\") | .ns_uuid" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json")
    rm -f "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json"
    if "$FIDELI_BIN" nvmeof list --json 2>/dev/null | jq -e \
        ".foreign_live[] | select(.subnqn==\"$nqn\" and .stack==\"nvmet\")" >/dev/null; then
        ok "A3: orphaned nvmet share shows foreign"
    else
        bad "A3: foreign list"
    fi
    out=$("$FIDELI_BIN" nvmeof adopt "$nqn" 2>&1)
    rc=$?
    if [ "$rc" -eq 0 ] && echo "$out" | grep -q "ledger-loss" && echo "$out" | grep -q "nvmet stack"; then
        ok "A3: adopt absorbed the orphan (class ledger-loss, nvmet)"
    else
        bad "A3: adopt: $out"
    fi
    rec=$(jq -c ".shares[] | select(.subnqn==\"$nqn\")" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json")
    if echo "$rec" | jq -e ".state==\"active\" and .stack==\"nvmet\" \
        and (.ns_uuid|ascii_downcase)==(\"$uuid\"|ascii_downcase) \
        and .listeners[0].nvmet_port_id==54060 \
        and .adopted_from.class==\"ledger-loss\"" >/dev/null; then
        ok "A3: record re-binds identity + the actual serving port id (54060)"
    else
        bad "A3: adopted record: $rec"
    fi
    out=$("$FIDELI_BIN" nvmeof restore --target-stack nvmet 2>&1)
    if echo "$out" | grep -q "$nqn: already live — verified no-op"; then
        ok "A3: restore verified no-op"
    else
        bad "A3: restore: $out"
    fi
    if "$FIDELI_BIN" nvmeof unshare "$nqn" >/dev/null 2>&1; then
        ok "A3: unshare adopted share"
    else
        bad "A3: unshare"
    fi
    if [ ! -d "$NVMET_CFS/subsystems/$nqn" ] && [ ! -d "$NVMET_CFS/ports/54060" ]; then
        ok "A3: zero target residue (subsystem + port gone)"
    else
        bad "A3: residue"
    fi
    mv "$STATE/legs/ledger-backup-a3.json" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json"
}

# ===========================================================================
# PR/PTPL matrix (pr-matrix.sh productized — full)
# ===========================================================================
leg_pr_matrix() {
    local out="$STATE/legs/pr-matrix.txt" nqn zb dev dev2 rescap report key1=0xA11CE key2=0xB0B
    local h2uuid="7b7b7b7b-2222-4222-8222-b2b2b2b2b2b2"
    local h2nqn="nqn.2014-08.org.nvmexpress:uuid:$h2uuid"
    : > "$out"

    # --- SPDK arm (the PTPL-bearing stack).
    nqn="nqn.2026-07.io.squeezefs:fideli-prmx-spdk"
    zb=$("$SUBSTRATE" mkzram $((1024 * 1024 * 1024)) prmx-spdk)
    "$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port 4580 --subnqn "$nqn" >> "$out" 2>&1 ||
        { bad "PRMX: spdk share failed"; return; }
    record "share=$nqn"
    "$FIDELI_BIN" nvmeof connect --ip 127.0.0.1 --port 4580 --subnqn "$nqn" >> "$out" 2>&1
    record "connected=$nqn"
    dev=$(finddev "$nqn") || { bad "PRMX: no device"; return; }

    rescap=$(nvme id-ns "$dev" -o json | jq -r .rescap)
    if [ $((rescap & 1)) = 1 ] && [ $(((rescap >> 1) & 1)) = 1 ]; then
        ok "PRMX: RESCAP carries PTPL-capable + Write-Exclusive bits (0x$(printf '%02x' "$rescap"))"
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
    nvme connect -t tcp -a 127.0.0.1 -s 4580 -n "$nqn" --hostnqn="$h2nqn" --hostid="$h2uuid" >> "$out" 2>&1
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

    # PTPL across a PRODUCT-VERB target power cycle: SIGKILL by OUR
    # pidfile -> `nvmeof target start` (load_config replays tgt-config).
    local spid persisted=""
    spid=$(cat "$SQUEEZEFS_NVMEOF_RUN_DIR/spdk_tgt.pid")
    kill -9 "$spid"
    sleep 1
    if "$FIDELI_BIN" nvmeof target start >> "$out" 2>&1; then
        ok "PRMX: product 'target start' after target SIGKILL (load_config)"
    else
        bad "PRMX: restart"
    fi
    record "spdk_pid=$(cat "$SQUEEZEFS_NVMEOF_RUN_DIR/spdk_tgt.pid" 2>/dev/null)"
    for _ in $(seq 1 60); do
        report=$(nvme resv-report "$dev2" --eds -o json 2>/dev/null)
        if [ -n "$report" ]; then
            if echo "$report" | jq -e ".regctl==1 and .rtype==1 and
                ([.regctlext[]?.rkey] | index($((key2))))" >/dev/null 2>&1; then
                persisted=1
                break
            fi
        fi
        sleep 2
    done
    if [ -n "$persisted" ]; then
        ok "PRMX: reservation SURVIVED the target power cycle (PTPL: holder $key2, rtype 1)"
    else
        bad "PRMX: PTPL persistence (report: $(echo "$report" | jq -c '{regctl, rtype}' 2>/dev/null))"
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
        ok "PRMX: spdk unshare"
    else
        bad "PRMX: unshare"
    fi

    # --- nvmet arm: fence + preempt (no PTPL claims — nvmet ptpls=0 by
    # design; the §6.7 errno contract is what this asserts).
    nqn="nqn.2026-07.io.squeezefs:fideli-prmx-nvmet"
    zb=$("$SUBSTRATE" mkzram $((512 * 1024 * 1024)) prmx-nvmet)
    "$FIDELI_BIN" nvmeof share "$zb" --ip 127.0.0.1 --port 4555 --subnqn "$nqn" \
        --target-stack nvmet >> "$out" 2>&1 || { bad "PRMX-N: share failed"; return; }
    record "share=$nqn"
    "$FIDELI_BIN" nvmeof connect --ip 127.0.0.1 --port 4555 --subnqn "$nqn" >> "$out" 2>&1
    record "connected=$nqn"
    dev=$(finddev "$nqn") || { bad "PRMX-N: no device"; return; }
    nvme resv-register "$dev" --nrkey="$key1" --rrega=0 --iekey --cptpl=3 >> "$out" 2>&1
    if nvme resv-acquire "$dev" --crkey="$key1" --rtype=1 --racqa=0 >> "$out" 2>&1; then
        ok "PRMX-N: register + acquire WE on kernel nvmet"
    else
        bad "PRMX-N: register/acquire"
    fi
    nvme disconnect -n "$nqn" >> "$out" 2>&1
    sleep 1
    nvme connect -t tcp -a 127.0.0.1 -s 4555 -n "$nqn" --hostnqn="$h2nqn" --hostid="$h2uuid" >> "$out" 2>&1
    dev2=$(finddev "$nqn") || { bad "PRMX-N: host2 device"; return; }
    if dd if=/dev/zero of="$dev2" bs=4096 count=1 oflag=direct conv=notrunc >> "$out" 2>&1; then
        bad "PRMX-N: non-holder write SUCCEEDED on nvmet"
    else
        ok "PRMX-N: non-holder write rejected (fence errno contract holds on nvmet)"
    fi
    nvme resv-register "$dev2" --nrkey="$key2" --rrega=0 --iekey --cptpl=3 >> "$out" 2>&1
    if nvme resv-acquire "$dev2" --crkey="$key2" --rtype=1 --racqa=1 --prkey="$key1" >> "$out" 2>&1; then
        ok "PRMX-N: preempt on nvmet"
    else
        bad "PRMX-N: preempt"
    fi
    {
        nvme resv-release "$dev2" --crkey="$key2" --rtype=1 --rrela=0
        nvme resv-register "$dev2" --crkey="$key2" --rrega=1
        nvme disconnect -n "$nqn"
    } >> "$out" 2>&1
    sleep 1
    if "$FIDELI_BIN" nvmeof unshare "$nqn" >> "$out" 2>&1; then
        ok "PRMX-N: unshare"
    else
        bad "PRMX-N: unshare"
    fi
}

# ===========================================================================
# Target-restart persistence (G2 — full)
# ===========================================================================
leg_g2_spdk() {
    local out="$STATE/legs/g2-spdk.txt" nqn_m nqn_d zm zd dev_m dev_d mnt dlog md5 md5b uuid
    local fenced reacq io_ok mode spid dpid ladder mounted reg
    local fab_base fab_nl fab_reconn_base fab_reconn fab_ctrls fab_ok
    nqn_m="nqn.2026-07.io.squeezefs:fideli-g2-meta"
    nqn_d="nqn.2026-07.io.squeezefs:fideli-g2-data"
    mnt="$STATE/mnt-g2"
    dlog="$STATE/legs/g2-daemon.log"
    : > "$out"
    : > "$dlog"
    mkdir -p "$mnt"
    zm=$("$SUBSTRATE" mkzram $((1024 * 1024 * 1024)) g2-meta)
    zd=$("$SUBSTRATE" mkzram $((2 * 1024 * 1024 * 1024)) g2-data)
    "$FIDELI_BIN" nvmeof share "$zm" --ip 127.0.0.1 --port 4621 --subnqn "$nqn_m" >> "$out" 2>&1 ||
        { bad "G2S: share meta"; return; }
    record "share=$nqn_m"
    "$FIDELI_BIN" nvmeof share "$zd" --ip 127.0.0.1 --port 4622 --subnqn "$nqn_d" >> "$out" 2>&1 ||
        { bad "G2S: share data"; return; }
    record "share=$nqn_d"
    uuid=$(jq -r ".shares[] | select(.subnqn==\"$nqn_m\") | .ns_uuid" "$SQUEEZEFS_NVMEOF_STATE_DIR/shares.json")
    "$FIDELI_BIN" nvmeof connect --ip 127.0.0.1 --port 4621 --subnqn "$nqn_m" >> "$out" 2>&1
    record "connected=$nqn_m"
    "$FIDELI_BIN" nvmeof connect --ip 127.0.0.1 --port 4622 --subnqn "$nqn_d" >> "$out" 2>&1
    record "connected=$nqn_d"
    dev_m=$(finddev "$nqn_m") || { bad "G2S: no meta device"; return; }
    dev_d=$(finddev "$nqn_d") || { bad "G2S: no data device"; return; }

    "$FIDELI_BIN" format "sqmeta://$dev_m" "sqdata://$dev_d" --force >> "$out" 2>&1 ||
        { bad "G2S: format"; return; }
    # Settle udev before mounting: format's write-then-close fires a change
    # uevent and systemd-udevd holds a BSD flock on the node while probing
    # (BLOCK_DEVICE_LOCKING) — a mount racing that probe trips the guard's
    # Layer-A flock as a phantom "another process holds the writer lock".
    udevadm settle --timeout=10 2>/dev/null || true
    RUST_LOG=info "$FIDELI_BIN" --log-file "$dlog" mount "sqmeta://$dev_m" "$mnt" \
        --daemon --allow-other >> "$out" 2>&1
    mounted=""
    for _ in $(seq 1 60); do
        awk -v m="$mnt" '$2==m{f=1} END{exit !f}' /proc/mounts && { mounted=1; break; }
        sleep 0.5
    done
    [ -n "$mounted" ] || { bad "G2S: mount did not appear"; return; }
    sleep 2
    mode=$(mnt_stat "$mnt" writer_guard_mode)
    if [ "$mode" = "flock+pr" ]; then
        ok "G2S: writer_guard_mode=flock+pr on PRODUCT-shared namespaces (G4)"
    else
        bad "G2S: guard mode=$mode"
    fi
    dd if=/dev/urandom of="$mnt/g2.bin" bs=1M count=16 2>>"$out" && sync
    md5=$(md5sum "$mnt/g2.bin" | awk '{print $1}')
    if [ -f "$SQUEEZEFS_NVMEOF_STATE_DIR/spdk/ptpl/$uuid.json" ]; then
        ok "G2S: ptpl_file materialized (reservation persisted to state dir)"
    else
        bad "G2S: ptpl file missing"
    fi

    # [PR 6] fabric_* baseline (design §6.9): the daemon's sampler beats
    # every 10 s — poll until it has published a settled view of this
    # mount's two fabric controllers (meta + data, both live) before we
    # yank the target. The settled baseline also makes the reconnect
    # assertion below deterministic: with a live sample banked and a
    # not-live sample observed in the down window, the post-reattach
    # not-live->live transition MUST be counted.
    fab_base=""
    fab_nl=""
    for _ in $(seq 1 20); do
        fab_base=$(mnt_stat "$mnt" fabric_controllers)
        fab_nl=$(mnt_stat "$mnt" fabric_ctrl_not_live)
        [ "${fab_base:-0}" -ge 2 ] && [ "${fab_nl:-1}" = "0" ] && break
        sleep 2
    done
    if [ "${fab_base:-0}" -ge 2 ] && [ "${fab_nl:-1}" = "0" ]; then
        ok "G2S: fabric_* baseline settled (controllers=$fab_base, not_live=0)"
    else
        bad "G2S: fabric baseline (controllers=${fab_base:-?} not_live=${fab_nl:-?})"
    fi
    fab_reconn_base=$(mnt_stat "$mnt" fabric_ctrl_reconnects)
    fab_reconn_base=${fab_reconn_base:-0}
    # [PR 6] squeezefs status renders the per-volume Fabric section off
    # the same sysfs source (both backing devices of this volume are
    # fabric-attached -> exactly 2 controller identities, NQN/addr/state
    # rows present).
    if "$FIDELI_BIN" status "sqmeta://$dev_m" > "$STATE/legs/g2-status.json" 2>>"$out" &&
        jq -e '.Fabric.fabric_controllers == 2 and .Fabric.fabric_ctrl_not_live == 0 and
               ([.Fabric.Controllers[].SubsysNqn] | length >= 2)' \
            "$STATE/legs/g2-status.json" >/dev/null; then
        ok "G2S: squeezefs status Fabric section renders (2 live controllers, NQN rows)"
    else
        bad "G2S: status Fabric section: $(jq -c '.Fabric // "absent"' "$STATE/legs/g2-status.json" 2>/dev/null)"
    fi

    # THE TARGET-RESTART PERSISTENCE WINDOW (G2). SIGKILL the target under
    # a live mount, restart through the product, and prove IO resumes with
    # the reservation intact. PR 6 gauge assertions ride this window
    # (design §6.9 / PR-plan PR 6 — the harness edit this PR owns).
    spid=$(cat "$SQUEEZEFS_NVMEOF_RUN_DIR/spdk_tgt.pid")
    kill -9 "$spid" || { bad "G2S: SIGKILL spdk_tgt"; return; }
    sleep 1
    # [PR 6] fabric_ctrl_not_live must RISE while the target is dead:
    # spdk_tgt's death resets the TCP connections, the kernel initiator
    # flips both controllers to `connecting`, and the next sampler beat
    # publishes it. Holding `target start` until the gauge moves keeps
    # the down window >= one observed not-live sample — the precondition
    # the settle assertion below builds on. (Sampled-transition law:
    # without this hold, a bounce faster than the 10 s cadence may
    # legitimately count zero.)
    fab_nl=""
    for _ in $(seq 1 30); do
        fab_nl=$(mnt_stat "$mnt" fabric_ctrl_not_live)
        [ "${fab_nl:-0}" -ge 1 ] && break
        sleep 2
    done
    if [ "${fab_nl:-0}" -ge 1 ]; then
        ok "G2S: fabric_ctrl_not_live rose while the target is dead (not_live=$fab_nl)"
    else
        bad "G2S: fabric_ctrl_not_live never rose in the down window"
    fi
    if "$FIDELI_BIN" nvmeof target start >> "$out" 2>&1 &&
        grep -q "load_config applied" "$out"; then
        ok "G2S: target start after SIGKILL replayed the SPDK source of truth"
    else
        bad "G2S: restart: $(tail -3 "$out")"
    fi
    record "spdk_pid=$(cat "$SQUEEZEFS_NVMEOF_RUN_DIR/spdk_tgt.pid" 2>/dev/null)"
    if [ -n "$FIDELI_RPCPY" ]; then
        "$FIDELI_RPCPY" -s "$SQUEEZEFS_NVMEOF_RUN_DIR/spdk.sock" nvmf_get_subsystems \
            > "$STATE/legs/g2-live.json" 2>/dev/null
        if jq -e ".[] | select(.nqn==\"$nqn_m\") | .namespaces[0] |
                ((.uuid|ascii_downcase)==(\"$uuid\"|ascii_downcase))" "$STATE/legs/g2-live.json" >/dev/null; then
            ok "G2S: share reappeared under the SAME NQN/nsid/UUID"
        else
            bad "G2S: post-restart identity"
        fi
    fi
    io_ok=""
    for _ in $(seq 1 60); do
        dd if="$mnt/g2.bin" of=/dev/null bs=1M count=1 2>/dev/null && { io_ok=1; break; }
        sleep 2
    done
    sleep 12 # one heartbeat re-check past reattach
    md5b=$(md5sum "$mnt/g2.bin" 2>/dev/null | awk '{print $1}')
    fenced=$(mnt_stat "$mnt" writer_guard_fenced)
    reacq=$(mnt_stat "$mnt" writer_guard_pr_reacquires)
    if [ -n "$io_ok" ] && [ "$md5b" = "$md5" ]; then
        ok "G2S: IO RESUMED through the bounce without operator action (data intact)"
    else
        bad "G2S: IO resume (ok=${io_ok:-no} md5=$md5b)"
    fi
    if [ "${fenced:-1}" = "0" ] && [ "${reacq:-1}" = "0" ]; then
        ok "G2S: reservation intact via PTPL (fenced=0, pr_reacquires=0 — §6.7 signal)"
    else
        bad "G2S: PTPL survival (fenced=$fenced reacq=$reacq)"
    fi
    if dd if=/dev/urandom of="$mnt/g2-post.bin" bs=1M count=4 2>/dev/null && sync; then
        ok "G2S: post-bounce writes land"
    else
        bad "G2S: post-bounce write"
    fi

    # [PR 6] fabric_* gauges settle across the bounce (design §6.9):
    # not_live back to 0 and the sampled-transition reconnect counter
    # incremented — the down window above banked a not-live sample per
    # controller, so the first post-reattach beat must observe the
    # not-live->live transition. fabric_controllers stays at its
    # baseline (same endpoints, reattached — population stable).
    fab_ok=""
    fab_reconn=""
    fab_ctrls=""
    for _ in $(seq 1 30); do
        fab_nl=$(mnt_stat "$mnt" fabric_ctrl_not_live)
        fab_reconn=$(mnt_stat "$mnt" fabric_ctrl_reconnects)
        fab_ctrls=$(mnt_stat "$mnt" fabric_controllers)
        [ "${fab_nl:-1}" = "0" ] && [ "${fab_reconn:-0}" -gt "$fab_reconn_base" ] &&
            { fab_ok=1; break; }
        sleep 2
    done
    if [ -n "$fab_ok" ]; then
        ok "G2S: fabric gauges settled (not_live=0, reconnects $fab_reconn_base -> $fab_reconn)"
    else
        bad "G2S: fabric settle (not_live=${fab_nl:-?} reconnects=${fab_reconn:-?} base=$fab_reconn_base)"
    fi
    if [ "${fab_ctrls:-0}" = "$fab_base" ]; then
        ok "G2S: fabric_controllers stable across the bounce (n=$fab_ctrls)"
    else
        bad "G2S: fabric_controllers moved across the bounce ($fab_base -> ${fab_ctrls:-?})"
    fi

    dpid=$(pgrep -f "squeezefs.*mount sqmeta://$dev_m" | head -1)
    [ -n "${dpid:-}" ] || { bad "G2S: no daemon pid to kill"; return; }
    kill -9 "$dpid"
    sleep 2
    umount -l "$mnt" 2>/dev/null
    sleep 1
    udevadm settle --timeout=10 2>/dev/null || true # (same udev-flock race as above)
    RUST_LOG=info "$FIDELI_BIN" --log-file "$dlog" mount "sqmeta://$dev_m" "$mnt" \
        --daemon --allow-other >> "$out" 2>&1
    mounted=""
    for _ in $(seq 1 60); do
        awk -v m="$mnt" '$2==m{f=1} END{exit !f}' /proc/mounts && { mounted=1; break; }
        sleep 0.5
    done
    if [ -n "$mounted" ]; then
        sleep 2
        md5b=$(md5sum "$mnt/g2.bin" 2>/dev/null | awk '{print $1}')
        if [ "$md5b" = "$md5" ]; then
            ok "G2S: kill-9 -> remount recovered on product shares (S1 ladder path)"
        else
            bad "G2S: remount data integrity"
        fi
    else
        bad "G2S: remount did not appear"
    fi
    ladder=$(grep -c "register conflicted with our own stale" "$dlog" 2>/dev/null || true)
    if [ "${ladder:-0}" -ge 1 ]; then
        ok "G2S: register ladder fired on the spec-strict target (hits=$ladder)"
    else
        bad "G2S: ladder never fired"
    fi
    if umount "$mnt" >> "$out" 2>&1; then
        ok "G2S: clean unmount"
    else
        bad "G2S: clean unmount"
    fi
    sleep 1
    reg=$(nvme resv-report "$dev_m" --eds -o json 2>/dev/null | jq -r .regctl)
    if [ "${reg:-x}" = "0" ]; then
        ok "G2S: zero PR residue after clean unmount"
    else
        bad "G2S: PR residue regctl=$reg"
    fi
    "$FIDELI_BIN" nvmeof disconnect "$nqn_m" >> "$out" 2>&1
    "$FIDELI_BIN" nvmeof disconnect "$nqn_d" >> "$out" 2>&1
    sleep 1
    if "$FIDELI_BIN" nvmeof unshare "$nqn_m" >> "$out" 2>&1; then
        ok "G2S: unshare meta"
    else
        bad "G2S: unshare meta"
    fi
    if "$FIDELI_BIN" nvmeof unshare "$nqn_d" >> "$out" 2>&1; then
        ok "G2S: unshare data"
    else
        bad "G2S: unshare data"
    fi
}

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
# addr_trtype="tcp" (write_attr at ensure_port), src/nvmeof/spdk/mod.rs
# creates TCP transports/listeners only, and `share` exposes no --trtype.
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

    log "RXE RESIDUAL (PR 7): product listener plumbing cannot express trtype=rdma — 'nvmeof share' has no --trtype, src/nvmeof/nvmet.rs pins addr_trtype=tcp, src/nvmeof/spdk/mod.rs pins TCP transport/listeners; this leg's rdma listener is harness-built on the product-shared subsystem."
    log "RXE RESIDUAL (PR 7): the SPDK arm cannot serve RDMA at all — the pinned v26.05 build is configured without --with-rdma (SPDK_CONFIGURE_ARGS: --disable-tests --disable-unit-tests --disable-examples only)."
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
    # Rides the raw guard-data namespaces BEFORE the guard legs (which
    # re-wipe + reformat them at their leg 0/1). Recorded rows only —
    # ordered A/B belongs to PR 7's bench rerun. Instrument: fio io_uring,
    # O_DIRECT, railed to cores 0-15. Engine policy note (2026-08-07,
    # `.benchmarks/2026-08-07-fio-engine-policy.md` rule 5): io_uring is
    # the sanctioned KERNEL-LANE raw-device instrument — these rows never
    # ride the shim (which cannot interpose io_uring) and are labeled
    # raw-ceiling rows, not FUSE-lane numbers.
    local arm dev row iops
    for arm in spdk nvmet; do
        case "$arm" in
        spdk) dev="$DEV_GDATA_SPDK" ;;
        nvmet) dev="$DEV_GDATA_NVMET" ;;
        esac
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
    done
}

# ===========================================================================
# Guard legs (tests/guard_smoke.sh — quick: 1 cycle; full: x10 + PTPL)
# ===========================================================================
leg_guard_spdk() {
    local loops=$1
    local ptpl_flag=()
    [ "$MODE" = full ] && ptpl_flag=(--ptpl)
    if "$GUARD_SMOKE" --stack spdk --loops "$loops" "${ptpl_flag[@]}" >> "$LOG" 2>&1; then
        ok "guard smoke spdk (loops=$loops${ptpl_flag[0]:+, ptpl}) GREEN — transcript $STATE/guard-smoke-spdk.txt"
    else
        bad "guard smoke spdk FAILED — see $STATE/guard-smoke-spdk.txt"
    fi
}

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
        ok "substrate up (product-verb-driven, both stacks)"
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
    log "=== NVMe-oF dual-stack fidelity tier: $MODE @ $(date -Is) ==="
    log "binary: ${FIDELI_SQZ_BIN:-$REPO/target/release/squeezefs} ($(md5sum "${FIDELI_SQZ_BIN:-$REPO/target/release/squeezefs}" | awk '{print $1}'))"
    log "Tctl: $(tctl || echo n/a)°C"

    run_leg substrate-up leg_substrate_up

    run_leg roundtrip-nvmet leg_roundtrip_nvmet
    run_leg roundtrip-spdk leg_roundtrip_spdk

    if [ "$MODE" = full ]; then
        run_leg loud-fail-matrix leg_loudfail
        run_leg crash-window-spdk leg_crash_spdk
        run_leg crash-window-nvmet leg_crash_nvmet
        run_leg adopt leg_adopt
        run_leg pr-matrix leg_pr_matrix
        run_leg g2-persistence-spdk leg_g2_spdk
        run_leg g2-persistence-nvmet leg_g2_nvmet
        run_leg soft-roce leg_softroce
        run_leg ab-smoke leg_ab_smoke
        run_leg guard-spdk-x10 leg_guard_spdk 10
        run_leg guard-nvmet-x10 leg_guard_nvmet 10
    else
        run_leg guard-spdk-x1 leg_guard_spdk 1
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
