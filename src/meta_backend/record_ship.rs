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
}

/// `record_ship_phase_ns` — the shipped verb's decomposition.
pub fn phase_json() -> serde_json::Value {
    let mut phases = serde_json::Map::new();
    for (i, name) in PHASE_NAMES.iter().enumerate() {
        phases.insert((*name).to_string(), PROF[i].to_json());
    }
    serde_json::Value::Object(phases)
}

/// Count one layout publish shipped to a slot holder.
pub(crate) fn note_publish_shipped() {
    FOREIGN_PUBLISH_SHIPS.fetch_add(1, Ordering::Relaxed);
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
    let (v_idx, local) = routed.route_ino(ino);
    let Some(vol) = routed.volumes.get(v_idx) else {
        return;
    };
    let Some(plane) = vol.slot_leases() else {
        return;
    };
    match failure {
        None => RECORD_SERVED.fetch_add(1, Ordering::Relaxed),
        Some(e) if crossvol_tx::is_slot_moved_refusal(e) => return,
        Some(_) => {
            RECORD_REFUSALS.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let Some(client) = crate::meta_ship::current_ship_client() else {
        return;
    };
    let Some((node_token, mount_slot)) = crate::cowriter::parse_node_member_id(&client) else {
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
