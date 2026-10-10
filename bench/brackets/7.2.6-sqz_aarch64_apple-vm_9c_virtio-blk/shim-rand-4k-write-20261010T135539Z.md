### shim-rand-4k-write — SCOPING-INVALID

instrument `fio-3.36 psync randwrite bs=4k direct=1 QD1×numjobs=4 size=1g/job` · substrate `nvmet-loop: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (966105390f14-dirty, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261010T135539Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | IOPS | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 4377 | 774.1 | 5734 | 1.003 | 1.007 / 0.689 | 4111 / 4127 | 0+2=2 | ×1.0000 | 0 | 0 | 0 | NO |
| 2 | B | 3097 | 806.9 | 7569 | 1.015 | 1.026 / 0.725 | 4163 / 4208 | 0+5=5 | ×1.0000 | 0 | 0 | 17 | NO |
| 3 | B | 927.7 | 2802 | 18481 | 1.000 | 1.000 / 0.792 | 4096 / 4096 | 0+0=0 | ×1.0000 | 0 | 0 | 13 | NO |
| 4 | A | 1275 | 2310 | 14483 | 1.000 | 1.000 / 0.813 | 4096 / 4096 | 0+0=0 | ×1.0000 | 0 | 0 | 1 | NO |

A mean 2826 (spread 3102), B mean 2012 (spread 2169) → A/B = 1.404; all legs: min 927.7 max 4377 spread 142.6 %.
A = B (same binary): the spread IS the venue's noise band for this row: **142.6 %**.
- INVALID leg 1/A: `engagement_shim` — Δ(ipc_ops_read 0 + ipc_ops_write 0) + gate routes 0 vs instrument ops 262672; sessions total 0, admission refusals Δ0 (budget 0); kernel-lane Δtransport_ring_requests 262732
- INVALID leg 1/A: `mem_budget_green` — mem_budget_level 2 at the after-snapshot (0 = Green; budget 6144 MiB)
- INVALID leg 2/B: `engagement_shim` — Δ(ipc_ops_read 0 + ipc_ops_write 0) + gate routes 0 vs instrument ops 185840; sessions total 0, admission refusals Δ0 (budget 0); kernel-lane Δtransport_ring_requests 185910
- INVALID leg 3/B: `engagement_shim` — Δ(ipc_ops_read 0 + ipc_ops_write 0) + gate routes 0 vs instrument ops 55663; sessions total 0, admission refusals Δ0 (budget 0); kernel-lane Δtransport_ring_requests 55723
- INVALID leg 4/A: `engagement_shim` — Δ(ipc_ops_read 0 + ipc_ops_write 0) + gate routes 0 vs instrument ops 76518; sessions total 0, admission refusals Δ0 (budget 0); kernel-lane Δtransport_ring_requests 76578
