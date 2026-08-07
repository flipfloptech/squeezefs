#!/usr/bin/env python3
"""D14 write-side zc bracket analysis (extends the DO-NOT-FLIP pricing
table forward for the write-side rig's four rows).

Usage: table.py <artifact-dir>
Per row: GB/s / IOPS / clat p50+p99 / box busy / daemon CPU per GB /
the write-vehicle SPLIT (direct vs extract ops+bytes) / patch+park
deltas / data-namespace write amplification + wareq-sz, plus the flip
verdict per the D14 rule (all write rows ≥ 0.97× and the read sentinel
≥ 39.5 GB/s).
"""

import json
import sys

OUT = sys.argv[1]
WLEGS = [("W1", 1), ("W2", 0), ("W3", 0), ("W4", 1)]
ROWS = ["seqwr", "dur", "rand4k", "rand4kow"]


def load(path):
    with open(path) as f:
        d = json.load(f)
    return d.get("metrics", d)


def cpu(path):
    with open(path) as f:
        return list(map(int, f.read().split()[1:]))


def disk_write_stats(path):
    """(bytes_written, write_ios) across the data namespaces."""
    sect = ios = 0
    with open(path) as f:
        for line in f:
            fields = line.split()
            ios += int(fields[7])  # writes completed
            sect += int(fields[9])  # sectors written
    return sect * 512, ios


def row_stats(leg, row):
    b = load(f"{OUT}/{leg}-{row}-before.json")
    a = load(f"{OUT}/{leg}-{row}-after.json")
    fio = json.load(open(f"{OUT}/{leg}-{row}-fio.json"))
    j = fio["jobs"]
    io = sum(x["write"]["io_bytes"] for x in j)
    bw = sum(x["write"]["bw_bytes"] for x in j)
    iops = sum(x["write"]["iops"] for x in j)
    pct = j[0]["write"].get("clat_ns", {}).get("percentile", {})
    p50 = pct.get("50.000000", 0) / 1e6
    p99 = pct.get("99.000000", 0) / 1e6
    c0, c1 = cpu(f"{OUT}/{leg}-{row}-cpu0"), cpu(f"{OUT}/{leg}-{row}-cpu1")
    tot = sum(c1) - sum(c0)
    idle = (c1[3] + c1[4]) - (c0[3] + c0[4])
    busy = 100.0 * (tot - idle) / tot if tot else 0
    dc = (
        int(open(f"{OUT}/{leg}-{row}-dcpu1").read())
        - int(open(f"{OUT}/{leg}-{row}-dcpu0").read())
    ) / 100.0
    w0, i0 = disk_write_stats(f"{OUT}/{leg}-{row}-disk0")
    w1, i1 = disk_write_stats(f"{OUT}/{leg}-{row}-disk1")
    dev_w, dev_ios = w1 - w0, i1 - i0
    d = lambda k: a.get(k, 0) - b.get(k, 0)
    total_gb = io / 1e9 * 70 / 60  # ramp-inclusive estimate
    return {
        "gbps": bw / 1e9,
        "iops": iops,
        "p50_ms": p50,
        "p99_ms": p99,
        "busy": busy,
        "dcpu": dc,
        "dcpu_ms_gb": 1000 * dc / total_gb if total_gb else 0,
        "wd": d("fuse3_zc_write_directs"),
        "wdb_gb": d("fuse3_zc_write_direct_bytes") / 1e9,
        "wx": d("fuse3_zc_write_extractions"),
        "wxb_gb": d("fuse3_zc_write_extract_bytes") / 1e9,
        "fb": d("fuse3_zc_fallbacks"),
        "pw": d("patch_writes"),
        "ep": d("extent_parks"),
        "amp": dev_w / (io * 70 / 60) if io else 0,
        "wareq_kb": dev_w / dev_ios / 1024 if dev_ios else 0,
    }


rowmed = {}
for row in ROWS:
    print(f"\n== row {row} ==")
    print(
        f"{'leg':>3} {'zc':>2} {'GB/s':>8} {'IOPS':>9} {'p50ms':>7} {'p99ms':>7} "
        f"{'busy%':>6} {'ms/GB':>6} {'directs':>9} {'dGB':>7} {'extracts':>9} "
        f"{'xGB':>7} {'patchW':>8} {'parks':>8} {'amp':>5} {'wareq':>6}"
    )
    armed, ctrl = [], []
    for leg, zc in WLEGS:
        try:
            s = row_stats(leg, row)
        except FileNotFoundError:
            print(f"{leg:>3} missing")
            continue
        (armed if zc else ctrl).append(s)
        print(
            f"{leg:>3} {zc:>2} {s['gbps']:>8.3f} {s['iops']:>9.0f} {s['p50_ms']:>7.2f} "
            f"{s['p99_ms']:>7.1f} {s['busy']:>6.1f} {s['dcpu_ms_gb']:>6.1f} "
            f"{s['wd']:>9} {s['wdb_gb']:>7.2f} {s['wx']:>9} {s['wxb_gb']:>7.2f} "
            f"{s['pw']:>8} {s['ep']:>8} {s['amp']:>5.2f} {s['wareq_kb']:>6.0f}"
        )
    if len(armed) == 2 and len(ctrl) == 2:
        med = lambda xs: sorted(xs)[0] + (sorted(xs)[1] - sorted(xs)[0]) / 2
        am, cm = med([s["gbps"] for s in armed]), med([s["gbps"] for s in ctrl])
        ratio = am / cm if cm else 0
        rowmed[row] = ratio
        b1 = armed[0]["gbps"] / ctrl[0]["gbps"] if ctrl[0]["gbps"] else 0
        b2 = armed[1]["gbps"] / ctrl[1]["gbps"] if ctrl[1]["gbps"] else 0
        print(
            f"  medians: armed {am:.3f} vs control {cm:.3f} -> {ratio:.3f}x "
            f"(brackets {b1:.3f}x / {b2:.3f}x)"
        )

# read sentinel
print("\n== read sentinel ==")
read_bw = {}
for leg, zc in [("R5", 1), ("R6", 0)]:
    try:
        b = load(f"{OUT}/{leg}-read-before.json")
        a = load(f"{OUT}/{leg}-read-after.json")
        fio = json.load(open(f"{OUT}/{leg}-read-fio.json"))
    except FileNotFoundError:
        print(f"{leg}: missing")
        continue
    j = fio["jobs"]
    bw = sum(x["read"]["bw_bytes"] for x in j) / 1e9
    read_bw[zc] = bw
    d = lambda k: a.get(k, 0) - b.get(k, 0)
    c0, c1 = cpu(f"{OUT}/{leg}-read-cpu0"), cpu(f"{OUT}/{leg}-read-cpu1")
    tot = sum(c1) - sum(c0)
    idle = (c1[3] + c1[4]) - (c0[3] + c0[4])
    busy = 100.0 * (tot - idle) / tot if tot else 0
    dc = (
        int(open(f"{OUT}/{leg}-read-dcpu1").read())
        - int(open(f"{OUT}/{leg}-read-dcpu0").read())
    ) / 100.0
    print(
        f"{leg} zc={zc}: {bw:.3f} GB/s busy={busy:.1f}% dcpu={dc:.1f}s "
        f"zc_GB={d('read_zc_serve_bytes')/1e9:.1f} replies={d('fuse3_zc_replies')} "
        f"fb={d('fuse3_zc_fallbacks')}"
    )

# the flip rule
print("\n== D14 flip verdict ==")
ok = True
for row, r in rowmed.items():
    verdict = "PASS" if r >= 0.97 else "FAIL"
    ok = ok and r >= 0.97
    print(f"  {row}: {r:.3f}x (need >= 0.97) {verdict}")
if 1 in read_bw:
    verdict = "PASS" if read_bw[1] >= 39.5 else "FAIL"
    ok = ok and read_bw[1] >= 39.5
    print(f"  read sentinel: {read_bw[1]:.3f} GB/s (need >= 39.5) {verdict}")
else:
    ok = False
    print("  read sentinel: MISSING")
print("FLIP" if ok else "DO NOT FLIP")
