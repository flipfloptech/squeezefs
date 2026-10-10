### creates-mdtest — SCOPING-VALID

instrument `mdtest (ior io500-sc19-605-g5528b36) -C -F -u -z 2 -b 8 -n <sized from the warm-up rate>, mpirun -np 4` · substrate `nvmet-loop: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (ce0edf4f004d, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261010T160749Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | creates/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 3719 | — | — | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 2 | B | 3906 | — | — | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 3 | B | 3488 | — | — | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 4 | A | 4623 | — | — | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |

A mean 4171 (spread 904.1), B mean 3697 (spread 418.3) → A/B = 1.128; all legs: min 3488 max 4623 spread 28.9 %.
A = B (same binary): the spread IS the venue's noise band for this row: **28.9 %**.

notes: 354384 files across 4 tasks × 73 dirs; elapsed 95.3 s (sized from the warm-up rate)
