### read-after-small-write — INVALID

instrument `fio-3.36 libaio randread bs=4k blockalign=512 direct=1 iodepth=32 numjobs=4 size=1g/job; warm-up randwrite bs=512 (rand-bs.fio)` · substrate `file-backed sparse volumes on /build (ext4 on /dev/vdc, shared disk: diskstats ratio is an upper bound)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `A squeezefs 0.1.0 (564631b052a1, gate profile, zig 0.17.0) / B squeezefs 0.1.0 (8ed8824dae3b, gate profile, zig 0.17.0)` · posture `symmetric` · format `defaults` · mount `defaults` · 20261011T184253Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | IOPS | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 25656 | 4358 | 15532 | — | — / — | 4194304 / — | 11+0=11 | — | 0 | 0 | 0 | NO |
| 2 | B | 25536 | 4227 | 17957 | — | — / — | 4194304 / — | 11+0=11 | — | 0 | 0 | 0 | yes |
| 3 | B | 13873 | 8454 | 32375 | — | — / — | 4194304 / — | 7+0=7 | — | 0 | 0 | 0 | NO |
| 4 | A | 19037 | 5997 | 20578 | — | — / — | 4194304 / — | 16+0=16 | — | 0 | 0 | 0 | NO |

| leg | arm | read device÷user (stats) | read device÷user (diskstats data) | bound | rareq-sz B (stats / disk) | ranged | demand blk | prefetch blk | lane blk | image | window | compose | write-side | direct leg | plane ledger |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 2.649 | 2.649 | 2.100 | 12984 / 11299 | 10383286272 | 5402263552 | 0 | 0 | 0 | 0 | 124801024 | 46137344 | 745488384 | closed |
| 2 | B | 1.987 | 1.988 | 2.100 | 9724 / 8507 | 10360037376 | 1199570944 | 0 | 0 | 0 | 0 | 125648896 | 46137344 | 740552704 | closed |
| 3 | B | 1.994 | 1.996 | 2.100 | 9700 / 8486 | 5689794560 | 633339904 | 0 | 0 | 0 | 0 | 41328640 | 29360128 | 404807680 | closed |
| 4 | A | 3.639 | 3.649 | 2.100 | 17724 / 15356 | 7701032960 | 8573157376 | 0 | 0 | 0 | 0 | 135512064 | 67108864 | 551841792 | closed |

| leg | arm | lookups | getattrs | dirents | node reads | node bytes | node bytes/record | node reads/record | cache hits | cache misses |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 15 | 10 | 0 | 0 | 0 | 0.000 | 0.000 | 10984789 | 0 |
| 2 | B | 15 | 9 | 0 | 0 | 0 | 0.000 | 0.000 | 10933183 | 0 |
| 3 | B | 15 | 9 | 0 | 0 | 0 | 0.000 | 0.000 | 6516854 | 0 |
| 4 | A | 15 | 9 | 0 | 0 | 0 | 0.000 | 0.000 | 8133289 | 0 |

A mean 22347 (spread 6619), B mean 19704 (spread 11663) → A/B = 1.134; all legs: min 13873 max 25656 spread 56.0 %.
- INVALID leg 1/A: `read_amplification_bound` — device ÷ user 2.6413 by stats arms minus write_side_read_dma_bytes (stats 2.6487, diskstats 2.6494) vs bound 2.1000 — a 4 KiB read at a 512 B grain spans ≤ 2 LBAs whatever composes it (extent runs over a RANGED base), × (1 + f); arms: read_dma_ranged_bytes 10383286272, read_dma_block_demand_bytes 5402263552, write_side_read_dma_bytes 46137344, compose_read_dma_bytes 124801024, transport_zc_direct_leg_dma_bytes 745488384
- INVALID leg 3/B: `mem_budget_green` — mem_budget_level 2 at the after-snapshot (0 = Green; budget 6144 MiB; top components transport_kmbuf_region 288 MiB, hot_block_tier 64 MiB, nvme_ranged_slab 36 MiB, kv_node_cache 1 MiB)
- INVALID leg 4/A: `read_amplification_bound` — device ÷ user 3.6250 by stats arms minus write_side_read_dma_bytes (stats 3.6394, diskstats 3.6495) vs bound 2.1000 — a 4 KiB read at a 512 B grain spans ≤ 2 LBAs whatever composes it (extent runs over a RANGED base), × (1 + f); arms: read_dma_ranged_bytes 7701032960, read_dma_block_demand_bytes 8573157376, write_side_read_dma_bytes 67108864, compose_read_dma_bytes 135512064, transport_zc_direct_leg_dma_bytes 551841792
