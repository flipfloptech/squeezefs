# The data-plane allocation partition

DLM **stage S9**, blocker **#3**, in S9's own words:

> *No data-plane allocation partition. Two writers' allocators would collide
> on fresh offsets — the DATA analogue of §6.2 item 3. This is the largest
> remaining gap for a real two-host write row.*

Landed in `src/data_alloc_lane.rs` + `src/block_allocator.rs` (the partition)
and `src/alloc_lane_grant.rs` (the admission — §9a); contracts
`tests/mw_data_alloc_lane_tests.rs` + `tests/mw_cowriter_lane_tests.rs`; operator surface `docs/operations.md`
§Multi-writer capacity planning. Siblings this composes with:
`docs/design-mw-cursors-and-incarnation.md` (§6.2 items 5/6 — the ino lanes
whose arithmetic this reuses, and the lifetime stamps whose funnel it must
not bypass), `docs/design-durable-block-refcounts.md` (item 1 — the durable
shared-ownership accounting this leaves untouched),
`.benchmarks/2026-08-05-mw-partitioned-append.md` (items 2/3/4 — the
page-partitioned extent bitmap this is the block-offset analogue of).

**No numbers in this document — ruling D11.** *"No cargo test, benches or
release gate yet until we are done implementing the DLM and can have N
Readers and Writers."* Every cost statement is a **prediction with a
falsification criterion** (§6), and the bench coverage
(`benches/write_path_bench.rs::alloc_lane`) exists **un-run**.

**Scope discipline, stated first.** This lands the **partition**, not the
admission. *Who* may write, and *which* writer is lane `w` of `W`, is §6.9
S4/S8/S9's problem — exactly as `kv/ino_lane.rs` says for inos. The line
this work had to reach is: *two writers' allocators cannot hand the same
device offset to two owners, a crash cannot make them, and a single writer
pays nothing for the property.*

**The admission landed separately, and it is §9a of this record**
(`src/alloc_lane_grant.rs`; contracts `tests/mw_cowriter_lane_tests.rs`): the
authority derives one lane per enrolled writer from the durable claim set,
carries it to each co-writer on the S9 custody lease, and — because the
reservation record of §3 is a *metadata commit* a co-writer cannot perform —
**commits that record on the co-writer's behalf**, ahead of every hand-out.
Read §9a together with §3.

---

## 1. The assumption, in source

| Assumption | Anchor | Failure under a second writer |
|---|---|---|
| `BlockAllocator`'s fresh-block cursor is a per-mount atomic over a shared index space, and its free list is a per-mount `DashSet` derived from the durable references | `block_allocator.rs` (`next_fresh_block`, `try_allocate_block`, `seed_from_durable_refs`) | Two writers mint **the same block index**, DMA into it concurrently, and both publish a layout that references it. On a passthrough volume that is **silent**: each file reads back a mixture. It is the §6.2-item-3 failure one plane down, and unlike the metadata planes there is no journal that would notice |

Note precisely *why* the derived free list does not save this. It is derived
from the SET's durable references, so both writers compute nearly the same
answer — which is the problem: two writers reading one free list both see
block 7 free and both claim it.

---

## 2. The law: fresh allocation is a residue class; frees are lane-blind

With `writers = W`, block index `b` belongs to **lane** `b % W`, and writer
`w` mints only lane-`w` indices: `w`, `w + W`, `w + 2W`, … The arithmetic is
`src/lane_core.rs` — the SAME core §6.2 item 5's ino lanes and item 6's
lifetime stamps run, with `base = 0` (block 0 is an ordinary allocatable
block, where local inos 0/1 are reserved).

**Why not a range hand-out.** Leasing chunks `[start, start + N)` from a
durable distribution watermark needs a **hand-out protocol**, i.e.
arbitration — which S4/S8 own and which this work is explicitly not allowed
to build. A residue class needs nothing but the writer's own id. The
interleave-not-contiguous choice is the extent bitmap's, for its reason:
an existing volume's allocation is dense at the low end, and interleaving
spreads N writers over it instead of handing writer 0 everything.

Three properties follow, and the third is what makes the DATA plane
**easier** than either metadata plane:

1. **disjointness** — two lanes never mint the same index, so "one device
   offset has at most one live owner" survives N concurrent writers *with no
   agreement between them*;
2. **attribution** — `block_lane_of(idx, W)` names the writer that minted
   any offset, so recovery and fsck classify a foreign writer's blocks
   instead of guessing;
3. **frees need no protocol at all.** The owning lane is *derivable from the
   offset*, so a writer freeing a block it did not allocate performs **no
   ownership lookup, no message and no record**. `begin_free` /
   `finish_free` are **untouched by this change** — that is the proof, not a
   claim: the free path contains no lane probe, and the bench group's
   `free_lane_of_4` ≡ `free_unpartitioned` prediction is the falsifier.

**Reuse obeys the same law**: a free block in lane `w` is re-allocatable
only by lane `w` (`try_allocate_block`'s candidate filter,
`allocate_block_below`'s and `allocate_block_at_or_above`'s too). That is
what makes reuse arbitration-free as well, and it is the source of the
stranding bound (§5) — the honest cost of the whole design.

### 2.1 Solo is the shipped path, not a special case of it

`writers == 1` means lane 0 owns every index at stride 1. Rather than argue
that this is *equivalent* to today's allocator, **a solo engagement installs
nothing at all** (`BlockAllocator::engage_alloc_lanes` returns `Ok(())`
without setting the `OnceLock`), so:

* no laned code path can differ from the shipped one, because none is
  reachable;
* the cost of the feature on a single-writer mount is one `OnceLock` probe on
  the allocation path and **zero** on the free path;
* the property is a **test**, not a comment:
  `single_writer_is_byte_identical_and_costs_nothing` asserts the same
  allocation sequence as a never-engaged allocator, the same contiguity
  picks, zero durable commits, and not one `alloc_lane_*` gauge moved.

The arithmetic tie is pinned separately (`solo_ties_the_shipped_arithmetic`):
at `W = 1` the lane is always 0, the in-lane rounding is the identity, and
the mint step is the floor itself — i.e. literally the shipped
`highest_block` CAS loop.

### 2.2 Why `LaneCursor` is NOT reused for the mint

This is the one place the design departs from the ino-lane sibling, and the
departure is deliberate:

1. a block mint must be able to **refuse at device capacity without
   advancing the cursor** (`next_fresh_block`'s CAS loop exists for exactly
   that: *"a refused racer must not bump the cursor"*), and `LaneCursor`'s
   `fetch_add` cannot un-mint;
2. after a lane **adoption** (§4) a writer mints in **several** residue
   classes at once, which one strided cursor cannot express.

So the mint stays the existing CAS loop with a lane STEP
(`next_owned_index_at_or_above`, ≤ `W − 1` mask tests), and the cursor keeps
publishing the **dense** frontier — the indices it skips belong to peers and
are neither minted nor free-listed here. The lane *arithmetic* is
`lane_core`'s, verbatim.

---

## 3. Durability: one record per (volume, lane), written ahead of use

### 3.1 Why derived state is not enough

The free list and the cursor are **derived**: the free list is the complement
of the durably-referenced set below the cursor
(`seed_from_durable_refs` / `recover_active_blocks_v3`). For a *lone* writer
that is sufficient, and re-minting an allocated-but-unpublished offset after
a crash is correct — nothing references it.

It fails when a lane **changes hands while a peer is still alive**. A
fenced-but-live predecessor (a zombie) may hold lane-`w` offsets it minted,
is DMA-ing into, and never published. The durable references do not mention
them, so a successor's derived floor sits *below* them and hands them out
again — to a second owner, while the zombie is still writing. S7's
quarantine cannot help: it is keyed on offsets someone *declared*, and these
were never declared to anyone.

### 3.2 The record

```text
name    alloc_lane:{vol_tag:016x}:{lane:04x}      xattr on ino 1
value   version(1) | writers(2) | lane(2) | reserved_upto(8) | xxh3(8)   [21 B]
meaning this lane will never mint an index at or above `reserved_upto`
```

Five deliberate choices:

* **on ino 1, as an internal xattr** — KD-2's plane, the `job:` /
  `writer_claim` precedent: whole-transaction atomic, torn-write immune,
  offline-probe readable, and **invisible through FUSE** because the VAL-2
  xattr screen is an ALLOWLIST (`user.*` / `security.*` / `trusted.*`), so a
  new internal name needs no denylist edit to be unreachable from a shell
  (pinned);
* **`vol_tag` is `block_refs::volume_tag`** — the SAME durable `vol-{hex}`
  identity `TREE_BLOCK_REFS` keys on (KD-5), never a path, an ordinal or a
  set position, so a volume removed and re-added cannot inherit a stranger's
  watermark;
* **keyed on the LANE, never on writer identity.** This is what makes
  adoption (§4) free of any new durable structure: the frontier is a *dense*
  index bound and a raise declares it for **every lane this mount owns**, so
  a future holder of an adopted lane also recovers above the indices we
  minted in it. One commit per grain on the shipped shape (a mount owns one
  lane); an adopting mount pays one per adopted lane, which is the price of
  reaching a dead writer's space. An adopted lane's frontier starts from our
  own dense cursor rather than from the dead holder's record, so that
  record can be written backwards — sound precisely because adoption demands
  a proof of death (nothing the dead holder minted can still be written), and
  the reason a LIVE peer's lane is never adoptable;
* **versioned and checksummed** even though it rides a whole-tx-atomic
  record: a torn or bit-flipped watermark that decoded *silently* would place
  the floor BELOW a live peer's offsets, which is the one failure the record
  exists to prevent. An unknown version refuses **loud** (forward-only);
* **written AHEAD of use, and the offset is handed out only after the write
  commits** (`hand_out_reserved`). If the commit fails the offset is given
  **back** — nothing durable and no device byte has touched it, so
  `free_block`'s begin+finish-with-nothing-between contract is exactly right.

### 3.3 The recovery rule and its crash windows

```text
floor = next_in_lane( max( derived_dense_floor,                  // published blocks
                           own-lane reservation,                 // minted-but-unpublished
                           every record whose width ≠ ours ) )   // lane identity changed
```

Each clause is load-bearing, and the third is the width-change case: under a
different `W` the index `b` belonged to a different lane, so every such
watermark must dominate **every** lane. A foreign lane's record at OUR width
is deliberately *not* a floor — those indices are not ours to mint.

Rounding up burns the skipped indices; they are recovered as ordinary
free-list gaps by whichever lane the current width assigns them, so nothing
is stranded permanently (the §4.8 ino law, one plane over: *"crash-skipped
ranges waste nothing that matters"*).

**The crash windows, enumerated:**

| Window | Outcome |
|---|---|
| crash between the reservation commit and the mints it covers | the successor's floor is the reservation, so it starts above indices nobody used. The gap becomes free-list supply for that lane — bounded by the grain, reachable, never lost |
| crash after minting, before publishing (the zombie case) | the mints are **below** the reservation, so the successor never re-mints them. This is the window derived state cannot see, and the only reason the record exists |
| crash after publishing | covered by the derived floor, exactly as before this change |
| crash mid-`setxattr` (torn record) | v3 metadata is whole-tx atomic and torn-immune, and the record's own checksum is the second line: a partial value refuses loud rather than reporting a lower frontier |
| width change (`W` → `W'`) across mounts | every recorded watermark floors every lane (clause 3), so no lane can mint an index a differently-laned predecessor used |
| **a mount with NO durable sink** (offline tools, unit fixtures) | the frontier is RAM-only and recovery falls back to the derived floor — i.e. exactly the pre-partition posture. Stated rather than pretended: such a mount is single-writer by construction |

### 3.4 Amortization

One commit per `reserve_grain_blocks` **fresh** blocks per lane;
**reuse pays nothing**, because a freed index is already dominated by the
recovered floor. So the rewrite-heavy regime — the one that allocates from
the free list at the block rate — adds **zero** commits, and streaming
ingest adds one per grain.

The grain **derives** (the standing derivation law — no free-floating
constants): `FLOOR_BLOCKS_PER_LANE × HEADROOM × cpus` (the write pipeline's
own cold-window inputs), floored at the eight-lane cold aggregate so a cold
streaming start never pays two commits inside one pipeline window, and capped
at 1/64 of a lane share because the grain is *also* the temporarily
unavailable window after a lane dies with a live peer.
`SQUEEZEFS_ALLOC_LANE_RESERVE_BLOCKS` overrides verbatim (registered per
ENG-10; `1` is the pathological A/B control). Tie test:
`the_reservation_grain_derives_and_the_knob_wins_verbatim`.

---

## 4. ENOSPC and fairness

A writer that cannot allocate while the set has space is worse than an
unfair distribution, so the ladder is explicit:

1. **this lane's free list**;
2. **this lane's virgin share**;
3. **a lane ADOPTED because its holder is proven dead** — the witness is
   S7's `DeadEpoch`, i.e. the *same drain proof* `release_quarantine`
   demands: a landed WERO preempt of the dead holder's registrant key on a PR
   substrate, an attested proof of death otherwise. Adoption adds the lane to
   a bitmask; its free blocks and its virgin share become mintable here;
4. **refuse `StorageFull` — loudly, naming how many free blocks belong to
   lanes this mount cannot reach** — and count
   `alloc_lane_enospc_refusals` (a must-stay-0 tripwire) with
   `alloc_lane_stranded_bytes` published as the standing bound.

**A live peer's lane is never stolen.** Stealing from a lane whose holder is
alive requires arbitration this layer does not perform, and the stolen offset
could collide with that peer's own reuse of it — the exact failure the
partition exists to prevent. "Proven dead" is the only witness that makes a
steal structurally safe, and it is a witness the system already produces.

The refusal deliberately keeps the `StorageFull` error class, so the ENOSPC
pressure valve, the reclaim drain-and-retry ladder and every caller's
handling are unchanged.

---

## 5. The stranded-capacity bound

```text
lane share          = ⌈(capacity_blocks − w) / W⌉         shares differ by ≤ 1 block
Σ lane shares       = capacity_blocks                     (the shares tile the device)
granularity cost    ≤ W − 1 blocks, set-wide
reachability bound  = capacity_blocks − Σ(owned lane shares)
                    ≤ capacity_blocks × (W − owned) / W + (W − 1)
```

Both readings matter and both are in `docs/operations.md`:

* **if every writer stays inside its share, the partition costs `W − 1`
  blocks** of usable capacity (12 MiB at 4 writers on the shipped 4 MiB
  block). That is the whole price;
* **a writer needing more than `capacity/W` is refused while free space
  exists elsewhere.** That is the reachability bound; it is published live as
  `alloc_lane_stranded_bytes` (summed across the mount's data volumes, and
  reduced exactly by each adoption), and it is why multi-writer capacity
  planning is per-writer rather than set-wide.

Pinned by `the_stranded_capacity_bound_is_the_published_formula` across five
widths and six capacities, including the tiling identity and the ≤ 1-block
share spread.

**The width rounds up to a power of two, and that is the third cost.**
`AppendPartition` — the ONE appender descriptor the partitioned journal,
extent bitmap, root ledger and ino lanes all run on — admits only power-of-two
widths (an uneven split would leave some appender fewer than the two root-ledger
slots the fallback property needs). So three writers run at `W = 4` and a
quarter of every data volume belongs to a lane nobody holds: unreachable
exactly like a live peer's lane, counted in `alloc_lane_stranded_bytes`, and
recovered in full the moment a fourth writer is enrolled in a new era. Plan
capacity on the ROUNDED width, not the writer count. Sharing the descriptor is
deliberate: a volume must never hold two disagreeing notions of "who is
writer 2".

**Fragmentation is the second cost, and it is not free.** Contiguity-aware
allocation keeps working **within** a lane (the VL4 `move_one` mover, VL7's
D1/D2 axes, and the W1 in-place patch predicate are all unchanged in kind),
but a run of blocks one writer owns is `W`-strided rather than dense, so
`frag_d1_contiguity` reads lower on a partitioned volume **by construction**.
Compare a partitioned mount against other partitioned mounts, never against a
single-writer baseline.

---

## 6. Composition with every landed invariant

| Invariant | How it still holds |
|---|---|
| **the lifetime-stamp funnel** (§6.2 item 6): every allocation returns through `claim_block_idx`, which mints the offset's lifetime | No new admission path was added. The lane logic changes *which index* the funnel is called with, never *whether*: the free-list arm, the fresh arm, `allocate_block_below` and `allocate_block_at_or_above` all still end in `claim_block_idx`. Pinned by `every_laned_allocation_carries_a_lifetime_stamp` — an offset that started a lifetime with no stamp reads `unknown` forever and silently re-opens §6.3 |
| **S7's dead-epoch quarantine**: admission claims the offset out of the free list; release needs a drain proof; the pressure answer is ENOSPC | Untouched. Quarantine admission removes the index from the free list, so the lane filter never sees it; `finish_free` still defers the publish; `release_quarantine` is still the only publisher. Pinned by `the_quarantine_still_gates_a_laned_allocation` |
| **the reclaim queue**: pop-ownership exactly-once, offsets non-reallocatable until reclaimed, the ENOSPC valve, park-don't-spill, the D0 fence latch | Untouched — the partition adds nothing to the free path and nothing to `block_reclaim`. The `begin_free → reclaim → finish_free` window is the same window (pinned by `the_reclaim_window_invariant_survives_the_partition`), and the ENOSPC valve still runs on `StorageFull` because the lane refusal keeps that class |
| **durable block refcounts** (bit 9, `TREE_BLOCK_REFS`, fsck class C8 must-stay-0) | Untouched: a reference is to a **block**, not to a lane, and the record key `(vol_tag, block_idx, owner_ino, block_index)` needs no lane component. The ledger is what makes the derived floor correct for published blocks, which is clause 1 of the recovery rule |
| **the fsck allocation-epoch side map + in-flight registry** | Untouched (both key on offsets). What DID change is C6 **reconciliation**: it now skips foreign-lane indices, because under a partition "untracked and not free-listed" is the NORMAL state of a peer's live block, and completing its free would publish another writer's block into this one's free list. Pinned by `the_fsck_reconcile_never_completes_a_foreign_lanes_free` |
| **contiguity-aware picks** (VL4 evacuation, VL7 D1/D2, W1) | Work within a lane, one stride coarser (§5's fragmentation note). The synchronous ascending pick gains one refusal: it cannot `await` a reservation raise, so a *fresh* mint past the frontier is refused loud and the mover defers — free-list picks are unaffected |
| **`virgin_bytes`** (the KD-4.6 discard watermark's input) | Scaled by the owned-lane count. Reporting the dense tail would tell the watermark there is `W`× more virgin supply than this writer can reach — the same class of lie the stranding gauge exists to prevent |
| **`get_used_blocks` / `df`** | Deliberately unchanged and set-wide: on a shared device `df` must report the DEVICE's usage, not one lane's. The free list stays the SET's free supply (dense), allocation is what filters, and the reachability delta is `alloc_lane_stranded_bytes` |
| **`allocate_specific_block`** | Deliberately lane-blind: the caller names an index it already owns durably (a clone/recovery path), which is not a fresh mint and needs no residue class |
| **DLM S5's reader gate** | Untouched: every mutating arm still routes through `reader_gate` first, and a reader engages no partition (it allocates nothing) |

---

## 7. Compatibility, and the bit this does NOT take

**No new incompat bit.** The gate is **bit 11**
(`FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA`), whose documented meaning already
is *"the volume set's format is expressed for more than one data-plane
writer"* — which the `alloc_lane:` reservation records are — and which S9's
arm already requires. This program has had **five** parallel bit claims
(8/9, then 10/11, then 12/13, then 14); not taking a sixth is the safest
available answer, and the disjointness pins in
`tests/writer_scoped_staging_tests.rs` (whose union clause asserts
`FEATURES_INCOMPAT_KNOWN` is exactly the enumerated set),
`tests/dlm_data_fence_tests.rs` and `tests/dlm_membership_tests.rs` are
therefore untouched by this landing.

| Volume | This binary | An older binary |
|---|---|---|
| **fresh format** (bit 11 unset — `SuperblockV3::plan` does not set it, ruling D9) | unpartitioned: `lanes` is `None`, every path is the shipped one | mounts exactly as before |
| **bit 11 stamped** (Phase 8, `set_multi_writer_data_bit`) + a solo mount | still unpartitioned (a solo partition installs nothing) | refuses loud — right: it would mint dense offsets across every peer's lane and ignore the reservation frontier a live peer published |
| **bit 11 stamped** + a mount that installed a non-solo partition | partitioned: lane-filtered allocation, lane-blind frees, durable per-lane reservations | refuses loud |
| **read-only mount** | allocates nothing; the reader gate refuses before any lane logic | unchanged |

Engagement is per-**set**: `DataRouter::set_meta_backend` engages only when
EVERY mounted meta volume carries bit 11 (a mixed set would have one volume's
recovery expressed for N writers and another's not), and only when
`data_alloc_lane::mount_partition()` is non-solo.

---

## 8. Cost — predicted, NOT measured (ruling D11)

The bench coverage is `benches/write_path_bench.rs::alloc_lane` (five A/B
pairs plus the record codec), written and **un-run**, carrying its
field-derived shapes in-file: the write-wall campaign's ~1,650 blocks/s
displaced at 6.3–6.8 GB/s (allocation is once per 4 MiB, never per op), a
rewrite regime's thousands-deep free list, and W1's 61–67 k IOPS patch rate —
which allocates nothing.

| Claim | Structural reason | Falsified by |
|---|---|---|
| the unpartitioned mint is unchanged | one `OnceLock` probe before the shipped CAS loop | `mint_fresh_unpartitioned` past its group threshold vs the committed reference |
| the laned mint costs a CONSTANT more | the lane step tests ≤ `W − 1` mask bits (`W ≤ 16`), then runs the same CAS | `mint_fresh_lane_of_4` scaling with anything but `W` |
| **the free path is byte-identical** | it contains no lane probe at all | ANY separation between `free_lane_of_4` and `free_unpartitioned` — which would refute the design's premise, not merely its cost |
| reuse under a partition costs ≈ `W`× the free-list scan | only 1/W of candidates qualify | worse than ≈ `W`× (the filter re-scans) or no different (it is not running). A genuine `W`× at field occupancy is what would justify a per-lane free list, which this design deliberately did **not** build |
| the reservation is amortized to ≤ 1 commit per grain, and 0 for reuse | free-list allocations never reserve | `alloc_lane_reservations` ÷ fresh blocks exceeding 1/grain (deterministic, so reportable even under D11 — and pinned as a test) |

Two cost facts are **structural** and hold regardless: `LanePartition` is one
allocation per volume (four words), and the durable footprint is one 21-byte
record per (volume, lane).

---

## 9a. The admission: the lane grant (landed — `src/alloc_lane_grant.rs`)

§9 item 1 below is **closed**. This section is the assignment story; contracts
`tests/mw_cowriter_lane_tests.rs`, operator surface `docs/operations.md`
§Multi-writer co-writer mounts.

### 9a.1 Who assigns, from what

The **authority**, from the §6.2 item 7 durable `claim_set` record — the same
record a co-writer's admission rung 3 already stands on, and a record only an
authority can write:

* it keeps **lane 0**;
* each enrolled WRITER member takes the next lane, in the record's own
  sorted-by-id order (`upsert_writer_member` keeps it sorted, so the order is a
  property of the record and not of who read it). Readers take no lane — they
  mint nothing;
* `writers` = (1 + enrolled writer members) **rounded up to a power of two**,
  because that is what `AppendPartition` admits (`MAX_APPENDERS` = 16 and the
  32-slot root ledger must split evenly). The rounding is an honest capacity
  cost: with three writers the width is 4, so a quarter of every data volume
  belongs to a lane nobody holds — unreachable exactly like a live peer's lane,
  published as `alloc_lane_stranded_bytes`, and stated in §5's terms.

The map is **injective** by construction (an index into a deduplicated sorted
list), which is the whole point: a knob would let two co-writers claim one
lane.

### 9a.2 Why the LEASE carries it, and the co-writer does not derive it

A co-writer can read the same record. It must not derive from it, and the
reason is a race rather than trust: **the roster grows.** With `W = 2` a live
co-writer mints `b % 2 == 1`; a node that read the record after a later
enrollment would derive `W = 3` and mint `b % 3 == 2` — and index 5 is in both
classes. So the authority's **arm-time snapshot** is the single source,
distributed on `LeaseFrame.{writer_lane, writers}`, and:

* a member the snapshot does not name is **refused at the custody join**,
  naming the remedy;
* **a width change is an act of a new authority ERA**, never of a live set —
  which is exactly the width-change case §3.3's clause 3 already handles
  across mounts (a record at a foreign width floors every lane, so no lane
  can mint an index a differently-laned predecessor used);
* a **renewal** that ever answered a different lane is a FAULT: the mount
  self-fences (§6.7's stricter client clock) rather than adopting a residue
  class its already-minted offsets do not belong to.

`(0, 1)` on the lease means SOLO — no partition at all — which is what an
authority with no enrolled co-writer answers, and it engages nothing.

### 9a.3 The durable reservation a co-writer cannot commit

§3's record is an xattr commit on ino 1 — **metadata** — and a co-writer holds
no metadata authority; that is its definition. The record is also not optional:
it is the only thing that stops a successor of a lane from re-minting the
unpublished tail of a crashed predecessor (§3.1).

So the raise **ships**, over S9's landed publish vocabulary
(`PublishCall::RaiseAllocLane`), and the AUTHORITY commits it. Three
properties make that safe, and all three live on the authority:

1. **the client names a LANE, never a record.** The owner derives the record
   name from `(vol_tag, lane)` itself, so the verb cannot address
   `writer_claim`, `claim_set`, `job:` or any other internal name;
2. **the lane is checked against the assignment the owner made**
   (`WriteCustodyOwner::check_lane_raise`), under the lease epoch the owner
   minted for that member — monotone, never reused, handed to nobody else;
3. **the frontier only ever rises.** Monotonicity is the owner's, so no caller
   can ask for a LOWER frontier — the one shape that would let a successor
   re-mint a live peer's offsets.

The offset is handed out only after that commit lands, because the raise runs
inside the landed `hand_out_reserved` (§3.2's last bullet, unchanged).

**Why authority pre-reservation loses.** Raising each member's frontier as part
of granting or renewing custody needs no new verb, and was rejected on three
counts: it puts a durable metadata commit on the **heartbeat** — the exact
plane §6.5 item 3 measured at 455 journal beats/s and S6 exists to keep off the
journal; it makes reservations proportional to **time** rather than to fresh
blocks, discarding §3.4's amortization (the property that keeps the rewrite hot
path at zero commits); and it must either over-reserve blindly on N volumes × M
members or **stall** a streaming writer at a renewal boundary. Shipping the
raise keeps the grain exactly as §3.4 defines it: one round trip per grain of
fresh blocks, none for reuse.

**Why not S8's generic `setxattr` verb.** It would let a co-writer write ANY
value to the record (a lower frontier, i.e. §3.1's failure) and any internal
name on ino 1. The record's monotonicity and its lane ownership have to be
enforced by the node that owns the metadata, which is what a purpose-built verb
does and a generic write cannot.

### 9a.4 The OPEN: the floor a mount that never walks cannot compute

A co-writer runs **no** ownership-recovery walk and **no** durable-reference
seed (the walk's free-completing arm must never run on a snapshot view), so its
cursor starts at 0 — and a lane alone would have it minting `w, w + W, …` from
the bottom of a device whose low blocks are LIVE. The frontier record is a
*ceiling* on its own minting, not a floor on history, so it does not answer
this.

The lane is therefore **opened** before any mint: a raise with `upto == 0`
commits nothing and answers §3.3's floor, computed on the authority from state
only it has — the durable reference ledger's dense frontier
(`durable_dense_frontier`), its own live cursor for that data volume (the
installed `DenseFrontierSource`), and every `alloc_lane:` record the volume
carries, composed through the SAME `recover_lane_floor`. `engage_allocator_lane`
wires the sink and runs the open in that order, so *"a laned mount opened its
lane"* is structural rather than a convention.

### 9a.5 What the co-writer's allocation shape is, exactly

* it allocates **fresh** blocks from its lane's virgin share. Its free list is
  empty by construction (no seed, no walk, and its frees execute on the
  authority), so `try_allocate_block` always falls to the fresh mint;
* every **free**'s ladder is EXECUTED by the authority — since the S9
  co-writer free path (`tests/mw_cowriter_free_tests.rs`) the router-level
  displaced free SHIPS as `PublishCall::FreeBlocks` and the authority runs
  `begin_free → reclaim → finish_free` itself, with the freed offset entering
  ITS free list in whatever lane `b % W` names (frees stay lane-blind, this
  design's own law — the shipped free just runs the arithmetic on the node
  whose ledger and reclaimer are live). The allocator-level arms
  (`begin_free`, `free_block`, `allocate_specific_block`, the W1 incarnation
  retire, the recovery walk) still refuse locally on a co-writer
  (`cowriter.accounting_refusals` counts exactly those), so a rewrite
  displaces through the router and never through them;
* a raise that cannot be made durable **gives the offset back** — and on a
  co-writer that give-back is itself refused (a free), so the index is skipped
  rather than returned. Bounded by raise failures, which are loud
  (`alloc_lane_raise_refusals`) and covered by our own frontier, so the index
  is never handed to a peer.

### 9a.6 What 9a does NOT close

* **lane-scoped lifetime stamps.** `set_meta_backend` still engages §6.2 item
  6's incarnation minter with `AppendPartition::SOLO` (it runs before the lease
  exists), so two writers mint stamp values from one sequence space. Harmless
  while lanes are disjoint (a stamp only ever pairs with an offset in its own
  lane), and the exposure is a lane **ADOPTION**: an adopter could re-mint a
  stamp value the dead holder used for the same offset. Nothing in the grant
  adopts a lane, and stamps only become durable under incompat bit 13, which
  nothing stamps (D9). The fix is to pass the granted lane to
  `engage_incarnations`, which needs the lane before `set_meta_backend` — i.e.
  the same ordering the OPEN sidesteps;
* **the in-flight declaration.** A co-writer does not yet call
  `declare_inflight` with its freshly minted offsets, so S7's quarantine covers
  only what it declares. The reservation record is the stronger protection for
  unpublished mints (it stops re-mints outright), which is why this is a gap in
  hygiene rather than in safety;
* **two hosts.** See §9 items 2–5 and the operator page: nothing stamps the
  capability bits, and the D0/PR halves need real hardware.

---

## 9. What this does NOT do — the residuals S9+ owes

1. ~~**The lane assignment.**~~ **Closed** by §9a
   (`src/alloc_lane_grant.rs`): the authority derives one lane per enrolled
   writer from the durable claim set and carries `(writer_lane, writers)` on
   `LeaseFrame`, exactly as this item predicted — plus the two things it did
   not anticipate, which §9a.3 and §9a.4 are: a co-writer cannot commit its own
   reservation record (it ships, and the authority commits it), and a mount
   that never walks the tree cannot compute its own floor (the authority opens
   it).
2. **A width change while writers are live.** The recovery rule handles a
   width change **across mounts** (clause 3). Changing `W` on a live set
   would need every writer to stop minting, publish, and re-derive — a
   coordinated act the membership plane could carry, and which nothing here
   attempts. §9a.2's rule is the shipped answer: a width change is a new
   authority ERA, and a member enrolled mid-era is refused at the join.
3. **Per-lane free lists.** The candidate filter is a scan over the shared
   free list, predicted at ≈ `W`× a dense scan. If measurement shows that
   matters at field occupancy, the answer is a per-lane index, not a change
   to the law.
4. **Cross-writer fsck.** C6 now declines to reconcile foreign lanes, which
   is the leak-safe direction. A partitioned set's *complete* reconciliation
   needs every writer's view — the same shared-authority gap §6.8 item 3 and
   the freed-offset grace period name.
5. **The §6.8 item-3 freed-offset grace period** is a sibling branch and
   composes at the offset level (this partition adds nothing to the free
   path, which is where that work lives).
