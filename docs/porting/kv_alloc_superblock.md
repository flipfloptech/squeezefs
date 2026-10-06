# PORTING SPEC — MetaLV superblock v3, volume format/image builder, extent allocator

> **v4 decisions (format v4, 2026-10-05; decision 0003 WP-F; 0002 rows 42–47).** The byte tables below
> are the reference's v3 layout — EVIDENCE, read for derivations and the WHY of every number. Where a
> "v4:" row exists it is what `squeezefs-zig` writes; nothing below obliges compatibility with the
> reference (0002 §2), and the acceptance bar of the preamble (the Rust binary mounting a Zig image) is
> retired. Rung 6 lands the appender plane against THESE tables, never the reference's.
>
> **v4: superblock** (`superblock.zig` `SuperblockWire`, 192 B inside the 4096 B sector, every field LE,
> `@offsetOf`-asserted): `magic "SQZMETLV"[8] @0 ‖ version u16 = 4 @8 ‖ reserved u16 = 0 @10 ‖
> node_size u32 @12 ‖ volume_len u64 @16 ‖ features_incompat u64 @24 ‖ features_ro u64 @32 ‖
> root_ledger {start, len} @40 ‖ journal @56 ‖ alloc_bitmap @72 ‖ heap @88 ‖ appender_dir @104 (`{0, 0}`
> = none) ‖ uuid[16] @120 ‖ set_uuid[16] @136 ‖ hash_seed u64 @152 ‖ sb_generation u64 @160 ‖
> reserved[16] = 0 @168 ‖ xxh3 u64 @184` over the whole sector with the field zeroed. The legacy magic
> `METALV01` classifies as `legacy{version}` → "reformat required". Capability bits (each ONE thing;
> every always-on mechanism is the version): `SYMMETRIC_PLANE = 1<<0`, `LAYOUT_DELTAS = 1<<1`,
> `BLOCK_MAP_TREE = 1<<2`, `VOLUME_LIFECYCLE = 1<<3`; a one-member format sets none. The DUR-5 copy
> lives at `volume_len − 4096` and is accepted only when its own `volume_len` names the slot it was read
> from. `validateGeometry` additionally refuses `heap.start % node_size != 0`. `hash_seed` is an
> INDEPENDENT `getrandom` draw (`geometry.mintSetIdentity`), never `xxh3(uuid)`.
>
> **v4: geometry/format.** One format class: the one-member forest — control tree (header id 8,
> empty leaf) then the native slot tree (header id 0, the description's records under kind-carrying
> keys), both roots in the ledger (`RootId{control = 8, native_slot = 0x0100}`); no appender directory,
> no appender-0 page at format (`sb.appender_dir = none`). Build order: register → zero `[0, heap.start)`
> → control image → native image → bitmap pages (gen 1) → ledger record (seq 1, slot 1, the one-member
> stamp `(set_uuid, pos 0 of 1, native slot 0, run {0, 1, 65536})`) → barrier → superblock (tail,
> barrier, sector 0, barrier) — 9 steps, 3 barriers. The `--single-writer` word, `FORMAT_FLAT` seam,
> `format_config` xattr and `multi_writer_flat` class are gone. Builder budget `usableBudgetOf =
> (node_size − 4096 − 40 − 32) × ¾` = 46,026 (64 KiB) / 193,482 (256 KiB) = `splitPartCapacity`.
>
> **v4: allocator.** One owner per bitmap (0003 Q3): `PartitionMap`, partitioned claims and
> per-appender coverage clocks are deleted; `KVAB` page = `magic "KVAB" u32 ‖ page_index u32 ‖
> generation u64 ‖ xxh3 u64` + 32,576 bits (ASCII magic in disk order). Journal deltas: kind **4** is the
> HEAP allocator's — key `extent u64 BE` (8 B) ONLY, value `0x01` alloc / `0x00 ‖ retire_tag u64` free;
> a 16 B key under kind 4 is `MalformedDelta`. Kind **5** (`key.TREE_DATA_ALLOC_RESERVED`) is reserved
> for the DATA-allocation bitmap deltas of charter §5.5 (rung 6): key `vol_tag u64 BE ‖ block_or_extent
> u64 BE`, value `{op u8, holder_term u64}` — a distinct kind, never a length-discriminated arm of kind 4.
>
> **v4 WILL write (rung 6; decided now so the reference's layout is not re-imported):**
> - Appender page (4096 B, A/B over FOUR slots per charter §5.5): `magic "KVAP" u32 @0 ‖ layout_version
>   u16 = 1 @4 ‖ reserved u16 = 0 @6 ‖ appender_id u32 @8 ‖ generation u64 @16 ‖ identity {node_token u64,
>   mount_slot u32, writer_id u32} @24 ‖ term u64 @40 ‖ state u8 {Free 0, Live 1, Recovering 2, Recovered 3}
>   @48 ‖ is_manager u8 @49 ‖ home_volume u16 @50 ‖ n_segments u8 ≤ 8 @52 ‖ n_grants u8 ≤ 4 @53 ‖
>   n_slots u16 ≤ SLOT_PAGE_BUDGET @54 ‖ head_hint u64 @56 ‖ ledger_tail_seq u64 @64 ‖ ckpt_seq u64 @72 ‖
>   seq_offset u64 @80 ‖ segments[8]{start u64, len u64} @88 ‖ grants[4]{start u64, len u64} @216 ‖ slot
>   entries[n] {slot u16, state u8, pad u8, g u32, root_addr u64, root_seq u64, cursor u64, extents
>   {start u64, len u64}} (40 B each) from @280 ‖ xxh3 u64 @4088` over `[0..4088)`. The version sits
>   right after the magic; `home_volume` is u16 (member positions are u16); `appender_id` is the u32 every
>   frame and page header carries. `SLOT_PAGE_BUDGET = (4088 − 280) / 40 = 95` at these widths.
> - Appender directory extent (`sb.appender_dir`, one node-size extent per chain link): the header page
>   FIRST at `dir.start` — `magic "KVAD" u32 ‖ version u16 = 1 ‖ reserved u16 ‖ pairs u32 ‖ next_link
>   {start u64, len u64} ‖ xxh3 u64` — then appender pairs at `dir.start + 4096 + pair × 8192` (slots A/B);
>   appender 0's four page slots are the fixed ring's first four pages (charter §5.5). The reference put
>   the header LAST (audit row 38); v4 puts it first like every other unit.
> - Stamping `SYMMETRIC_PLANE` is `enable-symmetric`'s ONE act: directory extent claimed and zeroed, header
>   page, appender-0 `Free` page, then the superblock with the bit and `appender_dir` set (one barriered
>   superblock write; `Superblock.setSymmetricPlane` refuses a repoint).

Reference (read-only): `~/Source/squeezefs` (Rust). Target: `~/Source/squeezefs-zig`.
Charter anchors: §1.7 (forward-only format; unknown incompat ⇒ "reformat required"; old
binaries refuse new volumes loud), §5.1 (superblock / bitmap / heap), §5.3 "Space".
Rust PRs covered: **K4** (`alloc_ext_core.rs`, `alloc_ext.rs`) and **K6a** (`superblock.rs`,
`builder.rs`). All integers little-endian unless marked BE. xxh3 = xxh3-64 (xxhash_rust).
Acceptance bar: a Zig `format` output must pass `classify_volume` + `KvMetaBackend::open`
(SB → newest ledger → bitmap+replay → tree roots) of the Rust binary, and
`be.free_extents() == sb.total_extents() − extents_allocated` (`tests/kv_backend_tests.rs:1455-1460`).

---------------------------------------------------------------------------------------------------
## 1. SUPERBLOCK v3 — byte-exact (`src/meta_backend/kv/superblock.rs`)

### 1.1 Sector 0 layout (one 4096-byte sector; `superblock.rs:11-27`, offsets `:62-88`)

```
[0..8)     magic            "METALV01"  (shared with retired v2 — version discriminates)   OFF_MAGIC=0
[8..12)    version: u32     3                                                               OFF_VERSION=8
[12..16)   node_size: u32   bytes; 4 KiB multiple in [65536, 1048576]; default 262144        OFF_NODE_SIZE=12
[16..24)   features_incompat: u64                                                           OFF_FEAT_INCOMPAT=16
[24..32)   features_ro: u64                                                                 OFF_FEAT_RO=24
[32..48)   root_ledger  ExtentRef {start: u64, len: u64}                                    OFF_ROOT_LEDGER=32
[48..64)   journal      ExtentRef                                                           OFF_JOURNAL=48
[64..80)   alloc_bitmap ExtentRef                                                           OFF_ALLOC_BITMAP=64
[80..96)   heap         ExtentRef                                                           OFF_HEAP=80
[96..112)  uuid: [u8;16]                                                                    OFF_UUID=96
[112..120) hash_seed: u64   keys dentry/xattr name hashes                                   OFF_HASH_SEED=112
[120..128) checksum: u64    xxh3_64 over the WHOLE 4096-byte sector with [120..128) zeroed   OFF_CHECKSUM=120
[128..136) sb_generation: u64   DUR-5 newest-valid-wins selector (primary vs backup)        OFF_SB_GENERATION=128
[136..152) appender_dir ExtentRef   bit-17 only; MUST be all-zero when bit 17 absent        OFF_APPENDER_DIR=136
[152..4096) zero padding (covered by the checksum)
```
ExtentRef encoding: `start u64 LE ‖ len u64 LE` (16 B); `end = start + len` (`:649-659`).
Checksum: `h.update(img[..120]); h.update([0u8;8]); h.update(img[128..4096])` (`:1158-1164`).
The encoder writes `appender_dir` only if `start != 0 || len != 0` (`:900-905`) — on a flat
image the field is zero bytes, so sector 0 is byte-identical to the pre-bit-17 image.
`SUPERBLOCK_V3_VERSION = 3`; `SUPERBLOCK_V3_LEN = SECTOR_SIZE = 4096`; `MAGIC_VALUE = b"METALV01"`.

### 1.2 `features_incompat` bits (`superblock.rs:96-585`) — every bit means ONE complete thing

| bit | const | meaning / presence rule |
|---|---|---|
| 0 | `KV_V3` | the v3 node layer. REQUIRED on every v3 volume (decoder refuses without it). |
| 1 | `NODE_SEQ_WATERMARK` | ledger records carry the node-seq mint watermark (Finding A). REQUIRED — absent ⇒ "pre-watermark … reformat required". |
| 2 | `KV_GUEST_SLOTS` | ledger records carry the §5.5.1a membership stamp (slot-mapped set member). Builder sets it iff a stamp is present (every public `format` does). |
| 3 | `KV_VOLUME_LIFECYCLE` | set has lifecycle history (non-legacy volume record / non-identity slot map / drain). Stamped at the FIRST lifecycle commit, bit-before-record. Never at format. |
| 4 | `KV_SLOT_MIGRATION` | VL5b-extended stamps (explicit `native_slot` + per-slot cursors) and guest-keyspace records. Builder sets it with bit 2. |
| 5 | `KV_LAYOUT_DELTAS` | the volume may carry `Delta`-kind layout records (`LayoutDelta` wire). Stamped bit-before-first-delta by the backend; never at format. |
| 6 | `KV_DYNAMIC_ROUTING` | stamps use stride-run slot sets over the DERIVED width W=2^16. REQUIRED when the deciding binary knows the bit — absent ⇒ "frozen routing width … reformat required". Always set by `plan`. |
| 7 | `KV_DURABLE_TERM` | `writer_claim` carries a `term`; `WRITER_TERM_XATTR` era ladder. OPTIONAL. Always set by `plan`. |
| 8 | `KV_PARTITIONED_APPEND` | ring / bitmap pages / 32-slot ledger partitioned per appender (page-interleaved bitmap ownership `page % writers`). OPTIONAL; one of the nine mw bits. |
| 9 | `KV_BLOCK_REFCOUNTS` | durable `TREE_BLOCK_REFS` (kind 6) — one record per block reference. OPTIONAL; one of the nine mw bits; test seam `SQUEEZEFS_TEST_STAMP_BLOCK_REFS=1` adds it alone. |
| 10 | `KV_WRITER_SCOPED_STAGING` | staged keys carry `:w_{16 hex}` writer scope; staging marker `{gen}@node:{16 hex}`. OPTIONAL; mw bit; seam `SQUEEZEFS_TEST_STAMP_WRITER_SCOPE=1`. |
| 11 | `KV_MULTI_WRITER_DATA` | multi-writer data plane capability gate; the TERMINAL mw bit: "bit 11 set ⇒ all nine set" (`multi_writer_class()`). |
| 12 | `KV_INO_LANES` | per-writer ino lanes: local `i` ∈ lane `(i−2) % W`. OPTIONAL; mw bit. |
| 13 | `KV_BLOCK_KEY_INCARNATION` | block keys `offset@<base36 incarnation>`; stamp `(term<<40)|lane_seq`. REQUIRES bit 7 (stamper refuses otherwise, `:1830-1854`). mw bit. |
| 14 | `KV_CLAIM_SET` | `claim_set` record at ino 1 (writer identities + PR registrant keys). mw bit. |
| 15 | `KV_LAYOUT_VERSIONS` | layout deltas carry `(base_version, version)` (wire flag bit 4). mw bit. |
| 16 | `KV_BLOCK_MAP_TREE` | block-map KV tree (kind 7) for PB-class files; layout head `kvmap:1`. OPTIONAL; stamped by the first crossing, never at format. |
| 17 | `KV_SYMMETRIC_FOREST` | the slot-tree FOREST: one mixed-kind tree per routing slot, control tree 0 (`TREE_CONTROL=8`), fixed ledger names tree 0 + native slot tree, **and** `appender_dir`. Stamped by every default `format`; absent on `--single-writer`. Since PR 14 presence-REQUIRED for a writable mount of the mw class. |

`MULTI_WRITER_FORMAT_BITS = bits {7,8,9,10,11,12,13,14,15}` (`:605-613`) — stamped as ONE act.
`FEATURES_INCOMPAT_KNOWN = bits 0..=17` (`:617-634`). Bits 18..63 unknown ⇒ refuse.
`FEATURES_RO_KNOWN = 0` (`:638`): any set ro bit is unknown ⇒ classification SUCCEEDS, the mount
is read-only (mutations withheld — `tests/kv_backend_tests.rs:2094 v3_unknown_ro_bit_withholds_mutations`).
Bit numbering collisions happened 4× in Rust (8/9, 10/11, 12/13, 12/14); disjointness pinned by
`incompat_bits_are_single_bit_and_pairwise_disjoint`. Zig: keep ONE table, test single-bit + pairwise-disjoint + union == KNOWN.

Fresh-format feature word (`SuperblockV3::plan`, `:797-800`): `bit0|bit1|bit6|bit7`.
Builder adds (`builder.rs:496-560`): `bit2|bit4` iff membership stamp (always, in public format);
`MULTI_WRITER_FORMAT_BITS` iff `multi_writer`; `bit17` iff `symmetric`; test seams bit 9 / bit 10.
⇒ **default `format`** word = `0b10_1111_1111_1101_0111` = `0x2FFD7` (bits 0,1,2,4,6,7,8,9,10,11,12,13,14,15,17 — bit 16 is NEVER stamped at format; verified against `ref-sym.img` sector 0 bytes [16..24) = `d7 ff 02 00 …`).
⇒ **`--single-writer`** word = bits {0,1,2,4,6,7} = `0xD7`.

### 1.3 Decoder totality (`decode_sector_with_known`, `:924-1033`) — order of checks, exact messages
1. `buf.len() != 4096` ⇒ Corrupt `"v3 superblock sector must be 4096 bytes, got {n}"`.
2. magic ≠ `METALV01` ⇒ `"bad v3 superblock magic (corrupted or foreign volume)"`.
3. version ≠ 3 ⇒ `"v3 decoder handed superblock version {v} (classification bug)"`.
4. checksum ⇒ `"superblock checksum mismatch: stored {:#018x}, computed {:#018x} — corrupted superblock"`.
5. decode all fields (no bounds reads beyond 4096; all fixed widths).
6. bit 0 absent ⇒ `"v3 superblock without the KV_V3 incompat bit (corrupt feature field)"`.
7. bit 1 absent ⇒ `"pre-watermark v3 volume: formatted before the node-seq mint watermark (Finding A) and no longer supported; reformat required"`.
8. `known & bit6 != 0 && sb & bit6 == 0` ⇒ `"v3 volume formatted with a frozen routing width (pre-dynamic-meta-routing) — no longer supported: routing widths are derived now, never chosen; reformat required (`squeezefs format --force`, destroys the old contents)"`.
9. `unknown = sb.features_incompat & !known; unknown != 0` ⇒ `"unknown incompatible feature bits on v3 superblock: bit A, bit B ({unknown:#x}) — upgrade squeezefs to mount this volume"` (bits listed ascending, each `bit {b}`, joined by `", "`).
10. `appender_dir != (0,0) && !bit17` ⇒ `"v3 superblock names an appender directory but carries no incompat bit 17 — the directory is part of bit 17's meaning (design-symmetric-metadata §7.1)"`.
11. `validate_geometry()` (§1.5). Unknown ro bits: pass.

### 1.4 `classify_sector0` (`:1222-1255`) — the version gate precedes the checksum
- `len != 4096` ⇒ Corrupt `"sector-0 image must be 4096 bytes, got {n}"`.
- magic all-zero ⇒ `Blank` (mount: `"{path} is not formatted (zeroed superblock) — run `squeezefs format` first"`, `backend.rs:2355`; format: proceed).
- magic ≠ METALV01 ⇒ `"invalid superblock magic {magic:?} (expected {MAGIC:?}) — corrupted or foreign volume"`.
- version ≤ 2 ⇒ `V2Legacy` (never decoded; mount: `"Metadata volume {path} is format v2, which is no longer supported (this binary mounts only format v3) — v2 volumes must be reformatted: run `squeezefs format` (destroys the old contents)"`, `meta_backend/mod.rs:207-213`).
- version == 3 ⇒ `V3(decode_sector)`.
- version > 3 ⇒ `"unsupported metadata format version {v} (this binary supports <= 3) — upgrade squeezefs"` (tested: message must NOT contain "checksum", `kv_backend_tests.rs:430-437`).

### 1.5 `validate_geometry` (`:1058-1154`) — every message prefixed `"v3 superblock geometry: "`
- `NodeLayout::new(node_size)` must pass (`node.rs:292-298`: `"invalid node_size {n}: must be a 4 KiB multiple in [65536, 1048576]"`).
- `root_ledger.start < 4096` ⇒ `"root ledger at {s} overlaps sector 0"`.
- `root_ledger.len != 131072` (= 32 × 4096, `checkpoint.rs:244-248`) ⇒ `"root ledger length {l} != the fixed 131072"`.
- `journal.start < ledger.end || journal.len == 0 || journal.len % 4096 != 0` ⇒ `"journal [{s}, +{l}) must follow the ledger in whole 4 KiB pages"`.
- `alloc_bitmap.start < journal.end` ⇒ `"alloc bitmap at {s} overlaps the journal"`.
- `heap.start % 4096 != 0 || heap.len < node_size || heap.len % node_size != 0` ⇒ `"heap [{s}, +{l}) must be whole {node_size}-byte extents on a 4096-byte boundary"`.
- Placement: `triple_below = bitmap.end ≤ heap.start`; `triple_within = heap.start ≤ ledger.start && bitmap.end ≤ heap.end`; neither ⇒ `"fixed structures (ledger/journal/bitmap ending at {e}) must sit either entirely below heap.start {h} (fresh format) or entirely within the heap [{hs}, {he}) on reserved extents (migrated volume) — got neither"`. (Zig format always produces `triple_below`; the decoder must still accept `triple_within`.)
- `alloc_bitmap.len < bitmap_region_len(total_extents)` ⇒ `"alloc bitmap length {l} cannot cover {n} extents"`.
- `total < compaction_reserve_extents(total) + 4` ⇒ `"heap of {total} extents cannot hold the compaction reserve (≥ {min} required)"`.
- `appender_dir.len != 0 && (len != node_size || start < heap.start || end > heap.end || (start−heap.start) % node_size != 0)` ⇒ `"appender directory [{s}, {e}) is not one heap extent of {node_size} bytes"`.
Derived accessors: `total_extents = heap.len / node_size`; `journal_pages = journal.len / 4096`.

### 1.6 DUR-5 redundant copy (`:1290-1498`)
- `backup_offset(volume_len) = ((volume_len / 4096) * 4096) − 4096`, `None` if that is `< 4096` (`:1302-1308`). The LAST aligned sector of the volume. `plan` reserves it by computing the heap over `usable = volume_len − 4096` (`:1751`).
- `backup_slot_for(sb, len) = backup_offset(len).filter(off ≥ sb.heap.end())` — on a heap that runs to the tail (pre-DUR-5) the copy is NOT written; `log::warn!("{path}: no room for the redundant superblock copy (the heap runs to the end of the volume — a pre-DUR-5 format); sector 0 has no backup. Reformatting reserves the tail sector.")`.
- **Write order** (`write_superblock_at_generation`, `:1438-1459`): backup copy `write_at` + `fdatasync`, THEN sector 0 `write_at` + `fdatasync`. Both carry the same `generation`. A tear on sector 0 recovers FORWARD; a tear on the copy leaves the primary untouched.
- `generation = next_superblock_generation(path) = max(gen of VALID primary, gen of VALID backup) + 1` (`:1462-1477`); `sector_generation(img) = LE u64 at [128..136)` (0 on anything older).
- **Loader choice** (`classify_volume_slot`, `:1351-1402`): register the device (O_DIRECT posture), read sector 0, `classify_sector0`. If it returns `Ok(anything)` — including `Blank` and `V2Legacy` — the primary is authoritative; the backup is NEVER consulted (a stale/crafted tail can never displace a readable primary). Only on `Err` of the primary: read `backup_offset(volume_len)`; accept iff it classifies `V3(sb)` AND `off ≥ sb.heap.end()` (the recovered image must itself reserve the slot it came from); then `log::error!("{path}: sector 0 is unreadable ({primary_err}) — mounting from the redundant superblock copy at offset {off} (generation {g}). Sector 0 is repaired on the next write mount; investigate the device.")` and return `(V3, Backup)`. Otherwise return the PRIMARY's error, prefixed `"{path}: "`.
- `repair_primary_superblock` (write mounts only): if the slot was `Backup`, rewrite sector 0 at `next_superblock_generation`, fdatasync, `log::error!("{path}: repaired a damaged sector 0 from the redundant superblock copy (generation {g})")`.
- `read_backup_superblock`: the tail copy as it lies (any generation), used by offline verbs only.
- Sector 0 is written ONLY by format and the bit stampers (`set_incompat_bit`, `:1533-1555`; serialized per path by an in-process mutex `SB_WRITE_LOCKS`); checkpoints never touch it. Stamper refusals: Blank ⇒ `"{path}: cannot stamp the {what} bit on an unformatted volume — run `squeezefs format` first"`; V2 ⇒ `"{path}: format v2 is no longer supported; reformat required"`. Returns `false` if already set (no write).
- `set_symmetric_forest(path, appender_dir)` (`:1720-1754`): stamps bit 17 + dir in ONE sector write; already stamped with a DIFFERENT dir ⇒ `"{path}: bit 17 is already stamped with appender directory {:#x}+{} — refusing to re-point it at {:#x}+{}"`.

---------------------------------------------------------------------------------------------------
## 2. VOLUME GEOMETRY / FORMAT

### 2.1 `SuperblockV3::plan(volume_len, node_size, journal_len_override, uuid, hash_seed)` (`:697-810`)
```
layout      = NodeLayout::new(node_size)?                      // 4 KiB multiple in [64 KiB, 1 MiB]
root_ledger = {start: 4096, len: 131072}                       // 32 × 4 KiB slots
journal.len = override.unwrap_or(journal_ring_len(volume_len))
   override: must be % 4096 == 0 and ≥ FLOOR = 256 KiB + MAX_ENTRY_LEN(128 KiB) = 393216
             else "journal ring override {len} bytes is invalid: must be a 4 KiB multiple of at least 393216 bytes (reserve + one max entry)"
   journal_ring_len(v) = clamp(v / 64, 8 MiB, 32 MiB) / 4096 * 4096      (JOURNAL_RING_MIN=8388608, MAX=33554432; `:640-645`, `:1169-1172`)
journal     = {start: ledger.end = 135168, len: journal.len}
upper_ext   = volume_len / node_size                           // deliberate over-estimate: breaks bitmap↔heap circularity
alloc_bitmap= {start: journal.end, len: bitmap_region_len(upper_ext)}   // = ceil(upper/32576) × 2 × 4096
heap_start  = ceil(bitmap.end / node_size) × node_size          // aligned to node_size (stronger than the 4 KiB the validator demands)
usable      = volume_len − 4096                                 // DUR-5 tail sector held back
total       = (usable − heap_start) / node_size                 // saturating
min_extents = compaction_reserve_extents(total) + 4             // reserve + room for roots and growth
total < min ⇒ "metadata volume too small for format v3: {volume_len} bytes leaves {total} heap extents of {node_size} bytes after the fixed structures (superblock + ledger + {journal_len}-byte journal + bitmap end at {heap_start}); at least {min} extents are required — grow the volume or pass a smaller --meta-journal-mb / --meta-node-kib"
heap        = {start: heap_start, len: total × node_size}
features_incompat = bit0|bit1|bit6|bit7 ; features_ro = 0 ; appender_dir = {0,0}
```
Worked examples (verified arithmetic):
```
vol 64 MiB,  node 64 KiB,  override 1 MiB: journal {135168,1048576} bitmap {1183744,8192} heap {1245184, 1004×65536} total=1004 reserve=20 backup=67104768
vol 256 MiB, node 256 KiB, default:        journal {135168,8388608} bitmap {8523776,8192} heap {8650752, 990×262144}  total=990  reserve=19 backup=268431360
vol 1 GiB,   node 256 KiB, default:        journal {135168,16777216} bitmap {16912384,8192} heap {17039360, 4030×262144} total=4030 reserve=80
vol 1 TiB,   node 256 KiB, default:        journal {135168,33554432} bitmap {33689600,1056768} heap {34865152, 4194170×262144} reserve=83883
```
`validate_node_kib(kib)` (`:1179-1198`): CLI accepts ONLY `64|128|256|512|1024` ⇒ `"--meta-node-kib {kib} is not supported: allowed values are 64|128|256|512|1024 (64 KiB floor, 1 MiB ceiling — design §5.1)"`; `< 256` returns a warning naming `record_value_cap(bytes) = min(65536, node_size/4) + 256` (`node.rs:248-276`).

### 2.2 Format classes and knobs (`builder.rs:1137-1505`)
`FormatV3Options { node_size, journal_len_override: Option<u64> (--meta-journal-mb in bytes), force (--force), full_wipe (--full), format_config_xattr: Option<Vec<u8>> }`.
Public entry points → `format_v3_inner(path, len, opts, stamp, class)`:
- `format_v3` (default: `Symmetric`), `format_v3_single_writer` (`SingleWriter`), `format_v3_stamped` (set member, `Symmetric`), `format_v3_stamped_single_writer`, `format_v3_stamped_symmetric` (`SymmetricExplicit`, seam-blind), `format_v3_stamped_multi_writer_flat` (`MultiWriterFlat`: nine bits, no forest — pre-flip class; writable mount REFUSES it since PR 14).
- Class → builder flags: `Symmetric|SymmetricExplicit` ⇒ `set_multi_writer()+set_symmetric()`; `SingleWriter` ⇒ nothing; `MultiWriterFlat` ⇒ `set_multi_writer()`. Test seam `SQUEEZEFS_TEST_FORMAT_FLAT=1` turns `Symmetric` into `SingleWriter` (never the explicit forms).
- **`--single-writer` effect on the stamp: NONE.** The membership stamp always rides the ledger (bits 2/4/6 always set; `:1453-1463` "EVERY format is a stamped set member now"). The opt-out only withholds `MULTI_WRITER_FORMAT_BITS` + bit 17 + the appender region. Such a volume mounts FLAT (three per-kind trees).

`format_v3_inner` sequence (`:1424-1505`):
1. `format_preflight(path, force)` (§2.4).
2. Regular file shorter than `volume_len` ⇒ `set_len(volume_len)` (grow; never shrink).
3. `register_meta_device(path)` (O_DIRECT from the first byte).
4. `full_wipe` ⇒ `zero_range(0, volume_len)` in 1 MiB chunks.
5. Stamp: caller's, else `plan_meta_slot_set(1).stamps[0]` = `{set_uuid: random, set_epoch: 1, member_position: 0, member_count: 1, routing_width: 65536, slots_hosted: [SlotRun{start:0, stride:1, count:65536}], native_slot: Some(0), slot_cursors: []}` (`meta_backend/mod.rs:6556-6609`; for `n` members: member `pos` hosts run `{start: pos, stride: n (1 if count==1), count: ceil((W−pos)/n)}`, `native_slot: Some(pos)`).
6. `BuilderConfig { node_size, journal_len_override, hash_seed: xxh3_64(stamp.set_uuid), uuid: random v4 }` — **hash_seed is set-wide (derived from set_uuid), uuid per-volume random**.
7. Root owner = `invoking_owner()` = `(SUDO_UID, SUDO_GID)` if set else `(geteuid, getegid)` (`config_ops.rs:91-100`).
8. `format_config_xattr` ⇒ `set_xattr(1, "user.squeezefs.format_config", cfg)`.
9. `set_membership_stamp(stamp)`; class flags; `build(path, volume_len)`.

### 2.3 `ImageBuilder::build` — write order and barriers (`builder.rs:483-732`)
```
 1. register_meta_device(path)
 2. sb = plan(...); OR feature bits per flags (§1.2)
 3. zero_range(path, 0, sb.heap.start)                 // §9 quick-format hygiene: SB+ledger+ring+bitmap. Heap NOT wiped
 4. alloc = ExtentAllocator::format(total, compaction_reserve_extents(total), BUILDER_PENDING_CAP=64)
 5. layout = symmetric ? NodeLayout::new_symmetric(node_size) /* v2 stamped frames, stamp (0,0) */ : NodeLayout::new(node_size)
 6. writer = TreeWriter::new(path, layout, heap.start, alloc, node_seq_base(uuid))
       node_seq_base(uuid) = u64::from_le_bytes(uuid[0..8]) & (u64::MAX >> 1)        // `:752-754`
 7. trees (order fixed):
      forest:  [(TREE_CONTROL=8, []), (KIND_INTERIOR=0, native)]  native = all records re-keyed by forest_key(kind, legacy_key), sorted
      flat:    [(1, inodes), (2, dentries), (3, xattrs)] + [(6, [])] iff bit 9 set
    each: write_tree(tree_id, records) → (root_addr, root_seq)                       // §3.2
 8. forest only: dir_extent = alloc.claim_internal(); sb.appender_dir = {heap.start + dir_extent×node_size, node_size}
      zero_range(dir); write DirHeader{chain_index:0, next:{0,0}, pairs: (node_size/4096 − 1)/2} at dir.end − 4096
      page0 = AppenderPage::free(0, generation 1); page0.segments = [appender0_ring_extent(journal) = {journal.start + 16384, journal.len − 16384}]
      write page0 at appender0_page_offsets(journal)[page_slot_for(1) = 1] = journal.start + 4096      // slot B
 9. alloc.write_dirty_pages(path, sb.alloc_bitmap.start, generation 1)              // every page dirty ⇒ whole bitmap, slot A
10. ledger = LedgerRecord{ seq: 1, tree_roots, journal_tail_seq: 0, next_ino: builder.next_ino, alloc_bitmap_generation: 1,
                           node_seq_watermark: writer.next_node_seq, membership_stamp, append_partition: None }
    write_ledger_slot(path, sb.root_ledger.start, &ledger)      // slot = 1 % 32 = 1 ⇒ offset root_ledger.start + 4096  (NOT slot 0)
11. fdatasync                                                    // barrier over everything the SB references
12. write_superblock_v3(path, &sb)                               // backup+fdatasync, then sector 0+fdatasync (§1.6)
13. fdatasync                                                    // nothing written since 12's own trailing fdatasync: the same barrier (a step list of (offset, bytes, barrier_after) collapses it)
→ BuiltImage { superblock, ledger_seq: 1, next_ino, nodes_written, extents_allocated: nodes_written + (symmetric ? 1 : 0) }
```
Atomicity: sector 0 is zeroed at step 3 and rewritten LAST — no crash prefix is mountable by any
binary (reads `Blank`); re-running `format` after a crash needs no `--force` (Blank). A stale tail
backup is never consulted while the primary classifies (Blank is a classification). Idempotence:
same description + same `uuid` + `hash_seed` ⇒ byte-identical image (determinism contract, `:12-23`);
production randomness lives only in `uuid` and `set_uuid`.
Heap hygiene without wiping: dead-generation bset frames on reused extents fail the
`node_seq_at_write == node_seq` chain check because node seqs are namespaced by `node_seq_base(uuid)`.

**Initial image contents.** Every tree root at format is a REAL node: `write_level` on an empty
record list still emits one leaf (level 0, `min_key = ""`, `max_key = KEY_SPACE_MAX = [0xFF;32]`,
0 records, horizon 0) (`:903-907`). So the forest image has exactly two nodes minimum: tree 0
(empty) and the native slot tree holding: inode `ino 1` → `InodeValue{mode: S_IFDIR|0o755, uid/gid: invoking owner (builder default 0:0), nlink: 2, size 0, times 0}` under key `1u64 BE ‖ 0x01`;
no dentries (an empty directory has no `.`/`..` records); xattr `user.squeezefs.format_config` (if
given) under `1 ‖ 0x03 ‖ hash56 ‖ coll` with value `XattrValue{name, value}`. `next_ino = 2`.
Flat image: three leaves (inodes, dentries-empty, xattrs) — tree 2's leaf is empty.
No guest slot trees exist at format (minted lazily by the mount). The journal ring is all zero
(`journal_tail_seq = 0`, nothing to replay). Record seqs are 0 (checkpoint-covered by construction).

### 2.4 `format_preflight(path, force)` (`builder.rs:1193-1262`) — no side effects, run over ALL volumes before any wipe
- `Blank` ⇒ `Ok`.
- `V3` ⇒ `open_probe` (read-only mount); any registration/`writer_claim` with a FRESH heartbeat ⇒ `InvalidOperation("Cannot format: metadata volume is actively mounted by clients: {keys:?}")` — even with `force`. Stale (older than `CLIENT_STALE_TTL_SECS` = 45 s) never blocks. Probe failure (corrupt) degrades to the plain force gate.
- `V2Legacy` ⇒ plain force gate.
- `Err(Corrupt(reason))` (pre-watermark, unknown bits, future version, torn checksum, foreign magic) ⇒ `refused = reason`, plain force gate (such a volume cannot host a live current client by construction).
- Other `Err` (I/O) ⇒ propagate.
- `!force` ⇒ `InvalidOperation`: with reason `"Metadata volume carries prior on-disk state ({reason}); refusing to destroy it. Pass --force to reformat."`, else `"Metadata volume is already formatted as SqueezeFS; refusing to destroy it. Pass --force to reformat."`.

---------------------------------------------------------------------------------------------------
## 3. THE IMAGE BUILDER (`builder.rs`)

### 3.1 What it is
An offline producer of complete, valid, checkpointed v3 images from an in-memory description —
used by `format` (empty root + optional config xattr), by tests (deterministic fixtures), and
(its `TreeWriter`) by the offline `enable-symmetric` conversion. Not a migration tool for v2
(v2 reader deleted). API:
```
pub struct BuilderConfig { node_size: usize, journal_len_override: Option<u64>, hash_seed: u64, uuid: [u8;16] }
impl BuilderConfig { pub fn new(node_size) -> Self }                 // random hash_seed + uuid v4
pub struct ImageBuilder { cfg, layout, inodes: BTreeMap<u64, InodeSpec>, dentries: BTreeMap<u64, BTreeMap<Vec<u8>, (u64,u8)>>,
                          xattrs: BTreeMap<(u64,Vec<u8>), Vec<u8>>, next_ino, membership_stamp: Option<_>, multi_writer: bool, symmetric: bool }
pub fn ImageBuilder::new(cfg) -> Result<Self>            // root ino 1: S_IFDIR|0o755, 0:0, nlink 2, times 0; next_ino = 2
pub fn set_membership_stamp(&mut self, stamp)            // ⇒ bits 2|4 + stamp in the bootstrap ledger
pub fn set_multi_writer(&mut self)                       // ⇒ MULTI_WRITER_FORMAT_BITS
pub fn set_symmetric(&mut self)                          // ⇒ bit 17, forest layout, appender region
pub fn add_dir(&mut self, parent, name, perm, uid, gid) -> Result<u64>    // child nlink 2; parent.nlink += 1
pub fn add_file(&mut self, parent, name, perm, uid, gid, size) -> Result<u64>   // nlink 1
pub fn add_link(&mut self, ino, parent, name) -> Result<()>              // non-dir only; nlink += 1
pub fn set_xattr(&mut self, ino, name, value) -> Result<()>              // 1 + name.len + value.len ≤ record_value_cap else ValueTooLarge
pub fn set_times(&mut self, ino, atime, mtime, ctime) -> Result<()>      // ns; default 0
pub fn set_owner(&mut self, ino, uid, gid) -> Result<()>
pub fn inode_count(&self) -> u64
pub async fn build(&self, path, volume_len) -> Result<BuiltImage>
pub struct BuiltImage { superblock: SuperblockV3, ledger_seq: u64, next_ino: u64, nodes_written: u64, extents_allocated: u64 }
pub const ROOT_INO: u64 = 1; pub const FORMAT_CONFIG_XATTR: &str = "user.squeezefs.format_config";
pub fn node_seq_base(uuid) -> u64; pub(crate) async fn zero_range(path, start, len)
pub async fn digest_backend(&KvMetaBackend) -> u64        // §4.10 digest: xxh3 over (kind byte, then per live record len‖key‖len‖value) for USER kinds in kind-then-key order, excluding writer_claim/writer_term xattrs
pub async fn digest_backend_kind_set(&KvMetaBackend, kind) -> (count, wrapping Σ xxh3(len‖key‖len‖value))   // kinds 6, 7 as sets
```
Invariants enforced at description time (typed errors BEFORE any byte is written): names 1..=255 B
(`NameTooLong`), no duplicate name per directory, parent exists and is a directory, no hard link to a
directory, xattr target exists, value cap, dentry hash chain `coll_seq` ≤ 255 (`DentryChainOverflow`,
counter `META_KV_DENTRY_COLLISION_OVERFLOWS`), xattr chain ≤ 255. Inos assigned monotonically from 2
in add order; `coll_seq` assigned in name-sorted order within a `(parent, hash54)` group; xattr
`coll` in `(ino, name)`-sorted order within `(ino, hash56)`.

### 3.2 `TreeWriter` — bottom-up packing (`:769-966`)
- `usable_budget = (node_size − NODE_PAGE(4096) − BSET_FRAME_LEN(32) − BSET_HEADER_LEN(32)) × 3 / 4` (the K5 ¾ split-fill convention; 64 KiB ⇒ 46,032 B; 256 KiB ⇒ 193,488 B = the v1 `split_part_capacity`). The frame term is the v1 `BSET_FRAME_LEN` on EVERY layout — a forest image (v2 40 B frames) packs with the same budget (`usable_budget_of`, builder.rs:853–859); the Zig port keeps the quirk for byte identity.
- `chunk_by_encoded_len`: greedy runs of key-ascending records, each run ≤ budget encoded bytes (a single record never exceeds it — the value cap guarantees it); empty input ⇒ one empty chunk.
- `write_level(tree_id, level, records)`: per chunk `i`: `max_key = last chunk ? KEY_SPACE_MAX : chunk.last().key`; `extent = alloc.claim_internal()` (lowest-first); `addr = heap.start + extent × node_size`; `node_seq = ++next_node_seq`; `write_node(path, layout, {addr, node_seq, tree_id, level, min_key, max_key}, chunk, journal_seq_horizon 0)`; `min_key = key_successor(max_key) = max_key ‖ 0x00`. First `min_key = ""`.
- `write_tree`: level 0 over records; while > 1 node: separators `Record::put(max_key, 0, encode_interior_value(addr, seq) = addr u64 LE ‖ seq u64 LE)`, next level; > 255 levels ⇒ Corrupt. Returns `(root_addr, root_seq)`.
- `planned_nodes(layout, records)` = exact node count the writer would emit (capacity preflight).
- `node_seq_watermark()` = highest minted seq (ledger field); `nodes_written()`.
- Node format itself (header page `KVND`, bset frame `KBSF`, v2 frame with `appender_id ‖ g` under bit 17) is the K2 spec; `node.rs:122-150`.

---------------------------------------------------------------------------------------------------
## 4. EXTENT ALLOCATOR (`alloc_ext_core.rs` — pure, loom-checked; `alloc_ext.rs` — I/O wrapper)

### 4.1 Bitmap page image (`alloc_ext.rs:24-36`, `:134-143`, `:269-333`)
```
[0..4)    magic: u32        ALLOC_PAGE_MAGIC = 0x4B564142 ("KVAB")
[4..8)    page_index: u32   misdirected-write guard
[8..16)   generation: u64   newest-valid-wins
[16..24)  xxh3_64: u64      over the whole 4096-B image with [16..24) zeroed: h(img[..16]); h([0;8]); h(img[24..])
[24..4096) bits              LSB-first; bit b of byte i = extent page_index×32576 + i×8 + b
ALLOC_PAGE_LEN=4096  ALLOC_PAGE_HDR_LEN=24  ALLOC_PAGE_DATA_LEN=4072  ALLOC_PAGE_BITS=32576  (509 u64 words/page)
```
Region at `base`: page `i` slot A at `base + i×8192`, slot B at `base + i×8192 + 4096`.
`bitmap_pages_for(n) = ceil(n / 32576)`; `bitmap_region_len(n) = pages × 2 × 4096`.
Bit set == allocated OR pending-free. Page bits are serialized from the RAM mirror's u64 words in LE
byte order (`page_bits`, `:1202-1211`): word `w` of the page covers extents `page×32576 + 64w ..`.
**Decoder totality** (`decode_bitmap_page(buf, page_index)`): `len < 4096` ⇒ Corrupt "truncated bitmap page: {n} of 4096 bytes"; magic ≠ ⇒ `"bad bitmap page magic {:#010x} (expected 0x4b564142)"`; index ≠ expected ⇒ `"bitmap page carries index {s}, expected {e} (misdirected write)"`; checksum ⇒ `ChecksumMismatch{stored, computed}`. At LOAD every failure means "slot absent" — never loud; an all-zero region reads as a fresh all-free heap (`test_load_short_region_reads_fresh`). Encoder refuses payload > 4072 (`"bitmap page payload {n} exceeds the 4072-byte page data area"`).
**A/B selection**: per page take the slot with the highest generation among valid slots (`generation > g` strict; slot A wins ties — but writes never produce ties, law 2 below). Fresh page state `SLOT_NONE = 2` ⇒ first write goes to slot A; thereafter `target = 1 − current`.

### 4.2 Journaled deltas (`alloc_ext.rs:38-73`, `:145-267`)
Ride the journal entry framing tagged `TREE_ALLOC_RESERVED = 4`, as `Put` records keyed by the
8-byte BE extent index (`extent_key`; `EXTENT_KEY_LEN = 8`, decode refuses any other length:
`"extent key must be 8 bytes, got {n}"`). Values:
```
Allocated: [0x01]                                   ALLOC_REC_ALLOCATED = 1
Freed:     [0x02, retire_seq: u64 LE]  (9 bytes)    ALLOC_REC_FREED = 2
```
`alloc_record(extent, seq) -> (4, Put(key, seq, [1]))`; `free_record(extent, retire_seq, seq)`.
`decode_alloc_record`: exactly `[1]` ⇒ Allocated; `[2] ++ 8 bytes` ⇒ Freed; anything else ⇒ Corrupt
`"malformed allocator delta value ({n} bytes, tag {:?}) for extent {e}"` — at LOAD this is LOUD
(the entry checksum already verified, so garbage here is a writer bug). `retire_seq` in the value is
the HISTORICAL tag kept byte-for-byte; `0` = never referenced by any checkpoint ⇒ immediately
reusable; non-zero ⇒ parks (the value is no longer the gate — the record's journal POSITION is, §4.4).
A 16-byte key with the same tag is a DATA allocation delta (`vol_tag`-prefixed, `data_alloc_bitmap`) — skipped by the heap allocator's replay.
Per-key LWW by seq makes replay idempotent (alloc→free(0)→alloc folds to Allocated).

### 4.3 RAM mirror and lock-free claim (`alloc_ext_core.rs`)
```
ExtCore { words: Box<[AtomicU64]> (ceil(total/64), bit set == allocated|pending), total, reserve (per partition),
          map: PartitionMap{writers, extents_per_page}, parts: Box<[Partition]> }
Partition { free_budget: AtomicU64, hint: AtomicU64, durable_seq: AtomicU64,
            pending: Box<[PendingSlot{stamp, extent, retire_seq}]>, pending_head, pending_tail, last_retire_seq,
            overflow: Mutex<VecDeque<(extent, gate)>>, overflow_len: AtomicU64 }
```
Construction (`new(total, reserve, pending_cap)` = solo; `new_partitioned(..., map)`): asserts
`total > 0`, `reserve < total`, `pending_cap ≥ 2` (a 1-slot Vyukov ring aliases "full at p" with
"vacant for p+1"). Partitioned: `per_reserve = ceil(reserve/writers)`, `per_pending = max(2, cap/writers)`,
budget_w = extents in pages `{w, w+n, w+2n…}` (page = `extents_per_page` bits; production 32576),
hint_w = `w × extents_per_page`. Solo = one partition owning everything; `owner_of_extent` is 0 without a division.

**Claim** (`claim_in(writer, class)`, `:534-593`) — budget-then-bit:
1. `floor = User ? reserve : 0`. CAS loop on `free_budget`: `free ≤ floor ⇒ Err(NoSpace)`; else `compare_exchange(free, free−1, AcqRel, Acquire)`.
2. `start = hint.load(Relaxed).min(total)`. Up to `CLAIM_RESCAN_LIMIT = 64` passes: solo ⇒ linear sweep `start..total` then `0..start`; `try_take_bit(idx) = words[idx/64].fetch_or(1<<(idx%64), AcqRel) & mask == 0` — the 0→1 winner is unique. On success `hint.store(idx+1, Relaxed)`. Partitioned ⇒ `scan_partition` visits only owned pages (pass 1 from the hint's page upward, pass 2 wrap to the partition's first page, pass 3 the head of the hint's own page). Between passes `spin_loop` (loom: `yield_now`).
3. After 64 empty passes: `free_budget.fetch_add(1, AcqRel)`; `Err(InvariantDrift)` (RES-14 — a budget win the bitmap cannot honour; the pre-fix code spun forever).
Why safe: invariant `clear_bits ≥ free_budget + budget-winners-without-a-bit`; every release path CLEARS THE BIT BEFORE incrementing the budget (`release`, `:706-717`: `fetch_and(!mask, AcqRel) & mask != 0 ⇒ free_budget.fetch_add(1, AcqRel); hint.fetch_min(extent, Relaxed)`); the budget CAS's Acquire pairs with the release's increment, so a winner's clear bit is visible.
`mark_allocated(&mut self, extent)` (mount seeding only; sets the bit THEN decrements the owner's budget — transiently violates the invariant, hence `&mut`).
Other reads: `free_extents()` (Σ budgets, Acquire), `is_allocated`, `snapshot_words()`, `pending_count()` = Σ (head−tail) + overflow_len.

### 4.4 Pending-free gate — the exact rule (`alloc_ext_core.rs:20-56`, `:738-922`)
A freed extent enters its OWNER partition's FIFO as `(extent, gate_seq)` with the bit STILL SET and
the budget NOT credited. `gate_seq` = the freeing SMO entry's free-record **journal seq (position)**
— the entry's highest seq. It is released only by `advance_durable_in(writer, tail)` where `tail` is
the `journal_tail_seq` of a root-ledger record written by THAT appender that a COMPLETED barrier
covers (`backend.rs:16172-16189 after_durable_barrier`: drains `pending_reclaim` entries whose push
epoch `< barrier_durable`, calls `alloc.advance_durable(tail)` and `ring.advance_reusable_upto(tail)`
on the same clock). Rule: release iff `gate_seq ≤ durable_tail` (tails are entry boundaries and the
free record is never record 0 of an SMO entry, so `≤` ≡ the design's strict `tail > free.seq`;
`alloc_ext.rs:980-989`). Coverage of the TAIL — not durability of the ledger RECORD — is what proves
every flip of the freeing entry is materialized (design §4.7 Option A / design-smo-replay-currency §2-A).
FIFO = bounded Vyukov MPMC ring: producer fills slot `pos % cap` while `stamp == pos`, CAS head
`pos→pos+1`, store extent/retire_seq (Relaxed), `stamp.store(pos+1, Release)`. If `stamp < pos`:
`pos − tail ≥ cap` ⇒ `Err(PendingFreeFull)` (genuinely full); else a consumer is mid-vacate — spin
and re-read head (NOT "full": the admission headroom probe reads cursors, so a cursor-level vacancy
must be pushable; loom pins this). Consumer (`advance_durable_in`): `durable_seq.fetch_max(seq, AcqRel)`;
loop: `pos = tail`, `head`; empty ⇒ stop; `stamp != pos+1` ⇒ stop (unpublished producer); peek
`retire_seq > durable` ⇒ stop (gates non-decreasing in push order — one serialized producer per
partition, debug-asserted via `last_retire_seq.fetch_max`); CAS tail `pos→pos+1`; winner stores
`stamp = pos + cap` (Release) then `release(extent)`. Then the overflow deque drains on the same
clock under its mutex (`overflow_len > 0` check keeps the hot path lock-free). Returns released extents (wrapper marks their pages dirty).
Debug contracts of `free_pending`: `extent < total`; bit already set; gate non-decreasing per partition.
`free_pending_forced(extent, gate) -> bool(overflowed)`: try the FIFO; at cap push to the unbounded
overflow instead (never refused) — reserved for the checkpoint flush pass's own compactions and
mount-side re-parking of replayed frees (refusing those closes the pinned-floor dependency cycle
{parked frees ↔ pinned tail ↔ refused compaction}, P2 2026-07-26 §9).
Headroom probes: `pending_has_room_in(w) = (head−tail) < cap`; `pending_room_in(w) = cap − (head−tail)`
(a merge parks two). Sound because the serialized SMO task is the only producer and drains only vacate.

### 4.5 Reserve, floor, admission, space classes
- `compaction_reserve_extents(total) = max(8, total / 50)` (`alloc_ext.rs:156-158`) — `Internal` class only. 8 = three roots + growth headroom; 2 % = measured fold-and-free headroom. Pinned values: 1→8, 100→8, 400→8, 450→9, 1000→20, 4,194,304→83,886.
- `compaction_floor_extents(reserve) = reserve / 2` (`:169-171`): deletes/overwrites (net-zero compactions, 1 extent) admitted down to the floor; splits need the WHOLE reserve clear.
- **User commits never claim**; the flush pass claims (Internal) for the SMOs they force. The commit pass (`admit_heap_locked`, `backend.rs:23978-24210`, under the union leaf write locks, before ring reservation) projects each touched leaf's log end; a leaf that overflows `node_size` has its SMO extents PROMISED: `need = smo_extents_for_parts(fold, parts)` (+1 for a root leaf's new root); `is_split = need > 1`; `delta = need − already_promised`; `claimable = alloc.free_extents() − cache.heap_promised()`; admitted iff `claimable − delta ≥ (is_split ? reserve : reserve/2)`, else the member refuses `KvError::NoSpace{free: claimable, reserve: floor}` ⇒ ENOSPC (after one retry behind a checkpoint cycle when `heap_promised > 0 || pending_count > 0`, `:23267`). Promises retract on a later-leaf refusal of the same member; consumed by the SMO; `meta_kv_heap_promised` must be 0 at quiesce. A leaf of a LEASED slot promises against its region's GRANT ledger, refusing `GrantExhausted` (EAGAIN-class) when the manager is unreachable.
- `KvError::NoSpace{free, reserve}` display: `"no space: {free} free extents with the {reserve}-extent compaction reserve intact — allocation refused (ENOSPC)"`; `PendingFreeFull{pending}`: `"pending-free list full ({pending} extents awaiting durable checkpoints); checkpoint required before further frees"` (`kv/mod.rs:1029-1138`).
- **Two classes of a non-advancing cycle** (design §4.7): a cycle whose flush pass deferred a node for `NoSpace` is the SPACE STANDSTILL — counted `meta_kv_heap_full_cycles`, latches posture `meta_kv_heap_full` (one WARN per transition: `"meta volume {}: metadata heap FULL — {why} (free={} claimable, {} promised to pending SMOs, reserve={}); growth needing a new leaf answers ENOSPC, reads and deletes continue, the volume is NOT failed (§4.7 space standstill class)"`), never terminal, does not advance the wedge rung. The FAILED terminal (volume latches EIO) is the WEDGE class alone: nothing deferred for space, retirements parked, no release AND no ledger-tail advance for `PENDING_FREE_FORCE_CYCLES = 8` consecutive barriered cycles (`checkpoint.rs:1500`; audited centrally in `checkpoint_cycle`). `heap_growth_floor_clear() = free − promised − smo_extents_for_parts(fold_capacity+1, 2) ≥ reserve`.
- Pending-free cap `PENDING_FREE_CAP = 65_536` (`backend.rs:210`; `TEST_PENDING_FREE_CAP` override) is a PRESSURE VALVE: threshold SMOs check `pending_has_room` at admission and on `PendingFreeFull` force a checkpoint and retry; forced retirements bypass it into the overflow. Counters: `META_KV_PENDING_FREE_PARKED/_RELEASED/_OVERFLOW` (overflow ≈ 0 steady), `META_KV_REPLAY_ROOT_FREES_DROPPED`, `META_KV_BITMAP_GENERATION_RAISES`; gauge `meta_kv_pending_free` is the wedge detector.

### 4.6 Wrapper: `ExtentAllocator` (`alloc_ext.rs:347-1212`)
```
ExtentAllocator { core: ExtCore, pages: u64, page_states: Box<[PageState{generation: AtomicU64, slot: AtomicU64 /*0,1,SLOT_NONE=2*/}]>,
                  dirty: Box<[AtomicU64]> (1 bit/page), partition: AppendPartition, foreign_page_writes: AtomicU64,
                  replayed_parks: Mutex<Vec<(writer u16, gate seq, extent)>> }
```
- `format(total, reserve, pending_cap)` / `format_partitioned(.., part)`: all-free; solo ⇒ EVERY page dirty (first checkpoint persists the whole bitmap); partitioned ⇒ only owned pages dirty.
- `load(path, base, total, reserve, pending_cap, mounted_tail, replay: &[ReplayedEntry])` / `load_partitioned(.., part, mounted_tails: &[u64], replay: &[MergedEntry])` — `load_inner` (`:546-718`):
  1. one `read_at(base, region_len)`; short read zero-extended.
  2. per page: decode both slots; newest valid ⇒ `mark_allocated` every set bit `< total`; `page_states[p] = {gen, slot}`; none valid ⇒ `{0, SLOT_NONE}`.
  3. per writer `advance_durable_in(w, mounted_tails[w] or 0)` — seeds the coverage clock at the mounted ledger's tail (nothing at/after it is covered).
  4. fold the window's kind-4 records per extent, newest wins (`folded[extent] = (writer, entry_seq + record_index, delta)` — the gate is the record's POSITION); data-alloc keys skipped; ownership check `owner_of_extent(e) == writer` else LOUD Corrupt `"replayed allocator delta for extent {e} came from writer {w} at seq {s}, but that extent belongs to writer {o}'s bitmap partition (spec §6.2 item 3 — the appenders were not disjoint)"`.
  5. apply survivors: `Allocated` ⇒ `mark_allocated`; `Freed{retire_seq: 0}` ⇒ `release`; `Freed{_}` ⇒ `mark_allocated` (idempotent; the bit may live only in the window) and DEFER to `replayed_parks` (seq-sorted). Every touched extent's page is marked dirty. `dirty` otherwise starts all-clear on load.
- `park_replayed_frees(&live_roots: BTreeSet<u64>) -> (parked, dropped)` (`:750-787`): must run once after every tree is open (roots final) and before any post-mount SMO. A free whose extent is a LIVE ROOT is DROPPED (counted `META_KV_REPLAY_ROOT_FREES_DROPPED`, warn) — the unpublished-root-swap carve-out: a root swap journals no pointer record, so a kill before the naming ledger record makes the predecessor live again while its `free(old)` is in the window. Others `free_pending_forced` (overflow allowed; counted). Called from `backend.rs:2906`.
- `claim_user() / claim_user_in(writer) / claim_internal()`: core claim + `mark_dirty(extent)`; `NoSpace` ⇒ `KvError::NoSpace{free: free_extents_in(writer), reserve}`; `InvariantDrift` ⇒ `log::error!("KV extent allocator INVARIANT DRIFT: the free budget won an entitlement the bitmap could not honour after 64 full rescans (free={}, reserve={}, total={}). Refusing the claim instead of spinning; the volume needs an fsck (class C2/C3)")` then `NoSpace`.
- `free_pending(extent, gate) -> Result<(), KvError::PendingFreeFull{pending}>` (+PARKED counter); `free_pending_forced` (+PARKED, +OVERFLOW if overflowed); `pending_has_room()`, `pending_has_room_for(n)`.
- `advance_durable(tail) -> released count` / `advance_durable_in(writer, tail)`: core drain + mark released pages dirty + `META_KV_PENDING_FREE_RELEASED += n`.
- `release_unpublished(extent)`: direct `release` + dirty (a build abandoned before publication — never referenced by any checkpoint).
- `has_dirty_pages()`; `resume_generation()` = max page generation on disk; mount's `checkpoint_seq` starts at `max(ledger.seq, alloc.resume_generation())` (`backend.rs:3051`) so a new write never ties.
- `write_dirty_pages(path, base, generation) -> Vec<u32 pages written ascending>` (`:1078-1191`), serialized-checkpoint-task only:
  1. snapshot-and-clear: `dirty[w].swap(0, AcqRel)` per word; collect pages `< pages`. Empty ⇒ `Ok([])`.
  2. partitioned: pages not owned ⇒ `foreign_page_writes += n` + warn (tripwire, still written).
  3. `words = core.snapshot_words()`; per page: `page_gen = (has_copy && generation ≤ cur_gen) ? cur_gen + 1 /* law 2: never tie or trail — counted META_KV_BITMAP_GENERATION_RAISES, warn */ : generation`; `target = slot == SLOT_NONE ? 0 : 1 − slot`; image = `encode_bitmap_page(page, page_gen, page_bits)`; offset `base + page×8192 + target×4096`.
  4. one `write_at` or `write_at_batch`. On ANY error (encode or I/O) **law 1**: `fetch_or` every snapshotted page back into `dirty` and return the error (allocator deltas create no dirty-node floor — dropping the dirty set would let the next tail advance past the only copy of those alloc/free records).
  5. on success record `{page_gen, target}` per page. Durability rides the CALLER's barrier; the caller records `generation` in its ledger record as `alloc_bitmap_generation`.
- Checkpoint usage (`checkpoint.rs:2476-2515`): `ckpt_seq = checkpoint_seq.fetch_add(1)+1`; `write_dirty_pages(.., ckpt_seq)`; barrier; ledger record with `seq = alloc_bitmap_generation = ckpt_seq`; barrier. So **`bitmap_generation` in the ledger ≡ the ledger record's seq** that persisted those pages (format: both 1).
- Partition map (`partition_map_for(AppendPartition)`): solo ⇒ `PartitionMap::solo(32576)`; else `new(writers, 32576)`. `owns_extent`, `partition()`, `foreign_page_writes()`.

### 4.7 Loom models (`loom-models/src/lib.rs:1600-2048`, `#[path]`-includes `alloc_ext_core.rs`) — what each pins and which weakening fails
1. `alloc_ext_no_double_alloc_under_concurrent_claimers` — 3 extents, reserve 0, cap 2; one seeded claim parked (gate 1) and drained; two threads × 2 claims: all claimed extents distinct, exactly 3, budget settles to 0. Fails if `try_take_bit` were a load+store instead of `fetch_or`, or if release incremented the budget before clearing the bit (a budget winner finds no bit ⇒ `InvariantDrift` panics the model).
2. `alloc_ext_pending_free_never_claimable_before_covering_tail` — 1 extent; `free_pending(e, 7)`; `advance_durable(6)` must release NOTHING (record durability ≠ flip coverage); a thread advances to 8 while main claims: success ⇒ `durable_seq ≥ 7`; `InvariantDrift` unreachable; exactly one of {raced claim, retry} wins. Fails if the gate compared `retire_seq > durable` loosely (`≥`), or if the drain released before stamping the vacate.
3. `alloc_ext_headroom_observed_at_admission_holds_at_push` — cap 2 saturated; a racing `advance_durable(4)` drains one; if `pending_has_room()` reads true the post-swap `free_pending` MUST succeed. Fails if the `stamp < pos` branch reported "full" during a consumer's mid-vacate window (the cursor-distance check at `:790-793` is what passes).
4. `alloc_ext_forced_overflow_gate_and_conservation` — cap 2 saturated (gates 3, 5); forced push at gate 7 races `advance_durable(6)`; a racing claim may win a or b (and then `durable_seq ≥` its gate) but NEVER c; after `advance_durable(8)` exactly 3 claims total, `pending_count == 0`, budget 0. Fails if the overflow drained on a different clock or released without the front-stop.
5. `alloc_ext_reserve_isolated_from_user_claims` — 3 extents, reserve 2: exactly one of two racing User claims wins; free stays 2; Internal drains the reserve while User is still refused; exhausted heap refuses Internal too; 3 unique live claims. Fails if the User floor were checked after the CAS or with `<` instead of `≤`.
6. `alloc_ext_partitioned_claims_never_cross` — 4 extents, 2/page, 2 writers: writer 0 gets {0,1}, writer 1 {2,3}, both then `NoSpace`.
7. `alloc_ext_partitioned_coverage_gates_never_cross` — both park at gate 100 (two ring spaces); `advance_durable_in(0, 100)` releases only writer 0's; writer 1's clock stays 0; `advance_durable_in(1, 99)` empty, `(1, 100)` releases theirs. Fails with a shared clock.

---------------------------------------------------------------------------------------------------
## 5. PUBLIC API (Rust pseudocode, one line each)

### superblock.rs
```
pub const MAGIC_VALUE: &[u8;8] = b"METALV01"; pub const SECTOR_SIZE = 4096; pub const SUPERBLOCK_V3_VERSION: u32 = 3; pub const SUPERBLOCK_V3_LEN = 4096
pub const FEATURE_INCOMPAT_*: u64 (18 bits, §1.2); pub const WRITER_SCOPED_STAGING_BIT: u32 = 10; pub const MULTI_WRITER_FORMAT_BITS; FEATURES_INCOMPAT_KNOWN; FEATURES_RO_KNOWN = 0
pub const JOURNAL_RING_MIN = 8 MiB; JOURNAL_RING_MAX = 32 MiB
pub struct ExtentRef { start: u64, len: u64 }  fn end() -> u64
pub struct SuperblockV3 { node_size: u32, features_incompat: u64, features_ro: u64, root_ledger, journal, alloc_bitmap, heap: ExtentRef, uuid: [u8;16], hash_seed: u64, appender_dir: ExtentRef }
  pub fn plan(volume_len, node_size, journal_len_override, uuid, hash_seed) -> Result<Self>   // §2.1
  pub fn total_extents() -> u64; journal_pages() -> u64; unknown_incompat() -> u64; unknown_ro() -> u64
  pub fn block_map_tree_stamped() -> bool /*bit16*/; symmetric_forest_stamped() /*bit17*/; multi_writer_class() /*bit11*/
  pub fn encode_sector() -> Result<Vec<u8>>  /*generation 0*/; encode_sector_at_generation(g) -> Result<Vec<u8>>
  pub fn decode_sector(buf) -> Result<Self>; decode_sector_with_known(buf, known_mask) -> Result<Self>   // §1.3
pub fn journal_ring_len(volume_len) -> u64; pub fn validate_node_kib(kib: u32) -> Result<(usize, Option<String>)>
pub enum VolumeFormat { Blank, V2Legacy, V3(SuperblockV3) }; pub enum SuperblockSlot { Primary, Backup }
pub fn classify_sector0(&[u8]) -> Result<VolumeFormat>; pub fn sector_generation(&[u8]) -> u64; pub fn backup_offset(volume_len) -> Option<u64>
pub fn register_meta_device(path) -> Result<MetaIoMode>
pub async fn classify_volume(path) -> Result<VolumeFormat>; classify_volume_slot(path) -> Result<(VolumeFormat, SuperblockSlot)>
pub async fn read_backup_superblock(path) -> Result<Option<SuperblockV3>>; write_superblock_v3(path, &sb) -> Result<()>; repair_primary_superblock(path) -> Result<bool>
pub async fn set_{volume_lifecycle,guest_slots,layout_deltas,block_map_tree,layout_versions,slot_migration,durable_term,block_refcounts,partitioned_append,writer_scoped_staging,multi_writer_data,ino_lanes,block_key_incarnation,claim_set}_bit(path) -> Result<bool /*newly set*/>
pub async fn set_symmetric_forest(path, appender_dir: ExtentRef) -> Result<bool>
```
### builder.rs — see §3.1; plus `format_preflight`, `format_v3*` (§2.2), `pub const FORMAT_FLAT_SEAM = "SQUEEZEFS_TEST_FORMAT_FLAT"`.
### alloc_ext_core.rs
```
pub enum AllocClass { User, Internal }; pub enum ClaimError { NoSpace, InvariantDrift }; pub struct PendingFreeFull
pub struct PartitionMap; fn solo(epp), new(writers, epp), writers(), extents_per_page(), is_solo(), owner_of_page(p), owner_of_extent(e)
pub struct ExtCore; fn new(total, reserve, pending_cap), new_partitioned(total, reserve, cap, map), total(), reserve(), map()
  free_extents(), free_extents_in(w), pending_count(), pending_count_in(w), pending_has_room(), pending_has_room_in(w), pending_room_in(w)
  durable_seq(), durable_seq_in(w), claim(class) -> Result<u64, ClaimError>, claim_in(w, class), inflate_free_budget_for_test(n)
  mark_allocated(&mut self, e), release(e), free_pending(e, gate) -> Result<(), PendingFreeFull>, free_pending_forced(e, gate) -> bool
  advance_durable(seq) -> Vec<u64>, advance_durable_in(w, seq) -> Vec<u64>, is_allocated(e), snapshot_words() -> Vec<u64>
```
### alloc_ext.rs
```
pub const ALLOC_PAGE_LEN=4096, ALLOC_PAGE_HDR_LEN=24, ALLOC_PAGE_DATA_LEN=4072, ALLOC_PAGE_BITS=32576, ALLOC_PAGE_MAGIC=0x4B564142, ALLOC_REC_ALLOCATED=1, ALLOC_REC_FREED=2, EXTENT_KEY_LEN=8
pub fn compaction_reserve_extents(total) -> u64; compaction_floor_extents(reserve) -> u64; bitmap_pages_for(n); bitmap_region_len(n)
pub fn extent_key(e) -> [u8;8]; decode_extent_key(&[u8]) -> Result<u64>
pub enum AllocDelta { Allocated{extent}, Freed{extent, retire_seq} }; pub fn alloc_record(e, seq) -> (u8, Record); free_record(e, retire_seq, seq) -> (u8, Record); decode_alloc_record(&Record) -> Result<AllocDelta>
pub fn encode_bitmap_page(page_index: u32, generation: u64, bits: &[u8]) -> Result<Vec<u8>>; decode_bitmap_page(buf, page_index) -> Result<(u64 gen, &[u8] bits)>
pub struct ExtentAllocator; fn format(total, reserve, cap), format_partitioned(.., part), async load(path, base, total, reserve, cap, mounted_tail, &[ReplayedEntry]), async load_partitioned(.., part, &[u64] tails, &[MergedEntry])
  park_replayed_frees(&BTreeSet<u64>) -> (u64, u64); total_extents(); partition(); owns_extent(e); foreign_page_writes(); reserve_extents(); free_extents(); pending_count(); durable_seq(); is_allocated(e); has_dirty_pages(); resume_generation()
  claim_user() -> Result<u64, KvError>; claim_user_in(w: u16); claim_internal(); free_pending(e, gate) -> Result<(), KvError>; free_pending_forced(e, gate); pending_has_room(); pending_has_room_for(n)
  advance_durable(tail) -> u64; advance_durable_in(w: u16, tail) -> u64; release_unpublished(e); async write_dirty_pages(path, base, generation) -> Result<Vec<u32>>
```

---------------------------------------------------------------------------------------------------
## 6. TESTS (name — one sentence; PURE = no device; FILE = temp file via uring_fs; MOUNT = backend open)

### tests/kv_alloc_tests.rs
- `test_compaction_reserve_policy` — PURE; pins `max(8, total/50)` at 1,100,400,450,1000,4Mi.
- `test_extent_key_memcmp_order_and_bounds` — PURE; BE keys sort numerically; 3- and 9-byte keys refused.
- `test_alloc_delta_records_roundtrip_and_reject_garbage` — PURE; alloc/free records round-trip through entry payload framing; `[]`, `[99]`, `[2,1,2,3]`, `[1,0]` and a 2-byte key are Corrupt.
- `test_core_claims_unique_reserve_isolated` — PURE; 16/4: 12 unique User claims then NoSpace; 4 Internal; exhausted refuses Internal.
- `test_core_pending_free_gated_until_durable_seq` — PURE; parked extents unclaimable at ANY pressure; advance(1) releases none; advance(2) exactly the first; repeat advance is a no-op.
- `test_core_pending_free_cap_and_lap_recycling` — PURE; cap 2 refuses the third (`PendingFreeFull`), extent stays claimed; drain recycles slots.
- `test_core_concurrent_claims_unique_across_pending_drain` — PURE (threads); 2048 extents, 32 parked, 8 claimers vs a drainer: no duplicates, all 2048 claimed, budget 0.
- `test_wrapper_typed_enospc_and_pending_pressure` — PURE; `NoSpace{free:2, reserve:2}`, message contains "ENOSPC"; `PendingFreeFull{pending:2}`; `release_unpublished` immediately reclaimable; `advance_durable(5)` returns 2.
- `test_bitmap_page_image_roundtrip_and_damage_detection` — PURE; exact image round-trip; wrong magic/index/checksum/oversize payload all invalid.
- `test_bitmap_ab_alternation_newest_valid_selection` — FILE; gen 1 → slot A, gen 2 → slot B (A keeps 1), nothing dirty ⇒ `[]`, load picks gen 2, `resume_generation() == 2`, next write gen 3 → slot A.
- `test_multi_page_dirty_tracking_and_boundaries` — FILE; 2 pages: format persists `[0,1]`; a release at extent 32576 dirties only page 1; load matches.
- `test_load_short_region_reads_fresh` — FILE; a 100-byte file loads as all-free, generation 0.
- `test_journal_replay_rebuilds_alloc_free_and_pending` — FILE (ring + bitmap in one file); pages at gen 1 + journal-only deltas: non-zero-tag frees keep their bits and park only after `park_replayed_frees(∅) == (2,0)`; tag-0 chain folds to allocated; parked extents never claimable; `advance_durable(e1_free_seq − 1)` drains exactly e0, then e1.
- `test_load_rejects_malformed_replayed_delta` — FILE; `[9,9]` under kind 4 fails load loud; a kind-1 record in the window is ignored.

### tests/allocation_act_lock_law_tests.rs — OUT OF SCOPE for this spec: a PURE static source census over `src/` of the DATA-plane `allocate_placed_block[_under_guard]` call sites (file, enclosing fn, count) asserting each caller's lock class; not the KV extent allocator.

### tests/format_guard_tests.rs (64 MiB temp files, node 64 KiB, ring 1 MiB)
- `blank_volume_formats_without_force` — MOUNT-free FILE; blank formats without `--force`.
- `formatted_volume_without_force_is_refused` — FILE; message contains "already formatted" and "--force".
- `formatted_volume_with_force_is_reformatted` — FILE.
- `live_client_blocks_format_even_with_force` — MOUNT; a fresh `client:{id}` registration refuses with "actively mounted".
- `stale_client_does_not_block_forced_format` — MOUNT; a registration older than `CLIENT_STALE_TTL_SECS` does not block.
- `preflight_refuses_without_side_effects` — MOUNT; refused preflight leaves the file byte-identical and the planted xattr readable.
- `preflight_allows_forced_idle_reformat` — MOUNT.
- `legacy_v2_volume_guarded_and_reformattable` — FILE+MOUNT; crafted `METALV01‖2u32`: mount refuses "no longer supported", format refuses without force, `--force` reformats to a mountable v3.
- `pre_watermark_v3_volume_guarded_and_force_reformattable` — FILE+MOUNT; forged bit-1-absent SB: mount refusal names "watermark" + "reformat required"; format without force refuses pointing at `--force`; `--force` succeeds.
- `mixed_pre_watermark_volume_set_force_reformats_all` — FILE+MOUNT; 4 volumes, 2 forged: all-volumes preflight wipes nothing; `--force` reformats all.
- `force_gate_over_refused_superblock_buries_all_residue_classes` — MOUNT; after `--force` over a refused SB: ring bytes all-zero, heap frames survive but no ghost dentry served, first create mints ino 2, fresh uuid.
- `cli_format_force_reformats_pre_watermark_set_then_mounts` — DEVICE/CLI (spawns the `squeezefs` binary, needs FUSE); end-to-end user flow.

### tests/kv_backend_tests.rs (format/mount parts; 64 MiB, node 64 KiB, ring 1 MiB, `TEST_SEED=0x5EED_CAFE_F00D_D00D`, `TEST_UUID=b"kv-backend-test!"`)
- `version_gate_distinguishes_blank_v2_v3_foreign_future` — FILE+MOUNT; Blank (mount error contains "format"), V2Legacy ("no longer supported"+"v2"+"reformat"), V3 (node_size/journal.len/hash_seed/bit 0 round-trip), foreign magic ("foreign"|"magic"), version 99 stamped on BOTH copies ⇒ "upgrade", not "checksum".
- `v3_superblock_checksum_covers_the_whole_sector` — FILE+MOUNT; a byte flipped at offset 900 of sector 0 alone RECOVERS via the backup; flipped on both ⇒ "checksum" from classify and from open.
- `v3_unknown_incompat_bit_refuses_naming_it_and_unknown_ro_does_not` — FILE+MOUNT; lowest unknown bit ⇒ message names `bit {b}`; bit-1-absent ⇒ "reformat required"+"watermark"; `features_ro |= 1<<3` ⇒ classifies, mounts, `unknown_ro() == 8`.
- `journal_ring_clamp_math` — PURE; 256 MiB→8 MiB, 1 GiB→16 MiB, 4 GiB→32 MiB, 100 TiB→32 MiB, page-aligned.
- `node_kib_knob_floor_warning_and_allowed_set` — PURE; allowed {64,128,256,512,1024}; <256 warns naming `record_value_cap`; 32/0/100/2048 refused.
- `v3_plan_uses_clamp_default_and_honors_override` — PURE; default = clamp (8 MiB on 64 MiB), override 2 MiB honoured, SB|ledger|ring|bitmap|heap ordered, 4 MiB volume fails.
- `v3_reports_cow_contract_class_with_physical_probe_alongside` — MOUNT; `atomicity_contract() == "cow-checksummed"`.
- `builder_output_is_deterministic_and_digest_stable` — FILE+MOUNT; two builds of one description are byte-identical (xxh3 of the whole image), `extents_allocated == nodes_written + forest`, digests equal.
- `builder_rejects_bad_descriptions_typed` — PURE; duplicate name, missing/non-dir parent, dir hard link, 256-byte name (`NameTooLong`), 17 KiB xattr on a 64 KiB-node volume (`ValueTooLarge`).
- `v3_quick_reformat_buries_previous_generation_records` — MOUNT; 200 files, clean shutdown, quick `--force` reformat: no `gen1_*` ghosts; every format mints a fresh uuid.
- `v3_dd_zeroed_head_virgin_format_buries_all_residue_classes` — MOUNT; after `dd` zeroes 1 MiB of the head the Blank path still re-zeroes the whole ring/ledger/bitmap and buries heap residue by seq namespace.
- `v3_format_guards_match_the_preflight_contract` — MOUNT; blank formats; refused without `--force` (message has `--force`); `--force` reformats; `user.squeezefs.format_config` readable at ino 1; crafted v2 refused then reformatted.
- `v3_mount_surfaces_ledger_and_allocator_state` — MOUNT; `mounted_ledger().seq == 1`, `next_ino` matches, `SUPERBLOCK_V3_VERSION == 3`, `free_extents == total − extents_allocated`, forest ⇒ `all_trees().len() == 2` (tree 0 + native) else 3 flat trees; builder times round-trip, unset = 0.
- `format_time_directories_carry_the_two_plus_subdirs_law`, `a_production_format_roots_nlink_is_two` — MOUNT; dir nlink = 2 + subdirs; root nlink 2.

---------------------------------------------------------------------------------------------------
## 7. NUMERIC CONSTANTS — with WHY (and measured numbers)

| value | why |
|---|---|
| sector 4096 | the metadata grain; one O_DIRECT sector; superblock = one durable-coverage unit |
| version 3 | v2 was the fixed-geometry format; `version > 2` already failed loud in every shipped binary ("upgrade squeezefs") |
| node_size default 262144; range [65536, 1048576]; 4 KiB multiple | bcachefs's shipped default; fanout ≈ 7,000 at 256 KiB ⇒ 100 M records at depth ≤ 3 with < 3 MiB interior; CLI restricts to powers of two 64..1024 KiB |
| record_value_cap = min(65536, node_size/4) + 256 | Linux `XATTR_SIZE_MAX` at the default node; `/4` keeps ≥ 4 records per leaf; +256 = 1-byte name_len + 255-byte name envelope |
| root ledger 32 × 4096 = 131072 at offset 4096 | 32 round-robin slots; one 128 KiB mount read; floor 2 slots per partitioned writer so a torn newest slot has a same-writer predecessor |
| journal `clamp(vol/64, 8 MiB, 32 MiB)` | resolved OQ 1: a flat 32 MiB would be ~12 % of a 256 MiB volume; 8 MiB floor still holds the 256 KiB reserve + 128 KiB max entry many times |
| override floor 393,216 = 256 KiB + 128 KiB | checkpoint-task reserve carve-out `max(256 KiB, ring/64)` + one `MAX_ENTRY_LEN` must always be admissible or a legal tx could never commit |
| MAX_ENTRY_LEN 131072 | 2× the largest legal record (64 KiB value) plus headers |
| bitmap page: 24 B header, 4072 B bits, 32576 extents/page; 1 TiB heap ⇒ 4 M bits ⇒ 512 KiB × 2 | 4 KiB = one torn-write unit; A/B pairs make a tear damage only the copy being replaced |
| DUR-5 backup = last aligned sector; costs ≤ 1 heap extent | no reserved space beside sector 0 on any existing volume (ledger at 4096); the tail needs no layout change |
| compaction reserve `max(8, 2 %)` | 8 = roots + growth floor (`plan` requires reserve + 4); 2 % = fold-and-free headroom; Internal-only so compaction can always run at user ENOSPC |
| compaction floor = reserve/2 | half admits deletes/overwrites on a full volume, half stays the flush pass's for interior cascades and under-projection (meta_volume_full tie test) |
| PENDING_FREE_CAP 65,536; BUILDER_PENDING_CAP 64 | pressure valve far above steady state (any parked retirement drains within 2 barriered cycles); builder never frees |
| pending_cap ≥ 2 | 1-slot Vyukov ring aliases stamps `p+1` (full vs vacant) — a gate bypass |
| CLAIM_RESCAN_LIMIT 64 | legitimate need is O(racing releases) — a generous ceiling; the pre-fix loop spun forever on budget/bitmap drift |
| PENDING_FREE_FORCE_CYCLES 8 | healthy convergence ≤ 2 cycles (first discharges dying floors, second's tail covers parked frees); 8 leaves only a tail nothing can discharge |
| `part()` indexes, never `%` | measured ~7 ns/claim ≈ 11 % of a claim on a solo volume (`.benchmarks/2026-08-05-mw-partitioned-append.md` §3) |
| bitmap partition unit = page, ownership `page % writers` | the page is the A/B write unit (one owner keeps alternation single-appender); interleaving spreads a dense low-end allocation evenly |
| ¾ leaf fill at build | the K5 split-fill convention: append headroom before the first SMO |
| `node_seq_base = uuid[0..8] LE & (u64::MAX >> 1)` | quick format leaves heap frames; uuid-namespaced seqs make a cross-generation chain match as improbable as a checksum collision; top bit clear = 2^63 headroom |
| hash_seed = xxh3_64(set_uuid) | set-wide: a migrated slot's seeded dentry hashes must resolve on its new volume; same secrecy class as the per-volume seed (plaintext in sector 0) |
| CLIENT_STALE_TTL_SECS 45 | mount-guard staleness law (D0) |
| W = 65536, MINT_SPREAD = STAMP_MAX_CURSORS/4 = 64, STAMP_MAX_RUNS 128, STAMP_MAX_CURSORS 256 | u16 slot id; worst-case stamped ledger record 3,765 B < 4,096 |
| appender-0 reserved pages 4 (16 KiB) | its `[A,B,R0,R1]` page slots are the fixed ring's first four pages |
| dir pairs per extent `(node_size/4096 − 1)/2` | header page last; e.g. 256 KiB ⇒ 31 pairs |
| Measured (comments): bit-9 derived accounting kept at format while wiring was incomplete (safety); the pinned-floor wedge of 2026-07-26 and the 2026-09-09 full-heap sweep motivated forced retirement + heap admission; Finding A (2026-07-13) motivated bit 1 and the uuid seq namespace. |

---------------------------------------------------------------------------------------------------
## 8. DECODER TOTALITY SUMMARY (Zig must match: never panic, never read past a bound, never allocate off an unchecked count)
- **Superblock**: fixed 4096-byte image; every field fixed-width at fixed offsets; checks in the order of §1.3/§1.4 (length → magic → version → checksum → KV_V3 → watermark → dynamic-routing → unknown bits → appender_dir-without-bit-17 → geometry). The version check precedes the checksum. `Blank` only for all-zero magic. Unknown ro bits never refuse. Primary readable ⇒ backup ignored.
- **Bitmap page**: fixed 4096-byte image; length → magic → page_index → checksum; bits beyond `total_extents` are ignored at load (`first..last` clipped to total); invalid slot = absent (never loud); both slots invalid = fresh page; short/absent region = all free.
- **Allocator delta**: key exactly 8 bytes BE; value exactly `[1]` or `[2]++8`; anything else Corrupt — LOUD at mount replay (entry checksum already verified).
- **Ledger slot** (for the bootstrap record, `checkpoint.rs:459-568`): `magic "KVRL" u32 ‖ payload_len u32 ‖ seq u64 ‖ xxh3 u64` (24 B; checksum over `img[..16] ‖ [0;8] ‖ payload`), payload `journal_tail_seq u64 ‖ next_ino u64 ‖ alloc_bitmap_generation u64 ‖ node_seq_watermark u64 ‖ n_roots u16 ‖ roots[(tree_id u8 ‖ addr u64 ‖ seq u64)] ‖ stamp? ‖ partition_suffix?(writer u16 ‖ writers u16)`; stamp = `set_uuid[16] ‖ set_epoch u64 ‖ member_position u16 ‖ member_count u16 ‖ routing_width u32 ‖ n_runs u16 ‖ runs[(start u16 ‖ stride u16 ‖ count u32)] ‖ native_slot_plus1 u32 ‖ n_cursors u16 ‖ cursors[(slot u16 ‖ next u64)]`; slot index `seq % 32`; invalid slot = absent; newest seq wins.
- **Appender page / dir header** (forest format output; `appender.rs:77-103`, `:788-793`): page magic `"KVAP"` 0x4B564150, layout byte 55 = 1, checksum at [16..24) over whole 4096 with field zeroed; a `Free` page encodes identity zeros, state `Free`, `n_segments = 1` with segment `{journal.start+16384, journal.len−16384}`, all counts 0 otherwise. Dir header magic `"KVAD"` 0x4B564144: `magic u32 ‖ chain_index u32 ‖ xxh3 u64 ‖ next.start u64 ‖ next.len u64 ‖ pairs u16`, zero-padded to 4096.

---------------------------------------------------------------------------------------------------
## 9. FORMAT-TIME APPENDER-0 PAGE, byte-exact (forest class only; `appender.rs:77-103`, `:316-407`)

Written ONCE by `build` at `journal.start + 4096` (slot B = `page_slot_for(generation 1) = 1 % 4`).
The other three slots (`journal.start + 0`, `+8192`, `+12288`) are left ZERO (classify as `Blank`;
newest-valid-wins over four slots picks this page). The ring proper for appender 0 begins at
`journal.start + 16384`.
```
[0..4)     magic u32          0x4B564150 "KVAP"
[4..8)     appender_id u32    0
[8..16)    generation u64     1
[16..24)   xxh3 u64           over img[..16] ‖ [0;8] ‖ img[24..4096]
[24..32)   node_token u64     0            (AppenderIdentity::default)
[32..48)   writer_id u128     0
[48..52)   mount_slot u32     0
[52]       state u8           0 = Free     (1 Live, 2 Recovering, 3 Recovered; other ⇒ "appender page carries unknown state {n}")
[53]       is_manager u8      0
[54]       home_volume u8     0
[55]       layout_version u8  1            (APPENDER_PAGE_LAYOUT_VERSION; a verified page with another value is ForeignLayout ⇒ REFUSE, never fallback)
[56..64)   term u64           0
[64..72)   recovered_by_term  0
[72..74)   n_segments u16     1
[74..80)   reserved           0
[80..96)   segments[0]        start = journal.start + 16384 (u64) ‖ len = journal.len − 16384 (u64)
[96..208)  segments[1..8]     0
[208..216) head_hint u64      0
[216..224) ledger_tail_seq    0
[224..232) ckpt_seq u64       0
[232..234) n_runs u16         0
[234..240) reserved           0
[240..304) runs[4]            0  (each: start u64 ‖ len u32 ‖ reserved u32)
[304..306) n_slots u16        0
[306..314) seq_offset u64     0
[314..4096) slot entries      0  (each 35 B: slot u16 ‖ state u8 ‖ g u32 ‖ slot_tree_extents u32 ‖ root.addr u64 ‖ root.seq u64 ‖ cursor u64; SLOT_PAGE_BUDGET = (4096−314)/35 = 108)
```
Directory extent (one heap extent, claimed from the bitmap, zeroed): pairs at `dir.start + pair×8192`
(all zero at format), header page at `dir.end − 4096`:
```
[0..4) 0x4B564144 "KVAD" ‖ [4..8) chain_index u32 = 0 ‖ [8..16) xxh3 (img[..8] ‖ [0;8] ‖ img[16..]) ‖ [16..24) next.start = 0 ‖ [24..32) next.len = 0 ‖ [32..34) pairs u16 = (node_size/4096 − 1)/2 ‖ zero to 4096
```

---------------------------------------------------------------------------------------------------
## 10. FORMAT CLASS MATRIX AND THE WRITABLE-DOOR GATES (`builder.rs:1406-1423`, `backend.rs:2379-2404`, `:285-310`)

| class | CLI | incompat word | trees in the ledger | appender region | writable mount |
|---|---|---|---|---|---|
| `Symmetric` (DEFAULT) | `format` | `0x2FFD7` (0,1,2,4,6,7,8..15,17) | `[(8, ctrl), (0, native)]` | dir extent + page 0 | forest (`SQUEEZEFS_SYMMETRIC_META` unset or `=1`) |
| `SingleWriter` | `format --single-writer` | `0xD7` (0,1,2,4,6,7) | `[(1),(2),(3)]` | none | flat solo |
| `MultiWriterFlat` (pre-flip, tests only) | — | `0x1FFD7` (no 17) | `[(1),(2),(3),(6)]` | none | REFUSED: `"{path}: not symmetric-forest capable (incompat bit 17 absent) — a multi-writer-class volume formatted before the symmetric default flip (PR 14). Run `squeezefs volume enable-symmetric <sqmeta-uri>` offline (design-symmetric-metadata §7.2), or `format --force` a fresh set; a writable mount of this class is presence-required …"` (half-converted variant names the `sym_upgrade:` marker and `--resume`/`--abort`) |

Forest volume with `SQUEEZEFS_SYMMETRIC_META=0` at a writer door ⇒ `"{path}: SQUEEZEFS_SYMMETRIC_META=0 on a symmetric-forest volume — the knob names no posture a writer could run under since the default flip (PR 14, design-symmetric-metadata §7.2: the unarmed-forest posture is retired; a writer without the slot-lease plane stamps frames no lease backs, §5.8.2). Mount with the default (the knob unset or `=1`); the read-only posture is `-o ro`, never a knob value. Nothing was written"`.
Read-only mounts, probes and fsck read every class. The ledger `membership_stamp` is present in ALL classes.
Forest-key transform the builder applies to every format-time record (`record.rs:188-218`; kinds 1/2/3/6/7 only):
ino-major kinds (1,2,3,7): `legacy[0..8] ‖ kind ‖ legacy[8..]` (inode 9 B, dentry 17 B, xattr 17 B, block-map 13 B);
refs (6): `0x06 ‖ legacy` (29 B). The routing ino (`legacy[0..8]`, or the owner at legacy offset 16 for refs) must
satisfy `ino >> 40 ≤ 65536`. The native slot tree's node headers carry `tree_id = 0`; tree 0's carry `8`.
Forest leaves use the v2 bset frame (`BSET_FRAME_VERSION_V2 = 2`, 40 B: v1's 32 B plus `appender_id u32 ‖ g u32` before the checksum) stamped `(0, 0)` — the manager's structural stamp (`node.rs:152-167`, `:310-314`).

---------------------------------------------------------------------------------------------------
## 11. MOUNT-SIDE WIRING OF THE ALLOCATOR (what a Zig mount must reproduce; `backend.rs:2330-2930`)

1. `classify_volume_slot(path)` → `V3(sb)` (+ `repair_primary_superblock` on a write mount if `Backup`).
2. `read_newest_ledger(path, sb.root_ledger.start)` → newest valid record (none on a non-fresh volume ⇒ loud).
3. Journal: `JournalRing::recover(path, ring_extent, pages, checkpoint_reserve_bytes(len), ledger.journal_tail_seq)` → the replay window `recovery.entries` (seq-sorted).
4. `ExtentAllocator::load(path, sb.alloc_bitmap.start, sb.total_extents(), compaction_reserve_extents(total), PENDING_FREE_CAP (or TEST_PENDING_FREE_CAP), ledger.journal_tail_seq, &recovery.entries)` (`:2461-2476`).
5. Node cache + trees from `ledger.tree_roots`; node-seq mint floor ≥ `ledger.node_seq_watermark`.
6. `alloc.park_replayed_frees(&trees.root_extents(&cache))` — once, before any post-mount SMO (`:2900-2908`).
7. `checkpoint_seq = max(ledger.seq, alloc.resume_generation())` (`:3051`).
8. Every later checkpoint cycle: `write_dirty_pages(.., ckpt_seq)` → barrier → ledger record `{seq: ckpt_seq, alloc_bitmap_generation: ckpt_seq, journal_tail_seq: tail, …}` → (the record becomes durable at a later barrier) → `after_durable_barrier` drains `pending_reclaim` entries pushed before that barrier's epoch and calls `alloc.advance_durable(tail)` + `ring.advance_reusable_upto(tail)` + `cache.set_durable_tail(tail)` — ONE clock for all three §4.6-pt-3 watermarks.
9. Shutdown fixpoint: a final cycle whose barrier released pending frees re-dirties their pages (`has_dirty_pages()`), so one more page write runs — otherwise released images read CLAIMED at the next mount (PR 11 finding, `alloc_ext.rs:837-846`).
Replay-window tail rule (K6b, out of scope here): alloc/free records stay inside `[tail, head]` until their effects are durable in bitmap pages — which is why the bitmap is "a checkpoint accelerator, not the sole truth".

---------------------------------------------------------------------------------------------------
## 12. ZIG `format` CHECKLIST — produce exactly this or the Rust binary refuses/misreads

1. `len = file size or block-device capacity`; grow a regular file to the requested length.
2. Plan per §2.1 (compute `journal_ring_len` with the clamp; `heap_start` aligned to `node_size`; `usable = len − 4096`; refuse below `reserve + 4` extents with the exact message).
3. Zero `[0, heap.start)` in 1 MiB O_DIRECT writes (also with `--full`: the whole device first).
4. Allocate extents lowest-first from a fresh all-free bitmap mirror (`claim_internal`), one per node, in tree order `[(8, []), (0, native)]` (forest) or `[(1),(2),(3)]` (flat); node seqs `base+1, base+2, …` with `base = uuid[0..8] LE & 0x7FFF_FFFF_FFFF_FFFF`; leaves ¾-filled greedily; interior separators `(max_key → addr‖seq LE)`; last node of every level `max_key = [0xFF;32]`, first `min_key = ""`, `min_{i+1} = max_i ‖ 0x00`.
5. Forest: claim one more extent for the appender directory (it is the NEXT lowest free extent after the nodes), zero it, write the `KVAD` header at its last page, write the appender-0 `Free` page (§9) at `journal.start + 4096`; set `sb.appender_dir`.
6. Write every bitmap page (all dirty) at generation 1 into slot A (`base + i×8192`), bits = set for every claimed extent (nodes + dir), encoded per §4.1.
7. Write the ledger record `{seq 1, roots, tail 0, next_ino 2 (+ described inos), alloc_bitmap_generation 1, node_seq_watermark = last minted seq, stamp (§2.2 step 5), no partition suffix}` at `root_ledger.start + 4096` (slot 1).
8. `fdatasync`.
9. Superblock: feature word per class (§1.2/§10); `generation = max(valid primary gen, valid tail gen) + 1`; write the tail copy at `len − (len % 4096) − 4096`, `fdatasync`, then sector 0, `fdatasync`.
10. `fdatasync` again (the Rust builder does; harmless).
Tie tests to port: `journal_ring_clamp_math`, `test_compaction_reserve_policy`, `node_kib_knob_floor_warning_and_allowed_set`, `v3_plan_uses_clamp_default_and_honors_override`, `v3_superblock_checksum_covers_the_whole_sector`, `v3_unknown_incompat_bit_refuses_naming_it_and_unknown_ro_does_not`, `builder_output_is_deterministic_and_digest_stable`, `v3_mount_surfaces_ledger_and_allocator_state`, and the 14 `kv_alloc_tests` plus the 7 loom models (port as exhaustive-interleaving or stress tests with the same assertions).
Cross-check against the reference: build the same description with the Rust binary (`BuilderConfig{node_size, journal_len_override, hash_seed: TEST_SEED, uuid: TEST_UUID}` + `set_membership_stamp(fixed stamp)`) and the Zig port; the two images must be byte-identical except where randomness enters (uuid, set_uuid-derived hash_seed) — pin them and diff.
