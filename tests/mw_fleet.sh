#!/usr/bin/env bash
# tests/mw_fleet.sh — the N-daemon single-node fleet harness (PR 6, rung 6)
# =============================================================================
#
# design-full-multi-writer §5.5: one rig, product verbs only (the
# nvmeof_target_substrate.sh discipline), built on the instance-suffixed
# **tcp devsub** (SQZ_DEVSUB_TRANSPORT=tcp — the fabric-sensitive venue,
# service-port slice 54100–54199, never the fidelity tier's 54000–54099).
#
# What `create N` builds (this rung's shape — see POSTURE below):
#   * an isolated tcp devsub instance (SQZ_DEVSUB_INSTANCE, disjoint from the
#     default devsub and from other agents' instances),
#   * ONE volume set formatted `--multi-writer` (rung 5, KD-MW-1 Phase A),
#   * durable `fabric_endpoint:` records written via the PRODUCT verb
#     `config set-fabric-endpoints` (rung 2, KD-MW-15),
#   * member 0 = the WRITER, mounted with an EXPLICIT per-mount
#     hostnqn/hostid pair (rung 2, KD-MW-3): its data-plane controllers are
#     DAEMON-OWNED connects resolved from the records — the rig never
#     pre-connects a data device for it (the product resolution path is
#     exercised, not masked). Engagement is ASSERTED two ways: the daemon
#     log's "daemon-owned controller resolved" line (the KD-MW-3 ENGAGED
#     stdout line stays in the daemonized child), and sysfs — every data
#     NQN must carry a controller under the writer's identity post-mount,
#   * members 1..N-1 = READ-ONLY coherent readers (`--read-only`, the DLM S5
#     mount half): no lease, no claim, no PR registrant — and therefore NO
#     explicit identity (see POSTURE),
#   * per-daemon `SQUEEZEFS_FLEET_SHARE=N` (rung 3b/3c, KD-MW-14),
#   * per-mount log + stats capture (stats snapshots are `cat $MNT/.stats`
#     — never cp: the aging trap),
#   * a recorded host-scoped-subsystem capability verdict (rung 5b probe —
#     see THE 5b GATE below).
#
# POSTURE (this rung, adjudicated 2026-08-15 — rung 5b):
#   Co-located multi-IDENTITY mounts (≥2 explicit hostnqn pairs on one
#   shared volume set) are IMPOSSIBLE on nvme_core.multipath=Y kernels: the
#   kernel groups fabric controllers by subsysnqn IGNORING hostnqn, per-path
#   namespaces are hidden gendisks, and the only openable node round-robins
#   both identities — the shape rung 2's rule 2 correctly refuses (proven
#   live at ed2223c9). The sqz-kernel fix (host-scoped fabric subsystems)
#   is rung 5b, validated inside the rung-6b qemu guest. UNTIL THEN this
#   rig mounts exactly ONE explicit-identity member (the writer — the
#   proven N=1 shape) plus identity-less readers, and every multi-identity
#   verb/leg is gated on the recorded 5b capability probe (skip-loud).
#   Readers hold no PR registrant by contract, so they need no identity and
#   the ladder admits them on the shared head unchanged.
#
#   S5 visibility note: `squeezefs clients` does NOT list read-only mounts
#   (a reader writes nothing, including its own registration; readers become
#   visible when S6's membership plane arms — rungs 7+). Reader identity /
#   posture is therefore read from the reader's OWN stats inode
#   (`mount_posture`, `client_slot`, `read_only_mount`).
#
# THE 5b GATE (--require-host-scoped-subsys / the recorded probe):
#   `create` probes whether this kernel scopes fabric subsystems by host
#   identity: (a) a module-param face, if the 5b kernel exposes one
#   (/sys/module/nvme_core/parameters/*host*scope*), else (b) EMPIRICAL —
#   with the writer identity's meta controller live (pre-mount, no I/O in
#   flight), connect a throwaway probe identity to the same subsystem NQN
#   and count /sys/class/nvme-subsystem entries carrying that NQN: 1 =
#   merged (stock multipath=Y), ≥2 = host-scoped (the 5b kernel). Verdict
#   recorded in $STATE/host_scoped; `--require-host-scoped-subsys` on
#   create refuses loud when absent. The probe runs ONLY before the writer
#   mounts — adding a path to a subsystem a live writer rides would
#   round-robin its I/O onto an unregistered association (PR-rejected).
#
# Verbs
#   create [N|N=<n>] [--cowriters K] [--require-host-scoped-subsys]
#                 build substrate + format + records + mount the fleet
#                 (refuses if state exists — run teardown first)
#   status        member table + capability verdict + identity map
#   mount <idx>   (re)mount one member    unmount <idx>   product umount
#   kill <idx> [--sig 9]   kill a member daemon (the S7-b matrices' verb)
#   probe-host-scoped      re-print the recorded 5b capability verdict
#   teardown      unmount + kill + disconnect + substrate teardown +
#                 ZERO-RESIDUE assertions (exits nonzero on any residue)
#   pause | partition | netem | --netns | --vm    STUBS — refused loud:
#                 pause/--vm land with rung 6b (qemu members); partition/
#                 netem/--netns land with rung 7 (the netem venue)
#
# RUNG-6 FINDINGS (2026-08-15) — FIXED, kept as this rig's history + the
# live regression tripwires it still asserts:
#   #1 (reader dirty-tail pin): a reader bootstrapping into a non-empty
#      writer journal tail replayed it as DIRTY RAM records nothing on a
#      read-only mount could ever flush — the S5 drop pass refused those
#      nodes forever (`meta_kv_revalidate_dirty_skips` climbed) and their
#      view froze at mount-time state. Fixed: the reader DECLARATION
#      absolves the replay residue (arm_reader_revalidation). Cargo pin:
#      readonly_mount_tests::reader_bootstrap_into_a_dirty_journal_tail_never_pins_nodes.
#      mount_reader_verified below remains as the LIVE tripwire (a nonzero
#      dirty_skips is a regression — die loud, no remount workaround).
#   #2 (multi-meta-volume live-reader non-convergence): one shared
#      RevalidationPoller driven per-volume let volume 0's cadence mark
#      suppress every sibling each pass — only meta volume 0 ever
#      revalidated (readdir-sees/lookup-misses, `d?????????`). Fixed:
#      poll_set_at makes ONE cadence decision per pass and polls EVERY
#      volume. Cargo pin:
#      readonly_mount_tests::a_live_reader_set_revalidates_every_meta_volume.
#      The rig defaults back to a MULTI-meta-volume set (MDS_COUNT=2) so
#      the smoke leg keeps exercising the fixed shape live.
#
# Env knobs
#   SQZ_BIN                     squeezefs binary (default target/release,
#                               falls back to target/debug — the
#                               mw_two_registrants_leg.sh discipline)
#   SQZ_MWFLEET_N=2             fleet width (create arg wins)
#   SQZ_MWFLEET_MDS_COUNT=2     metadata volumes (multi-volume by default —
#                               the fixed FINDING-#2 shape stays exercised
#                               live; 1 remains valid for narrow bisects)
#   SQZ_MWFLEET_OSS_COUNT=2     data volumes
#   SQZ_MWFLEET_OSS_GB=4        zram disksize GiB per data volume
#   SQZ_MWFLEET_INSTANCE=mwfleet   devsub instance suffix ([a-z0-9]{1,8})
#   SQZ_MWFLEET_STATE_DIR=/run/squeezefs-mwfleet
#   SQZ_MWFLEET_MNT_ROOT=/mnt/sqz-mwfleet
#
# Requires: root (re-execs via sudo), nvme-cli, kernel nvmet-tcp, python3.
# Refusals are LOUD with the reason (the dev_substrate/guard_smoke pattern)
# — never silent green.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SQZ="${SQZ_BIN:-$REPO/target/release/squeezefs}"
[ -x "$SQZ" ] || SQZ="$REPO/target/debug/squeezefs"

N_DEFAULT="${SQZ_MWFLEET_N:-2}"
# Default 2: multi-meta-volume is the field shape (production sets run 4)
# and the FIXED rung-6 finding #2's live regression venue — see the
# FINDINGS note in the header.
MDS_COUNT="${SQZ_MWFLEET_MDS_COUNT:-2}"
OSS_COUNT="${SQZ_MWFLEET_OSS_COUNT:-2}"
OSS_GB="${SQZ_MWFLEET_OSS_GB:-4}"
INSTANCE="${SQZ_MWFLEET_INSTANCE:-mwfleet}"
STATE="${SQZ_MWFLEET_STATE_DIR:-/run/squeezefs-mwfleet}"
MNT_ROOT="${SQZ_MWFLEET_MNT_ROOT:-/mnt/sqz-mwfleet}"

# The tcp devsub instance's identifier family (tests/dev_substrate.sh):
DEVSUB_STATE="/run/squeezefs-devsub-tcp-${INSTANCE}"
NQN_PREFIX="nqn.2026-07.io.squeezefs:devsubtcp${INSTANCE}-"
NVMET_CFS="/sys/kernel/config/nvmet"

MEMBERS="$STATE/members.tsv" # idx role mountpoint logfile hostnqn hostid pid
CONF="$STATE/config.env"

log() { echo "[mwfleet] $*"; }
warn() { echo "[mwfleet] WARN: $*" >&2; }
die() {
    echo "[mwfleet] ERROR: $*" >&2
    exit 1
}

# Product-verb wrapper: scrub the rig's own SQZ_* control variables from
# every squeezefs invocation — they are not registered knobs and the ENG-10
# registry announces them as probable typos on every verb otherwise.
SCRUB_ENV=("-u" "SQZ_BIN")
while IFS= read -r __kv; do SCRUB_ENV+=("-u" "${__kv%%=*}"); done \
    < <(env | grep -E '^(SQZ_MWFLEET_|SQZ_DEVSUB_)' || true)
unset __kv
sqz() { env "${SCRUB_ENV[@]}" "$SQZ" "$@"; }

ensure_root() {
    [ "$(id -u)" -eq 0 ] && return 0
    log "root required (nvmet configfs, mounts, fabrics connects) — re-executing via sudo"
    local knobs=()
    while IFS= read -r kv; do knobs+=("$kv"); done \
        < <(env | grep -E '^(SQZ_MWFLEET_|SQZ_DEVSUB_|SQZ_BIN=)' || true)
    exec sudo env "${knobs[@]}" bash "$0" "$@"
}

ensure_prereqs() {
    command -v nvme >/dev/null 2>&1 || die "nvme-cli is required"
    command -v python3 >/dev/null 2>&1 || die "python3 is required (row/state plumbing)"
    [ -x "$SQZ" ] || die "squeezefs binary not found at '$SQZ' (cargo build --release, or set SQZ_BIN)"
}

# --- sysfs walks (the initiator.rs shapes, shell face) ----------------------
# Head block device (strict nvme<X>n<Y>) serving <nqn>, multipath or not.
head_for_nqn() { # nqn -> echoes /dev path; rc 1 if absent
    local nqn="$1" d n base
    for d in /sys/class/nvme-subsystem/nvme-subsys* /sys/class/nvme/nvme*; do
        [ -r "$d/subsysnqn" ] || continue
        [ "$(cat "$d/subsysnqn" 2>/dev/null)" = "$nqn" ] || continue
        for n in "$d"/nvme*; do
            base="$(basename "$n")"
            [[ "$base" =~ ^nvme[0-9]+n[0-9]+$ ]] || continue
            [ -b "/dev/$base" ] || continue
            echo "/dev/$base"
            return 0
        done
    done
    return 1
}

# All controller dirs serving <nqn> (space-joined names), rc 1 if none.
ctrls_for_nqn() {
    local nqn="$1" c out=""
    for c in /sys/class/nvme/nvme*; do
        [ -r "$c/subsysnqn" ] || continue
        [ "$(cat "$c/subsysnqn" 2>/dev/null)" = "$nqn" ] && out="$out$(basename "$c") "
    done
    [ -n "$out" ] || return 1
    echo "${out% }"
}

# hostnqn attr of controller <name>.
ctrl_hostnqn() { cat "/sys/class/nvme/$1/hostnqn" 2>/dev/null || true; }

# One flattened stats-inode field (the JSON nests under "metrics").
stat_field() { # mountpoint key -> value
    cat "$1/.stats" | python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(sys.stdin)
print(flat(root.get("metrics", root), {}).get(sys.argv[1], ""))' "$2"
}

# Count of nvme-subsystem entries carrying <nqn> (the 5b probe's instrument).
subsys_count_for_nqn() {
    local nqn="$1" s n=0
    for s in /sys/class/nvme-subsystem/nvme-subsys*; do
        [ -r "$s/subsysnqn" ] || continue
        [ "$(cat "$s/subsysnqn" 2>/dev/null)" = "$nqn" ] && n=$((n + 1))
    done
    echo "$n"
}

wait_for() { # description tries cmd...
    local what="$1" tries="$2" i
    shift 2
    for ((i = 0; i < tries; i++)); do
        "$@" >/dev/null 2>&1 && return 0
        sleep 0.25
    done
    die "timed out waiting for $what"
}

# Deterministic per-member identity (KD-MW-3 shape: full pair, UUID-formed).
member_hostid() { printf 'cafef1e7-%04d-4000-8000-%012d' "$1" "$2"; }
member_hostnqn() { echo "nqn.2014-08.org.nvmexpress:uuid:$(member_hostid "$1" "$2")"; }

mnt_of() { echo "$MNT_ROOT/m$1"; }

# /proc/mounts-based liveness: `mountpoint -q` errors (ENOTCONN) on a mount
# whose FUSE daemon died or lost its devices — exactly the residue teardown
# must still sweep.
is_mounted() { awk -v m="$1" '$2==m {f=1} END {exit !f}' /proc/mounts; }

daemon_pid_for_mnt() { # mountpoint -> pid or empty
    pgrep -f "squeezefs.*mount.*$1" | head -1 || true
}

require_state() {
    [ -f "$CONF" ] || die "no fleet state at $STATE — run: sudo tests/mw_fleet.sh create N=2"
    # shellcheck disable=SC1090 # generated by create below
    . "$CONF"
    : "${FLEET_N:?}" "${META_PATHS:?}" "${W_HOSTNQN:?}" "${W_HOSTID:?}" "${CREATE_PID:?}"
}

# --- the 5b capability probe -------------------------------------------------
# Runs ONLY while no member daemon is mounted (see THE 5b GATE header note).
probe_host_scoped() { # meta_nqn create_pid -> echoes 0|1
    local nqn="$1" pid="$2" param p_hostnqn p_hostid before after ctrl
    # (a) the module-param face, if the 5b kernel ships one.
    for param in /sys/module/nvme_core/parameters/*host*scope* \
        /sys/module/nvme_core/parameters/*scope*host*; do
        if [ -r "$param" ]; then
            case "$(cat "$param")" in
            Y | y | 1)
                echo 1
                return 0
                ;;
            esac
        fi
    done
    # (b) empirical: a throwaway probe identity against the writer's meta
    # subsystem — merged (1 subsystem) vs host-scoped (2).
    p_hostnqn="$(member_hostnqn 90 "$pid")"
    p_hostid="$(member_hostid 90 "$pid")"
    before="$(subsys_count_for_nqn "$nqn")"
    if ! sqz nvmeof connect --ip "$TCP_ADDR" --port "$TCP_SVC" --subnqn "$nqn" \
        --hostnqn "$p_hostnqn" --hostid "$p_hostid" >/dev/null 2>&1; then
        warn "5b probe connect failed — recording not-host-scoped (the conservative verdict)"
        echo 0
        return 0
    fi
    sleep 1
    after="$(subsys_count_for_nqn "$nqn")"
    # Disconnect exactly the probe identity's controller(s).
    for ctrl in $(ctrls_for_nqn "$nqn" || true); do
        [ "$(ctrl_hostnqn "$ctrl")" = "$p_hostnqn" ] &&
            nvme disconnect -d "$ctrl" >/dev/null 2>&1
    done
    sleep 0.5
    if [ "$after" -gt "$before" ]; then echo 1; else echo 0; fi
}

# --- verbs -------------------------------------------------------------------
mount_member() { # idx
    require_state
    local idx="$1" mnt log role
    mnt="$(mnt_of "$idx")"
    log="$STATE/m${idx}.log"
    mkdir -p "$mnt"
    mountpoint -q "$mnt" && die "member $idx already mounted at $mnt"
    # The daemon environment: the fleet-share divisor (rung 3b) plus a
    # scrub of the rig's own SQZ_* control variables — they are not
    # SqueezeFS knobs and would trip the registry's typo announcement.
    local env_args=("${SCRUB_ENV[@]}")
    env_args+=("SQUEEZEFS_FLEET_SHARE=$FLEET_N") # options before assignments
    if [ "$idx" -eq 0 ]; then
        role="writer"
        # The proven N=1 explicit-identity shape: data plane daemon-owned
        # from the fabric_endpoint records. (The KD-MW-3 ENGAGED stdout
        # line stays inside the daemonized child; engagement is asserted
        # below via the daemon log + sysfs.)
        env "${env_args[@]}" "$SQZ" mount "sqmeta://$META_PATHS" "$mnt" \
            -o "hostnqn=$W_HOSTNQN,hostid=$W_HOSTID" \
            --daemon --allow-other --log-file "$log" \
            >"$STATE/m0.mount.out" 2>&1 ||
            die "writer mount failed: $(cat "$STATE/m0.mount.out")"
    else
        role="reader"
        # DLM S5 reader: no identity, no claim, no registrant (POSTURE).
        env "${env_args[@]}" "$SQZ" mount "sqmeta://$META_PATHS" "$mnt" \
            --read-only --daemon --allow-other --log-file "$log" \
            >"$STATE/m${idx}.mount.out" 2>&1 ||
            die "reader $idx mount failed: $(cat "$STATE/m${idx}.mount.out")"
    fi
    wait_for "member $idx mountpoint" 40 mountpoint -q "$mnt"
    wait_for "member $idx stats inode" 40 test -s "$mnt/.stats"
    if [ "$idx" -eq 0 ]; then
        # Rung-2 engagement, half 1: the daemon's own log names each data
        # volume's daemon-owned controller (half 2 — sysfs — runs in create).
        grep -q "daemon-owned controller resolved" "$log" ||
            die "writer log carries no 'daemon-owned controller resolved' line — the rung-2 connect path did not engage (log: $log)"
    fi
    local pid
    pid="$(daemon_pid_for_mnt "$mnt")"
    [ -n "$pid" ] || die "cannot find member $idx's daemon pid"
    # Update the members ledger (idx role mnt log hostnqn hostid pid).
    if [ "$idx" -eq 0 ]; then
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$idx" "$role" "$mnt" "$log" "$W_HOSTNQN" "$W_HOSTID" "$pid"
    else
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$idx" "$role" "$mnt" "$log" "-" "-" "$pid"
    fi >>"$MEMBERS.new"
    grep -v "^$idx	" "$MEMBERS" >>"$MEMBERS.new" 2>/dev/null || true
    sort -n "$MEMBERS.new" >"$MEMBERS" && rm -f "$MEMBERS.new"
    log "member $idx ($role) up at $mnt (pid $pid)"
}

# RUNG-6 FINDING #1's LIVE regression tripwire (the finding is FIXED — see
# the FINDINGS note in the header; cargo pin
# readonly_mount_tests::reader_bootstrap_into_a_dirty_journal_tail_never_pins_nodes):
# a reader may now bootstrap into ANY journal-tail state — the reader
# declaration absolves the replayed dirty residue — so a nonzero
# `meta_kv_revalidate_dirty_skips` after an epoch advance is a REGRESSION,
# never a re-roll. One mount, one nudge, one verdict; die loud.
mount_reader_verified() { # idx
    local idx="$1" probe w_mnt r_mnt skips
    w_mnt="$(mnt_of 0)"
    r_mnt="$(mnt_of "$idx")"
    mount_member "$idx"
    # Nudge the writer so the reader's revalidation epoch ADVANCES — the
    # dirty-skip tripwire only fires on an epoch advance over a pinned
    # node, so a quiet writer would hide a regression.
    probe="$w_mnt/.mwfleet-bootstrap-probe"
    date >"$probe" && sync
    sleep 2.5 # >= 2 reader revalidation polls (1 s cadence)
    skips="$(stat_field "$r_mnt" meta_kv_revalidate_dirty_skips)"
    rm -f "$probe"
    [ "$skips" = "0" ] ||
        die "reader $idx: meta_kv_revalidate_dirty_skips=$skips — the FIXED rung-6 pinned-node finding regressed (must stay 0 on every posture; see the FINDINGS note in the header)"
}

unmount_member() { # idx
    require_state
    local idx="$1" mnt
    mnt="$(mnt_of "$idx")"
    if mountpoint -q "$mnt"; then
        sqz umount "$mnt" >/dev/null 2>&1 || umount -l "$mnt" 2>/dev/null || true
    fi
    wait_for "member $idx unmount" 60 bash -c "! mountpoint -q '$mnt'"
    log "member $idx unmounted"
}

create_fleet() {
    local n="$N_DEFAULT" cowriters=0 require_hs=0 a
    for a in "$@"; do
        case "$a" in
        N=*) n="${a#N=}" ;;
        --cowriters)
            die "--cowriters takes a value (--cowriters K)"
            ;;
        --cowriters=*) cowriters="${a#--cowriters=}" ;;
        --require-host-scoped-subsys) require_hs=1 ;;
        --netns | --netem | --netem=* | --vm | --vm=*)
            die "'$a' is a later rung's surface (netns/netem: rung 7 — the netem venue; --vm: rung 6b qemu members). This rung stubs it loud, never silently ignores it"
            ;;
        [0-9]*) n="$a" ;;
        *) die "unknown create argument '$a'" ;;
        esac
    done
    [[ "$n" =~ ^[0-9]+$ ]] && [ "$n" -ge 1 ] || die "N must be a positive integer (got '$n')"
    [ -e "$CONF" ] && die "fleet state exists at $STATE — run 'sudo tests/mw_fleet.sh teardown' first"
    ensure_prereqs

    log "building tcp devsub instance '$INSTANCE' ($MDS_COUNT mds + $OSS_COUNT oss)"
    SQZ_DEVSUB_TRANSPORT=tcp SQZ_DEVSUB_INSTANCE="$INSTANCE" \
        SQZ_DEVSUB_MDS_COUNT="$MDS_COUNT" SQZ_DEVSUB_OSS_COUNT="$OSS_COUNT" \
        SQZ_DEVSUB_OSS_GB="$OSS_GB" \
        "$REPO/tests/dev_substrate.sh" create >/dev/null ||
        die "dev_substrate create failed"

    mkdir -p "$STATE" "$MNT_ROOT"
    : >"$MEMBERS"

    # Ordered NQN lists from the devsub manifest (role idx nqn ...).
    local meta_nqns=() data_nqns=() nqn role
    while IFS=$'\t' read -r role _ nqn _; do
        case "$role" in
        mds) meta_nqns+=("$nqn") ;;
        oss) data_nqns+=("$nqn") ;;
        esac
    done <"$DEVSUB_STATE/manifest.tsv"
    [ "${#meta_nqns[@]}" -eq "$MDS_COUNT" ] || die "manifest mds count mismatch"
    [ "${#data_nqns[@]}" -eq "$OSS_COUNT" ] || die "manifest oss count mismatch"

    # The nvmet port carrying our NQNs — its traddr/trsvcid feed the
    # fabric_endpoint records (read from configfs, never re-derived).
    local TCP_ADDR="" TCP_SVC="" p l
    for p in "$NVMET_CFS"/ports/*; do
        [ -d "$p" ] || continue
        for l in "$p"/subsystems/*; do
            [ -L "$l" ] || continue
            case "$(basename "$l")" in
            "$NQN_PREFIX"*)
                TCP_ADDR="$(cat "$p/addr_traddr")"
                TCP_SVC="$(cat "$p/addr_trsvcid")"
                break 2
                ;;
            esac
        done
    done
    [ -n "$TCP_SVC" ] || die "cannot locate the devsub instance's nvmet tcp port"
    log "target: $TCP_ADDR:$TCP_SVC (instance NQN prefix $NQN_PREFIX)"

    # Resolve the devsub-connected head paths (format-time paths).
    local meta_paths=() data_paths=() dev
    for nqn in "${meta_nqns[@]}"; do
        dev="$(head_for_nqn "$nqn")" || die "no head device for $nqn"
        meta_paths+=("$dev")
    done
    for nqn in "${data_nqns[@]}"; do
        dev="$(head_for_nqn "$nqn")" || die "no head device for $nqn"
        data_paths+=("$dev")
    done

    local meta_uri data_uri
    meta_uri="$(
        IFS=,
        echo "${meta_paths[*]}"
    )"
    data_uri="$(
        IFS=,
        echo "${data_paths[*]}"
    )"

    # --- format --multi-writer (rung 5) + the durable endpoint records
    # (rung 2, product verb) --------------------------------------------------
    log "format --multi-writer over sqmeta://$meta_uri sqdata://$data_uri"
    sqz format --multi-writer "sqmeta://$meta_uri" "sqdata://$data_uri" --force \
        >"$STATE/format.out" 2>&1 || die "format failed: $(tail -3 "$STATE/format.out")"

    # vol ids from the PRODUCT verb (never assumed): id -> backing map.
    sqz volume list "sqmeta://$meta_uri" >"$STATE/volume-list.out" 2>&1 ||
        die "volume list failed"
    local specs=() i vol_id
    for i in "${!data_paths[@]}"; do
        vol_id="$(awk -v b="${data_paths[$i]}" 'NR>1 && $NF==b {print $1}' \
            "$STATE/volume-list.out")"
        [ -n "$vol_id" ] || die "no durable volume id for ${data_paths[$i]} in volume list"
        specs+=("${vol_id}=${TCP_ADDR}:${TCP_SVC}:${data_nqns[$i]}")
    done
    log "config set-fabric-endpoints: ${specs[*]}"
    # Bounded retry: immediately after format exits, its D0 writer guard can
    # still be observed for a beat (the same-host dead-holder proof clears it
    # on the next attempt) — a transient measured on this box, never a
    # license to loop forever.
    local ep_ok=0 try
    for try in 1 2 3 4 5; do
        if sqz config set-fabric-endpoints "sqmeta://$meta_uri" "${specs[@]}" \
            >"$STATE/endpoints.out" 2>&1; then
            ep_ok=1
            break
        fi
        grep -q "holds the writer lock" "$STATE/endpoints.out" ||
            die "set-fabric-endpoints failed: $(tail -3 "$STATE/endpoints.out")"
        log "set-fabric-endpoints: post-format guard still visible (attempt $try) — retrying"
        sleep 2
    done
    [ "$ep_ok" = "1" ] ||
        die "set-fabric-endpoints never cleared the post-format guard: $(tail -3 "$STATE/endpoints.out")"

    # --- release every devsub-established controller: the DATA plane must
    # be un-pre-connected (the writer's daemon-owned connects are the point),
    # and the META plane must carry ONLY the writer's identity (rule 2) ------
    log "disconnecting the devsub's default-identity controllers (all instance NQNs)"
    for nqn in "${meta_nqns[@]}" "${data_nqns[@]}"; do
        nvme disconnect -n "$nqn" >/dev/null 2>&1 || true
    done
    sleep 1

    # --- operator-establish the META plane under the WRITER identity (the
    # §5.2 bootstrap exemption, product connect verb), in slot order ----------
    local create_pid=$$ W_HOSTNQN W_HOSTID
    W_HOSTNQN="$(member_hostnqn 0 "$create_pid")"
    W_HOSTID="$(member_hostid 0 "$create_pid")"
    local w_meta_paths=()
    for nqn in "${meta_nqns[@]}"; do
        sqz nvmeof connect --ip "$TCP_ADDR" --port "$TCP_SVC" --subnqn "$nqn" \
            --hostnqn "$W_HOSTNQN" --hostid "$W_HOSTID" >/dev/null 2>&1 ||
            die "writer-identity meta connect failed for $nqn"
        wait_for "head of $nqn" 40 head_for_nqn "$nqn"
        w_meta_paths+=("$(head_for_nqn "$nqn")")
    done
    local w_meta_uri
    w_meta_uri="$(
        IFS=,
        echo "${w_meta_paths[*]}"
    )"
    log "meta plane under writer identity: $w_meta_uri"

    # --- the 5b capability probe (pre-mount: nothing live rides the head;
    # probe_host_scoped reads TCP_ADDR/TCP_SVC via bash dynamic scoping) ------
    local host_scoped
    host_scoped="$(probe_host_scoped "${meta_nqns[0]}" "$create_pid")"
    echo "$host_scoped" >"$STATE/host_scoped"
    if [ "$host_scoped" = "1" ]; then
        log "5b capability: HOST-SCOPED fabric subsystems present (multi-identity legs unlocked)"
    else
        log "5b capability: MERGED subsystems (stock nvme_core.multipath=Y) — multi-identity legs will SKIP-loud (rung 5b: the sqz-kernel fix, validated in the 6b guest)"
    fi
    if [ "$require_hs" = "1" ] && [ "$host_scoped" != "1" ]; then
        die "--require-host-scoped-subsys: this kernel merges fabric subsystems across host identities (nvme_core.multipath=Y). Remedy: the rung-5b sqz kernel (docs/design-full-multi-writer.md rung 5b), or the documented stock-kernel workaround nvme_core.multipath=N (boot parameter)"
    fi
    if [ "$cowriters" != "0" ]; then
        [ "$host_scoped" = "1" ] ||
            die "--cowriters=$cowriters needs host-scoped fabric subsystems (rung 5b) — this kernel merges identities under one head (see the POSTURE header note)"
        die "--cowriters is gated open by the 5b kernel but its leg bodies land with rungs 7-10 (S6 arm onward) — not this rung"
    fi

    # --- persist config, mount the fleet -------------------------------------
    {
        echo "FLEET_N='$n'"
        echo "TCP_ADDR='$TCP_ADDR'"
        echo "TCP_SVC='$TCP_SVC'"
        echo "META_PATHS='$w_meta_uri'"
        echo "FORMAT_META_PATHS='$meta_uri'"
        echo "FORMAT_DATA_PATHS='$data_uri'"
        echo "META_NQNS='${meta_nqns[*]}'"
        echo "DATA_NQNS='${data_nqns[*]}'"
        echo "W_HOSTNQN='$W_HOSTNQN'"
        echo "W_HOSTID='$W_HOSTID'"
        echo "CREATE_PID='$create_pid'"
        echo "HOST_SCOPED='$host_scoped'"
    } >"$CONF"

    mount_member 0

    # Writer engagement (rung 2): every data NQN must now carry a controller
    # under the WRITER's identity — the daemon-owned connect happened.
    local ctrl found
    for nqn in "${data_nqns[@]}"; do
        found=0
        for ctrl in $(ctrls_for_nqn "$nqn" || true); do
            [ "$(ctrl_hostnqn "$ctrl")" = "$W_HOSTNQN" ] && found=1
        done
        [ "$found" = "1" ] ||
            die "data volume $nqn has no controller under the writer identity — the daemon-owned connect did not engage"
    done
    log "daemon-owned data connects verified (every data NQN carries the writer identity)"

    # Reader safety (identity-less readers open the FORMAT-TIME paths from
    # the durable records): each format-time data basename must currently
    # resolve to the SAME subsystem NQN it was recorded under. Kernel
    # instance numbers are lowest-free, so the disconnect/reconnect cycle
    # restores them on a quiet box; drift (a concurrent agent taking a
    # number) is refused LOUD here — never a silent wrong-device read.
    for i in "${!data_paths[@]}"; do
        dev="$(head_for_nqn "${data_nqns[$i]}")" ||
            die "data NQN ${data_nqns[$i]} has no head device after the writer's connect"
        [ "$dev" = "${data_paths[$i]}" ] ||
            die "device-name drift: ${data_nqns[$i]} is now $dev but was formatted as ${data_paths[$i]} (a concurrent nvme consumer moved instance numbers mid-create). Remedy: teardown and re-create on a quiet box — identity-less readers resolve the format-time path"
    done
    log "reader-safety verified (format-time data paths still name their recorded NQNs)"

    # No quiesce wait: readers bootstrap into ANY journal-tail state since
    # the rung-6 finding #1 fix (the declaration absolves the replayed
    # residue) — mount_reader_verified asserts the tripwire stays 0.

    local idx
    for ((idx = 1; idx < n; idx++)); do
        mount_reader_verified "$idx"
    done
    log "fleet up: 1 writer + $((n - 1)) reader(s), SQUEEZEFS_FLEET_SHARE=$n per daemon"
    status_fleet
}

status_fleet() {
    require_state
    echo "[mwfleet] instance=$INSTANCE target=$TCP_ADDR:$TCP_SVC host_scoped=$(cat "$STATE/host_scoped" 2>/dev/null || echo '?')"
    echo "[mwfleet] meta (writer-identity heads): $META_PATHS"
    printf '%-4s %-7s %-24s %-6s %-9s %s\n' IDX ROLE MOUNT PID LIVE IDENTITY
    local idx role mnt lg hn hi pid live
    while IFS=$'\t' read -r idx role mnt lg hn hi pid; do
        : "$lg" "$hi"
        live="dead"
        if [ -n "$pid" ] && [ "$pid" != "-" ] && kill -0 "$pid" 2>/dev/null &&
            mountpoint -q "$mnt"; then
            live="up"
        fi
        printf '%-4s %-7s %-24s %-6s %-9s %s\n' "$idx" "$role" "$mnt" "$pid" "$live" "$hn"
    done <"$MEMBERS"
}

kill_member() {
    require_state
    local idx="$1" sig="${2:-9}" pid
    pid="$(awk -F'\t' -v i="$idx" '$1==i {print $7}' "$MEMBERS")"
    [ -n "$pid" ] && [ "$pid" != "-" ] || die "member $idx has no recorded pid"
    kill "-$sig" "$pid" 2>/dev/null || die "kill -$sig $pid failed"
    log "member $idx (pid $pid) sent signal $sig"
}

teardown_fleet() {
    local rc=0
    if [ -f "$MEMBERS" ]; then
        local idx role mnt lg hn hi pid
        while IFS=$'\t' read -r idx role mnt lg hn hi pid; do
            : "$role" "$lg" "$hn" "$hi"
            if is_mounted "$mnt"; then
                sqz umount "$mnt" >/dev/null 2>&1 || umount -l "$mnt" 2>/dev/null || true
            fi
            for _ in $(seq 1 40); do
                is_mounted "$mnt" || break
                sleep 0.25
            done
            [ -n "$pid" ] && [ "$pid" != "-" ] && kill -0 "$pid" 2>/dev/null &&
                kill -9 "$pid" 2>/dev/null
            log "member $idx down"
        done <"$MEMBERS"
    fi
    # Sweep mounts the ledger does not know (partial-create residue): any
    # live mount under MNT_ROOT is ours by construction.
    # Enumerate from /proc/mounts, never the directory glob: stat() on a
    # dead FUSE mount answers ENOTCONN and would hide exactly the residue
    # this sweep exists for.
    local m mpid
    while IFS= read -r m; do
        [ -n "$m" ] || continue
        mpid="$(daemon_pid_for_mnt "$m")"
        sqz umount "$m" >/dev/null 2>&1 || umount -l "$m" 2>/dev/null || true
        for _ in $(seq 1 40); do
            is_mounted "$m" || break
            sleep 0.25
        done
        [ -n "$mpid" ] && kill -0 "$mpid" 2>/dev/null && kill -9 "$mpid" 2>/dev/null
        log "swept unledgered mount $m (pid ${mpid:-?})"
    done < <(awk -v r="$MNT_ROOT/" 'index($2, r) == 1 {print $2}' /proc/mounts)
    # Sweep daemons whose mounts already detached (lazy umounts, dead devs).
    for mpid in $(pgrep -f "squeezefs.*mount.*$MNT_ROOT/" || true); do
        kill -9 "$mpid" 2>/dev/null || true
        log "swept stray daemon pid $mpid"
    done
    sleep 1
    # Disconnect every controller still serving an instance NQN (writer
    # daemon-owned data connects + operator meta connects + probe leftovers).
    local c
    for c in /sys/class/nvme/nvme*; do
        [ -r "$c/subsysnqn" ] || continue
        case "$(cat "$c/subsysnqn" 2>/dev/null)" in
        "$NQN_PREFIX"*) nvme disconnect -d "$(basename "$c")" >/dev/null 2>&1 || true ;;
        esac
    done
    sleep 1
    if [ -d "$DEVSUB_STATE" ]; then
        SQZ_DEVSUB_TRANSPORT=tcp SQZ_DEVSUB_INSTANCE="$INSTANCE" \
            "$REPO/tests/dev_substrate.sh" teardown >/dev/null 2>&1 ||
            warn "devsub teardown reported errors"
    fi
    # --- zero-residue assertions (exit nonzero on ANY residue) --------------
    local s
    for s in "$NVMET_CFS"/subsystems/*; do
        [ -d "$s" ] || continue
        case "$(basename "$s")" in
        "$NQN_PREFIX"*)
            warn "RESIDUE: nvmet subsystem $(basename "$s") survived"
            rc=1
            ;;
        esac
    done
    for c in /sys/class/nvme/nvme*; do
        [ -r "$c/subsysnqn" ] || continue
        case "$(cat "$c/subsysnqn" 2>/dev/null)" in
        "$NQN_PREFIX"*)
            warn "RESIDUE: controller $(basename "$c") still serves an instance NQN"
            rc=1
            ;;
        esac
    done
    if [ -f "$MEMBERS" ]; then
        local pid
        while IFS=$'\t' read -r _ _ _ _ _ _ pid; do
            [ -n "$pid" ] && [ "$pid" != "-" ] && kill -0 "$pid" 2>/dev/null && {
                warn "RESIDUE: member daemon pid $pid still alive"
                rc=1
            }
        done <"$MEMBERS"
    fi
    if awk -v r="$MNT_ROOT/" 'index($2, r) == 1 {f=1} END {exit !f}' /proc/mounts; then
        warn "RESIDUE: mounts under $MNT_ROOT survived"
        rc=1
    fi
    rm -rf "$STATE"
    rmdir "$MNT_ROOT"/m* "$MNT_ROOT" 2>/dev/null || true
    if [ "$rc" -eq 0 ]; then
        log "teardown complete — zero residue"
    else
        die "teardown left residue (see WARN lines above)"
    fi
}

# --- dispatch ----------------------------------------------------------------
VERB="${1:-}"
[ -n "$VERB" ] || {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
    exit 1
}
shift || true
ensure_root "$VERB" "$@"

case "$VERB" in
create) create_fleet "$@" ;;
status) status_fleet ;;
mount) mount_member "${1:?mount needs a member index}" ;;
unmount) unmount_member "${1:?unmount needs a member index}" ;;
kill)
    IDX="${1:?kill needs a member index}"
    SIG=9
    [ "${2:-}" = "--sig" ] && SIG="${3:?--sig needs a value}"
    kill_member "$IDX" "$SIG"
    ;;
probe-host-scoped)
    require_state
    echo "host_scoped=$(cat "$STATE/host_scoped")"
    ;;
teardown) teardown_fleet ;;
pause)
    die "pause is the rung-6b VM members' verb (qemu 'stop' = hung kernel) — not built this rung"
    ;;
partition | netem)
    die "$VERB is the rung-7 netem/netns venue — not built this rung"
    ;;
*) die "unknown verb '$VERB' (create|status|mount|unmount|kill|probe-host-scoped|teardown)" ;;
esac
