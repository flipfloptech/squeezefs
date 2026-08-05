#!/usr/bin/env python3
"""tests/phase_delta.py — per-row phase-histogram delta analyzer.

Takes two stats-inode metric snapshots (the fleet_width_bracket.sh
pre/post files) and prints, for every ALWAYS-ON phase family
(write_pipeline_phase_ns, publish_phase_ns, meta_txpass_phase_ns,
write_transport_phase_ns, read_serve_phase_ns if it moved), the per-phase
bucket-count delta, an estimated total residence (bucket-midpoint
weighted) and the estimated mean — the instrument the 2026-07-31
write-wall / 2026-08-01 publish-drain campaigns built, composed per arm.

Usage: phase_delta.py <pre.json> <post.json> [family ...]
"""
import json
import sys

FAMILIES = [
    "write_pipeline_phase_ns",
    "publish_phase_ns",
    "meta_txpass_phase_ns",
    "write_transport_phase_ns",
]

# Bucket label -> midpoint estimate in microseconds. Buckets are powers
# of two ("<=2^k us"): (2^(k-1), 2^k] midpoint = 0.75 * 2^k; the first
# bucket is 0.5 us and the last open bucket 24 s.
def midpoints():
    labels = ["<=1us", "<=2us", "<=4us", "<=8us", "<=16us", "<=32us",
              "<=64us", "<=128us", "<=256us", "<=512us", "<=1024us",
              "<=2ms", "<=4ms", "<=8ms", "<=16ms", "<=32ms", "<=64ms",
              "<=128ms", "<=256ms", "<=512ms", "<=1024ms", "<=2s",
              "<=4s", "<=8s", "<=16s", ">16s"]
    out = {}
    for i, lab in enumerate(labels):
        if i == 0:
            out[lab] = 0.5
        elif lab == ">16s":
            out[lab] = 24e6
        else:
            out[lab] = 0.75 * (2 ** i)
    return out


MID = midpoints()


def load(path):
    d = json.load(open(path))
    return d.get("metrics", d)


def fmt_us(us):
    if us >= 1e6:
        return f"{us / 1e6:.2f}s"
    if us >= 1e3:
        return f"{us / 1e3:.1f}ms"
    return f"{us:.1f}us"


def main():
    pre, post = load(sys.argv[1]), load(sys.argv[2])
    fams = sys.argv[3:] or FAMILIES
    for fam in fams:
        a, b = pre.get(fam), post.get(fam)
        if not isinstance(a, dict) or not isinstance(b, dict):
            continue
        rows = []
        for phase in b:
            pb, pa = a.get(phase, {}), b[phase]
            dcount = 0
            dtot = 0.0
            hi = ""
            for lab, v in pa.items():
                d = v - pb.get(lab, 0)
                if d:
                    dcount += d
                    dtot += d * MID.get(lab, 0.0)
                    hi = lab  # labels iterate in insertion order (ascending)
            if dcount:
                rows.append((phase, dcount, dtot, dtot / dcount, hi))
        if not rows:
            continue
        print(f"== {fam} ==")
        print(f"  {'phase':<18} {'n':>9} {'est total':>12} {'est mean':>10}  max-bucket")
        for phase, n, tot, mean, hi in rows:
            print(f"  {phase:<18} {n:>9} {fmt_us(tot):>12} {fmt_us(mean):>10}  {hi}")
    # Scalar ledger of interest for the durable decomposition.
    keys = [k for k in post if isinstance(post[k], (int, float))
            and isinstance(pre.get(k, 0), (int, float))
            and post[k] - pre.get(k, 0) != 0]
    interesting = [k for k in keys if any(s in k for s in (
        "write_through", "durable_upload", "flush_seed", "staging_put",
        "write_pipeline", "layout_", "publish_", "meta_kv_journal",
        "meta_commit_group", "data_device_sync", "block_free",
        "extent_", "fold_", "patch_", "placed_", "ipc_", "nt_copy",
        "writeback", "overwrite_seed", "active_block_ooo",
        "meta_conveyor", "fuse_flush", "fuse_release", "lease_"))]
    if interesting:
        print("== scalar deltas ==")
        for k in sorted(interesting):
            print(f"  {k} = {post[k] - pre.get(k, 0)}")


if __name__ == "__main__":
    main()
