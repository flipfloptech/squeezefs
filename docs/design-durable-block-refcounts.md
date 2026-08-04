# Durable block refcounts and free list

**Normative design for pre-RC engineering spec §6.2 item 1** — "the largest
item". Ruling **D9**: the format change lands behind an incompat bit that is
**built but not stamped** on existing volumes; it stamps with the batched
Phase-8 reformat window.

Status: implemented on `feat/mw-durable-refcounts`. Contracts pinned by
`tests/durable_block_refs_tests.rs`; costs measured by the `block_refs`
Criterion group in `benches/meta_lv_bench.rs`.

---

## 1. The problem, in source

`BlockAllocator` (`src/block_allocator.rs`) holds two structures that decide
block ownership:

| Structure | Field | Durable? |
|---|---|---|
| refcounts | `refcounts: scc::HashMap<u64, AtomicU32>` | **no** |
| free list | `free_blocks: dashmap::DashSet<u64>` | **no** |
| allocation cursor | `highest_block: AtomicU64` | **no** |

All three were rebuilt at every mount by `recover_active_blocks_v3`: walk the
live inode tree, read each ino's `layout` xattr, and `recover_block()` every
offset it names. The pre-fix comment on `fsck_reconcile_accounting` states the
posture outright — *"both are mount-session RAM, rebuilt at mount — pure
derived state."*

§6.2's verdict: **"Without durable shared ownership accounting, no
multi-writer data path is expressible."** The mechanism is §6.3's:

> A block cloned on node A has refcount 2 on A and 1 on B, so B's aligned
> overwrite patches in place and mutates a file it never touched.
> `patch_ineligible_shared` never increments.

Derived state cannot survive a second writer because each writer derives from
the subset of the tree it walked. On a passthrough volume (the default) the
resulting cross-file overwrite is **silent**; a transformed volume fails its
AEAD tag, which is the one honest degradation. W1 sole-owner patch (61–67 k
IOPS, the primary random-write path), clone/CoW sharing, the reclaim window,
and fsck C2/C3 all read this state.

Two further costs, independent of multi-writer:

* the walk is O(live inodes) at every mount, against a documented 100 M-inode
  cap;
* it is O(device reads) too — every indirect block map is read back.

---

## 2. Design constraints that forced the shape

Four constraints, each of which eliminates an otherwise-attractive option.

1. **No new on-disk regions.** The upgrade path must be a superblock bit
   stamp on an existing volume (Phase 8). A volume stamped that way has no
   new region and cannot grow one, so the representation must live in the
   *existing* dynamically-allocated record space: the journal plus
   heap-allocated CoW nodes. This kills the "A/B bitmap pages like
   `alloc_ext.rs`" shape for the data plane, however well it fits the
   metadata heap — those pages live in a superblock-named region minted at
   format.
2. **It must survive journal wrap**, so it must be checkpointed into btree
   nodes: a journal-resident-only structure (the `TREE_ALLOC_RESERVED`
   shape) is bounded by the ring.
3. **The delta must ride the layout-publish transaction.** A second commit
   per publish would re-split exactly what the 2026-07-30 write-commit-economy
   campaign collapsed (`layout_publish_batches`, `publish_commit_groups`), and
   the 2026-08-01 rewrite-publish-drain campaign showed the conveyor already
   runs at ρ ≈ 0.92 — an added commit multiplies through the queueing formula.
4. **It must be O(batch), never O(file size).** The same two campaigns
   deleted an O(file-size)-per-publish journal term; re-introducing one as
   "accounting" would undo them.

Constraint 3 has a second-order consequence that fixes the keyspace: a
transaction commits on **one** meta volume — the one hosting the referencing
inode. Cross-volume transactions do not exist yet (they are spec §6.3's TX-1 /
S3.5). Therefore accounting records must live on the *same* volume as the
inode whose publish stages them, and a block's set-wide refcount is the **sum
of its per-volume record populations**. That additivity is not a compromise:
it is precisely the shape a second writer needs, since a writer can only ever
account for the references it owns.

---

## 3. The representation: one record per reference

A fourth logical tree, `TREE_BLOCK_REFS = 6`
(`src/meta_backend/kv/record.rs`), holding **one record per reference** — a
backpointer, the shape btrfs extent backrefs and bcachefs backpointers both
use.

```text
key (28 B, big-endian composites — memcmp order == logical order)
  [0..8)    vol_tag:     u64   durable data-volume identity
  [8..16)   block_idx:   u64   device offset / allocator chunk size
  [16..24)  owner_ino:   u64   the referencing inode (GLOBAL ino)
  [24..28)  block_index: u32   the owner's block-map index, or
                               BLOCK_INDEX_MAP_BLOB (u32::MAX)

value (4 B)
  [0]  version: u8   = 1; anything else refuses LOUD
  [1]  flags:   u8   bit 0 = the reference is an indirect-map blob
  [2..4) reserved: u16  zero; nonzero refuses loud
```

`vol_tag` derives from the durable `vol-{16 hex}` volume id (PR VL3 / KD-5)
by decoding the hex verbatim — the tag *is* the durable identity, with no
hash and therefore no collision argument to make. Grandfathered legacy ids
(device basenames) hash with xxh3-64. Stability is the contract: the tag is a
key component, so it derives only from the durable id string, never from a
path, a mount ordinal, or a set position.

### 3.1 Why not a count, and why not a delta

* **A count record** (`(vol, block) → u32`) needs read-modify-write at commit
  time. Two inodes cloning the same block hold no common lock — the per-ino
  4a DLM guard excludes same-ino writers only — so both would read 1 and
  write 2, losing an increment. Silently mis-counting shared ownership is the
  exact failure this structure exists to prevent.
* **A delta record** cannot express the *first* reference: the §4.2 fold
  counts a `Delta` with no underlying `Put` as an orphaned no-op
  (`META_KV_DELTA_ORPHANS`), and "is this the first reference?" is not
  knowable race-free. A third delta class would also need a branch in
  `fold_deltas_onto_put` and `fold_forward` beside the inode and layout
  classes.
* **A per-reference `Put`** is idempotent at a key nobody else writes; a
  release is a `Delete`; concurrent clones of one block touch distinct keys.
  No RMW, no lost updates, no new fold class.

### 3.2 What the shape buys

| Property | Mechanism |
|---|---|
| `refcount(block)` | the population of the 16-byte `(vol_tag, block_idx)` prefix (`block_range`) — an ordered scan over one or two records |
| the free list | **needs no durable structure**: it is the complement of the referenced set below the cursor, which is exactly the arithmetic `recover_block` / `fsck_reconcile_accounting` already run (`free = highest − referenced`) |
| the allocation cursor | `max(referenced block_idx) + 1`, seeded by the same `recover_block` protocol the derived walk runs |
| one tx | records stage into the same `KvTx` as the layout record and the inode record — one tx = one checksummed journal entry (§4.10) |
| O(batch) | the merge primitive already knows every key it inserts and displaces |
| the oracle | one record per layout map entry ⇒ the durable census and the layout-walk census are the same multiset **by construction** |
| room for S9 | a writer-id component appends after `block_index` without disturbing the refcount prefix |

---

## 4. Where the delta comes from

§5.3's **one merge discipline** is what makes this cheap: every striped/staged
block-map mutation goes through `merge_block_mappings_if_epoch` (or its
coalescing twin `publish_pass`), which already computes the inserted and
displaced keys under `INODE_META_LOCKS`. The accounting delta is therefore one
vector push per changed entry, computed where the truth already is:

| Site | Contribution |
|---|---|
| `BlockMapOp::Merge` / `MergeExpected` | `+new` and `−prev` per index whose key changed; an idempotent re-bind contributes nothing |
| `BlockMapOp::TruncateFrom` / `RemoveBlocks` | `−key` per pruned index |
| `publish_pass` (the coalesced hot path) | the same, per **applied** op — a fencing-stale op contributes nothing, because its entries never enter the map either |
| `clone_file` | `+key` per dest map entry (the durable half of `pin_block_validated`'s RAM pin) |
| `save_metadata_to_backend_ext` | `+`/`−` the indirect-map **blob** under `BLOCK_INDEX_MAP_BLOB`; the DUR-6 CoW blob's custody flips in the same entry that re-points the layout at it |
| `delete_file` (unlink → reclaim) | `−` every map entry and the blob — its own commit; see §6 |

Everything is funnelled through
`DataRouter::block_ref_ops(ino, &[(block_index, key, take)])`, which resolves
each key with `BackendRouter::block_ref_for` — `clean_block_key` +
`parse_block_key` with the same alias rules (`backend_0` / `squeezefs` /
bare-offset) the mount-time walk applies. That shared resolution is what keeps
the two censuses byte-comparable. Keys that resolve to no allocator-managed
offset (legacy `block_prefix` parts, unknown backends) are the same keys the
walk skips, and are counted in `meta_kv_block_refs_unresolved` rather than
dropped silently.

---

## 5. Mount recovery, and the derived walk as the oracle

`BackendRouter::recover_durable_block_refs` seeds every data volume from the
durable records — **no inode-tree walk**:

1. for each data volume, take its `vol_tag`;
2. scan `volume_range(vol_tag)` on every mounted meta volume (paged, 512
   records/page) and sum the populations (§2's additivity);
3. feed the block indices, ascending, through the **same** `recover_block`
   protocol the derived walk uses.

The result is identical to the derived answer by construction: same protocol,
same inputs, one durable record per layout map entry.

`BackendRouter::verify_durable_block_refs` is the **oracle**: it runs the old
walk folded into a census (`BlockAllocator::derived_block_census`) instead of
into the allocator, and diffs it against the durable population per block.
Both sides share `layout_owned_blocks` — an oracle that re-implemented the
extraction would only ever test the re-implementation.

* At **mount**: `SQUEEZEFS_BLOCK_REFS_VERIFY=1`. Off by default, because
  running it pays the very walk the records exist to delete.
* In **fsck**: class **C8** (`C8DurableRefDrift`), with the §5.6
  verify-before-report re-check. `meta_kv_block_refs_drift` is a
  **must-stay-0** tripwire. C8 repair is deliberately **refused**: restating
  the ledger from the walk would erase the evidence of *why* the invariant
  broke, and the layouts remain authoritative either way (they are the
  justification; the ledger is its index).

  **C8 detection is gated on the same `SQUEEZEFS_BLOCK_REFS_VERIFY=1` knob
  until the write-path wiring is complete — see §11.** Everything else about
  the class (verification, planning, the repair refusal, the counter) is
  wired; only the *unconditional* detection waits, so a true finding does not
  arrive as noise on healthy volumes.

---

## 6. `begin_free` → reclaim → `finish_free`, durably

The window has **no intermediate durable state**. The durable effect of a
terminal free *is* the `Delete` that rode the publish which dropped the
reference. So a crash anywhere in the window recovers to one of exactly two
states:

| Crash point | Durable state | Recovery |
|---|---|---|
| after the publish that dropped the reference | no record | block is FREE; on the free list if below the cursor, in the virgin tail otherwise. Reallocatable. `begin_free` of it is REFUSED (the double-release tripwire), so no double free |
| before it (the reclaimer's `finish_free` lost) | record present | block stays ALLOCATED — conservative, so the offset can never be minted to a second owner |

What a crash *does* lose is the queued `BLKDISCARD`/`PUNCH_HOLE`. That is
hygiene, not correctness — which is why `trim --full` walks the whole free
list rather than the debt tracker: the free list is the durable truth, the
debt tracker only the incremental record of it.

**Unlink/reclaim ordering.** `delete_file` releases the corpse's references
*before* freeing its blocks, in its own transaction. Both crash windows are
safe:

* *release → crash → no destroy*: the inode is already `nlink == 0`
  (reclaim runs after unlink committed), and the derived oracle **skips
  `nlink == 0`** — so "no references for a corpse" is what the oracle says
  too. No drift.
* *crash → no release*: the corpse's references survive and its blocks
  recover ALLOCATED — a leak, never a double-owner mint. fsck C2 ("allocated
  with zero referencers", whose referencer walk also skips corpses) and the
  C8 ledger name it.

A separate transaction is correct here: unlink/reclaim is not the publish
path (the no-second-commit rule is about the block publish), and
`destroy_inodes` is a batched multi-ino commit that does not — and should
not — decode layouts to learn block keys.

### 6.1 Stamping a non-empty volume: never trust an empty ledger

A volume the Phase-8 window stamps has layouts that reference blocks and a
ledger with **nothing in it**. A mount that treated that ledger as
authoritative would read every live block as free and hand it to the next
writer — one device offset, two owners, the exact failure this whole
structure exists to prevent.

The rule that closes it: **an empty durable population is never
authoritative.** `recover_durable_block_refs` scans first and seeds second;
a total population of zero DECLINES (returns `None`), so the caller runs the
derived walk. That fallback is free exactly when it is legitimate — a ledger
is empty either because the volume holds no data (the walk finds no layouts)
or because it has not been backfilled (the walk is needed anyway).

`backfill_durable_block_refs` then persists what the walk found: one
idempotent `Put` per reference (map entries, plus a spilled map's blob and
every entry the blob names), one transaction per inode. It is a one-time
upgrade pass, not the publish path, and it resumes cleanly after a crash
because every record is an idempotent `Put` at a content-derived key. After
it, the ledger IS authoritative and every later mount skips the walk.

Pinned end to end by
`stamping_a_non_empty_volume_backfills_instead_of_freeing_live_blocks`:
write three blocks with the bit OFF (zero records — the genuine legacy
state), stamp, assert the empty ledger declines, walk, backfill, assert
exactness; then remount seeding from records alone and assert a fresh
allocation does **not** collide with a live block.

---

## 7. Compatibility matrix (ruling D9)

`FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS = 1 << 8`, presence **OPTIONAL** — the
bit-7 (`KV_DURABLE_TERM`) pattern, not bit 6's presence-required one.

| Volume | This binary | An older binary |
|---|---|---|
| **fresh format** (bit 8 **NOT** set — `SuperblockV3::plan` deliberately omits it, ruling D9) | derived walk, byte-identical to pre-item-1 behavior; no tree; no record ever staged | mounts exactly as before |
| **un-stamped** (formatted before the bit existed — the same on-disk shape as a fresh format) | as above; **sector 0 untouched by mount** | mounts exactly as before |
| **stamped later** (`set_block_refcounts_bit`, Phase 8) | first *writable* mount mints the missing root; the empty ledger declines, the derived walk runs once, and the backfill persists it (§6.1) — after which the walk is never paid again | refuses loud |
| **read-only mount** (unknown-ro bits) | never mints, never accounts — degrades to the derived walk | unchanged |

**Nothing stamps it today** — not mount, and not `plan` either.
`set_block_refcounts_bit` is the sole stamping path and requires the volume
offline (the caller holds the D0 guard).

**Why `plan` omits it, which is a safety property and not only D9
discipline.** The durable ledger is only as complete as the set of write-path
sites that stage into it (§11). While that wiring is incomplete, a
partially-populated ledger is the *dangerous* state — not the empty one: it
is NON-empty, so §6.1's "an empty population is never authoritative" rule
does not fire, and every reference an unwired site failed to stage reads back
as a free block, which `recover_block` then hands to the next writer. That is
the one-offset-two-owners failure this structure exists to prevent, arriving
through the upgrade path instead of the multi-writer path. Derived accounting
has no such mode: it re-reads the layouts, which are always complete.
Re-adding the bit to `plan` is gated on the oracle running clean across the
write-path suites.
Stamp-then-crash is inert: the bit gates no silently-misdecoded record — an
older binary refuses the volume outright, and the tree root and records only
ever come from a mount that saw the bit. Root minting is itself idempotent
across a crash, because the claimed extent's bitmap bit only becomes durable
at a checkpoint, and the mint happens **before** journal replay so any
in-window accounting record folds into the fresh root by key.

Pinned by `a_fresh_format_does_not_carry_bit8_and_mounts_derived` (the format
boundary), `unstamped_volume_is_unchanged_by_mount_and_stays_derived`
(sector-0 byte comparison across a real mount + publish),
`stamping_the_bit_engages_accounting_on_the_next_mount`, and
`stamping_a_non_empty_volume_backfills_instead_of_freeing_live_blocks`.

---

## 8. Integration notes (what did and did not have to change)

Because the records live in a **real §4.2 tree**, the §4.4/§5.5 commit
pipeline needed **no change at all**: leaf resolution, the union leaf-lock
plan, pre-image capture, RAM apply, rollback and replay all dispatch
generically through `tree_by_id`. The lock order is unchanged (4a I-guard
then 4b leaf locks, taken by the conveyor pass).

Deliberate scoping decisions:

* `KvMetaBackend::trees()` stays the **three user trees**. The §4.10 digest
  walk, the VL5 slot-migration keyspace, and fsck's C1 tree walk are all
  defined over that array; widening it would silently redefine all three.
  Structural consumers (checkpoint flush, maintenance, ledger roots) use the
  new `all_trees()`.
* The root ledger's `n_roots` has been variable-length since PR K3 (worst-case
  budget: ~19 spare roots inside the 4 KiB slot), so the fourth root needs no
  wire change and the pre-item-1 *decoder* still parses the payload — old
  binaries are stopped at the superblock gate instead.
* `MigrationTee::note_committed` **skips** the tree. Its keys are
  volume-tagged, not ino-keyed, so `key_owner` would read a random `u64` as
  an ino and could tee a record into a migrating slot's keyspace, copying it
  to a volume it does not belong to.
* The journal's tree-id range widened from `1..=5` to `1..=TREE_ID_MAX (6)`;
  the tag byte's low nibble already had room.
* `debug_audit_records` gained an arm: an accounting record whose key or value
  does not decode under its own type is refused write-side, before a byte is
  persisted.
* The derived walk's `futures::executor::block_on` of the indirect-blob read
  is **gone**. The blob's entries now resolve in an async pass
  (`indirect_owned_blocks`); blocking an executor thread on a device read is
  the write-funnel conviction's exact anti-pattern.

---

## 9. Observability

| Counter | Meaning |
|---|---|
| `meta_kv_block_refs_staged` | references taken (`Put`s staged into layout txs) — the engagement instrument; flat under a striped write storm ⇒ the wiring regressed to derived-only |
| `meta_kv_block_refs_released` | references dropped (`Delete`s) — flat under overwrite churn ⇒ displaced blocks are leaking their accounting |
| `meta_kv_block_refs_recovered` | references seeded from durable records at mount — the count that replaced the inode-tree walk |
| `meta_kv_block_refs_drift` | **must stay 0** — durable-vs-derived disagreements (fsck C8) |
| `meta_kv_block_refs_unresolved` | accounting ops dropped for want of a resolvable block key; 0 on a healthy mount |

Env: `SQUEEZEFS_BLOCK_REFS_VERIFY=1` runs the oracle at mount.

---

## 10. What S9 still owes on top of this

This is the durable **single-writer** representation. It makes S9
*expressible*; it does not implement it. Still outstanding, and explicitly
not in this work:

1. **Per-writer key scoping (§6.2 item 8).** `active_block:`,
   `active_block_ext:` and `mapping:` keys still have no writer-id component,
   so recovery cannot classify a foreign writer's staged records. That is a
   different agent's item; this key layout reserves room for the same
   component (appended after `block_index`, leaving the refcount prefix
   intact).
2. **The other eight format assumptions of §6.2.** One journal ring head per
   volume (item 2), one A/B extent bitmap and `advance_durable` tail (3), one
   A/B root ledger with `slot = seq % 32` (4), the per-mount `next_ino` atomic
   (5), bare reusable block keys without an incarnation component (6), the
   singular `writer_claim` (7), process-local layout-delta base tokens (9),
   and the node-identity-free staging generation stamp (10).
3. **Cross-writer refcount arbitration.** Records are additive across meta
   volumes, but nothing stops two writers from *concurrently* taking the last
   reference decisions on one block. S9 needs custody tokens over the block,
   which is the DLM program's job — the ledger tells a writer what the set
   believes; it does not serialize writers.
4. **A freed-offset grace period (§6.8 item 3).** The highest-value item in
   the coherence analysis. The durable ledger makes "is this block still
   referenced?" answerable *without a walk*, which is the prerequisite; the
   grace period itself — refuse to reallocate an offset until every
   registered reader has acknowledged passing that epoch — is still to build.
5. **Node-cache revalidation (§6.8 item 2).** The records live in btree nodes,
   and the node cache is load-once RAM-authoritative. A second writer reading
   this tree needs the revalidation path (or, per §6.2's own conclusion,
   partitioning so two writers never cache the same node).
6. ~~**W1's seventh ineligibility clause (§6.7).**~~ **LANDED** by DLM S11
   (`feat/mw-range-custody`) — and the framing this document originally gave
   it was **wrong**, so it is corrected here rather than quietly dropped.

   The clause's input is the **range-grant table**
   (`dlm::span_range_shared`), not the refcount. The two predicates answer
   different questions and neither subsumes the other:

   | Clause | Question | Source |
   |---|---|---|
   | 4 (`begin_patch_sole_owner`) | do OTHER FILES reference this block? (clone/CoW) | the refcount — this document's subject |
   | 7 (`patch_range_shared`) | does another WRITER hold a byte-range grant overlapping this block? | the S11 grant table |

   So S11 did not need the durable refcount, and the durable refcount does
   not make clause 7 decidable. What remains true is a sharpening of item 3
   rather than a closure of item 6:

   * **Clause 7 closes the two-writers-one-inode-disjoint-ranges hazard;
     clause 4 still owns the two-files-one-block hazard**, and clause 4's
     read is still the PROCESS-LOCAL `scc::HashMap`. Making the ledger
     durable makes that answer survive a remount; it does not make it
     coherent across live writers. Under S9, clause 4's read must be
     re-sourced from the durable ledger (or invalidated by a custody
     protocol) or the §6.3 W1 hazard returns unchanged — a second writer's
     clone reference is invisible to the first writer's map no matter how
     durable it is at rest.
   * **The composition is already correct on the write path.** When clause 7
     refuses, the write falls back to CoW, which routes through the merge
     primitive (accounted); when the in-place-rewrite arm runs instead, it
     routes through `rewrite_shadow_record` (accounted via the deferred-op
     accumulator, §11). And when a range-custody writer displaces a block a
     clone still references, the durable refcount is what makes that free
     NON-terminal — pinned by the mixed-workload leg's
     displacement-then-free step.

---

## 11. The write-path wiring: how it was finished

**The oracle was the checklist, and it is now clean.** Turning the ledger on
across the layout-publishing suites (via the `SQUEEZEFS_TEST_STAMP_BLOCK_REFS`
format seam, since a fresh format deliberately does not stamp — §7) reported
drift of two shapes:

| Shape | Meaning |
|---|---|
| `0 durable vs N derived` | a **take** was missed: a site published a layout naming a block without staging its reference |
| `N durable vs 0 derived` | a **release** was missed: a site displaced or pruned a mapping without staging the release |

### The sites, and what each was missing

| Site | Missing | Fix |
|---|---|---|
| `merge_block_mappings_if_epoch` (all four `BlockMapOp` arms) | takes + releases | exact O(batch) delta from the insert/prune loop |
| `publish_pass` (the coalescing publish conveyor) | takes + releases, per APPLIED op | same, with fenced ops contributing nothing |
| `clone_file` | the dest's whole reference set | staged into the dest's layout commit |
| `save_metadata_to_backend_ext` | the indirect-map blob and its DUR-6 CoW predecessor | `BLOCK_INDEX_MAP_BLOB` take/release in the same tx |
| staged→striped promotion; StorageFull spill; staged whole-image promotion; staged truncate prune | whole-map swaps | `block_ref_ops_for_map_swap` (per-index diff; those sites are already O(map)) |
| `delete_file` (unlink → reclaim) | releases | its own tx, ordering per §6 |
| **`rewrite_shadow_record`** (the last one, and a CLASS) | **everything** — it mutates the RAM map, marks the layout dirty, and does not persist | the deferred-op accumulator, below |

### The last gap was a class, not a site

Accounting rides the layout COMMIT, but a site that mutates the map and leaves
the layout **dirty** defers persistence to a later publish — and that save
cannot know which references changed hands, because the map it is handed
already contains them. The rewrite-shadow ACK path does exactly this. On the
plain 3-block healthy population the oracle read:

```
oss1:4194304  durable=1 [ino 2 idx 1]  derived=0   <- stale: release missed
oss1:8388608  durable=0 []             derived=1   <- take missed
oss1:12582912 durable=0 []             derived=1   <- take missed
```

— index 0 correct, index 1 recorded against its FIRST binding and never
re-pointed, later indices never recorded: one publish's worth of staging then
silence, the signature of a map mutated *between* saves. (Localizing it needed
the drift log to name each block's OWNERS — ino + map index — because the
owner is what names the publishing site.)

The fix is **structural**: a per-ino deferred-op accumulator
(`DataRouterInner::pending_block_refs`). A site that mutates the map without
persisting NOTES its delta inside the same `INODE_META_LOCKS` section, and
whichever save persists that map DRAINS it into the same transaction. Deferred
deltas drain first (they are older than the persisting call's own), so a
take-then-release sequence on one index keeps its order; a failed commit
re-notes them, the dirty-layout refill discipline. **Any future deferring site
is correct by construction rather than by remembering.**

**The drain law (Vector B, closed 2026-08-04 — rc-manifest §3d item 4): notes
drain only into saves that persist the WHOLE map.** A publish-class save with
pending notes is forced off the O(batch) layout-delta path
(`save_metadata_to_backend_ext`'s `has_deferred_refs` eligibility clause): a
delta-class commit persists only its own `publish_entries`, so carrying the
notes would durably claim releases/takes for shadow bindings its folded map
does not reflect — a crash inside that window on a stamped volume is exactly
the C8 drift the ledger exists to prevent. The caller must make this call,
not the backend: delta-vs-full is otherwise the backend's verdict
(`merge_layout_and_size` → `use_delta`), reached after the refs are already
staged into the tx — and the symmetric alternative (leave the notes pending
across delta saves) is unsound because the backend's always-correct fallback
full-Put would then persist the bindings *without* their ledger records,
drifting in the take-missed direction. The forced full save is the notes'
immediate carrier (no starvation window; the epoch close's own full save
remains the terminal backstop), and the next publish re-enters the delta
economy with the chain re-based. Pinned red-first by the Vector B section of
`tests/durable_block_refs_tests.rs`.

### Completion criterion, met

`fsck` class **C8 is ungated** — `meta_kv_block_refs_drift` is a live
must-stay-0 tripwire — and the oracle runs clean with the ledger engaged
across: fsck, fsck-repair, write-through (+coverage), write-commit-economy,
publish-drain-economy, write-supersession, reclaim (block-free, async, batch,
discard-elision), truncate/hole/sparse, staged (crash-recovery, dirty-layout
refill, rmw-alloc), data-path-correctness, striped-overwrite-lazy-seed,
rewrite (amp, shadow), in-place overwrite, extent (overlay, patch,
record-recovery), and clone (refcount, cli, copy_file_range).

The permanent pin is
`durable_block_refs_tests::fsck_reports_no_durable_reference_drift_on_a_healthy_volume`:
it drives publishes, a displacement, a clone, a punch and a truncate on a
stamped volume, runs fsck, and asserts C8 is empty. It lives in that suite
rather than `fsck_tests` because a fresh format does not stamp bit 8, so
`fsck_tests`' own C8 arm does not run and the pin would evaporate.

**Detection cost when nothing drifts** (why C8 can be unconditional in fsck
but not at mount): fsck already walks every inode for C1/C2/C3 and the census
reuses that same extraction, so detection adds no walk fsck did not owe —
only one paged range scan of the reference tree per data volume (~2.9
ns/reference to decode) plus a `BTreeMap` fold and diff (~90 ns/reference).
At MOUNT the walk IS the whole cost the durable records exist to delete, so
there it stays behind `SQUEEZEFS_BLOCK_REFS_VERIFY=1`.

### The stamp: an argued decision NOT to take it now

Finishing the wiring and ungating C8 **permits** re-adding bit 8 to
`SuperblockV3::plan`. It is deliberately **not** taken, and the reasoning
belongs on the record rather than as an omission:

* **The upside today is nil.** Nothing consumes the durable ledger except the
  mount-time seed, and the derived walk it replaces is correct. The stamp's
  value arrives with S9 — and with it the Phase-8 window that ruling D9
  reserves for exactly this class of bit.
* **The downside is not symmetric.** If a shape this census does not cover is
  unwired, an unstamped volume loses *nothing* (derived accounting is used),
  while a stamped one can silently reallocate a live block: a
  partially-populated ledger is NON-empty, so §6.1's never-trust-an-empty-
  ledger rule does not fire.
* **The census, though broad, is not exhaustive.** 51 suites run clean with
  the ledger engaged (§11), but the release-gate tier does not run here —
  fstests / LTP / pjdfstests and any real-mount venue are outside it, as is
  GDS. "Clean across the suites" is strong evidence of completeness, not a
  proof of it, and the asymmetry above says to spend that uncertainty on the
  safe side.

So the shipped default stays derived accounting, and the stamp is one
`set_block_refcounts_bit` call in the Phase-8 window whose prerequisite
evidence is this document plus `.benchmarks/2026-08-04-durable-block-refcounts.md`.

### Still open

* **The whole-reference oracle.** The comparison is per-block counts; the
  drift log now names owners, but a tuple-level compare would localize
  without needing the log.
