#!/usr/bin/env python3
"""A-vs-B table for the kernel per-queue bg-accounting A/B
(`2026-09-06-kernel-bg-per-queue-ab.sh`; design-kernel-bg-per-queue §5).

    2026-09-06-kernel-bg-ab-table.py <run-dir>...

Each run dir is one rig boot (`ab-A-*` / `ab-B-*`). Rows are paired by
tag with the arm letter stripped (`A-rr4k-kern-1` ≡ `B-rr4k-kern-1`), and
the verdict columns are the ones §5 names: fio IOPS / clat p50 / p99, the
`fuse3-ur` worker's µs per fio op (the primary — `daemon_cpu_ns_by_class`
delta over the row's fio ios) and per transport op (`fuse3_zc_replies`),
box-wide busy %, the zc-bridge hops and the transport dispatch lag (exact
means from the `*_ns` sum/count deltas — the row-delta script's arithmetic
verbatim), and, for the `-perf` row, the flat self-time share of
`native_queued_spin_lock_slowpath` + `_raw_spin_lock` from `lock.txt`.
Per-arm medians close the table (n = the number of boots × rows per tag).
"""
import glob
import json
import os
import re
import statistics
import sys


def phase_mean(s0, s1, fam, ph):
    f0, f1 = s0.get(fam, {}), s1.get(fam, {})
    if not isinstance(f1, dict) or ph not in f1 or "count" not in f1[ph]:
        return None
    c = int(f1[ph]["count"]) - int(f0.get(ph, {}).get("count", 0))
    s = int(f1[ph]["sum_ns"]) - int(f0.get(ph, {}).get("sum_ns", 0))
    return s / c / 1000 if c else None


def busy(res, label):
    try:
        a = [int(x) for x in open(f"{res}/{label}.procstat0").readline().split()[1:]]
        b = [int(x) for x in open(f"{res}/{label}.procstat1").readline().split()[1:]]
    except OSError:
        return None
    dd = [y - x for x, y in zip(a, b)]
    tot = sum(dd)
    idle = dd[3] + dd[4]
    return 100 * (tot - idle) / tot if tot else None


def lock_shares(res, label):
    p = f"{res}/{label}.perf/lock.txt"
    if not os.path.exists(p):
        return None, None
    slow = raw = None
    for line in open(p):
        m = re.match(r"\s*([\d.]+)%\s+\S+\s+\[k\]\s+(\S+)", line)
        if not m:
            continue
        if m.group(2) == "native_queued_spin_lock_slowpath" and slow is None:
            slow = float(m.group(1))
        elif m.group(2) == "_raw_spin_lock" and raw is None:
            raw = float(m.group(1))
    return slow, raw


def row(res, label):
    s0 = json.load(open(f"{res}/{label}.stats0"))["metrics"]
    s1 = json.load(open(f"{res}/{label}.stats1"))["metrics"]
    jobs = json.load(open(f"{res}/{label}.fio.json"))["jobs"]
    side = "read" if sum(j["read"]["total_ios"] for j in jobs) else "write"
    ios = sum(j[side]["total_ios"] for j in jobs)
    iops = sum(j[side]["iops"] for j in jobs)
    p50 = statistics.mean(j[side]["clat_ns"]["percentile"]["50.000000"] for j in jobs) / 1000
    p99 = max(j[side]["clat_ns"]["percentile"]["99.000000"] for j in jobs) / 1000
    c0, c1 = s0["daemon_cpu_ns_by_class"], s1["daemon_cpu_ns_by_class"]
    ur = (int(c1.get("fuse3-ur", 0)) - int(c0.get("fuse3-ur", 0))) / 1000
    tops = int(s1.get("fuse3_zc_replies", 0)) - int(s0.get("fuse3_zc_replies", 0))
    slow, raw = lock_shares(res, label)
    trip = sum(int(s1.get(k, 0) or 0) - int(s0.get(k, 0) or 0)
               for k in ("invariant_tripwires", "fuse_op_watchdog_overdue", "transport_cq_overflows",
                         "transport_lease_overlong"))
    return {
        "iops": iops, "p50": p50, "p99": p99,
        "ur_us_fio": ur / ios if ios else None,
        "ur_us_top": ur / tops if tops else None,
        "busy": busy(res, label),
        "msg_hop": phase_mean(s0, s1, "zc_bridge_phase_ns", "msg_hop"),
        "wake_hop": phase_mean(s0, s1, "zc_bridge_phase_ns", "wake_hop"),
        "device_cq": phase_mean(s0, s1, "zc_bridge_phase_ns", "device_cq"),
        "dispatch_lag": phase_mean(s0, s1, "read_transport_phase_ns", "dispatch_lag"),
        "slowpath": slow, "raw_spin": raw, "tripwires": trip,
    }


COLS = [("iops", "IOPS", "{:,.0f}"), ("p50", "p50 µs", "{:.1f}"), ("p99", "p99 µs", "{:.0f}"),
        ("ur_us_fio", "f3-ur µs/fio-op", "{:.2f}"), ("ur_us_top", "µs/transport-op", "{:.2f}"),
        ("busy", "box busy %", "{:.1f}"), ("msg_hop", "msg_hop µs", "{:.1f}"),
        ("wake_hop", "wake_hop µs", "{:.1f}"), ("device_cq", "device_cq µs", "{:.1f}"),
        ("dispatch_lag", "dispatch_lag µs", "{:.1f}"), ("slowpath", "slowpath %", "{:.2f}"),
        ("raw_spin", "_raw_spin_lock %", "{:.2f}"), ("tripwires", "tripwires", "{:.0f}")]


def fmt(v, f):
    return "—" if v is None else f.format(v)


def main():
    runs = sys.argv[1:]
    if not runs:
        sys.exit(__doc__)
    table = {}  # tag -> arm -> [rows]
    for res in runs:
        for rf in sorted(glob.glob(f"{res}/*.row")):
            label = os.path.basename(rf)[:-4]
            # the prep-write row carries no arm letter — it mints the set, it is not a row
            if not re.match(r"^[AB]-", label) or not os.path.exists(f"{res}/{label}.fio.json"):
                continue
            arm, tag = label[0], label[2:]
            table.setdefault(tag, {}).setdefault(arm, []).append((os.path.basename(res), row(res, label)))
    for tag in sorted(table):
        print(f"\n== {tag}")
        print("| arm | boot | " + " | ".join(h for _, h, _ in COLS) + " |")
        print("|" + "---|" * (len(COLS) + 2))
        for arm in sorted(table[tag]):
            for boot, r in table[tag][arm]:
                print(f"| {arm} | {boot[3:]} | " + " | ".join(fmt(r[k], f) for k, _, f in COLS) + " |")
        meds = {}
        for arm in sorted(table[tag]):
            rows = [r for _, r in table[tag][arm]]
            meds[arm] = {k: (statistics.median([r[k] for r in rows if r[k] is not None])
                             if any(r[k] is not None for r in rows) else None) for k, _, _ in COLS}
            print(f"| {arm} | median n={len(rows)} | " + " | ".join(fmt(meds[arm][k], f) for k, _, f in COLS) + " |")
        if "A" in meds and "B" in meds:
            cells = []
            for k, _, _ in COLS:
                a, b = meds["A"][k], meds["B"][k]
                cells.append("—" if a in (None, 0) or b is None else f"{(b - a) / a * 100:+.1f} %")
            print("| B/A | Δ median | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    main()
