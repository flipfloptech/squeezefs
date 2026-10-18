# Rung 10 — the throughput/IOPS baseline on the Linux workstation (the `local` venue)

The reference every lever's A/B is judged against on this box; measurement only. The method is
`rung-10-perf-baseline.md`'s: `scripts/perf/matrix.sh` over RAM-backed NVMe namespaces (brd cut into
2 MDT + 2 OST, shared and connected through the product's own nvmet verbs over nvme-loop), the dist
build, fio io_uring O_DIRECT, 4 × 3 GiB lay-down, 20 s per row, the whole venue held
(`scripts/perf/quiet.sh`). Box: AMD Ryzen AI MAX+ PRO 395 (32 threads), 109 GiB, clocksource TSC,
kernel `7.2.8-7-omarchy-sqz`, binary `1d5c45a` (dist). Rows under `/build/perf/mx/ram-{raw,sqz}-r1/`.

| row | raw IOPS · MiB/s | SqueezeFS IOPS · MiB/s | % of raw | daemon cores (busiest classes) |
|---|---|---|---|---|
| 4k randread QD1×1 | 263.9 k · 1031 | 104.8 k · 409 | 40 % | 0.63 (`sqz-fuse-ur` 0.61) |
| 4k randread QD32×1 | 1047 k · 4090 | 314.2 k · 1227 | 30 % | 1.01 (`sqz-fuse-ur` 1.00) |
| 4k randread QD32×4 | 2063 k · 8057 | 793.3 k · 3099 | 38 % | 4.11 (`sqz-fuse-ur` 4.10) |
| 4k randwrite QD1×1 | 240.6 k · 940 | 57.4 k · 224 | 24 % | 0.88 (`sqz-fuse-ur` 0.45, `sqz-fuse-ln` 0.42) |
| 4k randwrite QD32×1 | 842.9 k · 3293 | 223.0 k · 871 | 26 % | 1.96 (`sqz-fuse-ln` 1.04, `sqz-fuse-ur` 0.90) |
| 4k randwrite QD32×4 | 2068 k · 8078 | 394.4 k · 1541 | 19 % | 7.38 (`sqz-fuse-ln` 4.20, `sqz-fuse-ur` 3.15) |
| 64k randread QD16×4 | 994.6 k · 62166 | 508.4 k · 31776 | 51 % | 3.81 (`sqz-fuse-ur` 3.79) |
| 64k randwrite QD16×4 | 917.9 k · 57367 | 299.0 k · 18687 | 33 % | 7.06 (`sqz-fuse-ln` 4.09, `sqz-fuse-ur` 2.93) |
| seq read 1M QD16×1 | 51.9 k · 51931 | 42.4 k · 42374 | 82 % | 1.38 (`sqz-fuse-ur` 1.00, io-wq 0.36) |
| seq read 1M QD16×4 | 80.2 k · 80249 | 68.4 k · 68395 | 85 % | 3.35 (`sqz-fuse-ur` 2.49, io-wq 0.83) |
| seq write 1M QD16×1 | 35.2 k · 35242 | 5.96 k · 5961 | 17 % | 1.28 (io-wq 0.61, `sqz-nvme` 0.14, `sqz-ckpt` 0.12) |
| seq write 1M QD16×4 | 58.7 k · 58706 | 14.7 k · 14721 | 25 % | 5.46 (io-wq 2.68, `sqz-fuse-ln` 0.87, `sqz-nvme` 0.60, `sqz-ckpt` 0.47) |
| seq write 4M QD4×4 | 14.4 k · 57722 | 3.55 k · 14178 | 25 % | 4.86 (io-wq 2.50, `sqz-fuse-ln` 0.73, `sqz-nvme` 0.53, `sqz-ckpt` 0.41) |

Latency (p50/p99 µs, SqueezeFS vs raw): 4k randread QD1 7.5/14.5 vs 3.0/6.0; 4k randwrite QD1
14.3/43.3 vs 3.1/6.3; seq write 1M QD16×1 2507/5931 vs 461/569. `dev/user` bytes 1.00 on every read
row and 1.00–1.01 on every write row (no amplification).

**Against the charter's bars** (32-core client; §1.4): sequential read 68.4 GB/s clears 43.9 (on a
loopback RAM substrate, so the fabric venue decides); **sequential write 14.7 GB/s vs 36.4 (2.5×
short)**; **4k write IOPS 394 k vs 727 k (1.8×)**; 4k read IOPS 793 k vs 942 k (1.2×).

**What the rows say about the levers** (the hypotheses of `rung-10-perf-baseline.md` and
`rung-10-seqwrite.md`, read against this box).
- Sequential writes: the daemon spends half its cores in io-wq (the `WRITE_FIXED` extraction of every
  big write into the bounce slab) and the handler lanes park on DMAs (`rung-10-seqwrite.md`: a DMA
  blocks its lane thread, ≈ 4.5 in flight). Levers (a) fiber-aware device waits and (b) zero-copy big
  writes from the slot; raw gives 58.7 GB/s here, so the copies are the ceiling, not the device.
- 4k writes: 7.4 cores for 394 k IOPS ≈ 19 µs of daemon CPU per write, split `sqz-fuse-ln` 4.2 /
  `sqz-fuse-ur` 3.2 — every write crosses from the worker to a foreign lane (lever 4: the data class
  on the worker's own lane, as reads already run). Reads cost ≈ 5 µs (4.1 cores at 793 k).
- 4k reads: `sqz-fuse-ur` alone; 38 % of raw at QD32×4 with the host at 6.9 of 32 cores — the
  per-request transport cost (lever 5: N workers per kernel queue) is what remains after lever 1.

This box has a TSC clocksource (kamrui's HPET lever 3 does not apply).
