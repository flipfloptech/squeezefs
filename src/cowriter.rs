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
//! | fresh block ALLOCATION | **yes, from the lane the authority granted** (DLM S9's allocation-lane grant): a residue class no peer mints in, whose durable reservation the authority commits ahead of every hand-out | [`crate::alloc_lane_grant`] + `BlockAllocator`'s `alloc_plane_gate` |
//! | terminal FREE of a displaced block | **yes, by SHIPPING** (DLM S9's co-writer free path): the durable delete already rode the layout publish, and the accounting ladder travels as a verb the AUTHORITY executes — `begin_free` → purge → reclaim → `finish_free`, grace ring and quarantine composing inside | [`ship_displaced_frees`] (client) + [`execute_shipped_frees`] (owner), `(lease_epoch, request_id)` dedup window |
//! | ownership accounting (specific claim / W1 incarnation retire / device reclaim / the recovery walk) | **none.** The durable truth of "who owns this offset" is metadata (`TREE_BLOCK_REFS`), and metadata authority is the authority's | `BlockAllocator`'s gate, whose co-writer class names what it may not do |
//!
//! That split is the honest reading of the blocker: *"a co-writer holding a
//! valid S9 grant has authority for the data plane while having none for
//! the metadata plane"* — and block ownership *accounting* is metadata, not
//! data. Placing a block in a lane nobody else can mint in is a data act;
//! deciding that an offset is now unowned is not.
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
//! | **5** | a PR-capable substrate whose **standing WERO (rtype 3) hold names this node as a registrant** | §6.7's *"multi-writer refuses to arm on non-PR substrates"* applies to the ADMISSION decision too. A co-writer whose DMA the device cannot reject is a co-writer nothing can fence, and its death could never produce a drain proof (the proof IS the preempt of this registrant key) |
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
//! # What a co-writer does about allocation
//!
//! It **allocates from the lane its authority granted** — the seam this
//! posture originally left open, closed by [`crate::alloc_lane_grant`]:
//!
//! * the lane `(w, W)` arrives on the **custody lease**, derived by the
//!   authority from the durable claim set. A co-writer never chooses, derives
//!   or configures it, because two co-writers choosing lanes is the collision
//!   the partition exists to prevent;
//! * fresh allocation is then a **residue class** (`b % W == w`) no other
//!   writer mints in, so no arbitration and no message is needed per block;
//! * the durable reservation that covers each grain is a metadata commit, so
//!   it **ships**: the AUTHORITY writes the `alloc_lane:` record, validates
//!   the lane against its own assignment, holds the monotonicity, and the
//!   offset is handed out only after that commit lands;
//! * the lane's **floor** is OPENED by the authority, because a co-writer
//!   runs no ownership-recovery walk and would otherwise mint from block 0 of
//!   a device whose low blocks are live.
//!
//! What stays refused is everything whose durable home is metadata this mount
//! cannot commit **and whose act does not ship**: `allocate_specific_block`
//! (lane-BLIND by design — a clone/recovery path naming an index it already
//! owns durably), the **W1 incarnation retire** (a lifetime retire is durable
//! ownership state, §6.2 item 6, and the §5.1 clone/patch fence is a two-word
//! process-local protocol no wire can compose — a co-writer's small overwrite
//! rides CoW-rewrite + shipped free instead, and the RTT a shipped retire
//! would put inside the one-DMA-zero-metadata path is self-defeating), the
//! **ownership recovery walk**, direct **device reclaim** (its queue is
//! ceased here — the authority's reclaimer runs the shipped frees'), and any
//! ALLOCATOR-level free reached without the router. Those are what
//! `cowriter.accounting_refusals` keeps counting; allocation and the
//! router-level terminal free no longer appear there — the free SHIPS
//! ([`ship_displaced_frees`], the module block at the end of this file) —
//! and neither do the two posture-shaped hot paths the 2026-08-19 fleet
//! capture convicted: the W1 probe declines upstream as the counted
//! `patch_ineligible_posture` decision, and a minted-but-never-published
//! offset's error cleanup exits through the quiet leak-safe abandon arm
//! (`BlockAllocator::abandon_unpublished_offset`, counted
//! `cowriter.unpublished_abandons` — the `free_ship_failures` recovery
//! pattern: the next derivation returns the offset to the free supply).

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
    /// Per-volume claim admission (`docs/design-per-volume-claim-admission.md`
    /// §5.1.3, ruling **D20**): this mount appends to the volume hosting
    /// slot 0 — so it is the SET AUTHORITY (lane assignment, the custody
    /// endpoint, the one grace ring, maintenance coordination, ino 1) —
    /// and ships its verbs for every volume a peer owns.
    SetAuthority,
    /// Per-volume claim admission: this mount appends to a SUBSET of the
    /// set and ships the rest to their owners. Its data plane is the
    /// co-writer class (custody + a granted lane; terminal frees ship).
    PartialAuthority,
}

impl MwRole {
    /// The operator-facing word (the knob's value and the stats gauge).
    pub fn as_str(self) -> &'static str {
        match self {
            MwRole::Authority => "authority",
            MwRole::CoWriter => "co-writer",
            MwRole::SetAuthority => "set-authority",
            MwRole::PartialAuthority => "partial-authority",
        }
    }
}

/// The requested role. Malformed values never fall back to a default —
/// `env_knobs`' startup gate refuses the process on an unregistered word,
/// and this reader is the enum's one consumer.
pub fn requested_role() -> MwRole {
    match crate::env_knobs::enum_knob(
        MW_ROLE_ENV,
        &[
            "authority",
            "co-writer",
            "set-authority",
            "partial-authority",
        ],
        "authority",
    ) {
        "co-writer" => MwRole::CoWriter,
        "set-authority" => MwRole::SetAuthority,
        "partial-authority" => MwRole::PartialAuthority,
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
        // Per-volume claim admission: neither per-volume posture is a
        // CO-WRITER declaration, so this reader answers `false` and says
        // nothing else about them. Their door is
        // `partial_authority::requested()`, which the mount path reads
        // next, and every refusal about them belongs to the seven-rung
        // ladder — including the missing opt-in, which is rung 1's and
        // names it. Answering here instead would refuse the POSTURE where
        // the ladder refuses the missing HALF.
        (MwRole::SetAuthority | MwRole::PartialAuthority, _) => Ok(false),
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

/// This client's **durable enrollment identity** (KD-MW-2, design
/// design-full-multi-writer §5.1): the pair `node_{16 hex}.m{8 hex}` —
/// the writer-scope node token plus this mount's mount slot — or the bare
/// `node_{16 hex}` form when this process has no mount slot (offline
/// verbs, tests; a bare id is also what a SLOT-WILDCARD roster entry
/// names, see [`crate::membership::member_id_matches`]). See the module
/// docs on why the node half is a node identity and not a mount uuid;
/// the slot half is what keeps two co-located mounts' identities apart.
/// Refuses loud exactly as
/// [`crate::writer_scope::resolve_node_identity`] does — an unstable
/// identity would enroll one host and admit another.
pub fn node_member_id() -> Result<String> {
    let node = crate::writer_scope::resolve_node_identity()?.token;
    Ok(match crate::writer_scope::mount_slot() {
        0 => format!("node_{node:016x}"),
        slot => format!("node_{node:016x}.m{slot:08x}"),
    })
}

/// Is the authority CO-LOCATED with this mount (rung-9 finding #1)?
///
/// The test is the D0 `writer_claim`'s **boot id** against this kernel's:
/// the same boot IS the same host — the shape whose merged multipath head
/// makes PR mutations unsound (register/wire_host_id ride round-robined
/// associations) and whose PR arbitration domain is shared anyway
/// (docs/operations.md §Multi-writer co-writer mounts, the honest
/// residual). Node-token or identity-string comparisons would be weaker:
/// both are derivable/spoofable configuration, while the boot id is what
/// the same-host dead-pid proof already keys on (`WriterClaim.boot`).
/// Empty evidence (no claim on some volume) answers `false` — the
/// register path stays, and its own gates decide.
pub fn co_located_with_authority(req: &AdmissionRequest) -> bool {
    let Ok(our_boot) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") else {
        return false;
    };
    let our_boot = our_boot.trim();
    !req.volumes.is_empty()
        && req
            .volumes
            .iter()
            .all(|v| v.claim.as_ref().is_some_and(|c| c.boot == our_boot))
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
    /// The owner's durable claim-set identity from its rendezvous record
    /// (rung-9 finding #3 — empty on legacy records; see
    /// [`crate::membership::OwnerRecord::owner_claim_id`]).
    pub owner_claim_id: String,
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

/// The refusal shape both admission ladders speak: `{posture} admission
/// REFUSED at rung {rung}: {detail}`, logged at error level.
///
/// Shared with [`crate::partial_authority`] (its ladder EXTENDS this one,
/// §5.3) so the two cannot drift into two spellings of one refusal; the
/// per-posture counter stays with the posture, because the co-writer
/// gauge must keep counting co-writer refusals only.
pub(crate) fn admission_refusal(posture: &str, rung: u8, detail: String) -> SqueezefsError {
    let msg = format!("{posture} admission REFUSED at rung {rung}: {detail}");
    log::error!("{msg}");
    SqueezefsError::InvalidOperation(msg)
}

fn refuse(rung: u8, detail: String) -> SqueezefsError {
    METRICS
        .cowriter_admission_refusals
        .fetch_add(1, Ordering::Relaxed);
    admission_refusal("co-writer", rung, detail)
}

/// Rung 2's format check, shared by both ladders: the capability bits this
/// set must carry, answered as the refusal DETAIL naming the volume, the
/// first missing bit and the required mask.
pub(crate) fn required_incompat_detail(path: &Path, features_incompat: u64) -> Option<String> {
    let missing = REQUIRED_INCOMPAT & !features_incompat;
    if missing == 0 {
        return None;
    }
    let bit = missing.trailing_zeros();
    Some(format!(
        "metadata volume {} does not carry incompat bit {bit} (missing mask {:#x} of the \
         required {:#x}), so the format cannot express a second writer safely. Bit 14 \
         (KV_CLAIM_SET) is the one this posture consumes directly: a half-engaged set is not a \
         claim set. Nothing stamps these bits today (ruling D9) — the Phase-8 reformat window \
         stamps them offline, and every volume of the set must carry all of them",
        path.display(),
        missing,
        REQUIRED_INCOMPAT
    ))
}

/// What a durable claim set says about one node — rung 3's lookup, shared
/// by both ladders (KD-MW-2: entries match exactly, or as a bare-node SLOT
/// WILDCARD naming every mount slot of that node).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberStanding {
    /// Named as a `Writer` member.
    Writer,
    /// Named, but as a `Reader` — it holds no custody and writes nothing.
    Reader,
    /// Not named at all.
    Absent,
}

pub(crate) fn member_standing(set: &ClaimSet, id: &str) -> MemberStanding {
    match set
        .members
        .iter()
        .find(|m| crate::membership::member_id_matches(&m.identity.id, id))
    {
        Some(m) if m.identity.role == MemberRole::Writer => MemberStanding::Writer,
        Some(_) => MemberStanding::Reader,
        None => MemberStanding::Absent,
    }
}

/// Rung 5, shared verbatim by both ladders (§5.3: *"unchanged in
/// substance"*): the DEVICE must name this node a registrant of the
/// standing WERO hold, or the mount is one nothing can fence. Answers the
/// refusal DETAIL; the caller wraps it in its own posture's refusal.
pub(crate) fn registrant_detail(
    ev: Option<&RegistrantEvidence>,
) -> std::result::Result<&RegistrantEvidence, String> {
    let Some(ev) = ev else {
        return Err(
            "no NVMe reservation evidence for the data namespaces. §6.7 requires ENFORCEMENT \
             for multi-writer (\"refused on non-PR\"), and that governs the admission decision, \
             not only the data plane: a co-writer whose DMA the device cannot reject is a \
             co-writer nothing can fence, and its death could never produce a drain proof"
                .to_string(),
        );
    };
    if !ev.pr_capable {
        return Err(
            "a data namespace advertises no NVMe reservation support (RESCAP=0) — the shape of \
             every loop-device substrate, including tests/dev_substrate.sh's default. A fenced \
             co-writer could then only be DETECTED, never rejected. Use a PR-capable namespace \
             (SQZ_DEVSUB_TRANSPORT=tcp, or real hardware)"
                .to_string(),
        );
    }
    if !ev.reservation_held {
        return Err(
            "no reservation is held on the data namespaces: nobody is fencing this data plane, \
             so a co-writer would be writing beside hosts the device does not authenticate. The \
             AUTHORITY takes the WERO hold at its multi-writer arm — arm it first"
                .to_string(),
        );
    }
    if !ev.wero {
        return Err(
            "the held reservation is not Write Exclusive – Registrants Only (rtype 3). Under any \
             other type this node's registration grants no write access, so admitting would \
             produce a mount whose every DMA the device rejects — fail-closed, but a lie about \
             the posture"
                .to_string(),
        );
    }
    if !ev.registered || ev.key == 0 {
        return Err(
            "this node's key is not among the standing WERO hold's registrants, so the device \
             would reject its writes AND the authority's preempt could never name it — no \
             registrant key means no drain proof, which means a dead co-writer's offsets could \
             never be released from quarantine"
                .to_string(),
        );
    }
    Ok(ev)
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
        if let Some(detail) = required_incompat_detail(&vol.path, vol.features_incompat) {
            return Err(refuse(2, detail));
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
        // KD-MW-2: entries match exactly, or as a bare-node SLOT WILDCARD
        // (`node_{16 hex}` names every mount slot of that node — the
        // single-mount-host convenience the §11 grammar keeps).
        match member_standing(set, &req.node_id) {
            MemberStanding::Writer => {}
            MemberStanding::Reader => {
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
            MemberStanding::Absent => {
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
    // Rung-9 finding #3: the claim set is keyed on DURABLE NODE ids
    // (rung-8 finding #3's one-identity law) while the rendezvous `id` is
    // the INCARNATION uuid — comparing the uuid against the set refused
    // every healthy fleet's first co-writer. The rendezvous record now
    // carries the owner's claim identity; legacy records (empty) keep the
    // uuid match, so a pre-split plane still links the pre-split way.
    if !req
        .volumes
        .iter()
        .filter_map(|v| v.claim_set.as_ref())
        .all(|set| {
            set.members.iter().any(|m| {
                m.identity.id == auth.owner_id
                    || (!auth.owner_claim_id.is_empty() && m.identity.id == auth.owner_claim_id)
            })
        })
    {
        return Err(refuse(
            4,
            format!(
                "the durable claim set does not name the membership authority '{}' (claim \
                 identity '{}') we joined as a member of this set. The plane that admits \
                 co-writers and the set that enrolls them must be the same authority, or an \
                 unrelated plane could admit a node into a set it has no authority over",
                auth.owner_id, auth.owner_claim_id
            ),
        ));
    }
    Ok(auth)
}

/// Rung 5 — the DEVICE names this node a registrant of the standing hold.
/// The checks and their texts are [`registrant_detail`], shared verbatim
/// with the per-volume ladder (§5.3: rung 5 is *"unchanged in substance"*).
fn rung_5_device_registrant(req: &AdmissionRequest) -> Result<&RegistrantEvidence> {
    registrant_detail(req.registrant.as_ref()).map_err(|detail| refuse(5, detail))
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
    let colocated = co_located_with_authority(&req);
    let enrolled_writer_keys: Vec<u64> = req
        .volumes
        .first()
        .and_then(|v| v.claim_set.as_ref())
        .map(|set| {
            set.members
                .iter()
                .filter(|m| m.identity.role == MemberRole::Writer && m.identity.pr_key != 0)
                .map(|m| m.identity.pr_key)
                .collect()
        })
        .unwrap_or_default();

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
    //
    // Rung-9 finding #1: the CO-LOCATED shape (this claim's `boot` is OUR
    // boot — same kernel, same box, same merged multipath head) must never
    // run PR MUTATIONS: head ioctls round-robin across associations, so the
    // register ladder's own-stale proof can name the LIVE authority's
    // holder key and release the whole set's fence (device-proven). It
    // ADOPTS the standing hold instead — the ops.md honest residual — with
    // the holder key cross-checked against the claim set's enrolled writer
    // keys. Remote co-writers (their own head, sound ladder) keep the
    // register path.
    let paths = data_paths.to_vec();
    let wero = if colocated {
        squeezefs_ipc::sqz_blocking::run_blocking(move || {
            crate::data_custody::adopt_wero_colocated(&paths, &enrolled_writer_keys)
        })
        .await?
    } else {
        squeezefs_ipc::sqz_blocking::run_blocking(move || {
            crate::data_custody::join_wero_as_registrant(&paths)
        })
        .await?
    };
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
            owner_claim_id: rec.owner_claim_id.clone(),
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
}

impl std::fmt::Debug for CoWriterArm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoWriterArm")
            .field("admission", &self.admission)
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
        // Stop latch (the D0 heartbeat precedent — sqz-meta tasks are
        // never aborted mid-poll): the renewal loop checks `stop` after
        // every sleep and exits before touching the client again.
        self.stop.store(true, Ordering::Release);
        crate::data_grant::uninstall_custody_client();
        crate::meta_ship::publish::uninstall_client();
        crate::meta_ship::uninstall_daemon_verb_router();
        crate::meta_ship::disarm_ownership();
        // Rung 17: the extent hooks die with the co-writer (retention
        // itself survives in RAM only as long as the process — MW-10's
        // acked-un-fsynced class owns a crash's residue).
        crate::extent_ship::uninstall_quiesce_hook();
        crate::extent_ship::uninstall_release_hook();
        crate::extent_ship::uninstall_spill_sink();
        if let Some(arm) = self.membership.take() {
            arm.disarm().await;
        }
        if let Some(wero) = self.wero.take() {
            squeezefs_ipc::sqz_blocking::run_blocking(move || drop(wero)).await;
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

    let stop = Arc::new(AtomicBool::new(false));
    let client = install_client_halves(
        meta,
        router,
        &ClientHalfParams {
            posture: "co-writer",
            node_id: &admission.node_id,
            authority_endpoint: &admission.custody_endpoint,
            pr_key: admission.pr_key,
            lane_remedy: "Enroll this node in SQUEEZEFS_MW_MEMBERS on the authority and re-arm it",
        },
        secret,
        &stop,
    )
    .await
    .inspect_err(|_| crate::meta_ship::disarm_ownership())?;

    let lane = client.lane_partition();
    log::warn!(
        "CO-WRITER ARMED (DLM S9): metadata verbs ship to '{}' at {}, write custody is acquired \
         there, fresh blocks are placed in allocation lane {} of {} (durable reservations \
         committed by the authority ahead of every hand-out), and this mount's own \
         free/reclaim accounting stays closed. Data DMA is authorized locally under the custody \
         epoch — only custody travels, never data",
        admission.authority_claim_id,
        admission.custody_endpoint,
        lane.writer_id(),
        lane.writers(),
    );
    Ok(CoWriterArm {
        admission,
        client,
        membership,
        wero: Some(wero),
        stop,
    })
}

/// What distinguishes one client-half arm from another — everything else
/// [`install_client_halves`] does is identical by law, not by coincidence.
pub(crate) struct ClientHalfParams<'a> {
    /// The posture word every log line and refusal names (`co-writer` /
    /// `partial authority`), so an operator reading one line knows which
    /// arm produced it.
    pub posture: &'a str,
    /// This mount's durable enrollment identity (KD-MW-2).
    pub node_id: &'a str,
    /// Where the SET AUTHORITY serves custody + the shipped publish path
    /// (D20: the owner of the slot-0 volume).
    pub authority_endpoint: &'a str,
    /// This node's device registrant key under the standing WERO hold.
    pub pr_key: u64,
    /// What an operator does about a SOLO lease — the roster that would
    /// have widened the partition differs by posture (`SQUEEZEFS_MW_MEMBERS`
    /// for a co-writer, the offline assignment verb for a partial
    /// authority), and naming the wrong one sends them to the wrong node.
    pub lane_remedy: &'a str,
}

/// **The client halves of the multi-writer plane**, installed identically
/// by both CLIENT postures: DLM S9's co-writer ([`arm`]) and per-volume
/// claim admission's PARTIAL AUTHORITY
/// ([`crate::multi_writer::arm_partial_authority`] — §5.7's *"the co-writer
/// client halves … composed with an owner half"*, which is a composition
/// instruction, not a licence to fork).
///
/// In order, and the order is load-bearing:
///
/// 1. the **publish client** — S9's vocabulary, and the transport the lane
///    raise below rides;
/// 2. the **daemon verb router** — rung 9's client half, consulted at the
///    `Metadata` trait impl itself so no call site can bypass it. A verb
///    whose participants all home locally still executes locally
///    ([`crate::meta_ship::daemon_verb_router`]'s third condition), which
///    is what makes one router correct for a mount that owns SOME volumes;
/// 3. the **custody client** — the acquire travels and what comes back is
///    custody the authority ISSUED;
/// 4. the **closed local accounting** — the reclaim queue is ceased, not
///    merely idle: terminal frees SHIP and the device reclaim they imply
///    runs on the authority's reclaimer;
/// 5. the **allocation lane** the lease carries, engaged with a floor
///    OPENED by the authority (neither posture walks the tree);
/// 6. the **renewal cadence**, which self-fences at this mount's own
///    (strictly earlier) deadline before the authority may re-grant.
///
/// **The ownership map must already be armed** — the lane OPEN ships
/// through it ([`crate::meta_ship::publish::raise_alloc_lane`] routes on
/// the owner of ino 1) — and it is the CALLER's, because the two postures
/// derive it differently: a co-writer's is all-foreign by construction, a
/// partial authority's is DERIVED per volume (KD-PV-3). A failure here
/// uninstalls exactly what this function installed; the map is the
/// caller's to disarm.
pub(crate) async fn install_client_halves(
    meta: &Arc<RoutedMetaBackend>,
    router: &crate::routing::DataRouter,
    params: &ClientHalfParams<'_>,
    secret: Vec<u8>,
    stop: &Arc<AtomicBool>,
) -> Result<Arc<crate::data_grant::WriteCustodyClient>> {
    let ClientHalfParams {
        posture,
        node_id,
        authority_endpoint,
        pr_key,
        lane_remedy,
    } = *params;

    crate::meta_ship::publish::install_client(crate::meta_ship::publish::PublishClient::new(
        node_id,
        secret.clone(),
    ));
    // Rung 9 — the S8 arm's client half: the daemon verb router. The FUSE
    // daemon's `Metadata`-trait verbs (unlink/rename/setattr/xattrs/…)
    // consult it at the trait impl itself (`meta_backend/mod.rs`), so on
    // this mount every FOREIGN-home one SHIPS instead of refusing at the
    // write gate — closing "S8's un-routed-daemon gap":
    // `cowriter.local_commit_refusals` growth on a real workload is a BUG
    // from here on, exactly as the S8-b falsifier demands.
    crate::meta_ship::install_daemon_verb_router(crate::meta_ship::MetaShipRouter::new(
        Arc::clone(meta),
        node_id,
        secret.clone(),
    ));

    // The custody client: the acquire travels, and what comes back is
    // custody the authority ISSUED (adopted with the owner's own token).
    let client = crate::data_grant::WriteCustodyClient::connect_with_clock(
        authority_endpoint,
        &secret,
        node_id,
        crate::membership::LeaseClock::monotonic(),
        pr_key,
    )
    .await
    .map_err(|e| {
        crate::meta_ship::publish::uninstall_client();
        crate::meta_ship::uninstall_daemon_verb_router();
        SqueezefsError::InvalidOperation(format!(
            "{posture} arm failed: the custody authority at {authority_endpoint} refused or \
             could not be reached ({e}). The admission ladder passed, so this is a transport or \
             authority-state problem, not a posture one"
        ))
    })?;
    crate::data_grant::install_custody_client(Arc::clone(&client));

    // The accounting latch: neither client posture DEALLOCATES locally —
    // §6.3's reclaim/discard hazard applies verbatim, and the offsets
    // belong to the authority's ledger. Terminal frees SHIP
    // (`ship_displaced_frees`, wired at the router's free seam), and the
    // device reclaim they imply runs on the AUTHORITY's reclaimer — so
    // this queue is ceased, not merely idle.
    router.backend_router.reclaim_cease();

    // DLM S9 blocker #3's ADMISSION (`crate::alloc_lane_grant`): the lease we
    // just adopted names this mount's data-plane allocation lane, so engage
    // it — the residue class this mount alone mints in, the routed
    // reservation sink (a client's raises SHIP: the record is a metadata
    // commit on ino 1, and the set authority is the only node that may
    // write it), and a floor OPENED by the authority (neither client
    // posture runs an ownership-recovery walk, so it must be told where
    // the set's live data ends).
    //
    // Deliberately AFTER the publish client is installed — the raise routes
    // through it — and after the custody client, whose lease is the only
    // source of the lane. A SOLO lease (an authority that has enrolled
    // nobody) engages nothing, and then allocation stays refused exactly as
    // it was before this seam closed.
    let lane = client.lane_partition();
    if lane.is_solo() {
        log::warn!(
            "{}: the authority granted no allocation lane (its era runs no data-plane \
             partition), so this mount can place NO fresh block — every ownership-accounting arm \
             stays refused (cowriter.accounting_refusals). {lane_remedy}",
            posture.to_uppercase()
        );
    } else if let Err(e) =
        crate::alloc_lane_grant::engage_co_writer_lanes(lane, &router.backend_router, meta).await
    {
        crate::data_grant::uninstall_custody_client();
        crate::meta_ship::publish::uninstall_client();
        crate::meta_ship::uninstall_daemon_verb_router();
        return Err(SqueezefsError::InvalidOperation(format!(
            "{posture} arm failed: the allocation lane {} of {} the authority granted could not \
             be engaged ({e}). Refusing the mount rather than serving one that would either \
             place no block at all or place one over another writer's",
            lane.writer_id(),
            lane.writers()
        )));
    }

    spawn_custody_renewal(Arc::clone(&client), Arc::clone(stop));
    Ok(client)
}

/// The co-writer's renewal cadence: renew before the authority's TTL, and
/// **self-fence at our own (strictly earlier) deadline** if renewal stops
/// completing — poisoning process data custody so nothing can land after
/// the authority may have granted those bytes elsewhere (§6.7's stricter
/// client clock, S6's law reused verbatim).
///
/// **Liveness isolation** (finding 2, the 2026-08-19 mw fleet run — the
/// membership twin's law applied verbatim; contracts
/// `tests/membership_liveness_tests.rs`): a lease renewal is a heartbeat
/// — it must be isolated from the workload whose stall it is supposed to
/// survive. The loop rides the dedicated `sqz-lease` lane (never the
/// shared sqz-meta pool a workload-class poll can occupy past `T_self`),
/// each attempt is deadline-bounded
/// ([`crate::data_grant::WriteCustodyClient::renew_tick_bound_ms`] —
/// `max(remaining-to-T_self / 3, one cadence)`), and the wire itself is
/// the client's DEDICATED lease session, never the acquire storm's (the
/// second fate-sharing the field's 35-attempt POSIX-5 ladders exposed).
/// A bounded attempt that expires past `T_self` still fences — §6.7 is
/// byte-identical; the isolation makes the fence unnecessary under load,
/// never weaker.
///
/// Public so the liveness contracts can drive the REAL cadence loop
/// against an in-process authority.
pub fn spawn_custody_renewal(
    client: Arc<crate::data_grant::WriteCustodyClient>,
    stop: Arc<AtomicBool>,
) {
    crate::meta_exec::spawn_lease("cowriter_custody_renewal", async move {
        loop {
            // Rung-10 finding #1: the due distance is the CLIENT's own
            // clock's (`renewal_due_ms`) — a fresh monotonic clock here
            // read now ≈ 0, so `due` equaled the absolute deadline and the
            // cadence doubled every cycle until the lease died at its 4th
            // renewal (pinned:
            // the_custody_renewal_cadence_is_anchored_on_the_clients_own_clock).
            let due = client.renewal_due_ms();
            squeezefs_ipc::sqz_time::sleep(Duration::from_millis(due)).await;
            if stop.load(Ordering::Acquire) {
                return;
            }
            let bound = client.renew_tick_bound_ms();
            match squeezefs_ipc::sqz_time::timeout(Duration::from_millis(bound), client.renew_all())
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if client.self_fence_due() {
                        client.self_fence(&format!("custody renewal failed: {e}"));
                        return;
                    }
                    log::warn!(
                        "co-writer custody renewal failed ({e}) — retrying before my own \
                         deadline (T_self, strictly earlier than the authority's TTL)"
                    );
                    squeezefs_ipc::sqz_time::sleep(Duration::from_millis(25)).await;
                }
                Err(_) => {
                    // The per-attempt warning the field lacked. Unlike an
                    // Err, a hang carries no owner verdict — but §6.7 does
                    // not wait for one: past T_self the fence fires HERE
                    // (it needs no wire), because a permanently hung
                    // attempt would otherwise never reach the Err arm.
                    if client.self_fence_due() {
                        client.self_fence(&format!(
                            "custody renewal attempt still incomplete at T_self (bounded at \
                             {bound} ms per attempt)"
                        ));
                        return;
                    }
                    log::warn!(
                        "co-writer custody renewal attempt exceeded its {bound} ms deadline \
                         (max(remaining-to-T_self/3, one cadence)) — abandoning it so the \
                         lease venue keeps its cadence; the abandoned wire session \
                         reconnects on the next attempt"
                    );
                    squeezefs_ipc::sqz_time::sleep(Duration::from_millis(25)).await;
                }
            }
        }
    })
}

// ===========================================================================
// DLM S9 — the co-writer FREE path (the displaced half of a rewrite).
//
// Contracts: `tests/mw_cowriter_free_tests.rs`. Wire:
// `crate::meta_ship::publish` (`PublishCall::FreeBlocks`). Operator story:
// `docs/operations.md` §Multi-writer co-writer mounts.
//
// The split, in one paragraph: a free's DURABLE effect (the
// `TREE_BLOCK_REFS` delete) already rides the layout publish that displaced
// the block — shipped, whole-tx, the ordering point. What is left is the
// ACCOUNTING ladder (`begin_free` → tier purge → reclaim enqueue →
// `finish_free`, with §6.8 item 3's grace ring and S7's quarantine composing
// inside `finish_free`), whose durable home is the authority's ledger and
// whose device reclaimer is live only there. So a co-writer's router-level
// terminal free SHIPS as a verb and the AUTHORITY executes the whole ladder
// exactly as if it had freed locally; the freed offset re-enters the free
// supply of whichever lane the arithmetic (`b % W`) names — frees stay
// lane-blind, which is the partition's own law.
// ===========================================================================

squeezefs_ipc::sqz_task_local! {
    /// The **authority-accounting venue marker**: set for exactly one task
    /// tree — the shipped-free executor's ([`execute_shipped_frees`]) —
    /// and read only on the never-taken co-writer branch of the ownership-
    /// accounting gates (`BlockAllocator::plane_gate`, the reclaim
    /// enqueue, the router's free seam).
    ///
    /// Why it exists: the mount-posture latch is PROCESS-scoped (correct —
    /// one process is one mount), but the owner-side executor acts with
    /// the AUTHORITY's accounting authority for the SET, on behalf of a
    /// validated peer request. In production the two coincide (the
    /// authority's posture is `writer`, so the gates never consult this);
    /// in any venue where one process plays both nodes, this marker is
    /// what keeps the executor's ladder from being mistaken for the
    /// co-writer's own — and it is NEVER ambiently active (pinned:
    /// `authority_solo_and_reader_free_paths_are_unchanged_and_w1_stays_refused`).
    static AUTHORITY_FREE_SCOPE: ();
}

/// Run `fut` under the authority-accounting scope. The ONE caller is the
/// shipped-free executor; nothing else may enter it (a second caller would
/// be a bypass of the ownership-accounting gate, not a venue).
pub async fn with_authority_accounting<F>(fut: F) -> F::Output
where
    F: std::future::Future,
{
    AUTHORITY_FREE_SCOPE.scope((), fut).await
}

/// `true` ⇔ the current task runs under [`with_authority_accounting`].
/// Cost discipline: consulted only AFTER a posture latch already said
/// reader/co-writer, so a write mount never pays the task-local probe.
pub fn authority_accounting_scope_active() -> bool {
    AUTHORITY_FREE_SCOPE.try_with(|_| ()).is_ok()
}

/// The free verb's client-side request ids: monotone per process. The
/// witness is `(lease_epoch, request_id)` — the epoch scopes the id, so a
/// restarted co-writer (a new join = a new epoch) can never alias a prior
/// incarnation's ids.
static FREE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Mint one client-side ship request id — shared by the free and harvest
/// verbs (each has its own dedup window; one monotone sequence keeps ids
/// unique per process regardless of verb).
pub(crate) fn next_ship_request_id() -> u64 {
    FREE_SEQ.fetch_add(1, Ordering::AcqRel) + 1
}

/// Bounded resend budget for one free verb. A protocol constant, not a
/// resource cap (nothing here derives from machine size): each resend is
/// absorbed exactly-once by the owner's dedup window, and past the budget
/// the abandon is the LEAK-SAFE direction (durably free, returned by the
/// authority's next derivation) — counted loud on `free_ship_failures`.
const FREE_SHIP_ATTEMPTS: u32 = 3;

/// **Ship a batch of displaced-block terminal frees to the authority** —
/// the co-writer arm of [`crate::routing::BackendRouter::free_block`] /
/// `free_blocks` (the write path's displacement calls, truncate's and
/// unlink's).
///
/// Per key, in order:
/// 1. the same resolution the local ladder runs (decoration strip, parse,
///    stale-incarnation refusal, unknown-backend skip);
/// 2. the verb ships — `(vol_tag, block_idx)`, the durable identity, never
///    a path or a key string — under the CURRENT lease epoch and one fresh
///    request id, with bounded epoch-stable retries (see below);
/// 3. only after the authority's acknowledgement: the LOCAL non-accounting
///    hygiene — the read-tier purge and the local tracking retire — so a
///    free that never shipped leaves this mount's state untouched
///    (leak-safe, re-derivable).
///
/// **A retry never re-keys.** It carries the SAME `(epoch, id)`; if the
/// lease epoch moves under it (revocation → re-join) the free is ABANDONED,
/// because a resend under the new epoch would be a new act the window
/// cannot correlate — the freed-then-reallocated ABA window. The abandoned
/// offset is durably unreferenced and recovery owns it.
pub async fn ship_displaced_frees(
    router: &crate::routing::BackendRouter,
    block_keys: &[&str],
) -> Result<()> {
    struct FreeGroup {
        alloc: Arc<crate::block_allocator::BlockAllocator>,
        vol_tag: u64,
        /// `(cleaned key, offset, block_idx)` per displaced block.
        entries: Vec<(String, u64, u64)>,
    }
    let mut groups: Vec<FreeGroup> = Vec::new();
    let mut first_err: Option<SqueezefsError> = None;
    for &key in block_keys {
        let cleaned = crate::routing::clean_block_key(key);
        let parts = match router.parse_block_key_parts(&cleaned) {
            Ok(p) => p,
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
                continue;
            }
        };
        // Spec §6.2 item 6: the same stale-lifetime refusal the local free
        // runs — a free under a dead incarnation is the §6.3 hazard's
        // destructive face wherever it executes.
        if !router.block_key_incarnation_ok(&cleaned) {
            if first_err.is_none() {
                first_err = Some(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "block key '{key}' names a dead incarnation of its device offset (spec \
                         §6.2 item 6) — refusing to ship its free"
                    ),
                )));
            }
            continue;
        }
        let Some(alloc) = router.allocator_for_be_id(&parts.be_id) else {
            // Unknown/offline backend: the local ladder's silent skip.
            continue;
        };
        let vol_tag = crate::meta_backend::kv::block_refs::volume_tag(alloc.volume_id());
        let idx = parts.offset / alloc.chunk_size().max(1);
        match groups
            .iter_mut()
            .find(|g| g.vol_tag == vol_tag && Arc::ptr_eq(&g.alloc, &alloc))
        {
            Some(g) => g.entries.push((cleaned, parts.offset, idx)),
            None => groups.push(FreeGroup {
                alloc,
                vol_tag,
                entries: vec![(cleaned, parts.offset, idx)],
            }),
        }
    }
    if groups.is_empty() {
        return match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        };
    }

    let Some(client) = crate::data_grant::custody_client() else {
        let blocks: u64 = groups.iter().map(|g| g.entries.len() as u64).sum();
        crate::meta_ship::publish::note_free_ship_failure(blocks);
        let msg = format!(
            "S9: {} displaced-block free(s) cannot ship — this co-writer holds no custody \
             client, so there is no lease epoch to present and no authority to execute the \
             ladder. Nothing moved locally (leak-safe: the offsets are durably unreferenced and \
             the authority's next derivation returns them); arm the co-writer mount, which \
             installs the client",
            blocks
        );
        log::error!("{msg}");
        return Err(SqueezefsError::InvalidOperation(msg));
    };
    let endpoint = client.endpoint().to_string();

    for group in groups {
        let epoch = client.lease_epoch();
        let request_id = next_ship_request_id();
        let idxs: Vec<u64> = group.entries.iter().map(|(_, _, idx)| *idx).collect();
        let mut attempt = 0u32;
        let shipped = loop {
            match crate::meta_ship::publish::ship_free_blocks(
                &endpoint,
                group.vol_tag,
                idxs.clone(),
                epoch,
                request_id,
            )
            .await
            {
                Ok(verdicts) => break Ok(verdicts),
                Err(e) => {
                    attempt += 1;
                    // A retry NEVER re-keys: if the lease epoch moved (a
                    // revocation → re-join happened under us), abandon —
                    // the window cannot correlate a new epoch's resend
                    // with the old one's possible execution, and the
                    // ABA-safe direction is the leak-safe one.
                    if client.lease_epoch() != epoch || attempt >= FREE_SHIP_ATTEMPTS {
                        break Err(e);
                    }
                    squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        };
        match shipped {
            Ok(_verdicts) => {
                // The authority owns the accounting now; retire this
                // mount's local, non-accounting view of the displaced
                // blocks — the read tiers (a reused offset must never
                // tier-hit the dead incarnation's bytes) and the local
                // refcount/incarnation tracking.
                for (key, offset, _idx) in &group.entries {
                    router.purge_read_tiers(key);
                    group.alloc.retire_shipped_free_tracking(*offset);
                }
            }
            Err(e) => {
                crate::meta_ship::publish::note_free_ship_failure(group.entries.len() as u64);
                log::error!(
                    "S9: ABANDONING {} displaced-block free(s) on vol_tag {:#016x} after \
                     {attempt} attempt(s) ({e}). The blocks are durably unreferenced (the \
                     publish landed) and stay out of every free list until the authority's \
                     next derivation — the leak-safe direction (free_ship_failures)",
                    group.entries.len(),
                    group.vol_tag,
                );
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// **The owner half: execute a peer's shipped frees** against this
/// authority's data plane — the [`crate::meta_ship::publish::FreeExecutor`]
/// body. Runs under [`with_authority_accounting`], because this ladder IS
/// the authority's own accounting act (in production the posture latch
/// already says `writer`; the scope is what keeps that true in any venue
/// where one process plays both nodes).
///
/// Per block, the verdict derivation — RAM first, the durable ledger for
/// what RAM never tracked:
///
/// * **RAM-tracked** (the authority minted or recovered it): the standard
///   [`crate::routing::BackendRouter::free_block`] ladder runs and the
///   refcount decides terminal vs not — byte-identical to a local free;
/// * **untracked, ledger population > 0**: `NonTerminal` — the reference
///   release already happened durably on the publish, and there is no RAM
///   state to move;
/// * **untracked, population 0, not already free/graced/quarantined/
///   mid-reclaim**: seed ONE reference ([`crate::block_allocator::
///   BlockAllocator::seed_shipped_free_reference`] — deliberately NOT
///   `recover_block`, whose gap-filling arm would declare a live peer's
///   unpublished tail free) and run the ladder — `Freed`;
/// * **anything else**: the double-release lineage — routed through the
///   ladder UNSEEDED so the existing untracked-free tripwire counts it,
///   and answered `Refused`.
///
/// `view` is the ledger reader's ownership binding (finding 13 — see
/// [`OwnerView`]): production passes [`live_owner_view`]; a venue where
/// one process plays both nodes passes [`local_owner_view`], because the
/// process-global map there belongs to the OTHER posture.
pub async fn execute_shipped_frees(
    backend: &Arc<crate::routing::BackendRouter>,
    meta: &Arc<RoutedMetaBackend>,
    vol_tag: u64,
    blocks: &[u64],
    view: &OwnerView,
) -> Result<Vec<crate::meta_ship::publish::FreeVerdict>> {
    use crate::meta_ship::publish::FreeVerdict;
    let Some((be_id, alloc)) = backend.allocator_for_volume_tag(vol_tag) else {
        return Err(SqueezefsError::InvalidOperation(format!(
            "S9: a shipped free names data volume tag {vol_tag:#016x}, which this authority \
             routes no allocator for — refusing rather than freeing on a guessed volume"
        )));
    };
    let backend = Arc::clone(backend);
    let meta = Arc::clone(meta);
    let blocks = blocks.to_vec();
    let view = Arc::clone(view);
    with_authority_accounting(async move {
        let chunk = alloc.chunk_size();
        let mut verdicts = Vec::with_capacity(blocks.len());
        let mut discharged: Vec<u64> = Vec::new();
        // Finding 13: ONE owner-partitioned ledger read for the whole
        // batch (a round trip per distinct peer, not per block) — the
        // untracked arm below consumes it.
        let populations = durable_block_refcounts_with(&meta, vol_tag, &blocks, &view).await?;
        for (slot, idx) in blocks.iter().copied().enumerate() {
            let offset = idx.saturating_mul(chunk);
            let key = backend.persist_block_key(&be_id, offset);
            let verdict = match alloc.refcount(offset) {
                Some(n) if n > 0 => {
                    backend.free_block(&key).await?;
                    if n == 1 {
                        FreeVerdict::Freed
                    } else {
                        FreeVerdict::NonTerminal
                    }
                }
                Some(_) => {
                    // A zero-count entry is the transient window of a
                    // racing terminal release: the OTHER free won, ours is
                    // the double-release lineage. Refuse without poking a
                    // mid-transition entry.
                    log::error!(
                        "S9: shipped free of block {idx} (vol_tag {vol_tag:#016x}) raced a \
                         terminal release mid-transition — refused (double-release lineage)"
                    );
                    crate::fuse_client::METRICS
                        .block_untracked_free_refusals
                        .fetch_add(1, Ordering::Relaxed);
                    FreeVerdict::Refused
                }
                None => {
                    if populations[slot] > 0 {
                        FreeVerdict::NonTerminal
                    } else if alloc.free_list_contains(idx)
                        || alloc.grace_holds(offset)
                        || alloc.is_quarantined(offset)
                        || alloc.inflight_contains(offset)
                    {
                        // Already free (or owed to grace / a dead epoch /
                        // the reclaimer): the double-release lineage. The
                        // UNSEEDED ladder refuses it on the existing
                        // untracked tripwire — never a second free.
                        backend.free_block(&key).await?;
                        FreeVerdict::Refused
                    } else {
                        alloc.seed_shipped_free_reference(offset);
                        backend.free_block(&key).await?;
                        FreeVerdict::Freed
                    }
                }
            };
            if verdict == FreeVerdict::Freed {
                // The offset is back under this authority's own ladder, so
                // any lane-harvest handout of it is DISCHARGED (rung 10):
                // it is no longer any epoch's reallocation hazard.
                discharged.push(offset);
            }
            verdicts.push(verdict);
        }
        crate::data_grant::discharge_lane_handouts(&discharged);
        Ok(verdicts)
    })
    .await
}

/// **The owner half of the lane free HARVEST** (rung 10, residual 2 — the
/// [`crate::meta_ship::publish::HarvestExecutor`] body): hand a validated
/// co-writer up to `max` free-listed block indices of ITS lane, removing
/// each from this authority's own free list (exactly-once) and recording
/// the handout against `lease_epoch` (quarantine-on-death until the
/// offset's next shipped free discharges it).
///
/// When the lane's supply is empty but frees may still sit in the reclaim
/// queue, the pass drains it once and rescans — the ENOSPC pressure
/// valve's act, on the one node whose reclaimer is live. Runs under
/// [`with_authority_accounting`] like the free executor: this IS the
/// authority's own accounting act, performed for a validated peer.
pub async fn execute_lane_harvest(
    backend: &Arc<crate::routing::BackendRouter>,
    vol_tag: u64,
    lane: u16,
    writers: u16,
    max: u64,
    lease_epoch: u64,
) -> Result<Vec<u64>> {
    let Some((_be_id, alloc)) = backend.allocator_for_volume_tag(vol_tag) else {
        return Err(SqueezefsError::InvalidOperation(format!(
            "S9: a lane free harvest names data volume tag {vol_tag:#016x}, which this \
             authority routes no allocator for — refusing rather than handing out offsets of a \
             guessed volume"
        )));
    };
    let backend = Arc::clone(backend);
    with_authority_accounting(async move {
        let max = max.max(1) as usize;
        let mut out: Vec<u64> = Vec::new();
        for pass in 0..3u8 {
            // Finding 15 (`.benchmarks/2026-08-25-s11-freeloop-stall.md`):
            // this is a REMOTE allocation funnel, so it runs the same
            // grace-ring head the local one does (`try_allocate_block`) —
            // without it, a grace-armed fleet's displaced offsets were
            // releasable yet unreachable (the ring is harvested only from
            // the authority's own allocation/free contexts, which stop
            // running exactly when the fleet's writers are the starving
            // ones), and the s11 row ENOSPC'd on a healthy volume with
            // the pressure gauge reading 0.
            match pass {
                0 => alloc.harvest_grace(),
                1 => {
                    // Nothing free in the lane: the supply may still be
                    // queued behind the reclaim manners law — drain, run
                    // the ring head again (the drain's finish_free defers
                    // INTO the ring on an armed plane), and rescan.
                    backend.reclaim_drain().await;
                    alloc.harvest_grace();
                }
                _ => {
                    // Still nothing: this writer is at its allocation
                    // cliff, which is exactly what the PRESSURE deadline
                    // exists for (the pressure ruling: prompt progress
                    // past one honest ack cycle, a fenced laggard —
                    // never a broken promise). This is also what makes
                    // the valve's reading honest fleet-wide: the capture
                    // ran a whole ENOSPC storm at pressure_pct 0 because
                    // only the authority's OWN cliff ever fed it.
                    alloc.harvest_grace_pressure();
                }
            }
            let mut candidates: Vec<u64> = alloc
                .free_block_indices()
                .into_iter()
                .filter(|idx| {
                    crate::data_alloc_lane::block_lane_of(*idx, writers) == u64::from(lane)
                })
                .collect();
            // Lowest-first: deterministic, and it keeps the handed-out run
            // as dense as a strided lane allows (the contiguity posture).
            candidates.sort_unstable();
            for idx in candidates {
                if out.len() >= max {
                    break;
                }
                if alloc.take_free_for_lane_grant(idx) {
                    out.push(idx);
                }
            }
            if !out.is_empty() {
                break;
            }
        }
        if !out.is_empty() {
            let chunk = alloc.chunk_size();
            let offsets: Vec<u64> = out.iter().map(|idx| idx * chunk).collect();
            crate::data_grant::note_lane_handouts(lease_epoch, &offsets);
            log::info!(
                "S9: lane free harvest served {} block(s) of lane {lane}/{writers} on vol_tag \
                 {vol_tag:#016x} to lease epoch {lease_epoch} (removed from this authority's \
                 free list; quarantine-on-death until discharged)",
                out.len()
            );
        }
        Ok(out)
    })
    .await
}

/// A [`crate::meta_ship::publish::HarvestExecutor`] over this authority's
/// data router — installed beside the free executor by
/// `multi_writer::arm_multi_writer` (and the rigs directly): the free
/// RETURNS a co-writer's displaced offset to the lane's supply, the
/// harvest is what makes that supply REACHABLE again.
pub fn router_harvest_executor(
    backend: Arc<crate::routing::BackendRouter>,
) -> crate::meta_ship::publish::HarvestExecutor {
    Arc::new(
        move |vol_tag: u64, lane: u16, writers: u16, max: u64, lease_epoch: u64| {
            let backend = Arc::clone(&backend);
            Box::pin(async move {
                execute_lane_harvest(&backend, vol_tag, lane, writers, max, lease_epoch).await
            })
        },
    )
}

/// A [`crate::meta_ship::publish::FreeExecutor`] over this authority's data
/// router + metadata set — what `multi_writer::arm_multi_writer` installs
/// beside the frontier source (and the rigs install directly). The
/// INSTALLER binds the ledger reader's ownership view (finding 13):
/// production passes [`live_owner_view`], the one-process rigs
/// [`local_owner_view`].
pub fn router_free_executor(
    backend: Arc<crate::routing::BackendRouter>,
    meta: Arc<RoutedMetaBackend>,
    view: OwnerView,
) -> crate::meta_ship::publish::FreeExecutor {
    Arc::new(move |vol_tag: u64, blocks: Vec<u64>| {
        let backend = Arc::clone(&backend);
        let meta = Arc::clone(&meta);
        let view = Arc::clone(&view);
        Box::pin(
            async move { execute_shipped_frees(&backend, &meta, vol_tag, &blocks, &view).await },
        )
    })
}

/// The ledger reader's view of metadata-volume ownership — **bound at ARM
/// time by the node that installs the shipped-free executor** (finding
/// 13's venue law): the process-global ownership map belongs to whichever
/// posture armed last, which in the one-process test rigs is not
/// necessarily the node whose ladder runs — an executor reading it there
/// ships the population read to ITSELF (self-serve on the same meta pool,
/// the wedge the mw_cowriter_free venue exposed). Production binds
/// [`live_owner_view`] because there the global map IS the installing
/// node's own; the single-authority rigs bind [`local_owner_view`].
pub type OwnerView = Arc<dyn Fn(usize) -> Option<Arc<crate::meta_ship::PeerOwner>> + Send + Sync>;

/// The live ownership plane's view — production's binding: reads the
/// global map at RUN time, so it follows `rearm_ownership` (slot
/// migration) instead of rotting on an arm-time snapshot.
pub fn live_owner_view() -> OwnerView {
    Arc::new(crate::meta_ship::owner_of_volume)
}

/// An all-local view — the single-authority binding (every volume of the
/// routed set is the installing node's own; classic S9's shape).
pub fn local_owner_view() -> OwnerView {
    Arc::new(|_| None)
}

/// The durable reference populations of a batch of blocks across the
/// metadata set — `refcount(block) == records under the (vol_tag,
/// block_idx) prefix` (`TREE_BLOCK_REFS`'s own law), **read where each
/// volume's ledger LIVES** (finding 13, per-volume claim admission PR 8;
/// contracts `tests/pv_shipped_free_ledger_tests.rs`):
///
/// * an OWNED volume (or every volume on an unarmed mount — the solo
///   re-gate: the view answers `None` there) reads its local live tree,
///   verbatim the pre-finding behaviour;
/// * a PEER-OWNED volume's count SHIPS to its owner
///   ([`crate::meta_ship::publish::ship_block_ref_population`], one
///   batched round trip per distinct peer), because this mount's copy is
///   a lagged reader snapshot — reading it answered a released reference
///   as still held (`NonTerminal` forever, the ship=128/serve=0 strand)
///   and a fresh one as absent (a false `Freed`, §6.3's destructive
///   face).
///
/// A peer that cannot answer is a loud error, never a fallback to the
/// snapshot: the shipped-free serve that needed the count refuses, and
/// the shipper's bounded retry ladder owns the leak-safe abandon.
pub async fn durable_block_refcounts_with(
    meta: &Arc<RoutedMetaBackend>,
    vol_tag: u64,
    block_idxs: &[u64],
    view: &OwnerView,
) -> Result<Vec<usize>> {
    let mut out = vec![0usize; block_idxs.len()];
    let mut peers: Vec<Arc<crate::meta_ship::PeerOwner>> = Vec::new();
    for (v_idx, kv) in meta.volumes.iter().enumerate() {
        match view(v_idx) {
            None => {
                for (slot, idx) in block_idxs.iter().enumerate() {
                    out[slot] += kv.block_ref_count(vol_tag, *idx).await.map_err(|e| {
                        SqueezefsError::InvalidOperation(format!(
                            "durable block-reference count failed on {} while serving a \
                             shipped free: {e}",
                            kv.device_path().display()
                        ))
                    })?;
                }
            }
            Some(peer) => {
                // One round trip per DISTINCT peer: the serve answers for
                // every volume that peer owns at once.
                if !peers
                    .iter()
                    .any(|p| p.peer_id == peer.peer_id && p.endpoint == peer.endpoint)
                {
                    peers.push(peer);
                }
            }
        }
    }
    for peer in peers {
        if peer.endpoint.is_empty() {
            return Err(SqueezefsError::InvalidOperation(format!(
                "S9: the block-reference population of vol_tag {vol_tag:#016x} needs peer \
                 '{}', whose endpoint is not yet resolved — refusing rather than validating a \
                 free against this mount's lagged snapshot of that peer's volume (finding 13)",
                peer.peer_id
            )));
        }
        let counts = crate::meta_ship::publish::ship_block_ref_population(
            &peer.endpoint,
            vol_tag,
            block_idxs.to_vec(),
        )
        .await?;
        for (slot, c) in counts.iter().enumerate() {
            out[slot] += *c as usize;
        }
    }
    Ok(out)
}

/// The LOCAL ledger census of one block — this mount's own trees only,
/// the pre-finding-13 read verbatim: the single-authority venues' truth
/// (classic S9, where every volume of the routed set is the authority's
/// own) and the instrument the free-path tests read back. The shipped-free
/// VALIDATION never calls this — it rides
/// [`durable_block_refcounts_with`] under the view its installer bound.
pub async fn durable_block_refcount(
    meta: &Arc<RoutedMetaBackend>,
    vol_tag: u64,
    block_idx: u64,
) -> Result<usize> {
    Ok(durable_block_refcounts_with(meta, vol_tag, &[block_idx], &local_owner_view()).await?[0])
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
        "unpublished_abandons": METRICS.cowriter_unpublished_abandons.load(Ordering::Relaxed),
        "local_commit_refusals": METRICS.cowriter_local_commit_refusals.load(Ordering::Relaxed),
        "custody_endpoint": declared_authority().unwrap_or_else(|| "none".to_string()),
    })
}
