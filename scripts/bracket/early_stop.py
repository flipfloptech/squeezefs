#!/usr/bin/env python3
"""The early-stop sequential reader (decision 0003 §K row 5): N workers cycle
over a file set, each open reads the first HEAD bytes of a file sequentially
in BS O_DIRECT reads and closes — the `head`/thumbnail/sampler shape whose
device cost is the prefetch pipeline's overshoot past the stop. Prints fio-
shaped JSON (`brk.fio_metrics` reads it) so the row folds like any other.

    early_stop.py --dir D --prefix P --files N --head BYTES --bs BYTES
                  --workers W --runtime S --output OUT.json
"""
import argparse
import json
import mmap
import os
import threading
import time

p = argparse.ArgumentParser()
p.add_argument("--dir", required=True)
p.add_argument("--prefix", required=True)
p.add_argument("--files", type=int, required=True)
p.add_argument("--head", type=int, required=True)
p.add_argument("--bs", type=int, required=True)
p.add_argument("--workers", type=int, required=True)
p.add_argument("--runtime", type=float, required=True)
p.add_argument("--output", required=True)
a = p.parse_args()

O_DIRECT = os.O_DIRECT  # the arch-correct flag (0o200000 on aarch64, 0o40000 on x86_64)
deadline = time.monotonic() + a.runtime
lock = threading.Lock()
totals = {"ops": 0, "bytes": 0, "files": 0, "errors": 0, "lat_ns": []}


def worker(idx):
    buf = mmap.mmap(-1, a.bs)  # page-aligned: O_DIRECT's grain
    ops = 0
    nbytes = 0
    files = 0
    errors = 0
    lat = []
    i = idx
    while time.monotonic() < deadline:
        path = os.path.join(a.dir, "%s.%d" % (a.prefix, i % a.files))
        i += a.workers
        try:
            fd = os.open(path, os.O_RDONLY | O_DIRECT)
        except OSError:
            errors += 1
            continue
        try:
            off = 0
            while off < a.head and time.monotonic() < deadline:
                t0 = time.monotonic_ns()
                n = os.readv(fd, [buf])
                lat.append(time.monotonic_ns() - t0)
                if n <= 0:
                    break
                ops += 1
                nbytes += n
                off += n
            files += 1
        except OSError:
            errors += 1
        finally:
            os.close(fd)
    with lock:
        totals["ops"] += ops
        totals["bytes"] += nbytes
        totals["files"] += files
        totals["errors"] += errors
        totals["lat_ns"].extend(lat)


t_start = time.monotonic()
threads = [threading.Thread(target=worker, args=(i,)) for i in range(a.workers)]
for t in threads:
    t.start()
for t in threads:
    t.join()
elapsed = time.monotonic() - t_start
lat = sorted(totals["lat_ns"]) or [0]


def pct(q):
    return lat[min(len(lat) - 1, int(q * (len(lat) - 1)))]


runtime_ms = int(elapsed * 1000)
doc = {
    "fio version": "early-stop.py",
    "jobs": [{
        "jobname": "early-stop", "error": 1 if totals["errors"] else 0,
        "read": {
            "total_ios": totals["ops"], "io_bytes": totals["bytes"], "runtime": runtime_ms,
            "bw_bytes": int(totals["bytes"] / elapsed) if elapsed else 0,
            "iops": totals["ops"] / elapsed if elapsed else 0,
            "clat_ns": {"mean": sum(lat) / len(lat), "max": lat[-1],
                        "percentile": {"50.000000": pct(0.5), "99.000000": pct(0.99), "99.900000": pct(0.999)}},
        },
        "write": {"total_ios": 0, "io_bytes": 0, "runtime": 0, "bw_bytes": 0, "iops": 0, "clat_ns": {"mean": 0, "max": 0, "percentile": {}}},
        "files_opened": totals["files"],
    }],
}
json.dump(doc, open(a.output, "w"))
