# Design: PB-class file support — the `TREE_BLOCK_MAP` KV tree (finding 42)

**Status: DRAFT Rev 0** (design phase 2026-09-01; drafted by the f42 planning
pass, unreviewed — run the design review loop before the first PR). The
implementation is the `feat/kvmap-*` PR ladder in §4.

## 0. Problem

A file's striped block map lives inline in its `layout` xattr until the
encoded map exceeds `xattr_value_cap − LAYOUT_INLINE_HEADROOM`
(`src/routing.rs` `needs_indirect`, ~6479) — ~60 KiB ≈ 6–8 GiB of file at
4 MiB blocks. Past that it spills to a single CoW blob block
(`encode_indirect_block_map`, routing.rs ~1188; the one-block refusal at
~6559). Consequences:

1. **Hard ceiling ≈ `block_size²/30`** (~545 GiB at 4 MiB blocks): 2 TB and
   1 PB files are impossible (finding 42, user-reported 2026-09-01).
2. **O(file) publish cost**: every publish of an indirect ino rewrites the
   whole blob (`publish_indirect_blob_bytes`) + a flush barrier.
3. **Delta-ineligible heads**: every indirect publish is a full save
   (routing.rs ~6766).
4. **Structural fragility**: the blob-CoW-under-shipping lifecycle is the
   class findings 23/24/33/35c/41 all live in (mint guards, compose memos,
   displaced-blob lists, custody transfer).
5. **Unbounded RAM**: the read path rehydrates the whole map per open giant
   file (routing.rs ~5632).

## 1. Verdict

Replace the blob with a **first-class KV tree**: `TREE_BLOCK_MAP = 7` under
superblock incompat **bit 16** (`KV_BLOCK_MAP_TREE`) — one record per
mapping (with run records for contiguity), riding the existing commit
conveyor, journal, checkpoint, node cache, and fold machinery verbatim.

Precedent: `TREE_BLOCK_REFS` already carries one record per block reference
at exactly this cardinality — the volume has already accepted
record-per-block scale for this population. The map tree is the same shape
keyed the other way.

Rejected alternatives (evaluated against publish cost / read cost / crash
atomicity / rung-19-20 compose / fsck / blast radius):

- **(a) chunked segment blobs** — keeps every structural ingredient of
  finding 41 (out-of-band CoW blob writes, custody transfer, compose
  memos) and multiplies the crash windows by K; still one device write +
  barrier per publish; still whole-segment RAM rehydration.
- **(b) delta-eligible indirect heads** — `decode_base_layout`'s refusal of
  indirect bases IS the fold law that keeps finding 41's class contained
  (layout_wire.rs ~633); legalizing delta-on-indirect widens it. Ceiling
  unchanged.
- **(c) extent encoding alone** — collapses sequential maps (~len×) but
  random-written files regress to today's ceiling. Folded INTO the chosen
  design as the run-record encoding instead (PR 6).

## 2. Format

- **Tree**: `TREE_BLOCK_MAP: u8 = 7`; bump `TREE_ID_MAX` (record.rs:43);
  disjointness pins per the bit-table convention.
- **Key** (memcmp-ordered, the block_refs BE convention):
  `owner_ino u64 BE ‖ block_index u32 BE` (12 B).
- **Value** (versioned like `BLOCK_REF_VALUE_VERSION`):
  - v1 **POINT** (~20 B): `version u8 ‖ kind ‖ vol_tag u64 ‖ offset u64` —
    binary, `vol_tag` = the durable `vol-{16 hex}` id verbatim (KD-5).
    Decorated keys (`damaged:` markers, `:extra` trailers) ride
    `kind=STRING` verbatim.
  - **RUN** (PR 6): key at `start_index`, value
    `kind=RUN ‖ vol_tag ‖ start_offset ‖ len u32` covering
    `[start, start+len)`. Read law: an exact-match POINT at N supersedes a
    covering RUN — overwrites are one Put, never a hot-path run split; a
    maintenance coalescer canonicalizes.
- **Head sentinel**: `block_map_id = Some("kvmap:1")`, `block_map: None` —
  ~100 B layout head. `decode_base_layout` refuses `kvmap:` bases exactly
  as `indirect:` — **kvmap heads stay layout-delta-ineligible by law** (no
  new fold-on-map path exists at all; the head Put is already O(100 B)).
- **Bit 16** stamped+barriered before a volume's first map record, never on
  untouched volumes (the bit-5 `KV_LAYOUT_DELTAS` precedent). Old binaries
  refuse loud. `indirect:` blob decode is retained forever on the
  read/walk side; a legacy blob ino converts on its first publish under a
  new binary (rehydrate → chunked migration → head-flip tx releases the
  blob's MAP_BLOB record → blob freed by the existing post-commit tail).
- **No MAP_BLOB sentinel for kvmap inos** — map records are metadata
  (checkpointed CoW nodes), not data blocks.

## 3. Mechanics

- **Crossing** (once per ino, idempotent): chunked record Puts
  (`SQUEEZEFS_MAP_MIGRATE_CHUNK`, default = the finding-38
  `BLOCK_REF_TX_CHUNK = 512` law) under the caller's held 3.5 section; one
  final tx flips the head to `kvmap:1` + tail chunk + this publish's
  BlockRefOps. A crash mid-migration leaves invisible re-Puttable residue
  (reads still follow the old head); fsck C11 reports it.
- **Publish**: map-entry Puts (+ range Deletes on truncate) staged into the
  SAME KvTx as the head Put and the BlockRefOps — one conveyor pass = one
  journal entry. A 64-block window ≈ 4 KiB of journal vs today's 4 MiB
  blob DMA + barrier + full save. The Vector-B deferred-notes complication
  simplifies: a kvmap save persists exactly what it carries.
- **Read**: dirty overlay first, then `get_block_mapping(ino, index)` — a
  point lookup with run-floor fallback. Warm = node-cache bset search;
  cold = one leaf read amortizing ~8 k neighboring entries. RAM bounded by
  the node-cache budget; optional per-ino window as R5 component
  `block_map_window` (floor 0). Co-writers resolve via a shipped
  `GetBlockMapRange` verb (the `XattrValueCap` pattern; schema bump).
- **Rung-19/20 compose**: shipped entries become record Puts under the
  owner's conveyor — same-key ordering is the KV layer's existing law. The
  blob-compose apparatus (memos, `IndirectBlobGuard`, displaced-blob
  lists) becomes legacy-only and shrinks toward deletion (no-dead-code).
- **fsck / walkers**: ONE shared extraction arm (`kvmap:` heads scan the
  tree) used by C2/C8, `verify_durable_block_refs` (both oracle sides
  through the same path — the C8 law), the mount recovery walk, backfill,
  defrag, jobs. New class **C11 — map-plane consistency** (C9/C10
  precedent): orphan map records (crossing residue / dead ino),
  run-vs-point coverage sanity; report-only + quarantine repair, zero-FP
  via the era floor + settle ladder.

## 4. PR ladder (each independently gateable, tests-first)

1. `feat/kvmap-tree-core` — tree 7 + codecs + bit 16 + tx ops +
   `get_block_mapping`. Proptest/fuzz codec laws; untouched-volume
   byte-identity pinned; `meta_lv_bench` group.
2. `feat/kvmap-crossing` — spill switch + chunked migration + head flip;
   legacy-blob conversion on publish. Crash-window matrix; C8 oracle clean;
   `SQUEEZEFS_KVMAP=0` A/B lever.
3. `feat/kvmap-read` — overlay-then-tree resolution + R5 window + range
   ops. Giant-sparse-index matrix; `read_serve_phase_ns` bench.
4. `feat/kvmap-walkers` — recovery walk, fsck C2/C8 arm + C11, backfill,
   defrag, jobs. `fsck_findings == 0` healthy; seeded-residue detection.
5. `feat/kvmap-mw` — `GetBlockMapRange` + owner compose record-Put arms.
   The finding-41 venue rig (24 writers × iodepth 16 over the crossing) as
   the per-PR house gate.
6. `perf/kvmap-runs` — run records + straight-run emission + coalescer.
   Counted A-B-B-A on tcp devsub; sustained rows; amplification columns.
7. `feat/kvmap-acceptance` — 2 TiB / sparse-PB soaks, lifecycle
   interactions, scoreboard giant-file row, docs + release adjudication.

Sequencing: PR 2 lands only after finding 41's fix (landed `08202c32`);
PR 5 is where the residual fold-law care lives.

## 5. Numbers

- Crossing: unchanged (~60 KiB inline ≈ 6–8 GiB at 4 MiB blocks); zero
  cost below it (pinned byte-identical).
- Ceiling: `min(2³² × block_size, u64)` = **16 EiB at 4 MiB blocks** (was
  ~545 GiB). Practical bound = meta capacity: ~8 GiB of map leaves +
  ~13 GiB of ref leaves per 1 PiB file at 4 MiB blocks; runs collapse the
  map side ~len× for sequential data.
- Gauges: `block_map_tree_{records,puts,deletes,run_puts,leaf_reads,lookup_overlay_hits,lookup_tree_hits}`,
  `map_migrate_{inos,records,resumed}`, `publish_map_record_bytes`
  (`publish_indirect_blob_bytes` / `layout_indirect_map_reads` must trend
  to 0 on converted volumes), `block_map_window_bytes` (R5), must-stay-0
  `fsck_map_orphan_records`, `meta_ship_publish.map_{shipped,served,refused}`.
- Knobs (ENG-10 registry): `SQUEEZEFS_MAP_MIGRATE_CHUNK` (derived, floor
  64), `SQUEEZEFS_KVMAP=0` (A/B measurement lever), `SQUEEZEFS_MAP_WINDOW_{MB,PCT}`.
