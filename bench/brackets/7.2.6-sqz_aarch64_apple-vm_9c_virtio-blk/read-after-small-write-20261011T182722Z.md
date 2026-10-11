### read-after-small-write — INVALID

instrument `fio-3.36 libaio randread bs=4k blockalign=512 direct=1 iodepth=32 numjobs=4 size=1g/job; warm-up randwrite bs=512 (rand-bs.fio)` · substrate `nvmet-loop: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `A squeezefs 0.1.0 (564631b052a1, gate profile, zig 0.17.0) / B squeezefs 0.1.0 (8ed8824dae3b, gate profile, zig 0.17.0)` · posture `symmetric` · format `defaults` · mount `defaults` · 20261011T182722Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | IOPS | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 20632 | 5341 | 19005 | — | — / — | 4194304 / — | 0+6=6 | — | 0 | 0 | 0 | NO |
| 2 | B | 19550 | 5145 | 39584 | — | — / — | 4194304 / — | 0+12=12 | — | 0 | 0 | 0 | yes |
| 3 | B | 25149 | 4489 | 15401 | — | — / — | 4194304 / — | 0+15=15 | — | 0 | 0 | 0 | yes |
| 4 | A | 25114 | 4489 | 15270 | — | — / — | 4194304 / — | 0+10=10 | — | 0 | 0 | 0 | NO |

| leg | arm | read device÷user (stats) | read device÷user (diskstats data) | bound | rareq-sz B (stats / disk) | ranged | demand blk | prefetch blk | lane blk | image | window | compose | write-side | direct leg | plane ledger |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 2.644 | 2.644 | 2.100 | 12959 / 11282 | 8402264064 | 4328521728 | 0 | 0 | 0 | 0 | 54415360 | 25165824 | 600150016 | closed |
| 2 | B | 2.009 | 2.009 | 2.100 | 9803 / 8580 | 7945396224 | 985661440 | 0 | 0 | 0 | 0 | 102555648 | 50331648 | 567685120 | closed |
| 3 | B | 1.993 | 1.993 | 2.100 | 9749 / 8532 | 10156769280 | 1199570944 | 0 | 0 | 0 | 0 | 168460288 | 62914560 | 729190400 | closed |
| 4 | A | 3.189 | 3.189 | 2.100 | 15629 / 13557 | 10173796352 | 8631877632 | 0 | 0 | 0 | 0 | 110583808 | 41943040 | 729018368 | closed |

| leg | arm | lookups | getattrs | dirents | node reads | node bytes | node bytes/record | node reads/record | cache hits | cache misses |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 15 | 9 | 0 | 0 | 0 | 0.000 | 0.000 | 9684810 | 0 |
| 2 | B | 15 | 13 | 0 | 2 | 524288 | 18725 | 0.071 | 8387618 | 0 |
| 3 | B | 15 | 10 | 0 | 2 | 524288 | 20972 | 0.080 | 10760202 | 0 |
| 4 | A | 15 | 10 | 0 | 0 | 0 | 0.000 | 0.000 | 9744120 | 0 |

A mean 22873 (spread 4482), B mean 22349 (spread 5599) → A/B = 1.023; all legs: min 19550 max 25149 spread 24.8 %.
- INVALID leg 1/A: `read_amplification_bound` — device ÷ user 2.6394 by stats arms minus write_side_read_dma_bytes (stats 2.6443, diskstats 2.6443) vs bound 2.1000 — a 4 KiB read at a 512 B grain spans ≤ 2 LBAs whatever composes it (extent runs over a RANGED base), × (1 + f); arms: read_dma_ranged_bytes 8402264064, read_dma_block_demand_bytes 4328521728, write_side_read_dma_bytes 25165824, compose_read_dma_bytes 54415360, transport_zc_direct_leg_dma_bytes 600150016
- INVALID leg 4/A: `read_amplification_bound` — device ÷ user 3.1825 by stats arms minus write_side_read_dma_bytes (stats 3.1893, diskstats 3.1893) vs bound 2.1000 — a 4 KiB read at a 512 B grain spans ≤ 2 LBAs whatever composes it (extent runs over a RANGED base), × (1 + f); arms: read_dma_ranged_bytes 10173796352, read_dma_block_demand_bytes 8631877632, write_side_read_dma_bytes 41943040, compose_read_dma_bytes 110583808, transport_zc_direct_leg_dma_bytes 729018368
