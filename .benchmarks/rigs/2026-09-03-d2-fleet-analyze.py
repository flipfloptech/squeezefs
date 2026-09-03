#!/usr/bin/env python3
"""D-2 fleet-row analysis: the authority's conveyor shape + the co-writers'
publish latency, read off the stats snapshots the D-1b fleet rig
(.benchmarks/rigs/2026-09-02-d1b-fleet-row.sh) writes per leg
(<out>/<leg>/m0_p0.json, m0_p1.json, m<i>_p{0,1}.json, wall).

    python3 .benchmarks/rigs/2026-09-03-d2-fleet-analyze.py <out-dir> [legs...]

Per leg: rho = sum(pass_total) / wall (the SERIALIZED apply server's
utilization), tx_queue_wait / pass_total / journal_ring_write /
window_lane_wait / window_total means (exact sum/count — audit A1),
leader vs durability passes, windows_inflight high-water, journal entries
per served publish, leaf-lock hold tail, and the co-writers'
publish_phase_ns.total + meta_ship_phase_ns.rtt means.
"""
import json
import sys


def flat(x, out=None, p=""):
    out = {} if out is None else out
    for k, v in x.items():
        if isinstance(v, dict):
            flat(v, out, p + k + ".")
        else:
            out[p + k] = v
    return out


def load(path):
    root = json.load(open(path))
    return flat(root.get("metrics", root))


def delta(a, b, k):
    return int(b.get(k, 0) or 0) - int(a.get(k, 0) or 0)


def mean_us(a, b, fam, ph):
    s = delta(a, b, f"{fam}.{ph}.sum_ns")
    c = delta(a, b, f"{fam}.{ph}.count")
    return (s / c / 1e3) if c else 0.0, c


def leg(out, name):
    d = f"{out}/{name}"
    a, b = load(f"{d}/m0_p0.json"), load(f"{d}/m0_p1.json")
    t0, t1 = map(float, open(f"{d}/wall").read().split())
    wall = t1 - t0
    passes = delta(a, b, "meta_conveyor_leader_passes")
    dpasses = delta(a, b, "meta_conveyor_durability_passes")
    hwm = int(b.get("meta_conveyor_windows_inflight_hwm", 0) or 0)
    served = delta(a, b, "meta_ship_publish.served")
    entries = delta(a, b, "meta_kv_journal_entries")
    rho = delta(a, b, "meta_txpass_phase_ns.pass_total.sum_ns") / 1e9 / wall
    fam = "meta_txpass_phase_ns"
    q, qn = mean_us(a, b, fam, "tx_queue_wait")
    pt, _ = mean_us(a, b, fam, "pass_total")
    ll, _ = mean_us(a, b, fam, "pass_leaf_locks")
    rw, _ = mean_us(a, b, fam, "journal_ring_write")
    pjw, _ = mean_us(a, b, fam, "pass_journal_write")
    lw, lwn = mean_us(a, b, fam, "window_lane_wait")
    wt, _ = mean_us(a, b, fam, "window_total")
    hold, holdn = mean_us(a, b, "lock_phase_ns", "leaf_lock_hold")
    # Tail of the leaf-lock hold: samples at/above 1 ms.
    labels = ["<=2ms", "<=4ms", "<=8ms", "<=16ms", "<=32ms", "<=64ms", "<=128ms",
              "<=256ms", "<=512ms", "<=1024ms", "<=2s", "<=4s", "<=8s", "<=16s", ">16s"]
    hold_tail = sum(delta(a, b, f"lock_phase_ns.leaf_lock_hold.buckets.{l}") for l in labels)
    cpu = delta(a, b, "daemon_cpu_ns") / 1e9
    print(f"== {name}: wall {wall:.1f} s, authority CPU {cpu:.2f} s")
    print(f"  conveyor: leader passes {passes}, durability passes {dpasses} "
          f"(windows/dpass {passes / dpasses if dpasses else 0:.2f}), windows hwm {hwm}, "
          f"rho(apply) {rho:.3f}")
    print(f"  tx_queue_wait {q:.0f} us (n={qn})  pass_total {pt:.0f}  pass_leaf_locks {ll:.0f}  "
          f"journal_ring_write {rw:.0f}  pass_journal_write {pjw:.0f}")
    print(f"  window_lane_wait {lw:.0f} us (n={lwn})  window_total {wt:.0f}  "
          f"commit latency ~ queue+window {q + wt:.0f} us")
    print(f"  leaf_lock_hold mean {hold:.1f} us over {holdn} (tail >=1 ms: {hold_tail})")
    print(f"  served publishes {served}, journal entries {entries} "
          f"({entries / served if served else 0:.3f}/publish), passes/publish "
          f"{passes / served if served else 0:.3f}")
    # Co-writers.
    import glob
    import re
    pubs, rtts, lat = 0, [], []
    for p0 in sorted(glob.glob(f"{d}/m*_p0.json")):
        i = re.search(r"m(\d+)_p0", p0).group(1)
        if i == "0":
            continue
        ca, cb = load(p0), load(f"{d}/m{i}_p1.json")
        t, n = mean_us(ca, cb, "publish_phase_ns", "total")
        r, rn = mean_us(ca, cb, "meta_ship_phase_ns", "rtt")
        if n:
            lat.append(t)
        if rn:
            rtts.append(r)
        pubs += delta(ca, cb, "meta_ship_publish.shipped")
    if lat:
        print(f"  co-writers: publish_phase_ns.total mean {sum(lat) / len(lat) / 1e3:.1f} ms, "
              f"meta_ship rtt mean {sum(rtts) / len(rtts) / 1e3 if rtts else 0:.2f} ms, "
              f"shipped {pubs}")


if __name__ == "__main__":
    out = sys.argv[1]
    legs = sys.argv[2:] or ["A1", "B1", "B2", "A2"]
    for l in legs:
        try:
            leg(out, l)
        except FileNotFoundError as e:
            print(f"== {l}: missing ({e})")
