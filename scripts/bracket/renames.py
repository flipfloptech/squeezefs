#!/usr/bin/env python3
"""rename/unlink journal entries per op (the `rename-unlink` row's instrument).

    renames.py <mnt> <workdir> <seconds> <pool> <threads>

Rounds until the TIMED spans sum to <seconds>: create <pool> files across
<threads> directories (untimed), snapshot `.stats`, then — timed — every
thread renames each of its files (f → g) and unlinks each g, snapshot again.
Reports Σ ops, Σ Δ`meta_kv_journal_entries` over the timed spans, entries/op,
ops/s, and the PAD entries beside. The untimed creates age the store.
"""
import json
import os
import sys
import threading
import time

mnt, work, seconds, pool, threads = sys.argv[1], sys.argv[2], float(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import brk  # noqa: E402


def stats_terms():
    with open(os.path.join(mnt, ".stats")) as f:
        return brk.terms(json.load(f))


per_thread = max(1, pool // threads)
timed = 0.0
ops = 0
entries = 0
pads = 0
fuse_ops = 0
rounds = 0
os.makedirs(work, exist_ok=True)
while timed < seconds:
    rounds += 1
    dirs = [os.path.join(work, "r%d-t%d" % (rounds, t)) for t in range(threads)]
    for d in dirs:
        os.mkdir(d)

    def create(d):
        for i in range(per_thread):
            fd = os.open(os.path.join(d, "f%d" % i), os.O_CREAT | os.O_WRONLY, 0o644)
            os.close(fd)

    ts = [threading.Thread(target=create, args=(d,)) for d in dirs]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    # Let the conveyor land the creates before the snapshot (bounded).
    prev = None
    for _ in range(50):
        cur = stats_terms()["meta_kv_journal_entries"]
        if cur == prev:
            break
        prev = cur
        time.sleep(0.1)
    s0 = stats_terms()
    t0 = time.monotonic()

    def churn(d):
        for i in range(per_thread):
            os.rename(os.path.join(d, "f%d" % i), os.path.join(d, "g%d" % i))
        for i in range(per_thread):
            os.unlink(os.path.join(d, "g%d" % i))

    ts = [threading.Thread(target=churn, args=(d,)) for d in dirs]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    t1 = time.monotonic()
    prev = None
    for _ in range(50):
        cur = stats_terms()["meta_kv_journal_entries"]
        if cur == prev:
            break
        prev = cur
        time.sleep(0.1)
    s1 = stats_terms()
    timed += t1 - t0
    ops += 2 * per_thread * threads
    entries += s1["meta_kv_journal_entries"] - s0["meta_kv_journal_entries"]
    pads += s1["meta_kv_journal_pad_entries"] - s0["meta_kv_journal_pad_entries"]
    fuse_ops += s1["fuse_ops"] - s0["fuse_ops"]
    for d in dirs:
        os.rmdir(d)

json.dump({
    "instrument": "renames.py", "rounds": rounds, "pool": pool, "threads": threads,
    "ops": ops, "renames": ops // 2, "unlinks": ops // 2, "bytes": 0, "elapsed_s": timed,
    "journal_entries": entries, "journal_pad_entries": pads, "fuse_ops": fuse_ops,
    "entries_per_op": (entries / ops) if ops else 0.0, "ops_per_s": (ops / timed) if timed else 0.0,
    "notes": "entries/op over the timed rename+unlink spans only; the writer heartbeat (10 s) and checkpoints add a few entries per minute (bias < 0.001/op at this rate)",
}, sys.stdout)
sys.stdout.write("\n")
