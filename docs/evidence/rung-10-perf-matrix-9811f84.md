# The performance matrix on `dev` `9811f84` (2026-10-28)

Measurement only, at the owner's request: every mdtest row and every fio row, with and without the
interposer. The workstation (32 threads, 109 GiB, Linux `7.3.0-1.7-mainline-sqz`, on AC, the
`performance` profile), the RAM-backed NVMe namespaces (`scripts/perf/ramnvme.sh`: 2 metadata + 2 data,
`dir spread depth 3`), the perf mount (dist build `9811f84`, `--supervise`), every venue lock held, each
phase started at k10temp Tctl ≤ 70 °C, fsck clean after every run. ext4: one namespace, default mkfs.
Rows from `scripts/perf/matrix.sh` (30 s each) via `scripts/perf/baseline.sh`; mdtest
`-n 10000 -u -i 1`. Raw outputs: `/build/perf/full-9811f84/`, rows in `/build/perf/mx/results.tsv`
(labels `*-9811f84`).

## mdtest (k ops/s)

| | FUSE -np 32 | FUSE -np 64 | FUSE -np 32, `-w 4096 -e 4096` | interposer -np 32, `-w 4096 -e 4096` |
|---|---|---|---|---|
| directory creation | 281 | 244 | 283 | 295 |
| directory stat | 9,537 | 7,962 | 8,693 | **78** |
| directory rename | 265 | 287 | 271 | **34** |
| directory removal | 274 | 295 | 279 | **37** |
| file creation | 241 | 263 | **25** | 23 |
| file stat | 9,643 | 8,463 | 1,977 | 1,849 |
| file read | 875 | 973 | 889 | **245** |
| file removal | 304 | 358 | 333 | 281 |

(`-w 4096 -e 4096` writes and reads 4 KiB per file; the first two columns move no data.)

## fio (4 KiB rows: IOPS, p50 µs in parentheses; the rest MiB/s)

Engines: psync for ext4, FUSE and both bypass arms (the shim interposes libc); the io_uring column is
the FUSE path at depth. Jobs: QD1 = 1; "×32" = min(qd × nj, 32) psync jobs.

| row | ext4 | FUSE psync | FUSE io_uring | interposer | interposer, direct arm |
|---|---|---|---|---|---|
| 4k randread QD1 | 261k (4) | 135k (7) | 129k (6) | **594k (2)** | 259k (4) |
| 4k randread QD32×1 | 3,464k (8) | 937k (22) | 819k (36) | 2,803k (6) | 3,324k (9) |
| 4k randread QD32×4 | 3,075k (10) | 876k (24) | 1,320k (89) | 3,138k (5) | 3,004k (10) |
| 4k randwrite QD1 | 230k (4) | 89k (11) | 105k (8) | 173k (4) | 220k (4) |
| 4k randwrite QD32×1 | 3,181k (9) | 648k (20) | 662k (40) | 1,246k (22) | 2,701k (11) |
| 4k randwrite QD32×4 | 2,960k (10) | 585k (21) | 1,159k (90) | 1,241k (22) | 2,660k (11) |
| 64k randread QD16×4 | 80,794 | 47,003 | 55,863 | 34,679 | 63,384 |
| 64k randwrite QD16×4 | 78,366 | 38,120 | 36,204 | 31,273 | 60,611 |
| 1 MiB seq read QD16×1 | 93,725 | 86,748 | 71,637 | 87,522 † | 77,580 † |
| 1 MiB seq read QD16×4 | 97,268 | 91,040 | 72,176 | 89,898 † | 71,567 † |
| 1 MiB seq write QD16×1 | 86,394 | 56,648 | 32,916 | 57,942 † | 66,267 † |
| 1 MiB seq write QD16×4 | 81,896 | 53,166 | 54,774 | 53,126 † | 59,865 † |
| 4 MiB seq write QD4×4 | 62,132 | 45,206 | 44,537 | 43,244 † | 47,520 † |

† the shim routes ≥ 1 MiB I/O to FUSE's zero-copy path (0 % ring engagement, by design); every 4k and
64k bypass row engaged 100 % of its ops (the direct arm: 32 layout leases, 0 recalls).

## Reading

- **Metadata through FUSE** (no interposer involved): 241–263 k creates/s, 304–358 k removes/s,
  9.5 M stats/s. Against the 2026-10-19 table (`rung-10-localfs-comparison.md`, 41 k creates) that is
  5.9× on creates and 8.3× on removes.
- **4 KiB random I/O**: the interposer is the answer the charter promised. Reads reach ext4 (3.1 M
  ×32; QD1 594 k, 2.3× ext4 — the daemon's read cache answers from RAM without a syscall). Writes
  reach 1.25 M on the ring (1.9× FUSE) and 2.7 M on the direct arm (90 % of ext4).
- **Streaming**: FUSE reads at 93 % of ext4 (91 GB/s), writes at 65 % (53–57 GB/s); the direct arm
  writes at 77 %.

## Findings (for levers, not fixed here)

1. **A fresh file's first write.** `-w 4096` drops file creation from 241 k to 25 k/s (≈ 1.3 ms per
   file per rank) — the 4k randwrite row writes 648 k/s into existing files, so the cost is the first
   write into a new file (layout birth / publish / a wait on the create's landing). The interposer does
   not change it (23 k): the cost is in the daemon.
2. **Directory operations under `-o interception`**: stat 78 k (÷ 110), rename 34 k (÷ 8), rmdir 37 k
   (÷ 7.5) against the plain mount — while file stat and removal stay near the plain mount. Something
   the interception mount does for directories (entry/attr invalidation, or a shim path hook) costs a
   round trip per op. Reproduce with the plain `mdtest` under `-o interception` with and without the
   preload to split mount option from shim.
3. **Short-lived fds under the shim**: file read (open, 4 KiB read, close) 245 k vs 889 k through FUSE;
   the daemon logged 386 sessions for 320 k opens, so sessions are reused and the cost is the per-open
   fd bind (the fd screen round trip). The shim is built for long-lived fds (fio); per-open binding
   needs to get cheap before mdtest-style workloads can use it.
4. **64 KiB on the ring** is slower than FUSE (34.7 vs 47.0 GB/s reads): the 64 KiB slot copy through
   the arena; the direct arm (63 GB/s) avoids it.
