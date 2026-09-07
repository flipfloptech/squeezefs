#!/usr/bin/env bash
# The s11-mpiio fleet row with evidence capture AND the per-second sampler:
# fleet up → sampler on → s11-mpiio → snapshot every mount's .stats, the
# fleet's daemon logs, the ior outputs → sampler off → teardown.
#
#   sudo env KEEP=/tmp/five/d4/keep-<tag> [SQZ_BIN=...] [LEVERS=<label>] \
#        [SQUEEZEFS_<knob>=...] bash .benchmarks/rigs/2026-09-07-s11-fleet-row.sh
#
# Env passthrough (SQUEEZEFS_* / SQZ_*) reaches the daemons via mw_fleet's
# environment — that is how a lever's A/B leg is run. Reduce with
# `2026-09-06-free-grace-fleet-reduce.py <KEEP>` (the end snapshot) and
# `2026-09-07-fleet-samples-reduce.py <KEEP>` (the time series). Root:
# the fleet's mounts and the .stats inodes (0400, mount uid) require it.
set -u
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"
export PATH="/run/wrappers/bin:/run/current-system/sw/bin:$PATH"
KEEP=${KEEP:-/tmp/five/d4/keep-$(date +%H%M%S)}
SAMPLE_INTERVAL=${SAMPLE_INTERVAL:-1}
# The fleet's mount root (mw_fleet.sh honours the same variable; the box
# runs it under /scratch because its root fs is full).
MNT_ROOT=${SQZ_MWFLEET_MNT_ROOT:-/mnt/sqz-mwfleet}
export SQZ_MWFLEET_MNT_ROOT="$MNT_ROOT"
mkdir -p "$KEEP/samples"
echo "== $(date +%T) fleet create host=$(hostname) kernel=$(uname -r) load=$(cut -d' ' -f1-3 /proc/loadavg) cpus=$(nproc) env=[${LEVERS:-default}] bin=${SQZ_BIN:-target/release/squeezefs} oss_gb=${SQZ_MWFLEET_OSS_GB:-32} mnt=$MNT_ROOT"
SQZ_MWFLEET_OSS_GB=${SQZ_MWFLEET_OSS_GB:-32} SQZ_MWFLEET_RANGE_CUSTODY=${SQZ_MWFLEET_RANGE_CUSTODY:-1} \
    tests/mw_fleet.sh create N=1 --cowriters=8 2>&1 | tail -n 3
python3 .benchmarks/rigs/2026-09-07-fleet-sampler.py --out "$KEEP/samples" --interval "$SAMPLE_INTERVAL" \
    --glob "$MNT_ROOT/m*" 2> "$KEEP/sampler.log" &
SAMPLER=$!
echo "== $(date +%T) sampler pid $SAMPLER; s11-mpiio"
tests/run_mw_matrix.sh s11-mpiio > "$KEEP/matrix.log" 2>&1
rc=$?
grep -E "mwmatrix|WARNING|GATE|PASS|FAIL|MiB/s" "$KEEP/matrix.log" | tail -n 30
echo "matrix rc=$rc"
echo "== $(date +%T) capture"
for m in "$MNT_ROOT"/m*; do n=$(basename "$m"); cat "$m/.stats" > "$KEEP/$n.stats.json" 2>/dev/null; done
kill -TERM "$SAMPLER" 2>/dev/null; wait "$SAMPLER" 2>/dev/null
cat "$KEEP/sampler.log"
cp /run/squeezefs-mwfleet/m*.log "$KEEP/" 2>/dev/null
cp -r /run/squeezefs-mwfleet/rows "$KEEP/rows" 2>/dev/null
ls "$KEEP" | head -30
echo "== $(date +%T) teardown"
tests/mw_fleet.sh teardown 2>&1 | tail -n 2
[ -n "${SUDO_USER:-}" ] && chown -R "$SUDO_USER" "$KEEP" 2>/dev/null
echo "== done rc=$rc keep=$KEEP"
