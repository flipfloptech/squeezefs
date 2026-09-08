#!/usr/bin/env bash
# D-5 same-binary LEVER bracket: the D-1b/D-2/C-2 fleet row on ONE binary
# (the campaign tip) with SQUEEZEFS_META_SHIP_INLINE_SERVE=1 (the shipped
# D-5 default: a served dispatch — S8 verb frame or S9 publish call/group/
# free/harvest — is polled on the accepting connection's own thread) vs =0
# (the pre-D-5 `spawn_meta_join` hop onto the shared sqz-meta lanes and
# back — C-2's fleet attribution read that hop at 2.0–2.3 ms per verb),
# A-B-B-A over the lever — the build-noise-free attribution, the 2026-09-04
# D-1c lever rig's shape verbatim. The daemons inherit SQUEEZEFS_* from
# this environment (tests/mw_fleet.sh scrubs only the SQZ_* rig variables).
#
# Substrate: tcp devsub (MANDATORY for this fabric-sensitive write row —
# AGENTS.md two-substrate rule), 8 co-writers × 24 concurrent
# `dd bs=1M count=128 conv=fsync` streams from /dev/zero (the device term
# removed by design; the row is the metadata/publish-plane ceiling).
#
# Verdict columns (2026-09-08-d5-fleet-analyze.py, per leg, authority m0):
#   dispatch      = meta_ship_owner_dispatch_ns.{queue_hop,run,wake_hop,total}
#                   exact means + bucket-resolution p99 of total (both planes)
#   engagement    = meta_ship.owner_dispatch_inline vs owner_dispatch_hops
#                   (inline ≡ dispatches on =1; hops carries them on =0)
#   S8 dispatch   = meta_ship_owner_phase_ns.dispatch mean (C-2's 2.0–2.3 ms term)
#   verbs/s       = (meta_ship_publish.served + meta_ship.served_verbs) / wall
#   ingest        = aggregate co-writer GiB/s (the row's headline, from the row rig)
#   daemon CPU    = daemon_cpu_ns delta by class (the RPC lanes land in `other`)
#   co-writers    = publish_phase_ns.total mean, meta_ship rtt mean, closure
# Row validity is the row rig's: ledger closure served ≈ shipped, refusals =
# owner_panics = 0, every stream rc 0.
#
# Usage (root, quiet box; the parent runs this — never the implementer):
#   BIN=/path/squeezefs sudo -n -E env "PATH=$PATH" \
#     bash .benchmarks/rigs/2026-09-08-d5-fleet-lever-abba.sh
set -eu
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
: "${BIN:?BIN (the campaign-tip binary) is required}"
FLEET="$REPO/tests/mw_fleet.sh"
ROW="$REPO/.benchmarks/rigs/2026-09-02-d1b-fleet-row.sh"
export COWRITERS="${COWRITERS:-8}" STREAMS="${STREAMS:-24}" MB="${MB:-128}" FILES="${FILES:-1}"
OSS_GB="${SQZ_MWFLEET_OSS_GB:-64}"
TOP="${OUT:-$REPO/target/d5-fleet-lever-abba}"
mkdir -p "$TOP"
STATE="${SQZ_MWFLEET_STATE_DIR:-/run/squeezefs-mwfleet}"
[ -f "$STATE/members.tsv" ] && { echo "a fleet already exists — refusing" >&2; exit 1; }
if pgrep -f "task check" >/dev/null || pgrep -x cargo >/dev/null; then
    echo "the box is not quiet (task check / cargo running)" >&2
    exit 1
fi
"$BIN" --version | grep -q "profile" || { echo "$BIN does not name its profile — not a squeezefs binary?" >&2; exit 1; }
echo "== D-5 lever A-B-B-A $(date -Is): $("$BIN" --version | head -1); loadavg $(cut -d' ' -f1-3 /proc/loadavg)" | tee "$TOP/box.log"
leg() { # label lever
    local label="$1" lever="$2"
    echo "[d5-lever] == leg $label: SQUEEZEFS_META_SHIP_INLINE_SERVE=$lever; loadavg $(cut -d' ' -f1-3 /proc/loadavg)" | tee -a "$TOP/box.log"
    SQUEEZEFS_META_SHIP_INLINE_SERVE="$lever" SQZ_BIN="$BIN" SQZ_MWFLEET_OSS_GB="$OSS_GB" \
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
    echo "[d5-lever] leg $label torn down to zero residue"
}
# LEGS = "<label>:<lever> ..." — the A-B-B-A order; the reverse bracket
# (0 1 1 0) is the second bracket's, so a position effect cannot pose as
# the lever's.
LEGS="${LEGS:-L1a:1 L0a:0 L0b:0 L1b:1}"
labels=()
for spec in $LEGS; do
    leg "${spec%%:*}" "${spec##*:}"
    labels+=("${spec%%:*}")
done
echo "loadavg at end: $(cut -d' ' -f1-3 /proc/loadavg)" | tee -a "$TOP/box.log"
for l in "${labels[@]}"; do echo "--- $l"; grep -E "aggregate|owner :" "$TOP/$l/table.txt"; done
python3 "$REPO/.benchmarks/rigs/2026-09-08-d5-fleet-analyze.py" "$TOP" "${labels[@]}" | tee "$TOP/analysis.txt"
