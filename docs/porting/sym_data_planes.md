# Porting spec — the symmetric DATA planes: allocation lease, data bitmap, ranged block grants, frees, the shared-block index, custody (rung 6)

Scope: charter §5.5 "Data-plane planes under the symmetric model", §5.6 (refs; `block_ref_probes`),
§6.1–§6.2 (the ONE-writer allocator + reclaim already landed), §8 (`df`); decisions 0002 (rows 44,
45, 104, 175, 185) and 0003 (Q3). The reference is EVIDENCE (0002 §1); every quirk is in §9 with the
better design and the port seam it fills.

Reference anchors (`FILE:Lnnn`, all under `/Users/joberdorf/Source/squeezefs/`):
`AL` = `src/meta_backend/kv/alloc_lease.rs` (the lease, the death ledger's records, the holder, the
arm) · `DAB` = `src/data_alloc_bitmap.rs` · `BG` = `src/block_grant.rs` · `BA` =
`src/block_allocator.rs` · `FG` = `src/free_grace.rs` · `DC` = `src/data_custody.rs` · `DG` =
`src/data_grant.rs` (the S9 write-custody protocol — NOT the allocation lease; see §0) · `CRC` =
`src/custody_revoke_core.rs` · `SRC` = `src/shared_ref_core.rs` · `SR` =
`src/meta_backend/kv/shared_refs.rs` · `SF` = `src/shipped_free.rs` · `RT` = `src/routing.rs` ·
`MGR` = `src/meta_ship/manager.rs` · `PUB` = `src/meta_ship/publish.rs` · `SS` =
`src/meta_backend/kv/slot_state.rs` · `MB` = `src/membership.rs` · `MW` = `src/membership_wire.rs` ·
`LCC` = `src/lease_clock_core.rs` · `FSCK` = `src/fsck.rs` · `LOOM` = `loom-models/src/lib.rs` ·
`DSM` = `docs/design-symmetric-metadata.md` · `DMP` = `docs/design-mw-data-alloc-partition.md` ·
`EK` = `src/env_knobs.rs` · `ACC` = `.benchmarks/2026-09-19-sym-acceptance.md`.
Port anchors (`P/…` under `/Users/joberdorf/Source/squeezefs-zig/src/`): `P/data/block_alloc.zig`
(`PBA`), `P/data/block_refs.zig`, `P/data/pack.zig`, `P/data/plane.zig`, `P/data/patch_w1.zig`,
`P/data/reclaim.zig`, `P/meta/alloc_ext.zig`, `P/meta/key.zig`, `P/meta/record.zig`,
`P/meta/tx.zig`; the earlier spec `docs/porting/data_alloc_reclaim.md` §11 (`DAR §11`) lists the
seams the rung-4 allocator left for this rung.

## 0. Orientation — what each reference module IS, and the two designs that overlap

- **`DMP` is the RETIRED design** (residue-class lanes `b % W`, `MAX_LANES = 16`, `alloc_lane:` xattr
  records on ino 1; `DMP:L56-92`, `L154-160`). The symmetric design replaced it with the floating
  allocation lease + ranged grants (`DSM:L514-518`: "Residue lanes ⇒ ranged block grants (KD-SYM-9)
  … `MAX_LANES` and the power-of-two stranding table retire"; `BG:L5-10`). What survives from `DMP`
  is ONE argument, the zombie window: a fenced-but-live predecessor may be DMA-ing into offsets it
  minted and never published, derived state cannot see them, so "the recovery floor is the grant's
  END" (`DMP:L144-152`, `L193-221`; `DSM:L534`; `BG:L28-31`). Nothing else of `DMP` is ported.
- **`DG` (`data_grant.rs`) is the S9 WRITE-CUSTODY protocol** (`JOIN/ACQUIRE/RENEW/RELEASE/RECLAIM`
  on verb block `0x0200..=0x02FF`, `DG:L184-213`): custody of a FILE's bytes from its slot holder, a
  fencing token per grant, revocation pull-based on the renewal (`DG:L62-82`). It is the "custody"
  half of item (6). The ALLOCATION lease lives in `AL` + `BG` + `DAB`; the only `DG` state the
  allocation planes consume is the dead epoch + quarantine (`DG:L1056-1063`, `L2238-2292`).
- Reference scaffolding not carried: every `once_cell` static registry (`HOLDINGS`, `ALLOCATORS`,
  `FREE_TARGETS`, `FREE_VENUES`, `REPLAYED_DELTAS`, `MOUNT_SEEDS`, `SET_HEAPS`, `CUSTODY_QUARANTINE`:
  `AL:L815-818`, `L1146-1148`, `L1212-1214`, `BG:L596-597`, `DAB:L915-917`, `DG:L6165-6170`) becomes
  a field of the port's `DataPlane` / `MetaSet` (§9 Q1).

## 1. The allocation lease

### 1.1 Records in tree 0 of volume 0 — byte-exact (reference) and the v4 `extern struct` (port)

Reference key: `b"alloc_lease:" ‖ vol_tag u64 BE` (20 B, `AL:L66-67`, `L91-96`); the whole family is
one inclusive range `alloc_lease_key(0)..=alloc_lease_key(u64::MAX)` (`AL:L99-101`). Value, LE,
version byte first (`AL:L73-76`, `L175-200`):

```text
[0]      version u8 = 1
[1..9)   holder.node_token u64     [9..13)  holder.mount_slot u32    [13..29) holder.writer_id u128
[29..33) holder_appender_id u32    [33..35) home_vol u16             [35..43) control_ino u64
[43..51) blocks u64                [51..59) term u64                 [59..61) n_refs u16
then n × { vol u16 ‖ start u64 ‖ len u64 } (18 B each)              fixed 61 B + 18n
```

Decode is total: version ≠ 1 refuses "forward-only" (`AL:L207-212`), length must equal `61 + 18n`
exactly (`AL:L240-246`), every ref `len ≠ 0` and `start + len` non-overflowing (`AL:L251-256`).
Semantics of the fields (`AL:L156-171`): `home_vol` = the holder's HOME metadata volume ordinal "where
its ring and its bitmap pages live"; `control_ino` = "the holder's control ino (the shared index's
home, PR 7)" — written as `ROOT_INO` always (`AL:L3534`) and read by nothing (§9 Q4); `blocks` = the
bitmap geometry; `term` = "+1 per grant, never at a replay"; `bitmap` = the pages, volume-qualified.

**Port v4** (AGENTS: `extern struct`, explicit endianness, comptime size assertions; values LE, keys
BE for memcmp). The port's writer identity is the u64 `writer_id = xxh3(boot_id ‖ hostname ‖
mount_slot ‖ pid ‖ start_ticks)` (0002 row 185), not a u128 uuid:

```zig
pub const AllocLeaseRecordV4 = extern struct {   // tree 0 of volume 0, key "alloc_lease:" ‖ vol_tag BE
    version: u8 = 1,            // [0]
    flags: u8 = 0,              // [1]  must be 0 (a reserved byte decodes nonzero ⇒ Corrupt)
    home_vol: u16,              // [2]  LE — the holder's HOME meta volume ordinal
    holder_appender_id: u32,    // [4]  LE — the holder's appender id on home_vol
    node_token: u64,            // [8]  LE
    writer_id: u64,             // [16] LE — the incarnation id (0002 row 185)
    mount_slot: u32,            // [24] LE
    bitmap_ref_count: u16,      // [28] LE — n, refs follow the fixed part
    reserved: u16 = 0,          // [30] must be 0
    blocks: u64,                // [32] LE — bits the pages cover (= capacity_bytes / block_size)
    term: u64,                  // [40] LE — +1 per grant to a NEW holder, never at a replay
};                              // 48 B; comptime assert @sizeOf == 48
pub const BitmapRefV4 = extern struct { vol: u16, reserved: [6]u8 = @splat(0), start: u64, len: u64 }; // 24 B
```

`control_ino` is dropped (unused — §9 Q4). Total `48 + 24n`; decode = the reference's checks plus
flags/reserved == 0. The same key, the same inclusive range.

The two sibling families the lease's ordering law reads (`AL:L14-21`, `L68-71`, `L77-88`):

| record | key (BE) | value (reference, LE) | port v4 |
|---|---|---|---|
| `dead_member:{id}` | `"dead_member:" ‖ node_token u64 ‖ mount_slot u32` (24 B, `AL:L117-123`) | v1 `version ‖ epoch u64 ‖ ts_ms u64` (17 B), v2 `+ pr_key u64` (25 B), v3 `+ early u8` (26 B); decoded BY LENGTH, `early` byte must be 0/1 (`AL:L297-343`) | `extern struct { version u8 = 1, early u8, reserved [6]u8, epoch u64, ts_ms u64, pr_key u64 }` = 32 B, ONE length (§9 Q5) |
| `recovered:{id, vol}` | `"recovered:" ‖ node_token ‖ mount_slot ‖ vol u16` (24 B, `AL:L145-152`) | `version ‖ by_term u64 ‖ ts_ms u64` (17 B, `AL:L354-375`) | `extern struct { version u8 = 1, reserved [7]u8, by_term u64, ts_ms u64 }` = 24 B |

`early` (`AL:L287-294`): the recorder class — `true` for `squeezefs appender clear` and the same-node
takeover, recorders that can run INSIDE a surviving custody writer's `T_self`; the recovery then
quarantines fresh custody grants on the recovered slots' files (§6.6). `pr_key` (`AL:L283-286`) = the
dead member's NVMe registrant key the recovering manager preempts (0 = none).

### 1.2 Acquisition — first-come at term 1 (`AllocLeaseAcquire`)

Served by VOLUME 0's manager only (`manager_gate`, `AL:L1548`; `MGR:L242-252`). Screen FIRST
(`screen_alloc_lease_acquire`, `AL:L1462-1492`): `vol_tag` must be a data volume the arm registered
(`data_volume_blocks`, `AL:L1199-1207`); `0 < blocks ≤ known` ("a wire integer is never an allocation
authority — a smaller ask is a smaller bitmap over the same volume"); `home_vol < set width`. A failed
screen is `Rejected` (`STATUS_REJECTED`, `manager_verb_rejected`), nothing written (`AL:L1549-1552`).
Then the ladder (`AL:L1553-1623`):

| prior record | answer |
|---|---|
| none | write `{holder = caller, term = 1, bitmap = []}`; `AllocLeaseGranted{term 1, already false, predecessor []}` |
| holder == caller | `already = true` WITH the record's pages + blocks (`manager_verb_replays`) — "crash row 8: a successor that died before its copy retries and still learns what to copy" (`AL:L1530-1533`, `L1556-1563`) |
| holder ≠ caller, no `dead_member:` | `Busy` ("a live holder's lease moves only by its release; allocate through block grants from the holder"), `manager_verb_refusals` (`AL:L1566-1577`) |
| dead, no `recovered:{holder, holder.home_vol}` | `LeaseDeferred` — the typed retryable class; "re-granted only after the home manager's ring replay applied the bitmap deltas" (`AL:L1579-1593`) |
| dead AND recovered | write `{holder = caller, term = prior.term + 1, blocks = prior.blocks, bitmap = prior.bitmap}` — the successor's record NAMES THE PREDECESSOR'S PAGES until it publishes its copy (`AL:L1594-1623`, `L1536-1538`) |

Every write is one control entry `Try`-admitted (`write_control_entry(…, EntryAdmission::Try)`,
`AL:L1609-1615`). **There is no renewal of the allocation lease**: liveness is the death ledger
("a dead holder succeeded only after `dead_member:` AND `recovered:`"), the holder's own
`AllocLeaseAcquire` answering `already` is the re-hold's probe (§1.6), and the S6 membership beat
keeps the HOLDER alive, not the lease. The term word moves only on succession.

### 1.3 `AllocLeaseBitmap` and `AllocLeaseRelease`

`AllocLeaseBitmap{vol_tag, identity, term, bitmap}` (`MGR:L253-260`; `AL:L1632-1676`): no record ⇒
`Busy`; `holder ≠ identity ∨ term ≠ rec.term` ⇒ `Busy`; the refs are screened by `screen_bitmap_refs`
(`AL:L2598-2656`) — every ref names a volume the set has, lies inside that volume's heap, starts a
whole number of nodes from the heap start, is a whole number of nodes long, non-empty, no overflow;
`Σ len ≥ region_len(blocks)` and `≤ ceil(region / node) × node` — else `Rejected`, nothing persisted;
`rec.bitmap == bitmap` ⇒ `already` (replay); else the record is rewritten with the new refs. The
barrier is volume 0's lane's (`DSM:L534`).

`AllocLeaseRelease{vol_tag, identity, term}` (`AL:L1680-1705`): no record, or another holder/term ⇒
`already = true` (idempotent, counted a replay); the holder at `term` ⇒ `Record::delete`. The clean
leave's path; `manager_gate(true)` admits it during the manager's own leave.

### 1.4 Succession, in order (the §5.5.1 crash table, `DSM:L538-549`)

1. The home shard evicts → `RecordDeath{member, epoch, pr_key}` → volume 0's manager writes
   `dead_member:` with PARKING admission ("never a `Try` refusal that loses the death"; a write that
   still fails parks in RAM for the ledger poll's retry, `defer_death_record`, `dead_member_write_
   deferrals` must-stay-0 — `AL:L1796-1806`, `L1010-1015`, `L1035-1044`, `L1943-1969`). The served
   verb screens the member against the manager's own live list ("a member this manager LISTS as live
   is never declared dead by a peer's word", `MGR:L1207-1225`).
2. **The record's arm on every allocation HOLDER in the process** (`AL:L1809-1847`): the dead writer's
   open grants are REVOKED (`ledger.revoke_dead(writer)`, `BG:L329-336` — the ranges leave the table,
   the BITS STAY SET) and every block of them is QUARANTINED on the volume's allocator under a freshly
   minted dead epoch (`declare_dead_epoch` + `quarantine_offsets`, `DC:L347-354`, `L1426-1435`);
   `dead_members_quarantined` counts per (death × holding with grants). A holder with no registered
   allocator leaves the bits SET and logs ("the census releases them as a leak").
3. Each volume manager recovers the dead member's region (§5.9, PR 10's driver) and — as one arm of
   that replay — applies the kind-4 `vol_tag`-prefixed deltas of the dead holder's TERM to its pages
   (`replay_data_alloc_deltas`, §2.6), THEN writes `recovered:{id, vol}` (`manager_record_recovered`,
   `AL:L1710-1733`, PARKING admission).
4. A successor's `AllocLeaseAcquire` passes (§1.2 row 5), reads the predecessor's pages through the
   volume-qualified refs (`read_alloc_bitmap_image`, `AL:L2169-2202` — refs screened before anything
   proportional is allocated), COPIES them into extents of its OWN grant on ITS home volume
   (`hold_alloc_lease(…, Some(image))` → `hold_bitmap`, §2.2) and publishes the new refs
   (`AllocLeaseBitmap`). The dead region stays `Recovered` (not `Free`) until `alloc_lease:` no longer
   names its extents (`AL:L34-35`).
5. Retirement (`retire_death_record`, `AL:L1860-1890`): deletes `dead_member:` AND every `recovered:
   {member, v}` — "a LATER death of the same identity is gated on ITS recovery, never the previous
   one's". Triggered by the member's own rejoin (`retire_own_death_records`, `AL:L1897-1909`: "a record
   left standing would have the next ledger poll recover a LIVE region") or the poll's sweep
   (`sweep_retirable_death_records`, `AL:L1916-1941`): age ≥ `DEATH_RECORD_RETIRE_TTLS (2) × T_owner`
   (`AL:L1053-1070`: one TTL for every manager's poll to act, one for the successor's grace window),
   no region pending, and no `alloc_lease:` names the member as holder.

The in-process same-node takeover (`acquire_and_hold`, `AL:L3523-3647`): on `Busy`, iff the D0 flock
this open holds proves the recorded holder dead (page 0 at open was owned by this node AND named that
mount slot, `AL:L3591-3597`) the arm itself records the death (`record_death` = the EARLY class,
`AL:L1750-1759`), replays the window onto the predecessor's pages, writes `recovered:`, and retries
(≤ 4 attempts, `AL:L3532`). A same-node holder whose page is any other is a JOINED appender — never
taken over. A foreign dead holder's `LeaseDeferred` is returned loud ("the arm does not wait for a
foreign recovery", `AL:L3632-3639`).

### 1.5 The holder's duties (what `AllocHolding` owns, `AL:L452-488`)

| duty | mechanism | §
|---|---|---|
| bitmap authority | `DataAllocBitmap` + the ONE ordering lock `mutations` + the single drainer `drain` | 2.4 |
| grants | `BlockGrantLedger` carve / return / revoke / adopt; `holder_block_grant`, `holder_return_blocks` | 3 |
| terminal frees shipped to it | the `FreeExecutor` (`execute_shipped_frees`) runs the whole ladder with the holder's accounting authority | 4.1 |
| `finish_free` clears | `note_finish_free` → bit CLEAR + CLEAR delta queued + the journaler kicked | 2.4 |
| page writes | `write_data_alloc_pages(ckpt_seq)` at the checkpoint, BEFORE barrier #1, for holdings homed on THIS backend only | 2.5 |
| the shared index home | `index_home_volume_for(vol_tag) = holding.home_vol` — the index is tree 0 of the holder's HOME volume, served by THAT volume's manager | 5.1 |
| dead writers | revoke + quarantine under a dead epoch at the death record | 3.6 |
| deferred-leak convergence | `defer_leaks` / `adopt_declared` / `converge_deferred` | 3.7 |
| `df` | population − own unconsumed window | 4.5 |

The holder venue (`HolderVenue`, `AL:L2741-2793`): a wire writer's data plane dials the holder's
LISTENER, kept in force and RE-RESOLVED off durable state after a transport failure ("a manager
failover moves the holder's listener … every post-failover block ask failed at the dead address and
every terminal free shipped nowhere", `AL:L2731-2740`); the grant sink and the free target follow it
together (`wire_block_grant_sink`, `AL:L2839-2944`; `refresh_free_target`, `AL:L2818-2829`;
`joined_holder_venue_moves` ≥ 1 across a failover).

### 1.6 The arm (mount path) — seed, direction-judged verdict, drain the list, install the window

`arm_symmetric_allocation` (`AL:L2973-3338`), per data volume this mount writes:

1. `blocks = capacity / chunk` registered; the allocator registered as the quarantine sink
   (`AL:L2991-3001`).
2. The derived truth `derived_allocation_snapshot()` (1 bit per block below the cursor; set = not on
   the local free list — `BA:L956-981`) and the durable truth `durable` (the mount's own by-block scan
   handed over ONCE as a `BlockBits`, `note_mount_seed`/`take_mount_seed`, `AL:L1146-1163`, else one
   `block_ref_scan` over every volume straight into bit words — "never a `BTreeSet`/`Vec` of the
   population (8 GiB at a full PiB)", `AL:L3024-3047`).
3. **Un-seeded refusal**: `derived.highest == 0 ∧ !durable.is_empty()` refuses the mount ("every live
   block would be re-granted", `AL:L3049-3059`).
4. `seed = derived ∪ durable`; `acquire_and_hold` (§1.4): term 1 ⇒ `hold_alloc_lease_seeded` (pages
   written SET for every seed block, barriered, THEN published — `AL:L2036-2057`); `already` with
   empty refs ⇒ seeded pages at the SAME term (our own prior incarnation died between acquire and
   publish); `already` with refs ⇒ `rehold_alloc_lease` (§2.6); a successor ⇒ copy.
5. **The second snapshot, judged by DIRECTION** (`AL:L3075-3209`): the cursor moved, or a free-listed
   block MINTED (off the list with a refcount, or without one while no trim window / in-flight owner
   explains it) ⇒ refuse "ALLOCATED during the arm"; a seed block the allocator now free-lists is a
   FREE the arm ABSORBS (`note_finish_free` idempotent; `data_alloc_bitmap_arm_absorbed_frees`); the
   LOSS direction — a block the allocator holds LIVE (off the list, not in flight, not graced, not
   quarantined) that reads CLEAR in the recovered bitmap — refuses loud with `note_drift`
   (`data_alloc_bitmap_drift`) and names the fsck remedy. A candidate must PERSIST across two reads
   ≥ 5 ms apart, at most three re-reads (`AL:L3150-3177`: the reclaim lanes keep a few blocks inside
   `publish_free_list`'s clear→insert gap at any instant).
6. The LEAK half at the (re-)hold: a SET bit nothing references and no open grant names is deferred
   (§3.7) and the verdict run at once (`AL:L3236-3307`).
7. "The bitmap IS the free list from here": `drain_free_list_into_grants()` empties the local list (its
   blocks read CLEAR in the bitmap and return only through a carve), the grant arm is installed with
   the in-process holder as its sink, and the list is drained once more for a free that landed between
   the two (`AL:L3309-3319`; `BA:L987-996`).

A JOINED appender (`arm_joined_allocation`, `AL:L3424-3473`) holds no lease, seeds nothing, walks no
census: the wire grant sink + the wire free target are installed per data volume, both following the
holder venue, and the member's renewal declares its windows (`install_window_decl_source`, §3.7).

## 2. The data allocation bitmap

### 2.1 Geometry

One bit per block per data volume: `bitmap_bytes_for(cap, block) = ceil((cap/block)/8)` — 32 KiB per
TiB at 4 MiB (`DAB:L115-124`; `DSM:L533`; tie test `the_data_bitmap_is_thirty_two_kib_per_tib`,
`tests/sym_block_grant_tests.rs:3101`). `pages_for(blocks) = ceil(blocks / 32,512)`,
`region_len = pages × 2 × 4096` (A/B) (`DAB:L106-113`).

### 2.2 The `KVDA` page — byte-exact

Reference (`DAB:L47-62`, `L78-100`, `L159-227`), 4 KiB, LE:

```text
[0..4)    magic u32 = 0x4B56_4441 ("KVDA"; distinct from the meta bitmap's "KVAB" so a misplaced page never verifies)
[4..8)    page_index u32          (misdirected-write guard)
[8..16)   generation u64          (newest-valid-wins selector)
[16..24)  vol_tag u64             (a page of another volume's bitmap never reads as ours)
[24..32)  xxh3_64 u64             (over the image with this field zeroed: header-then-payload)
[32..4096) bits, LSB-first: bit b of byte i = block page_index·32,512 + i·8 + b
```

Slot A of page `i` at `base + i·8 KiB`, slot B at `+4 KiB` (`DAB:L60-62`). Decode (`DAB:L188-227`):
length → magic → page_index → vol_tag → checksum; any failure = "this slot holds no valid page", never
loud. Encode zero-pads the bits (`DAB:L159-181`).

**Port v4**: the same 32-B header and payload, mirroring `P/meta/alloc_ext.zig` exactly (24-B KVAB
header `magic ‖ page_index ‖ generation ‖ xxh3` at `L25-37`, `encodeBitmapPage`/`decodeBitmapPage` at
`L172-200`) plus the `vol_tag` word before the checksum; `page_magic = readInt(u32, "KVDA", .little)`
(ASCII in disk order, 0002 row 38); `page_data_len = 4064`, `page_bits = 32,512`, `words_per_page =
508` (4064 / 8 — page boundaries fall on word boundaries exactly as KVAB's 509). `checksum.stamp(out,
24)` / `checksum.verify`. Decode errors `{Truncated, BadMagic, WrongIndex, WrongVolume,
ChecksumMismatch}`; at LOAD every one means "slot absent".

### 2.3 Placement — extents of the holder's grant on its HOME volume

`hold_bitmap` (`AL:L2062-2127`): `extents_needed = max(1, ceil(region_len / node_size))` claimed from
the home volume's heap allocator (`claim_internal` — appender 0 claims directly, its alloc deltas
journaled in ring 0 like every claim of its own, `AL:L2013-2016`); the image is written across the
claimed extents in claim order (`write_at_batch`), THEN the claims' alloc records are journaled as one
control entry, a failed write or journal RELEASES every claim (`release_unpublished`); the published
refs are the claims coalesced into runs `(heap.start + run.start × node, run.len × node)`. "The same
device as its ring, so 'a page write covers the deltas ≤ its tail' is the meta-bitmap law verbatim on
one device, and the holder is already a WERO registrant there" (`DSM:L533`; `DAB:L16-19`).

Port: the port's extent grants arrive at rung 6 with the appender model; the pages are claimed from
the holder's extent GRANT (not the manager's bitmap directly) — the extent-grant pool the charter
names ("the grant is a POOL"). The region image for a successor's copy is `region_image(generation)`:
every page in slot A at one generation, slot B zeroed (`DAB:L793-810`).

### 2.4 The deltas — kind, key, value, the ONE ordering lock

Reference: kind 4 (`TREE_ALLOC_RESERVED`) with a 16-B key `vol_tag u64 BE ‖ block_idx u64 BE`, told
apart from the heap's 8-B extent keys BY LENGTH (`is_data_alloc_delta_key`, `DAB:L91-98`, `L242-247`);
value `tag u8 ‖ term u64 BE` = 9 B, tags `0x11` SET / `0x12` CLEAR (`DAB:L95-100`, `L290-321`); the
TERM is the holder's: "a recovery folds the deltas of exactly the term it recovers, never another
holder's stale residue" (`DAB:L25-28`). One record per BIT; a drain writes ≤ `DELTAS_PER_ENTRY = 1024`
records per control entry (`AL:L1349`, `L2208-2243`).

**Port v4** — decision 0002 row 44 is already landed: kind 5 `key.TREE_DATA_ALLOC_RESERVED`
(`P/meta/key.zig:L39-42`; `P/meta/alloc_ext.zig:L41-45`), never a key-length arm under kind 4. The
delta is a RUN (§9 Q6 — one record per carve instead of 64):

```zig
// kind 5, key = vol_tag u64 BE ‖ start_block u64 BE (16 B)
pub const DataAllocDeltaV4 = extern struct {
    tag: u8,                 // 0x11 = SET, 0x12 = CLEAR
    reserved: [3]u8 = @splat(0),
    len: u32,                // blocks in the run, ≥ 1 (a carve is contiguous by construction, §3.2)
    term: u64,               // LE — the holder term that journaled it
};                           // 16 B; comptime assert
```

Replay expands a run into `len` per-block `(seq, delta)` pairs before the fold (§2.6), so the per-key
LWW law is unchanged. `CLEAR`s from `finish_free` are `len = 1`.

**The ONE ordering lock** (`AL:L440-451`, `L553-593`): every bit mutation — a carve's SET, a return's
CLEAR, `finish_free`'s CLEAR — moves the bit AND pushes its delta onto `mutations:
Mutex<VecDeque<QueuedDelta>>` in ONE critical section, so queue order IS RAM order; one drainer at a
time (`drain: SqzMutex`) journals head-first in chunks (`take_queued` / a failed entry `requeue_front`
— "nothing behind it was journaled meanwhile, since the drainer holds `drain`", `AL:L729-744`,
`L2204-2243`), so record-seq order is queue order and "the replay's per-key LWW fold by seq reads the
RAM order — a CLEAR is never journaled after a later re-grant's SET" (`DAB:L31-36`). A carve DRAINS the
queue itself and answers only once its SET (and everything before it) is durable (`holder_block_grant`,
`AL:L2253-2307`); a `finish_free` CLEAR is queued and the single-flight journaler is kicked
(`kick_journaler`, `AL:L746-812`: a CAS latch, a detached task that drains, releases the latch and
re-checks the queue; `JournalReserveExhausted` leaves the deltas queued — `data_alloc_bitmap_deltas_
deferred` — "the page write covers the bits, the next kick or the next carve journals them").

Port: the mutation lock is the holder's ONE `sync.Mutex` over a preallocated ring of run deltas (sized
at the hold: `2 × (grant floor × writers) + the reclaim batch` entries, never grown under the lock —
0002 row 32); the drainer is a fiber on the home volume's journal lane (`submitStructural` is the
wrong class — these are USER-class control entries, "a verb storm never eats the checkpoint reserve",
charter §5.5), parked on the admission `Notify` the Checkpointer owns (0002 row 27).

### 2.5 Pages before barrier #1 (the checkpoint step)

`write_data_alloc_pages(ckpt_seq)` (`AL:L2332-2364`) runs in the holder's checkpoint cycle "BEFORE
barrier #1 and the ledger record, beside the meta bitmap's own page write — the record's tail passes
every delta journaled below the cycle's head, so the pages must already carry them; a delta journaled
after the head is inside the window the tail keeps". For holdings HOMED ON THIS backend only ("another
volume's checkpoint touches neither the pages nor the dirty bits", pin `a_second_volumes_checkpoint_
never_writes_the_holders_pages_onto_its_own_device`). The queue is NOT drained in the cycle ("no
USER-class admission ever decides the cycle's outcome": a queued SET only ever costs a leak, a queued
CLEAR reads a genuinely free block); a non-empty queue kicks the journaler OFF the cycle. Once per
cycle `population_drift_check()` compares the maintained counter against the popcount (a disagreement
= `data_alloc_population_drift` tripwire + re-sync, `DAB:L599-619`; `population_scans` pins that the
allocation funnel performs none, `DAB:L969-976`).

`take_dirty_plan(generation)` (`DAB:L721-758`): every dirty page's image at `gen = max(page.generation,
generation − 1) + 1` into the ALTERNATE slot (`SLOT_NONE → A`, else `1 − cur`), the state advanced
BEFORE the write (a failed write `restore_dirty`s the pages, `DAB:L760-790`). Port: the same plan feeds
the Checkpointer's existing `PageWriteSet` executor (`P/meta/alloc_ext.zig:L5-7` — "the io task executes
the `PageWriteSet` this module plans"); the data pages join the meta bitmap's set of that cycle.

### 2.6 Mount / recovery load — newest valid, the replay, the kept window

`from_region_image` (`DAB:L448-497`): per page the newest VALID slot wins; both slots ALL-ZERO = fresh
(clear); **a page with NO valid slot and a non-zero byte in either is REFUSED** ("bogus refs, a torn
pair, an unreadable predecessor region must never read as an all-free bitmap — this bitmap has no
second witness until fsck's census runs"). A short image zero-extends.

`replay(records, term)` (`DAB:L819-858`): over the ring's window — only `TREE_ALLOC_RESERVED` records
with a 16-B key, of THIS `vol_tag` and THIS `term` — per-key LWW by record seq FIRST (a SET superseded by
a later CLEAR folds to the clear), then the survivors applied through `set`/`clear`; returns bits
changed. Pinned by `deltas_fold_lww_and_the_run_carve_skips_set_bits` (`DAB:L1059-1078`: another term's
delta never folds in).

`replay_data_alloc_deltas(bitmap, term)` (`AL:L2375-2406`) = the fixed ring scanned from the ledger tail
PLUS **the kept window** (`REPLAYED_DELTAS`, `DAB:L896-963`): "the mount's own bring-up checkpoint
advances the ledger tail past the window BEFORE any allocation lease is re-held on it, so a replay that
read the ring alone would find nothing — while the pages on the device still show a journaled grant as
FREE (§5.5.1's headline row)". The journal replay hands every data delta it meets to `note_replayed_
delta`, keyed `(vol_tag, term, home device)`; a newer term for the same `(vol_tag, home)` evicts the
older groups; the recovery arm takes its group; the leave drops the home's groups.

The re-hold (`rehold_alloc_lease`, `AL:L2135-2162`): refs screened, image read, bitmap loaded, the term's
deltas folded, changed pages written + `sync_device`, the holding registered on the SAME extents ("no
claim, no copy").

Port: the kept window is a field of the volume's recovery state (not a process static), consumed by the
re-hold or dropped at the leave — and the replay keeps run deltas as runs until the fold expands them.

### 2.7 The census oracle and the drift tripwire's LOSS direction

`DataAllocBitmap::drift(referenced, granted)` (`DAB:L865-882`): `LOSS = referenced ∧ clear` (the S1
class, report-only, `data_alloc_bitmap_drift` must-stay-0 — a future carve would overwrite a live
block); `LEAK = set ∧ ¬referenced ∧ ¬granted` (release). fsck C6 on a grant-armed allocator
(`FSCK:L5392-5450`): the bitmap against the census's TRACKED population (block indices), with the
holder's `open_ranges()` and the in-flight registry as the live-owner shield; a LOSS is a `C6Drift`
suspect ("the LOSS class"); leaks minus in-flight = `fsck_alloc_bitmap_leak_candidates`; a grant-armed
allocator whose holding this process lacks (a wire writer) DECLINES, `alloc_census_declined`
(`FSCK:L5443-5450`). C2's "referenced offset is not allocator-tracked" is exempted when the bitmap tracks
it (`alloc_bitmap_tracked_exempted`, `FSCK:L5372-5379`). C8 keeps the derived walk as the ORACLE for the
refs (charter §5.6; 0002 row 104); the design's oracle sum is `Σ slot trees ∪ shared index ∪ open
grants` (`DSM:L537`).

Port: `DataPlane` already exports the arm the oracle needs (`derivedAllocationSnapshot`,
`PBA:L1326-1336`); the rung-8 fsck binds `drift` over the holder's bitmap and the block-refs census.

## 3. Ranged block grants

### 3.1 `G` — the derivation and the floor's WHY

`G = clamp(2 × ewma_blocks/s × T_renewal, 64, cap / (2 × writers))` (`DSM:L518`; `BG:L13-20`;
`block_grant_derived(ewma_milli_blocks_per_s, t_renewal_ms, capacity_blocks, writers)`,
`BG:L174-187`): `want = ewma_mblk × 2 × t_ms / 1,000,000`; `cap_term = max(floor, cap / (2 ×
max(writers, 1)))`; `clamp(want, floor, cap_term)`. Terms:
- `T_renewal` = the membership beat (`renewal_beat_ms`, `MB:L3873-3890`): the member session's
  `renew_interval`, else the owner's clocks, else the derivation — `renew_interval = max(1 ms,
  min(T_self / 3, 10 s))` (`LCC:L62-64`; `MB:L889-894`; the shipped beat `CLIENT_HEARTBEAT_INTERVAL_SECS
  = 10`, `T_owner = CLIENT_STALE_TTL_SECS = 45` s, `fuse_client.rs:L191-195`). "The beat SIZES the grant
  — a WARMED grant covers two beats by construction" (`DSM:L518`).
- `2 ×` = one EWMA window of under-estimate (`BG:L17`).
- **floor 64 = one write-pipeline BDP window of 4 MiB blocks** ("`write_pipeline_depth_target`'s shipped
  floor posture — 256 MiB in flight is what a single streaming writer keeps between two beats, so a
  writer never asks the holder more than once per window at the norm", `BG:L47-51`; `BLOCK_GRANT_FLOOR =
  64`). At the floor a writer at 8 k blocks/s empties it in ≈ 8 ms against a 10 s beat — "so the ramp
  needs the ask until the EWMA warms" (`DSM:L518`, an ASSUMED shape).
- cap = half the volume spread over the writers ("half stays for reuse", `BG:L19-20`); a cap below the
  floor is the floor ("a tiny volume grants the floor and lets the carve truncate at what is free",
  `BG:L168-170`). `writers` = the holder's ledger writers + 1 (`AL:L2273`).
Tie test `the_derivation_clamps_between_the_floor_and_half_the_volume_per_writer` (`BG:L634-648`: 100
blocks/s × 10 s × 2 = 2,000; 1,024/(2×4) = 128; a 100-block volume → 64).

**As built the EWMA term is NEVER fed** (`want == 0` ⇒ `block_grant_derived(0, beat, blocks, writers)`,
`AL:L2274-2280`; the wire `want` is only CAPPED by `block_grant_derived(u64::MAX/4, 1, …)`,
`AL:L2282-2287`; the writer's top-up asks `(sink)(0, held)`, `BA:L829`): every grant is the floor. §9 Q7
names the fix (the WRITER measures and names `want`).

### 3.2 Carve = the lowest clear run at/above the frontier; bits SET at the carve

`BlockGrantLedger::carve(bitmap, writer, want, floor, held_unconsumed)` (`BG:L236-265`): idempotency
first — `held_unconsumed ≥ want ∧ writer has open grants` ⇒ `Already(its grants)` ("a replayed frame
carves nothing", KD-SYM-7); else `bitmap.first_clear_run(floor, want)` (`DAB:L647-671`: skip all-ones
words, the run truncated at `want` or the first set bit) **falling back to `first_clear_run(0, want)`**
("a hole below the floor is genuinely free, since every granted bit is SET", `BG:L226-229`); `len`
capped at `u32::MAX`; `bitmap.set_run(start, len)` UNDER the ledger lock, BEFORE any reply; the grant
pushed under the writer's name; `Full` when no clear bit remains. `floor = ledger.grant_frontier()` =
the highest granted block + 1 over every open grant — "the zombie floor a successor carves above"
(`BG:L356-363`; `AL:L2289`). The HOLDING wraps the carve in its mutation lock and queues the SET deltas
in the same critical section (`AL:L556-565`), then drains before answering (`AL:L2290-2306`); a drain
failure gives the bits back (`return_blocks`) and refuses. Test seam `TEST_HOLDER_GRANT_EXHAUST_N`
(`AL:L2668`) answers the next N carves with the drain's admission class.

### 3.3 The writer's window

`GrantWindow` (`BG:L395-540`): a mutex-held sorted `Vec<BlockGrant{start u64, len u32}>` (`BG:L58-61`),
`install` refuses an equal or OVERLAPPING grant (idempotent), tracks `installed` and the top-up
`reference = max(len installed)`; `mint()` consumes the LOWEST grant's first block (a refused mint moves
nothing); `give_back(block)` puts a minted-never-published block back (merged onto an adjacent range,
`consumed` un-counted; `false` ⇔ already unconsumed — "the double-handout lineage", `BG:L454-495`);
`remaining`, `unconsumed`, `drain` (the leave); **`wants_topup() = remaining × 2 < reference ∨
reference == 0`** — the 50 % law (`BG:L513-519`); `note_exhausted`.

The allocator's arm (`BlockGrantArm`, `BA:L418-425`, `L744-760`): `next_fresh_block` mints ONLY from
the window ("the holder's bitmap already set the bits before the grant reached us, so a fresh mint here
is never a dense-cursor claim"; `highest.fetch_max(idx + 1)`; `kick_block_grant_topup()` after every
mint; an empty window ⇒ `note_exhausted` + `StorageFull` "a top-up from the allocation holder is owed",
`BA:L2099-2121`). **The top-up is a PULL**: `kick_block_grant_topup` (`BA:L915-944`) is single-flight
(`topup_inflight` CAS) and detached ("a burst of mints costs one wire round trip, and the exhausted
mint's inline ask stays the belt"); `block_grant_topup` (`BA:L820-845`) asks `(sink)(0, held =
remaining)` when `wants_topup`, installs every answered grant. `classify_topup_answer` (`BA:L880-913`):
`Unreachable` ⇒ `Retryable{HolderUnreachable}` (never a latched `StorageFull`: "a manager failover on a
joined writer with a drained window"); `Deferred(why)` ⇒ `Retryable{HolderDeferred}` (the holder's ring-0
user window full for the beat, its lease in motion — `GrantAnswer`, `BG:L551-577`); `Refused` ⇒ loud
`InvalidOperation`; `Full/Landed/NotAsked` leave the verdict to the window. A latched armed allocator
re-kicks the ask at most once per renewal beat off the write path (`fresh_supply_exhausted`,
`BA:L714-732`; `alloc_fresh_supply_reask_ms`).

`abandon_unpublished` on a NON-holder (`BA:L2948-2984`): a never-published mint goes back into the
window (`block_grant_window_recycles`), the refcount entry removed and the word retired; a poisoned era
keeps the counted abandon.

Port seam: `GrantSource{mint, giveBack, remaining, wantsTopup, noteExhausted}` (`PBA:L302-317`) is
already consulted at `nextFreshBlock` (`PBA:L760-771`), `abandonUnpublished` (`PBA:L1149-1163`),
`freeSupplyBlocks` (`PBA:L1282-1286`) and `freshSupplyExhausted` (`PBA:L1387-1391`); `virginBytes` reads
0 on an armed allocator (`PBA:L1271-1276`). Rung 6 binds `GrantWindow` + the sink behind it.

### 3.4 `BlockGrant` / `ReturnBlocks` — the verbs

`BlockGrant{vol_tag, writer: WireIdentity, want u32, held_unconsumed u64}` (`MGR:L223-234`) → served by
the volume's ALLOCATION HOLDER: `holder_block_grant` (`AL:L2253-2307`; `MGR:L1101-1129`): no holding ⇒
`Busy` "this mount does not hold the lease"; homed elsewhere ⇒ `Busy`; `want == 0` ⇒ the derivation;
carve → drain → `BlocksGranted{grants: [(start, len)], already}` or `BlocksFull`. A `JournalReserve
Exhausted` drain ⇒ the manager's typed `Deferred` + the checkpoint kicked with the park's mark
(`AL:L2699-2702`); the wire sink keeps its session on a `Deferred` (`AL:L2906-2917`), re-resolves the
venue and retries ONCE on a transport failure (`AL:L2918-2934`).

`ReturnBlocks{vol_tag, writer, start, len}` (`MGR:L235-241`) → `holder_return_blocks` (`AL:L2313-2330`):
`ledger.return_blocks` (`BG:L283-321`) finds the ONE covering grant of that writer, splits it around the
range, CLEARS the bits UNDER the ledger lock ("a racing carve of the same block cannot interleave
between the table's change and the bitmap's"), `None` for a range the ledger does not grant that writer
("a wire integer is never an allocation authority"); then the CLEAR deltas drained in order →
`BlocksReturned{cleared}`.

Both are idempotent against durable/RAM state, both screened; verb codes `0x80..=0x8F` (`MGR:L218-222`:
"the codes are the DOCUMENTED identities every same-commit peer agrees on; the enum order is
append-only").

### 3.5 Liveness = the death ledger, never a TTL

"A grant's liveness is the writer's `dead_member` status, never a TTL: the carriage refresh tops the
grant up, it does not expire it, so a parked member's (§5.5.3) already-granted blocks stay its own until
the death ledger says otherwise" (`DSM:L518`; `BG:L20-22`). Pins: `a_parked_members_grants_stay_valid_
and_its_token_service_continues` (`tests/sym_block_grant_tests.rs:2425`).

### 3.6 A dead writer: revocation + quarantine under a dead epoch

At the death record (§1.4 step 2): `revoke_dead` returns the writer's grants (bits SET), `declare_dead_
epoch` mints the cohort id (`DC:L342-354`: a monotone sequence, "never reused"), `quarantine_offsets`
admits every block of every revoked grant (`DC:L1426-1435`; `BA:L1134-1155`: admission takes a free-
listed block OUT of the free list and marks its publish owed, "a quarantined offset must be unreachable
from every allocation path, not merely filtered by one of them"). Release needs a `DrainProof` (§6.3):
`release_quarantine(epoch)` (`BA:L1173-1198`) publishes the owed frees THROUGH the grace ring ("the drain
proof clears the CUSTODY gate; the reader coherence gate is separate and still applies") — and on the
holder that publish is `publish_free_list` ⇒ the bit CLEARS with its delta (§4.3). "A quarantine never
force-drains … ENOSPC is a bounded availability cost; the drain proof is the recovery act"
(`DC:L73-82`). Pin `a_death_record_revokes_the_dead_writers_grants_into_the_quarantine` (`tests/sym_
block_grant_tests.rs:743`).

### 3.7 Every live writer declares its windows; the successor adopts or releases

`RenewFrame.block_grant_windows: Vec<WindowDecl{vol_tag, ranges: Vec<BlockGrant>}>` (`MW:L231`,
`L733`; `MB:L1073-1091` the installed source; `BG:L63-75`): every beat the member declares, per data
volume, its unconsumed grant ranges PLUS its in-flight (minted, unpublished) offsets as block indices,
coalesced ascending (`held_block_ranges`, `AL:L3502-3520`: "a write between its mint and its layout
commit holds a block that is neither in the window nor referenced, and a failover inside that window
must not read it as nobody's").

The holder (`note_peer_windows`, `AL:L884-921`): every declaration for a volume it holds is SCREENED
against the volume's DURABLE block count BEFORE any walk (`screen_window_decl`, `BG:L122-148`: every
range non-empty, inside `[0, blocks)` judged without wrap, ascending, disjoint); one bad range refuses
the whole word (`data_alloc_bitmap_decls_rejected` must-stay-0, the writer not counted as declared);
then `adopt_declared` (`AL:L634-672`): the PENDING ∩ range blocks whose bits are SET are ADOPTED into the
ledger under the writer's name as runs (its death revokes them, its leave returns them; "a wire word sets
no bit"), the writer joins the declared set whatever it declared.

**The deferred leak release** (`DeferredLeaks`, `AL:L490-516`; `converge_deferred`, `AL:L682-721`): a SET
bit nothing references and no grant names is either a dead incarnation's remainder or a LIVE writer's
window the predecessor's RAM ledger granted and died with — indistinguishable from the bitmap alone.
Round 1 deferred while any peer page was `Live` and "the set grew ≈ G/2 blocks per writer per failover
for ever (53 → 340 of 4,096 blocks per volume across two failovers)"; clearing them re-carved blocks a
joiner still minted from ("200 mints → 144 distinct: a DOUBLE ALLOCATION", `AL:L3249-3256`). Now: every
candidate enters `pending`; once every `Live` peer page's writer has declared — or its page is no longer
`Live` (dead: recovered by the ledger poll) — what is still pending is released through `note_finish_
free` (bit CLEAR + delta, in order), `data_alloc_bitmap_leaks_released`; bounded by "the renewal beat +
the lease TTL + one poll"; closure `deferred ≡ released + adopted + pending`. Driven at the ledger poll
(`converge_deferred_leaks`, `AL:L929-944`; `live_peer_writers` off the appender directory,
`AL:L3348-3377`).

Port: the declaration rides the port's membership renewal frame (rung 6's S6 plane); the holder's
`DeferredLeaks` is a bitset over the volume (never a `BTreeSet<u64>` + `BTreeSet<String>`, §9 Q1).

## 4. Frees

### 4.1 A non-holder's terminal free SHIPS to the holder

`BackendRouter::free_block_verdict` (`RT:L5208-5221`): `frees_ship_to_holder(key) ∧ ¬authority_
accounting_scope_active()` ⇒ `ship_displaced_frees` and `Ok(false)` ("the pre-branch shape was a silent
local leak: `Ok(())` with nothing freed anywhere, forever"). The split (`SF:L6-16`): the free's DURABLE
effect (the `TREE_BLOCK_REFS` delete) already rode the displacing layout publish, whole-tx; what ships
is the ACCOUNTING ladder whose durable home is the holder's ledger.

The verb: `PublishCall::FreeBlocks{vol_tag, blocks: Vec<u64> (dense indices), lease_epoch,
request_id}` (`PUB:L463-474`) — RETRIED, "safe here alone": the holder's dedup window keys `(lease_
epoch, request_id)`, and a retry NEVER re-keys under a fresh epoch ("that is what closes the freed-then-
reallocated ABA window"). Reply `FreeVerdicts(Vec<FreeVerdict{Freed, NonTerminal, Refused}>)`
(`PUB:L553-560`, `L798`).

The shipper (`ship_displaced_frees`, `SF:L249-448`): groups keys by `(allocator, vol_tag)`; refuses a
key naming a dead incarnation (`block_key_incarnation_ok`, "§6.2 item 6"); no free target ⇒ `free_ship_
failures` + ERROR, nothing moved ("leak-safe: the offsets are durably unreferenced and the holder's next
derivation returns them"); dials the target (`holder_client_following`); `FREE_SHIP_ATTEMPTS = 3`
(`SF:L77-82`); a transport failure re-resolves the holder venue ONCE and re-keys (fresh client, fresh
request id — "the old id never reached anyone"); a lease-epoch change mid-retry ABANDONS (leak-safe);
per verdict: `Freed` ⇒ purge read tiers + `retire_shipped_free_tracking` (entry gone, word retired);
`NonTerminal` ⇒ purge + `release_shipped_free_tracking` (this reference only, never the word);
`Refused` ⇒ touch NOTHING (`SF:L396-428`: "the verdict-blind retire moved a LIVE owner's incarnation
word whenever a `Refused` double-release entry rode a served batch").

The executor (`execute_shipped_frees`, `SF:L478-617`), under `with_authority_accounting` (a task-local
marker — "NEVER ambiently active", `SF:L31-66`), ONE ledger read per batch (`durable_block_refcounts`);
per block:

| RAM state at the holder | verdict | why |
|---|---|---|
| tracked, incarnation word UNSTABLE | `Refused`, `block_live_free_refusals` | claimed/mid-write by its current owner — a stale duplicate of a dead lifetime (finding 24) |
| tracked, `population ≥ refcount` | `Refused`, `block_live_free_refusals` | a legitimate shipped free follows its publish, so population runs BELOW refcount; equality names a freed-and-REALLOCATED offset (finding 23's SHIELD; the wire carries no incarnation witness) |
| tracked, `refcount n > 0` | the standard `free_block` ladder; `Freed` iff `n == 1` else `NonTerminal` | byte-identical to a local free |
| tracked, count 0 | `Refused`, `block_untracked_free_refusals` | the transient window of a racing terminal release |
| untracked, population > 0 | `NonTerminal` | the release already happened durably |
| untracked, population 0, already free / bit CLEAR in the holder's bitmap / graced / quarantined / in flight | `Refused` through the UNSEEDED ladder (`block_untracked_free_refusals`) | the double-release lineage; "a refusal storm of one class is a duplicate free ISSUER on the shipper" |
| untracked, population 0, none of those | `seed_shipped_free_reference` + the ladder ⇒ `Freed` | "deliberately NOT `recover_block`, whose gap-filling arm would declare a live peer's unpublished tail free" |

Measured (`ACC:L1036`): 7 joiners rewriting 7 GiB in place — 983 frees/s served at the holder;
`free_shipped_blocks 1,824 ≡ free_served_blocks 1,824 ≥ 1,792 displaced`; `free_ship_failures 0`; the
holder's bitmap `set_bits 1,570 / clear_bits 1,824`; `manager_load_pct 1`.

Also at the holder: the served-publish SCREEN (`router_released_block_probe`, `SF:L630-642`): a served
layout publish whose frame TAKES a block this mount released (free-listed, graced or quarantined) is
refused before anything is staged (`served_publish_free_block_refusals` must-stay-0).

Port seam: `DAR §11 item 7` — the terminal ladder's step 1 is a `?FreeTarget` resolved by `vol_tag`
(`reclaim.zig`'s terminal path); the three allocator seams `seedShippedFreeReference` /
`retireShippedFreeTracking` / `releaseShippedFreeTracking` exist with rung-4 bodies. The
`UntrackedFreeGate{adjudicate}` (`PBA:L359-362`, consulted at `beginFree` `PBA:L1075-1081`) is the
holder's LOCAL face of the same ladder — the untracked arm of `free_block_verdict` (`RT:L5304-5335`: "a
block a FORMER lessee minted from its grant window, in a slot released, handed over or recovered to this
mount since its mount-time census … every `rm` at the manager of a file a departed joiner wrote leaked
its blocks").

### 4.2 The free-grace ring — why, shape, records, when a free leaves grace

**Why** (`FG:L7-27`): the read path's serve proof — "bytes for key K serve for block b iff the fetch was
incarnation-valid and the current map still binds b → K" — is process-local; a reader whose cached map
still binds `b → K` serves another file's bytes after the writer freed and REISSUED the offset, silently
on a passthrough volume. The ring "refuses to reallocate an offset until every registered reader has
acknowledged passing that epoch, riding the existing heartbeat. A reader that fails to acknowledge is
fenced, not waited on" (`FG:L31-36`). It is NOT the S7 quarantine (`FG:L84-108`: admission = every
terminal free vs a death event; release = a monotone bound vs a drain proof; FIFO vs cohort set; "they
compose in a fixed order — custody proof first, reader coherence second").

**Shape** (`GraceRing`, `FG:L1288-1322`, `L1352-1364`): one per allocator; `Mutex<VecDeque<GraceEntry
{label, offset, size}>>` ("the harvest must PEEK the front … the label is read INSIDE the lock so the
deque is ordered by construction"), `len` published lock-free, `bytes`, `cap`, per-ring `deferrals /
releases / timeout_deferrals` ("the per-volume closure `deferrals ≡ releases + len`"). The LABEL is the
owner's monotonic ms (`label_now`, `FG:L563`) — "an owner-minted causal label: a reader only ever echoes
a label the owner HANDED it … causal, not temporal" (`FG:L46-53`); the published BOUND is the minimum
acknowledged label over live members (`BOUND`, `u64::MAX` = nobody can hold a stale binding,
`FG:L253-258`; `publish_bound`, `FG:L687`).

**Records**: `defer(offset, size)` (`FG:L1393-1418`) — unarmed ⇒ `false` (one relaxed load, the caller
publishes as always); armed ⇒ push `{label, offset, size}`, counters. `GRACE_ENTRY_BYTES = 24`
(`FG:L1052`).

**When a free leaves grace** (`harvest_with`, `FG:L1459-1613`): pop the front while `label ≤ bound`, up to
`max` (`HARVEST_BATCH = 64`, "matches the block-reclaim drain's batch quantum", `FG:L1062-1068`); the
popped offsets are OWED a free-list publish by the allocator (`publish_grace_release` →
`publish_free_list`). If the oldest label is above the bound: `over_cap` or the deadline (`tightened_
bound_ms(runway)`) expired ⇒ `force_progress(oldest, why)` — the laggard is FENCED (evicted through S6,
`free_grace_laggard_fences`), the bound recomputed, `forced` releases counted `free_grace_forced_
releases` and the ring's `timeout_deferrals`. Harvest runs at every terminal free and at the allocation
funnel head (`BA:L2690-2699`; `PBA:L785-791` already calls the seam's `harvest` there). `holds(offset)`
is a bounded scan (diagnostics, the debt drainer's skip, fsck).

**The bounds** (`FG:L1091-1149`): `ack_cycle = 3 × renew_interval + 3 × staleness_bound + skew_max +
D_purge` ("learn a label, carry the ack home, the owner's publish cadence; qualify, wait for that pass,
expire the daemon caches"); the routine fence bound = 2 × cycle ("one missed cycle tolerated, mirroring
S6's three attempts"); an explicit `SQUEEZEFS_FREE_GRACE_MAX_MS` below ONE cycle REFUSES ("never
clamped"); the pressure bound = one cycle. The valve (`FG:L110-157`): `runway_ms = min(cap − held, free
supply) × span / held` (`FG:L1175-1184`); rung (a) prods laggards to a shorter cadence (`ProdParams::
cadence_for = clamp((runway − reader_lag) / 3, floor, renew)`, `FG:L1277-1281`); rung (b) slides the
deadline linearly from the routine bound to the pressure bound (`effective_bound_ms_from`,
`FG:L1194-1206`); rung (c) forces WITH the eviction. Ring cap `max(budget / 1024 / 24, 131,072)`
(`FG:L1054-1080`: "the measured saturated ingest of 12.7 GB/s over one default acknowledgement cycle
(≈ 38 s) displaces ≈ 120 k blocks").

**Under GPFS-strict tokens the ring is the TIMEOUT path alone** (`DSM:L519`, `L524`; `FG:L3436-3522`): the
freeing publish RECALLED every reader's token before it committed, so `recall_gate_verdict()` answers
`Gated` ⇒ the offset publishes directly (`BA:L2676-2689`, `free_grace_recall_gated_frees`); a LIVE
member's recall unacked past the bound opens a window `now + T_owner` on the owner clock (`note_recall_
unacked_live`, `FG:L3529-3537`) during which frees ride the ring (`Deferred`, `free_grace_recall_
timeout_deferrals`). Order in `finish_free` (`BA:L2656-2705`): quarantine (defer the publish) → recall
gate → grace defer → publish.

Port seam: `GraceRing{defer_, harvest, holds, len}` (`PBA:L338-349`), consulted at `finishFree`
(`PBA:L1104-1107`), `tryAllocateBlock` (`PBA:L785-791`), `takeDebtBatch` (`PBA:L1199`),
`fsckReconcileAccounting` (`PBA:L1443`) and `freshSupplyVisible` (`PBA:L1372`); the wall-bounded park is
`enospc.zig` (`DAR §4.4`: slice `clamp(pressure_bound/16, 10, 250)` ms, wall `2 × fence bound` floor
1 s). Rung 6 binds the ring, the label clock (the port's S6 owner clock) and the recall gate (the port's
token plane).

### 4.3 Reclaim interplay — what changes under grants

The port's reclaim (charter §6.2; `P/data/reclaim.zig:L17-39`, `L207-251`): discard elision while
`debt ≤ virgin_bytes`, the manners law, park-don't-spill. Under grants:

1. **The holder's `publish_free_list` CLEARS THE BIT instead of inserting into a list** (`BA:L2711-2742`):
   `note_finish_free(vol_tag, idx)` — after quarantine and grace, "so a clear bit means 'reallocatable
   now'" — and on a grant-armed allocator RETURNS ("a freed block is reallocated only through a GRANT —
   whose carve SETS its bit again — never off a local list with its bit clear"); `false` = DOUBLE FREE
   (`block_double_frees`). In the port this is `FinishFreeSink.noteFinishFree` (`PBA:L294-298`,
   `L1116-1124`) — and because the port's free list IS a bitmap already (`PBA:L14-20`), the holder's
   bitmap and the local free bitmap are ONE structure: the sink's body is "journal the delta" and the
   bit the port clears at `PBA:L1126` is the holder's bit (§9 Q2).
2. **`virgin_bytes` reads 0 on an armed writer** (`PBA:L1271-1276`; `BA:L2847-2854` "the flat inputs — the
   virgin tail and the local free list — read 0 on an armed allocator at a full cursor"), so the elision
   law `elide while debt ≤ virgin_bytes` (`P/data/reclaim.zig:L207`) elides nothing on a non-holder and,
   on the holder, must read the HOLDER's virgin tail = `bitmap.blocks − max(highest set bit + 1)`
   (`highest_set`, `DAB:L632-642`). The grant supply term the valve reads is `window.remaining + (blocks
   − population)` on the holder (`grant_supply_blocks`, `BA:L2855-2861`: the flat inputs "the free-grace
   valve would read as a permanent trough and prod / tighten / fence readers for").
3. A non-holder's terminal free never reaches its own reclaimer: the discard/punch is the HOLDER's
   (`SF:L8-15` "whose device reclaimer is live only there"); the shipper only purges tiers and retires
   tracking. The manners law and the debt drainer therefore run on the holder with the HOLDER's
   foreground signal — the shipped frees' displacement feeds the holder's displacement seed.
4. The reclaim queue's entry carries `vol` (the port's `Q5`, `PBA:L448-450`); a holder may hold several
   data volumes' leases — the drain lanes group by device as before.
5. `release_quarantine`'s owed publishes route through the grace ring, then the sink (`BA:L1176-1186`).

### 4.4 Quarantine — the port's two-line gate becomes the dead-epoch cohort

Port today: `State.quarantined` + `quarantine_count` + the `finishFree` gate (`PBA:L208-209`, `L520-521`,
`L1100-1103`), no admission or release path. Rung 6 adds `quarantineBlock(idx, DeadEpoch) bool` (take
the bit out of the free list if free — `tryTakeBit` + `debitFree` — mark the owed publish, state
`quarantined`; a live owner's block is admitted too and only gates its future free, `BA:L1127-1131`),
`isQuarantined`, `releaseQuarantine(DeadEpoch) usize` (the cohort scan is a bitmap/state scan, never a
hash walk, Q3; owed publishes → grace → sink). The `DeadEpoch` tag lives in the cell's `stamp` word
while quarantined (the stamp is dead — the incarnation was retired — so the slot is free) or in a
side map sized at admission; the port picks the stamp word (24 B/block stays).

### 4.5 `df` = population minus own unconsumed window

`get_used_blocks` (`BA:L3028-3052`): on a grant-armed allocator whose lease THIS process holds, `used =
bitmap.population − block_grant_remaining` ("every referenced block plus every open grant, the other
writers' windows counted as RESERVED, minus this writer's own unconsumed window"; the single-writer
`highest − free_list` "read a used count that only ever GREW on every armed default mount — `df` never
recovering after deletes", `ACC:L4941`); a grant-armed NON-holder keeps the arithmetic "— its truth is
at the holder, carried by no word it reads yet" (§9 Q8). Pin `an_armed_holders_used_count_follows_its_
bitmap_not_the_single_writer_arithmetic` (RED at `4096 vs 3996`). Port: `usedBlocks` already reads
`HolderView{bitmap_blocks, population, window_remaining}` through `AllocationLease.holderView`
(`PBA:L319-333`, `L1296-1299`); `DataPlane.statfs` sums `reachableFreeBlocks` (`P/data/plane.zig:
L918-933`) — on a holder that must become `blocks − population + own window` (the clear population IS
the free supply) and on a joiner the holder's population carried on the grant reply (Q8).

## 5. The shared-block index and the clone protocol

### 5.1 The index — home, record, probes

Home: `index_home_volume_for(vol_tag)` = the volume the data volume's allocation holder is HOMED on
(`holding.home_vol`), else volume 0 (`SR:L98-109`); the records live in THAT volume's tree 0 under
`b"shared_ref:"` (11 B) ‖ the 28-B block-ref key `vol_tag ‖ block_idx ‖ owner_ino ‖ block_index` = 39 B,
value `version u8 = 1 ‖ reserved × 3` (`SR:L14-18`, `L111-168`); written by that volume's MANAGER
(`manager_gate`, `SR:L456`) through `write_control_entry` with a PARKING pre-admission taken BEFORE the
verb mutex ("a park under `manager_verbs` deadlocks with the checkpoint task's grant verbs; a `Try`
fails the clone on a busy ring 0", `SR:L457-466`, `L543-552`). Pin `the_shared_index_home_follows_the_
allocation_holder` (`tests/sym_block_grant_tests.rs:2043`). Note the imprecision the charter inherits:
"the index home (the allocation holder)" means the tree 0 of the holder's HOME volume, served by that
volume's manager — the holder itself is an ordinary mount (§9 Q9 gives the port's shape: kind 9
`TREE_SHARED_INDEX`, already reserved at `P/meta/key.zig:L50`, keyed by the 28-B ref key, in the home
volume's tree 0).

Probes (`SR:L297-347`): `block_ref_probe_flags(vol_tag, block_idx, slot)` = ONE range probe of the
referencing ino's slot tree (`forest.refs_probe`), `RefProbe{count, shared = OR of the records' SHARED
bits}`; both halves decoded for every record ("a malformed accounting record is loud corruption, never
a silently skipped — or silently counted — reference"); `block_ref_probes` counts it, and
`shared_index_population` / every read inside ONE `release_shared` decision count as one probe
(`SR:L556-578`, `L643-644`).

### 5.2 Pack scope `(writer, slot, data volume)` — why `refcount(block)` is one probe

`DSM:L491` (KD-SYM-8): "a pack block's tenants are inos of ONE slot, so all of the block's references
live in ONE slot tree and `refcount(block)` there is exact"; `pack_scope_key(meta_volume, slot) =
(volume << 32) | slot` (`RT:L1742-1747`: "the native forest slot is `0` on every meta volume, so the slot
alone would pool two volumes' natives into one pack block whose references live in two slot trees"); the
writer is implicit (one lessee per slot). "No seed walk — refcounts are probed on demand" (`DSM:L492`).
Port: `packScopeKey` + `PackKey{vol_tag, scope}` already (`P/data/pack.zig:L183-195`); the port's
`refcount(block)` probe over the kind-6 prefix exists (`P/data/block_refs.zig:L8-10`) and gains the
`shared` fold.

### 5.3 The protocol, end to end (`DSM:L498-512`; `clone_share_blocks`, `RT:L8039-8145`)

Actors: C the cloner (dest `G`), O the holder of source `F`'s slot, H the index home of `b`'s volume.

0. Which blocks go through it (`RT:L8019-8038`): only entries whose two references would live in two
   slot trees — the pair is cross-slot, or the source's reference ALREADY carries SHARED ("a clone of a
   clone: the population is cross-slot whatever this pair's slots are"); a same-slot clone of an unshared
   block runs no protocol, no mark, no index, no bit (pin `a_same_slot_clone_of_an_unshared_block_runs_
   no_protocol_and_a_shared_source_does`).
1. **`MarkShared{vol_tag, block_idx, owner_ino (local key form), block_index}`** at O (`MGR:L181-192`;
   `mark_block_refs_shared`, `SR:L404-444`): under F's 4a EXCLUSIVE guard ("the same lock its layout
   commits and its W1 predicate's caller hold"), ONE tx over every source reference — `BlockRefOp::taken_
   shared` re-Puts the record WITH the bit; `Already` stages nothing; `Gone` (no record) aborts the clone
   `ENOENT` ("the cloner never publishes a reference to a block that may be freed", `RT:L8104-8117`).
   The ack follows the durability lane. The SERVED mark additionally runs under the source file's block
   guard `BLOCK_FLUSH_LOCKS(global ino, block_index)` (`MGR:L964-986`: "so the durable bit and the RAM
   mark land either wholly BEFORE a patch's fenced mark load or wholly AFTER a patch's DMA") and sets the
   RAM mark on the serving mount (`routed.note_marked`). The cloner sets its own RAM marks after the ack
   (`RT:L8119-8125`).
2. **`ShareBlock{vol_tag, block_idx, refs: [(owner_ino, block_index)]}`** at H (`MGR:L193-200`): BOTH inos'
   entries (source and target, "so C16 can check both sides") as ONE control entry, present ones skipped
   (`share_block`, `SR:L452-485`); chunked at 512 per call (`RT:L8137-8142`). The served form is SCREENED
   (`share_block_screened`, `SR:L495-528`): each named reference must exist AND carry SHARED on its own
   volume (`routed.ref_flags`) — "MarkShared precedes ShareBlock" — else the whole frame is `Rejected`.
3. C publishes G's layout with its own references `taken_shared` (`P/data/block_refs.zig:L133-143`,
   `L240-247`: "a SHARED new binding always re-Puts with the bit"; a `takenShared` is never cancelled
   against a release) — only after step 2 returns (`RT:L8033-8038`).
4. **A terminal free of a SHARED block is never decided locally** (`free_block_verdict`, `RT:L5250-5302`):
   `allocator.is_shared(offset)` ⇒ the installed `shared_free_gate` asks H `ReleaseShared{vol_tag,
   block_idx, owner: Some((ino, block_index))}` (`MGR:L201-210`; `release_shared`, `SR:L631-698`): delete
   the releaser's ONE entry ("the legal same-`off` nested clone+clip class gives one ino two references
   to one block and the other must stand"), GC every entry whose ino no longer holds a reference (the
   routed `still_referenced` arm — "the cloner died after step 2" window; the wire arm takes no GC
   verdict, `MGR:L1051-1056`), decide: `NotShared` (the index never named it ⇒ the ordinary local verdict),
   `Held{remaining}` ("this owner's reference is released in RAM — never to 0 — and the block stays
   allocated", `release_shared_held`, `BA:L1964-1977`), `Freed` (the releaser runs the terminal free). The
   pre-admission is re-sized when a racing `ShareBlock` grew the population (`RELEASE_RESIZE_ATTEMPTS =
   8`, `SR:L84-89`). A home that cannot decide ⇒ the reference and mark are KEPT, `shared_release_
   failures` must-stay-0 ("a local verdict on a shared block could free a peer's data", `RT:L5261-5276`).

Crash windows (`DSM:L510-511`): C dies after 1 ⇒ a SHARED flag with no clone (harmless: O's free ships
to H, H frees) — pin `a_cloner_dying_after_mark_shared_leaves_a_harmless_flag_c16_reports`; C dies after
2 ⇒ an index entry with no ref ⇒ C16 + GC at the release — pin `a_cloner_dying_after_share_block_is_
reported_by_c16_and_gcd_at_the_release`; O dies mid-1 ⇒ C retries at O's successor (the durable
witness is the flag). The mount seeds its RAM marks from the index at the arm (pin `the_shared_marks_
are_seeded_from_the_index_at_the_arm`).

**A surviving sole owner stays SHARED — the owed un-share** (charter §5.5): `Held{remaining: 1}` leaves
the last owner's durable bit SET and its block on the CoW path for ever (`RT:L5278-5293`; `SR:L212-216`).
The un-share needs a share-in-flight witness because a cloner may be between step 1 and step 3 of a NEW
clone of that very owner while the home decides "one entry remains": clearing the bit then would let a
W1 patch land under the clone. The witness the port owes: a per-block share intent at the home,
registered BEFORE step 1 (`ShareIntent{b, owner}`, a control record; or the index entry for the TARGET
written first with a `pending` flag) and retired at step 3's publish ack, so the home's `Held{1}` verdict
can issue `UnshareBlock{b, owner}` to the owner's holder — served under the owner ino's 4a guard (the
guard `MarkShared` also takes) carrying the home's `share_seq` — only when `in_flight(b) == 0`; a
`MarkShared` that lands first bumps the seq and the stale clear is refused at the home's re-check.
Priced as W1's CoW fallback (the charter's ruling): a sole owner of a once-shared block pays the whole-
block rewrite per sub-block overwrite (the `≈ 2,500×` rand-4k amplification W1 removes, `RT:L1719-1723`)
until the un-share lands.

### 5.4 The model (`SRC`; `LOOM:L7339-7424`)

`shared_ref_core`: `mark` = a Release store of `SHARED` (`SRC:L71-73`); `is_shared` = an Acquire load
(`SRC:L83-85`); callers on the "sole owner" paths interpose `cross_word_fence` (SeqCst) between their
own store and this load. The composed model: patcher = retire → fence → `peek == 1 ∧ !is_shared`;
owner = release → fence → `is_shared` (local terminal free iff terminal ∧ unmarked, else SHIP); cloner =
`mark` → fence → `try_acquire` → fence → `snapshot`. Invariants (`LOOM:L7390-7423`): no store-buffering
outcome (patched ∧ pinned ∧ pre-patch snapshot); a patch never proceeds past a visible mark; a pin that
landed makes the owner's release nonterminal and a terminal release that read the mark SHIPS; count
conservation. Weakening (`LOOM:L7330-7337`): the PATCHER's fence to `Release` fails; the cloner's fences
fail only when BOTH are weakened — "at least one `SeqCst` fence between the pin and the snapshot".

Port: `block_alloc.beginPatchSoleOwner` = plane gate → SeqCst `retire` → SeqCst `peekFenced == 1 ∧
!isShared` (`PBA:L1031-1037`); `pinBlockValidated` = SeqCst `tryAcquire` → SeqCst `snapshotFenced`
(`PBA:L991-996`); `beginFree`'s SHARED gate first (`PBA:L1065-1074`). The port's `FLAG_SHARED` is a bit
of the cell's `flags` byte (`PBA:L213`, `L1041-1051`) — the acq_rel `fetchOr`/`fetchAnd` on a byte beside
the refcount word; the `incarnation_model.zig` store-buffer model gains the third word (the port's
model-check obligation for this rung).

### 5.5 C16 (`shared_index_drift`, `SR:L742-791`; `FSCK:L4530-4560`, `L6135-6194`)

Over one data volume: the SHARED-flagged references of every mounted meta volume (owners resolved to
the GLOBAL ino) against the home's index entries — `FlagWithoutEntry` (source OR target) and
`EntryWithoutFlag`; report-only, `fsck_shared_index_drift`; a finding is confirmed by a second call
(verify-before-report). The whole-volume probe `shared_flagged_refs` (`SR:L351-366`).

## 6. Custody words and the custody revoke pair

### 6.1 The custody epoch — ONE authorization point

`CustodyEpoch(u64) = term_base() | (custody_generation & GRANT_SEQ_MAX)` (`DC:L123-181`): the durable
writer term in the high bits ("the successor of a failed mount bumps `WriterClaim.term` durably before
arming, so every token — and every DMA authorization — minted in an earlier era is stale by
construction", `DC:L37-44`), the per-process CUSTODY GENERATION in the low `GRANT_SEQ_BITS` ("0 on every
mount that does not hold remote write custody", `DC:L160-164`). `authorize_dma(carried)` (`DC:L271-297`)
is THE gate every data-plane write passes: poisoned ⇒ `WriterGuardFenced` (`data_dma_fence_refusals`);
`carried ≠ current` ⇒ refused + `data_dma_epoch_refusals` ("the offsets it authorized may already be
reallocated"). `advance_custody_generation` (`DC:L218-240`): a revoked grant costs the work authorized
under it, never the process; saturation is a POISON, not a wrap ("a carry into the term field would forge
a newer era"). `adopt_custody_generation` is monotone (`fetch_max`, `DC:L194-203`). Poison is sticky
(`DC:L150-153`, `L301-309`); under the symmetric plane it SPLITS: park for slot leases and the appender
region, poison only for non-reclaimable custody (`DSM:L563`; pin `data_custody_poison_splits_by_fence_
class`).

Port: the write path's carried epoch is `write_seams.zig`'s `current_fencing` (rung 6 engages, rung 4
engages with 1, `P/data/write_seams.zig:L204`); `authorizeDma(carried: ?CustodyEpoch)` is the ONE function
every DMA submit calls (the port's `plane.zig` submit gate), two relaxed loads on a solo mount.

### 6.2 The dead epoch and the block quarantine (the "custody words")

`DeadEpoch(u64)` from a monotone sequence, "never reused" (`DC:L323-354`). `BlockQuarantine`
(`DC:L357-457`): `admit(offset, epoch) bool` (idempotent; `dlm_quarantined_offsets`), `contains`,
`defer_free(offset) bool` ("the publish is owed to `release`"), `release(epoch) -> [(offset, free_
pending)]` (`dlm_quarantine_releases`). The allocator's faces §3.6. Pressure ruling: never force-drains
(`DC:L73-82`).

### 6.3 `DrainProof` — "no release without a proof" is a TYPE

`DrainProof::{PreemptLanded{namespaces ≥ 1}, ProvenDead{detail}}` (`DG:L1100-1148`): "there is no
`DrainProof::assumed()`, and `preempt_landed` refuses to exist for a preempt that landed on zero
namespaces". The custody authority's death path (`finish_kill`, `DG:L2238-2292`): the table half dropped
the lease and its grants' local leases; here ONE dead epoch is minted per CLIENT (not per grant — "a
client holding three grants has ONE custody", `DG:L50-54`) and the client's DECLARED in-flight
destinations (`RenewFrame.inflight: Vec<u64>` — "the cohort a dead epoch's quarantine is keyed on",
`DG:L359-367`) enter the quarantine through the `CustodyQuarantine` trait (`DG:L1056-1063`; "a data-plane
authority must be armed with one"); `release_dead(dead, proof)` (`DG:L2300-2315`) releases the cohort.
The expiry sweep `expire_due` runs on the cadence task, "never on a handler lane" (`DG:L2204-2229`).

### 6.4 The two Dekker pairs (`CRC`; `LOOM:L7655-7834`)

**Pair 1 — revoke / enter** (`CustodyUseCore{uses, cached}`, `CRC:L61-121`): a handler `enter()` =
`uses.fetch_add(SeqCst)` + `fence(SeqCst)`, then `is_cached()` (a SeqCst load) at the ONE cached-lease
accessor; the recall `revoke()` = `cached.store(0, SeqCst)` + `fence(SeqCst)`, then `settle_ready()` =
`uses.load(SeqCst) == 0`. Either the handler reads the lease revoked and re-acquires, or the settle counts
it in flight and the release waits — never "a write under a released grant". Weakening (`LOOM:L7661-
7669`): both fences removed and the four accesses `Relaxed` ⇒ loom finds `cached == 1 ∧ uses == 0`; the
handler's fence alone removed ⇒ the same; "the pair needs BOTH fences". Tests `a_handler_that_serves_the_
cached_lease_is_counted_before_the_release_departs` (`LOOM:L7695`), `two_handlers_against_one_recall_are_
each_counted_or_refused` (`LOOM:L7733`).

**Pair 2 — the handover's mark / the grant's re-check** (`HandoverMarkCore<K>{marks: Mutex<HashMap>,
count: AtomicUsize, seq}`, `CRC:L123-140`): the handover `arm`s its slot's mark under the mark table's
lock (the fast-path count stored) BEFORE its census under the grant table's lock; a grant inserts under
the grant table's lock BEFORE it re-reads the mark (the `Relaxed` count first, the table under its lock
only when non-zero). "Both sides take both locks, one per step, so the locks' acquire/release chains
order the steps … the `Relaxed` fast path is INSIDE the happens-before chain". Weakening (`LOOM:L7676-
7683`): the grant's two steps SWAPPED ⇒ the census is empty AND the grant reads `defers == false` — "a
grant spanning the move". `clear_if(key, seq)` clears only the mark it armed (`LOOM:L7810-7833`:
"stamps are monotone"). Tests `a_grant_racing_the_handover_is_censused_or_deferred` (`LOOM:L7767`),
`an_older_guards_drop_never_clears_a_newer_held_mark`.

Port: both cores become standalone models under `src/model/explorer.zig` (`expectHolds` + the
weakenings `expectFails`, store buffering modelled explicitly as the charter requires) before the
process plane binds them; Zig 0.17 has no standalone fence, so pair 1 is written as the port already
writes W1 — SeqCst RMWs on the words themselves (`PBA:L30-43`).

### 6.5 How W1's sole-owner probe consults them on an armed set (charter §6.2 item 2)

`sole_owner_posture_verdict` (`RT:L8199-8216`), SYNC, decided BEFORE any probe or the incarnation retire
("ONE function so the write path's pre-ack probe and W1 cannot drift"): unarmed ⇒ `None`; `!allocator.
holds_ownership_plane()` ⇒ **`NonHolder`** (a JOINED appender holding no allocation lease for the block's
data volume: "the patch retires a lifetime that plane accounts; the CoW path + the shipped free, counted
`patch_ineligible_posture`" — `RT:L1726-1734`; `holds_ownership_plane`, `BA:L1090-1098`: lease held OR the
authority-accounting scope); `slot_holder_home(ino).is_some()` ⇒ **`ForeignCustody`** (this mount holds the
file's custody from its SLOT HOLDER — "the block's references live in the holder's tree, which this mount
cannot read; the CoW path", `RT:L1716-1725`; `patch_ineligible_foreign_custody`). Then the durable clause
(`sole_owner_verdict`, `RT:L8229-8267`): ONE range probe of the patching ino's slot tree, `count == 1 ∧
!shared` ⇒ `Sole`, anything else (or a probe that cannot read — "the leak-safe direction") ⇒ `Shared`
(`patch_ineligible_shared`). Only then `begin_patch_sole_owner` (`BA:L1889-1923`): the plane gate, retire,
fence, `refcount == 1 ∧ !is_shared` (RAM) — "defense-in-depth, structurally unreachable from the product
ladders on every posture". Pins `w1_declines_a_foreign_custody_file_and_keeps_the_durable_probe_for_own_
files` (`tests/sym_custody_tests.rs:882`), `the_w1_ladders_decline_a_non_holders_patch_as_a_counted_
posture_decision` (`tests/sym_shared_refs_tests.rs:1284`), `the_w1_predicate_confirms_sole_ownership_in_
the_inos_slot_tree_on_an_armed_set` (`:571`).

Port: `patch_w1.CustodyProbe.sole_owner` already returns `SoleOwnerVerdict{sole, shared, foreign_custody,
non_holder}` and declines `.foreign_custody` / `.posture` before `beginPatchSoleOwner`
(`P/data/patch_w1.zig:L145-158`, `L217-222`). Rung 6 binds: `non_holder` = `!PlaneGate.holds(.patch_
retire)` read WITHOUT the gate's ERROR line (a second, silent accessor beside `planeGate`), `foreign_
custody` = the port's slot-custody table has a home for the ino, `shared` = the kind-6 probe's `shared`
fold.

### 6.6 The durable slot custody quarantine

`custody_quarantine:{slot}` in tree 0: key `"custody_quarantine:" ‖ slot u32 BE` (23 B), value `version
u8 ‖ until_ms u64 LE` (`SS:L249-305`); written in the recovery's own tree-0 entry beside the slot's
`Unleased` for a slot recovered from an EARLY death record, read at every arm, deleted once expired. The
bound `T_owner + 2 × skew_max` past the record's `ts_ms` (`DG:L6187-6216`: `T_owner` is the OWNER's view
past which a holder may re-grant; `2 × skew_max` absorbs the recorder's clock); the grant paths ask
`custody_quarantine_remaining(ino)` behind one relaxed load (`DG:L6222-6245`), answered with the retryable
class (`slot_custody_quarantine_refusals`, `slot_custody_quarantined`). Port v4: `extern struct {
version u8 = 1, reserved [7]u8, until_ms u64 }` = 16 B.

## 7. Knobs, gauges, constants

### 7.1 Knobs (class per charter §1.5–§1.6; registry rows for `src/core/knobs.zig`)

| row | class | reference | port |
|---|---|---|---|
| `SQUEEZEFS_TEST_ALLOC_ARM_HOLD_MS` / `_VERDICT_HOLD_MS` | test seam | `EK:L259-260` (the arm's hold→re-snapshot and snapshot→verdict windows) | keep both (the arm's two races) |
| `SQUEEZEFS_TEST_HOLDER_GRANT_EXHAUST_N` | test seam | the static `TEST_HOLDER_GRANT_EXHAUST_N` (`AL:L2668`) is NOT a knob | a registry row, read once at the holder's arm |
| `SQUEEZEFS_FREE_GRACE_MAX_MS` | A/B lever (explicit override of a derived bound; below one cycle REFUSES) | `EK:L359`; `FG:L1118-1141` | keep; range `[1 s, …]`, the gate refuses `< ack_cycle` |
| `SQUEEZEFS_FREE_GRACE_MAX_OFFSETS` | A/B lever (override of `resolve_ring_cap`) | `EK:L377`; `FG:L1084-1089` | keep |
| `SQUEEZEFS_FREE_GRACE_VALVE` | A/B lever (rungs a+b off) | `EK:L376` | keep |
| `SQUEEZEFS_FREE_GRACE_{DEMAND, PASS_ELASTIC, ACK_PIPELINE, ACK_RENEWAL, CAUGHT_UP_RELAX, REFRESH_ON_ACK, QUALIFY_CEILING, DRAIN_EPOCH_STAMP, DRAIN_OBSERVED, CHECKPOINT_COMPOSITE}` | A/B levers of the reader-ack ladder | `EK:L364-373` | NOT carried at rung 6: under GPFS-strict tokens the recall is the qualification and the ring is the timeout path (`DSM:L524`; charter §5.5 "tokens — the ONLY foreign-read method"); the ladder's arms belong to the retired S5 epoch reader (0002 row 45's class) — §9 Q10 |
| `SQUEEZEFS_FREE_GRACE_LANE_PUSH` | retired | `EK:L375` | never existed |
| `SQUEEZEFS_RANGE_CUSTODY` | A/B lever, default off | `EK:L347` | rung 6 ships whole-file custody only; the row is added when range custody lands |
| `SQUEEZEFS_SYM_ALLOW_NON_PR` | weaker-fence posture | `DC:L1391-1392` | not carried (0002 row 188) |
| `SQUEEZEFS_RECLAIM_BATCH_BLOCKS` (64) | existing | `EK:L190` | `HARVEST_BATCH` imports it |

### 7.2 Gauges (names = the registry; every one reads 0/`solo` on a `--single-writer` mount)

`alloc_lease` family (`AllocLeaseStats`, `AL:L954-985`, per holding): `alloc_lease{vol_tag, term}`,
`block_grants`, `block_grant_blocks`, `block_grant_blocks_returned`, `block_grants_revoked`,
`data_alloc_bitmap_set_bits`, `data_alloc_bitmap_clear_bits`, `data_alloc_bitmap_population` (O(1)
maintained, `DAB:L578-586`), `data_alloc_bitmap_deltas_journaled`, `data_alloc_bitmap_deltas_deferred`,
`free_grace_timeout_deferrals` (per ring).
Process gauges (`DAB:L969-1025`): `data_alloc_bitmap_drift` (must-stay-0, loss direction),
`data_alloc_bitmap_leaks_released / _deferred / _adopted / _pending` (closure `deferred ≡ released +
adopted + pending`), `data_alloc_bitmap_decls_rejected` (must-stay-0), `data_alloc_bitmap_arm_absorbed_
frees`, `data_alloc_population_drift` (tripwire), `population_scans` (0 on the funnel).
Writer side (`BA:L803-813`, `L2963`): `block_grant_topups`, `block_grant_window_remaining`,
`block_grant_window_recycles`, `block_grant_exhausted` (mints refused empty), `alloc_fresh_supply_reask_
ms`; refusal classes `HolderUnreachable` / `HolderDeferred` ride `retryable_refusals{class}`.
Death ledger (`AL:L994-1020`): `dead_members_recorded`, `dead_members_acted`, `dead_members_quarantined`,
`dead_member_propagation_ms`, `dead_member_write_deferrals` (must-stay-0), `dead_member_records_retired`,
`recovered_records`; `manager_vol0_unreachable`.
Frees: `free_shipped_blocks`, `free_served_blocks`, `free_refused_blocks`, `free_ship_failures`
(`PUB:L1126-1159`), `block_live_free_refusals`, `block_untracked_free_refusals` (tripwires, already in
`PBA:L377-381`), `block_untracked_free_adjudicated` (`PBA:L396`), `served_publish_free_block_refusals`
(must-stay-0), `joined_holder_venue_moves`; grace: `free_grace_deferrals / releases / forced_releases /
laggard_fences / alloc_stalls / pressure_parks / offsets / bytes / reader_acks / prods / bound_
tightenings / pressure_pct / effective_bound_ms / bound_age_ms / residence_ms (histogram) / hold_ms /
hold_phase_ns (exact-sum: defer→checkpointed, checkpointed→min_acked, min_acked→released, total;
`FG:L419-431`) / member_ack_lag_ms / ring_cap / recall_gated_frees / recall_timeout_deferrals`.
Shared index (`SR:L69-82`): `block_ref_probes`, `share_block_calls`, `release_shared_calls`,
`mark_shared_calls`, `shared_release_failures` (must-stay-0, already `PBA:L397`), `shared_blocks`
(`BA:L1959-1962`), `fsck_shared_index_drift`.
Custody (`DC`; `DG:L631-736`): `dlm_quarantined_offsets`, `dlm_quarantine_releases`, `dlm_custody_epoch_
advances`, `data_dma_fence_refusals`, `data_dma_epoch_refusals`, `custody_grants / renewals / releases /
conflicts / revokes / expiries / reclaims / grace_conflicts (must-stay-0 on a healthy failover) / unknown_
leases / self_fences / quarantined / proofs`, `slot_custody_quarantine_refusals`, `slot_custody_
quarantined`, `patch_ineligible_{shared, foreign_custody, posture}` (`P/data/patch_w1.zig:L64-106`
already), `accounting_plane_refusals`.
fsck: `fsck_alloc_bitmap_leak_candidates`, `fsck_alloc_bitmap_tracked_exempted`, `alloc_census_declined`
(`FSCK:L7405-7420`).
Manager: `manager_verbs / _replays / _refusals / _rejected` per verb (`MGR:L441-453` names).

### 7.3 Constants with WHY, and measured numbers

| constant | value | WHY (anchor) |
|---|---|---|
| `BLOCK_GRANT_FLOOR` | 64 blocks | one write-pipeline BDP window of 4 MiB blocks = 256 MiB between two beats (`BG:L47-51`) |
| grant cap | `cap / (2 × writers)` | half the volume stays for reuse (`BG:L19-20`) |
| `T_renewal` | `min(T_self/3, 10 s)` | three attempts before the member's own deadline (`LCC:L62-64`; `MB:L3971`) |
| `T_owner` / beat | 45 s / 10 s | `fuse_client.rs:L191-195` |
| `DEATH_RECORD_RETIRE_TTLS` | 2 × `T_owner` | one TTL to propagate+act, one for the successor's grace (`AL:L1053-1065`) |
| custody quarantine bound | `T_owner + 2 × skew_max` | the owner's re-grant instant + recorder clock disagreement (`DG:L6187-6216`) |
| `T_park_max` | `failover_bound + grace`, `grace = T_owner` | `AL:L1221-1226`; `MB:L901` |
| `DATA_ALLOC_PAGE_BITS` | 32,512 (4064 B payload) | 4 KiB sector − 32 B header (`DAB:L78-86`) |
| bitmap size | 32 KiB / TiB at 4 MiB | `DAB:L115-124` |
| `DELTAS_PER_ENTRY` | 1024 | one control entry per drain chunk (`AL:L1349`); moot under run deltas |
| `GRACE_ENTRY_BYTES` | 24 | `(label, offset, size)` (`FG:L1052`) |
| `RING_CAP_FLOOR` | 131,072 | 12.7 GB/s × ≈ 38 s ack cycle ≈ 120 k blocks (`FG:L1054-1060`) |
| ring cap | `max(budget/1024/24, floor)` | < 0.1 % of the budget, NOT an R5 component (it cannot shed) (`FG:L1070-1080`) |
| `HARVEST_BATCH` | 64 | = `SQUEEZEFS_RECLAIM_BATCH_BLOCKS` (`FG:L1062-1068`) |
| `ack_cycle` | `3·beat + 3·staleness + skew + D_purge` | `FG:L1091-1107` |
| routine fence bound | 2 × cycle | one missed cycle tolerated (`FG:L1109-1116`) |
| `FREE_SHIP_ATTEMPTS` | 3 | each resend absorbed exactly-once by the dedup window; past it leak-safe (`SF:L77-82`) |
| `RELEASE_RESIZE_ATTEMPTS` | 8 | a clone storm on one block; a loud-refusal belt, never a wait (`SR:L84-89`) |
| arm verdict persistence | 2 reads ≥ 5 ms apart, ≤ 3 re-reads | the clear→insert gap in `publish_free_list` (`AL:L3150-3157`) |
| same-node takeover attempts | 4 | one takeover + one deferred retry (`AL:L3532`) |
| `ShareBlock` chunk | 512 refs | the control entry's cap, ≈ 50 B per record (`RT:L8127-8138`) |
| pin retry | 3 | a patch is one DMA; a third proves a stuck writer (`PBA:L997-1003`) |

Measured: the leak growth without declared windows "53 → 340 of 4,096 blocks per volume across two
failovers" and the double allocation "200 mints → 144 distinct" (`AL:L497-500`, `L3254-3255`); the
shipped-free wall row (§4.1, `ACC:L1036`); `df` RED `4096 vs 3996` (`ACC:L4941`); the live full-volume
proof `data_alloc_bitmap_population 512 → 147` per pressure close (`ACC:L5137`); C6 false-positive
`386-vs-385` on single-writer arithmetic (`FSCK:L5398-5399`); the un-share's price ≈ 2,500× rand-4k
(`RT:L1719-1723`).

## 8. Tests (name — one sentence; venue)

Unit / in-process (`tests/sym_block_grant_tests.rs` unless noted; the port writes each as a portable
contract over the pure cores, Linux-gated only where a device is needed):
- `two_writers_grants_are_disjoint_and_the_zombie_floor_is_the_grants_end` (:245) — two carves never
  overlap and `grant_frontier` is the end of the highest.
- `the_block_grant_derivation_is_clamped_between_its_floor_and_cap` (:313) + `BG:L634-648` — the `G`
  tie test.
- `a_grant_journaled_before_a_holder_death_is_never_regranted` (:499; `_after_a_checkpoint` :504;
  `a_kill_after_the_ledger_record_still_finds_the_journaled_grant_set` :511; `_on_a_two_volume_set`
  :518) — the §5.5.1 headline row across kill windows (kill matrix; venue).
- `a_second_volumes_checkpoint_never_writes_the_holders_pages_onto_its_own_device` (:530).
- `a_terminal_frees_clear_is_journaled_at_finish_free_in_order_with_a_regrants_set` (:587) — the
  ordering law: CLEAR before a later SET of the same block, no checkpoint in the path.
- `an_allocation_lease_successor_never_regrants_a_referenced_block` (:646) — the whole crash table.
- `a_death_record_revokes_the_dead_writers_grants_into_the_quarantine` (:743).
- `an_armed_two_writer_set_allocates_disjoint_ranges_from_grants` (:816).
- `the_alloc_lease_record_round_trips_and_a_release_is_idempotent` (:894).
- `a_parked_member_reclaims_against_the_successor_the_ledger_names` (:1010); `a_park_never_granted_
  expires_at_t_park_max_with_eio_never_silent` (:1226); `a_parked_members_grants_stay_valid_and_its_
  token_service_continues` (:2425) — liveness is the ledger, not a TTL.
- `a_checkpoint_under_ring_pressure_lands_the_pages_and_never_fails_on_a_queued_delta` (:1288).
- `arming_a_populated_volume_seeds_the_bitmap_and_never_regrants_a_live_block` (:1367);
  `arming_a_populated_forest_set_seeds_from_the_refs_union_and_refuses_an_unseeded_allocator` (:1489).
- `an_armed_holders_supply_terms_read_the_window_and_the_clear_population` (:1604);
  `an_armed_holders_mint_lands_on_the_frees_the_valve_drained_in_the_same_attempt` (:1702);
  `an_armed_holders_used_count_follows_its_bitmap_not_the_single_writer_arithmetic` (:1825).
- `a_poisoned_window_declaration_is_refused_before_any_walk` (:1904) — `screen_window_decl`.
- `the_shared_index_home_follows_the_allocation_holder` (:2043).
- `the_funnel_reads_the_population_in_o1_and_the_checkpoint_checks_it` (:2087).
- `the_free_target_map_names_the_holder_and_finish_free_clears_its_bit` (:2605);
  `the_bitmap_agrees_with_the_census_oracle` (:2660); `free_grace_closure_holds_per_ring_on_two_hand_
  made_rings` (:2693).
- `the_wire_serves_pr8s_verbs_and_record_death` (:2859) — the verb block over a listener (2 daemons).
- `an_unarmed_allocation_plane_is_the_shipped_one` (:3055); `the_data_bitmap_is_thirty_two_kib_per_tib`
  (:3101).
- `a_clean_leave_then_a_mount_at_another_mount_point_of_the_same_host_arms` (:3124) — the same-node
  successor.
- `a_crashed_incarnations_window_remainder_is_released_at_the_rehold` (:3205); `a_joined_appenders_
  never_published_mint_returns_to_its_grant_window` (:3292).
- `an_unreachable_holder_is_the_retryable_class_never_exhaustion` (:3377); `a_holders_full_ring_beat_
  defers_the_ask_and_the_next_ask_lands` (:3435); `a_holder_whose_lease_is_in_motion_defers_the_ask`
  (:3509); `a_wire_deferred_answer_keeps_the_grant_session` (:3556).
- `DAB` unit: `page_round_trips_and_refuses_the_wrong_volume_and_index` (`DAB:L1045`), `deltas_fold_lww_
  and_the_run_carve_skips_set_bits` (`DAB:L1059`); `BG` unit: `a_given_back_block_is_the_next_mint_and_
  merges_onto_its_neighbours` (`BG:L651`), `grants_are_disjoint_and_the_window_mints_lowest_first`
  (`BG:L681`).
Shared index / clone (`tests/sym_shared_refs_tests.rs`): `a_clone_on_an_armed_forest_marks_the_source_
shared_and_indexes_both_inos` (:89); `a_clone_racing_a_truncate_never_frees_or_patches_a_shared_block`
(:167); `a_clone_of_a_clone_inherits_the_shared_bit` (:240); the two cloner-death windows (:284, :326);
`mark_shared_is_idempotent_against_the_durable_witness_and_gone_aborts_the_clone` (:395);
`an_unarmed_mount_clones_plain_and_every_gauge_stays_zero` (:436); `the_shared_marks_are_seeded_from_
the_index_at_the_arm` (:475); `the_one_slot_probe_agrees_with_the_derived_census_on_every_published_
block` (:519); `the_w1_predicate_confirms_sole_ownership_in_the_inos_slot_tree_on_an_armed_set` (:571);
`a_same_slot_clone_of_an_unshared_block_runs_no_protocol_and_a_shared_source_does` (:768); `a_full_ring_
0_parks_the_index_writers_and_never_fails_or_leaks` (:874); `a_truncate_shrink_of_a_clone_shared_tenant_
keeps_the_durable_shared_bit` (:1005); `the_clone_verbs_ride_the_manager_wire` (:1104); `the_index_
lives_in_tree0_and_never_pollutes_a_slot_trees_probe` (:1237); `the_w1_ladders_decline_a_non_holders_
patch_as_a_counted_posture_decision` (:1284); `a_holders_free_of_a_former_lessees_block_runs_the_owner_
ladder_instead_of_refusing` (:1386); `a_non_holders_rewrite_of_its_own_mapped_block_is_refused_on_a_
full_set` (:1609).
Pack scope (`tests/sym_pack_tests.rs`): `an_armed_set_packs_per_slot_and_an_unarmed_mount_keeps_pk2s_
one_scope` (:232); `two_meta_volumes_native_inos_never_share_a_pack_scope` (:353); `a_pack_block_whose_
slot_is_handed_over_keeps_its_whole_population_in_one_tree` (:393).
Custody (`tests/sym_custody_tests.rs`): `w1_declines_a_foreign_custody_file_and_keeps_the_durable_
probe_for_own_files` (:882); `a_recalled_writers_cached_lease_is_revoked_and_its_writes_reacquire_at_
the_new_holder` (:2418); `a_recall_crossing_the_slow_path_between_cache_and_insert_parks_the_lease_at_
once` (:3104); `a_grant_inside_the_flush_then_transfer_is_deferred_to_the_next_holder` (:2627);
`a_dead_holders_t_self_fence_is_scoped_to_its_own_custody_and_driven_by_the_cadence` (:1563);
`the_handovers_parks_and_bounds_derive_from_the_lease_clocks` (:3371).
Loom → port models: `shared_ref_core_composed_never_patches_or_frees_a_marked_block` (`LOOM:L7339`);
the four `custody_revoke_models` (`LOOM:L7695-7833`), each with its weakening.
`df`: `test_df_reflects_allocator_delta_after_durable_write`, `test_df_answers_against_live_mounted_
volume` (`tests/cli_clients_df_tests.rs:744`, `:799`) — CLI-level, venue.
**N-daemon needs** (`tests/sym_n_daemon_tests.rs`, the venue with ≥ 2 daemons on one set): `a_second_
daemon_joins_over_the_wire_and_commits_into_its_own_ring` (:328); `a_clean_leave_and_rejoin_of_one_
identity_never_regrants_a_live_image` (:706); `concurrent_storms_on_{two,four,eight}_writers_…`
(:873-902); `a_dead_joiners_region_is_recovered_by_the_manager_and_a_third_daemon_takes_its_slot`
(:3610); `a_rejoined_identitys_fresh_region_is_never_recovered_under_its_predecessors_death_record`
(:3733); plus the `sym-crash` / `sym-walls` acceptance legs (`ACC:L1036`, the grant failover pin "200
mints → 144 distinct"). The port's evidence note for this rung must carry a 3-daemon kill row: the
holder killed between a journaled carve and its page write, the successor's first carve above the
dead grant.

## 9. QUIRKS (decision 0002 rows to add) and the port seam each piece fills

| # | reference quirk | why poor | port design | seam |
|---|---|---|---|---|
| Q1 | Process-wide statics for every plane: `HOLDINGS`, `ALLOCATORS`, `FREE_TARGETS`, `FREE_VENUES`, `REPLAYED_DELTAS`, `MOUNT_SEEDS`, `SET_HEAPS`, `DATA_VOLUME_BLOCKS`, `CUSTODY_QUARANTINE`, `PENDING_DEATHS`; `DeferredLeaks` as `BTreeSet<u64>` + `BTreeSet<String>`; the ledger keyed by writer `String` | 0002 rows 68/191: path/process-keyed globals; per-op allocation (`Vec<BlockGrant>` per carve, `String` keys) | fields of `DataPlane` (per data volume: `?*Holding`, `?GrantWindow`, `?FreeTarget`) and `MetaSet`; `DeferredLeaks` a bitset + a writer-id bitset over the appender directory; writers keyed by appender id | `AllocationLease`, `GrantSource`, the `?FreeTarget` of `DAR §11.7` |
| Q2 | The holder keeps TWO bitmaps — `DataAllocBitmap` (words + dirty) beside the allocator's `free_blocks: DashSet` — and the arm DRAINS the list into the bitmap (`AL:L3309-3319`; `BA:L987-996`) | two sources of one truth; the drain is a race window the arm then re-drains | the port's `bits` IS the bitmap (`PBA:L14-20`): the holder's `Holding` OWNS the allocator's `bits` words (page images are serialized from them, 508 words per page), `FinishFreeSink` is "journal the delta"; no second structure, no drain | `FinishFreeSink` (`PBA:L294-298`), `derivedAllocationSnapshot` (`PBA:L1326`) |
| Q3 | Kind 4 shared by heap deltas (8-B key) and data deltas (16-B key), told apart by length (`DAB:L21-25`, `L242-247`) | one kind, two codecs | kind 5 `TREE_DATA_ALLOC_RESERVED` (landed, 0002 row 44) | `P/meta/key.zig:L39-42` |
| Q4 | `alloc_lease.control_ino` always `ROOT_INO`, read by nothing (`AL:L3534`, `L163-164`); `dead_member` decoded by THREE lengths (`AL:L309-320`) | a dead field in a forward-only record; length-sniffed versions (0002 row 41's class) | §1.1's v4 structs: no `control_ino`, one length each, reserved bytes must be 0 | the tree-0 record codecs |
| Q5 | The `early` recorder class as a trailing optional byte | same as Q4 | a fixed byte at offset 1 | same |
| Q6 | One delta record per BIT: a floor grant = 64 records (`AL:L556-565`, `L2215-2234`), `DELTAS_PER_ENTRY = 1024` chunks | 64 × ≈ 45 B of ring per carve, 64 LWW map entries per replay | RUN deltas `{tag, len u32, term}` (§2.4), expanded at the fold; a carve is one record, a `finish_free` CLEAR `len = 1` | the holder's delta queue |
| Q7 | `G`'s EWMA term is never fed: the holder derives at `ewma = 0` (`AL:L2274-2280`), the writer asks `want = 0` (`BA:L829`) — every grant is 64 | a derivation that always answers its floor; the floor is "the ramp" (`DSM:L518`) with no steady state | the WRITER measures `ewma_blocks/s` from the write-pipeline governor's delivery rate ÷ block size and names `want = clamp(2 × ewma × T_renewal, 64, …)` on the ask; the holder CAPS by `cap/(2 × writers)` (its screen) — "a WIRE joiner measures its own rate and names its size" (charter §5.5 extent grants) | `GrantSource.wantsTopup` + the sink's `want` |
| Q8 | A grant-armed NON-holder's `df` keeps `highest − free_list` (`BA:L3036-3039`; `ACC:L4941` "PR 14b owed") | a joiner's `statfs` lies after deletes | the holder's `population` rides every `BlocksGranted` reply and the renewal grant (`HolderView` for a joiner = `{blocks, population, window_remaining}` from the last word) | `AllocationLease.holderView` (`PBA:L329`) |
| Q9 | The shared index as `"shared_ref:"`-prefixed records in tree 0 (`SR:L14-18`) with `TREE_SHARED_INDEX = 9` "reserved, unwritten" (`SR:L23-27`) | an ASCII prefix on a binary key in a tree that already has a kind byte; a reserved kind nobody uses | kind 9 records keyed by the 29-B ref key (kind byte 9 ‖ the kind-6 key's body), value `{version u8 = 1, reserved [3]u8}`, in the holder's home volume's tree 0 — one range probe per block like kind 6 | `P/meta/key.zig:L50`; `SharedFreeGate` (`PBA:L352-356`) |
| Q10 | The free-grace ring carries the S5 epoch reader's whole ack ladder (`ReaderAckLadder`, 10 A/B levers, `FG:L2232-2960`, `EK:L364-373`) | the S5 projection reader retired with the flip (`FG:L3438-3441`); under tokens the ring is the timeout path alone | the port ships `GraceRing` + the recall gate + the laggard fence only; labels = the port's S6 owner clock; `ack_cycle` and the two bounds stay as the fence's derivation; no prod/tighten/demand ladder until a measured reader needs one | `GraceRing` (`PBA:L338-349`), `enospc.zig` |
| Q11 | `MarkShared` served under the FUSE layer's `BLOCK_FLUSH_LOCKS` (`MGR:L964-986`) — the metadata plane reaching into the write path's lock table | a cross-layer lock; the port's block guard is the data plane's | the served mark takes the port's `plane_locks` block stripe through a `DataPlane` hook the meta plane calls (the same rung-3 lock order; `plane_locks_model.zig` extended) | the `MarkShared` executor |
| Q12 | `custody_quarantine` lives in a RAM `HashMap` with lazy expiry (`DG:L6165-6245`) beside the durable record read only "at every arm" (`SS:L249-254`) | two homes; the RAM one answers the grant path | one durable record (§6.6) + a per-slot `until_ms` word in the slot lease table, written and read together | the slot custody table (rung 6) |
| Q13 | `holds_ownership_plane()` duplicates `plane_gate`'s lease arm to avoid its ERROR line (`BA:L1075-1098`) | two readers of one word | ONE `PlaneGate.holds` consulted silently by the posture verdict and loudly (tripwire + log) by the mutation sites | `PlaneGate` (`PBA:L286-289`) |
| Q14 | `FREE_SHIP_ATTEMPTS` sleeps 10 ms between attempts (`SF:L392`); the arm's verdict sleeps 5 ms (`AL:L3169`) | fixed sleeps as synchronization (AGENTS "no sleeps") | the ship retries ride the `io` executor's timer with the ladder's own backoff law; the arm's two reads are separated by the reclaim lane's quiesce (`Notify`), the 5 ms a documented floor | `reclaim.zig`'s terminal path |
| Q15 | The quarantine's owed publish re-enters the grace ring and `harvest_grace` inside `release_quarantine` (`BA:L1176-1186`) — a holder-side cohort scan `iter_sync` over a hash map | O(entries) per release over a hash | the cohort is a `State.quarantined` scan over the dense cells (Q3's shape); release = state → limbo → `finishFree` | `quarantineBlock / releaseQuarantine` (§4.4) |

Seam map (every piece → the port seam it fills, `DAR §11` numbering): the lease record + verbs →
`AllocationLease` (4, 6) + the rung-6 manager verb table; the bitmap + deltas + pages → `FinishFreeSink`
(2) over the port's own `bits`, the Checkpointer's `PageWriteSet`, the recovery's kept window; grants →
`GrantSource` (3) with `GrantWindow` + the sink; declared windows → the S6 renewal frame + the holder's
`DeferredLeaks`; shipped frees → the `?FreeTarget` (7) + the three tracking seams; the grace ring →
`GraceRing` (11) + the recall gate; quarantine → (10); the shared index → `SharedFreeGate` (8) + the
kind-9 codec + the clone path's `MarkShared/ShareBlock` client; the untracked ladder → `UntrackedFreeGate`
(9); custody → `PlaneGate` (1), `CustodyProbe.sole_owner`, `write_seams.current_fencing`, the two models.
