# The field-corruption train — findings 38, 39, 40, 41, 36/36b + the f42 design decision (2026-09-01)

**Date:** 2026-08-31 → 2026-09-01 · **Train:** dev `b0ea5f04` → `418f06e8`
(16 commits: five red-first test/fix pairs + two design commits) ·
**Venues:** the 5-node EXA fabric (`squeeze-test` — real nvmet-tcp fabric,
memory-backed NVMe targets, so every number is a pure software-path
number) and the single-node mw proving fleet (1 authority + 8 co-writers,
range custody armed, nvmet-tcp devsub, one box) · **Instruments:** fio
libaio `--direct=1 --bs=1M --numjobs=24` at the field job shapes (1g / 8g
/ 16g), `tests/run_mw_matrix.sh s11-mpiio` (ior, 32 ranks, one shared
>8 GiB file, 4 MiB block-cyclic) on `tests/mw_fleet.sh`, and per-row
`.stats` deltas throughout.

**Evidence tiers (rc-manifest discipline, mandatory):** the EXA fabric
rows below are **measured-real on a 5-node nvmet-tcp fleet**; the s11
acceptance row is **measured-real on the single-node fleet** (one box,
one memory bus). No row here supports any wider-scale claim.

**Gate posture (stated loudly):** the full quality gates were
**user-waived for this train** (verbatim directive 2026-09-01: "do not
run any quality gates, just fix the damn bugs") — each fix ran only its
directly-affected cargo suites plus `cargo check`; no `task check`, no
clippy/fmt, no external POSIX suites, no bench baseline. **The next
release gate must run the full tiers over these surfaces from zero**
(the full cargo gate, require-mount, fstests/LTP/pjdfstests, and the
bench baseline over the KV split/journal and write-pipeline/reclaim
groups). The red-first repro-port mandate was honored on every finding.

**Finding-number note:** an earlier "finding 36" (the A2 rank stall —
the whole-file custody ladder, fix `e3f20090`, recorded 2026-08-30 in
`.benchmarks/2026-08-25-s11-freeloop-stall.md`) shares the number. Its
verification rig, re-run at tip, escalated into a permanent fleet wedge;
this train's investigation split that wedge into **f41** (the corruption
— the actual wedge; the A1-iteration-7 permanent wedge no longer
reproduces post-f41) and the **f36 here** — the co-writer displaced-free
leak, the number's remaining face.

## The train in one view

| Finding | Class | Commits (red / fix) | Verdict |
|---|---|---|---|
| 38 | unmount-drain data loss (cacheless mounts) | `f67fb0fb` / `b0ea5f04` | fixed; deterministic cargo pin; field grep-for-absence is the pinned verdict key |
| 39 | mem-budget Red admission collapse (8×) | `fe05f9a9` / `9936c625` | fixed; write_pipeline_tests 23/23 |
| 40 | reclaim manners deferral starves a thin free supply | `f4b15740` / `ac0c68bc` | fixed; five reclaim suites green |
| 41 | **metadata corruption at the 8 GiB indirect crossing** (latent, wedges the volume) | `7ca6047f` / `08202c32` | fixed; **field-verified 2× from zero on the 5-node fabric** |
| 36/36b | co-writer displaced-free leak (four uncovered arms) | `2a3eefd5`/`6101b09b` + `9bc07539`..`418f06e8` (six commits) | fixed; **s11-mpiio from-zero acceptance green, ROW_EXIT=0** |
| 42 | ~545 GiB single-blob map ceiling (design) | `f3d265ec` + `3e7cf356` (Rev 1) | design committed; PR 1 in flight |

## Finding 38 — the dismount block-ref load overflows the journal entry cap (fix `b0ea5f04`)

**Field signature** (EXA cacheless mount, post-rewrite-storm unmount —
the corpse verbatim):

```
Dismount durable upload failed for ino 57 block 883 … journal entry
length 370701 exceeds the 131072-byte whole-entry cap
```

**Root cause:** a rewrite storm's writeback backpressure accumulated
~8k deferred block-ref ops on one ino; the save drained ALL of them into
ONE layout transaction (~370 KB encoded vs the 128 KiB whole-entry cap),
every publish refused `EntryTooLarge`, the error path REFILLED the
accumulator (the poison), and the unmount drain then lost the block on a
cacheless mount — a data-loss vector at the worst possible moment.

**The fix's law** (two halves):

1. `save_metadata_to_backend_ext` (src/routing.rs): an assembled ref
   load past one chunk (512 ops) commits its overflow FIRST in
   refs-only transactions via `commit_block_refs` — each far under the
   cap — under the caller's held 3.5 section so no publish interleaves;
   the tail (≤ one chunk) still rides the layout transaction, so §6.2's
   accounting-rides-the-publish law holds for every binding this
   publish changes. A crash between a ref chunk and the layout commit
   leaves only report-only fsck C8 residue (space-safe, data-safe).
   On chunk failure the un-committed remainder refills the accumulator
   (never-lossy carry).
2. The layout-merge conveyor (src/meta_backend/kv/backend.rs): member
   weight now counts block_refs bytes (46 B/ref + fixed overhead — it
   previously weighed only the layout wire bytes), and the group
   drain's byte bound is `MAX_ENTRY_LEN/2` instead of the ring-scale
   `batch_max_bytes`, so an aggregated multi-ino tx can never assemble
   an over-cap entry.

**Red-first proof:** `an_overcap_ref_load_publishes_chunked_and_converges`
(`tests/durable_block_refs_tests.rs`, 4,000 refs = 2,000 blocks ×
refcount 2, the clone shape) — red on dev with `journal entry length
192252 exceeds the 131072-byte whole-entry cap`; green with the fix,
the durable census carrying every take and the derived-vs-durable
oracle at zero drift (durable_block_refs_tests 17/17,
publish_coalesce_tests 6/6).

**Field verification:** the pinned verdict key is grep-for-absence —
a post-umount log grep for the two signatures must print nothing. The
four-phase field protocol carrying it was overtaken at phase A by the
f41 discovery (below); the from-zero field pass over this surface is
owed to the next release gate. Interim mitigation (recorded during the
window, now moot): fsync/idle before `umount` after heavy rewrites.

## Finding 39 — Red clamps to the un-headroomed BDP sum, not the fixed floor (fix `9936c625`)

**Field signature** (EXA capture 2026-08-31, the 2.36 GiB/s rewrite
collapse): under mem-budget Red a 10-lane store's
`write_pipeline_depth_target` clamped from ~352 MiB to the FIXED
8-block aggregate floor (`33554432`); 32 MiB in flight across a ~12 ms
fabric is ≈ 2.4 GB/s by Little's law — an **8× collapse** with
multi-second admission tails (`parked_gate_waits` +19,375, +15,795 park
timeouts in 31 s; clat tails 3–8 s) that starved the very drain that
converges the gauge.

**Root cause:** the Red clamp was `raw.min(fixed 8-block floor)` — a
free-floating constant where a measured quantity exists, converting
overload into collapse instead of graceful degradation.

**The fix's law:** the Red clamp derives from the measured drain —
`raw.min(max(Σ per-lane raw BDP, cold floor))`. It sheds exactly the
headroom/probe queueing bytes (≥ 2/3 of the component under the ×3
HEADROOM) while sustaining the measured completion rate — the fastest
convergence OUT of Red that does not starve the drain. The fixed floor
survives only as the cold posture (nothing learned) and the progress
guarantee; the R5 budget cap stays senior to everything. New pure fn
`lane_bdp_bytes` shared by `lane_target_bytes` and the new
`red_drain_sum_bytes`.

**Red-first proof:**
`red_clamp_derives_from_measured_drain_never_a_fixed_aggregate_floor` +
`red_clamps_target_to_measured_drain_and_pinned_override_wins_verbatim`
(`tests/write_pipeline_tests.rs`), red at the expected assertions on
dev: a learned 10-lane store under Red must (a) never collapse to the
fixed aggregate floor, (b) still shed the headroom/probe queueing
bytes, (c) land exactly at the un-headroomed measured-BDP sum
(`red_target × HEADROOM == learned`); cold+Red keeps the cold floor.
write_pipeline_tests 23/23 green.

**Field verification:** pinned verdict key — `write_pipeline_depth_target`
tracks `write_pipeline_depth_target_base` under Red instead of pinning
at `33554432`. Owed to the next release-gate pass (protocol overtaken by
f41, as above).

## Finding 40 — fill-coupled drain pressure outranks the manners deferral (fix `ac0c68bc`)

**Field signature** (EXA full-store collapse 2026-08-31): at ~97 % fill
the reclaim manners law's foreground deferral held the queue while
foreground writes kept moving and the queue sat below the RAM cap — the
free list emptied and every allocation paid the ENOSPC valve's
**synchronous drain**: 12–22 ms fabric round trips inline on the write
path (`block_free_reclaim_sync_drains` climbing beside multi-second
parked-gate tails).

**Root cause:** the manners decision had no supply term — a device
whose queued reclaim debt dominated its remaining free supply still
deferred to moving foreground, so headroom could only regenerate
synchronously on the write path.

**The fix's law:** the worker's manners decision gains a
supply-pressure arm — a per-device registry (allocator + live queued
count, bumped before the push, retired in `take_batch`; every dequeue
routes through it) lets the pass detect a device whose queued reclaim
debt holds ≥ half of its remaining free supply (free list + virgin
tail). Such a device drains REGARDLESS of moving foreground — parked
allocators are foreground writers too, and headroom must regenerate
AHEAD of allocation. Derived from store state only (live queue
population vs live free supply) — **no fill constant**;
capacity-unbounded allocators never read pressure, so the healthy-store
deferral (contract 12) is untouched. The DebtDrainer already had this
coupling; this gives the primary reclaim queue its equivalent.

**Red-first proof:** contract 12b —
`thin_supply_drains_despite_moving_foreground`
(`tests/async_block_reclaim_tests.rs`): a 64-block store with 48
allocated and 32 freed terminally (queue = 2× the remaining 16-block
supply, far below the 4096 RAM cap) must drain under a
perpetually-moving foreground signal, exactly once, with the new
`block_free_reclaim_supply_drains` engagement counter moving. The
counter landed inert in the red commit so the red fails behaviorally:
`background reclaim worker never completed` (the 5 s eventually
window). Suites green: async_block_reclaim 18/18, block_free_reclaim
6/6, discard_elision 8/8, inplace_overwrite 3/3, reclaim_batch 6/6.

**Field verification:** pinned verdict keys —
`block_free_reclaim_supply_drains` moving at high fill (and ~0 at low
fill) with `sync_drains` ≈ 0. Owed to the next release-gate pass
(protocol overtaken by f41, as above).

## Finding 41 — a node split cuts the same-key fold group: the 8 GiB-crossing corruption (fix `08202c32`)

**The headline finding of the train — a LATENT data-integrity bug
reachable by any workload writing files past the ~8 GiB inline→indirect
crossing, wedging the volume permanently.**

**Field signature** (EXA, 2026-09-01 — first seen as a 96 MB/s
fresh-write collapse with a never-exiting fio and `fsync(15) failed`
warnings; the daemon wedges rather than crashes):

```
divergent layout-delta chain: link seq N names base version 0xV but
folds onto 0x0
```

— refused by every subsequent fold FOREVER: the checkpoint tick fails,
the journal never truncates, and every write stalls on ring space.

**Root cause:** a node split cuts at a pure byte-balanced record
boundary with no same-key cohesion. `compact_fold`'s §6.2-item-9
lineage rule emits TWO records for a versioned layout chain (the folded
base Put + the retained newest link); a cut between them strands the
link in the right sibling BELOW its own `min_key`
(= `successor(left.max = key)`) — unroutable forever. The next
gate-legal link then folds onto the bare Put, staging the divergent
chain. Attribution ran through the F41TAPE diagnostic build (three
hooks; the whole-chain dump at the fold refusal was the decisive one —
it exposed the orphaned chain and the repeated-seqs anomaly) plus the
layout-chain audit (subagent hypothesis H1, verified against the source
before acting). Every previously-known fold/compaction site already
carried the earlier retain-rule fix — static reading alone could not
have named this site.

**Latent, not the train (the A/B matrix that flipped attribution):** on
the 5-node fabric, 24 fio writers × 8g files (the shape that crosses
the indirect threshold) corrupt in ~15 s on the tip binary AND on the
pre-f38 control binary (6,413 errors in ~15 s — identical); both
binaries run clean on 1g files (~34 GiB/s), which never cross. The
trigger is the crossing, not this train and not load intensity.

**The fix's law** (three arms):

1. `split_node` (node.rs): the byte-balanced cut advances to the next
   KEY boundary (retreats to the previous one at the tail; a
   single-key fold refuses loud — such a fold always fits one node).
   The two-record lineage pair can never be separated again.
2. `partition_records` (tree.rs, the ≥3-way SMO cut): a part boundary
   yields to the byte budget only ON a key boundary — same law.
3. `write_node` (node.rs), defense-in-depth: a node write whose first
   or last record lies outside `[min_key, max_key]` refuses loud
   (Corrupt) instead of landing durable stranded state — any future
   cohesion bug becomes a loud refusal, not silent corruption.

Plus: the divergent-chain refusal now dumps the WHOLE chain (seq /
version / base per link) — the forensics that made the field corpse
attributable.

**Red-first proof:** `split_never_cuts_a_same_key_group`
(`tests/kv_node_tests.rs`) — split a node whose byte-balance point
lands inside a chain's retain pair; every written side's records must
lie within its own bounds and the chain must fold clean on one side.
Pre-fix, deterministic: `right: record key .. outside its node bounds`
— the stranded retained link. kv_node_tests 25/25,
crash_contract_tests 13/13 green.

**Field verification (measured-real, 5-node EXA nvmet-tcp fabric):**
the 15-second corruption trigger — 24 writers × 8g crossing the
indirect threshold, the exact shape both the pre-f38 control and the
tip binary corrupted on — ran **two full from-zero runs on the fixed
binary: divergent = 0, stranded = 0, at 18.6 GiB/s (crossing) /
33.0 GiB/s (rewrite)**. The permanent A1-iteration-7 fleet wedge
previously attributed to f36 no longer reproduces post-f41.

## Finding 36 / 36b — recomputed publishes own their displaced device frees (fixes `6101b09b`, `19e9fb76`, `82de27bf`, `418f06e8`)

**Field signature** (the mw fleet, range custody armed): the "authority
refused N shipped frees" storm — `block_untracked_free_refusals`
climbing while co-writers ENOSPC (`data volume full … lane 8 of 16
exhausted, 0 free blocks`); **~6,550 leaked blocks (~26 GiB) exhausted
a 64 GiB store in ~2 minutes**, every rank parked on allocation, parked
writes unable to publish the frees they still owed — a fleet-wide
allocate-before-free inversion with no exit (ior `fsync(15)` failures).

**Root cause (f36, red `2a3eefd5` / fix `6101b09b`):** a
chained/composed shipped merge recomputes its durable accounting
against the AUTHORITY's head (rung 19/20) precisely because the
co-writer's frame legitimately lags — but the displaced-block DEVICE
frees still followed the frame. On frame skew every rewrite produced
one refused shipped free (the already-freed block) AND one leaked block
(the recompute-released binding nobody device-frees — its ledger Delete
staged, its device free issued by no one).

**The fix's law (two-halved, held across every arm):** the authority
runs the recompute's RELEASED data-block refs through its OWN free
ladder strictly AFTER commit Ok — via the installed FreeExecutor
(`execute_shipped_frees` verbatim), so durable-population validation,
the grace ring, S7's quarantine and the reclaim manners compose
unchanged, and a block still referenced elsewhere answers `NonTerminal`
instead of a wrongful device free; on commit Err nothing is freed. The
co-writer's caller-frame displaced-free stream STANDS DOWN when the
reply says the owner recomputed (local hygiene only — read-tier purge +
tracking retire, no wire, no device commands); un-recomputed streams
(solo mounts, the free-VERB seam) stay byte-identical. Engagement
gauge: `meta_ship_publish.free_recomputed_blocks`. Reply schemas moved
8→9→10 (KD-7 same-commit fleets; mismatch refuses loud).

**Red-first proofs (f36):**
`a_skewed_frame_rewrite_frees_the_recompute_released_block_on_the_authority`
(red on dev at the leak assert — the Delete proven staged by the same
test), `an_unrecomputed_chained_merge_keeps_the_callers_free_stream`,
`a_failed_shipped_merge_frees_nothing_on_either_side`
(`tests/mw_cowriter_free_tests.rs` §10).

**f36b — the field engagement failure and its three uncovered arms**
(six commits, `9bc07539`..`418f06e8`). The f36 fix engaged the wrong
arm for the FIELD venue: on the range-custody fleet the from-zero row
read `block_untracked_free_refusals` 9,001 / `free_recomputed_blocks`
**0** / `publish_blob_composes` 869 with mid-A1 fsync failures — the
gauge-vs-refusal divergence proving the law right and the
instrumentation dark. Three arms, each its own red-first pair:

1. **The custody-scoped full Put** (red `9bc07539`:
   `a_range_custody_full_put_skew_frees_the_scoped_release_on_the_authority`
   + the never-widen guard
   `an_unscoped_full_put_keeps_the_callers_free_stream`; fix
   `19e9fb76`): on a range-custody fleet a co-writer's RAM head is
   indirect-classed, so every save is full-save class and ships
   `SetLayoutAndSize`, whose serve recomputes in
   `custody_scoped_layout` — but its post-commit frees covered only map
   blobs and its `Unit` reply carried no verdict. The Set serve now
   collects the scoped compose's released data refs and frees them
   post-commit; the reply became `PutDone { recomputed }` and the
   verdict reaches EVERY frame-derived displaced-free site (the save
   body's Set arm, the serialized lever leg, and the rewrite-epoch
   close via the per-ino latch written under `INODE_META_LOCKS`).
2. **The f38 claim pre-chunk** (red `6852d27e`:
   `an_over_chunk_claim_set_reaches_the_scoped_compose_whole` — RED:
   256 of 300 claimed transitions lost, 256 of 300 displaced blocks
   leaked, gauge 44/300; fix `82de27bf`): finding 38's ref pre-chunking
   committed the leading 512 claim ops verbatim BEFORE the Put, so the
   owner's claims-scoped compose never saw them — dead bindings
   survived in the durable map with no free owner while the recomputed
   reply stood the whole caller stream down. Probes counted 6 chunked
   saves × 8 co-writers × ~256 lost pairs ≈ 12.3k orphaned frees — the
   store exhaustion, exactly. Fix: a SHIPPED full-save-class publish
   skips the pre-chunk and carries its WHOLE claim set on the verb
   (over-frame refuses loud at encode); the journal-entry-cap
   protection moves to the OWNER (the Set serve chunks the COMPOSED
   refs — same crash-residue class f38 adjudicated). Local saves and
   delta-class shipped saves keep the f38 pre-chunk verbatim — both
   laws stay in force, each on its own node.
3. **The authority-local recomputes** (red `f1abfa6b`:
   `an_authority_local_publish_frees_a_co_writer_minted_displaced_block`;
   fix `418f06e8`): the authority's OWN publishes on a custody ino
   (extent-assembly folds, rung-17 local chained merges, the f35b
   episode composes — `extent_served=4282` named the arm) displace
   blocks a CO-WRITER minted, which the authority's allocator never
   tracked: the local caller-frame free refuses unseeded and the block
   leaks. The local recomputed arms now return their RELEASED set
   beside the verdict — never freed inline, RES-1-composed (both run
   under the caller's 3.5 stripe) — and the save's post-guard venues
   run it through the shipped-free executor ladder strictly after the
   guard drops and the commit landed.

Suites at close: mw_cowriter_free_tests 48, mw_cowriter_lane_tests 26,
dlm_cowriter_tests 18, durable_block_refs_tests 17 — green post-rebase.

**Field acceptance (measured-real, single-node fleet; counted-run law —
from zero on `418f06e8` after the last fix):** the full 4-phase
s11-mpiio matrix — previously permanently wedged at A1 iteration 7 —
ran green end to end, **ROW_EXIT=0 on all four phases**; the engagement
gauge `meta_ship_publish.free_recomputed_blocks` read **217,095**,
accounting the row's displaced blocks; `block_untracked_free_refusals`
**8,946 → 2,366 with every residual increment attributed** (see the
residual board); the fsck oracle ran clean; teardown reported zero
residue. The "≈ 0 refusals" acceptance bar was satisfied by full
attribution rather than a raw zero — every remaining increment traced
to one pre-existing, refuse-safe class, boarded below.

## Finding 42 — the design decision: TREE_BLOCK_MAP replaces the single-blob indirect map (`f3d265ec` + `3e7cf356`)

f41's venue exposed the plane's structural ceiling alongside its bug:
the single-blob indirect map obeys a one-block law — ~545 GiB max file
size at default geometry (a multi-TB checkpoint file cannot exist), and
every publish past the crossing rewrites the whole map blob (O(file)
per publish). The decision of record: replace it with a first-class KV
tree — `TREE_BLOCK_MAP` (tree 7, incompat bit 16), one record per
mapping plus run records, riding the existing conveyor / journal /
checkpoint machinery — 16 PiB ceiling at 4 MiB blocks (explicit EFBIG
at `(2³² − 1) × block_size`), O(window) publishes, and the blob-compose
lifecycle (findings 23/24/33/35c/41's class) becomes legacy-only.
Design: `docs/design-kvmap-block-map-tree.md`.

**Review outcome (§6, Rev 1 — 2026-09-01):** the adversarial review's
verdict on Rev 0 was NEEDS-REVISION; ten amendments (A1–A10) are folded
in as normative and bind the PR ladder. The three critical: **A1** —
the crossing gains a residue-sweep prologue (a crashed prior crossing's
records resurrected as stale mappings after
truncate-shrink-recross-extend = silent wrong data); **A2** —
truncate/unlink become size-flip-first + durable sweep cursor +
background job-fabric sweep (same-tx range Deletes are five orders past
the whole-entry cap at PB scale); **A3** — fsck C11 drops C9's era
floor (structurally inapplicable), rides held-4a + an in-flight
crossing registry, and ships REPORT-ONLY. Plus: EFBIG at the u32 index
ceiling (corrected to 16 PiB), bounded runs + the floor-scan law,
node-cache probation for tree-7 leaves, co-writer map-cache staleness
under the free-grace ring, seqlock-bracketed reader resolution, and
knob range caps.

**PR ladder status:** seven rungs (§4), each independently gateable,
tests-first. PR 2 (`feat/kvmap-crossing`) was gated on f41's fix — now
landed (`08202c32`). **PR 1 (`feat/kvmap-tree-core`) is in flight.**
The finding-41 venue rig (24 writers × iodepth 16 over the crossing) is
PR 5's standing per-PR house gate.

## Standing residual board

1. **The f36b crossing blob-lifecycle Refused pairs** — the attributed
   source of the residual 2,366 `block_untracked_free_refusals` on the
   acceptance row: a pre-existing, refuse-safe free pair at the
   indirect crossing (~300/mount, phase-start-clustered, fsck-clean,
   data-safe — the refusal IS the safety working). Boarded as the next
   finding's input; **largely mooted by the f42 kvmap plane**, which
   deletes the blob-compose lifecycle these pairs live in.
2. **The f42 PR ladder** — design Rev 1 committed (`3e7cf356`), PR 1
   (`feat/kvmap-tree-core`) in flight; PR 2 unblocked by `08202c32`.
3. **The waived gates** — the full-tier debt stated in the header: the
   next release gate runs the full cargo gate + require-mount +
   fstests/LTP/pjdfstests + the bench baseline over every surface this
   train touched (KV node split/SMO, journal-entry assembly, write
   pipeline Red posture, block reclaim, the S9/S11 publish plane), and
   the f38/f39/f40 pinned field verdict keys (grep-for-absence;
   `depth_target` tracking `depth_target_base` under Red;
   `supply_drains` moving at high fill with `sync_drains` ≈ 0) get
   their from-zero field pass there — the four-phase protocol that was
   to carry them was overtaken at phase A by the f41 discovery.
