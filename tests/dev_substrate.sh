#!/usr/bin/env bash
# tests/dev_substrate.sh — virtual NVMe / memory dev substrate for SqueezeFS
# =========================================================================
#
# Creates (and tears down) the repo's preferred pseudo-everything dev-box
# substrate on machines with no spare raw NVMe: RAM block devices exposed as
# real /dev/nvmeXnY namespaces through kernel NVMe-oF **loop** targets — or,
# with SQZ_DEVSUB_TRANSPORT=tcp, through **nvmet-tcp on localhost**.
#
#     metadata (mds):  memory-backed null_blk ──┐
#                                               ├── nvmet subsystem ── nvme connect -t {loop|tcp} ── /dev/nvmeXnY
#     data     (oss):  zram (compressed RAM) ───┘
#
# Why this shape (measured, .benchmarks/2026-07-14-metadata-throughput-baseline.md):
#   file-backed volumes on btrfs distort every barrier-bound metadata number
#   (fdatasync p50 ~495 µs vs ~3 µs on memory-backed null_blk — 165×; strict
#   cadence collapses 3.8–8.9× on btrfs files). nvmet-loop over RAM block
#   devices runs the full kernel NVMe target/host stack (~9 µs barrier, FUA +
#   write-back cache, NVMe Persistent Reservations for the single-writer
#   guard) — the closest local analog to the NVMe-oF production path.
#
# The TWO-SUBSTRATE RULE (2026-07-27 amplification campaign,
# .benchmarks/2026-07-27-shim-write-amplification.md; methodology pinned in
# AGENTS.md → Benchmarks & Profiling):
#   * loop mode (default) — controlled-latency A/B: no network stack, the
#     lowest-noise venue for per-op decomposition and barrier-bound work.
#   * tcp mode — MANDATORY for fabric-sensitive rows (writes, bandwidth-
#     bound shapes, multi-connection): nvmet-tcp on 127.0.0.1 runs the real
#     NVMe/TCP queue/softirq machinery, so bandwidth-economy effects (write
#     amplification, request-size collapse, per-connection contention) that
#     the loop rig HIDES become measurable. Same backings (null_blk mds +
#     zram oss), same verbs; the two modes coexist (disjoint names, state
#     dirs, ports).
#   tcp mode owns the TCP service-port slice **54100–54199** (default
#   trsvcid 54129) — deliberately outside the NVMe-oF fidelity tier's
#   54000–54099 slice (tests/nvmeof_target_substrate.sh) so both rigs can
#   run on one box.
#
# Verbs
#   create        build the substrate (idempotent: healthy ⇒ status + exit 0;
#                 stale/partial state is cleaned and rebuilt)
#   teardown      remove ONLY the objects recorded in the state dir
#                 (idempotent; re-run after a partial teardown completes it)
#   status        table of devices, backing, sizes, usage + a format/mount hint
#   recreate      teardown + create
#   systemd-unit  emit an optional systemd unit to stdout (NOT installed)
#
# Env knobs (defaults sized for a >= 64 GiB dev box; this repo's box: 109 GiB)
#   SQZ_DEVSUB_TRANSPORT=loop     nvmet transport: loop (default) or tcp
#                                 (localhost NVMe/TCP; see the two-substrate
#                                 rule above). tcp mode namespaces everything
#                                 apart: NQNs devsubtcp-*, null_blk items
#                                 sqzdevsubtcp_*, state dir
#                                 /run/squeezefs-devsub-tcp, port id 52027
#   SQZ_DEVSUB_TCP_ADDR=127.0.0.1 tcp mode: target listen address
#   SQZ_DEVSUB_TCP_SVC=54129      tcp mode: NVMe/TCP service port — keep it
#                                 inside the devsub-tcp slice 54100–54199
#                                 (54000–54099 belongs to the fidelity tier)
#   SQZ_DEVSUB_MDS_COUNT=4        metadata namespaces (memory-backed null_blk)
#   SQZ_DEVSUB_MDS_GB=1           GiB per metadata device (RAM, allocated on write)
#   SQZ_DEVSUB_MDS_CACHE_MB=256   null_blk write-back cache MiB (>0 ⇒ real
#                                 FLUSH/FUA semantics: fua=1, write_cache=write back)
#   SQZ_DEVSUB_OSS_COUNT=4        data namespaces (zram)
#   SQZ_DEVSUB_OSS_GB=8           zram disksize GiB per data device (VIRTUAL —
#                                 RAM used ~= compressed working set only)
#   SQZ_DEVSUB_OSS_ALGO=zstd      zram compression algorithm
#   SQZ_DEVSUB_OSS_MEM_LIMIT_GB=0 hard RAM cap per zram device (0 = uncapped;
#                                 capped devices return EIO past the limit)
#   SQZ_DEVSUB_NVME_IO_QUEUES=4   nvme connect -i (bounded: per-CPU queue counts
#                                 hit blk_mq EXDEV on boxes with offlined CPUs)
#   SQZ_DEVSUB_PORT_ID=52026      nvmet configfs port id (52027 in tcp mode) —
#                                 ports carry no name, so ownership rides this
#                                 well-known id; a foreign port squatting it
#                                 makes create fail loud (override the knob),
#                                 and the id is only ever removed when its
#                                 links are all devsub-prefixed
#   SQZ_DEVSUB_STATE_DIR=/run/squeezefs-devsub   ownership manifest (tmpfs —
#                                 cleared on reboot, matching the RAM devices;
#                                 /run/squeezefs-devsub-tcp in tcp mode)
#   SQZ_DEVSUB_FORCE=0            teardown: 1 = unmount filesystems mounted from
#                                 OUR namespaces instead of refusing
#
# RAM budget at defaults: mds 4 x (1 GiB backing + 256 MiB cache) <= 5 GiB
# worst case; oss 4 x 8 GiB VIRTUAL — resident RAM is the compressed working
# set (ceiling 32 GiB only if you fill every byte with incompressible data).
# Typical dev/bench working sets: a few GiB total.
#
# Ownership & safety policy (non-negotiable)
#   * Every object carries a unique name: NQNs nqn.2026-07.io.squeezefs:devsub-*,
#     null_blk configfs items sqzdevsub_*, plus a manifest in the state dir
#     recording exactly which nullb items, zram indexes, nvmet subsystems, the
#     nvmet port, and nvme controllers this script created.
#   * teardown removes ONLY manifest-recorded objects (plus, on stale-state
#     recovery, objects that carry our unique prefixes). Foreign nvmet
#     subsystems/ports, zram devices (e.g. zram swap), and null_blk instances
#     are NEVER touched.
#   * zram indexes come from /sys/class/zram-control/hot_add (never assumes
#     zram0 is free); null_blk instances are explicit configfs items (never
#     module-param auto-instances).
#   * Modules (null_blk, zram, nvmet, nvme-loop) are loaded on demand and
#     NEVER unloaded on teardown — other users may have their own instances,
#     and a loaded idle module is harmless. This is deliberate policy.
#   * Everything is RAM-backed and reboot-ephemeral: dev/test only, NEVER
#     production data. Reformat after every reboot (or install the systemd
#     unit so create runs at boot).
#
# Requires: root (re-execs via sudo), Linux with configfs + null_blk + zram +
# nvmet + nvme-loop, nvme-cli.
#
# Examples
#   tests/dev_substrate.sh create
#   SQZ_DEVSUB_TRANSPORT=tcp tests/dev_substrate.sh create
#   SQZ_DEVSUB_OSS_GB=16 tests/dev_substrate.sh recreate
#   tests/dev_substrate.sh status
#   SQZ_DEVSUB_TRANSPORT=tcp tests/dev_substrate.sh teardown
#   tests/dev_substrate.sh systemd-unit > /etc/systemd/system/squeezefs-devsub.service

set -euo pipefail

TRANSPORT="${SQZ_DEVSUB_TRANSPORT:-loop}"
case "$TRANSPORT" in
loop | tcp) ;;
*)
    echo "[devsub] ERROR: SQZ_DEVSUB_TRANSPORT must be 'loop' or 'tcp' (got '$TRANSPORT')" >&2
    exit 1
    ;;
esac
TCP_ADDR="${SQZ_DEVSUB_TCP_ADDR:-127.0.0.1}"
TCP_SVC="${SQZ_DEVSUB_TCP_SVC:-54129}"
# Loop-mode port address: nvmet loop ports accept a free-form traddr, and
# `nvme connect -t loop -a` selects by it — the only way to name OUR port
# on a box that also carries foreign loop rigs (see create_port).
LOOP_TRADDR="sqzdevsub"

# Every ownership handle is transport-scoped and DISJOINT (prefix globs must
# not overlap: a loop-mode stale-state sweep must never claim tcp-mode
# objects, and vice versa), so both substrates can coexist on one box.
if [ "$TRANSPORT" = "tcp" ]; then
    STATE_DIR="${SQZ_DEVSUB_STATE_DIR:-/run/squeezefs-devsub-tcp}"
    PORT_ID="${SQZ_DEVSUB_PORT_ID:-52027}"
    NQN_PREFIX="nqn.2026-07.io.squeezefs:devsubtcp-"
    NULLB_PREFIX="sqzdevsubtcp_"
else
    STATE_DIR="${SQZ_DEVSUB_STATE_DIR:-/run/squeezefs-devsub}"
    PORT_ID="${SQZ_DEVSUB_PORT_ID:-52026}"
    NQN_PREFIX="nqn.2026-07.io.squeezefs:devsub-"
    NULLB_PREFIX="sqzdevsub_"
fi
MDS_COUNT="${SQZ_DEVSUB_MDS_COUNT:-4}"
MDS_GB="${SQZ_DEVSUB_MDS_GB:-1}"
MDS_CACHE_MB="${SQZ_DEVSUB_MDS_CACHE_MB:-256}"
OSS_COUNT="${SQZ_DEVSUB_OSS_COUNT:-4}"
OSS_GB="${SQZ_DEVSUB_OSS_GB:-8}"
OSS_ALGO="${SQZ_DEVSUB_OSS_ALGO:-zstd}"
OSS_MEM_LIMIT_GB="${SQZ_DEVSUB_OSS_MEM_LIMIT_GB:-0}"
IO_QUEUES="${SQZ_DEVSUB_NVME_IO_QUEUES:-4}"
FORCE="${SQZ_DEVSUB_FORCE:-0}"
NVMET_CFS="/sys/kernel/config/nvmet"
NULLB_CFS="/sys/kernel/config/nullb"
MANIFEST="$STATE_DIR/manifest.tsv" # role idx nqn kind backing_id backing_dev
PORT_FILE="$STATE_DIR/port.id"

log() { echo "[devsub] $*"; }
warn() { echo "[devsub] WARN: $*" >&2; }
die() {
    echo "[devsub] ERROR: $*" >&2
    exit 1
}

usage() {
    # Print the contiguous comment header (everything after the shebang).
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
}

# ---------------------------------------------------------------------------
# root re-exec (preserve SQZ_DEVSUB_* knobs across sudo)
# ---------------------------------------------------------------------------
ensure_root() {
    [ "$(id -u)" -eq 0 ] && return 0
    log "root required (configfs, modules, /dev plumbing) — re-executing via sudo"
    local knobs=()
    while IFS= read -r kv; do knobs+=("$kv"); done \
        < <(env | grep '^SQZ_DEVSUB_' || true)
    exec sudo env "${knobs[@]}" bash "$0" "$@"
}

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------
require_int() { # name value
    [[ "$2" =~ ^[0-9]+$ ]] || die "$1 must be a non-negative integer (got '$2')"
}

ensure_prereqs() {
    command -v nvme >/dev/null || die "nvme-cli not installed (need 'nvme connect -t loop')"
    mountpoint -q /sys/kernel/config ||
        mount -t configfs none /sys/kernel/config ||
        die "cannot mount configfs at /sys/kernel/config"
    # Loaded on demand; NEVER unloaded on teardown (see policy header).
    modprobe null_blk 2>/dev/null || true
    modprobe zram 2>/dev/null || true
    modprobe nvmet 2>/dev/null || true
    if [ "$TRANSPORT" = "tcp" ]; then
        modprobe nvmet_tcp 2>/dev/null || true
        modprobe nvme_tcp 2>/dev/null || true
    else
        modprobe nvme_loop 2>/dev/null || true
    fi
    [ -d "$NULLB_CFS" ] || die "null_blk configfs missing ($NULLB_CFS) — kernel lacks CONFIG_BLK_DEV_NULL_BLK?"
    [ -d "$NVMET_CFS" ] || die "nvmet configfs missing ($NVMET_CFS) — kernel lacks nvmet?"
    [ -e /sys/class/zram-control/hot_add ] || die "zram hot_add missing — kernel lacks zram?"
}

mds_nqn() { echo "${NQN_PREFIX}mds$1"; }
oss_nqn() { echo "${NQN_PREFIX}oss$1"; }

# Resolve the block-device name (nvmeXnY) serving <nqn>, multipath or not.
resolve_ns_dev() { # nqn -> echoes bare device name; rc 1 if absent
    local nqn="$1" d n base
    for d in /sys/class/nvme-subsystem/nvme-subsys* /sys/class/nvme/nvme*; do
        [ -r "$d/subsysnqn" ] || continue
        [ "$(cat "$d/subsysnqn" 2>/dev/null)" = "$nqn" ] || continue
        for n in "$d"/nvme*; do
            base="$(basename "$n")"
            [[ "$base" =~ ^nvme[0-9]+(c[0-9]+)?n[0-9]+$ ]] || continue
            [ -b "/dev/$base" ] || continue
            echo "$base"
            return 0
        done
    done
    return 1
}

resolve_ctrl() { # nqn -> echoes controller instance(s) (nvmeX), space-joined
    local nqn="$1" d out=""
    for d in /sys/class/nvme/nvme*; do
        [ -r "$d/subsysnqn" ] || continue
        [ "$(cat "$d/subsysnqn" 2>/dev/null)" = "$nqn" ] || continue
        out="$out$(basename "$d") "
    done
    [ -n "$out" ] || return 1
    echo "${out% }"
}

wait_for() { # description max_tries cmd...
    local what="$1" tries="$2" i
    shift 2
    for ((i = 0; i < tries; i++)); do
        "$@" >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    die "timed out waiting for $what"
}

# ---------------------------------------------------------------------------
# backing devices
# ---------------------------------------------------------------------------
create_nullb() { # name gb cache_mb -> echoes /dev path
    local name="$1" gb="$2" cache_mb="$3" d="$NULLB_CFS/$1" dev
    [ -d "$d" ] && die "null_blk item $name already exists (stale? run teardown)"
    mkdir "$d"
    echo 4096 >"$d/blocksize"
    echo $((gb * 1024)) >"$d/size" # MiB
    echo 1 >"$d/memory_backed"
    echo "$cache_mb" >"$d/cache_size" # >0 => write-back cache + FLUSH/FUA
    echo 0 >"$d/completion_nsec"
    echo 0 >"$d/irqmode"
    echo 1 >"$d/power"
    # Modern kernels name the disk after the configfs item; older ones use
    # nullb<index>.
    if [ -b "/dev/$name" ]; then
        dev="/dev/$name"
    else
        dev="/dev/nullb$(cat "$d/index")"
    fi
    wait_for "$dev" 50 test -b "$dev"
    echo "$dev"
}

create_zram() { # -> echoes a freshly allocated zram index; the device is
    #              configured only AFTER the manifest line is written, so a
    #              mid-create crash can never leave an unrecorded zram behind
    cat /sys/class/zram-control/hot_add # never assumes zram0 is free
}

configure_zram() { # idx gb algo mem_limit_gb
    local idx="$1" gb="$2" algo="$3" lim="$4" b="/sys/block/zram$1"
    wait_for "/dev/zram$idx" 50 test -b "/dev/zram$idx"
    if ! echo "$algo" >"$b/comp_algorithm" 2>/dev/null; then
        die "zram algorithm '$algo' unavailable (have: $(cat "$b/comp_algorithm"))"
    fi
    echo "${gb}G" >"$b/disksize"
    [ "$lim" != "0" ] && echo "${lim}G" >"$b/mem_limit"
    return 0
}

# ---------------------------------------------------------------------------
# nvmet plumbing
# ---------------------------------------------------------------------------
# The port id is our ownership handle (ports carry no name). If the id exists
# it must be a devsub leftover: OUR mode's transport (+ our listen address in
# tcp mode) and no foreign subsystem links.
port_is_ours() { # id -> 0 if the existing port can only be ours
    local p="$NVMET_CFS/ports/$1" l nqn
    [ "$(cat "$p/addr_trtype" 2>/dev/null)" = "$TRANSPORT" ] || return 1
    if [ "$TRANSPORT" = "tcp" ]; then
        [ "$(cat "$p/addr_traddr" 2>/dev/null)" = "$TCP_ADDR" ] || return 1
    fi
    for l in "$p"/subsystems/*; do
        [ -L "$l" ] || continue
        nqn="$(basename "$l")"
        case "$nqn" in
        "$NQN_PREFIX"*) ;;
        *) return 1 ;;
        esac
    done
    return 0
}

create_port() { # id
    local p="$NVMET_CFS/ports/$1"
    if [ -d "$p" ]; then
        port_is_ours "$1" ||
            die "nvmet port $1 exists and is NOT ours (foreign transport/links) — set SQZ_DEVSUB_PORT_ID to a free id"
        if [ "$TRANSPORT" = "loop" ] &&
            [ "$(cat "$p/addr_traddr" 2>/dev/null)" != "$LOOP_TRADDR" ]; then
            # A pre-traddr devsub leftover: the addr attrs are write-locked
            # once linked, and a bare-addressed loop port cannot be named
            # by `connect -a` — recreate it (link-free by port_is_ours +
            # the sweep, so this can only rebuild OUR stale port).
            if [ -z "$(ls -A "$p/subsystems" 2>/dev/null)" ]; then
                warn "recreating legacy devsub loop port $1 (no traddr stamp)"
                rmdir "$p"
            else
                die "devsub loop port $1 predates traddr stamping and still has links — run teardown first"
            fi
        else
            warn "adopting existing devsub $TRANSPORT port $1 (stale from a previous run)"
            return 0
        fi
    fi
    mkdir "$p"
    if [ "$TRANSPORT" = "tcp" ]; then
        echo ipv4 >"$p/addr_adrfam"
        echo "$TCP_ADDR" >"$p/addr_traddr"
        echo "$TCP_SVC" >"$p/addr_trsvcid"
        echo tcp >"$p/addr_trtype"
    else
        echo loop >"$p/addr_trtype"
        # Loop-port disambiguation (2026-07-27): a bare `nvme connect -t
        # loop` binds the FIRST registered loop port, so a box carrying a
        # foreign loop rig (e.g. the fuse-per-op campaign's sqzlat port)
        # rejects our subsystems with "connect request for invalid
        # subsystem". Stamp our traddr and connect with `-a` (must be set
        # BEFORE any subsystem link — the attr is write-locked after).
        echo "$LOOP_TRADDR" >"$p/addr_traddr"
    fi
}

create_subsys() { # nqn backing_dev port_id
    local nqn="$1" dev="$2" port="$3" s="$NVMET_CFS/subsystems/$1"
    mkdir "$s"
    echo 1 >"$s/attr_allow_any_host"
    mkdir "$s/namespaces/1"
    echo -n "$dev" >"$s/namespaces/1/device_path"
    # RAM-backed namespaces need a stamped UUID (duplicate/absent NGUID makes
    # the host connect fail) — .benchmarks/2026-07-14 baseline repro note.
    cat /proc/sys/kernel/random/uuid >"$s/namespaces/1/device_uuid"
    # Enable NVMe Persistent Reservations where the kernel offers the knob
    # (must be set before enable) so the single-writer mount guard lands
    # enforcement-grade on this substrate, like production NVMe-oF.
    if [ -f "$s/namespaces/1/resv_enable" ]; then
        echo 1 >"$s/namespaces/1/resv_enable"
    fi
    echo 1 >"$s/namespaces/1/enable"
    ln -s "$s" "$NVMET_CFS/ports/$port/subsystems/$nqn"
}

connect_subsys() { # nqn -> echoes /dev path of the namespace
    local nqn="$1" name
    if [ "$TRANSPORT" = "tcp" ]; then
        nvme connect -t tcp -a "$TCP_ADDR" -s "$TCP_SVC" -n "$nqn" -i "$IO_QUEUES" >/dev/null
    else
        # -a names OUR loop port (see create_port) — never the box's
        # first registered loop port.
        nvme connect -t loop -a "$LOOP_TRADDR" -n "$nqn" -i "$IO_QUEUES" >/dev/null
    fi
    wait_for "namespace of $nqn" 100 resolve_ns_dev "$nqn"
    name="$(resolve_ns_dev "$nqn")"
    wait_for "/dev/$name" 50 test -b "/dev/$name"
    echo "/dev/$name"
}

# ---------------------------------------------------------------------------
# manifest / status helpers
# ---------------------------------------------------------------------------
manifest_devs() { # role -> live namespace /dev paths for recorded rows, in order
    local role="$1" r nqn name _
    [ -f "$MANIFEST" ] || return 0
    while IFS=$'\t' read -r r _ nqn _ _ _; do
        [ "$r" = "$role" ] || continue
        if name="$(resolve_ns_dev "$nqn")"; then
            echo "/dev/$name"
        fi
    done <"$MANIFEST"
}

substrate_healthy() {
    [ -s "$MANIFEST" ] || return 1
    local nqn bdev _
    while IFS=$'\t' read -r _ _ nqn _ _ bdev; do
        [ -b "$bdev" ] || return 1
        resolve_ns_dev "$nqn" >/dev/null || return 1
    done <"$MANIFEST"
    return 0
}

join_commas() {
    local IFS=,
    echo "$*"
}

print_examples() { # full format/mount example block against live namespaces
    local mds oss
    mapfile -t mds < <(manifest_devs mds)
    mapfile -t oss < <(manifest_devs oss)
    [ "${#mds[@]}" -gt 0 ] && [ "${#oss[@]}" -gt 0 ] || return 0
    cat <<EOF

Format + mount against the substrate (QUICKSTART conventions; volumes are
RAM-backed and reboot-ephemeral — reformat after every reboot):

  sudo ./target/release/squeezefs format \\
    "sqmeta://$(join_commas "${mds[@]}")" \\
    "sqdata://$(join_commas "${oss[@]}")"

  sudo mkdir -p /mnt/squeezefs
  sudo ./target/release/squeezefs mount \\
    "sqmeta://$(join_commas "${mds[@]}")" \\
    /mnt/squeezefs --daemon --allow-others \\
    --log-file /tmp/squeezefs.log

(add --disk-cache-paths <dir> at format time if you want the staged layout;
for an all-RAM substrate use a tmpfs dir, e.g. $STATE_DIR/staging)
EOF
}

cmd_status() {
    if [ ! -s "$MANIFEST" ]; then
        log "no $TRANSPORT substrate (state dir $STATE_DIR empty or missing) — run:${SQZ_DEVSUB_TRANSPORT:+ SQZ_DEVSUB_TRANSPORT=$TRANSPORT} $0 create"
        return 0
    fi
    log "transport: $TRANSPORT (state $STATE_DIR)"
    local r nqn kind bdev name dev ctrl size inuse rows mds oss _
    rows="ROLE\tNQN\tBACKING\tSIZE\tCTRL\tNAMESPACE\tIN-USE-BY\n"
    while IFS=$'\t' read -r r _ nqn kind _ bdev; do
        if name="$(resolve_ns_dev "$nqn")"; then
            dev="/dev/$name"
            ctrl="$(resolve_ctrl "$nqn" 2>/dev/null || echo '-')"
            size="$(lsblk -bdno SIZE "$dev" 2>/dev/null | awk '{printf "%.1fG", $1/1024/1024/1024}')" || size="-"
            # findmnt exits 1 when the device has no mounts — not an error here
            inuse="$(findmnt -rn -o TARGET -S "$dev" 2>/dev/null | paste -sd, -)" || inuse=""
            if [ -z "$inuse" ] && command -v fuser >/dev/null; then
                inuse="$(fuser "$dev" 2>/dev/null | awk '{$1=$1; print "pids:" $0}')" || true
            fi
            [ -n "$inuse" ] || inuse="-"
        else
            dev="MISSING" ctrl="-" size="-" inuse="-"
        fi
        rows+="$r\t$nqn\t$bdev ($kind)\t$size\t$ctrl\t$dev\t$inuse\n"
    done <"$MANIFEST"
    printf '%b' "$rows" | column -t -s $'\t'
    substrate_healthy || warn "substrate is DEGRADED (missing devices — likely post-reboot stale state); 'create' will clean and rebuild"
    mapfile -t mds < <(manifest_devs mds)
    mapfile -t oss < <(manifest_devs oss)
    if [ "${#mds[@]}" -gt 0 ] && [ "${#oss[@]}" -gt 0 ]; then
        log "hint: squeezefs format \"sqmeta://$(join_commas "${mds[@]}")\" \"sqdata://$(join_commas "${oss[@]}")\" && squeezefs mount \"sqmeta://$(join_commas "${mds[@]}")\" /mnt/squeezefs --daemon"
    fi
}

# ---------------------------------------------------------------------------
# teardown
# ---------------------------------------------------------------------------
refuse_if_in_use() {
    local nqn name dev tgt mounted=() users=() _
    while IFS=$'\t' read -r _ _ nqn _ _ _; do
        name="$(resolve_ns_dev "$nqn")" || continue
        dev="/dev/$name"
        while IFS= read -r tgt; do
            [ -n "$tgt" ] && mounted+=("$dev -> $tgt")
        done < <(findmnt -rn -o TARGET -S "$dev" 2>/dev/null || true)
        if command -v fuser >/dev/null && fuser -s "$dev" 2>/dev/null; then
            local pids
            pids="$(fuser "$dev" 2>/dev/null | tr -s ' ')" || pids="?"
            users+=("$dev (pids:$pids)")
        fi
    done <"$MANIFEST"
    if [ "${#mounted[@]}" -gt 0 ]; then
        if [ "$FORCE" = "1" ]; then
            local m
            for m in "${mounted[@]}"; do
                warn "SQZ_DEVSUB_FORCE=1 — unmounting ${m#* -> } (on our namespace ${m%% *})"
                umount "${m#* -> }" || die "umount ${m#* -> } failed"
            done
        else
            printf '[devsub] ERROR: filesystems are mounted from substrate namespaces:\n' >&2
            printf '  %s\n' "${mounted[@]}" >&2
            die "unmount them first, or re-run with SQZ_DEVSUB_FORCE=1 (unmounts the above, which live on OUR devices only)"
        fi
    fi
    if [ "${#users[@]}" -gt 0 ]; then
        if [ "$FORCE" = "1" ]; then
            warn "namespaces still open by processes (proceeding under FORCE — their I/O will fail; no process is killed):"
            printf '  %s\n' "${users[@]}" >&2
        else
            printf '[devsub] ERROR: substrate namespaces are open by processes:\n' >&2
            printf '  %s\n' "${users[@]}" >&2
            die "stop them first (e.g. squeezefs umount), or re-run with SQZ_DEVSUB_FORCE=1"
        fi
    fi
}

remove_subsys() { # nqn [port_id]
    local nqn="$1" port="${2:-}" s="$NVMET_CFS/subsystems/$1" p
    # Disconnect any of OUR controllers first (by exact NQN — never foreign).
    if resolve_ctrl "$nqn" >/dev/null 2>&1; then
        nvme disconnect -n "$nqn" >/dev/null 2>&1 || true
    fi
    # Unlink from our recorded port, and (stale-state sweep) from any port
    # whose link points at OUR subsystem path.
    for p in "$NVMET_CFS"/ports/*/subsystems/"$nqn"; do
        [ -L "$p" ] || continue
        if [ -n "$port" ] && [ "$p" != "$NVMET_CFS/ports/$port/subsystems/$nqn" ]; then
            warn "unlinking $nqn from unexpected port $(basename "$(dirname "$(dirname "$p")")") (link targets our subsystem)"
        fi
        rm -f "$p"
    done
    [ -d "$s" ] || return 0
    if [ -d "$s/namespaces/1" ]; then
        echo 0 >"$s/namespaces/1/enable" 2>/dev/null || true
        rmdir "$s/namespaces/1"
    fi
    rmdir "$s"
}

remove_zram() { # idx
    local idx="$1" b="/sys/block/zram$1"
    [ -d "$b" ] || return 0
    echo 1 >"$b/reset" 2>/dev/null || true
    echo "$idx" >/sys/class/zram-control/hot_remove
}

remove_nullb() { # name
    local d="$NULLB_CFS/$1"
    [ -d "$d" ] || return 0
    echo 0 >"$d/power" 2>/dev/null || true
    rmdir "$d"
}

teardown_from_state() { # tolerant: absent objects are treated as already gone
    local port=""
    [ -f "$PORT_FILE" ] && port="$(cat "$PORT_FILE")"
    if [ -s "$MANIFEST" ]; then
        refuse_if_in_use
        local nqn kind bid bdev _
        while IFS=$'\t' read -r _ _ nqn _ _ _; do
            remove_subsys "$nqn" "$port"
        done <"$MANIFEST"
        while IFS=$'\t' read -r _ _ nqn kind bid bdev; do
            case "$kind" in
            nullb) remove_nullb "$bid" ;;
            zram) remove_zram "$bid" ;;
            *) warn "unknown backing kind '$kind' for $nqn — leaving $bdev alone" ;;
            esac
        done <"$MANIFEST"
    fi
    # Remove the port only if WE recorded it and no subsystems remain linked
    # (a foreign harness must never lose its port).
    if [ -n "$port" ] && [ -d "$NVMET_CFS/ports/$port" ]; then
        if [ -z "$(ls -A "$NVMET_CFS/ports/$port/subsystems" 2>/dev/null)" ]; then
            rmdir "$NVMET_CFS/ports/$port"
        else
            warn "port $port still has linked subsystems (foreign?) — leaving it in place"
        fi
    fi
    rm -rf "$STATE_DIR"
    # Policy: modules stay loaded (foreign instances may exist; idle modules
    # are harmless).
}

# Stale-state sweep: objects that carry OUR unique prefixes but predate the
# (lost/stale) manifest are ours by construction — clean them so create is
# self-healing. zram devices carry no name, but an orphaned devsub subsystem's
# namespace device_path records which zram index WE put behind it — that
# subsystem-attested index is reclaimed too. (The only leak window left is a
# crash between zram hot_add and the manifest append — one shell line — and a
# reboot clears it anyway.) Foreign zram devices are never touched.
sweep_prefixed_orphans() {
    local d nqn name bdev
    for d in "$NVMET_CFS"/subsystems/"$NQN_PREFIX"*; do
        [ -d "$d" ] || continue
        nqn="$(basename "$d")"
        bdev="$(cat "$d/namespaces/1/device_path" 2>/dev/null || true)"
        warn "stale-state sweep: removing orphaned subsystem $nqn"
        remove_subsys "$nqn" ""
        if [[ "$bdev" =~ ^/dev/zram([0-9]+)$ ]]; then
            warn "stale-state sweep: reclaiming zram${BASH_REMATCH[1]} (was behind $nqn)"
            remove_zram "${BASH_REMATCH[1]}"
        fi
    done
    for d in "$NULLB_CFS/$NULLB_PREFIX"*; do
        [ -d "$d" ] || continue
        name="$(basename "$d")"
        warn "stale-state sweep: removing orphaned null_blk $name"
        remove_nullb "$name"
    done
    # Our conventional port id, if now link-free, is a devsub leftover too.
    if [ -d "$NVMET_CFS/ports/$PORT_ID" ] && port_is_ours "$PORT_ID" &&
        [ -z "$(ls -A "$NVMET_CFS/ports/$PORT_ID/subsystems" 2>/dev/null)" ]; then
        warn "stale-state sweep: removing orphaned devsub port $PORT_ID"
        rmdir "$NVMET_CFS/ports/$PORT_ID"
    fi
}

cmd_teardown() {
    if [ ! -d "$STATE_DIR" ]; then
        log "nothing to tear down (no state dir $STATE_DIR)"
        sweep_prefixed_orphans
        return 0
    fi
    teardown_from_state
    sweep_prefixed_orphans
    log "teardown complete (modules left loaded by policy)"
}

# ---------------------------------------------------------------------------
# create
# ---------------------------------------------------------------------------
cmd_create() {
    require_int SQZ_DEVSUB_MDS_COUNT "$MDS_COUNT"
    require_int SQZ_DEVSUB_MDS_GB "$MDS_GB"
    require_int SQZ_DEVSUB_MDS_CACHE_MB "$MDS_CACHE_MB"
    require_int SQZ_DEVSUB_OSS_COUNT "$OSS_COUNT"
    require_int SQZ_DEVSUB_OSS_GB "$OSS_GB"
    require_int SQZ_DEVSUB_OSS_MEM_LIMIT_GB "$OSS_MEM_LIMIT_GB"
    require_int SQZ_DEVSUB_NVME_IO_QUEUES "$IO_QUEUES"
    require_int SQZ_DEVSUB_PORT_ID "$PORT_ID"
    if [ "$TRANSPORT" = "tcp" ]; then
        require_int SQZ_DEVSUB_TCP_SVC "$TCP_SVC"
        if [ "$TCP_SVC" -lt 54100 ] || [ "$TCP_SVC" -gt 54199 ]; then
            warn "SQZ_DEVSUB_TCP_SVC=$TCP_SVC is outside the devsub-tcp slice 54100–54199 \
(54000–54099 belongs to the NVMe-oF fidelity tier — collisions make both rigs fail confusingly)"
        fi
    fi
    [ "$MDS_COUNT" -ge 1 ] && [ "$OSS_COUNT" -ge 1 ] || die "need at least 1 mds and 1 oss device"

    ensure_prereqs

    if [ -s "$MANIFEST" ]; then
        if substrate_healthy; then
            log "substrate already exists and is healthy — nothing to do"
            cmd_status
            return 0
        fi
        warn "state dir has a stale/degraded substrate (post-reboot?) — cleaning it before recreating"
        teardown_from_state
    fi
    sweep_prefixed_orphans

    mkdir -p "$STATE_DIR"
    : >"$MANIFEST"
    trap 'warn "create FAILED part-way — recorded objects survive in '"$STATE_DIR"'; run: '"$0"' teardown"' ERR

    local port="$PORT_ID"
    create_port "$port"
    echo "$port" >"$PORT_FILE"
    if [ "$TRANSPORT" = "tcp" ]; then
        log "nvmet tcp port $port created ($TCP_ADDR:$TCP_SVC)"
    else
        log "nvmet loop port $port created"
    fi

    local i nqn bdev nsdev idx name
    for ((i = 0; i < MDS_COUNT; i++)); do
        nqn="$(mds_nqn "$i")"
        name="${NULLB_PREFIX}mds$i"
        bdev="$(create_nullb "$name" "$MDS_GB" "$MDS_CACHE_MB")"
        printf 'mds\t%s\t%s\tnullb\t%s\t%s\n' "$i" "$nqn" "$name" "$bdev" >>"$MANIFEST"
        create_subsys "$nqn" "$bdev" "$port"
        nsdev="$(connect_subsys "$nqn")"
        log "mds$i: $bdev (null_blk memory_backed=1, ${MDS_GB}G, cache ${MDS_CACHE_MB}M) -> $nqn -> $nsdev"
    done

    for ((i = 0; i < OSS_COUNT; i++)); do
        nqn="$(oss_nqn "$i")"
        idx="$(create_zram)"
        printf 'oss\t%s\t%s\tzram\t%s\t%s\n' "$i" "$nqn" "$idx" "/dev/zram$idx" >>"$MANIFEST"
        configure_zram "$idx" "$OSS_GB" "$OSS_ALGO" "$OSS_MEM_LIMIT_GB"
        create_subsys "$nqn" "/dev/zram$idx" "$port"
        nsdev="$(connect_subsys "$nqn")"
        log "oss$i: /dev/zram$idx ($OSS_ALGO, ${OSS_GB}G virtual) -> $nqn -> $nsdev"
    done

    trap - ERR
    {
        echo "created_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "transport=$TRANSPORT"
        [ "$TRANSPORT" = "tcp" ] && echo "tcp_addr=$TCP_ADDR tcp_svc=$TCP_SVC"
        echo "mds_count=$MDS_COUNT mds_gb=$MDS_GB mds_cache_mb=$MDS_CACHE_MB"
        echo "oss_count=$OSS_COUNT oss_gb=$OSS_GB oss_algo=$OSS_ALGO oss_mem_limit_gb=$OSS_MEM_LIMIT_GB"
        echo "io_queues=$IO_QUEUES"
    } >"$STATE_DIR/meta.env"

    log "substrate ready: $MDS_COUNT mds (null_blk) + $OSS_COUNT oss (zram) namespaces over nvmet-$TRANSPORT"
    log "RAM-backed and reboot-EPHEMERAL — dev/test only, never production data"
    cmd_status
    print_examples
}

cmd_systemd_unit() {
    local self
    self="$(readlink -f "$0")"
    cat <<EOF
# squeezefs-devsub.service — OPTIONAL: recreate the SqueezeFS dev substrate at
# boot (the devices are RAM-backed and vanish on reboot). NOT installed by
# this script — installing units is the operator's choice:
#
#   tests/dev_substrate.sh systemd-unit | sudo tee /etc/systemd/system/squeezefs-devsub.service
#   sudo systemctl daemon-reload
#   sudo systemctl enable --now squeezefs-devsub.service
#
[Unit]
Description=SqueezeFS dev-box virtual NVMe substrate (RAM-backed nvmet-loop)
Documentation=file://$(dirname "$(dirname "$self")")/QUICKSTART.md
After=local-fs.target systemd-modules-load.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=$self create
ExecStop=$self teardown
# Uncomment to override sizing (see the script header for all knobs):
#Environment=SQZ_DEVSUB_TRANSPORT=loop
#Environment=SQZ_DEVSUB_MDS_COUNT=4
#Environment=SQZ_DEVSUB_MDS_GB=1
#Environment=SQZ_DEVSUB_OSS_COUNT=4
#Environment=SQZ_DEVSUB_OSS_GB=8

[Install]
WantedBy=multi-user.target
EOF
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
main() {
    local verb="${1:-}"
    case "$verb" in
    create | teardown | status | recreate)
        ensure_root "$@"
        case "$verb" in
        create) cmd_create ;;
        teardown) cmd_teardown ;;
        status) cmd_status ;;
        recreate)
            cmd_teardown
            cmd_create
            ;;
        esac
        ;;
    systemd-unit) cmd_systemd_unit ;;
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
