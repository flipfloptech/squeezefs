# PORTING SPEC — MetaLV node layer ("PR K2"): format, load, append, compact/split, node_seq, lifecycle word

> **Checksum amendment (2026-10-11, decision 0003 §M; 0002 row 480).** Every `xxh3` / `xxh3-64` /
> `xxh3_64` below names the hash the reference used and this port used until format v5. The port's
> ONE hash is now **sqzhash128** (`src/core/sqzhash.zig`, the pinned definition): every 8-byte checksum
> and identity field is its LOW 64 bits under the fixed public format key (`checksum.hash64`,
> `hash64FieldZeroed`, `stamp`/`verify`, the streaming `checksum.Hasher`); the seeded name hashes
> (`hash54`/`hash56`) are `hash64Keyed` under the per-mount name key (`key.NameKey`: the volume's
> `hash_seed` zero-extended, expanded once at mount). Field names on the wire read `csum`. Byte
> layouts, offsets and widths below are unchanged; only the function behind the 8 bytes is.

> **v4 decisions (format v4, 2026-10-05; decision 0003 WP-F).** The header page below is unchanged
> (`KVND`, the `@offsetOf` layout, the four-rule frame screen); what moved under it: the bset frame is
> the ONE 40 B stamped frame (`node.FRAME_LEN = bset.BSET_FRAME_LEN = 40`; `NodeLayout{node_size, stamp}`
> — the v1/v2 split and `NodeLayout.frame` are gone), records carry the 16 B header and kind-carrying
> keys. **v4 ties** (256 KiB / 64 KiB): `foldCapacity = node_size − 4096 − 40 − 32` = 257,976 / 61,368;
> `splitPartCapacity = ¾C` = 193,482 / 46,026; `mergeCandidateCapacity = ¼C` = 64,494 / 15,342;
> `splitGrowthWindow` 127,690; interior separator record over an inode key = 16 + 9 + 16 = 41 B ⇒ fanout
> `193,482 / 41 = 4,719` (64 KiB: 1,122) and the full fold's `257,976 / 41 = 6,292` separators (the
> charter's "≈ 7,000"; 100 M records still at depth ≤ 3); a ref separator record is 49 B. A slot tree's
> nodes carry header id 0 and the RAM slot stamp; the control tree's carry 8. The byte-identity
> cross-check against the reference's `ref-sw.img` is retired (0002 row 50).

Source of truth (read-only): `/Users/joberdorf/Source/squeezefs/src/meta_backend/kv/{node.rs, node_seq.rs, node_state_core.rs}`,
`loom-models/src/lib.rs` (`node_state_core` model, lines 2050–2287), `docs/design-cow-kv-metadata.md`
§4.1/§4.2/§4.5/§4.6/§4.6a/§4.9a, `docs/design-symmetric-metadata.md` §5.8.2.
Charter anchors (squeezefs-zig `docs/charter.md`): §5.1 "Node" paragraph (line 108), §5.3 "Node cache" /
"Writeback" / "SMOs" (lines 128–131). Everything below is byte-exact unless marked *policy*.

Notation: `LE` = little-endian. `u16/u32/u64` are fixed-width LE. `xxh3` = xxh3_64 (xxhash-rust `xxh3_64`,
seed 0). `page_align(n) = ceil(n / 4096) · 4096`. `C` = `fold_capacity`.

---

## 0. Model in one screen

```
Node (one heap extent, node_size bytes, 4 KiB-aligned address A):
  [0 .. 4096)             header page   (checksummed as a unit; self-addressed: header.node_addr == A)
  [4096 .. tail)          bset frames, in append order, each frame page_align()'d
  [tail .. node_size)     never-written tail (zeros / residue of a previous incarnation)
Frame = frame header (32 B v1 | 40 B v2) ‖ K1 bset image ‖ zero pad to a 4 KiB multiple
```
- CoW: compaction/split/merge write a FRESH extent (never in place; `check_fresh_destination`, node.rs:1342–1351).
- Appends write ONLY `[tail, tail+frame_len)` — never-written bytes — so a torn append damages nothing live (node.rs:19–22, :999–1002).
- Incarnation: every frame carries `node_seq_at_write`; only frames `== header.node_seq` belong (node.rs:14–18, :846–849).
- Interior values = `(child_addr: u64 LE, child_node_seq: u64 LE)` = 16 B (tree.rs:148–167).

---

## 1. ON-DISK FORMAT (byte-exact)

### 1.1 Node header page — exactly 4096 B (`NODE_PAGE`, node.rs:123)

| off | width | field | value / rule |
|---|---|---|---|
| 0 | u32 | `magic` | `NODE_MAGIC = u32::from_le_bytes(b"KVND")` = `0x444E564B` (node.rs:126) |
| 4 | u16 | `version` | `NODE_FORMAT_VERSION = 1` (node.rs:128) |
| 6 | u8 | `tree_id` | tree id (0 = tree 0 / slot trees on a forest volume; 1 inodes, 2 dentries, 3 xattrs, …) |
| 7 | u8 | `level` | 0 = leaf; >0 interior (same format) |
| 8 | u64 | `node_addr` | SELF extent byte offset; mismatch at load = misdirected I/O → loud (node.rs:1128–1134) |
| 16 | u64 | `node_seq` | incarnation stamp (§3) |
| 24 | u32 | `node_size` | extent size this node was written for; must equal the volume layout's (node.rs:1135–1140) |
| 28 | u16 | `min_key_len` | |
| 30 | u16 | `max_key_len` | |
| 32 | u64 | `checksum` | xxh3 over the WHOLE 4096 B page with bytes [32..40) zeroed (`header_page_checksum`, node.rs:474–480) |
| 40 | `min_key_len` | `min_key` | raw bytes, inclusive lower bound |
| 40+min_key_len | `max_key_len` | `max_key` | raw bytes, inclusive upper bound |
| … | — | zero pad | to 4096 |

`NODE_HEADER_FIXED_LEN = 40` (node.rs:134). Encode refuses `min_key_len > 65535 || max_key_len > 65535 ||
40 + min + max > 4096` (node.rs:485–495). The design doc's field list `{magic, addr, node_seq, tree_id, level,
min_key, max_key, version, xxh3}` (charter §5.1) is exactly these fields in the ORDER ABOVE — the charter
prose order is not the byte order; `node_size` is an additional self-description field.

Decode order (node.rs:517–560): length == 4096 → magic → version → checksum (stored vs computed) →
`40 + min_key_len + max_key_len ≤ 4096` → slice keys. A zeroed extent fails on magic (`Corrupt`), never as a tear.

Checksum computation is streaming: `update(page[0..32]) ; update([0u8;8]) ; update(page[40..4096])`
— the Zig side MUST hash the zero-filled checksum slot in place, not skip it (node.rs:475–479).

### 1.2 Bset frame header — v1 (32 B) and v2 (40 B)

v1 (`BSET_FRAME_LEN = 32`, `BSET_FRAME_VERSION = 1`, node.rs:137–146) — every bit-17-ABSENT volume:

| off | width | field |
|---|---|---|
| 0 | u32 | `magic` = `BSET_FRAME_MAGIC = u32::from_le_bytes(b"KBSF")` = `0x4653424B` |
| 4 | u16 | `version` = 1 |
| 6 | u16 | reserved = 0 (written 0; NOT checked on read — see §9) |
| 8 | u64 | `node_seq_at_write` |
| 16 | u32 | `padded_len` — total frame length incl. pad; 4 KiB multiple |
| 20 | u32 | `bset_len` — exact K1 bset image length (header 32 + data) |
| 24 | u64 | `checksum` = xxh3 over bytes [0..24) |

v2 (`BSET_FRAME_V2_LEN = 40`, `BSET_FRAME_VERSION_V2 = 2`, node.rs:147–155) — every incompat-bit-17 (symmetric forest) volume:

| off | width | field |
|---|---|---|
| 0..24 | as v1 | (version = 2) |
| 24 | u32 | `appender_id` |
| 28 | u32 | `g` — the SLOT's lease generation at the write (never the appender's dlm_term) |
| 32 | u64 | `checksum` = xxh3 over bytes [0..32) |

Then: `[frame_len .. frame_len + bset_len)` = the K1 bset image; `[frame_len+bset_len .. padded_len)` = zeros.
`padded_len = page_align(frame_len + bset_len)` (node.rs:759). The pad is deliberately NOT covered by any
checksum (a torn append may truncate pad harmlessly, node.rs:144–146). A layout admits exactly ONE frame
version: v2-on-v1-layout and v1-on-v2-layout are both `Garbage` (node.rs:842–845).

The structural stamp `(appender_id, g) = (0, 0)` is the manager's for tree 0 / native slot / unleased trees
(`FrameStamp::default()`, node.rs:160–167).

### 1.3 Bset image (K1 — restated for byte identity; `bset.rs`)

`BSET_HEADER_LEN = 32` (bset.rs:36): `magic u32 = 0x4253_4554` (bset.rs:30 — a numeric constant serialized LE, so the
on-disk bytes are `54 45 53 42`, NOT ASCII "BSET"; contrast `KVND`/`KBSF`, which are `from_le_bytes` of the ASCII and
therefore appear in ASCII order on disk) `| version u16 = 1 |
reserved u16 = 0 (CHECKED = 0 on read, bset.rs:129–134) | record_count u32 | data_len u32 | journal_seq_horizon u64 |
checksum u64` where checksum = xxh3 over `[0..24) ‖ [0u8;8] ‖ [32..)` (bset.rs:21–27). Records follow, strictly
`(key, seq)` ascending, each `key_len u16 | kind u8 (1 Put, 2 Delta, 3 Delete) | seq u64 | val_len u32 | key | value`
(`RECORD_HEADER_LEN = 15`, record.rs:1025–1027, :1094–1111). Empty bset refused at build (bset.rs:45–49).
Parse requires `buf.len() == 32 + data_len` exactly — the node layer slices `bset_len` bytes before parsing.

### 1.4 Geometry constants

| name | value | why (file:line) |
|---|---|---|
| `NODE_PAGE` | 4096 | header page size AND append granularity (node.rs:122) |
| `MIN_NODE_SIZE` | 65,536 | format-knob floor `--meta-node-kib` (node.rs:247) |
| `MAX_NODE_SIZE` | 1,048,576 | knob ceiling (node.rs:249) |
| `DEFAULT_NODE_SIZE` | 262,144 | bcachefs's shipped `btree_node_size`; interior < 3 MiB/100 M records pinnable; 32–56 × 4 KiB appends amortize compaction ≈ 8 KiB/append; one-node read under the ~100 µs NVMe knee; value cap lands on XATTR_SIZE_MAX (design §4.4 "Read amplification") |
| `RECORD_VALUE_CAP_CEILING` | 65,536 | Linux `XATTR_SIZE_MAX` (node.rs:254) |
| `XATTR_RECORD_ENVELOPE_MAX` | 256 | 1 B name_len + 255 B name (node.rs:259) |
| `xattr_value_cap(ns)` | `min(65536, ns/4)` | §4.2; = 65,536 @256 KiB, 16,384 @64 KiB, 32,768 @128 KiB (node.rs:263) |
| `record_value_cap(ns)` | `xattr_value_cap + 256` | the ENCODED-value budget enforced at every write; 65,792 @256 KiB, 16,640 @64 KiB (node.rs:274–276; VL10 gate fix) |
| `NodeLayout::new(ns)` | valid iff `64 KiB ≤ ns ≤ 1 MiB ∧ ns % 4096 == 0` else `Corrupt` (node.rs:292–303) | |
| `fold_capacity()` | `ns − 4096 − frame_len − 32` | encoded record bytes ONE image holds after header + one frame (node.rs:379–381). 256 KiB v1: **257,984**; 256 KiB v2: 257,976; 64 KiB v1: 61,376 |
| `split_part_capacity()` | `C·3/4` (integer) | ¾ fill target of a split part (node.rs:387–389). 256 KiB v1: **193,488**; 64 KiB: 46,032 |
| `merge_pair_capacity()` | `= split_part_capacity` | §4.6a(a) (node.rs:395–397) |
| `merge_candidate_capacity()` | `C·3/4 − C/2` (= ¼C) | underfull bound (node.rs:407–409). 256 KiB v1: **64,496**; 64 KiB: 15,344 |
| `smo_extents_for_parts(fold, parts)` | `1` if `fold ≤ C` else `parts + 2` | heap-admission promise; +1 packer rounding, +1 cascade (node.rs:423–429) |
| `split_growth_window()` | `max(1, split_part_capacity − record_value_cap)` | bytes a promised leaf absorbs without re-walk (node.rs:436–440). 256 KiB v1: 127,696 |
| `append_frame_len(bytes)` | `0` if bytes==0 else `page_align(frame_len + 32 + bytes)` | (node.rs:445–451) |

Fanout math (design §4.2 line 172): interior record ≈ 15 hdr + 16 value + 8–16 key ≈ 36–40 B →
`257,984 / 36 ≈ 7,166` → "≈ 7,000 children at 256 KiB" → 100 M records at depth ≤ 3 with < 3 MiB of interior.

### 1.5 Append mechanics and "tail" tracking

There is NO tail field on disk. The tail is derived by the load walk (§2.1) and tracked in RAM as
`AppendDest { node_addr, node_seq, tail_offset }` (node.rs:580–589); `LoadedNode::append_dest()` seeds it;
each `append_bset` returns the new tail = `tail_offset + frame.len()` (node.rs:1031, :1037).
`write_node` returns `bytes_written = 4096 + padded base frame` which IS the fresh node's tail (node.rs:601–602).

`append_bset` (node.rs:1006–1038) preconditions: `node_addr % 4096 == 0`; `4096 ≤ tail_offset ≤ node_size ∧
tail_offset % 4096 == 0` else `Corrupt`; builds the frame; `frame.len() > node_size − tail_offset` ⇒
`NodeFull { needed, available }` (nothing written); else ONE `write_at(node_addr + tail_offset, frame)`.
Counters: `META_KV_NODE_APPENDS += 1`, `META_KV_NODE_APPEND_BYTES += frame.len()`.

`write_node` (node.rs:944–997): addr aligned; **finding-41 guard** — if records non-empty and
`first.key < min_key || last.key > max_key` ⇒ `Corrupt` (stranded-record hazard); image = header page ‖
(base frame iff records non-empty); `4096 + frame > node_size` ⇒ `NodeFull`; ONE `write_at(node_addr, image)`;
`META_KV_NODE_REWRITE_BYTES += image.len()`, `META_KV_NODE_IMAGES += 1`. Header-only node (no records) is legal:
tail = 4096, horizon 0.

`encode_bset_frame` (node.rs:740–784): every `value.len() > record_value_cap` ⇒ `ValueTooLarge { len, cap }`
(checked BEFORE building); `build_bset(records, horizon)` (non-empty, strictly (key,seq)-ascending);
`bset_len` and `padded_len` must fit u32 else `Corrupt`.

---

## 2. LOAD — `verify_node_extent_screened` (node.rs:1105–1309), the pure half of `load_node`

Input: `buf` (exactly `node_size` bytes, one `read_at`), `layout`, `node_addr`, `durable_tail: u64`,
`screen: Option<&FrameScreen>`.

### 2.1 Main walk (positional, trusts only verified bytes)

```
assert buf.len() == node_size (Corrupt) ; header = decode_page(buf[0..4096]) (errors wrapped "node at {addr}: …")
header.node_addr == node_addr  else Corrupt("self-address mismatch")
header.node_size == node_size  else Corrupt("node_size mismatch")
pos = 4096; ranges = []; torn_stop = false; prev_g = None; screened_at = None
while pos < node_size:
  match probe_frame(buf, pos, header.node_seq, layout):          # node.rs:830–860
    CleanEnd            -> break        # magic != KBSF (unwritten tail / residue / tear shorter than magic)
    StaleIncarnation    -> break        # verified frame, node_seq_at_write != header.node_seq (recycled residue; ANY other value, higher or lower)
    Garbage             -> torn_stop = true; break   # magic ok but frame checksum fails, OR version != layout's
    Frame(f):
      if !frame_geometry_ok(f, pos)      -> return Corrupt("impossible geometry")   # a VERIFIED liar is never a tear
      if f.stamp (v2):                    # frame screen, §2.3
          rule = screen ? screen.foreign_rule(stamp, pos, prev_g) : (prev_g.is_some() && stamp.g < prev_g ? Some(3) : None)
          if rule: screened_at = (pos, stamp, rule); break
          prev_g = stamp.g
      bset = buf[pos+frame_len .. pos+frame_len+f.bset_len]
      match BsetView::parse(bset):  Ok -> ranges.push(range); pos += f.padded_len
                                    Err -> torn_stop = true; break      # frame landed, records did not; never branch on its bytes
tail_offset = pos
```
`probe_frame` order: magic → checksum over `frame_len−8` bytes → version → node_seq → (v2) stamp. Only after the
checksum verifies is anything else branched on (node.rs:823–829).

`frame_geometry_ok` (node.rs:908–914): `padded_len ≥ 4096 ∧ padded_len % 4096 == 0 ∧ padded_len ≤ node_size − pos
∧ bset_len ≥ 32 ∧ frame_len + bset_len ≤ padded_len`.

### 2.2 Screened-stop diagnosis (only if `screened_at`)

Count by rule: breach rules {1,4} AND `screen.pr_fenced` ⇒ `META_KV_APPENDER_FENCE_BREACH += 1` + invariant
tripwire `meta_kv_appender_fence_breach`; otherwise `META_KV_FOREIGN_FRAMES_SCREENED += 1` + warn log
(node.rs:1205–1235). Then scan `probe = at + 4096, += 4096` while `probe + frame_len ≤ node_size`: a geometry-ok
same-incarnation frame whose stamp `screen.is_current_own(stamp)` (same `g_current`, and same appender while leased)
⇒ `META_KV_FOREIGN_FRAME_OVERWRITE_DETECTED += 1`, return `Corrupt("…zombie overwrote an acked frame…")`
(residual class (ii), node.rs:1236–1258). A screened stop is NOT a tear: the §2.4 pass is skipped.

### 2.3 The frame screen — `FrameScreen` (node.rs:184–245), v2 layouts only

```
FrameScreen { g_current: u32, appender_current: Option<u32>, recorded_tail: Option<(tails_g: u32, tail: u32)>, pr_fenced: bool }
foreign_rule(stamp, pos, prev_g) -> Option<u8>:
  1: stamp.g > g_current                                               # impossible → breach class
  2: recorded_tail = Some((tg, t)) ∧ stamp.g ≤ tg ∧ pos ≥ t            # stale lessee's frame at/past the recorded log tail of its generation
  3: prev_g = Some(p) ∧ stamp.g < p                                    # non-monotone g within one log
  4: appender_current = Some(a) ∧ stamp.g == g_current ∧ stamp.appender_id != a   # one lessee per leased generation (inert while UNLEASED)
  else None (kept)
is_breach_rule(r) = r ∈ {1, 4}
is_current_own(stamp) = stamp.g == g_current ∧ (appender_current.is_none() ∨ stamp.appender_id == appender_current)
```
`screen == None` (every v1 load; a v2 load with no lease plane) applies rule 3 alone (node.rs:1171–1175).
Rules are checked 1→2→3→4 in that order; the FIRST match is the reported rule.
**`(0,0)` first-frame priming law** (design-symmetric §5.8.2 line 680): a frame written with no fence installed
carries `(0,0)`; on a leaf whose earlier frames sit at `g ≥ 1` it trips rule 3 and ENDS the log there at the
next load — so every writer must prime its stamps (`prime_frame_stamps`) before its FIRST frame, including the
pre-claim ring-recovery flush. The first frame of a fresh log has `prev_g = None`, so rule 3 cannot fire on it.
`layout.stamped(stamp)` is a per-write copy; a v1 layout ignores it (node.rs:320–328).

### 2.4 Torn-tail diagnosis pass (only if `screened_at.is_none()`) — node.rs:1261–1295

```
dropped = torn_stop ? 1 : 0
probe = pos + 4096
while probe + frame_len ≤ node_size:
  if Frame(f) = probe_frame(buf, probe, header.node_seq, layout) ∧ frame_geometry_ok(f, probe):
     if Ok(view) = BsetView::parse(buf[probe+frame_len ..+ f.bset_len]):
        if view.journal_seq_horizon ≤ durable_tail:
            return Err(CheckpointCoveredBsetAfterTear { node_addr, bset_offset: probe, horizon, durable_tail })   # LOUD
        dropped += 1
  probe += 4096
if dropped > 0: META_KV_NODE_DROPPED_TAIL_BSETS += dropped
```
Semantics (design §4.5): the first failure truncates the view; a LATER valid same-incarnation bset with
`horizon ≤ durable_tail` means checkpoint-covered data follows a tear — impossible under the §4.6 barrier order ⇒
loud; otherwise the tail is the un-checkpointed replay window (replay re-supplies every seq > tail) ⇒ drop silently,
count in `META_KV_NODE_DROPPED_TAIL_BSETS` (also `LoadedNode::dropped_tail_bsets()`). Nonzero after a CLEAN
unmount is the corruption alert. Note the stride scan runs from `pos + 4096` (it does not re-probe the torn slot),
and `dropped` counts the torn unit itself (1) plus every verifying bset beyond it. On a forest slot tree the horizon
is a record stamp and `durable_tail` a ring-0 position, so the tripwire can only be QUIETER, never unsafe
(node.rs:1265–1270).

Returned: `LoadedNode { header, buf, bset_ranges, tail_offset = pos, dropped_tail_bsets, foreign_frames_screened ∈ {0,1} }`.

### 2.5 Interior values and the seq-mismatch retry

Interior record value (level > 0, `Put` only; `Delete` tombstones a separator): 16 B = `child_addr u64 LE ‖
child_node_seq u64 LE`; decode refuses any other length (tree.rs:156–167). Separator key = the child's `max_key`;
sibling partition is gap-free: `right.min = key_successor(left.max) = left.max ‖ 0x00` (node.rs:712–717).
Traversal (`KvTree::descend`, tree.rs:1215–1363) is latch-free over arc-swap snapshots; a restart happens on:
root extent retired, `root.seq != cached.node_seq`, routing hole (`next_live` finds nothing), child extent retired,
`child.node_seq() != child_seq`, or `cur.level < target_level` (root collapse). **Bound: `RETRY_BUDGET = 256`**
(tree.rs:71); every restart after the first `yield_now()`s (tree.rs:1274–1285); exhaustion is `Corrupt` carrying the
5 reason tallies `[root-retired, root-seq, routing-hole, child-retired, child-seq]`. A projection tree refreshes
its root every `PROJECTION_REFRESH_EVERY = 8` restarts (tree.rs:76). Reader mounts additionally retry a loud load
verdict `READER_LOAD_RETRIES = 3` times (a read racing a single `write_at` append), counted
`META_KV_READER_LOAD_RETRIES` (node_cache.rs:2451–2487).

### 2.6 Residue ceiling — `residue_seq_ceiling(buf) -> u64` (node.rs:1372–1406)

max of: header `node_seq` if the header page decodes, and `node_seq_at_write` of every frame header at any 4 KiB
slot whose magic is KBSF and whose version ∈ {1,2} verifies over ITS OWN checked span (24 or 32 B). 0 if nothing
verifies. Used by the offline census reclaim to floor the writer's seq above residue it is about to recycle.

### 2.7 Tail-page probe — `probe_tail_page(page, node_seq, layout) -> TailPage` (node.rs:892–903)

`page.len() < frame_len` ⇒ `Clear`. `probe_frame(page, 0, …)`: `Frame(f)` with v2 stamp `≠ layout.frame_stamp()`
⇒ `Foreign(stamp)`; any other `Frame` ⇒ `Ours`; else `Clear`. Called ONLY by `NodeCache::append_frozen` on a
non-solo volume (one extra 4 KiB read of the destination page): `Ours` ⇒ `META_KV_NODE_PARTITION_REFUSALS`, `Corrupt`
(a peer appended into a node we cache); `Foreign` ⇒ `META_KV_FOREIGN_FRAME_OVERWRITE_DETECTED`, `Corrupt`
(node_cache.rs:4073–4110).

---

## 3. NODE SEQS — `node_seq.rs`

Identity word of the CoW law: child/root pointer checks, the §4.5 frame incarnation check, the residue ceiling all
compare seqs for EQUALITY and rest on "no two nodes ever carry one seq" (node_seq.rs:4–7).

- Volume base `B = node_seq_base(uuid) = u64::from_le_bytes(uuid[0..8]) & (u64::MAX >> 1)` — top bit clear
  (builder.rs:752–754). Why: quick format zeroes only `[0, heap.start)`, so dead-generation frames survive; a
  uuid-derived base makes a cross-generation collision as unlikely as a checksum collision.
- `INCARNATION_SPACE_BITS K = 38`, `INCARNATION_SPACE = 2^38`, `INCARNATION_ORDINAL_BITS = 63 − 38 = 25`,
  `INCARNATION_ORDINAL_MAX = 2^25 − 2` (node_seq.rs:57–65). Why 38: 2^38 ≈ 2.7 × 10^11 mints per incarnation
  (a joiner at 200 SMOs/s — the storm rows' rate — for 43 years); 2^25 ≈ 33.5 M incarnations (15 k mounts
  re-joining daily for six years) (node_seq.rs:33–40).
- `incarnation_base(B, o) = B + o·2^38` iff `o ≤ ORDINAL_MAX` and `B + (o+1)·2^38` does not overflow; else `None`
  (node_seq.rs:69–77). Incarnation 0 = the manager's / every flat or unarmed mount's space (legacy seeding
  `ledger.seq.max(node_seq_watermark)`); every JOINED appender gets `o ≥ 1`, minted by the manager from the durable
  tree-0 counter, barriered BEFORE the reply; a REJOIN is a new incarnation (node_seq.rs:17–31).
- `screen_incarnation_base(B, word)`: refuse (`Rejected`) if `word < B`, `word == B` (incarnation 0 is the
  manager's), `(word − B) % 2^38 != 0`, or past capacity; else `Ok(word)` (node_seq.rs:93–117). Total, pure.
- Counter record: tree-0 key `b"node_seq_incarnations"`, value `version u8 = 1 ‖ minted u64 LE` (9 B exactly);
  decode refuses empty, version ≠ 1, len ≠ 9 (node_seq.rs:123–157).
- `NodeSeqHandle { next: AtomicU64, ceiling: u64, raises: bool }`:
  - `shared(seed)`: ceiling `u64::MAX`, raises (legacy, byte-identical to the pre-PR-13 AtomicU64).
  - `shared_bounded(seed, B)`: ceiling `incarnation_base(B, 1)`, raises.
  - `incarnation(base)`: `next = base`, ceiling `base + 2^38` (saturating), NEVER raises.
  - `next()`: `seq = fetch_add(1, AcqRel) + 1`; if `seq ≥ ceiling` ⇒ `fetch_min(ceiling)` (park so retries refuse too)
    and `Corrupt("node-seq space exhausted…")` — never wraps (node_seq.rs:208–221). First mint = base + 1.
  - `raise_to(seq)`: `if raises ∧ seq < ceiling { fetch_max(seq, AcqRel) }` — a target outside the space is ignored.
  - `load()` = the ledger's `node_seq_watermark`; `ceiling()`.
- ORDER comparisons on seqs are confined to one space or made equality (`install_recovered_root` uses `!=`); a
  numerically HIGHER stale frame stamp still classifies `StaleIncarnation` — never loud (node.rs:806–818).

---

## 4. THE LIFECYCLE CORE — `node_state_core.rs` (dependency-free, loom-included via `#[path]`)

### 4.1 Word layout

One `AtomicU64 word`. Bits: `DIRTY = 1`, `FREEZING = 1<<1`, `SUPERSEDED = 1<<2` (node_state_core.rs:59–64).
Decode (`state()`, Acquire load): `SUPERSEDED` dominates → `Superseded`; else `FREEZING` → `Serializing`
(even with DIRTY re-set); else `DIRTY` → `Dirty`; else `Clean` (node_state_core.rs:133–144).

| state | word | meaning |
|---|---|---|
| clean | 0 | nothing un-flushed; the ONLY evictable state |
| dirty | DIRTY | open delta holds records not yet frozen |
| serializing | FREEZING (± DIRTY) | a frozen delta image is in flight OUTSIDE the lock; DIRTY re-set = commits landed in the fresh delta meanwhile |
| superseded | SUPERSEDED (terminal) | severed from the cache (SMO swap or eviction); readers keep snapshots, writers revalidate-and-retry |

### 4.2 Transitions (every one is ONE atomic RMW; orderings as written)

| fn | RMW | ordering | result |
|---|---|---|---|
| `mark_dirty()` | `fetch_update(w → if w&SUP {None} else {w\|DIRTY})` | AcqRel / Acquire | `Err(Superseded)` iff SUP set — nothing accepted, caller re-resolves |
| `begin_freeze()` | CAS loop `w → (w & !DIRTY) \| FREEZING` | AcqRel / Acquire (`compare_exchange_weak`) | refuses in order: `Superseded`, `AlreadyFreezing`, `NotDirty` |
| `begin_forced_freeze()` | same CAS, but no `NotDirty` check | AcqRel / Acquire | enter FREEZING with an EMPTY frozen delta (VL7 D4 nudge / merge of a clean sibling) |
| `end_freeze() -> bool` | `fetch_and(!FREEZING)` | AcqRel | returns `prev & DIRTY != 0` (re-enqueue signal); legal on a superseded node; debug-asserts FREEZING was set |
| `abort_freeze()` | `fetch_update(w → (w & !FREEZING) \| DIRTY)` | AcqRel / Acquire | the frozen image could not be written; records restored to the open delta |
| `supersede() -> Result<SupersedeOutcome>` | `fetch_or(SUPERSEDED)` | AcqRel | `Err` if already SUP (double swap = bug); `was_dirty = prev&DIRTY`, `was_freezing = prev&FREEZING` |
| `try_evict() -> bool` | `compare_exchange(0, SUPERSEDED)` | AcqRel / Acquire | wins ONLY from exact `clean` |
| `is_dirty/is_freezing/is_superseded` | load | Acquire | |

### 4.3 What the loom model asserts (loom-models/src/lib.rs:2050–2287) — and what breaks it

1. `node_state_supersede_never_loses_a_racing_apply` (2058): thread A `mark_dirty`, main `supersede`; assert
   `¬(applied ∧ ¬was_dirty)`; terminal holds; post-terminal `mark_dirty` is `Err`.
2. `node_state_evict_never_wins_against_accepted_dirt` (2088): `try_evict` ∥ `mark_dirty`; assert `applied XOR evicted`;
   if applied then dirty ∧ not superseded, else superseded.
3. `node_state_freeze_swap_conserves_records` (2123): delta modelled as a loom `Mutex<u64>` standing in for the per-node
   RwLock; seed one record; committer applies under the lock; writeback `begin_freeze` + swap under the lock, `end_freeze`
   outside; assert `frozen + open == 2`, `is_dirty() == (open > 0)`, `redirtied ⇒ open > 0`, `¬is_freezing()`.
4. `node_state_at_most_one_freeze_in_flight` (2193): two racing `begin_freeze` on a dirty node ⇒ exactly one `Ok`;
   `is_freezing ∧ ¬is_dirty`.
5. `node_state_forced_freeze_conserves_and_reports_freezing` (2224): the `compact_node_forced` arm (ordinary freeze if
   open delta non-empty else forced), second forced freeze refused, then `supersede` must see `was_freezing`, `end_freeze`,
   `frozen + open == applied`, and `applied ∧ frozen == 0 ⇒ was_dirty`.

Weakenings that FAIL these models (structural — the file records no Relaxed-ordering experiment for this module, unlike
`epoch_core`/`journal_core`; the invariants are decided by RMW atomicity, module doc node_state_core.rs:22–42):
- `mark_dirty` as load-check-then-store (split RMW) ⇒ #1 (apply lands after `supersede` read a clean word → lost update)
  and #2 (both apply and evict win).
- `try_evict` as `fetch_or(SUPERSEDED)` (not a CAS from exactly 0) ⇒ #2 (eviction wins against accepted dirt / an in-flight freeze).
- `begin_freeze` as two RMWs (`fetch_and(!DIRTY)` then `fetch_or(FREEZING)`) ⇒ #3 (a racing apply between them is
  neither in the frozen pile nor flagged DIRTY → "dirty bit must exactly track the open delta" fails) and #4 (two freezers both win).
- `supersede` not returning the pre-state ⇒ #1/#5 (the SMO cannot know to carry the open delta).
Orderings: the loom model runs the exact AcqRel/Acquire pairs shipped; the Zig port must keep RMWs `acq_rel` and
loads `acquire` (the word publishes the delta-swap that `freeze_locked` performs under the lock).

### 4.4 Composition with writeback (charter §5.3 "Writeback") — `CachedNode::freeze_locked` (node_cache.rs:2147–2307) / `NodeCache::append_frozen` (node_cache.rs:4045–4145)

Under the per-node WRITE lock, on the serialized writeback/SMO task:
1. If `guard.frozen.is_some()` ⇒ return it (a frozen delta already awaits I/O). Self-heal tripwire: `FREEZING` latched
   with no frozen delta on a live node ⇒ `kv_freeze_latched_recovered`; `end_freeze()` if overlay empty else `abort_freeze()`.
2. Empty overlay ⇒ `Ok(None)`. Else `begin_freeze()` (refusal → `KvError::FreezeRefused { node_addr, refusal }`).
3. A `Drop` guard arms `abort_freeze()` for every error path until the publish (the 2026-08-19 W-A wedge fix).
4. **Freeze-time shadow-fold**: the overlay is `(key, seq)`-sorted; per key-run keep only the newest `Put`/`Delete` and
   every record after it (Delta-only runs keep everything); dropped count → `META_KV_NODE_FREEZE_SHADOW_DROPPED`.
   `horizon = max seq over the WHOLE overlay` (debug-asserted equal to max over kept records).
5. `frame = encode_bset_frame(layout, node_seq, records, horizon)`; `bset_image = frame[frame_len .. frame_len + bset_len]`
   (reads `bset_len` back from frame bytes [20..24)); the snapshot's base index is extended INCREMENTALLY with it
   (readers see identical content, now serialized) — O(existing + new), because the k-way rebuild was 37 % of the
   serial create path once threshold wakes made freezes frequent.
6. Disarm the restore guard, clear overlay/charges, `guard.frozen = Some(FrozenDelta { records, horizon, frame_len })`,
   `snapshot.store(new)`.
Then OUTSIDE the lock: `append_frozen` reads `(frozen, tail_offset)` under a read lock; `tail + frame_len > node_size`
⇒ `Ok(false)` (compact/split instead); non-solo ⇒ §2.7 probe; `append_bset(...)`; on `Ok(new_tail)` take the write
lock, `tail_offset = new_tail`, `frozen = None`, release promise if the projected log now fits, drop lock, `end_freeze()`;
`NodeFull` ⇒ `Ok(false)`.

**`oldest_dirty_seq`** = `CachedNode::dirty_floor: AtomicU64` (`u64::MAX` = none; node_cache.rs:1456–1461):
`apply_locked` does `fetch_min(entry_start_floor, AcqRel)` for any non-empty apply (:1833 — floors are the containing
journal ENTRY's start seq, design §4.6 pt 2); the checkpoint `take_dirty_floor()` = `swap(u64::MAX)` under the node
write lock IN THE SAME WINDOW as the freeze (so no record slips between), `restore_dirty_floor(f)` = `fetch_min(f)` on a
failed flush (:2050–2059). `journal_tail_seq = min(dirty_floor over all still-dirty nodes, else head)`.

### 4.5 Composition with SMOs (charter §5.3 "SMOs")

Build successors from a frozen snapshot with no locks → barrier images → parent-then-child locks → `take_overlay()`
(the bounded second merge) → `supersede()` each old object (outcome `was_dirty` is what the overlay carried;
`was_freezing` is asserted for freeze-borne sources) → publish successor → apply parent pointer records in ONE
`apply_locked` → retire old objects LAST → reserve inside the window → release → write the entry (design §4.6/§4.6a(b)).

---

## 5. COMPACT / SPLIT / MERGE at the node level

### 5.1 Fold sources — `fold_node_sources(src, extra, durable_tail)` (node.rs:1314–1337)

```
extra_horizon = max(extra.seq) or 0; extra_image = extra.is_empty() ? [] : build_bset(extra, extra_horizon)
views = [parse(extra_image)] (if any) ++ src.bset_views_newest_first()        # newest source FIRST
horizon = max over views.journal_seq_horizon (0 if none)
folded = bset::compact(views, durable_tail)        # n-way merge, per-key compact_fold; tombstone elided iff seq < durable_tail
```
`extra_records` = the caller's frozen dirty delta; must be strictly `(key,seq)` ascending; empty legal.

### 5.2 `compact_node(path, layout, src, extra, dst_addr, dst_node_seq, durable_tail)` (node.rs:1424–1467)

`check_fresh_destination` (aligned ∧ `dst != src.addr` else `Corrupt("CoW violation")`); fold; if non-empty,
`image_len = 4096 + page_align(frame_len + 32 + Σ encoded_len)`; `image_len > node_size` ⇒ `NodeFull` (= the SPLIT
signal — a node's own log alone can never overflow, folding only reclaims pad; reachable only when `extra` folds in);
`write_node` with identity (tree_id, level, min_key, max_key) copied from `src.header`, horizon from §5.1.

### 5.3 `split_node(path, layout, src, extra, left: SplitDest, right: SplitDest, durable_tail)` (node.rs:1486–1586)

Both dests fresh; `left.addr == right.addr` ⇒ `Corrupt`; fold; `folded.len() < 2` ⇒ `Corrupt`.
**Cut** (node.rs:1512–1547):
```
total = Σ encoded_len; cut = len−1; acc = 0
for (i, r): acc += len(r); if acc·2 ≥ total ∧ i+1 < len { cut = i+1; break }
# finding 41 — never cut inside a same-key group (compact_fold emits Put + retained link for a layout chain):
while cut < len ∧ folded[cut].key == folded[cut−1].key: cut += 1
if cut ≥ len: cut = len−1; while cut > 0 ∧ folded[cut].key == folded[cut−1].key: cut −= 1
              if cut == 0: Corrupt("all share one key")
(L, R) = folded.split_at(cut)
```
Left image: `min = src.min_key`, `max = L.last.key`. Right image: `min = key_successor(L.last.key)` (NOT the first right
key — a tighter bound would leave an unroutable gap), `max = src.max_key`. Both carry the SAME horizon; same tree_id/level.
Byte-balance means each fresh half holds > ½C of a fold > C — the hysteresis that makes ¼C the merge trigger.

### 5.4 Merge (node level; the tree-level task is §4.6a) — rule and arithmetic

Two ADJACENT siblings under ONE parent (`R.min == key_successor(L.max)`) are mergeable iff
`f(L) + f(R) ≤ ¾C ∧ min(f(L), f(R)) ≤ ¼C`, with `f(N)` = `NodeSnapshot::fold_bytes_upper_with(&[], ·, durable_tail)`
(K1 fold byte upper bound over log + open + frozen delta, crediting tombstones `seq < tail` as gone).
Why ¾C: `split_part_capacity` — the successor is exactly as full as a fresh split part with a part's headroom.
Why ¼C: `¾C − ½C` — a fresh half (> ½C) never pairs with an underfull node (> ½C + ≤ ¼C > ¾C), so a split is never
undone by the next merge; a merge successor needs > ¼C of growth to split again (node.rs:399–409, design §4.6a(a)).
Successor image: `level = L.level`, `min = L.min`, `max = R.max`, fold of both logs + both frozen deltas via ONE
`bset::compact` over the concatenated newest-first views (key spaces disjoint ⇒ view order immaterial). Parent records:
`Put(R.max → (succ_addr, succ_seq))` + `Delete(L.max)`; entry `[Put, Delete, alloc(succ), free(L), free(R)]`.
Numbers: a separator ≈ 40 B ⇒ a 64 KiB interior is a candidate below ≈ 380 live separators, 256 KiB below ≈ 1,600;
a heavy boundary (`f(P₁)+f(P₂) > ¾C`) needs > ≈ 3,900 live children across two parents at 256 KiB (≈ 960 at 64 KiB).

---

## 6. PUBLIC API (Rust shapes; one line each)

```rust
// node.rs
pub const NODE_PAGE: usize = 4096;  NODE_MAGIC: u32 = "KVND";  NODE_FORMAT_VERSION: u16 = 1;  NODE_HEADER_FIXED_LEN: usize = 40;
pub const BSET_FRAME_MAGIC: u32 = "KBSF";  BSET_FRAME_VERSION: u16 = 1;  BSET_FRAME_LEN: usize = 32;
pub const BSET_FRAME_VERSION_V2: u16 = 2;  BSET_FRAME_V2_LEN: usize = 40;
pub const MIN_NODE_SIZE = 64 KiB; MAX_NODE_SIZE = 1 MiB; DEFAULT_NODE_SIZE = 256 KiB;
pub const RECORD_VALUE_CAP_CEILING = 65_536; XATTR_RECORD_ENVELOPE_MAX = 256;
pub fn xattr_value_cap(node_size) -> usize;              // min(65536, ns/4)
pub fn record_value_cap(node_size) -> usize;             // xattr_value_cap + 256
pub struct FrameStamp { appender_id: u32, g: u32 }       // Default = (0,0) structural stamp
pub struct FrameScreen { g_current, appender_current: Option<u32>, recorded_tail: Option<(u32,u32)>, pr_fenced }
impl FrameScreen { fn is_breach_rule(u8)->bool; fn is_current_own(&self, FrameStamp)->bool; fn foreign_rule(&self, FrameStamp, pos, prev_g: Option<u32>)->Option<u8> }
pub struct NodeLayout { node_size, frame: V1 | V2(FrameStamp) }   // Copy
impl NodeLayout { fn new(ns)->Result; fn new_symmetric(ns)->Result; fn stamped(self, FrameStamp)->Self; fn symmetric_frames()->bool;
  fn frame_version()->u16; fn frame_len()->usize; fn frame_stamp()->Option<FrameStamp>; fn node_size()->usize;
  fn record_value_cap()->usize; fn xattr_value_cap()->usize; fn fold_capacity()->usize; fn split_part_capacity()->usize;
  fn merge_pair_capacity()->usize; fn merge_candidate_capacity()->usize; fn smo_extents_for_parts(fold_bytes, parts)->u64;
  fn split_growth_window()->usize; fn append_frame_len(bytes)->usize }
pub struct NodeHeader { node_addr: u64, node_seq: u64, tree_id: u8, level: u8, node_size: u32, min_key: Vec<u8>, max_key: Vec<u8> }
pub struct NodeWriteParams<'a> { node_addr, node_seq, tree_id, level, min_key: &[u8], max_key: &[u8] }
pub struct AppendDest { node_addr, node_seq, tail_offset: usize }
pub struct WrittenNode { node_addr, node_seq, min_key, max_key, record_count, bytes_written, journal_seq_horizon }
pub struct LoadedNode { header, buf: Bytes, bset_ranges: Vec<Range<usize>>, tail_offset, dropped_tail_bsets: u64, foreign_frames_screened: u64 }
impl LoadedNode { fn header(); fn bset_count(); fn bset(i)->Result<BsetView>; fn bset_views_newest_first()->Result<Vec<BsetView>>;
  fn lookup(key)->Result<Folded>; fn tail_offset(); fn dropped_tail_bsets(); fn foreign_frames_screened(); fn append_dest()->AppendDest;
  fn into_parts()->(NodeHeader, Bytes, Vec<Range<usize>>, usize) }
pub fn key_successor(key)->Vec<u8>;                       // key ‖ 0x00
pub fn encode_bset_frame(&NodeLayout, node_seq_at_write, &[Record], horizon)->Result<Vec<u8>>;   // THE append/write wire format
pub enum TailPage { Clear, Ours, Foreign(FrameStamp) }
pub fn probe_tail_page(page, node_seq, &NodeLayout)->TailPage;
pub fn encode_header_page(&NodeLayout, &NodeWriteParams)->Result<Vec<u8>>;   // the ONE header encoder
pub async fn write_node(path, &NodeLayout, &NodeWriteParams, base_records, horizon)->Result<WrittenNode>;
pub async fn append_bset(path, &NodeLayout, &AppendDest, records, horizon)->Result<usize /*new tail*/>;
pub async fn load_node(path, &NodeLayout, node_addr, durable_tail)->Result<LoadedNode>;
pub async fn load_node_screened(path, &NodeLayout, node_addr, durable_tail, Option<&FrameScreen>)->Result<LoadedNode>;
pub fn verify_node_extent(buf: Bytes, &NodeLayout, node_addr, durable_tail)->Result<LoadedNode>;           // pure half (fuzzed)
pub fn verify_node_extent_screened(buf, &NodeLayout, node_addr, durable_tail, Option<&FrameScreen>)->Result<LoadedNode>;
pub fn residue_seq_ceiling(buf)->u64;
pub async fn compact_node(path, &NodeLayout, &LoadedNode, extra, dst_addr, dst_node_seq, durable_tail)->Result<WrittenNode>;
pub struct SplitDest { node_addr, node_seq }
pub async fn split_node(path, &NodeLayout, &LoadedNode, extra, &SplitDest, &SplitDest, durable_tail)->Result<(WrittenNode, WrittenNode)>;
pub(crate) fn debug_audit_records(tree_id, level, &[Record])   // debug-only typed round-trip audit at the backend's staging boundary

// node_seq.rs
pub const INCARNATION_SPACE_BITS = 38; INCARNATION_SPACE = 1<<38; INCARNATION_ORDINAL_BITS = 25; INCARNATION_ORDINAL_MAX = 2^25 − 2;
pub fn incarnation_base(volume_base, ordinal)->Option<u64>;
pub fn screen_incarnation_base(volume_base, word)->Result<u64>;      // Rejected on bad wire word
pub const NODE_SEQ_INCARNATIONS_KEY = b"node_seq_incarnations"; NODE_SEQ_INCARNATIONS_VERSION: u8 = 1; NODE_SEQ_INCARNATIONS_LEN = 9;
pub fn encode_incarnations(minted)->Vec<u8>;  pub fn decode_incarnations(&[u8])->Result<u64>;
pub struct NodeSeqHandle { next: AtomicU64, ceiling: u64, raises: bool }
impl NodeSeqHandle { fn shared(seed); fn shared_bounded(seed, volume_base); fn incarnation(base); fn next()->Result<u64>; fn raise_to(seq); fn load()->u64; fn ceiling()->u64 }

// node_state_core.rs
pub enum LifecycleState { Clean, Dirty, Serializing, Superseded }
pub struct Superseded;  pub enum FreezeRefused { NotDirty, AlreadyFreezing, Superseded }
pub struct SupersedeOutcome { was_dirty: bool, was_freezing: bool }
pub struct NodeState { word: AtomicU64 }   // Default = new()
impl NodeState { fn new(); fn state()->LifecycleState; fn is_dirty(); fn is_freezing(); fn is_superseded();
  fn mark_dirty()->Result<(), Superseded>; fn begin_freeze()->Result<(), FreezeRefused>; fn begin_forced_freeze()->Result<(), FreezeRefused>;
  fn end_freeze()->bool; fn abort_freeze(); fn supersede()->Result<SupersedeOutcome, Superseded>; fn try_evict()->bool }

// tree.rs (interior value codec, used by this layer's consumers)
pub fn encode_interior_value(child_addr, child_seq)->Vec<u8>;  pub fn decode_interior_value(&[u8])->Result<(u64,u64)>;
```

Error variants this layer emits (mod.rs:952–1012): `ChecksumMismatch { stored, computed }`, `Corrupt(String)`,
`ValueTooLarge { len, cap }`, `FreezeRefused { node_addr, refusal }`, `NodeFull { needed, available }`,
`CheckpointCoveredBsetAfterTear { node_addr, bset_offset, horizon, durable_tail }`, `Rejected(String)` (node_seq screen), `Io`.

Counters touched (mod.rs): `META_KV_NODE_DROPPED_TAIL_BSETS`, `META_KV_NODE_APPENDS`, `META_KV_NODE_APPEND_BYTES`,
`META_KV_NODE_REWRITE_BYTES`, `META_KV_NODE_IMAGES`, `META_KV_FOREIGN_FRAMES_SCREENED`, `META_KV_APPENDER_FENCE_BREACH`,
`META_KV_FOREIGN_FRAME_OVERWRITE_DETECTED`, `META_KV_NODE_PARTITION_REFUSALS`, `META_KV_NODE_FREEZE_SHADOW_DROPPED`,
`META_KV_READER_LOAD_RETRIES`; tripwires `meta_kv_appender_fence_breach`, `kv_freeze_latched_recovered`.

---

## 7. TESTS (name — one sentence; **P** = pure/in-RAM, **D** = needs a device/temp file via uring)

`node.rs` has NO in-module `#[cfg(test)]` block (verified by grep); its unit-level contracts live in `tests/kv_node_tests.rs`.

### tests/kv_node_tests.rs (K2 contracts; D = file-backed 8 MiB "volume")
- `test_node_layout_validates_the_format_knob` **P** — 64 KiB/128 KiB/256 KiB/1 MiB accepted; 0, 4096, 60 KiB, 256 KiB+1, +512, 2 MiB rejected `Corrupt`.
- `test_record_value_cap_is_min_of_ceiling_and_quarter_node` **P** — envelope 256; value cap 65,536/16,384/32,768/65,536/65,536 for 256K/64K/128K/512K/1M; record cap = value cap + 256.
- `test_record_value_cap_enforced_with_typed_error` **D** — 64 KiB cap = 16,640; over-cap refused `ValueTooLarge` on both `write_node` and `append_bset`; exactly-cap passes.
- `test_write_load_roundtrip_preserves_header_and_base_records` **D** — write at a nonzero extent; header fields, 1 bset, 0 drops, `tail == bytes_written`, records byte-equal.
- `test_header_only_node_roundtrip` **D** — empty base: 0 bsets, tail 4096, lookup `Absent`.
- `test_append_bsets_then_load_folds_lww` **D** — small append consumes exactly one page; 3 bsets in order; Δtime folds onto base, tombstone shadows, untouched key is zero-copy `Borrowed`.
- `test_append_full_node_returns_node_full_and_keeps_node_loadable` **D** — 64 KiB node takes exactly 15 one-page appends, then `NodeFull{needed>available}`; all 15 intact, tail == 65,536, 0 drops.
- `test_load_detects_header_corruption_and_misdirection` **D** — flipping header byte 17 ⇒ `ChecksumMismatch|Corrupt`; whole valid image copied elsewhere ⇒ self-address `Corrupt`; zeroed extent ⇒ `Corrupt` (not a tear).
- `test_stale_bsets_from_recycled_extent_do_not_leak` **D** — first life (seq 5, base+2 appends) overwritten by second life (seq 9, base only): 1 bset, 0 drops, old records absent.
- `test_compact_folds_into_fresh_extent_old_untouched` **D** — in-place compaction refused; with `durable_tail=6` tombstone seq 5 elided, seq 8 survives; horizon = 9 (max over sources); source extent byte-identical after.
- `test_split_partitions_key_space_into_two_fresh_nodes` **D** — left==right and dst==src refused; left+right folds concatenate to the source fold; `right.min == key_successor(left.max)`; bounds inclusive.
- `test_compact_overflow_signals_split` **D** — full 64 KiB log + 8 × 4 KiB-value extra ⇒ `compact_node` `NodeFull`; log alone compacts to 15 records; `split_node` absorbs 15 + 8.
- `split_never_cuts_a_same_key_group` **D** — finding 41: a versioned layout chain (Put + retained link on one key) is never cut between its records.

### tests/kv_node_cache_coherence_tests.rs (§4.9a; D unless noted)
- `an_unarmed_cache_never_revalidates_and_never_drops` — unarmed cache is epoch 0 and a stray revalidate drops nothing.
- `arming_seeds_the_epoch_and_the_tail_and_drops_nothing` — arming seeds epoch + durable tail from the mounted record.
- `an_unchanged_ledger_poll_is_inert` — same record ⇒ no advance, no drop, same Arcs.
- `an_advanced_epoch_drops_every_stale_node_including_pinned_roots` — pinning is against eviction, not staleness.
- `a_root_named_identically_by_the_new_record_is_still_dropped` — byte-identical `(addr, seq)` is not a currency proof (appends grow the log).
- `the_reader_lags_by_exactly_one_polled_checkpoint` — consistency model: newer create invisible before, visible after, never backwards.
- `revalidation_absolves_prearm_dirt_and_counts_postarm_dirt` — pre-arm dirt is bootstrap residue (absolved); post-arm dirt counted.
- `the_purge_trigger_fires_once_per_advance_and_never_on_an_inert_poll` — trigger carries the epoch pair exactly once per advance.
- `the_tiered_purge_sink_routes_suspects_through_the_unified_purge` — suspects drain once through the one purge.
- `the_revalidation_cadence_derives_from_the_checkpoint_guarantee` **P** — cadence derived from the ≤ 1 s ceiling; env override wins; malformed keeps derivation.
- `the_poller_states_its_staleness_bound` **P** — bound = poll interval + checkpoint interval, machine-readable.
- `the_reader_api_surface_is_live` — poller, epoch handle, counter snapshot entry points exercised.
- `the_poller_is_due_only_after_its_interval_elapses` **P** — injected clock, no sleeps.
- `revalidate_reader_refuses_an_undeclared_mount` — adopting roots on a write mount is refused loud.
- `a_declared_reader_revalidates_through_the_real_ledger` — end-to-end adopt roots, drop stale, serve everything checkpointed.
- `a_non_authority_appender_may_not_mutate_an_interior_node` — interior population has ONE cacher; refused at `apply_locked` (`META_KV_NODE_PARTITION_REFUSALS`).
- `the_partition_gate_does_not_arbitrate_leaves` — leaves are not arbitrated by the cache.
- `the_partition_gate_is_inert_on_a_solo_volume` — solo gate structurally inert.
- `an_armed_reader_may_not_mutate_any_node` — armed reader mutating fails loud.
- `a_foreign_append_into_our_log_is_refused_instead_of_overwritten` — a peer's frame at our remembered tail ⇒ `probe_tail_page` `Ours` ⇒ `Corrupt`, not overwritten.
- `the_foreign_append_probe_is_inert_on_a_solo_volume` — same shape appends without complaint on solo.

### tests/kv_partitioned_append_tests.rs (journal/bitmap/ledger partitioning — not the node layer; listed for completeness)
- `test_append_partition_legality` **P** — `writers ≥ 1`, `writer_id < writers`, `writers ≤ MAX_APPENDERS` (≥ 2 ledger slots each).
- `test_sub_ring_placement_disjoint_and_solo_is_whole_extent` **P** — regions disjoint/equal/ordered; solo = whole extent.
- `test_partitioned_ring_geometry_preflight` **P** — every sub-ring must hold `MAX_ENTRY_LEN`.
- `test_solo_page_header_bytes_are_unchanged` **P** — solo page header byte-identical (D9).
- `test_two_appenders_both_survive_replay` **D** — two sub-rings both recover; merge carries appender identity.
- `test_merge_is_deterministic_and_preserves_per_ring_order` **P** — replay-twice equality; per-ring order kept.
- `test_foreign_appender_page_is_detected_not_silently_dropped` **D** — `foreign_pages` counted, not a tear.
- `test_partition_violation_same_key_two_writers_refused_loud` **P** — same key from two appenders in one window refused.
- `test_partition_violation_interior_record_from_non_authority` **P** — `level > 0` records only from the root authority.
- `test_partition_violation_alloc_record_for_foreign_extent` **P** — alloc deltas only inside own bitmap partition.
- `test_crash_mid_append_recovers_per_writer` **D** — a torn window in one sub-ring leaves the other whole.
- `test_ledger_slot_ranges_disjoint_and_solo_unchanged` **P** — solo `seq % 32`; partitioned contiguous ≥ 2-slot ranges.
- `test_solo_ledger_slot_bytes_are_unchanged` **P** — suffix-less record byte-identical.
- `test_partitioned_ledger_record_roundtrip` **P** — suffix costs exactly 4 bytes.
- `test_two_checkpointers_do_not_overwrite_each_other` **D** — same checkpoint seq, both records survive.
- `test_partitioned_ledger_loud_refusals` **D** — wrong slot owner / writer_count mismatch / non-authority roots refused.
- `test_pre_partition_record_serves_as_authority` **D** — pre-partition newest record is the authority's.
- `test_partition_map_page_ownership` **P** — bitmap pages interleaved per appender.
- `test_partitioned_claims_stay_inside_their_pages` **P** — no cross-appender extent claim.
- `test_partitioned_claims_over_a_ragged_last_page` **P** — partial last page handled.
- `test_partitioned_reserve_isolation` **P** — per-partition compaction reserve.
- `test_pending_free_gate_is_per_appender` **P** — one appender's tail never releases another's parked extent.
- `test_solo_core_is_unchanged` **P** — solo `ExtCore` verbatim.
- `test_two_appenders_bitmap_pages_do_not_clobber` **D** — both appenders' bits survive a load.
- `test_load_refuses_foreign_alloc_record` **D** — foreign alloc delta refused at load.
- `test_partitioned_append_bit_is_built_but_not_stamped` **P** — bit understood, never set by format/mount.
- `test_unstamped_volume_mount_leaves_the_superblock_unchanged` **D** — `--single-writer` volume mounts with a byte-identical superblock.

### tests/kv_loader_lock_style_tests.rs
- `the_node_caches_scc_tables_are_taken_in_one_acquisition_style` **P** — static rail: `node_cache.rs` carries no `*_async` scc acquisition (sync/async mix self-deadlocked the two-lane `sqz-meta` pool; fsck hung 31 min).
- `hundreds_of_colliding_flat_node_loads_on_the_meta_lanes_never_wedge` **D** (multi_thread, 4 workers) — hundreds of concurrent same-address loads through the meta lanes complete under a timeout.

### Related (outside the four files, named for the port's test plan)
- `tests/kv_tree_tests.rs::node_lifecycle_word_transitions` **P** — single-threaded word semantics: clean→dirty→serializing(+redirty)→dirty; abort restores; evict only from clean; supersede terminal + reports dirt.
- `loom-models` five models (§4.3) **P**.
- `tests/crash_contract_tests.rs::{test_kv_node_torn_tail_bset_dropped_counted_priors_intact, test_kv_node_valid_bset_after_tear_horizon_le_tail_fails_loud, test_kv_node_torn_rewrite_unreferenced_old_node_untouched}` **D** — the §2.4 classifier's three outcomes with `TORN_WRITE_FAULT`.
- `tests/sym_coherence_tests.rs::a_predecessors_pre_handover_frames_are_never_screened` **D** — rule 2's "before the recorded tail" keep.
- Fuzz targets: `fuzz/fuzz_targets/kv_node.rs` (drives `verify_node_extent`), `bset_frame_v2.rs` (constructive v2 logs from `encode_header_page`/`encode_bset_frame`), `kv_bset.rs`.
- node_seq.rs unit tests **P**: `the_space_arithmetic_partitions_the_63_bits_and_refuses_past_them`, `the_handle_mints_inside_its_space_and_refuses_at_the_ceiling`, `the_incarnation_record_round_trips_and_refuses_other_shapes` (node_seq.rs:249–298).

---

## 8. EVERY NUMERIC CONSTANT — with WHY (and the measured numbers quoted in comments)

| constant | value | why |
|---|---|---|
| header page / append grain | 4096 | sector/page atomicity unit; `append_bset`'s tail granularity IS the O_DIRECT sector grain's ceiling (design §4.4 pt 7) |
| header fixed len | 40 | `4+2+1+1+8+8+4+2+2+8` |
| frame len v1 / v2 | 32 / 40 | v2 adds `appender_id u32 + g u32`; 0.2 % overhead per 4 KiB frame (design-symmetric §5.8.2) |
| bset header len | 32 | `4+2+2+4+4+8+8` |
| record header len | 15 | `2+1+8+4` |
| min record len | 16 | header + 1-byte key (keys never empty) — bounds `record_count` (bset.rs:171) |
| node_size range | 64 KiB–1 MiB, default 256 KiB | §1.4 table; sub-256 KiB trades xattr headroom (`ns/4`) for cold-read latency |
| value cap ceiling | 65,536 | `XATTR_SIZE_MAX` |
| xattr envelope | 256 | `1 + XATTR_NAME_MAX(255)` |
| `C` @256 KiB v1 | 257,984 | `262144 − 4096 − 32 − 32` |
| ¾C / ¼C @256 KiB v1 | 193,488 / 64,496 | split part fill / merge trigger |
| C, ¾C, ¼C @64 KiB v1 | 61,376 / 46,032 / 15,344 | |
| `smo_extents_for_parts` extras | +2 | packer rounding + cascade below `split_growth_window` |
| fanout | ≈ 7,000 | `C / ~36 B` interior record ⇒ depth ≤ 3 at 100 M, < 3 MiB interior |
| `RETRY_BUDGET` | 256 | SMOs rare + serialized; more = routing bug; "retry budget exhausted fired ~1/2 bench runs" before the per-restart yield (tree.rs:1274–1285) |
| `PROJECTION_REFRESH_EVERY` | 8 | a racing SMO's window closes in a handful of restarts; small vs the budget |
| `READER_LOAD_RETRIES` | 3 | a reader's whole-extent read racing a single `write_at` append |
| `K` | 38 | 2^38 ≈ 2.7×10^11 mints (200 SMOs/s × 43 y); leaves 25 bits = 2^25 ≈ 33.5 M incarnations (15 k mounts rejoining daily × 6 y) |
| `INCARNATION_ORDINAL_MAX` | 2^25 − 2 | `B + (o+1)·2^38 < 2^64` for every `B < 2^63` |
| incarnations record len | 9 | `version u8 + minted u64` |
| `node_seq_base` mask | `u64::MAX >> 1` | top bit clear ⇒ 2^63 monotonic headroom |
| lifecycle bits | 1 / 2 / 4 | DIRTY / FREEZING / SUPERSEDED |
| default node cache | `max(budget/16, 512 MiB)`/volume; ~2,048 nodes ≈ 4–8 M hot records | charter §5.3; design §3 |
| checkpoint cadence | ≤ 1 s; dirty-node cap 4,096 | bounds mount-replay working set ≤ 1 GiB (design §4.6 pt 2) |
| measured | cold 256 KiB leaf read ≈ 85–120 µs NVMe vs 4 KiB ≈ 20–80 µs | design §4.4 "Read amplification" |
| measured | k-way index rebuild was 37 % of the serial create path | node_cache.rs:2275–2277 (why `extend_with` is incremental) |
| measured | 32–56 × 4 KiB bsets per 256 KiB node ⇒ compaction amortized ≈ 8 KiB/append | design §4.4 |
| measured | separator ≈ 40 B; interior merge candidate below ≈ 380 (64 KiB) / ≈ 1,600 (256 KiB) live separators; heavy boundary > ≈ 3,900 / ≈ 960 | design §4.6a(d′) |
| measured | `inline_tails_cap = (record_value_cap − 7)/12` ≈ 5,400 @256 KiB, 1,386 @64 KiB | design-symmetric §5.8.2 |
| test fact | 64 KiB node = header + exactly 15 one-page appends | kv_node_tests:396 |

---

## 9. DECODER TOTALITY — how a lying length/count is refused

Principle (design §9, node.rs:515–516, bset.rs:155–178): every length is checked against its CONTAINER before use;
checksums are integrity, not authentication — a checksum-valid liar must still be refused without allocation or panic.

Header page (node.rs:517–548): `len == 4096` → magic → version → checksum → `40 + min + max ≤ 4096` → slice.
Frame (node.rs:830–860, :908–914): magic gate → checksum over the version-specific span → version == layout's → seq ==
header's → THEN geometry: `padded_len ≥ 4096 ∧ % 4096 == 0 ∧ ≤ node_size − pos`, `bset_len ≥ 32`,
`frame_len + bset_len ≤ padded_len`. A verified frame failing geometry is `Corrupt` (writer bug/corruption), never a
tear, because a torn prefix breaks the checksum. `probe_frame` is only called with `pos + frame_len ≤ len` (main walk:
`pos < node_size` with `pos` a page multiple and `frame_len ≤ 40 < 4096`; stride scans: explicit `probe + frame_len ≤ node_size`).
Bset (bset.rs:110–219): `len ≥ 32` → magic → version → reserved == 0 → `len == 32 + data_len` → checksum →
`record_count ≤ data_len / 16` (the TEST-4 find: a 32 B bset claiming `u32::MAX` records was a 128 GiB
`Vec::with_capacity` abort) → walk exactly `record_count` records, each `RecordRef::decode` (header fits; kind ∈ {1,2,3};
`key_len ≠ 0`; Delete ⇒ `val_len == 0`; `15 + key_len + val_len` checked-add; fits the remainder) with strict
`(key, seq)` order → `pos == data.len()` (no trailing bytes).
Interior value (tree.rs:156–167): exactly 16 B. Incarnations record (node_seq.rs:138–157): exactly 9 B, version 1.
Node-seq wire word (node_seq.rs:93–117): checked-sub/mod/capacity before install. `incarnation_base` is all checked arithmetic.
`residue_seq_ceiling` probes only slots with `pos + 40 ≤ len` and never trusts geometry (it reads only the stamp).
`LoadedNode::bset(i)` re-parses a range verified at load (cannot fail) — the `Result` only propagates the impossible.

What is NOT checked (port must match, not "improve"): the frame's reserved u16 at [6..8) is written 0 but never
validated; the bset's reserved u16 IS validated; frame pad bytes are not hashed; a numerically higher stale
`node_seq_at_write` is silent `StaleIncarnation` (a loud tripwire would need a generation tag — rejected, node.rs:806–818).

---

## 10. Byte-identity checklist for the Zig port

1. Header: fields in §1.1 order; xxh3 streamed over `[0..32) ‖ 8 zero bytes ‖ [40..4096)`; keys packed at 40 with no gap.
2. Frame: `padded_len = page_align(frame_len + bset_len)`; checksum over 24 B (v1) / 32 B (v2); pad zeros; bset at `frame_len`.
3. Bset: K1 image exactly (`build_bset`), horizon = caller's; `write_node`'s base frame horizon = the caller's
   `journal_seq_horizon` (compaction/split pass the max over sources; `freeze_locked` passes the overlay's max seq).
4. Image for `write_node` = header page ‖ base frame (or header alone); `bytes_written` = tail.
5. Split cut = byte-balanced `acc·2 ≥ total` at `i+1`, then same-key cohesion forward-then-backward; right.min = left.max ‖ 0x00.
6. Load classifier: exact break/stop classes and counter increments of §2; stride scan starts at `pos + 4096`; screened
   stop skips the torn-tail pass; `dropped` includes the torn unit.
7. Lifecycle word: bit values and RMW shapes of §4.2 (one RMW per transition; CAS-from-0 for evict).
8. node_seq: `B` from uuid bytes 0..8 LE with top bit cleared; first mint = base + 1; park at ceiling on exhaustion.
