#!/usr/bin/env python3
"""Per-second time series of a multi-writer fleet's `.stats` inodes.

    2026-09-07-fleet-sampler.py --out <dir> [--interval 1.0] [--glob '/mnt/sqz-mwfleet/m*']

Runs until SIGTERM/SIGINT. Every interval it reads each mount's `.stats`
(root — the inode is 0400 owned by the mount uid) and appends ONE JSON
line per mount to `<out>/<mount>.jsonl` carrying the wall clock, the
monotonic offset from the sampler's start, and the finding-15 columns:
the authority's grace loop (bound age, ring offsets/bytes, releases,
deferrals, the hold stages' running means, the pressure valve), each
co-writer's lane (reachable blocks, ENOSPC refusals, harvests + blocks,
the supply hint, the horizon), the rewrite epochs (open, parked bytes,
swaps, rewritten blocks), the shipped-free ledger and the write pipeline.
Missing keys are recorded as null, so a row from an older binary still
reduces. The end snapshot the row wrapper already captures is the
authoritative total; these lines are the SHAPE over time — the instrument
`.benchmarks/2026-09-06-free-grace-term1-fleet.md` §4 named as owed.

The read is what a stats poller costs the daemon (a JSON render, ~250 KB
per mount per second here); the reclaim manners law keys on device
bytes, so polling cannot hold a drain deferred.
"""
import argparse
import glob
import json
import os
import signal
import sys
import time

SCALARS = [
    # authority — the grace loop
    "free_grace_mode", "free_grace_bound_age_ms", "free_grace_hold_ms", "free_grace_offsets",
    "free_grace_bytes", "free_grace_deferrals", "free_grace_releases", "free_grace_forced_releases",
    "free_grace_laggard_fences", "free_grace_alloc_stalls", "free_grace_pressure_pct",
    "free_grace_prod_renew_ms", "free_grace_checkpoint_ceiling_ms", "free_grace_demand_waits",
    "free_grace_bound_tightenings", "alloc_lane_supply_blocks", "alloc_lane_release_marks",
    "free_grace_lane_push_releases", "meta_kv_checkpoints", "membership_renewals",
    # member — the ack ladder
    "free_grace_acked_lag_ms", "free_grace_qualify_lag_ms", "free_grace_drain_lag_ms",
    "free_grace_drain_observed", "free_grace_drain_overdue", "free_grace_pass_interval_ms",
    "free_grace_advertised_ceiling_ms", "meta_kv_revalidate_epochs", "meta_kv_revalidate_polls",
    # co-writer — the lane
    "alloc_lane_id", "alloc_lane_writers", "alloc_lane_reachable_blocks", "alloc_lane_enospc_refusals",
    "alloc_lane_harvests", "alloc_lane_harvested_blocks", "alloc_lane_pushed_harvests",
    "alloc_lane_ahead_harvests", "alloc_lane_hint_refills", "alloc_lane_owed_blocks",
    "alloc_lane_harvest_horizon_ms", "alloc_lane_harvest_watermark",
    "alloc_lane_headroom_pct", "free_grace_lane_supply_hint", "free_grace_lane_push_wakes",
    # rewrite epochs
    "rewrite_shadow_open_epochs", "rewrite_shadow_parked_bytes", "rewrite_shadow_swaps",
    "rewrite_shadow_fallbacks", "rewrite_shadow_superseded", "rewrite_blocks",
    "rewrite_shadow_supply_closes", "rewrite_shadow_supply_close_blocks",
    # write side + tripwires
    "write_pipeline_inflight_bytes", "write_pipeline_admission_waits", "write_through_blocks",
    "block_claim_anomalies", "invariant_tripwires", "writeback_errors_latched", "mount_posture",
]
# nested objects: (key, subkeys)
NESTED = [
    ("meta_ship_publish", ["free_shipped_blocks", "free_served_blocks", "free_recomputed_blocks",
                           "free_refused_blocks", "harvest_served_blocks", "harvest_shipped_blocks",
                           "served", "shipped"]),
    ("cowriter", ["free_ship_own_lane_untracked", "unpublished_recycles", "accounting_refusals"]),
    ("free_grace_member_ack_lag_ms", ["min", "mean", "max"]),
]
# phase histograms → running mean ms (sum/count)
PHASES = [
    ("free_grace_hold_phase_ns", ["defer_checkpointed", "checkpointed_min_acked", "min_acked_released", "total"]),
    ("alloc_lane_visible_phase_ns", ["released_served", "served_visible", "total"]),
]

STOP = False


def _stop(*_):
    global STOP
    STOP = True


def sample(path):
    with open(path) as f:
        m = json.load(f)["metrics"]
    out = {k: m.get(k) for k in SCALARS}
    for key, subs in NESTED:
        obj = m.get(key) or {}
        for s in subs:
            out[f"{key}.{s}"] = obj.get(s) if isinstance(obj, dict) else None
    for fam, phases in PHASES:
        h = m.get(fam) or {}
        for ph in phases:
            e = h.get(ph) if isinstance(h, dict) else None
            out[f"{fam}.{ph}.count"] = e.get("count") if isinstance(e, dict) else None
            out[f"{fam}.{ph}.sum_ns"] = e.get("sum_ns") if isinstance(e, dict) else None
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--interval", type=float, default=1.0)
    ap.add_argument("--glob", default="/mnt/sqz-mwfleet/m*")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    signal.signal(signal.SIGTERM, _stop)
    signal.signal(signal.SIGINT, _stop)
    t0 = time.monotonic()
    files = {}
    n = 0
    while not STOP:
        tick = time.monotonic()
        wall = time.time()
        for mnt in sorted(glob.glob(a.glob)):
            name = os.path.basename(mnt)
            stats = os.path.join(mnt, ".stats")
            try:
                row = sample(stats)
            except (OSError, ValueError, KeyError) as e:
                row = {"error": str(e)[:120]}
            row["t_wall"] = wall
            row["t_rel_s"] = round(tick - t0, 3)
            fh = files.get(name)
            if fh is None:
                fh = files[name] = open(os.path.join(a.out, f"{name}.jsonl"), "a")
            fh.write(json.dumps(row, separators=(",", ":")) + "\n")
            fh.flush()
        n += 1
        # sleep the remainder of the interval (never negative)
        time.sleep(max(0.0, a.interval - (time.monotonic() - tick)))
    for fh in files.values():
        fh.close()
    print(f"sampler: {n} passes over {len(files)} mounts into {a.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
