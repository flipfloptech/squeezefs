#!/usr/bin/env python3
"""Per-row delta table for the R-1 rig: stats0/stats1 (.stats JSON) + fio JSON.

Every phase mean is EXACT (Δsum_ns / Δcount — the A1 histograms), never a
bucket midpoint. Usage: row_delta.py <results dir> <label>
"""
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
bw = sum(j["read"]["bw_bytes"] for j in jobs) / 2**30
runtime_s = max(j["read"]["runtime"] for j in jobs) / 1000

# Sustained flatness: first vs last third of the aggregate bw log.
flat = ""
try:
    import glob
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
    for ph in f1:
        c = int(f1[ph]["count"]) - int(f0.get(ph, {}).get("count", 0))
        s = int(f1[ph]["sum_ns"]) - int(f0.get(ph, {}).get("sum_ns", 0))
        if c:
            out.append((ph, c, s / c / 1000))
    return out

print(f"ROW {label}: iops={iops:.0f} bw={bw:.2f}GiB/s ops={ios} clat_mean={clat:.1f}us p50={p50:.1f} p99={p99:.1f} runtime={runtime_s:.0f}s {flat}")

# CPU per op / per byte by class.
c0, c1 = s0["daemon_cpu_ns_by_class"], s1["daemon_cpu_ns_by_class"]
tot = d("daemon_cpu_ns")
cls = {k: int(c1[k]) - int(c0.get(k, 0)) for k in c1}
print(f"   cpu: total {tot/1e9:.2f} s = {tot/ios/1000 if ios else 0:.2f} us/op = {tot/max(ubytes,1)*4096/1000:.2f} us/4KiB; "
      + " ".join(f"{k}={v/1e9:.2f}s" for k, v in sorted(cls.items(), key=lambda x: -x[1]) if v))

# Engagement + the timer class.
keys = ["timer_arms", "timer_tombstones_skipped", "transport_timer_arms", "transport_timer_tombstones_skipped",
        "read_zc_serve_bytes", "fuse3_zc_replies", "read_odirect_requests", "get_obj", "ranged_reads", "ranged_read_bytes",
        "read_dest_lease_bytes", "read_copy_dest_bytes", "read_copy_bounce_bytes", "read_copy_warm_serve_bytes",
        "read_dest_dma_bytes", "ipc_arena_copy_bytes", "ipc_ops_read", "ipc_bytes_out", "ipc_direct_reads",
        "ipc_fast_path_serves", "ipc_async_handoffs", "cache_hits", "cache_misses",
        "singleflight_waiter_result_serves", "read_fill_publishes_skipped", "channel_ticked_reregisters",
        "lock_ticked_reregisters", "lane_exec_tick_rescues", "uring_queue_full", "invariant_tripwires",
        "fuse_op_watchdog_overdue", "transport_cq_overflows"]
print("   ledger:", " ".join(f"{k}={d(k)}" for k in keys if k in s1))
print(f"   timer(ipc): arms/op={d('timer_arms')/ios if ios else 0:.3f} tombstones/op={d('timer_tombstones_skipped')/ios if ios else 0:.3f} "
      f"heap={s1.get('timer_heap_entries')} live={s1.get('timer_live_sleeps')}")
print(f"   timer(fuse3): arms/op={d('transport_timer_arms')/ios if ios else 0:.3f} tombstones/op={d('transport_timer_tombstones_skipped')/ios if ios else 0:.3f} "
      f"heap={s1.get('transport_timer_heap_entries')} live={s1.get('transport_timer_live_sleeps')}")

for fam in ["read_fill_phase_ns", "read_serve_phase_ns", "read_transport_phase_ns", "ipc_direct_phase_ns"]:
    rows = phase(fam)
    if rows:
        print(f"   {fam}: " + " ".join(f"{ph}={m:.1f}us(n={c})" for ph, c, m in rows))
ing = s1.get("ipc_ingress_ns")
if ing and int(ing["count"]) - int(s0["ipc_ingress_ns"]["count"]):
    c = int(ing["count"]) - int(s0["ipc_ingress_ns"]["count"])
    s = int(ing["sum_ns"]) - int(s0["ipc_ingress_ns"]["sum_ns"])
    print(f"   ipc_ingress_ns: mean={s/c/1000:.1f}us (n={c})")
