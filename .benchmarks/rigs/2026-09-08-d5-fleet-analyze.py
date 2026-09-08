#!/usr/bin/env python3
"""D-5 fleet-row analysis: the owner-dispatch verdict columns read off the
stats snapshots the D-1b fleet rig (.benchmarks/rigs/2026-09-02-d1b-fleet-row.sh)
writes per leg (<out>/<leg>/m0_p0.json, m0_p1.json, m<i>_p{0,1}.json, wall).

    python3 .benchmarks/rigs/2026-09-08-d5-fleet-analyze.py <out-dir> [legs...]

Per leg (the authority, m0): the D-5 split `meta_ship_owner_dispatch_ns`
(queue_hop / run / wake_hop / total — exact means, Δsum ÷ Δcount, plus the
bucket-resolution p99 of `total`: the upper bound of the power-of-two
bucket where the cumulative count crosses 99 %), the venue engagement
(`meta_ship.owner_dispatch_inline` vs `owner_dispatch_hops` — one of them
carries every dispatch), the S8 frame's `meta_ship_owner_phase_ns.dispatch`
mean (the 2.0–2.3 ms term C-2 attributed), verbs/s per authority (publish
calls + S8 verbs over the wall), the conveyor's rho(apply) and pass means
(the D-1c columns, so the two rungs read side by side), CPU by class, and
the co-writers' publish latency + frame RTT + the ledger closure.
"""
import glob
import json
import re
import sys

BUCKET_LABELS = [
    "<=1us", "<=2us", "<=4us", "<=8us", "<=16us", "<=32us", "<=64us", "<=128us", "<=256us",
    "<=512us", "<=1024us", "<=2ms", "<=4ms", "<=8ms", "<=16ms", "<=32ms", "<=64ms", "<=128ms",
    "<=256ms", "<=512ms", "<=1024ms", "<=2s", "<=4s", "<=8s", "<=16s", ">16s",
]


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


def bucket_p99(a, b, fam, ph):
    counts = [delta(a, b, f"{fam}.{ph}.{lab}") for lab in BUCKET_LABELS]
    total = sum(counts)
    if not total:
        return "—"
    acc = 0
    for lab, c in zip(BUCKET_LABELS, counts):
        acc += c
        if acc >= 0.99 * total:
            return lab
    return BUCKET_LABELS[-1]


def leg(out, name):
    d = f"{out}/{name}"
    a, b = load(f"{d}/m0_p0.json"), load(f"{d}/m0_p1.json")
    t0, t1 = map(float, open(f"{d}/wall").read().split())
    wall = t1 - t0
    served = delta(a, b, "meta_ship_publish.served")
    verbs = delta(a, b, "meta_ship.served_verbs")
    sframes = delta(a, b, "meta_ship_publish.served_frames")
    inline = delta(a, b, "meta_ship.owner_dispatch_inline")
    hops = delta(a, b, "meta_ship.owner_dispatch_hops")
    refusals = delta(a, b, "meta_ship_publish.refusals")
    panics = delta(a, b, "meta_ship_publish.owner_panics") + delta(a, b, "meta_ship.owner_panics")
    fam = "meta_ship_owner_dispatch_ns"
    disp = {ph: mean_us(a, b, fam, ph) for ph in ("queue_hop", "run", "wake_hop", "total")}
    s8, s8n = mean_us(a, b, "meta_ship_owner_phase_ns", "dispatch")
    s8e, _ = mean_us(a, b, "meta_ship_owner_phase_ns", "execute")
    s8t, _ = mean_us(a, b, "meta_ship_owner_phase_ns", "total")
    tx = "meta_txpass_phase_ns"
    rho = delta(a, b, f"{tx}.pass_total.sum_ns") / 1e9 / wall
    q, _ = mean_us(a, b, tx, "tx_queue_wait")
    pt, _ = mean_us(a, b, tx, "pass_total")
    passes = delta(a, b, "meta_conveyor_leader_passes")
    cpu = delta(a, b, "daemon_cpu_ns") / 1e9
    cls = {k.split(".")[-1]: delta(a, b, k) / 1e9 for k in b if k.startswith("daemon_cpu_ns_by_class.")}
    venue = "INLINE" if inline and not hops else ("HOP" if hops and not inline else f"MIXED inline={inline} hops={hops}")
    print(f"== {name}: wall {wall:.1f} s, authority CPU {cpu:.2f} s "
          f"(sqz-meta {cls.get('sqz-meta', 0):.2f}, sqz-jrnl {cls.get('sqz-jrnl', 0):.2f}, "
          f"other incl. RPC lanes {cls.get('other', 0):.2f})")
    print(f"  VENUE: {venue} — dispatches {disp['total'][1]} (inline {inline}, hops {hops}); "
          f"refusals {refusals}, owner panics {panics}")
    print("  RUNG: owner_dispatch_ns " + ", ".join(f"{ph} {m:.0f} us" for ph, (m, _) in disp.items())
          + f"; total p99 bucket {bucket_p99(a, b, fam, 'total')}")
    print(f"  S8 frame: owner_phase_ns.dispatch {s8:.0f} us (n={s8n}), execute {s8e:.0f} us, total {s8t:.0f} us")
    print(f"  conveyor: rho(apply) {rho:.3f}, passes {passes} ({passes / sframes if sframes else 0:.3f}/served frame), "
          f"tx_queue_wait {q:.0f} us, pass_total {pt:.0f} us")
    print(f"  authority: served publishes {served}, S8 verbs {verbs} -> {(served + verbs) / wall:.0f} verbs/s")
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
              f"shipped {pubs} (closure vs served: {served - pubs:+d})")


if __name__ == "__main__":
    out = sys.argv[1]
    legs = sys.argv[2:] or ["L1a", "L0a", "L0b", "L1b"]
    for l in legs:
        try:
            leg(out, l)
        except FileNotFoundError as e:
            print(f"== {l}: missing ({e})")
