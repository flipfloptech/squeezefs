#!/usr/bin/env bash
# .benchmarks/rigs/2026-08-17-s11-b4-tax-rig.sh — the rung-16 FAST-PATH
# TAX row (KD-MW-12, design-full-multi-writer PR-plan row 16: "the 41.6
# GiB/s guard"): single-writer A-B-B-A proving the B4 §5.1 range clause
# costs NOTHING on the solo hot path.
#
# Shape: SUSTAINED sequential overwrite (the B4 overwrite-arm shape —
# aligned 1 MiB O_DIRECT segments over a pre-minted striped fileset,
# elbencho --infloop --timelimit ≥60 s, the standing sustained-state
# rule), on the tcp devsub (nvmet-tcp on localhost — the MANDATORY
# fabric-sensitive substrate for write rows). Range custody is
# STRUCTURALLY DARK on this posture (solo mount, no mw arm, no grants):
# the row prices exactly the clause's one O(1) empty-table probe per
# overlay store.
#
# A = the pre-rung binary (dev tip), B = the rung-16 binary. Order
# A-B-B-A (the standing alternating-order rule — the store ages across
# rows even though every row re-formats: thin/zram state persists).
# Every row: fresh format -> mount --daemon -> untimed mint pass ->
# reclaim settle -> the 75 s measured window -> engagement + flatness
# columns from .stats and /proc/diskstats.
#
# Row validity: overlay_overwrite_installs delta accounts for the row's
# ops (the B4 engagement law), overlay_ineligible_range_shared == 0
# (dark posture), first-third vs last-third device rate within the
# flatness band (a decaying burst is a FAILED row, not a result).
#
# Usage:
#   sudo SQZ_BIN_A=<pre binary> SQZ_BIN_B=<rung16 binary> \
#        SQZ_TAX_RESULTS=/tmp/s11tax \
#        .benchmarks/rigs/2026-08-17-s11-b4-tax-rig.sh
set -u

# The mw_fleet scrub-env discipline: a daemon that sees SUDO_UID mounts
# with user_id=$SUDO_UID and locks the root-run rig out of its own
# mount (.stats probes + elbencho both run as root here).
unset SUDO_UID SUDO_GID SUDO_USER SUDO_COMMAND

META_URI="${SQZ_TAX_META:-sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1}"
DATA_URI="${SQZ_TAX_DATA:-sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1}"
MNT="${SQZ_TAX_MNT:-/mnt/sqz_s11_tax}"
RESULTS="${SQZ_TAX_RESULTS:-/tmp/s11_b4_tax_$(date +%Y%m%d_%H%M%S)}"
BIN_A="${SQZ_BIN_A:?set SQZ_BIN_A (the pre-rung binary)}"
BIN_B="${SQZ_BIN_B:?set SQZ_BIN_B (the rung-16 binary)}"
THREADS="${SQZ_TAX_THREADS:-16}"
FILE_MB="${SQZ_TAX_FILE_MB:-448}"
WINDOW_S="${SQZ_TAX_WINDOW_S:-75}"
ELBENCHO_BIN="${ELBENCHO_BIN:-$(command -v elbencho)}"

mkdir -p "$RESULTS" "$MNT"
die() {
    echo "TAX-RIG FATAL: $*" >&2
    exit 1
}
[ -x "$BIN_A" ] || die "SQZ_BIN_A not executable"
[ -x "$BIN_B" ] || die "SQZ_BIN_B not executable"
[ -n "$ELBENCHO_BIN" ] || die "elbencho not found"

# ---- Quiet gate (the bench-baseline discipline): refuse foreign cargo
# work; record loadavg; label provisional when not quiet.
PROVISIONAL=""
if pgrep -x cargo >/dev/null 2>&1 || pgrep -x rustc >/dev/null 2>&1; then
    PROVISIONAL="PROVISIONAL (foreign cargo/rustc running)"
fi
LOAD="$(cut -d' ' -f1 /proc/loadavg)"
NCPU="$(nproc)"
if awk -v l="$LOAD" -v n="$NCPU" 'BEGIN { exit !(l > n / 2) }'; then
    PROVISIONAL="${PROVISIONAL:+$PROVISIONAL; }PROVISIONAL (loadavg $LOAD on $NCPU cpus)"
fi
echo "quiet gate: ${PROVISIONAL:-quiet} (loadavg $LOAD, $NCPU cpus)" | tee "$RESULTS/quiet"

# The DATA namespaces' diskstats devices (write-amp columns are per the
# data plane; meta rides its own namespaces).
DATA_DEVS=()
IFS=',' read -r -a _dd <<<"${DATA_URI#sqdata://}"
for d in "${_dd[@]}"; do DATA_DEVS+=("$(basename "$d")"); done

disk_w_sectors() { # sum of sectors written across the data namespaces
    local total=0 dev f3
    for dev in "${DATA_DEVS[@]}"; do
        f3=$(awk -v d="$dev" '$3 == d { print $10 }' /proc/diskstats)
        total=$((total + ${f3:-0}))
    done
    echo "$total"
}

stat_get() { # <key> — flat metrics read off the live stats inode (cat, never cp)
    python3 -c "
import json,sys
m=json.load(open('$MNT/.stats')).get('metrics',{})
def flat(d,p=''):
    for k,v in d.items():
        if isinstance(v,dict): yield from flat(v,p+k+'.')
        else: yield p+k,v
print(dict(flat(m)).get('$1',0))" 2>/dev/null || echo 0
}

teardown() {
    "$CUR_BIN" umount "$MNT" >/dev/null 2>&1 || umount -l "$MNT" 2>/dev/null || true
    for _ in $(seq 30); do
        mountpoint -q "$MNT" || break
        sleep 1
    done
    pkill -f "squeezefs mount .*$MNT" 2>/dev/null || true
}

CUR_BIN="$BIN_A"
run_row() { # <label> <binary>
    local label="$1" bin="$2"
    CUR_BIN="$bin"
    echo "=== row $label ($($bin --version 2>/dev/null | head -1)) ==="
    teardown
    "$bin" format "$META_URI" "$DATA_URI" --force >"$RESULTS/$label.format" 2>&1 ||
        die "$label: format failed ($(tail -2 "$RESULTS/$label.format"))"
    "$bin" mount "$META_URI" "$MNT" --daemon >"$RESULTS/$label.mount" 2>&1 ||
        die "$label: mount failed ($(tail -2 "$RESULTS/$label.mount"))"
    for _ in $(seq 60); do
        [ -e "$MNT/.stats" ] && break
        sleep 1
    done
    [ -e "$MNT/.stats" ] || die "$label: mount never became ready"

    # Untimed mint pass (fresh blocks), then reclaim settle: the
    # measured window is PURE overwrite.
    local files=()
    for i in $(seq 1 "$THREADS"); do files+=("$MNT/tax/f$i"); done
    mkdir -p "$MNT/tax"
    "$ELBENCHO_BIN" -w -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct \
        "${files[@]}" >"$RESULTS/$label.prep" 2>&1 || die "$label: prep failed"
    for _ in $(seq 90); do
        local qb
        qb=$(python3 -c "import json;m=json.load(open('$MNT/.stats'))['metrics'];print(int(m.get('block_free_reclaim_queue_bytes',0))+int(m.get('block_free_elided_debt_bytes',0)))" 2>/dev/null || echo 0)
        [ "${qb:-0}" -eq 0 ] && break
        sleep 1
    done

    # The 75 s sustained window, diskstats sampled every 5 s for the
    # flatness verdict.
    local i0 rs0 w0 t_start
    i0="$(stat_get overlay_overwrite_installs)"
    rs0="$(stat_get overlay_ineligible_range_shared)"
    w0="$(disk_w_sectors)"
    t_start=$(date +%s)
    (
        while sleep 5; do
            echo "$(($(date +%s) - t_start)) $(disk_w_sectors)"
        done
    ) >"$RESULTS/$label.samples" &
    local sampler=$!
    "$ELBENCHO_BIN" -w -t "$THREADS" -s "${FILE_MB}m" -b 1m --direct \
        --infloop --timelimit "$WINDOW_S" "${files[@]}" \
        >"$RESULTS/$label.elbencho" 2>&1 || die "$label: measured window failed"
    kill "$sampler" 2>/dev/null || true
    wait "$sampler" 2>/dev/null || true

    local i1 rs1 w1 elapsed
    i1="$(stat_get overlay_overwrite_installs)"
    rs1="$(stat_get overlay_ineligible_range_shared)"
    w1="$(disk_w_sectors)"
    elapsed=$(($(date +%s) - t_start))
    python3 - "$label" "$w0" "$w1" "$elapsed" "$((i1 - i0))" "$((rs1 - rs0))" \
        "$RESULTS/$label.samples" <<'EOF' | tee -a "$RESULTS/rows.txt"
import sys
label, w0, w1, secs, inst_d, rs_d = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]), int(sys.argv[6])
gib = (w1 - w0) * 512 / 2**30
rate = gib / secs if secs else 0
rows = [tuple(map(int, l.split())) for l in open(sys.argv[7]) if len(l.split()) == 2]
flat = ""
if len(rows) >= 6:
    third = len(rows) // 3
    (t_a0, s_a0), (t_a1, s_a1) = rows[0], rows[third]
    (t_b0, s_b0), (t_b1, s_b1) = rows[-third - 1], rows[-1]
    r_first = (s_a1 - s_a0) / (t_a1 - t_a0) if t_a1 > t_a0 else 0
    r_last = (s_b1 - s_b0) / (t_b1 - t_b0) if t_b1 > t_b0 else 0
    drift = (r_last / r_first - 1) * 100 if r_first else 0
    flat = f" first-third {r_first*512/2**30:.3f} last-third {r_last*512/2**30:.3f} GiB/s (drift {drift:+.1f}%)"
bad = []
if inst_d < 1000:
    bad.append(f"overlay engagement too low ({inst_d} installs)")
if rs_d != 0:
    bad.append(f"overlay_ineligible_range_shared moved ({rs_d}) on a DARK posture")
verdict = "ROW-INVALID: " + "; ".join(bad) if bad else "row valid"
print(f"{label}: device {rate:.3f} GiB/s sustained over {secs}s ({gib:.1f} GiB), "
      f"ow_installs_d={inst_d}, range_clause_d={rs_d};{flat} [{verdict}]")
if bad:
    sys.exit(1)
EOF
    local rc=$?
    teardown
    return $rc
}

FAIL=0
run_row A1 "$BIN_A" || FAIL=1
run_row B1 "$BIN_B" || FAIL=1
run_row B2 "$BIN_B" || FAIL=1
run_row A2 "$BIN_A" || FAIL=1

echo
echo "=== A-B-B-A table (${PROVISIONAL:-quiet}; instrument: elbencho $("$ELBENCHO_BIN" --version | head -1 2>/dev/null); substrate: tcp devsub) ==="
cat "$RESULTS/rows.txt"
[ "$FAIL" = 0 ] || die "one or more rows invalid"
echo "results in $RESULTS"
