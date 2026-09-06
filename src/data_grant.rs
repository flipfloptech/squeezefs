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
///
/// **3 since S11 rung 15** (KD-MW-7, design §9.2/§11): the acquire carries
/// the required/desired pair ([`AcquireFrame::desired`]) and the renewal
/// reply carries the client's live **range vector**
/// ([`RenewReplyFrame::ranges`] — the revalidation surface the client
/// range cache rebuilds from). A peer that speaks 2 would decode a
/// desired-bearing acquire as a plain span (granting less than the law
/// requires) and silently drop the range vector, so the mismatch stays a
/// loud refusal.
///
/// **4 since S11 rung 17** (KD-MW-8's demotion barrier, design §9.3): the
/// renewal reply carries the incumbent's **demotion notices**
/// ([`RenewReplyFrame::demotions`] — composed under the same
/// `FileCustody` serialization that parked the second holder) and the
/// **extent coverage watermarks** ([`RenewReplyFrame::extent_covered`] —
/// the retention release's pull surface), and
/// [`VERB_CUSTODY_DEMOTE_ACK`] joined. (A sub-block grantee needs no
/// demoted-region mark of its own: a grant that does not cover a whole
/// block already classifies that block range-shared — the whole-block
/// re-acquire corner is the rung's named residual.) A 3-speaker would
/// silently drop the notice — the exact two-publishers window the
/// barrier exists to close — so the mismatch stays a loud refusal.
///
/// **5 since §9.3a's tail shrink** (residual board item 7's fix,
/// `.benchmarks/2026-08-19-blob-aware-merge-and-fabric-venue.md` §3): the
/// renewal reply carries the incumbent's **tail-shrink notices**
/// ([`RenewReplyFrame::shrinks`] — composed under the same `FileCustody`
/// serialization that parked the asker, the demotion notice's own race
/// pin), and [`VERB_CUSTODY_SHRINK_ACK`] joined (the incumbent answers
/// its written high-water inside the contested tail). A 4-speaker would
/// silently drop the notice and hold the asker to the incumbent's lease
/// TTL — the retry-amplification shape the fix deletes — so the mismatch
/// stays a loud refusal.
///
/// **6 since finding 16 half (a)** — the notice CARRIER widened
/// (`.benchmarks/2026-08-25-s11-freeloop-stall.md` §Finding 16: 40 of 51
/// shrink notices died with their grant because the only carrier was the
/// renewal reply and, under the block-cyclic interleave, grants live
/// shorter than a renewal cadence): the acquire reply became
/// [`AcquireReplyFrame`] and the release reply [`ReleaseReplyFrame`],
/// each carrying the same demotion/shrink notice sets the renewal reply
/// does, so a churn-shaped incumbent hears within ONE interaction. A
/// 5-speaker would decode the acquire reply's leading schema word as a
/// bare [`GrantRecord`]'s and adopt garbage custody, so the mismatch
/// stays a loud refusal.
/// **7 (finding 27,** `.benchmarks/2026-08-25-s11-freeloop-stall.md`**)**:
/// [`VERB_CUSTODY_NOTICE_POLL`] joined — the STANDING notice poll. Every
/// f16a carrier is a reply on a verb the incumbent must SEND, so a QUIET
/// incumbent (a rank at an MPI barrier) heard a pending demotion only at
/// its renewal cadence and the asker parked its full budget (the
/// shared-phase half-bandwidth dip). The poll is a client-initiated RPC
/// the authority PARKS and answers the instant a notice lands — the §9.3
/// barrier's own vocabulary ("reply-carried on a client-initiated RPC is
/// not a push"), the delegation recall channel's exact shape.
pub const CUSTODY_SCHEMA: u32 = 7;

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
/// Rung 17 (§9.3): the incumbent's demotion ACK — the client-initiated
/// RPC that retires its direct-DMA custody over the demoted block and
/// lets the parked second holder's grant issue.
pub const VERB_CUSTODY_DEMOTE_ACK: u16 = VERB_CUSTODY_BASE + 5;
/// §9.3a: the incumbent's tail-shrink ACK — the client-initiated RPC
/// answering its written high-water inside the contested stretch tail;
/// the authority resolves the pending shrink against it and the parked
/// asker's grant issues (exclusive on an unwritten tail, or through the
/// existing demotion barrier on a written one).
pub const VERB_CUSTODY_SHRINK_ACK: u16 = VERB_CUSTODY_BASE + 6;
/// Finding 27: the STANDING notice poll — parked by the authority,
/// answered the instant a demotion/shrink notice lands for the client
/// (or at the bounded park), so a QUIET incumbent hears at poll latency
/// instead of its renewal cadence. The reply carries the same notice
/// sets every f16a carrier does, gathered under the same `FileCustody`
/// serialization.
pub const VERB_CUSTODY_NOTICE_POLL: u16 = VERB_CUSTODY_BASE + 7;

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
/// Status: the §9.2 bounds refused a NEW range span — the geometry cap,
/// the `dlm_grant_table_bytes` R5 byte budget, or the Red clamp. The body
/// carries the refusal's budget arithmetic verbatim (the fleet-share
/// precedent: refusals name their numbers). Distinct from
/// [`CUSTODY_CONFLICT`] because nothing HOLDS the bytes — the client's
/// remedy is release/backoff, never waiting on a holder.
pub const CUSTODY_AT_CAPACITY: u16 = 0x46;

/// Rung 17: one client's live custody SHAPE on an ino — the
/// custody-scoped full-Put law's input (see
/// [`WriteCustodyOwner::client_custody_on`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientCustodyShape {
    /// No live grants (the pre-custody publish class — Puts apply
    /// verbatim, exactly as every shipped Put did before ranges existed).
    None,
    /// Whole-file custody: Puts stay fully authoritative.
    WholeFile,
    /// Range custody: a shipped full Put is authoritative only INSIDE
    /// these spans' blocks.
    Ranges(Vec<(u64, u64)>),
}

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
            // The custody lease carries no lane-supply hint: the hint rides
            // the MEMBERSHIP renewal (the 1 Hz prodded beat), never this
            // slower lease.
            lane_supply_blocks: 0,
            // Nor the writer's checkpoint ceiling — it rides the membership
            // grant that carries the label it is a promise about.
            checkpoint_ceiling_ms: 0,
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
    /// with **end exclusive**. On the S11 required/desired path this is
    /// **required** — the span the write needs, never trimmed.
    pub span: Option<(u64, u64)>,
    /// `true` = CW (concurrent write); `false` = EX.
    pub concurrent_write: bool,
    /// The caller's own wait budget, ms — clamped by the authority to one
    /// renewal cadence (a client that cannot get custody within a cadence
    /// must be TOLD, not held on a service lane).
    pub wait_ms: u64,
    /// **S11 rung 15** (schema 3): the best-effort desired window ⊇
    /// `span` — block-aligned outward by the client (the §9.2 rounding
    /// doctrine), always trimmable against live custody. `Some` selects
    /// the required/desired admit (EX-only, KD-MW-9); `None` is the plain
    /// S9 acquire, byte-identical to schema 2's semantics.
    pub desired: Option<(u64, u64)>,
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

/// One live range grant as the renewal reply carries it — the **range
/// vector on the lease** (S11 rung 15, design §11: *"custody lease
/// carries optional range vector"*): the authority's own record of this
/// client's live byte-range custody, authoritative for presence AND
/// absence, from which the client range cache rebuilds
/// (`meta_ship::tokens::replace_range_grants`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeVecEntry {
    pub grant_id: u64,
    pub ino: u64,
    /// `[start, end)`, end exclusive — possibly WIDER than any single ask
    /// (the admit-time merge widens grants in place).
    pub span: (u64, u64),
    /// The grant's fencing token (the file's generator — no new algebra).
    pub token: u64,
}

/// Rung 17 (§9.3): one renewal-carried **demotion notice** — a second
/// holder's acquire parked on `region` of `ino`, and THIS client's
/// grant (`incumbent_token`) must quiesce its direct DMA there,
/// re-route to extent-ship, and ACK ([`VERB_CUSTODY_DEMOTE_ACK`]).
/// Composed under the same `FileCustody` serialization that parked the
/// waiter, so a reply composed after the pending-mark always carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemotionNotice {
    pub ino: u64,
    /// The block-aligned region demoting to authority-assembled.
    pub region: (u64, u64),
    /// The addressee grant — this client acks by naming it.
    pub incumbent_token: u64,
}

/// §9.3a (schema 5): one renewal-carried **tail-shrink notice** — a
/// second holder's REQUIRED ask parked wholly inside `incumbent_token`'s
/// desired-minted stretch tail on `ino`, and THIS client is asked to
/// release the tail back to `floor` (block-hulled). The client shrinks
/// its covering cache FIRST (the tail must stop serving before the ack
/// travels — order load-bearing), then answers its written high-water
/// ([`VERB_CUSTODY_SHRINK_ACK`]). Composed under the same `FileCustody`
/// serialization that parked the asker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShrinkNotice {
    pub ino: u64,
    /// The block-hulled boundary the tail is asked to release back to.
    pub floor: u64,
    /// The addressee grant — this client acks by naming it.
    pub incumbent_token: u64,
}

/// A renewal's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewReplyFrame {
    pub schema: u32,
    pub lease: LeaseFrame,
    /// Grants of this client the authority no longer holds (revoked while
    /// the client was away). The pull-based revocation channel.
    pub dead_grants: Vec<u64>,
    /// **S11 rung 15** (schema 3): this client's live RANGE grants — the
    /// lease's range vector (revalidation surface; empty for a client
    /// holding only whole-file custody).
    pub ranges: Vec<RangeVecEntry>,
    /// **Rung 17** (schema 4): the demotion notices addressed to this
    /// client's grants — the §9.3 pull channel (reply-carried on a
    /// client-initiated RPC is not a push).
    pub demotions: Vec<DemotionNotice>,
    /// **Rung 17** (schema 4): per-ino covered witness-id watermarks —
    /// the retention release's renewal-observation surface (`(ino,
    /// covered_upto_request_id)`; the client releases every retained
    /// extent at or below the watermark).
    pub extent_covered: Vec<(u64, u64)>,
    /// **§9.3a** (schema 5): the tail-shrink notices addressed to this
    /// client's grants — the same pull channel the demotion notices ride.
    pub shrinks: Vec<ShrinkNotice>,
}

/// **Schema 6 (finding 16 half (a)): the acquire's answer** — the grant
/// plus the demotion/shrink notices addressed to the asking client's OTHER
/// live grants. The notices are gathered AFTER the acquire's own
/// arbitration completes (same-ino gathers therefore serialize behind the
/// pending-mark on the `FileCustody` entry — the renewal reply's
/// composed-after-the-mark argument, verbatim), so an incumbent whose
/// custody interactions are all acquires still hears a shrink within one
/// interaction instead of one renewal cadence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquireReplyFrame {
    pub schema: u32,
    /// The acquire's own outcome — semantics unchanged from the schema-5
    /// bare record.
    pub grant: GrantRecord,
    /// The §9.3 demotion notices addressed to this client's grants.
    pub demotions: Vec<DemotionNotice>,
    /// The §9.3a tail-shrink notices addressed to this client's grants.
    pub shrinks: Vec<ShrinkNotice>,
}

/// **Schema 6 (finding 16 half (a)): the release's answer** — the retired
/// count plus the same notice sets [`AcquireReplyFrame`] carries (the
/// notices name the client's SURVIVING grants; a notice whose incumbent
/// grant was in the released set resolves through the fence column as
/// before). Replaces the schema-5 raw little-endian count body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseReplyFrame {
    pub schema: u32,
    /// How many of the named grants the authority actually held.
    pub released: u64,
    /// The §9.3 demotion notices addressed to this client's grants.
    pub demotions: Vec<DemotionNotice>,
    /// The §9.3a tail-shrink notices addressed to this client's grants.
    pub shrinks: Vec<ShrinkNotice>,
}

/// Finding 27: the standing notice poll's ask — "park me until a notice
/// lands for my grants (or `park_ms`)".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticePollFrame {
    pub schema: u32,
    pub client: String,
    pub lease_epoch: u64,
    /// The client's requested park bound; the authority clamps it to one
    /// renewal cadence (the S6 venue discipline — no unbounded parks on a
    /// service lane).
    pub park_ms: u64,
}

/// Finding 27: the poll's reply — the same notice sets every f16a
/// carrier bears, gathered under the same `FileCustody` serialization
/// (the composed-after-the-mark argument transfers verbatim). Both
/// vectors empty = the bounded park elapsed quietly; the client re-parks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticePollReply {
    pub schema: u32,
    pub demotions: Vec<DemotionNotice>,
    pub shrinks: Vec<ShrinkNotice>,
    /// The park the authority actually applied (its clamp made visible).
    pub park_ms: u64,
}

/// Rung 17: the incumbent's demotion ack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemoteAckFrame {
    pub schema: u32,
    pub client: String,
    pub lease_epoch: u64,
    pub ino: u64,
    pub incumbent_token: u64,
    pub region: (u64, u64),
}

/// §9.3a: the incumbent's tail-shrink ack — its written high-water inside
/// the grant (`u64::MAX` = "unknown: my cache no longer carries the
/// mark, treat my whole span as potentially written" — the shed-cache
/// degradation, which resolves as an escalation, never a release).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShrinkAckFrame {
    pub schema: u32,
    pub client: String,
    pub lease_epoch: u64,
    pub ino: u64,
    pub incumbent_token: u64,
    /// The max byte end this client served write custody for through the
    /// named grant.
    pub watermark: u64,
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
    /// The objects this client held WHOLE-FILE before the failover.
    pub inos: Vec<u64>,
    /// **Rung 17** (schema 4): the RANGE grants this client held — MW-13's
    /// law needs a range holder to re-assert its ORIGINAL block-aligned
    /// grant (never a silent whole-file widening, which would conflict
    /// with every surviving peer's ranges) in the successor's grace
    /// window. `(ino, [start, end))` per span, re-granted EX.
    pub ranges: Vec<(u64, (u64, u64))>,
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
// S11 rung 15 — the client's wire-face range ledger (the authority-side
// family lives in `crate::dlm::range_custody_stats`).
static RANGE_ACQUIRES_CLIENT: AtomicU64 = AtomicU64::new(0);
/// Finding 27: standing notice-poll rounds this client COMPLETED (parked
/// or answered — the channel-liveness gauge; a co-writer whose custody
/// plane is armed and whose polls stay 0 has a dead channel).
static NOTICE_POLL_ROUNDS: AtomicU64 = AtomicU64::new(0);
/// Finding 27: notices ABSORBED via the standing poll (the quiet-incumbent
/// engagement instrument — the renewal remains the worst-case carrier, so
/// growth here is the poll beating the cadence).
static NOTICE_POLL_NOTICES: AtomicU64 = AtomicU64::new(0);
static RANGE_EXTENSIONS_CLIENT: AtomicU64 = AtomicU64::new(0);
/// Finding 34 (rung 1): ranged release verbs DEFERRED by the release gate
/// because their ino's publish pipeline was not yet quiescent — each one
/// is the ordering fix engaging (the verb re-queues and departs on a
/// later drain, behind the flush the gate kicked). Sustained growth with
/// releases flat means a flush that never completes (read it beside the
/// writeback error latches).
static RELEASES_DEFERRED: AtomicU64 = AtomicU64::new(0);

/// Finding 34 (rung 1): the RELEASE GATE — answers whether `ino`'s
/// publish pipeline is QUIESCENT (synchronously, lock-order-free: the
/// drain runs under the acquire path's order-2 `lease_locks` stripe, so
/// the gate may only PROBE, never take order-1 locks or await device
/// I/O). A `false` verdict both defers the ino's queued releases and is
/// the gate's cue to KICK a detached flush; `token` is the newest token
/// among the ino's releasing grants — the flush-kick's fencing hint.
pub type ReleaseGateHook = Arc<dyn Fn(u64, u64) -> bool + Send + Sync>;

static RELEASE_GATE: Lazy<arc_swap::ArcSwapOption<ReleaseGateHook>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the finding-34 release gate (the co-writer mount arm's act; a
/// re-arm replaces).
pub fn install_release_gate(hook: ReleaseGateHook) {
    RELEASE_GATE.store(Some(Arc::new(hook)));
}

/// Uninstall the release gate (unmount teardown / tests).
pub fn uninstall_release_gate() {
    RELEASE_GATE.store(None);
}

fn release_gate() -> Option<ReleaseGateHook> {
    RELEASE_GATE.load_full().map(|h| h.as_ref().clone())
}

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
        // S11 rung 15 — the client's wire-face range ledger (the
        // authority-side family is the `range_custody` object).
        "dlm_custody_range_acquires": RANGE_ACQUIRES_CLIENT.load(Ordering::Relaxed),
        "dlm_custody_range_extensions": RANGE_EXTENSIONS_CLIENT.load(Ordering::Relaxed),
        // Finding 34 (rung 1): release verbs deferred behind the ino's
        // publish drain — the ordering fix's engagement gauge.
        "dlm_custody_releases_deferred": RELEASES_DEFERRED.load(Ordering::Relaxed),
        // Finding 27: the standing notice poll (rounds = channel
        // liveness; notices = the quiet-incumbent engagement).
        "dlm_custody_notice_polls": NOTICE_POLL_ROUNDS.load(Ordering::Relaxed),
        "dlm_custody_notice_poll_notices": NOTICE_POLL_NOTICES.load(Ordering::Relaxed),
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

/// The S11 lever: ENG-10 `Kind::Bool`, read only when the mw plane is
/// armed — the `SQUEEZEFS_DELEGATION` form. **Static default OFF until
/// rungs 16/17 land** (adjudicated 2026-08-17 against §11's provisional
/// default-on: the concurrent same-ino layout-publish composition is
/// their machinery — see the registry entry and the rung-15 evidence
/// note); set-but-unarmed is announced-inert, never a refusal.
pub const RANGE_CUSTODY_ENV: &str = "SQUEEZEFS_RANGE_CUSTODY";

/// **Test seam** (the `TEST_DELEGATION_OVERRIDE` precedent): `0` = read
/// the env knob, `1` = force on, `2` = force off — so one suite binary can
/// pin both sides of the A/B without racing process-global env mutation.
pub static TEST_RANGE_CUSTODY_OVERRIDE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

/// Is the range-custody plane live on this process? One relaxed load on
/// every unarmed mount (the solo re-gate's law) — the knob is consulted
/// only past the armed gate, which is what keeps KD-MW-12's whole-file
/// fast path structurally untouched everywhere the plane is dark.
pub fn range_custody_enabled() -> bool {
    match TEST_RANGE_CUSTODY_OVERRIDE.load(Ordering::Relaxed) {
        1 => return crate::meta_ship::ownership_armed(),
        2 => return false,
        _ => {}
    }
    crate::meta_ship::ownership_armed() && crate::env_knobs::bool_knob(RANGE_CUSTODY_ENV, false)
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

/// The per-file geometry source the §9.2 span cap derives from —
/// `(file_size_bytes, block_size)` for an ino, behind a trait so the
/// custody authority never needs a metadata backend of its own (the
/// [`CustodyQuarantine`] pattern: the mount arm supplies the real
/// metadata lookup; a test supplies a fixed shape; an authority with no
/// source runs the byte budget alone — `None` here, because a conjured
/// default size would be exactly the constant Issue-19 forbids).
pub trait RangeGeometry: Send + Sync + std::fmt::Debug {
    /// The file's `(size, block_size)`, or `None` when the ino cannot be
    /// resolved (the cap arm then stands down for this ask; the byte
    /// budget still governs).
    fn geometry(&self, ino: u64) -> Pin<Box<dyn Future<Output = Option<(u64, u64)>> + Send + '_>>;
}

/// A fixed-shape [`RangeGeometry`]: every ino reads as one `size`-byte
/// file of `block_size`-byte blocks — the test seam, and the honest
/// answer for single-file rigs.
pub fn fixed_range_geometry(size: u64, block_size: u64) -> Arc<dyn RangeGeometry> {
    #[derive(Debug)]
    struct Fixed {
        size: u64,
        block_size: u64,
    }
    impl RangeGeometry for Fixed {
        fn geometry(
            &self,
            _ino: u64,
        ) -> Pin<Box<dyn Future<Output = Option<(u64, u64)>> + Send + '_>> {
            let out = Some((self.size, self.block_size));
            Box::pin(async move { out })
        }
    }
    Arc::new(Fixed { size, block_size })
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

// The lease/grant records themselves (`LeaseState` / `GrantEntry`) live in
// [`crate::grant_table_core`] — spec §6.9's `grant_table_core` loom
// obligation. The authority's OWN lease rides each grant entry: dropping
// it is what makes the bytes grantable again — so a revoke is structurally
// a drop, not a bookkeeping edit — and it is also the grant's TOKEN of
// record: [`WriteCustodyOwner::grants_snapshot`] reads it from there
// rather than from a copy, so an audit can never disagree with the custody
// that is actually held.

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
    /// The grant table — client leases, live grants, the grace window and
    /// the two mint words ([`crate::grant_table_core`], loom-modeled).
    table: crate::grant_table_core::GrantTableCore<LockLease>,
    /// S11 rung 15: the §9.2 span-cap geometry source (`None` = no source
    /// installed — the byte budget alone governs; the mount arm installs
    /// the metadata-backed lookup, tests a fixed shape). A lock, not an
    /// ArcSwap: read once per RANGED acquire (an episode event), never on
    /// a hot path.
    geometry: parking_lot::RwLock<Option<Arc<dyn RangeGeometry>>>,
    quarantine: Option<Arc<dyn CustodyQuarantine>>,
    /// The lane-harvest HANDOUT ledger (rung 10, residual 2): offsets this
    /// authority handed a co-writer's lease out of its own free list, keyed
    /// by lease epoch, undischarged. Merged into the death cohort at
    /// [`Self::finish_kill`] (the §3.1 zombie window for REUSED offsets),
    /// discharged when the offset's next shipped free returns it to this
    /// authority's own ladder. Bounded by the peer's live working set, not
    /// by time: handout → publish → re-free cycles discharge.
    handouts: parking_lot::Mutex<std::collections::HashMap<u64, std::collections::BTreeSet<u64>>>,
    granted: AtomicU64,
    released: AtomicU64,
    conflicts: AtomicU64,
    renewals: AtomicU64,
    revokes: AtomicU64,
    expiries: AtomicU64,
    reclaims: AtomicU64,
    grace_conflicts: AtomicU64,
    unknown_leases: AtomicU64,
    /// Finding 27: the standing notice polls park here; the dlm barrier's
    /// pending-mark hook wakes every parked poll (wake-all is deliberate —
    /// each poll re-gathers ITS client's notices and re-parks on empty).
    notice_notify: Arc<squeezefs_ipc::sqz_notify::Notify>,
}

impl std::fmt::Debug for WriteCustodyOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteCustodyOwner")
            .field("id", &self.id)
            .field("term", &self.term)
            .field("clients", &self.table.clients_len())
            .field("grants", &self.table.grants_len())
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
        // Finding 27: the barrier's pending-mark wakes every parked
        // notice poll (a process-global hook — the arbiter's mark sites
        // live below this module and cannot name the owner).
        let notice_notify = Arc::new(squeezefs_ipc::sqz_notify::Notify::new());
        {
            let notify = Arc::clone(&notice_notify);
            crate::dlm::install_range_pending_hook(Arc::new(move || {
                notify.notify_waiters();
            }));
        }
        Ok(Arc::new(Self {
            id: id.to_string(),
            term,
            clocks,
            clock,
            arbiter: LocalLockManager::new()?,
            lanes: ArcSwapOption::empty(),
            table: crate::grant_table_core::GrantTableCore::new(),
            geometry: parking_lot::RwLock::new(None),
            quarantine,
            handouts: parking_lot::Mutex::new(std::collections::HashMap::new()),
            granted: AtomicU64::new(0),
            released: AtomicU64::new(0),
            conflicts: AtomicU64::new(0),
            renewals: AtomicU64::new(0),
            revokes: AtomicU64::new(0),
            expiries: AtomicU64::new(0),
            reclaims: AtomicU64::new(0),
            grace_conflicts: AtomicU64::new(0),
            unknown_leases: AtomicU64::new(0),
            notice_notify,
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

    /// Install the §9.2 span-cap geometry source (S11 rung 15 — the
    /// `install_lane_assignment` pattern: post-arm, no signature change).
    /// Without one the per-file cap stands down and the byte budget alone
    /// bounds the table (documented in [`RangeGeometry`]).
    pub fn install_range_geometry(&self, source: Arc<dyn RangeGeometry>) {
        *self.geometry.write() = Some(source);
    }

    /// Live grants.
    pub fn held(&self) -> usize {
        self.table.grants_len()
    }

    /// Every live grant, ordered by grant id — the audit surface (see
    /// [`GrantAudit`]). Bounded by live custody, never by grants ever
    /// issued.
    pub fn grants_snapshot(&self) -> Vec<GrantAudit> {
        self.table.grants_snapshot_with(|id, g| GrantAudit {
            grant_id: id,
            client: g.client.clone(),
            lease_epoch: g.lease_epoch,
            ino: g.ino,
            span: g.span,
            token: g.lease.fencing_token(),
        })
    }

    /// KD-MW-16 (rung 10c, design-mw-fleet-jobs §6): does ANY live grant
    /// cover `ino`? The §5.7 mover quiescence probe's custody arm — a
    /// mover must never republish bytes a co-writer may be DMAing under
    /// custody. Whole-inode by design: span-granularity deferral would
    /// buy little (movers re-plan on a cadence) and cost a range check
    /// this table does not index.
    pub fn ino_granted(&self, ino: u64) -> bool {
        self.table
            .grants_snapshot_with(|_, g| g.ino == ino)
            .into_iter()
            .any(|hit| hit)
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
            held: self.table.grants_len() as u64,
            clients: self.table.clients_len() as u64,
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

    /// Record lane-harvest handouts against `lease_epoch` (rung 10,
    /// residual 2 — the harvest executor's act, after it removed the
    /// offsets from its own free list): an epoch that dies before the
    /// handed-out offset's reference lands durably takes it into the death
    /// cohort, exactly like a declared in-flight destination.
    pub fn note_lane_handouts(&self, lease_epoch: u64, offsets: &[u64]) {
        if offsets.is_empty() {
            return;
        }
        self.handouts
            .lock()
            .entry(lease_epoch)
            .or_default()
            .extend(offsets.iter().copied());
    }

    /// Discharge handouts: `offsets` came back under this authority's own
    /// ladder (the shipped free that returned them), so they are no longer
    /// any epoch's reallocation hazard.
    pub fn discharge_lane_handouts(&self, offsets: &[u64]) {
        if offsets.is_empty() {
            return;
        }
        let mut map = self.handouts.lock();
        map.retain(|_, set| {
            for off in offsets {
                set.remove(off);
            }
            !set.is_empty()
        });
    }

    /// The undischarged handouts of `lease_epoch` (test/probe surface).
    pub fn undischarged_handouts(&self, lease_epoch: u64) -> Vec<u64> {
        self.handouts
            .lock()
            .get(&lease_epoch)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// **Validate a shipped displaced-block FREE** (the co-writer free
    /// path's era gate, served in [`crate::meta_ship::publish`]): the
    /// presented lease epoch must be LIVE custody on this authority.
    ///
    /// Keyed on the **epoch alone**, deliberately — three reasons, each
    /// load-bearing:
    ///
    /// * the epoch IS the witness: this authority minted it, it is
    ///   monotone and never reused, and it travelled only to the member it
    ///   names over the authenticated wire — while the publish frame's
    ///   `client` string is documented *"logs and audit only"*, so keying
    ///   the gate on it would authenticate against a self-assertion;
    /// * frees are **lane-blind** (the partition's own law: `b % W`
    ///   derives the owner), so unlike the lane raise there is no
    ///   per-client assignment to match the caller against — ANY member
    ///   holding live custody may release a reference its publish already
    ///   dropped, exactly as any local writer may free a peer's block;
    /// * the threat the gate exists for is the FENCED mount: a revoked or
    ///   swept lease's epoch is out of this map, so its in-flight frees
    ///   refuse by era regardless of what the frame claims about itself.
    ///
    /// `Err(reason)` is the operator-facing refusal text (`client` is the
    /// audit string it names).
    pub fn check_free(&self, client: &str, lease_epoch: u64) -> std::result::Result<(), String> {
        if !self.table.epoch_live(lease_epoch) {
            return Err(format!(
                "S9: refusing a displaced-block free from '{client}': lease epoch {lease_epoch} \
                 is not custody on authority '{}' (revoked, swept past its TTL, or minted by a \
                 previous era). The mount must self-fence and re-join; its unfreed displaced \
                 blocks stay durably unreferenced and the next derivation (mount recovery / \
                 fsck C6) returns them to the free supply — the leak-safe direction, never a \
                 free executed for a fenced era",
                self.id
            ));
        }
        Ok(())
    }

    /// **Validate a shipped MUTATING publish verb's era** (finding #6,
    /// design-mw-layout-versions §6a): the presented lease epoch must be
    /// LIVE custody on this authority. The same law as [`Self::check_free`]
    /// — keyed on the epoch alone, for the three reasons stated there —
    /// with the refusal text naming the publish plane: a swept-but-not-yet-
    /// self-fenced zombie's layout publishes were the divergent-chain mint
    /// the s9-colocated-fence leg convicted, and a refusal here means
    /// NOTHING was applied.
    pub fn check_publish_era(
        &self,
        client: &str,
        lease_epoch: u64,
    ) -> std::result::Result<(), String> {
        if !self.table.epoch_live(lease_epoch) {
            return Err(format!(
                "S9: refusing a shipped publish from '{client}': lease epoch {lease_epoch} is \
                 not custody on authority '{}' (revoked, swept past its TTL, or minted by a \
                 previous era) — a fenced era's publish is the divergent-chain mint (spec §6.2 \
                 item 9; design-mw-layout-versions §6a). Nothing was applied; the mount must \
                 self-fence and re-join by remount, and its acked-un-fsynced staged work is \
                 the POSIX crash class the remount contract owns",
                self.id
            ));
        }
        Ok(())
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
        // A re-join REPLACES the prior lease (same identity, new epoch):
        // the client is telling us it lost its view, and keeping the old
        // epoch alive would leave custody nobody presents. Its GRANTS are
        // revoked with it — a client that cannot present its lease cannot
        // present its grants either, and bytes nobody can name must become
        // grantable. The mint / replace / kill-before-new-lease order is
        // the core's ([`crate::grant_table_core::GrantTableCore::join`],
        // loom-modeled): the dead epoch's quarantine runs BEFORE the new
        // lease becomes visible.
        let epoch = self.table.join(
            &req.client,
            req.pr_key,
            req.prior_epoch,
            now,
            self.clocks.t_owner.as_millis() as u64,
            |killed| {
                self.revokes.fetch_add(1, Ordering::Relaxed);
                REVOKES.fetch_add(1, Ordering::Relaxed);
                let prior = killed.lease.epoch;
                self.finish_kill(
                    &req.client,
                    killed,
                    &format!("re-joined with a fresh lease (prior epoch {prior})"),
                );
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
        self.table.lease_current(client, epoch)
    }

    /// A client's OWNER-side deadline in this clock's milliseconds — the
    /// instant after which the authority may grant its bytes elsewhere. The
    /// contract the client's stricter deadline is measured against.
    pub fn lease_deadline_ms(&self, client: &str) -> Option<u64> {
        self.table.lease_deadline_ms(client)
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
        let token = lease.fencing_token();
        // Validate-and-commit as ONE step under the table's own
        // linearization — never check-then-commit across the arbitration
        // await above: a lease that died while this acquire was parked
        // (revoked, or swept by the mw_custody_sweep cadence task)
        // retires the acquisition as a CONFLICT, and dropping the
        // arbiter lease keeps the bytes grantable — custody is never
        // committed under a dead epoch. Found by the rung-4
        // grant_table_core loom model BEFORE any fleet armed (S9 dark);
        // pinned by `a_granted_custody_never_outlives_its_lease` and the
        // cargo repro `a_revoke_during_a_parked_acquire_never_commits_custody`.
        let grant_id = match self.table.commit_grant_if_current(
            &req.client,
            req.lease_epoch,
            req.ino,
            req.span,
            lease,
        ) {
            Ok(id) => id,
            Err(dead_lease) => {
                drop(dead_lease);
                self.conflicts.fetch_add(1, Ordering::Relaxed);
                CONFLICTS.fetch_add(1, Ordering::Relaxed);
                log::warn!(
                    "S9: refusing custody of inode_{} {:?} to '{}': its lease epoch {} died \
                     while the acquire was parked in arbitration (revoked or swept mid-await) \
                     — the acquisition retires as a conflict and the client must self-fence \
                     and re-join",
                    req.ino,
                    req.span,
                    req.client,
                    req.lease_epoch
                );
                return Err(CUSTODY_CONFLICT);
            }
        };
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

    /// **S11 rung 15 — the required/desired grant** (KD-MW-7, §9.2): the
    /// range face of [`Self::grant`], carrying the refusal DETAIL so the
    /// budget arithmetic reaches the refused client verbatim (the
    /// fleet-share precedent — a `u16` alone would strand the numbers on
    /// the authority's log).
    ///
    /// The arbitration is [`LocalLockManager::acquire_lock_range_scoped`]
    /// under the client-lease merge scope
    /// ([`crate::dlm::range_scope_for_epoch`]): one scope per lease, so
    /// one client's adjacent stripes coalesce and two clients' never do.
    /// An EXTENSION answers the client's EXISTING grant id with the
    /// widened span and the SURVIVING token — the client widens its
    /// adopted record and its cached span; nothing re-mints, so every
    /// in-flight write fencing on that grant stays current.
    pub async fn grant_ranged(
        &self,
        req: &AcquireFrame,
        desired: (u64, u64),
    ) -> std::result::Result<GrantRecord, (u16, String)> {
        let named = |status: u16| (status, status_name(status).to_string());
        if req.schema != CUSTODY_SCHEMA {
            return Err(named(CUSTODY_SCHEMA_MISMATCH));
        }
        if req.concurrent_write {
            // KD-MW-9: v1 issues EX only — the desired path never carries
            // a mode.
            return Err((
                CUSTODY_MALFORMED,
                "S11: the required/desired path issues EX only (KD-MW-9's v1 issuance law) \
                 — a CW range has no issuer"
                    .to_string(),
            ));
        }
        let Some(required) = req.span else {
            return Err((
                CUSTODY_MALFORMED,
                "S11: a desired window without a required span — required is the \
                 never-trimmed floor and must be present"
                    .to_string(),
            ));
        };
        if !self.lease_current(&req.client, req.lease_epoch) {
            self.unknown_leases.fetch_add(1, Ordering::Relaxed);
            UNKNOWN_LEASES.fetch_add(1, Ordering::Relaxed);
            return Err(named(CUSTODY_UNKNOWN_LEASE));
        }
        if self.in_grace() {
            self.grace_conflicts.fetch_add(1, Ordering::Relaxed);
            GRACE_CONFLICTS.fetch_add(1, Ordering::Relaxed);
            return Err(named(CUSTODY_IN_GRACE));
        }
        // The §9.2 cap's geometry, resolved through the installed source
        // (None = no source, the byte budget alone governs — a conjured
        // size would be the Issue-19 constant).
        let geometry = {
            let source = self.geometry.read().clone();
            match source {
                Some(g) => g.geometry(req.ino).await,
                None => None,
            }
        };
        let scope = crate::dlm::range_scope_for_epoch(req.lease_epoch);
        let budget = Duration::from_millis(req.wait_ms).min(self.clocks.renew_interval);
        let path = format!("inode_{}", req.ino);
        let t_arb = Instant::now();
        let outcome = self
            .arbiter
            .acquire_lock_range_scoped(&path, required, desired, budget, geometry, Some(scope))
            .await;
        phase_record(CustodyPhase::Arbitrate, t_arb);
        let record = |grant_id: u64, span: (u64, u64), token: u64| GrantRecord {
            schema: CUSTODY_SCHEMA,
            grant_id,
            ino: req.ino,
            span: Some(span),
            token,
            term: self.term,
            custody_epoch: crate::dlm::compose_token(self.term, req.lease_epoch),
        };
        match outcome {
            Ok(crate::dlm::RangeAcquired::New { lease, span }) => {
                let token = lease.fencing_token();
                // Validate-and-commit as ONE step (the rung-4 loom
                // finding's law — see `grant`).
                match self.table.commit_grant_if_current(
                    &req.client,
                    req.lease_epoch,
                    req.ino,
                    Some(span),
                    lease,
                ) {
                    Ok(grant_id) => {
                        self.granted.fetch_add(1, Ordering::Relaxed);
                        Ok(record(grant_id, span, token))
                    }
                    Err(dead_lease) => {
                        drop(dead_lease);
                        self.conflicts.fetch_add(1, Ordering::Relaxed);
                        CONFLICTS.fetch_add(1, Ordering::Relaxed);
                        Err(named(CUSTODY_CONFLICT))
                    }
                }
            }
            Ok(
                crate::dlm::RangeAcquired::Extended { token, span }
                | crate::dlm::RangeAcquired::Covered { token, span },
            ) => {
                // The widened/covering grant is one THIS CLIENT already
                // holds (the merge scope is its lease) — answer its
                // existing id with the current span. A miss means a
                // racing revoke retired the (already-widened) record with
                // the lease: the whole union was freed together, and the
                // client must self-fence and re-join — the same answer
                // `grant`'s commit refusal gives.
                match self.grant_id_by_token(&req.client, token) {
                    Some(grant_id) => {
                        self.table.widen_grant_span(grant_id, Some(span));
                        Ok(record(grant_id, span, token))
                    }
                    None => {
                        self.conflicts.fetch_add(1, Ordering::Relaxed);
                        CONFLICTS.fetch_add(1, Ordering::Relaxed);
                        Err(named(CUSTODY_CONFLICT))
                    }
                }
            }
            Err(e) => {
                let reason = e.to_string();
                let status = if crate::dlm::is_range_capacity_refusal(&reason) {
                    CUSTODY_AT_CAPACITY
                } else {
                    self.conflicts.fetch_add(1, Ordering::Relaxed);
                    CONFLICTS.fetch_add(1, Ordering::Relaxed);
                    CUSTODY_CONFLICT
                };
                log::warn!(
                    "S11: refusing range custody of inode_{} [{},{}) to '{}' ({}): {reason}",
                    req.ino,
                    required.0,
                    required.1,
                    req.client,
                    status_name(status)
                );
                Err((status, reason))
            }
        }
    }

    /// Rung 17 (the custody-scoped full Put): `client`'s live custody
    /// SHAPE on `ino` — `None` (no grants: the pre-custody publish
    /// class), `WholeFile` (fully authoritative Puts), or the live range
    /// spans. O(live grants), an episode operation.
    pub fn client_custody_on(&self, client: &str, ino: u64) -> ClientCustodyShape {
        let mut spans: Vec<(u64, u64)> = Vec::new();
        let mut whole = false;
        for g in self
            .table
            .grants_snapshot_with(|_, g| (g.client == client && g.ino == ino).then_some(g.span))
        {
            match g {
                Some(None) => whole = true,
                Some(Some(span)) => spans.push(span),
                None => {}
            }
        }
        if whole {
            ClientCustodyShape::WholeFile
        } else if spans.is_empty() {
            ClientCustodyShape::None
        } else {
            ClientCustodyShape::Ranges(spans)
        }
    }

    /// Finding 34: does ANY holder carry a live RANGE grant on `ino`?
    /// The custody-less-Put shield's predicate (`custody_scoped_layout`):
    /// a grant-less client's full Put on an ino with live ranged holders
    /// is refused rather than applied verbatim — verbatim it reverts the
    /// live holders' entries to the shipper's stale base while both refs
    /// streams land (the s11-blockcyclic C8 mint). O(live grants).
    pub fn ino_has_range_grants(&self, ino: u64) -> bool {
        self.table
            .grants_snapshot_with(|_, g| (g.ino == ino && g.span.is_some()).then_some(()))
            .into_iter()
            .any(|hit| hit.is_some())
    }

    /// The installed §9.2 geometry source's answer for `ino` (`None` = no
    /// source / unresolvable — the scoping arm then stands down).
    pub async fn geometry_of(&self, ino: u64) -> Option<(u64, u64)> {
        let source = self.geometry.read().clone();
        match source {
            Some(g) => g.geometry(ino).await,
            None => None,
        }
    }

    /// The grant id of `client`'s live grant carrying `token` (tokens are
    /// globally unique, so an answer names exactly one grant). O(live
    /// grants) — an episode operation, bounded by the §9.2 caps.
    fn grant_id_by_token(&self, client: &str, token: u64) -> Option<u64> {
        self.table
            .grants_snapshot_with(|id, g| {
                (g.client == client && g.lease.fencing_token() == token).then_some(id)
            })
            .into_iter()
            .flatten()
            .next()
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
        let dead_grants = self.table.renew(client, lease_epoch, now, ttl, inflight);
        let Some(dead_grants) = dead_grants else {
            self.unknown_leases.fetch_add(1, Ordering::Relaxed);
            UNKNOWN_LEASES.fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "S9: co-writer '{client}' presented lease epoch {lease_epoch}, which is not \
                 custody on authority '{}' (revoked, swept past its TTL, or granted by a \
                 previous authority) — it must self-fence and re-join",
                self.id
            );
            return Err(CUSTODY_UNKNOWN_LEASE);
        };
        self.renewals.fetch_add(1, Ordering::Relaxed);
        // S11 rung 15: the lease's RANGE VECTOR — this client's live range
        // grants, from the authority's own table (authoritative for
        // presence AND absence; the client cache rebuilds from it).
        // O(live grants) on the heartbeat cadence — bounded by the §9.2
        // caps; a per-client index is the 1-TiB-shape residual, priced
        // when rung 18's rows demand it.
        let ranges = self.range_entries_of(client);
        let (demotions, shrinks) = Self::notices_for(&ranges);
        // Rung 17: the coverage watermarks — the retention release's
        // renewal-observation surface (pull only).
        let extent_covered = crate::extent_ship::owner_covered_watermarks(client);
        Ok(RenewReplyFrame {
            schema: CUSTODY_SCHEMA,
            lease: self.lease_frame(client, lease_epoch, now),
            dead_grants,
            ranges,
            demotions,
            extent_covered,
            shrinks,
        })
    }

    /// This client's live RANGE grants from the authority's own table —
    /// the renewal reply's range vector and every notice gather's address
    /// book. O(live grants), bounded by the §9.2 caps.
    fn range_entries_of(&self, client: &str) -> Vec<RangeVecEntry> {
        self.table
            .grants_snapshot_with(|id, g| {
                (g.client == client)
                    .then(|| {
                        g.span.map(|span| RangeVecEntry {
                            grant_id: id,
                            ino: g.ino,
                            span,
                            token: g.lease.fencing_token(),
                        })
                    })
                    .flatten()
            })
            .into_iter()
            .flatten()
            .collect()
    }

    /// The §9.3 demotion + §9.3a tail-shrink notices addressed to
    /// `entries`' grants. The reads serialize on the SAME `FileCustody`
    /// entry the pending-mark mutated (the arbiter's LOCK_MAP), so a reply
    /// composed after the mark ALWAYS carries the notice — the
    /// in-flight-renewal race pin's mechanism, shared verbatim by every
    /// carrier since finding 16 half (a) widened the set to the acquire
    /// and release replies.
    fn notices_for(entries: &[RangeVecEntry]) -> (Vec<DemotionNotice>, Vec<ShrinkNotice>) {
        let mut demotions = Vec::new();
        let mut shrinks = Vec::new();
        for entry in entries {
            for region in crate::dlm::demotion_notices_for(entry.ino, entry.token) {
                demotions.push(DemotionNotice {
                    ino: entry.ino,
                    region,
                    incumbent_token: entry.token,
                });
            }
            if let Some(floor) = crate::dlm::shrink_notice_for(entry.ino, entry.token) {
                shrinks.push(ShrinkNotice {
                    ino: entry.ino,
                    floor,
                    incumbent_token: entry.token,
                });
            }
        }
        (demotions, shrinks)
    }

    /// Finding 16 half (a): the notice sets addressed to `client`'s live
    /// grants — the acquire/release replies' gather (the renewal builds
    /// its own entries because it ships them as the range vector too).
    fn notices_for_client(&self, client: &str) -> (Vec<DemotionNotice>, Vec<ShrinkNotice>) {
        Self::notices_for(&self.range_entries_of(client))
    }

    /// Rung 17 (§9.3): serve one demotion ACK — validate the lease, mark
    /// the pending acked (the region reads DEMOTED from here on), wake
    /// the parked waiter. `Err(status)` mirrors [`Self::renew`]'s law.
    pub fn demote_ack(
        &self,
        client: &str,
        lease_epoch: u64,
        ino: u64,
        incumbent_token: u64,
        region: (u64, u64),
    ) -> std::result::Result<bool, u16> {
        if !self.lease_current(client, lease_epoch) {
            self.unknown_leases.fetch_add(1, Ordering::Relaxed);
            UNKNOWN_LEASES.fetch_add(1, Ordering::Relaxed);
            return Err(CUSTODY_UNKNOWN_LEASE);
        }
        Ok(crate::dlm::ack_demotion(ino, incumbent_token, region))
    }

    /// §9.3a: serve one tail-shrink ACK — validate the lease, resolve the
    /// pending against the client's written high-water, and reflect the
    /// arbiter's shrink into the OWNER's own grant record so the next
    /// renewal's range vector cannot resurrect the released tail into the
    /// client's cache. `Err(status)` mirrors [`Self::renew`]'s law.
    pub fn shrink_ack(
        &self,
        client: &str,
        lease_epoch: u64,
        ino: u64,
        incumbent_token: u64,
        watermark: u64,
    ) -> std::result::Result<crate::dlm::ShrinkResolution, u16> {
        if !self.lease_current(client, lease_epoch) {
            self.unknown_leases.fetch_add(1, Ordering::Relaxed);
            UNKNOWN_LEASES.fetch_add(1, Ordering::Relaxed);
            return Err(CUSTODY_UNKNOWN_LEASE);
        }
        let resolution = crate::dlm::ack_tail_shrink(ino, incumbent_token, watermark);
        let new_end = match resolution {
            crate::dlm::ShrinkResolution::None => None,
            crate::dlm::ShrinkResolution::Shrunk { floor } => Some(floor),
            crate::dlm::ShrinkResolution::Demoted { new_end } => Some(new_end),
        };
        if let Some(new_end) = new_end {
            if let Some(grant_id) = self.grant_id_by_token(client, incumbent_token) {
                let span = self
                    .table
                    .grants_snapshot_with(|id, g| (id == grant_id).then_some(g.span))
                    .into_iter()
                    .flatten()
                    .next()
                    .flatten();
                if let Some((start, end)) = span {
                    self.table
                        .widen_grant_span(grant_id, Some((start, new_end.min(end))));
                }
            }
        }
        Ok(resolution)
    }

    /// Retire named grants at the client's request. Dropping the
    /// authority's own lease is what makes the bytes grantable again.
    pub fn release(&self, client: &str, grant_ids: &[u64]) -> usize {
        let n = self.table.release(client, grant_ids);
        if n > 0 {
            self.released.fetch_add(n as u64, Ordering::Relaxed);
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
        let Some(killed) = self.table.revoke(client) else {
            return Vec::new();
        };
        self.revokes.fetch_add(1, Ordering::Relaxed);
        REVOKES.fetch_add(1, Ordering::Relaxed);
        Vec::from_iter(self.finish_kill(client, killed, reason))
    }

    /// Sweep every client lease past the authority's TTL (§6.7 "Recovery",
    /// client-failure half). Runs on the arm's cadence task, never on a
    /// handler lane.
    pub fn expire_due(&self) -> Vec<DeadCustody> {
        let now = self.clock.now_ms();
        // `>=` inside the scan: the lease expires AT the deadline, which is
        // the instant the client's own (strictly earlier) deadline was
        // measured against.
        let due = self.table.expire_scan(now);
        let mut out = Vec::new();
        for id in due {
            // Pop ownership (`revoke` removes the lease or answers None):
            // a racing revoke path retires this client exactly once.
            let Some(killed) = self.table.revoke(&id) else {
                continue;
            };
            self.expiries.fetch_add(1, Ordering::Relaxed);
            EXPIRIES.fetch_add(1, Ordering::Relaxed);
            out.extend(self.finish_kill(
                &id,
                killed,
                &format!(
                    "lease TTL {:?} expired without a renewal",
                    self.clocks.t_owner
                ),
            ));
        }
        out
    }

    /// The common death path's daemon half: the table half
    /// ([`crate::grant_table_core::GrantTableCore::revoke`]) already
    /// removed the lease and dropped the retired grants' own leases
    /// (releasing the bytes — the grant's client is told at its next
    /// renewal, the pull-based revocation channel, bounded by its own
    /// T_self); here the dead epoch is minted and the declared
    /// destinations enter the S7 quarantine.
    fn finish_kill(
        &self,
        client: &str,
        killed: crate::grant_table_core::Killed,
        reason: &str,
    ) -> Option<DeadCustody> {
        let crate::grant_table_core::Killed { lease, grant_ids } = killed;
        let grants = grant_ids;
        let epoch = crate::data_custody::declare_dead_epoch(&format!(
            "S9: co-writer '{client}' custody revoked by authority '{}' ({reason})",
            self.id
        ));
        // The death cohort: the client's DECLARED in-flight set, plus every
        // undischarged lane-harvest handout of this epoch (rung 10 —
        // offsets handed out of our own free list whose references never
        // landed durably: a fenced-but-live holder may still be DMA-ing
        // into them, and derived recovery would call them free).
        let mut cohort = lease.inflight.clone();
        if let Some(handed) = self.handouts.lock().remove(&lease.epoch) {
            for off in handed {
                if !cohort.contains(&off) {
                    cohort.push(off);
                }
            }
        }
        let admitted = match (&self.quarantine, cohort.is_empty()) {
            (Some(sink), false) => sink.quarantine(&cohort, epoch),
            (None, false) => {
                log::error!(
                    "S9: co-writer '{client}' died holding {} in-flight/handed-out offset(s) but \
                     this authority has no quarantine sink — those offsets are NOT protected \
                     from reallocation. A data-plane authority must be armed with one",
                    cohort.len()
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
            offsets: cohort,
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
        let awaiting = self.table.open_grace(until, expected);
        log::warn!(
            "S9: custody authority '{}' opened a failover grace window for {:?}: reclaim only, \
             conflicting fresh acquires refused; awaiting re-assertion from {awaiting} prior \
             co-writer(s) (without the window, failover is a cluster-wide forced-flush storm \
             — spec §6.7)",
            self.id,
            self.clocks.grace,
        );
    }

    /// `true` ⇔ the grace window is open (it closes on full re-assertion or
    /// at its deadline, whichever comes first).
    pub fn in_grace(&self) -> bool {
        match self.table.probe_grace(self.clock.now_ms()) {
            crate::grant_table_core::GraceProbe::Closed => false,
            crate::grant_table_core::GraceProbe::ClosedNow { never_reasserted } => {
                log::info!(
                    "S9: custody authority '{}' closed its grace window on the deadline with \
                     {never_reasserted} co-writer(s) never re-asserting — their custody is gone \
                     and fresh acquires are admitted again",
                    self.id,
                );
                false
            }
            crate::grant_table_core::GraceProbe::Open => true,
        }
    }

    /// Milliseconds left in the grace window (`0` = closed).
    pub fn grace_remaining_ms(&self) -> u64 {
        if !self.in_grace() {
            return 0;
        }
        self.table.grace_remaining_ms(self.clock.now_ms())
    }

    fn note_reclaim(&self, client: &str) {
        if self.table.note_reclaim(client) == crate::grant_table_core::ReclaimNote::ClosedEarly {
            log::info!(
                "S9: custody authority '{}' — every prior co-writer re-asserted, grace window \
                 closed early",
                self.id
            );
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
        let mut out = Vec::with_capacity(frame.inos.len() + frame.ranges.len());
        // Rung 17: whole-file re-assertions (span None) and RANGE
        // re-assertions ride one loop — a range holder re-asserts its
        // ORIGINAL span (MW-13's law), never a whole-file widening.
        let asks: Vec<(u64, Option<(u64, u64)>)> = frame
            .inos
            .iter()
            .map(|ino| (*ino, None))
            .chain(frame.ranges.iter().map(|(ino, span)| (*ino, Some(*span))))
            .collect();
        for (ino, span) in &asks {
            let ino = *ino;
            let req = AcquireFrame {
                schema: CUSTODY_SCHEMA,
                client: frame.client.clone(),
                lease_epoch: frame.lease_epoch,
                ino,
                span: *span,
                concurrent_write: false,
                // A reclaim never waits: the predecessor's grants are gone
                // with its RAM, so anything that conflicts is a LIVE
                // conflict the client must be told about.
                wait_ms: 0,
                desired: None,
            };
            // Deliberately NOT `self.grant()`: that path refuses inside the
            // grace window, and admitting reclaim is the window's entire
            // purpose.
            let mode = LockMode::Exclusive;
            let path = format!("inode_{ino}");
            let Ok(lease) = self
                .arbiter
                .acquire_lock_mode(&path, *span, mode, Duration::from_millis(0))
                .await
            else {
                self.conflicts.fetch_add(1, Ordering::Relaxed);
                CONFLICTS.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let token = lease.fencing_token();
            // The same validate-and-commit step as `grant()` (the rung-4
            // loom finding's fix): a reclaiming lease that died while
            // this arbitration awaited retires THIS ino's re-assertion
            // as a conflict (the reclaim's conflict form — the ino is
            // simply not in the reply) rather than committing custody
            // under a dead epoch.
            let grant_id = match self.table.commit_grant_if_current(
                &req.client,
                req.lease_epoch,
                ino,
                *span,
                lease,
            ) {
                Ok(id) => id,
                Err(dead_lease) => {
                    drop(dead_lease);
                    self.conflicts.fetch_add(1, Ordering::Relaxed);
                    CONFLICTS.fetch_add(1, Ordering::Relaxed);
                    log::warn!(
                        "S9: reclaim of inode_{ino} by '{}' refused: its lease epoch {} \
                             died while the re-assertion was parked in arbitration — the ino \
                             is omitted from the reply and the client must re-join",
                        req.client,
                        req.lease_epoch
                    );
                    continue;
                }
            };
            self.granted.fetch_add(1, Ordering::Relaxed);
            out.push(GrantRecord {
                schema: CUSTODY_SCHEMA,
                grant_id,
                ino,
                span: *span,
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

    /// Finding 16 half (a): compose the acquire's answer — the grant plus
    /// the notices addressed to `client`'s live grants, gathered AFTER the
    /// acquire's own arbitration completed (the composed-after-the-mark
    /// serialization argument — see [`AcquireReplyFrame`]).
    fn acquire_reply(&self, client: &str, grant: GrantRecord) -> AcquireReplyFrame {
        let (demotions, shrinks) = self.owner.notices_for_client(client);
        AcquireReplyFrame {
            schema: CUSTODY_SCHEMA,
            grant,
            demotions,
            shrinks,
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
                // S11 rung 15: a desired-bearing acquire takes the
                // required/desired path, whose refusals carry their DETAIL
                // (the budget arithmetic must reach the refused client —
                // the fleet-share refusal precedent).
                Ok(frame) if frame.desired.is_some() => {
                    let desired = frame.desired.expect("guarded");
                    match self.owner.grant_ranged(&frame, desired).await {
                        Ok(grant) => reply(
                            req.id,
                            &self.acquire_reply(&frame.client, grant),
                            "range grant",
                        ),
                        Err((status, detail)) => Self::refuse(
                            req.id,
                            status,
                            format!(
                                "range custody of inode_{} {:?} refused to '{}' ({}): {detail}",
                                frame.ino,
                                frame.span,
                                frame.client,
                                status_name(status)
                            ),
                        ),
                    }
                }
                Ok(frame) => match self.owner.grant(&frame).await {
                    Ok(grant) => reply(req.id, &self.acquire_reply(&frame.client, grant), "grant"),
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
                    // Finding 16 half (a): the release reply is a notice
                    // carrier too — gathered AFTER the release, so the
                    // notices name the client's SURVIVING grants only.
                    let (demotions, shrinks) = self.owner.notices_for_client(&frame.client);
                    reply(
                        req.id,
                        &ReleaseReplyFrame {
                            schema: CUSTODY_SCHEMA,
                            released: n as u64,
                            demotions,
                            shrinks,
                        },
                        "release reply",
                    )
                }
            },
            VERB_CUSTODY_DEMOTE_ACK => match decode::<DemoteAckFrame>(&req.body, "demote ack") {
                Err(e) => Self::refuse(req.id, CUSTODY_MALFORMED, format!("{e}")),
                Ok(frame) if frame.schema != CUSTODY_SCHEMA => Self::refuse(
                    req.id,
                    CUSTODY_SCHEMA_MISMATCH,
                    format!(
                        "peer speaks custody schema {} and this authority speaks \
                         {CUSTODY_SCHEMA}",
                        frame.schema
                    ),
                ),
                Ok(frame) => match self.owner.demote_ack(
                    &frame.client,
                    frame.lease_epoch,
                    frame.ino,
                    frame.incumbent_token,
                    frame.region,
                ) {
                    Ok(acked) => RpcResponse {
                        id: req.id,
                        status: CUSTODY_OK,
                        body: vec![u8::from(acked)],
                    },
                    Err(status) => Self::refuse(
                        req.id,
                        status,
                        format!(
                            "demotion ack from '{}' refused ({})",
                            frame.client,
                            status_name(status)
                        ),
                    ),
                },
            },
            VERB_CUSTODY_SHRINK_ACK => match decode::<ShrinkAckFrame>(&req.body, "shrink ack") {
                Err(e) => Self::refuse(req.id, CUSTODY_MALFORMED, format!("{e}")),
                Ok(frame) if frame.schema != CUSTODY_SCHEMA => Self::refuse(
                    req.id,
                    CUSTODY_SCHEMA_MISMATCH,
                    format!(
                        "peer speaks custody schema {} and this authority speaks \
                         {CUSTODY_SCHEMA}",
                        frame.schema
                    ),
                ),
                Ok(frame) => match self.owner.shrink_ack(
                    &frame.client,
                    frame.lease_epoch,
                    frame.ino,
                    frame.incumbent_token,
                    frame.watermark,
                ) {
                    Ok(resolution) => RpcResponse {
                        id: req.id,
                        status: CUSTODY_OK,
                        // One resolution byte: 0 = no pending (stale ack),
                        // 1 = shrunk to the floor, 2 = escalated to the
                        // demotion barrier (the client had written there).
                        body: vec![match resolution {
                            crate::dlm::ShrinkResolution::None => 0u8,
                            crate::dlm::ShrinkResolution::Shrunk { .. } => 1,
                            crate::dlm::ShrinkResolution::Demoted { .. } => 2,
                        }],
                    },
                    Err(status) => Self::refuse(
                        req.id,
                        status,
                        format!(
                            "tail-shrink ack from '{}' refused ({})",
                            frame.client,
                            status_name(status)
                        ),
                    ),
                },
            },
            VERB_CUSTODY_NOTICE_POLL => {
                match decode::<NoticePollFrame>(&req.body, "notice poll") {
                    Err(e) => Self::refuse(req.id, CUSTODY_MALFORMED, format!("{e}")),
                    Ok(frame) if frame.schema != CUSTODY_SCHEMA => Self::refuse(
                        req.id,
                        CUSTODY_SCHEMA_MISMATCH,
                        format!(
                            "peer speaks custody schema {} and this authority speaks \
                             {CUSTODY_SCHEMA}",
                            frame.schema
                        ),
                    ),
                    Ok(frame) => {
                        if !self.owner.lease_current(&frame.client, frame.lease_epoch) {
                            return Self::refuse(
                                req.id,
                                CUSTODY_UNKNOWN_LEASE,
                                format!(
                                    "notice poll from '{}' names a lease epoch {} that is \
                                     not custody",
                                    frame.client, frame.lease_epoch
                                ),
                            );
                        }
                        // The park is bounded by ONE renewal cadence (the
                        // S6 venue discipline: no unbounded parks on a
                        // service lane) — a client asking for more gets
                        // the clamp echoed in the reply and re-parks.
                        let park = Duration::from_millis(frame.park_ms)
                            .min(self.owner.clocks.renew_interval);
                        let deadline = Instant::now() + park;
                        let (demotions, shrinks) = loop {
                            let notified = self.owner.notice_notify.notified();
                            let (d, sh) = self.owner.notices_for_client(&frame.client);
                            if !d.is_empty() || !sh.is_empty() {
                                break (d, sh);
                            }
                            let remaining = deadline.saturating_duration_since(Instant::now());
                            if remaining.is_zero() {
                                break (Vec::new(), Vec::new());
                            }
                            // The 250 ms re-check slice is the deleg
                            // poll's shape: a wake lost to a race is
                            // re-gathered on the next slice, never
                            // stranded to the deadline.
                            let _ = squeezefs_ipc::sqz_time::timeout(
                                remaining.min(Duration::from_millis(250)),
                                notified,
                            )
                            .await;
                        };
                        reply(
                            req.id,
                            &NoticePollReply {
                                schema: CUSTODY_SCHEMA,
                                demotions,
                                shrinks,
                                park_ms: park.as_millis() as u64,
                            },
                            "notice poll reply",
                        )
                    }
                }
            }
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
        CUSTODY_AT_CAPACITY => "at capacity: §9.2 bounds refused a new span",
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

    /// Claim **S8's metadata verb block** for `service` (rung 9 — the S8
    /// arm's owner half): `VERB_META_BATCH`/`VERB_RECLAIM` ride the SAME
    /// listener as custody + publish, so an authority that grants write
    /// custody also answers the shipped `Metadata` verbs — the two halves
    /// of one mount posture, armed together or not at all.
    ///
    /// Rung 12: the SAME service also serves the S10 delegation block
    /// (`VERB_DELEG_RECALL`/`VERB_DELEG_REASSERT` — the holder's standing
    /// recall channel and the grace re-assertion), claimed here so an
    /// authority can never grant delegations whose recall wire is
    /// unrouted. Live finding #2: the first fleet run granted, then
    /// answered every poll `RPC_UNKNOWN_VERB` — the recall was
    /// undeliverable, timed out at the derived deadline, and the healthy
    /// holder was EVICTED (the escalation working, aimed at the wrong
    /// culprit).
    pub fn with_meta(self, service: Arc<crate::meta_ship::MetaShipService>) -> Self {
        self.with(
            crate::meta_ship::VERB_META_BATCH,
            crate::meta_ship::VERB_RECLAIM,
            Arc::clone(&service) as Arc<dyn RpcAsyncService>,
        )
        .with(
            crate::meta_ship::VERB_DELEG_BASE,
            crate::meta_ship::VERB_DELEG_LAST,
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
    pending: Arc<parking_lot::Mutex<Vec<(u64, u64, u64)>>>,
    /// S11 rung 15: the grant's object + token, so a range grant's death
    /// (release or revocation) retires its client-cache span immediately —
    /// a dead grant serving a covering probe is the one wrong answer the
    /// cache has available. `token == 0` ⇔ not a range grant (no cache
    /// state to retire; the S1 mint starts at 1, so 0 is never a token).
    ino: u64,
    token: u64,
}

impl ClientGrant {
    /// Drop this grant's client-cache range span, if it carries one.
    fn retire_cached_span(&self) {
        if self.token != 0 {
            crate::meta_ship::tokens::retire_range_grant(self.ino, self.token);
        }
    }
}

impl crate::dlm::RemoteGrant for ClientGrant {
    fn release(&self) {
        if self.live.swap(false, Ordering::AcqRel) {
            self.retire_cached_span();
            self.pending
                .lock()
                .push((self.grant_id, self.ino, self.token));
        }
    }
    fn live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }
}

/// One [`WriteCustodyClient::acquire_range`] outcome (S11 rung 15).
#[derive(Debug)]
pub enum RangeAcquireOutcome {
    /// A fresh grant: hold the lease — dropping/releasing it retires the
    /// span (and travels to the authority on the next drain).
    New {
        lease: LockLease,
        /// The granted span (the conflict-free desired-subset, ⊇ required).
        span: (u64, u64),
        grant_id: u64,
    },
    /// The authority WIDENED a grant this client already holds (the §9.2
    /// admit-time merge): same grant id, same token, wider span. The
    /// EXISTING lease handle is the custody — no new handle exists.
    Extended {
        token: u64,
        span: (u64, u64),
        grant_id: u64,
    },
}

/// A **co-writer**: it holds write custody granted by another node's
/// authority, and it writes the bytes itself.
pub struct WriteCustodyClient {
    id: String,
    endpoint: String,
    secret: Vec<u8>,
    /// The WORKLOAD wire session: acquires (which the authority parks in
    /// arbitration for up to one renewal cadence each) serialize here.
    session: crate::sqz_sync::SqzMutex<Option<RpcClient>>,
    /// The **lease-heartbeat wire session** (finding 2, 2026-08-20 —
    /// `tests/membership_liveness_tests.rs`): the idempotent lease-class
    /// verbs (renew / release / reclaim / the renewal-carried acks) ride
    /// their own session, because a heartbeat that queues behind the
    /// write path's acquire storm on the ONE shared session starves past
    /// `T_self` behind the very workload its stall-detection exists to
    /// survive. Dialed lazily on the first lease verb.
    lease_session: crate::sqz_sync::SqzMutex<Option<RpcClient>>,
    /// Finding 27b: the standing notice poll's OWN session — the poll
    /// PARKS on the authority for up to a renewal cadence, and a park
    /// holding the workload session's mutex starved every custody verb
    /// behind it (attempt 10's 3 MiB/s collapse). The `lease_session`
    /// precedent, applied to the third long-lived caller class.
    notice_session: crate::sqz_sync::SqzMutex<Option<RpcClient>>,
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
    /// Queued release verbs: `(grant_id, ino, token)` — the ino + token
    /// ride along so the finding-34 release gate can judge (and flush)
    /// the ino's publish pipeline before the verb departs.
    pending_releases: Arc<parking_lot::Mutex<Vec<(u64, u64, u64)>>>,
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
        let client = Arc::new(Self {
            id: id.to_string(),
            endpoint: endpoint.to_string(),
            secret: secret.to_vec(),
            session: crate::sqz_sync::SqzMutex::new(Some(session)),
            lease_session: crate::sqz_sync::SqzMutex::new(None),
            notice_session: crate::sqz_sync::SqzMutex::new(None),
            lease: arc_swap::ArcSwap::from_pointee(member),
            lease_epoch: AtomicU64::new(lease.epoch),
            lane: std::sync::atomic::AtomicU32::new(pack_lane(lease.writer_lane, lease.writers)),
            grants: scc::HashMap::new(),
            pending_releases: Arc::new(parking_lot::Mutex::new(Vec::new())),
            inflight: parking_lot::Mutex::new(Vec::new()),
            clock,
        });
        // Finding 27: the STANDING notice poll — one parked RPC per
        // custody client, so a QUIET incumbent (no verbs in flight)
        // hears a pending demotion/shrink at poll latency instead of its
        // renewal cadence. The task holds a Weak: the client's last drop
        // ends the channel.
        let weak = Arc::downgrade(&client);
        crate::meta_exec::spawn_meta("custody_notice_poll", notice_poll_run(weak));
        Ok(client)
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

    /// Milliseconds until the next renewal is due — computed **in the
    /// client's OWN clock domain**, and the ONE computation the renewal
    /// cadence consumes (rung-10 finding #1: the loop read a FRESHLY
    /// MINTED monotonic clock, whose origin is its creation instant, so
    /// `now ≡ 0` and `due` equaled the ABSOLUTE deadline — the sleep
    /// doubled every cycle until the lease died at its 4th renewal,
    /// invisibly on storming venues and fatally on any idle window).
    /// Never 0: the loop's sleep floor.
    pub fn renewal_due_ms(&self) -> u64 {
        self.lease
            .load()
            .renew_at_ms()
            .saturating_sub(self.clock.now_ms())
            .max(1)
    }

    /// `true` ⇔ this client is past its own deadline and MUST fail-stop now
    /// — before the authority's TTL lets those bytes be granted elsewhere.
    pub fn self_fence_due(&self) -> bool {
        self.lease.load().self_fence_due()
    }

    /// The per-attempt deadline bound for one renewal tick, ms (finding 2,
    /// 2026-08-20 — the lease-venue liveness law): `max(remaining-to-T_self
    /// / 3, one renewal cadence)`, in the client's own clock domain. Three
    /// bounded attempts always fit before `T_self`, and no single attempt
    /// can occupy the shared `sqz-lease` venue past one cadence.
    pub fn renew_tick_bound_ms(&self) -> u64 {
        let lease = self.lease.load();
        (lease
            .t_self_deadline_ms()
            .saturating_sub(self.clock.now_ms())
            / 3)
        .max(lease.renew_interval_ms())
        .max(1)
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
        // Rung-10 finding #2: a POISONED mount acquires nothing, INSTANTLY,
        // in the fence's own class — `WriterGuardFenced` is the one error
        // every retry ladder returns immediately, while a `LockFailed`
        // here fed the POSIX-5 ladder 35 round-trips and 30 s of budget
        // PER OP on a mount that had already fail-stopped. One relaxed
        // load on the healthy path.
        if crate::data_custody::poisoned() {
            return Err(SqueezefsError::WriterGuardFenced);
        }
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
            desired: None,
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
        let r: AcquireReplyFrame = decode(&reply.body, "grant")?;
        let grant = r.grant;
        let t_adopt = Instant::now();
        let handle = Arc::new(ClientGrant {
            grant_id: grant.grant_id,
            live: AtomicBool::new(true),
            pending: Arc::clone(&self.pending_releases),
            ino: grant.ino,
            token: 0, // plain acquires carry no client-cache range span
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
        // Finding 16 half (a): the acquire reply is a notice carrier —
        // absorbed AFTER this acquire's own outcome adopts, so a notice
        // about another of this client's grants can never reorder ahead
        // of the custody it rode in on.
        self.absorb_notices(&r.demotions, &r.shrinks).await;
        Ok(lease)
    }

    /// **S11 rung 15 — acquire EX byte-range custody by the §9.2
    /// required/desired law** (KD-MW-7): `required` is the span the write
    /// needs (granted whole or refused loud — never trimmed); `desired`
    /// the best-effort block-aligned stretch (always trimmable against
    /// live custody). The grant rides this client's S9 custody lease and
    /// is cached in the S8 token cache's range extension
    /// (`meta_ship::tokens::record_range_grant`), so subsequent writes
    /// inside the granted span pay one lock-free covering probe — the
    /// ≥99.5 %-local law's mechanism.
    ///
    /// An ask adjacent to a span this client already holds comes back as
    /// [`RangeAcquireOutcome::Extended`]: the authority WIDENED the
    /// existing grant (same grant id, same token — the admit-time merge),
    /// this client's adopted record and cached span widen to match, and
    /// **no new lease exists** — the original handle now covers the wider
    /// span. Like `acquire`, never retried on transport failure (no dedup
    /// window — a re-sent acquire could strand an unnameable grant).
    pub async fn acquire_range(
        &self,
        ino: u64,
        required: (u64, u64),
        desired: (u64, u64),
        wait: Duration,
    ) -> Result<RangeAcquireOutcome> {
        if crate::data_custody::poisoned() {
            return Err(SqueezefsError::WriterGuardFenced);
        }
        self.drain_releases().await;
        let frame = AcquireFrame {
            schema: CUSTODY_SCHEMA,
            client: self.id.clone(),
            lease_epoch: self.lease_epoch.load(Ordering::Acquire),
            ino,
            span: Some(required),
            concurrent_write: false, // KD-MW-9: v1 issues EX only
            wait_ms: wait.as_millis().min(u64::MAX as u128) as u64,
            desired: Some(desired),
        };
        let body = encode(&frame, "range acquire")?;
        let t = Instant::now();
        let reply = self.call_once(VERB_CUSTODY_ACQUIRE, body).await?;
        phase_record(CustodyPhase::Rtt, t);
        if reply.status != CUSTODY_OK {
            let detail = String::from_utf8_lossy(&reply.body).to_string();
            if reply.status == CUSTODY_UNKNOWN_LEASE {
                self.note_lease_lost(&detail);
            }
            return Err(SqueezefsError::LockFailed {
                reason: format!(
                    "S11: the custody authority at {} refused range custody of inode_{ino} \
                     [{},{}) ({}): {detail}",
                    self.endpoint,
                    required.0,
                    required.1,
                    status_name(reply.status)
                ),
            });
        }
        let r: AcquireReplyFrame = decode(&reply.body, "range grant")?;
        let grant = r.grant;
        let Some(span) = grant.span else {
            return Err(SqueezefsError::InvalidOperation(format!(
                "S11: the authority answered a range acquire on inode_{ino} with a \
                 WHOLE-FILE grant record — refusing the adoption rather than caching \
                 custody wider than was arbitrated"
            )));
        };
        if let Some(handle) = self.grants.read_sync(&grant.grant_id, |_, g| Arc::clone(g)) {
            // The EXTENSION face: the authority widened a grant this
            // client already holds. Widen the adopted record (same token,
            // same release identity) and the cached span; the existing
            // lease handle is the custody — no new handle exists. The
            // required union carries THIS ask's required, never the span
            // (§9.3a — on a shared-process authority the adopted record
            // IS the arbiter record, and a span-wide union would erase
            // the tail the shrink arm reclaims).
            if !crate::dlm::widen_adopted_grant(ino, grant.token, span, required) {
                // The local record died between the reply and this widen
                // (a racing lease loss retired it whole). The honest
                // answer is the lease-lost class — the caller re-joins.
                handle.live.store(false, Ordering::Release);
                return Err(SqueezefsError::LockFailed {
                    reason: format!(
                        "S11: the authority extended grant {} on inode_{ino}, but this \
                         client's adopted record is gone (custody died mid-extension) — \
                         self-fence and re-join",
                        grant.grant_id
                    ),
                });
            }
            crate::meta_ship::tokens::record_range_grant(ino, span, grant.token);
            RANGE_EXTENSIONS_CLIENT.fetch_add(1, Ordering::Relaxed);
            // Finding 16 half (a): the extension reply carries notices too.
            self.absorb_notices(&r.demotions, &r.shrinks).await;
            return Ok(RangeAcquireOutcome::Extended {
                token: grant.token,
                span,
                grant_id: grant.grant_id,
            });
        }
        let t_adopt = Instant::now();
        let handle = Arc::new(ClientGrant {
            grant_id: grant.grant_id,
            live: AtomicBool::new(true),
            pending: Arc::clone(&self.pending_releases),
            ino,
            token: grant.token,
        });
        let _ = self.grants.insert_sync(grant.grant_id, Arc::clone(&handle));
        crate::data_custody::adopt_custody_generation(crate::dlm::token_grant_seq(
            grant.custody_epoch,
        ));
        let lease = crate::dlm::adopt_remote_grant(
            ino,
            Some(span),
            grant.token,
            LockMode::Exclusive,
            handle as Arc<dyn crate::dlm::RemoteGrant>,
        )?;
        crate::meta_ship::tokens::record_range_grant(ino, span, grant.token);
        phase_record(CustodyPhase::Adopt, t_adopt);
        GRANTS.fetch_add(1, Ordering::Relaxed);
        RANGE_ACQUIRES_CLIENT.fetch_add(1, Ordering::Relaxed);
        // Finding 16 half (a): the acquire reply is a notice carrier —
        // absorbed after this acquire's own outcome adopts (see `acquire`).
        self.absorb_notices(&r.demotions, &r.shrinks).await;
        Ok(RangeAcquireOutcome::New {
            lease,
            span,
            grant_id: grant.grant_id,
        })
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
        // S11 rung 15: the lease's RANGE VECTOR revalidates the client
        // range cache — the authority's record is authoritative for
        // presence AND absence, so a release/revocation whose local
        // retire was lost still leaves the cache at the next heartbeat.
        crate::meta_ship::tokens::replace_range_grants(
            &r.ranges
                .iter()
                .map(|e| (e.ino, e.span, e.token))
                .collect::<Vec<_>>(),
        );
        // The reply-carried demotion/shrink notices — since finding 16
        // half (a) the SAME absorption every custody-channel reply runs.
        self.absorb_notices(&r.demotions, &r.shrinks).await;
        // Rung 17: the coverage watermarks release retained extents
        // (release path 2 — pull only, never on ack).
        for (ino, upto) in &r.extent_covered {
            crate::extent_ship::release_covered(*ino, *upto);
        }
        Ok(())
    }

    /// **Absorb reply-carried notices** — the client half of the §9.3/§9.3a
    /// pull channel, shared by every carrier since finding 16 half (a)
    /// widened the set from the renewal reply alone to the acquire and
    /// release replies (row 2's ledger: 40 of 51 shrink notices died with
    /// their grant waiting for a renewal that never came first).
    ///
    /// Rung 17 (§9.3) DEMOTION order is load-bearing: (1) mark the region
    /// demoted in the LOCAL table so every subsequent write to it
    /// classifies extent-ship, (2) QUIESCE the in-flight direct publishes,
    /// (3) only then ACK — the authority issues the parked grant the
    /// moment the ack lands, so a single publisher holds at every instant.
    ///
    /// §9.3a TAIL-SHRINK order is load-bearing: (1) the covering cache
    /// stops serving the released tail (so no later write can be served
    /// custody of bytes this ack gives away), (2) the written high-water
    /// is read AFTER that shrink (any serve that beat it is visible in
    /// the mark — the fetch_max happens under the serve itself), (3) the
    /// local adopted record narrows, (4) the ino learns its stretch
    /// ceiling (the repeat-collision prevention), (5) only then does the
    /// ack travel. A missing cache entry (an R5 shed) answers `u64::MAX`
    /// — "unknown, treat my whole span as written" — which the authority
    /// resolves as an escalation, never a release.
    async fn absorb_notices(&self, demotions: &[DemotionNotice], shrinks: &[ShrinkNotice]) {
        for notice in demotions {
            crate::extent_ship::note_demotion(notice.ino, notice.region).await;
            if let Err(e) = self
                .ack_demotion(notice.ino, notice.incumbent_token, notice.region)
                .await
            {
                // Never fatal: the un-acked pending resolves at this
                // lease's expiry on the OWNER's clock (the barrier's
                // bound) — loud, because the window is now the TTL.
                log::warn!(
                    "S9: demotion ack for ino {} {:?} failed ({e}) — the barrier resolves \
                     at this lease's expiry on the authority's clock",
                    notice.ino,
                    notice.region
                );
            }
        }
        for notice in shrinks {
            let watermark = crate::meta_ship::tokens::shrink_range_grant(
                notice.ino,
                notice.incumbent_token,
                notice.floor,
            )
            .unwrap_or(u64::MAX);
            // The local record narrows to what this client can still
            // honestly claim: the floor, or its own written high-water
            // where that reaches past it (the authority keeps the written
            // hull too — the Demoted arm). An unknown watermark
            // (u64::MAX) narrows nothing, matching the authority's
            // escalate-don't-release resolution.
            crate::dlm::shrink_adopted_grant(
                notice.ino,
                notice.incumbent_token,
                notice.floor.max(watermark),
            );
            crate::meta_ship::tokens::note_stretch_ceiling(notice.ino, notice.floor, watermark);
            if let Err(e) = self
                .ack_tail_shrink(notice.ino, notice.incumbent_token, watermark)
                .await
            {
                // Never fatal: the un-acked pending resolves at this
                // lease's expiry on the OWNER's clock (the fence column)
                // — loud, because the asker's window is now the TTL.
                log::warn!(
                    "S11 §9.3a: tail-shrink ack for ino {} floor {} failed ({e}) — the \
                     barrier resolves at this lease's expiry on the authority's clock",
                    notice.ino,
                    notice.floor
                );
            }
        }
    }

    /// Rung 17 (§9.3): ship one demotion ACK — the client-initiated RPC
    /// that retires this mount's direct-DMA custody over the demoted
    /// region (the caller has already marked the region locally and
    /// quiesced).
    pub async fn ack_demotion(
        &self,
        ino: u64,
        incumbent_token: u64,
        region: (u64, u64),
    ) -> Result<bool> {
        let frame = DemoteAckFrame {
            schema: CUSTODY_SCHEMA,
            client: self.id.clone(),
            lease_epoch: self.lease_epoch.load(Ordering::Acquire),
            ino,
            incumbent_token,
            region,
        };
        let body = encode(&frame, "demote ack")?;
        let reply = self.call_retrying(VERB_CUSTODY_DEMOTE_ACK, body).await?;
        if reply.status != CUSTODY_OK {
            let detail = String::from_utf8_lossy(&reply.body).to_string();
            if reply.status == CUSTODY_UNKNOWN_LEASE {
                self.note_lease_lost(&detail);
            }
            return Err(SqueezefsError::LockFailed {
                reason: format!(
                    "S9: demotion ack refused by {} ({}): {detail}",
                    self.endpoint,
                    status_name(reply.status)
                ),
            });
        }
        Ok(reply.body.first().copied().unwrap_or(0) != 0)
    }

    /// §9.3a: ship one tail-shrink ACK — the client-initiated RPC that
    /// answers this mount's written high-water inside the contested tail
    /// (the caller has already shrunk the covering cache and the local
    /// adopted record). The reply's resolution byte: 0 = no pending
    /// (stale ack), 1 = shrunk to the floor, 2 = escalated to the
    /// demotion barrier.
    pub async fn ack_tail_shrink(
        &self,
        ino: u64,
        incumbent_token: u64,
        watermark: u64,
    ) -> Result<u8> {
        let frame = ShrinkAckFrame {
            schema: CUSTODY_SCHEMA,
            client: self.id.clone(),
            lease_epoch: self.lease_epoch.load(Ordering::Acquire),
            ino,
            incumbent_token,
            watermark,
        };
        let body = encode(&frame, "shrink ack")?;
        let reply = self.call_retrying(VERB_CUSTODY_SHRINK_ACK, body).await?;
        if reply.status != CUSTODY_OK {
            let detail = String::from_utf8_lossy(&reply.body).to_string();
            if reply.status == CUSTODY_UNKNOWN_LEASE {
                self.note_lease_lost(&detail);
            }
            return Err(SqueezefsError::LockFailed {
                reason: format!(
                    "S11 §9.3a: tail-shrink ack refused by {} ({}): {detail}",
                    self.endpoint,
                    status_name(reply.status)
                ),
            });
        }
        Ok(reply.body.first().copied().unwrap_or(0))
    }

    /// Re-assert custody of `inos` inside a successor's grace window,
    /// adopting the fresh-era grants it returns.
    pub async fn reclaim(&self, inos: &[u64]) -> Result<Vec<GrantRecord>> {
        self.reclaim_with_ranges(inos, &[]).await
    }

    /// [`Self::reclaim`] carrying RANGE re-assertions too (rung 17,
    /// MW-13's law: a range holder re-asserts its ORIGINAL block-aligned
    /// grant — never a whole-file widening that would conflict with a
    /// surviving peer's ranges).
    pub async fn reclaim_with_ranges(
        &self,
        inos: &[u64],
        ranges: &[(u64, (u64, u64))],
    ) -> Result<Vec<GrantRecord>> {
        let frame = ReclaimFrame {
            schema: CUSTODY_SCHEMA,
            client: self.id.clone(),
            lease_epoch: self.lease_epoch.load(Ordering::Acquire),
            inos: inos.to_vec(),
            ranges: ranges.to_vec(),
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
    ///
    /// **Finding 34 (rung 1 — the release gate):** a RANGED grant's
    /// release verb departs only when the installed [`ReleaseGateHook`]
    /// judges its ino's publish pipeline QUIESCENT (no dirty layout, no
    /// open rewrite-epoch shadow, no in-flight publish). A release that
    /// outran its own backgrounded close-time flush handed the bytes back
    /// while the flush's full Put was still traveling — the owner then
    /// served that Put custody-less and VERBATIM: the whole-map clobber
    /// that stranded every reverted peer entry's durable take (the
    /// s11-blockcyclic C8 storm) and minted the refused duplicate frees.
    /// A non-quiescent ino's entries REQUEUE (the gate kicks a detached
    /// flush; the renewal cadence re-drains), so the verb's departure is
    /// ordered behind the publishes its custody justified. Whole-file
    /// grants (`token == 0`) and gate-less mounts (solo, tests, arms
    /// without the hook) ship exactly as before.
    pub async fn drain_releases(&self) {
        let batch: Vec<(u64, u64, u64)> = std::mem::take(&mut *self.pending_releases.lock());
        if batch.is_empty() {
            return;
        }
        let gate = release_gate();
        let mut ids: Vec<u64> = Vec::with_capacity(batch.len());
        let mut requeue: Vec<(u64, u64, u64)> = Vec::new();
        match gate {
            Some(gate) => {
                // One gate verdict per distinct ino: the max released
                // token rides as the flush-kick's fencing hint.
                let mut inos: std::collections::BTreeMap<u64, u64> =
                    std::collections::BTreeMap::new();
                for &(_, ino, token) in &batch {
                    if token != 0 {
                        let t = inos.entry(ino).or_insert(0);
                        *t = (*t).max(token);
                    }
                }
                let mut deferred: std::collections::HashSet<u64> = std::collections::HashSet::new();
                for (&ino, &token) in &inos {
                    if !gate(ino, token) {
                        deferred.insert(ino);
                    }
                }
                for entry in batch {
                    let (id, ino, token) = entry;
                    if token != 0 && deferred.contains(&ino) {
                        requeue.push(entry);
                    } else {
                        ids.push(id);
                    }
                }
                if !requeue.is_empty() {
                    RELEASES_DEFERRED.fetch_add(requeue.len() as u64, Ordering::Relaxed);
                    self.pending_releases.lock().extend(requeue);
                }
            }
            None => ids.extend(batch.into_iter().map(|(id, _, _)| id)),
        }
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
            Ok(reply) => {
                RELEASES.fetch_add(ids.len() as u64, Ordering::Relaxed);
                // Finding 16 half (a): the release reply is a notice
                // carrier — the notices name this client's SURVIVING
                // grants (a notice whose incumbent was in the released
                // set resolves through the fence column, as before). A
                // refused or undecodable reply loses only the ride, never
                // the release accounting above; the notice re-travels on
                // the next interaction or renewal.
                if reply.status == CUSTODY_OK {
                    match decode::<ReleaseReplyFrame>(&reply.body, "release reply") {
                        Ok(r) => self.absorb_notices(&r.demotions, &r.shrinks).await,
                        Err(e) => log::warn!(
                            "S9: release reply from {} undecodable ({e}) — reply-carried \
                             notices lost this ride (they re-travel on the next \
                             interaction or renewal)",
                            self.endpoint
                        ),
                    }
                }
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
            // A dead range grant's cached span must die with it — a
            // covering probe serving a retired token is the one wrong
            // answer the cache has available (S11 rung 15).
            handle.retire_cached_span();
        }
        let _ = self.grants.remove_sync(&grant_id);
    }

    /// This client's whole custody is gone: every adopted grant is dead and
    /// the custody generation advances, so no in-flight DMA authorized
    /// under it can land.
    /// **The publish plane's pull-based revocation channel fired** (finding
    /// #6, design-mw-layout-versions §6a): the authority refused a shipped
    /// publish `PUBLISH_STALE_LEASE` while `presented` is still this
    /// client's CURRENT lease epoch — authoritative proof this mount's
    /// custody era is dead. Compose the FULL fence at this round trip
    /// instead of waiting for the next renewal: adopted grants dead +
    /// custody generation advanced (the UnknownLease machinery) **plus**
    /// the writer self-fence (custody POISON — a swept co-writer cannot
    /// re-join in place; re-admission is by remount, the documented
    /// posture).
    ///
    /// Returns `false` — and fences NOTHING — when `presented` is an epoch
    /// this client already replaced (a re-join raced the frame): the
    /// refusal was about a dead frame, not about the live era.
    pub fn note_publish_era_refused(&self, presented: u64, detail: &str) -> bool {
        if self.lease_epoch() != presented {
            return false;
        }
        self.note_lease_lost(detail);
        self.self_fence(detail);
        true
    }

    fn note_lease_lost(&self, detail: &str) {
        let mut ids = Vec::new();
        self.grants.iter_sync(|id, _| {
            ids.push(*id);
            true
        });
        for id in ids {
            self.mark_dead(id);
        }
        // S11 rung 15: a client whose LEASE died holds no range custody
        // at all — the whole range cache dies with it (per-grant retires
        // above cover the known ids; this closes any residue).
        crate::meta_ship::tokens::clear_range_grants();
        crate::data_custody::advance_custody_generation(&format!(
            "S9: this node's custody lease is no longer custody ({detail})"
        ));
    }

    /// One roundtrip on the WORKLOAD session (acquires — the verbs the
    /// authority may park in arbitration).
    async fn call_once(
        &self,
        verb: u16,
        body: Vec<u8>,
    ) -> Result<crate::cluster_wire::RpcResponse> {
        self.call_once_on(&self.session, verb, body).await
    }

    async fn call_once_on(
        &self,
        session: &crate::sqz_sync::SqzMutex<Option<RpcClient>>,
        verb: u16,
        body: Vec<u8>,
    ) -> Result<crate::cluster_wire::RpcResponse> {
        let mut guard = session.lock().await;
        // Finding 14 (the width-8 re-grade's conviction): a pooled session
        // the wire's 60 s idle reaper closed is provably dead BEFORE the
        // send (its FIN answers a non-blocking peek), so replacing it here
        // costs none of this verb's ONE attempt and re-parks no
        // arbitration — without it, the first write-open after any quiet
        // spell failed EINVAL on a healthy fleet (the custody acquire is
        // this path's first carrier).
        if guard.as_ref().is_some_and(|c| c.dead_on_arrival()) {
            *guard = None;
        }
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

    /// One roundtrip, with ONE reconnect+resend, on the **lease-heartbeat
    /// session** ([`Self::lease_session`]) — never the workload session an
    /// acquire storm holds for a full arbitration park per attempt. Safe
    /// only for the idempotent lease-class verbs (renew / release /
    /// reclaim / the renewal-carried acks); an acquire never takes this
    /// path.
    async fn call_retrying(
        &self,
        verb: u16,
        body: Vec<u8>,
    ) -> Result<crate::cluster_wire::RpcResponse> {
        match self
            .call_once_on(&self.lease_session, verb, body.clone())
            .await
        {
            Ok(r) => Ok(r),
            Err(first) => {
                log::warn!(
                    "S9: custody verb {verb:#x} to {} failed ({first}) — reconnecting and \
                     resending (the verb is idempotent)",
                    self.endpoint
                );
                self.call_once_on(&self.lease_session, verb, body).await
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

/// **Validate a peer's displaced-block free** against the installed
/// authority ([`WriteCustodyOwner::check_free`]) — the era gate S9's
/// publish owner runs BEFORE the free verb's dedup window.
///
/// With **no authority installed** the free is refused for the same reason
/// the lane raise is: a node serving the publish vocabulary without a
/// custody authority minted no lease epochs, so it has nothing to check
/// the presented one against — and executing a free for an unverifiable
/// era is exactly the act the fence exists to prevent.
pub fn validate_free(client: &str, lease_epoch: u64) -> std::result::Result<(), String> {
    let Some(owner) = custody_owner() else {
        return Err(format!(
            "S9: refusing a displaced-block free from '{client}': this node serves the publish \
             vocabulary but has no custody authority armed, so lease epoch {lease_epoch} cannot \
             be verified as live custody. Arm the multi-writer authority \
             (SQUEEZEFS_MULTI_WRITER=1 + SQUEEZEFS_MW_BIND) — a free executed for an \
             unverifiable era is a fenced zombie's free"
        ));
    };
    owner.check_free(client, lease_epoch)
}

/// **Validate a shipped mutating publish verb's era** against the installed
/// authority ([`WriteCustodyOwner::check_publish_era`]) — the era gate S9's
/// publish owner runs on the layout-publish class (and every other mutating
/// verb) BEFORE the witness window (finding #6; design-mw-layout-versions
/// §6a).
///
/// With **no authority installed** the verb is refused for the same reason
/// the free is: a node serving the publish vocabulary without a custody
/// authority minted no lease epochs, so it has nothing to check the
/// presented one against — and applying a layout for an unverifiable era is
/// exactly the divergent-chain mint the gate exists to prevent.
pub fn validate_publish_era(client: &str, lease_epoch: u64) -> std::result::Result<(), String> {
    let Some(owner) = custody_owner() else {
        return Err(format!(
            "S9: refusing a shipped publish from '{client}': this node serves the publish \
             vocabulary but has no custody authority armed, so lease epoch {lease_epoch} cannot \
             be verified as live custody. Arm the multi-writer authority \
             (SQUEEZEFS_MULTI_WRITER=1 + SQUEEZEFS_MW_BIND) — a publish applied for an \
             unverifiable era is a fenced zombie's publish (design-mw-layout-versions §6a)"
        ));
    };
    owner.check_publish_era(client, lease_epoch)
}

/// Record lane-harvest handouts on the installed authority (rung 10 —
/// [`WriteCustodyOwner::note_lane_handouts`]). The one caller is the
/// harvest executor, which runs strictly AFTER the lane + era validation,
/// so a missing authority here is a torn-down test venue, never a
/// production window — logged loud, offsets covered by derived recovery.
pub fn note_lane_handouts(lease_epoch: u64, offsets: &[u64]) {
    match custody_owner() {
        Some(owner) => owner.note_lane_handouts(lease_epoch, offsets),
        None => log::error!(
            "S9: {} lane-harvest handout(s) for lease epoch {lease_epoch} could not be recorded \
             — no custody authority is installed (the offsets stay durably unreferenced; \
             derived recovery owns them)",
            offsets.len()
        ),
    }
}

/// Discharge lane-harvest handouts on the installed authority (rung 10 —
/// [`WriteCustodyOwner::discharge_lane_handouts`]): the offsets' next
/// shipped free returned them to this authority's own ladder.
pub fn discharge_lane_handouts(offsets: &[u64]) {
    if let Some(owner) = custody_owner() {
        owner.discharge_lane_handouts(offsets);
    }
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
/// **S11 rung 15**: ship a required/desired range acquire to the home's
/// owner (the [`acquire_remote`] pattern for the range face), mapping the
/// wire outcome back into the local [`crate::dlm::RangeAcquired`] shape
/// the homing entry point answers with.
/// Finding 27: the standing notice poll's client loop. Each round parks
/// one RPC on the authority (bounded there by one renewal cadence) and
/// absorbs whatever notices the reply carries — `absorb_notices` runs the
/// same quiesce+ack ladder every f16a carrier feeds, so the §9.3 ledger
/// still closes through the ACK column. Wire errors back off and retry
/// (the authority may be failing over — the renewal path owns re-join);
/// the loop ends when the client drops or the plane poisons.
async fn notice_poll_run(weak: std::sync::Weak<WriteCustodyClient>) {
    loop {
        let Some(client) = weak.upgrade() else { return };
        if crate::data_custody::poisoned() {
            return;
        }
        let frame = NoticePollFrame {
            schema: CUSTODY_SCHEMA,
            client: client.id.clone(),
            lease_epoch: client.lease_epoch.load(Ordering::Acquire),
            park_ms: 10_000,
        };
        let body = match encode(&frame, "notice poll") {
            Ok(b) => b,
            Err(_) => return,
        };
        // Finding 27b: the poll's park rides its OWN session — never the
        // workload session, whose mutex a 10 s park would hold against
        // every custody verb (attempt 10's collapse).
        let outcome = client
            .call_once_on(&client.notice_session, VERB_CUSTODY_NOTICE_POLL, body)
            .await;
        let mut backoff = None;
        match outcome {
            Ok(r) if r.status == CUSTODY_OK => {
                NOTICE_POLL_ROUNDS.fetch_add(1, Ordering::Relaxed);
                if let Ok(np) = decode::<NoticePollReply>(&r.body, "notice poll reply") {
                    let n = (np.demotions.len() + np.shrinks.len()) as u64;
                    if n > 0 {
                        NOTICE_POLL_NOTICES.fetch_add(n, Ordering::Relaxed);
                        client.absorb_notices(&np.demotions, &np.shrinks).await;
                    }
                }
            }
            Ok(_) | Err(_) => {
                // Unknown lease / schema refusal / wire error: the
                // renewal path owns diagnosis and re-join — this channel
                // only backs off so a flapping authority is not hammered.
                backoff = Some(Duration::from_millis(500));
            }
        }
        drop(client);
        if let Some(b) = backoff {
            squeezefs_ipc::sqz_time::sleep(b).await;
        }
    }
}

pub async fn acquire_remote_range(
    ino: u64,
    required: (u64, u64),
    desired: (u64, u64),
    ttl: Duration,
    slot: u64,
) -> Result<crate::dlm::RangeAcquired> {
    let Some(client) = custody_client() else {
        let reason = format!(
            "S11: lock object inode_{ino} homes on slot {slot}, which this node's lock \
             authority does not own, and no remote write-custody client is armed — refusing \
             the range acquire [{},{}) rather than granting custody the owner never issued. \
             Arm the multi-writer mount (SQUEEZEFS_MULTI_WRITER=1 with a PR-capable substrate \
             and a stamped format) so the acquire can travel",
            required.0, required.1
        );
        log::error!("{reason}");
        return Err(SqueezefsError::LockFailed { reason });
    };
    match client.acquire_range(ino, required, desired, ttl).await? {
        RangeAcquireOutcome::New { lease, span, .. } => {
            Ok(crate::dlm::RangeAcquired::New { lease, span })
        }
        RangeAcquireOutcome::Extended { token, span, .. } => {
            Ok(crate::dlm::RangeAcquired::Extended { token, span })
        }
    }
}

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
