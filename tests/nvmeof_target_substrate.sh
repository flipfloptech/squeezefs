#!/usr/bin/env bash
# tests/nvmeof_target_substrate.sh — dual-stack NVMe-oF target fidelity substrate
# ===============================================================================
#
# Stands up (and tears down to ZERO residue) the real-kernel fabric the
# NVMe-oF fidelity tier runs against (docs/design-nvmeof-target-management.md
# §6.8): BOTH target stacks — SPDK (spdk_tgt, JSON-RPC) and kernel nvmet
# (configfs) — serving zram-backed namespaces, stood up **through the
# product's own lifecycle verbs** (target setup/start, share, connect).
# The harness supplies only backings and assertions.
#
# This productizes `.agents/spdk-scoping/{rig-up,teardown,snapshot}.sh`
# under the tests/dev_substrate.sh ownership conventions.
#
# Verbs
#   create        build the substrate (idempotent: healthy ⇒ status + exit 0;
#                 stale/partial state is torn down and rebuilt). Writes the
#                 before-snapshot the teardown residue diff is judged against.
#   teardown      remove ONLY manifest-recorded / ledger-recorded objects,
#                 restore hugepages to the product-recorded prior, then take
#                 the after-snapshot and DIFF it against create's before-
#                 snapshot — a non-empty diff exits nonzero (the §6.8
#                 zero-residue proof, built in).
#   status        table of guard shares, target health, ledger summary
#   mkzram        <bytes> <label> — mint a manifest-recorded zram backing for
#                 a fidelity leg (never index 0); echoes /dev/zramN
#   snapshot      <label> — write $STATE/snap-<label>.txt (stable sections)
#   env           print the path of the env file consumers source
#
# What create provisions (all product-verb-driven, §6.8)
#   * state under its own namespace: $FIDELI_STATE (default
#     /tmp/squeezefs-fideli) — relocated SQUEEZEFS_NVMEOF_STATE_DIR/_RUN_DIR
#     live beneath it (the sanctioned §6.8 relocation seams; the host's real
#     /var/lib + /run/squeezefs state is never touched)
#   * nvmet port ids confined to the TEST-RESERVED SLICE 54000–54099:
#     SQUEEZEFS_NVMET_PORT_ID_BASE=54000, and every fidelity listener TCP
#     port is chosen so its fnv1a64 first-candidate offset < 100 (disjoint
#     from product 53000–53999, devsub 52026, scoping rig 52470/52471)
#   * SPDK binary: a pre-existing verified /opt/squeezefs/spdk/v26.05 pin is
#     preferred; else the sanctioned scoping build via SQUEEZEFS_SPDK_TGT_BIN
#     (loud, unpinned — §6.5's blessed rig posture); else `nvmeof target
#     install` (~35 s railed build; /opt is then manifest-recorded and
#     removed at teardown — the n3/n4 create-discipline)
#   * hugepages via `nvmeof target setup --hugemem-mb 2048` (the PRODUCT
#     records the prior; teardown restores via --restore-prior, with a raw
#     sysfs fallback if the product record survived a crash)
#   * spdk_tgt via `nvmeof target start` (pidfile mode)
#   * standing guard shares, 2 per stack (meta 2G + data 8G zram), NQNs
#     carrying the fidelity ownership marker: nqn.2026-07.io.squeezefs:fideli-*
#     (`:fideli-` is in the product's HARNESS_NQN_MARKERS — `nvmeof adopt`
#     refuses this fabric by name), connected via `nvmeof connect`
#
# Env knobs
#   FIDELI_STATE=/tmp/squeezefs-fideli   state/artifact root (fresh-cleaned at
#                                        create; artifacts survive until the
#                                        next create for post-mortem)
#   FIDELI_SQZ_BIN=<repo>/target/release/squeezefs   product binary
#   FIDELI_SCOPING_TGT=/var/tmp/spdk-scoping/spdk/build/bin/spdk_tgt
#   FIDELI_HUGEMEM_MB=2048               target setup reservation
#
# Ownership & safety policy (non-negotiable, dev_substrate.sh lineage)
#   * Every object carries the fideli marker or is recorded by exact id in
#     $STATE/manifest; teardown removes ONLY manifest/ledger entries plus
#     marker-attested orphans. Foreign nvmet trees, zram devices (zram0 =
#     user swap), user mounts (/mnt/squeezefs, /mnt/juicefs), ~/tmp/nvme,
#     containers, and /var/tmp/spdk-scoping (read-only here) are NEVER
#     touched; /etc/squeezefs/nvmeof_shares.json is never written.
#   * zram indexes come from /sys/class/zram-control/hot_add; index 0 aborts.
#   * Modules (nvmet, nvmet-tcp, nvme-tcp, and the soft-RoCE leg's rdma set)
#     are loaded on demand and NEVER unloaded on teardown — other users may
#     hold instances; a loaded idle module is harmless (dev_substrate
#     policy). rdma_rxe devices WE add (manifest `rdma_link=`) are removed.
#   * Kill only recorded pids (spdk_tgt by OUR pidfile/manifest; daemons by
#     OUR log-file path).
#
# Requires: root (re-execs via sudo), nvme-cli, jq. Consumers:
# tests/run_nvmeof_fidelity.sh (the tier orchestrator), tests/guard_smoke.sh.

set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
STATE="${FIDELI_STATE:-/tmp/squeezefs-fideli}"
BIN="${FIDELI_SQZ_BIN:-$REPO/target/release/squeezefs}"
SCOPING_TGT="${FIDELI_SCOPING_TGT:-/var/tmp/spdk-scoping/spdk/build/bin/spdk_tgt}"
HUGEMEM_MB="${FIDELI_HUGEMEM_MB:-2048}"

LEDGER_DIR="$STATE/state"   # SQUEEZEFS_NVMEOF_STATE_DIR (relocation seam)
RUN_DIR="$STATE/run"        # SQUEEZEFS_NVMEOF_RUN_DIR (relocation seam)
MANIFEST="$STATE/manifest"
ENV_FILE="$STATE/env.sh"
DEVICES_FILE="$STATE/devices.env"
PIN_PREFIX=/opt/squeezefs/spdk/v26.05
NVMET_CFS=/sys/kernel/config/nvmet
HP_SYSFS=/sys/kernel/mm/hugepages/hugepages-2048kB
PORT_ID_BASE=54000          # test-reserved slice [54000, 54099] (§6.8)

# NQNs — the `:fideli-` ownership marker is load-bearing: it is one of the
# product's HARNESS_NQN_MARKERS, so `nvmeof adopt` refuses this fabric.
NQN_PREFIX="nqn.2026-07.io.squeezefs:fideli-"
NQN_GMETA_SPDK="${NQN_PREFIX}guard-spdk-meta"
NQN_GDATA_SPDK="${NQN_PREFIX}guard-spdk-data"
NQN_GMETA_NVMET="${NQN_PREFIX}guard-nvmet-meta"
NQN_GDATA_NVMET="${NQN_PREFIX}guard-nvmet-data"
# Listener TCP ports: every nvmet-stack port below has
# fnv1a64("tcp:<ip>:<port>") % 900 < 100, keeping allocated configfs port
# ids inside [54000, 54099] (verified against src/nvmeof/nvmet.rs::fnv1a_64).
PORT_GMETA_SPDK=4560
PORT_GDATA_SPDK=4574
PORT_GUARD_NVMET=4514       # one port object (id 54085), two subsystem links

# log/warn go to stderr: several verbs (mkzram, snapshot) return values on
# stdout for $()-capture — a log line on stdout would corrupt the value
# (the bug class the predecessor draft shipped in resolve_spdk_bin).
log()  { echo "[fideli-substrate] $*" >&2; }
warn() { echo "[fideli-substrate] WARN: $*" >&2; }
die()  { echo "[fideli-substrate] ERROR: $*" >&2; exit 1; }
manifest() { echo "$1" >> "$MANIFEST"; }

usage() {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
}

ensure_root() {
    [ "$(id -u)" -eq 0 ] && return 0
    log "root required (configfs, modules, hugepages) — re-executing via sudo"
    local knobs=()
    while IFS= read -r kv; do knobs+=("$kv"); done \
        < <(env | grep -E '^(FIDELI_|SQUEEZEFS_)' || true)
    exec sudo env "${knobs[@]}" bash "$0" "$@"
}

ensure_prereqs() {
    [ -x "$BIN" ] || die "product binary missing at $BIN — cargo build --release first"
    command -v nvme >/dev/null || die "nvme-cli required"
    command -v jq >/dev/null || die "jq required"
    if ! mountpoint -q /sys/kernel/config; then
        mount -t configfs none /sys/kernel/config || die "cannot mount configfs"
    fi
    # Loaded on demand; NEVER unloaded on teardown (policy header).
    modprobe nvmet 2>/dev/null || true
    modprobe nvmet-tcp 2>/dev/null || true
    modprobe nvme-tcp 2>/dev/null || true
    [ -d "$NVMET_CFS" ] || die "nvmet configfs missing — kernel lacks nvmet?"
    [ -e /sys/class/zram-control/hot_add ] || die "zram hot_add missing — kernel lacks zram?"
}

# Product-verb environment: every squeezefs invocation in the fidelity tier
# (this script + run_nvmeof_fidelity.sh + guard_smoke.sh) runs under these.
substrate_env() {
    export SQUEEZEFS_NVMEOF_STATE_DIR="$LEDGER_DIR"
    export SQUEEZEFS_NVMEOF_RUN_DIR="$RUN_DIR"
    export SQUEEZEFS_NVMET_PORT_ID_BASE="$PORT_ID_BASE"
    unset SQUEEZEFS_NVMEOF_TARGET_STACK
}

# ---------------------------------------------------------------------------
# zram backings (hot_add; refuse index 0; manifest-recorded)
# ---------------------------------------------------------------------------
mkzram() { # size-bytes label -> echoes /dev/zramN (stdout is the value!)
    local size=$1 label=$2 idx
    mkdir -p "$STATE"
    idx=$(cat /sys/class/zram-control/hot_add)
    [ "$idx" != "0" ] || die "hot_add returned zram0 (user swap slot!) — abort"
    manifest "zram=$idx label=$label"
    echo "$size" > "/sys/block/zram$idx/disksize"
    echo "/dev/zram$idx"
}

# ---------------------------------------------------------------------------
# fabric helpers
# ---------------------------------------------------------------------------
finddev() { # nqn -> /dev/nvmeXn1 head node (native-multipath tolerant)
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

share_one() { # nqn backing port stack
    local nqn=$1 backing=$2 port=$3 stack=$4
    if ! "$BIN" nvmeof share "$backing" --ip 127.0.0.1 --port "$port" \
        --subnqn "$nqn" --target-stack "$stack" > "$STATE/share-$(basename "$nqn").txt" 2>&1; then
        die "product share of $nqn failed: $(tail -5 "$STATE/share-$(basename "$nqn").txt")"
    fi
    manifest "share=$nqn"
}

connect_one() { # nqn port -> echoes device (stdout is the value!)
    local nqn=$1 port=$2 dev
    "$BIN" nvmeof connect --ip 127.0.0.1 --port "$port" --subnqn "$nqn" \
        >> "$STATE/connect.txt" 2>&1 || die "product connect of $nqn failed"
    manifest "connected=$nqn"
    dev=$(finddev "$nqn") || die "no initiator device appeared for $nqn"
    echo "$dev"
}

# ---------------------------------------------------------------------------
# snapshot (stable sections only — the zero-residue witness; excludes
# $STATE itself and everything foreign-noisy)
# ---------------------------------------------------------------------------
cmd_snapshot() { # label -> echoes path (stdout is the value!)
    local label=${1:?snapshot needs a label}
    local out="$STATE/snap-$label.txt"
    mkdir -p "$STATE"
    {
        echo "--- hugepages 2M nr ---"
        cat "$HP_SYSFS/nr_hugepages" 2>/dev/null
        echo "--- /opt/squeezefs (present?) ---"
        if [ -d /opt/squeezefs ]; then echo present; else echo absent; fi
        echo "--- spdk_tgt processes (non-scoping) ---"
        pgrep -a spdk_tgt 2>/dev/null | grep -v spdk-scoping || echo "(none)"
        echo "--- fideli squeezefs daemons ---"
        pgrep -af "squeezefs.*log-file $STATE" 2>/dev/null || echo "(none)"
        echo "--- zram ---"
        find /dev -maxdepth 1 -name 'zram*' 2>/dev/null | sort
        echo "--- fideli fabric controllers ---"
        for c in /sys/class/nvme/nvme*; do
            [ -e "$c/subsysnqn" ] || continue
            grep -qE 'fideli|fidadopt' "$c/subsysnqn" 2>/dev/null && basename "$c"
        done
        echo "--- fideli nvmet subsystems ---"
        find "$NVMET_CFS/subsystems" -maxdepth 1 -mindepth 1 2>/dev/null |
            grep -E 'fideli|fidadopt' || echo "(none)"
        echo "--- nvmet ports in the test slice 54000-54099 (+ small-int 1-9) ---"
        for p in "$NVMET_CFS"/ports/*; do
            [ -d "$p" ] || continue
            case "$(basename "$p")" in
            540[0-9][0-9]) echo "port $(basename "$p")" ;;
            [1-9]) echo "port $(basename "$p")" ;;
            esac
        done
        echo "--- rdma links (fideli) ---"
        rdma link show 2>/dev/null | grep fideli || echo "(none)"
        echo "--- fideli loop devices ---"
        losetup -a 2>/dev/null | grep squeezefs-fideli || echo "(none)"
        echo "--- listening tcp 4400-4899 ---"
        ss -ltn 2>/dev/null | awk '$4 ~ /:(4[4-8][0-9][0-9])$/ {print $4}' | sort
    } > "$out" 2>&1
    echo "$out"
}

# ---------------------------------------------------------------------------
# teardown
# ---------------------------------------------------------------------------
wipe_marked_subsystem() { # nqn — manual configfs sweep, OURS ONLY by marker
    local nqn=$1 p
    case "$nqn" in
    *fideli* | *fidadopt*) ;;
    *)
        warn "refusing manual wipe of non-fidelity subsystem $nqn"
        return 1
        ;;
    esac
    for p in "$NVMET_CFS"/ports/*/subsystems/"$nqn"; do
        [ -L "$p" ] && rm -f "$p" 2>/dev/null
    done
    if [ -d "$NVMET_CFS/subsystems/$nqn" ]; then
        echo 0 > "$NVMET_CFS/subsystems/$nqn/namespaces/1/enable" 2>/dev/null
        rmdir "$NVMET_CFS/subsystems/$nqn/namespaces/1" 2>/dev/null
        rmdir "$NVMET_CFS/subsystems/$nqn" 2>/dev/null
    fi
}

cmd_teardown() {
    substrate_env
    if [ ! -d "$STATE" ]; then
        log "nothing to tear down (no state dir $STATE)"
        return 0
    fi
    local rc=0 m p s lo idx nqn stack id prior

    # 0. Unmount fidelity mounts + kill OUR daemons (log-file under $STATE).
    for m in "$STATE"/mnt*; do
        [ -d "$m" ] || continue
        umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null
    done
    pkill -f "log-file $STATE" 2>/dev/null && sleep 2

    # 0b. Kill leftover crash-window stall proxies (ours: marker in argv).
    pkill -f "fideli-rpc-stall" 2>/dev/null

    # 1. Disconnect our controllers: manifest-recorded NQNs + marker sweep.
    if [ -f "$MANIFEST" ]; then
        grep '^connected=' "$MANIFEST" 2>/dev/null | cut -d= -f2- | sort -u | while read -r nqn; do
            nvme disconnect -n "$nqn" >/dev/null 2>&1
        done
    fi
    for c in /sys/class/nvme/nvme*; do
        [ -e "$c/subsysnqn" ] || continue
        if grep -qE 'fideli|fidadopt' "$c/subsysnqn" 2>/dev/null; then
            nvme disconnect -n "$(cat "$c/subsysnqn")" >/dev/null 2>&1
        fi
    done
    sleep 1

    # 2. Unshare every ledgered record through the PRODUCT (accepts
    #    pending/removing intents — §6.4 law 6); spdk records get a --force
    #    fallback (R7); nvmet records never take --force (flag refuses).
    if [ -f "$LEDGER_DIR/shares.json" ]; then
        jq -r '.shares[]? | "\(.subnqn)\t\(.stack)"' "$LEDGER_DIR/shares.json" 2>/dev/null |
            while IFS=$'\t' read -r nqn stack; do
                if ! "$BIN" nvmeof unshare "$nqn" >/dev/null 2>&1; then
                    if [ "$stack" = "spdk" ]; then
                        "$BIN" nvmeof unshare "$nqn" --force >/dev/null 2>&1 ||
                            warn "unshare $nqn failed (spdk, forced)"
                    else
                        warn "unshare $nqn failed (nvmet)"
                    fi
                fi
            done
    fi

    # 3. Hand-built leg fixtures: manifest-recorded subsystems/ports, then a
    #    marker sweep for orphans a crashed leg may not have recorded.
    if [ -f "$MANIFEST" ]; then
        grep '^handbuilt_subsystem=' "$MANIFEST" 2>/dev/null | cut -d= -f2- | while read -r nqn; do
            wipe_marked_subsystem "$nqn"
        done
        grep '^handbuilt_port=' "$MANIFEST" 2>/dev/null | cut -d= -f2 | while read -r id; do
            p="$NVMET_CFS/ports/$id"
            if [ -d "$p" ] && [ -z "$(find "$p/subsystems" -mindepth 1 2>/dev/null)" ]; then
                rmdir "$p" 2>/dev/null
            fi
        done
    fi
    for s in "$NVMET_CFS"/subsystems/*; do
        [ -d "$s" ] || continue
        case "$(basename "$s")" in
        *fideli* | *fidadopt*)
            warn "marker sweep: removing orphaned $(basename "$s")"
            wipe_marked_subsystem "$(basename "$s")"
            ;;
        esac
    done
    # Our reserved-slice port shells, if link-free (leg crash residue).
    for p in "$NVMET_CFS"/ports/540[0-9][0-9]; do
        [ -d "$p" ] || continue
        if [ -z "$(find "$p/subsystems" -mindepth 1 2>/dev/null)" ]; then
            rmdir "$p" 2>/dev/null && warn "swept link-free slice port $(basename "$p")"
        fi
    done

    # 4. Target stop: product verb first, recorded pids as fallback.
    "$BIN" nvmeof target stop --force >/dev/null 2>&1
    if [ -f "$RUN_DIR/spdk_tgt.pid" ]; then
        p=$(cat "$RUN_DIR/spdk_tgt.pid" 2>/dev/null)
        [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null
    fi
    if [ -f "$MANIFEST" ]; then
        grep '^spdk_pid=' "$MANIFEST" 2>/dev/null | cut -d= -f2 | while read -r p; do
            kill -9 "$p" 2>/dev/null
        done
    fi

    # 5. rdma rxe devices we added (modules stay loaded by policy).
    if [ -f "$MANIFEST" ]; then
        grep '^rdma_link=' "$MANIFEST" 2>/dev/null | cut -d= -f2 | while read -r m; do
            rdma link del "$m" 2>/dev/null && log "removed rdma link $m"
        done
    fi
    rdma link show 2>/dev/null | awk '/fideli/ {gsub(/\/[0-9]+$/, "", $2); print $2}' |
        while read -r m; do
            rdma link del "$m" 2>/dev/null && warn "marker sweep: removed rdma link $m"
        done

    # 6. Loop devices over fidelity backing files.
    losetup -a 2>/dev/null | grep squeezefs-fideli | cut -d: -f1 | while read -r lo; do
        losetup -d "$lo" 2>/dev/null && log "detached $lo"
    done

    # 7. zram backings (ours only; never index 0).
    if [ -f "$MANIFEST" ]; then
        grep '^zram=' "$MANIFEST" 2>/dev/null | sed 's/^zram=//' | awk '{print $1}' | while read -r idx; do
            if [ "$idx" = "0" ]; then
                warn "refusing zram0"
                continue
            fi
            [ -b "/dev/zram$idx" ] || continue
            echo 1 > "/sys/block/zram$idx/reset" 2>/dev/null
            echo "$idx" > /sys/class/zram-control/hot_remove 2>/dev/null
        done
        sed -i '/^zram=/d' "$MANIFEST" 2>/dev/null
    fi

    # 8. Hugepages: product restore first; raw fallback from the product's
    #    own record if the verb path is unavailable.
    if [ -f "$LEDGER_DIR/spdk/hugepages-prior" ]; then
        if ! "$BIN" nvmeof target setup --restore-prior >/dev/null 2>&1; then
            prior=$(cat "$LEDGER_DIR/spdk/hugepages-prior" 2>/dev/null)
            [ -n "${prior:-}" ] && echo "$prior" > "$HP_SYSFS/nr_hugepages" 2>/dev/null
            warn "hugepages force-restored to recorded prior ${prior:-?}"
        fi
    fi

    # 9. /opt prefix ONLY if this substrate's create installed it.
    if [ -f "$MANIFEST" ] && grep -q '^created_opt_prefix=1' "$MANIFEST" 2>/dev/null; then
        find /opt/squeezefs -depth -delete 2>/dev/null
        sed -i '/^created_opt_prefix=/d' "$MANIFEST" 2>/dev/null
        log "removed /opt/squeezefs (installed by this substrate)"
    fi

    # 10. Zero-residue proof: after-snapshot diffed against create's before-
    #     snapshot (§6.8 — the assert is built in, not optional).
    if [ -f "$STATE/snap-before.txt" ]; then
        cmd_snapshot after >/dev/null
        if diff -u "$STATE/snap-before.txt" "$STATE/snap-after.txt" > "$STATE/residue.diff" 2>&1; then
            log "ZERO residue: before/after snapshots identical"
        else
            warn "RESIDUE DETECTED (diff follows)"
            cat "$STATE/residue.diff" >&2
            rc=1
        fi
    else
        warn "no before-snapshot found — residue diff skipped (partial-state teardown)"
    fi

    log "teardown complete (state/artifacts kept at $STATE until the next create; modules left loaded by policy)"
    return "$rc"
}

# ---------------------------------------------------------------------------
# create
# ---------------------------------------------------------------------------
substrate_healthy() {
    [ -f "$ENV_FILE" ] && [ -f "$DEVICES_FILE" ] || return 1
    substrate_env
    "$BIN" nvmeof target status --json 2>/dev/null | jq -e '.rpc.live == true' >/dev/null || return 1
    local n
    n=$("$BIN" nvmeof list --json 2>/dev/null |
        jq '[.shares[] | select(.classification == "managed" and .live == true)] | length')
    [ "${n:-0}" -ge 4 ] || return 1
    # shellcheck disable=SC1090 # generated by cmd_create
    . "$DEVICES_FILE"
    [ -b "${DEV_GMETA_SPDK:-/nonexistent}" ] && [ -b "${DEV_GMETA_NVMET:-/nonexistent}" ]
}

resolve_spdk_bin() {
    # Preference order (§6.8): verified pin > sanctioned scoping build (env
    # seam, loud unpinned) > product install (railed; /opt removed at
    # teardown when we created it). Echoes the override path or "" —
    # stdout is the value; all logging goes to stderr.
    if [ -x "$PIN_PREFIX/build/bin/spdk_tgt" ]; then
        log "SPDK binary: pre-existing pinned install at $PIN_PREFIX (preferred; left in place at teardown)"
        echo ""
        return 0
    fi
    if [ -x "$SCOPING_TGT" ]; then
        log "SPDK binary: sanctioned scoping build via SQUEEZEFS_SPDK_TGT_BIN=$SCOPING_TGT (loud, unpinned — §6.5 rig posture)"
        echo "$SCOPING_TGT"
        return 0
    fi
    log "SPDK binary: none present — running the pinned 'nvmeof target install' (~35 s railed)"
    manifest "created_opt_prefix=1"
    taskset -c 0-15 "$BIN" nvmeof target install > "$STATE/install.txt" 2>&1 ||
        die "target install failed: $(tail -5 "$STATE/install.txt")"
    echo ""
}

cmd_create() {
    ensure_prereqs

    if substrate_healthy; then
        log "substrate already exists and is healthy — nothing to do"
        cmd_status
        return 0
    fi
    if [ -d "$STATE" ] && [ -f "$MANIFEST" ]; then
        warn "stale/partial substrate state — tearing it down before recreating"
        cmd_teardown || warn "stale-state teardown reported residue (rebuilding anyway)"
    fi

    # Fresh state EVERY run (the n3-gate run-1 lesson: leftover tgt-config /
    # ledger is stale rig state, not a product precondition).
    [ -d "$STATE" ] && find "$STATE" -mindepth 1 -delete
    mkdir -p "$STATE" "$LEDGER_DIR" "$RUN_DIR"
    : > "$MANIFEST"

    log "before-snapshot (the zero-residue reference)"
    cmd_snapshot before >/dev/null

    substrate_env
    local override
    override=$(resolve_spdk_bin)
    if [ -n "$override" ]; then
        export SQUEEZEFS_SPDK_TGT_BIN="$override"
    fi

    # rpc.py for READ-ONLY assertions (target mutations are product-verb-only).
    local rpcpy=""
    if [ -f /var/tmp/spdk-scoping/spdk/scripts/rpc.py ]; then
        rpcpy="/var/tmp/spdk-scoping/spdk/scripts/rpc.py"
    elif [ -f "$PIN_PREFIX/src/scripts/rpc.py" ]; then
        rpcpy="$PIN_PREFIX/src/scripts/rpc.py"
    fi

    {
        echo "export SQUEEZEFS_NVMEOF_STATE_DIR=\"$LEDGER_DIR\""
        echo "export SQUEEZEFS_NVMEOF_RUN_DIR=\"$RUN_DIR\""
        echo "export SQUEEZEFS_NVMET_PORT_ID_BASE=\"$PORT_ID_BASE\""
        echo "unset SQUEEZEFS_NVMEOF_TARGET_STACK"
        if [ -n "$override" ]; then
            echo "export SQUEEZEFS_SPDK_TGT_BIN=\"$override\""
        else
            echo "unset SQUEEZEFS_SPDK_TGT_BIN"
        fi
        echo "FIDELI_BIN=\"$BIN\""
        echo "FIDELI_MANIFEST=\"$MANIFEST\""
        echo "FIDELI_RPCPY=\"$rpcpy\""
        echo "FIDELI_NQN_PREFIX=\"$NQN_PREFIX\""
    } > "$ENV_FILE"

    # Hugepages + target, product-verb-driven.
    "$BIN" nvmeof target setup --hugemem-mb "$HUGEMEM_MB" > "$STATE/setup.txt" 2>&1 ||
        die "target setup failed: $(cat "$STATE/setup.txt")"
    log "hugepages reserved via product setup ($(grep -o 'recorded prior nr_hugepages = [0-9]*' "$STATE/setup.txt" || echo 'prior recorded'))"
    "$BIN" nvmeof target start > "$STATE/start.txt" 2>&1 ||
        die "target start failed: $(cat "$STATE/start.txt")"
    local tgt_pid
    tgt_pid=$(cat "$RUN_DIR/spdk_tgt.pid" 2>/dev/null || echo "")
    [ -n "$tgt_pid" ] && manifest "spdk_pid=$tgt_pid"
    log "spdk_tgt up (pid ${tgt_pid:-?}) via 'nvmeof target start'"

    # Guard backings + standing shares (2 per stack), product-verb-driven.
    local z_gm_s z_gd_s z_gm_n z_gd_n
    z_gm_s=$(mkzram $((2 * 1024 * 1024 * 1024)) guard-spdk-meta)
    z_gd_s=$(mkzram $((8 * 1024 * 1024 * 1024)) guard-spdk-data)
    z_gm_n=$(mkzram $((2 * 1024 * 1024 * 1024)) guard-nvmet-meta)
    z_gd_n=$(mkzram $((8 * 1024 * 1024 * 1024)) guard-nvmet-data)

    share_one "$NQN_GMETA_SPDK" "$z_gm_s" "$PORT_GMETA_SPDK" spdk
    share_one "$NQN_GDATA_SPDK" "$z_gd_s" "$PORT_GDATA_SPDK" spdk
    share_one "$NQN_GMETA_NVMET" "$z_gm_n" "$PORT_GUARD_NVMET" nvmet
    share_one "$NQN_GDATA_NVMET" "$z_gd_n" "$PORT_GUARD_NVMET" nvmet
    log "guard shares up (2 per stack, product verbs; nvmet ids in the 54000 slice)"

    local d_gm_s d_gd_s d_gm_n d_gd_n
    d_gm_s=$(connect_one "$NQN_GMETA_SPDK" "$PORT_GMETA_SPDK")
    d_gd_s=$(connect_one "$NQN_GDATA_SPDK" "$PORT_GDATA_SPDK")
    d_gm_n=$(connect_one "$NQN_GMETA_NVMET" "$PORT_GUARD_NVMET")
    d_gd_n=$(connect_one "$NQN_GDATA_NVMET" "$PORT_GUARD_NVMET")

    {
        echo "NQN_GMETA_SPDK=\"$NQN_GMETA_SPDK\""
        echo "NQN_GDATA_SPDK=\"$NQN_GDATA_SPDK\""
        echo "NQN_GMETA_NVMET=\"$NQN_GMETA_NVMET\""
        echo "NQN_GDATA_NVMET=\"$NQN_GDATA_NVMET\""
        echo "PORT_GMETA_SPDK=$PORT_GMETA_SPDK"
        echo "PORT_GDATA_SPDK=$PORT_GDATA_SPDK"
        echo "PORT_GUARD_NVMET=$PORT_GUARD_NVMET"
        echo "DEV_GMETA_SPDK=\"$d_gm_s\""
        echo "DEV_GDATA_SPDK=\"$d_gd_s\""
        echo "DEV_GMETA_NVMET=\"$d_gm_n\""
        echo "DEV_GDATA_NVMET=\"$d_gd_n\""
    } > "$DEVICES_FILE"

    log "substrate ready (state: $STATE)"
    cmd_status
}

cmd_status() {
    substrate_env
    if [ ! -f "$ENV_FILE" ]; then
        log "no substrate (state dir $STATE empty) — run: $0 create"
        return 0
    fi
    echo "--- target status (spdk) ---"
    "$BIN" nvmeof target status --json 2>/dev/null |
        jq -c '{running: .running.mode, pid: .running.pid, rpc_live: .rpc.live, drift: .rpc.drift, subsystems, hugepages: .hugepages.total_2m, ledger}' ||
        echo "(target status unavailable)"
    echo "--- ledgered shares ---"
    "$BIN" nvmeof list --json 2>/dev/null |
        jq -r '.shares[] | "\(.subnqn)\t\(.stack)\t\(.state)\tlive=\(.live)"' ||
        echo "(list unavailable)"
    if [ -f "$DEVICES_FILE" ]; then
        echo "--- guard initiator devices ---"
        cat "$DEVICES_FILE"
    fi
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
main() {
    local verb="${1:-}"
    case "$verb" in
    create | teardown | status | mkzram | snapshot)
        ensure_root "$@"
        case "$verb" in
        create) cmd_create ;;
        teardown) cmd_teardown ;;
        status) cmd_status ;;
        mkzram) mkzram "${2:?mkzram needs size-bytes}" "${3:?mkzram needs a label}" ;;
        snapshot) cmd_snapshot "${2:-unlabeled}" ;;
        esac
        ;;
    env) echo "$ENV_FILE" ;;
    -h | --help | help) usage ;;
    "")
        usage
        exit 2
        ;;
    *)
        usage
        die "unknown verb '$verb'"
        ;;
    esac
}

main "$@"
