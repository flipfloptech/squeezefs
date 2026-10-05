# 0001 — The executor is our own `std.Io` implementation; primitives carry the charter's laws

Status: accepted 2026-10-05. Scope: charter §9 (concurrency model and executor).

## Context

Charter §9 forbids a third-party async runtime and requires a first-party, waker-based
executor with specific properties: per-core FUSE handler lanes; a small metadata pool
(`sqz-meta`); one journal-lane OS thread per writable volume that owns the volume's io_uring and
PARKS IN IT; a blocking pool; thread-per-connection wires; one timer thread; one fd-watch thread;
async mutex/rwlock with **no ownership assignment to sleeping waiters**, a **2 s ticked backstop**
on every unbounded park (`lock_ticked_reregisters`), **bounded barging**, write-preferring;
channels whose waiters register under the senders' lock; notify with permit + epoch; semaphore
under the barging law; single-flight; once-cell with cancelled-leader re-election; a task set
with genuine cancel-on-drop; stop latches; cancellation of a committer designed OUT.

Zig 0.17's standard library ships `std.Io`: a VTABLE interface (`async/concurrent/await/cancel`,
`Group`, `futexWait/futexWake`, clocks, sleep, files, sockets) against which every std I/O and
synchronisation primitive (`Io.Mutex`, `Condition`, `Event`, `Queue`, `RwLock`, `Semaphore`,
`Dir`, `File`, `net`) is written, plus two implementations: `Io.Threaded` (thread pool) and
`Io.Evented` (fiber-based; io_uring on Linux). The standard library is first-party by the
charter's meaning (the ban is on third-party runtimes), but `Io.Evented`'s scheduling does not
satisfy §9's laws (no lane homing, no ring-owning lane thread, no ticked backstop, fibers rather
than wakers), and swapping scheduling policy inside std is not an option.

## Decision

1. **Interface:** product code that needs to wait, spawn, or do file/socket I/O outside the hot
   paths is written against `std.Io` (an `io: Io` parameter, never a global). Tests run under
   `std.testing.io` (`Io.Threaded`) today; the Linux venue runs the same tests.
2. **Executor:** `src/exec/` will provide our own `Io` implementation (`SqzIo`) satisfying §9:
   per-core handler lanes keyed by CPU/NUMA node, the `sqz-meta` pool, journal-lane threads that
   park in their own ring (`io/uring_fs.OwnedRing` already is that ring), the blocking pool
   (`cap = cpus × 16`, floor 8), the timer thread (binary heap + cancel-safe tombstones), the
   fd-watch thread, waker-based task futures. It lands with rung 3 (the FUSE transport needs the
   lanes); rung 2b's device-bound pieces (the journal lane) use `OwnedRing` directly because a lane
   thread IS an OS thread owning a ring — no `Io` needed for its own parking.
3. **Primitives:** `src/sync/` implements the charter's primitives over the `Io` futex ops so
   they work under ANY `Io` (Threaded today, `SqzIo` later): `Mutex`/`RwLock` (release flips free
   and wakes the queue front; woken waiters re-contend; a fresh attempt takes a free lock only when
   nobody is queued; a queued waiter's re-contend barges; write-preferring; every unbounded park is
   a 2 s ticked wait that re-registers and counts `lock_ticked_reregisters`), `Notify` (permit +
   epoch; synchronous enable-then-recheck), `Semaphore` (barging law), `SingleFlight`, `OnceCell`
   (cancelled-leader re-election), `oneshot`/`mpsc`/`watch` channels (waiters register under the
   lock senders mutate under), `TaskSet` (cancel-on-drop over `Io.Group`). The lock-free wake
   protocols are model-checked with `src/model/explorer.zig`.
4. **Hot paths never take an `Io`:** per-op code uses the lock-free structures, `core/clock.zig`,
   and the rings directly. `std.Io.Mutex` is admissible only in the sanctioned
   metadata-transaction class (the conveyor queue), as the charter allows for `std.Thread.Mutex`.
5. **Cancellation:** committers are never cancelled (`lockUncancelable`, detached passes); the
   `Io` cancel protocol is used only where the charter names cancel-on-drop (task sets).

## Consequences

- No third-party dependency; `std.Io` is the one interface, so a primitive written once serves
  tests and the product.
- `SqzIo` must implement the full vtable; unsupported operations fail loud at init, never
  silently degrade (charter §1.8).
- Zig 0.17 has no `std.Thread.Mutex`; where the reference used a plain mutex in the sanctioned
  class, the port uses `std.Io.Mutex` with the `Io` captured at construction.
