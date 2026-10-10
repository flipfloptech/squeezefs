### seq-read-1m-buffered — SCOPING-VALID

instrument `fio-3.36 libaio read bs=1m direct=0 iodepth=32 numjobs=4 size=512m/job` · substrate `nvmet-tcp on 127.0.0.1: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (ce0edf4f004d, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261010T151933Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | MiB/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 1419 | 58458 | 683672 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 7 | yes |
| 2 | B | 2984 | 39584 | 69730 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 3 | B | 2930 | 40108 | 71827 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 4 | A | 2141 | 54264 | 103285 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 8 | yes |

A mean 1780 (spread 721.6), B mean 2957 (spread 53.7) → A/B = 0.602; all legs: min 1419 max 2984 spread 66.1 %.
A = B (same binary): the spread IS the venue's noise band for this row: **66.1 %**.
