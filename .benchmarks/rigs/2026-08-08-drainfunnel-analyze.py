#!/usr/bin/env python3
"""Drain-funnel decomposition analyzer (2026-08-08 r3): per-row
engagement gates (FATAL — the r2 set + ingress n ≡ ops) plus the funnel
ledger columns: ingress mean (bucket midpoints), drain passes, ops/pass,
per-op svc cost (pass mean ÷ ops/pass), empty passes, parks."""
import json
import sys

LABELS = ["<=1us", "<=2us", "<=4us", "<=8us", "<=16us", "<=32us",
          "<=64us", "<=128us", "<=256us", "<=512us", "<=1024us", "<=2ms",
          "<=4ms", "<=8ms", "<=16ms", "<=32ms", "<=64ms", "<=128ms",
          "<=256ms", "<=512ms", "<=1024ms", "<=2s", "<=4s", "<=8s",
          "<=16s", ">16s"]
UP = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2000, 4000, 8000,
      16000, 32000, 64000, 128000, 256000, 512000, 1024000, 2000000,
      4000000, 8000000, 16000000, 32000000]
MID = [0.5, 1.5, 3, 6, 12, 24, 48, 96, 192, 384, 768, 1512, 3000, 6000,
       12000, 24000, 48000, 96000, 192000, 384000, 768000, 1512000,
       3000000, 6000000, 12000000, 24000000]


def metrics(path):
    d = json.load(open(path))
    return d.get("metrics", d)


def hist_delta(pre, post, key, sub=None):
    a = pre.get(key, {})
    b = post.get(key, {})
    if sub is not None:
        a = a.get(sub, {}) if isinstance(a, dict) else {}
        b = b.get(sub, {}) if isinstance(b, dict) else {}
    if not isinstance(a, dict):
        a = {}
    if not isinstance(b, dict):
        b = {}
    return [int(b.get(l, 0)) - int(a.get(l, 0)) for l in LABELS]


def hstats(delta):
    n = sum(delta)
    if n == 0:
        return {"n": 0, "mean": None, "p50": None, "p99": None}
    mean = sum(c * MID[i] for i, c in enumerate(delta)) / n

    def pct(p):
        want = p * n
        run = 0
        for i, c in enumerate(delta):
            run += c
            if run >= want:
                return UP[min(i, len(UP) - 1)]
        return UP[-1]
    return {"n": n, "mean": round(mean, 1), "p50": pct(0.5), "p99": pct(0.99)}


def cd(pre, post, key):
    return int(post.get(key, 0)) - int(pre.get(key, 0))


def analyze(out, leg, qd):
    pre = metrics(f"{out}/leg{leg}-qd{qd}-pre.json")
    post = metrics(f"{out}/leg{leg}-qd{qd}-post.json")
    fio = json.load(open(f"{out}/leg{leg}-qd{qd}-fio.json"))
    rd = fio["jobs"][0]["read"]
    ios, iops = rd["total_ios"], rd["iops"]
    clat_mean = rd["clat_ns"]["mean"] / 1000.0
    p = rd["clat_ns"].get("percentile", {})
    p50 = p.get("50.000000", 0) / 1000.0
    p99 = p.get("99.000000", 0) / 1000.0

    ops = cd(pre, post, "ipc_ops_read")
    dd = cd(pre, post, "ipc_direct_drive_serves")
    ing = hstats(hist_delta(pre, post, "ipc_ingress_ns"))
    dp = hstats(hist_delta(pre, post, "ipc_drain_pass_ns"))
    infl = hstats(hist_delta(pre, post, "ipc_direct_phase_ns", "inflight"))
    tot = hstats(hist_delta(pre, post, "ipc_direct_phase_ns", "total"))
    empty = cd(pre, post, "ipc_drain_empty_passes")
    parks = cd(pre, post, "ipc_service_parks")
    wakes = cd(pre, post, "ipc_cqe_wake_writes")
    inline = cd(pre, post, "ipc_direct_inline_reaps")
    svc = int(post.get("ipc_service_threads", 0))
    owners = int(post.get("ipc_session_owners", 0))
    sessions = cd(pre, post, "ipc_binds")  # binds during THIS row = 0; use active
    active = int(post.get("ipc_sessions_active", 0))

    fails = []
    if ops < 0.99 * ios:
        fails.append(f"silent passthrough: ipc_ops_read {ops} < 0.99×ios {ios}")
    for trip in ("ipc_sessions_poisoned", "ipc_descriptor_rejects",
                 "ipc_direct_reap_stalls", "transport_lease_overlong"):
        d = cd(pre, post, trip)
        if d != 0:
            fails.append(f"tripwire {trip} moved by {d}")
    if ing["n"] < 0.99 * ops:
        fails.append(f"ingress instrument dark: {ing['n']} < 0.99×ops {ops}")

    ops_per_pass = round(ops / dp["n"], 2) if dp["n"] else None
    svc_per_op = round(dp["mean"] / ops_per_pass, 2) if (dp["mean"] and ops_per_pass) else None

    row = {
        "leg": leg, "qd": int(qd), "iops": round(iops),
        "clat_us": {"mean": round(clat_mean, 1), "p50": round(p50, 1), "p99": round(p99, 1)},
        "ops": ops, "dd_serves": dd,
        "ingress_us": ing, "inflight_us": infl, "total_us": tot,
        "drain_pass_us": dp, "ops_per_pass": ops_per_pass,
        "svc_us_per_op": svc_per_op,
        "empty_passes": empty, "parks": parks,
        "wakes_per_op": round(wakes / max(ops, 1), 4),
        "inline_reaps": inline,
        "svc_threads": svc, "session_owners": owners,
        "sessions_active": active, "binds_during_row": sessions,
    }
    json.dump(row, open(f"{out}/leg{leg}-qd{qd}-row.json", "w"), indent=1)
    print(json.dumps(row))
    if fails:
        for f in fails:
            print(f"ENGAGEMENT FAIL: {f}", file=sys.stderr)
        sys.exit(1)


def table(out, specs):
    print(f"{'leg':>4} {'qd':>3} {'IOPS':>9} {'clat mean':>10} {'p50':>7} "
          f"{'p99':>8} {'ingr mean':>10} {'ingr p99':>9} {'ops/pass':>9} "
          f"{'svc us/op':>10} {'pass mean':>10} {'sess':>5} {'own':>4}")
    for spec in specs:
        leg = spec.split("/")[0]
        for qd in (8, 32):
            try:
                r = json.load(open(f"{out}/leg{leg}-qd{qd}-row.json"))
            except FileNotFoundError:
                continue
            print(f"{r['leg']:>4} {r['qd']:>3} {r['iops']:>9} "
                  f"{r['clat_us']['mean']:>10} {r['clat_us']['p50']:>7} "
                  f"{r['clat_us']['p99']:>8} {r['ingress_us']['mean']:>10} "
                  f"{r['ingress_us']['p99']:>9} {str(r['ops_per_pass']):>9} "
                  f"{str(r['svc_us_per_op']):>10} {str(r['drain_pass_us']['mean']):>10} "
                  f"{r['sessions_active']:>5} {r['session_owners']:>4}")


if __name__ == "__main__":
    if sys.argv[2] == "table":
        table(sys.argv[1], sys.argv[3:])
    else:
        analyze(sys.argv[1], sys.argv[2], sys.argv[3])
