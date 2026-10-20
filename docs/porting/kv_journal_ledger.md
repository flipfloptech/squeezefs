# PORTING SPEC — MetaLV journal ring + reservation core + root ledger ("PR K3")

> **Checksum amendment (2026-10-11, decision 0003 §M; 0002 row 480).** Every `xxh3` / `xxh3-64` /
> `xxh3_64` below names the hash the reference used and this port used until format v5. The port's
> ONE hash is now **sqzhash128** (`src/core/sqzhash.zig`, the pinned definition): every 8-byte checksum
> and identity field is its LOW 64 bits under the fixed public format key (`checksum.hash64`,
> `hash64FieldZeroed`, `stamp`/`verify`, the streaming `checksum.Hasher`); the seeded name hashes
> (`hash54`/`hash56`) are `hash64Keyed` under the per-mount name key (`key.NameKey`: the volume's
> `hash_seed` zero-extended, expanded once at mount). Field names on the wire read `csum`. Byte
> layouts, offsets and widths below are unchanged; only the function behind the 8 bytes is.

> **v4 decisions (format v4, 2026-10-05; decision 0003 WP-F; 0002 rows 37, 40, 41).** The tables below
> are the reference's v3 bytes — evidence for the derivations; where a "v4:" row exists it is what
> `squeezefs-zig` writes. No compatibility with the reference is owed (0002 §2); the "Zig-side
> byte-identity checklist" at the end is retired.
>
> **v4: page header** (`journal.zig`, 24 B, xxh3 over `[0..16)`): `magic "KVJP" u32 @0 ‖ lap u32 @4 ‖
> first_entry_off u16 @8 (physical in-page offset 24..4095, `0xFFFF` = none) ‖ reserved u16 = 0 @10
> (checked) ‖ appender_id u32 @12 ‖ xxh3 u64 @16`. `writer_id u16` and `MAX_APPENDERS = 16` are gone;
> the fixed ring's appender is 0 (`FIXED_RING_APPENDER`); replay verifies `appender_id` against the
> mount's and counts other appenders' pages in `foreign_pages`. Page data = 4072 B.
>
> **v4: entry header** (24 B): `pos u64 @0 (== logical position; the stale-lap/end-of-log rejector) ‖
> len u32 @8 (payload bytes; `24 + len ≤ MAX_ENTRY_LEN = 128 KiB`) ‖ kind u8 @12 {0 = records, 1 = PAD} ‖
> reserved[3] = 0 @13 (checked) ‖ xxh3 u64 @16` over `header[0..16] ‖ payload`. A PAD entry is
> `kind = 1` with an all-zero payload of `pad − 24` bytes (charter §5.1 amended: "entry kind PAD"); the
> `0xFF` payload-tag sniff is gone. `PAD_MIN = 24` (one header), `maxPad = 24 + grain` (4120 @ 4096,
> 536 @ 512); `smoKeep = max(reserve/4, 4072) + maxPad` (65,536 + 4,120 on the 8 MiB floor).
>
> **v4: payload element** = `tag u8 (tree_id | level << 4)` ‖ **16 B record header** `{key_len u16, kind
> u8 {Put 0, Delete 1, Delta 2}, flags u8 = 0 (checked), val_len u32, seq u64}` ‖ key ‖ value; so a frame
> costs `1 + 16 + key + value` (an inode Put is `1 + 16 + 9 + 68`). Interior pointer records of a slot
> tree carry the journal key `slot u32 BE ‖ separator` (`key.interiorJournalKey`; `KEY_BUF_CAP = 36`);
> the control tree's carry the separator alone; `tree.route` is the inverse.
>
> **v4: root ledger record** (`ledger.zig`; 32 × 4096 B slots, slot = `seq % 32`, newest-valid-wins):
> header `magic "KVRL" u32 @0 ‖ version u16 = 1 @4 ‖ payload_len u16 @6 ‖ seq u64 @8 ‖ xxh3 u64 @16` (over
> `header[0..16] ‖ 0⁸ ‖ payload`); payload `journal_tail_seq u64 ‖ next_ino u64 ‖ alloc_bitmap_generation
> u64 ‖ node_seq_watermark u64 ‖ n_roots u16 ‖ roots[n]{root_id u16, node_addr u64, node_seq u64} ‖
> stamp {set_uuid[16], set_epoch u64, member_position u16, member_count u16, native_slot_present u8,
> native_slot u16, n_runs u16, runs[n]{start u16, stride u16 ≥ 1, count u32 ≥ 1}, n_cursors u16,
> cursors[n]{slot u16, next_local_ino u64}}`. `RootId = {control = 8, native_slot = 0x0100}` — a kind byte
> is never a root identity (a slot tree's header id is 0) and a guest slot tree's root is a tree-0
> `slot_state` record. The stamp is on EVERY record (every volume is a stamped set member; the
> one-member forest's is `(set_uuid, 0 of 1, native 0, run {0, 1, 65536})`). Decode-by-length, the
> frozen `routing_width` (W = 2^16 is derived from the u16 slot id), the bit-8 partition suffix and
> `stride == 0` singletons are gone. Budget: 128 runs + 256 cursors ⇒ worst case 3,767 B with 18 spare
> roots. `WORST_CASE_STAMPED_RECORD_LEN` is the comptime tie.
>
> **v4: the bit-8 partitioned-append model is deleted** (0003 Q3): no `AppendPartition`, no
> page-interleaved ring ownership, no per-appender ledger slot arithmetic. Rung 6 gives every appender
> its OWN ring (segment table on its page) stamping `position + seq_offset`; the `seq_floor` of a
> released slot lives in tree 0 (charter §5.5 "the record-seq space is monotone across rings").

Source of truth (read-only): `/Users/joberdorf/Source/squeezefs` (abbrev. `R/`).
Target: `/Users/joberdorf/Source/squeezefs-zig` per charter §5.1–§5.3.
Files: `R/src/meta_backend/kv/journal_core.rs` (641), `journal.rs` (2425),
`journal_lane.rs` (195), `checkpoint.rs` (ledger half), `backend.rs`
(conveyor/durability lane), `R/loom-models/src/lib.rs` (`journal_core` models
1261–1598, two-stage composition 6560–6900). All integers little-endian.
Checksum everywhere = xxh3_64 (xxhash_rust `xxh3_64` / streaming `Xxh3`) in the reference; v5: `checksum.hash64` / `checksum.Hasher` (sqzhash128 low 64, format key).

---------------------------------------------------------------------------
## 1. JOURNAL RING ON-DISK FORMAT
---------------------------------------------------------------------------

### 1.1 Extent placement and sizing (`superblock.rs`)
- Superblock sector 0, `SECTOR_SIZE = 4096` (superblock.rs:58), `SUPERBLOCK_V3_LEN = 4096` (:94).
- `root_ledger = {start: 4096, len: 131072}` (plan, superblock.rs:708–711).
- `journal = {start: root_ledger.end() = 135168, len: journal_len}` (:729–732).
- `journal_ring_len(volume_len) = clamp(volume_len/64, 8 MiB, 32 MiB) / 4096 * 4096`
  (superblock.rs:1169–1172; `JOURNAL_RING_MIN = 8 MiB` :641, `JOURNAL_RING_MAX = 32 MiB` :644).
- `--meta-journal-mb` override (`journal_len_override`): REFUSED unless
  `len % 4096 == 0 && len >= 256 KiB + MAX_ENTRY_LEN (= 262144 + 131072 = 393216)`
  — "reserve + one max entry must always be admissible" (superblock.rs:713–727).
- Bitmap follows the journal; heap aligned up to `node_size`; DUR-5 holds back the
  LAST 4096 B of the volume for the redundant superblock copy (:745–751). The ledger
  has no relation to the backup superblock beyond geometry (ledger is always at 4096).
- Ring pages = `journal.len / 4096` (a trailing partial page is unusable, journal.rs:654–660).

### 1.2 Page (4096 B) = 24 B header + 4072 B entry data
Constants (journal.rs:148–163):
```
JOURNAL_PAGE_LEN      = 4096
JOURNAL_PAGE_HDR_LEN  = 24
JOURNAL_PAGE_DATA_LEN = 4072
JOURNAL_PAGE_MAGIC    = 0x4B56_4A50   // "KVJP" (LE bytes on disk: 50 4A 56 4B)
FIRST_ENTRY_NONE      = 0xFFFF
ENTRY_HDR_LEN         = 20
MAX_ENTRY_LEN         = 131072        // 128 KiB, header INCLUDED
PAD_TAG               = 0xFF
```
Page header image, `page_header_image(lap, feo, writer_id)` (journal.rs:526–540):
```
[0..4)   magic           u32 = 0x4B564A50
[4..8)   lap             u32 = low 32 bits of geo.lap(page_start_pos)
[8..10)  first_entry_off u16 = PHYSICAL in-page offset (24..4095) of the first
                              entry that STARTS in this page; 0xFFFF = none
[10..12) _pad            u16 = 0
[12..14) writer_id       u16 = appender id; 0 for solo (the shipped bytes)
[14..16) _pad2           u16 = 0
[16..24) xxh3_64         u64 = xxh3_64(bytes[0..16])
```
Parse (`parse_page_header`, :546–560): magic mismatch ⇒ None; stored ≠ xxh3(bytes[0..16]) ⇒ None;
else `(lap, feo, writer_id)`. `_pad` fields are covered by the checksum but not validated.
Solo volumes: `writer_id = 0`, byte-identical to pre-partition images (ruling D9).

### 1.3 Logical byte space ↔ physical (journal_core.rs:10–19, 196–262)
Positions are monotonic u64 that NEVER wrap; only page DATA bytes count.
```
L               = pages * 4072                       // logical_len()
lap(pos)        = pos / L
ring_offset     = pos % L
page_index(pos) = (pos / 4072) % pages
in_page_off     = pos % 4072
page_start_pos  = pos - pos % 4072
physical(pos)   = page_offset(page_index(pos)) + 24 + in_page_off(pos)      // journal.rs:1239–1242
page_offset(i)  = journal.start + i*4096   (solo; segment-table walk for grown rings, journal.rs:839–854)
```
`segments(start,len)` → list of `{page, data_off, len}` contiguous, first may begin mid-page,
interior cover whole 4072 B, last may end mid-page (journal_core.rs:227–242).
`owned_page_starts(start,len)` → every `p` with `p % 4072 == 0 && start <= p < start+len`
(:248–261). The reservation containing a page's first logical byte OWNS that page's header.
Entry bytes can never land on a header slot by construction; a multi-page entry is simply
a logical range (no special casing).

### 1.4 Entry framing (journal.rs:86–105, 1291–1304)
```
[0..8)   seq   u64  == logical START position of the entry (seq ≡ position identity)
[8..12)  len   u32  payload byte length (NOT including the 20 B header)
[12..20) xxh3  u64  = xxh3_64( seq.le_bytes ‖ len.le_bytes ‖ payload )   // header fields, checksum excluded
[20..)   payload     = concat over staged records of ( tag: u8 ‖ record_frame )
```
`entry_checksum(seq, len, parts)` (journal.rs:512–520): streaming Xxh3 over the 8 seq bytes,
the 4 len bytes, then payload slices. Whole entry size = `20 + len`; must be ≤ 131072.
Entries pack BACK-TO-BACK in logical space: the 20 B header may straddle a page-data
boundary (test `test_entry_header_straddles_page_boundary`); the payload continues across
pages with one header and one checksum. `len` counts only payload bytes; page headers are
NOT part of the logical stream, so they are neither counted nor skipped — the mapping §1.3
places the bytes.

### 1.5 Payload element = tag ‖ K1 record frame
Tag (journal.rs:464–483): `tag = tree_id | (level << 4)`; `untag(t) = (t & 0x0F, t >> 4)`.
Admissible iff `1 <= tree_id <= 9` at any level, or `tree_id == 0 (KIND_INTERIOR) && level >= 1`.
(record.rs: TREE_INODES=1, DENTRIES=2, XATTRS=3, ALLOC_RESERVED=4, BACKPTR=5, BLOCK_REFS=6,
BLOCK_MAP=7, CONTROL=8, SHARED_INDEX=9=TREE_ID_MAX, KIND_INTERIOR=0.)
Record frame (record.rs:1025–1027, 1094–1111), `RECORD_HEADER_LEN = 15`:
```
[0..2)   key_len u16   (1..=65535; 0 is corrupt)
[2..3)   kind    u8    1=Put 2=Delta 3=Delete (0 never valid)
[3..11)  seq     u64   record seq (= entry seq_base + i, see §2.8)
[11..15) val_len u32   (must be 0 for Delete)
[15..)   key bytes ‖ value bytes
```
`record_frame_len(k,v) = 1 + 15 + k + v` (journal.rs:368–370);
`entry_len_for(records) = 20 + Σ record_frame_len`, `Err(EntryTooLarge{len,cap})` if > 131072
(:382–395). `entry_payload_cap() = 131072 - 20 = 131052`.

### 1.6 PAD entry (journal.rs:176–183, 1276–1287, 1753–1761)
An ORDINARY checksummed entry at logical `end = res.start + res.len`, whole size = `pad`:
```
seq     = end
len     = pad - 20
xxh3    = entry_checksum(end, pad-20, payload)
payload = [0xFF] ‖ zeros(pad - 21)
```
`PAD_MIN = 21` (journal_core.rs:113) = 20 B header + the 1 marker byte; compile-time assert
`PAD_MIN == ENTRY_HDR_LEN + 1` and `PAGE_HDR_LEN == 24` (journal.rs:171–174). Replay: a verified
entry whose payload[0] == 0xFF and payload[1..] all zero ⇒ `Parsed::Pad(end)` — a chain link
with no records; 0xFF followed by any nonzero byte ⇒ `Parsed::Fail`. `untag(0xFF) = (15,15)`
is outside the tree table, so a pre-pad decoder drops it (forward/reverse compat note,
design §4.4 pt 7). A PAD owns (and writes) the headers of pages whose first byte it covers,
exactly like an entry.

### 1.7 Sector-pad law — `CoreGeometry` (journal_core.rs:87–195)
```
CoreGeometry { page_data_len: u64 (4072), pages: u64, reserve_bytes: u64, grain: u64 }
grain: device sector grain (power of two ≤ 4096); 1 = unpadded (buffered fallback / legacy)
ring_grain_for(path) = meta_io_grain(path) if pow2 && ≤ 4096 else 1            (journal.rs:189–196)
meta_io_grain: statx(STATX_DIOALIGN) → sysfs logical_block_size → 4096; 1 when buffered

sector_aligned(pos) = grain<=1 || off==0 || (24+off) % grain == 0     where off = pos % 4072
pad_of(end):
  if sector_aligned(end) → 0
  target = end + 21; off = target % 4072
  if off == 0 → next = target
  else phys = 24+off; up = ceil(phys/grain)*grain
       if up >= 24+4072 → next = target + (4072 - off)      // page end: next page header is aligned
       else             → next = target + (up - phys)
  return next - end                                        // always in [21, 21+grain]
max_pad()  = 0 if grain<=1 else 21 + grain                  // slack every admission claims
smo_keep() = max(reserve_bytes/4, page_data_len) + max_pad()
```
Consequences: grain 4096 ⇒ `(24+off)%4096==0` only at off=4072 ⇒ the ONLY aligned in-page
position is a page start ⇒ every reservation pads to its page END (max_pad = 4117; a window
costs ≤ 21+4072 extra). grain 512 ⇒ aligned at off ∈ {488,1000,…,3560} and page end
(max_pad = 533). Loom geometry 64/8: `(24+off)%8==0 ⇔ off%8==0`.
Pad bytes are `Reservation.pad`; `padded_end() = start+len+pad` is what head/tail/completion
arithmetic reads (journal_core.rs:321–366).

### 1.8 Owned page headers — `first_entry_off` (journal.rs:1318–1335)
For each `page_start ∈ owned_page_starts(start, whole)` of an entry (or PAD) `[start, end)`:
```
feo = 24                             if start == page_start           // entry starts here
    = 24 + (end - page_start) as u16 if end < page_start + 4072       // entry ENDS inside: successor's start
    = 0xFFFF                         otherwise                        // spans the page entirely
lap = (page_start / L) as u32
op  = (page_offset(page_index(page_start)), page_header_image(lap, feo, writer_id))
```
So a page whose first entry is a successor that was NEVER written still carries its
predicted start (replay treats a `feo` pointing at garbage as end-of-log). A page that is
not any reservation's first byte (an entry starting mid-page in a page already owned by a
previous entry) gets NO header write from that entry.
Ops order per framed entry: payload segment ops (file_off = page_offset+24+data_off, slices
of ONE whole-entry buffer) then header ops (:1306–1335).

### 1.9 The aligned run — `aligned_ops(start, end, ops)` (journal.rs:1350–1407)
grain ≤ 1 or empty span ⇒ ops pass through verbatim (pre-pad bytes, op for op).
Else: physical runs of `segments(start, end-start)`:
`lo = page_base if data_off==0 else page_base+24+data_off`, `hi = page_base+24+data_off+len`;
adjacent runs with `prev_hi == lo` merge (a segment boundary or ring wrap starts a new run).
Every run must satisfy `lo % grain == 0 && hi % grain == 0` else `Corrupt` ("pad law not
honoured") and NOTHING is written. One `AlignedBuf::zeroed(hi-lo, mem_align)` per run; ops
sorted by offset are copied in; an op overrunning its run ⇒ `Corrupt`. Unwritten bytes inside
a run (a rolled-back member's hole, a header no survivor owns) are ZEROS. Output = one
`(lo, image)` per run.
Note `lo` for a run beginning at data_off==0 starts at the PAGE BASE (header included) so
the header op lands inside the same image.

### 1.10 Recovery head alignment — `align_head_for_writing` (journal.rs:1456–1481)
`pad = pad_of(head)`; 0 ⇒ Ok(0). Else `try_admit(0, Checkpoint)` (claim = 0 + max_pad)
or `Err(JournalReserveExhausted{needed: pad})` (caller defers: `align_own_rings_for_writing`
runs guarded checkpoint cycles up to `COVER_CYCLES_MAX = 64` until it admits, backend.rs:
18337–18394); `reserve_registered` (debug-asserts `(start,len,pad) == (head,0,pad)`);
`pad_ops` → `rmw_unaligned_ops`: covering grain-aligned spans are READ fresh, ops overlaid,
written whole (`write_at_batch`) — the one honest RMW of the ring (single writer, at open,
nothing open); buffered mode writes ops as-is. Then `complete(res)`; `note_pad_written(pad)`.
Only a WRITER calls this; a reader never writes.

---------------------------------------------------------------------------
## 2. THE RESERVATION CORE (`journal_core.rs`)
---------------------------------------------------------------------------

### 2.1 State (:421–430) and construction (:437–453)
```
JournalCore { geo: CoreGeometry,
              head: AtomicU64,          // next reservation's logical start (monotonic)
              admitted: AtomicU64,      // bytes admitted but not yet transferred to head
              reusable_upto: AtomicU64 }// reclamation watermark: positions < it overwritable
new(geo, head_pos, reusable_upto): debug_assert reusable_upto <= head_pos <= reusable_upto + L
  fresh ring: (0,0); remount: (recovery.head_pos, ledger.journal_tail_seq); grown/at: (start,start)
head()/admitted()/reusable_upto(): load(Acquire)
```
Invariant (:40–44): `head + admitted ≤ reusable_upto + L − reserve` for user admissions
(reserve = `geo.reserve_bytes`), and `≤ reusable_upto + L − keep` for checkpoint-class.
The "packed (lap, logical offset) word" of design §4.4 pt 2 is realised as this monotonic
position (lap/offset derived, §1.3) — ONE u64, no packing arithmetic needed.

### 2.2 Admission — `try_admit_keeping(len, class, keep)` (:494–531); `try_admit(len,class) = keep 0`
```
reserve = geo.reserve_bytes if class==User else keep
capacity = L; debug_assert reserve < capacity
slack = geo.max_pad(); claim = len + slack
adm = admitted.load(Acquire)
loop:
  head = head.load(Acquire)               // READ ORDER: admitted BEFORE head
  reusable = reusable_upto.load(Acquire)
  if head + adm + claim + reserve > reusable + capacity → return None
  CAS admitted: adm → adm+claim (AcqRel success, Acquire failure); Ok → Some(Admission{len,slack}); Err(cur) → adm=cur
```
`Admission { len, slack }` is `#[must_use]`; `is_empty() = len==0 && slack==0`.
Classes (:279–286): `User` (bounded by `L − reserve_bytes`), `Checkpoint` (full capacity minus
the caller's `keep`; the SMO/checkpoint task only). Production `reserve_bytes =
checkpoint_reserve_bytes(ring_len) = max(256 KiB, ring_len/64)` (journal.rs:230–232) where
`ring_len` is PHYSICAL bytes (`pages*4096`).
`JournalRing::try_admit_checkpoint_keeping(len)` = `try_admit_keeping(len, Checkpoint, smo_keep())`
(journal.rs:1096–1102) — every checkpoint-class consumer EXCEPT the flush pass's oldest node
per ring uses it (PR 14 §4.4be liveness law).
A parked user committer (admission refused) waits on `space_notified()` holding NO node lock
(backend.rs:23896–23950): loop {try_admit; stalls+=1; notified = space_notified(); try_admit;
check shutdown/failed; `kick_checkpoint_for_ring_park()` (sets `ring_park_kick` AtomicBool,
Release); timeout(min(remaining threshold, ≥10 ms), notified); if parked ≥ `timeout_threshold`
⇒ `note_journal_failure()` and reset the clock}.

### 2.3 `release(adm)` (:537–541), `split_admission(adm, first)` (:552–565)
`release`: `admitted.fetch_sub(len+slack, AcqRel)`; debug_assert prev ≥ claim; head untouched.
`split`: pure bookkeeping — `(Admission{first, slack}, Admission{len-first, 0})`; slack stays
with the FIRST piece (the one reserved); reserving a slack-less remainder on a padded ring is
a debug-assert failure / release-build clamp (:593–610).

### 2.4 `reserve(adm) → Reservation` (:583–628) — the transfer
```
claim = len + slack
if grain <= 1:
    start = head.fetch_add(len, AcqRel)                    // THE single fetch_add (design §4.4 pt 2)
    admitted.fetch_sub(claim, AcqRel)                      // head bumped BEFORE admitted decremented
    return Reservation{start, len, pad:0}
start = head.load(Acquire)
loop:
    need = pad_of(start + len)
    debug_assert(slack>0 || need==0); debug_assert(need <= slack)
    pad = min(need, slack)                                  // release-build clamp, never over-commit
    CAS head: start → start+len+pad (AcqRel / Acquire); Ok → break pad; Err(cur) → start = cur
admitted.fetch_sub(claim, AcqRel)
return Reservation{start, len, pad}
```
Never blocks, never fails: budget was admitted up front. `Reservation { start, len, pad }`,
`seq() = start`, `end() = start+len`, `padded_end() = start+len+pad`, `padded_len() = len+pad`,
`record_gate(i) = start + i` (position-domain gate for the i-th record; a record ≥ 1 byte so
it lies inside `[start, end)`) (:329–378). `Reservation::unpadded(start,len)`.
Why the two-counter read is safe (:46–58): try_admit reads admitted THEN head; reserve bumps
head THEN decrements admitted ⇒ any interleaving sees `head+admitted` ≥ true claim (double-
counts in flight, never under-counts). Admission may be spuriously refused, never over-commit.

### 2.5 `advance_reusable_upto(pos)` (:634–640)
`debug_assert pos <= head.load(Acquire)`; `reusable_upto.fetch_max(pos, AcqRel)` — monotonic;
stale/repeated advance is a no-op. Callers pass ONLY durable-covered tails (post-barrier).
`JournalRing::advance_reusable_upto` additionally `space_notify.notify_waiters()` (journal.rs:1220–1223).

### 2.6 Loom models (`R/loom-models/src/lib.rs`), all over `tiny_ring(pages, reserve) = {page_data_len:4, pages, reserve, grain:1}`
- `journal_core_reservations_disjoint_monotonic_wrap_exact` (:1286–1401): cap 12, two threads admit+reserve 3 B
  each ⇒ ranges exactly [0,3) and [3,6) in either assignment, seq==start, strictly monotonic, admitted==0, head==6;
  geometry of [3,6) = segments {p0 off3 len1},{p1 off0 len2}, owned_page_starts [4]; try_admit(7) refused until
  advance_reusable_upto(6); then [6,13) crosses into lap 1: segments {p1,2,2},{p2,0,4},{p0,0,1}, owned [8,12].
- `journal_core_admission_never_overcommits_reserve_protected` (:1409–1456): cap 8, reserve 3 ⇒ user budget 5; two
  racing user admits of 3: EXACTLY ONE succeeds (xor); `admitted+head ≤ reusable_upto+8`; checkpoint class admits 3
  more from the carve-out; a user retry still refused; releases settle admitted to 0.
- `journal_core_budget_conserved_across_transfer_release` (:1465–1506): cap 8; racing reserve(2) and release(2) ⇒
  admitted 0, head 2, [0,2); then exact-fit 6 admits and 1 more is refused.
- `journal_core_padded_reserve_lands_every_end_sector_aligned` (:1525–1598): geo {64 B pages, 2 pages, reserve 0,
  grain 8}, max_pad = 29; two racing admit(3)+reserve ⇒ a.start==0, b.start==a.padded_end(), every pad ≤ max_pad,
  every padded_end sector-aligned, `pad == pad_of(end)`, pad==0 || pad ≥ 21, head == Σ padded_len, admitted == 0.
What the model checks JOINTLY: admission + reservation + watermark (no overlap, contiguity,
monotonic seq, exact lap/wrap derivation, no over-commit, budget conservation, reserve not
user-consumable). Weakening that fails it (derivable from the module docs :46–58 and the
assertions): reversing EITHER order — reading `head` before `admitted` in `try_admit`, or
decrementing `admitted` before bumping `head` in `reserve` — admits the under-count
interleaving (both racers see the transferring bytes zero times) and model #2's
`admitted + head ≤ reusable_upto + 8` / exactly-one-of-two assertions fail. The loom header
does NOT carry an explicit "weakening-verified" tag for `journal_core` (unlike `epoch_core`,
`ipc_cqe_core`); the explicitly weakening-verified model in this area is the composition:
- `conveyor_two_stage_models::two_stage_acks_in_journal_order_parked_head_holds_successors_rollback_only_leaf_taker`
  (:6560–6900): restates `reserve_registered`/`complete`/`completed_upto` verbatim around the
  #[path]-included core; 3 windows on ONE leaf key, device lands window 1 FIRST as a FAILURE,
  then 0, then 2. Asserts: acks never leave journal order (`next_ack` fetch_add check), a parked
  predecessor holds successors' answers, rollback is stage B's ONLY leaf-lock take and is
  seq-conditional (`if leaf.seq == w.res.start { restore undo }`), `completed_upto ≥ group_end`
  after in-order completion. Weakening-verified (header :154–156): lane ack publish
  Release→Relaxed FAILS; device "landed" publish Release→Relaxed FAILS; completing the
  reservation at submit instead of in the lane FAILS.

### 2.7 In-flight registry around the core (`JournalRing`, journal.rs:663–671, 1109–1215)
```
Inflight { open: BTreeMap<start, padded_end>, completed_upto: u64 }   // std Mutex, never across .await
reserve_registered(adm) → (Reservation, seq_base):   lock; res = core.reserve(adm); open.insert(res.start, res.padded_end());
                                                     seq_base = res.start + seq_offset (Relaxed)   // ONE mutex section
complete(res): lock; open.remove(res.start) (debug_assert present); completed_upto = min(open) or core.head() if empty;
               unlock; completion_notify.notify_waiters()
completed_upto(): lock; read.          min_inflight_start(): min key of open or u64::MAX
wait_completed_upto(pos): loop { if completed_upto()>=pos return; n = completion_notify.notified() /*registers NOW*/;
                                 if completed_upto()>=pos return; n.await }   // register-recheck-await (todo-24 wedge law)
```
A failed/abandoned write still `complete`s (the hole completes for watermark purposes; bytes
never written). `seq_offset` (:619–634, 1123–1157): record stamp = position + offset; 0 for the
life of a ring nothing was handed to (shipped ring: seq ≡ position); `raise_seq_floor(floor)`
sets `offset = max(offset, floor+1−head)` under the mutex; `seq_frontier() = head+offset`;
`recover_seq_offset(o)` = fetch_max. `SeqSpan::stamped(seq_base, len) = [seq_base, seq_base+len)`
is the RECORD-seq domain (overlay addressing); `Reservation` is the POSITION domain.

---------------------------------------------------------------------------
## 3. WRITE + DURABILITY
---------------------------------------------------------------------------

### 3.1 Single entry — `write_entry(res, records)` (journal.rs:1413–1438)
`entry_ops` (encode payload; `entry_len_for(records) == res.len` else `Corrupt`; `framed_entry_ops`)
→ `pad_ops(res.end(), res.pad)` → `aligned_ops(res.start, res.padded_end(), ops)` → one op ⇒
`uring_fs::write_at`, else `write_at_batch` (parallel SQEs on one ring, UNORDERED between
entries, one completion, any failure fails the batch — uring_fs.rs:485–489, 131–132). Then
counters: `META_KV_JOURNAL_BYTES += res.len`, `META_KV_JOURNAL_ENTRIES += 1`, per-ring
`written_bytes/entries`, `note_pad_written(pad)` (PAD_ENTRIES += 1, PAD_BYTES += pad if pad>0).
`commit_entry(res, records)` = `write_entry` then UNCONDITIONAL `complete(res)` on both outcomes (:1638–1646).

### 3.2 Conveyor batch — `submit_entries_batch(window, parts, lane)` (:1589–1620)
`window` = the batch's ONE registered reservation (span + its pad); `parts` = surviving members'
`(Reservation, records)` sub-ranges, each exact-sized, non-overlapping, NOT necessarily
contiguous (a rolled-back member is absent ⇒ unwritten hole; its page headers are written
iff a survivor owns the page's first byte). Ops: Σ `entry_ops(part)` + `pad_ops(window.end(),
window.pad)` + `aligned_ops(window.start, window.padded_end())`. Submission ONLY (no wait):
on the lane thread with a ring ⇒ `lane.submit_write_at_batch` (`JOURNAL_RING_LANE_WRITES`),
else process pool `uring_fs::submit_write_at_batch` (`JOURNAL_RING_POOL_WRITES`). Returns
`EntriesWriteInFlight { completion, entries, bytes, pad, submitted_at }`; `poll_done()`,
`finish(ring, traced)` awaits and accounts (`note_entries_written`, `note_pad_written`).
Each member's bytes are BY CONSTRUCTION identical to a solo `write_entry` of the same
reservation (shared `entry_ops`) — the batch-of-1 equivalence.

### 3.3 Stage A (apply pass) — caps and order (backend.rs:39–72, 655–685, 23587 ff.)
Batch caps: txs `max(64, cpus×2)` (`SQUEEZEFS_META_COMMIT_BATCH_TXS`, floor 64), bytes
`max(256 KiB, ring_user_capacity/16)` (`SQUEEZEFS_META_COMMIT_BATCH_BYTES`, floor 256 KiB),
clamped to admissible ring capacity. Order: drain → ONE admission for Σ sizes (before any node
lock; §2.2 park loop) → lock UNION of leaves ascending/deduped, revalidate → pre-images → ONE
contiguous `reserve_registered` with per-tx seqs stamped back-to-back (member i's
`Reservation{start: cursor, len_i, pad 0}`, cursor += len_i; the window's pad belongs to the
window) → RAM apply → unlock → encode+SUBMIT → hand `ConveyorWindow` to stage B.

### 3.4 Stage B (durability lane) — `run_windows` (backend.rs:23587–23800)
Per GROUP = head window awaited + every successor whose write already landed:
(8) await each write outcome; `ring.complete(&w.res)` for EVERY window in journal order
(failed ones too) → (9) if any write Ok: ONE `wait_completed_upto(max padded_end)` per ring
→ per window in order: Ok ⇒ `journal_failures.store(0)`, record apply-hole end if the window had
rolled-back members; Err ⇒ `rollback_failed_tx(SeqSpan::stamped(seq_base, res.len), undo, ring)`
(re-acquire leaf locks ascending/deduped/revalidated, restore pre-image ONLY if the key's newest
dirty record still bears this window's seq — skip-if-newer, LWW-correct for Δtime merge records;
`COMMIT_RETRY_BUDGET` retries on stale leaves), `note_journal_failure()`, then
`checkpoint_past_region(region, padded_end)` (zero ring bytes; failure ⇒ another
`note_journal_failure`) → apply-holes in acked windows: checkpoint past the furthest BEFORE
acking survivors → (10) strict mode (`META_FLUSH_INTERVAL_MS == 0`): ONE coalesced
`sync_device()` barrier for the whole group; else `needs_flush.store(true)` (the ≤ 50 ms
deferred flusher barriers; default `resolve_flush_interval_ms() = 50`, legacy alias
`SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS`, mod.rs:191–200) → (11) terminal outcomes window by
window in journal order; guards drop BEFORE the ack.
**Zig port, amended 2026-10-20 (the owner's ruling, decision 0003 §BB; charter §5.2 "The ack
point"):** under the coalesced cadence the members are ANSWERED by the applier right after the
RAM apply, the encode, the copy of the window's image into the volume's journal tail (a memfd
mirror of the ring the mount's supervisor holds, `meta/journal_tail.zig`) and the lane handoff;
steps (8)–(10) stand for the landing (the registry completion, the prefix wait, the hole
checkpoints, the latch, the barrier) and step (11) answers nobody — it frees the window. Strict
mode keeps (11) as the ack. A write that fails after an ack rolls nothing back: the volume
latches FAILED at once. An fsync request's barrier starts only after every window admitted
before it landed. At the daemon's death the supervisor writes every un-landed slot of the tail
to the device in seq order and barriers before it releases the writer guard.
Failure latch: `JOURNAL_FAILURE_LATCH = 3` (backend.rs:241) consecutive `note_journal_failure`
⇒ `failed.swap(true)`, log once, mutations EIO until remount. Barrier failures use a SEPARATE
counter `barrier_failures` with the same 3-latch (reset on barrier success) because the
entry-write success path resets `journal_failures` right before the strict barrier (:1642–1650).

### 3.5 The journal lane thread (`journal_lane.rs`)
One OS thread per writable volume named `sqz-jrnl{idx}` (:140–142); `JOURNAL_RING_ENTRIES = 512`
SQ depth (:53). It creates an `OwnedRing` (fallback: ringless lane, `JOURNAL_LANES_RINGLESS`,
logged once, writes via the pool); the reactor is thread-local (`LANE_REACTOR`). Park =
`io_uring_enter` with the wake-eventfd READ SQE armed so a task wake and a device CQE are ONE
wait (`RingPark::park`); `unpark` = eventfd write; `service` = reap own CQEs once per loop
iteration; the completion's oneshot fires on the thread that polls the durability task next.
`submit_write_at_batch` answers `Some` only ON the lane thread with a ring. A/B lever
`SQUEEZEFS_JOURNAL_LANE=0` keeps both stages on the shared `sqz-meta` pool.
Measured (`.benchmarks/2026-09-03-c2-uring-fs-completion-hop.md`:235–240, 270–272): on the
shipped shape `wake_hop` was 746/664 µs of a 1,209/1,130 µs journal-write round trip = 62 % /
59 %; the lane takes it to 53–107 µs, `journal_ring_write` −34…−57 %, `queue_hop` 79/81 → 0,
io-wq `device` 383/381 → 464/596.

---------------------------------------------------------------------------
## 4. REPLAY (`replay_scan_image`, journal.rs:1780–1928; `parse_entry_at` :1716–1770)
---------------------------------------------------------------------------
Inputs: whole ring `image` (one `read_at` per segment, zero-extended on short read :1008–1019),
`geo`, `tail` (= mounted ledger's `journal_tail_seq`), `expect_writer` (0 solo).
```
L = logical_len; window_base = page_start_pos(tail); chain_end = tail + L
slots = pages + 1 if window_base < tail else pages        // mid-page tail: tail page's 2nd occurrence
headers[k] = parse_page_header(image[k*4096 .. +24]) for every physical page k (verified ONCE)
foreign_pages = count headers with writer != expect_writer (whole ring)
entries=[], dropped=0, pending=0, head_pos=tail, cursor=Some(tail), next_slot=0
slot_after(p) = (page_start_pos(p) - window_base)/4072 + 1
loop:
  Some(pos) if pos < chain_end:
     parse_entry_at(pos):
        Entry(records,end): dropped += pending; pending=0; head_pos=end; if pos>=tail push {seq:pos,records}; cursor=Some(end)
        Pad(end):           dropped += pending; pending=0; head_pos=end; cursor=Some(end)
        Fail:               pending += 1; next_slot = slot_after(pos); cursor=None
  Some(_): break                                              // chain ran exactly to the window end
  None (RESYNC): while next_slot < slots:
        pos_i = window_base + next_slot*4072; next_slot+=1; page = page_index(pos_i)
        match headers[page]:
          Some((_,_,w)) if w != expect_writer      → pending += 1          // foreign: discovery hole
          Some((lap,feo,_)) if lap == lap(pos_i) as u32:
                feo == 0xFFFF                      → continue             // attested continuation page
                feo < 24 || feo >= 4096            → pending += 1; continue // impossible offset
                else found = pos_i + (feo-24); break
          _                                        → pending += 1          // unverifiable / wrong lap
        found ⇒ cursor=Some(p) else break
entries.sort_by_key(seq)   // already sorted; defensive
→ JournalRecovery { entries, head_pos, dropped_torn: dropped, foreign_pages }
```
`parse_entry_at(pos, chain_end)` check ORDER: `pos+20 > chain_end` ⇒ Fail; read 20 B header via
logical mapping; `seq != pos` ⇒ Fail (stale-lap / end-of-log rejector); `whole = 20+len`;
`whole > 131072 || pos+whole > chain_end` ⇒ Fail (len rejected on bounds alone, NO governed byte
read); checksum over segments without copy ≠ stored ⇒ Fail; materialize payload; PAD check
(§1.6); `decode_entry_payload` Err ⇒ Fail (writer-bug class, dropped not loud).
Key properties: chain-PRIMARY (tail is always an entry boundary, so the walk starts AT `tail`;
page headers are consulted only for resync); an entry CONTINUING through a dead-header page is
still read at chain-known offsets and its checksum decides; a trailing failure is NEVER
confirmed (pending only becomes `dropped_torn` when a LATER entry/PAD parses) ⇒ a clean
unmount replays with `dropped_torn == 0`, which makes `replay_dropped_torn > 0 after a CLEAN
unmount` the corruption alert; after a crash nonzero is working-as-designed. A torn member's
same-page successors are unreachable until the next verified page header (chain-only
discovery inside a page). `head_pos` = one past the last verified chain link, ≥ tail; the core
resumes at it with `reusable_upto = tail` (`recover_*`, :1021–1028). Nothing in the scan is
loud; `Err` is device I/O only. Mount-level loud refusals live in `replay_merge` (:2170–2200):
`foreign_pages > 0`, or any `PartitionViolation` (Key overlap across writers, non-authority
`level>0` record, extent outside the emitter's bitmap partition) — all structurally 0 on solo.
Two-phase apply (backend.rs:2690–2810): phase 1 collects every `level > 0` record of mounted
trees, sorts `(level DESC, record.seq ASC)`, `apply_replayed_interior(key, level, seq, kind,
value, entry_start)` and raises the node-seq mint to any `child_seq` named by a Put; phase 2
walks the same `recovery.entries` applying `level == 0` content records by key through the
FINAL routing with original seqs (per-key LWW). Each replayed record's floor contribution is
its ENTRY start (`ReplayedEntry.seq`). `next_ino = max(ledger.next_ino, max replayed ino + 1)`.
`seq_offset_of_window(entries) = max over records of (r.seq − (e.seq + i))` (saturating; 0 on
the shipped ring) — recovery installs `max(page.seq_offset, this)`.

---------------------------------------------------------------------------
## 5. ROOT LEDGER (`checkpoint.rs`:144–815)
---------------------------------------------------------------------------
```
ROOT_LEDGER_SLOTS = 32; ROOT_LEDGER_SLOT_LEN = 4096; ROOT_LEDGER_LEN = 131072
ROOT_LEDGER_MAGIC = 0x4B56_524C  // "KVRL"
ROOT_LEDGER_HDR_LEN = 24; PAYLOAD_FIXED_LEN = 34; ROOT_ENC_LEN = 17
STAMP_FIXED_LEN = 34; RUN_ENC_LEN = 8; STAMP_MAX_RUNS = 128; STAMP_MAX_CURSORS = 256; APPEND_PARTITION_ENC_LEN = 4
```
Slot image (`encode_slot`, :459–568), always a FULL zero-padded 4096 B write:
```
[0..4)   magic  u32 = 0x4B56524C
[4..8)   len    u32 = payload_len
[8..16)  seq    u64 = checkpoint seq
[16..24) xxh3   u64 = xxh3(image[0..16] ‖ 8 zero bytes ‖ image[24 .. 24+payload_len])   // slot_checksum :409–415
payload @24:
  journal_tail_seq        u64
  next_ino                u64
  alloc_bitmap_generation u64
  node_seq_watermark      u64      // NOT in the charter's field list — REQUIRED on the wire (incompat bit NODE_SEQ_WATERMARK)
  n_roots                 u16
  n_roots × { tree_id u8 ‖ node_addr u64 ‖ node_seq u64 }                       // 17 B each
  [membership stamp — optional, present on every slot-mapped volume]:
     set_uuid [u8;16] ‖ set_epoch u64 ‖ member_position u16 ‖ member_count u16 ‖ routing_width u32 ‖
     n_runs u16 ‖ n_runs × { start u16 ‖ stride u16 ‖ count u32 } ‖
     native_slot_plus1 u32 (0 = none) ‖ n_cursors u16 ‖ n_cursors × { slot u16 ‖ next_local_ino u64 }
  [append partition — optional 4 B suffix, AFTER the stamp]: writer_id u16 ‖ writer_count u16
```
Encode refusals (`Corrupt`): runs > 128, cursors > 256, `24 + payload_len > 4096`, roots > 65535.
Worst-case stamped record = 24 + 34 + 5×17 + (34 + 128×8 + 4 + 256×10) = 3,765 B (:270–273).
Decode (`decode_slot`, :573–757) — total: `len < 24` ⇒ Err; magic ⇒ Err; `payload_len < 34 ||
24+payload_len > buf.len()` ⇒ Err (BEFORE touching governed bytes); checksum ⇒
`ChecksumMismatch`; `roots_end = 34 + n_roots*17 > payload_len` ⇒ Err; tail after roots is one
of exactly four shapes decided by LENGTH: 0 ⇒ nothing; exactly 4 ⇒ partition suffix only;
else a stamp (needs ≥ 34 more; `n_runs ≤ 128`; runs fit; `SlotSet::from_runs` validates
geometry; then 4+2 bytes; `n_cursors ≤ 256`; `cursors_end == payload_len || cursors_end + 4 ==
payload_len` else Err; `native_plus1−1` must fit u16) optionally followed by the 4-byte suffix
decoded through `AppendPartition::new(writers, writer_id)` (writers ∈ 1..=16, power of two,
id < writers; else Err). ANY Err = "slot holds no valid record" — never loud at the slot layer.
Placement: solo `slot = seq % 32` (`slot_index`, :446–451); partitioned
`writer_id * (32/writers) + seq % (32/writers)` (`ledger_slot_for`, :434–440;
`ledger_slots_per_writer = 32/writers`, `MAX_APPENDERS = 16` so every appender keeps ≥ 2 slots
— a torn newest slot must have its OWN predecessor).
`write_ledger_slot(path, base, rec)`: `write_at(base + slot_index*4096, image)`; durability rides
the caller's barrier (:778–787). `read_newest_ledger(path, base)`: ONE 128 KiB `read_at`, decode
all 32, keep the highest `seq` that verifies; `None` if none; short extent ⇒ rest absent (:796–815).
Mount: `None` on a v3-superblocked volume is LOUD (`"no valid root-ledger record in any of the 32
slots"`, backend.rs:2421–2429) — format ALWAYS writes a bootstrap record `{seq: 1,
journal_tail_seq: 0, alloc_bitmap_generation: 1, append_partition: None}` then fdatasync,
superblock, fdatasync (builder.rs:699–721). Reader poll `read_newest_ledger_from(adopted)`
(revalidate.rs:381–410): reads slot `(adopted+1) % 32` first and walks while `rec.seq >= expect`;
an older record or an all-zero slot stops; a non-decoding non-zero slot or a non-solo partitioned
record ⇒ whole-ledger fallback; gap belt: after 32 stopped polls read the whole ledger once.
THE DENSE RULE (mod.rs:415–422; checkpoint docs): checkpoint seqs are consumed strictly +1; every
checkpoint-class durable step that consumes a seq OUTSIDE a cycle (ring growth, appender leave,
region release — `consume_checkpoint_seq_for_bitmap`) RESTATES the last cycle's record at the
consumed seq (`META_KV_LEDGER_RESTATEMENTS`) so the predicted-slot poll never stops on a gap.
Checkpoint cycle tail (checkpoint.rs:22–51, 2898–2957): `H = head` at cycle start; flush dirty
nodes; write bitmap pages; BARRIER; `tail = min(H, min_inflight_start, dying_floors, unpublished
root floors, dying leaf floors, min dirty_floor over nodes)` — every input is an ENTRY START
(H = next reservation start; inflight starts; floors round DOWN to the containing entry's start —
FIND-SMO-TAIL §1b: record seqs are `entry_start+i`, a raw per-record floor could land mid-entry
and replay's chain walk would drop acked entries to the resync point). Record = `{seq: ckpt_seq,
tree_roots, journal_tail_seq: tail, next_ino, alloc_bitmap_generation: ckpt_seq,
node_seq_watermark: mint snapshot AFTER the flush loop, membership_stamp, append_partition: None}`;
on write failure the drained dying floors are restored. After the write: `checkpoint_seq`,
`last_ledger_tail`, `retire_seq = ckpt_seq+1`, push `(tail, barrier_push_epoch())` onto
`pending_reclaim` (DUR-3: epoch = barriers already STARTED, which cannot cover it).
`reusable_upto` advance (`after_durable_barrier`, backend.rs:16172–16230): `covered =
barrier_durable` (fetch_max of the completed barrier's start ticket); drain every
`pending_reclaim` entry with `epoch < covered` in order ⇒ `alloc.advance_durable(tail)`,
`ring.advance_reusable_upto(tail)` (wakes parkers), `cache.set_durable_tail(tail)`. All three
§4.6-pt-3 watermarks move on ONE clock, strictly post-barrier, never on the in-RAM tail.
Checkpoint triggers: `CHECKPOINT_MAX_AGE_MS = 1000` since last cycle END (flat: `flat_age_due`);
ring pressure `head − reusable_upto > L/2` (`ring_under_pressure`, :1957–1962); dirty nodes >
`max(4096, budget/32 ÷ node_size)`; shutdown; the park kick (`take_ring_park_kick` swap(false))
makes a cycle due with an immediate barrier. Tick period = flush interval (0 ⇒ 100 ms).

---------------------------------------------------------------------------
## 6. PUBLIC API (Rust-shaped pseudocode, one line each)
---------------------------------------------------------------------------
journal_core.rs
```
pub struct CoreGeometry { page_data_len, pages, reserve_bytes, grain: u64 }      // ring arithmetic input
pub const PAGE_HDR_LEN: u64 = 24; pub const PAD_MIN: u64 = 21;
impl CoreGeometry: logical_len, sector_aligned(pos), pad_of(end), max_pad, smo_keep, lap, ring_offset,
                   page_index, page_start_pos, in_page_off, segments(start,len)->Vec<PageSegment>, owned_page_starts(start,len)->Vec<u64>
pub struct PageSegment { page, data_off, len: u64 }
pub enum AdmissionClass { User, Checkpoint }
pub struct Admission { len, slack }   #[must_use]; len(), slack(), is_empty()
pub struct Reservation { start, len, pad }; unpadded(start,len), seq, end, padded_end, padded_len, record_gate(i)
pub struct SeqSpan { lo, hi }; stamped(seq_base,len), contains(seq)
pub struct JournalCore; new(geo, head_pos, reusable_upto), geometry, head, admitted, reusable_upto,
    try_admit(len,class)->Option<Admission>, try_admit_keeping(len,class,keep), release(adm),
    split_admission(adm,first)->(Admission,Admission), reserve(adm)->Reservation, advance_reusable_upto(pos)
```
journal.rs
```
pub consts: JOURNAL_PAGE_LEN, JOURNAL_PAGE_HDR_LEN, JOURNAL_PAGE_DATA_LEN, JOURNAL_PAGE_MAGIC, FIRST_ENTRY_NONE,
            ENTRY_HDR_LEN, MAX_ENTRY_LEN, PAD_TAG, MAX_APPENDERS=16, ROOT_AUTHORITY_WRITER=0
pub fn ring_grain_for(path)->u64; ring_max_pad_for(path)->u64; checkpoint_reserve_bytes(ring_len)->u64
pub struct AppendPartition { writers, writer_id }; SOLO, new(writers,id)->Result, writers, writer_id, is_solo, is_root_authority
pub fn partition_ring_pages(total,writers); partition_ring_base(base,total,part); partitioned_ring_geometry_ok(total,writers)
pub fn record_frame_len(k,v)->u64; entry_payload_cap()->u64; entry_len_for(&[(u8,Record)])->Result<u64>
pub fn pack_entries(payloads,&overhead)->Vec<Range>  // greedy ≤ MAX_ENTRY_LEN, ≥1 member per chunk
pub fn encode_entry_payload(&[(u8,Record)])->Vec<u8>; decode_entry_payload(&[u8])->Result<Vec<(u8,Record)>>
pub fn tag_for(tree,level)->u8; tag_tree_id_admissible(tree,level)->bool; untag(tag)->(u8,u8)
pub fn page_header_image(lap,feo,writer_id)->[u8;24]
pub struct RingSegment { base, pages }; extent(), from_extent(&ExtentRef)
pub struct ReplayedEntry { seq, records }; pub fn seq_offset_of_window(&[ReplayedEntry])->u64
pub struct JournalRecovery { entries, head_pos, dropped_torn, foreign_pages }
pub struct JournalRing; new(path,base,pages,reserve) / new_in_partition / new_segments / new_segments_at(…,start) ;
    async recover(path,base,pages,reserve,tail)->(Self,JournalRecovery) / recover_in_partition / recover_segments ;
    partition, segments, ring_bytes, page_offset(page), grown_with(seg)->Result<Self> (needs drained ring, ≤ 8 segments),
    core(), path(), written_entries/bytes/pad_bytes, grain,
    try_admit(len,class), try_admit_checkpoint_keeping(len), reserve_registered(adm)->(Reservation,seq_base),
    seq_offset, seq_frontier, raise_seq_floor(floor)->u64, recover_seq_offset(o),
    complete(&res), completed_upto, min_inflight_start, async wait_completed_upto(pos),
    advance_reusable_upto(pos), wake_parked, space_notified()->impl Future, physical_offset_of(pos),
    async write_entry(&res,records)->Result, async align_head_for_writing()->Result<u64>,
    submit_entries_batch(&window,&parts,lane)->Result<EntriesWriteInFlight>, async commit_entry(&res,records)
pub struct EntriesWriteInFlight { pub submitted_at }; poll_done(), async finish(ring,traced)->Result
pub struct MergedEntry { writer_id, seq, records }; MergedReplay { entries, dropped_torn, foreign_pages }
pub enum PartitionViolation { Key{..}, Structure{..}, Extent{..} }   (Display)
pub fn merge_replay_windows(parts)->MergedReplay; detect_partition_violations(entries,alloc_map)->Vec<_>;
       replay_merge(parts,alloc_map)->Result<MergedReplay>   // loud policy
pub enum AppenderViolation { Key, Lease, Extent }; detect_appender_violations(rings,leases,granted,recovering)
```
journal_lane.rs
```
pub statics: JOURNAL_LANES_SPAWNED, JOURNAL_LANES_RINGLESS, JOURNAL_RING_LANE_WRITES, JOURNAL_RING_POOL_WRITES
pub struct JournalLane; spawn(path,idx)->Arc<Self>, spawn_task(site,fut), submit_write_at_batch(path,&mut Option<ops>)->Option<WriteCompletion>
```
checkpoint.rs (ledger half)
```
pub consts: ROOT_LEDGER_SLOTS, ROOT_LEDGER_SLOT_LEN, ROOT_LEDGER_LEN, ROOT_LEDGER_MAGIC, ROOT_LEDGER_HDR_LEN,
            STAMP_MAX_RUNS, STAMP_MAX_CURSORS, CHECKPOINT_MAX_AGE_MS=1000, CHECKPOINT_MAX_DIRTY_NODES_FLOOR=4096,
            PENDING_FREE_FORCE_CYCLES=8, COVER_CYCLES_MAX=64, FIXPOINT_COVER_CYCLES_MAX=16
pub struct MembershipStamp {…}; resolved_native_slot, cursor_for(slot)
pub struct TreeRoot { tree_id, node_addr, node_seq }
pub struct LedgerRecord { seq, tree_roots, journal_tail_seq, next_ino, alloc_bitmap_generation, node_seq_watermark,
                          membership_stamp: Option<_>, append_partition: Option<_> }; slot_index, encode_slot->Result<Vec<u8>>, decode_slot(&[u8])->Result
pub fn ledger_slots_per_writer(writers)->u64; ledger_slot_for(seq,part)->u64
pub async fn write_ledger_slot(path,base,&rec)->Result; read_newest_ledger(path,base)->Result<Option<LedgerRecord>>
pub struct PartitionedLedger { writers, per_writer, tails, pre_partition_records }; authority()
pub async fn read_partitioned_ledger(path,base,writers)->Result<PartitionedLedger>  // 3 loud refusals
pub fn resolve_max_dirty_nodes(budget,node_size,env); checkpoint_tick_period_ms(ms); checkpoint_landing_ceiling_ms(ms); flat_age_due(elapsed,max)
```
Errors used: `KvError::{Corrupt(String), ChecksumMismatch{stored,computed}, EntryTooLarge{len,cap},
JournalReserveExhausted{needed}, Io(_)}` (mod.rs:948–1143).

---------------------------------------------------------------------------
## 7. TESTS (pure = no device/temp file; dev = temp file through uring_fs)
---------------------------------------------------------------------------
`tests/kv_journal_tests.rs` (clean-path contracts):
- `test_checkpoint_reserve_formula` (pure): 8 MiB→256 KiB, 16 MiB→256 KiB, 32 MiB→512 KiB, 64 MiB→1 MiB.
- `test_core_admission_reservation_budget_roundtrip` (pure): geo {10,4,8,1}: user stops at 32, checkpoint enters the reserve, nothing past 40; reserve moves head/admitted exactly; release conserves; watermark opens exactly the missing bytes; stale advance no-op.
- `a_checkpoint_class_admission_keeps_one_smo_claim_of_class_space` (pure): production page geometry, grain 4096, 8 pages, 2-page reserve: keep = 4072 + 4117; one 123 B record per page; keep-honouring admission refuses at 2 and 3 free pages, admits at 4 leaving ≥ keep; keep 0 ≡ try_admit.
- `test_core_geometry_lap_offset_segments` (pure): segments of [4000, +10000) at 4072-pages, owned starts [4072, 8144, 12216], wrap segments.
- `test_entry_payload_roundtrip_and_bounds` (pure): encode/decode; truncations and tag 0 / tag 10 are errors.
- `test_entry_cap_enforced_writer_side` (pure): > 128 KiB refused; exactly 128 KiB legal.
- `test_single_page_entry_roundtrip_and_page_header` (dev): base 4096; page-0 header magic/lap 0/feo 24/checksum; replay equals; head_pos == end.
- `test_packed_entries_single_page` (dev): 17 × 230 B entries fit page 0; page 1 untouched (magic 0); seq-sorted replay.
- `test_multi_page_entry_roundtrip_and_continuation_headers` (dev): 64 KiB entry (17 pages); continuation page feo 0xFFFF; end page feo = successor start; successor lands there.
- `test_entry_header_straddles_page_boundary` (dev): first entry 4062 B, second header split 10/10.
- `test_exact_page_fill_hands_next_header_to_successor` (dev): both pages feo 24.
- `test_wraparound_replay_window` (dev): 4 × 4000 B; admit refused at 16000/16288; advance watermark to rs[2].start; 5th wraps into lap 1; replay from tail = exactly [e3,e4,e5], dropped 0.
- `test_replay_skips_pre_tail_entries` (dev), `test_empty_ring_replays_zero_entries` (dev), `test_recover_resumes_head_and_watermark` (dev: core head = recovered head, reusable_upto = tail, appends continue).
- `test_replay_seq_sorted_and_fold_contract` (dev): Put + Δtime replay folds via `fold_newest_first` to the RAM value.
- `test_concurrent_appenders_disjoint_and_replayable` (dev, 8 threads × 4 entries): unique sorted seqs, admitted 0, head == Σ bytes, 32 entries.
- `test_ledger_slot_encode_decode_roundtrip` (pure): 4096 B image; flipped byte / `[..50]` / `[..10]` / zeros are errors.
- `test_ledger_roundtrip_newest_valid_wins` (dev), `test_ledger_round_robin_slot_placement_wraps` (dev: seq 40 → slot 8 overwrites seq 8; selection by seq), `test_ledger_empty_returns_none` (dev), `test_replayed_entry_shape` (pure).
`tests/crash_contract_tests.rs` (K3 section, dev + shim; `FaultGuard` clears faults):
- `test_torn_write_poisons_path_until_cleared`, `test_power_cut_reverts_unsynced_writes` (shim self-tests).
- `test_kv_journal_torn_last_entry_recovers_prefix_not_loud`: `arm_torn_write(phys(res.start), 10)` ⇒ writer sees `KvError::Io`; replay [1,2], head before the tear, dropped 0.
- `test_kv_journal_torn_entry_mid_log_drops_and_resyncs`: smash B payload ⇒ [1,3,4], dropped 1.
- `test_kv_journal_torn_page_header_recovers_never_loud`: (a) dead page-1 header under a 9000 B entry ⇒ all 3 recovered, dropped 0; (b) plus A's payload damaged ⇒ [2,3], dropped 2.
- `test_kv_journal_garbage_len_never_dereferenced`: len 0xFFFFFFFF and len 100000 (< cap, > window) ⇒ [1,3], dropped 1 each.
- `test_kv_journal_hole_then_resync_recovers_later_entries`: whole page 1 zeroed ⇒ [1,3], dropped 1.
- `test_kv_journal_torn_middle_page_of_multipage_entry`: E 9000 B smashed at +5000 ⇒ [2,3], no fragment of E, dropped 1.
- `test_kv_ledger_torn_slot_falls_back_to_predecessor`: torn slot-5 write (arm at `5*4096+100`, keep 20) ⇒ seq 4; smash slot 4 ⇒ seq 3.
- `test_kv_ring_reuse_never_overwrites_fallback_window`: 8-page ring, five 6000 B; ledger 2 written but NOT durable ⇒ admit(6000) refused; torn slot 2 ⇒ mount from seq 1 replays [1..5]; then fdatasync + advance ⇒ 6th wraps; mount from seq 2 replays [4,5,6].
- `test_kv_v3_torn_newest_ledger_mount_serves_predecessor`, `test_rename_one_entry_atomicity_power_cut` (`arm_power_cut` + `power_cut` around a routed rename: FULLY-OLD vs FULLY-NEW, never a mixture; flush knob parked at 3600000 ms).
- Batch (M7): `test_kv_batch_torn_first_member_drops_alone` ([102,103,200], dropped 1), `…torn_middle_member…` ([101,103,200]), `…torn_member_same_page_successor_follows_chain_rule` ([200] only), `…torn_last_member_is_end_of_log` ([101,102], head = parts[1].end, dropped 0), `…spanning_ring_wrap_replays_whole`, `test_kv_batch_mid_rollback_race_ram_equals_replay` (held pass + armed fault ⇒ whole-batch seq-conditional rollback, RAM == replay digest), `test_kv_batched_commits_replay_twice_digest_equal`.
`tests/write_commit_crash_tests.rs`: `wce_crash_child_entry` (child branch, env `SQUEEZEFS_WCE_CRASH_CHILD=1`) and `kill9_mid_publish_window_soak` (parent re-execs the test binary, SIGKILLs it after a random 5–50 ms, remounts; asserts every ledger-acked publish present, size-never-leads-data, replay idempotence digest, mount never loud; `SQUEEZEFS_WCE_CRASH_ROUNDS` default 10).
`tests/durability_matrix_tests.rs` (skim): data-DEVICE power-cut matrix via `dev_power_cut` (rows Inline/Staged/StripedWriteThrough/…); legs A (fsync-then-cut: acked bytes survive) and B (cut mid-write: whole image, never torn). Not journal-specific.
HARNESS MECHANISM (`uring_fs.rs`): `arm_torn_write(offset, keep)` — the first write intersecting
`offset` persists only its first `keep` bytes, completes `EIO`, poisons the path (reads AND writes
fail) until `clear_faults()`; `arm_power_cut(path)` records prior bytes of every write until an
`fdatasync` on the path marks them durable; `power_cut(path)` restores every still-volatile write
(returns the count); `patch_at(path, off, bytes)` plants bytes with an RMW of the covering aligned
span on an O_DIRECT path (harness-only; product writes at misaligned offsets are REFUSED);
`clear_faults()` resets everything. One relaxed `FAULTS_ACTIVE` load when disarmed. `dev_power_cut`
(src/dev_power_cut.rs) is the DATA-device twin at the `NvmeBlockDev` worker boundary (journal of
`(seq, offset, prior)`, barrier epoch covers entries journaled before it started, cut restores in
reverse order; env `SQUEEZEFS_TEST_POWER_CUT_DEVS`). Checkpoint test seams:
`TEST_CHECKPOINT_HALT_BEFORE_LEDGER` / `_AFTER_LEDGER` (checkpoint.rs:3031, 3060).

---------------------------------------------------------------------------
## 8. NUMERIC CONSTANTS — value and WHY
---------------------------------------------------------------------------
| const | value | why |
|---|---|---|
| JOURNAL_PAGE_LEN | 4096 | §4.1 page = the atomic sector unit; O_DIRECT grain ceiling |
| JOURNAL_PAGE_HDR_LEN / PAGE_HDR_LEN | 24 | 20 B of fields + 4 B pad so the u64 checksum is 8-aligned (journal.rs:28–34) |
| JOURNAL_PAGE_DATA_LEN | 4072 | 4096 − 24 |
| JOURNAL_PAGE_MAGIC | 0x4B564A50 | "KVJP" |
| FIRST_ENTRY_NONE | 0xFFFF | impossible physical offset (> 4095) — "no entry starts here" |
| ENTRY_HDR_LEN | 20 | seq u64 + len u32 + xxh3 u64 |
| MAX_ENTRY_LEN | 131072 | 2× the largest legal record (64 KiB xattr) + headers; writer-side guard; replay bound |
| PAD_TAG | 0xFF | `untag` → (15,15): outside the tree table at every level |
| PAD_MIN | 21 | 20 B header + 1 marker byte — the smallest writable PAD |
| max_pad | 21 + grain (4117 @4096, 533 @512) | PAD_MIN short of a boundary then a whole grain: the slack every admission claims |
| checkpoint_reserve_bytes | max(256 KiB, ring/64) | 256 KiB at the 8/16 MiB floor, 512 KiB at 32 MiB; binds in WINDOWS on a 4096-grain ring: 64 / 128 pages per pass |
| smo_keep | max(reserve/4, 4072) + max_pad | a quarter of the class, floored at one page: holds the widest SMO entry at every format geometry (nodes 64 KiB–1 MiB) |
| JOURNAL_RING_MIN / MAX | 8 MiB / 32 MiB | flat 32 MiB = 12 % of a 256 MiB volume (OQ 1); runway 32 MiB ≈ 139 K create entries ≈ 4.3 s at 32 K/s; 8 MiB ≈ 35 K ≈ 1.1 s > 1 s cadence |
| override floor | 256 KiB + 128 KiB = 393216 | reserve + one max entry must be admissible |
| ~17 entries/page | 4072 / 230 | create-sized entry ≈ 230 B (dentry ~80 + inode ~96 + Δtime ~32 + framing) |
| MAX_APPENDERS | 16 | 32 slots / 16 = 2 slots each — torn newest needs its own predecessor |
| RING_SEGMENTS_MAX | 8 | one 256 KiB segment = 64 pages; 8 = the 2 MiB largest growth step the page can name |
| JOURNAL_RING_ENTRIES | 512 | lane SQ depth: a window's ≤ batch-cap entries (1–2 SQEs each + headers) + the wake READ |
| JOURNAL_FAILURE_LATCH | 3 | consecutive journal-write (or barrier) failures ⇒ FAILED |
| ROOT_LEDGER_SLOTS / SLOT_LEN / LEN | 32 / 4096 / 128 KiB | round-robin, one mount read |
| ROOT_LEDGER_MAGIC | 0x4B56524C | "KVRL" |
| ROOT_LEDGER_HDR_LEN | 24 | magic, len, seq, xxh3 |
| PAYLOAD_FIXED_LEN | 34 | 4 × u64 + n_roots u16 (watermark widened it by 8) |
| ROOT_ENC_LEN | 17 | u8 + u64 + u64 |
| STAMP_MAX_RUNS / CURSORS | 128 / 256 | worst-case record 3,765 B < 4,096 with 19 spare roots; MINT_SPREAD 64 is ¼ of the cursor cap |
| CHECKPOINT_MAX_AGE_MS | 1000 | §4.6 "every ≤ 1 s"; reader poll cadence derives from it |
| tick period | flush_interval (0 ⇒ 100 ms) | strict mode's own tick |
| landing ceiling | 1000 + 2×tick (1,100 ms @50) | tick wait + one bounded drain |
| ring pressure | head − reusable_upto > L/2 | §4.6 "journal distance > ring/2" |
| dirty-node cap | max(4096, budget/32 ÷ node_size) | bounds pinned RAM and mount-replay working set |
| default flush interval | 50 ms | legacy v2 flusher knob reused as the cadence |
| commit batch txs / bytes | max(64, cpus×2) / max(256 KiB, user_cap/16) | arrivals scale with CPUs; ≥ 16 batch reservations cycle the ring |
| PENDING_FREE_FORCE_CYCLES / COVER_CYCLES_MAX | 8 / 64 | wedge terminal; recovery-pad deferral bound |
| PENDING_FREE_CAP | 65536 | allocator FIFO at mount |
| measured | 59–62 % | `wake_hop` 746/664 µs of 1,209/1,130 µs journal-write RTT on the shared lane (C-2) |
| measured | 650–672 µs of 752–776 µs | D-1b pass time spent in `journal_ring_write` before D-2 split the stages |
| measured | 0.4–0.9 % | FIND-VS-A acked-create loss closed by two-phase replay + entry-start floors |

---------------------------------------------------------------------------
## 9. DECODER TOTALITY (every input yields a verdict; no panic, no OOB)
---------------------------------------------------------------------------
Page header (24 B slice): magic ≠ ⇒ None; xxh3(bytes[0..16]) ≠ stored ⇒ None; else Some. Resync
additionally requires `lap == lap(pos_i) as u32`, `writer == expect_writer`, `feo ≠ 0xFFFF`,
`24 ≤ feo < 4096`; every other verifying header is a counted discovery hole.
Entry at `pos` (window `[tail, tail+L)`): header must fit the window; `seq == pos`; `20+len ≤
131072` AND `pos+20+len ≤ chain_end` BEFORE any payload byte is touched; checksum; then PAD
(`[0xFF, 0…]`) or `decode_entry_payload` — per element: tag admissible (`1..=9` any level, or 0 at
level ≥ 1), record header ≥ 15 B, `key_len ≠ 0`, kind ∈ {1,2,3}, Delete ⇒ `val_len == 0`,
`15+key_len+val_len` without overflow and ≤ remaining. Any failure ⇒ `Fail` (pending drop),
never Err. The scan can only `Err` on device I/O.
Ledger slot: `len ≥ 24`; magic; `34 ≤ payload_len ≤ buf.len()−24`; checksum; `34 + 17·n_roots ≤
payload_len`; tail shape by length (0 / 4 / stamp[+4]) with every sub-length checked against
`payload_len` before indexing; partition suffix validated (`1..=16`, pow2, id < writers). Any
failure ⇒ "slot absent". All-32-absent ⇒ loud at MOUNT only.
Out-of-table but checksum-valid structures (e.g. a foreign writer id, a partition violation, a
`level>0` record from a non-authority writer) are the LOUD class at `replay_merge` /
`read_partitioned_ledger`, never at the byte decoders.

### Zig-side byte-identity checklist
1. Page header bytes [0..16) exactly as §1.2 (writer_id 0, pads 0), xxh3_64 over [0..16).
2. Entry = `seq‖len‖xxh3(seq‖len‖payload)‖payload`; `seq` = logical start; positions skip 24 B per page.
3. Record frame `key_len u16‖kind u8‖seq u64‖val_len u32‖key‖value`, prefixed by `tag = tree | level<<4`.
4. PAD = entry at `end` with `len = pad−20`, payload `FF 00…00`; `pad = pad_of(end)` per §1.7.
5. Header ownership + `first_entry_off` per §1.8; header ops only for pages whose FIRST logical byte the reservation covers.
6. Aligned run coalescing per §1.9 (zero-fill holes; refuse misaligned runs).
7. Ledger slot per §5 incl. `node_seq_watermark`; checksum = xxh3(hdr[0..16]‖0⁸‖payload); zero-padded 4096 B; `slot = seq % 32`.
8. Replay walk order/accounting per §4 (chain-primary from `tail`, pending-then-confirmed drops, head_pos = last verified link end).
