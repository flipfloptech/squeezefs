//! The **ownership plane** — who owns a metadata volume, answered
//! lock-free on every verb's routing decision (DLM stage **S8**; spec
//! §6.7 decisions 1/2, §6.10 **R4**).
//!
//! # Granularity is the VOLUME, and that is a structural choice
//!
//! Spec §6.10 R4 states it: *"Ownership granularity is the volume, so the
//! modeled load needs ~46 metadata volumes, each with its own claim, PR
//! registration, checkpoint task, journal ring, and node cache."* This
//! module's API therefore takes **per-volume** assignments, which makes an
//! intra-volume split *unrepresentable* rather than merely refused. The
//! reason is durable and it is not S8's to fix:
//!
//! * one journal ring, one A/B extent bitmap, one 32-slot root ledger per
//!   volume (§6.2 items 2/3/4). Bit 8's partitioned append expresses all
//!   three for N appenders — but it is **built and NOT stamped** (ruling
//!   D9), and nothing assigns appender ids;
//! * one **node cache** per volume, and the coherence work's own named
//!   residual is exactly the missing third gate state ("reader for
//!   structure, appender for my own leaves" — `kv/revalidate.rs`).
//!
//! With one owner per volume, every one of those structures keeps exactly
//! one appender, so S8 needs **no on-disk change at all** — the honest
//! reason this stage stamps no incompat bit.
//!
//! # Where the map comes from
//!
//! Nothing here invents a directory service. Each volume already carries
//! a durable `writer_claim` record naming its holder and (since S2) its
//! durable `term`: **the D0 claim holder IS the owner**, and a peer
//! discovers ownership by reading the claims it already reads for the
//! guard ladder. This module is the runtime *cache* of that answer plus
//! the endpoint to reach it at (DISC-1 supplies endpoints from the
//! `client:{uuid}` records).
//!
//! # The fast path
//!
//! [`ownership_armed`] is one **relaxed load** of a flag word. An unarmed
//! mount — every mount that ships today — never touches the arc-swapped
//! table at all, so the routing decision on the local path costs that one
//! load and nothing else. An armed mount pays one `ArcSwapOption::load`
//! plus a `Vec` index: lock-free, wait-free, allocation-free.
//!
//! # One truth for both planes
//!
//! Arming publishes the derived **local slot set** into the S4 lock plane
//! ([`crate::dlm_slot`]) in the same call, because spec §6.7 decision 2's
//! whole point is that the lock master and the metadata authority are the
//! same process. A foreign volume's slots leave the lock plane's local
//! set at the same instant they leave the metadata plane's, so no window
//! exists in which one plane would grant what the other ships away.

use super::{OWNERSHIP_ARMED, OWNERSHIP_ARMS};
use crate::error::{Result, SqueezefsError};
use crate::meta_backend::RoutedMetaBackend;
use arc_swap::ArcSwapOption;
use once_cell::sync::Lazy;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// One peer that owns metadata volumes of this set.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerOwner {
    /// The peer's cluster identity (its mount uuid — the same id the
    /// `client:{uuid}` registration and the storage-trust proof carry).
    pub peer_id: String,
    /// `host:port` of the peer's cluster-wire RPC listener, as published
    /// by DISC-1.
    pub endpoint: String,
}

impl PeerOwner {
    /// A peer named by identity and endpoint.
    pub fn new(peer_id: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            peer_id: peer_id.into(),
            endpoint: endpoint.into(),
        }
    }
}

/// Which node owns each metadata volume of one set: `None` = **this**
/// node (the local, unshipped case).
///
/// Immutable once built and published behind an `ArcSwapOption`, so a
/// remastering event replaces the whole table (the `PlacementTable`
/// precedent) rather than mutating one that verbs are reading.
#[derive(Debug)]
pub struct OwnerMap {
    /// Per volume index: the owning peer, or `None` for local.
    volume_owners: Vec<Option<Arc<PeerOwner>>>,
    /// The slots this node's authority homes, derived from the set's live
    /// slot map at build time — what the S4 lock plane installs so both
    /// planes answer one question.
    local_slots: Vec<u16>,
    /// The set's frozen routing width, recorded so a re-arm can be
    /// detected against a set whose width differs (a different set).
    routing_width: u64,
}

impl OwnerMap {
    /// Build a map from **per-volume** assignments over `routed`'s live
    /// slot map: every volume index NOT named is local.
    ///
    /// Refuses loud on a volume index the set does not have — a map that
    /// names a volume this node cannot route to would ship verbs into a
    /// void.
    pub fn for_volumes(
        routed: &RoutedMetaBackend,
        foreign: Vec<(usize, PeerOwner)>,
    ) -> Result<Arc<Self>> {
        let count = routed.volumes.len();
        let mut volume_owners: Vec<Option<Arc<PeerOwner>>> = vec![None; count];
        for (v_idx, peer) in foreign {
            if v_idx >= count {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "metadata owner map invalid: volume index {v_idx} is past this set's {count} \
                     volumes (ownership is per VOLUME — spec §6.10 R4 — so an index is the whole \
                     grain and a wrong one cannot be interpreted)"
                )));
            }
            volume_owners[v_idx] = Some(Arc::new(peer));
        }
        let slot_map = routed.slot_map_snapshot();
        let local_slots: Vec<u16> = slot_map
            .iter()
            .enumerate()
            .filter(|(_, &v)| volume_owners.get(v).map(|o| o.is_none()).unwrap_or(false))
            .map(|(slot, _)| slot as u16)
            .collect();
        Ok(Arc::new(Self {
            volume_owners,
            local_slots,
            routing_width: routed.routing_width(),
        }))
    }

    /// Volumes this map covers.
    pub fn volume_count(&self) -> usize {
        self.volume_owners.len()
    }

    /// The owner of `v_idx`, or `None` when this node owns it (or the
    /// index is past the map — a stale index is never silently adopted as
    /// somebody's).
    #[inline]
    pub fn owner_of_volume(&self, v_idx: usize) -> Option<&Arc<PeerOwner>> {
        self.volume_owners.get(v_idx).and_then(|o| o.as_ref())
    }

    /// The slots this node's authority homes (the S4 lock plane's view of
    /// the same decision).
    pub fn local_slots(&self) -> &[u16] {
        &self.local_slots
    }

    /// The frozen routing width the map was derived over.
    pub fn routing_width(&self) -> u64 {
        self.routing_width
    }

    /// The volumes owned by the peer whose identity is `peer_id` — the
    /// rung-14 placement policy's candidate inversion (the
    /// fleet-of-authorities question: "does this shipping client own a
    /// volume I could home its slots on?"). Empty on every shipped fleet
    /// today, which is what keeps the migration policy structurally dark.
    pub fn volumes_owned_by(&self, peer_id: &str) -> Vec<usize> {
        self.volume_owners
            .iter()
            .enumerate()
            .filter(|(_, o)| o.as_ref().is_some_and(|p| p.peer_id == peer_id))
            .map(|(v, _)| v)
            .collect()
    }

    /// The map's foreign assignments, in [`Self::for_volumes`]'s input
    /// form — what a re-arm over a fresh slot map preserves.
    pub fn foreign_assignments(&self) -> Vec<(usize, PeerOwner)> {
        self.volume_owners
            .iter()
            .enumerate()
            .filter_map(|(v, o)| o.as_ref().map(|p| (v, (**p).clone())))
            .collect()
    }

    /// Distinct peers named by this map.
    pub fn peers(&self) -> Vec<Arc<PeerOwner>> {
        let mut out: Vec<Arc<PeerOwner>> = Vec::new();
        for peer in self.volume_owners.iter().flatten() {
            if !out.iter().any(|p| p.endpoint == peer.endpoint) {
                out.push(Arc::clone(peer));
            }
        }
        out
    }

    /// Volumes owned by this node.
    pub fn local_volumes(&self) -> usize {
        self.volume_owners.iter().filter(|o| o.is_none()).count()
    }
}

/// The installed map, or `None` = **solo**: this node owns every volume
/// and every verb takes today's path.
static OWNERS: Lazy<ArcSwapOption<OwnerMap>> = Lazy::new(ArcSwapOption::empty);

/// Install `map` as this mount's ownership plane, publishing the derived
/// local slot set into the S4 lock plane in the same call (one truth for
/// both planes — spec §6.7 decision 2).
///
/// **No production issuer yet, deliberately.** The arm belongs to the
/// multi-writer mount, which S9 owns: a mount that ships metadata but
/// cannot ship *data* custody is not a product — the data path refuses a
/// foreign-home lease loud (S4's own refusal), which is correct and is
/// exactly the gap S9 closes. It is public and reachable so the shipped
/// and refused behaviours are tested rather than commented, the
/// `dlm_slot::test_set_local_slots` / `dlm::test_arm_cw_mode` precedent.
///
/// **Assumption S9/S10 must honour:** a slot migration while armed moves
/// a slot between volumes, which changes the derived local slot set — the
/// migration must re-arm (publish a fresh map) at its cutover, exactly as
/// it already republishes the routing table.
pub fn arm_ownership(map: Arc<OwnerMap>) {
    let peers = map.peers();
    let local = map.local_volumes();
    let foreign = map.volume_count() - local;
    crate::dlm_slot::install_local_slots(Some(map.local_slots()));
    OWNERS.store(Some(Arc::clone(&map)));
    // Release: a thread that observes ARMED must observe the table it
    // names (and every reader loads the table with Acquire semantics
    // through `ArcSwapOption`).
    OWNERSHIP_ARMED.store(true, Ordering::Release);
    OWNERSHIP_ARMS.fetch_add(1, Ordering::Relaxed);
    log::warn!(
        "metadata function shipping ARMED (DLM S8): {local} of {} volumes owned locally, \
         {foreign} shipped to {} peer(s) {:?}; {} slots home locally. Metadata verbs on a \
         foreign volume now travel to its owner; the DATA plane still refuses foreign-home \
         custody loud (S9 ships that half)",
        map.volume_count(),
        peers.len(),
        peers.iter().map(|p| &p.peer_id).collect::<Vec<_>>(),
        map.local_slots().len(),
    );
}

/// Restore solo mode in both planes: every volume local, every slot
/// homed here, nothing shipped. Placement state (rung 14) dies with the
/// plane — assignments and policy evidence are meaningless without an
/// ownership map to invert.
pub fn disarm_ownership() {
    if OWNERSHIP_ARMED.swap(false, Ordering::AcqRel) {
        OWNERS.store(None);
        crate::dlm_slot::install_local_slots(None);
        super::placement::clear_runtime_state();
        log::warn!("metadata function shipping DISARMED (DLM S8): solo authority restored");
    }
}

/// Re-publish the ownership plane over `routed`'s LIVE slot map with the
/// standing foreign assignments preserved — the migration-while-armed law
/// this module's [`arm_ownership`] docs state: a slot migration changes
/// the derived local slot set, so the cutover must re-arm exactly as it
/// republishes the routing table. A no-op on an unarmed mount.
pub fn rearm_ownership(routed: &RoutedMetaBackend) -> Result<()> {
    let Some(map) = owner_map() else {
        return Ok(());
    };
    let fresh = OwnerMap::for_volumes(routed, map.foreign_assignments())?;
    arm_ownership(fresh);
    Ok(())
}

/// Is an ownership plane installed? **One relaxed load** — the fast path
/// every fencing read and every verb takes.
#[inline]
pub fn ownership_armed() -> bool {
    OWNERSHIP_ARMED.load(Ordering::Relaxed)
}

/// The installed map (`None` = solo).
#[inline]
pub fn owner_map() -> Option<Arc<OwnerMap>> {
    if !ownership_armed() {
        return None;
    }
    OWNERS.load_full()
}

/// The owner of the metadata volume at `v_idx`: `None` = local.
#[inline]
pub fn owner_of_volume(v_idx: usize) -> Option<Arc<PeerOwner>> {
    owner_map().and_then(|m| m.owner_of_volume(v_idx).cloned())
}

/// Does this node own the metadata volume at `v_idx`?
#[inline]
pub fn owns_volume(v_idx: usize) -> bool {
    match owner_map() {
        None => true,
        Some(map) => map.owner_of_volume(v_idx).is_none(),
    }
}

/// **The mint constraint** (spec §6.10 R4 + §6.2 items 2/3/4): a create
/// executing on this node must mint its child inode in a volume this node
/// **owns**, because minting into a foreign volume would append to a tree
/// whose journal ring, extent bitmap and root ledger belong to another
/// writer.
///
/// `picked` is the placement engine's choice (health + balance); `parent`
/// is the parent inode's volume, which the executing owner owns by
/// construction (the verb was shipped to the parent's owner). Unarmed:
/// `picked`, verbatim, after one relaxed load — the shipped mount's path
/// is bit-identical.
///
/// This is the Lockify "self-designating creator" the spec calls *free
/// here*: inos are monotonic and never reused, so the creating node can
/// mint locally with no directory round trip.
#[inline]
pub fn constrain_mint_volume(picked: usize, parent: usize) -> usize {
    if !ownership_armed() {
        return picked;
    }
    if owns_volume(picked) {
        return picked;
    }
    super::MINT_REDIRECTS.fetch_add(1, Ordering::Relaxed);
    log::debug!(
        "S8 mint redirect: volume {picked} is owned by a peer — minting into the parent's \
         volume {parent} instead (one appender per volume, §6.2 items 2/3/4)"
    );
    parent
}
