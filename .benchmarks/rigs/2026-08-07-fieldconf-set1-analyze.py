#!/usr/bin/env python3
# Set 1 per-leg analysis + FATAL engagement gates (write-lane fan-out field
# confirmation, 2026-08-07). Host python is 3.6 — keep it 3.6-clean.
# usage: set1_analyze.py OUT LEG MODE RAMP_SECS
import json
import re
import sys

OUT, LEG, MODE, RAMP = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
DATA_BLK = ["nvme10n1", "nvme12n1", "nvme14n1", "nvme16n1", "nvme18n1"]
try:
    DATA_BLK = [ln.strip() for ln in open("%s/leg%s-datadevs.txt" % (OUT, LEG)) if ln.strip()]
except OSError:
    pass


def load(p):
    with open(p) as f:
        d = json.load(f)
    return d.get("metrics", d)


b = load("%s/leg%s-before.json" % (OUT, LEG))
a = load("%s/leg%s-after.json" % (OUT, LEG))
fio = json.load(open("%s/leg%s-fio.json" % (OUT, LEG)))

fails = []

# --- fio row -------------------------------------------------------------
w = [j["write"] for j in fio["jobs"]]
bw = sum(x["bw_bytes"] for x in w) / 1e9
io_bytes = sum(x["io_bytes"] for x in w)
runtime_s = max(x["runtime"] for x in w) / 1000.0

# --- lane engagement (FATAL) --------------------------------------------
def lanes(snap):
    out = {}
    for e in snap.get("data_write_lane_submits", []):
        dev, vals = e.split("=", 1)
        out[dev] = [int(x) for x in vals.split(",")]
    return out


lb, la = lanes(b), lanes(a)
lane_rows = []
multi = 0
devs_with_traffic = 0
for dev in sorted(la):
    before = lb.get(dev, [])
    before = before + [0] * (len(la[dev]) - len(before))
    deltas = [x - y for x, y in zip(la[dev], before)]
    moved = sum(1 for d in deltas if d > 0)
    if sum(deltas) > 0:
        devs_with_traffic += 1
        if moved > 1:
            multi += 1
        elif MODE in ("A", "Z"):
            fails.append("armed leg but <=1 lane moved on %s: %s" % (dev, deltas))
    if MODE == "B" and any(d > 0 for d in deltas[1:]):
        fails.append("%s: B leg moved lanes beyond 0: %s" % (dev, deltas))
    lane_rows.append("    %s lanes %s" % (dev, deltas))
if devs_with_traffic == 0:
    fails.append("no data device moved any write lane")

# --- governor + tripwires ------------------------------------------------
gov = {
    "depth_target": a.get("write_pipeline_depth_target", 0),
    "depth_base": a.get("write_pipeline_depth_target_base", 0),
    "probe_ups": a.get("write_pipeline_depth_probe_ups", 0) - b.get("write_pipeline_depth_probe_ups", 0),
    "backoffs": a.get("write_pipeline_depth_probe_backoffs", 0) - b.get("write_pipeline_depth_probe_backoffs", 0),
}
fence = a.get("data_dma_fence_refusals", 0) - b.get("data_dma_fence_refusals", 0)
if fence != 0:
    fails.append("data_dma_fence_refusals moved: %d" % fence)

# --- dma phase mode ------------------------------------------------------
def dma_hist(snap):
    return snap.get("write_pipeline_phase_ns", {}).get("dma", {})


hd = {}
hb, ha = dma_hist(b), dma_hist(a)
for k in ha:
    d = ha[k] - hb.get(k, 0)
    if d > 0:
        hd[k] = d
top = sorted(hd.items(), key=lambda kv: -kv[1])[:3]
dma_mode = ", ".join("%s:%d" % kv for kv in top) if top else "(no dma samples)"

# --- diskstats: amplification + wareq-sz + thirds flatness ---------------
# sampler log: repeated blocks of [ts line, dev lines...]
samples = []  # (ts, {dev: (wr_ios, wr_sectors)})
cur_ts, cur = None, {}
for line in open("%s/leg%s-diskstats.log" % (OUT, LEG)):
    parts = line.split()
    if len(parts) == 1 and re.match(r"^\d+\.\d+$", parts[0]):
        if cur_ts is not None:
            samples.append((cur_ts, cur))
        cur_ts, cur = float(parts[0]), {}
    elif len(parts) >= 14 and parts[2] in DATA_BLK:
        # /proc/diskstats: fields 8=writes completed, 10=sectors written (1-based
        # after the 3 id fields): parts[7]=wr_ios parts[9]=wr_sectors
        cur[parts[2]] = (int(parts[7]), int(parts[9]))
if cur_ts is not None:
    samples.append((cur_ts, cur))

amp = wareq = None
thirds = []
if len(samples) >= 3:
    t0 = samples[0][0]
    # window start = sample closest to ramp end; window end = last sample
    start = min(samples, key=lambda s: abs((s[0] - t0) - RAMP))
    end = samples[-1]
    dev_bytes = sum((end[1][d][1] - start[1][d][1]) * 512 for d in DATA_BLK if d in end[1] and d in start[1])
    dev_ios = sum(end[1][d][0] - start[1][d][0] for d in DATA_BLK if d in end[1] and d in start[1])
    amp = dev_bytes / io_bytes if io_bytes else 0.0
    wareq = dev_bytes / dev_ios / 1024.0 if dev_ios else 0.0
    # thirds over [start, start + runtime] (the fio measured window)
    win = [s for s in samples if start[0] <= s[0] <= start[0] + runtime_s + 2.5]
    if len(win) >= 4:
        n = len(win) - 1
        cut1, cut2 = win[n // 3], win[2 * n // 3]
        def wsec(s):
            return sum(s[1][d][1] for d in DATA_BLK if d in s[1])
        thirds = [
            (wsec(cut1) - wsec(win[0])) * 512 / max(cut1[0] - win[0][0], 1e-9) / 1e9,
            (wsec(cut2) - wsec(cut1)) * 512 / max(cut2[0] - cut1[0], 1e-9) / 1e9,
            (wsec(win[-1]) - wsec(cut2)) * 512 / max(win[-1][0] - cut2[0], 1e-9) / 1e9,
        ]

# --- hot nvme-tcp connections (ss bytes_acked deltas, TX = write dir) ----
def ss_parse(path):
    conns = {}
    try:
        lines = open(path).read().splitlines()
    except OSError:
        return conns
    key = None
    for ln in lines:
        m = re.search(r"(\S+:\d+)\s+(\S+:4420)\s*$", ln)
        if m:
            key = (m.group(1), m.group(2))
            continue
        if key is not None:
            m2 = re.search(r"bytes_acked:(\d+)", ln)
            if m2:
                conns[key] = int(m2.group(1))
            key = None
    return conns


s1 = ss_parse("%s/leg%s-ss1.txt" % (OUT, LEG))
s2 = ss_parse("%s/leg%s-ss2.txt" % (OUT, LEG))
hot = {}
for k in s2:
    d = s2[k] - s1.get(k, s2[k])
    if d > 100 * 1024 * 1024:  # >100 MB TX in the 15 s mid-row window
        ip = k[1].rsplit(":", 1)[0]
        hot[ip] = hot.get(ip, 0) + 1
hot_total = sum(hot.values())
hot_str = " ".join("%s=%d" % kv for kv in sorted(hot.items())) or "(none)"

# --- report ---------------------------------------------------------------
print("  leg %s (%s): fio %.2f GB/s over %.0f s measured (%.1f GiB)" % (
    LEG, MODE, bw, runtime_s, io_bytes / 2**30))
print("    data_write_lanes=%s  devices with >1 submitting lane: %d/%d" % (
    a.get("data_write_lanes"), multi, len(la)))
for r in lane_rows:
    print(r)
print("    governor: depth_target=%s base=%s probe_ups(+%s) backoffs(+%s)" % (
    gov["depth_target"], gov["depth_base"], gov["probe_ups"], gov["backoffs"]))
print("    dma phase mode (delta top buckets): %s" % dma_mode)
if amp is not None:
    print("    amplification: dev/user = %.3f  wareq-sz ~ %.0f KiB" % (amp, wareq))
if thirds:
    flat = max(thirds) / min(thirds) if min(thirds) > 0 else float("inf")
    print("    thirds (device GB/s): %.2f / %.2f / %.2f  (max/min %.2f)" % (
        thirds[0], thirds[1], thirds[2], flat))
print("    hot nvme-tcp conns (>100MB TX mid-row): total=%d  per-target %s" % (
    hot_total, hot_str))
print("    tripwires: data_dma_fence_refusals delta=%d" % fence)

if fails:
    print("  ENGAGEMENT FAIL: " + "; ".join(fails))
    sys.exit(1)
