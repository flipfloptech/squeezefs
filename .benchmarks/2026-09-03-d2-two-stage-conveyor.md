# 2026-09-03 — D-2: the two-stage commit conveyor (DLM #2 ≡ write #5)

**Branch** `perf/d2-two-stage-conveyor` (worktree off the D-1b tip
`63c79b9d`, the branch landing on dev as `perf/d1b-publish-plane-batching`).
Measure `e6a227ed` → RED `56328c20` → fix `a33bb010` → field row (below).
Campaign: `docs/design-e2e-perf-audit.md` §3 board **#2** ("the conveyor is
one serialized server incl. the device write"), Appendix C write #5 /
Appendix D DLM #2; baseline `.benchmarks/2026-09-02-d1b-publish-plane-batching.md`
(the fleet row whose wall moved onto the conveyor: ρ 0.80 → 0.97); the
original conviction `.benchmarks/2026-08-01-rewrite-publish-drain.md` (ρ ≈
0.92, 0.78 ms pass vs 0.17 ms leaf-lock floor). Contracts
`tests/conveyor_two_stage_tests.rs`.

## The conviction (measured first, D-1b tip)

The M7 conveyor pass (`design-metadata-throughput.md` §5.5 D5) was ONE
serialized server per volume: drain → Σ admission → union leaf locks (RAM
apply) → unlock → journal ring write → completed-prefix wait → (strict)
barrier → fan-out. The ring write's COMPLETION sat inside that server's
service time, so while a pass waited on it the next batch's apply could not
start. The D-1b authority (deferred cadence, so no barrier in the pass):

| leg | passes | `tx_queue_wait` | `pass_total` | `pass_leaf_locks` | `journal_ring_write` | ρ = Σ pass_total ÷ wall |
|---|---|---|---|---|---|---|
| B1 d1b | 9,539 | 801 µs | 752 µs | 86 µs | **650 µs** | **0.967** |
| B2 d1b | 9,128 | 764 µs | 776 µs | 91 µs | **672 µs** | **0.980** |

650 µs to complete an ~860 B buffered write into page cache is not device
time — it is the `uring_fs` round trip (queue hop → worker submit → CQE →
oneshot wake onto an sqz-meta lane) on a box at load 85. The two-stage
lever moves it off the serialized server whatever its nature.

**The instrument** (`e6a227ed`): `uring_fs::arm_device_latency(path, write,
barrier)` — the SLOW-device seam (the existing stalls are PARKED-device
seams): every write / `fdatasync` on the path is held on a deadline lane
for its latency and then admitted for real, so concurrent requests overlap
as they would on a device with that service time and the device term is a
controlled constant `D`. Plus the D-2 gauges `meta_conveyor_windows_inflight`
/ `_hwm` (applied-but-unacked windows; 1 = serialized, ≥ 2 = overlapped)
and `meta_conveyor_durability_passes`, and `lock_phase_ns.leaf_lock_hold`
(the 4b union hold — the "never across device I/O" tripwire).

In-process rows on the SERIAL conveyor (release, 32-CPU dev box,
file-backed KV sandbox, 16 committers × 32 creates in one directory):

| row | tx/s | passes | tx/pass | queue µs | locks µs | write µs | barrier µs | pass µs | ρ | hwm |
|---|---|---|---|---|---|---|---|---|---|---|
| deferred D=0 | 51,507 | 64 | 8.0 | 133 | 75 | 68 | — | 147 | 0.95 | 1 |
| deferred D=500 µs | 10,947 | 65 | 7.9 | 670 | 86 | 617 | — | 708 | 0.98 | 1 |
| deferred D=2 ms | 3,203 | 65 | 7.9 | 2,303 | 105 | 2,321 | — | 2,439 | 0.99 | 1 |
| strict D=500 µs | 4,221 | 65 | 7.9 | 1,796 | 80 | 622 | 1,148 | 1,855 | 0.99 | 1 |

`tx/s = tx-per-pass ÷ (apply + D)` exactly; the 500 µs row is the field's B
leg made a constant (pass 708 vs 752, locks 86 vs 86, write 617 vs 650,
queue 670 vs 801, ρ 0.98 vs 0.97).

## The fix — stage A submits, stage B awaits/barriers/acks in journal order

* **Stage A — the apply pass** (`conveyor_pass_task` / `run_batch`): drain →
  admission → union leaf locks → revalidate → pre-images → ONE contiguous
  reservation → RAM apply → unlock → **encode + SUBMIT** the surviving
  entries (`JournalRing::submit_entries_batch` →
  `uring_fs::submit_write_at_batch`: one queue push, no wait) → hand a
  `ConveyorWindow` to stage B (enqueue then `try_lead`, no await between —
  the loom-modeled `ConveyorCore` protocol, a second instance). Stage A
  never waits on the device; `pass_total` is now the serialized service
  time only (locks + encode).
* **Stage B — the durability lane** (`durability_lane_task` /
  `run_windows`): one per-volume detached task (Weak upgrade per group,
  release before fan-out, exit via release-then-recheck) drains windows in
  handoff order and takes each GROUP — the head window awaited + every
  successor whose write has already landed (`WriteCompletion::poll_done`) —
  through: complete the reservations, ONE completed-prefix wait, the §4.4
  pt 4 hole checkpoints, ONE strict barrier, then the members' terminal
  outcomes in journal order. On the strict cadence that is group commit of
  BARRIERS (the `SyncCoalescer`'s shape moved downstream): the in-process
  strict row runs ~2 windows per barrier.
* **N in flight**: bounded by the ring's admissible capacity (§4.4 pt 5 —
  every window holds a registered reservation, stage A parks on admission
  before any node lock), never a constant.

**Unchanged, and how** (the module doc in `backend.rs`, "The two-stage
commit conveyor", carries the full argument):

* One tx = one checksummed entry: the window's entries are the same N
  ordinary entries in the same contiguous reservation — zero on-disk
  change; whole-tx atomicity + torn-write immunity are per entry (§4.10).
* Ack only after landed (deferred, the D0 law — stage B awaits the write)
  or barriered (strict). **Acks in journal order** — restated as the law it
  serves: a later window is never answered while an earlier window's entry
  is unlanded (an entry is chain-reachable only through its predecessors;
  a hole ahead of it strands it). In-order groups + the completed-prefix
  wait + the hole checkpoints BEFORE any ack are the pre-D-2 tail's
  discipline applied per window.
* DLM guards stay co-owned by the queue entries until the STAGE-B terminal
  outcome (never released at apply) — Issue 13 untouched.
* The D0 fail-stop lattice fires from stage B exactly as from the pass:
  `note_barrier_failure` at the strict barrier, `note_journal_failure` on a
  failed write / stuck hole checkpoint, `JOURNAL_FAILURE_LATCH` consecutive.
* Panic guards per stage (`PassSentinel` / `LaneSentinel`): abandon the
  reservations (never a `completed_upto` wedge), EIO every member, fail out
  the queue behind the dead stage, fail-stop when an applied window's write
  outcome is unknown.
* **Lock order 4b**: leaf-lock takers are {stage A, stage B's failed-write
  rollback arm, the checkpoint/SMO task}; stage A drops its locks BEFORE
  the submission (`leaf_lock_hold` p-tail ≪ D on every row below); the
  rollback arm is `rollback_failed_tx` verbatim — the §4.4 pt 4
  seq-conditional rollback the design wrote for CONCURRENT committers
  whose writes complete out of apply order, exactly the population two
  stages recreate (only Δtime merge records can share a key across
  windows; every other same-key writer is still excluded by the failed
  window's guards). No node-lock holder ever waits on stage B; stage B
  holds node locks only while waiting on other node locks ascending.
* The checkpoint tail rule needs no change: a window's reservation stays
  registered from A's in-lock reserve until B observes the write's
  outcome, so `min_inflight_start` holds the tail — and `reusable_upto` —
  behind every applied-but-unlanded window.

## Measured (in-process, release, same rows on the fix)

| row | tx/s | gain | passes | dur. passes | tx/pass | queue µs | locks µs | write µs | barrier µs | pass µs | ρ(apply) | hwm |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| deferred D=0 | 50,532 | 0.98× (par) | 81 | 69 | 6.3 | 63 | 83 | 69 | — | 89 | 0.71 | 2 |
| deferred D=500 µs | **21,360** | **1.95×** | 201 | 162 | 2.5 | **56** | 37 | 572 | — | **40** | **0.34** | 10 |
| deferred D=2 ms | **6,977** | **2.18×** | 254 | 192 | 2.0 | **32** | 33 | 2,076 | — | **37** | **0.13** | 12 |
| strict D=500 µs | **6,312** | **1.50×** | 131 | 68 | 3.9 | **28** | 46 | 1,147 | 1,158 | **51** | **0.08** | 5 |

`tx_queue_wait` collapsed 12–70× (670 → 56, 2,303 → 32, 1,796 → 28 µs);
the serialized server's service time fell to the leaf-lock window (708 →
40 µs) and its utilization from 0.98 to 0.34 / 0.13 / 0.08; the device
term now shows where it lives (`journal_ring_write` 572 / 2,076 µs, awaited
by the lane while the pass keeps applying — hwm 10–12). The closed loop of
16 committers caps the throughput gain at ≈ 2× (the serialized conveyor
ping-pongs two groups of C/2 per device period; two stages keep the whole
population in flight); the D=0 row is par (the local `uring_fs` round trip
is the same size as the apply, so there is little to overlap; more, smaller
passes).

**Contracts** (`tests/conveyor_two_stage_tests.rs`, 8; RED on `56328c20`,
green on `a33bb010`, debug): 8 one-tx windows at D = 20 ms complete in
20.8 ms (was 164.9 = 8 × 20.5) with hwm 8 (was 1), `pass_total` 29 µs (was
20.5 ms), 0 leaf-lock holds at/above D/2; closed loop 16 × 8 at D = 5 ms
sustains 14.8 txs per device period (was 7.57 = C/2); with window 0's ring
write PARKED, the seven applied-and-landed windows behind it stay
unanswered until it lands (acks never leave journal order), inos monotone
in enqueue order; strict: with the barrier parked, all 4 windows are
applied + submitted and none is acked, release ⇒ all ack; EBADE at the
barrier with six overlapping windows ⇒ every committer Err, `failed`
latched, fence counter trips, gauge closes; kill -9 with several windows in
flight (8 committers × one-tx windows against a 3 ms device, ×5 rounds):
every acked create resolves after remount, replay idempotent.

Suites green (`--test-threads=1`, debug): the new suite (8), `conveyor_tests`
(10), `crash_contract_tests` (25), `kv_journal_tests` (21),
`write_commit_crash_tests` (2), `durable_block_refs_tests` (17),
`publish_plane_batching_tests` (6), `meta_ship_owner_dispatch_tests` (4),
`mw_cowriter_free_tests` (49), `f46_kvmap_stream_publish_tests` (2),
`derivation_sweep_tests` (37), `kernel_op_economy_tests` (2),
`mount_writer_guard_tests` (43), `kv_backend_tests` (36), `crash_kill_tests`
(9), `fuse_watchdog_teardown_tests` (14), `dur_metadata_integrity_tests`
(10), `audit_instruments_tests` (22), `op_trace_tests` (16),
`publish_phase_tests` (5), `wedge_census_tests` (2),
`kv_smo_crash_completeness_tests` (12), `staged_crash_recovery_tests` (7),
`dismount_teardown_tests` (5), `kv_freeze_wedge_tests` (5),
`write_commit_economy_tests` (2), `publish_drain_economy_tests` (7),
`dlm_multi_writer_tests` (16), `dlm_cowriter_tests` (18),
`mw_cowriter_lane_tests` (26), `meta_ship_tests` (15). `cargo clippy
--all-targets --all-features -- -D warnings` + the shipped-config clippy
clean, `cargo fmt --check` clean. `task check` deferred (batched by
instruction).

## Field row — the D-1b fleet rig, A-B-B-A (measured-simulated tier)

`.benchmarks/rigs/2026-09-02-d1b-fleet-row.sh` (D-1b's rig, verbatim) on the
32-CPU dev box (117 GiB), tcp devsub (nvmet-tcp on `127.0.0.1`, zram OSS
2 × 64 GiB), `tests/mw_fleet.sh create N=1 --multi-writer --cowriters=8` per
leg, torn down to zero residue between legs. Row: **8 co-writers × 24
concurrent `dd bs=1M count=128 conv=fsync` streams from `/dev/zero`**
(device term removed by design), per-member directories. **A** = the D-1b
field-row binary `ec921b87` (code-identical to the D-1b tip `63c79b9d`; the
diff is one doc line), **B** = this branch `a33bb010`; both `--version
… profile release`, recorded in the create logs. Analysis:
`.benchmarks/rigs/2026-09-03-d2-fleet-analyze.py`. Box: no other daemon or
fleet during the run (the rig refuses on `task check`/`cargo`), but NOT as
quiet as D-1b's session — a sibling agent's substrate and a 10-minute-old
load tail were present; loadavg at leg start **8.3 / 11.4 / 8.2 / 6.7**
(D-1b's legs started at 3–9). Depth on the fleet = 2.

**The authority's conveyor** (the shape D-2 exists to move):

| leg | passes | ρ(apply) | `pass_total` | `pass_leaf_locks` | `tx_queue_wait` | `journal_ring_write` | `window_lane_wait` | commit latency (queue + window) | windows hwm | windows / dur. pass |
|---|---|---|---|---|---|---|---|---|---|---|
| A1 d1b | 9,252 | **1.001** | 725 µs | 95 | **696** | 618 | — | 1,421 µs | (1) | — |
| B1 d2 | 11,180 | **0.125** | **112** | 100 | **325** | 1,306 | 762 | 1,750 µs | **24** | 1.14 |
| B2 d2 | 11,035 | **0.140** | **101** | 87 | **285** | 1,011 | 621 | 1,403 µs | **22** | 1.14 |
| A2 d1b | 9,616 | **0.928** | 766 µs | 93 | **794** | 654 | — | 1,560 µs | (1) | — |

**Engagement: exact, and the finding is closed as stated.** The serialized
server's utilization fell from 1.00 / 0.93 to **0.125 / 0.140**; its
service time from 725 / 766 µs to **112 / 101 µs** = the leaf-lock window
(the device wait left the pass); `tx_queue_wait` **−55 %** (696 / 794 →
325 / 285 µs); 22–24 windows in flight at the peak (mean in flight ≈ Σ
`window_total` ÷ wall ≈ 1.6); journal entries per served publish unchanged
(0.261 / 0.261 → 0.259 / 0.259 — the Lever-B aggregation factor; one entry
per KvTx by construction); every tripwire flat (`meta_conveyor_pass_panics`
= `detached_task_panics` = `invariant_tripwires` = `uring_queue_full` =
`meta_kv_journal_full_stalls` = 0; ledger closure `served ≡ shipped` on
every leg; `refusals` = `owner_panics` = 0). `leaf_lock_hold` mean 94 / 82
µs over 11 k holds; its tail (95 / 96 samples ≥ 1 ms, one at ≤ 64 ms) is
ascending-acquire contention with the checkpoint freeze plus preemption on
a box at load 12–16 — no device call sits inside the hold by construction.

**Aggregate ingest: NOT up — PAR at best, brackets disagree.** A1 3.58, B1
**2.41**, B2 3.03, A2 3.02 GiB/s: the reversed bracket (B2 / A2) is par
(+0.3 %), the first (B1 / A1) −33 % with B1 started at the session's
highest load (11.4). Per the A-B-B-A rule a single-order delta the reversed
bracket does not reproduce is not attributable; the row's honest reading is
**par with one degraded leg**, and the same-binary spread (A1 3.58 vs A2
3.02 = 18 %) says this session's noise band is wider than D-1b's (±2 %).

**Why the freed conveyor did not convert on this venue — the next wall, by
number.** The per-tx commit latency (queue + window) is par-to-worse
(1,421 / 1,560 → 1,750 / 1,403 µs) because the journal write's completion
round trip itself rose, 618 / 654 → **1,306 / 1,011 µs** per window, and
the lane waits on it in order:

* `journal_ring_write` is not device time. Its MODE is 50–200 µs on every
  leg (A1: 2,291 samples ≤ 64 µs, 1,635 ≤ 128, 1,318 ≤ 256); the mean is a
  ms-class TAIL (A1: 622 ≤ 2 ms, 384 ≤ 4 ms, 259 ≤ 8 ms, 62 ≤ 16 ms; B1: 708
  ≤ 4 ms, 562 ≤ 8 ms, 317 ≤ 16 ms, 103 ≤ 32 ms, 12 ≤ 64 ms). A ~1 KiB
  page-cache write completes in µs; the tail is the `uring_fs` round trip —
  queue hop → worker submit → CQE → oneshot wake onto one of the two
  `sqz-meta` lane threads, which also host the pass task, the checkpoint
  task and every `spawn_meta_join` owner call (7 k/s here) — on a box with
  200+ runnable threads (192 dd + 9 daemons on 32 cores). That is DLM #3's
  wake-hop class (C-2), now isolated as THE term with its own per-window
  instrument.
* The in-order lane head-of-line-blocks behind that tail: `window_lane_wait`
  is bimodal — ~7,000 windows picked up in ≤ 16 µs (lane idle), ~1,900
  waited ≥ 1 ms and 588 waited ≥ 8 ms behind a head whose write was in the
  tail. In-order is the journal-order law (chain reachability), so the HOL
  is inherent; pre-D-2 the same slow write blocked the apply too.
* With the pass no longer paced by the device, batches got SMALLER —
  size-1 groups 78 % → 86 % of passes, no ≤ 32 groups on B — so passes /
  submissions / completion hops rose +18 % (9,252 / 9,616 → 11,180 /
  11,035) for the same entries. The audit's "`meta_commit_group_size` up"
  expectation assumed arrivals would keep accumulating during a device
  wait the lever removes; on a hop-bound venue the lost implicit batching
  offsets the freed serialization.
* Co-writer side, consistently: `publish_phase_ns.total` per save 21.4 /
  25.5 → 30.4 / 24.9 ms, its `queue_wait` 6.5 / 7.4 → 8.7 / 7.4, the frame
  RTT 3.41 / 3.52 → 4.28 / 3.52 ms, `write_pipeline_phase_ns.publish` 16.8 /
  19.3 → 25.6 / 21.6 ms — B1 worse, B2 par. Authority CPU 5.57 / 5.94 →
  6.17 / 5.79 s (par; `sqz-meta` 2.83 / 2.98 → 3.09 / 2.92).

**Verdict.** The mechanism is correct and engaged exactly (the conveyor is
no longer a serialized server; ρ 1.0 → 0.13, queue wait −55 %, crash
contracts green), and the in-process rows show the ~2× the closed loop
permits when the device term is real latency. On the co-located fleet the
binding term was never the device — it is the completion HOP chain under
CPU saturation, which this lever exposes rather than moves; aggregate
ingest is par, one leg degraded under load. D-2 therefore closes board #2's
MECHANISM and hands #3 (C-2: the resident pass/lane on `sqz_notify`, the
fan-out on the committer's lane, the `uring_fs` completion hop) the number
it needs: ~1.0–1.3 ms mean / 50–200 µs mode per journal write completion,
with a 5 % tail ≥ 4 ms, on the path from a page-cache write to the lane.

## Laws the design did not anticipate (reasoned here, written into the module doc)

1. **Stage B is a leaf-lock taker in one arm.** The lever statement said
   "stage B never touches node locks"; the §4.4 pt 4 rollback of a FAILED
   write needs the leaves to remove the window's records. It is the
   pre-conveyor committer rollback verbatim, under the 4b discipline, and
   the acyclicity argument absorbs it (module doc). The alternative — a
   rollback job re-queued onto stage A — would have added a queue-item
   class for a device-error path.
2. **A stage that holds open reservations must never wait on a mutex the
   reservation-completer needs.** The checkpoint task's FINAL cycle waited
   for `completed_upto == head` while holding the SMO mutex; stage B's hole
   checkpoints (`checkpoint_past`) take that mutex. With the serialized
   pass this was unreachable (one reservation, completed before any
   `checkpoint_past`); with a lane whose current group can checkpoint while
   later windows of its own still hold open reservations, it is a
   deadlock. Fix: the drain runs BEFORE the mutex (`checkpoint.rs`).
3. **The in-order-ack observable is racy at the receivers.** A group's
   fan-out wakes N committer tasks; the runtime schedules them in any
   order, so "committer i acked i-th" is not a contract a test can hold
   (the red test's first form failed on the fix for exactly this reason).
   The law was restated as what it protects: no later window is answered
   while an earlier window's entry is unlanded — pinned with a parked
   predecessor write.
4. **`completed_upto` is lane-gated.** A landed write whose window the lane
   has not reached keeps an open reservation, so `completed_upto == head`
   no longer means "every submitted write landed" (on the strict cadence it
   stalls behind a parked barrier). Correct for the tail rule and for
   shutdown's drain; a test that read it as the write-landed observable
   had to change (the strict-barrier pin now reads the in-flight gauge).
5. **Group commit of barriers falls out.** The lane's grouping (head
   awaited + landed successors) means the strict cadence pays one barrier
   per group without a timer — the §5.5 batching policy's "no timers, the
   batch is whatever is queued" applied downstream.

## Owed

1. `task check` (the full gate) — deferred by instruction.
2. **Loom on the two-stage handoff**: the handoff reuses `ConveyorCore`
   verbatim (a second instance; `with_gauge(None)` is the only change and
   the core compiles under `--cfg loom`), so the existing four conveyor
   models cover its protocol — but `loom-models/` does not currently build
   at the D-1b tip (`Grant.required_segments` / `plan_range` 4-arg drift in
   the grant-table models, foreign to this campaign), so the models were
   not RE-RUN here. Fix the drift, run `tests/run_loom.sh`.
3. DLM #3 (C-2: the resident pass task on `sqz_notify`, fan-out on the
   committer's lane) — the wake-hop residue is now the dominant term in
   `journal_ring_write` (a ~860 B page-cache write reads 650 µs on the
   loaded box: that is the hop, not the device) and in the lane's fan-out.
4. The fabric-venue fleet rows (squeeze-test / AWS) — D-1b's owed #1,
   unchanged.
