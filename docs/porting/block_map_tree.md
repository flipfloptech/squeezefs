# Porting spec — the block-map tree (kind 7, "kvmap") for PB-class files

Scope: charter §5.6 "The block-map tree" (`docs/charter.md:185-189`), the kind-7 key and values of §5.1
(`:110`), the striped layout of §6.1 (`:203`), lock order (3.5) (`:50`). Reference `R/` =
`/Users/joberdorf/Source/squeezefs`; port `P/` = `/Users/joberdorf/Source/squeezefs-zig`. The reference's
authoritative design record is `R/docs/design-kvmap-block-map-tree.md` (Rev 2.0, 2026-09-02; cited as
`D:§n` / `D:line`). `R/docs/design-mw-layout-versions.md` carries no kvmap-specific clause (grep
`kvmap|indirect|block_map_id` → 0 hits); its version-pair law is already in `P/src/data/layout.zig`.
`R/src/meta_backend/kv/indirect_map.rs` is the LEGACY blob arm (`indirect:` heads, `:1-27`) that the tree
superseded; the port does not carry it (`P/docs/porting/data_layouts.md:150`, Q-6).

Law (0002): the charter's mechanisms are requirements; the reference's incidental choices are not. §8
lists every quirk met here with the better design.

---

## 0. The problem, the verdict, the numbers (D:§0–§1, §5)

- Inline map ceiling: the layout record spills when the encoded map exceeds
  `xattr_value_cap − LAYOUT_INLINE_HEADROOM` (`R/routing.rs:10796-10801`; headroom `= 4096`, `:660`) —
  ≈ 60 KiB ≈ 6–8 GiB of file at 4 MiB blocks (D:14-17). In the port the cap is an ENTRY count:
  `inlineMapCap = (xattrValueCap(node_size) − (1 + "layout".len + 32 + 4)) / 48` (`P/src/data/plane.zig:711-715`)
  = `(65,536 − 43) / 48 = 1,364` entries at 256 KiB nodes ⇒ ≈ 5.33 GiB per file today
  (`MapEntryWire` 48 B, `P/src/data/layout.zig:169-179`).
- Reference's pre-tree ceiling: a single CoW blob block ⇒ `block_size²/30` ≈ 545 GiB (D:20-22); every
  publish rewrote the blob (O(file)); delta-ineligible heads; whole-map RAM per open (D:22-31).
- Verdict: one record per mapping in a first-class KV tree keyed the other way from the refs tree
  (D:35-43). Ceiling `(2³² − 1) × block_size` = **16 PiB − 4 MiB at 4 MiB blocks**; index `u32::MAX`
  reserved by the refs `MAP_BLOB` sentinel, enforced as EFBIG (D:189-192; A5 `D:215`).
- Practical bound: ≈ 8 GiB of map leaves + ≈ 13 GiB of ref leaves per 1 PiB file at 4 MiB blocks
  (points); runs collapse the map side ~len× for sequential data (D:193-195). 1 PiB sequential ⇒ 65,536
  run records ≈ 2.2 MiB vs 7.5 GiB of points (~3,500× fewer bytes) (D:408-410).
- Crossing cost: unchanged below the cap (zero below it, byte-identical) (D:187-188).

---

## 1. Value forms, the read law, floor lookup, range iteration

### 1.1 Key (already v4 in the port)

`owner ‖ 07 ‖ block_index u32 BE` = 13 B (`P/src/meta/key.zig:160-161`, `BlockMapKey:283-320`). Reference:
`owner_ino u64 BE ‖ block_index u32 BE` = 12 B, kind byte carried by the tree id (`R/kv/block_map.rs:73,
141-152`). `block_index == u32::MAX` is RESERVED: refused at encode AND decode (`R:141-147, 166-171`;
`P/key.zig:172-176, 292-294, 318`); it appears only as a range END bound (`index_range_from`,
`R:180-191`; `P/key.zig:419-426` `indexRangeFrom`). The bound is exact: every legal index of one ino is
inside, nothing of `ino + 1` is (`R:177-179`).

### 1.2 Values (port codec landed: `P/src/meta/record.zig:370-521`)

`version u8 = 1 ‖ kind u8 ‖ payload`, little-endian (`R:28-36, 78`).

| kind | byte | payload | len | emitted when |
|---|---|---|---|---|
| POINT | 1 | `vol_tag u64 ‖ offset u64` | 18 | undecorated, UNSTAMPED binding (`R:80-82`) |
| STRING | 2 | decorated block-key bytes verbatim | 2+n | `damaged:` markers, packed tenants, encoder-less arms (`R:84-86`; D:68-70) |
| RUN | 3 | POINT ‖ `len u32` | 22 | `2 ≤ len ≤ RUN_LEN_MAX` straight span, unstamped (`R:87-93`) |
| POINT2 | 4 | POINT ‖ `incarnation u64` | 26 | stamped binding; stamp VERBATIM, never re-attached (`R:94-101`) |
| RUN2 | 5 | RUN ‖ `start_incarnation u64` | 30 | stamped span whose stamps are LITERALLY consecutive (`R:103-109`) |

Refusals (every one `Corrupt`, loud): unknown version (a newer binary's mapping — "refusing to guess where
the block lives", `R:340-346`); unknown kind (`R:432-434`); wrong length per kind (`R:349-355` etc.);
`len ∉ 2..=4096` (`validate_run_len`, `R:314-327`: a 1-run is a point — one spelling per mapping or the
record-equality diff churns; a run past `RUN_LEN_MAX` is INVISIBLE to the floor probe); POINT2
`incarnation == 0` (stamp 0 = unstamped rides POINT; two spellings poison the diff, `R:386-396`); RUN2
`start == 0` or `start + len − 1` overflowing (the emitter verified every ACTUAL minted stamp, `R:414-423`).
Port: identical gates `P/record.zig:463-521` (`RUN_LEN_MIN = 2`, `RUN_LEN_MAX = 4096`, `:388-392`).

WHY the stamp is carried verbatim (Rev 1.4 #1, D:338-346): with incarnation keys engaged (the default
format) `persist_block_key` is not a pure function of `(vol_tag, offset)`; re-attaching the CURRENT stamp
at decode would let a freed-and-reissued offset pass the staleness refusal. Measured shrink where POINT
engaged over STRING: 35.9 % of map-record bytes (D:345-346). POINT2 vs stamped STRING is MARGINAL
(26 B vs ~18–30 B); the win is record COUNT via runs (D:410-412).

The run STRIDE is never stored: it is the owning data volume's block size, resolved by the router's
census (`map_run_stride: vol_tag → chunk_size`, `R/meta_backend/mod.rs:942, 1420-1425`; D:454-459);
a per-index binding of a run is `offset = start_offset + stride × delta`, `stamp = start + delta`
(`R/routing.rs:4248-4272` `map_entry_block_key_at`). Claims-scoped (shipped) trains never emit runs —
per-index by law (D:459; `R/mod.rs:5994-6004`).

### 1.3 The read law (D:72-75, §12a #3)

**An exact-match record at N supersedes a covering RUN.** Overwrites are ONE Put, never a hot-path run
split; a later publish re-canonicalizes. `get_block_mapping(ino, N)` = exact lookup first
(`block_map_exact`, `R/kv/backend.rs:13943-13966`, counter `META_KV_BLOCK_MAP_LOOKUP_EXACT`), then on a
miss the floor probe (`:13925-13934`). It answers `(record_index, entry)`: an exact hit answers `(N, e)`,
a covering run answers `(start, run)` and the caller derives the per-index binding from `N − start`
(`:13915-13919`).

### 1.4 The floor lookup = ONE bounded forward scan (A6, D:125-129, 216; `R:13968-14033`)

The tree has no predecessor primitive, so: on `N == 0` return none (no index precedes 0; `:13984-13987`);
otherwise `range([ino‖N−(RUN_LEN_MAX−1), ino‖N−1])`, paged forward 512 at a time (`:14004-14006`), keep
the LAST **covering** record — `idx + run_len > N` (`:14021-14026`) — NOT the last record plain: a
superseding point INSIDE a run's span (the read law's own legal shape) sits between the run and N and
taking it would read a covered index as absent (`:13994-14000`); take-last-among-coverers is also what
the mid-train crash-window needs (where two runs transiently overlap under the shrink ordering, the later
start is the newer truth, `:13998-14001`). Owner-ino prefix re-checked on every record, loud on mismatch
(`:14013-14019`). `RUN_LEN_MAX` bounds the scan BECAUSE it bounds every emitted run (`:13974-13975`);
counter `META_KV_BLOCK_MAP_LOOKUP_FLOOR` (`:13988`). Cost on a point-dense map ≈ 4 k records
(`D:829-830`) — which is why the finding-46 window train probes exact-only (§3.1).

### 1.5 Range iteration (reads, readahead, walkers)

- Per-volume primitive `block_map_range(ino, from, max)` (`R:14043-14071`): `range_kind(TREE_BLOCK_MAP,
  lo, hi, max)` over `index_range_from`'s exact bounds; counts `LOOKUP_RANGE` (ops) and `RANGE_RECORDS`
  (records returned — the "tree reads per publish" instrument finding 46 needed, `:14057`; `R/kv/mod.rs:
  566-574`). Strictly record-true (the sweep / C11 / diff paging loops rely on it, D:468-470).
- Routed `block_map_range` (`R/mod.rs:5930-5960`) PREPENDS the covering run VERBATIM when `from_index`
  lands strictly inside a preceding run's span (one floor probe, `:5946-5958`) — record-true (no clipped
  synthetic record exists; clipping needs the stride the meta layer lacks, D:466-469). Continuation
  loops resume at `last key + 1` and may be re-delivered the same run; their per-index overlay is
  idempotent and a page whose every record precedes the cursor means DONE (the no-progress guard,
  `:5924-5929`).
- The shared extraction (`kvmap_layout_entries_complete`, `R/routing.rs:4503-4554`): pages of 512, runs
  expanded through `map_entry_block_key_at(entry, delta)`, insert order == key order so a later exact
  record inside a run OVERRIDES the run-derived binding (the read law "rides record order", `:4508-4512`);
  completeness flag `false` on a scan error or an unresolvable tag (a walker treats partial as
  conservative; a REPAIR that judges against it must refuse, `:4496-4502`). BOTH C8 oracle sides, the
  recovery walk, the unlink enumeration and defrag ride it (D:150-154; Rev 1.1 #2/#3 D:238-244).
- Port today: `KvTree.range(start, end, max)` is latch-free leaf-by-leaf fold with
  `key.keySuccessor` resume (`P/src/meta/tree.zig:807-835`); 0002 row 176 says ino-major walks must resume
  with a same-kind successor — the kind-7 walk resumes at `blockMapKey(ino, last + 1)` (or the next ino)
  and decodes only kind 7.
- Node cache: tree-7 LEAVES demand-loaded enter the clock on PROBATION (§5.2); cold map amplification
  ≈ one 256 KiB leaf ≈ 8 k neighbouring entries ≈ 16–32 GiB of data per leaf (D:131-133, 259-261,
  282-284); no explicit next-leaf prefetch — a stream crosses a leaf every several seconds at multi-GB/s
  and the miss-fetch runs ahead of the data pipeline naturally (D:269-273).

---

## 2. The crossing (D:§3 "Crossing", A1/A3/A4, Rev 1.1, Rev 1.3, finding 43)

### 2.1 Trigger

The save path sizes the inline value (`encoded_len`, no buffer — PERF-8 `R/routing.rs:10784-10793`) and
`needs_indirect = map.is_some() ∧ inline_len > xattr_value_cap(owner) − LAYOUT_INLINE_HEADROOM`
(`:10796-10801`; the OWNER volume's cap — S9). Decision is a 3-way (`:10830-10845`): an existing `kvmap:`
head is FORCE-kvmap and is taken ABOVE the sizing pass (the PR 6a hoist, `:10736`, D:502-504); a NEW
beyond-inline crossing takes the tree iff `kvmap_enabled()` (`:1076-1078`) AND the arm can engage
(`kvmap_publish` returns `Ok(None)` = stood down, nothing consumed, legacy blob arm runs); else the blob.
Port: the trigger is the entry-count cap (`merged.items.len > inlineMapCap()` at `P/src/data/plane.zig:
1433-1437`, today `layout_kvmap_crossing_refused` + `Transient`) and the pre-screen at
`P/src/fuse/handler/kv_fs.zig:1468-1470` (`fuse_write_beyond_inline_map_refused`, EFBIG — the owed arm).
The port has no blob arm: the ONLY beyond-inline shape is tree 7, so "stand down" ⇒ refuse loud, never a
blob.

Engagement gate for a NEW crossing (`:10136-10156`): (a) the publish is LOCAL to the ino's home volume
(a co-writer never self-arms the owner, Rev 1.3 #1 D:300-308); (b) no LIVE range grants on the ino (a
head flip mid-episode would wedge every ranged peer, D:663-668); (c) `block_map_tree_ready(ino)` — the
bit-16 ratchet (`R/kv/backend.rs:14141-14217`): stamp the superblock bit under a mutex, barrier
(`sync_device`) BEFORE any map record can become durable (KD-14 bit-before-record), on a FOREST no root
to mint (a kind inside the slot trees, `:14163-14177`). Finding 43 (D:324-335): the gate must SELF-ARM
at the decision point — a pre-probe of "already engaged" was circular (only the train stamps the bit,
no offline verb exists ⇒ kvmap unreachable on every real mount: 1,324 blob saves, zero map records).
Port v4: `BLOCK_MAP_TREE` is one of four capability bits (0002 row 43); the ratchet is the same
stamp-at-first-use + barrier; "untouched volumes stay byte-identical" is the pin (D:333-334).

### 2.2 The train (`migrate_block_map_train`, `R/kv/backend.rs:14302-15125`)

ONE exclusive 4a I-guard for the whole train (`lock_inode_exclusive`, `:14322`; the stripes are
non-reentrant so no existing verb composes it — Rev 1.1 #1 D:226-234; the 3.5 stripe covers local
publishes only, 4a is what fsck's re-check and the served compose arms serialize against, A3/A4
D:100-104). Steps:

1. `write_gate` → `block_map_tree_ready` (`:14316-14319`; `None` ⇒ caller falls back / refuses).
2. Register the ino in the in-flight crossing registry (`CrossingGuard`, RAII, deregisters on every
   exit incl. errors, `:14321, 19440-19457`) — fsck C11's zero-FP shield (A3).
3. Read the DURABLE head under the held 4a (`:14332-14345`): its `gen` and `sweep_cursor` are the belt's
   and the cursor law's inputs. An establishing train (no kvmap head) structurally never meets a cursor
   (cursors live only in kvmap heads; heads never regress — sticky, `:14326-14331`).
4. **The A1 residue sweep generalized to a DIFF** (`:14253-14257, 14860-14972`): one paged scan of the
   ino's existing records (`block_map_range` from 0 in `chunk` pages, `:14886-14890`) staged against the
   desired map — Deletes for stale indices, Puts for new/changed bindings (a changed binding is ONE
   same-key Put, `:14905-14920`). On a healthy first crossing the scan is one empty leaf descent and the
   diff is the whole map. WHY (A1, CRITICAL D:91-96, 211): a crashed prior crossing's residue would
   otherwise resurrect as stale mappings after truncate-shrink → re-cross → extend — silent wrong data.
   The diff never reaches past a live sweep cursor (`:14896-14903`, §4.3).
5. Chunked commits of `chunk` ops per tx, each co-owning the guard (`hold_guards`, the M7 law,
   `:15092-15110`); `chunk = SQUEEZEFS_MAP_MIGRATE_CHUNK` default 512 (§6).
6. **The FLIP** as the final tx while the guard is still held (`set_layout_and_size_with_map_holding`,
   `:15111-15112`): head Put (`kvmap:1[…]`, `block_map = None`) + inode(size) Put + the refs tail + the
   tail map chunk — the one-tx shape (`:14260-14263`).

Diff crash-ORDER law (D:474-478; `:14550-14556, 14941-14971`): ops stage as GROUPS the chunk packer
keeps tx-atomic where they fit (a run-Put + the point-Deletes it covers ride ONE group, the A6 coalesce
law, `:14875-14879, 14947-14953`); a group past the chunk splits in order (Puts lead); coverage-SHRINKING
same-key Puts order AFTER the records that re-cover their tail (`shrink_keys`, `:14880-14884, 14914-14919,
14957-14968`); stale Deletes last (`:14969-14971`) — every pre-flip intermediate state resolves each index
to its old or new binding, never to absence.

Legacy-blob conversion (D:82-85; `R/routing.rs:10284-10318`) — not carried (no blob arm in the port).

### 2.3 What the flip changes in the layout record

Reference: `block_map_id = Some("kvmap:1[;sweep:K][;gen:N]")`, `block_map = None`, `file_type`, `size`,
`block_prefix`, `file_id`, `data_key` verbatim (`R/routing.rs:10330-10344`); the head carries the
caller's KNOWN gen (`base_gen` from the cached head, `:10324-10329`) and the train RE-STAMPS gen and
cursor from the DURABLE head (`:14498-14540`: gen 0 with no live cursor commits the caller's bytes
verbatim — the pre-belt byte-identity pin). Port (typed, landed): `Layout.stripedKvmap(size, sweep,
gen)` sets `F_MAP_KVMAP` (+ `F_SWEEP_CURSOR` / `F_MAP_GEN` presence flags, `P/src/data/layout.zig:
203-206, 461-472`); `validate` refuses `MAP_INLINE == MAP_KVMAP`, a cursor/gen on a non-kvmap striped
layout, and `MAP_GEN` with `map_gen == 0` (`:573-579`). The head is STICKY (Rev 1.1 #5 D:252-256): a
shrinking map never collapses back inline (collapse would need its own sweep tx and buys nothing — the
head is ~100 B either way); `validTransition` already makes striped terminal (`P/layout.zig:1293-1301`)
— add "kvmap ⇒ never `MAP_INLINE` again" to it.

### 2.4 Refs across the flip (port hazard)

Reference: the flip tx carries "this publish's BlockRefOps" only (`D:99-100`); the inline entries that
moved into tree 7 keep their references — a reference's identity `(vol_tag, block_idx, owner, index)`
is independent of where the MAP lives; no `MAP_BLOB` sentinel for kvmap inos (map records are metadata,
D:86-88). **Port:** `publishLayout` diffs `prev.named()` against `new.named()` and RELEASES every old
entry the new layout does not name (`P/src/data/layout.zig:1334-1366`), and a kvmap head "names nothing"
(`:522-524`) — so the flip as written would stage a `−ref` for every block of the file. The crossing must
bypass that diff: the flip's ref delta is exactly the publishing window's `+ref`/`−ref` (and the
`NamedIterator` of a kvmap head must be understood as "named elsewhere", not "named by nobody"). Make it
a typed arm: `publishLayout(.cross{ moved: n })` asserts `prev.entries.len == n` and stages no release.

### 2.5 Concurrency mid-crossing

- Writers: the whole train holds the ino's exclusive 4a; a concurrent publish on the same ino PARKS at
  the guard (ordinary 4a admission, charter §2 lock order) — never refused locally. Every chunk tx
  co-owns the guard (`hold_guards`), so the conveyor's pass applies it without re-acquiring.
- Reads: pre-flip records are INVISIBLE (the head still names the inline map, the resolve reads the
  head first, `:14265-14267`); the reader-side head+record resolution brackets with the reader
  revalidation seqlock on ARMED READERS only (A9; write mounts stamp `UNARMED_EPOCH`, zero epoch reads —
  D:277-281; `KVMAP_FETCH_BRACKET_RETRIES = 32`, `R/routing.rs:1207-1213`: the poll cadence is ≥ 1 s so a
  fetch loses at most one step per second; exhausting 32 means the epoch source is broken ⇒ loud).
- Shipped (co-writer) trains: `MigrateBlockMap` RETRIED-class idempotent verb, owner executes under its
  own 4a (A4); claims-scoped, never delete-by-absence (§3.3) — rung 6 material; the port's single-writer
  plane never reaches it.
- fsck during a crossing: the registry exempts the ino (`crossing_in_flight`, `R:14078-14080`;
  `counters.crossing_exempted`, `R/fsck.rs:5054-5057`).

### 2.6 Crash windows (D:14265-14273; `tests/kvmap_crossing_tests.rs:449`)

Every mid-train crash — mid-sweep, between Put chunks, chunk↔flip — leaves the SAME on-disk class: tree-7
records staged, head NOT flipped. Resolution: (a) the residue is invisible after remount (the head is
the truth); (b) the next crossing's A1 diff reconciles it, including a stale record at an index BEYOND
the final map; (c) fsck C11(a) reports it (owner has a non-kvmap head) report-only. On a RE-train of a
live `kvmap:` head: pre-flip Deletes remove bindings the punch/truncate already freed (deleting early is
the safe direction — a stale record naming a freed block is the A1 class) and pre-flip Puts expose
write-through bytes an unacked publish wrote (legal after a crash; the retried save re-diffs and
converges). The sticky sentinel + replay: the head Put rides the flip entry; replay folds every journaled
record by key into the (possibly fresh) tree root (`:14129-14132` "stamp-then-crash is inert").
Must-stay-0 after a clean run: `fsck_map_orphan_records`.

---

## 3. Writes on a tree-mapped file

### 3.1 A publish ships ONLY its window (finding 46, D:§18; the charter's law)

Field row (E2E baseline, squeeze-test, 24 fio × 8 GiB sequential 1 MiB direct writes, nvme-tcp):
**28.7 GB/s → 0.36 GiB/s** (~2,000×) from t+7 s when all 24 files crossed; clat 986 ms per 1 MiB write;
the REWRITE pass over the same kvmap files ran 32 GiB/s (D:794-800; `R/tests/f46_kvmap_stream_publish_
tests.rs:1-15`). Attribution: ≈ 2,830 post-crossing publishes paid 13,037 `block_map_range` pages
(≈ 4.6 pages ≈ the ino's whole ~1,500-record population per publish; `lookup_exact` 0), ~300 B of journal
each (the JOURNAL side was already O(window)), `publish_phase_ns` ≈ 275 ms mean, 128 ms–1 s tail; the
O(map) CPU (RAM-map clone + record encode + tree page + decode) ran on the 2-thread `sqz-meta` pool so
publishes serialized ACROSS inos too (D:800-812).

The law (D:814-820): a steady-state publish ships ONLY its window — tree operations proportional to the
blocks published, never to the file. The whole-map diff is legitimate at the CROSSING (the tree is
empty), on NON-publish saves that own deletes (truncate/punch/fsync-persist), and on a publish carrying
deferred accounting notes (Vector B: notes name bindings outside the window).

Mechanism (`MapTrainClaims{window: true}`, `R/kv/backend.rs:19425-19435`; `R/routing.rs:10181-10216,
10254-10264`): `take` = the window's indices, `release` = ∅, `entries` = the RAM map's CURRENT bindings at
exactly those indices (never the op's own key — RAM is whole-map authority), `base_gen: None`,
`served: false`. The train probes **exact-only** (`block_map_exact`, `:14614-14618`): with whole-map
authority behind it a miss is a fresh index or an index inside a run, and both stage the same superseding
point Put (the read law), so the ~4 k-record floor scan buys nothing (D:825-830). It NEVER recomputes
(`recompute_armed = claims ∧ !window`, `:14998-14999`): the caller's RAM merge captured every
displacement, so the frame commits verbatim and the caller keeps its displaced-free stream. A changed
take AT a run's own key still dissolves the run (D:833). Gate (`:10200-10208`): kvmap head ∧ !partial ∧
local ∧ no deferred notes ∧ no live range grants ∧ every window index binds in the RAM map; anything else
keeps its whole-map-class arm. Engagement gauge `kvmap_window_saves`; the tree-read face
`meta_kv_block_map_range_records` per publish must not grow with file size (`:10459-10465`).
Measured in the FUSE-level venue (two interleaved writers so allocations do not run-collapse): 511
publishes paid 268,914 range records before, 511 exact lookups and 0 range records after (D:844-848).

Records per window: a 64-block window ≈ 4 KiB of journal vs the 4 MiB blob DMA + barrier + full save it
replaced (D:109-111). The map Puts ride the SAME KvTx as the head Put and the refs (`stage_block_map`,
`R/kv/backend.rs:19139-19181`: one tx = one checksummed journal entry, "a mapping can never disagree with
the head/size that justifies it"); counters `META_KV_BLOCK_MAP_PUTS/RUN_PUTS/DELETES` tick there.

Port binding: `MapSeam.merge(MergeRequest{ino, inserts, min_size, proofs, fencing})` is the ONE
transaction under the stripe (`P/src/data/write_seams.zig:174-230`; `plane.mapMerge`, `P/plane.zig:
1404-1463`). For a kvmap head the merge stages one kind-7 Put per insert (+ its `+ref`), the `−ref` of a
displaced binding it finds by ONE exact lookup per insert (the displaced binding is also the RES-1 free
the caller owns, `MergeOutcome.displaced`, cap 64), and a head Put ONLY when size/gen/cursor change —
no range scan, no head rewrite per publish otherwise. `mapMerge` today returns `NotFound` on a kvmap
head (`:1420`) — the owed arm.

### 3.2 Write-side RAM: the `mem_budget/16` partial store (D:§12 opt. b, §14, §16)

- Rev 1.3 #2 (D:308-318): the fetch rehydrates the FULL tree-resolved map into the RAM entry because the
  write side treats the RAM map as whole-map authority (a `None` map + the delete-by-absence diff would
  MASS-DELETE live records). Write-side map RAM is O(file): ~10 MB at 545 GiB, ~13 GB at PB class.
- The budget share: `kvmap_write_map_budget_bytes = mem_budget / 16` — the node-cache default's divisor
  (`SQUEEZEFS_META_NODE_CACHE_*`, charter §5.3), NO floor, NO knob: a floor would ADMIT a map the budget
  cannot hold, and the smallest real budgets still admit multi-TiB classes (a 2 TiB map ≈ 33 MB)
  (`R/routing.rs:1086-1107`). Per-entry estimate `KVMAP_WRITE_MAP_ENTRY_EST = 64` B (one hash slot +
  String header + key body; an ESTIMATE by design — O(1) per merge, an exact Σ would re-add the O(map)
  pass the hoist removed, `:1109-1115`). Gauge `kvmap_write_map_bytes` = Σ over write-touched kvmap
  inos, registered at the one-merge seam (`note_kvmap_write_map`, `:17086-17103, 17234-17264`),
  revalidated lazily (`:17060-17078`).
- The 6a EFBIG refusal (a merge growing the estimate past the share refused EFBIG — "the FILE's class
  against this mount's RAM, not storage", D:493-501) was DELETED by 6c-i: the arithmetic is now the
  MODE-FLIP threshold (D:689-692, 749-751; `:1125-1140` `kvmap_size_over_budget(size, bs) = ceil(size/bs)
  × 64 > budget/16`, SIZE-based — deterministic from the head alone, no record walk). Live threshold on
  the field box: ≈ 5.15 GiB of MAP bytes ≈ a ~450 TiB file at 4 MiB blocks (D:780-782).
- PARTIAL mode (`R/routing.rs:9720-10060`): per-ino registry `kvmap_overlays: ino → BTreeMap<u32,
  Option<key>>` — presence IS the mode marker (`:9729-9731`); `Some(k)` = un-published binding, `None` =
  TOMBSTONE (a truncate/punch displaced the index; read-through must serve zeros and never resurrect the
  tree's stale binding, `:1155-1162`). Warm WINDOWS per ino ≤ 2 spans (one demand stream + one lateral,
  `:1142-1144`), each filled by ONE `block_map_range` page of 512 records (`:1146-1148`) clipped to 4,096
  expanded indices (a run-heavy page must not balloon the window; ≈ 256 KiB at the estimate,
  `:1150-1153`); keys back-to-back in ONE arena with `(index, start, len)` entries — a fill is two
  growable buffers, a probe a binary search (R-5 economy, `:1164-1197`). Span `hi` = `u32::MAX` when the
  page was short (tree exhausted above `lo`), else the last record's end (`:9929-9937`); a span stamps
  its load epoch, an epoch step invalidates it (`:1179-1182`). R5 component `block_map_window` (floor 0,
  weight 2, drop-at-will, `kvmap_windows_shed`, `:9860-9875`).
- Resolve ladder (`resolve_partial_block_binding`, `:10034-10060`): index ≥ sweep cursor ⇒ HOLE
  (shadowed residue, §4.3) → overlay verdict → window verdict (`Some(None)` = in-span hole is DEFINITIVE)
  → the TREE (one window fill, retained). `PartialMiss ≠ Hole` at every arm; an unresolvable record errs
  LOUD — never a fabricated hole (D:686-692). The il fast path probes overlay + window SYNC (lock-free)
  and a PartialMiss DEMOTES to the async handler (`:18295-18311`) — the §8 #1 reason the window store
  is a materialized span cache and not point-through (two consumers are contractually sync, D:258-267).
- Mode flips at CLEAN points only (fetch, post-publish republish, `:10494-10515`); a non-empty overlay
  pins partial regardless of size (`:1131-1133`); flip-down iff the overlay is empty
  (`kvmap_partial_disarm_if_clean`, `:9765-9775`). A partial merge mutates the OVERLAY, the scratch map
  stays empty (`:17286-17292`).
- The partial SAVE = overlay-only claims train (`overlay: true`, `R/routing.rs:10236-10253`): takes =
  dirty indices, releases = tombstones, bounded probes (one exact + floor per claimed index — "a 2-claim
  save on a 2,048-record ino pays 2 exact lookups where the tip paid 5 whole-map pages", D:751-753),
  displacement discovery WHOLLY in the train's recompute through the caller's live `ref_for` (option ii,
  D:697-700; `:14986-14999`). The shipped snapshot drains exactly (racing post-snapshot merges keep their
  entries — never-lossy, `:9824-9832`); windows drop on every local mutation (`:9844-9858`).
- Residual (D:850-852): the overlay train's per-claim floor probe is O(RUN_LEN_MAX) on a point-dense map.
- Port: `FileView.map_resident` already models "a kvmap head declines the in-place arms"
  (`P/src/data/write_seams.zig:147-155`; `plane.zig:1355` sets it `!isKvmap()`), and `device_overlay`
  declines on `!map_resident` (`P/src/data/device_overlay.zig:683`). The port never had a whole-map RAM
  authority for kvmap heads (its `mapMerge` reads the DURABLE record under the stripe), so the port's
  "window" is structural: every merge is O(inserts) exact lookups — Rev 1.3 #2's O(file) RAM and its
  delete-by-absence diff never exist. The partial store reduces to (a) the read side's warm spans
  (§3.2 window cache, sized per the derivation above) and (b) tombstones are unnecessary when deletes are
  durable in the same tx (the port's truncate stages the Deletes or the cursor; there is no un-published
  RAM delete to shadow a tree binding). Keep the `mem_budget/16` share as the WINDOW-cache budget's
  derivation (its WHY: the node-cache divisor; no floor).

### 3.3 Refs deltas for map-tree blocks

One record per REFERENCE in kind 6 rides the same tx as the map Put/Delete (charter §5.6; `R/kv/backend.rs:
14436-14439`, `:15310-15318`). The train's recompute in claims mode REPLACES the shipper's non-map-blob
frame with the tree→composed swap diff (f36b, `:14974-15067`): takes from the EXPLICIT adoption ledger
`ref_takes` (never from staged Puts — dissolve survivors mint no reference, `:14566-14571, 15031-15036`),
releases from `displaced`; `cancel_same_reference_pairs` cancels a run-start re-adopted as a point at the
SAME block (one reference described twice; a re-Put would strip the record's SHARED bit, C16,
`:15037-15058`). Refs chunk at `TRAIN_REF_TX_CHUNK = 512` in refs-only txs co-owning the guard, tail on
the flip (`:15069-15086`; `BLOCK_REF_TX_CHUNK = 512` on the routing side `R/routing.rs:10271-10282`). A
crash between a refs chunk and the flip leaves report-only C8 residue (space-safe, data-safe,
`:15073-15074`). C8's repair refuses a finding whose justification is "an incomplete kvmap walk"
(`R/docs/design-durable-block-refcounts.md:200-207`).

---

## 4. Truncate / unlink of giant files (A2, D:§3 "Truncate/unlink", §12b)

### 4.1 Why not same-tx Deletes

A 1 PiB truncate is 2²⁸ Deletes ≈ five orders past the 128 KiB whole-entry cap, and the synchronous
`truncate_layout` would materialize the removed set in RAM (A2, CRITICAL, D:113-116, 212).

### 4.2 Size-flip-first (`kvmap_truncate_handoff`, `R/kv/backend.rs:15168-15232`; router arm
`kvmap_truncate_sweep_handoff`, `R/routing.rs:22344-22536`)

- Threshold: `removed_estimate = (old_size − new_size) / block_size > kvmap_sweep_threshold_blocks()
  = map_migrate_chunk() × 64` (`:22388-22392`; `:1231-1233`) = 32,768 blocks ≈ 128 GiB at 4 MiB — DERIVED,
  never its own knob (the chunk knob is the one lever and the tests' A/B seam, `:1224-1230`). Below it,
  and on every non-kvmap shape, the synchronous path runs verbatim (`:22389-22392`; pinned
  `below_threshold_truncate_and_unlink_stay_verbatim`). A PARTIAL-mode shrink takes the handoff at ANY
  size (a bounded store cannot run the whole-map walk) and drains SYNCHRONOUSLY below the threshold
  (`:22381-22389, 22508-22528`).
- Stale custody never commits a size flip (`fencing_token < live_token ⇒ FencingTokenExpired`,
  `:22393-22401`).
- Under the 3.5 guard: bump the layout-prune epoch (in-flight delayed merges observe the cut and refuse),
  supersede shadow bindings past the cut, screen device overlays past the cut (`:22402-22417`); RAM map
  pruned with NO ref staging and NO frees for tree-mapped blocks (the durable record/ref/free work IS
  what defers) but tier PURGES still run (a reallocation under the same key must never serve stale tier
  bytes, `:22418-22428`); a partial entry prunes its OVERLAY instead (un-published mints above the cut
  are this mount's own — they free after the guard drops; tombstones above the cut drop,
  `:22429-22448`).
- `K = ceil(new_size / bs)` clamped to `u32::MAX − 1` (`:22464-22469`); the backend composes
  `cursor = min(existing, K)` under its 4a (the swept region only grows downward, `:15176-15179,
  15211`), rewrites `block_map_id` with the cursor and the gen PRESERVED (not a train — no bump,
  `:15179-15218`), `layout.size = new_size`, ONE two-record tx: head + inode size, no refs
  (`:15229-15230`; pinned ≤ 2 journal entries against a ~4,100-record removed set, D:521-524).
- Then: drop whole-file RAM snapshots (a later re-extend reads zeros, `:22493-22496`), retire overlays,
  free the pruned overlay's mints (RES-1), `map_sweep_jobs += 1`, `submit_kvmap_sweep(ino)`
  (`:22530-22535`).
- Call site: `truncate_layout` for striped files tries the handoff first (`:22564-22571`).

### 4.3 The background cursor-resumed sweep

- The cursor invariant (D:566-569): while `;sweep:K` is live, every record ≥ K is unreadable residue and
  no live record sits at/above K. Reads clamp (`resolve_partial_block_binding` returns HOLE at/above K,
  `R/routing.rs:10045-10049`; the fetch rehydration filters at the cursor, `:9703-9713`); the whole-map
  train bounds its diff below the cursor (`:14896-14903`).
- The chunk (`kvmap_sweep_chunk`, `R/kv/backend.rs:15259-15329`): under one held 4a, re-read the durable
  head (the plan IS the cursor — KD-6; a vanished record / cleared cursor ⇒ `NoCursor` = done,
  `:15273-15292`); `start = max(cursor, floor_of(CURRENT size))` (`:15293` — a write that grew the file
  back past the cursor never has its minted records deleted, `:15240-15246`); build ONE page
  (`kvmap_sweep_page`, `:15340-15489`): the left-boundary STRADDLER first (a run keyed below `start`
  covering it — floor probe; its keep head `[s, start)` is live, its tail consumes right-to-left,
  `:15422-15438`), then `block_map_range(start, chunk)` records; the budget counts COVERED INDICES, not
  records (`:15334-15336, 15351-15357`: a 4,096-index run's one-shot release measured **~178 KiB against
  the 128 KiB whole-entry cap** — the red-first suite's conviction, D:533-537); a run consumed partially
  is re-Put SHORTENED at its own key (`shortened_run_entry`, len 1 collapses to the point form —
  record-true arithmetic, `:15387-15421, 19343-19388`) so every committed intermediate state is
  coverage-exact; resume cursor = the first UNPROCESSED record's own key (never `idx + run_len` — a
  superseding point inside a deleted run's span is its own record, `:15454-15461`), or `last Delete + 1`
  after a full page (`:15471-15487`); every release pairs a freed key; an unresolvable tag REFUSES the
  chunk loud (never-lossy: deleting a record whose block cannot free strands it, `:15336-15339,
  15363-15382`). ONE KvTx = map Deletes/shortened Puts + their ref releases + the cursor-advance head
  Put (`set_layout_and_size_with_map_holding`, `:15310-15318`); the TERMINAL chunk clears the cursor in
  the same tx (`:15255-15256`). Freed keys return for the router's purge-before-free + reclaim enqueue
  AFTER the guard drops (RES-1, `R/routing.rs:22314-22321`).
- Cadence: `JobType::KvmapSweep` on the job fabric (authority-only, local pool, `throttle_pct: 0`,
  `R/jobs.rs:684-690`); `run_kvmap_sweep` loops one chunk per task, `chunk = map_migrate_chunk()`,
  `duty_park` after each (KD-3 throttle), checkpoint every `CHECKPOINT_SECS = 5` s (`:2570-2615, :57`);
  a chunk error fails the job LOUD + resumable (`:2598-2606`). Submission deduped on live non-terminal
  jobs for the ino (duplicates are SAFE — every chunk re-reads the cursor under the held 4a — but
  pointless, `:667-699`).
- Crash resume (KD-6, D:558-565): `adopt_kvmap_sweeps` at mount — one tree-7 OWNER skip-scan per volume
  (`block_map_owner_scan`: one bounded range probe per distinct owner then jump to `(owner+1, 0)`,
  O(owners) never O(records), `R/kv/backend.rs:14092-14123`), each owner's head read for a live cursor, a
  job regenerated for every cursor no live job covers; a `nlink == 0` owner re-arms the corpse mark
  (`R/jobs.rs:717-771`; `map_sweep_resumed`). Deletions are record-true and cursor-resumed, so they sum to
  exactly the residue across a crash — never a double free.
- The write-during-sweep / EXTEND BARRIER (D:569-580; `R/kv/backend.rs:14402-14451`): a LOCAL train whose
  `cursor_floor = ceil(size/bs)` (router-computed, `R/routing.rs:10354-10367`) exceeds the live cursor K
  sweeps exactly the re-exposed span `[K, floor)` — residue there would become servable the instant the
  size commits — in chunked txs co-owning the held 4a (no cursor advance inside: a crash re-runs
  record-true), and the flip head carries the advanced cursor (`flip_cursor = bound`). The degenerate
  barrier (no growth) still dissolves the left-boundary straddler (a shrink-superseding diff Put would
  otherwise drop its tail's coverage with the references never released, `:14421-14426`). A desired
  binding at/above the live cursor is a cursor-invariant break upstream ⇒ refuse (`:14453-14467`).
  Served (shipped) trains never barrier: a size-raising ship or a claim at/above K refuses retried-class
  (`map_refused`, `:14377-14400`).
- Unlink corpse (D:542-557; `kvmap_corpse_handoff_qualifies` `R/routing.rs:23002-23035`,
  `kvmap_corpse_sweep_handoff` `:23058-23146`): `delete_file` on an over-threshold (or already-cursored,
  or partial / over-budget) kvmap ino — judged off the DURABLE head alone, never the map rehydration —
  keeps the inode record + head ALIVE as a corpse: nlink 0, unreachable, `size 0`, `;sweep:0` (K = 0 =
  the whole range), the pending accounting notes drained as RELEASES in the same tx (dedup'd;
  `:23085-23103`); the torn-down rewrite epoch's displaced/shadow keys and the overlay's bindings free
  inline (RAM-only populations the sweep can never find, `:23066-23122`); staging teardown O(PRESENT).
  The job's TERMINAL chunk destroys the corpse (record + xattrs, one tx — the C9 destroy half, no
  quarantine: a planned teardown, `R/jobs.rs:2616-2634`). Reclaim and the mount corpse sweep WITHHOLD
  `destroy_inodes` for registry-marked corpses (destroying the head would orphan the records); the
  census/oracle already count `nlink == 0` layouts so mid-corpse reads drift-free (D:552-557).
- Interaction with reclaim/refs: per chunk `−ref` records ride the tx; the terminal free walks the
  ordinary ladder after the guard drops (`free_deferred_keys`); the pending-free gating on checkpoint
  COVERAGE (charter §5.3) applies to the map leaves' SMO images as to any node. Port: the sweep's
  per-chunk frees go through `block_refs` release → `reclaim` enqueue (RES-1) exactly as `AfterCommit`
  does for the inline truncate (`P/src/fuse/handler/kv_fs.zig:761-779` — today `isKvmap ⇒ FileTooBig`
  at `:763`, the owed arm).

---

## 5. EFBIG, probation, fsck C11

### 5.1 EFBIG at `u32::MAX` blocks

`max_file_size() = min(i64::MAX, block_size × u32::MAX)` (`R/fuse_client.rs:25010-25027`): block indices
are u32 across the striped layout (map keys, patch/extent records, prefetch lanes); the pre-fix paths
silently wrapped the index mod 2³² and read back zeros (fstests generic/525). Every size-growing entry
point screens it: write (`:16165`), setattr size (`:28485`), copy_file_range dest (`:29304`), fallocate
(`:30258`). On a crossed file the last legal index `u32::MAX − 1` admits and round-trips; minting
`u32::MAX` refuses EFBIG at admission with ZERO map-plane side effects — the codec's A5 refusal stays
structurally unreachable from FUSE (`tests/kvmap_read_tests.rs:718`). Port: `BLOCK_INDEX_MAP_BLOB`
already reserved in `key.zig:172-176`; the FUSE screen `end > block × u32::MAX ⇒ FileTooBig` belongs in
`kv_fs.write/setattr/fallocate` beside the `std.math.add` overflow check (`P/kv_fs.zig:1378`).

### 5.2 Tree-7 leaves on probation (A7, D:134-137, 273-276)

Demand-loaded tree-7 LEAVES enter the clock with the ref bit CLEAR (no second chance until a second
touch) so a giant streaming map's leaves evict first and cannot evict an unrelated workload's
inode/dentry nodes (`R/kv/node_cache.rs:1655-1661, 3788-3813`). Pre-A7 the stream's leaves all carried set
ref bits, the first over-budget sweep cleared everyone and evicted the OLDEST mapping — the foreign
working set (`tests/kvmap_read_tests.rs:933`). Interior nodes stay pinned; a root leaf is pinned by the
tree open right after. The reference's forest face: every slot-tree node carries header tree id 0, so
the verdict is `header.tree_id == 7 ∨ (tree_id == 0 ∧ leaf_carries_block_map_records)` — a scan of the
loaded leaf's keys for the first kind-7 record, read off the LOADED image before the cache takes it
(`:3781-3791, 3828-3837`); `META_KV_BLOCK_MAP_LEAF_READS` ticks there (`:3812`). **Port gap:**
`P/src/meta/node_cache.zig:1734` tests `header.tree_id == TREE_BLOCK_MAP` only, and in v4 every slot
tree's nodes carry header id 0 (`P/src/meta/tree.zig:364-366`) — the probation (`beginProbation`,
`:789-794`) is dead on a forest. Fix: classify by the leaf's key kinds at load (the `min_key`/`max_key`
of the header already bound the leaf's key range: a leaf whose `min_key`'s ino-major kind byte is 7 and
whose `max_key`'s is 7 is a pure map leaf — O(1), no bset scan; a mixed leaf uses the reference's
first-kind-7 scan).

### 5.3 fsck C11 — map-plane consistency, REPORT-ONLY (D:150-162, A3; `R/fsck.rs:36-44, 654-676`)

Arms: (a) orphan tree-7 records — the owner has no live inode record, or its head is not kvmap-class
(crossing residue) — the owner skip-scan is the ONLY detector of records no head names
(`evaluate_c11_orphans`, `:5032-5072`; suspect carries the LOCAL owner ino); (b) a `kvmap:` head with
nonzero size and ZERO tree records (the fully-empty case; partial coverage is undetectable — the
size-vs-sparse ambiguity, `:5168-5198`; nominated from the census's extraction pages); (c) run-vs-point
coverage sanity: a point on a DIFFERENT volume strictly inside a covering run's span
(`kvmap_run_foreign_shadows`, `:5084-5125`); same-volume points inside runs are LEGAL by the read law and
indistinguishable without the stride census (deliberately, D:486-491). Zero-FP shields: the in-flight
crossing registry (`crossing_in_flight`, exempted at nomination AND verify, `counters.crossing_exempted`)
and the held 4a; C9's era floor is structurally inapplicable (a record's ino may be years old while its
crossing is live on THIS mount, D:155-158). A head with an open sweep cursor is exempt-in-range (D:122;
pinned: `fsck_map_orphan_records = 0` across mid-sweep passes, D:590-593). WHY report-only: a false
quarantine would hole a live crossing; the A1 residue sweep is the reclaim path (`:42-44`). Gauges
`fsck_map_orphan_records` (must-stay-0), `fsck_map_empty_heads`, `fsck_map_run_foreign_shadows`
(`R/fuse_client.rs:6399-6407`). C11 runs only in the finalize (shards skip the map plane, `:2329`).

---

## 6. Knobs, gauges, constants

### 6.1 Knobs (`R/src/env_knobs.rs:172-174`)

| row | kind / range | default | doc (reference) | port class |
|---|---|---|---|---|
| `SQUEEZEFS_KVMAP` | Bool | on | NEW beyond-inline crossings take tree 7; `0` = legacy blob; never disables head resolution (A10) | **not carried**: the port has no blob arm, so the lever has no control arm (§8 Q5) |
| `SQUEEZEFS_KVMAP_OVERLAY` | Bool | on | over-budget inos take the PARTIAL store; `0` = whole-map RAM at any size — "the acceptance bracket control, never an operational escape" | **not carried**: the port has no whole-map write authority to toggle (§3.2) |
| `SQUEEZEFS_MAP_MIGRATE_CHUNK` | Int 64..=1024 | 512 | ops per crossing/sweep tx = the finding-38 `BLOCK_REF_TX_CHUNK` law; the cap keeps a mis-set knob under the 128 KiB whole-entry admission (A10); the in-process `clamp(64, 1024)` only guards mid-process env mutation (`R/routing.rs:1214-1222`) | `ab_lever` naming the arm, read ONCE through `knobs.intKnob`; no clamp (0002 row 14) — see §6.3 for the derivation that should replace the literal |
| `SQUEEZEFS_MAP_WINDOW_{MB,PCT}` | planned (D:201-203) | — | the R5 `block_map_window` component | not built in the reference (floor 0, weight 2); port: `derived_override` iff a measurement needs it |
| `SQUEEZEFS_BLOCK_REFS_VERIFY` | Bool | off | mount-time C8 walk (`R/docs/design-durable-block-refcounts.md:196-197`) | already the port's `verifyRefs` posture |

### 6.2 Gauges / counters (`R/src/meta_backend/kv/mod.rs:529-582`; `R/src/fuse_client.rs:7011-7075,
6399-6407`)

| name | kind | meaning / WHY |
|---|---|---|
| `meta_kv_block_map_puts` / `_deletes` | counter | records staged as Put / Delete in layout txs — "a flat counter under a crossing storm means the wiring regressed" |
| `meta_kv_block_map_run_puts` | counter | RUN/RUN2 Puts — run-emission engagement |
| `meta_kv_block_map_lookup_exact` / `_floor` / `_range` | counter | the §8 #5 SPLIT (a merged counter made §5's gauges un-derivable) |
| `meta_kv_block_map_range_records` | counter | records returned by range pages — `records ÷ publishes` growing with file size = the whole-map diff running where a window should (finding 46) |
| `meta_kv_block_map_leaf_reads` | counter | tree-7 leaves demand-paged off the device (cold amplification ≈ `leaf_reads × node_size`) |
| `publish_map_record_bytes` | counter | key+value bytes staged by kvmap saves (the blob-byte counter's twin; `publish_indirect_blob_bytes` / `layout_indirect_map_reads` must read 0 — not carried in the port) |
| `map_migrate_inos` / `_records` / `_resumed` | counter | crossings completed / ops staged / first crossings whose A1 diff found residue (`preexisting > 0`) |
| `map_sweep_jobs` / `_chunks` / `_records` / `_resumed` | counter | handoffs / chunk txs / records removed (job chunks + extend-barrier absorptions) / jobs regenerated at mount |
| `kvmap_write_map_bytes` (+ `_budget_bytes`) | gauge | Σ estimated resident write-map bytes (port: the window-cache bytes instead) |
| `kvmap_overlay_saves` / `kvmap_window_saves` | counter | partial saves / window saves — a streaming row's post-crossing publishes must account under the latter |
| `kvmap_partial_inos` | gauge | live partial-mode inos (`R/fuse_client.rs:12175`) |
| `block_map_window_bytes` / `_fills` / `_hits` / `_evictions` | gauge/counter | the R5 `block_map_window` component |
| `fsck_map_orphan_records` (must-stay-0) / `fsck_map_empty_heads` / `fsck_map_run_foreign_shadows` | counter | C11 (a)/(b)/(c) |
| `crossing_exempted` (fsck counter) | counter | A3 shield engagement |
| `meta_ship_publish.map_{shipped,served,refused,recomputed_releases}` | counter | mw plane (rung 6) |
| ONE INFO log line per CROSSING (ino, record ops, preexisting, mode) | log | finding 45: engagement must be greppable by the ladder's name (D:783-791) |

Port rail: every `SQUEEZEFS_*` literal is a registry row and every row is read (`tests/rails.zig`); the
gauge names above become `counters.ShardedCounter`/`Gauge` rows in `src/data/block_map_stats` (or the
plane's `stats`), never process statics (§8 Q7).

### 6.3 Constants with their WHY

| constant | value | WHY (on the line) |
|---|---|---|
| `RUN_LEN_MAX` | 4096 | the floor probe is ONE bounded forward scan of this width — a format LAW (a wider run is invisible to every reader) (`R/block_map.rs:124-129`) |
| `RUN_LEN_MIN` | 2 | a 1-run is a point — one spelling per mapping |
| crossing chunk | 512 ops/tx | = `BLOCK_REF_TX_CHUNK` (finding 38): 512 × (13 B key + 30 B value + 16 B header) ≈ 30 KiB of map records (+ 512 × 47 B refs ≈ 24 KiB) stays far inside the 128 KiB whole-entry cap; registry max 1,024. **Port derivation**: `chunk = (MAX_ENTRY_LEN / 2) / (record_header + key + max_value + ref_record)` ≈ `65,536 / 106` ≈ 618 → floor to 512; a tie test restates it |
| sweep budget unit | covered INDICES, not records | a 4,096-index run's one-shot release measured ~178 KiB vs the 128 KiB cap (`R/kv/backend.rs:15351-15357`) |
| `kvmap_sweep_threshold_blocks` | `chunk × 64` = 32,768 blocks ≈ 128 GiB | derived, never a knob — below it the synchronous path runs verbatim; above it the SETATTR is O(1) (`R/routing.rs:1224-1233`) |
| write-map / window budget | `mem_budget / 16`, no floor | the node-cache default's divisor; a floor would admit a map the budget cannot hold (`:1086-1107`) |
| `KVMAP_WRITE_MAP_ENTRY_EST` | 64 B | hash slot + String header + key body — an estimate by design, O(1) per merge (`:1109-1115`); port: the window arena's actual bytes are exact and cheap — use them |
| `KVMAP_WINDOW_SPANS_PER_INO` | 2 | one demand stream + one lateral (`:1142-1144`) |
| `KVMAP_WINDOW_FILL_RECORDS` | 512 | one `block_map_range` page (the leaf-span class) |
| `KVMAP_WINDOW_FILL_INDICES_MAX` | 4096 | a run-heavy page must not balloon the window; `RUN_LEN_MAX`-class ≈ 256 KiB at the estimate |
| `KVMAP_FETCH_BRACKET_RETRIES` | 32 | reader poll cadence ≥ 1 s ⇒ ≤ 1 lost step/s; exhaustion = a broken epoch source, loud (`:1207-1213`) |
| `TRAIN_REF_TX_CHUNK` | 512 | the f38 law on the claims arm (`R/kv/backend.rs:15075`) |
| job `CHECKPOINT_SECS` | 5 | the job fabric's §5.1.2 cadence (`R/jobs.rs:57`) |
| `KVMAP_HEAD_MAJOR` | 1 | always-forward head grammar (not carried: the port's head is typed fields) |
| ceiling | `block × (2³² − 1)` = 16 PiB − 4 MiB | u32 index everywhere; `u32::MAX` reserved (`R/fuse_client.rs:25010-25027`) |
| leaf span | ≈ 8 k entries ≈ 16–32 GiB of data at 4 MiB blocks | 256 KiB node ÷ ~30 B record (D:131-133, 259-261) |

---

## 7. Tests (name — the contract in one sentence)

`R/tests/kvmap_tree_tests.rs`: `key_codec_round_trips_and_sorts_memcmp` (:145) — BE composites sort
logically; `value_codec_round_trips_point_and_string` (:182); `run_and_stamped_codecs_round_trip_and_
refuse_unemittable_forms` (:210) — len ∉ 2..=4096, stamp 0, overflowing RUN2 refuse;
`head_sentinel_round_trips_and_carries_the_sweep_cursor` (:283); `reserved_index_and_malformed_keys_
refuse_loud` (:348); `unknown_value_version_kind_and_truncation_refuse_loud` (:376);
`unknown_kvmap_major_and_malformed_heads_refuse_loud` (:412); `map_records_ride_the_layout_transaction_
and_add_no_commit` (:441) — Puts ride the layout tx, zero extra entries; `map_ops_on_an_unstamped_volume_
refuse_loud_never_silently_drop` (:523); `get_block_mapping_and_range_round_trip_with_cross_ino_isolation`
(:575); `index_range_bounds_exactly_one_ino` (:725); `map_records_survive_a_crash_remount_through_the_
journal` (:742); `a_fresh_format_does_not_carry_bit16_and_sector0_stays_byte_identical` (:789);
`stamping_bit16_engages_the_tree_on_the_next_mount` (:859).

`kvmap_crossing_tests.rs`: `a_crossing_flips_the_head_and_the_tree_carries_every_mapping` (:317) — head
flips, tree carries every mapping, refetch resolves via the TREE, C8 drift 0;
`a_crossed_head_survives_a_crash_remount` (:413); `crash_residue_is_invisible_and_the_recross_sweeps_it`
(:449) — every mid-train crash window leaves "records staged, head not flipped"; invisible after remount;
the next A1 diff reconciles, including a record beyond the final map; `a_fresh_volume_self_arms_at_its_
first_crossing` (:575, finding 43); `the_knob_governs_new_crossings_only_and_heads_stay_sticky` (:639);
`a_legacy_indirect_ino_converts_on_its_first_publish` (:722, not ported); `the_derived_recovery_walk_
owns_a_kvmap_inos_blocks` (:827); `unlink_sweeps_the_records` (:879); `a_shipped_crossing_is_witnessed_
owner_ratcheted_and_era_gated` (:1052, rung 6).

`kvmap_read_tests.rs`: `undecorated_keys_publish_as_point_records_and_round_trip` (:325); `decorated_keys_
still_ride_string_verbatim` (:457); `incarnation_engaged_eras_ride_stamped_compact_forms` (:526);
`kvmap_boundary_admits_last_index_and_refuses_efbig_at_reserved` (:718); `giant_sparse_matrix_survives_
remount_exact` (:841); `tree7_demand_leaves_enter_the_clock_on_probation` (:933) — a once-touched giant
leaf stream evicts itself, a twice-touched foreign node survives; `reader_bracket_arms_on_armed_readers_
only` (:1056); `fetch_bracket_core_retries_on_epoch_step` (:1144); `lookup_counters_split_exact_vs_range_
and_count_leaf_reads` (:1187); `fetch_rehydration_is_leaf_amortized` (:1255).

`kvmap_run_tests.rs`: `a_sequential_publish_emits_coalesced_run_records` (:342) — ONE run record, bytes
collapse ~len×, expansion equals the published entries, C8 drift 0; `a_one_block_rewrite_
recanonicalizes_to_run_point_run` (:408); `an_exact_point_supersedes_a_covering_run_until_the_publish_
recanonicalizes` (:485) — the floor probe + the one-tx coalesce; `claims_never_partial_adopt_a_run_
silently` (:643); `stamped_keys_ride_point2_and_run2_with_verbatim_stamps` (:828);
`an_over_budget_kvmap_merge_flips_mode_instead_of_refusing` (:950); `run_expansion_equals_emission_at_
every_stride` (:1022) — 64 KiB / 1 MiB / 4 MiB strides (finding 44's class-catcher).

`kvmap_sweep_tests.rs`: `an_over_threshold_truncate_is_o1_and_plants_the_durable_cursor` (:374) — ≤ 2
journal entries vs ~4,100 records, records stand, RAM pruned, cold refetch excludes residue;
`below_threshold_truncate_and_unlink_stay_verbatim` (:436); `the_sweep_job_drains_residue_and_clears_the_
cursor` (:495); `crash_resume_regenerates_the_plan_from_the_durable_cursor` (:559);
`a_write_into_the_swept_range_never_deletes_minted_records` (:655) — the extend barrier + the per-chunk
CURRENT-size floor; `a_mid_sweep_fsck_reports_nothing` (:736); `an_over_threshold_unlink_leaves_a_corpse_
the_job_destroys` (:782); `a_claims_train_meeting_a_live_cursor_refuses_at_or_above_it` (:886);
`the_truncate_handoff_submits_the_job_through_the_wired_fabric` (:1008).

`kvmap_bounded_save_tests.rs`: `a_claims_train_probes_o_claims_not_o_map` (:299); `a_window_train_probes_
exact_only_and_commits_the_frame_verbatim` (:411); `a_window_train_supersedes_inside_a_run_and_dissolves_
at_its_key` (:570); `a_solo_local_claims_train_mints_no_generation` (:705); `the_mode_flip_is_size_based_
both_directions_with_bounded_fetch` (:912); `partial_miss_reads_through_holes_read_zeros_tombstones_never_
resurrect` (:997); `an_overlay_save_ships_the_overlay_and_the_recompute_frees_the_displaced_block` (:1091);
`partial_truncate_never_refuses_and_sweeps_via_bounded_ranges` (:1167); `knob_off_keeps_whole_map_ram_at_
any_size` (:1239); `an_over_cap_remount_resolves_identical_bindings_without_materializing` (:1279).

`f46_kvmap_stream_publish_tests.rs`: `a_streaming_extend_on_a_kvmap_ino_publishes_in_o_window_tree_reads`
(:303) — 256 post-crossing whole-block writes, two interleaved writers, `range_records` delta 0 and one
exact lookup per publish; `window_saves_compose_with_the_rewrite_and_truncate_arms` (:443).

`kvmap_walker_tests.rs`: `a_healthy_kvmap_volume_passes_fsck_with_zero_findings` (:657); `a_seeded_orphan_
map_record_is_c11_and_the_registry_shields_it` (:689); `a_foreign_volume_point_inside_a_run_is_c11_and_a_
same_volume_one_is_legal` (:819); `an_empty_kvmap_head_with_nonzero_size_is_c11_report_only` (:951);
`defrag_d2_gauges_compute_over_a_kvmap_volume` (:231); `a_drain_moves_kvmap_blocks_and_republishes_the_map`
(:432). `kvmap_mw_hazard_tests.rs` (rung 6): the §11 hazard table rows. `f44_overlay_rewrite_tests.rs`:
`multi_pass_rewrites_survive_cold_reads_with_the_overlay_{on,off}` (:357, :366) — finding 44's
content venue. `sparse_write_bounded_tests.rs` — the `max_file_size` EFBIG screens.

Port contracts to write red-first (portable unless noted): codec totality + fuzz (exists,
`P/record.zig:1700, 2714`); `inlineMapCap` tie test (1,364 at 256 KiB); the chunk derivation tie; the
threshold tie (`chunk × 64`); the floor-law model (a value-type tree of `(idx, run_len)` records + the
scan over every placement of a point inside a run — `expectHolds` "resolves to the newest coverer",
`expectFails` with take-last-plain); the sweep-page model (budget by indices; every committed
intermediate state coverage-exact; resume at the first unprocessed key — weakening: `idx + run_len`
resume skips a shadow point); the crossing crash matrix on the Linux backend (kill between every chunk
and the flip; remount; residue invisible; re-cross reconciles); the EFBIG boundary; probation on a forest
leaf; C11 (a)/(b)/(c) seeded; the window law closure `range_records == 0 ∧ lookup_exact == inserts` on
the streaming e2e.

---

## 8. QUIRKS (decision 0002 rows to add) and the better designs

| # | Reference quirk | Why poor | What the port does |
|---|---|---|---|
| Q1 | The head sentinel as a mini-language in a string field `kvmap:1[;sweep:K][;gen:N]` with hand-rolled canonical-decimal parsing (`R/block_map.rs:472-593`; the R-5 note that rendering back cost one `String` per segment per READ, `:501-506`) | a grammar inside a value; parsed on the read path; a "major version" per field instead of the record's | **landed** (0002 row 97): `F_MAP_KVMAP` + `sweep_cursor u32` / `map_gen u64` under presence flags, `validate` refuses `gen == 0` and a cursor on a non-kvmap layout (`P/layout.zig:203-206, 461-472, 573-579`) |
| Q2 | STRING (kind 2) map values carrying decorated key strings; the "encoder-less arm" storing every record as STRING (Rev 1.3 #3) and dissolve survivors re-materialized as STRING (`R/kv/backend.rs:14227-14240`) | string-typed bindings parsed per read (0002 rows 96/105's class); two spellings of one binding (STRING vs POINT) churn the record-equality diff | kind 2 = a binary `MappingWire` (40 B) for packed-tenant / DAMAGED bindings (`P/docs/porting/data_layouts.md:145-146`); POINT/POINT2 for whole-block bindings; no survivor ever regresses to a string |
| Q3 | `block_map_tree_engaged()` / `block_map_tree_ready()` self-arming ratchet + `kvmap_enabled()` lever + a legacy blob fallback arm whose stand-down returns `Ok(None)` (`R/routing.rs:10136-10156`; finding 43) | three postures for one capability; a stand-down to a 545 GiB-ceiling blob is a silent degrade (charter §1.8) | ONE posture: `BLOCK_MAP_TREE` is stamped at the first crossing (barriered before the first record), and a ratchet failure REFUSES the publish loud (the write keeps its custody and retries) — no blob, no lever |
| Q4 | Rev 1.3 #2's whole-map RAM write authority (O(file) per write-open), its delete-by-absence diff, and the three-generation repair (6a EFBIG cap → 6c-i partial store with tombstones → finding 46's window train) | the diff ran per streaming publish (28.7 GB/s → 0.36 GiB/s); tombstones exist only to shadow un-published RAM deletes | the port's merge reads the DURABLE record under the stripe and stages O(inserts) exact-lookup Puts (§3.1) — the window IS the merge; no whole-map authority, no delete-by-absence, no tombstones; the `mem_budget/16` share sizes the read-side window cache |
| Q5 | `SQUEEZEFS_KVMAP` / `SQUEEZEFS_KVMAP_OVERLAY` A/B levers whose off legs are the blob arm and "whole-map RAM at any size — an OOM the operator asked for" (`R/routing.rs:1116-1124`) | control arms that are measured losers / unsafe postures (0002 rows 85, 155) | not carried |
| Q6 | `SQUEEZEFS_MAP_MIGRATE_CHUNK` as a literal 512 with an in-process `clamp(64, 1024)` (`R/routing.rs:1220-1222`) | row 14/17's class: a free-floating constant with a knob on top; clamping is banned | `block_map.CROSSING_CHUNK` DERIVED from the whole-entry cap (§6.3) with a tie test; the row is an `ab_lever` override read once, refused out of range by the gate |
| Q7 | Gauges as process `static AtomicU64`s (`R/kv/mod.rs:535-582`) and the per-ino tables as `scc::HashMap`s on the router (`kvmap_overlays`, `kvmap_windows`, `kvmap_write_maps`, `kvmap_sweep_corpses`) | statics shared across mounts (0002 row 102); a shared-line RMW per op (`fetch_add` on one word per lookup) | `counters.ShardedCounter` rows owned by the plane; the window cache a bounded per-ino table (the `PipelineTable` shape, 0002 row 148) |
| Q8 | RUN coalescing is a POST-ENCODE seam on the local whole-map train only (`coalesce_map_runs`, `R/mod.rs:1057-1163, 5994-6004`); runs form iff consecutive indices land on ONE volume at offset stride == block (`:1125-1136`) — and placement is an atomic round-robin over the health band per block (`R/routing.rs:3413-3420` `get_active_backend`: "an atomic round-robin pick inside the 90 %-of-max band"; the port's `pick` is the same round-robin, `P/src/data/router.zig:19-22`), so on any set with ≥ 2 eligible volumes adjacent blocks alternate volumes and NO run can ever form; with the steady-state diff gone (finding 46) re-coalescing happens only on non-publish saves (D:853-856 — "6c-ii's windowed canonicalizer is the owed economy item") | the run form is near-inert on every multi-volume set; a canonicalizer that needs whole-map RAM was banned (D:710-711) | **Two options for the owner.** (A, recommended) make runs a WRITE-side default by placing striped files in STRIPE UNITS: the router pins a file's placement to one volume for `K` consecutive blocks, then rotates (`K` derived: `clamp(RUN_LEN_MAX / eligible_volumes, 16, 256)` — ≥ 64 MiB per unit at 4 MiB blocks keeps a single stream's device parallelism across `eligible` volumes while the merge's inserts for one window are offset-consecutive on one volume whenever the allocator's carve is "lowest clear run at/above the frontier" (charter §5.5)). The merge then emits a RUN directly from its window when the window's inserts are consecutive (index, offset, stamp), extending an existing run it finds at the window's left edge by ONE exact lookup at `start − 1`'s floor — no post-hoc canonicalizer, no whole-map pass; `health_effective` still decides WHICH volume each unit lands on (fill balance converges per unit, not per block). (B) keep per-block round-robin and add a strided run form `RUN_S {…, index_stride u8}` covering `start, start+s, …` with `s × len ≤ RUN_LEN_MAX` so the floor bound holds — more format, same inertness for random writes. The spec recommends A; note the trade-off (per-block spread parallelizes one 4 MiB block across `N` volumes' read lanes today; per-unit spread parallelizes across blocks, which the prefetch pipeline's K = 4 offset lanes already do) |
| Q9 | Probation classified by `header.tree_id == TREE_BLOCK_MAP`, then a bset KEY SCAN of the whole loaded leaf on a forest (`R/node_cache.rs:3788-3791, 3828-3837`) | an O(records) pass on every cold leaf load to answer a yes/no the header's key bounds already answer; the port's copy (`P/node_cache.zig:1734`) is dead on a forest | classify by `min_key`/`max_key` kind bytes (O(1)); mixed leaves fall back to the first-kind-7 scan; a tie test over a forest leaf (§5.2) |
| Q10 | `bincode::deserialize` / `serialize` of the head inside the train, the handoff and the sweep chunk (`R/kv/backend.rs:14498-14540, 15194-15228, 15279-15309`) with `"undecodable head"` refusals | a serialization-library artifact on the commit path (0002 rows 2/95) | the typed `Layout` + `LayoutDelta{mask: SWEEP_CURSOR\|MAP_GEN\|SIZE}` (`P/layout.zig:814-815`) — the cursor advance and the size flip are 10-byte Δ records, never a head rewrite |
| Q11 | Map-ops as `Vec<Vec<BlockMapOp>>` groups + `MapEntry::encode() -> Vec<u8>` per record (`:14557-14582, 19148-19149`) | per-record heap allocation on the publish path (charter §12) | records encoded straight into the tx arena (`t.arena`, as `publishLayout` does, `P/layout.zig:1350-1354`); groups as index ranges over one staged array |
| Q12 | The crossing registry as an `scc::HashMap<Ino, ()>` + RAII guard (`:19440-19457`) | fine in shape; a hash per fsck probe | a bit in the ino's 4a guard word (the exclusive hold IS the registration): fsck's shield = "the ino's I-guard is held exclusive by a crossing" — one atomic read |
| Q13 | `max_file_size()` recomputed per op from an atomic `block_size` (`R/fuse_client.rs:25024-25026`) | a format fact read through an atomic | `const` on the plane at open (0002 row 103's shape) |
| Q14 | `kvmap_truncate_sweep_handoff` re-fetching the whole metadata entry and pruning the RAM map under the 3.5 guard (`R/routing.rs:22360-22461`) | O(map) RAM work on a path whose whole point is O(1) | the port holds no RAM map: the handoff is the Δ record + the inode size Put, then `shape.publish`; tier purges of the removed range ride the sweep's chunks (the only place the removed keys are ever enumerated) |
| Q15 | The sweep's throttle `throttle_pct: 0` + `duty_park` + a 5 s checkpoint (`R/jobs.rs:684-690, 2610-2615`) | the manners law of charter §6.2 is the cadence contract, not a duty cycle | the sweep runs on the reclaimer's manners law (foreground device-byte movement ⇒ defer; idle ⇒ full width); one chunk tx per pass; cursor durability is the checkpoint |
| Q16 | `#[allow(clippy::too_many_arguments)]` on the train (12 parameters, three closures) (`:14301-14314`) | the closures exist to borrow the router's stride/encoder/ref resolution into the meta layer | the port's merge lives in the plane, which owns the router: `Geometry{chunk}` + the `vol_tag → allocator` table are fields, not callbacks |

### Port plan (what exists, what is owed)

Exists: kind-7 key (`P/key.zig`), value codecs + `RUN_LEN_MAX` (`P/record.zig`), typed head fields +
Δ table (`P/layout.zig`), `BLOCK_MAP_TREE` bit (0002 row 43), `inlineMapCap` (`P/plane.zig:711-715`),
`MapSeam`/`MergeRequest` (`P/write_seams.zig`), probation hook (`P/node_cache.zig:789-794`, dead on a
forest — Q9), the EFBIG pre-screens (`P/kv_fs.zig:1468-1470, 763, 1382`).

Owed (one rung, red-first, in this order): (1) `block_map.zig` resolve: exact → floor (one bounded range,
take-last-coverer) + the range iterator with same-kind resume; (2) the crossing in `plane.mapMerge`: when
`merged.len > inlineMapCap()` run the train under the ino's exclusive guard — A1 diff (empty-descent on a
healthy ino), chunked Puts, the flip as the window's tx with NO `−ref` for moved entries (§2.4); the
INFO line; `map_migrate_*`; (3) kvmap-head merges: exact-lookup Puts per insert + displaced `−ref`,
head Δ only on size/cursor change (§3.1), RUN emission from the window per Q8's ruling; (4) reads:
`metaBinding` through the resolve with the per-ino 2-span window cache charged to the budget
(`mem_budget/16`, floor 0, sheddable); (5) truncate/unlink: threshold `chunk × 64`, size-flip-first Δ
`{size, sweep_cursor}`, the sweep as a reclaimer-lane task with the index-budgeted page law, straddler
shortening, terminal cursor clear + corpse destroy, mount adoption by owner skip-scan; the extend barrier
in the merge when `ceil(size/bs) > cursor`; (6) EFBIG at `block × u32::MAX` in `kv_fs`; probation by
key-kind; fsck C11 (a)/(b)/(c) report-only in rung 8's fsck. Models: the floor law and the sweep page
(§7). Evidence note: the streaming-extend closure (`range_records == 0` per publish) is THE row.
