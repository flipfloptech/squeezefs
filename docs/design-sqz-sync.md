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

Fairness (**AMENDED 2026-08-13 — the rw5a writer-storm starvation**):
FIFO-wake-order-bounds-barging-in-practice was FALSIFIED by the batched
gate: `reader_cohort_survives_perpetual_patch_storm` wedged 40 minutes
with `TICK_RECOVERIES = 1351` and the storm task at 80 % CPU. A
release→relock loop's next `lock()` first-poll runs INLINE, nanoseconds
after `drop(guard)`, so the woken FIFO front — whose poll latency is
µs-class — lost every race, and each 2 s tick re-contend raced the same
nanosecond free window. `tokio::sync::Mutex` is fair (release hands the
permit to the FIFO front; the releaser's next `lock()` queues behind),
and every call site was written against that contract. The law is now
**bounded barging**: a FRESH attempt takes a free lock only when no one
is queued (exclusive yields to any waiter; shared additionally yields
to a queued writer — write preference, unchanged), while a QUEUED
waiter's re-contend still barges unconditionally. That preserves the
OQ-5 rail in tick-bounded form: a dead front waiter costs contenders
one TICK (its corpse is unlinked by no one, but the queued re-contend
passes it), never a wedge. `acquire_ticked` keeps the SAME waiter
across ticks (queue position = fairness slot survives the tick) and
takes a bare `try_acquire` fast path first, so the uncontended acquire
mints no timer entry and no allocation (the pre-fix shape registered a
`sqz_time` Sleep per lock op — visible in the storm profile). Pinned:
`writer_storm_never_starves_a_waiter` (red pre-fix: deterministic 10 s
starvation), the reshaped loom wedge rail (fresh-queues → ticked-barge),
and the dead-waiter test's tick-bounded deadline.
Cancel-safety: the acquire future's `Drop` unlinks
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

## Stage 1b (landed) + the attribution chain's terminus

Stage 1b replaced the fuse3 TPC handler-lane venue's executor: the
lanes now run **sqz-exec** (`crates/squeezefs-ipc/src/sqz_exec.rs` over
the loom-verified `exec_core` task state word, `#[path]`-shared into
fuse3 like `numa_core`) — wake→queue→poll is first-party code; tokio
remains only as the lanes' entered timer/aux-spawn handle and the main
runtime. Dead-lane re-dispatch kept its loud contract on a thread
liveness word. Tripwires: `lane_exec_tick_rescues` (a lane notify that
never delivered; ≈0) and `lane_exec_task_panics` (RES-8's lane face) —
both on the stats inode and the wedge-census line.

The instrumented 464-at-3.0GHz reproductions then walked the wedge to a
NAMED product bug, one census layer at a time:

1. sqz locks + lane executor: waiters tick (`lock_ticked_reregisters`
   grows through the wedge), `lane_exec_*` clean — waiter delivery is
   healed; the HOLDER never releases.
2. Named-wait census (device / commit / zc_extract classes + the
   `materialized` memo migration): ALL clean during the wedge — the
   holder parks in none of the classical wait classes.
3. Live write-phase census (`write-phase census` watchdog lines): the
   holder parks in **`overlay_store`** — `try_device_overlay_store`'s
   ACK-after-CQE wait (`fuse3 zc_write_store` → `WorkerMsg::ZcStore` →
   `ZcPend::HandlerStore` oneshot) — for 400+ s, with same-block
   writers queued behind the held stripe in `overlay_settle`/`checkout`.
4. Fused-residency watch (`FusedWatch` on the overdue-slot warns): the
   stuck writes are **fused-resident** (parked, not dropped), a ready
   fused task is intermittently left unpolled, and — decisive —
   **`zc_bridge_pends=4` while ZERO deadline-cancel lines print in
   435 s**: the bounded-outcome scan (zc-bridge-cqe-wedge campaign,
   2026-08-07) NEVER RAN on the stuck group's worker. The pend is
   stamped, the gate is armed, the watch thread ticks — and the worker
   never wakes to push its AsyncCancel.

**Terminus:** this is not a scheduler bug and not a lock bug — it is
the zc store (device-overlay D14 leg) losing its CQE under the
capped-clock 464 schedule AND the bounded-outcome deadline ladder
failing to engage because the stuck queue worker never leaves its
cq-wait (wake-fd poll/coalescer arming is the suspect seam; the
existing `SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES` suite covers the
extraction arms but not the HandlerStore arm, and not the
parked-worker-never-scans face). Next: red-first cargo repro of the
HandlerStore + parked-worker shape, then fix the wake/scan path — fix
uring, never bypass it.

## The bounded-outcome hardening loop (2026-08-13, branch fix/zc-store-bounded-outcome)

Landed (each red/green against the zc-bridge suite, which gained
contract 3 — the HandlerStore/quiet-mount leg, green with engagement):

* **Bounded park**: a queue worker holding live bridge pends or
  resident fused tasks parks EXT_ARG-bounded (100 ms — the dd-reaper
  cadence) instead of unbounded `submit_and_wait`; the deadline scan +
  rq drain are self-clocked, never dependent on a cross-thread wake
  reaching a parked worker. Gauge `transport_park_backstop_ticks`;
  live-gdb verified (62/62 parks unbounded before, 31/31 bounded after).
* **Orphaned-pend sweep**: every `Some` pend must hold a deadline
  ledger entry; the scan re-stamps any that lost theirs
  (`fuse3_zc_bridge_orphans`, must-stay-0) — and the scan is no longer
  `zc_mode`-gated (liveness machinery must not sit behind a mode bit).
* **Worker-published scan gauges** on the overdue-slot warns:
  `scan_passes` / `scan_pends_seen` / `scan_orphans_seen` /
  `park_backstop_ticks` — the live discriminator between "scan never
  runs" and "scan sees nothing".

Field verdict so far (464 at 3.0 GHz, counted probes): the wedge
persists, and the gauges FALSIFIED the orphan/parked-scan theories in
the field shape — during a live wedge the scan runs (369k passes),
every pend is stamped, none is overdue, no cancel fires, yet a block-0
stripe stays held 160+ s with all waiters healthy and ticking. The
stall is UPSTREAM of the bridge machinery: the overlay ticket /
ack-early finisher path (`overlay_settle` waits on the in-flight store
set; some runs show the holder in `overlay_store`, some show no live
holder unit at all — the leaked-guard face). Next instrument: a
stripe HELD flag (set on acquire, cleared on guard drop, aged) to split
leaked-guard vs parked-holder, plus naming the ack-early finisher's
awaits in the phase census.

## The A/B conviction (2026-08-13): ACK-early overlay stores

With every lower layer instrumented and exonerated by gauges — locks
(sqz_sync, held-probe: "genuinely held"), lane delivery (sqz-exec),
bridge pends (sent==taken, orphans 0), deadline ladder (scans running)
— the write-phase census kept naming ACK-early overlay machinery
(`overlay_store` / `ov_settle_retry` / the detached continuation), all
code dated 2026-08-10. The counted A/B at the 3.0 GHz wedge posture:

* `SQUEEZEFS_ZC_ACK_EARLY=1` (shipped default): wedges run 1 of nearly
  every count (6+ reproductions this campaign).
* **`SQUEEZEFS_ZC_ACK_EARLY=0`: 5/5 PASS.**

The wedge class lives in the ACK-early device-overlay arm (the §3.4
ACK-before-CQE machinery: retained-slot store continuation + ticket /
settle interlock), not in any scheduler, lock, or transport-delivery
layer. Root-causing THAT arm is the next campaign step; every
instrument built on the way down (named-wait census, write-phase
census with per-block keys and overlay sub-phases, stripe held-probe,
fused residency watch, bridge scan gauges, message economy, bounded
park, orphan sweep) is now permanent wedge-attribution machinery.

## THE FIX (2026-08-13, branch fix/ack-early-settle-wedge) — accepted

**Root cause** (live-gdb capture, thread 105 of the orphan-era wedge):
`await_overlay_inflight` was a `yield_now()` SPIN. The write handler
runs as a FUSED task polled by the transport queue worker's own pass
interleave, so a settle spinning on a live store ticket SELF-WOKE
forever — starving the very pass that pumps the ZcStore `WorkerMsg`
and reaps the store CQE whose ticket the settle waits on. Same-block
writers convoyed behind the held stripe (the checkout census), the
transport read healthy (the pend either never pumped or never reaped
by the starved pass), and capped clocks widened the same-block
settle-vs-store window — which is why 3.0 GHz selected it.

**Fix** (each half red/green):
* `await_overlay_inflight` is an EVENT WAIT on the record's new
  `inflight_change` Notify (register→re-check order; 100 ms belt so a
  missed notify site degrades to a tick; 30 s bark names a genuinely
  leaked ticket). Every store CQE routes through
  `DeviceOverlayRecord::complete_store_and_wake` (all 7 sites).
* zc-armed queue workers ALWAYS park bounded (their commit_rx receives
  foreign-thread zc messages whose only wake is the elidable
  coalescer→eventfd→PollAdd chain — the audit's stranded-message hole).
* Repro-port: `tests/overlay_settle_wait_tests.rs` — parks-and-wakes,
  never-spins (poll-count bound: the yield_now regression detector),
  and the lost-wake belt.

**Accepted**: generic/464 **×10 green** at 3.0 GHz boost-off with
ACK-early ON (the shipped default) — the posture that wedged run 1 of
nearly every pre-fix count; zero settle barks across the runs. The
Stage-1 acceptance instrument is MET; the held release battery may run
from zero.

## Stage 1c (2026-08-13): plane-critical tasks off the tokio scheduler

The plane's LOCKS (Stage 1) and the FUSE handler venue (Stage 1b) were
first-party; the PLANE-CRITICAL detached tasks still rode
`tokio::spawn` on the main multi-thread runtime — the venue whose task
delivery a wedge investigation can never fully exonerate. A lost one
wedges every committer parked on its fan-out, with no census presence.

Landed: `src/meta_exec.rs` — a small process-global sqz-exec pool
(`sqz-meta{N}` threads over the same loom-verified `exec_core`
delivery; own parked timer driver, deliberately independent of fuse3's
so offline verbs — fsck/format — carry no transport dependency). Moved
onto it, panic-contained (RES-8):

* the M7 **conveyor pass task** + the **layout-merge pass task**
  (`kv/backend.rs` — every committer parks on their fan-outs),
* the **checkpoint/SMO task** + the **times drain** (`kv/checkpoint.rs`
  — journal reclamation: a lost one wedges ring admission; their
  shutdown joins ride a drop-guarded completion channel, `DoneGuard`,
  preserving the panicked-task⇒Corrupt surface),
* the routing **publish pass**,
* the write-custody population: the never-lossy **writeback loop** and
  its per-unit uploads, the **extent-fold worker**, the **inode-reclaim
  pool** (stripe-takers/committers whose loss violates never-lossy).

Still tokio-hosted (Stage 2/3 scope, none plane-critical): mount-time
init/teardown tasks, health/stats pollers, job fabric + wire, R5 shed
workers, dehydration workers, supervisor.

## The rip-tokio-out sweep (2026-08-13, user ruling: go big, batch gates)

* **`sqz_time`** (`crates/squeezefs-ipc/src/sqz_time.rs`, `#[path]`-
  shared into fuse3): first-party `sleep`/`sleep_until`/`timeout`/
  `timeout_at`/`interval` over one `sqz-timer` OS thread (heap +
  condvar; cancel-safe tombstones). No tokio driver anywhere in a
  plane wait — the `sqz_sync` TICK, the write/reclaim backoffs, the
  guard heartbeat cadence, the watchdog tick, fuse3's transport
  sleeps all ride it. (`tokio::time::pause/advance` tests of swept
  code became real-time tests.)
* **Venue sweep**: every plane/custody/guard/diagnostic loop now on
  sqz-meta — added this pass: the D1.b op watchdog (the wedge
  instrument must survive a broken scheduler), the D0 writer-guard
  heartbeat (JoinHandle::abort → a stop latch checked BEFORE each
  beat, so a post-unmount beat can never touch a released guard), the
  R5 parked-shed worker, the backend health worker, the staging merge
  worker, the orphan-reclaim batches.
* **Deliberately still tokio** (none plane-critical): the CLI
  bootstrap runtime, job fabric + remote wire + cluster/membership
  wire (tokio net/TLS — Stage 3), mount init/teardown join fan-outs,
  the fabric sampler, and `tokio::sync` channel/notify primitives
  everywhere (driver-free: their wakers deliver through whatever
  first-party executor polls the task).

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
