### seq-read-1m-direct — SCOPING-INVALID

instrument `fio-3.36 libaio read bs=1m direct=1 iodepth=32 numjobs=4 size=512m/job` · substrate `nvmet-tcp on 127.0.0.1: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (ce0edf4f004d, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261010T143010Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | MiB/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 7955 | 14352 | 41681 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | NO |
| 2 | B | 8529 | 13697 | 37487 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 3 | B | 7262 | 15925 | 46924 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 4 | A | 7799 | 15008 | 41681 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |

A mean 7877 (spread 156.4), B mean 7895 (spread 1267) → A/B = 0.998; all legs: min 7262 max 8529 spread 16.1 %.
A = B (same binary): the spread IS the venue's noise band for this row: **16.1 %**.
- INVALID leg 1/A: `mem_budget_green` — mem_budget_level 2 at the after-snapshot (0 = Green; budget 7168 MiB; top components exec_fiber_stacks 703 MiB, transport_zc_bounce 288 MiB, transport_kmbuf_region 288 MiB, block_alloc_table 0 MiB)
