//! DLM **stage S9** — the **multi-writer mount arm**: the one opt-in path
//! that arms metadata ownership (S8), the data-plane custody fence and its
//! device-enforced WERO hold (S7), and the remote write-custody plane
//! ([`crate::data_grant`]) as a coherent whole — or refuses loudly, naming
//! the piece that is missing.
//!
//! # Why the arm is one function
//!
//! S8 declined to arm ownership and said why: *"a mount that ships metadata
//! but cannot ship data custody is not a product."* The converse is equally
//! true — a mount that grants remote data custody but publishes layouts
//! locally would append to a tree whose journal ring, extent bitmap and
//! root ledger belong to another writer. The three planes are only sound
//! **together**, so there is exactly one place that turns them on, and its
//! refusal ladder is the product's honest answer about what multi-writer
//! requires.
//!
//! # The ladder, in order, and why each rung is where it is
//!
//! 1. **A reader is refused.** `-o ro` is S5's posture: a reader takes no
//!    lease, writes no claim, holds no PR key. Demanding a multi-writer
//!    DATA plane on it is a category error, not a degradation.
//! 2. **The format's capability bits** — cheap, side-effect-free, and it
//!    must come before anything is acquired. [`REQUIRED_INCOMPAT`] is the
//!    set whose absence would make a second writer *unsound*, and the
//!    refusal names the first missing bit **and its offline stamping
//!    path**, because ruling **D9** means nothing stamps them today.
//! 3. **The substrate** (§6.9's S9 guarantee: *"full multi-writer on PR
//!    substrates; **refused on non-PR**"*): [`crate::data_custody`]'s own
//!    arm takes the WERO (rtype 3) hold, joining the standing one rather
//!    than forking a second key, and refuses a namespace that advertises no
//!    reservation support — naming it.
//! 4. **The membership plane**, because a co-writer that cannot be *seen*
//!    cannot be *evicted*: S6's eviction is what mints the dead epoch S7's
//!    quarantine is keyed on, and its census is what a `squeezefs clients`
//!    audit reads. Checked after the hold, so a refusal releases it.
//! 5. **A durable writer era** (`dlm_term > 0`). Bit 7 is in the required
//!    set, so the D0 gate has published one by the time we get here; a zero
//!    era would mean every custody epoch is a constant, i.e. the fence
//!    could not express a revocation at all.
//! 6. **A bind address** for the custody + publish authority. A
//!    multi-writer mount that serves neither is inert, so `off` is a
//!    refusal rather than a silent no-op.
//!
//! # What the arm CANNOT do yet in the field, stated plainly
//!
//! Two things stand between this code and a live two-host cluster, and
//! neither is S9's to fix quietly:
//!
//! * **Nothing stamps the capability bits** (ruling D9). Every volume in
//!   the field fails rung 2. The Phase-8 batched reformat window is where
//!   they land; the offline `set_*_bit` paths named in the refusal are what
//!   it calls, and `tests/dlm_multi_writer_tests.rs` calls them too.
//! * **The D0 Layer-B2 gate refuses a fresh foreign claim on every
//!   substrate** (`kv/backend.rs`'s `writer_guard_gate`), unconditionally
//!   and regardless of the claim-set bit. So a second *write* mount of one
//!   volume set cannot open the metadata volumes at all, and the co-writer
//!   posture this module's client half implements is unreachable from
//!   `main` until that gate learns to admit a co-member of an ENGAGED claim
//!   set (§6.2 item 7's consumer half). That change belongs with the
//!   stamped bit and a PR-capable fleet, not with a mechanism landing.
//!
//! What the arm therefore delivers today is the **authority** half in
//! production shape — a mount that can grant remote write custody and serve
//! the publish path for peers — plus a fully wired client half that the
//! suite exercises against it. The gap is named, not papered over.

use crate::data_custody::{self, CustodyPosture, WeroHold};
use crate::data_grant::{self, AsyncVerbRouter, CustodyQuarantine, WriteCustodyOwner};
use crate::error::{Result, SqueezefsError};
use crate::membership::{LeaseClock, LeaseClocks};
use crate::meta_backend::kv::superblock as sb;
use crate::meta_backend::RoutedMetaBackend;
use crate::meta_ship::{publish, OwnerMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Where this mount serves the custody + publish authority
/// (`SQUEEZEFS_MW_BIND`).
pub const MW_BIND_ENV: &str = "SQUEEZEFS_MW_BIND";

/// The `features_incompat` bits a multi-writer mount REQUIRES, and the
/// reason each one is not optional:
///
/// | Bit | Capability | Why a second writer is unsound without it |
/// |---|---|---|
/// | 7 | durable writer term | a custody epoch that restarts at every mount cannot express a revocation, and every fencing token would be era-less (§6.7 decision 4) |
/// | 9 | durable block refcounts | §6.2 item 1, verbatim: *"Without durable shared ownership accounting, no multi-writer data path is expressible"* — two writers each derive a private answer from the subtree they walked |
/// | 10 | writer-scoped staging | §6.2 items 8/10: unlabelled `active_block:`/`mapping:` keys and a node-blind staging root let one node adopt or wipe a peer's staged payloads |
/// | 11 | multi-writer data | S7's capability gate: the format's recovery paths are expressed for more than one data-plane writer |
/// | 13 | `offset ‖ incarnation` block keys | §6.3's first coherence obligation: with bare reusable offsets a stale binding is structurally UNDETECTABLE, so a peer serves another file's bytes with no error and no counter |
/// | 14 | the claim-set record | §6.2 item 7: `writer_claim` is singular — it expresses exclusion, not partition membership, and the registrant keys a preempt (i.e. a drain proof) needs live in the set |
///
/// **Deliberately NOT required**: bit 8 (partitioned append) and bit 12
/// (per-writer ino lanes). Both express **two appenders on ONE volume**,
/// and S9's ownership granularity is the VOLUME (spec §6.10 R4) — so every
/// §6.2 single-appender structure still has exactly one appender, exactly
/// as S8 argued when it stamped no bit at all. Requiring them would demand
/// a format change for a shape this stage never produces.
///
/// **S9 takes no new bit.** Every capability it gates on was already built
/// by the §6.2 format work; this program has had four parallel bit
/// collisions, and not taking a fifth is the safest available answer.
pub const REQUIRED_INCOMPAT: u64 = sb::FEATURE_INCOMPAT_KV_DURABLE_TERM
    | sb::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS
    | sb::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING
    | sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA
    | sb::FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION
    | sb::FEATURE_INCOMPAT_KV_CLAIM_SET;

/// The offline stamping verb for one required bit — what the refusal names,
/// so an operator learns what the Phase-8 reformat window owes rather than
/// only that something is missing.
fn stamping_path(bit: u64) -> &'static str {
    match bit {
        b if b == sb::FEATURE_INCOMPAT_KV_DURABLE_TERM => "superblock::set_durable_term_bit",
        b if b == sb::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS => "superblock::set_block_refcounts_bit",
        b if b == sb::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING => {
            "superblock::set_writer_scoped_staging_bit"
        }
        b if b == sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA => {
            "superblock::set_multi_writer_data_bit"
        }
        b if b == sb::FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION => {
            "superblock::set_block_key_incarnation_bit"
        }
        b if b == sb::FEATURE_INCOMPAT_KV_CLAIM_SET => "superblock::set_claim_set_bit",
        _ => "the Phase-8 reformat window",
    }
}

fn bit_name(bit: u64) -> &'static str {
    match bit {
        b if b == sb::FEATURE_INCOMPAT_KV_DURABLE_TERM => "durable writer term",
        b if b == sb::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS => "durable block refcounts",
        b if b == sb::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING => "writer-scoped staging",
        b if b == sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA => "multi-writer data plane",
        b if b == sb::FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION => {
            "offset-with-incarnation block keys"
        }
        b if b == sb::FEATURE_INCOMPAT_KV_CLAIM_SET => "the claim-set record",
        _ => "an unnamed capability",
    }
}

/// The mount's quarantine sink: a dead co-writer's declared destinations,
/// admitted to S7's do-not-reallocate cohort on this mount's data volumes.
///
/// **The predicate is capacity, not the refcount map, and the reason is the
/// honest shape of this stage.** A co-writer allocates from ITS OWN
/// process's [`crate::block_allocator::BlockAllocator`], so the authority's
/// refcount map has no entry for the offset at all — asking it would answer
/// "unknown" and quarantine nothing. Admitting on every data volume whose
/// capacity contains the offset over-quarantines at most one offset per
/// volume until the drain proof arrives, which is bounded, visible
/// (`dlm_quarantined_offsets`) and safe; under-quarantining would hand a
/// possibly-live zombie's offset to a new owner, which is silent corruption.
///
/// **The unambiguous form is `(volume, offset)`** and it is a named
/// residual: the wire would carry the data volume's durable `vol-{hex}` id
/// beside each offset, exactly as `TREE_BLOCK_REFS` keys do (KD-5).
pub struct RouterQuarantine {
    router: Arc<crate::routing::BackendRouter>,
}

impl std::fmt::Debug for RouterQuarantine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouterQuarantine")
            .field("data_volumes", &self.router.backends.len())
            .finish()
    }
}

impl RouterQuarantine {
    /// A sink over every data volume this mount routes to.
    pub fn new(router: Arc<crate::routing::BackendRouter>) -> Arc<Self> {
        Arc::new(Self { router })
    }
}

impl CustodyQuarantine for RouterQuarantine {
    fn quarantine(&self, offsets: &[u64], epoch: crate::data_custody::DeadEpoch) -> usize {
        let mut admitted = 0;
        for entry in self.router.backends.iter() {
            let alloc = &entry.value().block_allocator;
            let cap = alloc.capacity_bytes();
            for &offset in offsets {
                if cap == 0 || offset < cap {
                    admitted += usize::from(alloc.quarantine_offset(offset, epoch));
                }
            }
        }
        admitted
    }

    fn release(&self, epoch: crate::data_custody::DeadEpoch) -> usize {
        self.router
            .backends
            .iter()
            .map(|entry| entry.value().block_allocator.release_quarantine(epoch))
            .sum()
    }
}

/// Where the authority binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bind {
    /// Ruling D2's posture: an ephemeral port on every interface.
    Auto,
    /// An explicit address.
    Addr(std::net::SocketAddr),
    /// Explicitly off — a refusal for a multi-writer mount (see the ladder).
    Off,
}

fn resolve_bind() -> Result<Bind> {
    let raw = std::env::var(MW_BIND_ENV).unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("auto") {
        return Ok(Bind::Auto);
    }
    if raw.eq_ignore_ascii_case("off") {
        return Ok(Bind::Off);
    }
    raw.parse::<std::net::SocketAddr>()
        .map(Bind::Addr)
        .map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "{MW_BIND_ENV}='{raw}' is neither `auto`, `off`, nor an `addr:port` ({e}) — \
                 refusing rather than serving write custody somewhere the operator did not ask \
                 for"
            ))
        })
}

/// What a multi-writer mount armed, and the teardown that undoes it.
pub struct MultiWriterArm {
    listener: Option<Arc<crate::cluster_wire::RpcListener>>,
    owner: Arc<WriteCustodyOwner>,
    wero: Option<WeroHold>,
    stop: Arc<AtomicBool>,
    endpoint: String,
}

impl std::fmt::Debug for MultiWriterArm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiWriterArm")
            .field("endpoint", &self.endpoint)
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

impl MultiWriterArm {
    /// The address peers reach this mount's custody + publish authority on.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The custody authority.
    pub fn owner(&self) -> &Arc<WriteCustodyOwner> {
        &self.owner
    }

    /// Stop serving: end the cadence task, drop the listener, restore solo
    /// ownership, uninstall both custody halves, and release the WERO hold
    /// off the async runtime (its ioctls are blocking).
    pub async fn disarm(mut self) {
        // Stop latch (the D0 heartbeat precedent — sqz-meta tasks are
        // never aborted mid-poll): the sweep loop checks `stop` after
        // every cadence sleep and exits before touching the owner again.
        self.stop.store(true, Ordering::Release);
        if let Some(listener) = self.listener.take() {
            listener.shutdown();
        }
        data_grant::uninstall_custody_client();
        data_grant::uninstall_custody_owner();
        publish::uninstall_client();
        crate::meta_ship::uninstall_delegation_host();
        crate::meta_ship::disarm_ownership();
        // The lane map dies with the authority (it is era-scoped), and so does
        // the frontier source a served OPEN read. The allocators keep their
        // engaged lanes: a mount's own residue class is fixed for its life
        // (`install_mount_partition` refuses a swap), and the offsets it has
        // minted are in it.
        crate::alloc_lane_grant::uninstall_frontier_source();
        // The shipped-free EXECUTOR dies with the authority too: a served
        // free with no executor refuses loud rather than stranding half a
        // ladder on a disarmed mount. Same for its reuse half.
        publish::uninstall_free_executor();
        publish::uninstall_harvest_executor();
        if let Some(hold) = self.wero.take() {
            data_custody::release_hold(hold).await;
        }
        log::warn!(
            "multi-writer DISARMED: solo authority restored, no custody is granted to peers, \
             and the data-namespace WERO hold is released"
        );
    }
}

/// `true` ⇔ this mount was asked to arm the multi-writer planes
/// (`SQUEEZEFS_MULTI_WRITER=1` — the same knob S7 introduced, whose meaning
/// S9 completes).
pub fn requested() -> bool {
    data_custody::multi_writer_requested()
}

/// Mount-path entry point: arm the multi-writer planes when the operator
/// asked for them, else `Ok(None)` — the shipped single-writer posture,
/// after one env read.
pub async fn arm_mount_multi_writer(
    meta: &Arc<RoutedMetaBackend>,
    data_paths: &[PathBuf],
    read_only: bool,
    quarantine: Option<Arc<dyn CustodyQuarantine>>,
    backend: Option<&Arc<crate::routing::BackendRouter>>,
) -> Result<Option<MultiWriterArm>> {
    if !requested() {
        log::debug!(
            "multi-writer not requested (SQUEEZEFS_MULTI_WRITER is off — the shipped posture: \
             the D0 guard arbitrates and the data plane is fenced locally by the custody epoch)"
        );
        // Rung 12, design §11: `SQUEEZEFS_DELEGATION` set on an unarmed
        // mount is ANNOUNCED-INERT — a startup notice, never a refusal
        // (the SQUEEZEFS_MW_ROLE "read only when armed" precedent).
        if std::env::var(crate::meta_ship::DELEGATION_ENV)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            log::info!(
                "SQUEEZEFS_DELEGATION is set but no multi-writer plane is armed on this mount — \
                 the S10 delegation lever is INERT here (every dlm_delegation gauge stays 0 by \
                 construction); it engages only on mounts whose ownership plane is armed"
            );
        }
        return Ok(None);
    }
    // DLM S9: this is the AUTHORITY's arm. A mount that declared
    // `SQUEEZEFS_MW_ROLE=co-writer` owns the other half of the posture —
    // `cowriter::arm`, whose ladder already decided it is admissible — and
    // running this one there would try to ACQUIRE a reservation the
    // authority holds and to serve custody this mount has no claim to
    // grant.
    if crate::cowriter::requested_role() == crate::cowriter::MwRole::CoWriter {
        log::info!(
            "multi-writer: this mount is a CO-WRITER (SQUEEZEFS_MW_ROLE=co-writer), so the \
             AUTHORITY arm is not run — the co-writer arm installs the client halves of the \
             same three planes (ownership + publish + custody) after its admission ladder \
             passes"
        );
        return Ok(None);
    }
    arm_multi_writer(meta, data_paths, read_only, quarantine, backend).await
}

/// **Arm the multi-writer planes, or refuse naming what is missing.**
///
/// Called only when multi-writer was demanded, so every failure here is a
/// refusal rather than a degradation: the operator asked for a guarantee
/// class, and half of it is not a class.
pub async fn arm_multi_writer(
    meta: &Arc<RoutedMetaBackend>,
    data_paths: &[PathBuf],
    read_only: bool,
    quarantine: Option<Arc<dyn CustodyQuarantine>>,
    backend: Option<&Arc<crate::routing::BackendRouter>>,
) -> Result<Option<MultiWriterArm>> {
    // Rung 1: a reader.
    if read_only {
        return Err(SqueezefsError::InvalidOperation(
            "multi-writer refuses to arm on a READ-ONLY mount: a reader takes no lease, writes \
             no claim and holds no NVMe registrant key (DLM S5's posture), so there is no write \
             custody to grant or hold. Drop -o ro / --read-only, or unset \
             SQUEEZEFS_MULTI_WRITER."
                .to_string(),
        ));
    }
    // Rung 2: the format's capability bits — before anything is acquired.
    check_capabilities(meta)?;

    // Rung 3: the substrate. S7's own arm takes (or JOINS) the WERO hold and
    // refuses a namespace that advertises no reservation support, naming it.
    let paths = data_paths.to_vec();
    let wero = squeezefs_ipc::sqz_blocking::run_blocking(move || {
        data_custody::arm_data_plane(CustodyPosture::MultiWriter, &paths, true)
    })
    .await?;

    // Rung 4: the membership plane. A co-writer that cannot be SEEN cannot
    // be EVICTED, and eviction is what mints the dead epoch S7's quarantine
    // is keyed on. Released-on-refusal, so a refused arm leaves no
    // reservation behind.
    if crate::membership::membership_mode() == "off" {
        if let Some(hold) = wero {
            data_custody::release_hold(hold).await;
        }
        return Err(SqueezefsError::InvalidOperation(
            "multi-writer refuses to arm: the membership plane is off, so co-writers cannot be \
             discovered, cannot appear in `squeezefs clients`, and — the part that matters — \
             cannot be EVICTED. S6's eviction is what mints the dead epoch whose offsets enter \
             the do-not-reallocate quarantine, so without it a dead co-writer's destinations \
             would be handed to a new owner. Set SQUEEZEFS_MEMBERSHIP_BIND (auto, or an \
             addr:port) on this mount."
                .to_string(),
        ));
    }

    // Rung 5: a durable era. Bit 7 is required above, so the D0 gate has
    // published one — a zero era here means the gate did not run.
    let term = crate::dlm::durable_term();
    if term == 0 {
        if let Some(hold) = wero {
            data_custody::release_hold(hold).await;
        }
        return Err(SqueezefsError::InvalidOperation(
            "multi-writer refuses to arm: this process has no durable writer era (dlm_term = \
             0), so every custody epoch would be the same constant and a revocation could not \
             be expressed at all. The era is published by the D0 mount gate's claim barrier on \
             a volume carrying incompat bit 7 — arm after it."
                .to_string(),
        ));
    }

    // Rung 5b (DLM S9's co-writer half — §6.2 item 7's PRODUCER side): commit
    // the operator-declared co-writer roster into every volume's durable
    // claim set. This is the ONLY place a co-writer's member entry is
    // written, and it must be the authority: the record is a metadata commit
    // on ino 1, and a co-writer holds no metadata authority over these
    // volumes — which is exactly why it cannot enroll itself and why its
    // admission rung 3 refuses an unenrolled node naming this knob.
    //
    // Deliberately BEFORE the listener starts: a co-writer that dials before
    // its enrollment is durable would be refused at rung 3, and an operator
    // reading the two logs would see the refusal without its cause.
    let roster = crate::cowriter::rostered_members();
    if !roster.is_empty() {
        match crate::cowriter::enroll_members(&meta.volumes, &roster, term).await {
            Ok(0) => log::warn!(
                "multi-writer: the co-writer roster {roster:?} committed NO claim-set entries — \
                 every volume of this set must carry incompat bit 14 for the record to exist \
                 (rung 2 refuses a half-engaged set, so those nodes will be refused with their \
                 own ids named)"
            ),
            Ok(n) => log::warn!(
                "multi-writer: {n} durable co-writer enrollment(s) committed for {roster:?} in \
                 era {term} — each named node may now pass admission rung 3"
            ),
            Err(e) => {
                if let Some(hold) = wero {
                    data_custody::release_hold(hold).await;
                }
                return Err(e);
            }
        }
    }

    // Rung 6: where the authority serves.
    let bind = match resolve_bind() {
        Ok(Bind::Off) => {
            if let Some(hold) = wero {
                data_custody::release_hold(hold).await;
            }
            return Err(SqueezefsError::InvalidOperation(format!(
                "multi-writer refuses to arm: {MW_BIND_ENV}=off, so this mount would grant no \
                 custody and serve no peer's publish path — an inert multi-writer mount is a \
                 misconfiguration, not a posture. Give it `auto` (an ephemeral port on every \
                 interface — ruling D2) or an addr:port, or unset SQUEEZEFS_MULTI_WRITER."
            )));
        }
        Ok(Bind::Auto) => "0.0.0.0:0".parse().expect("literal addr"),
        Ok(Bind::Addr(a)) => a,
        Err(e) => {
            if let Some(hold) = wero {
                data_custody::release_hold(hold).await;
            }
            return Err(e);
        }
    };

    // Everything demanded is present. Arm, in dependency order.
    let secret = cluster_secret(meta).await.ok_or_else(|| {
        SqueezefsError::InvalidOperation(
            "multi-writer refuses to arm: the volume set carries no `job:enroll` record, which \
             is this wire's root of trust (possession of volume access IS cluster membership — \
             ruling D2). Enable the cluster listener (SQUEEZEFS_JOB_WIRE_BIND) so the secret \
             exists."
                .to_string(),
        )
    })?;

    let clocks = LeaseClocks::derive(Duration::ZERO)?;
    let renew = clocks.renew_interval;
    let owner = WriteCustodyOwner::arm(
        &format!("mw-{}", uuid::Uuid::new_v4()),
        term,
        // The predecessor's era is the D0 gate's business, and it already
        // did it: S2's law is that a successor bumps `WriterClaim.term`
        // DURABLY BEFORE arming, so the era this mount holds is already
        // strictly greater than every predecessor's. Passing 0 records that
        // we rely on the gate's evidence rather than re-deriving it here
        // (the `term < durable_term()` guard inside `arm` is the backstop
        // against a caller that armed with an era the gate never published).
        0,
        clocks,
        LeaseClock::monotonic(),
        quarantine,
    )?;

    // DLM S9 blocker #3's ADMISSION (`crate::alloc_lane_grant`,
    // docs/design-mw-data-alloc-partition.md §9 item 1): this era's data-plane
    // allocation lane map, derived from the DURABLE claim set — the record
    // this arm just wrote the roster into, and the only source that can make
    // two nodes agree without a message. The authority keeps lane 0; each
    // enrolled writer member gets the next lane; the width is fixed for the
    // era, and it reaches every member on its custody lease.
    //
    // With NO enrolled co-writer this is lane 0 of 1 = SOLO, which installs
    // nothing at all: the authority's own allocation is then not "equivalent
    // to" the shipped path, it IS the shipped path.
    let assignment = match derive_lane_assignment(meta).await {
        Ok(a) => a,
        Err(e) => {
            if let Some(hold) = wero {
                data_custody::release_hold(hold).await;
            }
            return Err(e);
        }
    };
    owner.install_lane_assignment(Arc::clone(&assignment));
    // Rung 10 — phantom-era frontier hygiene (rung-8 finding #4's named
    // residual): records of a width no live era runs are pruned HERE, at
    // the one node that both knows the era's width (it just derived it
    // from the durable claim set) and holds the metadata authority to
    // delete them. The pass folds a foreign-width record's protection
    // into every current-width lane FIRST (clause 3 preserved through the
    // delete); a solo era deletes outright — its D0 admission is the
    // no-live-peer proof and a solo engagement reads no records at all.
    // An undecodable record refuses the arm loud (the load law: a
    // watermark we cannot read is a floor we cannot honour — and one we
    // must not delete).
    match crate::data_alloc_lane::prune_stale_lane_records(meta, assignment.writers()).await {
        Ok(report) if report.pruned > 0 => {
            log::warn!(
                "multi-writer arm: pruned {} stale allocation-lane record(s) ({} folded into \
                 the current {}-way era first) — retired-width residue is gone from this set \
                 (alloc_lane_stale_records_pruned)",
                report.pruned,
                report.folded,
                assignment.writers(),
            );
        }
        Ok(_) => {}
        Err(e) => {
            if let Some(hold) = wero {
                data_custody::release_hold(hold).await;
            }
            return Err(SqueezefsError::InvalidOperation(format!(
                "multi-writer refuses to arm: the allocation-lane hygiene pass failed ({e}). A \
                 lane record that cannot be read or folded is a frontier that cannot be \
                 honoured — arming over it could let this era mint offsets a retired era's \
                 fenced writer may still hold"
            )));
        }
    }
    let authority_lane = assignment.authority_partition();
    if !authority_lane.is_solo() {
        let Some(backend) = backend else {
            if let Some(hold) = wero {
                data_custody::release_hold(hold).await;
            }
            return Err(SqueezefsError::InvalidOperation(format!(
                "multi-writer refuses to arm: this era's claim set enrolls {} co-writer(s), so \
                 the data plane must be partitioned into {} allocation lanes — but this arm was \
                 given no data-plane router to engage them on. An authority that granted lanes \
                 to peers while minting DENSE offsets itself would hand one device offset to two \
                 owners, silently on a passthrough volume",
                assignment.writers().saturating_sub(1),
                assignment.writers(),
            )));
        };
        // The dense-frontier source a served lane OPEN reads: a co-writer
        // never walks the tree, so the authority answers where the live data
        // ends from its own recovered cursor (composed with the durable
        // ledger).
        crate::alloc_lane_grant::install_frontier_source(
            crate::alloc_lane_grant::router_frontier_source(Arc::clone(backend)),
        );
        // The shipped-free EXECUTOR (DLM S9's co-writer free path,
        // `crate::cowriter::execute_shipped_frees`): a co-writer's
        // displaced-block terminal frees travel as publish verbs, and this
        // is the owner half that runs the full ladder — RAM release, tier
        // purge, reclaim enqueue, finish_free with the grace ring and the
        // quarantine composing inside it — against THIS mount's data
        // plane. Installed with the lane machinery because they are the
        // two halves of one rewrite: a lane grants the NEW block, the
        // executor retires the DISPLACED one.
        publish::install_free_executor(crate::cowriter::router_free_executor(
            Arc::clone(backend),
            Arc::clone(meta),
        ));
        // The free's REUSE half (rung 10, residual 2): the lane free
        // HARVEST executor — what makes a co-writer's freed supply
        // reachable again. Installed together because a set that returns
        // offsets to a lane's supply but can never hand them back leaks
        // toward ENOSPC on a store with free space.
        publish::install_harvest_executor(crate::cowriter::router_harvest_executor(Arc::clone(
            backend,
        )));
        if let Err(e) =
            crate::alloc_lane_grant::engage_authority_lanes(authority_lane, backend, meta).await
        {
            crate::alloc_lane_grant::uninstall_frontier_source();
            publish::uninstall_free_executor();
            publish::uninstall_harvest_executor();
            if let Some(hold) = wero {
                data_custody::release_hold(hold).await;
            }
            return Err(e);
        }
    }

    // Rung 9 — the S8 arm's owner half: the shipped `Metadata` verb
    // block (VERB_META_BATCH/VERB_RECLAIM) on the SAME listener. The
    // service adopts the durable era at construction
    // (`crate::dlm::durable_term()` — rung 5 above proved it nonzero),
    // so a successor authority's higher term makes every old-era
    // frame stale by construction (S8's era gate).
    let meta_svc = crate::meta_ship::MetaShipService::new(Arc::clone(meta));
    // Rung 12 — the S10 delegation host: the SAME service serves the
    // DelegRecall/DelegReassert block, and installing it is what arms the
    // coherence gate on this backend's mutation surface (grants may only
    // exist where the recall-before-conflicting-publish law is enforced).
    crate::meta_ship::install_delegation_host(Arc::clone(&meta_svc));
    let router = AsyncVerbRouter::new()
        .with_custody(Arc::clone(&owner))
        .with_publish(publish::PublishService::new(Arc::clone(meta)))
        .with_meta(meta_svc);
    let listener = match crate::cluster_wire::RpcListener::start_async(
        crate::cluster_wire::RpcListenerConfig {
            bind_addr: bind,
            security: None,
            service_threads: crate::cluster_wire::default_service_threads(),
            ..crate::cluster_wire::RpcListenerConfig::default()
        },
        secret.clone(),
        Arc::new(router),
    ) {
        Ok(l) => l,
        Err(e) => {
            // A host with no listener can never receive an ack — grants
            // must not exist without their recall wire.
            crate::meta_ship::uninstall_delegation_host();
            return Err(e);
        }
    };
    let endpoint = format!(
        "{}:{}",
        crate::cluster_wire::local_advertise_ip(),
        listener.endpoint().port()
    );
    data_grant::install_custody_owner(Arc::clone(&owner));
    publish::install_client(publish::PublishClient::new(owner.id(), secret));

    // The ownership plane (S8's un-called arm). The map is derived from what
    // the set durably says about custody: this node holds the D0 claim on
    // every volume it mounted — the gate refuses otherwise — so the derived
    // map is all-local today, and the honest consequence is that publish
    // routing and lock homing take their local arms while the SHIPPING
    // halves stay exercised only where a foreign volume exists. What would
    // populate foreign entries is the claim-set's per-volume ownership,
    // which needs the D0 Layer-B2 gate to admit a co-member (see the module
    // docs).
    let map = OwnerMap::for_volumes(meta, Vec::new())?;
    let foreign = map.volume_count() - map.local_volumes();
    crate::meta_ship::arm_ownership(map);

    let stop = Arc::new(AtomicBool::new(false));
    spawn_cadence(Arc::clone(&owner), wero.clone(), Arc::clone(&stop), renew);

    log::warn!(
        "MULTI-WRITER ARMED (DLM S9) on {endpoint}: era {term}, custody authority '{}', \
         data-plane fence class {}, {} of {} metadata volume(s) owned elsewhere. Peers may now \
         acquire write custody here and DMA directly to the shared namespaces — only custody \
         travels, never data (spec §6.9 S9)",
        owner.id(),
        data_custody::wero_mode(),
        foreign,
        meta.volumes.len(),
    );
    Ok(Some(MultiWriterArm {
        listener: Some(listener),
        owner,
        wero,
        stop,
        endpoint,
    }))
}

/// Derive this era's allocation lane map from the volume set's **durable**
/// claim sets (DLM S9 blocker #3's admission).
///
/// The authority's own identity in that record is the **membership owner's**
/// id — the entry `membership::arm_mount_membership` writes for itself — not
/// the custody authority's `mw-{uuid}`: the claim set is §6.2 item 7's
/// membership record, and the roster this arm enrolled sits beside that one
/// entry. Without an armed membership owner there is no such identity, and
/// then this authority runs SOLO rather than guessing which entry is itself
/// (guessing wrong would hand a co-writer the authority's own lane).
async fn derive_lane_assignment(
    meta: &Arc<RoutedMetaBackend>,
) -> Result<Arc<crate::alloc_lane_grant::LaneAssignment>> {
    let me = match crate::membership::installed_owner() {
        // Rung-8 finding #4: the ONE-identity law — the derivation must use
        // the same DURABLE claim identity `arm_owner` upserted (the owner's
        // id is the INCARNATION uuid; passing it made the authority's own
        // claim entry read as a foreign co-writer, so every solo MW mount
        // ran a phantom W=2 partition against itself and paid a durable
        // reservation frontier fsck C6 correctly reported as drift).
        Some(owner) => crate::membership::owner_claim_identity(owner.id()),
        None => {
            log::warn!(
                "multi-writer: no membership OWNER is installed on this mount, so its own \
                 claim-set identity is unknown — running SOLO (no data-plane allocation \
                 partition) rather than risking handing a co-writer this node's own lane"
            );
            return crate::alloc_lane_grant::LaneAssignment::derive("", &[]);
        }
    };
    let mut sets = Vec::new();
    for vol in &meta.volumes {
        if let Some(set) = crate::membership::ClaimSet::load(vol).await {
            // Only the DURABLE record is a roster: the projection of a
            // singular `writer_claim` expresses EXCLUSION and cannot name a
            // second member (the co-writer ladder's rung 2, same reason).
            if set.durable {
                sets.push(set);
            }
        }
    }
    crate::alloc_lane_grant::LaneAssignment::derive(&me, &sets)
}

/// Every meta volume must carry every required bit. The refusal names the
/// FIRST missing one, its meaning and its offline stamping path.
fn check_capabilities(meta: &Arc<RoutedMetaBackend>) -> Result<()> {
    if meta.volumes.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "multi-writer refuses to arm: the metadata set has no volumes".to_string(),
        ));
    }
    for vol in &meta.volumes {
        let have = vol.superblock().features_incompat;
        let missing = REQUIRED_INCOMPAT & !have;
        if missing != 0 {
            let bit = 1u64 << missing.trailing_zeros();
            return Err(SqueezefsError::InvalidOperation(format!(
                "multi-writer refuses to arm: metadata volume {} does not carry incompat bit {} \
                 ({}), so the format cannot express a second writer safely. Nothing stamps it \
                 today (ruling D9: the bits are built, not stamped) — the Phase-8 reformat \
                 window stamps it offline via {}. Missing mask {:#x} of the required {:#x}.",
                vol.device_path().display(),
                missing.trailing_zeros(),
                bit_name(bit),
                stamping_path(bit),
                missing,
                REQUIRED_INCOMPAT
            )));
        }
    }
    Ok(())
}

async fn cluster_secret(meta: &Arc<RoutedMetaBackend>) -> Option<Vec<u8>> {
    let first = meta.volumes.first()?;
    crate::membership::cluster_secret(first).await
}

/// The authority's cadence task: sweep expired client leases, and turn each
/// death into a **drain proof** where the substrate allows one.
///
/// The proof ladder is the job wire's §5.1.6 rung 2, reused verbatim: PR-
/// preempt the dead co-writer's registrant key under the standing WERO hold
/// (its resumed DMA is then rejected by the DEVICE), and only then release
/// its quarantined offsets. Without a registrant key — a detection-grade
/// peer — the offsets stay quarantined until recovery produces an attested
/// proof, which is honestly-unavailable space rather than a silent
/// reallocation.
fn spawn_cadence(
    owner: Arc<WriteCustodyOwner>,
    wero: Option<WeroHold>,
    stop: Arc<AtomicBool>,
    cadence: Duration,
) {
    crate::meta_exec::spawn_meta_contained("mw_custody_sweep", async move {
        loop {
            squeezefs_ipc::sqz_time::sleep(cadence).await;
            if stop.load(Ordering::Acquire) {
                return;
            }
            // Rung-9 finding #2: RE-VERIFY the standing WERO against the
            // device every sweep — before this, the fence-mode gauge read
            // 1 forever, even over a reservation the device no longer
            // held (a vanished hold re-acquires and counts on
            // `data_plane_wero_reacquires`; a foreign-usurped hold
            // poisons custody and drops the gauge — never healed over).
            if let Some(hold) = wero.clone() {
                let verdict =
                    squeezefs_ipc::sqz_blocking::run_blocking(move || hold.reverify_and_heal())
                        .await;
                match verdict {
                    crate::data_custody::WeroReverify::Held
                    | crate::data_custody::WeroReverify::Healed => {}
                    lost => {
                        log::error!(
                            "S9 custody sweep: the data-plane WERO hold is gone ({lost:?}) — \
                             custody is poisoned and no dead-epoch preempt can be issued from \
                             a fence this mount no longer holds"
                        );
                        continue;
                    }
                }
            }
            for dead in owner.expire_due() {
                log::warn!(
                    "S9: swept co-writer '{}' (lease epoch {}) — {}; {} offset(s) quarantined \
                     under {}",
                    dead.client,
                    dead.lease_epoch,
                    dead.reason,
                    dead.offsets.len(),
                    dead.epoch
                );
                if dead.offsets.is_empty() {
                    continue;
                }
                let Some(hold) = wero.clone() else {
                    continue;
                };
                if dead.pr_key == 0 {
                    log::warn!(
                        "S9: dead co-writer '{}' published no NVMe registrant key, so no \
                         preempt can prove it drained — its {} offset(s) stay quarantined until \
                         recovery attests (honestly-unavailable space, never a reallocation)",
                        dead.client,
                        dead.offsets.len()
                    );
                    continue;
                }
                let victim = dead.pr_key;
                // Rung-9 own-key guard: a CO-LOCATED co-writer's adopted
                // fenceability key IS this authority's own (shared PR
                // arbitration domain — the ops.md honest residual), and
                // preempting it would take down the authority's own fence.
                // Same-host death is the dead-pid ladder's proof, never a
                // PR preempt; its offsets stay quarantined until recovery
                // attests, exactly like the key-less arm above.
                if victim == hold.key() {
                    log::warn!(
                        "S9: dead co-writer '{}' is fenced by THIS authority's own key \
                         {victim:#x} (the co-located adopted shape) — no preempt can prove it \
                         drained without destroying our own fence; its {} offset(s) stay \
                         quarantined until recovery attests",
                        dead.client,
                        dead.offsets.len()
                    );
                    continue;
                }
                let landed =
                    squeezefs_ipc::sqz_blocking::run_blocking(move || hold.preempt(victim)).await;
                match data_grant::DrainProof::preempt_landed(landed) {
                    Some(proof) => {
                        owner.release_dead(&dead, proof);
                    }
                    None => log::error!(
                        "S9: the WERO preempt of dead co-writer '{}' (key {:#x}) landed on ZERO \
                         namespaces — no drain proof exists, so its offsets stay quarantined",
                        dead.client,
                        victim
                    ),
                }
            }
        }
    })
}
