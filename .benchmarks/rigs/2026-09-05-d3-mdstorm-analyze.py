#!/usr/bin/env python3
"""D-3 mdstorm A-B-B-A analyzer: per leg, the phase ops/s (from row_<leg>.txt)
beside the stripe-collision census deltas (from the runner's pre/post
`.stats` snapshots) — collisions and key waits per op on every striped
table, plus the 4a wait phase (`lock_phase_ns.dlm_guard_wait`).

    python3 2026-09-05-d3-mdstorm-analyze.py /dev/shm/sqz_mdstorm_d3_A1 ...
"""
import json
import re
import sys


def load(path):
    with open(path) as f:
        return json.load(f)


def find(obj, key):
    if isinstance(obj, dict):
        if key in obj:
            return obj[key]
        for v in obj.values():
            r = find(v, key)
            if r is not None:
                return r
    return None


def num(obj, key):
    v = find(obj, key)
    if isinstance(v, dict):
        v = v.get("count", v.get("value"))
    return float(v) if isinstance(v, (int, float)) else 0.0


TABLES = ["dlm_inode", "dlm_dentry", "serve_ino", "inode_meta", "block_flush", "lease_waiter"]


def main(dirs):
    print(f"{'leg':<4} {'stripes':>8} {'ops':>8} {'rename/s':>9} {'unlink/s':>9} {'create/s':>9} {'manydirs/s':>10} "
          f"{'4a_coll/op':>11} {'4a_key/op':>10} {'serve_coll/op':>13} {'4a_wait_us/op':>13} {'4a_wait_s':>9}")
    for d in dirs:
        leg = d.rstrip("/").split("_")[-1]
        pre, post = load(f"{d}/stats_{leg}_pre.json"), load(f"{d}/stats_{leg}_post.json")
        rows = {}
        with open(f"{d}/row_{leg}.txt") as f:
            for line in f:
                m = re.search(r"^(?:\[\w+\]\s+)?(\w+) ops=(\d+) wall_s=([\d.]+) ops_s=(\d+)", line)
                if m:
                    rows[m.group(1)] = (int(m.group(2)), float(m.group(3)), int(m.group(4)))
        ops = sum(v[0] for v in rows.values()) or 1
        d_ = lambda k: num(post, k) - num(pre, k)  # noqa: E731
        coll4a = d_("dlm_inode_stripe_collisions") + d_("dlm_dentry_stripe_collisions")
        key4a = d_("dlm_inode_key_waits") + d_("dlm_dentry_key_waits")
        serve = d_("serve_ino_stripe_collisions")
        wait = find(post, "dlm_guard_wait") or {}
        wait_pre = find(pre, "dlm_guard_wait") or {}
        sum_ns = (wait.get("sum_ns", 0) if isinstance(wait, dict) else 0) - (
            wait_pre.get("sum_ns", 0) if isinstance(wait_pre, dict) else 0)
        stripes = find(post, "dlm_inode_stripes")
        print(f"{leg:<4} {str(stripes):>8} {ops:>8} {rows.get('rename', (0, 0, 0))[2]:>9} "
              f"{rows.get('unlink', (0, 0, 0))[2]:>9} {rows.get('create', (0, 0, 0))[2]:>9} "
              f"{rows.get('manydirs', (0, 0, 0))[2]:>10} "
              f"{coll4a / ops:>11.5f} {key4a / ops:>10.5f} {serve / ops:>13.5f} {sum_ns / ops / 1e3:>13.2f} "
              f"{sum_ns / 1e9:>9.1f}")
        other = {t: d_(f"{t}_stripe_collisions") for t in TABLES if t not in ("dlm_inode", "dlm_dentry", "serve_ino")}
        if any(other.values()):
            print(f"     other-table collisions: {other}")


if __name__ == "__main__":
    main(sys.argv[1:])
