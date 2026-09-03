#!/usr/bin/env python3
"""Per-row delta table for the R-3 rig: stats0/stats1 (.stats JSON) + fio JSON.

Every phase mean is EXACT (Δsum_ns / Δcount — the A1 histograms), never a
bucket midpoint. Adds to the R-1 table the zc bridge decomposition
(`zc_bridge_phase_ns` — msg_hop / sq_wait / device_cq / wake_hop / total),
the funnel split (`read_fill_phase_ns` dev_queue / dev_service), the il
direct-drive split, and the R-3 engagement counters (worker enters /
passes / wakes per op). Usage: row_delta.py <results dir> <label>
"""
import glob
import json
import statistics
import sys

res, label = sys.argv[1], sys.argv[2]
s0 = json.load(open(f"{res}/{label}.stats0"))["metrics"]
s1 = json.load(open(f"{res}/{label}.stats1"))["metrics"]
fio = json.load(open(f"{res}/{label}.fio.json"))

jobs = fio["jobs"]
iops = sum(j["read"]["iops"] for j in jobs)
ios = sum(j["read"]["total_ios"] for j in jobs)
ubytes = sum(j["read"]["io_bytes"] for j in jobs)
clat = statistics.mean(j["read"]["clat_ns"]["mean"] for j in jobs) / 1000
p50 = statistics.mean(j["read"]["clat_ns"]["percentile"]["50.000000"] for j in jobs) / 1000
p99 = max(j["read"]["clat_ns"]["percentile"]["99.000000"] for j in jobs) / 1000
p999 = max(j["read"]["clat_ns"]["percentile"]["99.900000"] for j in jobs) / 1000
bw = sum(j["read"]["bw_bytes"] for j in jobs) / 2**30
runtime_s = max(j["read"]["runtime"] for j in jobs) / 1000

flat = ""
try:
    series = {}
    for f in glob.glob(f"{res}/{label}_bw.*.log"):
        for line in open(f):
            t, v = line.split(",")[:2]
            t = int(t) // 1000
            series[t] = series.get(t, 0) + int(v)
    ts = sorted(series)
    if len(ts) >= 9:
        n = len(ts) // 3
        a = statistics.mean(series[t] for t in ts[:n])
        c = statistics.mean(series[t] for t in ts[-n:])
        flat = f"first/last third {a/1024:.0f}/{c/1024:.0f} MiB/s ({(c-a)/a*100:+.1f} %)"
except Exception as e:  # noqa: BLE001
    flat = f"(bw log unreadable: {e})"


def d(k):
    return int(s1.get(k, 0) or 0) - int(s0.get(k, 0) or 0)


def phase(fam):
    out = []
    f0, f1 = s0.get(fam, {}), s1.get(fam, {})
    if not isinstance(f1, dict):
        return out
    for ph in f1:
        if not isinstance(f1[ph], dict) or "count" not in f1[ph]:
            continue
        c = int(f1[ph]["count"]) - int(f0.get(ph, {}).get("count", 0))
        s = int(f1[ph]["sum_ns"]) - int(f0.get(ph, {}).get("sum_ns", 0))
        if c:
            out.append((ph, c, s / c / 1000))
    return out


print(f"ROW {label}: iops={iops:.0f} bw={bw:.2f}GiB/s ops={ios} clat_mean={clat:.1f}us p50={p50:.1f} "
      f"p99={p99:.1f} p99.9={p999:.1f} runtime={runtime_s:.0f}s {flat}")

c0, c1 = s0["daemon_cpu_ns_by_class"], s1["daemon_cpu_ns_by_class"]
tot = d("daemon_cpu_ns")
cls = {k: int(c1[k]) - int(c0.get(k, 0)) for k in c1}
print(f"   cpu: total {tot/1e9:.2f} s = {tot/ios/1000 if ios else 0:.2f} us/op; "
      + " ".join(f"{k}={v/1e9:.2f}s" for k, v in sorted(cls.items(), key=lambda x: -x[1]) if v))

keys = ["read_zc_serve_bytes", "fuse3_zc_replies", "fuse3_read_inplace_replies", "ranged_reads",
        "read_dest_lease_bytes", "read_copy_dest_bytes", "read_copy_bounce_bytes", "read_dest_dma_bytes",
        "ipc_arena_copy_bytes", "ipc_ops_read", "ipc_bytes_out", "ipc_fast_path_serves", "ipc_async_handoffs",
        "ipc_direct_inline_reaps", "ipc_direct_reap_stalls", "cache_hits", "cache_misses",
        "transport_wake_writes", "transport_wakes_elided", "transport_commit_flushes", "transport_commits_submitted",
        "transport_park_backstop_ticks", "fuse3_fused_midpass_reaps", "fuse3_fused_passbottom_reaps",
        "transport_worker_passes", "transport_worker_enters", "transport_worker_parks",
        "transport_worker_eventfd_drains", "transport_worker_eventfd_reads",
        "dev_enters", "dev_wake_batches", "dev_fills",
        "invariant_tripwires", "fuse_op_watchdog_overdue", "transport_cq_overflows", "transport_lease_overlong",
        "op_trace_samples", "op_trace_dropped"]
print("   ledger:", " ".join(f"{k}={d(k)}" for k in keys if k in s1))
if ios:
    per = {k: d(k) / ios for k in ["transport_worker_passes", "transport_worker_enters", "transport_worker_parks",
                                    "transport_worker_eventfd_reads", "transport_wake_writes", "dev_enters",
                                    "dev_wake_batches"] if k in s1}
    if per:
        print("   per-op:", " ".join(f"{k}={v:.3f}" for k, v in per.items()))

for fam in ["zc_bridge_phase_ns", "read_serve_phase_ns", "read_fill_phase_ns", "read_transport_phase_ns",
            "fuse3_fused_timeline_ns", "ipc_direct_phase_ns"]:
    rows = phase(fam)
    if rows:
        print(f"   {fam}: " + " ".join(f"{ph}={m:.1f}us(n={c})" for ph, c, m in rows))
ing = phase("ipc_ingress_ns") if isinstance(s1.get("ipc_ingress_ns"), dict) and "count" in s1.get("ipc_ingress_ns", {}) else []
if "ipc_ingress_ns" in s1 and isinstance(s1["ipc_ingress_ns"], dict) and "count" in s1["ipc_ingress_ns"]:
    c = int(s1["ipc_ingress_ns"]["count"]) - int(s0["ipc_ingress_ns"].get("count", 0))
    s = int(s1["ipc_ingress_ns"]["sum_ns"]) - int(s0["ipc_ingress_ns"].get("sum_ns", 0))
    if c:
        print(f"   ipc_ingress_ns: {s/c/1000:.1f}us(n={c})")
