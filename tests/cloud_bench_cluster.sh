#!/usr/bin/env bash
#
# cloud_bench_cluster.sh — stand up, exercise, and tear down an occasional
# SqueezeFS benchmark cluster on AWS EC2 SPOT instances.
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
#     needs 288 vCPUs.
#   * Pre-built artifacts in $ARTIFACT_DIR (this script DEPLOYS, it does not
#     build): `squeezefs` (linux-gnu, glibc ≤ the AMI's — use the
#     `task build:ubuntu2404` dist output), optionally `libsqueezefs_il.so`,
#     and a DYNAMIC `elbencho` binary (the pinned static one cannot load the
#     il shim; dynamic is the house rule for any instrument that may run il).
#
# Cost table (spot prices move; these are planning numbers, and the
# max-spend guard below is the real protection):
#
#   preset  instance        x6 cluster  instance store/node        est. spot $/hr
#   ------  --------------  ----------  -------------------------  --------------
#   i4i     i4i.4xlarge     96 vCPU     1 x 3,750 GB Nitro NVMe    ~$3-4/hr   (the IOPS venue)
#   i3en    i3en.12xlarge   288 vCPU    4 x 7,500 GB NVMe          ~$10-14/hr (the throughput venue)
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

# ============================ CONFIG ========================================
# --- AWS placement --------------------------------------------------------
AWS_REGION="${AWS_REGION:-us-east-1}"
AWS_AZ="${AWS_AZ:-us-east-1a}"

# --- Instance preset ------------------------------------------------------
# i4i    = i4i.4xlarge  x N  (~$3-4/hr cluster on spot)   — the IOPS venue
# i3en   = i3en.12xlarge x N (~$10-14/hr cluster on spot) — the throughput venue
# custom = use INSTANCE_TYPE below verbatim (still burst-class-refused)
PRESET="${PRESET:-i4i}"
INSTANCE_TYPE="${INSTANCE_TYPE:-}"          # only read when PRESET=custom

# --- Node roles (default 2 mds + 2 oss + 1 client + 1 spare = 6 nodes) -----
# The spare is a job worker / second load generator (`squeezefs job worker`,
# extra elbencho client); the battery itself drives from client0.
N_MDS="${N_MDS:-2}"
N_OSS="${N_OSS:-2}"
N_CLIENT="${N_CLIENT:-1}"
N_SPARE="${N_SPARE:-1}"

# --- SSH ---------------------------------------------------------------
KEY_NAME="${KEY_NAME:-squeezefs-bench}"          # EC2 key pair name (must exist in region)
SSH_KEY_FILE="${SSH_KEY_FILE:-$HOME/.ssh/squeezefs-bench.pem}"
REMOTE_USER="${REMOTE_USER:-ubuntu}"             # matches the Ubuntu 24.04 AMI below
OPERATOR_CIDR="${OPERATOR_CIDR:-}"               # empty = auto-detect via checkip.amazonaws.com

# --- Max-spend guard (REQUIRED for launch/full) -----------------------------
# Integer cluster-hours. launch refuses without it and installs a detached
# teardown-at-deadline process as the safety net.
MAX_CLUSTER_HOURS="${MAX_CLUSTER_HOURS:-}"

# --- Artifacts to deploy (built elsewhere — task build:ubuntu2404) ----------
ARTIFACT_DIR="${ARTIFACT_DIR:-dist/ubuntu2404}"
ELBENCHO_BIN="${ELBENCHO_BIN:-$ARTIFACT_DIR/elbencho}"   # DYNAMIC build (house rule)

# --- Filesystem shape (mirrors tests/cluster_reset.sh) ----------------------
NQN_PREFIX="nqn.2026-07.io.squeezefs"
MOUNTPOINT="/scratch/mnt"
CACHE_DIR="/scratch/cache"
MOUNT_EXTRA="--interception --allow-other --log-file /tmp/sqz.log"

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
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new)
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
             install runtime deps (fuse3)
  assemble   share instance-store NVMe from storage nodes (squeezefs nvmeof
             share, nvmet), connect from the client (single-path — one NIC),
             format, mount, build_commit verification
  bench      run the standing elbencho battery; results land in
             .benchmarks/cloud/<timestamp>/ with full row labeling
  status     instance table, elapsed cluster-hours vs the max-spend guard,
             estimated spend
  teardown   terminate instances, delete SG/launch template/placement group,
             cancel the deadline guard, tag-scoped final sweep (fails loudly
             on any still-billing resource). Idempotent.
  full       launch -> deploy -> assemble -> bench -> teardown

flags:
  --dry-run          print every aws/ssh command instead of executing (no
                     credentials needed)
  --yes              skip the typed-YES confirmations (used by the deadline
                     guard and the failure trap)
  --cluster-id ID    operate on a specific cluster (default: the one recorded
                     in .cloud-bench/current)
  --preset P         i4i | i3en | custom (overrides $PRESET)

The max-spend guard: launch/full refuse unless MAX_CLUSTER_HOURS is a
positive integer. See the header quickstart for the cost table.
EOF
}

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
SUBCMD="${1:-}"
[ -n "$SUBCMD" ] || { usage; exit 1; }
shift || true

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
    --secs)       GUARD_SECS="${2:?--secs needs a value}"; shift ;;  # __deadline-guard only
    -h|--help)    usage; exit 0 ;;
    *)            die "unknown flag: $1 (see --help)" ;;
  esac
  shift
done

# ---------------------------------------------------------------------------
# Preset resolution
# ---------------------------------------------------------------------------
case "$PRESET" in
  i4i)
    INSTANCE_TYPE="i4i.4xlarge"
    EST_CLUSTER_HOURLY="~\$3-4/hr (spot, 6 nodes)"
    ;;
  i3en)
    INSTANCE_TYPE="i3en.12xlarge"
    EST_CLUSTER_HOURLY="~\$10-14/hr (spot, 6 nodes)"
    ;;
  custom)
    [ -n "$INSTANCE_TYPE" ] || die "PRESET=custom requires INSTANCE_TYPE"
    EST_CLUSTER_HOURLY="unknown (custom preset — check spot pricing yourself)"
    ;;
  *) die "PRESET must be i4i | i3en | custom (got: $PRESET)" ;;
esac

# Burst-class refusal — credit-throttled CPU/network + no instance store means
# every row would be INVALID under the labeling discipline. Non-negotiable.
case "$INSTANCE_TYPE" in
  t2.*|t3.*|t3a.*|t4g.*)
    die "burst-class instance type '$INSTANCE_TYPE' refused: CPU-credit throttling makes medians a function of credit balance, and there is no instance store. Use the i4i/i3en presets."
    ;;
esac

N_TOTAL=$((N_MDS + N_OSS + N_CLIENT + N_SPARE))
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
# Env values must not contain spaces (use comma-separated lists).
remote() {
  local ip="$1"; shift
  local envs=("$@")
  local script
  script="$(cat)"
  if $DRY_RUN; then
    printf "+ ssh %s@%s sudo env %s bash -s <<'EOS'\n" "$REMOTE_USER" "$ip" "${envs[*]:-}"
    printf '%s\nEOS\n' "$script"
  else
    ssh "${SSH_OPTS[@]}" -i "$SSH_KEY_FILE" "$REMOTE_USER@$ip" \
      "sudo env ${envs[*]:-} bash -s" <<<"$script"
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
        CID=*|AWS_REGION=*|AWS_AZ=*|PRESET=*|INSTANCE_TYPE=*|LT_ID=*|SG_ID=*|PG_NAME=*|VPC_ID=*|SUBNET_ID=*|LAUNCH_EPOCH=*|DEADLINE_EPOCH=*|DEADLINE_PID=*)
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

cmd_launch() {
  require_local_tools

  # --- Max-spend guard: refuse to launch without it -----------------------
  [[ "$MAX_CLUSTER_HOURS" =~ ^[0-9]+$ ]] && [ "$MAX_CLUSTER_HOURS" -ge 1 ] \
    || die "max-spend guard not set: export MAX_CLUSTER_HOURS=<positive integer hours>. The script will not launch billing instances without a deadline (preset $PRESET costs $EST_CLUSTER_HOURLY)."
  if ! $DRY_RUN; then
    [ -f "$SSH_KEY_FILE" ] || die "SSH key file not found: $SSH_KEY_FILE"
  fi

  CID="sqzbench-$(date +%Y%m%d-%H%M%S)"
  STATE_DIR="$STATE_ROOT/$CID"
  PG_NAME="$CID"

  confirm "About to launch $N_TOTAL x $INSTANCE_TYPE SPOT instances in $AWS_AZ ($EST_CLUSTER_HOURLY), max $MAX_CLUSTER_HOURS cluster-hour(s), cluster id $CID. This COSTS MONEY."

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

  log "launch 2/7: AMI (Ubuntu 24.04 LTS via SSM) + default subnet in $AWS_AZ"
  local ami
  ami="$(awsq "ami-dryrun" ssm get-parameter \
    --name /aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id \
    --query Parameter.Value --output text)"
  SUBNET_ID="$(awsq "subnet-dryrun" ec2 describe-subnets \
    --filters "Name=availability-zone,Values=$AWS_AZ" "Name=default-for-az,Values=true" \
    --query 'Subnets[0].SubnetId' --output text)"
  [ "$SUBNET_ID" != "None" ] || die "no default subnet in $AWS_AZ (default VPC required; it auto-assigns the public IPs SSH needs)"
  VPC_ID="$(awsq "vpc-dryrun" ec2 describe-subnets --subnet-ids "$SUBNET_ID" \
    --query 'Subnets[0].VpcId' --output text)"
  echo "  ami=$ami subnet=$SUBNET_ID vpc=$VPC_ID"

  log "launch 3/7: placement group (cluster strategy) + security group"
  # cluster placement = same-rack networking; note: it narrows the spot pool,
  # which capacity-optimized allocation partially compensates for.
  awsc ec2 create-placement-group --group-name "$PG_NAME" --strategy cluster \
    --tag-specifications "ResourceType=placement-group,Tags=[{Key=$TAG_KEY,Value=$CID}]"
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
  LT_ID="$(awsq "lt-dryrun" ec2 create-launch-template \
    --launch-template-name "$CID" \
    --tag-specifications "ResourceType=launch-template,Tags=[{Key=$TAG_KEY,Value=$CID}]" \
    --launch-template-data "{\"ImageId\":\"$ami\",\"KeyName\":\"$KEY_NAME\",\"SecurityGroupIds\":[\"$SG_ID\"],\"Placement\":{\"GroupName\":\"$PG_NAME\"},\"TagSpecifications\":[{\"ResourceType\":\"instance\",\"Tags\":[{\"Key\":\"$TAG_KEY\",\"Value\":\"$CID\"}]}]}" \
    --query 'LaunchTemplate.LaunchTemplateId' --output text)"
  # capacity-optimized = fewest interruptions, which is what protects the
  # counted-run discipline. type=instant returns instance ids synchronously.
  local canned_ids i
  canned_ids=""
  for ((i = 0; i < N_TOTAL; i++)); do canned_ids+="i-dryrun$i "; done
  local ids_text
  ids_text="$(awsq "$canned_ids" ec2 create-fleet --type instant \
    --launch-template-configs "[{\"LaunchTemplateSpecification\":{\"LaunchTemplateId\":\"$LT_ID\",\"Version\":\"\$Latest\"},\"Overrides\":[{\"InstanceType\":\"$INSTANCE_TYPE\",\"SubnetId\":\"$SUBNET_ID\",\"AvailabilityZone\":\"$AWS_AZ\"}]}]" \
    --spot-options 'AllocationStrategy=capacity-optimized,InstanceInterruptionBehavior=terminate' \
    --target-capacity-specification "TotalTargetCapacity=$N_TOTAL,DefaultTargetCapacityType=spot" \
    --tag-specifications "ResourceType=fleet,Tags=[{Key=$TAG_KEY,Value=$CID}]" \
    --query 'Instances[].InstanceIds[]' --output text)"
  mapfile -t NODE_IDS < <(xargs -n1 <<<"$ids_text")
  [ "${#NODE_IDS[@]}" -eq "$N_TOTAL" ] \
    || die "spot fleet delivered ${#NODE_IDS[@]}/$N_TOTAL instances (insufficient spot capacity for $INSTANCE_TYPE in $AWS_AZ?) — tearing down"
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
# deploy — push artifacts + runtime deps. Deploys only; never builds.
# ---------------------------------------------------------------------------
cmd_deploy() {
  require_local_tools
  load_state --placeholder-ok

  local sqz="$ARTIFACT_DIR/squeezefs"
  local shim="$ARTIFACT_DIR/libsqueezefs_il.so"
  if ! $DRY_RUN; then
    [ -x "$sqz" ] || die "squeezefs artifact missing/not executable: $sqz (build elsewhere: task build:ubuntu2404)"
    [ -x "$ELBENCHO_BIN" ] || die "elbencho artifact missing: $ELBENCHO_BIN (must be a DYNAMIC build)"
  fi
  local sqz_sha="dryrun-sha"
  if ! $DRY_RUN; then
    sqz_sha="$(sha256sum "$sqz" | awk '{print $1}')"
  fi

  local idx name ip
  for idx in "${!NODE_NAMES[@]}"; do
    name="${NODE_NAMES[$idx]}"; ip="${NODE_PUB[$idx]}"
    log "deploy: $name ($ip)"
    remote "$ip" <<'EOS'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get -qq update
apt-get -qq install -y fuse3 nvme-cli >/dev/null
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
      push "$ELBENCHO_BIN" "$ip" "$REMOTE_DIR/elbencho"
      if $DRY_RUN || [ -f "$shim" ]; then
        push "$shim" "$ip" "$REMOTE_DIR/libsqueezefs_il.so"
      else
        warn "no il shim at $shim — il rows unavailable on this cluster (kernel-FUSE rows unaffected)"
      fi
    fi
  done
  echo
  echo "Artifacts deployed (sha256 $sqz_sha). Next: tests/cloud_bench_cluster.sh assemble"
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
  remote "$client_ip" SQZ="$REMOTE_DIR/squeezefs" MNT="$MOUNTPOINT" NQN_PREFIX="$NQN_PREFIX" <<'EOS'
set -euo pipefail
if mountpoint -q "$MNT" 2>/dev/null; then
  umount "$MNT" || umount -l "$MNT" || true
  sleep 2
fi
pkill -f 'squeezefs moun[t]' 2>/dev/null || true
sleep 1
# LOUD disconnect verification (survivors auto-reconnect to rebuilt shares
# and poison the connect step — same failure mode as tests/cluster_reset.sh)
for c in /sys/class/nvme/nvme*/subsysnqn; do
  [ -e "$c" ] || continue
  nqn=$(cat "$c")
  case "$nqn" in "$NQN_PREFIX"*) "$SQZ" nvmeof disconnect --subnqn "$nqn" || true ;; esac
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

  log "assemble 2/4: storage nodes — instance-store NVMe discovery + nvmet share"
  # One script for every storage node, parameterized purely via env (kept in
  # a variable so the dry-run print and the real run can never diverge).
  # Instance-store devices carry model "Amazon EC2 NVMe Instance Storage";
  # EBS devices ("Amazon Elastic Block Store") are deliberately excluded.
  local storage_share_script
  storage_share_script="$(cat <<'EOS'
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
  echo "SHARED $nqn"
  i=$((i + 1))
done
EOS
)"
  local meta_specs="" data_specs=""   # csv of nqn@private_ip
  local idx name role ip priv shares nqn
  for idx in "${!NODE_NAMES[@]}"; do
    name="${NODE_NAMES[$idx]}"
    role="$(role_of "$name")"
    { [ "$role" = "mds" ] || [ "$role" = "oss" ]; } || continue
    ip="${NODE_PUB[$idx]}"; priv="${NODE_PRIV[$idx]}"
    echo "-- $name ($ip, fabric $priv)"
    # mds shares only its first instance-store device (meta needs little);
    # oss shares every instance-store namespace it has (i4i.4xlarge: 1,
    # i3en.12xlarge: 4).
    if $DRY_RUN; then
      remote "$ip" NAME="$name" KIND="$role" NQN_BASE="$NQN_PREFIX" PRIV_IP="$priv" SQZ="$REMOTE_DIR/squeezefs" \
        <<<"$storage_share_script"
      shares="$NQN_PREFIX:$name-d0"     # canned: 1 device/node in dry-run
    else
      shares="$(remote "$ip" NAME="$name" KIND="$role" NQN_BASE="$NQN_PREFIX" PRIV_IP="$priv" SQZ="$REMOTE_DIR/squeezefs" \
        <<<"$storage_share_script" | awk '/^SHARED /{print $2}')"
      [ -n "$shares" ] || die "$name shared nothing"
    fi
    for nqn in $shares; do
      if [ "$role" = "mds" ]; then
        meta_specs="${meta_specs:+$meta_specs,}$nqn@$priv"
      else
        data_specs="${data_specs:+$data_specs,}$nqn@$priv"
      fi
      echo "   shared $nqn @ $priv"
    done
  done
  [ -n "$meta_specs" ] && [ -n "$data_specs" ] || die "assemble produced empty share lists (meta='$meta_specs' data='$data_specs')"

  log "assemble 3/4: client — single-path connect, format, mount"
  # Cloud note: these instances have ONE NIC, so this is a SINGLE-PATH
  # connect per subsystem — no second-path loop and no iopolicy step (the
  # round-robin iopolicy write in tests/cluster_reset.sh only matters with
  # two fabric paths; with one path it is a no-op, so it is skipped here
  # on purpose).
  remote "$client_ip" \
    SQZ="$REMOTE_DIR/squeezefs" MNT="$MOUNTPOINT" CACHE="$CACHE_DIR" \
    META_SPECS="$meta_specs" DATA_SPECS="$data_specs" \
    MOUNT_EXTRA_STR="$(printf '%s' "$MOUNT_EXTRA" | tr ' ' ',')" <<'EOS'
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
    [ "$live" = 1 ] || "$SQZ" nvmeof connect --ip "$ip" --subnqn "$nqn"
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
META_URI="sqmeta://$meta_devs"
DATA_URI="sqdata://$data_devs"
echo "format: $META_URI $DATA_URI"
"$SQZ" format "$META_URI" "$DATA_URI" --disk-cache-paths "$CACHE"

read -ra EXTRA <<<"$(printf '%s' "$MOUNT_EXTRA_STR" | tr ',' ' ')"
"$SQZ" mount "$META_URI" "$MNT" --daemon "${EXTRA[@]}"
sleep 2
mountpoint -q "$MNT" || { echo "mount did not come up" >&2; exit 1; }
echo "$META_URI" >/etc/squeezefs-bench-meta-uri
EOS

  log "assemble 4/4: build_commit verification ritual"
  # The mounted daemon must be the deployed artifact: the .stats build_commit
  # must appear in `squeezefs --version` of the deployed binary.
  remote "$client_ip" SQZ="$REMOTE_DIR/squeezefs" MNT="$MOUNTPOINT" <<'EOS'
set -euo pipefail
ver="$("$SQZ" --version)"
commit="$(grep -oE '"build_commit": *"[0-9a-f]+"' "$MNT/.stats" | grep -oE '[0-9a-f]{7,40}' | head -1)"
[ -n "$commit" ] || { echo "no build_commit in $MNT/.stats" >&2; exit 1; }
case "$ver" in
  *"$commit"*) echo "build_commit OK: $commit ($ver)" ;;
  *) echo "build_commit MISMATCH: mounted daemon reports $commit but deployed binary is '$ver'" >&2; exit 1 ;;
esac
EOS
  echo
  echo "Cluster assembled and mounted at $MOUNTPOINT on client0. Next: tests/cloud_bench_cluster.sh bench"
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
  echo "# substrate=aws-spot/$INSTANCE_TYPE/$AWS_AZ (instance-store NVMe over nvmet-tcp, single NIC)"
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
    echo "cluster=$CID preset=$PRESET instance_type=$INSTANCE_TYPE az=$AWS_AZ region=$AWS_REGION market=spot"
    echo "instrument=$ELB_VER"
    echo "substrate=aws-spot/$INSTANCE_TYPE/$AWS_AZ — a THIRD substrate class: never mix into devsub loop/tcp medians"
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
# full — launch -> deploy -> assemble -> bench -> teardown. The global EXIT
# trap best-effort-tears-down on any failure past a successful launch.
# ---------------------------------------------------------------------------
cmd_full() {
  FULL_ACTIVE=true
  cmd_launch
  ASSUME_YES=true       # the cost confirmation already happened at launch
  cmd_deploy
  cmd_assemble
  cmd_bench
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
  bench)             cmd_bench ;;
  status)            cmd_status ;;
  teardown)          cmd_teardown ;;
  full)              cmd_full ;;
  __deadline-guard)  cmd_deadline_guard ;;
  -h|--help|help)    usage ;;
  *)                 usage; die "unknown subcommand: $SUBCMD" ;;
esac
