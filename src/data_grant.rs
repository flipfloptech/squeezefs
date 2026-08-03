//! DLM **stage S9** — **remote write custody**: the protocol that lets a
//! co-writer hold a file's (or a byte range's) write custody from another
//! node's authority and then **DMA directly** to the shared device
//! (`docs/pre-rc-engineering-spec.md` §6.9 S9 row — *"custody tokens,
//! remote clients DMA directly"*; §6.7 "Recovery" and "On external
//! consensus"; execution-plan rulings **D8**, **D9**, **D11**).
//!
//! # The one sentence that shapes everything
//!
//! **Only custody travels; data never does.** A grant is a decision plus a
//! fencing token — tens of bytes on the cluster wire — and the bytes it
//! authorizes go straight from the co-writer's `NvmeBlockDev` to the shared
//! namespace. Funnelling data through the owner would put a fabric hop and
//! an owner copy on the write path, which is the opposite of every measured
//! lesson in this tree.
//!
//! # What this stage adds, and what it deliberately reuses
//!
//! Nothing here is a new mechanism where an existing one answers:
//!
//! | Question | Answered by | Not re-invented here |
//! |---|---|---|
//! | may these two spans be held at once? | [`crate::dlm`]'s S11 interval algebra, via a lease the OWNER holds on the client's behalf | no second range rule, no second conflict matrix |
//! | what is this object's generation? | the owner's own fencing mint, carried in the grant and adopted with [`crate::dlm::adopt_remote_grant`] | no client-side mint |
//! | when does custody die? | S6's [`LeaseClocks`] — the owner's TTL and the member's strictly earlier `T_self` | no second clock law |
//! | how does a fenced writer stop? | S7: the client's own [`crate::data_custody`] generation, and the DEVICE under the shared WERO hold | no third fence door |
//! | what happens to a dead epoch's blocks? | S7's `declare_dead_epoch` → quarantine → **drain proof** → release | no second quarantine |
//!
//! # The state machine
//!
//! ```text
//!            JOIN                    ACQUIRE
//!   (none) ────────► LeaseHeld ──────────────► GrantHeld ─┐
//!                      │  ▲                        │      │ RELEASE
//!                      │  └──── RENEW ◄────────────┘      │
//!                      │                                  ▼
//!                      │                             (retired)
//!                      │
//!                      │ revoke / TTL expiry (owner)          T_self (client)
//!                      ▼                                      ▼
//!                 DeadCustody ── quarantine ── DrainProof ── released
//!                      │                                      ▲
//!                      └── the client advances its custody generation
//!                          (never poisons) or, past T_self, self-fences
//! ```
//!
//! Every transition is one place:
//!
//! * **JOIN** mints the client's lease epoch. The *epoch a grant carries*
//!   is `compose(owner_term, lease_epoch)` — per **lease**, not per grant,
//!   because a client holding three grants has ONE custody, and it is
//!   custody that moves. The client adopts it as its
//!   [`crate::data_custody`] generation floor and thereafter authorizes
//!   every DMA under `current_epoch()`.
//! * **ACQUIRE** takes the local lease on the owner (whole-file or one
//!   span) and returns its fencing token. A conflict is a **refusal**
//!   inside the caller's own wait budget — never a wider grant, never a
//!   silent whole-file promotion.
//! * **RENEW** is the heartbeat and the only pull-based revocation channel:
//!   it carries the client's **in-flight destination offsets** (the job
//!   wire's pre-allocated-destination law — the authority must know which
//!   offsets a dead epoch could still be writing) and it returns which of
//!   the client's grants have died.
//! * **REVOKE / EXPIRY** are the owner's act: the client lease leaves the
//!   table, its grants' local leases drop (so the bytes become grantable),
//!   ONE dead epoch is minted for the whole client, and its declared
//!   in-flight offsets enter the S7 quarantine.
//! * **RECLAIM** is the failover re-assertion the successor's grace window
//!   admits (§6.7 "Recovery"), answering with **fresh-era** tokens.
//!
//! # Why revocation is pull-based, stated plainly
//!
//! There is **no owner→client callback channel** here. The client learns a
//! revocation at its next renewal, and the bound on that discovery is
//! `T_self` — at which point it fail-stops itself *before* the owner's TTL
//! lets those bytes be granted elsewhere (S6's asymmetry). A push
//! backchannel would shorten the window, and it is what spec §6.6's
//! "server→client revocation" and risk **R5**'s thrash valve need; it is
//! **not built**, and the honest consequence is that a revoked client keeps
//! believing it holds custody for up to one renewal cadence — which is
//! safe, because the device (WERO) and its own `T_self` are what stop it,
//! not its belief.

use crate::cluster_wire::{
    RpcAsyncService, RpcClient, RpcRequest, RpcResponse, RPC_OK, RPC_UNKNOWN_VERB,
};
use crate::data_custody::DeadEpoch;
use crate::dlm::{LocalLockManager, LockLease, LockMode};
use crate::error::{Result, SqueezefsError};
use crate::membership::{LeaseClock, LeaseClocks, MemberRole, MemberSession, SelfFence};
use arc_swap::ArcSwapOption;
use bincode::Options as _;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// The wire: its own verb block, its own schema
// ---------------------------------------------------------------------------

/// The custody vocabulary's schema. Independent of the transport's
/// (`CLUSTER_WIRE_SCHEMA`) and of S8's metadata vocabulary: a mismatch is
/// refused loud, because a custody-bearing protocol has no safe guess.
///
/// **2 since the lease began carrying the co-writer's allocation lane**
/// ([`LeaseFrame::writer_lane`]). A peer that speaks 1 would adopt a lease
/// with no lane and then either refuse every allocation or — far worse —
/// mint DENSE offsets across every peer's residue class, so the mismatch
/// must be a loud refusal at the join rather than a field with a default.
pub const CUSTODY_SCHEMA: u32 = 2;

/// First verb of S9's block. S3 reserved 0 for its ping, S8's metadata
/// vocabulary took 16/17, S6's membership owns `0x0100..=0x01FF`; custody
/// takes `0x0200..=0x02FF` so the four vocabularies grow without
/// collision.
pub const VERB_CUSTODY_BASE: u16 = 0x0200;
/// Last verb of S9's block.
pub const VERB_CUSTODY_LAST: u16 = 0x02FF;
/// Join the custody plane: mint this client's lease epoch.
pub const VERB_CUSTODY_JOIN: u16 = VERB_CUSTODY_BASE;
/// Acquire write custody of a file or one byte range.
pub const VERB_CUSTODY_ACQUIRE: u16 = VERB_CUSTODY_BASE + 1;
/// Renew the client lease, carrying its in-flight destinations.
pub const VERB_CUSTODY_RENEW: u16 = VERB_CUSTODY_BASE + 2;
/// Release named grants.
pub const VERB_CUSTODY_RELEASE: u16 = VERB_CUSTODY_BASE + 3;
/// Re-assert custody inside a successor's grace window.
pub const VERB_CUSTODY_RECLAIM: u16 = VERB_CUSTODY_BASE + 4;

/// Status: the call succeeded.
pub const CUSTODY_OK: u16 = RPC_OK;
/// Status: an incompatible grant covers the requested bytes.
pub const CUSTODY_CONFLICT: u16 = 0x41;
/// Status: the presented lease is not custody (revoked, swept past its
/// TTL, or minted by a previous authority) — self-fence and re-join.
pub const CUSTODY_UNKNOWN_LEASE: u16 = 0x42;
/// Status: the authority is inside its failover grace window and the frame
/// carried a fresh acquire.
pub const CUSTODY_IN_GRACE: u16 = 0x43;
/// Status: vocabulary schema mismatch.
pub const CUSTODY_SCHEMA_MISMATCH: u16 = 0x44;
/// Status: undecodable body (bounded, refused loud).
pub const CUSTODY_MALFORMED: u16 = 0x45;

/// A client's join (or re-join after losing its lease view).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinFrame {
    pub schema: u32,
    /// The co-writer's cluster identity.
    pub client: String,
    /// Its NVMe registrant key under the shared WERO hold (`0` = none) —
    /// what makes a PREEMPT of this client possible, i.e. what makes a
    /// drain proof obtainable.
    pub pr_key: u64,
    /// `Some` ⇒ this is a re-join carrying the lease epoch it held.
    pub prior_epoch: Option<u64>,
}

/// The client lease: everything the co-writer needs to compute its own
/// (strictly earlier) deadline, plus the authorization epoch its grants
/// ride.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseFrame {
    pub schema: u32,
    /// The client's lease epoch — monotone per authority, never reused.
    pub epoch: u64,
    /// The authority's durable writer era.
    pub term: u64,
    /// The custody **epoch** every DMA under this lease is authorized
    /// under: `compose(term, epoch)`.
    pub custody_epoch: u64,
    pub t_owner_ms: u64,
    pub skew_max_ms: u64,
    pub d_purge_ms: u64,
    pub renew_ms: u64,
    /// The authority's monotonic instant of the grant — diagnostics only.
    /// A member NEVER anchors on a foreign clock (S6's law).
    pub granted_at_owner_ms: u64,
    /// **This member's data-plane allocation lane** (DLM S9 blocker #3's
    /// admission — `docs/design-mw-data-alloc-partition.md` §9 item 1): the
    /// residue class `writer_lane` of [`Self::writers`] that this mount, and
    /// only this mount, may mint fresh block indices from.
    ///
    /// Minted by the AUTHORITY from the durable claim set
    /// ([`crate::alloc_lane_grant::LaneAssignment`]) — never chosen, guessed
    /// or configured by the joining node, because two co-writers choosing
    /// their own lanes is exactly the collision the partition exists to
    /// prevent. `(0, 1)` means SOLO, i.e. *no partition at all*, which is
    /// what every authority with no enrolled co-writer answers and what
    /// keeps single-writer allocation byte-identical.
    pub writer_lane: u16,
    /// The partition width every member of this era agrees on. It changes
    /// only when the authority re-arms (a new era), because a live writer's
    /// residue class cannot be redefined under offsets it has already minted.
    pub writers: u16,
}

impl LeaseFrame {
    fn to_membership_grant(self) -> crate::membership::Grant {
        crate::membership::Grant {
            epoch: self.epoch,
            term: self.term,
            t_owner_ms: self.t_owner_ms,
            skew_max_ms: self.skew_max_ms,
            d_purge_ms: self.d_purge_ms,
            renew_ms: self.renew_ms,
            granted_at_owner_ms: self.granted_at_owner_ms,
        }
    }
}

/// An acquire request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquireFrame {
    pub schema: u32,
    pub client: String,
    /// The lease epoch the client believes it holds.
    pub lease_epoch: u64,
    pub ino: u64,
    /// `None` = whole-inode custody; `Some((start,end))` = `[start,end)`
    /// with **end exclusive**.
    pub span: Option<(u64, u64)>,
    /// `true` = CW (concurrent write); `false` = EX.
    pub concurrent_write: bool,
    /// The caller's own wait budget, ms — clamped by the authority to one
    /// renewal cadence (a client that cannot get custody within a cadence
    /// must be TOLD, not held on a service lane).
    pub wait_ms: u64,
}

/// One live grant as the client sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRecord {
    pub schema: u32,
    /// The authority's id for this grant (the release/renew key).
    pub grant_id: u64,
    pub ino: u64,
    pub span: Option<(u64, u64)>,
    /// The object's fencing generation, minted by the authority.
    pub token: u64,
    /// The authority's era.
    pub term: u64,
    /// The custody epoch of the lease this grant rides.
    pub custody_epoch: u64,
}

/// A renewal: the heartbeat plus the client's in-flight destinations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewFrame {
    pub schema: u32,
    pub client: String,
    pub lease_epoch: u64,
    /// Device offsets this client may still be writing — the cohort a
    /// dead epoch's quarantine is keyed on.
    pub inflight: Vec<u64>,
}

/// A renewal's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewReplyFrame {
    pub schema: u32,
    pub lease: LeaseFrame,
    /// Grants of this client the authority no longer holds (revoked while
    /// the client was away). The pull-based revocation channel.
    pub dead_grants: Vec<u64>,
}

/// A release of named grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseFrame {
    pub schema: u32,
    pub client: String,
    pub lease_epoch: u64,
    pub grant_ids: Vec<u64>,
}

/// A grace-window re-assertion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimFrame {
    pub schema: u32,
    pub client: String,
    pub lease_epoch: u64,
    /// The objects this client held before the failover.
    pub inos: Vec<u64>,
}

/// The reclaim's answer: fresh-era grants for what was admitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimReplyFrame {
    pub schema: u32,
    pub grants: Vec<GrantRecord>,
}

fn decode_limit() -> u64 {
    u64::from(crate::cluster_wire::CONTROL_MAX_FRAME_BYTES)
}

fn encode<T: Serialize>(value: &T, what: &str) -> Result<Vec<u8>> {
    let body = bincode::DefaultOptions::new()
        .serialize(value)
        .map_err(|e| SqueezefsError::InvalidOperation(format!("S9 {what} encode failed: {e}")))?;
    if body.len() as u64 > decode_limit() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "S9 {what} of {} B exceeds the cluster wire's CONTROL class cap ({} B)",
            body.len(),
            decode_limit()
        )));
    }
    Ok(body)
}

/// Decode an **untrusted** frame body: bounded, so a lying in-body length
/// cannot make the decoder allocate (the `cluster_wire` codec discipline).
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T> {
    bincode::DefaultOptions::new()
        .with_limit(decode_limit())
        .deserialize(bytes)
        .map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "S9 {what}: undecodable frame body ({} B): {e}",
                bytes.len()
            ))
        })
}

// ---------------------------------------------------------------------------
// The ledger (spec §6.9's named counters, this stage's face)
// ---------------------------------------------------------------------------

static GRANTS: AtomicU64 = AtomicU64::new(0);
static RENEWALS: AtomicU64 = AtomicU64::new(0);
static RELEASES: AtomicU64 = AtomicU64::new(0);
static CONFLICTS: AtomicU64 = AtomicU64::new(0);
static REVOKES: AtomicU64 = AtomicU64::new(0);
static EXPIRIES: AtomicU64 = AtomicU64::new(0);
static RECLAIMS: AtomicU64 = AtomicU64::new(0);
static GRACE_CONFLICTS: AtomicU64 = AtomicU64::new(0);
static UNKNOWN_LEASES: AtomicU64 = AtomicU64::new(0);
static RPCS: AtomicU64 = AtomicU64::new(0);
static QUARANTINED: AtomicU64 = AtomicU64::new(0);
static PROOFS: AtomicU64 = AtomicU64::new(0);
static SELF_FENCES: AtomicU64 = AtomicU64::new(0);

/// The client-side view of the custody ledger (the owner's own per-instance
/// counters are [`WriteCustodyOwner::stats`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientStats {
    /// Grants this node ACQUIRED from an authority.
    pub grants: u64,
    /// Renewals it completed.
    pub renewals: u64,
    /// Grants it released.
    pub releases: u64,
    /// Acquires refused for conflicting custody.
    pub conflicts: u64,
    /// Renewals answered "this lease is not custody" — the pull-based
    /// revocation channel firing.
    pub unknown_leases: u64,
    /// Custody round trips (the S4 `dlm_rpcs` family's S9 face).
    pub rpcs: u64,
    /// Times this node fail-stopped its own custody at `T_self`.
    pub self_fences: u64,
}

/// Read the process's custody ledger.
pub fn stats() -> ClientStats {
    ClientStats {
        grants: GRANTS.load(Ordering::Relaxed),
        renewals: RENEWALS.load(Ordering::Relaxed),
        releases: RELEASES.load(Ordering::Relaxed),
        conflicts: CONFLICTS.load(Ordering::Relaxed),
        unknown_leases: UNKNOWN_LEASES.load(Ordering::Relaxed),
        rpcs: RPCS.load(Ordering::Relaxed),
        self_fences: SELF_FENCES.load(Ordering::Relaxed),
    }
}

/// The `data_custody` stats-inode object: the remote-custody plane's
/// ledger, `0` in every field on a single-writer mount BY CONSTRUCTION
/// (nothing is armed, so nothing grants or ships).
pub fn stats_json() -> serde_json::Value {
    let c = stats();
    serde_json::json!({
        "mode": mode(),
        "dlm_custody_grants": c.grants,
        "dlm_custody_renewals": c.renewals,
        "dlm_custody_releases": c.releases,
        "dlm_custody_conflicts": c.conflicts,
        "dlm_custody_unknown_leases": c.unknown_leases,
        "dlm_custody_self_fences": c.self_fences,
        "dlm_rpcs_custody": c.rpcs,
        "dlm_revokes_issued": REVOKES.load(Ordering::Relaxed),
        "dlm_revokes_expired": EXPIRIES.load(Ordering::Relaxed),
        "dlm_custody_reclaims": RECLAIMS.load(Ordering::Relaxed),
        "dlm_custody_grace_conflicts": GRACE_CONFLICTS.load(Ordering::Relaxed),
        "dlm_custody_quarantined_offsets": QUARANTINED.load(Ordering::Relaxed),
        "dlm_custody_drain_proofs": PROOFS.load(Ordering::Relaxed),
        "dlm_custody_held": OWNER
            .load()
            .as_ref()
            .map(|o| o.held() as u64)
            .unwrap_or(0),
        // VAL-7a's census law applied to custody: the per-grant census
        // names peer identities, inos and byte ranges, so it is OPT-IN
        // behind the same knob as the key census; the COUNT above always
        // exports and is what tooling keys on.
        "dlm_custody_grant_census": custody_census(),
    })
}

/// The opt-in per-grant census (`SQUEEZEFS_STATS_KEY_CENSUS=1`), else
/// `null` — VAL-7a's law: a census that names identities, inos and byte
/// ranges is a debugging surface, and the count is the always-on gauge.
fn custody_census() -> serde_json::Value {
    if !crate::env_knobs::bool_knob("SQUEEZEFS_STATS_KEY_CENSUS", false) {
        return serde_json::Value::Null;
    }
    let Some(owner) = OWNER.load_full() else {
        return serde_json::Value::Null;
    };
    serde_json::Value::Array(
        owner
            .grants_snapshot()
            .into_iter()
            .map(|g| {
                serde_json::json!({
                    "grant_id": g.grant_id,
                    "client": g.client,
                    "lease_epoch": g.lease_epoch,
                    "ino": g.ino,
                    "span": g.span.map(|(s, e)| vec![s, e]),
                    "token": g.token,
                })
            })
            .collect(),
    )
}

/// `off` (nothing armed — the shipped default), `authority` (this mount
/// grants custody to peers), `co-writer` (it holds custody from a peer), or
/// `both`.
pub fn mode() -> &'static str {
    match (OWNER.load().is_some(), CLIENT.load().is_some()) {
        (false, false) => "off",
        (true, false) => "authority",
        (false, true) => "co-writer",
        (true, true) => "both",
    }
}

// ---------------------------------------------------------------------------
// Phase attribution (the deferred S9 fan-out row needs terms, not a number)
// ---------------------------------------------------------------------------

/// Client-side phases of one custody operation (`dlm_custody_phase_ns`).
///
/// Same always-on cost contract as `meta_ship_phase_ns` /
/// `publish_phase_ns`: one `Instant::now()` and one relaxed `fetch_add` per
/// boundary. `arbitrate` is the term §6.5 item 1's arithmetic is about — it
/// is the authority's wait, and it is what a contended shared-file row will
/// be made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum CustodyPhase {
    /// Encode + the authenticated round trip.
    Rtt = 0,
    /// The owner-side arbitration (its local acquire, including any wait).
    Arbitrate = 1,
    /// Client-side adoption: era, floor, local custody record.
    Adopt = 2,
    /// The renewal round trip.
    Renew = 3,
}

const CUSTODY_PHASES: usize = 4;
const CUSTODY_PHASE_NAMES: [&str; CUSTODY_PHASES] = ["rtt", "arbitrate", "adopt", "renew"];

static PHASES: Lazy<[crate::fuse_client::LatencyHistogram; CUSTODY_PHASES]> =
    Lazy::new(|| std::array::from_fn(|_| crate::fuse_client::LatencyHistogram::default()));

/// Record a custody phase span started at `t0`.
#[inline]
pub fn phase_record(phase: CustodyPhase, t0: Instant) {
    PHASES[phase as usize].record(t0.elapsed());
}

/// `dlm_custody_phase_ns` — the decomposition the deferred fan-out row
/// needs.
pub fn phase_json() -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (i, name) in CUSTODY_PHASE_NAMES.iter().enumerate() {
        out.insert((*name).to_string(), PHASES[i].to_json());
    }
    serde_json::Value::Object(out)
}

// ---------------------------------------------------------------------------
// The quarantine sink and the drain proof
// ---------------------------------------------------------------------------

/// Where a dead epoch's offsets go — S7's allocator quarantine, behind a
/// trait so the custody authority never needs to know how many data volumes
/// a mount has (the mount arm supplies the backend router's view; a test
/// supplies one allocator; a device-less authority supplies `None`).
pub trait CustodyQuarantine: Send + Sync + std::fmt::Debug {
    /// Admit `offsets` to `epoch`'s do-not-reallocate cohort. Returns the
    /// newly admitted count.
    fn quarantine(&self, offsets: &[u64], epoch: DeadEpoch) -> usize;
    /// Release `epoch`'s whole cohort — reachable only with a
    /// [`DrainProof`].
    fn release(&self, epoch: DeadEpoch) -> usize;
}

/// **Evidence that a dead epoch can no longer submit DMA.**
///
/// S7's law is *"release requires a drain proof … a release without a proof
/// is a correctness bug"*. Making the proof a TYPE whose constructors
/// demand evidence is how that stops being a comment: there is no
/// `DrainProof::assumed()`, and [`Self::preempt_landed`] refuses to exist
/// for a preempt that landed on zero namespaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainProof {
    /// A WERO preempt of the victim's registrant key landed on this many
    /// namespaces, so its resumed DMA is rejected by the DEVICE (§5.1.6
    /// rung 2 — the strongest proof available).
    PreemptLanded {
        /// Namespaces where the preempt landed (≥ 1 by construction).
        namespaces: u64,
    },
    /// Recovery proved the holder dead (a provably-dead pid on this host,
    /// or an operator attestation — the `squeezefs claim clear` class).
    /// Weaker than a preempt and deliberately named differently, so an
    /// audit can tell which proof a release stood on.
    ProvenDead {
        /// What was proven, and how.
        detail: String,
    },
}

impl DrainProof {
    /// A landed WERO preempt. `None` when it landed nowhere — a preempt
    /// that touched no namespace proves nothing.
    pub fn preempt_landed(namespaces: u64) -> Option<Self> {
        (namespaces > 0).then_some(Self::PreemptLanded { namespaces })
    }

    /// An attested proof of death (the detection-grade path).
    pub fn proven_dead(detail: &str) -> Self {
        Self::ProvenDead {
            detail: detail.to_string(),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::PreemptLanded { namespaces } => {
                format!("WERO preempt landed on {namespaces} namespace(s)")
            }
            Self::ProvenDead { detail } => format!("proven dead: {detail}"),
        }
    }
}

/// A client's custody after it died: the cohort a quarantine is keyed on.
///
/// ONE per client epoch, not one per grant — the epoch belongs to the
/// client's lease, and its offsets are what a successor must not reallocate.
#[derive(Debug, Clone)]
pub struct DeadCustody {
    /// The dead client's identity.
    pub client: String,
    /// The lease epoch that died.
    pub lease_epoch: u64,
    /// The S7 cohort id its offsets are quarantined under.
    pub epoch: DeadEpoch,
    /// Its grants at the moment of death (diagnostics / audit).
    pub grants: Vec<u64>,
    /// The in-flight destination offsets it last declared — the cohort.
    pub offsets: Vec<u64>,
    /// Its NVMe registrant key, for the preempt that becomes the proof
    /// (`0` = none, i.e. only an attested proof is available).
    pub pr_key: u64,
    /// Why it died.
    pub reason: String,
}

// ---------------------------------------------------------------------------
// The authority
// ---------------------------------------------------------------------------

/// Per-instance counters of one authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OwnerStats {
    /// Grants issued.
    pub granted: u64,
    /// Grants retired by a client release.
    pub released: u64,
    /// Acquires refused for conflicting custody.
    pub conflicts: u64,
    /// Renewals served.
    pub renewals: u64,
    /// Clients revoked by an operator/recovery act.
    pub revokes: u64,
    /// Clients swept past the authority's TTL.
    pub expiries: u64,
    /// Re-assertions admitted.
    pub reclaims: u64,
    /// Fresh acquires refused inside a grace window (**must stay 0** on a
    /// healthy failover, where clients reclaim and wait).
    pub grace_conflicts: u64,
    /// Renewals answered "not custody".
    pub unknown_leases: u64,
    /// Live grants.
    pub held: u64,
    /// Live client leases.
    pub clients: u64,
}

struct ClientLease {
    epoch: u64,
    pr_key: u64,
    renewed_ms: u64,
    deadline_ms: u64,
    inflight: Vec<u64>,
    /// Grants whose custody died while this client was away — drained by
    /// its next renewal (the pull-based revocation channel).
    dead_grants: Vec<u64>,
}

struct GrantState {
    client: String,
    lease_epoch: u64,
    ino: u64,
    span: Option<(u64, u64)>,
    /// The authority's OWN lease, held on the client's behalf. Dropping it
    /// is what makes the bytes grantable again — so a revoke is
    /// structurally a drop, not a bookkeeping edit. It is also the grant's
    /// TOKEN of record: [`WriteCustodyOwner::grants_snapshot`] reads it from
    /// here rather than from a copy, so an audit can never disagree with the
    /// custody that is actually held.
    lease: LockLease,
}

/// One live grant as an audit sees it — the custody half of
/// `squeezefs clients`: which peer holds which bytes of which object, under
/// which token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantAudit {
    pub grant_id: u64,
    pub client: String,
    pub lease_epoch: u64,
    pub ino: u64,
    pub span: Option<(u64, u64)>,
    pub token: u64,
}

struct Grace {
    until_ms: u64,
    expected: std::collections::BTreeSet<String>,
}

/// The **write-custody authority**: the node that arbitrates who may write
/// which bytes, using its own local lock manager as the arbiter.
///
/// It holds no data path of its own: a grant is a decision, and the bytes
/// go straight from the co-writer to the device.
pub struct WriteCustodyOwner {
    id: String,
    term: u64,
    clocks: LeaseClocks,
    clock: LeaseClock,
    /// The arbiter. Deliberately the **local** manager, never the homing
    /// [`crate::dlm_slot::SlotLockManager`]: for an object it owns this
    /// node IS the authority, and routing its own acquire through the
    /// homing gate would bounce it off the client path it is serving (the
    /// `meta_ship::owner_authority_token` precedent).
    arbiter: LocalLockManager,
    /// DLM **S9** blocker #3's admission: the data-plane allocation lane map
    /// this era runs under ([`crate::alloc_lane_grant::LaneAssignment`],
    /// derived from the durable claim set by the multi-writer arm). `None`
    /// on an authority with no enrolled co-writer — and then every lease
    /// says SOLO, which installs no partition anywhere.
    lanes: ArcSwapOption<crate::alloc_lane_grant::LaneAssignment>,
    clients: scc::HashMap<String, ClientLease>,
    grants: scc::HashMap<u64, GrantState>,
    next_epoch: AtomicU64,
    next_grant: AtomicU64,
    grace: parking_lot::Mutex<Option<Grace>>,
    quarantine: Option<Arc<dyn CustodyQuarantine>>,
    granted: AtomicU64,
    released: AtomicU64,
    conflicts: AtomicU64,
    renewals: AtomicU64,
    revokes: AtomicU64,
    expiries: AtomicU64,
    reclaims: AtomicU64,
    grace_conflicts: AtomicU64,
    unknown_leases: AtomicU64,
}

impl std::fmt::Debug for WriteCustodyOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteCustodyOwner")
            .field("id", &self.id)
            .field("term", &self.term)
            .field("clients", &self.clients.len())
            .field("grants", &self.grants.len())
            .field("in_grace", &self.in_grace())
            .finish_non_exhaustive()
    }
}

impl WriteCustodyOwner {
    /// Arm the authority in `term`.
    ///
    /// **Refuses a non-greater era than the process's durable term** for
    /// the same reason [`crate::membership::MembershipOwner::arm`] does
    /// (§6.7 "Recovery"): a successor bumps `term` durably BEFORE arming,
    /// so every token and every DMA authorization from the predecessor's
    /// era is stale by construction. An authority that armed on an equal
    /// era could hand out custody indistinguishable from the dead one's.
    ///
    /// `quarantine` is where a dead epoch's offsets go. `None` is honest
    /// for an authority with no data plane of its own (a metadata-only
    /// owner), and it is then loud at every death: an unquarantined dead
    /// epoch is a reallocation hazard the operator must know about.
    pub fn arm(
        id: &str,
        term: u64,
        prior_term: u64,
        clocks: LeaseClocks,
        clock: LeaseClock,
        quarantine: Option<Arc<dyn CustodyQuarantine>>,
    ) -> Result<Arc<Self>> {
        if term <= prior_term && !(term == 0 && prior_term == 0) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "custody authority '{id}' refuses to arm in era {term}: the predecessor's \
                 durable era is {prior_term}, and §6.7 recovery requires the successor to bump \
                 the term DURABLY before arming (an equal era makes its grants \
                 indistinguishable from the dead authority's, so a zombie's token would still \
                 dominate). Arm after the D0 gate's claim barrier, which is what publishes the \
                 new era"
            )));
        }
        let durable = crate::dlm::durable_term();
        if term < durable {
            return Err(SqueezefsError::InvalidOperation(format!(
                "custody authority '{id}' refuses to arm in era {term}: this process's durable \
                 writer term is {durable}, and §6.7 recovery requires the successor's era to \
                 dominate every predecessor's (an earlier era would mint tokens a live writer \
                 already dominates). Arm after the D0 gate's claim barrier, which is what \
                 publishes the era"
            )));
        }
        if term > crate::dlm::TERM_MAX {
            return Err(SqueezefsError::InvalidOperation(format!(
                "custody authority '{id}' refuses to arm in era {term}: the composed-token \
                 term field is {} bits ({} max) and a carry would alias a retired era's \
                 tokens with a live one's",
                64 - crate::dlm::GRANT_SEQ_BITS,
                crate::dlm::TERM_MAX
            )));
        }
        log::info!(
            "write-custody authority '{id}' armed in era {term} (superseding {prior_term}): client lease TTL {:?}, member \
             deadline T_self {:?}, renewal cadence {:?}, grace {:?}. Only CUSTODY travels here \
             — a co-writer's bytes go straight to the device (DLM S9, spec §6.9)",
            clocks.t_owner,
            clocks.t_self,
            clocks.renew_interval,
            clocks.grace,
        );
        Ok(Arc::new(Self {
            id: id.to_string(),
            term,
            clocks,
            clock,
            arbiter: LocalLockManager::new()?,
            lanes: ArcSwapOption::empty(),
            clients: scc::HashMap::new(),
            grants: scc::HashMap::new(),
            next_epoch: AtomicU64::new(1),
            next_grant: AtomicU64::new(1),
            grace: parking_lot::Mutex::new(None),
            quarantine,
            granted: AtomicU64::new(0),
            released: AtomicU64::new(0),
            conflicts: AtomicU64::new(0),
            renewals: AtomicU64::new(0),
            revokes: AtomicU64::new(0),
            expiries: AtomicU64::new(0),
            reclaims: AtomicU64::new(0),
            grace_conflicts: AtomicU64::new(0),
            unknown_leases: AtomicU64::new(0),
        }))
    }

    /// The authority's identity.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The era it grants in.
    pub fn term(&self) -> u64 {
        self.term
    }

    /// The lease parameters clients are granted under.
    pub fn clocks(&self) -> &LeaseClocks {
        &self.clocks
    }

    /// Live grants.
    pub fn held(&self) -> usize {
        self.grants.len()
    }

    /// Every live grant, ordered by grant id — the audit surface (see
    /// [`GrantAudit`]). Bounded by live custody, never by grants ever
    /// issued.
    pub fn grants_snapshot(&self) -> Vec<GrantAudit> {
        let mut out = Vec::with_capacity(self.grants.len());
        self.grants.iter_sync(|id, g| {
            out.push(GrantAudit {
                grant_id: *id,
                client: g.client.clone(),
                lease_epoch: g.lease_epoch,
                ino: g.ino,
                span: g.span,
                token: g.lease.fencing_token(),
            });
            true
        });
        out.sort_by_key(|g| g.grant_id);
        out
    }

    /// Per-instance counters.
    pub fn stats(&self) -> OwnerStats {
        OwnerStats {
            granted: self.granted.load(Ordering::Relaxed),
            released: self.released.load(Ordering::Relaxed),
            conflicts: self.conflicts.load(Ordering::Relaxed),
            renewals: self.renewals.load(Ordering::Relaxed),
            revokes: self.revokes.load(Ordering::Relaxed),
            expiries: self.expiries.load(Ordering::Relaxed),
            reclaims: self.reclaims.load(Ordering::Relaxed),
            grace_conflicts: self.grace_conflicts.load(Ordering::Relaxed),
            unknown_leases: self.unknown_leases.load(Ordering::Relaxed),
            held: self.grants.len() as u64,
            clients: self.clients.len() as u64,
        }
    }

    // -----------------------------------------------------------------
    // DLM S9 blocker #3 — the allocation-lane assignment this era runs
    // under (docs/design-mw-data-alloc-partition.md §3; the map itself is
    // `crate::alloc_lane_grant::LaneAssignment`).
    // -----------------------------------------------------------------

    /// Install the era's lane map — the multi-writer arm's act, from the
    /// DURABLE claim set, before the listener admits its first join.
    ///
    /// A store rather than a once-cell on purpose: an authority installs one
    /// map per era, and a *changed* map is a fault its clients must observe
    /// (they self-fence at their next renewal rather than adopting a lane
    /// their already-minted offsets do not belong to).
    pub fn install_lane_assignment(&self, map: Arc<crate::alloc_lane_grant::LaneAssignment>) {
        log::warn!(
            "S9: custody authority '{}' runs era {} with a {}-way data-plane allocation \
             partition ({} enrolled co-writer lane(s); this node is lane 0). Every member's lane \
             travels on its lease — no knob can put two writers in one residue class",
            self.id,
            self.term,
            map.writers(),
            map.writers().saturating_sub(1),
        );
        self.lanes.store(Some(map));
    }

    /// The era's lane map (`None` = solo: no partition anywhere).
    pub fn lane_assignment(&self) -> Option<Arc<crate::alloc_lane_grant::LaneAssignment>> {
        self.lanes.load_full()
    }

    /// The `(lane, writers)` pair `client` was assigned — `(0, 1)` (SOLO)
    /// when this authority runs no partition, and `None` when it runs one
    /// that does not name this client (the roster-growth rule: a member
    /// enrolled after the arm has no lane in this era).
    fn lane_for(&self, client: &str) -> Option<(u16, u16)> {
        match self.lane_assignment() {
            None => Some((0, 1)),
            Some(map) => map.lane_of(client).map(|lane| (lane, map.writers())),
        }
    }

    fn lease_frame(&self, client: &str, epoch: u64, now: u64) -> LeaseFrame {
        let (writer_lane, writers) = self.lane_for(client).unwrap_or((0, 1));
        LeaseFrame {
            schema: CUSTODY_SCHEMA,
            epoch,
            term: self.term,
            custody_epoch: crate::dlm::compose_token(self.term, epoch),
            t_owner_ms: self.clocks.t_owner.as_millis() as u64,
            skew_max_ms: self.clocks.skew_max.as_millis() as u64,
            d_purge_ms: self.clocks.d_purge.as_millis() as u64,
            renew_ms: self.clocks.renew_interval.as_millis() as u64,
            granted_at_owner_ms: now,
            writer_lane,
            writers,
        }
    }

    /// **Validate a reservation raise** a peer shipped (S9's allocation-lane
    /// seam, served in [`crate::meta_ship::publish`]): the presented lease
    /// must be this client's current custody, and the lane it names must be
    /// the lane THIS authority assigned it, at this era's width.
    ///
    /// That is what makes a co-writer's lane unforgeable in the only way that
    /// matters here: the durable record is written by the authority, its
    /// content is checked against a map derived from a record only the
    /// authority can write, and the client's claim on a lane is backed by a
    /// lease epoch the authority minted and handed to nobody else.
    ///
    /// `Err(reason)` is the operator-facing refusal text.
    pub fn check_lane_raise(
        &self,
        client: &str,
        lease_epoch: u64,
        lane: u16,
        writers: u16,
    ) -> std::result::Result<(), String> {
        if !self.lease_current(client, lease_epoch) {
            return Err(format!(
                "S9: refusing an allocation-lane raise from '{client}': lease epoch \
                 {lease_epoch} is not custody on authority '{}' (revoked, swept past its TTL, or \
                 minted by a previous authority). It must self-fence and re-join — a raise under \
                 a dead lease would durably move a frontier for a lane this node may no longer \
                 hold",
                self.id
            ));
        }
        match self.lane_for(client) {
            Some((assigned, width)) if assigned == lane && width == writers => Ok(()),
            Some((assigned, width)) => Err(format!(
                "S9: refusing an allocation-lane raise from '{client}' for lane {lane} of \
                 {writers}: this authority assigned it lane {assigned} of {width}. A writer may \
                 only ever declare a frontier for its OWN residue class — declaring a peer's \
                 would let two mounts hand out one device offset, which is the collision the \
                 partition exists to prevent"
            )),
            None => Err(format!(
                "S9: refusing an allocation-lane raise from '{client}': this era's allocation \
                 partition does not name it, so it holds no lane. Enrollment after an arm does \
                 not widen a live partition — re-arm the authority (a new era) to admit it"
            )),
        }
    }

    /// Admit a co-writer (fresh, or a re-join that lost its lease view).
    pub fn join(&self, req: &JoinFrame) -> Result<LeaseFrame> {
        if req.client.is_empty() {
            return Err(SqueezefsError::InvalidOperation(
                "S9: a co-writer must present a non-empty identity".into(),
            ));
        }
        // DLM S9 blocker #3: a member this era's allocation partition does not
        // name has no lane, and a co-writer with no lane can place no fresh
        // block. Refusing at the JOIN is what makes the roster-growth rule
        // honest: enrollment after an arm does not widen a live partition,
        // because every live writer's residue class is fixed for the era —
        // widening it under them would put two mounts in overlapping classes
        // (at `W = 2` lane 1 mints `b % 2 == 1`; a node that later read
        // `W = 3` would mint `b % 3 == 2`, and index 5 is in both).
        if self.lane_for(&req.client).is_none() {
            let map = self.lane_assignment();
            return Err(SqueezefsError::InvalidOperation(format!(
                "S9: authority '{}' refuses the join of '{}': this era's {}-way data-plane \
                 allocation partition does not name it, so there is no allocation LANE to grant \
                 — and a co-writer without a lane cannot place a fresh block anywhere. A \
                 partition width changes only at an authority RE-ARM (a new era), never under \
                 live writers: add this node to SQUEEZEFS_MW_MEMBERS and re-arm the authority",
                self.id,
                req.client,
                map.map(|m| m.writers()).unwrap_or(1),
            )));
        }
        let now = self.clock.now_ms();
        let epoch = self.next_epoch.fetch_add(1, Ordering::AcqRel);
        // A re-join REPLACES the prior lease (same identity, new epoch):
        // the client is telling us it lost its view, and keeping the old
        // epoch alive would leave custody nobody presents. Its GRANTS are
        // revoked with it — a client that cannot present its lease cannot
        // present its grants either, and bytes nobody can name must become
        // grantable.
        if let Some(prior) = self.clients.read_sync(&req.client, |_, l| l.epoch) {
            if Some(prior) != req.prior_epoch || req.prior_epoch.is_none() {
                self.revoke_client(
                    &req.client,
                    &format!("re-joined with a fresh lease (prior epoch {prior})"),
                );
            }
        }
        let _ = self.clients.insert_sync(
            req.client.clone(),
            ClientLease {
                epoch,
                pr_key: req.pr_key,
                renewed_ms: now,
                deadline_ms: now + self.clocks.t_owner.as_millis() as u64,
                inflight: Vec::new(),
                dead_grants: Vec::new(),
            },
        );
        log::info!(
            "S9: co-writer '{}' joined authority '{}' in era {} with lease epoch {epoch} \
             (custody epoch {:#x}, registrant key {:#x})",
            req.client,
            self.id,
            self.term,
            crate::dlm::compose_token(self.term, epoch),
            req.pr_key
        );
        Ok(self.lease_frame(&req.client, epoch, now))
    }

    /// Is `client`'s presented lease epoch its current custody?
    fn lease_current(&self, client: &str, epoch: u64) -> bool {
        self.clients
            .read_sync(client, |_, l| l.epoch == epoch)
            .unwrap_or(false)
    }

    /// A client's OWNER-side deadline in this clock's milliseconds — the
    /// instant after which the authority may grant its bytes elsewhere. The
    /// contract the client's stricter deadline is measured against.
    pub fn lease_deadline_ms(&self, client: &str) -> Option<u64> {
        self.clients.read_sync(client, |_, l| l.deadline_ms)
    }

    /// Grant write custody: the authority takes ITS OWN lease on the bytes
    /// and hands the client the fencing token.
    ///
    /// The wait budget is `min(requested, one renewal cadence)`: a client
    /// that cannot be given custody within a cadence must be told, not held
    /// on a service lane (the S6 venue discipline applied to a waiting
    /// arbitration).
    pub async fn grant(&self, req: &AcquireFrame) -> std::result::Result<GrantRecord, u16> {
        if req.schema != CUSTODY_SCHEMA {
            return Err(CUSTODY_SCHEMA_MISMATCH);
        }
        if !self.lease_current(&req.client, req.lease_epoch) {
            self.unknown_leases.fetch_add(1, Ordering::Relaxed);
            UNKNOWN_LEASES.fetch_add(1, Ordering::Relaxed);
            return Err(CUSTODY_UNKNOWN_LEASE);
        }
        if self.in_grace() {
            self.grace_conflicts.fetch_add(1, Ordering::Relaxed);
            GRACE_CONFLICTS.fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "S9: refusing a FRESH acquire from '{}' on inode_{} — authority '{}' is inside \
                 its failover grace window, which admits reclaim only (spec §6.7 Recovery)",
                req.client,
                req.ino,
                self.id
            );
            return Err(CUSTODY_IN_GRACE);
        }
        let mode = if req.concurrent_write {
            LockMode::ConcurrentWrite
        } else {
            LockMode::Exclusive
        };
        let budget = Duration::from_millis(req.wait_ms).min(self.clocks.renew_interval);
        let path = format!("inode_{}", req.ino);
        let t_arb = Instant::now();
        let lease = self
            .arbiter
            .acquire_lock_mode(&path, req.span, mode, budget)
            .await;
        phase_record(CustodyPhase::Arbitrate, t_arb);
        let lease = match lease {
            Ok(lease) => lease,
            Err(e) => {
                self.conflicts.fetch_add(1, Ordering::Relaxed);
                CONFLICTS.fetch_add(1, Ordering::Relaxed);
                log::warn!(
                    "S9: refusing custody of inode_{} {:?} to '{}': {e}",
                    req.ino,
                    req.span,
                    req.client
                );
                return Err(CUSTODY_CONFLICT);
            }
        };
        let grant_id = self.next_grant.fetch_add(1, Ordering::AcqRel);
        let token = lease.fencing_token();
        let _ = self.grants.insert_sync(
            grant_id,
            GrantState {
                client: req.client.clone(),
                lease_epoch: req.lease_epoch,
                ino: req.ino,
                span: req.span,
                lease,
            },
        );
        self.granted.fetch_add(1, Ordering::Relaxed);
        log::debug!(
            "S9: granted custody {grant_id} of inode_{} {:?} to '{}' at token {token:#x}",
            req.ino,
            req.span,
            req.client
        );
        Ok(GrantRecord {
            schema: CUSTODY_SCHEMA,
            grant_id,
            ino: req.ino,
            span: req.span,
            token,
            term: self.term,
            custody_epoch: crate::dlm::compose_token(self.term, req.lease_epoch),
        })
    }

    /// Renew a client lease, absorbing its in-flight destination set and
    /// reporting which of its grants have died. One `scc` probe — this is
    /// the plane's hot operation and it commits nothing.
    pub fn renew(
        &self,
        client: &str,
        lease_epoch: u64,
        inflight: &[u64],
    ) -> std::result::Result<RenewReplyFrame, u16> {
        let now = self.clock.now_ms();
        let ttl = self.clocks.t_owner.as_millis() as u64;
        let mut dead_grants = Vec::new();
        let updated = self
            .clients
            .update_sync(client, |_, l| {
                if l.epoch != lease_epoch {
                    return false;
                }
                l.renewed_ms = now;
                l.deadline_ms = now + ttl;
                l.inflight = inflight.to_vec();
                dead_grants = std::mem::take(&mut l.dead_grants);
                true
            })
            .unwrap_or(false);
        if !updated {
            self.unknown_leases.fetch_add(1, Ordering::Relaxed);
            UNKNOWN_LEASES.fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "S9: co-writer '{client}' presented lease epoch {lease_epoch}, which is not \
                 custody on authority '{}' (revoked, swept past its TTL, or granted by a \
                 previous authority) — it must self-fence and re-join",
                self.id
            );
            return Err(CUSTODY_UNKNOWN_LEASE);
        }
        self.renewals.fetch_add(1, Ordering::Relaxed);
        Ok(RenewReplyFrame {
            schema: CUSTODY_SCHEMA,
            lease: self.lease_frame(client, lease_epoch, now),
            dead_grants,
        })
    }

    /// Retire named grants at the client's request. Dropping the
    /// authority's own lease is what makes the bytes grantable again.
    pub fn release(&self, client: &str, grant_ids: &[u64]) -> usize {
        let mut n = 0;
        for id in grant_ids {
            let mine = self
                .grants
                .read_sync(id, |_, g| g.client == client)
                .unwrap_or(false);
            if !mine {
                continue;
            }
            if self.grants.remove_sync(id).is_some() {
                n += 1;
                self.released.fetch_add(1, Ordering::Relaxed);
            }
        }
        n
    }

    /// **Revoke a co-writer**: it leaves the lease table, every grant it
    /// held is retired (so the bytes become grantable), ONE dead epoch is
    /// minted for its whole custody, and its declared in-flight offsets
    /// enter the S7 do-not-reallocate quarantine.
    ///
    /// Returns one [`DeadCustody`] — the cohort a [`DrainProof`] releases.
    /// Empty when the client was not a member.
    pub fn revoke_client(&self, client: &str, reason: &str) -> Vec<DeadCustody> {
        let Some((_, lease)) = self.clients.remove_sync(client) else {
            return Vec::new();
        };
        self.revokes.fetch_add(1, Ordering::Relaxed);
        REVOKES.fetch_add(1, Ordering::Relaxed);
        Vec::from_iter(self.kill(client, lease, reason))
    }

    /// Sweep every client lease past the authority's TTL (§6.7 "Recovery",
    /// client-failure half). Runs on the arm's cadence task, never on a
    /// handler lane.
    pub fn expire_due(&self) -> Vec<DeadCustody> {
        let now = self.clock.now_ms();
        let mut due: Vec<String> = Vec::new();
        self.clients.iter_sync(|id, l| {
            // `>=`: the lease expires AT the deadline, which is the instant
            // the client's own (strictly earlier) deadline was measured
            // against.
            if now >= l.deadline_ms {
                due.push(id.clone());
            }
            true
        });
        due.sort();
        let mut out = Vec::new();
        for id in due {
            let Some((_, lease)) = self.clients.remove_sync(&id) else {
                continue;
            };
            self.expiries.fetch_add(1, Ordering::Relaxed);
            EXPIRIES.fetch_add(1, Ordering::Relaxed);
            out.extend(self.kill(
                &id,
                lease,
                &format!(
                    "lease TTL {:?} expired without a renewal",
                    self.clocks.t_owner
                ),
            ));
        }
        out
    }

    /// The common death path: retire the client's grants, mint its dead
    /// epoch, quarantine its declared destinations.
    fn kill(&self, client: &str, lease: ClientLease, reason: &str) -> Option<DeadCustody> {
        let mut grants: Vec<u64> = Vec::new();
        self.grants.iter_sync(|id, g| {
            if g.client == client {
                grants.push(*id);
            }
            true
        });
        grants.sort_unstable();
        for id in &grants {
            // Dropping the authority's lease releases the bytes. The
            // grant's client is told at its next renewal — the pull-based
            // revocation channel — and it is bounded by its own T_self.
            let _ = self.grants.remove_sync(id);
        }
        let epoch = crate::data_custody::declare_dead_epoch(&format!(
            "S9: co-writer '{client}' custody revoked by authority '{}' ({reason})",
            self.id
        ));
        let admitted = match (&self.quarantine, lease.inflight.is_empty()) {
            (Some(sink), false) => sink.quarantine(&lease.inflight, epoch),
            (None, false) => {
                log::error!(
                    "S9: co-writer '{client}' died holding {} declared in-flight offset(s) but \
                     this authority has no quarantine sink — those offsets are NOT protected \
                     from reallocation. A data-plane authority must be armed with one",
                    lease.inflight.len()
                );
                0
            }
            (_, true) => 0,
        };
        QUARANTINED.fetch_add(admitted as u64, Ordering::Relaxed);
        log::warn!(
            "S9: co-writer '{client}' (lease epoch {}) DIED — {reason}; {} grant(s) retired, \
             {admitted} offset(s) quarantined under {epoch} until a drain proof arrives",
            lease.epoch,
            grants.len()
        );
        Some(DeadCustody {
            client: client.to_string(),
            lease_epoch: lease.epoch,
            epoch,
            grants,
            offsets: lease.inflight,
            pr_key: lease.pr_key,
            reason: reason.to_string(),
        })
    }

    /// **The drain proof**: release a dead custody's quarantined cohort.
    /// Returns the count released (`0` when nothing was quarantined).
    ///
    /// The proof is a value the caller had to construct from evidence — see
    /// [`DrainProof`] — which is what makes "no release without a proof"
    /// structural rather than asserted.
    pub fn release_dead(&self, dead: &DeadCustody, proof: DrainProof) -> usize {
        let released = self
            .quarantine
            .as_ref()
            .map(|sink| sink.release(dead.epoch))
            .unwrap_or(0);
        PROOFS.fetch_add(1, Ordering::Relaxed);
        log::info!(
            "S9: {} proven drained ({}) — {released} quarantined offset(s) released for dead \
             co-writer '{}'",
            dead.epoch,
            proof.describe(),
            dead.client
        );
        released
    }

    /// Open the failover **grace window** (§6.7): only reclaim is admitted
    /// until every id in `expected` has re-asserted, or until the window's
    /// deadline.
    pub fn open_grace(&self, expected: Vec<String>) {
        let until = self.clock.now_ms() + self.clocks.grace.as_millis() as u64;
        let expected: std::collections::BTreeSet<String> = expected.into_iter().collect();
        log::warn!(
            "S9: custody authority '{}' opened a failover grace window for {:?}: reclaim only, \
             conflicting fresh acquires refused; awaiting re-assertion from {} prior \
             co-writer(s) (without the window, failover is a cluster-wide forced-flush storm \
             — spec §6.7)",
            self.id,
            self.clocks.grace,
            expected.len()
        );
        *self.grace.lock() = Some(Grace {
            until_ms: until,
            expected,
        });
    }

    /// `true` ⇔ the grace window is open (it closes on full re-assertion or
    /// at its deadline, whichever comes first).
    pub fn in_grace(&self) -> bool {
        let mut guard = self.grace.lock();
        let Some(g) = guard.as_ref() else {
            return false;
        };
        if self.clock.now_ms() >= g.until_ms {
            log::info!(
                "S9: custody authority '{}' closed its grace window on the deadline with {} \
                 co-writer(s) never re-asserting — their custody is gone and fresh acquires \
                 are admitted again",
                self.id,
                g.expected.len()
            );
            *guard = None;
            return false;
        }
        true
    }

    /// Milliseconds left in the grace window (`0` = closed).
    pub fn grace_remaining_ms(&self) -> u64 {
        if !self.in_grace() {
            return 0;
        }
        let guard = self.grace.lock();
        guard
            .as_ref()
            .map(|g| g.until_ms.saturating_sub(self.clock.now_ms()))
            .unwrap_or(0)
    }

    fn note_reclaim(&self, client: &str) {
        let mut guard = self.grace.lock();
        let Some(g) = guard.as_mut() else {
            return;
        };
        g.expected.remove(client);
        if g.expected.is_empty() {
            log::info!(
                "S9: custody authority '{}' — every prior co-writer re-asserted, grace window \
                 closed early",
                self.id
            );
            *guard = None;
        }
    }

    /// Serve a grace-window **reclaim**: the client re-asserts the objects
    /// it held under the predecessor and receives **fresh-era** custody.
    ///
    /// Admitted whether or not the window is open — a client reconnecting
    /// late is re-asserting state, not acquiring it, and refusing would
    /// strand custody without making anything safer (the S8 reclaim
    /// reasoning, unchanged).
    pub async fn reclaim(
        &self,
        frame: &ReclaimFrame,
    ) -> std::result::Result<Vec<GrantRecord>, u16> {
        if frame.schema != CUSTODY_SCHEMA {
            return Err(CUSTODY_SCHEMA_MISMATCH);
        }
        if !self.lease_current(&frame.client, frame.lease_epoch) {
            self.unknown_leases.fetch_add(1, Ordering::Relaxed);
            UNKNOWN_LEASES.fetch_add(1, Ordering::Relaxed);
            return Err(CUSTODY_UNKNOWN_LEASE);
        }
        let mut out = Vec::with_capacity(frame.inos.len());
        for ino in &frame.inos {
            let req = AcquireFrame {
                schema: CUSTODY_SCHEMA,
                client: frame.client.clone(),
                lease_epoch: frame.lease_epoch,
                ino: *ino,
                span: None,
                concurrent_write: false,
                // A reclaim never waits: the predecessor's grants are gone
                // with its RAM, so anything that conflicts is a LIVE
                // conflict the client must be told about.
                wait_ms: 0,
            };
            // Deliberately NOT `self.grant()`: that path refuses inside the
            // grace window, and admitting reclaim is the window's entire
            // purpose.
            let mode = LockMode::Exclusive;
            let path = format!("inode_{ino}");
            let Ok(lease) = self
                .arbiter
                .acquire_lock_mode(&path, None, mode, Duration::from_millis(0))
                .await
            else {
                self.conflicts.fetch_add(1, Ordering::Relaxed);
                CONFLICTS.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let grant_id = self.next_grant.fetch_add(1, Ordering::AcqRel);
            let token = lease.fencing_token();
            let _ = self.grants.insert_sync(
                grant_id,
                GrantState {
                    client: req.client.clone(),
                    lease_epoch: req.lease_epoch,
                    ino: *ino,
                    span: None,
                    lease,
                },
            );
            self.granted.fetch_add(1, Ordering::Relaxed);
            out.push(GrantRecord {
                schema: CUSTODY_SCHEMA,
                grant_id,
                ino: *ino,
                span: None,
                token,
                term: self.term,
                custody_epoch: crate::dlm::compose_token(self.term, frame.lease_epoch),
            });
        }
        self.reclaims.fetch_add(1, Ordering::Relaxed);
        RECLAIMS.fetch_add(1, Ordering::Relaxed);
        self.note_reclaim(&frame.client);
        log::info!(
            "S9: authority '{}' admitted a reclaim of {}/{} object(s) from '{}' in era {} \
             (grace {})",
            self.id,
            out.len(),
            frame.inos.len(),
            frame.client,
            self.term,
            if self.in_grace() { "open" } else { "closed" }
        );
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// The authority's RPC surface
// ---------------------------------------------------------------------------

/// The custody verbs, served on the S3 wire's pinned lanes.
///
/// **Async by necessity, and the venue rule is why the split exists**: an
/// acquire runs the authority's own arbitration, which may WAIT for a
/// conflicting holder. The future is polled on the pinned lane that
/// received the frame (never on a conveyor task), and the wait is clamped
/// to one renewal cadence so a lane can never be pinned by a client's
/// patience.
#[derive(Debug)]
pub struct CustodyService {
    owner: Arc<WriteCustodyOwner>,
}

impl CustodyService {
    /// Serve `owner`'s custody verbs.
    pub fn new(owner: Arc<WriteCustodyOwner>) -> Arc<Self> {
        Arc::new(Self { owner })
    }

    fn refuse(id: u64, status: u16, reason: String) -> RpcResponse {
        log::warn!("S9 custody refusal: {reason}");
        RpcResponse {
            id,
            status,
            body: reason.into_bytes(),
        }
    }

    async fn serve(&self, req: RpcRequest) -> RpcResponse {
        match req.verb {
            VERB_CUSTODY_JOIN => match decode::<JoinFrame>(&req.body, "join") {
                Err(e) => Self::refuse(req.id, CUSTODY_MALFORMED, format!("{e}")),
                Ok(frame) if frame.schema != CUSTODY_SCHEMA => Self::refuse(
                    req.id,
                    CUSTODY_SCHEMA_MISMATCH,
                    format!(
                        "peer speaks custody schema {} and this authority speaks \
                         {CUSTODY_SCHEMA} — refusing rather than guessing at a \
                         custody-bearing frame",
                        frame.schema
                    ),
                ),
                Ok(frame) => match self.owner.join(&frame) {
                    Ok(lease) => reply(req.id, &lease, "lease"),
                    Err(e) => Self::refuse(req.id, CUSTODY_MALFORMED, format!("{e}")),
                },
            },
            VERB_CUSTODY_ACQUIRE => match decode::<AcquireFrame>(&req.body, "acquire") {
                Err(e) => Self::refuse(req.id, CUSTODY_MALFORMED, format!("{e}")),
                Ok(frame) => match self.owner.grant(&frame).await {
                    Ok(grant) => reply(req.id, &grant, "grant"),
                    Err(status) => Self::refuse(
                        req.id,
                        status,
                        format!(
                            "custody of inode_{} {:?} refused to '{}' ({})",
                            frame.ino,
                            frame.span,
                            frame.client,
                            status_name(status)
                        ),
                    ),
                },
            },
            VERB_CUSTODY_RENEW => match decode::<RenewFrame>(&req.body, "renew") {
                Err(e) => Self::refuse(req.id, CUSTODY_MALFORMED, format!("{e}")),
                Ok(frame) => {
                    match self
                        .owner
                        .renew(&frame.client, frame.lease_epoch, &frame.inflight)
                    {
                        Ok(r) => reply(req.id, &r, "renew reply"),
                        Err(status) => Self::refuse(
                            req.id,
                            status,
                            format!(
                                "the lease epoch {} of '{}' is not custody ({})",
                                frame.lease_epoch,
                                frame.client,
                                status_name(status)
                            ),
                        ),
                    }
                }
            },
            VERB_CUSTODY_RELEASE => match decode::<ReleaseFrame>(&req.body, "release") {
                Err(e) => Self::refuse(req.id, CUSTODY_MALFORMED, format!("{e}")),
                Ok(frame) => {
                    let n = self.owner.release(&frame.client, &frame.grant_ids);
                    RpcResponse {
                        id: req.id,
                        status: CUSTODY_OK,
                        body: (n as u64).to_le_bytes().to_vec(),
                    }
                }
            },
            VERB_CUSTODY_RECLAIM => match decode::<ReclaimFrame>(&req.body, "reclaim") {
                Err(e) => Self::refuse(req.id, CUSTODY_MALFORMED, format!("{e}")),
                Ok(frame) => match self.owner.reclaim(&frame).await {
                    Ok(grants) => reply(
                        req.id,
                        &ReclaimReplyFrame {
                            schema: CUSTODY_SCHEMA,
                            grants,
                        },
                        "reclaim reply",
                    ),
                    Err(status) => Self::refuse(
                        req.id,
                        status,
                        format!(
                            "reclaim from '{}' refused ({})",
                            frame.client,
                            status_name(status)
                        ),
                    ),
                },
            },
            other => RpcResponse {
                id: req.id,
                status: RPC_UNKNOWN_VERB,
                body: format!("S9: unknown custody verb {other}").into_bytes(),
            },
        }
    }
}

fn status_name(status: u16) -> &'static str {
    match status {
        CUSTODY_CONFLICT => "conflicting custody",
        CUSTODY_UNKNOWN_LEASE => "unknown lease",
        CUSTODY_IN_GRACE => "grace window: reclaim only",
        CUSTODY_SCHEMA_MISMATCH => "schema mismatch",
        CUSTODY_MALFORMED => "malformed",
        _ => "refused",
    }
}

fn reply<T: Serialize>(id: u64, value: &T, what: &str) -> RpcResponse {
    match encode(value, what) {
        Ok(body) => RpcResponse {
            id,
            status: CUSTODY_OK,
            body,
        },
        Err(e) => RpcResponse {
            id,
            status: CUSTODY_MALFORMED,
            body: format!("S9 {what} encode failed: {e}").into_bytes(),
        },
    }
}

impl RpcAsyncService for CustodyService {
    fn call<'a>(
        &'a self,
        req: RpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcResponse> + Send + 'a>> {
        Box::pin(self.serve(req))
    }
}

/// Dispatch by verb RANGE to registered **awaiting** services — the async
/// twin of [`crate::membership_wire::VerbRouter`], additive for the same
/// reason: several stages must put verbs on ONE listener without editing
/// each other's dispatch.
///
/// Membership keeps its own synchronous listener (its arbitration is RAM
/// only, which is why S6 chose `RpcService`); this router hosts the two
/// vocabularies that must await — S9's custody and S9's publish path.
pub struct AsyncVerbRouter {
    routes: Vec<(u16, u16, Arc<dyn RpcAsyncService>)>,
}

impl std::fmt::Debug for AsyncVerbRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncVerbRouter")
            .field(
                "ranges",
                &self
                    .routes
                    .iter()
                    .map(|(lo, hi, _)| format!("{lo:#x}..={hi:#x}"))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Default for AsyncVerbRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncVerbRouter {
    /// An empty router (every verb unclaimed).
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    /// Claim `lo..=hi` for `service`.
    pub fn with(mut self, lo: u16, hi: u16, service: Arc<dyn RpcAsyncService>) -> Self {
        self.routes.push((lo.min(hi), hi.max(lo), service));
        self
    }

    /// Claim S9's custody block for `owner`.
    pub fn with_custody(self, owner: Arc<WriteCustodyOwner>) -> Self {
        self.with(
            VERB_CUSTODY_BASE,
            VERB_CUSTODY_LAST,
            CustodyService::new(owner),
        )
    }

    /// Claim S9's publish block for `service`.
    pub fn with_publish(self, service: Arc<crate::meta_ship::publish::PublishService>) -> Self {
        self.with(
            crate::meta_ship::publish::VERB_PUBLISH_BASE,
            crate::meta_ship::publish::VERB_PUBLISH_LAST,
            service,
        )
    }
}

impl RpcAsyncService for AsyncVerbRouter {
    fn call<'a>(
        &'a self,
        req: RpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcResponse> + Send + 'a>> {
        Box::pin(async move {
            for (lo, hi, svc) in &self.routes {
                if req.verb >= *lo && req.verb <= *hi {
                    return svc.call(req).await;
                }
            }
            RpcResponse {
                id: req.id,
                status: RPC_UNKNOWN_VERB,
                body: Vec::new(),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// The co-writer
// ---------------------------------------------------------------------------

/// One live grant on the client, and the handle the adopted
/// [`LockLease`] releases through.
struct ClientGrant {
    grant_id: u64,
    live: AtomicBool,
    /// The release queue this grant pushes onto when its lease drops.
    /// `Drop` is synchronous and the release must travel, so the wire hop
    /// is deferred to [`WriteCustodyClient::drain_releases`] (the renewal
    /// cadence drains it, and so does the next acquire).
    pending: Arc<parking_lot::Mutex<Vec<u64>>>,
}

impl crate::dlm::RemoteGrant for ClientGrant {
    fn release(&self) {
        if self.live.swap(false, Ordering::AcqRel) {
            self.pending.lock().push(self.grant_id);
        }
    }
    fn live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }
}

/// A **co-writer**: it holds write custody granted by another node's
/// authority, and it writes the bytes itself.
pub struct WriteCustodyClient {
    id: String,
    endpoint: String,
    secret: Vec<u8>,
    session: tokio::sync::Mutex<Option<RpcClient>>,
    /// This client's lease view — S6's [`MemberSession`], reused verbatim
    /// so the stricter-clock law and the writer's self-fence (which poisons
    /// process data custody) have exactly one implementation.
    lease: arc_swap::ArcSwap<MemberSession>,
    lease_epoch: AtomicU64,
    /// DLM **S9** blocker #3: the data-plane allocation lane the authority
    /// granted on the lease, packed `writers << 16 | lane`. `0` is SOLO —
    /// i.e. no partition, which is what an authority with no enrolled
    /// co-writer answers.
    lane: std::sync::atomic::AtomicU32,
    grants: scc::HashMap<u64, Arc<ClientGrant>>,
    pending_releases: Arc<parking_lot::Mutex<Vec<u64>>>,
    inflight: parking_lot::Mutex<Vec<u64>>,
    clock: LeaseClock,
}

impl std::fmt::Debug for WriteCustodyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteCustodyClient")
            .field("id", &self.id)
            .field("endpoint", &self.endpoint)
            .field("lease_epoch", &self.lease_epoch.load(Ordering::Relaxed))
            .field("grants", &self.grants.len())
            .finish_non_exhaustive()
    }
}

impl WriteCustodyClient {
    /// Dial `endpoint` and JOIN the custody plane, adopting the lease
    /// (and, with it, the authority's era and this client's custody epoch).
    pub async fn connect(endpoint: &str, secret: &[u8], id: &str) -> Result<Arc<Self>> {
        Self::connect_with_clock(endpoint, secret, id, LeaseClock::monotonic(), 0).await
    }

    /// [`Self::connect`] naming the clock and the NVMe registrant key —
    /// the mount arm passes its WERO key so a preempt of this client is
    /// possible (i.e. so a drain proof about it can exist).
    pub async fn connect_with_clock(
        endpoint: &str,
        secret: &[u8],
        id: &str,
        clock: LeaseClock,
        pr_key: u64,
    ) -> Result<Arc<Self>> {
        let mut session = RpcClient::connect(endpoint, secret, id, None).await?;
        let anchor = clock.now_ms();
        let frame = JoinFrame {
            schema: CUSTODY_SCHEMA,
            client: id.to_string(),
            pr_key,
            prior_epoch: None,
        };
        let lease = Self::join_on(&mut session, &frame).await?;
        let member = MemberSession::adopt(
            id,
            MemberRole::Writer,
            &lease.to_membership_grant(),
            anchor,
            clock.clone(),
        );
        // The grant carries its epoch (S7's `CustodyEpoch` raw form): the
        // client adopts the generation as its floor and thereafter
        // authorizes every DMA under `current_epoch()`.
        crate::data_custody::adopt_custody_generation(crate::dlm::token_grant_seq(
            lease.custody_epoch,
        ));
        crate::dlm::adopt_durable_term(lease.term);
        Ok(Arc::new(Self {
            id: id.to_string(),
            endpoint: endpoint.to_string(),
            secret: secret.to_vec(),
            session: tokio::sync::Mutex::new(Some(session)),
            lease: arc_swap::ArcSwap::from_pointee(member),
            lease_epoch: AtomicU64::new(lease.epoch),
            lane: std::sync::atomic::AtomicU32::new(pack_lane(lease.writer_lane, lease.writers)),
            grants: scc::HashMap::new(),
            pending_releases: Arc::new(parking_lot::Mutex::new(Vec::new())),
            inflight: parking_lot::Mutex::new(Vec::new()),
            clock,
        }))
    }

    async fn join_on(session: &mut RpcClient, frame: &JoinFrame) -> Result<LeaseFrame> {
        let body = encode(frame, "join")?;
        RPCS.fetch_add(1, Ordering::Relaxed);
        let reply = session.call(VERB_CUSTODY_JOIN, body).await?;
        if reply.status != CUSTODY_OK {
            return Err(SqueezefsError::InvalidOperation(format!(
                "S9: the custody authority refused the join (status {}): {}",
                reply.status,
                String::from_utf8_lossy(&reply.body)
            )));
        }
        decode(&reply.body, "lease")
    }

    /// This co-writer's identity.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The lease epoch this client holds — its proof, on every verb that
    /// needs one, that the authority granted it what it is presenting.
    pub fn lease_epoch(&self) -> u64 {
        self.lease_epoch.load(Ordering::Acquire)
    }

    /// **The data-plane allocation lane the authority granted** (DLM S9
    /// blocker #3): the residue class this mount, and only this mount, may
    /// mint fresh block indices from.
    ///
    /// [`AppendPartition::SOLO`](crate::meta_backend::kv::journal::AppendPartition::SOLO)
    /// when the authority runs no partition, and
    /// then nothing engages — a co-writer's allocation stays refused exactly
    /// as it was before this seam closed, which is the honest answer for a
    /// single-writer authority that has enrolled nobody.
    pub fn lane_partition(&self) -> crate::meta_backend::kv::journal::AppendPartition {
        unpack_lane(self.lane.load(Ordering::Acquire))
    }

    /// The authority it holds custody from.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The client's OWN deadline (S6's `T_self`), strictly earlier than the
    /// authority's TTL by construction.
    pub fn t_self_deadline_ms(&self) -> u64 {
        self.lease.load().t_self_deadline_ms()
    }

    /// When the next renewal is due.
    pub fn renew_at_ms(&self) -> u64 {
        self.lease.load().renew_at_ms()
    }

    /// `true` ⇔ this client is past its own deadline and MUST fail-stop now
    /// — before the authority's TTL lets those bytes be granted elsewhere.
    pub fn self_fence_due(&self) -> bool {
        self.lease.load().self_fence_due()
    }

    /// **Fail-stop our own custody** (§6.7's stricter client clock): a
    /// WRITER poisons process data custody, so nothing can land after the
    /// authority may have re-granted. Idempotent, and counted.
    pub fn self_fence(&self, reason: &str) -> SelfFence {
        let fence = self.lease.load().self_fence(reason);
        if fence.first {
            SELF_FENCES.fetch_add(1, Ordering::Relaxed);
        }
        fence
    }

    /// Declare the device offsets this client may still be writing. The set
    /// rides the next renewal, and it is the cohort the authority
    /// quarantines if this client's custody dies (the job wire's
    /// pre-allocated-destination law).
    pub fn declare_inflight(&self, offsets: &[u64]) {
        *self.inflight.lock() = offsets.to_vec();
    }

    /// Acquire write custody of `ino` — the whole file (`span: None`) or one
    /// `[start, end)` byte range — and adopt it as local custody.
    pub async fn acquire(
        &self,
        ino: u64,
        span: Option<(u64, u64)>,
        mode: LockMode,
        wait: Duration,
    ) -> Result<LockLease> {
        // A queued release must reach the authority BEFORE a new acquire,
        // or a client that released and re-acquired the same span would
        // conflict with itself.
        self.drain_releases().await;
        let frame = AcquireFrame {
            schema: CUSTODY_SCHEMA,
            client: self.id.clone(),
            lease_epoch: self.lease_epoch.load(Ordering::Acquire),
            ino,
            span,
            concurrent_write: mode == LockMode::ConcurrentWrite,
            wait_ms: wait.as_millis().min(u64::MAX as u128) as u64,
        };
        let body = encode(&frame, "acquire")?;
        let t = Instant::now();
        // ACQUIRE is deliberately NOT retried on a transport failure: a
        // re-sent acquire after a lost reply would leave a grant on the
        // authority that this client cannot name, and the honest answer is
        // an error the caller can act on (S8's dedup window is what makes a
        // retry safe there; this vocabulary has none — a named residual).
        let reply = self.call_once(VERB_CUSTODY_ACQUIRE, body).await?;
        phase_record(CustodyPhase::Rtt, t);
        if reply.status != CUSTODY_OK {
            let detail = String::from_utf8_lossy(&reply.body).to_string();
            if reply.status == CUSTODY_UNKNOWN_LEASE {
                self.note_lease_lost(&detail);
            }
            return Err(SqueezefsError::LockFailed {
                reason: format!(
                    "S9: the custody authority at {} refused custody of inode_{ino} {span:?} \
                     ({}): {detail}",
                    self.endpoint,
                    status_name(reply.status)
                ),
            });
        }
        let grant: GrantRecord = decode(&reply.body, "grant")?;
        let t_adopt = Instant::now();
        let handle = Arc::new(ClientGrant {
            grant_id: grant.grant_id,
            live: AtomicBool::new(true),
            pending: Arc::clone(&self.pending_releases),
        });
        let _ = self.grants.insert_sync(grant.grant_id, Arc::clone(&handle));
        crate::data_custody::adopt_custody_generation(crate::dlm::token_grant_seq(
            grant.custody_epoch,
        ));
        let lease = crate::dlm::adopt_remote_grant(
            grant.ino,
            grant.span,
            grant.token,
            mode,
            handle as Arc<dyn crate::dlm::RemoteGrant>,
        )?;
        phase_record(CustodyPhase::Adopt, t_adopt);
        GRANTS.fetch_add(1, Ordering::Relaxed);
        Ok(lease)
    }

    /// Renew the client lease, carrying the declared in-flight set and
    /// absorbing the authority's answer about which grants have died.
    ///
    /// An "unknown lease" answer is the pull-based revocation channel: this
    /// client's custody is gone, every adopted grant is marked dead, and
    /// the process's custody generation ADVANCES (never poisons — a client
    /// that can re-join is not a fenced zombie).
    pub async fn renew_all(&self) -> Result<()> {
        self.drain_releases().await;
        let inflight = self.inflight.lock().clone();
        let frame = RenewFrame {
            schema: CUSTODY_SCHEMA,
            client: self.id.clone(),
            lease_epoch: self.lease_epoch.load(Ordering::Acquire),
            inflight,
        };
        let body = encode(&frame, "renew")?;
        let anchor = self.clock.now_ms();
        let t = Instant::now();
        let reply = self.call_retrying(VERB_CUSTODY_RENEW, body).await?;
        phase_record(CustodyPhase::Renew, t);
        if reply.status != CUSTODY_OK {
            let detail = String::from_utf8_lossy(&reply.body).to_string();
            self.note_lease_lost(&detail);
            return Err(SqueezefsError::LockFailed {
                reason: format!(
                    "S9: renewal refused by the custody authority at {} ({}) — this node's \
                     custody is gone: {detail}",
                    self.endpoint,
                    status_name(reply.status)
                ),
            });
        }
        let r: RenewReplyFrame = decode(&reply.body, "renew reply")?;
        // DLM S9 blocker #3 — **the lane is stable across a renewal, or this
        // mount fail-stops.** A renewal is a heartbeat, not a re-assignment:
        // the offsets we have already minted belong to the lane we were
        // granted, so adopting a different residue class would start handing
        // out indices a PEER owns. There is no safe way to continue, and the
        // house answer to "custody may have moved" is the stricter client
        // clock's — poison our own custody first, before anything can land.
        let held = self.lane.load(Ordering::Acquire);
        let answered = pack_lane(r.lease.writer_lane, r.lease.writers);
        if answered != held {
            let detail = format!(
                "the authority at {} answered a renewal naming allocation lane {} of {}, but \
                 this mount holds lane {} of {} and has minted in it — a live writer's residue \
                 class cannot be redefined under it (a width change is a new authority ERA, \
                 which this mount must re-join to observe)",
                self.endpoint,
                r.lease.writer_lane,
                r.lease.writers,
                held & 0xffff,
                held >> 16,
            );
            log::error!("S9: {detail}");
            self.self_fence(&detail);
            return Err(SqueezefsError::LockFailed { reason: detail });
        }
        self.lease
            .load()
            .renewed(&r.lease.to_membership_grant(), anchor);
        RENEWALS.fetch_add(1, Ordering::Relaxed);
        if !r.dead_grants.is_empty() {
            for id in &r.dead_grants {
                self.mark_dead(*id);
            }
            crate::data_custody::advance_custody_generation(&format!(
                "S9: the authority at {} retired {} of this node's grant(s)",
                self.endpoint,
                r.dead_grants.len()
            ));
        }
        Ok(())
    }

    /// Re-assert custody of `inos` inside a successor's grace window,
    /// adopting the fresh-era grants it returns.
    pub async fn reclaim(&self, inos: &[u64]) -> Result<Vec<GrantRecord>> {
        let frame = ReclaimFrame {
            schema: CUSTODY_SCHEMA,
            client: self.id.clone(),
            lease_epoch: self.lease_epoch.load(Ordering::Acquire),
            inos: inos.to_vec(),
        };
        let body = encode(&frame, "reclaim")?;
        let reply = self.call_retrying(VERB_CUSTODY_RECLAIM, body).await?;
        if reply.status != CUSTODY_OK {
            return Err(SqueezefsError::LockFailed {
                reason: format!(
                    "S9: reclaim refused by {} ({}): {}",
                    self.endpoint,
                    status_name(reply.status),
                    String::from_utf8_lossy(&reply.body)
                ),
            });
        }
        let r: ReclaimReplyFrame = decode(&reply.body, "reclaim reply")?;
        for grant in &r.grants {
            crate::dlm::adopt_durable_term(grant.term);
        }
        RECLAIMS.fetch_add(1, Ordering::Relaxed);
        Ok(r.grants)
    }

    /// Flush queued releases to the authority. Called by the renewal
    /// cadence, by the next acquire, and directly by a caller that wants
    /// the release to have LANDED (a test, or unmount teardown).
    pub async fn drain_releases(&self) {
        let ids: Vec<u64> = std::mem::take(&mut *self.pending_releases.lock());
        if ids.is_empty() {
            return;
        }
        for id in &ids {
            let _ = self.grants.remove_sync(id);
        }
        let frame = ReleaseFrame {
            schema: CUSTODY_SCHEMA,
            client: self.id.clone(),
            lease_epoch: self.lease_epoch.load(Ordering::Acquire),
            grant_ids: ids.clone(),
        };
        let Ok(body) = encode(&frame, "release") else {
            return;
        };
        match self.call_retrying(VERB_CUSTODY_RELEASE, body).await {
            Ok(_) => {
                RELEASES.fetch_add(ids.len() as u64, Ordering::Relaxed);
            }
            Err(e) => {
                // A release that could not be delivered is NOT lost work:
                // the authority's TTL retires the whole lease, which is
                // exactly the recovery path a dead client takes. Loud,
                // because it means the bytes stay held for up to one TTL.
                log::warn!(
                    "S9: could not deliver the release of {} grant(s) to {} ({e}) — those \
                     bytes stay held until this client's lease TTL expires",
                    ids.len(),
                    self.endpoint
                );
            }
        }
    }

    fn mark_dead(&self, grant_id: u64) {
        if let Some(handle) = self.grants.read_sync(&grant_id, |_, g| Arc::clone(g)) {
            // `live = false` WITHOUT queueing a release: the authority has
            // already retired it, so telling it again would be a lie about
            // a grant we no longer hold.
            handle.live.store(false, Ordering::Release);
        }
        let _ = self.grants.remove_sync(&grant_id);
    }

    /// This client's whole custody is gone: every adopted grant is dead and
    /// the custody generation advances, so no in-flight DMA authorized
    /// under it can land.
    fn note_lease_lost(&self, detail: &str) {
        let mut ids = Vec::new();
        self.grants.iter_sync(|id, _| {
            ids.push(*id);
            true
        });
        for id in ids {
            self.mark_dead(id);
        }
        crate::data_custody::advance_custody_generation(&format!(
            "S9: this node's custody lease is no longer custody ({detail})"
        ));
    }

    async fn call_once(
        &self,
        verb: u16,
        body: Vec<u8>,
    ) -> Result<crate::cluster_wire::RpcResponse> {
        let mut guard = self.session.lock().await;
        if guard.is_none() {
            *guard = Some(RpcClient::connect(&self.endpoint, &self.secret, &self.id, None).await?);
        }
        RPCS.fetch_add(1, Ordering::Relaxed);
        let out = guard
            .as_mut()
            .expect("connected above")
            .call(verb, body)
            .await;
        if out.is_err() {
            *guard = None;
        }
        out
    }

    /// [`Self::call_once`] with ONE reconnect+resend. Safe only for the
    /// idempotent verbs (join / renew / release / reclaim); an acquire never
    /// takes this path.
    async fn call_retrying(
        &self,
        verb: u16,
        body: Vec<u8>,
    ) -> Result<crate::cluster_wire::RpcResponse> {
        match self.call_once(verb, body.clone()).await {
            Ok(r) => Ok(r),
            Err(first) => {
                log::warn!(
                    "S9: custody verb {verb:#x} to {} failed ({first}) — reconnecting and \
                     resending (the verb is idempotent)",
                    self.endpoint
                );
                self.call_once(verb, body).await
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The allocation-lane pair on the wire (DLM S9 blocker #3): packed
// `writers << 16 | lane`, with `0` reading as SOLO so an unassigned lease and
// a solo authority are the same, single, no-partition answer.
// ---------------------------------------------------------------------------

fn pack_lane(lane: u16, writers: u16) -> u32 {
    if writers <= 1 {
        return 0;
    }
    (u32::from(writers) << 16) | u32::from(lane)
}

fn unpack_lane(packed: u32) -> crate::meta_backend::kv::journal::AppendPartition {
    use crate::meta_backend::kv::journal::AppendPartition;
    if packed == 0 {
        return AppendPartition::SOLO;
    }
    AppendPartition::new((packed >> 16) as u16, (packed & 0xffff) as u16)
        .unwrap_or(AppendPartition::SOLO)
}

// ---------------------------------------------------------------------------
// The process registry, and S4's foreign-home seam
// ---------------------------------------------------------------------------

static OWNER: Lazy<ArcSwapOption<WriteCustodyOwner>> = Lazy::new(ArcSwapOption::empty);
static CLIENT: Lazy<ArcSwapOption<WriteCustodyClient>> = Lazy::new(ArcSwapOption::empty);

/// Install this process's custody AUTHORITY (the stats surface and the
/// cadence task read it).
pub fn install_custody_owner(owner: Arc<WriteCustodyOwner>) {
    OWNER.store(Some(owner));
}

/// Install this process's co-writer client — what turns S4's foreign-home
/// refusal into a remote acquire.
pub fn install_custody_client(client: Arc<WriteCustodyClient>) {
    CLIENT.store(Some(client));
}

/// Uninstall the co-writer client (disarm / unmount / test teardown).
pub fn uninstall_custody_client() {
    CLIENT.store(None);
}

/// Uninstall the authority.
pub fn uninstall_custody_owner() {
    OWNER.store(None);
}

/// The installed authority, if any.
pub fn custody_owner() -> Option<Arc<WriteCustodyOwner>> {
    OWNER.load_full()
}

/// The installed co-writer client, if any.
pub fn custody_client() -> Option<Arc<WriteCustodyClient>> {
    CLIENT.load_full()
}

/// **Validate a peer's allocation-lane raise** against the installed
/// authority's assignment — the check S9's publish owner runs before a remote
/// value reaches a durable record
/// ([`WriteCustodyOwner::check_lane_raise`]).
///
/// With **no authority installed** the raise is refused: a node serving the
/// publish vocabulary without a custody authority has made no lane
/// assignment, so it has nothing to check a lane claim against — and
/// committing an unchecked frontier for an unknown lane is precisely the act
/// that could let two mounts hand out one device offset.
pub fn validate_lane_raise(
    client: &str,
    lease_epoch: u64,
    lane: u16,
    writers: u16,
) -> std::result::Result<(), String> {
    let Some(owner) = custody_owner() else {
        return Err(format!(
            "S9: refusing an allocation-lane raise from '{client}' for lane {lane} of {writers}: \
             this node serves the publish vocabulary but has no custody authority armed, so it \
             made no lane assignment and has nothing to check the claim against. Arm the \
             multi-writer authority (SQUEEZEFS_MULTI_WRITER=1 + SQUEEZEFS_MW_BIND) — a frontier \
             committed for an unverified lane could put two mounts in one residue class"
        ));
    };
    owner.check_lane_raise(client, lease_epoch, lane, writers)
}

/// **S4's foreign-home seam, resolved.**
///
/// S4 counted the round trip and then refused, because *"granting a foreign
/// home locally would be two nodes each believing they hold exclusive
/// custody"*. S9 makes the round trip real: the acquire travels to the
/// home's authority, and what comes back is custody that authority issued.
///
/// With no client armed the refusal stands, and it now names the missing
/// half rather than a future stage.
pub async fn acquire_remote(
    ino: u64,
    span: Option<(u64, u64)>,
    mode: LockMode,
    ttl: Duration,
    slot: u64,
) -> Result<LockLease> {
    let Some(client) = custody_client() else {
        let reason = format!(
            "S9: lock object inode_{ino} homes on slot {slot}, which this node's lock authority \
             does not own, and no remote write-custody client is armed — refusing {span:?} \
             {mode:?} rather than granting custody the owner never issued. Arm the multi-writer \
             mount (SQUEEZEFS_MULTI_WRITER=1 with a PR-capable substrate and a stamped format) \
             so the acquire can travel"
        );
        log::error!("{reason}");
        return Err(SqueezefsError::LockFailed { reason });
    };
    client.acquire(ino, span, mode, ttl).await
}
