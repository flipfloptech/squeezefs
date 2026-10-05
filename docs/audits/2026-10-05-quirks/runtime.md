# Runtime-mechanism design audit — rung 2b (MetaLV commit/durability/checkpoint/space)

Reviewer scope: `src/meta/{journal_core,journal,journal_lane,ledger,alloc_core,alloc_ext,node_snapshot,node_cache,tree,smo,leaf_merge,checkpoint,checkpoint_cycle,conveyor,device_loader,backend,dlm,tx,heap_law,inos,economy,epoch_core,conveyor_core,node_state_core}.zig`, `src/sync/*.zig`, `src/core/{counters,thread_name,knobs}.zig`, read against `docs/charter.md` §1, §2, §5.2–5.3, §9–§11, decisions 0001/0002, the two porting surveys, and the rung-2a/2b evidence notes. Read-only; nothing in the repo was modified.

Ruling applied (decision 0002): the charter's mechanisms, invariants and derivations are requirements; the reference's incidental choices are evidence. A finding is either (a) a reference quirk carried that the charter does not require and a better design exists, or (b) a port shortcut that violates the charter. Deviations already listed in the evidence notes ("Deviations" sections of rung-02a/02b) are not re-reported unless I disagree.

Verdict vocabulary: **KEEP** (good engineering, protect it) · **CHANGE NOW** (cheap, and leaving it costs more later or is a correctness/charter breach) · **CHANGE AT RUNG N** (the right moment is when that rung's machinery exists) · **ASK OWNER** (a product posture, not an engineering call).

---

## 0. Executive summary

Three items are **port shortcuts that break a charter mechanism today** and should land before anything builds on rung 2b:

1. **The writeback-threshold pipeline is disconnected** (#4): the conveyor pushes threshold nodes onto `KvTree.maintenance` (the lock-free queue), but the checkpoint task drains its *own* `Checkpointer.maintenance` ArrayList, which nothing in the product fills. The charter's "writeback at one bset page of open delta" never fires on a mounted volume; only the ≤ 1 s cadence flush does.
2. **The SMO's pre-lock barrier failure is swallowed** (#6): `smoBarrierHook` is `be.dev.barrier() catch {}` and `SmoJournal.barrier` returns `void`, so a failed `fdatasync` lets the SMO publish pointer records to images that may not be durable — the one durable-order obligation the SMO protocol rests on.
3. **Lane barriers never feed DUR-3** (#3): only the checkpoint task's own `syncDevice` advances `reusable_upto`; the lane's coalesced/strict barriers (which are most barriers on a busy volume) are invisible to reclamation, and the `KickMark.flush`/`requestFlush` arm that would bridge them is dead code in the product.

Two items are **reference quirks carried that the charter does not require** and that a reviewer should rule on: the `heap_full` twin latches (#5 — collapse now), and SMO entries written through the io pool instead of the lane (#7 — sound today, record it, make it lane-native when the executor lands).

The **coalesced 50 ms default** (#1) is the charter's own posture and is correct for the `kill -9` law; what is wrong is that `50` is a hand-me-down with no reason on the line and lives in three places. Keep the posture, derive the number, unify the source.

Everything else is either KEEP (the port's lock-free cores, the lane's phase machine, DUR-3, the pure deciders, the queue-cell bounds are better than the reference) or a rung-3 follow-up.

---

## 1. Findings index

| # | Item | Where | Verdict |
|---|---|---|---|
| 1 | Coalesced-mode default `SQUEEZEFS_META_FLUSH_INTERVAL_MS = 50` | `journal_lane.zig:95-98`, `checkpoint.zig:85-86`, `knobs.zig:104` | KEEP posture; CHANGE NOW the line (derive + one source) |
| 2 | `50` defined three times; "one source" comment is false | `checkpoint.zig:85-86` vs `journal_lane.zig:98` vs `knobs.zig:104` | CHANGE NOW |
| 3 | Lane barriers do not advance DUR-3 (`reusable_upto`); `KickMark.flush` dead | `checkpoint_cycle.zig:480-482, 524-556, 1084-1089, 1129-1131, 1164`; `journal_lane.zig:1412-1424`; `backend.zig` (no `kick(.flush)`) | CHANGE NOW |
| 4 | Threshold writeback disconnected: two maintenance queues that never meet | `conveyor.zig:1120-1129` → `tree.zig:479-481`; `checkpoint_cycle.zig:496-516, 1025-1035`; `smo.zig:1049-1063` (test-only callers); `checkpoint_cycle.zig:342-354` (no trees in `Deps`) | CHANGE NOW |
| 5 | `heap_full` carried by two latches; `HeapPromises`/`recordCycle` dead twins | `alloc_core.zig:739-777`; `heap_law.zig:212-249`; `checkpoint_cycle.zig:417-418, 763, 921-935`; `alloc_ext.zig:772-823` (callers only tests `:1222-1243`) | CHANGE NOW |
| 6 | SMO pre-lock barrier failure swallowed | `backend.zig:651-654`; `smo.zig:161-164, 893`; `leaf_merge.zig:587`; `backend.zig:861` | CHANGE NOW |
| 7 | SMO entries written through the io POOL, not the lane | `smo.zig:115-126, 176-194`; `backend.zig:656-667` | KEEP now; record in 0002; CHANGE AT RUNG 3 |
| 8 | `wait_completed_upto` as a yield-spin | `journal_core.zig:712-721`; product caller `checkpoint_cycle.zig:1060-1066` (shutdown only); the lane uses the poll form `journal_lane.zig:1271` | KEEP (known deviation; shutdown-only) — add the close-order assert |
| 9 | Space wake split in two notifies; the 10 ms park tick is load-bearing | `checkpoint_cycle.zig:379, 543-556, 1147-1151, 1239-1245`; `conveyor.zig:127-131, 847-863`; `backend.zig:510, 748-753, 812-816` | CHANGE NOW |
| 10 | `dlm.max_requests = 16` vs the 128-ino times drain | `dlm.zig:146-149, 269-273`; `economy.zig:714-720`; survey §2.16 | CHANGE AT RUNG 3 (decide now) |
| 11 | `tx.GuardSet` as `*anyopaque` + release fn | `tx.zig:61-82`; `dlm.zig:170-188` | KEEP; bind with an `Io`-carrying ctx at rung 3 |
| 12 | SETATTR-echo absorber | `economy.zig:17-28, 774-905` | KEEP the map; ASK OWNER at rung 3 whether the echo arm is reachable |
| 13 | `destroy_inodes` batching | `inos.zig:229-287` | KEEP |
| 14 | Parent-guard class from the record (vs "exclusive-both") | `economy.zig:185-227, 598-645` | KEEP |
| 15 | Spin locks: where, and on which paths | `journal_core.zig:527-549, 637-671`; `conveyor.zig:538, 1310-1317`; `economy.zig:785`; `ledger.zig:549`; `checkpoint_cycle.zig:400, 423`; `alloc_core.zig:163-191` | KEEP (with two fixes: never allocate under `inflight_lock`; the registry's O(open) scan) |
| 16 | `std.Io.Mutex` uses | `conveyor_core.zig:82`; `backend.zig:264`; `node_cache.zig:1318, 1324, 1358`; `sync/rwlock.zig:33-40, 63` | KEEP |
| 17 | `ArcSwap.load` is a shared-line RMW per read | `sync/arc_swap.zig:20-23`; root slot `tree.zig:301-310` | KEEP now; measured A/B at rung 4 |
| 18 | Hooks that run a checkpoint cycle ON the lane thread | `journal_lane.zig:309-323, 1344-1356, 1382-1385`; `backend.zig:746-753` | KEEP (reference shape, charter-named); add a wall histogram |
| 19 | `RETRY_BUDGET = 256`, `READER_LOAD_RETRIES = 3` | `node.zig:76-88` | KEEP |
| 20 | Rollback/compensation OOM exits are silent | `conveyor.zig:1286, 1392, 1406, 1415, 1430, 1435, 1459-1461, 1469-1478, 1499-1501, 1530-1531` | CHANGE NOW |
| 21 | Dead ring does not latch FAILED | `journal_lane.zig:1168-1191` | CHANGE NOW (one line) |
| 22 | Test doubles in product files (`RamLane`, `TestVolume`, `VolatileImage`, `Harness`) | `conveyor.zig:1552-1707, 1780`; `smo.zig:1222`; `journal.zig:1147`; `checkpoint_cycle.zig:1265` | CHANGE AT RUNG 3 (extract stage B; move doubles under a testkit) |
| 23 | `catch unreachable` / `@panic` on handler-reachable paths | census below | KEEP — none found on a handler path |
| 24 | Per-op / per-batch allocation on the commit path | `conveyor.zig:939-947, 1061-1095, 1168, 1246-1267, 1313`; `tx.zig:409-421` | KEEP now; pool at rung 3 |
| 25 | `counters.max_stripes = 64` with round-robin stripe assignment | `counters.zig:20-47` | CHANGE AT RUNG 3 (derive from possible CPUs) |
| 26 | `commit_parked` gauge add/sub per commit | `conveyor.zig:708-711` | low; CHANGE AT RUNG 3 |
| 27 | Thread names: `sqz-uring-fs<N>` digits truncated under a tag; not a charter class; `tag & 0xf` | `thread_name.zig:36-44, 62-73, 83-96`; `uring_fs.zig:95` | low; CHANGE NOW (rename) |
| 28 | Knob class `.tuning` is not one of the charter's three classes | `knobs.zig:60-67, 98, 104, 125-127, 131` | ASK OWNER |
| 29 | The "no runtime literal in product code" rail does not exist | `tests/rails.zig` (two knob censuses only) | owed (charter §11) |
| 30 | `SpaceAudit`'s 8-cycle wedge rule; the FAILED lattice shape; 32 ledger slots; 2 s tick | `checkpoint.zig:88-111`; `journal.zig:1067-1113`; `ledger.zig:48`; `sync/tick.zig:23` | KEEP |
| 31 | `waitHeadStable` under `smo_lock` relies on close ordering | `checkpoint_cycle.zig:1060-1066, 1078-1079`; `backend.zig:847-860` | KEEP; add an assertion |
| 32 | Constants census (every bare numeric judged) | §3 below | mixed |

---

## 2. Findings in full

Each finding: **where** · **reference did / port did** · **why it may be poor** · **what the charter requires** · **proposed design** · **cost now vs later** · **verdict**.

### #1 — The coalesced-mode default (50 ms of acked-but-volatile metadata)

- **Where.** `src/meta/journal_lane.zig:95-98` (`default_flush_interval_ms = 50`, comment: "the legacy v2 flusher cadence reused as the coalesced barrier's period"); `src/meta/journal_lane.zig:1330-1380` (`runVerdicts`: strict ⇒ `.want_barrier` before `.ack`; coalesced ⇒ `setNeedsFlush` then `.ack`); `src/meta/journal_lane.zig:1412-1424` (`flush_due_ns = max(now, last_barrier + interval)`).
- **Reference / port.** Identical: acks after the write CQE; one trailing barrier per interval; strict = 0.
- **Why it may be poor.** On a power cut (not a process death) up to one interval of acked metadata is lost whole — the rung-02b "portable power cut" row states exactly that. The *number* carries no physical or measured reason: "legacy v2 flusher cadence reused" is a hand-me-down (charter §1.5: "A fixed clamp needs a documented physical/format/kernel/measured reason on the line").
- **What the charter requires.** §5.2: "Acks only after the entry landed (strict mode: after the barrier)." §5.3: "fsync: the trailing coalesced barrier covers journal pages; strict mode (`META_FLUSH_INTERVAL_MS=0`) barriers per commit, coalesced." §1.10: "Zero acked-data loss on any single-node `kill -9` at any instant." — `kill -9` is a process death; a completed O_DIRECT write has left the process and survives it, so §1.10 holds in coalesced mode (the kill matrix row 1 proves it). Power loss is not in §1.10; POSIX owes durability only at `fsync`, and the port's `fsync` IS the trailing barrier (`backend.zig:805-809`). The charter therefore explicitly sanctions the coalesced posture as the default.
- **Proposed design.** Keep coalesced as the default. Replace the bare `50` with a derivation that states what it buys: the interval bounds the power-loss exposure in TIME while the barrier count per second bounds device cost; a defensible line is `interval = clamp(k × measured barrier wall, 10 ms, CHECKPOINT_MAX_AGE/2)` seeded at 50 ms until a barrier has been measured, or — simpler and honest — keep `50` as a **measurement lever** (`.ab_lever`, logged at mount when non-default) with the WHY on the line: "50 ms = 1/20 of the checkpoint cadence, so a cycle's two covering barriers (`coveringBarriers`) cost ≤ 10 % of the cadence at the measured 1.3–1.9 ms barrier; the kill -9 law is independent of it; power-loss exposure ≤ one interval." Either satisfies §1.5; the hand-me-down comment does not.
- **Cost now vs later.** One comment + one constant now; a knob-class change is a registry row edit.
- **Verdict.** **KEEP the posture; CHANGE NOW the line.** Do not switch the default to strict: §5.3 names strict as the opt-in, and the reference's measured strict cost (3 parks/window vs 2) is real.

### #2 — Three copies of the one default

- **Where.** `checkpoint.zig:85-86` ("`SQUEEZEFS_META_FLUSH_INTERVAL_MS` default (the lane's; one source)" — but it defines its own `50`), `journal_lane.zig:98` (a second `50`), `knobs.zig:104` (`.default = "50"`, documentary per `knobs.zig:73-75`). No tie test relates the three (`checkpoint.zig:630` ties the parser to its own constant only; `tests/rails.zig` checks names, not defaults).
- **Why it may be poor.** Drift between the lane's cadence and the checkpoint tick (`checkpointTickPeriodMs` reads the checkpoint copy; the lane reads its own) would silently decouple the tick from the flush interval the survey §4.1 ties together.
- **Charter.** §1.5 "Every derivation has a drift-is-red tie test."
- **Proposed design.** `journal_lane.zig` imports `checkpoint.DEFAULT_FLUSH_INTERVAL_MS` (checkpoint is portable; the lane is Linux-only so the dependency direction is the right one); add a rail/tie test that parses every `.int` registry row's `default` string and compares it with the typed default the reading module exports (a tiny table of `{key, &const}`).
- **Verdict.** **CHANGE NOW.**

### #3 — Lane barriers never advance DUR-3; `KickMark.flush` is dead

- **Where.** `checkpoint_cycle.zig:524-538` (`syncDevice`: the ONLY place `barrier_durable` moves and `afterDurableBarrier` runs); `:543-556` (`afterDurableBarrier` → `advanceDurable`, `advanceReusableUpto`, `setDurableTail`, `space_notify`); `journal_lane.zig:1412-1424` (the lane's coalesced barrier, on the lane ring, with no DUR-3 ticket); `checkpoint_cycle.zig:480-482` (`requestFlush`), `:1084-1089` (the tick's deferred barrier), `:1129-1131`/`:1164` (`KickMark.flush`) — the only callers are tests (`checkpoint_cycle.zig:1796, 1890`); `backend.zig` never kicks `.flush` (grep: `kick(.ring_park)` `:727`, `kick(.threshold)` `:732` only).
- **Reference / port.** The reference had ONE `sync_device` (`SyncCoalescer`) that both the tick's deferred flush and the cycle ran, so every barrier drained `pending_reclaim` (survey §2.10, §2.15). The port split barriers into two streams: the lane's (FSYNC SQE on the lane ring — good) and the checkpointer's (pool `fdatasync`), and only the second feeds DUR-3.
- **Why it may be poor.** (a) A ledger record written by a non-`barrier_now` cycle becomes `reusable_upto` only at the NEXT checkpointer barrier (next cycle's barrier #1, ≤ `CHECKPOINT_MAX_AGE`), although the lane barriered the device up to 20 times in between — reclamation lags ≈ 2 cycles ≈ 2 s on the 8 MiB floor ring (≈ 1.1 s of creates at 32 k/s), so admission parks and park-kicked immediate-barrier cycles become the steady state under load rather than the exception. (b) Two independent barrier issuers on one device where the charter has one trailing coalesced barrier. (c) `KickMark.flush` + `requestFlush` + the tick's deferred-barrier arm are dead code (charter §1.9).
- **Charter.** §5.3: "Ring reclamation gates on the ledger record being DURABLE (`reusable_upto` watermark, post-barrier)" — *any* post-write barrier makes it durable; §5.3 "fsync: the trailing coalesced barrier"; §1.9 no dead code.
- **Proposed design.** Give the lane two DUR-3 hooks: `barrier_started() → ticket` (the checkpointer's `barrier_starts.fetchAdd`) taken when the FSYNC SQE is pushed, and `barrier_landed(ticket)` at its CQE (`barrier_durable.fetchMax` + `afterDurableBarrier`). Then delete `KickMark.flush`, `requestFlush`, the tick's deferred-barrier arm and `meta_flush_deferred` (or re-point the counter at the lane's coalesced barriers). The cycle's own barrier #1 keeps using the sink; both streams now move the same three watermarks on one clock, which is the DUR-3 law as written in `checkpoint_cycle.zig:28-36`.
- **Cost now vs later.** ~40 lines now (two fn pointers in `journal_lane.Hooks`, one call in `startBarrier`, one in `barrierDone`); later it becomes an executor-era refactor because the lane thread and the checkpoint task will share the lane (decision 0002 row 6).
- **Verdict.** **CHANGE NOW.**

### #4 — The threshold-writeback pipeline is disconnected

- **Where.** Producer: `conveyor.zig:1120-1129` (A10: `t.enqueueMaintenance(n.addr)` → `tree.zig:479-481` → the lock-free `MaintenanceQueue`, `tree.zig:213-290`), then `kickThreshold` → `backend.zig:730-733` → `task.kick(.threshold)`. Consumer: `checkpoint_cycle.zig:1025-1035` (`drainMaintenance` pops `self.popMaintenance()` = `Checkpointer.maintenance`, an `ArrayList` under `maint_lock`, `:496-516`), whose only filler `Checkpointer.enqueueMaintenance` is called from one unit test (`checkpoint_cycle.zig:1772`). The tree-queue consumer `smo.runMaintenanceUntil` (`smo.zig:1049-1063`) is reached only from `smo.TestVolume` (`:1332, 1336`) and tests. `Checkpointer.Deps` (`checkpoint_cycle.zig:342-354`) holds no trees. `grep maintenance src/meta/backend.zig` → nothing.
- **Reference / port.** Reference: `apply_locked` threshold ⇒ `tree.enqueue_maintenance` ⇒ `ckpt_wake.notify_one()` ⇒ `maintenance_pass` ⇒ `maintain_node` (freeze → append → else `smo_replace`) (survey §2.9). Port: both halves exist, both tested in isolation (the lock-free queue is even model-checked), and they are not connected in `backend.zig`.
- **Why it may be poor.** On a mounted volume the "one bset page" writeback never runs; the open overlay grows until the cadence cycle (≤ 1 s) or the dirty-node cap. Effects: more RAM in overlays/heads, larger freezes at the cycle (longer `prebarrier_wall` → the term the cadence anticipates), `snap_tail` re-merges more often, and `meta_kv_maintenance_queue_full` climbs once the per-tree ring (`maintenanceQueueCapacity`) fills — a tripwire-shaped counter that will read nonzero on every busy mount. The kill matrix and the closure identities cannot see this (they do not depend on when a bset is appended), which is why it slipped.
- **Charter.** §5.3 "Writeback: freeze the dirty delta under the lock into a 4 KiB-padded bset image, release, append outside the lock"; survey §2.9/§4.1 "Writeback threshold 4096 B of open delta — one bset page (§4.6 pt 1)".
- **Proposed design.** Delete `Checkpointer.maintenance`/`maint_lock`/`enqueueMaintenance`/`popMaintenance` (an O(n) `orderedRemove(0)` + `insert(0)` ArrayList under a spin lock — the wrong structure anyway). Give `Checkpointer.Deps` the trees (or a `MaintenanceSource` seam `{ctx, pop: fn(ctx) ?{tree, addr}, pending: fn(ctx) bool}` the backend binds to a round-robin over `be.trees[*].popMaintenance()`), and have `drainMaintenance` pop from it; `maintenancePending` becomes `any tree.maintenancePending()`. The `reserve_deferred_addr` re-insert-at-front becomes a `tree.enqueueMaintenance(addr)` (the queue is MPSC-safe for that). Add a backend test: N commits over threshold on a mount ⇒ `meta_kv_maintenance_items > 0` and `ram.appends > 0` BEFORE any cadence cycle.
- **Cost now vs later.** ~60 lines now. Later: every rung-3/4 bench row (creates/s, RSS) is measured against a mount without threshold writeback and becomes a wrong baseline.
- **Verdict.** **CHANGE NOW.** This is the most consequential finding in the audit.

### #5 — `heap_full` on two latches; `HeapPromises` and `recordCycle` are dead twins

- **Where.** `alloc_core.SpaceAudit` (`alloc_core.zig:739-777`) owns `heap_full`, `heap_full_cycles` AND the wedge rung, and its `tick` returns a `PostureTransition`. `heap_law.HeapFullPosture` (`heap_law.zig:212-249`) owns a second `full` word with `enter/leave/cycleFlushPass`. `checkpoint_cycle.Checkpointer` holds BOTH (`:417-418`); C5 calls `posture.cycleFlushPass` (`:763`) and C12 calls `audit.tick(obs)` whose `out.posture` is **ignored** (`wedgedTailAudit`, `:921-935`), so `SpaceAudit.heap_full`/`heap_full_cycles` are shadow state nobody reads. `alloc_ext.HeapPromises` (`:772-800`) and `alloc_ext.recordCycle` (`:804-823`, which DOES act on `out.posture` and increments `meta_kv_heap_full_cycles` again) are referenced only by tests (`alloc_ext.zig:1222-1243`). `heap_law.HeapLedger` (`heap_law.zig:94-124`) is the live ledger bound to `NodeCache.heap_promised`.
- **Reference / port.** The reference had one posture word on the backend and one `heap_promised` on the cache. The port grew a second posture inside the pure audit (rung 2a, before `heap_law` existed in 2b) and a second ledger in `alloc_ext` for the same reason; 2b never retired them.
- **Why it may be poor.** Two sources of truth for one posture, one of which is dead; a future wiring of `recordCycle` double-counts `meta_kv_heap_full_cycles`; `SpaceAudit`'s tie tests assert transitions the product never consumes. Charter §1.9.
- **Charter.** §5.3 "a cycle that defers a node for NoSpace is the counted non-terminal space class; the FAILED terminal is the wedge class alone" — one posture, one wedge rung.
- **Proposed design.** `SpaceAudit` keeps ONLY the wedge rung (`stalled_cycles`, `failed`, the four verdicts); drop `heap_full`, `heap_full_cycles`, `PostureTransition`, `CycleOutcome.posture`. `HeapFullPosture` stays the one posture (it already owns the gauge, the log lines and the three enter / two leave sites). Delete `alloc_ext.HeapPromises` and `alloc_ext.recordCycle`; move their two tests onto `HeapLedger`/`HeapFullPosture`. Update `alloc_core_model`'s posture rows accordingly.
- **Cost now vs later.** ~120 lines deleted now; free.
- **Verdict.** **CHANGE NOW** (agreeing with the implementer who reported it).

### #6 — The SMO's images-before-pointers barrier can fail silently

- **Where.** `backend.zig:651-654`: `fn smoBarrierHook(...) void { be.dev.barrier() catch {}; }`. `smo.zig:161-164`: `pub fn barrier(self: *SmoJournal) void` (cannot propagate). Call sites `smo.zig:893` (P2 of `smoReplace`) and `leaf_merge.zig:587` (merge). `device_loader.zig:145-148` logs and counts `io_errors` but the caller proceeds. Also `backend.zig:861` (the close-path belt barrier, `catch {}`).
- **Reference / port.** Reference: `j.barrier()?` (survey §3.4 P2: "images durable BEFORE any pointer record exists … On error release every claimed extent"). Port: logged, counted, then the lock window runs anyway and the pointer records are reserved and written.
- **Why it may be poor.** A failed `fdatasync` here is exactly the case the barrier exists for: a later replayed `Put(succ.max → addr)` may route to an image that never reached media. The checkpoint's own barriers do propagate (`checkpoint_cycle.zig:530-533` → `latch.noteFailure(.barrier)`), so the lattice sees cycle barriers but not SMO barriers.
- **Charter.** §1.8 "Fail loud, never silent … repeated journal-barrier failure … latches the volume FAILED. Never a silent fallback to a weaker posture." §5.3 SMO order "barrier the images → parent-then-child locks".
- **Proposed design.** `SmoJournal.barrier` returns `error{BarrierFailed}!void`; the hook returns the sink's error; `smoReplace`/`smoMerge` treat it like `JournalReserveExhausted` (release claimed extents via the existing `release_unpublished` arm, surface a typed retryable `BarrierFailed`), and the backend's hook calls `latch.noteFailure(.barrier)` so three in a row latch. The close-path belt at `backend.zig:861` should at least log (the final tick already barriered; the belt is informational).
- **Cost now vs later.** ~25 lines now; a red test is easy (`arm_barrier_error` seam + a pending SMO ⇒ no pointer record written, extents released, `barrier_failures = 1`).
- **Verdict.** **CHANGE NOW.**

### #7 — SMO entries go to the device through the io pool, not the lane

- **Where.** `smo.zig:115-126` (`DeviceHooks` doc: "SYNCHRONOUSLY through the io pool — never through the journal lane, whose own thread runs hole checkpoints (a lane-thread SMO parking on a lane ticket would wait on itself)"); `smo.zig:176-194` (`commitEntry` → `d.write` → `reg.complete` → `d.completed`); `backend.zig:656-667` (`smoWriteHook` → `dev.writeOps` on the `uring_fs` pool; `smoCompletedHook` → `lane.kick()`).
- **Reference / port.** Reference: lane-native only when ON the lane thread with a ring, else the pool path (survey §2.6 "`submit_write_at_batch` is lane-native only when called ON the lane thread … else `None` ⇒ pool path"); checkpoint barriers via the process pool (survey §2.17). Port: always the pool for SMO entries; the lane for user windows and their barriers.
- **Is a second writer to the ring sound?** Yes, as built: both writers draw disjoint byte ranges from ONE reservation core + ONE registry (`reserveRegistered`, `journal_core.zig:637-649`), page headers are owned per reservation (the `first_entry_off` law), the pool write completes (CQE reaped) before the SMO proceeds, the lane's completed-prefix wait (`journal_lane.zig:1271`) parks behind SMO reservations below the group's end until `complete` + the `completed` kick, and both `fdatasync` paths cover all completed writes to the fd regardless of which ring issued them. Ordering of reservations is the registry lock's; nothing else is needed. The evidence note's "mid-SMO entry (swap done, entry unlanded)" row exercises it.
- **Why it may still be poor.** Charter §9 "One journal lane OS thread per writable volume owning that volume's io_uring and parking in it" and decision 0002 row 6 ("both stages on the ONE lane thread") describe a single ring for the volume's journal I/O; a pool write is a second ring + a synchronous park on another thread's completion. It is the cross-thread wake hop the lane was built to remove, paid once per SMO (rare: ≈ 1 per 55 creates in the measured storm — ~940/s at the 52.5 k entries/s row, so not negligible at the bar).
- **Proposed design.** Rung 3: a thread-local "on the lane" flag; `smoWriteHook` issues the SQE on the lane ring and reaps inline when on the lane thread (the hole-checkpoint re-entrancy case), and otherwise enqueues the write as a lane message and parks on a futex slot (the shape the lane already has for windows). The pool path then goes away for journal writes (charter §1.2 spirit: one path).
- **Cost now vs later.** Zero now; the rung-3 change is ~80 lines. Record the current shape in decision 0002's ledger (it is not there) and in the evidence note's deviations.
- **Verdict.** **KEEP now; record; CHANGE AT RUNG 3.**

### #8 — `wait_completed_upto` as a yield-spin

- **Where.** `journal_core.zig:712-721`. Product callers: `checkpoint_cycle.zig:1060-1066` (`waitHeadStable`, the shutdown fixpoint only) and `conveyor.RamLane.pump` (`:1663`, a test double — see #22). The lane itself uses the non-blocking poll (`journal_lane.zig:1266-1279`) and parks in its ring.
- **Verdict.** **KEEP** — already a declared deviation in both evidence notes, and in the product it is reached only at close. Two notes: (a) `waitHeadStable` runs under `smo_lock` (`checkpoint_cycle.zig:1232-1237`); it is deadlock-free only because `backend.close` quiesces the conveyor and shuts the lane down BEFORE `task.stop()` (`backend.zig:847-860`) — a lane thread blocked in `checkpointPast` on `smo_lock` while the task spins on the lane's completion would wedge. Assert `cv.windowsInflight() == 0` at the top of the final tick so a reordering fails loud. (b) The 64-iteration bound (`:1061`) has no reason on the line.

### #9 — The admission space-wake is split across two notifies; the 10 ms park tick is doing real work

- **Where.** `Checkpointer.space_notify: jc.Notify` (a poll epoch word, `checkpoint_cycle.zig:379`) is bumped in `afterDurableBarrier` (`:554`). The conveyor parks on a *different* object, the futex-backed `sync.Notify` at `CheckpointHooks.space` (`conveyor.zig:494`, bound to `be.space` at `backend.zig:510`). The only bridge is `CheckpointTask.runOnce`'s loop (`checkpoint_cycle.zig:1239-1245`: notify iff `reusable_upto` changed since the task's last run). `reusable_upto` also moves from `backend.checkpointNow` (`:812-816`, the pass's space retry on a committer thread) and `backend.checkpointPast` (`:748-753`, the lane's hole checkpoint) — neither wakes `be.space`. The parked pass therefore relies on `PARK_TICK_NS = 10 ms` (`conveyor.zig:127-131, 863`) to notice.
- **Reference / port.** The reference's `space_notify.notify_waiters` was the one object admission parked on (survey §3.6), and the 10 ms was a floor on a timeout whose normal exit was the notify. The port's two-object shape turns the floor into the primary wake on two of three paths.
- **Why it may be poor.** Up to 10 ms of dead time per ring-full event on the paths that most need the wake (the space retry IS waiting for room); `full_stalls` counts every 10 ms iteration rather than stalls; each iteration re-kicks the checkpoint task (`:858`).
- **Charter.** §9 "every unbounded park a 2 s ticked wait that re-registers" — the tick is a backstop, not a transport. §5.3 "A committer parked at ring admission kicks the checkpoint task … with an immediate barrier" (the wake back is implied).
- **Proposed design.** Make `afterDurableBarrier` the ONE notifier: give `Checkpointer` an optional `space_wake: ?*sync.Notify` + the `Io` it needs (the backend has one; the portable tests leave it null and keep the poll word), and call `notifyWaiters(io)` there. Delete the task-loop bridge and `last_reusable_upto`. Then `PARK_TICK_NS` is a pure backstop and can become the sync tick (2 s) or stay at 10 ms with "lost-wake backstop" as its only reason; `full_stalls` counts once per park entry.
- **Cost now vs later.** ~20 lines now. Later it hides inside rung-3 latency rows as a mysterious 10 ms mode.
- **Verdict.** **CHANGE NOW.**

### #10 — `dlm.max_requests = 16` versus a 128-ino drain plan

- **Where.** `dlm.zig:146-149` ("Rename EXCHANGE across parents is 4 inode + 2 dentry requests; 16 leaves 4× headroom … and bounds the stack frame"), `:269-273` (`PlanTooLarge` past 16). `economy.zig:714-720` (`PENDING_TIMES_DRAIN_BATCH = 128` — "one DLM `lockMany` set, one entry"); the drain test (`economy.zig:1759-1835`) never takes guards. Survey §2.16: the reference committed "batches of ≤ 128 inos per `lock_many` set".
- **Why it may be poor.** When rung 3 wires the drain, `lockMany` refuses the plan. The obvious fallback — drain 16 at a time — breaks the economy gate: at 100 k echoes the drain costs 6,250 entries instead of 782, and `unlinkEntriesPerOp(100k, 62, 6250)` ≈ 1.08 > the 1.05 gate (`inos.zig:272-287`).
- **Charter.** §5.2 "unlink destroys ride batched `destroy_inodes` (≈ 1.0 entries/op; gate ≤ 1.05)"; §2 lock order "one canonical multi-key acquisition per volume per op".
- **Proposed design.** Decide now; implement at rung 3 with the drain: either raise `max_requests` to `PENDING_TIMES_DRAIN_BATCH` (a `GuardSet` of 2 × 128 × 24 B ≈ 6 KiB on the frame — fine for a task, and the drain is the only large plan) or add `lockManyLarge(io, inos: []const u64)` for same-class, same-mode, ascending-stripe sets with heap-owned guards. Derive `PENDING_TIMES_DRAIN_BATCH` from `dlm.max_requests` (or the reverse) with a tie test so they cannot disagree again; the 128 KiB entry cap is not the binding constraint (128 × 10–18 B Δs ≪ 128 KiB).
- **Verdict.** **CHANGE AT RUNG 3 (decide now).**

### #11 — `tx.GuardSet` as `*anyopaque` + release fn

- **Where.** `tx.zig:61-82` (type-erased; `release` idempotent); `dlm.zig:170-188` (the typed by-value set whose `release` needs an `io: Io`); the only binding is a test (`conveyor.zig:1948-1959`).
- **Why it exists.** `tx` is portable and must not take an `Io`; `dlm` is `Io`-shaped. The erasure is the seam between them.
- **Why it may be poor.** The ctx will be a committer-frame object; soundness rests on "a committer is never cancelled" (decision 0001 §5, charter §9 "cancellation of a committer is designed OUT") — true, and the conveyor releases guards strictly before `done.send` (`conveyor.zig:751-757`), so the frame is alive. The hazard is only future: a cancelable committer would dangle the pointer.
- **Proposed design.** Keep the erasure; at rung 3 bind it to a heap- or arena-owned `{set: dlm.GuardSet, io: Io}` that the `QueuedTx` arena owns (the arena already moves into the queue entry, `tx.zig:409-421`), never a stack frame. Add the comment to `tx.GuardSet` that the ctx must outlive the terminal outcome and may not live on a cancelable frame.
- **Verdict.** **KEEP.**

### #12 — The SETATTR-echo absorber: right mechanism, contingent premise

- **Where.** `economy.zig:17-28` (the WHY), `:774-905` (`PendingTimes`: 16 spin-locked stripes, `fold` on every inode read when non-empty, `absorb` by shape), the drain (`:906-939`), four gauges (`:58-72`).
- **Charter.** §5.2 names it: "the kernel's SETATTR ctime/mtime echo is absorbed when it matches committed/in-flight times (never blanket suppression)". So it is a requirement — *given the echo exists*.
- **Is it a FUSE-protocol workaround §4 makes unnecessary?** The echo is a consequence of `FUSE_WRITEBACK_CACHE`: the kernel sets `S_NOCMTIME` on every FUSE inode unless the connection has writeback cache AND the inode is a regular file; with `S_NOCMTIME` the kernel never authors c/mtime and never flushes a times-only SETATTR. `FUSE_HANDLE_KILLPRIV_V2` and the attr TTL do not touch this. Charter §4's INIT list never mentions `FUSE_WRITEBACK_CACHE`; the daemon owns accumulation, W1, overlays and staging itself (§6.2), which argues for write-through. If rung 3's INIT does not advertise writeback cache, the echo never arrives and `absorb`'s shape test + `meta_kv_times_echo_absorbed` are unreachable. The MAP itself still has a second, independent user named in the code: the write path's single-authority stamp (`economy.zig:759-766`, generic/003), which parks a write's mtime refinement instead of journaling per write — that stands regardless.
- **Secondary design note.** When the map is non-empty, every inode read on the volume takes a stripe spin lock (`:825-830` → `:816-821`): a shared-line RMW per read (charter §10) on a metadata read path; the reference used a lock-free map. At rung 3/4 the refinement belongs ON the per-ino RAM object (the FUSE inode table), making `fold` two plain loads and deleting the map.
- **Verdict.** **KEEP the mechanism; ASK OWNER at rung 3** whether writeback cache is in the INIT posture; if not, delete the echo arm (shape test, counter, G4 gate) and keep `PendingTimes` for the write path; move the residency onto the inode object when it exists.

### #13 — `destroy_inodes` batching

- **Where.** `inos.zig:229-287`: `stageDestroy`, `destroyEntryBytes` (sized by the admission's own `recordFrameLen`), `packDestroyEntries` (greedy under `entryPayloadCap`, "ONE member always goes"), `unlinkEntriesPerOp` with the 1.05 gate.
- **Verdict.** **KEEP.** It is charter-mandated (§5.2), the chunker derives from the arithmetic the commit enforces rather than a constant, and inos are never reused so there is no free-ordering rule to get wrong. The reclaim gather (FORGET-driven) that fills the batch is rung 3's.

### #14 — Parent-guard class derived from the record (vs "exclusive-both")

- **Where.** `economy.zig:185-227` (`SharedTimes` ⇒ `.shared`; `Put` ⇒ `.exclusive`; `parentGuardFor`), `:598-645` (rename: shared for Δtimes, exclusive for the cross-directory directory move's nlink `Put`s), `GuardPlan` (`:134-179`) as data in `dlm`'s request types.
- **Reference / port.** Not a divergence: the reference's `RoutedParentUpdate::SharedTimes` and the charter §5.2 ("Parent-time updates are Δtime merge records under the SHARED parent guard") are the same law; the port makes the class a function of the staged record, which is the cleaner statement of it.
- **One theoretical hole worth a comment, not a change.** Two creates of different names in one directory hold different `D{parent,name}` stripes and a SHARED parent; if their names collide on `hash54` both chain probes can pick the same `coll_seq` under no common exclusive guard; the pass's same-key co-queue exclusion (survey §2.2 step 1) is a debug assert. Probability ≈ 2⁻⁵⁴ per pair; the reference has the same shape. Note it beside `stageDentryPut`.
- **Verdict.** **KEEP.**

### #15 — Spin locks: inventory and paths

| Lock | Path | Hold | Judgement |
|---|---|---|---|
| `InflightRegistry.lock` (`journal_core.zig:599`) | `reserveRegistered` (`:637-649`) INSIDE the union leaf-lock window (stage A); `complete` (`:654-666`) on the lane; `completedUpto`/`minInflightStart` polls | O(1) append; `complete` is O(open) (`indexOf` `:668-671` + `orderedRemove`) | Reference shape (survey §2.7 "around the core, not inside it"). Charter §2 4c calls the reservation "a wait-free atomic, not a lock" — the core is; the registry is not. Acceptable: open ≤ windows in flight (small); lock order 4b → registry is consistent. Later: a bounded ring indexed by reservation order makes `complete` O(1). **KEEP.** |
| `Conveyor.inflight_lock` (`conveyor.zig:538`) | `track` (`:1310-1317`), `untrack`, `lookupInflight` per window | `self.inflight.put(gpa, …)` **may allocate under the spin lock** | Pre-size `inflight` to `queueCapacity(pages)` at init and use `putAssumeCapacity`; or index windows by cell as the lane does. **CHANGE NOW (small).** |
| `PendingTimes.Stripe.lock` (`economy.zig:785`) | every inode read while the map is non-empty; absorb/park/retire under the ino's I-guard | O(1) hash op | See #12. |
| `ReclaimQueue.lock` (`ledger.zig:549`) | one push per checkpoint, pops per barrier | O(1) | cold. **KEEP.** |
| `Checkpointer.tape_lock` / `maint_lock` (`checkpoint_cycle.zig:400, 423`) | cycle end / the dead queue | — | `maint_lock` goes with #4. |
| `alloc_core` overflow lock (`alloc_core.zig:163-191`) | forced-retirement overflow (cold) | O(1) | **KEEP.** |

Spin shape (`journal_core.zig:530-544`): TTAS with `yield` every 64 spins — honest for nanosecond sections; a descheduled holder costs the spinner a yield, not a burn. None of these sits on the §1.3 data path.

### #16 — `std.Io.Mutex` uses

`conveyor_core.zig:82` (the queue; the charter's sanctioned class), `backend.zig:264` (`smo_lock`; the serialized SMO task), `node_cache.zig:1318/1324/1358` (shard writers, clock ring, retired set — miss path only, decision 0002 row 7; hits are `ArcSwap` loads), and the `interior` mutex of every `sync/*` primitive (`rwlock.zig:33-40, 63`: nanosecond critical sections, never held across a park). Consequence worth knowing: every 4a guard and 4b leaf lock pays two interior lock/unlock pairs (acquire + release) even uncontended; a packed lock-word fast path (CAS, no interior mutex when nobody is queued) would halve the atomics per guard — a measured A/B for rung 4, not a correctness item. **KEEP.**

### #17 — `ArcSwap.load` is one shared-line RMW per read

- **Where.** `sync/arc_swap.zig:20-23` ("`load` is ONE `fetch_add` on the word"), plus a `fetch_sub` on the Arc's strong count at release; read on every `descend` level (`tree.zig:301-310` root slot; each node's snapshot slot; each shard's map slot).
- **Reference / port.** The `arc_swap` crate's fast path uses per-thread debt slots: a load is a plain load + a thread-local store + a re-check, with NO shared RMW; the port's split-count protocol is simpler, 64-bit, and model-checked (2,146 states) — a genuinely good piece of work — but every lookup from every lane does 2 RMWs on the ROOT slot's line and 2 per level below it.
- **Charter.** §1.3 "latch-free hot paths … arc-swap-style immutable snapshots"; §10 "no shared-line RMW on hot paths".
- **Proposed design.** Keep now (correctness first; rung 2 has no lookup storm to measure). At rung 4 (the read path) run the counted A/B: a hazard/debt-slot reader (`threadlocal` slot array, store pointer → re-read word → proceed; the storer scans slots before freeing) against the split count on a 32-lane lookup storm. The root slot is the line to watch; a `tree_root_loads` sharded counter makes the pressure visible in `.stats` for free.
- **Verdict.** **KEEP now; measured A/B at rung 4.**

### #18 — Hooks installed on the lane that run a checkpoint cycle on the lane thread

- **Where.** `journal_lane.zig:309-323` ("both run ON the lane thread, synchronously, and must never wait on this lane"); `checkpoint_past` (`:1351, 1367, 1382-1385`) → `backend.checkpointPast` (`:746-753`) takes `smo_lock` and runs a full cycle (SMO mutex, node locks, pool writes, pool `fdatasync`).
- **Reference / port.** Same shape (survey B7: the lane runs `checkpoint_past_region` under the SMO mutex); the charter §5.2 names "the hole checkpoints" as the durability lane's.
- **Why it may be poor.** For the wall of a cycle (barrier ≈ 2 ms + the flush) the lane reaps no CQE and acks nothing: a head-of-line block on the whole volume's durability. It is on the write-failure / apply-hole path only (rare by construction), and the lock order holds (the `smo_lock` holder never waits on the lane except at close, #8/#31).
- **Proposed design.** Keep; add `journal_lane_hook_wall_ns` (an exact-sum histogram) so a slow hole checkpoint is visible rather than inferred from ack latency.
- **Verdict.** **KEEP.**

### #19 — `RETRY_BUDGET = 256`, `READER_LOAD_RETRIES = 3`

`node.zig:76-88` states the reason on the line: 256 is a bug detector for a serialized, rare SMO (the pre-`yield` tripwire history is cited); 3 covers one append in flight per node for a reader's torn-frame race. Both are what §1.5 asks of a fixed bound. **KEEP.**

### #20 — The rollback / compensation arm exits silently on allocation failure

- **Where.** `conveyor.zig:1286` (in-window roll-out `removeOverlayRecordsLocked … catch {}`), `:1392` (`arena.alloc … catch return` — rollback abandoned), `:1406, 1415` (same), `:1430` (`catch {}`), `:1435` (`comps.append … catch {}` — a compensation DROPPED), `:1459, 1461` (`catch return`), `:1469-1478` (`catch break`), `:1499-1501`, `:1530` (`OwnedRec.fromRecordRef … catch continue` — the frame IS written to the journal at `:1535` while its RAM apply was skipped), `:1531` (`applyLocked … catch {}`).
- **Why it may be poor.** Each of these leaves RAM ≠ replay (records in an overlay whose entry will never land, or a compensation on disk that RAM never applied). The reference's `PassSentinel` latched FAILED whenever `applied_unrolled` could be true (survey §2.2, "RAM diverged from replay"). These are OOM paths, so rare, but §1.8 is about exactly the rare path.
- **Charter.** §1.8 "Must-stay-0 tripwire counters for every 'cannot happen' … Never a silent fallback"; §5.2 "Failure: a journal write error re-acquires leaf locks and rolls back per-record seq-conditionally".
- **Proposed design.** One helper `self.divergedFromReplay(label)` = `latch.failStop()` + a per-site `Tripwire` (`conveyor_divergence_<site>`), called at every exit above that cannot complete the roll-out/compensation; the in-window roll-out failure at `:1286` additionally marks the batch so no survivor is acked (the entry is still unwritten for that member — the hole law already covers the disk side).
- **Cost now vs later.** ~15 call sites, one helper. Later: a silent divergence found by fsck after a crash is the hardest class to attribute.
- **Verdict.** **CHANGE NOW.**

### #21 — A dead lane ring does not latch the volume

`journal_lane.zig:1168-1191`: `ringDied` logs, `failAll`s in-flight windows and refuses later ones; the FAILED latch only follows from three consecutive window verdicts (`noteJournalFailure`, `:1387-1389`). With one or two windows in flight and no later commit the volume is "dead lane, not FAILED". Charter §1.8: "a device fence latches the volume FAILED" — a ring that cannot `io_uring_enter` is terminal for the device. `ringDied` should call `latch.failStop()` (one line) and count `journal_lane_rings_dead`. **CHANGE NOW.**

### #22 — Test doubles living in product modules

- **Where.** `conveyor.RamLane` (`conveyor.zig:1552-1707`: a second, ~150-line implementation of stage B — group formation, `waitCompletedUpto`, verdicts, rollback hook, hole checkpoint, barrier, acks), `conveyor.Harness` (`:1780`), `smo.TestVolume` (`smo.zig:1222+`, used by `tree.zig`, `leaf_merge.zig`), `journal.VolatileImage` (`journal.zig:1147`), `checkpoint_cycle.TestVol` (`:1265`). None is gated by `builtin.is_test` (the only gates are `test_error_logs` fields).
- **Why it may be poor.** `SQUEEZEFS_TEST_*` seams (charter §11) hold windows in the REAL mechanism; `RamLane` is a parallel mechanism — a change to stage B must now be made twice, and the portable suite proves the double, not the lane. (Zig's lazy analysis keeps unreferenced `pub` decls out of a non-test binary, so there is no binary cost; the cost is protocol duplication.) `TestVolume`/`VolatileImage`/`TestVol` are fixtures, less of a concern, but they still make `tree.zig` tests depend on `smo.zig`'s test code.
- **Charter.** §11 "seams (`SQUEEZEFS_TEST_*`) hold the exact window a pin needs"; §1.9 no dead code; §12 "prefer plain procedures over generic machinery".
- **Proposed design.** Extract stage B's phase machine (`formGroup`/`foldLatePredecessors`/`runVerdicts`/`fanOut` + the barrier decision) into a portable `durability.zig` over a tiny `Device` seam; `journal_lane.Worker` drives it from CQEs and `RamLane` collapses to a ~30-line driver over `VolatileImage` — one protocol, two drivers, and the "portable power cut" row keeps its evidence value. Move the fixtures under `src/meta/testing/` (imported from `test` blocks only) or gate them behind `builtin.is_test`.
- **Verdict.** **CHANGE AT RUNG 3** (when stage B moves onto the executor's lane anyway).

### #23 — `catch unreachable` / `@panic` reachable from a handler

Census of product sites: `journal.zig:439` (`FrameIter` over a payload the parser validated — documented, fine); `node.zig:461` (header decode of an already-verified image), `:1090` (a comptime-sized layout); `economy.zig:284, 406` (encode into a bounded local buffer); `journal_lane.zig:779` (`bufPrint` of `sqz-jrnl<N>` into a fixed buffer); `record.zig:262, 325` (same pattern), `:2564` (`keyIndex`, test helper); `inos.zig:457`, `alloc_ext.zig:744, 833`, `superblock.zig:694, 792`, `geometry.zig:452`, `heap_law.zig:583, 966`, `builder.zig:137, 326` — tests or format-time. `@panic("oom")` only in tests (`alloc_core.zig:791`, `checkpoint_cycle.zig:1366`). `unreachable` in `conveyor.zig:810, 1429, 1529` and `smo.zig:1483` are "index of a member of a set we just built" — true by construction. Charter §1.8 is honoured. **KEEP.**

### #24 — Allocation per op / per batch on the commit path

Per tx: one arena (`tx.zig:409-421`, moved not copied into the queue entry — good). Per batch (stage A): an `ArenaAllocator` (`conveyor.zig:939-947`), a `Window` + its arena + two dupes (`:1061-1095`), one `OwnedRec` heap copy per record and a `group` ArrayList (`:1246-1267`), the aligned write image (`:1168` → `journal.zig:602-604`, 4 KiB-aligned so no bounce — good), and a hash-map `put` under a spin lock (`:1313`). Charter §12's allocation pins are data-path (W1 ≤ 4, warm read ≤ 1, …); the metadata commit is the sanctioned transaction class and the reference allocated comparably. **KEEP now;** at rung 3 give the lane a free list of `Window`s and aligned images sized by `queueCapacity`, and pre-size `inflight` (#15).

### #25 — `counters.max_stripes = 64` with round-robin stripe assignment

`counters.zig:20-23` gives a reason ("above 64 live threads stripes are shared and still exact"), and `stripeIndex` (`:42-47`) hands out stripes round-robin at first touch. On a > 64-thread daemon (a 128-CPU client has 128 handler lanes by §4's `nqueues = possible CPUs`) two HOT threads share a line — a shared-line RMW on the hot path by construction (charter §10). Derive the stripe count from `possible_cpus` (`next_pow2`, no cap; 128 × 64 B = 8 KiB per counter, ~200 counters ≈ 1.6 MiB) or assign stripes by CPU at first touch with a per-CPU table. **CHANGE AT RUNG 3** (when the lanes exist and the box is known).

### #26 — `commit_parked` gauge add/sub per commit

`conveyor.zig:708-711`: every committer does `commit_parked.add(1)` / `sub(1)` on one `Gauge` line — a contended RMW pair per commit (≈ 64 k RMWs/s on one line at the 32 k creates/s bar). Charter §10 "counters are per-thread sharded … no shared-line RMW on hot paths; `counters.Gauge` for levels". A level that is `entered − exited` can be two `ShardedCounter`s folded on read. Minor. **CHANGE AT RUNG 3.**

### #27 — Thread names

`thread_name.zig:36-44`: under a mount tag the base is truncated to 13; `sqz-uring-fs63` becomes `sqz-uring-fs6m?` — worker 6 and 63 are indistinguishable (the test at `:83-96` asserts only length and suffix). `tag & 0xf` (`:42`) collides mount slots ≥ 16. `sqz-uring-fs` (`uring_fs.zig:95`) is not in `classes` (`:62-73`) so pool workers are attributed to `other`. Charter §9 lists the class names; the uring file layer should be named under one of them (`sqz-blk` is the blocking/offload pool) or get its own class row. Cheap. **CHANGE NOW** (a rename + a class row + widen the tag to two hex digits or refuse slots ≥ 16 loud).

### #28 — The `.tuning` knob class

`knobs.zig:60-67` defines `tuning` beside `ab_lever`, `test_seam`, `derived_override`, …; `SQUEEZEFS_META_FLUSH_INTERVAL_MS`, `SQUEEZEFS_TIMEOUT`, `SQUEEZEFS_CHECKPOINT_MAX_AGE_MS`, `SQUEEZEFS_PARKED_GATE_ASSIST_MS`, `SQUEEZEFS_FLEET_SHARE` are `.tuning`. Charter §1.6: "Knobs are classified: delete / `SQUEEZEFS_TEST_*` (seams) / measurement A/B levers (logged at mount when non-default)." A "tuning" knob the user is expected to set is the posture the charter argues against ("The user never chooses a data path"). Either fold `.tuning` into `.ab_lever` (so they are logged at mount when non-default) or document the fourth class in `knobs_and_rails.md` with the rule that distinguishes it. **ASK OWNER.**

### #29 — The missing rail

`tests/rails.zig` implements the knob census and the inverse census. Charter §11 also names "no runtime literal in product code" as a static rail. Given #1/#2 and the census in §3 below, this rail is the cheapest way to keep §1.5 honest: a scan for bare integer literals ≥ 2 in non-test product lines that are not inside a `comptime`/`const` with a `///` comment above, with an explicit exemption list. **Owed.**

### #30 — Things that are right as they are

- **`SpaceAudit`'s 8-cycle wedge rule** (`alloc_core.zig:770`, `checkpoint.zig:88-96`): "a resolvable pinned floor converges in 2 cycles; 8 leaves only a tail nothing can discharge" — a stated physical reason; `COVER_CYCLES_MAX = 8 × 8`, `FIXPOINT = 2 × 8`, `HOLE = 8` all derive from it (`:97-111`). Note the rung advances only on `barrier_now` cycles (`checkpoint_cycle.zig:825-835`) — correct: a non-barriered cycle has not tested whether the tail can move.
- **The FAILED lattice** (`journal.zig:1067-1113`): two consecutive-failure counters with the rationale for why the barrier counter is separate (`:1067-1072`), plus `failStop` for protocol terminals. Matches survey §3.5 minus PR fencing (rung 5/6). (#21 adds one input.)
- **32 ledger slots** (`ledger.zig:48`): format (charter §5.1 says 32 — 128 KiB = one mount read).
- **The 2 s tick** (`sync/tick.zig:23`): charter §9 names it.
- **`STRICT_TICK_PERIOD_MS = 100`** (`checkpoint.zig:80-83`): "a tenth of the max age" — derived.
- **`park_backstop_ns = 100 ms`** (`journal_lane.zig:88-93`), **`inboundCap`** (`:106-114`), **`first_touch_spins`** (`dlm.zig:108-113`), **`spin_burst`** (`journal_core.zig:530-531`): each carries its reason.

### #31 — `waitHeadStable` under `smo_lock`

See #8(a). Add `assert(windowsInflight == 0)` (or a tripwire) at the final tick's entry; the ordering in `backend.close` (`:847-860`) is correct today and the comment there says why — make the dependency executable.

---

## 2b. The wait-for graph as implemented (charter §2, survey §2.17) — re-derived from the code

The survey's table was written against the reference; the port adds two edges (the SMO pool path and the lane's `smo_lock` take) and removes one (no pool writes for user windows). Re-derived:

| Holder (thread) | Holds | May wait on | Must never wait on | Where |
|---|---|---|---|---|
| committer (any thread) | its 4a `GuardSet` (co-owned by the queue entry) | the conveyor `Done` oneshot (`park`) | node locks, ring space, `smo_lock`, the lane | `conveyor.zig:599-613, 708-712` |
| conveyor pass (the election winner's thread — a committer's, until rung 3) | 4a (held, never acquired); union 4b leaf write locks; the ring admission; the registry spin lock inside the 4b window | ring admission with NO node lock (`admitUserBudget`, parks on `space` + 10 ms tick, #9); 4b locks ascending; `smo_lock` via `checkpointNow`/`checkpointPast` holding NO node lock (`:1032, 1184, 1350`); the `inflight` spin lock | device I/O or ring space under a node lock (holds: submit is after `unlockAll`, `:1130, 1194`); 4a acquisition; the lane's completion | `conveyor.zig:925-1197` |
| journal lane (`sqz-jrnl<N>`) | window reservations; the registry spin lock in `complete`; `smo_lock` + the cycle's node locks INSIDE the `checkpoint_past` hook; in the rollback hook: 4b leaf locks ascending | its own ring's CQEs (parks in the ring); the registry's completed prefix (advanced by itself and by SMO `commitEntry` + `completed` kick); `smo_lock` (hole checkpoints, #18) | — (no node-lock holder waits on the lane; the `smo_lock` holder never waits on the lane except #31) | `journal_lane.zig:1260-1402`; `backend.zig:669-672, 746-753` |
| checkpoint/SMO task (own thread until rung 3) | `smo_lock` (around every `runOnce`, `checkpoint_cycle.zig:1232-1237`); parent-then-child 4b locks briefly; checkpoint-class admissions (never park: `tryAdmit`, refusal ⇒ defer) | 4b locks (RAM-only holders); pool device I/O OUTSIDE node locks (`appendFrozen`, images, pages, barrier #1, SMO entries via `smoWriteHook`); the registry spin lock (`reserve`, `complete`) | ring admission as a park; 4a; the lane (holds except at close, #31) | `checkpoint_cycle.zig:700-847`; `smo.zig:176-194, 889-1000` |
| `uring_fs` pool workers | nothing of the metadata plane | the device | — | `device_loader.zig:145-178` |
| times drain (rung 3) | its own 4a `lockMany` set | the conveyor oneshot | `smo_lock` | — (not yet wired; #10) |
| clock eviction | one node's lifecycle CAS; the shard writer mutex / clock mutex (miss path) | nothing | — | `node_cache.zig:1322-1348` |

Edges: committer → pass (oneshot); pass → ring space (holding nothing) → checkpoint task (`afterDurableBarrier`) → device; pass → `smo_lock` (space retry / abandon, holding nothing); lane → device (its ring); lane → registry prefix → {lane itself, SMO `commitEntry`}; lane → `smo_lock` (hole checkpoint); checkpoint task → device (pool), → registry (spin, O(1)), → lane only as a KICK (never a wait). The SMO's pool write is a synchronous wait on the pool's completion while holding `smo_lock` but NO node lock (P4 is after release, `smo.zig:998`) — admissible. The graph is acyclic given (i) the lane's `smo_lock` wait is never while the task waits on the lane (true except `waitHeadStable`, ordered by `backend.close`, #31) and (ii) `checkpointNow` from the pass is called with no node lock and no admission (`conveyor.zig:1014-1015`). The port's acyclicity therefore rests on the same two laws as the reference's (`backend.rs:97-123` per the survey) plus the close ordering — write the close-ordering law down as an assertion.

One subtle order the code gets right and nobody documents: the registry spin lock is taken INSIDE the 4b window by the pass (`reserveRegistered`) and OUTSIDE any node lock by the lane (`complete`) and the SMO (`reserve` inside its own parent/child window). So the partial order is 4b → registry on both writers; no holder of the registry lock ever takes a node lock. Add that sentence to `journal_core.zig:594-596`.

---

## 2c. Red-first repro for each CHANGE NOW item (charter §11)

| # | The red test (in the module's own test block, portable unless noted) |
|---|---|
| 4 | Backend (Linux): format → open → commit N txs onto one leaf until `overlay_bytes ≥ writeback_delta_bytes` → `task.kick(.threshold)` → `runOnce` → expect `ckpt.stats.meta_kv_maintenance_items > 0`, `device_loader.stats.appends > 0`, `meta_kv_checkpoints` unchanged, `tree.maintenancePending() == false`. Today: items = 0, the tree queue still holds the node, and after `queueCapacity` such commits `meta_kv_maintenance_queue_full > 0`. |
| 6 | `smo.TestVolume` with a barrier hook that fails once + a node past `fold_capacity` → `smoReplace` → expect `error.BarrierFailed`, no interior `Put` in the ring, claimed extents back to the free count, `latch.barrier_failures == 1`, parent routing unchanged. Today: the SMO completes and the pointer record is in the ring. |
| 3 | Backend: coalesced mode; one commit; one cadence cycle WITHOUT `barrier_now` (ledger written, `reclaim` holds one entry); then the lane's coalesced barrier lands (`fsync()`); expect `core.reusableUpto() == last_ledger_tail` with `meta_device_syncs` unchanged. Today it stays at the previous tail until the next task barrier. Plus a rail-style test that `KickMark.flush` has a product caller (or delete it). |
| 5 | After the refactor the test is the deletion: `alloc_core` posture rows move to `heap_law`; one test asserts `meta_kv_heap_full_cycles` increments exactly once per deferring cycle through the ONE path. |
| 20 | `std.testing.FailingAllocator` on the conveyor's `gpa` armed at the rollback's N-th allocation with `RamLane.failWindow` ⇒ expect `latch.isFailed()` and the named tripwire fired; today the rollback returns silently and the leaf still holds the window's records (`snapshot.newestSeqOf(key)` inside the span). |
| 9 | Harness: park a pass at ring admission (tiny ring, `hold`), run `be.checkpointNow()` from another thread ⇒ the pass must resume within one `sync.Notify` wake (assert `conveyor.stats.full_stalls` rose by exactly 1 and the resume time ≪ 10 ms, measured with `clock.monotonicNs`, no sleeps). Today: resumes at the 10 ms tick. |
| 21 | Lane (Linux): seam that makes `io_uring_enter` fail once ⇒ `latch.isFailed()` immediately, `journal_lane_rings_dead == 1`. Today: not failed until three later windows fail. |
| 2 | A tie test in `checkpoint.zig`: `journal_lane.default_flush_interval_ms` is gone; the registry's `.default` string for the row parses to `DEFAULT_FLUSH_INTERVAL_MS`. |
| 15 | `conveyor.init` with `FailingAllocator` after init: N windows ⇒ zero allocations from `track` (pre-sized). |
| 27 | `thread_name`: `commNameWith("sqz-uring-fs63", 3)` keeps both digits; `classify` of the pool worker's name is not `other`; a tag ≥ 16 refuses loud or encodes two digits. |
| 31 | `checkpoint_cycle`: `tick(final_cycle = true)` with a window still in flight ⇒ the tripwire fires (not a wedge). |

---

## 3. Constants census — bare numerics in the audited modules, judged

Rule applied (charter §1.5): a fixed number needs a physical/format/kernel/measured reason ON THE LINE or a derivation with a tie test.

| Constant | Where | Reason on the line? | Judgement |
|---|---|---|---|
| `default_flush_interval_ms = 50` | `journal_lane.zig:98`, `checkpoint.zig:86` | "legacy v2 flusher cadence reused" | **No** — #1/#2 |
| `PARK_TICK_NS = 10 ms` | `conveyor.zig:127-131` | "the reference's 10 ms floor" | **No** — becomes a pure backstop after #9; then either the sync tick or a stated lost-wake bound |
| `DEFAULT_TIMEOUT_S = 30` | `conveyor.zig:124-126`, `knobs.zig:131` | "wedged-not-failed class" | Weak — tie it to the §4 mount supervisor's 30 s (the two must agree or the supervisor aborts before the lattice escalates) |
| `PENDING_TIMES_DRAIN_BATCH = 128` | `economy.zig:714-720` | "the `destroy_inodes` batching shape … far inside the 128 KiB cap" | Weak — derive from `dlm.max_requests` (#10) |
| `PENDING_TIMES_DRAIN_CAP = 512` | `economy.zig:722-726` | "bound the crash-loss window by count" | Acceptable as a bound; 4 × batch should be stated as the formula |
| `stripe_count = 16` (`PendingTimes`) | `economy.zig:779-782` | "keeps that cost off any single cache line" | Weak; goes away with the per-ino residency (#12) |
| `max_requests = 16` | `dlm.zig:146-149` | "4× headroom over EXCHANGE; bounds the stack frame" | Stated; conflicts with #10 |
| `stripe_load_factor_inv = 16`, `dlm_stripes_shipped = 4096` | `dlm.zig:56-61` | derived law + shipped floor | Yes |
| `COMMIT_RETRY_BUDGET = node.RETRY_BUDGET = 256` | `conveyor.zig:121-123`, `node.zig:76-81` | "bug detector" | Yes |
| `READER_LOAD_RETRIES = 3`, `PROJECTION_REFRESH_EVERY = 8` | `node.zig:82-88` | one append in flight; "small against the budget" | Yes / acceptable |
| `JOURNAL_FAILURE_LATCH = 3` | `journal.zig:76` | "§4.4 pt 4 'repeated'" | Yes |
| `RING_MIN/MAX = 8/32 MiB` | `journal.zig:78-82` | measured runway (139 K entries ≈ 4.3 s) | Yes |
| `MAX_ENTRY_LEN = 128 KiB`, `PAGE_LEN`, `PAGE_HDR_LEN = 24`, `ENTRY_HDR_LEN = 20` | `journal.zig:48-62` | format | Yes |
| `ring_entries` (lane SQ depth) | `journal_lane.zig:81-86` → `uring_fs.journal_ring_entries` | window shape | Yes |
| `park_backstop_ns = 100 ms` | `journal_lane.zig:88-93` | measured ratio to the round trip and the cadence | Yes |
| `inboundCap` clamp 64..8192 | `journal_lane.zig:106-114` | pages; tiny rings; 32 MiB = 8192 pages | Yes |
| `queueCapacity` clamp 64..8192 | `conveyor.zig:181-187` | same law as the lane | Yes |
| `BATCH_TXS_FLOOR = 64`, `BATCH_BYTES_FLOOR = 256 KiB`, `/16` | `conveyor.zig:110-120` | shipped floors + liveness margin | Yes |
| `STRICT_TICK_PERIOD_MS = 100` | `checkpoint.zig:80-83` | tenth of the max age | Yes |
| `CHECKPOINT_MAX_AGE_MS = 1000` | `checkpoint.zig:60-68` | charter §5.3 | Yes |
| `CHECKPOINT_MAX_DIRTY_NODES_FLOOR = 4096`, `/32` | `checkpoint.zig:70-78` | shipped floor; budget share | Yes |
| `PENDING_FREE_FORCE_CYCLES = 8` and its multiples | `checkpoint.zig:88-111` | convergence argument | Yes |
| `waitHeadStable` 64 iterations | `checkpoint_cycle.zig:1061` | none | **No** — tie to `COVER_CYCLES_MAX` or state why |
| `DEFAULT_CACHE_BUDGET = 512 MiB`, `/16`, shard `/64` clamp 16..4096 | `node_cache.zig:91-126` | shipped floor; budget share; nodes per shard | Yes |
| `MAX_INFLIGHT_LOADS = 4 × 64` | `node_cache.zig:111-114` | "4 outstanding loads per stripe thread" | Acceptable; inherits #25 |
| `DEFAULT_WRITEBACK_DELTA_BYTES = NODE_PAGE` | `node_cache.zig:99-101` | one bset page | Yes (and inert today, #4) |
| `OVERLAY_TAIL_MAX = 8`, `FoldMemo` 8 cells | `node_snapshot.zig` (per survey §4.1) | bounds the per-apply clone; budget accounting | Yes |
| `MAINTENANCE_QUEUE_FLOOR = 64`, `CAP = 1 << 20` | `tree.zig:176-182` | "smallest cache (16 shards × 1 node) still queues a storm"; "one slot per node of the largest budget … 2^20 bounds the table at 16 MiB" | Yes |
| `counters.max_stripes = 64` | `counters.zig:20-23` | "above 64 live threads stripes are shared and still exact" | Stated but wrong for the hot path on > 64-thread boxes (#25) |
| `tick_ns = 2 s` | `sync/tick.zig:23` | charter §9 | Yes |
| `arc_swap` 48 pointer bits, flush at 2¹⁵ | `arc_swap.zig:40-50` | VA ceiling; headroom argument | Yes |
| `NLINK_MAX = 65_000`, `SYMLINK_TARGET_MAX = 4095` | `economy.zig:98-107` | ext4 `LINK_MAX`; `PATH_MAX − 1` | Yes |
| `RENAME_ENTRIES_PER_OP_GATE = 1.02`, `UNLINK… = 1.05` | `economy.zig:700-702`, `inos.zig:272-277` | measured 1.003 / 1.018 | Yes |
| `ROOT_LEDGER_SLOTS = 32` | `ledger.zig:48` | format | Yes |
| comm tag `& 0xf` | `thread_name.zig:42` | none | **No** (#27) |

---

## 4. GOOD decisions worth protecting

1. **Monotonic admission totals** replacing the reference's ABA-blind byte-count CAS (`journal_core.zig`, decision 0002 row 1; model 6, 450 states). The single most valuable correction in the port.
2. **The lane is lane-native only** (`journal_lane.zig:33-37`): no pool-write fallback, no ringless degrade — a box that cannot give the lane a ring refuses at spawn. Stronger than the reference and exactly charter §1.2.
3. **The barrier is an `FSYNC|DATASYNC` SQE on the lane's own ring** (`journal_lane.zig:1426-1428`), never a blocking `fsync(2)` that would stall every CQE behind it.
4. **Stage B as an explicit phase machine with no blocking wait** (`advanceDurability`, `journal_lane.zig:1260-1291`): prefix waits park in the ring via the register-recheck token (`pollCompletedUpto`, `journal_core.zig:700-710`), acks are delivered on the lane thread. This is the charter's "park IN it" done right.
5. **Bounded conveyor queue cells** (`conveyor_core.zig:76-122`; `queueCapacity`) — no per-enqueue allocation, group enqueue under one lock hold, the leader protocol model-checked (6 models). Better than the reference's `Vec`.
6. **The tx arena moves into the queue entry** (`tx.zig:402-421`): records are never copied between staging and the pass.
7. **DUR-3 as a ticket epoch** (`checkpoint_cycle.zig:28-36, 524-556`): three watermarks on one clock, strictly post-barrier. (Finish it with #3.)
8. **Pure deciders with tie tests**: `decideCheckpoint`, `SpaceAudit.tick`, `heap_law.admitHeapLocked`/`decideSpaceRetry`, `resolveCaps`, `checkpointTickPeriodMs` and the whole §4.1 derivation family — decision logic with no side effects, each restating its formula in a test.
9. **`heap_full` is a posture, never terminal** (`heap_law.zig:24-31`) with the retract-on-refusal fix for the mid-wave fixpoint (`:158-176`) — the charter's space law stated precisely.
10. **The tree's `MaintenanceQueue`** (`tree.zig:211-290`): lock-free, model-checked (stamp-before-payload weakening fails). The right structure — it only needs a consumer (#4).
11. **`ArcSwap` split-count protocol** (`arc_swap.zig`): model-checked, deferred free to the last reader, overflow tripwired. Correct by construction; the contention note (#17) is about speed, not safety.
12. **`SmoJournal` frames pre-built before the lock window** (evidence 2b, "nothing after the swap can fail") and **publish → flip → retire** ordering with exact per-record `entry_floor` stamps.
13. **Guard class as data** (`economy.GuardPlan`, `parentGuardFor`): the record kind decides the 4a mode; the lock manager's canonical order is the one place it is applied (`dlm.zig:22-34`).
14. **Typed refusals** (`smo.Refusal`, `heap_law.NoSpace`, `tx.BuildRefusal`) and one errno row per module instead of string-parsed outcomes.
15. **The `FailureLatch` with a separate barrier counter** and the sentence explaining why (`journal.zig:1067-1072`).
16. **`inos.packDestroyEntries`** sizing chunks from the admission's own `recordFrameLen` rather than a guessed constant.
17. **No `@panic`/`unreachable` on a handler-reachable path** (#23); every "cannot happen" that exists is a labelled `Tripwire`.
18. **The knob registry with both census rails** — the reference's convention test, done once, in the gate.
19. **`O_DIRECT` images allocated 4 KiB-aligned** (`journal.zig:602-604`) so the lane never bounces a product write.
20. **The close ordering** (`backend.zig:847-860`: conveyor quiesce → lane shutdown → task stop) — correct; make it executable (#31).

---

## 5. Ordered change list

**Now (before anything builds on 2b):** #4 (wire the threshold drain to the trees' queues; delete the ArrayList queue) · #6 (SMO barrier propagates; latch) · #3 (lane barriers feed DUR-3; delete the dead flush kick) · #5 (one posture; delete `HeapPromises`/`recordCycle`) · #20 (divergence exits latch + tripwire) · #9 (one space notify) · #21 (dead ring latches) · #2 + #1's line (one source for 50, with its WHY) · #15's `inflight` pre-size · #27 (names) · #31 (assert).

**Record (decision 0002 ledger / evidence 2b deviations):** #7 (SMO entries via the pool until rung 3; why; the soundness argument above) · #22 (RamLane as a protocol duplicate, to be extracted).

**Rung 3:** #7 (lane-native SMO writes with an on-lane check) · #10 (raise `max_requests` or a large-plan API, tie to the drain batch) · #11 (the `Io`-carrying guard ctx) · #22 (extract stage B; move fixtures) · #24 (window/image pools) · #25 (stripes from possible CPUs) · #26 (sharded parked level) · #12 (decide on writeback cache ⇒ keep or delete the echo arm; residency on the inode object).

**Rung 4 (measured):** #17 (debt-slot reader A/B on the root slot) · #16 (lock-word fast path for the rwlock).

**Ask owner:** #28 (the `.tuning` class) · #12 (writeback cache posture) · #1 (keep coalesced as the default — recommended yes).

**Owed rail:** #29.
