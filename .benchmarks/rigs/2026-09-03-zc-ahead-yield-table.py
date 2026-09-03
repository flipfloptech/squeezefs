#!/usr/bin/env python3
"""The zc-ahead-yield bracket's summary table: one line per read row from
the driver's stats0/stats1 + fio JSON (usage: table.py <run dir> [rows...]).
Every mean is exact (Δsum/Δcount); dev/user counts every device byte the
row's serve legs moved (zc + pooled fills + dest DMA) over the row's user
bytes scaled to include the 10 s ramp."""
import json
import statistics
import sys

run = sys.argv[1]
rows = sys.argv[2:] or [
    "A1-seq1m-kern", "B1-seq1m-kern", "B2-seq1m-kern", "A2-seq1m-kern",
    "A1-seq1m-il", "B1-seq1m-il", "B2-seq1m-il", "A2-seq1m-il",
    "B1-seq1m-kern-60", "A2-seq1m-kern-60", "B1-seq1m-il-60", "A2-seq1m-il-60",
    "A1-rr4k-kern", "B1-rr4k-kern", "B2-rr4k-kern", "A2-rr4k-kern",
]
cols = ["row", "GiB/s", "clat us", "p50", "p99", "cpu s", "us/op", "ur s", "tpc s", "other s",
        "lane_fetch", "lane_serves", "pf_issued", "pf_evict_unc", "hold_evict_unc", "ahead_evict",
        "probe up/back", "sf_waiters", "zc GB", "warm_copy GB", "dest_copy GB", "fill_dma GB",
        "fd_serves", "demotes", "user GB", "dev/user"]
print(" | ".join(cols))
for t in rows:
    a = json.load(open(f"{run}/{t}.stats0"))["metrics"]
    b = json.load(open(f"{run}/{t}.stats1"))["metrics"]
    f = json.load(open(f"{run}/{t}.fio.json"))["jobs"]

    def d(k):
        return int(b.get(k, 0) or 0) - int(a.get(k, 0) or 0)

    c0, c1 = a.get("daemon_cpu_ns_by_class", {}), b.get("daemon_cpu_ns_by_class", {})
    cls = {k: (int(c1[k]) - int(c0.get(k, 0))) / 1e9 for k in c1}
    ios = sum(j["read"]["total_ios"] for j in f)
    ub = sum(j["read"]["io_bytes"] for j in f)
    bw = sum(j["read"]["bw_bytes"] for j in f) / 2**30
    clat = statistics.mean(j["read"]["clat_ns"]["mean"] for j in f) / 1000
    p50 = statistics.mean(j["read"]["clat_ns"]["percentile"]["50.000000"] for j in f) / 1000
    p99 = max(j["read"]["clat_ns"]["percentile"]["99.000000"] for j in f) / 1000
    dev = d("read_zc_serve_bytes") + d("read_fill_dma_bytes") + d("read_dest_dma_bytes")
    rt = max(j["read"]["runtime"] for j in f) / 1000
    cpu = d("daemon_cpu_ns")
    vals = [t, f"{bw:.2f}", f"{clat:.0f}", f"{p50:.0f}", f"{p99:.0f}", f"{cpu/1e9:.1f}", f"{cpu/ios/1000:.1f}",
            f"{cls.get('fuse3-ur', 0):.1f}", f"{cls.get('fuse3-tpc', 0):.1f}", f"{cls.get('other', 0):.1f}",
            str(d("read_lane_fetches")), str(d("read_lane_serves")), str(d("prefetch_issued")),
            str(d("prefetch_evicted_unconsumed")), str(d("read_lane_hold_evicted_unconsumed")),
            str(d("read_lane_hold_ahead_evictions")),
            f"{d('read_lane_depth_probe_ups')}/{d('read_lane_depth_probe_backoffs')}",
            str(d("singleflight_waiter_result_serves")), f"{d('read_zc_serve_bytes')/1e9:.0f}",
            f"{d('read_copy_warm_serve_bytes')/1e9:.1f}", f"{d('read_copy_dest_bytes')/1e9:.1f}",
            f"{d('read_fill_dma_bytes')/1e9:.1f}", str(d("transport_fast_dispatch_serves")),
            str(d("transport_fast_dispatch_demotes")), f"{ub/1e9:.0f}", f"{dev/(ub*(rt+10)/rt):.3f}"]
    print(" | ".join(vals))
