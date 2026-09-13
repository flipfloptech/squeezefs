# 2026-09-13 — Symmetric metadata PR 1: the slot-tree forest (`feat/sym-forest`, DARK)

**What landed.** The on-disk shape of `docs/design-symmetric-metadata.md`
§5.2 B(i) (KD-SYM-2): under incompat **bit 17** (`KV_SYMMETRIC_FOREST`) a
metadata volume holds ONE mixed-kind KV tree per routing slot instead of one
tree per record kind, plus a control tree (`TREE_CONTROL = 8`, "tree 0")
whose `slot_state` records name every guest slot tree's root; the fixed
ledger names tree 0 and the native slot tree. The kind bytes are the tree
ids (`KIND_INTERIOR = 0` for a slot tree's interior records, `tag_for(0,
level ≥ 1)`; `TREE_SHARED_INDEX = 9` reserved; `TREE_ID_MAX` 7 → 9); forest
keys are `ino ‖ kind ‖ rest` (inode 9 B / dentry 17 B / xattr 17 B /
block-map 13 B) and `0x06 ‖ …` for block references (29 B). One router
(`kv/forest.rs` `SlotTrees`) turns every `(kind, legacy key)` the shipped
codecs produce into `(slot tree, forest key)`; slot trees are minted lazily
on a slot's first record; the checkpoint task publishes every moved guest
root into tree 0 before its flush pass, so the cycle's ledger record covers
a slot tree's root swap.

**It lands DARK.** Nothing stamps bit 17 but the test seam
`SQUEEZEFS_TEST_STAMP_SYMMETRIC=1` (`Kind::Harness`, registered); `format
--symmetric` / `volume enable-symmetric` are PR 11's, the default flip PR
14's. **A bit-17-absent volume takes the shipped code path verbatim** — the
PR's most important pin, and the reason every pre-forest suite is the
regression instrument here.

Two commits on `feat/sym-forest` off `dev` `89f34ed7`: the red-first
contracts (`bb059206`), the implementation (`df4682ff`), then the docs +
this note. Dev-box, debug-build, in-process evidence ONLY — scoping, per
the venue rule; the squeeze-test solo re-gate A-B-B-A (gate 1) is owed
(§7).

---

## 1. The contracts (`tests/sym_forest_tests.rs`, 22, all green)

| Contract | Pins |
|---|---|
| `kind_bytes_are_the_tree_ids_and_the_id_space_extends_to_the_control_and_shared_trees` | kinds 1/2/3/6/7 = the tree ids; `KIND_INTERIOR = 0`, `TREE_CONTROL = 8`, `TREE_SHARED_INDEX = 9`, `TREE_ID_MAX = 9`; `is_slot_tree_kind` is exactly the five |
| `forest_key_lengths_match_the_design` | 9 / 17 / 17 / 13 / 29 |
| `forest_keys_round_trip_every_kind_and_refuse_a_wrong_length` | `forest_key` ∘ `split_forest_key` = id; a wrong-length legacy key is refused at the encoder; the byte layout (`ino ‖ kind ‖ rest`, `kind ‖ legacy` for refs) |
| `forest_keys_preserve_memcmp_order_within_a_kind` | 400 random native + guest inos × 5 kinds: forest order ≡ legacy order |
| `a_kind_byte_is_never_another_trees_id` | kinds 0/4/5/8/9/10 refused on both sides (round-3 Issue 6); a forged kind byte 8 or 0 inside a slot-tree key is corruption; an 8-byte key has no kind byte |
| `a_record_routes_to_the_slot_its_key_names` | native → 0, guest `s` → `s + 1`; a dentry routes by its PARENT, a ref by its OWNER at offset 17, a separator like a content key |
| `stat_and_layout_are_adjacent_in_ino_major_order` | inode < dentries < xattrs < block map < next ino; every by-block key after every ino-major key |
| `interior_records_carry_kind_zero_and_the_control_tree_tags_decode` | `tag_for(0, 1) = 0x10`; `decode_entry_payload` admits (0, level ≥ 1), 8, 9 and refuses (0, 0) and 10 |
| `bit_17_is_the_symmetric_forest_and_is_known_to_this_binary` | `1 << 17`, ≠ bit 16, in `FEATURES_INCOMPAT_KNOWN` |
| `the_test_seam_is_a_registered_harness_knob` | ENG-10 |
| `slot_state_records_round_trip_and_refuse_malformed_images` / `slot_state_keys_sort_by_slot_index_and_decode` | tree 0's codec: versioned, truncation / future version / unknown variant refused; keys memcmp-sort by slot |
| **`format_without_the_seam_stamps_nothing_and_names_the_shipped_roots`** | **the DARK pin**: the un-stamped builder image has no bit 17 and names exactly the three §4.2 roots |
| `format_under_the_seam_stamps_bit_17_and_names_tree_zero_and_the_native_slot_root` | the stamped image's ledger names ids {0, 8} and no per-kind root |
| `the_forest_mounts_and_serves_the_conformance_population` / `the_un_stamped_volume_takes_the_shipped_path_and_serves_the_same_population` | the kv_backend_tests population (lookup / getattr / readdir / xattr / hard link) served identically from both layouts |
| **`the_forest_and_the_shipped_layout_fold_to_the_same_digest`** | the §4.10 digest over `(kind, legacy key, value)` is layout-independent — the forest is a relayout, never a rewrite |
| `forest_mutations_commit_checkpoint_and_replay_to_the_same_digest` | 64 creates + xattrs + 16 unlinks → clean shutdown (empty window) → remount digest-equal, names + xattrs served |
| `forest_replay_twice_digest_equality` | 48 creates, barrier, drop-without-checkpoint → two successive replays of the same window equal the live digest and each other |
| **`a_stamped_set_mints_into_guest_slot_trees_whose_roots_ride_tree_zero`** | a one-member derived-width set under the seam: 128 creates spread over the 64 rotor slots → 64 slot trees (63 minted lazily); `checkpoint_now` publishes exactly 63 `slot_state` records into tree 0 and the ledger still names only {0, 8}; remount reopens all 64 from tree 0, digest-equal, every child + xattr resolves |
| `an_empty_slot_owns_no_extent` | a fresh single-member forest: one slot tree, zero `slot_state` records, no root for an untouched guest slot |
| `a_root_swap_pins_the_floor_until_the_ledger_names_it` | 48 files × 4 KB xattrs swap the native root; the checkpoint's ledger names the swapped root; the SECOND barriered cycle's tail stands past the pre-swap head (reclamation lags one cycle by design — the same on a flat volume) |

Regression instrument (un-stamped volumes, the shipped path): `kv_backend_tests` 36, `kv_leaf_merge_tests` 13, `kv_node_cache_coherence_tests` 21, `kv_partitioned_append_tests` 27, `kvmap_tree_tests` 14, `kv_journal_tests`, `durable_block_refs_tests` 24, `crash_contract_tests` 25, `derivation_sweep_tests` 51, `env_knob_convention_tests` 21, `docs_parity_tests` 5, `writer_scoped_staging_tests` 32, `decoder_property_tests` 29, `fsck_tests` / `fsck_c9_tests` / `fsck_c10_tests` — all green after the sweep (two pins moved with the format law: `TREE_ID_MAX` is 9, and 10 — not 8 — is the first id past the table).

## 2. The design as built — and the four places it was decided

| Decision | As built | Why |
|---|---|---|
| **Where the kind byte enters** | ONCE, at the staging choke point (`build_queued_tx` → `stage_key`); every downstream step — leaf resolution, the journal entry, replay, the migration tee — sees the forest key, and the tag stays the kind (the journal wire is the shipped one byte-for-byte, only the key bytes gained the kind) | the design's "content records journaled `tag_for(kind, 0)`", with the ~300 per-kind key builders across the tree left untouched: they produce LEGACY keys and the router frames them |
| **Node → tree dispatch** | every slot tree's nodes carry header `tree_id` 0, so `CachedNode` gained a RAM-only `forest_slot` stamp (set by the owning `KvTree` at `descend` return and at every publish) and the checkpoint's flush pass, the heap-admission root check and the maintenance enqueue dispatch by it; `KvTree::owns` = header id ∧ stamp at every maintenance / merge / census filter | the 40-byte node header has no free field; a separator-derived slot is not defined for a one-leaf root spanning `[b"", MAX]`; the stamp is one relaxed store on the traversal path |
| **Where guest roots live** | tree 0, written by the checkpoint task as ONE checkpoint-class journal entry (`try_admit(Checkpoint)` → `reserve_registered` → `apply_replayed` with the reserved seqs → `commit_entry`) BEFORE the flush pass; the native root and tree 0's own root ride the ledger | the SMO journal's own protocol; a journaled record replays into tree 0 if un-flushed, and the flush pass carries tree 0's leaf in the same cycle, so the cycle's ledger record covers the publication — the FIND-VS-A dying-floor argument holds per slot tree with no new mechanism |
| **Range scans** | `range_kind(kind, start, end, max)` translates sentinel bounds (`[0]`, `KEY_SPACE_MAX`) as prefix cuts, probes ONE slot tree when the window spans one slot (chain scans, an ino's xattrs, a directory's entries) and walks the forest in slot order otherwise (= legacy key order), filtering the mixed leaf by kind and returning legacy keys | every walker in the tree (fsck, defrag, jobs, migration, the census) works unchanged on both layouts |

Two more decisions worth stating: `flat_trees()` (the pre-forest `trees()`
surface) is a FLAT volume's per-kind trees and EMPTY on a forest —
production code never indexes it (every `src/` caller was swept to the
kind-routed API; the tests that drive per-kind trees directly format
un-stamped volumes); and `TreeSet::{Flat, Forest}` is the one enum every
layout-dependent site matches on, so a grep for `TreeSet::` is the
complete list of places the two layouts diverge.

## 3. In-process measurements (dev box, `cargo test` debug build — scoping only)

`a_stamped_set_…`'s shape: one derived-width member, 64 KiB nodes, 1 MiB
ring; 128 `create` + `setxattr` into one directory, then a second round of
128 (measurement scaffolding, removed after the rows were read; 3 runs each).

| Row | flat (un-stamped) | forest (stamped) |
|---|---|---|
| first round, 128 files (incl. the forest's 63 first-touch mints) | 15.1 / 16.7 / 17.4 ms | 27.6 / 32.2 / 26.4 ms |
| second round, 128 files (no mints) | 15.7 / 15.5 / 17.4 ms | 16.9 / 24.5 / 16.5 ms |
| heap extents used after one checkpoint | 5 of 1,004 (4 trees + 1 SMO) | 65 of 1,004 (tree 0 + 64 slot trees) |
| trees | 4 (inodes, dentries, xattrs, refs) | 65 |

Read: the steady-state create path is within the debug build's noise band
of the shipped one on the good runs (the 24.5 ms sample carries a
background checkpoint tick); the first wave pays the mint cost — 63 × (one
extent claim + one 64 KiB node write + load-back) — which is exactly §1.6's
per-slot extent floor and first-wave row, and the reason the ceiling is
`--meta-node-kib` on small sets. Two per-op costs were found and removed
on the way (both in the same test's second-round row): `seq_handle()`
walked the whole tree set per staged record (65 Arc clones + a sort — now
one clone from tree 0), and `range_kind` iterated every slot tree for a
single-slot chain scan (now one probe). **None of this is a number**: the
row is a debug build on the thermally-capped dev box; gate 1's A-B-B-A on
squeeze-test (mdstorm, rand-4k, `w_fresh`, mount time) is owed and is what
adjudicates "within noise".

## 4. Found beside the program

- **`TEST_SMO_BUILD_PAUSE_TREE` used `0` both as OFF and as the compared
  tree id.** A slot tree's node-header id IS 0, so every slot-tree SMO
  parked forever on an UNARMED seam — the first checkpoint of a forest
  volume hung in `smo_replace`'s build window holding the SMO mutex, and
  `checkpoint_now` behind it. The arm check now tests the sentinel first.
  (Every shipped tree has id ≥ 1, so the shipped path never met it.)
- **Per-kind ownership filters in `KvTree` compared header ids only.**
  With 64 slot trees all at header id 0, `flush_dirty` /
  `cache_map_dirty_addrs` / the merge census / `resident_nodes_at` would
  have adopted every other slot tree's nodes. `KvTree::owns` (id ∧ stamp)
  is at every site.

## 5. Stats

`meta_kv_forest_slot_trees_minted` (guest slot trees minted this mount —
the lazy-mint engagement gauge, 0 for the life of every un-stamped mount),
`meta_kv_forest_root_publishes` (`slot_state` records published; ≤
checkpoints × slot trees), `meta_kv_forest_key_violations` (**must stay
0**: a record under a key the §5.2.1 codec refuses — the
partition-violation class of "a kind byte is never another tree's id").
Exported on the stats inode; rows in `docs/operations.md`.

## 6. Deviations from the PR row (for adjudication)

1. **`SlotTrees` lives in a new `kv/forest.rs`** (the row named
   `kv/tree.rs` / `kv/backend.rs`); `KvTree` gained the slot-tree
   constructors and the owner stamp, `backend.rs` the routing helpers.
2. **The forest's node-header `tree_id` is 0 (`KIND_INTERIOR`) for every
   slot tree node**, leaf and interior alike — the design names kind 0 for
   interior RECORDS' tags; the header reuse follows (a leaf's header id was
   never a routing input, and a per-slot id does not fit a u8).
3. **Content records' journal keys carry the kind byte; the tag is the
   kind** — exactly §5.2.1. Journal INTERIOR records of slot trees carry
   tag `(0, level)` and forest-form separator keys, routed by slot.
4. **The fsck census walks a forest's slot trees once per KIND** through
   the kind-routed `range_kind` (three passes over the mixed leaves) —
   correct, not the design's ONE-walk census (§5.8.5); the walker
   structure is per kind throughout `fsck.rs`, and the one-walk refactor
   is owed (§7).
5. **`digest_walk(&[&KvTree])` was folded into `digest_backend`** (the
   per-tree form had no caller left once the digest became kind-routed).
6. **The pack law (§5.4.3) is untouched**: block references route by
   their OWNER ino, so a pack block's tenants of different slots put its
   references in different slot trees today; `block_ref_count` /
   `block_ref_scan` therefore probe EVERY slot tree (the `0x06 ‖ vol ‖ blk`
   prefix is one range per tree) — exact, O(slot trees) per probe, until
   PR 7 packs per `(writer, slot, data volume)`.
7. **The `slot_tree_record` / `slot_state_record` fuzz targets and the
   `decoder_property_tests` mirror are not written** (§7).

## 7. Owed

- **Gate 1 on squeeze-test**: the A-B-B-A solo re-gate (mdstorm, rand-4k,
  `w_fresh`, scoreboard smoke, mount time) at this format-changing PR —
  `dlm_rpcs == 0`, within noise; leaf count vs population; the packing row.
- **Fuzz**: `slot_tree_record` (absorbing `kvmap_record`) and
  `slot_state_record` targets + the stable proptest mirror.
- **The ONE-walk fsck census** over the mixed leaves (C1–C10 in one pass
  per slot tree).
- **PR 2's owed input**: `slot_state.tails` is written empty and
  `cursor` 0 — the handover / per-slot cursor semantics are PR 2/4's.
- **The `apply_locked` lease gate** (`leased_slots`) — PR 4, "lands HERE,
  with the first two live appenders".
- The forest's per-slot extent floor at small `--meta-node-kib` (gate 6).
