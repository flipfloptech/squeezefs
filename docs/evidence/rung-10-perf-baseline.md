# Rung 10 — the throughput/IOPS baseline: SqueezeFS against bare metal on kamrui (perfbase)

The baseline every throughput/IOPS lever is judged against; measurement only. On RAM-backed NVMe
(no device ceiling) SqueezeFS reaches **2–14 % of raw on every small-block row, 31–34 % on parallel
sequential writes, 71–83 % on sequential reads**. On the SSD the drive hides the big-block gap, never
the 4 KiB one. One defect explains most of the small-block gap: every read and write allocates,
fills, decodes and frees the file's WHOLE layout record, whose cost grows with the file's size (16 µs
of worker CPU per read on 256 MiB files, 110 µs on 3 GiB files).

**Venue.** kamrui: AMD Ryzen 5 5500U (6 C / 12 T), 61 GiB, kernel `7.2.6-sqz`, **clocksource
`hpet`** (the kernel offers only `hpet acpi_pm`; no TSC), fio 3.41, perf 7.0.14. SSD: WD Blue SN550
1 TB (`WDS100T2B0C`, DRAM-less, PCIe 3 x4). Binary `squeezefs 0.1.0 (ccbf1a6df307, dist profile)`,
sha256 `3ebee97f78f1…`. `ccbf1a6` adds only scripts to `dev` `bb94bf1`. Default knobs
(12 queues × depth 32, zero_copy, budget 14,623 MiB, write-back timer 1 s). Each session held the
gate lock and every build slot and started with the host < 0.5 cores busy.
**Substrates.** *SSD*: scratch LVs on `ubuntu-vg`, `blkdiscard`ed before each session and removed
afterwards. Raw and ext4 use one 16 G LV; SqueezeFS uses 2 MDT × 4 G + 2 OST × 32 G LVs. *RAM*:
`scripts/perf/ramnvme.sh`. one `brd` disk cut into 2 × 4 GiB + 2 × 14 GiB partitions, each shared by
`squeezefs nvmeof share --transport loop` and attached by `nvmeof connect`: real `/dev/nvmeXnY`
namespaces (512 B LBA). brd, not `null_blk memory_backed`: null_blk copies under one per-device
spinlock; brd reads take none. Raw and ext4 use the `ost0` namespace.
**Rows.** `scripts/perf/matrix.sh` after one lay-down of 4 × 3 GiB (per-job files, or offset regions
on a device): fio io_uring, O_DIRECT, 30 s time-based, n = 2; median (`scripts/perf/mxtable.py`),
⚠ = spread > 10 %. 30 s rows are scoping, not the charter's ≥ 60 s headline rows.
`scripts/perf/baseline.sh <session> <rep>` replays any cell.

| row | SSD raw | ext4/SSD | SqueezeFS/SSD | SqueezeFS % raw · % ext4 | RAM raw | ext4/RAM | SqueezeFS/RAM | SqueezeFS % raw · % ext4 |
|---|---|---|---|---|---|---|---|---|
| 4k randread QD1×1 (IOPS) | 10.2 k ⚠ | 10.5 k | 5.0 k | 49 % · 47 % | 63.0 k | 64.3 k | 8.3 k | 13 % · 13 % |
| 4k randread QD32×1 | 122.9 k | 126.3 k | 12.9 k | 10 % · 10 % | 176.2 k | 180.8 k | 13.6 k | 8 % · 8 % |
| 4k randread QD32×4 | 190.5 k | 184.0 k | 33.6 k | 18 % · 18 % | 630.0 k | 669.8 k | 32.6 k | 5 % · 5 % |
| 4k randwrite QD1×1 | 46.5 k | 46.4 k | 3.9 k | 8 % · 8 % | 62.3 k | 62.1 k | 3.9 k | 6 % · 6 % |
| 4k randwrite QD32×1 | 87.3 k ⚠ | 109.6 k | 6.2 k | 7 % · 6 % | 169.8 k | 171.1 k | 6.1 k | 4 % · 4 % |
| 4k randwrite QD32×4 | 104.3 k ⚠ | 73.7 k ⚠ | 11.5 k | 11 % · 16 % | 614.0 k | 633.5 k | 11.6 k | 2 % · 2 % |
| 64k randread QD16×4 (MiB/s) | 1086 ⚠ | 1114 ⚠ | 1831 | 169 % · 164 % | 12845 | 12745 | 1761 | 14 % · 14 % |
| 64k randwrite QD16×4 | 204 ⚠ | 253 ⚠ | 473 ⚠ | 232 % · 187 % | 9236 | 9138 | 720 | 8 % · 8 % |
| 1M seq read QD16×1 | 1554 | 1598 ⚠ | 1899 | 122 % · 119 % | 7595 | 7672 | 5430 | 71 % · 71 % |
| 1M seq read QD16×4 | 1800 ⚠ | 1865 | 2117 | 118 % · 114 % | 8080 | 8163 | 6697 | 83 % · 82 % |
| 1M seq write QD16×1 | 656 ⚠ | 658 ⚠ | 435 ⚠ | 66 % · 66 % | 6384 | 6174 | 1167 | 18 % · 19 % |
| 1M seq write QD16×4 | 1048 ⚠ | 1279 ⚠ | 635 ⚠ | 61 % · 50 % | 6889 | 6957 | 2359 | 34 % · 34 % |
| 4M seq write QD4×4 | 1106 ⚠ | 1255 ⚠ | 631 ⚠ | 57 % · 50 % | 6883 | 6925 | 2138 | 31 % · 31 % |

**Reading the SSD columns.** The drive writes 1.2–1.5 GB/s for ≈ 10 s (SLC cache), ≈ 830 MiB/s to
30 s, then 530–730 MiB/s (`baseline.sh slc`: 56 GiB of 1 MiB writes); every SSD write row runs past
the cache, so its spread is the drive's state. SqueezeFS "beating" raw is an artifact: raw/ext4's 64k
rows follow their own 4k randwrite rows (the drive is garbage-collecting; SqueezeFS's wrote ≈ 10×
less), and SqueezeFS reads reach the drive as whole 4 MiB blocks where fio's raw row issues 1 MiB.
SqueezeFS's 4k rows are equal on SSD and RAM (33.6 k vs 32.6 k): not device-bound. Device ÷ user
bytes on RAM: reads 1.00, writes 1.00 (4 KiB) and 1.01–1.06 (sequential); the SSD's 2.00 on every
1 MiB row, raw LV included, is dm-linear counting each 1 MiB bio twice in diskstats. FUSE requests per
user op: 1.000 on every profiled row (`.stats` `transport_ring_requests` ÷ fio ops; `opcount.sh`'s
trace buffer cannot hold 1 M requests).

**Cost per op on RAM, QD32×4** — thread CPU (`/proc/<pid>/task` deltas) ÷ IOPS, split by the share
of the class's `perf record -a -g` samples whose stack holds a phase (`perf script --no-inline`,
`scripts/perf/incl.py`).
- **4k randread.** The FUSE workers burn 3.57 cores at 32.6 k IOPS: 110 µs per read. Layout resolution
  (`layoutRecord`/`mapLookup`) takes 69 %. Allocation churn takes 75 %: `PageAllocator.map` → mmap,
  first-touch faults, ReleaseSafe's memset of `undefined`, munmap. Waiting on the process mmap_lock
  takes 20 %, and `io_uring_enter` 6 %. No worker saturates (busiest 36–54 %); the shared mmap_lock
  and the per-read CPU set the ceiling. Per read: 1 request, 3 node-cache hits, one zero-copy direct leg.
- **4k randwrite.** The handler lanes use 275 µs per write: layout 71 % (`findXattr`, which reads the
  layout's xattr record, is 30 %), allocation churn 72 %, clock reads 9 %. The workers use another
  81 µs per write: `io_uring_enter` 46 %, and `read_hpet` 27 % (≈ 22 µs per write). Every write is
  handed to a foreign lane, with 5.5 ring enters per write.
- **1M seq read ×4.** The workers use 309 µs per MiB: layout 48 %, `io_uring_enter` 28 %. Across the
  host, 54 % of samples are the nvmet-loop target's kworkers copying brd pages. 8.8 of 12 cores are
  busy, so the substrate's memcpy is the ceiling (raw used 11.8 cores for 8.1 GB/s).
- **1M seq write ×4.** The daemon uses 2.4 ms of CPU per MiB. The daemon's io-wq workers take 3.1
  cores (1.35 ms/MiB): 94 % in `shmem_file_write_iter` and 68 % waiting on that inode's rwsem. The
  lanes take 0.56 ms/MiB: layout 53 %, xattr 29 %.

**Ranked bottlenecks, levers, expected gains (hypotheses; the arithmetic is from the numbers above).**
1. **Whole-layout resolution per op, O(file size) and allocating.** 256 MiB files (64 blocks): 16 µs of
   worker CPU per read and 247.6 k IOPS at QD32×4 (a 6 s smoke row, n = 1). 3 GiB files (768 blocks):
   107–110 µs. Reading only the first 256 MiB of the 3 GiB files still costs 108 µs (`ram-sqz-p256`), so
   the cost follows the file's size, not the range read: ≈ 7.7 µs + 0.13 µs per block. *Lever*: resolve
   the one block entry from a borrowed node-cache view or a per-thread scratch buffer. No heap
   allocation, no full decode/validate, and one resolution per op instead of the probe plus the
   direct-leg recheck. *Expected*: worker CPU per read falls to ≤ 16 µs, so 4k randread QD32×4 rises
   ≥ 7× (to ≈ 250 k; 110/16 × 32.6 k ≈ 224 k, and the smoke measured 247.6 k). QD1 latency drops from
   109 µs to ≈ 30 µs (≈ 33 k IOPS). Lane CPU per write drops from 275 µs to ≈ 80 µs, so 4k randwrite
   rises ≈ 3× (to ≈ 35 k). 64k random rises in step. The mmap_lock wait (17–20 %) disappears with
   the allocation.
2. **Sequential writes serialize on the zero-copy bounce slab's single memfd.** All queues'
   `WRITE_FIXED` writes go into one shmem inode (`transport/worker.zig` `Slab`), and its i_rwsem
   serializes them (68 % of io-wq time). *Lever*: stage through the slab's VA (a memcpy on the
   worker, no write through the inode), or one memfd per queue. *Expected*: the 1.35 ms/MiB of io-wq
   time falls to its copy share (17 % ≈ 0.23 ms/MiB). Sequential write ×4 then reaches ≈ 2× (≈ 4.5–5
   GB/s), where lever 1's lane cost (0.56 ms/MiB on 1.3 cores) and the target's memcpy are next.
3. **HPET clocksource on kamrui.** `read_hpet` is 27 % of the workers' write samples, 7 % of the
   lanes', and 19 % of all samples on the 4k randwrite row (idle transitions included). *Lever*: find
   why the kernel does not offer TSC (boot log; `tsc=reliable`) before trusting any per-op number
   from this box. In the product, read the clock once per op and use the coarse clock outside
   latency histograms. *Expected*: up to 22 µs per write on the worker; the row's 19 % of samples is the upper bound.
4. **Every write crosses threads** (worker → foreign lane, 5.5 enters per write); a QD1 write takes
   228 µs, raw takes 10 µs. *Lever*: run the data class on the worker's own lane, as reads already do
   (0003 §AB.3 for writes). *Expected*: one handoff pair, 18–33 µs per write at QD1
   (`rung-10-dev-perf.md` §9.4). It matters only after lever 1.
5. **One io_uring job spreads over the worker queues**: QD32×1 gives only 1.6× QD1 (13.6 k), busiest
   worker 31–46 %. After lever 1, re-measure against dev-perf §9.7 (b) (N workers per queue).
6. **Sequential reads are bound by the substrate** (83 % of raw): the loop target's memcpy uses 54 % of
   the host. Lever 1 frees about 1 core of workers. *Expected* ≤ +10 %. On nvmet-tcp or real drives
   this row is the device's.
7. Minor: `sqz-membudget` costs 0.04–0.05 cores on every row, whatever the load (4 % of its thread).

**Harness (landed).** `scripts/perf/ramnvme.sh up|down|status` (`down` checked: no subsystem, port
link, controller, port or brd left; the nvmet modules were loaded before, so they stay loaded).
`scripts/perf/matrix.sh`, `scripts/perf/baseline.sh`, `scripts/perf/mxtable.py`, and
`scripts/perf_mount.sh --ram`. Raw-device rows refuse a device that is mounted or carries a
signature. Every SqueezeFS session ended with a clean `fsck --offline` (0 findings).
