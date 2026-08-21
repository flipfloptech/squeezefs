#!/usr/bin/env bash
# tests/l1a_sweep.sh — the FIND-L1-A sweep harness (PR RW1 of
# docs/design-random-small-writes.md; §5.3 W3 forensics grid; G-RW4's
# acceptance instrument; G-RW1's deferral-signature source).
#
# Scripted t×mb O_DIRECT write grid over a fresh-volume SqueezeFS sandbox,
# ON-RAIL (taskset CPUSET — the scoreboard rails) and OFF-RAIL (full CPU
# mask — FIND-VS-B proved shard-geometry defects hide on the 16-CPU rail),
# n>=3 reps per cell, every mount rig-armed (SQUEEZEFS_OP_PROFILE=1).
#
# Emits the falsifiable "CONVOY-SHAPED" signature the design's G-RW1
# deferral clause cites (review Issue 18 — a definition, so the deferral
# can never be invoked rhetorically). Per (rail, shape) the verdict is
# CONVOY-SHAPED iff ALL of:
#   (a) t16-vs-t8 boundary:   median(t16) < 0.95 x median(t8) at mb256;
#   (b) mb boundary:          median(mb256@t16) < 0.95 x median(mb12@t16)
#       (mb < writers throttles the convoy out of sight — L1's lever);
#   (c) block_lock_wait tail class: the per-row >=2ms-bucket sample count
#       at t16/mb256 exceeds 10x the t8/mb256 count (the L1 report's
#       "241-sample <=16 ms tail" class), with the rig's cross-key vs
#       same-key split recorded alongside (H2b attribution).
#
# Shapes:
#   seq  (default): elbencho -w -t $T -s ${FILE_MB}m -b 1m --direct on $T
#                   files — the L1 report's regressing shape (write seq 1m).
#   rand:           untimed seq prep + elbencho -w --rand -t $T -b 4k
#                   --iodepth 16 --direct --timelimit $TIMELIMIT — the
#                   scoreboard rand_write_4k row (RW2's G-RW1 adjudication
#                   re-runs THIS shape through the same signature).
#
# Usage:
#   tests/l1a_sweep.sh                       # full grid (~40-60 min)
#   SQUEEZEFS_L1A_SMOKE=1 tests/l1a_sweep.sh # plumbing micro-grid
#   SQUEEZEFS_L1A_SHAPE=rand SQUEEZEFS_L1A_TVALUES="8 16" tests/l1a_sweep.sh
#
# Env knobs (all optional):
#   SQUEEZEFS_L1A_BIN       squeezefs binary (default $REPO/target/release/
#                           squeezefs) — RW3's historical-pair attribution
#                           points this at a detached-worktree build
#   SQUEEZEFS_L1A_DIR       sandbox root (default /var/tmp/squeezefs_l1a;
#                           tmpfs refused; /mnt/squeezefs + ~/tmp/nvme refused)
#   SQUEEZEFS_L1A_TVALUES   writer counts        (default "8 12 13 16")
#   SQUEEZEFS_L1A_MBVALUES  max_background grid  (default "12 256"; ct = 3/4)
#   SQUEEZEFS_L1A_RAILS     "on off" subset      (default "on off")
#   SQUEEZEFS_L1A_REPS      reps per cell        (default 3)
#   SQUEEZEFS_L1A_FILE_MB   seq per-writer file MiB (default 256)
#   SQUEEZEFS_L1A_SHAPE     seq | rand           (default seq)
#   SQUEEZEFS_L1A_TIMELIMIT rand row seconds     (default 30)
#   SQUEEZEFS_L1A_DATASET_GB rand dataset GiB over 16 files (default 16)
#   SQUEEZEFS_L1A_CPUSET    on-rail mask         (default 0-15 when >=20 CPUs)
#   SQUEEZEFS_L1A_CAGE_MB   daemon memcg cage    (default 16384)
#   SQUEEZEFS_L1A_OUT_DIR   artifacts dir (default $DIR/artifacts/<ts>)
#   SQUEEZEFS_L1A_KEEP=1    keep the sandbox stores after the run
#
# Safety rails (the house protocol): never touches /mnt/squeezefs,
# /mnt/juicefs or ~/tmp/nvme; unique sandbox + timestamped artifacts; kills
# by PID only; daemons in systemd-run scopes (memcg cages) when available;
# 3-poll quiet gate before every timed row; Tctl >= 88 C HARD-pauses the
# grid until the box cools.

set -uo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"

SMOKE="${SQUEEZEFS_L1A_SMOKE:-0}"
DIR="${SQUEEZEFS_L1A_DIR:-/var/tmp/squeezefs_l1a}"
TVALUES="${SQUEEZEFS_L1A_TVALUES:-8 12 13 16}"
MBVALUES="${SQUEEZEFS_L1A_MBVALUES:-12 256}"
RAILS="${SQUEEZEFS_L1A_RAILS:-on off}"
REPS="${SQUEEZEFS_L1A_REPS:-3}"
FILE_MB="${SQUEEZEFS_L1A_FILE_MB:-256}"
SHAPE="${SQUEEZEFS_L1A_SHAPE:-seq}"
TIMELIMIT="${SQUEEZEFS_L1A_TIMELIMIT:-30}"
DATASET_GB="${SQUEEZEFS_L1A_DATASET_GB:-16}"
CAGE_MB="${SQUEEZEFS_L1A_CAGE_MB:-16384}"
QUIET_POLLS="${SQUEEZEFS_L1A_QUIET_POLLS:-3}"
QUIET_SECS="${SQUEEZEFS_L1A_QUIET_SECS:-5}"
ROW_TIMEOUT="${SQUEEZEFS_L1A_ROW_TIMEOUT:-600}"
# RW4 / G-RW6: format-time compression ("none" | "lz4" | "zstd") — the
# compressed-volume rand-write row (every write patch-ineligible).
COMPRESSION="${SQUEEZEFS_L1A_COMPRESSION:-none}"

if [ "$SMOKE" = "1" ]; then
    TVALUES="${SQUEEZEFS_L1A_TVALUES:-2}"
    MBVALUES="${SQUEEZEFS_L1A_MBVALUES:-256}"
    RAILS="${SQUEEZEFS_L1A_RAILS:-on}"
    REPS="${SQUEEZEFS_L1A_REPS:-1}"
    FILE_MB="${SQUEEZEFS_L1A_FILE_MB:-32}"
    TIMELIMIT="${SQUEEZEFS_L1A_TIMELIMIT:-5}"
    DATASET_GB="${SQUEEZEFS_L1A_DATASET_GB:-1}"
    QUIET_POLLS=1
    QUIET_SECS=1
fi

ONLINE_CPUS="$(nproc)"
if [ -n "${SQUEEZEFS_L1A_CPUSET:-}" ]; then
    CPUSET="$SQUEEZEFS_L1A_CPUSET"
elif [ "$ONLINE_CPUS" -ge 20 ]; then
    CPUSET="0-15"
else
    CPUSET=""
fi

TS="$(date -u +%Y%m%dT%H%M%SZ)"
ART="${SQUEEZEFS_L1A_OUT_DIR:-$DIR/artifacts/$TS}"
ROWS_TSV="$ART/rows.tsv"
KEEP="${SQUEEZEFS_L1A_KEEP:-0}"

log() { echo "[l1a $(date -u +%H:%M:%S)] $*" >&2; }
die() {
    log "FATAL: $*"
    exit 2
}

# ---------------------------------------------------------------------------
# Safety rails
# ---------------------------------------------------------------------------
case "$DIR" in
/mnt/squeezefs* | /mnt/juicefs* | "$HOME/tmp/nvme"*) die "refusing protected path $DIR" ;;
esac
mkdir -p "$DIR" || die "cannot create $DIR"
SUB_FSTYPE="$(stat -f -c %T "$DIR" 2>/dev/null || echo unknown)"
[ "$SUB_FSTYPE" = "tmpfs" ] && die "sandbox on tmpfs defeats device evidence — pick a disk path"
mkdir -p "$ART/logs" "$ART/rows"

SQZ_BIN="${SQUEEZEFS_L1A_BIN:-$REPO_DIR/target/release/squeezefs}"
[ -x "$SQZ_BIN" ] || die "squeezefs binary missing at $SQZ_BIN — cargo build --release first (or fix SQUEEZEFS_L1A_BIN)"
ELBENCHO_BIN="$(command -v elbencho || true)"
[ -n "$ELBENCHO_BIN" ] || die "elbencho not found in PATH"

# Parent disk for /proc/diskstats evidence (partition -> disk).
SRC_DEV="$(df --output=source "$DIR" | tail -1)"
DISK="$(lsblk -no pkname "$SRC_DEV" 2>/dev/null | head -1)"
[ -z "$DISK" ] && DISK="$(basename "$SRC_DEV" 2>/dev/null)"
grep -q " $DISK " /proc/diskstats 2>/dev/null || {
    log "WARN: no diskstats row for '$DISK' — device evidence disabled"
    DISK=""
}

tctl_read() {
    sensors 2>/dev/null | awk '/Tctl/{gsub(/[+°C]/,"",$2); print $2; exit}'
}

cotenants() {
    local out=""
    pgrep -x rustc >/dev/null 2>&1 && out="${out}rustc,"
    pgrep -x cargo >/dev/null 2>&1 && out="${out}cargo,"
    pgrep -f pytest >/dev/null 2>&1 && out="${out}pytest,"
    pgrep -x elbencho >/dev/null 2>&1 && out="${out}foreign-elbencho,"
    echo "$out"
}

# House 3-poll quiet gate + the RW1 rails' HARD thermal pause: Tctl >= 88 C
# blocks the grid until the box cools (never proceeds hot); the ordinary
# <80 C poll gate flags DIRTY after ~5 min instead of blocking forever.
ROW_DIRTY=""
quiet_gate() {
    ROW_DIRTY=""
    local tries=0 streak=0 tctl cot
    while :; do
        tctl="$(tctl_read)"
        if [ -n "$tctl" ] && python3 -c "exit(0 if float('$tctl') >= 88 else 1)" 2>/dev/null; then
            log "THERMAL PAUSE: Tctl=${tctl}C >= 88C — waiting for cooldown"
            sleep 30
            continue
        fi
        cot="$(cotenants)"
        if [ -z "$cot" ] && python3 -c "exit(0 if float('${tctl:-0}') < 80 else 1)" 2>/dev/null; then
            streak=$((streak + 1))
            [ "$streak" -ge "$QUIET_POLLS" ] && return 0
        else
            streak=0
            tries=$((tries + 1))
            if [ "$tries" -ge 60 ]; then
                ROW_DIRTY="DIRTY(gate:${cot}tctl=${tctl:-na})"
                log "quiet-gate never settled — proceeding $ROW_DIRTY"
                return 0
            fi
        fi
        sleep "$QUIET_SECS"
    done
}

# systemd-run cage wrapper (root-tolerant; degrades to uncaged with a WARN).
CAGES_OK=""
cage_probe() {
    if command -v systemd-run >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
        local user_arg=()
        [ "$(id -u)" -ne 0 ] && user_arg=(--user)
        if systemd-run --quiet --collect --scope "${user_arg[@]}" -p MemoryMax=64M true 2>/dev/null; then
            CAGES_OK=1
        fi
    fi
    [ "$CAGES_OK" = "1" ] || log "WARN: systemd-run cages unavailable — daemon runs UNCAGED"
}

cage_cmd() { # <memmax_mb> <unit_suffix>
    if [ "$CAGES_OK" = "1" ]; then
        local user_arg=()
        [ "$(id -u)" -ne 0 ] && user_arg=(--user)
        CAGE_ARGV=(systemd-run --quiet --collect --scope "${user_arg[@]}"
            --unit "l1a-$2-$$-$RANDOM" -p "MemoryMax=${1}M")
    else
        CAGE_ARGV=()
    fi
}

pin_cmd() { # <rail>
    if [ "$1" = "on" ] && [ -n "$CPUSET" ]; then
        PIN_ARGV=(taskset -c "$CPUSET")
    else
        PIN_ARGV=()
    fi
}

# ---------------------------------------------------------------------------
# SqueezeFS sandbox stack (fresh volumes per (rail, mb) mount — the L1
# fresh-volume isolation protocol)
# ---------------------------------------------------------------------------
SQZ_DIR="$DIR/sqz"
SQZ_MNT="$DIR/mnt"
SQZ_STAGING="$SQZ_DIR/staging"
SQZ_PID=""

sqz_meta_uri() {
    echo "sqmeta://$SQZ_DIR/meta1.img,$SQZ_DIR/meta2.img,$SQZ_DIR/meta3.img,$SQZ_DIR/meta4.img"
}

sqz_format() { # <tag>
    mkdir -p "$SQZ_DIR" "$SQZ_MNT"
    rm -f "$SQZ_DIR"/meta{1,2,3,4}.img "$SQZ_DIR"/data{1,2,3,4}.img
    rm -rf "$SQZ_STAGING"
    mkdir -p "$SQZ_STAGING"
    local i data_gb dataset_gb
    if [ "$SHAPE" = "rand" ]; then
        dataset_gb="$DATASET_GB"
    else
        # seq: worst-case cell = max(t) writers x FILE_MB, x2 CoW headroom.
        local tmax=0
        for i in $TVALUES; do [ "$i" -gt "$tmax" ] && tmax=$i; done
        dataset_gb=$(((tmax * FILE_MB + 1023) / 1024))
    fi
    data_gb=$(((dataset_gb * 2 + 7) / 4 + 2))
    for i in 1 2 3 4; do
        truncate -s 1G "$SQZ_DIR/meta$i.img" || die "truncate meta$i"
        truncate -s "${data_gb}G" "$SQZ_DIR/data$i.img" || die "truncate data$i"
    done
    "$SQZ_BIN" format \
        "$(sqz_meta_uri)" \
        "sqdata://$SQZ_DIR/data1.img,$SQZ_DIR/data2.img,$SQZ_DIR/data3.img,$SQZ_DIR/data4.img" \
        --disk-cache-paths "$SQZ_STAGING" \
        --compression "$COMPRESSION" \
        --force >"$ART/logs/format_$1.log" 2>&1 ||
        die "squeezefs format failed (see $ART/logs/format_$1.log)"
}

sqz_mount() { # <tag> <rail> <mb>
    local tag="$1" rail="$2" mb="$3"
    local ct=$((mb * 3 / 4))
    local logf="$ART/logs/mount_${tag}.log"
    if ! stat "$SQZ_MNT" >/dev/null 2>&1; then
        fusermount3 -uz "$SQZ_MNT" 2>/dev/null || umount -l "$SQZ_MNT" 2>/dev/null || true
        sleep 0.5
    fi
    cage_cmd "$CAGE_MB" "$tag"
    pin_cmd "$rail"
    # Rig ARMED on every daemon: the signature's tail attribution
    # (per-site + cross-key/same-key) comes from the RW1 rig families.
    # shellcheck disable=SC2094
    env SQUEEZEFS_OP_PROFILE=1 "${CAGE_ARGV[@]}" "${PIN_ARGV[@]}" "$SQZ_BIN" mount \
        "$(sqz_meta_uri)" "$SQZ_MNT" --daemon \
        --log-file "$logf" \
        -o "max_background=$mb,congestion_threshold=$ct" >>"$logf" 2>&1
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
    log "mounted $tag (rail=$rail mb=$mb ct=$ct pid=$SQZ_PID rig=armed)"
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
    if ! stat "$SQZ_MNT" >/dev/null 2>&1; then
        log "WARN: stale ENOTCONN mountpoint — lazy-detaching (teardown-SIGBUS class)"
        fusermount3 -uz "$SQZ_MNT" 2>/dev/null || umount -l "$SQZ_MNT" 2>/dev/null || true
        sleep 0.5
    fi
    for i in $(seq 1 300); do
        [ -n "$SQZ_PID" ] && [ -d "/proc/$SQZ_PID" ] || break
        sleep 0.2
    done
    if [ -n "$SQZ_PID" ] && [ -d "/proc/$SQZ_PID" ]; then
        log "WARN: daemon $SQZ_PID alive after unmount — SIGKILL by PID"
        kill -9 "$SQZ_PID" 2>/dev/null || true
    fi
    SQZ_PID=""
}

cleanup() {
    sqz_umount
    if [ "$KEEP" != "1" ]; then
        rm -f "$SQZ_DIR"/meta{1,2,3,4}.img "$SQZ_DIR"/data{1,2,3,4}.img
        rm -rf "$SQZ_STAGING"
    fi
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Row machinery
# ---------------------------------------------------------------------------
snap() { # <prefix> <suffix>
    local pfx="$1" sfx="$2"
    [ -n "$DISK" ] && grep " $DISK " /proc/diskstats >"$pfx.disk.$sfx"
    date +%s.%N >"$pfx.t.$sfx"
    cat "$SQZ_MNT/.stats" >"$pfx.stats.$sfx" 2>/dev/null || true
}

parse_elbencho() { # <file> <OP> <KEY>
    awk -v op="$2" -v key="$3" '
        /^[A-Z]+ +Elapsed/ { cur = $1 }
        $1 == op { cur = op }
        cur == op && index($0, key) {
            for (i = NF; i >= 1; i--) if ($i ~ /^[0-9.]+$/) { print $i; exit }
        }' "$1" 2>/dev/null | head -1 | grep . || echo "NA"
}

dataset_files() { # <count>
    DATA_FILES=()
    local i
    for i in $(seq -w 1 "$1"); do DATA_FILES+=("$SQZ_MNT/l1a/f$i"); done
}

# run_cell <rail> <mb> <t> <rep>
run_cell() {
    local rail="$1" mb="$2" t="$3" rep="$4"
    local cell="${SHAPE}.rail-${rail}.mb${mb}.t${t}.r${rep}"
    local pfx="$ART/rows/$cell"

    quiet_gate
    local tctl load
    tctl="$(tctl_read)"
    load="$(cut -d' ' -f1 /proc/loadavg)"

    pin_cmd "$rail"
    local rc value unit op
    if [ "$SHAPE" = "seq" ]; then
        dataset_files "$t"
        mkdir -p "$SQZ_MNT/l1a"
        rm -f "${DATA_FILES[@]}"
        snap "$pfx" before
        timeout -k 10 "$ROW_TIMEOUT" "${PIN_ARGV[@]}" "$ELBENCHO_BIN" \
            -w -t "$t" -s "${FILE_MB}m" -b 1m --direct "${DATA_FILES[@]}" \
            >"$pfx.elbencho" 2>&1
        rc=$?
        snap "$pfx" after
        op=WRITE
        value="$(parse_elbencho "$pfx.elbencho" WRITE "Throughput MiB/s")"
        unit="MiB/s"
    else
        dataset_files 16
        mkdir -p "$SQZ_MNT/l1a"
        if [ ! -f "${DATA_FILES[0]}" ]; then
            log "prep: rand dataset ($DATASET_GB GiB over 16 files, untimed)"
            timeout -k 10 $((ROW_TIMEOUT * 3)) "${PIN_ARGV[@]}" "$ELBENCHO_BIN" \
                -w -t 16 -s "$((DATASET_GB * 1024 / 16))m" -b 1m --direct \
                "${DATA_FILES[@]}" >"$ART/logs/prep_rand.$RANDOM.log" 2>&1 ||
                die "rand dataset prep failed"
        fi
        snap "$pfx" before
        timeout -k 10 "$ROW_TIMEOUT" "${PIN_ARGV[@]}" "$ELBENCHO_BIN" \
            -w --rand -t "$t" -b 4k --iodepth 16 --direct \
            --timelimit "$TIMELIMIT" "${DATA_FILES[@]}" \
            >"$pfx.elbencho" 2>&1
        rc=$?
        snap "$pfx" after
        op=WRITE
        value="$(parse_elbencho "$pfx.elbencho" WRITE IOPS)"
        unit="IOPS"
    fi

    local totmib
    totmib="$(parse_elbencho "$pfx.elbencho" "$op" "Total MiB")"
    [ "$rc" -eq 124 ] && log "WARN: $cell HIT ROW TIMEOUT (${ROW_TIMEOUT}s)"

    # block_lock_wait tail class + rig attribution + device deltas.
    local sig
    sig="$(python3 - "$pfx" <<'EOF'
import json, sys

p = sys.argv[1]

def jload(path):
    try:
        return json.load(open(path)).get("metrics", {})
    except Exception:
        return {}

MS_BUCKETS = ["<=2ms", "<=4ms", "<=8ms", "<=16ms", "<=32ms", "<=64ms",
              "<=128ms", "<=256ms", "<=512ms", "<=1024ms", "<=2s", "<=4s",
              "<=8s", "<=16s", ">16s"]

def tail(hist_b, hist_a):
    if not isinstance(hist_a, dict):
        return 0
    return sum(int(hist_a.get(k, 0)) - int((hist_b or {}).get(k, 0))
               for k in MS_BUCKETS)

def total(hist_b, hist_a):
    if not isinstance(hist_a, dict):
        return 0
    return sum(int(v) - int((hist_b or {}).get(k, 0))
               for k, v in hist_a.items())

b, a = jload(f"{p}.stats.before"), jload(f"{p}.stats.after")
blw_tail = tail(b.get("block_lock_wait"), a.get("block_lock_wait"))
blw_total = total(b.get("block_lock_wait"), a.get("block_lock_wait"))
audit_b = b.get("block_lock_stripe_audit") or {}
audit_a = a.get("block_lock_stripe_audit") or {}
cross = total(audit_b.get("cross_key_waits"), audit_a.get("cross_key_waits"))
same = total(audit_b.get("same_key_waits"), audit_a.get("same_key_waits"))
cross_tail = tail(audit_b.get("cross_key_waits"), audit_a.get("cross_key_waits"))
same_tail = tail(audit_b.get("same_key_waits"), audit_a.get("same_key_waits"))
sites_b = b.get("block_lock_wait_by_site") or {}
sites_a = a.get("block_lock_wait_by_site") or {}
site_tails = ",".join(
    f"{s}={tail(sites_b.get(s), sites_a.get(s))}"
    for s in sites_a
    if tail(sites_b.get(s), sites_a.get(s)) > 0
) or "none"
uqf = int(a.get("uring_queue_full", 0)) - int(b.get("uring_queue_full", 0))
pool = int(a.get("aligned_pool_misses", 0)) - int(b.get("aligned_pool_misses", 0))
dev = ""
try:
    d0 = open(f"{p}.disk.before").read().split()
    d1 = open(f"{p}.disk.after").read().split()
    dt = float(open(f"{p}.t.after").read()) - float(open(f"{p}.t.before").read())
    rmib = (int(d1[5]) - int(d0[5])) / 2 / 1024 / dt
    wmib = (int(d1[9]) - int(d0[9])) / 2 / 1024 / dt
    dev = f" dev_rMiB/s={rmib:.0f} dev_wMiB/s={wmib:.0f}"
except Exception:
    pass
print(f"blw_ms_tail={blw_tail} blw_total={blw_total} cross_key={cross} "
      f"same_key={same} cross_ms_tail={cross_tail} same_ms_tail={same_tail} "
      f"site_ms_tails={site_tails} uring_queue_full={uqf} "
      f"pool_misses={pool}{dev}")
EOF
)"

    {
        echo "cell=$cell rc=$rc value=$value unit=$unit total_mib=$totmib"
        echo "honesty: tctl=${tctl:-na}C load=$load ${ROW_DIRTY:-quiet}"
        echo "rig: $sig"
    } | tee "$pfx.env"

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$SHAPE" "$rail" "$mb" "$t" "$rep" "$value" "$unit" "$rc" \
        "${ROW_DIRTY:-quiet}" "$(echo "$sig" | tr ' \t' ';;')" >>"$ROWS_TSV"
}

# ---------------------------------------------------------------------------
# The grid
# ---------------------------------------------------------------------------
cage_probe
log "grid: shape=$SHAPE rails=[$RAILS] mb=[$MBVALUES] t=[$TVALUES] reps=$REPS cpuset=${CPUSET:-none}"
log "artifacts: $ART"
printf 'shape\trail\tmb\tt\trep\tvalue\tunit\trc\tdirty\trig\n' >"$ROWS_TSV"

for rail in $RAILS; do
    for mb in $MBVALUES; do
        tag="${SHAPE}_${rail}_mb${mb}"
        sqz_format "$tag"
        sqz_mount "$tag" "$rail" "$mb"
        for t in $TVALUES; do
            for rep in $(seq 1 "$REPS"); do
                run_cell "$rail" "$mb" "$t" "$rep"
            done
        done
        sqz_umount
    done
done

# ---------------------------------------------------------------------------
# The CONVOY-SHAPED signature verdict (per rail)
# ---------------------------------------------------------------------------
python3 - "$ROWS_TSV" "$SHAPE" <<'EOF'
import statistics as st
import sys

rows_tsv, shape = sys.argv[1], sys.argv[2]
rows = []
for line in open(rows_tsv).read().splitlines()[1:]:
    f = line.split("\t")
    if len(f) < 10 or f[7] not in ("0",):
        continue
    try:
        val = float(f[5])
    except ValueError:
        continue
    rig = dict(kv.split("=", 1) for kv in f[9].split(";") if "=" in kv)
    rows.append(dict(rail=f[1], mb=int(f[2]), t=int(f[3]), val=val,
                     tail=int(rig.get("blw_ms_tail", 0)),
                     cross=int(rig.get("cross_key", 0)),
                     same=int(rig.get("same_key", 0))))

def med(sel, key="val"):
    vs = [r[key] for r in rows if all(r[k] == v for k, v in sel.items())]
    return st.median(vs) if vs else None

def medi(sel, key):
    vs = [r[key] for r in rows if all(r[k] == v for k, v in sel.items())]
    return int(st.median(vs)) if vs else 0

mbs = sorted({r["mb"] for r in rows})
ts = sorted({r["t"] for r in rows})
hi_mb, lo_mb = (max(mbs), min(mbs)) if mbs else (None, None)
hi_t, lo_t = (max(ts), min(ts)) if ts else (None, None)

for rail in sorted({r["rail"] for r in rows}):
    print(f"\n=== rail={rail} shape={shape} — medians (n per cell varies by DIRTY/rc filter) ===")
    for mb in mbs:
        line = " ".join(
            f"t{t}={med(dict(rail=rail, mb=mb, t=t)):.0f}"
            for t in ts if med(dict(rail=rail, mb=mb, t=t)) is not None
        )
        print(f"  mb{mb}: {line}")
    if None in (hi_mb, lo_mb, hi_t, lo_t) or hi_mb == lo_mb or hi_t == lo_t:
        print("  SIGNATURE: grid too small for a verdict (need 2 mb x 2 t values)")
        continue
    m_hi_hi = med(dict(rail=rail, mb=hi_mb, t=hi_t))
    m_hi_lo = med(dict(rail=rail, mb=hi_mb, t=lo_t))
    m_lo_hi = med(dict(rail=rail, mb=lo_mb, t=hi_t))
    if None in (m_hi_hi, m_hi_lo, m_lo_hi) or 0 in (m_hi_lo, m_lo_hi):
        print("  SIGNATURE: missing cells — no verdict")
        continue
    r_t = m_hi_hi / m_hi_lo          # t-hi vs t-lo at mb-hi  (boundary a)
    r_mb = m_hi_hi / m_lo_hi         # mb-hi vs mb-lo at t-hi (boundary b)
    tail_hi = medi(dict(rail=rail, mb=hi_mb, t=hi_t), "tail")
    tail_lo = medi(dict(rail=rail, mb=hi_mb, t=lo_t), "tail")
    cross_hi = medi(dict(rail=rail, mb=hi_mb, t=hi_t), "cross")
    same_hi = medi(dict(rail=rail, mb=hi_mb, t=hi_t), "same")
    tail_cls = tail_hi > 10 * max(tail_lo, 1) or (tail_lo == 0 and tail_hi >= 50)
    convoy = r_t < 0.95 and r_mb < 0.95 and tail_cls
    print(
        f"  SIGNATURE rail={rail} shape={shape} "
        f"t{hi_t}/t{lo_t}@mb{hi_mb}={r_t:.2f} "
        f"mb{hi_mb}/mb{lo_mb}@t{hi_t}={r_mb:.2f} "
        f"blw_ms_tail@t{hi_t}={tail_hi} @t{lo_t}={tail_lo} "
        f"cross_key@t{hi_t}={cross_hi} same_key@t{hi_t}={same_hi} "
        f"verdict={'CONVOY-SHAPED' if convoy else 'not-convoy-shaped'}"
    )
    print(
        "  (CONVOY-SHAPED iff ALL: t-boundary < 0.95, mb-boundary < 0.95, "
        "ms-tail at t-hi > 10x t-lo — the G-RW1 deferral clause cites this "
        "line verbatim)"
    )
EOF

log "done — rows: $ROWS_TSV"
