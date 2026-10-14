#!/usr/bin/env python3
"""O_APPEND writes (the `append-4b` and `append-close-1k` rows' instrument).

    appends.py <workdir> <seconds> <writers> [<record bytes>]

Each writer appends to its own file for <seconds>; then every file is read
back whole and checked: exactly the records in order (`append_reader`'s
verdict). Without <record bytes> (fstests generic/069's shape) a writer opens
its file `O_CREAT|O_WRONLY|O_APPEND` once and writes the little-endian
integers 0, 1, 2, … four bytes at a time. With it every record is its own
open-write-close (a shell's `echo … >> log` loop: each close drains the
file's custody — rung-10 dev perf §10.4): record i is the little-endian
integer i repeated to <record bytes>. Reports Σ ops, Σ bytes (=
`user_write_bytes`), ops/s, the per-writer counts, `bs` (the record) and
`direct` 0 for the harness's ledgers, and `instrument_errors` for a readback
that did not match.
"""
import json
import os
import struct
import sys
import threading
import time

work, seconds, writers = sys.argv[1], float(sys.argv[2]), int(sys.argv[3])
record = int(sys.argv[4]) if len(sys.argv) > 4 else 0
bs = record or 4
os.makedirs(work, exist_ok=True)
counts = [0] * writers
stop = time.monotonic() + seconds


def record_bytes(i):
    return (struct.pack("<I", i) * (bs // 4 + 1))[:bs]


def run_close_each(w):
    path = os.path.join(work, "testfile.%d" % w)
    i = 0
    while time.monotonic() < stop:
        for _ in range(16):
            fd = os.open(path, os.O_CREAT | os.O_WRONLY | os.O_APPEND, 0o600)
            try:
                if os.write(fd, record_bytes(i)) != bs:
                    raise OSError("short write")
            finally:
                os.close(fd)
            i += 1
    counts[w] = i


def run(w):
    fd = os.open(os.path.join(work, "testfile.%d" % w), os.O_CREAT | os.O_WRONLY | os.O_APPEND, 0o600)
    i = 0
    try:
        while time.monotonic() < stop:
            # 256 writes between clock reads: the clock is not the instrument.
            for _ in range(256):
                if os.write(fd, struct.pack("<I", i)) != 4:
                    raise OSError("short write")
                i += 1
    finally:
        os.close(fd)
    counts[w] = i


t0 = time.monotonic()
ts = [threading.Thread(target=run_close_each if record else run, args=(w,)) for w in range(writers)]
for t in ts:
    t.start()
for t in ts:
    t.join()
elapsed = time.monotonic() - t0
errors = 0
error_text = ""
for w in range(writers):
    with open(os.path.join(work, "testfile.%d" % w), "rb") as f:
        data = f.read()
    if len(data) != bs * counts[w]:
        errors += 1
        error_text = "writer %d: %d bytes, expected %d" % (w, len(data), bs * counts[w])
        continue
    for i in range(counts[w]):
        good = data[bs * i:bs * (i + 1)] == record_bytes(i) if record else struct.unpack_from("<I", data, 4 * i)[0] == i
        if not good:
            errors += 1
            error_text = "writer %d: record %d out of order" % (w, i)
            break
ops = sum(counts)
json.dump({
    "instrument": "appends.py", "writers": writers, "ops": ops, "bytes": bs * ops,
    "user_write_bytes": bs * ops, "direction": "write", "bs": bs, "direct": 0, "close_each": bool(record),
    "elapsed_s": elapsed, "ops_per_s": ops / elapsed if elapsed else 0.0,
    "per_writer": counts, "instrument_errors": errors, "instrument_error_text": error_text,
}, sys.stdout)
sys.stdout.write("\n")
