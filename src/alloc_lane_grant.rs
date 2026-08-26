//! **The allocation-lane GRANT** — DLM **S9**: who is lane `w` of `W`, how
//! that answer reaches a co-writer, and how a mount with no metadata
//! authority gets a durable reservation for the lane it was granted.
//!
//! This module is the seam between two landed designs that each deferred it
//! to the other:
//!
//! * `docs/design-mw-data-alloc-partition.md` (§9 item 1) built the partition
//!   and said *"`mount_partition()` is `SOLO` until an admission installs
//!   one, and nothing in the shipped mount path does. The natural source is
//!   S9's custody lease"*;
//! * `src/cowriter.rs` built the posture and **refused fresh allocation**,
//!   *"naming the data-plane allocation partition that owns the problem"*.
//!
//! Contracts: `tests/mw_cowriter_lane_tests.rs`. Operator surface:
//! `docs/operations.md` §Multi-writer co-writer mounts / §Multi-writer
//! capacity planning.
//!
//! # 1. The assignment: derived from the DURABLE claim set, never chosen
//!
//! [`LaneAssignment::derive`] reads the §6.2 item 7 `claim_set` record — the
//! record only an AUTHORITY can write, and the same record a co-writer's
//! admission rung 3 already stands on — and assigns:
//!
//! * **lane 0 to the authority** (so an authority with no enrolled co-writer
//!   is lane 0 of 1, which is `SOLO`, which installs *nothing*: the
//!   single-writer path is not "equivalent to" today's, it IS today's);
//! * **lane `1 + i` to the i-th enrolled WRITER member**, in the record's own
//!   sorted-by-id order (`upsert_writer_member` keeps it sorted, so the order
//!   is a property of the record rather than of who read it);
//! * **`writers` = (1 + enrolled writer members) rounded up to a power of
//!   two**, because that is what [`AppendPartition`] admits (see
//!   [`LaneAssignment::writers`] for the cost of the rounding). Reader members
//!   take no lane — they mint no offsets.
//!
//! The map is **injective** by construction (an index into a deduplicated
//! sorted list), which is the whole point: a knob would let two co-writers
//! claim one lane, and two writers minting in one residue class is exactly
//! the collision the partition exists to prevent.
//!
//! # 2. The lease is the carrier — and why the co-writer must not derive
//!
//! A co-writer could read the same durable record and compute the same map.
//! It must not, and the reason is a race rather than a trust question: the
//! roster GROWS. With `W = 2` a live co-writer mints `b % 2 == 1`; if a
//! second node read the record after an enrollment and derived `W = 3` it
//! would mint `b % 3 == 2`, and index 5 is in both classes. So the
//! **authority's arm-time snapshot is the single source**, distributed on the
//! custody lease ([`crate::data_grant::LeaseFrame::writer_lane`]), and a
//! member the snapshot does not name is refused at the JOIN. A width change
//! is therefore an act of a new authority **era**: every live writer's
//! residue class is fixed for the era, and the partition's own recovery rule
//! already handles a width change across mounts (its clause 3 — a record at a
//! foreign width floors every lane).
//!
//! # 3. The durable reservation: shipped, committed by the authority
//!
//! The partition's reservation record (`alloc_lane:{vol_tag}:{lane}` on ino
//! 1) is a **metadata commit**, and a co-writer holds no metadata authority —
//! that is its whole definition. It also cannot be skipped: the record is the
//! only thing that stops a successor of a lane from re-minting offsets a
//! crashed predecessor minted and never published (the zombie window derived
//! state cannot see, design record §3.1).
//!
//! So the raise **ships**, over S9's landed publish vocabulary
//! ([`crate::meta_ship::publish::raise_alloc_lane`]), and the AUTHORITY
//! commits it. Three properties make that safe, and all three live on the
//! authority:
//!
//! 1. **the client names a LANE, never a record** — the owner derives the
//!    record's name from `(vol_tag, lane)` itself, so the verb cannot address
//!    `writer_claim`, `claim_set` or `job:`;
//! 2. **the lane is checked against the assignment the owner made**
//!    ([`crate::data_grant::WriteCustodyOwner::check_lane_raise`]), under the
//!    lease epoch the owner minted for that member;
//! 3. **the frontier only ever rises** — monotonicity is the owner's, so no
//!    caller can ask for a LOWER frontier, which is the one shape that would
//!    let a successor re-mint a live peer's offsets.
//!
//! The alternative — the authority pre-reserving each member's frontier as
//! part of granting or renewing custody — was rejected on measurement
//! grounds, not taste: it puts a durable metadata commit on the HEARTBEAT
//! (the exact plane §6.5 item 3 measured at 455 journal beats/s and S6 exists
//! to keep off the journal), it makes reservations proportional to TIME
//! instead of to fresh blocks (the design's grain amortization is what keeps
//! the rewrite hot path at zero commits), and it either over-reserves blindly
//! on N volumes × M members or stalls a streaming writer at a renewal
//! boundary.
//!
//! # 4. The OPEN: the floor a mount that never walks cannot compute
//!
//! A co-writer runs **no** ownership-recovery walk and **no** durable-
//! reference seed (`main.rs` skips both: the walk's free-completing arm must
//! never run on a snapshot view), so its allocation cursor starts at 0. A
//! lane alone would then have it minting `w, w + W, …` from the bottom of a
//! device whose low blocks are LIVE.
//!
//! [`engage_allocator_lane`] therefore OPENS the lane before any mint:
//! `raise_alloc_lane` with `upto = 0` commits nothing and answers the
//! recovery rule's floor, computed on the authority from state only it has —
//! the durable reference ledger's dense frontier, its own live cursor for
//! that data volume, and every `alloc_lane:` record the volume carries. The
//! same function wires the reservation sink, in that order, so "a laned mount
//! opened its lane" is structural rather than a convention.

use crate::error::{Result, SqueezefsError};
use crate::membership::{ClaimSet, MemberRole};
use crate::meta_backend::kv::journal::AppendPartition;
use crate::meta_backend::RoutedMetaBackend;
use std::sync::Arc;

/// The widest partition the data plane can express — **derived, not chosen**:
/// [`AppendPartition`] is the one appender descriptor every partitioned
/// structure in the tree already runs on (§6.2 items 2/3/4 and the ino lanes),
/// and it admits at most
/// [`MAX_APPENDERS`](crate::meta_backend::kv::journal::MAX_APPENDERS) — so a
/// roster past this refuses at derivation, before any member is told a lane
/// the descriptor would reject.
pub const MAX_LANES: u16 = crate::meta_backend::kv::journal::MAX_APPENDERS;

/// Which node computes the floor a freshly engaged lane resumes at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneFloor {
    /// **This mount computes it** — the AUTHORITY's posture: its own
    /// ownership recovery (the durable-reference seed, or the derived layout
    /// walk) already established the dense frontier, so the floor is the
    /// landed rule over its own cursor plus the volume's records.
    Local,
    /// **The authority computes it** — a CO-WRITER's posture: it runs no
    /// recovery walk at all, so it must be TOLD where the set's live data
    /// ends (§4 of the module docs).
    Authority,
}

/// One era's data-plane allocation lane map: one lane per enrolled writer,
/// derived from the durable claim set by the authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneAssignment {
    authority_id: String,
    /// Enrolled writer members other than the authority, in the claim
    /// record's own sorted order. Index `i` holds lane `i + 1`.
    co_writers: Vec<String>,
}

impl LaneAssignment {
    /// **Derive the era's map** from the durable claim set(s) of a volume
    /// SET. `authority_id` is this authority's own claim-set member id (the
    /// membership owner's id — the entry `membership::arm_mount_membership`
    /// writes for itself).
    ///
    /// The union across volumes is deliberate: every volume of a set carries
    /// the same roster in the normal flow (`enroll_members` commits to each),
    /// and a set whose volumes disagree must produce ONE map — taking the
    /// union means a member named anywhere gets a lane, which is the
    /// leak-safe direction (a member with no lane cannot allocate; a member
    /// with a lane two nodes disagree about could collide, and the union
    /// cannot produce that because the map is derived once, here, and
    /// distributed on the lease).
    ///
    /// Refuses **loud** past [`MAX_LANES`].
    pub fn derive(authority_id: &str, sets: &[ClaimSet]) -> Result<Arc<Self>> {
        let mut co_writers: Vec<String> = Vec::new();
        for set in sets {
            for m in set.members.iter() {
                // Readers mint nothing, so they take no lane.
                if m.identity.role != MemberRole::Writer {
                    continue;
                }
                if m.identity.id == authority_id {
                    continue;
                }
                if !co_writers.contains(&m.identity.id) {
                    co_writers.push(m.identity.id.clone());
                }
            }
        }
        co_writers.sort();
        let members = co_writers.len() + 1;
        if members > MAX_LANES as usize {
            let msg = format!(
                "refusing a {members}-writer data-plane allocation partition: the append \
                 partition descriptor every partitioned structure runs on admits at most \
                 {MAX_LANES} appenders, so a set with more than {} enrolled co-writer(s) cannot \
                 be partitioned by residue class. Refusing here — before any member is told a \
                 lane — rather than handing out one the descriptor would reject at engagement",
                MAX_LANES - 1
            );
            log::error!("{msg}");
            return Err(SqueezefsError::InvalidOperation(msg));
        }
        Ok(Arc::new(Self {
            authority_id: authority_id.to_string(),
            co_writers,
        }))
    }

    /// The partition width every member of this era agrees on: the member
    /// count **rounded up to a power of two**, because that is what
    /// [`AppendPartition`] admits (an uneven split would leave some appender
    /// fewer than the two root-ledger slots the fallback property needs — the
    /// same law the partitioned journal, extent bitmap and root ledger run
    /// on).
    ///
    /// The rounding has an honest capacity cost: with three writers the width
    /// is 4, so a quarter of every data volume belongs to a lane nobody
    /// holds. It is not lost — it is *unreachable*, exactly like a live
    /// peer's lane, and it is published as `alloc_lane_stranded_bytes` and
    /// stated in `docs/operations.md` §Multi-writer capacity planning. The
    /// alternative (a width that is not a power of two) is not available: the
    /// partition descriptor is shared with the metadata plane's structures on
    /// purpose, so a volume can never hold two disagreeing notions of "who is
    /// writer 2".
    pub fn writers(&self) -> u16 {
        ((self.co_writers.len() + 1).next_power_of_two() as u16).max(1)
    }

    /// The authority's own claim-set member id.
    pub fn authority_id(&self) -> &str {
        &self.authority_id
    }

    /// The enrolled co-writer ids, in lane order (index `i` = lane `i + 1`).
    pub fn co_writers(&self) -> &[String] {
        &self.co_writers
    }

    /// `member_id`'s lane, or `None` when this era's map does not name it
    /// (the roster-growth rule — see the module docs §2).
    pub fn lane_of(&self, member_id: &str) -> Option<u16> {
        if member_id == self.authority_id {
            return Some(0);
        }
        self.co_writers
            .iter()
            .position(|id| id == member_id)
            .map(|i| (i + 1) as u16)
    }

    /// The partition `member_id` runs under (`None` ⇔ unnamed).
    pub fn partition_for(&self, member_id: &str) -> Option<AppendPartition> {
        AppendPartition::new(self.writers(), self.lane_of(member_id)?).ok()
    }

    /// The AUTHORITY's own partition — lane 0, and [`AppendPartition::SOLO`]
    /// when it has enrolled nobody, which is what keeps a single-writer
    /// mount's allocation literally the shipped path.
    pub fn authority_partition(&self) -> AppendPartition {
        AppendPartition::new(self.writers(), 0).unwrap_or(AppendPartition::SOLO)
    }
}

// ---------------------------------------------------------------------------
// The dense-frontier source: what an authority knows and a peer cannot
// ---------------------------------------------------------------------------

/// A live per-data-volume dense frontier lookup, by durable `vol_tag`: one
/// past the highest block index this mount's allocator has minted or
/// recovered. `None` ⇔ this mount routes no such volume.
pub type DenseFrontierSource = Arc<dyn Fn(u64) -> Option<u64> + Send + Sync>;

static FRONTIER_SOURCE: once_cell::sync::Lazy<arc_swap::ArcSwapOption<DenseFrontierSource>> =
    once_cell::sync::Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install this mount's live frontier source (the multi-writer authority's
/// arm). Without it a served OPEN falls back to durable state alone, which is
/// conservative in the wrong direction only if the ledger is behind the
/// cursor — hence the authority installs it.
pub fn install_frontier_source(src: DenseFrontierSource) {
    FRONTIER_SOURCE.store(Some(Arc::new(src)));
}

/// Uninstall it (disarm / unmount / test teardown).
pub fn uninstall_frontier_source() {
    FRONTIER_SOURCE.store(None);
}

/// This mount's live dense frontier for `vol_tag`, when it has one.
pub fn local_dense_frontier(vol_tag: u64) -> Option<u64> {
    let guard = FRONTIER_SOURCE.load();
    let src = guard.as_ref()?;
    src(vol_tag)
}

/// A frontier source over every data volume a [`crate::routing::BackendRouter`]
/// routes to.
pub fn router_frontier_source(backend: Arc<crate::routing::BackendRouter>) -> DenseFrontierSource {
    Arc::new(move |vol_tag: u64| {
        backend
            .lane_allocators()
            .into_iter()
            .find(|alloc| {
                crate::meta_backend::kv::block_refs::volume_tag(alloc.volume_id()) == vol_tag
            })
            .map(|alloc| alloc.highest_block_index())
    })
}

// ---------------------------------------------------------------------------
// Engagement
// ---------------------------------------------------------------------------

/// **Engage one allocator's granted lane**: the partition, its durable
/// reservation sink, and the floor it resumes at — in that order, which is
/// what makes "an offset is handed out only after its reservation is durable"
/// hold from the first mint.
///
/// A **solo** partition engages nothing at all and returns `Ok(())`: that is
/// the single-writer byte-identity property, and it is why an authority with
/// no enrolled co-writer cannot take a different code path than the shipped
/// one.
pub async fn engage_allocator_lane(
    alloc: &Arc<crate::block_allocator::BlockAllocator>,
    part: AppendPartition,
    meta: &Arc<RoutedMetaBackend>,
    floor: LaneFloor,
) -> Result<()> {
    alloc.engage_alloc_lanes(part)?;
    if alloc.lane_partition().is_none() {
        // Solo: nothing installed, nothing to reserve, nothing to open.
        return Ok(());
    }
    let vol_tag = crate::meta_backend::kv::block_refs::volume_tag(alloc.volume_id());
    alloc.set_lane_reserve_sink(crate::data_alloc_lane::routed_reserve_sink(
        Arc::clone(meta),
        alloc.volume_id(),
        part.writers(),
    ));
    if matches!(floor, LaneFloor::Authority) {
        // Rung 10 (residual 2): only a CO-WRITER needs the harvest — its
        // shipped frees land on the AUTHORITY's free list, which its own
        // funnel can never reach. The authority's lane-0 supply is its own
        // free list already, so wiring the sink there would ship a verb to
        // itself for offsets one probe away.
        alloc.set_lane_harvest_sink(crate::data_alloc_lane::routed_harvest_sink(
            alloc.volume_id(),
            part,
        ));
        // The ahead-of-stall refill task (design-free-grace-sustain §5.5,
        // PR 4): single-flight per volume BY CONSTRUCTION (one task per
        // laned co-writer engagement), off the allocation path — it
        // samples the claim rate each tick and harvests when this
        // allocator is OWED supply and its lane-reachable stock sits
        // below the derived watermark, so the refill RTT leaves the
        // writer's critical path. Tick = the 1 s checkpoint ceiling (the
        // same physics floor the elastic passes clamp to — nothing about
        // the loop's answers changes faster). Exits when the allocator
        // drops (the Weak upgrade fails) — nothing joins it, so
        // `detached::contain` owns its panic accounting (RES-8).
        let weak = Arc::downgrade(alloc);
        crate::meta_exec::spawn_meta_join("alloc_lane_ahead_refill", async move {
            let origin = std::time::Instant::now();
            loop {
                squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(
                    crate::meta_backend::kv::checkpoint::CHECKPOINT_MAX_AGE_MS as u64,
                ))
                .await;
                let Some(alloc) = weak.upgrade() else {
                    return;
                };
                let now_ms = origin.elapsed().as_millis() as u64;
                let _ = alloc.ahead_refill_tick(now_ms).await;
            }
        });
    }
    let floor_kind = floor;
    let floor = match floor {
        LaneFloor::Local => {
            // The landed rule over state this mount has: its own recovered
            // dense cursor plus every reservation record the volume carries.
            let records =
                crate::data_alloc_lane::load_lane_reservations_for_tag(meta, vol_tag).await?;
            crate::data_alloc_lane::recover_lane_floor(&records, alloc.highest_block_index(), part)
        }
        LaneFloor::Authority => {
            // The OPEN: this mount never walks the tree, so the authority
            // answers where the set's live data ends. Its answer already
            // composes the records with the dense frontier (it runs the same
            // `recover_lane_floor`), so nothing local is added — a stale local
            // read could only ever lower it.
            crate::meta_ship::publish::raise_alloc_lane(
                meta,
                vol_tag,
                part.writer_id(),
                part.writers(),
                0,
            )
            .await?
        }
    };
    alloc.install_lane_floor(floor);
    log::warn!(
        "data-plane allocation lane ENGAGED on volume '{}': lane {} of {}, resuming at block \
         {floor} ({}). Fresh allocation is this mount's residue class; frees stay lane-blind",
        alloc.volume_id(),
        part.writer_id(),
        part.writers(),
        match floor_kind {
            LaneFloor::Local => "floor from this mount's own ownership recovery",
            LaneFloor::Authority => "floor OPENED by the authority (this mount never walks)",
        }
    );
    Ok(())
}

/// Engage the AUTHORITY's own lane on every allocator this router owns, and
/// install its mount-wide partition.
///
/// Its floor is its own recovery's answer: an authority walks (or seeds from
/// the durable ledger), so it knows the dense frontier without asking anyone.
pub async fn engage_authority_lanes(
    part: AppendPartition,
    backend: &Arc<crate::routing::BackendRouter>,
    meta: &Arc<RoutedMetaBackend>,
) -> Result<()> {
    engage_router_lanes(part, backend, meta, LaneFloor::Local).await
}

/// Engage a CO-WRITER's granted lane on every allocator this router owns, and
/// install its mount-wide partition. Its floor is OPENED by the authority.
pub async fn engage_co_writer_lanes(
    part: AppendPartition,
    backend: &Arc<crate::routing::BackendRouter>,
    meta: &Arc<RoutedMetaBackend>,
) -> Result<()> {
    engage_router_lanes(part, backend, meta, LaneFloor::Authority).await
}

async fn engage_router_lanes(
    part: AppendPartition,
    backend: &Arc<crate::routing::BackendRouter>,
    meta: &Arc<RoutedMetaBackend>,
    floor: LaneFloor,
) -> Result<()> {
    if part.is_solo() {
        log::info!(
            "data-plane allocation partition NOT installed: this mount is the only writer of the \
             set (lane 0 of 1), so allocation is the shipped path — literally, not equivalently"
        );
        return Ok(());
    }
    // Every volume of the set must express a multi-writer data plane: a
    // lane-unaware mount of a partitioned set mints DENSE offsets across
    // every peer's lane and ignores the frontier a live peer published.
    for vol in &meta.volumes {
        if vol.superblock().features_incompat
            & crate::meta_backend::kv::superblock::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA
            == 0
        {
            let msg = format!(
                "refusing to engage allocation lane {} of {}: metadata volume {} does not carry \
                 incompat bit 11 (multi-writer data plane), so its recovery paths are not \
                 expressed for more than one data-plane writer. Nothing stamps it today (ruling \
                 D9) — the Phase-8 reformat window does, via \
                 superblock::set_multi_writer_data_bit",
                part.writer_id(),
                part.writers(),
                vol.device_path().display()
            );
            log::error!("{msg}");
            return Err(SqueezefsError::InvalidOperation(msg));
        }
    }
    crate::data_alloc_lane::install_mount_partition(part)?;
    for alloc in backend.lane_allocators() {
        engage_allocator_lane(&alloc, part, meta, floor).await?;
    }
    Ok(())
}
