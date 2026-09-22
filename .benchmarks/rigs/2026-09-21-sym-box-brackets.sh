#!/usr/bin/env bash
# The symmetric-metadata program's N-WRITER acceptance brackets (design gates
# 2 / 3 / 3b / 3c / 5 / 7 — docs/design-symmetric-metadata.md §8) on
# squeeze-test: the `tests/run_mw_matrix.sh sym-*` legs, each on the box's OWN
# tcp devsub fleet (`tests/mw_fleet.sh --symmetric` — nvmet-tcp on 127.0.0.1
# with resv_enable=1, null_blk metadata, zram data: the design's gate-3 venue
# "on the tcp devsub; squeeze-test brackets" and the D-5 fleet precedent on
# this box, .benchmarks/2026-09-08-d5-fleet-squeeze-test.md), run with
# `--venue=box` so every must-stay-0 gauge is a VERDICT, never a
# venue-attributed reading. Gate 1 is NOT here — it is PR 1's rig verbatim
# (2026-09-13-sym-pr1-solo-regate.sh) on the reset-v5 fabric.
#
# Shape (the minimum count the owner allows — AGENTS §Benchmark VENUE):
#   * one PASS per gate = REPEATS positions of the leg (default 2 — the two
#     same-arm positions give the noise band; the gate-2 leg is itself an
#     A-B-B-A (sym-1 local-1 local-2 sym-2), the others are self-relative
#     or absolute by design, so a second position is the band, not a
#     second bracket);
#   * fleet A (gates 2 / 3 / 3b / 3c / 7): `create N=2 --symmetric
#     --writers=7 --token-readers` — a manager, 7 joined writers (N = 8 for
#     sym-scale / sym-walls), one token reader (the -ls half of 3b);
#   * fleet B (gate 5): `create N=32 --symmetric --writers=1 --token-readers`
#     — 1 writer × 31 token readers, the design's broadcast row;
#   * fleet C (gate 7 row (b) at the design's N = 32): `create N=1
#     --symmetric --writers=31` — a manager + 31 joined writers; the
#     `walls32` gate runs sym-walls there with a SMALL row (a)
#     (`--walls-files=4`: 31 × 4 × 64 MiB fits the zram) so the join storm
#     of 32 mounts is the row; fleet A's `walls` is row (a) at N = 8;
#   * fleet M (gate 3's A ARM — the SHIPPED authority + co-writers on the
#     SAME binary, the design's "vs today's authority+co-writers"):
#     `create N=1 --cowriters=7` — an S9 authority + 7 co-writers, default
#     format, no symmetric knob; the `mwscale` gate runs `mw-scale` there
#     (the box re-run rung's leg — PR 13's box-rows rung had stated it as
#     the harness gap);
#   each fleet torn down to ZERO residue after its gates (the rig's own
#   assertion — a residue fails the pass).
#
# The A arm of gate 3b the design names ("vs today's authority+co-writers")
# has no leg in the tree (sym-shared-dir asserts the flip) — stated in the
# acceptance record §3.9; gate 3's A arm is fleet M's `mwscale`.
#
# Preflight REFUSES (loud, before any fleet exists): a non-sqz kernel (the
# 2026-09-22 finding: the box booted into the DDN Lustre kernel — FUSE-over-
# io_uring cannot arm there), a busy box, an existing fleet, a binary that
# does not name its profile, a missing TAR_SRC when gate 2 is requested.
#
# Usage (root; the repo's tests/ tree must sit beside this rig at
# <repo>/.benchmarks/rigs/ — on the box: rsync tests/ .benchmarks/rigs/ to
# /scratch/tmp/sym-box/repo/ and run from there):
#   sudo env BIN=/scratch/tmp/sym-box/squeezefs-B \
#            TAR_SRC=/scratch/tmp/sym-box/linux/fs \
#            [GATES="tarx scale mwscale shared-dir foreign-touch walls readers walls32"] \
#            [REPEATS=2] [FRESH_FLEET_PER_LEG=1] [OUT=/scratch/tmp/sym-box/brackets-<ts>] \
#            [SQZ_MWFLEET_OSS_GB=16] [SQZ_DEVSUB_OSS_ALGO=lzo-rle] \
#        bash 2026-09-21-sym-box-brackets.sh
#   SMOKE=1 = the laptop PLUMBING run ("it works" only — the venue law):
#   relaxes the sqz-kernel and quiet-box gates, labels every line SMOKE, and
#   is what FLEET_A_WRITERS / FLEET_B_READERS / LEG_EXTRA (appended to every
#   leg's arguments, e.g. "--sym-files=2000 --ingest-mb=64 --scale-ns=1,2")
#   exist for; no number from a SMOKE run enters a record.
#
# Artifacts: $OUT/<gate>-r<i>/ = the leg's $STATE/rows/* (its tables,
# verdicts, per-daemon .stats snapshots, venue-attributed ledger),
# $OUT/<gate>-r<i>.log (the leg's stdout+stderr), $OUT/<gate>-r<i>.{thermal,
# loadavg,dmesg}, $OUT/fleet-<A|B|C>-<n>.{create,teardown}.log, $OUT/box.log (the
# venue block: host, kernel, binary identity, fio-free), $OUT/SUMMARY.txt
# (every leg's verdict + table lines, by gate and position).
set -eu
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
: "${BIN:?BIN (the arm-B binary — the PR 13b tip or the flip binary) is required}"
FLEET="$REPO/tests/mw_fleet.sh"
MATRIX="$REPO/tests/run_mw_matrix.sh"
[ -x "$FLEET" ] && [ -x "$MATRIX" ] || { echo "tests/mw_fleet.sh / tests/run_mw_matrix.sh not found under $REPO (place the repo's tests/ beside this rig)" >&2; exit 2; }
TS=$(date -u +%Y%m%d-%H%M%S)
OUT="${OUT:-/scratch/tmp/sym-box/brackets-$TS}"
GATES="${GATES:-tarx scale mwscale shared-dir foreign-touch walls readers walls32}"
REPEATS="${REPEATS:-2}"
TAR_SRC="${TAR_SRC:-${SQZ_MWMATRIX_TAR_SRC:-}}"
STATE="${SQZ_MWFLEET_STATE_DIR:-/run/squeezefs-mwfleet}"
SMOKE="${SMOKE:-0}"
LEG_EXTRA="${LEG_EXTRA:-}"
FLEET_A_WRITERS="${FLEET_A_WRITERS:-7}"
FLEET_B_READERS="${FLEET_B_READERS:-31}"
FLEET_C_WRITERS="${FLEET_C_WRITERS:-31}"
FLEET_M_COWRITERS="${FLEET_M_COWRITERS:-7}"
# The zram algorithm: squeeze-test's sqz kernel offers lzo-rle / lzo only
# (the D-5 fleet ran `lzo-rle` there); the laptop has zstd. The devsub's
# default is zstd, so the box run names its algorithm — refused loud by
# the devsub otherwise ("zram algorithm 'zstd' unavailable").
export SQZ_BIN="$BIN" SQZ_MWFLEET_OSS_GB="${SQZ_MWFLEET_OSS_GB:-16}" SQZ_DEVSUB_OSS_ALGO="${SQZ_DEVSUB_OSS_ALGO:-zstd}"
mkdir -p "$OUT"
TAG="sym-box"; [ "$SMOKE" = "1" ] && TAG="sym-box SMOKE"
log() { echo "[$TAG] $*" | tee -a "$OUT/box.log"; }
die() { echo "[$TAG] ERROR: $*" | tee -a "$OUT/box.log" >&2; exit 1; }

# ---- preflight ---------------------------------------------------------------
[ "$(id -u)" -eq 0 ] || die "root required (the fleet mounts, the stats inodes)"
case "$(uname -r)" in
*sqz*) ;;
*) [ "$SMOKE" = "1" ] || die "the running kernel is $(uname -r), not the sqz series — FUSE-over-io_uring cannot arm here (the 2026-09-22 finding: squeeze-test booted into the DDN Lustre kernel; restoring the grub default to vmlinuz-6.19.14-sqz and rebooting is the OWNER's act); SMOKE=1 for a laptop plumbing run" ;;
esac
[ -x "$BIN" ] || die "BIN $BIN is not executable"
# `env -u`: the daemon's knob gate announces every unregistered SQZ_* name
# on stderr — SQZ_BIN / SQZ_MWFLEET_* are the RIG's words (the fleet scrubs
# them before it launches a daemon), so the identity read runs without them.
env -u SQZ_BIN -u SQZ_MWFLEET_OSS_GB -u SQZ_DEVSUB_OSS_ALGO "$BIN" --version | grep -q "profile" || die "$BIN does not name its profile — not a squeezefs binary?"
[ -f "$STATE/members.tsv" ] && die "a fleet already exists at $STATE — tear it down first (sudo $FLEET teardown)"
if pgrep -x cargo >/dev/null 2>&1 || pgrep -x rustc >/dev/null 2>&1 || pgrep -x fio >/dev/null 2>&1; then
    [ "$SMOKE" = "1" ] || die "the box is not quiet (cargo / rustc / fio running)"
fi
load1="$(awk '{print int($1)}' /proc/loadavg)"
[ "$load1" -le 2 ] || [ "$SMOKE" = "1" ] || die "loadavg $(cut -d' ' -f1-3 /proc/loadavg) — a measured row needs a quiet box (load1 ≤ 2)"
[ "$SMOKE" != "1" ] || [ -z "$LEG_EXTRA$([ "$FLEET_A_WRITERS" = 7 ] && [ "$FLEET_B_READERS" = 31 ] && [ "$FLEET_C_WRITERS" = 31 ] || echo x)" ] ||
    log "SMOKE: LEG_EXTRA='$LEG_EXTRA' writers=$FLEET_A_WRITERS readers=$FLEET_B_READERS storm-writers=$FLEET_C_WRITERS — plumbing only, no number stands"
[ "$SMOKE" = "1" ] || [ -z "$LEG_EXTRA" ] || die "LEG_EXTRA is a SMOKE lever (the acceptance shapes are the legs' defaults)"
[ "$SMOKE" = "1" ] || { [ "$FLEET_A_WRITERS" = 7 ] && [ "$FLEET_B_READERS" = 31 ] && [ "$FLEET_C_WRITERS" = 31 ] && [ "$FLEET_M_COWRITERS" = 7 ]; } || die "FLEET_{A,C}_WRITERS / FLEET_B_READERS / FLEET_M_COWRITERS are SMOKE levers (the acceptance shapes are 7 / 31 / 31 / 7)"
for t in nvme python3 cc tar dd; do command -v "$t" >/dev/null 2>&1 || die "missing tool: $t"; done
case " $GATES " in
*" tarx "*) { [ -n "$TAR_SRC" ] && [ -d "$TAR_SRC" ]; } || die "gate 2 (tarx) needs TAR_SRC=<linux>/fs (the real linux fs/ corpus — the box has no internet: ship the tree)" ;;
esac
[[ "$REPEATS" =~ ^[0-9]+$ ]] && [ "$REPEATS" -ge 1 ] || die "REPEATS takes a positive integer"

{
    echo "== sym-box brackets $TS host $(hostname) kernel $(uname -r) gates [$GATES] repeats $REPEATS out $OUT"
    echo "   B: $(env -u SQZ_BIN -u SQZ_MWFLEET_OSS_GB -u SQZ_DEVSUB_OSS_ALGO "$BIN" --version 2>/dev/null | head -1) sha256 $(sha256sum "$BIN" | cut -c1-16)"
    echo "   fleet: $FLEET (tcp devsub, OSS ${SQZ_MWFLEET_OSS_GB} GiB zram ${SQZ_DEVSUB_OSS_ALGO} per data volume); matrix: $MATRIX --venue=box"
    echo "   loadavg $(cut -d' ' -f1-3 /proc/loadavg); cpus $(nproc); mem $(awk '/MemTotal/ {printf "%.0f GiB", $2/1048576}' /proc/meminfo)"
    echo "   patch 0031 (per-queue bg budget) is part of the sqz series on this kernel: $(uname -v)"
} | tee -a "$OUT/box.log"

thermal() { for h in /sys/class/hwmon/hwmon*/temp*_input; do [ -r "$h" ] && echo "hwmon $(basename "$(dirname "$h")")/$(basename "$h")=$(cat "$h")"; done > "$1" 2>/dev/null; }
hottest() { sort -t= -k2 -n "$1" 2>/dev/null | tail -1 | sed 's/.*=//' | awk '{printf "%.0f", $1/1000}'; }

FLEET_SEQ=0
fleet_up() { # A|B|C
    local shape="$1" args
    FLEET_SEQ=$((FLEET_SEQ + 1))
    case "$shape" in
    A) args="N=2 --symmetric --writers=$FLEET_A_WRITERS --token-readers" ;;
    B) args="N=$((FLEET_B_READERS + 1)) --symmetric --writers=1 --token-readers" ;;
    C) args="N=1 --symmetric --writers=$FLEET_C_WRITERS" ;;
    M) args="N=1 --cowriters=$FLEET_M_COWRITERS" ;;
    *) die "fleet shape $shape" ;;
    esac
    log "fleet $shape: create $args $(date -u +%FT%TZ)"
    # shellcheck disable=SC2086 # deliberate word split of the create args
    bash "$FLEET" create $args >"$OUT/fleet-$shape-$FLEET_SEQ.create.log" 2>&1 ||
        die "fleet $shape create FAILED — tail: $(tail -5 "$OUT/fleet-$shape-$FLEET_SEQ.create.log")"
    log "fleet $shape up: $(grep -c . "$STATE/members.tsv") members"
}
fleet_down() { # A|B|C
    local shape="$1"
    log "fleet $shape: teardown $(date -u +%FT%TZ)"
    bash "$FLEET" teardown >"$OUT/fleet-$shape-$FLEET_SEQ.teardown.log" 2>&1 ||
        die "fleet $shape teardown FAILED (residue) — tail: $(tail -5 "$OUT/fleet-$shape-$FLEET_SEQ.teardown.log")"
    [ -f "$STATE/members.tsv" ] && die "residue after teardown of fleet $shape"
    log "fleet $shape torn down to zero residue"
}

# The legs' extra arguments per gate (the acceptance shapes of PR 13's
# record §3 — every leg prints its engagement beside its number).
leg_args() { # gate
    case "$1" in
    tarx) echo "sym-tarx" ;;
    scale) echo "sym-scale --scale-ns=1,2,4,8" ;;
    mwscale) echo "mw-scale --scale-ns=1,2,4,8" ;;
    shared-dir) echo "sym-shared-dir" ;;
    foreign-touch) echo "sym-foreign-touch" ;;
    walls) echo "sym-walls" ;;
    walls32) echo "sym-walls --walls-files=4" ;;
    readers) echo "sym-readers" ;;
    *) die "unknown gate '$1' (tarx scale mwscale shared-dir foreign-touch walls readers walls32)" ;;
    esac
}
run_leg() { # gate position
    local gate="$1" pos="$2" tag="$1-r$2" args rc=0 t0 t1
    args="$(leg_args "$gate") $LEG_EXTRA"
    rm -rf "$STATE/rows" 2>/dev/null || true
    mkdir -p "$STATE/rows"
    thermal "$OUT/$tag.thermal0"
    echo "-- leg $tag: run_mw_matrix.sh $args --venue=box loadavg=$(cut -d' ' -f1-3 /proc/loadavg) hottest=$(hottest "$OUT/$tag.thermal0")°C $(date -u +%FT%TZ)" | tee -a "$OUT/box.log"
    t0=$(date +%s)
    # shellcheck disable=SC2086 # deliberate word split of the leg args
    SQZ_MWMATRIX_TAR_SRC="$TAR_SRC" bash "$MATRIX" $args --venue=box >"$OUT/$tag.log" 2>&1 || rc=$?
    t1=$(date +%s)
    thermal "$OUT/$tag.thermal1"
    cut -d' ' -f1-3 /proc/loadavg >"$OUT/$tag.loadavg"
    dmesg -T 2>/dev/null | grep -iE "fuse|WARN|lockdep|BUG|nvme.*(error|reset|timeout)" | tail -5 >"$OUT/$tag.dmesg" || true
    mkdir -p "$OUT/$tag"
    cp -r "$STATE/rows/." "$OUT/$tag/" 2>/dev/null || true
    echo "   leg $tag rc=$rc wall=$((t1 - t0))s hottest=$(hottest "$OUT/$tag.thermal1")°C" | tee -a "$OUT/box.log"
    {
        echo "### $tag (rc=$rc, $((t1 - t0)) s)"
        grep -h -E "^(gate|== |deleted-stays-deleted|.*VERDICT|.*verdict)" "$OUT/$tag.log" 2>/dev/null | head -40
        for f in "$OUT/$tag"/*/*table* "$OUT/$tag"/*/*verdict*; do
            [ -f "$f" ] || continue
            echo "--- $(basename "$(dirname "$f")")/$(basename "$f")"
            cat "$f"
        done
        [ -s "$OUT/$tag/venue-attributed.txt" ] && { echo "--- venue-attributed (must be EMPTY on the box)"; cat "$OUT/$tag/venue-attributed.txt"; }
        grep -h "VENUE-ATTRIBUTED\|must stay 0\|ERROR" "$OUT/$tag.log" | head -10
        echo
    } >>"$OUT/SUMMARY.txt"
    # A RED leg on the box is a PRODUCT finding or a MISS — stop this
    # gate's pass (attribute from $OUT/$tag.log + the snapshots), run the
    # others.
    [ "$rc" = "0" ] || { log "leg $tag RED (rc=$rc) — the gate's pass stops here; see $OUT/$tag.log"; return 1; }
}

: >"$OUT/SUMMARY.txt"
[ "$SMOKE" != "1" ] || echo "SMOKE RUN on $(hostname) ($(uname -r)) — laptop PLUMBING evidence only; no number below stands (the venue law)" >>"$OUT/SUMMARY.txt"
FAILED=""
fleet_a_gates=""
fleet_b_gates=""
fleet_c_gates=""
fleet_m_gates=""
for g in $GATES; do
    case "$g" in
    readers) fleet_b_gates="$fleet_b_gates $g" ;;
    walls32) fleet_c_gates="$fleet_c_gates $g" ;;
    mwscale) fleet_m_gates="$fleet_m_gates $g" ;;
    *) fleet_a_gates="$fleet_a_gates $g" ;;
    esac
done
# One fleet per SHAPE by default; FRESH_FLEET_PER_LEG=1 recreates the
# fleet before EVERY leg (≈ 35 s on the box). The legs judge the must-
# stay-0 set on ABSOLUTE gauges at their entry, so a gauge one leg moved
# (the box's first pass: `appender_flush_ceiling_overruns` +1 during
# sym-scale) kills every later leg on the same fleet at its door — a fresh
# fleet per leg keeps each position's verdict its OWN.
FRESH_FLEET_PER_LEG="${FRESH_FLEET_PER_LEG:-0}"
run_shape() { # shape gates...
    local shape="$1" g i up=0
    shift
    for g in "$@"; do
        for ((i = 1; i <= REPEATS; i++)); do
            if [ "$up" = "1" ] && [ "$FRESH_FLEET_PER_LEG" = "1" ]; then
                fleet_down "$shape"
                up=0
            fi
            [ "$up" = "1" ] || { fleet_up "$shape"; up=1; }
            run_leg "$g" "$i" || { FAILED="$FAILED $g-r$i"; break; }
        done
    done
    [ "$up" = "1" ] && fleet_down "$shape"
    return 0
}
# shellcheck disable=SC2086 # deliberate word split of the gate lists
[ -z "$fleet_a_gates" ] || run_shape A $fleet_a_gates
# shellcheck disable=SC2086
[ -z "$fleet_b_gates" ] || run_shape B $fleet_b_gates
# shellcheck disable=SC2086
[ -z "$fleet_c_gates" ] || run_shape C $fleet_c_gates
# shellcheck disable=SC2086
[ -z "$fleet_m_gates" ] || run_shape M $fleet_m_gates
log "== done $(date -u +%FT%TZ) $OUT failed=[${FAILED# }]"
cat "$OUT/SUMMARY.txt"
[ -z "$FAILED" ] || { echo "RED legs:$FAILED (each stopped its gate's pass; attribute before any row is written)" >&2; exit 3; }
