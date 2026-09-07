# 2026-09-07 — finding 51: the authority's `read_settle_lost_serialized` storm is its own retired word for a recycled co-writer block

| | |
|---|---|
| **Branch** | `fix/read-settle-lost-serialized-authority` off `dev` 2a486273 |
| **Commits** | `bf826a29` (red contract) · `10388c45` (fix) · `d3cb44ac` (docs) — on `dev`; **§8 (phase B1)**: `fix/finding-51-adopt-key-incarnation` off `dev` a7cab076 — `ca8c287d` (red contracts + gauges) · `fc561927` (fix) · docs commit |
| **Evidence** | `/tmp/five/d4/keep-t1-all4/` (m0 + m50–m57 `.log` / `.stats.json`, `rows/s11mpiio-1788742525/A1.out`) and the baseline row `/tmp/five/d4/keep-t1-baseline/`; `tests/run_mw_matrix.sh s11-mpiio` on the 1 + 8 range-custody fleet, tcp devsub |
| **Class** | data-path correctness — a structural validation failure on the authority for every co-writer block that has been through the authority's free ladder once: the authority cannot read or fold a RECYCLED co-writer block (EIO), which under S11 is every co-writer's `fsync(2)` that holds a retained extent |
| **Fleet row** | A1 **PASSED** on `dev` (day-2 row B: `read_settle_lost_serialized` 0, `served_binding_witnesses` 101,299, fsync EIOs 0, the S11 gate met for the first time); **B1 exposed §8 — its re-run is OWED (parent)** |

## 1. What the evidence says (read before any code)

Two rows (this session's `keep-t1-all4` and the `keep-t1-baseline` before
it), same shape: the authority m0 burns the must-stay-0 RES-22 tripwire
`read_settle_lost_serialized` while running **no** application I/O
(`fuse_ops` 77 / 78 for the whole row), always on block 1290 of inode 2,
the key changing every ior iteration.

| question | answer | source |
|---|---|---|
| who reads block 1290 on the authority? | the **`FlushExtents` executor** — `SqueezefsFilesystem::flush_shipped_extents(2)` → `flush_inode_to_backend` → the fold of the shipped extents parked for that block → `fetch_seed_image` → `get_block_for_index_stripe_held` → the ladder → the caller-stripe settle arm. `seed_settle_escalations` 101 (all4) / 131 (baseline); `stale_binding_escalations` (the pure-read arm) **0** | m0.stats |
| does the ledger close? | **exactly**: 101 escalations × 4 settle attempts = 404 losses = 78 `read_settle_stale_head_refetches` (cache-head, the legal finding-25 race) + **326 tripwires**; 101 × 24 = 2,424 `stale_binding_rebinds`. Baseline: 131 × 4 = 524 = 104 + 420; 131 × 24 = 3,144. **Every attempt lost; nothing ever served** | m0.stats, both rows |
| who called FlushExtents, how often? | m52 / m53 / m55 (the three co-writers holding retained extents: `extent_retained_bytes` 409,600 / 405,504 / 843,776) — `FUSE Fsync: FlushExtents barrier for ino 2 failed: … block 1290 of inode_2 did not settle after 4 serialized stripe-held settle attempts` **39 + 34 + 28 = 101** times, once per rank per iteration (4 ranks/mount, ≈ 1 s apart — the fsyncs serialize behind each other's `flush_inode_to_backend`; the "1 Hz" is that plus the tripwire's 1 s/site log rate-limit) | m52/m53/m55.log |
| why block 1290 for all three? | `flush_inode_to_backend(2)` is one pass over EVERY parked extent of ino 2 — the block-1290 fold dies first and the whole force returns EIO. m52 is the retainer of the block-1290 extent (409,600 B retained = the authority's `parked_extent_bytes` 409,600); block 1290 is rank 10's (1290 % 32) and m52 hosts ranks 8–11 | m0/m52.stats, `A1.dispatch.sh` |
| what did the application see? | ior `WARNING: fsync(15) failed` — the 101 FlushExtents EIOs (+ 18 `StorageFull` fsync errors at the row's end, finding 15's supply, not this finding) — `inconsistent file size by different tasks` (10,737,418,240 expected / 10,619,977,728 stat'd = 112 MiB), and phase A1 **NOT SUSTAINED** (1,649 → 649 MiB/s) | `rows/*/A1.out`, `matrix.log` |
| whose lane are the tripwired offsets in? | **lane 2 — all 13 distinct offsets** (626, 1218, 3154, 3202, 3346, 3570, 4370, 4562, 4610, 5458, 6578, 7122 on the two data volumes, plus idx 2) = m52's lane (`alloc_lane_id` 2). The authority is lane 0: **it never minted one of these** | m0.log offsets ÷ 4 MiB mod 16; m5x.stats |
| what state does the authority hold for them? | `live_incarnation 0, fill(word) None, refcount None, free_listed false, inflight false, quarantined false, key_names 0` — **an incarnation word EXISTS and is UNSTABLE** (`fill_incarnation` answers `Some(UNKNOWN_STABLE)` when no word exists; `None` is a present word with `stable = 0`) | the tripwire line's `binding_move_diagnosis` |
| who creates an unstable word on the authority for an offset it never minted? | only `mark_incarnation_unstable` — reached from `begin_free` (the shipped/recomputed free ladder the authority runs FOR its co-writers: `free_recomputed_blocks` 30,865 + `free_served_blocks` 3,697 this row), the W1 patch fence (`patch_writes` 0), or `retire_shipped_free_tracking` (co-writer only). `publish_block` / `seed_incarnation` create STABLE words | `src/block_allocator.rs` |
| did those offsets go back to the co-writer? | `harvest_served_blocks` 31,280 — the authority handed ≈ every freed lane block back to its lane; ior rewrites every block every iteration, so from iteration 2 on the co-writers write on RECYCLED offsets | m0.stats |
| a mutator racing the settle? | **No.** The head was backend-fresh under (3) + (3.5) + the serve stripe and named the key; the word was already unstable before the window opened and stayed so after — nothing moved during the window. The loss is the word's steady state | the ledger's 100 % loss rate; §2 |

## 2. The mechanism (`src/block_allocator.rs`, `src/cowriter.rs::execute_shipped_frees`, `src/routing.rs::settled_resolve_fetch_locked`)

The incarnation seqlock is **per process**. A co-writer's rewrite of block
`b` runs, across the two nodes:

1. co-writer: `claim_block_idx(X′)` (its word for X′ retired) → DMA →
   `publish_block(X′)` (its word stable) → the layout publish naming
   `b → X′` SHIPS; the authority serves it (the custody-scoped compose)
   and recomputes the displaced set: **X is freed on the authority**
   (`free_recomputed_releases` → `begin_free(X)` → the authority's word
   for X **retired**, gen+1 unstable) → grace ring → free list.
2. the co-writer's lane runs dry → `HarvestLaneFree` → the authority hands
   X back (`claim_for_lane_harvest` removes it from its list; its word
   stays retired — correct: X's next content is mid-DMA on the co-writer).
3. co-writer: `claim_block_idx(X)` → DMA → `publish_block(X)` — **the
   co-writer's word**. The layout publish `b → X` ships; the authority
   serves it, its map now names X. **Nothing on the authority publishes
   its word for X**: the authority never claims a foreign-lane offset, so
   `mark_incarnation_unstable`'s retire from step 1 has no matching
   `publish_block`, ever.
4. every authority fill of X now fails: `fetch_block_device_true` →
   `fill_incarnation(X)` = `None` → `serve_valid = false`. The ladder
   burns 24 rebinds (`fill_valid=false` on an UNCHANGED binding — the
   "contention" face, except nothing is contending), hands off to the
   settle arm, which resolves the backend-fresh head under both locks,
   fetches X, loses the same verdict 4 times, and returns *"did not settle
   after 4 serialized stripe-held settle attempts"*. The FlushExtents
   force propagates it; the co-writer's fsync returns EIO.

Finding 30 met the mirror image on the co-writer (`retire_shipped_free_
tracking`'s orphaned-unstable word for an offset the AUTHORITY re-mints)
and closed it locally with retire+publish under a new generation. The
authority's face was left open because the authority has a real witness
the co-writer lacks — the served publish — and needs it: publishing at
the harvest handout would leave the window between the handout and the
co-writer's DMA-complete publish unprotected (a straggler fill of the
dead binding could publish mid-DMA bytes into the authority's tiers
under X — the ABA the seqlock exists for), and a never-freed FRESH mint
has the same window today only because no stale binding to a
never-bound key can exist.

## 3. The fix — the served publish is the authority's DMA witness

A co-writer publishes strictly after its DMA (the write pipeline's
allocate → dma → publish law; the ACK-early overlay releases its permit at
the CQE before the coverage publish). So the serve that ADOPTS key X into
a durable head is the authority's proof that X's device content is
final, and it plays the role `publish_block` plays for a local write:

* `BlockAllocator::witness_served_binding(block_idx)` — publishes the word
  iff the offset is in a **foreign** lane (`!lane_is_ours`); own-lane
  words belong to the local claim → DMA → publish protocol (a peer's take
  on one is a clone of an already-stable block or a stale view the
  compose dropped), and an unpartitioned allocator owns every lane, so a
  solo mount never publishes here.
* `BackendRouter::witness_served_bindings(&[BlockRef])` — routes each
  taken reference to its volume's allocator (`vol_tag`, KD-5), counts on
  **`served_binding_witnesses`** (stats inode).
* `meta_ship::publish::install_binding_witness` — the owner-side hook,
  installed by the authority arm **beside the free executor** (the
  executor retires a displaced offset's word, the witness re-publishes it
  when a peer's publish adopts the offset again — one lifetime, both
  halves on the authority's data plane; uninstalled with it on disarm
  and on the lane-engage error arm). Fired **strictly after commit Ok**
  with the commit's TAKEN data references (`taken && !is_map_blob`) on
  every served layout-class arm: `SetLayoutAndSize` (the prepare's final
  refs — recomputed from the head→composed diff on a production
  authority — carried on `LayoutPostCommit::taken_data` to
  `finish_layout_publish`; the kvmap scoped-put `Done` arm), the chained
  `MergeLayoutAndSize`, `CommitBlockRefs`, and the `MigrateBlockMap`
  train. A refused or failed serve adopted nothing and witnesses nothing.
  The authority's own `WriteExtent` assembly writes ride the local
  protocol and are untouched.

Ordering: the settle arm holds (3) → (3.5) → the per-ino serve stripe
(finding 25 rung A); the served commit and its post-commit witness run
under the same serve stripe (`run_layout_group` holds the stripe guards
across prepare + commit + finish; the serial `serve_layout_publish` holds
`_ino_guard` across `execute`). A settle that sees X in a backend-fresh
head therefore sees X's word published. The un-striped ladder may read a
head naming X a few µs before the witness runs — one rebind, then valid.

What stays: between the authority's `begin_free(X)` and the re-adopting
serve, X's word is RETIRED (pinned) — the straggler-fill protection is
not traded away. `read_settle_lost_serialized` keeps its must-stay-0
meaning for the class that remains: a retire/claim of a CURRENTLY-BOUND
key outside (3)/(3.5)/the serve stripe (the comment at the emission site
names the narrowing).

## 4. The repro (`tests/mw_authority_recycled_binding_tests.rs`)

The `mw_cowriter_free_leak_tests` rig (authority with executors + probe
+ resolver + geometry; co-writer with the production FUSE write path under
range custody; one process, two data planes, one KV backend) plus an
AUTHORITY-side reader router over the authority's own allocator.

* `an_authority_read_of_a_recycled_co_writer_block_validates_first_try` —
  10 rounds of the co-writer rewriting blocks 2..6 of an 8-block shared
  file on a store sized so rounds ≥ 4 run on HARVESTED offsets; after
  every round the authority reads the range (pure-read posture,
  `device_true` so the fixture router's tiers stay out of the verdict —
  the same `fetch_block_device_true` verdict and `settled_resolve_fetch_
  locked` interior the fold's stripe-held seed fetch runs). Contracts:
  the round's bytes on the first ladder attempt (`invariant_tripwires`,
  `stale_binding_escalations`, `read_settle_stale_head_refetches`,
  `stale_binding_rebinds` all unchanged); every named foreign-lane key
  STABLE on the authority; every displaced lifetime the authority freed
  UNSTABLE until re-adopted; a default-path read on a fresh reader at the
  end; `served_binding_witnesses` = rounds × 4 exactly.
  **RED on dev 2a486273** at round 4, block 3, idx 19 (`recycled = true`):
  `block 3 of inode_2 did not settle after 4 serialized settle attempts`,
  ledger delta tripwires +3, stale-head refetches +1 (= 4 losses),
  escalations +1, rebinds +8. Rounds 0–3 (fresh mints, no word →
  `UNKNOWN_STABLE`) passed — exactly the fleet's "iteration 1 fine,
  every later iteration EIO" shape. **GREEN with the fix**: 23
  recycled-block serves, 21 offsets freed and re-adopted, 40 witnesses.
* `the_binding_witness_never_publishes_an_own_lane_or_solo_word` — the
  two edges: a solo allocator publishes nothing; a partitioned authority's
  own-lane retired word stays retired; a foreign-lane never-seen offset
  gets its first stable word; the authority's `begin_free` retires it; the
  next witness re-publishes it; the gauge counts the two foreign publishes
  and neither own-lane refusal.

What the seed path adds over the pure-read path is only the lock arm
((3.5) under a caller-held (3) instead of (3) → (3.5)); the verdict and
the settle interior are shared, which is why the fleet's `seed_settle_
escalations` and the repro's `stale_binding_escalations` name the same
loss. The co-writer's FlushExtents force itself is not driven in-process
here (the sub-block share that produced the retained extent is a
separate S11 shape; `mw_authority_assembler_tests` owns that rig).

## 5. What the application would have seen, before and after

Before: on a range-custody fleet, `fsync(2)` on any co-writer holding a
retained extent for a block whose current offset the authority has
recycled → **EIO** (ior: `WARNING: fsync(15) failed`, 101×; a
POSIX application treats it as data-durability failure). Any read of the
shared file ON THE AUTHORITY (`cat`, a checkpoint verifier, fsck's C7
scrub reading through the router) → EIO for every recycled block after
the first rewrite iteration. The authority's log: 1 tripwire line/s, the
`invariant_tripwires` gauge climbing ≈ 3 per failed fsync.

After: the fsync's FlushExtents force folds and returns the covering
version; the authority serves recycled blocks on the first attempt;
`invariant_tripwires` stays flat; `served_binding_witnesses` grows with
the co-writers' inserted blocks.

## 6. Suites

Targeted (this worktree, `--test-threads=1`): the new file (2/2), and the
green re-runs listed in the report — `mw_cowriter_free_tests` 50,
`mw_cowriter_free_leak_tests` 9, `mw_cowriter_lane_tests` 26,
`dlm_cowriter_tests` 18, `dlm_multi_writer_tests` 16,
`mw_authority_assembler_tests` 21, `rebind_starvation_tests` 5,
`mw_widthn_refs_tests` 15, `mw_block_key_incarnation_tests` 12,
`pv_shipped_free_ledger_tests` 2, `pv_partial_arm_tests` 10,
`mw_ranged_lease_ladder_tests` 15, `mw_arbiter_fold_tests` 3,
`publish_plane_batching_tests` 8, plus the second batch in the report.
`cargo fmt --check` and `cargo clippy --all-targets --all-features -D
warnings` clean. The full `task check` and the fleet re-run are the
parent's.

## 7. NOT claimed

* **The fleet row is owed.** This note proves the mechanism from the
  row's own ledger (the exact closure in §1) and reproduces + closes it
  in-process; the s11-mpiio re-run (tripwires 0 on m0,
  `FlushExtents barrier … failed` 0 on m5x, `served_binding_witnesses` ≈
  the co-writers' inserted blocks, the A1 sustained gate) is the parent's.
* **Why m52/m53/m55 held sub-block extents on a 4 MiB-aligned row** (the
  S11 range-share that routed a slice of a whole-block write through
  `ship_extent`) is not diagnosed here; the retained extents are the
  TRIGGER of the authority's fold, not the cause of its loss. Without them
  the same words would still be retired and every authority READ of the
  file would still EIO.
* The 112 MiB aggregate-size shortfall and the 18 `StorageFull` fsync
  errors are finding 15's supply terms, not this finding's.
* No claim about the co-writer's own tiers: finding 30's local
  retire+publish stands as shipped.

## 8. Regression found by phase B1 — a served publish displacing an OPEN authority overlay record's captured old binding

### 8.1 What B1 saw

The fix above let the s11-mpiio row pass A1 for the first time, so the
matrix ran **B1** — the `-F` file-per-proc reference (32 ranks × 320 MiB
files, same fleet) — for the first time ever (row A, pre-fix, died at A1
and never reached it). B1 collapsed 770 → 148 MiB/s from iteration 7 on
(`rows/*/B1.out`: 1,562 / 899 / 658 / 262 / 1,264 / 1,250 / 1,279 then
157 / 148 / 157 / 152 / 148 / 148 / 145 MiB/s), while the authority
logged **28,039 `STALE BLOCK-KEY BINDING refused`** lines
(`block_key_incarnation_refusals` 28,015) between 14:35:08 and 14:43:22
(`/tmp/five/d4/keep-day2-B/m0.log`). The working hypothesis handed to this
branch was that §3's witness re-stabilizes a seqlock word whose
incarnation NUMBER is the authority's, so every later validation of the
co-writer's key is refused as stale. **The code and the log refute it:**

| question | answer | source |
|---|---|---|
| does the witness touch the number `incarnation_ok` compares? | **No.** `IncarnationCell` has two fields — the seqlock `word` and the lifetime `stamp`. `witness_served_binding` → `publish_block` sets the word's stable bit (`incarnation_core::publish`); `live_incarnation` reads the STAMP, which only `claim_block_idx` (own mints) and `seed_incarnation` (the mount walk) ever write. For a foreign-lane offset on a fresh fleet the stamp is `INCARNATION_NONE` before and after the witness — "unknown, accept" | `src/block_allocator.rs` |
| whose offsets were refused? | **10 distinct keys / 10 offsets, ALL lane 0** — the authority's own mints, which `witness_served_binding` never reaches (`lane_is_ours` returns before the publish). Every line: live stamp > key stamp, same era (the offset was freed and re-minted by the authority itself) | m0.log, `off ÷ 4 MiB mod 16` |
| who validated them 28,039 times? | **the device-overlay settle.** `write-arm settle for ino 23223 block 11 failed attempt 1 … 8192` (and 89203:22, 166896:15, 101360:30 — 4 records at power-of-two cadence; 10 records in all): `settle_overlay_block_write_arm` retrying a DEAD captured old binding every 50 ms for nine minutes, plus the read-venue settle the co-writers' `FlushExtents` forces ran (`FUSE Fsync: FlushExtents barrier for ino N failed: … names a dead incarnation` ×37 — EIO after 24 attempts) | m0.log, m5x.log |
| was it ever a regression? | the phase never ran before this fix; row A logged 0 because it died at A1. B's own A1 phase logged 0. **First exposure, not regression** | `keep-day2-A/matrix.log`, `keep-day2-B/matrix.log` |
| a re-stated same-key publish refused? | **No** — pinned green from birth in `an_authority_read_of_a_recycled_co_writer_block_validates_first_try` (a full Put re-stating the unchanged map is served with `block_key_incarnation_refusals` Δ0) | the test |

### 8.2 The mechanism (`src/fuse_client.rs::settle_overlay_block_locked`, `src/meta_ship/publish.rs` owner side, design-overlay-overwrite §5.7)

design-overlay-overwrite §5.7 states the law: *"a durable map mutation
of an overlaid index by a path that does not run the settle screen would
strand `old_binding` pointing at a key the durable path displaced and
will free"* — the KD-1.11 resurrection class — and wires the one-authority
screen into the ROUTING primitive's op arms (`overlay_screen_merge`:
foreign `Merge` on an open-overlay index ⇒ supersede). A **served
publish never runs that primitive**: the owner-side compose commits on the
KV backend directly (rung 18's own words) and only invalidates the
authority fs's RAM head afterwards. So, on B1:

1. the authority's assembler holds an OPEN overwrite record on
   `(ino 23223, block 11)` — a shipped slice (m54, lane 7, shipped 912
   extents in this phase while its lane sat at `StorageFull`) — whose
   captured `old_binding` is the authority's own stamped lane-0 block
   `8925478912@163bqi69j`;
2. the co-writer's whole-block publish of block 11 is SERVED; the
   custody-scoped compose adopts its key and RECOMPUTES the displaced set;
   `free_recomputed_releases` → the executor → `begin_free(8925478912)`
   (RAM-tracked, population 0 after the commit — the finding-23 shield
   passes) → the offset returns to the lane-0 free list;
3. the authority's next mint (another overlay dest) re-mints it — live
   stamp `…5115`, the record's key still `…4919`;
4. every settle of the record reads its old image through the captured
   key: `read_nvme_block_old_image` → `incarnation_ok` → **refused**. The
   write-arm form (`settle_overlay_block_write_arm`, entered by any write
   to a block with a live overlay — a new shipped slice for block 11)
   retries FOREVER under the held block guard at 50 ms — 20 refusals/s per
   record, the `WriteExtent` serve never returns, the co-writer's
   `write(2)` hangs; the read-venue form (the `FlushExtents` force)
   exhausts 24 attempts into fsync EIO.

Why B1 and not A1: in the shared-file phase the authority's assembly of a
block and the co-writer's whole-block write of the SAME block do not meet
(one rank owns each block); in file-per-proc they do — a holder's shipped
slices and its own local whole-block writes of one file interleave (the
S11 grant shape under a clamped range — `range_custody_stretch_ceiling_
clamps` — is the residual board's item, not this note's).

### 8.3 The fix — the served-publish half of the §5.7 screen, plus a dead-capture belt

* **The screen** (`publish::install_served_displacement_sink`, installed
  by the authority fs beside the rung-18 invalidation in
  `install_extent_assembler`, uninstalled with it on disarm): every
  served layout-class commit hands the indices it DISPLACED (the final
  refs frame's releases — the recompute's on a scoped/chained arm, the
  caller's on a verbatim arm — plus the recompute's released set) to the
  sink strictly after commit Ok and BEFORE the recompute's frees. The sink
  invalidates the fs's RAM head first (an install racing the commit
  refetches the durable head instead of capturing the displaced key), then
  `DataRouter::overlay_supersede_served_displaced` applies §5.7's
  `Merge`-class containment through the same probe/supersede/retire hooks
  the primitive uses: the record is superseded and retired detached. A
  LEGAL peer publisher, so no `overlay_foreign_merge` tripwire; counted on
  **`overlay_superseded_by_served_publish`** with a loud line naming the
  S11 shape.
* **The belt** (`settle_overlay_block_locked`): before reading a record's
  captured old image, `block_key_incarnation_ok(old_key)`; a DEAD
  lifetime means a durable publish displaced the block past the record —
  supersede + teardown, `Ok(false)`, counted on
  **`overlay_superseded_dead_old_binding`**. The one refusal it costs is
  the detection (0 while the screen catches every displacement); the
  write arm converges in one attempt instead of never.

Both follow §5.7's own disposition for a foreign `Merge` ("the durable map
is the authority the moment the foreign merge commits; keeping the record
alive would serve dest bytes the durable authority displaced"). The
rejected alternative — re-capturing the record onto the block's CURRENT
binding and settling over it — would apply the record's slice over the
displacing whole block: right when the slice is the newer write, wrong
when the whole block is (the same write(2) shape — a kernel-split 4 MiB
write whose first segment shipped — is the ambiguous one), and §5.7's
no-revalidation-word argument depends on the capture staying immutable.
Which of the two writes is newer is the S11 holder's knowledge, not the
authority's — the design question the residual board owns.

### 8.4 The repro (`tests/mw_authority_recycled_binding_tests.rs`, `displaced_overlay`)

The scenario: an authority whose keys are STAMPED (`engage_stamped_keys`
— the fleet's `set_meta_backend` act), a seeded striped file, an OPEN
overwrite record on block 3 through the B4 seam (captured old binding =
the seed's stamped lane-0 key), a co-writer's served whole-block write of
block 3 (its fs write + fsync), the recompute's free, the authority's
re-mint of the offset (mint until the victim offset comes round), the
premise probe `!block_key_incarnation_ok(old_key)`, then the co-writer
side torn down.

* `a_served_publish_displacing_an_open_overlays_old_binding_supersedes_it_at_the_commit`
  (assembler installed): a gap read on the authority serves the
  co-writer's bytes, the fsync settles nothing, `overlay_open` converges,
  `block_key_incarnation_refusals` Δ0, `invariant_tripwires` Δ0, the
  screen gauge Δ1, the belt gauge Δ0, the whole block reads as the
  durable authority's. **RED on a7cab076: the gap read EIOs** (`Errno(5)`).
* `a_settle_whose_captured_old_binding_died_supersedes_instead_of_retrying_the_dead_key`
  (no sinks installed — the escape shape): the record survives the served
  publish (premise), the fsync returns, the belt gauge Δ1, exactly ONE
  refusal, tripwires Δ0, the block reads as the durable authority's.
  **RED on a7cab076: the fsync EIOs** after the 24-attempt settle.

Both red runs were taken one test per process: in the red state a
panicked scenario leaves its authority un-stopped and its ino-2 range
grants in the process-global lock map, which made the FOLLOWING tests in
the same process fail on unrelated writes — the pollution is a property
of the red, and the whole file runs green together on the fix (4/4).

### 8.5 What B1's application saw, before and after

Before: from iteration 7 the co-writers' `write(2)`s on blocks whose
served publish had displaced an open authority overlay record HUNG
behind the authority's write-arm settle (the `WriteExtent` serve parked on
the held block guard, never returning), and every `fsync(2)` of those
inos returned **EIO** (`FlushExtents barrier … names a dead incarnation`,
37×); aggregate ingest 770 → 148 MiB/s; the authority's log 20 refusal
lines/s per wedged record. After: the served commit supersedes the
record — the slice's bytes yield to the durable whole block (§5.7's
newest-wins for a foreign `Merge`), the co-writer's retention releases at
its next force (the head version covers), no settle ever reads a dead
key, writes and fsyncs return.

### 8.6 NOT claimed

* **The B1 re-run is owed (parent):** `block_key_incarnation_refusals`
  0 on m0, `overlay_superseded_by_served_publish` > 0 (the shape is real
  in file-per-proc), `overlay_superseded_dead_old_binding` 0 (the screen
  catches every displacement), no `write-arm settle … failed` lines, the
  B1 sustained gate.
* **The S11 straddle** — why a file-per-proc holder ships slices of a
  block it also writes whole (the clamped-grant share on a single-writer
  file, `range_custody_stretch_ceiling_clamps`), and which of the two
  writes is newer — is not adjudicated here; the containment follows
  §5.7's stated disposition and the ordering question goes to the
  residual board.
* **Adopting the key's stamp at the witness** (the hypothesis's remedy) is
  NOT implemented: it fixes nothing in B1 (lane-0 offsets), and done
  naively it would make the finding-28 probe (`retain_live_bindings` →
  `block_key_incarnation_ok`) DROP every recycled co-writer key as "dead"
  at the compose (a recorded older stamp vs the re-minted key's newer
  one), which is a live-binding loss. The §6.3 gap it would close on a
  REMOUNTED authority — `seed_incarnation` at the mount walk records a
  co-writer's stamp for a foreign-lane offset, and the co-writer's later
  re-mint then reads as dead — is real and named for the board; a lane-
  aware probe semantics is the prerequisite, not this branch's change.
* The 144 `begin_free REFUSED untracked offset` lines and the m54 lane
  `StorageFull` in B1 are finding 15's supply terms.
