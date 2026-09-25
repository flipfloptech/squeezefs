#!/usr/bin/env bash
#
# cloud_bench_cluster.sh — stand up, exercise, and tear down an occasional
# SqueezeFS benchmark cluster on AWS EC2 (on-demand by default; MARKET=spot opts in).
#
# ============================ OPERATOR QUICKSTART ===========================
#
# Prerequisites (on the operator box — this script makes NO cargo builds):
#   * aws cli v2, configured (`aws sts get-caller-identity` works) with EC2
#     permissions in $AWS_REGION (run-instances/create-fleet, security groups,
#     launch templates, placement groups, SSM parameter read).
#   * An EC2 key pair in the region ($KEY_NAME) + its private key file
#     ($SSH_KEY_FILE) readable by you.
#   * Spot vCPU quota for the preset ("All Standard (A, C, D, H, I, M, R, T,
#     Z) Spot Instance Requests"): i4i preset needs 96 vCPUs, i3en preset
#     needs 288 vCPUs, mw preset needs 32 vCPUs (SYMMETRIC=1: 8 x EVERY node
#     = 8 x (N_MDS + N_OSS + N_CLIENT) — S1 N_CLIENT=3 48, S2 N_CLIENT=8 88
#     at the default 1 mds + 2 oss, 136 at S2 + N_OSS=8).
#   * Pre-built artifacts in $ARTIFACT_DIR (this script DEPLOYS, it does not
#     build): `squeezefs` (linux-gnu, glibc ≤ the AMI's — use the
#     `task build:ubuntu2404` dist output; the mw preset rides the Ubuntu
#     26.04 AMI and wants `task build:ubuntu2604`), optionally
#     `libsqueezefs_il.so`, and a DYNAMIC `elbencho` binary (the pinned
#     static one cannot load the il shim; dynamic is the house rule for any
#     instrument that may run il). The mw preset's instrument is the pinned
#     ior the s11-mpiio leg builds ON the client — elbencho is optional there.
#
# Cost table (spot prices move; these are planning numbers, and the
# max-spend guard below is the real protection):
#
#   preset  instance        cluster            instance store/node        est. spot $/hr
#   ------  --------------  -----------------  -------------------------  --------------
#   i4i     i4i.4xlarge     x6 (96 vCPU)       1 x 3,750 GB Nitro NVMe    ~$3-4/hr   (the IOPS venue)
#   i3en    i3en.12xlarge   x6 (288 vCPU)      4 x 7,500 GB NVMe          ~$10-14/hr (the throughput venue)
#   mw      i4i.2xlarge     x4 (32 vCPU)       1 x 1,875 GB Nitro NVMe    ~$0.5-0.8/hr (the MW MPI-IO venue;
#                                                                          campaign ~2-3 h => ~$1.5-3 total)
#   mw+SYMMETRIC=1 N_CLIENT=n: x(N_MDS+N_OSS+n) i4i.2xlarge, ~$0.686/hr on-demand per node
#           (EVERY node bills) — at the default 1 mds + 2 oss: S1 (n=3) x6 ~$4.1/hr,
#           S2 (n=8) x11 ~$7.5/hr (n=2: x5 ~$3.4/hr, gates 2 + 3 only);
#           S2 + N_OSS=8 (the 2026-09-24 approved shape) x17 ~$11.7/hr
#
# NO burst-class (t2/t3/t3a/t4g) instances, ever: CPU-credit throttling makes
# a median a function of the credit balance (not the code under test), their
# network baseline is burst-shaped the same way, and they carry no instance
# store — every row measured on one would be INVALID under the house
# labeling discipline. The launch preflight refuses them.
#
# One-command happy path (launch -> deploy -> assemble -> bench -> teardown):
#
#   MAX_CLUSTER_HOURS=3 tests/cloud_bench_cluster.sh full
#
# Or step-by-step:
#
#   MAX_CLUSTER_HOURS=3 tests/cloud_bench_cluster.sh launch
#   tests/cloud_bench_cluster.sh deploy
#   tests/cloud_bench_cluster.sh assemble
#   tests/cloud_bench_cluster.sh bench       # results -> .benchmarks/cloud/<ts>/
#   tests/cloud_bench_cluster.sh status
#   tests/cloud_bench_cluster.sh teardown
#
# The MULTI-WRITER MPI-IO arm (the cheapest adequate venue for the
# shared-vs-disjoint s11-mpiio ior row over a REAL nvme-tcp network — see
# docs/field-mpiio-runbook.md §Cloud venue). Pass PRESET=mw (or --preset mw)
# on EVERY subcommand: the preset selects the role shape, the AMI and the
# artifact defaults, and only the node list persists in cluster state:
#
#   MAX_CLUSTER_HOURS=3 PRESET=mw tests/cloud_bench_cluster.sh launch
#   PRESET=mw tests/cloud_bench_cluster.sh deploy       # needs dist/ubuntu2604
#   PRESET=mw tests/cloud_bench_cluster.sh assemble-mw
#   PRESET=mw tests/cloud_bench_cluster.sh bench-mw     # rows -> .benchmarks/cloud/<ts>/
#   tests/cloud_bench_cluster.sh teardown
#
# The SYMMETRIC fleet shape (design-symmetric-metadata §8 gates 2 / 3 / 3b
# on REAL nodes — PR 15, the program's only multi-node venue): PRESET=mw
# SYMMETRIC=1 (or --symmetric) makes N_CLIENT the WRITER NODE COUNT — one
# symmetric writer per client node (the MANAGER on client0 = the D0 winner,
# a JOINED writer on every other client node through the join ladder), the
# storage nodes as today. Pass PRESET=mw SYMMETRIC=1 N_CLIENT=<n> on EVERY
# subcommand:
#
#   MAX_CLUSTER_HOURS=3 PRESET=mw SYMMETRIC=1 N_CLIENT=2 tests/cloud_bench_cluster.sh launch
#   PRESET=mw SYMMETRIC=1 N_CLIENT=2 tests/cloud_bench_cluster.sh deploy
#   PRESET=mw SYMMETRIC=1 N_CLIENT=2 tests/cloud_bench_cluster.sh assemble-sym
#   PRESET=mw SYMMETRIC=1 N_CLIENT=2 tests/cloud_bench_cluster.sh bench-sym  # rows -> .benchmarks/cloud/<ts>/sym-rows/
#   tests/cloud_bench_cluster.sh teardown
#
#   shape S1 = N_CLIENT=3 (6 x i4i.2xlarge: gates 2 + 3b + gate 3 at N <= 3;
#              gate 3b needs >= 2 JOINED writers beside the manager, so
#              N_CLIENT=2 — 5 nodes — runs gates 2 + 3 only: SYM_ROWS=tarx,scale)
#   shape S2 = N_CLIENT=8 (11 x i4i.2xlarge: gate 3's full N = 1/2/4/8 ladder)
#   SYM_TAR_SRC=<linux>/fs is the gate-2 corpus (the box used linux-7.2.3/fs).
#
# Every subcommand takes --dry-run (prints the aws/ssh commands, executes
# nothing, needs no credentials). `launch`/`full` refuse to run without the
# max-spend guard (MAX_CLUSTER_HOURS) and install a teardown-at-deadline
# safety process the moment instances exist. `teardown` is idempotent and
# ends with a tag-scoped sweep that FAILS LOUDLY if anything is still
# billing.
#
# Labeling discipline (see docs/cloud-benchmarking.md): every bench row is
# stamped with instrument, substrate (instance type + AZ + spot), venue, and
# order. Cloud rows are a THIRD substrate class — never mix them into
# devsub loop/tcp medians. A spot interruption mid-battery ABORTS the count
# (multi-run discipline: restart from zero); partial results are labeled
# INVALID, never spliced.
#
set -euo pipefail

# The node-side scripts whose logic a --dry-run cannot exercise live in one
# sourced lib the unit file tests/cloud_bench_cluster_units.sh runs against
# fake system tools (the apt hygiene, the machine-id assertion).
# shellcheck source=tests/cloud_bench_node_scripts.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/cloud_bench_node_scripts.sh"

# ============================ CONFIG ========================================
# --- AWS placement --------------------------------------------------------
AWS_REGION="${AWS_REGION:-us-east-1}"
AWS_AZ="${AWS_AZ:-us-east-1a}"
# cluster (default: same-rack, narrowest spot pool) | partition | spread |
# none (no placement group — the fallback when the cluster-PG spot pool is
# dry; same-AZ networking only, and the row label carries the placement).
PLACEMENT_STRATEGY="${PLACEMENT_STRATEGY:-cluster}"
# on-demand (default) | spot. Default flipped 2026-08-19 (user ruling):
# a spot reclaim 20 minutes into the mw acceptance session aborted the
# count (the multi-run discipline restarts counted runs from zero), so
# determinism is worth the ~2-3x hourly premium at these cluster sizes
# (~$2.75/hr on-demand for the 4-node mw preset). MARKET=spot remains
# the opt-in discount for uncounted/exploratory sessions; the market is
# stamped into every row label either way.
MARKET="${MARKET:-on-demand}"

# --- Instance preset ------------------------------------------------------
# i4i    = i4i.4xlarge  x N  (~$3-4/hr cluster on spot)   — the IOPS venue
# i3en   = i3en.12xlarge x N (~$10-14/hr cluster on spot) — the throughput venue
# mw     = i4i.2xlarge  x 4  (~$0.5-0.8/hr cluster on spot) — the CHEAPEST
#          adequate venue for the multi-writer s11-mpiio MPI-IO row
#          (assemble-mw / bench-mw); Ubuntu 26.04 AMI + dist/ubuntu2604
#          artifacts by default (the MW kernel floors — see AMI below)
# custom = use INSTANCE_TYPE below verbatim (still burst-class-refused)
PRESET="${PRESET:-i4i}"
INSTANCE_TYPE="${INSTANCE_TYPE:-}"          # only read when PRESET=custom

# --- Node roles --------------------------------------------------------------
# Defaults are PRESET-dependent (resolved after preset selection below; an
# explicit env value always wins): i4i/i3en/custom = 2 mds + 2 oss +
# 1 client + 1 spare = 6 nodes (the spare is a job worker / second load
# generator; the battery drives from client0); mw = 1 mds + 2 oss +
# 1 client = 4 nodes, NO spare (cheapness is the ruling — the MPI-IO row's
# whole fleet is CO-LOCATED on client0 and needs no second load generator).
N_MDS="${N_MDS:-}"
N_OSS="${N_OSS:-}"
N_CLIENT="${N_CLIENT:-}"
N_SPARE="${N_SPARE:-}"

# --- SSH ---------------------------------------------------------------
KEY_NAME="${KEY_NAME:-squeezefs-bench}"          # EC2 key pair name (must exist in region)
SSH_KEY_FILE="${SSH_KEY_FILE:-$HOME/.ssh/squeezefs-bench.pem}"
REMOTE_USER="${REMOTE_USER:-ubuntu}"             # matches the Ubuntu AMIs below (24.04/26.04)
OPERATOR_CIDR="${OPERATOR_CIDR:-}"               # empty = auto-detect via checkip.amazonaws.com

# --- Max-spend guard (REQUIRED for launch/full) -----------------------------
# Integer cluster-hours. launch refuses without it and installs a detached
# teardown-at-deadline process as the safety net.
MAX_CLUSTER_HOURS="${MAX_CLUSTER_HOURS:-}"

# --- AMI (SSM parameter path; default is PRESET-dependent) -------------------
# i4i/i3en/custom ride Ubuntu 24.04 (unchanged). mw needs TWO kernel floors
# the 24.04 GA kernel lacks — client FUSE-over-io_uring
# (/sys/module/fuse/parameters/enable_uring, mainline v6.14+) and
# storage-node nvmet Persistent Reservations (resv_enable, v6.13+; the S9
# multi-writer arm refuses non-PR substrates) — so it defaults to Ubuntu
# 26.04 LTS. The floor is NEVER trusted from the AMI: assemble-mw probes
# both and refuses loud with the remedy.
AMI_SSM_PARAM="${AMI_SSM_PARAM:-}"

# --- Artifacts to deploy (built elsewhere; this script never builds) ---------
# Default is PRESET-dependent: dist/ubuntu2404 (task build:ubuntu2404) for
# i4i/i3en/custom, dist/ubuntu2604 (task build:ubuntu2604) for mw — the
# artifact's glibc must match the AMI.
ARTIFACT_DIR="${ARTIFACT_DIR:-}"
ELBENCHO_BIN="${ELBENCHO_BIN:-}"   # DYNAMIC build (house rule); default
                                   # $ARTIFACT_DIR/elbencho once resolved

# --- Filesystem shape (mirrors tests/cluster_reset.sh) ----------------------
NQN_PREFIX="nqn.2026-07.io.squeezefs"
MOUNTPOINT="/scratch/mnt"
CACHE_DIR="/scratch/cache"
MOUNT_EXTRA="--interception --allow-other --log-file /tmp/sqz.log"

# --- Multi-writer fleet shape (PRESET=mw: assemble-mw / bench-mw) ------------
# CO-LOCATED (docs/operations.md §Multi-writer co-writer mounts): the
# authority and every co-writer mount on client0 and share the box's default
# NVMe host identity — the proven tests/cluster_reset_v5_mw.sh field recipe.
MW_COWRITERS="${MW_COWRITERS:-2}"   # co-writer mounts at $MOUNTPOINT-cw1..K.
                                    # 2 = the s11-mpiio leg's floor, sized for
                                    # the cheap 8-vCPU client (1 authority +
                                    # 2 co-writer daemons + 2x4 ior ranks);
                                    # the field/design shape is 8 — raise it
                                    # together with the client instance size.
MW_PORT="${MW_PORT:-45999}"         # authority custody/publish bind — STABLE
                                    # (the mw_fleet rung-10 finding: `auto`
                                    # mints a fresh ephemeral port per
                                    # incarnation, so a remounted authority
                                    # would be undialable by the recorded
                                    # endpoint forever)
MW_MEMBERSHIP_BIND="${MW_MEMBERSHIP_BIND:-auto}"  # the S9 arm's rung 4
                                                  # requires the membership
                                                  # plane armed
MW_RANGE_CUSTODY="${MW_RANGE_CUSTODY:-1}"  # on every co-writer — the
                                           # s11-mpiio row REQUIRES it (the
                                           # product default stays OFF; the
                                           # leg's engagement gate convicts
                                           # an unarmed fleet at probe time)
MW_FLEET_SHARE="${MW_FLEET_SHARE:-}"       # SQUEEZEFS_FLEET_SHARE per daemon;
                                           # EMPTY = derived 1+MW_COWRITERS
                                           # (KD-MW-14: N co-located daemons
                                           # divide the machine)
MW_IOR_PROCS="${MW_IOR_PROCS:-4}"          # ior procs per co-writer mount
                                           # (the leg's --procs, 1..16)
MW_MOUNT_EXTRA="--allow-other"             # NO --interception on MW mounts
                                           # (the proven mw_fleet/v5-mw
                                           # recipe — the MPI-IO row drives
                                           # the kernel FUSE path); per-mount
                                           # --log-file is appended per daemon

# --- Symmetric fleet shape (PRESET=mw SYMMETRIC=1: assemble-sym / bench-sym) -
# ONE symmetric writer PER CLIENT NODE (design-symmetric-metadata §7.3 — the
# join ladder; PR 12b's N-daemon posture made multi-node): client0 mounts
# first and wins the D0 ladder (the MANAGER); client1..N-1 join it over the
# wire as full writers (their own ring, page, slot leases, checkpoint task).
# Every node is its OWN registrant (the REMOTE posture, `joined_registrant_
# posture = registrant`): the node's nvme-cli host identity
# (/etc/nvme/hostnqn + hostid) is the registrant — assemble-sym asserts the
# identities DISTINCT across the client nodes and regenerates a duplicate (a
# baked AMI clones the file onto every node), and BEFORE them /etc/machine-id
# (the daemon's node token — src/writer_scope.rs — is derived from it: the
# 2026-09-24 cloud row's first assemble failed at the mounts because every
# joiner carried the manager's cloned `(node_token, mount_slot)`; a clone is
# regenerated with systemd-machine-id-setup). No SQUEEZEFS_FLEET_SHARE (one
# daemon per node owns its machine — the point of the venue). The set is
# formatted CACHE-LESS (no --disk-cache-paths — tests/mw_fleet.sh's shape
# for the box and local fleets): every beyond-inline file is a whole
# striped block at its publish, so gate 2 prices the DMA the box priced.
SYMMETRIC="${SYMMETRIC:-0}"
SYM_PORT="${SYM_PORT:-45999}"              # every writer's listener: <node private ip>:SYM_PORT
                                           # — an EXPLICIT bind (advertised verbatim); a
                                           # STABLE port for the same reason MW_PORT is
SYM_MEMBERSHIP_BIND="${SYM_MEMBERSHIP_BIND:-auto}"  # the manager's S6 shard (the joiners are
                                                    # its members — rung 3 of their ladder)
SYM_TOKEN_READER="${SYM_TOKEN_READER:-1}"  # 1 = also mount a `--read-only` TOKEN reader at
                                           # $MOUNTPOINT-ro on client0 (the -ls half of gate
                                           # 3b: K + C tokens, 0 leaf reads); 0 = skip it
SYM_TAR_SRC="${SYM_TAR_SRC:-}"             # the gate-2 corpus DIRECTORY (<linux>/fs) — the
                                           # box used linux-7.2.3/fs (2,468 entries); one
                                           # tarball is shipped to the two extracting nodes
SYM_TARBALL="${SYM_TARBALL:-}"             # …or a pre-made tarball (wins over SYM_TAR_SRC)
SYM_RT="${SYM_RT:-60}"                     # the sustained window per measured phase
                                           # (the AGENTS.md rule; the local scoping pass
                                           # runs 10). On the cloud venue a RATE phase
                                           # shorter than this is INVALID (the driver's
                                           # --size-to-rt=auto pilot sizes the phases to
                                           # fill it; a burst row is a failed row)
SYM_FILES="${SYM_FILES:-40000}"            # per-writer creates — a FLOOR: the driver's
                                           # N = 1 pilot sizes the create phase to SYM_RT
SYM_THREADS="${SYM_THREADS:-4}"            # storm threads per writer node
SYM_INGEST_MB="${SYM_INGEST_MB:-1024}"     # per-writer ingest MiB (4 MiB blocks, fsync) —
                                           # a FLOOR, sized to SYM_RT by the pilot up to
                                           # SYM_INGEST_CAP_MB
SYM_INGEST_CAP_MB="${SYM_INGEST_CAP_MB:-65536}"  # the pilot's per-writer ingest ceiling
                                           # (8 writers x 64 GiB fits the 2 x 1,875 GB
                                           # data namespaces)
SYM_SCALE_NS="${SYM_SCALE_NS:-}"           # gate 3's N ladder; EMPTY = 1,2,4,8 capped at N_CLIENT
SYM_ROWS="${SYM_ROWS:-tarx,shared,scale}"  # the row sets bench-sym runs, in
                                           # this order (the -ls half's token
                                           # reader lists BEFORE sym-scale's
                                           # leave/rejoin storm)
SYM_ARM_A="${SYM_ARM_A:-0}"                # 1 = ALSO run the design's "vs today" A arm
                                           # (authority + co-writers at the same N, the same
                                           # binary, default format) as an A-B-B-A over the
                                           # row set — DOUBLES the row wall; the B-only law
                                           # rows are the minimum (NOT BUILT in PR 15: the
                                           # A arm needs a per-node co-writer recipe this
                                           # rig does not have — refused loud)

# --- Benchmark battery knobs ------------------------------------------------
BENCH_THREADS=16          # thread count for 1m + seq rows
BENCH_QD_THREADS=32       # thread count for the t32qd32 rand-4k rows
BENCH_IODEPTH=32
FILE_SIZE_1M="2g"         # per-file size for the 1 MiB-block rows
FILE_SIZE_4K="256m"       # per-file size for the fresh seq-write-4k row

# --- Bookkeeping -------------------------------------------------------------
TAG_KEY="squeezefs-bench"                        # every AWS resource carries this tag
STATE_ROOT="${STATE_ROOT:-.cloud-bench}"         # local per-cluster state (gitignored)
RESULTS_ROOT="${RESULTS_ROOT:-.benchmarks/cloud}"
REMOTE_DIR="/opt/squeezefs-bench"                # artifact landing dir on nodes
# IdentitiesOnly: the operator's ssh agent may hold more keys than sshd's
# MaxAuthTries (6) — without it the bench key is never offered and every node
# reads "Too many authentication failures" (2026-09-12: ten agent keys; the
# launch stalled at 6/7 and the first cluster was torn down unused).
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new -o IdentitiesOnly=yes)
# ============================================================================

SCRIPT_PATH="$(readlink -f "$0")"

log()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
warn() { printf 'WARNING: %s\n' "$*" >&2; }
die()  { printf 'FATAL: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
usage: tests/cloud_bench_cluster.sh <subcommand> [flags]

subcommands:
  launch     create placement group + SG + launch template, launch spot fleet,
             wait running + SSH-reachable, install teardown-at-deadline guard
  deploy     scp squeezefs/elbencho/shim artifacts to every node, verify sha256,
             install runtime deps (fuse3), take Ubuntu's unattended apt off
             the session (unattended-upgrades drained + masked, apt-daily*
             timers off; a live apt/dpkg transaction waited for by its lock,
             bounded, never killed)
  assemble   share instance-store NVMe from storage nodes (squeezefs nvmeof
             share, nvmet), connect from the client (single-path — one NIC),
             format, mount, build_commit verification
  assemble-mw  (PRESET=mw) the same fabric steps as assemble, DIVERGING at
             the mount into the tests/cluster_reset_v5_mw.sh multi-writer
             fleet recipe: 1 authority + $MW_COWRITERS co-writers co-located
             on client0 (rung-3 enrollment harvest, roster re-arm, posture
             gates), nvmet PR assert per storage node + nvme resv-report per
             data namespace, both MW kernel floors probed loud
  bench      run the standing elbencho battery; results land in
             .benchmarks/cloud/<timestamp>/ with full row labeling
  bench-mw   (PRESET=mw) run the s11-mpiio shared-vs-disjoint MPI-IO ior row
             (tests/run_mw_matrix.sh external-mounts mode) over the MW fleet;
             rows -> .benchmarks/cloud/<timestamp>/ (cloud rows are a THIRD
             substrate class — never spliced into devsub medians)
  assemble-sym (PRESET=mw SYMMETRIC=1) the same fabric steps as assemble-mw,
             DIVERGING at the format into `format --symmetric` and at the
             mount into ONE symmetric writer PER CLIENT NODE: the MANAGER on
             client0, a JOINED writer on client1..N_CLIENT-1 (the join
             ladder over the real wire — each node its OWN registrant, its
             /etc/machine-id and nvme host identity asserted DISTINCT and a
             baked AMI's clone regenerated), an
             optional `--read-only` TOKEN reader on client0; posture gates
             read from every node's .stats (mount_posture writer,
             symmetric_join non-null, manager_lease held / joined_appender_id,
             appenders_known == N, the membership census)
  bench-sym  (PRESET=mw SYMMETRIC=1) the three symmetric row sets — gate 2
             sym-tarx (a JOINED node's tar -x vs the manager-local S0), gate
             3 sym-scale (N = 1/2/4/8 writer NODES — the per-node law), gate
             3b sym-shared-dir (+ -ls) — driven from THIS box over ssh by
             tests/cloud_sym_rows.sh with the matrix's own laws
             (tests/sym_rows_lib.sh); rows -> .benchmarks/cloud/<ts>/sym-rows/
  sym-hook   (internal) `sym-hook mount|unmount <client-pub-ip> <mnt>` —
             the driver's leave/rejoin of one writer node (gate 3's
             "exactly N appenders live")
  status     instance table, elapsed cluster-hours vs the max-spend guard,
             estimated spend
  teardown   terminate instances, delete SG/launch template/placement group,
             cancel the deadline guard, tag-scoped final sweep (fails loudly
             on any still-billing resource). Idempotent.
  full       launch -> deploy -> assemble -> bench -> teardown
             (PRESET=mw: launch -> deploy -> assemble-mw -> bench-mw -> teardown;
              PRESET=mw SYMMETRIC=1: … -> assemble-sym -> bench-sym -> teardown)

flags:
  --dry-run          print every aws/ssh command instead of executing (no
                     credentials needed)
  --yes              skip the typed-YES confirmations (used by the deadline
                     guard and the failure trap)
  --cluster-id ID    operate on a specific cluster (default: the one recorded
                     in .cloud-bench/current)
  --preset P         i4i | i3en | mw | custom (overrides $PRESET)
  --symmetric        the symmetric fleet shape (PRESET=mw only; = SYMMETRIC=1)

The max-spend guard: launch/full refuse unless MAX_CLUSTER_HOURS is a
positive integer. See the header quickstart for the cost table.

Pass PRESET (env or --preset) on EVERY subcommand of an mw cluster — the
preset selects the role shape, the AMI and the artifact defaults; only the
node list persists in cluster state.
EOF
}

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
SUBCMD="${1:-}"
[ -n "$SUBCMD" ] || { usage; exit 1; }
shift || true
# sym-hook's three positionals (the driver's contract: `<flags> mount|unmount
# <host> <mnt>` — the driver appends them after the rig's own flags)
HOOK_POS=()

DRY_RUN=false
ASSUME_YES=false
CID_ARG=""
GUARD_SECS=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run)    DRY_RUN=true ;;
    --yes)        ASSUME_YES=true ;;
    --cluster-id) CID_ARG="${2:?--cluster-id needs a value}"; shift ;;
    --preset)     PRESET="${2:?--preset needs a value}"; shift ;;
    --symmetric)  SYMMETRIC=1 ;;
    --secs)       GUARD_SECS="${2:?--secs needs a value}"; shift ;;  # __deadline-guard only
    -h|--help)    usage; exit 0 ;;
    *)
      if [ "$SUBCMD" = "sym-hook" ] && [ "${#HOOK_POS[@]}" -lt 3 ]; then
        HOOK_POS+=("$1")
      else
        die "unknown flag: $1 (see --help)"
      fi
      ;;
  esac
  shift
done
HOOK_VERB="" HOOK_HOST="" HOOK_MNT=""
if [ "$SUBCMD" = "sym-hook" ]; then
  [ "${#HOOK_POS[@]}" -eq 3 ] || die "sym-hook needs three positionals: mount|unmount <host> <mnt> (got: ${HOOK_POS[*]:-none})"
  HOOK_VERB="${HOOK_POS[0]}"; HOOK_HOST="${HOOK_POS[1]}"; HOOK_MNT="${HOOK_POS[2]}"
fi

# ---------------------------------------------------------------------------
# Preset resolution
# ---------------------------------------------------------------------------
case "$PRESET" in
  i4i)
    INSTANCE_TYPE="i4i.4xlarge"
    EST_CLUSTER_HOURLY="~\$12-17/hr on-demand / ~\$3-4/hr spot (6 nodes)"
    ;;
  i3en)
    INSTANCE_TYPE="i3en.12xlarge"
    EST_CLUSTER_HOURLY="~\$30-45/hr on-demand / ~\$10-14/hr spot (6 nodes)"
    ;;
  mw)
    # ONE launch template + ONE spot fleet covers every node (the existing
    # launch structure), so the mw preset is UNIFORM i4i.2xlarge rather than
    # per-role client/storage types — a per-role split (i4i.xlarge storage +
    # i4i.2xlarge client) would need a second template + fleet and complicate
    # the teardown/guard surface for ~$0.2/hr of savings at this node count.
    # i4i.2xlarge: 8 vCPU, 1 x 1,875 GB instance-store Nitro NVMe, "up to
    # 12.5 Gbps" network. The client runs the whole co-located fleet
    # (1 authority + MW_COWRITERS co-writer daemons + the ior ranks); the
    # storage nodes idle at one nvmet target each.
    #
    # Small-instance "up to N Gbps" network baselines are BURST-shaped too
    # (unlike CPU credits, not refused): the s11-mpiio row survives because
    # it is a same-substrate shared-vs-disjoint RATIO with internal A-B-B-A
    # brackets, and the leg's flatness/self-sizing gates catch credit sag —
    # but the row stamp records the instance types so the label stays honest.
    #
    # INSTANCE_TYPE is an OVERRIDE here (the one preset that honors it):
    # the 8-co-writer field/design shape needs a bigger client (the comment
    # on MW_COWRITERS above — "raise it together with the client instance
    # size"), and the fleet stays UNIFORM one-type by the same one-template
    # rationale. i4i.4xlarge (16 vCPU) matches the proven local venue;
    # i4i.8xlarge (32 vCPU) removes the client CPU-starvation question.
    INSTANCE_TYPE="${INSTANCE_TYPE:-i4i.2xlarge}"
    EST_CLUSTER_HOURLY="~\$2.7-2.8/hr on-demand at the i4i.2xlarge default / ~\$11/hr at i4i.8xlarge (4 nodes; planning numbers)"
    ;;
  custom)
    [ -n "$INSTANCE_TYPE" ] || die "PRESET=custom requires INSTANCE_TYPE"
    EST_CLUSTER_HOURLY="unknown (custom preset — check $MARKET pricing yourself)"
    ;;
  *) die "PRESET must be i4i | i3en | mw | custom (got: $PRESET)" ;;
esac

# Preset-dependent defaults (an explicit env value always wins — see the
# config block notes on roles, AMI kernel floors, and artifact glibc).
[ "$SYMMETRIC" = "0" ] || [ "$SYMMETRIC" = "1" ] || die "SYMMETRIC must be 0 or 1 (got: $SYMMETRIC)"
if [ "$SYMMETRIC" = "1" ] && [ "$PRESET" != "mw" ]; then
  die "SYMMETRIC=1 is the mw preset's shape (PRESET=mw SYMMETRIC=1 N_CLIENT=<writer nodes>)"
fi
if [ "$PRESET" = "mw" ]; then
  N_MDS="${N_MDS:-1}"
  N_OSS="${N_OSS:-2}"
  if [ "$SYMMETRIC" = "1" ]; then
    # N_CLIENT = the WRITER NODE COUNT (one symmetric writer per node);
    # the smallest multi-node shape is the default.
    N_CLIENT="${N_CLIENT:-2}"
    [[ "$N_CLIENT" =~ ^[0-9]+$ ]] && [ "$N_CLIENT" -ge 2 ] ||
      die "SYMMETRIC=1 needs N_CLIENT >= 2 writer nodes (a one-node symmetric set is the solo mount every local venue already measures; got: $N_CLIENT)"
    # (EST_CLUSTER_HOURLY for this shape is set below, once N_TOTAL is known
    # — it prices EVERY node, not 3 + N_CLIENT.)
  else
    N_CLIENT="${N_CLIENT:-1}"
  fi
  N_SPARE="${N_SPARE:-0}"
  AMI_SSM_PARAM="${AMI_SSM_PARAM:-/aws/service/canonical/ubuntu/server/26.04/stable/current/amd64/hvm/ebs-gp3/ami-id}"
  ARTIFACT_DIR="${ARTIFACT_DIR:-dist/ubuntu2604}"
else
  N_MDS="${N_MDS:-2}"
  N_OSS="${N_OSS:-2}"
  N_CLIENT="${N_CLIENT:-1}"
  N_SPARE="${N_SPARE:-1}"
  AMI_SSM_PARAM="${AMI_SSM_PARAM:-/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id}"
  ARTIFACT_DIR="${ARTIFACT_DIR:-dist/ubuntu2404}"
fi
ELBENCHO_BIN="${ELBENCHO_BIN:-$ARTIFACT_DIR/elbencho}"

# Burst-class refusal — credit-throttled CPU/network + no instance store means
# every row would be INVALID under the labeling discipline. Non-negotiable.
case "$INSTANCE_TYPE" in
  t2.*|t3.*|t3a.*|t4g.*)
    die "burst-class instance type '$INSTANCE_TYPE' refused: CPU-credit throttling makes medians a function of credit balance, and there is no instance store. Use the i4i/i3en presets."
    ;;
esac

N_TOTAL=$((N_MDS + N_OSS + N_CLIENT + N_SPARE))
if [ "$PRESET" = "mw" ] && [ "$SYMMETRIC" = "1" ]; then
  # EVERY node bills — N_MDS + N_OSS + N_CLIENT + N_SPARE. The previous
  # `3 + N_CLIENT` priced the default 1 mds + 2 oss only: the 2026-09-24
  # cloud row launched 17 nodes (N_OSS=8) while its typed-YES line read
  # 11 x i4i.2xlarge.
  EST_CLUSTER_HOURLY="~\$$(python3 -c "print(f'{0.686*$N_TOTAL:.2f}')")/hr on-demand ($N_TOTAL x i4i.2xlarge at ~\$0.686/hr = $N_MDS mds + $N_OSS oss + $N_CLIENT client + $N_SPARE spare; planning number)"
fi
ROLE_NAMES=()
for ((i = 0; i < N_MDS; i++));    do ROLE_NAMES+=("mds$i");    done
for ((i = 0; i < N_OSS; i++));    do ROLE_NAMES+=("oss$i");    done
for ((i = 0; i < N_CLIENT; i++)); do ROLE_NAMES+=("client$i"); done
for ((i = 0; i < N_SPARE; i++));  do ROLE_NAMES+=("spare$i");  done
[ "$N_MDS" -ge 1 ] && [ "$N_OSS" -ge 1 ] && [ "$N_CLIENT" -ge 1 ] \
  || die "need at least 1 mds, 1 oss, 1 client (have mds=$N_MDS oss=$N_OSS client=$N_CLIENT)"

# ---------------------------------------------------------------------------
# Execution helpers — every mutating aws/ssh/scp call goes through these so
# --dry-run prints the exact command instead of running it.
# ---------------------------------------------------------------------------
run() { # run <cmd...> — mutating local command
  if $DRY_RUN; then
    printf '+'; printf ' %q' "$@"; printf '\n'
  else
    "$@"
  fi
}

awsc() { # awsc <args...> — mutating aws call
  run aws --region "$AWS_REGION" "$@"
}

awsq() { # awsq <canned-dry-run-output> <args...> — query aws call
  local canned="$1"; shift
  if $DRY_RUN; then
    printf '+ aws --region %s %s\n' "$AWS_REGION" "$*" >&2
    printf '%s\n' "$canned"
  else
    aws --region "$AWS_REGION" "$@"
  fi
}

# remote <ip> [VAR=val ...]  — remote root script arrives on stdin (heredoc).
# The env words ride ONE command string the remote login shell re-parses,
# so each is `%q`-quoted: a value with a space stays one assignment by
# construction (comma-separated lists remain the house style for lists).
remote() {
  local ip="$1"; shift
  local envq=""
  [ "$#" -eq 0 ] || envq="$(printf '%q ' "$@")"
  local script
  script="$(cat)"
  if $DRY_RUN; then
    printf "+ ssh %s@%s sudo env %sbash -s <<'EOS'\n" "$REMOTE_USER" "$ip" "$envq"
    printf '%s\nEOS\n' "$script"
  else
    ssh "${SSH_OPTS[@]}" -i "$SSH_KEY_FILE" "$REMOTE_USER@$ip" \
      "sudo env ${envq}bash -s" <<<"$script"
  fi
}

push() { # push <local-file> <ip> <remote-path-under-/tmp-then-installed>
  local src="$1" ip="$2" dst="$3"
  run scp "${SSH_OPTS[@]}" -i "$SSH_KEY_FILE" "$src" "$REMOTE_USER@$ip:/tmp/$(basename "$dst")"
  remote "$ip" DST="$dst" SRC="/tmp/$(basename "$dst")" <<'EOS'
set -euo pipefail
mkdir -p "$(dirname "$DST")"
install -m 0755 "$SRC" "$DST"
EOS
}

confirm() { # confirm <prompt> — typed-YES gate for destructive/costly actions
  $ASSUME_YES && return 0
  $DRY_RUN && { echo "(dry-run: would ask for typed YES: $1)"; return 0; }
  echo "$1"
  read -r -p "Type YES to continue: " ans
  [ "$ans" = "YES" ] || die "aborted (typed-YES not given)"
}

require_local_tools() {
  local t
  for t in aws ssh scp; do
    if ! command -v "$t" >/dev/null 2>&1; then
      if $DRY_RUN; then
        warn "tool missing on operator box: $t (dry-run continues; a real run needs it)"
      else
        die "required tool missing on operator box: $t"
      fi
    fi
  done
  if ! $DRY_RUN; then
    aws --version 2>&1 | grep -q 'aws-cli/2' || die "aws cli v2 required ('aws --version' does not report aws-cli/2)"
  fi
}

# ---------------------------------------------------------------------------
# Cluster state (local, gitignored). One dir per cluster id; `current` names
# the active one. Teardown can also work purely from --cluster-id + tags, so
# a lost state dir never strands a billing resource.
# ---------------------------------------------------------------------------
CID=""
STATE_DIR=""
LT_ID="" SG_ID="" PG_NAME="" VPC_ID="" SUBNET_ID=""
LAUNCH_EPOCH="" DEADLINE_EPOCH="" DEADLINE_PID=""
SYM_STORAGE_DEVS=""   # assemble-sym records `<oss pub ip>:<dev>,…` (bench-sym's amplification columns)
NODE_NAMES=() NODE_IDS=() NODE_PUB=() NODE_PRIV=()

state_file() { echo "$STATE_DIR/cluster.env"; }

save_state() {
  mkdir -p "$STATE_DIR"
  {
    echo "CID=$CID"
    echo "AWS_REGION=$AWS_REGION"
    echo "AWS_AZ=$AWS_AZ"
    echo "PRESET=$PRESET"
    echo "INSTANCE_TYPE=$INSTANCE_TYPE"
    echo "LT_ID=$LT_ID"
    echo "SG_ID=$SG_ID"
    echo "PG_NAME=$PG_NAME"
    echo "VPC_ID=$VPC_ID"
    echo "SUBNET_ID=$SUBNET_ID"
    echo "LAUNCH_EPOCH=$LAUNCH_EPOCH"
    echo "DEADLINE_EPOCH=$DEADLINE_EPOCH"
    echo "DEADLINE_PID=$DEADLINE_PID"
    echo "SYM_STORAGE_DEVS=$SYM_STORAGE_DEVS"
    local i
    for i in "${!NODE_NAMES[@]}"; do
      echo "NODE ${NODE_NAMES[$i]} ${NODE_IDS[$i]} ${NODE_PUB[$i]} ${NODE_PRIV[$i]}"
    done
  } >"$(state_file)"
  echo "$CID" >"$STATE_ROOT/current"
}

load_state() { # load_state [--placeholder-ok] [--tags-ok]
  local flags="$*"
  # in-memory state from an earlier step of the same invocation (e.g. `full`)
  if [ -n "$CID" ] && [ "${#NODE_NAMES[@]}" -gt 0 ]; then
    STATE_DIR="${STATE_DIR:-$STATE_ROOT/$CID}"
    return 0
  fi
  if [ -n "$CID_ARG" ]; then
    CID="$CID_ARG"
  elif [ -f "$STATE_ROOT/current" ]; then
    CID="$(cat "$STATE_ROOT/current")"
  fi
  if [ -n "$CID" ] && [ -f "$STATE_ROOT/$CID/cluster.env" ]; then
    STATE_DIR="$STATE_ROOT/$CID"
    local key rest
    NODE_NAMES=() NODE_IDS=() NODE_PUB=() NODE_PRIV=()
    while read -r key rest; do
      case "$key" in
        NODE)
          local n id pub priv
          read -r n id pub priv <<<"$rest"
          NODE_NAMES+=("$n"); NODE_IDS+=("$id"); NODE_PUB+=("$pub"); NODE_PRIV+=("$priv")
          ;;
        CID=*|AWS_REGION=*|AWS_AZ=*|PRESET=*|INSTANCE_TYPE=*|LT_ID=*|SG_ID=*|PG_NAME=*|VPC_ID=*|SUBNET_ID=*|LAUNCH_EPOCH=*|DEADLINE_EPOCH=*|DEADLINE_PID=*|SYM_STORAGE_DEVS=*)
          # shellcheck disable=SC2163  # deliberate: keys are the enumerated allowlist above
          export "$key"
          ;;
      esac
    done <"$STATE_DIR/cluster.env"
    return 0
  fi
  if [ -n "$CID" ] && [[ "$flags" == *--cid-only-ok* ]]; then
    # teardown/status can work purely from --cluster-id + tags (a lost state
    # dir must never strand a billing resource)
    STATE_DIR="$STATE_ROOT/$CID"
    warn "no state file for $CID — operating from tags only"
    return 0
  fi
  if $DRY_RUN && [[ "$flags" == *--placeholder-ok* ]]; then
    # Fabricated placeholder cluster so --dry-run can showcase every
    # subcommand without state and without credentials.
    CID="${CID:-sqzbench-DRYRUN}"
    STATE_DIR="$STATE_ROOT/$CID"
    LT_ID="lt-dryrun" SG_ID="sg-dryrun" PG_NAME="$CID" VPC_ID="vpc-dryrun" SUBNET_ID="subnet-dryrun"
    LAUNCH_EPOCH="$(date +%s)" DEADLINE_EPOCH=$((LAUNCH_EPOCH + 3600)) DEADLINE_PID=""
    NODE_NAMES=() NODE_IDS=() NODE_PUB=() NODE_PRIV=()
    local i
    for i in "${!ROLE_NAMES[@]}"; do
      NODE_NAMES+=("${ROLE_NAMES[$i]}")
      NODE_IDS+=("i-dryrun$i")
      NODE_PUB+=("203.0.113.$((10 + i))")
      NODE_PRIV+=("10.0.1.$((10 + i))")
    done
    SYM_STORAGE_DEVS=""
    for i in "${!ROLE_NAMES[@]}"; do
      [ "$(role_of "${ROLE_NAMES[$i]}")" = "oss" ] && SYM_STORAGE_DEVS="${SYM_STORAGE_DEVS:+$SYM_STORAGE_DEVS,}${NODE_PUB[$i]}:/dev/nvme1n1"
    done
    warn "dry-run without cluster state: using placeholder cluster $CID"
    return 0
  fi
  die "no cluster state (run 'launch' first, or pass --cluster-id; state root: $STATE_ROOT)"
}

node_pub() { # node_pub <name>
  local i
  for i in "${!NODE_NAMES[@]}"; do
    [ "${NODE_NAMES[$i]}" = "$1" ] && { echo "${NODE_PUB[$i]}"; return 0; }
  done
  die "unknown node: $1"
}

node_priv() { # node_priv <name>
  local i
  for i in "${!NODE_NAMES[@]}"; do
    [ "${NODE_NAMES[$i]}" = "$1" ] && { echo "${NODE_PRIV[$i]}"; return 0; }
  done
  die "unknown node: $1"
}

role_of() { # role_of <name> -> mds|oss|client|spare
  case "$1" in
    mds*) echo mds ;; oss*) echo oss ;; client*) echo client ;; spare*) echo spare ;;
    *) die "unroleable node name: $1" ;;
  esac
}

mw_shape_check() { # the MW fleet knobs, validated before anything costly
  [ "$SYMMETRIC" = "1" ] && return 0   # the symmetric shape has no co-writers (sym_shape_check)
  [[ "$MW_COWRITERS" =~ ^[0-9]+$ ]] && [ "$MW_COWRITERS" -ge 2 ] \
    || die "MW_COWRITERS must be an integer >= 2 (the s11-mpiio leg's floor; got: $MW_COWRITERS)"
  [[ "$MW_IOR_PROCS" =~ ^[0-9]+$ ]] && [ "$MW_IOR_PROCS" -ge 1 ] && [ "$MW_IOR_PROCS" -le 16 ] \
    || die "MW_IOR_PROCS must be 1..16 (the leg's --procs range; got: $MW_IOR_PROCS)"
}

# ---------------------------------------------------------------------------
# Global EXIT trap — one handler covers launch (partial-resource cleanup),
# full (best-effort teardown on mid-run failure), and bench (spot-monitor
# reap). Installed once at dispatch; never layered/clobbered by subcommands.
# ---------------------------------------------------------------------------
LAUNCH_RESOURCES_CREATED=false
LAUNCH_OK=false
FULL_ACTIVE=false
TRAP_RUNNING=false

global_exit_trap() {
  local rc=$?
  $TRAP_RUNNING && return 0
  TRAP_RUNNING=true
  spot_monitor_stop
  if [ "$rc" -ne 0 ] && ! $DRY_RUN; then
    if $FULL_ACTIVE && $LAUNCH_OK; then
      warn "full run failed mid-way — best-effort teardown of $CID"
      ASSUME_YES=true cmd_teardown \
        || warn "best-effort teardown ALSO failed — run 'teardown --cluster-id $CID' manually; resources may still be BILLING"
    elif $LAUNCH_RESOURCES_CREATED && ! $LAUNCH_OK; then
      warn "launch failed after creating AWS resources — best-effort teardown of $CID now"
      ASSUME_YES=true cmd_teardown \
        || warn "best-effort teardown ALSO failed — run 'teardown --cluster-id $CID' manually; resources may still be BILLING"
    fi
  fi
}

# Every refusal that is DECIDABLE on the operator box runs here, BEFORE the
# typed YES and the first billed minute (review round 1, Issue 10 — a
# refusal deploy/assemble/bench would have raised is ≈ 15–20 billed
# node-minutes under `full`): the artifacts, the instrument, and the
# symmetric shape's own preconditions. The later steps keep their checks
# (idempotent — a step-by-step operator may change the env between them).
launch_preflight() {
  if ! $DRY_RUN; then
    [ -f "$SSH_KEY_FILE" ] || die "SSH key file not found: $SSH_KEY_FILE"
    [ -x "$ARTIFACT_DIR/squeezefs" ] \
      || die "squeezefs artifact missing/not executable: $ARTIFACT_DIR/squeezefs (build elsewhere: task build:${ARTIFACT_DIR##*/}) — refused BEFORE launch"
    if [ "$PRESET" != "mw" ] && [ ! -x "$ELBENCHO_BIN" ]; then
      die "elbencho artifact missing: $ELBENCHO_BIN (must be a DYNAMIC build) — refused BEFORE launch"
    fi
  fi
  if [ "$PRESET" = "mw" ]; then
    if [ "$SYMMETRIC" = "1" ]; then
      sym_preflight
    else
      mw_shape_check
    fi
  fi
}

cmd_launch() {
  require_local_tools

  # --- Max-spend guard: refuse to launch without it -----------------------
  [[ "$MAX_CLUSTER_HOURS" =~ ^[0-9]+$ ]] && [ "$MAX_CLUSTER_HOURS" -ge 1 ] \
    || die "max-spend guard not set: export MAX_CLUSTER_HOURS=<positive integer hours>. The script will not launch billing instances without a deadline (preset $PRESET costs $EST_CLUSTER_HOURLY)."
  launch_preflight

  CID="sqzbench-$(date +%Y%m%d-%H%M%S)"
  STATE_DIR="$STATE_ROOT/$CID"
  PG_NAME="$CID"

  confirm "About to launch $N_TOTAL x $INSTANCE_TYPE ${MARKET^^} instances in $AWS_AZ ($EST_CLUSTER_HOURLY), max $MAX_CLUSTER_HOURS cluster-hour(s), cluster id $CID. This COSTS MONEY."

  log "launch 1/7: operator IP for the SSH ingress rule"
  if [ -n "$OPERATOR_CIDR" ]; then
    echo "  using configured OPERATOR_CIDR=$OPERATOR_CIDR"
  elif $DRY_RUN; then
    OPERATOR_CIDR="203.0.113.7/32"
    echo "+ curl -s https://checkip.amazonaws.com   (dry-run: canned $OPERATOR_CIDR)"
  else
    OPERATOR_CIDR="$(curl -sf https://checkip.amazonaws.com | tr -d '[:space:]')/32"
    [[ "$OPERATOR_CIDR" =~ ^[0-9.]+/32$ ]] || die "could not auto-detect operator IP; set OPERATOR_CIDR"
    echo "  detected $OPERATOR_CIDR"
  fi

  log "launch 2/7: AMI (baked base preferred) + default subnet in $AWS_AZ"
  # Baked-base preference (2026-08-19/20: session-time apt lost 40+ billed
  # minutes to a wedged regional mirror TWICE): prefer the newest available
  # self-owned image tagged squeezefs-bench-base=$PRESET — packages
  # preinstalled at bake time, zero apt on the session clock (deploy's
  # dpkg -s verify + fallback still guards a stale bake). BASE_AMI=<id>
  # pins explicitly; BASE_AMI=none forces the stock SSM AMI. Re-bake:
  #   aws ec2 create-image --instance-id <provisioned node> --no-reboot \
  #     --tag-specifications 'ResourceType=image,Tags=[{Key=squeezefs-bench-base,Value=mw}]' ...
  local ami=""
  if [ "${BASE_AMI:-}" = "none" ]; then
    :
  elif [ -n "${BASE_AMI:-}" ]; then
    ami="$BASE_AMI"
  else
    ami="$(awsq "" ec2 describe-images --owners self \
      --filters "Name=tag:squeezefs-bench-base,Values=$PRESET" "Name=state,Values=available" \
      --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text)"
    [ "$ami" = "None" ] && ami=""
  fi
  if [ -n "$ami" ]; then
    echo "  using baked base AMI $ami (tag squeezefs-bench-base=$PRESET; BASE_AMI=none opts out)"
  else
    ami="$(awsq "ami-dryrun" ssm get-parameter \
      --name "$AMI_SSM_PARAM" \
      --query Parameter.Value --output text)"
  fi
  SUBNET_ID="$(awsq "subnet-dryrun" ec2 describe-subnets \
    --filters "Name=availability-zone,Values=$AWS_AZ" "Name=default-for-az,Values=true" \
    --query 'Subnets[0].SubnetId' --output text)"
  [ "$SUBNET_ID" != "None" ] || die "no default subnet in $AWS_AZ (default VPC required; it auto-assigns the public IPs SSH needs)"
  VPC_ID="$(awsq "vpc-dryrun" ec2 describe-subnets --subnet-ids "$SUBNET_ID" \
    --query 'Subnets[0].VpcId' --output text)"
  echo "  ami=$ami subnet=$SUBNET_ID vpc=$VPC_ID"

  log "launch 3/7: placement group ($PLACEMENT_STRATEGY strategy) + security group"
  # cluster placement = same-rack networking; note: it narrows the spot pool,
  # which capacity-optimized allocation partially compensates for. When the
  # cluster-PG spot pool is dry across AZs (the 2026-08-19 1/4-delivered
  # shape in three AZs), PLACEMENT_STRATEGY=none launches without a PG —
  # same-AZ networking only; the row's LABEL carries the placement, so a
  # PG-less row is honest, just a different substrate line.
  if [ "$PLACEMENT_STRATEGY" != "none" ]; then
    awsc ec2 create-placement-group --group-name "$PG_NAME" --strategy "$PLACEMENT_STRATEGY" \
      --tag-specifications "ResourceType=placement-group,Tags=[{Key=$TAG_KEY,Value=$CID}]"
  fi
  SG_ID="$(awsq "sg-dryrun" ec2 create-security-group \
    --group-name "$CID" --description "squeezefs bench cluster $CID" --vpc-id "$VPC_ID" \
    --tag-specifications "ResourceType=security-group,Tags=[{Key=$TAG_KEY,Value=$CID}]" \
    --query GroupId --output text)"
  LAUNCH_RESOURCES_CREATED=true
  # intra-SG all traffic (NVMe-oF/TCP fabric etc.) + SSH from the operator only
  awsc ec2 authorize-security-group-ingress --group-id "$SG_ID" \
    --ip-permissions "IpProtocol=-1,UserIdGroupPairs=[{GroupId=$SG_ID}]"
  awsc ec2 authorize-security-group-ingress --group-id "$SG_ID" \
    --protocol tcp --port 22 --cidr "$OPERATOR_CIDR"

  log "launch 4/7: launch template + spot fleet (capacity-optimized)"
  # Package installs ride cloud-init USER DATA — they run during boot, in
  # parallel across every node, overlapped with the launch waits and the
  # binary pushes (the 2026-08-19 lesson: a serial deploy-time apt against
  # a slow mirror held a billing cluster idle for 40+ minutes). deploy
  # WAITS on cloud-init and verifies, with an apt fallback only if
  # user-data failed. The mw client set installs on every node — a few
  # idle MiB on storage nodes buys the one-template simplicity.
  # `attr` (getfattr/setfattr): the symmetric rows read the striped
  # directory's `user.squeezefs.stripes` — the cloud image ships libattr1
  # but not the tools (PR 15 review round 1, Issue 1).
  local extra_pkgs=""
  [ "$PRESET" = "mw" ] && extra_pkgs=" openmpi-bin libopenmpi-dev python3 curl gcc make attr"
  local user_data
  user_data="$(printf '#!/bin/bash\nexport DEBIAN_FRONTEND=noninteractive\napt-get -qq update\napt-get -qq install -y fuse3 nvme-cli%s\n' "$extra_pkgs" | base64 -w0)"
  LT_ID="$(awsq "lt-dryrun" ec2 create-launch-template \
    --launch-template-name "$CID" \
    --tag-specifications "ResourceType=launch-template,Tags=[{Key=$TAG_KEY,Value=$CID}]" \
    --launch-template-data "{\"ImageId\":\"$ami\",\"KeyName\":\"$KEY_NAME\",\"SecurityGroupIds\":[\"$SG_ID\"],\"UserData\":\"$user_data\"$([ "$PLACEMENT_STRATEGY" != none ] && printf ',"Placement":{"GroupName":"%s"}' "$PG_NAME"),\"TagSpecifications\":[{\"ResourceType\":\"instance\",\"Tags\":[{\"Key\":\"$TAG_KEY\",\"Value\":\"$CID\"}]}]}" \
    --query 'LaunchTemplate.LaunchTemplateId' --output text)"
  # on-demand (default): no interruption risk — what protects the
  # counted-run discipline outright. spot: capacity-optimized = fewest
  # interruptions. type=instant returns instance ids synchronously.
  local canned_ids i
  canned_ids=""
  for ((i = 0; i < N_TOTAL; i++)); do canned_ids+="i-dryrun$i "; done
  local market_args=()
  if [ "$MARKET" = "spot" ]; then
    market_args+=(--spot-options 'AllocationStrategy=capacity-optimized,InstanceInterruptionBehavior=terminate')
  fi
  local ids_text
  ids_text="$(awsq "$canned_ids" ec2 create-fleet --type instant \
    --launch-template-configs "[{\"LaunchTemplateSpecification\":{\"LaunchTemplateId\":\"$LT_ID\",\"Version\":\"\$Latest\"},\"Overrides\":[{\"InstanceType\":\"$INSTANCE_TYPE\",\"SubnetId\":\"$SUBNET_ID\",\"AvailabilityZone\":\"$AWS_AZ\"}]}]" \
    "${market_args[@]}" \
    --target-capacity-specification "TotalTargetCapacity=$N_TOTAL,DefaultTargetCapacityType=$MARKET" \
    --tag-specifications "ResourceType=fleet,Tags=[{Key=$TAG_KEY,Value=$CID}]" \
    --query 'Instances[].InstanceIds[]' --output text)"
  mapfile -t NODE_IDS < <(xargs -n1 <<<"$ids_text")
  [ "${#NODE_IDS[@]}" -eq "$N_TOTAL" ] \
    || die "$MARKET fleet delivered ${#NODE_IDS[@]}/$N_TOTAL instances (insufficient $MARKET capacity for $INSTANCE_TYPE in $AWS_AZ?) — tearing down"
  echo "  instances: ${NODE_IDS[*]}"

  log "launch 5/7: wait until running (timeout ~10 min), fetch IPs, tag roles"
  awsc ec2 wait instance-running --instance-ids "${NODE_IDS[@]}"
  local canned_net=""
  for ((i = 0; i < N_TOTAL; i++)); do
    canned_net+="i-dryrun$i"$'\t'"10.0.1.$((10 + i))"$'\t'"203.0.113.$((10 + i))"$'\n'
  done
  local net
  net="$(awsq "$canned_net" ec2 describe-instances --instance-ids "${NODE_IDS[@]}" \
    --query 'Reservations[].Instances[].[InstanceId,PrivateIpAddress,PublicIpAddress]' --output text)"
  NODE_NAMES=() NODE_PUB=() NODE_PRIV=()
  local id priv pub name idx=0
  declare -A BY_ID_PRIV=() BY_ID_PUB=()
  while read -r id priv pub; do
    [ -n "$id" ] || continue
    BY_ID_PRIV[$id]="$priv"; BY_ID_PUB[$id]="$pub"
  done <<<"$net"
  for idx in "${!NODE_IDS[@]}"; do
    id="${NODE_IDS[$idx]}"
    name="${ROLE_NAMES[$idx]}"
    NODE_NAMES+=("$name")
    NODE_PRIV+=("${BY_ID_PRIV[$id]:-unknown}")
    NODE_PUB+=("${BY_ID_PUB[$id]:-unknown}")
    [ "${BY_ID_PUB[$id]:-}" != "None" ] || die "$name ($id) got no public IP — default subnet must MapPublicIpOnLaunch"
    awsc ec2 create-tags --resources "$id" \
      --tags "Key=Name,Value=$CID-$name" "Key=$TAG_KEY:role,Value=$(role_of "$name")"
    echo "  $name  $id  pub=${BY_ID_PUB[$id]:-?}  priv=${BY_ID_PRIV[$id]:-?}"
  done

  log "launch 6/7: wait for SSH on every node (timeout 5 min/node)"
  local ip try ok
  for idx in "${!NODE_NAMES[@]}"; do
    ip="${NODE_PUB[$idx]}"
    if $DRY_RUN; then
      printf '+ ssh %s@%s true   (poll until reachable)\n' "$REMOTE_USER" "$ip"
      continue
    fi
    ok=false
    for try in $(seq 1 30); do
      if ssh "${SSH_OPTS[@]}" -i "$SSH_KEY_FILE" "$REMOTE_USER@$ip" true 2>/dev/null; then
        ok=true; break
      fi
      sleep 10
    done
    $ok || die "${NODE_NAMES[$idx]} ($ip) never became SSH-reachable ($try tries)"
    echo "  ${NODE_NAMES[$idx]} ($ip): ssh ok"
  done

  log "launch 7/7: record state + install teardown-at-deadline guard"
  LAUNCH_EPOCH="$(date +%s)"
  DEADLINE_EPOCH=$((LAUNCH_EPOCH + MAX_CLUSTER_HOURS * 3600))
  if $DRY_RUN; then
    printf '+ setsid nohup %q __deadline-guard --cluster-id %s --secs %s --yes &\n' \
      "$SCRIPT_PATH" "$CID" $((MAX_CLUSTER_HOURS * 3600))
    DEADLINE_PID="dryrun"
    echo "(dry-run: no state written, no instances launched)"
  else
    mkdir -p "$STATE_DIR"
    setsid nohup "$SCRIPT_PATH" __deadline-guard --cluster-id "$CID" \
      --secs $((MAX_CLUSTER_HOURS * 3600)) --yes >>"$STATE_DIR/deadline-guard.log" 2>&1 &
    DEADLINE_PID=$!
    save_state
    echo "  state: $(state_file)"
  fi
  echo "  deadline: $(date -d "@$DEADLINE_EPOCH" 2>/dev/null || echo "+${MAX_CLUSTER_HOURS}h") (guard pid $DEADLINE_PID — teardown fires automatically)"
  LAUNCH_OK=true
  echo
  echo "Cluster $CID up. Next: tests/cloud_bench_cluster.sh deploy"
}

cmd_deadline_guard() {
  # hidden: sleeps to the deadline then force-teardown. Detached by launch.
  [ -n "$CID_ARG" ] && [ -n "$GUARD_SECS" ] || die "__deadline-guard needs --cluster-id and --secs"
  sleep "$GUARD_SECS"
  echo "[deadline-guard] MAX_CLUSTER_HOURS deadline reached for $CID_ARG — tearing down NOW"
  ASSUME_YES=true
  load_state --cid-only-ok
  cmd_teardown
}

# ---------------------------------------------------------------------------
# deploy — push artifacts + runtime deps, and take Ubuntu's unattended apt
# off the session (the upgrader drained then masked, the timers off; a
# LIVE apt/dpkg transaction is waited for by its lock, bounded, never
# killed — tests/cloud_bench_node_scripts.sh). Deploys only; never builds.
# ---------------------------------------------------------------------------
cmd_deploy() {
  require_local_tools
  load_state --placeholder-ok

  local sqz="$ARTIFACT_DIR/squeezefs"
  local shim="$ARTIFACT_DIR/libsqueezefs_il.so"
  if ! $DRY_RUN; then
    [ -x "$sqz" ] || die "squeezefs artifact missing/not executable: $sqz (build elsewhere: task build:${ARTIFACT_DIR##*/})"
    if [ ! -x "$ELBENCHO_BIN" ]; then
      if [ "$PRESET" = "mw" ]; then
        # The MW row's instrument is the pinned ior the s11-mpiio leg builds
        # ON the client — elbencho only matters if you also run `bench`.
        warn "no dynamic elbencho at $ELBENCHO_BIN — the elbencho battery (bench) is unavailable on this cluster; bench-mw is unaffected"
      else
        die "elbencho artifact missing: $ELBENCHO_BIN (must be a DYNAMIC build)"
      fi
    fi
  fi
  local sqz_sha="dryrun-sha"
  if ! $DRY_RUN; then
    sqz_sha="$(sha256sum "$sqz" | awk '{print $1}')"
  fi

  local idx name ip
  for idx in "${!NODE_NAMES[@]}"; do
    name="${NODE_NAMES[$idx]}"; ip="${NODE_PUB[$idx]}"
    log "deploy: $name ($ip)"
    # Ubuntu's unattended apt is OFF for the session's life (the run-1
    # rig fix; the node script + its bounds are tests/cloud_bench_node_
    # scripts.sh's, pinned by tests/cloud_bench_cluster_units.sh): first,
    # so the package verify below never queues behind a background
    # upgrade for the apt lock.
    remote "$ip" NODE="$name" APT_UPGRADE_WAIT_MAX_S="$APT_UPGRADE_WAIT_MAX_S" APT_UPGRADE_POLL_S="$APT_UPGRADE_POLL_S" \
      <<<"$NODE_APT_HYGIENE_SCRIPT"
    # Packages were installed by cloud-init user-data during boot (in
    # parallel, overlapped with launch waits) — deploy WAITS on cloud-init
    # (bounded) and verifies, falling back to a direct apt only if
    # user-data failed. Never a serial mirror fetch on billed idle time.
    remote "$ip" NEED_MW="$([ "$PRESET" = "mw" ] && echo 1 || echo 0)" <<'EOS'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
cloud-init status --wait --long >/dev/null 2>&1 || true
need="fuse3 nvme-cli"
[ "$NEED_MW" = "1" ] && need="$need openmpi-bin libopenmpi-dev python3 curl gcc make attr"
missing=""
for p in $need; do dpkg -s "$p" >/dev/null 2>&1 || missing="$missing $p"; done
if [ -n "$missing" ]; then
  echo "cloud-init did not deliver:$missing — falling back to direct apt" >&2
  apt-get -qq update
  # shellcheck disable=SC2086
  apt-get -qq install -y $missing >/dev/null
fi
# The TOOLS the rows invoke, by name (a package verified is not a binary
# resolved): die LOUD here, on the deploy, never mid-row on a billing fleet.
tools="fusermount3 nvme python3 tar"
[ "$NEED_MW" = "1" ] && tools="$tools mpirun mpicc curl gcc make getfattr setfattr ping"
for t in $tools; do
  command -v "$t" >/dev/null 2>&1 || { echo "deploy: tool '$t' missing on this node after the package install (the rows need it)" >&2; exit 1; }
done
EOS
    push "$sqz" "$ip" "$REMOTE_DIR/squeezefs"
    # sha256 verification: same-commit artifact on every node (KD-7 spirit)
    remote "$ip" WANT_SHA="$sqz_sha" BIN="$REMOTE_DIR/squeezefs" <<'EOS'
set -euo pipefail
got=$(sha256sum "$BIN" | awk '{print $1}')
[ "$got" = "$WANT_SHA" ] || { echo "sha256 mismatch on deployed squeezefs: got $got want $WANT_SHA" >&2; exit 1; }
"$BIN" --version
EOS
    if [ "$(role_of "$name")" = "client" ] || [ "$(role_of "$name")" = "spare" ]; then
      if $DRY_RUN || [ -x "$ELBENCHO_BIN" ]; then
        push "$ELBENCHO_BIN" "$ip" "$REMOTE_DIR/elbencho"
      fi
      if $DRY_RUN || [ -f "$shim" ]; then
        push "$shim" "$ip" "$REMOTE_DIR/libsqueezefs_il.so"
      else
        warn "no il shim at $shim — il rows unavailable on this cluster (kernel-FUSE rows unaffected)"
      fi
    fi
    # (mw bench prerequisites — openmpi/python3/curl/gcc/make — ride the
    # launch-time cloud-init user-data and the verify above; the former
    # deploy-time apt leg is retired.)
  done
  echo
  echo "Artifacts deployed (sha256 $sqz_sha). Next: tests/cloud_bench_cluster.sh assemble"
}

# ---------------------------------------------------------------------------
# Shared assemble machinery — ONE copy each of the fabric-step scripts, so
# assemble and assemble-mw can never diverge on them. Every script is
# parameterized purely via env and fed to `remote` from a variable, so the
# dry-run print and the real run can never diverge either.
# ---------------------------------------------------------------------------

# Client prologue: unmount every ${MNT}* mount (reverse-sorted so co-writer
# mounts — and the symmetric shape's `-ro` token reader — unmount before the
# authority they dial — this also reaps a prior MW fleet under a plain
# re-assemble), kill stray daemons, then LOUDLY verify
# the disconnect (survivors auto-reconnect to rebuilt shares and poison the
# connect step — same failure mode as tests/cluster_reset.sh).
CLIENT_PROLOGUE_SCRIPT="$(cat <<'EOS'
set -euo pipefail
while read -r m; do
  umount "$m" || umount -l "$m" || true
done < <(awk -v m="$MNT" '$2 == m || index($2, m "-") == 1 {print $2}' /proc/mounts | sort -r)
sleep 2
pkill -f 'squeezefs moun[t]' 2>/dev/null || true
sleep 1
for c in /sys/class/nvme/nvme*/subsysnqn; do
  [ -e "$c" ] || continue
  nqn=$(cat "$c")
  # (`nvmeof disconnect` takes the SUBNQN positionally — the v4 recipe's note.)
  case "$nqn" in "$NQN_PREFIX"*) "$SQZ" nvmeof disconnect "$nqn" || true ;; esac
done
gone=0
for _ in $(seq 1 15); do
  if grep -lq "$NQN_PREFIX" /sys/class/nvme/nvme*/subsysnqn 2>/dev/null; then
    sleep 1
  else
    gone=1; break
  fi
done
[ "$gone" = 1 ] || { echo "could not disconnect all $NQN_PREFIX connections" >&2; exit 1; }
EOS
)"

# Storage-node share: instance-store discovery + `squeezefs nvmeof share`
# (nvmet stack — the product verb writes resv_enable=1 before enable when
# the kernel offers the knob). Instance-store devices carry model "Amazon
# EC2 NVMe Instance Storage"; EBS devices ("Amazon Elastic Block Store")
# are deliberately excluded.
STORAGE_SHARE_SCRIPT="$(cat <<'EOS'
set -euo pipefail
modprobe nvmet nvmet-tcp 2>/dev/null || true
devs=()
for b in /sys/block/nvme*n1; do
  [ -r "$b/device/model" ] || continue
  case "$(tr -d ' ' <"$b/device/model")" in
    *InstanceStorage*) devs+=("/dev/${b##*/}") ;;
  esac
done
[ "${#devs[@]}" -gt 0 ] || { echo "no instance-store NVMe on $NAME (burst/EBS-only instance type?)" >&2; exit 1; }
if [ "$KIND" = mds ]; then devs=("${devs[0]}"); fi
i=0
for d in "${devs[@]}"; do
  nqn="$NQN_BASE:$NAME-d$i"
  "$SQZ" nvmeof unshare "$nqn" 2>/dev/null || true
  wipefs -a "$d" >/dev/null 2>&1 || true
  "$SQZ" nvmeof share "$d" --target-stack nvmet --ip "$PRIV_IP" --subnqn "$nqn"
  echo "SHARED $nqn $d"
  i=$((i + 1))
done
EOS
)"

# Client fabric: local instance store -> /scratch, single-path connect per
# subsystem, optional end-to-end PR verify (MW_PR_VERIFY=1), format, then
# either the plain single mount or none (MW_SKIP_MOUNT=1 — the MW fleet
# recipe owns the mounts). Records the meta URI for the later steps.
CLIENT_FABRIC_SCRIPT="$(cat <<'EOS'
set -euo pipefail
modprobe nvme-tcp 2>/dev/null || true

# local instance store -> /scratch (cache/staging dirs + mountpoint parent)
if ! mountpoint -q /scratch 2>/dev/null; then
  dev=""
  for b in /sys/block/nvme*n1; do
    [ -r "$b/device/model" ] || continue
    case "$(tr -d ' ' <"$b/device/model")" in
      *InstanceStorage*) dev="/dev/${b##*/}"; break ;;
    esac
  done
  [ -n "$dev" ] || { echo "client has no instance-store NVMe for /scratch" >&2; exit 1; }
  mkfs.ext4 -q -F "$dev"
  mkdir -p /scratch
  mount "$dev" /scratch
fi
mkdir -p "$MNT" "$CACHE"

connect_all() { # connect_all <csv nqn@ip> -> resolved /dev list (in csv order)
  local specs="$1" out="" spec nqn ip found s ns b c
  IFS=, read -ra SPECS <<<"$specs"
  for spec in "${SPECS[@]}"; do
    nqn="${spec%@*}"; ip="${spec#*@}"
    live=0
    for c in /sys/class/nvme/nvme*/subsysnqn; do
      [ -e "$c" ] || continue
      [ "$(cat "$c")" = "$nqn" ] && { live=1; break; }
    done
    # >&2: the connect verb prints progress on stdout ("Connecting to
    # NVMe-oF target at ...") and this function's stdout IS the captured
    # device CSV — swallowed chatter reached resv-report as a "device"
    # (the 2026-08-19 mw-preset false "no PR transport" abort).
    [ "$live" = 1 ] || "$SQZ" nvmeof connect --ip "$ip" --subnqn "$nqn" >&2
  done
  for spec in "${SPECS[@]}"; do
    nqn="${spec%@*}"
    found=""
    for _ in $(seq 1 30); do
      for s in /sys/class/nvme-subsystem/nvme-subsys*; do
        [ -e "$s/subsysnqn" ] || continue
        [ "$(cat "$s/subsysnqn")" = "$nqn" ] || continue
        ns=""
        for c in "$s"/nvme*n*; do
          b="${c##*/}"
          [[ "$b" =~ ^nvme[0-9]+n[0-9]+$ ]] && ns="$b" && break
        done
        [ -n "$ns" ] && found="/dev/$ns" && break
      done
      [ -n "$found" ] && break
      sleep 1
    done
    [ -n "$found" ] || { echo "namespace for $nqn never appeared" >&2; exit 1; }
    echo "  $nqn -> $found" >&2
    out="${out:+$out,}$found"
  done
  printf '%s' "$out"
}

meta_devs="$(connect_all "$META_SPECS")"
data_devs="$(connect_all "$DATA_SPECS")"
if [ "${MW_PR_VERIFY:-0}" = 1 ]; then
  # v5-mw end-to-end PR verify: resv-report must SUCCEED on every DATA
  # namespace (the report may be empty; the command failing means the
  # fabric does not transport Persistent Reservations and the S9 arm will
  # refuse the mount).
  IFS=, read -ra DDEVS <<<"$data_devs"
  for d in "${DDEVS[@]}"; do
    nvme resv-report "$d" >/dev/null 2>&1 || {
      echo "nvme resv-report $d FAILED — the fabric does not transport Persistent Reservations end-to-end (storage-node kernel nvmet PR = mainline v6.13+; the mw preset's Ubuntu 26.04 AMI ships it)" >&2
      exit 1
    }
  done
  echo "PR verify: resv-report OK on all ${#DDEVS[@]} data namespaces"
fi
META_URI="sqmeta://$meta_devs"
DATA_URI="sqdata://$data_devs"
if [ "${MW_SKIP_FORMAT:-0}" != 1 ]; then
  # FORMAT_EXTRA_STR: extra `format` flags, comma-separated (the symmetric
  # shape's `--symmetric`); empty = the shipped format verbatim.
  # FORMAT_CACHE=0: a CACHE-LESS set (no --disk-cache-paths — every
  # beyond-inline file a whole striped block, visible to every node at its
  # publish; the shape tests/mw_fleet.sh formats the box and local fleets
  # with, so the symmetric rows price the same DMA the box priced). The
  # default keeps every other preset's staged format verbatim.
  # --force: every assemble is a declared REFORMAT (the typed-YES text), and
  # a RE-assemble meets the previous assemble's superblock — the storage
  # node's `wipefs -a` does not know the METALV01 magic, so `format` refused
  # it without --force (the 2026-09-24 cloud row's assemble attempt 2 met
  # attempt 1's format). --force keeps the live-client refusal: a
  # heartbeat-fresh registration still refuses the format loud.
  read -ra FEXTRA <<<"$(printf '%s' "${FORMAT_EXTRA_STR:-}" | tr ',' ' ')"
  if [ "${FORMAT_CACHE:-1}" = 0 ]; then
    echo "format (CACHE-LESS): $META_URI $DATA_URI --force ${FEXTRA[*]:-}"
    "$SQZ" format "$META_URI" "$DATA_URI" --force "${FEXTRA[@]}"
  else
    echo "format: $META_URI $DATA_URI --disk-cache-paths $CACHE --force ${FEXTRA[*]:-}"
    "$SQZ" format "$META_URI" "$DATA_URI" --disk-cache-paths "$CACHE" --force "${FEXTRA[@]}"
  fi
else
  # a JOINING node: the set is formatted by the manager's node — connect
  # only, and record the SAME meta URI (the devices resolve in csv order,
  # so the URI is the set's on every node)
  echo "connected (no format — a joining node): $META_URI $DATA_URI"
fi
echo "$META_URI" >/etc/squeezefs-bench-meta-uri
echo "$DATA_URI" >/etc/squeezefs-bench-data-uri   # the registrant-count gate reads every namespace
if [ "${MW_SKIP_MOUNT:-0}" != 1 ]; then
  read -ra EXTRA <<<"$(printf '%s' "$MOUNT_EXTRA_STR" | tr ',' ' ')"
  "$SQZ" mount "$META_URI" "$MNT" --daemon "${EXTRA[@]}"
  sleep 2
  mountpoint -q "$MNT" || { echo "mount did not come up" >&2; exit 1; }
fi
EOS
)"

# share_storage_nodes — run STORAGE_SHARE_SCRIPT on every mds/oss node and
# fill SHARED_META_SPECS/SHARED_DATA_SPECS (csv of nqn@private_ip). mds
# shares only its first instance-store device (meta needs little); oss
# shares every instance-store namespace it has (i4i.4xlarge: 1,
# i3en.12xlarge: 4, i4i.2xlarge: 1).
SHARED_META_SPECS=""
SHARED_DATA_SPECS=""
share_storage_nodes() {
  SHARED_META_SPECS=""
  SHARED_DATA_SPECS=""
  SYM_STORAGE_DEVS=""
  local idx name role ip priv shares nqn
  for idx in "${!NODE_NAMES[@]}"; do
    name="${NODE_NAMES[$idx]}"
    role="$(role_of "$name")"
    { [ "$role" = "mds" ] || [ "$role" = "oss" ]; } || continue
    ip="${NODE_PUB[$idx]}"; priv="${NODE_PRIV[$idx]}"
    echo "-- $name ($ip, fabric $priv)"
    if $DRY_RUN; then
      remote "$ip" NAME="$name" KIND="$role" NQN_BASE="$NQN_PREFIX" PRIV_IP="$priv" SQZ="$REMOTE_DIR/squeezefs" \
        <<<"$STORAGE_SHARE_SCRIPT"
      shares="$NQN_PREFIX:$name-d0"     # canned: 1 device/node in dry-run
      [ "$role" = "oss" ] && SYM_STORAGE_DEVS="${SYM_STORAGE_DEVS:+$SYM_STORAGE_DEVS,}$ip:/dev/nvme1n1"
    else
      local share_lines
      share_lines="$(remote "$ip" NAME="$name" KIND="$role" NQN_BASE="$NQN_PREFIX" PRIV_IP="$priv" SQZ="$REMOTE_DIR/squeezefs" \
        <<<"$STORAGE_SHARE_SCRIPT" | awk '/^SHARED /')"
      shares="$(awk '{print $2}' <<<"$share_lines")"
      [ -n "$shares" ] || die "$name shared nothing"
      if [ "$role" = "oss" ]; then
        local dev
        for dev in $(awk '{print $3}' <<<"$share_lines"); do
          SYM_STORAGE_DEVS="${SYM_STORAGE_DEVS:+$SYM_STORAGE_DEVS,}$ip:$dev"
        done
      fi
    fi
    for nqn in $shares; do
      if [ "$role" = "mds" ]; then
        SHARED_META_SPECS="${SHARED_META_SPECS:+$SHARED_META_SPECS,}$nqn@$priv"
      else
        SHARED_DATA_SPECS="${SHARED_DATA_SPECS:+$SHARED_DATA_SPECS,}$nqn@$priv"
      fi
      echo "   shared $nqn @ $priv"
    done
  done
  [ -n "$SHARED_META_SPECS" ] && [ -n "$SHARED_DATA_SPECS" ] \
    || die "assemble produced empty share lists (meta='$SHARED_META_SPECS' data='$SHARED_DATA_SPECS')"
}

# assert_storage_pr — the v5-mw PR ASSERTION, per storage node. The product
# share verb writes resv_enable=1 BEFORE enable when the kernel offers the
# knob; on a kernel WITHOUT nvmet PR it only prints a detection-grade note.
# The S9 multi-writer arm refuses non-PR at mount time — fail HERE at
# target-build time instead, naming the node and the remedy, and
# distinguishing "no knob" (kernel lacks nvmet PR, mainline v6.13+) from
# "knob present but 0".
assert_storage_pr() {
  local idx name role ip
  for idx in "${!NODE_NAMES[@]}"; do
    name="${NODE_NAMES[$idx]}"
    role="$(role_of "$name")"
    { [ "$role" = "mds" ] || [ "$role" = "oss" ]; } || continue
    ip="${NODE_PUB[$idx]}"
    remote "$ip" NQN_PREFIX="$NQN_PREFIX" NODE="$name" <<'EOS' \
      || die "storage node $name: PR assertion failed (see above) — the S9 multi-writer arm would refuse this substrate"
set -u
cfg=/sys/kernel/config/nvmet
fail=0; n=0
for s in "$cfg"/subsystems/"$NQN_PREFIX"*; do
  [ -d "$s" ] || continue
  n=$((n + 1))
  r="$s/namespaces/1/resv_enable"
  if [ ! -f "$r" ]; then
    echo "FATAL[$NODE]: $(basename "$s"): nvmet exposes NO resv_enable knob — this node's kernel $(uname -r) lacks nvmet Persistent Reservations (mainline v6.13+; the mw preset's default Ubuntu 26.04 AMI ships it — check AMI_SSM_PARAM). Update the NODE kernel/nvmet module." >&2
    fail=1
  elif [ "$(cat "$r")" != "1" ]; then
    echo "FATAL[$NODE]: $(basename "$s"): resv_enable=$(cat "$r") (want 1 — the product share verb writes it before enable when the knob exists)" >&2
    fail=1
  fi
done
[ "$n" -gt 0 ] || { echo "FATAL[$NODE]: no prefixed subsystems found after share" >&2; exit 1; }
[ "$fail" -eq 0 ] && echo "  PR assert: $n namespace(s) resv_enable=1 on $NODE"
exit "$fail"
EOS
  done
}

# verify_build_commit <client-ip> <mounts-csv> — every mounted daemon must
# be the deployed artifact: the .stats build_commit must appear in
# `squeezefs --version` of the deployed binary (KD-7 spirit).
verify_build_commit() {
  remote "$1" SQZ="$REMOTE_DIR/squeezefs" MNTS="$2" <<'EOS'
set -euo pipefail
ver="$("$SQZ" --version)"
IFS=, read -ra MS <<<"$MNTS"
for m in "${MS[@]}"; do
  commit="$(grep -oE '"build_commit": *"[0-9a-f]+"' "$m/.stats" | grep -oE '[0-9a-f]{7,40}' | head -1)"
  [ -n "$commit" ] || { echo "no build_commit in $m/.stats" >&2; exit 1; }
  case "$ver" in
    *"$commit"*) echo "build_commit OK: $m -> $commit" ;;
    *) echo "build_commit MISMATCH on $m: mounted daemon reports $commit but deployed binary is '$ver'" >&2; exit 1 ;;
  esac
done
echo "build_commit ritual: ${#MS[@]} mount(s) verified against '$ver'"
EOS
}

# ---------------------------------------------------------------------------
# assemble — instance-store discovery, nvmet shares, single-path connect,
# format, mount, build_commit ritual. Idempotent: re-running unshares,
# disconnects and rebuilds from scratch (fresh format — data is destroyed).
# ---------------------------------------------------------------------------
cmd_assemble() {
  require_local_tools
  load_state --placeholder-ok
  confirm "assemble REFORMATS the cluster volumes (any prior benchmark data on $CID is destroyed)."

  local client_ip
  client_ip="$(node_pub client0)"

  log "assemble 1/4: client prologue — unmount + disconnect survivors (idempotency)"
  remote "$client_ip" SQZ="$REMOTE_DIR/squeezefs" MNT="$MOUNTPOINT" NQN_PREFIX="$NQN_PREFIX" \
    <<<"$CLIENT_PROLOGUE_SCRIPT"

  log "assemble 2/4: storage nodes — instance-store NVMe discovery + nvmet share"
  share_storage_nodes

  log "assemble 3/4: client — single-path connect, format, mount"
  # Cloud note: these instances have ONE NIC, so this is a SINGLE-PATH
  # connect per subsystem — no second-path loop and no iopolicy step (the
  # round-robin iopolicy write in tests/cluster_reset.sh only matters with
  # two fabric paths; with one path it is a no-op, so it is skipped here
  # on purpose).
  remote "$client_ip" \
    SQZ="$REMOTE_DIR/squeezefs" MNT="$MOUNTPOINT" CACHE="$CACHE_DIR" \
    META_SPECS="$SHARED_META_SPECS" DATA_SPECS="$SHARED_DATA_SPECS" \
    MOUNT_EXTRA_STR="$(printf '%s' "$MOUNT_EXTRA" | tr ' ' ',')" \
    <<<"$CLIENT_FABRIC_SCRIPT"

  log "assemble 4/4: build_commit verification ritual"
  verify_build_commit "$client_ip" "$MOUNTPOINT"
  echo
  echo "Cluster assembled and mounted at $MOUNTPOINT on client0. Next: tests/cloud_bench_cluster.sh bench"
}

# ---------------------------------------------------------------------------
# assemble-mw — the MULTI-WRITER fleet shape (PRESET=mw): the same fabric
# steps as assemble (prologue, instance-store shares, single-path connect,
# format with client instance-store staging), DIVERGING at the mount into
# the tests/cluster_reset_v5_mw.sh recipe — 1 authority at $MOUNTPOINT +
# $MW_COWRITERS co-writers at $MOUNTPOINT-cw1..K, CO-LOCATED on client0
# (default NVMe host identity: no per-mount hostnqn/hostid, no
# fabric_endpoint records, no host-scoped-subsystem kernel — patch 0030 is
# multi-identity-only). Two hard kernel floors, both probed loud (never
# trusted from the AMI): client FUSE-over-io_uring (mainline v6.14+) and
# storage-node nvmet Persistent Reservations (v6.13+ — the S9 arm refuses
# non-PR substrates). Idempotent: re-running reaps the fleet and rebuilds
# from scratch (fresh format — data is destroyed).
# ---------------------------------------------------------------------------
cmd_assemble_mw() {
  require_local_tools
  load_state --placeholder-ok
  mw_shape_check
  local fleet_share
  fleet_share="${MW_FLEET_SHARE:-$((1 + MW_COWRITERS))}"
  confirm "assemble-mw REFORMATS the cluster volumes (any prior benchmark data on $CID is destroyed) and mounts 1 authority + $MW_COWRITERS co-writer daemons on client0."

  local client_ip
  client_ip="$(node_pub client0)"

  log "assemble-mw 1/6: client kernel floor — FUSE-over-io_uring (v6.14+)"
  remote "$client_ip" <<'EOS'
set -euo pipefail
modprobe fuse 2>/dev/null || true
[ -e /sys/module/fuse/parameters/enable_uring ] || {
  echo "FATAL: client kernel $(uname -r) lacks FUSE-over-io_uring (no /sys/module/fuse/parameters/enable_uring; mainline v6.14+ needed) — every SqueezeFS mount requires the transport. Remedy: launch with the mw preset's default Ubuntu 26.04 AMI (AMI_SSM_PARAM) or any v6.14+ kernel." >&2
  exit 1
}
echo "client kernel $(uname -r): fuse.enable_uring present"
EOS

  log "assemble-mw 2/6: client prologue — unmount fleet + disconnect survivors (idempotency)"
  remote "$client_ip" SQZ="$REMOTE_DIR/squeezefs" MNT="$MOUNTPOINT" NQN_PREFIX="$NQN_PREFIX" \
    <<<"$CLIENT_PROLOGUE_SCRIPT"

  log "assemble-mw 3/6: storage nodes — instance-store share + nvmet PR assert (resv_enable=1; v6.13+ floor)"
  share_storage_nodes
  assert_storage_pr

  log "assemble-mw 4/6: client — single-path connect, PR verify (nvme resv-report per data namespace), format"
  remote "$client_ip" \
    SQZ="$REMOTE_DIR/squeezefs" MNT="$MOUNTPOINT" CACHE="$CACHE_DIR" \
    META_SPECS="$SHARED_META_SPECS" DATA_SPECS="$SHARED_DATA_SPECS" \
    MOUNT_EXTRA_STR="$(printf '%s' "$MW_MOUNT_EXTRA" | tr ' ' ',')" \
    MW_PR_VERIFY=1 MW_SKIP_MOUNT=1 \
    <<<"$CLIENT_FABRIC_SCRIPT"

  log "assemble-mw 5/6: mount the multi-writer fleet — authority + $MW_COWRITERS co-writers (FLEET_SHARE=$fleet_share, MW port $MW_PORT)"
  # The v5-mw §5 recipe, co-located on client0: authority phase-1 arm (no
  # roster) -> per-co-writer enrollment-id probes (each mount attempt is
  # REFUSED at rung 3; the refusal prints the durable id the roster needs;
  # gather_admission mutates nothing before rung 5, so the probe is
  # side-effect-free) -> re-arm the authority with the harvested roster
  # (enrollment is the AUTHORITY's durable act, a new era) -> mount the
  # admitted co-writers. SUDO_* is scrubbed from every daemon launch so the
  # daemon posture (mount ownership, admin-lane identity) is
  # root-deterministic regardless of sudo-vs-root-shell (the admin lane
  # admits peercred uid 0 — src/ipc_host.rs). Every mount readiness-gates on
  # its own log lines + .stats posture — a silently-degraded arm is
  # contractually impossible.
  remote "$client_ip" \
    SQZ="$REMOTE_DIR/squeezefs" MNT="$MOUNTPOINT" \
    COWRITERS="$MW_COWRITERS" FLEET_SHARE="$fleet_share" \
    MW_PORT="$MW_PORT" MEMBERSHIP_BIND="$MW_MEMBERSHIP_BIND" \
    RANGE_CUSTODY="$MW_RANGE_CUSTODY" \
    MOUNT_EXTRA_STR="$(printf '%s' "$MW_MOUNT_EXTRA" | tr ' ' ',')" <<'EOS'
set -euo pipefail
die() { echo "FATAL: $*" >&2; exit 1; }
[ -s /etc/squeezefs-bench-meta-uri ] || die "no recorded meta URI — the connect/format step did not run"
META_URI="$(cat /etc/squeezefs-bench-meta-uri)"
read -ra EXTRA <<<"$(printf '%s' "$MOUNT_EXTRA_STR" | tr ',' ' ')"
AUTH_LOG=/tmp/sqz-mw-authority.log

# One flattened stats-inode field (the JSON nests under "metrics") — the
# v5-mw / mw_fleet stat_field helper verbatim.
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

wait_for() { # description tries cmd...
  local what="$1" tries="$2" i
  shift 2
  for ((i = 0; i < tries; i++)); do
    "$@" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  die "timed out waiting for $what"
}

poll_stat() { # mountpoint key want tries what
  local mnt="$1" key="$2" want="$3" tries="$4" what="$5" v i
  v=""
  for ((i = 0; i < tries; i++)); do
    v="$(stat_field "$mnt" "$key" 2>/dev/null || true)"
    [ "$v" = "$want" ] && return 0
    sleep 0.5
  done
  die "$what: $key='$v' (want $want)"
}

unmount_and_reap() { # mountpoint (idempotent — v5-mw verbatim)
  local mnt="$1" pid t
  pid="$(pgrep -f "squeezefs mount .* $mnt( |$)" | head -1 || true)"
  if awk -v m="$mnt" '$2==m {f=1} END {exit !f}' /proc/mounts; then
    env -u SUDO_UID -u SUDO_GID -u SUDO_USER "$SQZ" umount "$mnt" >/dev/null 2>&1 || true
  fi
  if awk -v m="$mnt" '$2==m {f=1} END {exit !f}' /proc/mounts; then
    umount -l "$mnt" 2>/dev/null || true
  fi
  wait_for "unmount of $mnt" 60 bash -c "! awk -v m='$mnt' '\$2==m {f=1} END {exit !f}' /proc/mounts"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    for ((t = 0; t < 20; t++)); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.5
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
}

MW_ENDPOINT=""
mount_authority() { # [roster] — phase-1 arm, or the roster re-arm (a new era)
  local roster="${1:-}"
  local env_args=(env -u SUDO_UID -u SUDO_GID -u SUDO_USER
    "SQUEEZEFS_FLEET_SHARE=$FLEET_SHARE"
    "SQUEEZEFS_IPC_ALLOW_DEV=1"
    "SQUEEZEFS_MEMBERSHIP_BIND=$MEMBERSHIP_BIND"
    "SQUEEZEFS_MULTI_WRITER=1"
    "SQUEEZEFS_MW_BIND=0.0.0.0:$MW_PORT")
  [ -n "$roster" ] && env_args+=("SQUEEZEFS_MW_MEMBERS=$roster")
  mkdir -p "$MNT"
  "${env_args[@]}" "$SQZ" mount "$META_URI" "$MNT" \
    --daemon "${EXTRA[@]}" --log-file "$AUTH_LOG" \
    >/tmp/sqz-mw-authority.mount.out 2>&1 ||
    die "authority mount failed: $(cat /tmp/sqz-mw-authority.mount.out)"
  wait_for "authority mountpoint" 120 mountpoint -q "$MNT"
  wait_for "authority stats inode" 120 test -s "$MNT/.stats"
  # Rung-8 engagement gates (mw_fleet mount_member verbatim): the WERO hold
  # must stand (fence-mode gauge + the acquire log line) and the S6 plane
  # must own — a silently-degraded arm is contractually impossible, so a
  # miss here is a refusal we somehow did not see; die loud either way.
  poll_stat "$MNT" data_plane_fence_mode 1 120 \
    "authority: the S7 WERO hold did not engage (log: $AUTH_LOG)"
  grep -q "data-plane WERO (rtype 3) acquired" "$AUTH_LOG" ||
    die "authority log carries no 'data-plane WERO (rtype 3) acquired' line (log: $AUTH_LOG)"
  poll_stat "$MNT" membership_mode owner 120 \
    "authority: the S6 membership plane did not engage (log: $AUTH_LOG)"
  MW_ENDPOINT="$(sed -n 's/.*MULTI-WRITER ARMED (DLM S9) on \(.*\): era.*/\1/p' "$AUTH_LOG" | tail -1)"
  [ -n "$MW_ENDPOINT" ] ||
    die "authority log carries no 'MULTI-WRITER ARMED (DLM S9) on <endpoint>' line (log: $AUTH_LOG)"
  echo "  authority up at $MNT (WERO held, membership owner, MW endpoint $MW_ENDPOINT)"
}

cowriter_env() { # -> the co-writer daemon env (v5-mw / mw_fleet verbatim)
  echo env -u SUDO_UID -u SUDO_GID -u SUDO_USER \
    "SQUEEZEFS_FLEET_SHARE=$FLEET_SHARE" \
    "SQUEEZEFS_IPC_ALLOW_DEV=1" \
    "SQUEEZEFS_MULTI_WRITER=1" \
    "SQUEEZEFS_MW_ROLE=co-writer" \
    "SQUEEZEFS_MW_AUTHORITY=$MW_ENDPOINT" \
    "$([ "$RANGE_CUSTODY" = "1" ] && echo SQUEEZEFS_RANGE_CUSTODY=1 || echo SQUEEZEFS_RANGE_CUSTODY=0)"
}

probe_cowriter_id() { # mountpoint probe-out probe-log -> echoes the enrollment id
  local mnt="$1" out="$2" plog="$3" id
  mkdir -p "$mnt"
  # A co-writer mount against a roster that does not name it is REFUSED at
  # rung 3, and the refusal prints this mountpoint's durable enrollment id
  # (KD-MW-2 node_{16 hex}.m{8 hex}). gather_admission mutates nothing
  # before rung 5 — the probe is side-effect-free. cowriter_env is a
  # deliberate word list.
  if $(cowriter_env) "$SQZ" mount "$META_URI" "$mnt" \
    --daemon "${EXTRA[@]}" --log-file "$plog" >"$out" 2>&1; then
    die "co-writer PROBE mount at $mnt was ADMITTED against an empty roster — rung 3 did not engage (out: $out)"
  fi
  id="$(grep -o "Add 'node_[0-9a-f.m]*'" "$out" | head -1 | sed "s/^Add '//; s/'$//")"
  [ -n "$id" ] || die "co-writer probe refusal at $mnt carries no enrollment id (want the rung-3 \"Add 'node_...'\" remedy; out: $out)"
  echo "$id"
}

mount_cowriter() { # idx
  local i="$1" mnt clog
  mnt="$MNT-cw$i"
  clog="/tmp/sqz-mw-cw$i.log"
  mkdir -p "$mnt"
  # Rung-9 engagement (v5-mw verbatim): the five-rung ladder ADMITTED and
  # the mount is the co-writer posture, never a silently-degraded one.
  $(cowriter_env) "$SQZ" mount "$META_URI" "$mnt" \
    --daemon "${EXTRA[@]}" --log-file "$clog" \
    >"/tmp/sqz-mw-cw$i.mount.out" 2>&1 ||
    die "co-writer $i mount failed: $(cat "/tmp/sqz-mw-cw$i.mount.out")"
  wait_for "co-writer $i mountpoint" 120 mountpoint -q "$mnt"
  wait_for "co-writer $i stats inode" 120 test -s "$mnt/.stats"
  grep -q "CO-WRITER ADMITTED" "$clog" ||
    die "co-writer $i log carries no 'CO-WRITER ADMITTED' line — the admission ladder did not engage (log: $clog)"
  poll_stat "$mnt" mount_posture co-writer 40 "co-writer $i posture (log: $clog)"
  poll_stat "$mnt" membership_mode member 120 "co-writer $i: the S6 join did not engage (log: $clog)"
  echo "  co-writer $i up at $mnt (ADMITTED, posture=co-writer, membership=member)"
}

mount_authority
ROSTER=""
for ((i = 1; i <= COWRITERS; i++)); do
  id="$(probe_cowriter_id "$MNT-cw$i" "/tmp/sqz-mw-cw$i.probe.out" "/tmp/sqz-mw-cw$i.probe.log")"
  echo "  co-writer $i enrollment id harvested: $id"
  ROSTER="${ROSTER:+$ROSTER,}$id"
done
echo "  re-arming the authority with the roster (a new era): $ROSTER"
unmount_and_reap "$MNT"
mount_authority "$ROSTER"
for ((i = 1; i <= COWRITERS; i++)); do
  mount_cowriter "$i"
done
MOUNT_LIST="$MNT"
for ((i = 1; i <= COWRITERS; i++)); do MOUNT_LIST="$MOUNT_LIST,$MNT-cw$i"; done
echo "MW fleet ready: SQZ_MWMATRIX_MOUNTS=$MOUNT_LIST"
EOS

  log "assemble-mw 6/6: build_commit verification ritual across ALL mounts"
  local mounts_csv i
  mounts_csv="$MOUNTPOINT"
  for ((i = 1; i <= MW_COWRITERS; i++)); do mounts_csv="$mounts_csv,$MOUNTPOINT-cw$i"; done
  verify_build_commit "$client_ip" "$mounts_csv"
  echo
  echo "MW fleet assembled: authority $MOUNTPOINT + $MW_COWRITERS co-writers ($MOUNTPOINT-cw1..cw$MW_COWRITERS) on client0."
  echo "Next: PRESET=mw tests/cloud_bench_cluster.sh bench-mw"
}

# ---------------------------------------------------------------------------
# assemble-sym — the SYMMETRIC fleet shape (PRESET=mw SYMMETRIC=1): the same
# fabric steps as assemble-mw, DIVERGING at the format into
# `format --symmetric` and at the mount into ONE symmetric writer PER CLIENT
# NODE — the MANAGER on client0 (the D0 winner), a JOINED writer on
# client1..N-1 through the join ladder (design-symmetric-metadata §7.3; PR
# 12b's N-daemon posture on N real hosts), an optional `--read-only` TOKEN
# reader on client0. Every node is its own registrant (the REMOTE posture):
# its nvme-cli host identity, asserted DISTINCT across the client nodes —
# and before it its /etc/machine-id, the daemon's node token's source
# (a baked AMI's clone is regenerated). Idempotent: re-running reaps every
# mount and rebuilds from scratch (fresh format — data is destroyed).
# ---------------------------------------------------------------------------
SYM_READER_MNT="$MOUNTPOINT-ro"

sym_client_names() { # every client node name, client0 first
  local idx
  for idx in "${!NODE_NAMES[@]}"; do
    [ "$(role_of "${NODE_NAMES[$idx]}")" = "client" ] && echo "${NODE_NAMES[$idx]}"
  done
}

sym_shape_check() {
  [ "$SYMMETRIC" = "1" ] || die "this subcommand is the symmetric shape's — pass SYMMETRIC=1 (or --symmetric) with PRESET=mw N_CLIENT=<writer nodes>"
  [[ "$SYM_PORT" =~ ^[0-9]+$ ]] && [ "$SYM_PORT" -ge 1024 ] && [ "$SYM_PORT" -le 65535 ] \
    || die "SYM_PORT must be a port in 1024..65535 (got: $SYM_PORT)"
  [ "$SYM_TOKEN_READER" = "0" ] || [ "$SYM_TOKEN_READER" = "1" ] || die "SYM_TOKEN_READER must be 0 or 1"
  [ "$SYM_ARM_A" = "0" ] || die "SYM_ARM_A=1: the design's 'vs today' A arm (authority + co-writers on N nodes) is NOT BUILT in PR 15 — this rig has no per-node co-writer recipe (the co-located v5-mw recipe is client0-only); the B-only law rows are the minimum. Run with SYM_ARM_A=0."
}

# The row set's preconditions, decidable on the operator box: the row
# words, the gate-3b shape (>= 2 JOINED writers), the corpus, the driver,
# the numbers. Run at launch (before money) and again at bench-sym.
sym_preflight() {
  sym_shape_check
  local r
  for r in ${SYM_ROWS//,/ }; do
    case "$r" in tarx | scale | shared) ;; *) die "SYM_ROWS names an unknown row set '$r' (tarx|shared|scale)" ;; esac
  done
  if [[ ",$SYM_ROWS," == *,shared,* ]] && [ "$N_CLIENT" -lt 3 ]; then
    die "gate 3b (sym-shared-dir) needs >= 2 JOINED writers — the flip triggers on foreign creates from MORE THAN ONE creator and the holder is a joined writer — so N_CLIENT >= 3 (have $N_CLIENT); use N_CLIENT=3 or SYM_ROWS=tarx,scale"
  fi
  if [[ ",$SYM_ROWS," == *,tarx,* ]]; then
    if [ -n "$SYM_TARBALL" ]; then
      $DRY_RUN || [ -s "$SYM_TARBALL" ] || die "SYM_TARBALL=$SYM_TARBALL is missing/empty"
    elif [ -n "$SYM_TAR_SRC" ]; then
      $DRY_RUN || [ -d "$SYM_TAR_SRC" ] || die "SYM_TAR_SRC=$SYM_TAR_SRC is not a directory"
    else
      die "the gate-2 row needs the corpus — SYM_TAR_SRC=<linux>/fs (the box used linux-7.2.3/fs, 2,468 entries) or SYM_TARBALL=<file>; or drop tarx from SYM_ROWS"
    fi
  fi
  [ -x "$(dirname "$SCRIPT_PATH")/cloud_sym_rows.sh" ] || die "row driver missing: $(dirname "$SCRIPT_PATH")/cloud_sym_rows.sh"
  [ -r "$(dirname "$SCRIPT_PATH")/sym_rows_lib.sh" ] || die "law lib missing: $(dirname "$SCRIPT_PATH")/sym_rows_lib.sh"
  [ -r "$(dirname "$SCRIPT_PATH")/mdstorm.c" ] || die "tests/mdstorm.c missing (the create storm the driver compiles on every writer node)"
  [[ "$SYM_RT" =~ ^[0-9]+$ ]] || die "SYM_RT must be seconds (got: $SYM_RT)"
  [[ "$SYM_FILES" =~ ^[0-9]+$ ]] && [ "$SYM_FILES" -ge 100 ] || die "SYM_FILES must be an integer >= 100 (got: $SYM_FILES)"
  [[ "$SYM_THREADS" =~ ^[0-9]+$ ]] && [ "$SYM_THREADS" -ge 1 ] || die "SYM_THREADS must be an integer >= 1 (got: $SYM_THREADS)"
  [[ "$SYM_INGEST_MB" =~ ^[0-9]+$ ]] && [ "$SYM_INGEST_MB" -ge 4 ] && [ $((SYM_INGEST_MB % 4)) -eq 0 ] \
    || die "SYM_INGEST_MB must be a multiple of 4 MiB >= 4 (got: $SYM_INGEST_MB)"
  [[ "$SYM_INGEST_CAP_MB" =~ ^[0-9]+$ ]] && [ "$SYM_INGEST_CAP_MB" -ge "$SYM_INGEST_MB" ] \
    || die "SYM_INGEST_CAP_MB must be an integer >= SYM_INGEST_MB (got: $SYM_INGEST_CAP_MB)"
  [ -z "$SYM_SCALE_NS" ] || [[ "$SYM_SCALE_NS" =~ ^[0-9]+(,[0-9]+)*$ ]] || die "SYM_SCALE_NS must be a comma-separated N list (got: $SYM_SCALE_NS)"
}

# The one flattened-stats reader the remote gates use (the JSON nests under
# "metrics") — v5-mw / mw_fleet's stat_field, plus a per-volume FIRST and
# an ALL-EQUAL face for the symmetric families (JSON arrays per volume).
SYM_STAT_HELPERS="$(cat <<'EOS'
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
stat_first() { # mountpoint key -> the first per-volume element (a scalar is itself)
  cat "$1/.stats" | python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(sys.stdin)
v = flat(root.get("metrics", root), {}).get(sys.argv[1], "")
if isinstance(v, list): v = v[0] if v else ""
print(v)' "$2"
}
stat_all_eq() { # mountpoint key want -> 1|0
  cat "$1/.stats" | python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(sys.stdin)
v = flat(root.get("metrics", root), {}).get(sys.argv[1], None)
vals = v if isinstance(v, list) else [v]
print(1 if vals and all(str(x) == sys.argv[2] for x in vals) else 0)' "$2" "$3"
}
wait_for() { # description tries cmd...
  local what="$1" tries="$2" i
  shift 2
  for ((i = 0; i < tries; i++)); do
    "$@" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  die "timed out waiting for $what"
}
poll_stat() { # mountpoint key want tries what
  local mnt="$1" key="$2" want="$3" tries="$4" what="$5" v i
  v=""
  for ((i = 0; i < tries; i++)); do
    v="$(stat_field "$mnt" "$key" 2>/dev/null || true)"
    [ "$v" = "$want" ] && return 0
    sleep 0.5
  done
  die "$what: $key='$v' (want $want)"
}
poll_all_eq() { # mountpoint key want tries what
  local mnt="$1" key="$2" want="$3" tries="$4" what="$5" i
  for ((i = 0; i < tries; i++)); do
    [ "$(stat_all_eq "$mnt" "$key" "$want" 2>/dev/null || echo 0)" = "1" ] && return 0
    sleep 0.5
  done
  die "$what: $key != $want on every volume (last: $(stat_field "$mnt" "$key" 2>/dev/null))"
}
unmount_and_reap() { # mountpoint (idempotent — v5-mw verbatim)
  local mnt="$1" pid t
  pid="$(pgrep -f "squeezefs mount .* $mnt( |$)" | head -1 || true)"
  if awk -v m="$mnt" '$2==m {f=1} END {exit !f}' /proc/mounts; then
    env -u SUDO_UID -u SUDO_GID -u SUDO_USER "$SQZ" umount "$mnt" >/dev/null 2>&1 || true
  fi
  if awk -v m="$mnt" '$2==m {f=1} END {exit !f}' /proc/mounts; then
    umount -l "$mnt" 2>/dev/null || true
  fi
  wait_for "unmount of $mnt" 240 bash -c "! awk -v m='$mnt' '\$2==m {f=1} END {exit !f}' /proc/mounts"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    for ((t = 0; t < 40; t++)); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.5
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
}
EOS
)"

# The MANAGER mount on client0: the D0 winner of an armed set IS the
# manager (KD-SYM-3). The knob-armed shape — SQUEEZEFS_SYMMETRIC_META=1,
# NO posture knob (SQUEEZEFS_MULTI_WRITER / MW_ROLE / MW_AUTHORITY /
# MW_MEMBERS are RETIRED spellings under the plane), the membership shard
# at `auto` (rung 3), the listener at the node's private ip + SYM_PORT
# (rung 7 — advertised verbatim into its claim-set entry: what every
# joiner's `resolve_holder_endpoint` reads). Engagement: the ladder's own
# log line, `mount_posture writer`, `manager_lease held` + `symmetric_meta
# 1` on every volume, `membership_mode owner`, `symmetric_join` published,
# `writer_guard_mode flock+pr` (the metadata PR the death path's preempt
# fences), `data_plane_fence_mode 1` (rung 4's WERO).
SYM_MOUNT_MANAGER_SCRIPT="$(cat <<'EOS'
set -euo pipefail
die() { echo "FATAL: $*" >&2; exit 1; }
[ -s /etc/squeezefs-bench-meta-uri ] || die "no recorded meta URI — the connect/format step did not run"
META_URI="$(cat /etc/squeezefs-bench-meta-uri)"
read -ra EXTRA <<<"$(printf '%s' "$MOUNT_EXTRA_STR" | tr ',' ' ')"
LOG=/tmp/sqz-sym-manager.log
# --log-file opens O_APPEND: rotate the previous incarnation's log aside so
# the log-line gates below read THIS mount's lines only (a rejoin through
# sym-hook, a re-assemble — review round 1, Issue 7); bench-sym pulls
# `sqz-sym-*.log*`.
[ -f "$LOG" ] && mv "$LOG" "$LOG.$(date +%s)"
mkdir -p "$MNT"
env -u SUDO_UID -u SUDO_GID -u SUDO_USER \
  "SQUEEZEFS_IPC_ALLOW_DEV=1" \
  "SQUEEZEFS_SYMMETRIC_META=1" \
  "SQUEEZEFS_MEMBERSHIP_BIND=$MEMBERSHIP_BIND" \
  "SQUEEZEFS_MW_BIND=$PRIV_IP:$PORT" \
  "$SQZ" mount "$META_URI" "$MNT" --daemon "${EXTRA[@]}" --log-file "$LOG" \
  >/tmp/sqz-sym-manager.mount.out 2>&1 ||
  die "manager mount failed: $(cat /tmp/sqz-sym-manager.mount.out)"
wait_for "manager mountpoint" 240 mountpoint -q "$MNT"
wait_for "manager stats inode" 240 test -s "$MNT/.stats"
grep -q "SYMMETRIC WRITER JOINED" "$LOG" ||
  die "manager log carries no 'SYMMETRIC WRITER JOINED' line — the join ladder did not run (log: $LOG)"
poll_stat "$MNT" mount_posture writer 60 "manager posture (log: $LOG)"
poll_all_eq "$MNT" manager_lease held 120 "manager: manager_lease (the D0 winner IS the manager, KD-SYM-3; log: $LOG)"
poll_all_eq "$MNT" symmetric_meta 1 60 "manager: the symmetric plane did not arm (log: $LOG)"
poll_all_eq "$MNT" writer_guard_mode flock+pr 60 "manager: writer_guard_mode (the metadata PR is not held — a non-PR substrate?; log: $LOG)"
poll_stat "$MNT" data_plane_fence_mode 1 240 "manager: rung 4's data WERO did not engage (log: $LOG)"
poll_stat "$MNT" membership_mode owner 240 "manager: the S6 shard did not arm (log: $LOG)"
ep="$(stat_field "$MNT" symmetric_join.endpoint)"
[ -n "$ep" ] || die "manager: symmetric_join.endpoint is empty — rung 7 published no listener (log: $LOG)"
case "$ep" in "$PRIV_IP:"*) : ;; *) die "manager: symmetric_join.endpoint='$ep' does not carry the node's private ip $PRIV_IP — the joiners would dial the wrong address" ;; esac
echo "MANAGER_ENDPOINT $ep"
echo "  manager up at $MNT (posture writer, lease held, WERO held, membership owner, serving on $ep)"
EOS
)"

# A JOINED writer on client1..N-1: the fifth door over the wire (PR 12b) —
# no posture knob, the join target resolved off DURABLE state (the
# manager's heartbeat-fresh claim + its published listener + the set's
# secret). The REMOTE posture: this node holds no local flock and its boot
# id is not the claim's, so rung 4 REGISTERS under the manager's standing
# hold with this node's own identity (`joined_registrant_posture =
# registrant` — `adopted` here would mean the joiner believed itself
# co-located, a product finding on this venue). Engagement: the joined
# door's log line, `mount_posture writer`, `joined_appender_id ≥ 1`,
# `manager_lease peer:…`, `slot_leases_held ≥ 1`, `membership_mode
# member`, `symmetric_join` published with this node's endpoint.
SYM_MOUNT_JOINER_SCRIPT="$(cat <<'EOS'
set -euo pipefail
die() { echo "FATAL: $*" >&2; exit 1; }
[ -s /etc/squeezefs-bench-meta-uri ] || die "no recorded meta URI — the connect step did not run on this node"
META_URI="$(cat /etc/squeezefs-bench-meta-uri)"
read -ra EXTRA <<<"$(printf '%s' "$MOUNT_EXTRA_STR" | tr ',' ' ')"
LOG=/tmp/sqz-sym-joiner.log
# --log-file opens O_APPEND: rotate the previous incarnation's log aside so
# the log-line gates below read THIS mount's lines only (a rejoin through
# sym-hook, a re-assemble — review round 1, Issue 7); bench-sym pulls
# `sqz-sym-*.log*`.
[ -f "$LOG" ] && mv "$LOG" "$LOG.$(date +%s)"
mkdir -p "$MNT"
env -u SUDO_UID -u SUDO_GID -u SUDO_USER \
  "SQUEEZEFS_IPC_ALLOW_DEV=1" \
  "SQUEEZEFS_SYMMETRIC_META=1" \
  "SQUEEZEFS_MW_BIND=$PRIV_IP:$PORT" \
  "$SQZ" mount "$META_URI" "$MNT" --daemon "${EXTRA[@]}" --log-file "$LOG" \
  >/tmp/sqz-sym-joiner.mount.out 2>&1 ||
  die "joined writer mount failed on $NODE: $(cat /tmp/sqz-sym-joiner.mount.out)"
wait_for "joiner mountpoint" 240 mountpoint -q "$MNT"
wait_for "joiner stats inode" 240 test -s "$MNT/.stats"
grep -q "mounted as a JOINED symmetric appender" "$LOG" ||
  die "$NODE: log carries no 'mounted as a JOINED symmetric appender' line — the joined door did not engage (log: $LOG)"
grep -q "SYMMETRIC WRITER JOINED as a NON-MANAGER appender" "$LOG" ||
  die "$NODE: log carries no 'SYMMETRIC WRITER JOINED as a NON-MANAGER appender' line — rungs 4/7 did not arm (log: $LOG)"
poll_stat "$MNT" mount_posture writer 60 "$NODE posture (log: $LOG)"
jid="$(stat_first "$MNT" joined_appender_id)"
[ "${jid:-0}" -ge 1 ] 2>/dev/null || die "$NODE: joined_appender_id='$jid' (want ≥ 1) — log: $LOG"
jpost="$(stat_first "$MNT" joined_registrant_posture)"
[ "$jpost" = "registrant" ] ||
  die "$NODE: joined_registrant_posture='$jpost' (want registrant — the REMOTE posture: this node is its own registrant under the manager's hold; 'adopted' means the joiner believed itself co-located with the manager, 'detection' a non-PR namespace) — log: $LOG"
lease="$(stat_first "$MNT" manager_lease)"
case "$lease" in peer:*) : ;; *) die "$NODE: manager_lease='$lease' (want peer:…) — log: $LOG" ;; esac
held="$(stat_first "$MNT" slot_leases_held)"
[ "${held:-0}" -ge 1 ] 2>/dev/null || die "$NODE: slot_leases_held='$held' (want ≥ 1) — log: $LOG"
poll_stat "$MNT" membership_mode member 240 "$NODE: the S6 join did not engage (log: $LOG)"
ep="$(stat_field "$MNT" symmetric_join.endpoint)"
case "$ep" in "$PRIV_IP:"*) : ;; *) die "$NODE: symmetric_join.endpoint='$ep' does not carry this node's private ip $PRIV_IP" ;; esac
echo "  joined writer up at $MNT on $NODE (appender $jid, manager_lease=$lease, $held slot(s), registrant=$jpost, serving on $ep)"
EOS
)"

# A `--read-only` TOKEN reader (PR 5's §5.7.2 posture): member-reader +
# token client, every kernel TTL derived to 0, the holders dialed off
# durable state. Engagement: the token arm's own line and
# `reader_staleness_bound_ms == 0` (R-SYM-4).
SYM_MOUNT_READER_SCRIPT="$(cat <<'EOS'
set -euo pipefail
die() { echo "FATAL: $*" >&2; exit 1; }
META_URI="$(cat /etc/squeezefs-bench-meta-uri)"
read -ra EXTRA <<<"$(printf '%s' "$MOUNT_EXTRA_STR" | tr ',' ' ')"
LOG=/tmp/sqz-sym-reader.log
# --log-file opens O_APPEND: rotate the previous incarnation's log aside so
# the log-line gates below read THIS mount's lines only (a rejoin through
# sym-hook, a re-assemble — review round 1, Issue 7); bench-sym pulls
# `sqz-sym-*.log*`.
[ -f "$LOG" ] && mv "$LOG" "$LOG.$(date +%s)"
mkdir -p "$MNT"
env -u SUDO_UID -u SUDO_GID -u SUDO_USER \
  "SQUEEZEFS_IPC_ALLOW_DEV=1" \
  "SQUEEZEFS_SYMMETRIC_META=1" \
  "$SQZ" mount "$META_URI" "$MNT" --read-only --daemon "${EXTRA[@]}" --log-file "$LOG" \
  >/tmp/sqz-sym-reader.mount.out 2>&1 ||
  die "token reader mount failed: $(cat /tmp/sqz-sym-reader.mount.out)"
wait_for "reader mountpoint" 240 mountpoint -q "$MNT"
wait_for "reader stats inode" 240 test -s "$MNT/.stats"
grep -q "read under TOKENS from" "$LOG" ||
  die "token reader log carries no 'read under TOKENS from' line — the token arm did not engage (log: $LOG)"
poll_stat "$MNT" reader_staleness_bound_ms 0 60 "token reader: R-SYM-4's posture word (log: $LOG)"
echo "  token reader up at $MNT (reader_staleness_bound_ms=0)"
EOS
)"

# sym_mount_node <name> manager|joiner|reader — one node's mount, gated.
sym_mount_node() {
  local name="$1" kind="$2" ip priv
  ip="$(node_pub "$name")"; priv="$(node_priv "$name")"
  local script mnt
  case "$kind" in
    manager) script="$SYM_MOUNT_MANAGER_SCRIPT"; mnt="$MOUNTPOINT" ;;
    joiner)  script="$SYM_MOUNT_JOINER_SCRIPT";  mnt="$MOUNTPOINT" ;;
    reader)  script="$SYM_MOUNT_READER_SCRIPT";  mnt="$SYM_READER_MNT" ;;
    *) die "sym_mount_node: kind must be manager|joiner|reader" ;;
  esac
  remote "$ip" \
    SQZ="$REMOTE_DIR/squeezefs" MNT="$mnt" NODE="$name" PRIV_IP="$priv" PORT="$SYM_PORT" \
    MEMBERSHIP_BIND="$SYM_MEMBERSHIP_BIND" \
    MOUNT_EXTRA_STR="$(printf '%s' "$MW_MOUNT_EXTRA" | tr ' ' ',')" \
    <<<"$SYM_STAT_HELPERS
$script"
}

# sym_unmount_node <name> <mnt> — the product umount (the clean LEAVE:
# every slot handed back, the page Free), the lazy fallback, the reap.
sym_unmount_node() {
  remote "$(node_pub "$1")" SQZ="$REMOTE_DIR/squeezefs" MNT="$2" <<<"$SYM_STAT_HELPERS
die() { echo \"FATAL: \$*\" >&2; exit 1; }
unmount_and_reap \"\$MNT\"
echo \"  unmounted \$MNT on $1\""
}

cmd_assemble_sym() {
  require_local_tools
  load_state --placeholder-ok
  sym_shape_check
  local -a clients
  mapfile -t clients < <(sym_client_names)
  [ "${#clients[@]}" -ge 2 ] || die "assemble-sym needs >= 2 client nodes (N_CLIENT=$N_CLIENT; the cluster has ${#clients[@]})"
  local n="${#clients[@]}"
  local reader_word=""
  [ "$SYM_TOKEN_READER" = "1" ] && reader_word=" + a token reader on ${clients[0]}"
  confirm "assemble-sym REFORMATS the cluster volumes (any prior benchmark data on $CID is destroyed) with format --symmetric and mounts ONE symmetric writer per client node ($n nodes: the manager on ${clients[0]}, joined writers on ${clients[*]:1})$reader_word."

  local c ip
  log "assemble-sym 1/9: client kernel floor — FUSE-over-io_uring (v6.14+) on EVERY client node"
  for c in "${clients[@]}"; do
    remote "$(node_pub "$c")" NODE="$c" <<'EOS'
set -euo pipefail
modprobe fuse 2>/dev/null || true
[ -e /sys/module/fuse/parameters/enable_uring ] || {
  echo "FATAL[$NODE]: kernel $(uname -r) lacks FUSE-over-io_uring (no /sys/module/fuse/parameters/enable_uring; mainline v6.14+ needed) — every SqueezeFS mount requires the transport. Remedy: the mw preset's default Ubuntu 26.04 AMI (AMI_SSM_PARAM) or any v6.14+ kernel." >&2
  exit 1
}
echo "$NODE kernel $(uname -r): fuse.enable_uring present"
EOS
  done

  log "assemble-sym 2/9: client prologue on EVERY client node — unmount + disconnect survivors (idempotency)"
  for c in "${clients[@]}"; do
    remote "$(node_pub "$c")" SQZ="$REMOTE_DIR/squeezefs" MNT="$MOUNTPOINT" NQN_PREFIX="$NQN_PREFIX" \
      <<<"$CLIENT_PROLOGUE_SCRIPT"
  done

  log "assemble-sym 3/9: node identities — /etc/machine-id DISTINCT across the client nodes (the daemon's node token is derived from it)"
  # The daemon's node token — half of the KD-MW-2 `(node_token, mount_slot)`
  # identity every appender page, claim-set entry and membership record
  # carries — is derived from /etc/machine-id (src/writer_scope.rs). A
  # baked AMI clones the file onto every node, so every joiner of the
  # 2026-09-24 cloud row carried the MANAGER's identity and assemble
  # attempt 1 failed at the mounts (where in the ladder is not known — the
  # evidence is lost; the identity equality is the derivation). The node
  # script + the assert/regenerate loop are tests/cloud_bench_node_scripts.sh's
  # (`sym_assert_machine_ids`), pinned by tests/cloud_bench_cluster_units.sh;
  # runs after the prologue (no daemon is up) and before any
  # identity-bearing step.
  sym_assert_machine_ids "${clients[@]}"

  log "assemble-sym 4/9: host identities — every client node its OWN registrant (nvme-cli hostnqn AND hostid, each DISTINCT across the fleet)"
  # A baked AMI clones /etc/nvme/hostnqn + hostid onto every node; two
  # nodes with one identity ALIAS at the target (one registrant, the
  # fence blind to which host wrote). nvmet keys a PR registrant by the
  # controller's HOST ID (`nvmet_pr_registrant.hostid`) — so the hostid is
  # the key that matters and the NQN the name beside it: BOTH are asserted
  # distinct, a duplicate of EITHER regenerates both on the later node
  # (review round 1, Issue 2). The device's own answer is gated after the
  # mounts: `pr_registrant_shared == 0` on every writer.
  local -A seen_nqn=() seen_hid=()
  local nqn hid
  for c in "${clients[@]}"; do
    ip="$(node_pub "$c")"
    nqn="" hid=""   # per iteration (the dry-run branch names each node's own)
    if $DRY_RUN; then
      remote "$ip" NODE="$c" REGEN=0 <<'EOS'
set -euo pipefail
mkdir -p /etc/nvme
[ "$REGEN" = 1 ] && rm -f /etc/nvme/hostnqn /etc/nvme/hostid
[ -s /etc/nvme/hostnqn ] || nvme gen-hostnqn >/etc/nvme/hostnqn
[ -s /etc/nvme/hostid ] || { cat /proc/sys/kernel/random/uuid >/etc/nvme/hostid; }
echo "IDENTITY $(tr -d '[:space:]' </etc/nvme/hostnqn) $(tr -d '[:space:]' </etc/nvme/hostid)"
EOS
      nqn="nqn.2014-08.org.nvmexpress:uuid:dryrun-$c"
    else
      local out try
      for try in 1 2; do
        out="$(remote "$ip" NODE="$c" REGEN="$([ "$try" = 2 ] && echo 1 || echo 0)" <<'EOS'
set -euo pipefail
mkdir -p /etc/nvme
[ "$REGEN" = 1 ] && rm -f /etc/nvme/hostnqn /etc/nvme/hostid
[ -s /etc/nvme/hostnqn ] || nvme gen-hostnqn >/etc/nvme/hostnqn
[ -s /etc/nvme/hostid ] || { cat /proc/sys/kernel/random/uuid >/etc/nvme/hostid; }
echo "IDENTITY $(tr -d '[:space:]' </etc/nvme/hostnqn) $(tr -d '[:space:]' </etc/nvme/hostid)"
EOS
)"
        nqn="$(awk '/^IDENTITY /{print $2}' <<<"$out")"; hid="$(awk '/^IDENTITY /{print $3}' <<<"$out")"
        [ -n "$nqn" ] && [ -n "$hid" ] || die "$c: could not read its nvme host identity"
        local dup=""
        [ -n "${seen_nqn[$nqn]:-}" ] && dup="hostnqn $nqn duplicates ${seen_nqn[$nqn]}'s"
        [ -n "${seen_hid[$hid]:-}" ] && dup="${dup:+$dup; }hostid $hid duplicates ${seen_hid[$hid]}'s (the PR registrant KEY)"
        if [ -n "$dup" ]; then
          [ "$try" = 1 ] || die "$c: $dup — STILL after regeneration"
          warn "$c: $dup (a baked AMI's clone); regenerating both"
          continue
        fi
        break
      done
    fi
    [ -n "${hid:-}" ] || hid="dryrun-hid-$c"
    seen_nqn[$nqn]="$c"
    seen_hid[$hid]="$c"
    echo "  $c: hostnqn $nqn hostid $hid"
  done

  log "assemble-sym 5/9: storage nodes — instance-store share + nvmet PR assert (resv_enable=1; v6.13+ floor)"
  share_storage_nodes
  assert_storage_pr

  log "assemble-sym 6/9: ${clients[0]} — single-path connect, PR verify, format --symmetric CACHE-LESS (no --disk-cache-paths: the box/local fleets' shape; the manager's node formats, nobody mounts yet)"
  remote "$(node_pub "${clients[0]}")" \
    SQZ="$REMOTE_DIR/squeezefs" MNT="$MOUNTPOINT" CACHE="$CACHE_DIR" \
    META_SPECS="$SHARED_META_SPECS" DATA_SPECS="$SHARED_DATA_SPECS" \
    MOUNT_EXTRA_STR="$(printf '%s' "$MW_MOUNT_EXTRA" | tr ' ' ',')" \
    FORMAT_EXTRA_STR="--symmetric" FORMAT_CACHE=0 MW_PR_VERIFY=1 MW_SKIP_MOUNT=1 \
    <<<"$CLIENT_FABRIC_SCRIPT"

  log "assemble-sym 7/9: ${clients[*]:1} — single-path connect + PR verify (no format: a joining node)"
  for c in "${clients[@]:1}"; do
    echo "-- $c"
    remote "$(node_pub "$c")" \
      SQZ="$REMOTE_DIR/squeezefs" MNT="$MOUNTPOINT" CACHE="$CACHE_DIR" \
      META_SPECS="$SHARED_META_SPECS" DATA_SPECS="$SHARED_DATA_SPECS" \
      MOUNT_EXTRA_STR="$(printf '%s' "$MW_MOUNT_EXTRA" | tr ' ' ',')" \
      MW_PR_VERIFY=1 MW_SKIP_MOUNT=1 MW_SKIP_FORMAT=1 \
      <<<"$CLIENT_FABRIC_SCRIPT"
  done

  log "assemble-sym 8/9: mount the symmetric fleet — the MANAGER on ${clients[0]}, then a JOINED writer per node (${clients[*]:1})${reader_word:+, then$reader_word}"
  local manager_out
  manager_out="$(sym_mount_node "${clients[0]}" manager)"
  echo "$manager_out"
  local mgr_ep
  mgr_ep="$(awk '/^MANAGER_ENDPOINT /{print $2}' <<<"$manager_out")"
  $DRY_RUN && mgr_ep="$(node_priv "${clients[0]}"):$SYM_PORT"
  [ -n "$mgr_ep" ] || die "the manager's endpoint was not reported"
  for c in "${clients[@]:1}"; do
    sym_mount_node "$c" joiner
  done
  if [ "$SYM_TOKEN_READER" = "1" ]; then
    sym_mount_node "${clients[0]}" reader
  fi
  # The fleet-wide gates at the manager: N Live pages (the manager + every
  # joiner — `appenders_known`, the directory's Live-page count; a daemon's
  # `appenders_live` counts only the regions IT joined), the membership
  # census — every joiner is a WRITER member of the manager's shard
  # (`membership_writers == N − 1`; `membership_members` also counts the
  # token reader, so it is not the gate).
  remote "$(node_pub "${clients[0]}")" MNT="$MOUNTPOINT" N="$n" <<<"$SYM_STAT_HELPERS
die() { echo \"FATAL: \$*\" >&2; exit 1; }
poll_all_eq \"\$MNT\" appenders_known \"\$N\" 240 \"manager: appenders_known — the directory's Live-page count (want \$N = the manager + \$((N - 1)) joined writers; appenders_live counts only the regions THIS daemon joined)\"
poll_stat \"\$MNT\" membership_writers \"\$((N - 1))\" 240 \"manager: membership_writers (every joiner a WRITER member of the manager's shard)\"
echo \"  fleet gates: appenders_known=\$(stat_first \"\$MNT\" appenders_known) membership_members=\$(stat_field \"\$MNT\" membership_members) membership_writers=\$(stat_field \"\$MNT\" membership_writers) membership_readers=\$(stat_field \"\$MNT\" membership_readers)\""

  # The DEVICE's word on the identities: the Reservation Report's
  # REGISTRANT COUNT. Every node's daemon registers on every namespace of
  # the set (the manager HOLDS + registers, each REMOTE joiner REGISTERS
  # under the set's key with ITS host identity; the reader registers
  # nothing) — so `nvme resv-report -e` on any client node must list
  # exactly N distinct Host IDs per namespace. Two nodes aliasing as one
  # host (a cloned identity) read N − 1; a joiner that adopted instead of
  # registering reads N − 1 too. (Round 1's `pr_registrant_shared` was
  # vacuous here — the MANAGER's own gauge, evaluated at its open before
  # any joiner, sensitive only to a different rkey under its own hostid;
  # review round 2, Issue 14.) The manager's `pr_registrants_per_namespace`
  # — the daemon's own REGCTL read, refreshed on the guard's 10 s heartbeat
  # for the METADATA namespaces — is printed beside it as the record.
  log "assemble-sym 8b/9: the device's registrant count — nvme resv-report -e on every namespace of the set reads $n distinct Host IDs (the manager + $((n - 1)) joined writers)"
  remote "$(node_pub "${clients[0]}")" N="$n" MNT="$MOUNTPOINT" <<<"$SYM_STAT_HELPERS
die() { echo \"FATAL: \$*\" >&2; exit 1; }
set -euo pipefail
command -v nvme >/dev/null 2>&1 || die \"nvme-cli missing on the manager's node\"
devs=\"\$(sed 's|^sqmeta://||' /etc/squeezefs-bench-meta-uri),\$(sed 's|^sqdata://||' /etc/squeezefs-bench-data-uri)\"
IFS=, read -ra DEVS <<<\"\$devs\"
[ \"\${#DEVS[@]}\" -ge 2 ] || die \"no namespaces recorded on this node (the connect step did not run)\"
for d in \"\${DEVS[@]}\"; do
  # a joiner's registration is synchronous at its join, but the report is
  # the device's — one short retry window absorbs a fabric round trip
  got=-1
  for try in \$(seq 1 20); do
    : \"\$try\"
    got=\"\$(nvme resv-report -e -o json \"\$d\" 2>/dev/null | python3 -c '
import json, sys
r = json.load(sys.stdin)
regs = r.get(\"regctlext\") or r.get(\"regctl_ext\") or r.get(\"regctls\") or []
print(len({str(e.get(\"hostid\", \"\")).lower() for e in regs if str(e.get(\"hostid\", \"\"))}))' 2>/dev/null || echo -1)\"
    [ \"\$got\" = \"\$N\" ] && break
    sleep 1
  done
  [ \"\$got\" = \"\$N\" ] || die \"\$d: nvme resv-report -e lists \$got distinct Host ID(s) (want \$N = the manager + \$((N - 1)) joined writers, each its OWN registrant) — a cloned host identity (two nodes as one registrant) or a joiner that adopted instead of registering; the fence between such nodes is process-local, not device-enforced\"
  echo \"  \$d: \$got distinct registrant Host IDs (the device's report)\"
done
echo \"  manager pr_registrants_per_namespace (the daemon's REGCTL read, 10 s heartbeat on the metadata namespaces): \$(stat_field \"\$MNT\" pr_registrants_per_namespace)\""

  log "assemble-sym 9/9: build_commit verification ritual on every node"
  for c in "${clients[@]}"; do
    local mnts="$MOUNTPOINT"
    [ "$c" = "${clients[0]}" ] && [ "$SYM_TOKEN_READER" = "1" ] && mnts="$MOUNTPOINT,$SYM_READER_MNT"
    verify_build_commit "$(node_pub "$c")" "$mnts"
  done
  if ! $DRY_RUN; then
    save_state   # SYM_STORAGE_DEVS for bench-sym
  fi
  echo
  echo "Symmetric fleet assembled: manager ${clients[0]}:$MOUNTPOINT (serving on $mgr_ep) + $((n - 1)) joined writer(s) on ${clients[*]:1}$([ "$SYM_TOKEN_READER" = "1" ] && echo " + token reader ${clients[0]}:$SYM_READER_MNT")."
  echo "Next: PRESET=mw SYMMETRIC=1 N_CLIENT=$N_CLIENT tests/cloud_bench_cluster.sh bench-sym"
}

# ---------------------------------------------------------------------------
# sym-hook — the row driver's leave/rejoin of ONE writer node (gate 3's
# "exactly N appenders live" + its deleted-stays-deleted-across-the-leave
# arm): `sym-hook mount|unmount <client-pub-ip> <mnt>`. A joined writer's
# clean unmount IS the leave (every slot handed back, its page Free); its
# mount rejoins through the ladder (its own region as own residue).
# ---------------------------------------------------------------------------
cmd_sym_hook() {
  require_local_tools
  load_state --placeholder-ok
  sym_shape_check
  local name="" idx
  for idx in "${!NODE_NAMES[@]}"; do
    [ "${NODE_PUB[$idx]}" = "$HOOK_HOST" ] && name="${NODE_NAMES[$idx]}"
  done
  [ -n "$name" ] || die "sym-hook: no node of $CID has public ip $HOOK_HOST"
  [ "$(role_of "$name")" = "client" ] || die "sym-hook: $name is not a client node"
  [ "$name" != "client0" ] || die "sym-hook: client0 is the MANAGER — the driver never leaves/rejoins it"
  [ "$HOOK_MNT" = "$MOUNTPOINT" ] || die "sym-hook: '$HOOK_MNT' is not the writer mount ($MOUNTPOINT)"
  case "$HOOK_VERB" in
    mount)   sym_mount_node "$name" joiner ;;
    unmount) sym_unmount_node "$name" "$HOOK_MNT" ;;
    *) die "sym-hook: verb must be mount|unmount (got '$HOOK_VERB')" ;;
  esac
}

# ---------------------------------------------------------------------------
# bench-sym — the three symmetric row sets over the assembled fleet, driven
# from THIS box by tests/cloud_sym_rows.sh (one row driver, the matrix's
# laws via tests/sym_rows_lib.sh). Rows land under
# .benchmarks/cloud/<ts>/sym-rows/ with the manifest, per-row labels, the
# per-node .stats snapshots and the fsck transcripts. A cloud row is a
# THIRD substrate class — never spliced into devsub or squeeze-test medians.
# ---------------------------------------------------------------------------
cmd_bench_sym() {
  require_local_tools
  load_state --placeholder-ok
  sym_preflight
  local -a clients
  mapfile -t clients < <(sym_client_names)
  [ "${#clients[@]}" -ge 2 ] || die "bench-sym needs >= 2 client nodes (assemble-sym first)"
  [ "${#clients[@]}" -eq "$N_CLIENT" ] || die "bench-sym: the cluster has ${#clients[@]} client nodes but N_CLIENT=$N_CLIENT — pass the shape the cluster was launched with"
  local driver
  driver="$(dirname "$SCRIPT_PATH")/cloud_sym_rows.sh"
  local corpus_arg=""
  if [ -n "$SYM_TARBALL" ]; then
    corpus_arg="--tarball=$SYM_TARBALL"
  elif [ -n "$SYM_TAR_SRC" ]; then
    corpus_arg="--tar-src=$SYM_TAR_SRC"
  fi
  local ns="$SYM_SCALE_NS"
  if [ -z "$ns" ]; then
    local n
    for n in 1 2 4 8; do [ "$n" -le "${#clients[@]}" ] && ns="${ns:+$ns,}$n"; done
  fi
  local ts
  ts="$(date +%Y-%m-%d-%H%M%S)"
  BENCH_DIR="$RESULTS_ROOT/$ts"
  if $DRY_RUN; then
    echo "(dry-run: results would land in $BENCH_DIR/sym-rows — nothing is written)"
  else
    mkdir -p "$BENCH_DIR/sym-rows"
  fi

  log "bench-sym: fleet-liveness preflight (every client node's mount)"
  local c
  for c in "${clients[@]}"; do
    remote "$(node_pub "$c")" MNT="$MOUNTPOINT" NODE="$c" <<'EOS'
set -euo pipefail
mountpoint -q "$MNT" || { echo "symmetric fleet not assembled: $MNT is not mounted on $NODE (run assemble-sym)" >&2; exit 1; }
test -s "$MNT/.stats" || { echo "$NODE: $MNT/.stats unreadable" >&2; exit 1; }
echo "$NODE: $MNT live"
EOS
  done

  # The driver's node table: manager=client0, writer=client1.., reader=client0's -ro.
  local -a entries=("manager=$(node_pub "${clients[0]}"):$MOUNTPOINT")
  for c in "${clients[@]:1}"; do entries+=("writer=$(node_pub "$c"):$MOUNTPOINT"); done
  [ "$SYM_TOKEN_READER" = "1" ] && entries+=("reader=$(node_pub "${clients[0]}"):$SYM_READER_MNT")
  local substrate="aws-$MARKET/$INSTANCE_TYPE/$AWS_AZ/pg-$PLACEMENT_STRATEGY (instance-store NVMe over nvmet-tcp, single NIC; one symmetric writer per node)"
  local -a drv=("$driver"
    "--rows=$SYM_ROWS" "--rt=$SYM_RT" "--files=$SYM_FILES" "--threads=$SYM_THREADS" "--ingest-mb=$SYM_INGEST_MB"
    "--scale-ns=$ns" "--sqz=$REMOTE_DIR/squeezefs" "--rowdir=$BENCH_DIR/sym-rows" "--venue=cloud"
    "--size-to-rt=auto" "--ingest-cap-mb=$SYM_INGEST_CAP_MB"
    "--substrate=$substrate" "--cluster=$CID" "--format=--symmetric cache-less (no --disk-cache-paths)" "--ssh-key=$SSH_KEY_FILE" "--ssh-user=$REMOTE_USER"
    "--mount-hook=$SCRIPT_PATH sym-hook --preset $PRESET --symmetric --cluster-id $CID"
    "--manager-priv=$(node_priv "${clients[0]}")" "--remote-dir=$REMOTE_DIR/sym-rows")
  [ -n "$corpus_arg" ] && drv+=("$corpus_arg")
  [ -n "$SYM_STORAGE_DEVS" ] && drv+=("--storage=$SYM_STORAGE_DEVS")
  # (the driver's own ssh options are this rig's SSH_OPTS verbatim —
  # BatchMode, ConnectTimeout, accept-new, IdentitiesOnly)
  $DRY_RUN && drv+=("--dry-run")
  drv+=("${entries[@]}")

  sym_manifest() {
    echo "cluster=$CID preset=$PRESET symmetric=1 instance_type=$INSTANCE_TYPE az=$AWS_AZ region=$AWS_REGION market=$MARKET placement=$PLACEMENT_STRATEGY"
    echo "rows=$SYM_ROWS (gate 2 sym-tarx | gate 3 sym-scale N in {$ns} | gate 3b sym-shared-dir + -ls) — tests/cloud_sym_rows.sh over ssh, laws = tests/sym_rows_lib.sh (the matrix's)"
    echo "fleet: manager ${clients[0]}:$MOUNTPOINT + $((${#clients[@]} - 1)) joined writer node(s) ${clients[*]:1}$([ "$SYM_TOKEN_READER" = "1" ] && echo " + token reader ${clients[0]}:$SYM_READER_MNT"); ONE symmetric writer per node; storage=$SYM_STORAGE_DEVS"
    echo "format=--symmetric CACHE-LESS (no --disk-cache-paths; the box/local fleets' shape) rt=$SYM_RT files=$SYM_FILES threads=$SYM_THREADS ingest_mb=$SYM_INGEST_MB corpus=${SYM_TARBALL:-$SYM_TAR_SRC}"
    echo "instrument=tar -xf (the shipped corpus), tests/mdstorm.c, dd bs=4M conv=fsync, python3 O_CREAT|O_EXCL creators, ls -l (the matrix's sym legs' instruments)"
    echo "substrate=$substrate — cloud substrate, a THIRD class: never spliced into devsub loop/tcp or squeeze-test medians (docs/rc-manifest.md tiers)"
    echo "repo_commit=$(git rev-parse HEAD 2>/dev/null || echo unknown)"
    echo "discipline: an instance leaving running mid-row => COUNT ABORTED, restart from zero (never splice); every rate row RT >= $SYM_RT s"
    echo "ts=$ts"
  }
  if $DRY_RUN; then
    echo "-- manifest.txt would contain:"; sym_manifest | sed 's/^/   /'
  else
    sym_manifest >"$BENCH_DIR/manifest.txt"
  fi

  log "bench-sym: the row driver -> $BENCH_DIR/sym-rows"
  spot_monitor_start
  assert_fleet_running
  local row_rc=0
  if $DRY_RUN; then
    printf '+'; printf ' %q' "${drv[@]}"; printf '\n'
    "${drv[@]}" || row_rc=$?
  else
    # EVIDENCE BEFORE VERDICT: the driver writes its rows/snapshots/fsck
    # transcripts into the results dir as it goes; the per-node daemon
    # logs are pulled below UNCONDITIONALLY, then the verdict.
    "${drv[@]}" 2>&1 | tee "$BENCH_DIR/sym-rows/driver.log" || row_rc=$?
  fi
  spot_monitor_stop

  log "bench-sym: pull the per-node daemon logs"
  for c in "${clients[@]}"; do
    if $DRY_RUN; then
      printf '+ scp %s@%s:/tmp/sqz-sym-*.log %s/sym-rows/logs/%s/\n' "$REMOTE_USER" "$(node_pub "$c")" "$BENCH_DIR" "$c"
      continue
    fi
    mkdir -p "$BENCH_DIR/sym-rows/logs/$c"
    # --log-file is 0600 root (VAL-7h): stage world-readable copies.
    remote "$(node_pub "$c")" <<'EOS' || true
set -euo pipefail
mkdir -p /tmp/sym-rows/logs
for f in /tmp/sqz-sym-*.log /tmp/sqz-sym-*.log.* /tmp/sqz-sym-*.mount.out; do
  [ -f "$f" ] && install -m 0644 "$f" /tmp/sym-rows/logs/ || true
done
chmod -R a+rX /tmp/sym-rows/logs
EOS
    scp -r "${SSH_OPTS[@]}" -i "$SSH_KEY_FILE" "$REMOTE_USER@$(node_pub "$c"):/tmp/sym-rows/logs/." "$BENCH_DIR/sym-rows/logs/$c/" \
      || warn "could not pull $c's daemon logs"
  done
  [ "$row_rc" -eq 0 ] \
    || die "the symmetric row driver FAILED (rc=$row_rc) — rows/snapshots/logs pulled to $BENCH_DIR/sym-rows before this verdict (evidence before verdict)"
  assert_fleet_running   # the rows count only if the fleet survived them
  echo
  echo "Symmetric rows complete. Results: $BENCH_DIR (manifest; sym-rows/rows.txt = the labelled rows + verdicts; per-node .stats snapshots, fsck transcripts and daemon logs under sym-rows/; fleet verified running end-to-end — count valid)"
}

# ---------------------------------------------------------------------------
# bench — the standing elbencho battery with house labeling + A-B-B-A helper
# + spot-interruption abort (counted-run discipline).
# ---------------------------------------------------------------------------
BENCH_ORDER=0
BENCH_DIR=""
CLIENT_IP=""
ELB_VER="elbencho (version unknown)"
SPOT_FLAG=""
MON_PID=""

assert_fleet_running() {
  $DRY_RUN && return 0
  local not_running
  # shellcheck disable=SC2016  # JMESPath: backtick literals must NOT shell-expand
  not_running="$(aws --region "$AWS_REGION" ec2 describe-instances --instance-ids "${NODE_IDS[@]}" \
    --query 'Reservations[].Instances[?State.Name!=`running`].[InstanceId,State.Name]' --output text)"
  if [ -n "$not_running" ] || { [ -n "$SPOT_FLAG" ] && [ -f "$SPOT_FLAG" ]; }; then
    {
      echo "SPOT INTERRUPTION / instance-state change detected mid-run:"
      echo "${not_running:-"(flagged by background monitor)"}"
      echo
      echo "COUNT ABORTED — per the multi-run discipline this run is INVALID"
      echo "and the count restarts from zero on a fresh cluster. Do NOT splice"
      echo "these partial results into any median."
    } | tee "$BENCH_DIR/COUNT-ABORTED-SPOT-INTERRUPTION.txt" >&2
    die "spot interruption aborted the benchmark count (results dir marked INVALID: $BENCH_DIR)"
  fi
}

spot_monitor_start() {
  $DRY_RUN && { echo "+ (background spot-interruption monitor: aws ec2 describe-instances poll, 20s cadence)"; return 0; }
  SPOT_FLAG="$BENCH_DIR/.spot-interrupted"
  (
    while sleep 20; do
      # shellcheck disable=SC2016  # JMESPath: backtick literals must NOT shell-expand
      bad="$(aws --region "$AWS_REGION" ec2 describe-instances --instance-ids "${NODE_IDS[@]}" \
        --query 'length(Reservations[].Instances[?State.Name!=`running`][])' --output text 2>/dev/null || echo poll-failed)"
      if [ "$bad" != "0" ] && [ "$bad" != "poll-failed" ]; then
        echo "instance-state change at $(date -u +%FT%TZ): $bad instance(s) not running" >"$SPOT_FLAG"
        break
      fi
    done
  ) &
  MON_PID=$!
}

spot_monitor_stop() {
  if [ -n "$MON_PID" ]; then
    kill "$MON_PID" 2>/dev/null || true
    MON_PID=""
  fi
}

stats_snapshot() { # stats_snapshot <file>
  local out="$1"
  if $DRY_RUN; then
    printf '+ ssh %s@%s sudo cat %s/.stats > %s\n' "$REMOTE_USER" "$CLIENT_IP" "$MOUNTPOINT" "$out"
  else
    remote "$CLIENT_IP" MNT="$MOUNTPOINT" <<'EOS' >"$out"
set -euo pipefail
cat "$MNT/.stats"
EOS
  fi
}

row_stamp() { # row_stamp <label> <cmd> — the house labeling discipline, per row
  echo "# row=$1"
  echo "# order=$BENCH_ORDER"
  echo "# instrument=$ELB_VER (dynamic; sync drivers unless --iodepth stated in cmd)"
  echo "# substrate=aws-$MARKET/$INSTANCE_TYPE/$AWS_AZ/pg-$PLACEMENT_STRATEGY (instance-store NVMe over nvmet-tcp, single NIC)"
  echo "# venue=cloud-bench-cluster/$PRESET cluster=$CID"
  echo "# ts=$(date -u +%FT%TZ)"
  echo "# cmd=$2"
}

bench_row() { # bench_row <label> <remote command string>
  local label="$1" cmd="$2"
  assert_fleet_running
  BENCH_ORDER=$((BENCH_ORDER + 1))
  local ord rowfile
  ord="$(printf '%02d' "$BENCH_ORDER")"
  rowfile="$BENCH_DIR/$ord-$label.txt"
  log "bench row $BENCH_ORDER: $label"
  if $DRY_RUN; then
    row_stamp "$label" "$cmd"                     # shown, not written
    printf '  (row output would be captured to %s)\n' "$rowfile"
    stats_snapshot "$BENCH_DIR/$ord-$label.stats-before.json"
    remote "$CLIENT_IP" <<EOS
set -euo pipefail
$cmd
EOS
    stats_snapshot "$BENCH_DIR/$ord-$label.stats-after.json"
    return 0
  fi
  row_stamp "$label" "$cmd" >"$rowfile"
  stats_snapshot "$BENCH_DIR/$ord-$label.stats-before.json"
  remote "$CLIENT_IP" <<EOS | tee -a "$rowfile"
set -euo pipefail
$cmd
EOS
  stats_snapshot "$BENCH_DIR/$ord-$label.stats-after.json"
}

# A-B-B-A alternating-order bracket (standing comparison rule: any A/B whose
# shared store AGES across runs must run both orders; a single-order delta is
# an ordering artifact until the reversed bracket reproduces it).
abba_bracket() { # abba_bracket <labelA> <cmdA> <labelB> <cmdB>
  local la="$1" ca="$2" lb="$3" cb="$4"
  bench_row "${la}-A1" "$ca"
  bench_row "${lb}-B1" "$cb"
  bench_row "${lb}-B2" "$cb"
  bench_row "${la}-A2" "$ca"
}

cmd_bench() {
  require_local_tools
  load_state --placeholder-ok
  CLIENT_IP="$(node_pub client0)"

  local ts
  ts="$(date +%Y-%m-%d-%H%M%S)"
  BENCH_DIR="$RESULTS_ROOT/$ts"
  if $DRY_RUN; then
    echo "(dry-run: results would land in $BENCH_DIR — nothing is written)"
  else
    mkdir -p "$BENCH_DIR"
  fi

  if ! $DRY_RUN; then
    remote "$CLIENT_IP" MNT="$MOUNTPOINT" <<'EOS'
set -euo pipefail
mountpoint -q "$MNT" || { echo "cluster not assembled: $MNT not mounted" >&2; exit 1; }
EOS
    ELB_VER="$(remote "$CLIENT_IP" ELB="$REMOTE_DIR/elbencho" <<'EOS'
set -euo pipefail
"$ELB" --version | head -1
EOS
)"
  fi

  local elb="$REMOTE_DIR/elbencho"
  local F="$MOUNTPOINT/bench/f{1..$BENCH_THREADS}"
  local G="$MOUNTPOINT/bench/g{1..$BENCH_THREADS}"

  manifest() {
    echo "cluster=$CID preset=$PRESET instance_type=$INSTANCE_TYPE az=$AWS_AZ region=$AWS_REGION market=$MARKET placement=$PLACEMENT_STRATEGY"
    echo "instrument=$ELB_VER"
    echo "substrate=aws-$MARKET/$INSTANCE_TYPE/$AWS_AZ/pg-$PLACEMENT_STRATEGY — a THIRD substrate class: never mix into devsub loop/tcp medians"
    echo "repo_commit=$(git rev-parse HEAD 2>/dev/null || echo unknown)"
    echo "roles: mds=$N_MDS oss=$N_OSS client=$N_CLIENT spare=$N_SPARE"
    echo "discipline: spot interruption mid-battery => COUNT ABORTED, restart from zero (never splice)"
    echo "ts=$ts"
  }
  if $DRY_RUN; then
    echo "-- manifest.txt would contain:"; manifest | sed 's/^/   /'
  else
    manifest >"$BENCH_DIR/manifest.txt"
  fi

  log "bench: battery -> $BENCH_DIR"
  # The global EXIT trap reaps the monitor on any failure path.
  spot_monitor_start

  if ! $DRY_RUN; then
    remote "$CLIENT_IP" MNT="$MOUNTPOINT" <<'EOS'
set -euo pipefail
mkdir -p "$MNT/bench"
EOS
  else
    printf '+ ssh %s@%s sudo mkdir -p %s/bench\n' "$REMOTE_USER" "$CLIENT_IP" "$MOUNTPOINT"
  fi

  # 1) seq write 1m: relaxed vs --sync durable, as an A-B-B-A bracket (the
  #    store ages across writes — RW6 durability-leveled rows, both labeled).
  #    This also primes the f-files every later row reads/overwrites.
  abba_bracket \
    "seq-write-1m-relaxed" \
    "$elb -w -t $BENCH_THREADS -b 1m -s $FILE_SIZE_1M --direct --lat $F" \
    "seq-write-1m-durable-sync" \
    "$elb -w -t $BENCH_THREADS -b 1m -s $FILE_SIZE_1M --direct --sync --lat $F"

  # 2) large-block reads
  bench_row "seq-read-1m" \
    "$elb -r -t $BENCH_THREADS -b 1m -s $FILE_SIZE_1M --direct --lat $F"
  bench_row "rand-read-1m" \
    "$elb -r -t $BENCH_THREADS -b 1m -s $FILE_SIZE_1M --rand --direct --lat $F"

  # 3) small-block reads
  bench_row "rand-read-4k-t${BENCH_QD_THREADS}qd${BENCH_IODEPTH}" \
    "$elb -r -t $BENCH_QD_THREADS -b 4k -s $FILE_SIZE_1M --rand --iodepth $BENCH_IODEPTH --direct --lat $F"
  bench_row "seq-read-4k" \
    "$elb -r -t $BENCH_THREADS -b 4k -s $FILE_SIZE_1M --direct --lat $F"

  # 4) small-block writes: rand-write OVERWRITES the primed f-files (the
  #    sole-owner patch / overwrite venue); seq-write-4k gets fresh g-files.
  bench_row "rand-write-4k-overwrite-primed" \
    "$elb -w -t $BENCH_QD_THREADS -b 4k -s $FILE_SIZE_1M --rand --iodepth $BENCH_IODEPTH --direct --lat $F"
  bench_row "seq-write-4k" \
    "$elb -w -t $BENCH_THREADS -b 4k -s $FILE_SIZE_4K --direct --lat $G"

  # 5) cleanup
  bench_row "cleanup-delete" \
    "$elb -F -t $BENCH_THREADS $F $G && rmdir $MOUNTPOINT/bench"

  spot_monitor_stop
  assert_fleet_running   # final check: the whole battery counts only if the fleet survived it

  log "bench: pull daemon log"
  if $DRY_RUN; then
    printf '+ scp %s@%s:/tmp/sqz.log %s/sqz.log\n' "$REMOTE_USER" "$CLIENT_IP" "$BENCH_DIR"
  else
    scp "${SSH_OPTS[@]}" -i "$SSH_KEY_FILE" "$REMOTE_USER@$CLIENT_IP:/tmp/sqz.log" "$BENCH_DIR/sqz.log" \
      || warn "could not pull /tmp/sqz.log"
  fi
  echo
  echo "Battery complete. Results: $BENCH_DIR (fleet verified running end-to-end — count valid)"
}

# ---------------------------------------------------------------------------
# bench-mw — the s11-mpiio shared-vs-disjoint MPI-IO ior row over the MW
# fleet (assemble-mw first). Pushes tests/run_mw_matrix.sh to the client
# under a repo-shaped dir and drives its EXTERNAL-MOUNTS mode as root: the
# leg builds the pinned ior 4.0.0 on the client (sha256-checked, from
# github over the client's internet), self-sizes a >=60 s sustained window,
# gates shared >= 0.8x disjoint in BOTH internal A-B-B-A brackets, verifies
# engagement exactly, and runs the warm fsck/C8 oracle. Rows are pulled
# back under $RESULTS_ROOT/<ts>/ with the house labeling: a cloud row is
# measured-real over a real nvme-tcp network but a THIRD substrate class —
# never spliced into devsub loop/tcp medians (docs/rc-manifest.md tiers).
# ---------------------------------------------------------------------------
cmd_bench_mw() {
  require_local_tools
  load_state --placeholder-ok
  mw_shape_check
  CLIENT_IP="$(node_pub client0)"

  local ts
  ts="$(date +%Y-%m-%d-%H%M%S)"
  BENCH_DIR="$RESULTS_ROOT/$ts"
  if $DRY_RUN; then
    echo "(dry-run: results would land in $BENCH_DIR — nothing is written)"
  else
    mkdir -p "$BENCH_DIR"
  fi

  local mounts_csv i
  mounts_csv="$MOUNTPOINT"
  for ((i = 1; i <= MW_COWRITERS; i++)); do mounts_csv="$mounts_csv,$MOUNTPOINT-cw$i"; done

  log "bench-mw: fleet-liveness + client toolchain preflight"
  remote "$CLIENT_IP" MNTS="$mounts_csv" <<'EOS'
set -euo pipefail
IFS=, read -ra MS <<<"$MNTS"
for m in "${MS[@]}"; do
  mountpoint -q "$m" || { echo "MW fleet not assembled: $m is not mounted (run assemble-mw)" >&2; exit 1; }
done
for t in mpirun mpicc curl gcc make python3; do
  command -v "$t" >/dev/null 2>&1 || { echo "client lacks $t — re-run: PRESET=mw tests/cloud_bench_cluster.sh deploy" >&2; exit 1; }
done
echo "fleet live (${#MS[@]} mounts), MPI toolchain present"
EOS

  log "bench-mw: push tests/run_mw_matrix.sh (repo-shaped: the leg derives REPO from its own dirname/.. and builds ior into REPO/target/mw-ior)"
  push "$(dirname "$SCRIPT_PATH")/run_mw_matrix.sh" "$CLIENT_IP" "$REMOTE_DIR/repo/tests/run_mw_matrix.sh"

  local row_cmd
  row_cmd="SQZ_BIN=$REMOTE_DIR/squeezefs SQZ_MWMATRIX_MOUNTS=$mounts_csv SQZ_MWMATRIX_ROWDIR=$REMOTE_DIR/mw-rows bash $REMOTE_DIR/repo/tests/run_mw_matrix.sh s11-mpiio --procs=$MW_IOR_PROCS"
  mw_manifest() {
    echo "cluster=$CID preset=$PRESET instance_type=$INSTANCE_TYPE az=$AWS_AZ region=$AWS_REGION market=$MARKET placement=$PLACEMENT_STRATEGY"
    echo "row=s11-mpiio shared-vs-disjoint (tests/run_mw_matrix.sh external-mounts mode)"
    echo "fleet: 1 authority + $MW_COWRITERS co-writers co-located on client0; mounts=$mounts_csv; procs/mount=$MW_IOR_PROCS"
    echo "instrument=pinned ior 4.0.0 + mpirun (exact versions printed by the leg in the row output)"
    echo "substrate=aws-$MARKET/$INSTANCE_TYPE/$AWS_AZ (instance-store NVMe over nvmet-tcp, single NIC) — cloud substrate, a THIRD class: never spliced into devsub loop/tcp medians (docs/rc-manifest.md tiers)"
    echo "repo_commit=$(git rev-parse HEAD 2>/dev/null || echo unknown)"
    echo "discipline: spot interruption mid-row => COUNT ABORTED, restart from zero (never splice)"
    echo "ts=$ts"
  }
  mw_row_stamp() { # the house labeling discipline, MW face
    echo "# row=s11-mpiio-shared-vs-disjoint"
    echo "# order=$BENCH_ORDER"
    echo "# instrument=pinned ior 4.0.0 + mpirun (exact versions in the leg output below)"
    echo "# substrate=aws-$MARKET/$INSTANCE_TYPE/$AWS_AZ/pg-$PLACEMENT_STRATEGY (instance-store NVMe over nvmet-tcp, single NIC) — cloud substrate (third class — never spliced into devsub medians)"
    echo "# venue=cloud-bench-cluster/$PRESET cluster=$CID cowriters=$MW_COWRITERS procs=$MW_IOR_PROCS"
    echo "# ts=$(date -u +%FT%TZ)"
    echo "# cmd=$row_cmd"
  }

  log "bench-mw: s11-mpiio row -> $BENCH_DIR"
  spot_monitor_start
  assert_fleet_running
  BENCH_ORDER=1
  local rowfile="$BENCH_DIR/01-s11-mpiio-shared-vs-disjoint.txt"
  if $DRY_RUN; then
    echo "-- manifest.txt would contain:"; mw_manifest | sed 's/^/   /'
    mw_row_stamp                                    # shown, not written
    printf '  (row output would be captured to %s)\n' "$rowfile"
    remote "$CLIENT_IP" <<EOS
set -euo pipefail
mkdir -p "$REMOTE_DIR/mw-rows" "$REMOTE_DIR/repo/target"
$row_cmd
EOS
  else
    mw_manifest >"$BENCH_DIR/manifest.txt"
    mw_row_stamp >"$rowfile"
    # EVIDENCE BEFORE VERDICT (2026-08-19 lesson: a failing row died under
    # set -e before the pull below ever ran, and the teardown then
    # destroyed the on-cluster A1.out that named the failure): capture the
    # row's rc, pull the artifacts UNCONDITIONALLY, and only then fail.
    row_rc=0
    remote "$CLIENT_IP" <<EOS | tee -a "$rowfile" || row_rc=$?
set -euo pipefail
mkdir -p "$REMOTE_DIR/mw-rows" "$REMOTE_DIR/repo/target"
$row_cmd
EOS
  fi
  spot_monitor_stop

  log "bench-mw: pull rows + fleet logs"
  if ! $DRY_RUN; then
    # --log-file is 0600 root (VAL-7h): stage world-readable copies inside
    # the rows dir so the unprivileged scp below can carry everything home.
    remote "$CLIENT_IP" ROWDIR="$REMOTE_DIR/mw-rows" <<'EOS'
set -euo pipefail
mkdir -p "$ROWDIR/logs"
for f in /tmp/sqz-mw-*.log /tmp/sqz-mw-*.mount.out /tmp/sqz-mw-*.probe.out; do
  [ -f "$f" ] && install -m 0644 "$f" "$ROWDIR/logs/" || true
done
chmod -R a+rX "$ROWDIR"
EOS
  fi
  run scp -r "${SSH_OPTS[@]}" -i "$SSH_KEY_FILE" \
    "$REMOTE_USER@$CLIENT_IP:$REMOTE_DIR/mw-rows" "$BENCH_DIR/mw-rows"
  [ "${row_rc:-0}" -eq 0 ] \
    || die "s11-mpiio row FAILED (rc=$row_rc) — artifacts pulled to $BENCH_DIR/mw-rows before this verdict (evidence before verdict)"
  assert_fleet_running   # the row counts only if the fleet survived it
  echo
  echo "s11-mpiio row complete. Results: $BENCH_DIR (stamped row + manifest; per-phase ior outputs, stats snapshots and fsck report under mw-rows/; fleet verified running end-to-end — count valid)"
}

# ---------------------------------------------------------------------------
# status
# ---------------------------------------------------------------------------
cmd_status() {
  require_local_tools
  load_state --placeholder-ok --cid-only-ok
  log "status: cluster $CID ($PRESET / $INSTANCE_TYPE in $AWS_AZ, spot)"
  local canned=""
  local i
  for i in "${!NODE_NAMES[@]}"; do
    canned+="${NODE_IDS[$i]}"$'\t'"$INSTANCE_TYPE"$'\t'"running"$'\t'"${NODE_PUB[$i]}"$'\t'"${NODE_PRIV[$i]}"$'\n'
  done
  # shellcheck disable=SC2016  # JMESPath: backtick literals must NOT shell-expand
  awsq "$canned" ec2 describe-instances \
    --filters "Name=tag:$TAG_KEY,Values=$CID" "Name=instance-state-name,Values=pending,running,stopping,stopped" \
    --query 'Reservations[].Instances[].[InstanceId,InstanceType,State.Name,PublicIpAddress,PrivateIpAddress,Tags[?Key==`Name`]|[0].Value]' \
    --output table
  if [ -n "$LAUNCH_EPOCH" ]; then
    local now elapsed_h
    now="$(date +%s)"
    elapsed_h=$(((now - LAUNCH_EPOCH + 3599) / 3600))
    echo
    echo "elapsed: ~${elapsed_h} cluster-hour(s); estimated spend: ${elapsed_h}x $EST_CLUSTER_HOURLY"
    if [ -n "$DEADLINE_EPOCH" ]; then
      if [ "$now" -lt "$DEADLINE_EPOCH" ]; then
        echo "deadline guard: teardown in $(((DEADLINE_EPOCH - now) / 60)) min (pid ${DEADLINE_PID:-?})"
      else
        warn "deadline PASSED — the guard should have torn this down; run teardown NOW"
      fi
    fi
  fi
}

# ---------------------------------------------------------------------------
# teardown — idempotent; ends with a loud tag-scoped sweep.
# ---------------------------------------------------------------------------
cmd_teardown() {
  require_local_tools
  load_state --placeholder-ok --cid-only-ok
  confirm "teardown terminates every instance and deletes every AWS resource tagged $TAG_KEY=$CID."

  log "teardown 1/5: cancel deadline guard"
  if [ -n "$DEADLINE_PID" ] && [ "$DEADLINE_PID" != "dryrun" ]; then
    run kill "$DEADLINE_PID" || true
  elif $DRY_RUN; then
    echo "+ kill <deadline-guard-pid>"
  fi

  log "teardown 2/5: terminate instances (by tag — catches strays state missed)"
  local ids
  ids="$(awsq "i-dryrun0 i-dryrun1 i-dryrun2 i-dryrun3 i-dryrun4 i-dryrun5" ec2 describe-instances \
    --filters "Name=tag:$TAG_KEY,Values=$CID" "Name=instance-state-name,Values=pending,running,stopping,stopped" \
    --query 'Reservations[].Instances[].InstanceId' --output text | tr '\n' ' ' | tr -s ' ')"
  ids="${ids# }"; ids="${ids% }"
  if [ -n "$ids" ]; then
    # shellcheck disable=SC2086  # deliberate word-split: ids is a space-joined instance-id list
    awsc ec2 terminate-instances --instance-ids $ids
    # shellcheck disable=SC2086
    awsc ec2 wait instance-terminated --instance-ids $ids
  else
    echo "  no live instances tagged $TAG_KEY=$CID (already terminated)"
  fi

  log "teardown 3/5: delete launch template"
  local lt
  lt="$(awsq "lt-dryrun" ec2 describe-launch-templates \
    --filters "Name=launch-template-name,Values=$CID" \
    --query 'LaunchTemplates[0].LaunchTemplateId' --output text || echo None)"
  if [ -n "$lt" ] && [ "$lt" != "None" ]; then
    awsc ec2 delete-launch-template --launch-template-id "$lt"
  else
    echo "  launch template already gone"
  fi

  log "teardown 4/5: delete security group (retries: ENI detach lag) + placement group"
  local sg try deleted
  sg="$(awsq "sg-dryrun" ec2 describe-security-groups \
    --filters "Name=group-name,Values=$CID" \
    --query 'SecurityGroups[0].GroupId' --output text || echo None)"
  if [ -n "$sg" ] && [ "$sg" != "None" ]; then
    deleted=false
    for try in $(seq 1 12); do
      if awsc ec2 delete-security-group --group-id "$sg" 2>/dev/null; then
        deleted=true; break
      fi
      echo "  SG $sg still has dependencies (try $try/12) — waiting 10s"
      $DRY_RUN && { deleted=true; break; }
      sleep 10
    done
    $deleted || warn "could not delete SG $sg — the final sweep below will fail loudly"
  else
    echo "  security group already gone"
  fi
  if awsq "$CID" ec2 describe-placement-groups --filters "Name=group-name,Values=$CID" \
      --query 'PlacementGroups[0].GroupName' --output text | grep -q "$CID"; then
    awsc ec2 delete-placement-group --group-name "$CID"
  else
    echo "  placement group already gone"
  fi

  log "teardown 5/5: FINAL SWEEP — anything still billing tagged $TAG_KEY=$CID?"
  local leftovers=""
  local live_inst live_sg live_lt live_pg
  live_inst="$(awsq "" ec2 describe-instances \
    --filters "Name=tag:$TAG_KEY,Values=$CID" "Name=instance-state-name,Values=pending,running,stopping,stopped" \
    --query 'Reservations[].Instances[].InstanceId' --output text)"
  live_sg="$(awsq "" ec2 describe-security-groups --filters "Name=group-name,Values=$CID" \
    --query 'SecurityGroups[].GroupId' --output text)"
  live_lt="$(awsq "" ec2 describe-launch-templates --filters "Name=launch-template-name,Values=$CID" \
    --query 'LaunchTemplates[].LaunchTemplateId' --output text || true)"
  live_pg="$(awsq "" ec2 describe-placement-groups \
    --filters "Name=group-name,Values=$CID" \
    --query 'PlacementGroups[].GroupName' --output text || true)"
  if [ -n "$live_inst" ]; then leftovers+="  instances: $live_inst"$'\n'; fi
  if [ -n "$live_sg" ]; then leftovers+="  security groups: $live_sg"$'\n'; fi
  if [ -n "$live_lt" ]; then leftovers+="  launch templates: $live_lt"$'\n'; fi
  if [ -n "$live_pg" ]; then leftovers+="  placement groups: $live_pg"$'\n'; fi
  if [ -n "$leftovers" ]; then
    printf 'STILL-BILLING RESOURCES REMAIN for %s:\n%s' "$CID" "$leftovers" >&2
    die "teardown INCOMPLETE — remove the resources above manually (aws console/cli), then re-run teardown to re-verify"
  fi
  echo "  sweep clean: nothing tagged $TAG_KEY=$CID remains"

  # other clusters' strays are not a failure here, but say so loudly
  local other
  # shellcheck disable=SC2016  # JMESPath: backtick literals must NOT shell-expand
  other="$(awsq "" ec2 describe-instances \
    --filters "Name=tag-key,Values=$TAG_KEY" "Name=instance-state-name,Values=pending,running" \
    --query 'Reservations[].Instances[].[InstanceId,Tags[?Key==`squeezefs-bench`]|[0].Value]' --output text)"
  if [ -n "$other" ]; then
    warn "OTHER squeezefs-bench clusters still running (not this one's problem, but check billing):"$'\n'"$other"
  fi

  if ! $DRY_RUN && [ -d "$STATE_ROOT/$CID" ]; then
    rm -rf "$STATE_ROOT/${CID}.torn-down"
    mv "$STATE_ROOT/$CID" "$STATE_ROOT/${CID}.torn-down"
    if [ "$(cat "$STATE_ROOT/current" 2>/dev/null)" = "$CID" ]; then
      rm -f "$STATE_ROOT/current"
    fi
  fi
  echo
  echo "Cluster $CID torn down; nothing is billing."
}

# ---------------------------------------------------------------------------
# full — launch -> deploy -> assemble -> bench -> teardown (PRESET=mw swaps
# in assemble-mw/bench-mw). The global EXIT trap best-effort-tears-down on
# any failure past a successful launch.
# ---------------------------------------------------------------------------
cmd_full() {
  FULL_ACTIVE=true
  cmd_launch
  ASSUME_YES=true       # the cost confirmation already happened at launch
  cmd_deploy
  if [ "$PRESET" = "mw" ] && [ "$SYMMETRIC" = "1" ]; then
    cmd_assemble_sym
    cmd_bench_sym
  elif [ "$PRESET" = "mw" ]; then
    cmd_assemble_mw
    cmd_bench_mw
  else
    cmd_assemble
    cmd_bench
  fi
  cmd_teardown
}

# ---------------------------------------------------------------------------
# dispatch
# ---------------------------------------------------------------------------
trap global_exit_trap EXIT
case "$SUBCMD" in
  launch)            cmd_launch ;;
  deploy)            cmd_deploy ;;
  assemble)          cmd_assemble ;;
  assemble-mw)       cmd_assemble_mw ;;
  assemble-sym)      cmd_assemble_sym ;;
  bench)             cmd_bench ;;
  bench-mw)          cmd_bench_mw ;;
  bench-sym)         cmd_bench_sym ;;
  sym-hook)          cmd_sym_hook ;;
  status)            cmd_status ;;
  teardown)          cmd_teardown ;;
  full)              cmd_full ;;
  __deadline-guard)  cmd_deadline_guard ;;
  -h|--help|help)    usage ;;
  *)                 usage; die "unknown subcommand: $SUBCMD" ;;
esac
