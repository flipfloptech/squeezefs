#!/bin/bash
# SqueezeFS vs JuiceFS — the standing "beat-JuiceFS" scoreboard.
#
# A re-runnable, matched-conditions A/B harness that turns "faster than
# JuiceFS everywhere" into a release gate. Both stacks get the same substrate
# (durable stores in ONE directory on the same device class), matched cache
# budgets, identical elbencho drivers, and quiet-gated timed rows. Output is
# a win/loss table (rows = regime x workload); ANY loss row fails the gate.
#
# Protocol (normative, 2026-07-15 session):
#   Matched substrate  — SqueezeFS meta+data volumes AND the JuiceFS meta db +
#                        file:// object store + local cache all live under
#                        SQUEEZEFS_VS_SUBSTRATE_DIR (refused if tmpfs).
#   Matched budgets    — one knob (SQUEEZEFS_VS_CACHE_MB) drives both:
#                        SqueezeFS  --mem-budget ${MB}M (single RAM authority:
#                                   tiers + node cache + transport arenas) and
#                                   mount --disk-cache-size ${MB}MB (staging).
#                        JuiceFS    --buffer-size ${MB} (their RAM knob) and
#                                   --cache-size ${MB} (their disk-cache knob).
#                        The mapping is honest but not exact: JuiceFS splits
#                        RAM/disk into two knobs and does NOT budget its meta
#                        engine or cache index RAM; SqueezeFS's one budget
#                        carries everything. Where it errs, it errs in
#                        JuiceFS's favor.
#   Three regimes      — R1 as-deployed (all cache layers live, capped
#                        budgets, dataset 2-4x cache);
#                        R2 device-true (JuiceFS --cache-size 0 + cache-dir on
#                        substrate + a tight memcg cage to defeat the file://
#                        page-cache serve [decomposition-report precedent] —
#                        verified via diskstats + object-GET counters;
#                        SqueezeFS -o direct_device_true — verified via
#                        .stats read_device_true_reads/get_obj + diskstats);
#                        R3 cold-cache (full drops + remounts, first pass).
#   Workload grid      — 6 shapes per regime, elbencho throughout (identical
#                        drivers): seq write 1M / seq read 1M / rand read 4k
#                        (the user's exact line: -t 16 -b 4k --iodepth 16
#                        --direct) / rand write 4k / stat storm / del storm.
#
# Usage:
#   tests/run_vs_juicefs.sh                    # full scoreboard (~30-60 min)
#   SQUEEZEFS_VS_SMOKE=1 tests/run_vs_juicefs.sh   # micro-grid plumbing proof
#   SQUEEZEFS_VS_REGIMES="R2" tests/run_vs_juicefs.sh
#   SQUEEZEFS_VS_ALLOW_LOSS="R1.rand_write_4k" tests/run_vs_juicefs.sh
#
# Exit code: 0 = no loss rows (gate green); 1 = at least one LOSS/INVALID row
# not covered by SQUEEZEFS_VS_ALLOW_LOSS; 2 = harness/setup failure.
# SQUEEZEFS_VS_SMOKE=1 never gates on W/L (plumbing proof only).
#
# Env knobs (all optional):
#   SQUEEZEFS_VS_SUBSTRATE_DIR   matched substrate root (default
#                                /var/tmp/squeezefs_vs_juicefs; tmpfs refused)
#   SQUEEZEFS_VS_CACHE_MB        matched cache budget MiB (default 4096)
#   SQUEEZEFS_VS_DATASET_GB      seq/rand dataset GiB, 16 files (default 16 =
#                                4x the default cache budget)
#   SQUEEZEFS_VS_TREE_DIRS/_TREE_FILES  stat/del tree geometry per thread
#                                (default 8 x 1024 = 131072 files @ 16 threads)
#   SQUEEZEFS_VS_CAGE_MB         daemon memcg cage MiB, both systems
#                                (default 16384)
#   SQUEEZEFS_VS_R2_JFS_CAGE_MB  R2 JuiceFS page-cache-defeat cage (default
#                                2048 — the decomposition-report mechanism;
#                                JuiceFS has no device-true switch, the cage
#                                is the only honest way to force device serve
#                                on a file:// object store)
#   SQUEEZEFS_VS_TIMELIMIT       rand-row seconds (default 30)
#   SQUEEZEFS_VS_REGIMES         subset of "R1 R2 R3"
#   SQUEEZEFS_VS_WORKLOADS       subset of "seq_write_1m seq_read_1m
#                                rand_read_4k rand_write_4k stat_storm
#                                del_storm"
#   SQUEEZEFS_VS_SYSTEMS         subset of "jfs sqz" (debug)
#   SQUEEZEFS_VS_ALLOW_LOSS      comma list of row ids (R1.seq_write_1m,...)
#                                exempt from the gate — known-loss tracking
#   SQUEEZEFS_VS_QUIET_LOAD      load1 quiet threshold (default 2.0)
#   SQUEEZEFS_VS_QUIET_POLLS     consecutive quiet polls needed (default 3)
#   SQUEEZEFS_VS_QUIET_SECS     seconds between polls (default 5)
#   SQUEEZEFS_VS_CPUSET          taskset range for daemons+drivers (default
#                                0-15 when >=20 CPUs online, else unpinned)
#   SQUEEZEFS_VS_JUICEFS_VERSION sandbox auto-install version (default 1.3.0)
#   SQUEEZEFS_VS_KEEP=1          keep substrate stores after the run
#   SQUEEZEFS_VS_OUT_DIR         artifacts dir (default
#                                $SUBSTRATE/artifacts/<UTC timestamp>)
#
# Safety rails: kills by PID only; refuses to operate under /mnt/squeezefs or
# ~/tmp/nvme; daemons run in systemd-run scopes (memcg cages); quiet-gate
# before every timed row; rows re-checked for co-tenants afterward and
# DIRTY-flagged, never silently blended.

set -uo pipefail

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUNUSER="${SUDO_USER:-}"

SMOKE="${SQUEEZEFS_VS_SMOKE:-0}"
SUBSTRATE="${SQUEEZEFS_VS_SUBSTRATE_DIR:-/var/tmp/squeezefs_vs_juicefs}"
CACHE_MB="${SQUEEZEFS_VS_CACHE_MB:-4096}"
CAGE_MB="${SQUEEZEFS_VS_CAGE_MB:-16384}"
R2_JFS_CAGE_MB="${SQUEEZEFS_VS_R2_JFS_CAGE_MB:-2048}"
THREADS=16
REGIMES="${SQUEEZEFS_VS_REGIMES:-R1 R2 R3}"
WORKLOADS="${SQUEEZEFS_VS_WORKLOADS:-seq_write_1m seq_read_1m rand_read_4k rand_write_4k stat_storm del_storm}"
SYSTEMS="${SQUEEZEFS_VS_SYSTEMS:-jfs sqz}"
ALLOW_LOSS="${SQUEEZEFS_VS_ALLOW_LOSS:-}"
QUIET_LOAD="${SQUEEZEFS_VS_QUIET_LOAD:-2.0}"
JUICEFS_VERSION="${SQUEEZEFS_VS_JUICEFS_VERSION:-1.3.0}"
ELBENCHO_VERSION="${SQUEEZEFS_VS_ELBENCHO_VERSION:-3.1-9}"
JFS_WRITEBACK="${SQUEEZEFS_VS_JFS_WRITEBACK:-0}" # sensitivity knob: JuiceFS
# ships writeback OFF (durability default) — the scoreboard measures shipped
# defaults; set =1 to A/B their staged-writeback posture.
KEEP="${SQUEEZEFS_VS_KEEP:-0}"

if [ "$SMOKE" = "1" ]; then
    DATASET_GB="${SQUEEZEFS_VS_DATASET_GB:-1}"
    TREE_DIRS="${SQUEEZEFS_VS_TREE_DIRS:-2}"
    TREE_FILES="${SQUEEZEFS_VS_TREE_FILES:-64}"
    TIMELIMIT="${SQUEEZEFS_VS_TIMELIMIT:-5}"
    QUIET_POLLS="${SQUEEZEFS_VS_QUIET_POLLS:-1}"
    QUIET_SECS="${SQUEEZEFS_VS_QUIET_SECS:-1}"
    ROW_TIMEOUT="${SQUEEZEFS_VS_ROW_TIMEOUT:-180}"
else
    DATASET_GB="${SQUEEZEFS_VS_DATASET_GB:-16}"
    TREE_DIRS="${SQUEEZEFS_VS_TREE_DIRS:-8}"
    TREE_FILES="${SQUEEZEFS_VS_TREE_FILES:-1024}"
    TIMELIMIT="${SQUEEZEFS_VS_TIMELIMIT:-30}"
    QUIET_POLLS="${SQUEEZEFS_VS_QUIET_POLLS:-3}"
    QUIET_SECS="${SQUEEZEFS_VS_QUIET_SECS:-5}"
    # A wedged mount must FAIL the row (rc=124 -> INVALID -> gate), not hang.
    ROW_TIMEOUT="${SQUEEZEFS_VS_ROW_TIMEOUT:-900}"
fi

FILE_MB=$((DATASET_GB * 1024 / 16))          # per-file size, 16 files
DATA_VOL_GB=$(((DATASET_GB * 2 + 7) / 4 + 2)) # per data volume (4 volumes)

ONLINE_CPUS="$(nproc)"
if [ -n "${SQUEEZEFS_VS_CPUSET:-}" ]; then
    CPUSET="$SQUEEZEFS_VS_CPUSET"
elif [ "$ONLINE_CPUS" -ge 20 ]; then
    CPUSET="0-15" # phase-1 rails: daemon + driver contend on one pinned set
else
    CPUSET=""
fi

TS="$(date -u +%Y%m%dT%H%M%SZ)"
ART="${SQUEEZEFS_VS_OUT_DIR:-$SUBSTRATE/artifacts/$TS}"
ROWS_TSV="$ART/rawrows.tsv"

# Globals initialized for set -u.
ROW_DIRTY_PRE=""
ROW_DIRTY_COLD=""
COLD_DEGRADED=0
CAGES_OK=1
JFS_META_ENGINE="n/a"
SUB_FSTYPE=""
SUB_SRC=""
DISK=""
JUICEFS_BIN=""
ELBENCHO_BIN=""

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
log() { echo "[vs-juicefs] $*"; }
die() {
    echo "[vs-juicefs] FATAL: $*" >&2
    exit 2
}

# Forbidden-path guard: this harness must NEVER touch the user's live mounts
# or NVMe volume images.
guard_paths() {
    local resolved
    resolved="$(readlink -f "$SUBSTRATE" 2>/dev/null || echo "$SUBSTRATE")"
    case "$resolved" in
    "" | "/") die "refusing substrate dir '$SUBSTRATE'" ;;
    /mnt/squeezefs*) die "substrate under /mnt/squeezefs is forbidden" ;;
    "$HOME/tmp/nvme"* | /home/*/tmp/nvme*) die "substrate under ~/tmp/nvme is forbidden" ;;
    esac
}

# Substrate honesty: both durable stores must sit on the same non-tmpfs
# device class. tmpfs would turn "durable store" into a RAM benchmark.
check_substrate() {
    mkdir -p "$SUBSTRATE" || die "cannot create $SUBSTRATE"
    local fstype
    fstype="$(df --output=fstype "$SUBSTRATE" 2>/dev/null | tail -1 | tr -d ' ')"
    if [ "$fstype" = "tmpfs" ] || [ "$fstype" = "ramfs" ]; then
        die "substrate $SUBSTRATE is $fstype — matched-substrate protocol requires a durable device (set SQUEEZEFS_VS_SUBSTRATE_DIR)"
    fi
    SUB_FSTYPE="$fstype"
    SUB_SRC="$(df --output=source "$SUBSTRATE" 2>/dev/null | tail -1 | tr -d ' ')"
    # Parent disk for /proc/diskstats evidence (partition -> disk).
    local part
    part="$(basename "$SUB_SRC")"
    DISK="$(lsblk -no pkname "$SUB_SRC" 2>/dev/null | head -1)"
    [ -z "$DISK" ] && DISK="$part"
    if ! grep -q " $DISK " /proc/diskstats; then
        log "WARN: no /proc/diskstats row for '$DISK' — device evidence disabled"
        DISK=""
    fi
}

find_or_install_tool() { # <name> <install_fn> -> echoes path or empty
    local name="$1" installer="$2" cand
    for cand in "$SUBSTRATE/bin/$name" "$(command -v "$name" 2>/dev/null)" "$HOME/.local/bin/$name"; do
        if [ -n "$cand" ] && [ -x "$cand" ]; then
            echo "$cand"
            return 0
        fi
    done
    mkdir -p "$SUBSTRATE/bin"
    "$installer" && [ -x "$SUBSTRATE/bin/$name" ] && echo "$SUBSTRATE/bin/$name"
}

# shellcheck disable=SC2329 # invoked indirectly via find_or_install_tool
install_juicefs() { # sandbox-only install, never system paths
    log "juicefs not found — installing v$JUICEFS_VERSION into $SUBSTRATE/bin (sandbox only)" >&2
    local url="https://github.com/juicedata/juicefs/releases/download/v${JUICEFS_VERSION}/juicefs-${JUICEFS_VERSION}-linux-amd64.tar.gz"
    curl -fsSL "$url" -o "$SUBSTRATE/bin/juicefs.tgz" >&2 &&
        tar -xzf "$SUBSTRATE/bin/juicefs.tgz" -C "$SUBSTRATE/bin" juicefs >&2 &&
        rm -f "$SUBSTRATE/bin/juicefs.tgz" && chmod +x "$SUBSTRATE/bin/juicefs"
}

# shellcheck disable=SC2329 # invoked indirectly via find_or_install_tool
install_elbencho() {
    log "elbencho not found — installing static v$ELBENCHO_VERSION into $SUBSTRATE/bin (sandbox only)" >&2
    local url="https://github.com/breuner/elbencho/releases/download/v${ELBENCHO_VERSION}/elbencho-static-x86_64.tar.gz"
    curl -fsSL "$url" -o "$SUBSTRATE/bin/elbencho.tgz" >&2 &&
        tar -xzf "$SUBSTRATE/bin/elbencho.tgz" -C "$SUBSTRATE/bin" >&2 &&
        rm -f "$SUBSTRATE/bin/elbencho.tgz" && chmod +x "$SUBSTRATE/bin/elbencho"
}

# systemd-run cage wrapper: root-tolerant (system scope as root, --user
# otherwise); degrades to uncaged with a loud warning when unavailable.
probe_cages() {
    CAGES_OK=0
    if command -v systemd-run >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
        local user_arg=()
        [ "$(id -u)" -ne 0 ] && user_arg=(--user)
        if systemd-run --quiet --collect --scope "${user_arg[@]}" \
            -p MemoryMax=64M true 2>/dev/null; then
            CAGES_OK=1
        fi
    fi
    [ "$CAGES_OK" = "1" ] || log "WARN: systemd-run cages unavailable — daemons run UNCAGED (memcg budgets unenforced)"
}

cage_cmd() { # <memmax_mb> <unit_suffix> -> fills CAGE_ARGV array
    local memmax_mb="$1" suffix="$2"
    CAGE_ARGV=()
    if [ "$CAGES_OK" = "1" ]; then
        CAGE_ARGV=(systemd-run --quiet --collect --scope
            --unit "sqzvs-${suffix}-$$-$(date +%s%N)"
            -p "MemoryMax=${memmax_mb}M" -p MemorySwapMax=0)
        [ "$(id -u)" -ne 0 ] && CAGE_ARGV+=(--user)
    fi
}

pin_cmd() { # -> fills PIN_ARGV
    PIN_ARGV=()
    [ -n "$CPUSET" ] && PIN_ARGV=(taskset -c "$CPUSET")
}

tctl_read() {
    sensors 2>/dev/null | awk '/Tctl/{gsub(/[+°C]/,"",$2); print $2; exit}'
}

# Co-tenant honesty (phase-1 row.sh lineage). Called only while OUR elbencho
# is not running (pre-gate / post-row), so any elbencho match is foreign.
cotenants() {
    local out=""
    pgrep -x rustc >/dev/null 2>&1 && out="${out}rustc,"
    pgrep -x cargo >/dev/null 2>&1 && out="${out}cargo,"
    pgrep -f pytest >/dev/null 2>&1 && out="${out}pytest,"
    pgrep -x elbencho >/dev/null 2>&1 && out="${out}foreign-elbencho,"
    echo "$out"
}

# House 3-poll quiet gate: QUIET_POLLS consecutive polls, QUIET_SECS apart,
# each requiring no co-tenants (comm-exact builds/pytest/foreign elbencho)
# and Tctl < 80. load1 is RECORDED in the honesty line but does not gate
# per-row — back-to-back rows inherit our own decaying loadavg (smoke-run
# finding); the load threshold is a SESSION-START precondition instead.
# Never blocks forever: after ~5 min the row proceeds flagged DIRTY(gate).
quiet_gate() { # -> sets ROW_DIRTY_PRE
    ROW_DIRTY_PRE=""
    local tries=0 streak=0 tctl cot
    while [ "$streak" -lt "$QUIET_POLLS" ]; do
        cot="$(cotenants)"
        tctl="$(tctl_read)"
        if [ -z "$cot" ] &&
            python3 -c "exit(0 if float('${tctl:-0}') < 80 else 1)"; then
            streak=$((streak + 1))
        else
            streak=0
            tries=$((tries + 1))
            if [ "$tries" -ge 60 ]; then
                ROW_DIRTY_PRE="DIRTY(gate:${cot}tctl=${tctl:-na}),"
                log "quiet-gate never settled — proceeding $ROW_DIRTY_PRE"
                return 0
            fi
            sleep "$QUIET_SECS"
            continue
        fi
        [ "$streak" -lt "$QUIET_POLLS" ] && sleep "$QUIET_SECS"
    done
}

# Session-start precondition: the box must be genuinely idle before the first
# timed row (SQUEEZEFS_VS_QUIET_LOAD on load1). Proceeds DIRTY after ~5 min.
session_quiet_gate() {
    local i load
    for i in $(seq 1 60); do
        load="$(cut -d' ' -f1 /proc/loadavg)"
        if [ -z "$(cotenants)" ] &&
            python3 -c "exit(0 if float('$load') < float('$QUIET_LOAD') else 1)"; then
            return 0
        fi
        [ "$i" = "1" ] && log "waiting for idle box (load1=$load, threshold $QUIET_LOAD)..."
        sleep "$QUIET_SECS"
    done
    log "WARN: box never went idle (load1=$load) — proceeding; rows carry honesty lines"
}

drop_caches() { # root-tolerant; returns 0 if the page cache actually dropped
    sync
    if [ "$(id -u)" -eq 0 ]; then
        echo 3 >/proc/sys/vm/drop_caches && return 0
    elif sudo -n true 2>/dev/null; then
        sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches' && return 0
    fi
    return 1
}

# ---------------------------------------------------------------------------
# SqueezeFS stack (4 meta + 4 data file-backed volumes on the substrate)
# ---------------------------------------------------------------------------
SQZ_BIN="$REPO_DIR/target/release/squeezefs"
SQZ_DIR="$SUBSTRATE/sqz"
SQZ_MNT="$SUBSTRATE/sqz_mnt"
SQZ_STAGING="$SQZ_DIR/staging"
SQZ_PID=""

sqz_meta_uri() {
    echo "sqmeta://$SQZ_DIR/meta1.img,$SQZ_DIR/meta2.img,$SQZ_DIR/meta3.img,$SQZ_DIR/meta4.img"
}

sqz_format() {
    mkdir -p "$SQZ_DIR" "$SQZ_MNT"
    rm -f "$SQZ_DIR"/meta{1,2,3,4}.img "$SQZ_DIR"/data{1,2,3,4}.img
    rm -rf "$SQZ_STAGING"
    mkdir -p "$SQZ_STAGING"
    local i
    for i in 1 2 3 4; do
        truncate -s 1G "$SQZ_DIR/meta$i.img" || die "truncate meta$i"
        truncate -s "${DATA_VOL_GB}G" "$SQZ_DIR/data$i.img" || die "truncate data$i"
    done
    "$SQZ_BIN" format \
        "$(sqz_meta_uri)" \
        "sqdata://$SQZ_DIR/data1.img,$SQZ_DIR/data2.img,$SQZ_DIR/data3.img,$SQZ_DIR/data4.img" \
        --disk-cache-paths "$SQZ_STAGING" \
        --force >"$ART/logs/sqz_format_$1.log" 2>&1 ||
        die "squeezefs format failed (see $ART/logs/sqz_format_$1.log)"
}

sqz_mount() { # <tag> <cage_mb> [extra mount args...]
    local tag="$1" memmax="$2"
    shift 2
    local logf="$ART/logs/sqz_mount_${tag}.log"
    # Defense in depth: never hand the daemon a stale/ENOTCONN mountpoint.
    if ! stat "$SQZ_MNT" >/dev/null 2>&1; then
        fusermount3 -uz "$SQZ_MNT" 2>/dev/null || umount -l "$SQZ_MNT" 2>/dev/null || true
        sleep 0.5
    fi
    cage_cmd "$memmax" "sqz-$tag"
    pin_cmd
    # shellcheck disable=SC2094 # --log-file is a path arg, not a read
    "${CAGE_ARGV[@]}" "${PIN_ARGV[@]}" "$SQZ_BIN" mount \
        "$(sqz_meta_uri)" "$SQZ_MNT" --daemon \
        --mem-budget "${CACHE_MB}M" \
        --disk-cache-size "${CACHE_MB}MB" \
        --log-file "$logf" "$@" >>"$logf" 2>&1
    local i
    for i in $(seq 1 200); do
        mountpoint -q "$SQZ_MNT" &&
            grep -q "transport armed for this session" "$logf" 2>/dev/null && break
        sleep 0.3
    done
    mountpoint -q "$SQZ_MNT" || {
        tail -5 "$logf" >&2
        die "squeezefs mount failed (see $logf)"
    }
    SQZ_PID="$(pgrep -f "squeezefs mount sqmeta://$SQZ_DIR" | head -1)"
    grep -m1 "FUSE-over-io_uring registered" "$logf" || true
}

sqz_umount() {
    [ -d "$SQZ_MNT" ] || return 0
    "$SQZ_BIN" umount "$SQZ_MNT" >/dev/null 2>&1 ||
        fusermount3 -u "$SQZ_MNT" 2>/dev/null || true
    local i
    for i in $(seq 1 150); do
        mountpoint -q "$SQZ_MNT" || break
        sleep 0.2
    done
    # Stale ENOTCONN attachment: the documented pre-existing teardown-SIGBUS
    # class (L1 report) can crash the daemon mid-drain and leave the kernel
    # mount half-dead; the next mount refuses it LOUD. Detach lazily (the
    # mount.fuse.squeezefs precedent) so the grid keeps filling.
    if ! stat "$SQZ_MNT" >/dev/null 2>&1; then
        log "WARN: stale ENOTCONN mountpoint after unmount (teardown-SIGBUS class) — lazy-detaching"
        fusermount3 -uz "$SQZ_MNT" 2>/dev/null || umount -l "$SQZ_MNT" 2>/dev/null || true
        sleep 0.5
    fi
    # Wait for the daemon to drain; kill by PID only as last resort.
    for i in $(seq 1 300); do
        [ -n "$SQZ_PID" ] && [ -d "/proc/$SQZ_PID" ] || break
        sleep 0.2
    done
    if [ -n "$SQZ_PID" ] && [ -d "/proc/$SQZ_PID" ]; then
        log "WARN: squeezefs daemon $SQZ_PID still alive after unmount — SIGKILL by PID"
        kill -9 "$SQZ_PID" 2>/dev/null || true
    fi
    SQZ_PID=""
}

# ---------------------------------------------------------------------------
# JuiceFS stack (meta engine + file:// object store + cache, same substrate)
# ---------------------------------------------------------------------------
JFS_DIR="$SUBSTRATE/jfs"
JFS_MNT="$SUBSTRATE/jfs_mnt"
JFS_CACHE="$JFS_DIR/cache"
JFS_PID=""
REDIS_PID=""
JFS_META=""

jfs_meta_setup() { # auto-detect redis, fall back sqlite3; fresh per regime
    if command -v redis-server >/dev/null 2>&1; then
        if [ -z "$REDIS_PID" ] || ! [ -d "/proc/$REDIS_PID" ]; then
            REDIS_PORT=$((16379 + RANDOM % 1000))
            mkdir -p "$JFS_DIR/redis"
            # Production-honest persistence (their recommended AOF posture)
            # on the SAME substrate as the SqueezeFS meta volumes.
            redis-server --port "$REDIS_PORT" --bind 127.0.0.1 \
                --dir "$JFS_DIR/redis" --appendonly yes --appendfsync everysec \
                --save '' --daemonize no >"$ART/logs/redis.log" 2>&1 &
            REDIS_PID=$!
            sleep 1
            kill -0 "$REDIS_PID" 2>/dev/null || die "redis-server failed to start"
        fi
        redis-cli -p "$REDIS_PORT" flushall >/dev/null 2>&1 || true
        JFS_META="redis://127.0.0.1:$REDIS_PORT/1"
        JFS_META_ENGINE="redis($(redis-server --version | awk '{print $3}'))"
    else
        rm -f "$JFS_DIR"/meta.db*
        JFS_META="sqlite3://$JFS_DIR/meta.db"
        JFS_META_ENGINE="sqlite3"
    fi
}

jfs_format() { # <tag>
    mkdir -p "$JFS_DIR" "$JFS_MNT" "$JFS_CACHE"
    rm -rf "$JFS_DIR/objstore" "$JFS_CACHE"
    mkdir -p "$JFS_DIR/objstore" "$JFS_CACHE"
    jfs_meta_setup
    # --trash-days 0: matched delete semantics — SqueezeFS deletes are real
    # destroys; JuiceFS default trash would rename-to-trash instead.
    "$JUICEFS_BIN" format --storage file --bucket "$JFS_DIR/objstore" \
        --trash-days 0 "$JFS_META" sqzvs \
        >"$ART/logs/jfs_format_$1.log" 2>&1 ||
        die "juicefs format failed (see $ART/logs/jfs_format_$1.log)"
}

jfs_mount() { # <tag> <cage_mb> <cache_size_mb> <buffer_size_mb>
    local tag="$1" memmax="$2" cache_sz="$3" buf_sz="$4"
    local logf="$ART/logs/jfs_mount_${tag}.log"
    cage_cmd "$memmax" "jfs-$tag"
    pin_cmd
    local extra=()
    [ "$JFS_WRITEBACK" = "1" ] && extra+=(--writeback)
    # shellcheck disable=SC2094 # --log is a path arg, not a read
    "${CAGE_ARGV[@]}" "${PIN_ARGV[@]}" "$JUICEFS_BIN" mount -d \
        --no-usage-report \
        --cache-dir "$JFS_CACHE" \
        --cache-size "$cache_sz" \
        --buffer-size "$buf_sz" \
        "${extra[@]}" \
        --log "$logf" \
        "$JFS_META" "$JFS_MNT" >>"$logf" 2>&1
    local i
    for i in $(seq 1 200); do
        mountpoint -q "$JFS_MNT" && break
        sleep 0.3
    done
    mountpoint -q "$JFS_MNT" || {
        tail -5 "$logf" >&2
        die "juicefs mount failed (see $logf)"
    }
    JFS_PID="$(pgrep -f "juicefs mount.*$JFS_MNT" | head -1)"
}

jfs_umount() {
    [ -n "$JUICEFS_BIN" ] && [ -d "$JFS_MNT" ] || return 0
    mountpoint -q "$JFS_MNT" || {
        JFS_PID=""
        return 0
    }
    "$JUICEFS_BIN" umount "$JFS_MNT" >/dev/null 2>&1 || true
    local i
    for i in $(seq 1 150); do
        mountpoint -q "$JFS_MNT" || break
        sleep 0.2
    done
    mountpoint -q "$JFS_MNT" &&
        "$JUICEFS_BIN" umount --force "$JFS_MNT" >/dev/null 2>&1
    if ! stat "$JFS_MNT" >/dev/null 2>&1; then
        fusermount3 -uz "$JFS_MNT" 2>/dev/null || umount -l "$JFS_MNT" 2>/dev/null || true
        sleep 0.5
    fi
    for i in $(seq 1 300); do
        [ -n "$JFS_PID" ] && [ -d "/proc/$JFS_PID" ] || break
        sleep 0.2
    done
    if [ -n "$JFS_PID" ] && [ -d "/proc/$JFS_PID" ]; then
        log "WARN: juicefs daemon $JFS_PID still alive after unmount — SIGKILL by PID"
        kill -9 "$JFS_PID" 2>/dev/null || true
    fi
    JFS_PID=""
}

# ---------------------------------------------------------------------------
# Row machinery (phase-1 row.sh lineage: snapshots + honesty lines per row)
# ---------------------------------------------------------------------------
snap() { # <prefix> <suffix> <mnt> <pid>
    local pfx="$1" sfx="$2" mnt="$3" pid="$4"
    [ -n "$DISK" ] && grep " $DISK " /proc/diskstats >"$pfx.disk.$sfx"
    date +%s.%N >"$pfx.t.$sfx"
    if [ -n "$pid" ] && [ -d "/proc/$pid" ]; then
        cat "/proc/$pid/io" >"$pfx.io.$sfx" 2>/dev/null
        awk '{print $14, $15}' "/proc/$pid/stat" >"$pfx.cpu.$sfx" 2>/dev/null
    fi
    cat "$mnt/.stats" >"$pfx.stats.$sfx" 2>/dev/null || true
}

parse_elbencho() { # <file> <OP> <KEY> -> value (LAST DONE column) or "NA"
    awk -v op="$2" -v key="$3" '
        /^[A-Z]+ +Elapsed/ { cur = $1 }
        $1 == op { cur = op }
        cur == op && index($0, key) {
            for (i = NF; i >= 1; i--) if ($i ~ /^[0-9.]+$/) { print $i; exit }
        }' "$1" 2>/dev/null | head -1 | grep . || echo "NA"
}

# run_row <regime> <workload> <system> <mnt> <pid> <op> <key> <unit> -- <elbencho args...>
run_row() {
    local regime="$1" wl="$2" sys="$3" mnt="$4" pid="$5" op="$6" key="$7" unit="$8"
    shift 8
    [ "$1" = "--" ] && shift
    local rowid="${regime}.${wl}" pfx="$ART/rows/${regime}.${wl}.${sys}"
    mkdir -p "$ART/rows"

    quiet_gate
    local tctl load
    tctl="$(tctl_read)"
    load="$(cut -d' ' -f1 /proc/loadavg)"

    snap "$pfx" before "$mnt" "$pid"
    pin_cmd
    timeout -k 10 "$ROW_TIMEOUT" "${PIN_ARGV[@]}" "$ELBENCHO_BIN" "$@" \
        >"$pfx.elbencho" 2>&1
    local rc=$?
    snap "$pfx" after "$mnt" "$pid"

    local dirty_post=""
    local cot
    cot="$(cotenants)"
    [ -n "$cot" ] && dirty_post="DIRTY(post:$cot)"
    local dirty="${ROW_DIRTY_PRE}${ROW_DIRTY_COLD}${dirty_post}"
    ROW_DIRTY_COLD=""
    [ -z "$dirty" ] && dirty="quiet"
    [ "$rc" -eq 124 ] && log "WARN: $rowid.$sys HIT ROW TIMEOUT (${ROW_TIMEOUT}s) — wedged-mount class"

    local value totmib elapsed
    value="$(parse_elbencho "$pfx.elbencho" "$op" "$key")"
    totmib="$(parse_elbencho "$pfx.elbencho" "$op" "Total MiB")"
    elapsed="$(python3 -c "
t0=open('$pfx.t.before').read().strip(); t1=open('$pfx.t.after').read().strip()
print(f'{float(t1)-float(t0):.1f}')" 2>/dev/null)"

    # Device evidence (diskstats delta over the row) + daemon CPU.
    local devline=""
    if [ -n "$DISK" ] && [ -s "$pfx.disk.before" ] && [ -s "$pfx.disk.after" ]; then
        devline="$(python3 - "$pfx" <<'EOF'
import sys
p = sys.argv[1]
d0 = open(f"{p}.disk.before").read().split()
d1 = open(f"{p}.disk.after").read().split()
dt = float(open(f"{p}.t.after").read()) - float(open(f"{p}.t.before").read())
rio = (int(d1[3]) - int(d0[3])) / dt
rmib = (int(d1[5]) - int(d0[5])) / 2 / 1024 / dt
wio = (int(d1[7]) - int(d0[7])) / dt
wmib = (int(d1[9]) - int(d0[9])) / 2 / 1024 / dt
cores = ""
try:
    u0, s0 = map(int, open(f"{p}.cpu.before").read().split())
    u1, s1 = map(int, open(f"{p}.cpu.after").read().split())
    cores = f" daemon_cores={(u1 + s1 - u0 - s0) / dt / 100:.1f}"
except OSError:
    pass
print(f"dev_r/s={rio:.0f} dev_rMiB/s={rmib:.0f} dev_w/s={wio:.0f} dev_wMiB/s={wmib:.0f}{cores}")
EOF
)"
    fi

    # Serve-evidence + device-true verification per system.
    local verify
    verify="$(VERIFY_REGIME="$regime" VERIFY_WL="$wl" python3 - "$pfx" "$sys" "$totmib" <<'EOF'
import json, os, sys
p, sysname, totmib = sys.argv[1], sys.argv[2], sys.argv[3]
regime, wl = os.environ["VERIFY_REGIME"], os.environ["VERIFY_WL"]
def jload(path):
    try:
        return json.load(open(path)).get("metrics", {})
    except Exception:
        return {}
def pload(path):
    out = {}
    try:
        for line in open(path):
            parts = line.split()
            if len(parts) == 2:
                try: out[parts[0]] = float(parts[1])
                except ValueError: pass
    except OSError:
        pass
    return out
try:
    user_mib = float(totmib)
except ValueError:
    user_mib = 0.0
try:
    d0 = open(f"{p}.disk.before").read().split()
    d1 = open(f"{p}.disk.after").read().split()
    dev_read_mib = (int(d1[5]) - int(d0[5])) / 2 / 1024
except Exception:
    dev_read_mib = -1.0
msgs = []
if sysname == "sqz":
    b, a = jload(f"{p}.stats.before"), jload(f"{p}.stats.after")
    get_d = a.get("get_obj", 0) - b.get("get_obj", 0)
    dtr_d = a.get("read_device_true_reads", 0) - b.get("read_device_true_reads", 0)
    msgs.append(f"get_objΔ={get_d} device_true_readsΔ={dtr_d} mode={a.get('direct_device_true')}")
    if regime == "R2" and "read" in wl:
        ok = a.get("direct_device_true") is True and dtr_d > 0
        if ok and user_mib > 0 and dev_read_mib >= 0:
            ok = dev_read_mib >= 0.5 * user_mib
        msgs.append("VERIFY=device-true-OK" if ok else "VERIFY=FAILED-not-device-true")
else:
    b, a = pload(f"{p}.stats.before"), pload(f"{p}.stats.after")
    hits_d = a.get("juicefs_blockcache_hits", 0) - b.get("juicefs_blockcache_hits", 0)
    get_mib = (a.get("juicefs_object_request_data_bytes_GET", 0)
               - b.get("juicefs_object_request_data_bytes_GET", 0)) / 2**20
    msgs.append(f"blockcache_hitsΔ={hits_d:.0f} objGETΔMiB={get_mib:.0f}")
    if regime == "R2" and "read" in wl:
        ok = user_mib > 0 and get_mib >= 0.9 * user_mib
        if ok and dev_read_mib >= 0:
            ok = dev_read_mib >= 0.5 * user_mib
        msgs.append("VERIFY=device-true-OK" if ok else "VERIFY=FAILED-cache-or-pagecache-serve")
print(" ".join(msgs))
EOF
)"

    {
        echo "rowid=$rowid sys=$sys rc=$rc value=$value unit=$unit total_mib=$totmib elapsed=${elapsed}s"
        echo "honesty: tctl=${tctl:-na}C load=$load $dirty"
        [ -n "$devline" ] && echo "device: $devline"
        echo "serve: $verify"
    } | tee "$pfx.env"

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$rowid" "$regime" "$wl" "$sys" "$unit" "$value" "$rc" "$dirty" \
        "$(echo "$devline" | tr ' ' ';')" "$(echo "$verify" | tr ' \t' ';;')" \
        >>"$ROWS_TSV"

    [ "$rc" -ne 0 ] && log "WARN: elbencho rc=$rc on $rowid.$sys (row recorded as-is)"
    return 0
}

# ---------------------------------------------------------------------------
# Workload grid
# ---------------------------------------------------------------------------
dataset_files() { # <mnt> -> fills DATA_FILES array
    local mnt="$1" i
    DATA_FILES=()
    for i in $(seq -w 1 16); do DATA_FILES+=("$mnt/vsdata/f$i"); done
}

ensure_dataset() { # <mnt> — untimed prep when seq_write_1m isn't in the grid
    local mnt="$1"
    dataset_files "$mnt"
    [ -f "${DATA_FILES[0]}" ] && return 0
    mkdir -p "$mnt/vsdata"
    log "prep: creating dataset (untimed)"
    pin_cmd
    timeout -k 10 $((ROW_TIMEOUT * 2)) \
        "${PIN_ARGV[@]}" "$ELBENCHO_BIN" -w -t "$THREADS" -s "${FILE_MB}m" -b 1m \
        --direct "${DATA_FILES[@]}" >"$ART/logs/prep_dataset.$RANDOM.log" 2>&1 ||
        die "dataset prep failed"
}

ensure_tree() { # <mnt> — untimed prep for stat/del storms
    local mnt="$1"
    [ -d "$mnt/vstree" ] && [ -n "$(ls -A "$mnt/vstree" 2>/dev/null)" ] && return 0
    mkdir -p "$mnt/vstree"
    log "prep: creating tree ($((THREADS * TREE_DIRS * TREE_FILES)) files, untimed)"
    pin_cmd
    timeout -k 10 $((ROW_TIMEOUT * 2)) \
        "${PIN_ARGV[@]}" "$ELBENCHO_BIN" -w -d -t "$THREADS" -n "$TREE_DIRS" \
        -N "$TREE_FILES" -s 4k "$mnt/vstree" >"$ART/logs/prep_tree.$RANDOM.log" 2>&1 ||
        die "tree prep failed"
}

# run_workload <regime> <workload> <system> <mnt> <pid>
run_workload() {
    local regime="$1" wl="$2" sys="$3" mnt="$4" pid="$5"
    dataset_files "$mnt"
    case "$wl" in
    seq_write_1m)
        mkdir -p "$mnt/vsdata"
        rm -f "${DATA_FILES[@]}"
        run_row "$regime" "$wl" "$sys" "$mnt" "$pid" WRITE "Throughput MiB/s" "MiB/s" -- \
            -w -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct "${DATA_FILES[@]}"
        ;;
    seq_read_1m)
        ensure_dataset "$mnt"
        run_row "$regime" "$wl" "$sys" "$mnt" "$pid" READ "Throughput MiB/s" "MiB/s" -- \
            -r -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct "${DATA_FILES[@]}"
        ;;
    rand_read_4k) # the user's exact iodepth line
        ensure_dataset "$mnt"
        run_row "$regime" "$wl" "$sys" "$mnt" "$pid" READ "IOPS" "IOPS" -- \
            -r --rand -t "$THREADS" -b 4k --iodepth 16 --direct \
            --timelimit "$TIMELIMIT" "${DATA_FILES[@]}"
        ;;
    rand_write_4k)
        ensure_dataset "$mnt"
        run_row "$regime" "$wl" "$sys" "$mnt" "$pid" WRITE "IOPS" "IOPS" -- \
            -w --rand -t "$THREADS" -s "${FILE_MB}m" -b 4k --iodepth 16 --direct \
            --timelimit "$TIMELIMIT" "${DATA_FILES[@]}"
        ;;
    stat_storm)
        ensure_tree "$mnt"
        run_row "$regime" "$wl" "$sys" "$mnt" "$pid" STAT "Files/s" "files/s" -- \
            --stat -t "$THREADS" -n "$TREE_DIRS" -N "$TREE_FILES" "$mnt/vstree"
        ;;
    del_storm)
        ensure_tree "$mnt"
        run_row "$regime" "$wl" "$sys" "$mnt" "$pid" RMFILES "Files/s" "files/s" -- \
            -F -D -t "$THREADS" -n "$TREE_DIRS" -N "$TREE_FILES" "$mnt/vstree"
        ;;
    *) die "unknown workload '$wl'" ;;
    esac
}

# ---------------------------------------------------------------------------
# Regimes
# ---------------------------------------------------------------------------
mount_for_regime() { # <regime> <system> <tag>
    local regime="$1" sys="$2" tag="$3"
    if [ "$sys" = "sqz" ]; then
        case "$regime" in
        R2) sqz_mount "$tag" "$CAGE_MB" -o direct_device_true ;;
        *) sqz_mount "$tag" "$CAGE_MB" ;;
        esac
    else
        case "$regime" in
        # R2: cache-size 0, cache-dir stays on the substrate, tight cage to
        # defeat the file:// object store's kernel page-cache serve (the
        # decomposition-report mechanism — JuiceFS has no device-true knob).
        # buffer-size = their shipped default 300M, exactly the
        # decomposition's forced-device-serve row config; larger buffers
        # OOM-loop the daemon inside the tight cage (smoke-run finding).
        R2) jfs_mount "$tag" "$R2_JFS_CAGE_MB" 0 300 ;;
        *) jfs_mount "$tag" "$CAGE_MB" "$CACHE_MB" "$CACHE_MB" ;;
        esac
    fi
}

# A daemon death mid-grid (e.g. cage OOM) must cost ONE flagged remount and
# keep the scoreboard filling — never abort the remaining rows (the row that
# died still records rc!=0/NA and gates). PIDs are refreshed every call:
# `juicefs mount -d` runs a supervisor that can respawn the daemon silently.
ensure_alive() { # <regime> <system> <tag>
    local regime="$1" sys="$2" tag="$3" mnt
    if [ "$sys" = "sqz" ]; then mnt="$SQZ_MNT"; else mnt="$JFS_MNT"; fi
    if ! mountpoint -q "$mnt" 2>/dev/null || ! stat "$mnt" >/dev/null 2>&1; then
        log "WARN: $sys mount dead before $tag — remounting once (row flagged)"
        ROW_DIRTY_COLD="${ROW_DIRTY_COLD}DIRTY(remounted-dead-daemon),"
        if [ "$sys" = "sqz" ]; then sqz_umount; else jfs_umount; fi
        mount_for_regime "$regime" "$sys" "remount_${tag}"
    fi
    if [ "$sys" = "sqz" ]; then
        SQZ_PID="$(pgrep -f "squeezefs mount sqmeta://$SQZ_DIR" | head -1)"
    else
        JFS_PID="$(pgrep -f "juicefs mount.*$JFS_MNT" | head -1)"
    fi
}

cold_reset() { # <regime> <system> <tag> — R3 full drop + remount before a row
    local regime="$1" sys="$2" tag="$3"
    if [ "$sys" = "sqz" ]; then sqz_umount; else jfs_umount; fi
    if [ "$sys" = "jfs" ]; then
        rm -rf "$JFS_CACHE"
        mkdir -p "$JFS_CACHE"
    fi
    if ! drop_caches; then
        COLD_DEGRADED=1
        # page cache survived: not cold
        ROW_DIRTY_COLD="${ROW_DIRTY_COLD}DIRTY(no-page-drop),"
    fi
    mount_for_regime "$regime" "$sys" "$tag"
}

run_regime_system() { # <regime> <system>
    local regime="$1" sys="$2" wl pid mnt
    log "=== $regime / $sys ==="
    if [ "$sys" = "sqz" ]; then
        sqz_format "${regime}"
        mount_for_regime "$regime" "$sys" "${regime}"
        mnt="$SQZ_MNT"
    else
        jfs_format "${regime}"
        mount_for_regime "$regime" "$sys" "${regime}"
        mnt="$JFS_MNT"
    fi
    for wl in $WORKLOADS; do
        ensure_alive "$regime" "$sys" "${regime}_${wl}"
        if [ "$regime" = "R3" ]; then
            # Cold-cache: prep state warm, then full drop + remount, then the
            # timed first pass.
            case "$wl" in
            seq_write_1m) ;; # cold by construction on the fresh volume
            stat_storm | del_storm)
                ensure_tree "$mnt"
                cold_reset "$regime" "$sys" "${regime}_${wl}"
                ;;
            *)
                ensure_dataset "$mnt"
                cold_reset "$regime" "$sys" "${regime}_${wl}"
                ;;
            esac
        elif [ "$regime" = "R2" ]; then
            # Device-true reads must not inherit residual page cache from the
            # dataset-writing pass (the objstore files were just written —
            # smoke-run finding: seq_read GETs served from page cache at 1.00x
            # GET amplification but zero device reads). Best-effort drop; the
            # per-row VERIFY line remains the authority.
            case "$wl" in
            seq_read_1m | rand_read_4k)
                drop_caches || ROW_DIRTY_COLD="${ROW_DIRTY_COLD}FLAG(no-page-drop),"
                ;;
            esac
        fi
        if [ "$sys" = "sqz" ]; then pid="$SQZ_PID"; else pid="$JFS_PID"; fi
        run_workload "$regime" "$wl" "$sys" "$mnt" "$pid"
    done
    if [ "$sys" = "sqz" ]; then sqz_umount; else jfs_umount; fi
}

# ---------------------------------------------------------------------------
# Teardown
# ---------------------------------------------------------------------------
# shellcheck disable=SC2329 # invoked via the EXIT trap
cleanup() {
    trap - EXIT
    sqz_umount || true
    jfs_umount || true
    if [ -n "$REDIS_PID" ] && [ -d "/proc/$REDIS_PID" ]; then
        kill "$REDIS_PID" 2>/dev/null || true
    fi
    if [ "$KEEP" != "1" ]; then
        rm -rf "$SQZ_DIR" "$JFS_DIR"
        rmdir "$SQZ_MNT" "$JFS_MNT" 2>/dev/null || true
        log "substrate stores removed (SQUEEZEFS_VS_KEEP=1 to retain); artifacts kept at $ART"
    else
        log "substrate stores retained at $SUBSTRATE"
    fi
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
    guard_paths
    check_substrate
    mkdir -p "$ART/logs" "$ART/rows"
    : >"$ROWS_TSV"

    # Binaries: build squeezefs if missing (as the invoking user when sudo'd),
    # resolve or sandbox-install juicefs + elbencho. Never system paths.
    if [ ! -x "$SQZ_BIN" ]; then
        log "building squeezefs release binary"
        if [ "$(id -u)" -eq 0 ] && [ -n "$RUNUSER" ] && [ "$RUNUSER" != "root" ]; then
            su -s /bin/bash "$RUNUSER" -c "cd '$REPO_DIR' && cargo build --release" ||
                die "cargo build failed"
        else
            (cd "$REPO_DIR" && cargo build --release) || die "cargo build failed"
        fi
    fi
    JUICEFS_BIN="$(find_or_install_tool juicefs install_juicefs)"
    [ -n "$JUICEFS_BIN" ] || die "juicefs unavailable and sandbox install failed"
    ELBENCHO_BIN="$(find_or_install_tool elbencho install_elbencho)"
    [ -n "$ELBENCHO_BIN" ] || die "elbencho unavailable and sandbox install failed"
    probe_cages

    if [ "$(id -u)" -ne 0 ] && ! sudo -n true 2>/dev/null; then
        case " $REGIMES " in *" R3 "*)
            log "WARN: no root and no passwordless sudo — R3 page-cache drops degraded (rows will be flagged)"
            ;;
        esac
    fi

    # Version + provenance pinning (goes into the scoreboard verbatim).
    local sqz_sha sqz_ver jfs_ver eb_ver kern
    sqz_sha="$(git -C "$REPO_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)"
    sqz_ver="squeezefs @ $sqz_sha (md5 $(md5sum "$SQZ_BIN" | cut -d' ' -f1))"
    jfs_ver="$("$JUICEFS_BIN" version 2>/dev/null || "$JUICEFS_BIN" --version 2>/dev/null | head -1)"
    eb_ver="$("$ELBENCHO_BIN" --version 2>/dev/null | awk '/Version/{print $3; exit}')"
    kern="$(uname -r)"

    log "substrate: $SUBSTRATE ($SUB_FSTYPE on $SUB_SRC, disk=$DISK)"
    log "budget: ${CACHE_MB} MiB matched | dataset: ${DATASET_GB} GiB (16 x ${FILE_MB} MiB) | tree: $((THREADS * TREE_DIRS * TREE_FILES)) files"
    log "$sqz_ver"
    log "juicefs: $jfs_ver"
    log "elbencho: $eb_ver | kernel: $kern | cpus: $ONLINE_CPUS (cpuset: ${CPUSET:-none})"
    [ "$SMOKE" = "1" ] && log "SMOKE MODE: micro-grid, gate disabled"

    trap cleanup EXIT
    session_quiet_gate

    local regime sys
    for regime in $REGIMES; do
        for sys in $SYSTEMS; do
            run_regime_system "$regime" "$sys"
        done
    done

    # -----------------------------------------------------------------------
    # Scoreboard: merge rows, compute verdicts, emit markdown + TSV, gate.
    # -----------------------------------------------------------------------
    SCORE_META="sqz=$sqz_ver | jfs=$jfs_ver (meta=$JFS_META_ENGINE) | elbencho=$eb_ver | kernel=$kern | box=$(nproc)cpu/$(free -g | awk '/^Mem/{print $2}')GiB | substrate=$SUB_FSTYPE:$SUB_SRC | cache_budget=${CACHE_MB}MiB | dataset=${DATASET_GB}GiB | cage=${CAGE_MB}MiB (R2 jfs ${R2_JFS_CAGE_MB}MiB) | cold_degraded=$COLD_DEGRADED" \
        SMOKE="$SMOKE" ALLOW_LOSS="$ALLOW_LOSS" \
        python3 - "$ROWS_TSV" "$ART/scoreboard.md" "$ART/scoreboard.tsv" <<'EOF'
import os, sys
rows_tsv, out_md, out_tsv = sys.argv[1], sys.argv[2], sys.argv[3]
smoke = os.environ.get("SMOKE") == "1"
allow = {r.strip() for r in os.environ.get("ALLOW_LOSS", "").split(",") if r.strip()}
meta = os.environ.get("SCORE_META", "")

rows = {}
order = []
for line in open(rows_tsv):
    f = line.rstrip("\n").split("\t")
    if len(f) < 10:
        continue
    rowid, regime, wl, sysname, unit, value, rc, dirty, dev, verify = f[:10]
    if rowid not in rows:
        rows[rowid] = {"regime": regime, "wl": wl, "unit": unit}
        order.append(rowid)
    try:
        v = float(value)
    except ValueError:
        v = None
    rows[rowid][sysname] = {"v": v, "rc": rc, "dirty": dirty, "verify": verify, "dev": dev}

def fmt(v, unit):
    if v is None:
        return "NA"
    return f"{v:,.0f}" if (unit != "MiB/s" or v >= 100) else f"{v:,.1f}"

losses, invalid = [], []
md = ["| Row | Workload | JFS | SQZ | SQZ/JFS | Verdict | Flags |",
      "|---|---|---:|---:|---:|:--:|---|"]
tsv = ["row_id\tregime\tworkload\tunit\tjfs\tsqz\tratio\tverdict\tflags"]
for rowid in order:
    r = rows[rowid]
    j, s = r.get("jfs"), r.get("sqz")
    jv = j["v"] if j else None
    sv = s["v"] if s else None
    flags = []
    for name, side in (("jfs", j), ("sqz", s)):
        if side:
            if side["dirty"] != "quiet":
                flags.append(f"{name}:{side['dirty']}")
            if "VERIFY=FAILED" in side.get("verify", ""):
                flags.append(f"{name}:!DEV")
            if side["rc"] != "0":
                flags.append(f"{name}:rc={side['rc']}")
    ratio = None
    if jv and sv and jv > 0:
        ratio = sv / jv
    if sv is None or (s and "VERIFY=FAILED" in s.get("verify", "")):
        # A missing/unverified SQZ row can never claim a win; ALLOW_LOSS
        # covers it for known-loss tracking like any other loss.
        verdict = "INVALID" + (" (allowed)" if rowid in allow else "")
        if rowid not in allow:
            invalid.append(rowid)
    elif jv is None:
        verdict = "W (jfs NA)"
    elif ratio is None:
        verdict = "INVALID" + (" (allowed)" if rowid in allow else "")
        if rowid not in allow:
            invalid.append(rowid)
    elif ratio > 1.05:
        verdict = "**W**"
    elif ratio < 0.95:
        verdict = "L" + (" (allowed)" if rowid in allow else "")
        if rowid not in allow:
            losses.append(rowid)
    else:
        verdict = "TIE"
    unit = r["unit"]
    md.append(f"| {rowid} | {r['wl']} ({unit}) | {fmt(jv, unit)} | {fmt(sv, unit)} | "
              f"{ratio:.2f}x | {verdict} | {' '.join(flags)} |"
              if ratio is not None else
              f"| {rowid} | {r['wl']} ({unit}) | {fmt(jv, unit)} | {fmt(sv, unit)} | "
              f"— | {verdict} | {' '.join(flags)} |")
    tsv.append(f"{rowid}\t{r['regime']}\t{r['wl']}\t{unit}\t{jv if jv is not None else 'NA'}\t"
               f"{sv if sv is not None else 'NA'}\t{f'{ratio:.4f}' if ratio else 'NA'}\t"
               f"{verdict.replace('*', '')}\t{','.join(flags)}")

header = ["# SqueezeFS vs JuiceFS scoreboard", "",
          f"Provenance: {meta}", "",
          "Verdict rule: W if SQZ > 1.05x JFS, L if < 0.95x, TIE inside ±5%. "
          "INVALID = missing/unverified SQZ row (counts as loss for the gate).", ""]
open(out_md, "w").write("\n".join(header + md) + "\n")
open(out_tsv, "w").write("\n".join(tsv) + "\n")
print("\n".join(header + md))

gate_fail = losses + invalid
if gate_fail and not smoke:
    print(f"\nGATE: FAIL — loss/invalid rows: {', '.join(gate_fail)}", file=sys.stderr)
    sys.exit(1)
print(f"\nGATE: {'SMOKE (not gating)' if smoke else 'GREEN — no loss rows'}")
EOF
    local gate_rc=$?

    log "scoreboard: $ART/scoreboard.md (+ .tsv); raw rows + counter snapshots + device evidence: $ART/rows/"
    exit "$gate_rc"
}

main "$@"
