#!/usr/bin/env python3
"""Table for the inline-raise threshold sweep (2026-09-09-inline-raise-sweep-local.sh).

    2026-09-09-inline-raise-sweep-reduce.py <OUT>

One row per (ceiling T, file size S, row kind): files/s (fio's own group
iops), clat p50 / p99.9, the layout mix the row took (inline vs staged
writes), metadata-plane bytes per file (journal / node appends — exact
deltas over the row), node-cache misses, the fsync decomposition (exact
means), daemon CPU per file, and the staged residue left on the mount.
The cells where T >= S are the INLINE posture for that size, T < S the
STAGED one; the shipped posture is T = 4096.
"""
import glob
import json
import os
import re
import sys


def pm(a, b, fam, ph):
    f0, f1 = a.get(fam) or {}, b.get(fam) or {}
    c = (f1.get(ph) or {}).get("count", 0) - (f0.get(ph) or {}).get("count", 0)
    s = (f1.get(ph) or {}).get("sum_ns", 0) - (f0.get(ph) or {}).get("sum_ns", 0)
    return s / c / 1e3 if c else None


def row(out, tag):
    a = json.load(open(f"{out}/{tag}.stats0"))
    b = json.load(open(f"{out}/{tag}.stats1"))
    am, bm = a["metrics"], b["metrics"]
    fio = json.load(open(f"{out}/{tag}.fio.json"))["jobs"][0]
    w = fio["write"]

    def d(k):
        return (bm.get(k) or 0) - (am.get(k) or 0)

    files = w["total_ios"] or 1
    return {
        "files_s": w["iops"],
        "p50": w["clat_ns"]["percentile"]["50.000000"] / 1e3,
        "p999": w["clat_ns"]["percentile"]["99.900000"] / 1e3,
        "inline": d("layout_inline_writes"),
        "staged": d("layout_staged_writes"),
        "jbytes": d("meta_kv_journal_bytes") / files,
        "abytes": d("meta_kv_node_append_bytes") / files,
        "misses": d("meta_kv_node_cache_misses"),
        "fs_total": pm(am, bm, "fsync_phase_ns", "total"),
        "fs_meta_barrier": pm(am, bm, "fsync_phase_ns", "meta_barrier"),
        "fs_data_flush": pm(am, bm, "fsync_phase_ns", "data_flush"),
        "cpu": d("daemon_cpu_ns") / files / 1e3,
        "resident": b.get("nvme_staged_write_file_count"),
        "trip": sum(d(k) or 0 for k in ("invariant_tripwires", "fuse_op_watchdog_overdue",
                                         "transport_slots_overdue", "writeback_errors_latched")),
    }


COLS = [("files_s", "files/s", "{:,.0f}"), ("p50", "p50 µs", "{:.0f}"), ("p999", "p99.9 µs", "{:.0f}"),
        ("inline", "inline writes", "{:,.0f}"), ("staged", "staged writes", "{:,.0f}"),
        ("jbytes", "journal B/file", "{:,.0f}"), ("abytes", "node-append B/file", "{:,.0f}"),
        ("misses", "node misses", "{:,.0f}"),
        ("fs_total", "fsync µs", "{:.0f}"), ("fs_meta_barrier", "meta_barrier µs", "{:.0f}"),
        ("fs_data_flush", "data_flush µs", "{:.0f}"),
        ("cpu", "daemon µs/file", "{:.0f}"), ("resident", "staged resident", "{}"), ("trip", "tripwires", "{:.0f}")]


def fmt(v, f):
    return "—" if v is None else f.format(v)


def main():
    out = sys.argv[1]
    rows = {}
    for jf in sorted(glob.glob(f"{out}/*.fio.json")):
        tag = os.path.basename(jf)[: -len(".fio.json")]
        m = re.match(r"^T(\d+)-S(\w+)-(fsync|create)$", tag)
        if not m or not os.path.exists(f"{out}/{tag}.stats1"):
            continue
        rows[(m.group(3), int(m.group(1)), m.group(2))] = row(out, tag)
    for kind in ("fsync", "create"):
        print(f"\n== {kind} rows (24 jobs, one {kind}{' per file' if kind == 'fsync' else ''}, 20 s)")
        print("| ceiling T | size S | posture | " + " | ".join(h for _, h, _ in COLS) + " |")
        print("|" + "---|" * (len(COLS) + 3))
        for (k, t, s), r in sorted(rows.items(), key=lambda x: (x[0][0], int(re.sub(r"\D", "", x[0][2])), x[0][1])):
            if k != kind:
                continue
            size = int(re.sub(r"\D", "", s)) * 1024
            posture = "INLINE" if t >= size else "staged"
            if t == 4096:
                posture += " (shipped)"
            print(f"| {t} | {s} | {posture} | " + " | ".join(fmt(r.get(c), f) for c, _, f in COLS) + " |")


if __name__ == "__main__":
    main()
