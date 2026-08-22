//! **Per-volume claim admission — the seven-rung ladder**
//! (`docs/design-per-volume-claim-admission.md` §5.1/§5.3, rulings
//! **D18/D19/D20**, KD-PV-1…16; contracts `tests/pv_admission_tests.rs`).
//!
//! # What this module decides, and what it deliberately does not
//!
//! Every metadata volume of a set is opened `OpenMode::Write` today, in set
//! order, by ONE node (`open_meta_volume_set`) — that loop, not a policy,
//! is what makes "one metadata authority per volume set" true, and it is
//! what bounds aggregate co-writer ingest at ≈ 2.6 GiB/s. This ladder is
//! the decision that lets a mount open a set in which **a different node
//! appends to each volume**, while keeping **exactly one appender per
//! volume** (KD-PV-1 — the durable single-appender structures are
//! untouched: one journal ring, one A/B extent bitmap, one root ledger,
//! one node cache per volume).
//!
//! It is the co-writer ladder ([`crate::cowriter::classify_admission`])
//! **extended, never forked** — the shared rungs are literally shared
//! (`cowriter::required_incompat_detail`, `cowriter::member_standing`,
//! `cowriter::registrant_detail`, all `pub(crate)`) — with two new rungs
//! and one structural change:
//!
//! | Rung | Requirement | The threat it answers |
//! |---|---|---|
//! | **1** | `SQUEEZEFS_MULTI_WRITER=1`, `SQUEEZEFS_MW_ROLE` = `set-authority` \| `partial-authority`, not `-o ro`, and — for a partial authority — a declared `SQUEEZEFS_MW_AUTHORITY` | an accidental second write mount. The posture is DECLARED, never inferred, so a plain mount of a claimed set still refuses `FreshForeign` verbatim |
//! | **2** | every volume carries the capability bits (bit 14 included) and answers a **durable** `claim_set`; its `owner` is read **per volume** | a half-engaged set is not a claim set, and a projection of `writer_claim` expresses EXCLUSION — it cannot carry an assignment at all |
//! | **3** | this node is a `Writer` member everywhere; the assignment map is COMPLETE; each peer volume's `owner` is an enrolled writer; the declared role matches the slot-0 volume's assignment (D20) | self-assertion, and a partial map — a set where one volume names an owner and its sibling does not has no coherent appender story |
//! | **4** | a live membership lease (partial authority) whose era is not older than the **slot-0 volume's** claim | a mount with no live authority has no custody source and no evictor. Terms diverge per owner, so the comparison is against slot 0 only — a peer volume's own era is learned at runtime through `era_relearns` |
//! | **5** | the standing WERO hold names this node a registrant | §6.7's *"refused on non-PR"* governs the admission decision too: a mount whose DMA the device cannot reject is one nothing can fence |
//! | **6** | **assignment ∧ evidence, per volume** (KD-PV-3) | two nodes with different maps is two appenders or an orphaned volume. An own volume must be one the D0 ladder would GRANT; a peer volume must be one a NAMED peer is actually appending to. Anything else refuses — never adopts |
//! | **7** | every peer volume's **projected** `claim_set` shows its own assignment | the freeze precondition §5.9's online inode plane rests on: a monotone projection that shows the assignment record shows every commit that preceded it on that volume (§5.9.2). Self-certifying — no barrier verb |
//!
//! The ladder is **pure**: it opens nothing, reads no environment and
//! mutates nothing. Producing the evidence — the probe opens, the D0
//! classification, a peer volume's checkpoint-consistent projection, the
//! durable identity of a live claim holder — is the mount path's job, and
//! the partial open that consumes the decision is PR 4. Nothing calls this
//! from `main` yet.
//!
//! # Two identities, and the one this ladder decides over
//!
//! `WriterClaim.id` is a **per-mount uuid** (`backend.rs`'s `writer_id`),
//! while `claim_set.owner` is a **durable member id** (KD-MW-2). Comparing
//! them directly is the rung-9 finding #3 mistake that once refused every
//! healthy fleet's first co-writer, and §5.1.1's `recognizes` sketch would
//! re-make it. So rung 6 decides over
//! [`PvVolumeEvidence::holder_member_id`] — the live holder resolved to its
//! DURABLE identity by the gather — and an **unresolved holder is silence**:
//! it refuses, because "never adopt on silence" (KD-PV-3) is exactly the
//! law that a `None` here must not be allowed to soften.
//!
//! # Why the decision is unforgeable
//!
//! [`SetAdmission`]'s fields are private and [`classify_set_admission`] is
//! its only constructor, so PR 4's `open_peer_owned` cannot be reached
//! without a decision this ladder took: the D0 Layer-B2 gate stays
//! byte-identical for every mount that has not passed here.

use crate::cowriter::{
    admission_refusal, member_standing, registrant_detail, required_incompat_detail,
    AuthorityLeaseEvidence, MemberStanding, MwRole, RegistrantEvidence, MW_AUTHORITY_ENV,
    MW_ROLE_ENV,
};
use crate::error::{Result, SqueezefsError};
use crate::membership::{member_id_matches, ClaimSet};
use crate::meta_backend::kv::backend::WriterClaim;
use std::path::{Path, PathBuf};

/// The multi-writer opt-in, named in rung 1's refusals.
const MULTI_WRITER_ENV: &str = "SQUEEZEFS_MULTI_WRITER";

/// The D0 **Layer-B2** standing of one volume's replayed claim, as the
/// mount gate's own `classify_claim` answered it.
///
/// Supplied as EVIDENCE rather than re-derived here: the dead-pid proof,
/// the boot-id scope and the `CLIENT_STALE_TTL_SECS` window are the gate's
/// law and must have exactly one spelling (PR 4 maps `ClaimEvidence` onto
/// this at the gather site).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimStanding {
    /// No claim, our own residue, or a holder proved dead on this boot —
    /// the D0 ladder would grant it.
    Reclaimable,
    /// A heartbeat-fresh claim from a holder we cannot prove dead.
    Fresh,
    /// TTL-stale, or unattributable bytes: a PR substrate preempts it, a
    /// non-PR substrate refuses and asks for operator attestation.
    Stale,
}

/// One metadata volume's admission evidence, read through a probe open
/// (never blocked, never writes).
#[derive(Debug, Clone)]
pub struct PvVolumeEvidence {
    /// The volume's device path (named in every refusal).
    pub path: PathBuf,
    /// The **durable** `vol-{hex}` identity (KD-5) every mode is keyed on
    /// — never a path, an ordinal or a set position.
    pub vol_id: String,
    /// `true` ⇔ this volume hosts slot 0, so its owner is the SET
    /// AUTHORITY (D20 / KD-PV-6: `route_ino_width(1, W) == (0, 1)`).
    pub hosts_slot_0: bool,
    /// `features_incompat` from the superblock the mount would open.
    pub features_incompat: u64,
    /// Does THIS volume's namespace advertise NVMe reservation support?
    /// Rung 6's own-mode `Stale` arm needs it: the D0 ladder preempts a
    /// stale claim on a PR substrate and refuses on any other.
    pub pr_capable: bool,
    /// The replayed `writer_claim`, when one exists.
    pub claim: Option<WriterClaim>,
    /// The live holder's **durable member id**, resolved by the gather —
    /// never `WriterClaim.id`, which is a per-mount uuid (see the module
    /// docs). `None` = unresolved, which rung 6 reads as silence.
    pub holder_member_id: Option<String>,
    /// What the D0 Layer-B2 gate says about that claim.
    pub standing: ClaimStanding,
    /// The claim set as `ClaimSet::load` answers it: the durable record on
    /// an engaged volume, the singular projection otherwise (which rung 2
    /// refuses — `durable == false`).
    pub claim_set: Option<ClaimSet>,
    /// The claim set as this mount's **checkpoint-consistent projection**
    /// of the volume answers it — the view a peer-owned volume is served
    /// from. Rung 7's evidence; unread on an own-mode volume, which this
    /// mount appends to.
    pub projected_claim_set: Option<ClaimSet>,
    /// Where this volume's owner serves its metadata plane, when the
    /// gather knows: the declared set-authority endpoint for the slot-0
    /// owner, or the member's own published endpoint. Best-effort — a
    /// peer's endpoint is resolved from the live membership census at
    /// runtime, so an absent one is announced, never refused (refusing
    /// would make the FIRST node of a fleet unmountable).
    pub owner_endpoint: Option<String>,
}

/// Everything the ladder decides over. The mount path builds this in PR 4;
/// the suite builds it directly (the co-writer `AdmissionRequest`
/// precedent) so every rung's refusal is pinned without a fabric.
#[derive(Debug, Clone)]
pub struct SetAdmissionRequest {
    /// `SQUEEZEFS_MULTI_WRITER=1`.
    pub multi_writer: bool,
    /// The declared `SQUEEZEFS_MW_ROLE`.
    pub role: MwRole,
    /// `-o ro` / `--read-only` (a category error for this posture).
    pub read_only: bool,
    /// This node's durable enrollment identity
    /// ([`crate::cowriter::node_member_id`]).
    pub node_id: String,
    /// The SET authority's endpoint (`SQUEEZEFS_MW_AUTHORITY`, D20).
    /// Required for `partial-authority`; **not read** for `set-authority`,
    /// which IS the endpoint.
    pub set_authority_endpoint: Option<String>,
    /// Per-volume evidence. Order is the caller's and carries no meaning:
    /// modes are keyed on [`PvVolumeEvidence::vol_id`].
    pub volumes: Vec<PvVolumeEvidence>,
    /// The membership plane's evidence (a partial authority's lease).
    pub authority: Option<AuthorityLeaseEvidence>,
    /// The device's evidence for the data namespaces.
    pub registrant: Option<RegistrantEvidence>,
}

/// What this mount does with ONE volume of the set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeMode {
    /// This mount is the appender: the full D0 ladder runs (Layer A,
    /// Layer B1, the claim commit, the checkpoint and times-drain tasks).
    Own,
    /// A peer appends to it: a released `LOCK_SH` probe, no PR acquire, no
    /// claim, no tasks — and every metadata mutation ships to `owner_id`.
    Peer {
        /// The durable member id currently appending to the volume: the
        /// live holder, which rung 6 proved is in the volume's assignment
        /// set (`owner` ∪ `successors`). After a KD-PV-12 adoption the
        /// record's `owner` still names the dead predecessor, so the
        /// HOLDER is what a verb must be shipped to (§5.10).
        owner_id: String,
        /// Where to ship, when the gather knew it (see
        /// [`PvVolumeEvidence::owner_endpoint`]).
        owner_endpoint: String,
    },
}

/// **The decision.** Constructible only by [`classify_set_admission`] —
/// every field is private, so no caller can open a peer-owned volume
/// without a gate outcome in hand.
#[derive(Debug, Clone)]
pub struct SetAdmission {
    node_id: String,
    role: MwRole,
    set_authority_endpoint: String,
    pr_key: u64,
    volumes: Vec<PathBuf>,
    slot_0_vol_id: String,
    modes: Vec<(String, VolumeMode)>,
}

impl SetAdmission {
    /// This node's durable enrollment identity.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The posture this decision was taken for.
    pub fn role(&self) -> MwRole {
        self.role
    }

    /// The SET authority's endpoint (empty on a set authority, which is
    /// the endpoint).
    pub fn set_authority_endpoint(&self) -> &str {
        &self.set_authority_endpoint
    }

    /// This node's device registrant key under the standing WERO hold.
    pub fn pr_key(&self) -> u64 {
        self.pr_key
    }

    /// `true` ⇔ `paths` is exactly the set this decision was taken over.
    ///
    /// Order-insensitive on purpose: the mount path opens
    /// `disc.ordered_paths` in canonical `member_position` order, not in
    /// the caller's URI order. A foreign volume, a missing volume or a
    /// truncated list all answer `false` — an admission decided over one
    /// set may never open a volume of another
    /// (the `open_co_writer` precedent).
    pub fn covers(&self, paths: &[String]) -> bool {
        // Both directions, not just "every path is one of mine": a list
        // that repeats one volume and omits another has the right length
        // and every entry covered, and it must not pass.
        paths.len() == self.volumes.len()
            && paths
                .iter()
                .all(|p| self.volumes.iter().any(|v| v == Path::new(p)))
            && self
                .volumes
                .iter()
                .all(|v| paths.iter().any(|p| Path::new(p) == v))
    }

    /// The volume paths this decision was taken over — named in the
    /// cross-set refusal, exactly as `CoWriterAdmission::volumes` is.
    pub fn volumes(&self) -> &[PathBuf] {
        &self.volumes
    }

    /// `true` ⇔ this decision was taken over a set containing `path`.
    ///
    /// The per-volume half of [`Self::covers`]: the open takes one volume
    /// at a time, and the whole-set check has already run by then. A path
    /// the decision never saw is refused rather than opened on the
    /// strength of a vol id it happens to name (the `open_co_writer`
    /// precedent, one grain finer).
    pub fn covers_path(&self, path: &Path) -> bool {
        self.volumes.iter().any(|v| v == path)
    }

    /// What this mount does with the volume of DURABLE id `vol_id`.
    /// `None` = this decision does not name that volume, which the open
    /// must read as a refusal, never as a default.
    pub fn mode_for(&self, vol_id: &str) -> Option<&VolumeMode> {
        self.modes
            .iter()
            .find(|(id, _)| id == vol_id)
            .map(|(_, mode)| mode)
    }

    /// `true` ⇔ this mount appends to at least one volume of the set.
    pub fn owns_any(&self) -> bool {
        self.modes.iter().any(|(_, m)| *m == VolumeMode::Own)
    }

    /// `true` ⇔ this mount owns the volume hosting slot 0, i.e. it is the
    /// SET AUTHORITY (D20).
    pub fn is_set_authority(&self) -> bool {
        self.mode_for(&self.slot_0_vol_id) == Some(&VolumeMode::Own)
    }
}

// ---------------------------------------------------------------------------
// The ladder
// ---------------------------------------------------------------------------

fn refuse(rung: u8, detail: String) -> SqueezefsError {
    admission_refusal("per-volume claim", rung, detail)
}

/// The role AFTER rung 1 narrowed it. Carrying this rather than
/// [`MwRole`] is what keeps the later rungs total: `authority` and
/// `co-writer` are refused at rung 1 and are then unrepresentable, so no
/// rung needs an arm for a posture that cannot reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Posture {
    SetAuthority,
    PartialAuthority,
}

/// One volume's rung-2 reading: the durable set beside the evidence it was
/// read with. The per-volume OWNER reading is what rung 2 adds to the
/// co-writer ladder's uniform verdict.
struct VolumeReading<'a> {
    vol: &'a PvVolumeEvidence,
    set: &'a ClaimSet,
}

impl VolumeReading<'_> {
    /// The durable member id assigned to append to this volume.
    fn assigned_owner(&self) -> Option<&str> {
        self.set.owner.as_deref()
    }

    /// The volume's ASSIGNMENT SET — `owner` ∪ `successors` (§5.10, Issue
    /// 24: after a legitimate KD-PV-12 adoption the record's `owner` still
    /// names the dead predecessor, so a singleton reading would refuse the
    /// very volume the opt-in recovered).
    fn assignment_names(&self, id: &str) -> bool {
        self.assigned_owner()
            .is_some_and(|o| member_id_matches(o, id))
            || self.set.successors.iter().any(|s| member_id_matches(s, id))
    }

    /// Is this node a declared adoption candidate for the volume?
    fn declares_successor(&self, node_id: &str) -> bool {
        self.set
            .successors
            .iter()
            .any(|s| member_id_matches(s, node_id))
    }
}

/// Rung 1 — the posture is DECLARED, not inferred. Answers the narrowed
/// posture and the SET authority's endpoint (empty for a set authority).
fn rung_1_declaration(req: &SetAdmissionRequest) -> Result<(Posture, &str)> {
    if !req.multi_writer {
        return Err(refuse(
            1,
            format!(
                "{MULTI_WRITER_ENV} is off. Per-volume claim admission is one half of the \
                 multi-writer guarantee class; without the opt-in this mount is a \
                 single-writer mount and the D0 guard arbitrates it, refusing a claimed set \
                 exactly as it always has"
            ),
        ));
    }
    let posture = match req.role {
        MwRole::SetAuthority => Posture::SetAuthority,
        MwRole::PartialAuthority => Posture::PartialAuthority,
        MwRole::Authority | MwRole::CoWriter => {
            return Err(refuse(
                1,
                format!(
                    "{MW_ROLE_ENV} is '{}', not `set-authority` or `partial-authority`. A mount \
                     that opens a set whose volumes a PEER holds the D0 claim on must say so: \
                     the posture is never inferred from the durable assignment, because an \
                     operator who typed nothing and got a per-volume open would learn nothing",
                    req.role.as_str()
                ),
            ));
        }
    };
    if req.read_only {
        return Err(refuse(
            1,
            "this mount is READ-ONLY (`-o ro` / `--read-only`). A reader takes no lease, holds \
             no registrant key and appends to nothing (DLM S5's posture); assigning it volumes \
             to append to is a category error, not a degradation. Drop -o ro, or drop the role"
                .to_string(),
        ));
    }
    let declared = req.set_authority_endpoint.as_deref().unwrap_or("").trim();
    if posture == Posture::SetAuthority {
        // D20: the set authority IS the endpoint, so the knob is NOT READ
        // here — a fleet that exports one authority endpoint everywhere
        // and varies only the role must still mount.
        if !declared.is_empty() {
            log::info!(
                "per-volume claim admission: {MW_AUTHORITY_ENV}={declared} is set on a \
                 `set-authority` mount and is NOT read — under D20 the owner of the slot-0 \
                 volume IS the set authority endpoint"
            );
        }
        return Ok((posture, ""));
    }
    if declared.is_empty() {
        return Err(refuse(
            1,
            format!(
                "no set authority is declared ({MW_AUTHORITY_ENV} is unset). A partial \
                 authority appends to a SUBSET of the set and ships every other volume's \
                 metadata verbs — plus its custody, its lane grant and its terminal frees — to \
                 the SET authority (the owner of the slot-0 volume, D20), so a partial \
                 authority with nowhere to dial is inert. Set {MW_AUTHORITY_ENV}=addr:port"
            ),
        ));
    }
    Ok((posture, declared))
}

/// Rung 2 — the format expresses a claim SET on every volume, durably, and
/// the OWNER reading becomes per volume: this is the verdict vector's
/// input (rung 3 resolves it into modes, once it knows the map is whole).
fn rung_2_engaged_claim_set(req: &SetAdmissionRequest) -> Result<Vec<VolumeReading<'_>>> {
    if req.volumes.is_empty() {
        return Err(refuse(2, "the metadata set has no volumes".to_string()));
    }
    let mut readings = Vec::with_capacity(req.volumes.len());
    for vol in &req.volumes {
        if let Some(detail) = required_incompat_detail(&vol.path, vol.features_incompat) {
            return Err(refuse(2, detail));
        }
        match &vol.claim_set {
            Some(set) if set.durable => readings.push(VolumeReading { vol, set }),
            Some(_) => {
                return Err(refuse(
                    2,
                    format!(
                        "metadata volume {} answered its claim set as the PROJECTION of a \
                         singular `writer_claim`, not as the durable `claim_set` record. A \
                         projection expresses EXCLUSION — one holder — and cannot carry a \
                         per-volume ownership assignment at all (`ClaimSet::store` refuses one \
                         without bit 14). Stamp bit 14, then run `squeezefs volume set-owners`",
                        vol.path.display()
                    ),
                ));
            }
            None => {
                return Err(refuse(
                    2,
                    format!(
                        "metadata volume {} carries no claim set: there is no durable statement \
                         of who appends to it, so no per-volume posture can be decided over it",
                        vol.path.display()
                    ),
                ));
            }
        }
    }
    Ok(readings)
}

/// Rung 3 — durable enrollment, a COMPLETE assignment map, and the
/// declared role verified against the slot-0 volume (D20). Its output is
/// the per-volume verdict vector.
fn rung_3_assignment_and_enrollment(
    req: &SetAdmissionRequest,
    posture: Posture,
    readings: &[VolumeReading<'_>],
) -> Result<(Vec<(String, VolumeMode)>, usize)> {
    for r in readings {
        match member_standing(r.set, &req.node_id) {
            MemberStanding::Writer => {}
            MemberStanding::Reader => {
                return Err(refuse(
                    3,
                    format!(
                        "the claim set on {} names this node '{}' as a READER member. A reader \
                         appends to nothing and ships nothing; a per-volume posture needs a \
                         `writer` entry on EVERY volume of the set. The offline `squeezefs \
                         volume set-owners` verb enrolls every fleet member as a writer on \
                         every volume (KD-PV-4)",
                        r.vol.path.display(),
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
                         itself. Re-run `squeezefs volume set-owners`, which enrolls every \
                         fleet member as a pid-less writer on every volume in the same \
                         bracket (KD-PV-4), then remount this node",
                        r.vol.path.display(),
                        req.node_id
                    ),
                ));
            }
        }
    }

    if readings.iter().all(|r| r.assigned_owner().is_none()) {
        return Err(refuse(
            3,
            "no volume of this set carries a per-volume ownership assignment: this is an \
             ordinary single-authority set, and the per-volume posture has no durable basis \
             over it. Assign ownership offline with `squeezefs volume set-owners <uri> \
             <vol-id>=<member-id>:<subtree-root> ...` (D19: assignment is an operator act that \
             stays put), or mount as `authority` / `co-writer`"
                .to_string(),
        ));
    }

    let mut modes = Vec::with_capacity(readings.len());
    for r in readings {
        let Some(owner) = r.assigned_owner() else {
            return Err(refuse(
                3,
                format!(
                    "metadata volume {} ({}) carries NO owner while another volume of this set \
                     does: a partial assignment map has no coherent appender story — the \
                     unassigned volume belongs to everyone and to nobody. Re-run `squeezefs \
                     volume set-owners` over the WHOLE set (it brackets the act with the \
                     owner_assign: intent marker, so an interrupted run resumes rather than \
                     leaving this shape)",
                    r.vol.path.display(),
                    r.vol.vol_id
                ),
            ));
        };
        let ours = member_id_matches(owner, &req.node_id);
        // KD-PV-12: a declared successor adopts ONLY where the D0 ladder
        // would grant the claim anyway; while the assigned owner is alive
        // (a fresh claim) the volume stays its owner's. Rung 6 re-checks
        // the standing against the substrate, so the reading here decides
        // WHICH door the volume goes through, never whether it may.
        let adopting =
            !ours && r.declares_successor(&req.node_id) && r.vol.standing != ClaimStanding::Fresh;
        let mode = if ours || adopting {
            VolumeMode::Own
        } else {
            if member_standing(r.set, owner) != MemberStanding::Writer {
                return Err(refuse(
                    3,
                    format!(
                        "metadata volume {} ({}) is assigned to '{owner}', which the set does \
                         not enroll as a writer member. An assignment naming a node the claim \
                         set does not know is an assignment nothing can honour — re-run \
                         `squeezefs volume set-owners`, which writes both halves in one bracket",
                        r.vol.path.display(),
                        r.vol.vol_id
                    ),
                ));
            }
            VolumeMode::Peer {
                // The live holder is who a verb must be shipped to; the
                // record's `owner` is the fallback until rung 6 resolves
                // it, and rung 6 refuses an unresolved holder outright.
                owner_id: r
                    .vol
                    .holder_member_id
                    .clone()
                    .unwrap_or_else(|| owner.to_string()),
                owner_endpoint: r.vol.owner_endpoint.clone().unwrap_or_default(),
            }
        };
        modes.push((r.vol.vol_id.clone(), mode));
    }

    // D20 / KD-PV-6: ino 1 homes on slot 0 and slot 0 homes on
    // `slot_to_volume[0]`, so the owner of THAT volume is the set
    // authority. The evidence must name exactly one such volume, or the
    // role cannot be verified at all.
    let hosts: Vec<usize> = readings
        .iter()
        .enumerate()
        .filter(|(_, r)| r.vol.hosts_slot_0)
        .map(|(i, _)| i)
        .collect();
    let (slot_0_idx, slot_0) = match hosts.as_slice() {
        [one] => (*one, &readings[*one]),
        others => {
            return Err(refuse(
                3,
                format!(
                    "the evidence names {} volumes as the host of slot 0, and exactly one must \
                     be (ino 1 pins to slot 0 through route_ino_width, and slot 0 is \
                     non-migratable while a multi-owner plane is armed — KD-PV-6). Without it \
                     the SET AUTHORITY is underivable, so the declared role cannot be verified",
                    others.len()
                ),
            ));
        }
    };
    let owns_slot_0 = modes
        .iter()
        .any(|(id, m)| *id == slot_0.vol.vol_id && *m == VolumeMode::Own);
    match (posture, owns_slot_0) {
        (Posture::SetAuthority, true) | (Posture::PartialAuthority, false) => {}
        (Posture::SetAuthority, false) => {
            return Err(refuse(
                3,
                format!(
                    "this mount declared `set-authority`, but volume {} ({}) — the host of slot \
                     0 — is assigned to '{}'. Under D20 the SET AUTHORITY is whoever appends to \
                     the slot-0 volume (it assigns allocation lanes, serves the S9 custody \
                     endpoint, owns the only freed-offset grace ring, coordinates maintenance \
                     and homes ino 1); the role is a declaration and this ladder decides it. \
                     Declare {MW_ROLE_ENV}=partial-authority",
                    slot_0.vol.path.display(),
                    slot_0.vol.vol_id,
                    slot_0.assigned_owner().unwrap_or("(unassigned)")
                ),
            ));
        }
        (Posture::PartialAuthority, true) => {
            return Err(refuse(
                3,
                format!(
                    "this mount declared `partial-authority`, but it is assigned volume {} ({}) \
                     — the host of slot 0 — so it IS the set authority under D20 and must say \
                     so: the set-authority posture keeps a FULL data plane (the W1 in-place \
                     patch, the ownership recovery walk, direct reclaim, the grace ring), which \
                     a partial authority latches away. Declare {MW_ROLE_ENV}=set-authority",
                    slot_0.vol.path.display(),
                    slot_0.vol.vol_id
                ),
            ));
        }
    }

    if !modes.iter().any(|(_, m)| *m == VolumeMode::Own) {
        return Err(refuse(
            3,
            format!(
                "this mount is assigned NO volume of the set: it would append to nothing and \
                 ship everything, which is exactly the CO-WRITER posture — and that one is \
                 built, measured and cheaper (it takes no per-volume open at all). Declare \
                 {MW_ROLE_ENV}=co-writer, or assign this node a volume offline with `squeezefs \
                 volume set-owners`"
            ),
        ));
    }
    Ok((modes, slot_0_idx))
}

/// Rung 4 — a LIVE authority, and the era comparison keyed on the SLOT-0
/// volume alone (D20). Terms diverge per owner by design, so a peer
/// volume's own era never enters this comparison: it is learned — or
/// refused — at runtime through `era_relearns`.
fn rung_4_live_authority(
    req: &SetAdmissionRequest,
    posture: Posture,
    readings: &[VolumeReading<'_>],
    slot_0_idx: usize,
) -> Result<()> {
    if posture == Posture::SetAuthority {
        // A set authority does not JOIN the membership plane — under D20
        // it owns it. What it must not do is arm beside a live foreign
        // owner of the same set, so the check is the provable direction
        // only (an empty `owner_claim_id` is a legacy rendezvous record
        // and proves nothing either way).
        if let Some(auth) = req.authority.as_ref() {
            if auth.live
                && !auth.owner_claim_id.is_empty()
                && !member_id_matches(&auth.owner_claim_id, &req.node_id)
            {
                return Err(refuse(
                    4,
                    format!(
                        "a LIVE membership plane for this set is owned by '{}' at {}, and this \
                         mount declared `set-authority`. Under D20 the set authority IS the \
                         membership owner, so two of them is two planes: the members would \
                         hold leases from one node while custody, lane assignment and the \
                         grace ring lived on another. Stop the other owner, or declare \
                         `partial-authority`",
                        auth.owner_claim_id, auth.endpoint
                    ),
                ));
            }
        }
        return Ok(());
    }

    let Some(auth) = req.authority.as_ref() else {
        return Err(refuse(
            4,
            "no membership plane. A partial authority that cannot be SEEN cannot be EVICTED, \
             and S6's eviction is what mints the dead epoch whose offsets enter the \
             do-not-reallocate quarantine — without it a dead partial authority's destinations \
             would be handed to a new owner. The SET AUTHORITY must arm \
             SQUEEZEFS_MEMBERSHIP_BIND (auto, or an addr:port) and publish its rendezvous \
             record"
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
    let slot_0 = &readings[slot_0_idx];
    let slot_0_term = slot_0.vol.claim.as_ref().map(|c| c.term).unwrap_or(0);
    if auth.term < slot_0_term {
        return Err(refuse(
            4,
            format!(
                "the membership authority '{}' armed in term {} but the SLOT-0 volume {} ({}) \
                 carries a `writer_claim` in term {slot_0_term} — the plane we joined belongs \
                 to an OLDER era than the set authority we can see (a failover happened, or two \
                 planes are running). Under D20 the comparison is against slot 0 alone, because \
                 each volume's era advances with ITS owner; re-arm the current set authority's \
                 membership plane",
                auth.owner_id,
                auth.term,
                slot_0.vol.path.display(),
                slot_0.vol.vol_id
            ),
        ));
    }
    // Rung-9 finding #3: the claim set is keyed on DURABLE node ids while
    // the rendezvous `id` is the incarnation uuid, so the record's own
    // claim identity is what the set is read against (legacy records —
    // empty — keep the uuid match).
    if !readings.iter().all(|r| {
        r.set.members.iter().any(|m| {
            m.identity.id == auth.owner_id
                || (!auth.owner_claim_id.is_empty() && m.identity.id == auth.owner_claim_id)
        })
    }) {
        return Err(refuse(
            4,
            format!(
                "the durable claim set does not name the membership authority '{}' (claim \
                 identity '{}') we joined as a member of this set. The plane that admits \
                 members and the set that enrolls them must be the same authority, or an \
                 unrelated plane could admit a node into a set it has no authority over",
                auth.owner_id, auth.owner_claim_id
            ),
        ));
    }
    Ok(())
}

/// Rung 5 — the DEVICE names this node a registrant of the standing WERO
/// hold (the shared checks and texts; §5.3: *"unchanged in substance"*).
fn rung_5_device_registrant(req: &SetAdmissionRequest) -> Result<&RegistrantEvidence> {
    registrant_detail(req.registrant.as_ref()).map_err(|detail| refuse(5, detail))
}

/// Rung 6 — **assignment ∧ evidence, per volume** (KD-PV-3, §5.1.1's
/// complete classification table). Disagreement refuses the mount naming
/// both the record's owner and the observed holder; nothing is ever
/// adopted on silence.
fn rung_6_ownership_coherence(
    req: &SetAdmissionRequest,
    readings: &[VolumeReading<'_>],
    modes: &[(String, VolumeMode)],
) -> Result<()> {
    for (r, (_, mode)) in readings.iter().zip(modes) {
        let owner = r.assigned_owner().unwrap_or_default();
        match mode {
            VolumeMode::Own => match r.vol.standing {
                ClaimStanding::Reclaimable => {}
                // KD-PV-12 (ii) / rev 3 Issue 24: the D0 ladder preempts a
                // TTL-stale claim where the device can fence its holder,
                // and refuses everywhere else. Admission must answer the
                // same way, or it promises an open that will refuse.
                ClaimStanding::Stale if r.vol.pr_capable => {}
                ClaimStanding::Stale => {
                    return Err(refuse(
                        6,
                        format!(
                            "metadata volume {} ({}) is assigned to this node, but it carries a \
                             TTL-stale `writer_claim` on a substrate with no NVMe reservation \
                             support: the D0 ladder refuses such a claim rather than preempting \
                             a holder the device cannot fence, so admitting here would promise \
                             an open that refuses. The remedy is the attested one — prove the \
                             holder is gone and run `squeezefs claim clear` — or use a \
                             PR-capable namespace",
                            r.vol.path.display(),
                            r.vol.vol_id
                        ),
                    ));
                }
                ClaimStanding::Fresh => {
                    return Err(refuse(
                        6,
                        format!(
                            "metadata volume {} ({}) is assigned to this node ('{}'), but a \
                             heartbeat-FRESH `writer_claim` is on it, held by {}. Assignment \
                             and evidence disagree, which is two appenders or an orphaned \
                             volume — this mount fails closed rather than racing the D0 ladder \
                             for a volume something is already appending to. Stop the other \
                             holder, or re-assign offline (`squeezefs volume get-owners` prints \
                             assignment beside evidence)",
                            r.vol.path.display(),
                            r.vol.vol_id,
                            req.node_id,
                            r.vol
                                .holder_member_id
                                .as_deref()
                                .unwrap_or("an unresolved holder")
                        ),
                    ));
                }
            },
            VolumeMode::Peer { .. } => match r.vol.standing {
                ClaimStanding::Fresh => {
                    let Some(holder) = r.vol.holder_member_id.as_deref() else {
                        return Err(refuse(
                            6,
                            format!(
                                "metadata volume {} ({}) carries a fresh `writer_claim` whose \
                                 holder could not be resolved to a durable member id (the \
                                 claim's own `id` is a per-mount uuid, not an enrollment \
                                 identity). An unresolvable holder is SILENCE, and ownership \
                                 never moves on silence: this mount would be shipping every \
                                 verb for the volume to a node it cannot name",
                                r.vol.path.display(),
                                r.vol.vol_id
                            ),
                        ));
                    };
                    if !r.assignment_names(holder) {
                        return Err(refuse(
                            6,
                            format!(
                                "metadata volume {} ({}) is assigned to '{owner}' (successors: \
                                 {}), but the live claim is held by '{holder}'. Assignment ∧ \
                                 evidence DISAGREE, so the ownership map fails closed: shipping \
                                 this volume's verbs to a node the record does not entitle \
                                 would make it an appender nobody assigned. Re-assign offline, \
                                 or stop the holder",
                                r.vol.path.display(),
                                r.vol.vol_id,
                                if r.set.successors.is_empty() {
                                    "none".to_string()
                                } else {
                                    r.set.successors.join(", ")
                                }
                            ),
                        ));
                    }
                }
                ClaimStanding::Reclaimable => {
                    return Err(refuse(
                        6,
                        format!(
                            "metadata volume {} ({}) is assigned to peer '{owner}', but NOTHING \
                             claims it: that owner is dead, was never started, or the \
                             assignment is stale. This mount is not the assignee, so it must \
                             not take the claim — and it must not serve a set with a volume no \
                             node appends to. Start the owner, declare a successor and \
                             re-assign offline, or clear the assignment; `squeezefs volume \
                             get-owners` prints assignment beside evidence",
                            r.vol.path.display(),
                            r.vol.vol_id
                        ),
                    ));
                }
                ClaimStanding::Stale => {
                    return Err(refuse(
                        6,
                        format!(
                            "metadata volume {} ({}) is assigned to peer '{owner}', whose \
                             `writer_claim` is TTL-stale. A partial writer must never preempt a \
                             peer's claim: preempting would make it the appender of a volume it \
                             is not assigned, which is the one thing per-volume admission \
                             exists to prevent. Start that owner, or re-assign the volume \
                             offline (`squeezefs volume get-owners`)",
                            r.vol.path.display(),
                            r.vol.vol_id
                        ),
                    ));
                }
            },
        }
    }
    Ok(())
}

/// Rung 7 — **the freeze precondition** (§5.9.2, KD-PV-8). Every
/// peer-owned volume's projected `claim_set` must show that volume's own
/// assignment: a monotone, checkpoint-consistent projection that shows the
/// assignment record shows every commit that preceded it on that volume —
/// including every cross-owner dentry that will ever exist there, because
/// M1 + M2 + the cross-owner slot-migration refusal freeze that set at the
/// assignment instant. One durable, observable fact; no barrier verb, no
/// ack ledger, no timeout.
fn rung_7_freeze_precondition(
    readings: &[VolumeReading<'_>],
    modes: &[(String, VolumeMode)],
) -> Result<()> {
    for (r, (_, mode)) in readings.iter().zip(modes) {
        if !matches!(mode, VolumeMode::Peer { .. }) {
            continue;
        }
        let assigned = r.assigned_owner().unwrap_or_default();
        let Some(projected) = r.vol.projected_claim_set.as_ref() else {
            return Err(refuse(
                7,
                format!(
                    "no projection of metadata volume {} ({}) could be read, so nothing \
                     certifies that this mount's view of a peer-owned volume already contains \
                     its assignment. The online inode plane rests on that fact (§5.9.2): a view \
                     that predates the assignment also predates cross-owner dentries it would \
                     then read as unreferenced",
                    r.vol.path.display(),
                    r.vol.vol_id
                ),
            ));
        };
        match projected.owner.as_deref() {
            Some(seen) if seen == assigned => {}
            Some(seen) => {
                return Err(refuse(
                    7,
                    format!(
                        "the projection of peer-owned volume {} ({}) shows owner '{seen}' while \
                         the volume's durable record assigns '{assigned}': this mount's view \
                         PREDATES the current assignment, so the cross-owner reference set it \
                         would read is not yet frozen. Wait for the next checkpoint to be \
                         polled, or re-run the assignment (the verb checkpoints every volume \
                         before exiting, which is what makes this precondition self-certifying)",
                        r.vol.path.display(),
                        r.vol.vol_id
                    ),
                ));
            }
            None => {
                return Err(refuse(
                    7,
                    format!(
                        "the projection of peer-owned volume {} ({}) carries NO owner while the \
                         volume's durable record assigns '{assigned}' — the view predates the \
                         assignment commit itself. Refusing: the freeze precondition is what \
                         lets an online pass read a peer's records at all",
                        r.vol.path.display(),
                        r.vol.vol_id
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// **Decide.** Runs the seven rungs in order and returns the per-volume
/// admission, or the first rung's refusal naming what is missing, the
/// volume by durable id, and the remedy.
///
/// Pure: it reads no environment, opens nothing and mutates nothing. PR 4's
/// gather is what turns a mount into a [`SetAdmissionRequest`].
pub fn classify_set_admission(req: &SetAdmissionRequest) -> Result<SetAdmission> {
    let (posture, endpoint) = rung_1_declaration(req)?;
    let readings = rung_2_engaged_claim_set(req)?;
    let (modes, slot_0_idx) = rung_3_assignment_and_enrollment(req, posture, &readings)?;
    rung_4_live_authority(req, posture, &readings, slot_0_idx)?;
    let dev = rung_5_device_registrant(req)?;
    rung_6_ownership_coherence(req, &readings, &modes)?;
    rung_7_freeze_precondition(&readings, &modes)?;

    let slot_0_vol_id = readings[slot_0_idx].vol.vol_id.clone();
    let admission = SetAdmission {
        node_id: req.node_id.clone(),
        role: req.role,
        set_authority_endpoint: endpoint.to_string(),
        pr_key: dev.key,
        volumes: req.volumes.iter().map(|v| v.path.clone()).collect(),
        slot_0_vol_id,
        modes,
    };
    let owned = admission
        .modes
        .iter()
        .filter(|(_, m)| *m == VolumeMode::Own)
        .count();
    log::warn!(
        "PER-VOLUME CLAIM ADMISSION ({}) over {} volume(s): node '{}' appends to {} and ships \
         {} to their owners; set authority = {} (slot-0 volume {}), device registrant key {:#x}. \
         Every peer-owned volume is opened with a released LOCK_SH probe only — no PR acquire, \
         no writer_claim, no checkpoint task — and its metadata mutations SHIP",
        admission.role.as_str(),
        admission.modes.len(),
        admission.node_id,
        owned,
        admission.modes.len() - owned,
        if admission.is_set_authority() {
            "this mount".to_string()
        } else {
            format!("a peer at {}", admission.set_authority_endpoint)
        },
        admission.slot_0_vol_id,
        admission.pr_key,
    );
    Ok(admission)
}
