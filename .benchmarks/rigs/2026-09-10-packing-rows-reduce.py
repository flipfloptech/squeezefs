#!/usr/bin/env python3
"""Reduce a packing-rows run (2026-09-10-packing-rows-local.sh) to markdown.

    python3 .benchmarks/rigs/2026-09-10-packing-rows-reduce.py <OUT> [<OUT2> ...]

Several OUT dirs (A-B-B-A positions on the box) are merged: every row is
reported per position, then the per-arm medians. Space rows are
deterministic (a slot is a slot on any host); the throughput/latency rows
are the venue's and are labeled by the driver.log's hostname line.
"""
import json
import statistics
import sys
from pathlib import Path


def load(out: Path):
    rows = [json.loads(l) for l in (out / "rows.jsonl").read_text().splitlines() if l.strip()]
    host = ""
    for l in (out / "driver.log").read_text().splitlines():
        if l.startswith("[") and "== packing rows (" in l:
            host = l.split("== packing rows (", 1)[1].split(")", 1)[0]
            break
    return host, rows


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else float("nan")


def main(outs):
    runs = [(Path(o), *load(Path(o))) for o in outs]
    print(f"# packing rows — {len(runs)} run(s)")
    for p, host, _ in runs:
        print(f"- `{p}` — {host}")
    print()

    print("## dismount row (FILES small files, no fsync → `squeezefs umount` promotes)")
    print()
    print("| pos | arm | files | umount wall s | blocks after | drift | mismatches | fsck | daemon summary |")
    print("|---|---|---|---|---|---|---|---|---|")
    for i, (_, _, rows) in enumerate(runs, 1):
        for r in rows:
            if r["row"] != "dismount":
                continue
            print(f"| {i} | {r['arm']} | {r['files']} | {r['umount_wall_s']:.2f} | {r['blocks_after']} | {r['drift']} | "
                  f"{r['mismatches']} | {r['fsck_findings']} | {r['summary']} |")
    print()

    print("## compaction row (`defrag --pack` on the dismount population; A = the LEGACY one-block-per-file volume, B = half the tenants deleted)")
    print()
    print("| pos | arm | blocks before → after | freed | tenants moved | mismatches | fsck | drift after | report pack |")
    print("|---|---|---|---|---|---|---|---|---|")
    for i, (_, _, rows) in enumerate(runs, 1):
        for r in rows:
            if r["row"] != "compact":
                continue
            rep = dict(r.get("report_pack") or {})
            rep.pop("rows", None)  # the per-block table stays in <OUT>/compact-<arm>.report.json
            print(f"| {i} | {r['arm']} | {r['blocks_before']} → {r['blocks_after']} | {r['freed']} | {r['moved']} | "
                  f"{r['mismatches']} | {r['fsck_findings']} | {r['drift_after']} | `{json.dumps(rep)}` |")
    print()

    print("## fsync-promotion row (SQUEEZEFS_FSYNC_PROMOTE_STAGED=1 both arms; fio create + fsync-on-close, 16 KiB)")
    print()
    print("| pos | arm | files | files/s | clat p50 µs | p99.9 µs | fsync µs (staged_promote) | promoted (packed) | BLOCKS | dev/user bytes | wareq-sz KiB | cpu µs/file | tripwires |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    per_arm = {}
    for i, (_, _, rows) in enumerate(runs, 1):
        for r in rows:
            if r["row"] != "fsyncrow":
                continue
            per_arm.setdefault(r["arm"], []).append(r)
            print(f"| {i} | {r['arm']} | {r['files']} | {r['files_per_s']:,.0f} | {r['clat_p50_us']:.0f} | {r['clat_p999_us']:.0f} | "
                  f"{r['fsync_total_us']:.0f} ({r['fsync_staged_promote_us']:.0f}) | {r['promoted']} ({r['promoted_packed']}) | "
                  f"{r['blocks']} | {r['amp']:.2f}× | {r['wareq_kib']:.0f} | {r['daemon_cpu_us_per_file']:.0f} | {r['tripwires']} |")
    print()
    if all(k in per_arm for k in ("A", "B")):
        a, b = per_arm["A"], per_arm["B"]
        def m(rs, k): return med([r[k] for r in rs])
        print("### per-arm medians")
        print()
        print("| metric | A (one block per file) | B (packing) | B/A |")
        print("|---|---|---|---|")
        for k, label in [("files_per_s", "files/s"), ("clat_p50_us", "clat p50 µs"), ("clat_p999_us", "clat p99.9 µs"),
                         ("fsync_total_us", "fsync total µs"), ("blocks", "blocks minted"), ("amp", "device/user bytes"),
                         ("daemon_cpu_us_per_file", "daemon CPU µs/file")]:
            ma, mb = m(a, k), m(b, k)
            ratio = mb / ma if ma else float("nan")
            print(f"| {label} | {ma:,.2f} | {mb:,.2f} | {ratio:.3f} |")
        print()
    drift = [r for _, _, rows in runs for r in rows if r["row"] == "fsyncrow-verify"]
    if drift:
        print("fsync-row remount oracle drift: " + ", ".join(f"{r['arm']}={r['drift']}" for r in drift))


if __name__ == "__main__":
    main(sys.argv[1:] or ["target/packing-rows"])
