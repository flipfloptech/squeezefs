#!/bin/bash
# SqueezeFS multi-reference scoreboard — the standing top-3 proof surface.
#
# PERFORMANCE IS PRIMARY (user directive, 2026-07-18): "if we are not one of
# the top 3 fastest FUSE filesystems that exist, I don't care about any other
# feature." This harness is that claim's proof surface: a re-runnable,
# matched-conditions A/B/n that measures SqueezeFS against the reference
# fast-FUSE field on the same substrate with matched budgets and identical
# elbencho drivers, and adjudicates a per-row-family TOP-3 rank. Every future
# perf lever re-runs it. It absorbs and supersedes tests/run_vs_juicefs.sh
# (forward-only; the JuiceFS column and its verdict lineage carry over).
#
# The kernel-FUSE table this emits is the PRIMARY claim surface. A future L4
# LD_PRELOAD interception mode will add separately-labeled rows — do not mix
# those semantics into this grid.
#
# Reference systems (user-confirmed set; pinned releases, fetch-if-missing):
#   jfs   JuiceFS          — meta engine (redis if present, else sqlite3) +
#                            file:// object store on the substrate
#   swfs  SeaweedFS        — native stack: `weed server` (master+volume+
#                            filer) + `weed mount` (FUSE)
#   gee   geesefs          — S3-backed FUSE client over the local RustFS
#                            object store
#   mps3  mountpoint-s3    — S3-backed FUSE client over the SAME local RustFS
#                            object store (object store held constant so the
#                            gee/mps3 rows differ only by client)
#   goofys is SKIPPED (unmaintained); MooseFS REJECTED (user: not
#   interested). Do not add others without a user directive.
#
# Protocol (normative; extends the 2026-07-15 vs-JuiceFS protocol):
#   Matched substrate  — every system's durable stores (SqueezeFS meta+data
#                        volumes, JuiceFS meta db + objstore + cache, weed
#                        master/volume/filer stores, RustFS volumes, all
#                        client caches) live under SQUEEZEFS_SB_SUBSTRATE_DIR
#                        (refused if tmpfs).
#   Matched budgets    — one knob (SQUEEZEFS_SB_CACHE_MB) drives every
#                        system's documented cache surface:
#                        sqz  --mem-budget ${MB}M + --disk-cache-size ${MB}MB
#                        jfs  --buffer-size ${MB} + --cache-size ${MB}
#                        swfs weed mount -cacheCapacityMB=${MB}
#                        gee  --memory-limit ${MB} (their RAM data cache;
#                             disk cache stays off = their default, and they
#                             ship no disk-cache size cap to match against)
#                        mps3 --cache <dir> --max-cache-size ${MB} (their
#                             documented caching config; bare default is
#                             cache-off which would be an unfair R1 posture)
#                        The mapping is honest but not exact (refs do not
#                        budget their meta engines / index RAM; SqueezeFS's
#                        one budget carries everything). Where it errs, it
#                        errs in the references' favor.
#   Fair posture       — each ref runs its documented recommended/default
#                        configuration otherwise. Recorded deviations:
#                        jfs --trash-days 0 (matched delete semantics),
#                        mps3 --allow-delete --allow-overwrite (documented
#                        opt-ins the grid requires: del rows + fresh-file
#                        rewrites), mps3 --force-path-style (local S3
#                        endpoint), gee/mps3 --endpoint (local RustFS).
#                        Nothing is crippled, nothing heroically tuned.
#   Three regimes      — R1 as-deployed (all cache layers live, capped
#                        budgets, dataset 2–4x cache);
#                        R2 device-true (sqz -o direct_device_true, verified
#                        via .stats; jfs --cache-size 0 --buffer-size 300 +
#                        tight cage [decomposition-report mechanism]; swfs
#                        -cacheCapacityMB=0 + tight cages on mount AND weed
#                        server; gee --memory-limit 300 + tight cages on
#                        client AND RustFS; mps3 no --cache (their default) +
#                        tight cages on client AND RustFS; read rows verified
#                        per row via /proc/diskstats device-byte evidence);
#                        R3 cold-cache (full page-cache drops + client cache
#                        wipes + remounts, first pass).
#   Workload grid      — 6 shapes per regime, elbencho throughout (identical
#                        drivers, page-aligned O_DIRECT buffers — the
#                        instrument-alignment lesson): seq write 1M / seq
#                        read 1M / rand read 4k (the user's exact line:
#                        -t 16 -b 4k --iodepth 16 --direct) / rand write 4k /
#                        stat storm / del storm. n=1 per row, elbencho LAST
#                        DONE steady state, quiet-gated (house discipline).
#
# RW6 — durability-leveled timing (write-family rows; harness honesty fix):
#   Write rows carry TWO modes:
#     relaxed  — the row as elbencho reports it: each system's native ACK
#                semantics (page-cache/async-flush ACKs included), published
#                LABELED, never gating.
#     durable  — fsync/fdatasync-INCLUSIVE timing at matched durability
#                level: data durable before the clock stops. GOVERNS the
#                write-family verdicts.
#   Mechanism (verified against elbencho v3.1-9 SOURCE, Coordinator.cpp +
#   LocalWorker::anyModeSync): elbencho's only durability flag, --sync, runs
#   as a SEPARATE post-phase SYNC step (syncfs on the bench paths) whose
#   elapsed is excluded from the WRITE row, and several FUSE clients no-op
#   FUSE_SYNCFS — so --sync alone cannot deliver durable write numbers, and
#   elbencho has no fsync-at-close/O_SYNC option. The harness therefore times
#   its own durability pass immediately after the elbencho WRITE phase,
#   identical for every system:
#     (a) os.fdatasync() on every dataset file through the FUSE mount
#         (O_RDWR reopen; O_RDONLY fallback where write-open is refused —
#         mount-s3, whose uploads already completed at close inside the
#         timed phase), then
#     (b) syncfs() on the mount (FUSE_SYNCFS where honored), then
#     (c) syncfs() on the SUBSTRATE directory — flushes every system's
#         local backing store (JuiceFS file:// objects, RustFS volumes, weed
#         volume files, SqueezeFS image files) to the device, the matched
#         "bytes on stable storage" boundary.
#   durable value = phase bytes (or ops) / (elbencho WRITE elapsed [csv
#   `time ms [last]`] + durability-pass elapsed). Read rows are unaffected
#   (no durability semantics). Known ACK postures recorded per ref: geesefs
#   ACKs writes before flush by default (--fsync-on-close off; fsync honored
#   — its relaxed rows carry the `ack-async` label); JuiceFS file:// objstore
#   ACKs ride the kernel page cache; mount-s3 close() blocks on upload
#   completion (relaxed ≈ durable by construction).
#
# Capability matrix (N/S cells — user-locked semantics):
#   Where a reference does not support a workload BY DESIGN the cell reads
#   "N/S (not supported by design)" — NEVER "0 IOPS", never a LOSS; N/S cells
#   are neutral for the gate and excluded from top-3 ranking denominators.
#   The matrix is declarative (ns_reason below, one-line reason per cell) and
#   VERIFIED EMPIRICALLY at mount time: each declared-N/S op is attempted and
#   its refusal errno recorded in capabilities.tsv (a probe that SUCCEEDS
#   flags matrix rot loudly and the row runs anyway).
#   Current matrix: mps3.rand_write_4k — sequential-upload semantics, no
#   random/out-of-order writes by design (verified EBADF on this box).
#
# Verdicts & gate:
#   Per primary row (read/stat/del rows in relaxed mode; write rows in
#   durable mode), per reference: W if SQZ > 1.05x ref, L if < 0.95x, TIE
#   inside ±5%. Rank = SqueezeFS's position among the numeric cells of the
#   row (1 = fastest). Exit nonzero on ANY unattributed LOSS or INVALID
#   (missing/unverified/failed SqueezeFS cell) in the primary table.
#   SQUEEZEFS_SB_ALLOW_LOSS names attributed known losses
#   ("R1.seq_write_1m" = row vs every ref, "R1.seq_write_1m.jfs" = row vs
#   one ref). Post-RW6 the allowlist is expected EMPTY: the historical
#   R1/R3.seq_write_1m entries were the JuiceFS page-cache-ACK artifact that
#   durable mode exists to retire. A reference whose own setup/run fails is
#   recorded n/a-with-reason (named residual, not a gate event); SqueezeFS
#   failures always gate — never weaken the sqz side to make a run pass.
#
# Usage:
#   tests/run_scoreboard.sh                  # full scoreboard (~1.5-3 h)
#   tests/run_scoreboard.sh teardown         # kill owned procs + wipe stores
#   SQUEEZEFS_SB_SMOKE=1 tests/run_scoreboard.sh    # micro-grid plumbing
#   SQUEEZEFS_SB_SYSTEMS="sqz jfs" SQUEEZEFS_SB_REGIMES="R2" ...
#   (legacy SQUEEZEFS_VS_* spellings of every knob are honored for
#    compatibility — e.g. SQUEEZEFS_VS_SMOKE, SQUEEZEFS_VS_ALLOW_LOSS.)
#
# Exit code: 0 = gate green (or smoke); 1 = unattributed loss/invalid row;
# 2 = harness/setup failure.
#
# Env knobs (all optional; SQUEEZEFS_SB_* preferred, SQUEEZEFS_VS_* honored):
#   ..._SUBSTRATE_DIR   matched substrate root (default
#                       /var/tmp/squeezefs_scoreboard; tmpfs refused)
#   ..._TOOLS_DIR       pinned-tool cache OUTSIDE the repo (default
#                       /var/tmp/squeezefs-scoreboard-tools)
#   ..._CACHE_MB        matched cache budget MiB (default 4096)
#   ..._DATASET_GB      seq/rand dataset GiB, 16 files (default 16)
#   ..._TREE_DIRS/_TREE_FILES  stat/del tree geometry per thread
#   ..._CAGE_MB         daemon memcg cage MiB, all systems (default 16384)
#   ..._R2_REF_CAGE_MB  R2 reference-stack cage MiB (default 2048; legacy
#                       spelling SQUEEZEFS_VS_R2_JFS_CAGE_MB honored)
#   ..._TIMELIMIT       rand-row seconds (default 30)
#   ..._ROW_TIMEOUT     per-row hard timeout seconds (default 1200)
#   ..._REGIMES         subset of "R1 R2 R3"
#   ..._WORKLOADS       subset of the 6-shape grid
#   ..._SYSTEMS         subset of "sqz jfs swfs gee mps3" (sqz required for
#                       verdicts)
#   ..._ALLOW_LOSS      comma list of attributed-loss row ids (see above)
#   ..._PORT_BASE       scoreboard port slice base (default 53300; the slice
#                       is base+0..base+99 plus weed gRPC at port+10000 —
#                       53311 redis, 53321/22/23 weed master/volume/filer
#                       [gRPC 63321/22/23], 53331 rustfs; jfs metrics binds
#                       port 0 (kernel-assigned — their default 9567 can be
#                       a user tenant).
#                       Reserved elsewhere: 52026 devsub, 52470/52471,
#                       54000-54099 nvmeof fidelity — do not point the base
#                       at those slices)
#   ..._QUIET_LOAD/_QUIET_POLLS/_QUIET_SECS  quiet-gate posture
#   ..._CPUSET          taskset range (default 0-15 when >=20 CPUs online)
#   ..._KEEP=1          keep substrate stores after the run
#   ..._OUT_DIR         artifacts dir (default $SUBSTRATE/artifacts/<UTC ts>)
#   SQUEEZEFS_VS_JFS_WRITEBACK=1  A/B JuiceFS staged-writeback posture
#                       (ships off; scoreboard measures shipped defaults)
#
# Pinned versions (exact releases + checksums, recorded in provenance):
#   JuiceFS 1.4.0, SeaweedFS 4.39, geesefs 0.43.8, mountpoint-s3 1.22.3,
#   RustFS 1.0.0-beta.10 (no stable release exists — newest beta, recorded
#   as such), elbencho 3.1-9. See the PIN_* table below.
#
# Safety rails: kills by PID only; refuses to operate under /mnt/squeezefs,
# /mnt/juicefs, or ~/tmp/nvme; never touches the user's own juicefs/redis
# containers, /usr/local/bin/squeezefs, zram/null_blk/nvmet state, or any
# process it did not spawn; dedicated port slice preflighted free; daemons
# run in systemd-run scopes (memcg cages); quiet-gate before every timed
# row; rows re-checked for co-tenants afterward and DIRTY-flagged, never
# silently blended; full `teardown` verb.

set -uo pipefail

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RUNUSER="${SUDO_USER:-}"
VERB="${1:-run}"

env2() { # env2 <SUFFIX> <default> — SQUEEZEFS_SB_* preferred, SQUEEZEFS_VS_* honored
    local sb="SQUEEZEFS_SB_$1" vs="SQUEEZEFS_VS_$1"
    if [ -n "${!sb:-}" ]; then
        echo "${!sb}"
    elif [ -n "${!vs:-}" ]; then
        echo "${!vs}"
    else
        echo "$2"
    fi
}

SMOKE="$(env2 SMOKE 0)"
SUBSTRATE="$(env2 SUBSTRATE_DIR /var/tmp/squeezefs_scoreboard)"
TOOLS_DIR="$(env2 TOOLS_DIR /var/tmp/squeezefs-scoreboard-tools)"
CACHE_MB="$(env2 CACHE_MB 4096)"
CAGE_MB="$(env2 CAGE_MB 16384)"
R2_REF_CAGE_MB="$(env2 R2_REF_CAGE_MB "${SQUEEZEFS_VS_R2_JFS_CAGE_MB:-2048}")"
THREADS=16
REGIMES="$(env2 REGIMES "R1 R2 R3")"
WORKLOADS="$(env2 WORKLOADS "seq_write_1m seq_read_1m rand_read_4k rand_write_4k stat_storm del_storm")"
SYSTEMS="$(env2 SYSTEMS "sqz jfs swfs gee mps3")"
ALLOW_LOSS="$(env2 ALLOW_LOSS "")"
QUIET_LOAD="$(env2 QUIET_LOAD 2.0)"
PORT_BASE="$(env2 PORT_BASE 53300)"
KEEP="$(env2 KEEP 0)"
JFS_WRITEBACK="${SQUEEZEFS_VS_JFS_WRITEBACK:-0}"

# Pinned releases (exact versions + archive checksums; fetch-if-missing into
# TOOLS_DIR, never into system paths, never touching user-installed copies).
PIN_JUICEFS_VER="1.4.0"
PIN_JUICEFS_SHA256="6dedd730487e7dac1b11c5801682a89692f2e6b97890baf7ac943407500b85ab"
# v1.4.0 release binaries wedge at mount on this kernel class (their
# ensureFuseDev holds /dev/fuse open — upstream one-liner ca2aef0 / PR #7252,
# post-1.4.0): the pin is v1.4.0 + that cherry-pick, source-built. The
# patched-binary sha256 below is preferred when present in TOOLS_DIR
# (juicefs-1.4.0-p1/, with ca2aef0.patch alongside for provenance); the
# release tarball is the fallback where the kernel is unaffected.
PIN_JUICEFS_P1_SHA256="c20be65f0380af4b4502f1c06306738880161a2cbcc55557a79c059dba99f70a"
PIN_SEAWEEDFS_VER="4.39"
PIN_SEAWEEDFS_MD5="5dc4acbfb3111a6c18a4ebf5eb012753" # vendor-published .md5
PIN_GEESEFS_VER="0.43.8"
PIN_GEESEFS_SHA256="81dd5a9035669ec4bdecf1f54bf6368ecad66258700e2f99e722770c71e5e7f4"
PIN_MPS3_VER="1.22.3"
PIN_MPS3_SHA256="54a7e2e22308dd33d467f5fb6d361cc09c7b3eae32b38ccdbca8163ca7c6dd1f"
PIN_RUSTFS_VER="1.0.0-beta.10" # no stable RustFS release exists (2026-07); newest beta, recorded
PIN_RUSTFS_SHA256="2bea1080165ada57984c4fc4a80b149e4a6f096c57e3fd56ef89c30c703e743e"
PIN_ELBENCHO_VER="3.1-9"
PIN_ELBENCHO_SHA256="beda2921a0d4b158c1733c9d433e459d64721cf47ab4cb6d090f6d7ea1a1b3b4"

# Dedicated port slice (see header). All localhost-bound.
PORT_REDIS=$((PORT_BASE + 11))
PORT_WEED_MASTER=$((PORT_BASE + 21))
PORT_WEED_VOLUME=$((PORT_BASE + 22))
PORT_WEED_FILER=$((PORT_BASE + 23))
PORT_RUSTFS=$((PORT_BASE + 31))

# Local-only S3 credentials for the scoreboard's own RustFS instance.
S3_AK="sbscore"
S3_SK="sbscore-secret-1"

if [ "$SMOKE" = "1" ]; then
    DATASET_GB="$(env2 DATASET_GB 1)"
    TREE_DIRS="$(env2 TREE_DIRS 2)"
    TREE_FILES="$(env2 TREE_FILES 64)"
    TIMELIMIT="$(env2 TIMELIMIT 5)"
    QUIET_POLLS="$(env2 QUIET_POLLS 1)"
    QUIET_SECS="$(env2 QUIET_SECS 1)"
    ROW_TIMEOUT="$(env2 ROW_TIMEOUT 300)"
else
    DATASET_GB="$(env2 DATASET_GB 16)"
    TREE_DIRS="$(env2 TREE_DIRS 8)"
    TREE_FILES="$(env2 TREE_FILES 1024)"
    TIMELIMIT="$(env2 TIMELIMIT 30)"
    QUIET_POLLS="$(env2 QUIET_POLLS 3)"
    QUIET_SECS="$(env2 QUIET_SECS 5)"
    # A wedged mount must FAIL the row (rc=124 -> INVALID -> gate), not hang.
    ROW_TIMEOUT="$(env2 ROW_TIMEOUT 1200)"
fi

FILE_MB=$((DATASET_GB * 1024 / 16))           # per-file size, 16 files
DATA_VOL_GB=$(((DATASET_GB * 2 + 7) / 4 + 2)) # per sqz data volume (4 volumes)

ONLINE_CPUS="$(nproc)"
CPUSET="$(env2 CPUSET "__auto__")"
if [ "$CPUSET" = "__auto__" ]; then
    if [ "$ONLINE_CPUS" -ge 20 ]; then
        CPUSET="0-15" # house rails: daemon + driver contend on one pinned set
    else
        CPUSET=""
    fi
fi

TS="$(date -u +%Y%m%dT%H%M%SZ)"
ART="$(env2 OUT_DIR "$SUBSTRATE/artifacts/$TS")"
ROWS_TSV="$ART/rawrows.tsv"
CAPS_TSV="$ART/capabilities.tsv"

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
WEED_BIN=""
GEESEFS_BIN=""
MPS3_BIN=""
RUSTFS_BIN=""
declare -A SYS_FAIL=()    # sys -> reason (setup failed after 3 attempts)
declare -A NS_OVERRIDE=() # "sys.wl" -> 1 (declared N/S but probe succeeded)
declare -A CAPS_DONE=()   # sys -> 1 (capability probes recorded)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
log() { echo "[scoreboard] $*"; }
die() {
    echo "[scoreboard] FATAL: $*" >&2
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
    /mnt/juicefs*) die "substrate under /mnt/juicefs is forbidden" ;;
    "$HOME/tmp/nvme"* | /home/*/tmp/nvme*) die "substrate under ~/tmp/nvme is forbidden" ;;
    esac
}

# Substrate honesty: every durable store must sit on the same non-tmpfs
# device class. tmpfs would turn "durable store" into a RAM benchmark.
check_substrate() {
    mkdir -p "$SUBSTRATE" || die "cannot create $SUBSTRATE"
    local fstype
    fstype="$(df --output=fstype "$SUBSTRATE" 2>/dev/null | tail -1 | tr -d ' ')"
    if [ "$fstype" = "tmpfs" ] || [ "$fstype" = "ramfs" ]; then
        die "substrate $SUBSTRATE is $fstype — matched-substrate protocol requires a durable device (set SQUEEZEFS_SB_SUBSTRATE_DIR)"
    fi
    SUB_FSTYPE="$fstype"
    SUB_SRC="$(df --output=source "$SUBSTRATE" 2>/dev/null | tail -1 | tr -d ' ')"
    local part
    part="$(basename "$SUB_SRC")"
    DISK="$(lsblk -no pkname "$SUB_SRC" 2>/dev/null | head -1)"
    [ -z "$DISK" ] && DISK="$part"
    if ! grep -q " $DISK " /proc/diskstats; then
        log "WARN: no /proc/diskstats row for '$DISK' — device evidence disabled"
        DISK=""
    fi
}

# The scoreboard's port slice must be free before anything starts — a busy
# port means a foreign tenant we must not disturb.
ports_preflight() {
    local p busy=""
    for p in "$PORT_REDIS" "$PORT_WEED_MASTER" "$PORT_WEED_VOLUME" \
        "$PORT_WEED_FILER" $((PORT_WEED_MASTER + 10000)) \
        $((PORT_WEED_VOLUME + 10000)) $((PORT_WEED_FILER + 10000)) \
        "$PORT_RUSTFS"; do
        if ss -tln 2>/dev/null | awk '{print $4}' | grep -qE "[:.]${p}\$"; then
            busy="$busy $p"
        fi
    done
    [ -z "$busy" ] || die "scoreboard port(s)$busy already in use (foreign tenant) — free them or set SQUEEZEFS_SB_PORT_BASE"
}

sha256_ok() { # <file> <expected>
    echo "$2  $1" | sha256sum -c --quiet - >/dev/null 2>&1
}

fetch_url() { # <url> <dest>
    curl -fsSL --retry 2 --max-time 600 -o "$2" "$1"
}

# Pinned-tool resolution: TOOLS_DIR only (fetch-if-missing + checksum). A
# system-installed binary is used ONLY when the download is impossible AND
# its --version output matches the pin exactly (recorded in provenance).
tool_note() { echo "$*" >>"$ART/logs/tools.log"; }

resolve_elbencho() {
    local dir="$TOOLS_DIR/elbencho-$PIN_ELBENCHO_VER" tgz="$TOOLS_DIR/elbencho-static-x86_64-$PIN_ELBENCHO_VER.tar.gz"
    if [ ! -x "$dir/elbencho" ]; then
        mkdir -p "$dir"
        if [ ! -f "$tgz" ]; then
            fetch_url "https://github.com/breuner/elbencho/releases/download/v${PIN_ELBENCHO_VER}/elbencho-static-x86_64.tar.gz" "$tgz" || true
        fi
        if [ -f "$tgz" ] && sha256_ok "$tgz" "$PIN_ELBENCHO_SHA256"; then
            tar -xzf "$tgz" -C "$dir" && chmod +x "$dir/elbencho"
        fi
    fi
    if [ -x "$dir/elbencho" ]; then
        ELBENCHO_BIN="$dir/elbencho"
    elif command -v elbencho >/dev/null 2>&1 &&
        elbencho --version 2>/dev/null | grep -q "Version: *$PIN_ELBENCHO_VER"; then
        ELBENCHO_BIN="$(command -v elbencho)"
        tool_note "elbencho: using system binary (version-matched $PIN_ELBENCHO_VER; download unavailable)"
    else
        die "elbencho $PIN_ELBENCHO_VER unavailable (download failed, no version-matched system binary)"
    fi
}

resolve_juicefs() {
    local p1="$TOOLS_DIR/juicefs-$PIN_JUICEFS_VER-p1/juicefs"
    if [ -x "$p1" ] && sha256_ok "$p1" "$PIN_JUICEFS_P1_SHA256"; then
        JUICEFS_BIN="$p1"
        tool_note "juicefs: v$PIN_JUICEFS_VER + ca2aef0 source build (kernel mount-hang fix)"
        return 0
    fi
    local dir="$TOOLS_DIR/juicefs-$PIN_JUICEFS_VER" tgz="$TOOLS_DIR/juicefs-$PIN_JUICEFS_VER-linux-amd64.tar.gz"
    if [ ! -x "$dir/juicefs" ]; then
        mkdir -p "$dir"
        if [ ! -f "$tgz" ]; then
            fetch_url "https://github.com/juicedata/juicefs/releases/download/v${PIN_JUICEFS_VER}/juicefs-${PIN_JUICEFS_VER}-linux-amd64.tar.gz" "$tgz" || true
        fi
        if [ -f "$tgz" ] && sha256_ok "$tgz" "$PIN_JUICEFS_SHA256"; then
            tar -xzf "$tgz" -C "$dir" juicefs && chmod +x "$dir/juicefs"
        fi
    fi
    [ -x "$dir/juicefs" ] && JUICEFS_BIN="$dir/juicefs"
}

resolve_seaweedfs() {
    local dir="$TOOLS_DIR/seaweedfs-$PIN_SEAWEEDFS_VER" tgz="$TOOLS_DIR/seaweedfs-$PIN_SEAWEEDFS_VER.tar.gz"
    if [ ! -x "$dir/weed" ]; then
        mkdir -p "$dir"
        if [ ! -f "$tgz" ]; then
            fetch_url "https://github.com/seaweedfs/seaweedfs/releases/download/${PIN_SEAWEEDFS_VER}/linux_amd64.tar.gz" "$tgz" || true
        fi
        if [ -f "$tgz" ] && echo "$PIN_SEAWEEDFS_MD5  $tgz" | md5sum -c --quiet - >/dev/null 2>&1; then
            tar -xzf "$tgz" -C "$dir" && chmod +x "$dir/weed"
        fi
    fi
    [ -x "$dir/weed" ] && WEED_BIN="$dir/weed"
}

resolve_geesefs() {
    local bin="$TOOLS_DIR/geesefs-$PIN_GEESEFS_VER"
    if [ ! -x "$bin" ]; then
        fetch_url "https://github.com/yandex-cloud/geesefs/releases/download/v${PIN_GEESEFS_VER}/geesefs-linux-amd64" "$bin.tmp" || true
        if [ -f "$bin.tmp" ] && sha256_ok "$bin.tmp" "$PIN_GEESEFS_SHA256"; then
            mv "$bin.tmp" "$bin" && chmod +x "$bin"
        else
            rm -f "$bin.tmp"
        fi
    fi
    [ -x "$bin" ] && GEESEFS_BIN="$bin"
}

resolve_mps3() {
    local dir="$TOOLS_DIR/mount-s3-$PIN_MPS3_VER" tgz="$TOOLS_DIR/mount-s3-$PIN_MPS3_VER.tar.gz"
    if [ ! -x "$dir/bin/mount-s3" ]; then
        mkdir -p "$dir"
        if [ ! -f "$tgz" ]; then
            fetch_url "https://s3.amazonaws.com/mountpoint-s3-release/${PIN_MPS3_VER}/x86_64/mount-s3-${PIN_MPS3_VER}-x86_64.tar.gz" "$tgz" || true
        fi
        if [ -f "$tgz" ] && sha256_ok "$tgz" "$PIN_MPS3_SHA256"; then
            tar -xzf "$tgz" -C "$dir" && chmod +x "$dir/bin/mount-s3"
        fi
    fi
    [ -x "$dir/bin/mount-s3" ] && MPS3_BIN="$dir/bin/mount-s3"
}

resolve_rustfs() {
    local dir="$TOOLS_DIR/rustfs-$PIN_RUSTFS_VER" zip="$TOOLS_DIR/rustfs-$PIN_RUSTFS_VER.zip"
    if [ ! -x "$dir/rustfs" ]; then
        mkdir -p "$dir"
        if [ ! -f "$zip" ]; then
            fetch_url "https://github.com/rustfs/rustfs/releases/download/${PIN_RUSTFS_VER}/rustfs-linux-x86_64-gnu-v${PIN_RUSTFS_VER}.zip" "$zip" || true
        fi
        if [ -f "$zip" ] && sha256_ok "$zip" "$PIN_RUSTFS_SHA256"; then
            python3 -c "import zipfile,sys; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" "$zip" "$dir" &&
                chmod +x "$dir/rustfs"
        fi
    fi
    [ -x "$dir/rustfs" ] && RUSTFS_BIN="$dir/rustfs"
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
            --unit "sqzsb-${suffix}-$$-$(date +%s%N)"
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

# Co-tenant honesty. Called only while OUR elbencho is not running
# (pre-gate / post-row), so any elbencho match is foreign.
cotenants() {
    local out=""
    pgrep -x rustc >/dev/null 2>&1 && out="${out}rustc,"
    pgrep -x cargo >/dev/null 2>&1 && out="${out}cargo,"
    pgrep -f pytest >/dev/null 2>&1 && out="${out}pytest,"
    pgrep -x elbencho >/dev/null 2>&1 && out="${out}foreign-elbencho,"
    echo "$out"
}

# House 3-poll quiet gate: QUIET_POLLS consecutive polls, QUIET_SECS apart,
# each requiring no co-tenants and Tctl < 80. load1 is RECORDED in the
# honesty line but only gates at session start. Never blocks forever: after
# ~5 min the row proceeds flagged DIRTY(gate).
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
# timed row. Proceeds DIRTY after ~5 min.
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

wait_mounted() { # <mnt> <logf> [expect_grep] -> 0 when mounted
    local mnt="$1" logf="$2" expect="${3:-}" i
    for i in $(seq 1 200); do
        if mountpoint -q "$mnt" 2>/dev/null; then
            [ -z "$expect" ] && return 0
            grep -q "$expect" "$logf" 2>/dev/null && return 0
        fi
        sleep 0.3
    done
    mountpoint -q "$mnt" 2>/dev/null
}

fuse_unmount() { # <mnt> — polite then lazy
    local mnt="$1" i
    mountpoint -q "$mnt" 2>/dev/null || {
        # Stale ENOTCONN attachment: detach lazily so the grid keeps filling.
        if [ -d "$(dirname "$mnt")" ] && ! stat "$mnt" >/dev/null 2>&1; then
            fusermount3 -uz "$mnt" 2>/dev/null || umount -l "$mnt" 2>/dev/null || true
        fi
        return 0
    }
    fusermount3 -u "$mnt" 2>/dev/null || umount "$mnt" 2>/dev/null || true
    for i in $(seq 1 100); do
        mountpoint -q "$mnt" 2>/dev/null || return 0
        sleep 0.2
    done
    fusermount3 -uz "$mnt" 2>/dev/null || umount -l "$mnt" 2>/dev/null || true
    sleep 0.5
    return 0
}

kill_pid_wait() { # <pid> <name> — TERM, wait, KILL as last resort
    local pid="$1" name="$2" i
    [ -n "$pid" ] && [ -d "/proc/$pid" ] || return 0
    kill "$pid" 2>/dev/null || true
    for i in $(seq 1 100); do
        [ -d "/proc/$pid" ] || return 0
        sleep 0.2
    done
    log "WARN: $name pid $pid still alive — SIGKILL by PID"
    kill -9 "$pid" 2>/dev/null || true
}

capture_pid_retry() { # <sys> -> pid on stdout (may be empty); never fails
    # The daemon pid can be legitimately unfindable for a moment around the
    # daemonization double-fork/exec window — a missing pid must degrade
    # observability (snapshots skip), never fail the mount (smoke finding:
    # the bare trailing assignment leaked pgrep's rc as the mount verdict).
    local i pid=""
    for i in $(seq 1 10); do
        pid="$(sys_pid "$1" 2>/dev/null || true)"
        [ -n "$pid" ] && break
        sleep 0.5
    done
    echo "$pid"
}

# ---------------------------------------------------------------------------
# System: SqueezeFS (4 meta + 4 data file-backed volumes on the substrate)
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
        die "squeezefs mount failed (see $logf)" # sqz failure always fatal — never weaken the sqz side
    }
    SQZ_PID="$(capture_pid_retry sqz)"
    grep -m1 "FUSE-over-io_uring registered" "$logf" || true
}

sqz_umount() {
    [ -d "$SQZ_MNT" ] || return 0
    "$SQZ_BIN" umount "$SQZ_MNT" >/dev/null 2>&1 || true
    fuse_unmount "$SQZ_MNT"
    local i
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
# System: JuiceFS (meta engine + file:// object store + cache, same substrate)
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
            mkdir -p "$JFS_DIR/redis"
            # Production-honest persistence (their recommended AOF posture)
            # on the SAME substrate as the SqueezeFS meta volumes. Dedicated
            # port from the scoreboard slice — never the user's redis.
            redis-server --port "$PORT_REDIS" --bind 127.0.0.1 \
                --dir "$JFS_DIR/redis" --appendonly yes --appendfsync everysec \
                --save '' --daemonize no >"$ART/logs/redis.log" 2>&1 &
            REDIS_PID=$!
            sleep 1
            kill -0 "$REDIS_PID" 2>/dev/null || die "redis-server failed to start"
        fi
        redis-cli -p "$PORT_REDIS" flushall >/dev/null 2>&1 || true
        JFS_META="redis://127.0.0.1:$PORT_REDIS/1"
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
        --trash-days 0 "$JFS_META" sqzsb \
        >"$ART/logs/jfs_format_$1.log" 2>&1 || return 1
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
        --metrics "127.0.0.1:0" \
        "${extra[@]}" \
        --log "$logf" \
        "$JFS_META" "$JFS_MNT" >>"$logf" 2>&1
    wait_mounted "$JFS_MNT" "$logf" || {
        tail -5 "$logf" >&2
        return 1
    }
    JFS_PID="$(capture_pid_retry jfs)"
    return 0
}

jfs_umount() {
    [ -n "$JUICEFS_BIN" ] && [ -d "$JFS_MNT" ] || return 0
    if mountpoint -q "$JFS_MNT" 2>/dev/null; then
        "$JUICEFS_BIN" umount "$JFS_MNT" >/dev/null 2>&1 || true
        local i
        for i in $(seq 1 150); do
            mountpoint -q "$JFS_MNT" || break
            sleep 0.2
        done
        mountpoint -q "$JFS_MNT" &&
            "$JUICEFS_BIN" umount --force "$JFS_MNT" >/dev/null 2>&1
    fi
    fuse_unmount "$JFS_MNT"
    local i
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
# System: SeaweedFS native stack (weed server: master+volume+filer; weed
# mount: FUSE). Fresh stores per regime-run; own port slice; local sockets
# under the substrate (never /tmp residue).
# ---------------------------------------------------------------------------
SWFS_DIR="$SUBSTRATE/swfs"
SWFS_MNT="$SUBSTRATE/swfs_mnt"
SWFS_CACHE="$SWFS_DIR/mount_cache"
SWFS_SRV_PID=""
SWFS_MNT_PID=""

swfs_server_start() { # <tag> <cage_mb>
    local tag="$1" memmax="$2" i
    local logf="$ART/logs/swfs_server_${tag}.log"
    rm -rf "$SWFS_DIR"
    mkdir -p "$SWFS_DIR/data" "$SWFS_CACHE" "$SWFS_MNT"
    cage_cmd "$memmax" "swfs-srv-$tag"
    pin_cmd
    (cd "$SWFS_DIR" && "${CAGE_ARGV[@]}" "${PIN_ARGV[@]}" nohup "$WEED_BIN" server \
        -dir="$SWFS_DIR/data" -ip=127.0.0.1 -master.peers=none \
        -master.port="$PORT_WEED_MASTER" -volume.port="$PORT_WEED_VOLUME" \
        -filer -filer.port="$PORT_WEED_FILER" \
        >"$logf" 2>&1 &)
    for i in $(seq 1 200); do
        SWFS_SRV_PID="$(pgrep -f "weed server.*$SWFS_DIR" | head -1)"
        # Readiness = a WRITABLE volume can be assigned (smoke finding: the
        # filer comes up seconds before volume allocation; a write burst in
        # the gap dies with "No writable volumes" and 0-size files).
        if [ -n "$SWFS_SRV_PID" ] &&
            curl -fsS --max-time 2 "http://127.0.0.1:$PORT_WEED_MASTER/dir/assign" 2>/dev/null |
            grep -q '"fid"'; then
            return 0
        fi
        sleep 0.3
    done
    tail -5 "$logf" >&2
    return 1
}

swfs_server_stop() {
    if [ -n "$SWFS_SRV_PID" ] && [ -d "/proc/$SWFS_SRV_PID" ]; then
        kill -INT "$SWFS_SRV_PID" 2>/dev/null || true # weed handles INT, shrugs at TERM
        sleep 1
    fi
    kill_pid_wait "$SWFS_SRV_PID" "weed server"
    SWFS_SRV_PID=""
}

swfs_mount() { # <tag> <cage_mb> <cache_capacity_mb>
    local tag="$1" memmax="$2" cache_mb="$3" i
    local logf="$ART/logs/swfs_mount_${tag}.log"
    cage_cmd "$memmax" "swfs-mnt-$tag"
    pin_cmd
    "${CAGE_ARGV[@]}" "${PIN_ARGV[@]}" nohup "$WEED_BIN" mount \
        -filer="127.0.0.1:$PORT_WEED_FILER" -dir="$SWFS_MNT" \
        -cacheDir="$SWFS_CACHE" -cacheCapacityMB="$cache_mb" \
        -localSocket="$SWFS_DIR/mount.sock" \
        >"$logf" 2>&1 &
    for i in $(seq 1 200); do
        mountpoint -q "$SWFS_MNT" 2>/dev/null && break
        sleep 0.3
    done
    mountpoint -q "$SWFS_MNT" || {
        tail -5 "$logf" >&2
        return 1
    }
    SWFS_MNT_PID="$(capture_pid_retry swfs)"
    return 0
}

swfs_umount() {
    fuse_unmount "$SWFS_MNT"
    kill_pid_wait "$SWFS_MNT_PID" "weed mount"
    SWFS_MNT_PID=""
}

# ---------------------------------------------------------------------------
# Shared S3 backend for gee + mps3: one local RustFS instance (object store
# held constant so those two rows differ only by client). Fresh store per
# system-run; bucket created via curl SigV4 (no external S3 CLI needed).
# ---------------------------------------------------------------------------
RUSTFS_DIR="$SUBSTRATE/rustfs"
RUSTFS_PID=""

rustfs_start() { # <tag> <cage_mb>
    local tag="$1" memmax="$2" i
    local logf="$ART/logs/rustfs_${tag}.log"
    rm -rf "$RUSTFS_DIR"
    mkdir -p "$RUSTFS_DIR/data"
    cage_cmd "$memmax" "rustfs-$tag"
    pin_cmd
    RUSTFS_ACCESS_KEY="$S3_AK" RUSTFS_SECRET_KEY="$S3_SK" \
        "${CAGE_ARGV[@]}" "${PIN_ARGV[@]}" nohup "$RUSTFS_BIN" server \
        --address "127.0.0.1:$PORT_RUSTFS" "$RUSTFS_DIR/data" \
        >"$logf" 2>&1 &
    for i in $(seq 1 100); do
        RUSTFS_PID="$(pgrep -f "rustfs server.*$RUSTFS_DIR" | head -1)"
        if [ -n "$RUSTFS_PID" ] &&
            curl -fsS -o /dev/null --max-time 2 "http://127.0.0.1:$PORT_RUSTFS/" 2>/dev/null; then
            return 0
        fi
        # rustfs returns 403 on / without auth — probe via sigv4 list instead
        if [ -n "$RUSTFS_PID" ] &&
            curl -sS -o /dev/null --max-time 2 --aws-sigv4 "aws:amz:us-east-1:s3" \
                --user "$S3_AK:$S3_SK" "http://127.0.0.1:$PORT_RUSTFS/" 2>/dev/null; then
            return 0
        fi
        sleep 0.3
    done
    tail -5 "$logf" >&2
    return 1
}

rustfs_stop() {
    kill_pid_wait "$RUSTFS_PID" "rustfs"
    RUSTFS_PID=""
}

s3_make_bucket() { # <bucket> — retried: a fresh RustFS can 5xx briefly
    local code i
    for i in $(seq 1 20); do
        code="$(curl -sS -o /dev/null -w "%{http_code}" --max-time 15 -X PUT \
            "http://127.0.0.1:$PORT_RUSTFS/$1" \
            --aws-sigv4 "aws:amz:us-east-1:s3" --user "$S3_AK:$S3_SK" 2>/dev/null)"
        if [ "$code" = "200" ] || [ "$code" = "409" ]; then
            return 0
        fi
        sleep 0.5
    done
    log "WARN: bucket $1 creation failed after retries (last http=$code)"
    return 1
}

# ---------------------------------------------------------------------------
# System: geesefs (S3-backed FUSE client over RustFS)
# ---------------------------------------------------------------------------
GEE_MNT="$SUBSTRATE/gee_mnt"
GEE_BUCKET="gee-bench"
GEE_PID=""

gee_mount() { # <tag> <cage_mb> <memory_limit_mb>
    local tag="$1" memmax="$2" memlim="$3"
    local logf="$ART/logs/gee_mount_${tag}.log"
    mkdir -p "$GEE_MNT"
    cage_cmd "$memmax" "gee-$tag"
    pin_cmd
    # shellcheck disable=SC2094 # --log-file is a path arg, not a read
    AWS_ACCESS_KEY_ID="$S3_AK" AWS_SECRET_ACCESS_KEY="$S3_SK" \
        "${CAGE_ARGV[@]}" "${PIN_ARGV[@]}" "$GEESEFS_BIN" \
        --endpoint "http://127.0.0.1:$PORT_RUSTFS" \
        --memory-limit "$memlim" \
        --log-file "$logf" \
        "$GEE_BUCKET" "$GEE_MNT" >>"$logf" 2>&1
    wait_mounted "$GEE_MNT" "$logf" || {
        tail -5 "$logf" >&2
        return 1
    }
    GEE_PID="$(capture_pid_retry gee)"
    return 0
}

gee_umount() {
    fuse_unmount "$GEE_MNT"
    kill_pid_wait "$GEE_PID" "geesefs"
    GEE_PID=""
}

# ---------------------------------------------------------------------------
# System: mountpoint-s3 (S3-backed FUSE client over the SAME RustFS)
# ---------------------------------------------------------------------------
MPS3_MNT="$SUBSTRATE/mps3_mnt"
MPS3_CACHE="$SUBSTRATE/mps3_cache"
MPS3_BUCKET="mps3-bench"
MPS3_PID=""

mps3_mount() { # <tag> <cage_mb> <cache_mb|0=no cache>
    local tag="$1" memmax="$2" cache_mb="$3"
    local logf="$ART/logs/mps3_mount_${tag}.log"
    mkdir -p "$MPS3_MNT"
    local extra=()
    if [ "$cache_mb" != "0" ]; then
        rm -rf "$MPS3_CACHE"
        mkdir -p "$MPS3_CACHE"
        extra+=(--cache "$MPS3_CACHE" --max-cache-size "$cache_mb")
    fi
    cage_cmd "$memmax" "mps3-$tag"
    pin_cmd
    # --allow-delete/--allow-overwrite: documented opt-ins the grid needs
    # (del rows; fresh-file rewrites). Recorded in provenance.
    AWS_ACCESS_KEY_ID="$S3_AK" AWS_SECRET_ACCESS_KEY="$S3_SK" \
        "${CAGE_ARGV[@]}" "${PIN_ARGV[@]}" nohup "$MPS3_BIN" \
        "$MPS3_BUCKET" "$MPS3_MNT" --foreground \
        --endpoint-url "http://127.0.0.1:$PORT_RUSTFS" --force-path-style \
        --region us-east-1 --allow-delete --allow-overwrite \
        "${extra[@]}" >"$logf" 2>&1 &
    wait_mounted "$MPS3_MNT" "$logf" || {
        tail -5 "$logf" >&2
        return 1
    }
    MPS3_PID="$(capture_pid_retry mps3)"
    return 0
}

mps3_umount() {
    fuse_unmount "$MPS3_MNT"
    kill_pid_wait "$MPS3_PID" "mount-s3"
    MPS3_PID=""
}

# ---------------------------------------------------------------------------
# Capability matrix (declarative) + empirical N/S verification
# ---------------------------------------------------------------------------
ns_reason() { # <sys> <wl> -> one-line reason (empty = supported)
    case "$1.$2" in
    mps3.rand_write_4k)
        echo "sequential-upload semantics: no random/out-of-order writes by design"
        ;;
    *) echo "" ;;
    esac
}

is_ns() { # <sys> <wl> -> 0 if the cell is N/S (declared and not overridden)
    [ -n "$(ns_reason "$1" "$2")" ] && [ -z "${NS_OVERRIDE[$1.$2]:-}" ]
}

# Empirically verify every declared-N/S op refuses on the live mount; record
# the refusal errno (never guess). A probe that SUCCEEDS flags matrix rot
# loudly and un-declares the cell for this run.
verify_capabilities() { # <sys> <mnt>
    local sys="$1" mnt="$2" wl reason out
    [ -n "${CAPS_DONE[$sys]:-}" ] && return 0
    CAPS_DONE[$sys]=1
    for wl in $WORKLOADS; do
        reason="$(ns_reason "$sys" "$wl")"
        [ -z "$reason" ] && continue
        case "$wl" in
        rand_write_4k)
            out="$(python3 - "$mnt" <<'EOF'
import errno, os, sys
mnt = sys.argv[1]
probe = os.path.join(mnt, ".sb_caps_probe")
try:
    fd = os.open(probe, os.O_CREAT | os.O_WRONLY, 0o644)
    os.write(fd, b"seed" * 4096)
    os.close(fd)
except OSError as e:
    print(f"probe-setup-failed({errno.errorcode.get(e.errno, e.errno)})")
    sys.exit(0)
try:
    fd = os.open(probe, os.O_RDWR)
    try:
        os.lseek(fd, 4096, os.SEEK_SET)
        os.write(fd, b"x" * 4096)
        print("SUCCEEDED")
    finally:
        os.close(fd)
except OSError as e:
    print(f"refused({errno.errorcode.get(e.errno, e.errno)})")
try:
    os.unlink(probe)
except OSError:
    pass
EOF
)"
            ;;
        *) out="not-probed" ;;
        esac
        printf '%s\t%s\t%s\t%s\n' "$sys" "$wl" "$reason" "$out" >>"$CAPS_TSV"
        if [ "$out" = "SUCCEEDED" ]; then
            log "WARN: capability-matrix rot: $sys.$wl declared N/S but the op SUCCEEDED — running the row"
            NS_OVERRIDE["$sys.$wl"]=1
        else
            log "caps: $sys.$wl N/S verified ($out) — $reason"
        fi
    done
}

emit_ns_row() { # <regime> <wl> <sys> <mode>
    local rowid="${1}.${2}"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$rowid" "$1" "$2" "$4" "$3" "-" "NS" "0" "quiet" "-" \
        "$(ns_reason "$3" "$2" | tr ' \t' ';;')" >>"$ROWS_TSV"
}

emit_na_row() { # <regime> <wl> <sys> <mode> <reason>
    local rowid="${1}.${2}"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$rowid" "$1" "$2" "$4" "$3" "-" "NA" "254" "SETUP-FAILED" "-" \
        "$(echo "$5" | tr ' \t' ';;')" >>"$ROWS_TSV"
}

# ---------------------------------------------------------------------------
# Row machinery (snapshots + honesty lines per row) + RW6 durability pass
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

# RW6 durability pass — see header. Runs IMMEDIATELY after the write phase
# (no quiet gate in between); prints total seconds + component split.
durability_pass() { # <mnt> <pfx> -> echoes elapsed seconds ("NA" on failure)
    local mnt="$1" pfx="$2"
    python3 - "$mnt" "$SUBSTRATE" "${DATA_FILES[@]}" <<'EOF' >"$pfx.durable" 2>&1
import ctypes, os, sys, time
mnt, substrate, files = sys.argv[1], sys.argv[2], sys.argv[3:]
libc = ctypes.CDLL("libc.so.6", use_errno=True)
t0 = time.monotonic()
fsynced = skipped = 0
for f in files:
    fd = None
    try:
        fd = os.open(f, os.O_RDWR)
    except OSError:
        try:
            fd = os.open(f, os.O_RDONLY)  # mount-s3: upload completed at close
        except OSError:
            skipped += 1
            continue
    try:
        os.fdatasync(fd)
        fsynced += 1
    except OSError:
        skipped += 1
    finally:
        os.close(fd)
t1 = time.monotonic()
for d in (mnt, substrate):  # FUSE_SYNCFS where honored, then backing-store flush
    try:
        fd = os.open(d, os.O_RDONLY)
    except OSError:
        continue
    try:
        if libc.syncfs(fd) != 0:
            raise OSError(ctypes.get_errno(), "syncfs")
    finally:
        os.close(fd)
t2 = time.monotonic()
print(f"{t2 - t0:.3f} fdatasync_s={t1 - t0:.3f} syncfs_s={t2 - t1:.3f} "
      f"files_fsynced={fsynced} files_skipped={skipped}")
EOF
    awk 'NR==1{print $1; exit}' "$pfx.durable" 2>/dev/null | grep -E '^[0-9.]+$' || echo "NA"
}

csv_field() { # <csvfile> <op> <column-name> -> value or NA
    python3 - "$1" "$2" "$3" <<'EOF' 2>/dev/null || echo "NA"
import csv, sys
path, op, col = sys.argv[1], sys.argv[2], sys.argv[3]
val = None
with open(path, newline="") as f:
    for row in csv.DictReader(f):
        if row.get("operation", "").strip().upper() == op.upper():
            val = row.get(col, "").strip()
print(val if val else "NA")
EOF
}

# run_row <regime> <workload> <system> <mnt> <pid> <op> <key> <unit> <block_kib> -- <elbencho args...>
# block_kib: block size for durable-ops math on IOPS rows (0 = seq MiB/s row;
# -1 = non-write row, no durable mode).
run_row() {
    local regime="$1" wl="$2" sys="$3" mnt="$4" pid="$5" op="$6" key="$7" unit="$8" block_kib="$9"
    shift 9
    [ "$1" = "--" ] && shift
    local rowid="${regime}.${wl}" pfx="$ART/rows/${regime}.${wl}.${sys}"
    mkdir -p "$ART/rows"

    quiet_gate
    local tctl load
    tctl="$(tctl_read)"
    load="$(cut -d' ' -f1 /proc/loadavg)"

    snap "$pfx" before "$mnt" "$pid"
    pin_cmd
    rm -f "$pfx.csv"
    timeout -k 10 "$ROW_TIMEOUT" "${PIN_ARGV[@]}" "$ELBENCHO_BIN" \
        --csvfile "$pfx.csv" --label "$rowid.$sys" "$@" \
        >"$pfx.elbencho" 2>&1
    local rc=$?

    # RW6: durability pass rides IMMEDIATELY after the write phase.
    local t_dur="" t_write_ms="NA" tot_mib="NA"
    if [ "$block_kib" != "-1" ]; then
        t_dur="$(durability_pass "$mnt" "$pfx")"
        t_write_ms="$(csv_field "$pfx.csv" "$op" "time ms [last]")"
        tot_mib="$(csv_field "$pfx.csv" "$op" "MiB [last]")"
    fi
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

    # Durable value: bytes (or ops) over write-elapsed + durability-pass.
    local durable_value="NA" durable_note=""
    if [ "$block_kib" != "-1" ]; then
        durable_value="$(python3 - "$t_write_ms" "$tot_mib" "$t_dur" "$block_kib" "$rc" <<'EOF'
import sys
t_ms, mib, t_dur, blk_kib, rc = sys.argv[1:6]
try:
    if rc != "0":
        raise ValueError
    total_s = float(t_ms) / 1000.0 + float(t_dur)
    mibf = float(mib)
    blk = int(blk_kib)
    if total_s <= 0:
        raise ValueError
    if blk > 0:  # IOPS row: ops = MiB * 1024 / block_kib
        print(f"{mibf * 1024.0 / blk / total_s:.0f}")
    else:  # seq row: MiB/s
        print(f"{mibf / total_s:.1f}")
except (ValueError, ZeroDivisionError):
    print("NA")
EOF
)"
        durable_note="t_write_ms=$t_write_ms;t_dur_s=$t_dur;mib=$tot_mib"
    fi

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

    # Serve-evidence + device-true verification per system. sqz/jfs keep
    # their counter proofs; objstore-backed refs use device-byte evidence.
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
elif sysname == "jfs":
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
else:
    # objstore/filer-backed refs: no portable counters — device bytes are
    # the evidence (substrate diskstats over the row).
    msgs.append(f"dev_read_MiB={dev_read_mib:.0f} user_MiB={user_mib:.0f}")
    if regime == "R2" and "read" in wl:
        ok = user_mib > 0 and dev_read_mib >= 0.5 * user_mib
        msgs.append("VERIFY=device-true-OK" if ok else "VERIFY=FAILED-cache-or-pagecache-serve")
print(" ".join(msgs))
EOF
)"

    {
        echo "rowid=$rowid sys=$sys rc=$rc value=$value unit=$unit total_mib=$totmib elapsed=${elapsed}s durable=$durable_value $durable_note"
        echo "honesty: tctl=${tctl:-na}C load=$load $dirty"
        [ -n "$devline" ] && echo "device: $devline"
        echo "serve: $verify"
    } | tee "$pfx.env"

    # Relaxed row (native ACK semantics — labeled, non-gating for writes).
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$rowid" "$regime" "$wl" "relaxed" "$sys" "$unit" "$value" "$rc" "$dirty" \
        "$(echo "$devline" | tr ' ' ';')" "$(echo "$verify" | tr ' \t' ';;')" \
        >>"$ROWS_TSV"
    # Durable row (fsync-inclusive; governs write-family verdicts).
    if [ "$block_kib" != "-1" ]; then
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$rowid" "$regime" "$wl" "durable" "$sys" "$unit" "$durable_value" "$rc" "$dirty" \
            "$(echo "$devline" | tr ' ' ';')" "$(echo "$durable_note" | tr ' \t' ';;')" \
            >>"$ROWS_TSV"
    fi

    [ "$rc" -ne 0 ] && log "WARN: elbencho rc=$rc on $rowid.$sys (row recorded as-is)"
    return 0
}

# ---------------------------------------------------------------------------
# Workload grid
# ---------------------------------------------------------------------------
dataset_files() { # <mnt> -> fills DATA_FILES array
    local mnt="$1" i
    DATA_FILES=()
    for i in $(seq -w 1 16); do DATA_FILES+=("$mnt/sbdata/f$i"); done
}

ensure_dataset() { # <mnt> <sys> — untimed prep when seq_write_1m isn't in the grid
    local mnt="$1" sys="$2"
    dataset_files "$mnt"
    [ -f "${DATA_FILES[0]}" ] && return 0
    mkdir -p "$mnt/sbdata"
    log "prep: creating dataset (untimed)"
    pin_cmd
    if ! timeout -k 10 $((ROW_TIMEOUT * 2)) \
        "${PIN_ARGV[@]}" "$ELBENCHO_BIN" -w -t "$THREADS" -s "${FILE_MB}m" -b 1m \
        --direct "${DATA_FILES[@]}" >"$ART/logs/prep_dataset.$RANDOM.log" 2>&1; then
        # A reference's prep failure is that reference's row, never the run's.
        [ "$sys" = "sqz" ] && die "sqz dataset prep failed"
        log "ERROR: $sys dataset prep failed (row becomes n/a)"
        return 1
    fi
}

ensure_tree() { # <mnt> <sys> — untimed prep for stat/del storms
    local mnt="$1" sys="$2"
    [ -d "$mnt/sbtree" ] && [ -n "$(ls -A "$mnt/sbtree" 2>/dev/null)" ] && return 0
    mkdir -p "$mnt/sbtree"
    log "prep: creating tree ($((THREADS * TREE_DIRS * TREE_FILES)) files, untimed)"
    pin_cmd
    if ! timeout -k 10 $((ROW_TIMEOUT * 2)) \
        "${PIN_ARGV[@]}" "$ELBENCHO_BIN" -w -d -t "$THREADS" -n "$TREE_DIRS" \
        -N "$TREE_FILES" -s 4k "$mnt/sbtree" >"$ART/logs/prep_tree.$RANDOM.log" 2>&1; then
        [ "$sys" = "sqz" ] && die "sqz tree prep failed"
        log "ERROR: $sys tree prep failed (row becomes n/a)"
        return 1
    fi
}

prep_failed_row() { # <regime> <wl> <sys> — NA rows for a failed untimed prep
    emit_na_row "$1" "$2" "$3" "relaxed" "untimed prep failed (see $ART/logs)"
    case "$2" in seq_write_1m | rand_write_4k)
        emit_na_row "$1" "$2" "$3" "durable" "untimed prep failed (see $ART/logs)"
        ;;
    esac
}

# run_workload <regime> <workload> <system> <mnt> <pid>
run_workload() {
    local regime="$1" wl="$2" sys="$3" mnt="$4" pid="$5"
    dataset_files "$mnt"
    case "$wl" in
    seq_write_1m)
        mkdir -p "$mnt/sbdata"
        rm -f "${DATA_FILES[@]}"
        run_row "$regime" "$wl" "$sys" "$mnt" "$pid" WRITE "Throughput MiB/s" "MiB/s" 0 -- \
            -w -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct "${DATA_FILES[@]}"
        ;;
    seq_read_1m)
        ensure_dataset "$mnt" "$sys" || {
            prep_failed_row "$regime" "$wl" "$sys"
            return 0
        }
        run_row "$regime" "$wl" "$sys" "$mnt" "$pid" READ "Throughput MiB/s" "MiB/s" -1 -- \
            -r -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct "${DATA_FILES[@]}"
        ;;
    rand_read_4k) # the user's exact iodepth line
        ensure_dataset "$mnt" "$sys" || {
            prep_failed_row "$regime" "$wl" "$sys"
            return 0
        }
        run_row "$regime" "$wl" "$sys" "$mnt" "$pid" READ "IOPS" "IOPS" -1 -- \
            -r --rand -t "$THREADS" -b 4k --iodepth 16 --direct \
            --timelimit "$TIMELIMIT" "${DATA_FILES[@]}"
        ;;
    rand_write_4k)
        ensure_dataset "$mnt" "$sys" || {
            prep_failed_row "$regime" "$wl" "$sys"
            return 0
        }
        run_row "$regime" "$wl" "$sys" "$mnt" "$pid" WRITE "IOPS" "IOPS" 4 -- \
            -w --rand -t "$THREADS" -s "${FILE_MB}m" -b 4k --iodepth 16 --direct \
            --timelimit "$TIMELIMIT" "${DATA_FILES[@]}"
        ;;
    stat_storm)
        ensure_tree "$mnt" "$sys" || {
            prep_failed_row "$regime" "$wl" "$sys"
            return 0
        }
        run_row "$regime" "$wl" "$sys" "$mnt" "$pid" STAT "Files/s" "files/s" -1 -- \
            --stat -t "$THREADS" -n "$TREE_DIRS" -N "$TREE_FILES" "$mnt/sbtree"
        ;;
    del_storm)
        ensure_tree "$mnt" "$sys" || {
            prep_failed_row "$regime" "$wl" "$sys"
            return 0
        }
        run_row "$regime" "$wl" "$sys" "$mnt" "$pid" RMFILES "Files/s" "files/s" -1 -- \
            -F -D -t "$THREADS" -n "$TREE_DIRS" -N "$TREE_FILES" "$mnt/sbtree"
        ;;
    *) die "unknown workload '$wl'" ;;
    esac
}

# ---------------------------------------------------------------------------
# Regimes: per-system format/mount with matched budgets
# ---------------------------------------------------------------------------
sys_format() { # <regime> <system> <tag> -> nonzero on failure (refs only)
    local regime="$1" sys="$2" tag="$3"
    case "$sys" in
    sqz) sqz_format "$tag" ;; # dies on failure — sqz is never skipped
    jfs) jfs_format "$tag" ;;
    swfs)
        swfs_server_stop
        if [ "$regime" = "R2" ]; then
            swfs_server_start "$tag" "$R2_REF_CAGE_MB"
        else
            swfs_server_start "$tag" "$CAGE_MB"
        fi
        ;;
    gee | mps3)
        rustfs_stop
        if [ "$regime" = "R2" ]; then
            rustfs_start "$tag" "$R2_REF_CAGE_MB" || return 1
        else
            rustfs_start "$tag" "$CAGE_MB" || return 1
        fi
        if [ "$sys" = "gee" ]; then
            s3_make_bucket "$GEE_BUCKET"
        else
            s3_make_bucket "$MPS3_BUCKET"
        fi
        ;;
    *) die "unknown system '$sys'" ;;
    esac
}

mount_for_regime() { # <regime> <system> <tag>
    local regime="$1" sys="$2" tag="$3"
    case "$sys" in
    sqz)
        case "$regime" in
        R2) sqz_mount "$tag" "$CAGE_MB" -o direct_device_true ;;
        *) sqz_mount "$tag" "$CAGE_MB" ;;
        esac
        ;;
    jfs)
        case "$regime" in
        # R2: cache-size 0, cache-dir stays on the substrate, tight cage to
        # defeat the file:// object store's kernel page-cache serve (the
        # decomposition-report mechanism — JuiceFS has no device-true knob).
        # buffer-size 300 = their shipped default; larger buffers OOM-loop
        # the daemon inside the tight cage.
        R2) jfs_mount "$tag" "$R2_REF_CAGE_MB" 0 300 ;;
        *) jfs_mount "$tag" "$CAGE_MB" "$CACHE_MB" "$CACHE_MB" ;;
        esac
        ;;
    # R2 note: for the objstore/filer-backed refs the page-cache-serving
    # process is the STORE (weed server / RustFS — tight-caged in sys_format),
    # not the FUSE client; the client keeps the standard cage with its data
    # caches disabled/minimal (smoke finding: a 2G-caged weed mount OOMs on
    # the 16 GiB prep). JuiceFS differs by architecture: its file:// driver
    # runs inside the client daemon, so the tight cage sits there (the
    # decomposition-report mechanism, unchanged).
    swfs)
        case "$regime" in
        R2) swfs_mount "$tag" "$CAGE_MB" 0 ;;
        *) swfs_mount "$tag" "$CAGE_MB" "$CACHE_MB" ;;
        esac
        ;;
    gee)
        case "$regime" in
        R2) gee_mount "$tag" "$CAGE_MB" 300 ;;
        *) gee_mount "$tag" "$CAGE_MB" "$CACHE_MB" ;;
        esac
        ;;
    mps3)
        case "$regime" in
        R2) mps3_mount "$tag" "$CAGE_MB" 0 ;; # no --cache = their default
        *) mps3_mount "$tag" "$CAGE_MB" "$CACHE_MB" ;;
        esac
        ;;
    esac
}

sys_umount() { # <system>
    case "$1" in
    sqz) sqz_umount ;;
    jfs) jfs_umount ;;
    swfs) swfs_umount ;;
    gee) gee_umount ;;
    mps3) mps3_umount ;;
    esac
}

sys_backend_stop() { # <system>
    case "$1" in
    swfs) swfs_server_stop ;;
    gee | mps3) rustfs_stop ;;
    *) : ;;
    esac
}

sys_mnt() { # <system> -> mountpoint path
    case "$1" in
    sqz) echo "$SQZ_MNT" ;;
    jfs) echo "$JFS_MNT" ;;
    swfs) echo "$SWFS_MNT" ;;
    gee) echo "$GEE_MNT" ;;
    mps3) echo "$MPS3_MNT" ;;
    esac
}

sys_reap() { # <system> — kill lingering daemons from failed attempts (path-anchored)
    local pat pids p
    case "$1" in
    sqz) pat="squeezefs mount sqmeta://$SQZ_DIR" ;;
    jfs) pat="juicefs mount.*$JFS_MNT" ;;
    swfs) pat="weed mount.*$SWFS_MNT" ;;
    gee) pat="geesefs.*$GEE_MNT" ;;
    mps3) pat="mount-s3.*$MPS3_MNT" ;;
    *) return 0 ;;
    esac
    pids="$(pgrep -f "$pat" 2>/dev/null || true)"
    for p in $pids; do
        kill_pid_wait "$p" "$1 straggler"
    done
}

sys_pid() { # <system> -> refreshed daemon pid
    case "$1" in
    sqz) pgrep -f "squeezefs mount sqmeta://$SQZ_DIR" | head -1 ;;
    jfs) pgrep -f "juicefs mount.*$JFS_MNT" | head -1 ;;
    swfs) pgrep -f "weed mount.*$SWFS_MNT" | head -1 ;;
    gee) pgrep -f "geesefs.*$GEE_MNT" | head -1 ;;
    mps3) pgrep -f "mount-s3.*$MPS3_MNT" | head -1 ;;
    esac
}

# A daemon death mid-grid (e.g. cage OOM) must cost ONE flagged remount and
# keep the scoreboard filling — never abort the remaining rows.
ensure_alive() { # <regime> <system> <tag>
    local regime="$1" sys="$2" tag="$3" mnt
    mnt="$(sys_mnt "$sys")"
    if ! mountpoint -q "$mnt" 2>/dev/null || ! stat "$mnt" >/dev/null 2>&1; then
        log "WARN: $sys mount dead before $tag — remounting once (row flagged)"
        ROW_DIRTY_COLD="${ROW_DIRTY_COLD}DIRTY(remounted-dead-daemon),"
        sys_umount "$sys"
        mount_for_regime "$regime" "$sys" "remount_${tag}" || true
    fi
}

wipe_client_caches() { # <system> — R3 cold protocol
    case "$1" in
    jfs)
        rm -rf "$JFS_CACHE"
        mkdir -p "$JFS_CACHE"
        ;;
    swfs)
        rm -rf "$SWFS_CACHE"
        mkdir -p "$SWFS_CACHE"
        ;;
    mps3)
        rm -rf "$MPS3_CACHE"
        mkdir -p "$MPS3_CACHE"
        ;;
    *) : ;; # sqz: remount drops tiers; gee: RAM-only cache dies with remount
    esac
}

cold_reset() { # <regime> <system> <tag> — R3 full drop + remount before a row
    local regime="$1" sys="$2" tag="$3"
    sys_umount "$sys"
    wipe_client_caches "$sys"
    if ! drop_caches; then
        COLD_DEGRADED=1
        ROW_DIRTY_COLD="${ROW_DIRTY_COLD}DIRTY(no-page-drop)," # page cache survived: not cold
    fi
    mount_for_regime "$regime" "$sys" "$tag" || true
}

run_regime_system() { # <regime> <system>
    local regime="$1" sys="$2" wl pid mnt attempt ok=0
    log "=== $regime / $sys ==="

    if [ -n "${SYS_FAIL[$sys]:-}" ]; then
        log "skipping $sys (earlier setup failure: ${SYS_FAIL[$sys]})"
        for wl in $WORKLOADS; do
            emit_na_row "$regime" "$wl" "$sys" "relaxed" "${SYS_FAIL[$sys]}"
            case "$wl" in seq_write_1m | rand_write_4k)
                emit_na_row "$regime" "$wl" "$sys" "durable" "${SYS_FAIL[$sys]}"
                ;;
            esac
        done
        return 0
    fi

    # 3 real setup attempts, then the system is a named residual — one
    # reference must never block the standing surface. sqz dies loud instead.
    for attempt in 1 2 3; do
        if sys_format "$regime" "$sys" "${regime}_a${attempt}" &&
            mount_for_regime "$regime" "$sys" "${regime}_a${attempt}"; then
            ok=1
            break
        fi
        log "WARN: $sys setup attempt $attempt failed"
        sys_umount "$sys"
        sys_reap "$sys" # a half-started daemon must not poison the retry
        sleep 2
    done
    if [ "$ok" != "1" ]; then
        [ "$sys" = "sqz" ] && die "sqz setup failed — the gate side never degrades"
        SYS_FAIL[$sys]="setup failed 3x in $regime (see $ART/logs)"
        log "ERROR: ${SYS_FAIL[$sys]} — column becomes n/a-with-reason"
        sys_backend_stop "$sys"
        for wl in $WORKLOADS; do
            emit_na_row "$regime" "$wl" "$sys" "relaxed" "${SYS_FAIL[$sys]}"
            case "$wl" in seq_write_1m | rand_write_4k)
                emit_na_row "$regime" "$wl" "$sys" "durable" "${SYS_FAIL[$sys]}"
                ;;
            esac
        done
        return 0
    fi

    mnt="$(sys_mnt "$sys")"
    verify_capabilities "$sys" "$mnt"

    for wl in $WORKLOADS; do
        if is_ns "$sys" "$wl"; then
            log "$regime.$wl.$sys: N/S — $(ns_reason "$sys" "$wl")"
            emit_ns_row "$regime" "$wl" "$sys" "relaxed"
            case "$wl" in seq_write_1m | rand_write_4k)
                emit_ns_row "$regime" "$wl" "$sys" "durable"
                ;;
            esac
            continue
        fi
        ensure_alive "$regime" "$sys" "${regime}_${wl}"
        if [ "$regime" = "R3" ]; then
            # Cold-cache: prep state warm, then full drop + remount, then the
            # timed first pass.
            case "$wl" in
            seq_write_1m) ;; # cold by construction on the fresh volume
            stat_storm | del_storm)
                ensure_tree "$mnt" "$sys" || {
                    prep_failed_row "$regime" "$wl" "$sys"
                    continue
                }
                cold_reset "$regime" "$sys" "${regime}_${wl}"
                ;;
            *)
                ensure_dataset "$mnt" "$sys" || {
                    prep_failed_row "$regime" "$wl" "$sys"
                    continue
                }
                cold_reset "$regime" "$sys" "${regime}_${wl}"
                ;;
            esac
        elif [ "$regime" = "R2" ]; then
            # Device-true reads must not inherit residual page cache from the
            # dataset-writing pass. Best-effort drop; the per-row VERIFY line
            # remains the authority.
            case "$wl" in
            seq_read_1m | rand_read_4k)
                drop_caches || ROW_DIRTY_COLD="${ROW_DIRTY_COLD}FLAG(no-page-drop),"
                ;;
            esac
        fi
        pid="$(sys_pid "$sys")"
        run_workload "$regime" "$wl" "$sys" "$mnt" "$pid"
    done
    sys_umount "$sys"
    sys_backend_stop "$sys"
}

# ---------------------------------------------------------------------------
# Teardown
# ---------------------------------------------------------------------------
# shellcheck disable=SC2329 # invoked via cleanup (EXIT trap)
teardown_all() {
    sqz_umount || true
    jfs_umount || true
    swfs_umount || true
    gee_umount || true
    mps3_umount || true
    swfs_server_stop || true
    rustfs_stop || true
    if [ -n "$REDIS_PID" ] && [ -d "/proc/$REDIS_PID" ]; then
        kill "$REDIS_PID" 2>/dev/null || true
    fi
    REDIS_PID=""
}

# Best-effort sweep for the teardown VERB (fresh process: no recorded PIDs).
# Kill patterns are ownership-anchored to OUR substrate paths only.
teardown_verb() {
    local mnt pids
    for mnt in "$SQZ_MNT" "$JFS_MNT" "$SWFS_MNT" "$GEE_MNT" "$MPS3_MNT"; do
        fuse_unmount "$mnt"
    done
    for pat in "squeezefs mount sqmeta://$SUBSTRATE" "juicefs mount.*$SUBSTRATE" \
        "weed (server|mount).*$SUBSTRATE" "geesefs.*$SUBSTRATE" \
        "mount-s3.*$SUBSTRATE" "rustfs server.*$SUBSTRATE" \
        "redis-server 127.0.0.1:$PORT_REDIS"; do
        pids="$(pgrep -f "$pat" 2>/dev/null || true)"
        for p in $pids; do
            kill_pid_wait "$p" "$pat"
        done
    done
    if [ "$KEEP" != "1" ]; then
        rm -rf "$SQZ_DIR" "$JFS_DIR" "$SWFS_DIR" "$RUSTFS_DIR" "$MPS3_CACHE"
        rmdir "$SQZ_MNT" "$JFS_MNT" "$SWFS_MNT" "$GEE_MNT" "$MPS3_MNT" 2>/dev/null || true
        log "substrate stores removed (artifacts kept under $SUBSTRATE/artifacts)"
    fi
    log "teardown complete"
}

# shellcheck disable=SC2329 # invoked via the EXIT trap
cleanup() {
    trap - EXIT
    teardown_all
    if [ "$KEEP" != "1" ]; then
        rm -rf "$SQZ_DIR" "$JFS_DIR" "$SWFS_DIR" "$RUSTFS_DIR" "$MPS3_CACHE"
        rmdir "$SQZ_MNT" "$JFS_MNT" "$SWFS_MNT" "$GEE_MNT" "$MPS3_MNT" 2>/dev/null || true
        log "substrate stores removed (SQUEEZEFS_SB_KEEP=1 to retain); artifacts kept at $ART"
    else
        log "substrate stores retained at $SUBSTRATE"
    fi
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
    guard_paths
    if [ "$VERB" = "teardown" ]; then
        teardown_verb
        exit 0
    fi
    [ "$VERB" = "run" ] || die "unknown verb '$VERB' (run|teardown)"
    check_substrate
    ports_preflight
    mkdir -p "$ART/logs" "$ART/rows" "$TOOLS_DIR"
    : >"$ROWS_TSV"
    printf 'sys\tworkload\tdeclared_ns_reason\tprobe_result\n' >"$CAPS_TSV"

    # SqueezeFS binary: build if missing (as the invoking user when sudo'd;
    # house build rails). /usr/local/bin/squeezefs is never used or touched.
    if [ ! -x "$SQZ_BIN" ]; then
        log "building squeezefs release binary"
        if [ "$(id -u)" -eq 0 ] && [ -n "$RUNUSER" ] && [ "$RUNUSER" != "root" ]; then
            su -s /bin/bash "$RUNUSER" -c "cd '$REPO_DIR' && taskset -c 0-15 env CARGO_BUILD_JOBS=12 cargo build --release" ||
                die "cargo build failed"
        else
            (cd "$REPO_DIR" && taskset -c 0-15 env CARGO_BUILD_JOBS=12 cargo build --release) || die "cargo build failed"
        fi
    fi

    # Pinned tools (fetch-if-missing, checksum-verified). A missing REFERENCE
    # marks its column n/a-with-reason; a missing DRIVER dies.
    resolve_elbencho
    resolve_juicefs || true
    resolve_seaweedfs || true
    resolve_geesefs || true
    resolve_mps3 || true
    resolve_rustfs || true
    [ -n "$JUICEFS_BIN" ] || SYS_FAIL[jfs]="juicefs $PIN_JUICEFS_VER unavailable (download+checksum failed)"
    [ -n "$WEED_BIN" ] || SYS_FAIL[swfs]="seaweedfs $PIN_SEAWEEDFS_VER unavailable (download+checksum failed)"
    [ -n "$GEESEFS_BIN" ] || SYS_FAIL[gee]="geesefs $PIN_GEESEFS_VER unavailable (download+checksum failed)"
    [ -n "$MPS3_BIN" ] || SYS_FAIL[mps3]="mountpoint-s3 $PIN_MPS3_VER unavailable (download+checksum failed)"
    if [ -z "$RUSTFS_BIN" ]; then
        SYS_FAIL[gee]="${SYS_FAIL[gee]:-rustfs $PIN_RUSTFS_VER unavailable (shared S3 backend)}"
        SYS_FAIL[mps3]="${SYS_FAIL[mps3]:-rustfs $PIN_RUSTFS_VER unavailable (shared S3 backend)}"
    fi
    probe_cages

    if [ "$(id -u)" -ne 0 ] && ! sudo -n true 2>/dev/null; then
        case " $REGIMES " in *" R3 "*)
            log "WARN: no root and no passwordless sudo — R3 page-cache drops degraded (rows will be flagged)"
            ;;
        esac
    fi

    # Version + provenance pinning (goes into the scoreboard verbatim).
    local sqz_sha sqz_ver jfs_ver eb_ver weed_ver gee_ver mps3_ver rustfs_ver kern
    sqz_sha="$(git -C "$REPO_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)"
    sqz_ver="squeezefs @ $sqz_sha (md5 $(md5sum "$SQZ_BIN" | cut -d' ' -f1))"
    jfs_ver="${JUICEFS_BIN:+$("$JUICEFS_BIN" version 2>/dev/null | head -1)}"
    weed_ver="${WEED_BIN:+$("$WEED_BIN" version 2>/dev/null | head -1)}"
    gee_ver="${GEESEFS_BIN:+$("$GEESEFS_BIN" --version 2>&1 | head -1)}"
    mps3_ver="${MPS3_BIN:+$("$MPS3_BIN" --version 2>/dev/null | head -1)}"
    rustfs_ver="${RUSTFS_BIN:+$("$RUSTFS_BIN" --version 2>/dev/null | head -1)}"
    eb_ver="$("$ELBENCHO_BIN" --version 2>/dev/null | awk '/Version/{print $3; exit}')"
    kern="$(uname -r)"

    log "substrate: $SUBSTRATE ($SUB_FSTYPE on $SUB_SRC, disk=$DISK)"
    log "budget: ${CACHE_MB} MiB matched | dataset: ${DATASET_GB} GiB (16 x ${FILE_MB} MiB) | tree: $((THREADS * TREE_DIRS * TREE_FILES)) files"
    log "$sqz_ver"
    log "refs: jfs=${jfs_ver:-n/a} | swfs=${weed_ver:-n/a} | gee=${gee_ver:-n/a} | mps3=${mps3_ver:-n/a} | s3-backend=${rustfs_ver:-n/a}"
    log "elbencho: $eb_ver | kernel: $kern | cpus: $ONLINE_CPUS (cpuset: ${CPUSET:-none}) | ports: base=$PORT_BASE"
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
    # Scoreboard: merge rows, compute verdicts + top-3 ranks, emit, gate.
    # -----------------------------------------------------------------------
    SCORE_META="sqz=$sqz_ver | jfs=${jfs_ver:-n/a} (meta=$JFS_META_ENGINE) | swfs=${weed_ver:-n/a} | gee=${gee_ver:-n/a} | mps3=${mps3_ver:-n/a} | s3=${rustfs_ver:-n/a} | elbencho=$eb_ver | kernel=$kern | box=$(nproc)cpu/$(free -g | awk '/^Mem/{print $2}')GiB | substrate=$SUB_FSTYPE:$SUB_SRC | cache_budget=${CACHE_MB}MiB | dataset=${DATASET_GB}GiB | cage=${CAGE_MB}MiB (R2 refs ${R2_REF_CAGE_MB}MiB) | cold_degraded=$COLD_DEGRADED" \
        SMOKE="$SMOKE" ALLOW_LOSS="$ALLOW_LOSS" SYSTEMS="$SYSTEMS" \
        python3 - "$ROWS_TSV" "$ART/scoreboard.md" "$ART/scoreboard.tsv" <<'EOF'
import os, sys

rows_tsv, out_md, out_tsv = sys.argv[1], sys.argv[2], sys.argv[3]
smoke = os.environ.get("SMOKE") == "1"
allow = {r.strip() for r in os.environ.get("ALLOW_LOSS", "").split(",") if r.strip()}
meta = os.environ.get("SCORE_META", "")
systems = os.environ.get("SYSTEMS", "sqz jfs swfs gee mps3").split()
refs = [s for s in systems if s != "sqz"]
WRITE_FAMILY = {"seq_write_1m", "rand_write_4k"}

cells = {}   # (regime, wl, mode) -> {sys: {...}}
order = []
for line in open(rows_tsv):
    f = line.rstrip("\n").split("\t")
    if len(f) < 11:
        continue
    rowid, regime, wl, mode, sysname, unit, value, rc, dirty, dev, note = f[:11]
    key = (regime, wl, mode)
    if key not in cells:
        cells[key] = {"unit": unit if unit != "-" else ""}
        order.append(key)
    if unit != "-" and not cells[key]["unit"]:
        cells[key]["unit"] = unit
    try:
        v = float(value)
    except ValueError:
        v = None
    cells[key][sysname] = {"v": v, "raw": value, "rc": rc, "dirty": dirty,
                           "note": note, "dev": dev}

def is_primary(wl, mode):
    return (mode == "durable") if wl in WRITE_FAMILY else (mode == "relaxed")

def fmt(v, unit, raw):
    if raw == "NS":
        return "N/S"
    if v is None:
        return "n/a"
    return f"{v:,.0f}" if (unit != "MiB/s" or v >= 100) else f"{v:,.1f}"

losses, invalid = [], []
prim_md = ["", "## Primary kernel-FUSE table (gating; write rows = durable mode)", "",
           "| Row | Workload (unit) | SQZ | " +
           " | ".join(f"{r.upper()} | SQZ/{r} | v" for r in refs) + " | Rank |",
           "|---|---|---:|" + "---:|---:|:--:|" * len(refs) + ":--:|"]
lab_md = ["", "## Relaxed write rows (native ACK semantics — labeled, non-gating)", "",
          "| Row | Workload (unit) | SQZ | " +
          " | ".join(f"{r.upper()} | SQZ/{r}" for r in refs) + " |",
          "|---|---|---:|" + "---:|---:|" * len(refs) + ""]
tsv = ["row_id\tregime\tworkload\tmode\tunit\t" +
       "\t".join(systems) + "\trank\tverdicts\tflags"]
family = {}  # (wl, mode) -> list of (rowid, rank, n_ranked)

for key in order:
    regime, wl, mode = key
    r = cells[key]
    unit = r["unit"]
    rowid = f"{regime}.{wl}"
    primary = is_primary(wl, mode)
    s = r.get("sqz")
    sv = s["v"] if s else None
    flags = []
    for name in systems:
        side = r.get(name)
        if not side:
            continue
        if side["dirty"] not in ("quiet", ""):
            flags.append(f"{name}:{side['dirty']}")
        if "VERIFY=FAILED" in side.get("note", ""):
            flags.append(f"{name}:!DEV")
        if side["rc"] not in ("0",) and side["raw"] not in ("NS", "NA"):
            flags.append(f"{name}:rc={side['rc']}")

    sqz_bad = (s is None or sv is None or s["rc"] != "0"
               or "VERIFY=FAILED" in s.get("note", ""))
    verdicts = []
    row_cells = []
    ranked_vals = []
    if not sqz_bad:
        ranked_vals.append(sv)
    for ref in refs:
        side = r.get(ref)
        raw = side["raw"] if side else "NA"
        rv = side["v"] if side else None
        if raw == "NS":
            verdicts.append((ref, "N/S", None))
        elif rv is None:
            verdicts.append((ref, "ref-n/a", None))
        else:
            ranked_vals.append(rv)
            ratio = (sv / rv) if (not sqz_bad and rv > 0) else None
            if ratio is None:
                verdicts.append((ref, "—", None))
            elif ratio > 1.05:
                verdicts.append((ref, "**W**", ratio))
            elif ratio < 0.95:
                aid_row, aid_ref = rowid, f"{rowid}.{ref}"
                allowed = aid_row in allow or aid_ref in allow
                verdicts.append((ref, "L (allowed)" if allowed else "**L**", ratio))
                if primary and not allowed and not smoke:
                    losses.append(f"{rowid}.{ref}" + ("@durable" if mode == "durable" else ""))
            else:
                verdicts.append((ref, "TIE", ratio))
        row_cells.append((ref, raw, rv))

    if primary and sqz_bad:
        if rowid not in allow:
            invalid.append(rowid + ("@durable" if mode == "durable" else ""))
        rank_txt = "INVALID"
        rank = None
    elif sqz_bad:
        rank_txt = "—"
        rank = None
    else:
        better = sum(1 for v in ranked_vals if v > sv)
        rank = 1 + better
        rank_txt = f"{rank}/{len(ranked_vals)}"
    if primary and rank is not None:
        family.setdefault((wl, mode), []).append((rowid, rank, len(ranked_vals)))

    sqz_txt = fmt(sv, unit, s["raw"] if s else "NA")
    ref_cols = []
    for (ref, verdict, ratio) in verdicts:
        side = r.get(ref)
        raw = side["raw"] if side else "NA"
        rv = side["v"] if side else None
        ratio_txt = f"{ratio:.2f}x" if ratio is not None else "—"
        ref_cols.append((fmt(rv, unit, raw), ratio_txt, verdict))
    label = f"{rowid}{'@durable' if mode == 'durable' else ''}"
    if primary:
        prim_md.append(f"| {label} | {wl} ({unit}) | {sqz_txt} | " +
                       " | ".join(f"{c[0]} | {c[1]} | {c[2]}" for c in ref_cols) +
                       f" | {rank_txt} |")
    elif mode == "relaxed" and wl in WRITE_FAMILY:
        lab_md.append(f"| {rowid}@relaxed | {wl} ({unit}) | {sqz_txt} | " +
                      " | ".join(f"{c[0]} | {c[1]}" for c in ref_cols) + " |")
    tsv.append("\t".join([rowid, regime, wl, mode, unit] +
                         [str(r.get(nm, {}).get("raw", "NA")) for nm in systems] +
                         [rank_txt, ";".join(f"{v[0]}={v[1]}" for v in verdicts),
                          ",".join(flags)]))

top3_md = ["", "## Top-3 adjudication (per row-family, primary table)", "",
           "| Workload family | Rows | Worst rank | Top-3? |", "|---|---|:--:|:--:|"]
overall_top3 = True
for (wl, mode), rows_list in sorted(family.items()):
    worst = max(rk for (_, rk, _) in rows_list)
    denom = max(n for (_, _, n) in rows_list)
    ok = worst <= 3
    overall_top3 = overall_top3 and ok
    label = f"{wl}{'@durable' if mode == 'durable' else ''}"
    detail = ", ".join(f"{rid}:{rk}/{n}" for (rid, rk, n) in rows_list)
    top3_md.append(f"| {label} | {detail} | {worst}/{denom} | {'YES' if ok else '**NO**'} |")
if family:
    top3_md.append("")
    top3_md.append(f"**Overall: SqueezeFS is {'TOP-3 OR BETTER on every adjudicated row-family' if overall_top3 else 'NOT top-3 on at least one row-family — see NO rows'}.**")

header = ["# SqueezeFS multi-reference scoreboard", "",
          f"Provenance: {meta}", "",
          "Verdict rule (per reference): W if SQZ > 1.05x ref, L if < 0.95x, TIE inside ±5%. "
          "Write-family verdicts are governed by the durable rows (fsync-inclusive, RW6); "
          "relaxed write rows are published labeled below. N/S = not supported by design "
          "(neutral; excluded from ranks). Rank = SqueezeFS position among numeric cells "
          "(1 = fastest). INVALID = missing/unverified/failed SqueezeFS cell (gates).", ""]
doc = header + prim_md + lab_md + top3_md
open(out_md, "w").write("\n".join(doc) + "\n")
open(out_tsv, "w").write("\n".join(tsv) + "\n")
print("\n".join(doc))

gate_fail = losses + invalid
if gate_fail and not smoke:
    print(f"\nGATE: FAIL — loss/invalid rows: {', '.join(gate_fail)}", file=sys.stderr)
    sys.exit(1)
print(f"\nGATE: {'SMOKE (not gating)' if smoke else 'GREEN — no unattributed loss rows'}")
EOF
    local gate_rc=$?

    log "scoreboard: $ART/scoreboard.md (+ .tsv); capability ledger: $CAPS_TSV; raw rows + counter snapshots + device evidence: $ART/rows/"
    exit "$gate_rc"
}

main "$@"
