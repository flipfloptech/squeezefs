#!/usr/bin/env python3
"""Analyzer for 2026-09-05-write-lever-abba-local.sh rows: per leg × row,
fio GiB/s / clat mean / p99 / p99.9 beside the `.stats` deltas of the gauge
names given on the command line (nested keys are found by name anywhere in
the stats tree; a histogram-valued gauge reports count and mean µs).

    python3 2026-09-05-write-lever-abba-analyze.py target/w2-abba \
        write_lock_wait_exclusive write_lock_wait_shared write_lock_scope_entire
"""
import json
import os
import sys


def find(obj, key):
    if isinstance(obj, dict):
        if key in obj:
            return obj[key]
        for v in obj.values():
            r = find(v, key)
            if r is not None:
                return r
    return None


def delta(pre, post, key):
    a, b = find(pre, key), find(post, key)
    if isinstance(b, dict) and "count" in b:
        n = b.get("count", 0) - (a.get("count", 0) if isinstance(a, dict) else 0)
        s = b.get("sum_ns", 0) - (a.get("sum_ns", 0) if isinstance(a, dict) else 0)
        return f"{n} × {s / n / 1e3:.1f}µs" if n else "0"
    if isinstance(b, (int, float)):
        return f"{b - (a if isinstance(a, (int, float)) else 0):.0f}"
    return "—"


def main(out, gauges):
    for row in ("w_fresh", "w_rewrite"):
        print(f"== {row}")
        print(f"{'leg':<4} {'GiB/s':>6} {'clat ms':>8} {'p99 ms':>7} {'p99.9 ms':>9}  " + "  ".join(f"{g}" for g in gauges))
        for leg in ("A1", "B1", "B2", "A2"):
            f = f"{out}/{leg}.{row}.json"
            if not os.path.exists(f):
                continue
            j = json.load(open(f))["jobs"][0]["write"]
            p = j["clat_ns"]["percentile"]
            pre, post = json.load(open(f"{out}/{leg}.{row}.pre.json")), json.load(open(f"{out}/{leg}.{row}.post.json"))
            cols = "  ".join(delta(pre, post, g) for g in gauges)
            print(f"{leg:<4} {j['bw_bytes'] / 2**30:>6.2f} {j['clat_ns']['mean'] / 1e6:>8.1f} "
                  f"{p.get('99.000000', 0) / 1e6:>7.0f} {p.get('99.900000', 0) / 1e6:>9.0f}  {cols}")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2:])
