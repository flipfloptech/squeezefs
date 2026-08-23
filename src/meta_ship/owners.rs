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
use crate::membership::{member_id_matches, ClaimSet};
use crate::meta_backend::kv::backend::WriterClaim;
use crate::meta_backend::RoutedMetaBackend;
use arc_swap::ArcSwapOption;
use once_cell::sync::Lazy;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Per volume: the durable ASSIGNMENT SET this entry was derived
    /// against — `claim_set.owner` ∪ `successors` (§5.10, Issue 24).
    /// Empty on a volume no operator assigned, which is every volume of
    /// every set the field can mount before PR 7's verb.
    assignment: Vec<Vec<String>>,
    /// Per volume: the fail-closed latch. Set when a runtime
    /// re-derivation finds a holder the assignment set does not name;
    /// cleared only by installing a freshly derived map, which is what
    /// "until a re-derivation from a fresh read agrees" means.
    poisoned: Vec<AtomicBool>,
    /// Per volume: a PEER's volume that NOTHING appended to when this map
    /// was derived — its owner had not started yet (a cold fleet), or was
    /// down. The state is the derivation's, so it lives as long as the map
    /// does and a fresh derivation (a remount) is what re-reads it; the
    /// gauge is `meta_ship.volumes_peer_unclaimed`.
    unclaimed: Vec<bool>,
    /// The slots this node's authority homes, derived from the set's live
    /// slot map at build time — what the S4 lock plane installs so both
    /// planes answer one question.
    local_slots: Vec<u16>,
    /// The volume hosting **slot 0** — D20's set-authority anchor,
    /// derived from the same live slot map. KD-PV-6 pins slot 0
    /// non-migratable while the plane is armed, which is what keeps this
    /// index true for the map's whole lifetime.
    slot_0_volume: usize,
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
        // **No durable ASSIGNMENT stands behind a caller-named peer.**
        // This constructor is the co-writer arm's (every volume owned by
        // the authority it dialed) and the suites'; neither reads a
        // `claim_set.owner`, so the runtime conjunction has nothing to
        // read and [`reconcile_owner_from`] stays inert on these maps.
        // Recording the caller's assertion here instead would make a
        // co-writer poison its whole set the first time its authority
        // failed over — the per-mount-uuid-vs-durable-id mismatch is
        // exactly the rung-9 finding #3 class.
        let assignment: Vec<Vec<String>> = vec![Vec::new(); count];
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
        // No evidence was read here either, so no volume is KNOWN to lack
        // an appender: the degraded gauge belongs to the derivation.
        let unclaimed = vec![false; count];
        Ok(Self::build(routed, volume_owners, assignment, unclaimed))
    }

    /// The shared tail of both constructors: derive the local slot set
    /// from the live slot map and freeze the width.
    fn build(
        routed: &RoutedMetaBackend,
        volume_owners: Vec<Option<Arc<PeerOwner>>>,
        assignment: Vec<Vec<String>>,
        unclaimed: Vec<bool>,
    ) -> Arc<Self> {
        let slot_map = routed.slot_map_snapshot();
        let local_slots: Vec<u16> = slot_map
            .iter()
            .enumerate()
            .filter(|(_, &v)| volume_owners.get(v).map(|o| o.is_none()).unwrap_or(false))
            .map(|(slot, _)| slot as u16)
            .collect();
        let poisoned = volume_owners
            .iter()
            .map(|_| AtomicBool::new(false))
            .collect();
        let slot_0_volume = slot_map.first().copied().unwrap_or(0);
        Arc::new(Self {
            volume_owners,
            assignment,
            poisoned,
            unclaimed,
            local_slots,
            slot_0_volume,
            routing_width: routed.routing_width(),
        })
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
    ///
    /// Distinct by durable **identity**, not by endpoint: every consumer
    /// asks *which owners are there* and keys on `peer_id` (the fsck
    /// inode-plane fan-out dispatches per owner, `reader_staleness_bound_owners`
    /// counts them, the arm log names them). Deduping by endpoint
    /// collapsed two owners whose endpoints are both UNRESOLVED into one —
    /// reachable whenever more than one owner has not published yet, which
    /// is a three-owner fleet's cold start — and a collapsed owner is a
    /// shard never dispatched.
    pub fn peers(&self) -> Vec<Arc<PeerOwner>> {
        let mut out: Vec<Arc<PeerOwner>> = Vec::new();
        for peer in self.volume_owners.iter().flatten() {
            if !out
                .iter()
                .any(|p| member_id_matches(&p.peer_id, &peer.peer_id))
            {
                out.push(Arc::clone(peer));
            }
        }
        out
    }

    /// Volumes owned by this node.
    pub fn local_volumes(&self) -> usize {
        self.volume_owners.iter().filter(|o| o.is_none()).count()
    }

    /// The INDICES of the volumes this node appends to — the authority set
    /// [`MetaShipService::with_authority`] serves and refuses outside of.
    ///
    /// [`MetaShipService::with_authority`]: crate::meta_ship::MetaShipService::with_authority
    pub fn local_volume_set(&self) -> Vec<usize> {
        self.volume_owners
            .iter()
            .enumerate()
            .filter(|(_, o)| o.is_none())
            .map(|(v, _)| v)
            .collect()
    }

    /// Does this map name at least one PEER-owned volume — i.e. is a
    /// multi-owner plane armed? The predicate KD-PV-13's disarm and the
    /// §5.4 sweep's cross-owner refusals read.
    pub fn multi_owner(&self) -> bool {
        self.volume_owners.iter().any(|o| o.is_some())
    }

    /// **D20**: does this node append to the volume hosting slot 0 — i.e.
    /// is it the SET AUTHORITY? Answered from the derived local slot set,
    /// because `route_ino_width(1, W) == (0, 1)` pins ino 1 to slot 0 and
    /// KD-PV-6 pins slot 0 to its volume while the plane is armed.
    pub fn owns_slot_0(&self) -> bool {
        self.local_slots.contains(&0)
    }

    /// **The SET AUTHORITY** (D20): the owner of the volume hosting slot
    /// 0 — `None` when that is this node. What KD-PV-14's maintenance
    /// refusal names so an operator learns where to go instead of being
    /// told only where not to be.
    pub fn set_authority(&self) -> Option<&Arc<PeerOwner>> {
        self.owner_of_volume(self.slot_0_volume)
    }

    /// Is this volume's entry POISONED (§5.10)? A poisoned entry answers
    /// neither "local" nor "ship there": both would be a guess about who
    /// may append, and the fail-closed answer is a loud refusal.
    #[inline]
    pub fn is_poisoned(&self, v_idx: usize) -> bool {
        self.poisoned
            .get(v_idx)
            .map(|p| p.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// The `volumes_peer_unclaimed` gauge: peer-owned volumes that NOTHING
    /// appended to when this map was DERIVED.
    ///
    /// **Not a tripwire, and not a liveness monitor** — it is the honest
    /// name for a legitimate state (an owner that has not started, or one
    /// that is down), read once, at the instant this mount derived its
    /// map. On the documented bring-up order the SET AUTHORITY mounts
    /// first, so it reports `K − 1` for the life of that mount by
    /// construction; a peer mounting afterwards reports only the owners
    /// still missing. The live drift instrument is `squeezefs volume
    /// get-owners`, which reads the durable records at the moment it is
    /// asked.
    pub fn unclaimed_count(&self) -> u64 {
        self.unclaimed.iter().filter(|u| **u).count() as u64
    }

    /// The `owner_map_poisoned_volumes` gauge (**must stay 0**).
    pub fn poisoned_count(&self) -> u64 {
        self.poisoned
            .iter()
            .filter(|p| p.load(Ordering::Relaxed))
            .count() as u64
    }

    /// Is `id` a member of this volume's durable ASSIGNMENT SET — `owner`
    /// ∪ `successors`? Reading the SET rather than the singleton is what
    /// keeps a legitimate KD-PV-12 adoption from poisoning every peer's
    /// entry: the adoption deliberately writes nothing, so `owner` still
    /// names the dead predecessor while the live claim names the
    /// successor (§5.10, rev 3 Issue 24).
    pub fn assignment_names(&self, v_idx: usize, id: &str) -> bool {
        self.assignment
            .get(v_idx)
            .is_some_and(|names| names.iter().any(|n| member_id_matches(n, id)))
    }

    /// A REPLACEMENT for this map with `foreign` as its owner vector —
    /// the `PlacementTable` precedent: an installed table is immutable and
    /// a change republishes the whole thing rather than mutating one verbs
    /// are reading.
    ///
    /// Everything derived from the set (the assignment sets, the local
    /// slot vector, the slot-0 anchor, the frozen width) rides through
    /// unchanged, because none of them can move without a re-derivation.
    /// Every poison latch rides through too, except `cleared` — the one
    /// volume whose own fresh read just agreed.
    fn respun(&self, foreign: &[(usize, PeerOwner)], cleared: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            volume_owners: (0..self.volume_count())
                .map(|v| {
                    foreign
                        .iter()
                        .find(|(idx, _)| *idx == v)
                        .map(|(_, p)| Arc::new(p.clone()))
                })
                .collect(),
            assignment: self.assignment.clone(),
            poisoned: (0..self.volume_count())
                .map(|v| AtomicBool::new(Some(v) != cleared && self.is_poisoned(v)))
                .collect(),
            unclaimed: self.unclaimed.clone(),
            local_slots: self.local_slots.clone(),
            slot_0_volume: self.slot_0_volume,
            routing_width: self.routing_width,
        })
    }

    /// Latch the poison. `true` ⇔ this call set it (idempotent), which is
    /// what keeps the log line and the counter honest under a storm.
    fn poison(&self, v_idx: usize) -> bool {
        self.poisoned
            .get(v_idx)
            .map(|p| !p.swap(true, Ordering::Release))
            .unwrap_or(false)
    }
}

/// One volume's OWNERSHIP EVIDENCE — everything the derivation decides
/// over (KD-PV-3's conjunction, in one struct).
///
/// Public and separate from the gather for the [`SetAdmissionRequest`]
/// reason: a disagreeing or unattested peer volume cannot be part of an
/// OPEN set — `open_peer_owned` refuses it at the door and a plain
/// `KvMetaBackend::open` refuses its live foreign claim — so the refusal
/// arms are only pinnable over evidence a test builds directly.
///
/// [`SetAdmissionRequest`]: crate::partial_authority::SetAdmissionRequest
#[derive(Debug, Clone)]
pub struct VolumeOwnership {
    /// The durable `vol-{hex}` identity (KD-5), named in every refusal.
    pub vol_id: String,
    /// The volume's device path, also named in every refusal.
    pub path: PathBuf,
    /// The durable `claim_set` as the volume answers it — the ASSIGNMENT
    /// half. `None` / a non-durable projection = unassigned.
    pub claim_set: Option<ClaimSet>,
    /// The replayed `writer_claim` — the live EVIDENCE half.
    pub claim: Option<WriterClaim>,
    /// What the D0 Layer-B2 gate says about that claim — the gate's own
    /// classification (`KvMetaBackend::claim_standing`), never a second
    /// spelling of the dead-pid proof and the TTL window.
    ///
    /// The derivation needs it because `claim.is_some()` is not the same
    /// question as *is anything appending here*: a claim left by a holder
    /// this boot can prove dead reads `Reclaimable`, and the ladder and
    /// the peer door both admit that volume DEGRADED. Deciding it
    /// differently here would refuse the mount they just admitted.
    pub standing: crate::partial_authority::ClaimStanding,
    /// `true` ⇔ **this mount** holds this volume's claim, i.e. the D0
    /// ladder granted it here and this process is its appender. The
    /// strongest evidence there is for an own-mode volume, and the one a
    /// peer cannot forge.
    pub appended_locally: bool,
}

impl VolumeOwnership {
    /// The durable member id assigned to append here.
    fn assigned_owner(&self) -> Option<&str> {
        self.claim_set
            .as_ref()
            .filter(|s| s.durable)
            .and_then(|s| s.owner.as_deref())
    }

    /// `owner` ∪ `successors` — the assignment SET (§5.10).
    fn assignment_set(&self) -> Vec<String> {
        let Some(set) = self.claim_set.as_ref().filter(|s| s.durable) else {
            return Vec::new();
        };
        let mut names: Vec<String> = set.owner.iter().cloned().collect();
        names.extend(set.successors.iter().cloned());
        names
    }

    /// The live holder's DURABLE identity, resolved through the KD-PV-17
    /// attestation. `None` = silence: `WriterClaim.id` is a per-mount
    /// uuid, so an unattested claim names nobody the assignment can be
    /// read against.
    fn holder(&self) -> Option<&str> {
        let claim = self.claim.as_ref()?;
        self.claim_set.as_ref()?.resolve_holder(claim)
    }

    /// Is NOTHING appending to this volume — no claim, our own residue, or
    /// a holder this boot proved dead? The D0 gate's `Reclaimable`, which
    /// is the one reading under which a peer's volume is admitted DEGRADED
    /// rather than shipped to a live holder.
    fn no_live_appender(&self) -> bool {
        self.standing == crate::partial_authority::ClaimStanding::Reclaimable
    }
}

/// The installed map, or `None` = **solo**: this node owns every volume
/// and every verb takes today's path.
static OWNERS: Lazy<ArcSwapOption<OwnerMap>> = Lazy::new(ArcSwapOption::empty);

// ---------------------------------------------------------------------------
// The DERIVATION (KD-PV-3, §5.10): assignment ∧ evidence, fail-closed.
// ---------------------------------------------------------------------------

fn refuse(detail: String) -> SqueezefsError {
    log::error!("ownership map REFUSED: {detail}");
    SqueezefsError::InvalidOperation(format!(
        "the ownership map cannot be derived: {detail}. The map is DERIVED from the durable \
         assignment conjoined with the live claim evidence and it FAILS CLOSED (KD-PV-3): two \
         nodes with different maps is two appenders on one journal ring, or a volume nobody \
         appends to"
    ))
}

/// **Derive the ownership map from evidence** — the pure core.
///
/// Per volume, `claim_set.owner` (∪ `successors`) is the ASSIGNMENT and
/// the replayed `writer_claim` — resolved to its holder's durable id
/// through the KD-PV-17 attestation, or, for a volume this mount opened
/// `Own`, the D0 grant itself — is the EVIDENCE. They must agree:
///
/// | assignment | evidence | verdict |
/// |---|---|---|
/// | none, on EVERY volume | any | all-local (the shipped, unassigned set) |
/// | none, on SOME volume | any | **refuse** — a partial map has no coherent appender story |
/// | this node | this mount appends | LOCAL |
/// | a peer (∪ successors) | that peer holds the claim | ship to the HOLDER |
/// | a peer | **nothing appends there** | the peer's entry, **DEGRADED** — its owner is not up |
/// | this node | nothing appends there, and this mount did not open it `Own` | **refuse** — a peer entry naming ourselves ships every verb to our own endpoint |
/// | anything | an unattested LIVE claim | **refuse** — silence never adopts |
/// | anything | a holder the set does not name | **refuse** — the R5 divergence itself |
///
/// The DEGRADED row is the cold-start correction: "never adopt on silence"
/// forbids TAKING a volume this node is not assigned, and this row takes
/// nothing. Refusing it made an assigned set unmountable by construction —
/// at a cold fleet start nothing claims anything — and cost the whole
/// namespace over one absent owner, where the product's stated blast
/// radius (`docs/operations.md`, *ownership does not fail over*) is that
/// owner's subtree alone.
///
/// `endpoint_of` resolves a durable member id to where it serves. An
/// absent endpoint is announced, never refused: refusing would make the
/// FIRST node of a fleet unmountable, and a verb toward an empty endpoint
/// refuses loudly at the publish/ship site.
pub fn derive_owner_map_from(
    routed: &RoutedMetaBackend,
    node_id: &str,
    evidence: &[VolumeOwnership],
    endpoint_of: &dyn Fn(&str) -> Option<String>,
) -> Result<Arc<OwnerMap>> {
    let count = routed.volumes.len();
    if evidence.len() != count {
        return Err(refuse(format!(
            "the evidence covers {} volume(s) and the set has {count}",
            evidence.len()
        )));
    }
    if evidence.iter().all(|v| v.assigned_owner().is_none()) {
        // The shipped shape: no operator has run `volume set-owners` over
        // this set, so this node appends to every volume exactly as it
        // always has. Nothing is derived because nothing was assigned.
        return Ok(OwnerMap::build(
            routed,
            vec![None; count],
            vec![Vec::new(); count],
            vec![false; count],
        ));
    }

    let mut volume_owners: Vec<Option<Arc<PeerOwner>>> = Vec::with_capacity(count);
    let mut assignment: Vec<Vec<String>> = Vec::with_capacity(count);
    // Per volume: is this a peer's volume that NOTHING appends to — the
    // degraded state `meta_ship.volumes_peer_unclaimed` publishes.
    let mut unclaimed: Vec<bool> = Vec::with_capacity(count);
    for vol in evidence {
        let Some(owner) = vol.assigned_owner() else {
            return Err(refuse(format!(
                "metadata volume {} ({}) carries NO owner while another volume of this set \
                 does — the unassigned volume belongs to everyone and to nobody. Re-run \
                 `squeezefs volume set-owners` over the WHOLE set",
                vol.path.display(),
                vol.vol_id
            )));
        };
        let names = vol.assignment_set();
        let ours = names.iter().any(|n| member_id_matches(n, node_id));
        if vol.appended_locally {
            // The D0 ladder granted this mount the claim. That is the
            // strongest evidence a volume can carry — and if the record
            // assigns it elsewhere, this mount is the divergence.
            if !ours {
                return Err(refuse(format!(
                    "metadata volume {} ({}) is assigned to '{owner}', but this node holds its \
                     D0 claim and is appending to it. Assignment and evidence DISAGREE: either \
                     this mount took a volume that is not its own (mount it as \
                     `partial-authority` / `set-authority` so per-volume admission decides, or \
                     stop it), or the assignment is stale and must be re-run offline",
                    vol.path.display(),
                    vol.vol_id
                )));
            }
            volume_owners.push(None);
            assignment.push(names);
            unclaimed.push(false);
            continue;
        }
        if vol.no_live_appender() {
            // **DEGRADED, not refused.** Nothing appends to a volume this
            // node is not assigned: its owner has not started yet (every
            // volume of a cold fleet reads exactly this) or it is down.
            // The entry stays the ASSIGNED owner's — never ours, which is
            // the adoption KD-PV-3 forbids — and a verb about it refuses
            // loud at the ship site, the same not-yet-up path an owner
            // that has not published an endpoint already takes.
            if member_id_matches(owner, node_id) {
                return Err(refuse(format!(
                    "metadata volume {} ({}) is assigned to THIS node, nothing appends to it, \
                     and this mount did not open it `Own`. Shipping its verbs to the record's \
                     owner would ship them to ourselves, and taking its claim here would be an \
                     adoption outside the D0 ladder — so this fails closed. Mount as \
                     `set-authority` / `partial-authority` so per-volume admission opens it, or \
                     re-assign it offline",
                    vol.path.display(),
                    vol.vol_id
                )));
            }
            log::warn!(
                "ownership map: metadata volume {} ({}) is assigned to '{owner}' and NOTHING \
                 appends to it — that owner has not started yet, or it is down. Its entry is \
                 installed DEGRADED: this mount never appends there and never takes the claim, \
                 and every verb about it refuses loud at the ship site until its owner arrives \
                 (gauge `meta_ship.volumes_peer_unclaimed`)",
                vol.path.display(),
                vol.vol_id
            );
            let endpoint = endpoint_of(owner).unwrap_or_default();
            volume_owners.push(Some(Arc::new(PeerOwner::new(owner, endpoint))));
            assignment.push(names);
            unclaimed.push(true);
            continue;
        }
        let Some(claim) = vol.claim.as_ref() else {
            // The D0 gate says something appended here ({:?}) while no
            // `writer_claim` DECODES — unattributable bytes in the record
            // (the gate's own `StaleForeign(None)`), or two readings of
            // one volume that disagree. Either way nothing can name the
            // appender, and a map built on one reading alone is a guess.
            return Err(refuse(format!(
                "metadata volume {} ({}) is assigned to '{owner}' and carries no decodable \
                 `writer_claim`, yet the D0 gate classified it {:?}. Unattributable bytes name \
                 nobody, and this mount will not decide who appends to a volume it is not \
                 assigned: verify the holder is gone, then `squeezefs claim clear`",
                vol.path.display(),
                vol.vol_id,
                vol.standing
            )));
        };
        let Some(holder) = vol.holder() else {
            return Err(refuse(format!(
                "metadata volume {} ({}) carries a live `writer_claim` (id {}) whose holder \
                 resolves to nothing: the claim's own id is a per-mount uuid, not an enrollment \
                 identity, and no KD-PV-17 attestation names it. An unresolvable holder is \
                 SILENCE, and ownership never moves on silence",
                vol.path.display(),
                vol.vol_id,
                claim.id
            )));
        };
        if !names.iter().any(|n| member_id_matches(n, holder)) {
            return Err(refuse(format!(
                "metadata volume {} ({}) is assigned to '{owner}' (successors: {}), but the \
                 live claim is held by '{holder}'. Shipping this volume's verbs to a node the \
                 record does not entitle would make it an appender nobody assigned — re-assign \
                 offline, or stop the holder",
                vol.path.display(),
                vol.vol_id,
                if names.len() <= 1 {
                    "none".to_string()
                } else {
                    names[1..].join(", ")
                }
            )));
        }
        if member_id_matches(holder, node_id) {
            return Err(refuse(format!(
                "metadata volume {} ({}) names THIS node '{node_id}' as the holder of its live \
                 claim, but this mount did not take it — the attestation is a prior \
                 incarnation's residue. Refusing rather than adopting a claim we cannot prove \
                 is ours",
                vol.path.display(),
                vol.vol_id
            )));
        }
        // Verbs follow the HOLDER, not the record's `owner`: after a
        // KD-PV-12 adoption the record still names the dead predecessor.
        let endpoint = endpoint_of(holder).unwrap_or_default();
        if endpoint.is_empty() {
            log::warn!(
                "ownership map: metadata volume {} ({}) is appended to by '{holder}', whose \
                 endpoint is not published yet — its entry is installed WITHOUT one, so verbs \
                 on it refuse loud at the ship site until the membership census answers. This \
                 is the first-node-of-a-fleet shape and is announced, never refused",
                vol.path.display(),
                vol.vol_id
            );
        }
        volume_owners.push(Some(Arc::new(PeerOwner::new(holder, endpoint))));
        assignment.push(names);
        unclaimed.push(false);
    }
    Ok(OwnerMap::build(
        routed,
        volume_owners,
        assignment,
        unclaimed,
    ))
}

/// [`derive_owner_map_from`]'s gather: read every volume's durable claim
/// set and replayed claim from the OPEN set, then derive.
///
/// A volume this mount appends to is one that is not read-only — the D0
/// grant, which is what an `Own`-mode open holds and a `Peer`-mode open
/// deliberately does not.
pub async fn derive_owner_map(
    routed: &RoutedMetaBackend,
    node_id: &str,
    endpoint_of: &dyn Fn(&str) -> Option<String>,
) -> Result<Arc<OwnerMap>> {
    let mut evidence = Vec::with_capacity(routed.volumes.len());
    for vol in &routed.volumes {
        evidence.push(gather_volume(vol).await);
    }
    derive_owner_map_from(routed, node_id, &evidence, endpoint_of)
}

/// One volume's evidence, read from the open backend.
async fn gather_volume(
    vol: &Arc<crate::meta_backend::kv::backend::KvMetaBackend>,
) -> VolumeOwnership {
    VolumeOwnership {
        vol_id: vol.durable_volume_id(),
        path: vol.device_path().to_path_buf(),
        claim_set: ClaimSet::load(vol).await,
        claim: vol.read_writer_claim().await,
        standing: vol.claim_standing().await,
        appended_locally: !vol.is_read_only(),
    }
}

// ---------------------------------------------------------------------------
// The RUNTIME half (§5.10): poison, never adopt on silence.
// ---------------------------------------------------------------------------

/// Poison volume `v_idx`'s entry: verbs on it refuse loud from here until
/// a re-derivation from a fresh read agrees (installing a fresh map is
/// what clears it). `true` ⇔ this call latched it.
///
/// The gauge is `owner_map_poisoned_volumes` (**must stay 0**).
pub fn poison_volume(v_idx: usize, reason: &str) -> bool {
    let Some(map) = owner_map() else {
        return false;
    };
    if !map.poison(v_idx) {
        return false;
    }
    log::error!(
        "OWNERSHIP MAP POISONED on metadata volume {v_idx}: {reason}. Verbs on it now REFUSE \
         rather than ship to a node the durable assignment does not entitle to append there \
         (owner_map_poisoned_volumes — a must-stay-0 tripwire). Re-derive by remounting, or \
         re-assign offline with `squeezefs volume set-owners`"
    );
    true
}

/// Is `v_idx` poisoned? One relaxed load on an unarmed mount.
#[inline]
pub fn volume_poisoned(v_idx: usize) -> bool {
    owner_map().is_some_and(|m| m.is_poisoned(v_idx))
}

/// The `owner_map_poisoned_volumes` gauge.
pub fn poisoned_volumes() -> u64 {
    owner_map().map(|m| m.poisoned_count()).unwrap_or(0)
}

/// The `volumes_peer_unclaimed` gauge — how many of this set's peer-owned
/// volumes had no appender when this mount derived its map. 0 on every
/// unarmed mount, and on every fully-present fleet.
pub fn unclaimed_peer_volumes() -> u64 {
    owner_map().map(|m| m.unclaimed_count()).unwrap_or(0)
}

/// **The runtime conjunction** (§5.10): reconcile ONE volume's installed
/// entry against a fresh reading of its durable record.
///
/// `true` = the fresh read agrees (including a legitimate KD-PV-12
/// adoption, which moves the entry to the successor without poisoning —
/// the record's `owner` still names the dead predecessor by design). Any
/// disagreement — a holder outside the assignment set, an unattested
/// claim, no claim at all — POISONS instead of refusing, because a
/// running mount cannot refuse: the fail-closed answer at runtime is a
/// volume whose verbs stop.
pub fn reconcile_owner_from(v_idx: usize, fresh: &VolumeOwnership) -> bool {
    let Some(map) = owner_map() else {
        return true;
    };
    if map.owner_of_volume(v_idx).is_none() {
        // A volume this node appends to: its ownership cannot move under
        // a running mount without the D0 claim moving first, which the
        // guard's own heartbeat/fence path owns.
        return true;
    }
    if map.assignment.get(v_idx).is_none_or(|a| a.is_empty()) {
        // No durable assignment stands behind this entry (a co-writer's
        // map, a test constructor's): there is nothing for a fresh read
        // to disagree WITH, so the conjunction has no verdict to give.
        // Poisoning here would fail-stop a healthy co-writer the first
        // time its authority failed over.
        return true;
    }
    let Some(holder) = fresh.holder() else {
        // SILENCE. It never ADOPTS — nothing moves — but it is not the
        // poison predicate either (§5.10 poisons on "a holder the
        // assignment set does not name"): a successor's attestation is
        // written just after its claim commit, so an unresolved holder is
        // most often that window. The installed entry stands, loudly, and
        // a verb shipped to a peer that no longer holds the claim is
        // refused by that peer's own era gate.
        log::warn!(
            "ownership map: metadata volume {v_idx}'s live claim resolves to no durable \
             identity (the KD-PV-17 attestation is absent or is about an older claim), so this \
             re-derivation reaches no verdict. The installed entry STANDS — silence never moves \
             ownership — and the next read decides"
        );
        return true;
    };
    // The assignment SET, not the singleton: read from the FRESH record
    // when it carries one, else from the map the derivation installed.
    let fresh_names = fresh.assignment_set();
    let entitled = if fresh_names.is_empty() {
        map.assignment_names(v_idx, holder)
    } else {
        fresh_names.iter().any(|n| member_id_matches(n, holder))
    };
    if !entitled {
        poison_volume(
            v_idx,
            &format!(
                "the live claim is held by '{holder}', which the volume's durable assignment \
                 set does not name"
            ),
        );
        return false;
    }
    if map
        .owner_of_volume(v_idx)
        .is_some_and(|p| !member_id_matches(&p.peer_id, holder))
    {
        // A DECLARED successor adopted the volume (KD-PV-12): the record
        // still names the predecessor, so the entry follows the holder.
        log::warn!(
            "ownership map: metadata volume {v_idx} is now appended to by '{holder}', a member \
             of its durable assignment set — a declared successor adopted it (KD-PV-12). Verbs \
             follow the HOLDER; the record's `owner` deliberately still names its predecessor"
        );
        adopt_holder(v_idx, holder);
    }
    true
}

/// Republish the installed map with `v_idx` pointing at `holder` — the
/// KD-PV-12 adoption's runtime effect, done by REPLACING the table (the
/// `PlacementTable` precedent) rather than mutating one verbs are reading.
fn adopt_holder(v_idx: usize, holder: &str) {
    let Some(map) = owner_map() else {
        return;
    };
    let endpoint = map
        .owner_of_volume(v_idx)
        .map(|p| p.endpoint.clone())
        .unwrap_or_default();
    let mut foreign = map.foreign_assignments();
    for (idx, peer) in foreign.iter_mut() {
        if *idx == v_idx {
            *peer = PeerOwner::new(holder, endpoint.clone());
        }
    }
    // The adoption clears THIS volume's latch (its re-derivation agreed)
    // and carries every other volume's forward: a poisoned sibling stays
    // poisoned until its own fresh read agrees.
    arm_ownership(map.respun(&foreign, Some(v_idx)));
}

/// **Fill in the endpoints a derivation could not resolve** — the
/// first-node-of-a-fleet shape, closed rather than merely announced.
///
/// [`derive_owner_map_from`] installs a peer entry WITHOUT an endpoint
/// when nothing published one yet, and says so loudly. On a fleet that
/// window is not a corner: a partial authority cannot even be admitted
/// until the set authority's membership plane is live (rung 4), so the
/// SET AUTHORITY always derives its own map BEFORE its peers exist — and
/// without this pass its entries for them would stay endpoint-less for
/// the life of the mount, refusing every verb about a volume a peer
/// legitimately owns.
///
/// This moves nothing about WHO owns a volume: the peer identity and the
/// assignment set are untouched and only an EMPTY endpoint is filled, so
/// it cannot express an ownership change (that is a derivation's job, or
/// `reconcile_owner_from`'s). Returns how many entries it resolved; a
/// nonzero answer republished the table.
pub fn refresh_peer_endpoints(resolve: &dyn Fn(&str) -> Option<String>) -> usize {
    let Some(map) = owner_map() else {
        return 0;
    };
    let mut foreign = map.foreign_assignments();
    let mut filled = 0usize;
    for (v_idx, peer) in foreign.iter_mut() {
        if !peer.endpoint.is_empty() {
            continue;
        }
        let Some(endpoint) = resolve(&peer.peer_id).filter(|e| !e.is_empty()) else {
            continue;
        };
        log::warn!(
            "ownership map: metadata volume {v_idx}'s owner '{}' published its endpoint \
             ({endpoint}) — verbs on that volume now ship there instead of refusing",
            peer.peer_id
        );
        peer.endpoint = endpoint;
        filled += 1;
    }
    if filled > 0 {
        arm_ownership(map.respun(&foreign, None));
    }
    filled
}

/// Volumes whose owner is known but whose endpoint is not — what the
/// refresh pass above is still waiting for (`0` ⇒ nothing to wait for, so
/// no cadence needs to run at all).
pub fn unresolved_peer_endpoints() -> usize {
    let Some(map) = owner_map() else {
        return 0;
    };
    map.foreign_assignments()
        .iter()
        .filter(|(_, p)| p.endpoint.is_empty())
        .count()
}

/// **The era-relearn follow-up** (§5.10, extending the existing
/// `stale_term_refusals` / `era_relearns` pair rather than duplicating
/// it): this client learned a NEW era from `peer_id`, which means that
/// peer failed over — so every volume the map says it appends to is
/// re-derived from a FRESH read, and any that no longer agrees is
/// poisoned.
///
/// Fire-and-forget on the caller's side: the relearn happens on a refusal
/// path that must stay allocation-light and cannot await I/O, and the
/// reconcile's own verdict is published through the poison latch and its
/// gauge. A no-op on an unarmed mount, on a map with no entry for that
/// peer, and when no daemon router is installed (no live set to re-read).
pub fn note_era_relearn(peer_id: &str) {
    let Some(map) = owner_map() else {
        return;
    };
    let volumes = map.volumes_owned_by(peer_id);
    if volumes.is_empty() {
        return;
    }
    let Some(router) = super::DAEMON_VERB_ROUTER.load_full() else {
        return;
    };
    let routed = Arc::clone(router.inner());
    let peer = peer_id.to_string();
    crate::meta_exec::spawn_meta("owner_map_relearn_reconcile", async move {
        for v_idx in volumes {
            // A disagreeing verdict logged and poisoned inside the
            // reconcile; only a failed READ needs a word here.
            match reconcile_volume_owner(&routed, v_idx).await {
                Ok(_) => {}
                Err(e) => log::warn!(
                    "ownership map: re-reading metadata volume {v_idx} after '{peer}' relearned \
                     its era failed ({e}) — the installed entry stands until a read succeeds"
                ),
            }
        }
    });
}

/// Re-read volume `v_idx` from the live set and reconcile it — the era
/// relearn's follow-up (§5.10: *"a relearn on a volume whose live holder
/// is not in that volume's durable ASSIGNMENT SET poisons the entry …
/// until a re-derivation from a fresh read agrees"*).
pub async fn reconcile_volume_owner(routed: &RoutedMetaBackend, v_idx: usize) -> Result<bool> {
    let Some(vol) = routed.volumes.get(v_idx) else {
        return Err(SqueezefsError::InvalidOperation(format!(
            "ownership reconcile: volume {v_idx} is past this set's {} volumes",
            routed.volumes.len()
        )));
    };
    let fresh = gather_volume(vol).await;
    Ok(reconcile_owner_from(v_idx, &fresh))
}

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
    let unclaimed = map.unclaimed_count();
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
    if unclaimed > 0 {
        log::warn!(
            "ownership map: this set came up DEGRADED — {unclaimed} of the {foreign} peer-owned \
             volume(s) had NO appender at derivation, so those owners have not started yet or \
             are down. Their subtrees' verbs refuse loud at the ship site until they arrive; \
             everything else serves (gauge `meta_ship.volumes_peer_unclaimed`, and `squeezefs \
             volume get-owners` prints assignment beside evidence)"
        );
    }
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

/// **The routing decision, poison-aware** — the funnel every metadata verb
/// and every publish takes: `Ok(None)` = local, `Ok(Some(peer))` = ship,
/// `Err` = the entry is POISONED and the answer is a loud refusal (§5.10).
///
/// One relaxed load on an unarmed mount, which is every mount that ships.
#[inline]
pub fn route_volume(v_idx: usize) -> Result<Option<Arc<PeerOwner>>> {
    if !ownership_armed() {
        return Ok(None);
    }
    let Some(map) = OWNERS.load_full() else {
        return Ok(None);
    };
    if map.is_poisoned(v_idx) {
        return Err(SqueezefsError::InvalidOperation(format!(
            "metadata volume {v_idx}'s ownership entry is POISONED: a re-derivation found its \
             live claim held by a node the durable assignment set does not name, so this mount \
             refuses rather than appending to a volume it may not own or shipping to a node the \
             record does not entitle (KD-PV-3's fail-closed law; owner_map_poisoned_volumes). \
             Re-derive by remounting, or re-assign offline with `squeezefs volume set-owners`"
        )));
    }
    Ok(map.owner_of_volume(v_idx).cloned())
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
