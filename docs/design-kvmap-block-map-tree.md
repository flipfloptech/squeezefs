# Design: PB-class file support — the `TREE_BLOCK_MAP` KV tree (finding 42)

**Status: DRAFT Rev 1** (design phase 2026-09-01; Rev 0 drafted by the f42
planning pass; Rev 1 folds the adversarial review's amendments — §6 — whose
three critical findings supersede the corresponding Rev 0 clauses. Do not
start the PR ladder before §6's A1–A5 are reflected in PR 1/2 scopes). The
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

- **Crossing** (once per ino, idempotent): a **residue-sweep prologue**
  (Rev 1, A1 — chunked range-Delete of `[ino‖0, ino‖u32::MAX]` before the
  first Put: a crashed prior crossing's residue would otherwise resurrect
  as stale mappings after a truncate-shrink-recross-extend sequence —
  silent wrong data; on a healthy ino the sweep is one empty leaf
  descent), then chunked record Puts (`SQUEEZEFS_MAP_MIGRATE_CHUNK`,
  default = the finding-38 `BLOCK_REF_TX_CHUNK = 512` law, registry max
  ≤ 1,024 so a mis-set knob cannot poison the entry cap), then one final
  tx flipping the head to `kvmap:1` + tail chunk + this publish's
  BlockRefOps. **The whole train (sweep → chunks → flip) holds the ino's
  exclusive 4a** (Rev 1, A3/A4 — the 3.5 section covers local publishes
  only; 4a is what fsck's re-check and the served compose arms serialize
  against). A co-writer-custodied ino's crossing SHIPS: the map travels as
  a RETRIED-class idempotent verb with the FreeBlocks dedup-window
  pattern, executed by the authority under the same held-4a train. A
  crash mid-migration leaves invisible residue the next crossing's sweep
  deletes; fsck C11 reports it (report-only — see §Mechanics/fsck).
- **Publish**: map-entry Puts staged into the SAME KvTx as the head Put
  and the BlockRefOps — one conveyor pass = one journal entry. A 64-block
  window ≈ 4 KiB of journal vs today's 4 MiB blob DMA + barrier + full
  save. The Vector-B deferred-notes complication simplifies: a kvmap save
  persists exactly what it carries.
- **Truncate/unlink** (Rev 1, supersedes Rev 0's same-tx range Deletes —
  impossible at scale: a 1 PiB truncate is 2²⁸ Deletes ≈ five orders past
  the whole-entry cap, and today's `truncate_layout` would materialize the
  removed set in RAM): **size-flip-first + background sweep**. The SETATTR
  tx commits only the new size plus a durable per-ino sweep cursor in the
  head sentinel (`kvmap:1;sweep:K`); reads clamp to size, so shadowed
  records are immediately unreadable. A job-fabric sweep then walks the
  ino's range in chunks — each tx = map Deletes + their ref releases
  (§6.2's law preserved per chunk) + reclaim enqueues — crash-resumable
  from the cursor, throttled. Unlink/`delete_file` ride the same sweep.
  fsck treats a head with an open sweep cursor as exempt-in-range.
- **Read**: dirty overlay first, then `get_block_mapping(ino, index)` — a
  point lookup with run-floor fallback. Rev 1 (A6): the tree has no
  floor/predecessor primitive, so runs are bounded (`RUN_LEN_MAX = 4096`)
  and the floor is one bounded forward `range([ino‖N−RUN_LEN_MAX, ino‖N])`
  take-last with an owner-ino prefix check; the coalescer's run-Put and
  point-Deletes ride ONE tx. Rev 1 (A9): head+record resolution brackets
  with the reader revalidation seqlock (retry on epoch change) and the
  `CachedMetadata`-head-vs-live-record skew rule is stated in PR 3. Warm
  = node-cache bset search; cold = one leaf read amortizing ~8 k
  neighboring entries. Rev 1 (A7): tree-7 leaf loads enter the node-cache
  clock on PROBATION (no second-chance until a second touch) so a giant
  streaming file cannot evict an unrelated workload's inode/dentry nodes;
  PR 3 carries the interference bench row. RAM bounded by the node-cache
  budget; optional per-ino window as R5 component `block_map_window`
  (floor 0). Co-writers resolve via a shipped `GetBlockMapRange` verb
  (the `XattrValueCap` pattern; schema bump) — Rev 1 (A8): replies carry
  the leaf-span (~8 k entries) around the miss, and the co-writer's map
  cache states its staleness law: own-custody ranges are authoritative
  from its own overlay; foreign ranges are the S5 reader-staleness class,
  SAFE by construction under the free-grace ring (a stale mapping
  resolves to a freed-but-not-reallocated block until the free-epoch
  ack), invalidated on free-epoch acks.
- **Rung-19/20 compose**: shipped entries become record Puts under the
  owner's conveyor — same-key ordering is the KV layer's existing law. The
  blob-compose apparatus (memos, `IndirectBlobGuard`, displaced-blob
  lists) becomes legacy-only and shrinks toward deletion (no-dead-code).
- **fsck / walkers**: ONE shared extraction arm (`kvmap:` heads scan the
  tree) used by C2/C8, `verify_durable_block_refs` (both oracle sides
  through the same path — the C8 law), the mount recovery walk, backfill,
  defrag, jobs. New class **C11 — map-plane consistency**: orphan map
  records (crossing residue / dead ino), run-vs-point coverage sanity.
  **Rev 1 (A3): C9's era floor is structurally inapplicable here** (a map
  record's ino may be years old while its crossing is live on THIS mount
  — C10's own reasoning); zero-FP instead rides the crossing's held 4a +
  an in-flight crossing registry (the C2/C3 `inflight_exempted` pattern:
  an incomplete pass records no verdict for a registered ino). **C11
  ships REPORT-ONLY** (the C8 posture) until both shields are pinned —
  a false-positive quarantine here would hole a live crossing.

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
- Ceiling: `(2³² − 1) × block_size` = **16 PiB at 4 MiB blocks** (was
  ~545 GiB; index `u32::MAX` is reserved by the refs MAP_BLOB sentinel,
  and the bound is enforced as an explicit EFBIG refusal — §6 A5).
  Practical bound = meta capacity: ~8 GiB of map leaves + ~13 GiB of ref
  leaves per 1 PiB file at 4 MiB blocks; runs collapse the map side
  ~len× for sequential data.
- Gauges: `block_map_tree_{records,puts,deletes,run_puts,leaf_reads,lookup_overlay_hits,lookup_tree_hits}`,
  `map_migrate_{inos,records,resumed}`, `publish_map_record_bytes`
  (`publish_indirect_blob_bytes` / `layout_indirect_map_reads` must trend
  to 0 on converted volumes), `block_map_window_bytes` (R5), must-stay-0
  `fsck_map_orphan_records`, `meta_ship_publish.map_{shipped,served,refused}`.
- Knobs (ENG-10 registry): `SQUEEZEFS_MAP_MIGRATE_CHUNK` (derived, floor
  64), `SQUEEZEFS_KVMAP=0` (A/B measurement lever), `SQUEEZEFS_MAP_WINDOW_{MB,PCT}`.

## 6. Rev 1 — adversarial review amendments (2026-09-01)

The review's verdict on Rev 0 was NEEDS-REVISION; every amendment below is
folded into §§2–5 and binds the PR ladder:

| # | Severity | Amendment (now normative) |
|---|----------|---------------------------|
| A1 | CRITICAL | Crossing prologue = chunked residue range-Delete of the ino's whole index range (kills the truncate-shrink-recross-extend stale-mapping resurrection — silent wrong data) |
| A2 | CRITICAL | Truncate/unlink = size-flip-first + durable sweep cursor in the head + background job-fabric sweep; never same-tx range Deletes (2²⁸ Deletes ≈ 5 orders past the entry cap) |
| A3 | CRITICAL | C11: era floor inapplicable (C10's reasoning); crossing holds 4a across the whole train; in-flight crossing registry exemption; REPORT-ONLY until both are pinned |
| A4 | HIGH | Served/co-writer crossing: whole-map ships as a RETRIED-class idempotent verb (dedup window); owner executes under held 4a; the 3.5 claim is local-publish-only |
| A5 | HIGH | Explicit EFBIG at `(2³² − 1) × block_size` (u32::MAX reserved by the refs sentinel); ceiling corrected to 16 PiB at 4 MiB blocks |
| A6 | MEDIUM | `RUN_LEN_MAX = 4096` + floor-by-bounded-forward-scan (no floor primitive exists); owner-ino prefix check; run-Put + point-Deletes one-tx coalesce law |
| A7 | MEDIUM | Tree-7 leaves enter the node-cache clock on probation; PR 3 carries the unrelated-workload interference bench row |
| A8 | MEDIUM | `GetBlockMapRange` leaf-span replies + co-writer map cache with the free-grace-bounded staleness law, invalidated on free-epoch acks |
| A9 | MEDIUM | Reader-side head+record resolution seqlock-bracketed; head-cache skew rule stated |
| A10 | LOW | `SQUEEZEFS_MAP_MIGRATE_CHUNK` registry max ≤ 1,024; `SQUEEZEFS_KVMAP=0` governs new crossings only (kvmap-head resolution can never be disabled) |

## 7. Rev 1.1 — PR-2 implementation-map addenda (2026-09-01)

The pre-implementation code map surfaced five design-level facts that bind
PR 2 (line anchors drift — anchor on symbols):

1. **The held-4a train needs its own backend seam.** `DlmLockManager`
   stripes are non-reentrant (re-locking self-deadlocks), so the crossing
   train cannot compose existing verbs (`commit_block_refs` /
   `set_layout_and_size` each acquire their own 4a). The train is a
   backend-internal method (`migrate_block_map_train`) taking
   `lock_inode_exclusive(ino)` ONCE and running every chunk tx with
   `hold_guards(clone)` — the M7 guards-co-ownership law; the `routed_*`
   guards-parameter precedent. Shipped inos: the co-writer ships ONE
   RETRIED-class `MigrateBlockMap` verb (FreeBlocks dedup-window
   template); the OWNER runs the train under its own 4a + serve stripe
   (the finding-36b whole-claim-set law).
2. **Derived recovery must learn tree 7 IN PR 2** (not PR 4): on a bit-9-
   absent volume the mount recovery walk derives owned blocks from layout
   heads — a `kvmap:` head would yield zero and gap-completion would
   free-list LIVE data. Either the tree-7 walk arm lands in PR 2 or kvmap
   crossings hard-gate on `block_refs_engaged()` (bit 9).
3. **Both C8 oracle sides learn tree 7 in PR 2** (fsck census + derived
   walk through the shared extraction), or every kvmap ino reads as drift
   on a healthy volume — PR 2's own acceptance gate demands it.
4. **The read-minimal set is mandatory in PR 2**: cache eviction +
   refetch on a `kvmap:1` head must resolve via the tree (a `block_map:
   None` head would zero-read). Unlink/`delete_file` of a kvmap ino needs
   at least a bounded synchronous record sweep until A2's job-fabric
   sweep lands (never silent residue).
5. **kvmap heads are STICKY** (decision): a shrinking map never collapses
   back inline — collapse would need its own sweep tx and buys nothing
   (the head is ~100 B either way). Pinned by test in PR 2.

## 8. Rev 1.2 — PR-3 read-map addenda (2026-09-01)

The PR-3 pre-implementation map's design-level facts:

1. **The window cache is a HYBRID**: materialized leaf-span ranges (one
   `block_map_range` fill ≈ 4–8 k entries ≈ 16–32 GiB of data at 4 MiB
   blocks — restores the O(1) probe), probed sync + lock-free. Point-
   through alone is impossible: two consumers are contractually SYNC
   (the il §5.5.1 fast path and the read-lane coverage screen) — without
   a sync-probeable store, every kvmap warm il read DEMOTES to the async
   handoff (the 1 M-IOPS engine structurally off) and lane coverage
   reads UNCOVERED. Per-ino span cap (1–2, LRU) ⇒ ≤ ~512 KiB per
   random-access PB file; R5 component `block_map_window` (floor 0,
   weight 2, drop-at-will — clean derived cache).
2. **No R2/lane map prefetch**: one leaf spans 16–32 GiB of data, so a
   stream crosses a leaf every several seconds at multi-GB/s and the
   window's miss-fetch runs naturally ahead of the data pipeline; a
   `block_map_window_misses`-class counter adjudicates ever pricing an
   explicit next-window prefetch.
3. **A7 probation is one line**: `CachedNode.ref_bit` initializes true —
   demand-loaded tree-7 LEAVES construct with it false (no second chance
   until a second touch); interior/pinned behavior untouched.
4. **A9 bracket is armed-readers-only**: write mounts stamp
   `UNARMED_EPOCH` and the one-KvTx head+records commit + 3.5/4a
   serialization + the existing rebind/currency ladder already own
   racing-publish skew — pinned no-bracket byte-identity on writers.
   Window entries stamp their load epoch (the `publish_stamped` law).
5. **Cold map amplification IMPROVES** (≈ 256 KiB leaf vs the 4 MiB blob
   per 16 GiB file); the risk is per-lookup latency shape, and the
   PR-1 merged `LOOKUPS` counter must split (exact / range / overlay
   hits; `leaf_reads` = tree-7-attributed node-cache misses) or §5's
   gauges are un-derivable.
6. **Map-window invalidation needs its own epoch-step drop arm** (the
   R-6 purge sink is block-key-addressed); co-writer foreign-range
   windows also drop at the free-epoch ack promotion point — BEFORE the
   ack rides the renewal, keeping the acknowledged-label safety argument
   true of the map cache. The co-writer cache lands in PR 3 with a
   pluggable fill (local S5 read now; the PR-5 `GetBlockMapRange` verb
   later).
