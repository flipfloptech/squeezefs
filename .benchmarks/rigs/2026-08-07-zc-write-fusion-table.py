#!/usr/bin/env python3
"""zc-write-fusion bracket table (2026-08-07).

Usage: 2026-08-07-zc-write-fusion-table.py <outdir> <legA1,legB1,legB2,legA2>

Reads the rig's per-row fio JSON + stats snapshots and prints the A-B-B-A
table: per-leg bw/IOPS/lat, per-row A-vs-B ratios (median of the two legs
per side), daemon CPU per byte, and the engagement columns.
"""
import json
import sys


def load(path):
    with open(path) as f:
        return json.load(f)


def row_stats(out, leg, row):
    f = load(f"{out}/{leg}-{row}-fio.json")
    io = sum(j["write"]["io_bytes"] for j in f["jobs"])
    bw = sum(j["write"]["bw_bytes"] for j in f["jobs"])
    iops = sum(j["write"]["iops"] for j in f["jobs"])
    lat = f["jobs"][0]["write"].get("clat_ns", {}).get("percentile", {})
    p50 = lat.get("50.000000", 0) / 1e6
    p99 = lat.get("99.000000", 0) / 1e6
    b = load(f"{out}/{leg}-{row}-before.json")
    b = b.get("metrics", b)
    a = load(f"{out}/{leg}-{row}-after.json")
    a = a.get("metrics", a)
    d = lambda k: a.get(k, 0) - b.get(k, 0)
    # Daemon CPU ticks across the row (utime+stime, USER_HZ=100).
    dcpu = (
        int(open(f"{out}/{leg}-{row}-dcpu1").read())
        - int(open(f"{out}/{leg}-{row}-dcpu0").read())
    ) / 100.0
    def dsec(p):
        return sum(int(line.split()[9]) for line in open(p)) * 512
    amp = (dsec(f"{out}/{leg}-{row}-disk1") - dsec(f"{out}/{leg}-{row}-disk0")) / max(1, io)
    return {
        "io": io,
        "bw": bw,
        "iops": iops,
        "p50": p50,
        "p99": p99,
        "dcpu": dcpu,
        "amp": amp,
        "fusions": d("fuse3_zc_write_fusions"),
        "directs": d("fuse3_zc_write_directs"),
        "extractions": d("fuse3_zc_write_extractions"),
        "demotions": d("fuse3_zc_write_fusion_demotions"),
    }


def main():
    out = sys.argv[1]
    legs = sys.argv[2].split(",") if len(sys.argv) > 2 else ["F1", "F2", "F3", "F4"]
    a_legs, b_legs = [legs[0], legs[3]], [legs[1], legs[2]]
    rows = ["rand4k", "rand4kow", "seqwr", "dur"]
    print(f"{'row':10} {'leg':4} {'GB/s':>7} {'IOPS':>9} {'p50ms':>7} {'p99ms':>8} "
          f"{'dCPU s':>7} {'amp':>5} {'fusions':>9} {'directs':>9} {'extract':>9} {'dem':>4}")
    stats = {}
    for row in rows:
        for leg in legs:
            s = row_stats(out, leg, row)
            stats[(leg, row)] = s
            print(f"{row:10} {leg:4} {s['bw']/1e9:7.3f} {s['iops']:9.0f} {s['p50']:7.2f} "
                  f"{s['p99']:8.2f} {s['dcpu']:7.1f} {s['amp']:5.2f} {s['fusions']:9d} "
                  f"{s['directs']:9d} {s['extractions']:9d} {s['demotions']:4d}")
        print()
    print("A-vs-B (median of the two legs per side; brackets = both A/B pairings):")
    for row in rows:
        av = sorted(stats[(l, row)]["bw"] for l in a_legs)
        bv = sorted(stats[(l, row)]["bw"] for l in b_legs)
        amed = sum(av) / 2
        bmed = sum(bv) / 2
        # Alternating-order brackets: (A1/B1, A2/B2) — first-vs-first and
        # last-vs-last pairings.
        b1 = stats[(a_legs[0], row)]["bw"] / max(1, stats[(b_legs[0], row)]["bw"])
        b2 = stats[(a_legs[1], row)]["bw"] / max(1, stats[(b_legs[1], row)]["bw"])
        acpu = sum(stats[(l, row)]["dcpu"] / max(1, stats[(l, row)]["io"]) for l in a_legs) / 2
        bcpu = sum(stats[(l, row)]["dcpu"] / max(1, stats[(l, row)]["io"]) for l in b_legs) / 2
        print(f"  {row:10} A={amed/1e9:.3f} GB/s  B={bmed/1e9:.3f} GB/s  ratio={amed/max(1,bmed):.3f}x "
              f"(brackets {b1:.3f}x / {b2:.3f}x)  dCPU/GB A={acpu*1e9:.2f}s B={bcpu*1e9:.2f}s")


if __name__ == "__main__":
    main()
