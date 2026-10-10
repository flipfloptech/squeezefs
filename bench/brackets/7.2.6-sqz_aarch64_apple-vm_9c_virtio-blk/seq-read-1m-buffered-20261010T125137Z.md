### seq-read-1m-buffered — SCOPING-INVALID

instrument `fio-3.36 libaio read bs=1m direct=0 iodepth=32 numjobs=4 size=1g/job` · substrate `nvmet-tcp on 127.0.0.1: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (ce0edf4f004d, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261010T125137Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | MiB/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 2662 | 42205 | 99090 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 2 | B | 2971 | 39584 | 89653 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 3 | B | 2697 | 41681 | 103285 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 4 | A | 2068 | 51118 | 152044 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | NO |

A mean 2365 (spread 593.7), B mean 2834 (spread 273.7) → A/B = 0.835; all legs: min 2068 max 2971 spread 34.7 %.
A = B (same binary): the spread IS the venue's noise band for this row: **34.7 %**.
- INVALID leg 4/A: `mem_budget_green` — mem_budget_level 1 at the after-snapshot (0 = Green; budget 6144 MiB)
