# Porting spec — the IL daemon side

Service threads, the sync read fast path, sever-at-dequeue writes, placed sever, the direct-drive
lane, arena lease and teardown.

Rung 7 (charter §13.7), daemon half. The shim/SDK protocol — bootstrap xattr, HELLO/`SCM_RIGHTS`,
the fd-screen RULES, the sealed memfd layout, the ring/slot/ doorbell cores and their models, the
bail-out ladder, the hybrid lane gate's client half — is the sibling spec. This one covers what the
daemon DOES with an admitted session: the threads that drain it, the ops they serve, the lane they
drive the device from, and how it all tears down.

## Sources

Reference (`R:` = `/Users/joberdorf/Source/squeezefs`): `R:src/ipc_host.rs` (3871 — admission,
service threads, the dequeue loop, the arena lease, teardown); `R:src/ipc_service.rs` (1836 — the
per-op sink: fast path, demote, sever, placed sever, dd gates, the Invalidator);
`R:src/ipc_direct.rs` (2578 — the direct-drive engine: shards, rings, flush, reapers, fusion, the
W1-shaped write, trains); `R:src/placed_sever.rs` (415), `R:src/placed_core.rs` (assemblies and
claims); `R:src/fuse_client.rs` — `ipc_*_probe` (15500–16288), `placed_sever_for` (16149–16187),
`resolve_interception_posture` (31285–31344), `IpcDirectPhase` (4190–4244), the adoption site
(21310–21334); `R:crates/squeezefs-ipc/src/{sizing,layout,cqe_core}.rs`;
`R:docs/design-preload-interception.md` §5.5/§5.8/§8; `R:docs/design-il-direct-write.md`;
`R:docs/design-one-path.md` (shim rows); `R:loom-models/src/lib.rs`;
`R:.benchmarks/2026-07-{19,26,28}-*`, `2026-08-{06,07,08,11,12}-*`.

Port bindings (`P:` = `/Users/joberdorf/Source/squeezefs-zig`): `P:src/exec/sqz_io.zig`
(`spawnHome` 435–441, `homeLane` 447–453, `spawnGroupOn` 458–474); `P:src/data/warm_ladder.zig`
(`WarmLadder.probe` 155–200, `Verdict` 115–119, `Demote` 105–110, `ipc_hold_probe_*` 66–67);
`P:src/fuse/handler/op_guard.zig` (`Table.tryAcquire` 369); `P:src/data/write_admit.zig`
(`Payload` 271–281, `Request` 283–293, `WritePath.write` 551–602, `LastWriteEnd` 235–262);
`P:src/data/accumulate.zig` (`Pool`/`Cell` 160–253, `Table.checkout` 425–457);
`P:src/data/sole_owner_patch.zig` (`shapeEligible` 128–136, `tryPatch` 211–264);
`P:src/data/write_seams.zig` (`LockSite` 290–301, `BlockLocks.tryAcquire` 360–366);
`P:src/io/uring128.zig` (`Ring128.init` 184, `submitAndWaitTimed` 345, `registerFiles` 403,
`taskrun_ladder` 682–687, `prepWrite` 556); `P:src/io/ring_park.zig` (`Parked` 117–);
`P:src/sync/futex.zig` (32–73); `P:src/mem/sizing.zig` (ipc rows 16–99); `P:src/mem/budget.zig`
(`Component` 389–432, `register` 484); `P:src/core/thread_name.zig` (56–57, 80);
`P:src/fuse/transport/session.zig` (`notifyInvalInode` 814, the `writeback_cache` want 945);
`P:src/fuse/handler/kv_fs.zig` (`read` 2068, `write` 2110); `P:src/fuse/handler/filesystem.zig`
(`Ctx.window` 57, `WriteArgs` 177).

Conventions: a number with no citation is arithmetic on a cited one. "svc" = service thread, "dd" =
direct-drive, "il" = the interception lane (the scoreboard label).

---

## 0. The shape in one page

```
 client process (shim)                       daemon
 ────────────────────                         ──────────────────────────────────────────────
 HELLO + fd ──SEQPACKET──▶ sqz-ipc-accept ──▶ ctl thread per connection (connection_loop)
                                              │ screen ladder → fd screen → budget → OWNER pick
                                              │ ensure_service_threads(owner) → memfd → SessionOk
                                              ▼
 slot claim → ring push ─shm─▶ sqz-ipc-svc<N> (one per owner; a session is pinned to ONE for life)
 doorbell (futex)              │ drain: ring pop → snapshot desc → validate → dispatch
                               │   READ : try_read ladder → serve INTO arena → complete   (sync)
                               │          miss/contended → drop guard → spawnHome(lane)   (async)
                               │          governed O_DIRECT miss → dd lane SQE            (CQE)
                               │   WRITE: W1-shaped → dd lane WRITE SQE under try-locked (3)
                               │          else sever at dequeue (placed | pooled) → spawnHome
                               │ flush(): placed handoffs spawn; ONE enter on MY dd shard;
                               │          inline CQ drain of MY shard (fusion)
                               ▼
                       sqz-ipc-dd<N>  (one per shard; shard == svc owner index)
                               │ submit_and_wait(1) bounded 100 ms → drain CQ → finish/finish_write
                               │ → slot complete → cqe doorbell
 slot DONE ◀──shm── futex_wake (if WAITER) ; cqe seq bump (+ wake iff parked ∧ mark ∧ unpaid)
```

Laws every section restates:
One session per svc thread for life (`R:ipc_host.rs:921–925`); Snapshot-then-validate,
serve-from-snapshot (`:1006`, `:1064–1140`); Drop the guard before ANY continuation
(`R:ipc_service.rs:977–985`); Sever before the handoff counter increments (`:456–478`); Never a
runtime inject queue (`:55–69`); Every wait on client memory bounded ≤ 5 ms (`R:ipc_host.rs:88`);
`munmap` only after the last accessor, by structure (`:897–907`); Engagement: `ipc_ops_* ≡ row ops`
or the row is INVALID (`R:docs/design-preload-interception.md:491`).

---

## 1. The service-thread model

### 1.1 Count: the drain-lane width, ONE function for both halves

`service_thread_ceiling_from(env, cpus)` = explicit `SQUEEZEFS_IPC_SERVICE_THREADS` clamped
`1..=64`, else `il_drain_lanes_default(cpus)` (`R:ipc_host.rs:244–248`).

`il_drain_lanes_default(cpus) = clamp(ceil(3·cpus/8), 2, 64)`
(`R:crates/squeezefs-ipc/src/sizing.rs:100–105`; `div_ceil` — "allocation math rounds UP", the
2026-08-14 ruling at `:101–104`).

`dd_shards_from` is the SAME function with the same clamp (`R:ipc_direct.rs:184–188`), so service
threads and dd shards are 1:1 by construction (`:176–183`: "a shard set wider than the ceiling is
production-DARK, since governed submits only ever ride owner-indexed lanes; a ceiling wider than the
shard set shares reapers").

WHY the slope (`sizing.rs:71–85`) — the counted 2026-08-06 width sweep (squeeze-test, 32 CPUs / 2
nodes, il rand-4k 32×qd32, 3×30 s rows per width, W8 brackets at both ends):

| width | IOPS | verdict |
|---|---|---|
| W8 (the old `cpus/4`) | 622–636 k | baseline |
| **W12** | **695–700 k (+10.4 %, clat down)** | best |
| W16 | 675–690 k | past the knee |
| W24 | 616–626 k | oversubscription — regression |

In lane-threads/core terms the grid is `2W/cpus ∈ {0.5, 0.75, 1.0, 1.5}`; the optimum sits where the
lane PAIR (svc + dd reaper = two OS threads per lane, `:63–64`) takes ¾ of the core budget, leaving
the complement to the co-located client fleet; `3·cpus/8` is `2W = ¾·cpus` solved for W (`:80–81`).

Rails:
floor 2 = the pre-L4-8 single-consumer plateau; `⌊3c/8⌋ ≥ ⌊c/4⌋` pointwise, so no box derives below
the previously shipped width (`:86–88`); ceiling 64 = the explicit-lever clamp parity — a default
must be expressible as an explicit setting; engages only at cpus ≥ 174 (`:88–92`).

DOMINANCE over the shim's `il_sessions_default(cpus) = clamp(ceil(cpus/4), 2, 16)` (`:55–59`; the 16
is `SESSION_REGISTRY_SLOTS/2`, `:50`) at every cpus is what makes the 2026-07-28 topology
unrepresentable: 4 shim sessions vs 8 daemon threads left half the drain capacity idle at 7.5 of a
16.6 GB/s ceiling (`sizing.rs:1–29`, `R:ipc_host.rs:216–223`); the hand fix sessions=8 recovered +44
% (`sizing.rs:13–14`).

The cpus input is PROCESS parallelism (the affinity mask), never `available_parallelism()` of the
first toucher (`R:ipc_host.rs:232–236`; decision 0002 row 22).

PORT:
`il_drain_lanes_default` beside `metaLanesFrom` as a pure function of `derive.Facts.cpus`; One tie
test: `svcCeiling(c) == ddShards(c)` ∧ `≥ ilSessionsDefault(c)` for `c ∈ 1..=4096` (the reference
pins at `sizing.rs:264–275`), plus the canonical values `{1→2, 4→2, 8→3, 16→6, 32→12, 64→24,
256→64}` (`:240–262`); `SQUEEZEFS_IPC_SERVICE_THREADS` / `SQUEEZEFS_IPC_DD_SHARDS`:
`derived_override`, range `1..=64` — the reference registry says `1..1<<12` then clamps to 64 in
code (`R:src/env_knobs.rs:281–282` vs `ipc_host.rs:246`); the port's row IS the admissible range
(charter §1.6; §8 quirk 1).

### 1.2 Spawn-on-admission, owner pick, pinning

Threads spawn ON SESSION ADMISSION, lowest owner index first, up to the ceiling
(`R:ipc_host.rs:2834–2863`):
1. `ensure_service_threads(owner)` spawns `sqz-ipc-svc{spawned}` for every index `≤ owner` not yet
   spawned, under the `threads` registry mutex (`:2838–2855`).
2. It refuses after `shutting_down` (`:2842–2844`).
3. `ipc_service_threads` reports SPAWNED threads — 0 on a session-less host (`:1479–1482`,
   `:2857–2860`).
4. Threads are never joined early: "a thread that has served stays (its empty park is the 5
   ms-bounded doorbell wait); the field bug was threads that NEVER had a ring to drain"
   (`:1242–1247`).

Owner pick at HELLO = `reserve_owner(placement_node)` (`:2780–2805`):
under the `owner_loads` mutex, `numa_core::pick_owner` minimizing `(load, distance, index)` when
NUMA placement is active, else plain min-load-then-lowest-index; the increment happens under the
same lock (reserve at pick, release at teardown/refusal `:2811–2823`) because the retired
registry-scan count "raced its own insert" — 32 simultaneous HELLOs convoyed onto low indices
(`:1234–1240`); balance-first (load dominates distance) is the 530 k-ceiling fix: the retired
locality-first order confined a fork-clustered fleet to ONE node's owner subset — 4 of 8 lanes on
the 2×16 box, the ~530 k rand-4k il ceiling with clat doubling per qd step (`:2321–2337`,
`R:docs/design-preload-interception.md:496`); the gauge `ipc_session_owners` = owners with ≥ 1 live
session (`:2800–2803`).

Session→node inference: the peer pid's last-run CPU via `SO_PEERCRED`; SIBLING sessions of one pid
rotate across exec nodes nearest-first so one fd-sharded app never piles every arena onto one socket
(`:2303–2320`). The arena's bind hint is the CHOSEN owner's partition node — memory follows thread
(`:2350–2360`).

`service_loop(idx)` pins itself to its partition node's CPU set ∩ the process mask (`:2892–2898`)
and sets its dd lane `set_service_lane(idx)` (`:2899–2903`, `R:ipc_direct.rs:319–321`).

PORT:
owner = index into `[]Owner` of the ceiling's length; `placement.Topology.nodeCpus` supplies the pin
set; svc threads are plain `std.Thread`s outside the executor and never take an `Io` on the hot path
(AGENTS.md); ctl-thread waits (socket recv, teardown joins) run with the process `Io`.

### 1.3 Session ownership invariant

"A session is pinned to exactly one service thread at admission, for its whole lifetime. There is no
drain handoff and no shard rebalance of live sessions — a service-thread-count change or load skew
rebalances only by natural session churn" (`R:docs/design-preload-interception.md:341`;
`R:ipc_host.rs:921–925`).

This is the `ipc_ring_core` model's single-consumer PRECONDITION — stated outside the model because
the model cannot see its violation (`design-preload-interception.md:278`). The consumer cursor is
daemon-private, behind a never-contended mutex that only documents exclusivity (`:933–936`).

### 1.4 The dequeue loop

Per `service_loop` pass (`R:ipc_host.rs:2927–3092`):
1. **Epoch-cached owned set.** Re-collect `sessions` from the registry ONLY when `session_epoch`
   moved (admission/teardown bump it `:2404`, `:2549`); the hot pass never touches the registry
   mutex (`:1213–1218`, `:2928–2939` — the 2026-07-26 service economy: per-pass re-collect was
   measurable coherence traffic across 8 spinning threads). Staleness bound one pass: a torn-down
   session drains at most once more, safely (bindings cleared, mapping Arc-held, `torn_down` skips
   it at the next refresh `:2546–2549`).
2. **`drain_pass(sessions, rr_start)`** (`:2870–2889`): round-robin from a rotating start; each
   session gets `drain(sink, drain_budget)` with `drain_budget = geometry.slots` (`:1279–1281`,
   `:1435`) — VAL-5e: a fairness bound, not a throughput cap; one honest in-flight window drains in
   a single pass; the bound only removes the ability to starve a sibling (`:954–962`). A session
   returning `None` is poisoned (`:2885`). Served ops refresh `last_active_ms` (`:2882–2883`).
3. **`IpcSession::drain`** (`:963–1056`), per popped ring index:
   1. slot bounds check — out-of-range ⇒ poison (`:971–978`);
   2. `slot.core.try_begin_serve()` — a non-SUBMITTED slot ⇒ poison (`:979–986`);
   3. `inflight.begin()` BEFORE the serve; `live > slots` ⇒ the client re-published a slot it does
      not own ⇒ poison (VAL-5c `:987–1003`);
   4. **`snapshot_descriptor()` — THE one linearization read** (`:1004–1006`);
   5. ONE `monotonic_ns` read `t0_ns` — the dequeue instant; closes the ingress delta from the
      slot's client stamp and anchors every `ipc_direct_phase_ns` span (`:1007–1014`, `:1030–1033`);
   6. op-trace id when armed (`:1022–1029`);
   7. build `SlotCompletion{map Arc, slot_index, inflight}` (`:1045–1049`);
   8. `serve_validated` (`:1050`);
   9. `served ≥ budget` ⇒ break (`:1052–1054`).
4. **`serve_validated`** (`:1064–1140`):
   1. unknown op ⇒ `-EINVAL` + `ipc_descriptor_rejects` (`:1088–1090`);
   2. dead binding id ⇒ reject (`:1091–1093`);
   3. `len > max_op_bytes` ⇒ reject (`:1094–1096`);
   4. `arena_window(arena_off, len)` out of bounds ⇒ reject (`:1097–1099`);
   5. wrong-direction op ⇒ `-EBADF`, counted as a descriptor reject — "the attack/bug tripwire
      family" (`:1100–1117`);
   6. `OP_ECHO` served inline from a severed copy: byte sum + inverted write-back (`:1118–1128`);
   7. READ/WRITE ⇒ `sink.serve_data(DataOp{desc snapshot, binding, t0_ns, trace_id, payload:
      ArenaWindow}, completion)` (`:1129–1138`).
5. **`sink.flush()` once per sweep** (`:2948–2952`) — the liveness rule: "any work deferred during
   `serve_data` MUST become kernel-visible here — the service thread may park for up to its bounded
   window right after" (`:536–542`).
6. Served > 0 ⇒ record `ipc_drain_pass_ns` / `ipc_drain_flush_ns`, `last_progress = now`, loop
   (`:2953–2966`). Served == 0 ⇒ `ipc_drain_empty_passes` and the park ladder (§1.5).

**Completion** (`SlotCompletion::complete`, `:411–459`), in order:
1. release the in-flight ledger FIRST — the slot goes FREE the instant DONE lands; releasing after
   would make an honest back-to-back resubmit look like an over-admission (`:412–419`);
2. `set_result`;
3. `core.complete()` — the prior WAITER bit ⇒ `FUTEX_WAKE(state_word, i32::MAX)`; breadth is
   load-bearing since the event-driven reap: a split submitter/reaper pair may BOTH park on one op's
   word (`:425–434`);
4. the completion doorbell `cqe.complete(latch)` ⇒ `Wake` (`ipc_cqe_wake_writes`, futex_wake on the
   seq word `i32::MAX`) | `Collapsed` (`ipc_cqe_wake_collapsed`) | `Elided` (`ipc_cqe_wake_elided`)
   (`:444–458`).

A handle dropped without `complete` releases the ledger but loses the reply — the client's deadline
covers it (`:473–482`). Exactly one completion per dequeued op (`:410–411`).

### 1.5 The park ladder (every wait on client memory bounded)

After an empty pass (`:2967–3091`):
Spin window = explicit `SQUEEZEFS_IPC_SPIN_US` verbatim (`0..=10_000` µs, `:161–165`), else the
adaptive governor when `SQUEEZEFS_IPC_SPIN_ADAPTIVE` and the lane owns ≥ 1 session (`:2974–3007`).
The governor's window derives from the lane's own park-duration EWMA, the `/proc/stat` busy ceiling
`100 − lanes/cores %`, and the session count (`R:crates/squeezefs-ipc/src/spin_governor_core.rs`);
gauges `ipc_spin_window_us`, `ipc_spin_absorbed_parks`, `ipc_spin_disengaged_busy`; The STATIC
default was adjudicated 0 on 2026-07-26: every nonzero ambient window bought its libaio-fleet gain
by stealing runnable CPU from the sync lane (−4 % at 20 µs, −22 % at 100 µs with 8 threads;
`:104–119`).

Park (`:3043–3091`):
1. set every owned session's `daemon_parked = 1` (SeqCst);
2. SNAPSHOT every doorbell;
3. RESCAN (`drain_pass` again) and `flush()`;
4. if the rescan served nothing: `futex_wait_many` (`futex_waitv(2)`, ≤ 128 words `:3778`) on ALL
   owned doorbells against the pre-rescan snapshots, bounded `SERVICE_PARK_MAX = 5 ms` (`:88`,
   `:3063–3084`);
5. clear `daemon_parked` (`:3088–3090`).

Two lost-wake classes this order closes (`:3020–3041`):
1. **Snapshot BEFORE rescan.** Snapshotting after folds a submission that raced the rescan's tail
   into the wait's expected value — 13 % of parks expired by timeout under load. Model
   `ipc_park_snapshot_before_rescan_never_strands` (`R:loom-models/src/lib.rs:3790`; weakening: the
   shipped pre-fix order fails `:3786–3788`).
2. **Wait on ALL doorbells.** The former single-doorbell wait deafened the thread to every other
   session's wake — the sessions=8 < sessions=4 inversion.

A sessions-empty lane parks on `park_timeout(5 ms)` (`:3064`). `ipc_service_parks` counts parks
(`:3067`).

PORT:
`sync/futex.zig:37 waitTimeout` is the single-word form; add `waitMany(words: []const Waiter,
timeout_ns)` over `futex_waitv` (`SYS_futex_waitv`, `FUTEX2_SIZE_U32 = 0x02`, max 128) with the
single-word fallback on ENOSYS — the degrade is loud once; The 5 ms bound is a documented constant
(charter §7 "every wait on client memory bounded"; the D18 ladder cap); The daemon-parked / doorbell
/ coalescer words are the sibling spec's `wake_core`; the svc loop uses them exactly as the FUSE
worker does (`drain → disarm → scan`).

### 1.6 What runs on the svc thread vs what is handed off

| On the svc thread (sync; no `Io`, no await, no alloc per op) | Handed off |
|---|---|
| dequeue, validate, ECHO (`R:ipc_host.rs:963–1140`) | — |
| READ: `try_read` + guarded probe + serve into the arena + complete (`R:ipc_service.rs:961–1025`) | READ miss / contended / cold attr ⇒ the FULL read handler on a fuse lane (`:1026–1089`, `:240–323`) |
| READ governed O_DIRECT miss: probe + SQE on MY dd shard (`:937–960`, `:1127–1178`) | its CQE-side fallback ⇒ the read handler (`R:ipc_direct.rs:2098–2119`) |
| WRITE: W1-shaped probe + fence + SQE on MY dd shard (`:1207–1233`, `:392–433`) | the durable-times park, ONE per drain batch (`R:ipc_direct.rs:1981–2000`, `R:ipc_service.rs:341–357`) |
| WRITE: the sever (placed or pooled) — the ONE arena read (`R:ipc_service.rs:456–478`) | the write handler with the severed payload (`:503–527`); placed severs DEFERRED to `flush()` (`:528–538`) |
| `flush()`: spawn deferred placed handoffs, ONE enter on my shard, inline CQ drain (`:1258–1269`) | — |
| bind/unbind invalidation hooks: a synchronous notify enqueue, zero spawn (`:693–706`) | — |

**The handoff venue** (`R:ipc_service.rs:55–79`): `handoff_spawn` → `tpc_spawn_guarded` = the fuse3
per-core handler lanes — "the same venue kernel-lane handlers run on"; `handoff_spawn_on(node, fut)`
prefers a lane pinned on the payload's arena node when placement is active (`:221–232`).

PORT:
`SqzIo.spawnHome(cpu, node, fn, args)` (`P:src/exec/sqz_io.zig:435–441`) with `cpu = null` (a svc
thread has no requester CPU) and `node = arena_node` — "the fuse lane homed on `cpu`, else the
node's round-robin, else the fuse rotation; dead lanes are walked around"; Use
`spawnGroupOn(homeLane(null, node), session_group, …)` (`:458–474`, `:447–453`) so every handoff is
a GROUP member the session's teardown joins (§5.4); The handoff body constructs a handler `Ctx`
whose `window` IS the arena window (`P:src/fuse/handler/filesystem.zig:57`) — the port's form of the
E-IL2 "arena dest" (§2.2).

### 1.7 Thread names (the CPU accounting contract)

`sqz-ipc-svc<N>` (`R:ipc_host.rs:2850–2852`); `sqz-ipc-dd<N>` (`R:ipc_direct.rs:997–999`);
`sqz-ipc-accept` / `sqz-ipc-accp` (path listener; the 16-char base `sqz-ipc-accept-p` blew the comm
budget AND truncated onto the abstract thread's spelling `:1493–1498`); `sqz-ipc-reap` (`:1505`);
`sqz-ipc-thp` (`:2639`; not carried — §8 quirk 7).

PORT: `thread_name.zig:56–57` already classifies `sqz-ipc-svc` / `sqz-ipc-dd`; `sqz-ipc-svc63` is in
the worst-case fit list (`:80`). Add `sqz-ipc-acc` and `sqz-ipc-reap` to the fit test.

---

## 2. The sync READ fast path

### 2.1 Step by step (`DataPlaneSink::serve_read`, `R:ipc_service.rs:917–1091`)

0. `ServeStamp::begin()` — the ro-coherence in-flight stamp bracketing the sync legs (`:923`).
1. **Device-true policy skip.** `binding.odirect ∧ direct_device_true` ⇒ never the tier ladder (a
   tier serve would turn a device-true row warm — measurement fraud); run the dd read probe →
   `engine.submit` or fall back to the handler; NOT counted as a miss demotion (`:924–960`).
2. **`lock.try_read()`** on the per-inode guard (`:961–962`). `Err` ⇒ a writer holds or queues: feed
   the read lane (`missed = true`, so the handler never observes this op again — `lane_pre_fed`),
   `ipc_fast_path_lock_demotions`, `enqueue_read` (`:1076–1089`).
3. **`ipc_read_probe_locked(ino, offset, len, &payload)`** under the guard
   (`R:fuse_client.rs:16195–16288`):
   1. attr-cache size — a miss would need the handler's async `getattr` ⇒ `Miss` (`:16205–16208`);
   2. ONE `metadata_cache.get` whose size OVERRIDES the attr size — the load-bearing stale-size
      guard: a read racing a write must not return short and let the kernel cache zero pages
      (`:16209–16228`);
   3. a pre-step layout entry ⇒ `Miss` (`:16216–16226`);
   4. `offset ≥ size` ⇒ `Eof` (`:16230–16232`);
   5. multi-block ⇒ `Miss` (`:16236–16240`);
   6. an open device-overlay record on the block ⇒ `Miss` (KD-OV-13; the OVERWRITE population's old
      key would be the R1 stale-serve class `:16241–16253`);
   7. active buffer present: extent repr or not covering `[rel, rel_end)` ⇒ `Miss`, else
      `Hit(snapshot.slice)` — a CoW-stable snapshot, immutable for the completion's life
      (`:16254–16287`);
   8. no active buffer: `router.try_read_range_sync(file, meta, offset, read_len, out)` = staging
      mmap → hot tier → read-lane hold → NVMe read cache, each writing STRAIGHT into the arena
      `PayloadSink` ⇒ `Served(n, arm)`; all declined ⇒ `Miss` (`:16256–16270`).
4. **`drop(guard)` BEFORE ANY CONTINUATION** (`R:ipc_service.rs:977–985`).
5. Dispatch on the verdict:
   - `Eof` ⇒ `complete(0)` + `ipc_fast_path_serves` + `ipc_ops_read` (`:987–991`);
   - `Hit(bytes)` ⇒ `payload.write(&bytes)` — the ONE copy, outside the guard ("exactly the memcpy
     the tier leg used to spend on the intermediate buffer there" `:982–984`), counters, complete,
     THEN the lane touch (`:992–1007`);
   - `Served(n)` ⇒ bytes already in the arena; counters, complete, lane touch (`:1008–1025`);
   - `Miss` ⇒ lane touch FIRST (`missed = true`; the 4th contiguous op's stream classification
     vetoes direct-drive for THIS op `:1026–1039`), then for an O_DIRECT binding
     `try_direct_drive_default` (§4.2), else `ipc_fast_path_miss_demotions` + `enqueue_read`
     (`:1062–1073`).

The two demotion counters split regression semantics: `lock` growth on read-only workloads =
unexpected writers; `miss` growth on warm workloads = fast-path rot
(`R:docs/design-preload-interception.md:345`, `:492`).

PORT:
`WarmLadder.probe(ino, offset, size, odirect, window)` (`P:src/data/warm_ladder.zig:155–200`) IS
this probe — same rungs, same try-only law (its doc: "nothing in rungs 0–5 awaits, spins, allocates
per op or touches a device"), already carrying `ipc_hold_probe_serves/misses` (`:66–67`) and the
`.contended` / `.miss` / `.pre_screen` / `.write_side` demote reasons (`:105–110`); The il sink
calls it with `window = the validated arena window` and maps `Verdict`: `.served{n}` ⇒ complete(n);
`.eof` ⇒ complete(0); `.demote(.contended)` ⇒ `ipc_fast_path_lock_demotions`;
`.demote(.miss|.write_side)` ⇒ `ipc_fast_path_miss_demotions`; `.demote(.pre_screen)` on `odirect ∧
device_true` ⇒ the dd probe (the policy skip); The ladder already drops the guard inside `probe`
(`defer guard.release` `:164`) before returning — drop-before-enqueue is structural in the port; One
difference to keep: the reference copies a `Hit` snapshot AFTER the guard drops (`:982–984`); the
port's ladder copies under the guard. The reference's own tier legs ALSO copy under the guard
(`try_read_range_sync` with `out` = the arena), so only the active-buffer arm differs; keep the
port's one code path (§8 quirk 3).

### 2.2 Serve into the arena: ONE copy, or a DMA for uninterpreted bytes

`ArenaWindow` implements `PayloadSink` (`R:ipc_host.rs:666–695`): `write_at` copies caller-private
bytes into the window (counted on `ipc_arena_copy_bytes` and the NUMA pass instrument); `zero_at`
zero-fills. Every tier serve lands in place — "no intermediate heap buffer" (`:662–665`).

The handoff path (`R:ipc_service.rs:81–116`, `:266–310`): the `IPC_READ_DEST` task-local carries
`(window base, len)` around exactly one handler invocation; the read handler's dest-armed legs land
bytes IN PLACE; the pointer-equality check `reply.data.as_ptr() == window base` counts
`ipc_read_dest_serves`; heap replies (staged/inline/virtual arms) still bounce through
`payload.write`. Lever `SQUEEZEFS_IL_READ_DEST`, default ON — E-IL2 deleted a 1 MiB-class alloc +
full CPU pass per cold il read (`:267–270`).

Direct-arena DMA (the dd lane, §4.2) is §5.3.1-rule-3 legal only because read payloads are
uninterpreted bytes; transform volumes are prelude-ineligible (`R:ipc_direct.rs:67–71`).

Copy ledger (il): `dest + bounce + dest_dma + ipc_arena_copy_bytes ≈ ipc_bytes_out`
(`R:docs/design-one-path.md:673`). Tier hit = 1 daemon copy + 1 app copy; device-true direct = DMA
into the arena + 1 app copy — "strictly one better than kernel FUSE can ever do"
(`R:docs/design-preload-interception.md:355–358`).

PORT: the arena window is the handler `Ctx.window` (`P:filesystem.zig:57`), so `kv_fs.read`
(`P:kv_fs.zig:2068–2103`) already serves in place; the inline arm's `@memcpy` into `c.window`
(`:2089–2095`) is in place too. The port has NO bounce arm for ring reads: `ipc_read_dest_serves` =
handoff replies (always in place), `ipc_arena_copy_bytes` counts only the sync ladder's copies.

### 2.3 The demote: drop the guard FIRST, then hand off

"Drop-before-enqueue is load-bearing, not style: tokio's `RwLock` is write-preferring/FIFO, so a
handoff re-acquiring `lock.read()` behind a queued writer while the service thread still holds its
read guard is a self-deadlock" (`R:docs/design-preload-interception.md:345`;
`R:ipc_service.rs:14–19`, `:977–985`).

The port's `sync/rwlock.zig` is write-preferring too (charter §9 "bounded barging …
write-preferring"), so the same hazard exists; `WarmLadder.probe` releasing inside makes it
unrepresentable.

The handoff re-runs the FULL handler under its own guard and re-seeds a cold attr cache first so
warm workloads return to the sync path after ONE miss (`R:ipc_service.rs:282–287`). Pin:
`queued_writer_lock_demotion_completes_after_release_no_deadlock`
(`R:tests/preload_parity_tests.rs`).

### 2.4 The ~130 µs/op inject-queue finding

`R:.benchmarks/2026-07-26-ipc-handoff-economy.md:1–60`:
Cold ddt il t16qd16, 290.9 k IOPS, avg 879 µs; per-op CPU across the tokio workers only ~14 µs —
"the ~100–130 µs/op gap vs the kernel lane at equal offered load is queueing, not compute"; The
structural asymmetry: the kernel transport's dispatch task spawns handlers onto the per-core
`TPC_SCHEDULER` lanes (unbounded FIFO channels), while the svc thread is a FOREIGN OS thread to
tokio — `Handle::spawn` lands every miss on the multi-thread runtime's GLOBAL INJECT QUEUE, which
saturated workers poll only between local-queue batches; Fix: route handoffs through the same
per-core lanes — "direction 3 in its cheapest shape — not a new executor"; directions 1 (batching)
and 2 (direct-drive) were NOT needed for that term; Secondary: the 30 µs spin default burned 13.6 %
of daemon cycles in `__vdso_clock_gettime` across 4 svc threads; Pins:
`handoff_runs_on_a_current_thread_handler_lane`,
`handoff_spawns_from_a_plain_os_thread_without_ambient_runtime` (`R:ipc_service.rs:1786–1835`).

PORT: there is no inject queue to avoid — `spawnHome` enqueues on the lane's own queue and wakes the
lane's ring (`exec.md` §3.4); the pin becomes "a svc-thread handoff lands on a FUSE-class lane homed
on the arena node, never `sqz-meta`" (an assertion on the lane class inside the spawned fiber).

### 2.5 The ring-side read-lane feed

Every ring read observes into the §5.3 read lanes EXACTLY once: warm serves AFTER completion (silent
consumption must advance the consume edge or issue wedges at its window), misses BEFORE the ladder
(`R:ipc_service.rs:967–976`, `:1026–1039`; `router.ring_read_lane_touch(meta, ino, offset, len,
missed)` `R:routing.rs:13277`).

The hold-probe campaign (`.benchmarks/2026-08-03-il-hold-probe.md`): the §5.5.1 fourth leg
(read-lane hold) serves ring reads at ~µs; its R1b ceremony dispatches to the handler lanes exactly
once per serve; `ipc_hold_probe_{serves,misses}` are the engagement pair, 0 by construction under
`SQUEEZEFS_READ_LANE=0` and on `direct_device_true` mounts
(`R:docs/design-preload-interception.md:494`).

PORT: `WarmLadder.probe` already calls `rp.ringReadLaneTouch` (`P:warm_ladder.zig:197–198`) and
defers the hold ceremony through `Ceremony.spawn` (`:149–152`).

---

## 3. WRITES

### 3.1 Sever-at-dequeue and the severed-buffer pool

Law (`R:docs/design-preload-interception.md:360`): a ring WRITE's arena lease "is consumed by the
accumulation merge / one-shot severing copy BEFORE the op completes, and the slot is not FREEd
(arena slab not reusable) until the lease drops".

In the daemon this is `spawn_write_handoff` (`R:ipc_service.rs:448–539`):
the sever is the ONE arena read, performed synchronously on the svc thread BEFORE
`ipc_async_handoffs` increments — tests park the handoff behind a held writer and scribble the
arena; the scribble must be inert (`:456–458`; pins
`arena_severance_client_scribble_never_reaches_acked_bytes`,
`adversarial_mid_serve_mutation_serves_the_snapshot`); the severed payload IS the source for the
write handler, "entering the same severance-boundary machinery FUSE leases use" (`:40–44`); every
derived value (checksums, `--write-verification` compares, transform input) comes from the severed
copy, never a second arena read (§5.3.1 rule 2).

**The pool** (`SeveredPool`, `R:ipc_host.rs:706–785`):
The deleted engine: `vec![0u8; len]` per ring write is a slab-sized (`max_op_bytes`) allocation past
jemalloc's tcache, so a streaming write paid arena mutex + extent recycle + MADV purge + ~256 page
re-faults (with kernel re-zeroing the sever memcpy immediately overwrites) per op — **measured 1.76
M minor faults/s across 5 saturated svc threads at 12.7 GB/s** (the field's 81–87 % %system
signature) (`:708–717`); Recycled buffers keep their pages mapped and warm: the sever becomes ONE
user-space memcpy, zero new copies (`:715–718`); Sizing is derived: buffers are `max_op_bytes`
(uniform class, `len` set per op); queue slots = `clamp(arena_cap_bytes / max_op_bytes, 1, 65536)` —
the structural in-flight bound: severed bytes in flight can never exceed the live arenas R5
admitted, so retention is worst-case bounded by the session-shm cap (`:719–725`, `:740–744`);
`get()` pops or allocates (`ipc_severed_pool_{hits,misses}`); `put()` recycles only full-class
buffers, a full queue drops (dealloc = the pre-pool behavior) (`:751–784`); `PooledSevered` returns
the buffer when the LAST clone drops — custody crosses the handoff and the handler freely; a parked
payload only delays that buffer's reuse (`:787–810`); Gauge `ipc_severed_pool_bytes`; R5 component
`ipc_severed_buffers`, non-sheddable (`R:fuse_client.rs:31774`; charter §6.2).

PORT:
An `mpmc.zig`-backed free list of `max_op_bytes` slabs, page-aligned so a pooled slab can be an
O_DIRECT DMA source on the dd lane's unaligned leg (§4.5); hits/misses as `counters.ShardedCounter`,
bytes as `Gauge`; register `("ipc_severed_buffers", floor 0, weight 0, non-sheddable)` with
`mem_budget.MemBudget.register` (`P:budget.zig:484`); The handoff carries `write_admit.Payload{
.bytes = slab[0..len] }` (`P:write_admit.zig:271–281`); the fiber owns the slab for its life and
`put()`s at exit (the port has no refcounted `Bytes`; the one retaining arm — the W2 extent park —
copies into its own slabs, `data_write_path.md` arm F).

### 3.2 Placed sever: whole-block stream chunks into a shared assembly the first merge ADOPTS

The 1-copy ring write path (`R:src/placed_sever.rs:1–34`): the ring path paid TWO copies where the
kernel path pays one (arena → severed buffer, severed buffer → `ActiveBlockBuf`). Placed sever makes
the severed DESTINATION the block's future backing: whole-block-stream chunks of one `(ino, block)`
sever into one shared **assembly** (`SharedBlock` + `PlacedClaims`); the first merging handler
ADOPTS it as the overlay backing; every sibling merge is a coverage record with the copy elided
(pointer proof) (`:5–12`).

**Eligibility** (`placed_sever_for`, `R:fuse_client.rs:16149–16187`; screens cheapest first):
1. block size a 4 KiB multiple (`:16156–16158`);
2. `rel % 4096 == 0 ∧ len % 4096 == 0 ∧ rel + len ≤ bs` (`:16160–16163`);
3. `len > patch_max_bytes()` — at or below the W1 cap the op may be absorbed by the patch/extent
   machinery before the merge, so a raised cap widens patch and narrows placement together, never
   overlaps (`:16164`, `:16131–16134`);
4. `offset + len ≤ max_file_size` — the EFBIG mirror (`:16165`);
5. cached layout class == striped; a miss demotes (`:16170–16176`);
6. NO live overlay entry for the block — an existing entry can never adopt a new backing
   (`:16178–16183`).

**Keying and claims** (`PlacedSeverRegistry::sever`, `R:placed_sever.rs:116–211`):
1. registry = latch-free map `(ino, block) → Arc<BlockAssembly>` (`:85–95`);
2. get-or-create under the per-key entry guard; creation is charged to `placed_assembly_bytes` and
   refused past `assembly_cap_bytes = min(budget/8, 2 GiB)` (0-budget pre-arm ⇒ the 2 GiB rail
   alone; `:50–56`, `:152–169`);
3. `claims.begin_claim(rel/4096, len/4096)` — page-granular exclusive bits; overlap with a live
   claim or a sealed assembly ⇒ pooled fallback (`:179–189`; `ipc_placed_sever_fallbacks`);
4. `outstanding++` under the guard (`:192`);
5. the ONE arena read `block.write_at(rel, src, len)` OUTSIDE the entry guard (a 1 MiB memcpy must
   not hold the per-key latch) inside the claims WRITER window (`:195–201`);
6. `claims.end_write()`; `ipc_placed_severs`; return `Bytes::from_owner(PlacedSevered{registry, key,
   assembly, rel, len})` (`:202–210`).

Dropping the payload releases the claim bits and, at `outstanding == 0` (Release decrement + Acquire
fence on the last one out), reaps a never-adopted entry (`:274–304`, `:257–264`).

**Adoption** (`take_for_adoption`, `:222–253`; called by the write merge's entry-absent branch under
the block's `BLOCK_FLUSH_LOCKS` guard, `R:fuse_client.rs:21310–21334`):
`remove_if` the entry whose region at `rel` IS `payload_ptr` (pointer proof — only a placed sever
can satisfy it) AND `claims.seal_for_adoption()` succeeds (no sever mid-copy — the Dekker); hand the
`SharedBlock` to `ActiveBlockBuf::adopted(&shared, has_existing_bytes)` — `placed_adoptions`; the
seed class is identical to fresh/deferred: "adoption changes WHERE the backing memory came from,
never the coverage/seed law" (`:21316–21334`); a pointer match whose seal is refused counts
`placed_adoption_refusals` — the whole cohort demotes to the copy path
(`R:placed_sever.rs:238–248`); sibling merges: pointer-proof elision `placed_merge_elides`
(`R:fuse_client.rs:21508`).

**The Dekker** (`R:placed_core.rs:21–33`): the claimer publishes `writers` THEN checks `sealed`; the
adopter publishes `sealed` THEN reads `writers`; `SeqCst` forbids both observing the old value —
"the same shape as the W1 §5.1 clone/patch fence". Sealing is permanent. Models
(`R:loom-models/src/lib.rs:4540–4903`): `placed_claims_overlap_exclusive_and_rollback_clean`,
`placed_claims_never_double_grant_while_held`,
`placed_seal_vs_claim_dekker_never_adopts_over_a_writer`,
`placed_assembly_refcount_reaps_only_at_zero_and_publishes_writes`,
`placed_assembly_reclaim_under_the_guard_survives_the_reap`.

**CoW destruction-safety**: every in-flight placed payload holds the assembly's shared handle, so
the adopted cell is NON-unique: in-place mutation copies away instead of touching severed bytes
(`R:placed_sever.rs:20–26`; `data_write_path.md:589` "`adopt(shared)` for placed-sever assemblies
(starts non-unique)").

**The deferral** (`PENDING_PLACED_HANDOFFS`, `R:ipc_service.rs:180–213`, `:528–538`):
a placed sever's whole win is that every sibling chunk lands in the assembly BEFORE the first merge
parks the overlay entry (an entry-present block never adopts; post-creation chunks pay the 2-copy
fallback); spawning the handler at `serve_data` raced the rest of the drain pass — measured ~50 %
sever engagement / ~28 % merge elision on the t16×4 MiB bracket — so placed handoffs queue
service-thread-locally and `flush()` spawns them after the pass severed everything in flight; pooled
writes and read demotions keep their immediate spawn; foreign callers (dd CQE-side fallbacks, on
reaper threads with no flush contract) pass `defer_placed_to_sweep = false` (`:440–447`).

PORT:
An `Assembly` = one `accumulate.Pool.Cell` (`P:accumulate.zig:160–253`) checked out through a new
`Pool.acquireShared()` (refs start at 1 + outstanding payloads) plus a `PlacedClaims{bits:
[]Atomic(u64), writers, sealed}` (one bit per 4 KiB — `CLAIM_PAGE`, `R:placed_core.rs:46`); Registry
keyed `accumulate.Key{ino, block}` (`P:accumulate.zig:272`); `Table.checkout` grows an `adopt:
?*Pool.Cell` argument (`:425–457`): entry-absent ∧ pointer-proven ∧ sealed ⇒ `repr = .{ .full =
adopted }` with refs > 1 (so `uniqueOrCopy` copies away, `:245–253`), `placed_adoptions`; The merge
in `admitPiece` (arm G/J, `data_write_path.md:133–136`) elides its `@memcpy` when `data.ptr ==
cell.data[rel..].ptr` (`placed_merge_elides`); `write_admit.Payload` gains `.placed = { cell, rel,
len, registry }` whose bytes view is `cell.data[rel..rel+len]`; drop = `claims.release` +
`outstanding--` + reap; The `SeveredPool` and the `Assembly` pool are DISTINCT; the assembly cap
`min(budget/8, 2 GiB)` is a component gauge `placed_assemblies` (non-sheddable; charter §6.2); The
claims core is a model (`placed_claims_model.zig`) with the Dekker weakening (drop either `SeqCst`
fence ⇒ `expectFails`).

### 3.3 Interception mounts force kernel write-through (KD-11)

`resolve_interception_posture(custom_opts, cli_flag, env_flag, writeback_default)`
(`R:fuse_client.rs:31285–31344`):
`interception = "interception" ∈ opts ∨ --interception ∨ SQUEEZEFS_IPC=1`; `interception ∧ explicit
writeback` ⇒ refuse LOUD ("mount option conflict … Drop one of the two options" `:31324–31331`);
`write_back = interception ? false : explicit ? true : default` (`:31333–31339`).

WHY (`R:docs/design-preload-interception.md:374`): the default-on kernel writeback cache acks a
buffered `write(2)` once the page is DIRTIED — before the daemon sees a FUSE WRITE — so a ring read
(direct-to-daemon by construction) would miss it; write-through "restores the invariant that daemon
state contains every acked write from any transport … closed STRUCTURALLY, not by invalidation
heuristics" (JuiceFS ships the same posture). Cost: buffered kernel-path small writes lose
dirty-page batching — priced as an A/B line; the tax falls on the residual unintercepted-writer mix.
Pin: `kd11_interception_forces_write_through_and_refuses_explicit_writeback`
(`R:tests/ipc_host_tests.rs`).

PORT: the pure resolver beside the mount options; its result feeds `InitWants.writeback_cache`
(`P:session.zig:945`, `P:codec.zig:895`). The refusal is a mount-time typed error, not a log line.

**The residual windows the daemon owes a mitigation for**
(`design-preload-interception.md:376–382`): W1 (ring writes → stale buffered kernel READERS) is
bounded by `FUSE_NOTIFY_INVAL_INODE` on bind, on the FIRST ring write per `(ino, window)` (default 1
s), and on the last unbind; W2 (cross-process `MAP_SHARED` readers) and W3(b) (cross-process mmap
writers) are declared unsupported; W4 inherits W1.

The `Invalidator` (`R:ipc_service.rs:571–684`):
`on_bind` resets the ino's write high-water and fires `Whole` (`:602–609`); `on_last_unbind` fires
`Whole` (`:614–617`); the host asks `ino_bound_anywhere` across every live session first
(`R:ipc_host.rs:1702–1713`, `:2043–2047`); `on_write(ino, end)` (`:631–656`): the window's `Whole`
when due; else POSIX-8's `AttrsOnly` (`off < 0` — attrs and ACLs, no page) when the write GREW the
file past the announced high-water (a `lseek(SEEK_END)` the shim routes to the kernel must learn the
size before appending over live data `:560–568`); else `ipc_inval_suppressed`; the hook is a
SYNCHRONOUS enqueue in the caller's context — the retired `Handle::spawn` per fire was the
inject-queue venue again (`:693–706`); FIRE-AND-FORGET by contract: a ring completion may overtake
the kernel-side invalidation; the Whole arm can lawfully sleep a full READ round trip in
`invalidate_inode_pages2_range`, so a synchronous device write on this path is banned (`:726–748`);
counters `ipc_inval_notifies`, `ipc_inval_attrs_only`, `ipc_inval_suppressed`.

PORT: `Session.notifyInvalInode(ino, off, len)` (`P:session.zig:814`, `error{QueueFull}` — a full
sideband queue is counted, never awaited); the high-water and last-fire words live in a striped
per-ino table (the reference's unreclaimed `scc::HashMap` grows one entry per ring-written ino for
the mount's life — `:577–587` records the ceiling; §8 quirks 4, 8).

DIVERGED (2026-10-28, decision 0003 §CE — the reference's interception mount kept every kernel TTL
at 1 s, so its limiter leaned on expiry, and the port never advertises `FUSE_AUTO_INVAL_DATA`, so
no TTL ever refreshed a page): the mount is ISOLATED for the TTL law, every class `T_owner` (a
regular file's SETATTR/LINK reply excepted: the base attr class, `KvFs.attrTtlFor`), and the
limiter drives coherence by invalidation alone, never dropping a fire. `P:src/ipc/ring_inodes.zig`
is the mount-owned table of words `{hwm u48, gen u3, last_fire_s u13}`. `P:src/ipc/host/inval.zig`:
BIND fires nothing (the binding's first write is due); a write: due ⇒ `Whole` (leading edge),
grew ⇒ awaited `AttrsOnly`, else suppressed: its binding ROW is marked dirty (`Binding.dirty`; the
clean → dirty edge raises the session's sweep flag) and a row read dead (SeqCst, after the mark:
`inval.Row`) fires `Whole` itself; the ctl poller's TRAILING sweep once per window
(`IpcHost.sweepDirty`: flag lowered, then the rows), UNBIND and a session's teardown fire `Whole`
for a dirty row (`fireIfDirty`, a swap: once); a kernel size set (`KvFs.noteSizeSet`: truncate,
`O_TRUNC`, collapse/insert) sets the high-water to the new size and moves `gen` — a write admitted
before it (gen read before its admission) raises nothing; a ring read that parked an access time
(`note_read` → true, or `Ctx.atime_parked` on the handoff) fires `AttrsOnly`; `QueueFull` falls back
to the reserved attrs-only cells (`Session.notifyAttrs`). Models: `P:src/ipc/ring_inodes_model.zig`
(the unbind's Dekker; the sweep's mark-then-flag order). Counters `ipc_inval_unbind_fires`,
`ipc_inval_sweep_fires`, `ipc_inval_dead_binding_fires`, `ipc_inval_atime`.

### 3.4 The write path entry the service calls

The ring write enters the write handler's body with the severed payload as its source —
`fs.write(request, ino, fh 0, offset, severed, write_flags, 0)` (`R:ipc_service.rs:503–506`): "same
fencing acquisition, same inode lock, same coverage-union write-through, same W1 patch eligibility"
(`R:docs/design-preload-interception.md:346`).

`write_flags = FUSE_WRITE_KILL_SUIDGID` iff the binding's `kill_priv` — the session peer's
HELLO-time `uid + CAP_FSETID` class standing in for the kernel's per-write `!capable(CAP_FSETID)`
(`:487–498`, `R:ipc_host.rs:358–369`); On success: `ipc_ops_write`, `ipc_bytes_in += written`,
complete, THEN `inval.on_write(ino, offset + written)` (`:507–521`); The synthetic request identity
is the daemon's own (uid/gid/pid cached once; authorization already happened at the fd screen;
`ReplySlot::Classical` = "the reply rides the IPC completion, not a FUSE ring slot" `:861–875`).

PORT:
The handoff fiber builds `filesystem.Ctx{ unique 0, uid/gid/pid = the daemon's, io = the lane's Io,
window = arena window }` and calls `kv_fs.write(c, WriteArgs{ ino, fh 0, offset, data = severed,
held = null, class = .entire_op, write_flags, flags })` (`P:kv_fs.zig:2110`,
`P:filesystem.zig:177`); That routes to `WritePath.write` (`P:write_admit.zig:551–602`): the ENOSPC
terminal law, the per-ino `last_write_end` word, the W1 shape half, then per-piece admission A (W1)
→ B (overlay) → C → … (`:616–`); a placed payload rides `.placed`, a pooled one `.bytes`; The
level-1 guard is the handler's own `admitWrite` classification (`P:op_guard.zig:447`) — the
shared-mode handoff applies to ring writes exactly as to kernel ones.

---

## 4. The direct-drive lane

### 4.1 Shards and rings

`DirectDriveEngine::spawn` (`R:ipc_direct.rs:806–969`), built LAZILY on the first governed op
(`R:ipc_service.rs:770–777`, `:879–899`; a mount that never sees the shape pays nothing; a failed
spawn latches `None` loudly and every governed op falls back forever):
1. open every data volume through the ladder R/W+O_DIRECT → R/W buffered → R/O+O_DIRECT → R/O
   buffered (`:826–871`); a volume that refuses R/W degrades to reads-only for the dd write lane,
   loudly once (`VolSlot.writable`, `:700–709`);
2. `width = dd_shards()` (§1.1); `nodes = owner_nodes(width)` — the svc threads' own partition at
   this width, so lane i's reaper lands on lane i's submitter's node (`:875–880`);
3. per shard: `IoUring::builder().setup_coop_taskrun().build(512)` with a one-shot EINVAL degrade to
   the plain setup (`:882–909`); `register_files(all volume fds)` — fixed files, a refusal falls
   back to raw fds (`:910–923`); `EngineState{inflight slab, free list, inflight_count, staged}`,
   `cq_gate`, `pending_submits`, a lazy reaper handle, `node` (`:924–939`).

**`RING_ENTRIES = 512` per shard** (`:165–169`): "512 in-flight direct reads ≫ any observed per-lane
governed depth (t32qd32 offers ≤ 1024 across 8 service threads, and each service thread owns its own
shard; SQ-full refusals fall back to the handler — counted, never stranded)". The slab is pre-sized
to 512 entries and recycled through the free list so BOTH directions' submit paths are
allocation-free (`:598–609`; ~360 KiB/shard).

**WHY `COOP_TASKRUN` and NOT the FUSE rings' `SINGLE_ISSUER|DEFER_TASKRUN`** (`:882–891`;
`.benchmarks/2026-08-08-shim-drain-funnel-r3.md:64–67`):
a dd ring has TWO issuers — the svc thread submits, the `sqz-ipc-dd` reaper calls `submit_and_wait`
— so `SINGLE_ISSUER` is structurally violated, and `DEFER_TASKRUN` requires `SINGLE_ISSUER`; without
any flag the kernel delivers each completion's task-work by `TWA_SIGNAL` — interrupting whichever
thread last touched the ring and running `io_handle_tw_list` under the ring's `uring_lock` ON THE
SVC THREAD: the funnel profile's 9.5 % `get_signal → task_work_run → __mutex_lock` term; with
`COOP_TASKRUN` task-work runs only when a task enters the ring — the reaper's own bounded wait,
where it belongs; the trade (lever `SQUEEZEFS_IPC_DD_COOP_TASKRUN`, `:288–299`): a CQE submitted by
a svc thread posts only at that thread's NEXT ring entry — one sweep away (231–375 µs pass cadence),
which the post-ACK-fast `device_cq` ledger reads as CQE-post deferral proportional to offered load;
`0` restores signal-delivered task-work.

PORT: `Ring128.init(512, IORING_SETUP_COOP_TASKRUN)` (`P:uring128.zig:184`) — NOT a `taskrun_ladder`
rung (`:682–687` all carry `SINGLE_ISSUER`); a two-rung dd ladder `{COOP_TASKRUN, 0}` with the
EINVAL degrade logged once. SQE128/CQE32 ride every `Ring128` (`:680–681`) — harmless here.

**Lane routing** (`:301–332`): a thread-local `DD_LANE` set by the svc loop
(`set_service_lane(idx)`); a foreign submitter gets a dense round-robin lane on first use
(`current_lane`); governed submits ride shard `lane % width`, so the shard partition mirrors the
session→owner partition and each shard's submit-side mutex is effectively uncontended (`:313–321`).

WHY shards at all (`:711–723`): the pre-shard single-ring + single unpinned reaper serialized every
CQE-side serve on ONE thread — the field's 208 k-IOPS-flat rand-4k il ceiling at 235 µs fabric RTT ×
256 in-flight while the kernel path spread the same work over per-CPU queues (273–280 k).

### 4.2 Reads: probe → submit → CQE

**Who enters:**

(a) `odirect ∧ direct_device_true` ⇒ every governed miss (DIALED P1); (b) a DEFAULT mount's O_DIRECT
miss that ran the FULL warm ladder and whose R1b admission decision DENIES (DIALED P1.5) —
"semantically a device-true serve: ranged device read, no tier publish, nothing to invalidate"
(`R:ipc_service.rs:1040–1061`, `R:ipc_direct.rs:5–18`).

`try_direct_drive_default` (`:1127–1178`):
1. `tier_admission == SecondTouch` only (`Policy`);
2. `ranged_eligible(file, len)` — the §5.6 threshold + stream-classification veto (`Policy`);
3. the probe;
4. a GRANT-shaped escalation candidate (`ranged_escalation_candidate` records the ghost TOUCH
   always, then the governor's NON-RESERVING peek) ⇒ handler (`Policy` — the admission fetch +
   publish stay there; `allow_escalation` remains the only reservation site);
5. DENIED ⇒ submit. Buffered bindings never enter (`:1060–1062`).

**The prelude** (`ipc_direct_read_probe`, `R:fuse_client.rs:15500–…`):
1. `4096 ≤ len ≤ 64 KiB` — the R3 ranged window class the 64 KiB bounce pool sizes (`:15516`;
   `Shape`);
2. passthrough only (`Meta` `:15521`);
3. strict in-bounds single block (`:15530–15541`);
4. `ServeStamp::begin()` BEFORE the binding resolves (`:15544`);
5. ONE zero-clone `metadata_cache.peek_with` (the former `get` cloned five Arcs per call
   `:15550–15555`): pre-step entry ⇒ `Meta`; striped ⇒ `end ≤ size`, RAM-resident map, mapped block
   (`Layout` on a hole), whole-block undecorated key (`Layout` on `bk:off:len`); staged ⇒ the PK8
   packed-tenant arm under `SQUEEZEFS_IPC_DD_PACKED` (`:15566–…`);
6. the overlay/staged-sibling/extent-record screen (ANY overlay ⇒ `Overlay`);
7. the 795 custody snapshot `{key, epoch, tracked, incarnation}` and parse-carry of `(be_id,
   dev_off)` so the submit re-parses nothing (`R:fuse_client.rs:8966–9020`; the probe's string
   builds were ~8 % of svc cycles `:8977–8979`, `parse_block_key` ~4.4 % `:9008–9011`).

Classes `Shape | Meta | Layout | Overlay | PackedShape | Backend | Policy` (`:8925–8958`); counters
`ipc_direct_ineligible_*`.

**Submit** (`DirectDriveEngine::submit`, `R:ipc_direct.rs:1026–1230`):
1. unknown/unhealthy volume ⇒ `backend` (`:1036–1047`);
2. window `[rel & !(LBA−1), min(ceil(rel_end, LBA), window_cap))` — the handler's ranged arithmetic
   verbatim; `window_cap` = block size on the striped arm, the tenant's grain-rounded slot on the
   packed arm so a neighbouring tenant's bytes are never DMA'd (`:1049–1062`);
3. **direct iff `window == req_len ∧ arena_ptr % 4096 == 0`** — the arena IS the DMA destination
   (zero daemon copies); else a 64 KiB `read_bounce_pool` buffer and ONE copy of the request slice
   at the CQE (`ipc_direct_drive_bounces` `:1064–1075`);
4. accounting: `ipc_direct_drive_submits`, `ranged_reads`/`packed_reads` + bytes,
   `read_device_true_reads` only on ddt mounts, `read_odirect_requests`, `note_foreground(window)`
   (`:1077–1119`);
5. `t_insert_ns` closes `admit` and anchors `inflight` — one clock read (`:1122–1129`);
6. `ensure_reaper(lane)` BEFORE the SQE publishes (`:1149–1160`);
7. under the shard `state` mutex: slab index from the free list, `Read` SQE (`Fixed(vol.fixed)` or
   `Fd(raw)`) at `dev_off + aligned_start`, `user_data = idx`; SQ full ⇒ stamp staged, `submit()`,
   retry once, else refuse (`backend`, submits un-counted `:1164–1223`);
8. `pending_submits++`; `≥ dd_eager_threshold` ⇒ `flush_shard` (`:1225–1228`).

**CQE** (`finish`, `:2031–2123`):
`exact = res == window` ∧ `ipc_direct_revalidate(&snap)` (binding still → key, custody epoch
unchanged, fill incarnation stable — charter §6.3 "validation AFTER the bytes are in hand"); exact ⇒
the bounce leg copies `window[win_skew..][..req_len]` into the arena (`read_fill_dma_bytes +=
window`), the direct leg counts `read_dest_dma_bytes += window`; `get_obj`, `ipc_ops_read`,
`ipc_bytes_out`, `ipc_direct_drive_serves` (+ `ipc_direct_packed_{serves,bytes}`),
`complete(req_len)`; ONE end-of-op clock read closes `finish` and `total` (`:2042–2097`); else
`ipc_direct_drive_fallbacks_post` and `spawn_read_handoff` — "the handler owns the truth
(never-lossy, never-fabricating)" (`:2098–2119`).

Single-flight is BYPASSED by design: ranged reads are not single-flighted; validity comes from the
post-DMA revalidation (`:47–57`).

PORT: `WarmLadder.directLegEligible` (`P:warm_ladder.zig:127–131`) is the shape half; the dd read
probe is `ReadPath`'s ranged-leg prelude returning a `Snapshot{key, binding, incarnation, epoch,
vol_tag, dev_off, window_cap}` with the SAME recheck hook `fsv.DirectLeg.hook` the zc bridge runs
(`:27–32`). The bounce pool = `read_fill_pool`'s 64 KiB class.

### 4.3 Lane-scoped flush, eager flush, the cadence governor

`flush()` (`R:ipc_direct.rs:1592–1619`): with `SQUEEZEFS_IPC_DD_LANE_FLUSH` (default ON) a svc
thread enters ONLY its own lane's ring; `0` = flush-all.

WHY (`:270–286`; `.benchmarks/2026-08-08-shim-drain-funnel-r3.md:38–41`, `:95–111`):
the flush-ALL sweep put every svc thread on every shard's kernel `uring_lock` whenever that shard
had unflushed SQEs — the funnel profile's #1 term, **31.2 % of svc cycles in
`mutex_spin_on_owner`/`osq_lock` under `io_uring_enter` at 32×32, 12 threads racing 12 rings**;
counted A-B-B-A (tcp devsub, 32-CPU, rand-4k il, 60 s): **32×8 931.3 k vs 666.7 k (+39.7 %), 32×32
1,185.6 k vs 926.6 k (+27.9 %)**, clat mean −28 %/−22 %, ingress mean 161 → 40 µs / 583 → 190 µs;
the CL0 isolation leg (lane-flush OFF on the new binary) gave most of it back (977 k at 32×32) — C1
carries the win; `COOP_TASKRUN` + the clock economy ≈ +5–7 %; liveness (`:277–282`): every
production submitter IS a lane owner, so its own sweep-end flush carries its SQEs; a straggler on a
foreign lane is carried by that lane's REAPER on its 100 ms cadence.

`flush_shard` (`:1680–1703`): `claim_and_stamp` swap-claims `pending_submits` (concurrent callers
never double-enter the same tail; a lost race is harmless `:1621–1626`), stamps every staged op's
`sq_wait` with ONE clock read (`:1630–1643`, `:1649–1678`), then `ring.submit()` with 3 retries on
EINTR/EAGAIN/EBUSY; a failed enter is loud and the reaper's own enter carries the published SQEs
(`:1684–1702`).

Eager mid-sweep flush (`:201–254`):
explicit `SQUEEZEFS_IPC_DD_EAGER_FLUSH = K > 0` enters once a lane's unflushed count reaches K; `0`
= sweep-only (one enter per sweep); ABSENT = the `CadenceCore` governed threshold (`:247–254`;
`ipc_dd_cadence_{k,probe_ups,probe_backoffs}`); history (`:231–246`): r4 showed per-lane device
concurrency bounded by issue CADENCE at fabric RTT (~18 per lane); the write-wall sweep found K=16 a
counted +4–8 % at qd32 (849/847 k vs 776/782 k), but `inflight/2` was FALSIFIED in-bracket, so per
the no-fixed-constants law the measured optimum ships only as the explicit lever and the default is
the probe governor (the write-pipeline pattern).

PORT: carry the governor (`write_pipeline.zig`'s probe core) and the lever; never a literal 16.

### 4.4 Reaper/drain fusion and the reaper's wait

`sqz-ipc-dd<N>` (`reap_loop`, `:1713–1850`):
1. pinned to the shard's node; `set_service_lane(idx)` so follower re-drives from its postludes ride
   its own shard (`:1715–1721`);
2. loop: exit iff `shutting_down ∧ inflight_count == 0` (`:1731–1739`);
3. `claim_and_stamp` — its enter carries any unclaimed tail (`:1740–1744`);
4. `reaper_wait` = `submit_and_wait(1)` bounded by `IORING_ENTER_EXT_ARG` 100 ms (`:1860–1884`):
   ETIME ⇒ `Ok(0)`; EINVAL ⇒ pre-5.11, degrade once to unbounded waits AND DISARM the inline-reap
   fusion arm — an inline-consumed NOP wake could otherwise strand the shutdown join (`:1869–1879`,
   `:789–795`);
5. `cq_gate.lock()` → `drain_cq_locked` (`:1839–1848`).

The 100 ms is a liveness re-check cadence, not tuning (the `SERVICE_PARK_MAX` posture; idle cost 10
wakes/s/shard `:1853–1859`).

**Fusion** (`SQUEEZEFS_IPC_DD_INLINE_REAP`, default ON; `:256–268`, `:1601–1618`):
after its flush, the owning svc thread `try_lock`s its lane's `cq_gate` and drains its own CQ INLINE
— a userspace CQ peek, zero syscall, zero context switch — where the dedicated reaper paid a
per-batch `io_uring_enter` wake + context switch per ~1.4 CQEs (0.72 enters/op + 2.36 ctx/op at the
32×8 ceiling shape); the reaper stays the blocking BACKSTOP; `try_lock` never blocks (contention
means the reaper is already draining); other lanes' CQs are never touched; disabled while shutting
down — teardown CQEs belong to the reaper's drain contract (`:1577–1584`); counter
`ipc_direct_inline_reaps`; lock order `cq_gate` then `state`, never the reverse (`:730–737`).

**`drain_cq_locked`** (`:1892–2025`):
1. collect every CQE; `user_data == NOP_WAKE` skipped (`:1919–1925`);
2. per op: take the slab entry under `state` (free list push, `inflight_count−−`; an unknown slot ⇒
   loud drop `:1926–1945`);
3. ONE clock read closes `inflight` and `device_cq` — anchor = `t_flush_ns` or, for the racer class
   stamped 0, `t_insert_ns`, so `sq_wait + device_cq` never double-counts (`:1946–1973`);
4. `finish` / `finish_write(…, &mut batch)` (`:1974–1977`);
5. the batch tails: ONE coalesced durable-times handoff per batch (ino-deduped, max stamp;
   `:1981–2000`; `ipc_dd_write_times_dispatches`), then the train pumps (`:2002–2004`), then the
   cadence census (`:2005–2023`).

The ACK-fast drain law (`:1899–1908`): every pre-ACK step stays inline per op; the HEAVY tails
(times handoff; follower re-drives = SQE prep + inline nvme-tcp TX) defer past the LAST ACK, so a
CQE popped mid-batch never waits behind a sibling's tail.

**Stall/leak law** (MEM-7c, `:1722–1836`): a failed enter while ops are in flight must NEVER exit
the reaper — every pending op PINS its session mapping; exiting lets `shutdown` join and the mapping
release while kernel DMA can still land in it. Retry at 1 ms, `ipc_direct_reap_stalls`; past
`REAP_STALL_LIMIT = 5_000` consecutive stalls LEAK the in-flight entries (`mem::forget` — their
pages stay mapped for the process's life), close the wedged trains and fail every parked follower
LOUD with EIO (they have no DMA in flight), then exit. Unmapping under DMA is never an option.

PORT: `ring_park.Parked(ParkFace)` (`P:ring_park.zig:117–`) is the owner-thread park primitive; the
dd reaper's wait is `Ring128.submitAndWaitTimed(1, 100 ms)` (`P:uring128.zig:345`) — the EXT_ARG
form already exists. `cq_gate` is a `std.Thread.Mutex` (sanctioned: a short try-lock gate between
two OS threads; no fiber ever parks on it).

### 4.5 The W1-shaped write served inline under a try-locked block stripe

The ruling (`R:docs/design-il-direct-write.md:10–20`): "why are we chasing 1 M write IOPS at the
kernel level when our read IOPS are at the IL/SHIM level?" — an il write paid the ring hop AND the
full handler. On the engaged il rand-4k row (277.5 k IOPS, qd32 t32) the handler lanes took 77.8 %
of CPU (46.4 µs/op) vs 7.8 % on `sqz-ipc-svcN` (`:22–31`); over half the lane cost was RAM-tier
bucket contention (22.1 %), clock ceremony (23.5 %) and `clear_page_erms` (6.3 %), not write work
(`:33–41`). Target: execute the W1 shape ON the ipc lane, device-true, no handler handoff (`:18–20`,
`:43–61`).

**Gate** (`serve_write`, `R:ipc_service.rs:1207–1233`): `SQUEEZEFS_IL_DIRECT_WRITE` (default ON; a
runtime cell — `getenv` per op contended the std env lock `:127–152`) ∧ `patch_max_bytes() != 0` ⇒
`drive_direct_write`; `Err` ⇒ the sever→handoff path with the class counted.

**Probe** (`ipc_direct_write_probe`, `R:fuse_client.rs:15874–15913`):
The lock-free shape half (`dd_write_shape_probe`, `:15937–15982`):
1. `len != 0`, not a virtual ino (`Shape`);
2. `len ≤ patch_max_bytes` — over the cap belongs to placement/accumulation (`Shape`);
3. `offset % 4096 == 0 ∧ len % 4096 == 0` (`Align` — "v1 is LBA-aligned-only BY CONTRACT (crash
   blast radius: only app-written sectors are ever rewritten)");
4. single block (`Shape`);
5. no write verification — the pooled vehicle's read-back is the verifier (`Shape`);
6. passthrough crypto (`Shape`).

Then **ONE `BLOCK_FLUSH_LOCKS.try_lock()` — never a blocking acquire**; contended ⇒ `BlockLock`
(`:15892–15899`); the holder is stamped for the watchdog (`:15900–15907`).

The state half under the held guard (`dd_write_state_probe`, `:15989–16118`):
1. striped; `end ≤ size` (extending owes a meta commit); RAM-resident map; mapped block; whole-block
   undecorated key (`:16001–16024`);
2. NO overlay of any kind — active buffer, staged active block, staged extent record, open
   device-overlay record (`:16027–16042`; W2's law: an in-place patch under one would be folded
   over);
3. not stream-adjacent (`last_write_end == offset` ⇒ `Shape` `:16045–16051`);
4. custody RAM-only: a cached, local, currently-held lease (`Lease`); whole-inode exclusive range
   (`Range`); a `kill_priv` peer only under the HELD `killpriv_clean` latch (`Killpriv` — the
   clearing is async metadata work the handler runs BEFORE its data lands, the VFS order; one
   fallback latches it `:16077–16084`);
5. `(be_id, dev_off)` parse-carry (`:16087–16089`); on an armed symmetric set the block's allocator
   must hold the ownership plane (`:16095–16100`);
6. COMMIT: `note_last_write_end` — the coverage bookkeeping for THIS shape (`record_write` lives on
   accumulation buffers the overlay screen excluded) — and `mark_handle_dirty` (`:16104–16105`;
   `R:docs/design-il-direct-write.md:120–125`).

The snapshot (`IpcDirectWriteSnapshot`, `R:fuse_client.rs:9049–9067`) travels WITH the held guard;
there is no CQE-side revalidation ladder because "the mapping cannot move while the DMA is in
flight" under (3) — the same protection `try_sole_owner_patch` runs under (`:9043–9048`,
`design-il-direct-write.md:132–137`).

**Submit** (`submit_write` → `submit_write_core`, `R:ipc_direct.rs:1300–1569`):
1. `trains.arm(key)` (the conveyor, below);
2. volume present ∧ `writable` ∧ healthy, else `backend` (`:1377–1389`);
3. `ensure_reaper(lane)` (`:1391–1394`);
4. DMA source: the arena window verbatim when `arena_ptr % 4096 == 0`, else the §5.5.2 sever into a
   4 KiB-aligned `BUFFER_POOL` slab (ONE copy; "a racing client scribble on the live-arena leg is
   torn CONTENT on the device — the client's own POSIX hazard; §5.3.1 rule 2's severed-copy
   discipline exists for DERIVED values; the payload is uninterpreted bytes DMA'd exactly once"
   `:1396–1427`);
5. **§5.1 steps 1a+1b: `allocator.begin_patch_sole_owner(dev_off)` = mark unstable → `fence(SeqCst)`
   → refcount re-check**; a pinned clone ⇒ `publish_block` (re-stabilize, content unchanged) +
   `ipc_dd_write_ineligible_fence_backoff` + handler fallback (`:1429–1445`);
6. **RES-6: `device.authorize_zc_store()` strictly BEFORE the SQE**; a fence refusal re-stabilizes,
   purges the 4 tiers + both LRUs, `ipc_dd_write_fence_refusals`, logs, and completes `-errno` LOUD
   — "a fenced holder must not fall back to a second submission path" (`:1446–1468`);
7. `t_insert_ns` (`admit` closes `:1470–1481`);
8. under the shard mutex: slab entry + `Write` SQE at `dev_write_off`; SQ-full retry-once else
   re-stabilize + `backend` fallback (`:1497–1562`);
9. eager flush (`:1564–1567`).

Every non-submitted outcome hands the GUARD BACK (`DdWriteSubmit::{Refused, Fallback}` `:661–673`)
so the caller owns the train disposition.

**CQE postlude** (`finish_write`, `:2133–2271`; order copied from `try_sole_owner_patch` + the
handler's tail):
1. BOTH exits: `publish_block(dev_off)` — the §5.1 word re-stabilizes under a NEW generation; the
   4-arm `purge_block_key`; the stale whole-file LRU drops — no allocate, free, map merge, journal
   or staging (`:2153–2163`).
2. Exact ⇒ W-5 `note_fsync_touched_keys` — an in-place DMA changes no map key; stamp or the next
   fsync skips its barrier (`:2171–2177`).
3. `patch_writes`/`patch_write_bytes` (a direct write IS a W1 write in the ledger),
   `ipc_dd_write_{serves,bytes}`, `ipc_ops_write`, `ipc_bytes_in` (`:2178–2192`).
4. The WriteTimes `publish_attr` through the identical-publish elision door with `size_claim: None`
   — non-extending by eligibility; a size-neutral completion must never resurrect a stale-high size
   (`:2198–2212`).
5. Killpriv: structurally nothing to clear (`:2213–2217`).
6. The W1 inval `on_write(ino, offset + len)` (`:2218–2221`).
7. **ACK** (`:2223`); `finish`/`total` closed by one clock read; the durable-times refinement pushed
   into the batch (`:2235–2241`).
8. Short/failed DMA ⇒ `patch_dma_errors`, loud warn, complete `res` or `-EIO` — nothing acked,
   parked or lost (`:2242–2260`).
9. Drop the bounce; drop the op — the arena pin releases only after the last DMA byte was consumed
   (`:2261–2265`); push `(train_key, block_guard)` to the batch (`:2266–2270`).

**Trains — the per-block follower conveyor** (`WriteTrains`, `:401–596`; the rig's 99.8 %
`block_lock` attribution):
`TrainState{open, tenure, queue}` per `(ino, block)` ever contended, never removed (`:438–447`);
`open` is the LANE-HOLDER authority — true exactly while a dd-write leader holds the block's guard
(armed at probe success, closed on EVERY release path), so a park can never wait behind a FOREIGN
holder (handler/fold/flush/truncate) (`:420–426`); `try_park`: a `BlockLock` refusal whose holder is
the lane itself (`ipc_dd_write_block_parks`); `TRAIN_BOUND = Q_DEPTH_DESIRED` ops per tenure AND the
queue capacity — one ring depth's worth, so a fold/flush/handler waiter parks behind at most one
train (`:401–407`, `:493–509`); `pop_under_tenure` pops the next follower while `tenure < BOUND`,
else CLOSES (`:518–537`; `ipc_dd_write_park_redrives`); `run_train` (`:2289–2394`) re-drives each
follower UNDER the already-held guard via `ipc_direct_write_probe_locked` (no try_lock; the world
may have moved ⇒ honest arm + handler) and `submit_write_core`, flushing its SQE immediately — no
sweep-end flush runs on the reaper's cadence (`:2346–2353`); At close: drop the guard and drain
residuals through the NORMAL probe — a residual that wins the fresh try_lock becomes the next
leader; a `Rearmed` observation STOPS the drain (the re-park livelock fix: the drain ran under
`cq_gate`, a popped residual re-parked onto the re-armed train it was popping — 2.2 B park/redrive
pairs, 160 s stripe waits at qd12+ `:449–460`, `:548–578`); `pop_any` = the ring-death leak arm only
(`:580–596`).

**Parity with the handler** (`R:docs/design-il-direct-write.md:63–70`, `:110–147`): the D0 fence
(`authorize_zc_store`) and the §5.1 fence pair both run; coverage = the stream word; killpriv = the
latch gate; KD-11 holds by construction (the lane IS write-through); phase spans ride
`ipc_direct_phase_ns`, not a separate family (`:117–119`); the durable times park is the ONE async
postlude member, dispatched after the ACK (`:139–144`).

PORT:
Split `sole_owner_patch.tryPatch` (`P:sole_owner_patch.zig:211–264`) at the DMA: `beginPatch(deps, req)
PatchError!?Begun{vol, idx, dest, len}` = the custody probes, the authoritative map lookup, the
durable sole-owner verdict and the fence pair (`:214–236`); `finishPatch(deps, begun, dma:
DmaError!DmaProof)` = `publishBlock`, `purge`, the touched stamp, the counters, the EIO law
(`:245–263`); The handler's inline arm calls both around `deps.device.write(…)`; the dd lane calls
`beginPatch` on the svc thread, prepares `prepWrite(sqe, .{.fixed = vol}, src, dest)`
(`P:uring128.zig:556`) on its shard ring, and `finishPatch` at the CQE; The block guard =
`BlockLocks.tryAcquire(io, ino, block)` (`P:write_seams.zig:360–366`) with a new
`LockSite.il_direct_write` for the watchdog's holder stamp; HELD across the SQE, released in the
postlude (level 3 only; nothing acquired under it — the extended P1-9 order
`R:ipc_direct.rs:386–389`); The train registry is a striped map `(ino, block) → TrainState` with a
bounded ring buffer of `TRAIN_BOUND` entries — allocation-free in steady state;
`write_admit.WritePath` exposes `dirtyHandle(ino)` and `last_write_end.note` for the probe's commit
step.

### 4.6 libaio batch decomposition — what the daemon sees

The shim's `io_submit` interposer classifies every iocb independently
(`R:crates/squeezefs-preload/src/interpose.rs:2259–2342`):
an unbound fd, a `RWF_*` passthrough flag, `IOCB_FLAG_RESFD`, a size the hybrid lane gate routes to
the kernel (`kernel_lane_route(nbytes, min)`), or a multi-slot op (`nbytes > slab` — "a multi-slot
async op would hold slots hostage") ⇒ `Kernel`; the rest ⇒ `Ring` (`:2300–2325`); a mixed batch is
split by `AioCtxState::submit_batch` with the prefix law preserved (`:2330–2341`); a pure-kernel
batch is the real call verbatim (`:2327–2329`).

**The daemon sees ONE ring op per Ring-class iocb** — a slot claimed, a descriptor filled from
`(aio_fildes → binding, offset, nbytes, buf → arena slab)`, pushed without waiting (an
`OpTicket{slot, gen}` the reaper later polls, `R:crates/squeezefs-preload/src/session.rs:107–116`).
Nothing in the daemon knows about batches: no batch id, no ordering obligation, no `io_event` —
`io_getevents` is the client merging per-slot DONE states with kernel events.

The daemon's only batch-shaped behaviour is the completion doorbell's batch-wake mark: the reaper
parks with `k = reap_batch_wake_threshold(pending) = clamp(pending/4, 2, pending)`
(`R:sizing.rs:185–191`; FLOOR, not ceil — a later wake is the unsafe direction) and the daemon
elides wakes toward it until the seq reaches the mark, ≤ 1 wake per park era under the latch
(`R:cqe_core.rs:45–60`, `:62–98`).

Measured (OQ-1, `R:docs/design-preload-interception.md:569`): fio libaio jobs=16 qd=32 **633,183
IOPS device-true**, beating `-t 256` sync threads' 622,112 at 1/16th the client threads.

### 4.7 Completion ordering

None is promised across ops:
a slot completes when ITS CQE is drained (reads `finish`, writes `finish_write`); the shard CQ is
consumed in CQE order by whichever consumer holds `cq_gate`; two CQEs of one client thread can
complete in either order; a client op spanning chunks "pipelined — all submitted before any is
waited on, reaped in offset order with POSIX prefix semantics" is the SHIM's job
(`design-preload-interception.md:254`).

Within one op: `ledger.finish` → `set_result` → `state.swap(DONE)` → futex wake → cqe bump
(`R:ipc_host.rs:411–459`) — the result is visible before DONE (the slot core's
publish-then-recheck), the doorbell after. The dd write's ACK follows every pre-ACK postlude step
(re-stabilize, purge, attr publish, inval enqueue `:2153–2223`); the times refinement and the next
follower's re-drive run AFTER the batch's last ACK (`:1981–2004`).

---

## 5. Arena lease, refcounted unmap, heartbeat, teardown

### 5.1 The lease and the unmap

`SessionMapping` (`R:ipc_host.rs:815–907`) is an `Arc`:
every `ArenaWindow` (`:572–577`), every `SlotCompletion` (`:398`), every svc pass clone it; `Drop`
runs `munmap(base, total_bytes)` — "ordered after every accessor by Arc ownership" (`:897–907`;
§5.3.1 rule 4 "early unmap is unrepresentable"); a completion racing teardown "writes into live
private memory, never a freed mapping" (`:392–396`); a kill-9 mid-DMA on the dd lane never unmaps
under the CQE because the `Pending` holds the `DataOp` (`R:ipc_direct.rs:73–77`, `:344–347`); the
THP-prep queue holds a `Weak` so the queue never owns a dead session's arena — 256 × 120 MiB ≈ 30
GiB held past teardown → R5 Red was the field capture (`:1319–1332`).

PORT: `SessionMap{ refs: Atomic(u32), base, len }` with `retain()`/`release()` (acq_rel on the last
drop pairing with the accessors' acquire — name the pair on the line); `ArenaWindow` and
`SlotCompletion` hold a retained pointer; `release()` at 0 ⇒ `munmap`. In-flight dd entries and
handoff fibers retain; the teardown's last release is therefore after the last CQE and the last
fiber by structure; the session's `Io.Group` join (§5.4) is only the WAIT, not the safety.

### 5.2 Heartbeat

The header carries `heartbeat: AtomicU64` — "daemon heartbeat word (client-side death detection,
§5.7)" (`R:layout.rs:316–317`, initialised 0 `:352`) — but **no daemon code ever writes it** (no
`.heartbeat` reference in `ipc_host.rs`, `ipc_service.rs` or the shim).

The shipped death detection is:
the shim's per-op deadline `SQUEEZEFS_IL_OP_TIMEOUT_MS` (default 30 s,
`R:crates/squeezefs-preload/src/session.rs:40–42`) ⇒ poison; the ctl socket EOF/ECONNRESET; the
header `generation` bump (`:425–426`, `:766–767`) the daemon performs on poison
(`R:ipc_host.rs:2586–2590`) and idle reap (`:1552`).

§8 quirk 2.

### 5.3 Idle reap, shed, poison

`sqz-ipc-reap` ticks every `clamp(idle_ms/4, 100 ms, 5 s)` and tears down sessions idle ≥
`SQUEEZEFS_IPC_IDLE_SECS` (default 300; 0 disables) with a generation bump — counted
`ipc_sessions_reaped`, never the poison tripwire (`:1527–1557`); activity = served ops (`:2882`) and
BINDs (`:2000–2003`); `shed_to(target)` (the R5 shed hook) refuses NEW sessions until the gauge is ≤
target, one-shot, never tears a live session (`:1614–1621`, `:2273–2283`); the `ipc_session_arenas`
component: floor 0, weight 1, non-reclaimable (`design-preload-interception.md:404`);
`poison_session` = `ipc_sessions_poisoned` + loud log + generation bump + teardown (`:2578–2592`).
Causes: an out-of-range or non-SUBMITTED published slot index, `live > slots`, a BIND without an fd,
an unexpected or malformed ctl datagram (`:971–1003`, `:2027–2030`, `:2049–2057`).

### 5.4 Session teardown and what a dead client leaves

`connection_loop` ends on socket EOF — client death, reliable even with forked descendants because
the atfork child closes its copy (`design-preload-interception.md:396`) — → `teardown_session(…,
"ctl socket EOF")` (`R:ipc_host.rs:2053`, `:2061`).

`teardown_session` (`:2511–2573`), exactly once via the `torn_down` swap:
1. clear bindings; shut the socket (`:2516–2517`);
2. under ONE registry lock hold: remove the session and FOLD its client stats page (lane-gate
   routes/bytes, the wake-economy trio) into the host accumulators, so a concurrent snapshot counts
   the session on exactly one half (`:2518–2545`);
3. bump `session_epoch` — a stale svc snapshot drains it at most once more, safely (`:2546–2549`);
4. uid count; `arena_bytes` and `ipc_arena_bytes` down by the charged footprint;
   `ipc_sessions_active−−`; `release_owner` (`:2550–2570`);
5. the mapping unmaps when the last accessor drops (`:2571–2572`).

What a dead client leaves:
in-flight ops complete into the still-mapped arena — "acked-to-nobody is harmless; state is
daemon-owned" (`design-preload-interception.md:396`); abandoned slots are GC'd with the session
(`:402`); the Invalidator's per-ino entries and the train registry entries stay (recorded ceilings,
`R:ipc_service.rs:577–587`, `R:ipc_direct.rs:438–447`); the severed-pool and bounce buffers recycle
on their payload drops; zero persistent residue: memfd + abstract socket (`:396`); the path socket
file is unlinked at host shutdown (`R:ipc_host.rs:1634–1641`).

Host `shutdown` (`:1624–1696`):
1. flag;
2. `shutdown(2)` both listeners — closing an fd does NOT unblock a blocking accept; remove the path
   socket;
3. sever EVERY live ctl connection (VAL-5b — a lingering or silent client must never wedge teardown;
   `CTL_RECV_POLL` 500 ms is the backstop re-check `:90–96`; `CTL_HANDSHAKE_TIMEOUT` 10 s drops an
   unestablished peer `:98–102`);
4. tear down every session; drop the THP sender;
5. unpark every thread — the reap tick is up to 5 s and sleeping it out added a flat 5 s to every
   interception umount (`:1680–1686`); join;
6. `sink.shutdown_threads()` → `DirectDriveEngine::shutdown` (`R:ipc_direct.rs:2400–2428`): flag,
   take each spawned reaper's handle under the reaper mutex (serialized with `ensure_reaper`),
   NOP-wake its ring, join — each reaper drains its in-flight CQEs first, bounded by device latency.

RES-12: the join runs inside the host's shutdown hop (the daemon runs it on the blocking pool), not
on whichever lane drops the last sink (`R:ipc_service.rs:779–806`).

PORT: ctl threads and the accept thread are `exec.blocking` jobs (`P:blocking.zig:147 submit`) or
plain threads with `recv` timeouts — never lane fibers; the per-session `Io.Group`
(`P:sqz_io.zig:458–474`) joins the handoff fibers at teardown; the dd shards are owned by the il
host and shut down in the same order (sessions → svc threads → reapers).

### 5.5 Where the fd screen runs and what it unlocks

The screen (`screen_fd`, `R:ipc_host.rs:2452–2505`) runs on the CTL THREAD at HELLO (the credential
fd — `:2259–2264`) and at every BIND (`:1987–2026`); its rules are the sibling spec's. What it
unlocks here:
a `BindingRights{ino, read_ok, write_ok, odirect, kill_priv}` row in the session's latch-free
binding table (`:1990–1998`); `read_ok/write_ok` gate the per-op `EBADF` (`:1100–1117`); `odirect`
is the READ class the dd probe and the handler receive (`:343–357`; dropping it reclassified
O_DIRECT ring reads as buffered — the measured 13× il-vs-kernel collapse on cold working sets
`R:ipc_service.rs:250–257`); `kill_priv` is the write flag (§3.4).

The HELLO additionally unlocks the owner pick, the svc thread spawn and the memfd (§1.2);
`SessionOk` is sent only after the admission histogram sample and the prep enqueue, so anything a
client can observe after it is already visible (`:2412–2429`).

---

## 6. Knobs, gauges, constants

### 6.1 Knobs (class per the port's `knobs.Class`, `P:knobs.zig:57–69`)

| Knob | Kind / range | Default | Class | Doc (reference row) |
|---|---|---|---|---|
| `SQUEEZEFS_IPC_SERVICE_THREADS` | int 1..=64 | `il_drain_lanes_default(cpus)` | derived_override | svc-thread ceiling, spawn-on-admission (`R:env_knobs.rs:281`; code clamp `ipc_host.rs:246`) |
| `SQUEEZEFS_IPC_DD_SHARDS` | int 1..=64 | same function | derived_override | dd shards; one lane per svc thread (`:282`; `ipc_direct.rs:186`) |
| `SQUEEZEFS_IPC_MEM_MAX` / `_MEM_PCT` / `_ARENA_MB` | see `P:knobs.zig:114–116` | derived | derived_override | already landed in the port |
| `SQUEEZEFS_IPC_IDLE_SECS` | int 0..=86_400 | 300 | derived_override (0 = off) | idle reap (`:276`) |
| `SQUEEZEFS_IPC_INVAL_WINDOW_MS` | int 1.. | 1000 | ab_lever | W1 inval coalescing window (`:277`) |
| `SQUEEZEFS_IPC_MAX_OP_BYTES` | int 1.. | 1 MiB (`layout.rs:96`) | derived_override | per-op payload ceiling (`:278`) |
| `SQUEEZEFS_IPC_SPIN_US` | int 0..=10_000 | absent = governor | ab_lever | explicit wins verbatim incl. 0 (`:287`; code clamp `ipc_host.rs:163`) |
| `SQUEEZEFS_IPC_SPIN_ADAPTIVE` | bool | on | ab_lever | the spin governor arm (`:288`) |
| `SQUEEZEFS_IPC_DD_EAGER_FLUSH` | int 0..=512 | absent = governed | ab_lever | K > 0 verbatim; 0 = sweep-only (`:283`; clamp to `RING_ENTRIES` `ipc_direct.rs:228`) |
| `SQUEEZEFS_IPC_DD_INLINE_REAP` | bool | on | ab_lever | reaper/drain fusion (`:284`) |
| `SQUEEZEFS_IPC_DD_LANE_FLUSH` | bool | on | ab_lever | lane-scoped flush; 0 = flush-all (`:294`) |
| `SQUEEZEFS_IPC_DD_COOP_TASKRUN` | bool | on | ab_lever | dd ring flag (`:295`) |
| `SQUEEZEFS_IPC_DD_PACKED` | bool | on | ab_lever | packed-tenant dd read arm (`:285`) |
| `SQUEEZEFS_IPC_CQE_WAKE_LATCH` | bool | on | ab_lever | completion-doorbell wake collapse (`:298`) |
| `SQUEEZEFS_IL_DIRECT_WRITE` | bool | on | ab_lever | the dd WRITE lane; 0 = sever→handoff for every write (`:301`) |
| `SQUEEZEFS_IL_READ_DEST` | bool | on | — | arena-dest read serves (`:300`); not carried — the handler always serves into `Ctx.window` (§8 quirk 9) |
| `SQUEEZEFS_IPC_ALLOW_DEV`, `SQUEEZEFS_IPC_SOCKET_DIR` | — | — | sibling spec | admission + rendezvous rows |
| `SQUEEZEFS_IPC_ARENA_THP` | bool | on | ab_lever | §8 quirk 7 — one `madvise` at creation, no worker |
| `SQUEEZEFS_TEST_DDW_CQE_HOLD` | bool | 0 | test_seam | the reference's `set_test_ddw_cqe_hold` seam (`R:ipc_direct.rs:116–132`): pauses CQ consumption so a leader's tenure stays open and followers deterministically park |

Shim-side (`SQUEEZEFS_IL_*`: `SESSIONS`, `SPINS`, `MAX_RUN_SLOTS`, `OP_TIMEOUT_MS`, `REAP_PARK_MAX`,
`SPARSE_BATCH_MARKS`, `REAP_QUANTUM_US`, `KERNEL_LANE_MIN`; `R:env_knobs.rs:290–302`) are the
sibling spec's; they appear in the daemon's registry only because the ONE registry owns every
`SQUEEZEFS_*` literal.

### 6.2 Gauges and counters (`R:fuse_client.rs` METRICS: 87 `ipc_` fields + `placed_*`, `il_*`)

Engagement identity (charter §7): **an il row is INVALID unless `Δ(ipc_ops_read + ipc_ops_write)` ≈
the row's ops** (`design-preload-interception.md:491`). With the hybrid gate: `ipc_ops_* +
ipc_lane_gate_kernel_routes ≈ ops` and `ipc_bytes_* + ipc_lane_gate_kernel_bytes ≈ bytes`
(`design-one-path.md:680`).

Per-lane closures:
reads: `fast_path_serves + async_handoffs(read) + direct_drive_serves ≡ ipc_ops_read`; writes:
`dd_write_serves + async_handoffs(write) ≡ ipc_ops_write` (`design-il-direct-write.md:92–94`); dd:
`direct_drive_serves + fallbacks_post ≡ submits`; hold probe: `serves + misses ≈ probes`.

| Family | Members | Signal |
|---|---|---|
| sessions | `ipc_sessions_{active,total,reaped,poisoned}`, `ipc_session_owners`, `ipc_service_threads`, `ipc_session_admission_ns` (histogram) | leaks; `poisoned` must-stay-0; owners < width under load = lane concentration |
| binds | `ipc_binds`, `ipc_bind_refused_{version,flags,mode,budget,nonce,peercred,disabled}`, `ipc_binds_dev_override`, `ipc_admission_refusals` | the refusal ledger |
| volume | `ipc_ops_{read,write}`, `ipc_bytes_{in,out}` | THE engagement proof |
| fast path | `ipc_fast_path_serves`, `ipc_fast_path_{lock,miss}_demotions`, `ipc_async_handoffs`, `ipc_hold_probe_{serves,misses}`, `ipc_read_dest_serves`, `ipc_arena_copy_bytes` | rot vs writers (§2.1) |
| drain | `ipc_drain_pass_ns`, `ipc_drain_flush_ns`, `ipc_drain_empty_passes`, `ipc_ingress_ns`, `ipc_service_parks`, `ipc_spin_{window_us,absorbed_parks,disengaged_busy}` | ops/pass; the funnel (`drain-funnel-r3.md:12–18`) |
| wake economy | `ipc_cqe_wake_{writes,elided,collapsed}`, `ipc_cqe_pass_wake_flushes`, `il_submit_harvested`, `il_park_eras`, `il_slot_reroutes` (client page, display-only) | `writes/(writes+elided+collapsed)` ≤ 0.25 at 32×8 (`design-preload-interception.md:496`) |
| severance | `ipc_severed_pool_{hits,misses,bytes}`, `ipc_placed_severs`, `ipc_placed_sever_fallbacks`, `placed_adoptions`, `placed_adoption_refusals`, `placed_merge_elides`, `placed_assembly_bytes` | miss growth on a stream = the fault engine is back |
| dd reads | `ipc_direct_drive_{submits,serves,bounces,fallbacks_post}`, `ipc_direct_ineligible_{shape,meta,layout,overlay,packed_shape,backend,policy}`, `ipc_direct_packed_{serves,bytes}`, `ipc_direct_shards`, `ipc_direct_inline_reaps`, `ipc_direct_reap_stalls`, `ipc_direct_phase_ns{admit,inflight,finish,total,sq_wait,device_cq}` | `total ≈ admit + inflight + finish`; `inflight ≈ sq_wait + device_cq` (`R:fuse_client.rs:4190–4231`) |
| dd writes | `ipc_dd_write_{serves,bytes,fence_refusals,block_parks,park_redrives,times_dispatches}`, `ipc_dd_write_ineligible_{shape,lease,range,block_lock,killpriv,overlay,backend,align,fence_backoff}`, `ipc_dd_cadence_{k,probe_ups,probe_backoffs}` | the decision ledger; `fence_refusals` loud |
| coherence | `ipc_inval_{notifies,attrs_only,suppressed}` | unbounded growth = the limiter broke |
| tripwires | `ipc_descriptor_rejects`, `ipc_sessions_poisoned` | must-stay-0 in production: client bug or attack |
| budget | `ipc_arena_bytes`, `ipc_severed_pool_bytes`, `placed_assembly_bytes` | R5 components |

PORT: per-op counts are `counters.ShardedCounter`, levels `Gauge`, phases `ShardedLatencyHistogram`
(exact-sum), the two tripwires `Tripwire`. The three `il_*` client-page words and `ipc_lane_gate_*`
stay display-only sums — never a control input.

### 6.3 Constants (each with its WHY on the line)

| Constant | Value | WHY | Ref |
|---|---|---|---|
| svc/dd width | `clamp(ceil(3·cpus/8), 2, 64)` | §1.1 counted sweep; lane pair = ¾ core budget | `sizing.rs:100–105` |
| `SERVICE_PARK_MAX` | 5 ms | §5.3.1 rule 5 / D18 ladder cap: no wait on a client word exceeds it | `ipc_host.rs:86–88` |
| spin clamp | 0..=10 000 µs | the explicit lever's rail | `:163` |
| `RING_ENTRIES` | 512/shard | ≫ per-lane governed depth (≤ 1024/8 lanes at t32qd32) | `ipc_direct.rs:165–169` |
| reaper wait | 100 ms EXT_ARG | liveness re-check cadence (the fusion arm may consume the NOP wake) | `:1853–1862` |
| `REAP_STALL_LIMIT` | 5 000 × 1 ms | MEM-7c: leak rather than unmap under DMA or hang the join | `:1722–1728` |
| `TRAIN_BOUND` | `Q_DEPTH_DESIRED` (32) | one ring depth per tenure: a foreign waiter parks behind ≤ 1 train | `:401–407` |
| `LBA` | 4096 | the ranged window grain (= the handler's) | `:334–336` |
| dd read class | 4 KiB ≤ len ≤ 64 KiB | the R3 ranged window class the bounce pool sizes | `fuse_client.rs:15513–15516` |
| `CLAIM_PAGE` | 4096 | placed severs page-aligned by contract | `placed_core.rs:43–46` |
| assembly cap | `min(budget/8, 2 GiB)` | the session-shm cap shape; the rail alone pre-arm | `placed_sever.rs:42–56` |
| severed-pool slots | `clamp(arena_cap/max_op, 1, 65536)` | the structural in-flight bound; the rail caps the pointer array | `ipc_host.rs:741–744` |
| ctl conn cap | `clamp(2·(arena_cap/footprint), 32, 4096)` | sessions the budget can hold ×2 for in-flight handshakes; the thread rail | `:250–269` |
| listen backlog | `clamp(ctl_cap, 64, 4096)` | the hardcoded 64 convoyed a 256-process fleet (~150–190 ms/process) | `:271–286` |
| `NONCE_TTL` | 60 s | current + previous accepted | `:82–84` |
| `CTL_RECV_POLL` / `CTL_HANDSHAKE_TIMEOUT` | 500 ms / 10 s | teardown liveness; matched to the shim's ctl deadline | `:90–102` |
| idle reap tick | `clamp(idle/4, 100 ms, 5 s)` | 4 ticks per window | `:1534` |
| inval window | 1 s | W1 rate limit; attrs-only exempt | `env_knobs.rs:277`, `ipc_service.rs:546–549` |
| `KERNEL_LANE_RTT_DELTA_NS` | 2 300 | 1/253 k − 1/594 k from the D14 rand-4k bracket | `sizing.rs:107–116` |
| kernel lane min | `memBW × 2.3 µs / 2 copies`, 4 KiB grain, clamp `[slab, max_op]` | the hybrid gate (shim-side); ≈ 140–164 KiB field | `:127–164`, `design-one-path.md:85` |
| batch wake k | `clamp(pending/4, 2, pending)` | liveness cap dominates; FLOOR (a later wake is unsafe) | `sizing.rs:166–191` |

Measured headlines (`.benchmarks/2026-07-19-l4-interception-closing.md:20–21`):
**il-warm 1,017,548 IOPS** (971,709 / 1,017,548 / 1,020,676; kernel-FUSE warm 644,726 ⇒ 1.58×);
**il-device-true 622,112** (523,995 / 622,112 / 642,504; kernel-FUSE dt 604,313); later campaigns on
the devsub: 1,246,011 at 32×32 with lane-scoped flush (`drain-funnel-r3.md:86`), sustained 1,223,389
over 90 s (`:90–92`); writes: 277.5 k engaged il rand-4k before the dd write lane
(`design-il-direct-write.md:24`); the lane's bracket is owed (`:146–151`).

---

## 7. Tests

Legend: ✦ = needs a real mount and the shim; ◆ = device/venue; unmarked = portable. One sentence
where the name does not already say it.

**Host / sessions** (`R:tests/ipc_host_tests.rs`): `happy_path_bind_serves_echo_and_only_echo` (ECHO
round-trips through a svc thread; READ/WRITE refuse ENOSYS on the echo sink);
`wrong_direction_ring_ops_refuse_ebadf_both_directions`;
`resubmitting_slots_past_the_in_flight_bound_poisons_the_session` /
`honest_in_flight_up_to_the_slot_count_never_poisons` (VAL-5c);
`a_saturating_session_cannot_starve_its_service_thread_siblings` (VAL-5e budget + rotation);
`session_teardown_on_eof_frees_arena_and_gauges`; `shed_refuses_new_sessions_never_tears_live_ones`;
`a_silent_connection_cannot_wedge_host_shutdown`;
`an_established_but_idle_connection_survives_the_recv_timeout`;
`a_connect_storm_is_capped_at_the_derived_ctl_conn_bound`;
`finished_ctl_threads_do_not_accumulate_join_handles`;
`kd11_interception_forces_write_through_and_refuses_explicit_writeback`;
`two_colocated_hosts_discover_by_mount_and_never_cross`. Admission balance
(`ipc_admission_balance_tests.rs`): `sequential_fleet_admission_balances_exactly`,
`concurrent_fleet_admission_balances_exactly`, `teardown_releases_the_owner_ledger`. Convoy
(`ipc_admission_convoy_tests.rs`): `ctl_listen_backlog_derives_from_the_ctl_cap`,
`session_admission_records_one_latency_sample_per_admission`,
`a_session_establishment_burst_is_admitted_completely`. Sizing: `service_ceiling_default_ties_to_*`
(root) / `sessions_default_ties_to_*` (shim), `dd_shard_width_derivation_ties_to_drain_lane_width`.

**Fast path / parity** ✦ (`R:tests/preload_parity_tests.rs`): `ring_read_matches_fuse_read_byte_parity`;
`ryw_across_transports_both_directions` (incl. buffered kernel write → ring read under KD-11);
`fsync_through_fuse_after_ring_writes` (§5.6.3 ordering + kill-9);
`arena_severance_client_scribble_never_reaches_acked_bytes`;
`adversarial_mid_serve_mutation_serves_the_snapshot`;
`queued_writer_lock_demotion_completes_after_release_no_deadlock` (the drop-before-enqueue pin);
`cold_attr_miss_demotes_and_eof_fast_path_serves`; `warm_staged_reads_serve_on_the_sync_fast_path`;
`hot_tier_leg_serves_on_the_sync_fast_path_when_seeded`; `ring_read_carries_the_bindings_odirect_class`;
`ddt_mount_odirect_binding_skips_the_tier_fast_path`; `ring_write_applies_the_killpriv_clearing_law`;
`mixed_lane_writes_on_one_file_stay_coherent_and_both_lanes_account`;
`lane_gate_counters_ride_the_stats_page_to_the_stats_inode`; `data_plane_stats_fields_export`. Hold
probe (`ipc_hold_probe_tests.rs`): `hold_probe_serves_bytes_exact_and_ceremony_lands_exactly_once`,
`hold_probe_lane_deposits_stay_ledger_invisible`, `hold_probe_miss_counts_and_demote_path_is_preserved`,
`read_lane_disabled_makes_the_probe_structurally_inert`,
`sync_hot_leg_serves_credit_the_hold_toward_retirement`. Op economy (`ipc_op_economy_tests.rs`):
`warm_fast_path_serves_are_allocation_free` (charter §12: warm shim serve 0 allocs),
`unparked_completions_elide_cqe_wakes`, `parked_reaper_is_woken_by_completion_cqe_wake`,
`wake_economy_instruments_register_and_export_at_zero`. Venue (`ipc_service.rs:1786–1835`):
`handoff_runs_on_a_current_thread_handler_lane`,
`handoff_spawns_from_a_plain_os_thread_without_ambient_runtime`.

**Severance / placed**: `R:tests/shim_parity_tests.rs` ✦
`streaming_ring_write_severs_into_block_buffer_one_copy`,
`ineligible_shapes_ride_the_pooled_sever_unchanged`, `rewrite_of_parked_block_falls_back_to_merge_copy`;
`placed_sever.rs` units `placed_payload_reads_back_and_reaps_at_zero`,
`a_misaligned_or_out_of_bounds_sever_is_refused_in_every_build`,
`adoption_takes_the_backing_and_seals_new_claims_out`, `foreign_pointer_never_adopts_a_live_assembly`;
models `placed_*` (§3.2).

**Direct-drive reads** ◆ (`R:tests/ipc_direct_drive_tests.rs`):
`ddt_ranged_ring_read_direct_drives_with_exact_engagement`;
`unaligned_shapes_direct_drive_through_the_bounce_leg`;
`prelude_misses_fall_back_to_the_handler_with_ledger`; `custody_snapshot_revalidation_governs_the_serve`;
`host_shutdown_is_prompt_with_direct_drive_traffic`; `direct_drive_serves_record_residence_phases`;
`direct_drive_inflight_splits_into_sq_wait_and_device_cq`; `ring_ops_record_ingress_residence`;
`drain_passes_record_duration_and_counts`; `inline_reap_fusion_serves_bursts_exactly_and_exports`;
`default_mount_governed_miss_ladder`; `default_mount_clamped_denials_direct_drive_with_denial_accounting`;
`buffered_ring_misses_never_direct_drive`; `stream_classified_files_ride_the_handler_by_policy`;
`governor_peek_is_non_reserving_and_counts_denials`; `distinct_service_lanes_engage_distinct_shards`;
`shutdown_joins_every_shard_reaper_promptly`. Units (`R:ipc_direct.rs:2431–2578`):
`foreign_thread_lane_fallback_is_dense_and_stable`, `service_lane_pin_wins`,
`dd_eager_flush_lever_parses_and_clamps`, `dd_eager_threshold_law`,
`residual_drain_stops_when_the_train_rearms`, `residual_drain_drains_closed_trains_and_leak_arm_pops_any`.

**Direct-drive writes** ◆ (`R:tests/il_direct_write_tests.rs`):
`direct_writes_serve_on_the_svc_lane_and_stay_byte_exact`;
`ineligible_shapes_fall_back_and_count_their_clause`;
`contended_block_lock_counts_its_own_arm_and_demotes`;
`unlatched_killpriv_counts_its_own_arm_then_relatches`;
`clone_pinned_block_counts_the_fence_backoff_arm_and_cows`;
`fenced_guard_refuses_direct_writes_loud_never_a_fallback`;
`direct_write_purges_read_tiers_before_the_ack`; `lever_off_routes_every_write_to_the_handoff_path`;
`same_block_followers_park_and_train_on_the_guard_holder`;
`drain_batch_coalesces_the_times_park_dispatch`; `train_bound_closes_the_tenure_and_residuals_redrive`;
`teardown_completes_parked_followers_never_silence`.

**Lifecycle / coherence** ✦ (`R:tests/preload_lifecycle_tests.rs`):
`idle_sessions_reap_and_active_sessions_survive`;
`invalidation_fires_on_bind_and_rate_limited_writes_never_reads`;
`size_changing_ring_writes_fire_attrs_only_invalidations_inside_the_window`;
`the_last_unbind_of_an_inode_fires_a_whole_inode_invalidation`;
`teardown_with_parked_handoff_completes_without_poison`;
`fork_child_poison_never_severs_the_parent_session`. Venue (`ipc_inval_venue_tests.rs`):
`inval_hook_delivers_without_an_ambient_runtime`, `inval_hook_fires_from_a_plain_os_thread`,
`inval_hook_skips_pre_mount_fires`.

**Models** (`R:loom-models/src/lib.rs`): `ipc_park_snapshot_before_rescan_never_strands` (3790);
`ipc_slot_snapshot_single_read_never_pre_submit_hostile_bounded` (2992 — the dequeue snapshot is the
single linearization read); `ipc_cqe_*` (3176–3597) and `ipc_ring_*` / `ipc_slot_*` (the sibling
spec); `placed_*` (4540–4903).

**PORT test plan.** Portable: the width/clamp tie tests (§1.1); the claims model + its Dekker
weakening; the severed pool (hits/misses/bytes closure; full-class-only recycle); the train state
machine (arm/park/pop/close/residual/rearm) as a standalone model under `explorer.expectHolds`; the
inval ladder (window / high-water / attrs-only) over an injected hook;
`resolve_interception_posture`, `kernel_lane_min_default`, `reap_batch_wake_threshold`. Linux-only:
an in-process session (memfd + ring + one svc thread) driving the sink against `kv_fs` on `/build` —
the parity rows without the shim; the dd shard against a loop device for the bounce/direct legs and
the stall law. ✦ rows wait for the shim.

---


## 8. Quirks per decision 0002 and the better design

| # | Reference quirk | Why it is poor | What we do |
|---|---|---|---|
| 1 | The registry rows for `SQUEEZEFS_IPC_SERVICE_THREADS` / `_DD_SHARDS` admit `1..1<<12` while the code CLAMPS to 64 (`R:env_knobs.rs:281–282` vs `ipc_host.rs:246`, `ipc_direct.rs:187`) | Two sources of truth; a clamp where §1.6 says refuse | The row's range is `1..=64`; out of range refuses at the gate; the derivation's ceiling 64 is the same number by construction (tie test) |
| 2 | A `heartbeat` word in the session header that no daemon code writes (`R:layout.rs:316–317`; §5.2) — death detection is the op deadline + socket EOF + generation | Dead protocol state the docs still describe as a mechanism | The daemon STAMPS it: `realtimeCoarseS` once per svc pass (one relaxed store per pass, zero per op) and the shim's ladder reads it before the socket probe — or delete the word; decided with the sibling spec, never left half-built |
| 3 | `IpcReadProbe::Hit` copies the active-buffer snapshot AFTER the guard drops while every tier leg copies INTO the arena UNDER the guard (`R:ipc_service.rs:982–984`, `fuse_client.rs:16256–16270`) | Two copy postures for one probe, justified by critical-section length for one arm only | `WarmLadder.probe` serves every rung into `window` under the shared guard (one path, `P:warm_ladder.zig:155–200`); the guard is a try-lock so the hold is bounded by the copy, never a wait |
| 4 | The engine's trains, the Invalidator's maps and the placed registry use `scc::HashMap` entries that are NEVER removed — "recorded ceiling" classes (`R:ipc_service.rs:577–587`, `ipc_direct.rs:438–447`) | Unbounded growth tied to the working set, excused per site | Striped fixed tables keyed by hash (`LastWriteEnd`'s own shape, `P:write_admit.zig:235–262`): a collision costs one extra notify / one honest `block_lock` fallback, never correctness; memory is a constant |
| 5 | `DataPlaneSink.direct` is a `OnceLock<Option<Arc<Engine>>>` built lazily from a svc thread on the FIRST governed op (`R:ipc_service.rs:770–777`): volumes are opened and rings built inside a hot pass, and volumes added later are prelude-ineligible forever (`ipc_direct.rs:803–805`) | Mount-time work on the data path; a frozen volume set | The dd shards are built at HOST spawn (the il host arms only on an interception mount, so a mount that never sees the shape still pays nothing it would not pay for the FUSE rings); `volume add-data` registers the new fd with `registerFilesUpdate` (`P:uring128.zig:408`) on every shard |
| 6 | The dd reaper's unrecoverable-ring arm `mem::forget`s the in-flight entries and leaks a held block guard — "that block's stripe wedges" (`R:ipc_direct.rs:1790–1806`) | A permanent stripe wedge as the terminal state | The same leak law for the mappings (never unmap under DMA), but the held guard is RELEASED after the leak with the block latched `damaged` in the map (the bit the port already carries, `P:sole_owner_patch.zig:221`), so later writers CoW instead of wedging; the volume's fail-stop latch (charter §1.8) is the real recourse |
| 7 | `sqz-ipc-thp`: a deferred `MADV_POPULATE_WRITE + MADV_COLLAPSE` worker over every arena, with its own ledger and skip arms (`R:ipc_host.rs:1319–1388`, `:2600–2755`) | A fourth thread class and four counters for a page-size upgrade the kernel's THP policy already offers on a memfd | `madvise(MADV_HUGEPAGE)` at creation (one call, no worker) and let `khugepaged` collapse; measure; `SQUEEZEFS_IPC_ARENA_THP` stays as the A/B |
| 8 | The Invalidator's `write_hwm` + `last_write` are two maps probed per write; a ring write that neither grew the file nor crossed the window still pays two latch-free lookups (`R:ipc_service.rs:631–656`) | Two tables for one per-ino word | ONE striped per-ino word `{hwm: u48, last_fire_s: u16}` (coarse seconds suffice for a 1 s window) — one load, one CAS |
| 9 | `SQUEEZEFS_IL_READ_DEST=0` forces the handoff to serve into heap `Bytes` and then `payload.write` into the arena (`R:ipc_service.rs:117–125`, `:302–310`) | A lever whose control arm is a deliberately worse copy count | Not carried: the handler's `Ctx.window` IS the arena (`P:filesystem.zig:57`); `ipc_arena_copy_bytes` counts only the sync ladder's copies; `ipc_read_dest_serves` = handoff replies landed in place (always) |
| 10 | Placed-write handoffs defer through a `thread_local!` `RefCell<Vec<Pin<Box<dyn Future>>>>` drained by `flush()` (`R:ipc_service.rs:180–213`) — a heap-boxed future per placed write | A per-op allocation on the write lane; a thread-local queue a foreign caller can strand (`:440–447`) | The svc thread owns a fixed `[]PendingHandoff` of `drain_budget` entries (a pass can defer at most that many); `flush()` spawns them with `spawnGroupOn`; no allocation, no thread-local |
| 11 | The svc loop's park snapshot is a `Vec<u32>` cleared and refilled per park (`R:ipc_host.rs:2922–2926`, `:3045–3050`) and `futex_wait_many` builds a `FutexWaitv` array per call (`:3792`) | Per-park work proportional to sessions that is already bounded by 128 | A fixed `[128]Waiter` per svc thread (the `FUTEX_WAITV_MAX` rail is the bound); more than 128 owned sessions splits into rounds |
| 12 | `SlotCompletion::complete` wakes the slot word with `i32::MAX` AND bumps the cqe doorbell AND may wake it — up to two futex syscalls per completion in the parked case (`R:ipc_host.rs:420–458`) | Two wake vectors for one event | Kept as-is: the two words serve different parkers (a sync waiter on its slot vs a libaio reaper on the session); the latch bounds the second to ≤ 1 per era. Recorded, not changed |

Items 1, 4, 5, 8, 9, 10, 11 land with rung 7; 2 is decided jointly with the sibling spec; 6 and 7
carry a measured A/B before the default flips.
