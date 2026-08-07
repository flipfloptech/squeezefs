#!/usr/bin/env python3
# Set 2 per-row FATAL engagement gates + table lines (hybrid lane gate field
# confirmation, 2026-08-07). Host python is 3.6 — keep it 3.6-clean.
# usage: set2_gate.py OUT TAG KIND [ARG]
#   KIND: lane_write | lane_read | ring_rand_read | ring_seq_read |
#         dd_sticky COUNT | medians | sustained
import glob
import json
import statistics
import sys

OUT, TAG, KIND = sys.argv[1], sys.argv[2], sys.argv[3]
ARG = sys.argv[4] if len(sys.argv) > 4 else None
DATA_BLK = ["nvme10n1", "nvme12n1", "nvme14n1", "nvme16n1", "nvme18n1"]
try:
    DATA_BLK = [ln.strip() for ln in open("%s/datadevs.txt" % OUT) if ln.strip()]
except OSError:
    pass


def load(p):
    with open(p) as f:
        d = json.load(f)
    return d.get("metrics", d)


def delta(b, a, k):
    return a.get(k, 0) - b.get(k, 0)


def fio_side(path, side):
    f = json.load(open(path))
    s = [j[side] for j in f["jobs"]]
    return (sum(x["bw_bytes"] for x in s), sum(x["io_bytes"] for x in s),
            sum(x["total_ios"] for x in s))


def amp_cols(tag, user_bytes):
    try:
        def rd(p):
            m = {}
            for ln in open(p):
                parts = ln.split()
                if len(parts) >= 14 and parts[2] in DATA_BLK:
                    m[parts[2]] = (int(parts[7]), int(parts[9]))
            return m
        db = rd("%s/%s-ds-before.txt" % (OUT, tag))
        da = rd("%s/%s-ds-after.txt" % (OUT, tag))
        wb = sum((da[d][1] - db[d][1]) * 512 for d in da if d in db)
        wi = sum(da[d][0] - db[d][0] for d in da if d in db)
        if user_bytes and wi:
            return "amp=%.3f wareq-sz~%.0fKiB" % (wb / user_bytes, wb / wi / 1024.0)
    except OSError:
        pass
    return "amp=n/a"


fails = []

if KIND in ("lane_write", "lane_read", "ring_rand_read", "ring_seq_read", "sustained"):
    b = load("%s/%s-before.json" % (OUT, TAG))
    a = load("%s/%s-after.json" % (OUT, TAG))
    side = "write" if KIND == "lane_write" else "read"
    bw, io, ios = fio_side("%s/%s-fio.json" % (OUT, TAG), side)
    lane_b = delta(b, a, "ipc_lane_gate_kernel_bytes")
    lane_r = delta(b, a, "ipc_lane_gate_kernel_routes")
    ring_in = delta(b, a, "ipc_bytes_in")
    ring_out = delta(b, a, "ipc_bytes_out")
    ops_r = delta(b, a, "ipc_ops_read")
    ops_w = delta(b, a, "ipc_ops_write")

    if KIND == "lane_write":
        if lane_b < 0.99 * io:
            fails.append("lane bytes %d < 0.99 x row %d" % (lane_b, io))
        if ring_in > 0.01 * io:
            fails.append("ring bytes_in %d > 0.01 x row" % ring_in)
        print("  %s: %.2f GB/s WRITE (%.1f GiB) lane_bytes=%d (%.4f x row) ring_in=%d %s"
              % (TAG, bw / 1e9, io / 2**30, lane_b, lane_b / io if io else 0,
                 ring_in, amp_cols(TAG, io)))
    elif KIND in ("lane_read", "sustained"):
        if lane_b < 0.99 * io:
            fails.append("lane bytes %d < 0.99 x row %d" % (lane_b, io))
        if ring_out > 0.01 * io:
            fails.append("ring bytes_out %d > 0.01 x row" % ring_out)
        print("  %s: %.2f GB/s READ (%.1f GiB) lane_bytes=%d (%.4f x row) ring_out=%d"
              % (TAG, bw / 1e9, io / 2**30, lane_b, lane_b / io if io else 0, ring_out))
        if KIND == "sustained":
            # thirds from the fio bw logs (KiB/s samples, 1 s buckets)
            per_t = {}
            for lf in glob.glob("%s/S_bw.*.log" % OUT):
                for ln in open(lf):
                    p = ln.split(",")
                    if len(p) >= 2:
                        t = int(p[0]) // 1000
                        per_t[t] = per_t.get(t, 0) + int(p[1])
            ts = sorted(per_t)
            if len(ts) >= 9:
                n = len(ts)
                th = [statistics.mean([per_t[t] for t in ts[i * n // 3:(i + 1) * n // 3]])
                      * 1024 / 1e9 for i in range(3)]
                print("    thirds (GB/s): %.2f / %.2f / %.2f" % (th[0], th[1], th[2]))
                if min(th) < 0.8 * max(th):
                    fails.append("sustained row not flat across thirds: %s" % th)
            else:
                fails.append("sustained row: no bw log samples")
    elif KIND == "ring_rand_read":
        iops = bw / 4096  # bw_bytes/s over 4k
        if ops_r < 0.99 * ios:
            fails.append("ring ops_read %d < 0.99 x row ios %d" % (ops_r, ios))
        if lane_r > 8:
            fails.append("lane routes %d not ~0 on the 4k row" % lane_r)
        print("  %s: %s IOPS rand-4k READ ring ops=%d/%d lane_routes=%d"
              % (TAG, format(int(iops), ","), ops_r, ios, lane_r))
    elif KIND == "ring_seq_read":
        if ops_r < 0.99 * ios:
            fails.append("OFF leg ring ops_read %d < 0.99 x row ios %d" % (ops_r, ios))
        print("  %s: %.2f GB/s READ (OFF/all-ring) ring ops=%d/%d bytes_out=%d lane_routes=%d"
              % (TAG, bw / 1e9, ops_r, ios, ring_out, lane_r))

elif KIND == "dd_sticky":
    count = int(ARG)
    b = load("%s/%s-before.json" % (OUT, TAG))
    a = load("%s/%s-after.json" % (OUT, TAG))
    lane_r = delta(b, a, "ipc_lane_gate_kernel_routes")
    ops_w = delta(b, a, "ipc_ops_write")
    if lane_r != count:
        fails.append("sticky routes %d != dd count %d" % (lane_r, count))
    if ops_w != 0:
        fails.append("ring ops_write %d != 0 on the latched stream" % ops_w)
    print("  %s: dd sticky routes=%d (== count %d) ring writes=%d %s"
          % (TAG, lane_r, count, ops_w, amp_cols(TAG, count * 2**20)))

elif KIND == "medians":
    arms = ["ON", "OFF", "OFF", "ON", "ON", "OFF"]
    vals = {"ON": [], "OFF": []}
    for i, arm in enumerate(arms, 1):
        bw, io, ios = fio_side("%s/AB%d%s-fio.json" % (OUT, i, arm), "read")
        vals[arm].append(bw / 1e9)
    mon = statistics.median(vals["ON"])
    moff = statistics.median(vals["OFF"])
    print("  AB verdict: ON legs %s -> median %.2f GB/s; OFF legs %s -> median %.2f GB/s; ratio ON/OFF = %.2fx"
          % (["%.2f" % v for v in vals["ON"]], mon,
             ["%.2f" % v for v in vals["OFF"]], moff, mon / moff if moff else 0))

if fails:
    print("  ENGAGEMENT FAIL (%s): %s" % (TAG, "; ".join(fails)))
    sys.exit(1)
