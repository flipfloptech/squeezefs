# Porting spec + design survey — the executor (charter §9) as `SqzIo`, our own `std.Io`

Status: research output 2026-10-06 (read-only against `~/Source/squeezefs` and Zig 0.17.0 std).
Specification: `docs/charter.md` §9 (L258–268) and §4 (L72–98). Ruling: `docs/decisions/0001` — the
executor is OUR `std.Io` implementation; `src/sync/` already carries the charter's primitives over the
`Io` futex ops (0001 item 3, L36–45). `docs/decisions/0002`: the reference is evidence, quirks are not
requirements; every departure gets a ledger row. Paths below: `R:` = `~/Source/squeezefs`,
`Z:` = `/opt/homebrew/Cellar/zig/0.17.0/lib/zig/std`, `P:` = `~/Source/squeezefs-zig`.

Three files the assignment named are NOT executor code and are out of scope here: `R:src/lane_core.rs`
is the per-writer ino LANE cursor (residue-class partition of one ino space, L1–40; deleted by 0002
row 45), `R:src/park_core.rs` is the symmetric-metadata park GATE (`Running → Parked → Expired`,
L1–33; rung 6), and `R:src/read_lane.rs` is the cold-stream read HOLD (`ReadLaneHold`, L1–100; rung
4). The `loom-models` `lane_core`/`park_core` tests (L4172–4260, L7428+) model those, not the executor.

---

## PART 1 — the reference executor, exactly

### 1.1 Why a first-party executor exists (the OQ-5 record)

`R:docs/design-sqz-sync.md` L106–135: with the locks already first-party, fstests `generic/464` at
3.0 GHz still wedged with a SINGLE lost write task — `lock_ticked_reregisters` read **0 during the
wedge** (L120–124): the lost task's timer wakes no-op'd like its I/O wakes, so "a task lost at the
scheduler layer cannot be healed by ANY future-layer backstop". Conclusion (L127–135): wake→queue→poll
must be OUR code. Stage 1b (L137–147) replaced the fuse3 handler lanes' executor with `sqz_exec`;
Stage 1c (L270–297) moved plane-critical detached tasks onto `sqz-meta`; the rip-tokio-TOTAL sweep
(L321–375) deleted every runtime: the shipped binary links no tokio, enforced by
`tests/no_tokio_convention_tests.rs` (L325–328). The primitive table is L336–353; the venue map
L355–365; the four laws L367–375 (bounded barging; immortal-fs exit edge; stop latches never abort;
enable-then-recheck).

### 1.2 `sqz_exec::LaneExec` — one lane (`R:crates/squeezefs-ipc/src/sqz_exec.rs`)

Design doc L11–37. Data:

| Item | Where | Exact shape |
|---|---|---|
| `TICK` | L49 | `Duration::from_secs(2)` — "the same 2 s liveness posture as `sqz_sync::TICK`" |
| `LanePark` trait | L58–65 | `park(&self, tick) -> bool` (true = tick fired; lane thread only, queue mutex NOT held), `unpark(&self)` (any thread, MUST be sticky: an unpark before the park returns it at once), `service(&self)` (lane thread, once per loop iteration, never blocks — a ring parker reaps CQEs here) |
| `CondvarPark` | L68–94 | sticky `Mutex<bool>` + `Condvar`; `park` loops `wait_timeout` until flag or timeout; returns `timed_out && !woke` |
| `TICK_RESCUES` | L99 | ticks that found runnable work waiting (≈0 healthy; growth = a notify delivery bug) |
| `TASK_PANICS` | L102 | tasks completed-by-panic |
| `LaneTask` | L106–114 | `state: TaskState` (§1.3), `future: Mutex<Option<Pin<Box<dyn Future>>>>` (the waker's `Arc` keeps the TASK alive from foreign threads, so the slot needs the mutex), `exec: Arc<ExecShared>` |
| `Wake for LaneTask` | L120–133 | `wake`/`wake_by_ref`: `if state.wake() { exec.enqueue(task) }` |
| `Ready` | L137–142 | `queue: VecDeque<Arc<LaneTask>>` + `parked: bool` under ONE mutex ("so the mark's visibility to an enqueuer needs no fence argument") |
| `ExecShared::enqueue` | L151–159 | lock → `push_back` → read `parked` → unlock → `if parked { park.unpark() }` |
| `spawn` / `spawn_boxed` | L196–213 | spawn IS the first wake: `TaskState::new_scheduled()` then `enqueue` |
| `shutdown` | L218–221 | store flag (Release) + `unpark` — "the process-wide lanes are process-lifetime; per-volume journal lanes live with their volume" |
| `run` | L225–300 | see below |
| `queue_len` | L303–310 | diagnostics |

The run loop (L225–300), in order: (1) `shutdown` check; (2) lock, `pop_front`; if empty → set
`parked = true` IN THE SAME LOCK SECTION (L239), unlock, `park(TICK)` (L241), relock, `parked = false`,
`if timed_out && !queue.is_empty() { TICK_RESCUES += 1 }` (L244–248), unlock, `park.service()`,
`continue`; (3) `begin_poll()` false ⇒ stale entry, skip (L256–259); (4) build `Waker` from the
`Arc<LaneTask>`, lock the future slot, `catch_unwind(poll)` (L270–282): a panic drops the future,
counts `TASK_PANICS`, `end_poll_complete()`; (5) `Ready` ⇒ drop future + `end_poll_complete()`;
`Pending` ⇒ `if end_poll_pending() { enqueue(task) }` (L290–296: "a wake landed mid-poll: the runner
owns the reschedule — exec_core law 1"); (6) `park.service()` after every poll (L298).

Lost-wake-freedom argument (L33–37): the lane marks itself parked under the lock in the section that
found the queue empty; every enqueue reads the mark under the lock after its push; a push the lane's
check missed sees the mark and unparks; parkers are sticky.

### 1.3 `exec_core::TaskState` — the delivery state word (`R:crates/squeezefs-ipc/src/exec_core.rs`)

One `AtomicU8` per task (L48–50). States L36–45: `IDLE=0` (the only state a wake may enqueue from),
`SCHEDULED=1` (in the queue, or being pushed by the wake that won), `RUNNING=2`, `NOTIFIED=3` (polled
AND woken since the poll began), `COMPLETE=4` (terminal; wakes no-op forever). Transitions:

| Op | Lines | Transition | Returns |
|---|---|---|---|
| `new_scheduled` | L55–59 | starts `SCHEDULED` (spawn IS its first wake) | — |
| `wake` | L66–86 | CAS loop: `IDLE→SCHEDULED` (enqueue), `RUNNING→NOTIFIED` (no enqueue), `SCHEDULED\|NOTIFIED\|COMPLETE` no-op; `AcqRel` success / `Acquire` failure ("an enqueue-winning wake publishes the waker's preceding writes to the runner's next poll") | `true` iff the caller must enqueue |
| `begin_poll` | L92–99 | CAS `SCHEDULED→RUNNING`; false ⇒ stale queue entry | bool |
| `end_poll_pending` | L105–120 | CAS `RUNNING→IDLE`; on failure the state is `NOTIFIED` ⇒ plain `store(SCHEDULED, Release)` ("only the runner may leave NOTIFIED") | `true` iff the caller must re-enqueue |
| `end_poll_complete` | L124–130 | `swap(COMPLETE, AcqRel)`; debug-asserts prev ∈ {RUNNING, NOTIFIED} | — |

Laws (L10–17): (1) a wake is never dropped while the task runs; (2) a task is never queued twice —
only `IDLE→SCHEDULED` and post-poll `NOTIFIED→SCHEDULED` enqueue, both single-winner.

Loom models (`R:loom-models/src/lib.rs` L5477–5560; the file is `#[path]`-included at L330–331):
`wake_racing_poll_is_never_lost_and_never_double_queued` (L5493–5518: runner `begin_poll`, a thread
`wake()`s concurrently with `end_poll_pending`; asserts `runner_requeues ^ waker_enqueues`, final state
`SCHEDULED`, and the rescheduled task is pollable), `concurrent_wakes_enqueue_exactly_once`
(L5522–5536: two wakes on an IDLE task, exactly one enqueues), `wake_racing_completion_noops`
(L5538–5558: a wake racing `end_poll_complete` never enqueues; final `COMPLETE`). **No weakening is
recorded for these three** (the lib docs record weakenings for other cores, e.g. L185–190) — our port
must ship one (§3.13).

### 1.4 Venues — what runs where

**(a) fuse3 handler lanes** (`R:crates/fuse3/src/raw/session.rs`). `TpcScheduler` L5922–5947:
`lanes: Vec<Lane{exec, alive: Arc<AtomicBool>}>` (L5913–5920), `next_idx` rotation, `node_lanes[node]`
(lane indices grouped by the dense NUMA node of each lane's home core), `node_next[node]` cursors,
`lane_of_cpu[cpu] -> Option<lane>` (L5938–5946: "the kernel routes a request to the queue of the
requester's CPU, so a demoted READ minted by queue worker qid goes to the lane whose home core is that
CPU — ONE lane per queue"). `new()` L5950–6076: `core_ids = affinity::process_cpus()` (the PROCESS
mask by pid, never the calling thread's — the Hang-1 lesson, `affinity.rs` L117–127); **core 0 is
removed when > 1 CPU** ("Reserve Core 0 for OS kernel tasks", L5955–5957); `core_count` = `core_ids.len()`
else `available_parallelism()` else 16 (L5960–5966); per lane `i`: `home_cpu = core_ids[i % len]`
(L5989–5993), node grouping (L5995–5997), first lane homed on a core wins `lane_of_cpu` (L6000–6002),
affinity set = `scoped_affinity_cpus(scope, home, core_ids, …)` (L6014–6026), thread named
`fuse3-tpc{i}` (L6036; an unnamed thread inherited the comm of the first toucher — field CPU got
mis-attributed to `sqz-ipc-svcN`, L6028–6034), a `DeadMark` drop guard flips `alive` false on ANY exit
(L6040–6046), `set_current_affinity` (L6048–6050), thread-locals `IS_TPC_LANE`/`CURRENT_LANE`
(L6054–6055, decl L6213–6227), then `exec.run()` (L6064). Dispatch: `spawn` round-robin (L6078–6098;
zero lanes ⇒ a transient `fuse3-tpc-fallback` thread `block_on`s the future, L6083–6091);
`spawn_on_node` (L6106–6119: round-robin WITHIN the node's group, else global — "locality is a
preference, never an availability constraint"); `spawn_boxed_home(cpu, node, fut)` (L6127–6150:
`lane_of_cpu[cpu]` → node group → global). `dispatch` (L6166–6197): walk lanes from `first_idx`,
skip dead ones, count `TPC_LANE_REDISPATCHES += attempt` and `error!` log (L6173–6183), ALL dead ⇒
`process::abort()` ("a daemon that can never again answer a FUSE request must fail loud, not wedge
every mount user", L6186–6196). `same_lane_dispatch_enabled()` (L6239–6252, knob
`SQUEEZEFS_FUSE_SAME_LANE_DISPATCH`, default true): `spawn_read` (L6266–6287) spawns onto the CURRENT
lane when already on a lane thread ("deleting one unbounded-channel hop and one cross-thread wake per
op" — 412 µs `dispatch_lag` at 256 in-flight, L6229–6238); other opcodes keep the rotation (`spawn`,
L6291–6303). Public faces: `tpc_spawn` L6317, `tpc_spawn_on_node` L6331, `tpc_dispatch_boxed` L6342,
`tpc_thread_count` L6346, `on_tpc_lane_thread` L6355. The demote arm hands the boxed READ handler to
`tpc_dispatch_boxed(qid_is_cpu.then_some(qid), queue_node, fut)` (`fuse_over_uring.rs` L7488–7504:
"the first field bracket's node round-robin woke a different parked lane per op and moved the tail
onto the worker's blind window"). RES-18 (L5875–5907): the lane queues stay unbounded because resident
over-uring handler futures cannot exceed `queues × q_depth` (the slot count) — backpressure is the
kernel's; the sideband must never be bounded (it carries INTERRUPT).

**Affinity** (`R:crates/fuse3/src/raw/affinity.rs`): `PinScope::{Core, Node}` (L29–35), default
`Node` — the serve-latency decomposition convicted the pinned-thread runqueue hostage: "queue_wait 1.55
+ dispatch_lag 1.70 ms/op on cold reads"; SCHED_FIFO and affinity-widening each collapsed both terms
~99 % (L4–12). `scoped_affinity_cpus` (L70–94): `Core → [home]`; `Node → avail ∩ node_cpus(node_of(home))`,
empty or unknown node ⇒ the whole available set ("freedom is the safe direction"). `set_current_affinity`
(L97–115, best-effort). `process_cpus` (L128–151: `sched_getaffinity(pid)`).

**(b) The fused lane** (`R:crates/fuse3/src/raw/connection/fused.rs`): one `FusedLane` per drain-group
worker, owned by that worker thread (L218–239): a `slab::Slab<FusedTask>` + shared `FusedRunQueue`
(`Mutex<VecDeque<(id, push_stamp_ns)>>`, L103–118) whose wakers push an id and wake the worker through
the group's `WakeCoalescer` + eventfd (L87–97, counting `TRANSPORT_WAKE_WRITES/ELIDED`); the owning
worker's own pushes skip the eventfd write (`CURRENT_FUSED_LANE` thread-local, L133–150). Admission
`spawn` refuses at `capacity` = the group's aggregate ent depth (L58–61, L270–282) — the caller demotes.
`drain_one` polls EXACTLY ONE ready task per pass (L284–330: "the worker alternates one handler poll with
one WorkerMsg drain + eager ring submit, so a write's DMA launches while the NEXT handler's CPU runs …
the serial pass shape left devices busy ~50 µs of every ~600 µs pass — the measured ~21-in-flight
equilibrium on the 32-CPU rig"); stale ids are skipped; a per-poll `Arc<FusedWaker>` is minted (L311–314);
panics drop the future (its `ReplyTx` guard synthesizes the error reply) and count `panics` (L232, L322–330).
Motivation L5–15: on zc-armed queues a small WRITE paid two cross-thread wakes + two schedules per
4 KiB op — the 0.796× rand-4k row. Fusion ceiling `payload/8` (L56–57).

**(c) `sqz-meta` pool** (`R:src/meta_exec.rs`): `meta_lanes_from(cpus) = if cpus > 1 { 2 } else { 1 }`
(L43–49: "the population is a handful of mostly-parked loops, not a throughput venue"; `cpus` is the
fleet-share-DIVIDED root); lazily spawned `sqz-meta{i}` threads each running `LaneExec::run` with no
ambient runtime (L51–71); `spawn_meta` round-robins (L134–140) and wraps every body in
`detached::contain(site, fut)`; `spawn_meta_join` returns an sqz oneshot receiver (L148–162;
`RecvError` = the task unwound); `JoinStamps{picked_at, done_at}` (L172–176) for the dispatch-hop
split; `DoneGuard` (L205–230) sends `completed` on drop so a panicked plane task surfaces as Corrupt.
Members (design doc L283–291): conveyor pass, layout-merge pass, checkpoint/SMO task, times drain,
publish pass, writeback loop, extent-fold worker, inode-reclaim pool, op watchdog, D0 heartbeat.

**(d) `sqz-lease`** (L85–120): ONE extra `LaneExec` thread hosting only the two lease renewal
cadences — "a lease renewal is a heartbeat — it must be isolated from the workload whose stall it is
supposed to survive" (finding 2, 2026-08-19); "why not the timer thread: it fires wakers ONLY".

**(e) Journal lane** (`R:src/meta_backend/kv/journal_lane.rs`): `RingPark` implements `LanePark` over
the thread-local reactor (L77–102: `park` → `reactor.park(tick)`, `unpark` → eventfd wake, `service`
→ reap); `JournalLane::spawn` (L116–163) creates an `OwnedRing` of `JOURNAL_RING_ENTRIES = 512` (L53),
builds `LaneExec::with_park(RingPark)` — or, if the ring fails, `LaneExec::new()` with pool writes
(L117–134; QUIRK Q7). Thread `sqz-jrnl{idx}` (L141). The port already has this lane on
`io/ring_park.ParkedRing` (`P:src/meta/journal_lane.zig` L457–542) — the only `LanePark` implementor
that parks in a ring; the fuse3 lanes park on the condvar.

**(f) Blocking pool + `block_on`** (`R:crates/squeezefs-ipc/src/sqz_blocking.rs`): `IDLE_REAP = 10 s`
(L36); `set_sizing_parallelism` (L52–54, first value wins); `pool_cap_from(cpus) = cpus·16 max 8`
(L60–62: "blocking work is I/O-parked, not CPU-bound — the oversubscription mirrors the class; floor 8 =
the minimum useful parallel blocking capacity on any host"); `block_on` (L79–96: thread-park waker,
re-poll loop); `yield_now` (L101–115: wake-then-Pending once); `submit` (L150–177): push, then spawn a
thread iff `queue.len > idle && total < cap` (the displacement-storm lesson: notifying one idle worker
for a BURST double-books it), else `notify_one`; thread name `sqz-blk{seq}`; `worker_loop` (L179–201):
idle threads exit after `IDLE_REAP` with an empty queue; `run_blocking` (L205–213) → `wrap_job`
(L217–229: `catch_unwind` + oneshot; a dead awaiter drops the result); `DedicatedWorker` (L236–281):
ONE named thread with its own FIFO ("liveness-class blocking work that must never queue behind the
pool's bulk population") — the one instance is `sqz-lease-io` (`R:src/membership_wire.rs` L116);
`RunBlocking::poll` re-panics in the awaiter (L289–302).

**(g) Timer thread** (`R:crates/squeezefs-ipc/src/sqz_time.rs`): `Entry{deadline, id}` min-heap
(L27–51); `Registry{heap: BinaryHeap, live: HashMap<id, SleepState{Waiting(Option<Waker>) | Fired}>}`
(L54–66); `Service{reg: Mutex, cv: Condvar, next_id}` (L68–72); lazily spawned `sqz-timer` (L95–109);
`service_loop` (L111–157): pop everything due, mark `Fired`, collect wakers, wake OUTSIDE the lock
(L135–141), then `wait_timeout` until the next deadline or `wait` if empty (L144–155); an absent `live`
entry is a cancelled tombstone (counted, L129–132). API: `sleep(d)` L224, `sleep_until(deadline)` L229,
`timeout(d, fut)` L265 and `timeout_at(deadline, fut)` L273 — `Timeout::poll` polls the future FIRST
("a ready future beats an already-expired deadline", L244–262), `interval(d)` L297 whose `tick()` is
`sleep(period)` (missed ticks DELAY, first tick after one full period, L280–292). Registration
(`Sleep::new` L168–186) notifies the condvar only when the new deadline is the earliest (L174–183).
**Cancellation = tombstone**: `Drop for Sleep` removes the `live` entry (L212–221); the heap entry stays
and the service skips it when popped. Gauges L77–93.

**(h) fd-watch thread** (`R:crates/squeezefs-ipc/src/sqz_fdwatch.rs`): one `sqz-fdwatch` thread parked
in `epoll_wait(-1)` over 16 events (L85–116; EINTR → continue; any other error PANICS, L96–98 — "a
broken reactor must be LOUD"); `readable(fd)` (L120–123) = `ticked(ReadableFut)`; the future registers
`EPOLLIN | EPOLLONESHOT` with `EPOLL_CTL_MOD` then `ADD` on `ENOENT` (L182–194), stores the waker in a
per-fd slot with a generation (`Slot{gen, state}`, L46–53); one awaiter per fd — a second registration
replaces the first, which completes `FdEvent::Replaced` loudly (L33–40, L175–180, L201–203); `Drop`
deregisters only its own generation (L209–223). Contract L14–24: level-triggered semantics per await so
"drain then re-await" sites port 1:1.

**(i) Task-locals** (`R:…/sqz_task.rs`): `LocalKey::scope(value, fut)` pushes the value onto a
thread-local stack on every poll and pops on exit via a drop guard (L1–11, L92–125); `with`/`try_with`
(L63–77). **(j) `race2`** (`R:…/sqz_future.rs` L17–45): biased LEFT; the loser is dropped (cancel safety
is the callee's contract). **(k) Thread-per-connection**: `sqz-ipc-svc{n}` service threads are spawned
on demand per OWNER index and shard sessions (`R:src/ipc_host.rs` L2834–2863), i.e. "thread per
connection" is per-owner-sharded there; the cluster wires are std::net + sync rustls threads (design doc
L358–360). **(l) `main`** drives the CLI through `sqz_blocking::block_on` (`R:src/main.rs` L3580,
L2189, L3397).

### 1.5 Laws (each learned from a live conviction — design doc L367–375)

1. Bounded barging (locks; already in `P:src/sync/`). 2. **Immortal-fs**: a loop pinning the filesystem
on a process-lifetime lane must have an exit edge (idle-exit-and-respawn, weak upgrade failure,
last-clone-drop stop guard, channel closure). 3. **Stop latches, never abort**: tasks are never cancelled
mid-poll from outside; loops check a latch at their cadence; `sqz_taskset`'s cancel gate is the one
sanctioned abort shape. 4. Enable-then-recheck. Plus charter §9 L264: cancellation of a committer is
designed OUT (a dropped admission leaks ring budget forever) — passes run detached and panic-guarded
(`R:src/detached.rs` L1–40: `contain(site, fut)` catches the unwind, counts `detached_task_panics`, logs
the site; "deliberately NOT a retry").

### 1.6 Gauges and thread classes

| Gauge (stats inode) | Source | Meaning |
|---|---|---|
| `lane_exec_tick_rescues` | `R:src/fuse_client.rs` L11633 ← `sqz_exec::TICK_RESCUES` | lane tick that found work queued (≈0) |
| `lane_exec_task_panics` | L11634 ← `TASK_PANICS` | handler completed-by-panic |
| `detached_task_panics` | L11596 | `contain` catches |
| `lock_ticked_reregisters` | L11606 ← `sqz_sync::TICK_RECOVERIES` | lock parks healed by the tick |
| `channel_ticked_reregisters` | `sqz_channel.rs` L35 `TICKED_WAIT_RECOVERIES` | every other ticked park |
| `timer_arms`, `timer_tombstones_skipped`, `timer_heap_entries`, `timer_live_sleeps` | L11615–11618; `sqz_time.rs` L77–93 | ipc registry face |
| `transport_timer_*` (same four) | L11624–11627 | the fuse3 `#[path]` copy's SECOND registry — "R-1 measured ≈ 0.38 arms per kernel READ on this face while the row above stayed at ≈ 0.001" (L11621–11623) |
| `tpc_lane_redispatches` | `session.rs` L6203–6208 | dead-lane re-routes (must-stay-0) |
| `transport_park_backstop_ticks` | `fuse_over_uring.rs` L2190, L2206 | 100 ms worker backstop engagements |
| `transport_wake_writes` / `transport_wakes_elided` | `fused.rs` L87–97 | the coalescer's ratio |
| `fused_lane` ready depth, `panics` | `fused.rs` L124–131, L232 | watchdog attribution |
| `ipc_service_threads` | `ipc_host.rs` L2859–2861 | spawned service threads |
| `daemon_cpu_ns`, `daemon_cpu_ns_by_class` | `R:src/daemon_cpu.rs` L1–20 | RUSAGE_SELF total; per-thread `/proc/self/task/*/schedstat` `sum_exec_runtime` folded by comm prefix, sampled at stats-read time only; retired tids keep their last ns (monotone) |

Class table (`daemon_cpu.rs` L31–82, first prefix match wins, `OTHER` L85): `fuse3-tpc`, `f3-ur` →
`fuse3-ur` (R-1 found these folded into `other` at 51 % of a kern rand-4k row's CPU, L39–41),
`sqz-ipc-svc`, `sqz-ipc-dd`, `sqz-meta`, `sqz-jrnl`, `sqz-blk`, `sqz-nvme`, `sqz-zcrx`, `sqz-timer`.
Charter §9 L268 (amended 2026-10-05) names ours: `sqz-fuse-lane<N>`, `sqz-fuse-ur<qid>`, `sqz-meta`,
`sqz-jrnl<N>`, `sqz-blk`, `sqz-timer`, `sqz-ipc-svc`, `sqz-lease-io`. Comm budget: 15 visible chars
(`comm_core.rs` L36; `P:src/core/thread_name.zig` L17).

### 1.7 Knobs the executor venues read (`R:src/env_knobs.rs`)

| Knob | Kind / range | Default | Purpose | Port status |
|---|---|---|---|---|
| `SQUEEZEFS_FUSE_PIN_SCOPE` | Enum `node\|core` (L237) | `node` | lane + queue-worker affinity scope; `core` = pre-campaign 1-CPU pin (A0 control) | row 92 in `knobs_and_rails.md`; register when `exec/placement.zig` reads it |
| `SQUEEZEFS_FUSE_SAME_LANE_DISPATCH` | Bool (L238) | `1` | READ handlers spawn on the dispatching lane instead of the rotation | row 93; read by `concurrent` placement |
| `SQUEEZEFS_NUMA` | Bool (L226) | `on` | all NUMA placement actions; `0` = A/B lever, instrument stays live (`numa_core.rs` L27–30, L505–510) | row 82 |
| `SQUEEZEFS_JOURNAL_LANE` | Bool (L159) | `on` | per-volume `sqz-jrnl` lane vs both stages on `sqz-meta` (D-2 control) | row 23; the Zig journal lane has no fallback (0002 row 29 posture) — QUIRK Q8 |
| `SQUEEZEFS_MEMBERSHIP_RENEW_LANE` | Bool (L357) | `on` | renewal round trip on `sqz-lease-io` vs the shared pool | row 204; rung 6 |
| `SQUEEZEFS_FUSE_ZC_FUSION_MAX` | Int 4096..2^30 (L246) | derived `payload/8` | fused-dispatch ceiling (transport, rung 3) | row 101 |

No knob sizes the lanes, the meta pool, the blocking pool, the tick or the timer (all derived — §1.8).

### 1.8 Derived constants and measured numbers (with WHY)

| Constant | Value | WHY (reference line) |
|---|---|---|
| lane park tick | 2 s (`sqz_exec.rs` L49) | the lock plane's liveness posture; a lost notify costs one tick, never a wedge |
| worker park backstop | 100 ms (`fuse_over_uring.rs` L342–346) | the ipc dd-reaper's shipped backstop; "field captures proved wake chains lose exactly one wake under load" (charter §4 L90); only armed while a reply is owed |
| handler lanes | one per process-mask CPU minus core 0 (`session.rs` L5954–5966) | qid == CPU id; core 0 reserved for kernel tasks |
| drain-group width | `max(node_cpus/4, 1)` (charter §4 L80) | width 8 on 32 CPUs halved drain cost at IOPS par; one context per CPU paid a thread wake + ~1 enter per op |
| meta lanes | 2, or 1 on a uniprocessor (`meta_exec.rs` L43–49) | a handful of mostly-parked loops |
| lease lane | exactly 1 (`meta_exec.rs` L100–107) | two renewal loops, occupancy bounded by `T_self/3` |
| blocking cap / floor | `cpus × 16`, floor 8 (`sqz_blocking.rs` L60–62) | I/O-parked class oversubscribes ×16; 8 = minimum useful parallel blocking capacity |
| blocking idle reap | 10 s (L36) | cached-thread hygiene |
| journal ring entries | 512 (`journal_lane.rs` L53) | — (the Zig lane derives its own) |
| epoll batch | 16 events (`sqz_fdwatch.rs` L86) | — |
| fused ceiling | `payload/8` = 128 KiB at 1 MiB (`env_knobs.rs` L246) | the hop-vs-inline-copy crossover (~2 wakes + 2 schedules vs a DRAM merge) |
| Measured: cross-thread wake hop | 59–62 % of the write round trip (`P:docs/decisions/0002` row 6) | both conveyor stages on ONE lane thread |
| Measured: ingress queueing | `queue_wait` 1.55 + `dispatch_lag` 1.70 ms/op cold reads (`affinity.rs` L5–6); 412 µs at 256 in-flight (`session.rs` L6230–6231) | node-scoped affinity; same-lane dispatch |
| Measured: fused lane | 0.796× rand-4k with 2 cross-thread wakes/op (`fused.rs` L14–15); ~21 in flight, devices busy ~50 µs of ~600 µs pass (L288–293) | poll ONE handler per pass, interleaved with submits |
| Measured: enters per create | 31 → 9 (charter §4 L90) | commit batching: pushes append, the loop-bottom enter flushes |
| Measured: timer arms | 0.38/READ transport face vs 0.001 ipc (`fuse_client.rs` L11621–11623) | the per-op timer class is the ticked park; two registries double the cost |

### 1.9 Tests (name — one sentence)

`sqz_exec.rs`: `spawn_runs_to_completion` (L327) a spawned task completes; `foreign_thread_wake_repolls`
(L341) a future parked Pending and woken from a foreign OS thread is re-polled; `mid_poll_wake_repolls`
(L390) a self-wake during the poll re-polls exactly once more (polled == 2); `panicking_task_never_kills_the_lane`
(L425) RES-8's lane face; `pluggable_park_is_unparked_only_while_parked` (L472) one foreign spawn onto a
parked lane = exactly one unpark, a lane-local spawn adds none, `service` runs ≥ once per park and per
poll; `condvar_park_is_sticky_and_ticks` (L527) a pre-delivered unpark returns the park at once, no unpark
honours the tick; `pluggable_park_never_loses_a_wake` (L549) 2000 spawns from 4 threads all complete in
< TICK/2 (the port judges the same claim by the lane's own books — 0 tick rescues, a lost wake being
a park only the tick ends — since a wall clock measures the Debug build's per-recycle costs: 6.4 s on
x86_64 with 0 rescues); `stress_many_tasks_many_foreign_wakes` (L597) 200 ping-pong tasks all complete.
`session.rs::tpc_dispatch_tests::dead_lane_redispatches_loudly_healthy_lanes_never` (L7431) healthy lanes
never move the counter; a dead lane re-routes every dispatch and counts.
`sqz_time.rs`: `sleep_fires_and_timeout_splits` (L328), `dropped_sleep_cancels_cleanly` (L350) a dropped
hour-long sleep leaves the service live, `many_sleeps_orderly` (L361) 50 sleeps from 50 threads all fire.
`sqz_blocking.rs`: `block_on_drives_timers_and_channels` (L310); `run_blocking_returns_and_reuses_threads`
(L317); `burst_submits_run_in_parallel_past_one_idle_worker` (L342: 4 × 100 ms must finish < 250 ms);
`run_blocking_panic_propagates_to_awaiter` (L363); `dedicated_worker_is_fifo_on_its_own_thread_and_survives_a_panic`
(L376). `sqz_task.rs`: `scope_binds_across_awaits_and_unbinds_outside` (L132), `nested_scopes_shadow` (L146).
`sqz_future.rs`: `race_is_left_biased_and_loser_cancels` (L54). Loom: the three `exec_core` models (§1.3).

### 1.10 Reference QUIRKS (decision 0002 candidates) and the better design

| # | Quirk | Why poor | Better design (what SqzIo does) |
|---|---|---|---|
| Q1 | The pluggable `LanePark` exists but only the journal lane parks in a ring; every fuse3 handler lane parks on a condvar (`session.rs` L5982 `LaneExec::new()`), so a handler's device completion crosses a thread (0002 row 6's hop class) | the charter (§9 L261, §4) wants handler lanes homed per CPU AND completions delivered in-lane | every lane owns a `ParkedRing`; a fiber's I/O is an SQE on ITS lane's ring; the CQE resumes it on the same thread (§3.2) |
| Q2 | `#[path]`-shared copies: fuse3 has its OWN `sqz_exec`, `sqz_time`, `sqz_blocking`, `sqz_fdwatch` statics (`fuse3/src/lib.rs` L129–171) ⇒ two `sqz-timer` threads, two blocking pools (`sqz-blk{N}` names collide), and `lane_exec_tick_rescues` exports only the fuse3 copy — the `sqz-meta` lanes' rescues are never exported (`fuse_client.rs` L11633 reads `fuse3::sqz_exec`) | a crate-layout artifact that duplicated threads and hid a tripwire | ONE `SqzIo` instance, one set of gauges |
| Q3 | A heap `Arc<LaneTask>` per spawn, an `Arc<FusedWaker>` per POLL (`fused.rs` L311–314), boxed futures, `Mutex<Option<future>>` per task | per-op allocation on the request path (charter §12) | fibers from a per-lane free list, intrusive queue nodes, no per-poll allocation |
| Q4 | Timer = process-global heap + HashMap under ONE mutex, a cross-thread hop per fire, tombstone churn as a gauge (`sqz_time.rs` L54–59, L83) | every ticked park (0.38/READ) pays the global lock twice and a thread wake | per-lane timer heap with generation lazy-deletion; the lane's ring park is bounded by the next deadline; no timer thread for lane fibers (§3.6) |
| Q5 | fd-watch = an epoll thread + a cross-thread wake per readiness (`sqz_fdwatch.rs`) | a reactor hop where the lane already owns a ring | `POLL_ADD` one-shot on the lane ring (§3.7) |
| Q6 | `catch_unwind` as the error boundary (`sqz_exec.rs` L270, `sqz_blocking.rs` L223, `detached.rs`) | 0002 row 9: hides programming errors behind a counter | Zig: error unions + tripwires; a true bug aborts loud (ReleaseSafe, 0002 row 23) |
| Q7 | Journal lane ring failure degrades to a condvar lane + pool writes (`journal_lane.rs` L117–134) | charter §1.2: no classical fallback; 0002 row 29 | the Zig lane refuses at spawn (landed) |
| Q8 | `SQUEEZEFS_JOURNAL_LANE=0` = both stages on `sqz-meta` through the uring_fs pool | an A/B arm the Zig design cannot have without re-growing the pool path | not carried; the row becomes `retired` with successor `—` when registered |
| Q9 | Lanes wrap when `core_count > cores` ("never in production", L5998–6002) and a 16-CPU fallback when the mask is empty (L5960–5966) | a guessed constant | `derive.facts()` is the ONE source (`P:src/core/derive.zig` L75–137); an empty mask is a startup refusal |
| Q10 | `Interval::tick` = `sleep(period)` measured from RETURN (`sqz_time.rs` L280–292) — drift by the loop body's duration | a cadence that slips under load | `Timeout{.deadline}` arithmetic: next = prev_deadline + period (delay only when behind) |
| Q11 | `race2` boxes both futures (`sqz_future.rs` L24–26) | two heap allocations per select | `Io.Select`/`Group` + on-frame fibers; no `race2` |
| Q12 | Core 0 reservation by index (`session.rs` L5955–5957) | assumes CPU 0 is in the mask and is "the" kernel CPU | a derived reservation: drop the lowest cpu of the mask only when the mask has > 1 CPU AND it is a housekeeping CPU (`/sys/devices/system/cpu/isolated` / `nohz_full` absent) — otherwise no reservation; recorded in the evidence note |

---

## PART 2 — the Zig 0.17 `std.Io` design survey

### 2.1 The interface value and the `VTable` (`Z:Io.zig`)

`Io = struct { userdata: ?*anyopaque, vtable: *const VTable }` (L24–25). `Evented` = `Uring` on
Linux when `fiber.supported` (L31–36). Every vtable function takes `userdata` first. Fields (L51–244):

| Group | Field (line) → signature | Notes |
|---|---|---|
| crash | `crashHandler(ud) void` (L52) | called from the panic handler |
| tasks | `async(ud, result: []u8, result_alignment, context: []const u8, context_alignment, start: fn(ctx, result) void) ?*AnyFuture` (L58–70) — null ⇒ result already populated, `await` is a no-op; `concurrent(ud, result_len, result_alignment, context, context_alignment, start) ConcurrentError!*AnyFuture` (L72–81); `await(ud, *AnyFuture, result: []u8, result_alignment) void` (L85–94); `cancel(…same…) void` = await + cancel request (L100–109) | `context` is COPIED by the implementation; `result` pointer lifetime ends at the call |
| groups | `groupAsync(ud, *Group, context, context_alignment, start: fn(ctx) void) void` (L112–121); `groupConcurrent(…) ConcurrentError!void` (L123–132); `groupAwait(ud, *Group, token: *anyopaque) Cancelable!void` (L133); `groupCancel(ud, *Group, token) void` (L134) | `Group = { token: atomic ?*anyopaque, state: usize }` (L1344–1346); `await`/`cancel` early-return when `token == null` and assert it is null after (L1408–1431) ⇒ the implementation's status word MUST read 0 when the group is empty |
| cancel | `recancel(ud) void` (L136); `swapCancelProtection(ud, new: CancelProtection) CancelProtection` (L137); `checkCancel(ud) Cancelable!void` (L138) | `CancelProtection = enum(u1){unblocked, blocked}` (L1451–1458); idiom L1460–1468 |
| futex | `futexWait(ud, ptr: *const u32, expected: u32, Timeout) Cancelable!void` (L140); `futexWaitUncancelable(ud, ptr, expected) void` (L141); `futexWake(ud, ptr, max_waiters: u32) void` (L142) | public wrappers L1681–1709 (`futexWake` returns early on 0) |
| operations | `operate(ud, Operation) Cancelable!Operation.Result` (L144); `batchAwaitAsync(ud, *Batch) Cancelable!void` (L145); `batchAwaitConcurrent(ud, *Batch, Timeout) Batch.AwaitConcurrentError!void` (L146); `batchCancel(ud, *Batch) void` (L147) | |
| dir | 26 fns `dirCreateDir … dirHardLink` (L149–174) | blocking semantics in both std impls |
| file | `fileStat, fileLength, fileClose([]const File), fileWritePositional(File, header, data: []const []const u8, splat, offset) !usize, fileWriteFileStreaming, fileWriteFilePositional, fileReadPositional(File, data: []const []u8, offset) !usize, fileSeekBy, fileSeekTo, fileSync, fileIsTty, fileEnableAnsiEscapeCodes, fileSupportsAnsiEscapeCodes, fileSetLength, fileSetOwner, fileSetPermissions, fileSetTimestamps, fileLock, fileTryLock, fileUnlock, fileDowngradeLock, fileRealPath, fileHardLink` (L176–199); `fileMemoryMap{Create,Destroy,SetLength,Read,Write}` (L201–205) | |
| process / stderr | `processExecutableOpen, processExecutablePath, lockStderr, tryLockStderr, unlockStderr, processCurrentPath, processSetCurrentDir, processSetCurrentPath, processReplace, processSpawn, childWait, childKill` (L207–218); `progressParentFile, inheritParentDir, inheritParentFile` (L220–222) | |
| time | `now(ud, Clock) Timestamp` (L224); `clockResolution(ud, Clock) !Duration` (L225); `sleep(ud, Timeout) Cancelable!void` (L226) | `Clock = {real, awake (CLOCK_MONOTONIC), boot (CLOCK_BOOTTIME), cpu_process, cpu_thread}` (L836–884) |
| random | `random(ud, []u8) void`, `randomSecure(ud, []u8) !void` (L228–229) | |
| net | `netListenIp, netAccept, netBindIp, netConnectIp, netListenUnix, netConnectUnix, netSocketCreatePair, netWriteFile, netClose, netShutdown, netInterfaceNameResolve, netInterfaceName, netLookup` (L231–243) | |

Public task API: `Io.async(io, function, args) Future(R)` (L2526–2550) and `Io.concurrent(io, function,
args) ConcurrentError!Future(R)` (L2565–2589) type-erase `args` into `context` and the return into
`result`; `Future(R) = { any_future: ?*AnyFuture, result: R }` with `await` (L1319–1324) and `cancel`
(L1311–1316: "the task receives `error.Canceled` from its next cancelation point … only the next
cancelation point will return it"). `ConcurrentError = error{ConcurrencyUnavailable}` (L2552–2556).
`Cancelable = error{Canceled}` (L815–818). `Group.async/concurrent` wrap `function` whose return must
coerce to `Cancelable!void`; `error.Canceled` is a propagation boundary (L1363–1393). `Timeout =
union(enum){none, duration: Clock.Duration, deadline: Clock.Timestamp}` (L1251–1292). `Io.sleep(io,
duration, clock)` (L2597–2602). `Io.Mutex` (L1716–1780): states `unlocked/locked_once/contended`; `lock`
spins on `swap(.contended)` + `futexWait` (L1731–1747); `lockUncancelable` (L1752); `unlock` wakes 1 on
`contended` (L1770–1779) — THIS is the futex traffic `src/sync/`'s interior mutexes generate.

### 2.2 `Operation` and `Batch`

`Operation = union(enum){file_read_streaming, file_write_streaming, device_io_control, net_receive,
net_send, net_read, net_write}` (L246–255) with per-variant `Result` error sets (L260–489) and a
derived `Operation.Result` union (L491–501). `Operation.Storage` (L503–526) is the implementer's
per-slot scratch: `unused` (double node), `submission{node, operation}`, `pending{node, tag, userdata:
[7]usize}` (L514–519 — 56 bytes the implementation owns while an op is in flight), `completion{node,
result}`. `Batch` (L578–728): `init(storage)` (L590), `add` (L613) / `addAt` (L622) move a slot
`unused → submitted`; `next()` (L655) dequeues completions back to `unused`; `awaitAsync(io)` (L682 —
may run ops inline, cannot fail `ConcurrencyUnavailable`), `awaitConcurrent(io, timeout)` (L695),
`cancel(io)` (L705–727: aborts pending submissions itself, then `batchCancel`; asserts `pending` empty and
`userdata == null` after). `operateTimeout` (L562–571) is a one-slot batch.

### 2.3 `Io.Threaded` facts (`Z:Io/Threaded.zig`; `std.testing.io` IS this — `Z:testing.zig` L23–24)

- One `Io.Mutex` + `Io.Condition` + `run_queue: SinglyLinkedList` + `busy_count` + `wait_group`
  (L30–47). Pool sizing: `async_limit` default `cpus − 1` (L1643), `concurrent_limit` default
  `.unlimited` (L37); threads are spawned on demand when `pool_size − busy_count == 0` (L2106–2117),
  detached, each with `stack_size` (default `std.Thread.SpawnConfig.default_stack_size` = 16 MiB,
  `Z:Thread.zig` L307); `deinit` joins (L1716–1737).
- `async` (L2071–2125): allocate a `Future` closure (`gpa`); if `busy_count ≥ async_limit` run INLINE and
  return null; else prepend to `run_queue`, `condSignal`. `concurrent` (L2127–2171): fails
  `ConcurrencyUnavailable` on OOM, limit, or spawn failure. `groupAsync/groupConcurrent` (L2173–2286)
  likewise, bumping the group status `num_running`.
- Group status packs into `Io.Group.token` as `{num_running: usize−2 bits, have_awaiter, canceled}`
  (L459–485; "the zero value intentionally represents the initial group state … a requirement of
  `Io.Group`"); `state` holds `**atomic u32` the last member increments and futex-wakes (L536–576).
- `await` (L2414–2479) / `groupAwait` (L2288–2345): `futexWait` on a stack `num_completed` word; a
  cancel while awaiting marks the future canceled and `waitForCancelWithSignaling` (L611–631: exponential
  backoff from 1 µs sending `SIG.IO` via `pthread_kill`/`tgkill` to threads blocked in a syscall for that
  awaitable — L1259–1297; the signal handler does nothing, `init` installs it L1656–1667).
- Cancellation state lives in `Thread.status` (`cancelation: u3` ∈ {none, parked, blocked, …, canceling,
  canceled}, L845–887) on a per-worker `Thread` struct reachable via `threadlocal var current`
  (L897). `Syscall.start/checkCancel/finish` (L1346–1400) bracket every blocking syscall; `Thread.current
  == null` (a thread the pool did not create) ⇒ uncancelable (L1352). `checkCancel` (L908–934).
- `futexWait` (L2512–2524 → L985–1003): raw `futex_4arg(WAIT, private)` under the `Syscall` guard; EINTR/
  EAGAIN/ETIMEDOUT all return (caller re-checks). `futexWake` (L2533) = `FUTEX_WAKE`. `sleep` (L11907 →
  `sleepPosix` L11915–11949): `clock_nanosleep` (ABSTIME for deadlines) under the guard.
- `operate` (L2540–2601): the syscall inline on the calling thread. `batchAwait*` on POSIX: `poll(2)` over
  the submitted fds, then the ready ops synchronously (L2615–2660, L2759+).

### 2.4 `Io.Uring` (= `Io.Evented` on Linux) facts (`Z:Io/Uring.zig`)

- Topology: `threads.allocated[0]` is the MAIN thread (L863–899); worker threads are spawned lazily by
  `schedule` up to `thread_limit` (default `cpus − 1`, L793–796) with `IORING_SETUP_ATTACH_WQ |
  R_DISABLED | COOP_TASKRUN | SINGLE_ISSUER` sharing the main ring's wq (L1024–1029), enabled by
  `REGISTER_ENABLE_RINGS` on the thread (L1077–1084); the main ring is `COOP_TASKRUN | SINGLE_ISSUER`
  (L891). **No `DEFER_TASKRUN`.** Ring size `1 << log2_ring_entries`, default `log2 = 3` ⇒ **8 entries**
  (L800). Worker stacks `idle_stack_size = 512 KiB` (L56–58: "Empirically saw >128KB being used by the
  self-hosted backend to panic. Empirically saw glibc complain about 256KB").
- Per thread (L95–107): `idle_context`, `current_context`, `ready_queue: ?*Fiber` (lock-free stack
  with a `finished` sentinel used as a "busy" marker), `free_queue`, `io_uring`, three search cursors.
  `enqueue()` (L128–133) gets an SQE or submits first; `submit()` PANICS on any enter error but EINTR
  (L135–140).
- `Fiber` (L149–162): `context`, `link{awaiter | group prev/next}`, `status{queue_next | awaiting_group |
  free_next}`, `cancel_status: packed{requested, awaiting: nothing|group|io_uring fd}` (L166–215),
  `cancel_protection{user, acknowledged}` (L217–248). Sizes: `max_result_size = 512`, **`min_stack_size =
  60 MiB`** (L263), `max_context_size = 1024`, `allocation_size` page-rounded (L268–274); `create`
  (L281–312) pops the thread's free list, steals (`max_steal_free_search = 4`), else
  `allocator.alignedAlloc(allocation_size)` — the allocator wrapper serialises the backing allocator with an
  `Io.Mutex` (L608–662). **No guard page**; the closure + context live at the TOP of the allocation
  (`AsyncClosure.fromFiber`, L1380–1384) and the stack grows down from the closure.
- Scheduling: `yield(ready, pending_task)` (L961–974) switches to the next ready fiber or the idle
  context, carrying a `SwitchMessage.PendingTask` ∈ {nothing, reschedule, await, group_await,
  group_cancel, batch_await, destroy, exit} (L1270–1279) that `handle` executes AFTER the switch on the
  new stack (L1281–1352 — e.g. `.await` xchg's `awaiting.link.awaiter` and reschedules if the awaited
  fiber already `finished`, L1297–1302). `schedule` (L977–1075): try ONE idle thread (`max_idle_search =
  1`): CAS its `ready_queue null → head` then `MSG_RING` wakeup into its ring (L985–1009); else spawn a
  thread (L1011–1063); else push onto own queue (L1066–1074). `findReadyFiber` (L927–959): own queue,
  then steal from ≤ 2 others. `idle` loop (L1145–1263): `submit_and_wait(1)`, `copy_cqes` ≤ 256, dispatch
  by `user_data` tag (L1090–1100): `0b00` = `*Fiber` (result written into the fiber's result bytes,
  L1188–1194), `0b01` = cancel request from another ring → `ASYNC_CANCEL` on own ring (L1196–1213),
  `0b10` = batch pending slot (L1215–1236), `0b11` = timeout (L1237–1250).
- `async` = `concurrent` else inline (L1428–1441). `concurrent` (L1443–1509) asserts result ≤ 512 B and
  context ≤ 1024 B, builds the fiber, `schedule`s (and `submit`s if a MSG_RING was queued). `await`
  (L1511–1523) yields with `.await` unless already `finished`, then copies the result and `destroy`s.
  `cancel` (L1525–1535) = `requestCancel` + `await`; `requestCancel` (L361–414): set the bit; if awaiting
  a group propagate; if awaiting a ring op: `ASYNC_CANCEL` (same ring) or `MSG_RING` (other ring) with
  `off = fiber|0b01`.
- Cancellation points are `CancelRegion` brackets (L416–470): `init` reads `cancel_protection.check()`;
  `await(awaiting)` atomically moves `cancel_status.awaiting` and returns `error.Canceled` if the bit
  was already set; `deinit` moves it back to `.nothing`. `swapCancelProtection` (L1896–1902),
  `checkCancel` (L1904–1920), `recancel` (L1890–1894) are per-fiber fields.
- `futexWait` (L1922–2007): `IORING_OP_FUTEX_WAIT` (`FUTEX2_FLAGS{size=U32, private}`, `off = expected`,
  `addr3 = maxInt(u32)` mask) optionally `IOSQE_IO_LINK`ed to a `LINK_TIMEOUT` (the `kernel_timespec`
  lives on the fiber's stack across the `yield` — legal only because the fiber stays parked until the
  CQE); `futexWaitUncancelable` (L2009–2041); `futexWake` (L2043–2064): `IORING_OP_FUTEX_WAKE` with
  `CQE_SKIP_SUCCESS` + `submit()`. `sleep` (L4879–4940): `IORING_OP_TIMEOUT` with `ABS`/`REALTIME`/
  `BOOTTIME` flags. `operate` (L2066–2122): READV/WRITEV via the ring; **`net_send`, `net_read`,
  `net_write` are TODO** (L2108–2120). `batchAwaitConcurrent` (L2222–2323) arms a `TIMEOUT` SQE with
  `user_data = &batch.userdata | 0b11` and `TIMEOUT_REMOVE`s it after. Syscalls with no opcode run inline
  under `Sync` + optional `sync_limit` semaphore (L472–530; e.g. `openat` IS an `OPENAT` SQE, L5609–5650).

### 2.5 `Io.fiber` facts (`Z:Io/fiber.zig`)

`supported` for aarch64/riscv64/x86_64 (L1–4). `Context = extern struct{sp, fp, pc}` (L7–24) — only
three words: `contextSwitch(s: *const Switch) *const Switch` (L29–321) declares EVERY register
clobbered, so the compiler spills what it needs around the call; the asm stores `sp`, `fp` and the
resume `pc` into `old`, loads `new`, and `br`/`jmp`s. The function passes one pointer (`x1`/`a1`/
`rsi`) across the switch — Uring's `SwitchMessage` rides it. The file has NO stack allocation and NO
guard pages: stacks are the caller's business.

### 2.6 Why neither std implementation satisfies §9 (0001 L19–24, made exact)

| §9 requirement | Threaded | Uring |
|---|---|---|
| per-core lanes homed on a CPU/NUMA node; handoff targets the home lane | a mutex-protected shared run queue, no homing (L1796–1803) | work is placed on ONE idle thread found by a rotating search or stolen (L977–1009, L934–955); no CPU/NUMA notion |
| a lane thread owns ITS ring and parks in it | no rings | yes, but `COOP_TASKRUN` only (no `DEFER_TASKRUN`), 8 entries, shared wq, and any thread may `MSG_RING` into any ring |
| 2 s ticked backstop on unbounded parks | none | none (`submit_and_wait(1)` unbounded, L1152) |
| explicit lane classes (`sqz-fuse-lane`, `sqz-meta`, `sqz-jrnl`, `sqz-blk`) | one pool | one pool |
| wake→queue→poll owned and model-checked | mutex + condvar | lock-free `ready_queue` stacks + `MSG_RING` — not modelled, not ours |
| no per-op allocation | closure per `async` via `gpa` (L2086) | fibers pooled, but 60 MiB virtual per fiber and a mutexed allocator (L608–662) |
| fail loud, never degrade | `async` silently runs inline past the limit (L2097–2102) | `submit` panics on ring errors (L135–140); `net_*` TODO |

---

## PART 3 — `SqzIo`: our `std.Io` implementation (design)

### 3.1 Shape

One process-wide `SqzIo` struct (`src/exec/sqz_io.zig`), ONE `Io` handle (`sqz.io()`), ONE vtable:

```
pub const SqzIo = struct {
    threaded: Io.Threaded,     // FIRST field: the delegate for every blocking dir/file/process/net op
    lanes: []Lane,             // all lanes, every class (fuse, meta, lease, journal, main)
    placement: Placement,      // cpu → lane, node → lanes, cursors, pin scope
    futexes: FutexTable,       // fiber-aware futex waits (§3.5)
    blocking: BlockingPool,    // sqz-blk (§3.7)
    gpa: Allocator,
    ...
};
```

`userdata = &sqz.threaded`; our own vtable functions recover the owner with
`@fieldParentPtr("threaded", t)`. The vtable is composed at comptime from Threaded's:
`const base = comptime Io.Threaded.io(@as(*Io.Threaded, undefined)).vtable.*;` (Threaded's `io()` only
stores the pointer, `Z:Io/Threaded.zig` L1811–1813), then override the fields we own. Delegated Threaded
functions receive a valid `*Threaded` and work unchanged; because `Thread.current` is null on our threads
(L897, L1352) their syscalls are UNCANCELABLE — exactly the posture the charter wants for committers,
and the only `Io` cancellation that exists in SqzIo is the one we implement (§3.8).

Owned: `crashHandler, async, concurrent, await, cancel, groupAsync, groupConcurrent, groupAwait,
groupCancel, recancel, swapCancelProtection, checkCancel, futexWait, futexWaitUncancelable, futexWake,
now, clockResolution, sleep, operate` (file streaming on a lane ⇒ lane ring; everything else ⇒ Threaded's
inline syscall), `batchAwaitAsync/batchAwaitConcurrent/batchCancel` (v1: Threaded's `poll(2)` form
when called OFF a lane; ON a lane fiber a `Tripwire` + `@panic("Batch on a lane")` — nothing in the
product uses `Batch`; 0001 "unsupported operations fail loud at init, never silently degrade"), `random`
(delegate), `randomSecure` (delegate). Delegated: every `dir*`, `file*` (incl. memory map), `process*`,
stderr lock, `inheritParent*`, `net*`.

`SqzIo` also exposes non-vtable placement API (§3.4) that returns the SAME `Io.Future(T)` type
(`any_future = *Fiber`), so `Future.await/cancel` go through our vtable unchanged.

### 3.2 The lane = one OS thread + one fiber scheduler + one `ParkedRing`

```
pub const Lane = struct {
    index: u16, class: Class,            // Class = enum { fuse, meta, lease, journal, main }
    home_cpu: ?u16, node: ?u16,
    ring: ring_park.ParkedRing,          // THE park: eventfd wake through the coalescer + EXT_ARG timed wait
    inbox: mpmc.BoundedMpmc(*Fiber),     // cross-thread wakes/spawns; capacity = fiber_capacity (law 2: a fiber is queued at most once)
    ready: FiberList,                    // lane-thread-only intrusive FIFO (no atomics)
    timers: TimerHeap,                   // §3.6
    free: FiberList, live: u32, fiber_capacity: u32,
    idle_ctx: Io.fiber.Context, current: ?*Fiber,
    stats: LaneStats,                    // queue depth gauge, tick rescues, polls, fibers_live, hwm
};
```

Pass (the `LaneExec::run` loop L225–300 and the charter §4 worker pass L90, fused):
1. `ring.service()` (drain → DISARM → scan: `P:src/io/ring_park.zig` L306–311); drain `inbox` into
   `ready` (every popped fiber is `SCHEDULED` by construction).
2. Reap CQEs (`copy_cqes`): `wake_ud` → `ring.consumeWake`; a fiber `user_data` → store `{res, flags}` in
   the fiber's result slot, `state.wake()` → if it returns true push to `ready` (a CQE is just another
   wake, so the state word also absorbs a futex wake racing a CQE).
3. Expire timers (`timers.popDue(now)` → `state.wake()` → `ready`).
4. Run fibers from `ready` until empty, each: `begin_poll()` (false ⇒ stale, skip) → `contextSwitch` into
   the fiber → it returns to the idle context with a `Message` ∈ {`.parked` (end_poll_pending: NOTIFIED
   ⇒ requeue), `.finished` (end_poll_complete; hand the result to the awaiter, §3.8; recycle), `.yield`
   (requeue at the tail — `yield_now`)}. The lane's ring SQEs the fiber prepared (`lane.getSqe()` while
   running on the lane thread — legal under `SINGLE_ISSUER`) are flushed by the loop-bottom enter, so a
   handler's commit + device SQEs batch (the 31 → 9 enters law).
5. Park bound: `tick = min(2 s, next_timer − now)`; `ring.armWake()`; if `ready` or `inbox` non-empty ⇒
   non-blocking `submit()` and loop; else `ring.park(tick)`; on `.tick` with `inbox` non-empty ⇒
   `exec_lane_tick_rescues.inc()` (the L244–248 shape, now a must-stay-≈0 per lane).

Fused-lane posture is a lane OPTION: the rung-3 `sqz-fuse-ur<qid>` worker IS a lane (class `fuse_ur`)
whose step 4 runs exactly ONE fiber per pass between message drains and eager submits (the measured
~50 µs/600 µs DMA overlap, `fused.rs` L284–296) — `Lane.poll_budget: u16` (1 for drain-group workers,
unbounded for handler lanes). The `sqz-jrnl<N>` lane (`P:src/meta/journal_lane.zig`) already embeds
`ParkedRing` with its own slot table; it becomes `Lane{.class = .journal}` with its conveyor passes as
fibers when the rung-3 cutover happens (0002 row 6: both stages on ONE lane thread) — the Worker's
`parkOnce` (L1051) is this pass's step 5.

Wake protocol (two model-checked halves): (a) `exec_core` word (§3.3) decides WHO enqueues; (b) the
enqueuer does `inbox.push(fiber)` then `ring.wake()` (coalescer arm → eventfd write iff armed:
`ring_park.zig` L69–88) — publish-then-arm, the `nvme_derive.WakeCoalescer` pair (L316–327). A wake
issued BY the lane's own thread (fiber A wakes fiber B on the same lane) pushes to `ready` directly with
no eventfd write (the `CURRENT_FUSED_LANE` elision, `fused.rs` L133–150) — `Lane.current` threadlocal.

### 3.3 Fibers and the state word

`exec/task_state.zig` ports `exec_core` verbatim as `TaskState` over `std.atomic.Value(u8)` (states
L36–45, ops §1.3) — the fiber's scheduling word: `IDLE` = parked (futex/CQE/timer/awaiting), `SCHEDULED`
= in `inbox`/`ready`, `RUNNING` = on the CPU, `NOTIFIED` = woken while running (the wake-before-park
race: a `futexWake` lands between the fiber's registration and its switch-out; the lane requeues it on
`.parked`), `COMPLETE`. This replaces the reference's per-parker sticky flag and Uring's
`ready_queue` CAS dance with ONE word whose laws are already stated.

```
pub const Fiber = struct {
    ctx: Io.fiber.Context, state: TaskState, lane: *Lane, home: ?*Lane,
    msg: Message,                         // the value carried across the switch
    awaiter: std.atomic.Value(?*Fiber),   // Uring L1300–1302 shape; `finished` sentinel
    group: ?*Group, group_link: {prev,next},
    cancel: CancelStatus, protection: CancelProtection,   // Uring L166–248 shape
    cqe: struct { res: i32, flags: u32 }, // result slot for ring ops
    futex_link: FutexWaiter, timer_gen: u32,
    result: [max_result]u8 align(16), context: [max_context]u8 align(16),
    start: *const fn (*const anyopaque, *anyopaque) void,
    stack: []align(page) u8,              // mmap'd; low page PROT_NONE
};
```

Stacks: `mmap(PROT_READ|WRITE, MAP_PRIVATE|ANONYMOUS|NORESERVE|STACK)` of `stack_size + page`, then
`mprotect(low page, PROT_NONE)` — the guard Uring lacks (§2.4). `stack_size` is DERIVED per class and
tie-tested: fuse/meta fibers 1 MiB (reason on the line: Uring's measured 128–256 KiB floor for std code
paths, L56–57, ×4 for our on-frame node buffers — `kv_node` 64 KiB node images and the 40 B frame
staging live on the fiber frame), journal/main 4 MiB; the debug/test build fills the stack with a canary
and `exec_fiber_stack_hwm_bytes` (Gauge, max) records the high-water at recycle so the pin test
`fiber stack high-water ≤ stack_size/2 on the deepest handler path` fails before an overflow ever can.
`max_result = 512`, `max_context = 1024` (Uring's limits, L257–265) asserted at comptime against every
product spawn site (an `Io.async` of a function with a larger arg tuple is a compile error via a comptime
check in the placement API; the vtable path asserts at runtime like Uring L1451–1454).

Entry trampoline: as Uring's `AsyncClosure.entry/call` (L1390–1425) — naked asm moving `sp` to the first
arg, then a `callconv(.withStackAlign(.c, 16))` `noreturn` that runs `start(context, result)`, then
`finish()` (sets `COMPLETE`, `awaiter.swap(finished)`, schedules the awaiter if present) and switches to
the lane's idle context forever.

### 3.4 Placement — which lane runs what

`exec/placement.zig` builds once at init from `derive.facts()` (`P:src/core/derive.zig` L75–137; the
ONE cpus source, 0002 row 22) and sysfs NUMA (`/sys/devices/system/node/node*/cpulist`, the port of
`numa_core::topology`):

- `fuse` lanes: one per CPU of the process mask (minus the Q12 reservation), `home_cpu = cpu`,
  `lane_of_cpu[cpu]`, `node_lanes[node]`, thread `sqz-fuse-lane<N>`, affinity = `scopedAffinityCpus(scope,
  home, avail, node_of, node_cpus)` ported exactly from `affinity.rs` L70–94 with knob
  `SQUEEZEFS_FUSE_PIN_SCOPE` (`node` default). `SQUEEZEFS_NUMA=0` ⇒ no affinity call, node groups still
  computed (instrument live).
- `meta` lanes: `metaLanesFrom(cpus) = if (cpus > 1) 2 else 1` (`meta_exec.rs` L43–49), no home,
  `sqz-meta<i>`; `lease` lane: exactly 1, `sqz-lease` (named in the reference; the charter's list has
  `sqz-lease-io` for the blocking venue — both fit the comm budget).
- `journal` lanes: one per writable volume, spawned by the volume (`sqz-jrnl<N>`); `main`: the calling
  thread of `SqzIo.run` (`block_on`), class `main`, no rename.

Vtable `concurrent` chooses: on a lane thread ⇒ the CURRENT lane (same-lane dispatch, default ON; knob
`SQUEEZEFS_FUSE_SAME_LANE_DISPATCH=0` ⇒ fuse-class rotation — the A0 control); on a non-lane thread ⇒
the meta pool round-robin (the reference's `spawn_meta` default; `sqz-blk` jobs and wire threads that
spawn land there). Explicit API (all return `Io.Future(R)` / `ConcurrentError`):
`spawnOn(lane: *Lane, …)`, `spawnHome(cpu: ?u16, node: ?u16, …)` = `lane_of_cpu[cpu]` → node group
round-robin → fuse rotation (`spawn_boxed_home` L6127–6150 exactly), `spawnMeta(…)`, `spawnLease(…)`,
`spawnUncancelable(lane, …)` (committers: the fiber starts with `protection = .blocked`). Refusal: a
lane at `fiber_capacity` returns `error.ConcurrencyUnavailable` — the transport tries the worker's own lane
and the rotation, and its `fail_ent` answers `EAGAIN`-class (0002 row 10) only when every lane refuses (§CP); counted `exec_spawn_refused` (per class). Dead lane: a lane thread that
exits (ring dead after `failAll`-class verdicts) flips `alive=false`; `spawnHome`/`spawnMeta` walk to the
next live lane counting `exec_dead_lane_redispatches` (must-stay-0) and logging `log.err` once per dead
lane; ALL lanes of a class dead ⇒ `std.process.abort()` with a loud line (L6186–6196 posture).
Capacity: `fiber_capacity(fuse lane) = q_depth × ceil(nqueues / fuse_lanes) + slack(16) + fanout(cpus)` (port: the
fan-out share is `Lane.optional_cap` — `async`/`Group.async` past it run inline, decision 0003 §CP; RES-18: resident
handler futures ≤ `queues × q_depth`, L5880–5890), `meta` lanes 256 (a handful of loops; a tie test
counts the plane's spawn sites), journal 64, main 16; all preallocated at init (stacks mmap'd lazily on
first use, so the virtual reserve is the only cost).

### 3.5 Fiber-aware futex — how `src/sync/` parks a fiber, not the thread

Every primitive in `P:src/sync/` waits via `tick.park/parkUntil` (`tick.zig` L50–75) →
`futex.ioWaitTimeout` (`futex.zig` L64–71) → `io.futexWaitTimeout` → vtable `futexWait`; and its interior
`Io.Mutex` → `futexWait`/`futexWake`. SqzIo's `futexWait(ptr, expected, timeout)`:

- Off a lane (`Lane.current == null`): `futexes.raw_waiters.fetchAdd(1, .seq_cst)`; raw
  `futex.waitTimeout(ptr, expected, ns)` (`futex.zig` L37–47); `fetchSub`. Uncancelable.
- On a lane fiber: `bucket = futexes.buckets[hash(ptr)]` (256 buckets, each a spin lock + intrusive list
  of `FutexWaiter{ptr, fiber}`); lock; **re-read `ptr.* != expected ⇒ unlock, return**; link; unlock;
  if `timeout` arm the lane timer (§3.6); `yield(.parked)`; on resume: unlock-unlink if still linked (a
  timeout or cancel woke us), cancel the timer if a wake did. Cancel protection `.unblocked` ⇒ the
  `CancelRegion` check before parking returns `error.Canceled` if requested (§3.8).
- `futexWake(ptr, n)`: lock bucket; move ≤ n matching waiters off the list; unlock; for each:
  `fiber.state.wake()` ⇒ `lane.enqueue(fiber)` (same-lane fast path or `inbox.push + ring.wake()`);
  then `fence(.seq_cst); if (bucket.raw_waiters.load(.monotonic) != 0) futex.wake(ptr, n)` — the
  WHOLE count, whatever the fibers took (2026-10-19, `rung-10-mdtest-wedge.md`): a counted raw waiter
  may be the owner thread of a DRIVEN lane a woken fiber lives on, which runs the fiber only once it
  is awake itself; a count-one wake the fiber took alone left both asleep on a free word. The raw
  count is per bucket (a raw park elsewhere does not tax the handler lanes' wakes). The Dekker pair:
  the raw waiter increments BEFORE its kernel compare-and-wait, the waker publishes the word (the
  primitive's store) BEFORE the fence+load — so a raw waiter either sees the new word (EAGAIN) or is
  counted when the waker looks. Models `futex_table_model.zig`: 1 fiber waiter + 1 raw waiter + 1
  waker; invariant "both woken"; weakening: the raw count incremented AFTER the kernel wait (modelled as
  the wait step preceding the increment step) ⇒ a raw waiter sleeps forever (`expectFails`); and
  `DrivenOwnerModel` (the raw waiter owns the fiber's lane, count one; weakening: the kernel wake
  skipped once a fiber took the count ⇒ the owner sleeps forever).
- A thread that DRIVES a lane never blocks off a fiber on anything a fiber of that lane may hold or
  be handed: its filesystem calls are try-only (`node_cache.try_only`: the probes, the inline FORGET's
  corpse judgement), and `exec_driven_owner_queued_parks` counts the timed raw parks that remain.

The 2 s ticked backstop composes unchanged: `tick.park` bounds every wait by `tick_ns` (`tick.zig`
L25, L62–66) ⇒ on a fiber the bound is a lane timer (§3.6) and the fiber's requeue is a `TaskState.wake`
from the timer path — a lost `futexWake` (one that found the waiter unlinked, or a wake the inbox lost)
costs one tick and counts `lock_ticked_reregisters`/`channel_ticked_reregisters` exactly as today. The
lane's OWN park tick (§3.2 step 5) is the second, independent backstop (`exec_lane_tick_rescues`).

### 3.6 Timers — per-lane heap, no timer thread

`exec/timer_heap.zig`: a binary min-heap of `Entry{deadline_ns, fiber, gen}` owned by the lane thread
(no lock). Arm (fiber on the lane): `fiber.timer_gen +%= 1; heap.push(deadline, fiber, gen)`. Cancel
(a wake arrived first): `fiber.timer_gen +%= 1` — the heap entry becomes a tombstone popped later
(`exec_timer_stale_pops`, the reference's `timer_tombstones_skipped` L83 but with no lock and no
HashMap); an entry whose `gen != fiber.timer_gen` is dropped. `sleep(timeout)` = arm + `yield(.parked)`;
`.none` on a fiber parks forever (legal: a cancel wakes it). Off a lane, `sleep` = `clock_nanosleep`
(Threaded's `sleepPosix` L11915–11949, delegated). `Clock` for deadlines: `.awake` via
`core/clock.monotonicNs` (`.real`/`.boot` converted at arm; `cpu_*` refused loud). The lane's park bound
is `min(2 s, heap.peekDeadline − now)` so a timer never needs a `TIMEOUT` SQE (Uring's sleep arms one
per call, L4879–4940, and its futex timeouts a `LINK_TIMEOUT` per wait — SQE churn on every ticked
park). `Interval`: deadline arithmetic (Q10).

0002 ledger row to add: "`sqz-timer` thread (process-global heap + HashMap + condvar, a cross-thread
hop per fire) → per-lane heaps; the thread name is retired from the §9 list" — OR, if the owner keeps
the thread, `exec/timer_thread.zig` (S) is a lane of class `timer` whose only fibers are the heap
owner: every fire is then `lane.enqueue` into the sleeper's lane — the measured hop class (59–62 %)
on every tick. Recommendation: per-lane heaps.

### 3.7 fd-watch, blocking pool, `block_on`, wires

- `readable(fd)` (`exec/fdwatch.zig`): on a lane fiber: `POLL_ADD(fd, POLLIN)` one-shot SQE on the lane
  ring with `user_data = fiber`, `yield(.parked)`, return `.ready` (the `sqz_fdwatch` contract L14–24:
  level-triggered per await; the caller drains then re-awaits); a second awaiter on the same fd is a
  caller bug → `Tripwire exec_fdwatch_double_await` and `.replaced`. Off a lane: `poll(2)`. No epoll
  thread (Q5; a 0002 row).
- Blocking pool (`exec/blocking.zig`): `sqz-blk<N>` OS threads, `cap = cpus × 16` floor 8
  (`pool_cap_from`, L60–62, tie test), `IDLE_REAP = 10 s`, spawn-per-uncovered-job (L155–168 law, tested
  by the 4 × 100 ms < 250 ms burst). `runBlocking(fn, args)` from a fiber: the job cell `{start, ctx,
  result, fiber}` lives ON THE FIBER'S FRAME (it stays parked until completion — no allocation); pushed to
  a `BoundedMpmc(*Job)` of capacity `cap × 4` (refusal = `error.ConcurrencyUnavailable`, counted); the
  worker runs it, then `fiber.state.wake()` → `lane.enqueue`. From a non-lane thread `runBlocking` runs
  inline (there is nothing to yield). `DedicatedWorker` (`sqz-lease-io`) = a `BlockingPool` with cap 1
  and no reap. Panics: none to catch (Q6).
- `SqzIo.run(main_fn, args)`: the calling thread registers itself as `Lane{.class = .main}` (ring +
  idle context, as Uring's main thread L863–901), spawns the root fiber, runs the pass loop until the
  root is `COMPLETE`, returns its result; other lanes are process-lifetime (`LaneExec::shutdown` doc
  L215–217). This IS "main drives the CLI with a thread-park `block_on`" (§9 L263) — the park is a ring
  park.
- Wires: `std.Thread` per connection using the delegated blocking `net*` ops (uncancelable; a stop
  latch + `netShutdown` ends a connection — the design doc's "socket shutdown+join quiescence", L361–363).
  Rung 6.

### 3.8 Cancellation — confined

- Per-fiber `protection: {user: CancelProtection, acknowledged}` and `cancel: {requested, awaiting:
  nothing | futex | timer | ring_op | group | blocking}` (Uring's packing, L166–248). `Future.cancel` →
  `requestCancel(fiber)`: set `requested`; by `awaiting`: `futex` ⇒ unlink under the bucket lock and
  `wake` the fiber (it resumes, sees the bit, returns `error.Canceled`); `timer` ⇒ bump `gen` + wake;
  `ring_op` ⇒ `lane.inbox.push(.{ .cancel = fiber })` — the OWNING lane thread issues `ASYNC_CANCEL`
  on its own ring (never a foreign thread: `SINGLE_ISSUER`); `blocking` ⇒ nothing (a blocking job is
  never interrupted; it wakes on completion and the cancel is observed at the next point); `group` ⇒
  propagate to members. `checkCancel`/`swapCancelProtection`/`recancel` are the per-fiber fields; off a
  lane they are no-ops / `.unblocked` (Threaded's posture for foreign threads, L2402).
- Committers: `spawnUncancelable` starts `.blocked`; `tick.parkUncancelable` (`tick.zig` L88–95)
  already brackets every committer park. `TaskSet` (`P:src/sync/taskset.zig` L31–104) is the ONE
  sanctioned cancel-on-drop shape and rides `Group.cancel` → our `groupCancel`.
- `Group` (`exec/group.zig`): `token` word = `{num_running, have_awaiter, canceled}` (Threaded L481–485;
  zero = empty, as `Io.Group` requires), `state` = `?*Fiber` awaiter (or a futex word when the awaiter is
  a non-lane thread); the last member's `fetchSub` seeing `have_awaiter` wakes the awaiter; `groupAwait`
  = set `have_awaiter` + `yield(.parked)`; `groupCancel` = set `canceled`, walk members (intrusive list
  under a small spin lock), `requestCancel` each, then await. Model: `group_model.zig` — 2 members + 1
  awaiter; invariant: the awaiter is woken exactly once and `token` ends 0; weakening: `have_awaiter`
  set with `.monotonic` after reading `num_running` (two steps swapped) ⇒ a lost wake.

### 3.9 `operate` and file I/O on a lane

On a lane fiber, `operate(.file_read_streaming | .file_write_streaming)` = `READV`/`WRITEV` SQE on the
lane ring (Uring L2124–2185 shape; `user_data = fiber`; `-1` offset = streaming); `device_io_control`
and every `net_*` ⇒ Threaded inline (uncancelable, blocks the lane thread — acceptable only on
`main`/`meta`; a `Tripwire exec_blocking_op_on_fuse_lane` fires when a fuse-class lane runs one). The
product's device paths never use `operate`: `uring_fs`/`nvme_dev`/`journal_lane` own their rings
(0001 item 4). SQE lifetime rule: any memory an SQE references (iovecs, `kernel_timespec`, buffers) must
outlive the CQE — a fiber that armed an SQE stays parked until its CQE lands or `ASYNC_CANCEL`'s CQE
confirms (the `-ECANCELED` completion of the original op is the release point, never the cancel's own
CQE); the fiber's result slot is the only place a CQE is written.

### 3.10 `std.testing.io` stays the unit-test `Io`

All of `src/sync/` uses only `futexWait*`, `futexWake`, `swapCancelProtection`, `checkCancel`, `Group`
and `concurrent` — the subset both Threaded and SqzIo implement. Test law for `src/exec/`: each
primitive test body is a `fn (io: Io) !void` run under `std.testing.io` (every host) AND under a
`SqzIo.testInstance(lanes = 2)` (Linux only, gated `builtin.os.tag == .linux` in `squeezefs.zig`) — the
same contract, two executors. `SqzIo.testInstance` spawns 2 meta lanes + no fuse lanes, deinit joins
them (`shutdown` latch + `forceWake`).

### 3.11 Observability (charter §10)

Gauges (`exec/stats.zig`, all `counters.*`): `exec_lane_queue_depth[lane]` (Gauge: `ready + inbox`),
`exec_fibers_live[class]`, `exec_fiber_stack_hwm_bytes`, `exec_polls[class]` (ShardedCounter),
`exec_same_lane_wakes` / `exec_cross_lane_wakes` (engagement of homing: ≈ all same-lane on a
queue-per-CPU session), `exec_wake_writes` / `exec_wake_elided` (from each lane's ring), `exec_lane_tick_rescues`
(≈0), `exec_spawn_refused[class]`, `exec_dead_lane_redispatches` (0), `exec_timer_arms`, `exec_timer_fires`,
`exec_timer_stale_pops`, `exec_blocking_threads`, `exec_blocking_queue_depth`, `exec_blocking_spawns`,
`exec_blocking_refused`, tripwires `exec_fdwatch_double_await`, `exec_blocking_op_on_fuse_lane`,
`exec_batch_on_lane`. `lock_ticked_reregisters`/`channel_ticked_reregisters` stay in `tick.zig`.
`daemon_cpu_ns_by_class` (`exec/cpu_class.zig`): `/proc/self/task/<tid>/comm` + `schedstat` field 1
folded by prefix table {`sqz-fuse-lane`, `sqz-fuse-ur`, `sqz-meta`, `sqz-lease`, `sqz-jrnl`, `sqz-blk`,
`sqz-nvme`, `sqz-ipc-svc`, `sqz-zcrx`, `other`} with the retired-tid ledger (`daemon_cpu.rs` L7–11,
L112–121), sampled only on `.stats` reads; `Σ by_class ≤ total` by reading classes before `getrusage`.
Thread names via `thread_name.nameCurrentThread`; the comm tie test must cover `sqz-fuse-lane<N>` at the
widest `N` (see Q-A below).

### 3.12 Risks

1. **Stack sizes** — a Zig handler fiber keeps its whole call tree on the fiber stack (Rust futures were
   heap state machines); a `std.fmt` panic path alone needed >128 KiB in Uring's experience (L56). Guard
   page + canary HWM gauge + a derived per-class size; a refusal at init if `mmap` of
   `fiber_capacity × stack_size` virtual fails (`vm.overcommit`).
2. **SQE lifetime across stack switches** — §3.9's rule; enforced by construction (the fiber owns the
   referenced memory and parks until the CQE).
3. **`DEFER_TASKRUN | SINGLE_ISSUER`** — only the lane thread may `io_uring_enter`; cross-lane cancel
   and wake go through the inbox + eventfd, never a foreign `enter`; `ParkedRing.quiesce` must run ON
   the lane thread before `deinit` (`ring_park.zig` L160–166); a fiber migrated to another lane (we never
   migrate: `home` is fixed at spawn — work stealing is designed OUT because a fiber's ring ops are
   bound to its lane's ring).
4. **Long polls** — a CPU-bound fiber on a lane delays every CQE on that ring (`DEFER_TASKRUN` processes
   completions only on enter); the blocking pool exists for that; a watchdog gauge `exec_poll_max_ns`
   (histogram) names offenders.
5. **Kernel floor** — `IORING_OP_FUTEX_WAIT/WAKE` (6.7) is NOT needed (we park fibers in our table, the
   raw path is `futex(2)`); `MSG_RING` is not used; the rings are plain `ParkedRing`s (EXT_ARG, 5.11+).
6. **`Io.Threaded` delegate on lane threads** — `Thread.current == null` makes its syscalls
   uncancelable (intended) but also means a Threaded `async` call through OUR vtable never happens
   (we own `async`); the embedded instance must be created with `concurrent_limit = .nothing` and
   `async_limit = .nothing` so a stray delegate path cannot spawn pool threads.
7. **Comm budget** — `sqz-fuse-lane<N>` is 13 chars + digits: `N ≥ 100` truncates (a 128-CPU box).
   Question Q-A.
8. **Fiber capacity vs transport depth** — `fiber_capacity` must be derived from the transport geometry
   (`nqueues × depth`); the transport's init must come first or the lanes resize once before `session
   ready` (lanes accept a one-time `reserve(n)` before the first fiber).

### 3.13 Module layout, sizes, tests, model checks

| Module | Size | Contents | Tests (red-first) |
|---|---|---|---|
| `exec/task_state.zig` | S | `TaskState` (§3.3) | the three loom models' assertions as unit tests on two threads |
| `exec/task_state_model.zig` | M | explorer model: runner {begin_poll, end_poll_pending/complete} × waker(s) {wake} | `expectHolds`: wake racing poll never lost/double-queued; two wakes enqueue once; wake racing completion no-ops. **Weakenings**: `end_poll_pending` as `store(IDLE)` instead of CAS ⇒ lost mid-poll wake; `wake` as load+store ⇒ double enqueue — both `expectFails` |
| `exec/fiber.zig` | M | stack mmap/guard, `Fiber`, trampoline over `Io.fiber.contextSwitch`, canary HWM | switch round-trip; guard page faults in a child process (`fork` + `SIGSEGV` expected); HWM measured; result/context limits |
| `exec/timer_heap.zig` | S | heap + gen tombstones | order, stale pop counted, cancel-then-fire ignored, `peekDeadline` |
| `exec/futex_table.zig` | M | buckets, waiter lists, raw-waiter Dekker | fiber waiter woken; raw waiter woken; mixed; `expected` mismatch returns at once; timed wait |
| `exec/futex_table_model.zig` | M | §3.5 model | `expectHolds`; weakening (raw count after wait) `expectFails` |
| `exec/lane.zig` | L | `Lane`, pass loop, inbox, park bound, tick rescue, affinity, naming, stats | ports of §1.9: spawn completes; foreign wake re-polls; mid-poll wake re-polls once more; 2000 spawns/4 threads < 1 s with `tick_rescues == 0`; same-lane spawn writes no eventfd (wake_writes unchanged); shutdown joins; a `SQUEEZEFS_TEST_EXEC_DROP_WAKE` seam drops one eventfd write and the tick rescue heals it (`exec_lane_tick_rescues == 1`) |
| `exec/group.zig` (+`group_model.zig`) | M | `Io.Group` over fibers | await after completion returns at once; cancel wakes parked members; TaskSet tests under SqzIo; model + weakening |
| `exec/placement.zig` | M | cpus/NUMA map, `lane_of_cpu`, node groups, cursors, pin scope, dead-lane walk | synthetic topologies (1 node, 4 nodes, CPU-less node, holes); `scopedAffinityCpus` table; dead lane re-routes and counts; all dead aborts (child process) |
| `exec/blocking.zig` | M | pool + dedicated | reuse; burst 4 × 100 ms < 250 ms; idle reap; FIFO on dedicated; refusal at cap×4 |
| `exec/fdwatch.zig` | S | `POLL_ADD` readable | eventfd readable; double await tripwire |
| `exec/sqz_io.zig` | L | vtable composition, `async/concurrent/await/cancel`, cancel protection, `sleep`, `operate`, `run`, `testInstance` | every `src/sync/` test body under SqzIo (§3.10); `Io.Mutex` contended across 2 lanes; `sleep` accuracy ≥ requested; cancel of a parked fiber returns `error.Canceled` once; `recancel` re-arms; blocked protection survives a cancel; `operate` READV on a lane; batch on lane trips |
| `exec/stats.zig`, `exec/cpu_class.zig` | S / M | gauges, JSON; CPU by class | closure identities; class table covers every charter name; a renamed thread moves class on the next sample |
| `exec/exec.zig` | S | root: re-exports + `test {}` | — |

Explorer cores (charter §9 L266): `task_state_model` (the delivery word), `futex_table_model` (the
raw/fiber Dekker), `group_model` (last-member vs awaiter), and the **wake coalescer** arm/disarm pair
(`nvme_derive.WakeCoalescer`, today only unit-tested at `nvme_derive.zig` L712 — owed: `wake_core_model`
with the weakening "disarm before drain" ⇒ a push between drain and disarm is lost) — the charter's "park
gate" and "completion doorbell" rows. Evidence note records which weakening fails each.

### 3.14 Measured numbers that justify the design choices

| Choice | Number (source) |
|---|---|
| completions resume the fiber on the lane that issued them (no cross-thread hop) | wake hop = 59–62 % of the write round trip (0002 row 6); fused lane: 2 hops/op cost 0.796× rand-4k (`fused.rs` L14–15) |
| home lane = the queue's CPU, same-lane spawn default | 412 µs `dispatch_lag` at 256 in-flight from the rotation (`session.rs` L6230–6231); the node round-robin "woke a different parked lane per op and moved the tail onto the worker's blind window" (`fuse_over_uring.rs` L7496–7500) |
| node-scoped affinity default, `core` as the control | `queue_wait` 1.55 + `dispatch_lag` 1.70 ms/op; ~99 % collapse with widening (`affinity.rs` L5–12) |
| poll ONE fiber per pass on `sqz-fuse-ur` lanes | devices busy ~50 µs of ~600 µs pass; ~21 in flight on 32 CPUs (`fused.rs` L288–293) |
| loop-bottom enter flushes the pass's SQEs | 31 → 9 `io_uring_enter` per create (charter §4 L90) |
| 2 s tick on every park; 100 ms only while a reply is owed | `TICK` L49; `PARK_BACKSTOP_TICK` L346; "wake chains lose exactly one wake under load" (charter §4 L90) |
| per-lane timer heap, no timer thread | 0.38 timer arms per kernel READ (`fuse_client.rs` L11621–11623) each paying a global lock + a hop |
| blocking pool `cpus × 16` floor 8, spawn per uncovered job | `pool_cap_from` L60–62; the displacement-storm collapse to one lane (L155–168, test L335–360) |
| 2 meta lanes + 1 lease lane | `meta_lanes_from` L43–49; the heartbeat silenced on the shared pool (L85–99) |
| fiber stacks ≥ 512 KiB class floor, guard page | ">128KB … to panic", "glibc complain about 256KB" (`Z:Io/Uring.zig` L56–58); std's own 60 MiB reserve has no guard (L263, L281–312) |
| ring entries per lane derived from `fiber_capacity` (one SQE per fiber in flight + wake + slack), not 8 | Uring's default `log2_ring_entries = 3` (L800) would make every pass submit-stall (`enqueue` submits on full, L128–133) |

### 3.15 Questions for the owner

- **Q-A** `sqz-fuse-lane<N>` exceeds the 15-char comm for `N ≥ 100`. Amend to `sqz-fuse-ln<N>` (fits
  `N ≤ 9999`) or keep and document a 99-lane display ceiling? Recommendation: amend (one line in §9).
- **Q-B** Retire the `sqz-timer` thread (per-lane heaps, §3.6) and the epoll `sqz-fdwatch` thread
  (`POLL_ADD`, §3.7) from the §9 bullets, recording both as 0002 rows? Recommendation: yes.
- **Q-C** Core-0 reservation (Q12): keep the reference's "drop the lowest CPU when > 1" or derive from
  `isolcpus`/`nohz_full`? Recommendation: derive, with the `core` pin scope as the control arm.
- **Q-D** `Batch` on a lane: fail loud (recommended, v1) or implement over the lane ring (L, Uring
  L2204–2580 shape) now?
