# Rung 2a — MetaLV foundation: codecs, the ONE fold, journal + reservation core, ledger, allocator, superblock, geometry, the lifecycle cores

Charter §13 rung 2, first half (the reference's PR K1, K3, K4, the codec/plan half of K6a, and the
three dependency-free cores K5 depends on). Landed 2026-10-05 on `dev` (`9591508`). All portable:
byte images, no device I/O — the lane/format tasks bind them to `io/`.

## What landed

| Module | Lines | Tests | Contract |
|---|---|---|---|
| `meta/key.zig`, `record.zig`, `bset.zig` | ~5,100 | 78 | Kinds 0–9 in legacy + forest layouts; the 57 B inode value (`rdev` in the old `flags2` slot); dentry/xattr/block-ref/block-map codecs with every refusal; Δinode (mask bits 0–9) and the `0x4C31` layout delta (base pinned by bincode fixture bytes); **the ONE fold** (`foldNewestFirst`, `foldDeltasOntoPut`, `compactFold` with `seq < durable_tail` elision, `foldForward`) shared by lookup, merge, compaction and replay; `KBSF` v1/v2 frames (stamp at `[24..32)`), the `(key asc, seq desc, source asc)` merge with the run-end memo; the `record_count ≤ data_len/16` allocation bound |
| `meta/journal_core.zig` + `_model.zig` | 2,100 | 25 | The reservation core (admission classes, the checkpoint reserve `max(256 KiB, ring/64)`, ONE `fetch_add` unpadded / CAS loop padded, `CoreGeometry` + the sector-pad law `PAD_MIN = 21`), the in-flight registry with the completed-prefix law; 7 exhaustive models |
| `meta/journal.zig`, `ledger.zig` | 3,100 | 45 | Page header (24 B, xxh3 over `[0..16)`), entry + PAD framing, aligned-run coalescing (zero holes, misaligned refused), head-alignment RMW, `replayScanImage` per the spec pseudocode, `JOURNAL_FAILURE_LATCH = 3`, torn-write/power-cut image harness; the 32-slot ledger codec (incl. `node_seq_watermark`), newest-valid-wins, DENSE restatement, tail floor, reclaim epoch gating |
| `meta/alloc_core.zig` + `_model.zig`, `alloc_ext.zig` | 3,000 | 36 | Budget-then-bit `fetch_or` claim (`CLAIM_RESCAN_LIMIT = 64`), Vyukov pending-free FIFO with the stamp protocol, coverage gate on the SAME appender's barrier-covered tail, forced overflow (a pressure valve, never a liveness gate), compaction reserve `max(8, 2 %)` + floor `reserve/2`, `heap_promised` admission, the space-standstill vs 8-cycle-wedge audit; `KVAB` pages (24 B header, 32,576 bits, A/B), deltas, DUR-4 page writes, replay; 7 exhaustive models |
| `meta/superblock.zig`, `geometry.zig` | 1,900 | 19 | 152 B superblock with `@offsetOf` assertions, 18 incompat bits, decode in the reference's check order with every refusal string in ONE table, `classifySector0`, both legal geometry placements, DUR-5 backup rule and write plan; `plan()` with the four worked examples (64 MiB / 256 MiB / 1 GiB / 1 TiB), feature words, appender-0 `Free` page + `KVAD` header bytes, `buildPlan` with typed placeholders for the node images |
| `meta/node_state_core.zig`, `epoch_core.zig`, `conveyor_core.zig` + models | 2,800 | 40 | The lifecycle word (eleven ops, every RMW `acq_rel` with its pairing named), `RevalidationEpoch`/`AppendGate`/`NodeEnv`/`StructuralHolds` with the two-word publish law, `ConveyorCore(T)` (group atomicity under ONE lock hold, SeqCst leader, release-then-recheck); 15 exhaustive models |

## Evidence

Instrument: `zig build gate` (fmt + unit + rails), Debug test profile. Venues: the `sqz-zig` container
(Ubuntu 24.04, kernel 7.2.6-sqz, aarch64) and the macOS dev box (portable modules).

| Venue | Result |
|---|---|
| Linux | 10/10 steps; **403/403** (401 unit + 2 rails); exit 0 |
| macOS | 10/10 steps; 349 pass + 2 Linux-only skips + 2 rails; exit 0 |

Byte-identity evidence: the reference's `tests/fixtures/findvsa_pred_bset.bin` parses byte-for-byte
(4,334 records, horizon 72,220,661; compaction at the real tail 76,036,303 keeps every live key);
`BSET_MAGIC` pinned as numeric LE (`54 45 53 42`) vs the ASCII-ordered `KVND`/`KBSF`; the geometry
plan reproduces the spec's four worked examples; the layout-delta base is pinned by fixture bytes
produced with the real bincode 1.3.3.

## Model checks (29 models, every weakening proven to fail)

| Core | Model | States | Fails under |
|---|---|---|---|
| journal | disjoint monotonic wrap | 324 | unconditional by sizing |
| journal | no over-commit, reserve protected | 92 | holds under read-order reversal too (the spec's derivation naming it was wrong) |
| journal | budget conserved across transfer/release | 215 | — |
| journal | padded reserve aligned | 246 | — |
| journal | admit races transfer | 32 | admission reading `head` first; transfer retiring the claim first |
| journal | **three-party ABA** | 450 | **the reference protocol itself** |
| journal | completed prefix (two-stage) | 683 | complete-at-submit; relaxed landed; relaxed ack |
| alloc | 7 models (double alloc, coverage gate, headroom, forced overflow, reserve isolation, partitioned claims/clocks) | 80–2,603 | load-then-store bit take; gate off-by-one; budget before bit; stale-stamp full; `<` floor; shared budget; shared clock |
| node_state | 5 models | 5–58 | split `markDirty`/`supersede`/`endFreeze`; `tryEvict` as `fetchOr`; `beginFreeze` as two RMWs |
| epoch | 4 models | 8–23 | epoch-then-tail publish; tail-then-epoch load; CAS as store (×2); post-read re-probe |
| conveyor | 6 models | 16–1,271 | load-then-store lead; recheck-before-release; non-SeqCst unlead; push outside the mutex; entry retraction; per-item lock hold |

### Finding against the reference: the two-word reservation protocol over-commits

The reference's admission keeps a byte-count word (`admitted`) CAS'd up at admission and
decremented when a claim transfers into `head` (`head += len; admitted −= claim`). A CAS on a byte
COUNT is ABA-blind. With three parties on a cap-8 ring: X holds 3; Z reads `admitted = 3`, `head = 0`;
X transfers (`head = 3`, `admitted = 0`); Y admits 3 (`admitted` back to 3); Z's CAS `3 → 8` succeeds
— Y's bytes are counted zero times and the commitment is 11 of 8, which overwrites X's unreclaimed
bytes. Model 6 reproduces it exhaustively in 450 states.

The port keeps **monotonic totals** — `admitted_total` (the CAS word; it can never round-trip),
`released_total`, `transferred_total`; outstanding = admitted − released − transferred — with the
same shape otherwise (ONE `fetch_add` on `head` unpadded, the CAS loop padded; admission reads the
transfer counter BEFORE `head`; the transfer bumps `head` BEFORE retiring the claim). All four spec
models hold and both access-order weakenings still fail. `docs/porting/kv_journal_ledger.md` §2 needs
this correction; the reference should be told.

## Spec corrections recorded by the implementers

- Default format feature word is `0x2FFD7` (the builder never stamps bit 16 at format); the spec's
  `0x3FFD7` contradicted its own bit list.
- `kv_node.md` §4.3: the `begin_freeze` two-RMW split is caught by model 4; model 3's only RMW outside
  the node lock is `end_freeze`, whose split is what model 3 catches.
- Worst-case stamped ledger record is 3,771 B (the spec's 3,765 omits `n_cursors` and the 4 B suffix).

## Deviations (deliberate)

- Zig errors carry no strings: the reference's `Corrupt(String)` family is a typed error set
  (`Corrupt`, `MixedDeltaClasses`, `LayoutChain*`, `LayoutBase{…}`, `ChecksumMismatch`, …); the
  refusal TEXT for the superblock lives in one `Refusal.write` table.
- The bincode layout base keeps the reference's acceptance set (trailing bytes allowed, any map
  order) so no fold that succeeds in Rust fails here; charter §12's "exactly the encoder's form
  decodes" holds for every codec we own.
- `wait_completed_upto` is a poll token + a yield-spin wait until the executor (rung 2b) lands.
- `std.Thread.Mutex` does not exist in Zig 0.17; the conveyor's sanctioned mutex is `std.Io.Mutex`
  (`lockUncancelable` — committer cancellation is designed out, charter §9).

## Wave 2 (same rung): node format, `format`, builder — byte-identical to the reference

`meta/node_seq.zig`, `meta/node.zig` (K2: header page with `@offsetOf` assertions, `NodeLayout` ties
C = 257,984 / ¾C = 193,488 / ¼C = 64,496, fanout 7,166 ⇒ depth ≤ 3 at 100 M, `verifyNodeExtentScreened`
per the spec pseudocode with the four-rule frame screen and the torn-tail stride pass, compact /
split with same-key cohesion / merge images), `meta/format.zig` (the 13-step plan as
`[]Step{offset, bytes, barrier_after}` + `formatImage`), `meta/builder.zig` (`ImageBuilder`,
`TreeWriter` ¾-fill packing, digest walks). 39 tests. Gate: Linux **442/442**, macOS 384 + 6 skips
(4 are venue-only cross-checks).

**Oracle: the reference binary** (v1.3.0 debug, copied from the user's `sqz-gate` into the venue at
`/build/ref/squeezefs`).
- Reference `format` → Zig loaders: both classes (`--single-writer` flat, default symmetric forest)
  decode end to end — superblock, newest ledger slot (seq 1 → slot 1), every root node, every record;
  ino 1 decodes `S_IFDIR|0o755`, nlink 2; the `format_config` xattr key's hash56 recomputes from the
  sector-0 seed; bitmap slot A bits `0b111`; `KVAD` header and appender-0 `Free` page byte-identical to
  `geometry.zig`'s images; the DUR-5 tail copy identical to sector 0.
- Zig `formatImage` → reference `fsck --offline`: with the reference's identity pinned as inputs,
  Zig's image is **byte-identical** to the reference's for both classes (`cmp` over 256 MiB: no
  difference) and for a `--meta-node-kib 64 --meta-journal-mb 1` pair at 128 MiB; with fresh uuids,
  215/174 differing bytes — all expected randomness (node seqs, checksums, uuid/hash_seed, xattr key
  hash). Every Zig image: `fsck` exit 0, `findings: 0 (clean)`. Both directions are durable venue tests.

Spec corrections from this wave (`kv_alloc_superblock.md` updated in place): `usable_budget` is
46,032 (64 KiB) / 193,488 (256 KiB) — the header page is subtracted; the frame term is the v1 32 B on
every layout; step 13's fdatasync coincides with step 12's (3 barriers total).

## Format v4 (2026-10-05) — ONE format act (decision 0003 WP-F; 0002 rows 35–50)

`superblock.version = 4`; the Rust reference refuses every Zig image loud and **the reference
oracle is retired for the trees**: the byte-identity cross-checks above (node cross-check 1,
format cross-check 2), the `fsck --offline` calls in the backend tests and the reference-generated
FIND-VS-A fixture are gone. The Zig invariants are the oracle: encode∘decode = id and decoder
totality under `std.testing.fuzz` for every codec (keys, records, inode value, bset + frame, journal
entry + page header, ledger record, superblock), replay ≡ RAM (`crashReplay` over `smo.TestVolume`),
SMO completeness, the Linux kill matrix (every tx acked BEFORE the cut present whole, no partial tx,
the DUR-8c watermark `max(ledger, replayed + 1)` exactly), `format → open → commit → close → open`
on the one-member forest. `tests/fixtures/findvsa_pred_bset.v4.bin` is Zig-generated
(`tests/fixtures/gen_findvsa_pred_bset.zig`; README records the seed, horizon and counts) and pinned
to 254,932 bytes / 3,515 records.

What changed, per module (every byte table in `docs/porting/*.md` carries a "v4:" row):

| Unit | v3 (reference) | v4 |
|---|---|---|
| Keys (`key.zig`) | two layouts: kindless legacy + forest transform | ONE layout, kind byte on every key: inode 9 B, dentry/xattr 17 B, block map 13 B, refs 29 B (`06` prepended); slot = `key_ino >> 40`; `checkSlotTreeKey` at admission, in the mount fold and in `tree.route` |
| Inode value (`record.zig`) | LEB128 tag + 57 B | 68 B LE `extern struct`, size-asserted; Δ field table derived from it at comptime; `0x81 0x00` refused |
| Record header | 14 B + leading kind byte | 16 B `{key_len u16, kind u8, flags u8 = 0, val_len u32, seq u64}` |
| Entry header (`journal.zig`) | 16 B, PAD by payload sniff | 24 B `{pos, len, kind (0 records / 1 PAD), reserved[3] = 0, xxh3 u64}`; `PAD_MIN = 24`, `maxPad` 4120 @ 4096 / 536 @ 512 |
| Page header | `writer_id u16`, `MAX_APPENDERS = 16` | `{magic, lap, first_entry_off, reserved u16 = 0, appender_id u32, xxh3}` (24 B) |
| Bset frame (`bset.zig`) | v1 32 B / v2 40 B | ONE 40 B stamped frame; `BSET` ASCII magic; reserved checked; `(0, 0)` = the one-member forest's stamp |
| Ledger (`ledger.zig`) | decode-by-length, optional stamp, bit-8 suffix, `routing_width` | ONE layout with `version u16`; `RootId{control = 8, native_slot = 0x0100}`; stamp always; stride ≥ 1; worst case 3,767 B |
| Superblock (`superblock.zig`) | 152 B hand table, 18 bits | 192 B `extern struct`, checksum last, `volume_len`, 4 capability bits, `set_uuid` + independent `hash_seed` |
| Geometry/format/builder | three format classes, appender plane at format | one plan: control tree (empty) + native slot tree; 9 build steps; no appender directory/page; `SetIdentity{set_uuid, hash_seed}` minted by `mintSetIdentity` (getrandom) |
| Allocator | `PartitionMap`, partitioned claims/clocks, 7 models | one owner per bitmap, 5 single-owner models; kind 4 = heap deltas only, kind 5 reserved for data-alloc deltas |
| Trees (`tree.zig`) | per-kind trees, `treeFor(tree_id)` | `TreeIdentity{control, slot}`; `tree.route` = THE routing law (content by key slot and kind, interior by header id through `slot ‖ separator`) shared by replay, the conveyor and the backend |
| Gate (`epoch_core.zig`) | `AppendGate{writers, writer_id}` + foreign-append probe | `ReaderGate` alone; slot ownership is the lease gate's verdict |
| Inos (`inos.zig`) | ino lanes (`lane_core`), dentry child read as a local | one cursor per slot; the fold routes a dentry's GLOBAL child back to `(slot, local)` |
| Errors | three errno tables | `core/errors.zig` through `coreError` (rows `Busy`, `UnsupportedVolume`, `MalformedKey`, `ValueTooLarge` added) |

New tie numbers (256 KiB / 64 KiB nodes): fold capacity 257,976 / 61,368; split part 193,482 /
46,026 (= the builder's `usableBudgetOf`); merge candidate 64,494 / 15,342; interior fanout at 9 B
inode keys 193,482 / 41 = 4,719 (64 KiB: 1,122); the full fold's separators 257,976 / 41 = 6,292
(the charter's "≈ 7,000"); ref separator record 49 B; `smoKeep` 65,536 + 4,120; the widest SMO
entry's pointer key is `KEY_BUF_CAP = 36` B (`slot ‖ KEY_SPACE_MAX`).

Gate: Linux **738 pass + 1 skip (739) + 3 rails**, exit 0; macOS **641 pass + 7 skips (648) + 3
rails**, exit 0. Two bugs found by the invariants while retiring the oracle: (1) the DUR-8c fold
burned ~W inos per crash replay (0002 row 49; the kill matrix now asserts `next_ino ≤ minted + 1`);
(2) the kill-matrix harness owed every ack ever made, including those acked after the cut — with
the native leaf now receiving every kind the threshold drain replaces it mid-stream, the cut moved
and the harness now owes exactly the acks it saw (an atomic acked counter read at the cut, the copy
through `fdatasync` + O_DIRECT reads).

## Owed (rung 2b)

Node snapshot + node cache, tree + SMO + leaf merge, 4a object locks, tx staging, the journal lane, the conveyor +
durability lane, checkpoint, heap law, inos, the journal economy — `docs/porting/kv_tree_cache_commit_survey.md`
§7 is the ordered plan with the tests per module.
