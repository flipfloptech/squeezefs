#!/usr/bin/env python3
"""Write `total_mib` MiB as `chunk_kib` KiB buffered writes starting at
`skew_pages` × 4 KiB (a misaligned writeback grid), then fsync. Usage:
misaligned.py <path> <total_mib> <chunk_kib> <skew_pages> [direct]"""
import os, sys, time
path, total_mib, chunk_kib, skew = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
direct = len(sys.argv) > 5 and sys.argv[5] == "direct"
flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC | (os.O_DIRECT if direct else 0)
fd = os.open(path, flags, 0o644)
chunk = chunk_kib * 1024
buf = bytearray(chunk)
if direct:
    import mmap
    m = mmap.mmap(-1, chunk)
    buf = m
off = skew * 4096
t0 = time.time()
remaining = total_mib * 1024 * 1024
while remaining > 0:
    n = min(chunk, remaining)
    w = os.pwrite(fd, buf[:n] if not direct else buf, off)
    off += w
    remaining -= w
t1 = time.time()
os.fsync(fd)
t2 = time.time()
os.close(fd)
print(f"write {t1-t0:.2f}s fsync {t2-t1:.2f}s total {t2-t0:.2f}s")
