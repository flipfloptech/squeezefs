#!/usr/bin/env python3
"""D-1c fleet-row analysis: the conveyor-group-per-frame verdict columns read
off the stats snapshots the D-1b fleet rig
(.benchmarks/rigs/2026-09-02-d1b-fleet-row.sh) writes per leg
(<out>/<leg>/m0_p0.json, m0_p1.json, m<i>_p{0,1}.json, wall).

    python3 .benchmarks/rigs/2026-09-04-d1c-fleet-analyze.py <out-dir> [legs...]

Per leg (the authority, m0): served frames / conveyor passes and the rung's
number PASSES PER SERVED FRAME (→ 1 with the lever on; the C-2 venue ratio
with it off), the group ledger (group commits, member txs, live group size,
frame_groups vs served frames — the engagement), rho(apply) and the
meta_txpass_phase_ns means (tx_queue_wait / pass_total / window_total),
verbs/s per authority (publish calls + S8 verbs over the wall), journal
entries per served publish (unchanged by construction), CPU by class, and
the co-writers' publish latency + frame RTT. The C-2 analyzer
(2026-09-03-c2-fleet-analyze.py) keeps the journal-write hop decomposition;
run both on the same legs when the io-wq term matters.
"""
import glob
import json
import re
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
    served = delta(a, b, "meta_ship_publish.served")
    sframes = delta(a, b, "meta_ship_publish.served_frames")
    scalls = delta(a, b, "meta_ship_publish.served_frame_calls")
    schains = delta(a, b, "meta_ship_publish.served_chains")
    verbs = delta(a, b, "meta_ship.served_verbs")
    entries = delta(a, b, "meta_kv_journal_entries")
    has_ledger = "meta_conveyor_group_commits" in b
    gcommits = delta(a, b, "meta_conveyor_group_commits")
    gtxs = delta(a, b, "meta_conveyor_group_txs")
    fgroups = delta(a, b, "meta_ship_publish.frame_groups")
    fam = "meta_txpass_phase_ns"
    rho = delta(a, b, f"{fam}.pass_total.sum_ns") / 1e9 / wall
    q, qn = mean_us(a, b, fam, "tx_queue_wait")
    pt, _ = mean_us(a, b, fam, "pass_total")
    wt, _ = mean_us(a, b, fam, "window_total")
    g1 = delta(a, b, "meta_commit_group_size.1")
    gsum = sum(delta(a, b, k) for k in b if k.startswith("meta_commit_group_size."))
    cpu = delta(a, b, "daemon_cpu_ns") / 1e9
    cls = {k.split(".")[-1]: delta(a, b, k) / 1e9 for k in b if k.startswith("daemon_cpu_ns_by_class.")}
    # The cluster-wire RPC lanes (`sqz-cluster-svc{n}`) have no class of
    # their own in daemon_cpu.rs — they land in `other`.
    print(f"== {name}: wall {wall:.1f} s, authority CPU {cpu:.2f} s "
          f"(sqz-meta {cls.get('sqz-meta', 0):.2f}, sqz-jrnl {cls.get('sqz-jrnl', 0):.2f}, "
          f"other incl. RPC lanes {cls.get('other', 0):.2f})")
    print(f"  RUNG: served frames {sframes} ({scalls / sframes if sframes else 0:.2f} calls/frame, "
          f"{schains / sframes if sframes else 0:.2f} chains/frame), conveyor passes {passes} -> "
          f"PASSES PER SERVED FRAME {passes / sframes if sframes else 0:.3f} "
          f"(passes/publish {passes / served if served else 0:.3f})")
    if has_ledger:
        print(f"  groups: {gcommits} commits carrying {gtxs} txs (group size "
              f"{gtxs / gcommits if gcommits else 0:.2f}), frame_groups {fgroups} of {sframes} served frames "
              f"({'ENGAGED' if gcommits else 'lever OFF / not engaged'})")
    else:
        print("  groups: (no ledger on this binary — pre-D-1c control)")
    print(f"  conveyor: rho(apply) {rho:.3f}, size-1 batches {100 * g1 / gsum if gsum else 0:.0f} % of {gsum}, "
          f"tx_queue_wait {q:.0f} us (n={qn}), pass_total {pt:.0f} us, window_total {wt:.0f} us")
    print(f"  authority: served publishes {served}, S8 verbs {verbs} -> {(served + verbs) / wall:.0f} verbs/s; "
          f"journal entries {entries} ({entries / served if served else 0:.3f}/publish)")
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
    legs = sys.argv[2:] or ["L0a", "L1a", "L1b", "L0b"]
    for l in legs:
        try:
            leg(out, l)
        except FileNotFoundError as e:
            print(f"== {l}: missing ({e})")
