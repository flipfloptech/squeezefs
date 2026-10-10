### shim-rand-4k-write — SCOPING-VALID

instrument `fio-3.36 psync randwrite bs=4k direct=1 QD1×numjobs=4 size=512m/job` · substrate `nvmet-loop: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (ce0edf4f004d, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261010T155710Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | IOPS | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 4514 | 313.3 | 10813 | 1.000 | 1.000 / 0.000 | 4096 / 4096 | 0+0=0 | ×1.0000 | 0 | 0 | 0 | yes |
| 2 | B | 13950 | 162.8 | 1614 | 1.062 | 1.070 / 0.005 | 4372 / 4400 | 0+61=61 | ×1.0000 | 0 | 0 | 0 | yes |
| 3 | B | 13932 | 164.9 | 2007 | 1.127 | 1.138 / 0.011 | 4662 / 4703 | 0+121=121 | ×1.0000 | 0 | 0 | 0 | yes |
| 4 | A | 3786 | 497.7 | 10289 | 1.004 | 1.004 / 0.001 | 4114 / 4114 | 0+1=1 | ×1.0000 | 0 | 0 | 3 | yes |

A mean 4150 (spread 728.6), B mean 13941 (spread 17.8) → A/B = 0.298; all legs: min 3786 max 13950 spread 112.4 %.
A = B (same binary): the spread IS the venue's noise band for this row: **112.4 %**.
