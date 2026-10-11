### rand-4k-read-unaligned — INVALID

instrument `fio-3.36 libaio randread bs=4k blockalign=512 direct=1 iodepth=32 numjobs=4 size=1g/job` · substrate `nvmet-loop: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `A squeezefs 0.1.0 (564631b052a1, gate profile, zig 0.17.0) / B squeezefs 0.1.0 (8ed8824dae3b, gate profile, zig 0.17.0)` · posture `symmetric` · format `defaults` · mount `defaults` · 20261011T182210Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | IOPS | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 14794 | 7373 | 27132 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | NO |
| 2 | B | 15079 | 7307 | 27132 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 3 | B | 16886 | 6586 | 22938 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 4 | A | 17197 | 6390 | 23462 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | NO |

| leg | arm | read device÷user (stats) | read device÷user (diskstats data) | bound | rareq-sz B (stats / disk) | ranged | demand blk | prefetch blk | lane blk | image | window | compose | write-side | direct leg | plane ledger |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 2.986 | 2.986 | 2.100 | 14643 / 12711 | 6059118592 | 4366270464 | 0 | 0 | 0 | 0 | 0 | 0 | 431751168 | closed |
| 2 | B | 1.970 | 1.970 | 2.100 | 9645 / 8433 | 6194098176 | 666894336 | 0 | 0 | 0 | 0 | 0 | 0 | 441032704 | closed |
| 3 | B | 2.001 | 2.001 | 2.100 | 9780 / 8553 | 6946496512 | 864026624 | 0 | 0 | 0 | 0 | 0 | 0 | 493211648 | closed |
| 4 | A | 2.811 | 2.811 | 2.100 | 13785 / 11986 | 7046062080 | 4336910336 | 0 | 0 | 0 | 0 | 0 | 0 | 500162560 | closed |

| leg | arm | lookups | getattrs | dirents | node reads | node bytes | node bytes/record | node reads/record | cache hits | cache misses |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 15 | 9 | 0 | 0 | 0 | 0.000 | 0.000 | 7530738 | 0 |
| 2 | B | 15 | 9 | 0 | 0 | 0 | 0.000 | 0.000 | 7684021 | 0 |
| 3 | B | 15 | 9 | 0 | 0 | 0 | 0.000 | 0.000 | 8611798 | 0 |
| 4 | A | 15 | 9 | 0 | 0 | 0 | 0.000 | 0.000 | 8752898 | 0 |

A mean 15995 (spread 2404), B mean 15983 (spread 1807) → A/B = 1.001; all legs: min 14794 max 17197 spread 15.0 %.
- INVALID leg 1/A: `read_amplification_bound` — device ÷ user 2.9855 by diskstats (exclusive namespaces) (stats 2.9855, diskstats 2.9855) vs bound 2.1000 — max over offsets of W(off,4096)/4096 = 8192/4096 = 2 at a 512 B grain, × (1 + f), f = 5 %; arms: read_dma_ranged_bytes 6059118592, read_dma_block_demand_bytes 4366270464, transport_zc_direct_leg_dma_bytes 431751168
- INVALID leg 4/A: `read_amplification_bound` — device ÷ user 2.8113 by diskstats (exclusive namespaces) (stats 2.8113, diskstats 2.8113) vs bound 2.1000 — max over offsets of W(off,4096)/4096 = 8192/4096 = 2 at a 512 B grain, × (1 + f), f = 5 %; arms: read_dma_ranged_bytes 7046062080, read_dma_block_demand_bytes 4336910336, transport_zc_direct_leg_dma_bytes 500162560
