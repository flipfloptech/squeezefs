# design-sqz-sync — the scheduler-free metadata-plane lock primitive (Stage 1)

**Status:** Stage 1 in implementation (2026-08-12). **Charter (user ruling
2026-08-12):** migrate hot and metadata-critical paths OFF tokio; tokio
remains (for now) the control/handler plane only; the metadata plane's
serialization must not depend on `tokio::sync`'s wake-delivery protocol.
The release battery is HELD until the metadata plane is scheduler-free.

## Why (the OQ-5 record, condensed)

A load-schedule-dependent lost wakeup wedges writes forever (July storm
forensics + 2026-08-12 fstests `generic/464` at capped clocks: 5/6 wedge
rate; both today's tip and yesterday's — pre-existing, not a campaign
regression). The July live decode: a `tokio::sync` batch-semaphore popped
its queued waiter WITH permits fully assigned and invoked its waker — and
the task was never polled again. Counted discriminators this session:

* tokio upgrade — falsified (1.53.1 is latest; reproduces identically).
* Ticked-acquire wrappers on lock levels 1/3/3.5/4a — **insufficient**:
  5/6 wedge with ZERO tick engagements and ZERO lock-wait census, i.e.
  the dead wait sits in the UNWRAPPED metadata guts (the 4b node locks /
  journal / cache population). Chasing sites with wrappers is whack-a-mole.

## What (the primitive)

`src/sqz_sync.rs`: `SqzMutex<T>` and `SqzRwLock<T>`, async-first, with
THE two properties that make the wedge class unrepresentable:

1. **No ownership assignment to sleeping waiters (barging).** Release
   never hands the lock to a queued waiter; it flips the lock free and
   WAKES the front of the queue (all leading readers, or one writer).
   Woken waiters re-contend on poll. A waiter whose wake is lost owns
   nothing and blocks no one — the lock stays takeable by everyone else.
   (The tokio protocol assigns permits to the popped waiter first; a
   never-polled assignee wedges the world. Ours cannot.)
2. **The tick backstop is built into the primitive.** Every async
   acquire re-polls at `TICK` (2 s) — timer wakes ride the driver, a
   different delivery path from waker handoff — so even a waiter whose
   OWN wake was lost self-heals at the next tick instead of parking
   forever. `SQZ_SYNC_TICK_RECOVERIES` (exported as the existing
   `lock_ticked_reregisters` stat) counts engagements: 0 on healthy
   schedules; growth = a lost wake was absorbed, loudly.

Fairness: FIFO wake order bounds barging in practice (the woken waiter
is the first to re-poll in the common case); the tick bounds the
pathological case. Cancel-safety: the acquire future's `Drop` unlinks
its waiter (nothing is ever reserved for it, so cancellation leaks
nothing). Guards: lifetime (`lock/read/write/try_*`) + `Arc`-owned
(`read_owned/write_owned`) — the exact surface the plane's census uses.

## Migration population (Stage 1 — "metadata plane scheduler-free")

Every `tokio::sync` lock in: `src/meta_backend/dlm.rs` (the 4a stripes),
`src/meta_backend/kv/{backend,journal,node_cache,checkpoint,superblock,
tree}.rs` (the 4b population the wrappers never covered), and the
fuse_client stripe populations (`BLOCK_FLUSH_LOCKS`, `INODE_META_LOCKS`,
`active_inode_locks`, plus `lease_locks` — level 2 is the same plane)
that serialize the write path into the plane, and the direct-drive
carrier types (`src/ipc_direct.rs` threads the level-3 block guard).
Conveyor fan-out oneshots are Stage 1b (separate wait class, same
program). Lock ORDER (P1-9) is untouched — this changes the wait
mechanism, never the discipline.

**Enforcement rail:** `tests/sqz_sync_convention_tests.rs` fails the
build on any `tokio::sync::{Mutex,RwLock}` literal in the migrated
population (the env-knob-registry pattern: the convention is a test, so
regression is a red gate, not a review hope). A documented NON-plane
lock in a population file carries a same-line `// sqz-sync-exempt`
marker (today: `ZcWriteSlot::materialized`, a per-write data-path memo
that migrates with its own stage).

**Loom model:** the acquire/release/wake state machine is extracted
dependency-free as `src/sqz_sync_core.rs` (`LockCore<W>` — the
`gauge_core` pattern: `loom-models/` `#[path]`-includes the exact
shipped transitions; the core returns wakers instead of calling them,
so no wake ever runs inside the interior critical section). Models
(`sqz_sync_models`): the dead-waiter wedge rail (a registered,
never-repolled waiter with its wake dropped never prevents a fresh
acquire — **falsification-verified**: substituting the tokio
assign-to-popped-waiter release protocol fails the model), shared-grant
never observes a live writer, and the FIFO wake shape (all leading
shared, or one exclusive; tokens only, ownership on re-poll).

## Stage-1 field attribution (2026-08-12, counted runs aborted at run 1)

The lock migration landed gate-green, and the 464-at-3.0GHz instrument
then produced the NEXT attribution (3/3 reproductions, live-daemon gdb
captures `~/sqz-battery-logs/sqzsync-wedge-{run2,attrib1}.gdb.txt`):

* The lock plane is now HONEST during a wedge: waiters are census-named
  (`lock-wait census: block/write_checkout … waited 356s` with the
  stripe's last holder), and the conveyor/journal/checkpoints stay LIVE
  through the whole stall — the old all-dark signature is gone.
* The stuck party is a SINGLE lost write task (one capture: two holders
  + two same-stripe waiters; the cleaner capture: ONE write, ZERO lock
  waiters, `pipeline_inflight_blocks=0`, leases closed, all 8 NVMe
  uring workers idle-parked, all 216 threads parked).
* `lock_ticked_reregisters` (now on the wedge-census line, which logs
  fine while `.stats` reads hang) reads **0 during the wedge**: the
  lost task's timer wakes no-op just like its I/O wakes — consistent
  with the July ticked-wrapper evidence (5/6 wedge, zero engagements).
  A task lost at the scheduler layer cannot be healed by ANY
  future-layer backstop, ours included.

**Conclusion:** barging protects the plane from a dead WAITER (proved:
the queue keeps moving, everything is named), but the wedge class lives
in the tokio scheduler's task delivery, so a lost HOLDER — or a lone
lost op awaiting its device-completion oneshot — still stalls its
dependents. "Metadata plane scheduler-free" therefore requires the
write-path critical sections and their completion WAITS to run off
tokio tasks entirely (the svc-thread / dd-reaper pattern the hot path
already uses): the Stage 1b/2 wait classes are on the critical path
before the held battery can run, not optional follow-ons.

## Acceptance

* The primitive's unit rails + a loom model of the acquire/release/wake
  state machine (single-winner, no-lost-lock, reader/writer exclusion).
* The counted field instrument: fstests `generic/464` ×10 at 3.0 GHz
  boost-off (the shape that wedges 5/6 on tokio locks) — green ×10 with
  `lock_ticked_reregisters` free to be nonzero (the backstop working is
  a PASS; the wedge is the only failure).
* Full gate + the held release battery, from zero, after Stage 1 lands.

## Out of scope (staged next)

Stage 1b: conveyor/commit fan-out parks. Stage 2: fuse3 lanes from
current-thread runtimes to plain uring event loops. Stage 3:
jobs/wire/background. Full tokio removal is the program's end state;
each stage ships independently behind the same gates.
