#!/usr/bin/env python3
"""zc write-bracket analysis: per-row table + phase_ns attribution.

Usage: table.py <artifact-dir>
Reads the rig's per-row artifacts (fio json, stats before/after,
/proc/stat + daemon-cpu + diskstats snapshots) and prints the evidence
tables: GB/s / IOPS / clat percentiles / box busy / daemon CPU per GB /
zc engagement / data-namespace write amplification, plus the
write_pipeline_phase_ns bucket-shift attribution (detach_lag / dma /
total) between armed and control legs.
"""

import json
import sys

OUT = sys.argv[1]
WLEGS = [("W1", 1), ("W2", 0), ("W3", 0), ("W4", 1)]
ROWS = ["seqwr", "dur", "rand4k"]
PHASES = ["admit_wait", "detach_lag", "lock_wait", "dma", "publish", "total"]


def load(path):
    with open(path) as f:
        d = json.load(f)
    return d.get("metrics", d)


def cpu(path):
    with open(path) as f:
        return list(map(int, f.read().split()[1:]))


def disk_sectors_written(path):
    tot = 0
    with open(path) as f:
        for line in f:
            fields = line.split()
            # /proc/diskstats: field 9 (0-based idx 9) = sectors written
            tot += int(fields[9])
    return tot * 512


def row_stats(leg, row):
    b = load(f"{OUT}/{leg}-{row}-before.json")
    a = load(f"{OUT}/{leg}-{row}-after.json")
    fio = json.load(open(f"{OUT}/{leg}-{row}-fio.json"))
    j = fio["jobs"]
    io = sum(x["write"]["io_bytes"] for x in j)
    bw = sum(x["write"]["bw_bytes"] for x in j)
    iops = sum(x["write"]["iops"] for x in j)
    # clat percentiles (ns) — group_reporting folds into one job entry
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
    dev_w = disk_sectors_written(f"{OUT}/{leg}-{row}-disk1") - disk_sectors_written(
        f"{OUT}/{leg}-{row}-disk0"
    )
    d = lambda k: a.get(k, 0) - b.get(k, 0)
    # ramp-inclusive user bytes ≈ window × 70/60 (the read rig's estimate)
    total_gb = io / 1e9 * 70 / 60
    return {
        "io_gb": io / 1e9,
        "gbps": bw / 1e9,
        "iops": iops,
        "p50_ms": p50,
        "p99_ms": p99,
        "busy": busy,
        "dcpu": dc,
        "dcpu_ms_gb": 1000 * dc / total_gb if total_gb else 0,
        "wx": d("fuse3_zc_write_extractions"),
        "wxb_gb": d("fuse3_zc_write_extract_bytes") / 1e9,
        "fb": d("fuse3_zc_fallbacks"),
        "sk": d("fuse3_zc_slot_payload_skips"),
        "dev_w_gb": dev_w / 1e9,
        "amp": dev_w / (io * 70 / 60) if io else 0,
        "before": b,
        "after": a,
    }


def phase_delta(leg, row, phase):
    b = load(f"{OUT}/{leg}-{row}-before.json").get("write_pipeline_phase_ns", {})
    a = load(f"{OUT}/{leg}-{row}-after.json").get("write_pipeline_phase_ns", {})
    pb, pa = b.get(phase, {}), a.get(phase, {})
    return {k: pa.get(k, 0) - pb.get(k, 0) for k in pa if pa.get(k, 0) != pb.get(k, 0)}


def bucket_mean_us(d):
    """Approximate mean from bucket deltas (bucket midpoint heuristic)."""
    import re

    def edge(k):
        m = re.match(r"<=(\d+)(us|ms|s)", k)
        if not m:
            return None
        v = int(m.group(1))
        return v * {"us": 1, "ms": 1000, "s": 1000000}[m.group(2)]

    tot = n = 0
    for k, c in d.items():
        e = edge(k)
        if e is None or c <= 0:
            continue
        tot += c * e * 0.75  # midpoint-ish of a power-of-two bucket
        n += c
    return tot / n if n else 0


for row in ROWS:
    print(f"\n== row {row} ==")
    hdr = (
        f"{'leg':>3} {'zc':>2} {'GB/s':>8} {'IOPS':>9} {'p50ms':>7} {'p99ms':>7} "
        f"{'busy%':>6} {'dcpu_s':>7} {'ms/GB':>6} {'wx':>9} {'wx_GB':>8} "
        f"{'fb':>3} {'devW_GB':>8} {'amp':>5}"
    )
    print(hdr)
    for leg, zc in WLEGS:
        s = row_stats(leg, row)
        print(
            f"{leg:>3} {zc:>2} {s['gbps']:>8.3f} {s['iops']:>9.0f} {s['p50_ms']:>7.2f} "
            f"{s['p99_ms']:>7.1f} {s['busy']:>6.1f} {s['dcpu']:>7.1f} "
            f"{s['dcpu_ms_gb']:>6.1f} {s['wx']:>9} {s['wxb_gb']:>8.1f} {s['fb']:>3} "
            f"{s['dev_w_gb']:>8.1f} {s['amp']:>5.2f}"
        )

print("\n== write_pipeline_phase_ns bucket-mean shift (us, midpoint estimate) ==")
print(f"{'row':>7} {'phase':>11} " + " ".join(f"{leg:>9}" for leg, _ in WLEGS))
for row in ROWS:
    for ph in PHASES:
        vals = []
        for leg, _ in WLEGS:
            vals.append(bucket_mean_us(phase_delta(leg, row, ph)))
        print(f"{row:>7} {ph:>11} " + " ".join(f"{v:>9.0f}" for v in vals))

# read sentinel
print("\n== read sentinel ==")
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
