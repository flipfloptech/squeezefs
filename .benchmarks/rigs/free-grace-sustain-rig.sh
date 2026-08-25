#!/usr/bin/env bash
# Free-grace sustain campaign column harvester
# (docs/design-free-grace-sustain.md PR 1, §8; finding 15 part 2).
#
# Reads a PAIR of stats-inode snapshots (the mw matrix rows' mN_pK.json
# shape — nested under "metrics") or live mount paths, and prints the
# sustain columns per member:
#
#   bound_age_ms (the loop-latency instrument, p1 gauge) |
#   residence p50/p90 bucket (from the free_grace_residence_ms histogram
#   delta — the T8 phase-model adjudicator, capture step γ) |
#   demand_waits_d (site-0 observations, capture step β) |
#   deferrals_d / releases_d / offsets (closure: d ≡ r + held) |
#   alloc_fresh_d / alloc_freelist_d / harvested_d (the attribution
#   split, capture step α — which stream is recycle-bound) |
#   lane_reachable (p1 gauge; absent = engagement-gated off) |
#   prods_d / tightenings_d / pressure_pct / demand-face (PR 3)
#
# Usage:
#   .benchmarks/rigs/free-grace-sustain-rig.sh p0.json p1.json [label]
#   .benchmarks/rigs/free-grace-sustain-rig.sh --live /mnt/sqz-mwfleet/m0
# The mw matrix rows produce the snapshot pairs; this rig only reads.
set -euo pipefail

if [ "${1:-}" = "--live" ]; then
    mnt=${2:?mount path}
    p1=$(mktemp)
    trap 'rm -f "$p1"' EXIT
    cat "$mnt/.stats" >"$p1"
    p0="$p1"
    label="${3:-$(basename "$mnt") (live, deltas read 0)}"
else
    p0=${1:?p0 snapshot json}
    p1=${2:?p1 snapshot json}
    label="${3:-$(basename "$p1")}"
fi

python3 - "$p0" "$p1" "$label" <<'PY'
import json, sys

p0, p1, label = sys.argv[1], sys.argv[2], sys.argv[3]

def load(path):
    root = json.load(open(path))
    return root.get("metrics", root)

a, b = load(p0), load(p1)

def num(d, key):
    v = d.get(key, 0)
    return v if isinstance(v, (int, float)) else 0

def delta(key):
    return int(num(b, key)) - int(num(a, key))

def hist_delta(key):
    h0, h1 = a.get(key, {}), b.get(key, {})
    if not isinstance(h1, dict):
        return {}
    return {
        k: int(h1.get(k, 0)) - int(h0.get(k, 0) if isinstance(h0, dict) else 0)
        for k in h1
        if int(h1.get(k, 0)) - int(h0.get(k, 0) if isinstance(h0, dict) else 0) > 0
    }

def pctl(hist, q):
    total = sum(hist.values())
    if total == 0:
        return "-"
    # Buckets are labeled and ordered by the latency core; order by the
    # p1 snapshot's own key order (json preserves it).
    run = 0
    for k, v in hist.items():
        run += v
        if run * 100 >= total * q:
            return k
    return "-"

res = hist_delta("free_grace_residence_ms")
deferrals, releases = delta("free_grace_deferrals"), delta("free_grace_releases")
held = int(num(b, "free_grace_offsets"))
closure = "OK" if deferrals == releases + (held - int(num(a, "free_grace_offsets"))) else "SPLIT"
reachable = b.get("alloc_lane_reachable_blocks")

print(f"== free-grace sustain columns: {label}")
print(f"bound_age_ms      {int(num(b, 'free_grace_bound_age_ms'))}")
print(f"residence p50/p90 {pctl(res, 50)} / {pctl(res, 90)}  (samples_d {sum(res.values())})")
print(f"demand_waits_d    {delta('free_grace_demand_waits')}  (site 0 — the coupled statement)")
print(f"defer/rel/held    {deferrals} / {releases} / {held}  closure {closure}")
print(
    f"alloc split_d     fresh {delta('alloc_fresh_mints')} | freelist "
    f"{delta('alloc_from_freelist')} | harvested {delta('alloc_lane_harvested_blocks')}"
)
print(f"lane_reachable    {reachable if reachable is not None else 'absent (gate off)'}")
print(
    f"valve             prods_d {delta('free_grace_prods')} | tightenings_d "
    f"{delta('free_grace_bound_tightenings')} | pressure_pct {int(num(b, 'free_grace_pressure_pct'))}"
)
PY
