# Rung 10 — the journal lane's off-lane ticket waits park the fiber (lever (a)'s follow-up)

**What changed.** A journal `Ticket.wait` from a fiber on ANOTHER lane — a handler fiber's `fsync`
(`kv_fs.fsync` → `KvBackend.fsync` → `conveyor.barrier` → `requestBarrier` → `Ticket.wait`), a structural
entry off the lane — parked the CALLER THREAD on the cell's raw futex, so the handler lane sat out the
barrier round trip with every fiber homed there. Now the lane carries the device's wait route:
`JournalLane.routeWaits(io)` installs `SqzIo`'s futex face once (the backend, right after the spawn, before
the first request — asserted on the cell pool being whole, `mpmc.len()`), the off-lane waiter parks through
`Completion.waitIo` (a fiber alone in the executor's futex table; a raw thread — the checkpoint thread's
structural entries, tests — on the counted kernel wait), `deliver` wakes through `completeIo`. The lane's
OWN fibers keep their fiber park answered by the lane's reap, exactly as before; a bare lane keeps the raw
futex on both sides. The handoff is the futex table's modelled protocol (the cell's release CAS is the token
store, then the wake): no new model, no lock-free core touched. Decision 0001's amendment carries the
extension. Counted: `journal_lane_wait_fiber_parks` / `journal_lane_wait_raw_parks` (on a serving mount the
fiber parks ≡ the fsync barriers; the raw parks are the checkpoint thread's structural entries).

**The red test** (`meta.journal_lane`): a fiber on a `SqzIo.testInstance()` meta lane requests a barrier
while stage B is held at `hold_before_scan` (the request sits in the inbound queue; nothing sleeps); a second
fiber homed on the same lane must run meanwhile. Before: "the fiber waiting on the journal ticket holds its
lane thread: the bystander never ran". After: it runs, one fiber park and one raw park (the warm-up) are
counted, the release acks the fsync through the route.

**A/B** (`~/tmp/fsync-ab/session.sh`: the whole venue held per session as `quiet.sh` does, RAM NVMe
namespaces, `perf_mount.sh --ram --fresh`, 2 MDT + 2 OST; fio io_uring O_DIRECT QD1, 20 s rows on laid-down
files; A = dev `464025a`, B = `c8541b9` on it — the change that landed as `654fd83`, was reverted in
`a9f7f5d` and re-lands with the fix below; legs A B B A then B A A B; every `fsck --offline` clean; this
workstation). Latencies are means over the legs (µs); the spread is (max − min) ÷ mean of the IOPS.

| row | A legs (IOPS) | B legs | B ÷ A | spread A / B | write p50 / p99 A → B | fsync p50 / p99 A → B | daemon cores |
|---|---|---|---|---|---|---|---|
| 4k write, fsync each, 1 job | 13.6, 13.9, 13.9, 14.8 k | 17.3, 15.7, 17.2, 13.1 k | **1.13** | 9 % / 26 % | 18.2 / 48.9 → 15.3 / 43.8 | 43.3 / 103 → 38.9 / 99 | 0.86 → 0.90 |
| 4k write, fsync each, 4 jobs | 42.3, 37.7, 39.6, 39.5 k | 38.0, 36.2, 38.4, 41.9 k | 0.97 | 12 % / 15 % | 28.1 / 53.1 → 28.3 / 50.1 | 61.8 / 147 → 65.0 / 149 | 3.24 → 3.31 |
| 4k write, fsync each, 16 jobs | 89.4, 89.5, 89.1, 89.9 k | 90.8, 90.0, 90.5, 90.3 k | 1.01 | 1 % / 1 % | 43.2 / **115.7 → 41.7 / 81.9** | 102 / 334 → 106 / **301** | 9.34 → 9.71 |
| 1 MiB seq, fsync per 32, 1 job (MiB/s) | 9020, 6051, 7371, 7246 | 5922, 8192, 5577, 8189 | 0.94 | 40 % / 38 % | 90 / 205 → 93 / 210 | 294 / 799 → 306 / 780 | 0.46 → 0.41 |

The 16-job row exercises the law and has the tight spread: the same IOPS at a write p99 of 82 µs against
116 and an fsync p99 of 301 against 334 — sixteen jobs on thirty-two CPUs share lanes often enough that a
write behind another job's fsync shows at p99, and it no longer waits the barrier out; the cost is the lane
pass the woken fiber rides (+4 % daemon cores). The 1-job row is placement-sensitive (three legs 1.2×, one
A-like), the 4-job and 1 MiB rows sit inside their spreads. Re-run once on dev `02e942b` (A B): 1 job 16.5 →
17.5 k, 4 jobs 38.3 → 38.0 k, 16 jobs 90.1 → 91.1 k at write p99 115 → 81 and fsync p99 326 → 293, the 1 MiB
row 6.6 → 6.9 GB/s. **What binds now:** every `fsync` on a 2-MDT set pays TWO serial barrier round trips —
`MetaSet.fsync` loops every member (2.1 barriers per fsync on every row) though the ino's parked stamp
commits to ONE home volume; barriering the touched volume(s) only, or joining the members' barriers (charter
§6.4), halves the fsync latency here and is the next lever.

**The wedge `654fd83` shipped, and its fix.** `sw1m-q16j1` (one job's QD16 1 MiB streaming overwrite)
wedged the daemon: 16 writes of one ino parked past `write_admitted`, lock ticks climbing, every thread asleep
but one or two handler lanes RUNNING — `eu-stack` on the repro: both inside `block_grant.WriterArm.mintBlock`
under `installRecord`. The interleaving: (1) write W1, a fiber on lane L, mints; the grant window wants a
top-up; W1 wins the single-flight latch `topup_inflight` and runs the inline carve — `blockGrant` →
`bitmap.drain` → `journalDeltas` → `controlEntryFn` → `smo_journal.commitEntry` → `smoWriteHook` →
`submitStructural` → `Ticket.wait`, which the route now PARKS (before: a raw futex blocking L's thread, so no
second minter could run on L); (2) W2, another of the 16 writes homed on L, mints, loses the latch, finds the
window empty and the sink inline, and SPINS on the latch — on the premise "a fiber of this lane cannot hold
the latch mid-carve since the carve has no park point"; (3) the journal lane lands W1's entry, `deliver` →
`completeIo` → `L.enqueue(W1)`; L's thread is inside W2's poll and never reaches its pass top, W1 never runs,
the latch never clears. The route removed the spin's premise. The fix (the branch's `fix(data)` commit): the
latch is a futex word with the device's wait route (`WriterArm.routeWaits(io)` at the holder's arm) — a lost
racer parks on it through the executor's futex face, the winner wakes the racers at its release; a bare arm
(tests) spins as before; the handshake is the futex table's protocol (token store, then wake). Pins: the
`block_grant` unit test (a winner parked mid-carve on lane 0, a racer on lane 0, a bystander on lane 0 must
run — red under the spin: "the racer spinning on the latch holds its lane thread: the bystander never ran")
and **the smoke row** `A STREAMING OVERWRITE NEVER WEDGES THE DAEMON` (a 3/4-full set, one job's QD16 1 MiB
O_DIRECT overwrite for 12 s — fio where present, else the smoke's kernel-aio writer; `block_grant_topups >
0`, `fuse_op_watchdog_overdue == 0`, `lock_ticked_reregisters == 0`, the gauges clean, an orderly unmount):
red on the unfixed tree in 15 s with the production signature (`wedge census: fuse_ops_inflight=16
overdue_now=16 lock_ticked_reregisters=24 stalest_worker_pass_ms=15067`), green after (2661 top-ups, 19
lost-racer waits with fio; 20156 and 1272 with the kernel-aio writer). Decision 0001 records the corollary:
a route reaching a path means auditing its spinners.

**The streaming rows after the fix** (`quiet.sh ram-sqz`, `MX_ROWS='sw1m-q16j1|sw1m-q16j4'`, 20 s, MiB/s):
six sessions over `a9f7f5d` — ×1 27.0, 27.3, 27.1, 26.3, 26.8 and ×4 46.4, 45.6, 45.4, 46.1, 45.6 in five
clean sessions; the sixth's ×4 row died of the PANIC below, not a wedge. Rebased onto dev `02e942b` (B A B):
×1 30.0 / 29.5 vs A 28.4 (A's earlier legs 29.0, 26.9); ×4 47.5 / 48.7 vs 48.5 (48.1, 47.7).

**A pre-existing crash the fix surfaces** (1 of 6 sessions, the ×4 row; never in the box's perf log
before): `std.Io.Mutex.unlock` → `unreachable` inside `sync.Notify.notifyWaiters` from
`device_overlay.finishStore`. `finishStore` drops the record's in-flight hold (`core.completeStore`) and only
THEN notifies `rec.inflight_change`; `awaitInflight` proceeds the instant `inflightEmpty()` reads true, the
settle retires the record, and `Registry.install` re-inits `r.inflight_change = .init` — under the notifier's
interior lock when the notifier was preempted between its `completeStore` and its `unlock`, which then finds
`.unlocked`. The record outlives its hold by one notify; the overlay owner's (notify under a pin on the
record, or a `Notify` that survives the record's reuse).

**The other raw waits of the class** (`grep -n "futex.wait(" src/meta src/data src/fuse`, `Completion.wait`
callers), none on the warm fsync path, owed: `io/uring_fs.Pool` cells (`waitCell`, `WriteCompletion.wait`) on
a node-cache MISS from a handler or lane fiber (`device_loader.readExtent`; the checkpoint thread's writes are
its own raw thread) — the Pool wants the same `routeWaits(io)`, and by the corollary an audit of
`tree.zig`'s walker yield loop first; the transport's sideband cells (`notifyNbAwaited`, the classical-reply
fallback: the IPC host's raw thread and a rare fallback); the transport's own arm/teardown waits (raw
threads, not fibers); `nvme_dev`'s watermark epoch is routed already.

**Not measured: `mdtest`.** `mpirun -np 32 mdtest -n 10000 -u` hangs dev `464025a` and this branch alike
within 40–140 s of the create phase (both killed by their supervisor after 30 s unresponsive; census: 31
handler lanes idle in `io_cqring_wait`, 10–14 `sqz-fuse-ur` workers in `__futex_wait`, nothing running); at
`-np 8` dev crawls with `lock_ticked_reregisters` 3073 after 20 min — the `handler_entry` stall signature of
`rung-10-symmetric-defects.md` §2.2, on one host. Pre-existing on `dev`, reported.

**Suites** (`scripts/venue test`, after the fix): journal_lane 61/61, sync.completion 43/43, sync.mpmc
47/47, exec 141/141, block_grant 55/57 (2 skipped), sym_planes 118/118, write_admit 78/78, block_alloc
68/68, data.plane 50/50, backend 73/75 (2 skipped), kv_fs 93/93, fsync 66/79 (13 skipped), e2e 109/111 (2
skipped); the smoke rows "the data plane" 43/43, `sync -f` 42/42, the KILL rows 51/52 (1 skipped) and the
streaming-overwrite row green. No full gate (the train gates and lands).
