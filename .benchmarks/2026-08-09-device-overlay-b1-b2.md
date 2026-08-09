# Approach B — device-backed visible overlay, rungs B1+B2 (2026-08-09)

Branch `perf/device-overlay` off dev `51486ea3`. Charter:
`docs/design-device-overlay.md` Rev 2 (the nine laws, the B1–B9 ladder,
KD-OV-10..14); comparator per the §1.4 four-case table = **case 2, the
shipped extraction control** (Approach A perf-falsified-but-correct,
`.benchmarks/2026-08-09-fuse-placed-merge.md` — its mechanism is dead
evidence, not a bar). Falsification-first discipline: each rung's
falsifier stated before implementation; a falsified rung STOPS the
ladder with its evidence.

## Verdict up front

* **B1 PASSED its falsifier** — every law was expressible as a pure
  transition (no under-specification found); the loom models CAUGHT and
  fixed a real store-buffering hole in the freeze/begin_store pair
  before any I/O existed.
* **B2's falsifier FIRED — honest failure, the ladder stops here.**
  Engagement mechanics are EXACT (store bytes 96.9 % of user bytes, all
  slot-direct, extraction 3.1 %, nt_copy 2.3 %, 31–32 qids engaged,
  amp 1.008, zero fallbacks/tripwires across every leg), and the armed
  posture **loses 0.77× to the shipped extraction control in both
  bracket orders** (armed median 1.126 GB/s vs control median 1.456,
  reset-per-leg, ≥ 75 s flat sustained rows). The residual is named and
  measured (§5): **the armed leg is pipeline-depth-bound by
  ACK-after-CQE (KD-OV-7) at kernel-serialized lane count** — the
  extraction loop the overlay deletes was NOT the control's binding
  constraint on this venue, because the control pays its two CPU passes
  OFF the ACK path while the BDP pipeline keeps ~128 MiB of DMA in
  flight; the overlay's in-flight custody is structurally capped at
  (lanes × 1 MiB). The §8 payload-retention accelerator (ACK-early,
  kernel 0029 charter) is therefore the PRECONDITION for any further
  rung, not an accelerator — recorded as the redirect.
* `SQUEEZEFS_DEVICE_OVERLAY` ships **default OFF** (the measured
  posture — D17), lever + machinery retained as the counted A/B
  instrument with correctness green.

## 1. B1 — the pure state core (landed)

`a3cc909e` (red: 13 contracts — deterministic law pins + proptest
schedules) → `b7fb36fe` (the build). What landed:

* **`src/coverage_core.rs`** — `record_write`'s written-coverage union
  extracted VERBATIM into the ONE shared law (KD-OV-2). `active_block.rs`
  routes through it behavior-identically: the existing coverage suites
  (lib `cache::active_block` 270-test bin, `write_through_coverage` 8,
  `extent_overlay` 14, `extent_patch` 20) re-ran unmodified, green.
* **`src/overlay_core.rs`** — the record state machine
  (`Open → Frozen → Published | Superseded | FenceDropped`), the law-6
  generation word (bumped per ACCEPTED segment; §5.2 revalidates on
  EQUALITY), law-3 coverage publication (full-success CQE only;
  `covered_probe` additionally screens the in-flight claim bitmap so an
  in-flight range is never served — the re-write-in-flight torn-read
  screen), the §2.3 range-claim overlap exclusion REUSING
  `placed_core::PlacedClaims` (grant at submit, release only at CQE —
  the stale-DMA counterexample unrepresentable; KD-OV-10; `overlaps()`
  added to placed_core as a read probe, not a fork), and law 9 as
  `rollback_admissible ⇔ terminal ∧ inflight-empty` with the
  per-terminal-state disposition left to the prod mint owner (KD-OV-11).
* **Proptest** (`tests/overlay_core_tests.rs`): arbitrary
  begin/complete/freeze/supersede schedules — no two overlapping
  in-flight claims ever granted; coverage grows only via full-success
  CQEs and serves exactly when no in-flight store overlaps; the
  completion transition fires once; generations strictly monotone;
  law-9 equivalence at every step.
* **Loom** (`loom-models`, `#[path]` — the exact shipped code):
  `overlay_validated_serve_is_never_torn` (§5.2 words; device bytes as
  Relaxed words — the protocol words carry ALL the exclusion) and
  `overlay_freeze_drain_observes_final_coverage`. **The second model
  FAILED against the first implementation** — a real SB hole: a store
  could pass its state re-check while the freezer read `inflight == 0`,
  i.e. neither backed out nor was drained. Fixed with the W1 §5.1
  `fence(SeqCst)` pair (`begin_store`'s publish→check fence +
  `inflight_empty`'s check-side fence); **both weakening-verified**
  (removing either fails the model — 3 failing schedules each).

## 2. B2 — sync reservation + fresh/hole stores (landed, lever OFF)

`497ac390` (red: 8 contracts) → `965ecb1d` (the build) → `d719553e`
(the per-block epoch-screen fix + the rig). Scope exactly the ladder's:
blocks with NO old binding (fresh allocations and holes — law 5's gaps
are zeros, no displaced free exists).

* **Write half**: the SHAPE screen decided BEFORE the extraction (an
  eligible slot never pays the bounce); the STATE screen under the held
  `BLOCK_FLUSH_LOCKS` guard — striped authority only (§7.1: `file_type`
  IS the promotion-complete witness), block unmapped in the inline map
  (indirect maps decline, noted below), no staged/RAM custody, no
  rewrite-shadow binding **for this block** (the ino-wide screen was
  measured to cascade-disable the overlay — one accumulation block's
  detached publish opens an epoch and every concurrent fresh segment
  declines forever; the five KD-OV-12/B4 hazards are all same-block, so
  the screen narrowed to `rewrite_epoch_binds_block`). Install =
  `allocate_block` (law 2; incarnation unstable till publication) +
  `InflightAllocGuard` (fsck visibility ONLY) + `MintedBlockGuard` (the
  law-9 rollback owner; disposition `Published` ⇒ disarm, `Superseded`
  ⇒ free, `FenceDropped` ⇒ disarm-without-free — W5). Store =
  `ZcWriteSlot::store` on the FUSE queue ring (KD-OV-4), pooled §4.3
  vehicle as the loud fallback; **ACK-after-CQE** (KD-OV-7). Coverage
  completion freezes + publishes on a detached `tpc_spawn_guarded` task
  (the pipeline-depth posture); publication = ONE
  `merge_block_mappings_coalesced` tx (law 7) with the FIND-M11-A
  fencing-retry loop; failures keep the record (the acked bytes' only
  copy) — never freed; `WriterGuardFenced` is the W5 arm.
* **fsync** = freeze → complete → **seed zeros to a whole block**
  (§6.2 step 3) → data barrier (step 4 strictly before step 5) →
  publish; unmount drain ditto; `overlay_unpublished_at_fsync` counted.
* **Read posture (B2)**: reads of open overlays DRAIN
  (freeze/complete/seed/publish) — the §5.2 lock-free composition is
  PR B3. The ipc direct-drive probe gained the overlay screen (§5.4's
  DEMOTE-never-purge law); truncate supersedes at/beyond the cut and
  drains below; fallocate/unlink/copy_file_range drain (§7 row 6).
* **fuse3**: `hold_candidate` gains the streaming-hold mode (armed at
  mount by the knob; the registered HOLD GATE's overlay arm does exact
  screening — un-consumable holds pay the memoized lazy extraction,
  which is the armed posture's priced cost) + the per-qid store census
  (`zc_write_store_qid_census`, `overlay_store_submits_qids` on the
  stats inode — the §4.3 fabric-queue spread instrument).
* **Stats**: the §9.1 `overlay_*` family + `unpublished_offsets_recovered`
  (correction C: wired to the recovery walk's gap-completing arm — the
  honest generic census; overlay attribution exists only as the
  fault-injection suite's harness knowledge, KD-OV-14).
* **Contracts green** (`tests/device_overlay_tests.rs`, 8): engagement +
  publish-at-completion; partial-overlay fsync seeds exactly the gap
  bytes and size floors at the acked end (generic/795); law-5
  recycled-content pin against a deliberately dirtied free list (a
  freed 0xEE file's offsets recycle into overlay destinations — gaps
  read zeros, never 0xEE); RYW + a generic/209 fresh-file storm; the
  KD-OV-12 B2 pin (shadow default-ON, zero epoch interaction across
  overlay windows); **kill-9 recovery** (undrained session → remount:
  pre-overlay image intact, un-fsynced overlay bytes lost per the
  stated §6.1 contract, and the unpublished destination is re-handed by
  the recovered allocator — offset-exact); structural refusals
  (unaligned, mapped-block overwrite); truncate drain-or-supersede.
  Full `write_visibility_tests` (generic/209 storm + serialized
  discriminator) green with the machinery in-tree.

## 3. The bracket (counted, from zero after two rig-gate re-derivations)

Venue: **tcp devsub** (nvmet-tcp on lo; meta /dev/nvme{1..4}n1 nullb,
data /dev/nvme{5..8}n1 zram 8G×4), sqz kernel 7.1.6-1-cachyos-sqz,
`fuse3_zc_negotiated=1` FATAL-pinned per leg, binary `d719553e`
(release, default features). Instrument: fio 3.42 libaio direct=1,
bs=1M iodepth=8 numjobs=8 nrfiles=4 size=512m — **batched fresh-ingest
sustained rows** (B2's eligible shape is fresh files; a `time_based`
loop over one file set would leave eligibility after pass 1): write a
fresh 16 GiB set, unlink it (offsets recycle through the drain +
reclaim), repeat to ≥ 75 s of fio wall time (20–26 batches/leg).
RESET-PER-LEG (fresh format), order **O1 C1 C2 O2 O3 C3** (A-B-B-A +
third samples), per-batch BWs are the flatness check. Rig:
`.benchmarks/rigs/2026-08-09-device-overlay-bracket.sh`; artifacts
`~/tmp/sqz-overlay-artifacts-2026-08-09/`. Counted-run discipline: the
first counted run aborted on a rig GATE (the control posture's own amp
floor, below) — gate re-derived, count restarted from zero; the
sequence above is the from-zero pass.

Same-day RAW ceiling (4-dev aggregate): **2.272 GB/s** @ bs=1M qd8×8;
**2.250** @ bs=4M (request size is a NON-term at raw on this venue);
2.068 @ bs=1M qd32×8.

| leg | posture | batches | BW GB/s | amp | wareq | store share | direct | extract | nt_copy | qids (max share) |
|---|---|---|---|---|---|---|---|---|---|---|
| O1 | overlay | 21 | 1.147 | 1.008 | 1048 KiB | 96.9 % | 87.4 GB | 3.1 % | 2.3 % | 32 (14 %) |
| C1 | control | 26 | 1.446 | 1.053 | 4007 KiB | 0 | 0 | 100.0 % | 99.2 % | — |
| C2 | control | 26 | 1.488 | 1.052 | 4007 KiB | 0 | 0 | 100.0 % | 99.2 % | — |
| O2 | overlay | 20 | 1.119 | 1.008 | 1048 KiB | 96.9 % | 83.2 GB | 3.1 % | 2.3 % | 32 (13 %) |
| O3 | overlay | 20 | 1.126 | 1.008 | 1048 KiB | 96.9 % | 83.2 GB | 3.1 % | 2.3 % | 31 (12 %) |
| C3 | control | 26 | 1.456 | 1.058 | 4007 KiB | 0 | 0 | 100.0 % | 99.2 % | — |

**Medians: overlay 1.126, control 1.456 → 0.773×** (bracket orders:
O-before-C 1.147/1.446 = 0.79×, O-after-C 1.126/1.488 = 0.76× — the
loss is order-independent). Overlay = **0.495× same-day raw** (the
§1.4 row-4 floor is ≥ 0.80× — missed by 1.6×). Every row flat (no
batch decay beyond noise); fallbacks/short stores 0; every tripwire 0
(`overlay_fence_drops`, `overlay_teardown_waits`,
`overlay_unpublished_at_fsync`, `overlay_supersessions`,
`invariant_tripwires`, zc bridge cancels/lost,
`data_dma_fence_refusals`, `write_pipeline_fence_drops`);
`overlay_open` 0 at every quiesce; publishes ≡ installs.

The 3.1 % non-overlay residue is **block 0 of every file** — the
§7.1 promotion boundary working as specified (the inline/staged prefix
plus its whole-block write-through), not an engagement defect.

Amp columns: overlay **1.008** vs control **1.052–1.058** — the
overlay is ~4.5 % more byte-efficient at the device (no staging spill,
no seed, no restage churn); the control's 1.052 exceeded the 1.05 gate
on its own posture (first counted run's abort) — the armed rows keep
the 1.05 FATAL, the control gates at 1.10 with this probe recorded
(the Approach-A small-row precedent: a posture floor of the batched
fresh-ingest shape, not an overlay term). wareq 1048 KiB on the armed
legs is the per-segment store size by construction — NOT a collapse
(raw proves the venue is request-size-flat).

## 4. Attribution probes (labeled, not acceptance)

| probe | posture | BW GB/s | clat mean |
|---|---|---|---|
| bracket O (32 lanes: 8 jobs × 4 files) | overlay | 1.126–1.147 | 46.7 ms (p50 45, p99 85) |
| bracket C (32 lanes) | control | 1.446–1.488 | 40.5 ms (p50 22, p99 202) |
| 128 lanes (nrfiles=16, size=256m) | overlay | 1.229 | 41.2 ms |
| 128 lanes | control | 1.621 | 29.3 ms |
| control + `SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS=0` (sync inline WT) | control | 1.407 | 40.3 ms |

## 5. The residual, named (the B2 falsifier's required profile)

1. **The loop was not the cost — the control hides it off the ACK
   path.** The control pays TWO full CPU passes per byte (extract
   100 %, nt_copy 99.2 %) and still wins: on a 32-CPU box at
   1.45 GB/s, ~3 GB/s of memcpy is spare change, and the write
   pipeline detaches the whole-block DMA from the WRITE reply — its
   BDP governor held **128–137 MiB** of in-flight DMA
   (`write_pipeline_depth_target`) vs the armed leg's structural
   ceiling of ~32 MiB (one 1 MiB store per kernel-serialized lane).
2. **ACK-after-CQE × per-inode serialization is the bound.** The
   Approach-A capture probes proved extending O_DIRECT writes are
   `i_rwsem`-serialized per inode (cohorts of one). Under KD-OV-7 every
   1 MiB WRITE's reply waits for its device CQE, so the armed leg's
   concurrency is exactly the lane count: 32 lanes × 1 MiB ÷
   (per-segment end-to-end ≈ 28 ms at saturation) ≈ 1.15 GB/s —
   the measured row. Quadrupling lanes (128) moved the armed leg only
   +9 % (control +11 %) — by then both postures share the venue's
   FUSE-path plateau, but the ordering never inverts.
3. **The request-size term is ~0 here** — raw is flat 1 MiB vs 4 MiB
   (2.27 vs 2.25 GB/s); the armed leg's wareq 1048 KiB is not what
   loses the bracket on this venue (a real fabric may differ — a field
   row would need its own raw bracket).
4. **The ACK-coupled CONTROL still beats the overlay** (sync-inline
   lever: 1.407 vs 1.126): even paying one whole-block DMA on every
   4th ACK, the control's other 3 ACKs are RAM-merge-instant. Per-lane
   arithmetic: control-sync ≈ 44 MB/s/lane vs overlay ≈ 35 MB/s/lane —
   the overlay pays the device round trip PER SEGMENT, the control per
   BLOCK at worst.
5. What the overlay DID buy, for the record: device-byte economy (amp
   1.008 vs 1.05+), **2.4× flatter tail** (p99 85 ms vs 202 ms — every
   ACK is honest), and exact fabric-queue spread (31–32 qids, max share
   12–14 %). None of it converts to bandwidth while the ACK is coupled.

**Redirect:** the §8 payload-retention accelerator (kernel 0029 draft —
ACK-early with the slot pages retained; the §8 future law for reads of
ACKed-but-incomplete stores) is the named precondition for re-running
this bracket; without it, laws 1+KD-OV-7 bound the armed leg to
lanes × 1 MiB of in-flight custody by construction. That is a KERNEL
track item (owned outside this campaign — `docker/kernel-sqz/`
untouched here). B3+ (read composition, overwrite shapes, B5's gate
row) stay un-built per the falsification-first ladder rule.

## 6. Gates

* Red-first throughout: `a3cc909e` (B1 contracts, compile-red),
  `497ac390` (B2 contracts, compile-red against the missing module +
  metrics); the loom freeze-drain model red against the un-fenced core.
* **×10 blast radius (consecutive, final tip `d719553e`)**:
  {device_overlay (8), overlay_core (13), write_visibility (19),
  write_pipeline (22), write_through_coverage (8), extent_patch (20),
  fsync_writeback_tail_loss (3), publish_coalesce (6)} × 10 = 990 test
  executions, zero failures.
* Both workspaces `clippy --all-targets -- -D warnings` (root also
  `--all-features`) + `fmt --check` clean; fork suite 160; env-knob
  convention (21) + metrics (9) green; loom overlay models green with
  both fences weakening-verified.
* Venue restored: mounts torn down, daemons exited, tcp devsub healthy
  (all namespaces `IN-USE-BY -`), no netem, no killall used.

## 7. Deferrals (KD-OV items not biting B1/B2)

* **KD-OV-12** — the B4 coexistence arm (one pending-binding authority
  for OVERWRITE shapes) is deferred to B4 by the ladder itself; B2
  discharges its face structurally (fresh/hole only, the per-block
  epoch screen, and the pinned zero-shadow-interaction contract).
* **KD-OV-13** — the tier-precedence law bites B3/B4 (old-binding tier
  entries); B2's fresh shape HAS no old-binding tier population.
  Discharged for B2 by the dd-probe screen's DEMOTE arm (a fast path
  that cannot compose hands the op to the handler path, which drains).
* Indirect-mapped layouts decline overlay installs (the inline map is
  the only fresh-block proof B2 reads) — a B3+ item if the ladder
  resumes.
* `unpublished_offsets_recovered` counts the census's gap-completing
  arm only; tail offsets past the last durable reference are
  structurally uncountable (indistinguishable from never-allocated) —
  stated in the field docs and asserted by arithmetic in the
  fault-injection test instead.
* The B2 read-drain posture (reads publish open overlays) is B2-scoped
  by design; §5.2/§5.3 lock-free composition is B3 — un-built under the
  stop.
