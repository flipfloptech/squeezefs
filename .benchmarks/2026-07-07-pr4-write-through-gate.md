# PR 4 gate: complete-block write-through — before/after

Design: `docs/design-zero-copy-write-path.md` §5.3 (PR 4). Gate: **large-seq
write ≥ 1.9× the freshly re-run current-dev baseline**, with the design's
pre-authorized fallback band (1.6–1.9× → record and proceed; acceptance
judged cumulatively at the unchanged ≥ 3× gate in PR 5/PR 7).

## Method

Unprivileged local mount, same volume shape for both binaries, recreated
fresh per run set (btrfs backing, `chattr +C` on both files):

```
~/tmp/sqfs_p4/meta.bin  256M   ~/tmp/sqfs_p4/data.bin  20G
squeezefs format sqmeta://…/meta.bin sqdata://…/data.bin       # 4 MiB blocks
squeezefs mount sqmeta://…/meta.bin ~/tmp/sqfs_p4/mnt \
  --disk-cache-paths ~/tmp/sqfs_p4/staging --daemon --log-file … \
  --uid $(id -u) --gid $(id -g)
squeezefs bench ~/tmp/sqfs_p4/mnt -t 10 --large-size 1024 --only large-seq-write
```

FUSE-over-io_uring armed on every mount (`transport armed for this
session`, 32 queues × depth 4). BEFORE = dev @ b7a2973 (PRs 1–3 landed);
AFTER = dev + PR 4. Box note: a separate production squeezefs daemon
(~27 GB RSS) shares the machine — single-run read numbers are noisy;
write rows were stable across runs.

## Primary gate — Write Large Seq, t=10 × 1 GiB, 1 MiB chunks

| Run | BEFORE (dev) | AFTER (PR 4) |
|---|---|---|
| 1 | 946.46 MiB/s | 1693.69 MiB/s |
| 2 | 904.46 MiB/s | 1611.00 MiB/s |
| 3 | 929.15 MiB/s | 1728.83 MiB/s |
| 4 | 905.81 MiB/s (fresh volume) | 1742.46 MiB/s |
| second fresh volume | — | 1620.70 MiB/s |
| **mean** | **~921 MiB/s** | **~1679 MiB/s** |

**Ratio: ~1.82×** — inside the design's pre-authorized 1.6–1.9× band
(recorded shortfall vs 1.9×: ~0.08×; proceed-and-judge-cumulatively per
the PR 4 gate text). Avg ack latency 1.04–1.11 ms → 0.57–0.62 ms.

Note the baseline itself moved since the attribution doc (430–512 MiB/s
pre-PR 1): PRs 1–3 + machine state put current dev at ~921 MiB/s, so the
absolute AFTER number (~1.68 GB/s) already exceeds the original ≥ 1.3 GB/s
stack target; the ratio gate is measured against the honest fresh baseline
as required.

## Adoption / mechanism proof (stats inode, AFTER, after 4×10 GiB runs)

| Field | Value | Meaning |
|---|---|---|
| `write_through_blocks` | 10200 | ≈ striped-seq volume: full adoption |
| `write_through_bytes` | 42781900800 | ~40 GiB direct-to-device |
| `write_through_fallbacks` | 0 | no backpressure degradation |
| `active_block_memset_elided_bytes` | 41382563840 | seed memsets gone |
| `active_block_cow_copies` | 0 | no read/write collisions paid |
| `nvme_staging_current_bytes` | 0 | **staging bypassed entirely** (BEFORE left ~2.6 GB staged after the same workload) |
| `uring_queue_full` / `writeback_hard_failures` | 0 / 0 | clean |

Staging-write-volume delta: BEFORE writes every completed block to the
staging mmap + msync before the device write; AFTER writes zero staging
bytes for sequential streams — also visible operationally: dev's teardown
staged-drain (2.6 GB backlog) vs AFTER's immediate clean unmount.

## No-regression rows (full bench, t=10, defaults: large 128 MB/thread)

| Row | BEFORE | AFTER | Δ |
|---|---|---|---|
| Write Large Seq | 675.31 MiB/s | 1183.26 MiB/s | **+75%** |
| Write Large Rand | 481.45 MiB/s | 502.30 MiB/s | +4% |
| Write Small Seq | 233.91 MiB/s | 243.89 MiB/s | +4% |
| Write Small Rand | 213.29 MiB/s | 235.83 MiB/s | +11% |
| Read Small Seq | 2654.82 MiB/s | 3214.38 MiB/s | +21% |
| Read Small Rand | 2545.54 MiB/s | 3163.14 MiB/s | +24% |
| Read Large Seq | 1238.29 MiB/s | 1105.60 MiB/s | see below |
| Read Large Rand | 2097.07 MiB/s | 1191.53 MiB/s | see below |
| Metadata Stat | 132178 ops/s | 171999 ops/s | +30% |
| Metadata Mkdir | 29824 ops/s | 31513 ops/s | +6% |
| Metadata Readdir | 16275 ops/s | 16924 ops/s | +4% |
| Metadata Rmdir | 33395 ops/s | 28717 ops/s | −14% (noise: repeated metadata-only runs show ±17% run-to-run on the SAME binary: Mkdir 24574→20494) |
| Metadata Delete | 3588 ops/s | 3605 ops/s | ±0 |

Large-read rows are coupled to the write phase in this instrument: BEFORE
leaves the just-written blocks resident in the staging mmap (reads hit
mmap for free until writeback drains); AFTER the same blocks are already
on the device, so the read phase does honest device reads. Decoupled
cold-read check (write 4 GiB + fsync, full unmount, remount, read):
BEFORE {1.1 GB/s, 282 MB/s}, AFTER {919 MB/s, 1.1 GB/s} — distributions
overlap; no read-path signal (PR 4 changes no read code beyond the
coverage-aware RAM-buffer hit).

## Mixed write + hot-read row (validates the skipped read-LRU put)

512 MiB hot file re-read while a concurrent 8 GiB sequential writer runs:
BEFORE 2.0 GB/s, AFTER 1.9 GB/s — flat; the §5.3 "no `read_lru.put` for
striped write-through" policy does not cost hot reads (and spares the LRU
2,560 plaintext evictions per 10 GiB stream).

## Ack-latency row

Block-completing request ack: 1.04–1.11 ms (staging copy + msync) →
0.57–0.62 ms (DMA + merge) at t=10 — the §5.3 latency-shape trade measured
*better* than before under concurrency, because the staging mmap write +
msync cost more than the pipelined DMA.
