# Rung 10 — fiber-aware device waits (lever (a) of `rung-10-seqwrite.md`)

**What changed.** A data-device request (`nvme_dev.Device.writeBlock` / `readBlock` / `readInto` /
`flush` / `discardRange`) parked the CALLER THREAD on the request cell's raw futex (decision 0001 item
2), so a handler fiber held its lane thread for the whole DMA and every fiber homed there waited with
it. Now a device carries a wait route: `Device.routeWaits(io)` installs `SqzIo`'s futex face once,
before the first request (the data plane does it at volume open), and both sides of every cell use it —
the waiter through `Io.futexWaitUncancelable` (`sync.Completion.waitIo`: a fiber links into the
executor's futex table and its lane thread runs the other fibers; a raw thread takes the counted kernel
wait), the completing lane through `Io.futexWake` (`Completion.completeIo`: the bucket's fibers, then
the kernel wake iff a raw waiter is counted). The write watermark's barrier drain takes the same route.
A bare device (tests, offline tools) keeps the raw futex on both sides. The handoff IS the futex table's
modelled protocol (`exec/futex_table_model.zig`: the cell's release CAS is the token store, then the
wake), so no new model; the lock-free cores are untouched. Decision 0001 carries the amendment (owner
ruling 2026-10-18); `docs/porting/nvme_dev.md` §1.7 and §7 the port note and the new rows.

**The red test** (`io.nvme_dev`: "a fiber waiting on a device write parks only itself …"). A fiber on a
meta lane writes while the lane worker is held before its park (`Seams.hold_before_park`: the SQE is
pumped, never submitted — the DMA cannot complete and nothing sleeps); a second fiber homed on the same
lane must run meanwhile. Before: "the fiber waiting on the DMA holds its lane thread: the bystander
never ran". After: it runs, the wait counts one `dev_wait_fiber_parks`, `dev_dma_inflight` reads 1
while the DMA is held and 0 after, the write lands byte-exact, and a raw thread's wait through the same
route counts one `dev_wait_raw_parks`. `sync.completion`'s routed face is pinned under both executors
(a raw waiter and a task waiter completed from a raw thread).

**Observability** (`.stats`, `nvme_dev` family): `dev_dma_inflight` (requests between the lane-queue
push and the completion publish, every class and device), `dev_wait_fiber_parks` / `dev_wait_raw_parks`.
On a serving mount every handler wait is a fiber park (B's rows: raw parks 0, fiber parks ≡ fills). The
in-flight bound is the callers' admission — the write pipeline's depth target (64 MiB here, 0 admission
waits), whose derivation and tie tests are `write_pipeline.zig`'s; nothing new is derived.

**In-flight DMAs, mid-row** (1M seq write QD16×4, `.stats` sampled every 100 ms over the 20 s row;
`device_lock_waiters` = callers parked on write-class completions, on both builds):

| | min | p50 | mean | p90 | max |
|---|---|---|---|---|---|
| A (dev `887c334`) | 0 | 3 | 3.5 | 6 | 11 |
| B (this branch) — `device_lock_waiters` / `dev_dma_inflight` | 0 / 0 | 8 / 7 | 10.3 / 9.6 | 22 / 21 | 50 / 50 |

A's ≈ 3.5 is the lanes (`rung-10-seqwrite.md`'s ≈ 4.5 on kamrui's 11 lanes); B's is bounded by fio's
64 outstanding 1 MiB writes and the governor's 64 MiB target, never by a thread.

**A/B** (`scripts/perf/quiet.sh ram-sqz`, the whole venue held per session, legs A B B A over the full
matrix, 20 s rows, 4 × 3 GiB lay-down each; A = dev `887c334`, B = `fc1f138` on it; dist builds; this
workstation — AMD Ryzen AI MAX+ PRO 395, 109 GiB, `7.2.8-7-omarchy-sqz`; every session's `fsck
--offline` clean). Mean B ÷ mean A, both legs' values; the pair spread is (max − min) ÷ mean.

| row | A legs | B legs | B ÷ A | pair spread A / B | daemon cores A → B | host cores A → B |
|---|---|---|---|---|---|---|
| 4k randread QD1×1 (IOPS) | 113.0 k, 78.3 k | 98.6 k, 87.1 k | 0.97 | 36 % / 12 % | 0.61 → 0.61 | 1.1 → 1.1 |
| 4k randread QD32×1 | 300.5 k, 302.5 k | 298.8 k, 247.5 k | 0.91 | 0.7 % / 19 % | 1.02 → 1.02 | 1.8 → 2.0 |
| 4k randread QD32×4 | 800.7 k, 806.1 k | 730.7 k, 749.5 k | 0.92 | 0.7 % / 2.5 % | 4.11 → 4.02 | 7.0 → 7.1 |
| 4k randwrite QD1×1 | 46.0 k, 45.4 k | 40.3 k, 39.0 k | 0.87 | 1.3 % / 3.4 % | 0.89 → 0.84 | 1.1 → 1.0 |
| 4k randwrite QD32×1 | 220.8 k, 220.0 k | 225.2 k, 234.3 k | 1.04 | 0.4 % / 4.0 % | 1.97 → 2.01 | 2.7 → 3.0 |
| 4k randwrite QD32×4 | 391.2 k, 386.5 k | 399.2 k, 401.6 k | 1.03 | 1.2 % / 0.6 % | 7.37 → 7.41 | 9.1 → 9.2 |
| 64k randread QD16×4 | 508.9 k, 505.4 k | 522.4 k, 528.6 k | 1.04 | 0.7 % / 1.2 % | 3.84 → 3.79 | 9.9 → 10.1 |
| 64k randwrite QD16×4 | 296.9 k, 296.7 k | 299.8 k, 300.6 k | 1.01 | 0.1 % / 0.3 % | 7.06 → 7.09 | 10.5 → 10.7 |
| 1M seq read QD16×1 (MiB/s) | 42144, 41948 | 42400, 42155 | 1.01 | 0.5 % / 0.6 % | 1.40 → 1.39 | 9.5 → 9.4 |
| 1M seq read QD16×4 | 66799, 67106 | 67296, 66092 | 1.00 | 0.5 % / 1.8 % | 3.30 → 3.30 | 24.1 → 24.0 |
| **1M seq write QD16×1** | 5758, 5805 | 10433, 10401 | **1.80** | 0.8 % / 0.3 % | 1.31 → 2.94 | 2.2 → 4.9 |
| **1M seq write QD16×4** | 14847, 14880 | 18696, 18654 | **1.26** | 0.2 % / 0.2 % | 5.44 → 9.40 | 9.1 → 16.6 |
| 4M seq write QD4×4 | 14683, 14476 | 18960, 13611 ⚠ | 1.12 (1.29 on the clean pair) | 1.4 % / 33 % | 4.9 → 8.6 / 5.5 | 8.3 → 15.3 / 21.6 |

Latency (p50/p99 µs, first legs): 1M×1 2605/5931 → 1450/2867; 1M×4 3850/12648 → 3064/9110; 4M×4
3818/13042 → 2933/7832; the 4k rows within ±1 µs at p50. ⚠ B-r2's 4M row ran with 21.6 host cores
busy at 5.45 daemon cores (B-r1: 15.3 at 8.63) — a foreign load on the box during that row, not the
daemon's; the clean pair is +29 %.

**The small-block rows.** Four of them read 3–13 % lower in B's two full-matrix legs, but they do not
exercise the change: the 4k random reads are the transport's zero-copy direct leg (`READ_FIXED(device →
slot)` on the FUSE queue ring — `dev_fills` 1–2 per row, `dev_wait_*` 2), and the 4k random writes are
absorbed by the write side (92–94 device fills per 20 s row). Controls: B′ — this branch with the route
uninstalled (`dev.routeWaits` commented out), the same binary otherwise — measured 112.8 k / 360 k /
851 k / 48.9 k on those rows (at or above A), and a later B session (route on) 98.2 k / 310 k / 841 k /
44.5 k. Then four more legs of the small rows alone (B A A B), so each side has four:

| row | A legs (IOPS) | B legs | B ÷ A | spread A / B |
|---|---|---|---|---|
| 4k randread QD1×1 | 113.0, 78.3, 97.4, 79.7 k | 98.6, 87.1, 79.1, 109.0 k | 1.01 | 38 % / 32 % |
| 4k randread QD32×1 | 300.5, 302.5, 307.8, 261.6 k | 298.8, 247.5, 251.4, 338.6 k | 0.97 | 16 % / 32 % |
| 4k randread QD32×4 | 800.7, 806.1, 806.5, 714.2 k | 730.7, 749.5, 754.0, 828.1 k | 0.98 | 12 % / 13 % |
| 4k randwrite QD1×1 | 46.0, 45.4, 44.1, 35.6 k | 40.3, 39.0, 36.9, 55.4 k | 1.00 | 24 % / 43 % |
| 4k randwrite QD32×1 | 220.8, 220.0, 223.8, 214.9 k | 225.2, 234.3, 234.6, 213.5 k | 1.03 | 4 % / 9 % |
| 4k randwrite QD32×4 | 391.2, 386.5, 391.6, 398.0 k | 399.2, 401.6, 397.8, 392.5 k | 1.02 | 3 % / 2 % |
| 64k randread QD16×4 | 508.9, 505.4, 509.2, 504.4 k | 522.4, 528.6, 522.8, 516.4 k | 1.03 | 1 % / 2 % |
| 64k randwrite QD16×4 | 296.9, 296.7, 303.3, 301.4 k | 299.8, 300.6, 298.2, 297.2 k | 1.00 | 2 % / 1 % |

The 4k rows swing 12–43 % across sessions of ONE binary on this box (the QD1 rows most: 36–113 k
for 4k randwrite QD1 over eight A/B sessions), so every small-block difference sits inside its own
noise; the rows with a tight spread (QD32×4 writes, the 64k rows) are within ±3 %.

**What binds now.** The 1M×4 row's 18.7 GB/s is the two copies (`iou-wrk` 4.9 cores = the
`WRITE_FIXED` extraction of every big write into the bounce slab, then the RAM device's copy; charter
§6.2 item 1 wants zero daemon copies) — lever (b). The 1M×1 row doubled because one job's 16 writes now
overlap on the device instead of serializing behind one lane thread. The 4M×4 row is also bounded by
FUSE's 1 MiB `max_write`: each 4 MiB request is four sequential 1 MiB round trips, so QD4×4 is 16
concurrent DMAs where 1M×4 is 64.

**Suites** (`scripts/venue test`): nvme_dev 75/75, nvme_derive 59/59, exec 137/137, journal_lane 60/60,
device_overlay 61/61, write_admit 77/77, data.plane 49/49, sync.completion, sync.futex — all green; the
smoke rows "the data plane WITH a cache path" and "THE KILL MATRIX" green; no full gate (the
orchestrator gates and lands). The A/B above is against dev `887c334`; lever (b) (`7ab7e68`, zero-copy
big writes) landed meanwhile and this branch is rebased onto it (the suites above re-run green on the
combination) — the two levers' joint numbers are the orchestrator's measurement to make.
