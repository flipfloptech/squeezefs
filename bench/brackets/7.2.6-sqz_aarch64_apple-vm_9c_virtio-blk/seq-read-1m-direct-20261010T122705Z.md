### seq-read-1m-direct — SCOPING-VALID

instrument `fio-3.36 libaio read bs=1m direct=1 iodepth=32 numjobs=4 size=1g/job` · substrate `nvmet-tcp on 127.0.0.1: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (ce0edf4f004d, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261010T122705Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | MiB/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 7999 | 15139 | 34865 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 2 | B | 8312 | 14615 | 33423 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 3 | B | 8067 | 14877 | 36962 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 4 | A | 7912 | 15008 | 39584 | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |

A mean 7956 (spread 86.4), B mean 8190 (spread 245.3) → A/B = 0.971; all legs: min 7912 max 8312 spread 5.0 %.
A = B (same binary): the spread IS the venue's noise band for this row: **5.0 %**.
