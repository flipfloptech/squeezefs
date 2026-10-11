### rand-4k-read-unaligned — INVALID

instrument `fio-3.36 libaio randread bs=4k blockalign=512 direct=1 iodepth=32 numjobs=4 size=1g/job` · substrate `file-backed sparse volumes on /build (ext4 on /dev/vdc, shared disk: diskstats ratio is an upper bound)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `A squeezefs 0.1.0 (564631b052a1, gate profile, zig 0.17.0) / B squeezefs 0.1.0 (8ed8824dae3b, gate profile, zig 0.17.0)` · posture `symmetric` · format `defaults` · mount `defaults` · 20261011T183744Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | IOPS | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 14922 | 7242 | 28180 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | NO |
| 2 | B | 15035 | 7176 | 28180 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 3 | B | 15770 | 6849 | 27394 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 4 | A | 19698 | 5341 | 30540 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | NO |

| leg | arm | read device÷user (stats) | read device÷user (diskstats data) | bound | rareq-sz B (stats / disk) | ranged | demand blk | prefetch blk | lane blk | image | window | compose | write-side | direct leg | plane ledger |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 2.958 | 3.102 | 2.100 | 14515 / 11634 | 6108413952 | 4303355904 | 0 | 0 | 0 | 0 | 0 | 0 | 437108736 | closed |
| 2 | B | 2.005 | 2.151 | 2.100 | 9819 / 8114 | 6174121984 | 796917760 | 0 | 0 | 0 | 0 | 0 | 0 | 439201792 | closed |
| 3 | B | 1.991 | 2.120 | 2.100 | 9763 / 8052 | 6466592768 | 788529152 | 0 | 0 | 0 | 0 | 0 | 0 | 460984320 | closed |
| 4 | A | 2.678 | 2.679 | 2.100 | 13138 / 11417 | 8066523136 | 4320133120 | 0 | 0 | 0 | 0 | 0 | 0 | 576380928 | closed |

| leg | arm | lookups | getattrs | dirents | node reads | node bytes | node bytes/record | node reads/record | cache hits | cache misses |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 15 | 9 | 0 | 0 | 0 | 0.000 | 0.000 | 7594840 | 0 |
| 2 | B | 15 | 9 | 0 | 0 | 0 | 0.000 | 0.000 | 7660159 | 0 |
| 3 | B | 15 | 9 | 0 | 0 | 0 | 0.000 | 0.000 | 8027796 | 0 |
| 4 | A | 15 | 12 | 0 | 0 | 0 | 0.000 | 0.000 | 10024880 | 0 |

A mean 17310 (spread 4775), B mean 15403 (spread 735.4) → A/B = 1.124; all legs: min 14922 max 19698 spread 29.2 %.
- INVALID leg 1/A: `read_amplification_bound` — device ÷ user 2.9579 by stats arms (stats 2.9579, diskstats 3.1017) vs bound 2.1000 — max over offsets of W(off,4096)/4096 = 8192/4096 = 2 at a 512 B grain, × (1 + f), f = 5 %; arms: read_dma_ranged_bytes 6108413952, read_dma_block_demand_bytes 4303355904, transport_zc_direct_leg_dma_bytes 437108736
- INVALID leg 4/A: `read_amplification_bound` — device ÷ user 2.6776 by stats arms (stats 2.6776, diskstats 2.6786) vs bound 2.1000 — max over offsets of W(off,4096)/4096 = 8192/4096 = 2 at a 512 B grain, × (1 + f), f = 5 %; arms: read_dma_ranged_bytes 8066523136, read_dma_block_demand_bytes 4320133120, transport_zc_direct_leg_dma_bytes 576380928
