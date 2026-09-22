//! **Symmetric PR 13b — the record-level metanode ship** (design
//! `docs/design-symmetric-metadata.md` §5.10, the row "`write` to a
//! FOREIGN-owned file"; §5.1.4's "a mutation of a foreign slot ships to its
//! holder — the metanode arm"; PR 13's flip blocker, record §4.4z).
//!
//! Under the ARMED symmetric plane the OWNERSHIP unit is the slot (§5.1.1):
//! a `setattr` / `setxattr` / `removexattr` / layout publish of an object
//! whose forest slot ANOTHER appender leases is applied by that appender
//! — under ITS lease and commit door, in ITS ring, recalling the object's
//! tokens through its own conveyor pass — and this mount SHIPS the verb
//! there. The record-level verbs travel as the S8 `MetaCall`s they already
//! are (`ship_meta_call`, the striping verbs' wire); the layout publish
//! rides the S9 publish plane with the per-HOLDER custody lease PR 9's arm
//! dialed (`meta_ship::publish`). The NAMESPACE verbs keep PR 6's intent
//! arm — the S8 router's `create` would bypass the creator's-rotor mint.
//!
//! What this module owns: the ONE resolver every record-level site asks
//! ([`record_home`] — tree 0's lessee through PR 6's `step_home`, the
//! holder's endpoint bound on demand), the ship with its bounded slot-moved
//! re-resolve ([`ship_record_verb`]), the holder-side served note (the
//! dominance window + the served ledger), and the family's ledger. An
//! UNARMED volume pays one `Option` test; inside a served verb this mount
//! IS the holder and every site applies locally.
//!
//! The one refusal left: a holder this mount cannot reach (its endpoint
//! unpublished — a joiner whose ladder has not reached rung 7; a dead
//! member's, until PR 10's recovery re-leases its slots) is the
//! RETRYABLE class [`RefusalClass::HolderUnreachable`] (`EAGAIN`) — never
//! PR 13's interim `EREMOTE`, which named an arm that did not exist.

use super::{Ino, RoutedMetaBackend};
use crate::error::{RefusalClass, Result, SqueezefsError};
use crate::fuse_client::LatencyHistogram;
use crate::meta_backend::crossvol_tx::{self, StepHome};
use crate::meta_ship::{MetaCall, MetaReply};
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Record-level verbs this mount SHIPPED to a slot holder (one per logical
/// verb; a slot-moved re-resolve does not count twice).
static RECORD_SHIPS: AtomicU64 = AtomicU64::new(0);
/// Record-level verbs this mount APPLIED for a peer under its own lease
/// (the holder side). Across a fleet Σ `record_ships` ≡ Σ `record_served`
/// + Σ `record_refusals` + in flight.
static RECORD_SERVED: AtomicU64 = AtomicU64::new(0);
/// Served record-level verbs this holder REFUSED with anything but the
/// slot-moved class (the door's `SlotBusy` is the requester's re-resolve,
/// its own row) — **must stay 0** on a healthy fleet.
static RECORD_REFUSALS: AtomicU64 = AtomicU64::new(0);
/// Shipped verbs re-resolved ONCE after the holder's slot-moved refusal
/// (defects 28/29's arm on this wire) — a handover racing a ship, legal.
static RECORD_SHIP_REDIRECTS: AtomicU64 = AtomicU64::new(0);
/// Verbs refused `EAGAIN` because the slot's holder has no endpoint bound
/// on this mount ([`RefusalClass::HolderUnreachable`]).
static RECORD_UNREACHABLE: AtomicU64 = AtomicU64::new(0);
/// Layout publishes this mount SHIPPED to a slot holder (the S9 plane
/// re-keyed by slot holder — `meta_ship::publish` counts here).
static FOREIGN_PUBLISH_SHIPS: AtomicU64 = AtomicU64::new(0);
/// Layout publishes this holder SERVED for a peer under the armed plane.
static FOREIGN_PUBLISH_SERVED: AtomicU64 = AtomicU64::new(0);
/// Layout publishes this mount shipped to a slot holder whose terminal
/// outcome was a FAILURE — refused at the holder, or the wire failed past
/// the witnessed resend (review round 1, Issue 4; a slot-moved redirect is
/// not terminal). `ships` counts the ones that LANDED, so at rest
/// `foreign_publish_ships ≡ foreign_publish_served`; a refusal is the
/// writer's retry class and lands nowhere.
static FOREIGN_PUBLISH_REFUSALS: AtomicU64 = AtomicU64::new(0);
/// Served mutations for which this HOLDER's FUSE layer invalidated its
/// own view of the object — the router's RAM entry, the kernel's attrs
/// (and pages for a publish) — over the classical sideband
/// (`served_mutation_invals`; ≡ the served verbs + publishes on a
/// mounted holder, 0 in-process).
static SERVED_MUTATION_INVALS: AtomicU64 = AtomicU64::new(0);
/// Of those, the `FUSE_NOTIFY_PRUNE` pushes — a writeback-cache kernel
/// (uapi ≥ 7.45) told to evict the unreferenced inode so its next lookup
/// adopts the served size / times (`served_mutation_prunes`).
static SERVED_MUTATION_PRUNES: AtomicU64 = AtomicU64::new(0);
/// Served mutations whose object's layout entry the HOLDER's sink KEPT
/// because it was DIRTY — the holder's own acked, unsaved write (review
/// round 1, Issue 1; `served_mutation_dirty_kept`). A record verb beside
/// it is legal; a served PUBLISH beside it is the tripwire
/// `served_publish_over_dirty_entry`.
static SERVED_MUTATION_DIRTY_KEPT: AtomicU64 = AtomicU64::new(0);
/// Served mutations whose layout-entry discard the holder's sink SKIPPED
/// because the ino's (3.5) stripe was held — a write or a persist of it in
/// flight (`served_mutation_discard_skipped`; the entry's 1 s `cached_at`
/// TTL expires it instead — no epoch step runs at the holder).
static SERVED_MUTATION_DISCARD_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Phases of one shipped record-level verb (`record_ship_phase_ns`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum RecordShipPhase {
    /// tree 0's lessee → the holder's endpoint (the on-demand bind
    /// included).
    Resolve = 0,
    /// The S8 round trip.
    Rtt = 1,
    /// The whole ship (resolve + rtt + the bounded re-resolve).
    Total = 2,
}

const PHASES: usize = 3;
const PHASE_NAMES: [&str; PHASES] = ["resolve", "rtt", "total"];

static PROF: Lazy<[LatencyHistogram; PHASES]> =
    Lazy::new(|| std::array::from_fn(|_| LatencyHistogram::default()));

#[inline]
fn phase_record(phase: RecordShipPhase, t0: Instant) {
    PROF[phase as usize].record(t0.elapsed());
}

/// One snapshot of the family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecordShipStats {
    pub record_ships: u64,
    pub record_served: u64,
    pub record_refusals: u64,
    pub record_ship_redirects: u64,
    pub record_unreachable: u64,
    pub foreign_publish_ships: u64,
    pub foreign_publish_served: u64,
    pub foreign_publish_refusals: u64,
    pub served_mutation_invals: u64,
    pub served_mutation_prunes: u64,
    pub served_mutation_dirty_kept: u64,
    pub served_mutation_discard_skipped: u64,
}

/// Read the family.
pub fn stats() -> RecordShipStats {
    RecordShipStats {
        record_ships: RECORD_SHIPS.load(Ordering::Relaxed),
        record_served: RECORD_SERVED.load(Ordering::Relaxed),
        record_refusals: RECORD_REFUSALS.load(Ordering::Relaxed),
        record_ship_redirects: RECORD_SHIP_REDIRECTS.load(Ordering::Relaxed),
        record_unreachable: RECORD_UNREACHABLE.load(Ordering::Relaxed),
        foreign_publish_ships: FOREIGN_PUBLISH_SHIPS.load(Ordering::Relaxed),
        foreign_publish_served: FOREIGN_PUBLISH_SERVED.load(Ordering::Relaxed),
        foreign_publish_refusals: FOREIGN_PUBLISH_REFUSALS.load(Ordering::Relaxed),
        served_mutation_invals: SERVED_MUTATION_INVALS.load(Ordering::Relaxed),
        served_mutation_prunes: SERVED_MUTATION_PRUNES.load(Ordering::Relaxed),
        served_mutation_dirty_kept: SERVED_MUTATION_DIRTY_KEPT.load(Ordering::Relaxed),
        served_mutation_discard_skipped: SERVED_MUTATION_DISCARD_SKIPPED.load(Ordering::Relaxed),
    }
}

/// Count one kernel invalidation pushed for a served mutation, and
/// whether a prune rode with it.
pub fn note_served_mutation_inval(pruned: bool) {
    SERVED_MUTATION_INVALS.fetch_add(1, Ordering::Relaxed);
    if pruned {
        SERVED_MUTATION_PRUNES.fetch_add(1, Ordering::Relaxed);
    }
}

/// **The holder's layout entry after a served mutation** — the sink's
/// verdict from [`DataRouter::discard_clean_layout_entry`] counted (review
/// round 1, Issue 1): a kept DIRTY entry beside a record verb is legal
/// (the verb never changes the layout; the attr-cache drop + the kernel
/// invalidation are the whole act) and counted; beside a served LAYOUT
/// PUBLISH it is the custody law broken — a colleague published a layout
/// of a file this holder has an acked, unsaved write of, which S9's
/// exclusive custody forbids — reported on the `invariant_tripwires`
/// label `served_publish_over_dirty_entry`, the dirty entry KEPT (the
/// local authority; dropping it would lose this mount's acked bytes to a
/// peer's, which no law admits). A skipped discard is counted.
///
/// [`DataRouter::discard_clean_layout_entry`]: crate::routing::DataRouter::discard_clean_layout_entry
pub fn note_served_mutation_layout_entry(
    ino: Ino,
    kind: ServedMutation,
    verdict: crate::routing::LayoutEntryDiscard,
) {
    use crate::routing::LayoutEntryDiscard;
    match verdict {
        LayoutEntryDiscard::Discarded => {}
        LayoutEntryDiscard::DirtyKept => {
            SERVED_MUTATION_DIRTY_KEPT.fetch_add(1, Ordering::Relaxed);
            if kind == ServedMutation::Data {
                crate::note_invariant_tripwire(
                    "served_publish_over_dirty_entry",
                    &format!(
                        "a layout publish served for a peer landed on ino {ino} while this \
                         holder's own layout entry for it is DIRTY (an acked, unsaved write) — \
                         two writers under one custody; the dirty entry is kept"
                    ),
                );
            }
        }
        LayoutEntryDiscard::HeldSkipped => {
            SERVED_MUTATION_DISCARD_SKIPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The family's keys inside the `meta_ship` stats object.
pub fn stats_into(out: &mut serde_json::Map<String, serde_json::Value>) {
    let s = stats();
    out.insert("record_ships".into(), s.record_ships.into());
    out.insert("record_served".into(), s.record_served.into());
    out.insert("record_refusals".into(), s.record_refusals.into());
    out.insert(
        "record_ship_redirects".into(),
        s.record_ship_redirects.into(),
    );
    out.insert("record_unreachable".into(), s.record_unreachable.into());
    out.insert(
        "foreign_publish_ships".into(),
        s.foreign_publish_ships.into(),
    );
    out.insert(
        "foreign_publish_served".into(),
        s.foreign_publish_served.into(),
    );
    out.insert(
        "foreign_publish_refusals".into(),
        s.foreign_publish_refusals.into(),
    );
    out.insert(
        "served_mutation_invals".into(),
        s.served_mutation_invals.into(),
    );
    out.insert(
        "served_mutation_prunes".into(),
        s.served_mutation_prunes.into(),
    );
    out.insert(
        "served_mutation_dirty_kept".into(),
        s.served_mutation_dirty_kept.into(),
    );
    out.insert(
        "served_mutation_discard_skipped".into(),
        s.served_mutation_discard_skipped.into(),
    );
}

/// `record_ship_phase_ns` — the shipped verb's decomposition.
pub fn phase_json() -> serde_json::Value {
    let mut phases = serde_json::Map::new();
    for (i, name) in PHASE_NAMES.iter().enumerate() {
        phases.insert((*name).to_string(), PROF[i].to_json());
    }
    serde_json::Value::Object(phases)
}

/// Count one layout publish that LANDED at a slot holder.
pub(crate) fn note_publish_shipped() {
    FOREIGN_PUBLISH_SHIPS.fetch_add(1, Ordering::Relaxed);
}

/// Count one layout publish shipped to a slot holder that FAILED
/// terminally (refused at the holder, or the wire failed).
pub(crate) fn note_publish_refused() {
    FOREIGN_PUBLISH_REFUSALS.fetch_add(1, Ordering::Relaxed);
}

/// Count one layout publish served for a peer under the armed plane.
pub(crate) fn note_publish_served() {
    FOREIGN_PUBLISH_SERVED.fetch_add(1, Ordering::Relaxed);
}

/// Where a record-level verb on GLOBAL ino `ino` executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordHome {
    /// Apply here: an unarmed volume, an own / unleased slot, a slot one
    /// of this mount's own regions leases, or inside a served verb (this
    /// mount IS the holder there).
    Local,
    /// Appender `holder` leases the slot and serves at `endpoint`.
    Foreign { holder: u32, endpoint: Arc<str> },
}

/// The retryable refusal of a verb whose slot holder this mount cannot
/// reach — [`RefusalClass::HolderUnreachable`], `EAGAIN`.
pub fn unreachable_holder(ino: Ino, holder: u32, what: &str) -> SqueezefsError {
    RECORD_UNREACHABLE.fetch_add(1, Ordering::Relaxed);
    SqueezefsError::retryable(
        RefusalClass::HolderUnreachable { holder },
        format!(
            "{what} of ino {ino}: its record lives in a forest slot appender {holder} leases, \
             and this mount knows no endpoint for it (its join ladder has not published one, or \
             it is dead until PR 10's recovery re-leases its slots) — the record-level ship to \
             the slot holder cannot travel; retry (EAGAIN; meta_ship.record_unreachable)"
        ),
    )
}

/// Resolve `ino`'s record home, binding the holder's endpoint on demand
/// (`sym_join::bind_holder_endpoint_on_demand`, once). `Err` = the holder
/// is unreachable (the retryable class).
pub async fn record_home(routed: &RoutedMetaBackend, ino: Ino, what: &str) -> Result<RecordHome> {
    let (v_idx, local) = routed.route_ino(ino);
    let Some(vol) = routed.volumes.get(v_idx) else {
        return Ok(RecordHome::Local);
    };
    if !vol.slot_lease_armed() || crate::meta_ship::executing_for_ship_client() {
        return Ok(RecordHome::Local);
    }
    match crossvol_tx::step_home_bound(routed, v_idx, local).await {
        StepHome::Local => Ok(RecordHome::Local),
        // The DOOR's law is the truth: a slot leased by a region THIS
        // mount owns (PR 2–4's declared partition, one process) commits
        // into that region's ring here.
        StepHome::Foreign { holder, .. } | StepHome::Unreachable { holder }
            if vol.is_own_region(holder) =>
        {
            Ok(RecordHome::Local)
        }
        StepHome::Foreign { holder, endpoint } => Ok(RecordHome::Foreign { holder, endpoint }),
        StepHome::Unreachable { holder } => Err(unreachable_holder(ino, holder, what)),
    }
}

/// Whether `ino`'s slot is another appender's for a record-level
/// decision — the sync predicate (no bind; `false` unarmed, for an own /
/// unleased / own-region slot, and inside a served verb).
pub fn slot_is_foreign(routed: &RoutedMetaBackend, ino: Ino) -> bool {
    let (v_idx, local) = routed.route_ino(ino);
    let Some(vol) = routed.volumes.get(v_idx) else {
        return false;
    };
    if !vol.slot_lease_armed() || crate::meta_ship::executing_for_ship_client() {
        return false;
    }
    match crossvol_tx::foreign_holder_of(routed, v_idx, local) {
        Some(holder) => !vol.is_own_region(holder.appender_id),
        None => false,
    }
}

/// **Ship one record-level verb to its slot holder.** `Ok(None)` = the verb
/// is this mount's to apply (every unarmed / own / served shape);
/// `Ok(Some(reply))` = the holder applied it. A holder's SLOT-MOVED
/// refusal (the door's `SlotBusy` — the slot was handed over between the
/// resolve and the apply, defects 28/29's class) re-resolves the lessee
/// ONCE (`reresolve_slot_holder`: a joiner asks the manager) and dispatches
/// again — locally when the slot is ours now, to the new holder otherwise;
/// a second stale answer is the retryable class the caller sees.
pub async fn ship_record_verb(
    routed: &RoutedMetaBackend,
    ino: Ino,
    call: MetaCall,
) -> Result<Option<MetaReply>> {
    let what = call.verb().name();
    let t_total = Instant::now();
    let mut redirected = false;
    loop {
        let t_resolve = Instant::now();
        let home = record_home(routed, ino, what).await?;
        let (holder, endpoint) = match home {
            RecordHome::Local => return Ok(None),
            RecordHome::Foreign { holder, endpoint } => (holder, endpoint),
        };
        phase_record(RecordShipPhase::Resolve, t_resolve);
        if !redirected {
            RECORD_SHIPS.fetch_add(1, Ordering::Relaxed);
        }
        let t_rtt = Instant::now();
        match crossvol_tx::ship_meta_call(&endpoint, holder, call.clone()).await {
            Ok(reply) => {
                phase_record(RecordShipPhase::Rtt, t_rtt);
                phase_record(RecordShipPhase::Total, t_total);
                return Ok(Some(reply));
            }
            Err(e) if !redirected && crossvol_tx::is_slot_moved_refusal(&e) => {
                phase_record(RecordShipPhase::Rtt, t_rtt);
                redirected = true;
                RECORD_SHIP_REDIRECTS.fetch_add(1, Ordering::Relaxed);
                let (v_idx, local) = routed.route_ino(ino);
                let slot = super::kv::record::forest_slot_of_ino(local);
                let _ = routed.volumes[v_idx].reresolve_slot_holder(slot).await;
                log::debug!(
                    "record ship: {what} of ino {ino} was refused at appender {holder} ({endpoint}) \
                     because the slot moved ({e}); re-resolved once"
                );
            }
            Err(e) => {
                phase_record(RecordShipPhase::Rtt, t_rtt);
                phase_record(RecordShipPhase::Total, t_total);
                return Err(e);
            }
        }
    }
}

/// **The holder side's note** after a record-level verb applied HERE for a
/// shipping client: the served ledger, and the slot's dominance window
/// (§5.1.4 — a served ship is the requester's op on the slot, PR 13's
/// defect 9/13 law; the requester is the shipping mount's appender where
/// this plane knows its identity). Nothing outside a served verb, nothing
/// on an unarmed volume; a refusal that is not the door's slot-moved
/// class lands on `record_refusals`.
pub async fn note_served(
    routed: &RoutedMetaBackend,
    ino: Ino,
    failure: Option<&SqueezefsError>,
    served_at: Instant,
) {
    if !crate::meta_ship::executing_for_ship_client() {
        return;
    }
    let (v_idx, _local) = routed.route_ino(ino);
    let Some(vol) = routed.volumes.get(v_idx) else {
        return;
    };
    if vol.slot_leases().is_none() {
        return;
    }
    match failure {
        None => RECORD_SERVED.fetch_add(1, Ordering::Relaxed),
        Some(e) if crossvol_tx::is_slot_moved_refusal(e) => return,
        Some(_) => {
            RECORD_REFUSALS.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    // Boxed (PR 13f): the served tail's state (the sink, the dominance
    // note's resolve) sat inline in every routed record verb's future on
    // every posture; a served verb owns its allocation.
    Box::pin(async {
        note_served_mutation(ino, ServedMutation::Attrs).await;
        let Some(client) = crate::meta_ship::current_ship_client() else {
            return;
        };
        note_served_slot_ship(routed, ino, &client, served_at).await;
    })
    .await;
}

/// **The slot's dominance window fed by ONE served act** (§5.1.4 — a
/// served ship is the requester's op on the slot, PR 13's defect 9/13
/// law): the requester is the shipping mount's appender where this plane
/// knows its identity (`client` = its KD-MW-2 member id), the ship's wall
/// the served act's measured `served_at`. The two callers: a served
/// record verb ([`note_served`]) and a served LAYOUT PUBLISH
/// (`PublishService::note_foreign_publish_served` — review round 1, Issue
/// 8: a writer that dominates a slot through DATA writes alone, design
/// §5.10's own "`write` to a FOREIGN-owned file" row, never triggered the
/// offer before). Nothing on an unarmed volume, nothing for this mount's
/// own appender.
pub async fn note_served_slot_ship(
    routed: &RoutedMetaBackend,
    ino: Ino,
    client: &str,
    served_at: Instant,
) {
    let (v_idx, local) = routed.route_ino(ino);
    let Some(vol) = routed.volumes.get(v_idx) else {
        return;
    };
    let Some(plane) = vol.slot_leases() else {
        return;
    };
    let Some((node_token, mount_slot)) = crate::cowriter::parse_node_member_id(client) else {
        return;
    };
    let Some(requester) = plane.appender_of_identity(node_token, mount_slot) else {
        return;
    };
    if requester == vol.own_appender_id() {
        return;
    }
    let slot = super::kv::record::forest_slot_of_ino(local);
    let ship_ns = u64::try_from(served_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let _ = vol.note_slot_ship(slot, requester, ship_ns).await;
}

// ---------------------------------------------------------------------------
// The holder's OWN caches after a served mutation.
// ---------------------------------------------------------------------------

/// What a served record-level verb changed at the holder — the FUSE
/// layer's invalidation scope (an attrs-only kernel invalidation for a
/// record verb; pages too for a layout publish that changed the bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServedMutation {
    /// `setattr` / `setxattr` / `removexattr`: the inode record and its
    /// xattrs.
    Attrs,
    /// A layout publish: the file's size and bytes.
    Data,
}

/// The FUSE layer's hook: a served mutation of `ino` applied at THIS
/// mount's KV below the daemon's own caches — the router's RAM metadata
/// entry and the kernel's attr / page cache read the pre-mutation words
/// until told otherwise (the `sym-foreign-file` leg's first run: the
/// holder read its colleague's append as the old 8 bytes for the mount's
/// life while every other mount read the new 14). Installed once per
/// mount by `SqueezefsFilesystem`; absent on the in-process fixtures,
/// whose reads go to the KV directly.
pub type ServedMutationSink = Arc<
    dyn Fn(Ino, ServedMutation) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

static SERVED_MUTATION_SINK: Lazy<arc_swap::ArcSwapOption<ServedMutationSink>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the FUSE layer's served-mutation sink (one per process — the
/// mount's).
pub fn install_served_mutation_sink(sink: ServedMutationSink) {
    SERVED_MUTATION_SINK.store(Some(Arc::new(sink)));
}

/// Run the installed sink for a served mutation of `ino`; a no-op with
/// none installed.
pub async fn note_served_mutation(ino: Ino, kind: ServedMutation) {
    if let Some(sink) = SERVED_MUTATION_SINK.load_full() {
        sink(ino, kind).await;
    }
}

/// The FUSE layer's hook for a RECALLED object (a token this mount held
/// on a foreign object, recalled by its holder's commit): the daemon's
/// attr cache and the kernel's view of the object (attrs + pages, and the
/// writeback-cache kernel's inode) read the pre-commit words until told
/// otherwise — the same face as a served mutation, seen from a third
/// mount (the fidelity leg's manager read a joiner's append as the old 5
/// bytes and the shipped `chmod` as the old mode for the inode's life).
/// The router's layout entry is NOT this hook's: the recall sink drops it
/// under the dirty law itself. Installed once per mount; absent in-process.
pub type RecalledObjectSink = Arc<dyn Fn(Ino) + Send + Sync>;

static RECALLED_OBJECT_SINK: Lazy<arc_swap::ArcSwapOption<RecalledObjectSink>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the FUSE layer's recalled-object sink.
pub fn install_recalled_object_sink(sink: RecalledObjectSink) {
    RECALLED_OBJECT_SINK.store(Some(Arc::new(sink)));
}

/// Run the installed sink for a recalled object `ino` (GLOBAL); a no-op
/// with none installed.
pub fn note_recalled_object(ino: Ino) {
    if let Some(sink) = RECALLED_OBJECT_SINK.load_full() {
        sink(ino);
    }
}
