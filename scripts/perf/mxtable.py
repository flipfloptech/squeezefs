#!/usr/bin/env python3
# mxtable.py results.tsv [metric] — matrix.sh's rows across repetitions: per (target, row) the median of
# `metric` (IOPS or MiB/s, default by row: IOPS for the 4 KiB rows, MiB/s for the rest) and its spread
# (max − min) ÷ median, flagged `!` above 10 %. A label's trailing `-rN` names the repetition.
import collections, re, statistics, sys
rows = collections.defaultdict(list); order = []; targets = []
for line in open(sys.argv[1]):
    f = line.rstrip("\n").split("\t")
    if len(f) < 6 or f[0] == "label":
        continue
    t = re.sub(r"-r\d+$", "", f[0])
    if t not in targets: targets.append(t)
    if f[1] not in order: order.append(f[1])
    rows[(t, f[1])].append(f)
forced = sys.argv[2] if len(sys.argv) > 2 else None
print("row\t" + "\t".join(targets))
for r in order:
    col = 2 if (forced or ("IOPS" if re.match(r"r[rw]4k-", r) else "MiB/s")) == "IOPS" else 3
    cells = []
    for t in targets:
        v = [float(f[col]) for f in rows.get((t, r), [])]
        if not v:
            cells.append("-"); continue
        m = statistics.median(v); sp = (max(v) - min(v)) / m if m else 0
        cells.append("%.0f%s (%d%%, n=%d)" % (m, "!" if sp > 0.10 else "", round(100 * sp), len(v)))
    print(r + ("" if col == 2 else " MiB/s") + "\t" + "\t".join(cells))
