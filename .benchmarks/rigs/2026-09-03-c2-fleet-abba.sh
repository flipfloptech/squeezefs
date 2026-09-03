#!/usr/bin/env bash
# C-2 field A-B-B-A: the D-1b/D-2 fleet row (8 co-writers x 24 concurrent
# `dd conv=fsync` streams, /dev/zero — the device term removed by design)
# on the LOCAL box, A = the dev tip (the D-2 shape: both conveyor stages on
# the shared sqz-meta pool, journal writes through the uring_fs pool), B =
# the C-2 branch (per-volume journal lane owning the volume's journal ring).
#
# Runs .benchmarks/rigs/2026-09-02-d1b-fleet-row.sh verbatim (one fleet at
# a time, torn down to zero residue between legs) and the C-2 analysis over
# its snapshots. Refuses on a busy box (task check / cargo / a live fleet).
#
# Usage (root):
#   A_BIN=/path/squeezefs.dev B_BIN=/path/squeezefs.c2 \
#     sudo -n -E env "PATH=$PATH" bash .benchmarks/rigs/2026-09-03-c2-fleet-abba.sh
set -eu
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
: "${A_BIN:?A_BIN (the dev-tip control binary) is required}"
: "${B_BIN:?B_BIN (the C-2 binary) is required}"
export OUT="${OUT:-$REPO/target/c2-fleet-abba}"
export COWRITERS="${COWRITERS:-8}" STREAMS="${STREAMS:-24}" MB="${MB:-128}"
mkdir -p "$OUT"
{
    echo "== C-2 fleet A-B-B-A $(date -Is) on $(hostname), $(nproc) cpus, kernel $(uname -r)"
    echo "A: $("$A_BIN" --version | head -1)"
    echo "B: $("$B_BIN" --version | head -1)"
    echo "loadavg at start: $(cut -d' ' -f1-3 /proc/loadavg)"
    echo "foreign squeezefs mounts: $(mount | grep -cE 'sqz|squeezefs' || true)"
} | tee "$OUT/box.log"
if [ -f "${SQZ_MWFLEET_STATE_DIR:-/run/squeezefs-mwfleet}/members.tsv" ]; then
    echo "a fleet already exists — refusing (one fleet at a time)" >&2
    exit 1
fi
bash "$REPO/.benchmarks/rigs/2026-09-02-d1b-fleet-row.sh" 2>&1 | tee "$OUT/rig.log"
echo "loadavg at end: $(cut -d' ' -f1-3 /proc/loadavg)" | tee -a "$OUT/box.log"
python3 "$REPO/.benchmarks/rigs/2026-09-03-c2-fleet-analyze.py" "$OUT" | tee "$OUT/analysis.txt"
