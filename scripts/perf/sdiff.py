#!/usr/bin/env python3
# sdiff.py before.json after.json [filter-substr...] — counter deltas between two .stats snapshots;
# histograms print count and sum/count mean.
import json, sys
a = json.load(open(sys.argv[1])); b = json.load(open(sys.argv[2])); flt = sys.argv[3:]
def flat(d, p=""):
    out = {}
    if isinstance(d, dict):
        if "count" in d and "sum_ns" in d:
            out[p] = ("hist", d.get("count", 0), d.get("sum_ns", 0))
            return out
        for k, v in d.items():
            out.update(flat(v, p + "." + k if p else k))
    else:
        out[p] = d
    return out
fa = flat(a); fb = flat(b)
for k in sorted(fb):
    if flt and not any(f in k for f in flt):
        continue
    vb = fb[k]; va = fa.get(k)
    if isinstance(vb, tuple):
        c = vb[1] - (va[1] if isinstance(va, tuple) else 0)
        s = vb[2] - (va[2] if isinstance(va, tuple) else 0)
        if c:
            print("%s count=%d mean_us=%.1f sum_ms=%.1f" % (k, c, s / c / 1e3, s / 1e6))
    elif isinstance(vb, (int, float)) and isinstance(va, (int, float)) and vb != va:
        print(k, vb - va)
