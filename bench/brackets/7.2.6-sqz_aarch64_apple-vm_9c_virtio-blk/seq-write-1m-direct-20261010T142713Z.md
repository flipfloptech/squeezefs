### seq-write-1m-direct — SCOPING-INVALID

instrument `fio-3.36 libaio write bs=1m direct=1 iodepth=32 numjobs=4 size=512m/job` · substrate `nvmet-tcp on 127.0.0.1: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (ce0edf4f004d, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261010T142713Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | — | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 0 | — | — | — | — / — | 1048029 / — | 0+3864=3864 | — | 0 | 0 | 4 | NO |
| 2 | B | 0 | — | — | — | — / — | 1048392 / — | 0+4316=4316 | — | 0 | 0 | 23 | NO |
| 3 | B | 0 | — | — | — | — / — | 1048418 / — | 0+5024=5024 | — | 0 | 0 | 45 | NO |
| 4 | A | 0 | — | — | — | — / — | 1047424 / — | 0+3895=3895 | — | 0 | 0 | 11 | NO |

A mean 0.000 (spread 0), B mean 0.000 (spread 0); all legs: min 0 max 0 spread 0.0 %.
A = B (same binary): the spread IS the venue's noise band for this row: **0.0 %**.
- INVALID leg 1/A: `instrument_ok` — instrument rc 4, 106 I/O errors (fio io_u errors (see measure.log)); daemon log: 106 `answered EAGAIN` line(s)
- INVALID leg 1/A: `mem_budget_green` — mem_budget_level 2 at the after-snapshot (0 = Green; budget 7168 MiB; top components exec_fiber_stacks 717 MiB, transport_zc_bounce 288 MiB, transport_kmbuf_region 288 MiB, block_alloc_table 0 MiB)
- INVALID leg 2/B: `instrument_ok` — instrument rc 4, 128 I/O errors (fio io_u errors (see measure.log)); daemon log: 128 `answered EAGAIN` line(s)
- INVALID leg 2/B: `mem_budget_green` — mem_budget_level 2 at the after-snapshot (0 = Green; budget 7168 MiB; top components exec_fiber_stacks 718 MiB, transport_zc_bounce 288 MiB, transport_kmbuf_region 288 MiB, block_alloc_table 0 MiB)
- INVALID leg 3/B: `instrument_ok` — instrument rc 4, 126 I/O errors (fio io_u errors (see measure.log)); daemon log: 126 `answered EAGAIN` line(s)
- INVALID leg 3/B: `mem_budget_green` — mem_budget_level 2 at the after-snapshot (0 = Green; budget 7168 MiB; top components exec_fiber_stacks 725 MiB, transport_zc_bounce 288 MiB, transport_kmbuf_region 288 MiB, block_alloc_table 0 MiB)
- INVALID leg 4/A: `instrument_ok` — instrument rc 4, 128 I/O errors (fio io_u errors (see measure.log)); daemon log: 128 `answered EAGAIN` line(s)
- INVALID leg 4/A: `mem_budget_green` — mem_budget_level 2 at the after-snapshot (0 = Green; budget 7168 MiB; top components exec_fiber_stacks 718 MiB, transport_zc_bounce 288 MiB, transport_kmbuf_region 288 MiB, block_alloc_table 0 MiB)
