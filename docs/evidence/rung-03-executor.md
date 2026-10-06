# Rung 3 (executor) — `SqzIo`, our own `std.Io` implementation

Status: landed 2026-10-06 on `dev`. Spec: `docs/porting/exec.md` §3 (module layout §3.13);
decisions 0001 (the executor is an `Io` implementation), 0003 §D (Q-A lane names, Q-B per-lane
timers + `POLL_ADD`, Q-C derived core reservation, Q-D `Batch` fails loud); 0002 ledger rows 62–70.

## What landed (`src/exec/`, root `exec.zig`)

| Module | Portable | Contents |
|---|---|---|
| `task_state.zig` (+ `_model.zig`) | yes | the delivery word `IDLE/SCHEDULED/RUNNING/NOTIFIED/COMPLETE`, laws 1–2; the three loom models + two weakenings |
| `timer_heap.zig` | yes | per-lane heap with INDEXED O(log n) cancel (0002 row 64), `Interval` deadline arithmetic (row 69) |
| `placement.zig` | yes (probe Linux) | topology parsers (`cpulist`, `isolcpus=`/`nohz_full=`), the derived reservation (row 65), `scopedAffinityCpus`, `Placement` (lane_of_cpu, node groups, cursors, dead-lane walk) |
| `cpu_class.zig`, `stats.zig` | yes | `daemon_cpu_ns_by_class` with the monotone retired ledger; every `exec_*` gauge, tripwire and the JSON |
| `fiber.zig` | Linux | mmap'd stacks with a guard page (row 62), the derived per-class size (512 KiB floor ×2 / ×8), the trampoline over `Io.fiber.contextSwitch`, the packed cancel word, canary HWM |
| `futex_table.zig` (+ `_model.zig`) | Linux / model portable | 256 buckets, fiber waiters, the raw-waiter Dekker |
| `lane.zig` | Linux | `Lane` = OS thread + fiber scheduler + `ParkedRing`; the pass in the model-checked order (reap → DISARM → scan → timers → run → ONE park); inbox; 2 s tick, 100 ms backstop while owed; tick rescue disarms the coalescer; affinity; names `sqz-fuse-ln<N>`/`sqz-fuse-ur<qid>`/`sqz-meta<i>`/`sqz-lease`/`sqz-jrnl<N>` |
| `group.zig` (+ `_model.zig`) | Linux / model portable | `Io.Group` over fibers: one-RMW join, members found by their cancel word (no list, no allocation), THE cancel request dispatch |
| `blocking.zig` | Linux | `sqz-blk<N>` pool (`cpus × 16`, floor 8; one idle worker or a spawn under the cap; 10 s reap through `sqz-timer`), `sqz-lease-io` dedicated worker, `runBlocking` parks the fiber |
| `fdwatch.zig` | Linux | `readable(fd)` = `POLL_ADD` one-shot on the lane ring; the double-await tripwire; `poll(2)` off a lane |
| `timer_thread.zig` | Linux | ONE `sqz-timer`: heap + futex condvar + cancel-safe tombstones (the fire stores under the lock) |
| `sqz_io.zig` | Linux | the vtable: `Io.Threaded` embedded first and delegated for every blocking dir/file/process/net op; ours for tasks, groups, cancel protection, futex, `sleep`/`now`, `operate` (READV/WRITEV on the lane ring), `Batch` (Q-D); `spawnOn/spawnHome/spawnMeta/spawnLease/spawnUncancelable/runBlocking/readable`; `run` (main is a ring-parked lane); `testInstance` |

Wiring: `src/squeezefs.zig` `pub const exec = @import("exec/exec.zig");` with the Linux gating inside
`exec.zig`. Knob rows added: `SQUEEZEFS_FUSE_PIN_SCOPE`, `SQUEEZEFS_NUMA`, `SQUEEZEFS_FUSE_SAME_LANE_DISPATCH`,
`SQUEEZEFS_TEST_EXEC_DROP_WAKE`. `mem_budget` component `exec_fiber_stacks` (gauge = mapped stack bytes,
non-sheddable).

## The two-executor test law (§3.10)

Every `src/sync/*` test body is a `fn (io: Io) !void` run by `sync/testing.zig`'s `underBoth` under
`std.testing.io` (Threaded) and under `SqzIo.testInstance()` (2 meta lanes) on Linux. 88 sync tests,
all green under both; three bodies were added for fiber-specific shapes (a fiber park woken by a raw
thread's token, fibers on both lanes + raw threads contending for one rwlock, a fiber producer/consumer
pair over a bounded mpsc). No primitive code changed.

## Model checks (explorer, default limits)

| Model | States (holds) | Weakening | States to failure |
|---|---|---|---|
| `task_state` wake racing poll | 7 | `end_poll_pending` as `store(IDLE)` (the mid-poll NOTIFIED lost) | `FinalViolated` after 6 |
| `task_state` concurrent wakes | 4 | `wake` as load + store (both see IDLE) | `InvariantViolated` (queued twice) after 7 |
| `task_state` wake racing completion | 5 | — (the model's assertion is the no-op) | — |
| `futex_table` (fiber + raw waiter + waker) | 174 | raw count incremented AFTER the kernel wait | `InvariantViolated` (raw waiter sleeps forever) after 11 |
| `group` (2 members + awaiter) | 17 | the awaiter's RMW split into load + store | `InvariantViolated` (lost wake) after 10 |
| `wake_core` (2 producers + lane) | 53 | disarm AFTER the scan | `InvariantViolated` (lost push) after 28 |

The reference's loom models recorded no weakening for `exec_core`; the two above are the RMW splits
that break each law.

## Measured (the Linux venue, debug build, 8 vCPU aarch64 — scoping only)

- 2,000 spawns from 4 foreign threads onto one 64-slot lane: all complete in **≈ 205 ms**, `tick_rescues = 0`
  (lane test `pluggable_park_never_loses_a_wake`); per-recycle canary scan ≈ 10 µs at 1 MiB.
- `SQUEEZEFS_TEST_EXEC_DROP_WAKE`: a dropped eventfd write is healed by the 2 s tick, counted exactly once,
  and the lane is responsive within the tick afterwards (the rescue disarms the coalescer).
- Blocking burst: 4 × 100 ms finish in < 250 ms past one idle worker; a 20 ms reap exits the worker within
  2 s through `sqz-timer`.
- Fiber sleep ≥ requested on the lane heap (`exec_timer_arms` +1 per sleep); raw sleep through `sqz-timer`.

## Deviations and owed items

- Lane-local timers remove by index (no tombstones) — 0002 row 64 records why.
- `Io.Batch.awaitAsync` on a lane aborts (its error set admits no refusal) — row 70; `awaitConcurrent`
  refuses.
- ~~Journal-class lanes (`sqz-jrnl<N>`) have their names, stack class and tests here; the cutover of
  `meta/journal_lane.zig` onto `Lane{.class = .journal}` (0002 row 6) is the rung-3 transport agent's
  follow-up~~ — landed 2026-10-06 (`rung-02b` evidence, "ONE thread per volume"). The lane gained
  the `Resident` hook (a CQE sink below the fiber tag space + the thread's start/exit edges), a
  `ring_entries` override, `enqueueReporting` (the producer's wake-cost mirror) and `enters()`; the
  fiber trampoline zeroes the link register so a stack walk (a panic trace, the test allocator's
  capture) ends at the fiber's first frame. The `fuse_ur` worker's `poll_budget = 1` remains the
  transport agent's.
- The `main` CLI still runs on `Io.Threaded`; `SqzIo.run` is tested and waits for the FUSE session.
