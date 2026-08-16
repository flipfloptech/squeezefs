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
#   * ONE volume set formatted multi-writer-capable — the DEFAULT format
#     class since the rung-10b Phase-B flip (KD-MW-1; `--single-writer` is
#     the opt-out no fleet ever wants),
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
#     see THE 5b GATE below),
#   * with `--vm=V` (rung 6b): V qemu/KVM GUEST members — each an
#     independent kernel and independent clock domain (§12) — booted on
#     the sqz 6.19.14-sqz kernel (patch 0030,
#     nvme_core.fabrics_host_scoped_subsystems=Y) and dialing the HOST's
#     nvmet-tcp target. See THE VM LEG below.
#
# THE VM LEG (rung 6b — --vm=V, pause/resume, vm-boot/vm-exec/vm-stop):
#   Networking is qemu USER-NET (slirp) — the simplest robust shape: the
#   guest's connects to the slirp gateway 10.0.2.2 are re-originated by
#   the qemu process against the HOST's loopback, so the guest reaches
#   the instance devsub's nvmet-tcp port (127.0.0.1:$TCP_SVC) at
#   10.0.2.2:$TCP_SVC with no bridge, no tap, no host interface changes
#   and no extra privileges. Consequence: fabric addresses are DOMAIN-
#   RELATIVE (host says 127.0.0.1, guests say 10.0.2.2 — $VM_GW), so
#   guest-side volume sets record 10.0.2.2 in their fabric_endpoint:
#   records; guest members joining the HOST-formatted set as fleet
#   members (S6-b'/rung-7 rows) resolve their meta plane by in-guest
#   operator connects.
#   The guest image is a minimal busybox initramfs + the host-built
#   squeezefs binary over a 9p share (tests/mw_guest_image.sh — NOT a
#   distro pipeline; boot pair built from the sqz kernel RPMs, so
#   `--vm` REFUSES loud until docker/kernel-sqz/build.sh has produced
#   them). Control plane = the share's job executor: `vm-exec <idx>
#   <script>` runs a shell job inside the guest and returns its rc/
#   output. `pause <idx>` / `resume <idx>` are qemu monitor stop/cont —
#   the S6-b' HUNG-KERNEL shape kill-9 cannot produce (the guest's TCP
#   stack freezes mid-conversation instead of closing); the S6-b' row
#   itself is rung 7's — this rung ships the verb.
#   With --vm the devsub is built with ONE EXTRA mds + oss namespace
#   pair ($GUEST_META_NQN / $GUEST_DATA_NQN), RESERVED for the in-guest
#   legs (run_mw_matrix.sh vm-hostscope-validate / vm-multi-identity):
#   no in-guest leg ever adds a path to a subsystem a live HOST writer
#   rides, and guest-side formats never touch the fleet set. /dev/kvm
#   absent falls back to TCG, stated loud (correctness legs only).
#   Zero-residue extends to guests: teardown powers off / kills every
#   recorded guest AND sweeps any qemu process whose cmdline names this
#   fleet's state dir; a survivor is teardown-failing residue. Guests
#   have no disk images (kernel+initramfs boot, 9p share under $STATE);
#   the cached boot pair lives in target/mw-guest as a BUILD product
#   (like target/release — not fleet residue).
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
# MEMBERSHIP ARMING (rung 7 — the S6 rows; design-full-multi-writer §7.2):
#   `create --membership[=auto|addr:port]` arms the DLM S6 plane: the
#   WRITER mounts with SQUEEZEFS_MEMBERSHIP_BIND (default `auto`) and
#   becomes the lease AUTHORITY (its job wire — on by default — writes the
#   job:enroll root of trust first); READERS need no knob and JOIN at
#   mount when they find the fresh rendezvous record — the rig asserts
#   every reader's `membership_mode=member` loudly (a reader that could
#   not join is invisible-to-census residue, never a quiet green). The
#   owner's advertised endpoint is parsed from the writer log and
#   persisted (MEMBERSHIP_ENDPOINT) for the netns/guest reachability
#   preflights. `--lease-ttl-ms=N` shortens the OWNER's lease TTL
#   (SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS — a measurement lever for the
#   fence rows; the S6-a journal row runs the shipped 45 s clocks).
#
# MULTI-WRITER ARMING (rung 8 — the S7 rows; design-full-multi-writer §7.2):
#   `create --multi-writer` arms the DLM S7 data-plane custody fence on the
#   WRITER: SQUEEZEFS_MULTI_WRITER=1 demands the DEVICE-ENFORCED guarantee
#   class — the mount takes a WERO (rtype 3) reservation on EVERY data
#   namespace (data_plane_fence_mode=1) and REFUSES loud on a non-PR
#   substrate or an unstamped format (this fleet's tcp devsub is nvmet
#   resv_enable=1 and the DEFAULT format stamps the nine bits since the
#   rung-10b flip, so both rungs hold). The arm's rung 4 requires the
#   membership plane, so
#   --multi-writer IMPLIES --membership (auto) when not given explicitly.
#   Engagement is asserted per mount: data_plane_fence_mode=1 read from
#   the stats inode + the WERO acquire line in the writer log. SCOPE
#   (rung 8): the AUTHORITY arm only — the S7 rows fence the WRITER itself.
#
# CO-WRITER MEMBERS (rung 9 — the S8 rows; design-full-multi-writer §7.2):
#   `create --cowriters=K` (which implies the --multi-writer ARM — the
#   rig flag stays accepted as the explicit spelling) mounts K CO-LOCATED co-writer
#   members on the index slice 50.. (COWRITER_BASE): the DLM S9 posture —
#   metadata read-only locally with EVERY metadata verb SHIPPED to the
#   authority (the REAL S8 shipping client), data DMA its own under a
#   granted custody lease. Bring-up is the ops.md flow mechanized: each
#   mountpoint's durable enrollment id (`node_….m…` — KD-MW-2) is
#   harvested from its rung-3 refusal (probe_cowriter_id, side-effect
#   free), the AUTHORITY is re-armed with SQUEEZEFS_MW_MEMBERS=<roster>
#   (enrollment is the authority's durable act — a new era), then the
#   co-writers mount with SQUEEZEFS_MW_ROLE=co-writer +
#   SQUEEZEFS_MW_AUTHORITY=<the recorded MW_ENDPOINT>. CO-LOCATED shape
#   (ops.md §Multi-writer co-writer mounts, the honest residual): they
#   share the box's PR host identity — no explicit hostnqn, no 5b kernel
#   needed; their WERO registrant key rides the default host association.
#   A co-writer may mount in a netns (`mount 50 --netns[=<delay>]`) so its
#   SHIPPING wire is netem-shapeable — the S8-a RTT-sweep venue (netem now
#   accepts `<N>us` grain). Engagement asserted per mount: the
#   'CO-WRITER ADMITTED' log line + mount_posture=co-writer.
#
# NETNS / NETEM / PARTITION (rung 7 — the S6-b venue; stubs retired):
#   A READER member can be mounted inside its OWN network namespace
#   (`mount <idx> --netns[=<delay_ms>]`): a per-member netns + veth pair
#   (host 10.207.<100+idx>.1/24 <-> ns .2), default route via the host
#   side, loose rp_filter on the host veth. Only the daemon's USERSPACE
#   TCP (the membership/cluster wire) rides the netns — the NVMe fabric
#   is kernel-plane and block devices are namespace-blind, so the reader
#   serves I/O unchanged while its heartbeat path is shapeable:
#     netem <idx> <ms|off>   tc netem delay on BOTH veth ends (RTT +2*ms)
#     partition <idx> on|off veth link down/up (the hard partition)
#   The writer/owner never mounts in a netns (members must dial its
#   advertised endpoint). Teardown deletes every fleet netns and asserts
#   zero netns/veth residue.
#
# Verbs
#   create [N|N=<n>] [--cowriters K] [--vm=V] [--require-host-scoped-subsys]
#          [--membership[=auto|addr:port]] [--lease-ttl-ms=N] [--multi-writer]
#                 build substrate + format + records + mount the fleet
#                 (refuses if state exists — run teardown first); --vm=V
#                 boots V sqz-kernel guests after the fleet is up
#   status        member table + capability verdict + identity map + VMs
#   mount <idx> [--netns[=<delay_ms>]]   (re)mount one member (readers may
#                 mount inside their own netns — see NETNS above)
#   unmount <idx>   product umount
#   netem <idx> <ms|Nus|Nms|off>  shape a netns-mounted member's wire
#   partition <idx> on|off  hard-partition a netns-mounted member
#   kill <idx> [--sig 9]   kill a member daemon (the S7-b matrices' verb)
#   probe-host-scoped      re-print the recorded 5b capability verdict
#   vm-boot <idx> [--no-hostscope]   boot one guest (ephemeral leg guests
#                 use idx >= 90; --no-hostscope boots WITHOUT the 0030
#                 param — the vm-hostscope-validate negative arm)
#   vm-exec <idx> <script-file|->    run a shell job inside the guest
#                 (job executor over the 9p share); prints output,
#                 propagates the job's exit code
#   vm-stop <idx>          power off one guest (poweroff job -> monitor
#                 quit -> SIGKILL escalation; console/log preserved)
#   pause <idx> | resume <idx>       qemu monitor stop/cont on a guest —
#                 the S6-b' hung-kernel verb (the row is rung 7's)
#   teardown      guests + netns + unmount + kill + disconnect + substrate
#                 teardown + ZERO-RESIDUE assertions (exits nonzero on
#                 any residue, qemu survivors and netns/veth included)
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
#   SQZ_MWFLEET_VM_MEM_MB=3072  guest RAM (R5 derives from it in-guest)
#   SQZ_MWFLEET_VM_CPUS=4       guest vcpus (fuse-over-uring queue count)
#   SQZ_MWGUEST_OUT             boot-pair cache (tests/mw_guest_image.sh)
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
VMS="$STATE/vms.tsv"         # idx pid accel hostscope(1|0) vmdir
CONF="$STATE/config.env"

# Rung-6b guest geometry (see THE VM LEG header note).
VM_GW="10.0.2.2" # the slirp gateway = the HOST, guest-domain address
VM_MEM_MB="${SQZ_MWFLEET_VM_MEM_MB:-3072}"
VM_CPUS="${SQZ_MWFLEET_VM_CPUS:-4}"
GUEST_IMG_DIR="${SQZ_MWGUEST_OUT:-$REPO/target/mw-guest}"

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
    < <(env | grep -E '^(SQZ_MWFLEET_|SQZ_DEVSUB_|SQZ_MWGUEST_)' || true)
unset __kv
sqz() { env "${SCRUB_ENV[@]}" "$SQZ" "$@"; }

ensure_root() {
    [ "$(id -u)" -eq 0 ] && return 0
    log "root required (nvmet configfs, mounts, fabrics connects) — re-executing via sudo"
    local knobs=()
    while IFS= read -r kv; do knobs+=("$kv"); done \
        < <(env | grep -E '^(SQZ_MWFLEET_|SQZ_DEVSUB_|SQZ_MWGUEST_|SQZ_BIN=)' || true)
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

# Rung 9: co-writer members live on their own index slice (the VM-slice
# precedent — readers keep 1..N-1 untouched).
COWRITER_BASE=50

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

# --- rung-6b guests (THE VM LEG header note) ---------------------------------
vm_dir() { echo "$STATE/vm$1"; }

vm_pid() { # idx -> recorded pid or empty
    [ -f "$VMS" ] || return 0
    awk -F'\t' -v i="$1" '$1==i {print $2}' "$VMS"
}

vm_alive() { # idx
    local pid
    pid="$(vm_pid "$1")"
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null
}

# HMP over the monitor unix socket (python3 — socat is not a prereq).
vm_monitor_cmd() { # idx cmd
    local d
    d="$(vm_dir "$1")"
    [ -S "$d/monitor.sock" ] || die "guest $1 has no monitor socket at $d/monitor.sock"
    python3 - "$d/monitor.sock" "$2" <<'PYEOF'
import socket, sys, time
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
s.settimeout(5)
try:
    s.recv(4096)  # the HMP banner
except OSError:
    pass
s.sendall((sys.argv[2] + "\n").encode())
time.sleep(0.2)
s.close()
PYEOF
}

# Boot one guest. Usage: vm_boot <idx> [--no-hostscope]
# The 0030 param rides the kernel cmdline by default (the sqz fleet/guest
# posture, design-mw-multipath-kernel §3); --no-hostscope boots the SAME
# kernel with the param absent — the vm-hostscope-validate negative arm.
vm_boot() {
    local idx="$1" hostscope=1
    [ "${2:-}" = "--no-hostscope" ] && hostscope=0
    vm_alive "$idx" && die "guest $idx is already running (pid $(vm_pid "$idx"))"
    [ -f "$GUEST_IMG_DIR/vmlinuz" ] && [ -f "$GUEST_IMG_DIR/initramfs.img" ] ||
        die "no guest boot pair at $GUEST_IMG_DIR — build the sqz kernel (docker/kernel-sqz/build.sh), then: tests/mw_guest_image.sh build"
    command -v qemu-system-x86_64 >/dev/null 2>&1 || die "qemu-system-x86_64 is required for --vm/vm-boot"
    local d accel append
    d="$(vm_dir "$idx")"
    rm -rf "$d" && mkdir -p "$d/share/jobs" "$d/share/bin" "$d/share/lib"
    # Stage the HOST-BUILT binary + its loader closure onto the 9p share
    # (rung-6b charter: the binary is shared, never baked into the image).
    cp "$SQZ" "$d/share/bin/squeezefs"
    # KD-MW-2: a stable per-guest node identity (the initramfs has no
    # machine-id; the guest init installs this as /etc/squeezefs/node-id).
    # Stable across reboots of THIS guest (the vm dir persists for the
    # fleet's life), distinct across guests and from the host.
    echo "mwfleet-guest-$INSTANCE-$idx-$(hostname)" >"$d/share/node-id"
    local lib
    while IFS= read -r lib; do
        [ -f "$lib" ] && cp "$lib" "$d/share/lib/"
    done < <(ldd "$SQZ" 2>/dev/null | awk '{ for (i=1;i<=NF;i++) if ($i ~ /^\//) print $i }' | sort -u)
    accel=kvm
    if [ ! -w /dev/kvm ]; then
        accel=tcg
        warn "guest $idx: /dev/kvm unavailable — TCG fallback (correctness legs only, stated loud)"
    fi
    append="console=ttyS0 rdinit=/init panic=-1"
    [ "$hostscope" = "1" ] && append="$append nvme_core.fabrics_host_scoped_subsystems=Y"
    # shellcheck disable=SC2054 # commas live inside quoted qemu option strings
    local qemu_args=(-machine "q35,accel=$accel")
    [ "$accel" = "kvm" ] && qemu_args+=(-cpu host)
    # shellcheck disable=SC2054 # commas live inside quoted qemu option strings
    qemu_args+=(
        -smp "$VM_CPUS" -m "$VM_MEM_MB"
        -kernel "$GUEST_IMG_DIR/vmlinuz" -initrd "$GUEST_IMG_DIR/initramfs.img"
        -append "$append"
        -netdev user,id=n0 -device virtio-net-pci,netdev=n0
        -virtfs "local,path=$d/share,mount_tag=hostshare,security_model=none"
        -display none -serial "file:$d/console.log"
        -monitor "unix:$d/monitor.sock,server,nowait"
        -pidfile "$d/qemu.pid" -daemonize
    )
    qemu-system-x86_64 "${qemu_args[@]}" ||
        die "qemu launch failed for guest $idx (console: $d/console.log)"
    local pid ready=0 i
    pid="$(cat "$d/qemu.pid")"
    for ((i = 0; i < 240; i++)); do
        [ -f "$d/share/guest-ready" ] && ready=1 && break
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.5
    done
    if [ "$ready" != "1" ]; then
        kill -9 "$pid" 2>/dev/null || true
        die "guest $idx never reached ready (console tail: $(tail -3 "$d/console.log" 2>/dev/null | tr '\n' ' '))"
    fi
    grep -v "^$idx	" "$VMS" >"$VMS.new" 2>/dev/null || true
    printf '%s\t%s\t%s\t%s\t%s\n' "$idx" "$pid" "$accel" "$hostscope" "$d" >>"$VMS.new"
    sort -n "$VMS.new" >"$VMS" && rm -f "$VMS.new"
    log "guest $idx up (pid $pid, accel=$accel, hostscope=$hostscope, kernel $(cat "$d/share/guest-ready"))"
}

# Run one shell job inside a guest via the share's job executor.
# Usage: vm_exec <idx> <script-file|-> [timeout-s]  — prints the job's
# output and propagates its exit code.
vm_exec() {
    local idx="$1" src="${2:--}" to="${3:-300}" d seq job rc i
    vm_alive "$idx" || die "guest $idx is not running"
    d="$(vm_dir "$idx")"
    seq=$(($(cat "$d/jobseq" 2>/dev/null || echo 0) + 1))
    echo "$seq" >"$d/jobseq"
    job="$d/share/jobs/j$seq"
    if [ "$src" = "-" ]; then cat >"$job.sh.tmp"; else cp "$src" "$job.sh.tmp"; fi
    mv "$job.sh.tmp" "$job.sh"
    for ((i = 0; i < to * 2; i++)); do
        [ -f "$job.rc" ] && break
        vm_alive "$idx" || die "guest $idx died mid-job (console: $d/console.log)"
        sleep 0.5
    done
    [ -f "$job.rc" ] || die "guest $idx job j$seq timed out after ${to}s (output so far: $d/share/jobs/j$seq.out)"
    cat "$job.out" 2>/dev/null || true
    rc="$(cat "$job.rc")"
    return "$rc"
}

vm_pause() { # idx — qemu STOP: the S6-b' hung-kernel shape (verb only
    # this rung; the row is rung 7's). The guest's TCP stack freezes
    # mid-conversation instead of closing — the shape kill-9 cannot make.
    local idx="$1"
    vm_alive "$idx" || die "guest $idx is not running"
    vm_monitor_cmd "$idx" stop
    log "guest $idx PAUSED (qemu stop — vcpus + guest clock frozen, TCP left mid-conversation)"
}

vm_resume() { # idx — qemu cont
    local idx="$1"
    vm_alive "$idx" || die "guest $idx is not running"
    vm_monitor_cmd "$idx" cont
    log "guest $idx resumed (qemu cont)"
}

vm_stop() { # idx — poweroff job -> monitor quit -> SIGKILL; logs preserved
    local idx="$1" d pid i
    d="$(vm_dir "$idx")"
    pid="$(vm_pid "$idx")"
    [ -n "$pid" ] || {
        warn "guest $idx has no recorded pid"
        return 0
    }
    if kill -0 "$pid" 2>/dev/null; then
        # A paused guest cannot run jobs — resume first (best-effort).
        vm_monitor_cmd "$idx" cont 2>/dev/null || true
        echo "poweroff -f" >"$d/share/jobs/off.sh.tmp" 2>/dev/null &&
            mv "$d/share/jobs/off.sh.tmp" "$d/share/jobs/off.sh" 2>/dev/null || true
        for ((i = 0; i < 20; i++)); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.5
        done
        kill -0 "$pid" 2>/dev/null && vm_monitor_cmd "$idx" quit 2>/dev/null || true
        for ((i = 0; i < 10; i++)); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.5
        done
        kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null || true
    fi
    wait_for "guest $idx exit" 40 bash -c "! kill -0 '$pid' 2>/dev/null"
    if [ -f "$VMS" ]; then
        grep -v "^$idx	" "$VMS" >"$VMS.new" 2>/dev/null || true
        mv "$VMS.new" "$VMS"
    fi
    log "guest $idx down (console preserved at $d/console.log)"
}

# --- rung-7 netns/netem plumbing (the S6-b venue — see the header note) ------
ns_name() { echo "sqzmw-$INSTANCE-m$1"; }
veth_host() { echo "sqzmw${1}h"; }
veth_ns() { echo "sqzmw${1}n"; }
ns_subnet() { echo "10.207.$((100 + $1))"; }

ns_exists() { ip netns list 2>/dev/null | awk '{print $1}' | grep -qx "$(ns_name "$1")"; }

netns_setup() { # idx — create the member's netns + veth + routes
    local idx="$1" ns hv nv net
    ns="$(ns_name "$idx")" hv="$(veth_host "$idx")" nv="$(veth_ns "$idx")" net="$(ns_subnet "$idx")"
    ns_exists "$idx" && die "netns $ns already exists — netns residue from a prior mount (unmount $idx first)"
    ip netns add "$ns"
    ip link add "$hv" type veth peer name "$nv"
    ip link set "$nv" netns "$ns"
    ip addr add "$net.1/24" dev "$hv"
    ip link set "$hv" up
    ip netns exec "$ns" ip addr add "$net.2/24" dev "$nv"
    ip netns exec "$ns" ip link set lo up
    ip netns exec "$ns" ip link set "$nv" up
    ip netns exec "$ns" ip route add default via "$net.1"
    # The member dials the owner's PRIMARY-interface endpoint through the
    # veth; strict rp_filter on the host side would drop the 10.207/24
    # source arriving toward a non-veth local address.
    sysctl -qw "net.ipv4.conf.$hv.rp_filter=2" || true
    log "netns $ns up (veth $hv <-> $nv, $net.0/24)"
}

netns_teardown() { # idx — best-effort delete (veth pair dies with the ns)
    local idx="$1" ns hv
    ns="$(ns_name "$idx")" hv="$(veth_host "$idx")"
    ip netns del "$ns" 2>/dev/null || true
    ip link del "$hv" 2>/dev/null || true
}

netem_set() { # idx <ms|off>
    local idx="$1" spec="$2" ns hv nv
    ns="$(ns_name "$idx")" hv="$(veth_host "$idx")" nv="$(veth_ns "$idx")"
    ns_exists "$idx" || die "member $idx is not netns-mounted (mount $idx --netns first)"
    if [ "$spec" = "off" ]; then
        tc qdisc del dev "$hv" root 2>/dev/null || true
        ip netns exec "$ns" tc qdisc del dev "$nv" root 2>/dev/null || true
        log "netem cleared on member $idx's veth pair"
        return 0
    fi
    # Rung 9 (the S8-a RTT sweep needs µs grain): a bare number stays ms
    # (the rung-7 contract); an explicit `<N>us` / `<N>ms` suffix passes
    # through to tc verbatim.
    local unit_spec
    if [[ "$spec" =~ ^[0-9]+$ ]]; then
        unit_spec="${spec}ms"
    elif [[ "$spec" =~ ^[0-9]+(us|ms)$ ]]; then
        unit_spec="$spec"
    else
        die "netem takes a delay in ms, <N>us, <N>ms, or 'off' (got '$spec')"
    fi
    # BOTH ends (each direction pays the delay once): wire RTT +2*delay.
    tc qdisc replace dev "$hv" root netem delay "$unit_spec"
    ip netns exec "$ns" tc qdisc replace dev "$nv" root netem delay "$unit_spec"
    log "netem delay $unit_spec armed on both ends of member $idx's veth (wire RTT +2x$unit_spec)"
    tc -s qdisc show dev "$hv" | head -2
}

partition_set() { # idx <on|off>
    local idx="$1" state="$2" hv
    hv="$(veth_host "$idx")"
    ns_exists "$idx" || die "member $idx is not netns-mounted (mount $idx --netns first)"
    case "$state" in
    on)
        ip link set "$hv" down
        log "member $idx PARTITIONED (veth $hv down — the hard-partition shape)"
        ;;
    off)
        ip link set "$hv" up
        log "member $idx partition healed (veth $hv up)"
        ;;
    *) die "partition takes on|off (got '$state')" ;;
    esac
}

# --- verbs -------------------------------------------------------------------
mount_member() { # idx [--netns[=<delay_ms>]]
    require_state
    local idx="$1" mnt log role netns=0 netem_ms=""
    case "${2:-}" in
    "") : ;;
    --netns) netns=1 ;;
    --netns=*)
        netns=1
        netem_ms="${2#--netns=}"
        [[ "$netem_ms" =~ ^[0-9]+$ ]] || die "--netns=<delay_ms> needs a number (got '$netem_ms')"
        ;;
    *) die "unknown mount argument '${2}'" ;;
    esac
    mnt="$(mnt_of "$idx")"
    log="$STATE/m${idx}.log"
    mkdir -p "$mnt"
    mountpoint -q "$mnt" && die "member $idx already mounted at $mnt"
    # The daemon environment: the fleet-share divisor (rung 3b) plus a
    # scrub of the rig's own SQZ_* control variables — they are not
    # SqueezeFS knobs and would trip the registry's typo announcement.
    local env_args=("${SCRUB_ENV[@]}")
    env_args+=("SQUEEZEFS_FLEET_SHARE=$FLEET_N") # options before assignments
    # Dev-tree build identities (`-dirty`) are degenerate under the KD-7
    # skew gate; the admin lane (online fsck — the S7-b oracle) refuses
    # them without the counted dev override (the run_preload_gate.sh
    # precedent — announced loudly on both ends, ipc_binds_dev_override).
    env_args+=("SQUEEZEFS_IPC_ALLOW_DEV=1")
    if [ "$idx" -eq 0 ]; then
        role="writer"
        [ "$netns" = "0" ] ||
            die "the writer/owner never mounts in a netns — members must dial its advertised endpoint (see the NETNS header note)"
        # Rung 7: the membership arm rides the WRITER (the lease
        # authority); its job wire (on by default) writes job:enroll first.
        if [ -n "${MEMBERSHIP:-}" ]; then
            env_args+=("SQUEEZEFS_MEMBERSHIP_BIND=$MEMBERSHIP")
            [ -n "${MEMBERSHIP_LEASE_TTL_MS:-}" ] &&
                env_args+=("SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS=$MEMBERSHIP_LEASE_TTL_MS")
        fi
        # Rung 8: the S7/S9 AUTHORITY arm — the device-enforced guarantee
        # class (WERO rtype 3 on every data namespace). Rung 9: the bind is
        # a STABLE rig port (SQZ_MWFLEET_MW_PORT, default 45999), because
        # the S8-b era split needs a SUCCESSOR authority the old era's
        # frames can still reach — `auto` mints a fresh ephemeral port per
        # incarnation, so old-era clients would dial a dead endpoint
        # forever and the stale-term/era-relearn split could never engage.
        # (Field topology is the same: operators bind a known port.)
        if [ "${MW:-0}" = "1" ]; then
            env_args+=("SQUEEZEFS_MULTI_WRITER=1")
            # Rung-10 rig finding: the port must be CONF-persistent, not
            # ambient — a successor remounted from a LATER invocation
            # (matrix legs, operators) that lacks SQZ_MWFLEET_MW_PORT in
            # its environment would otherwise re-arm on the DEFAULT port
            # while every co-writer keeps dialing the recorded
            # MW_ENDPOINT: re-admission then refuses 'Connection refused'
            # forever (the s9-failover first run's shape).
            env_args+=("SQUEEZEFS_MW_BIND=0.0.0.0:${SQZ_MWFLEET_MW_PORT:-${MW_PORT:-45999}}")
        fi
        # Rung 9: the operator-declared co-writer roster (enrollment is the
        # AUTHORITY's durable act — ops.md §Multi-writer co-writer mounts).
        if [ -n "${MW_ROSTER:-}" ]; then
            env_args+=("SQUEEZEFS_MW_MEMBERS=$MW_ROSTER")
        fi
        # The proven N=1 explicit-identity shape: data plane daemon-owned
        # from the fabric_endpoint records. (The KD-MW-3 ENGAGED stdout
        # line stays inside the daemonized child; engagement is asserted
        # below via the daemon log + sysfs.)
        env "${env_args[@]}" "$SQZ" mount "sqmeta://$META_PATHS" "$mnt" \
            -o "hostnqn=$W_HOSTNQN,hostid=$W_HOSTID" \
            --daemon --allow-other --log-file "$log" \
            >"$STATE/m0.mount.out" 2>&1 ||
            die "writer mount failed: $(cat "$STATE/m0.mount.out")"
    elif [ "$idx" -ge "$COWRITER_BASE" ]; then
        # Rung 9 (the S8 arm): a CO-WRITER member — the DLM S9 posture, the
        # REAL S8 shipping client (every metadata verb ships to the
        # authority; data DMA is its own under a granted custody lease).
        # CO-LOCATED shape (docs/operations.md §Multi-writer co-writer
        # mounts, the stated honest residual): it shares the box's PR host
        # identity, so it mounts with NO explicit hostnqn — its WERO
        # registrant key rides the default host association, distinct from
        # the writer's explicit member-0 identity. No 5b kernel needed.
        role="cowriter"
        [ "${MW:-0}" = "1" ] || die "co-writer members need a multi-writer-ARMED fleet (create ... --cowriters=K arms it automatically)"
        [ -n "${MW_ENDPOINT:-}" ] || die "no MW_ENDPOINT recorded — the writer's 'MULTI-WRITER ARMED' line was not parsed (writer log: $STATE/m0.log)"
        env_args+=("SQUEEZEFS_MULTI_WRITER=1")
        env_args+=("SQUEEZEFS_MW_ROLE=co-writer")
        env_args+=("SQUEEZEFS_MW_AUTHORITY=$MW_ENDPOINT")
        local launch=(env "${env_args[@]}" "$SQZ")
        if [ "$netns" = "1" ]; then
            netns_setup "$idx"
            launch=(nsenter "--net=/run/netns/$(ns_name "$idx")" env "${env_args[@]}" "$SQZ")
        fi
        "${launch[@]}" mount "sqmeta://$META_PATHS" "$mnt" \
            --daemon --allow-other --log-file "$log" \
            >"$STATE/m${idx}.mount.out" 2>&1 ||
            die "co-writer $idx mount failed: $(cat "$STATE/m${idx}.mount.out")"
        [ -n "$netem_ms" ] && netem_set "$idx" "$netem_ms"
    else
        role="reader"
        # Rung 7 (the S6-b venue): a reader may mount inside its own netns
        # so its membership wire is shapeable (netem/partition) — block
        # devices and the FUSE mount are namespace-blind.
        local launch=(env "${env_args[@]}" "$SQZ")
        if [ "$netns" = "1" ]; then
            netns_setup "$idx"
            # nsenter --net, NEVER `ip netns exec`: the latter unshares a
            # MOUNT namespace too (to bind /etc/netns + remount /sys), so
            # the FUSE mount would land invisible to the root mount ns.
            launch=(nsenter "--net=/run/netns/$(ns_name "$idx")" env "${env_args[@]}" "$SQZ")
        fi
        # DLM S5 reader: no identity, no claim, no registrant (POSTURE).
        "${launch[@]}" mount "sqmeta://$META_PATHS" "$mnt" \
            --read-only --daemon --allow-other --log-file "$log" \
            >"$STATE/m${idx}.mount.out" 2>&1 ||
            die "reader $idx mount failed: $(cat "$STATE/m${idx}.mount.out")"
        [ -n "$netem_ms" ] && netem_set "$idx" "$netem_ms"
    fi
    wait_for "member $idx mountpoint" 40 mountpoint -q "$mnt"
    wait_for "member $idx stats inode" 40 test -s "$mnt/.stats"
    if [ "$role" = "cowriter" ]; then
        # Rung 9 engagement: the five-rung ladder ADMITTED and the mount is
        # the co-writer posture (never a silently-degraded authority/reader).
        grep -q "CO-WRITER ADMITTED" "$log" ||
            die "co-writer $idx log carries no 'CO-WRITER ADMITTED' line — the admission ladder did not engage (log: $log)"
        local posture
        posture="$(stat_field "$mnt" mount_posture)"
        [ "$posture" = "co-writer" ] ||
            die "co-writer $idx mount_posture='$posture' (want co-writer) — log: $log"
        log "member $idx co-writer posture engaged (CO-WRITER ADMITTED, mount_posture=co-writer)"
    fi
    if [ "$idx" -eq 0 ]; then
        # Rung-2 engagement, half 1: the daemon's own log names each data
        # volume's daemon-owned controller (half 2 — sysfs — runs in create).
        grep -q "daemon-owned controller resolved" "$log" ||
            die "writer log carries no 'daemon-owned controller resolved' line — the rung-2 connect path did not engage (log: $log)"
        # Rung 8 engagement: the MW arm must have taken the WERO hold on
        # every data namespace — the fence-mode gauge is the instrument
        # (0 would mean the mount silently degraded to detection grade,
        # which the arm is contractually forbidden to do: it refuses).
        if [ "${MW:-0}" = "1" ]; then
            local fm ftry
            fm=""
            for ((ftry = 0; ftry < 40; ftry++)); do
                fm="$(stat_field "$mnt" data_plane_fence_mode)"
                [ "$fm" = "1" ] && break
                sleep 0.5
            done
            [ "$fm" = "1" ] ||
                die "writer data_plane_fence_mode='$fm' (want 1) — the S7 WERO hold did not engage (log: $log)"
            grep -q "data-plane WERO (rtype 3) acquired" "$log" ||
                die "writer log carries no 'data-plane WERO (rtype 3) acquired' line — the S7 arm did not engage (log: $log)"
            log "member 0 multi-writer data plane engaged (data_plane_fence_mode=1, WERO held)"
        fi
    fi
    # Rung 7: membership engagement is asserted PER MOUNT when armed — an
    # owner that did not arm or a reader that could not join is residue,
    # never a quiet green (the join is at-mount, so this converges fast;
    # netns members get the same deadline through their veth).
    if [ -n "${MEMBERSHIP:-}" ]; then
        local want_mode got_mode mtry
        if [ "$idx" -eq 0 ]; then want_mode="owner"; else want_mode="member"; fi
        got_mode=""
        for ((mtry = 0; mtry < 60; mtry++)); do
            got_mode="$(stat_field "$mnt" membership_mode)"
            [ "$got_mode" = "$want_mode" ] && break
            sleep 0.5
        done
        [ "$got_mode" = "$want_mode" ] ||
            die "member $idx membership_mode='$got_mode' (want $want_mode) — the S6 plane did not engage (log: $log)"
        log "member $idx membership engaged (mode=$want_mode)"
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

# Rung 9: parse the writer's ADVERTISED custody+publish endpoint (what a
# co-writer dials — SQUEEZEFS_MW_AUTHORITY) from its MW arm line, and
# persist/refresh it in the fleet config (append wins on re-source).
record_mw_endpoint() {
    local ep
    ep="$(sed -n 's/.*MULTI-WRITER ARMED (DLM S9) on \(.*\): era.*/\1/p' "$STATE/m0.log" | tail -1)"
    [ -n "$ep" ] || die "writer log carries no 'MULTI-WRITER ARMED (DLM S9) on <endpoint>' line (log: $STATE/m0.log)"
    echo "MW_ENDPOINT='$ep'" >>"$CONF"
    MW_ENDPOINT="$ep"
    log "multi-writer authority endpoint: $ep"
}

# Rung 9, the enrollment harvest (ops.md §Multi-writer co-writer mounts,
# "Bringing one up" steps 2-3, mechanized): a co-writer mount attempt
# against an authority whose roster does not name it is REFUSED at rung 3,
# and the refusal prints this mountpoint's durable enrollment id
# (`node_{16 hex}.m{8 hex}` — KD-MW-2's (node, mount_slot) pair, stable per
# mount point). The probe expects exactly that refusal and echoes the id;
# gather_admission mutates nothing before rung 5, so the probe is
# side-effect-free.
probe_cowriter_id() { # idx -> echoes the durable enrollment id
    local idx="$1" mnt out id
    mnt="$(mnt_of "$idx")"
    out="$STATE/m${idx}.probe.out"
    mkdir -p "$mnt"
    if env "${SCRUB_ENV[@]}" "SQUEEZEFS_FLEET_SHARE=$FLEET_N" \
        "SQUEEZEFS_IPC_ALLOW_DEV=1" \
        "SQUEEZEFS_MULTI_WRITER=1" "SQUEEZEFS_MW_ROLE=co-writer" \
        "SQUEEZEFS_MW_AUTHORITY=$MW_ENDPOINT" \
        "$SQZ" mount "sqmeta://$META_PATHS" "$mnt" \
        --daemon --allow-other --log-file "$STATE/m${idx}.probe.log" \
        >"$out" 2>&1; then
        die "co-writer $idx PROBE mount was ADMITTED against a roster that does not name it — rung 3 did not engage (out: $out)"
    fi
    id="$(grep -o "Add 'node_[0-9a-f.m]*'" "$out" | head -1 | sed "s/^Add '//; s/'$//")"
    [ -n "$id" ] || die "co-writer $idx probe refusal carries no enrollment id (want the rung-3 \"Add 'node_…'\" remedy; out: $out)"
    echo "$id"
}

unmount_member() { # idx
    require_state
    local idx="$1" mnt pid
    mnt="$(mnt_of "$idx")"
    pid="$(awk -F'\t' -v i="$idx" '$1==i {print $7}' "$MEMBERS" 2>/dev/null)"
    # A FENCED co-writer (S7 self-fence: FUSE connection aborted, daemon
    # poisoned-but-alive) makes `mountpoint -q` LIE (EINVAL reads as
    # not-mounted) while /proc/mounts still carries the entry — every
    # probe below is /proc/mounts-based (is_mounted), and the lazy-umount
    # arm always runs when the entry survives the product verb.
    if is_mounted "$mnt"; then
        sqz umount "$mnt" >/dev/null 2>&1 || true
    fi
    if is_mounted "$mnt"; then
        umount -l "$mnt" 2>/dev/null || true
    fi
    wait_for "member $idx unmount" 60 bash -c "! awk -v m='$mnt' '\$2==m {f=1} END {exit !f}' /proc/mounts"
    # Reap a lingering daemon (a fenced holder is dead-until-remount BY
    # CONTRACT; its process surviving a lazy detach would hold the flock
    # probes and the ledger pid against the successor's slot).
    if [ -n "$pid" ] && [ "$pid" != "-" ] && kill -0 "$pid" 2>/dev/null; then
        local t
        for ((t = 0; t < 20; t++)); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.5
        done
        if kill -0 "$pid" 2>/dev/null; then
            kill -9 "$pid" 2>/dev/null || true
            log "member $idx daemon (pid $pid) reaped after detach (a fenced holder is dead-until-remount)"
        fi
    fi
    if ns_exists "$idx"; then
        netns_teardown "$idx"
        log "member $idx netns removed"
    fi
    log "member $idx unmounted"
}

create_fleet() {
    local n="$N_DEFAULT" cowriters=0 require_hs=0 vms=0 mw=0 a
    local membership="${SQZ_MWFLEET_MEMBERSHIP:-}" lease_ttl_ms="${SQZ_MWFLEET_LEASE_TTL_MS:-}"
    for a in "$@"; do
        case "$a" in
        N=*) n="${a#N=}" ;;
        --cowriters)
            die "--cowriters takes a value (--cowriters K)"
            ;;
        --cowriters=*) cowriters="${a#--cowriters=}" ;;
        --require-host-scoped-subsys) require_hs=1 ;;
        --vm) die "--vm takes a value (--vm=V)" ;;
        --vm=*) vms="${a#--vm=}" ;;
        --membership) membership="auto" ;;
        --membership=*) membership="${a#--membership=}" ;;
        --lease-ttl-ms=*) lease_ttl_ms="${a#--lease-ttl-ms=}" ;;
        --multi-writer) mw=1 ;;
        [0-9]*) n="$a" ;;
        *) die "unknown create argument '$a'" ;;
        esac
    done
    [[ "$n" =~ ^[0-9]+$ ]] && [ "$n" -ge 1 ] || die "N must be a positive integer (got '$n')"
    [[ "$vms" =~ ^[0-9]+$ ]] || die "--vm=V needs a non-negative integer (got '$vms')"
    [ -z "$lease_ttl_ms" ] || [[ "$lease_ttl_ms" =~ ^[0-9]+$ ]] ||
        die "--lease-ttl-ms takes milliseconds (got '$lease_ttl_ms')"
    [ -z "$lease_ttl_ms" ] || [ -n "$membership" ] || [ "$mw" = "1" ] ||
        die "--lease-ttl-ms is the OWNER's membership lease knob — it needs --membership"
    [[ "$cowriters" =~ ^[0-9]+$ ]] || die "--cowriters=K needs a non-negative integer (got '$cowriters')"
    if [ "$cowriters" -gt 0 ] && [ "$mw" != "1" ]; then
        # A co-writer's admission rung 1 demands the opt-in on BOTH halves.
        mw=1
        log "--cowriters implies --multi-writer (the admission ladder's rung 1)"
    fi
    if [ "$mw" = "1" ] && [ -z "$membership" ]; then
        # The MW arm's rung 4 refuses with membership off (a co-writer that
        # cannot be SEEN cannot be EVICTED) — imply the default arm loudly.
        membership="auto"
        log "--multi-writer implies --membership (the S9 arm's rung 4 refuses with the plane off)"
    fi
    [ -e "$CONF" ] && die "fleet state exists at $STATE — run 'sudo tests/mw_fleet.sh teardown' first"
    ensure_prereqs
    # --vm preflight FIRST (fail before any substrate exists): the boot
    # pair must be buildable from the sqz kernel RPMs.
    if [ "$vms" -gt 0 ]; then
        command -v qemu-system-x86_64 >/dev/null 2>&1 ||
            die "--vm=$vms: qemu-system-x86_64 is required"
        "$REPO/tests/mw_guest_image.sh" build ||
            die "--vm=$vms: guest boot pair build failed (build the sqz kernel first: docker/kernel-sqz/build.sh)"
        [ -w /dev/kvm ] ||
            warn "--vm=$vms: /dev/kvm unavailable — guests will run TCG (correctness legs only)"
    fi

    # With guests, ONE EXTRA mds + oss namespace pair is RESERVED for the
    # in-guest legs (THE VM LEG header note): guest-side connects/formats
    # never touch a subsystem a live host writer rides.
    local mds_total="$MDS_COUNT" oss_total="$OSS_COUNT"
    if [ "$vms" -gt 0 ]; then
        mds_total=$((MDS_COUNT + 1))
        oss_total=$((OSS_COUNT + 1))
    fi
    log "building tcp devsub instance '$INSTANCE' ($mds_total mds + $oss_total oss$([ "$vms" -gt 0 ] && echo ', last pair RESERVED for guest legs'))"
    SQZ_DEVSUB_TRANSPORT=tcp SQZ_DEVSUB_INSTANCE="$INSTANCE" \
        SQZ_DEVSUB_MDS_COUNT="$mds_total" SQZ_DEVSUB_OSS_COUNT="$oss_total" \
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
    [ "${#meta_nqns[@]}" -eq "$mds_total" ] || die "manifest mds count mismatch"
    [ "${#data_nqns[@]}" -eq "$oss_total" ] || die "manifest oss count mismatch"
    # Split off the reserved guest-leg pair (the LAST of each role).
    local guest_meta_nqn="" guest_data_nqn=""
    if [ "$vms" -gt 0 ]; then
        guest_meta_nqn="${meta_nqns[$((mds_total - 1))]}"
        guest_data_nqn="${data_nqns[$((oss_total - 1))]}"
        meta_nqns=("${meta_nqns[@]:0:$MDS_COUNT}")
        data_nqns=("${data_nqns[@]:0:$OSS_COUNT}")
        log "guest-leg reserved namespaces: meta=$guest_meta_nqn data=$guest_data_nqn"
    fi

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

    # Rung 7 (the S6-b' guest-member row): record the RESERVED pair's
    # format-era paths too — the in-guest connect plan reproduces the
    # WHOLE format-time instance numbering, so the fleet's format-time
    # data paths resolve to the right namespaces inside a fresh guest
    # kernel (gaps in the numbering would shift every later instance).
    local guest_meta_path="" guest_data_path=""
    if [ "$vms" -gt 0 ]; then
        guest_meta_path="$(head_for_nqn "$guest_meta_nqn")" ||
            die "no head device for the reserved guest meta NQN"
        guest_data_path="$(head_for_nqn "$guest_data_nqn")" ||
            die "no head device for the reserved guest data NQN"
    fi

    local meta_uri data_uri
    meta_uri="$(
        IFS=,
        echo "${meta_paths[*]}"
    )"
    data_uri="$(
        IFS=,
        echo "${data_paths[*]}"
    )"

    # --- format (multi-writer-capable is the DEFAULT class since the
    # rung-10b Phase-B flip — no flag needed) + the durable endpoint
    # records (rung 2, product verb) ------------------------------------------
    log "format (default = multi-writer-capable) over sqmeta://$meta_uri sqdata://$data_uri"
    sqz format "sqmeta://$meta_uri" "sqdata://$data_uri" --force \
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
    # NOTE (rung 6b): the RESERVED guest-leg pair's host controllers stay
    # CONNECTED on purpose — they are instance-number ANCHORS. The
    # disconnect/reconnect cycle below restores every fleet head's
    # format-time /dev name only because the kernel hands out lowest-free
    # instance numbers; vacating the reserved pair's slots would shift
    # the daemon-owned data connects onto different numbers and trip the
    # reader-safety drift refusal. The idle host controllers never carry
    # I/O (guest legs ride the TARGET through their own guest-kernel
    # controllers); teardown's instance-NQN sweep drops them.
    log "disconnecting the devsub's default-identity controllers (fleet NQNs; reserved pair kept as instance anchors)"
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
        # Rung 9: CO-LOCATED co-writers need no 5b kernel — they share the
        # box's PR host identity (the ops.md honest residual: on this shape
        # the metadata read-only half is enforced by the mount's own code,
        # not by the device), so no second fabric identity is created and
        # the merged-head POSTURE note does not apply. Multi-IDENTITY
        # (cross-host-shaped) co-writers stay gated on 5b / the VM leg.
        log "--cowriters=$cowriters: CO-LOCATED co-writers (shared PR host identity — the ops.md honest-residual shape; device-enforced co-writer fencing rows stay 5b/VM territory)"
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
        echo "VM_COUNT='$vms'"
        echo "VM_GW='$VM_GW'"
        echo "GUEST_META_NQN='$guest_meta_nqn'"
        echo "GUEST_DATA_NQN='$guest_data_nqn'"
        echo "GUEST_META_PATH='$guest_meta_path'"
        echo "GUEST_DATA_PATH='$guest_data_path'"
        echo "MEMBERSHIP='$membership'"
        echo "MEMBERSHIP_LEASE_TTL_MS='$lease_ttl_ms'"
        echo "MW='$mw'"
        echo "MW_PORT='${SQZ_MWFLEET_MW_PORT:-45999}'"
        echo "COWRITERS='$cowriters'"
    } >"$CONF"
    : >"$VMS"

    mount_member 0

    # Rung 7: persist the owner's ADVERTISED endpoint (what every member —
    # host, netns'd, or guest — dials), parsed from the arm line the owner
    # logs; the netns/guest venues preflight reachability against it.
    if [ -n "$membership" ]; then
        local memb_ep
        memb_ep="$(grep -o "membership OWNER armed on [^ ]*" "$STATE/m0.log" | head -1 | awk '{print $NF}')"
        [ -n "$memb_ep" ] || die "membership armed but the owner log carries no 'membership OWNER armed on' line"
        echo "MEMBERSHIP_ENDPOINT='$memb_ep'" >>"$CONF"
        log "membership owner endpoint: $memb_ep (lease TTL ${lease_ttl_ms:-45000 (shipped)} ms)"
    fi

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

    # Rung 9: the co-writer bring-up — the ops.md "Bringing one up" flow,
    # mechanized. Phase 1 harvests each mountpoint's durable enrollment id
    # from its rung-3 refusal; phase 2 re-arms the AUTHORITY with the
    # roster (enrollment is the authority's durable act — a new era);
    # phase 3 mounts the admitted co-writers.
    if [ "$cowriters" -gt 0 ]; then
        record_mw_endpoint
        local roster="" cid cw_idx
        for ((idx = 0; idx < cowriters; idx++)); do
            cw_idx=$((COWRITER_BASE + idx))
            cid="$(probe_cowriter_id "$cw_idx")"
            log "co-writer $cw_idx enrollment id harvested: $cid"
            roster="${roster:+$roster,}$cid"
        done
        echo "MW_ROSTER='$roster'" >>"$CONF"
        MW_ROSTER="$roster"
        export MW_ROSTER
        log "re-arming the authority with the roster (a new era): $roster"
        unmount_member 0
        mount_member 0
        record_mw_endpoint
        for ((idx = 0; idx < cowriters; idx++)); do
            mount_member $((COWRITER_BASE + idx))
        done
    fi

    # Rung-6b guests LAST (they dial the target the fleet already rides;
    # they hold no member role this rung — the in-guest legs drive them).
    for ((idx = 0; idx < vms; idx++)); do
        vm_boot "$idx"
    done
    log "fleet up: 1 writer + $((n - 1)) reader(s) + $cowriters co-writer(s) + $vms guest(s), SQUEEZEFS_FLEET_SHARE=$n per daemon"
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
    if [ -s "$VMS" ]; then
        printf '%-4s %-7s %-6s %-6s %-10s %s\n' VM ACCEL PID LIVE HOSTSCOPE DIR
        local vidx vpid vaccel vhs vdir vlive
        while IFS=$'\t' read -r vidx vpid vaccel vhs vdir; do
            vlive="dead"
            kill -0 "$vpid" 2>/dev/null && vlive="up"
            printf '%-4s %-7s %-6s %-6s %-10s %s\n' "$vidx" "$vaccel" "$vpid" "$vlive" "$vhs" "$vdir"
        done <"$VMS"
    fi
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
    # Guests FIRST (leaf consumers of the target; their fabric
    # connections drop with the qemu process).
    if [ -f "$VMS" ]; then
        local vidx
        while IFS=$'\t' read -r vidx _; do
            [ -n "$vidx" ] && vm_stop "$vidx" || true
        done < <(cat "$VMS")
    fi
    # Sweep guests the ledger does not know (partial vm-boot residue):
    # any qemu whose cmdline names this fleet's state dir is ours.
    local qpid
    for qpid in $(pgrep -f "qemu-system-x86_64.*$STATE/vm" || true); do
        kill -9 "$qpid" 2>/dev/null || true
        log "swept unledgered guest pid $qpid"
    done
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
    # Rung 7: sweep every fleet netns (the veth pair dies with it).
    local nsn
    for nsn in $(ip netns list 2>/dev/null | awk '{print $1}' | grep "^sqzmw-$INSTANCE-m" || true); do
        ip netns del "$nsn" 2>/dev/null || true
        log "swept netns $nsn"
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
    if pgrep -f "qemu-system-x86_64.*$STATE/vm" >/dev/null 2>&1; then
        warn "RESIDUE: qemu guest process(es) for this fleet survived"
        rc=1
    fi
    if ip netns list 2>/dev/null | awk '{print $1}' | grep -q "^sqzmw-$INSTANCE-m"; then
        warn "RESIDUE: fleet netns survived"
        rc=1
    fi
    if ip -o link show 2>/dev/null | awk -F': ' '{print $2}' | grep -q "^sqzmw[0-9]*h"; then
        warn "RESIDUE: fleet veth survived"
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
mount) mount_member "${1:?mount needs a member index}" "${2:-}" ;;
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
vm-boot)
    require_state
    vm_boot "${1:?vm-boot needs a guest index}" "${2:-}"
    ;;
vm-exec)
    require_state
    vm_exec "${1:?vm-exec needs a guest index}" "${2:--}" "${3:-300}"
    ;;
vm-stop)
    require_state
    vm_stop "${1:?vm-stop needs a guest index}"
    ;;
pause)
    require_state
    vm_pause "${1:?pause needs a guest index}"
    ;;
resume)
    require_state
    vm_resume "${1:?resume needs a guest index}"
    ;;
netem)
    require_state
    netem_set "${1:?netem needs a member index}" "${2:?netem needs a delay in ms, or off}"
    ;;
partition)
    require_state
    partition_set "${1:?partition needs a member index}" "${2:?partition needs on|off}"
    ;;
*) die "unknown verb '$VERB' (create|status|mount|unmount|kill|netem|partition|probe-host-scoped|vm-boot|vm-exec|vm-stop|pause|resume|teardown)" ;;
esac
