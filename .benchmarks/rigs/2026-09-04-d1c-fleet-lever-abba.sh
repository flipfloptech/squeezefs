#!/usr/bin/env bash
# D-1c same-binary LEVER bracket: the D-1b/D-2/C-2 fleet row on ONE binary
# (the D-1c branch) with SQUEEZEFS_PUBLISH_CONVEYOR_GROUP=0 (the shipped
# D-1b shape: the owner dispatches a frame's chains concurrently and every
# call commits its own tx — the co-queue is arrival spread ÷ pass latency)
# vs =1 (one conveyor group per shipped frame: the round's layout publishes
# staged together and enqueued under one queue lock), A-B-B-A over the
# lever — the build-noise-free attribution (the 2026-09-03 C-2 lever rig's
# shape verbatim). The daemons inherit SQUEEZEFS_* from this environment
# (tests/mw_fleet.sh scrubs only the SQZ_* rig variables).
#
# Substrate: tcp devsub (MANDATORY for this fabric-sensitive write row —
# AGENTS.md two-substrate rule), 8 co-writers × 24 concurrent
# `dd bs=1M count=128 conv=fsync` streams from /dev/zero (the device term
# removed by design; the row is the metadata/publish-plane ceiling).
#
# Verdict columns (2026-09-04-d1c-fleet-analyze.py, per leg, authority m0):
#   passes/frame  = META_CONVEYOR_LEADER_PASSES delta / meta_ship_publish.served_frames delta
#                   (the rung: → 1 with the lever on; the venue ratio with it off)
#   group size    = meta_conveyor_group_txs / meta_conveyor_group_commits (0 off)
#   frame_groups  = meta_ship_publish.frame_groups delta (≈ served_frames on)
#   pass_total    = meta_txpass_phase_ns.pass_total mean + rho(apply)
#   ingest        = aggregate co-writer GiB/s (the row's headline, from the row rig)
#   verbs/s       = (meta_ship_publish.served + meta_ship.served_verbs) / wall per authority
#   daemon CPU    = daemon_cpu_ns delta (by class: sqz-meta / sqz-jrnl / other = incl. the RPC lanes)
#   journal entries per publish (unchanged by construction — one tx = one entry)
# Row validity is the row rig's: ledger closure served ≈ shipped, refusals =
# owner_panics = 0, every stream rc 0.
#
# Usage (root, quiet box; the parent runs this — never the implementer):
#   BIN=/path/squeezefs.d1c sudo -n -E env "PATH=$PATH" \
#     bash .benchmarks/rigs/2026-09-04-d1c-fleet-lever-abba.sh
set -eu
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
: "${BIN:?BIN (the D-1c binary) is required}"
FLEET="$REPO/tests/mw_fleet.sh"
ROW="$REPO/.benchmarks/rigs/2026-09-02-d1b-fleet-row.sh"
export COWRITERS="${COWRITERS:-8}" STREAMS="${STREAMS:-24}" MB="${MB:-128}" FILES="${FILES:-1}"
OSS_GB="${SQZ_MWFLEET_OSS_GB:-64}"
TOP="${OUT:-$REPO/target/d1c-fleet-lever-abba}"
mkdir -p "$TOP"
STATE="${SQZ_MWFLEET_STATE_DIR:-/run/squeezefs-mwfleet}"
[ -f "$STATE/members.tsv" ] && { echo "a fleet already exists — refusing" >&2; exit 1; }
if pgrep -f "task check" >/dev/null || pgrep -x cargo >/dev/null; then
    echo "the box is not quiet (task check / cargo running)" >&2
    exit 1
fi
"$BIN" --version | grep -q "profile" || { echo "$BIN does not name its profile — not a squeezefs binary?" >&2; exit 1; }
echo "== D-1c lever A-B-B-A $(date -Is): $("$BIN" --version | head -1); loadavg $(cut -d' ' -f1-3 /proc/loadavg)" | tee "$TOP/box.log"
leg() { # label lever
    local label="$1" lever="$2"
    echo "[d1c-lever] == leg $label: SQUEEZEFS_PUBLISH_CONVEYOR_GROUP=$lever; loadavg $(cut -d' ' -f1-3 /proc/loadavg)" | tee -a "$TOP/box.log"
    SQUEEZEFS_PUBLISH_CONVEYOR_GROUP="$lever" SQZ_BIN="$BIN" SQZ_MWFLEET_OSS_GB="$OSS_GB" \
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
    echo "[d1c-lever] leg $label torn down to zero residue"
}
leg L0a 0
leg L1a 1
leg L1b 1
leg L0b 0
echo "loadavg at end: $(cut -d' ' -f1-3 /proc/loadavg)" | tee -a "$TOP/box.log"
for l in L0a L1a L1b L0b; do echo "--- $l"; grep -E "aggregate|owner :" "$TOP/$l/table.txt"; done
python3 "$REPO/.benchmarks/rigs/2026-09-04-d1c-fleet-analyze.py" "$TOP" L0a L1a L1b L0b | tee "$TOP/analysis.txt"
