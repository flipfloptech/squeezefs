#!/usr/bin/env python3
"""4-byte O_APPEND writes (the `append-4b` row's instrument; fstests generic/069's shape).

    appends.py <workdir> <seconds> <writers>

Each writer opens its own file `O_CREAT|O_WRONLY|O_APPEND` and writes the
little-endian integers 0, 1, 2, … four bytes at a time for <seconds>; then
every file is read back whole and checked: exactly the integers in order
(`append_reader`'s verdict). Reports Σ ops, Σ bytes (= `user_write_bytes`),
ops/s, the per-writer counts, `bs` 4 and `direct` 0 for the harness's
ledgers, and `instrument_errors` for a readback that did not match.
"""
import json
import os
import struct
import sys
import threading
import time

work, seconds, writers = sys.argv[1], float(sys.argv[2]), int(sys.argv[3])
os.makedirs(work, exist_ok=True)
counts = [0] * writers
stop = time.monotonic() + seconds


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
ts = [threading.Thread(target=run, args=(w,)) for w in range(writers)]
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
    if len(data) != 4 * counts[w]:
        errors += 1
        error_text = "writer %d: %d bytes, expected %d" % (w, len(data), 4 * counts[w])
        continue
    for i in range(counts[w]):
        if struct.unpack_from("<I", data, 4 * i)[0] != i:
            errors += 1
            error_text = "writer %d: integer %d out of order" % (w, i)
            break
ops = sum(counts)
json.dump({
    "instrument": "appends.py", "writers": writers, "ops": ops, "bytes": 4 * ops,
    "user_write_bytes": 4 * ops, "direction": "write", "bs": 4, "direct": 0,
    "elapsed_s": elapsed, "ops_per_s": ops / elapsed if elapsed else 0.0,
    "per_writer": counts, "instrument_errors": errors, "instrument_error_text": error_text,
}, sys.stdout)
sys.stdout.write("\n")
