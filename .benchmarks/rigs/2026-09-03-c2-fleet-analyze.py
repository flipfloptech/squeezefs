#!/usr/bin/env python3
"""C-2 fleet-row analysis: the D-2 conveyor shape PLUS the journal write's
completion-hop decomposition and the journal-lane engagement, read off the
stats snapshots the D-1b fleet rig
(.benchmarks/rigs/2026-09-02-d1b-fleet-row.sh) writes per leg
(<out>/<leg>/m0_p0.json, m0_p1.json, m<i>_p{0,1}.json, wall).

    python3 .benchmarks/rigs/2026-09-03-c2-fleet-analyze.py <out-dir> [legs...]

Per leg (the authority, m0): rho(apply), tx_queue_wait / pass_total /
journal_ring_write (mean AND mode bucket) / window_lane_wait / window_total
means (exact sum/count — audit A1), the uring_fs_write_phase_ns split
(queue_hop / device / wake_hop / total), leader vs durability passes,
windows hwm, the commit-group size distribution (size-1 share), journal
entries per served publish, CPU by class (sqz-meta vs sqz-jrnl), the
journal-lane engagement pair (lane vs pool writes; a pre-C-2 control has
neither key), and the co-writers' publish latency + frame RTT.
"""
import glob
import json
import re
import sys

BUCKETS = ["<=1us", "<=2us", "<=4us", "<=8us", "<=16us", "<=32us", "<=64us", "<=128us",
           "<=256us", "<=512us", "<=1024us", "<=2ms", "<=4ms", "<=8ms", "<=16ms", "<=32ms",
           "<=64ms", "<=128ms", "<=256ms", "<=512ms", "<=1024ms", "<=2s", "<=4s", "<=8s",
           "<=16s", ">16s"]


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


def mode_us(a, b, fam, ph):
    best, best_n = 0, 0
    for i, lab in enumerate(BUCKETS):
        n = delta(a, b, f"{fam}.{ph}.buckets.{lab}")
        if n > best_n:
            best, best_n = i, n
    return 1 << best


def tail_share(a, b, fam, ph, floor_label):
    total = delta(a, b, f"{fam}.{ph}.count")
    if not total:
        return 0.0
    i0 = BUCKETS.index(floor_label)
    tail = sum(delta(a, b, f"{fam}.{ph}.buckets.{lab}") for lab in BUCKETS[i0:])
    return 100.0 * tail / total


def leg(out, name):
    d = f"{out}/{name}"
    a, b = load(f"{d}/m0_p0.json"), load(f"{d}/m0_p1.json")
    t0, t1 = map(float, open(f"{d}/wall").read().split())
    wall = t1 - t0
    passes = delta(a, b, "meta_conveyor_leader_passes")
    dpasses = delta(a, b, "meta_conveyor_durability_passes")
    hwm = int(b.get("meta_conveyor_windows_inflight_hwm", 0) or 0)
    served = delta(a, b, "meta_ship_publish.served")
    verbs = delta(a, b, "meta_ship.served_verbs")
    entries = delta(a, b, "meta_kv_journal_entries")
    fam = "meta_txpass_phase_ns"
    rho = delta(a, b, f"{fam}.pass_total.sum_ns") / 1e9 / wall
    q, qn = mean_us(a, b, fam, "tx_queue_wait")
    pt, _ = mean_us(a, b, fam, "pass_total")
    ll, _ = mean_us(a, b, fam, "pass_leaf_locks")
    rw, rwn = mean_us(a, b, fam, "journal_ring_write")
    rw_mode = mode_us(a, b, fam, "journal_ring_write")
    rw_tail = tail_share(a, b, fam, "journal_ring_write", "<=4ms")
    lw, lwn = mean_us(a, b, fam, "window_lane_wait")
    lw_tail = tail_share(a, b, fam, "window_lane_wait", "<=2ms")
    wt, _ = mean_us(a, b, fam, "window_total")
    ufs = "uring_fs_write_phase_ns"
    has_ufs = f"{ufs}.total.count" in b
    qh, _ = mean_us(a, b, ufs, "queue_hop")
    dv, _ = mean_us(a, b, ufs, "device")
    wh, _ = mean_us(a, b, ufs, "wake_hop")
    ut, un = mean_us(a, b, ufs, "total")
    lane_w = delta(a, b, "journal_ring_lane_writes")
    pool_w = delta(a, b, "journal_ring_pool_writes")
    has_lane = "journal_ring_lane_writes" in b
    g1 = delta(a, b, "meta_commit_group_size.1")
    gsum = sum(delta(a, b, k) for k in b if k.startswith("meta_commit_group_size."))
    cpu = delta(a, b, "daemon_cpu_ns") / 1e9
    cls = {k.split(".")[-1]: delta(a, b, k) / 1e9 for k in b if k.startswith("daemon_cpu_ns_by_class.")}
    print(f"== {name}: wall {wall:.1f} s, authority CPU {cpu:.2f} s "
          f"(sqz-meta {cls.get('sqz-meta', 0):.2f}, sqz-jrnl {cls.get('sqz-jrnl', 0):.2f}, "
          f"other {cls.get('other', 0):.2f})")
    print(f"  conveyor: leader passes {passes}, durability passes {dpasses} "
          f"(windows/dpass {passes / dpasses if dpasses else 0:.2f}), windows hwm {hwm}, "
          f"rho(apply) {rho:.3f}, size-1 groups {100 * g1 / gsum if gsum else 0:.0f} % of {gsum}")
    print(f"  tx_queue_wait {q:.0f} us (n={qn})  pass_total {pt:.0f}  pass_leaf_locks {ll:.0f}")
    print(f"  journal_ring_write mean {rw:.0f} us / mode <= {rw_mode} us (mean/mode {rw / rw_mode if rw_mode else 0:.1f}x, "
          f">= 4 ms {rw_tail:.1f} %, n={rwn})")
    if has_ufs:
        print(f"  uring_fs hops: queue_hop {qh:.0f}  device {dv:.0f}  wake_hop {wh:.0f}  total {ut:.0f} us (n={un})")
    else:
        print("  uring_fs hops: (no instrument on this binary — pre-C-2 control)")
    print(f"  window_lane_wait {lw:.0f} us (n={lwn}, >= 2 ms {lw_tail:.1f} %)  window_total {wt:.0f}  "
          f"commit latency ~ queue+window {q + wt:.0f} us")
    if has_lane:
        print(f"  journal lane: lane writes {lane_w}, pool writes {pool_w} "
              f"({'ENGAGED' if lane_w and lane_w >= 0.95 * passes else 'NOT engaged'} vs {passes} passes)")
    else:
        print("  journal lane: (no lane on this binary — pre-C-2 control: every write via the pool)")
    print(f"  served publishes {served}, S8 verbs {verbs}, journal entries {entries} "
          f"({entries / served if served else 0:.3f}/publish), passes/publish "
          f"{passes / served if served else 0:.3f}")
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
