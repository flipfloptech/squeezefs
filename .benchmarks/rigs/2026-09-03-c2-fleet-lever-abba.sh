#!/usr/bin/env bash
# C-2 same-binary LEVER bracket: the D-1b/D-2 fleet row on ONE binary (the
# C-2 branch) with SQUEEZEFS_JOURNAL_LANE=0 (the shipped D-2 shape: both
# conveyor stages on the sqz-meta pool, journal writes via the uring_fs
# pool) vs =1 (the per-volume journal lane), A-B-B-A over the lever — the
# build-noise-free attribution beside the two-binary bracket
# (2026-09-03-c2-fleet-abba.sh). The daemons inherit SQUEEZEFS_* from this
# environment (tests/mw_fleet.sh scrubs only the SQZ_* rig variables).
#
# Usage (root):
#   BIN=/path/squeezefs.c2 sudo -n -E env "PATH=$PATH" \
#     bash .benchmarks/rigs/2026-09-03-c2-fleet-lever-abba.sh
set -eu
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
: "${BIN:?BIN (the C-2 binary) is required}"
FLEET="$REPO/tests/mw_fleet.sh"
ROW="$REPO/.benchmarks/rigs/2026-09-02-d1b-fleet-row.sh"
export COWRITERS="${COWRITERS:-8}" STREAMS="${STREAMS:-24}" MB="${MB:-128}"
OSS_GB="${SQZ_MWFLEET_OSS_GB:-64}"
TOP="${OUT:-$REPO/target/c2-fleet-lever-abba}"
mkdir -p "$TOP"
STATE="${SQZ_MWFLEET_STATE_DIR:-/run/squeezefs-mwfleet}"
[ -f "$STATE/members.tsv" ] && { echo "a fleet already exists — refusing" >&2; exit 1; }
if pgrep -f "task check" >/dev/null || pgrep -x cargo >/dev/null; then
    echo "the box is not quiet (task check / cargo running)" >&2
    exit 1
fi
echo "== C-2 lever A-B-B-A $(date -Is): $("$BIN" --version | head -1); loadavg $(cut -d' ' -f1-3 /proc/loadavg)" | tee "$TOP/box.log"
leg() { # label lane
    local label="$1" lane="$2"
    echo "[c2-lever] == leg $label: SQUEEZEFS_JOURNAL_LANE=$lane; loadavg $(cut -d' ' -f1-3 /proc/loadavg)" | tee -a "$TOP/box.log"
    SQUEEZEFS_JOURNAL_LANE="$lane" SQZ_BIN="$BIN" SQZ_MWFLEET_OSS_GB="$OSS_GB" \
        bash "$FLEET" create N=1 --multi-writer --cowriters="$COWRITERS" \
        >"$TOP/$label.create.log" 2>&1 || { echo "fleet create failed for $label" >&2; exit 1; }
    mkdir -p "$TOP/$label"
    # ROW_ONLY writes <OUT>/row/*; lift it to <TOP>/<label>/ for the analyzer.
    OUT="$TOP/$label.rowout" ROW_ONLY=1 bash "$ROW" >"$TOP/$label.row.log" 2>&1 || {
        cat "$TOP/$label.row.log" >&2
        bash "$FLEET" teardown >"$TOP/$label.teardown.log" 2>&1 || true
        exit 1
    }
    cp -r "$TOP/$label.rowout/row/." "$TOP/$label/"
    SQZ_BIN="$BIN" bash "$FLEET" teardown >"$TOP/$label.teardown.log" 2>&1 || { echo "teardown failed for $label" >&2; exit 1; }
    [ -f "$STATE/members.tsv" ] && { echo "residue after teardown" >&2; exit 1; }
    echo "[c2-lever] leg $label torn down to zero residue"
}
leg L0a 0
leg L1a 1
leg L1b 1
leg L0b 0
echo "loadavg at end: $(cut -d' ' -f1-3 /proc/loadavg)" | tee -a "$TOP/box.log"
for l in L0a L1a L1b L0b; do echo "--- $l"; grep -E "aggregate|owner :" "$TOP/$l/table.txt"; done
python3 "$REPO/.benchmarks/rigs/2026-09-03-c2-fleet-analyze.py" "$TOP" L0a L1a L1b L0b | tee "$TOP/analysis.txt"
