# PORTING SPEC — Data layouts: Inline / Staged / Packed / Striped, the staging ring, the packer, the router

> **Checksum amendment (2026-10-11, decision 0003 §M; 0002 row 480).** Every `xxh3` / `xxh3-64` /
> `xxh3_64` below names the hash the reference used and this port used until format v5. The port's
> ONE hash is now **sqzhash128** (`src/core/sqzhash.zig`, the pinned definition): every 8-byte checksum
> and identity field is its LOW 64 bits under the fixed public format key (`checksum.hash64`,
> `hash64FieldZeroed`, `stamp`/`verify`, the streaming `checksum.Hasher`); the seeded name hashes
> (`hash54`/`hash56`) are `hash64Keyed` under the per-mount name key (`key.NameKey`: the volume's
> `hash_seed` zero-extended, expanded once at mount). Field names on the wire read `csum`. Byte
> layouts, offsets and widths below are unchanged; only the function behind the 8 bytes is.

Reference (read-only): `/Users/joberdorf/Source/squeezefs` — `R=src`. Port: `/Users/joberdorf/Source/squeezefs-zig`
(do not modify from this spec). Charter laws pinned: §6.1 (layouts, promotion, the packing law), §5.6 (block-map
tree, layout deltas, publish coalescing), §5.4 (routing), §1.5 (every constant derived), §1.7 (forward-only), §12
(extern structs, canonical total codecs). Decision 0002 rows 2–4 govern the wire: the layout record becomes an
`extern struct` with a canonical total codec (row 2), ONE `Delta` kind (row 3), the fixed-width inode (row 4, landed
as `InodeValueWire`, row 36). **Every bincode/serde/string artifact below is flagged `⚠ ARTIFACT` and gets a v4 row
in §11.**

Conventions: `LE`/`BE` as marked; `[a..b)` byte ranges; `CHUNK` = `block_allocator::CHUNK_SIZE` = 4 MiB
(`R/block_allocator.rs:112`); `GRAIN` = `routing::LBA_GRAIN` = 4096 (`R/routing.rs:26`); `ceil(len)` =
`len.div_ceil(GRAIN) × GRAIN` (`pack_slot_len`, `R/routing.rs:60-62`). "3.5" = the per-ino block-map meta stripe
(`INODE_META_LOCKS`, 4096-way, `R/routing.rs:441-443`). "RES-1" = never free a block under a 3.5 guard.

---

## 0. Map of the reference

| Unit | Where | What |
|---|---|---|
| The persisted `layout` xattr value and its delta record | `R/layout_wire.rs` (832 lines) | `LayoutMetadata` (bincode), `LayoutDelta` (hand wire, magic `0x4C31`), the version gate |
| Layout enum/transitions, inline bound, promotion primitive, packer glue, mapping grammar, placement table, incarnation keys | `R/routing.rs` (23,961 lines) | `write_file` :18559, `promote_staged_file` :15389, `parse_block_mapping` :8334, `PlacementTable` :1428, `persist_block_key` :3808 |
| The open pack block | `R/pack.rs` (670 lines) | `OpenPack`, `PackTenant`, `Packer` |
| Staging ring (RAM/budget/ledger/generation/format fences/merge worker) | `R/cache/nvme.rs` (2,552) | `NvmeStaging`, `stage_write` :1380, `start_merge_worker` :2156 |
| Segment mmap ring (on-disk record geometry) | `R/tiering/nvme.rs` (1,959) | `NvmeShard`, `recover_index` :1142 |
| Block-map tree values + kvmap head sentinel | `R/meta_backend/kv/block_map.rs` | POINT/STRING/RUN/POINT2/RUN2, `KvmapHead` |
| Mint spread, `pick_mint_volume`, `pick_mint_slot`, generation string | `R/meta_backend/mod.rs` | :50-61, :2585-2632, :3266-3297, :6691-6713 |
| Design evidence | `R/docs/design-small-file-packing.md` (584), `design-mw-layout-versions.md` (306), `design-durable-block-refcounts.md` §3-§6 | normative measured numbers |

---

## 1. THE LAYOUT RECORD AND EVERY WIRE FORM (byte-exact)

### 1.1 The RAM shape — `CachedMetadata` (`R/routing.rs:727-820`) and its persisted twin `LayoutMetadata` (`R/layout_wire.rs:57-69`)

| Field | RAM type | Persisted | Meaning |
|---|---|---|---|
| `file_type` | `CompactString` | `String` | `"inline"` / `"staged"` / `"striped"` — THE layout discriminator; default `"inline"` (`:890`). ⚠ ARTIFACT: a string-typed enum |
| `size` | u64 | u64 | logical size (truncate-up holes are implicit zero tails, `:19104-19108`) |
| `block_map_id` | `Option<Arc<str>>` | `Option<String>` | `None`, `"indirect:<bk>"` (legacy spill blob, `:2284-2434`), or the kvmap head `kvmap:1[;sweep:K][;gen:N]` (§1.5) |
| `block_prefix` | `Option<Arc<str>>` | `Option<String>` | legacy `{prefix}/part_{n}` keying (`:18534-18536`); nothing mints it any more |
| `file_id` | `Option<Arc<str>>` | `Option<String>` | the staged ring key — a `Uuid::new_v4().to_string()` (36 chars, `:19179-19182`) ⚠ ARTIFACT (36-byte text for 16 bytes) |
| `data_key` | `Option<Bytes>` | `Option<Vec<u8>>` | the INLINE payload — the payload IS the record (charter §6.1) |
| `block_map` | `Option<Arc<HashMap<u32,String>>>` | `Option<HashMap<u32,String>>` | block index → mapping string (§1.3) |
| `cached_at`, `layout_dirty`, `layout_delta_chain`, `layout_base_token`, `layout_version`, `block_map_id_own_mint`, `reader_step_gen` | RAM only | — | freshness horizon (1 s, `:881-885`); dirty = this mount's acked unsaved authority; the delta-eligibility chain depth (`LAYOUT_DELTA_CHAIN_INELIGIBLE = u32::MAX`, `:824`); fencing-era token; §6.2-item-9 link version; blob mint ownership; reader purge generation |

### 1.2 Persisted `layout` value = bincode 1.3 default of `LayoutMetadata` ⚠ ARTIFACT (`R/layout_wire.rs:57-69, 667-670`; `Cargo.toml:53`)

bincode 1.3 `serialize` = fixint, LE, `String`/`Vec` = `u64 len ‖ bytes`, `Option` = `u8 tag {0,1} ‖ T`, `HashMap` =
`u64 count ‖ entries`. The ONLY canonicalisation is the sorted-map serializer (`serialize_block_map_sorted`,
`:74-94`; the borrowing twin `:124-145`): entries ascend by block index. **Decoding accepts ANY map order and
trailing bytes** (`bincode::deserialize` allows trailing bytes — the 0002 row-2 conviction). Byte layout:

```
[0..8)   file_type_len u64 LE ‖ file_type bytes ("inline" | "staged" | "striped")
         size u64 LE
         block_map_id: tag u8 ‖ [len u64 LE ‖ bytes]
         block_prefix: tag u8 ‖ [len u64 LE ‖ bytes]
         file_id:      tag u8 ‖ [len u64 LE ‖ bytes]        (36-char uuid text)
         data_key:     tag u8 ‖ [len u64 LE ‖ bytes]        (the inline payload)
         block_map:    tag u8 ‖ [count u64 LE ‖ count × { block u32 LE ‖ key_len u64 LE ‖ key bytes }]
```
A legacy JSON value is sniffed by first byte `'{'` (`fetch_metadata_from_backend`, `:8396-8400`; `decode_layout_any`,
`:623-627`). `LayoutMetadataRef::encoded_len` (`:165-168`) is the "needs indirect/kvmap?" sizing probe — bincode
computes the size without encoding.

**v4 (THIS port writes — §1.7): a fixed `extern struct` header + per-kind body; no bincode, no strings, no JSON sniff.**

### 1.3 The block mapping string — the tenant grammar ⚠ ARTIFACT (string-typed binding)

Full grammar (every consumer resolves through TWO choke points — `clean_block_key_ref` for the base,
`parse_block_mapping` for the window):

```
mapping   := [ "damaged:" ] base [ ":" rel_off ":" packed_len ]          (R/routing.rs:2032-2090, 8334-8382)
base      := [ be_id "://" ] offset [ "@" incarnation ]                   (R/routing.rs:3731-3755, 3808-3839)
be_id     := "vol-" 16 lowercase hex                                       (R/lib.rs:1048 — fastrand u64; never reused)
offset    := decimal u64 device byte offset (CHUNK-aligned)
incarnation := canonical lowercase base-36 of (era << 24 | lane_seq), no leading zero, ≤ 13 chars (R/routing.rs:2154-2197)
rel_off   := decimal u64, rel_off % GRAIN == 0
packed_len:= decimal usize, EXACT stored-image length; rel_off + ceil(packed_len) ≤ CHUNK
```
* Default-slot alias: a bare `offset` with no `://` resolves to `backend_0` (`split_key`, `:3737-3740`);
  `persist_block_key_body` emits the bare form iff the backend IS the default slot's Arcs (`:3827-3839`) — the
  "first volume stays byte-identical" rule. ⚠ ARTIFACT (an alias that depends on registration order).
* `clean_block_key_ref` (`:2054-2063`): strip `damaged:`, then cut at the first `:` after `://` (or at the first `:`
  when no scheme). The `@` separator was chosen precisely so the incarnation SURVIVES cleaning and the decoration
  still parses after it (`:2098-2108`).
* `split_mapping_decoration` (`:2076-2090`): the 3-part split; 2- or 4-part strings answer `None` (treated as the
  bare form by the caller, which then fails the offset parse).
* `parse_block_mapping` (`:8334-8382`) — **THE ONE parse choke point** (charter §6.1): `damaged:` ⇒ `EIO`
  (`:8340-8344`); decorated ⇒ `rel_off` undecodable / `packed_len` undecodable / `rel_off % GRAIN != 0` /
  `rel_off + ceil(len) > CHUNK` ⇒ `EIO` + `packed_mapping_refusals` + ERROR log (`:8348-8376`); returns
  `(offset, rel_off, len, exact=true)`; bare ⇒ `(offset, 0, block_size, exact=false)` (`:8378-8380`). Before PK1
  a non-numeric `rel_off` was `unwrap_or(0)` (design §1.2) — tenant 0's bytes served as this tenant's.
* `is_whole_block_mapping` (`:7760`, `!rest.contains(':')`) is the W1/overlay/in-place screen: a decorated mapping
  is refused by every in-place arm (design §1.2 table).
* Incarnation composition (`:2117-2137`): `INCARNATION_SEQ_BITS = dlm::GRANT_SEQ_BITS`; `compose(era, seq)` =
  `None` iff `era == 0 ∨ era > TERM_MAX ∨ seq == 0 ∨ seq > SEQ_MAX` (callers degrade to `INCARNATION_NONE = 0`,
  loudly — detection lost, never a wrong lifetime). `persist_block_key` (`:3808-3823`) reads the LIVE stamp of the
  offset (`live_incarnation_for`, `:3782-3790`), never mints one; gate = one relaxed load of `incarnation_era`.

### 1.4 The layout DELTA record — hand wire with magic `0x4C31` (`R/layout_wire.rs:185-213, 383-393, 459-586`)

Already byte-specified in `docs/porting/kv_record_bset.md` §2.7; restated for completeness:
```
[0..2)  magic u16 LE = 0x4C31 ("L1")  — outside InodeDelta's mask space so the fold branches on the payload (:177-185)
[2]     flags: bit0 BLOCK_MAP_ID, bit1 BLOCK_PREFIX, bit2 FILE_ID, bit3 DATA_KEY, bit4 VERSIONED; others ⇒ Malformed (:383-393, 526-530)
[3..19) iff bit4: base_version u64 LE ‖ version u64 LE  (version 0 under the flag ⇒ Malformed, :534-544)
        file_type_len u8 ‖ file_type ‖ size u64 LE
        [block_map_id len u16 ‖ bytes] [block_prefix …] [file_id …] [data_key len u16 ‖ bytes]
        entry_count u32 LE ‖ entries × { block u32 LE ‖ key_len u16 LE ‖ key }     (decoder presizes min(n, 4096), :566)
trailing bytes ⇒ Malformed (:573)
```
Semantics: "full minus map" — every non-map field ABSOLUTE, `entries` are map INSERTS only (removals take the full
Put, `:360-362`); `apply_to` (`:599-612`) inserts then overwrites. `decode_base_layout` refuses a JSON base, a
non-bincode base, an `indirect:` base and a `kvmap:` base (`:636-663`). Version pair law (design-mw-layout-versions
§2 `:33-50`): `version = (term << 40) | seq` from the S1 `GRANT_SEQ` mint; `base_version` = previous link's version
or 0. Commit gate `layout_version_gate` (`:278-324`; table design §3 `:74-79`): `Stage` / `Rebase` (full Put) /
`Diverged{claimed, head}` (REFUSED loud). Fold law (design §4 `:114-135`): per base segment all-versioned or
all-unversioned, first versioned link claims 0, every later `base_version == previous.version`; violations are
`KvError::Corrupt`.
⚠ ARTIFACT (0002 row 3): a second delta CLASS discriminated by a payload magic, with its own fold arm.

### 1.5 The kvmap head sentinel and the block-map tree values (`R/meta_backend/kv/block_map.rs`)

Head (`:472-498, 548-595`): `block_map_id = "kvmap:1[;sweep:K][;gen:N]"`, `block_map = None`. Grammar EXACT:
segments in encoder order only, canonical decimal (no `007`, no `+7`), `gen:0` refused (the encoder omits an absent
generation), unknown major refuses (always-forward). Sticky: an existing head is force-kvmap regardless of the
`SQUEEZEFS_KVMAP` lever (`R/routing.rs:1071-1078`). ⚠ ARTIFACT (a mini-language inside a string field).

Tree-7 key `owner_ino u64 BE ‖ block_index u32 BE` (12 B; `u32::MAX` reserved for the refs MAP_BLOB sentinel,
`:138-152`). Values (LE, `version u8 = 1 ‖ kind u8 ‖ body`, `:28-36, 73-129`):

| kind | byte | body | len |
|---|---|---|---|
| POINT | 1 | `vol_tag u64 ‖ offset u64` | 18 |
| STRING | 2 | decorated key bytes VERBATIM (packed tenants, `damaged:`) | 2+n |
| RUN | 3 | `vol_tag ‖ start_offset ‖ len u32` (2 ≤ len ≤ `RUN_LEN_MAX = 4096`) | 22 |
| POINT2 | 4 | POINT ‖ `incarnation u64` (0 refused — "two spellings") | 26 |
| RUN2 | 5 | RUN ‖ `start_incarnation u64` (stamps literally consecutive) | 30 |

Port note: v4 of this port already carries the kind byte on the key (`key.zig`, kind 7). The decorated/packed tenant
rides STRING today ⚠ ARTIFACT — v4 makes it a binary `MappingWire` (§1.7).

### 1.6 The indirect block-map blob (legacy spill; read-only in spirit) (`R/routing.rs:2284-2434`)
`magic "SQFSIMAP" ‖ version u32 LE (1 | 2) ‖ [v2: payload_len u32 ‖ xxh3 u64 over header(with digest zeroed)+payload]
‖ bincode Vec<(u32, String)>` sorted by index. v1 has no digest (the DUR-6 silent-corruption path). The kvmap tree
superseded it (`SQUEEZEFS_KVMAP` default on). **v4: not carried** — PB-class maps are tree-7 only (§11 Q-6).

### 1.7 v4 — the layout record as an `extern struct` with a canonical TOTAL codec (what the port writes)

Design rule: the xattr `layout` VALUE is `LayoutHeaderWire ‖ body`, `value.len == 32 + header.body_len` exactly
(any other length refuses); every reserved byte is 0 and checked; a `Delta` record on the same key is
`field_mask u16 ‖ masked header fields ‖ [inserts]` — the SAME `Delta(field_mask, fields)` shape as the Δinode
record, discriminated by the KEY (xattr kind 03, name `layout`), never by a payload magic (0002 row 3).

```zig
/// 32 B, little-endian, size/offset-asserted at comptime.
pub const LayoutHeaderWire = extern struct {
    layout_kind: u8,        // @0  0 inline | 1 staged | 2 packed | 3 striped   (the file_type string retired)
    version: u8,            // @1  = 1; any other value refuses (forward-only)
    flags: u16,             // @2  bit0 HAS_FILE_ID, bit1 HAS_DURABLE_COPY (staged: a promoted/spilled MappingWire follows),
                            //     bit2 MAP_INLINE, bit3 MAP_KVMAP, bit4 SWEEP_CURSOR, bit5 MAP_GEN; others = 0 (checked)
    body_len: u32,          // @4  bytes after the header; total = 32 + body_len, exactly
    size: u64,              // @8  logical size
    layout_version: u64,    // @16 §6.2-item-9 link version of THIS record (0 = unversioned); a Put IS a base
    base_version: u64,      // @24 the claim (Delta only; a Put carries 0)
};
/// 40 B — ONE binary binding (replaces `[be://]offset[@inc]:rel_off:len` and `damaged:`).
pub const MappingWire = extern struct {
    vol_tag: u64,           // @0  the `vol-{16 hex}` decoded verbatim (block_refs.rs:193-194) — never an ordinal/alias
    offset: u64,            // @8  device byte offset, CHUNK-aligned (checked)
    incarnation: u64,       // @16 0 = none
    rel_off: u32,           // @24 % GRAIN == 0 (checked)
    len: u32,               // @28 exact stored-image length; rel_off + ceil(len) ≤ CHUNK (checked); flag WHOLE_BLOCK ⇒ len ignored
    flags: u8,              // @32 bit0 WHOLE_BLOCK (the undecorated binding), bit1 DAMAGED (fsck quarantine marker)
    reserved: [7]u8,        // @33 = 0
};
pub const MapEntryWire = extern struct { block: u32, reserved: u32 = 0, mapping: MappingWire }; // 48 B
```
Bodies (`body_len` fixed by kind + flags, so decode is total and `exactly the encoder's form decodes`):
* `inline`: `data_len u32 ‖ data[data_len]`; `data_len ≤ inline ceiling` (§6) — refuse above (a value the format
  cannot hold).
* `staged`: `file_id: [16]u8` (the uuid's 16 bytes, not 36 chars) ‖ `[MappingWire iff HAS_DURABLE_COPY]`.
* `packed`: `file_id: [16]u8` ‖ `MappingWire` (the tenant; `WHOLE_BLOCK` clear, `len ≥ 1`). A distinct KIND — the
  reference overloads `staged` + `block_map[0]`.
* `striped`: `MAP_INLINE` ⇒ `entry_count u32 ‖ MapEntryWire[entry_count]` ascending by `block` (checked — the
  canonical order is a decode law, not a serializer courtesy); `MAP_KVMAP` ⇒ `[sweep_cursor u32 iff SWEEP_CURSOR] ‖
  [map_gen u64 iff MAP_GEN]` (the head sentinel as fields). Exactly one of the two map bits set.
* `Delta` on the layout key: `field_mask u16` over the header's fields in declaration order (`layout_kind` 0,
  `flags` 1, `size` 2, `layout_version` 3, `base_version` 4, `file_id` 5, `durable_copy` 6, `sweep_cursor` 7,
  `map_gen` 8, `MAP_INSERTS` 9) ‖ the masked fields ‖ `[entry_count u32 ‖ MapEntryWire[] iff MAP_INSERTS]`. Fold =
  overwrite masked fields, insert entries; the field table is DERIVED from the struct at comptime like
  `inode_field_table` (`record.zig:130-140`). The version pair rides the record's own header, so the "peek at fixed
  offsets 3..19" primitive (`:201-243`) becomes "read the header".
Size ties: inline record with a 4 KiB payload = 32 + 4 + 4096 = 4,132 B (vs ≈ 4,150 bincode + the string type);
packed = 32 + 16 + 40 = 88 B (vs ≈ 150–180 B of strings today); striped entry 48 B vs ≈ 35–60 B of text — the
delta/kvmap laws keep the per-publish term O(batch) regardless (charter §5.6).

---

## 2. THE FOUR LAYOUTS — semantics, transitions, durability order

### 2.1 Dispatch — `DataRouter::write_file` (`R/routing.rs:18559-19280`)
1. Fencing check (`:18570-18580`); fetch meta; a `"striped"` layout is HANDED BACK (`WriteFileOutcome::LayoutStriped`,
   `:1312-1320`): every striped mutation runs in the block-guarded striped path, never here (the 2026-09-10
   `overlay_foreign_merge` data loss, `:1299-1310`).
2. `inline_max = inline_max_bytes(ino)` (§6). A staged file, or an inline file whose write ends past the ceiling,
   takes the block-0 guard `BLOCK_FLUSH_LOCKS(ino, 0)` (`:18593-18608`) and RE-RESOLVES the identity under it
   (`:18617-18630`; a flip to striped while waiting ⇒ `LayoutStriped`).
3. W2 staged rider (`:18639-18712`): a sub-image overwrite ≤ 25 % of the image, non-extending, under the fold
   thresholds rides an `active_block_ext:` extent record instead of the whole-image RMW; it bumps the stage
   generation FIRST (`:18688`) so a racing promotion's generation check aborts.
4. `stripe_threshold = inline_max` with NO staging dirs, else `block_size` (`:18714-18718`) — the reference's
   staging paths were a FORMAT decision (`mount` rejected `--disk-cache-paths`, `R/main.rs:2822-2829`; `config
   set-cache-paths` changed them). **Zig (2026-10-21, 0002 row 551): the staging ring is the WRITE CACHE in RAM on
   every mount — no path, no format flag; `SQUEEZEFS_WRITE_CACHE_PCT=0` is the cache-less mount** (inline and
   striped alone).
5. RMW seed: inline from the RAM LRU/`data_key` (`:18754-18764`); staged from the ring (`read_staged_into`,
   `:18767-18777`) or, on a ring miss, the promoted mapping through the BINDING-VALIDATED loop (`:18806-18890`:
   `read_mapping_window` → re-resolve `freshest_layout_identity` → `still_bound` ⇔ type staged ∧ same file_id ∧
   `block_map[0] == mapping`; movement ⇒ `staged_identity_retries`, ≤ 64 attempts then loud; a bare legacy mapping is
   clipped to `meta.size`, `:18830-18840`). Rider fold-first (`:18897-18924`).
6. `new_size = max(existing_len, end, meta.size)` (`:18941-18942`). Then:
   * `new_size > stripe_threshold` ⇒ **staged/inline → striped** (`:18944-19101`, §2.5).
   * `new_size ≤ inline_max` ⇒ **inline** (`:19134-19171`): `file_type = "inline"`, `data_key = payload`,
     `file_id = None`, `block_map = None`, `layout_dirty = true` (RAM-only until fsync/release); both RAM LRUs
     refreshed; a superseded ring entry / durable copy released with its `−ref` NOTED for the dirty save (`FIND-PK-5`,
     `:19160-19165`).
   * else, staging dirs present ∧ `new_size ≤ block_size` ⇒ **staged** (`:19172-19280`): `file_id` kept or minted
     (`Uuid::new_v4`), `stage_write` into the ring, then under 3.5: ring-residency check (leg 5 of the zeros-LOSS
     family, `:19201-19215`); resident ⇒ `block_map = None, layout_dirty = true`; NOT resident (a promotion consumed
     exactly these bytes) ⇒ ADOPT its published mapping, dirty unconditionally (`:19224-19233`).

### 2.2 Inline (charter §6.1 ¶1)
Payload = `data_key`; visible to every client at the layout commit (the record IS the data). Cost law: an inline
byte rides the metadata plane twice (journal entry + CoW node append) — the one-page derivation (§6). Transitions:
inline → staged/striped by growth past the ceiling (§2.1 step 6); staged → inline by promotion when
`max(size, image) ≤ inline_max` (`promote_staged_file_inline`, `:16127-16198`: ONE commit flips the type, carries
the bytes, `block_map = None`, the displaced durable copy's `−ref` in the same tx `:16168-16171`, LRUs purged, the
ring entry released INSIDE the guarded section generation-gated `:16185-16187`, displaced keys freed after the guard
(RES-1)). Counters `layout_inline_writes`, `layout_promoted_inline`.

### 2.3 Staged (charter §6.1 ¶2)
The whole payload is one ring entry keyed by `file_id` (§3); the layout is `{file_type "staged", file_id, size,
block_map None, layout_dirty}` until a promotion/spill publishes `block_map[0]` (the durable copy — own block
`bk:0:len`, packed `bk:off:len`, or a transformed/truncate-clip image). Readers elsewhere see **size-consistent
zeros** until promotion (§3.7). A staged file whose `file_id` has a resident ring entry is the mover's deferral class
(`staged_tenant_ring_resident`, `:15930-15942`). Durable-copy supersession: a fresh stage releases the older
durable copy ONLY when the ring entry survives to be authoritative (`release_superseded_staged`, `:16293`;
`:19249-19260`) — partial free of a packed tenant (`pack_partial_frees`).

### 2.4 Packed (charter §6.1 ¶3) — in the reference `file_type == "staged"` with `block_map[0] = base:off:len`
Full treatment in §4. Transitions: staged → packed by promotion (`PromotedInto::Packed`, `:234-238`); packed → staged
by any overwrite/append (whole-image RMW re-stages under the same `file_id`, generation bump, partial free of the
tenant); packed → striped by growth (§2.5); packed → inline by a promotion under the ceiling; passthrough
truncate-shrink = mapping clip (`DurableClip::Mapping`, `:335-353`); transformed shrink = a new tenant
(`DurableClip::Image`). **v4 gives it its own `layout_kind = 2`** (§1.7).

### 2.5 Striped (charter §6.1 ¶4) and the layout-type FLIP LAW
`:18944-19101`: the transition is SPARSE (only data-bearing blocks assembled — the generic/285 OOM fix, `:18945-18955`);
`durable_write_sparse_blocks` DMAs every chunk and returns `(block_mappings, inflight_guards)` (`:19003-19004`);
`SQUEEZEFS_TEST_GROWTH_COMMIT_STALL_MS` parks HERE (`:19015-19019`, the record §4.4bt window); THEN under 3.5
(`:19021-19080`): `file_type = "striped"`, `size`, `block_map`, `file_id = None`; the displaced map is the CURRENT
RAM entry's (`swap_displaced_map`, `:19031`), `block_ref_ops_for_map_swap(old, new)` rides the SAME save
(`save_metadata_to_backend_refs`), the RAM entry published, the ring entry + displaced durable copy released; on a
failed save the refs are RETRACTED (`:19069`) and every fresh mint freed after the guard (`:19084-19090`). **Block I/O
strictly before the metadata type flip** (charter §6.1 last bullet; design §4.2 "P0 layout atomicity"). Placement =
health- and fill-weighted (§5.2). PB-class maps cross into tree 7 (kvmap head, `SQUEEZEFS_KVMAP`); inline maps spill at
the per-volume record cap minus `LAYOUT_INLINE_HEADROOM = 4096` (`:651-660`); layout deltas (`SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN`
64) and per-ino publish coalescing (`SQUEEZEFS_PUBLISH_COALESCE_MAX` 64) collapse the O(file) publish term (charter §5.6).

### 2.5a Read precedence by layout (charter §6.3; `R/routing.rs:19520` `read_file_range_zero_copy_with_meta`, the staged arm `:19638-19846`, `:7677-7704`)
Write-side overlays outrank durable state (the accumulation buffer's coverage-aware snapshot, an open device-overlay
record, extent records composed over the base, staged ring entries — "a dirty local authority outranks durable
state"). Then by layout: `inline` serves `data_key` (the RAM LRUs are refreshed by every inline write, `:19155-19156`);
`staged` walks §3.7's ladder (ring → durable copy through the binding-validated funnel → in-flight backoff → zeros);
`striped` resolves `block_key_in(meta, b)` (`:7653-7704`: the inline map, the partial-store overlay/warm window/tree
read-through for kvmap inos, or a HOLE) then the tier walk of charter §6.3. **Binding validation law** (every durable
read of a staged/packed mapping): bytes for mapping M serve only if, AFTER the read completed, the CURRENT identity
still binds `block_map[0] == M` with the same `file_type` and `file_id` (`still_bound`, `:18818-18825` write side;
the read side's twin at `:19763-19795`); movement ⇒ re-resolve (`staged_identity_retries`); a stale-incarnation refusal
on a still-bound mapping ⇒ `EIO` + tripwire (§4.6). The size clamp on a bare legacy mapping (`!exact`) is applied
on this leg only because every durable-mapping publish persists `size` in the same commit (`:18830-18840`).

### 2.6 One merge discipline — `BlockMapOp` (`R/routing.rs:912-945`)
`Merge(&[(idx, key)])` (degenerate `Merge(&[])` = size-only re-read-and-save under 3.5), `MergeExpected(&[(idx,
expected, new)])` (the mover's stale ⇒ no-op law), `TruncateFrom{new_size}`, `RemoveBlocks(&[idx])`. Every
prune bumps the per-ino `LAYOUT_PRUNE_EPOCHS` word (`:591-612`) so a delayed merge that captured content before a
truncate refuses (`merge_block_mappings_if_epoch`). The durable-ref delta is computed where the truth is
(`block_ref_ops` per changed index, `:9263-9290`; the map-swap sites diff whole maps by STRING equality,
`:9224-9251` — a clip `bk:off:len → bk:off:len'` emits a `−ref` + `+ref` pair for the SAME `BlockRef`, population
unchanged, design §5.7).

---

## 3. THE STAGING RING

### 3.1 Directory law and identity (`R/cache/nvme.rs:15-64, 84-111, 118-134, 314-447`; `R/config_ops.rs:130-161`)
* Every staging root and both segment trees are `0700` (`STAGING_DIR_MODE`, `create_private_dir_all`); every segment
  file is `0600` (`STAGING_FILE_MODE`, applied at create AND re-asserted by `fchmod` on open — `R/tiering/nvme.rs:406-416`).
  Reason: plaintext user payload on passthrough volumes, and every staged object key in the headers regardless.
* `.squeezefs_generation` marker = `"squeezefs-staging-generation-v1\n" ‖ generation ‖ "\n"` (dual rebind form has
  two generation lines, `:93-95`). The generation string = `"v3:" ‖ hex(uuid)` per metadata superblock uuid in
  DISCOVERY order, joined by `|` (`generation_from_uuids`, `R/meta_backend/mod.rs:6701-6713`), optionally suffixed
  by the writer scope `:w_{16 hex}[.m{8 hex}]` (`R/writer_scope.rs:114-115, 196-203`). ⚠ ARTIFACT (a text identity;
  v4: `set_uuid[16] ‖ set_epoch u64 ‖ node_token u64 ‖ mount_slot u32` in a checksummed binary marker — §11 Q-8).
* `bind_staging_generation` (`:314-447`): exact match ⇒ ok; dual marker listing ours ⇒ canonicalise; same set,
  unscoped marker while we are scoped ⇒ UPGRADE (adopt + restamp, `staging_scope_upgrades`); a PEER's scope with LIVE
  custody ⇒ REFUSE the mount loud (`staging_foreign_scope_refusals`); otherwise dead generation ⇒ WIPE both segment
  trees + gds files (`staging_generation_discards`) and restamp. Marker I/O = io_uring + `fdatasync` (never adopted
  volatile).
* **Port law (0003 §T.1, 0002 row 501):** the root is the MEMBER's — `<cache path>/member-<mount slot as 8
  hex>` holds its marker, `staging_segment/` and `cache_segment/`; the mount slot is the member scope of the
  identity the mount presents (the manager's 0, a joiner's mount-point derivation), stable across its
  incarnations. The owner's exclusive `flock` on the root dir is the LIVE claim, taken before the marker is
  read: a held claim refuses `StagingOwnerLive` (whatever the set), another member's marker refuses
  `StagingForeignOwner` (never adopted, never wiped), only the member's own successor adopts; the binary
  marker is v3 (80 B: + `appender_id u32 ‖ owner_pid u32`). Liveness is the claim, never the ring's content.
* `.squeezefs_staging_format` = `"squeezefs-staging-format-v1\n" ‖ version` (`:451-466`); `STAGING_FORMAT_VERSION = 3`
  read ceiling, write version 3 iff writer-scoped else 2 (`:50-64`); a FUTURE version refuses the root as a unit
  (`:493-508`) — the forward-only downgrade fence.

### 3.2 Segment ring on-disk/mmap geometry (`R/tiering/nvme.rs:11-12, 394-446, 463-497, 1142-1212`)
* Per staging dir: `staging_segment/` (recovered) and `cache_segment/` (WIPED at every start — offsets are not
  incarnation-stable across mounts, `R/cache/nvme.rs:1150-1184`). Shards per device = `shard_plan(per_device_capacity,
  default_shards = max(next_pow2(cores), 16), max_entry = block_size)` (`:1091-1101, 1188-1190`): halve from the
  power-of-two default while `capacity/shards < 2 × entry + SHARD_REPLACE_SLACK (64 KiB)`; < 10 MiB ⇒ 1 shard.
  Reason: a same-key crash-safe REPLACE holds two copies (FIND-VS-B). Shard pick = `xxh3_64(key) % shards`
  (`R/tiering/nvme.rs:1295`).
* A shard file is NEVER shrunk (`:418-432`, the FIND-VS-A SIGBUS); grown to capacity; `MmapMut::map_mut`.
* Record geometry (writer, `put_impl` `:531-594`; reader `get` `:463-497`; `recover_index` `:1142-1212`):
  ```
  @rec (aligned to 4096 when capacity ≥ 4096, else 1):
    magic u32 LE = 0xCAFEBABE ‖ key_len u32 LE ‖ val_len u32 LE        (HEADER_SIZE = 12)
    key[key_len]
    pad to the next alignment boundary
    value[val_len]
  footprint = (val_start − rec) + val_len
  ```
  **No per-record checksum** ⚠ ARTIFACT — recovery accepts any header whose lengths are sane (`:1169-1171`) and
  resolves same-key duplicates last-wins (`:1188-1195`); the value-shape parse at read time arbitrates a torn survivor.
  Removal/flush zeroes `magic` (tombstone) and punches the hole / `MADV_DONTNEED`s the pages (`reclaim_extent`,
  `:355-392`), so a cleanly unmounted dir scans EMPTY (`scan_live_segment_keys`, `:20-78`).
* Value of a staged-layout entry (`stage_write`, `:1387-1394`; `parse_staged_blob`, `:654-671`):
  `meta_len u64 BE ‖ StagedMetadata{fencing_token u64 BE ‖ original_size u64 BE ‖ path_len u32 BE ‖ file_path}
  ‖ payload[original_size]`; the `active_block[_ext]:` family pads the header to 4 KiB so payloads stay 4 KiB-aligned
  for the zero-copy DMA source (`:678-680`). ⚠ ARTIFACT: BE in a value, a textual path, no digest.
* Extent record (`active_block_ext:`): magic `"SQZEXT01"`, 40 B header, v2 xxh3 covers header+body (`:683-715`).
* **The staging write is two-phase and never destroys a live entry** (`reserve_and_write`, `R/tiering/nvme.rs:728-905`):
  phase 1 under the shard WRITE lock — `live` = every indexed extent (INCLUDING the current copy of this key) ∪
  `pending` in-flight placements, sorted; candidates = the aligned write cursor, 0, and the aligned end of every live
  extent (first-fit over the holes promotion/unlink punched); try candidates at/after the cursor, then wrapped; no fit
  ⇒ `false` (loud backpressure — the caller spills durably); reserve `[t, t+block)` in `pending`, advance
  `write_offset`. Phase 1b OUTSIDE the lock — write header, key, `meta_len BE`, meta, optional zero pad (the 4 KiB
  `align_data_to` of the block family), payload; `msync(MS_ASYNC)`. Phase 2 under the write lock — flip the index to
  the replacement, drop the `pending` entry, `reclaim_extent` the superseded copy (tombstone + punch/`MADV_DONTNEED`).
  A same-key replace is therefore ATOMIC for readers (one intact copy at every instant — the fstests 074/127/616
  transient-zeros window, `:812-819`) and torn-write immune (a crash between phases leaves two copies; recovery
  dedups last-wins). `get` (`:463-497`) takes `read_recursive` (never parks behind a queued writer) and returns a guard
  over `[val_start, val_start+val_len)` of the mmap — the zero-syscall "staging mmap" exception charter §1.2 names.
  The read-cache `put_impl` (`:531-716`) is the wrap-around-evicting twin (a cache may destroy entries).

### 3.3 RAM structures (`R/cache/nvme.rs:979-1047`)
`staging_dirs`, `max_write_bytes`/`max_read_bytes` (default ¼ of the dirs' aggregate capacity each,
`R/cache/mod.rs:180-208`), `current_staged_write_bytes` (the budget gauge), `staged_ledger: scc::HashMap<file_id,
(cost, stage_generation)>` (every charge/credit goes through it — the gauge can never ratchet), `active_block_index`
(conservative-present occupancy index for the W1 predicate), `active_block_custody` (the dismount drain's predicate),
`staged_writes_in_flight` (every ring key — a gauge), `write_tx` (bounded mpsc to the merge worker), two
`NvmeCache`s (staging / read), `space_freed_notify`, `staged_drained_notify`. `STAGE_GENERATION` is a process-global
`AtomicU64` starting at 1 (`:588-592`): generation 0 is reserved for mount-recovered entries so no new assignment can
forge it (the `staged_generation_aba_tests` class).

### 3.4 Admission and pressure — `stage_write` (`:1380-1557`)
1. Build the header; `padded_size = 8 + meta_len + data_len`. Refuse `StorageFull` with no staging dirs (`:1397-1402`)
   or when the host disk safeguard trips (< 1 % or < 100 MB free, `:1404-1410`).
2. `prior_cost` of the same `file_id` (re-stage charges the DELTA); `over_cap(cur) = cur − prior + padded > max_write`.
   Over cap ⇒ `kick_promotion(exclude = file_id, 64)` and wait on `space_freed_notify` up to 2,000 ms, re-kicking;
   still over ⇒ `StorageFull` (`:1432-1473`). The caller escalates to the durable spill (`staged_spill_escalations`).
3. Ring write + ledger charge under the file_id's ledger entry lock on the blocking pool (`:1483-1514`;
   `reserve_and_write` keeps the old copy live until the replacement is placed); refused ⇒ `StorageFull` loud
   (never destroys live entries, `:1515-1523`). The ledger row becomes `(padded_size, next_stage_generation())`.
4. **The 75 % high-water**: `high_water = max_write − max_write/4` (`:1548`); past it the entry is enqueued to the merge
   worker (`try_enqueue_staged_merge`, `:1549-1555`) — the hot path itself never promotes.
Ledger-lock invariant (`:1414-1425`): every executor-side ledger access is an `*_async` op.

### 3.5 Promotion points (charter §6.1 ¶2) — all run ONE primitive `promote_staged_file` (§4.4)
| Point | Where | Shape |
|---|---|---|
| Staging-pool pressure | merge worker `start_merge_worker` (`:2156-2207`) | batch ≤ 4 MiB or 500 ms flush timeout (`:2166-2167`), promoted at once while `gauge > high_water` (`:2181-2185`); `promote_batch` (`:2213-2260`) re-arms the pack arm (`begin_promotion_batch`), dedups file_ids, runs the primitive per item; failures leave the entry resident (never-wrong) |
| Clean unmount | `promote_all_staged_files_at_dismount` (`R/fuse_client.rs:24869`) over `resident_staged_files` (`R/cache/nvme.rs:1620-1622`) | gauges `dismount_promoted_{files,bytes,inline_files}`, `dismount_promote_failures`; then `seal_open_packs` before "Dismount clean". **Zig adds the judgement** (`staging.DismountVerdict`, `plane.judgeAtDismount`; `docs/evidence/rung-10-dismount-corpse.md`): a resident whose ino has no live inode (absent or `nlink == 0`) is a corpse's custody — destroyed, never promoted (`dismount_corpse_residents_destroyed`); a resident of a live ino whose layout does not name it (no layout, kind not staged, another file id — the ring release is the step AFTER a promotion's commit, outside the transaction, so a kill between the two leaves one) is destroyed as unreachable residue (`dismount_unnamed_residents_destroyed`); only a resource decline stays resident (never-wrong). A spilled block (`writeback.blockKey`) is judged by its ino alone. |
| `fsync` | `R/fuse_client.rs:23338-23370` iff `SQUEEZEFS_FSYNC_PROMOTE_STAGED=1` | **Zig (2026-10-21, 0002 row 551): promote-at-fsync is THE law of the write cache — fsync/fdatasync/syncfs promote the ino's residents before the ack, a declined promotion is EIO; the lever and the ranged sync went with the file ring.** The reference: placed BEFORE the barrier step so the data barrier covers the block and the meta barrier last names it; a failure never fails the fsync. **Zig (charter §1 item 10, 2026-10-19; 0002 row 549):** the lever is the ALTERNATIVE posture; off, the record stays staged and fsync/fdatasync make it power-cut durable in place — `Segment.syncResident`: ONE `IORING_OP_FSYNC` over the record's span on the handler's lane ring, coalesced across concurrent fsyncs (`SyncCoalescer`), after the promote leg and before the meta barrier; `syncfs` syncs the whole ring once when it has residents (`Segment.syncAll`). The reference's `sync_key` leg (§10.2 of `data_write_path.md`) was a whole-shard `File::sync_all` per call on the blocking pool |

### 3.6 Recovery after a crash (`R/cache/nvme.rs:1131-1148, 1238-1297`; `R/tiering/nvme.rs:1142-1212`)
Generation gate first (dead generation ⇒ wipe), format fence second, read-cache wipe, then `recover_index` on each
staging shard iff the dir had segment data: a linear header scan at the alignment grain, skipping whole footprints
(value interiors are never scanned for headers, `:1197-1200`), dropping insane headers loud, last-wins on duplicates,
`write_offset = max_end`. Ledger seeding: `active_block[_ext]:` keys → occupancy index (never budget-counted); every
other key → `staged_ledger[file_id] = (len, generation 0)` and the budget gauge; foreign-scope records are counted and
left in place (`staging_foreign_scope_records`). A staged entry present after recovery is the file's ONLY copy of its
acked-unfsynced bytes on THIS host; its layout on the metadata volumes may still say `block_map None` (dirty-at-crash)
— the read path serves the ring; a missing entry is the lost class below.

### 3.7 What a staged entry means to a READER, and the lost-reads counter (`R/routing.rs:16200-16226, 19805-19846, 2280-2282`)
Read ladder for `file_type == "staged"`: this client's ring (`read_staged`) → the durable mapping `block_map[0]` through
the binding-validated funnel (§4.6) → identity moved ⇒ re-dispatch (`staged_identity_retries`) → budget-counted but not
resident ⇒ a stage/promotion is IN FLIGHT: bounded backoff (4 yields, then 2 ms sleeps) → **STABLE lost identity**
(ring gone, no mapping, no in-flight stage, identity unchanged) ⇒ serve `min(offset+size, meta.size) − offset` ZEROS
(`lost_staged_range_len`), WARN once per read, `staged_payload_lost_reads += 1` (`note_lost_staged_payload`). Two
indistinguishable causes (the doc at `:16205-16214`): another client's live staged custody (the healthy
cross-client shape until that client promotes), or a crash discarded it. Charter §6.1: "stated, counted". Measured
(design §1.1): 155 lost reads on the residue note's leg B; `= 0` after a clean unmount of the writer is gate G-PK2.

---

## 4. PACKING — the full mechanism

### 4.1 The size dispatch and the two-faced law (`R/routing.rs:60-99, 15433-15450, 15619-15629`; design §5.5 `:269-289`)
`slot = pack_slot_len(stored_image_len)`; `pack_max_slot_bytes()` = explicit `SQUEEZEFS_PACK_MAX_SLOT_BYTES` else
`CHUNK/2`. Under the lever: `slot > pack_max_slot` ⇒ own block (`pack_own_block_promotions`); else the packed arm.
**Why `CHUNK/2`**: an own block wastes `CHUNK − s` forever (a staged file never grows in place), a packed tenant
costs one compaction copy of `s`; packing wins iff `CHUNK − s ≥ s`. The SAME break-even is the compaction victim rule
(`defrag::is_pack_victim(live) = live ≤ pack_max_slot_bytes()`, `R/defrag.rs:286-293`). Tie test
`pack_max_slot_default_is_half_the_chunk_and_the_grain_is_the_lba_law` (`tests/derivation_sweep_tests.rs:562-585`:
`pack_slot_len(1) = 4096`, `(4096) = 4096`, `(4097) = 8192`). Lever `SQUEEZEFS_SMALL_FILE_PACKING` default ON since
2026-09-10 (`:103-122`; measured §9).

### 4.2 RAM shapes (`R/pack.rs:69-104, 163-168, 213-232`)
```
OpenPack { be_id: String, scope: u64, allocator: Arc<BlockAllocator>, device: Arc<NvmeBlockDev>,
           base: u64 (device offset), base_key: String (persist_block_key(be_id, base) — every tenant's prefix),
           next_slot: AtomicU64 (monotone cursor), committed: AtomicU32 (GAUGE ONLY), opened_at: Instant,
           sealed: AtomicBool (CAS false→true; the first sealer runs the seal), seq: u64 (process-monotone open sequence) }
PackTenant { pack: Arc<OpenPack>, off: u64 (GRAIN-aligned), _inflight: InflightAllocGuard (the tenant's OWN registration on base) }
PackKey { be_id, scope }      // scope = pack_scope_key(meta_volume, forest_slot) on an armed set (R/routing.rs:1742-1747, 8155-8167), else 0
Packer { open: scc::HashMap<PackKey, Arc<ArcSwapOption<OpenPack>>>, refill: ArcSwapOption<sqz_flight::Sender<RefillOutcome>>, stopped: AtomicBool }
```
No pack-level in-flight guard: every TENANT holds its own `inflight_register(base)` from reserve to commit (the registry
is a per-offset counter) and the pack's +1 pin alone is declared to fsck's shared C2/C3 arm through the pack-open ledger
(`jobs::PACK_OPEN_LEDGER`, a `Mutex<Vec<String>>` of base keys, `R/jobs.rs:4086-4119`; entered at OPEN under a
transient registration, `R/pack.rs:418-426`).

### 4.3 Reservation, pin arithmetic, refill, seal (`R/pack.rs:108-153, 269-330, 332-348, 376-411, 418-440, 462-493, 503-547, 555-598`)
* `try_reserve(slot)`: `prev = next_slot.fetch_add(slot, AcqRel)`; owns `[prev, prev+slot)` iff `prev + slot ≤ CHUNK`
  (`:110-113`). The cursor is monotone — every reservation after the overflowing one overflows too.
* `reserve(router, slot, scope)` loop (`:269-330`): stopped ⇒ `Ok(None)`; an unsealed pack of `scope` ⇒ reserve →
  `inflight_register(base)` → `increment_refcount(base)` (acquire-from-nonzero; a refusal means a concurrent overflow's
  pin release was the LAST reference — seal, vacate, retry, `:290-305`); overflow ⇒ CAS-seal and the first sealer runs
  `run_seal(Full)`; no pack ⇒ `refill`.
* **Pin arithmetic**: `allocate_placed_block` mints at refcount 1 = the packer's PIN (`move_one`'s discipline); each
  tenant adds 1 at reserve; so while open `refcount = committed + in-flight + 1` (module doc `:16-24`); on the
  authority no sequence of tenant deletes can free the block under a DMA.
* `refill` (`:376-411`): single-flighted on `sqz_flight` — the leader `open_block`s ONE placed block (`:418-440`:
  `allocate_placed_block` → `persist_block_key` → ledger insert under a transient registration → cursor 0, `committed
  0`, `seq = PACK_OPEN_SEQ + 1`) and `install`s it (`:462-493`: an existing UNSEALED pack wins and the fresh block is
  abandoned unpublished); `StorageFull` ⇒ `stop_for_batch` (OQ-1: ONE aggregated WARN, the arm stops for the rest of
  the batch, `pack_refill_storage_full`; `begin_promotion_batch` re-arms, `:253-255, 444-456`); the flight is cleared
  BEFORE the send (`:406-409`). Counters `pack_reservation_refills`, `pack_blocks_opened`.
* `run_seal(kind)` (`:503-547`): vacate the table slot → `release_pack_reference(router, base_key)` → ledger remove →
  `pack_blocks_sealed_{full,dismount,drain}`; a `Terminal` release with `committed == 0` is an ABANDONED pack
  (`pack_blocks_abandoned`, ≈ 0). `release_pack_reference` (`R/block_allocator.rs:3004-3027`): untracked ⇒ no-op
  counted `pack_release_untracked_noops` + ERROR (must stay 0 single-writer); else the ROUTER ladder
  `free_block_verdict` ⇒ `Terminal | Nonterminal` (`PackRelease`, `:3724-3736`) — a pack block's end of life is
  byte-identical to a striped block's (tier purge, incarnation retire, reclaim queue, discard elision, grace).
* Seal triggers: FULL (`:306-314`), DISMOUNT (`seal_all`, `:555-571`, after the promotion pass, before "Dismount
  clean"), DRAIN (`seal_block`, `:579-598`, the drain mover found the open pack on its victim), BATCH (co-writer frame
  reply, design §5.6). **No idle seal in v1** — one half-empty block per data volume per scope is the stated cost,
  visible through `pack_open_block_{age_ms,occupancy}` (`gauges`, `:602-620`).
* Compaction hooks: `open_room_bytes` (`:627-642`, the lone-victim rule: a single victim frees a block iff the open pack
  already has room for its windows), `open_scopes` (`:646-655`), `open_block_keys` (`:658-669`, the opt-in census).

### 4.4 ONE promotion primitive, two halves (`R/routing.rs:15389-16115`)
`promote_staged_file(path, file_id, token) = prepare → commit` (`:15399-15411`).
**`prepare_promotion`** (`:15421-15450`) = `read_promotion_image` (`:15459-15542`): `staged_generation(file_id)` (gone
⇒ `None`), `read_staged` (counted-but-not-resident ⇒ reconcile the ledger), the W2 rider FENCE (a live extent record
⇒ defer, `:15477-15490`), `logical = max(cached size, raw_len)`; `logical ≤ inline_max` ⇒ `PromotionImage::Inline`;
else transform (`process_write_async`) → a stored image > CHUNK is the FIND-RW4-A geometry breach: stay resident, ERROR
(`:15518-15533`) → `DeviceImage{slot = pack_slot_len(len)}`. Dispatch (`:15440-15449`): lever ∧ `slot ≤ max` ⇒
`prepare_packed_promotion` (`:15667-15695`: `land_packed_image` → `Ok(None)` when the arm is stopped,
`pack_arm_stopped_promotions`); else `prepare_block_from_image` → `land_own_block` (`:15576-15602`: allocate, register
in-flight, key `{persist_block_key}:0:{len}`, ONE `write_block`, a DMA error abandons the never-published offset,
`publish_block`, the W-5 fsync touched stamp).
**`land_tenant`** (`:15730-15767`, shared with the compaction re-pack): key `{base_key}:{off}:{len}`; ONE
`write_block(base + off, image)` of exactly `slot` bytes (the device's unaligned arm zero-fills `[len, slot)` —
`nvme_unaligned_write_fallbacks` is EXPECTED growth on a packed passthrough row, design §10); `publish_block(base)`
(idempotent `fetch_or` — the FIRST tenant's publish stabilises the word; a stable word keeps the block movable and a
peer's tenant free answerable, design §5.2 `:232`); `pack_promoted_bytes`, `pack_slot_pad_bytes += slot − len`; a DMA
error releases the tenant (`release_pack_tenant`, `pack_slots_abandoned`) and errors.
**`commit_promotion`** (`:15952-16115`): `SQUEEZEFS_TEST_PACK_COMMIT_STALL_MS` parks a PACKED tenant between DMA and
commit (`:15985-15990`, the kill-9 window); then under `meta_lock_acquire(ino)` (3.5): re-check `file_type ==
"staged" ∧ file_id same ∧ staged_generation == gen` (`:16008-16013`) else `Ok(None)`; CoW insert `block_map[0] =
block_key` (`Arc::make_mut`); `size = max(size, raw_len)` (the leg-5 fail3 tape: never persist a size smaller than
the image, `:16020-16028`); `layout_dirty = false`; `refs = block_ref_ops_for_map_swap(ino, old_map, new_map)` — the
`+ref` and any displaced `−ref` in the SAME tx (`FIND-PK-2`, `:16031-16043`); `save_metadata_to_backend_refs`; a failed
save RETRACTS the refs (`:16050`); RAM publish; a displaced older copy is PURGED under the guard and FREED after it
(RES-1, `:16054-16061, 16088`); the ring entry is released INSIDE the guarded section, generation-gated
(`remove_staged_if_generation_async`, `:16063-16082` — leg 5: released outside the lock it landed inside an RMW's
stage→commit window and freed the sole copy). After the guard: committed ⇒ `layout_promoted_block` /
`note_landed_tenant_committed` (`layout_promoted_packed`, `committed += 1`); anything else ⇒ `release_landed_site`
(block arm: abandon the unpublished offset; packed arm: release the tenant's reference — TERMINAL when the pack sealed
and every sibling was deleted, so never under 3.5).
**Ordering law**: per tenant, DMA (step 4) completes before the commit (step 5) begins; the tenant's durable existence IS
its layout commit, which carries its C8 reference in the same checksummed journal entry (design §5.2 `:238`).

### 4.5 Scope on an armed set (`R/routing.rs:1737-1747, 8147-8167`)
`pack_scope_key(meta_volume, forest_slot) = (meta_volume << 32) | slot` — the native forest slot is 0 on EVERY meta
volume, so the volume ordinal keeps two volumes' natives apart (`sym_pack_tests::two_meta_volumes_native_inos_never_share_a_pack_scope`).
A pack block's tenants are inos of one slot of one volume ⇒ every reference lives in ONE slot tree ⇒ the W1
`refcount(b)` probe is one range read of that tree (charter §5.5 "Pack blocks are scoped per (writer, slot, data
volume)"). Unarmed: scope 0 = PK2's one open pack per data volume.

### 4.6 Reads — ONE ranged routed window (`R/routing.rs:11933-12035`; design §5.10 `:353-365`)
`read_mapping_window_raw(mapping, rel)`: `parse_block_mapping`; bare ⇒ whole device-block window (`read_block`);
exact ⇒ `cleaned = clean_block_key_ref(mapping)`; passthrough ∧ `rel` ⇒ image span `[rel.start, rel.end) ∩ [0, sz)`,
transformed ⇒ the whole image; window `[floor_GRAIN(start), ceil_GRAIN(end))` read at `off + win_start` on the
mapping's OWN backend through `read_block_range` (FIND-PK-0: never `self.nvme_writer`; FIND-PK-1: never a prefix
read from the block start); `packed_reads += 1`, `packed_read_bytes += window`; slice `[lo, hi)`.
`read_mapping_window` decodes (passthrough = identity). The stale-incarnation refusal from `read_block_range`
(`incarnation_ok`) inside a still-bound loop is the finding-51 contradiction: `classify_bound_mapping_error`
(`:12017-12035`) trips `invariant_tripwires["read_stale_bound_mapping"]` and answers `EIO`, never zeros, never the
reissued offset's bytes. Device-true per read: nothing publishes into the read tiers (OQ-6 open). PK8 lifts the
direct-drive prelude's screen for a passthrough tenant whose request lies inside its own slot (`SQUEEZEFS_IPC_DD_PACKED`).

### 4.7 Tenant ops (design §5.7 `:315-325`; `R/routing.rs:335-353, 16293, 21918, 22539`)
| Op | Effect |
|---|---|
| overwrite / append | whole-image RMW seeded through the funnel (binding-validated), `stage_write` same `file_id` (generation bump), publish `block_map None, layout_dirty`, `release_superseded_staged` ⇒ `free_block(base:off:len)` NONTERMINAL (`pack_partial_frees`); the slot is dead until compaction; the file re-packs at its next promotion |
| grow past `block_size` | the staged→striped transition (§2.5) releases the tenant reference |
| truncate-shrink, passthrough | `DurableClip::Mapping{old, new = base:off:len'}` — a pure mapping clip, zero device writes, same-key Delete+Put in one tx (population and RAM refcount unchanged; the old mapping is NOT freed), `pack_mapping_clips` |
| truncate-shrink, transformed | `DurableClip::Image` — re-encode, land through `land_stored_image` (packed when it fits), old reference partially freed |
| truncate-up | size-only (implicit-zero tail) |
| unlink | `−ref` in the destroy tx, then `free_block` — nonterminal until the last tenant; the last is the terminal free through the ordinary ladder (`pack_terminal_frees`) |
| clone, ring-resident source | stages a COPY under a fresh `file_id` |
| clone, promoted source | SHARES the mapping verbatim (`+1` durable ref, `+1` RAM pin — FIND-PK-3) — the legal identical-window class; composed with a clip ⇒ nested same-`off` windows, the second legal class |

### 4.8 Compaction — the defrag D1 pack arm (`R/defrag.rs:134, 286-293`; `R/routing.rs:15823-15899`; design §5.8 `:327-341`)
Occupancy of a block = `Σ_{distinct off} ceil(max len at that off) / CHUNK` over the census's C8 walk (`MappingRef`s),
walk-priced (`null` until measured). Victims = pack blocks with `live ≤ pack_max_slot_bytes()` ascending; a plan runs
only if it frees ≥ 1 block. `repack_window(ino, mapping)` (`:15823-15891`): read the window RAW (transform-opaque),
`xxh3` it, reserve a slot of `pack_slot_len(len)` in the open pack of the tenant's scope, `land_tenant`, read back and
verify the hash (mismatch ⇒ release + error); the destination is UNPUBLISHED until every referencer of that `off` is
republished to `base_key:off':len_i` (each with its own `len`); `abandon_repacked_window` releases a window no
referencer took. Mover quiesce clauses: defer an OPEN pack (`inflight_contains ∨ pack ledger hit`,
`pack_mover_open_defers`; a drain victim's open pack is SEALED, `pack_blocks_sealed_drain`) and a tenant with a
RESIDENT ring entry (`pack_mover_resident_defers`). Gauges `pack_compaction_{tenants_moved, windows_copied,
bytes_copied, bytes_reclaimed, blocks_freed, deferred}`, laws `copied ≤ reclaimed`, `windows_copied ≤ tenants_moved`.

### 4.9 Crash contract (design §5.4 `:257-267`; `R/pack.rs:31-37`)
Nothing durable records the open block. ≥ 1 committed tenant ⇒ N layout records name `base:off_i:len_i` + N C8
records ⇒ the block recovers ALLOCATED at `refcount = N`; the pin was RAM; the tail `[next_slot_at_crash, CHUNK)` and
abandoned slots are dead bytes inside a live block (compaction reclaims). No committed tenant ⇒ nothing names the block
⇒ it recovers FREE (the free list is the complement of the referenced set). A torn slot DMA is invisible (no layout
names it; a committed tenant's image completed before its commit began). A durable open-pack record was rejected: C8
oracle drift class + crash-leak class for no gain on the authority.

### 4.10 fsck C12 (`R/fsck.rs:55-57, 682-700`; design §5.9 `:343-351`)
`C12Overlap{vol, offset, ino_a, block_idx_a, ino_b, block_idx_b}`: two live mappings on one `(vol, offset)` with
`off_a ≠ off_b` whose `[off, off+ceil(len))` windows intersect — the only shape a packer bug or torn recovery produces
(a `fetch_add` mints no two windows sharing a start; clone/clip never move a start). Same-`off` windows of any lengths
are the two legal share classes. `C12Overrun{vol, offset, ino, block_idx, mapping}`: `off + ceil(len) > CHUNK`, or
`off % GRAIN ≠ 0`, or a decoration present but UNDECODABLE (the census resolves the base and would otherwise never see
it). Detection rides the census `mappings` vector (same-`off` collapsed to `max(len)`); zero-FP = settle + fresh
re-read of BOTH layouts under their inos' exclusive 4a leases; an open pack block is exempt; **report-only**
(`fsck_repair_classC12` structurally 0; `pack_tenant_overlap_findings` must-stay-0). The pack-open ledger's exemption
for the +1 pin is counted `fsck_pack_ledger_exempted` (`R/fuse_client.rs:6388`).

---

## 5. THE ROUTER

### 5.1 Volume table (`R/routing.rs:1336-1339, 1360-1372, 1846-1939, 2924-2964`; `R/lib.rs:1010-1011, 1048`)
`BackendRouter { default_allocator, default_device, backends: DashMap<be_id, Arc<StorageBackend{device, block_allocator}>>,
unhealthy_backends: DashMap<be_id, bool>, block_size: Arc<AtomicU64>, volume_records: ArcSwap<Vec<DataVolumeRecord>>,
placement_table: ArcSwap<PlacementTable>, placement_picks: DashMap<be_id, Arc<AtomicU64>>, reclaim, debt,
incarnation_era: Arc<AtomicU64>, fsync_ordinals, … }`. Volume ids `vol-{16 hex}` minted from `fastrand::u64`
(`R/lib.rs:1048`), never reused; `vol_tag = u64::from_str_radix(hex)` verbatim (`R/block_refs.rs:193-194`). Durable
state lattice `Active → Draining → Retired` (+ `Draining → Active` on undrain, `Disabled`; `R/lib.rs:1010-1011`);
`placement_eligible(be_id) = healthy ∧ (recordless ∨ state == active)` (`:3213-3221`). `build_backend` reuses the
default slot's Arcs when the backing device IS the default device — the bare-key invariant (`:2936-2946`);
`publish_backend` is the LAST step of an online add (write-path selection enabled last).

### 5.2 Health math and the placement table (`R/routing.rs:1376-1399, 1404-1496, 2781-2922, 3411-3448`)
* `BALANCE_PENALTY_K = 1000.0` (10 pp of fill above the set mean costs 100 health points), `BALANCE_PENALTY_CAP = 300`
  (balance never outvotes health — a degraded backend still loses to a merely full one). `balance_penalty(fill, mean)
  = clamp(round(1000 × (fill − mean)), 0, 300)` (NaN/≤ 0 ⇒ 0); `health_effective(h, fill, mean) = h.saturating_sub(penalty)`.
* `refresh_placement_table` (`:2781-2922`): candidates = `backend_0` only on a bare router, else every registration;
  census `fill = used/capacity` (`used = used_blocks × chunk`; an unbounded allocator falls back to a device-size probe
  at refresh cadence); `set_mean = Σused/Σcap` over ELIGIBLE rows; `device_health = (1 − fill) × 1000`; `weight =
  health_effective(…)` iff eligible else 0; band = eligible rows with `weight ≥ max × 9/10` (a zero max keeps every
  eligible row); `fill_spread = max − min` fill over eligible rows (the G-VL-8 gauge `backend_fill_spread`);
  `placement_table_refreshes += 1`. Runs on the health-worker cadence and on every registration/state/health/retire
  transition — NEVER per write.
* `PlacementTable::pick(healthy)` (`:1462-1482`): `start = rr.fetch_add(1)`; scan the band from `start`, first row the
  instant `unhealthy_backends` gate admits ⇒ `row.take()` (`backend_placement_picks += 1`). ONE ArcSwap load + an
  O(#backends) scan, zero locks, zero syscalls. `get_active_backend` (`:3420-3448`) rebuilds ONCE when the table is
  stale for the candidate population (`placement_table_current`, `:3411-3418`) or the band is empty, then errors
  "No healthy storage backends available for write". The choice table is immutable per snapshot (charter §6.1
  "a published immutable choice table").
* `allocate_placed_block` (`:3486-3511`) = pick + `allocate_block_grace_bounded` as ONE act; `StorageFull` runs the
  pressure-supply hook once (closes open rewrite epochs) and retries one fresh pick; `allocate_placed_block_under_guard`
  (`:3528-3539`) is the form for callers holding 3.5 or a 4a guard (the hook's close would park there).

* Diagnostic twin `get_backend_health(be_id)` (`:3368-3394`): `free_blocks / total_blocks × 1000` where
  `total_blocks = dev_size / (4 × 1024 × 1024)` — a hard-coded 4 MiB literal and a device-size probe per call; the
  `.config` surface ONLY, never placement. ⚠ QUIRK (§11 Q-17b).
* Health lattice as it touches routing: `is_backend_healthy` (`:3352-3362`) = not in `unhealthy_backends` ∧ the device
  node exists (TTL-cached probe, `backend_health_probe_tests`); the fail-stop latch marks `unhealthy_backends`, which
  the pick's closure reads INSTANTLY even against a stale snapshot (`placement_tests::unhealthy_mark_is_instant_against_a_stale_table`);
  `Draining`/`Retired`/`Disabled` leave the band at the next refresh (`eligibility_and_refresh_on_state_change`), and
  the drain mover moves blocks off through `move_one`/the pack re-pack (§4.8).

### 5.3 Metadata volume health, `pick_mint_volume`, `MINT_SPREAD`, the mint rotor (`R/meta_backend/mod.rs:50-61, 2531-2560, 2585-2632, 3266-3297`)
* `get_volume_health(idx)` = `min(1000, free_extents/total_extents × 1000)`, 0 when disabled, cached 500 ms
  (`:2531-2560`) — directory/file placement by estimated remaining capacity (charter §5.4 last bullet).
* `pick_mint_volume(parent_v)` (`:2585-2632`): one volume ⇒ the parent's; else candidates = enabled volumes (armed: only
  volumes this node OWNS — a preference, never a gate) with their health; sort desc; keep `health ≥ max × 9/10`;
  `META_COUNTER.fetch_add(1) % top.len()` — the process-global round-robin (charter §5.4 "children mint round-robin over
  the healthy volumes").
* `DERIVED_ROUTING_WIDTH = u16::MAX + 1 = 65,536` (`:50`); `MINT_SPREAD = STAMP_MAX_CURSORS / 4 = 256/4 = 64`
  (`:61`; `R/kv/checkpoint.rs:281`): a freshly spread volume consumes ≤ ¼ of its stamp-cursor budget, leaving ¾ for
  cursors travelling in with migrated slots. Per volume the MINT SET = its first `min(MINT_SPREAD, hosted)` hosted
  slots ascending (`:1034, 1166-1170`).
* `pick_mint_slot(volume_idx)` (`:3266-3297`): width ≤ 1 ⇒ 0; armed symmetric plane ⇒ `lease_mint_slot` (parent
  affinity below `A_max`, else the rotor slot with most headroom; the overflow arm asks the manager for one more slot
  up to `2M`); a client-owned dedicated slot (rung 14) if armed; else `k = mint_rr[volume].fetch_add(1) % mints.len()`,
  slot `mints[k]` — the per-volume rotor. Gather mode (`user.squeezefs.gather`) forces the directory's slot (`:3357-3374`).
* **How a new ino gets its slot/volume** (charter §5.4; port `src/meta/inos.zig:295-316`): `v = pick_mint_volume(parent_v)`
  → `slot = pick_mint_slot(v)` → `local = allocate_local_ino_in_slot(v, slot)` (the slot's cursor) → `ino =
  makeGlobalIno(local, slot, W) = (local − 2)·W + slot + 2`; the inverse `routeIno(ino, W) = {slot = (ino−2) % W,
  local = (ino−2)/W + 2}`; ino 1 → slot 0. The port's `inos.routeIno`/`makeGlobalIno` ARE this arithmetic; the rotor
  (`mint_rr[v]`) and the mint set are what rung 4/5 add on top (`geometry.routing_width = 65,536`,
  `inos.zig:295`). A record's slot tree is `key_ino >> 40` (v4, 0002 row 35) — the mint slot and the forest slot are
  the same number by construction (`forest_slot_of_ino(local)`, `R/routing.rs:8161-8166`).

### 5.4 The layout-type flip law, restated for the router
Every fresh-block site (write-through, overlay, flush/fold uploads, spills, blobs, the own-block promotion arm, the
packer's refill) calls `allocate_placed_block` and DMAs BEFORE the layout commit that names the block; the commit
carries the `+ref`/`−ref` in the same tx; the displaced binding is freed only after the durable publish and after the
3.5 guard drops (RES-1). Durable promotion = the same law applied to a type flip (§2.5, §4.4).
Port detail: a promotion picks ONCE. That pick names the transform and the pack key `(volume, scope)`, and the
packer's refill mints on the key's volume, never on a second pick (the reference keys its row by the refill's
answer instead). A pack placed off its key's volume escaped fsck's open-pack exemption: leak67.

---

## 6. THE INLINE BOUND — derivation and the 16 KiB measurement (`R/routing.rs:4-54, 358-404, 651-660`; `R/env_knobs.rs:166`)
* `INLINE_MAX_FLOOR = 4096` = the default at EVERY node size (`derived_inline_max_bytes(cap) = min(4096,
  ceiling(cap))`): the cost priced is per PAYLOAD BYTE on the metadata plane, not per node — an inline write rides the
  plane twice (journal entry + CoW node append ≈ 2× its bytes) against the staged layout's fixed ≈ 0.3 KB per file;
  one page is where the record's payload costs about what a block mapping does.
* Format bound `inline_max_bytes_ceiling(cap) = max(cap − LAYOUT_INLINE_HEADROOM, FLOOR)` with `cap = xattr value cap
  = min(64 KiB, node_size/4)` and `LAYOUT_INLINE_HEADROOM = 4096` (the layout wire's framing allowance): 60 KiB at the
  256 KiB node, 12 KiB at the 64 KiB floor. Registry range top `INLINE_MAX_CEILING = RECORD_VALUE_CAP_CEILING − 4096`
  (Linux `XATTR_SIZE_MAX`-derived). `inline_max_bytes_for_cap` applies an explicit override verbatim up to the
  volume's bound; above it the bound applies, logged once (`:393-402`) ⚠ QUIRK (a clamp; charter §1.6 says refuse).
* **Measured** (`.benchmarks/2026-09-09-inline-raise-sweep-local.md`, cited `:11-14` and design §1.1): 16 KiB inline
  = −34 % files/s, 2.0× fsync µs, **59× journal bytes/file**, 50× node-append bytes/file; 32 KiB = −68 %, 3.3×,
  117×/96× and a 1 GiB metadata volume heap-exhausted and FAIL-STOPPED in 20 s. The default flipped back to one page
  (`00942fd2`). Tie test `inline_max_bytes_default_is_one_page_and_the_bound_is_the_kv_value_cap`
  (`tests/derivation_sweep_tests.rs:483`).
* Per-ino ceiling in force = `inline_max_bytes_for_cap(min over volumes of xattr_value_cap)` (`:8287-8298`).

---

## 7. KNOBS (every row read by this surface; class per charter §1.6: **lever** = measurement A/B, **seam** = `SQUEEZEFS_TEST_*`, **delete** = retire)

| Knob (`R/env_knobs.rs`) | Kind / range | Default | Purpose | Class |
|---|---|---|---|---|
| `SQUEEZEFS_INLINE_MAX_BYTES` (:166) | Int `4096..=INLINE_MAX_CEILING` | 4096 (derived) | the inline ceiling override up to the format bound; published `inline_max_bytes` | lever |
| `SQUEEZEFS_SMALL_FILE_PACKING` (:167) | Bool | on | the packed promotion arm; `0` = one-block-per-file control (read funnel lever-independent) | lever |
| `SQUEEZEFS_PACK_MAX_SLOT_BYTES` (:168) | Int `GRAIN..=CHUNK` | derived `CHUNK/2` | own-block threshold (= compaction victim line); published `pack_max_slot_bytes` | lever |
| `SQUEEZEFS_FSYNC_PROMOTE_STAGED` (:254) | Bool | off | RETIRED 2026-10-21 (successor `SQUEEZEFS_WRITE_CACHE_PCT`): promote-at-fsync is the write cache's law | retired |
| `SQUEEZEFS_PUBLISH_COALESCE_MAX` (:169) | Int `0..=2^20` | 64 | per-ino publish coalescing window; `≤ 1` = serialized | lever |
| `SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN` (:170) | Int `0..=2^20` | 64 | delta chain cap before a full re-base; `0` = full Puts only | lever |
| `SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX` (:171) | Int `0..=2^20` | 0 (= derive from `META_COMMIT_BATCH_TXS`) | delta-class saves per multi-ino tx; `1` = per-save | lever |
| `SQUEEZEFS_KVMAP` (:172) | Bool | on | new beyond-inline crossings take tree 7; never disables head resolution | lever |
| `SQUEEZEFS_KVMAP_OVERLAY` (:173) | Bool | on | partial map store above `budget/16` | lever (bracket control) |
| `SQUEEZEFS_MAP_MIGRATE_CHUNK` (:174) | Int `64..=1024` | 512 | crossing-train ops per tx (`sweep threshold = chunk × 64`) | lever |
| `SQUEEZEFS_REWRITE_SHADOW` (:185) | Bool | on | shadow dual-map rewrite epoch | lever |
| `SQUEEZEFS_BLOCK_REFS_VERIFY` (:163) | Bool | off | the C8 oracle at mount | lever (diagnostic) |
| `SQUEEZEFS_DEFAULT_BLOCK_SIZE` (:165) | Int `4096..` | 4 MiB | `CHUNK_SIZE` when format recorded none | **delete** (a format fact, never a knob — §11 Q-12) |
| `SQUEEZEFS_IPC_DD_PACKED` (:285) | Bool | on | direct-drive packed-tenant read arm | lever |
| `SQUEEZEFS_PUBLISH_CONVEYOR_GROUP` (:337) | Bool | on | owner-side one-group-per-frame (the co-writer pack law's dependency) | lever |
| `SQUEEZEFS_TEST_PACK_COMMIT_STALL_MS` (:262) | Int `0..=600000` | 0 | park a packed tenant between DMA and commit | seam |
| `SQUEEZEFS_TEST_GROWTH_COMMIT_STALL_MS` (:402) | Int | 0 | park the staged→striped flip between DMA and commit | seam |
| `SQUEEZEFS_TEST_STAMP_WRITER_SCOPE` (:414) | Bool | 0 | stamp incompat bit 10 at format | seam |
| `SQUEEZEFS_NODE_ID_FILE` (:426) | Str | none | node identity source (writer scope) | lever (ops) |
| read-cache budgets `read/write_mem_cache_size`, `read/write_disk_cache_size` (`R/cache/mod.rs:104-208`) | size strings | 10 % RAM / 25 % aggregate | the ring and read-cache sizes | **config**, not env |

⚠ Three rows are read with a SECOND parser (`std::env::var … parse::<i64>()`): `PUBLISH_COALESCE_MAX`
(`R/routing.rs:1018-1028`), `LAYOUT_DELTA_MAX_CHAIN` (`:1054-1064`), `PUBLISH_COMMIT_GROUP_MAX` (`:1263-1273`) —
0002 row 14's class. `MAP_MIGRATE_CHUNK` is CLAMPED in-process (`:1220-1222`). `READ_HOT_BLOCK_CACHE_MB` and
`READ_ADMISSION_FILL_PCT` are read raw in `R/cache/mod.rs:132-163`. All five become `knobs.intKnob` rows in the port.

---

## 8. GAUGES AND COUNTERS (names as the `.stats` inode publishes them; `R/fuse_client.rs:5854-5978, 6153-6172, 6215, 6361, 6388, 7439-7573, 11173-11181, 11251-11269, 11486-11488, 13904, 13951, 14189`)

| Family | Meaning | Law |
|---|---|---|
| `layout_{inline,staged,striped}_writes` | write-path layout mix | engagement |
| `layout_promoted_{inline,block,packed}`, `pack_own_block_promotions` | promotion dispatch | `packed + block + inline ≡ promotions`; `own_block = 0` on a ≤ CHUNK/2 population |
| `pack_promoted_bytes`, `pack_slot_pad_bytes` | image bytes packed; GRAIN pad | `pad/bytes` = the 4 KiB-grain tax (Alt G's instrument) |
| `pack_blocks_{opened,sealed_full,sealed_dismount,sealed_drain,abandoned}` | pack life cycle | `abandoned ≈ 0` |
| `pack_reservation_refills`, `pack_refill_storage_full`, `pack_arm_stopped_promotions`, `pack_slots_abandoned` | refill / OQ-1 / dead slots | `refills ≈ opened` |
| `pack_partial_frees`, `pack_terminal_frees` | tenant releases | `partial + terminal ≡ releases` |
| `pack_release_untracked_noops` | release on an entry already retired | **must-stay-0 single-writer** |
| `pack_mapping_clips`, `pack_mover_open_defers`, `pack_mover_resident_defers` | PK3 tenant ops / mover interplay | — |
| `served_publish_free_block_refusals` | the authority's served-publish screen | **must-stay-0** |
| `pack_open_blocks`, `pack_open_block_age_ms`, `pack_open_block_occupancy` (permille), `pack_open_scopes`, `pack_open_blocks_census` (opt-in) | the idle-tail instrument | `open ≤ volumes × scopes` |
| `frag_d1_pack_occupancy{,_mean}` (permille+1, `null` until measured), `frag_d1_pack_blocks`, `pack_reclaimable_bytes`, `pack_blocks_below_half` | D1 pack axis | walk-priced |
| `pack_compactions`, `pack_compaction_{tenants_moved,windows_copied,bytes_copied,bytes_reclaimed,blocks_freed,deferred}` | the mover | `copied ≤ reclaimed`; `windows ≤ tenants` |
| `packed_reads`, `packed_read_bytes` (sharded) | the read funnel | `packed_read_bytes ≤ Σ ceil(image)` + the sub-window law |
| `packed_mapping_refusals` | the parse choke point refused | **must-stay-0** |
| `pack_tenant_overlap_findings`, `fsck_repair_classC12`, `fsck_pack_ledger_exempted` | C12 | first **must-stay-0**, second structurally 0 |
| `staged_payload_lost_reads` | zeros served for an unreachable staged payload | 0 on any client of a cleanly unmounted set |
| `staged_identity_retries`, `staged_rmw_pooled_seeds`, `staged_spill_escalations`, `staged_rider_extent_writes`, `staged_rider_folds`, `extent_spills`, `extent_spill_bytes` | staged write path | — |
| `staged_truncate_inplace_shrinks`, `staged_truncate_durable_clips` | truncate arms | — |
| `staging_generation_discards`, `staging_scope_upgrades`, `staging_foreign_scope_{refusals,discards,records}`, `staging_drain_barriers`, `staging_writer_scope` | the generation/scope gate | `refusals` = a mount refused |
| `staging_put_bytes_{drain,flush,teardown,wt_fallback}`, `staging_sibling_probes`, `staging_sibling_hops_elided` | ring traffic | — |
| `staged_writes_in_flight`, `current_staged_write_bytes` (`staging_mmap`) | ring population / budget | levels |
| `staging_cell_state_bytes` (`staging_cell_state`, attribution-only) | the live cells' value-hash states, `@sizeOf(ValueHash)` = 1,056 B each | `≤ maxCells(capacity) × 1,056` (`Segment.cellStateBound`) |
| `staging_tables_bytes` (`staging_tables`, attribution-only) | the segment's fixed per-cell tables, preallocated at `maxCells` for the ring's life: `cells` (112 B since 2026-10-19: the retired list, the index sequence and the supersession — 80 before), `free_cells` (4 B), `pending` (40 B), `scratch` (40 B, one extra), the key index (`ceilPow2(n / 0.8 + 1)` slots × 33 B) | `= Segment.tablesBound(capacity)` per open segment: 36,047,084 B at the venue's 546 MiB ring, 4,292,844 B at 64 MiB — 6.3 % of the ring; still through puts and removes, returned at `deinit` |
| `dismount_promoted_{files,bytes,inline_files}`, `dismount_promote_failures`, `dismount_corpse_residents_destroyed`, `dismount_unnamed_residents_destroyed`, `fsync_promoted_files` | the two promotion points (the dismount pass's destroyed arms are Zig's) | a clean unmount's fsck finds no `[C4]` custody orphan |
| `nvme_unaligned_write_fallbacks` | device pad arm | EXPECTED growth per packed non-4 KiB image (pre-declared, design §10) |
| `block_key_incarnation_{refusals,unknown,exhausted}` | stale-lifetime refusal / honest unknown / mint exhaustion | `refusals` **must-stay-0** |
| `meta_kv_block_refs_{drift,recovered,unresolved,staged,released}` | C8 ledger | `drift` **must-stay-0** |
| `block_double_frees`, `block_untracked_free_refusals`, `block_live_free_refusals` | allocator forensics | **must-stay-0** |
| `invariant_tripwires["read_stale_bound_mapping"]`, `["served_mark_shared_under_patch"]` | §4.6 contradiction; §5 served mark under a patch | **must-stay-0** |
| `backend_placement_picks`, `backend_placement_weight`, `backend_fill_ratio` (per row), `backend_fill_spread`, `placement_table_refreshes` | §5.2 | `fill_spread` → ≤ 10 pp after rebalance |
| `inline_max_bytes`, `pack_max_slot_bytes` | published derivations | — |
| `slot_rotor`, `rotor_mints`, `slot_rotor_cap_refusals`, `mint_redirects` | §5.3 | — |
| `layout_publish_batches`, `layout_publish_batched_blocks`, `layout_indirect_map_reads`, `layout_indirect_map_read_bytes` | publish coalescing / legacy blob | blob reads 0 on a kvmap set |

---

## 9. CONSTANTS WITH WHY, AND MEASURED NUMBERS

| Constant | Value | WHY (file:line) |
|---|---|---|
| `CHUNK_SIZE` | 4 MiB | the striped block = the allocator chunk (`R/block_allocator.rs:112`); the stored-image ceiling (`:128-140`) |
| `LBA_GRAIN` | 4096 | the O_DIRECT device fd refuses non-LBA windows; no 512-native probe until `ranged_read_unaligned_bounces`/`pack_slot_pad_bytes` shows waste (`R/routing.rs:17-26`) |
| `pack_max_slot` | `CHUNK/2` | the break-even `CHUNK − s ≥ s` (`:82-99`) |
| `INLINE_MAX_FLOOR` | 4096 | one page; metadata-plane byte cost (§6, `:4-16`) |
| `LAYOUT_INLINE_HEADROOM` | 4096 | the layout wire's framing allowance under the KV value cap (`:651-660`) |
| `INODE_META_STRIPES` | 4096 | RAM-only read→merge→save hold, never a commit park (`:437-441`) |
| `high_water` | `max_write − max_write/4` (75 %) | arm the drain BEFORE the pool hard-fills; the hot path never promotes (`R/cache/nvme.rs:1544-1555`) |
| merge batch | 4 MiB / 500 ms | amortize promotion commits; immediate under pressure (`:2166-2167, 2181-2185`) |
| over-cap wait | 2,000 ms, 64 kicks | bounded park for the merge worker to credit space (`:1443-1459`) |
| disk safeguard | < 1 % or < 100 MB free | host-FS safety (`:1404-1410`) |
| `SHARD_REPLACE_SLACK` | 64 KiB | two key frames + metadata/pad frames + alignment per copy (`:1057-1060`) |
| default shards | `max(next_pow2(cores), 16)`, halved to fit `2 × entry + slack` | FIND-VS-B structural refusal (`:1091-1101, 1188-1190`) |
| tiny pool | < 10 MiB ⇒ 1 shard | no lock parallelism worth having (`:1092-1094`) |
| `STAGE_GENERATION` start | 1 | 0 reserved for mount-recovered entries (`:580-592`) |
| `BLOCK_MAGIC` | `0xCAFEBABE`, header 12 B | segment record framing (`R/tiering/nvme.rs:11-12`) ⚠ |
| `BALANCE_PENALTY_K` / `CAP` | 1000 / 300 | 10 pp = 100 points; balance never outvotes health (`R/routing.rs:1373-1381`) |
| band | `≥ 90 % of max weight` | spread, not fairness bookkeeping (`:2886-2892`); mint volumes use the same 9/10 (`R/meta_backend/mod.rs:2622-2625`) |
| `DERIVED_ROUTING_WIDTH` | 65,536 | the u16 slot-id wire type (`:40-50`) |
| `MINT_SPREAD` | 64 = `STAMP_MAX_CURSORS/4` | ≤ ¼ of the cursor budget per fresh volume (`:52-61`; `R/kv/checkpoint.rs:281`) |
| health cache | 500 ms | mkdir storms must not scan allocator occupancy per op (`R/meta_backend/mod.rs:2538-2546`) |
| `RUN_LEN_MAX` | 4096 | the exact-miss floor probe's bound — a format law (`R/kv/block_map.rs:124-129`) |
| staged binding-retry bound | 64 | then loud, never a silent zeros seed (`R/routing.rs:18809-18815`) |
| in-flight backoff | 4 yields then 2 ms | a stage/promotion is in flight (`:19820-19834`) |
| delta chain / coalesce | 64 / 64 | bounded cold-read fold and replay working set per key (`:1049-1053`) |
| kvmap write-map budget | `budget/16`, no floor | a floor would admit a map the budget cannot hold (`:1086-1107`) |

**Measured (normative, A-B-B-A on squeeze-test unless stated):**
* Packing default flip (`.benchmarks/2026-09-10-packing-rows-squeeze-test.md`, cited `R/env_knobs.rs:167`, design `:492`):
  files/s +0.5 %, fsync −3.7 %, device/user bytes 1.00× both arms; **96,000 fsync-promoted 16 KiB files = 375 blocks
  instead of 96,000; dismount 20,000 → 148; the legacy volume 20,000 → 147 after one `defrag --pack`**.
* The 64× space law it replaced (`.benchmarks/2026-09-09-fsync-promote-staged-ab.md`, design §1.1): 140,183
  promotions × 4 MiB = 548 GiB against a 480 GiB set in 8 s; 132,439 promotions FAILED `StorageFull`; the fstests
  TEST device's 2,193 staged files = 8.6 GiB of blocks for 100–140 MiB of data at every clean unmount; the fsync
  lever's own latency −16.7 % files/s, 338 → 425 µs, p99.9 +122 % (a mix of promotion and the ENOSPC storm).
* Inline raise (§6): 16 KiB −34 % / 59×; 32 KiB −68 % / 117× + a fail-stopped 1 GiB meta volume.
* Staged write path metadata economy: ≈ 308–312 B journal + ≈ 299–309 B node-append per file regardless of size;
  a promotion adds one layout commit + a 32 B C8 record (design §1.1).
* C8 reference cost 40–52 ns + 46 B per reference (`.benchmarks/2026-08-04-durable-block-refcounts.md`).
* FIND-PK-2 repro (design §1.3): unfixed drift 200/200, 8 of 200 promoted files CLOBBERED by fresh striped writes; fixed 0/0.
* `HashMap::clone` of block maps = 24 % of daemon CPU on the cold rand-4k row before `Arc` sharing (`R/routing.rs:748-752`).

---

## 10. TESTS (name — one sentence; **device** = needs a block device / loop file; **mount** = needs a FUSE mount; else pure/in-process)

`tests/packed_mapping_wire_tests.rs` — `wire_law_holds_at_every_off_len_inside_the_block` (random `(off,len)`: parse returns them, clean returns the base, whole-block false, STRING round-trip); `overrun_and_malformed_decorations_refuse_eio_never_another_tenants_bytes`; `a_tail_tenant_reads_its_own_window_not_the_block_prefix` (FIND-PK-1, `read_fill_dma_bytes = ceil(len)`); `the_c7_scrub_reads_the_tenant_window_of_a_nonzero_rel_off_mapping`; `a_clone_of_a_promoted_staged_file_pins_the_shared_block` (FIND-PK-3); `a_stale_incarnation_on_a_still_bound_mapping_refuses_eio_and_trips_the_tripwire`; `a_promoted_staged_file_placed_off_the_default_volume_reads_byte_exact` (**mount**, 2 data volumes, FIND-PK-0). In-process fixtures otherwise.
`tests/small_file_packing_tests.rs` (**mount**) — `a_clean_unmount_packs_the_population_into_shared_blocks_other_clients_read_exactly` (≤ `ceil(Σceil/CHUNK) + volumes` blocks, remount at another mount point byte-exact, `staged_payload_lost_reads = 0`, a second population past the promoted offsets); `lever_off_promotes_one_block_per_file_and_packs_nothing`; `a_plain_mount_packs_by_default`; `kill9_inside_the_pack_windows_loses_nothing_and_leaks_nothing` (`SQUEEZEFS_TEST_PACK_COMMIT_STALL_MS`; C2/C3/C8/C12 empty); `online_fsck_during_live_packing_reports_nothing_ten_times` (the pin excused by the pack ledger, tenants by the in-flight registry).
`tests/pack_tenant_ops_tests.rs` (**mount** + in-process) — `passthrough_truncate_shrink_of_packed_tenants_clips_the_mapping_with_no_data_io` (population and refcount unchanged, oracle clean); plus the growth-vs-promotion stale-reference repro behind `SQUEEZEFS_TEST_GROWTH_COMMIT_STALL_MS`.
`tests/pack_compaction_tests.rs` — `occupancy_is_exact_on_a_planted_population_and_counts_both_share_classes_once`; `victims_below_half_repack_into_one_fresh_pack_and_free_while_a_block_above_half_is_untouched`; `the_legacy_one_block_per_file_population_is_compacted_into_packs`.
`tests/fsck_c12_tests.rs` — `two_tenants_intersecting_at_different_off_are_one_report_only_finding`; `overrun_misaligned_and_undecodable_decorations_are_each_reported`; `two_whole_block_referencers_are_not_a_finding`; `identical_windows_of_a_cloned_promoted_small_file_are_not_a_finding`; `nested_same_off_windows_are_not_a_finding_but_a_moved_start_is`; `a_tenant_released_between_the_census_and_the_verify_does_not_fire`; `an_open_pack_block_never_fires_and_the_sealed_one_does`; `a_healthy_packed_population_runs_c12_empty_ten_times_under_live_promotion`; `an_online_census_is_bounded_at_the_ino_watermarks_it_started_with`.
`tests/sym_pack_tests.rs` — `a_layout_publish_through_the_routed_layer_lands_its_reference_in_the_owners_slot_tree`; `a_flat_volumes_ledger_keys_the_global_owner_verbatim`; `a_backend_committed_reference_probes_in_the_slot_its_owner_names`; `an_armed_set_packs_per_slot_and_an_unarmed_mount_keeps_pk2s_one_scope`; `two_meta_volumes_native_inos_never_share_a_pack_scope`; `a_pack_block_whose_slot_is_handed_over_keeps_its_whole_population_in_one_tree`; `a_gather_mode_directorys_children_live_in_its_slot_past_the_cap_and_never_automatically`; `an_unarmed_mount_ignores_the_gather_xattr_and_the_gauge_stays_zero`.
`tests/placement_tests.rs` — proptests `penalty_clamp_bounds`, `penalty_monotone_in_fill`, `balance_never_outvotes_health`; `penalty_slope_and_cap_exact`; `band_and_round_robin_over_imbalanced_table`; `eligibility_and_refresh_on_state_change`; `unhealthy_mark_is_instant_against_a_stale_table`; `picks_are_table_only_no_refresh_no_syscall`; `refresh_counter_and_fill_gauge_track_rebuilds`; `fill_spread_converges_on_imbalanced_set`; `w1_patch_unaffected_while_placement_prefers_other_backend`; `stats_inode_carries_the_placement_family`.
`tests/inline_raise_tests.rs` (**mount**) — `a_small_fsynced_file_is_inline_and_a_live_reader_mount_sees_it`; `the_default_ceiling_is_one_page_and_keeps_the_small_file_staged`; `the_override_at_the_format_bound_is_accepted_and_published`; `growth_past_the_inline_ceiling_promotes_durably`; `dismount_promotes_small_staged_files_inline_without_blocks`; `the_fsync_lever_dispatches_small_staged_files_inline`; `an_inline_ceiling_above_the_kv_cap_refuses_the_process`.
`tests/fsync_promote_staged_tests.rs` (**mount**) — `lever_off_fsync_leaves_staged_layout_files_resident`; `lever_on_fsync_promotes_staged_layout_files_for_other_clients`.
`tests/dismount_staged_residue_tests.rs` (**mount**) — `a_mount_holding_only_staged_layout_files_unmounts_without_the_drain_wait`; `a_clean_unmount_promotes_every_staged_layout_file_for_other_clients`; `a_promoted_staged_file_owns_its_block_in_the_durable_ledger` (FIND-PK-2 contract 3); `a_full_data_volume_refuses_new_bytes_before_the_ack_and_keeps_every_acked_byte`.
`tests/staging_generation_tests.rs` — `reformat_over_stale_staging` (+ `_v3`), `same_generation_warm_remount_keeps_staging` (+ `_v3`), `volume_set_generation_identity_contracts`, `unstamped_nonempty_staging_is_discarded_once`, `garbage_marker_with_nonempty_staging_is_discarded`.
`tests/staged_crash_recovery_tests.rs` — `test_recover_index_true_geometry_survives_post_recovery_writes`; `test_recover_index_ignores_false_magic_inside_value_interiors`; `test_recover_index_discards_corrupt_header_and_resyncs`; `test_read_cache_segments_do_not_resurrect_across_remount`; `test_staged_meta_with_lost_ring_entry_reads_zeros_not_error`; `test_staged_ring_survives_kill9_remount_and_post_recovery_writes` (fork child).
`tests/staged_generation_aba_tests.rs` — `stale_generation_never_matches_a_new_incarnation`; `generation_is_single_use_across_release_and_restage_cycles`.
`tests/staging_budget_tests.rs` — `test_restage_same_file_does_not_leak_staging_budget`; `test_unlink_returns_staging_budget`; `test_staging_full_writes_recover_and_drain`; `test_spilled_write_reads_back_new_data_and_returns_budget`; `test_stage_write_shard_full_is_loud_never_lossy`; `test_flush_guard_holds_do_not_stall_staging_churn`; `test_remount_budget_counts_staged_entries_only`.
`tests/staged_truncate_stale_tests.rs` — `trunc_cycle_reads_zeros_under_ring_fragmentation`; `trunc_cycle_reads_zeros_after_promotion`; `trunc_cycles_under_promotion_churn_never_resurrect`; `staged_minifsx_under_promotion_churn_byte_exact`; `ring_in_place_shrink_clips_reads_and_refuses_growth`; `rmw_after_trunc_cycle_stays_clipped_and_sized`.
`tests/staged_identity_visibility_tests.rs` — `restage_storm_reads_never_transient_zeros`; `layout_transition_storm_…`; `promotion_pressure_storm_…`; `ring_same_key_replace_is_atomic_for_readers`; `striped_pressure_storm_reads_never_stale_or_zeros`; `ring_concurrent_placements_never_overlap`; `dead_ring_extents_do_not_retain_rss`; `warm_read_loop_yields_to_peer_tasks_on_one_worker`.
`tests/staged_multifile_ring_pressure_tests.rs` — `extend_vs_promotion_never_strands_payload`; `promote_patch_cycles_never_clobber_neighbor_mappings`; `rmw_commit_vs_promotion_never_frees_sole_copy`; `multifile_ring_pressure_seed_{a,b,c}`.
`tests/layout_delta_fold_tests.rs` — `layout_delta_folds_onto_base_put`; `layout_delta_chain_accumulates_entries_and_lww_fields`; `delta_size_and_entries_are_one_record`; `fold_forward_matches_from_scratch_on_layout_chains`; `fold_forward_equivalence_property_layout_class`; `compact_fold_materializes_layout_chain_into_one_put`; `tombstone_shadows_layout_chain`; `layout_delta_onto_indirect_base_fails_loud`; `borrowed_view_encodes_byte_identically`.
`tests/mw_layout_version_tests.rs` — 11 contracts (design-mw-layout-versions §7 `:290-298`): wire round-trip/strip/refusal, bit-15 disjointness, fold divergence/mixing/tie-duplicates, failover fold identity across bset and replay, commit-gate refusal with untouched durable state, compaction-tolerance re-base, pre-stamp first touch, journal entry-count equality + exact 16 B/delta, concurrent chains, Lever A closure.
`tests/dynamic_meta_routing_tests.rs` — `test_derived_width_is_the_slot_id_namespace`; `test_routing_round_trip_at_derived_width`; `test_mint_spread_rotates_across_the_mint_set_and_survives_remount`; `test_created_inos_stable_across_slot_migration_and_remount`; `test_discovery_is_uri_order_independent_at_derived_width`; plus the stamp/bit tests.
`tests/mw_slot_placement_tests.rs` — the client-owned dedicated-slot placement arm (rung 14) and its migration half.
`tests/derivation_sweep_tests.rs` — `inline_max_bytes_default_is_one_page_and_the_bound_is_the_kv_value_cap` (:483); `pack_max_slot_default_is_half_the_chunk_and_the_grain_is_the_lba_law` (:562); `pack_compaction_victim_threshold_is_the_own_block_threshold` (cited `R/defrag.rs:285`).
`tests/staging_wipe_guard_tests.rs`, `staging_lock_handover_tests.rs`, `staging_second_mapper_tests.rs`, `staging_shard_deadlock_tests.rs`, `writer_scoped_staging_tests.rs`, `backend_health_probe_tests.rs`, `ipc_direct_packed_tests.rs`, `kvmap_*_tests.rs` — adjacent surfaces (cache-path consent/wipe, same-mount-point handover, a second mapper, the ledger-lock deadlock, scope stamps, probe TTL caching, PK8, the tree-7 ladder).
`R/layout_wire.rs:720-832` unit tests — `roundtrip_and_strictness`, `canonical_block_map_encoding_is_deterministic`, `apply_refuses_indirect_and_json_and_garbage_bases`, `apply_merges_entries_and_overwrites_non_map_fields`.
`R/cache/nvme.rs:2480-2552` — `shard_plan` table tests. `R/kv/block_map.rs:600-660` — head grammar tests.

**Port contracts to add (red-first, portable unless marked):** encode∘decode = id and `std.testing.fuzz` totality for
`LayoutHeaderWire`+bodies, `MappingWire`, the Δlayout payload, the staging marker and segment record (v4); the two
derivation ties; the pin arithmetic as a MODEL (`refcount = committed + inflight + 1`; weakening: drop the
acquire-from-nonzero refusal ⇒ a terminal pin release races a reservation); the seal CAS single-sealer model; the
staged generation ABA model (weakening: recycled generations); the placement table pick (band ∧ instant health gate);
`pick_mint_volume` 9/10 band + round-robin; the parse choke point's refusal matrix; **Linux-only**: the ring's
mmap/`fallocate`/`madvise` layer, the kill-9 matrix, the mount suites.

---

## 11. QUIRKS (per 0002) — each with the better design the port takes

| # | Reference quirk | Why poor | What the port does |
|---|---|---|---|
| Q-1 | The layout record is bincode of a serde struct: `file_type` a String enum, `Option` tags, u64 lengths, trailing bytes accepted, any map order decoded, JSON sniffed by `'{'` (`R/layout_wire.rs:57-69`, `R/routing.rs:8396-8400`) | 0002 row 2: a serialization-library artifact as an on-disk format; not canonical, not total | §1.7 `LayoutHeaderWire` + per-kind bodies; `layout_kind u8`; exact-length decode; ascending `block` checked; no JSON arm (forward-only among OUR releases) |
| Q-2 | The block mapping is a decorated STRING `[be://]offset[@inc]:rel_off:len`, `damaged:` a prefix, the default slot a bare-offset alias, backend named by id text, incarnation in base-36 (`R/routing.rs:2044-2090, 3731-3839`) | string-typed bindings parsed on every read; an alias that depends on registration order; two parsers (`clean_block_key` vs `parse_block_mapping`) that must agree | `MappingWire` 40 B: `vol_tag` (the durable id decoded once), `offset`, `incarnation`, `rel_off`, `len`, flags `WHOLE_BLOCK`/`DAMAGED`; ONE decode that checks the window law; `vol_tag → allocator` through the router's table, never an alias |
| Q-3 | Layout delta = a second delta class with magic `0x4C31`, its own encoder/decoder and fold arm; the version pair at fixed offsets 3..19 peeked without decode (`R/layout_wire.rs:185-243, 383-586`) | 0002 row 3: two code paths for one algebra; a payload magic as the class discriminator | ONE `Delta(field_mask, fields)` kind, the field table derived from `LayoutHeaderWire` at comptime (like `inode_field_table`); the key's kind + name discriminates; the version pair is in the record header (`layout_version`, `base_version`) so a Put and a Delta share one header and the gate reads a struct field |
| Q-4 | `packed` is `file_type "staged"` + `block_map[0]` with `rel_off ≠ 0`; the reader tells a packed tenant from a ring-resident staged file by probing the ring | a layout class hidden inside another; every consumer branches on string shape | `layout_kind = packed` with `file_id` + one `MappingWire`; `staged` carries an optional durable copy under a flag |
| Q-5 | kvmap head = `kvmap:1[;sweep:K][;gen:N]` text in `block_map_id` (`R/kv/block_map.rs:472-595`) | a grammar inside a string field; canonical-decimal checks hand-rolled | `MAP_KVMAP` flag + `sweep_cursor u32` + `map_gen u64` fields under their presence flags |
| Q-6 | The indirect map blob (`SQFSIMAP` v1/v2, bincode payload, `block_map_id = "indirect:<bk>"`), `block_prefix` part keys (`R/routing.rs:2284-2434, 18534-18536`) | legacy spill the kvmap tree superseded; v1 is the DUR-6 silent-corruption path; `block_prefix` has no minter | not carried: beyond-inline maps are tree 7 only (charter §5.6); `block_prefix` deleted |
| Q-7 | `file_id` = a 36-char uuid string; the staging key = those bytes (`R/routing.rs:19179-19182`) | 36 B of text for 16 B of identity in every layout record, journal entry and ring header | `file_id: [16]u8`; the ring key = the 16 bytes (+ the ino, so the recovery scan can name the inode without a path) |
| Q-8 | Staging generation = `"v3:" ‖ hex(uuid) ‖ "|" …` text plus `:w_{16 hex}[.m{8 hex}]`, two text marker files (`R/cache/nvme.rs:15-64, 84-111`; `R/meta_backend/mod.rs:6701-6713`) | text identities compared by string; a dual-line rebind protocol in prose; the format version in a second file | ONE checksummed binary marker `StagingMarkerWire{magic, version u16, set_uuid[16], set_epoch u64, node_token u64, mount_slot u32, content_version u32, flags(dual rebind: prev_set_epoch), xxh3}` (the identity is derived from the set uuid as the charter says — "generation-stamped to the volume set's uuid-derived identity" — but compared as bytes) |
| Q-9 | Segment record `magic ‖ key_len ‖ val_len ‖ key ‖ pad ‖ value` has NO checksum; recovery trusts sane lengths and resolves duplicates last-wins; a torn value is arbitrated by "the value-shape parse at read time" (`R/tiering/nvme.rs:11-12, 1169-1195`) | charter §5.1 "Checksums everywhere"; a torn staged payload CAN decode as plausible bytes on a passthrough volume | v4 record header `{magic "SQZS", version u16, flags u16, key_len u32, val_len u32, stage_generation u64, fencing_token u64, xxh3 u64 over header+key+value}` = 40 B, 4 KiB-aligned; recovery drops a bad digest loud (counted); the generation in the header makes the ledger seed exact |
| Q-10 | The staged blob value header is BE (`fencing_token u64 BE ‖ original_size u64 BE ‖ path_len u32 BE ‖ file_path`) inside an LE ring (`R/cache/nvme.rs:605-648`); the inode is recovered from a `inode_N` PATH string (`parse_inode_from_path`, `R/routing.rs:640-649`) | mixed endianness in one unit; a path as an identity | Q-9's header carries `ino u64 LE` + `original_size u64 LE`; no path |
| Q-11 | Three knobs read raw (`std::env::var`), one clamped in-process (`R/routing.rs:1018-1028, 1054-1064, 1220-1222, 1263-1273`); two more in `R/cache/mod.rs:132-163`; `inline_max_bytes_for_cap` CLAMPS an over-bound override with a warning (`:393-402`) | 0002 row 14 (a second lenient parser; clamping banned by charter §1.6) | every row through `knobs.intKnob`/`boolKnob`; an inline ceiling above THIS volume's format bound REFUSES the mount naming the bound |
| Q-12 | `SQUEEZEFS_DEFAULT_BLOCK_SIZE` "when format did not record one" (`R/env_knobs.rs:165`); `block_size` an `Arc<AtomicU64>` on the router | a format fact as an env knob; a runtime-mutable word for a frozen geometry | the block size is a superblock/membership-stamp field; the router holds it `const` after open; the knob does not exist |
| Q-13 | `OpenPack.be_id: String`, `base_key: String`, `PackKey{be_id: String, scope}`, the pack ledger a `Mutex<Vec<String>>` (`R/pack.rs:69-104, 205-208`; `R/jobs.rs:4086`) | string identities on a promotion path; a locked vector for a set | `OpenPack{vol_tag u64, base u64, scope u64, …}`; the ledger a lock-free set keyed `(vol_tag, offset)` (an `scc`-class map or a sharded table) |
| Q-14 | `Packer.open: scc::HashMap<PackKey, Arc<ArcSwapOption<OpenPack>>>` + `arc_swap` + `sqz_flight` for ONE open pack per key | three libraries for one slot | one `std.atomic.Value(?*OpenPack)` per `(vol_tag, scope)` row in a fixed table sized by the data-volume count × leased-slot bound; the refill single-flight on `sync/single_flight.zig` |
| Q-15 | The PIN is a RAM refcount increment with no durable witness; `committed` is "GAUGE ONLY"; the terminal-vs-nonterminal verdict is the allocator's RAM count (`R/pack.rs:16-29, 88-94`) | correct on the authority (design §5.4 argues it), but the two words can disagree and nothing checks | keep the design (no durable open-pack record — the argument stands) but make the pin a MODEL-CHECKED core (`refcount = committed + inflight + 1`) with the weakening recorded, and a tripwire `pack_pin_refcount_drift` at seal (`refcount − committed − inflight ≠ 1`) |
| Q-16 | `pick_mint_volume` round-robin counter is a `static META_COUNTER` process global; `get_volume_health` cache a `static HEALTH_CACHE` (`R/meta_backend/mod.rs:2538-2546, 2627-2631`) | process statics shared across mounts/fixtures (0002 row 68's class) | per-`RoutedMetaBackend` fields |
| Q-17 | The placement table's `device_health = (1 − fill) × 1000` and the balance penalty both derive from fill — "health" is fill twice (`R/routing.rs:2858-2865`) | the health term carries no device health (no error rate, no latency, no fence state beyond the boolean gate) | keep the formula (measured G-VL-8 behaviour) but name the term `fill_health`; the fence/latch state stays the absolute gate; a device-health input is an owed row |
| Q-17b | `get_backend_health` divides the device size by a literal `4 * 1024 * 1024` and probes the device size per call (`R/routing.rs:3384-3393`) | a runtime literal where `chunk_size()` exists; a syscall on a diagnostic that `.config` polls | the stats face reads the placement snapshot's row (`backend_placement_weight`, `backend_fill_ratio`); the function does not exist |
| Q-21 | PK4's co-writer BATCH pack (`(owner, home volume)` partition, the `pack_group` publish frame, `PUBLISH_PACK_GROUP_{UNAVAILABLE,SPLIT}`, `SealKind::Batch`, `pack_batch_*`/`pack_cowriter_group_*` gauges — design §5.6 `:291-311`; `R/env_knobs.rs:167`) | the co-writer posture it closes was RETIRED at the symmetric default flip (PR 14: `cowriter.rs` deleted; `R/env_knobs.rs:186`); the surviving reference code has `else if !false` stubs where the arm was (`R/routing.rs:15445, 15620`) | not carried: on the symmetric plane every RW mount is an equal writer with its own allocation grant and slot trees, and the pack SCOPE (`(meta volume, forest slot)`, §4.5) is what keeps a block's references in ONE tree; the authority-side served-publish screen survives as the `served_publish_free_block_refusals` tripwire on the custody-ship path (charter §5.5 "record-level mutations … ONE shipped verb") |
| Q-22 | `SealKind` has three arms but the doc/gauges name a fourth (`pack_blocks_sealed_batch`, design §10) that the enum lacks (`R/pack.rs:191-199` vs `R/fuse_client.rs:5883-5886`) | a documented gauge nothing can tick (the static rail "every row is read" would go red) | `SealKind = {full, dismount, drain}`; the gauge family matches the enum exactly |
| Q-18 | `inline_max_bytes()` per ino recomputes `min over volumes of xattr_value_cap` on every write (`R/routing.rs:8287-8298`) | an O(volumes) scan on the write path for a per-set constant | derive once at open (`derive.Facts` + the set's min node size); per-volume caps differ only across format generations |
| Q-19 | `PromotedInto`, `WriteFileOutcome`, `LayoutFlip`, `PreparedSite` are fine; but `prepare_promotion` contains `else if !false` (`R/routing.rs:15445, 15620`) — a dead co-writer branch left behind | dead code (charter §1.9) | not carried |
| Q-20 | The merge-worker `promote_batch` dedups by cloning `file_id` Strings into a `HashSet<String>` per batch and logs `info!` per promoted file (`R/cache/nvme.rs:2229-2252`) | per-op allocation + a log line per file (the 132 k-line anti-pattern design §10 names) | dedup on the 16-byte id in a stack-allocated set bounded by the batch cap; ONE aggregated line per batch |

When the port meets a further quirk on this surface it adds a row to decision 0002 before diverging.

---

## 12. PORT PLAN — modules, venue, what already exists

| Unit | Port module (proposed) | Portable? | Builds on |
|---|---|---|---|
| `LayoutHeaderWire`, bodies, `MappingWire`, `MapEntryWire`, the Δlayout field table, `encode/decode` total + fuzz | `src/data/layout_wire.zig` | yes | `record.zig`'s `InodeValueWire` + `inode_field_table` pattern (`src/meta/record.zig:104-140`); retires `record.zig:635-…` (`LAYOUT_DELTA_MAGIC`, `LayoutDelta`, the bincode base) per 0002 row 2 |
| Kind-7 block-map values (POINT/RUN/POINT2/RUN2 binary; STRING replaced by a `MappingWire` kind) + `kvmap` head fields | `src/meta/block_map.zig` | yes | `key.zig` kind 7 (13 B key already v4) |
| Inline bound derivation (`derivedInlineMax`, `inlineMaxCeiling(cap)`, `inlineMaxForCap`) + tie test | `src/data/inline_bound.zig` | yes | `derive.Facts`; `node.xattrValueCap(node_size)` |
| Pack law (`packSlotLen`, `packMaxSlot`, `isPackVictim`) + tie test | `src/data/pack_law.zig` | yes | `knobs.optIntKnob(SQUEEZEFS_PACK_MAX_SLOT_BYTES)` |
| The open pack / packer core as a MODEL (reserve `fetch_add`, pin arithmetic, CAS seal, single-flight refill, OQ-1 stop) | `src/data/pack_core.zig` + `src/data/pack_core_model.zig` | yes (model); the DMA/allocator glue Linux-only | `src/model/explorer.zig` (`expectHolds` / `expectFails`), `sync/single_flight.zig` |
| Placement table (`balancePenalty`, `healthEffective`, `refresh`, `pick`) + proptests | `src/data/placement.zig` | yes | `counters.Gauge` for `backend_fill_spread`, ArcSwap-style immutable snapshot (`node_cache.zig`'s `ArcSwap`) |
| Mint rotor / `pickMintVolume` / `pickMintSlot` | `src/meta/inos.zig` (extend) | yes | `routeIno`/`makeGlobalIno` (`inos.zig:303-316`), `geometry.routing_width`; `MINT_SPREAD = STAMP_MAX_CURSORS / 4` tied to the ledger stamp's cursor budget (`kv_journal_ledger.md`) |
| Mapping parse choke point (`parseMapping` over `MappingWire`: window law, `DAMAGED` ⇒ EIO, `packed_mapping_refusals`) | `src/data/mapping.zig` | yes | `core/errors.zig` (EIO terminal class) |
| Staging marker v4 + segment record v4 codecs, `shardPlan`, the ledger/generation ABA model | `src/staging/format.zig`, `src/staging/ledger_model.zig` | yes | `core/checksum.zig` xxh3-64 |
| The mmap ring (`mmap`, `fallocate(PUNCH_HOLE)`, `madvise`, `msync`), recovery scan, the merge worker, the dismount pass | `src/staging/ring.zig`, `src/staging/promote.zig` | **Linux-only** (`builtin.os.tag == .linux` in `squeezefs.zig`) | `io/uring_fs` for marker I/O; `exec` lanes (`sqz-meta` class) for the merge worker; `thread_name` |
| Promotion primitive (`prepare`/`commit`), the four landing arms, `commitPromotion` under the 3.5 stripe, RES-1 | `src/data/promote.zig` | commit half portable against a fake backend; DMA half Linux-only | `tx.KvTx` (`+ref/−ref` staged in the same tx — `block_refs` ops), `stripe` locks |
| fsck C12 detection over the census | `src/fsck/c12.zig` (rung 8) | yes | the census `MappingRef` vector |
| Compaction D1 pack arm (`repackWindow`, occupancy, the plan) | `src/jobs/defrag_pack.zig` (rung 8) | plan portable; mover Linux-only | `move_one` discipline |

Gauges to register (`src/core/counters.zig` rows; every name §8 lists is a registry row read by its module — the
static rail): `ShardedCounter` for every per-op count (`packed_reads`, `layout_promoted_*`, `pack_*_frees`, …),
`Gauge` for the levels (`pack_open_blocks`, `current_staged_write_bytes`, `backend_fill_spread`), `Tripwire` for
`packed_mapping_refusals`, `pack_release_untracked_noops`, `served_publish_free_block_refusals`,
`pack_tenant_overlap_findings`, `block_key_incarnation_refusals`, `meta_kv_block_refs_drift`, `read_stale_bound_mapping`,
`pack_pin_refcount_drift` (new, Q-15). Knob rows (§7) enter `src/core/knobs.zig` with their classes; the three
raw-read rows and the two `cache/mod.rs` rows become ordinary registry rows; `SQUEEZEFS_DEFAULT_BLOCK_SIZE` is not
registered (Q-12).

Evidence note owed at rung 4 (`docs/evidence/`): the two derivation ties; the pack-core model's weakenings (drop
acquire-from-nonzero ⇒ a terminal pin release races a reservation; drop the CAS seal ⇒ two sealers release the pin
twice; drop the generation gate ⇒ the ABA class of `staged_generation_aba_tests`); the layout codec's totality sweep
over lengths 0..(32 + 4 + 4096 + margin); and the measured bars this spec carries (§9) restated as the bars to meet
or beat on the venue: `staged_payload_lost_reads = 0` after a clean unmount, `packed + block + inline ≡ promotions`,
`96,000 → 375` blocks on the fsync-promote row and `20,000 → 148` on the dismount row at `CHUNK/2`.
