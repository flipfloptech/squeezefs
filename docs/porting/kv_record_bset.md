# PORTING SPEC — MetaLV record / bset layer ("PR K1")

> **v4 decisions (format v4, 2026-10-05; decision 0003 WP-F; 0002 rows 35–39, 47).** The bytes below are
> the reference's v3 — evidence; where a "v4:" row exists it is what `squeezefs-zig` writes. §10's
> "the Zig port must match the BYTES" is retired (0002 §2): the Zig codecs are pinned by their own
> encode∘decode = id, totality fuzzers and the Zig-generated FIND-VS-A fixture
> (`tests/fixtures/findvsa_pred_bset.v4.bin`, 254,932 B, 3,515 records).
>
> **v4: keys — ONE layout, the kind byte on every key** (`key.zig`): inode `ino u64 BE ‖ 01` (9 B);
> dentry `parent u64 BE ‖ 02 ‖ hash54_coll u64 BE` (17 B); xattr `ino ‖ 03 ‖ hash56_coll` (17 B); block map
> `owner ‖ 07 ‖ block_index u32 BE` (13 B); block refs `06 ‖ vol_tag u64 ‖ block_idx u64 ‖ owner_ino u64 ‖
> block_index u32` (29 B; prefix 17 B = the refcount range probe); control (kind 8) string keys. The
> ino-major kinds carry the kind at offset 8, refs prepend it (`routeOff`); a record's slot is `key_ino
> >> 40` (`FOREST_SLOT_MAX = 65536`; the ladder's soundness `(FOREST_SLOT_MAX + 1) << 40 < 0x02 << 56` is a
> comptime assertion). `checkSlotTreeKey(kind, key)` is the one agreement check (admission, mount fold,
> routing). The kindless legacy layout, `forestKey/splitForestKey`, `KeyFraming` are gone.
> `KEY_SPACE_MAX = 0xFF × 32`; `KEY_BUF_CAP = 36`.
>
> **v4: record header** (16 B, LE): `key_len u16 ‖ kind u8 {Put 0, Delete 1, Delta 2} ‖ flags u8 = 0
> (checked) ‖ val_len u32 ‖ seq u64`; `RECORD_MIN_LEN = 17`.
>
> **v4: inode value** = `InodeValueWire` extern struct, 68 B LE, size- and offset-asserted: `mode u32 @0 ‖
> uid u32 @4 ‖ gid u32 @8 ‖ nlink u32 @12 ‖ flags u32 @16 ‖ rdev u64 @20 ‖ size u64 @28 ‖ atime i64 @36 ‖
> mtime i64 @44 ‖ ctime i64 @52 ‖ flags2 u32 @60 ‖ reserved[4] = 0 @64`. Any other length refuses (the
> LEB128 version tag and its non-canonical `0x81 0x00` spelling are gone). The Δinode field table
> (`inode_field_table`) is DERIVED from the struct's field names at comptime: mask bits in field order
> (mode 0 … ctime 9, flags2 10; `DELTA_MASK_ALL = 0x7FF`, `INODE_DELTA_MAX_LEN = 66`) — ONE source for the
> Put and the Delta (0002 row 3's one Delta kind at rung 4 builds on it).
>
> **v4: bset header** (32 B): `magic "BSET" u32 ‖ version u16 = 1 ‖ reserved u16 = 0 (checked) ‖
> record_count u32 ‖ data_len u32 ‖ journal_seq_horizon u64 ‖ xxh3 u64` (ASCII magic in disk order; the
> reference's numeric `54 45 53 42` is gone). **v4: bset frame** (40 B, the ONE frame): `magic "KBSF" u32 ‖
> version u16 = 1 ‖ reserved u16 = 0 (checked) ‖ node_seq_at_write u64 ‖ padded_len u32 ‖ bset_len u32 ‖
> appender_id u32 ‖ g u32 ‖ xxh3 u64` over `[0..32)`; the stamp is on every frame and `(0, 0)` is the
> manager's / the one-member forest's.

Reference (read-only): `/Users/joberdorf/Source/squeezefs` — `R=src/meta_backend/kv`.
Target: `/Users/joberdorf/Source/squeezefs-zig`. Charter laws pinned: §5.1 "Keys",
"Record kinds and ONE fold algebra", "Checksums everywhere", bset `{…}` framing.

Every byte below was read off the Rust encoders/decoders, not the docs. Where the
charter/design-doc prose and the as-built bytes differ, **§10 lists the delta** — the
Zig port must match the BYTES (volumes already exist).

Conventions: `LE` = little-endian, `BE` = big-endian. `[a..b)` = byte range. All
multi-byte **key** fields are BE (memcmp order == logical order); all **value** and
**framing** fields are LE (no ordering requirement). `xxh3` = XXH3-64 (xxhash-rust
`xxh3_64` / `Xxh3` streaming), digest stored LE.

---

## 1. KEY ENCODING

### 1.1 Kind byte == historical tree id (`record.rs:20-69`)

| const | value | meaning |
|---|---|---|
| `KIND_INTERIOR` | 0 | interior record of a slot tree (legal iff level ≥ 1); also node-header `tree_id` of every slot-tree node |
| `TREE_INODES` | 1 | |
| `TREE_DENTRIES` | 2 | |
| `TREE_XATTRS` | 3 | |
| `TREE_ALLOC_RESERVED` | 4 | reserved (snapshots) |
| `TREE_BACKPTR_RESERVED` | 5 | reserved |
| `TREE_BLOCK_REFS` | 6 | durable block refs (incompat bit 8) |
| `TREE_BLOCK_MAP` | 7 | block-map tree (incompat bit 16) |
| `TREE_CONTROL` | 8 | tree 0 of a forest volume (incompat bit 17); NEVER a slot-tree kind |
| `TREE_SHARED_INDEX` | 9 | reserved; own tree, never a slot-tree kind |
| `TREE_ID_MAX` | 9 | = `TREE_SHARED_INDEX`; journal tag nibble bounds it at 15 |

`is_slot_tree_kind(k) = k ∈ {1,2,3,6,7}` (`record.rs:164-169`). This set IS bit 17's
meaning: a new slot-tree kind needs a NEW incompat bit.

Journal tag byte (`journal.rs:464-483`): `tag = tree_id | (level << 4)`; `untag(t) =
(t & 0x0F, t >> 4)`. Admissible iff `tree_id ∈ 1..=9` at any level, or `tree_id == 0 ∧
level ≥ 1`.

### 1.2 Legacy (per-kind) keys — the shape the per-kind codecs read (`record.rs:428-524`)

```
inode   (8 B):  [0..8) ino u64 BE                                     INODE_KEY_LEN=8
dentry (16 B):  [0..8) parent_ino u64 BE ‖ [8..16) suffix u64 BE      DENTRY_KEY_LEN=16
                suffix = (hash54 << 8) | coll_seq     (62 bits; top 2 bits structurally 0)
xattr  (16 B):  [0..8) ino u64 BE ‖ [8..16) ((hash56 << 8) | coll_seq) u64 BE   XATTR_KEY_LEN=16
                (top 0 bits free — 56+8 = 64; no structural-zero check on decode)
blockmap(12 B): [0..8) owner_ino u64 BE ‖ [8..12) block_index u32 BE  BLOCK_MAP_KEY_LEN=12
                block_index == u32::MAX is RESERVED: refused at encode AND decode
                (block_map.rs:141-173); it appears only as a range END bound (index_range_from)
blockref(28 B): [0..8) vol_tag u64 BE ‖ [8..16) block_idx u64 BE ‖ [16..24) owner_ino u64 BE
                ‖ [24..28) block_index u32 BE                          BLOCK_REF_KEY_LEN=28
                BLOCK_REF_PREFIX_LEN=16 (refcount prefix), BLOCK_REF_OWNER_OFF=16
                block_index == u32::MAX (BLOCK_INDEX_MAP_BLOB) = "this ref is the indirect map blob"
```

Decoders refuse wrong lengths. `decode_dentry_key` additionally refuses `suffix >> 62 != 0`
(`record.rs:496-500`). `HASH54_MAX = 2^54−1`, `HASH56_MAX = 2^56−1`.

### 1.3 Forest keys — kind byte inserted (`record.rs:71-146, 188-219, 397-409`)

Under bit 17 every content record lives in ONE mixed-kind tree per routing slot. The
forest key is a pure function of `(kind, legacy)`:

```
ino-major family (kind ∈ {1,2,3,7}; first byte ≤ 0x01 because (s+1)<<40 ≤ 2^56):
    forest = legacy[0..8] ‖ kind ‖ legacy[8..]
    inode 9 B · dentry 17 B · xattr 17 B · block-map 13 B      (FOREST_*_KEY_LEN = legacy+1)
by-block family (kind == 6; first byte = 0x06 ≥ 0x02, sorts after every ino-major key):
    forest = 0x06 ‖ legacy                                      29 B; owner ino at offset 17
                                                                (FOREST_BLOCK_REF_OWNER_OFF = 1+16)
```

Both transforms preserve memcmp order among keys of one kind. `forest_key(kind, legacy)`
refuses: kind ∉ slot-tree set; `legacy.len() != legacy_key_len(kind)`; routing ino's slot
> `FOREST_SLOT_MAX`. `split_forest_key` is the exact inverse.

`forest_key_kind(key)` (`record.rs:248-300`) decodes the kind WITHOUT materializing:
- empty ⇒ Corrupt; `key[0] == 0x06` ⇒ kind 6; `key[0] > 0x01` ⇒ Corrupt ("neither ino-major
  nor refs prefix"); else `kind = key[8]` (missing ⇒ Corrupt); `key[8] == 0x06` ⇒ Corrupt
  (refs are by-block, never ino-major); `!is_slot_tree_kind(kind)` ⇒ Corrupt;
  `key.len() != legacy_key_len(kind)+1` ⇒ Corrupt; routing-ino slot > `FOREST_SLOT_MAX` ⇒ Corrupt.
- `forest_route_off(kind) = 17 if kind==6 else 0`.

`classify_refused_key` (`record.rs:344-394`) → `RawKeyDefect::{MalformedKnownKind{kind,want,got},
UnknownKind{kind}, Truncated{got}, SlotOutOfNamespace{kind,slot}}`; fsck's raw C1 repair
deletes ONLY `MalformedKnownKind` and `SlotOutOfNamespace`.

### 1.4 Slot derivation & guest keyspace (`record.rs:94-133`, `meta_backend/mod.rs:37-81`)

```
GUEST_NS_SHIFT = 40;  GUEST_NS_BASE = 1<<40      native locals ∈ [0, 2^40)
guest_local_ino(slot:u16, local) = ((slot as u64 + 1) << 40) | local     (local < 2^40)
split_guest_local(l) = None if l < 2^40 else Some(((l>>40)−1) as u16, l & (2^40−1))
ForestSlot = u32;  NATIVE_FOREST_SLOT = 0;  guest_forest_slot(s:u16) = s+1
FOREST_SLOT_MAX = 65536  (= guest_forest_slot(u16::MAX))
forest_slot_of_ino(key_ino) = (key_ino >> 40) as u32         (unchecked)
checked_forest_slot refuses slot > FOREST_SLOT_MAX
forest_key_slot(key) = forest_slot_of_ino(BE u64 at forest_route_off(kind))
legacy_key_slot(kind, legacy) = same, from legacy offsets (16 for refs, 0 otherwise)
```

### 1.5 Interior records (`tree.rs:67,147-167`, `forest.rs:909-937`)

- Interior separator key = child's inclusive `max_key`; rightmost child's is
  `KEY_SPACE_MAX = [0xFF; 32]`. Leaf keys must be non-empty and `< KEY_SPACE_MAX`;
  interior separators non-empty and `≤ KEY_SPACE_MAX`.
- Interior VALUE (16 B LE): `[0..8) child_addr u64 LE ‖ [8..16) child_node_seq u64 LE`.
  `decode_interior_value` refuses len ≠ 16.
- Journal key of a SLOT tree's interior pointer record: `slot u32 BE ‖ separator`
  (`INTERIOR_JOURNAL_SLOT_LEN = 4`); a flat tree journals the bare separator. The
  record's tag is `tag_for(KIND_INTERIOR, level)`. `split_interior_journal_key` refuses
  `len ≤ 4`.
- Key successor (sibling partition): `key_successor(k) = k ‖ 0x00` (`node.rs:712-717`).

### 1.6 Seeded hashes (`record.rs:533-542`)

```
hash54(name, seed) = xxh3_64_with_seed(name, seed) >> 10
hash56(name, seed) = xxh3_64_with_seed(name, seed) >> 8
```
`seed` = superblock `hash_seed` (random at format). `xxh3_64_with_seed` is XXH3-64 with the
standard seed parameter (xxhash-rust semantics == upstream xxHash `XXH3_64bits_withSeed`).

### 1.7 `coll_seq` probing (`record.rs:552-577`)

`first_free_coll_seq(occupied) -> ?u8`: 256-bit bitmap over occupied; lowest clear bit
(holes refilled; input order irrelevant); all 256 set ⇒ `None`.
`assign_dentry_coll_seq` = same, but `None` ⇒ `KvError::DentryChainOverflow` AND
`META_KV_DENTRY_COLLISION_OVERFLOWS += 1`. Callers first probe the chain by FULL-NAME
comparison (existing name = replace, not a new slot).

### 1.8 Readdir cookie (`record.rs:1634-1684`)

```
READDIR_OFFSET_DOT=1, READDIR_OFFSET_DOTDOT=2, READDIR_COOKIE_BIAS=3
dentry_key_suffix(h54, coll) = ((h54 & HASH54_MAX) << 8) | coll
encode(h54, coll) = 3 + dentry_key_suffix(h54, coll)       max = 2^62 + 2; sign bit always clear
decode(c): 0 ⇒ Start; 1 ⇒ AfterDot; 2 ⇒ AfterDotDot;
           else p = c−3; p > dentry_key_suffix(HASH54_MAX, 0xFF) ⇒ InvalidReaddirCookie(c);
           else AfterEntry{hash54: p>>8, coll_seq: p & 0xFF}  (resume: entries with suffix STRICTLY > p)
```
The cookie payload IS the dentry key's `[8..16)` word — stable across concurrent inserts.

---

## 2. VALUE ENCODINGS

### 2.1 Inode value — 57 B (`record.rs:681-740`)

```
[0]      varint(INODE_VALUE_VERSION=1)   LEB128; decode refuses version ≠ 1 (multi-byte 0x81 0x01 = 129 refused too)
[1..5)   mode  u32 LE
[5..9)   uid   u32 LE
[9..13)  gid   u32 LE
[13..17) nlink u32 LE
[17..21) flags u32 LE
[21..25) rdev  u32 LE     ← the wire word historically named `flags2`; now mknod rdev (new_encode_dev u32); 0 for non-devices
[25..33) size  u64 LE
[33..41) atime u64 LE     ← ns; stored as u64 bit pattern (charter says i64 ns — same bytes)
[41..49) mtime u64 LE
[49..57) ctime u64 LE
```
Total **57 B** = 1 + 6×4 + 4×8 (pinned by `inode_value_roundtrips_with_leading_version_varint`).
Decode: varint → fixed fields via bounds-checked `Reader` → `finish()` refuses trailing bytes.

### 2.2 Dentry value (`record.rs:746-793`)

```
[0..8)  child_ino u64 LE
[8]     file_type u8 (DT_*)
[9]     name_len  u8          > 255 ⇒ KvError::NameTooLong{len} at encode
[10..)  name bytes (exactly name_len)
```
Decode refuses truncation, lying `name_len`, trailing bytes.

### 2.3 Xattr value (`record.rs:795-834`)

```
[0]       name_len u8        > 255 ⇒ NameTooLong
[1..1+n)  name
[1+n..)   value = REMAINDER (record framing carries the total length; no trailing-byte check)
```
Caps (`node.rs:254-276`): `RECORD_VALUE_CAP_CEILING = 65_536`; `xattr_value_cap(ns) =
min(65536, ns/4)` (user-facing); `XATTR_RECORD_ENVELOPE_MAX = 256`; `record_value_cap(ns) =
xattr_value_cap(ns) + 256` = the node layer's cap on ONE ENCODED record value, enforced in
`encode_bset_frame` (`KvError::ValueTooLarge{len,cap}`). Not enforced in record.rs/bset.rs.

### 2.4 Block-ref value — 4 B (`block_refs.rs:148-172, 236-246, 519-548`)

```
[0]     version u8 = BLOCK_REF_VALUE_VERSION = 1   (≠1 ⇒ Corrupt)
[1]     flags u8:  bit0 BLOCK_REF_FLAG_MAP_BLOB (set iff key.block_index == u32::MAX)
                   bit1 BLOCK_REF_FLAG_SHARED
                   any other bit ⇒ Corrupt
[2..4)  reserved u16 LE = 0  (≠0 ⇒ Corrupt)
```
`volume_tag(id)`: `"vol-"` + exactly 16 hex ⇒ parsed u64 verbatim; else `xxh3_64(id bytes)`.

### 2.5 Block-map value (`block_map.rs:76-129, 252-435`)

```
[0] version u8 = 1          [1] kind u8          (len < 2 ⇒ Corrupt; version ≠ 1 ⇒ Corrupt)
POINT  (kind 1, 18 B): [2..10) vol_tag u64 LE ‖ [10..18) offset u64 LE
STRING (kind 2, ≥2 B): [2..)  decorated block-key bytes VERBATIM
RUN    (kind 3, 22 B): [2..10) vol_tag ‖ [10..18) start_offset ‖ [18..22) len u32 LE
POINT2 (kind 4, 26 B): POINT ‖ [18..26) incarnation u64 LE      (incarnation == 0 ⇒ Corrupt)
RUN2   (kind 5, 30 B): RUN   ‖ [22..30) start_incarnation u64 LE
                       (start==0 ∨ start.checked_add(len−1) overflows ⇒ Corrupt)
```
`RUN_LEN_MAX = 4096`; `validate_run_len`: `len ∉ 2..=4096 ⇒ Corrupt` (a 1-run is a point).
Exact lengths enforced per kind; unknown kind ⇒ Corrupt. `run_len()` = len for RUN/RUN2 else 1.
Head sentinel string (layout record, not a KV value): `kvmap:1[;sweep:K][;gen:N]`, canonical
decimals only, segments in encoder order only, `gen:0` refused.

### 2.6 Inode delta payload — the Δtime record (`record.rs:841-1007`)

```
[0..2)  mask u16 LE        unknown bits (mask & !0x3FF) ⇒ Corrupt
then, in THIS canonical order, only the masked fields:
  bit0 DELTA_MODE  mode u32 | bit1 DELTA_UID  uid u32 | bit2 DELTA_GID gid u32 | bit3 DELTA_NLINK nlink u32
  bit4 DELTA_FLAGS flags u32 | bit5 DELTA_RDEV rdev u32 | bit6 DELTA_SIZE size u64
  bit7 DELTA_ATIME atime u64 | bit8 DELTA_MTIME mtime u64 | bit9 DELTA_CTIME ctime u64
DELTA_MASK_ALL = 0x3FF;  DELTA_TIMES = MTIME|CTIME = 0x300
```
`InodeDelta::times(m,c)` → 18 B (`0x00 0x03 ‖ m ‖ c`). `InodeDelta::ctime(c)` → 10 B (`0x00 0x02 ‖ c`).
Full mask → 2+24+32 = 58 B. `apply(base)`: field OVERWRITE of masked fields (idempotent).
Decode refuses truncation and trailing bytes.

### 2.7 Layout delta payload — second delta class (`layout_wire.rs:184-243, 436-586`)

Discriminator: `is_layout_delta(p) = p.len() ≥ 2 ∧ u16 LE p[0..2] == LAYOUT_DELTA_MAGIC
= 0x4C31 ("L1")`. 0x4C31 has bits above 0x3FF so an inode-delta decoder refuses it loud.

```
[0..2) magic u16 LE = 0x4C31
[2]    flags u8: bit0 BLOCK_MAP_ID, bit1 BLOCK_PREFIX, bit2 FILE_ID, bit3 DATA_KEY, bit4 VERSIONED; others ⇒ Malformed
[3..19) iff bit4: base_version u64 LE ‖ version u64 LE   (version == 0 under the flag ⇒ Malformed)
       file_type_len u8 ‖ file_type (UTF-8) ‖ size u64 LE
       [block_map_id: len u16 LE ‖ bytes]  iff bit0   (UTF-8)
       [block_prefix: len u16 ‖ bytes]     iff bit1
       [file_id: len u16 ‖ bytes]          iff bit2
       [data_key: len u16 ‖ bytes]         iff bit3   (raw bytes)
       entry_count u32 LE ‖ entries × { block u32 LE ‖ key_len u16 LE ‖ key (UTF-8) }
trailing bytes ⇒ Malformed. Decoder presizes entries with min(n, 4096) (never trusts n).
```
`layout_delta_versions(p)`: `Some((base, ver))` iff len ≥ 19 ∧ magic ∧ bit4 ∧ ver ≠ 0.
`restamp_delta_versions(p, b, v)`: v==0 ⇒ copy; versioned wire ⇒ patch `[3..19)` in place;
else decode → set_versions → encode.
Base for this class = `XattrValue{name, value}` whose `value` is a **bincode-serialized
`LayoutMetadata`** `{file_type: String, size: u64, block_map_id: Option<String>, block_prefix:
Option<String>, file_id: Option<String>, data_key: Option<Vec<u8>>, block_map:
Option<HashMap<u32,String>>}` with the map serialized in ASCENDING block-index order (canonical).
`decode_base_layout` refuses: first byte `'{'` (legacy JSON), non-bincode, `block_map_id`
starting `"indirect:"` or `"kvmap:"`. `apply_to`: if entries non-empty insert each `(b→k)` into
the map (creating it); then OVERWRITE file_type/size/block_map_id/block_prefix/file_id/data_key.
**Port note:** the Zig fold must reproduce bincode's default (fixint, LE, u64 lengths, Option
tag u8) encoding of this struct byte-for-byte — pin it with fixtures from the Rust side.

---

## 3. RECORD KINDS, FRAMING, THE ONE FOLD ALGEBRA

### 3.1 Kinds & framing (`record.rs:1013-1173`)

`RecordKind: Put=1, Delta=2, Delete=3` (0 is never valid). `RECORD_HEADER_LEN = 15`:

```
[0..2)   key_len u16 LE      (0 ⇒ Corrupt "empty key")
[2]      kind u8             (∉{1,2,3} ⇒ Corrupt)
[3..11)  seq u64 LE
[11..15) val_len u32 LE      (kind==Delete ∧ val_len≠0 ⇒ Corrupt)
[15..15+key_len)  key
[..+val_len)      value
```
**There is NO flags field** (charter prose lists one; the bytes do not). `encoded_len = 15 +
key.len + value.len`. Decode: header ≥ 15 B; `total = 15 + key_len + val_len` via checked_add;
`buf.len() < total ⇒ Corrupt`; returns `(RecordRef{key,seq,kind,value}, total)`.
Journal entry payload = repeated `tag u8 ‖ <this framing>` (`journal.rs:434-446`).

Owned `Record{key: Vec<u8>, seq: u64, kind, value: Vec<u8>}`; borrowed `RecordRef<'a>` — the
fold/bset layers consume `RecordRef`. `Record::put/delta/delete` constructors; `delta` encodes
an `InodeDelta`.

### 3.2 Fold outcome types (`record.rs:1181-1204, 1477-1520`)

```
Folded<'a> = Put{value: Cow<[u8]>, seq} | Tombstone{seq} | Absent
   live_value(): Some(bytes) for Put, None otherwise
FoldedHead = Live{value: Bytes, decoded: ?InodeValue, materialized: bool} | Tombstone | Absent
   owned_bytes(): value.len() iff Live∧materialized else 0
```

### 3.3 `fold_newest_first` — THE algebra (`record.rs:1342-1387`), exact pseudocode

```
fn fold_newest_first(records: iterator<RecordRef>, newest-seq-first) -> Folded:
    deltas = []                      # collected newest-first
    for r in records:                # debug_assert prev.seq >= r.seq
        match r.kind:
          Delta  => deltas.push(r)
          Delete => return Tombstone{seq: r.seq}            # collected deltas DISCARDED, NOT counted
          Put    => if deltas.empty(): return Put{value: Borrowed(r.value), seq: r.seq}   # zero-copy
                    else: return Put{value: Owned(fold_deltas_onto_put(r.value, deltas)),
                                     seq: deltas[0].seq}     # seq = NEWEST delta's seq
    if !deltas.empty(): META_KV_DELTA_ORPHANS += deltas.len()   # Δ-without-base, one per orphan record
    return Absent

fn fold_deltas_onto_put(base: bytes, deltas: [RecordRef] newest-first) -> bytes:
    META_KV_FOLD_RECORD_DECODES += 1 + deltas.len()
    if is_layout_delta(deltas[0].value):
        x = XattrValue::decode(base);  layout = decode_base_layout(x.value)   # refusals ⇒ Corrupt
        prev_version = None; saw_unversioned = false; last_seq = None
        for d in deltas.reverse():                        # ASCENDING seq
            if !is_layout_delta(d.value): Corrupt("mixed delta classes")
            dec = LayoutDelta::decode(d.value)
            duplicate = (last_seq == Some(d.seq)); last_seq = Some(d.seq)
            if !duplicate:                                 # exact-seq duplicates skip the link law
                if dec.version != 0:
                    if saw_unversioned: Corrupt("versioned above unversioned")
                    want = prev_version.unwrap_or(0)
                    if dec.base_version != want: Corrupt("divergent layout-delta chain … CHAIN: <tape>")
                    prev_version = Some(dec.version)
                else:
                    if prev_version.is_some(): Corrupt("unversioned above versioned")
                    saw_unversioned = true
            dec.apply_to(layout)
        META_KV_LAYOUT_DELTA_FOLDS += deltas.len()
        return XattrValue::encode_parts(x.name, encode_layout(layout))   # canonical re-encode
    else:
        base_iv = InodeValue::decode(base)                # garbage base ⇒ Corrupt (only when a delta forces it)
        for d in deltas.reverse():                        # ASCENDING seq; newest applied last wins
            if is_layout_delta(d.value): Corrupt("mixed delta classes")
            InodeDelta::decode(d.value).apply(base_iv)
        return base_iv.encode()
```
Input contract: exact `seq` ties across sources keep input order and are legal only for
identical-effect records (a node bset overlapping the journal replay window).

### 3.4 `compact_fold` (`record.rs:1418-1471`)

```
fn compact_fold(group: [RecordRef] one key, newest-first, durable_tail: u64) -> [Record] (key,seq)-ascending:
    if group.empty(): return []
    first = group[0]
    # §6.2 item-9 lineage rule: newest record is a VERSIONED layout link ⇒ fold BELOW it, retain it restamped to base 0
    if first.kind == Delta ∧ is_layout_delta(first.value):
        if Some((_, version)) = layout_delta_versions(first.value):
            if Put{value, seq} = fold_newest_first(group[1..]):
                return [ Record::put(key, seq, value),
                         Record{key, seq: first.seq, kind: Delta, value: restamp_delta_versions(first.value, 0, version)} ]
            # prefix folded to Tombstone/Absent ⇒ fall through to the plain algebra
    match fold_newest_first(group):
      Put{value, seq}                      => [Record::put(key, seq, value)]
      Tombstone{seq} if seq >= durable_tail => [Record::delete(key, seq)]     # inside replay window: SURVIVES
      Tombstone{..} | Absent               => []                             # elided (seq < tail) / orphan
```
**Elision law: `seq < durable_tail` ⇒ drop; `seq == durable_tail` ⇒ KEEP.** The comparison
crosses record-seq vs ring-position domains on purpose and is conservative under the seq-space
law (`seq = position + offset ≥ position`).

### 3.5 `fold_forward` — the incremental step (`record.rs:1559-1628`)

Theorem: `prev == fold(history) ∧ rec.seq > every seq in history ⇒ fold_forward(prev, rec) == fold(rec ∪ history)`.
```
fn fold_forward(prev: FoldedHead, kind, value: Bytes) -> FoldedHead:
    Put    => Live{value: value.clone(), decoded: None, materialized: false}
    Delete => Tombstone
    Delta  => match prev:
        Live{v, decoded} =>
            if is_layout_delta(value):
                META_KV_FOLD_RECORD_DECODES += 2
                x = XattrValue::decode(v); layout = decode_base_layout(x.value)
                LayoutDelta::decode(value).apply_to(layout);  META_KV_LAYOUT_DELTA_FOLDS += 1
                Live{value: XattrValue::encode_parts(x.name, encode_layout(layout)), decoded: None, materialized: true}
            else:
                base = decoded ?? { META_KV_FOLD_RECORD_DECODES += 1; InodeValue::decode(v) }
                META_KV_FOLD_RECORD_DECODES += 1;  InodeDelta::decode(value).apply(base)
                Live{value: base.encode(), decoded: Some(base), materialized: true}
        Tombstone => Tombstone
        Absent    => { META_KV_DELTA_ORPHANS += 1; Absent }
```
Deliberately version-BLIND (no chain-link law); the commit gate keeps divergent links off the
durable chain, and every from-scratch fold runs the strict law.

### 3.6 Every site that must share the algebra byte-identically

| site | how the reference shares it |
|---|---|
| point lookup across bsets | `bset::lookup` → `fold_newest_first` (`bset.rs:401-412`) |
| node-level lookup | `LoadedNode::lookup` → `bset::lookup` over `bset_views_newest_first()` (`node.rs:653-667`) |
| cached-node lookup (overlay tail ∪ stable ∪ base) | `node_cache.rs:864-941`: head serve (D7.a) → memo serve (D7.b) → `fold_newest_first(gather)`; gather = tail-run reversed ‖ stable-run reversed ‖ base group (newest-first) |
| overlay apply (commit AND mount replay) | `node_cache.rs:1860-1906`: head = `fold_forward(prev_head_or_fold_below, rec)`; Put/Delete heads self-define; stale heads above an out-of-order insert are invalidated (reads fall back to from-scratch) |
| bset n-way merge → compaction | `bset::compact` → `merge` → group by key → `compact_fold` (`bset.rs:421-434`) |
| journal replay | records re-applied through the SAME overlay `apply` path with ORIGINAL seqs; reads fold with the same function |

Sharing mechanism: **one free function** `fold_newest_first` (+ `compact_fold`, `fold_forward`
in the same file, with `fold_forward ≡ fold_newest_first` pinned by proptest). No trait.
The Zig port should likewise expose exactly one fold function and have every caller route
through it; `fold_forward` is an optimization that must be property-tested against it.

### 3.7 Property tests (what, and how inputs are generated)

- `fold_forward_matches_fold_newest_first` (`record.rs:2447-2514`, proptest): steps ∈ vec(1..24)
  of `prop_oneof![Put(iv(any u64)), Delta(times(any,any)), Delta(ctime(any)), Delete]`, one key,
  seq = index+1. Asserts head bytes == from-scratch bytes; Tombstone↔Tombstone; Absent↔Absent;
  and `decoded.encode() == value`.
- `replay_reproduces_ram_is_one_fold_theorem` (`record.rs:2558-2601`): ChaCha8 seed 0x5EED, 50
  rounds × 120 seqs, keys `inode_key(1..=6)`, op mix 50 % Put / 30 % Δtimes / 20 % Delete; for
  tail ∈ {0, rand(1..=120), 121}: `live(compact(history, tail) ∪ {r: r.seq ≥ tail}) == live(history)`
  AND `compact(compact(history,tail),tail) == compact(history,tail)` (fixed point).
- `bset_nway_merge_matches_the_reference_order_under_long_same_key_runs` (`bset.rs:872-916`):
  xorshift64 seed 0x9E3779B97F4A7C15, 400 trials, 1–6 sources, 1–5 keys/source, runs 1–9;
  `merge(views) == sort_by(key asc, seq desc, source asc)`.
- `fold_forward_equivalence_property_layout_class` (`tests/layout_delta_fold_tests.rs:190-256`):
  256 proptest histories of layout Put / unversioned layout delta / Delete; incremental ≡ from-scratch.
- `fold_head_and_memo_equal_from_scratch_fold` (`tests/kv_fold_slimming_tests.rs:661-786`):
  48 cases of ≤48 ops over 4 keys {Put, DeltaTimes, DeltaCtime, Delete, Freeze, Remove{depth}}
  on a real file-backed node; first read (head/cold) AND repeat read (memo) ≡ `fold_newest_first`
  over surviving mirrored records. **Needs device (io_uring file I/O).**

---

## 4. BSET FORMAT

### 4.1 Bset image (`bset.rs:29-80`) — `BSET_HEADER_LEN = 32`

```
[0..4)   magic u32 LE = BSET_MAGIC = 0x4253_4554  (bytes on disk: 54 45 53 42 — "TESB"; the u32 reads "BSET")
[4..6)   version u16 LE = BSET_VERSION = 1
[6..8)   reserved u16 LE = 0           (≠0 ⇒ Corrupt)
[8..12)  record_count u32 LE
[12..16) data_len u32 LE               (buffer MUST be exactly 32 + data_len)
[16..24) journal_seq_horizon u64 LE    (newest journal seq this bset covers)
[24..32) checksum u64 LE = xxh3 over [0..24) ‖ 8×0x00 ‖ [32..)
[32..)   records, each in §3.1 framing, strictly (key, seq) ASCENDING
```
**No `node_seq_at_write` and no `len (4 KiB multiple)` live here** — those are the node-level
FRAME (§4.3). `build_bset(records, horizon)` refuses: empty input; any adjacent pair not
strictly `(key, seq)` ascending (duplicates included); `data_len > u32::MAX`.

### 4.2 `BsetView::parse` (`bset.rs:110-219`) — verify ONCE, then infallible slicing

Order of gates: len < 32 ⇒ Corrupt → magic → version → reserved → `buf.len() != 32+data_len`
⇒ Corrupt → **checksum** (`ChecksumMismatch{stored, computed}`) → **count bound**:
`min_record_len = 15+1 = 16; record_count > data_len/16 ⇒ Corrupt` (the record_count is a
CLAIM, never an allocation authority — TEST-4 find: a 32 B image claiming u32::MAX records was a
128 GiB `Vec::with_capacity` abort) → walk `record_count` records with `RecordRef::decode`,
refusing any pair not strictly `(key,seq)` ascending → `pos != data.len()` ⇒ Corrupt (trailing).
Stores `RecordMeta{key_off u32, key_len u16, kind, seq, val_off u32, val_len u32}` per record.

API after parse: `len()`, `is_empty()`, `journal_seq_horizon()`, `record(i) -> RecordRef`
(panics OOR), `find(key) -> Range` = `[partition_point(k < key), partition_point(k <= key))`
(seq-ascending range for a multi-seq key; empty at insertion point), `iter()`.

### 4.3 Node-level bset FRAME (`node.rs:122-155, 740-783, 830-860, 907-914`) — K2, included because the charter folds it into "bset"

```
NODE_PAGE = 4096; frame lands at the node's current 4 KiB-aligned tail; padded_len = ceil4K(frame_len + bset_len)
v1 (BSET_FRAME_LEN = 32; every bit-17-ABSENT volume):
  [0..4)   magic u32 LE = BSET_FRAME_MAGIC = u32::from_le_bytes(b"KBSF")   (bytes: 4B 42 53 46)
  [4..6)   version u16 LE = 1
  [6..8)   reserved u16 = 0 (written 0; NOT checked on read)
  [8..16)  node_seq_at_write u64 LE      ≠ header node_seq ⇒ StaleIncarnation (clean end, recycled extent)
  [16..20) padded_len u32 LE             4 KiB multiple
  [20..24) bset_len u32 LE
  [24..32) checksum u64 LE = xxh3_64([0..24))
  [32..32+bset_len) bset image (§4.1) ; zero padding to padded_len (NOT covered by any checksum)
v2 (BSET_FRAME_V2_LEN = 40; every bit-17 volume):
  [0..24)  as v1 with version = 2
  [24..28) appender_id u32 LE   ┐ FrameStamp — (0,0) is the manager's structural stamp
  [28..32) g u32 LE             ┘ (the SLOT's lease generation, never the appender's dlm_term)
  [32..40) checksum u64 LE = xxh3_64([0..32))
  [40..40+bset_len) bset image; zero padding
```
Probe order (`probe_frame`): magic ≠ ⇒ CleanEnd; checksum over `frame_len−8` bytes fails ⇒
Garbage; version ≠ layout's version ⇒ Garbage (v1 on v2 layout and vice versa are both
foreign); `node_seq_at_write ≠ node_seq` ⇒ StaleIncarnation; else Frame. Geometry
(`frame_geometry_ok`): `padded_len ≥ 4096 ∧ padded_len % 4096 == 0 ∧ padded_len ≤ node_size −
pos ∧ bset_len ≥ 32 ∧ frame_len + bset_len ≤ padded_len`. Frame screen rules (v2, `FrameScreen::
foreign_rule`): (1) `g > g_current`; (2) `recorded_tail = Some((tg, tail)) ∧ g ≤ tg ∧ pos ≥
tail`; (3) `g < prev_g`; (4) `appender_current = Some(l) ∧ g == g_current ∧ appender_id ≠ l`.
Rules 1 and 4 are the breach class under a device fence.

Node header page (`node.rs:125-134, 483-512`) for completeness: `[0..4) "KVND" u32 LE ‖ [4..6)
version=1 ‖ [6] tree_id ‖ [7] level ‖ [8..16) node_addr ‖ [16..24) node_seq ‖ [24..28) node_size
u32 ‖ [28..30) min_key_len u16 ‖ [30..32) max_key_len u16 ‖ [32..40) xxh3 ‖ [40..) min_key ‖
max_key`, zero-filled to 4096; checksum = xxh3 over the whole page with `[32..40)` zeroed.
`fold_capacity(ns) = ns − 4096 − frame_len − 32`.

### 4.4 Source ordering & the n-way merge (`node.rs:653-661`, `bset.rs:290-396`)

- `sources[0]` is the NEWEST. A node's sources = its frames in REVERSE append order
  (`bset_views_newest_first`). Newness is positional (append order), not by horizon or seq.
- `merge(sources)` yields the record multiset ordered `(key asc, seq desc, source index asc)`:
  each key's group contiguous and newest-first — exactly the fold's input shape. Equal
  `(key, seq)` across sources: the lower (newer) source first; BOTH survive (fold is idempotent).
- A same-key multi-seq run WITHIN one source (stored ascending) is emitted in REVERSE; the
  per-source candidate is `record(run_end − 1 − run_emitted)`. `run_end` is memoized per run
  (finding 49: O(run²) decodes made a 35 k-record Δtime overlay fold take 44 s).
- Candidate selection: linear scan over sources (bset log depth ≈ 32–56 at 256 KiB) picking
  `min key`, then `max seq`, then lowest source index (ties keep the earlier `best`).
- `lookup(sources, key)`: gather `view.find(key)` from every source, `sort_unstable_by(seq desc,
  then source asc)`, `fold_newest_first`.
- `compact(sources, durable_tail)`: `merge` → group by key → `compact_fold` → output is strictly
  `(key, seq)`-ascending and directly `build_bset`-able.

### 4.5 Materialized folded head & the 8-entry memo (node_cache.rs — NOT in record/bset)

- D7.a head: each overlay record may carry `folded: Option<FoldedHead>` = fold of itself over
  everything older for its key (single head per key; `node_cache.rs:1860-1925`). Lookup serves
  the newest overlay record's head with zero decodes (`META_KV_FOLD_HEAD_SERVES`).
- D7.b memo: `FOLD_MEMO_CAPACITY = 8` populate-once `OnceLock` cells on the IMMUTABLE snapshot;
  `probe(key)` scans all 8; only `Cow::Owned` (delta-materialized) folds populate; dies with the
  snapshot at every swap. Counters `META_KV_FOLD_MEMO_{HITS,MISSES,BYTES}`;
  `MEMO_CELL_OVERHEAD = size_of::<MemoCell>()`.

---

## 5. CHECKSUMS (all xxh3-64, stored LE)

| unit | covers | field treatment |
|---|---|---|
| bset image | `[0..24) ‖ 00×8 ‖ [32..)` | checksum field at `[24..32)` ZEROED (streamed as 8 zero bytes) — `checksum_image`, `bset.rs:21-27` |
| bset frame v1 | `[0..24)` only | checksum EXCLUDED (follows the covered bytes); padding and embedded bset NOT covered (bset has its own) |
| bset frame v2 | `[0..32)` only | same; includes the `appender_id‖g` stamp |
| node header page | whole 4096 B with `[32..40)` zeroed | `header_page_checksum`, `node.rs:473-480` |
| record framing | none | trusted only inside a verified bset/entry |

Verified ONCE at load/parse; never per read.

---

## 6. PUBLIC API (Rust pseudocode; one line each)

```rust
// record.rs — tree ids / forest
pub const TREE_INODES:u8=1; TREE_DENTRIES=2; TREE_XATTRS=3; TREE_ALLOC_RESERVED=4; TREE_BACKPTR_RESERVED=5;
          TREE_BLOCK_REFS=6; TREE_BLOCK_MAP=7; TREE_CONTROL=8; TREE_SHARED_INDEX=9; KIND_INTERIOR=0; TREE_ID_MAX=9;
pub type ForestSlot = u32;  pub const NATIVE_FOREST_SLOT=0; FOREST_SLOT_MAX=65536;
pub const fn guest_forest_slot(slot:u16)->ForestSlot                 // slot+1
pub fn forest_slot_of_ino(key_ino:u64)->ForestSlot                   // key_ino>>40, unchecked
pub const FOREST_{INODE,DENTRY,XATTR,BLOCK_MAP,BLOCK_REF}_KEY_LEN: 9,17,17,13,29;  FOREST_BLOCK_REF_OWNER_OFF=17
pub fn is_slot_tree_kind(kind:u8)->bool                              // {1,2,3,6,7}
pub fn forest_key(kind:u8, legacy:&[u8])->Result<Vec<u8>>            // insert/prepend kind byte; refuses bad kind/len/slot
pub fn legacy_key_slot(kind:u8, legacy:&[u8])->Result<ForestSlot>    // slot without framing
pub fn forest_key_kind(key:&[u8])->Result<u8>                        // kind byte with full shape check
pub enum RawKeyDefect { MalformedKnownKind{kind,want,got}, UnknownKind{kind}, Truncated{got}, SlotOutOfNamespace{kind,slot} }
pub fn classify_refused_key(key:&[u8])->Option<RawKeyDefect>         // None iff forest_key_kind ok
pub fn split_forest_key(key:&[u8])->Result<(u8, Vec<u8>)>            // inverse of forest_key
pub fn forest_key_slot(key:&[u8])->Result<ForestSlot>                // slot of a forest key
// keys
pub const INODE_KEY_LEN=8; DENTRY_KEY_LEN=16; XATTR_KEY_LEN=16; HASH54_MAX=(1<<54)-1; HASH56_MAX=(1<<56)-1;
pub fn inode_key(ino:u64)->[u8;8]                                    // BE
pub fn decode_inode_key(&[u8])->Result<u64>
pub fn dentry_key_suffix(hash54:u64, coll_seq:u8)->u64               // (h<<8)|c
pub fn dentry_key(parent:u64, hash54:u64, coll_seq:u8)->[u8;16]
pub fn decode_dentry_key(&[u8])->Result<(u64,u64,u8)>                // refuses suffix>>62 != 0
pub fn xattr_key(ino:u64, hash56:u64, coll_seq:u8)->[u8;16]
pub fn decode_xattr_key(&[u8])->Result<(u64,u64,u8)>
pub fn dentry_name_hash54(name:&[u8], seed:u64)->u64                 // xxh3_seeded>>10
pub fn xattr_name_hash56(name:&[u8], seed:u64)->u64                  // xxh3_seeded>>8
pub fn first_free_coll_seq(occupied: impl IntoIterator<Item=u8>)->Option<u8>
pub fn assign_dentry_coll_seq(occupied)->Result<u8>                  // None ⇒ DentryChainOverflow + counter
// values
pub const INODE_VALUE_VERSION:u64=1;
pub struct InodeValue{mode,uid,gid,nlink,flags,rdev:u32, size,atime,mtime,ctime:u64}  // encode()->Vec<u8> (57 B); decode(&[u8])->Result<Self>
pub struct DentryValue{child_ino:u64, file_type:u8, name:Vec<u8>}    // encode()->Result; encode_parts(child,ty,name)->Result; decode
pub struct XattrValue{name:Vec<u8>, value:Vec<u8>}                   // encode()->Result; encode_parts(name,value)->Result; decode
pub const DELTA_MODE=1<<0 … DELTA_CTIME=1<<9; DELTA_MASK_ALL=0x3FF; DELTA_TIMES=0x300;
pub struct InodeDelta{mask:u16, fields:InodeValue}                   // times(m,c); ctime(c); encode(); decode(); apply(&mut InodeValue)
// records / fold
pub enum RecordKind{Put=1, Delta=2, Delete=3}
pub const RECORD_HEADER_LEN=15;
pub struct Record{key:Vec<u8>, seq:u64, kind, value:Vec<u8>}        // put(k,seq,v); delta(k,seq,&InodeDelta); delete(k,seq); record_ref()
pub struct RecordRef<'a>{key:&[u8], seq, kind, value:&[u8]}          // encoded_len(); encode_into(&mut Vec); decode(&[u8])->Result<(Self,usize)>; to_record()
pub enum Folded<'a>{Put{value:Cow<[u8]>,seq}, Tombstone{seq}, Absent} // live_value()->Option<&[u8]>
pub fn fold_newest_first<'a>(records: impl IntoIterator<Item=RecordRef<'a>>)->Result<Folded<'a>>
pub fn compact_fold(group_newest_first:&[RecordRef], durable_tail:u64)->Result<Vec<Record>>
pub enum FoldedHead{Live{value:Bytes, decoded:Option<InodeValue>, materialized:bool}, Tombstone, Absent} // owned_bytes()
pub fn fold_forward(prev:&FoldedHead, kind:RecordKind, value:&Bytes)->Result<FoldedHead>
// readdir
pub const READDIR_OFFSET_DOT=1; READDIR_OFFSET_DOTDOT=2; READDIR_COOKIE_BIAS=3;
pub enum ReaddirPos{Start, AfterDot, AfterDotDot, AfterEntry{hash54:u64, coll_seq:u8}}
pub fn encode_readdir_cookie(hash54:u64, coll_seq:u8)->u64
pub fn decode_readdir_cookie(cookie:u64)->Result<ReaddirPos>

// bset.rs
pub fn checksum_image(buf:&[u8])->u64                                // xxh3 with [24..32) zeroed
pub const BSET_MAGIC:u32=0x42534554; BSET_VERSION:u16=1; BSET_HEADER_LEN=32;
pub fn build_bset(records:&[Record], journal_seq_horizon:u64)->Result<Vec<u8>>
pub struct BsetView<'a>                                              // parse(&[u8])->Result; len(); is_empty(); journal_seq_horizon(); record(i)->RecordRef; find(key)->Range<usize>; iter()
pub struct BsetIter / MergeIter : Iterator<Item=RecordRef>
pub fn merge<'v,'a>(sources:&'v [BsetView<'a>])->MergeIter            // (key asc, seq desc, source asc)
pub fn lookup<'a>(sources:&[BsetView<'a>], key:&[u8])->Result<Folded<'a>>
pub fn compact<'a>(sources:&[BsetView<'a>], durable_tail:u64)->Result<Vec<Record>>

// block_refs.rs
pub const BLOCK_REF_KEY_LEN=28; BLOCK_REF_PREFIX_LEN=16; BLOCK_REF_OWNER_OFF=16; BLOCK_REF_VALUE_LEN=4;
          BLOCK_REF_VALUE_VERSION:u8=1; BLOCK_REF_FLAG_MAP_BLOB=1; BLOCK_REF_FLAG_SHARED=2; BLOCK_INDEX_MAP_BLOB:u32=u32::MAX;
pub fn volume_tag(volume_id:&str)->u64
pub struct BlockRef{vol_tag,block_idx,owner_ino:u64, block_index:u32}  // is_map_blob(); key(); value(); value_with(shared)
pub struct BlockRefOp{reference, take:bool, shared:bool}             // taken(); taken_shared(); released(); value()
pub fn cancel_same_reference_pairs(ops:&mut Vec<BlockRefOp>)         // drop (released r, unshared taken r) pairs
pub fn block_ref_key(vol_tag,block_idx,owner_ino:u64, block_index:u32)->[u8;28]
pub fn decode_block_ref_key(&[u8])->Result<BlockRef>
pub struct BlockRefValue{map_blob:bool, shared:bool};  pub fn decode_block_ref_value(&[u8])->Result<BlockRefValue>
pub fn volume_range(vol_tag)->(Vec<u8>,Vec<u8>)                      // [lo, hi) over a volume; hi = next tag or 29×0xFF
pub fn block_range(vol_tag, block_idx)->(Vec<u8>,Vec<u8>)            // [lo, hi) over one block; hi = next idx or max-key‖0x00

// block_map.rs
pub const BLOCK_MAP_KEY_LEN=12; BLOCK_MAP_VALUE_VERSION=1; BLOCK_MAP_KIND_{POINT=1,STRING=2,RUN=3,POINT_STAMPED=4,RUN_STAMPED=5};
          BLOCK_MAP_{POINT=18,RUN=22,POINT_STAMPED=26,RUN_STAMPED=30}_VALUE_LEN; RUN_LEN_MAX=4096; KVMAP_HEAD_PREFIX="kvmap:"; KVMAP_HEAD_MAJOR=1;
pub fn block_map_key(owner_ino:u64, block_index:u32)->Result<[u8;12]> // refuses u32::MAX
pub fn decode_block_map_key(&[u8])->Result<(u64,u32)>                // refuses u32::MAX
pub fn index_range_from(owner_ino, from_index)->([u8;12],[u8;12])    // inclusive bounds; hi uses raw u32::MAX
pub enum MapEntry{Point{vol_tag,offset}, String(Vec<u8>), Run{vol_tag,start_offset,len}, PointStamped{..,incarnation}, RunStamped{..,start_incarnation}}
          // encoded_len(); run_len(); encode()
pub fn decode_block_map_value(&[u8])->Result<MapEntry>
pub enum BlockMapOp{Put{owner_ino,block_index,entry}, Delete{owner_ino,block_index}}  // key()->Result<[u8;12]>
pub struct KvmapHead{sweep_cursor:Option<u32>, gen:u64}             // encode()->String
pub fn parse_kvmap_head(&str)->Result<KvmapHead>

// node.rs (frame — K2 surface the charter folds into "bset")
pub const NODE_PAGE=4096; BSET_FRAME_MAGIC=u32::from_le_bytes(b"KBSF"); BSET_FRAME_VERSION=1; BSET_FRAME_LEN=32;
          BSET_FRAME_VERSION_V2=2; BSET_FRAME_V2_LEN=40; RECORD_VALUE_CAP_CEILING=65536; XATTR_RECORD_ENVELOPE_MAX=256;
pub struct FrameStamp{appender_id:u32, g:u32}
pub struct FrameScreen{g_current:u32, appender_current:Option<u32>, recorded_tail:Option<(u32,u32)>, pr_fenced:bool} // is_breach_rule(r); is_current_own(stamp); foreign_rule(stamp,pos,prev_g)->Option<u8>
pub fn xattr_value_cap(node_size)->usize; pub fn record_value_cap(node_size)->usize
pub struct NodeLayout                                                // new(ns); new_symmetric(ns); stamped(FrameStamp); frame_version(); frame_len(); frame_stamp(); node_size(); fold_capacity(); …
pub fn encode_bset_frame(layout:&NodeLayout, node_seq_at_write:u64, records:&[Record], horizon:u64)->Result<Vec<u8>>
pub fn probe_tail_page(page:&[u8], node_seq:u64, layout:&NodeLayout)->TailPage{Clear,Ours,Foreign(FrameStamp)}
pub fn key_successor(key:&[u8])->Vec<u8>                             // key‖0x00

// tree.rs / forest.rs / journal.rs (keys this layer must know)
pub const KEY_SPACE_MAX:[u8;32]=[0xFF;32];
pub fn encode_interior_value(child_addr:u64, child_seq:u64)->Vec<u8>; pub fn decode_interior_value(&[u8])->Result<(u64,u64)>
pub const INTERIOR_JOURNAL_SLOT_LEN=4; pub fn interior_journal_key(slot:ForestSlot, sep:&[u8])->Vec<u8>; pub fn split_interior_journal_key(&[u8])->Result<(ForestSlot,&[u8])>
pub fn tag_for(tree_id:u8, level:u8)->u8; pub fn tag_tree_id_admissible(tree_id,level)->bool; pub fn untag(tag:u8)->(u8,u8)
pub fn encode_entry_payload(records:&[(u8,Record)])->Vec<u8>; pub fn decode_entry_payload(&[u8])->Result<Vec<(u8,Record)>>

// mod.rs — KvError variants this layer raises
DentryChainOverflow | ChecksumMismatch{stored,computed} | Corrupt(String) | InvalidReaddirCookie(u64) | NameTooLong{len} | ValueTooLarge{len,cap}
// mod.rs — counters this layer moves
META_KV_DENTRY_COLLISION_OVERFLOWS, META_KV_DELTA_ORPHANS, META_KV_FOLD_RECORD_DECODES (not on stats JSON),
META_KV_LAYOUT_DELTA_FOLDS, META_KV_FOLD_HEAD_SERVES, META_KV_FOLD_MEMO_{HITS,MISSES,BYTES}, META_KV_FOREST_KEY_VIOLATIONS
```

---

## 7. TESTS (name — one sentence; **pure** unless marked **device**)

### record.rs unit tests (`record.rs:1690-2704`) — all pure
- `tree_ids_pin_the_on_disk_format` — pins ids 0..=9 and `TREE_ID_MAX ≤ 0x0F`.
- `inode_key_is_big_endian_and_memcmp_ordered` — `inode_key(1)==[0,…,1]`, 1000 random pairs memcmp == numeric; len 7/9 refused.
- `dentry_key_orders_parent_then_hash_then_coll_seq` — layout, suffix top-2-bits zero, roundtrip, 1000 random tuple-order checks, len-15 and `0x80|byte8` refused.
- `xattr_key_orders_ino_then_hash_then_coll_seq` — same shape for xattr keys; len-12 refused.
- `dentry_hash54_is_the_seeded_xxh3_top_54_bits` — formula pin, seed-sensitivity, 4096 names ≤ HASH54_MAX.
- `xattr_hash56_is_the_seeded_xxh3_top_56_bits` — same for `>> 8`.
- `first_free_coll_seq_fills_the_lowest_hole` — ∅⇒0, {0,1}⇒2, {0,2}⇒1, {5}⇒0, order-independent, 0..=254⇒255, 0..=255⇒None.
- `dentry_chain_overflow_at_256_is_a_clean_counted_error` — 256th fits (255), 257th ⇒ `DentryChainOverflow` and counter +1.
- `inode_value_roundtrips_with_leading_version_varint` — 57 B, byte0==1, rdev roundtrips, version 2 / varint 129 / truncation / trailing / empty refused.
- `dentry_value_roundtrips_and_caps_name_at_255` — 15 B for "hello", 255 ok, 256 ⇒ NameTooLong, lying name_len (200, 2) refused, 9 zero bytes refused.
- `xattr_value_roundtrips_and_caps_name_at_255` — 312 B, empty value ok, 256-name refused, lying name_len refused, empty refused.
- `inode_delta_times_applies_only_time_fields` — 18 B, mask 0x300, apply touches only mtime/ctime.
- `inode_delta_rejects_unknown_mask_bits_and_bad_lengths` — full mask 58 B roundtrip; bit 10 / truncation / trailing / 1 B refused.
- `record_framing_roundtrips_all_kinds` — Put/Delta/Delete encode→decode sequentially, `encoded_len` exact.
- `record_decode_rejects_malformed_framing` — truncated header/key/value, key_len u16::MAX, val_len u32::MAX, kind 0/4, empty key, tombstone with value.
- `fold_lww_newest_put_wins` — newest Put wins, borrowed (zero-copy); empty ⇒ Absent.
- `fold_tombstone_shadows_older_records_in_both_scan_shapes` — `[del7, put3]` ⇒ Tombstone{7}; `[Δ9, del7, put3]` ⇒ Tombstone{7} with orphan counter UNCHANGED.
- `fold_newer_put_revives_key_after_tombstone` — `[put9, del7, put3]` ⇒ Put seq 9.
- `fold_applies_deltas_ascending_onto_newest_put` — `[Δ9, Δ5, put2, put1]` ⇒ Put{seq 9, Owned}, times from Δ9, value fields from put2.
- `fold_delta_without_base_is_a_counted_absent` — `[Δ9, Δ5]` ⇒ Absent, counter +2.
- `fold_values_stay_opaque_unless_a_delta_forces_a_decode` — garbage Put alone folds Borrowed; with a Δ on top ⇒ Corrupt.
- `compact_fold_tombstone_elision_respects_the_durable_tail_boundary` — del7: tail 8 ⇒ [], tail 7 ⇒ kept, tail 0 ⇒ kept.
- `compact_fold_emits_one_folded_put_or_nothing_per_key` — Δ+Put ⇒ one Put seq 5; shadowed Puts ⇒ newest; orphan Δ ⇒ [] (+1); del7 over put3 at tail 8 ⇒ []; empty ⇒ [].
- `fold_forward_matches_fold_newest_first` (proptest) — §3.7.
- `replay_reproduces_ram_is_one_fold_theorem` — §3.7.
- `readdir_cookie_reserved_offsets_and_bias` — constants, cookie(0,0)==3, decodes of 0/1/2/3.
- `readdir_cookie_sign_bit_is_always_clear` — max cookie == 2^62+2; 10 000 random cookies positive as i64.
- `readdir_cookie_roundtrips_and_matches_the_key_suffix_resume_rule` — roundtrip, `cookie−3 == key[8..16]`, order-preserving.
- `readdir_cookie_decode_rejects_payloads_we_never_issue` — u64::MAX and `3+2^62` refused; `3+2^62−1` ok.

### bset.rs unit tests (`bset.rs:441-1086`) — all pure
- `compaction_preserves_the_lineage_a_live_link_claims` — versioned layout chain compacts to Put + retained link (base 0); a later link claiming the old head still folds.
- `compaction_of_unversioned_chains_stays_single_record` — inode Δ chain ⇒ exactly one Put.
- `bset_build_parse_roundtrip_preserves_records_and_header` — 4 records, magic, horizon 9, roundtrip, kind of record 3.
- `bset_build_rejects_empty_unsorted_and_duplicate_inputs` — empty / key-desc / seq-desc / dup (key,seq) refused.
- `bset_parse_detects_corruption_anywhere_via_xxh3` — flips at 9, 35, last ⇒ ChecksumMismatch; bad magic/version ⇒ Corrupt; truncations; trailing 8 B refused.
- `bset_parse_bounds_checks_every_length_field` — restamped val_len u32::MAX, record_count 3 (high), 1 (low), data_len−1 all refused.
- `bset_parse_rejects_out_of_order_records_behind_a_valid_checksum` — spliced unsorted image with valid checksum refused.
- `bset_binary_search_finds_ranges_without_decoding` — ranges 0..1, 1..4 (seqs 2,5,8), 4..5; misses at insertion points.
- `bset_nway_merge_orders_key_asc_seq_desc_source_priority` — exact 7-element merged order with a cross-source duplicate preserved.
- `bset_nway_merge_reverses_same_key_runs_within_one_source` — in-source `[put3, del8]` emits 8 then 3; compact at tail 4 keeps the tombstone.
- `bset_nway_merge_matches_the_reference_order_under_long_same_key_runs` — §3.7.
- `bset_lookup_folds_across_sources_with_the_single_algebra` — Δ over older Put; tombstone shadows; untouched key Borrowed; absent.
- `bset_compact_emits_a_buildable_folded_output` — at tail 6: `[Put k1 seq9, Delete k3 seq8, Put k4 seq4]`; rebuilds; idempotent.
- `forced_collision_chain_resolves_by_name_and_orders_cookies` — two names at one (parent, hash54) with coll 0/1 resolve by full name; cookies adjacent.

### Integration files
- `tests/kv_fold_slimming_tests.rs` — **device** (tempfile + io_uring, `NODE_SIZE = 64 KiB`):
  `overlay_head_serves_hot_parent_probe_with_zero_decodes`; `overlay_head_serves_tombstone_and_plain_put`;
  `memo_serves_second_fold_of_bset_resident_key`; `memo_probes_race_free_across_threads` (8 threads);
  `mid_range_rollback_removal_folds_exactly`; `fold_head_and_memo_equal_from_scratch_fold` (proptest, §3.7);
  `node_charge_includes_overlay_and_memo_bytes`; `tiny_budget_eviction_bounds_memo_gauge`;
  `eviction_skips_externally_held_nodes`; `reference_fold_matches_known_history` (**pure**: the harness's
  reference fold against a known history).
- `tests/findvsa_fold_fixture_tests.rs` — **pure** (fixtures `tests/fixtures/findvsa_pred_bset.bin`,
  `findvsa_extra_records.jsonl` `[key_hex, kind, seq, val_hex]`, `findvsa2_stranding_window.jsonl`):
  `real_storm_split_fold_preserves_every_live_key` — `compact([extra, pred], 76_036_303)` keeps every input key;
  `recaptured_window_pins_the_stranding_signature` — every lost Put had `seq ≥ tail`, dentry keys 16 B, inode keys 8 B,
  stranding signature (`flip.seq > put.seq ∧ flip.key ≥ put.key`) recomputes and is the majority.
  **Port value: `findvsa_pred_bset.bin` is a REAL bset image — the Zig parser must accept it byte-for-byte.**
- `tests/layout_delta_fold_tests.rs` — **pure**: `layout_delta_folds_onto_base_put`; `layout_delta_chain_accumulates_entries_and_lww_fields`
  (+ byte-deterministic twice); `delta_size_and_entries_are_one_record` (magic 0x4C31 at [0..2)); `fold_forward_matches_from_scratch_on_layout_chains`;
  `fold_forward_equivalence_property_layout_class` (§3.7); `compact_fold_materializes_layout_chain_into_one_put`;
  `tombstone_shadows_layout_chain`; `layout_delta_onto_indirect_base_fails_loud` (error text contains "indirect");
  `borrowed_view_encodes_byte_identically` (bincode borrowed view == owned).
- `tests/mw_arbiter_fold_tests.rs` — **device** (full FUSE filesystem harness). NOT the record algebra: "fold" here
  is the DLM arbiter folding shipped-assembly extents (range-shared classification). Three tests:
  `the_arbiters_fold_of_a_single_holders_extents_keeps_the_fast_paths`, `the_arbiters_fold_under_a_demoted_region_keeps_the_fast_paths`,
  `a_two_holder_span_still_declines_the_arbiters_fast_paths`. Out of K1 scope.
- `tests/decoder_property_tests.rs` (relevant, **pure**): `kv_bset_record_count_cannot_drive_allocation` (32 B image claiming
  u32::MAX records refused); `kv_bset_parse_survives_a_tight_address_space_limit`; `kv_bset_record_count_bound_holds_across_data_lengths`;
  `record_decoders_never_panic` (0..512 B); `bset_parse_never_panics` (0..1024 B, raw + restamped); `bset_frame_v2_walk_never_panics`;
  `bset_frame_v2_screen_matches_the_rule_function`; `non_content_kinds_never_frame`; `record_frame_round_trips`;
  `inode_value_round_trips`; `dentry_value_round_trips`; `xattr_value_round_trips`; `bset_round_trips`; `layout_delta_round_trips`.
- Fuzz: `fuzz/fuzz_targets/kv_bset.rs` (raw + restamped arms; asserts in-bounds, strictly ascending, `find` locates self,
  `iter().count()==len`), `kv_record.rs` (all key/value/cookie decoders total; `decode(encode(x))==x`; record walk makes progress),
  `bset_frame_v2.rs`, `kv_node.rs`, `layout_wire.rs`.

---

## 8. NUMERIC CONSTANTS (with WHY) & MEASURED NUMBERS

| constant | value | why (from the source comments) |
|---|---|---|
| `GUEST_NS_SHIFT` | 40 | ≥ 2^40 ≈ 1.1×10¹² native inos, 10× past the ≥ 100 M-inode cap (`mod.rs:35-37`) |
| `FOREST_SLOT_MAX` | 65 536 | native slot + the u16 guest-slot namespace; `DERIVED_ROUTING_WIDTH = 65 536` |
| `HASH54_MAX` | 2^54−1 | 54 bits leave room for `coll_seq` (8) + bias inside a 62-bit, sign-clear i64 FUSE offset |
| `HASH56_MAX` | 2^56−1 | no cookie constraint on xattrs (`listxattr` has no resume) |
| `coll_seq` domain | 0..=255 | u8 probe space bounded by construction; expected colliding pairs in a 1 M dir ≈ n²/2^55 ≈ 2.8×10⁻⁵ (design §4.2) |
| `READDIR_COOKIE_BIAS` | 3 | offsets 0/1/2 reserved for start/`.`/`..`; max cookie 2^62+2 keeps the sign bit clear |
| `INODE_VALUE_VERSION` | 1 | leading varint for future extension |
| inode value | 57 B | varint + 6×u32 + 4×u64 |
| `DELTA_MASK_ALL` | 0x3FF | 10 defined bits; anything else = decode error; 0x4C31 layout magic lies outside on purpose |
| `RECORD_HEADER_LEN` | 15 | u16 key_len + u8 kind + u64 seq + u32 val_len |
| `RecordKind` | 1,2,3 | start at 1 so a zeroed byte is never a valid kind |
| `BSET_HEADER_LEN` | 32 | fixed header |
| min record len | 16 | header + 1-byte key ⇒ `record_count ≤ data_len/16` (TEST-4 allocation bound) |
| `NODE_PAGE` | 4096 | header page size and append granularity |
| `BSET_FRAME_LEN / _V2_LEN` | 32 / 40 | v2 adds `appender_id u32 ‖ g u32` before the checksum |
| `MIN/DEFAULT/MAX_NODE_SIZE` | 64 KiB / 256 KiB / 1 MiB | format knob range; 256 KiB = bcachefs default |
| `RECORD_VALUE_CAP_CEILING` | 65 536 | Linux `XATTR_SIZE_MAX` |
| `XATTR_RECORD_ENVELOPE_MAX` | 256 | 1-byte name_len + ≤255 name — so a full 64 KiB user value with a maximal name still fits (fstests generic/020) |
| xattr value cap | `min(65536, node_size/4)` | keeps largest record + headers inside one node at every knob |
| `RUN_LEN_MAX` | 4096 | A6: the floor probe is one bounded forward range scan of this width — a format LAW, not a knob |
| run len domain | 2..=4096 | a 1-run is a point; a wider run is invisible to the floor probe |
| `BLOCK_INDEX_MAP_BLOB` | u32::MAX | u32 indices address 16 EiB at 4 MiB blocks; top value unreachable as a real index |
| `BLOCK_REF_VALUE_LEN` | 4 | version ‖ flags ‖ reserved u16 |
| `KEY_SPACE_MAX` | 32×0xFF | top separator every tree's rightmost child shares |
| `FOLD_MEMO_CAPACITY` | 8 | "the hot-parent-key case needs 1"; fixed = the §5.7 per-node memory bound |
| `TREE_ID_MAX` | 9 | journal tag's low nibble bounds ids at 15 |
| `INTERIOR_JOURNAL_SLOT_LEN` | 4 | slot u32 BE prefix on a slot tree's interior journal key |

Measured numbers quoted in comments:
- Durable refs: **40–52 ns CPU + 46 B journal per reference** (`AGENTS.md:163`, `.benchmarks/2026-08-04-durable-block-refcounts.md`).
- Interior fanout at 256 KiB ≈ **~7,000 children** (≈ 36 B/interior record) ⇒ 100 M records at depth ≤ 3 with < 3 MiB interior (design §4.2).
- Bset log depth per node **~32–56** at 256 KiB (`bset.rs:365`) — why the merge's candidate scan is linear.
- Fold re-decode tax before D7: **~16 % of daemon CPU** (`mod.rs:663`, design-metadata-throughput §5.7).
- Finding 49: a **35 k-record** frozen Δtime overlay folded in **44 s** before the run-end memo (`bset.rs:323-327`).
- TEST-4: `record_count = u32::MAX` ⇒ `Vec::with_capacity(4 294 967 295)` × 32 B = **128 GiB** abort (`bset.rs:157-170`).
- Rung-19 field corpse: "link seq 3024497 names base version 0x800000001d6 but folds onto 0x0", **5,421** failed checkpoint ticks (`bset.rs:511-515`).
- Pre-campaign journal entry mean **18.2 KiB** per 4 MiB published block (`layout_wire.rs:10-11`).
- findvsa fixture `durable_tail = 76_036_303`; FIND-VS-A: 14 predecessor keys lost; recapture: 311 acked creates lost on a CLEAN replay.

---

## 9. FUZZ / DECODER-TOTALITY POSTURE

Law (design §9): **every length/count in the wire is a CLAIM validated against its container
before any byte is dereferenced and never an allocation authority.**

- `Reader::take(n)` (`record.rs:632-647`): `pos.checked_add(n)` (overflow ⇒ Corrupt) then
  `end > buf.len()` ⇒ Corrupt; `finish()` refuses trailing bytes (InodeValue, DentryValue,
  InodeDelta; NOT XattrValue whose value is the remainder).
- `RecordRef::decode`: `total = 15 +key_len +val_len` via `checked_add` chain; `buf.len() < total`
  ⇒ Corrupt; empty key refused; tombstone with value refused; kind ∉ {1,2,3} refused.
- `BsetView::parse`: magic/version/reserved → exact `32 + data_len` length → checksum → **count
  bound `record_count ≤ data_len / 16`** → walk with ordering check → exact end. `Vec::with_capacity
  (record_count)` happens only AFTER the bound. Successful parse ⇒ every `record(i)` is in-bounds.
- `LayoutDelta::decode`: `entries` presized `min(n, 4096)`; every string length checked; UTF-8 checked.
- Frame probe: branches ONLY on checksum-verified bytes; geometry validated against `node_size − pos`.
- `decode_readdir_cookie`: payload bound before any shift.
- Varint: > 64 bits ⇒ Corrupt; truncated ⇒ Corrupt.
- Fuzz contracts: `parse`/`decode` return or error, never panic, never over-allocate; a record
  walk must make progress (`used > 0`); `decode(encode(x)) == x` for every value type;
  `view.find(r.key)` contains `i`; after parse, strictly `(key, seq)` ascending.
- The checksum is **integrity, not authentication**: fuzz and tests RESTAMP mutated images to
  reach the structural gates behind it (`checksum_image` is `pub` for exactly this).

---

## 10. DISCREPANCIES — charter/design prose vs as-built bytes (match the BYTES)

1. **Inode value is 57 B, not "≈ 96 B"**; field order `{mode,uid,gid,nlink,flags,rdev,size,atime,mtime,ctime}`
   — `rdev` occupies the word the docs call `flags2`; times are u64 LE ns.
2. **Record framing has no `flags` field**: `key_len u16 ‖ kind u8 ‖ seq u64 ‖ val_len u32` (LE), key, value.
3. **Bset header** is `{magic, version u16, reserved u16, record_count u32, data_len u32, journal_seq_horizon u64, xxh3 u64}`
   (32 B). `node_seq_at_write` and the 4 KiB-multiple `padded_len` live in the node FRAME (`KBSF`), not the bset.
4. The v2 `appender_id ‖ g` stamp is in the FRAME at `[24..32)`, before the frame checksum at `[32..40)`; the bset
   image itself is identical on v1 and v2 volumes.
5. Tombstone elision is `seq < durable_tail` ⇒ elide; `seq == tail` survives (the charter's wording agrees; pinned by test).
6. Block-ref key is 28 B legacy / 29 B forest; `vol_tag` precedes `block_idx` (charter order matches).
7. `fold_newest_first`'s returned `seq` for a delta-folded Put is the NEWEST DELTA's seq, not the Put's.
8. Deltas collected above a `Delete` are discarded WITHOUT counting as orphans; only an exhausted scan counts.
9. Two delta classes share the `Delta` kind, discriminated by payload magic `0x4C31`; a chain mixing classes ⇒ Corrupt.
10. The forest inserts the kind at offset 8 for ino-major kinds but PREPENDS it for refs (kind 6), and refuses `0x06` at offset 8.
