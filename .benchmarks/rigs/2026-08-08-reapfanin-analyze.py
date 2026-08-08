#!/usr/bin/env python3
"""Reap-fanin bracket analyzer (2026-08-08): per-row engagement gates
(FATAL) + the row table. See 2026-08-08-reapfanin-rig.sh for the gate
list. The tail-multiplier metric is client clat p99 ÷ daemon inflight
p99 (bucket-bounded — the local box's low fabric RTT makes the MEAN win
small, so the multiplier is the local instrument; stated per bracket)."""
import json
import sys
import glob

BUCKET_US = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2000, 4000,
             8000, 16000, 32000, 64000, 128000, 256000, 512000, 1024000,
             2000000, 4000000, 8000000, 16000000, 32000000]
LABELS = ["<=1us", "<=2us", "<=4us", "<=8us", "<=16us", "<=32us",
          "<=64us", "<=128us", "<=256us", "<=512us", "<=1024us", "<=2ms",
          "<=4ms", "<=8ms", "<=16ms", "<=32ms", "<=64ms", "<=128ms",
          "<=256ms", "<=512ms", "<=1024ms", "<=2s", "<=4s", "<=8s",
          "<=16s", ">16s"]


def metrics(path):
    d = json.load(open(path))
    return d.get("metrics", d)


def hist_delta(pre, post, key, sub=None):
    a = pre.get(key, {})
    b = post.get(key, {})
    if sub is not None:
        a = a.get(sub, {})
        b = b.get(sub, {})
    return [int(b.get(l, 0)) - int(a.get(l, 0)) for l in LABELS]


def hist_stats(delta):
    n = sum(delta)
    if n == 0:
        return (0, None, None)

    def pct(p):
        want = p * n
        run = 0
        for i, c in enumerate(delta):
            run += c
            if run >= want:
                return BUCKET_US[min(i, len(BUCKET_US) - 1)]
        return BUCKET_US[-1]
    return (n, pct(0.50), pct(0.99))


def counter_delta(pre, post, key):
    return int(post.get(key, 0)) - int(pre.get(key, 0))


def ingress_count(m):
    h = m.get("ipc_ingress_ns", {})
    return sum(int(v) for v in h.values()) if isinstance(h, dict) else 0


def analyze(out, leg, qd):
    pre = metrics(f"{out}/leg{leg}-qd{qd}-pre.json")
    post = metrics(f"{out}/leg{leg}-qd{qd}-post.json")
    fio = json.load(open(f"{out}/leg{leg}-qd{qd}-fio.json"))
    rd = fio["jobs"][0]["read"]
    ios = rd["total_ios"]
    iops = rd["iops"]
    clat = rd["clat_ns"]
    p = clat.get("percentile", {})
    p50 = p.get("50.000000", 0) / 1000.0
    p99 = p.get("99.000000", 0) / 1000.0
    p999 = p.get("99.900000", 0) / 1000.0

    ops = counter_delta(pre, post, "ipc_ops_read")
    dd = counter_delta(pre, post, "ipc_direct_drive_serves")
    fallb = counter_delta(pre, post, "ipc_direct_drive_fallbacks_post")
    wakes = counter_delta(pre, post, "ipc_cqe_wake_writes")
    elided = counter_delta(pre, post, "ipc_cqe_wake_elided")
    inline = counter_delta(pre, post, "ipc_direct_inline_reaps")
    ing = ingress_count(post) - ingress_count(pre)

    # --- FATAL gates ---------------------------------------------------
    fails = []
    if ops < 0.99 * ios:
        fails.append(f"silent passthrough: ipc_ops_read {ops} < 0.99×ios {ios}")
    for trip in ("ipc_sessions_poisoned", "ipc_descriptor_rejects",
                 "ipc_direct_reap_stalls", "transport_lease_overlong"):
        d = counter_delta(pre, post, trip)
        if d != 0:
            fails.append(f"tripwire {trip} moved by {d}")
    if leg.startswith("C") and ing < 0.99 * ops:
        fails.append(f"ingress instrument dark: {ing} < 0.99×ops {ops}")
    if leg.startswith("B") and ing != 0:
        fails.append(f"base leg recorded ingress samples ({ing}) — pair skew?")

    n_i, i50, i99 = hist_stats(hist_delta(pre, post, "ipc_direct_phase_ns", "inflight"))
    n_t, t50, t99 = hist_stats(hist_delta(pre, post, "ipc_direct_phase_ns", "total"))
    gi = hist_delta(pre, post, "ipc_ingress_ns") if not isinstance(
        pre.get("ipc_ingress_ns"), (int, float, type(None))) else [0] * 26
    n_g, g50, g99 = hist_stats(gi)

    tailx = (p99 / i99) if (i99 and p99) else None

    row = {
        "leg": leg, "qd": int(qd), "ios": ios, "iops": round(iops),
        "clat_us": {"p50": round(p50, 1), "p99": round(p99, 1), "p999": round(p999, 1)},
        "ops": ops, "dd_serves": dd, "dd_fallbacks_post": fallb,
        "wakes": wakes, "wakes_elided": elided,
        "wakes_per_op": round(wakes / max(ops, 1), 4),
        "inline_reaps": inline,
        "inflight_us": {"n": n_i, "p50": i50, "p99": i99},
        "total_us": {"n": n_t, "p50": t50, "p99": t99},
        "ingress_us": {"n": n_g, "p50": g50, "p99": g99},
        "tail_multiplier_p99": round(tailx, 2) if tailx else None,
    }
    # thirds flatness from the per-second iops logs
    secs = {}
    for f in glob.glob(f"{out}/leg{leg}-qd{qd}_iops.*.log"):
        for line in open(f):
            parts = line.strip().split(",")
            if len(parts) >= 2:
                t = int(parts[0]) // 1000
                secs[t] = secs.get(t, 0) + int(parts[1])
    if secs:
        ts = sorted(secs)
        vals = [secs[t] for t in ts]
        third = max(len(vals) // 3, 1)
        f1 = sum(vals[:third]) / third
        f3 = sum(vals[-third:]) / third
        row["thirds"] = {"first": round(f1), "last": round(f3),
                         "decay_pct": round(100 * (f3 - f1) / max(f1, 1), 1)}
    json.dump(row, open(f"{out}/leg{leg}-qd{qd}-row.json", "w"), indent=1)
    print(json.dumps(row))
    if fails:
        for f in fails:
            print(f"ENGAGEMENT FAIL: {f}", file=sys.stderr)
        sys.exit(1)


def table(out, legs):
    print(f"{'leg':>4} {'qd':>3} {'IOPS':>9} {'clat p50':>9} {'p99':>9} "
          f"{'p99.9':>9} {'infl p50/p99':>13} {'ingr p50/p99':>13} "
          f"{'wk/op':>6} {'tail×':>6} {'decay%':>7}")
    for leg in legs:
        for qd in (8, 32):
            try:
                r = json.load(open(f"{out}/leg{leg}-qd{qd}-row.json"))
            except FileNotFoundError:
                continue
            th = r.get("thirds", {})
            print(f"{r['leg']:>4} {r['qd']:>3} {r['iops']:>9} "
                  f"{r['clat_us']['p50']:>9} {r['clat_us']['p99']:>9} "
                  f"{r['clat_us']['p999']:>9} "
                  f"{str(r['inflight_us']['p50'])+'/'+str(r['inflight_us']['p99']):>13} "
                  f"{str(r['ingress_us']['p50'])+'/'+str(r['ingress_us']['p99']):>13} "
                  f"{r['wakes_per_op']:>6} {str(r['tail_multiplier_p99']):>6} "
                  f"{str(th.get('decay_pct', '-')):>7}")


if __name__ == "__main__":
    if sys.argv[2] == "table":
        table(sys.argv[1], sys.argv[3:])
    else:
        analyze(sys.argv[1], sys.argv[2], sys.argv[3])
