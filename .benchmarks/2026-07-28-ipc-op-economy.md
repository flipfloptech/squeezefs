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

## 5. Phase B — PENDING (quiet box only; explicit TODO)

The box was contended by the v1.1 release gate for the entire session:
**no counted IOPS row in this note is acceptance**. When the box is
quiet:

1. Build the fabric-latency rig (loop substrate for the latency A/B,
   plus the tcp substrate if any write rows are bracketed):
   `sudo tests/dev_substrate.sh create` (and
   `sudo SQZ_DEVSUB_TRANSPORT=tcp tests/dev_substrate.sh create` for
   fabric-sensitive rows). State the substrate per row.
2. A-B-B-A brackets vs dev tip `f3579f7` (KD-7 pairs per side, fresh
   mount per side, medians of 3, engagement exact — `ipc_ops_*` deltas
   account for every row; alternating order per the standing
   A-B-B-A rule):
   - **Warm il row** (the lever-1 acceptance): elbencho sync t8/t32
     rand-4k over a warm 4×200 MiB set, default interception mount —
     expect the warm ceiling to move with the freed ~12 allocs/op +
     one memcpy/op; also re-run the 2026-07-19 G-L4-2 warm shape
     (1.02 M IOPS reference).
   - **Reap rows** (the lever-2 acceptance): fio/elbencho libaio
     t16qd16 + t32qd32, sessions ∈ {1,4,8}, plus the 16-process fio
     fleet — watch `ipc_cqe_wake_writes/(writes+elided)`,
     svc voluntary ctx switches, completion clat avg/max (the
     9.3 ms tail class), and the ~525 k plateau.
   - **Protected rows** (must not regress): il sync t32 qd1
     (the psync-class row), il libaio t1 qd1 RTT (~35 µs il
     advantage), kernel rows as context.
3. `REAP_EVENT_PARK_MAX` A/B at qd32 (event-parked vs 24-batched under
   the NEW wake economics) — retune or keep, counted.
4. `sudo tests/run_preload_gate.sh` leg 2 (mount parity + engagement +
   kill-9 ×5 and fork-kill-parent soaks — the reaper-parked-leak
   posture rides these).
5. Record everything as an addendum to this note; only then may the
   campaign's rows feed the scoreboard.

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
