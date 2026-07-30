# 2026-07-29 — Probe-up depth governor + the multi-backend write funnel

Branch `perf/probe-up-governor` (off dev `778d6d0`; started at `b26c293`,
rebased). Two folded findings, one charter: **the governor's depth must
actually reach the devices** — (1) the pure-BDP depth target self-limits
(never discovers headroom), (2) a stage between admission and the wire
froze whole executor lanes at steady-state-rewrite fill (admitted
custody parked while every device starved in lockstep).

Implementation: `ProbeCore` in `src/write_pipeline_core.rs`
(loom-included) + wiring in `src/write_pipeline.rs`; the async ENOSPC
valve in `src/block_allocator.rs` / `src/block_reclaim.rs` /
`src/routing.rs`. Contracts: `tests/write_pipeline_tests.rs` §2b,
`tests/async_block_reclaim_tests.rs` contract 2b; loom
`write_pipeline_probe_epoch_roll_is_single_winner`. Rigs:
`tests/pug_bracket.sh` (dialed venue), `tests/pug_funnel_bracket.sh`
(multi-backend fast venue).

Session note (2026-07-29): the campaign crossed a session restart
after the final valve amendment (`a44b775`, 02:51) — every §5 table
below was re-measured FROM ZERO on verified final-tip binaries
(daemon + shim both stamped `a44b7753fee5`, KD-7 pair) per the
counted-restart discipline; the pre-amend brackets (the three
intermediate valve cuts) survive only as the counted-death ledger in
§2.2.

Session note (2026-07-30): a SECOND session crash landed mid-hunt on
a rare storm-test hang first seen while gating the `965c528` rig fix.
The successor session finished the hunt (§6b — pre-existing tokio-
layer lost wakeup, dev-tip reproduces, filed OQ-5), re-ran the full
gate from zero on `965c528` (§6), and filled the §5.4 clean-box
write_matrix verdicts on both final binaries. The TEMP-DIAG hunt
apparatus was deliberately never committed (branch stash + forensic
packet in `~/squeezefs-evidence/pug-wedge-forensics/`).

## 1. Field finding 1 — the self-limiting BDP governor

4-node 2×200GbE nvme-tcp, 32-CPU client, nullblk targets, binary
`b26c293`, steady 64t × 4 MiB il streaming (counted):

| Config | Result |
|---|---|
| DEFAULT governor | **11.6 GB/s**, per-device aqu-sz ~6.5, w_await ~3.5 ms |
| forced `DEPTH_BLOCKS=32` | higher (between) |
| forced `DEPTH_BLOCKS=64` | **13,739 MiB/s (+18 %)**, avg op latency 18.6 ms |
| raw fio ceiling, QD128 | 16.6 GB/s aggregate; client CPU ~28 % |

The tell: default aqu-sz ≈ the governor's own BDP target (5.8 GB/s ×
3.5 ms ≈ 5 blocks/device). **BDP = measured bandwidth × measured
service time is a self-fulfilling equilibrium** — at saturation
inflight ≡ bw × lat by Little's law, so the target converges to
SUSTAINING the current operating point, never DISCOVERING that the
backend would deliver more at more depth. Standing laws applied: no
constants (the env lever stays measurement-only), and latency is spent
only where it buys throughput.

## 2. Field finding 2 — the write funnel (folded mid-campaign)

Same cluster, 4-wide data plane, forced `DEPTH_BLOCKS=128`, 32t × 4 MiB
il rewrite sustained: **6.8–7.0 GB/s — HALF the 2-wide plane's 13.7**.
Mid-run: `write_pipeline_inflight_blocks = 121` (pipe FULL at its
512 MiB target), `admission_waits` climbing — but iostat showed only
**aqu-sz 2–4.5 per device** (~9–18 requests standing TOTAL), all 4
heads in lockstep. >100 admitted blocks (~450 MiB) parked between
admission and submission. mds idle, lanes symmetric, client CPU cool.

### 2.1 The conviction (reproduced + decomposed on the 4-wide rig)

Reproduced on the multi-backend fast venue (§4): pipe full (112–128
in flight), aggregate aqu ~17, devices at ~4.5 each. Scratch
decomposition histograms (spawn→first-poll / crypto / allocate /
device write / map merge) plus a channel-occupancy probe localized the
parking: the nvme worker channel was EMPTY at send, crypto/allocate
were µs — but at steady-state rewrite near volume fill,
`block_free_reclaim_sync_drains` stormed (**275 engagements / 30 s**)
and every engagement ran `drain_sync()` — 1 ms `thread::sleep` waits
plus synchronous device discard/punch ioctls — **inline on the
allocating task's executor thread**. Detached pipeline uploads run on
the fuse3 tpc lanes (current-thread runtimes): one engagement froze a
whole lane and EVERY handler/upload future queued on it — admitted
custody parked lane-wide, all devices starving together (the field's
lockstep). The monotonic block cursor means every long-lived volume
eventually lives in this regime (allocation = displaced-block reclaim
racing), so sustained rewrite is the trigger, and MORE backends = more
allocators racing = more engagements.

Suspect adjudication (orchestrator list): `max_background_uploads` /
`upload_semaphore` gate only the STAGING writeback worker — pipeline
write-through uploads never ride it; `striped_block_concurrency` gates
RMW reads + teardown flush, not the pipeline; `stripe_write_semaphore`
had **no acquire site at all** (dead code — deleted, no-dead-code law);
the per-device `NvmeBlockDev` UringWorker is wide (SQ 1024, batched
submission, channel measured empty at send). The detach→submit handoff
(`tpc_spawn`) was A/B'd against a multi-thread-runtime spawn: no
delta — the lanes were not slow, they were FROZEN by the valve.

### 2.2 The fix (red-first: contract 2b)

`enospc_valve_never_blocks_executor_threads`: a canary task on a
2-worker runtime must keep ticking through a stalled valve
(deterministic via the new `SQUEEZEFS_TEST_RECLAIM_STALL_MS` seam).
RED: canary ticked 1× during 301 ms — both workers frozen.

Final fix shape (`SpaceValve` is now async; no fixed widths anywhere):
a brim allocation immediately runs `ReclaimQueue::drain_off_thread` —
the reclaim work (batch discard/punch ioctls + the 1 ms in-flight
waits) runs on the **blocking pool** while only the ALLOCATING task
awaits (honest backpressure on exactly the task that needs the space;
executor lanes never freeze), and racing engagements each run their
own pass **concurrently** — width derives from allocation demand,
never a fixed funnel (`take_batch`'s processing reservation makes
concurrent passes sound). The loop terminates: each pass either
allocates, or drains supply someone allocated, or observes nothing
pending and refuses StorageFull honestly. Contract 6 (fenced daemon
never drains → honest StorageFull) and contract 2
(drain-before-refuse; genuine fullness refuses) are unchanged;
`sync_drains` still counts only passes that processed entries
(contract 8) — under the final shape it is the per-allocation
engagement gauge (finer-grained than the old inline count).

**The counted-death ledger** (two intermediate cuts died on the storm
bracket, each restart-from-zero per the counted-restart discipline;
A/B medians of 3, {f128, default} cells):

| Cut | Mechanism | Bracket verdicts | Death |
|---|---|---|---|
| 1 — single-flight (`drain_serial` mutex) | leader drains, racing engagements share passes | 0.91×/0.91×, then 0.91×/0.84× (two brackets) | the old inline valve, for all its lane-freezing, accidentally parallelized reclaim across engaged threads; the mutex serialized it |
| 2 — supply-wait first (park on `finish_free` notify, 2 ms tick ×8, then escalate) | consume supply as the background reclaimer lands it | 1.04×/1.04× (one favorable bracket), then 0.88×/0.87× on the engagement-checked re-run | up to ~16 ms of park latency per brim allocation is pipeline LIFETIME in a depth-limited regime; the favorable bracket did not reproduce |
| 3 — concurrent immediate off-thread (FINAL) | every engagement drains for itself, off-thread | §5.1 (0.97×/0.98× parity band, both orders) | — |

## 3. The probe-up control policy (finding 1's fix)

A dimensionless Q6 multiplier (`ProbeCore`) scales the governed BDP
sum; BDP arithmetic untouched; rolled per 500 ms epoch by whichever
completion thread wins the epoch CAS (the `Lane` window shape):

* **Saturation signal**: admission parked this epoch OR pipe at/above
  target. No saturation ⇒ extra depth serves nothing.
* **HOLD, unsaturated** — decay ⅛ toward 1.0 (*the latency guard*:
  qd1/low-offered-load never inherits streaming depth).
* **HOLD, saturated + headroom (below R5 cap, not Red) + cooled** —
  LAUNCH: baseline := this epoch's delivery rate; mul += ¼.
* **PROBING, delivery ≥ baseline + 1/16** — ADOPT, re-arm immediately
  (discovery compounds; floor → 32× bound in ~15 epochs ≈ 8 s).
* **PROBING, dead gain** — RETREAT to pre-probe mul + 8-epoch
  cool-down (dead-gain latency tax duty-cycled to ~1/9 — BBR shape).
* **HOLD re-validation** (senior to launching) — adopted depth whose
  delivery collapses below ⅞ of the adopted rate steps back ×0.8.

Seniority unchanged: R5 cap bounds every probe, Red clamps to floor,
pinned override bypasses verbatim, `AdmissionCore` untouched. Loom:
epoch roll single-winner, weakening-verified BOTH directions (CAS →
check-then-store = red / double-launch; AcqRel → Relaxed = green — the
property rides atomicity, no hidden ordering claim). Gauges:
`write_pipeline_depth_target_base` (current-vs-base = live probe
contribution), `write_pipeline_depth_probe_{ups,backoffs}`.

## 4. Substrates + instruments (two-substrate rule; every row nvmet-tcp)

`pug` tcp devsub instance (meta 4 × 1 GiB null_blk nvme17–20n1; zram
data nvme21–24n1, :54133). Two purpose-built data venues:

* **Dialed venue** (the depth-term rig, 2026-07-27 recipe): configfs
  null_blk `sqzpuglat0`, 24 GiB, memory_backed, `completion_nsec=20ms`,
  `irqmode=2`, `max_sectors=8192`, nvmet-tcp :54141. Verified: fio 4M
  qd1 = 187 MiB/s / clat 21.3 ms; qd32 = 2854 MiB/s (campaign:
  186/3040).
* **Multi-backend fast venue** (the funnel rig — fast service, MANY
  backends, the shape the dialed rig structurally cannot see): 4 ×
  6 GiB memory-backed null_blk, `completion_nsec=1.5ms`, discard on,
  nvmet-tcp :54142 (nvme25–28n1). Raw: 4M qd1 = 1642 MiB/s/ns; 4-ns
  4×qd16 = 12.1–17.8 GiB/s (size-dependent). 16 GiB fileset ≈ ⅔ fill
  ⇒ steady-state rewrite lives in the displacement/reclaim regime.
  Re-verified at close (final-binary session): dial 4M qd1 =
  188 MiB/s / clat 21.3 ms; fast-ns 4M qd1 = 1558 MiB/s; raw ceiling
  4-ns 4×qd16×4M = **11.8 GiB/s** (512k: 10.6 GiB/s).

Instruments stated: elbencho 3.1-10 (dynamic, sync driver, --direct;
sustained rows `--infloop --timelimit 60` with diskstats thirds), fio
psync/libaio/io_uring for raw rows, per-run `/proc/diskstats` deltas,
stats-inode deltas. Binaries: A = branch tip, B = dev-tip `b26c293`
(binary-identical to `778d6d0` — the two commits landed since are
test/docs-only). il rows engagement-checked (`ipc_ops_write` Δ).
Contention label: the box hosts the main tree's fstests release gate
(house-standard busy posture); bracket windows verified load ≲ 2, and
suspicious single-order deltas were re-run reversed-order per the
A-B-B-A rule. The final-binary close session additionally shared the
box with a live sibling benchmark campaign (`perf/read-saturation` —
disjoint devsub devices :54147, shared CPU): ambient load 1–14 across
bracket windows, absorbed by A-B-B-A interleaving + medians + the
reversed-order rule (applied once, §5.1).

## 5. Rig tables

### 5.1 Multi-backend fast venue — sustained 60 s il rewrite, A-B-B-A ×3, medians (final binaries)

| Cell | A (branch) | B (dev-tip) | A/B | A sync_drains | B sync_drains | fence_drops |
|---|---|---|---|---|---|---|
| forced 128 (the field lever) | 3036 MiB/s | 3115 MiB/s | 0.97× | 1473–1646 (off-thread) | 346–635 (lane-freezing) | 0 / 0 |
| default governor (forward) | 3187 MiB/s | 3471 MiB/s | 0.92× | 812–927 | 215–463 | 0 / 0 |
| default governor (REVERSED order, B-A-A-B) | 2747 MiB/s | 2811 MiB/s | **0.98×** | 626–1293 | 256–357 | 0 / 0 |

The forward default cell's 0.92× carried one visibly ambient-depressed
A tail run (2478 with the sibling campaign live); per the standing
A-B-B-A rule the cell was re-run REVERSED and the deficit did not
reproduce (0.98× — and the whole venue shifted down with it, i.e. the
delta was ordering/ambient, not binary). Verdict: **parity band both
orders** — exactly what this venue can prove (see honesty note).
Engagement exact on all 18 runs (`ipc_ops_write` Δ ≈ user ops ×
chunking; zero INVALID); probe gauges on A default: ups 9–14 /
backoffs 9–16 per 60 s (probing at the venue's plateau, retreating on
dead gain — the duty cycle working as designed), probes dormant under
the f128 pin on every A run; B has no probe gauges. A's per-device
aggregate aqu ran 15.9–18.0 vs B's 13.6–16.5 on f128 — the admitted
custody actually reaches the devices. `inflight_mid` 22–128 across
reps on both binaries (the pipe still fills transiently at this
venue's fill level — the residual parking is in-device/fabric
queueing + per-ino 4a merge serialization, both latency-derived, not
fixed widths; see §7 OQ-2).
NOTE (venue honesty): this single-box venue is bounded ~9 GB/s by
colocated producer+softirq CPU on first-write (admission_waits = 0 —
the pipe never even fills), and at the storm's ⅔-fill rewrite it runs
~3–3.5 GB/s where reclaim work itself is CPU on the same box — so it
cannot exhibit the field's absolute halving NOR hand the off-thread
valve a throughput win; what it proves is the valve mechanism
(present on B: 215–635 lane-freezing inline engagements per run; A
pays 626–1646 engagements off-thread) at ≥ parity throughput, with
executor liveness pinned by the red-first canary contract — the
field's lockstep-starvation term (fuse3 current-thread lanes frozen
behind drain ioctls) is structurally gone.

### 5.2 Dialed venue (20 ms) — the depth-term brackets (final binaries)

Short bracket (elbencho kernel 4t × 4M × 4g first-write, A-B-B-A ×3,
medians; one quiet window, sibling at 1-core compile):

| Cell | A (branch) | B (dev-tip) | A/B | aqu-sz (A / B) | amp |
|---|---|---|---|---|---|
| default governor | 1606 MiB/s | 1589 MiB/s | 1.01× | 58–87 / 41–48 | 1.000–1.001 |
| forced 64 | 1615 MiB/s | 1604 MiB/s | 1.01× | 57 / 57 | 1.000–1.001 |

**A default / A forced-64 = 0.99× — the DEFAULT governor reaches the
forced-optimal-lever throughput** (the part-1 acceptance shape), with
its probe visibly driving depth (A default aqu 58–87 vs B's 41–48;
ups 3–5 / backoffs 2–3 inside each ~10 s run).

Sustained 60 s kernel 4t × 4M rewrite loop (A-B-B-A ×3 default,
1 rep/binary f64), same session:

| Cell | A | B | A/B | A gauges |
|---|---|---|---|---|
| default (med of 3) | 1815 MiB/s | 1777 MiB/s | 1.02× | ups 17–30 / backoffs 15–27 per 60 s; `depth_target` 282–424 MB vs `depth_target_base` 209–271 MB — the probe contributing **+25–56 % live depth** at read time; drains 63–598 off-thread |
| forced 64 | 2054 MiB/s (thirds 2057/2038/2057 — flat) | 1877 MiB/s | 1.09× | probe dormant under pin (0/0), target = 64 × 4 MiB verbatim |

Thirds flat on cited rows; amp = 1.000 and wareq ≈ 4 MiB (4093–4095)
on every dial row; `write_pipeline_fence_drops` = 0 on every run.

VENUE HONESTY (final-binary session): today's dial venue ran at
~1.6–2.0 GB/s — roughly HALF the interim-session rates — under a
persistent ~2-core sibling-campaign tax on the shared localhost
nvme-tcp/softirq stack, and at that rate BOTH binaries' pipelines
already keep the 20 ms device saturated (B's BDP aqu 41+ suffices),
so the venue currently cannot DISCRIMINATE the depth term A-vs-B:
default-vs-default reads 1.01–1.02× rather than the interim-session
+12 %/+38 % reads (16 GiB / 20 GiB ramp-amortized and 60 s sustained,
interim tips `14c8a0a`/pre-amend — those survive as SCOPING evidence
only, per the counted-restart discipline; the binaries differ from
final only in the ENOSPC valve shape, which engages on these rows at
just 36–598 drains/60 s). What the final-binary dial rows PROVE:
default ≈ forced-optimal (0.99×), never below dev-tip, probe
engagement/dormancy exactly per policy, amp exactly 1.0. The
depth-term discovery claim itself rests on the field capture (§1:
+18 % forced over the self-limiting BDP) plus the probe-contribution
gauges above; OQ-1 (field re-measure) remains the definitive
instrument.

### 5.3 Latency-guard rows (zram data over nvmet-tcp, fio psync 4k qd1, medians of 3, final binaries)

Quiet-window bracket (sibling 0 %, load < 3; B-A-A-B order):

| Row | A | B | verdict |
|---|---|---|---|
| buffered IOPS | 26.0k | 24.7k | flat (1.05×, in band) |
| O_DIRECT IOPS | 4190 | 4445 | flat (0.94×, in band) |
| O_DIRECT clat avg | 216–251 µs (med 238) | 218–244 µs (med 225) | flat — the venue's ~235 µs fabric RTT bound |

`probe_ups = 0` on EVERY O_DIRECT qd1 A-run and every buffered A-run,
`depth_target` = `depth_target_base` = the floor on all of them — the
RTT-bound low-offered-load shape never launches a probe and never
inherits streaming depth (the latency guard, contract-pinned). A
forward-order pass earlier in a sibling-storm window scattered
0.67×–2.4× in BOTH directions on these rows (one B rep at 2.5k IOPS,
one at 154k) and is labeled ambient scoping per the A-B-B-A rule; the
quiet bracket above is the counted row. (The interim-tip session's
q1 rows — 520k/16.5k with ~60 µs clat — were measured on pre-amend
binaries on a venue whose artifacts did not survive the session
restart in reconstructable form; they are superseded by the counted
final-binary rows above and kept only as the lineage of the guard's
first green.)

### 5.4 write_matrix parity sweep

FILLED-AT-CLOSE (both binaries, fresh loop-devsub zram venue per
sweep — 1 × null_blk meta + 4 × zram data, REPS=3, the `965c528`
FIXED rig for BOTH sweeps). The first (pre-rig-fix) branch pass on a
memory-pressured box FAILED parity on 4 buffered rows; the dev-tip
pass on the same pressured venue showed the mirror-image scatter
(branch kernel-side faster on 2 of them) — both passes were measuring
the RIG (age skew + rand-row residue; the two defects `965c528`
fixed) and are labeled scoping. The clean-box verdicts (2026-07-30
post-crash session, quiet box, thermal steps logged but every pair's
twins adjacent):

| Sweep | Binary | pairs | WIN | LOSS | PAR | exit |
|---|---|---|---|---|---|---|
| A | branch `965c528` | 18 | 6 | **0** | 12 | 0 (green) |
| B | dev-tip `778d6d0` | 18 | 5 | **0** | 13 | 0 (green) |

Both sweeps: engagement exact on every shim row (`ring_per_op` 1.00–
4.00 as expected per bs/slab), zero INVALID cells, and the governing
rule holds on both binaries (shim never loses; wins every decided
pair — A: 4k/64k rand+seq class, B: 4k class + seq-64k-odirect). The
branch neither creates nor loses parity anywhere on the matrix —
consistent with §5.1's parity band on the funnel venue. Standing
observation from the scoping passes: at REPS≥2 the matrix's seq-row
`rm -f` under in-flight pipeline uploads moves
`write_pipeline_fence_drops` on BOTH binaries (+1…+11 per 4m-buffered
row) — a pre-existing unlink-vs-detached-upload race classification,
filed as OQ-3.

## 6. Gates

**Final tip `965c528` (rig-fix commit), from zero — 2026-07-30
post-crash session** (the session that first ran this gate died into
the §6b wedge hunt; this is the counted-restart re-run on the clean
tree):

* `cargo clippy --all-targets --all-features -- -D warnings` — clean.
* `cargo fmt --check` — clean.
* `cargo test --all-features -- --test-threads=1` — **150 binaries,
  0 failures** (the storm test passes at full affinity; its §6b wedge
  needs the 2-CPU squeeze).
* `cargo doc --no-deps` — clean. `cargo bench --benches -- --test` —
  exit 0.
* Loom (`tests/run_loom.sh`) — **52/52** incl.
  `write_pipeline_probe_epoch_roll_is_single_winner`.
* **statfs_tests ×10 loaded soak — 30/30 green, 0 hangs** (looping fat
  release build in a second worktree; load 2.6–4.2; 64–66 s/roll),
  fusectl waiting-connections residue sweep clean.

The earlier from-zero gate on `a44b775` (pre-rig-fix tip, 2026-07-29
— identical code surface, the two commits between differ only in
tests/write_matrix.sh + tests/pug_bracket.sh):

* `cargo clippy --all-targets --all-features -- -D warnings` — clean.
* `cargo fmt --check` — clean.
* `cargo test --all-features -- --test-threads=1` — **150 binaries,
  1,545 tests, 0 failures** (includes the 21 write_pipeline contracts
  — the 7 probe-up contracts among them — and the 12
  async_block_reclaim contracts incl. the 2b canary).
* `cargo doc --no-deps` — clean.
* `cargo bench --benches -- --test` — exit 0 (criterion smoke).
* Loom (`tests/run_loom.sh`, `LOOM_MAX_PREEMPTIONS=3`) — **52/52**
  incl. `write_pipeline_probe_epoch_roll_is_single_winner`
  (weakening-verified both directions per the model's commit).
* **statfs_tests ×10 loaded soak** — **30/30 green, 0 hangs** (load
  recipe: looping fat release build with `touch src/lib.rs` per
  iteration in a second worktree, PLUS the live sibling campaign —
  ambient load 3–30 across rolls; 70–85 s/roll vs the 62 s quiet
  nominal), fusectl waiting-connections residue sweep clean.
* `write_pipeline_fence_drops` = 0 on every run of the close session
  (funnel 18 + reversed 6 + dial rows).

## 6b. Post-crash session (2026-07-29/30): the storm-test wedge forensics

The session that landed `965c528` (the write_matrix rig-validity fix)
died mid-hunt on a **rare hang of
`staged_identity_visibility_tests::layout_transition_storm_reads_never_transient_zeros`**
first seen while re-running the gate under ambient load. The successor
session finished the hunt with a phase-tagged TEMP-DIAG apparatus
(never committed — preserved as the branch stash "TEMP-DIAG wedge-hunt
apparatus"; raw captures in `~/squeezefs-evidence/pug-wedge-forensics/`).
Findings, in evidence order:

* **Repro shape**: never at full affinity on a quiet box (40/40 + a
  clean-gate history); fires ~1/40–1/200 runs when the 8-worker test
  runtime is squeezed onto 2 CPUs (`taskset -c 0-1`, the ambient-load
  schedule made deterministic-ish). Always the same op: round-N op 6,
  the staged→striped promotion.
* **Pre-existence adjudicated**: the UNMODIFIED dev-tip `778d6d0`
  binary wedges the same way (3 catches, same squeeze) — the branch
  touches no meta/DLM surface (`git diff 778d6d0..965c528` = write
  pipeline, reclaim valve, rigs). **Not a campaign regression.**
* **App layers exonerated by direct measurement**: commit conveyor
  idle and healthy at wedge time (live probe `pending=0, leader=false`;
  pass lifecycle spawned==started==exited, panics=0); no upload stuck
  (20 s watchdogs silent); no lock-order inversion (census + tape show
  canonical 3.5→4a ordering); guard census balanced (0 live holders on
  the wedged stripe).
* **The wedged state, decoded from a live process** (gdb typed dumps,
  field addresses resolved from debug info): the promotion parks
  forever in `set_layout_and_size`'s 4a
  `DlmLockManager::lock_inode_exclusive` on a stripe whose
  `tokio::sync::RwLock<()>` batch semaphore reads **permits = 0 free,
  ZERO live guards, one queued waiter with `state` = its full request
  (nothing assigned) and its waker registered**; `try_read` and
  `try_write` both fail. In parallel the M6 times-drain task sits
  mid-`lock_many` (phase tag 20) — in several catches holding earlier
  batch stripes (census-visible) and parked on the same dead stripe.
* **Onset tape** (a 65k-entry acquire/release event ring): reader
  shared guard held → drain's exclusive acquire queues (sweeps mr−1
  from the counter) → reader releases (assigns the last permit — the
  semaphore pops the drain's fully-assigned waiter and takes its waker
  for `wake_all`) → **the drain task never re-polls** (its 536 M
  assigned permits die with the popped node) → the promotion queues
  behind an empty, permit-less semaphore. `Handle::dump` taskdumps at
  wedge time show the drain/reader tasks with EMPTY traces (the
  notified-but-never-run shape) while every runtime worker is parked
  in its driver (gdb) and other tasks keep running (reader beat
  ~130k/s throughout).
* **Independent of tokio version**: reproduces identically on tokio
  1.52.3 (the lock) and 1.53.1 (`batch_semaphore` byte-equivalent to
  upstream master; no relevant CHANGELOG fix between). A standalone
  1,000-line model of the exact stripe/reader/drain/promotion pattern
  (3×2 M rounds, same 2-CPU squeeze) does NOT reproduce — the missing
  ingredient is somewhere in the full process (task census ~10 tasks +
  uring worker threads + fuse3 current-thread lanes).

**Adjudication**: a pre-existing, load-schedule-dependent lost task
wakeup at the tokio-runtime layer (scheduled-task-never-polled /
popped-waiter-never-woken), NOT probe-up/funnel machinery and NOT the
4a protocol. Filed as **OQ-5** below with the forensic packet; per the
standing rule that load-dependent hangs are first-class product bugs
it needs its own campaign (minimal upstream-facing repro + fix or
pinned workaround), which is out of this campaign's charter. The
campaign gate (full-affinity) is unaffected.

## 7. Open questions

* **OQ-1**: field re-measure with this branch (default ≈ forced-64 ≈
  13.7+ GB/s expected on the 2-wide plane; on the 4-wide plane the
  valve fix should lift the 6.8 GB/s halving toward the raw ceiling —
  `block_free_reclaim_sync_drains` and per-device aqu-sz are the
  instruments; client CPU is the next wall per 2026-07-27 OQ-1/OQ-3).
* **OQ-2**: per-ino block-map merge serialization (4a
  `INODE_META_LOCKS`) is the largest remaining upload-lifetime term at
  high thread-per-file counts (map_merge p90 ≈ 64 ms at 32t on the
  fast venue); a per-ino merge coalescer (one merge per ino per
  conveyor pass carrying N entries) would cut it without touching the
  lock order.
* **OQ-3**: `write_pipeline_fence_drops` moves under unlink-during-
  in-flight-upload (REPS≥2 write_matrix seq rows, both binaries): the
  disposition conflates a benign local unlink race (custody moot —
  should resolve as verified no-op) with genuine fencing. Repro-port +
  reclassification owed.
* **OQ-4**: adopt-threshold sensitivity (1/16 gain per ¼ depth step)
  — a windowed-median delivery estimator would allow tighter
  thresholds (v1.1 economics).
* **OQ-5** (pre-existing, found by this campaign's gate — §6b): rare
  lost-task-wakeup wedge under CPU-starved schedules (storm test,
  2-CPU squeeze, ~1/40–1/200; dev-tip reproduces). Tokio-layer
  scheduled-task-never-polled signature with a fully-decoded onset
  tape; forensic packet in `~/squeezefs-evidence/pug-wedge-forensics/`
  + the branch stash "TEMP-DIAG wedge-hunt apparatus". Needs its own
  campaign: minimal repro (the storm-test task census is the seed),
  upstream issue or pinned workaround, and a red-first cargo pin once
  deterministic.
