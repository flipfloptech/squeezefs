# Design: The Rewrite Program — Rev 1 (P0 foundation)

**Status:** Rev 1 — P0 design + implementation record (branch
`feat/rewrite-program-p0`).
**Charter (company engineering leadership, 2026-08-02, verbatim):**
> "Rewrite program: sequential overwrite of existing striped data must
> match fresh ingest device-byte rate (±5%) and pay zero discards during
> the row; loop-rewrite must be latest-wins (device writes ≈ unique
> blocks, not ops). Vehicles: shadow dual-map + data-plane supersession +
> discard elision / substrate-probed inplace. Durability is a named mount
> class, not an accident of CoW."

**P0/P1 scope (this campaign, landed in this order):** Idea 17
(`rewrite_amp` SLO — built FIRST, it is the acceptance instrument),
Idea 4 (discard elision until pressure), Idea 2 (latest-wins coalesce,
the safe-today subset), Idea 1 (shadow dual-map). Ideas 6/7/8 get design
sections (§9) with implementation deferred to the next campaign.
**Explicitly out of scope** (leadership do-not-staff list): reclaim lane
width, inplace-default-on-zram, publish aggregation.

## 1. Evidence lineage (what this program stands on)

The rewrite tax has been peeled in five prior campaigns; every layer
below is landed machinery this design composes with, never replaces:

| Layer | Fix | Evidence |
|---|---|---|
| Write-Zeroes on free (+1.0× amp) | `BLKDISCARD`/`PUNCH_HOLE` classification, never zeroing writes | `.benchmarks/2026-07-27-shim-write-amplification.md` |
| Synchronous discard on the write path (~235 µs/block fabric RTT) | background reclaim queue (`src/block_reclaim.rs`) | `.benchmarks/2026-07-27-async-block-reclaim.md` |
| Reclaim drain vs foreground; at-cap inline arm | manners law + park-don't-spill + demand-derived lanes | `.benchmarks/2026-07-31-write-wall.md` |
| Inline-awaited uploads (aqu-sz < 2) | write pipeline: ACK-parked custody + runtime-BDP admission (`src/write_pipeline.rs`) | `.benchmarks/2026-07-27-write-pipeline-depth.md` |
| Rewrite publish 7.7× (commit-rate-coupled queueing) | era-guarded RAM base (Lever A) + aggregated publish commits (Lever B) | `.benchmarks/2026-08-01-rewrite-publish-drain.md` |

**What remains, named by the instruments:** (a) the displaced-block
lifecycle itself — every rewritten block still pays CoW displacement:
one map-delta churn + one `free_block` enqueue (0.95–1.78 ms/block
`displaced_free` in-pipe under contention) + one eventual device discard
(at-cap drains put the discard stream back on the fabric DURING long
rewrite rows: the queue cap is 4096 blocks = 16 GiB at 4 MiB, and a
32 GiB row hits it mid-row); (b) rewrites and fresh ingest ride
different-shaped publish streams (rewrite = displace + free; fresh =
insert), so the rows diverge structurally even at publish parity;
(c) no coalescing exists for repeated rewrites of a hot block — device
writes ≈ ops always. The conserved-work verdict of the rewrite-publish-
drain campaign (§9 there) is the standing instruction: further client
drain surgery converts nothing — the remaining currency is **byte
amplitude** (write_amp 1.13–1.16 = displaced-block CoW + reclaim
discards riding the same fabric) and **work deletion** (frees, discards,
per-block commits that need not exist at all). That is exactly this
program.

## 2. The SLO (Idea 17) — built first, the acceptance instrument

### 2.1 Formula (KD-17.1)

Per measured row, on the DATA namespace(s):

```
rewrite_amp = device_write_bytes / user_overwrite_bytes
```

with **`discard_bytes_during_row`** (and `d_ops`) surfaced alongside on
every row — a rewrite row that hides its discards in a sibling window is
INVALID (the settle-hygiene rule from the rewrite-publish-drain venue
applies: rows start with `block_free_reclaim_queue_bytes = 0` and the
elision debt gauge noted).

* `device_write_bytes` = per-row `/proc/diskstats` write-sector delta on
  the data namespace(s) (meta rides its own namespace — the data delta
  is exact; the standing write-amplification instrument, verbatim).
* `user_overwrite_bytes` = the instrument's written bytes for the row
  (elbencho/fio user bytes).
* Daemon-side attribution (stats inode, new counters §8): the row's
  device bytes must reconcile against `rewrite_device_write_bytes` +
  fresh-class writes; its user bytes against `rewrite_user_bytes`. A row
  whose deltas do not account for its traffic is INVALID (charter rule 4
  posture).

### 2.2 Gate values (KD-17.2, from the charter)

| Row | Gate | Class |
|---|---|---|
| `seq_overwrite` (full overwrite of an existing striped fileset) | `rewrite_amp ≤ 1.05` AND `d_ops == 0` during the row | target classes: bdev backing, elision on (default), passthrough |
| `seq_overwrite` vs `fresh_ingest` | device-byte rate within ±5 % (A-B-B-A, both orders — aging store) | same |
| `loop_rewrite` (time-based loop over a hot set) | latest-wins: device writes ≈ unique blocks, not ops | **two faces** — see below |

**The loop-rewrite gate has two faces** (KD-17.3, stated honestly):

1. **Overlapping rewrites** (re-dirty while a prior upload of the same
   block is still unpublished — deep-qd hot sets, kernel-split segments,
   racing writers): gated NOW by Idea 2's supersession — a superseded
   in-flight upload publishes nothing and the device pays once per
   surviving image. Engagement instrument: `write_pipeline_supersessions`.
2. **Non-overlapping loops** (file-scale passes where each block's
   rewrite arrives after its previous upload published): device writes ≈
   unique blocks **per flush cadence** requires retaining dirty custody
   across the gap — a durability-*cadence* question that belongs to the
   named durability classes (Idea 8, `data=writeback`). Under today's
   default class the P0 instrument MEASURES AND REPORTS the coalesce
   factor (`ops·bytes ÷ device bytes`) on this face; the ≈-unique-blocks
   gate arms when Idea 8 lands. (This is the one keyed decision flagged
   for orchestrator review — §10.)

### 2.3 Instrument implementation

* **`tests/write_amp_rig.sh` row extension:** two new rows —
  `seq_overwrite_1m` (untimed fileset prep through the kernel path, then
  the timed full overwrite; prints `REWRITE_AMP`, `d_ops`/`d_MiB` during
  the row, and the fresh-row comparison ratio) and `loop_rewrite`
  (time-based hot-set loop; prints device-writes ÷ unique-block-bytes
  and the supersession engagement delta). Both rows carry the standing
  amplification columns and stats-inode deltas; substrate law applies
  (loop devsub is scoping-only; nvmet-tcp or the field fabric for
  acceptance).
* **Daemon counters** (§8): `rewrite_user_bytes`,
  `rewrite_device_write_bytes`, `rewrite_blocks` — counted where the
  write path *knows* it is displacing/replacing an existing mapping
  (the complete-block upload's merge-displacement observation, the
  in-place arms, and the shadow-epoch record), so the stats inode
  attributes any measured row without diskstats access.

## 3. Idea 4 — discard elision until pressure

### 3.1 The mechanism (KD-4.1, KD-4.2)

Charter vehicle: *elision = queue-without-issuing until pressure — the
queue machinery exists.* Resolved as: for **BdevDiscard-class backings
only** (block devices — NVMe DSM Deallocate), a terminal free skips the
reclaim queue entirely:

```
begin_free (retire incarnation)  →  read-tier purge  →  finish_free
(immediately reallocatable)      →  debt record (per-device, RAM)
```

This is the *sanctioned* zero-destructive-work window collapse —
`BlockAllocator::free_block` (begin + finish with nothing between) is an
existing, documented shape (error-unwind frees). Correctness never
depended on freed ranges being deallocated: unmapped blocks serve zeros
from hole semantics, reuse is guarded by write-before-publish + the
incarnation seqlock (the shim-write-amplification adjudication §3,
verbatim). What the discard buys is *substrate hygiene* (thin-pool space
return, FTL health) — an eventually-idle service, not a write-path one.

**FilePunch-class backings (regular-file volumes) keep the queued
reclaimer verbatim** — the punch is the host-FS space return whose
absence is a *real* ENOSPC vector on overcommitted host filesystems (the
`333ce23` lineage). No elision there.

### 3.2 Debt structure + claim-cancels-debt (KD-4.3)

Per-`BackendRouter` `DiscardDebt`: a lock-free map
`device_path → scc::HashMap<offset, bytes>` + a byte gauge
(`block_free_elided_debt_bytes`). Bounded by construction: debt ⊆ the
free list, worst case `capacity/chunk` entries (1 TiB / 4 MiB ≈ 262 k ×
~32 B ≈ 8 MiB — RAM-trivial and it only shrinks under reuse).

* **Allocation cancels debt:** `claim_block_idx` removes the offset's
  debt entry (one lock-free probe on a mostly-empty map when elision is
  idle). A reused offset owes no discard — its new owner's DMA rewrites
  the range.
* **The free list is the durable truth; debt is only the incremental
  tracker.** Debt is RAM-only. Lost debt (kill-9, unmount) is
  un-returned thin space, re-covered on reuse or by the next mount's
  trim venue — a *full* trim discards every free-listed range without
  needing debt state (the free list is rebuilt from durable maps at
  mount).

### 3.3 Trim protocol — no discard ever races an owner (KD-4.4)

A trim pass claims each offset **out of the free list** (the same
`DashSet::remove` atomic claim allocation uses), issues the discard,
then re-inserts it. An offset is never simultaneously allocatable and
being-discarded; a lost claim (racing allocation) just drops the debt
entry. This preserves the begin_free→reclaim→finish_free law's *intent*
(reclaim strictly happens-before any new owner's DMA) with the claim as
the ownership token instead of the free window.

### 3.4 Venues + the watermark (KD-4.5, KD-4.6 — no constants)

| Venue | Behavior |
|---|---|
| **During a row** (foreground device I/O active, debt below watermark) | **elide entirely — zero device discard commands** (the charter row gate). The manners-law foreground probe (`device_activity_signal`) is the signal, reused verbatim. |
| **Pressure** (debt above watermark) | paced drain engages regardless of foreground (single-lane, manners-paced — the at-cap analogue), counted `block_free_debt_pressure_drains`. |
| **Idle** | drain debt to zero (the idle catch-up posture — idle target CPU is free, measured 2,700–5,900 cmd/s in the write-wall width experiments). |
| **fstrim / defrag** | `BackendRouter::trim_elided(full: bool)` — the operator face; `full = true` claims-and-discards the entire free list (post-unmount hygiene recovery). Wired into the defrag data pass (the job-fabric venue); a dedicated CLI verb can front it later without design change. |
| **Unmount** | debt intentionally NOT drained (space return is not a teardown obligation; the next trim venue covers it — documented posture). |

**Watermark derivation (no constants):**

```
elide while  debt_bytes(dev) ≤ virgin_bytes(dev)
where        virgin_bytes = (capacity_blocks − highest_block) × chunk
```

While a device retains at least as much never-minted (virgin) capacity
as its unreturned debt, the substrate's physical exposure from elision
is bounded by what fresh-minting the same workload would have consumed
anyway — the thin pool is no worse off than if we had never reused a
block. Once debt exceeds the virgin tail, the store is running
predominantly on reuse and unreturned debt is the dominant thin
exposure: the paced drain engages. Self-scaling with capacity and fill;
zero tunables. (Unbounded-capacity allocators — offline tools/tests —
have an infinite virgin tail: pressure never fires; idle/trim venues
still drain.)

### 3.5 Ledger + lever (KD-4.7)

* Identity extension: `block_free_reclaim_queued + block_free_reclaim_elided
  ≡ terminal frees` (the field-ledger law, both arms honest).
* New counters §8: `block_free_reclaim_elided`,
  `block_free_elided_debt_bytes` (gauge, returns toward 0 under
  trim/reuse), `block_free_trim_discards` / `block_free_trim_bytes`
  (per-block/byte, the `block_free_discards` twins for the trim venue),
  `block_free_debt_pressure_drains`.
* **`SQUEEZEFS_DISCARD_ELISION=0`** restores the queued-reclaim path
  verbatim — the A/B measurement lever AND the operational escape for
  substrates that require eager space return. Default ON (bdev class).
* A **fenced** daemon never trims (`fence_halted` latch, same law as the
  reclaimer): destructive device commands cease permanently.

### 3.6 Thin-provisioning documented class (KD-4.8) + substrate posture

This extends the generic/213 adjudication: SqueezeFS is thin-by-design,
and with elision the space-return contract is **"freed space returns to
thin substrates at trim venues (idle / pressure / fstrim / defrag), not
synchronously with the free"** — the ext4/xfs periodic-fstrim posture,
stated in docs/operations.md. zram-vs-SSD note: on zram targets a
discard frees compressed memory but rewrite reuse replaces the slot
anyway (slot-replace ≈ 2× fresh cost — the measured
`SQUEEZEFS_INPLACE_OVERWRITE` verdict); elision-by-default is correct on
both, and the future substrate probe (Idea 6, §9.1) can retune the
*pacing*, never the correctness.

## 4. Idea 2 — latest-wins coalesce (the safe-today subset)

### 4.1 The durability line (KD-2.1 — the charter's explicit statement)

**Durability class changes ONLY under the future `data=writeback` class
(Idea 8).** Under today's default class:

* **Safe today — implemented:** superseding an *unpublished in-flight*
  upload (its bytes have not been named by any durable map — dropping
  the stale image loses nothing the contract ever promised), and
  **retained dirty authority** (the parked `ActiveBlockBuf` remains the
  readable RAM authority across supersessions instead of being retired
  by a stale completion; it is already R5-gauged custody —
  `parked_full_buffer_bytes` — and already sheddable-to-device via the
  existing parked-buffer flush machinery).
* **Not safe today — deferred to Idea 8:** delaying an upload *launch*
  beyond the current eager cadence (idle-window deferral). The ACK
  itself has been durability-decoupled since the depth campaign (custody
  parks, durability owed at fsync/close — O_DIRECT included), so
  deferral would not change the *contract*, but it changes the de-facto
  crash-exposure cadence of ACKed bytes — exactly what a *named* class
  must own, not an accident of an optimization (the charter's last
  sentence). Forced-device triggers for that future arm are already
  fixed by this design: fsync/close (existing flush), R5 Red (shed), and
  an idle timer (Idea 8).

### 4.2 The supersession mechanism (KD-2.2)

Today `write_through_complete_block` holds `BLOCK_FLUSH_LOCKS(ino, b)`
across the whole upload (crypto → allocate → DMA → merge), so a rewrite
of an in-DMA block *waits out the fabric RTT* and then pays its own full
upload — device writes ≡ ops, serialized. The pipeline path (the ACK
path's detached task) is restructured:

```
lock ─ snapshot buffer + capture (write_epoch, prune_epoch) ─ unlock
     ─ crypto → allocate → DMA → publish_block   (UNLOCKED window)
lock ─ revalidate: entry present ∧ write_epoch unchanged
     ├─ valid      ⇒ merge/publish + retire overlay (as today)
     └─ superseded ⇒ FREE the fresh offset (accounting-only unwind,
                     never published), publish nothing, LEAVE the entry
                     parked — the newer completion's task owns the single
                     durable publish. Counted write_pipeline_supersessions.
```

* **`write_epoch`** is a per-buffer monotone counter bumped by every
  coverage merge, **under the block lock** (all coverage mutations
  already run under `BLOCK_FLUSH_LOCKS` — the §5.3 law). Stamp and
  revalidation both run under the same lock ⇒ the supersession decision
  is **lock-serialized by design, not lock-free** — no fence protocol,
  no loom model owed (contrast §5.1's clone/patch fence, which races two
  words *without* a common lock). The deterministic schedule driver is a
  DMA stall seam (`SQUEEZEFS_TEST_UPLOAD_STALL_MS`), the
  `SQUEEZEFS_TEST_WRITE_STALL_MS` pattern.
* The unlocked window writes only to a **fresh, unpublished offset**
  (refcount 1, incarnation unstable-until-publish): invisible to clones,
  readers, and fills — no interaction with the §5.1 fence. The
  **in-place arms keep today's under-lock serialization verbatim**
  (their DMA mutates a LIVE mapped offset; the unlocked window never
  applies to them — KD-2.3).
* **Entry-gone at revalidation** (a flush leg/fsync published it first,
  or punch/truncate removed it): same skip-and-free arm — the durable
  state is owned by whoever holds the lock last; a stale completion can
  never publish over it. This subsumes the delayed-merge prune-epoch
  refusal (the merge's own `LAYOUT_PRUNE_EPOCHS` guard remains as the
  map-level belt).
* **The planted-stale-CQE contract:** stall upload A's DMA mid-flight,
  land a full-block rewrite (epoch bump), release A — assert A publishes
  nothing, frees its orphan offset (no leak: allocator accounting
  reconciles), the newer task publishes, reads observe only the newest
  bytes, `write_pipeline_supersessions == 1`.

### 4.3 CQE-supersession law (KD-2.4 — the FIND-M11-A language extension)

FIND-M11-A's law is *"a superseded writer era must not publish"* (fencing
generations, cross-mount). This design extends the same sentence one
level down, intra-mount: **a superseded DMA's completion must not
publish a stale generation** — where the generation is the block's
`write_epoch` under its lock. Both laws share the shape: validate
currency at publish time, under the authority that orders publishes
(DLM token there, block lock here); a stale completion resolves as a
contractual no-op that unwinds its own resources. Fencing composes
unchanged: a superseded task never reaches the merge, so the fence
surface (merge-time revalidation) is hit only by the current task —
`FenceDrop` semantics verbatim.

### 4.4 What this buys the rows

Overlapping rewrites (deep-qd hot sets; kernel-split segments re-covering
a block; racing writers) stop paying one device write per completion:
the stale in-flight image dies unpublished and its writer-side latency
disappears (the rewrite no longer serializes behind a prior image's
fabric RTT). Non-overlapping loop faces are Idea 8's (§2.2, §9.3).

## 5. Idea 1 — shadow dual-map

### 5.1 The shape (KD-1.1..KD-1.4)

A **rewrite epoch** per ino makes a sequential overwrite structurally
identical to fresh ingest: writes allocate fresh blocks (map B), the
per-block *durable* publish disappears, and ONE whole-tx swap publishes
the epoch. It rides three existing pieces wholesale:

1. **RAM-authoritative reads** (`fetch_metadata`'s dirty-authority law):
   the epoch's per-block records update the RAM `metadata_cache` map
   (`b → B_key`, CoW `Arc::make_mut`, under `INODE_META_LOCKS` — the
   merge primitive's own discipline, minus the save) and mark
   `layout_dirty`. RYW holds because reads already serve dirty RAM
   entries as the local authority.
2. **The in-flight allocation registry** (`inflight_register`): the
   epoch holds one `InflightAllocGuard` per B block until the swap is
   durable — **this is the named fsck hook**: mid-epoch B blocks are
   allocated-but-unpublished with a live owner, exactly the §5.6 C2/C3
   in-flight-registry exemption (plus the allocation-epoch side map for
   scan-latched mints). A crashed daemon drops the guards and shields
   nothing — the recovery walk owns the accounting.
3. **The layout save** (`save_metadata_to_backend`): the swap is ONE
   save under `INODE_META_LOCKS` with fencing revalidation inside — one
   KvTx = **one checksummed journal entry** (whole-tx atomicity + torn
   immunity by construction, v3 §4.10). No new commit machinery.

**Epoch open (KD-1.1):** the first complete-block write-through on a
striped ino that would displace an existing mapping (rewrite shape),
detected at merge-prep in the upload path. Lever:
`SQUEEZEFS_REWRITE_SHADOW=0` restores per-block durable publishes (A/B).

**Map B allocation as-fresh (KD-1.2):** B blocks come from
`allocate_block()` verbatim — allocation policy stays outside the epoch,
which is the whole Idea-13 interface: a future virgin∪reusable coloring
policy slots inside the allocator entry and the epoch inherits it with
zero contact surface.

**The record (KD-1.3):** per block, under `BLOCK_FLUSH_LOCKS(ino,b)` +
`INODE_META_LOCKS(ino)`: DMA'd + `publish_block`ed B key → RAM map;
displaced A key **parked in the epoch** (never freed yet); read tiers
purged for the displaced key (it left the map); B offset's inflight
guard moves into the epoch.

**The swap (KD-1.4):** epoch close = one durable save of the (dirty)
layout, then — strictly after the save returns — free every parked A key
(terminal frees ride Idea 4's elision: **zero discards**; clone-shared
keys decrement-only via `free_block`'s refcount law, which is how the VL
pre-publish refcount ledger is respected), drop the B guards, count
`rewrite_shadow_swaps`/`rewrite_shadow_bytes`.

### 5.2 The deferred-free law (the safety keystone)

> **A parked A key may be freed only after a durable save that no longer
> references it. Crash recovery owns every other case.**

This single law makes *intermediate* saves legal (KD-1.5): any durable
layout save mid-epoch (a staged-family persist, a truncate, an fsync of
another shape) commits whatever B bindings the RAM map carries — a
legitimate **partial swap**, whole-tx per save. Parked A keys whose
blocks were covered by that save become freeable at close; blocks not
yet covered keep their crash-fallback to A. There is no window in which
a durable map references a freed block.

### 5.3 Epoch close triggers (KD-1.6)

| Trigger | Why |
|---|---|
| **Full coverage** (epoch blocks × block_size ≥ file size) | the natural end of a sequential overwrite |
| **fsync / flush_inode_to_backend** | durability demanded now — close before the meta barrier (the flush legs never shadow-record: they demand durability, KD-1.10) |
| **RELEASE (last close)** | the writeback-cache durability boundary |
| **Shape-change ops** (truncate/punch prune, staging spill/promotion, extent-record commit, clone src/dst, setattr size) | those paths' merges keep their own durable primitive; closing first keeps one authority per family. (Movers need NO force-close: their `MergeExpected` supersession law skips silently when the epoch's RAM map outran their captured A key — re-plan revisits.) |
| **ENOSPC early-close** (KD-1.7) | a mid-epoch `StorageFull` closes the epoch (the swap frees parked A supply), then retries the allocation once; still full ⇒ the never-lossy ladder + `rewrite_shadow_fallbacks` (the loud fallback to today's CoW). **Structurally late on a co-writer** — see the supply-coupled close below and the KD-1.7 amendment |
| **Supply-coupled close (co-writer lanes)** — 2026-09-07, `.benchmarks/2026-09-07-rewrite-epoch-supply-close.md`; `SQUEEZEFS_REWRITE_SUPPLY_CLOSE` (default on) | the KD-1.7 early-close made AHEAD of the `StorageFull`, on the signal the lane refill already samples: a laned co-writer's ahead-refill tick (`BlockAllocator::ahead_refill_tick` / `pushed_refill_tick`, after the harvest) closes the mount's open epochs when `lane_reachable_blocks < watermark`, where `watermark = ceil(claim-rate EWMA × refill horizon)` capped at lane-share/4 is the ahead-harvest's own threshold — the blocks one loop transit consumes (design-free-grace-sustain §5.5). **Derivation**: the deficit `watermark − reachable` is what the closes must yield to restore one transit's cover (at exhaustion it IS the watermark); `routing::supply_close_plan` closes the LARGEST epoch first (parked count), accumulating until the yield covers the deficit — the fewest publishes for the most supply. **Bound**: every candidate yields ≥ 1 block, so closes per tick ≤ deficit ≤ watermark ≤ share/4 — a co-writer never publishes more epochs in a tick than blocks it is short; the storm shape (that many distinct epochs each parking one block) is exactly the one on which the un-shadowed path would already have published once per block. Candidates left open are counted (`rewrite_shadow_supply_close_bounded`). The close is `close_rewrite_epoch` verbatim (one whole-tx publish + the §5.2 deferred frees), so every §5.6 window stays true and nothing new is durable; never under the D0 fence (the W5 arm is the fence tripwires' story); idempotent against the dismount's own closes. Scope: installs only on a HARVESTING lane (`DataRouter::arm_rewrite_supply_close` → `BlockAllocator::set_lane_supply_close_sink`, from `cowriter::install_client_halves`) — a single writer and an authority keep KD-1.6/1.7 byte-identically (pinned). Ledger: `rewrite_shadow_supply_closes` / `_blocks`, `_declined_{covered,no_parked}`, `_bounded`. Contracts `tests/rewrite_shadow_supply_close_tests.rs`. **Mount-wide by law (finding 15's fpp re-attribution, 2026-09-07 — `.benchmarks/2026-09-07-cowriter-fpp-supply-residue.md` §8)**: the residue landing tried a PER-VOLUME plan (candidates = the keys parked on the asking volume, `SQUEEZEFS_REWRITE_SUPPLY_CLOSE_PER_VOLUME`) on the premise that a close's yield must land on the exhausted volume; the D row falsified the premise — under the lane-aware placement the volumes' stocks are equalized (the §5.9 band + the failover), so a key returning to EITHER volume restocks the mount — and measured the harm: with per-volume candidates and the covered sibling's tick declining, the keys parked on the covered volume stranded to the iteration boundary (14 % of the displaced keys released by the routine closes vs 0.3–1 % on both C rows; supply-closed blocks/s −17 %). Retired the same day; the plan is mount-wide, and contract 7 (the two-volume rig) pins that a starving volume's tick publishes the mount's largest epoch whose keys live on its sibling |
| **Idle / size-stable** | epochs idle past the sweep horizon close on the maintenance tick — bounds crash exposure of RAM-only bindings and defends the (theoretical) moka TTI eviction of a dirty entry. Horizon derives as TTI/10 (the eviction horizon it defends against), never a standalone constant |
| **Fence observed / unmount drain** | §5.4 / teardown |

Belt for the eviction hole (KD-1.9): `fetch_metadata_from_backend`
**composes any open epoch's shadow records over the fetched map** before
publishing to cache — one hook site; a refilled entry can never lose B
bindings structurally, so the idle-close is exposure-bounding, not
correctness-bearing.

**Supersession coherence (KD-1.11, the 2026-08-04 field fix —
`tests/rewrite_shadow_supersede_tests.rs`):** a mid-epoch **durable**
publish of a shadowed index (a flush-leg merge, a conveyor pass, a
truncate/punch prune, a mover `MergeExpected` apply) makes the epoch's
RAM-only `shadow[b]` binding stale — the durable path displaces and
frees the shadow's B key on its own discipline. The durable primitive
and `publish_pass` therefore **evict the superseded shadow entries under
the same `INODE_META_LOCKS` section that mutates the map**
(`supersede_shadow_bindings[_from]`, counted
`rewrite_shadow_superseded`), so the KD-1.9 compose can never resurrect
a displaced-and-freed key. Without this, the squeeze-test EXA overwrite
storm resurrected freed bindings into dirty entries that persisted:
fsck **C2Lost** (live map binds an allocator-untracked offset — the
deterministic read-EIO face: the freed key's incarnation word stays
retired for the whole session), **C2Leaked** (the clobbered durable key,
allocated with zero referencers), **C3** + cross-file corruption once
the double-freed offset reallocated (the run17 class), all healing on
remount — pure in-session poison. The compose also dirties the refilled
entry only when it actually inserted a binding (an empty shadow must not
shield a backend-true map from refills).

### 5.4 D0 fencing law (KD-1.8)

The swap's save presents the ino's CURRENT DLM token and revalidates
inside `save_metadata_to_backend` (existing machinery). Fenced at close:
**publish nothing, free NOTHING** (not A — the durable map may still
reference it; not B — an intermediate save may have published some),
drop guards + ledger, invalidate the ino's RAM cache entry (no
fenced-stale map may serve), count `rewrite_shadow_fence_drops`, loud.
The successor writer's recovery walk owns all accounting — the
FIND-M11-A remount law verbatim. A fenced holder fail-stops pre-swap by
construction: the D0 `failed` latch refuses the commit, and the
reclaimer/trim fence-halt latch guarantees no destructive device command
ever issues from the zombie.

**The fence class is the GENUINE one only (2026-09-06,
`.benchmarks/2026-09-06-cowriter-free-residual-lineage.md`):** the D0
custody poison (`data_custody::poisoned`) or a `WriterGuardFenced`
publish refusal (a dead custody era). A `FencingTokenExpired` from the
swap's save is a PROCESS-LOCAL lease rotation — within one process it can
only mean this daemon re-acquired the ino's lease between the closer's
token capture and the revalidation (sibling handles, range stripe grants)
— and the epoch's RAM-only bindings are the newest acked custody in
existence, so the close re-presents the ino's current generation and
converges (`rewrite_shadow_close_retries`), the 2026-08-06 tail-loss law
every other publish site already runs. The pre-law arm took W5 on a live
mount: it discarded acked bytes, and it dropped the local hygiene of
parked A keys whose displacement an intermediate publish had already
covered — on a co-writer, whose authority had already freed them, the
lingering refcounts fired `CLAIM ANOMALY` on every re-harvest (the s11
fleet's 850 of 913 anomalies, one fenced close per carrier).

### 5.5 Capacity (KD-1.7) + VL composition

Bounded ≤ 2× per file by construction (each block: at most one parked A
+ one live B). ENOSPC → early-close (above). VL capacity preflight:
mid-epoch, allocator used-counts include both A and B — preflight
honestly over-counts by the open epochs' parked bytes (gauged:
`rewrite_shadow_parked_bytes`); the epoch registry drains at close and
the preflight's checkpoint re-verification (`paused-capacity`) already
tolerates transient occupancy (`evacuate_transient_bytes` precedent).

**KD-1.7 amendment (2026-09-07 — the co-writer posture,
`.benchmarks/2026-09-07-rewrite-epoch-supply-close.md`):** "ENOSPC →
early-close, retry once" is a SINGLE-WRITER law. It rests on a close
returning the parked A supply to the local free list at once, so the one
retry finds it. On a co-writer the freed A key does not come back for one
whole recycle-loop transit — the free ships to the authority (or is
recomputed at the publish), enters the grace ring, is released to the
authority's per-lane list, and returns only on a harvest RPC — so the
retry finds nothing and the write falls into the never-lossy ladder / the
`alloc_lane_enospc_refusals` storm. And the 2× bound above is a bound on
a lane-PARTITIONED share: a co-writer rewriting a slice of a shared file
never reaches full coverage, fsync/RELEASE come at the iteration boundary,
so it parks its whole per-iteration displacement against a share that
must also hold live + new + the previous burst still in flight (the s11
fleet, `.benchmarks/2026-09-06-free-grace-term1-fleet.md` §3: 160 + 160 +
≤ 160 + ≤ 160 against 512 per volume). The supply-coupled close (§5.3)
is KD-1.7's remedy on that posture: the same close, fired on the lane's
own supply signal BEFORE the `StorageFull`, so the parked term is bounded
by the lane's headroom instead of the iteration's length. KD-1.7 itself
is unchanged and still the last arm.

### 5.6 Crash-window table (KD-1.5 — every window pinned by a contract)

| # | Window | Durable state at crash | Recovery outcome |
|---|---|---|---|
| W1 | mid-epoch, no persist yet | map = A entire; B blocks DMA'd but referenced by no durable map | recovery walk seeds refcounts from durable maps only ⇒ B offsets unclaimed ⇒ free-listed; reuse guarded by write-before-publish + incarnation seqlock. File reads = A (pre-rewrite image — un-fsynced ACKed writes lost: the writeback-class contract, unchanged). **A intact.** |
| W2 | torn swap entry | journal entry torn ⇒ detected-and-ignored (v3 §4.10) | ≡ W1 |
| W3 | swap durable, A frees not yet run | map = B; parked A keys unfreed | recovery walk: A offsets referenced by no durable map ⇒ free-listed (the reclaim-queue kill-9 posture: recovery owns the accounting) |
| W4 | post-swap, elided debt outstanding | debt is RAM-only | un-returned thin space; free list is durably derived ⇒ next trim venue covers (KD-4.9) |
| W5 | fenced pre-swap | map = A (save refused) | zombie freed nothing (fence-halt), published nothing; successor recovery sees plain A + unclaimed B ⇒ ≡ W1 for the successor |
| W6 | intermediate save committed, then crash | map = A ⊕ B-partial (whole-tx per save) | per-block: persisted B bindings are real (DMA'd, published, durable map names them ✓); their parked A keys are durably unreferenced ⇒ recovery frees; unpersisted blocks ≡ W1. Legal by the §5.2 deferred-free law. |

**Live-fsck composition (not a crash window):** a mid-epoch scan sees B
offsets as allocated-but-tree-unreferenced ⇒ exempted by the in-flight
registry (live epoch guards) and the allocation-epoch side map;
`fsck_findings` stays 0 — pinned by a contract test.

### 5.7 What rides the shadow record (KD-1.10 — scope fence)

**ONLY the complete-block write-through on the ACK path** (the pipeline
task and the `sync_inline` A/B arm) shadow-records. Every other merge
keeps the durable primitive verbatim: the flush legs (fsync/teardown
demand durability NOW), the writeback/staged family (their staged
custody is *durable* custody — a RAM-only merge followed by staged-entry
release would be a durability regression), promotion, spill, extent
fold, truncate/punch, movers. The ACK path's custody is RAM-parked and
crash-lossy by class — deferring its *map binding* to the swap moves no
durability line.

## 6. How the P0 ideas compose on the charter rows

**Sequential overwrite of existing striped data** (the ±5 % row): the
epoch makes the write stream allocation-shaped exactly like fresh ingest
(allocate → DMA → RAM record; no per-block durable publish, no per-block
free enqueue, no `displaced_free` phase); Idea 4 removes every discard
from the row (elided debt, drained at idle); the close pays one commit +
one bulk elided free pass. Device bytes = user bytes (amp → ~1.0; the
gate ≤ 1.05); rate → fresh-ingest rate because it *is* the fresh-ingest
code path plus one epilogue.

**Loop-rewrite** (latest-wins): overlapping face — supersession (Idea 2)
publishes only the newest image per block; epoch composition means the
superseded image's entire lifecycle (offset, DMA, record) unwinds
without ever touching durable state. Non-overlapping face — measured and
reported now; gated under Idea 8 (§2.2).

**Durability**: unchanged class, now *stated*: ACK-parked custody,
durable at fsync/close/RELEASE; the epoch's swap rides exactly those
boundaries. Idea 8 names the classes (§9.3).

## 7. Lock order & invariants (P1-9 audit)

* Epoch record: (1) inode locks untouched → (3) `BLOCK_FLUSH_LOCKS`
  (held by the upload caller) → 4a/4b never (no commit). The RAM-map
  mutation takes `INODE_META_LOCKS` (the merge primitive's own meta
  lock) *inside* the block lock — the existing merge-under-block-lock
  order, unchanged.
* Epoch close: `INODE_META_LOCKS` → save (4a/4b inside the commit
  conveyor, unchanged) → frees (post-lock). Close never takes block
  locks (it consumes only recorded state; racing per-block records
  serialize on the meta lock).
* Supersession: block lock → [unlocked DMA] → block lock. No new
  cross-lock edges; the unlocked window holds nothing.
* The reclaim/trim fence-halt latch and the D0 `failed` latch remain the
  only destructive-I/O gates; both are observed by every new device-
  command site (trim).

## 8. Observability (stats inode — all new fields)

| Family | Fields | Semantics |
|---|---|---|
| Idea 17 | `rewrite_user_bytes`, `rewrite_device_write_bytes`, `rewrite_blocks` | rewrite-class attribution: user bytes landing on already-mapped striped ranges; device write bytes submitted for displacing/in-place/shadow blocks; blocks displaced-or-replaced. Row validity: a rewrite row's deltas must account for its traffic. |
| Idea 4 | `block_free_reclaim_elided`, `block_free_elided_debt_bytes` (gauge), `block_free_trim_discards`, `block_free_trim_bytes`, `block_free_debt_pressure_drains` | ledger identity `queued + elided ≡ terminal frees`; debt gauge returns toward 0 under trim/reuse; pressure drains ≈ 0 below watermark. |
| Idea 2 | `write_pipeline_supersessions`, `write_pipeline_superseded_bytes` | stale in-flight completions that published nothing (the latest-wins engagement instrument, overlapping face). |
| Idea 1 | `rewrite_shadow_swaps`, `rewrite_shadow_bytes`, `rewrite_shadow_fallbacks`, `rewrite_shadow_fence_drops`, `rewrite_shadow_open_epochs` (gauge), `rewrite_shadow_parked_bytes` (gauge), `rewrite_shadow_superseded` | swaps = closes that persisted; bytes = B bytes swapped; fallbacks = ENOSPC-degraded closes; fence_drops must stay 0 on healthy mounts; parked gauge = the VL preflight transient; superseded = stale shadow bindings evicted by mid-epoch durable publishes (KD-1.11 — growth is the two machineries composing correctly). |
| Idea 1 — the supply-coupled close (co-writer lanes, 2026-09-07) | `rewrite_shadow_supply_closes`, `rewrite_shadow_supply_close_blocks`, `rewrite_shadow_supply_close_declined_covered`, `rewrite_shadow_supply_close_declined_no_parked`, `rewrite_shadow_supply_close_bounded` | closes the refill tick fired on the lane-supply signal (⊆ swaps) and the parked A keys they released (the engagement instrument: a rewriting co-writer whose lane ENOSPCs with this flat is parking its supply to the boundary); the decision ledger — `covered` = the lane covered one loop transit (the healthy per-tick beat; growing beside lane ENOSPC = the watermark is not reading the starvation), `no_parked` = starving with nothing to inject (the KD-1.7-only shape), `bounded` = candidates left open because larger epochs covered the deficit (growing beside lane ENOSPC = the deficit under-reads the need). ALL 0 on single-writer / authority mounts by construction. |

## 9. Design-only sections (implementation next campaign)

### 9.1 Idea 6 — substrate-probed reclaim/replace economics

Runtime probe (portable-by-default law: measured behavior, never device
model tables): at mount, per data namespace, a bounded (~100 ms) probe
over never-minted tail blocks measures (a) fresh-write, (b) same-offset
rewrite, (c) discard command service. Verdicts (`substrate_replace_cost`,
`substrate_discard_cost` gauges) feed *pacing policy only*: trim-drain
width/cadence (Idea 4), and the Idea 7 in-place election. Harmless where
the traits are absent (probe refusal ⇒ today's defaults). Keyed
decisions to resolve at implementation: probe venue on shared/PR-fenced
namespaces (must ride the D0 writer claim; never probe a volume we do
not own), probe blocks must be claimed via the allocator (never raw
device offsets), re-probe cadence (mount-only vs health-worker).

### 9.2 Idea 7 — intent-derived in-place overwrite

Generalizes the W1 whole-block in-place arm from a lever
(`SQUEEZEFS_INPLACE_OVERWRITE`) to an *election*: eligible full-block
overwrites choose in-place vs shadow-CoW from the Idea 6 verdict
(replace-cheap substrates in-place; replace-costly shadow). Inherits the
6-clause predicate + §5.1 fence verbatim; the crash contract stays
"only app-written sectors are ever rewritten". Composition pin: an
in-place election *inside* an open epoch records nothing (no
displacement) — the epoch simply never sees the block. Keyed decisions:
election hysteresis (never flap per-block), interaction with encryption
(transformed volumes stay CoW — stored length varies), and the A/B
proof obligation on both substrate classes.

### 9.3 Idea 8 — durability as a named mount class

Three classes, declared at mount (`-o data={ordered|writeback|sync}`):

* **`data=ordered`** (today, the default): ACK-parked custody, eager
  upload launch, durable at fsync/close. The rewrite program's P0 lands
  entirely inside this class.
* **`data=writeback`**: upload launch may defer — retained dirty
  custody bounded by an R5 dirty share + an idle re-dirty window;
  forced-device triggers fsync/R5 Red/idle timer (the charter list).
  This is where Idea 2's non-overlapping loop face (device writes ≈
  unique blocks across passes) arms its gate, and where the retention
  window becomes a *stated* crash-exposure cadence instead of an
  accident.
* **`data=sync`**: per-write durability (upload + commit before ACK) —
  the audit/wal-hosting posture; prices the full pipeline per op.

Keyed decisions to resolve: class surface (INIT-time, immutable per
mount), interaction with O_SYNC/O_DSYNC flags (per-op override wins
toward durability, never away), fsync semantics identical in all
classes, and the scoreboard's durability-leveled row labeling (RW6
precedent) gaining a class column.

## 10. Keyed decisions flagged for orchestrator review

1. **Idea 2 retention scoping (KD-2.1):** the safe-today subset is
   resolved as supersession + retained-dirty-authority; the idle-window
   upload deferral (required for the non-overlapping loop-rewrite gate)
   is classified as `data=writeback` machinery (Idea 8). The
   loop-rewrite SLO therefore gates on the overlapping face now and
   measures the non-overlapping face until Idea 8. If leadership intended
   the full loop gate in P0, Idea 8's class naming must move into this
   campaign.
2. **W6 (intermediate saves as partial swaps):** legality rests on the
   §5.2 deferred-free law (A keys freed only post-durable-unreference).
   The alternative (force-closing on every dirty persist) was rejected
   as a needless serialization; the law is pinned by contract tests.

## 11. Test plan (red-first, per idea, in landing order)

| Idea | Suite | Contracts |
|---|---|---|
| 17 | `tests/rewrite_amp_tests.rs` + `write_amp_rig.sh` rows | counter semantics (overwrite grows `rewrite_*`, fresh does not); rig rows print REWRITE_AMP + mid-row discard columns + engagement checks |
| 4 | `tests/discard_elision_tests.rs` | elided-until-pressure (zero device commands during foreground); immediate reallocatability; claim-cancels-debt; trim protocol (claim-discard-reinsert; lost claim = skip); watermark engagement (debt > virgin ⇒ paced drain); ledger identity; `SQUEEZEFS_DISCARD_ELISION=0` verbatim old path; fence-halt |
| 2 | `tests/write_supersession_tests.rs` | planted-stale-CQE (stall seam): stale completion publishes nothing + frees orphan + newest bytes win; entry-gone skip; fsync-race durability (flush leg wins ⇒ pipeline task no-ops); no-leak reconciliation; counters |
| 1 | `tests/rewrite_shadow_tests.rs` | epoch open/record/RYW; ONE-commit swap (journal-entry delta); frees only post-swap (elided); W1 crash (drop-without-close ⇒ remount reads A, B blocks reclaimed, `fsck_findings` 0); W5 fence (stale token ⇒ nothing freed, loud); ENOSPC early-close + fallback counter; mid-epoch fsck composition (inflight-exempt); refetch-compose (eviction hole closed); gauges |
| 1 (KD-1.11) | `tests/rewrite_shadow_supersede_tests.rs` | mid-epoch durable merge evicts the superseded shadow binding (refill resolves the durable key, never a freed one); the full field resurrection sequence cannot double-free or leak (every mapped offset stays allocator-tracked, read-back exact); truncate prunes shadowed indexes past the cut |

Merge bar: clippy `-D warnings` + fmt; the write-path family
(`write_through_coverage_tests`, `write_pipeline*`, reclaim/fence
suites, `transport_lease_overlong_tests`, `shim_parity_tests`, fsck
suites touched) + the new suites, `--test-threads=1`. Loom: no lock-free
core added (supersession is block-lock-serialized by design — §4.2;
elision debt uses existing scc/DashSet claim primitives whose protocols
are already loom/field-pinned); adjudication recorded in the evidence
note.

## 12. Field acceptance (the reformat window — owed rows)

Stated per the cluster epoch lock (no deployment this campaign): the
field acceptance owes (1) fresh-vs-rewrite A-B-B-A with `rewrite_amp`
columns (gate ±5 % and ≤ 1.05 / zero mid-row discards), (2) the
loop-rewrite latest-wins row with the device-writes≈unique-blocks proof
(supersession engagement + coalesce factor), (3) the
zero-mid-row-discards assert on a ≥ 60 s sustained row (flatness law),
(4) a loaded soak (the rewrite-publish-drain §7 recipe) with the wedge
indicator set all-zero. Local brackets ride the tcp devsub (substrate
law: loop is scoping-only for these rows).
