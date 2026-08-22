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
        // A caller-named peer IS the assignment this map was built
        // against: there is no durable record behind it, so the poison
        // predicate reads exactly what the constructor asserted.
        let mut assignment: Vec<Vec<String>> = vec![Vec::new(); count];
        for (v_idx, peer) in foreign {
            if v_idx >= count {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "metadata owner map invalid: volume index {v_idx} is past this set's {count} \
                     volumes (ownership is per VOLUME — spec §6.10 R4 — so an index is the whole \
                     grain and a wrong one cannot be interpreted)"
                )));
            }
            assignment[v_idx] = vec![peer.peer_id.clone()];
            volume_owners[v_idx] = Some(Arc::new(peer));
        }
        Ok(Self::build(routed, volume_owners, assignment))
    }

    /// The shared tail of both constructors: derive the local slot set
    /// from the live slot map and freeze the width.
    fn build(
        routed: &RoutedMetaBackend,
        volume_owners: Vec<Option<Arc<PeerOwner>>>,
        assignment: Vec<Vec<String>>,
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
        Arc::new(Self {
            volume_owners,
            assignment,
            poisoned,
            local_slots,
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
/// | anything | nothing claims it | **refuse** — a set with a hole |
/// | anything | an unattested claim | **refuse** — silence never adopts |
/// | anything | a holder the set does not name | **refuse** — the R5 divergence itself |
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
        ));
    }

    let mut volume_owners: Vec<Option<Arc<PeerOwner>>> = Vec::with_capacity(count);
    let mut assignment: Vec<Vec<String>> = Vec::with_capacity(count);
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
            continue;
        }
        let Some(claim) = vol.claim.as_ref() else {
            return Err(refuse(format!(
                "metadata volume {} ({}) is assigned to '{owner}', but NOTHING claims it: that \
                 owner is dead, was never started, or the assignment is stale. This mount is \
                 not its assignee, so it must neither take the claim nor serve a set with a \
                 volume no node appends to (ownership does not fail over — declare a successor \
                 or re-assign offline)",
                vol.path.display(),
                vol.vol_id
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
    }
    Ok(OwnerMap::build(routed, volume_owners, assignment))
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
    let Some(holder) = fresh.holder() else {
        poison_volume(
            v_idx,
            "the live claim resolves to no durable identity (silence — the KD-PV-17 attestation \
             is absent or stale), and ownership never moves on silence",
        );
        return false;
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
    let fresh = Arc::new(OwnerMap {
        volume_owners: (0..map.volume_count())
            .map(|v| {
                foreign
                    .iter()
                    .find(|(idx, _)| *idx == v)
                    .map(|(_, p)| Arc::new(p.clone()))
            })
            .collect(),
        assignment: (0..map.volume_count())
            .map(|v| map.assignment.get(v).cloned().unwrap_or_default())
            .collect(),
        // The adoption clears THIS volume's latch (its re-derivation
        // agreed) and carries every other volume's forward: a poisoned
        // sibling stays poisoned until its own fresh read agrees.
        poisoned: (0..map.volume_count())
            .map(|v| AtomicBool::new(v != v_idx && map.is_poisoned(v)))
            .collect(),
        local_slots: map.local_slots.clone(),
        routing_width: map.routing_width,
    });
    arm_ownership(fresh);
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
