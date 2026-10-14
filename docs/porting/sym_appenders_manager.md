# PORTING SPEC — Symmetric metadata, part A: appenders, the appender page/directory, and the MANAGER

> **Checksum amendment (2026-10-11, decision 0003 §M; 0002 row 480).** Every `xxh3` / `xxh3-64` /
> `xxh3_64` below names the hash the reference used and this port used until format v5. The port's
> ONE hash is now **sqzhash128** (`src/core/sqzhash.zig`, the pinned definition): every 8-byte checksum
> and identity field is its LOW 64 bits under the fixed public format key (`checksum.hash64`,
> `hash64FieldZeroed`, `stamp`/`verify`, the streaming `checksum.Hasher`); the seeded name hashes
> (`hash54`/`hash56`) are `hash64Keyed` under the per-mount name key (`key.NameKey`: the volume's
> `hash_seed` zero-extended, expanded once at mount). Field names on the wire read `csum`. Byte
> layouts, offsets and widths below are unchanged; only the function behind the 8 bytes is.

> Scope: charter §5.5 "The forest (KD-SYM-2)", "Partition law", "Appenders", "The record-seq space
> is monotone across rings", "Node seqs are incarnation-scoped", "The manager lease", "Extent grants";
> §5.1 (the format's appender directory + appender-0 page); §5.7 (the D0 winner IS the manager);
> §1.7. The SLOT-LEASE verb bodies (`AcquireSlots/AcquireSlot/OfferSlot/ReleaseSlot/ResolveSlot`,
> handover, the commit door, page-budget overflow), death/recovery (§5.9), tokens, cross-owner
> intents and the data-plane leases are OTHER specs' — only their dispatch shape and the tree-0
> records they share are captured here.
>
> Law of this spec (decision 0002): the reference is EVIDENCE. Every "v4:" row is what the port
> writes; every byte table without one is the reference's, read for its derivations and its WHY.
> Nothing obliges compatibility (0002 §2). Quirks are flagged `Q-n` and collected in §8.

Reference (read-only): `~/Source/squeezefs`. Abbreviations for `file:line` cites below:

| tag | file |
|---|---|
| `A` | `src/meta_backend/kv/appender.rs` (3,484 lines — read whole) |
| `SS` | `src/meta_backend/kv/slot_state.rs` |
| `AL` | `src/meta_backend/kv/alloc_lease.rs` |
| `F` | `src/meta_backend/kv/forest.rs` |
| `NS` | `src/meta_backend/kv/node_seq.rs` |
| `B` | `src/meta_backend/kv/backend.rs` |
| `J` | `src/meta_backend/kv/backend/joined.rs` |
| `R` | `src/meta_backend/kv/backend/recovery.rs` |
| `M` | `src/meta_ship/manager.rs` |
| `MOD` | `src/meta_backend/kv/mod.rs` |
| `FC` | `src/fuse_client.rs` |
| `EK` | `src/env_knobs.rs` |
| `D` | `docs/design-symmetric-metadata.md` |
| `T-A` / `T-M` / `T-N` | `tests/sym_appender_tests.rs` / `tests/sym_manager_tests.rs` / `tests/sym_n_daemon_tests.rs` |
| `C` | `tests/common/sym.rs` |

Port (do not modify in this task): `~/Source/squeezefs-zig`. Cited as `Z:<path>:<line>`.

---------------------------------------------------------------------------------------------------
## 0. WHERE THE PORT STANDS TODAY (what this spec builds on)

- ONE appender per volume: the format-time fixed ring is appender 0 — `journal.FIXED_RING_APPENDER = 0`
  (`Z:src/meta/journal.zig:79-81`), the backend writes every page header with it
  (`Z:src/meta/backend.zig:654`), `smo.zig` stamps it "until rung 6 hands rings to joiners"
  (`Z:src/meta/smo.zig:153-155`). The v4 page header already carries `appender_id u32`
  (`Z:src/meta/journal.zig:54-57`; 0002 row 40) and the v4 bset frame carries `(appender_id u32, g u32)`
  (`Z:src/meta/bset.zig:437-474`; 0002 row 39) — so joiners' rings and frames need NO format change.
- The one-member forest: tree 0 (`TREE_CONTROL = 8`, `Z:src/meta/key.zig:48`) + the native slot tree,
  `TreeIdentity{control, slot}` and `route` as the ONE routing law (`Z:src/meta/tree.zig:296-342`); a
  guest slot tree's root lives in tree 0 as `slot_state:{s}` (`Z:src/meta/routing/slot_state.zig:1-10`).
  The rung-5 `slot_state` codec is the UNLEASED shape alone: key = ASCII `slot_state:` + canonical
  DECIMAL slot (`Z:…/slot_state.zig:17-35`), value = 28 B `{version u16 = 1, flags u16 = 0, root_addr,
  root_seq, cursor}` (`Z:…/slot_state.zig:42-86`) — "rung 6 grows the shape behind the version".
- `node_seq.zig` is COMPLETE for this spec's needs: `K = 38`, `INCARNATION_SPACE`, `incarnationBase`,
  `screenIncarnationBase` (the joiner's screen), the tree-0 record `node_seq_incarnations` =
  `version u8 = 1 ‖ minted u64 LE` (9 B) and `NodeSeqHandle.{shared, sharedBounded, incarnation}`
  (`Z:src/meta/node_seq.zig:24-199`). The manager mints the counter; the joiner installs
  `NodeSeqHandle.incarnation(base)`.
- The extent allocator is ONE owner per bitmap with ONE coverage clock — the manager's — and the
  charter's grants come from this pool (`Z:src/meta/alloc_core.zig:44-48`); `alloc_core.SpaceAudit`
  already carries `deferred_for_grant` and the `.manager_dependency` rung (`Z:…/alloc_core.zig:515-558`).
- The superblock already has `appender_dir @104` (`{0,0}` = none), the `SYMMETRIC_PLANE = 1<<0`
  capability bit, `setSymmetricPlane(dir)` refusing a re-point, and the geometry check "one node-size
  heap extent" (`Z:src/meta/superblock.zig:54,141-150,291-337`). `geometry.plan` writes
  `.appender_dir = .none` and NO page/directory yet (`Z:src/meta/geometry.zig:9-14,126`): the v4
  page/`KVAD` bytes are DECIDED in `Z:docs/porting/kv_alloc_superblock.md:40-58` but not written.
  **Reconcile**: that decision's slot-entry line reads `extents {start u64, len u64}` — a slip; the field
  is the reference's `slot_tree_extents: u32` COUNT (`A:263-265`), see §2.2 below for the corrected v4
  layout (which restores the charter's `SLOT_PAGE_BUDGET = 108`).
- `writer_id` is a u64 (`xxh3(boot_id ‖ hostname ‖ mount_slot ‖ pid ‖ start_ticks)`,
  `Z:src/meta/guard/claim.zig:67-77`; 0002 row 185), `mount_slot` a u32 (`Z:src/data/staging.zig:968-969`).
- Knob registry conventions: `Class = {switch_, derived_override, test_seam, ab_lever, config,
  enum_select, harness, retired}` (`Z:src/core/knobs.zig:57-72`); `SQUEEZEFS_CHECKPOINT_MAX_AGE_MS`
  (1000, `ab_lever`) and `SQUEEZEFS_META_FLUSH_INTERVAL_MS` (50) exist (`Z:…/knobs.zig:106,127`).

---------------------------------------------------------------------------------------------------
## 1. TREE 0 — EVERY CONTROL RECORD, BYTE-EXACT

Tree 0 is the manager's (`D:361-371`, KD-SYM-2/3): header id `TREE_CONTROL = 8`, its root in the fixed
ledger, written ONLY by the volume's manager through ring-0 control entries (`B:9051-9058`: a joined
appender's `write_control_entry` refuses loud — "ring 0 and tree 0 are the manager's"). Per volume:
`slot_state:{s}`, `slot_tails:{s}`, `extent_grant:{a}`, `appender_hint:{identity}`,
`node_seq_incarnations`; on VOLUME 0 only (the set-wide role, `D:370`): `alloc_lease:{vol}`,
`dead_member:{id}`, `recovered:{id, vol}`, `dir_rename`, `custody_quarantine:{slot}` (charter §5.5).
All reference values are little-endian with a leading `version u8`; every key is an ASCII prefix
followed by BIG-endian binary fields so a prefix range walk yields index order (`SS:31-33`).

### 1.1 `slot_state:{s}` (`SS:39-240`)

Key: `b"slot_state:" ‖ slot: u32 BE` = 15 B (`SS:40-42,66-71`); the slot is the FOREST slot (native = 0,
guest = routing slot + 1 — `A:682-688`). Value version byte = **3** (`SS:45`; v1/v2 refused loud:
"forward-only"), variant byte 1 = Unleased, 2 = Leased (`SS:46-47`):

```
Unleased (50 B, SS:48-54,133-152):
[0]      version u8 = 3
[1]      variant u8 = 1
[2..10)  root.addr u64        the slot tree's root node address
[10..18) root.seq u64         its node_seq
[18..26) cursor u64           next local ino (a FLOOR — §5.1.8 never lowered)
[26..30) g u32                lease generation (the frame screen's word)
[30..34) slot_tree_extents u32   the tree's image count (the affinity cap's input)
[34..42) last_written u64     manager seq at the last release (`prefer: unleased-then-idle`'s rank)
[42..50) seq_floor u64        the departing ring's stamp frontier (§1.6 / charter "seq space")

Leased (54 B, SS:55-63,154-176):
[0]      version u8 = 3
[1]      variant u8 = 2
[2..6)   appender_id u32      the LESSEE (KD-SYM-17 — resolution is a projection)
[6..10)  g u32
[10..18) page_addr u64        device offset of the lessee's directory slot A
[18..26) root.addr u64        } the words AS OF THE GRANT: between the grant and the lessee's first
[26..34) root.seq u64         } page write the tree's root has no other durable home (SS:57-61)
[34..42) cursor u64
[42..46) slot_tree_extents u32
[46..54) seq_floor u64        the floor the grant raised the lessee's ring above
```

Decode is total: `len < 2` ⇒ Corrupt; version ≠ 3 ⇒ Corrupt; each variant's length exact (`SS:181-239`).
Semantics the records carry (`D:365-368`): an UNLEASED slot's root lives here; a LEASED slot's LIVE root
rides the lessee's page, and tree 0's Leased words are the grant-time snapshot a page entry supersedes
"once written (a newer root seq)" (`SS:60-61`); the page-budget OVERFLOW law rewrites a Leased record
with the current root (`D:308`, `F:823-829` `note_published_with_cursor`).

**v4 (port):** keep the port's `routing/slot_state.zig` module as THE codec and grow it behind the
version — `extern struct` with `[N]u8` LE fields (the port's convention, no alignment padding needed):
```
SlotStateWire: version u16 = 2 ‖ variant u8 {unleased = 1, leased = 2} ‖ flags u8 = 0 (checked)
  ‖ appender_id u32 (0 when unleased) ‖ g u32 ‖ page_addr u64 (0 when unleased)
  ‖ root_addr u64 ‖ root_seq u64 ‖ cursor u64 ‖ slot_tree_extents u32 ‖ reserved u32 = 0 (checked)
  ‖ last_written u64 (0 when leased) ‖ seq_floor u64                                  = 68 B
```
ONE fixed length for both variants (14–18 B over the reference's 50/54 B — nothing at tree 0's size;
the door's exact pre-admission the reference needed two fixed sizes for — `SS:24-27` — is then one size). The port's
decimal-ASCII key: see **Q-1** (§8) — recommended v4 key `slot_state:` ‖ `slot u16 BE` (ROUTING slot;
the port's W is u16 and its `ForestSlot` is derived from it, `Z:src/meta/key.zig:494-506`), so the census
walk reads slots in index order and the key is 13 B instead of up to 16.

### 1.2 `slot_tails:{s}` (`SS:479-741`)

Written by the RELEASE in the same control entry as `Unleased`, left alone by the grant, so a Leased slot
keeps its previous generation's tails readable (the frame screen's rule 2 reads them while the slot is
leased at `g + 1` — `SS:18-28`). Key `b"slot_tails:" ‖ slot u32 BE` (`SS:479-481`). Value:
```
[0]     version u8 = 1
[1..5)  g u32                 the generation the tails attest
[5..7)  n_tails u16           inline count, or 0xFFFF = SPILLED (TAILS_SPILLED, SS:489-492)
inline: (leaf_addr u64 ‖ tail u32) × n_tails                       12 B each (TAIL_ENTRY_LEN, SS:487)
spilled: n_runs u16 ‖ (extent_addr u64 ‖ count u32) × n_runs ‖ xxh3 u64   (xxh3 over the concatenated
         entry images of every spill extent, SS:617-621; each extent holds node_size/12 entries, SS:536-539)
```
Inline cap = `(value_cap − 7) / 12` and `< 65535` (`SS:531-534`) — at the 256 KiB node's
`recordValueCap = 65,792` (`Z:src/meta/geometry.zig:140-142`) that is 5,482 leaves inline. Spill extents
are written + barriered BEFORE the record names them, released when the record is superseded
(`SS:541-546`). Under O_DIRECT the spill image is zero-extended to the grain (`D:792`).

**v4:** `SlotTailsWire` head `{version u16 = 1, flags u16 (bit 0 = spilled), g u32, count u32}` then
either `TailEntryWire{leaf_addr u64, tail u32, reserved u32}` × count (16 B, aligned) or
`SpillRunWire{extent_addr u64, count u32, reserved u32}` × count ‖ `xxh3 u64`. The sentinel-in-count
(`0xFFFF`) becomes a flag bit (**Q-2**). Inline cap derives from the same `recordValueCap`.

### 1.3 `extent_grant:{a}` (`SS:749-937`)

The manager's attestation of appender `a`'s WHOLE grant — claimed and unclaimed alike; the page names
only the unclaimed remainder (`SS:806-813`). Key `b"extent_grant:" ‖ appender_id u32 BE` = 17 B
(`SS:749-751,775-780`). Value `version u8 = 1 ‖ n_runs u16 ‖ (start u64 ‖ len u32) × n_runs`
(`SS:753-756`, run = 12 B). Runs are NORMALIZED — ascending, non-empty, non-adjacent — and the decoder
refuses any other spelling (`SS:877-937`: `run.start <= prev_end` ⇒ Corrupt; overflow ⇒ Corrupt). The
record's run cap under the tree-0 value cap: `extent_grant_max_runs = (value_cap − 3) / 12` ≤ 65,535
(`SS:767-772`) = 5,482 at 256 KiB — the ONE bound on the pool's fragmentation (`B:9598-9633`: a carve
whose fragments would push the merged record past it is CUT; a return that would split a run past it is
`extent_return_run_cap_refusals`, `A:3012-3020`). `contains(extent)` is the manager's custody probe
(`SS:852-857`); `refuse_extents_held_elsewhere` checks a carve against EVERY other appender's record
(`B:10560-10567`) — two custodians of one extent is `extent_grant_conflicts` (must-stay-0, `A:3004-3007`).

**v4:** `ExtentGrantWire{version u16 = 1, reserved u16 = 0, n_runs u32}` ‖ `GrantRunWire{start u64,
len u32, reserved u32}` × n (16 B, aligned); same normalization law, same `maxRuns(value_cap)` derivation.

**v5 (rung 10 symmetric defects, decision 0003 §AE.4, 0002 row 525): a FAMILY of chunks.** Key
`kind ‖ tag 3 ‖ appender u32 BE ‖ id u64 BE` (14 B; one appender's chunks are one range walk, the
move marker at id `u64::MAX` last). Value `ExtentGrantChunkHead{version u16 = 2, kind u8 (1 runs,
2 marker), reserved u8, n_runs u32, appender u32, reserved u32, id u64, staged_lo u64, staged_hi
u64, csum u64}` (48 B; the head repeats the key's appender and id; `checksum.stamp` over the whole
value) ‖ `GrantRunWire` × n, 1 ≤ n ≤ 253 (a chunk ≤ 4 KiB: one journal page, below every node's
value cap). The record = the union of the effective chunks (pairwise disjoint; adjacent runs across
a chunk bound coalesce). A change REPLACES only the chunks whose owned share of the extent space
(from a chunk's first run to the next chunk's) changed, under fresh ids; a replacement that does not
fit its change's entry is STAGED: the marker `[staged_lo, staged_hi]` + the fresh chunks first (no
reader counts a staged chunk), then the change's own entry deletes the replaced chunks and the
marker. A marker found at a later load never landed: its chunks and the marker are deleted before
the next change. The run cap and its refusals are gone; a carve is cut to what ONE entry carries
beside its `alloc` deltas (≈ 2,298 extents: an `alloc` frame and, at worst, one chunk delete each).

### 1.4 `appender_hint:{identity}` (`SS:329-476`)

What an IDENTITY's predecessor incarnation grew to under load — read at its next `JoinAppender`, so a
rejoin after a storm starts at the sized ring and pool (`SS:329-339`; `D:438` "the rejoin"). Keyed by the
identity's STABLE part `(node_token, mount_slot)` — a Free directory page is anybody's next (`SS:337-339`).
Key `b"appender_hint:" ‖ node_token u64 BE ‖ mount_slot u32 BE` = 26 B (`SS:342-344,388-394`). Value 46 B:
```
[0]      version u8 = 1
[1..9)   ring_bytes u64        the ring size the identity's GrowRings reached (0 = no hint)
[9..17)  grant_extents u64     the largest derived grant it asked (0 = no hint)
[17]     pending u8 ∈ {0, 1}   a GrowRing segment carved that no page names yet (§3.3's durable witness)
[18..22) appender_id u32       } the PendingSegment (SS:352-374): bound to the INCARNATION that asked
[22..30) term u64              } (appender id + page term); every word 0 and checked 0 when pending = 0
[30..38) start u64 (bytes)     }
[38..46) len u64 (bytes)       }  len == 0 or start+len overflow ⇒ Corrupt (SS:451-457)
```
Writers: a wire `ExtentGrant` above the floor rides the hint put in its carve's LAST entry
(`B:9669-9693`); `GrowRing` writes `ring_bytes = max(grown, hint)` + the pending witness in the carve's
own control entry (`B:11094-11110`); `settle_pending_ring_segment` clears or returns the witness
(`B:10754-10833`). Never its own write — always a rider on another control entry (`B:10725-10740`).

**v4:** `AppenderHintWire{version u16 = 1, flags u16 (bit 0 = pending), ring_bytes u64, grant_extents
u64, pending_appender_id u32, reserved u32, pending_term u64, pending_start u64, pending_len u64}` = 52 B.
Key `appender_hint:` ‖ `node_token u64 BE ‖ mount_slot u32 BE` as the reference (binary keys are the
port's choice for every identity-keyed record — see Q-1).

### 1.5 `node_seq_incarnations` (`NS:123-127`; port `Z:src/meta/node_seq.zig:110-129`)

ASCII key `node_seq_incarnations`, value `version u8 = 1 ‖ minted u64 LE` = 9 B. **Already v4 in the
port**, decoder total (empty / version / length refused). Minted by the manager at EVERY join (a rejoin
included): read → `minted + 1` written as ONE control entry → BARRIERED before the base is answered
(`B:11129-11139`): a manager dying after the reply never re-mints the ordinal; dying before it burns one.
Volume-local (each volume its own base `B = nodeSeqBase(uuid)` and counter). Base of ordinal `o` =
`B + o · 2^38` (`Z:…/node_seq.zig:51-60`); exhausting 2^25 ordinals REFUSES the join loud, never wraps.

### 1.6 `custody_quarantine:{slot}` and `dir_rename` (`SS:248-328`, `SS:954-1021`) — volume 0 only

- `custody_quarantine:` ‖ `slot u32 BE`; value `version u8 = 1 ‖ until_ms u64` (9 B): a slot recovered
  from an EARLY death record refuses fresh custody grants until `until_ms`; written in the recovery's own
  tree-0 entry beside the slot's `Unleased`, read at every arm, deleted once expired (`SS:249-255`).
- `dir_rename` (the ONE key); value `version u8 = 1 ‖ holder u32 ‖ term u64 ‖ since_ns u64` (21 B,
  `SS:964-969`): the set-wide directory-rename lease (KD-SYM-14); `term` is PROVENANCE (the serving
  manager's era), never a check (`SS:976-980`); `since_ns` is CLOCK_REALTIME (**Q-3**).

**v4:** `CustodyQuarantineWire{version u16, reserved u16, until_ns u64}` (monotone-free: a cross-process
wall deadline is honest only as realtime — keep realtime, say so on the line);
`DirRenameWire{version u16, reserved u16, holder u32, term u64, since_realtime_ns u64}`.

### 1.7 `alloc_lease:{vol}`, `dead_member:{id}`, `recovered:{id, vol}` (`AL:62-376`) — volume 0 only

Bodies are the data-plane lease spec's; the BYTES are tree 0's and are captured here.
- `alloc_lease:` ‖ `vol_tag u64 BE` (`AL:66,91-96`); value (`AL:75-76,173-200`): `version u8 = 1 ‖
  node_token u64 ‖ mount_slot u32 ‖ writer_id u128 ‖ holder_appender_id u32 ‖ home_vol u16 ‖ control_ino
  u64 ‖ blocks u64 ‖ term u64 ‖ n u16 ‖ (vol u16 ‖ start u64 ‖ len u64) × n` — fixed 61 B + 18 B per
  bitmap extent; `len == 0` / overflow refused (`AL:252-257`).
- `dead_member:` ‖ `node_token u64 BE ‖ mount_slot u32 BE` (`AL:68,116-123`); value `version u8 = 1 ‖
  epoch u64 ‖ ts_ms u64 [‖ pr_key u64 [‖ early u8]]` — THREE lengths 17/25/26 decoded BY LENGTH
  (`AL:77-87,309-343`; **Q-4**, the 0002 row-41 class).
- `recovered:` ‖ `node_token ‖ mount_slot ‖ vol u16 BE` (`AL:70-71,145-152`); value `version u8 = 1 ‖
  by_term u64 ‖ ts_ms u64` (17 B, `AL:88,355-375`).

**v4:** one fixed layout each — `AllocLeaseWire{version u16, reserved u16, node_token u64, mount_slot u32,
holder_appender_id u32, writer_id u64, home_vol u16, reserved u16, n_bitmap u32, control_ino u64, blocks
u64, term u64}` ‖ `BitmapRefWire{vol u16, reserved[6], start u64, len u64}` × n;
`DeadMemberWire{version u16, flags u16 (bit 0 = early), reserved u32, epoch u64, ts_realtime_ns u64, pr_key
u64}` = 32 B; `RecoveredWire{version u16, reserved u16, reserved u32, by_term u64, ts_realtime_ns u64}` = 24 B.

### 1.8 `appender_alloc` — named by the charter and the design, NEVER BUILT (`D:356,487`; no code hit for
`appender_alloc` anywhere in `src/`). The appender DIRECTORY is the allocation: a `Free` or blank pair is
an unallocated id, the lowest Free page is the next (`B:10358-10379`), the chain grows by one extent when
every pair is taken (`B:10380-10438`). **v4 decision for the port:** do NOT define a tree-0 record nothing
reads (the rails rule, `Z:tests/rails.zig`); the superblock's `appender_dir` + the `KVAD` chain IS the
appender allocation truth. Record the deletion of the name in 0002's ledger (**Q-5**).

### 1.9 Key-range walks and the control tree's size

Every family has an inclusive `[prefix ‖ 0…, prefix ‖ 0xFF…]` range (`SS:73-77,505-508,782-785`;
`AL:98-101`); the census and the open walk them in slot / appender order. Size at the operating point:
≤ 65,536/V `slot_state` records × ~60 B — 1,024 ≈ 56 KiB on V = 64 (one leaf), 8,192 ≈ 450 KiB on V = 8
(`D:127`). `meta_kv_forest_root_publishes` counts `slot_state` puts (`F:60-63`, `FC:13546-13552`).

---------------------------------------------------------------------------------------------------
## 2. THE APPENDER PAGE, ITS FOUR SLOTS, THE DIRECTORY, THE RING

### 2.1 The reference page, byte-exact (`A:77-103`, `A:339-407` encode, `A:412-568` decode)

4,096 B LE image; xxh3-64 at `[16..24)` over the image with that field zeroed (`A:576-585`); magic
`0x4B564150` = "KVAP" as a LE u32 (`A:54`); a/b over FOUR slots (§2.4). Fixed part 314 B:
```
[0..4)    magic u32 "KVAP"                [4..8)    appender_id u32
[8..16)   generation u64 (newest-valid-wins key, +1 per write)
[16..24)  xxh3 u64 (field zeroed)
[24..32)  node_token u64      }           [32..48)  writer_id u128    } identity (A:191-198)
[48..52)  mount_slot u32      }
[52]      state u8 {0 Free, 1 Live, 2 Recovering, 3 Recovered} (A:127-139; other ⇒ Corrupt)
[53]      is_manager u8 {0,1} (other ⇒ Corrupt, A:550-558)
[54]      home_volume u8
[55]      layout_version u8 = 1 (APPENDER_PAGE_LAYOUT_VERSION, A:104-113; ≠ 1 with a valid checksum
          = ForeignLayout ⇒ the whole four-slot read REFUSES, never a fallback — A:593-614, 650-675)
[56..64)  term u64 (+1 per (re)join)      [64..72)  recovered_by_term u64
[72..74)  n_segments u16 ≤ 8              [74..80)  reserved[6] = 0 (checked)
[80..208) segments[8] {start u64, len u64} (16 B each; unused entries 0, checked)
[208..216) head_hint u64                  [216..224) ledger_tail_seq u64
[224..232) ckpt_seq u64                   [232..234) n_runs u16 ≤ 4   [234..240) reserved[6] = 0
[240..304) runs[4] {start u64, len u32, reserved u32 = 0} (16 B each)
[304..306) n_slots u16 ≤ SLOT_PAGE_BUDGET  [306..314) seq_offset u64
[314..)   slot entries × n_slots, 35 B each, slot-ASCENDING (A:524-532):
          slot u16 ‖ state u8 {0 Live, 1 Releasing} ‖ g u32 ‖ slot_tree_extents u32 ‖ root.addr u64 ‖
          root.seq u64 ‖ cursor u64
          every byte past the last entry = 0 (checked) ⇒ decode ∘ encode = id (A:465-491)
```
`SLOT_PAGE_BUDGET = (4096 − 314) / 35 = 108` — DERIVED from the fixed part and the entry width, "never a
free constant" (`A:121-125`; `D:430`: the design's 122 predates `slot_tree_extents`). Bounds:
`RING_SEGMENTS_MAX = 8` (one 256 KiB segment = 64 pages; 8 = the 2 MiB largest growth step before the
page's remaining bytes bind — `A:58-61`), `GRANT_RUNS_MAX = 4` (the current run, its successor, two
returns in flight — the alloc_lane ahead-refill depth, `A:62-64`). The page IS the appender's ledger
record (tail, checkpoint seq, roots, cursors) AND its directory entry — one write per checkpoint, one
read per recovery; it never names tree 0's root or the stamp, so two managers are unrepresentable
(`D:430`, KD-SYM-3). `AppenderPage::free(id, generation)` is what format and a region release write:
identity zeros, Free, no segments/grant/slots (`A:316-334`).

### 2.2 v4: the port's page (reconciling `kv_alloc_superblock.md:40-50` with the charter's 108)

Decisions already taken there and KEPT: version right after the magic as u16; xxh3 LAST at `@4088` over
`[0..4088)`; `home_volume u16` (member positions are u16); `appender_id u32` everywhere; ASCII magic in
disk order ("KVAP"). CORRECTIONS: the slot entry carries `slot_tree_extents u32` (a COUNT, `A:263-265`),
not an extent ref; the identity's `writer_id` is the port's u64 (0002 row 185), not u128; and the
reference's `recovered_by_term` is kept (the recovery spec reads it — `A:287-288`). Byte-array fields
(`[N]u8` LE, the port's `SlotStateWire` idiom) make the 35 B entry legal in an `extern struct`:
```
AppenderPageWire (fixed 296 B):
@0    magic [4]u8 = "KVAP"             @4    layout_version u16 = 1      @6   reserved u16 = 0
@8    appender_id u32                  @12   reserved u32 = 0
@16   generation u64
@24   node_token u64                   @32   mount_slot u32              @36  reserved u32 = 0
@40   writer_id u64                                                           (identity = 24 B)
@48   term u64
@56   state u8 {0,1,2,3}  @57 is_manager u8  @58 home_volume u16  @60 n_segments u8 ≤ 8
@61   n_grants u8 ≤ 4     @62 n_slots u16 ≤ 108
@64   head_hint u64       @72 ledger_tail_seq u64   @80 ckpt_seq u64   @88 seq_offset u64
@96   recovered_by_term u64
@104  segments[8] {start u64, len u64}                                   (128 B → @232)
@232  grants[4] {start u64, len u64}   (extent INDEX runs widened to u64 — no reserved pad)  (64 B → @296)
@296  SlotEntryWire × n_slots, 35 B each: slot u16 ‖ state u8 ‖ g u32 ‖ slot_tree_extents u32 ‖
      root_addr u64 ‖ root_seq u64 ‖ cursor u64       — slot-ascending; tail zero to @4088 (checked)
@4088 xxh3 u64 over [0..4088)
SLOT_PAGE_BUDGET = (4088 − 296) / 35 = 108   (comptime assert; the charter's number restated)
```
Every reserved word is checked zero; every count checked against its bound before any element is read;
unused table entries zero; the four-slot reader's three-way classification `Valid | Blank | Corrupt |
ForeignLayout` is kept, with ForeignLayout refusing the whole read (`A:593-675`). Fuzz the decoder
(`std.testing.fuzz`) and assert encode∘decode = id (the reference's `appender_page` target, `A:43-44`).

### 2.3 Where the four slots live (`A:14-26`, `A:716-773`, `A:2415-2430`)

- Appender **0**: `[A, B, R0, R1]` = the fixed journal extent's first FOUR pages
  (`appender0_page_offsets`: `journal.start + {0, 4096, 8192, 12288}`, `A:719-727`); its ring proper
  starts at `journal.start + 16384` (`appender0_ring_extent`, `A:731-737`;
  `APPENDER0_RESERVED_PAGES = 4`, `A:69-71`). Format writes ONE `Free` page at slot
  `page_slot_for(generation 1) = 1` (= `journal.start + 4096`) and leaves the other three ZERO
  (`Z:docs/porting/kv_alloc_superblock.md:598-631`).
- Appenders **≥ 1**: the directory pair `[A, B]` at `dir.start + pair × 8192` (+4096 for B) inside a
  chain extent (`dir_pair_offsets`, `A:751-755`); `[R0, R1]` = the first two pages of the ring's FIRST
  segment (`ring_side_offsets`, `A:758-763`; `RING_SIDE_LEDGER_PAGES = 2`, `A:72-74`), the ring proper
  of that segment starting at `+8192` (`first_segment_ring_part`, `A:767-773`). A directory read reads
  R0/R1 only once a valid directory image names a first segment (`A:2365-2371`).
- The directory extent (reference): one `node_size` heap extent per chain link holding
  `(node_size/4096 − 1)/2` pairs from its FIRST page up, the header page LAST (`dir_pairs_per_extent`,
  `dir_header_offset = extent.end − 4096`, `A:740-748`). `DirHeader` = `magic "KVAD" u32 ‖ chain_index
  u32 ‖ xxh3 u64 (field zeroed) ‖ next.start u64 ‖ next.len u64 ‖ pairs u16`, zero-padded, padding
  checked (`A:775-847`). The chain walk is bounded by the heap's extent count (a longer chain is a cycle
  ⇒ Corrupt) and refuses a header whose `chain_index` is not the expected one (`A:2335-2356`).
  **v4 (already decided, `kv_alloc_superblock.md:51-55`):** the header page FIRST at `dir.start` —
  `KVAD` ‖ `version u16 = 1` ‖ `reserved u16` ‖ `pairs u32` ‖ `next_link {start, len}` ‖ `xxh3` — pairs at
  `dir.start + 4096 + pair × 8192`; `pairs = (node_size/4096 − 1)/2` (31 at 256 KiB, 7 at 64 KiB).
  Appender ids are dense: `0` the fixed ring, then `1..` in chain order (`A:2336,2378-2382`); ids are
  stable for the volume's life (`D:432`).

### 2.4 Newest-valid-wins and the DIRECTORY-FIRST law (`A:28-35`, `A:587-591`, `A:2255-2292`)

- Slot rotation: `page_slot_for(generation) = generation % 4` (`A:589-591`) — the three predecessors stay
  intact; the reader takes the valid image with the highest generation over the four (`A:656-675`), so a
  torn newest page falls back to a predecessor of the SAME appender with two of slack (`D:430`).
- **Directory-first**: a directory image must ALWAYS name the region's CURRENT ring table (the reader's
  invariant: it finds R0/R1 through a directory image). `dir_named_mask(dir_pages, table)` = bit i iff
  directory slot i's valid image names `table` (`A:2262-2270`); `page_write_slots(mask, generation)`:
  both bits set ⇒ `[page_slot_for(generation)]`, else every directory slot NOT yet naming the table
  (A then B, one generation apart) (`A:2286-2292`). A table change — a join onto a fresh ring, a growth,
  a leave — resets the region's `dir_named` to 0 (`A:2605-2610`) and the next page write lands in both
  directory slots before any ring-side slot is used (`B:11521-11542`); the charter's "written to every
  such directory slot in the same barriered step". The join writes BOTH directory slots then barriers
  (`B:10619-10623`); the leave likewise (`J:3237-3241`).
  **v4:** `pageWriteSlots(mask, generation)` and `dirNamedMask` are pure functions with a tie test over
  every (mask, generation) cell (the reference pins `page_slot_rotation_covers_the_four_slots`,
  `T-A:360`).

### 2.5 What a checkpoint writes into the page (`B:11791-11834`)

Per cycle, per own region: `ledger_tail_seq = tail` (this ring's replay tail), `ckpt_seq` = the cycle's
seq, `head_hint` = the ring head (region 0: the fixed ring's), `seq_offset` = the ring's offset IN FORCE
(not the last cycle's — a grant since may have raised it, `B:4821-4823`), `segments` = the table as
maintained at open/growth (never derived from page ranges, `B:11819-11829`), `grant` = the unclaimed
remainder's LARGEST ≤ 4 runs (`page_runs_of`, `A:1400-1409`; the rest stays in the RAM pool unnamed —
`A:1965-1982`), `slots` = the entries `prepare_page_entries` ranks (the overflow law — the slot-lease
spec's). The write pushes `(tail, barrier epoch)` to `pending_reclaim` (DUR-3 per ring, `B:11835-11840`).
A `Free` page at a leave keeps id, term and `head_hint` and sets `ledger_tail_seq = head_hint` — the
seq-space watermark the next carve continues from (`J:3135-3140,3233-3236`).

### 2.6 The ring segment table, zeroing, and seq-space continuation

- A ring is a SEGMENT TABLE of whole heap extents, ≤ 8 (`A:291-293`, `D:419-421`); region 0's ring IS the
  fixed ring and never grows (`A:2555-2558`); a region ≥ 1's ring is REPLACED by `grown_with` on a
  drained stall, never grown in place (`A:2556-2558`, `J:2456-2493`). Positions continue across a swap
  (`A:2682-2686`: the head is a byte position that continues on any table).
- **Zeroing law** (`A:2183-2213`, `zero_extents`): every carved extent — a join's ring, a growth
  segment, a rejoin's — is ZEROED (4 MiB chunks, one sequential write per extent) BEFORE any page names
  it. WHY: the heap hands a fresh ring the extents a predecessor incarnation's ring occupied, with that
  ring's checksummed entries still on the device at lap 0 like the new ring's own; the replay chain
  would walk past its own head into them (a predecessor's `+ref` out-voting an acked release by
  position; a predecessor's `free(extent)` folding into the grant and RETURNED while an image stands).
  A zeroed page verifies nothing and replays to nothing (`A:2192-2195`). A control-plane act at the
  carve, never a hot path (`A:2195-2197`). Pin: `a_re_carved_ring_never_replays_its_predecessor_
  incarnations_entries` (`T-A:2699`).
- **Seq-space continuation at a fresh incarnation**: the join sets `head_hint = ledger_tail_seq =
  max(page.head_hint, ring 0's head)`, `ckpt_seq = 0` (`B:10610-10616`) — the charter's
  "`max(page.head_hint, ring 0 head)`"; the carve law's rationale (`B:4826-4833`): record seqs are ring
  POSITIONS and per-key LWW compares them raw across incarnations, so a fresh ring must start above
  every position its id ever stamped AND above ring 0's head (the manager's structural records on the
  slots it hands out).

### 2.7 The record-seq space is MONOTONE across rings (`D:301`; `A:300-306`, `A:1411-1446`)

Every ring stamps `position + seq_offset` (the offset stamped AT reservation under the in-flight mutex,
so seq order stays reservation order within a ring); a release records the departing ring's FRONTIER
(`head + offset`) as the slot's `seq_floor` (tree 0 §1.1, the wire's slot words), max'ed over every
release; a grant / re-adoption RAISES the receiving ring's offset above it (`raise_seq_floor`, a monotone
fetch_max, idempotent). The offset is durable on the page (`seq_offset`), recovered as `max(page, the
window's own stamps)` — exact, since `seq − (entry position + index)` IS the offset (`A:304-306`); page 0's
raises ring 0's at open (`B:20635-20639` `recover_seq_offset`). A ring nothing was ever handed to stamps
positions verbatim (offset 0) — the one-member forest is byte-identical to today. The MANAGER's side: a
release raises ring 0's offset above the departing ring's floor too (`D:303`: a merge's parent flips
stamped below the lessee's split pointers were shadowed by the fold). `SEQ_FRONTIER_SANE_MAX = u64::MAX/2`
(`A:1411-1420`: positions are journal bytes + one per grant; 2^63 bytes is provably not a frontier; a
frontier past it would saturate every later stamp and collapse the fold's LWW to insertion order).

**v4 (port):** the journal core's packed `(lap, logical offset)` word (`Z:src/meta/journal_core.zig`)
gains a `seq_offset: atomic u64` on the ring object, added at `reserve` (the stamper), never on the
positions themselves (the pending-free gate stays a POSITION — `D:309` round 3 Issue 20: "every §4.7
pending-free gate is a ring POSITION"); `raiseSeqFloor(floor)` = `fetchMax(floor + 1 − head)`-class
monotone raise with an explicit ordering comment; recovery = `max(page.seq_offset, max over the window of
(seq − position − index))`. Model it: a standalone model of {reserve-stamp, raise, recover} proving every
stamp after a raise exceeds the floor, with the WEAKENING (raise after the reservation's stamp) failing.

### 2.8 Node seqs are INCARNATION-scoped (`D:375`; `NS`; port `node_seq.zig`)

The volume base `B = nodeSeqBase(uuid)` opens incarnation 0 — the manager's — bounded by `B + 2^38`
(`NodeSeqHandle.sharedBounded`, raised by replayed roots INSIDE its space only); every `JoinAppender`
(a rejoin included) is minted ordinal `o ≥ 1` from the durable counter (§1.5) and the joiner mints in
`[B + o·2^38, B + (o+1)·2^38)`, its handle NEVER raised (`Z:…/node_seq.zig:9-16,158-162`). The word travels
in `Joined.node_seq_base`; the JOINER SCREENS it before anything is installed (`screenIncarnationBase`:
exactly `B + o·2^K` for `1 ≤ o ≤ 2^25 − 2`, else `Rejected` — `Z:…/node_seq.zig:83-101`;
`JOINED_WIRE_WORDS_REJECTED`, `J:33-38`). Every cross-appender comparison is equality or generation,
never order (`D:375`). Pins: `a_forged_node_seq_base_in_the_join_reply_is_refused_before_anything_is_
installed` (`T-N:1301`), `two_joined_appenders_never_mint_an_equal_node_seq` (`T-N:1475`).

### 2.9 Partition law and routing (charter §5.5; `D:473-475`; `F`)

A slot tree has exactly one writer-cacher, its lessee; `apply_locked` refuses a mutation to a slot this
mount does not lease — `meta_kv_leaf_lease_refusals`, must-stay-0 (`D:475`). The region a record journals
into is the one LEASING its slot, else the manager (region 0) (`A:3104-3112` `region_of_slot`); a
transaction spanning two appenders' slots is `Corrupt` ("a cross-appender mutation is a cross-owner
operation, never one entry in two rings", `B:4481-4505`). A slot tree's interior pointer records are
journaled under `slot u32 BE ‖ separator` (`F:908-937` `interior_journal_key` — a separator alone cannot
name the tree: every tree's top separator is `KEY_SPACE_MAX`, and refs sort after every ino-major key so a
slot tree's key space is not one contiguous range); the port's `tree.route(trees, tree_id, level, k)`
(`Z:src/meta/tree.zig:961`) already routes interior records by `(tree_id = 0, level ≥ 1)`.
Lazy mint: a slot with no records owns no extent; the first record mints the tree under ONE mint mutex
(`F:16-21,52-54`), from the lessee's GRANT when a region leases it (`F:124-136` `MintContext.region`);
a non-writer never mints (`F:104-121` `MintPolicy::Refuse` — loud, `invariant_tripwires`).
A root PUBLICATION remembers its home `{Tree0, Page}` (`F:69-94`); the manager maintains only the trees
it leases plus the UNLEASED ones (`maintains_slot`: `leased ∨ (¬foreign ∧ is_manager)`, `B:5655-5684`;
skips counted `merge_sweep_foreign_skips`).

---------------------------------------------------------------------------------------------------
## 3. THE REGION LIFECYCLE

### 3.1 JOIN — `JoinAppender { identity, ring_want_bytes }` → `Joined { appender_id, page_addr,
ring_segments, grant, already, node_seq_base }` (`M:135-142,483-497`; `B:10263-10701`)

Order of effects at the manager, under `manager_verbs` (one verb at a time, `B:10286`):
1. `read_directory` (ONE whole-directory walk per verb — `appender_directory_reads`, `A:2294-2305`).
2. **Durable idempotency** (KD-SYM-7): a `Live` page under this identity IS the join — answer
   `already = true` with the page's segments and its UNCLAIMED remainder (the page's `grant` word,
   never the whole record), mint a FRESH incarnation anyway (a rejoin is a new incarnation), and if the
   page is Live with an EMPTY grant and an empty record the first join died before its grant: complete
   it here (`B:10287-10329`).
3. **Capacity refusal**: `live ≥ set.capacity` ⇒ `manager_verb_refusals` + `Busy("{live} appenders are
   live and the ring budget admits {capacity} (appenders_capacity = heap/16 ÷ ring) — the lever is the
   metadata volume COUNT, never a format-time client count")` (`B:10330-10356`; charter "a join past it
   refuses naming the volume count as the lever"; `D:772-774` R-SYM-6). The budget's remainder is
   refreshed from `rings_in_use(entries)` (`A:1386-1395`, `B:10338-10346`).
4. **Page allocation**: the lowest Free/blank page whose id no in-process region holds (`B:10358-10379`);
   none ⇒ grow the chain by one `claim_internal` extent: zero it, write its header (chain_index + 1,
   next = none), barrier, re-write the previous last header with `next = extent`, barrier — a torn growth
   leaves a chain that ends where it did (`B:10380-10438`).
5. **Settle a pending GrowRing word** of the identity's PREVIOUS incarnation against its LAST page in any
   state (`B:10445-10466`; §3.3).
6. **Ring size**: `ring_want_bytes == 0` ⇒ `resolve_sym_ring_bytes_hinted(hint.ring_bytes)` =
   `max(derived(ewma 0) = floor, hint)` clamped `[512 KiB, ceiling]` page-aligned (`A:930-939`); explicit
   ⇒ clamped to `[floor, ceiling]` (`B:10467-10477`). Then the ask UNDER THE BUDGET:
   `join_ring_bytes_under_budget(ask, remaining, node_size) = max(min(ask, remaining ↓ node), floor)`
   (`A:965-979`; the count law budgets every admitted appender one FLOOR ring; a hinted ask above the
   remainder lands the remainder and grows later).
7. **Carve**: `ceil(ring_bytes / node_size)` × `claim_internal`, sorted, coalesced; a fragmented heap keeps
   what the page's table can name — `ring_segments_that_fit(runs, floor_extents)`: all if ≤ 8 runs, else
   the largest runs filling HALF the table when their total reaches the floor, else the largest 8; the
   rest released, never a refusal (`A:1315-1352`, `B:10500-10556`). The carve must sit in no appender's
   grant record (`refuse_extents_held_elsewhere`, `B:10557-10567`); ZERO it (`B:10569-10576`); the claims'
   alloc deltas as ONE control entry `Try`-admitted (`B:10577-10587`).
8. **Mint the node-seq incarnation** (§1.5), durable before the page goes Live (`B:10588-10599`).
9. **The page Live into BOTH directory slots** (+1 generation each) then ONE barrier (`B:10600-10623`):
   `term += 1`, `state = Live`, `recovered_by_term = 0`, `identity`, `is_manager = false`,
   `home_volume = 0` (**Q-6**), `segments`, `head_hint = ledger_tail_seq = max(page.head_hint, ring0.head)`,
   `ckpt_seq = 0`, `grant` and `slots` cleared. `appender_joins += 1`, `appenders_known = max(live + 1,
   regions)` (`B:10624-10626`).
10. OUTSIDE the verb mutex (not reentrant): a FRESH join over a Free page whose `extent_grant` record is
    non-empty RETURNS the residue first (`appender_join_residue_returned` — a carve whose reply was lost,
    `B:10647-10672`); then the initial grant `want = max(GRANT_EXTENTS_FLOOR + M, hint.grant_extents)`
    (the join's known cost class: the rotor's `M` first images + the SMO floor, `A:1373-1376`,
    `B:10673-10688`); `plane.note_identity(id, node, slot)` for the membership carriage.
Also at the service edge BEFORE step 1: `retire_joiners_death_record` at the LEDGER volume — a standing
`dead_member:` naming the joining `(node, mount slot)` is a PREDECESSOR's and is retired before any page
goes Live under it (`M:733-767`). The identity word is bound to the session's authenticated peer
(`screen_identity_peer`, `M:1374-1403`: strict for `PublishEndpoint`/`LeaveAppender`; for `JoinAppender`
only when the peer IS a member id).

The JOINER's side (`J:1-21`, `J:91-131`): no flock, no superblock repair, the fixed ring replayed as a
PROJECTION (no mint), `JoinAppender` over the wire BEFORE `open_inner`, the region stood up from the
`Joined` reply's words — the RAM grant from the REPLY's grant word, never its own page read (F-C2:
the manager writes the page twice — Live with the grant cleared, then the runs — and a page read may
land the first image; `J:110-118`), `node_seq_base` screened then `NodeSeqHandle.incarnation(base)`.
Pins: `join_appender_over_the_wire_allocates_a_page_ring_and_grant_and_replays_already` (`T-M:754`),
`a_join_replayed_against_a_successor_manager_answers_already` (`T-M:905`),
`a_join_interrupted_after_its_page_replays_already_with_the_grant_it_was_owed` (`T-M:1866`),
`a_joiner_honours_the_joined_replys_grant_word_not_its_page_read` (`T-N:1588`).

### 3.2 LEAVE — `LeaveAppender { identity, appender_id, unclaimed }` → `Left { already }` (`M:286-297`;
`J:3130-3282`)

Preconditions screened before any effect (each a `Rejected` = `manager_verb_rejected`): not one of this
mount's own regions; a directory page exists; the page is `Live` (a `Free` page answers `already`; a
`Recovering`/`Recovered` page is the recovery driver's); the page's `(node, mount slot)` is the caller's
(`J:3151-3203`). `Busy` (retry) while the page names a slot or tree 0 leases it anything — "release them
first (flush-then-transfer to nobody)" (`J:3204-3216`). Then: the unclaimed runs return through the
screened `ReturnExtents` law (§5.3) (`J:3218-3220`); the SMO mutex is taken BEFORE `manager_verbs`
(the consumed seq restates the ledger's roots, so no cycle may be mid-flight — `J:3221-3226`); the
pending GrowRing word is settled (a segment the page names goes with the ring; one it never named is
returned — `J:3227-3231`); the page goes `Free` into BOTH directory slots keeping id, term and
`head_hint` (`ledger_tail_seq = head_hint`), barrier (`J:3232-3241`); only THEN the ring extents'
bits clear (`release_unpublished` each — "no durable page names them any more, and no journal record
ever did", `J:3242-3252`) and land with their own bitmap write + the ledger record the consumed seq
names (`consume_checkpoint_seq_for_bitmap` — the ledger is DENSE, `D:409`; `J:3253-3255`);
`ring_budget_remaining += ring bytes`, `appender_leaves += 1`, `appenders_known` recomputed.
WHY pages-barrier-release-bitmap-barrier and not the reverse: a `Live` page naming FREED extents across a
barrier is the double-custodian window; a crash between the two barriers leaves the extents claimed —
C13's orphan class, never a loss (`B:4633-4648`). The in-process (manager's own) leave is the same
law per region (`B:4649-4680`); a region whose ring is UNCOVERED at the leave is NOT released — its page
stays `Live` for the next open's own-residue recovery, loud (`B:4666-4680`; pin
`a_leave_over_an_uncovered_window_keeps_the_page_live_for_the_next_open`, `T-M:2396`).

### 3.3 GROW — `GrowRing { appender_id, want_bytes }` → `RingGrown { segment: Option<(start, len)> }`
(`M:338-353`; manager `B:10872-11127`; joiner `J:2229-2509`)

Manager side, under `manager_verbs`:
1. Screen: not an own region (`Rejected`); the page `Live` (`Rejected`); the page's identity derives the
   session's peer when the peer is a member id (`B:10913-10951`).
2. **Durable witness FIRST** (§5.3.5): the identity's `appender_hint.pending` — same `appender_id` AND
   same page `term` AND the page does not name it ⇒ answer it VERBATIM (`manager_verb_replays`; a lost
   `RingGrown` re-asked carves nothing more); a named or stale word is settled (cleared / returned)
   before any carve (`B:10952-10980`).
3. `page.segments.len() ≥ 8` ⇒ `None` (`B:10981-10983`).
4. Room = `min(ceiling − page.ring_bytes, budget_remaining)` (`grow_ring_room_bytes`, `A:981-987`;
   `ring_budget_remaining_bytes = heap/16 − rings_in_use`, `A:958-963`); `want_extents =
   min(want, room) / node_size`; 0 ⇒ `None` logged with the remainder (`B:10984-11009`).
5. Carve `want_extents` × `claim_internal` (a partial carve on `NoSpace` keeps what it got; nothing ⇒
   `NoSpace`), sort, take the LONGEST adjacent run, release the rest (`B:11010-11067`). **Segment floor**
   (`grow_ring_segment_floor_extents = ceil(want/2) ≥ 1`, `A:989-999`): a run below it answers `None`
   with every claim released — a table slot is spent only on a run of the doubling class
   (`appender_grow_ring_short_declines`, `B:11042-11061`).
6. The run must sit in no grant record; ZERO it; its alloc deltas + the hint put `{ring_bytes =
   max(grown, hint), pending = {appender_id, term, start, len}}` as ONE control entry; barrier
   (`B:11077-11115`); `ring_budget_remaining -= len`.
Joiner side (`joined_grow_ring_if_due`, run at the cadence):
- Due when `derived > current` or a fresh stall arrived (`stalls != stalls_at_last_grow`), and not
  declined at THIS size since (`grow_declined_at`, `J:2240-2258`; the PR 13i law: never a verb per cycle
  against a limit the manager already named).
- Target = `clamp(max(derived, 2 × current), current, ceiling)` — ONE STEP IS AT LEAST A DOUBLING
  (`J:2259-2267`: the table holds 8 segments; following a converging EWMA by its increments would spend
  them on a storm's first second and stall at a quarter of the derived size); `step = (target − current)
  ↓ node_size`; 0 ⇒ nothing.
- **Drain-then-grow** (`J:2288-2380`): close the growth gate (`growing = true` — every pass parks at
  admission; the Dekker pair `growing` ↔ `passes_inside`/`windows_inflight`, `A:2591-2602`), wait the
  window empty on the region's `drained` notify running cover cycles between waits (a parked pass needs a
  cycle to advance the tail), bounded by one landing ceiling; then cover to the FIXPOINT `head ==
  reusable_upto ∧ no reservation open`, bounded `FIXPOINT_COVER_CYCLES_MAX`; not drained ⇒ open the gate,
  retry next cycle.
- `GrowRing` under the SMO mutex (`J:2381-2383`); `None` ⇒ `grow_declined_at = current`,
  `joined_ring_grow_declined`; `WireDeferred` ⇒ debug, retry next cycle.
- **Replace-when-drained**: `page.segments.push(extent)`, `head_hint = ledger_tail_seq = head`,
  `dir_named = 0`, the page into BOTH directory slots + barrier BEFORE any position is written under the
  new map (`J:2420-2434`); a failed page write pops the segment and leaves the carve under the pending
  witness (`J:2435-2455`); `ring.grown_with(segment)` at the drained head; a refused swap re-writes the
  page WITHOUT the segment (the never-in-place law, `J:2456-2489`); `last_tail = durable_tail = head`,
  `ring_grows += 1`, gate opened (`J:2491-2498`). Region 0's fixed ring never grows (`A:2555-2558`).
Pins: `a_stalled_appender_ring_grows_a_segment_and_its_content_survives` (`T-A:1774`),
`growth_never_swaps_a_ring_with_a_stage_b_window_in_flight` (`T-A:2987`), `a_grow_ring_re_asked_after_a_
lost_reply_carves_once_and_the_death_path_returns_an_unnamed_segment` (`T-N:12350`),
`a_grow_ring_never_carves_past_the_volumes_ring_budget` (`T-N:13852`),
`a_grow_ring_on_a_fragmented_heap_spends_no_table_slot_on_a_short_run` (`T-N:13941`).

### 3.4 REJOIN via `appender_hint`, and the identity binding (`A:200-253`, `D:432`, `B:20549-20584`)

- "Is this page OURS": page 0 of a WRITER open is this node's whatever mount slot it carries (the D0
  flock is the same-host death proof for the manager's page — `owned_by_node`); EVERY other page, and
  every page on a JOINED open, is ours iff it carries this exact `(node, mount slot)` (`is_mount`;
  `page_is_own`, `A:231-253`). WHY the narrowing: under N daemons on one host every same-node page was
  "own residue" and the manager's remount would have adopted a LIVE joiner's ring (`A:236-240`).
- A rejoin presents the predecessor page's identity so the manager answers `already` (`J:94-98`); its
  ring is own residue and is replayed (`J:99-101`; pin `a_joiners_rejoin_over_its_live_page_replays_its_
  window_as_own_residue`, `T-N:3415`); a `Recovered` page is re-adopted with a bumped term and a FRESH
  ring head — a Recovered ring is never replayed (`A:136-139`, `D:432`).
- The hint gives the rejoin its ring size and pool (§3.1 step 6, step 10); a sized rejoin on a fragmented
  heap takes what fits (`T-N:13065`); a rejoin recovers the page word ∩ the record (`RegionGrant::
  recover`, `A:1723-1757` — a page-named extent the record no longer grants was RETURNED after that
  page write; `appender_stale_page_words_dropped`), then the own-residue POOL census restores claimed-
  unreachable extents to UNCLAIMED (`restore_own_pools`, `R:2694`; `appender_pool_restored_extents`;
  pin `a_joiner_killed_with_a_pool_wider_than_its_page_names_rejoins_with_the_pool_whole`, `T-N:12883`).
- A joiner carrying the manager's node token from ANOTHER host refuses naming the machine id
  (`T-N:16849`) — the node token is per host.

### 3.5 Capacity (`A:941-979`, `B:20546-20547`, `D:772-774`)

`ring_budget_bytes = heap / 16` (four times the solo ring's `volume/64` share, a quarter of the heap left
for images at the peak — `D:124`); `appenders_capacity = budget / ring_bytes` where `ring_bytes` at open
is `resolve_sym_ring_bytes(0, volume_len)` = the FLOOR 512 KiB (`B:20546`) — so the published capacity is
`heap/16 ÷ 512 KiB` (a 1 TiB heap ⇒ 131,072; `D:124`'s "128 k at the floor, 16 k at 4 MiB"). The ONE
hard resource a join refuses on (`D:772-774`, KD-SYM-21: `format` never sizes for clients; the soft
capacities are MEASURED gauges). Published `appenders_capacity`, `appenders_live`, `appenders_known`,
`appender_ring_budget_remaining_bytes` (`FC:13610-13623,13687-13690,13833`); `squeezefs appenders` lists
pages (`D:834`); the CLI's volume status prints `appenders_capacity` (`main.rs:8285`).

---------------------------------------------------------------------------------------------------
## 4. THE MANAGER

### 4.1 Election = the D0 ladder's winner; `is_manager` stamped at the JOIN (`B:4507-4631`, `B:20623-20634`)

There is NO separate election (`D:1117` PR 3 "the manager IS the D0 ladder's winner"). The writer's `open`
walks charter §5.7's ladder (flock → PR → `writer_claim`, `Z:src/meta/backend.zig:10-44`); its LAST step
before the tasks spawn is `join_appender_regions` (`B:4517-4631`): every region this mount holds goes
`Live` under its identity with `term += 1`, `recovered_by_term = 0`, `is_manager = (region.id == 0)`
(`B:4565-4578`), `home_volume = 0`; `manager_lease = Held` (`B:4591-4592`); `wero_meta` = the WERO hold
is in force; `joined = true`; ONE barriered `checkpoint_now` makes the join durable (bitmap bits of the
rings' extents, the ledger, every page in its Live form — `B:4598-4600`); `appenders_known` from the
directory's Live count; the initial grant of every declared region with an empty remainder; the slot
leases armed (`B:4614-4629` — the slot-lease spec). The posture word at open (`B:20623-20634`): page 0
`Live` ∧ not mine ⇒ `Peer{node_token}`; else `Vacant`; `Held` is the join's to declare. **The successor
arm** (`B:4536-4560`): a page 0 `Live` under a FOREIGN node at a writer open means the D0 ladder was WON
over its holder (a fresh claim was refused at the gate; a stale one preempted at the device or attested
clear) — the dead manager's ring IS the fixed ring this open already replayed, so the role passes with the
page (KD-SYM-3). `ManagerLease.word()` ∈ {`held`, `peer:0x…`, `vacant`} (`A:2748-2767`).
The manager leases the NATIVE slot (KD-SYM-2, `D:371`), which holds control records (`writer_claim`,
`claim_set`, jobs) and `/`'s dentries — so the D0 heartbeat is a LOCAL append; volume 0's manager is the
set-wide role holder (slot 0 = the root pin). `manager_should_release_role(unreachable_ms, t_owner) =
unreachable > t_owner` is the §5.5.2 vol-0 rule (`A:1085-1092`; `manager_vol0_unreachable`).

### 4.2 ZERO per-op work (`D:132`, `D:322-324`, KD-SYM-17)

Resolving an object's holder is a control-plane PROJECTION of tree 0 (`Leased{appender_id}`) + the
appender→endpoint table through a per-mount `SlotHolderCache`; `ResolveSlot` is the stale-view fallback,
counted apart from `dlm_rpcs` (`D:324`). The manager's verb rate at the operating point: a join storm ≈
195 joins + 390 tree-0 writes in ~1 min; steady ≪ 100/s against ≥ 30 k verbs/s per core at ≤ 32 µs
service (`D:132`, `D:91`). `maintainable_trees` is leased ∪ unleased (`B:5655-5684`): the manager
maintains the UNLEASED slot trees' SMOs in ring 0 and publishes their roots into tree 0
(`publish_forest_roots`, `B:21785`; `F:805-816` roots whose publication moved); a leased slot's root
rides its lessee's page, so the manager publishes nothing for it (`D:1118` PR 4 "the page write records
the publication").

### 4.3 The verb family and the dispatch shape (`M:53-63`, `M:134-353`, `M:776-1372`)

ONE wire verb `VERB_MANAGER_CALL = 0x0500` carrying a `ManagerRequestFrame{schema u32 = MANAGER_SCHEMA
(1), request_id u64, volume u16, call}`; the reply `ManagerReplyFrame{schema, request_id, reply}`
(`M:463-642`). Frame statuses: `OK`; `SCHEMA` (another vocabulary version — "refusing rather than
guessing at a grant-bearing frame", `M:810-820`); `MALFORMED` (undecodable, bounded decode at the CONTROL
cap, `M:644-674`); `NOT_MANAGER` (this node holds no manager lease / not a forest volume, `M:821-827`);
`REFUSED = 48` (the durable witness contradicts the caller — `manager_verb_refusals`, must-stay-0);
`REJECTED = 49` (a wire integer names what durable state cannot — `manager_verb_rejected`, the
buggy/hostile-peer class, kept OFF the must-stay-0 gauge); `DEFERRED = 50` ("not now", nothing written,
retry — `M:64-89`). `ManagerSetService` dispatches by the frame's `volume` ordinal to that volume's
`ManagerService` (`M:1414-1449`); ordinal 0 is the ledger volume (`M:1430-1431`).

Verbs and their executors (dispatch at `M:866-1281`; bodies cited; the slot verbs' BODIES are the
slot-lease spec's):

| verb | reply | executor | idempotent against |
|---|---|---|---|
| `JoinAppender{identity, ring_want_bytes}` | `Joined{…, already, node_seq_base}` | `B:10274` | a `Live` page under the identity (§3.1) |
| `LeaveAppender{identity, appender_id, unclaimed}` | `Left{already}` | `J:3143` | a `Free` page (§3.2) |
| `ExtentGrant{appender_id, want}` | `Granted{runs}` | `B:9424` | the page word ∩ the record covering `want` ⇒ verbatim (§5.3) |
| `ReturnExtents{appender_id, runs}` | `Returned{cleared, already}` | `B:9826` | runs outside the record are `already` (§5.3) |
| `GrowRing{appender_id, want_bytes}` | `RingGrown{segment?}` | `B:10906` | the hint's pending segment this incarnation's page does not name ⇒ verbatim (§3.3) |
| `PublishEndpoint{identity, appender_id, endpoint, pr_key}` | `Published{already}` | `J:3295` | the claim-set entry already holds the same address |
| `ResolveEndpoint{appender_id}` | `Endpoint{endpoint?}` | `J:3415` | a read (holder table, then the live claim set) |
| `PublishRoots{appender_id, roots[WireSlotRoot]}` | `RootsPublished{published, already}` | `B:7015` | tree 0's current words (`already` per root) |
| `AcquireSlots{appender_id, want}` / `AcquireSlot{appender_id, slot}` | `SlotsGranted{slots, already}` / `SlotRefused{slot, holder, g}` | slot-lease spec | tree 0 `Leased{caller, g}` ⇒ `already` |
| `OfferSlot{appender_id, slot, to}` | `Offered` | slot-lease spec | RAM only — an offer that dies with the manager expires (`D:469`) |
| `ReleaseSlot{appender_id, slot, g, words, tails}` | `Released{already}` | slot-lease spec | tree 0 already `Unleased` with those words |
| `ResolveSlot{slot}` | `Holder{appender_id, g}` / `Unleased{g}` | slot-lease spec | a read |
| `DirRenameLock/Unlock{appender_id}` | `DirRenameLocked{already}` / `DirRenameBusy{holder}` / `DirRenameUnlocked{already}` | cross-owner spec | the `dir_rename` record (volume 0 only — `ordinal` screened) |
| `MarkShared/ShareBlock/ReleaseShared` | `Marked{already}` / `SharedGone` / `Shared{inserted, already}` / `SharedReleased{shared, remaining}` | data-plane spec | the SHARED bit / the index entry |
| `BlockGrant/ReturnBlocks` | `BlocksGranted{grants, already}` / `BlocksFull` / `BlocksReturned{cleared?}` | data-plane spec (the ALLOCATION HOLDER serves these, `M:223-226`) | the writer's unconsumed windows |
| `AllocLeaseAcquire/Bitmap/Release`, `RecordRecovered`, `RecordDeath` | `AllocLeaseGranted{term, already, predecessor_bitmap, predecessor_blocks}` / `Recorded{already}` | data-plane / recovery specs (volume 0's manager) | the `alloc_lease:` / `recovered:` / `dead_member:` records |

Every executor records exact-sum `admit / execute / reply` ns on the volume's ledger and the wall
(`ManagerVerbLedger::record`, `A:3038-3051`; `verbs_per_s`, `load_pct = Σ service ÷ wall from the first
verb` — MEASURED, never declared, `A:3053-3074`, `D:774`).

**Reply classification — ONE classifier over every verb's error** (`M:1282-1337`):
`KvError::Rejected` → `Refused{reason}` under `STATUS_REJECTED`; `GrantDeferred` / `LeaseDeferred` /
`HandoverDeferred` → `Deferred{reason}` under `STATUS_DEFERRED`; `JournalReserveExhausted` (the verb's
ring-0 control entry found the USER window full — a verb holds the verb mutex and may not park) →
**`kick_checkpoint_for_ring_park()` + `Deferred`** (`M:1313-1330`; the charter's "answers `Deferred` and
kicks the checkpoint"); every other error → `Refused` under `STATUS_REFUSED`. On the JOINER's side a
`Deferred` is `KvError::WireDeferred` → `EAGAIN` (`MOD:1164-1173,1335`), typed apart from the wire FAILURE
class `Busy`; `GrantExhausted` → EAGAIN; `SlotBusy` → EAGAIN with `RefusalClass::SlotMoved{slot, holder}`
(`MOD:1338-1350`); `Rejected` → EINVAL (`MOD:1367`). The conveyor's batch-failure fan-out clones the
error PER CLASS so every member's errno is the pass's own (`MOD:1196-1299`; F-C3 — before it every
retryable class flattened to `Corrupt` → `EINVAL`). The charter's typed retryable set (`Deferred`,
`SlotMoved`, `StaleHolderView`, `HolderUnreachable`, `MembershipPending`, `ListenerRefused`,
`StaleOwnerEra`) ⊂ `RefusalClass` (`src/error.rs:210`, `src/meta_ship/wire.rs:736`) — the port's
`core/errors.zig` adds them as `EAGAIN`-class rows (0002 row 48: ONE table).

### 4.4 Control entries admit in the USER class (`B:9040-9107`)

`write_control_entry(recs, admit)`: ONE ring-0 entry carrying tree-0 puts + allocator deltas, applied to
tree 0 in RAM after the reservation, written, then BARRIERED (a verb answers only from durable state).
`EntryAdmission::Try` = `try_admit(len, AdmissionClass::User)` — a verb storm competes with user commits
for the admissible window and can never eat the checkpoint reserve the task's own publication relies on
("the rate bound IS the class", `B:9044-9049`); a refusal is `JournalReserveExhausted` → the caller's
retry (§4.3). `TryCheckpoint` keeps ONE SMO claim behind every control entry (a publication is
deferrable, the tail-pinning node's SMO is not — `B:9082-9088`); `Held(pre)` is the door's parking
pre-admission split to the exact length (`B:9089-9106`). The value cap is enforced HERE (`ValueTooLarge`),
never discovered at the flush pass (`B:9065-9075`). Grant carves wider than one entry are CHUNKED by
`pack_grant_deltas` under `MAX_ENTRY_LEN`, each chunk carrying the record AS IT STANDS AFTER the chunk
(`A:1253-1284`; `B:9694-9752`: a kill between two chunks leaves the partial carve the record names —
legal; pin `a_carve_past_one_control_entry_lands_whole_and_the_leave_returns_it_whole`, `T-N:14029`).

### 4.5 The SCREEN per verb — every wire word against DURABLE/derived state before any effect

"Bounded codec = bounded EXECUTION" (`M:850-864`, `D:305`): a frame's integers are claims, never
allocation authority. The screens, by verb:
- `JoinAppender`: identity ↔ session peer (`M:1374-1403`); `ring_want_bytes` clamped `[floor, ceiling]`
  then the budget (§3.1); the chain walk bounded by the heap (`A:2336-2346`).
- `ExtentGrant`: `appender_id == 0` ⇒ REFUSED ("the manager claims from the bitmap it owns", `B:9474-
  9481`); `want` → `clamp_grant_want(want, cap)` (`A:1207-1216`): 0 = the derived size, explicit ≤ cap,
  where cap = the derivation for an in-process region and `grant_extents_wire_cap = max(free_heap /
  (4 × appenders_known), 8)` for a WIRE appender (`A:1218-1232`, `B:9501-9519`); the page word
  INTERSECTED with the record (`extent_grant_stale_page_words`, `B:9520-9550`); the carve bounded by the
  free heap and the record's run cap (`B:9562-9633`).
- `ReturnExtents`: `validate_return_runs` — every `start + len` must not overflow and must lie inside
  `total_extents`, the first offender is the rejection (`A:1094-1108`); then `coalesce_runs` (sorted,
  merged, bounded by the FRAME's length — `A:1110-1166`; the pre-fix path emitted one extent per frame-run
  × record-extent and built an 8 GiB list on the manager) then `intersect_coalesced_with_record` (a merge
  walk, output bounded by the RECORD, `A:1174-1205`); `cleared + already ≡ named` on every reply
  (`B:9866-9873`).
- `GrowRing`: Live page; peer binding through the page's identity; `want_bytes` clamped to the room
  (§3.3).
- `LeaveAppender` / `PublishEndpoint`: identity ↔ peer STRICT; the page Live under `(node, mount slot)`;
  `endpoint` must parse as a socket address (`J:3312-3314`); the `unclaimed` runs through the return law.
- `ReleaseSlot` words (`screen_release_words`, `A:1556-1597`; bounds `ReleaseWordBounds`, `A:1450-1477`):
  `seq_floor` STRICTLY above the grant's recorded floor and ≤ `release_seq_floor_bound = min(max(page.
  seq_offset, granted_floor_max + 1) + head_hint + 2 × ring_len, SEQ_FRONTIER_SANE_MAX)` (`A:1434-1446`:
  the newest durable page is at most one checkpoint stale; a head advances at most one ring length past
  the tail its page declared before the next page write); `cursor` ≥ the grant's and ≤ `GUEST_NS_BASE`;
  `slot_tree_extents` ≤ `total_extents`; `root` = the recorded root verbatim OR a node-aligned heap address
  whose extent the appender's grant record holds (`root_extent_of`, `A:1532-1545`) AND (I/O, the caller's
  second witness) whose node decodes with the word's incarnation stamp and whose records name the
  released slot (`D:305`). A page no appender checkpoint ever wrote (`ckpt_seq == 0`) bounds the floor by
  the sane cap alone (`D:305`).
- `PublishRoots` words: the same bounds MINUS the seq-floor clauses (`screen_publish_root_words`,
  `A:1599-1638`) — a publication moves no floor.
- The frame's `appender_id` on every slot verb must name a `Live` directory page and never one of this
  mount's own regions; an offer's `to` must name a known appender and not the holder (`D:305`).
- `RecordDeath`: a member this manager LISTS as live is never declared dead by a peer's word
  (`screen_record_death`, `M:1206-1225`).
Pins: `a_wire_releases_slot_words_are_screened_before_any_effect` (eight poisoned words + five poisoned
identities, the volume byte-identical after each — `D:305`), `wire_integers_are_never_allocation_
authority` (`T-M:1722`), `manager_wire_bounds_tests.rs`, the `manager_call_frame` fuzz target + the
proptest mirror (`M:19-21`).

**v4 wire codec (port):** the reference's bodies are `bincode` (**Q-7**, the 0002 row-2/53 class). The
port defines each request/reply as an `extern struct` header + length-framed arrays under the cluster
wire's CONTROL cap: `ManagerFrameWire{schema u16, verb u16, status u16, flags u16, request_id u64,
volume u16, reserved[6], body_len u32, xxh3 u64}` then the verb body (`JoinAppenderWire{node_token,
mount_slot, reserved, writer_id, ring_want_bytes}` …); every count checked against `body_len` before an
element is read; `decodeExact`; fuzzed. Variant codes are EXPLICIT u16s (never a declaration index).

### 4.6 The lock order the manager's verbs obey (`A:2534-2548`, `B:9282`, `D:304`, `J:3221-3226`)

`smo` (the per-volume SMO mutex) BEFORE `manager_verbs` for any verb that moves a lease or consumes a
checkpoint seq (the checkpoint task's own order); within a region `grant` BEFORE `page` (the reverse
deadlocked the `.stats` poll against the checkpoint task — F-C4; a static rail refuses a `page` guard
whose block acquires `grant`, `tests/appender_lock_order_tests.rs`); neither guard across an await
(`A:2534-2548`). A wire ask is `Try` under the SMO mutex — nothing parks under it (`D:304`). The port:
a parking `sync.Mutex` for `smo`/`manager_verbs` (lane fibers), spin locks for `grant`/`page` (no await
inside), and a debug-only lock-level rail in `sync/lock_order.zig` (0002 row 209's pattern).

---------------------------------------------------------------------------------------------------
## 5. EXTENT GRANTS

### 5.1 Derivation (`A:1001-1051`, `A:1218-1232`, `D:436`)

`grant_extents_derived(ewma_smo_milli_per_s, failover_bound_ms, free_heap, appenders) =
clamp(2 × ewma_smo × failover_bound_s, GRANT_EXTENTS_FLOOR, free_heap / (4 × appenders))` (`A:1036-1051`;
milli-SMO/s × ms / 1,000,000). WHY each term: `2 ×` = one EWMA window of under-estimate; FLOOR 8 = one
image per pending root swap per cycle (a compaction or split of a slot tree claims ≤ 2 extents) × up to
4 pending swaps per cycle — the mixed tree's SMO budget (physical, `A:1015-1019`); the cap = a quarter of
the free heap spread over the LIVE appenders (`appenders_known`: in-process regions AND wire joiners,
`B:9359-9375`); `SMO_IMAGES_MAX = 3 + 1` (a split's ≤ 3 parts + a root leaf's new root — the INTERNAL
reactive refill asks exactly ONE SMO's need from the reserve, `A:1003-1014`). `failover_bound_ms =
CLIENT_STALE_TTL_SECS × 1000 + ladder_ms + replay_ms` — DERIVED, the two measured terms landing at the join
(`A:1071-1083`, `B:4580-4590`; ≈ 46.5 s shipped, `D:146`). The ewma folds per cycle `ewma = ewma × 7/8 +
rate / 8` off the volume's OWN flush pass's fresh images (`A:2664-2678`; never the process-wide counters).
A WIRE joiner's rate is the JOINER's to measure — the manager's derivation for one reads 0 ⇒ the floor —
so the joiner derives its size and asks `want`, screened at the manager against the heap-share cap alone
(`A:1218-1232`, `D:438`; F-R5: before it every production joiner refilled in ≤ 8-extent grants, 1,483
grants of 2–8 extents in one storm row).

### 5.2 Refill at 50 %, the POOL, the shrink (`A:1354-1384`, `A:1656-2163`, `J:2821-2930`)

- In-process: `refill_due = unclaimed × 2 ≤ refill_reference` (the unclaimed count at the last refill
  decision, `A:1680-1682,2157-2162`); joined: `joined_refill_due(headroom, derived) = headroom < derived/2`
  where `headroom = unclaimed − promised` (`A:1378-1384`, `A:2058-2064`). Refill is proactive at the
  cadence (USER class) AND reactive in the flush pass (INTERNAL class, one SMO's images, drawing the
  compaction floor like the manager's own SMOs — `B:9449-9453`; pin `the_reactive_refill_asks_the_reserve_
  for_one_smos_images_not_the_derived_grant`, `T-M:2759`).
- The RAM grant `RegionGrant{unclaimed, claimed, pending[(extent, gate_seq)], returnable, window_claims,
  granted, returned, refill_reference, promised}` (`A:1659-1693`): `claim()` takes the lowest extent of the
  SMALLEST unclaimed run (a recycled single is consumed before it fragments the page word, `A:1759-1775`);
  `free_pending(extent, gate_seq)` parks on THIS region's tail; `advance_durable(tail)` releases gate ≤
  tail into `returnable` (`A:1899-1914`); `unpark_live_roots` = the root-swap carve-out at replay
  (`A:1839-1858`); `transfer_out/in` = a handover's custody move (bits stay set, the closure reads them as
  returned/granted — `A:1860-1886`); `held = claimed + pending + returnable` (`A:2140-2145`).
- **The POOL** (`A:1984-2009` `recycle`): a drained returnable batch re-enters the region's OWN unclaimed
  set, lowest first, while the pool is below `keep` (the target; `u64::MAX` on a pressure-driven cycle —
  the ring, not the heap, is the bottleneck); what does not fit is the SURPLUS the cadence returns. A
  compaction's claim + retire is then net-zero inside the grant (before: every retired image travelled to
  the manager and came back one cycle later as a fresh carve — a control entry + barrier each way).
  ONE target `joined_pool_target(derived, promised, pool_floor, cap) = clamp(max(derived + promised,
  pool_floor), promised, cap)`, `pool_floor = 8 + M` (`A:1354-1376`): the recycle's keep, the shrink's mark
  and the ask's size. The quiet cadence SHRINKS to it (`shrink_to` — smallest runs first, `A:2011-2044`)
  and the page is rewritten WITHOUT the surplus + barriered BEFORE the return lands (no device page ever
  names an extent the record no longer grants; a failed page write undoes the shrink — `J:2900-2931`).
  The page names the pool's largest 4 runs; the rest is RAM-only, recovered as CLAIMED on a crash and
  restored by the own-residue census (§3.4). Closure `extent_grant_extents ≡ claimed + returned +
  unclaimed` holds across a recycle and a shrink (`A:1996-1997`, `A:2017-2020`; `grant_closure`,
  `A:3077-3092`; `assert_grant_closure` in every manager test, `T-M:148`).
- Promises: a leased leaf's SMO draws the GRANT, never the heap, so its §4.7 admission promise rides the
  region's ledger (`promise_ledger`, `A:1683-1693,2089-2094`; `extent_grant_promised`, 0 at quiesce).

### 5.3 The manager's `ExtentGrant` / `ReturnExtents` executors (`B:9421-9793`, `B:9794-9930`)

Grant (under `manager_verbs`): screen (§4.5) → the page word ∩ the record = the remainder → `remainder ≥
want > 0` ⇒ REPLAY, verbatim (`B:9551-9555`) → carve `want − remainder` (TOPS UP to the pool size, never
an increment — a refill at 50 % lands the pool at the derived size, `B:9556-9562`) in the class (USER:
`claim_user`; INTERNAL: `claim_internal`), partial on `NoSpace`, nothing ⇒ `NoSpace` (the SPACE class,
never `Ok(empty)` counted as a stall, `B:9453-9454`) → cut to the record's run cap → refuse if held
elsewhere → `cache.unretire_extent` each, and for a WIRE appender drop every cached node inside the
granted extents (a stale image under the new pointer — `B:9644-9668`) → the hint put when a wire ask grew
(`B:9669-9693`) → chunked control entries each `alloc(extent)` deltas + the merged record (`B:9694-9752`)
→ `extent_grants += 1`, `extent_grant_extents += claimed` → the PAGE names the remainder's largest runs
(in-process: `write_region_page`; wire: `write_wire_joiner_page_grant` into the joiner's directory pair —
`B:9762-9784`). Return (`return_extents_inner`, `B:9876-9930`+): the fresh record under the verb mutex;
extents outside it are `already`; an in-process region's RAM grant must not disagree with the bitmap —
CLAIMED (a live image) or PENDING (its free parked on the region's tail — the coverage gate) ⇒ REFUSED
(`manager_verb_refusals`; `GrantHeld`, `A:1640-1657`, pin `a_return_of_a_pending_extent_is_refused_until_
the_tail_covers_its_free`, `T-M:2601`); unclaimed/returnable dropped from RAM BEFORE the durable return
(an SMO claiming one between the screen and the bit clear would write into an extent the manager
re-grants); the deltas are `free(extent, retire 0)` — immediately reusable since the appender's own tail
covered the free (`B:9794-9800`) — plus the record WITHOUT them, ONE control entry; a live node of THIS
mount at a returned extent keeps it claimed (`extent_return_live_refusals`, must-stay-0, `A:3008-3011`).

### 5.4 The STRUCTURAL door (`B:5907-5988`, `D:304`)

A grant of an UNLEASED slot to any appender but the manager takes the SMO mutex BEFORE the verb mutex and
`clear_ring0_window_of_unleased(smo, slots)`: cycle the checkpoint under the hold until ring 0's window is
CLEAR of every named unleased slot's records (`slot_record_frontiers_above(tail)` ∩ not leased ∩ not
foreign), bounded by `COVER_CYCLES_MAX` (= 8 barriered rounds — the ONE constant of the four "cycle until
the tail covers X" loops, `D:304`): at the bound a tail that MOVED answers `GrantDeferred{slot, cycles,
frontier, tail_start, tail}` (→ `Deferred`/EAGAIN, `slot_grant_deferrals`, nothing written); a tail that
did not move at all is `Corrupt` (the stuck-reservation class, "a defect, never a longer wait",
`B:5958-5983`). WHY: the manager's structural moves on an unleased tree journal into ring 0, and the next
open judges them against tree 0's FINAL lessee (`Lease` violation, window-scoped); a pass holds the mutex
for its whole pass, so no SMO is in flight when the lease moves and no pass can span the grant
(`B:5907-5926`). The re-check-before-apply alternative was rejected (`D:304`). A manager releasing its
OWN slot raises nothing on ring 0 (its floor IS ring 0's frontier). Pin: `a_wire_grant_of_an_unleased_tree_
waits_for_the_sweep_and_clears_its_window` (×10 stamped, `D:304`).

### 5.5 `GrantExhausted` (EAGAIN) vs ENOSPC, and the stall bound (`MOD:1035-1053`, `D:436`, `B:9455-9472`)

An appender whose grant is empty and whose manager cannot refill REFUSES the SMO (and the append to that
tree) `GrantExhausted{appender, unclaimed, needed}` → EAGAIN — "the heap is not full, the appender is out
of grant"; `needed` is the images the act needs IN TOTAL so the reactive refill asks exactly that
(`MOD:1035-1053`); counted `manager_dependency_stalls` (must-stay-0 at the sized grant — the headroom is
sized so the bound is never reached at the measured rate, `D:436`); published `manager_dependency_stall_
bound_ms = manager_failover_bound_ms` (`FC:13756-13767`). ENOSPC is the HEAP's class alone: a manager
carve that cannot serve one extent answers `NoSpace` (`B:9453-9454`; pin `a_full_heap_on_a_leased_slot_
is_the_space_class_not_a_manager_stall`, `T-M:1988`). The INTERNAL class is admitted during the shutdown
fixpoint (the leave's gate) — behind the ordinary write gate a burst that drained a floor-sized grant in
the final cycles deferred every SMO for ever and the leave kept the page `Live` over an uncovered ring
(`B:9461-9472`). The unreachable-manager seam is in-process (`A:2971-2986`; the port: a field on the
set, not a static — Q-8).

---------------------------------------------------------------------------------------------------
## 6. KNOBS, GAUGES, CONSTANTS, MEASURED NUMBERS

### 6.1 Knobs (reference rows `EK:416-427`; the port's class per `Z:src/core/knobs.zig:57-72`)

| knob | kind / range | default | class (port) | reads |
|---|---|---|---|---|
| `SQUEEZEFS_SYMMETRIC_META` | bool | 1 | `switch_` — `=0` on a stamped volume REFUSES the writable open ("once armed, stays armed"; `-o ro` is the read posture, never a knob value) | the writer door (`D:884`) |
| `SQUEEZEFS_SYM_RING_KB` | int `[512, 32768]` (floor KiB .. `JOURNAL_RING_MAX` KiB, tie-tested `EK:102-108`) | derived `clamp(2 × ewma_commit_bytes/s × CHECKPOINT_MAX_AGE, 512 KiB, volume/64)` | `derived_override` — explicit wins verbatim; **the reference CLAMPS an over-ceiling value with a warning (`A:908-919`) — Q-9: the port REFUSES at the gate (charter §1.6)** | the join's ring ask (`A:903-921`) |
| `SQUEEZEFS_SYM_GRANT_EXTENTS` | int `[8, 65536]` | derived (§5.1) | `derived_override` | `resolve_grant_extents` (`A:1053-1069`) |
| `SQUEEZEFS_SYM_MINT_SLOTS` | int `[1, 64]` | derived `clamp(W/(2 × writers_known), 1, 64)` | `derived_override` (the slot-lease spec's reader; the join's pool floor reads `M` here, `A:1373-1376`) | |
| `SQUEEZEFS_META_PR_WERO`, `SQUEEZEFS_SYM_ALLOW_NON_PR` | bool | 1 / 0 | **NOT CARRIED** (0002 row 188: neither weaker posture exists in the port; `T_owner` is a constant) | |
| `SQUEEZEFS_TEST_SYM_APPENDER_SLOTS` | harness | — | **NOT CARRIED** (PR 2's declared static partition — a stand-in for the lease gate the port lands at once) | |
| `SQUEEZEFS_TEST_META_BUFFERED` | bool | 0 | `test_seam` — already in the port (`Z:…/knobs.zig:103`) | |
| `SQUEEZEFS_CHECKPOINT_MAX_AGE_MS` | int | 1000 | `ab_lever` — already in the port (`Z:…/knobs.zig:127`); the ring derivation's and the flush ceiling's base | |

The in-process seams the reference keeps as statics (`TEST_JOIN_FORGE_NODE_SEQ_BASE` `M:50-51`,
`TEST_JOIN_HOLD_AFTER_PAGE` `B:10638`, `TEST_MANAGER_UNREACHABLE` `A:2975-2986`,
`TEST_PROJECTION_REFRESH_PARK` `J:44-52`) become FIELDS on the port's `AppenderSet`/manager instance
(0002 row 157's law), named as `SQUEEZEFS_TEST_*` registry rows only where a process boundary needs them.

### 6.2 Gauges (names as the reference exports them, `FC:13546-13840`; design `D:995-999`)

Appender family (per volume; `counters.Gauge` unless noted): `appender_id`, `appenders_live` (own regions
whose pages are Live — `A:3193-3207`), `appender_live_pages_at_mount`, `appender_native_slot`,
`appenders_capacity` (derived), `appenders_known` (the directory's Live count the grant cap divides by),
`appender_joins`, `appender_leaves`, `appender_self_recoveries` (counters), `appender_ring_bytes`,
`appender_ring_segments`, `appender_ring_grows`, `appender_flush_ceiling_overruns` (**Tripwire**),
`appender_flush_ceiling_{recovery,service}_extensions`, `appender_flush_ceiling_excused_ns` (exact-sum),
`appender_flush_ceiling_excused_max_ms`, `appender_flush_ceiling_ms` (= `CHECKPOINT_MAX_AGE + 2 × tick` =
1,100 ms, published so the docs cannot drift — `A:2434-2451`), `appender_flush_ceiling_service_cap_ms`,
`appender_pressure_cycles`, `appender_pending_segments_returned` (≈ 0), `appender_pool_restored_extents`
(0 on a clean lifecycle), `appender_ring_budget_remaining_bytes`, `appender_grow_ring_short_declines`,
`appender_stale_page_words_dropped` (≈ 0), `appender_join_residue_returned` (≈ 0),
`appender_directory_reads` (a wire `ExtentGrant` performs exactly ONE — pin `T-N:13771`); per region
`smo_last_cycle` / `smo_folded_total` (the rate's witnesses, `A:2627-2630`). Joiner side:
`joined_wire_words_rejected` (must-stay-0), `joined_ring_grow_declined`, `joined_wire_deferrals`,
`joined_wire_ring_grows`, `joined_control_refusals`.
Manager family: `manager_lease` (`held | peer:<node> | vacant`), `meta_pr_wero`, `manager_failover_bound_ms`
(derived), `manager_dependency_stall_bound_ms`, `manager_dependency_stalls` (**Tripwire** at the sized
grant), `manager_verbs`, `manager_verbs_per_s`, `manager_load_pct` (measured), `manager_service_ns`
(`ShardedLatencyHistogram`: `admit / execute / reply / total`, exact-sum), `manager_verb_replays`,
`manager_verb_refusals` (**Tripwire**), `manager_verb_rejected` (a counter — a hostile peer is not a
cannot-happen), `manager_vol0_unreachable`, `extent_grants`, `extent_grant_extents`, `extent_returns`,
`extent_grant_{granted,claimed,returned,unclaimed}` (the closure's terms, per writer), `extent_grant_
promised`, `extent_grant_conflicts` (**Tripwire**), `extent_grant_stale_page_words`,
`extent_return_live_refusals` (**Tripwire**), `extent_return_run_cap_refusals` (capacity class).
Forest family: `meta_kv_forest_slot_trees_minted`, `meta_kv_forest_root_publishes`,
`meta_kv_forest_page_publications_demoted`, `meta_kv_forest_key_violations` (**Tripwire**),
`meta_kv_forest_reader_window_skips`, `meta_kv_forest_reader_unpublished_children`,
`meta_kv_cursor_page_writes`, `meta_kv_leaf_lease_refusals` (**Tripwire**), `meta_kv_replay_{key,lease,
extent}_violations` (**Tripwire**), `merge_sweep_foreign_skips`. Every one reads 0/`solo`/`vacant` on a
one-member forest (charter §5.5 last line; `FC:13593-13602`).
Closure laws to assert in the smoke: `appender_joins − appender_leaves − appender_recoveries ≡
appenders_live`; `extent_grant_extents ≡ claimed + returned + unclaimed` (`D:1010`).

### 6.3 Constants with WHY

| constant | value | WHY (cite) |
|---|---|---|
| `SYM_RING_FLOOR_BYTES` | `(256 KiB + MAX_ENTRY_LEN).next_power_of_two()` = 512 KiB | the checkpoint carve-out + one max entry must be admissible or a legal tx could never commit; the power of two makes the segment count whole at every node size (`A:851-871`); ≈ 63 windows of user slice on a 4096-grain device (`A:858-869`) |
| ring ceiling | `clamp(volume/64, 8 MiB, 32 MiB)` | an appender never needs more than the whole volume did (`A:873-878`; `Z:src/meta/geometry.zig:40-45`) |
| `RING_SEGMENTS_MAX` | 8 | one 256 KiB segment = 64 pages; eight = the 2 MiB largest growth step before the page's bytes bind (`A:58-61`) |
| `GRANT_RUNS_MAX` | 4 | current run + successor + two returns in flight (`A:62-64`) |
| `APPENDER_PAGE_SLOTS` | 4 | directory pair + two ring-side (two predecessors of slack for a torn newest, `A:66-68`) |
| `SLOT_PAGE_BUDGET` | 108 | `(page − fixed) / entry` (`A:121-125`; §2.2) |
| `GRANT_EXTENTS_FLOOR` | 8 | 2 extents per SMO × 4 pending swaps per cycle (`A:1015-1019`) |
| `SMO_IMAGES_MAX` | 4 | a split's ≤ 3 parts + a root leaf's new root (`A:1003-1014`) |
| `GRANT_EXTENTS_MAX` | 65,536 | more extents than slot trees is a units mistake (`A:1020-1023`) |
| ring budget | `heap/16` | 4× the solo ring's share; a quarter of the heap stays for images at the peak (`A:941-947`, `D:124`) |
| `SEQ_FRONTIER_SANE_MAX` | `u64::MAX/2` | positions are bytes + one per grant (`A:1411-1420`) |
| `COVER_CYCLES_MAX` | 8 | the clause-b audit fail-stops a never-moving tail inside the eighth cycle, so a loop at the bound has a tail that MOVES (`D:304`) |
| flush ceiling | `CHECKPOINT_MAX_AGE + 2 × tick` = 1,100 ms | the trigger fires AT the age; the two tick terms are the wait and the bounded drain (`A:2434-2451`) |
| failover bound | `45 s + ladder + replay` ≈ 46.5 s | `CLIENT_STALE_TTL_SECS` + the two measured terms (`A:1071-1083`, `D:146`) |
| EWMA | `7/8 · ewma + 1/8 · rate` | per checkpoint cycle (`A:2664-2696`) |
| `ZERO_CHUNK` | 4 MiB | one sequential write per extent at the carve (`A:2198-2203`) |
| `K`, ordinal bits | 38, 25 | 2.7 × 10^11 mints per incarnation (200 SMOs/s for 43 years) × 33.5 M incarnations (`Z:…/node_seq.zig:24-35`) |

### 6.4 The 12,500-client operating point (`D:113-158`; arithmetic on measured constants)

N = 12,500 writers, W = 65,536, V = 64 (recommended) / V = 8: appenders per volume `N/V` = **195** / 1,563;
regions incl. foreign — steady ≈ 195 + a few hundred, worst 12,500 × 512 KiB = 6 GiB of rings + 98 MiB
of pages (inside the 64 GiB budget); per-appender ring at the 100 creates/s norm = the 512 KiB floor, at
the 8 k peak 4 MiB ⇒ 195 × 512 KiB = **98 MiB** (norm) / 780 MiB (peak) per volume; ring budget `heap/16`
= 64 GiB on a 1 TiB heap ⇒ 16 k appenders at 4 MiB, 128 k at the floor — **binds first at V = 1 and the
8 k peak** (49 GiB ≈ the budget; V ≥ 2 clears it); grant at peak: 5 SMO/s × 2 × 46.5 s ≈ 470 extents =
117 MiB per appender ⇒ 23 GiB per TiB at V = 64 (2.3 %), **18 % at V = 8 — the second thing that binds**;
tree-0 lessee state 1,024 × ~60 B ≈ 56 KiB (one leaf) / 450 KiB; manager verbs: join storm ≈ 195 × 5 ms ≈
1 s of manager time (8 s at V = 8), steady ≪ 100/s vs ≥ 30 k/s per core (owner `execute ≤ 8 µs, total ≤
32 µs` measured at the veth floor, `D:91`); join storm of 32 mounts measured 3.66–4.06 s with 250–291
verbs, Σ service 12–16 s (58 ms per verb under 31 concurrent joins — `D:918`); PR registrants ≤ N per
namespace, report ≈ 800 KiB at 12,500 (`D:154`). Format constants: ≈ 230 B entry per create; 256 KiB
nodes ≈ 800 files/leaf; cold leaf read 85–120 µs (`D:101`).

---------------------------------------------------------------------------------------------------
## 7. TESTS — the contracts to port (name → one sentence; venue)

Legend: [P] portable unit (runs on macOS too), [L] Linux backend e2e, [N] needs N daemons.

**Page / directory / ring (T-A):**
- `slot_page_budget_derives_from_the_pages_fixed_part` [P] (`T-A:178`): the budget is `(page − fixed)/entry`, never a literal — port as a comptime assert + tie test on 108.
- `appender_page_round_trips_at_every_bound_and_refuses_past_them` [P] (`T-A:223`): 8 segments / 4 runs / 108 slots encode and decode; one more of each is refused by the encoder.
- `appender_page_decode_is_total_and_canonical` [P] (`T-A:261`): a non-zero reserved byte, an unsorted slot list, an unknown state byte, `is_manager = 2`, a foreign layout byte each refuse; decode∘encode = id — fuzz it.
- `newest_valid_page_wins_and_a_torn_newest_falls_back_to_its_predecessor` [P] (`T-A:326`): four images, the highest valid generation wins; a torn newest falls back; a ForeignLayout in any slot refuses the whole read.
- `page_slot_rotation_covers_the_four_slots` [P] (`T-A:360`); add the directory-first tie: `pageWriteSlots(mask, g)` over every mask.
- `directory_header_round_trips_and_pairs_per_extent_derive_from_the_node_size` [P] (`T-A:370`): `KVAD` round-trip; 31 pairs at 256 KiB, 7 at 64 KiB.
- `ring_size_derivation_floor_ceiling_and_capacity_tie` [P] (`T-A:398`): floor 512 KiB, ceiling = the solo clamp, capacity = `heap/16 ÷ ring`.
- `routing_slot_and_forest_slot_convert_both_ways_around_the_native_slot` [P] (`T-A:436`).
- `the_default_format_writes_appender_zeros_page_pair_and_the_first_directory_extent` [L] (`T-A:531`): format writes the Free page at slot 1, zero elsewhere, the `KVAD` header, `sb.appender_dir` set.
- `a_segmented_ring_maps_pages_through_its_segment_table_and_replays_across_segments` [P] (`T-A:593`).
- `a_solo_ring_is_one_segment_and_growth_needs_a_drained_ring` [P] (`T-A:654`).
- `appender_violations_key_lease_extent_are_each_detected` [P] (`T-A:765`): the replay detector's three classes, each loud.
- `a_writer_joins_appender_zero_and_writes_its_page_per_checkpoint` [L] (`T-A:1078`): `is_manager`, term 1, `manager_lease = held`, the page's tail/ckpt_seq move per cycle.
- `own_residue_is_recovered_at_rejoin_with_a_bumped_term` [L] (`T-A:1176`); `a_same_node_live_page_under_another_mount_slot_is_own_residue_under_d0` [L] (`T-A:1234`); `a_foreign_live_page_refuses_the_writer_open_and_a_probe_lists_it` [L] (`T-A:1298`); `an_own_recovering_page_is_own_residue_and_the_join_takes_it_back` [L] (`T-A:1394`).
- `two_appenders_commit_into_two_rings_and_replay_to_the_union_digest` [L] (`T-A:1468`): two regions on one volume, replay digest ≡ one writer's.
- `a_stalled_appender_ring_grows_a_segment_and_its_content_survives` [L] (`T-A:1774`); `growth_never_swaps_a_ring_with_a_stage_b_window_in_flight` [L] (`T-A:2987`) — model the Dekker pair.
- `a_re_carved_ring_never_replays_its_predecessor_incarnations_entries` [L] (`T-A:2699`): the zeroing law.
- `a_clean_unmount_returns_a_declared_regions_ring_extents_to_the_heap` [L] (`T-A:2647`); `a_region_rejoined_after_a_clean_unmount_is_found_live_after_a_crash` [L] (`T-A:2572`).
- `the_flush_ceiling_is_the_checkpoint_age_and_a_parked_device_moves_the_overrun_counter` [L] (`T-A:2001`); `a_steady_write_stream_at_the_default_cadence_never_overruns_the_flush_ceiling` [L] (`T-A:2880`).
- `the_stats_reader_never_deadlocks_against_a_grants_page_update` [L] (`T-A:3152`): the `grant` → `page` order.

**Manager / grants (T-M):**
- `the_extent_grant_record_round_trips_and_is_normalized` [P] (`T-M:161`): non-normalized runs refuse.
- `grant_size_derivation_floor_cap_and_the_knob_tie` [P] (`T-M:199`): §5.1 restated.
- `the_ram_grant_claims_the_smallest_run_first_parks_frees_on_its_tail_and_recycles_past_it` [P] (`T-M:288`); `the_page_names_the_largest_runs_and_the_rest_stay_in_the_ram_pool` [P] (`T-M:2052`); `a_grant_run_overlapping_held_extents_adds_only_the_extents_the_grant_does_not_hold` [P] (`T-M:259`).
- `a_declared_region_joins_with_a_grant_in_tree_zero_and_on_its_page` [L] (`T-M:376`); `a_declared_regions_smos_journal_into_its_own_ring_and_claim_inside_its_grant` [L] (`T-M:442`).
- `an_exhausted_grant_with_the_manager_unreachable_stalls_loud_and_never_loses_a_record` [L] (`T-M:529`): `GrantExhausted` → EAGAIN, `manager_dependency_stalls += 1`, nothing lost.
- `a_return_replayed_against_durable_state_answers_already` [L] (`T-M:618`); `an_unconsumed_grant_is_answered_verbatim` [L] (`T-M:1931`); `a_return_drops_the_extents_from_the_ram_grant_and_refuses_a_live_image` [L] (`T-M:2189`); `a_return_of_a_pending_extent_is_refused_until_the_tail_covers_its_free` [L] (`T-M:2601`); `a_returns_run_cap_refusal_is_the_capacity_class_on_its_own_gauge` [L] (`T-M:2679`).
- `the_manager_call_frames_round_trip_and_the_wire_schema_is_six` [P] (`T-M:682`) — port as the v4 wire codec's round-trip + fuzz.
- `join_appender_over_the_wire_allocates_a_page_ring_and_grant_and_replays_already` [N] (`T-M:754`); `a_join_replayed_against_a_successor_manager_answers_already` [N] (`T-M:905`); `a_join_past_appenders_capacity_refuses_naming_the_volume_count_lever` [N] (`T-M:955`); `a_joins_ring_is_bounded_by_the_ring_budgets_remainder` [N] (`T-M:1001`); `a_join_storm_of_32_completes_inside_the_bound_and_grows_the_directory_chain` [N] (`T-M:1079`); `a_join_interrupted_after_its_page_replays_already_with_the_grant_it_was_owed` [N] (`T-M:1866`).
- `wire_integers_are_never_allocation_authority` [N] (`T-M:1722`): a `ReturnExtents` of 250 K copies of one run, a run past the volume, a `want` past the cap — REJECTED, bounded.
- `the_grant_cap_counts_every_live_appender_wire_joiners_included` [N] (`T-M:2253`).
- `a_full_heap_on_a_leased_slot_is_the_space_class_not_a_manager_stall` [L] (`T-M:1988`); `the_reactive_refill_asks_the_reserve_for_one_smos_images_not_the_derived_grant` [L] (`T-M:2759`); `a_batch_failed_at_resolve_by_grant_exhaustion_fans_out_the_grant_class_to_every_member` [L] (`T-M:2854`).
- `a_leave_over_an_uncovered_window_keeps_the_page_live_for_the_next_open` [L] (`T-M:2396`); `a_leased_trees_smo_inside_the_final_cycle_survives_a_clean_leave` [L] (`T-M:2336`).
- `a_flat_mount_has_no_manager_lease_and_no_grants` [L] (`T-M:2952`): the one-member forest's gauges read vacant/0.

**N-daemon (T-N):**
- `a_second_daemon_joins_over_the_wire_and_commits_into_its_own_ring` (`T-N:328`); `three_daemons_hold_disjoint_slots_and_commit_into_three_rings` (`T-N:4673`); `concurrent_storms_on_four_writers_never_cross_a_record_into_another_appenders_leaf` (`T-N:873`).
- `a_clean_leave_and_rejoin_of_one_identity_never_regrants_a_live_image` (`T-N:706`); `a_joiners_rejoin_over_its_live_page_replays_its_window_as_own_residue` (`T-N:3415`); `a_rejoined_identitys_fresh_region_is_never_recovered_under_its_predecessors_death_record` (`T-N:3733`).
- `a_forged_node_seq_base_in_the_join_reply_is_refused_before_anything_is_installed` (`T-N:1301`); `two_joined_appenders_never_mint_an_equal_node_seq` (`T-N:1475`).
- `a_joiner_honours_the_joined_replys_grant_word_not_its_page_read` (`T-N:1588`); `a_joined_reply_word_wider_than_the_page_lands_whole_in_ram_and_largest_on_the_page` (`T-N:1682`); `a_joiners_stats_reader_never_deadlocks_against_its_own_checkpoint_page_writer` (`T-N:1803`).
- `a_joiners_extent_supply_under_a_create_storm_grows_its_ring_and_recycles_its_grant` (`T-N:11980`); `a_grow_ring_re_asked_after_a_lost_reply_carves_once_and_the_death_path_returns_an_unnamed_segment` (`T-N:12350`); `a_joiner_killed_with_a_pool_wider_than_its_page_names_rejoins_with_the_pool_whole` (`T-N:12883`); `a_sized_join_on_a_fragmented_heap_carves_what_fits_the_table_never_refusing` (`T-N:13065`); `a_quiet_joiners_pool_above_its_target_shrinks_at_the_cadence_and_asks_for_nothing` (`T-N:13168`); `a_carve_whose_reply_was_lost_is_reconciled_at_the_ids_fresh_join` (`T-N:13287`); `a_shrinks_return_never_leaves_the_page_naming_a_returned_extent` (`T-N:13429`); `a_rejoin_adopts_the_page_word_intersected_with_the_record` (`T-N:13568`); `a_joiners_smo_rate_reads_its_own_volumes_images` (`T-N:13691`); `a_wire_extent_grant_walks_the_appender_directory_once` (`T-N:13771`); `a_grow_ring_never_carves_past_the_volumes_ring_budget` (`T-N:13852`); `a_grow_ring_on_a_fragmented_heap_spends_no_table_slot_on_a_short_run` (`T-N:13941`); `a_carve_past_one_control_entry_lands_whole_and_the_leave_returns_it_whole` (`T-N:14029`).
- `a_join_the_manager_defers_is_the_retryable_class_never_busy` (`T-N:16241`); `a_join_at_an_unreachable_manager_is_the_transport_class_the_mount_path_retries` (`T-N:9298`); `the_joined_door_refuses_without_the_plane_and_never_writes_a_control_entry` (`T-N:8087`).
- `a_live_joiner_follows_a_manager_failover_to_the_successors_listener` (`T-N:6963`); `a_joiners_checkpoint_cycle_meeting_a_dead_manager_completes_and_follows_the_successor` (`T-N:7667`); `a_two_daemon_storm_survives_the_managers_remount_with_zero_violations` (`T-N:4535`).
- `a_joiner_carrying_the_managers_node_token_from_another_host_refuses_naming_the_machine_id` (`T-N:16849`).

**The N-daemon harness (how several daemons run on one host against one volume set).** In-process
(`T-N:1-17`, `C:415-497`): the MANAGER is a real `KvMetaBackend` opened through `open` (the D0 ladder —
flock, claim); a `HoldersVenue` stands up ONE loopback `RpcListener` (`127.0.0.1:0`, 2 service threads,
the venue secret) fronting `MetaShipService` + `ManagerSetService::new(&routed.volumes)`, registers the
endpoint in every volume's holder table and installs the cross-volume shipper (`C:429-480`); each JOINER
is a second/third real backend set opened through `open_routed_meta_set_joined` with a
`JoinedSetAdmission{manager_endpoint, secret, peer_id = member id derived from the identity, identity}`
(`T-N:104-123`) — the identity is the manager's NODE token (one host) at mount slot `0x5000 + n`
(`T-N:87-101`), the production shape of N daemons on one box; a `Knobs` struct applies/clears the
process-global knobs around each open (`C:85-100`); `reset_process_state` clears every process-global
holding/sink/park between tests (`T-N:39-54`). Every joiner act travels the real wire; a manager
failover is the venue re-stood at the OLD address (`stand_up_at(…, install_shipper = false)`, `C:433-438`).
Out of process: `tests/mw_fleet.sh create N [--vm=V]` builds N daemons (member 0 the writer with an
explicit per-mount identity, readers optionally in their own netns with `netem`/`partition`/`kill`
verbs — `mw_fleet.sh:10-178`); `sym-two-host` adds a qemu/KVM guest sharing the metadata LUN over
nvmet-tcp on a host-only TAP — two KERNELS, the O_DIRECT coherence pin (`D:802-805`).
**Port venue:** the Linux container runs several daemons as separate PROCESSES on one file-backed set
(the charter's N-daemon shape needs no VM); the in-process form maps onto N `MetaSet`s in one test binary
over `std.testing.io`, the manager's listener on `127.0.0.1:0`, and the port's `SQUEEZEFS_TEST_FLEET_SHARE`
pins the budget share. The two-host pin stays a venue script (the `container` tooling has no second
kernel); the O_DIRECT posture it pins is already the port's (`Z:src/io/uring_fs.zig`, charter §5.1).

---------------------------------------------------------------------------------------------------
## 8. QUIRKS — per decision 0002; each a ledger row before the port diverges

| # | reference quirk | why it is poor | what the port does |
|---|---|---|---|
| Q-1 | Tree-0 keys mix spellings: the port's rung-5 `slot_state:{s}` is DECIMAL ASCII (`Z:…/slot_state.zig:17-35`) while every reference control key is prefix ‖ BE binary (`SS:31-33`); decimal keys do not sort in slot order (`slot_state:10 < slot_state:2`) so the census and the open's prefix walk read slots out of index order and need up to 16 B | one convention per tree; a range walk should yield index order (the reference's stated reason) | v4: every tree-0 key is `prefix ‖ BE fields` — `slot_state:`/`slot_tails:` ‖ `slot u16 BE` (the routing slot; the port's W is u16), `extent_grant:` ‖ `appender_id u32 BE`, `appender_hint:` ‖ `node_token u64 BE ‖ mount_slot u32 BE`, the volume-0 families likewise; the charter's `{s}` spelling is the LOG's. The rung-5 codec's decimal key is replaced before any release (0002 §2: obligations start at our first release) |
| Q-2 | `slot_tails` discriminates inline vs spilled by the sentinel count `0xFFFF` (`SS:489-492`); `dead_member` decodes THREE lengths (`AL:309-320`); `slot_state` has two variant lengths (`SS:54,63`) | sentinels in counts and decode-by-length are not versioned formats (0002 row 41's class) | one fixed head per record with `version u16 ‖ flags u16`, flags for inline/spilled and `early`, ONE length per record (`slot_state` 64 B both variants), every reserved word checked zero |
| Q-3 | `dir_rename.since_ns` is `SystemTime::now()`; `dead_member.ts_ms`, `recovered.ts_ms`, `custody_quarantine.until_ms` are Unix ms | mixed units (ns vs ms) for one clock | every cross-process timestamp is `realtime_ns` (the clock must be realtime — a quarantine deadline is compared on another host — stated on the line via `clock.realtimeCoarseS`-class accessors); monotonic only for in-process stamps |
| Q-4 | `alloc_lease` carries `writer_id u128` (`AL:187`) and the page carries `writer_id u128` (`A:82`) | the port's writer id is a derived u64 (0002 row 185) | u64 everywhere; the page identity is 24 B |
| Q-5 | `appender_alloc` named in the design/charter, never built (`D:356,487`; no code) | a record nothing reads is dead code (`tests/rails.zig`) | not defined; the directory + `appender_dir` IS the allocation; the charter clause is annotated |
| Q-6 | `home_volume` is written 0 by both the join (`B:10608`) and the wire join (`B:4576`) — a u8 the design means as the member position (`D:418`) | a field that never carries its meaning; u8 cannot name a u16 member position | v4 `home_volume u16` = the joiner's HOME volume ordinal from the frame (the membership shard key of §5.5 "sharded by home volume"); the manager writes what the join frame carried, tie-tested against the set's ordinal |
| Q-7 | Wire bodies are serde/`bincode` (`M:18-21,650-674`) with verb identities = the enum's DECLARATION INDEX (`M:355-361,374-383`: "the wire encodes the declaration index") and `String` reasons/endpoints | a serialization-library artifact as the wire (0002 rows 2/53); a reordering is a silent re-numbering; `String` on a control frame | `extern struct` frames with explicit u16 verb codes, length-framed arrays checked against `body_len`, `decodeExact`, xxh3 per frame, fuzzed; the endpoint is a `sockaddr` image; reasons are typed codes the client renders |
| Q-8 | Test seams as process statics (`TEST_MANAGER_UNREACHABLE` `A:2975`, `TEST_JOIN_FORGE_NODE_SEQ_BASE` `M:50`, `TEST_JOIN_HOLD_AFTER_PAGE` `B:10638`, `TEST_PROJECTION_REFRESH_PARK` `J:44`); `APPENDER_DIRECTORY_READS` a process-wide static (`A:2299`) | process globals leak across mounts and tests (0002 row 157, row 102) | fields on the `AppenderSet` / manager instance; `appender_directory_reads` a per-volume `ShardedCounter` |
| Q-9 | `SQUEEZEFS_SYM_RING_KB` above the volume's ceiling is CLAMPED with a warning (`A:908-919`) | clamping is banned (charter §1.6; 0002 row 14) | the gate refuses naming the ceiling; the registry range is `[512, 32768]` KiB and the per-volume ceiling is checked at open |
| Q-10 | The capacity is computed at open with the FLOOR ring (`B:20546-20547`) while a join may carve a hinted/explicit ring far above it — `appenders_capacity` then over-states what the budget admits; the join refuses on `live ≥ capacity` alone (`B:10347`), the remainder law catches the rest | two laws for one resource | publish `appenders_capacity = budget_remaining ÷ floor` (live) beside the format-time `heap/16 ÷ floor`; refuse on the REMAINDER (`join_ring_bytes_under_budget` cannot reach the floor) — one law |
| Q-11 | `recovered_by_term` and `term` are both on the page but the layout byte 55 doubles as a one-byte version (`A:104-113`); `reserved[6]` twice to align tables | a version in a byte, two reserved runs | `layout_version u16 @4` right after the magic (the superblock's convention, 0002 row 43), ONE reserved discipline, xxh3 LAST |
| Q-12 | The reference's `GrantRun.len` is u32 on the record and the page but u64 on the segment table; the page grant run carries `reserved u32` (`A:99-100`) | two widths for one unit | v4 page grants `{start u64, len u64}` (the segment table's shape); the tree-0 record keeps `len u32` (an extent count never exceeds u32 on a volume the format admits) with `reserved u32` checked zero |
| Q-13 | The manager's `ExtentGrant` for an in-process declared region vs a wire joiner follows two code paths (`set.region(id)` vs the directory read, `B:9393-9420,9762-9784`) because PR 2's static partition pre-dated the lease gate | a harness posture inside the product path | the port has ONE appender shape: every non-manager appender is a WIRE joiner (the N-daemon test is the venue); the `TEST_SYM_APPENDER_SLOTS` seam and `declared_partition` (`A:2470-2528`) are not carried |
| Q-14 | Capacity/`live` is computed by whole-directory walks per verb (`read_directory` on every join/grant/leave/grow — `A:2294-2305`); the manager caches nothing of the directory | O(appenders) device reads per verb (195 pairs ≈ 400 page reads at the operating point); accepted by the reference as the F-B1 term | the port keeps the DEVICE read as the truth (shared LUN — a RAM mirror is stale by construction, `D:780`) but reads the ONE page the verb names plus a cached `live` count refreshed per cadence; `appender_directory_reads` counts whole walks (joins only) |

---------------------------------------------------------------------------------------------------
## 9. PORT CHECKLIST (what rung 6a of this spec lands; red-first, portable where it can be)

1. `src/meta/appender.zig` [P]: `AppenderPageWire` (§2.2) + `DirHeaderWire`, total decoders, `classify`
   {valid, blank, corrupt, foreign_layout}, `newestValid`, `pageSlotFor`, `dirNamedMask`,
   `pageWriteSlots`, the device geometry (`appender0PageOffsets`, `appender0RingExtent`,
   `dirPairOffsets`, `ringSideOffsets`, `firstSegmentRingPart`), derivations (`ringFloor`, `ringCeiling`,
   `ringBytesDerived`, `ringBudget`, `appendersCapacity`, `joinRingUnderBudget`, `growRoom`,
   `growSegmentFloor`, `ringSegmentsThatFit`, `grantDerived`, `grantWireCap`, `clampGrantWant`,
   `joinedPoolTarget`, `joinedPoolFloor`, `refillDue`, `releaseSeqFloorBound`, `screenReleaseWords`,
   `screenPublishRootWords`, `validateReturnRuns`, `coalesceRuns`, `intersectWithRecord`) each with a tie
   test; `RegionGrant` as a plain value type with its invariants (`granted ≡ held + returned + unclaimed`)
   property-tested; the `ManagerVerbLedger` on `counters.*`.
2. `src/meta/control_records.zig` [P]: §1's v4 `extern struct`s + BE keys, canonical total codecs,
   fuzzed; `routing/slot_state.zig` grows into it (Q-1/Q-2).
3. `src/meta/geometry.zig`/`format.zig` [L]: `enable-symmetric`'s ONE act — the directory extent claimed
   and zeroed, the `KVAD` header FIRST, appender 0's `Free` page at slot 1, the superblock with the bit +
   `appender_dir` (`kv_alloc_superblock.md:56-58`); the journal ring's first 16 KiB reserved from the
   ring proper on a stamped volume (`journal_core` geometry takes `appender0RingExtent`).
4. `src/meta/journal*.zig` [P]: the segment table in the ring geometry; `seq_offset` on the reservation
   stamp + `raiseSeqFloor` + recovery (§2.7) with its model; `grownWith` replace-when-drained and the
   growth Dekker model (`growing` ↔ `passes_inside`/`windows_inflight`).
5. `src/meta/manager.zig` [L]: the D0 join (§4.1: `is_manager`, `manager_lease`, the barriered cycle),
   `write_control_entry` in the USER class with the `Deferred` + checkpoint kick, the verb executors of
   §3/§5 under `smo → manager_verbs` and `grant → page`, the structural door (§5.4), the screens (§4.5).
6. `src/meta/joined.zig` [L]: the joiner's door (no flock; projection replay; `JoinAppender` before
   open; the reply's grant word; `screenIncarnationBase` → `NodeSeqHandle.incarnation`), its cadence
   (fold rates, refill at 50 %, recycle/shrink, `ReturnExtents`, `GrowRing` drain-then-grow, the page
   into both directory slots), its leave.
7. `src/wire/manager_wire.zig` [P]: the v4 frames (Q-7), total + fuzzed; `core/errors.zig` gains the
   retryable rows (`Deferred`, `SlotMoved`, `StaleHolderView`, `HolderUnreachable`, `MembershipPending`,
   `ListenerRefused`, `StaleOwnerEra`, `GrantExhausted`) as `EAGAIN`-class.
8. Knob rows (§6.1), gauges (§6.2) on `counters.*` with the `stats` namespace, the rails green; the
   `appender clear <uri> <id>` verb's REFUSAL ladder (live-mounted flock, heartbeat-fresh claim on the
   volume OR volume 0, own residue, `appender clear 0`) in `cli/` (`R:3045-3170`) — its recovery body is
   the §5.9 spec's.
9. Tests of §7 in the stated venues; the evidence note records: the 108 budget tie, `capacity =
   heap/16 ÷ 512 KiB` on the venue's volume, a 3-daemon join/commit/leave with the closure laws exact,
   `manager_verb_refusals = 0`, `extent_grant_conflicts = 0`, `joined_wire_words_rejected = 0`, and the
   measured `manager_service_ns` per verb on the venue.
