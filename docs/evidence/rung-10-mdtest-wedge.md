# Rung 10 — the mdtest wedge: the drain worker parked behind its own lane's fibers (2026-10-19)

**The finding** (`rung-10-fsync-route.md`): `mpirun -np 32 mdtest -n 10000 -u` wedged `dev` (`464025a`,
`2edfd47`) within 40–140 s of the create phase, the supervisor killing the daemon after 30 s; at
`-np 8` a crawl with `lock_ticked_reregisters` 3073 after 20 min — the `handler_entry` signature of
`rung-10-symmetric-defects.md` §2.2, on ONE host.

**The census** (`~/tmp/mdwedge/repro.sh`: the venue held as `quiet.sh` holds it, `ramnvme.sh` RAM
namespaces, 2 MDT + 2 OST — a symmetric set —, the pristine `2edfd47` dist binary mounted WITHOUT
`--supervise` so the wedge stays up; `-np 32`). A crawl from the first second: `fuse_ops` 1,194 at
t + 91 s, 2,336 at t + 230 s (≈ 10 ops/s), `.stats` unreadable for 10 s stretches. Threads: 22
`sqz-fuse-ur` workers in `io_cqring_wait`, **10 in `__futex_wait`**, every handler and journal lane
idle, nothing running. The ten user stacks (SIGQUIT, one thread at a time) are ONE stack: `tick.park`
← `rwlock.lockUncancelable` ← `kv_fs_stripes.verdictOf` ← `mapOf` ← `kv_fs.lookup` (`findChild`) or
`kv_fs.getattr` (`foldIfStriped`) ← `probe_lookup` / `probe_getattr` ← `lifecycle.probeMeta` ←
`worker.deliver` — the worker's TRY-ONLY probe, off a fiber, blocking on the striping prelude's
verdict-cache `sync.Mutex`. `.stats` at the capture: `lock_ticked_reregisters` **1,793** (≈ 7.5/s:
fifteen waiters ticking every 2 s), `dir_stripe_verdict_hits` 2,333 ≈ `fuse_ops` (every namespace op
takes that lock), `exec_fibers_live.fuse_ur` 12 (handler fibers parked on the driven lanes),
`channel_ticked_reregisters` 0, `exec_lane_tick_rescues` 0, `fuse_op_watchdog_overdue` 0 (every op
does complete — one tick per hop).

**The interleaving.** Since 0003 §AB.3 each drain worker DRIVES a handler lane: a namespace request's
handler is a fiber that runs only inside the worker's pass. `verdictOf` runs on every lookup, getattr,
create and unlink of a symmetric mount and takes ONE process-wide `sync.Mutex` for a RAM-only section.
Under 32 ranks its FIFO forms; the probe on worker `W` queues as a RAW waiter behind a create fiber `F`
of `W`'s own lane. The holder releases: the rwlock hands the wake token to the FRONT (`F`), the lock is
FREE, every fresh contender yields to the queue (bounded barging, law 2) — and `F` runs only when `W`
pumps its lane. `W` sleeps until its 2 s tick, barges, takes and releases (one `lock_ticked_reregisters`),
and the new front is the next fiber whose owner sleeps: two seconds per hop across the whole mount, the
hot directories' op guards held by the frozen fibers through it. The supervisor's probe is a `stat` of
the control inode — a `probe_getattr` into the same queue — so it misses for 30 s and kills.

The same shape on an `Io.Mutex` (every primitive's interior, reached by every `tryLock`) is PERMANENT,
not ticked: `FutexTable.wake(ptr, 1)` moved the first matching fiber off the bucket and, the count
spent, returned without the kernel wake — a raw waiter that owns the woken fiber's lane slept for ever
on a FREE mutex, the fiber enqueued behind it.

**The fix.**
- `exec/futex_table.zig`: the raw side is kernel-woken for the WHOLE count whenever the word's bucket
  counts a raw waiter, whatever the fibers took (a spurious raw wake re-contends; every caller loops
  on its predicate); the raw count lives in the BUCKET, so a raw park elsewhere (a journal ticket's)
  does not tax the handler lanes' wakes. Model `futex_table_model.zig` `DrivenOwnerModel` (one fiber on
  a driven lane, its owner as the raw waiter, a count-one waker; the fiber resumes only while the owner
  is awake): `expectHolds`; the weakening `.fiber_takes_the_raw_wake` — the pre-fix `remaining == 0`
  return — `expectFails` with the owner and the fiber asleep for ever.
- `fuse/handler/kv_fs_stripes.zig`: the verdict cache's lock under `node_cache.try_only` is a `tryLock`
  — refused ⇒ `WouldBlock` (`dir_stripe_verdict_try_refusals`), the lane re-asks the op: the probe's
  own demotion path. The worker never queues behind its own fibers there.
- `fuse/handler/kv_fs.zig` `forget`: the corpse judgement's inode read is try-only too — the worker runs
  every ring FORGET inline, and a demand page (or the cohort wait on a load a fiber of its own lane
  leads: the same inversion through `flight.zig`, with no tick to barge on) joins the gather unjudged
  (`fuse_forget_judgements_deferred`); the cadence's `destroyCorpses` re-judges `nlink`.
- `exec_driven_owner_queued_parks` (`exec/stats.zig`, counted at `SqzIo.futexWait`): a TIMED raw park
  by a driven lane's owner off a fiber — the class of this note. 0 on a healthy daemon; growth beside
  `lock_ticked_reregisters` names the next such site. `docs/porting/exec.md` §3.5 carries both laws.

**The red tests.** `exec.sqz_io` "a count-one mutex wake reaches a DRIVEN lane's owner parked raw behind
one of its own fibers …": a driven lane attached on a thread, its fiber parked on a held `Io.Mutex`, the
owner's own raw acquire behind it; the holder releases once the owner is counted AND asleep (its
`stat` state) and must not have to wake it by hand — on the old table the 2 s rescue fired (`39/40 … 1
failed`). `fuse.handler.kv_fs_stripes` "the drain worker's try-only probe never queues behind the
verdict cache's lock …": `probe_lookup` on a thread against a held lock, `waiters` 0, `WouldBlock`.
`fuse.handler.kv_fs` "a FORGET whose corpse judgement would park the drain worker … defers the judgement
to the gather …": the `MemStore.try_only_refusals` seam; a corpse destroyed, a live inode left.

**The oracle** (`~/tmp/mdwedge/session.sh`: the venue held once; the fixed dist binary — `9fc7ff6`,
`-dirty` by this note alone —; the same RAM set; no supervisor; `mdtest -n 10000 -u -i 1`).

| run | creates/s | stat/s | unlink/s | `lock_ticked_reregisters` | `fuse_op_watchdog_overdue` | `exec_driven_owner_queued_parks` | try-refused / verdicts |
|---|---|---|---|---|---|---|---|
| `-np 32` r1 | **43,776** | 10.47 M | 36,505 | 0 | 0 | 0 | 345,994 / 2,879,974 |
| `-np 32` r2 | **42,160** | 10.48 M | 36,915 | 0 | 0 | 0 | 343,149 / 2,879,974 |
| `-np 32` r3 | **42,395** | 10.48 M | 36,203 | 0 | 0 | 0 | 348,633 / 2,879,974 |
| `-np 8` | 59,783 | 6.42 M | 49,669 | 0 | 0 | 0 | 18,825 / 720,021 |
| `-np 64 -n 5000` | 33,382 | 4.55 M | 28,325 | 0 | 0 | 0 | 517,443 / 2,880,147 |

Every run completes (40 s wall at `-np 32`); the ≥ 32 k creates/s many-dirs bar holds at every width.
`channel_ticked_reregisters`, `exec_lane_tick_rescues`, the transport's lost-wake tripwires and
`invariant_tripwires` are 0 in all five; `fuse_forget_judgements_deferred` 0 (640 k forgets, every
corpse read a hit). The try-only verdict's cost: ≈ 12 % of the probes' verdicts are refused and demoted
to the worker's own lane (`transport_meta_inline_demotes` 346 k against 614 k serves at `-np 32`) — a
fiber on the same thread, no cross-thread hop. The same code before its commit measured 49.8 / 41.6 /
42.8 k at `-np 32`, 59.3 k at `-np 8`, 35.5 k at `-np 64`: the spread between sessions exceeds the one
within.

**Suites.** `scripts/venue test` over `futex`, `exec.`, `lifecycle`, `op_guard`, `kv_fs`, `transport`,
`e2e`, `sync.` and the new rows: 480/482 passed (2 skipped). Smoke rows in the held session: KERNEL AIO
on the plain mount, THE KILL MATRIX, A STRIPED DIRECTORY ON ONE DAEMON, THE SHARED HOT DIRECTORY ACROSS
DAEMONS — 43/43. No full gate (the train gates).

**Not changed.** The verdict lock stays one process-wide mutex over every namespace op of a symmetric
mount (RAM-only holds; a lever to measure, not a defect). The `gatherCorpse` overflow arm still drives
a destroy inline on the forgetting thread (a bounded queue's fallback).
