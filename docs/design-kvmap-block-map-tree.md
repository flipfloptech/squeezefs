# Design: PB-class file support — the `TREE_BLOCK_MAP` KV tree (finding 42)

**Status: DRAFT Rev 1.9** (design phase 2026-09-01; Rev 0 drafted by the
f42 planning pass; Rev 1 folds the adversarial review's amendments — §6 —
whose three critical findings supersede the corresponding Rev 0 clauses;
Rev 1.7 records PR 5b's landed laws — §13, numbered past dev's Rev 1.6
PR-6-split section; Rev 1.8 records PR 6a's landed laws — §12a; Rev 1.9
records PR 6b's landed laws — §12b. Do not start the PR ladder
before §6's A1–A5 are reflected in PR 1/2 scopes). The implementation is
the `feat/kvmap-*` PR ladder in §4.

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

## 9. Rev 1.3 — PR-2 landed deviations (2026-09-01, normative)

PR 2 (`708ab67e`) landed with three deliberate deviations, each test- or
safety-forced:

1. **Co-writer NEW crossings do not ship** — a shipped `MigrateBlockMap`
   from the routing gate let the wire self-arm bit 16 on the owner and
   flipped every mw fleet's blob lifecycle by default (three pinned
   blob-lifecycle contracts failed). The landed gate: a NEW crossing
   engages only when the publish is LOCAL to an engaged volume; a
   co-writer follows the STICKY head (once the authority crossed the
   ino, force-kvmap saves ship the verb — witnessed, era-gated,
   owner-ratcheted, owner-side f38 chunking, all as designed).
2. **Fetch rehydrates the FULL tree-resolved map into `CachedMetadata`**
   (not `block_map: None` + a per-index bridge): the write side treats
   the RAM map as whole-map authority — a `None`-map refetch would make
   the next save's diff MASS-DELETE live records. Consequence: write-
   side map RAM is O(file) for now (~10 MB at the 545 GiB class —
   acceptable; ~13 GB at PB class — NOT), so **PB-class write-side map
   boundedness is a new ladder item** (fold with A2's sweep work /
   PR 6): the save diff must become overlay/delta-based before PB files
   are writable in practice. PR 3's read windowing stands unchanged
   (read-only opens and eviction/refetch on non-writing mounts are
   where the window bounds RAM).
3. **PR 2 emits STRING map records only** (router-true key strings
   verbatim); POINT encoding is the PR 3/6 economy item, decoded
   support already whole.

## 10. Rev 1.4 — finding 43 + PR-3 landed deviations (2026-09-01, normative)

**Finding 43 (fixed `fc0549aa`)**: PR 2's crossing gate pre-probed
`block_map_tree_engaged` — circular, since only the train behind the gate
stamps the bit and no offline verb exists: kvmap was UNREACHABLE on every
real mount (caught by the first live smoke: 1,324 blob saves, zero map
records). The normative law is what §2 always said — the bit-5
stamp-at-first-use ratchet: a local new crossing SELF-ARMS via the routed
`block_map_tree_ready(ino)` probe at the decision point, before any state
is consumed; ratchet failure keeps the legacy blob arm; untouched volumes
stay byte-identical. Live-verified: self-arm + legacy→kvmap conversion of
3 blob-headed inos, crc32c-verified reads, remount byte-identity, oracle
drift 0.

**PR 3 (`e424ba0a`) deviations:**

1. **Incarnation-era keys keep STRING records** (a real §2 gap): with
   bit 13 engaged (the default format), `persist_block_key` output is not
   a pure function of `(vol_tag, offset)` — an 18-byte POINT cannot carry
   the lifetime stamp, and re-attaching the CURRENT stamp at decode would
   let a freed-and-reissued offset pass the staleness refusal. POINT
   engages on disengaged eras and on any exactly-round-tripping key
   (pinned); a stamped-key-capable compact form joins PR 6's value work.
   Measured shrink where POINT engages: 35.9 % of map-record bytes.
2. **A5 EFBIG pre-existed** via the `max_file_size()` screen (its bound
   IS the u32 index law); PR 3 pinned the kvmap-class boundary contracts
   (last legal index `u32::MAX − 1` round-trips; minting `u32::MAX`
   refuses with zero map-plane side effects) and corrected §5's ceiling
   arithmetic comment (16 PiB − one block at 4 MiB blocks).
3. **The §8 overlay-hit gauge is deferred** to the map-RAM-boundedness
   rung: Rev 1.3 #2's whole-map fetch has no per-index overlay-vs-tree
   decision point to count.

## 11. Rev 1.5 — PR-5 map: the serve-arm hazard table (2026-09-01)

The PR-5 pre-implementation map classified every serve arm a kvmap ino
can reach on the MULTI-WRITER planes. Three are SILENT-WRONG on the
PR-3-era tip and gate any mw fleet run until PR 5a closes them
(single-writer mounts never reach any of them):

| Arm | Today on a `kvmap:1` head | Class |
|---|---|---|
| Authority-LOCAL episode compose (`fetch_durable_layout_head`) | reads the head as an EMPTY map (`block_map: None` by format law) → the compose diffs claims against emptiness → mass-deletes live tree-7 records | SILENT-WRONG (worst — no wire needed) |
| `custody_scoped_layout` (S11 scoped Put) | kvmap head falls through the undecodable-base "legacy verbatim" arm → head regressed to the shipper's stale inline map, tree records orphaned | SILENT-WRONG |
| Chained shipped `MergeLayoutAndSize` | `head_indirect` gates on the word "indirect" → stages a versioned delta ONTO the delta-ineligible kvmap base → delayed read-side poison (or the compaction arm refuses loud) | SILENT-WRONG (delayed) |
| `MigrateBlockMap` (sticky-head ship) | owner train correct for a sole current-base shipper; refuses loud under live range grants (f34) | correct / refuses-loud |
| Fetch/cache-refill | full tree rehydrate | correct |

Further PR-5-binding facts: (a) **the shipped train's diff is
delete-by-absence** — a second writer's stale whole-map ship would
mass-delete a peer's fresh bindings (Rev 1.3 #2 gone cross-writer), and
a diff-deleted binding the shipper never claimed mints NO ref release
and NO device free (the f36b leak class, kvmap twin owed: owner-side
recompute + `recomputed` on `MapMigrated`, schema bump). The law chosen:
**claims-scoped adoption for SHIPPED trains** (the f35 pattern — adopt
under claim_take, delete only under claim_release, never
delete-by-absence; the whole-map diff stays local-authority-only), with
a head-sentinel map generation as the belt. (b) The LOCAL train lacks
the f34 range-grant screen (serve-side only) — parity required. (c) The
existing s11-mpiio fleet row already sizes past the crossing on default
formats, so with f43's self-arm THE ROW IS the kvmap-compose field gate
— and on the PR-3 tip it would hit the empty-map compose (correctness
red). Do not run range-custody fleets on tips between PR 3 and PR 5a.
(d) `GetBlockMapRange`: co-writer READS already resolve locally under
the S5 staleness bound (safe via free-grace); the verb's load-bearing
face is the WRITE-side base refresh — measure the read side before
building it.

## 12. Rev 1.6 — the PR-6 split (2026-09-02)

The PR-6 pre-map splits the economy/scale rung into three, with four
structural findings:

- **PR 6a `perf/kvmap-runs-point2`** — RUN records + the stamped-capable
  compact value, COUPLED: incarnation stamps gate RUN exactly as they
  gated POINT (Rev 1.4 #1), so plain runs are near-inert on default
  formats unless the stamp-carrying forms (POINT2 = 26 B
  vol_tag‖offset‖incarnation verbatim — never re-attached; RUN2 iff
  consecutive-mint lane_seqs prove stride-1) ship together. Run
  detection at the routed train's post-encode seam; run-aware diff
  equality (run-Put + point-Deletes ride the existing train tx — the
  coalesce law for free); floor-scan on exact-miss + mid-run range
  starts; expansion at the TWO shared surfaces only. NO separate
  coalescer: every sticky-head publish re-runs the whole-map diff, so
  THE PUBLISH TRAIN IS THE CANONICALIZER (owed for real when 6c kills
  the whole-map diff). 1 PiB sequential: 65,536 run records ≈ 2.2 MiB
  vs 7.5 GiB of points (~3,500× fewer bytes; the crossing train drops
  to ~128 chunk txs). POINT2's byte win vs stamped STRINGs is
  MARGINAL for bare-offset keys (26 B vs ~18-30 B) — measure before
  committing; the record-count/structure win is what runs deliver.
- **PR 6b `feat/kvmap-sweep`** — the A2 background sweep:
  size-flip-first SETATTR (+`;sweep:K` head — the grammar and the C11
  exemption already exist), `JobType::KvmapSweep` with the
  cursor-head-scan KD-6 plan, the derived sync-vs-job handoff
  threshold (`size/block_size` vs `map_migrate_chunk()×K` — ~128 GiB
  at defaults stays synchronous), per-chunk ONE-tx law (map Deletes +
  ref releases + cursor advance; frees post-commit, RES-1). Main open
  design point: the unlink corpse's head-vs-destroy ordering (the
  sweep needs the cursor OR the C11 orphan census as its plan).
- **PR 6c `feat/kvmap-bounded-save`** (STRICTLY post-5b) — write-side
  map-RAM boundedness via the dirty-index overlay + claims-scoped
  LOCAL train (the `ref_changes` per-merge delta is the ready-made
  overlay seed; segmented windows are structurally invalid while the
  local diff is delete-by-absence). Until then: option (b)'s honest
  cap — a `kvmap_write_map_bytes` gauge + derived budget refusing
  over-budget giant write-opens loudly (ships with 6a/6b). Venue
  arithmetic: 24×16 GiB write-active ≈ 10 MB (trivial); one 1 PiB
  write-active file ≈ 13-26 GB (the blocker — and that shape is the
  S11 venue, itself post-5b).

Also: the routing-layer "incarnation not engaged" comment contradicts
Rev 1.4 #1 — verify bit-13 default engagement before sizing POINT2
(likely a stale comment); the pre-short-circuit O(map) inline-sizing
pass on kvmap saves is a free CPU hoist for 6a.

## 12a. Rev 1.8 — PR 6a landed (2026-09-02, normative)

`perf/kvmap-runs-point2` landed §12's first rung. The adjudications:

1. **The bit-13 flag RESOLVED: stamps ARE engaged on default formats.**
   `format_v3` stamps `MULTI_WRITER_FORMAT_BITS` (bit 13 included) since
   the rung-10b Phase-B flip, a write mount takes a durable term (bit 7)
   so `block_key_incarnation_engaged()` is true per volume, and
   `DataRouter::set_meta_backend` engages `engage_incarnation_keys` —
   every fresh mint carries a stamp. The routing-layer "nothing stamps
   bit 13 today (ruling D9)" comments were STALE (pre-flip) and are
   corrected. Consequence per §12's coupling law: **all four forms
   shipped** — RUN (kind 3, 22 B), POINT2 (kind 4, 26 B), RUN2 (kind 5,
   30 B; emitted iff the covered stamps are literally consecutive
   composed words — the emitter verifies every actual stamp, so decode
   arithmetic reproduces exactly what was minted).
2. **The run stride is the router's census, never stored**: records stay
   at the pinned §12 widths; `RoutedMetaBackend`'s `map_run_stride` hook
   (`vol_tag → chunk_size`, installed at `set_meta_backend` beside the
   PR-3 encoder) feeds emission, and the two shared expansion surfaces
   resolve per-index keys through `map_entry_block_key_at(entry, delta)`.
   Claims-scoped trains never emit runs (per-index by law).
3. **The floor law is take-last-COVERING, not take-last** (a §12
   sharpening the tests forced): a superseding point INSIDE a run's span
   — the read law's own legal shape — sits between the run and the
   query, so the A6 bounded scan keeps the last record whose
   `start + run_len > N`, at exact-miss (`get_block_mapping`, which now
   answers `(record_index, entry)`) and in the claims train's
   covering-run resolve alike. The ROUTED `block_map_range` prepends the
   covering run record VERBATIM for a mid-span `from_index` (record-true
   — no clipped synthetic record exists to fabricate, and clipping would
   need the stride the meta layer lacks); the per-volume primitive stays
   strictly record-true for the sweep/C11/diff paging loops, and the
   fetch loop gained the no-progress guard + below-cursor re-delivery
   skip.
4. **Diff crash-order law**: ops stage as GROUPS the chunk packer keeps
   tx-atomic where they fit (the A6 run-Put + covered-point-Deletes
   one-tx law); coverage-shrinking same-key Puts order AFTER the records
   that re-cover their tail, stale deletes last — every pre-flip
   intermediate state resolves each index to its old or new binding,
   never to absence.
5. **The claims×runs law (§12's "never partial-adopt")**: a changed take
   INSIDE a run adopts as ONE superseding point (§2's own overwrite
   law); a release inside a run — or a changed take AT the run's own key
   — DISSOLVES the run into per-index records (survivors as router-true
   STRINGs, exact records never clobbered, the run's own key staged
   last), and the next full local publish re-coalesces; a desired-side
   run on a claims train refuses loud. Ref takes ride an explicit
   adoption ledger (dissolve survivors mint no reference).
6. **fsck C11 gained arm (c)** — run-vs-point coverage sanity: a
   DIFFERENT-volume point strictly inside a run's span reports
   (report-only, A3-shielded, `fsck_map_run_foreign_shadows`);
   same-volume points inside runs are legal by the §2 read law
   (arithmetic-equal shadow and overwrite alike — indistinguishable
   without the router's stride census, deliberately).
7. **The §12 option-b cap shipped**: `kvmap_write_map_bytes` (Σ estimated
   resident RAM map bytes of write-touched kvmap inos, revalidated
   lazily) + the derived `mem_budget/16` share (the node-cache divisor;
   no floor, no knob) refusing over-budget kvmap merges **EFBIG** at the
   §5.3 merge seam (growth ops only; truncate/punch never refuse). EFBIG
   and not ENOSPC: the condition is the FILE's class against this
   mount's RAM, not storage. The read-open residency of a giant map
   (fetch rehydration) stays un-capped — that is PR 3's window / 6c's
   overlay, not this cap's charter.
8. **The economy hoist landed**: the sticky-kvmap probe now precedes the
   O(map) inline-sizing pass in the save body.
9. Gauges added: `meta_kv_block_map_run_puts`,
   `meta_kv_block_map_lookup_floor`, `kvmap_write_map_bytes` (+
   `kvmap_write_map_budget_bytes`), `fsck_map_run_foreign_shadows`.
   Contracts: `tests/kvmap_run_tests.rs`, the codec pins in
   `tests/kvmap_tree_tests.rs` + `tests/decoder_property_tests.rs`, the
   C11 (c) arm in `tests/kvmap_walker_tests.rs`. `map_migrate_records` /
   `preexisting` count RECORDS (runs collapse them — the engagement
   face).

## 12b. Rev 1.9 — PR 6b landed: the A2 background sweep (2026-09-02, normative)

`feat/kvmap-sweep` landed §12's second rung. The adjudications:

1. **Size-flip-first is O(1) by construction**: the over-threshold
   striped-kvmap shrink (`(old_size − new_size)/block_size >`
   `kvmap_sweep_threshold_blocks() = map_migrate_chunk() × 64` — derived,
   never a knob) bypasses the publish train entirely:
   `KvMetaBackend::kvmap_truncate_handoff` commits size + `;sweep:K`
   (K = the first removed index; `min`-composed with any live cursor, so
   the swept region only grows downward) in ONE two-record tx (pinned
   ≤ 2 journal entries against a ~4,100-record removed set). The RAM map
   prunes under the 3.5 guard with NO ref staging and NO frees — the
   durable record/ref/free work IS what defers. Below the threshold the
   PR-2 synchronous paths run verbatim (pinned).
2. **The chunk law**: `JobType::KvmapSweep` (authority-only, local-pool,
   `mover_scope` None — per-ino serialization is the chunk's own held
   4a) runs `kvmap_sweep_chunk` per task: ONE KvTx = record-true map
   Deletes + their BlockRefOp releases + the cursor-advance head Put;
   freed keys return for the router's purge + reclaim enqueue AFTER the
   guard drops (RES-1). The deletable floor re-derives from the CURRENT
   size under the held 4a per chunk. The chunk budget counts COVERED
   INDICES, not records — the red-first suite convicted the one-shot
   run dissolve at 178 KiB against the 128 KiB whole-entry cap, so a
   straddling/whole run consumes from the RIGHT: each tx re-Puts the run
   shortened by exactly the span whose references it releases (len 1
   collapses to the point form), keeping every committed intermediate
   state coverage-exact. The TERMINAL chunk clears the cursor in the
   same tx as the final Deletes.
3. **The corpse ordering (the §12 open design point — decided)**:
   `delete_file` on an over-threshold (or already-cursored) kvmap ino
   keeps the inode record + head ALIVE as a corpse — nlink 0,
   unreachable, `;sweep:0`, size 0 — probed off the durable head BEFORE
   `fetch_metadata` (the corpse path never pays the Rev 1.3 #2 whole-map
   rehydration). The handoff tx drains the pending accounting notes as
   releases; the torn-down rewrite epoch's displaced/shadow keys free
   inline (bounded — never durable records, so invisible to the sweep);
   staging teardown stays O(PRESENT). The job's terminal chunk performs
   the destroy (record + xattrs, one tx — the C9 destroy shape; no
   quarantine, it is a planned teardown). Reclaim and the mount corpse
   sweep WITHHOLD `destroy_inodes` for registry-marked corpses
   (destroying the head would orphan the records); the census/oracle
   already include `nlink == 0` layouts (the 2026-08-23 corpse-census
   correction), so the mid-corpse state reads drift-free — C8/C9/C10/C11
   all pinned clean mid-sweep.
4. **KD-6, both halves**: the live half is the `submit_kvmap_sweep`
   hook (`wire_kvmap_sweep_submit`, deduped on live jobs); the crash
   half is `adopt_kvmap_sweeps` at mount — one tree-7 owner SKIP-scan
   per volume, a job regenerated for every live cursor no record
   covers, corpse marks re-armed from `nlink == 0`. Chunk deletions are
   record-true and cursor-resumed, so the counted-run law holds across
   a crash: deletions sum to exactly the residue (pinned per boundary),
   never a double free.
5. **The write-during-sweep law (the cursor invariant)**: while
   `;sweep:K` is live, every record ≥ K is unreadable residue and no
   live record sits at/above K. RAM write authority EXCLUDES residue
   (the fetch rehydration and `fetch_durable_layout_head` both filter at
   the cursor); the whole-map train — which re-stamps the flip head's
   cursor from the DURABLE head, like the gen belt, so a mid-sweep
   publish can never lose the plan — bounds its diff scan below the
   cursor and runs the **extend barrier** when
   `cursor_floor = ceil(size/bs)` exceeds it: the re-exposed span's
   residue deletes + releases in chunked txs co-owning the held 4a
   (freed keys ride the caller's post-commit tail), and the cursor
   advances to the floor. The degenerate barrier (no growth) still
   dissolves the left-boundary straddler — a shrink-superseding diff Put
   would otherwise drop its tail's coverage with the references never
   released. Claims-scoped (shipped/co-writer) trains never barrier: a
   size-raising ship or a claim at/above the cursor refuses
   retried-class (`map_refused`), below-cursor claims compose with the
   cursor preserved verbatim — and the handoff itself is gated on
   `publishes_locally`, so a co-writer truncate plants the cursor on the
   OWNER when its shipped SETATTR executes there (the job is
   authority-only). A re-cross can never meet a cursor: cursors live
   only in kvmap heads and kvmap heads never regress (the sticky pin).
6. Gauges: `map_sweep_{jobs,chunks,records,resumed}` (stats JSON;
   `records` counts job chunks + publish-barrier absorptions);
   `fsck_map_orphan_records` pinned 0 across mid-sweep passes (C11's
   orphan and empty-head arms both read live-record + kvmap-head /
   cursor-exempt states as healthy — no fsck change was needed, pinned).
   Contracts: `tests/kvmap_sweep_tests.rs`.

## 13. Rev 1.7 — PR 5b landed: kvmap multi-writer support (2026-09-02, normative)

PR 5b (`feat/kvmap-mw`) replaced PR 5a's two loud refusals with real
support. The landed laws:

1. **Claims-scoped SHIPPED trains (§11 law b, built)**: the owner-executed
   train for a `MigrateBlockMap` whose DURABLE base is a `kvmap:` head
   adopts a Put only under the shipper's take claims (its whole refs
   frame — a shipped save carries it un-chunked, f36b) and deletes only
   under an explicit release-without-take absent from the shipped map
   (the f35 removal law) — **never delete-by-absence**. The whole-map
   diff survives exactly where RAM is whole-map authority: local solo
   saves, the episode-compose window (serve-window exemption), and
   ESTABLISHING trains (crossing/conversion — the tree is empty, the
   diff is vacuous). Claimed adopt candidates pass the f28 live-binding
   probe; a stale shipper's size never regresses a peer's growth (the
   f35 size law on the claims arm).
2. **The map-generation BELT**: the head sentinel grammar is now
   `kvmap:1[;sweep:K][;gen:N]` (`gen` omitted at 0 — pre-belt heads and
   solo volumes stay byte-identical; `gen:0` and out-of-order segments
   refuse). Every committed train on the MW PLANE bumps it — claims
   trains, any train on a custody-armed authority, and any head whose
   gen was ever minted (monotone across disarm/remount); solo mounts
   never mint one (dark by default). A shipped train whose `base_gen`
   (schema 12's new verb field; the routing arm parses it from the
   cached head id and re-stamps from every `MapMigrated.gen` reply)
   mismatches the durable head refuses RETRIED-class ("layout delta base
   unusable: kvmap map generation", `map_refused`) under the train's
   held 4a; the client-side error arm re-learns the head id from the
   local reader view (head-id-only patch — the RAM map stays this
   mount's write authority).
3. **The f36b recompute twin**: with the rung-19 resolver armed, a
   claims-scoped train REPLACES the shipper's non-map-blob frame with
   the tree→composed swap diff (a stale frame legitimately MIS-NAMES the
   displaced binding), stages it in the flip tx (over-cap loads chunk
   in-train, refs-only txs co-owning the held 4a — f38's law), and the
   authority frees the TRUE displaced set via
   `free_recomputed_releases` strictly after commit Ok.
   `MapMigrated` gained `recomputed`/`released`/`gen` (schema 11→12,
   KD-7 same-commit fleets); `meta_ship_publish.map_recomputed_releases`
   is the engagement gauge; the caller stands its displaced-free ship
   down on `recomputed` and purges local tiers only (routing's "a kvmap
   publish is never owner-recomputed" shortcut deleted). Unarmed
   (no-resolver) trains keep the caller frame byte-identical and the
   caller keeps its free stream — the f36 preservation arm. In-frame
   TRANSIENT displacements (took-and-released between publishes) follow
   the accepted scoped-Put semantics (final-state diff) — a shared
   pre-existing class, not widened here.
4. **The S11 ∘ kvmap scoped compose (item 3, replaces §11 row 2's
   refusal)**: a scoped `SetLayoutAndSize` meeting a kvmap DURABLE head
   composes over the TREE-RESOLVED map under the claims law — claims
   custody-scoped to the holder's spans minus demoted regions (whole-file
   and grant-free shapes compose span-unfiltered; the finding-34
   custody-less-against-grants class keeps its refusal on
   `unscoped_put_refusals`) — and persists via the claims-scoped train
   (sticky durable head id, the train re-stamps the gen); the reply stays
   `PutDone{recomputed}`. An indirect shipped side rehydrates through the
   rung-20 hook. **Remaining refusals** (retried-class, `map_refused`): a
   Put SHIPPING a kvmap head over a non-kvmap durable base (sticky heads
   never regress — a stale/foreign frame), a kvmap-headed
   `SetLayoutAndSize` at all (no product path mints one — sticky saves
   ship the train), an undecodable legacy/JSON ship over a kvmap base
   (the retired "legacy verbatim" arm was §11 row 2's clobber), and §11
   row 3's chained merge onto a kvmap base (unchanged — the shipper
   refetches and re-ships the train).
5. **f34 retirement for kvmap (item 4, gated on the s11-shaped contract —
   green)**: the sticky-head SERVE refusal lifted for kvmap bases (a
   range holder's whole-map ship runs the claims train, span-scoped); the
   LOCAL-train screen lifted the same way (outside the compose window,
   live grants ⇒ claims-scoped from the save's own refs frame). **Kept**:
   a NEW CROSSING under live range grants still stands down to the blob
   arm (caller gate) and a direct whole-map crossing train under live
   grants still refuses — flipping the head mid-episode takes whole-map
   authority nobody arbitrated. Acceptance:
   `a_range_granted_kvmap_ino_composes_two_scoped_writers_and_the_episode_publish`
   (two scoped writers + the authority's episode compose, all landing,
   oracle clean).

Gauges added: `meta_ship_publish.map_recomputed_releases`. Contracts:
`tests/kvmap_mw_hazard_tests.rs` (items 1, 3, 4),
`tests/mw_cowriter_free_tests.rs`
(`a_skewed_kvmap_ship_frees_the_true_displaced_set_on_the_authority` —
item 2), `tests/kvmap_tree_tests.rs` (the gen grammar).

## 14. Rev 1.9 — the PR-6c map: bounded maps, both sides (2026-09-02)

The 6c pre-map's verdicts (normative for the remaining rungs):

- **S1: the §8 window cache was never built** — every mount full-rehydrates
  a kvmap map at open. 6c owes the bounded READ-residency story too, or a
  PB read-open OOMs regardless of the save-side work. The §8 window and
  the 6c overlay land as ONE structure: `CachedMetadata.block_map` becomes
  the bounded dirty-overlay-plus-warm-spans store with a `partial` marker
  and TOMBSTONES (a partial map's read-through must never resurrect a
  displaced-but-unswept tree binding), gated by the knob-less mode
  heuristic at the 6a cap's own seam — whole-map RAM below `mem_budget/16`
  (every real fleet file today), overlay past it (the PB class), EFBIG
  deleted. Reads: `PartialMiss ≠ Hole` at every resolve arm (the
  silent-wrong bomb rows); the anomalous-entry refetch arm becomes the
  legitimate bounded tree-resolving arm with warm-span retention.
- **S2: PR 5b built ~70 % of the train** — 6c's local save ships the
  overlay as a claims-scoped train (entries = dirty indices, claims = the
  refs frame; `base_gen: None` — local trains serialize on the held 4a).
  Displacement discovery moves WHOLLY to the train's f36b recompute
  (option ii): the merge stops capturing prev-bindings, tier purges ride
  the publish tail over `released`. Two train breaks to fix first: the
  claims arm's `current` scan is itself O(map) (must become
  claims-bounded probes), and the gen-bump keys on claims-presence (a
  solo mount's local claims trains would mint gens — split the criterion;
  the solo-dark pin governs).
- **The coalescer replacement** (for §12a #6's law, which 6c deletes):
  rung 1 — the LOCAL claims train may emit runs over its OWN contiguous
  takes (the shipped per-index law exists for partial-adopt hazards a
  held-4a local train doesn't have); rung 2 — a windowed canonicalizer
  chunk in the 6b job family (cursor-resumable, never whole-map), gauged
  by a record-count-vs-ideal instrument, never a knob. A full-republish
  trigger is banned (it re-requires whole-map RAM).
- **Sequencing**: 6b is a HARD prerequisite (overlay-mode truncate must
  never refuse — it size-flips into the sweep). 6c splits: 6c-i =
  mechanism (partial semantics, mode flip, read-through, overlay saves;
  the B8/B9 train fixes first as their own gateable step); 6c-ii =
  economy (the coalescer rungs) — adjudicate at PR 7 whether the PB
  soak tolerates points-not-runs if 6c-ii defers.
- Crossing + sub-threshold kvmap inos + legacy arms keep whole-map RAM
  verbatim (pinned); an `SQUEEZEFS_KVMAP_OVERLAY=0` A/B lever (registry
  entry) is sanctioned for the acceptance brackets only.
