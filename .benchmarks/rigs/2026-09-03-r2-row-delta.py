#!/usr/bin/env python3
"""Per-row delta table for the R-2 rig: stats0/stats1 (.stats JSON) + fio JSON.

Every phase mean is EXACT (Δsum_ns / Δcount — the A1 histograms), never a
bucket midpoint. Python 3.6-compatible (the field box). Usage:
row_delta.py <results dir> <label>
"""
import glob
import json
import sys


def mean(xs):
    xs = list(xs)
    return sum(xs) / len(xs) if xs else 0.0


res, label = sys.argv[1], sys.argv[2]
s0 = json.load(open("%s/%s.stats0" % (res, label)))["metrics"]
s1 = json.load(open("%s/%s.stats1" % (res, label)))["metrics"]
fio = json.load(open("%s/%s.fio.json" % (res, label)))

jobs = fio["jobs"]
iops = sum(j["read"]["iops"] for j in jobs)
ios = sum(j["read"]["total_ios"] for j in jobs)
ubytes = sum(j["read"]["io_bytes"] for j in jobs)
clat = mean(j["read"]["clat_ns"]["mean"] for j in jobs) / 1000
pct = lambda q: mean(j["read"]["clat_ns"]["percentile"][q] for j in jobs) / 1000  # noqa: E731
p50, p90, p99, p999 = pct("50.000000"), pct("90.000000"), pct("99.000000"), pct("99.900000")
bw = sum(j["read"]["bw_bytes"] for j in jobs) / 2 ** 30
runtime_s = max(j["read"]["runtime"] for j in jobs) / 1000

# Sustained flatness: first vs last third of the aggregate 1 s bw log.
flat = ""
try:
    series = {}
    for f in glob.glob("%s/%s_bw.*.log" % (res, label)):
        for line in open(f):
            t, v = line.split(",")[:2]
            t = int(t) // 1000
            series[t] = series.get(t, 0) + int(v)
    ts = sorted(series)
    if len(ts) >= 9:
        n = len(ts) // 3
        a = mean(series[t] for t in ts[:n])
        c = mean(series[t] for t in ts[-n:])
        flat = "first/last third %.0f/%.0f MiB/s (%+.1f %%)" % (a / 1024, c / 1024, (c - a) / a * 100)
except Exception as e:  # noqa: BLE001
    flat = "(bw log unreadable: %s)" % e


def d(k):
    return int(s1.get(k, 0) or 0) - int(s0.get(k, 0) or 0)


def phase(fam):
    out = []
    f0, f1 = s0.get(fam, {}), s1.get(fam, {})
    for ph in f1:
        c = int(f1[ph]["count"]) - int(f0.get(ph, {}).get("count", 0))
        s = int(f1[ph]["sum_ns"]) - int(f0.get(ph, {}).get("sum_ns", 0))
        out.append((ph, c, s / c / 1000 if c else 0.0))
    return out


print("ROW %s: iops=%.0f bw=%.2fGiB/s ops=%d clat_mean=%.1fus p50=%.1f p90=%.1f p99=%.1f p99.9=%.1f runtime=%.0fs %s"
      % (label, iops, bw, ios, clat, p50, p90, p99, p999, runtime_s, flat))

# CPU per op by class.
c0, c1 = s0.get("daemon_cpu_ns_by_class", {}), s1.get("daemon_cpu_ns_by_class", {})
tot = d("daemon_cpu_ns")
cls = {k: int(c1[k]) - int(c0.get(k, 0)) for k in c1}
print("   cpu: total %.2f s = %.2f us/op; %s" % (
    tot / 1e9, tot / ios / 1000 if ios else 0,
    " ".join("%s=%.2fs" % (k, v / 1e9) for k, v in sorted(cls.items(), key=lambda x: -x[1]) if v)))

# R-2 engagement: serves + demotes vs the row's READs (the .stats brackets
# add a handful of demotes); served READs never take the in-place arm.
serves, demotes, inplace, zc = d("transport_fast_dispatch_serves"), d("transport_fast_dispatch_demotes"), \
    d("fuse3_read_inplace_replies"), d("fuse3_zc_replies")
print("   fast-dispatch: serves=%d demotes=%d (serves+demotes=%d vs fio ops %d) inplace_replies=%d zc_replies=%d"
      % (serves, demotes, serves + demotes, ios, inplace, zc))

keys = ["read_zc_serve_bytes", "read_odirect_requests", "ranged_reads", "ranged_read_bytes", "get_obj",
        "read_dest_lease_bytes", "read_copy_dest_bytes", "read_copy_bounce_bytes", "read_copy_warm_serve_bytes",
        "read_dest_dma_bytes", "cache_hits", "hot_block_hits", "read_lane_serves", "read_fill_publishes_skipped",
        "transport_wake_writes", "transport_wakes_elided", "transport_commit_batch_flushes",
        "transport_commit_batch_commits", "transport_park_backstop_ticks",
        "invariant_tripwires", "transport_lease_overlong", "fuse_op_watchdog_overdue", "transport_cq_overflows",
        "read_dest_overruns", "transport_requests_abandoned", "fuse3_zc_bridge_cancels", "op_trace_dropped"]
print("   ledger:", " ".join("%s=%d" % (k, d(k)) for k in keys if k in s1))

for fam in ["read_transport_phase_ns", "transport_reap_gap_ns", "read_serve_phase_ns", "read_fill_phase_ns"]:
    rows = phase(fam)
    if rows and any(c for _, c, _ in rows):
        print("   %s: " % fam + " ".join("%s=%.1fus(n=%d)" % (ph, m, c) for ph, c, m in rows))
