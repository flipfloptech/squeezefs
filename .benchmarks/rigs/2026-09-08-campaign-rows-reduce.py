#!/usr/bin/env python3
"""A-vs-B table for the campaign-rows rig (`2026-09-08-campaign-rows-abba.sh`).

    2026-09-08-campaign-rows-reduce.py <out-dir>...

Rows are paired by tag with the arm letter + sequence position stripped
(`E1-rr4k-kern` ≡ `F2-rr4k-kern`); the arm is the tag's first character
and the A-B-B-A order is the second. Every mean is EXACT (Δsum_ns / Δcount
over the row's stats0/stats1 pair), never a bucket midpoint. Columns per
row class:

* every row — fio IOPS / bw / clat p50 / p99 / p99.9 of the job's active
  side, daemon CPU µs per fio op (`daemon_cpu_ns` Δ ÷ fio ios — the
  R-5/W-6 handler-economy verdict column), box busy %, bw-log flatness
  (first vs last third), and the must-stay-0 tripwire sum;
* write rows — `patch_writes` (the W1 in-place arm), `write_through_blocks`,
  the write-pipeline residence `total`, the ranged-gap-seed engagement pair
  (W-6 #10: `overlay_gap_seed_ranged_bytes` ÷ `overlay_gap_seed_bytes`);
* fsync rows — fio's `sync` latency block (mean / p50 / p99), fsyncs
  (`meta_sync_requests` Δ — present on both arms), data-device sync
  REQUESTS per fsync (W-5's touched-namespace lever: the shipped E arm
  requests every namespace per fsync, F only the touched ones), physical
  data-device syncs per fsync, meta-device syncs per fsync, and the F-only
  `fsync_phase_ns` decomposition (data_flush / data_barrier / meta_barrier /
  meta_publish / total) with `fsync_parallel_joins`.

Per-arm medians close each table with the F/E Δ of the medians.
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


def flatness(res, label):
    series = {}
    for f in glob.glob(f"{res}/{label}_bw.*.log"):
        for line in open(f):
            t, v = line.split(",")[:2]
            t = int(t) // 1000
            series[t] = series.get(t, 0) + int(v)
    ts = sorted(series)
    if len(ts) < 9:
        return None
    n = len(ts) // 3
    a = statistics.mean(series[t] for t in ts[:n])
    c = statistics.mean(series[t] for t in ts[-n:])
    return (c - a) / a * 100 if a else None


TRIPWIRES = ("invariant_tripwires", "fuse_op_watchdog_overdue", "transport_cq_overflows",
             "transport_lease_overlong", "write_pipeline_fence_drops", "data_dma_fence_refusals",
             "writeback_errors_latched", "detached_task_panics")


def row(res, label):
    s0 = json.load(open(f"{res}/{label}.stats0"))["metrics"]
    s1 = json.load(open(f"{res}/{label}.stats1"))["metrics"]
    jobs = json.load(open(f"{res}/{label}.fio.json"))["jobs"]

    def d(k):
        a, b = s0.get(k), s1.get(k)
        if isinstance(a, (int, float)) and isinstance(b, (int, float)):
            return b - a
        return None

    side = "read" if sum(j["read"]["total_ios"] for j in jobs) else "write"
    ios = sum(j[side]["total_ios"] for j in jobs)
    pct = lambda j, p: j[side]["clat_ns"]["percentile"][p]  # noqa: E731
    r = {
        "iops": sum(j[side]["iops"] for j in jobs),
        "bw": sum(j[side]["bw_bytes"] for j in jobs) / 2**20,
        "p50": statistics.mean(pct(j, "50.000000") for j in jobs) / 1000,
        "p99": max(pct(j, "99.000000") for j in jobs) / 1000,
        "p999": max(pct(j, "99.900000") for j in jobs) / 1000,
        "cpu_us": (d("daemon_cpu_ns") or 0) / ios / 1000 if ios else None,
        "busy": busy(res, label),
        "flat": flatness(res, label),
        "trip": sum(d(k) or 0 for k in TRIPWIRES),
    }
    if side == "write":
        r["patch"] = d("patch_writes")
        r["wt_blocks"] = d("write_through_blocks")
        r["wp_total"] = phase_mean(s0, s1, "write_pipeline_phase_ns", "total")
        gs, gr = d("overlay_gap_seed_bytes"), d("overlay_gap_seed_ranged_bytes")
        r["gap_ranged_pct"] = (100 * gr / gs) if gs and gr is not None else (0.0 if gs else None)
    syncs = [j for j in jobs if j.get("sync", {}).get("total_ios")]
    if syncs:
        s = syncs[0]["sync"]
        r["sync_mean"] = statistics.mean(j["sync"]["lat_ns"]["mean"] for j in syncs) / 1000
        r["sync_p50"] = statistics.mean(j["sync"]["lat_ns"]["percentile"]["50.000000"] for j in syncs) / 1000
        r["sync_p99"] = max(j["sync"]["lat_ns"]["percentile"]["99.000000"] for j in syncs) / 1000
        del s
    fs = d("meta_sync_requests")
    if fs:
        r["fsyncs"] = fs
        r["dreq_per"] = (d("data_device_sync_requests") or 0) / fs
        r["dsync_per"] = (d("data_device_syncs") or 0) / fs
        r["msync_per"] = (d("meta_device_syncs") or 0) / fs
        r["joins"] = d("fsync_parallel_joins")
        for ph in ("data_flush", "staged_promote", "data_barrier", "meta_barrier", "meta_publish", "total"):
            r[f"fs_{ph}"] = phase_mean(s0, s1, "fsync_phase_ns", ph)
        # The fsync-promotes-staged lever (2026-09-09): promotions per fsync +
        # the staged-layout population the row created.
        r["promoted"] = d("fsync_promoted_files")
        r["promote_fail"] = d("fsync_promote_failures")
        r["staged_writes"] = d("layout_staged_writes")
    return r


BASE = [("iops", "IOPS", "{:,.0f}"), ("bw", "MiB/s", "{:,.0f}"), ("p50", "p50 µs", "{:.1f}"),
        ("p99", "p99 µs", "{:.0f}"), ("p999", "p99.9 µs", "{:.0f}"), ("cpu_us", "daemon µs/op", "{:.2f}"),
        ("busy", "box busy %", "{:.1f}"), ("flat", "flat %", "{:+.1f}"), ("trip", "tripwires", "{:.0f}")]
WRITE = [("patch", "patch_writes", "{:,.0f}"), ("wt_blocks", "write_through", "{:,.0f}"),
         ("wp_total", "wp total µs", "{:.0f}"), ("gap_ranged_pct", "gap ranged %", "{:.0f}")]
FSYNC = [("sync_mean", "sync mean µs", "{:.0f}"), ("sync_p50", "sync p50 µs", "{:.0f}"),
         ("sync_p99", "sync p99 µs", "{:.0f}"), ("fsyncs", "fsyncs", "{:,.0f}"),
         ("dreq_per", "data sync req/fsync", "{:.2f}"), ("dsync_per", "data syncs/fsync", "{:.2f}"),
         ("msync_per", "meta syncs/fsync", "{:.2f}"), ("joins", "parallel joins", "{:,.0f}"),
         ("promoted", "fsync promotions", "{:,.0f}"), ("promote_fail", "promote fail", "{:,.0f}"),
         ("staged_writes", "staged-layout writes", "{:,.0f}"),
         ("fs_data_flush", "fs data_flush µs", "{:.0f}"), ("fs_staged_promote", "fs staged_promote µs", "{:.0f}"),
         ("fs_data_barrier", "fs data_barrier µs", "{:.0f}"),
         ("fs_meta_barrier", "fs meta_barrier µs", "{:.0f}"), ("fs_meta_publish", "fs meta_publish µs", "{:.0f}"),
         ("fs_total", "fs total µs", "{:.0f}")]


def fmt(v, f):
    return "—" if v is None else f.format(v)


def table(tag, arms, cols):
    cols = [c for c in cols if any(r.get(c[0]) is not None for rs in arms.values() for _, r in rs)]
    print(f"\n== {tag}")
    print("| arm | pos | " + " | ".join(h for _, h, _ in cols) + " |")
    print("|" + "---|" * (len(cols) + 2))
    meds = {}
    for arm in sorted(arms):
        for pos, r in sorted(arms[arm]):
            print(f"| {arm} | {pos} | " + " | ".join(fmt(r.get(k), f) for k, _, f in cols) + " |")
    for arm in sorted(arms):
        rows = [r for _, r in arms[arm]]
        meds[arm] = {k: (statistics.median([r[k] for r in rows if r.get(k) is not None])
                         if any(r.get(k) is not None for r in rows) else None) for k, _, _ in cols}
        print(f"| {arm} | median n={len(rows)} | " + " | ".join(fmt(meds[arm][k], f) for k, _, f in cols) + " |")
    letters = sorted(meds)
    if len(letters) == 2:
        a, b = letters
        cells = []
        for k, _, _ in cols:
            x, y = meds[a][k], meds[b][k]
            cells.append("—" if x in (None, 0) or y is None else f"{(y - x) / x * 100:+.1f} %")
        print(f"| {b}/{a} | Δ median | " + " | ".join(cells) + " |")


def main():
    runs = sys.argv[1:]
    if not runs:
        sys.exit(__doc__)
    tags = {}  # tag -> arm -> [(pos, row)]
    for res in runs:
        for jf in sorted(glob.glob(f"{res}/*.fio.json")):
            label = os.path.basename(jf)[: -len(".fio.json")]
            m = re.match(r"^([A-Z])(\d+)-(.+)$", label)
            if not m or not os.path.exists(f"{res}/{label}.stats1"):
                continue
            arm, pos, tag = m.group(1), int(m.group(2)), m.group(3)
            tags.setdefault(tag, {}).setdefault(arm, []).append((pos, row(res, label)))
    for tag in sorted(tags):
        cols = list(BASE)
        if "kern" in tag and ("rw" in tag or "wdur" in tag) or "fsync" in tag or "smallf" in tag:
            cols += WRITE
        if "fsync" in tag or "wdur" in tag or "smallf" in tag:
            cols += FSYNC
        table(tag, tags[tag], cols)


if __name__ == "__main__":
    main()
