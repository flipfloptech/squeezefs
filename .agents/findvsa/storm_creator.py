#!/usr/bin/env python3
"""Acked-create storm with a per-create ack log (FIND-VS-A part 2).

Each worker creates files under <mnt>/ktree/w<idx>/d<j>/ and appends the path
to its own ack log ONLY AFTER close() returned (create+write+close acked by
the filesystem). kill -9 of the daemon mid-storm makes workers fail loudly;
the union of ack logs is the acked set the remount must serve.
"""
import os
import sys
import multiprocessing as mp


def worker(mnt, logdir, idx, dirs, files_per_dir, runid):
    ack = open(os.path.join(logdir, f"acked.{idx}"), "w", buffering=1)
    payload = b"x" * 4096
    for d in range(dirs):
        dpath = f"{mnt}/ktree/w{idx}/d{d}"
        os.makedirs(dpath, exist_ok=True)
        for f in range(files_per_dir):
            # Unique, byte-searchable name (image forensics).
            p = f"{dpath}/f{f:04d}q{runid}w{idx:02d}d{d}z"
            try:
                fd = os.open(p, os.O_CREAT | os.O_WRONLY, 0o644)
                os.write(fd, payload)
                os.close(fd)
            except OSError as e:
                print(f"worker {idx}: {p}: {e}", flush=True)
                return
            ack.write(p + "\n")
    ack.close()


if __name__ == "__main__":
    mnt, logdir = sys.argv[1], sys.argv[2]
    workers = int(sys.argv[3]) if len(sys.argv) > 3 else 16
    dirs = int(sys.argv[4]) if len(sys.argv) > 4 else 8
    fpd = int(sys.argv[5]) if len(sys.argv) > 5 else 1024
    runid = sys.argv[6] if len(sys.argv) > 6 else "r0"
    os.makedirs(f"{mnt}/ktree", exist_ok=True)
    procs = [
        mp.Process(target=worker, args=(mnt, logdir, i, dirs, fpd, runid))
        for i in range(workers)
    ]
    for p in procs:
        p.start()
    for p in procs:
        p.join()
    print("storm done")
