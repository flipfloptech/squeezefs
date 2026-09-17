//! **The JOINED non-manager appender** (design-symmetric-metadata §7.3 /
//! §5.1 / §5.3 / §5.9; PR 12b — `feat/sym-n-daemon-posture`): the fifth
//! door beside `open` / `open_read_only` / `open_co_writer` / `open_probe`,
//! for EVERY RW mount of an ARMED set that did not win the D0 ladder.
//!
//! | Step of `open` | Joined appender |
//! |---|---|
//! | Layer A `flock(LOCK_EX)` | **not taken** (the manager holds it on a shared host; N joiners coexist) |
//! | DUR-5 superblock repair | **skipped** — sector 0 is the manager's |
//! | Bootstrap replay of the FIXED ring | as a PROJECTION: tree 0 + the manager's slot trees replayed like a non-writer's (no mint, unpublished slots skipped — they are the manager's to publish) |
//! | Layer B2 / B1 / `writer_claim` | **none** — the manager's claim stands; **rung 4's metadata half runs HERE, before any wire verb or device write**: on a PR-capable namespace a CO-LOCATED joiner (the manager's flock on this host, or its claim's boot) ADOPTS the manager's standing rtype-3 hold — zero device mutation, the holder cross-checked against the key the manager's durable claim derives (`WriterClaim::pr_key`) — and a REMOTE joiner REGISTERS under it with the set's one registrant key (`data_custody::join_wero_as_appender`); a manager holding rtype 1 refuses the join naming `SQUEEZEFS_META_PR_WERO`; a non-PR namespace needs KD-SYM-13's opt-in |
//! | `JoinAppender` | **over the wire** to the manager, BEFORE `open_inner`: the region (page + ring + grant) the manager mints is stood up beside the projection as this mount's own — a rejoin over an own `Live` page replays its ring as own residue |
//! | `AcquireSlots` | **over the wire**: `M` rotor slots installed into the own region; every first touch of an unleased slot is a wire `AcquireSlot`; a slot another appender leases refuses `SlotBusy` at the commit door |
//! | checkpoint + times-drain tasks | **spawned** — the checkpoint cycle is the JOINED one (`joined_checkpoint_cycle`, dispatched from the shared `checkpoint_cycle`): its slot trees' flushes, its page (the Issue-31 words), its tail, its SMOs in its ring under its grant, its extent returns and refills over the wire; never the ledger, the bitmap, tree 0 or page 0 |
//!
//! `regions[0]` of the joined backend is the MANAGER's region held for its
//! replay window and its tail only — `AppenderSet::owns_region(0)` is
//! `false`, `own_appender_id()` is the joined id, and every control write
//! (`write_control_entry`, the manager verbs) refuses loud here: the
//! joiner trusts nothing it did not read durably or derive, and writes
//! nothing another process owns.

use super::super::appender::{AppenderIdentity, AppenderRegion, AppenderState, GrantRun};
use super::super::record::ForestSlot;
use super::super::tree::{RootPtr, SmoContext};
use super::super::KvError;
use super::{KvMetaBackend, OpenPosture, ReadOnlyCause};
use crate::meta_ship::manager::{ManagerClient, ManagerReply, WireSlotGrant};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// What a non-manager RW mount needs to open one volume as a JOINED
/// appender — decided by the join ladder off DURABLE state
/// (`sym_join::resolve_holder_endpoint(vol, 0)` for the manager's
/// published listener, `membership::cluster_secret` for the proof of
/// storage membership), never a knob: there is no authority endpoint to
/// declare under the plane (§6.1 retires it).
#[derive(Debug, Clone)]
pub struct JoinedAppenderAdmission {
    /// The manager's S8 listener (the ladder's rung 7 published it into
    /// its claim-set entry).
    pub manager_endpoint: String,
    /// The set's `job:enroll` secret — possession of volume access IS
    /// cluster membership (ruling D2).
    pub secret: Vec<u8>,
    /// This node's member id (the wire peer id every plane knows it by).
    pub peer_id: String,
    /// The volume's ordinal in the routed set (the manager frame's
    /// `volume` word — one listener serves every volume a node manages).
    pub volume: u16,
    /// This mount's KD-MW-2 identity — `(node_token, mount_slot)`; the
    /// `writer_id` is minted per open. On one host every daemon shares the
    /// node token, so the mount slot is what tells the pages apart.
    pub identity: AppenderIdentity,
    /// The ONE registrant key this mount registers under on every
    /// namespace class of the set when it is REMOTE to the manager
    /// (minted once per set open by `open_routed_meta_set_joined`; unused
    /// by a co-located joiner, which adopts). `0` = mint at the door.
    pub registrant_key: u64,
}

/// The wire join's outcome, handed into `open_inner` so the appender open
/// stands the region up beside the manager's projection.
#[derive(Clone)]
pub(in crate::meta_backend::kv) struct JoinedOpen {
    pub appender_id: u32,
    /// The identity the page carries (the rejoin presents the predecessor
    /// page's, so the manager's `JoinAppender` answers `already`).
    pub identity: AppenderIdentity,
    /// The manager answered an existing `Live` page (a rejoin) — its ring
    /// is own residue; a fresh join's window is empty.
    pub already: bool,
    /// The wire the join travelled — the region open refills the grant
    /// through it BEFORE the own-residue replay mints (a rejoin whose
    /// predecessor died with its grant consumed).
    pub wire: Arc<JoinedWire>,
}

impl std::fmt::Debug for JoinedOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinedOpen")
            .field("appender_id", &self.appender_id)
            .field("identity", &self.identity)
            .field("already", &self.already)
            .finish_non_exhaustive()
    }
}

/// The joined appender's wire to its manager and the gauges of its
/// verbs (the Joined family, §11).
pub struct JoinedWire {
    client: crate::sqz_sync::SqzMutex<ManagerClient>,
    /// The manager's endpoint this mount dialed.
    pub endpoint: String,
    /// This mount's appender id on the volume.
    pub appender_id: u32,
    /// The identity the page carries (`(node, mount slot)` + this open's
    /// writer id).
    pub identity: AppenderIdentity,
    /// Manager verbs this mount issued (`joined_wire_verbs`).
    pub verbs: AtomicU64,
    /// Slots acquired over the wire — the join's rotor and every first
    /// touch (`joined_wire_acquires`).
    pub acquires: AtomicU64,
    /// Slots released over the wire (`joined_wire_releases`).
    pub releases: AtomicU64,
    /// Extent grants received / batches returned over the wire.
    pub extent_grants: AtomicU64,
    pub extent_returns: AtomicU64,
    /// Verbs the wire could not complete (a refused / failed call — the
    /// caller's retry class; `joined_wire_failures`).
    pub failures: AtomicU64,
    /// **Must-stay-0**: a control write (a tree-0 put, an allocator delta,
    /// a manager verb's executor) reached this joined appender — a site
    /// that assumed "this mount is the manager".
    pub control_refusals: AtomicU64,
    /// Ring-growth decisions this joiner could not take (its ring cannot
    /// grow — PR 2's drain-then-grow bound holds: it drains at
    /// `admissible ÷ cadence` per tick instead; `joined_ring_grow_declined`).
    pub grow_declined: AtomicU64,
    /// The manager is on THIS host (its D0 flock is held here, or its
    /// claim carries this boot): rung 4 ADOPTED its holds (KD-SYM-22) and
    /// this mount publishes no registrant key of its own.
    pub colocated: bool,
    /// The registrant key this mount REGISTERED on the metadata namespace
    /// (a remote joiner's; the data half registers the same one) — `0`
    /// when it adopted or the substrate is detection-grade.
    pub registrant_key: u64,
    /// Rung 4's metadata-namespace hold (an adopted or registrant
    /// `WeroHold`; `None` detection-grade), released at the END of the
    /// leave — after the last ring write and the wire `LeaveAppender`.
    meta_hold: std::sync::Mutex<Option<crate::data_custody::WeroHold>>,
}

impl JoinedWire {
    /// One verb's ledger step: counted, and a failure counted too.
    fn note<T>(&self, r: Result<T, KvError>) -> Result<T, KvError> {
        self.verbs.fetch_add(1, Ordering::Relaxed);
        if r.is_err() {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
        r
    }

    /// Release rung 4's metadata-namespace hold (an adopted hold releases
    /// nothing at the device; a registrant unregisters its key) —
    /// off-runtime, once: the leave's last act and every failed open's.
    pub async fn release_meta_hold(&self) {
        let hold = self.meta_hold.lock().unwrap().take();
        if let Some(hold) = hold {
            crate::data_custody::release_hold(hold).await;
        }
    }

    /// Whether rung 4's metadata hold is still held (the leave drops it
    /// last).
    pub fn meta_hold_standing(&self) -> bool {
        self.meta_hold.lock().unwrap().is_some()
    }
}

/// The Joined family's snapshot (§11) — `None` on every mount that is not
/// a joined appender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedStats {
    pub appender_id: u32,
    pub manager_endpoint: String,
    pub wire_verbs: u64,
    pub wire_acquires: u64,
    pub wire_releases: u64,
    pub wire_extent_grants: u64,
    pub wire_extent_returns: u64,
    pub wire_failures: u64,
    pub control_refusals: u64,
    pub ring_grow_declined: u64,
    /// Rung 4's posture word: `adopted` (co-located — the manager's hold
    /// shared, no key of ours), `registrant` (remote — our key under it)
    /// or `detection` (a non-PR substrate under KD-SYM-13's opt-in).
    pub registrant_posture: &'static str,
}

/// The one-line error a wire verb's unexpected reply becomes.
fn unexpected(verb: &str, reply: &ManagerReply) -> KvError {
    KvError::Busy(format!("{verb} answered {reply:?}"))
}

fn wire_err(verb: &str, e: crate::error::SqueezefsError) -> KvError {
    KvError::Busy(format!("{verb} over the wire failed: {e}"))
}

/// **One extent refill over the wire** (§5.3.3): `ExtentGrant { own, want }`
/// to the manager, the fresh runs landed in `region`'s RAM grant. Shared
/// by the mounted joiner (`KvMetaBackend::joined_extent_grant`, `pub(super)`) and the
/// joined OPEN's grant pre-sizing before its own-residue replay (no
/// backend exists yet there). Returns the extents received.
///
/// The PAGE is the witness the manager's §5.3.5 idempotency reads (a wire
/// appender's unclaimed remainder = its page's grant word): written
/// BEFORE the ask, so a remainder this mount has since CLAIMED (its
/// mints, its SMO images, a recovered window's in-window claims) is not
/// answered back verbatim as if still free, and AFTER a fresh carve, so
/// the runs the manager's record now names are on the page before any
/// of them is claimed — a crash between the record and this write leaves
/// them C13's claimed-unreachable class (the census returns them), never
/// a grant the next open reads as empty. Neither write barriers: this
/// runs inside the commit pipeline's resolve step (the mint's own device
/// write is there too) and a barrier here would park the pass behind the
/// durability lane it feeds; the next checkpoint's barrier makes the page
/// durable, and a joiner dying before it leaves its claims to the C13
/// census exactly as a lost page does.
pub(super) async fn wire_extent_refill(
    path: &Path,
    heap_start: u64,
    cache: &super::super::node_cache::NodeCache,
    wire: &JoinedWire,
    region: &AppenderRegion,
    want: u32,
) -> Result<u64, KvError> {
    if super::super::appender::test_manager_unreachable() {
        return Ok(0);
    }
    let name_remainder_on_page = |region: &AppenderRegion| {
        let mut g = region.grant();
        g.trim_to_page_runs();
        let mut page = region.page.lock().unwrap_or_else(|e| e.into_inner());
        page.grant = g.unclaimed_runs();
    };
    name_remainder_on_page(region);
    KvMetaBackend::write_region_page_at(path, region).await?;
    let runs: Vec<GrantRun> = {
        let mut c = wire.client.lock().await;
        wire.note(
            c.extent_grant(wire.appender_id, want)
                .await
                .map_err(|e| wire_err("ExtentGrant", e)),
        )?
    };
    // §5.3.5: the manager answers the caller's unclaimed remainder
    // VERBATIM when it covers the ask — only the runs the RAM grant does
    // not hold at all (unclaimed, claimed or parked) are new.
    let before = region.grant().unclaimed();
    let fresh: Vec<GrantRun> = {
        let g = region.grant();
        runs.iter()
            .filter(|r| !(r.start..r.start + u64::from(r.len)).all(|e| g.contains(e)))
            .cloned()
            .collect()
    };
    if !fresh.is_empty() {
        // The granted extents' cache barrier BEFORE the grant is claimable:
        // their previous images may sit in this mount's projection
        // (`NodeCache::drop_nodes_in_extents`).
        let extents: Vec<u64> = fresh
            .iter()
            .flat_map(|r| r.start..r.start + u64::from(r.len))
            .collect();
        let dropped = cache.drop_nodes_in_extents(heap_start, &extents)?;
        if dropped > 0 {
            log::info!(
                "meta volume {}: joined appender {} dropped {dropped} stale projection node(s) \
                 inside a fresh extent grant",
                path.display(),
                wire.appender_id
            );
        }
        region.grant().add_runs(&fresh);
        name_remainder_on_page(region);
        KvMetaBackend::write_region_page_at(path, region).await?;
    }
    let got = region.grant().unclaimed().saturating_sub(before);
    if got > 0 {
        wire.extent_grants.fetch_add(1, Ordering::Relaxed);
    }
    Ok(got)
}

impl KvMetaBackend {
    /// **Rung 4's METADATA half on a joiner** (design §5.8.1: the manager
    /// holds rtype 3, every other appender is a registrant; KD-SYM-13's
    /// opt-in on a substrate without reservations): runs BEFORE the wire
    /// join and before any device write of this mount. Returns the hold
    /// to keep for the mount's life and the key this mount REGISTERED
    /// (`0` when it adopted or the substrate is detection-grade).
    ///
    /// * PR-capable + co-located: ADOPT the manager's standing hold, the
    ///   holder cross-checked against the key its durable claim derives.
    /// * PR-capable + remote: REGISTER under it with the set's one key.
    /// * A standing hold that is not registrants-only (`SQUEEZEFS_META_PR_
    ///   WERO=0` on the manager) refuses — under rtype 1 no registration
    ///   grants write access, so every ring write would be rejected.
    /// * No reservation support: refuse without `SQUEEZEFS_SYM_ALLOW_NON_
    ///   PR=1`; detection-grade, announced, with it.
    async fn joined_meta_registrant(
        path: &Path,
        admission: &JoinedAppenderAdmission,
        colocated: bool,
        claim: Option<&super::WriterClaim>,
    ) -> Result<(Option<crate::data_custody::WeroHold>, u64), KvError> {
        let pr_capable = {
            let p = path.to_path_buf();
            squeezefs_ipc::sqz_blocking::run_blocking(move || {
                crate::meta_backend::reservation::resolve_for_mount(&p).is_some()
            })
            .await
        };
        let allow_non_pr = crate::env_knobs::bool_knob("SQUEEZEFS_SYM_ALLOW_NON_PR", false);
        if !pr_capable {
            if !allow_non_pr {
                return Err(KvError::Busy(format!(
                    "{}: a joined appender needs a PR-capable metadata namespace (design-\
                     symmetric-metadata §5.8.1 — the manager holds Write Exclusive – Registrants \
                     Only and every other appender writes as a REGISTRANT); this namespace \
                     advertises no NVMe Persistent Reservations, so a fenced joiner's ring write \
                     could only be DETECTED. KD-SYM-13: set SQUEEZEFS_SYM_ALLOW_NON_PR=1 to join \
                     DETECTION-GRADE (lab use) or use a PR-capable namespace (the kernel nvmet \
                     target)",
                    path.display()
                )));
            }
            log::warn!(
                "meta volume {}: JOINED APPENDER ON A NON-PR METADATA NAMESPACE under \
                 SQUEEZEFS_SYM_ALLOW_NON_PR=1 (KD-SYM-13) — its fence is detection-grade: the \
                 frame screen's tail scan, never the device",
                path.display()
            );
            return Ok((None, 0));
        }
        let enrolled: Vec<u64> = claim.map(|c| c.pr_key()).into_iter().collect();
        let key = if admission.registrant_key != 0 {
            admission.registrant_key
        } else {
            loop {
                let k = rand::Rng::gen::<u64>(&mut rand::thread_rng());
                if k != 0 {
                    break k;
                }
            }
        };
        let paths = vec![path.to_path_buf()];
        let joined = squeezefs_ipc::sqz_blocking::run_blocking(move || {
            crate::data_custody::join_wero_as_appender(&paths, colocated, &enrolled, Some(key))
        })
        .await
        .map_err(|e| {
            KvError::Busy(format!(
                "{}: the join ladder's rung 4 (registrant) refuses on the METADATA namespace \
                 ({}): {e}. A joined appender writes its ring under the manager's Write \
                 Exclusive – Registrants Only hold (rtype 3, SQUEEZEFS_META_PR_WERO=1 at the \
                 manager); no other rtype admits it",
                path.display(),
                if colocated {
                    "co-located — adopting the manager's standing hold"
                } else {
                    "remote — registering under the manager's standing hold"
                }
            ))
        })?;
        let hold = joined.hold().clone();
        let registered = if colocated { 0 } else { joined.evidence().key };
        log::info!(
            "meta volume {}: joined appender's rung 4 on the METADATA namespace — {} (holder key \
             {:#x}{})",
            path.display(),
            if colocated {
                "ADOPTED the co-located manager's rtype-3 hold, nothing registered (KD-SYM-22: \
                 one host, one registrant)"
            } else {
                "REGISTERED under the manager's rtype-3 hold"
            },
            joined.evidence().key,
            if registered != 0 {
                format!(", our key {registered:#x}")
            } else {
                String::new()
            }
        );
        Ok((Some(hold), registered))
    }

    /// **Rung 5 — the wire `JoinAppender`**: the predecessor page of this
    /// identity (if any) decides what is PRESENTED (a `Live` one's
    /// identity, so the manager answers `already`; a `Recovering` one
    /// refuses — the retryable class), the manager is dialed, the join
    /// asked. Returns the connected client, the identity presented, the
    /// appender id and the `already` word.
    async fn wire_join_appender(
        path: &Path,
        admission: &JoinedAppenderAdmission,
        sb: &super::super::superblock::SuperblockV3,
        writer_id: u128,
    ) -> Result<(ManagerClient, AppenderIdentity, u32, bool), KvError> {
        let entries = super::super::appender::read_directory(path, sb).await?;
        let predecessor = entries.iter().find_map(|e| {
            e.page.as_ref().filter(|p| {
                p.identity.node_token == admission.identity.node_token
                    && p.identity.mount_slot == admission.identity.mount_slot
                    && matches!(p.state, AppenderState::Live | AppenderState::Recovering)
            })
        });
        if let Some(p) = predecessor.filter(|p| p.state == AppenderState::Recovering) {
            return Err(KvError::Busy(format!(
                "{}: appender {}'s page is RECOVERING under this identity (node {:#018x}, mount \
                 slot {:#x}) — the manager is recovering the predecessor's region; the join \
                 retries after the recovery lands (EAGAIN class)",
                path.display(),
                p.appender_id,
                p.identity.node_token,
                p.identity.mount_slot
            )));
        }
        let presented = match predecessor {
            Some(p) => p.identity,
            None => AppenderIdentity {
                writer_id,
                ..admission.identity
            },
        };
        let mut client = ManagerClient::connect(
            &admission.manager_endpoint,
            &admission.secret,
            &admission.peer_id,
            admission.volume,
        )
        .await
        .map_err(|e| {
            KvError::Busy(format!(
                "{}: dialing the manager at {} for JoinAppender failed: {e}",
                path.display(),
                admission.manager_endpoint
            ))
        })?;
        let (appender_id, already) = match client
            .join(presented, 0)
            .await
            .map_err(|e| wire_err("JoinAppender", e))?
        {
            ManagerReply::Joined {
                appender_id,
                already,
                ..
            } => (appender_id, already),
            ManagerReply::Refused { reason } => {
                return Err(KvError::Busy(format!(
                    "{}: the manager refused JoinAppender: {reason}",
                    path.display()
                )))
            }
            other => return Err(unexpected("JoinAppender", &other)),
        };
        Ok((client, presented, appender_id, already))
    }

    /// **Open one volume as a JOINED non-manager appender** (the fifth door
    /// — see the module doc for the step table). The wire `JoinAppender`
    /// runs FIRST, idempotent against the directory (§5.3.5): a `Live`
    /// page already carrying this `(node, mount slot)` is a predecessor
    /// that died un-recovered, and its identity is what the join presents
    /// so the manager answers `already` and the open replays that ring as
    /// own residue (§5.3.2's identity binding); a `Recovering` page of this
    /// identity is mid-recovery by the manager — refused as the retryable
    /// class (the joiner parks one beat, PR 10's law); a `Recovered` page is
    /// never re-adopted (§5.8.3 — the join mints a fresh region).
    pub async fn open_joined_appender(
        path: &Path,
        admission: &JoinedAppenderAdmission,
    ) -> Result<Arc<Self>, KvError> {
        if !super::super::slot_lease::symmetric_meta_requested() {
            return Err(KvError::Busy(format!(
                "{}: a joined-appender open needs the symmetric plane (SQUEEZEFS_SYMMETRIC_META=1) \
                 — a second RW mount without it is the D0 single-writer guard's refusal, never a \
                 join (design-symmetric-metadata §7.3)",
                path.display()
            )));
        }
        let local_flock = match Self::probe_shared_lock(path) {
            super::SharedProbe::LocalExclusiveHolder => {
                log::info!(
                    "meta volume {}: joined-appender open — a LOCAL exclusive holder (the \
                     manager's write mount on this host) holds the writer lock; this mount takes \
                     no lock and joins over the wire",
                    path.display()
                );
                true
            }
            super::SharedProbe::NoLocalExclusiveHolder => {
                log::info!(
                    "meta volume {}: joined-appender open — the manager is on another host",
                    path.display()
                );
                false
            }
            super::SharedProbe::Unknown => false,
        };
        // The predecessor's page, if any: presented to the join so the
        // manager answers the SAME region (`already`).
        let sb = match super::classify_volume(path).await? {
            super::VolumeFormat::V3(sb) => sb,
            _ => {
                return Err(KvError::Corrupt(format!(
                    "{} is not a v3 volume — nothing to join",
                    path.display()
                )))
            }
        };
        if !sb.symmetric_forest_stamped() {
            return Err(KvError::Corrupt(format!(
                "{}: not symmetric-forest capable (incompat bit 17 absent) — a joined appender \
                 needs the forest; run `squeezefs volume enable-symmetric` or format `--symmetric`",
                path.display()
            )));
        }
        // The manager's durable claim: whose fence stands on this
        // namespace (its derived key), and whether it is THIS host's
        // (the flock above is the kernel's proof; the claim's boot the
        // co-writer ladder's — either makes the join co-located).
        let claim = {
            let probe = crate::meta_backend::open_volume_probe(&path.to_string_lossy()).await?;
            probe.read_writer_claim().await
        };
        let colocated = local_flock
            || claim
                .as_ref()
                .is_some_and(|c| !c.boot.is_empty() && c.boot == super::read_boot_id());
        let (meta_hold, registrant_key) =
            Self::joined_meta_registrant(path, admission, colocated, claim.as_ref()).await?;
        // Rung 5 — the wire join. Every refusal from here releases rung
        // 4's hold off-runtime (a registrant's key never outlives a
        // refused join).
        let writer_id = uuid::Uuid::new_v4().as_u128();
        let joined = match Self::wire_join_appender(path, admission, &sb, writer_id).await {
            Ok(j) => j,
            Err(e) => {
                if let Some(hold) = meta_hold {
                    crate::data_custody::release_hold(hold).await;
                }
                return Err(e);
            }
        };
        let (client, presented, appender_id, already) = joined;
        let wire = Arc::new(JoinedWire {
            client: crate::sqz_sync::SqzMutex::new(client),
            endpoint: admission.manager_endpoint.clone(),
            appender_id,
            identity: AppenderIdentity {
                writer_id,
                ..admission.identity
            },
            verbs: AtomicU64::new(1),
            acquires: AtomicU64::new(0),
            releases: AtomicU64::new(0),
            extent_grants: AtomicU64::new(0),
            extent_returns: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            control_refusals: AtomicU64::new(0),
            grow_declined: AtomicU64::new(0),
            colocated,
            registrant_key,
            meta_hold: std::sync::Mutex::new(meta_hold),
        });
        let mut inner = match Self::open_inner(
            path,
            OpenPosture::JoinedAppender,
            Some(JoinedOpen {
                appender_id,
                identity: presented,
                already,
                wire: Arc::clone(&wire),
            }),
        )
        .await
        {
            Ok(inner) => inner,
            Err(e) => {
                wire.release_meta_hold().await;
                return Err(e);
            }
        };
        inner.writer_id = uuid::Uuid::from_u128(writer_id).to_string();
        inner.ro_cause = ReadOnlyCause::Writable;
        let be = Arc::new(inner);
        let _ = be.conveyor_self.set(Arc::downgrade(&be));
        let _ = be.joined.set(wire);
        be.trace_guard_event("joined_appender_admitted");
        if let Err(e) = be.join_joined_region(already).await {
            be.abandon_joined_open().await;
            return Err(e);
        }
        if let Err(e) = be.arm_joined_leases().await {
            be.abandon_joined_open().await;
            return Err(e);
        }
        super::super::checkpoint::spawn_checkpoint_task(&be);
        super::super::checkpoint::spawn_times_drain_task(&be);
        log::warn!(
            "meta volume {}: mounted as a JOINED symmetric appender {appender_id} (manager at {}, \
             {}, {}) — no writer_claim, no reservation of its own; its ring, page and slot \
             trees are its own, tree 0 and the manager's trees a projection. Guarantee class: {}",
            path.display(),
            admission.manager_endpoint,
            if already {
                "rejoined over own residue"
            } else {
                "fresh join"
            },
            if colocated {
                "co-located: the manager's hold adopted"
            } else if registrant_key != 0 {
                "remote: a registrant under the manager's hold"
            } else {
                "detection-grade"
            },
            be.writer_guard_mode()
        );
        Ok(be)
    }

    /// The joined appender's wire, `None` on every other door.
    pub fn joined_wire(&self) -> Option<&Arc<JoinedWire>> {
        self.joined.get()
    }

    /// Whether this mount is a JOINED non-manager appender (PR 12b).
    pub fn is_joined_appender(&self) -> bool {
        self.appenders
            .as_ref()
            .is_some_and(|s| s.is_joined_appender())
    }

    /// The Joined family's snapshot; `None` unless this mount joined.
    pub fn joined_stats(&self) -> Option<JoinedStats> {
        let w = self.joined.get()?;
        Some(JoinedStats {
            appender_id: w.appender_id,
            manager_endpoint: w.endpoint.clone(),
            wire_verbs: w.verbs.load(Ordering::Relaxed),
            wire_acquires: w.acquires.load(Ordering::Relaxed),
            wire_releases: w.releases.load(Ordering::Relaxed),
            wire_extent_grants: w.extent_grants.load(Ordering::Relaxed),
            wire_extent_returns: w.extent_returns.load(Ordering::Relaxed),
            wire_failures: w.failures.load(Ordering::Relaxed),
            control_refusals: w.control_refusals.load(Ordering::Relaxed),
            ring_grow_declined: w.grow_declined.load(Ordering::Relaxed),
            registrant_posture: if w.colocated {
                "adopted"
            } else if w.registrant_key != 0 {
                "registrant"
            } else {
                "detection"
            },
        })
    }

    /// The joined region (the one region this mount writes).
    fn joined_region(&self) -> Result<&Arc<AppenderRegion>, KvError> {
        let set = self.appenders.as_ref().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: not a symmetric-forest volume",
                self.path.display()
            ))
        })?;
        let id = set.joined_appender.ok_or_else(|| {
            KvError::Busy(format!(
                "{}: this mount is not a joined appender",
                self.path.display()
            ))
        })?;
        set.region(id).ok_or_else(|| {
            KvError::Corrupt(format!(
                "{}: joined appender {id} has no region object",
                self.path.display()
            ))
        })
    }

    /// The refusal every control write meets on a joined appender —
    /// counted on the must-stay-0 `control_refusals`: tree 0, the bitmap,
    /// the ledger and the manager's verbs are another process's.
    pub(super) fn refuse_joined_control(&self, what: &str) -> Result<(), KvError> {
        let Some(w) = self.joined.get() else {
            return Ok(());
        };
        w.control_refusals.fetch_add(1, Ordering::Relaxed);
        Err(KvError::Busy(format!(
            "{}: {what} on a JOINED appender ({}) — the manager (peer at {}) owns tree 0, the \
             bitmap and the ledger; a joiner reaches them over the wire only \
             (design-symmetric-metadata KD-SYM-3; joined_control_refusals must stay 0)",
            self.path.display(),
            w.appender_id,
            w.endpoint
        )))
    }

    /// The JOIN of the region the wire minted: the page goes `Live` under
    /// THIS open's identity at `term + 1` (a rejoin re-stamps the
    /// predecessor's page; a fresh join's page already carries the
    /// identity the manager wrote), one barriered page write. The
    /// manager's `JoinAppender` made the ring durable; this write is the
    /// joiner's first as the page's one writer.
    async fn join_joined_region(&self, rejoin: bool) -> Result<(), KvError> {
        let set = self.appenders.as_ref().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: not a symmetric-forest volume",
                self.path.display()
            ))
        })?;
        let wire = self.joined.get().ok_or_else(|| {
            KvError::Corrupt(format!("{}: joined wire unset", self.path.display()))
        })?;
        let region = self.joined_region()?;
        {
            let mut page = region.page.lock().unwrap_or_else(|e| e.into_inner());
            if rejoin {
                page.term += 1;
            }
            page.state = AppenderState::Live;
            page.recovered_by_term = 0;
            page.identity = wire.identity;
            page.is_manager = false;
            page.home_volume = 0;
            // The page's ONE writer from here on (PR 12's Issue-31 law):
            // `ckpt_seq ≥ 1` tells the manager's slot / grant rewrites to
            // yield — every later word (a grant's runs, a lease's root)
            // reaches this mount on the reply and ITS page write names it.
            // The counter is the JOINER's own (its page's, never the
            // manager's ledger seq the open seeded).
            page.ckpt_seq += 1;
            self.checkpoint_seq.store(page.ckpt_seq, Ordering::Release);
        }
        set.joins.fetch_add(1, Ordering::Relaxed);
        set.joined.store(true, Ordering::Release);
        set.appenders_known.store(
            set.live_pages_at_mount.max(set.regions.len() as u64),
            Ordering::Relaxed,
        );
        self.write_region_page(region).await?;
        self.sync_device().await.map_err(KvError::Io)
    }

    /// A failed joined open tears down what it stood up: the checkpoint
    /// task is not yet spawned, nothing durable of ours but the page and
    /// the manager's region — left for the next open of this identity to
    /// rejoin over (`already`), never freed from here.
    async fn abandon_joined_open(&self) {
        self.shutting_down.store(true, Ordering::Release);
        self.ring.wake_parked();
        if let Some(wire) = self.joined.get() {
            wire.release_meta_hold().await;
        }
    }

    // -----------------------------------------------------------------
    // The wire acquires — the ladder's rung 6 and the door's first touch.
    // -----------------------------------------------------------------

    /// The plane, armed or not yet (the arm below raises the gate LAST),
    /// or the refusal — a joined open runs under it by construction.
    fn joined_plane(&self) -> Result<Arc<super::super::slot_lease::SlotLeasePlane>, KvError> {
        self.appenders
            .as_ref()
            .and_then(|a| a.slot_leases())
            .cloned()
            .ok_or_else(|| {
                KvError::Busy(format!(
                    "{}: the symmetric plane is not armed on this joined appender",
                    self.path.display()
                ))
            })
    }

    /// **Rung 6 — the joined appender's slot leases** (§5.1.2 / §5.3.2):
    /// tree 0 read as the lease PROJECTION, the own page's entries settled
    /// against it (rows 5 / 6 of §5.3.4 over the wire), every slot tree 0
    /// names as OURS re-adopted (a rejoin), then `AcquireSlots { want: M }`
    /// over the wire — each grant installed into the own region (the
    /// tree opened at the grant's root, the cursor floor, the seq-space
    /// raise, the door bit); every other leased slot marked FOREIGN; the
    /// S4 table, the frame fence, the token holder, the gate armed (never
    /// as the manager — an UNLEASED tree is the manager's to maintain).
    async fn arm_joined_leases(&self) -> Result<(), KvError> {
        let set = self.appenders.as_ref().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: not a symmetric-forest volume",
                self.path.display()
            ))
        })?;
        let plane = self.joined_plane()?;
        let wire = Arc::clone(self.joined.get().ok_or_else(|| {
            KvError::Corrupt(format!("{}: joined wire unset", self.path.display()))
        })?);
        let own = wire.appender_id;
        // The join's grant (or a rejoin's recovered remainder) may name
        // extents whose previous images this open's projection loaded —
        // the cache barrier before the first mint can claim one.
        {
            let extents: Vec<u64> = self
                .joined_region()?
                .grant()
                .unclaimed_runs()
                .iter()
                .flat_map(|r| r.start..r.start + u64::from(r.len))
                .collect();
            let dropped = self
                .cache
                .drop_nodes_in_extents(self.sb.heap.start, &extents)?;
            if dropped > 0 {
                log::info!(
                    "meta volume {}: joined appender {own} dropped {dropped} stale projection \
                     node(s) sitting inside its granted extents",
                    self.path.display()
                );
            }
        }
        self.load_slot_leases(&plane).await?;
        self.settle_joined_page_entries(&plane, &wire).await?;
        // A rejoin: every slot tree 0 still names as ours is re-adopted
        // without a write (its records replayed as own residue above).
        let mut held = 0usize;
        for slot in plane.table.held_by(own) {
            let words = crate::slot_lease_core::SlotWords {
                seq_floor: plane.table.get(slot).map_or(0, |l| l.words.seq_floor),
                ..Default::default()
            };
            self.install_lease(set, &plane, own, slot, words, false)
                .await?;
            held += 1;
        }
        // The rotor: `M` slots, `prefer: unleased-then-idle` at the manager.
        let m = plane.mint_slots();
        let rotor_now = plane.table.rotor_held_by(own);
        if rotor_now < m {
            let want = u16::try_from(m - rotor_now).unwrap_or(u16::MAX);
            let (grants, _already) = {
                let mut c = wire.client.lock().await;
                wire.note(
                    c.acquire_slots(own, want)
                        .await
                        .map_err(|e| wire_err("AcquireSlots", e)),
                )?
            };
            for g in grants {
                let slot = self.install_wire_grant(&plane, &wire, &g, true).await?;
                plane.rotor_update(|rotor| {
                    if !rotor.contains(&slot) {
                        rotor.push(slot);
                    }
                });
            }
        } else {
            // A rejoin whose rotor tree 0 still records: the rotor bit is
            // RAM (tree 0 does not carry it) — rebuilt from the held set.
            let mut readopted: Vec<ForestSlot> = plane
                .table
                .held_by(own)
                .into_iter()
                .filter(|s| *s != super::super::record::NATIVE_FOREST_SLOT)
                .collect();
            readopted.sort_unstable();
            readopted.truncate(m as usize);
            for slot in &readopted {
                plane.table.mark_rotor(*slot, own);
            }
            plane.rotor_update(|rotor| *rotor = readopted);
        }
        self.publish_slot_owners(set, &plane);
        plane.refresh_holders();
        plane.seed_n_floor_inputs();
        // The manager's endpoint is the one binding the join already
        // knows: appender 0's tokens (every unleased slot's reads, the
        // manager's own slots' objects), its shipped steps and its
        // custody are served there. Every other appender's is resolved
        // off durable state (`sym_join::bind_live_appender_endpoints`).
        plane.holders.set_endpoint(0, &wire.endpoint);
        // PR 5 — the frame fence (every frame of ours carries `(own, g)`),
        // the token HOLDER (this writer grants tokens on the objects of
        // its slots and recalls them before its conflicting commits).
        if let Ok(weak) = self.conveyor_identity() {
            self.cache
                .install_frame_fence(Arc::new(super::SlotFrameFence { be: weak }));
        }
        self.arm_token_holder();
        plane.gate.arm();
        super::super::slot_lease::register_carriage_plane(&plane);
        log::info!(
            "meta volume {}: joined appender {own} ARMED — {} slot(s) leased ({held} re-adopted, \
             {} rotor), M = {m}, manager {}",
            self.path.display(),
            plane.gate.leased_count(),
            plane.rotor.load().len(),
            wire.endpoint
        );
        Ok(())
    }

    /// The §5.3.4 rows 5 / 6 for the joiner's OWN page as loaded: a
    /// `Releasing` entry with tree 0 still leasing the slot to us at that
    /// `g` is COMPLETED over the wire (the handover this identity died
    /// inside — the manager writes tree 0); one tree 0 no longer leases to
    /// us is dropped (tree 0 wins). A `Live` entry under another lessee at
    /// tree 0's `g` is the C14 conflict class and refuses (PR 4's arm).
    async fn settle_joined_page_entries(
        &self,
        plane: &super::super::slot_lease::SlotLeasePlane,
        wire: &Arc<JoinedWire>,
    ) -> Result<(), KvError> {
        use super::super::appender::SlotEntryState;
        let own = wire.appender_id;
        let native = self.appenders.as_ref().map_or(0, |a| a.native_slot);
        let loaded = std::mem::take(
            &mut *plane
                .loaded_page_entries
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        let Some(entries) = loaded.get(&own) else {
            return Ok(());
        };
        let region = self.joined_region()?;
        for se in entries {
            let slot = super::super::appender::forest_slot_of_page_slot(se.slot, native);
            let lease = plane.table.get(slot);
            match se.state {
                SlotEntryState::Live => {
                    if let Some(l) = lease {
                        if l.state != crate::slot_lease_core::LeaseState::Unleased
                            && l.holder != own
                            && l.g <= se.g
                        {
                            plane.conflicts.fetch_add(1, Ordering::Relaxed);
                            return Err(KvError::Corrupt(format!(
                                "{}: slot {slot} is LIVE on appender {own}'s page at g {} while \
                                 tree 0 leases it to appender {} at g {} — a slot custody conflict \
                                 (design-symmetric-metadata §5.8.5 C14; slot_lease_conflicts). \
                                 Refusing the join; the remedy is `squeezefs appender clear`",
                                self.path.display(),
                                se.g,
                                l.holder,
                                l.g
                            )));
                        }
                        if l.holder == own
                            && l.state != crate::slot_lease_core::LeaseState::Unleased
                            && se.slot_tree_extents != 0
                            && plane.extents.get(slot) == 0
                        {
                            plane.extents.set(slot, u64::from(se.slot_tree_extents));
                        }
                    }
                }
                SlotEntryState::Releasing => match lease {
                    Some(l)
                        if l.state != crate::slot_lease_core::LeaseState::Unleased
                            && l.holder == own
                            && l.g == se.g =>
                    {
                        let words = crate::slot_lease_core::SlotWords {
                            root: (se.root.addr, se.root.seq),
                            cursor: se.cursor,
                            extents: se.slot_tree_extents,
                            seq_floor: region.ring().seq_frontier().max(l.words.seq_floor),
                        };
                        let tails = match self.forest().and_then(|f| f.tree(slot)) {
                            Some(t) => self.leaf_tails(&t).await?,
                            None => Vec::new(),
                        };
                        self.wire_release_slot(wire, slot, se.g, words, tails)
                            .await?;
                        plane.table.load(
                            slot,
                            crate::slot_lease_core::SlotLease::unleased(se.g, 0, words),
                        );
                        log::warn!(
                            "meta volume {}: appender {own}'s page named slot {slot} RELEASING at \
                             g {} with tree 0 still leasing it — the handover this identity died \
                             inside is completed over the wire (§5.3.4 row 5)",
                            self.path.display(),
                            se.g
                        );
                    }
                    _ => log::info!(
                        "meta volume {}: appender {own}'s page named slot {slot} RELEASING at g \
                         {} — tree 0 no longer leases it to this appender; dropped (row 6)",
                        self.path.display(),
                        se.g
                    ),
                },
            }
        }
        Ok(())
    }

    /// Install one wire grant into the own region: the table mirrors the
    /// manager's `Leased { own, g }` (the projection is refreshed by the
    /// grant's own words — the manager wrote tree 0 before it answered),
    /// then PR 4's `install_lease` (the tree at the grant's root, the
    /// cursor floor, the seq-space raise, the door bit), the holder cache
    /// and the rotor bit. Returns the FOREST slot.
    async fn install_wire_grant(
        &self,
        plane: &Arc<super::super::slot_lease::SlotLeasePlane>,
        wire: &Arc<JoinedWire>,
        g: &WireSlotGrant,
        rotor: bool,
    ) -> Result<ForestSlot, KvError> {
        let set = self.appenders.as_ref().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: not a symmetric-forest volume",
                self.path.display()
            ))
        })?;
        let own = wire.appender_id;
        let slot = self.forest_slot_of_routing(g.slot);
        let words: crate::slot_lease_core::SlotWords = g.words.into();
        plane.table.load(
            slot,
            crate::slot_lease_core::SlotLease::leased_with(own, g.g, words),
        );
        if rotor {
            plane.table.mark_rotor(slot, own);
        }
        plane.gate.clear_foreign(slot);
        // The grant's tree arrives from the MANAGER's (or a previous
        // lessee's) appends: the cross-daemon barrier re-reads it at the
        // granted root under this mount's SMO mutex (no pass of ours spans
        // the transfer; the slot was nobody's here until this instant).
        {
            let _smo = self.smo.lock().await;
            self.adopt_transferred_slot_tree(
                slot,
                RootPtr {
                    addr: words.root.0,
                    seq: words.root.1,
                },
            )
            .await?;
        }
        self.install_lease(set, plane, own, slot, words, true)
            .await?;
        plane.holders.learn(
            slot,
            crate::slot_holder_cache::SlotHolder {
                appender_id: own,
                g: g.g,
            },
        );
        plane.grants.fetch_add(1, Ordering::Relaxed);
        wire.acquires.fetch_add(1, Ordering::Relaxed);
        Ok(slot)
    }

    /// **The door's first touch over the wire** (§5.1.2 first-writer-
    /// takes-it): `AcquireSlot { own, slot }` to the manager — `Granted` ⇒
    /// installed into the own region; `SlotRefused { holder }` ⇒ the
    /// "ship to the holder" class (`KvError::SlotBusy`); `Deferred` ⇒ the
    /// retry class (EAGAIN, nothing written at the manager).
    pub(super) async fn joined_acquire_slot(&self, slot: ForestSlot) -> Result<(), KvError> {
        let plane = self.joined_plane()?;
        let wire = Arc::clone(self.joined.get().ok_or_else(|| {
            KvError::Corrupt(format!("{}: joined wire unset", self.path.display()))
        })?);
        let own = wire.appender_id;
        let routing = self.routing_slot_of_forest(slot)?;
        let reply = {
            let mut c = wire.client.lock().await;
            wire.note(
                c.acquire_slot(own, routing)
                    .await
                    .map_err(|e| wire_err("AcquireSlot", e)),
            )?
        };
        match reply {
            ManagerReply::SlotsGranted { slots, .. } => {
                for g in &slots {
                    self.install_wire_grant(&plane, &wire, g, false).await?;
                }
                if let Some(set) = self.appenders.as_ref() {
                    self.publish_slot_owners(set, &plane);
                }
                Ok(())
            }
            ManagerReply::SlotRefused { holder, g, .. } => {
                // The projection learns the holder the manager named.
                plane
                    .table
                    .load(slot, crate::slot_lease_core::SlotLease::leased(holder, g));
                plane.gate.mark_foreign(slot);
                plane.holders.learn(
                    slot,
                    crate::slot_holder_cache::SlotHolder {
                        appender_id: holder,
                        g,
                    },
                );
                plane.door_refusals.fetch_add(1, Ordering::Relaxed);
                Err(KvError::SlotBusy { slot, holder, g })
            }
            ManagerReply::Deferred { reason } => Err(KvError::GrantDeferred {
                slot,
                cycles: 0,
                frontier: 0,
                tail_start: 0,
                tail: 0,
            })
            .map_err(|e| {
                log::info!(
                    "meta volume {}: AcquireSlot {slot} deferred by the manager ({reason}); the \
                     committer retries",
                    self.path.display()
                );
                e
            }),
            other => Err(unexpected("AcquireSlot", &other)),
        }
    }

    /// `ReleaseSlot` over the wire — the joiner's tree-0 step of every
    /// flush-then-transfer and of its leave (§5.1.4): the manager screens
    /// every word before it writes (`screen_release_words`).
    async fn wire_release_slot(
        &self,
        wire: &Arc<JoinedWire>,
        slot: ForestSlot,
        g: u32,
        words: crate::slot_lease_core::SlotWords,
        tails: Vec<(u64, u32)>,
    ) -> Result<bool, KvError> {
        let routing = self.routing_slot_of_forest(slot)?;
        let mut c = wire.client.lock().await;
        let out = wire.note(
            c.release_slot(wire.appender_id, routing, g, words.into(), tails)
                .await
                .map_err(|e| wire_err("ReleaseSlot", e)),
        )?;
        wire.releases.fetch_add(1, Ordering::Relaxed);
        Ok(out)
    }

    /// The durable tree-0 step of a slot release, dispatched by posture:
    /// the manager writes tree 0 itself (`manager_release_slot`), a joined
    /// appender asks its manager over the wire. The ONE dispatch both
    /// `transfer_slot_locked` (the handover) and the leave ride.
    pub(super) async fn release_slot_durably(
        &self,
        region_id: u32,
        slot: ForestSlot,
        words: crate::slot_lease_core::SlotWords,
        g: u32,
        tails: Vec<(u64, u32)>,
    ) -> Result<(), KvError> {
        match self.joined.get() {
            Some(wire) => {
                self.wire_release_slot(wire, slot, g, words, tails).await?;
                if let Some(plane) = self.slot_leases() {
                    let _ = plane.table.release(slot, region_id, g, words, 0);
                    plane.holders.forget(slot);
                }
                Ok(())
            }
            None => self
                .manager_release_slot(region_id, slot, words, g, tails)
                .await
                .map(|_already| ()),
        }
    }

    // -----------------------------------------------------------------
    // The joined checkpoint cycle and its grant cadence.
    // -----------------------------------------------------------------

    /// **One checkpoint cycle of a JOINED appender** (§4.6 pt 2 for ONE
    /// ring; §5.3.2 the page as the appender's ledger record): the dirty
    /// nodes of ITS slot trees flushed (snapshot-then-write; SMOs through
    /// the region-scoped context into ITS ring under ITS grant), barrier
    /// #1, the flush-ceiling audit, the region's TAIL (its head, its oldest
    /// open reservation, the floors of its slots' leaves and unpublished
    /// roots — every position in ITS ring), ITS page with the Issue-31 words
    /// (`head_hint` = its head, `seq_offset` in force, `ckpt_seq` + 1), the
    /// tail on its pending-reclaim ledger, and — under `barrier_now` — the
    /// barrier that advances its `reusable_upto`. Nothing of the manager's
    /// is touched: no tree-0 publication (a leased slot's root rides its
    /// page, KD-SYM-3), no meta bitmap page, no ledger record, no page 0.
    /// The grant cadence at the end returns and refills over the wire.
    pub(in crate::meta_backend::kv) async fn joined_checkpoint_cycle(
        &self,
        smo: &mut SmoContext,
        barrier_now: bool,
    ) -> Result<(), KvError> {
        let cycle_started = std::time::Instant::now();
        let set = self.appenders.as_ref().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: not a symmetric-forest volume",
                self.path.display()
            ))
        })?;
        let region = self.joined_region()?;
        let own = region.id;
        let ring = region.ring();
        let h = ring.core().head();

        // ---- Flush pass over OUR nodes alone: a dirty node of the
        // manager's projection (tree 0, its slot trees — folded at the
        // open's replay, refreshed by the poll) is never ours to write.
        let mut dirty: Vec<Arc<super::CachedNode>> = Vec::new();
        let mut oldest_since: u64 = 0;
        self.node_cache().for_each_node(|n| {
            if n.dirty_floor() != u64::MAX
                && !n.state().is_superseded()
                && self.region_of_node(n) == own
            {
                if n.level() == 0 {
                    let since = n.dirty_since_ns();
                    if since != 0 {
                        oldest_since = if oldest_since == 0 {
                            since
                        } else {
                            oldest_since.min(since)
                        };
                    }
                }
                dirty.push(Arc::clone(n));
            }
        });
        let had_dirty: Vec<(u32, u64)> = if oldest_since != 0 {
            vec![(own, oldest_since)]
        } else {
            Vec::new()
        };
        let mut deferred_for_grant = 0u64;
        let mut deferred_for_space = 0u64;
        for node in dirty {
            let addr = node.addr();
            let tree = self.tree_of_node(&node)?;
            let mut out = tree.checkpoint_flush_node(smo, addr).await;
            // The REACTIVE refill (§5.3.3) over the wire: the flush pass
            // that exhausts the grant asks the manager for this SMO's own
            // need and retries the node once.
            if let Err(KvError::GrantExhausted { needed, .. }) = &out {
                let want = u32::try_from(*needed)
                    .unwrap_or(u32::MAX)
                    .max(super::super::appender::SMO_IMAGES_MAX);
                match self.joined_extent_grant(want).await {
                    Ok(n) if n > 0 => {
                        out = tree.checkpoint_flush_node(smo, addr).await;
                    }
                    Ok(_) => {}
                    Err(e) => log::warn!(
                        "meta volume {}: joined appender {own}'s reactive ExtentGrant deferred \
                         ({e})",
                        self.path.display()
                    ),
                }
            }
            match out {
                Ok(()) => {}
                Err(KvError::JournalReserveExhausted { needed }) => {
                    log::debug!(
                        "joined checkpoint: SMO reserve exhausted ({needed} B) at node \
                         {addr:#x}; deferred to the next cycle"
                    );
                }
                Err(KvError::GrantExhausted {
                    unclaimed, needed, ..
                }) => {
                    deferred_for_grant += 1;
                    region.dependency_stalls.fetch_add(1, Ordering::Relaxed);
                    log::warn!(
                        "joined checkpoint: appender {own}'s extent grant is exhausted \
                         ({unclaimed} unclaimed, {needed} needed) at node {addr:#x}; compaction \
                         deferred until the manager refills it (manager_dependency_stalls)"
                    );
                }
                Err(KvError::NoSpace { free, reserve }) => {
                    deferred_for_space += 1;
                    log::debug!(
                        "joined checkpoint: metadata heap exhausted (free={free}, \
                         reserve={reserve}) at node {addr:#x}; compaction deferred"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        // PR 8: the DATA allocation bitmap pages of a lease HOMED on this
        // volume through this joiner's grant (a no-op unheld) — before the
        // barrier that makes the page durable.
        let ckpt_seq = self.checkpoint_seq.load(Ordering::Acquire) + 1;
        self.write_data_alloc_pages(ckpt_seq).await?;

        // ---- Barrier #1: our node appends become durable.
        self.sync_device().await.map_err(KvError::Io)?;
        self.note_flush_ceiling(&had_dirty, crate::mono_core::monotonic_ns_u64());

        // ---- The region's tail: every clamp a position in ITS ring.
        let dying_leaf_floors = self.node_cache().take_dying_leaf_floors();
        // The volume-wide dying floors (interior nodes and root swaps of
        // our trees — the SMO context stamps them per slot too): drained
        // and folded into our tail like a declared region's.
        let dying_floors = self.node_cache().take_dying_floors();
        let mut tail = h.min(ring.min_inflight_start()).min(dying_floors);
        for (slot, f) in self.unpublished_root_floors() {
            if region.leases_slot(slot) {
                tail = tail.min(f);
            }
        }
        for (slot, f) in &dying_leaf_floors {
            if region.leases_slot(*slot) {
                tail = tail.min(*f);
            }
        }
        self.node_cache().for_each_node(|n| {
            let floor = n.dirty_floor();
            if floor != u64::MAX && self.region_of_node(n) == own {
                tail = tail.min(floor);
            }
        });
        // A grant exhaustion or the space class pins the tail at the
        // deferred node's floor by construction (its floor was restored
        // inside `checkpoint_flush_node`) — the classes the manager's
        // cycle counts, here on the joiner's face.
        if deferred_for_space > 0 {
            self.heap_full_cycles.fetch_add(1, Ordering::Relaxed);
        }

        // ---- ITS page: the Issue-31 words, this cycle's seq.
        let region_tails = vec![(own, tail)];
        if let Err(e) = self
            .write_appender_pages(0, ckpt_seq, h, &region_tails)
            .await
        {
            self.node_cache().restore_dying_floors(dying_floors);
            self.node_cache()
                .restore_dying_leaf_floors(dying_leaf_floors);
            return Err(e);
        }
        self.checkpoint_seq.store(ckpt_seq, Ordering::Release);
        self.last_ledger_tail.store(tail, Ordering::Release);
        super::super::META_KV_CHECKPOINTS.fetch_add(1, Ordering::Relaxed);
        crate::free_grace::note_checkpoint_completed(cycle_started.elapsed());
        if barrier_now {
            // The page is durable, the tail advances now (the parked
            // committer's drain shape and the leave's guarantee).
            self.sync_device().await.map_err(KvError::Io)?;
        }
        if deferred_for_grant > 0 {
            log::debug!(
                "joined checkpoint: {deferred_for_grant} node(s) deferred for an exhausted extent \
                 grant (tail {tail}); the manager's refill owns the retry"
            );
        }
        // Ring growth is the manager's bitmap act — declined here; the
        // ring drains at `admissible ÷ cadence` per tick (PR 2's law).
        {
            let stalls = region.stalls.load(Ordering::Relaxed);
            if stalls != region.stalls_at_last_grow.load(Ordering::Relaxed) {
                region.stalls_at_last_grow.store(stalls, Ordering::Relaxed);
                if let Some(w) = self.joined.get() {
                    w.grow_declined.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        // ---- The grant cadence over the wire.
        let now = crate::mono_core::monotonic_ns_u64();
        let last = set.cadence_last_ns.swap(now, Ordering::AcqRel);
        let cycle_ms = if last == 0 {
            0
        } else {
            now.saturating_sub(last) / 1_000_000
        };
        self.joined_grant_cadence(cycle_ms).await
    }

    /// `ExtentGrant { own, want }` over the wire: the manager carves and
    /// journals; the runs land in the own region's RAM grant (the page
    /// names the remainder at the next checkpoint). Returns the extents
    /// received.
    pub(super) async fn joined_extent_grant(&self, want: u32) -> Result<u64, KvError> {
        let wire = Arc::clone(self.joined.get().ok_or_else(|| {
            KvError::Corrupt(format!("{}: joined wire unset", self.path.display()))
        })?);
        let region = self.joined_region()?;
        wire_extent_refill(
            &self.path,
            self.sb.heap.start,
            &self.cache,
            &wire,
            region,
            want,
        )
        .await
    }

    /// The joined appender's grant cadence (§5.3.3): fold the SMO rate,
    /// ship the extents this cycle's barrier released as `ReturnExtents`
    /// over the wire, refill at 50 % consumption.
    async fn joined_grant_cadence(&self, cycle_ms: u64) -> Result<(), KvError> {
        let wire = Arc::clone(self.joined.get().ok_or_else(|| {
            KvError::Corrupt(format!("{}: joined wire unset", self.path.display()))
        })?);
        let region = self.joined_region()?;
        region.fold_smo_rate(cycle_ms);
        let returnable = region.grant().take_returnable();
        if !returnable.is_empty() {
            let runs = super::super::slot_state::ExtentGrantRecord::from_extents(
                returnable.iter().copied(),
            )
            .runs;
            let out = {
                let mut c = wire.client.lock().await;
                wire.note(
                    c.return_extents(wire.appender_id, &runs)
                        .await
                        .map_err(|e| wire_err("ReturnExtents", e)),
                )
            };
            match out {
                Ok(_) => {
                    wire.extent_returns.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    region.grant().restore_returnable(returnable);
                    log::warn!(
                        "meta volume {}: joined appender {}'s ReturnExtents deferred ({e}); \
                         retried next cycle",
                        self.path.display(),
                        region.id
                    );
                }
            }
        }
        let due = {
            let g = region.grant();
            g.refill_due()
        };
        if due && !super::super::appender::test_manager_unreachable() {
            if let Err(e) = self.joined_extent_grant(0).await {
                log::warn!(
                    "meta volume {}: joined appender {}'s ExtentGrant refill deferred ({e})",
                    self.path.display(),
                    region.id
                );
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // The leave.
    // -----------------------------------------------------------------

    /// **The joined appender's LEAVE** (§5.1.3 region release, the clean-
    /// unmount arm): after the final checkpoint covered its ring, every
    /// slot it leases is handed to NOBODY over the wire — flush-then-
    /// transfer per slot (`transfer_slot_locked`: the door drained, the
    /// window cleared, the page `Releasing`, `ReleaseSlot` to the manager,
    /// the page without it) — then `LeaveAppender`: the manager writes the
    /// page `Free` and returns the ring extents and the unclaimed grant to
    /// its heap. An UNCOVERED ring keeps the region whole (page `Live`,
    /// ring and grant claimed, leases held) for the next open of this
    /// identity to recover as own residue — loud.
    pub(in crate::meta_backend::kv) async fn leave_joined_regions(&self) -> Result<(), KvError> {
        let set = self.appenders.as_ref().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: not a symmetric-forest volume",
                self.path.display()
            ))
        })?;
        let wire = Arc::clone(self.joined.get().ok_or_else(|| {
            KvError::Corrupt(format!("{}: joined wire unset", self.path.display()))
        })?);
        if self.is_failed() || !set.joined.load(Ordering::Acquire) {
            return Ok(());
        }
        let region = self.joined_region()?;
        let own = region.id;
        {
            let ring = region.ring();
            let core = ring.core();
            if core.head() > core.reusable_upto() {
                log::warn!(
                    "meta volume {}: joined appender {own}'s ring is UNCOVERED at the leave (head \
                     {}, reusable_upto {}) — its page stays Live with its roots and its ring \
                     claimed; the next open of this identity recovers the window as own residue",
                    self.path.display(),
                    core.head(),
                    core.reusable_upto()
                );
                return Ok(());
            }
        }
        let Some(plane) = self.slot_leases().cloned() else {
            return Ok(());
        };
        let held: Vec<ForestSlot> = region.leases().iter().copied().collect();
        // PR 9: every live custody grant this holder issued is recalled
        // before its slots go Unleased.
        let still = crate::data_grant::recall_custody_at_leave(self.volume_uuid(), &held).await;
        if !still.is_empty() {
            log::error!(
                "meta volume {}: joined appender {own} keeps its leases at the leave — live \
                 custody grants on slot(s) {still:?} survived the recall; its page stays Live",
                self.path.display()
            );
            return Ok(());
        }
        {
            let _handover = self.handover.lock().await;
            for slot in held {
                if let Err(e) = self.transfer_slot_locked(own, slot).await {
                    log::error!(
                        "meta volume {}: joined appender {own} could not release slot {slot} at \
                         the leave ({e}) — its page stays Live for the next open to rejoin over",
                        self.path.display()
                    );
                    return Ok(());
                }
            }
        }
        super::super::slot_lease::unregister_carriage_plane(&plane);
        super::super::alloc_lease::disarm_symmetric_roles();
        // The carriage's member side is this set's: a grant adopted after
        // the leave carries nothing this mount may act on.
        crate::membership::uninstall_slot_carriage_sink();
        // The unclaimed remainder returns with the region.
        let returnable = {
            let mut g = region.grant();
            let mut all = g.take_returnable();
            all.extend(g.take_unclaimed());
            all
        };
        let runs =
            super::super::slot_state::ExtentGrantRecord::from_extents(returnable.iter().copied())
                .runs;
        let out = {
            let mut c = wire.client.lock().await;
            wire.note(
                c.leave_appender(wire.identity, own, &runs)
                    .await
                    .map_err(|e| wire_err("LeaveAppender", e)),
            )
        };
        match out {
            Ok(already) => {
                {
                    let mut page = region.page.lock().unwrap_or_else(|e| e.into_inner());
                    page.state = AppenderState::Free;
                    page.slots.clear();
                    page.grant.clear();
                }
                region.released.store(true, Ordering::Release);
                set.leaves.fetch_add(1, Ordering::Relaxed);
                log::info!(
                    "meta volume {}: joined appender {own} LEFT (LeaveAppender{}) — its page is \
                     Free, its ring and {} unclaimed extent(s) returned to the manager's heap",
                    self.path.display(),
                    if already { ", already" } else { "" },
                    returnable.len()
                );
            }
            Err(e) => {
                region.grant().restore_returnable(returnable);
                log::warn!(
                    "meta volume {}: joined appender {own}'s LeaveAppender failed ({e}) — its \
                     page stays Live with no leases; the next open of this identity rejoins \
                     over it",
                    self.path.display()
                );
            }
        }
        // Rung 4's metadata hold goes LAST — after this mount's final
        // ring write and its LeaveAppender: a registrant's key must cover
        // every write it issued (an adopted hold releases nothing).
        wire.release_meta_hold().await;
        Ok(())
    }

    // -----------------------------------------------------------------
    // The manager's executor of the joiner's LeaveAppender.
    // -----------------------------------------------------------------

    /// **`LeaveAppender { identity, appender_id, unclaimed }`** — served by
    /// the MANAGER (§5.1.3 region release for a wire appender): the page
    /// must be `Live` under `identity`'s `(node, mount slot)` and name no
    /// slot, and tree 0 must lease it nothing (every lease released
    /// first — else `Busy`, the caller finishes its handovers); the
    /// unclaimed runs return through the screened `ReturnExtents` law
    /// (intersected with the appender's record — a wire integer is never
    /// an allocation authority); the page goes `Free` into BOTH directory
    /// slots keeping its id, term and head (the seq-space watermark the
    /// next carve continues from), barriered; then the ring extents' bits
    /// clear and land with their own bitmap write + barrier. A `Free`
    /// page answers `already`; a `Recovering` / `Recovered` page or a
    /// foreign identity is `Rejected` (the wire-word law).
    pub async fn manager_leave_appender(
        &self,
        identity: AppenderIdentity,
        appender_id: u32,
        unclaimed: &[GrantRun],
    ) -> Result<bool, KvError> {
        use super::super::appender::{read_directory, write_page};
        let set = self.manager_gate(false)?;
        if set.owns_region(appender_id) {
            set.verbs.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(KvError::Rejected(format!(
                "{}: LeaveAppender names appender {appender_id}, one of this mount's own regions",
                self.path.display()
            )));
        }
        let node_size = u64::from(self.sb.node_size);
        let _g = self.manager_verbs.lock().await;
        let entries = read_directory(&self.path, &self.sb).await?;
        let Some(entry) = entries.iter().find(|e| e.appender_id == appender_id) else {
            set.verbs.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(KvError::Rejected(format!(
                "{}: LeaveAppender names appender {appender_id}, which has no directory page",
                self.path.display()
            )));
        };
        let Some(mut page) = entry.page.clone() else {
            return Ok(true);
        };
        match page.state {
            AppenderState::Free => return Ok(true),
            AppenderState::Live => {}
            other => {
                set.verbs.rejected.fetch_add(1, Ordering::Relaxed);
                return Err(KvError::Rejected(format!(
                    "{}: LeaveAppender of appender {appender_id} whose page is {other:?} — a \
                     region under recovery is the recovery driver's, never its holder's to leave",
                    self.path.display()
                )));
            }
        }
        if page.identity.node_token != identity.node_token
            || page.identity.mount_slot != identity.mount_slot
        {
            set.verbs.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(KvError::Rejected(format!(
                "{}: LeaveAppender of appender {appender_id} by node {:#018x} / mount slot {:#x} — \
                 the page is Live under node {:#018x} / mount slot {:#x}",
                self.path.display(),
                identity.node_token,
                identity.mount_slot,
                page.identity.node_token,
                page.identity.mount_slot
            )));
        }
        let leased: Vec<ForestSlot> = set
            .slot_leases()
            .map(|p| p.table.held_by(appender_id))
            .unwrap_or_default();
        if !page.slots.is_empty() || !leased.is_empty() {
            return Err(KvError::Busy(format!(
                "{}: LeaveAppender of appender {appender_id} while it still leases {} slot(s) \
                 (page names {}) — release them first (flush-then-transfer to nobody)",
                self.path.display(),
                leased.len(),
                page.slots.len()
            )));
        }
        drop(_g);
        if !unclaimed.is_empty() {
            self.manager_return_runs(appender_id, unclaimed).await?;
        }
        let _g = self.manager_verbs.lock().await;
        let segments = std::mem::take(&mut page.segments);
        page.state = AppenderState::Free;
        page.slots.clear();
        page.grant.clear();
        page.ledger_tail_seq = page.head_hint;
        for off in entry.dir_offsets {
            page.generation += 1;
            write_page(&self.path, off, page.encode()?).await?;
        }
        self.sync_device().await.map_err(KvError::Io)?;
        // No durable page names the ring extents any more, and no journal
        // record ever did: the immediate release is the right op (the
        // in-process leave's law), its bits on their own write + barrier.
        for ext in &segments {
            let mut off = ext.start;
            while off < ext.end() {
                self.alloc
                    .release_unpublished((off - self.sb.heap.start) / node_size);
                off += node_size;
            }
        }
        let ckpt_seq = self.checkpoint_seq.fetch_add(1, Ordering::AcqRel) + 1;
        self.alloc
            .write_dirty_pages(&self.path, self.sb.alloc_bitmap.start, ckpt_seq)
            .await?;
        self.sync_device().await.map_err(KvError::Io)?;
        set.leaves.fetch_add(1, Ordering::Relaxed);
        let live = entries
            .iter()
            .filter(|e| {
                e.appender_id != appender_id
                    && e.page
                        .as_ref()
                        .is_some_and(|p| p.state == AppenderState::Live)
            })
            .count() as u64;
        set.appenders_known
            .store(live.max(set.regions.len() as u64), Ordering::Relaxed);
        log::info!(
            "meta volume {}: LeaveAppender — appender {appender_id} (node {:#018x}, mount slot \
             {:#x}) left: page Free, {} ring segment(s) and {} unclaimed run(s) returned",
            self.path.display(),
            identity.node_token,
            identity.mount_slot,
            segments.len(),
            unclaimed.len()
        );
        Ok(false)
    }

    /// **The manager's `PublishEndpoint`** (PR 12b — the joiner's half of
    /// the holder → endpoint binding, §5.1.6): the joined appender's
    /// listener into ITS claim-set member entry on this volume (the id
    /// `cowriter::node_member_id_of(node, mount slot)` — what
    /// `sym_join::resolve_holder_endpoint` reads for its page on any mount;
    /// the process-less form, `pid 0`, which the same-boot dead-writer
    /// prune exempts — the entry is REPLACED by the identity's next
    /// publish and read as unreachable while its listener is down) and
    /// into this manager's slot holder table. Screened before any effect:
    /// the page must be `Live` under `identity`, the endpoint a socket
    /// address. `Ok(already)` when the same address already stood.
    pub async fn manager_publish_endpoint(
        &self,
        identity: AppenderIdentity,
        appender_id: u32,
        endpoint: &str,
        pr_key: u64,
    ) -> Result<bool, KvError> {
        use super::super::appender::read_directory;
        let set = self.manager_gate(false)?;
        let reject = |why: String| {
            set.verbs.rejected.fetch_add(1, Ordering::Relaxed);
            KvError::Rejected(format!(
                "{}: PublishEndpoint of appender {appender_id} rejected — {why} \
                 (manager_verb_rejected)",
                self.path.display()
            ))
        };
        if endpoint.parse::<std::net::SocketAddr>().is_err() {
            return Err(reject(format!("{endpoint:?} is not a socket address")));
        }
        if set.owns_region(appender_id) {
            return Err(reject("one of this mount's own regions".to_string()));
        }
        let entries = read_directory(&self.path, &self.sb).await?;
        let page = entries
            .iter()
            .find(|e| e.appender_id == appender_id)
            .and_then(|e| e.page.as_ref())
            .filter(|p| p.state == AppenderState::Live)
            .ok_or_else(|| reject("no Live page".to_string()))?;
        if !page
            .identity
            .is_mount(identity.node_token, identity.mount_slot)
        {
            return Err(reject(format!(
                "the page is Live under node {:#018x} / mount slot {:#x}, not the caller's \
                 {:#018x} / {:#x}",
                page.identity.node_token,
                page.identity.mount_slot,
                identity.node_token,
                identity.mount_slot
            )));
        }
        let member_id =
            crate::cowriter::node_member_id_of(identity.node_token, identity.mount_slot);
        let already = crate::membership::ClaimSet::load(self)
            .await
            .and_then(|s| {
                s.members
                    .iter()
                    .find(|m| crate::membership::member_id_matches(&m.identity.id, &member_id))
                    .map(|m| m.identity.endpoint.as_deref() == Some(endpoint))
            })
            .unwrap_or(false);
        if !already {
            let member = crate::membership::MemberIdentity {
                id: member_id,
                role: crate::membership::MemberRole::Writer,
                pid: 0,
                boot: String::new(),
                endpoint: Some(endpoint.to_string()),
                pr_key,
            };
            crate::membership::upsert_writer_member(self, &member, crate::dlm::durable_term())
                .await
                .map_err(|e| {
                    KvError::Busy(format!(
                        "{}: PublishEndpoint of appender {appender_id} could not write the \
                         claim-set entry: {e}",
                        self.path.display()
                    ))
                })?;
        }
        if let Some(plane) = set.slot_leases() {
            plane.holders.set_endpoint(appender_id, endpoint);
        }
        if already {
            set.verbs.replays.fetch_add(1, Ordering::Relaxed);
        }
        Ok(already)
    }

    /// The joiner's half of [`Self::manager_publish_endpoint`]: publish
    /// this mount's listener through the manager (the join ladder's rung 7
    /// on a joined appender) and bind it as our own in the holder table.
    /// `Ok(already)`.
    pub async fn joined_publish_endpoint(
        &self,
        endpoint: &str,
        pr_key: u64,
    ) -> Result<bool, KvError> {
        let wire = Arc::clone(self.joined.get().ok_or_else(|| {
            KvError::Corrupt(format!("{}: joined wire unset", self.path.display()))
        })?);
        let already = {
            let mut c = wire.client.lock().await;
            wire.note(
                c.publish_endpoint(wire.identity, wire.appender_id, endpoint, pr_key)
                    .await
                    .map_err(|e| wire_err("PublishEndpoint", e)),
            )?
        };
        if let Ok(plane) = self.joined_plane() {
            plane.holders.set_endpoint(wire.appender_id, endpoint);
        }
        Ok(already)
    }

    /// **The manager's `ResolveEndpoint`** (PR 12b, N ≥ 3): appender
    /// `appender_id`'s published listener off this manager's holder table
    /// (every `PublishEndpoint` it served, its own arm's binding) or, for
    /// an appender that published to a PREDECESSOR manager, off its live
    /// claim set through the durable resolver. One `u32` word, one table
    /// lookup, at most one directory read + one claim-set read; `None` =
    /// not published. A joined appender refuses (a control read it cannot
    /// answer freshly — its projection is its open's).
    pub async fn manager_resolve_endpoint(
        &self,
        appender_id: u32,
    ) -> Result<Option<String>, KvError> {
        let set = self.manager_gate(false)?;
        if let Some(e) = set
            .slot_leases()
            .and_then(|p| p.holders.endpoint(appender_id))
        {
            return Ok(Some(e.to_string()));
        }
        let resolved = crate::sym_join::resolve_holder_endpoint(self, appender_id).await;
        if let (Some(e), Some(plane)) = (resolved.as_ref(), set.slot_leases()) {
            plane.holders.set_endpoint(appender_id, e);
        }
        Ok(resolved)
    }

    /// The joiner's half of [`Self::manager_resolve_endpoint`]: ask the
    /// manager for `appender_id`'s listener over the wire — the manager's
    /// table is exact where this joiner's claim-set projection is its
    /// open's. `Ok(None)` = not published.
    pub async fn joined_resolve_endpoint(
        &self,
        appender_id: u32,
    ) -> Result<Option<String>, KvError> {
        let wire = Arc::clone(self.joined.get().ok_or_else(|| {
            KvError::Corrupt(format!("{}: joined wire unset", self.path.display()))
        })?);
        let mut c = wire.client.lock().await;
        wire.note(
            c.resolve_endpoint(appender_id)
                .await
                .map_err(|e| wire_err("ResolveEndpoint", e)),
        )
    }

    // -----------------------------------------------------------------
    // The membership carriage's member side (§5.9, §5.1.4).
    // -----------------------------------------------------------------

    /// **The wire holder's flush-then-transfer on a release notice**
    /// (§5.1.4 as the design writes it for a member: the manager's recall
    /// of a slot this joiner holds — a requester's accepted offer — rides
    /// its renewal grant's `slot_release_notices`, and the holder runs
    /// `release_slot_handover` exactly as the manager's cadence runs its
    /// own: door closed and drained, the ring flushed clear of the slot,
    /// the tokens on it recalled, the page `Releasing`, `ReleaseSlot {
    /// root, cursor, seq_floor, tails }` over the wire, the page without
    /// the slot; the requester's `AcquireSlot` then lands at `g + 1`).
    /// `routing` names ROUTING slots; a slot this joiner does not hold
    /// (already released, or another volume's) is skipped. Returns the
    /// slots released; a handover refused `Busy` / deferred for custody
    /// is "not this beat" (the next renewal carries the notice again).
    pub async fn joined_act_on_release_notices(&self, routing: &[u16]) -> usize {
        let Some(wire) = self.joined.get() else {
            return 0;
        };
        let Ok(plane) = self.joined_plane() else {
            return 0;
        };
        let own = wire.appender_id;
        let mut released = 0;
        for r in routing {
            let slot = self.forest_slot_of_routing(*r);
            let held_here = matches!(
                plane.table.resolve(slot),
                crate::slot_lease_core::Resolved::Holder { holder, .. } if holder == own
            );
            if !plane.gate.is_leased(slot) || !held_here {
                continue;
            }
            match self.release_slot_handover(own, slot).await {
                // The wire `ReleaseSlot` inside counts on `wire_releases`.
                Ok(()) => released += 1,
                Err(KvError::Busy(why)) | Err(KvError::HandoverDeferred(why)) => {
                    log::debug!(
                        "meta volume {}: joined appender {own}'s release of slot {slot} on the \
                         manager's notice waits a beat — {why}",
                        self.path.display()
                    );
                }
                Err(e) => log::warn!(
                    "meta volume {}: joined appender {own} could not release slot {slot} on the \
                     manager's notice ({e}); the next renewal carries it again",
                    self.path.display()
                ),
            }
        }
        released
    }

    /// **The offer's acceptance** (§5.1.4 — the dominance rule's requester
    /// side): every `(routing slot, g)` the grant says stands offered to
    /// this joiner is taken with `AcquireSlot` (the wire first touch —
    /// `joined_acquire_slot`); an offer that lapsed or moved answers
    /// `SlotBusy` / `Deferred` and is left to the next beat. Returns the
    /// slots acquired.
    pub async fn joined_accept_offers(&self, offered: &[(u16, u32)]) -> usize {
        if self.joined.get().is_none() {
            return 0;
        }
        let mut taken = 0;
        for (r, _g) in offered {
            let slot = self.forest_slot_of_routing(*r);
            match self.joined_acquire_slot(slot).await {
                Ok(()) => taken += 1,
                Err(e) => log::debug!(
                    "meta volume {}: the offer of slot {slot} was not taken — {e}",
                    self.path.display()
                ),
            }
        }
        taken
    }

    // -----------------------------------------------------------------
    // The control-plane projection.
    // -----------------------------------------------------------------

    /// **Refresh the joined appender's tree-0 PROJECTION** (§5.2 — tree 0
    /// on a non-manager is read at open and POLLED; §5.7.1's control-plane
    /// row): read the manager's newest ledger record (the predicted-slot
    /// read, PR 5's), and when tree 0's root moved install it writer-legal
    /// (`install_recovered_root` — the node seq verified against the
    /// pointer, the seq handle raised) and reload the lease table from it
    /// (every slot NOT ours takes the manager's word; ours are RAM-
    /// authoritative and never overwritten). Returns whether the root
    /// moved. Never runs on the manager (its tree 0 is live).
    pub async fn refresh_control_projection(&self) -> Result<bool, KvError> {
        let Some(wire) = self.joined.get() else {
            return Ok(false);
        };
        let Some(forest) = self.forest() else {
            return Ok(false);
        };
        let plane = self.joined_plane()?;
        let base = self.sb.root_ledger.start;
        let Some(rec) = super::super::checkpoint::read_newest_ledger(&self.path, base).await?
        else {
            return Ok(false);
        };
        let Some(root) = rec
            .tree_roots
            .iter()
            .find(|r| r.tree_id == super::super::record::TREE_CONTROL)
        else {
            return Ok(false);
        };
        let control = forest.control();
        let root = RootPtr {
            addr: root.node_addr,
            seq: root.node_seq,
        };
        if root == control.root() {
            return Ok(false);
        }
        control.install_recovered_root(root, 0).await?;
        // The projection's lease words for every slot that is not ours.
        let own = wire.appender_id;
        let before: std::collections::BTreeMap<ForestSlot, crate::slot_lease_core::SlotLease> =
            plane
                .table
                .snapshot()
                .into_iter()
                .filter(|(_, l)| {
                    l.holder == own && l.state != crate::slot_lease_core::LeaseState::Unleased
                })
                .collect();
        self.load_slot_leases(&plane).await?;
        for (slot, lease) in before {
            plane.table.load(slot, lease);
        }
        if let Some(set) = self.appenders.as_ref() {
            self.publish_slot_owners(set, &plane);
        }
        plane.refresh_holders();
        Ok(true)
    }
}
