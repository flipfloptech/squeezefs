#!/usr/bin/env python3
"""Per-row delta table for the R-4 rig: stats0/stats1 (.stats JSON) + fio JSON.

R-3's table plus the reap-thread economy ledger: the spin words
(`transport_spin_{absorbed,expired,ns,refused_busy,window_us}` — absorbed
= parks the spin deleted, ns = the CPU it paid), the commit-batch words,
the reap-gap counts per op (enters / parks / CQEs) and their exact means,
and the box-wide busy % when the driver left a `/proc/stat` pair.

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
ops_cpu = max(d("fuse3_read_inplace_replies") + d("transport_fast_dispatch_serves"), d("ipc_ops_read"), ios, 1)
# Two divisors, stated: fio ops (row only — the R-2/R-3 notes' column) and
# the stats population (row + ramp — the honest per-op cost).
print(f"   cpu: total {tot/1e9:.2f} s = {tot/ios/1000 if ios else 0:.2f} us/fio-op | {tot/ops_cpu/1000:.2f} us/stats-op (stats ops {ops_cpu}); "
      + " ".join(f"{k}={v/1e9:.2f}s" for k, v in sorted(cls.items(), key=lambda x: -x[1]) if v))

keys = ["read_zc_serve_bytes", "fuse3_zc_replies", "fuse3_read_inplace_replies", "ranged_reads",
        "read_dest_lease_bytes", "read_copy_dest_bytes", "read_copy_bounce_bytes", "read_dest_dma_bytes",
        "ipc_arena_copy_bytes", "ipc_ops_read", "ipc_bytes_out", "ipc_fast_path_serves", "ipc_async_handoffs",
        "ipc_direct_inline_reaps", "ipc_direct_reap_stalls", "cache_hits", "cache_misses",
        "transport_wake_writes", "transport_wakes_elided", "transport_commit_flushes", "transport_commits_submitted",
        "transport_park_backstop_ticks", "fuse3_fused_midpass_reaps", "fuse3_fused_passbottom_reaps",
        "transport_spin_absorbed", "transport_spin_expired", "transport_spin_ns", "transport_spin_refused_busy",
        "transport_commit_batch_flushes", "transport_commit_batch_commits",
        "transport_worker_passes", "transport_worker_enters", "transport_worker_parks",
        "transport_worker_eventfd_drains", "transport_worker_eventfd_reads",
        "dev_enters", "dev_wake_batches", "dev_fills",
        "invariant_tripwires", "fuse_op_watchdog_overdue", "transport_cq_overflows", "transport_lease_overlong",
        "op_trace_samples", "op_trace_dropped"]
print("   ledger:", " ".join(f"{k}={d(k)}" for k in keys if k in s1))
# Per-op divisors: the STATS population (row + ramp — the fio `ios` excludes
# the ramp, which would inflate every per-op ratio by the ramp's share).
ops_stats = max(d("fuse3_read_inplace_replies") + d("transport_fast_dispatch_serves"), d("ipc_ops_read"), 1)
if ios:
    per = {k: d(k) / ops_stats for k in ["transport_wake_writes", "transport_spin_absorbed", "transport_spin_expired",
                                    "transport_commit_batch_flushes", "dev_enters", "dev_wake_batches"] if k in s1}
    rg0, rg1 = s0.get("transport_reap_gap_ns", {}), s1.get("transport_reap_gap_ns", {})
    for ph in ["blind", "blind_cqe", "park"]:
        if ph in rg1:
            per[f"reap_{ph}"] = (int(rg1[ph]["count"]) - int(rg0.get(ph, {}).get("count", 0))) / ops_stats
    if per:
        print("   per-op:", " ".join(f"{k}={v:.3f}" for k, v in per.items()))
    spin_ns = d("transport_spin_ns")
    if "transport_spin_ns" in s1:
        print(f"   spin: absorbed={d('transport_spin_absorbed')} expired={d('transport_spin_expired')} "
              f"spin_us/op={spin_ns/ops_stats/1000:.2f} refused_busy={d('transport_spin_refused_busy')} "
              f"window_us={s1.get('transport_spin_window_us')}")
    cb = d("transport_commit_batch_commits"); cf = d("transport_commit_batch_flushes")
    if cf:
        print(f"   commit batch: commits/flush={cb/cf:.2f} flushes/op={cf/ops_stats:.3f} (ops_stats={ops_stats})")
import os
ps0, ps1 = f"{res}/{label}.procstat0", f"{res}/{label}.procstat1"
if os.path.exists(ps0) and os.path.exists(ps1):
    a = [int(x) for x in open(ps0).read().split()[1:]]; b = [int(x) for x in open(ps1).read().split()[1:]]
    dd = [y - x for x, y in zip(a, b)]; tot = sum(dd); idle = dd[3] + dd[4]
    if tot:
        print(f"   box cpu: busy {100*(tot-idle)/tot:.1f}% (user {100*dd[0]/tot:.1f} sys {100*dd[2]/tot:.1f} "
              f"irq {100*dd[5]/tot:.1f} softirq {100*dd[6]/tot:.1f})")

for fam in ["zc_bridge_phase_ns", "read_serve_phase_ns", "read_fill_phase_ns", "read_transport_phase_ns",
            "fuse3_fused_timeline_ns", "ipc_direct_phase_ns", "transport_reap_gap_ns"]:
    rows = phase(fam)
    if rows:
        print(f"   {fam}: " + " ".join(f"{ph}={m:.1f}us(n={c})" for ph, c, m in rows))
ing = phase("ipc_ingress_ns") if isinstance(s1.get("ipc_ingress_ns"), dict) and "count" in s1.get("ipc_ingress_ns", {}) else []
if "ipc_ingress_ns" in s1 and isinstance(s1["ipc_ingress_ns"], dict) and "count" in s1["ipc_ingress_ns"]:
    c = int(s1["ipc_ingress_ns"]["count"]) - int(s0["ipc_ingress_ns"].get("count", 0))
    s = int(s1["ipc_ingress_ns"]["sum_ns"]) - int(s0["ipc_ingress_ns"].get("sum_ns", 0))
    if c:
        print(f"   ipc_ingress_ns: {s/c/1000:.1f}us(n={c})")
