# Rung 10 — the journal lane's off-lane ticket waits park the fiber (lever (a)'s follow-up)

**What changed.** A journal `Ticket.wait` from a fiber on ANOTHER lane — a handler fiber's `fsync`
(`kv_fs.fsync` → `KvBackend.fsync` → `conveyor.barrier` → `JournalLane.requestBarrier` → `Ticket.wait`),
a structural entry from a space retry off the lane — parked the CALLER THREAD on the cell's raw futex,
so the handler lane sat out the barrier round trip and every fiber homed there waited with it
(`rung-10-fiber-device-waits.md` named this follow-up). Now the lane carries the device's wait route:
`JournalLane.routeWaits(io)` installs `SqzIo`'s futex face once (the backend, right after the spawn,
before the first request — asserted on the cell pool being whole), the off-lane waiter parks through
`Completion.waitIo` (a fiber alone in the executor's futex table, a raw thread — the checkpoint thread's
structural entries, tests — on the counted kernel wait), and `deliver` wakes through `completeIo`. The
lane's OWN fibers (the pass fiber, the stage-B fiber inside a hook) keep their fiber park answered by the
lane's reap, exactly as before; a bare lane keeps the raw futex on both sides. The handoff is the futex
table's modelled protocol (`exec/futex_table_model.zig`: the cell's release CAS is the token store, then
the wake), so no new model and no lock-free core touched. Decision 0001's amendment carries the
extension; `sync/mpmc.zig` gained `len()` (the pool-whole assertion).

**The red test** (`meta.journal_lane`: "a fiber on ANOTHER lane waiting on a journal ticket parks only
itself …"). A fiber on a meta lane of `SqzIo.testInstance()` requests a barrier while stage B is held at
the top of its poll (`hold_before_scan`: the request sits in the inbound queue, no barrier can start,
nothing sleeps); a second fiber homed on the same meta lane must run meanwhile. Before: "the fiber
waiting on the journal ticket holds its lane thread: the bystander never ran" (30 s diagnostic bound).
After: it runs; the wait counts one `journal_lane_wait_fiber_parks`, the test thread's warm-up wait one
`journal_lane_wait_raw_parks`, the release acks the fsync through the route (one coalesced barrier).

**Observability** (`.stats`, `journal_lane` family): `journal_lane_wait_fiber_parks` /
`journal_lane_wait_raw_parks`. On a serving mount the fiber parks ≡ the fsync barriers (B's rows:
730 k parks / 730 k barriers at j1); the raw parks are the checkpoint thread's structural entries
(8–105 on the 4k rows, ~1 k on the 1 MiB row).

**A/B** (`~/tmp/fsync-ab/session.sh`: the whole venue held per session as `quiet.sh` does, `ramnvme.sh`
RAM namespaces, `perf_mount.sh --ram --fresh`, 2 MDT + 2 OST; fio io_uring O_DIRECT, QD1, 20 s rows on
laid-down files; A = dev `464025a` from the main tree, B = `c8541b9` on it (the same change rebased onto `732a3da`, a docs-only
commit, is `654fd83`); legs A B B A then B A A B,
four per side; every session's `fsck --offline` clean; this workstation, `7.2.8-7-omarchy-sqz`).
Latencies are means over the legs (µs); the spread is (max − min) ÷ mean of the IOPS.

| row | A legs (IOPS) | B legs | B ÷ A | spread A / B | write p50 / p99 A → B | fsync p50 / p99 A → B | daemon cores A → B |
|---|---|---|---|---|---|---|---|
| 4k write, fsync each, 1 job | 13.6, 13.9, 13.9, 14.8 k | 17.3, 15.7, 17.2, 13.1 k | **1.13** | 9 % / 26 % | 18.2 / 48.9 → 15.3 / 43.8 | 43.3 / 103 → 38.9 / 99 | 0.86 → 0.90 |
| 4k write, fsync each, 4 jobs | 42.3, 37.7, 39.6, 39.5 k | 38.0, 36.2, 38.4, 41.9 k | 0.97 | 12 % / 15 % | 28.1 / 53.1 → 28.3 / 50.1 | 61.8 / 147 → 65.0 / 149 | 3.24 → 3.31 |
| 4k write, fsync each, 16 jobs | 89.4, 89.5, 89.1, 89.9 k | 90.8, 90.0, 90.5, 90.3 k | 1.01 | 1 % / 1 % | 43.2 / **115.7 → 41.7 / 81.9** | 102 / 334 → 106 / **301** | 9.34 → 9.71 |
| 1 MiB seq write, fsync per 32, 1 job (MiB/s) | 9020, 6051, 7371, 7246 | 5922, 8192, 5577, 8189 | 0.94 | 40 % / 38 % | 90 / 205 → 93 / 210 | 294 / 799 → 306 / 780 | 0.46 → 0.41 |

The 16-job row is the one that exercises the law and the one with a tight spread: the same IOPS at a
write p99 of 82 µs against 116 µs and an fsync p99 of 301 against 334 — sixteen jobs on thirty-two CPUs
share lanes often enough that a write behind another job's fsync shows at p99, and it no longer waits
the barrier out. Its cost is the lane pass the woken fiber now rides (+0.26 cores on the handler lanes,
+0.10 on the journal lanes: 9.34 → 9.71 of the daemon's cores, +4 %). The 1-job row reads 1.13× on
three legs at 15.7–17.3 k and one at 13.1 k (A-like latencies) — placement-sensitive, as every QD1 row on
this box is (`rung-10-perf-baseline-workstation.md`); the 4-job row and the 1 MiB row sit inside their
own spreads (12–15 % and 38–40 %, both sides).

**What binds now.** Every `fsync` on this 2-MDT set pays TWO serial barrier round trips:
`MetaSet.fsync` loops every member (`for (self.volumes) |be| try be.fsync()`), so the barrier count is
2.1 × the fsync count on every row (730 k barriers for 345 k fsyncs at j1). The ino's parked stamp
commits to ONE home volume; barriering only the volumes the fsync's drain touched (or issuing the
members' barriers concurrently and joining them, charter §6.4's "independent legs joined") halves the
fsync latency on this shape and is the next lever here. At 16 jobs the journal lanes are at 1.1 cores
and the handler lanes at 5.2: the per-fsync cost is the FUSE round trip plus the two barriers, not a
thread held.

**The other raw waits of the class** (`grep -n "futex.wait(" src/meta src/data src/fuse`,
`Completion.wait` callers), none on the steady-state fsync path, left as owed:
- `io/uring_fs.Pool` cells (`waitCell`, `WriteCompletion.wait`): a node-cache MISS from a handler fiber
  or from the journal lane's pass/stage-B fiber (`device_loader.readExtent` → `pool.readAt`) parks the
  thread until the io pool's CQE; the checkpoint thread's node images, ledger slots and bitmap pages are
  its own raw thread. The fsync rows run warm (no misses), so the win is the cold-cache lookup; the Pool
  needs the same `routeWaits(io)` (the backend has the `Io` at open) — owed.
- the transport's sideband cells (`fuse/transport/session.zig`: `notifyNbAwaited`, the classical-reply
  fallback): the IPC host's `sqz-ipc-svc` thread (raw) and a rare fallback; not in the lane class.
- `fuse/transport/worker.zig` `started` / `transport_done`, the arm barrier's `waitTimeout`, the slot
  pools' `freed` ticks: the transport's own raw threads at arm and teardown — not fiber waits.
- `io/nvme_dev.zig`'s watermark epoch: routed by lever (a).

**Not measured: `mdtest`.** `mpirun -np 32 mdtest -n 10000 -u` hangs dev `464025a` AND this branch
alike within 40–140 s of the create phase (both daemons killed by their supervisor after 30 s
unresponsive; census identical: 31 handler lanes idle in `io_cqring_wait`, 10–14 `sqz-fuse-ur` workers in
`__futex_wait`, both journal lanes idle, nothing running); at `-np 8` dev crawls at a few creates per
second with `lock_ticked_reregisters` 3073 after 20 min and `fuse_op_watchdog_overdue` 3 — the
`handler_entry` stall signature of `rung-10-symmetric-defects.md` §2.2 (a lost wake self-healing on the
2 s tick), on a single host this time. A pre-existing, load-dependent hang on `dev` (charter §1.1); not
this lever's, reported to the orchestrator.

**Suites** (`scripts/venue test`): journal_lane 61/61, sync.completion 43/43, sync.mpmc 47/47, exec
138/138, backend 73/75 (2 skipped), kv_fs 91/91, fsync 66/79 (13 skipped), e2e 109/111 (2 skipped); the
smoke rows `sync -f` 42/42 and the KILL rows (THE KILL MATRIX, THE ORDERED LAW'S KILL ROW) 51/52 (1
skipped) green. No full gate (the train gates and lands).
