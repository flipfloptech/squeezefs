#!/usr/bin/env python3
"""Reduce a captured s11-mpiio fleet row (`/tmp/five/d4/run-keep.sh`'s KEEP
dir — `m*.stats.json` + `matrix.log`) to the finding-15 verdict columns.

    2026-09-06-free-grace-fleet-reduce.py <keep-dir>...

Per keep dir: the matrix gate line (sustained / NOT SUSTAINED and its
first→last-third means), the authority's free-grace loop (bound age, hold
mean, the `free_grace_hold_phase_ns` stage means, offsets/bytes held at
capture, the closure law `deferrals ≡ releases + offsets`, the tripwires
`forced_releases` / `laggard_fences` / `alloc_stalls`, per-member ack lag),
and per co-writer the lane exhaustion (`alloc_lane_enospc_refusals`), the
term-3 anomalies and the shipped-free residue. Same columns for every row,
so a before/after pair reads off one table.
"""
import glob
import json
import os
import re
import sys


def load(p):
    return json.load(open(p))["metrics"]


def phase_means_ms(fam):
    if not isinstance(fam, dict):
        return {}
    out = {}
    for ph, h in fam.items():
        if isinstance(h, dict) and h.get("count"):
            out[ph] = h["sum_ns"] / h["count"] / 1e6
    return out


def gate_line(keep):
    p = os.path.join(keep, "matrix.log")
    if not os.path.exists(p):
        return "(no matrix.log)"
    lines = [l.rstrip() for l in open(p, errors="replace") if re.search(r"SUSTAINED|GATE|PASS|FAIL", l)]
    return "\n   ".join(lines[-3:]) if lines else "(no gate line)"


def reduce(keep):
    print(f"\n== {keep}")
    print(f"   gate: {gate_line(keep)}")
    auth = os.path.join(keep, "m0.stats.json")
    if os.path.exists(auth):
        a = load(auth)
        hp = phase_means_ms(a.get("free_grace_hold_phase_ns"))
        lag = a.get("free_grace_member_ack_lag_ms") or {}
        deferrals, releases, offsets = (a.get(k, 0) for k in ("free_grace_deferrals", "free_grace_releases", "free_grace_offsets"))
        print(f"   authority: mode={a.get('free_grace_mode')} members={a.get('free_grace_members')} "
              f"bound_age_ms={a.get('free_grace_bound_age_ms')} hold_ms={a.get('free_grace_hold_ms')} "
              f"pressure_pct={a.get('free_grace_pressure_pct')} prod_renew_ms={a.get('free_grace_prod_renew_ms')} "
              f"checkpoint_ceiling_ms={a.get('free_grace_checkpoint_ceiling_ms', '—')}")
        print("   hold phases (mean ms): " + "  ".join(f"{k}={v:,.0f}" for k, v in sorted(hp.items())))
        print(f"   held at capture: offsets={offsets:,} bytes={a.get('free_grace_bytes', 0) / 2**30:.2f} GiB; "
              f"closure deferrals {deferrals:,} ≡ releases {releases:,} + offsets {offsets:,} → "
              f"{'OK' if deferrals == releases + offsets else 'BROKEN'}")
        print(f"   tripwires: forced_releases={a.get('free_grace_forced_releases')} laggard_fences={a.get('free_grace_laggard_fences')} "
              f"alloc_stalls={a.get('free_grace_alloc_stalls')} demand_waits={a.get('free_grace_demand_waits')} "
              f"tightenings={a.get('free_grace_bound_tightenings')} prods={a.get('free_grace_prods')}")
        print(f"   member ack lag ms: min={lag.get('min')} mean={lag.get('mean')} max={lag.get('max')} "
              f"(members {lag.get('members')}); reader_acks={a.get('free_grace_reader_acks')} "
              f"meta_kv_checkpoints={a.get('meta_kv_checkpoints', '—')} membership_renewals={a.get('membership_renewals', '—')}")
    cws = sorted(glob.glob(os.path.join(keep, "m[1-9]*.stats.json")),
                 key=lambda p: int(re.search(r"m(\d+)", os.path.basename(p)).group(1)))
    if cws:
        print("   | mount | lane | ENOSPC refusals | headroom % | claim anomalies | own_lane_untracked | lane visible total ms (n) | fsync failures |")
        print("   |---|---|---|---|---|---|---|---|")
        tot_enospc = 0
        for p in cws:
            c = load(p)
            name = os.path.basename(p).split(".")[0]
            vis = phase_means_ms(c.get("alloc_lane_visible_phase_ns"))
            n = (c.get("alloc_lane_visible_phase_ns") or {}).get("total", {}).get("count", 0) if isinstance(c.get("alloc_lane_visible_phase_ns"), dict) else 0
            cw = c.get("cowriter") or {}
            enospc = c.get("alloc_lane_enospc_refusals", 0)
            tot_enospc += enospc
            print(f"   | {name} | {c.get('alloc_lane_id')}/{c.get('alloc_lane_writers')} | {enospc:,} | "
                  f"{c.get('alloc_lane_headroom_pct', '—')} | {c.get('block_claim_anomalies', 0)} | "
                  f"{cw.get('free_ship_own_lane_untracked', 0)} | {vis.get('total', 0):,.0f} ({n:,}) | "
                  f"{c.get('writeback_errors_latched', 0)} |")
        print(f"   Σ lane ENOSPC refusals across co-writers: {tot_enospc:,}")


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    for keep in sys.argv[1:]:
        reduce(keep.rstrip("/"))


if __name__ == "__main__":
    main()
