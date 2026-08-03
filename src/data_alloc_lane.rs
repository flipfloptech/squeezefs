//! **The data-plane allocation partition** — DLM **S9** blocker #3, in
//! S9's own words: *"Two writers' allocators would collide on fresh
//! offsets — the DATA analogue of §6.2 item 3. This is the largest
//! remaining gap for a real two-host write row."*
//!
//! Design record: `docs/design-mw-data-alloc-partition.md`. Contracts:
//! `tests/mw_data_alloc_lane_tests.rs`. Siblings this is deliberately a
//! copy of, one plane over: `crate::lane_core` + `kv::ino_lane` (§6.2 item
//! 5, per-writer ino lanes) and `kv::alloc_ext`'s page-partitioned extent
//! bitmap (§6.2 item 3). **No third pattern was invented.**
//!
//! ## The law: fresh allocation is a residue class; frees are lane-blind
//!
//! With `writers = W`, block index `b` belongs to **lane** `b % W`, and
//! writer `w` mints only lane-`w` indices: `w`, `w + W`, `w + 2W`, …
//! ([`block_lane_of`], the same arithmetic core the ino lanes run —
//! `crate::lane_core`, base 0 here because block index 0 is a legitimate
//! allocatable block while local ino 0/1 are reserved).
//!
//! Three properties follow, and the third is the one that makes the DATA
//! plane *easier* than the metadata plane:
//!
//! 1. **disjointness** — two lanes never mint the same index, so "one
//!    device offset has at most one live owner" survives N concurrent
//!    writers **with no arbitration, no range hand-out and no durable
//!    distribution watermark**;
//! 2. **attribution** — [`block_lane_of`] names the writer that minted any
//!    offset, so recovery and fsck classify a foreign writer's blocks
//!    instead of guessing;
//! 3. **frees need no protocol at all.** The owning lane is *derivable
//!    from the offset*, so a writer freeing a block it did not allocate
//!    performs **no ownership lookup, no message and no record**: it
//!    releases the reference, and the block re-enters the free supply of
//!    whichever lane the arithmetic says owns it. `begin_free` /
//!    `finish_free` are therefore **byte-identical under partition** — the
//!    partition lives entirely in the ALLOCATION direction.
//!
//! Reuse follows the same law as a fresh mint: a free block in lane `w` is
//! re-allocatable only by lane `w`. That is what makes reuse
//! arbitration-free too, and it is the source of the stranding bound
//! ([`stranded_blocks_bound`]) — the honest cost of the whole design,
//! published as a gauge and stated in `docs/operations.md`.
//!
//! **Solo (`W = 1`) is lane 0 = every block, stride 1.** A solo engagement
//! installs *nothing at all* (see `BlockAllocator::engage_alloc_lanes`), so
//! single-writer allocation is not "equivalent to" today's path — it **is**
//! today's path, and the tie tests assert both halves (the arithmetic
//! collapses, and the allocator installs no partition).
//!
//! ## Durability: one record per (data volume, lane)
//!
//! The RAM free list and the allocation cursor are **derived** state
//! (`BlockAllocator::seed_from_durable_refs` / `recover_active_blocks_v3`:
//! the free list is the complement of the durably-referenced set below the
//! cursor). Derived state is enough for a crash-and-remount of a *lone*
//! writer — an allocated-but-unpublished offset is referenced by nothing,
//! so re-minting it is correct and is today's behaviour.
//!
//! It is **not** enough when a lane changes hands while a peer is still
//! alive. A fenced-but-live predecessor (a zombie) may hold lane-`w`
//! offsets that it minted, is DMA-ing into, and never published; the
//! durable references do not mention them, so a successor's derived floor
//! sits *below* them and hands them out again. So each lane carries one
//! durable **reservation watermark** — [`LaneReservation`], written *ahead
//! of use* — and recovery's floor is
//! `max(reservation, derived dense floor)` rounded up into the lane
//! ([`recover_lane_floor`]). A successor therefore starts above every
//! offset its predecessor *could* have minted, not merely above the ones it
//! managed to publish.
//!
//! The record is keyed on **(volume, lane)** and never on writer identity,
//! which is what makes lane ADOPTION (`BlockAllocator::adopt_lane`) free of
//! new durable state: an adopting writer raises the adopted lane's own
//! watermark as it mints there, so any future holder of that lane recovers
//! above it.
//!
//! Reservation cost is `1` metadata commit per [`reserve_grain_blocks`]
//! fresh blocks **per lane** — free-list reuse never reserves (a freed
//! offset is dominated by the derived floor), which keeps the rewrite hot
//! path at exactly zero reservation work.
//!
//! ## Where the lane comes from
//!
//! Who may write, and which writer is lane `w` of `W`, is **§6.9 S4/S8/S9's
//! admission problem** — exactly as `kv::ino_lane` says for inos. This
//! module makes the partition *expressible and safe*; the assignment itself
//! is [`crate::alloc_lane_grant`]: the authority derives one lane per
//! enrolled writer from the **durable claim set**, keeps lane 0, and hands
//! each co-writer its `(lane, writers)` pair on the S9 **custody lease** —
//! which is why no knob can put two co-writers in one residue class.
//! Nothing in a single-writer mount path installs a non-solo partition (a
//! solo assignment IS `SOLO`, and a solo engagement installs nothing).

use crate::error::{Result, SqueezefsError};
use crate::lane_core;
use crate::meta_backend::kv::journal::AppendPartition;
use std::sync::Arc;

/// The base of the block-index space. Zero, unlike
/// [`crate::meta_backend::kv::ino_lane::LOCAL_INO_BASE`]: block index 0 is
/// an ordinary allocatable block, so lane `w` owns `w` itself.
pub const BLOCK_LANE_BASE: u64 = 0;

/// Record-name prefix of the durable per-lane reservation
/// ([`lane_record_name`]). Unprefixed internal family on ino 1, beside
/// `writer_claim` / `job:` — screened from FUSE by the VAL-2 xattr
/// ALLOWLIST (`kv::backend::xattr_name_allowed`), which admits only
/// `user.*` / `security.*` / `trusted.*`, so a new internal name needs no
/// denylist edit and can never be reached from a shell.
pub const LANE_RECORD_PREFIX: &str = "alloc_lane:";

/// `LaneReservation` wire version. A record carrying anything else refuses
/// **loud** (forward-only, the house directive): an unknown version means a
/// newer binary reserved a frontier this one cannot interpret, and guessing
/// would hand out an offset that binary may own.
pub const LANE_RESERVATION_VERSION: u8 = 1;

/// Encoded length: `version | writers | lane | reserved_upto | xxh3`.
pub const LANE_RESERVATION_LEN: usize = 1 + 2 + 2 + 8 + 8;

/// This mount's data-plane partition, packed `writers << 16 | writer_id`
/// (one relaxed load on the engagement path, no lock).
static MOUNT_PARTITION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// This mount's data-plane allocation partition —
/// [`AppendPartition::SOLO`] until an admission installs one, which is
/// **every single-writer mount**.
///
/// The installing authority is §6.9 **S9's custody plane**: the lease a
/// co-writer holds is what says *which* writer of *how many* it is
/// (`LeaseFrame::writer_lane` / `writers`, minted by the authority from the
/// durable claim set — [`crate::alloc_lane_grant::LaneAssignment`]), and the
/// installer is [`crate::alloc_lane_grant`]'s engagement. Deliberately a process-wide
/// word rather than a per-volume one: a mount is one writer of the SET, and
/// two volumes disagreeing about which lane this mount owns is precisely the
/// state where two allocators would mint into a peer's residue class.
pub fn mount_partition() -> AppendPartition {
    let packed = MOUNT_PARTITION.load(std::sync::atomic::Ordering::Acquire);
    if packed == 0 {
        return AppendPartition::SOLO;
    }
    AppendPartition::new((packed >> 16) as u16, (packed & 0xffff) as u16)
        .unwrap_or(AppendPartition::SOLO)
}

/// Install this mount's partition (S9's admission — see
/// [`mount_partition`]). Idempotent; a second, DIFFERENT partition is
/// refused loud rather than swapped under allocators that have already
/// minted in a lane.
pub fn install_mount_partition(part: AppendPartition) -> Result<()> {
    let packed = (u32::from(part.writers()) << 16) | u32::from(part.writer_id());
    match MOUNT_PARTITION.compare_exchange(
        0,
        packed,
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Acquire,
    ) {
        Ok(_) => Ok(()),
        Err(prev) if prev == packed => Ok(()),
        Err(prev) => {
            let msg = format!(
                "refusing to install allocation partition {}/{}: this mount is already writer \
                 {} of {} and has may have minted in that lane — a partition swap would move \
                 live offsets into a peer's residue class",
                part.writer_id(),
                part.writers(),
                prev & 0xffff,
                prev >> 16
            );
            log::error!("{msg}");
            Err(SqueezefsError::InvalidOperation(msg))
        }
    }
}

/// **Test seam** (the `data_custody::test_reset_custody_generation`
/// precedent): clear the installed partition. Production has none — a
/// mount's lane is fixed for the life of the process.
pub fn test_reset_mount_partition() {
    MOUNT_PARTITION.store(0, std::sync::atomic::Ordering::Release);
}

/// The lane that owns block index `block_idx` in a `writers`-way partition
/// — **the attribution function, and the reason a free needs no protocol**.
/// `writers <= 1` reads as solo (lane 0 owns everything).
pub fn block_lane_of(block_idx: u64, writers: u16) -> u64 {
    lane_core::lane_of(block_idx, BLOCK_LANE_BASE, u64::from(writers))
}

/// [`block_lane_of`] for a byte offset — the form the free / read / fsck
/// paths hold. `chunk_size` is the allocator's physical stride
/// ([`crate::block_allocator::CHUNK_SIZE`]); a zero chunk reads as solo
/// rather than dividing by zero.
pub fn offset_lane_of(offset: u64, chunk_size: u64, writers: u16) -> u64 {
    if chunk_size == 0 {
        return 0;
    }
    block_lane_of(offset / chunk_size, writers)
}

/// The first block index writer `part` may ever mint.
pub fn first_block_in_lane(part: AppendPartition) -> u64 {
    lane_core::first_in_lane(
        BLOCK_LANE_BASE,
        u64::from(part.writers()),
        u64::from(part.writer_id()),
    )
}

/// The smallest lane-`part` index `≥ floor` — the recovery / mint rounding.
/// Idempotent: a floor already in the lane comes back unchanged.
pub fn next_block_in_lane_at_or_above(floor: u64, part: AppendPartition) -> u64 {
    lane_core::next_in_lane_at_or_above(
        floor,
        BLOCK_LANE_BASE,
        u64::from(part.writers()),
        u64::from(part.writer_id()),
    )
}

/// The smallest index `≥ floor` whose lane is in `owned` (a lane bitmask —
/// bit `l` set ⇔ lane `l` is ours). `None` ⇔ the mask is empty, which no
/// engaged partition ever is (a partition always owns its own lane).
///
/// This — not a [`crate::lane_core::LaneCursor`] — is the mint primitive,
/// for two reasons stated once here because they are the whole reason
/// `LaneCursor` does not fit the block plane:
///
/// 1. a block mint must be able to **refuse at device capacity without
///    advancing the cursor** (`BlockAllocator::next_fresh_block`'s CAS
///    loop: "a refused racer must not bump the cursor"), and a
///    `fetch_add` cursor cannot un-mint;
/// 2. after a lane ADOPTION a writer mints in **several** residue classes
///    at once, which one strided cursor cannot express.
///
/// The lane *arithmetic* is still `lane_core`'s, verbatim.
pub fn next_owned_index_at_or_above(floor: u64, owned: u64, writers: u16) -> Option<u64> {
    if owned == 0 {
        return None;
    }
    let w = u64::from(writers.max(1));
    if w == 1 {
        // Solo: lane 0 owns every index, so the answer is the floor
        // itself — the shipped `highest_block` CAS loop, unchanged.
        return Some(floor);
    }
    for step in 0..w {
        let idx = floor.saturating_add(step);
        if owned & (1u64 << block_lane_of(idx, writers)) != 0 {
            return Some(idx);
        }
    }
    None
}

/// Blocks lane `lane` owns on a device of `capacity_blocks` — the EXACT
/// count (`⌈(capacity − lane) / W⌉`), which is what makes the per-writer
/// capacity statement in `docs/operations.md` arithmetic rather than
/// approximate. Lane shares differ by at most one block.
pub fn lane_capacity_blocks(capacity_blocks: u64, writers: u16, lane: u16) -> u64 {
    let w = u64::from(writers.max(1));
    let l = u64::from(lane) % w;
    if capacity_blocks <= l {
        return 0;
    }
    (capacity_blocks - l).div_ceil(w)
}

/// **The stranded-capacity bound, in blocks** — the capacity a writer
/// holding `owned_lanes` of `writers` lanes cannot reach on a device of
/// `capacity_blocks`, i.e. every block belonging to a lane it does not own:
///
/// ```text
/// stranded(cap, W, owned) = cap − Σ_{l ∈ owned} lane_capacity(cap, W, l)
///                         ≤ cap × (W − owned) / W + (W − 1)
/// ```
///
/// Two readings an operator needs, both stated in `docs/operations.md`:
///
/// * **the reachability bound** (this function): with one lane of `W`, a
///   writer can be refused `StorageFull` while up to `cap × (W−1)/W`
///   blocks are free — which is why the ENOSPC rule is *adopt a proven-dead
///   lane, then refuse loudly naming the number*, never "refuse silently";
/// * **the granularity bound**: a writer's own share is `cap/W ± 1` block,
///   so partitioning costs at most **W − 1 blocks** of usable capacity
///   versus an unpartitioned device. That is the number to plan with when
///   every writer stays inside its share.
pub fn stranded_blocks_bound(capacity_blocks: u64, writers: u16, owned: u64) -> u64 {
    let w = u64::from(writers.max(1));
    let mut reachable = 0u64;
    for lane in 0..w {
        if owned & (1u64 << lane) != 0 {
            reachable = reachable.saturating_add(lane_capacity_blocks(
                capacity_blocks,
                writers.max(1),
                lane as u16,
            ));
        }
    }
    capacity_blocks.saturating_sub(reachable)
}

/// Fresh blocks one durable reservation covers, per lane.
///
/// **Derived, never a free-floating constant** (the standing derivation
/// law): the grain must exceed the fresh blocks a streaming writer can mint
/// inside one write-pipeline window, or a reservation commit would land on
/// the per-block path instead of once per grain —
/// `FLOOR_BLOCKS_PER_LANE × HEADROOM × cpus` is exactly that window's
/// cold-start shape ([`crate::write_pipeline`]'s own derivation inputs).
///
/// * **floor** `FLOOR_BLOCKS_PER_LANE × 8` — one grain must cover the
///   write pipeline's cold aggregate floor over its eight-lane shape, so a
///   cold streaming start never pays two commits inside one window;
/// * **ceiling** `lane share / 64` — the grain is also the *temporarily
///   unavailable* window after a lane dies with a live peer (its
///   unpublished tail is quarantined until the drain proof), so it is
///   capped at 1/64 of the lane rather than left to scale with core count
///   on a small volume.
///
/// `SQUEEZEFS_ALLOC_LANE_RESERVE_BLOCKS` overrides verbatim (the A/B lever;
/// `1` = a commit per fresh block, the pathological control).
pub fn reserve_grain_blocks(capacity_blocks: u64, writers: u16) -> u64 {
    if let Some(explicit) =
        crate::env_knobs::opt_int_knob::<u64>("SQUEEZEFS_ALLOC_LANE_RESERVE_BLOCKS")
    {
        if explicit > 0 {
            return explicit;
        }
    }
    let floor = crate::write_pipeline::FLOOR_BLOCKS_PER_LANE.saturating_mul(8);
    let derived = crate::write_pipeline::FLOOR_BLOCKS_PER_LANE
        .saturating_mul(crate::write_pipeline::HEADROOM)
        .saturating_mul(crate::cpu::process_parallelism() as u64);
    let lane_share = lane_capacity_blocks(capacity_blocks, writers, 0);
    let ceiling = (lane_share / 64).max(floor);
    derived.clamp(floor, ceiling)
}

/// One lane's durable reservation watermark: *this lane will never mint an
/// index at or above `reserved_upto`* until a newer record says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneReservation {
    /// The partition width the reserving writer ran under. A record whose
    /// width disagrees with the reading mount's makes lane identity
    /// meaningless, so [`recover_lane_floor`] treats EVERY such record as a
    /// floor for EVERY lane (see that function).
    pub writers: u16,
    /// The lane this record reserves for.
    pub lane: u16,
    /// Exclusive block-index bound of the reservation.
    pub reserved_upto: u64,
}

impl LaneReservation {
    /// Encode: `version | writers | lane | reserved_upto | xxh3(prefix)`.
    /// Checksummed like every other on-disk unit (§4.10) even though it
    /// rides a whole-tx-atomic record — a torn or bit-flipped watermark
    /// that decoded silently could place the floor BELOW a live peer's
    /// minted offsets, which is the one failure this record exists to
    /// prevent.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(LANE_RESERVATION_LEN);
        out.push(LANE_RESERVATION_VERSION);
        out.extend_from_slice(&self.writers.to_be_bytes());
        out.extend_from_slice(&self.lane.to_be_bytes());
        out.extend_from_slice(&self.reserved_upto.to_be_bytes());
        let sum = xxhash_rust::xxh3::xxh3_64(&out);
        out.extend_from_slice(&sum.to_be_bytes());
        out
    }

    /// Decode, refusing loud on length, version, checksum, or a lane
    /// outside its own width.
    pub fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() != LANE_RESERVATION_LEN {
            return Err(Self::refuse(format!(
                "lane reservation record is {} B, expected {LANE_RESERVATION_LEN}",
                raw.len()
            )));
        }
        if raw[0] != LANE_RESERVATION_VERSION {
            return Err(Self::refuse(format!(
                "lane reservation record version {} is not {LANE_RESERVATION_VERSION}: a newer \
                 binary reserved a frontier this one cannot interpret (forward-only)",
                raw[0]
            )));
        }
        let want = xxhash_rust::xxh3::xxh3_64(&raw[..LANE_RESERVATION_LEN - 8]);
        let got = u64::from_be_bytes(raw[LANE_RESERVATION_LEN - 8..].try_into().unwrap());
        if want != got {
            return Err(Self::refuse(format!(
                "lane reservation checksum mismatch (want {want:#x}, got {got:#x})"
            )));
        }
        let writers = u16::from_be_bytes([raw[1], raw[2]]);
        let lane = u16::from_be_bytes([raw[3], raw[4]]);
        let reserved_upto = u64::from_be_bytes(raw[5..13].try_into().unwrap());
        if writers == 0 || lane >= writers {
            return Err(Self::refuse(format!(
                "lane reservation names lane {lane} of {writers} writers, which is not a lane"
            )));
        }
        Ok(Self {
            writers,
            lane,
            reserved_upto,
        })
    }

    fn refuse(msg: String) -> SqueezefsError {
        log::error!("{msg}");
        SqueezefsError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, msg))
    }
}

/// The durable record's name: `alloc_lane:{vol_tag:016x}:{lane:04x}`.
///
/// `vol_tag` is the SAME durable data-volume identity `TREE_BLOCK_REFS`
/// keys on ([`crate::meta_backend::kv::block_refs::volume_tag`], KD-5) —
/// never a path, an ordinal, or a set position, so a volume that is
/// removed and re-added cannot inherit a stranger's watermark.
pub fn lane_record_name(vol_tag: u64, lane: u16) -> String {
    format!("{LANE_RECORD_PREFIX}{vol_tag:016x}:{lane:04x}")
}

/// Inverse of [`lane_record_name`] — the offline-probe / listing shape.
pub fn parse_lane_record_name(name: &str) -> Option<(u64, u16)> {
    let rest = name.strip_prefix(LANE_RECORD_PREFIX)?;
    let (tag, lane) = rest.split_once(':')?;
    if tag.len() != 16 || lane.len() != 4 {
        return None;
    }
    Some((
        u64::from_str_radix(tag, 16).ok()?,
        u16::from_str_radix(lane, 16).ok()?,
    ))
}

/// **The recovery rule.** The floor writer `part` may resume minting from,
/// given every reservation record found for this volume and the derived
/// dense floor (`highest_block` after the durable-reference seed —
/// `max(referenced) + 1`).
///
/// ```text
/// floor = next_in_lane(max( derived_dense_floor,
///                           own-lane reservation,
///                           every record whose width ≠ ours ))
/// ```
///
/// Three clauses, each load-bearing:
///
/// * **the derived floor** is the pre-partition rule, unchanged — it is
///   what covers offsets a predecessor published;
/// * **the own-lane reservation** is what covers offsets a predecessor
///   *minted but never published*, including a zombie's in-flight tail
///   (the crash window derived state cannot see);
/// * **a foreign-width record** invalidates lane identity itself: under a
///   different `W` the index `b` belonged to a different lane, so every
///   such watermark must dominate every lane. Rounding up burns indices
///   into free-list gaps (reachable again by whichever lane the NEW width
///   assigns them), never permanently.
///
/// A record for a FOREIGN lane at OUR width is deliberately not a floor:
/// that lane's indices are not ours to mint, so its watermark says nothing
/// about our residue class. (Adopting that lane raises our own view of it —
/// `BlockAllocator::adopt_lane`.)
pub fn recover_lane_floor(
    records: &[LaneReservation],
    derived_dense_floor: u64,
    part: AppendPartition,
) -> u64 {
    let mut floor = derived_dense_floor;
    for rec in records {
        let relevant = rec.writers != part.writers() || rec.lane == part.writer_id();
        if relevant {
            floor = floor.max(rec.reserved_upto);
        }
    }
    next_block_in_lane_at_or_above(floor, part)
}

/// The durable sink one lane's reservation raise runs through: `(lane,
/// reserved_upto)` → a committed, barriered record.
///
/// Boxed async closure, the [`crate::block_allocator::SpaceValve`] shape —
/// the allocator must not learn the metadata plane's types, and the
/// reservation must be `await`ed on the allocating task (honest
/// backpressure on exactly the writer that needs the frontier).
pub type LaneReserveSink = Arc<
    dyn Fn(u16, u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
        + Send
        + Sync,
>;

/// The production sink: one **routed** raise per grain, which on a mount
/// that holds metadata authority is one `setxattr` on ino 1 — a
/// whole-tx-atomic, checksummed, torn-immune commit through the M7 conveyor
/// (KD-2's plane, the `job:` record precedent) — and on a **co-writer** is
/// the same commit performed BY THE AUTHORITY, shipped over S9's publish
/// vocabulary ([`crate::meta_ship::publish::raise_alloc_lane`]).
///
/// One sink for both postures, because the routing decision already exists:
/// a raise names ino 1, and `owner_of(ino 1)` is either nobody (we hold the
/// authority — today's local commit, unchanged) or the peer that does. A
/// co-writer cannot commit this record itself by construction (its write
/// gate refuses every local commit), and it must not be able to: the record
/// is what stops a successor of its lane from re-minting its unpublished
/// tail, so its *monotonicity* and its *lane ownership* have to be enforced
/// by the node that owns the metadata (see `raise_alloc_lane`).
///
/// The reply — the durable frontier now in force — is deliberately
/// **discarded here**: the frontier this mount may mint below is established
/// once, by the lane OPEN in
/// [`crate::alloc_lane_grant::engage_allocator_lane`], which is the same
/// function that wires this sink and runs the open before any mint. Every
/// later raise asks for a bound strictly above the one it already holds, so
/// the authority's `max` is our own value and there is nothing to adopt.
pub fn routed_reserve_sink(
    meta: Arc<crate::meta_backend::RoutedMetaBackend>,
    volume_id: &str,
    writers: u16,
) -> LaneReserveSink {
    let vol_tag = crate::meta_backend::kv::block_refs::volume_tag(volume_id);
    Arc::new(move |lane: u16, upto: u64| {
        let meta = Arc::clone(&meta);
        Box::pin(async move {
            crate::meta_ship::publish::raise_alloc_lane(&meta, vol_tag, lane, writers, upto)
                .await
                .map(|_frontier| ())
        })
    })
}

/// **Commit one lane's reservation raise, monotonically** — the durable half
/// of the record, and the ONE place it is ever written.
///
/// * `requested_upto == 0` is the **OPEN**: a pure query that commits
///   nothing and answers the frontier this lane must resume at. It is what a
///   mount that runs no ownership-recovery walk of its own (a co-writer) uses
///   to learn where the set's live data ends;
/// * any greater value RAISES the record, and the answer is the frontier now
///   in force.
///
/// `floor` is the minimum a lane with **no record of its own at this width**
/// may resume at — the recovery rule's answer computed by the node that can
/// compute it (see [`durable_dense_frontier`] and
/// [`crate::alloc_lane_grant`]). It is deliberately not consulted when a
/// same-width record exists: that record already dominates every index its
/// lane could have minted, and re-flooring against a peer's progress would
/// drag this lane's frontier forward for nothing.
///
/// **The frontier only ever rises.** A caller asking for a lower bound is a
/// no-op, not an error: lowering a durable frontier is exactly how a
/// successor would re-mint a live peer's offsets, so no caller — least of
/// all a remote one — may ask for it.
pub async fn commit_lane_raise(
    meta: &crate::meta_backend::RoutedMetaBackend,
    vol_tag: u64,
    lane: u16,
    writers: u16,
    requested_upto: u64,
    floor: Option<u64>,
) -> Result<u64> {
    if writers == 0 || lane >= writers {
        let msg = format!(
            "refusing a lane reservation raise naming lane {lane} of {writers} writers, which is \
             not a lane"
        );
        log::error!("{msg}");
        return Err(SqueezefsError::InvalidOperation(msg));
    }
    let name = lane_record_name(vol_tag, lane);
    let existing = match crate::meta_backend::Metadata::getxattr(meta, 1, &name).await? {
        // An undecodable record refuses the raise rather than being
        // overwritten: a watermark we cannot read is a floor we cannot
        // honour, and writing over it would erase the evidence.
        Some(raw) => Some(LaneReservation::decode(&raw)?),
        None => None,
    };
    let base = match &existing {
        Some(rec) if rec.writers == writers => rec.reserved_upto,
        // A record at a DIFFERENT width floors every lane (the recovery
        // rule's clause 3 — under another `W` these indices belonged to
        // another lane), and the open floor still applies.
        Some(rec) => rec.reserved_upto.max(floor.unwrap_or(0)),
        None => floor.unwrap_or(0),
    };
    let new = base.max(requested_upto);
    let already_covered =
        matches!(&existing, Some(rec) if rec.writers == writers && rec.reserved_upto >= new);
    if requested_upto == 0 || already_covered || new == 0 {
        return Ok(new);
    }
    let rec = LaneReservation {
        writers,
        lane,
        reserved_upto: new,
    };
    crate::meta_backend::Metadata::setxattr(meta, 1, &name, &rec.encode()).await?;
    Ok(new)
}

/// The `floor` argument [`commit_lane_raise`] wants, computed by a node that
/// holds the metadata authority — and **`None` whenever this lane already
/// carries a same-width record**, which is what keeps the ledger scan below
/// off the per-grain path (it runs once per lane per era, on the OPEN).
///
/// `local_dense` is the caller's own live cursor for the volume (0 when it
/// has none): the derived dense frontier is the referenced-set complement,
/// and a mount that has been WRITING knows a fresher one than the ledger
/// does — an offset it minted and has not published yet is in neither.
pub async fn lane_open_floor(
    meta: &crate::meta_backend::RoutedMetaBackend,
    vol_tag: u64,
    lane: u16,
    writers: u16,
    local_dense: u64,
) -> Result<Option<u64>> {
    let part = match AppendPartition::new(writers, lane) {
        Ok(p) => p,
        Err(_) => {
            let msg = format!(
                "refusing to compute an allocation-lane floor for lane {lane} of {writers} \
                 writers, which is not a lane"
            );
            log::error!("{msg}");
            return Err(SqueezefsError::InvalidOperation(msg));
        }
    };
    // Its own record FIRST, and cheapest: when it exists at this width it
    // dominates every index this lane could have minted, so nothing below
    // runs — which is exactly what keeps the ledger scan off the per-grain
    // path (it costs one RAM-authoritative `getxattr` per raise, and the scan
    // only on the OPEN).
    let name = lane_record_name(vol_tag, lane);
    if let Some(raw) = crate::meta_backend::Metadata::getxattr(meta, 1, &name).await? {
        if LaneReservation::decode(&raw)?.writers == writers {
            return Ok(None);
        }
    }
    let records = load_lane_reservations_for_tag(meta, vol_tag).await?;
    let dense = durable_dense_frontier(meta, vol_tag)
        .await?
        .max(local_dense);
    Ok(Some(recover_lane_floor(&records, dense, part)))
}

/// The **derived dense frontier** for one data volume, from durable state
/// only: one past the highest block index the set's durable reference ledger
/// names (`TREE_BLOCK_REFS`, incompat bit 9 — the recovery rule's clause 1).
///
/// This is the number a mount that never walks the inode tree cannot compute
/// for itself, and it is why a co-writer's lane is OPENED by its authority:
/// with a cursor of 0 a lane alone would have it minting `w, w + W, …` from
/// the bottom of a device whose low blocks are live.
///
/// An **empty** ledger answers `0` rather than refusing, and the reason is
/// the §6.2 item 1 rule read carefully: an empty population is never
/// *authoritative*, so the caller composes this with everything else it
/// knows (the live cursor of the mount that walked, and every `alloc_lane:`
/// record — [`recover_lane_floor`]). On a genuinely empty volume set 0 is
/// the true answer.
pub async fn durable_dense_frontier(
    meta: &crate::meta_backend::RoutedMetaBackend,
    vol_tag: u64,
) -> Result<u64> {
    let mut frontier = 0u64;
    for kv in &meta.volumes {
        let refs = kv.block_ref_scan(vol_tag).await.map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "durable block-reference scan failed on {} while opening an allocation lane: {e}",
                kv.device_path().display()
            ))
        })?;
        for r in refs {
            frontier = frontier.max(r.block_idx.saturating_add(1));
        }
    }
    Ok(frontier)
}

/// Every reservation record this volume carries — the mount-time recovery
/// read, and the offline-probe shape (`getxattr` on ino 1 by name family).
///
/// Undecodable records refuse the WHOLE load rather than being skipped: a
/// silently-dropped watermark is exactly the lost floor this record exists
/// to be.
pub async fn load_lane_reservations(
    meta: &crate::meta_backend::RoutedMetaBackend,
    volume_id: &str,
) -> Result<Vec<LaneReservation>> {
    load_lane_reservations_for_tag(
        meta,
        crate::meta_backend::kv::block_refs::volume_tag(volume_id),
    )
    .await
}

/// [`load_lane_reservations`] by durable volume TAG — the form a node serving
/// a PEER's raise holds (the wire carries the tag, never a volume id string,
/// because the tag is the durable identity `TREE_BLOCK_REFS` keys on).
pub async fn load_lane_reservations_for_tag(
    meta: &crate::meta_backend::RoutedMetaBackend,
    vol_tag: u64,
) -> Result<Vec<LaneReservation>> {
    let mut out = Vec::new();
    for name in crate::meta_backend::Metadata::listxattr(meta, 1).await? {
        let Some((tag, _lane)) = parse_lane_record_name(&name) else {
            continue;
        };
        if tag != vol_tag {
            continue;
        }
        if let Some(raw) = crate::meta_backend::Metadata::getxattr(meta, 1, &name).await? {
            out.push(LaneReservation::decode(&raw)?);
        }
    }
    Ok(out)
}
