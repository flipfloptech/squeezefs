#!/usr/bin/env bash
# tests/pv_volume_set.sh — the N-METADATA-VOLUME set fixture
# (design-per-volume-claim-admission PR 0, the viability gate)
# =============================================================================
#
# ONE daemon, ONE volume set, N metadata volumes. This is NOT the N-daemon
# fleet shape: tests/mw_fleet.sh builds N mounts over ONE set on the tcp
# devsub and needs root, a fabric and an instance teardown ladder. PR 0's
# question is about the per-VOLUME cost inside a single daemon (claim,
# checkpoint task, journal ring, node cache), which the recipe's spec-R4
# width (~46 metadata volumes) multiplies — so the fixture that answers it
# is one mount over a wide set, and the memory-plane half of it is
# honestly measurable UNPRIVILEGED on file-backed volumes (budget
# derivation and cache sizing are properties of the daemon, not of device
# barriers). mw_fleet.sh is reused where it fits (the rows the fleet shape
# owns); this fixture exists because that shape does not fit this row.
#
# SUBSTRATE-AGNOSTIC BY CONSTRUCTION. The device list is an input:
#
#   file (default)   sparse images under $STATE/meta — unprivileged, and
#                    the honest venue for the memory-plane rows only.
#   dev              SQZ_PVSET_META_DEVS=/dev/nvme1n1,/dev/nvme2n1,...
#                    (+ SQZ_PVSET_DATA_DEVS) — the tests/dev_substrate.sh
#                    nvmet re-run under root uses the SAME code path, so
#                    the barrier-bound rows (checkpoint CPU, journal
#                    balance) can be re-taken without a second harness.
#                    SQZ_PVSET_SUBSTRATE names the label carried on every
#                    row (default "file" / "dev" by device class).
#
# Verbs:
#   create N [--mem-budget=SIZE] [--node-cache-mb=MB] [--kernel-ttl-ms=MS]
#            [--meta-mb=MB] [--data-mb=MB] [--flush-ms=MS] [--tag=NAME]
#                    format + mount + wait ready; writes $STATE/config.env
#                    (N, MNT, PID, SUBSTRATE, every knob in force) and
#                    ASSERTS the set width the daemon actually opened
#                    (len(meta_kv_journal_entries_per_volume) == N) — a
#                    fixture that silently opened fewer volumes would make
#                    every row a lie.
#   remount          unmount + mount the SAME set with the SAME knobs
#                    (data intact): the cold-daemon-cache and mount-replay
#                    instrument. Re-asserts the width and updates PID.
#   teardown         product umount, bounded wait, fusermount3 -uz
#                    fallback, kill-9 backstop, image + state removal
#                    (never touches a caller-supplied DEVICE).
#   status           the config + live posture.
#   mnt | pid | meta-uri | state | remount-times
#
# --kernel-ttl-ms=0 pins the four per-class kernel TTLs
# (SQUEEZEFS_FUSE_{ATTR,ENTRY,DIR_ENTRY,NEGATIVE}_TTL_MS) to 0 so a
# metadata READ pass actually reaches the daemon — without it the kernel's
# 1 s attr/entry caches serve a re-stat sweep entirely and the daemon-side
# node-cache row measures nothing (the instrument-alignment law).
#
# Env: SQZ_PVSET_STATE_DIR (default ${TMPDIR:-/tmp}/squeezefs-pvset),
#      SQZ_PVSET_META_DEVS, SQZ_PVSET_DATA_DEVS, SQZ_PVSET_SUBSTRATE,
#      SQZ_PVSET_META_MB (default 512), SQZ_PVSET_DATA_MB (default 4096),
#      SQZ_BIN, SQZ_PVSET_SYMMETRIC (symmetric PR 13, gate 6; INVERTED at
#      the PR-14 flip): `1` (the DEFAULT) formats the plain default — the
#      symmetric forest, every mount an armed writer — and measures the
#      forest's per-slot extent floor, slot-tree bytes, ring and page cost
#      per volume width; `0` formats `--single-writer`, the flat solo
#      class that every pre-PR-13 row measured (the A arm of gate 6).
#
# Exit: 0 green, nonzero on any refusal. Root is NOT required for the
# file substrate (this box mounts FUSE unprivileged); a device substrate
# needs whatever access the devices need.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SQZ="${SQZ_BIN:-$REPO/target/release/squeezefs}"
STATE="${SQZ_PVSET_STATE_DIR:-${TMPDIR:-/tmp}/squeezefs-pvset}"
CONF="$STATE/config.env"
META_MB="${SQZ_PVSET_META_MB:-512}"
DATA_MB="${SQZ_PVSET_DATA_MB:-4096}"
SYMMETRIC="${SQZ_PVSET_SYMMETRIC:-1}"

log() { echo "[pvset] $*"; }
die() {
    echo "[pvset] ERROR: $*" >&2
    exit 1
}

# KD-7 dev override for the admin lane (dev-tree `-dirty` identities are
# degenerate): the product `umount` verb rides it.
export SQUEEZEFS_IPC_ALLOW_DEV=1

daemon_pid() { # -> the daemon pid serving $1 (empty when none)
    local mnt="$1" p
    for p in $(pgrep -x squeezefs 2>/dev/null || true); do
        if tr '\0' '\n' <"/proc/$p/cmdline" 2>/dev/null | grep -qx -- "$mnt"; then
            echo "$p"
            return 0
        fi
    done
    return 0
}

# Mount $2 (meta URI) at $3 with the knobs in $4..$6; asserts the opened
# width against $1 and echoes the daemon pid.
do_mount() { # n meta_uri mnt mem_budget node_cache_mb flush_ms ttl_ms
    local n="$1" meta_uri="$2" mnt="$3" mem_budget="$4" node_cache_mb="$5"
    local flush_ms="$6" ttl_ms="$7"
    local -a args=("$meta_uri" "$mnt" --daemon --log-file "$STATE/mount.log")
    [ -n "$mem_budget" ] && args+=(--mem-budget "$mem_budget")
    local -a menv=()
    # A default-format set mounts ARMED by default (PR 14); a
    # `--single-writer` set is the flat class whatever the knob says.
    [ -n "$node_cache_mb" ] && menv+=("SQUEEZEFS_META_NODE_CACHE_MB=$node_cache_mb")
    [ -n "$flush_ms" ] && menv+=("SQUEEZEFS_META_FLUSH_INTERVAL_MS=$flush_ms")
    if [ -n "$ttl_ms" ]; then
        menv+=("SQUEEZEFS_FUSE_ATTR_TTL_MS=$ttl_ms" "SQUEEZEFS_FUSE_ENTRY_TTL_MS=$ttl_ms"
            "SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS=$ttl_ms" "SQUEEZEFS_FUSE_NEGATIVE_TTL_MS=$ttl_ms")
    fi
    if [ "${#menv[@]}" -gt 0 ]; then
        env "${menv[@]}" "$SQZ" mount "${args[@]}" \
            >>"$STATE/mount.log" 2>&1 || die "mount failed (see $STATE/mount.log)"
    else
        "$SQZ" mount "${args[@]}" \
            >>"$STATE/mount.log" 2>&1 || die "mount failed (see $STATE/mount.log)"
    fi

    local _
    for _ in $(seq 1 600); do
        mountpoint -q "$mnt" && [ -r "$mnt/.stats" ] && break
        sleep 0.2
    done
    mountpoint -q "$mnt" || die "mount never appeared (see $STATE/mount.log)"

    # Engagement: the daemon must have opened exactly N volumes. The
    # per-volume journal array IS the width instrument (its length is the
    # opened set), so a silently narrower set can never carry a row.
    local opened
    opened="$(python3 -c '
import json, sys
m = json.load(open(sys.argv[1] + "/.stats"))["metrics"]
print(len(m["meta_kv_journal_entries_per_volume"]))' "$mnt")"
    [ "$opened" = "$n" ] ||
        die "daemon opened $opened metadata volume(s), expected $n"

    local pid
    pid="$(daemon_pid "$mnt")"
    [ -n "$pid" ] || die "cannot resolve the daemon pid for $mnt"
    echo "$pid"
}

do_umount() { # mnt pid
    local mnt="$1" pid="$2" _
    "$SQZ" umount "$mnt" >>"$STATE/umount.log" 2>&1 || true
    for _ in $(seq 1 600); do
        mountpoint -q "$mnt" || break
        sleep 0.2
    done
    mountpoint -q "$mnt" && fusermount3 -uz "$mnt" >/dev/null 2>&1
    for _ in $(seq 1 600); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.2
    done
    if kill -0 "$pid" 2>/dev/null; then
        log "daemon $pid survived the unmount ladder — SIGKILL backstop"
        kill -9 "$pid" 2>/dev/null || true
        sleep 1
    fi
}

create() {
    local n="$1"
    shift
    local mem_budget="" node_cache_mb="" flush_ms="" ttl_ms="" tag="pvset"
    for a in "$@"; do
        case "$a" in
        --mem-budget=*) mem_budget="${a#--mem-budget=}" ;;
        --node-cache-mb=*) node_cache_mb="${a#--node-cache-mb=}" ;;
        --kernel-ttl-ms=*) ttl_ms="${a#--kernel-ttl-ms=}" ;;
        --meta-mb=*) META_MB="${a#--meta-mb=}" ;;
        --data-mb=*) DATA_MB="${a#--data-mb=}" ;;
        --flush-ms=*) flush_ms="${a#--flush-ms=}" ;;
        --tag=*) tag="${a#--tag=}" ;;
        *) die "create: unknown argument '$a'" ;;
        esac
    done
    [[ "$n" =~ ^[0-9]+$ ]] && [ "$n" -ge 1 ] && [ "$n" -le 256 ] ||
        die "create takes N in 1..256 (got '$n')"
    [ -x "$SQZ" ] || die "missing $SQZ (cargo build --release)"
    [ -e "$CONF" ] && die "a fixture is already live at $STATE — teardown first"

    local substrate meta_paths=() data_paths=()
    if [ -n "${SQZ_PVSET_META_DEVS:-}" ]; then
        substrate="${SQZ_PVSET_SUBSTRATE:-dev}"
        local devs=()
        IFS=, read -r -a devs <<<"$SQZ_PVSET_META_DEVS"
        [ "${#devs[@]}" -ge "$n" ] ||
            die "SQZ_PVSET_META_DEVS names ${#devs[@]} device(s), need $n"
        local i
        for ((i = 0; i < n; i++)); do
            [ -b "${devs[$i]}" ] || [ -c "${devs[$i]}" ] ||
                die "SQZ_PVSET_META_DEVS[$i]='${devs[$i]}' is not a device node"
            meta_paths+=("${devs[$i]}")
        done
        [ -n "${SQZ_PVSET_DATA_DEVS:-}" ] ||
            die "SQZ_PVSET_META_DEVS given without SQZ_PVSET_DATA_DEVS (a device substrate never mixes in a file data volume)"
        IFS=, read -r -a data_paths <<<"$SQZ_PVSET_DATA_DEVS"
    else
        substrate="${SQZ_PVSET_SUBSTRATE:-file}"
    fi

    mkdir -p "$STATE/meta" "$STATE/stage" "$STATE/mnt"
    local mnt="$STATE/mnt"

    if [ "${#meta_paths[@]}" -eq 0 ]; then
        local i
        for ((i = 0; i < n; i++)); do
            rm -f "$STATE/meta/vol$i.img"
            truncate -s "${META_MB}M" "$STATE/meta/vol$i.img"
            meta_paths+=("$STATE/meta/vol$i.img")
        done
        rm -f "$STATE/data.img"
        truncate -s "${DATA_MB}M" "$STATE/data.img"
        data_paths=("$STATE/data.img")
    fi

    local meta_uri data_uri
    meta_uri="sqmeta://$(
        IFS=,
        echo "${meta_paths[*]}"
    )"
    data_uri="sqdata://$(
        IFS=,
        echo "${data_paths[*]}"
    )"

    rm -rf "$STATE/stage" && mkdir -p "$STATE/stage"
    local -a fmt_args=()
    [ "$SYMMETRIC" = "1" ] || fmt_args+=(--single-writer)
    log "format: $n metadata volume(s), substrate=$substrate, tag=$tag layout=$([ "$SYMMETRIC" = "1" ] && echo symmetric || echo single-writer)"
    "$SQZ" format "$meta_uri" "$data_uri" --disk-cache-paths "$STATE/stage" --force "${fmt_args[@]}" \
        >"$STATE/format.log" 2>&1 || die "format failed (see $STATE/format.log)"

    local pid
    pid="$(do_mount "$n" "$meta_uri" "$mnt" "$mem_budget" "$node_cache_mb" "$flush_ms" "$ttl_ms")"

    {
        echo "PVSET_N=$n"
        echo "PVSET_TAG=$tag"
        echo "PVSET_MNT=$mnt"
        echo "PVSET_PID=$pid"
        echo "PVSET_SUBSTRATE=$substrate"
        echo "PVSET_META_URI=$meta_uri"
        echo "PVSET_MEM_BUDGET=$mem_budget"
        echo "PVSET_NODE_CACHE_MB=$node_cache_mb"
        echo "PVSET_FLUSH_MS=$flush_ms"
        echo "PVSET_TTL_MS=$ttl_ms"
        echo "PVSET_META_MB=$META_MB"
        echo "PVSET_STATE=$STATE"
        echo "PVSET_SYMMETRIC=$SYMMETRIC"
    } >"$CONF"
    log "ready: N=$n pid=$pid mnt=$mnt substrate=$substrate"
}

remount() {
    [ -f "$CONF" ] || die "no fixture at $STATE"
    # shellcheck disable=SC1090 # generated by create
    . "$CONF"
    # The class the set was formatted for (recorded at create).
    SYMMETRIC="${PVSET_SYMMETRIC:-1}"
    local t0 t1 t2
    t0="$(date +%s.%N)"
    do_umount "$PVSET_MNT" "$PVSET_PID"
    t1="$(date +%s.%N)"
    local pid
    pid="$(do_mount "$PVSET_N" "$PVSET_META_URI" "$PVSET_MNT" "$PVSET_MEM_BUDGET" \
        "$PVSET_NODE_CACHE_MB" "$PVSET_FLUSH_MS" "$PVSET_TTL_MS")"
    t2="$(date +%s.%N)"
    sed -i "s/^PVSET_PID=.*/PVSET_PID=$pid/" "$CONF"
    # Split halves: the unmount ladder's drain is a fixed cost, the MOUNT
    # half is the per-volume open/replay the width row cares about.
    {
        echo "PVSET_LAST_UMOUNT_S=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.3f", b - a}')"
        echo "PVSET_LAST_MOUNT_S=$(awk -v a="$t1" -v b="$t2" 'BEGIN{printf "%.3f", b - a}')"
    } >"$STATE/last_remount.env"
    log "remounted: N=$PVSET_N pid=$pid"
}

teardown() {
    [ -f "$CONF" ] || {
        log "no fixture at $STATE (nothing to tear down)"
        rm -rf "$STATE"
        return 0
    }
    # shellcheck disable=SC1090 # generated by create
    . "$CONF"
    do_umount "$PVSET_MNT" "$PVSET_PID"
    # Device substrates are the caller's; only our own images are removed.
    rm -rf "$STATE"
    log "torn down (state removed)"
}

status() {
    [ -f "$CONF" ] || die "no fixture at $STATE"
    cat "$CONF"
    # shellcheck disable=SC1090 # generated by create
    . "$CONF"
    if mountpoint -q "$PVSET_MNT"; then
        echo "LIVE: mounted, daemon $(daemon_pid "$PVSET_MNT")"
    else
        echo "DEAD: $PVSET_MNT is not a mountpoint"
    fi
}

read_conf_field() {
    [ -f "$CONF" ] || die "no fixture at $STATE"
    # shellcheck disable=SC1090 # generated by create
    . "$CONF"
    case "$1" in
    mnt) echo "$PVSET_MNT" ;;
    pid) echo "$PVSET_PID" ;;
    meta-uri) echo "$PVSET_META_URI" ;;
    state) echo "$STATE" ;;
    remount-times) cat "$STATE/last_remount.env" 2>/dev/null || true ;;
    esac
}

VERB="${1:-}"
shift || true
case "$VERB" in
create) create "$@" ;;
remount) remount ;;
teardown) teardown ;;
status) status ;;
mnt | pid | meta-uri | state | remount-times) read_conf_field "$VERB" ;;
*)
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
    exit 1
    ;;
esac
