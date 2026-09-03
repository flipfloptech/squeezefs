#!/usr/bin/env python3
"""tests/op_trace_stitch.py — the per-op trace STITCH (e2e audit A2).

Reads a `.trace` dump (`cat <mnt>/.trace > trace.json` — the read DRAINS
the daemon's rings) and emits, per op id, the stage timeline with its
deltas; per stage transition and per named phase span, the n / p50 /
p99 / mean table; and — given the row's pre/post `.stats` snapshots —
the CONTAINMENT check against the A1 histograms' exact `sum_ns`/`count`
words: the trace's per-op mean for a phase must agree with the
histogram's exact mean over the same row (the trace is a 1-in-`divisor`
sample of the same population), and `n × divisor` with the histogram's
count delta. This replaces the subtraction laws the rigs carried
(`fio clat − transport_total = kernel residue`,
`clat − ipc_direct total = client + ingress`): every number here is a
join on ONE op, never a difference of two independent histograms.

With `--tp <csv>` (the kernel fuse tracepoints, CLOCK_MONOTONIC ns —
the recipe in `crates/fuse3/src/raw/op_trace.rs`):

    bpftrace -e '
      tracepoint:fuse:fuse_request_send { printf("send,%llu,%llu\\n", args->unique, nsecs); }
      tracepoint:fuse:fuse_request_end  { printf("end,%llu,%llu\\n",  args->unique, nsecs); }' \\
      > fuse_tp.csv

the stitch joins each op's kernel `send`/`end` to its daemon chain and
reports the kernel-side residues per op (`send → transport_recv`,
`reply_commit → end`) as MEASURED spans.

Usage:
  op_trace_stitch.py trace.json [--stats-pre pre.json --stats-post post.json]
                     [--tp fuse_tp.csv] [--ops N] [--tolerance 0.25] [--json out.json]

Exit status: 0 = stitched (containment within tolerance where checkable);
1 = no samples or a containment failure.
"""
import argparse
import json
import statistics
import sys

# Named phase spans as (start_stage, end_stage, exact): the phase
# families' histograms whose span has a start stamp in the chain. A
# stage is the END boundary of the phase it sits beside (the Rust
# `Stage` doc), so pairs read straight off the phase tables. `exact` =
# the histogram's own t0 IS the start stamp's instant (the containment
# bound applies); otherwise the start is the nearest earlier stamp and
# the span is reported as an approximation (`~`), never a verdict —
# e.g. `dispatch_lag`'s t0 is read a few hundred ns after the `dispatch`
# stamp, `prelude`'s at the read handler's entry after `handler_entry`.
# (family, phase) -> (start, end, exact)
PHASE_SPANS = {
    ("read_transport_phase_ns", "queue_wait"): ("transport_recv", "dispatch", True),
    ("read_transport_phase_ns", "dispatch_lag"): ("dispatch", "handler_entry", False),
    ("read_transport_phase_ns", "reply_commit"): ("handler_return", "reply_commit", True),
    ("read_transport_phase_ns", "transport_total"): ("transport_recv", "reply_commit", True),
    ("write_transport_phase_ns", "queue_wait"): ("transport_recv", "dispatch", True),
    ("write_transport_phase_ns", "dispatch_lag"): ("dispatch", "handler_entry", False),
    ("write_transport_phase_ns", "reply_commit"): ("handler_return", "reply_commit", True),
    ("write_transport_phase_ns", "transport_total"): ("transport_recv", "reply_commit", True),
    ("read_serve_phase_ns", "prelude"): ("handler_entry", "read_routed", False),
    ("read_serve_phase_ns", "meta_resolve"): ("read_routed", "meta_resolved", False),
    ("read_serve_phase_ns", "key_resolve"): ("meta_resolved", "keys_resolved", False),
    ("read_serve_phase_ns", "classify_probe"): ("keys_resolved", "tier_probed", False),
    ("read_serve_phase_ns", "total"): ("handler_entry", "read_return", False),
    ("read_fill_phase_ns", "dev_service"): ("dev_submit", "dev_complete", True),
    ("write_pipeline_phase_ns", "dev_service"): ("dev_submit", "dev_complete", True),
    ("write_pipeline_phase_ns", "detach_lag"): ("write_admitted", "write_detached", False),
    ("meta_txpass_phase_ns", "tx_queue_wait"): ("meta_enqueue", "pass_begin", True),
    ("meta_txpass_phase_ns", "pass_total"): ("pass_begin", "pass_end", True),
    ("publish_phase_ns", "queue_wait"): ("publish_enqueue", "publish_drained", True),
    ("publish_phase_ns", "total"): ("publish_enqueue", "publish_done", True),
    ("ipc_direct_phase_ns", "admit"): ("ipc_dequeue", "ipc_admitted", True),
    ("ipc_direct_phase_ns", "sq_wait"): ("ipc_admitted", "ipc_sq_enter", True),
    ("ipc_direct_phase_ns", "device_cq"): ("ipc_sq_enter", "ipc_cqe", True),
    ("ipc_direct_phase_ns", "inflight"): ("ipc_admitted", "ipc_cqe", True),
    ("ipc_direct_phase_ns", "finish"): ("ipc_cqe", "ipc_complete", True),
    ("ipc_direct_phase_ns", "total"): ("ipc_dequeue", "ipc_complete", True),
    ("lock_phase_ns", "dlm_guard_hold"): ("dlm_guard_acquired", "dlm_guard_released", True),
}

# Which op class a family's histogram counts (so the count check divides
# the right population): "read" = ops whose chain has a read stage,
# "write" = a write-pipeline/write transport stage, None = any op.
FAMILY_CLASS = {
    "read_transport_phase_ns": "read",
    "read_serve_phase_ns": "read",
    "read_fill_phase_ns": "read",
    "write_transport_phase_ns": "write",
    "write_pipeline_phase_ns": "write",
}

# `fast_dispatch` (R-2): a READ served inline on the reaping queue worker
# stamps it AT the arrival instant and never stamps `dispatch` /
# `handler_entry` (it paid neither hop). The daemon records exact zeros on
# `queue_wait`/`dispatch_lag` for such an op, so the stitch aliases the
# stamp for both (`expand_fast_dispatch`) — the spans read 0 and the
# containment check keeps dividing the whole READ population.
READ_MARKERS = {"read_routed", "read_return", "meta_resolved", "fast_dispatch"}
WRITE_MARKERS = {"write_admitted", "write_done", "write_dma_done"}
FAST_DISPATCH_ALIASES = ("dispatch", "handler_entry")


def expand_fast_dispatch(chain):
    """Alias a served op's `fast_dispatch` stamp as `dispatch` and
    `handler_entry` (same instant) when the op carries neither."""
    names = {n for _, n in chain}
    if "fast_dispatch" not in names:
        return chain
    at = next(ns for ns, n in chain if n == "fast_dispatch")
    for alias in FAST_DISPATCH_ALIASES:
        if alias not in names:
            chain.append((at, alias))
    chain.sort()
    return chain


def fmt_ns(ns):
    if ns is None:
        return "-"
    ns = float(ns)
    if abs(ns) >= 1e9:
        return f"{ns / 1e9:.3f}s"
    if abs(ns) >= 1e6:
        return f"{ns / 1e6:.3f}ms"
    if abs(ns) >= 1e3:
        return f"{ns / 1e3:.1f}us"
    return f"{ns:.0f}ns"


def pct(vals, q):
    if not vals:
        return None
    s = sorted(vals)
    i = min(len(s) - 1, max(0, int(round(q * (len(s) - 1)))))
    return s[i]


def load_trace(path):
    d = json.load(open(path))
    stages = {int(k): v for k, v in d.get("stages", {}).items()}
    ops = {}
    for op_id, stage, ns in d.get("samples", []):
        ops.setdefault(int(op_id), []).append((int(ns), stages.get(int(stage), f"stage{stage}")))
    for chain in ops.values():
        chain.sort()
        expand_fast_dispatch(chain)
    return d, ops


def load_stats(path):
    d = json.load(open(path))
    return d.get("metrics", d)


def load_tp(path):
    """`kind,unique,ns` lines (kind ∈ send|end); whitespace also accepted."""
    send, end = {}, {}
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            parts = line.replace(",", " ").split()
            if len(parts) < 3:
                continue
            kind, unique, ns = parts[0], int(parts[1]), int(parts[2])
            if kind == "send":
                send.setdefault(unique, ns)
            elif kind == "end":
                end[unique] = ns
    return send, end


def op_class(chain):
    names = {n for _, n in chain}
    if names & READ_MARKERS:
        return "read"
    if names & WRITE_MARKERS:
        return "write"
    return "other"


def first(chain, name):
    for ns, n in chain:
        if n == name:
            return ns
    return None


def last(chain, name):
    out = None
    for ns, n in chain:
        if n == name:
            out = ns
    return out


def span(chain, start, end):
    a, b = first(chain, start), last(chain, end)
    if a is None or b is None or b < a:
        return None
    return b - a


def table(title, rows):
    """rows: list of (label, values_ns)."""
    rows = [(k, v) for k, v in rows if v]
    if not rows:
        return
    print(f"== {title} ==")
    print(f"  {'span':<44} {'n':>7} {'p50':>10} {'p99':>10} {'mean':>10}")
    for label, vals in rows:
        print(
            f"  {label:<44} {len(vals):>7} {fmt_ns(pct(vals, 0.5)):>10} "
            f"{fmt_ns(pct(vals, 0.99)):>10} {fmt_ns(statistics.fmean(vals)):>10}"
        )


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("trace")
    ap.add_argument("--stats-pre")
    ap.add_argument("--stats-post")
    ap.add_argument("--tp", help="fuse tracepoint CSV (send/end, unique, CLOCK_MONOTONIC ns)")
    ap.add_argument("--ops", type=int, default=3, help="per-op timelines to print (0 = none)")
    ap.add_argument("--tolerance", type=float, default=0.25,
                    help="containment: |trace mean / histogram mean − 1| bound")
    ap.add_argument("--min-n", type=int, default=20,
                    help="containment: minimum traced ops per phase before the bound applies")
    ap.add_argument("--json", help="write the stitched tables here")
    args = ap.parse_args()

    dump, ops = load_trace(args.trace)
    divisor = int(dump.get("divisor") or 1)
    print(
        f"trace: armed={dump.get('armed')} divisor={divisor} samples_total={dump.get('samples_total')} "
        f"dropped={dump.get('dropped')} ops={len(ops)} samples={sum(len(c) for c in ops.values())}"
    )
    if not ops:
        print("no samples — arm the ring (SQUEEZEFS_OP_TRACE=1 or `op-trace on`) and run a row first")
        return 1
    if dump.get("dropped"):
        print(f"WARNING: {dump['dropped']} samples dropped — chains may be truncated; "
              f"raise the divisor or drain more often")

    out = {"divisor": divisor, "ops": len(ops)}

    # ---- per-op timelines -------------------------------------------------
    if args.ops:
        print(f"== per-op timelines (first {args.ops} by op id) ==")
        for op_id in sorted(ops)[: args.ops]:
            chain = ops[op_id]
            t0 = chain[0][0]
            kind = "il" if op_id >> 63 else "kernel"
            print(f"  op {op_id} ({kind}, {op_class(chain)}): {len(chain)} stamps")
            prev = None
            for ns, name in chain:
                delta = "" if prev is None else f"  +{fmt_ns(ns - prev)}"
                print(f"    {ns - t0:>12} ns  {name:<24}{delta}")
                prev = ns

    # ---- stage transitions (consecutive stamps) ---------------------------
    trans = {}
    for chain in ops.values():
        for (a_ns, a), (b_ns, b) in zip(chain, chain[1:]):
            trans.setdefault(f"{a} -> {b}", []).append(b_ns - a_ns)
    table("stage transitions (consecutive stamps, per op)",
          sorted(trans.items(), key=lambda kv: -len(kv[1])))
    out["transitions"] = {k: {"n": len(v), "p50": pct(v, 0.5), "p99": pct(v, 0.99),
                              "mean": statistics.fmean(v)} for k, v in trans.items()}

    # ---- named phase spans -------------------------------------------------
    spans = {}
    for (fam, phase), (start, end, _exact) in PHASE_SPANS.items():
        vals = []
        for chain in ops.values():
            s = span(chain, start, end)
            if s is not None:
                vals.append(s)
        if vals:
            spans[(fam, phase)] = vals
    table("phase spans (stage pairs; ~ = start is the nearest earlier stamp)",
          [(f"{'' if PHASE_SPANS[(fam, phase)][2] else '~'}{fam}.{phase} "
            f"[{PHASE_SPANS[(fam, phase)][0]}→{PHASE_SPANS[(fam, phase)][1]}]", v)
           for (fam, phase), v in sorted(spans.items())])
    out["spans"] = {f"{fam}.{phase}": {"n": len(v), "p50": pct(v, 0.5), "p99": pct(v, 0.99),
                                       "mean": statistics.fmean(v)}
                    for (fam, phase), v in spans.items()}

    # ---- containment vs the A1 histograms -----------------------------------
    failures = 0
    if args.stats_pre and args.stats_post:
        pre, post = load_stats(args.stats_pre), load_stats(args.stats_post)
        classes = {}
        for chain in ops.values():
            classes[op_class(chain)] = classes.get(op_class(chain), 0) + 1
        print("== containment vs histograms (trace sample vs exact sum_ns/count) ==")
        print(f"  {'phase':<40} {'trace n':>8} {'n×div':>9} {'hist Δn':>9} "
              f"{'trace mean':>11} {'hist mean':>11} {'ratio':>7}  verdict")
        cont = {}
        for (fam, phase), vals in sorted(spans.items()):
            hp = post.get(fam, {}).get(phase)
            hq = pre.get(fam, {}).get(phase, {})
            if not isinstance(hp, dict) or "sum_ns" not in hp:
                continue
            dn = hp["count"] - hq.get("count", 0)
            dsum = hp["sum_ns"] - hq.get("sum_ns", 0)
            hist_mean = dsum / dn if dn else None
            tmean = statistics.fmean(vals)
            ratio = (tmean / hist_mean) if hist_mean else None
            n_scaled = len(vals) * divisor
            exact = PHASE_SPANS[(fam, phase)][2]
            verdict = "n/a"
            if hist_mean is not None and not exact:
                verdict = "~approx"
            elif hist_mean is not None and len(vals) >= args.min_n:
                ok = abs(ratio - 1.0) <= args.tolerance
                verdict = "OK" if ok else "FAIL"
                if not ok:
                    failures += 1
            elif hist_mean is not None:
                verdict = f"n<{args.min_n}"
            print(f"  {fam + '.' + phase:<40} {len(vals):>8} {n_scaled:>9} {dn:>9} "
                  f"{fmt_ns(tmean):>11} {fmt_ns(hist_mean):>11} "
                  f"{(f'{ratio:.2f}' if ratio is not None else '-'):>7}  {verdict}")
            cont[f"{fam}.{phase}"] = {"trace_n": len(vals), "n_scaled": n_scaled, "hist_n": dn,
                                      "trace_mean": tmean, "hist_mean": hist_mean,
                                      "ratio": ratio, "verdict": verdict}
        out["containment"] = cont
        out["classes"] = classes
        print(f"  traced op classes: {classes}")

    # ---- the kernel join ----------------------------------------------------
    if args.tp:
        send, end = load_tp(args.tp)
        pre_res, post_res, k_total, d_total = [], [], [], []
        matched = 0
        for op_id, chain in ops.items():
            if op_id >> 63:
                continue  # il ops have no kernel unique
            s, e = send.get(op_id), end.get(op_id)
            recv, commit = first(chain, "transport_recv"), last(chain, "reply_commit")
            if s is None or e is None:
                continue
            matched += 1
            k_total.append(e - s)
            if recv is not None:
                pre_res.append(recv - s)
            if commit is not None:
                post_res.append(e - commit)
            if recv is not None and commit is not None:
                d_total.append(commit - recv)
        print(f"== kernel join (fuse:fuse_request_send/end by unique) == matched {matched} of "
              f"{sum(1 for o in ops if not o >> 63)} kernel ops")
        table("kernel-side residues (MEASURED per op, not subtracted)", [
            ("send → transport_recv (kernel queue → daemon arrival)", pre_res),
            ("reply_commit → end (daemon commit → kernel completion)", post_res),
            ("transport_recv → reply_commit (daemon-visible)", d_total),
            ("send → end (kernel-visible total)", k_total),
        ])
        out["kernel_join"] = {"matched": matched,
                              "pre_residue": {"n": len(pre_res), "p50": pct(pre_res, 0.5), "p99": pct(pre_res, 0.99)},
                              "post_residue": {"n": len(post_res), "p50": pct(post_res, 0.5), "p99": pct(post_res, 0.99)},
                              "kernel_total": {"n": len(k_total), "p50": pct(k_total, 0.5), "p99": pct(k_total, 0.99)}}

    if args.json:
        with open(args.json, "w") as f:
            json.dump(out, f, indent=1)

    if failures:
        print(f"CONTAINMENT FAILED on {failures} phase(s) (tolerance {args.tolerance})")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
