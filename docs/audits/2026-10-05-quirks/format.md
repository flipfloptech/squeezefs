# On-disk format audit — reference quirks vs charter requirements

Reviewer: design review, read-only. Repo `/Users/joberdorf/Source/squeezefs-zig` at `dev` (rung 2a landed, 2b in flight).
Ruling applied: `docs/decisions/0002-no-compatibility-obligation.md` — "the charter's mechanisms, invariants, laws and
measured derivations" are requirements; "where the reference's code embodies a choice the charter does not require and a
better design exists, we take the better design"; "No on-disk or wire compatibility with the reference"; byte-identity was
"a correctness oracle of convenience, never a requirement" (0002 §Decision 1–2). Compatibility obligations "begin at our
first release, not before" — so every "change now" below is free today and priced in reformat-of-the-fleet later.

Charter clauses used as the ruler (quoted where a row needs them): §1.7 (charter.md:23) "Forward-only on-disk/wire
format … Old binaries refuse new volumes loud. Capability bits mean one complete thing each."; §5.1 (charter.md:106–114)
superblock / ledger / journal / node / keys / record kinds / checksums; §5.4 (138–140) W = 2^16, guest keyspace; §5.5
(146–154) "The forest (KD-SYM-2): one mixed-kind KV tree per routing slot", appender pages, node-seq incarnations; §5.6
(185–189) refs and block map; §12 (charter.md:294) "On-disk and wire structs are `extern struct` with explicit
little/big-endian fields, `comptime` size assertions, and canonical codecs (decode is total; exactly the encoder's form
decodes; every unit carries its xxh3-64)."

Method: read every format module (`key`, `record`, `bset`, `node`, `node_seq`, `journal`, `ledger`, `superblock`,
`geometry`, `alloc_ext`, `format`, `builder`) and the four porting specs; one claim (LEB128 non-canonical acceptance) was
confirmed by a throwaway test run through `zig build test` and the file restored byte-for-byte (`git status` clean).

One meta-recommendation up front (see §3): every "change now" row should land as ONE format act with `version = 4` in
sector 0, so the Rust binary refuses a Zig volume with its existing message "unsupported metadata format version 4 —
upgrade squeezefs" (`kv_alloc_superblock.md:95`) instead of misparsing — that is §1.7's "old binaries refuse new volumes
loud" applied to the reference itself.

---

## 1. Findings

Verdict legend: **NOW** = change before rung 2b builds fixtures on it; **R4/R6** = change at that rung (where the codec
first becomes product-visible); **KEEP** = the reference's choice is good engineering or charter-mandated; **ASK** = owner
decision. "Cost later" assumes a shipped Zig release: every format change = reformat (charter §1.7, no migration).

| # | Item | Where (file:line) | What the reference did | Why it may be poor | What the charter requires | Proposed design | Cost now vs later | Verdict |
|---|---|---|---|---|---|---|---|---|
| 1 | Dual key layouts (legacy kindless vs forest with kind byte) | `key.zig:6-13`, `:152-156` (legacy lens), `:510-517` (forest lens), `:576-589` `forestKey`, `:652-662` `splitForestKey`; `tx.zig:294` `KeyFraming{flat,forest}`; `inos.zig:136,246`; `builder.zig:308-326` re-keys at format | Kept the pre-forest per-kind trees (keys without a kind byte) as the `--single-writer` class and the migration source; the forest key is a transform `forest_key(kind, legacy)` applied at the tree boundary | Two codecs for one fact, a transform on every commit (`tx.zig:430-433`), an `extra = 1 if forest` term in every size equation (`inos.zig:246`), a `digest over (kind, legacy key)` equivalence law that exists only for `enable-symmetric` conversion verification (charter.md:148 last sentence). A single-writer volume loses the ino-major locality (inode ‖ its xattrs ‖ its dentries adjacent) the charter's keys give | charter.md:110 **specifies the keys WITH the kind byte**: "inode `ino ‖ 01`… dentry `parent ‖ 02 ‖ hash54 ‖ coll_seq`… xattr `ino ‖ 03 ‖ …`… block refs `06 ‖ vol_tag ‖ …`… block map `owner ‖ 07 ‖ block_index`". charter.md:146: "The forest (KD-SYM-2): one mixed-kind KV tree per routing slot". The kindless layout appears nowhere in the charter | ONE key layout — the charter's. `--single-writer` = a one-member forest with no appender region/manager plane (exactly what the default image already is minus the directory extent). Delete `legacyKeyLen/legacyKeySlot/splitForestKey`, `KeyFraming`, the builder's `nativeRecords` re-key; per-kind decoders read the charter key directly (`InodeKey = ino_be[8] ‖ kind`) | Now: ~400 lines across key/tx/inos/builder/format + fixtures; the rung-2b backend (mounts only the flat class today, `backend.zig:18,321`) mounts the native slot tree instead. Later: reformat + two key codecs forever | **NOW** (ASK to confirm the single-writer class may be forest-shaped) |
| 2 | Kind byte at offset 8 for ino-major kinds, prepended for refs | `key.zig:9-13`, `:556-560` `forestRouteOff`, `:601-631` `classifyKey` ("first byte ≤ 0x01 because (slot+1)<<40 ≤ 2^56") | Two placements; a 30-line classification ladder keyed on the first byte's magnitude | Looks like an accident; the ladder's soundness rests on `FOREST_SLOT_MAX = 65536` ⇒ ino < 2^57 | charter.md:110 mandates exactly these two placements (`ino ‖ 01` vs `06 ‖ vol_tag`); charter.md:185: "`refcount(block)` = population of the `(vol_tag, block_idx)` prefix"; charter.md:172: "a block's references live in ONE slot tree and `refcount(block)` is one range probe" — refs MUST be by-block, ino-major kinds MUST be by-ino for locality | Keep both placements; keep the ladder but state the invariant as a `comptime` assertion (`(FOREST_SLOT_MAX + 1) << 40 < 0x02 << 56`) instead of prose | — | **KEEP** (charter-mandated; the asymmetry is the refcount range probe) |
| 3 | 57 B inode value: LEB128 version tag + `rdev` in the old `flags2` slot | `record.zig:128-160` (`INODE_VALUE_LEN = 57`, `InodeValueWire{version u8, fields}`), `:133-137` ("`rdev` occupies the word historically named `flags2`") | A varint-prefixed, slot-reused struct grown by accretion | Not an `extern struct` layout (56 fixed bytes behind a 1–10 byte prefix); `flags2` is GONE from the wire while the charter lists it; the varint is non-canonical (row 4) | charter.md:110: "packed inode `{mode, uid, gid, nlink, flags, size, atime, mtime, ctime (i64 ns), flags2}` ≈ 96 B"; charter.md:294 `extern struct`, canonical | Fixed-width `extern struct`, size-asserted: `{version u8, reserved u8, flags2 u16, mode, uid, gid, nlink, flags, rdev (6×u32 LE), size u64, atime, mtime, ctime (3×i64 LE), reserved [4]u8}` = 64 B (the charter's "≈ 96 B" is an estimate — its listed fields sum to 56–60 B; 64 B with a reserved tail is honest; widen to 96 only if `btime`/`generation` are wanted now) | Now: one codec + `InodeDelta.field_table` + ~10 tests; the retired-oracle cross-check in `node.zig:2198-2256` already expects to go. Later: reformat | **NOW** — pull 0002 ledger row 4 forward from rung 4; the inode value has no dependency on the layout record |
| 4 | LEB128 accepts non-canonical encodings | `record.zig:113-122` `decodeVarint` (no "shortest form" check); `:195-200` | Varint decode = sum of 7-bit groups; `0x81 0x00` = 1 | **Verified by test**: a 58-byte value `81 00 ‖ 56 fixed bytes` decodes to the same `InodeValue` as the 57-byte canonical form. Two byte strings decode to one value ⇒ not canonical; the existing test (`record.zig:1607-1609`) only checks `81 01` (= 129, refused by the version gate, not by canonicality) | charter.md:294: "exactly the encoder's form decodes" | Subsumed by row 3 (fixed u8 version). If a varint is ever kept anywhere: refuse a trailing `0x00` group / any encoding longer than `encodeVarint(value)` | Trivial now; a fuzz-finding later | **NOW** (with row 3) |
| 5 | Layout base = Rust `bincode` 1.3 (`LayoutMetadata`) | `record.zig:907-914` ("TRAILING BYTES ALLOWED — bincode 1.3's `deserialize` is `allow_trailing_bytes`"), `:1015-1035` (any map order, later duplicate wins) | A serialization library's default encoding as on-disk format | Non-canonical by construction (0002 ledger row 2); u64 length prefixes on every string; `Option` tags; the Zig fold carries ~190 lines of bincode emulation + fixtures (`record.zig:1811-1930`) that are throwaway | charter.md:294 canonical/total; 0002 row 2 already rules | `extern struct` layout record with a canonical total codec (0002 row 2, rung 4). Add: refuse trailing bytes, refuse unsorted/duplicate map entries, refuse non-UTF-8 at the one decoder | Scheduled; the only cost of waiting is that rung-2b fold/compaction tests pinned to bincode fixtures are rewritten at rung 4 | **R4** (as ruled) — but do NOT add new rung-2b fixtures on the bincode base |
| 6 | Two delta classes under one `Delta` kind, discriminated by payload magic `0x4C31` | `record.zig:12-16`, `:669-672`, `:685-690` `isLayoutDelta`, `comptime` guard `:568-570`; `kv_record_bset.md:819` (§10.9) | Reused `RecordKind::Delta` for a second payload grammar; `0x4C31 & ~0x3FF ≠ 0` keeps the inode decoder from misparsing | Type-by-sniffing; the fold has two arms (`record.zig:1344-1395`) and a "mixed classes ⇒ Corrupt" rule that exists only because the kind byte does not say which class | charter.md:112: ONE algebra `Put(value)`, `Delta(field_mask, fields)`, `Delete`; 0002 row 3 | `Delta(field_mask, fields)` over a per-record-type field table (0002 row 3); the record's key kind selects the table — no magic | Scheduled | **R4** (as ruled) |
| 7 | Record frame has no flags byte; header is 15 B (`u64 seq` at offset 3) | `record.zig:1123-1132`; `kv_record_bset.md:278-279` | `key_len u16 ‖ kind u8 ‖ seq u64 ‖ val_len u32` | Not naturally aligned (harmless with `[N]u8` fields, but every "future bit" needs a format bump); the Zig comment `record.zig:1123-1124` claims "the charter's prose lists one" — **false**: charter.md:112 lists no frame flags (the spec meant the reference's design doc) | charter.md:112 names only the three kinds; §12 `extern struct` (alignment not demanded) | Optional: 16 B header `{key_len u16, kind u8, flags u8 (must be 0), val_len u32, seq u64}` — naturally aligned, one reserved byte. Cost: interior record 36 → 37 B (fanout −2.7 %), refs 46 → 47 B. Either way fix the comment | Free now; impossible later | **ASK** (lean: take the 16 B header — a reserved must-be-zero byte is the only cheap insurance a forward-only format has); fix the comment **NOW** |
| 8 | Magics with mixed conventions: numeric-LE `BSET_MAGIC` ("TESB" on disk) vs ASCII-LE `KVND`/`KBSF`/`KVJP`/`KVRL`/`KVAB`/`KVAP`/`KVAD` | `bset.zig:29-32`; `node.zig:54-55`; `journal.zig:52-53`; `ledger.zig:52-53`; `alloc_ext.zig:36-37`; `geometry.zig:252-255`; `kv_node.md:93-96` | `BSET_MAGIC: u32 = 0x4253_4554` written LE ⇒ bytes `54 45 53 42` | A hex-dump reads every other magic as ASCII and this one backwards; two conventions for one concept | None directly; §12's "explicit endianness" spirit | One rule: every magic is 4 ASCII bytes read as a LE u32 (`std.mem.readInt(u32, "KVBS", .little)`), so on-disk bytes are the ASCII; also a `comptime` table test that no two magics collide | Trivial now | **NOW** |
| 9 | `KBSF` frame v1 (32 B) vs v2 (40 B, `appender_id ‖ g`); a layout admits exactly one; the builder packs forest images with the **v1** budget term | `bset.zig:434-442`, `:493-520` `FrameFormat`, `:606-633`; `node.zig:207-240` (`new` vs `newSymmetric`, `symmetricFrames`); `node.zig:1177,1188` (257,984 vs 257,976); `builder.zig:437-446` `usableBudgetOf` ("the frame term is the v1 32 B on EVERY layout … so a forest image packs byte-identically to the reference") | Added the fencing stamp as a second frame version instead of redefining the frame | Two header structs, two fold capacities, a v1-on-v2/v2-on-v1 "garbage" arm, a `NodeLayout.frame` discriminant threaded through every node API, and a builder constant chosen for byte-identity with the reference (0002 says that is not a requirement). The stamp costs 8 B per frame that is 4 KiB-padded anyway — effectively 0 | charter.md:174: "Detection-grade frame screen on every bset frame (v2: `appender_id u32 ‖ g u32` before the checksum)" — the charter describes the stamped frame as THE frame | ONE frame = the stamped one, renamed v1 of the Zig format; `(0, 0)` is the single-writer/manager stamp. Delete `FrameFormat`, `newSymmetric`, `symmetricFrames`, the version-foreign arm; `usableBudgetOf` uses the real frame length | Now: ~150 lines + capacity tie-tests. Later: reformat | **NOW** |
| 10 | `KBSF` `reserved u16` written 0 but **not checked** on read | `bset.zig:449-450`, `:606-633` (no reserved check), `:1366` test pins the non-check | Reference `node.rs` did not check it | A non-canonical encoding decodes (`§12: exactly the encoder's form decodes`); the bset header's reserved IS checked (`bset.zig:176`) — inconsistent | charter.md:294 | Check `== 0` (it is under the checksum anyway) | Trivial | **NOW** |
| 11 | Journal page header: 24 B `{magic, lap u32, feo u16, pad u16, writer_id u16, pad u16, xxh3[0..16)}` | `journal.zig:16-17`, `:48-56`, `:265-288`; `kv_journal_ledger.md:40-53` | `writer_id` bolted into the old pad at `[12..14)` for the bit-8 partitioned ring; pads covered by the checksum "but not validated" (`journal.zig:277-278`) | (a) `writer_id u16` while frames carry `appender_id u32` (`bset.zig:465`) — width inconsistency; u16 came from `MAX_APPENDERS = 16` (`journal.zig:69-71`), but the forest's capacity is `heap/16 ÷ ring` (charter.md:153) = 819,200 appenders on a 100 TiB volume at 8 MiB rings > 65,535; (b) pads not validated ⇒ non-canonical acceptance; (c) `lap` truncated to u32 (bounded: 2^32 laps × 8 MiB ≈ 36 PB of journal writes — acceptable, state it) | charter.md:106: header `{magic, lap, first_entry_off, pad, xxh3}` — no writer id at all. Under the forest each appender owns its ring segments and "Every carved ring extent is ZEROED before a page names it" (charter.md:153), so foreign pages cannot arise by design | Re-lay the same 24 B: `{magic u32, lap u32, first_entry_off u16, reserved u16 (=0 checked), appender_id u32, xxh3 u64}`. Keep an appender id as a detection-grade screen (same class as the frame stamp), widened to u32. Validate reserved | Trivial now (one encoder, one decoder, replay `expect_writer` type) | **NOW** |
| 12 | xxh3 over `[0..16)` only (page header) | `journal.zig:273,282` | Header checksum excludes the data area | Looks thin | charter.md:106 "checksummed 24 B header"; entries carry `xxh3(payload)` (charter.md:106) | Nothing — the data area is written by several reservations (owned by different entries) after the header; it cannot be under the header's digest; entries cover it | — | **KEEP** |
| 13 | `seq ≡ logical ring position` in the entry header | `journal.zig:19`, `:466-473` `frameEntry(start…)`, `:824-828` `if (seq != pos) return .fail` | The entry's `seq` field IS its start position; record seqs = `position + seq_offset + i` | Naming: "seq" in the header is a position; in records it is a seq | charter.md:116: PAD "checksummed, `seq == position`"; charter.md:154: "every ring stamps `position + seq_offset`" | Keep the identity (it gives stale-lap rejection with no lap field in the entry and makes every entry self-locating). Rename the header field `pos` in the Zig struct for honesty | — | **KEEP** (rename only) |
| 14 | PAD entry = ordinary entry with payload `FF 00…00`; detected by sniffing the payload | `journal.zig:63-65`, `:475-479`, `:842-845`; `kv_journal_ledger.md:108-121` | No entry type field; `untag(0xFF) = (15,15)` keeps a pre-pad decoder from mis-tagging | Type-by-content; the all-zero scan is O(pad) ≤ 4117 B | charter.md:116 **specifies it**: "an explicit PAD entry (checksummed, `seq == position`, payload tag 0xFF + zeros, skipped as a chain link by replay)" | Keep. (If row 7's reserved byte lands in the ENTRY header too — `{pos u64, len u32, kind u8, reserved[3], xxh3 u64}` = 24 B, naturally aligned — PAD becomes `kind = 1` and the sniff goes; the charter's wording would need a one-word amendment) | Free now if done with rows 7/11; the charter text is the obstacle | **KEEP** (ASK whether to align the entry header to 24 B with a kind byte) |
| 15 | `0xFFFF` as `first_entry_off` "none" sentinel | `journal.zig:54-56`, `:486-490`, `:943-948` | Physical in-page offset with an impossible value | None | charter.md:118: "resync at the next verified page header with `first_entry_off ≠ 0xFFFF`" — charter-specified | Keep; the replay already treats any other impossible value (<24 or ≥4096) as a hole | — | **KEEP** |
| 16 | Ledger record tail decoded "by LENGTH" into one of four shapes (nothing / 4 B partition suffix / stamp / stamp + suffix) | `ledger.zig:17-24`, `:207-223` (`membership_stamp: ?`, `append_partition: ?`), `:316-320`, `:330-332`, `:370-374`, `:409` | Optional trailing sub-records disambiguated by residual byte count | Grammar-by-length is fragile (a future 4-byte field is unrepresentable), the two optionals are both vestiges: the stamp is present on EVERY volume ("EVERY format is a stamped set member now", `kv_alloc_superblock.md:157`) and the partition suffix is bit 8's (row 20) | charter.md:106: record `{seq, per-tree roots (tree_id, addr, node_seq), journal_tail_seq, next_ino, bitmap_generation, membership stamp}` — the stamp is a plain field, there is no partition suffix | ONE fixed layout: `{version u16, n_roots u16, journal_tail_seq, next_ino, bitmap_generation, node_seq_watermark, stamp (always), roots[]}`; drop the suffix. Note `node_seq_watermark` is NOT in the charter's list but is load-bearing (Finding A, `ledger.zig:26-29`) — keep it and amend the charter | Now: ~60 lines in `ledger.zig` + `checkpoint_cycle.zig:806,980` nulls. Later: reformat | **NOW** |
| 17 | Ledger `routing_width u32` stored in the stamp ("never re-derived at mount") | `ledger.zig:183-185`; `geometry.zig:197-198` | Carried from the era when W was chosen (bit 6 exists for that transition) | A frozen format law stored as data can only disagree with the law | charter.md:138: "`W = 2^16 = 65,536` … DERIVED (the u16 slot-id wire type), frozen forever" | Drop the field, or keep it as a tripwire (`≠ 65536 ⇒ Corrupt`) — I'd drop it; the version is the law | Trivial | **NOW** (with row 16) |
| 18 | Ledger `SlotRun` accepts `stride == 0` singletons while "a singleton's stride is canonically 1" | `ledger.zig:113-118`, `:145` (`stride == 0 and count > 1 ⇒ Corrupt` only) | Lenient decode | Two encodings of one set | charter.md:294 canonical | Refuse `stride == 0` outright | Trivial | **NOW** |
| 19 | Superblock 152 B: `checksum` at 120, then `sb_generation` at 128 and `appender_dir` at 136 appended after it; `appender_dir` written only when non-zero; `Sector0Fixed` uses native-endian `u32/u64` fields | `superblock.zig:24-39`, `:43-58`, `:225-246` (`if (!self.appender_dir.isNone()) writeExtent`), `kv_alloc_superblock.md:15-38` | Two fields bolted on after the checksum field to keep pre-DUR-5 / pre-bit-17 images byte-identical | Bolt-on layout; the `if` is dead logic over a zeroed buffer (kept only for byte-identity); `Sector0Fixed` is "declared for size/offsets" but not the §12 explicit-endian struct the rest of the tree uses (`[N]u8` fields) | charter.md:106 lists the fields (incl. `features_ro`, "random `hash_seed`", "a DUR-5 backup copy"); §12 extern struct with explicit endianness | Re-lay: `{magic[8], version u32, node_size u32, features_incompat u64, features_ro u64, sb_generation u64, volume_len u64 (NEW, row 21), uuid[16], hash_seed u64, root_ledger, journal, alloc_bitmap, heap, appender_dir (5×16), checksum u64}` = 160 B, checksum last; `[N]u8` LE fields; always write every field | Trivial now | **NOW** |
| 20 | 18 incompat bits, of which 15 are set by every default format; bits 0/1/6 always required; bits 2+4 set together; nine "mw" bits "stamped as ONE act"; bit 11 means "the other eight are set" | `superblock.zig:124-144` table, `:169-170` `multi_writer_format_bits`, `:157-158` ("bit 11 set ⇒ all nine set"), `:173` known mask; `geometry.zig:140-160` (`0x2FFD7`) | Bits accreted one per feature PR; migration gates (`required`, `required_when_known`) for volumes that predate a mechanism | A bit that is always required is the VERSION, not a capability; a bit that means "eight other bits" is not "one complete thing"; two bits that are always set together are one thing; bit 8 (partitioned append — ring/bitmap/ledger split by `writer_id`) is the pre-forest multi-writer model the charter's appender rings + extent grants replaced (charter.md:153,158) — it drags `AppendPartition`, `partitionRing*`, `ledgerSlotFor`, `PartitionMap`, `MAX_APPENDERS = 16` (~135 refs across `journal/ledger/alloc_core/alloc_ext.zig`) | charter.md:23: "Capability bits mean one complete thing each"; charter.md:146: "Incompat bit 17 = the forest + appender directory" (one thing — good); charter.md:188–189 name bits for the block-map tree and layout deltas | `version = 4` absorbs every always-on bit (0,1,2,4,6,7,9,10,11,12,13,14,15 and — with row 1 — 17). Real capability bits that can legitimately differ per volume or flip later: `SYMMETRIC_PLANE` (absent on `--single-writer`), `LAYOUT_DELTAS` (stamped before the first delta), `BLOCK_MAP_TREE` (stamped at first crossing), `VOLUME_LIFECYCLE`. Four bits, each one thing. Delete the bit-8 partition machinery (dead code under the charter, §1.9 "No dead code") | Now: table + tests + the partition code removal (sizeable but mechanical). Later: reformat and a permanent 18-bit museum | **NOW** for the table; **ASK** to confirm bit 8's machinery is not wanted by rung 6 (lean: delete — the charter's appender model has no page-interleaved bitmap ownership) |
| 21 | No `volume_len` in the superblock; the DUR-5 backup is found at `backup_offset(device length)` at mount | `kv_alloc_superblock.md:111-113` (`backup_offset(volume_len)`, filter `off ≥ heap.end`); `geometry.zig:86` (`usable = volume_len − 4096`) | Backup location derived from the device size the mount observes | A grown device (LVM extend) hides the backup (it is then mid-device); the `off ≥ heap.end` filter is a patch | charter.md:106 "a DUR-5 backup copy" (location unspecified) | Store `volume_len` (or a `backup_sb` extent) in sector 0 (row 19's layout has the slot). Tail placement itself is right (distance from sector 0 is what protects against partition-table/`mkfs` collateral) — keep it, and fix the WHY comment in the spec (`kv_alloc_superblock.md:515`: "no reserved space beside sector 0 on any existing volume" is the compat reason, not the engineering one) | Trivial now | **NOW** |
| 22 | `hash_seed = xxh3(set_uuid)` | `geometry.zig:234-238`; `kv_alloc_superblock.md:165,525` | Derived from the set uuid to be set-wide without a new wire field | The charter says "random"; a deterministic function of an IDENTIFIER (uuids are shown in URIs, `.stats`, CLI output, membership records) is not an independent secret — anyone who learns the set uuid can precompute colliding names; the "same secrecy class as the per-volume seed (plaintext in sector 0)" argument conflates "readable from the raw device" with "printed everywhere" | charter.md:106: "random `hash_seed`"; charter.md:110: "seeded so names cannot be chosen to collide" | Mint an independent random u64 at SET creation; every member's superblock carries the same value (set-wide property preserved; a migrated slot's hashes still resolve). `format` takes it as an input like `set_uuid` | Trivial now (one more input to `FormatOptions`) | **NOW** |
| 23 | Node header page: 4 KiB page, 40 B fixed + variable `min_key`/`max_key` (u16 lengths), checksum over the whole page with the field zeroed; `node_size` self-description | `node.zig:54-58`, `:320-345`, `:372-399`, `:404-427` | As described | u16 key lengths are over-wide (keys ≤ 36 B) — harmless; ~4 KB of the page is zero | charter.md:108: header page `{magic, addr, node_seq, tree_id, level, min_key, max_key, version, xxh3}` | Keep. `node_size` and the self-address are good misdirect guards; min/max in the header back the finding-41 guard and the revalidation predicate. (A later measured optimization could pack the first frame into the header page's remainder — 1.6 % of a 256 KiB node — not now) | — | **KEEP** |
| 24 | Bitmap page `KVAB`: 24 B header `{magic, page_index, generation, xxh3}`, 32,576 bits/page, A/B slots at `base + i×8192 (+4096)` | `alloc_ext.zig:24-37`, `:77-84`, `:172-195`; `kv_alloc_superblock.md:272-286` | 4072 × 8 = 32,576 = 509 × 64 (word-aligned pages) | "Why that number" — it is derived: page 4096 − header 24, and it happens to be a multiple of 64 | charter.md:106: "allocator bitmap (1 bit per 256 KiB heap extent, A/B page pairs `{generation, xxh3, bits}`)" | Keep; add the derivation as a `comptime` assertion (`page_bits % 64 == 0`) which `alloc_ext.zig:33-35` states in prose. `page_index` is a good misdirect guard the charter does not list — keep | — | **KEEP** |
| 25 | Alloc deltas and DATA-alloc deltas share tag/kind 4, told apart by key LENGTH (8 vs 16) | `alloc_ext.zig:43-51` ("A DATA allocation delta shares the kind under a `vol_tag`-prefixed 16-byte key; the heap allocator's replay skips it"); `kv_alloc_superblock.md:302` | One tag for two record families | Grammar-by-length again (cf. row 16); kind 5 `TREE_BACKPTR_RESERVED` sits unused | charter.md:172: data-bitmap "mutations are `vol_tag`-prefixed kind-4 deltas" — the charter itself says kind 4 | Reserve a distinct kind (5) for data-alloc deltas; amend the charter's one word. Define now, land with the data bitmap | Free at rung 6 if decided now | **R6** (decide now, record in the spec) |
| 26 | `heap_start` aligned to `node_size` (stronger than the 4 KiB the validator demands); validator also accepts the migrator's `triple_within` geometry | `geometry.zig:85` ; `superblock.zig:286-307` (`triple_within = heap.start ≤ ledger.start …`) | Plan aligns strongly; decoder accepts weakly (plus a geometry only the retired v2→v3 migrator produced) | Decoder arm that no Zig writer can produce = dead decoder logic; the weaker validator admits node addresses that are not `node_size`-aligned absolutely | charter.md:106 (no rule); §1.9 no dead code | Keep the strong alignment (extent index = `addr / node_size`, `addr % node_size == 0` is a free misdirect check; waste ≤ node_size − 4 KiB); make the validator REQUIRE it and delete `triple_within` | Trivial | **NOW** (validator only) |
| 27 | `ledger@4096`, `journal@135168`, bitmap after the journal, over-estimated bitmap (`volume_len / node_size`) | `journal.zig:87-101`; `geometry.zig:63-105` | Sequential placement; bitmap sized for `volume_len/node_size` to break the bitmap↔heap circularity | None real — the superblock names every extent, so none of these is a format law; the over-estimate wastes ≤ a few pages | charter.md:106 lists the structures in this order | Keep. Remove the `comptime assert(JOURNAL_START == 135168)` style pins (`journal.zig:100`) in favour of "the superblock's extent" — the constant is a plan choice, not a law | — | **KEEP** |
| 28 | `node_seq` incarnation scheme `K = 38`, `[B + o·2^38, …)`, `B = uuid[0..8] LE & (2^63 − 1)` | `node_seq.zig:24-35`, `:47-49`; `geometry.zig:243-245` | As described | A reviewer might call K = 38 free-floating | charter.md:154: "Node seqs are incarnation-scoped: base `B` from the volume uuid; … a joiner mints in `[B + o·2^38, B + (o+1)·2^38)` (2^25 incarnations)" — charter-specified, with the derivation on the line (`node_seq.zig:24-27`: 200 SMOs/s × 43 years; 15 k mounts rejoining daily × 6 years) | Keep | — | **KEEP** |
| 29 | `u32 block_index` in block-map and ref keys ⇒ 16 PiB per file; comment says "16 EiB" | `key.zig:162-165` ("u32 indices address 16 EiB at 4 MiB blocks"); `kv_record_bset.md:762` | u32 index, `u32::MAX` reserved as the map-blob sentinel | 2^32 × 4 MiB = 2^54 B = **16 PiB**, not EiB — the WHY comment is off by 1024× (the charter has it right) | charter.md:188: "Ceiling 16 PiB at 4 MiB blocks (`EFBIG` at `u32::MAX`)" — a stated format law | Keep u32 (charter); fix the arithmetic in the comment and the spec | Trivial (comment) | **KEEP** (fix comment **NOW**) |
| 30 | `u16` slot id, `W = 65,536` | `key.zig:473-476`; `ledger.zig:113-118` | u16 wire type | — | charter.md:138: derived from the u16 wire type, frozen forever | Keep | — | **KEEP** |
| 31 | Journal `MAX_ENTRY_LEN = 128 KiB` with WHY "2× the largest legal record (a 64 KiB xattr) plus framing" | `journal.zig:59-62`; `kv_alloc_superblock.md:512` | 131,072 B cap | The WHY does not hold: `recordValueCap(256 KiB) = 65,792` (`record.zig:300-302`) + 15 + key ⇒ one record ≤ 65,843 B; two = 131,686 > 131,052 payload cap. A tx with two max-value Puts cannot commit; fine as a law, wrong as a derivation | charter.md:106: "max 128 KiB" — charter-specified | Keep 128 KiB; restate the WHY ("one max record + its refs/Δs with margin; a 128 KiB entry is one page-aligned run on a 4 KiB-grain ring") and add a tie test that ONE max record + framing fits | Comment only | **KEEP** (fix comment **NOW**) |
| 32 | `format_config` JSON xattr on ino 1 (`user.squeezefs.format_config`) written by format; reference fsck REQUIRES it | `format.zig:56-59,122`; `builder.zig:34`; `backend.zig:925-932` (test JSON carries `meta_routing_width`, `meta_slot_runs`, `meta_volumes`…); 0002 row 8 | A CLI config blob stored as a user xattr on the root inode and used as a mount bootstrap | (a) It is in the `user.` namespace — a user can `setfattr`/`removexattr` it; (b) it duplicates the membership stamp (routing width, slot runs, members) — two homes for one truth; (c) JSON parsing on the mount path. The Zig mount does not read it (only the test writes it so the reference oracle accepts the image) | charter.md:143: "order-independent mount discovery via per-volume membership stamps with URI-disagreement refusals" — the stamp is the discovery mechanism; 0002 row 8 | Stop writing it at format. If the CLI wants a set description on the volume, make it a tree-0 control record (`set_config`) or a `trusted.`-class reserved xattr, never a mount or fsck requirement | Trivial now (and the oracle retires with the version bump) | **NOW** |
| 33 | Tag byte `tree_id | level << 4` on every journal record, redundant with the key's kind byte for slot-tree kinds | `key.zig:78-98`; `inos.zig:124-126` (a disagreeing pair refuses the mount) | Pre-forest: the tag WAS the only kind | 1 byte/record of redundancy | charter.md:110 (keys carry the kind); charter.md:116 (PAD tag) | Keep — the redundancy is a free tripwire, and the tag is still the ONLY discriminator for kind 0 (interior, needs `level`), kind 4 (alloc deltas, 8 B BE keys), kind 8 (control, string keys) | — | **KEEP** |
| 34 | Appender-0 page layout: `layout_version` at byte 55 between state flags; `home_volume u8` | `geometry.zig:291-302` (`out[55] = layout_version`); `kv_alloc_superblock.md:548-574` | Byte-exact reference page | `home_volume u8` caps a set at 256 metadata volumes while `member_position/member_count` are u16 (`ledger.zig:182-183`) — an underived limit; the version byte buried mid-struct | charter.md:153 lists the page's fields incl. "`home_volume`… a layout version byte" (no widths) | Re-lay at rung 6 when the page codec lands: version right after the magic, `home_volume u16`, `appender_id u32` consistent with frames | Free if the spec is amended now | **R6** (fix the spec now) |
| 35 | Decoder totality elsewhere: bset header `reserved` checked; record count bounded by `data_len/16`; ledger lengths checked before touch; superblock version gate before checksum | `bset.zig:176,181-189`; `ledger.zig:333-347`; `superblock.zig:257-264` | Good | — | charter.md:294; charter.md:281 "A lying length/count is never an allocation authority" | Keep; the remaining non-total spots are rows 4, 10, 11(b), 18 | — | **KEEP** |
| 36 | Ledger roots `(tree_id u8, addr, seq)` with `tree_id = 0` (= `KIND_INTERIOR`) naming "the native slot tree" and `8` naming tree 0 | `ledger.zig:105-110`; `geometry.zig:347` (`forest_trees = [(8, control_empty), (0, native)]`); `key.zig:31-33` ("also the node header `tree_id` of every slot-tree node") | The interior-record kind byte 0 doubles as the header `tree_id` of every slot tree and as the ledger's name for the native one | One byte with three meanings (record kind, node-header class, ledger root identity); with 65,537 slot trees the ledger can only ever name two roots, and "0" tells a reader nothing about WHICH slot | charter.md:106: roots are `(tree_id, addr, node_seq)`; charter.md:146: "fixed ledger names tree 0 + native slot tree" | Under row 16's fixed layout name roots as `(role u8 {control = 1, native_slot = 2}, slot u32, addr, seq)` or simply two named fields `control_root`, `native_root` (the charter says exactly two roots live here). Node headers keep `tree_id` as a class byte {control, slot} | Trivial with row 16 | **NOW** (with row 16) |
| 37 | Stamp `native_slot_plus1 u32` (0 = none) | `ledger.zig:186-190` ("a u16 plus1 would overflow at slot 65535"), `:303-304`, `:420-423` | Option encoded as `value + 1` in a wider integer | Works, but under one layout every volume is a stamped member whose native slot = `member_position` (`geometry.zig:224`) — the field is derivable; if kept, `present u8 ‖ slot u16` is the honest encoding | charter.md:149: the native slot is the manager's and "holds control records and `/`'s dentries" | Drop (derive from `member_position`) or encode `{present u8, slot u16}` | Trivial with row 16 | **NOW** (with row 16) |
| 38 | Appender directory: header page is the LAST page of the extent | `geometry.zig:304-313` (`dirHeaderOffset = extent.end() − 4096`), `:316-326`; `kv_alloc_superblock.md:575-578` | Pairs at `dir.start + pair × 8192`, header at the end | A reader must know `node_size` to find the header (it does — from sector 0 — so this is a convention, not a bug); every other unit puts its header first | charter.md:153: "the appender directory (an extent chain named by the superblock)" — no layout | Header page FIRST, pairs from `dir.start + 4096`; purely conventional | Free at rung 6 (the directory codec lands there; today only the empty header is written) | **R6** (fix the spec now) |
| 39 | Block-ref value 4 B `{version u8, flags u8, reserved u16}`; block-map values `version u8 ‖ kind u8 ‖ …`, incl. the `STRING` kind carrying a decorated block key verbatim and the `kvmap:1[;sweep:K][;gen:N]` head sentinel as text | `record.zig:343-388`, `:391-545`; `key.zig:162-165` | Per-value version bytes; one string-typed kind | A per-record version byte is 1 B/ref of overhead; strings in a binary format | charter.md:110: refs value "`{version, flags(bit0 indirect blob, bit1 SHARED), reserved}`", block map "POINT/RUN/STRING/POINT2/RUN2 values"; charter.md:188: "head sentinel `kvmap:1[;sweep:K][;gen:N]` … sticky"; charter.md:203: the decorated form "`[backend://]offset[@incarnation]:rel_off:len`" is the data plane's mapping grammar | Keep — all charter-specified; the decoders are already exact-length per kind and refuse unknown bits/kinds (`record.zig:376-387`, `:542-544`) | — | **KEEP** |
| 40 | Dentry value `child_ino u64 ‖ file_type u8 ‖ name_len u8 ‖ name`; xattr value `name_len u8 ‖ name ‖ value-as-remainder` | `record.zig:216-278`, `:280-337` | Compact, length-prefixed names, the xattr value as the remainder (the record frame carries the total) | None | charter.md:110: dentry `{child_ino, type, name}`, xattr `{name, value ≤ min(64 KiB, node_size/4)}` | Keep; `NAME_MAX = 255` is Linux's | — | **KEEP** |
| 41 | Reserved tree ids 4 (`ALLOC_RESERVED`, used by alloc deltas), 5 (`BACKPTR_RESERVED`, unused), 9 (`SHARED_INDEX`); tag nibble bounds ids at 15 | `key.zig:37-57`; `journal.zig:296-299` | Historical table with holes | Holes are fine; the nibble bound is a format law to state (it is: `comptime assert(TREE_ID_MAX <= 0x0F)`, `key.zig:53-56`) | charter.md:110: "tree 0 control `TREE_CONTROL = 8`; shared index kind 9 reserved" | Keep; give kind 5 to data-alloc deltas (row 25) rather than minting a new id | — | **KEEP** |

---

## 2. Details and evidence for the load-bearing rows

### Row 1 — the legacy layout is the quirk; the forest key IS the charter's key
`key.zig:6-13` presents the two layouts as peers ("Two layouts exist on disk"). But charter.md:110 writes every key WITH
the kind byte in the forest's position, and charter.md:146 says the forest is "one mixed-kind KV tree per routing slot".
The kindless layout survives for two reasons that are both non-requirements: migration from pre-forest volumes (0002 §2:
no compatibility owed) and the `--single-writer` class (charter.md:147 "the one flat opt-out" — an opt-out from the
symmetric PLANE; nothing in §5.7 asks for per-kind trees). Concrete costs today: `tx.zig:430-433` frames every staged key
on a forest volume; `inos.zig:246` carries `extra = if (framing == .forest) 1 else 0` into every entry-size equation;
`builder.zig:308-326` re-keys records at format; `key.zig:652-662` exists only to undo the framing. The rung-2b backend
currently refuses the default image (`backend.zig:321`) and mounts only the flat class — i.e. the class the charter calls
the opt-out is the only one the port can mount. One layout removes a whole axis from rung 2b before it hardens.

### Rows 3–4 — the inode value is neither fixed-width nor canonical
`record.zig:151-155` wraps a 56 B `extern struct` behind a one-byte "varint" and `record.zig:113-122` decodes LEB128 by
summation. Probe (run via `zig build test -Dtest-filter=…`, file restored): `[0x81, 0x00] ‖ canon[1..]` (58 B) decodes to
the identical `InodeValue`; asserting `error.Corrupt` on it FAILS. Charter.md:294: "exactly the encoder's form decodes".
0002 row 4 already rules for a fixed-width `extern struct` "size-asserted (rung 4, with the layout record)" — the inode
value has no dependency on the layout record; waiting lets rung 2b (node_cache, tx, inos, economy, checkpoint) pin fixtures
on 57 B values that will be rewritten anyway.

### Row 9 — one frame
`bset.zig:493-520` makes the frame version a layout property; `node.zig:207-240` threads it through `new`/`newSymmetric`/
`stamped`/`symmetricFrames`/`frameLen`; `node.zig:1177` vs `:1188` pin two fold capacities (257,984 / 257,976);
`builder.zig:437-446` deliberately uses the v1 32 B term on forest layouts so the image is byte-identical to the
reference. Charter.md:174 describes the stamped frame as the frame screen's input on "every bset frame". A single-writer
volume writes `(0, 0)` — the manager's structural stamp — and loses nothing. The 8 B stamp is inside a 4 KiB-padded unit.

### Row 11 — `writer_id u16` is a legacy width
`journal.zig:69-71`: "32 ledger slots / 16 = 2 slots per appender" ⇒ `MAX_APPENDERS = 16` ⇒ u16 is generous for 16.
Under the charter the appender count is `heap/16 ÷ ring` (charter.md:153) and appenders are identified by `appender_id
u32` in frames (`bset.zig:465`) and in the appender page (`kv_alloc_superblock.md:549`). Two widths for one identity.

### Rows 16–18 — the ledger record should be one layout
`ledger.zig:330-332`: "The tail after the roots is one of exactly four shapes decided by LENGTH". The stamp is present on
every volume the Zig format can produce (`geometry.zig:140-143` always sets bits 2|4; `kv_alloc_superblock.md:157`
"EVERY format is a stamped set member now"), so `membership_stamp: ?` is never null in practice; `append_partition` is bit
8's. `routing_width` (`ledger.zig:183-185`) stores a constant the charter freezes (charter.md:138). `node_seq_watermark`
is absent from charter.md:106's list but is load-bearing (`ledger.zig:26-29`, Finding A) — a charter gap to amend, not a
quirk to remove.

### Row 19–21 — sector 0
`superblock.zig:24-39`: checksum at 120, then two fields at 128/136 appended after it so older images stay byte-identical
(`kv_alloc_superblock.md:35-37`: "The encoder writes `appender_dir` only if `start != 0 || len != 0` … so sector 0 is
byte-identical to the pre-bit-17 image"). `superblock.zig:43-58` declares `Sector0Fixed` with native-endian `u32/u64`
fields — unlike `key.zig`/`record.zig`/`bset.zig`, which use `[N]u8` with explicit `readInt(.little)`; §12 asks for
explicit endianness in the struct. The DUR-5 backup is located from the device length at mount
(`kv_alloc_superblock.md:111`), so the superblock does not know where its own backup is; 8 bytes fix it.

### Row 20 — the feature word, bit by bit
`geometry.zig:154-155`: the default word is `0x2FFD7` — bits {0,1,2,4,6,7,8,9,10,11,12,13,14,15,17}: fifteen bits set on
a FRESH volume. `superblock.zig:124-144` classifies them: `required` (0, 1), `required_when_known` (6), `format_time`
(2, 4, 7–15, 17), `stamped_later` (3, 5, 16). Only the `stamped_later` class and bit 17's presence/absence
(`--single-writer`) are capabilities in §1.7's sense ("one complete thing each"); `required` bits are the version;
`format_time` bits that every format sets are the version too; bit 11 is defined as "the other eight" (`superblock.zig:157`).

| bit | name (`superblock.zig:125-143`) | what it gates | disposition in a v4 word |
|---|---|---|---|
| 0 | `KV_V3` | the v3 node layer; decoder refuses without it | → version |
| 1 | `NODE_SEQ_WATERMARK` | ledger carries the mint watermark; refuses without it | → version (the watermark is a plain field, row 16) |
| 2 | `KV_GUEST_SLOTS` | ledger carries the membership stamp | → version (every volume is stamped, row 16) |
| 3 | `KV_VOLUME_LIFECYCLE` | set has lifecycle history; stamped at first lifecycle commit | **capability** (stamped later) |
| 4 | `KV_SLOT_MIGRATION` | VL5b stamps + guest-keyspace records; always with bit 2 | → version (bit 2 and bit 4 are one thing) |
| 5 | `KV_LAYOUT_DELTAS` | volume may carry layout `Delta` records; stamped before the first | **capability** (stamped later) |
| 6 | `KV_DYNAMIC_ROUTING` | stride runs over W = 2^16; refused when absent | → version (W is frozen, charter.md:138) |
| 7 | `KV_DURABLE_TERM` | `writer_claim` carries a term | → version (the D0 ladder is mandatory, charter.md:191) |
| 8 | `KV_PARTITIONED_APPEND` | ring/bitmap/ledger split per `writer_id` | **delete** — superseded by appender rings + extent grants (charter.md:153,158); see below |
| 9 | `KV_BLOCK_REFCOUNTS` | durable refs (kind 6) | → version (charter.md:185: refs are THE refcount mechanism) |
| 10 | `KV_WRITER_SCOPED_STAGING` | staged keys carry a writer scope | → version |
| 11 | `KV_MULTI_WRITER_DATA` | "bit 11 set ⇒ all nine set" | → version (a summary bit is not "one complete thing") |
| 12 | `KV_INO_LANES` | `slot = (ino − 2) % W` | → version (charter.md:138 states it as THE routing law) |
| 13 | `KV_BLOCK_KEY_INCARNATION` | block keys `offset@incarnation` | → version |
| 14 | `KV_CLAIM_SET` | `claim_set` record at ino 1 | → version |
| 15 | `KV_LAYOUT_VERSIONS` | layout deltas carry `(base_version, version)` | → version |
| 16 | `KV_BLOCK_MAP_TREE` | kind-7 tree for PB-class files; stamped at first crossing | **capability** (stamped later; charter.md:188 "incompat bit") |
| 17 | `KV_SYMMETRIC_FOREST` | forest + appender directory; absent on `--single-writer` | **capability**, renamed `SYMMETRIC_PLANE` — under row 1 the FOREST layout is universal; what differs is the appender region + manager plane |

Four capability bits; everything else is `version = 4`. Bit 8's partitioned ring/ledger/bitmap (`journal.zig:205-249`,
`ledger.zig:453-470`, `alloc_ext.zig:209-234,301-312`, `alloc_core.zig` `PartitionMap`) is the pre-forest multi-writer
design; the charter's writers have their own ring segments (charter.md:153) and extent GRANTS from a manager-owned
bitmap (charter.md:158), never `page % writers` ownership. Grep count of partition-related lines today: `journal.zig` 28,
`ledger.zig` 39, `alloc_core.zig` 30, `alloc_ext.zig` 38 — all of it dead under the charter's model (§1.9).

### Rows 7 and 14 — the two unaligned headers, if the owner wants the reserved bytes
Record header (`record.zig:1125-1130`): `key_len u16 @0 ‖ kind u8 @2 ‖ seq u64 @3 ‖ val_len u32 @11` = 15 B. Entry header
(`journal.zig:466-473`): `seq u64 @0 ‖ len u32 @8 ‖ xxh3 u64 @12` = 20 B. Both are encoded through `[N]u8` fields /
`writeInt`, so alignment is not a correctness issue. The question is only whether to spend 1 B/record and 4 B/entry on a
reserved byte each. Arithmetic: interior record ≈ 36 B → 37 B (fanout ≈ 7,166 → 6,972 at 256 KiB; depth ≤ 3 still holds
100 M records); refs 46 → 47 B (+2.2 % of the measured 46 B per reference, charter.md:185); entry 20 → 24 B on an average
≈ 230 B entry (+1.7 %, `kv_alloc_superblock.md` recovery-bound derivation uses 230 B). Against: a forward-only format
with no reserved byte can only evolve by incompat bit + version bump, which is exactly what §1.7 prescribes anyway — so the
reserved bytes buy convenience, not capability. I lean to taking them (they also make the entry's PAD a `kind` instead of a
payload sniff), but the charter's PAD wording (charter.md:116) would need a one-clause amendment and the owner's call.

### Row 25 — kind 4 by key length
`alloc_ext.zig:48-51`: "A DATA allocation delta shares the kind under a `vol_tag`-prefixed 16-byte key; the heap
allocator's replay skips it." Replay dispatches a record family on `key.len`. The charter (charter.md:172) says "kind-4
deltas" for the data bitmap, so this needs a one-word charter amendment to use kind 5 (`TREE_BACKPTR_RESERVED`, unused).
Deciding it now costs nothing; deciding it at rung 6 after the heap-alloc replay has hardened means touching replay again.

### Row 26 — a decoder arm nothing writes
`superblock.zig:303-307`: `triple_within` accepts the ledger/journal/bitmap INSIDE the heap "on reserved extents
(migrated volume)". The v2→v3 migrator is retired in the reference and never existed here; `geometry.zig:85` always
produces `triple_below` with `heap_start` aligned to `node_size`. An accepting arm that no writer can exercise is untested
by construction and dead under §1.9; the validator should instead REQUIRE what `plan` guarantees
(`heap.start % node_size == 0`), making `node_addr % node_size == 0` a free misdirect check everywhere.

### Row 22 — the seed
`geometry.zig:236-237`: `hashSeed(set_uuid) = xxh3(&set_uuid)`. Charter.md:106 says "random `hash_seed`"; the design
goal (charter.md:110) is that "names cannot be chosen to collide". A seed computable from a published identifier is not
that. The set-wide property is preserved by carrying one independent random u64 on every member.

### Row 32 — the config xattr
`backend.zig:925`: the test's config JSON carries `meta_routing_width`, `meta_slot_runs`, `meta_volumes` — the same facts
as the ledger stamp (`ledger.zig:179-193`). `format.zig:122` writes it under `user.squeezefs.format_config`
(`builder.zig:34`); the Zig mount never reads it (`format.zig:522` reads it back only in a describe-roundtrip test). It
exists so the reference `fsck --offline` oracle accepts the image — an oracle 0002 §3 retires on the first format change.

---

## 3. Sequencing recommendation

1. **One format act, `version = 4`.** Rows 1, 3–4, 8–11, 16–22, 26, 32 (and 7/14 if the owner takes the aligned headers)
   land together on a branch from `dev`, with `superblock.version_v3` → `version_v4` and the Rust cross-check tests
   (`node.zig:2198-2256`, `format.zig` venue cross-checks) retired per 0002 §3. The reference then refuses every Zig image
   with "unsupported metadata format version 4 — upgrade squeezefs" (`kv_alloc_superblock.md:95`) — §1.7's "old binaries
   refuse new volumes loud" holds against the reference too, and no half-changed image can ever be misread by it.
2. Add one row per change to 0002's ledger (its §4 rule: "Every departure is recorded below with its reason").
3. Rung-4 rows (5, 6) stay scheduled; rung-6 rows (25, 34) get their widths/kinds written into the specs now so rung 6
   does not re-import the reference's layout.
4. Comment/spec fixes (rows 7, 29, 31; the DUR-5 WHY in row 21) ride the same branch.

Rough size: key/tx/inos/builder/format (row 1) ≈ 400 lines; record (rows 3–4) ≈ 150; bset/node/builder (rows 9–10) ≈ 150;
journal (row 11) ≈ 40; ledger (rows 16–18, 36–37) ≈ 100; superblock/geometry (rows 19–22, 26) ≈ 120; partition removal
(row 20) ≈ −600. Net negative.

What the single-writer class becomes under rows 1 + 9 + 20: a volume whose sector 0 lacks `SYMMETRIC_PLANE`, whose ledger
names a control root (empty tree 0) and the native slot tree, whose frames carry stamp `(0, 0)`, with no appender directory
and no appender-0 page — i.e. today's default image minus the directory extent and page. The rung-2b backend
(`backend.zig:18,321`) then mounts THAT instead of refusing it, and nothing in rung 2b ever sees a kindless key or a v1
frame. Rung 6 adds the plane; it does not change a byte of any tree.

---

## 4. Good decisions worth protecting

- **Big-endian memcmp keys with the kind byte at offset 8** (`key.zig:104-112`, charter.md:110): one comparison for
  every consumer, and an inode, its xattrs and its children's dentries share one leaf region. Refs by block (`06 ‖
  vol_tag ‖ block_idx ‖ owner`) so `refcount(block)` is one range probe (charter.md:185).
- **Guest keyspace `(slot + 1) << 40 | local`** and `FOREST_SLOT_MAX = 65536` (`key.zig:457-504`): slot = top 24 bits,
  native below 2^40, both total and cheap.
- **Checksums everywhere, one convention**: xxh3-64, field zeroed IN PLACE and hashed (`core/checksum.zig:31-50`, used by
  bset/node/ledger/superblock/bitmap). Verified once at load, never per read (charter.md:114).
- **Record count as a claim**: `record_count ≤ data_len / 16` before any allocation (`bset.zig:181-189`, the TEST-4
  find); lengths checked against the container everywhere (`record.zig:55-91` `Reader`, `ledger.zig:333-347`).
- **Verify-once-then-slice** (`bset.zig:162-240`): `RecordMeta` offsets fit u32 by construction; zero-copy `RecordRef`.
- **Frame checksum excludes the pad; the bset carries its own** (`bset.zig:539-542`): a torn append damages nothing live;
  positional torn-bset classification with the `horizon ≤ durable_tail ⇒ loud` tripwire (`node.zig:742-772`).
- **`node_seq_at_write == header.node_seq`** incarnation check + **uuid-namespaced `node_seq_base`** (`node_seq.zig:41-49`):
  quick format need not wipe the heap; a recycled extent's old frames end the log cleanly.
- **Self-addressed node header with `node_size` self-description** (`node.zig:655-656`): misdirected I/O and a
  foreign-layout node are both refused loud.
- **`seq ≡ position`** in entry headers (`journal.zig:824-828`): every entry is self-locating; a stale lap fails with no
  lap field in the entry; PAD entries are ordinary checksummed chain links (charter.md:116).
- **Logical byte space that skips page headers** (`journal_core.zig` geometry; `journal.zig:26-30`): an entry's header
  may straddle a page boundary and nothing special-cases it; `first_entry_off` is PREDICTED for an unwritten successor
  (`journal.zig:481-490`) so replay resyncs without a scan.
- **Ledger: 32 slots = one 128 KiB read; full zero-padded 4 KiB slot writes** (`ledger.zig:17-19`) so a shorter record
  can never leave a longer predecessor's tail parseable; newest-valid-wins; the DENSE rule (`ledger.zig:11-15`).
- **Superblock: whole-sector checksum; version gate BEFORE the checksum** (`superblock.zig:257-264`) so a future format
  is reported as such, never as a tear; `Blank` classification; DUR-5 tail copy with a generation; the backup is never
  consulted while the primary classifies (`kv_alloc_superblock.md:113`).
- **Bitmap A/B pages with `page_index`** (`alloc_ext.zig:186-195`): a tear damages only the copy being replaced; a
  misdirected page is refused; the bitmap is "a checkpoint accelerator, not the sole truth" (`kv_alloc_superblock.md:610`).
- **Interior value `(child_addr, child_node_seq)`** (`key.zig:680-706`): a seq mismatch IS the concurrent-SMO signal
  (charter.md:108); `KEY_SPACE_MAX = 32 × 0xFF` as every rightmost child's separator; `keySuccessor = k ‖ 0x00`
  (`key.zig:744-751`) so siblings partition the space with no unroutable gap.
- **Record kinds start at 1; `Delete` enforces `val_len == 0`** (`record.zig:1104-1121`, `:1190`): a zeroed byte is never
  a valid kind.
- **Readdir cookie = key suffix + 3, 62 bits, sign-clear** (`key.zig:804-845`, charter.md:110): cookies are stable
  across concurrent inserts because they ARE the key.
- **`coll_seq` probing by full-name comparison with a counted clean overflow** (`key.zig:768-802`).
- **Fixed-capacity `KeyBuf`** (`key.zig:114-142`): the builders allocate nothing.
- **Node-seq incarnation spaces** with the derivation on the line (`node_seq.zig:24-35`, charter.md:154).
- **Block-map `RUN_LEN_MAX = 4096` as a format LAW** (`record.zig:411-415`): the floor probe is one bounded forward scan.
- **The ONE fold algebra** with `compactFold`'s `seq < durable_tail` elision law and `foldForward ≡ foldNewestFirst`
  pinned by property test (`record.zig:1252-1530`, charter.md:112) — the format's semantics, and the thing every codec
  change above must leave untouched.

---

## 5. What I could not verify, and questions for the owner

1. **Bit 8 under the reference's forest.** The spec says the format writes `append_partition: None`
   (`kv_alloc_superblock.md:189`) and describes per-appender ring segments and extent grants; I did not open the Rust
   `appender.rs`/`backend.rs` to confirm no forest path still uses `PartitionMap` for bitmap page ownership. If rung 6's
   design wants page-interleaved ownership after all, row 20's "delete" becomes "keep as derived from appender id" — but
   the charter text (charter.md:158, grants as a POOL from the manager) argues against it.
2. **Single-writer = one-member forest?** Row 1 reads charter.md:147 "the one flat opt-out" as an opt-out from the
   symmetric PLANE, not a mandate for per-kind trees. If the owner reads "flat" as "per-kind trees", row 1 becomes KEEP
   and rows 9/16/36 shrink accordingly. I think the locality argument (one leaf region per inode) and the deletion of a
   whole codec axis make the forest-everywhere reading the better design regardless of the wording.
3. **Record/entry header reserved bytes (rows 7, 14).** A genuine trade (≈ 2 % of bytes for an evolution slack the
   charter's bit-and-bump discipline does not strictly need). Owner's call; either answer is defensible.
4. **Inode value width (row 3).** 64 B with a reserved tail, or 96 B with `btime`/`generation` now. The charter's "≈ 96 B"
   is an estimate (its listed fields sum to 56–60 B). I would take 64 B and add fields by version if ever needed.
5. **`hash_seed` provenance (row 22).** If the owner considers the set uuid non-public, the derivation is harmless; I
   still recommend an independent random seed because it costs one input and removes the question.
6. **Interaction with 0002's rows 2–4 schedule.** Pulling the inode value (row 3) forward from rung 4 is my
   recommendation, not the ruling's; the layout record (rows 5–6) stays at rung 4 because it depends on rung-4 types.
