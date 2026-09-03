#!/usr/bin/env python3
"""Per-row delta table for the zc-ahead-yield field bracket.

stats0/stats1 (.stats JSON) + fio JSON → one ROW line (fio) + the
engagement ledger the lever is judged on: the speculative-issue arms
(`prefetch_issued` / `read_lane_fetches` / `read_lane_holds` /
`read_lane_serves`), the spiral detectors (`read_lane_hold_evicted_
unconsumed`, `prefetch_wasted`, `prefetch_evicted_unconsumed`), the
serve-venue split (`read_zc_serve_bytes` vs `read_copy_warm_serve_bytes`
— the lever trades zc device→page DMA for hold hits that copy), the R-2/
R-3 dispatch partition, daemon CPU per op by class, tripwires. Every phase
mean is EXACT (Δsum_ns / Δcount). Usage: row_delta.py <results dir> <label>
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
rw = "read" if sum(j["read"]["total_ios"] for j in jobs) else "write"
iops = sum(j[rw]["iops"] for j in jobs)
ios = sum(j[rw]["total_ios"] for j in jobs)
ubytes = sum(j[rw]["io_bytes"] for j in jobs)
clat = statistics.mean(j[rw]["clat_ns"]["mean"] for j in jobs) / 1000
p50 = statistics.mean(j[rw]["clat_ns"]["percentile"]["50.000000"] for j in jobs) / 1000
p99 = max(j[rw]["clat_ns"]["percentile"]["99.000000"] for j in jobs) / 1000
p999 = max(j[rw]["clat_ns"]["percentile"]["99.900000"] for j in jobs) / 1000
bw = sum(j[rw]["bw_bytes"] for j in jobs)
runtime_s = max(j[rw]["runtime"] for j in jobs) / 1000

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
        flat = f"first/last third {a/1024/1024:.2f}/{c/1024/1024:.2f} GiB/s ({(c-a)/a*100:+.1f} %)"
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


print(f"ROW {label}: {rw} bw={bw/2**30:.2f}GiB/s ({bw/1e9:.2f}GB/s) iops={iops:.0f} ops={ios} "
      f"clat_mean={clat:.1f}us p50={p50:.1f} p99={p99:.1f} p99.9={p999:.1f} runtime={runtime_s:.0f}s {flat}")

c0, c1 = s0.get("daemon_cpu_ns_by_class", {}), s1.get("daemon_cpu_ns_by_class", {})
tot = d("daemon_cpu_ns")
cls = {k: int(c1[k]) - int(c0.get(k, 0)) for k in c1}
print(f"   cpu: total {tot/1e9:.2f} s = {tot/ios/1000 if ios else 0:.2f} us/op = {tot/ubytes*1e9/1e9 if ubytes else 0:.3f} s/GB; "
      + " ".join(f"{k}={v/1e9:.2f}s" for k, v in sorted(cls.items(), key=lambda x: -x[1]) if v))

eng = ["prefetch_issued", "prefetch_completed", "prefetch_wasted", "prefetch_evicted_unconsumed",
       "read_lane_fetches", "read_lane_fetch_bytes", "read_lane_holds", "read_lane_serves", "read_lane_serve_bytes",
       "read_lane_hold_retired", "read_lane_hold_evicted_unconsumed", "read_lane_hold_ahead_evictions",
       "read_lane_covered_skips", "read_lane_wasted", "read_lane_depth_probe_ups", "read_lane_depth_probe_backoffs",
       "singleflight_waiter_result_serves", "read_admission_evicted_unhit"]
print("   ahead:", " ".join(f"{k}={d(k)}" for k in eng if k in s1),
      f"| gauges: read_lane_depth_target={s1.get('read_lane_depth_target')} read_lane_hold_bytes={s1.get('read_lane_hold_bytes')} "
      f"prefetch_window_hwm={s1.get('prefetch_window_hwm')} read_lane_armed={s1.get('read_lane_armed')}")

serve = ["read_zc_serve_bytes", "read_dest_lease_bytes", "read_dest_dma_bytes", "read_copy_dest_bytes",
         "read_copy_warm_serve_bytes", "read_copy_bounce_bytes", "read_fill_dma_bytes", "nt_read_serve_bytes",
         "hot_block_hits", "hot_block_misses", "cache_hits", "cache_misses", "ranged_reads", "ranged_read_bytes",
         "stale_binding_rebinds", "stale_binding_escalations"]
print("   serve:", " ".join(f"{k}={d(k)}" for k in serve if k in s1), f"| user_bytes={ubytes}")

disp = ["fuse3_zc_replies", "fuse3_read_inplace_replies", "transport_fast_dispatch_serves",
        "transport_fast_dispatch_demotes", "fuse3_zc_read_fusions", "fuse3_zc_read_fusion_demotions",
        "transport_wake_writes", "transport_wakes_elided", "transport_worker_passes", "transport_worker_enters",
        "transport_worker_parks", "dev_enters", "dev_fills"]
print("   dispatch:", " ".join(f"{k}={d(k)}" for k in disp if k in s1))

il = ["ipc_ops_read", "ipc_bytes_out", "ipc_fast_path_serves", "ipc_async_handoffs", "ipc_hold_probe_serves",
      "ipc_hold_probe_misses", "ipc_arena_copy_bytes", "ipc_read_dest_serves", "ipc_direct_inline_reaps",
      "ipc_direct_reap_stalls", "ipc_sessions_active", "ipc_service_threads"]
if d("ipc_ops_read"):
    print("   il:", " ".join(f"{k}={d(k)}" for k in il if k in s1))

trip = ["invariant_tripwires", "fuse_op_watchdog_overdue", "transport_cq_overflows", "transport_lease_overlong",
        "read_dest_overruns", "transport_requests_abandoned", "fuse3_zc_bridge_cancels", "detached_task_panics",
        "data_dma_fence_refusals", "write_path_seed_read_bytes"]
print("   tripwires:", " ".join(f"{k}={d(k)}" for k in trip if k in s1))

mem = ["mem_budget_level", "mem_budget_red_events", "mem_budget_yellow_events", "mem_budget_hard_backstops"]
print("   mem:", " ".join(f"{k}={s1.get(k)}" for k in mem if k in s1))

for fam in ["read_serve_phase_ns", "read_fill_phase_ns", "read_transport_phase_ns", "zc_bridge_phase_ns",
            "transport_reap_gap_ns", "ipc_direct_phase_ns"]:
    rows = phase(fam)
    if rows:
        print(f"   {fam}: " + " ".join(f"{ph}={m:.1f}us(n={c})" for ph, c, m in rows))
