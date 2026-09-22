#!/usr/bin/env bash
#
# cloud_sym_rows.sh — the symmetric program's MULTI-NODE row driver
# (design-symmetric-metadata §8 gates 2 / 3 / 3b; PR 15 `perf/sym-cloud-row`).
#
# The three acceptance row sets the box ran CO-LOCATED (run_mw_matrix.sh
# `sym-tarx` / `sym-scale` / `sym-shared-dir`(+`-ls`)), driven over ssh from
# the operator's box against ONE symmetric writer PER NODE — the venue gate
# 3's law ("aggregate create/s and ingest scale with N, bounded by no node")
# is written for. Every law is tests/sym_rows_lib.sh's — the SAME
# definitions run_mw_matrix.sh sources — so the cloud row and the box row
# read one law; this file owns only the venue (where a command runs, how a
# `.stats` inode is captured, how a node's mount is left and rejoined).
#
# Usage:
#   tests/cloud_sym_rows.sh [flags] manager=<host>:<mnt> writer=<host>:<mnt>...
#                           [reader=<host>:<mnt>]
#
#   <host> is `local` (run here — the laptop's fleet, the "it works" venue)
#   or an ssh target `[user@]ip`; every command runs as ROOT on the node
#   (sudo). The manager entry comes first; every writer is a JOINED writer
#   (its `.stats` reads `joined_appender_id ≥ 1`); the optional reader is a
#   `-o ro` TOKEN reader (the `-ls` half — SKIPPED loud without one).
#
# Flags (every one has a SQZ_CLOUDSYM_* default — harness variables, never
# a daemon's):
#   --rows=tarx,shared,scale    row sets to run (default all three, in
#                               that order — shared's -ls half ahead of
#                               scale's leave/rejoin storm)
#   --rt=S                      the sustained window per measured phase
#                               (default 60 — the AGENTS.md rule; the local
#                               scoping pass runs 10; 0 = one pass)
#   --files=N --threads=T       per-writer creates and threads (scale;
#                               shared sizes per_writer = files / creators);
#                               under --size-to-rt=auto these are FLOORS
#   --ingest-mb=M               per-writer ingest MiB (4 MiB blocks, fsync;
#                               a floor under --size-to-rt=auto)
#   --size-to-rt=auto|off       auto (the cloud venue's default): an N = 1
#                               PILOT on the manager (a short create storm,
#                               a 256 MiB ingest, a small shared-dir wave)
#                               sizes --files / --ingest-mb / the shared
#                               row's per-creator count so every RATE phase
#                               fills --rt (the sustained-state rule); off
#                               (the laptop's default) = the sizes as given
#   --ingest-cap-mb=M           the per-writer ingest ceiling the pilot may
#                               size up to (default 65536 — the cloud
#                               namespaces hold it; local zram does not)
#   --scale-ns=1,2,4,8          gate 3's N ladder (capped at the writers
#                               given + 1)
#   --tar-src=DIR | --tarball=F the `tar -x` corpus (the linux fs/ tree —
#                               the box used linux-7.2.3/fs, 2,468 entries;
#                               ship the SAME tarball to keep rows comparable)
#   --tarx-reps=R               extractions per arm (default 1 — the box's
#                               exact shape: gate 2 is a WALL RATIO, not a
#                               rate, and on a cache-less set every
#                               beyond-inline file is a whole 4 MiB block, so
#                               one fs/ extraction is ≈ 7 GiB of data blocks;
#                               0 = as many as fit --rt, each into a fresh
#                               subdir — sized for a volume that holds them)
#   --sqz=PATH                  the squeezefs binary ON THE NODES (default
#                               `squeezefs` in root's PATH)
#   --mdstorm-src=FILE          tests/mdstorm.c (compiled on every writer node)
#   --rowdir=DIR                results (snapshots, tables, verdicts, fsck)
#   --venue=cloud|laptop        the VENUE word (default cloud; laptop = the
#                               ruling's one venue-attributed gauge)
#   --substrate=LABEL --cluster=ID --instrument=TEXT --format=TEXT
#                               the row label words (--format = the set's
#                               format posture: cache-less vs staged)
#   --ssh-key=FILE --ssh-user=U --ssh-opt=OPT (repeatable)
#   --mount-hook=CMD            `CMD mount|unmount <host> <mnt>` — how a
#                               writer node's mount is LEFT and REJOINED
#                               (gate 3's "exactly N appenders live" and its
#                               deleted-stays-deleted-across-the-leave arm);
#                               without it the idle writers stay mounted
#                               and the row says so
#   --storage=host:dev,...      DATA namespaces on the storage nodes — the
#                               ingest row's amplification columns
#                               (/proc/diskstats deltas: device ÷ user bytes,
#                               wareq-sz) — n/a without it
#   --manager-priv=IP           the manager's fabric address (the writer
#                               node pings it — the row's measured RTT)
#   --dry-run                   print every ssh/local command, run nothing
#                               (the canned walls read as --rt, so a plain
#                               dry-run exits 0)
#   --test-force-sub-rt         the Issue-15 pin: every RATE phase's wall is
#                               judged as 0 s — on --venue=cloud the driver
#                               MUST exit nonzero (a dry-run transcript is
#                               the evidence); harness-only
#
# Exit: 0 = every row's engagement law GREEN and the oracle clean; nonzero
# on any violated law (the row is INVALID, never a number).
#
# The rows are the matrix's, verbatim in shape:
#   gate 2  sym-tarx      A-B-B-A: sym-1 (writer 1 extracts into a directory
#                         it created) local-1 local-2 (S0 = the MANAGER's
#                         own extract on its node — the matrix's shape:
#                         the joiners and the reader stay MOUNTED and idle,
#                         so the manager serves their renewals and token
#                         planes during S0; never a solo mount) sym-2; law
#                         ≤ 1.10× S0, verbs/entry < 0.05, handovers 0,
#                         rpcs 0.
#   gate 3  sym-scale     N ∈ ns: N writers (the manager + N−1 joiners) each
#                         create --files in its own directory then ingest
#                         --ingest-mb; ≥ 0.7 × N × the N=1 rate on both
#                         rows; appenders_known == N; handovers 0; ships ≤ N;
#                         rpcs 0; must-stay-0 deltas 0; deleted stays deleted
#                         across every joiner's clean leave; C/CPU-S beside
#                         the multiple.
#   gate 3b sym-shared-dir every writer creates into ONE directory the first
#                         joiner made: one flip at the holder, stripe ships
#                         > 0, shipped ≡ served, handovers 0; -ls: the
#                         reader's cold `ls -l` = K_D + K_root + C (+ ≤ 4)
#                         tokens (K_root = the mount root's stripes, 0
#                         unstriped — PR 13d), 0 data-leaf reads.
# After every row set: `fsck --json` on the manager (findings 0) +
# `meta_kv_block_refs_drift` / `data_alloc_bitmap_drift` 0 + the must-stay-0
# set on every writer.
#
set -euo pipefail
set -E
trap 'rc=$?; [ "$rc" = "0" ] || echo "[sym-rows] ERROR: exit $rc at line $LINENO: $BASH_COMMAND" >&2' ERR

REPO="$(cd "$(dirname "$0")/.." && pwd)"

# The LOCAL venue's mount hook: `tests/cloud_sym_rows.sh fleet-hook
# mount|unmount local <mnt>` maps the fleet rig's mountpoint
# (`<MNT_ROOT>/m<idx>`) to `tests/mw_fleet.sh mount|unmount <idx>` — the
# rig's own leave/rejoin verbs, so the local functional pass exercises the
# same "exactly N appenders live" arm the cloud rig's `sym-hook` does.
if [ "${1:-}" = "fleet-hook" ]; then
    verb="${2:?fleet-hook needs mount|unmount}"
    host="${3:?fleet-hook needs <host>}"
    mnt="${4:?fleet-hook needs <mnt>}"
    [ "$host" = local ] || { echo "[sym-rows] fleet-hook: host must be local (got '$host')" >&2; exit 1; }
    idx="${mnt##*/m}"
    [[ "$idx" =~ ^[0-9]+$ ]] || { echo "[sym-rows] fleet-hook: '$mnt' is not a fleet mountpoint (<root>/m<idx>)" >&2; exit 1; }
    case "$verb" in
    mount) exec sudo -n "$REPO/tests/mw_fleet.sh" mount "$idx" ;;
    unmount) exec sudo -n "$REPO/tests/mw_fleet.sh" unmount "$idx" ;;
    *) echo "[sym-rows] fleet-hook: verb must be mount|unmount (got '$verb')" >&2; exit 1 ;;
    esac
fi

log() { echo "[sym-rows] $*"; }
warn() { echo "[sym-rows] WARN: $*" >&2; }
die() {
    echo "[sym-rows] ERROR: $*" >&2
    # A died row leaves its background storms on the nodes — reap the ssh
    # children (the remote storms end with their ssh session).
    pkill -P $$ 2>/dev/null || true
    exit 1
}

# --- flags ------------------------------------------------------------------------
# Default order: the -ls half (a token reader's cold listing) runs BEFORE
# sym-scale's leave/rejoin storm, so the reader's per-holder planes stand
# for the listing (a writer that rejoined at another port replaces them).
ROWS="${SQZ_CLOUDSYM_ROWS:-tarx,shared,scale}"
RT="${SQZ_CLOUDSYM_RT:-60}"
FILES="${SQZ_CLOUDSYM_FILES:-40000}"
THREADS="${SQZ_CLOUDSYM_THREADS:-4}"
INGEST_MB="${SQZ_CLOUDSYM_INGEST_MB:-1024}"
SCALE_NS="${SQZ_CLOUDSYM_SCALE_NS:-1,2,4,8}"
SIZE_TO_RT="${SQZ_CLOUDSYM_SIZE_TO_RT:-}"
INGEST_CAP_MB="${SQZ_CLOUDSYM_INGEST_CAP_MB:-65536}"
TAR_SRC="${SQZ_CLOUDSYM_TAR_SRC:-}"
TARBALL="${SQZ_CLOUDSYM_TARBALL:-}"
TARX_REPS="${SQZ_CLOUDSYM_TARX_REPS:-1}"
SQZ_NODE="${SQZ_CLOUDSYM_SQZ:-squeezefs}"
MDSTORM_SRC="${SQZ_CLOUDSYM_MDSTORM_SRC:-$REPO/tests/mdstorm.c}"
ROWDIR="${SQZ_CLOUDSYM_ROWDIR:-}"
SYM_VENUE="${SQZ_CLOUDSYM_VENUE:-cloud}"
SUBSTRATE="${SQZ_CLOUDSYM_SUBSTRATE:-}"
CLUSTER="${SQZ_CLOUDSYM_CLUSTER:-}"
INSTRUMENT="${SQZ_CLOUDSYM_INSTRUMENT:-}"
FORMAT_LABEL="${SQZ_CLOUDSYM_FORMAT:-}"
SSH_KEY="${SQZ_CLOUDSYM_SSH_KEY:-}"
SSH_USER="${SQZ_CLOUDSYM_SSH_USER:-}"
SSH_EXTRA=()
MOUNT_HOOK="${SQZ_CLOUDSYM_MOUNT_HOOK:-}"
STORAGE="${SQZ_CLOUDSYM_STORAGE:-}"
MANAGER_PRIV="${SQZ_CLOUDSYM_MANAGER_PRIV:-}"
REMOTE_DIR="${SQZ_CLOUDSYM_REMOTE_DIR:-/tmp/sym-rows}"
DRY_RUN=false
FORCE_SUB_RT=false
ENTRIES=()
for a in "$@"; do
    case "$a" in
    --rows=*) ROWS="${a#--rows=}" ;;
    --rt=*) RT="${a#--rt=}" ;;
    --files=*) FILES="${a#--files=}" ;;
    --threads=*) THREADS="${a#--threads=}" ;;
    --ingest-mb=*) INGEST_MB="${a#--ingest-mb=}" ;;
    --scale-ns=*) SCALE_NS="${a#--scale-ns=}" ;;
    --size-to-rt=*) SIZE_TO_RT="${a#--size-to-rt=}" ;;
    --ingest-cap-mb=*) INGEST_CAP_MB="${a#--ingest-cap-mb=}" ;;
    --tar-src=*) TAR_SRC="${a#--tar-src=}" ;;
    --tarball=*) TARBALL="${a#--tarball=}" ;;
    --tarx-reps=*) TARX_REPS="${a#--tarx-reps=}" ;;
    --sqz=*) SQZ_NODE="${a#--sqz=}" ;;
    --mdstorm-src=*) MDSTORM_SRC="${a#--mdstorm-src=}" ;;
    --rowdir=*) ROWDIR="${a#--rowdir=}" ;;
    --venue=*) SYM_VENUE="${a#--venue=}" ;;
    --substrate=*) SUBSTRATE="${a#--substrate=}" ;;
    --cluster=*) CLUSTER="${a#--cluster=}" ;;
    --instrument=*) INSTRUMENT="${a#--instrument=}" ;;
    --format=*) FORMAT_LABEL="${a#--format=}" ;;
    --ssh-key=*) SSH_KEY="${a#--ssh-key=}" ;;
    --ssh-user=*) SSH_USER="${a#--ssh-user=}" ;;
    --ssh-opt=*) SSH_EXTRA+=("${a#--ssh-opt=}") ;;
    --mount-hook=*) MOUNT_HOOK="${a#--mount-hook=}" ;;
    --storage=*) STORAGE="${a#--storage=}" ;;
    --manager-priv=*) MANAGER_PRIV="${a#--manager-priv=}" ;;
    --remote-dir=*) REMOTE_DIR="${a#--remote-dir=}" ;;
    --dry-run) DRY_RUN=true ;;
    --test-force-sub-rt) FORCE_SUB_RT=true ;;
    -h | --help)
        awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
        exit 0
        ;;
    manager=* | writer=* | reader=*) ENTRIES+=("$a") ;;
    *) die "unknown argument '$a' (see --help)" ;;
    esac
done
case "$SYM_VENUE" in cloud | laptop) ;; *) die "--venue takes cloud|laptop (got '$SYM_VENUE')" ;; esac
[ -n "$SIZE_TO_RT" ] || SIZE_TO_RT="$([ "$SYM_VENUE" = cloud ] && echo auto || echo off)"
case "$SIZE_TO_RT" in auto | off) ;; *) die "--size-to-rt takes auto|off (got '$SIZE_TO_RT')" ;; esac
[[ "$INGEST_CAP_MB" =~ ^[0-9]+$ ]] && [ "$INGEST_CAP_MB" -ge 4 ] || die "--ingest-cap-mb takes MiB >= 4 (got '$INGEST_CAP_MB')"
[[ "$RT" =~ ^[0-9]+$ ]] || die "--rt takes seconds (got '$RT')"
[[ "$FILES" =~ ^[0-9]+$ ]] && [ "$FILES" -ge 100 ] || die "--files takes an integer ≥ 100 (got '$FILES')"
[[ "$THREADS" =~ ^[0-9]+$ ]] && [ "$THREADS" -ge 1 ] || die "--threads takes an integer ≥ 1 (got '$THREADS')"
[[ "$INGEST_MB" =~ ^[0-9]+$ ]] && [ "$INGEST_MB" -ge 4 ] && [ $((INGEST_MB % 4)) -eq 0 ] ||
    die "--ingest-mb takes a multiple of 4 MiB ≥ 4 (got '$INGEST_MB')"
[[ "$TARX_REPS" =~ ^[0-9]+$ ]] || die "--tarx-reps takes an integer ≥ 0 (got '$TARX_REPS'; an empty value would loop the node's extraction unbounded)"
[ "${#ENTRIES[@]}" -ge 1 ] || die "no mounts given — see --help (manager=<host>:<mnt> writer=<host>:<mnt> …)"

# --- the node table ---------------------------------------------------------------
# idx 0 = the manager, 60.. = the joined writers (the fleet rig's JOINER_BASE
# slice, so a snapshot file reads like the matrix's), 1 = the token reader.
declare -A HOST=() MNT=() ROLE=()
WRITERS=()
READER=""
next_w=60
for e in "${ENTRIES[@]}"; do
    role="${e%%=*}"
    spec="${e#*=}"
    host="${spec%%:*}"
    mnt="${spec#*:}"
    [ -n "$host" ] && [ -n "$mnt" ] && [ "$mnt" != "$spec" ] || die "malformed entry '$e' (want role=<host>:<mnt>)"
    [[ "$mnt" = /* ]] || die "entry '$e': the mountpoint must be absolute"
    case "$role" in
    manager)
        [ -z "${HOST[0]:-}" ] || die "two manager entries"
        HOST[0]="$host" MNT[0]="$mnt" ROLE[0]=manager
        ;;
    writer)
        HOST[$next_w]="$host" MNT[$next_w]="$mnt" ROLE[$next_w]=writer
        WRITERS+=("$next_w")
        next_w=$((next_w + 1))
        ;;
    reader)
        [ -z "$READER" ] || die "two reader entries"
        HOST[1]="$host" MNT[1]="$mnt" ROLE[1]=reader
        READER=1
        ;;
    esac
done
[ -n "${HOST[0]:-}" ] || die "no manager= entry"
[ "${#WRITERS[@]}" -ge 1 ] || die "no writer= entry — the rows need ≥ 1 joined writer (gate 3b needs ≥ 2)"

# --- execution: local or ssh, always root, dry-run prints --------------------------
SSH_BASE=(-o BatchMode=yes -o ConnectTimeout=15 -o StrictHostKeyChecking=accept-new -o IdentitiesOnly=yes -o ServerAliveInterval=15)
ssh_target() { # host -> [user@]host
    local h="$1"
    if [ -n "$SSH_USER" ] && [[ "$h" != *@* ]]; then echo "$SSH_USER@$h"; else echo "$h"; fi
}
ssh_cmd() { # -> the ssh argv prefix for interactive-less root exec
    printf '%s\n' ssh "${SSH_BASE[@]}" "${SSH_EXTRA[@]}"
    [ -n "$SSH_KEY" ] && printf '%s\n' -i "$SSH_KEY"
    return 0
}

# rx <idx> [VAR=val ...] — the root script arrives on stdin (heredoc).
# Prints the script's stdout. Env values must not contain spaces.
rx() {
    local idx="$1"
    shift
    local host="${HOST[$idx]}" script
    script="$(cat)"
    if $DRY_RUN; then
        if [ "$host" = local ]; then
            printf "+ [m%s local] sudo env %s bash -s <<'EOS'\n%s\nEOS\n" "$idx" "$*" "$script" >&2
        else
            printf "+ [m%s] ssh %s sudo env %s bash -s <<'EOS'\n%s\nEOS\n" "$idx" "$(ssh_target "$host")" "$*" "$script" >&2
        fi
        printf '%s\n' "${RX_CANNED:-}"
        return 0
    fi
    if [ "$host" = local ]; then
        if [ "$(id -u)" = 0 ]; then
            env "$@" bash -s <<<"$script"
        else
            sudo -n env "$@" bash -s <<<"$script"
        fi
    else
        local -a sshv
        mapfile -t sshv < <(ssh_cmd)
        "${sshv[@]}" "$(ssh_target "$host")" "sudo env $* bash -s" <<<"$script"
    fi
}

# rx_bg <idx> <outfile> [VAR=val ...] — like rx, detached; the caller waits
# on the pid ($!) — the row's parallel storms.
rx_bg() {
    local idx="$1" out="$2" script
    shift 2
    # A backgrounded command's default stdin is /dev/null — the heredoc is
    # read HERE and handed to the child explicitly.
    script="$(cat)"
    if $DRY_RUN; then
        # the printed command rides the inherited stderr (never a re-opened
        # /dev/stderr — that truncates a redirected transcript)
        rx "$idx" "$@" <<<"$script" >/dev/null &
    else
        rx "$idx" "$@" <<<"$script" >"$out" 2>"$out.err" &
    fi
}

# push_file <idx> <local-file> <remote-path>
push_file() {
    local idx="$1" src="$2" dst="$3" host
    host="${HOST[$idx]}"
    if $DRY_RUN; then
        if [ "$host" = local ]; then
            echo "+ [m$idx local] install -D -m 0644 $src $dst" >&2
        else
            echo "+ [m$idx] scp $src $(ssh_target "$host"):/tmp/$(basename "$dst") && sudo install -D -m 0644 /tmp/$(basename "$dst") $dst" >&2
        fi
        return 0
    fi
    if [ "$host" = local ]; then
        rx "$idx" SRC="$src" DST="$dst" <<'EOS'
set -euo pipefail
install -D -m 0644 "$SRC" "$DST"
EOS
    else
        local -a scpv=(scp "${SSH_BASE[@]}" "${SSH_EXTRA[@]}")
        [ -n "$SSH_KEY" ] && scpv+=(-i "$SSH_KEY")
        "${scpv[@]}" "$src" "$(ssh_target "$host"):/tmp/$(basename "$dst")"
        rx "$idx" SRC="/tmp/$(basename "$dst")" DST="$dst" <<'EOS'
set -euo pipefail
install -D -m 0644 "$SRC" "$DST"
EOS
    fi
}

# --- the shared laws --------------------------------------------------------------
ROWDIR="${ROWDIR:-$REPO/target/sym-rows/$(date +%Y-%m-%d-%H%M%S)}"
# shellcheck disable=SC2034  # read by the lib's sym_zero_venue_note
SYM_VENUE_LEDGER="$ROWDIR/venue-attributed.txt"
# shellcheck disable=SC2034  # the lib's stderr prefix
SYM_LOG_TAG="[sym-rows]"
# shellcheck source=tests/sym_rows_lib.sh
. "$REPO/tests/sym_rows_lib.sh"
$DRY_RUN || mkdir -p "$ROWDIR"

# --- stats: capture, never cp (the aging trap) ------------------------------------
# snap <idx> <label> — `<rowdir>/m<idx>_p<label>.json` (the lib's file shape)
snap() {
    local idx="$1" label="$2" out
    out="$ROWDIR/m${idx}_p${label}.json"
    if $DRY_RUN; then
        RX_CANNED='{"metrics":{}}' rx "$idx" MNT="${MNT[$idx]}" <<'EOS' >/dev/null
cat "$MNT/.stats"
EOS
        return 0
    fi
    rx "$idx" MNT="${MNT[$idx]}" <<'EOS' >"$out" || die "cannot snapshot m$idx's stats inode (${HOST[$idx]}:${MNT[$idx]})"
set -euo pipefail
cat "$MNT/.stats"
EOS
    python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$out" 2>/dev/null ||
        die "m$idx's .stats is not JSON (${HOST[$idx]}:${MNT[$idx]}) — a daemon that cannot answer"
}
# live reads (one snapshot to a scratch file, then the lib's readers)
stat_field() { # idx key
    local f="$ROWDIR/.live-m$1.json"
    $DRY_RUN && { echo 0; return 0; }
    rx "$1" MNT="${MNT[$1]}" <<'EOS' >"$f"
set -euo pipefail
cat "$MNT/.stats"
EOS
    sym_json_field "$f" "$2"
}
stat_sum() { # idx key
    local f="$ROWDIR/.live-m$1.json"
    $DRY_RUN && { echo 0; return 0; }
    rx "$1" MNT="${MNT[$1]}" <<'EOS' >"$f"
set -euo pipefail
cat "$MNT/.stats"
EOS
    sym_json_sum "$f" "$2"
}
stat_all_eq() { # idx key want
    local f="$ROWDIR/.live-m$1.json"
    $DRY_RUN && { echo 1; return 0; }
    rx "$1" MNT="${MNT[$1]}" <<'EOS' >"$f"
set -euo pipefail
cat "$MNT/.stats"
EOS
    sym_json_all_eq "$f" "$2" "$3"
}
stat_first() { # idx key
    local f="$ROWDIR/.live-m$1.json"
    $DRY_RUN && { echo 0; return 0; }
    rx "$1" MNT="${MNT[$1]}" <<'EOS' >"$f"
set -euo pipefail
cat "$MNT/.stats"
EOS
    sym_json_first "$f" "$2"
}

# The must-stay-0 set of one LIVE writer (an oracle face).
sym_zero_set_live() { # label idx
    local f="$ROWDIR/.live-m$2.json"
    $DRY_RUN && return 0
    rx "$2" MNT="${MNT[$2]}" <<'EOS' >"$f"
set -euo pipefail
cat "$MNT/.stats"
EOS
    sym_zero_set_file "$1" "$2" "$f"
}

# --- the label -------------------------------------------------------------------
BENCH_ORDER=0
declare -A NODE_KERNEL=() NODE_BUILD=()
node_facts() { # every distinct host: kernel + the mounted daemon's build_commit
    local idx
    for idx in 0 "${WRITERS[@]}" ${READER:+1}; do
        if $DRY_RUN; then
            NODE_KERNEL[$idx]="(kernel)"
            NODE_BUILD[$idx]="(build_commit)"
            continue
        fi
        NODE_KERNEL[$idx]="$(rx "$idx" <<'EOS'
uname -r
EOS
)"
        NODE_BUILD[$idx]="$(stat_field "$idx" build_commit)"
    done
}
RTT_TEXT="n/a"
measure_rtt() { # the first writer node → the manager's fabric address
    local w="${WRITERS[0]}"
    if [ "${HOST[$w]}" = local ] && [ "${HOST[0]}" = local ]; then
        RTT_TEXT="co-located (one host, no wire RTT)"
        return 0
    fi
    [ -n "$MANAGER_PRIV" ] || { RTT_TEXT="not measured (no --manager-priv)"; return 0; }
    if $DRY_RUN; then
        RX_CANNED="rtt min/avg/max/mdev = 0.100/0.120/0.150/0.010 ms" rx "$w" TARGET="$MANAGER_PRIV" <<'EOS' >/dev/null
ping -c 10 -i 0.2 -q "$TARGET" | tail -1
EOS
        RTT_TEXT="(dry-run)"
        return 0
    fi
    RTT_TEXT="$(rx "$w" TARGET="$MANAGER_PRIV" <<'EOS' || echo "ping failed"
ping -c 10 -i 0.2 -q "$TARGET" 2>/dev/null | tail -1 | sed 's/^rtt //'
EOS
)"
}
row_stamp() { # row cmd — the caller bumps BENCH_ORDER (a stamp inside a
    # pipeline runs in a subshell)
    echo "# row=$1"
    echo "# order=$BENCH_ORDER"
    echo "# instrument=${INSTRUMENT:-the matrix's sym legs' instruments (tar -xf the shipped corpus; tests/mdstorm.c T=$THREADS; dd bs=4M conv=fsync; python3 O_CREAT|O_EXCL creators; ls -l)}"
    echo "# substrate=${SUBSTRATE:-unlabelled (pass --substrate)}"
    echo "# format=${FORMAT_LABEL:-unstated (pass --format: cache-less vs staged — the shape gate 2 prices)}"
    echo "# venue=$SYM_VENUE${CLUSTER:+ cluster=$CLUSTER} — one symmetric writer per node; the cloud row is a THIRD substrate class, never spliced into devsub or squeeze-test medians"
    echo "# rt=$RT s (the sustained window per measured phase)"
    echo "# rtt=$RTT_TEXT (writer m${WRITERS[0]} → the manager)"
    local idx
    for idx in 0 "${WRITERS[@]}" ${READER:+1}; do
        echo "# node m$idx=${ROLE[$idx]} host=${HOST[$idx]} mnt=${MNT[$idx]} kernel=${NODE_KERNEL[$idx]:-?} build=${NODE_BUILD[$idx]:-?}"
    done
    echo "# ts=$(date -u +%FT%TZ)"
    echo "# cmd=$2"
}
ROWS_FILE="$ROWDIR/rows.txt"
emit() { # append a labelled line to the rows file + stdout
    if $DRY_RUN; then echo "$*"; else echo "$*" | tee -a "$ROWS_FILE"; fi
}

# --- the posture preflight (every node's .stats says what it is) --------------------
preflight() {
    log "preflight: the fleet's posture from every node's .stats"
    local idx v
    v="$(stat_field 0 mount_posture)"
    $DRY_RUN || [ "$v" = "writer" ] || die "manager m0 (${HOST[0]}:${MNT[0]}): mount_posture='$v' (want writer)"
    $DRY_RUN || [ "$(stat_all_eq 0 manager_lease held)" = "1" ] ||
        die "manager m0: manager_lease != held on every volume (the D0 winner IS the manager, KD-SYM-3)"
    $DRY_RUN || [ "$(stat_all_eq 0 symmetric_meta 1)" = "1" ] ||
        die "manager m0: symmetric_meta != 1 on every volume — the plane is not armed"
    for idx in "${WRITERS[@]}"; do
        v="$(stat_field "$idx" mount_posture)"
        $DRY_RUN || [ "$v" = "writer" ] || die "writer m$idx (${HOST[$idx]}:${MNT[$idx]}): mount_posture='$v' (want writer)"
        v="$(stat_first "$idx" joined_appender_id)"
        $DRY_RUN || [ "${v:-0}" -ge 1 ] 2>/dev/null || die "writer m$idx: joined_appender_id='$v' (want ≥ 1 — a JOINED writer)"
        v="$(stat_first "$idx" manager_lease)"
        $DRY_RUN || [[ "$v" = peer:* ]] || die "writer m$idx: manager_lease='$v' (want peer:…)"
    done
    if [ -n "$READER" ]; then
        v="$(stat_field 1 reader_staleness_bound_ms)"
        $DRY_RUN || [ "$v" = "0" ] || die "reader m1 (${HOST[1]}:${MNT[1]}): reader_staleness_bound_ms='$v' (want 0 — a TOKEN reader, R-SYM-4)"
    fi
    # The fleet-wide appender count is the DIRECTORY's Live-page count
    # (`appenders_known`); `appenders_live` counts the regions THIS mount
    # joined (1 on every daemon).
    local n=$((1 + ${#WRITERS[@]}))
    v="$(stat_first 0 appenders_known)"
    $DRY_RUN || [ "$v" = "$n" ] || die "manager m0: appenders_known=$v (want $n — the manager + ${#WRITERS[@]} joined writers' Live pages)"
    log "preflight: manager m0 + ${#WRITERS[@]} joined writer(s)${READER:+ + 1 token reader} — appenders_known=$v"
}

# --- tools on the nodes ---------------------------------------------------------------
# Every tool a row invokes must resolve on the node BEFORE any row runs:
# a missing binary dies LOUD here, never mid-row on a billing fleet (the
# review's Issue 1: `getfattr` is not on the cloud image, and a failed
# `getfattr … || echo 0` read K = 0 — a false RED of the -ls law).
tools_preflight() {
    local idx tools
    for idx in 0 "${WRITERS[@]}" ${READER:+1}; do
        # comma-separated: an rx env value rides `sudo env K=V … bash -s` as
        # ONE ssh command string, so it must carry no spaces
        tools="python3,stat,timeout,ls,rm,mkdir"
        if [ "$idx" != "1" ]; then tools="$tools,tar,cc,dd,getfattr"; fi
        if [ "$idx" = "${WRITERS[0]}" ] && [ -n "$MANAGER_PRIV" ]; then tools="$tools,ping"; fi
        RX_CANNED="" rx "$idx" TOOLS="$tools" <<'EOS' || die "m$idx (${HOST[$idx]}): a tool the rows need is missing (see above) — install it on the node before any row runs"
missing=""
for t in ${TOOLS//,/ }; do command -v "$t" >/dev/null 2>&1 || missing="$missing $t"; done
[ -z "$missing" ] || { echo "missing on $(hostname):$missing" >&2; exit 1; }
EOS
    done
    log "tools preflight: every row tool resolves on every node"
}
MDSTORM_BIN="$REMOTE_DIR/mdstorm"
MDSTORM_INSTALLED=0
install_mdstorm() {
    [ "$MDSTORM_INSTALLED" = 1 ] && return 0
    MDSTORM_INSTALLED=1
    [ -r "$MDSTORM_SRC" ] || die "mdstorm source missing: $MDSTORM_SRC"
    local idx
    for idx in 0 "${WRITERS[@]}"; do
        push_file "$idx" "$MDSTORM_SRC" "$REMOTE_DIR/mdstorm.c"
        rx "$idx" SRC="$REMOTE_DIR/mdstorm.c" BIN="$MDSTORM_BIN" <<'EOS' || die "m$idx: cc mdstorm.c failed (gcc missing on the node?)"
set -euo pipefail
cc -O2 -pthread -o "$BIN" "$SRC"
EOS
    done
}
CORPUS_REMOTE="$REMOTE_DIR/corpus.tar"
CORPUS_ENTRIES=0
CORPUS_LOCAL=""
prepare_corpus() {
    if [ -n "$TARBALL" ]; then
        CORPUS_LOCAL="$TARBALL"
    elif [ -n "$TAR_SRC" ]; then
        [ -d "$TAR_SRC" ] || die "--tar-src '$TAR_SRC' is not a directory"
        CORPUS_LOCAL="$ROWDIR/corpus.tar"
        if $DRY_RUN; then
            echo "+ tar -cf $CORPUS_LOCAL -C $(dirname "$TAR_SRC") $(basename "$TAR_SRC")" >&2
        else
            tar -cf "$CORPUS_LOCAL" -C "$(dirname "$TAR_SRC")" "$(basename "$TAR_SRC")"
        fi
    else
        die "sym-tarx needs the corpus: --tar-src=<linux>/fs (design §5.10: the linux fs/ corpus) or --tarball=<file>"
    fi
    if $DRY_RUN; then
        CORPUS_ENTRIES=2468
    else
        [ -s "$CORPUS_LOCAL" ] || die "corpus tarball missing/empty: $CORPUS_LOCAL"
        CORPUS_ENTRIES="$(tar -tf "$CORPUS_LOCAL" | wc -l)"
    fi
    local idx
    for idx in 0 "${WRITERS[0]}"; do
        push_file "$idx" "$CORPUS_LOCAL" "$CORPUS_REMOTE"
    done
    log "corpus: $CORPUS_LOCAL ($CORPUS_ENTRIES entries) shipped to m0 and m${WRITERS[0]}"
}

# --- the oracle (fsck --json at the manager + drift + the must-stay-0 set) ------------
sym_oracle() { # label
    local label="$1" rc=0 findings idx out
    out="$ROWDIR/fsck-$label.json"
    if $DRY_RUN; then
        RX_CANNED='{"findings":[],"findings_elided":0}' rx 0 SQZ="$SQZ_NODE" MNT="${MNT[0]}" <<'EOS' >/dev/null
timeout 900 "$SQZ" fsck "$MNT" --json
EOS
        echo "(dry-run: would judge findings == 0, meta_kv_block_refs_drift == 0 and the must-stay-0 set on every writer)" >&2
        return 0
    fi
    rx 0 SQZ="$SQZ_NODE" MNT="${MNT[0]}" <<'EOS' >"$out" 2>"$out.err" || rc=$?
timeout 900 "$SQZ" fsck "$MNT" --json
EOS
    [ "$rc" != "124" ] || die "$label: online fsck HUNG past 900 s — transcript $out"
    findings="$(sym_fsck_json_findings "$out")"
    [ "$rc" = "0" ] && [ "$findings" = "0" ] ||
        die "$label: online fsck rc=$rc findings=$findings — $out / $out.err"
    for idx in 0 "${WRITERS[@]}"; do
        [ "$(stat_sum "$idx" meta_kv_block_refs_drift)" = "0" ] ||
            die "$label: meta_kv_block_refs_drift != 0 on m$idx (C8 oracle RED)"
        sym_zero_set_live "$label" "$idx"
    done
    log "$label: oracle clean (fsck findings 0, C8 drift 0, the must-stay-0 set flat on every writer)"
}

# --- the mount hook (a writer node's LEAVE and REJOIN) -----------------------------
hook() { # mount|unmount idx
    local verb="$1" idx="$2"
    [ -n "$MOUNT_HOOK" ] || return 1
    if $DRY_RUN; then
        echo "+ $MOUNT_HOOK $verb ${HOST[$idx]} ${MNT[$idx]}" >&2
        return 0
    fi
    # shellcheck disable=SC2086 # the hook is a command WORD LIST by contract
    $MOUNT_HOOK "$verb" "${HOST[$idx]}" "${MNT[$idx]}" || die "mount hook '$MOUNT_HOOK $verb' failed for m$idx (${HOST[$idx]}:${MNT[$idx]})"
}
is_mounted() { # idx -> 0 yes
    $DRY_RUN && return 0
    rx "$1" MNT="${MNT[$1]}" <<'EOS'
mountpoint -q "$MNT"
EOS
}
# Exactly the writers `want...` live (gate 3's "exactly N appenders live"):
# every other joined writer LEAVES cleanly, a wanted one not up JOINS; the
# manager's directory must then count N Live pages. Without a hook every
# writer stays mounted (the row says so) and appenders_known reads the fleet.
ensure_writers() { # n want_idx...
    local n="$1" j w want t
    shift
    if [ -z "$MOUNT_HOOK" ]; then
        return 0
    fi
    for j in "${WRITERS[@]}"; do
        want=0
        for w in "$@"; do [ "$w" = "$j" ] && want=1; done
        if [ "$want" = "1" ]; then
            is_mounted "$j" || hook mount "$j"
        else
            if is_mounted "$j"; then hook unmount "$j"; fi
        fi
    done
    $DRY_RUN && return 0
    for t in $(seq 1 90); do
        : "$t"
        [ "$(stat_all_eq 0 appenders_known "$n")" = "1" ] && return 0
        sleep 1
    done
    die "the manager's appender directory never read $n Live page(s) (appenders_known=$(stat_field 0 appenders_known)) — a writer's leave or join did not land"
}

# --- diskstats on the storage nodes (the ingest row's amplification columns) -------------
declare -A STG_HOST=() STG_DEV=()
STG_IDXS=()
parse_storage() {
    [ -n "$STORAGE" ] || return 0
    local i=200 spec
    IFS=, read -r -a specs <<<"$STORAGE"
    for spec in "${specs[@]}"; do
        STG_HOST[$i]="${spec%%:*}"
        STG_DEV[$i]="${spec#*:}"
        HOST[$i]="${STG_HOST[$i]}"
        MNT[$i]="-"
        ROLE[$i]=storage
        STG_IDXS+=("$i")
        i=$((i + 1))
    done
}
# diskstats_sample <tag> — per storage device: sectors written, write ops
diskstats_sample() {
    local tag="$1" i
    $DRY_RUN && { for i in "${STG_IDXS[@]}"; do RX_CANNED="0 0" rx "$i" DEV="${STG_DEV[$i]}" <<'EOS' >/dev/null
b="$(basename "$(readlink -f "$DEV")")"
awk -v d="$b" '$3==d {print $10, $8}' /proc/diskstats
EOS
    done; return 0; }
    for i in "${STG_IDXS[@]}"; do
        RX_CANNED="0 0" rx "$i" DEV="${STG_DEV[$i]}" <<'EOS' >"$ROWDIR/.diskstats-$tag-$i" 2>/dev/null || echo "0 0" >"$ROWDIR/.diskstats-$tag-$i"
b="$(basename "$(readlink -f "$DEV")")"
awk -v d="$b" '$3==d {print $10, $8}' /proc/diskstats
EOS
    done
}
# amplification <tag0> <tag1> <user_bytes> -> "dev/user=X wareq_sz=Y B"
amplification() {
    local t0="$1" t1="$2" user="$3" i sec0 ops0 sec1 ops1 dsec=0 dops=0
    [ "${#STG_IDXS[@]}" -gt 0 ] || { echo "amp=n/a(no --storage)"; return 0; }
    for i in "${STG_IDXS[@]}"; do
        read -r sec0 ops0 <"$ROWDIR/.diskstats-$t0-$i"
        read -r sec1 ops1 <"$ROWDIR/.diskstats-$t1-$i"
        dsec=$((dsec + sec1 - sec0))
        dops=$((dops + ops1 - ops0))
    done
    python3 -c "
dev=$dsec*512; ops=$dops; user=$user
print(f'dev_bytes={dev} user_bytes={user} dev/user={dev/max(1,user):.3f} wareq_sz={dev/max(1,ops):.0f}B write_ops={ops}')"
}

# --- acked writes present (the lib's laws; the census runs ON the nodes) --------------
# (the lib's python text travels as the SCRIPT BODY — an rx env value rides
# `sudo env K=V … bash -s` as one ssh command string and cannot carry spaces)
# tree_census <idx> <path-under-mount> -> "entries bytes" as m<idx> sees it
tree_census() {
    RX_CANNED="0 0" rx "$1" P="${MNT[$1]}$2" <<<"python3 - \"\$P\" <<'PYEOF'
$SYM_TREE_CENSUS_PY
PYEOF"
}
# zero_file <idx> <path-under-mount> -> "bytes zero_ok"
zero_file() {
    RX_CANNED="0 1" rx "$1" P="${MNT[$1]}$2" <<<"python3 - \"\$P\" <<'PYEOF'
$SYM_ZERO_FILE_PY
PYEOF"
}
# The mount a WRITER's acked writes are read back through: another writer
# when one is mounted (the manager for a joiner, the first mounted joiner
# for the manager), else the token reader, else none (N = 1 with no reader).
other_mount_for() { # idx [mounted-writers...] -> idx | ""
    local idx="$1" j
    shift
    for j in "$@"; do [ "$j" != "$idx" ] && { echo "$j"; return 0; }; done
    [ -n "$READER" ] && { echo 1; return 0; }
    echo ""
}
# stripe_k_of <idx> <rel path> -> K as m<idx> reports it (the lib's reader
# shipped to the node; 0 = unstriped; dies loud on a missing tool)
stripe_k_of() {
    local k
    # the lib's reader IS the remote script (an env value cannot carry it
    # over `sudo env … bash -s`); the path arrives as $P
    k="$(RX_CANNED=64 rx "$1" P="${MNT[$1]}$2" <<<"set -- \"\$P\"
$SYM_STRIPE_K_SH")" || true
    [ -n "$k" ] && [ "$k" != "SYM_K_TOOL_FAIL" ] && [[ "$k" =~ ^[0-9]+$ ]] ||
        die "m$1 (${HOST[$1]}): cannot read user.squeezefs.stripes on ${MNT[$1]}$2 (getfattr missing on the node, or not a live directory) — the -ls law needs K, never a silent 0"
    echo "$k"
}
# acked_tree_check <label> <writer idx> <rel path> <via idx>
acked_tree_check() {
    local label="$1" w="$2" rel="$3" via="$4" we ge wb gb
    read -r we wb <<<"$(tree_census "$w" "$rel")"
    read -r ge gb <<<"$(tree_census "$via" "$rel")"
    $DRY_RUN && { echo "(dry-run: would judge acked tree $rel: m$w $we/$wb ≡ m$via $ge/$gb)" >&2; return 0; }
    sym_law_acked_tree "$label" "$we" "$ge" "$wb" "$gb" "m$via (${HOST[$via]}:${MNT[$via]})"
    emit "   acked-writes present ($label): $we entries / $wb bytes acked at m$w read back identical through m$via"
}

# ===================================================================================
# gate 2 — sym-tarx
# ===================================================================================
# One venue arm: extract the corpus into a directory the node creates, as
# many reps as fit --rt (≥ 1; each into a fresh subdir so every extraction
# is "into a directory it created"), timed ON THE NODE; snapshots at both
# ends (the extracting node + the manager) around a settle. Prints
# `label wall_per_rep reps ops_s`.
sym_venue_extract() { # idx label
    local idx="$1" label="$2" reps="$TARX_REPS" out
    local rt_arg=0
    # reps 0 = fill --rt (each extraction into a fresh subdir); ≥ 1 = that many
    [ "$reps" = "0" ] && { rt_arg="$RT"; reps=1; }
    sleep 2
    [ "$idx" != "0" ] && snap "$idx" "${label}0"
    snap 0 "${label}0"
    out="$(RX_CANNED="2.00 1 $CORPUS_ENTRIES" rx "$idx" MNT="${MNT[$idx]}" TAR="$CORPUS_REMOTE" LABEL="$label" REPS="$reps" RT="$rt_arg" ENTRIES="$CORPUS_ENTRIES" <<'EOS'
set -euo pipefail
dest="$MNT/s8a-$LABEL"
mkdir -p "$dest"
t0="$(date +%s.%N)"
n=0
while :; do
  d="$dest/r$n"
  mkdir "$d"
  tar -xf "$TAR" -C "$d"
  n=$((n + 1))
  now="$(date +%s.%N)"
  el="$(python3 -c "print($now-$t0)")"
  if [ "$RT" = "0" ]; then [ "$n" -ge "$REPS" ] && break; else python3 -c "import sys; sys.exit(0 if $el >= $RT else 1)" && break; fi
done
t1="$(date +%s.%N)"
python3 -c "
w=$t1-$t0; n=$n
print(f'{w/n:.3f} {n} {n*$ENTRIES/w:.0f}')"
EOS
)" || die "sym-tarx $label: tar -x FAILED on m$idx (a shipped verb errored — see the daemon logs)"
    sleep 2
    [ "$idx" != "0" ] && snap "$idx" "${label}1"
    snap 0 "${label}1"
    # ACKED WRITES PRESENT (the lib's law): the extracted tree as the
    # extracting node sees it ≡ as ANOTHER mount reads it (the manager for
    # the joiner's arm, the first joiner for the manager's) — after the
    # snapshots (the read-back's tokens are the other mount's, never the
    # arm's judged deltas), before the venue's blocks go back.
    local via
    via="$(other_mount_for "$idx" 0 "${WRITERS[0]}")"
    [ -n "$via" ] && acked_tree_check "sym-tarx $label" "$idx" "/s8a-$label" "$via" >&2
    # The venue's blocks back before the next arm (untimed) — AFTER the
    # snapshots: on a joined writer every terminal free SHIPS to the
    # allocation holder as a publish-plane frame, which would land on the
    # arm's `meta_ship_publish.shipped` delta (the matrix's order).
    rx "$idx" MNT="${MNT[$idx]}" LABEL="$label" <<'EOS' || true
rm -rf "$MNT/s8a-$LABEL"
EOS
    echo "$label $out"
}

row_tarx() {
    local jw="${WRITERS[0]}" label
    BENCH_ORDER=$((BENCH_ORDER + 1))
    prepare_corpus
    log "gate 2 (sym-tarx): corpus $CORPUS_ENTRIES entries; venue = joined writer m$jw (${HOST[$jw]}) extracting into a directory IT created over the REAL fabric (rtt $RTT_TEXT), vs S0 = the manager's own extract on m0 (${HOST[0]}) with the ${#WRITERS[@]} joiner(s)${READER:+ + the reader} mounted and idle; A-B-B-A; $([ "$TARX_REPS" = 0 ] && echo "≥ $RT s per arm" || echo "$TARX_REPS extraction(s) per arm — the box's shape")"
    local -a rows=()
    sym_arm() { # label -> row line
        local label="$1" out wire xv ship pub verbs_per h_j h_m rpcs
        out="$(sym_venue_extract "$jw" "$label")"
        if $DRY_RUN; then echo "$out (dry-run: would judge verbs/entry < 0.05, handovers 0, rpcs 0)"; return 0; fi
        wire="$(sym_delta "$ROWDIR" "$jw" "$label" joined_wire_verbs)"
        xv="$(sym_delta "$ROWDIR" "$jw" "$label" xv_cross_owner_steps_shipped)"
        ship="$(sym_delta "$ROWDIR" "$jw" "$label" meta_ship.shipped_verbs)"
        pub="$(sym_delta "$ROWDIR" "$jw" "$label" meta_ship_publish.shipped)"
        h_j="$(sym_delta "$ROWDIR" "$jw" "$label" slot_handovers)"
        h_m="$(sym_delta "$ROWDIR" 0 "$label" slot_handovers)"
        rpcs="$(stat_field "$jw" dlm_rpcs)"
        # the per-rep verb counts (the law is per entry over every rep's entries)
        local reps entries_total
        reps="$(echo "$out" | awk '{print $3}')"
        entries_total=$((CORPUS_ENTRIES * reps))
        verbs_per="$(sym_law_gate2_engagement "$label" "$entries_total" "$wire" "$xv" "$ship" "$pub" "$h_j" "$h_m" "$rpcs")"
        echo "$out wire=$wire xv=$xv ship=$ship pub=$pub verbs/entry=$verbs_per handovers=0"
    }
    local_arm() { # label -> row line (the S0 shape: the manager's own
        # extract with the joiners and the reader mounted and idle — the
        # matrix's `local_arm`, never a solo mount)
        local label="$1" out
        out="$(sym_venue_extract 0 "$label")"
        echo "$out manager-local-S0(joiners-idle-mounted)"
    }
    rows+=("$(sym_arm sym-1)")
    rows+=("$(local_arm local-1)")
    rows+=("$(local_arm local-2)")
    rows+=("$(sym_arm sym-2)")
    {
        row_stamp "sym-tarx" "tar -xf $CORPUS_REMOTE (entries=$CORPUS_ENTRIES) ×reps; A-B-B-A sym-1 local-1 local-2 sym-2"
        echo "== gate 2: tar -x on a JOINED WRITER node (m$jw) over the real fabric vs S0 = the manager's own extract (m0; joiners idle and mounted — the matrix's shape) — entries=$CORPUS_ENTRIES per rep; $([ "$TARX_REPS" = 0 ] && echo "≥ $RT s per arm" || echo "$TARX_REPS extraction(s) per arm") =="
        printf '%-10s %-10s %-5s %-8s %s\n' ARM WALL/REP_S REPS OPS_S ENGAGEMENT
        local r a b c d rest
        for r in "${rows[@]}"; do
            read -r a b c d rest <<<"$r"
            printf '%-10s %-10s %-5s %-8s %s\n' "$a" "$b" "$c" "$d" "$rest"
        done
    } | tee -a "$([ "$DRY_RUN" = true ] && echo /dev/null || echo "$ROWS_FILE")"
    # (dry-run: the walls below are the canned "2.00" — the verdict runs on
    # them to show its shape; the oracle prints its commands)
    local s1 s2 l1 l2
    s1="$(echo "${rows[0]}" | awk '{print $2}')"
    l1="$(echo "${rows[1]}" | awk '{print $2}')"
    l2="$(echo "${rows[2]}" | awk '{print $2}')"
    s2="$(echo "${rows[3]}" | awk '{print $2}')"
    sym_law_gate2_verdict "$s1" "$s2" "$l1" "$l2" | tee "$([ "$DRY_RUN" = true ] && echo /dev/null || echo "$ROWDIR/symtarx-verdict.txt")" | tee -a "$([ "$DRY_RUN" = true ] && echo /dev/null || echo "$ROWS_FILE")"
    $DRY_RUN && echo "(dry-run: the verdict above ran on CANNED walls)"
    sym_oracle sym-tarx
    log "sym-tarx PUBLISHED (rows + verdict + snapshots in $ROWDIR)"
}

# --- the sustained-state rule (AGENTS.md): a RATE phase shorter than --rt ---------
# On the CLOUD venue a burst row is "a FAILED row, not a result" — the
# phase is judged INVALID (the row's verdict word; the driver exits
# nonzero after its row sets, evidence kept), never warned past. On the
# laptop it is scoping and a WARN. Prints the verdict suffix ("" when the
# phase filled RT). Every caller runs this in a command substitution — a
# SUBSHELL — so the flag is set by the CALLER off the non-empty word
# (`rt_flag`), never in here (review round 2, Issue 15: the first build
# set it here and the end-of-run refusal was unreachable).
RT_INVALID=0
rt_flag() { # verdict-word... -> RT_INVALID=1 when any word is non-empty
    local w
    for w in "$@"; do [ -z "$w" ] || RT_INVALID=1; done
}
sym_rt_verdict() { # label wall_s phase -> "" | "INVALID(sub-RT …)"
    local label="$1" wall="$2" phase="$3"
    $FORCE_SUB_RT && wall=0   # the pin: judge every phase as a burst
    [ "$(python3 -c "print(1 if $wall >= $RT else 0)")" = "1" ] && return 0
    if [ "$SYM_VENUE" = cloud ]; then
        echo "INVALID(sub-RT:$phase ${wall}s<${RT}s)"
        echo "[sym-rows] $label: the $phase phase ran ${wall} s < RT=$RT s — a burst is a FAILED row on the cloud venue (the sustained-state rule); the row is INVALID (size --files/--ingest-mb up or let --size-to-rt=auto size them)" >&2
    else
        warn "$label: the $phase phase ran ${wall} s < RT=$RT s — size --files/--ingest-mb up for the counted row (the sustained-state rule; scoping on the laptop)"
    fi
}

# The N = 1 PILOT (--size-to-rt=auto): a short create storm + a 256 MiB
# ingest on the manager, then a small shared-dir wave by every creator into
# a throwaway directory, each timed on the node — the measured rates size
# the counted phases to fill --rt with 25 % headroom. The pilot's files are
# removed before any snapshot a row judges; its directory flips once (a
# warm-up the rows' per-row deltas never see).
PILOT_DONE=0
SHARED_PER_WRITER=""
size_to_rt() {
    [ "$SIZE_TO_RT" = auto ] || return 0
    [ "$PILOT_DONE" = 1 ] && return 0
    PILOT_DONE=1
    install_mdstorm
    local pf out c_rate i_rate
    pf=$((FILES < 5000 ? FILES : 5000))
    log "pilot (--size-to-rt=auto): N = 1 on the manager — $pf creates ($THREADS threads) + 256 MiB ingest, then a shared-dir wave by every creator; sizing the rows to fill RT=$RT s"
    out="$(RX_CANNED="5000.0 1000.0" rx 0 MNT="${MNT[0]}" STORM="$MDSTORM_BIN" T="$THREADS" F="$pf" <<'EOS'
set -euo pipefail
d="$MNT/pilot-$$"
mkdir -p "$d"
t0="$(date +%s.%N)"; "$STORM" "$d" "$T" "$F" create >/dev/null 2>&1; t1="$(date +%s.%N)"
dd if=/dev/zero of="$d/ingest.bin" bs=4M count=64 conv=fsync status=none
t2="$(date +%s.%N)"
rm -rf "$d"
python3 -c "print(f'{$F/($t1-$t0):.1f} {256/($t2-$t1):.1f}')"
EOS
)" || die "pilot: the N = 1 create/ingest pilot FAILED on the manager"
    read -r c_rate i_rate <<<"$out"
    # creates: rate × RT × 1.25, rounded up to 1,000; ingest: MiB/s × RT × 1.25,
    # rounded up to 4 MiB, capped (the local zram cannot hold a cloud-sized row)
    local want_files want_mb
    want_files="$(python3 -c "import math; print(max($FILES, int(math.ceil($c_rate*$RT*1.25/1000))*1000))")"
    want_mb="$(python3 -c "import math; print(max($INGEST_MB, int(math.ceil($i_rate*$RT*1.25/4))*4))")"
    if [ "$want_mb" -gt "$INGEST_CAP_MB" ]; then
        warn "pilot: the ingest sized to RT wants $want_mb MiB per writer, capped at --ingest-cap-mb=$INGEST_CAP_MB (the ingest phase may read sub-RT — INVALID on the cloud venue)"
        want_mb="$INGEST_CAP_MB"
    fi
    FILES="$want_files"
    INGEST_MB="$want_mb"
    # the shared row: every creator into ONE directory — a different
    # mechanism (ships to the holder), so its own pilot wave: 500 per
    # creator into a throwaway directory of the first joiner
    local holder="${WRITERS[0]}" prel idx s_rate
    prel="/pilot-shared-$(date +%s)"
    rx "$holder" P="${MNT[$holder]}$prel" <<'EOS' || die "pilot: the shared-dir pilot's mkdir failed on m$holder"
mkdir "$P"
EOS
    local -a pids=()
    local t0 t1 p rc=0
    t0="$(date +%s.%N)"
    for idx in 0 "${WRITERS[@]}"; do
        rx_bg "$idx" "$ROWDIR/.pilot-shared-w$idx" D="${MNT[$idx]}$prel" PFX="p$idx" N=500 <<'EOS'
python3 - "$D" "$PFX" "$N" <<'PYEOF'
import os, sys
d, pfx, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
for i in range(n):
    fd = os.open(f"{d}/{pfx}-{i:07d}", os.O_CREAT | os.O_WRONLY | os.O_EXCL, 0o644); os.close(fd)
PYEOF
EOS
        pids+=($!)
    done
    for p in "${pids[@]}"; do wait "$p" || rc=1; done
    t1="$(date +%s.%N)"
    [ "$rc" = "0" ] || die "pilot: a shared-dir pilot creator FAILED (see $ROWDIR/.pilot-shared-w*.err)"
    rx "$holder" P="${MNT[$holder]}$prel" <<'EOS' || true
rm -rf "$P"
EOS
    if $DRY_RUN; then
        SHARED_PER_WRITER=$((FILES / (1 + ${#WRITERS[@]})))
    else
        s_rate="$(python3 -c "print(f'{500/($t1-$t0):.1f}')")" # per creator, into one directory
        SHARED_PER_WRITER="$(python3 -c "import math; print(max($FILES // (1 + ${#WRITERS[@]}), int(math.ceil($s_rate*$RT*1.25/100))*100))")"
    fi
    log "pilot: create $c_rate/s, ingest $i_rate MiB/s, shared $([ "$DRY_RUN" = true ] && echo '(dry-run)' || echo "$s_rate")/creator/s → --files=$FILES --ingest-mb=$INGEST_MB shared per-creator=$SHARED_PER_WRITER (RT $RT s, 25 % headroom)"
    emit "# pilot(size-to-rt): create=${c_rate}/s ingest=${i_rate}MiB/s shared_per_creator=${s_rate:-dry}/s -> files=$FILES ingest_mb=$INGEST_MB shared_per_writer=$SHARED_PER_WRITER"
}

# ===================================================================================
# gate 3 — sym-scale
# ===================================================================================
row_scale() {
    local -a ns
    IFS=',' read -r -a ns <<<"$SCALE_NS"
    local maxn=$((1 + ${#WRITERS[@]})) n
    local -a ns_ok=()
    for n in "${ns[@]}"; do
        if [ "$n" -le "$maxn" ]; then ns_ok+=("$n"); else warn "sym-scale: N=$n exceeds the writers present ($maxn) — skipped"; fi
    done
    [ "${#ns_ok[@]}" -ge 1 ] || die "sym-scale: no N in '$SCALE_NS' fits the ${#WRITERS[@]} writer(s) given"
    install_mdstorm
    size_to_rt
    BENCH_ORDER=$((BENCH_ORDER + 1))
    log "gate 3 (sym-scale): N ∈ {${ns_ok[*]}} writer NODES each creating $FILES files ($THREADS threads) in its OWN directory, then ingesting $INGEST_MB MiB (4 MiB blocks, conv=fsync); exactly N appenders live per row${MOUNT_HOOK:+ (the idle writers LEAVE — mount hook)}"
    [ -n "$MOUNT_HOOK" ] || warn "sym-scale: no --mount-hook — the idle writers stay MOUNTED (appenders_known reads the whole fleet; the deleted-stays-deleted-across-the-leave arm is skipped)"
    local SYM_RUN rate1="" ingest1="" verdict_all=MET zero_miss_all="" removed="$ROWDIR/removed-sample.txt"
    $DRY_RUN && removed="$(mktemp -t sym-rows-dryrun-removed.XXXXXX)"
    SYM_RUN="$(date +%s)"
    local table="$ROWDIR/symscale-table.tsv"
    $DRY_RUN && table=/dev/null
    : >"$table"
    {
        row_stamp "sym-scale" "mdstorm T=$THREADS F=$FILES create per writer node; dd bs=4M count=$((INGEST_MB / 4)) conv=fsync per writer node; N ∈ {${ns_ok[*]}}"
        sym_gate3_header
    } | tee -a "$table" | tee -a "$([ "$DRY_RUN" = true ] && echo /dev/null || echo "$ROWS_FILE")"
    for n in "${ns_ok[@]}"; do
        local -a writers=(0)
        local i idx
        for ((i = 0; i < n - 1; i++)); do writers+=("${WRITERS[$i]}"); done
        ensure_writers "$n" "${writers[@]:1}"
        sleep 2
        for idx in "${writers[@]}"; do snap "$idx" "n${n}0"; done
        local live
        live="$(stat_first 0 appenders_known)"
        if [ -n "$MOUNT_HOOK" ]; then
            $DRY_RUN || [ "$live" = "$n" ] || die "sym-scale N=$n: appenders_known=$live at the manager (want exactly $n Live pages)"
        fi
        # The create row: every writer node's storm at once, one directory each.
        local -a pids=()
        local t0 t1 t_row0
        t0="$(date +%s.%N)"
        t_row0="$t0"
        for idx in "${writers[@]}"; do
            rx_bg "$idx" "$ROWDIR/create-n$n-w$idx.txt" MNT="${MNT[$idx]}" DIR="scale-$SYM_RUN-n$n-w$idx" STORM="$MDSTORM_BIN" T="$THREADS" F="$FILES" <<'EOS'
set -euo pipefail
mkdir -p "$MNT/$DIR"
"$STORM" "$MNT/$DIR" "$T" "$F" create
EOS
            pids+=($!)
        done
        local p rc=0
        for p in "${pids[@]}"; do wait "$p" || rc=1; done
        t1="$(date +%s.%N)"
        [ "$rc" = "0" ] || die "sym-scale N=$n: a create storm FAILED (see $ROWDIR/create-n$n-w*.txt{,.err})"
        local create_rate create_wall
        create_wall="$(python3 -c "print(f'{$t1-$t0:.1f}')")"
        $DRY_RUN && create_wall="$RT"   # canned: the dry-run "fills RT"
        create_rate="$(python3 -c "print(f'{$n*$FILES/($t1-$t0):.0f}')")"
        # the create phase's own daemon-CPU face (a snapshot between the phases)
        for idx in "${writers[@]}"; do snap "$idx" "n${n}c"; done
        local create_cpu_ns=0 v_cpu creates_per_cpu_s
        if ! $DRY_RUN; then
            for idx in "${writers[@]}"; do
                v_cpu="$(python3 -c "
import json
a=json.load(open('$ROWDIR/m${idx}_pn${n}0.json'))['metrics']['daemon_cpu_ns']
b=json.load(open('$ROWDIR/m${idx}_pn${n}c.json'))['metrics']['daemon_cpu_ns']
print(int(b)-int(a))" 2>/dev/null || echo 0)"
                create_cpu_ns=$((create_cpu_ns + v_cpu))
            done
        fi
        creates_per_cpu_s="$(python3 -c "print(f'{$n*$FILES*1e9/max(1,$create_cpu_ns):.0f}')")"
        # The ingest row: 4 MiB blocks, conv=fsync, one file per writer node;
        # diskstats on the DATA namespaces around it (the amplification columns).
        diskstats_sample "n${n}i0"
        pids=()
        t0="$(date +%s.%N)"
        for idx in "${writers[@]}"; do
            rx_bg "$idx" "$ROWDIR/ingest-n$n-w$idx.txt" MNT="${MNT[$idx]}" DIR="scale-$SYM_RUN-n$n-w$idx" COUNT="$((INGEST_MB / 4))" <<'EOS'
set -euo pipefail
dd if=/dev/zero of="$MNT/$DIR/ingest.bin" bs=4M count="$COUNT" conv=fsync status=none
EOS
            pids+=($!)
        done
        for p in "${pids[@]}"; do wait "$p" || rc=1; done
        t1="$(date +%s.%N)"
        [ "$rc" = "0" ] || die "sym-scale N=$n: an ingest dd FAILED (see $ROWDIR/ingest-n$n-w*.txt.err)"
        diskstats_sample "n${n}i1"
        local ingest_rate ingest_wall amp
        ingest_wall="$(python3 -c "print(f'{$t1-$t0:.1f}')")"
        $DRY_RUN && ingest_wall="$RT"
        ingest_rate="$(python3 -c "print(f'{$n*$INGEST_MB/($t1-$t0):.0f}')")"
        amp="n/a"
    $DRY_RUN || amp="$(amplification "n${n}i0" "n${n}i1" $((n * INGEST_MB * 1024 * 1024)))"
        sleep 2
        for idx in "${writers[@]}"; do snap "$idx" "n${n}1"; done
        if $DRY_RUN; then
            echo "(dry-run: N=$n — would judge handovers 0, ships ≤ $n, rpcs 0, the must-stay-0 deltas, ≥ 0.7 × N × the N=1 rate and the RT rule from the snapshots above; the acked read-backs and the removed-sample ls follow)"
        fi
        # THE ENGAGEMENT LAW (§8 gate 3) — the lib's.
        local handovers=0 ships=0 rpcs=0 v zero_miss=""
        $DRY_RUN || for idx in "${writers[@]}"; do
            v="$(sym_delta "$ROWDIR" "$idx" "n$n" slot_handovers)"
            handovers=$((handovers + v))
            v="$(sym_delta "$ROWDIR" "$idx" "n$n" slot_ships)"
            ships=$((ships + v))
            v="$(stat_field "$idx" dlm_rpcs)"
            rpcs=$((rpcs + v))
            v="$(sym_zero_violations_delta "$ROWDIR" "$idx" "n$n")"
            [ -z "$v" ] || zero_miss="$zero_miss m$idx:{$v}"
        done
        $DRY_RUN || sym_law_gate3_engagement "$n" "$handovers" "$ships" "$rpcs"
        local mgr_load="0" mgr_cpu="0"
        if ! $DRY_RUN; then
            mgr_load="$(sym_json_first "$ROWDIR/m0_pn${n}1.json" manager_load_pct)"
            mgr_cpu="$(python3 -c "
import json
a=json.load(open('$ROWDIR/m0_pn${n}0.json'))['metrics']['daemon_cpu_ns']
b=json.load(open('$ROWDIR/m0_pn${n}1.json'))['metrics']['daemon_cpu_ns']
print(f'{100*(int(b)-int(a))/1e9/max(1e-9, $t1-$t_row0):.0f}')" 2>/dev/null || echo 0)"
        fi
        [ -n "$rate1" ] || rate1="$create_rate"
        [ -n "$ingest1" ] || ingest1="$ingest_rate"
        local cr ir verdict
        cr="$(python3 -c "print(f'{$create_rate/$rate1:.2f}')")"
        ir="$(python3 -c "print(f'{$ingest_rate/$ingest1:.2f}')")"
        verdict="$(sym_law_gate3_row "$n" "$create_rate" "$rate1" "$ingest_rate" "$ingest1")"
        if [ -n "$zero_miss" ]; then
            verdict="MISS(must-stay-0:$zero_miss)"
            zero_miss_all="$zero_miss_all N=$n:$zero_miss"
        fi
        # The sustained-state rule: both RATE phases must fill RT (the
        # cloud venue's INVALID word lands in the verdict column).
        local rt_c rt_i
        rt_c="$(sym_rt_verdict "sym-scale N=$n" "$create_wall" create)"
        rt_i="$(sym_rt_verdict "sym-scale N=$n" "$ingest_wall" ingest)"
        rt_flag "$rt_c" "$rt_i"
        [ -z "$rt_c$rt_i" ] || verdict="$verdict $rt_c $rt_i"
        [ "$verdict" = "MET" ] || verdict_all=MISS
        sym_gate3_row_line "$n" "$create_rate" "$cr" "$creates_per_cpu_s" "$ingest_rate" "$ir" "$mgr_load" "$mgr_cpu" "$handovers" "$ships" "$rpcs" "$verdict" | tee -a "$table" | tee -a "$([ "$DRY_RUN" = true ] && echo /dev/null || echo "$ROWS_FILE")"
        # The write-amplification instrument's third column (AGENTS.md): the
        # block_free_* reclaim ledger over the ingest window, Σ over the
        # row's writers — a freed-block path that WRITES instead of
        # deallocating (Write Zeroes on a target without DSM) shows here
        # beside the device ÷ user ratio (the local pass read 1.87× on the
        # N = 1 row that followed sym-tarx's rm -rf of ≈ 6,000 blocks).
        local bf="" k v_bf
        $DRY_RUN || for k in block_free_discards block_free_discard_bytes block_free_file_punches block_free_punch_bytes block_free_reclaim_skipped block_free_reclaim_commands; do
            v_bf=0
            for idx in "${writers[@]}"; do
                v="$(sym_delta "$ROWDIR" "$idx" "n$n" "$k" 2>/dev/null || echo 0)"
                v_bf=$((v_bf + v))
            done
            bf="$bf ${k#block_free_}=$v_bf"
        done
        emit "   N=$n walls: create ${create_wall}s ingest ${ingest_wall}s (RT $RT s); appenders_known=$live; ingest amplification: $amp; block_free (row window, Σ writers):$bf"
        # ACKED WRITES PRESENT (the lib's laws): every writer's create tree
        # and its fsynced ingest file read back through ANOTHER mount (a
        # writer of the row when N ≥ 2, else the token reader; at N = 1 with
        # no reader there is no other mount — stated).
        for idx in "${writers[@]}"; do
            local via
            via="$(other_mount_for "$idx" "${writers[@]}")"
            if [ -z "$via" ]; then
                emit "   acked-writes present (N=$n m$idx): no other mount to read back through (N = 1, no reader) — the manager's own view is the census (deleted-stays-deleted below reads it)"
                continue
            fi
            acked_tree_check "sym-scale N=$n m$idx" "$idx" "/scale-$SYM_RUN-n$n-w$idx" "$via"
            local zb zok
            read -r zb zok <<<"$(zero_file "$via" "/scale-$SYM_RUN-n$n-w$idx/ingest.bin")"
            $DRY_RUN || sym_law_acked_ingest "sym-scale N=$n m$idx" "$((INGEST_MB * 1024 * 1024))" "$zb" "$zok" "m$via (${HOST[$via]}:${MNT[$via]})"
            $DRY_RUN || emit "   acked-writes present (N=$n m$idx): ingest.bin $zb bytes read back through m$via, all zero"
        done
        for idx in "${writers[@]}"; do
            # The LAST names the storm created (`ls -U` = readdir order = the
            # order `rm -rf` unlinks in) — the deleted-stays-deleted sample.
            RX_CANNED="/scale-$SYM_RUN-n$n-w$idx/f000199" rx "$idx" MNT="${MNT[$idx]}" DIR="scale-$SYM_RUN-n$n-w$idx" <<'EOS' >>"$removed" || true
ls -U "$MNT/$DIR" 2>/dev/null | tail -200 | sed "s|^|/$DIR/|"
rm -rf "$MNT/$DIR" 2>/dev/null || true
EOS
        done
    done
    if $DRY_RUN; then
        echo "(dry-run: the gate-3 verdict line would print here; the deleted-stays-deleted arm — every writer's clean leave, the removed sample through the manager, one writer's rejoin, the sample through it — follows with its commands)"
    else
        sym_law_gate3_verdict_line "$verdict_all" | tee "$ROWDIR/symscale-verdict.txt" | tee -a "$ROWS_FILE"
        [ -z "$zero_miss_all" ] || die "sym-scale: a must-stay-0 gauge moved:$zero_miss_all (rows above; the row set is RED)"
    fi
    # DELETED STAYS DELETED across every writer's CLEAN LEAVE: every joined
    # writer unmounts (the leave's flush-then-transfer of every slot), the
    # removed sample is judged through the MANAGER, then through a
    # REMOUNTED writer (a fresh open of the durable state). ONE classifier
    # for every arm (the lib's `sym_stat_deleted_classify`): only ENOENT is
    # "deleted"; an EIO/EAGAIN is a daemon that cannot answer.
    stat_removed_via() { # idx rel -> the classifier's word
        local out rc=0
        if $DRY_RUN; then
            rx "$1" P="${MNT[$1]}$2" <<'EOS' >/dev/null
timeout 30 stat "$P" >/dev/null
EOS
            echo deleted
            return 0
        fi
        # the remote `timeout`'s exit (124 = hung) rides the ssh exit code
        # verbatim; stderr (the ENOENT text) comes home as $out
        out="$(rx "$1" P="${MNT[$1]}$2" <<'EOS' 2>&1 >/dev/null
timeout 30 stat "$P" >/dev/null
EOS
)" || rc=$?
        sym_stat_deleted_classify "$rc" "$out"
    }
    judge_removed() { # idx what -> resurrected count (dies on hung/error)
        local idx="$1" what="$2" resurrected=0 verdict rel
        while IFS= read -r rel; do
            [ -n "$rel" ] || continue
            verdict="$(stat_removed_via "$idx" "$rel")"
            case "$verdict" in
            deleted) ;;
            resurrected)
                resurrected=$((resurrected + 1))
                echo "RESURRECTED at $what: $rel" >>"$ROWDIR/resurrected.txt"
                ;;
            hung) die "sym-scale: stat of removed $rel through $what HUNG past 30 s (a parked lookup)" ;;
            error:*) die "sym-scale: stat of removed $rel through $what failed with something other than ENOENT: ${verdict#error:}" ;;
            esac
        done <"$removed"
        echo "$resurrected"
    }
    if [ -s "$removed" ]; then
        local total resurrected resurrected_j=0 j
        total="$(wc -l <"$removed" | tr -d ' ')"
        if [ -n "$MOUNT_HOOK" ]; then
            for j in "${WRITERS[@]}"; do
                if is_mounted "$j"; then hook unmount "$j"; fi
            done
            resurrected="$(judge_removed 0 "the manager after every writer's clean leave")"
            emit "deleted-stays-deleted (manager, after every writer's clean leave): $resurrected of $total sampled removed names resolve"
            hook mount "${WRITERS[0]}"
            resurrected_j="$(judge_removed "${WRITERS[0]}" "remounted writer m${WRITERS[0]}")"
            emit "deleted-stays-deleted (remounted writer m${WRITERS[0]}): $resurrected_j of $total"
        else
            resurrected="$(judge_removed 0 "the manager (writers still mounted — no hook)")"
            emit "deleted-stays-deleted (manager; the writers stayed mounted — no --mount-hook): $resurrected of $total sampled removed names resolve"
        fi
        [ "$resurrected" = "0" ] && [ "$resurrected_j" = "0" ] ||
            die "sym-scale: DELETED DID NOT STAY DELETED — $resurrected (manager) / $resurrected_j (remounted writer) of $total sampled removed names resolve (see $ROWDIR/resurrected.txt)"
    fi
    $DRY_RUN && rm -f "$removed"
    # Every writer back up for the rows that follow.
    ensure_writers "$((1 + ${#WRITERS[@]}))" "${WRITERS[@]}"
    sym_oracle sym-scale
    log "sym-scale PUBLISHED (table + verdict + snapshots in $ROWDIR)"
}

# ===================================================================================
# gate 3b — sym-shared-dir (+ -ls)
# ===================================================================================
row_shared() {
    [ "${#WRITERS[@]}" -ge 2 ] ||
        die "sym-shared-dir needs ≥ 2 joined writers (the flip triggers on foreign creates from MORE THAN ONE creator)"
    local holder="${WRITERS[0]}" shared_rel per_writer idx
    BENCH_ORDER=$((BENCH_ORDER + 1))
    shared_rel="/shared-$(date +%s)"
    local -a writers=(0 "${WRITERS[@]}")
    size_to_rt
    per_writer="${SHARED_PER_WRITER:-$((FILES / ${#writers[@]}))}"
    rx "$holder" P="${MNT[$holder]}$shared_rel" <<'EOS' || die "sym-shared-dir: the holder's mkdir failed"
set -euo pipefail
mkdir "$P"
EOS
    local k_stripes
    k_stripes="$(stat_first 0 slot_rotor)"
    log "gate 3b (sym-shared-dir): ${#writers[@]} creator NODES × $per_writer files into ONE directory held by m$holder (${HOST[$holder]}); the flip to K stripes (K derives from MINT_SPREAD = $k_stripes) on the holder's observed creator count"
    for idx in "${writers[@]}"; do snap "$idx" "sd0"; done
    local -a pids=()
    local t0 t1
    t0="$(date +%s.%N)"
    for idx in "${writers[@]}"; do
        rx_bg "$idx" "$ROWDIR/shared-w$idx.count" D="${MNT[$idx]}$shared_rel" PFX="w$idx" N="$per_writer" <<'EOS'
python3 - "$D" "$PFX" "$N" <<'PYEOF'
import os, sys
d, pfx, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
ok = 0
for i in range(n):
    try:
        fd = os.open(f"{d}/{pfx}-{i:07d}", os.O_CREAT | os.O_WRONLY | os.O_EXCL, 0o644)
        os.close(fd)
        ok += 1
    except OSError as e:
        sys.stderr.write(f"create {pfx}-{i}: {e}\n")
        break
print(ok)
PYEOF
EOS
        pids+=($!)
    done
    local p rc=0
    for p in "${pids[@]}"; do wait "$p" || rc=1; done
    t1="$(date +%s.%N)"
    [ "$rc" = "0" ] || die "sym-shared-dir: a creator FAILED (see $ROWDIR/shared-w*.count.err)"
    sleep 3
    for idx in "${writers[@]}"; do snap "$idx" "sd1"; done
    $DRY_RUN && echo "(dry-run: would judge flips == 1 at the holder, striped, stripe ships > 0, shipped ≡ served, handovers 0 from the snapshots above; the census, the acked read-back, the -ls half and the oracle follow with their commands)"
    local created=0 c wall
    for idx in "${writers[@]}"; do
        if $DRY_RUN; then c="$per_writer"; else c="$(tr -d '[:space:]' <"$ROWDIR/shared-w$idx.count")"; fi
        [ "$c" = "$per_writer" ] || die "sym-shared-dir: m$idx created $c of $per_writer (see $ROWDIR/shared-w$idx.count.err)"
        created=$((created + c))
    done
    wall="$(python3 -c "print(f'{$t1-$t0:.2f}')")"
    $DRY_RUN && wall="$RT"
    local listed
    listed="$(RX_CANNED="$created" rx "$holder" P="${MNT[$holder]}$shared_rel" <<'EOS'
ls -f "$P" | grep -c '^w' || true
EOS
)"
    [ "$listed" = "$created" ] ||
        die "sym-shared-dir: the directory lists $listed names but $created creates were acked (the striped readdir merge or a lost dentry)"
    # ACKED WRITES PRESENT (the lib's law): the holder's census of the
    # directory ≡ the manager's (a creator reading a foreign holder's
    # directory through its tokens) — the -ls half below is the reader's.
    acked_tree_check "sym-shared-dir" "$holder" "$shared_rel" 0
    local flips=0 flip_at="" striped=0 shipped=0 served=0 stripe_ships=0 handovers=0 v
    $DRY_RUN && { flips=1; flip_at="m$holder(1) "; striped=1; stripe_ships=1; }
    $DRY_RUN || for idx in "${writers[@]}"; do
        v="$(sym_delta "$ROWDIR" "$idx" sd dir_stripe_flips)"
        [ "$v" = "0" ] || flip_at="${flip_at}m$idx($v) "
        flips=$((flips + v))
        v="$(sym_delta "$ROWDIR" "$idx" sd xv_cross_owner_steps_shipped)"
        shipped=$((shipped + v))
        v="$(sym_delta "$ROWDIR" "$idx" sd xv_cross_owner_steps_served)"
        served=$((served + v))
        v="$(sym_delta "$ROWDIR" "$idx" sd dir_stripe_ships)"
        stripe_ships=$((stripe_ships + v))
        v="$(sym_delta "$ROWDIR" "$idx" sd slot_handovers)"
        handovers=$((handovers + v))
        sym_zero_set_file sym-shared-dir "$idx" "$ROWDIR/m${idx}_psd1.json"
    done
    $DRY_RUN || striped="$(sym_json_sum "$ROWDIR/m${holder}_psd1.json" dir_striped_dirs)"
    # K_D = the directory's stripe count as the HOLDER reports it (≥ 1 after
    # the flip); K_root = the mount ROOT's as the MANAGER reports it (0 while
    # `/` is unstriped — a token reader's first `stat /` folds the root's
    # stripes, one records-only grant each: design §8 row 3b's K_root term,
    # PR 13d). Both through the lib's ONE die-loud reader shipped to the
    # node: a missing tool or an unreadable answer is a harness failure,
    # never a silent 0.
    local xattr_k k_root
    xattr_k="$(stripe_k_of "$holder" "$shared_rel")"
    [ "$xattr_k" -ge 1 ] || die "sym-shared-dir: the holder's user.squeezefs.stripes read K_D=$xattr_k (want ≥ 1 after the flip)"
    k_root="$(stripe_k_of 0 "")"
    local rt_s
    rt_s="$(sym_rt_verdict sym-shared-dir "$wall" create)"
    rt_flag "$rt_s"
    {
        row_stamp "sym-shared-dir" "python3 O_CREAT|O_EXCL creators: ${#writers[@]} nodes × $per_writer into ONE directory held by m$holder"
        echo "== gate 3b: ${#writers[@]} creator nodes × $per_writer into ONE directory (holder m$holder): wall $wall s, $(python3 -c "print(f'{$created/($t1-$t0):.0f}')") creates/s aggregate (RT $RT s)${rt_s:+ $rt_s} =="
        echo "   flips=$flips at [$flip_at] striped_dirs(holder)=$striped K_D=$xattr_k K_root=$k_root xv_shipped=$shipped xv_served=$served dir_stripe_ships=$stripe_ships handovers=$handovers"
    } | tee -a "$([ "$DRY_RUN" = true ] && echo /dev/null || echo "$ROWS_FILE")"
    $DRY_RUN || sym_law_gate3b_engagement "$holder" "$flips" "$flip_at" "$striped" "$stripe_ships" "$shipped" "$served" "$handovers"
    log "sym-shared-dir: flip at the holder, $stripe_ships stripe ships, closure shipped ≡ served ($shipped), 0 handovers"

    # --- sym-shared-dir-ls: a COLD token reader's `readdir + stat` ------------
    if [ -z "$READER" ]; then
        warn "sym-shared-dir-ls SKIPPED: no reader= entry (the cold readdir + stat row is K stripe tokens + C inode tokens, 0 leaf reads — mount a -o ro TOKEN reader and pass reader=<host>:<mnt>)"
    else
        rx 1 <<'EOS' || true
sync
echo 3 >/proc/sys/vm/drop_caches 2>/dev/null || true
EOS
        snap 1 "ls0"
        local statted
        t0="$(date +%s.%N)"
        statted="$(RX_CANNED="$created" rx 1 P="${MNT[1]}$shared_rel" <<'EOS'
ls -l "$P" | grep -c '^-' || true
EOS
)"
        t1="$(date +%s.%N)"
        snap 1 "ls1"
        [ "$statted" = "$created" ] || die "sym-shared-dir-ls: the reader statted $statted of $created children"
        if $DRY_RUN; then
            echo "(dry-run: would judge the -ls law — dlm_token_grants ∈ [K_D + K_root + C, K_D + K_root + C + 4], 0 data-leaf reads net of the poll, merges ≥ 1 — and the plane-replacement witnesses from the two reader snapshots above)"
        else
        # The instrument's precondition: the reader's per-holder planes
        # stood for the whole listing. A plane REPLACED mid-window (its
        # holder's endpoint died — a writer rejoined at another port) takes
        # its cumulative history with it and the delta reads short — an
        # INVALID instrument, never a law's verdict (found by the local
        # pass behind sym-scale's leave/rejoin on `auto`-port joiners).
        # Witnesses: the two COUNTERS going backwards, and the per-volume
        # plane COUNT changing (`dlm_token_cached` is a level — an eviction
        # inside the window is legitimate and never aborts a paid run).
        local planes_msg
        planes_msg="planes $(sym_json_field "$ROWDIR/m1_pls0.json" dlm_token_reader_holder_planes) → $(sym_json_field "$ROWDIR/m1_pls1.json" dlm_token_reader_holder_planes)"
        for k in dlm_token_grants dlm_token_recalls_received; do
            [ "$(sym_json_any_decrease "$ROWDIR/m1_pls0.json" "$ROWDIR/m1_pls1.json" "$k")" = "0" ] ||
                die "sym-shared-dir-ls: INSTRUMENT INVALID — the reader's $k went BACKWARDS on a volume across the listing (a per-holder token plane was replaced mid-row: a holder's endpoint changed — $planes_msg); the row cannot be judged from these deltas — re-run the -ls half on a reader whose planes are fresh (run 'shared' ahead of 'scale', or remount the reader)"
        done
        [ "$(sym_json_any_change "$ROWDIR/m1_pls0.json" "$ROWDIR/m1_pls1.json" dlm_token_reader_holder_planes)" = "0" ] ||
            warn "sym-shared-dir-ls: the reader's per-holder plane count CHANGED across the listing ($planes_msg) — a first-touch plane dial inside the window (expected on a cold reader: the counters above stayed monotone, so the deltas stand)"
        local grants merges misses hits dropped epochs
        grants="$(sym_delta "$ROWDIR" 1 ls dlm_token_grants)"
        merges="$(sym_delta "$ROWDIR" 1 ls dir_stripe_readdir_merges)"
        misses="$(sym_delta "$ROWDIR" 1 ls meta_kv_node_cache_misses)"
        hits="$(sym_delta "$ROWDIR" 1 ls dlm_token_hits)"
        dropped="$(sym_delta "$ROWDIR" 1 ls meta_kv_revalidate_nodes_dropped)"
        epochs="$(sym_delta "$ROWDIR" 1 ls meta_kv_revalidate_epochs)"
        emit "== sym-shared-dir-ls: cold readdir + stat of $created children over K_D=$xattr_k stripes (root K_root=$k_root) on token reader m1 (${HOST[1]}): $(python3 -c "print(f'{$t1-$t0:.2f}')") s; dlm_token_grants=$grants (law: K_D + K_root + C = $((xattr_k + k_root + created)), + the directory itself, its parent and the reader's root) readdir_merges=$merges node_cache_misses=$misses token_hits=$hits =="
        sym_law_gate3b_ls "$grants" "$xattr_k" "$k_root" "$created" "$misses" "$dropped" "$epochs" "$merges"
        sym_zero_reader_file sym-shared-dir-ls 1 "$ROWDIR/m1_pls1.json"
        log "sym-shared-dir-ls: $grants tokens for K_D=$xattr_k + K_root=$k_root + C=$created, 0 data-leaf reads for the listing ($misses misses = the poll's $dropped dropped images over $epochs epoch steps + ≤ K tree-0 lessee reads)"
        fi
    fi
    rx "$holder" P="${MNT[$holder]}$shared_rel" <<'EOS' || true
rm -rf "$P" 2>/dev/null || true
EOS
    sym_oracle sym-shared-dir
    log "sym-shared-dir PUBLISHED (rows + snapshots in $ROWDIR)"
}

# ===================================================================================
# main
# ===================================================================================
log "venue=$SYM_VENUE rows=$ROWS rt=${RT}s rowdir=$ROWDIR"
# Shape preconditions BEFORE any row runs (a billing cluster must not learn
# them two row sets in): gate 3b's flip triggers on foreign creates from
# MORE THAN ONE creator — the holder is a joined writer, so it needs a
# second joined writer beside the manager (≥ 3 writer nodes).
if [[ ",$ROWS," == *,shared,* ]] && [ "${#WRITERS[@]}" -lt 2 ]; then
    die "sym-shared-dir needs ≥ 2 joined writers (${#WRITERS[@]} given): the flip triggers on foreign creates from MORE THAN ONE creator and the holder is a joined writer — run ≥ 3 writer nodes (N_CLIENT ≥ 3), or drop 'shared' from --rows"
fi
parse_storage
node_facts
measure_rtt
preflight
tools_preflight
$DRY_RUN || : >"$ROWS_FILE"
{
    echo "cluster=${CLUSTER:-} venue=$SYM_VENUE substrate=${SUBSTRATE:-} format=${FORMAT_LABEL:-}"
    echo "rows=$ROWS rt=$RT files=$FILES threads=$THREADS ingest_mb=$INGEST_MB scale_ns=$SCALE_NS"
    echo "manager m0=${HOST[0]}:${MNT[0]}"
    for idx in "${WRITERS[@]}"; do echo "writer m$idx=${HOST[$idx]}:${MNT[$idx]}"; done
    [ -n "$READER" ] && echo "reader m1=${HOST[1]}:${MNT[1]}"
    echo "storage=${STORAGE:-none} rtt=$RTT_TEXT mount_hook=${MOUNT_HOOK:-none}"
    echo "repo_commit=$(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)"
    echo "ts=$(date -u +%FT%TZ)"
} >"$([ "$DRY_RUN" = true ] && echo /dev/null || echo "$ROWDIR/manifest.txt")"
for r in ${ROWS//,/ }; do
    case "$r" in
    tarx) row_tarx ;;
    scale) row_scale ;;
    shared) row_shared ;;
    *) die "unknown row set '$r' (tarx|scale|shared)" ;;
    esac
done
if [ "$RT_INVALID" = 1 ]; then
    # Reached from the PARENT shell (the callers flag it); the dry-run under
    # --test-force-sub-rt is the pin that it fires (review round 2, Issue 15).
    die "a RATE phase ran shorter than RT=$RT s on the cloud venue — the row(s) marked INVALID(sub-RT) in $ROWS_FILE are burst rows, not results (every other law's evidence is kept in $ROWDIR)$($FORCE_SUB_RT && echo ' — --test-force-sub-rt: this nonzero exit IS the Issue-15 pin')"
elif $DRY_RUN; then
    log "dry-run complete: every command printed, nothing executed"
else
    log "ALL ROW SETS PUBLISHED — $ROWS_FILE (labels, tables, verdicts); snapshots + fsck transcripts in $ROWDIR"
fi
