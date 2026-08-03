//! DLM **stage S9 — the CO-WRITER mount posture and its admission gate**
//! (pre-RC engineering spec §6.2 item 7's *consumer* half, §6.9 S9;
//! contracts `tests/dlm_cowriter_tests.rs`; operator surface
//! `docs/operations.md` §Multi-writer co-writer mounts).
//!
//! # The blocker this module closes, in S9's own words
//!
//! > *"The D0 Layer-B2 gate refuses a fresh foreign claim on every
//! > substrate, unconditionally and regardless of bit 14. A co-writer holds
//! > no `writer_claim`, so it cannot open the metadata volumes at all; the
//! > co-writer posture is unreachable from `main` until that gate admits a
//! > co-member of an ENGAGED claim set."*
//!
//! and: *"a metadata-read-only/data-read-write posture does not exist
//! (`BlockAllocator::reader_gate` refuses on `-o ro`)."*
//!
//! # What a co-writer IS
//!
//! | Plane | A co-writer's authority | Where that is enforced |
//! |---|---|---|
//! | metadata | **none locally.** Every mutation is SHIPPED to the volume's authority (S8 ownership + S9's publish vocabulary) | [`crate::meta_backend::kv::backend::KvMetaBackend::open_co_writer`] latches the volume read-only with cause `CoWriterMount`, so `write_gate` refuses a local commit naming the shipped path |
//! | data (DMA) | **yes, under a granted custody lease.** The bytes never funnel through the authority — only the custody travels | [`crate::data_custody::authorize_dma`], the ONE authorization point, under the epoch the grant established |
//! | ownership accounting (allocate / terminal free / W1 incarnation retire) | **none.** The durable truth of "who owns this offset" is metadata (`TREE_BLOCK_REFS`), and metadata authority is the authority's | `BlockAllocator`'s gate, whose co-writer class names the data-plane allocation partition that owns it |
//!
//! That split is the honest reading of the blocker: *"a co-writer holding a
//! valid S9 grant has authority for the data plane while having none for
//! the metadata plane"* — and block ownership accounting is metadata, not
//! data.
//!
//! # The admission ladder, in evaluation order
//!
//! Cheapest and most declarative first; nothing that MUTATES anything
//! (the device registration of rung 5) runs until every earlier rung has
//! passed, and a later refusal undoes it.
//!
//! | Rung | Requirement | The threat it answers |
//! |---|---|---|
//! | **1** | `SQUEEZEFS_MULTI_WRITER=1`, `SQUEEZEFS_MW_ROLE=co-writer`, a declared `SQUEEZEFS_MW_AUTHORITY`, and NOT `-o ro` | an accidental second write mount. The posture must be **declared**, never inferred from evidence, so a plain `mount` of a claimed set still refuses `FreshForeign` verbatim — the D0 gate itself is untouched by this module |
//! | **2** | every volume of the set carries the six S9 capability bits, **bit 14 (`KV_CLAIM_SET`) included**, and its claim set is the DURABLE record | a half-engaged set is not a claim set. Un-engaged, membership IS the singular `writer_claim` read through a projection — a record that expresses *exclusion* and cannot represent a second member, so admitting off it would be admitting off nothing |
//! | **3** | that durable set names **this node** as a `Writer` member | self-assertion. Admission is by durable **enrollment**, written by the authority (see below), never by a claim the joining node makes about itself at open time |
//! | **4** | the membership plane is armed, this mount holds a **live** member lease from the authority, and the rendezvous era is not older than the claim's | a co-writer with no live authority has no custody source and — the part that matters — no **evictor**: S6's eviction is what mints the dead epoch its offsets are quarantined under. It must refuse, not proceed hopefully |
//! | **5** | a PR-capable substrate whose **standing WERO (rtype 2) hold names this node as a registrant** | §6.7's *"multi-writer refuses to arm on non-PR substrates"* applies to the ADMISSION decision too. A co-writer whose DMA the device cannot reject is a co-writer nothing can fence, and its death could never produce a drain proof (the proof IS the preempt of this registrant key) |
//!
//! # Who writes the claim-set member entry (the crux)
//!
//! **The authority, and only the authority.** The `claim_set` record is an
//! xattr commit on ino 1 — a metadata mutation — and a co-writer holds no
//! metadata authority by construction. So a co-writer *cannot* enroll
//! itself, and this module does not pretend otherwise: enrollment is an act
//! of the authority over its operator-declared roster
//! ([`MW_MEMBERS_ENV`] → [`enroll_members`], committed at the authority's
//! multi-writer arm).
//!
//! That forces the enrollment identity to be a **node** identity rather
//! than a per-mount uuid: an entry must exist *before* the mount that would
//! mint a uuid exists. It is therefore the writer-scope node token
//! ([`crate::writer_scope::resolve_node_identity`], rendered
//! `node_{16 hex}`) — already required here because incompat bit 10 is in
//! the capability set, already host-stable and reboot-stable by contract,
//! and already the thing that scopes this node's staged payloads. A refused
//! co-writer prints its own token, so the operator's step is mechanical:
//! copy it into the authority's `SQUEEZEFS_MW_MEMBERS`, restart the
//! authority's arm, remount the co-writer.
//!
//! # What a co-writer takes on `flock` — nothing
//!
//! A co-writer takes the S5 reader's **released `LOCK_SH` probe**
//! (classification for the mount log) and retains no lock, for reasons that
//! are subtler than the reader's:
//!
//! * a retained `LOCK_SH` would deny the authority its daemon-lifetime
//!   `LOCK_EX` when both live on one host — and, order-reversed, a local
//!   authority would deny every co-writer;
//! * **two co-writer mounts on ONE host are legitimate** (separate mount
//!   points), and their mutual exclusion is the authority's custody
//!   arbitration, not a local lock;
//! * the thing Layer A protects is the metadata volume's *single-appender*
//!   structures (journal ring, A/B extent bitmap, 32-slot root ledger). A
//!   co-writer appends to none of them: its metadata mutations ship. It
//!   therefore needs no exclusion of that volume and grants none.
//!
//! And it cannot be mistaken for an owner by the recovery ladder, because
//! it leaves **no evidence of ownership** anywhere the ladder looks: no
//! `writer_claim` (so `squeezefs clients`' live/stale/dead classification
//! and `claim clear` both see only the authority's record), no retained
//! flock (so the dead-pid proof's "the flock was free" premise is
//! unaffected), and no meta-namespace PR registrant (so a successor's
//! preempt arbitration sees exactly the registrants it saw before). What it
//! DOES leave is a data-namespace registrant key and a claim-set member
//! entry — both deliberately, because those are what make it fenceable and
//! visible.
//!
//! # What a co-writer does about allocation today
//!
//! It **refuses fresh allocation**, loudly, naming the data-plane
//! allocation partition that owns the problem. The alternative — taking
//! pre-allocated destinations the way the job wire's remote workers do —
//! is expressible (the S9 grant already carries a declared-destination
//! cohort) but is not a write PATH: the layout publish, the displaced-block
//! free and the refcount delta of every write would each have to ride the
//! authority, and only the publish half is landed. Refusing at the
//! allocation point keeps the seam in ONE place and keeps the failure loud
//! instead of half-wired.

use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use crate::membership::{ClaimSet, MemberRole};
use crate::meta_backend::kv::backend::{KvMetaBackend, WriterClaim};
use crate::meta_backend::RoutedMetaBackend;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Which half of the multi-writer plane this mount is (`SQUEEZEFS_MW_ROLE`).
pub const MW_ROLE_ENV: &str = "SQUEEZEFS_MW_ROLE";
/// The `addr:port` a co-writer dials for custody + the shipped publish path.
pub const MW_AUTHORITY_ENV: &str = "SQUEEZEFS_MW_AUTHORITY";
/// The AUTHORITY's operator-declared co-writer roster (comma list of node ids).
pub const MW_MEMBERS_ENV: &str = "SQUEEZEFS_MW_MEMBERS";

/// The capability bits a co-writer's set must carry — **exactly** S9's arm
/// requirement, re-exported so the two halves of one posture can never
/// disagree about what the format must express.
pub use crate::multi_writer::REQUIRED_INCOMPAT;

/// Which half of the multi-writer plane this mount was asked to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MwRole {
    /// The shipped posture: hold the D0 claim, serve custody and the
    /// publish path, own the claim-set roster.
    Authority,
    /// Metadata read-only locally (mutations shipped), data read-write
    /// under a granted custody lease.
    CoWriter,
}

impl MwRole {
    /// The operator-facing word (the knob's value and the stats gauge).
    pub fn as_str(self) -> &'static str {
        match self {
            MwRole::Authority => "authority",
            MwRole::CoWriter => "co-writer",
        }
    }
}

/// The requested role. Malformed values never fall back to a default —
/// `env_knobs`' startup gate refuses the process on an unregistered word,
/// and this reader is the enum's one consumer.
pub fn requested_role() -> MwRole {
    match crate::env_knobs::enum_knob(MW_ROLE_ENV, &["authority", "co-writer"], "authority") {
        "co-writer" => MwRole::CoWriter,
        _ => MwRole::Authority,
    }
}

/// `true` ⇔ this mount demands the CO-WRITER posture
/// (`SQUEEZEFS_MULTI_WRITER=1` **and** `SQUEEZEFS_MW_ROLE=co-writer`).
///
/// Refuses (rather than degrading to an authority mount) when the role is
/// declared without the opt-in: the D0 refusal must never be bypassed by
/// inference, and an operator who typed the role and got a silent authority
/// mount would learn nothing.
pub fn co_writer_requested() -> Result<bool> {
    let role = requested_role();
    let opted_in = crate::data_custody::multi_writer_requested();
    match (role, opted_in) {
        (MwRole::CoWriter, true) => Ok(true),
        (MwRole::CoWriter, false) => Err(SqueezefsError::InvalidOperation(format!(
            "{MW_ROLE_ENV}=co-writer without {}=1: the co-writer posture is HALF of the \
             multi-writer plane, and arming half of a guarantee class is not a class. Set \
             SQUEEZEFS_MULTI_WRITER=1 (and give this mount a PR-capable substrate, a stamped \
             format, a live authority and a durable enrollment — see docs/operations.md \
             §Multi-writer co-writer mounts), or unset {MW_ROLE_ENV}. Refusing rather than \
             mounting as an authority: that would be a SECOND write mount decided by \
             inference, which is exactly what the D0 guard exists to prevent",
            "SQUEEZEFS_MULTI_WRITER"
        ))),
        (MwRole::Authority, _) => Ok(false),
    }
}

/// The authority endpoint a co-writer dials, or `None` when undeclared.
pub fn declared_authority() -> Option<String> {
    std::env::var(MW_AUTHORITY_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("none"))
}

/// The AUTHORITY's operator-declared co-writer roster.
pub fn rostered_members() -> Vec<String> {
    std::env::var(MW_MEMBERS_ENV)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("none"))
        .collect()
}

/// This node's **durable enrollment identity**: `node_{16 hex}` over the
/// writer-scope node token (see the module docs on why it is a node
/// identity and not a mount uuid). Refuses loud exactly as
/// [`crate::writer_scope::resolve_node_identity`] does — an unstable
/// identity would enroll one host and admit another.
pub fn node_member_id() -> Result<String> {
    Ok(format!(
        "node_{:016x}",
        crate::writer_scope::resolve_node_identity()?.token
    ))
}

// ---------------------------------------------------------------------------
// The evidence the ladder decides over
// ---------------------------------------------------------------------------

/// One metadata volume's admission evidence, read through a probe open
/// (never blocked, never writes).
#[derive(Debug, Clone)]
pub struct VolumeAdmissionEvidence {
    /// The volume's device path (named in every refusal).
    pub path: PathBuf,
    /// `features_incompat` from the superblock the mount would open.
    pub features_incompat: u64,
    /// The volume's `writer_claim` — the authority's own durable record.
    pub claim: Option<WriterClaim>,
    /// The claim set as [`ClaimSet::load`] answers it: the durable record
    /// on an engaged volume, the singular projection otherwise (which
    /// rung 2 refuses — `durable == false`).
    pub claim_set: Option<ClaimSet>,
}

/// The membership plane's evidence: the rendezvous record the authority
/// published, plus whether THIS mount holds a live member lease from it.
#[derive(Debug, Clone)]
pub struct AuthorityLeaseEvidence {
    /// The membership owner's id (from its durable rendezvous record).
    pub owner_id: String,
    /// Where that owner serves the membership plane.
    pub endpoint: String,
    /// The durable writer era the owner armed in.
    pub term: u64,
    /// `true` ⇔ this mount JOINED and holds a lease — the liveness proof.
    /// A rendezvous record alone proves only that an owner once armed.
    pub live: bool,
    /// The member epoch the owner granted (informational; logged).
    pub member_epoch: u64,
}

/// The device's answer about the standing WERO hold on the data namespaces
/// (gathered by [`crate::data_custody::join_wero_as_registrant`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistrantEvidence {
    /// Every namespace advertises reservation support.
    pub pr_capable: bool,
    /// The held reservation is Write Exclusive – **Registrants Only**.
    pub wero: bool,
    /// A reservation is held at all (by the authority).
    pub reservation_held: bool,
    /// OUR key appears among the device's registrants.
    pub registered: bool,
    /// This node's registrant key (`0` = none).
    pub key: u64,
    /// Namespaces the join covered.
    pub namespaces: usize,
}

/// Everything the ladder decides over. The mount path's
/// [`gather_admission`] is what builds this in production; the suite builds
/// it directly (the `dlm_slot::test_set_local_slots` precedent) so every
/// rung's refusal is pinned without a fabric.
#[derive(Debug, Clone)]
pub struct AdmissionRequest {
    /// `SQUEEZEFS_MULTI_WRITER=1`.
    pub multi_writer: bool,
    /// `SQUEEZEFS_MW_ROLE=co-writer`.
    pub role_co_writer: bool,
    /// `-o ro` / `--read-only` (a category error for this posture).
    pub read_only: bool,
    /// This node's durable enrollment identity ([`node_member_id`]).
    pub node_id: String,
    /// The declared authority endpoint ([`declared_authority`]).
    pub custody_endpoint: Option<String>,
    /// Per-volume evidence, in set order.
    pub volumes: Vec<VolumeAdmissionEvidence>,
    /// The membership plane's evidence.
    pub authority: Option<AuthorityLeaseEvidence>,
    /// The device's evidence.
    pub registrant: Option<RegistrantEvidence>,
}

/// **The decision.** Constructible only by [`classify_admission`] — every
/// field is private, so no caller can open a co-writer backend without a
/// gate outcome in hand ([`KvMetaBackend::open_co_writer`] takes one).
#[derive(Debug, Clone)]
pub struct CoWriterAdmission {
    node_id: String,
    authority_claim_id: String,
    authority_term: u64,
    membership_owner_id: String,
    membership_endpoint: String,
    custody_endpoint: String,
    pr_key: u64,
    volumes: Vec<PathBuf>,
}

impl CoWriterAdmission {
    /// This node's durable enrollment identity (also its custody-client id
    /// and its claim-set member id).
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The D0 claim holder this co-writer is subordinate to.
    pub fn authority_claim_id(&self) -> &str {
        &self.authority_claim_id
    }

    /// The durable writer era the authority's claim names.
    pub fn authority_term(&self) -> u64 {
        self.authority_term
    }

    /// The membership owner whose live lease admitted us.
    pub fn membership_owner_id(&self) -> &str {
        &self.membership_owner_id
    }

    /// Where that owner serves the membership plane.
    pub fn membership_endpoint(&self) -> &str {
        &self.membership_endpoint
    }

    /// Where the custody + publish authority is dialled.
    pub fn custody_endpoint(&self) -> &str {
        &self.custody_endpoint
    }

    /// This node's device registrant key under the standing WERO hold.
    pub fn pr_key(&self) -> u64 {
        self.pr_key
    }

    /// The volumes this decision was made over.
    pub fn volumes(&self) -> &[PathBuf] {
        &self.volumes
    }

    /// `true` ⇔ `path` is one of them (the per-volume open's own check: an
    /// admission decided over one set may not open a volume of another).
    pub fn covers(&self, path: &Path) -> bool {
        self.volumes.iter().any(|v| v == path)
    }
}

// ---------------------------------------------------------------------------
// The ladder
// ---------------------------------------------------------------------------

fn refuse(rung: u8, detail: String) -> SqueezefsError {
    METRICS
        .cowriter_admission_refusals
        .fetch_add(1, Ordering::Relaxed);
    let msg = format!("co-writer admission REFUSED at rung {rung}: {detail}");
    log::error!("{msg}");
    SqueezefsError::InvalidOperation(msg)
}

/// Rung 1 — the posture is DECLARED, not inferred.
fn rung_1_declaration(req: &AdmissionRequest) -> Result<&str> {
    if !req.multi_writer {
        return Err(refuse(
            1,
            "SQUEEZEFS_MULTI_WRITER is off. The co-writer posture is half of the multi-writer \
             guarantee class; without the opt-in this mount is a single-writer mount and the D0 \
             guard arbitrates it, refusing a claimed set exactly as it always has"
                .to_string(),
        ));
    }
    if !req.role_co_writer {
        return Err(refuse(
            1,
            format!(
                "{MW_ROLE_ENV} is not `co-writer`. An AUTHORITY mount is never admitted as a \
                 co-writer by inference — the posture that opens a volume another node holds \
                 the D0 claim on must be stated by the operator"
            ),
        ));
    }
    if req.read_only {
        return Err(refuse(
            1,
            "this mount is READ-ONLY (`-o ro` / `--read-only`). A reader takes no lease, holds \
             no registrant key and mutates no plane (DLM S5's posture); demanding write custody \
             on it is a category error, not a degradation. Drop -o ro, or drop the co-writer \
             role"
                .to_string(),
        ));
    }
    let endpoint = req.custody_endpoint.as_deref().unwrap_or("").trim();
    if endpoint.is_empty() {
        return Err(refuse(
            1,
            format!(
                "no authority is declared ({MW_AUTHORITY_ENV} is unset). A co-writer acquires \
                 every write custody it holds from an authority and ships every metadata \
                 mutation to it, so a co-writer with nowhere to dial is inert — a \
                 misconfiguration, not a posture. Set {MW_AUTHORITY_ENV}=addr:port to the \
                 authority's SQUEEZEFS_MW_BIND endpoint"
            ),
        ));
    }
    Ok(endpoint)
}

/// Rung 2 — the format expresses a claim SET, on EVERY volume, durably.
fn rung_2_engaged_claim_set(req: &AdmissionRequest) -> Result<()> {
    if req.volumes.is_empty() {
        return Err(refuse(2, "the metadata set has no volumes".to_string()));
    }
    for vol in &req.volumes {
        let missing = REQUIRED_INCOMPAT & !vol.features_incompat;
        if missing != 0 {
            let bit = missing.trailing_zeros();
            return Err(refuse(
                2,
                format!(
                    "metadata volume {} does not carry incompat bit {bit} (missing mask {:#x} of \
                     the required {:#x}), so the format cannot express a second writer safely. \
                     Bit 14 (KV_CLAIM_SET) is the one this posture consumes directly: a \
                     half-engaged set is not a claim set. Nothing stamps these bits today \
                     (ruling D9) — the Phase-8 reformat window stamps them offline, and every \
                     volume of the set must carry all of them",
                    vol.path.display(),
                    missing,
                    REQUIRED_INCOMPAT
                ),
            ));
        }
        match &vol.claim_set {
            Some(set) if set.durable => {}
            Some(_) => {
                return Err(refuse(
                    2,
                    format!(
                        "metadata volume {} answered its claim set as the PROJECTION of a \
                         singular `writer_claim`, not as the durable `claim_set` record. A \
                         projection expresses EXCLUSION — one holder — and cannot represent a \
                         second member, so admitting off it would be admitting off nothing. \
                         Stamp bit 14 and let the authority write the record",
                        vol.path.display()
                    ),
                ));
            }
            None => {
                return Err(refuse(
                    2,
                    format!(
                        "metadata volume {} carries no claim set and no writer claim at all — \
                         there is no authority to be a co-writer OF (mount the authority first)",
                        vol.path.display()
                    ),
                ));
            }
        }
        if vol.claim.is_none() {
            return Err(refuse(
                2,
                format!(
                    "metadata volume {} carries no `writer_claim`: nothing holds D0 on it, so \
                     this set has no authority. Mount the authority first — a co-writer is \
                     subordinate to a claim holder by construction",
                    vol.path.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Rung 3 — the durable set NAMES this node, as a writer.
fn rung_3_durable_enrollment(req: &AdmissionRequest) -> Result<()> {
    for vol in &req.volumes {
        let set = vol
            .claim_set
            .as_ref()
            .expect("rung 2 established a durable claim set");
        let named = set.members.iter().find(|m| m.identity.id == req.node_id);
        match named {
            Some(m) if m.identity.role == MemberRole::Writer => {}
            Some(_) => {
                return Err(refuse(
                    3,
                    format!(
                        "the claim set on {} names this node '{}' as a READER member. A reader \
                         holds no custody and writes nothing; a co-writer needs a `writer` \
                         entry. The authority enrolls the role — re-enroll through \
                         {MW_MEMBERS_ENV}",
                        vol.path.display(),
                        req.node_id
                    ),
                ));
            }
            None => {
                return Err(refuse(
                    3,
                    format!(
                        "the durable claim set on {} does not name this node '{}'. Admission is \
                         by durable ENROLLMENT, never by a claim a joining node makes about \
                         itself: the `claim_set` record is a metadata commit, and a co-writer \
                         holds no metadata authority, so it CANNOT enroll itself. Add '{}' to \
                         the AUTHORITY's {MW_MEMBERS_ENV} roster and re-arm it (one durable \
                         entry per rostered node, §6.2 item 7), then remount this node",
                        vol.path.display(),
                        req.node_id,
                        req.node_id
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Rung 4 — a LIVE authority (custody source and, above all, evictor).
fn rung_4_live_authority(req: &AdmissionRequest) -> Result<&AuthorityLeaseEvidence> {
    let Some(auth) = req.authority.as_ref() else {
        return Err(refuse(
            4,
            "no membership plane. A co-writer that cannot be SEEN cannot be EVICTED, and S6's \
             eviction is what mints the dead epoch whose offsets enter the do-not-reallocate \
             quarantine — without it a dead co-writer's destinations would be handed to a new \
             owner. The AUTHORITY must arm SQUEEZEFS_MEMBERSHIP_BIND (auto, or an addr:port) \
             and publish its rendezvous record"
                .to_string(),
        ));
    };
    if !auth.live {
        return Err(refuse(
            4,
            format!(
                "the membership authority '{}' at {} did not grant this mount a lease, so its \
                 liveness is unproven. A rendezvous record only says an owner once armed; a \
                 LIVE lease is what proves there is a custody source to acquire from and an \
                 evictor to be swept by. Refusing rather than proceeding hopefully",
                auth.owner_id, auth.endpoint
            ),
        ));
    }
    let claim_term = req
        .volumes
        .iter()
        .filter_map(|v| v.claim.as_ref().map(|c| c.term))
        .max()
        .unwrap_or(0);
    if auth.term < claim_term {
        return Err(refuse(
            4,
            format!(
                "the membership authority '{}' armed in term {} but this set's `writer_claim` \
                 names term {claim_term} — the plane we joined belongs to an OLDER era than the \
                 D0 holder we can see, so it is not the authority of this set (a failover \
                 happened, or two planes are running). Re-arm the current authority's \
                 membership plane",
                auth.owner_id, auth.term
            ),
        ));
    }
    if !req
        .volumes
        .iter()
        .filter_map(|v| v.claim_set.as_ref())
        .all(|set| set.members.iter().any(|m| m.identity.id == auth.owner_id))
    {
        return Err(refuse(
            4,
            format!(
                "the durable claim set does not name the membership authority '{}' we joined as \
                 a member of this set. The plane that admits co-writers and the set that \
                 enrolls them must be the same authority, or an unrelated plane could admit a \
                 node into a set it has no authority over",
                auth.owner_id
            ),
        ));
    }
    Ok(auth)
}

/// Rung 5 — the DEVICE names this node a registrant of the standing hold.
fn rung_5_device_registrant(req: &AdmissionRequest) -> Result<&RegistrantEvidence> {
    let Some(ev) = req.registrant.as_ref() else {
        return Err(refuse(
            5,
            "no NVMe reservation evidence for the data namespaces. §6.7 requires ENFORCEMENT \
             for multi-writer (\"refused on non-PR\"), and that governs the admission decision, \
             not only the data plane: a co-writer whose DMA the device cannot reject is a \
             co-writer nothing can fence, and its death could never produce a drain proof"
                .to_string(),
        ));
    };
    if !ev.pr_capable {
        return Err(refuse(
            5,
            "a data namespace advertises no NVMe reservation support (RESCAP=0) — the shape of \
             every loop-device substrate, including tests/dev_substrate.sh's default. A fenced \
             co-writer could then only be DETECTED, never rejected. Use a PR-capable namespace \
             (SQZ_DEVSUB_TRANSPORT=tcp, or real hardware)"
                .to_string(),
        ));
    }
    if !ev.reservation_held {
        return Err(refuse(
            5,
            "no reservation is held on the data namespaces: nobody is fencing this data plane, \
             so a co-writer would be writing beside hosts the device does not authenticate. The \
             AUTHORITY takes the WERO hold at its multi-writer arm — arm it first"
                .to_string(),
        ));
    }
    if !ev.wero {
        return Err(refuse(
            5,
            "the held reservation is not Write Exclusive – Registrants Only (rtype 2). Under any \
             other type this node's registration grants no write access, so admitting would \
             produce a mount whose every DMA the device rejects — fail-closed, but a lie about \
             the posture"
                .to_string(),
        ));
    }
    if !ev.registered || ev.key == 0 {
        return Err(refuse(
            5,
            "this node's key is not among the standing WERO hold's registrants, so the device \
             would reject its writes AND the authority's preempt could never name it — no \
             registrant key means no drain proof, which means a dead co-writer's offsets could \
             never be released from quarantine"
                .to_string(),
        ));
    }
    Ok(ev)
}

/// **Decide.** Runs the five rungs in order and returns the admission, or
/// the first rung's refusal naming what is missing and its remedy.
///
/// Pure: it reads no environment, opens nothing, and mutates nothing but
/// the refusal counter. [`gather_admission`] is what turns a mount into an
/// [`AdmissionRequest`].
pub fn classify_admission(req: &AdmissionRequest) -> Result<CoWriterAdmission> {
    let endpoint = rung_1_declaration(req)?;
    rung_2_engaged_claim_set(req)?;
    rung_3_durable_enrollment(req)?;
    let auth = rung_4_live_authority(req)?;
    let dev = rung_5_device_registrant(req)?;

    let claim = req
        .volumes
        .iter()
        .filter_map(|v| v.claim.as_ref())
        .max_by_key(|c| c.term)
        .expect("rung 2 established a claim on every volume");
    let admission = CoWriterAdmission {
        node_id: req.node_id.clone(),
        authority_claim_id: claim.id.clone(),
        authority_term: claim.term,
        membership_owner_id: auth.owner_id.clone(),
        membership_endpoint: auth.endpoint.clone(),
        custody_endpoint: endpoint.to_string(),
        pr_key: dev.key,
        volumes: req.volumes.iter().map(|v| v.path.clone()).collect(),
    };
    METRICS.cowriter_admissions.fetch_add(1, Ordering::Relaxed);
    log::warn!(
        "CO-WRITER ADMITTED (DLM S9) over {} volume(s): node '{}', authority claim '{}' in era \
         {}, membership owner '{}' at {} (member epoch {}), custody at {}, device registrant key \
         {:#x} on {} namespace(s). This mount holds NO metadata authority — every metadata \
         mutation ships — and writes data only under custody the authority grants",
        admission.volumes.len(),
        admission.node_id,
        admission.authority_claim_id,
        admission.authority_term,
        admission.membership_owner_id,
        admission.membership_endpoint,
        auth.member_epoch,
        admission.custody_endpoint,
        admission.pr_key,
        dev.namespaces,
    );
    Ok(admission)
}

// ---------------------------------------------------------------------------
// Enrollment — the AUTHORITY's act
// ---------------------------------------------------------------------------

/// Enroll `node_ids` as durable `Writer` members of every volume's claim
/// set (§6.2 item 7) — **the authority's commit**, called from its
/// multi-writer arm over the operator-declared [`MW_MEMBERS_ENV`] roster.
///
/// Returns how many entries were committed. A volume without incompat bit
/// 14 commits nothing and says so through
/// [`crate::membership::upsert_writer_member`]'s own contract (single-writer
/// byte-identity is a law, not an intention), which is why a co-writer's
/// rung 2 refuses such a set rather than trusting an entry that was never
/// written.
///
/// The entries carry `pid = 0`, `boot = ""` and no endpoint on purpose:
/// enrollment names a NODE, not a process, and must exist before the mount
/// that would fill those in. The co-writer's live identity (its endpoint
/// and its device registrant key) reaches the authority through the custody
/// join, not through this record.
pub async fn enroll_members(
    volumes: &[Arc<KvMetaBackend>],
    node_ids: &[String],
    term: u64,
) -> Result<usize> {
    let mut committed = 0usize;
    for id in node_ids {
        let identity = crate::membership::MemberIdentity {
            id: id.clone(),
            role: MemberRole::Writer,
            pid: 0,
            boot: String::new(),
            endpoint: None,
            pr_key: 0,
        };
        for be in volumes {
            if crate::membership::upsert_writer_member(be, &identity, term).await? {
                committed += 1;
            }
        }
    }
    if committed > 0 {
        log::warn!(
            "claim set: {committed} durable co-writer enrollment(s) committed for {:?} in term \
             {term} — each named node may now be ADMITTED as a co-writer of this set (§6.2 item \
             7; the authority is the only writer of this record)",
            node_ids
        );
    }
    Ok(committed)
}

// ---------------------------------------------------------------------------
// The mount-path preflight
// ---------------------------------------------------------------------------

/// What a successful preflight leaves the mount holding: the decision, the
/// live membership lease that proved rung 4, the device registration that
/// proved rung 5, and the cluster secret both wires authenticate against.
pub struct CoWriterPreflight {
    /// The gate's outcome — `open_co_writer` takes it.
    pub admission: CoWriterAdmission,
    /// The live member session (a co-writer joins as a WRITER member, so
    /// its self-fence poisons process data custody rather than purging a
    /// cache).
    pub membership: Option<crate::membership::MembershipArm>,
    /// The device registration under the authority's standing WERO hold.
    pub wero: crate::data_custody::WeroRegistrantJoin,
    /// The `job:enroll` storage-trust secret (ruling D2's root of trust).
    pub secret: Vec<u8>,
}

impl std::fmt::Debug for CoWriterPreflight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoWriterPreflight")
            .field("admission", &self.admission)
            .field("wero", &self.wero)
            .finish_non_exhaustive()
    }
}

/// **Gather the ladder's evidence and decide** — the mount path's one call
/// before it opens the metadata set as a co-writer.
///
/// Evidence is gathered in ladder order, so nothing that mutates state runs
/// before every declarative rung has passed: rung 1 is env only; rungs 2/3
/// read through PROBE opens (never blocked by the authority's flock, never
/// writing); rung 4 joins the membership plane (a RAM lease — no journal
/// transaction); and only then does rung 5 register a key on the device,
/// which a later refusal undoes by dropping the join.
pub async fn gather_admission(
    meta_lvs: &[String],
    data_paths: &[PathBuf],
    on_purge: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<CoWriterPreflight> {
    // Rung 1 — env only.
    let mut req = AdmissionRequest {
        multi_writer: crate::data_custody::multi_writer_requested(),
        role_co_writer: requested_role() == MwRole::CoWriter,
        read_only: crate::fuse_client::read_only_mount(),
        node_id: node_member_id()?,
        custody_endpoint: declared_authority(),
        volumes: Vec::new(),
        authority: None,
        registrant: None,
    };
    rung_1_declaration(&req)?;

    // Rungs 2/3 — probe opens (read-only, lock-free, never blocked).
    let probes = crate::meta_backend::open_probe_routed_meta_set(meta_lvs).await?;
    for be in &probes.volumes {
        req.volumes.push(VolumeAdmissionEvidence {
            path: be.device_path().to_path_buf(),
            features_incompat: be.superblock().features_incompat,
            claim: be.read_writer_claim().await,
            claim_set: ClaimSet::load(be).await,
        });
    }
    rung_2_engaged_claim_set(&req)?;
    rung_3_durable_enrollment(&req)?;

    // Rung 4 — the membership plane: the rendezvous record, then a real
    // JOIN as a writer member (the liveness proof).
    let first = probes
        .volumes
        .first()
        .expect("rung 2 established a non-empty set");
    let secret = crate::membership::cluster_secret(first)
        .await
        .ok_or_else(|| {
            refuse(
                4,
                "the volume set carries no `job:enroll` record, which is this cluster's root of \
             trust (possession of volume access IS cluster membership — ruling D2). The \
             AUTHORITY writes it when its cluster listener starts (SQUEEZEFS_JOB_WIRE_BIND)"
                    .to_string(),
            )
        })?;
    let rendezvous = {
        let mut best: Option<crate::membership::OwnerRecord> = None;
        for be in &probes.volumes {
            if let Some(rec) = crate::membership::read_owner_record(be).await {
                if best.as_ref().is_none_or(|b| rec.term > b.term) {
                    best = Some(rec);
                }
            }
        }
        best
    };
    drop(probes);

    // The device half FIRST only in one respect: the join must present our
    // registrant key so the authority can preempt us (i.e. so a drain proof
    // about us can exist). Registration is the mutation, so it happens
    // after rungs 1–3 and its failure is rung 5's refusal.
    let paths = data_paths.to_vec();
    let wero =
        tokio::task::spawn_blocking(move || crate::data_custody::join_wero_as_registrant(&paths))
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!("co-writer WERO join task failed: {e}"))
            })??;
    req.registrant = Some(wero.evidence());

    if let Some(rec) = rendezvous {
        let joined = crate::membership::join_as_writer_member(
            &rec,
            secret.clone(),
            &req.node_id,
            wero.evidence().key,
            on_purge,
        )
        .await?;
        req.authority = Some(AuthorityLeaseEvidence {
            owner_id: rec.id.clone(),
            endpoint: rec.endpoint.clone(),
            term: rec.term,
            live: joined.is_some(),
            member_epoch: crate::membership::installed_member_epoch(),
        });
        // A refusal from here on drops `wero`, which unregisters — the
        // device is left exactly as we found it.
        let admission = match classify_admission(&req) {
            Ok(a) => a,
            Err(e) => {
                if let Some(arm) = joined {
                    arm.disarm().await;
                }
                return Err(e);
            }
        };
        return Ok(CoWriterPreflight {
            admission,
            membership: joined,
            wero,
            secret,
        });
    }
    // No rendezvous record at all: rung 4's own refusal, with the device
    // registration undone by the drop.
    rung_4_live_authority(&req)?;
    unreachable!("rung 4 refuses when no membership evidence exists")
}

// ---------------------------------------------------------------------------
// The arm
// ---------------------------------------------------------------------------

/// What a co-writer mount armed, and the teardown that undoes it.
pub struct CoWriterArm {
    admission: CoWriterAdmission,
    client: Arc<crate::data_grant::WriteCustodyClient>,
    membership: Option<crate::membership::MembershipArm>,
    wero: Option<crate::data_custody::WeroRegistrantJoin>,
    stop: Arc<AtomicBool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for CoWriterArm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoWriterArm")
            .field("admission", &self.admission)
            .field("tasks", &self.tasks.len())
            .finish_non_exhaustive()
    }
}

impl CoWriterArm {
    /// The gate outcome this arm stands on.
    pub fn admission(&self) -> &CoWriterAdmission {
        &self.admission
    }

    /// The custody client every write of this mount acquires through.
    pub fn client(&self) -> &Arc<crate::data_grant::WriteCustodyClient> {
        &self.client
    }

    /// Stop being a co-writer: end the renewal cadence, uninstall both
    /// client halves and the ownership plane, leave the membership plane,
    /// and unregister this node's key from the standing WERO hold (zero
    /// device residue — the authority's reservation is untouched).
    pub async fn disarm(mut self) {
        self.stop.store(true, Ordering::Release);
        for task in self.tasks.drain(..) {
            task.abort();
        }
        crate::data_grant::uninstall_custody_client();
        crate::meta_ship::publish::uninstall_client();
        crate::meta_ship::disarm_ownership();
        if let Some(arm) = self.membership.take() {
            arm.disarm().await;
        }
        if let Some(wero) = self.wero.take() {
            let _ = tokio::task::spawn_blocking(move || drop(wero)).await;
        }
        log::warn!(
            "CO-WRITER DISARMED: no custody is held, nothing ships, and this node is no longer \
             a registrant of the data namespaces' WERO hold"
        );
    }
}

/// **Arm the co-writer's client halves** over an admitted mount: the
/// ownership plane (the authority owns EVERY volume of this set, so every
/// metadata verb ships), the custody client (S4's foreign-home seam), the
/// publish client (S9's vocabulary), and the renewal cadence whose failure
/// self-fences this node BEFORE the authority may re-grant.
///
/// Also latches the data plane's accounting closed: the reclaim queue is
/// ceased, exactly as a reader's is, because terminal frees and device
/// deallocation are the authority's accounting and not ours.
pub async fn arm(
    meta: &Arc<RoutedMetaBackend>,
    router: &crate::routing::DataRouter,
    preflight: CoWriterPreflight,
) -> Result<CoWriterArm> {
    let CoWriterPreflight {
        admission,
        membership,
        wero,
        secret,
    } = preflight;

    // The ownership plane: every volume of this set is owned by the
    // authority, so `meta_ship`'s routing ships every verb and
    // `constrain_mint_volume` can never mint locally.
    let foreign: Vec<(usize, crate::meta_ship::PeerOwner)> = (0..meta.volumes.len())
        .map(|i| {
            (
                i,
                crate::meta_ship::PeerOwner::new(
                    admission.authority_claim_id.clone(),
                    admission.custody_endpoint.clone(),
                ),
            )
        })
        .collect();
    let map = crate::meta_ship::OwnerMap::for_volumes(meta, foreign)?;
    crate::meta_ship::arm_ownership(map);
    crate::meta_ship::publish::install_client(crate::meta_ship::publish::PublishClient::new(
        &admission.node_id,
        secret.clone(),
    ));

    // The custody client: the acquire travels, and what comes back is
    // custody the authority ISSUED (adopted with the owner's own token).
    let client = crate::data_grant::WriteCustodyClient::connect_with_clock(
        &admission.custody_endpoint,
        &secret,
        &admission.node_id,
        crate::membership::LeaseClock::monotonic(),
        admission.pr_key,
    )
    .await
    .map_err(|e| {
        crate::meta_ship::publish::uninstall_client();
        crate::meta_ship::disarm_ownership();
        SqueezefsError::InvalidOperation(format!(
            "co-writer arm failed: the custody authority at {} refused or could not be reached \
             ({e}). The admission ladder passed, so this is a transport or authority-state \
             problem, not a posture one",
            admission.custody_endpoint
        ))
    })?;
    crate::data_grant::install_custody_client(Arc::clone(&client));

    // The accounting latch: a co-writer frees nothing and deallocates
    // nothing (§6.3's reclaim/discard hazard applies verbatim — the
    // offsets belong to the authority's ledger).
    router.backend_router.reclaim_cease();

    let stop = Arc::new(AtomicBool::new(false));
    let renew = spawn_custody_renewal(Arc::clone(&client), Arc::clone(&stop));
    log::warn!(
        "CO-WRITER ARMED (DLM S9): metadata verbs ship to '{}' at {}, write custody is acquired \
         there, and this mount's own allocator/reclaim accounting is closed. Data DMA is \
         authorized locally under the custody epoch — only custody travels, never data",
        admission.authority_claim_id,
        admission.custody_endpoint,
    );
    Ok(CoWriterArm {
        admission,
        client,
        membership,
        wero: Some(wero),
        stop,
        tasks: vec![renew],
    })
}

/// The co-writer's renewal cadence: renew before the authority's TTL, and
/// **self-fence at our own (strictly earlier) deadline** if renewal stops
/// completing — poisoning process data custody so nothing can land after
/// the authority may have granted those bytes elsewhere (§6.7's stricter
/// client clock, S6's law reused verbatim).
fn spawn_custody_renewal(
    client: Arc<crate::data_grant::WriteCustodyClient>,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(crate::detached::contain(
        "cowriter_custody_renewal",
        async move {
            loop {
                let now = crate::membership::LeaseClock::monotonic().now_ms();
                let due = client.renew_at_ms().saturating_sub(now).max(1);
                tokio::time::sleep(Duration::from_millis(due)).await;
                if stop.load(Ordering::Acquire) {
                    return;
                }
                if let Err(e) = client.renew_all().await {
                    if client.self_fence_due() {
                        client.self_fence(&format!("custody renewal failed: {e}"));
                        return;
                    }
                    log::warn!(
                        "co-writer custody renewal failed ({e}) — retrying before my own deadline \
                     (T_self, strictly earlier than the authority's TTL)"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        },
    ))
}

/// The `cowriter` stats-inode object. Every field is inert (`off` / 0) on
/// every shipped mount, because the posture is opt-in twice over.
pub fn stats_json() -> serde_json::Value {
    let posture = crate::fuse_client::mount_posture();
    serde_json::json!({
        "mount_posture": posture.as_str(),
        "mw_role": requested_role().as_str(),
        "admissions": METRICS.cowriter_admissions.load(Ordering::Relaxed),
        "admission_refusals": METRICS.cowriter_admission_refusals.load(Ordering::Relaxed),
        "accounting_refusals": METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed),
        "local_commit_refusals": METRICS.cowriter_local_commit_refusals.load(Ordering::Relaxed),
        "custody_endpoint": declared_authority().unwrap_or_else(|| "none".to_string()),
    })
}
