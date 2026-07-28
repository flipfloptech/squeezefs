# 2026-07-28 — IPC op economy: allocation-free warm serve prelude + completion-doorbell wake elision

Branch `perf/ipc-op-economy` (off dev tip `f3579f7`), run in an isolated
git worktree while the main tree ran the v1.1 release gate (the box was
CONTENDED for the whole session — every build/test `nice -n 19`;
substrate/root rigs untouched because the release gate owns them).
Commits: red `eeebeae` (the lever-1 contract + profiler), green
`2db89ec` (prelude alloc elimination), `eab5868` (CqeDoorbell core +
loom), `ba6ebf1` (wake-elision wiring), plus the docs/evidence commit
carrying this note.

## 0. Instruments (contention posture — honest)

The release gate owned the machine and the devsub substrate, so Phase A
used **deterministic, contention-tolerant instruments only**:

- **Alloc profile/contract**: `tests/ipc_op_economy_tests.rs` — a
  counting global allocator around an engagement-verified warm
  ring-read window over the REAL host → pinned service thread → sink
  path (raw-protocol client, the preload_parity harness shape).
  `SQZ_ALLOC_TRACE=1` flips it into the alloc-site profiler
  (recursion-guarded backtrace capture, deduped site table). Alloc
  COUNTS are load-independent; this is the standing per-commit gate.
- **Wake economy**: the `ipc_cqe_wake_{writes,elided}` counters +
  in-process contract tests (elision + prompt parked wake) + the loom
  model. Syscall-rate/IOPS claims are **deferred to Phase B** (§5).

No IOPS number in this note is acceptance evidence; the counted
brackets are Phase B (quiet box).

## 1. Lever 1 — the warm-serve prelude allocations

### 1.1 Profile (the conviction table)

Pre-fix, the warm §5.5.1 staged-serve fast path measured **11.97
allocs/op** (5,000-op window, engagement exact: `ipc_fast_path_serves`
delta == ops). The traced window attributed, per op:

| allocs/op | site |
|---|---|
| 2 | `CachedMetadata::clone` (metadata_cache get #1 in `ipc_read_probe_locked`) — `file_type` String + `file_id` String |
| 2 | `CachedMetadata::clone` (get #2, the tier-serve branch re-get) |
| 1 | `keys::active_block` (CompactString > 24 B ⇒ heap) |
| 1 | `FsKey::fmt` (the `.to_string()` on that key) |
| 1 | `keys::inode_path` (`format!`) |
| 1 | `keys::active_block_ext_for_path` (the W2 overlay probe key) |
| 1 | `read_staged_zero_copy` — `Bytes::copy_from_slice(file_id)` key mint for `get_static(&Bytes)` |
| 1 | `StagedMetadata::deserialize` — a `file_path` String materialized per serve to read ONE u64 |
| 1 | `try_read_range_sync` — the payload `Bytes::copy_from_slice` bounce (alloc + extra memcpy) |
| ~0.2 | service-loop park vectors (`observed` + waiter `Vec` per park pass — a per-OP cost at qd1 RTT) |

(The striped/hot-tier shape shares the same sites minus the staged
header peek, plus a `block_map` value-String clone.)

### 1.2 Fix (what landed — deletion, not pooling)

- ONE `metadata_cache.get` per probe (the second was pure waste).
- `CachedMetadata` field types: `file_type: CompactString` (every
  layout class inlines), `block_map_id`/`block_prefix`/`file_id:
  Option<Arc<str>>` (clone = refcount). The struct is handed out BY
  VALUE on every read — its clone is now allocation-free everywhere,
  not just on the ring path.
- `keys::StackKey` (192 B fixed, overflow → heap fallback, NEVER
  truncates) + `active_block_stack`/`inode_path_stack`; the probe and
  `try_read_range_sync` format keys on the stack. Borrowed-key lookups:
  dashmap/moka/scc accept `&str`, `get_static` is `&[u8]`-keyed
  (`Bytes: Borrow<[u8]>` — same hash, zero mint).
- `StagedMetadata::peek_original_size` — identical validation
  (length arithmetic + the UTF-8 check), no `file_path` String.
- **Serve-into-arena** (`crate::PayloadSink`, implemented by
  `ArenaWindow`): the tier legs write payload straight into the ring
  op's validated arena window (`IpcReadProbe::Served(n)`) — the
  intermediate `Bytes` bounce (1 alloc + 1 memcpy/op) is deleted. The
  active-buffer snapshot leg keeps its outside-the-guard copy
  (critical section unchanged); the tier legs' memcpy already ran
  under the guard pre-fix (into the bounce buffer), so guard hold time
  is unchanged or better.
- Zero-alloc W2 overlay gate `has_staged_extent_runs` — existence
  probe on the latch-free occupancy index; conservative
  demote-on-presence (a record-bearing block is not a warm clean
  block; the async handler composes as before).
- Allocation-free service parks: hoisted+reused doorbell-snapshot Vec;
  `futex_wait_many` is iterator-fed with a stack `[FutexWaitv; 128]`.

### 1.3 Contract (green, standing)

`warm_fast_path_serves_are_allocation_free`: 5,000-op engagement-exact
warm window, bound ≤ 1 alloc / 100 ops (moka-housekeeping tolerant).
Post-fix measurement: **~6 stray allocs / 5,000 ops (≈ 0.12/100)**, and
the traced window attributes **zero remaining squeezefs-frame sites**.
12 → ≈ 0 allocs/op.

## 2. Lever 2 — completion-side wake economy (the cqe doorbell)

### 2.1 The serialization term

Pre-fix, the completion-reap side paid, in the sparse (event-driven)
regime: one `park_prepare` RMW per pending ticket per park (WAITER
bits), a `futex_waitv` **array built per pending ticket** (O(qd) setup
per park cycle), and — the daemon half — **one `FUTEX_WAKE` syscall per
completion** toward any WAITER'd slot. That per-completion
collect-and-wake stream is the named ~525 k IOPS cap term
(reap-economy note §5 already measured the deep-regime face of it:
fully event-parked qd32 regressed −6 % from exactly these costs).

### 2.2 The protocol (what landed)

`squeezefs-ipc::cqe_core::CqeDoorbell` — per-session completion seq +
parked-reaper count, embedded in the session header's reserved line 2
(own cache line; `IPC_ABI` 1 → 2, KD-7 already forces same-build
pairs):

- **Daemon** (`SlotCompletion::complete`, the single completion site):
  after the slot DONE publish — seq bump, then wake ONLY if a reaper is
  registered parked (`ipc_cqe_wake_writes`), else elide
  (`ipc_cqe_wake_elided`). Per-slot WAITER wakes for sync clients are
  UNCHANGED.
- **Reaper** (libaio sparse regime): `cqe_park_begin`
  (register-then-snapshot) per DISTINCT session (≤ `IL_SESSIONS`
  words), the mandatory post-registration pending re-scan (the
  disarm→scan law), `futex_waitv` over the session words, `park_end`.
  `Session::ticket_wait_entry` deleted (no dead code).
- The Dekker `fence(SeqCst)` pair in `complete()`/`park_begin()` is
  **load-bearing** (the W1 §5.1 house pattern): loom found the strand
  without it on the first run.

Trust boundary: both words are client-writable shm — the worst a
hostile client buys is the pre-campaign one-wake-per-completion posture
(bounded, self-harm only); the daemon never waits on either word. A
kill-9'd parked reaper leaks a nonzero parked count on ITS session
until idle reap — bounded to that session's completions, degrading to
the pre-campaign wake rate, never a correctness term.

### 2.3 Verification

- **Loom** (`loom-models`, 48/48 green — 47 existing + the new
  `ipc_cqe_parked_reaper_never_stranded`, composed with the shipped
  `SlotCore` DONE publish). Weakening verified ×3 (each fails the
  model, then restored): (a) either Dekker fence removed, (b)
  `park_begin` permuted to snapshot-before-register, (c) the daemon's
  `parked` load weakened to `Relaxed`.
- **Contracts** (in-process, deterministic):
  `unparked_completions_elide_cqe_wakes` (256 warm serves ⇒ writes
  delta 0, elided ≥ 256) and
  `parked_reaper_is_woken_by_completion_cqe_wake` (raw-protocol reaper
  parked on the cqe word wakes < 2 s vs the 5 s strand bound; the wake
  is counted).
- `REAP_EVENT_PARK_MAX = 24` (the deep-regime batching threshold) kept
  VERBATIM — it was sized under the old wake economics and is flagged
  in its doc as a Phase B re-measure candidate. No unmeasured behavior
  change in the deep regime.

## 3. Stats added

`ipc_cqe_wake_writes` / `ipc_cqe_wake_elided` (stats inode; the
`transport_wake_*` naming discipline): `writes/(writes+elided) ≈ 1`
under a saturated reaping client means the parked gate stopped eliding.
Must-stay-0 tripwires (`ipc_descriptor_rejects`,
`ipc_sessions_poisoned`) untouched. AGENTS.md updated (LD_PRELOAD
section + stats surface).

## 4. Gates (Phase A, contended box — all `nice -n 19`)

- `cargo clippy --all-targets --all-features -- -D warnings` — clean
  (root; ipc + preload crates too).
- `cargo fmt --check` — clean (root + ipc + preload crates).
- `cargo test --all-features -- --test-threads=1` — from zero on the
  final tree: **146/146 test binaries green** (zero FAILED, zero
  panics; the definitive persisted-log run, 21.6 min contended).
- `cargo doc --no-deps` — clean.
- `cargo bench --benches -- --test` — bench smoke green (all four
  bench binaries).
- loom: **48/48** (`tests/run_loom.sh` — 47 existing + the new cqe
  model).
- `tests/run_preload_gate.sh` leg 1 (unprivileged) — **PASSED**
  (sanctioned preload-release build + crate clippy/fmt/tests, Issue-4
  wrong-profile guard proof, plain-file passthrough battery, libaio
  lifecycle passthrough ×3 orderings).
- **Leg 2 (sudo) PENDING**: the v1.1 release gate was actively running
  on the main tree's substrate for the whole session (verified live —
  its cargo test binaries + the `sqzdevsub_*` null_blk items). Run
  before merge: `sudo tests/run_preload_gate.sh`.

## 5. Phase B — the counted acceptance brackets (2026-07-28, quiet box)

Rebased onto dev `22c31ac` first (clean 7/7; the write-times
single-authority fix composes — `park_write_times` rides the write
handler after the attr publish and never touches the retyped
`CachedMetadata` fields; ring writes inherit the single-authority stamp
through the same handler; `write_times_durability_tests` green).

### B.0 Setup

- **Sides (KD-7 same-commit daemon+shim pairs, clean identities, no dev
  override):** BASE = dev tip `22c31ac`; CAMP = the campaign tip
  (`e45e221` for the bracket, `62cdb76` after the retune below).
- **Venues:** `V-zram` = devsub loop (null_blk mds `/dev/nvme1n1` +
  zram oss `/dev/nvme5n1`) — the G-L4-2 acceptance venue, warm/dt/write
  rows; `V-lat` = devsub mds + a 235 µs fabric-latency data namespace
  (memory null_blk `completion_nsec=235000`, irqmode=2 → nvmet-loop,
  `nvme connect -i 8`; fio psync qd1 raw: 4,141 IOPS ≈ 241 µs — the
  2026-07-26 reap-economy venue) — every libaio/park row. LOOP
  substrate throughout (per-op-economy campaign; no bandwidth-bound row
  is claimed, so no tcp/write-amp columns are owed).
- **Instruments:** elbencho 3.1-10 (dynamic; sync + `--iodepth` libaio
  drivers) and fio 3.42 (dynamic; qd1 RTT + the 16-process fleet). 4 KiB
  `--rand --direct`, dataset 8 × 256 MiB striped (coverage-bound il
  rows: engagement delta == 524,288 == the full dataset op count, the
  strongest §3-rule-4 form). fio rows: 10/15 s time_based, engagement =
  fio `total_ios` == `ipc_ops_read` delta. Warm rows: hot tier sized
  over the dataset (`SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB=3072`), two
  shim-side warm passes (second-touch admission), fast-path share 99 %.
- **Discipline:** fresh format + fresh mount per side invocation,
  medians of 3 per row, A-B-B-A across side invocations, no-shim kernel
  rows as the box-state CANARY. Driver: `/tmp/sqz-phaseb/rig.sh`
  (row = IOPS + every engagement/wake/CPU delta from the stats inode).

### B.1 Bracket 1 (counted; clean window 14:01–14:13 UTC, canaries stable ±2 %)

Medians of 3; engagement exact on every il cell (`dR`/`dW` == ops; a
warm cell is 99 % `ipc_fast_path_serves`, a dt/aio cell is 100 %
`ipc_direct_drive_serves`).

**Lever 1 (V-zram):**

| row | BASE-1 | CAMP-1 | CAMP-2 | BASE-2 | verdict |
|---|---|---|---|---|---|
| warm-il-t8 (IOPS) | 1,435,257 | 1,747,958 | 1,855,525 | 1,443,762 | **+22–29 %, order-independent** |
| … daemon CPU / 524 k ops | 1,370 ms | 1,120 ms | 1,000 ms | 1,300 ms | **−18–23 % CPU/op** |
| warm-il-t32 | 1,169,502 | 1,546,763 | 1,355,211 | 1,196,065 | **+15–30 %** |
| warm-kernel-t8 (canary, no shim) | 248,740 | 252,506 | 250,310 | 242,672 | flat ✓ (box stable) |
| il-randwrite-t8 | 141,022 | 140,364 | 139,302 | 134,410 | **wash** (writes are handoffs — lever 1 never touched them; stated) |
| dt-il-t256 | 507,601 | 502,599 | 496,197 | 489,608 | **wash** (direct-drive path, lever-1-free by design) |

**Lever 2 (V-lat, doorbell at the un-retuned park threshold 24):**

| row | BASE-1 | CAMP-1 | CAMP-2 | BASE-2 | verdict |
|---|---|---|---|---|---|
| il-aio-t16qd16 | 486,633 | 380,133 | 373,397 | 477,878 | **−21 % REGRESSION — found by the bracket** (below) |
| il-aio-t32qd32 | 535,031 | 529,077 | 515,429 | 514,831 | wash (deep regime batches, wakes ≈ 0) |
| il-aio-rtt-qd1 (fio) | 3,968 / 251 µs | 3,964 / 251 µs | 3,971 / 250 µs | 3,956 / 251 µs | wash; **the qd1 latency contract held** (cqeW == ops there by design) |
| il-fleet-16proc (fio) | 482,930 | 475,028 | 478,841 | 468,733 | wash (one reaper per session ⇒ herd size 1) |
| kern-aio-t16qd16 (canary) | 361,091 | 349,993 | 342,180 | 344,704 | ±3 % ✓ |

### B.2 The found regression and its mechanism (the wake counters convict it)

At t16qd16 the campaign side paid `ipc_cqe_wake_writes` ≈ 519 k per
524 k ops: qd16 per ctx ≤ the old park threshold (24) kept elbencho's
16 threads event-parked, and the SESSION-level doorbell — 16 threads
sharing 4 fd-sharded sessions — wakes EVERY parked reaper on the
session per completion (breadth is the no-strand law): a wake herd +
spurious rescans the per-ticket WAITER parks never paid. The fleet
(one reaper per session) and qd32 (deep regime, batched) were immune —
exactly the counter signature.

### B.3 The `REAP_EVENT_PARK_MAX` A/B (flagged in Phase A; campaign side, V-lat, same clean window)

| config | t16qd16 | t32qd32 | rtt-qd1 (clat) | fleet (daemon CPU) |
|---|---|---|---|---|
| 24 (old default) | 380,133 / 373,397 | 529,077 / 515,429 | 3,964–3,971 (250–251 µs) | 475,028 / 478,841 (70.9–71.1 s) |
| 4096 (event-park all) | 366,068 | **256,054 (−52 %)** | 3,957 (251 µs) | 477,379 (70.7 s) |
| 0 (batch all) | 505,356 | 525,612 | **3,301 (301 µs — the 50 µs quantum tax)** | 484,527 (64.4 s) |
| **2 (shipped default, `62cdb76`)** | **505,952** | **526,867** | **3,965 (251 µs)** | **488,089 (63.9 s)** |

**Retune adjudicated 24 → 2** (commit `62cdb76`): qd ≤ 2 keeps the
event-driven wake (the latency-contract shapes — qd1 RTT byte-identical
to baseline), everything deeper batches on the 50 µs quantum whose
latency share is invisible at depth. Against the counted baselines:
t16qd16 **505,952 vs 486,633/477,878 (+4–6 %, the regression is now a
win)**, t32qd32 wash (−2 %/+2 % vs the two baseline sides), rtt wash,
fleet **+1–4 % with −10 % daemon CPU** (63.9 s vs 70.2–71.2 s per
~7.2 M ops). `ipc_cqe_wake_writes` ≈ 0 at depth (7–164 per 524 k ops),
== ops at qd1 — the elision gauge behaves exactly as designed.

### B.4 Contaminated brackets (recorded, NOT counted)

A second full bracket (CAMP-3/BASE-3/CAMP-4, 14:26–14:37) and a
confirmation pair (CAMP-5/BASE-4, 14:43–14:51) ran while the box
degraded (a desktop slicer + kswapd churn): the no-shim kernel CANARY
rows collapsed −55–65 % on BOTH sides simultaneously (e.g.
warm-kernel-t8 249 k → 93–117 k), so those pairs are invalid as counts.
Recorded because their WITHIN-window direction corroborates every
verdict above (CAMP-5 vs BASE-4: warm-t8 1,494 k vs 1,195 k, t16qd16
242 k vs 220 k, t32qd32 236 k vs 213 k, fleet 241 k vs 217 k — campaign
ahead on every il row at the retuned default). The canary discipline is
the takeaway: **a bracket without a no-change context row cannot even
see this failure mode.**

### B.5 Preload gate (final binary `62cdb76` pair)

`sudo tests/run_preload_gate.sh` — **both legs PASSED**: leg 1
(sanctioned build, Issue-4 guard, passthrough battery, libaio lifecycle
×3), leg 2 (mount parity + engagement, notify delivery, dup /
close_range / lseek pins, fio + elbencho + fio-libaio verify,
foreign-netns rendezvous, **kill-9 soak ×5 and fork-then-kill-parent —
zero session/arena residue on the ABI-2 doorbell**, establish-refused
shape, direct-drive kill-9 soak).

### B.6 Post-rebase / final-tree gates

- Rebase onto dev `22c31ac`: clean (7/7, zero conflicts); directly-
  affected suites green post-rebase (ipc_op_economy, preload_session,
  **write_times_durability**, killpriv_v2, metrics_counter).
- Final tree (`perf/ipc-op-economy` @ the retune tip): clippy
  `-D warnings` clean (root + ipc + preload), fmt clean, **loom 48/48**,
  bench smoke green, `cargo doc --no-deps` = the 3 pre-existing
  warnings that reproduce byte-identically at dev tip `22c31ac`
  (handoff_spawn/GhostTable private-item links — inherited, recorded).
- `cargo test --all-features -- --test-threads=1` from zero:
  **147/147 test binaries green** (the acceptance pass). One earlier
  from-zero roll hit `writeback_tests::test_small_block_map_stays_inline`
  ("72 vs 71 allocated data blocks") — **reproduced on the UNTOUCHED
  baseline worktree at dev tip `22c31ac` (1-of-3 full-binary rolls)**:
  a pre-existing intermittent inherited from dev, recorded here so it
  is not silently absorbed; not this branch's surface. (A second
  apparent failure, `async_block_reclaim_tests::field_rewrite_...`,
  came from an operator error — two overlapped full-suite runs in one
  worktree — and is not evidence of anything.)

### B.7 Rig teardown

`/mnt/sqz-phaseb` unmounted, daemons killed; `tests/dev_substrate.sh
teardown` + the `sqzlat_oss0` fabric-latency device torn down
(nvme disconnect, nvmet port/subsystem removed, null_blk powered off) —
zero-residue verified in the closing checklist.

## 6. Found while profiling (recorded)

- The per-park `Vec` pair in the service loop was a per-OP allocator
  cost at qd1 RTT shapes (one park cycle per op) — fixed here; worth
  remembering: **park-path costs are op-path costs at RTT depths**.
- loom models SeqCst RMW/load pairs across two locations WEAKER than
  the C++ SC total order unless an explicit `fence(SeqCst)` sits
  between — the same lesson as the W1 patch/clone fence. Any future
  Dekker-shaped gate here must be fence-paired and loom-checked.
- `.benchmarks/2026-07-28-release-gate-v1.1.md` exists untracked in
  this worktree (the release gate's evidence file) — deliberately NOT
  committed by this branch.
