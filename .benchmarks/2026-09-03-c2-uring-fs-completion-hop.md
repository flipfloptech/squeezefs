# 2026-09-03 — C-2: the journal ring write's completion hop (DLM #3)

**Branch** `perf/c2-uring-fs-completion-hop` (worktree off dev `96156b15`).
Measure `fa27b21a` → RED `cc3d8e8d` → fix `f42a0994` → D-1 pin re-graded
`88fa1444` → CPU class `e4720cf1` → rigs `674714fc` → field rows (below).
Campaign: `docs/design-e2e-perf-audit.md` §3 board **#3** ("wake-hop
inflation on the conveyor — THE binding term post-D-2"), Appendix D DLM #3;
baseline `.benchmarks/2026-09-03-d2-two-stage-conveyor.md` (its fleet row
and its handed-down number: `journal_ring_write` 1.0–1.3 ms MEAN / 50–200 µs
MODE per window, 5 % ≥ 4 ms). Contracts `tests/conveyor_completion_hop_tests.rs`.

## The conviction (measured first)

D-2 left `journal_ring_write` (submission → observed completion) as the
one term the freed conveyor still waited on, and named its nature without
an instrument that could split it: a ~1 KiB page-cache write reading a
ms-class mean over a 50–200 µs mode is not a device, it is a chain of
thread hops. **The instrument** (`fa27b21a`): `uring_fs_write_phase_ns`,
always-on, exact-sum, zero-alloc — the request's own oneshot payload
(`WriteOutcome`) carries the two worker-side instants, one clock read per
reap pass stamps every outcome it delivers, and the awaiting task's first
observation closes the spans:

| phase | span | what it is |
|---|---|---|
| `queue_hop` | submit (the caller's queue push) → the pool worker admitted it | the caller→worker wake + queue residence |
| `device` | admitted → the worker reaped the last CQE and sent the outcome | the kernel write — an **io-wq punt** for a buffered block-device write (`blkdev_write_iter` refuses `IOCB_NOWAIT` buffered writes; probed on loop / btrfs / tmpfs: no buffered `IORING_OP_WRITE` completes inline at `submit()`) — plus the worker's own wake out of its ring wait |
| `wake_hop` | outcome sent → the durability task observed it | the oneshot's waker → the task's lane queue → that lane thread's dispatch |
| `total` | submit → observed | ≡ `journal_ring_write` |

Op-trace stages `ufs_submit` / `ufs_admit` / `ufs_wake` / `ufs_observed`
ride the window's traced members.

**The harness** (`tests/conveyor_completion_hop_tests.rs`): the D-2 sandbox
at D = 0 (the field's page-cache write, no latency arm), 16 committers × 32
creates, under (i) a serve-shaped hog on the `sqz-meta` lanes (the owner's
~7 k/s `spawn_meta_join` verbs made a controlled load: N tasks × a CPU
burst + yield), (ii) whole-box saturation (2 × cpus spinning threads —
the fleet's 200 runnable threads on 32 cores), (iii) both. Shipped chain,
release, 32-CPU dev box (load 7–20 — another agent's substrate present):

| row | tx/s | `journal_ring_write` mean / mode | `queue_hop` | `device` | `wake_hop` | `window_lane_wait` |
|---|---|---|---|---|---|---|
| quiet | 74,588 | 52 µs / 64 | 4.6 | 21.5 | **25.5** | 38.6 |
| lane hog (3 × 100 µs) | 41,972 | 191 / 256 | 5.6 | 36.3 | **148.5** | 183.4 |
| box hog (64 spinners) | 10,496 | 478 / 128 (3.7×) | 175.9 | 66.5 | **234.2** | 361.8 |
| lane + box hog | 13,327 | 718 / 256 (2.8×) | 227.7 | 353.3 | 136.5 | 300.3 |
| **4 × 2 ms serve bursts** (the structural row) | 2,450 | 3,187 / 4,096 | 8.5 | 275.5 | **2,902.3** | 2,942.0 |

Debug rows (same shape) read the same story with the clock reads visible:
quiet 116 µs = queue 11 + device 38 + **wake_hop 66**; lane + box 1,206 =
136 + 257 + **930**; a second quiet roll 203 µs / mode 64 (3.2×) with
wake_hop 164 of the 203 — the audit's "mean ≫ mode", quiet, is the hop.

The `wake_hop` is the term: the completion's waker enqueues the durability
task on a lane it shares with the serve plane, the checkpoint task and
every other metadata-plane loop, and under load waits behind whatever is
queued ahead. The structural row makes it unmistakable: behind 2 ms serve
bursts the hop is 2.9 ms, the in-order lane's next-window pickup 2.9 ms,
and the apply pass's own dispatch (`tx_queue_wait`) 3.0 ms — all three
conveyor deliveries HOL-blocked by unrelated work. The `sqz-meta` hog alone
(3 × 100 µs) does NOT inflate the quiet mean much because it TRADES the
parked lane's futex wake (~60 µs on this box) for a burst wait — the hop
costs either way; what the serve load does is halve the pass rate
(passes 72 → 35) and double the group size (7 → 14), the implicit batching
D-2 noted.

## The contract (RED `cc3d8e8d`)

`completion_delivery_is_isolated_from_the_serve_lanes`: four 2 ms hog
bursts saturate both `sqz-meta` lanes; 16 × 32 creates at D = 0; laws —
`wake_hop` mean < burst/4, `window_lane_wait` mean < burst/4,
`tx_queue_wait` mean < burst/2, plus every row's validity (one tx = one
entry, gauge closes, the three spans sum EXACTLY to the observed round
trip, `total` ≡ `journal_ring_write`). RED on the shipped chain: 2,866 /
2,970 / 3,364 µs against 2,000 µs bursts (debug); 2,902 / 2,942 / 3,015
(release). The audit's "mean tracks mode within 2×" is printed on every
row; with octave buckets it is not a pinnable statistic (a mean inside the
mode bucket reads < 1×, the shipped structural row reads 0.8×), so the
burst-relative laws above are the pin.

## The fix (`f42a0994`) — the per-volume journal lane

Every writable volume gets its **own lane** (`sqz-jrnl{N}`; derived — one
per volume that commits; a reader / co-writer / peer-owned volume never
commits and never gets one), which runs BOTH conveyor stages and OWNS the
volume's journal io_uring, parking IN it:

* **`sqz_exec::LaneExec` pluggable park** (`LanePark`: park / unpark /
  service; `CondvarPark` is the default every existing lane keeps). The
  lost-wake argument moved onto the queue mutex: the lane marks itself
  parked in the lock section that found the queue empty, every enqueue
  reads the mark under the lock after its push, and parkers are STICKY
  (an unpark before the park returns it at once — the eventfd counter's
  law). Unit-pinned: unparked only while parked, sticky + tick, a
  2,000-spawn foreign storm completing in ≪ one TICK.
* **`uring_fs::OwnedRing` / `OwnedReactor`**: the pool's reactor (same
  slot table, continuation law, fault shim, `WriteCompletion` handle)
  driven by the thread that awaits it. `submit_write_at_batch` = admit +
  `submit()` (no queue hop — `queue_hop` reads 0 by construction);
  `park` = `io_uring_enter` with the wake-eventfd `READ` SQE armed (a task
  wake and a device CQE are ONE wait; the TICK rides `IORING_ENTER_EXT_ARG`,
  pre-5.11 degrades to an untimed wait the eventfd keeps lossless);
  `service` = reap on the owning thread — the completion's oneshot fires
  on the very thread that polls the durability task next. The slow-device
  seam rides the ring as a linked `TIMEOUT(ETIME_SUCCESS)` chain, so a
  REAL late CQE exercises the lane's park path in every D-2 contract; the
  stall / torn / sector / power-cut arms apply verbatim (a held write
  resumes through the pool onto the same completion).
* **`kv::journal_lane::JournalLane`**: the thread + its `RingPark` + the
  thread-bound reactor in a thread-local; a submission is ring-native only
  from the lane thread (the apply pass, by construction) — off-lane or
  ringless it takes the pool. Spawned on the volume's first commit, dropped
  with the backend (signal, never joined from a drop). A box with no
  io_uring runs a ringless lane on the pool, counted + logged.
* **Lever + gauges**: `SQUEEZEFS_JOURNAL_LANE` (default on; `0` = the
  shipped D-2 shape — the same-binary A/B control);
  `journal_lanes_spawned` / `journal_lanes_ringless`;
  `journal_ring_lane_writes` (engagement ≈ conveyor passes) /
  `journal_ring_pool_writes`; `daemon_cpu_ns_by_class.sqz-jrnl`.

What is unchanged: everything D-2 pinned — one tx = one checksummed entry
in the same contiguous reservation, acks in journal order behind a parked
predecessor, the barrier gate, the fail-stop lattice from stage B, the
rollback-only leaf take, lane-gated `completed_upto` — plus the `sqz-meta`
pool for every other metadata-plane loop. `tests/run_loom.sh`: all 100
models green including `conveyor_two_stage_models` (the handoff protocol is
the same `ConveyorCore`; the lane changes WHERE the tasks run, not the
word protocol).

**Lever (c) — batch shaping — NOT landed**: the ledger does not convict it.
The lane changed the field's group-size mix by one point (size-1 groups
84 % → 87–89 %, passes par), and the conveyor's commit latency is now ≈ 5 %
of the co-writer's per-block publish (below) — a coalescing window would
buy fewer io-wq wakes at the price of a timer on the lane for a term that
no longer binds the row.

## Measured — in-process (release, 32-CPU dev box, same binary, lever on vs off)

| row | tx/s on / off | `journal_ring_write` on / off | `wake_hop` on / off | `tx_queue_wait` on / off |
|---|---|---|---|---|
| quiet | **86,158** / 74,588 | 53 / 52 µs | **5.1** / 25.5 | 49 / 46 |
| lane hog | **51,460** / 41,972 | **48** / 191 | **1.8** / 148.5 | 87 / 45 |
| box hog | **16,429** / 10,496 | **194** / 478 | **4.4** / 234 | 216 / 342 |
| lane + box hog | 13,459 / 13,327 | **274** / 718 | **37** / 136 | 389 / 176 |
| 4 × 2 ms serve bursts | **35,681** / 2,450 | **63** / 3,187 | **4.0** / 2,902 | **156** / 3,015 |

The contract is GREEN with the lever on and RED with it off (same binary).
`queue_hop` is 0 on every lane row; the `device` span (submit → the lane
reaped the CQE) is the whole round trip and reads 47 µs quiet vs the
pool's 21.5 — the lane reaps between its own polls, so a CQE landing
mid-pass is observed when the pass ends; `total` is par quiet and the
row's throughput is up 15 %. D-2's own release rows on the fix: deferred
D=0 **88,297** tx/s (D-2: 50,532), D=500 µs **26,850** (21,360), D=2 ms
**7,472** (6,977), strict **6,782** (6,312); every D-2 contract green,
kill-9 × 5 rounds replay-exact.

## Field row — the D-1b/D-2 fleet rig, A-B-B-A (measured-simulated tier)

`.benchmarks/rigs/2026-09-03-c2-fleet-abba.sh` (= D-1b's rig verbatim +
the C-2 analysis `2026-09-03-c2-fleet-analyze.py`) on the 32-CPU dev box
(117 GiB), tcp devsub (nvmet-tcp on `127.0.0.1`, zram OSS 2 × 64 GiB),
`tests/mw_fleet.sh create N=1 --multi-writer --cowriters=8` per leg, torn
down to zero residue between legs. Row: **8 co-writers × 24 concurrent
`dd bs=1M count=128 conv=fsync` streams from `/dev/zero`**, per-member
directories. **A** = dev tip `96156b15` (D-2's shape), **B** = `e4720cf1`
(this branch), both `profile release`. Loadavg at leg start 4.1 / 4.1 /
5.3 / 10.3 (D-2's session: 8–11; one foreign idle mount present, no
foreign I/O). Artifacts `target/c2-fleet-abba/` in this worktree.

**The authority's conveyor:**

| leg | ingest | passes | ρ(apply) | `pass_total` | `tx_queue_wait` | `journal_ring_write` mean / mode | ≥ 4 ms | `window_lane_wait` (≥ 2 ms) | commit latency (queue + window) | `sqz-meta` / `sqz-jrnl` CPU |
|---|---|---|---|---|---|---|---|---|---|---|
| A1 d2 | 3.07 GiB/s | 10,554 | 0.147 | 109 µs | 322 | 1,405 / 64 (22×) | 17.9 % | 907 (16.6 %) | 1,846 µs | 2.92 / — |
| B1 c2 | 3.00 | 10,947 | 0.217 | 159 | 518 | **671** / 32 (21×) | **8.1 %** | **343** (6.1 %) | **1,340** | 2.29 / 1.07 |
| B2 c2 | 2.89 | 11,085 | 0.203 | 152 | 526 | **745** / 32 (23×) | **8.5 %** | **386** (6.1 %) | **1,422** | 2.27 / 1.03 |
| A2 d2 | 2.98 | 11,149 | 0.144 | 104 | 296 | 1,125 / 64 (17.6×) | 15.6 % | 666 (14.3 %) | 1,535 | 2.98 / — |

B's hop split: `queue_hop` **0** / `device` 561 / 655 / `wake_hop` 107 / 88
µs (n = passes). Engagement exact: `journal_ring_lane_writes` = passes,
`journal_ring_pool_writes` = 0 on both B legs; every leg VALID (ledger
closure `served ≡ shipped`, `refusals` = `owner_panics` = 0, entries per
publish 0.258 / 0.263 → 0.264 / 0.263 — unchanged by construction);
authority CPU par (5.92 / 5.97 → 6.03 / 5.96 s) with the conveyor's CPU
moved off `sqz-meta` (−0.63 / −0.71 s) onto `sqz-jrnl` (1.03–1.07 s).

**Verdict per the acceptance list:**

* `journal_ring_write` mean → mode: **halved, not closed** — 1,405 / 1,125
  → 671 / 745 µs (−52 % / −34 %), the ≥ 4 ms tail 17.9 / 15.6 % → 8.1 /
  8.5 %, both brackets agree. The mode is now 32 µs; the mean stays ≈ 21×
  it because the residue is `device` (561–655 µs): the **io-wq punt** a
  buffered block-device write takes (a kernel thread wake, then the lane's
  ring-wait wake) under a box at load 85 — the two kernel wakes the
  userspace fix cannot remove — plus the lane's own busy time (a CQE
  landing mid-pass is reaped when the pass ends; `wake_hop` 88–107 µs on
  the fleet IS that serialization: the durability task polls after the
  pass ahead of it in the lane's queue).
* `window_lane_wait` (the in-order lane's HOL): **907 / 666 → 343 / 386 µs,
  ≥ 2 ms tail 16.6 / 14.3 % → 6.1 / 6.1 %.**
* `tx_queue_wait`: **up in three brackets of four** — 322 / 296 → 518 /
  526 µs here (+61 % / +78 %), 342 → 251 (−27 %) and 312 → 541 (+73 %) in
  the lever bracket below — tracking `pass_total` (104–109 → 97–159). Two
  costs of one thread: the pass and the durability group serialize (a pass
  queued behind a group's fan-out — N cross-thread oneshot sends — waits
  for it), and a dedicated thread that parks between bursts pays a
  scheduler dispatch per burst on a saturated box where the shared
  `sqz-meta` lanes were rarely parked. Commit latency (queue + window) is
  **down in every bracket** (−27 % / −7 % here, −49 % / −12 % below) because
  the window half fell further than the queue half rose.
* entries/tx ≥ today's: **unchanged** (0.264 / 0.263 vs 0.258 / 0.263; one
  tx = one entry by construction; the Lever-B aggregation is the layout
  conveyor's, untouched).
* **Aggregate co-writer ingest: NOT up — par-to-slightly-negative.** A1
  3.07, B1 3.00, B2 2.89, A2 2.98 GiB/s: −2.3 % / −3.0 %, inside the D-1b
  session's ±2 % band by a hair on one bracket and outside it on the other,
  consistent in sign. The same-binary lever bracket (below) is the
  attribution.

**The same-binary LEVER bracket** (`.benchmarks/rigs/2026-09-03-c2-fleet-
lever-abba.sh`: the C-2 binary with `SQUEEZEFS_JOURNAL_LANE=0` vs `=1`,
A-B-B-A over the lever — build noise removed, and the instrument present
on BOTH postures; loadavg at leg start 0.7 / 3.2 / 3.2 / 3.9, the quietest
legs of the session; `target/c2-fleet-lever-abba/`):

| leg | lane | ingest | passes | ρ(apply) | `pass_total` | `tx_queue_wait` | `journal_ring_write` mean / mode | ≥ 4 ms | `queue_hop` | `device` | **`wake_hop`** | `window_lane_wait` | commit latency |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| L0a | off | 3.18 GiB/s | 10,765 | 0.153 | 107 | 342 | 1,209 / 64 (18.9×) | 14.9 % | 79 | 383 | **746** | 765 | 1,672 |
| L1a | on | 3.23 | 11,862 | 0.154 | 97 | **251** | **518** / 32 (16.2×) | **6.1 %** | 0 | 464 | **53** | **263** | **860** |
| L1b | on | 2.91 | 10,977 | 0.196 | 147 | 541 | **682** / 32 (21.3×) | **8.2 %** | 0 | 596 | **85** | **371** | **1,359** |
| L0b | off | 3.06 | 10,905 | 0.139 | 100 | 312 | 1,130 / 64 (17.6×) | 14.9 % | 81 | 381 | **664** | 667 | 1,539 |

This is the finding measured on the fleet itself: on the shipped shape the
**`wake_hop` is 746 / 664 µs of the 1,209 / 1,130 µs round trip — 62 % /
59 %** — the oneshot's wake onto the shared `sqz-meta` lane, with the
caller→worker `queue_hop` another 79 / 81 and the io-wq `device` 383 / 381.
The lane takes `wake_hop` to 53 / 85 (the lane's own poll order),
`queue_hop` to 0, and leaves `device` at 464 / 596 (the punt plus the
lane's reap deferral). Ingest: +1.6 % / −4.9 % — the brackets disagree in
sign, the same-posture spread is 11 % (L1a vs L1b) — **par**, as in the
two-binary bracket. `tx_queue_wait` is down 27 % in one bracket and up 73 %
in the other, tracking `pass_total` (97 vs 147 µs on the two lane legs):
the lane's own service time is what the queue wait follows, and it varies
with the box.

**Why ingest does not move, by number.** The conveyor is no longer where
this row's time is. On the co-writer, one per-block publish
(`publish_phase_ns.total` 25.2 / 26.0 → 25.7 / 29.7 ms) is: its own layout
conveyor's `queue_wait` 7.4 / 7.4 → 7.4 / 8.9 ms, `save_encode` 7.1 / 8.1 →
7.6 / 9.1 ms (a ~50 µs encode reading 7–9 ms — the co-writer's `sqz-meta`
lanes starving under 192 `dd`), and `meta_commit` 9.6 / 10.0 → 9.5 / 10.7
ms, which is the ship lane's `queue_wait` 3.2 / 3.4 → 3.5 / 4.0 ms + the
frame's `rtt` 4.2 / 4.0 → 4.3 / 4.6 ms; on the authority, the owner's
per-verb `meta_ship_owner_phase_ns.dispatch` is **2.2 / 2.0 → 2.2 / 2.3 ms**
(the `spawn_meta_join` hop onto the shared `sqz-meta` lanes — the SAME
class C-2 just removed from the journal path, now the largest term in the
owner's 14 ms per-frame `total`) and `execute` 0.9 / 0.8 → 0.8 / 0.8 ms.
The conveyor's commit latency (1.3–1.8 ms) is ≈ 5 % of the publish and
≈ 2.5 % of the co-writer's 51–65 ms whole-block pipeline residence
(`write_pipeline_phase_ns.total`); halving it cannot show in GiB/s on a
row whose wall is CPU starvation of 200 threads on 32 cores. D-2 took the
conveyor from ρ 1.0 to 0.13 and the row did not move; C-2 took its commit
latency down another 27 % and the row did not move — the conveyor was the
binding term of the AUTHORITY, not of the ROW, since D-2 landed.

**Verdict.** The mechanism is correct and engaged exactly (lane writes ≡
passes on every lane leg; every D-2 contract and all 100 loom models
green). The finding is confirmed on the fleet by the lever bracket — the
shared-lane wake hop WAS 59–62 % of the journal write's round trip — and
removed: `wake_hop` 660–750 → 53–107 µs, `journal_ring_write` −34…−57 %,
its ≥ 4 ms tail halved, the in-order lane's pickup −50…−66 %, commit
latency −7…−49 %, all four brackets agreeing in sign. Aggregate ingest is
PAR (−3.0 / −2.3 / +1.6 / −4.9 %, inside the same-posture spread), because
the conveyor stopped being the row's binding term when D-2 landed (§ above,
by number). Board #3's mechanism closes; the number it hands on is the
owner's `dispatch` hop (DLM #7, 2.0–2.3 ms per verb — the same class, one
plane over) and the co-writer's CPU-starved publish pipeline.

## Laws the design did not anticipate (written into the code)

1. **A dedicated thread reaps between its own polls.** With both stages
   on one thread, a CQE that lands during a pass is observed when the pass
   ends; `device` on the lane therefore carries the pass's remaining
   service time and `journal_ring_write` reads par quiet where the pool
   reaped concurrently. The alternative — a second thread — is the hop the
   campaign removed. The single-committer chain (no batching) reads 8 µs
   end to end (6.2 device + 1.4 wake_hop) against the pool's 19 µs.
2. **The D-1 pass ceiling was a venue constant.** `a_frame_of_independent_
   verbs_commits_in_few_conveyor_passes` pinned ≤ 4 passes per 64-verb
   frame against the pool's implicit batching (the pass waited behind the
   serves for a lane, then drained 2–3 batches); the responsive lane
   drains the frame's arrival spread as it lands (4–8 passes, 10 release
   rolls: 4,4,4,5,5,5,6,7,7,7). The pin now reads `FRAME / 4` — ≥ 4 verbs
   co-queued per pass, an order of magnitude under the serial loop — the
   same meaning without one venue's dispatch timing (`88fa1444`).
3. **`blkdev_write_iter` refuses NOWAIT buffered writes.** Every buffered
   `IORING_OP_WRITE` to a block device (and, measured, to btrfs and tmpfs
   files) punts to io-wq — there is no inline completion to reap at
   `submit()` (a 4,000-write probe: 0–1 inline on every venue). The io-wq
   worker's wake is therefore a per-write kernel thread wake no ring
   ownership removes; it is the `device` residue above.

## Owed

1. `task check` (the full gate) — deferred by instruction (batched).
2. **The io-wq residue** (`device` 561–655 µs mean, mode 32 µs, 8 % ≥ 4 ms
   on the saturated fleet): the remaining journal-write term is two
   kernel wakes per window. Candidates, each a measured A/B, none landed:
   (a) fewer windows — a write-coalescing arm downstream of the apply (a
   window applied while a write is in flight submits with the next
   completion; bounded by one write round trip, no timer) — lever (c)'s
   honest form; (b) `IORING_REGISTER_IOWQ_AFF` pinning the lane's io-wq
   workers beside it; (c) an O_DIRECT whole-page journal write (a RAM
   mirror of the active ring page; the completion becomes the device's
   irq, no kernel thread) — a design of its own.
3. **DLM #7 is now the owner's largest term**: `meta_ship_owner_phase_ns.
   dispatch` 2.0–2.3 ms per verb on the fleet — the `spawn_meta_join` hop
   from the wire thread onto the shared `sqz-meta` lanes, the class this
   campaign removed from the journal path. "Execute on the accepting lane"
   is the ledger's lever; with the conveyor at ρ 0.2 and commit latency
   1.4 ms, the publish RTT (4.3 ms) is half dispatch.
4. **The co-writer side owns the row**: `save_encode` 7–9 ms and the layout
   conveyor's `queue_wait` 7–9 ms per save under CPU starvation — the
   co-writer's `sqz-meta` lanes, not the authority's. Whether the
   per-volume-lane pattern (a dedicated thread for the layout conveyor's
   pass) helps a CPU-bound co-writer is a question for a venue with CPU
   headroom (the fabric fleets — D-1b's owed #1, unchanged).
5. `tx_queue_wait` +60–80 % on the fleet: the pass's dispatch on a parked
   dedicated thread under saturation. A spin-before-park on the journal
   lane (the `SQUEEZEFS_IPC_SPIN_US` precedent — a fleet lever, never an
   ambient default) would trade a core for it; unmeasured.
6. **Finding 49 — FIXED (`fix/f49-ring-park-abort`: RED `eea11eba`, fix
   `3d3ad22d`).** Batch gate #3 on dev `4d194c59` (C-2 + R-4 on D-2):
   `cargo test --all-features --test kv_scale_tests -- --test-threads=1`
   failed 3/3 on `million_entry_directory_storm_create_lookup_readdir_
   rmdir`'s UNLINK storm with `commit aborted while parked for ring
   space`, the volume fail-stopped, and `rightmost_separator_pointer_
   record_replays_clean` inherited the poisoned process.
   * **Mechanism: none of (a)/(b)/(c) — a fourth, (d): the checkpoint's
     cadence starved by its own unbounded threshold drain.** On the
     failing run `meta_kv_checkpoints` sat at 1 and `reusable_upto` at
     249 for the whole ~190 s storm while `head` climbed to the 32 MiB
     cap; `min_inflight_start` = MAX (no open reservation held the tail —
     not (c)); `windows_inflight` 0–1 with the `sqz-jrnl0` lane parked
     idle in `io_cqring_wait` (stage B was not behind stage A — not (b));
     the durability lane had nothing to wait on and the checkpoint's
     barrier rides the `uring_fs` process pool, not the journal lane (no
     cycle — not (a); the acyclicity argument is now `checkpoint.rs`'s
     module doc). The checkpoint task's select — `timeout_at(next_tick,
     wake.notified())` — polls the wake BEFORE the sleep; every pass under
     the storm crossed the writeback threshold and re-armed the
     maintenance wake, and `run_maintenance` popped until empty, so the
     cadence arm (the ONLY path to a ring-pressure `checkpoint_cycle`,
     i.e. the only thing that advances `reusable_upto` for a parked
     committer) was never reached. The drain never emptied because single
     items took 1–96 s: gdb on the spinning `sqz-meta0` (R state, one
     CPU) → `bset::MergeIter::run_end` ← `compact` ← `fold_node_sources`
     ← `compact_node` ← `smo_replace` ← `checkpoint_flush_node`, and a
     fold census read 2 views / 35,114 records in → 1,839 out in **44.3 s**
     (the next: 29,654 records, 31 s) — the directory inode's frozen Δtime
     overlay is one same-key run, and `run_end` rescanned it from its
     start for EVERY candidate (O(run²) record decodes). The D1.b
     escalation then did what it is specified to do at 3 × 30 s of
     continuous park. Latent before C-2 (the cadence contract is RED with
     `SQUEEZEFS_JOURNAL_LANE=0` too — 0 cycles over a 5.0 s storm); C-2's
     faster commit rate plus the all-features dhat allocator's global
     lock made it deterministic under the gate (default-features and the
     lane-off shape ran the storm 4× faster and outran the threshold).
   * **Fix at the law level (three parts):** the cadence deadline is read
     off the CLOCK after every wake (a wake returning past the deadline IS
     the deadline); every threshold drain is bounded by one cadence period
     (`KvTree::run_maintenance_until`, ≥ 1 item per pass so it always
     progresses, leftovers re-arm the wake; the shutdown tick stays
     unbounded); `MergeIter` memoizes the current run's end per source.
     With all three the storm alone (all-features, lane on) creates in
     9.8 s / unlinks in 9.5 s (was 76 s / 110–190 s + fail-stop); the
     longest maintenance item 547 ms. What the drain bound does NOT bound
     is one item's own service time — the reclaim-latency floor under a
     storm is one SMO + one flush pass, stated in the module doc.
   * **Contracts** (`tests/conveyor_two_stage_tests.rs` §3): `checkpoint_
     cadence_survives_a_sustained_maintenance_storm` (16 committers × 300
     × 4 KiB xattr commits, 32 MiB ring, 15 ms armed device latency; 0
     cycles RED → 3 GREEN, 0 stalls) and `parked_committers_are_released_
     by_the_checkpoint_under_a_sustained_storm` (8 MiB ring, `SQUEEZEFS_
     TIMEOUT=1`, an observer that sees `reusable_upto` advance while
     committers are parked — green before and after, pins the law).
   * **Loom:** the ring-capacity dimension is already model-checked where
     it is a word protocol — the `journal_core` models (admission past
     `reusable_upto` refused, released by `advance_reusable_upto`,
     conservation across transfer). Finding 49 is a scheduling-LIVENESS
     property under a real clock (a select arm never reached), which
     loom's bounded interleaving of atomics cannot express; the cargo
     contract is its pin.
   * **Field exposure: the C-2 brackets were NOT exposed.** Per leg the
     authority wrote 8.15–8.34 MB of journal (7.7–7.9 MB on the busy
     volume) over ~7.5 s against the 32 MiB ring — **≈ 0.24 rings per
     leg** — with 19–20 checkpoint cycles per leg (one every ≈ 0.4 s) and
     `meta_kv_journal_full_stalls` +0 on both volumes in all four legs
     (`target/c2-fleet-abba/*/m0_p{0,1}.json`). A fail-stop would have
     read as an ingest cliff; the rows were par. The trigger needs TWO
     ratios at once: the serialized SMO drain's utilization ≥ 1 (item
     arrivals × item service time — the maintenance queue never empties
     within a cadence period) sustained for ≥ (ring − reserve) ÷ commit
     byte rate — on the fleet ≈ 32 MiB ÷ 1.05 MB/s ≈ 30 s of a never-
     empty queue, against an observed drain that emptied 20× per leg. The
     storm hit both: 100 k same-directory unlinks (the debug-gate
     population; 8 writers, dhat allocator) whose per-entry journal bytes
     filled the ring while its ONE drain item ran 31–44 s — with the
     checkpoint stuck at cycle 1 from the storm's first seconds, the
     ring's whole capacity was the budget, and the 90 s D1.b ladder
     (3 × `SQUEEZEFS_TIMEOUT`) elapsed inside the drain.
