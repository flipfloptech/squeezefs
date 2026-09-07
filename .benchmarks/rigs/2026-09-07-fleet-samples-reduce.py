#!/usr/bin/env python3
"""Reduce a fleet row's per-second samples (`2026-09-07-fleet-sampler.py`)
into the finding-15 time series, aligned to the ior iteration boundaries.

    2026-09-07-fleet-samples-reduce.py <keep-dir> [--step 5] [--lane m50]

`<keep-dir>/samples/m*.jsonl` are the sampler's lines; `<keep-dir>/rows/
s11mpiio-*/A1.out` (or `ior-A1.out`) gives the iteration boundaries
(StartTime + the cumulative per-iteration `total(s)`). Prints, every
`--step` seconds: the authority's grace loop (bound age, offsets held,
release RATE per second, the ring's pressure, the harvest-serve rate
across all lanes), and one co-writer lane's view (reachable blocks,
parked bytes + open epochs, ENOSPC refusals and harvested blocks per
interval, the supply hint the authority last advertised for its lane).
An `|` in the first column marks a sample interval containing an ior
iteration boundary. Rates are deltas of monotone counters divided by the
interval — the columns the end snapshot cannot show.
"""
import argparse
import datetime as dt
import glob
import json
import os
import re
import sys


def load(path):
    rows = []
    for line in open(path):
        line = line.strip()
        if line:
            rows.append(json.loads(line))
    return rows


def boundaries(keep):
    cands = glob.glob(os.path.join(keep, "rows", "s11mpiio-*", "A1.out")) + glob.glob(os.path.join(keep, "ior-A1.out"))
    if not cands:
        return []
    txt = open(cands[0], errors="replace").read()
    m = re.search(r"StartTime\s*:\s*(.+)", txt)
    if not m:
        return []
    start = dt.datetime.strptime(m.group(1).strip(), "%a %b %d %H:%M:%S %Y").timestamp()
    out, t = [], start
    for line in txt.splitlines():
        if line.startswith("write "):
            cols = line.split()
            # ior columns: access bw IOPS Latency block xfer open wr/rd close total iter
            try:
                t += float(cols[9])
                out.append((t, int(cols[10]), float(cols[1])))
            except (ValueError, IndexError):
                pass
    return out


def d(cur, prev, key):
    a, b = cur.get(key), (prev or {}).get(key)
    return None if a is None or b is None else a - b


def fmt(v, f="{:.0f}"):
    return "—" if v is None else f.format(v)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("keep")
    ap.add_argument("--step", type=float, default=5.0)
    ap.add_argument("--lane", default="m50")
    a = ap.parse_args()
    sdir = os.path.join(a.keep, "samples")
    auth = load(os.path.join(sdir, "m0.jsonl"))
    lane = load(os.path.join(sdir, f"{a.lane}.jsonl"))
    if not auth:
        sys.exit(f"no samples under {sdir}")
    bounds = boundaries(a.keep)
    print(f"== {a.keep}: {len(auth)} authority samples, {len(lane)} {a.lane} samples, "
          f"{len(bounds)} ior iterations" + (f" ({bounds[0][2]:.0f} → {bounds[-1][2]:.0f} MiB/s)" if bounds else ""))
    print(f"   authority: bound_age | held offsets | releases/s | deferrals/s | pressure % | prod ms | "
          f"harvest-served blocks/s (all lanes)     {a.lane}: reachable | parked MiB | open epochs | "
          f"ENOSPC/int | harvested/int | swaps/int | supply hint | rewritten/int")
    prev_a = prev_l = None
    next_print = 0.0
    bi = 0
    for i, cur in enumerate(auth):
        t = cur["t_rel_s"]
        if t < next_print and i != len(auth) - 1:
            continue
        # a lane sample at (about) the same instant
        lcur = min(lane, key=lambda r: abs(r["t_rel_s"] - t)) if lane else {}
        mark = " "
        while bi < len(bounds) and bounds[bi][0] <= cur["t_wall"]:
            mark = "|"
            bi += 1
        dt_s = (t - prev_a["t_rel_s"]) if prev_a else None
        rate = lambda k, src, prv: (None if not dt_s or dt_s <= 0 else (d(src, prv, k) or 0) / dt_s)
        parked = lcur.get("rewrite_shadow_parked_bytes")
        print(f"{mark}{t:6.0f}s  {fmt(cur.get('free_grace_bound_age_ms')):>6} {fmt(cur.get('free_grace_offsets')):>6} "
              f"{fmt(rate('free_grace_releases', cur, prev_a)):>6} {fmt(rate('free_grace_deferrals', cur, prev_a)):>6} "
              f"{fmt(cur.get('free_grace_pressure_pct')):>4} {fmt(cur.get('free_grace_prod_renew_ms')):>5} "
              f"{fmt(rate('meta_ship_publish.harvest_served_blocks', cur, prev_a)):>6}      "
              f"{fmt(lcur.get('alloc_lane_reachable_blocks')):>6} {fmt(None if parked is None else parked / 2**20):>6} "
              f"{fmt(lcur.get('rewrite_shadow_open_epochs')):>4} {fmt(d(lcur, prev_l, 'alloc_lane_enospc_refusals')):>6} "
              f"{fmt(d(lcur, prev_l, 'alloc_lane_harvested_blocks')):>6} {fmt(d(lcur, prev_l, 'rewrite_shadow_swaps')):>4} "
              f"{fmt(lcur.get('free_grace_lane_supply_hint')):>5} {fmt(d(lcur, prev_l, 'rewrite_blocks')):>6}")
        prev_a, prev_l = cur, lcur
        next_print = t + a.step
    if bounds:
        print("   ior iterations (MiB/s): " + " ".join(f"{bw:.0f}" for _, _, bw in bounds))


if __name__ == "__main__":
    main()
