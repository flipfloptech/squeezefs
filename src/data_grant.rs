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
use crate::custody_revoke_core::HandoverMarkCore;
use crate::data_custody::DeadEpoch;
use crate::dlm::{LocalLockManager, LockLease, LockMode};
use crate::error::{Result, SqueezefsError};
use crate::membership::{LeaseClock, LeaseClocks, MemberRole, MemberSession, SelfFence};
use arc_swap::ArcSwapOption;
use bincode::Options as _;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
/// **8 (symmetric PR 9, review round 2 — Issues 5/9)**: every f16a carrier
/// ([`RenewReplyFrame`], [`AcquireReplyFrame`], [`ReleaseReplyFrame`],
/// [`NoticePollReply`]) gained `recalls` — the [`RecallNotice`]s of a slot
/// handover's flush-then-release (the same pull channel the demotion and
/// shrink notices ride), and [`CUSTODY_DEFERRED`] joined the status words.
/// KD-7: same-commit fleets; the program's wire is unreleased.
pub const CUSTODY_SCHEMA: u32 = 8;

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
/// Status (symmetric PR 9, review round 2 — Issues 5/9): the object's slot
/// is MID-HANDOVER — its custody grants were recalled and the slot's next
/// holder grants it; nothing holds the bytes, the caller RETRIES (the
/// `EAGAIN` class, bounded by the handover: one renewal beat of the
/// recalled writer's release plus the slot's move).
pub const CUSTODY_DEFERRED: u16 = 0x47;

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
            lane_supply_volumes: Vec::new(),
            // Nor the writer's checkpoint ceiling — it rides the membership
            // grant that carries the label it is a promise about.
            checkpoint_ceiling_ms: 0,
            // Nor the pack-group posture (PK4): the SET authority's
            // membership grant advertises it; this frame is custody only.
            pack_group_available: false,
            // Nor the slot-lease carriage (PR 4): a slot lease lives on the
            // MEMBERSHIP lease, never the custody one.
            slot_leases_ack: Default::default(),
            slot_release_notices: Vec::new(),
            offered_slots: Vec::new(),
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

/// **Symmetric PR 9 (schema 8): one pull-channel custody RECALL** — the
/// slot holder is handing the object's slot over (design §5.1.4,
/// flush-then-transfer) and asks THIS client to release grant `grant_id`
/// on `ino` once the ino's publish pipeline is quiescent (the release
/// rides the finding-34 release gate). The grant stays LIVE at the holder
/// until the release lands — no in-flight DMA under it is voided (a
/// recall is a routine handover, never the `dead_grants` revocation) —
/// and the handover defers until it does; a re-acquire of the object
/// meanwhile answers [`CUSTODY_DEFERRED`], so the recall cannot be undone.
/// Composed under the owner's own serialization; rides every f16a carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallNotice {
    pub ino: u64,
    /// The addressee grant — the client's own handle for it.
    pub grant_id: u64,
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
    /// **PR 9** (schema 8): the handover recalls addressed to this
    /// client's grants — the same pull channel.
    pub recalls: Vec<RecallNotice>,
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
    /// **PR 9** (schema 8): the handover recalls addressed to this
    /// client's grants.
    pub recalls: Vec<RecallNotice>,
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
    /// **PR 9** (schema 8): the handover recalls addressed to this
    /// client's SURVIVING grants.
    pub recalls: Vec<RecallNotice>,
}

/// Finding 27: the standing notice poll's ask — "park me until a notice
/// lands for my grants (or `park_ms`)".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticePollFrame {
    pub schema: u32,
    pub client: String,
    pub lease_epoch: u64,
    /// The client's requested park bound ([`notice_poll_park`] — inside
    /// the wire's reply bound, so the client outlives the park); the
    /// authority clamps it to one renewal cadence (the S6 venue
    /// discipline — no unbounded parks on a service lane).
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
    /// **PR 9** (schema 8): the handover recalls addressed to this
    /// client's grants — a recall lands on a parked poll at once.
    pub recalls: Vec<RecallNotice>,
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
/// Standing-poll rounds that ended WITHOUT an answered park — a wire
/// error or a refusal. Each one costs the incumbent its notice session
/// and a 500 ms backoff with no parked poll, so a demotion landing in
/// that window waits for the re-dial. Rounds alone could not show this
/// (`.benchmarks/2026-09-08-assembler-contracts-notice-poll.md`): a fleet
/// whose quiet rounds all lost the reply-bound race read as "the channel
/// lives, fewer notices". Steady growth on a quiet co-writer is session
/// churn.
static NOTICE_POLL_FAILURES: AtomicU64 = AtomicU64::new(0);
static RANGE_EXTENSIONS_CLIENT: AtomicU64 = AtomicU64::new(0);
/// Finding 34 (rung 1): ranged release verbs DEFERRED by the release gate
/// because their ino's publish pipeline was not yet quiescent — each one
/// is the ordering fix engaging (the verb re-queues and departs on a
/// later drain, behind the flush the gate kicked). Sustained growth with
/// releases flat means a flush that never completes (read it beside the
/// writeback error latches).
static RELEASES_DEFERRED: AtomicU64 = AtomicU64::new(0);
/// PR 9: grants this node acquired from a file's SLOT HOLDER rather than
/// the set authority (`dlm_custody_via_slot_holder`) — 0 unarmed, 0 for
/// every own-slot file (the local arbiter, no RPC).
static VIA_SLOT_HOLDER: AtomicU64 = AtomicU64::new(0);
/// PR 9: slot-holder grants whose reply carried the file's records and
/// installed them as this node's token (`dlm_custody_token_carried`) —
/// ≤ `via_slot_holder`; the difference is grants whose token a recall
/// retired mid-flight.
static TOKEN_CARRIED: AtomicU64 = AtomicU64::new(0);
/// PR 9 (review round 2, Issue 4): xattr pages a carried token fetched
/// past its first page (`dlm_custody_token_carried_pages`) — a set wider
/// than one grant page; 0 for every file whose xattrs fit one page.
static TOKEN_CARRIED_PAGES: AtomicU64 = AtomicU64::new(0);
/// PR 9 (review round 2, Issue 8): carried tokens whose install FAILED
/// after custody was granted (`dlm_custody_token_carry_failures`) — the
/// lease is kept, the next serve fetches; ≈ 0.
static TOKEN_CARRY_FAILURES: AtomicU64 = AtomicU64::new(0);
/// PR 9 (review round 2, Issue 10): slot-holder custody clients that
/// reached `T_self` and fenced THEIR OWN custody — that holder's grants
/// marked dead, the process generation advanced, that holder's token
/// planes stopped — never the whole mount's poison
/// (`dlm_custody_holder_fences`, must-stay-0 on a healthy fleet).
static HOLDER_FENCES: AtomicU64 = AtomicU64::new(0);
/// PR 9 (review round 2, Issues 5/9): handover recall notices this
/// writer absorbed — each one a grant released once its ino's pipeline
/// quiesced (`dlm_custody_recalls_absorbed`).
static RECALLS_ABSORBED: AtomicU64 = AtomicU64::new(0);
/// PR 9 (review round 2, Issues 5/9): grants this process's custody
/// authorities RECALLED for slot handovers (`dlm_custody_recalled`).
static CUSTODY_RECALLED: AtomicU64 = AtomicU64::new(0);
/// PR 9 (review round 2, Issue 5): slot handovers DEFERRED for a live
/// custody grant (`slot_handover_custody_deferrals`, the Slot-lease
/// family) — the cadence's retries while the recalled writer releases.
pub static HANDOVER_CUSTODY_DEFERRALS: AtomicU64 = AtomicU64::new(0);
/// PR 9 (round 3 — the stale-resolve window): grants this writer was
/// answered by an appender tree 0 no longer named as the slot's holder
/// (the request resolved before a move and served after it) — released at
/// once and re-acquired where tree 0 points (`dlm_custody_stale_holder_grants`;
/// ≈ 0 — one per handover racing an in-flight acquire at most).
static STALE_HOLDER_GRANTS: AtomicU64 = AtomicU64::new(0);
/// The token-wire correlation ids of the carried custody grants.
static CARRIED_REQUEST_IDS: AtomicU64 = AtomicU64::new(1);
/// **Test seam** (round 3, Issue 21's pin): the writer DROPS the next
/// carrier's recall notices unabsorbed — a reply lost on the wire; the
/// recall must re-travel on the following carrier.
pub static TEST_DROP_RECALL_CARRIER_ONCE: AtomicBool = AtomicBool::new(false);
/// **Test seam** (round 4, Issue 28's pin): the writer DROPS EVERY
/// carrier's recall notices while set — a writer that keeps renewing and
/// never releases a recalled grant, the shape the leave's bound-expiry
/// posture exists for.
pub static TEST_DROP_RECALL_CARRIERS_ALL: AtomicBool = AtomicBool::new(false);

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

/// **Symmetric PR 9 (review round 3, Issue 20) — the recall's FUSE-layer
/// half.** A handover [`RecallNotice`] must reach the writer's CACHED
/// lease (the FUSE layer's `active_leases`, held from a file's first write
/// to its last close), or every later write is admitted under the
/// released grant: `revoke(ino)` PARKS the cached lease (removed from the
/// cache — the next write re-acquires through the ladder, at the slot's
/// NEXT holder once it moved — but not dropped: its drop is what queues
/// the release verb); `settle()` drops every parked lease whose in-flight
/// custody uses (the WRITE handlers and detached DMA continuations that
/// took the token before the revoke) have drained, queueing their gated
/// releases, and answers how many it dropped; `pending()` counts the
/// parked leases still waiting. Installed by the FUSE layer beside the
/// release gate (`install_slot_custody_hooks`); absent (the in-process
/// contracts, no FUSE layer) a recall releases the grant handle directly.
pub struct RecallHooks {
    pub revoke: Arc<dyn Fn(u64) + Send + Sync>,
    pub settle: Arc<dyn Fn() -> usize + Send + Sync>,
    pub pending: Arc<dyn Fn() -> usize + Send + Sync>,
}

static RECALL_HOOKS: Lazy<arc_swap::ArcSwapOption<RecallHooks>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the recall hooks (the FUSE layer's, when the slot-custody plane
/// arms; a re-arm replaces).
pub fn install_recall_hooks(hooks: RecallHooks) {
    RECALL_HOOKS.store(Some(Arc::new(hooks)));
}

/// Uninstall the recall hooks (unmount teardown / tests).
pub fn uninstall_recall_hooks() {
    RECALL_HOOKS.store(None);
}

fn recall_hooks() -> Option<Arc<RecallHooks>> {
    RECALL_HOOKS.load_full()
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
    /// PR 9: grants acquired from a file's slot HOLDER (0 unarmed).
    pub via_slot_holder: u64,
    /// PR 9: slot-holder grants whose reply carried the file's token.
    pub token_carried: u64,
    /// PR 9: xattr pages a carried token fetched past its first.
    pub token_carried_pages: u64,
    /// PR 9: carried installs that failed after custody was granted.
    pub token_carry_failures: u64,
    /// PR 9: slot-holder clients that fenced their own custody at `T_self`.
    pub holder_fences: u64,
    /// PR 9: handover recall notices absorbed (grants released for a
    /// slot's move).
    pub recalls_absorbed: u64,
    /// PR 9: grants this process's authorities recalled for handovers.
    pub recalled: u64,
    /// PR 9 (round 3): grants answered by an appender that no longer held
    /// the slot — released at once, re-acquired where tree 0 points.
    pub stale_holder_grants: u64,
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
        via_slot_holder: VIA_SLOT_HOLDER.load(Ordering::Relaxed),
        token_carried: TOKEN_CARRIED.load(Ordering::Relaxed),
        token_carried_pages: TOKEN_CARRIED_PAGES.load(Ordering::Relaxed),
        token_carry_failures: TOKEN_CARRY_FAILURES.load(Ordering::Relaxed),
        holder_fences: HOLDER_FENCES.load(Ordering::Relaxed),
        recalls_absorbed: RECALLS_ABSORBED.load(Ordering::Relaxed),
        recalled: CUSTODY_RECALLED.load(Ordering::Relaxed),
        stale_holder_grants: STALE_HOLDER_GRANTS.load(Ordering::Relaxed),
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
        // liveness; notices = the quiet-incumbent engagement; failures =
        // rounds that died — session churn, a dark window each).
        "dlm_custody_notice_polls": NOTICE_POLL_ROUNDS.load(Ordering::Relaxed),
        "dlm_custody_notice_poll_notices": NOTICE_POLL_NOTICES.load(Ordering::Relaxed),
        "dlm_custody_notice_poll_failures": NOTICE_POLL_FAILURES.load(Ordering::Relaxed),
        // Symmetric PR 9 — custody by the slot holder: grants served by a
        // file's slot holder rather than the set authority, and how many
        // of those carried the file's token (0 unarmed; 0 on an own file).
        "dlm_custody_via_slot_holder": c.via_slot_holder,
        "dlm_custody_token_carried": c.token_carried,
        "dlm_custody_token_carried_pages": c.token_carried_pages,
        "dlm_custody_token_carry_failures": c.token_carry_failures,
        "dlm_custody_holder_fences": c.holder_fences,
        "dlm_custody_recalls_absorbed": c.recalls_absorbed,
        "dlm_custody_recalled": c.recalled,
        "dlm_custody_stale_holder_grants": c.stale_holder_grants,
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
    /// PR 9: the grant ids a handover RECALLED and whose release has not
    /// landed — the recall SET every carrier re-gathers its client's
    /// notices from (round 3, Issue 21); a grant leaves it at its release
    /// or its client's death.
    recalled_grants: parking_lot::Mutex<std::collections::HashSet<u64>>,
    recalled: AtomicU64,
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
            recalled_grants: parking_lot::Mutex::new(std::collections::HashSet::new()),
            recalled: AtomicU64::new(0),
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
        let recalls = self.take_recalls_for(client);
        Ok(RenewReplyFrame {
            schema: CUSTODY_SCHEMA,
            lease: self.lease_frame(client, lease_epoch, now),
            dead_grants,
            ranges,
            demotions,
            extent_covered,
            shrinks,
            recalls,
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

    /// **Symmetric PR 9 (review round 2, Issues 5/9) — recall the live
    /// grants on `inos`** for a slot handover (design §5.1.4,
    /// flush-then-transfer): one [`RecallNotice`] per live grant is queued
    /// for its client and the parked notice polls are woken, so the
    /// recall lands on a quiet writer at once and on a busy one with its
    /// next carrier. The grants stay LIVE here until each client's release
    /// lands — nothing is voided, no DMA is refused (a recall is a routine
    /// handover, never the `dead_grants` revocation). Idempotent per
    /// grant (the handover is retried every cadence tick). Returns the
    /// number of grants recalled by THIS call. O(live grants).
    pub fn recall_grants_on(&self, inos: &[u64]) -> usize {
        if inos.is_empty() {
            return 0;
        }
        let targets: Vec<u64> = self
            .table
            .grants_snapshot_with(|id, g| inos.contains(&g.ino).then_some(id))
            .into_iter()
            .flatten()
            .collect();
        let mut fresh = 0usize;
        {
            let mut seen = self.recalled_grants.lock();
            for grant_id in targets {
                if seen.insert(grant_id) {
                    fresh += 1;
                }
            }
        }
        if fresh > 0 {
            self.recalled.fetch_add(fresh as u64, Ordering::Relaxed);
            CUSTODY_RECALLED.fetch_add(fresh as u64, Ordering::Relaxed);
            self.notice_notify.notify_waiters();
        }
        fresh
    }

    /// The handover recalls addressed to `client` — GATHERED FROM STATE on
    /// every carrier (round 3, Issue 21: the demotion / shrink notices'
    /// own law — a carrier whose reply is lost on the wire re-travels the
    /// notice at the next; the first build `mem::take`d a queue and a
    /// lost reply orphaned the recall for the file's lifetime): the
    /// client's live grants whose ids the handover recalled. The set
    /// empties as the releases land (a released grant leaves the table
    /// AND the recalled set); the writer's absorb is idempotent.
    fn take_recalls_for(&self, client: &str) -> Vec<RecallNotice> {
        let seen = self.recalled_grants.lock();
        if seen.is_empty() {
            return Vec::new();
        }
        self.table
            .grants_snapshot_with(|id, g| {
                (g.client == client && seen.contains(&id)).then_some(RecallNotice {
                    ino: g.ino,
                    grant_id: id,
                })
            })
            .into_iter()
            .flatten()
            .collect()
    }

    /// The grant ids of `client`'s live grants on `ino` (PR 9 — the
    /// contracts' probe of a recall's addressee).
    pub fn grant_ids_on(&self, client: &str, ino: u64) -> Vec<u64> {
        self.table
            .grants_snapshot_with(|id, g| (g.client == client && g.ino == ino).then_some(id))
            .into_iter()
            .flatten()
            .collect()
    }

    /// The live grants mapped by `f` (review round 2, Issue 18 — a
    /// by-value read, no `String` clones; the handover's slot census).
    pub fn grants_snapshot_with<T>(
        &self,
        f: impl Fn(u64, &crate::grant_table_core::GrantEntry<LockLease>) -> T,
    ) -> Vec<T> {
        self.table.grants_snapshot_with(f)
    }

    /// Grants recalled for slot handovers by this authority (PR 9).
    pub fn recalled(&self) -> u64 {
        self.recalled.load(Ordering::Relaxed)
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
            // PR 9: a released grant leaves the recalled set (a later grant
            // id is never reused — the set is bounded by live recalls).
            let mut seen = self.recalled_grants.lock();
            if !seen.is_empty() {
                for id in grant_ids {
                    seen.remove(id);
                }
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
        {
            // PR 9: the dead client's recalls die with it.
            let mut seen = self.recalled_grants.lock();
            if !seen.is_empty() {
                for id in &grants {
                    seen.remove(id);
                }
            }
        }
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
        let recalls = self.owner.take_recalls_for(client);
        AcquireReplyFrame {
            schema: CUSTODY_SCHEMA,
            grant,
            demotions,
            shrinks,
            recalls,
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
                // PR 9 (review round 2, Issues 5/9): an object whose slot
                // is mid-handover — its grants recalled — is granted by
                // the slot's NEXT holder; this one defers (one relaxed
                // load on every shipped path: the recall set is empty
                // unless a symmetric handover is in flight).
                Ok(frame) if handover_recall_defers(frame.ino) => Self::refuse(
                    req.id,
                    CUSTODY_DEFERRED,
                    format!(
                        "inode_{}'s slot is mid-handover (its custody grants were recalled) — \
                         retry: the slot's next holder grants it",
                        frame.ino
                    ),
                ),
                // S11 rung 15: a desired-bearing acquire takes the
                // required/desired path, whose refusals carry their DETAIL
                // (the budget arithmetic must reach the refused client —
                // the fleet-share refusal precedent).
                Ok(frame) if frame.desired.is_some() => {
                    let desired = frame.desired.expect("guarded");
                    match self.owner.grant_ranged(&frame, desired).await {
                        Ok(grant) if handover_recall_defers(frame.ino) => {
                            self.owner.release(&frame.client, &[grant.grant_id]);
                            Self::refuse(
                                req.id,
                                CUSTODY_DEFERRED,
                                format!(
                                    "inode_{}'s slot went mid-handover as it was granted — \
                                     released; retry: the slot's next holder grants it",
                                    frame.ino
                                ),
                            )
                        }
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
                    // The mark read AGAIN after the grant (round 3 — the
                    // grant side of the Dekker pair with the handover's
                    // arm-then-census): a grant a concurrent handover's
                    // census missed would span the move — released here,
                    // the writer retries at the next holder.
                    Ok(grant) if handover_recall_defers(frame.ino) => {
                        self.owner.release(&frame.client, &[grant.grant_id]);
                        Self::refuse(
                            req.id,
                            CUSTODY_DEFERRED,
                            format!(
                                "inode_{}'s slot went mid-handover as it was granted — \
                                 released; retry: the slot's next holder grants it",
                                frame.ino
                            ),
                        )
                    }
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
                    let recalls = self.owner.take_recalls_for(&frame.client);
                    reply(
                        req.id,
                        &ReleaseReplyFrame {
                            schema: CUSTODY_SCHEMA,
                            released: n as u64,
                            demotions,
                            shrinks,
                            recalls,
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
                        let (demotions, shrinks, recalls) = loop {
                            let notified = self.owner.notice_notify.notified();
                            let (d, sh) = self.owner.notices_for_client(&frame.client);
                            let rc = self.owner.take_recalls_for(&frame.client);
                            if !d.is_empty() || !sh.is_empty() || !rc.is_empty() {
                                break (d, sh, rc);
                            }
                            let remaining = deadline.saturating_duration_since(Instant::now());
                            if remaining.is_zero() {
                                break (Vec::new(), Vec::new(), Vec::new());
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
                                recalls,
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
        CUSTODY_DEFERRED => "deferred: the object's slot is mid-handover",
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

    /// Claim the **symmetric manager's verb block** (`0x0500`, design-
    /// symmetric-metadata §6.3) for `service` — PR 4's mount-path wiring
    /// of PR 3's `ManagerService`: the manager verbs ride the SAME
    /// listener as custody, publish and the S8 metadata verbs, one venue
    /// for every owner-side verb of the node.
    pub fn with_manager(self, service: Arc<crate::meta_ship::manager::ManagerSetService>) -> Self {
        self.with(
            crate::meta_ship::manager::VERB_MANAGER_BASE,
            crate::meta_ship::manager::VERB_MANAGER_LAST,
            service,
        )
    }

    /// Claim the **read-token verb block** (`0x0600`, design-symmetric-
    /// metadata §5.7 — PR 5) for `service`: an armed symmetric writer
    /// serves grants, the readers' standing recall channels, acks and
    /// releases on this SAME listener, dispatched by the frame's volume
    /// ordinal.
    pub fn with_tokens(self, service: Arc<crate::meta_ship::token_plane::TokenSetService>) -> Self {
        self.with(
            crate::meta_ship::token_plane::VERB_TOKEN_BASE,
            crate::meta_ship::token_plane::VERB_TOKEN_LAST,
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
    pending: Arc<parking_lot::Mutex<Vec<PendingRelease>>>,
    /// PR 9: set by a handover [`RecallNotice`] — the release the recall
    /// queues rides the finding-34 gate whatever the grant's shape (a
    /// whole-file grant ships ungated on the ordinary close path, whose
    /// flush precedes its release by construction; a recalled one has no
    /// such ordering and must wait for the ino's pipeline to quiesce).
    recalled: AtomicBool,
    /// S11 rung 15: the grant's object + token, so a range grant's death
    /// (release or revocation) retires its client-cache span immediately —
    /// a dead grant serving a covering probe is the one wrong answer the
    /// cache has available. `token == 0` ⇔ not a range grant (no cache
    /// state to retire; the S1 mint starts at 1, so 0 is never a token).
    ino: u64,
    token: u64,
    /// The grant's FENCING token (the S1 mint the owner answered) — the
    /// release gate's flush hint for a recalled whole-file grant, whose
    /// `token` (the range-cache span key) is 0.
    fence_token: u64,
}

impl ClientGrant {
    /// Drop this grant's client-cache range span, if it carries one.
    fn retire_cached_span(&self) {
        if self.token != 0 {
            crate::meta_ship::tokens::retire_range_grant(self.ino, self.token);
        }
    }
}

/// One queued release verb: the grant, its ino + token (so the
/// finding-34 release gate can judge and flush the ino's publish pipeline
/// before the verb departs), and whether the gate applies (`token != 0`
/// — a ranged grant — or a PR 9 handover recall).
#[derive(Debug, Clone, Copy)]
struct PendingRelease {
    id: u64,
    ino: u64,
    /// The gate's flush hint: the range token, else the grant's fencing
    /// token (a recalled whole-file grant).
    hint: u64,
    gated: bool,
}

impl crate::dlm::RemoteGrant for ClientGrant {
    fn release(&self) {
        if self.live.swap(false, Ordering::AcqRel) {
            self.retire_cached_span();
            self.pending.lock().push(PendingRelease {
                id: self.grant_id,
                ino: self.ino,
                hint: if self.token != 0 {
                    self.token
                } else {
                    self.fence_token
                },
                gated: self.token != 0 || self.recalled.load(Ordering::Acquire),
            });
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
    /// Queued release verbs — the ino + token ride along so the
    /// finding-34 release gate can judge (and flush) the ino's publish
    /// pipeline before the verb departs.
    pending_releases: Arc<parking_lot::Mutex<Vec<PendingRelease>>>,
    inflight: parking_lot::Mutex<Vec<u64>>,
    clock: LeaseClock,
    /// PR 9: WHOSE custody this client holds — the set authority's (the
    /// co-writer posture, every shipped behaviour) or one SLOT HOLDER's
    /// among N (the per-holder generation and fence scoping — review
    /// round 2, Issues 7/10).
    scope: CustodyScope,
    /// PR 9: set once this client fenced its own custody (scope
    /// `SlotHolder`) — set LAST, after the fence's work (the completion
    /// word a waiter may observe); every later verb refuses; the arm
    /// drops it.
    fenced: AtomicBool,
    /// PR 9: the fence's entry latch (ONE fencer runs the work).
    fencing: AtomicBool,
    /// PR 9: this client's own `Arc` (set once at connect) — the recall
    /// absorb spawns its settle loop from a `&self` method.
    weak_self: std::sync::OnceLock<std::sync::Weak<WriteCustodyClient>>,
}

/// PR 9 (review round 2, Issues 7/10): the scope of a custody client's
/// lease — what its JOIN adopts into the process and what its `T_self`
/// fences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyScope {
    /// The set AUTHORITY's lease (the S9 co-writer): its custody
    /// generation and era are the PROCESS's — the JOIN adopts both, the
    /// `T_self` fence POISONS process data custody.
    SetAuthority,
    /// One SLOT HOLDER's lease among N (the symmetric plane): its lease
    /// epoch and era are ITS OWN — never folded into the process word
    /// (adopting a busier holder's would void in-flight DMA under every
    /// other holder's grant: `data_dma_epoch_refusals`, acked data lost)
    /// — and its `T_self` fences THIS client's grants alone (marked dead,
    /// the generation advanced, its token planes stopped; the mount lives
    /// and every other holder's files keep writing).
    SlotHolder,
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
        Self::connect_scoped(
            endpoint,
            secret,
            id,
            clock,
            pr_key,
            CustodyScope::SetAuthority,
        )
        .await
    }

    /// **PR 9: JOIN one SLOT HOLDER among N** ([`CustodyScope::SlotHolder`]):
    /// the lease's epoch and era stay this client's own — nothing is
    /// folded into the process custody generation or durable term (review
    /// round 2, Issue 7: a busier holder's lease epoch adopted through the
    /// shared `fetch_max` voided every in-flight DMA authorized under
    /// another holder's grant), and its `T_self` fences this client alone
    /// (Issue 10).
    pub async fn connect_slot_holder(
        endpoint: &str,
        secret: &[u8],
        id: &str,
        clock: LeaseClock,
        pr_key: u64,
    ) -> Result<Arc<Self>> {
        Self::connect_scoped(
            endpoint,
            secret,
            id,
            clock,
            pr_key,
            CustodyScope::SlotHolder,
        )
        .await
    }

    async fn connect_scoped(
        endpoint: &str,
        secret: &[u8],
        id: &str,
        clock: LeaseClock,
        pr_key: u64,
        scope: CustodyScope,
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
        // SET AUTHORITY's client adopts the generation as its floor and
        // thereafter authorizes every DMA under `current_epoch()`. A SLOT
        // HOLDER's client adopts nothing into the process (Issue 7): its
        // lease epoch is one holder's counter among N and its term that
        // holder's volume era — neither is this process's.
        if scope == CustodyScope::SetAuthority {
            crate::data_custody::adopt_custody_generation(crate::dlm::token_grant_seq(
                lease.custody_epoch,
            ));
            crate::dlm::adopt_durable_term(lease.term);
        }
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
            scope,
            fenced: AtomicBool::new(false),
            fencing: AtomicBool::new(false),
            weak_self: std::sync::OnceLock::new(),
        });
        let _ = client.weak_self.set(Arc::downgrade(&client));
        // Finding 27: the STANDING notice poll — one parked RPC per
        // custody client, so a QUIET incumbent (no verbs in flight)
        // hears a pending demotion/shrink at poll latency instead of its
        // renewal cadence. The task holds a Weak: the client's last drop
        // ends the channel. Read once per connect; only the test seam can
        // withhold it.
        if notice_poll_armed() {
            let weak = Arc::downgrade(&client);
            crate::meta_exec::spawn_meta("custody_notice_poll", notice_poll_run(weak));
        }
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
    ///
    /// PR 9 (review round 2, Issue 10): a [`CustodyScope::SlotHolder`]
    /// client fences ITS OWN custody instead — the mount holds N such
    /// leases and one dead holder is not the mount's death: every grant
    /// from this holder is marked dead and the process generation advances
    /// (the S9 law for a lost lease — the work authorized under those
    /// grants is refused at the device gate, the mount lives and re-acquires
    /// from the slot's next holder), this holder's token planes stop
    /// serving (PR 5's `T_self` law, scoped), and the arm forgets the
    /// holder. What remains whole-mount is the `advance`'s retire of DMA
    /// in flight under OTHER holders' grants at that instant — the
    /// one-word epoch carrier's cost, stated in the note and owed to the
    /// per-object capture PR 12b's write-path arm can make.
    pub fn self_fence(&self, reason: &str) -> SelfFence {
        if self.scope == CustodyScope::SlotHolder {
            return self.fence_holder_scoped(reason);
        }
        let fence = self.lease.load().self_fence(reason);
        if fence.first {
            SELF_FENCES.fetch_add(1, Ordering::Relaxed);
        }
        fence
    }

    fn fence_holder_scoped(&self, reason: &str) -> SelfFence {
        // ONE fencer (the renewal loop is the caller; a second call is
        // idempotent): the fence's WORK runs first and the observable
        // `fenced` latch is set LAST (round 3, Issue 27 — the first build
        // latched before its work and a waiter on the latch raced it).
        if !self.fencing.swap(true, Ordering::AcqRel) {
            HOLDER_FENCES.fetch_add(1, Ordering::Relaxed);
            log::error!(
                "PR 9: custody client '{}' at slot holder {} FENCED ITS OWN CUSTODY at T_self \
                 ({reason}): every grant from this holder is dead, this mount's custody \
                 generation advances, the holder's token planes stop — the mount itself lives \
                 (dlm_custody_holder_fences)",
                self.id,
                self.endpoint
            );
            // This client's grants alone (round 3, Issue 23): each marked
            // dead — its own range span retired with it — never the
            // process-wide range-cache clear `note_lease_lost` runs for
            // the set authority's lease.
            let mut ids = Vec::new();
            self.grants.iter_sync(|id, _| {
                ids.push(*id);
                true
            });
            for id in ids {
                self.mark_dead(id);
            }
            crate::data_custody::advance_custody_generation(&format!(
                "PR 9: this node's custody lease at slot holder {} is no longer custody \
                 (T_self: {reason})",
                self.endpoint
            ));
            holder_fenced(&self.endpoint);
            self.fenced.store(true, Ordering::Release);
            return SelfFence {
                role: crate::membership::MemberRole::Writer,
                poisoned_data_custody: false,
                purge_requested: false,
                first: true,
                parked: false,
            };
        }
        SelfFence {
            role: crate::membership::MemberRole::Writer,
            poisoned_data_custody: false,
            purge_requested: false,
            first: false,
            parked: false,
        }
    }

    /// PR 9: this client's custody scope.
    pub fn scope(&self) -> CustodyScope {
        self.scope
    }

    /// PR 9: did this slot-holder client fence its own custody?
    pub fn fenced(&self) -> bool {
        self.fenced.load(Ordering::Acquire)
    }

    /// **PR 9 — the clean leave's custody half** (review round 2, Issue
    /// 2): every grant this client still holds is released at its holder
    /// — queued as the ordinary release each handle's drop would queue
    /// (idempotent with a later drop) and DRAINED so the verb has landed
    /// when this returns. The FUSE layer's cached leases may still be
    /// alive at unmount; the holder must not wait a lease TTL for them.
    pub async fn release_all_grants(&self) {
        let mut handles: Vec<Arc<ClientGrant>> = Vec::new();
        self.grants.iter_sync(|_, g| {
            handles.push(Arc::clone(g));
            true
        });
        for g in handles {
            crate::dlm::RemoteGrant::release(&*g);
        }
        self.drain_releases().await;
    }

    /// **PR 9 — absorb handover RECALL notices** (the client half of
    /// design §5.1.4's flush-then-release): each named grant is marked
    /// dead in the local table (the FUSE layer's next write on the file
    /// re-acquires — from the slot's next holder once it has moved;
    /// meanwhile the old holder answers `CUSTODY_DEFERRED`) and its
    /// release is queued GATED (finding 34: the verb departs only once the
    /// ino's publish pipeline is quiescent, a flush kicked otherwise); the
    /// caller drains the queue (`true` ⇔ something was queued). Nothing is
    /// voided: no DMA authorization is refused for a recall.
    fn absorb_recalls(&self, recalls: &[RecallNotice]) -> bool {
        if recalls.is_empty() {
            return false;
        }
        if TEST_DROP_RECALL_CARRIER_ONCE.swap(false, Ordering::AcqRel)
            || TEST_DROP_RECALL_CARRIERS_ALL.load(Ordering::Acquire)
        {
            return false;
        }
        let hooks = recall_hooks();
        let mut absorbed = 0u64;
        for n in recalls {
            let Some(g) = self.grants.read_sync(&n.grant_id, |_, g| Arc::clone(g)) else {
                continue;
            };
            if g.ino != n.ino {
                log::warn!(
                    "PR 9: recall notice names grant {} on inode_{} but this client holds it on \
                     inode_{} — ignored (the holder's word does not match the grant)",
                    n.grant_id,
                    n.ino,
                    g.ino
                );
                continue;
            }
            // Idempotent (round 3, Issue 21 — the notice is re-gathered on
            // every carrier until the release lands): a grant already
            // recalled is not counted twice.
            if g.recalled.swap(true, Ordering::AcqRel) {
                continue;
            }
            match &hooks {
                // The FUSE layer parks its cached lease: every later write
                // re-acquires; the lease's drop (at the settle, once the
                // in-flight uses drained) queues the gated release.
                Some(h) => (h.revoke)(g.ino),
                // No FUSE layer (the in-process contracts): the grant
                // handle releases directly — the caller's `LockLease`
                // reads it released.
                None => crate::dlm::RemoteGrant::release(&*g),
            }
            absorbed += 1;
        }
        if absorbed > 0 {
            RECALLS_ABSORBED.fetch_add(absorbed, Ordering::Relaxed);
            log::info!(
                "PR 9: {absorbed} custody grant(s) recalled by the slot holder at {} for a \
                 handover — released once quiescent (dlm_custody_recalls_absorbed)",
                self.endpoint
            );
            if hooks.is_some() {
                // The settle is driven promptly (the renewal cadence alone
                // would bound the handover by one shipped beat): a
                // detached loop drops the parked leases as their uses
                // drain and drains the releases, until nothing is parked
                // or the recall's bound passed.
                if let Some(client) = self.weak_self.get().and_then(|w| w.upgrade()) {
                    crate::meta_exec::spawn_lease("slot_custody_recall_settle", async move {
                        client.settle_recalled_until_clear().await;
                    });
                }
            }
        }
        absorbed > 0
    }

    /// The recall's settle loop (round 3, Issue 20): poll the FUSE layer's
    /// parked leases at the derived park until none is pending or the
    /// recall bound passed (a straggler past it is the holder's `T_owner`
    /// sweep's — loud).
    async fn settle_recalled_until_clear(&self) {
        let Some(hooks) = recall_hooks() else {
            return;
        };
        let park = self.handover_retry_park();
        let bound = self.handover_recall_bound();
        let started = Instant::now();
        loop {
            if (hooks.settle)() > 0 {
                self.drain_releases().await;
            }
            if (hooks.pending)() == 0 {
                return;
            }
            if started.elapsed() >= bound {
                log::warn!(
                    "PR 9: {} recalled custody lease(s) still hold in-flight uses after the \
                     recall bound {bound:?} — their releases travel at the next settle; the \
                     holder's T_owner sweep bounds the handover",
                    (hooks.pending)()
                );
                return;
            }
            squeezefs_ipc::sqz_time::sleep(park).await;
        }
    }

    /// The park between two attempts of a DEFERRED acquire and between two
    /// settle polls — DERIVED from the holder's lease clocks as granted
    /// (round 3, Issue 25): one twentieth of the renewal beat (50 ms at
    /// the contracts' 1 s beat; 500 ms at the shipped 10 s), tie-tested.
    pub fn handover_retry_park(&self) -> Duration {
        handover_retry_park_for(Duration::from_millis(self.lease.load().renew_interval_ms()))
    }

    /// The recall's bound as the WRITER derives it from its lease at the
    /// holder — two renewal beats ([`handover_recall_bound_for`]; the
    /// holder's own read is [`handover_recall_bound_ms`]).
    pub fn handover_recall_bound(&self) -> Duration {
        handover_recall_bound_for(Duration::from_millis(self.lease.load().renew_interval_ms()))
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
            let reason = format!(
                "S9: the custody authority at {} refused custody of inode_{ino} {span:?} ({}): \
                 {detail}",
                self.endpoint,
                status_name(reply.status)
            );
            // PR 9: a slot mid-handover is the retryable class, typed.
            if reply.status == CUSTODY_DEFERRED {
                return Err(SqueezefsError::refused(libc::EAGAIN, reason));
            }
            return Err(SqueezefsError::LockFailed { reason });
        }
        let r: AcquireReplyFrame = decode(&reply.body, "grant")?;
        let lease = self.adopt_grant(&r.grant, mode)?;
        // Finding 16 half (a): the acquire reply is a notice carrier —
        // absorbed AFTER this acquire's own outcome adopts, so a notice
        // about another of this client's grants can never reorder ahead
        // of the custody it rode in on.
        self.absorb_notices(&r.demotions, &r.shrinks).await;
        if self.absorb_recalls(&r.recalls) {
            self.drain_releases().await;
        }
        Ok(lease)
    }

    /// Adopt a whole-file / span grant the authority answered: the client's
    /// handle, the custody generation, the owner's own token in this
    /// process's custody table. The ONE adopt for every carrier of a plain
    /// grant record (the S9 acquire reply; PR 9's token-carried grant).
    fn adopt_grant(&self, grant: &GrantRecord, mode: LockMode) -> Result<LockLease> {
        let t_adopt = Instant::now();
        let handle = Arc::new(ClientGrant {
            grant_id: grant.grant_id,
            live: AtomicBool::new(true),
            pending: Arc::clone(&self.pending_releases),
            ino: grant.ino,
            token: 0, // plain acquires carry no client-cache range span
            fence_token: grant.token,
            recalled: AtomicBool::new(false),
        });
        let _ = self.grants.insert_sync(grant.grant_id, Arc::clone(&handle));
        // Issue 7: only the SET AUTHORITY's grant moves the process
        // generation and era; a slot holder's is adopted into the lock
        // table alone (`adopt_remote_grant_scoped(.., false)` — the
        // grant's token term is that holder's volume era, and folding it
        // into `term_base()` would void every in-flight epoch too).
        let adopt_process_words = self.scope == CustodyScope::SetAuthority;
        if adopt_process_words {
            crate::data_custody::adopt_custody_generation(crate::dlm::token_grant_seq(
                grant.custody_epoch,
            ));
        }
        let lease = crate::dlm::adopt_remote_grant_scoped(
            grant.ino,
            grant.span,
            grant.token,
            mode,
            handle as Arc<dyn crate::dlm::RemoteGrant>,
            adopt_process_words,
        )?;
        phase_record(CustodyPhase::Adopt, t_adopt);
        GRANTS.fetch_add(1, Ordering::Relaxed);
        Ok(lease)
    }

    /// **PR 9 — the custody acquire at the file's SLOT HOLDER, the token
    /// carried** (design §5.5 the "S9 custody endpoint" row): ONE round
    /// trip on PR 5's token wire (`TokenCall::CustodyGrant`) answers the
    /// S9 grant record — adopted exactly as an authority's — AND the
    /// object's records under a read token the holder registered for this
    /// client. `object` is the LOCAL key ino on `volume`; the grant names
    /// the GLOBAL ino. Like `acquire`, never retried on a transport
    /// failure (no dedup window — a re-sent acquire could strand a grant
    /// this client cannot name). A `NotHolder` answer travels up: the
    /// caller re-resolves the holder (tree 0 moved under its view).
    pub async fn acquire_carrying_token(
        &self,
        volume: u16,
        object: u64,
        span: Option<(u64, u64)>,
        mode: LockMode,
        wait: Duration,
    ) -> Result<CarriedAcquire> {
        use crate::meta_ship::token_plane as tp;
        if crate::data_custody::poisoned() {
            return Err(SqueezefsError::WriterGuardFenced);
        }
        self.drain_releases().await;
        let request_id = CARRIED_REQUEST_IDS.fetch_add(1, Ordering::Relaxed);
        let body = tp::encode_request(&tp::TokenRequestFrame {
            schema: tp::TOKEN_SCHEMA,
            request_id,
            volume,
            client: self.id.clone(),
            call: tp::TokenCall::CustodyGrant {
                object,
                span,
                concurrent_write: mode == LockMode::ConcurrentWrite,
                wait_ms: wait.as_millis().min(u64::MAX as u128) as u64,
                lease_epoch: self.lease_epoch.load(Ordering::Acquire),
            },
        })?;
        let t = Instant::now();
        let reply = self.call_once(tp::VERB_TOKEN_CALL, body).await?;
        phase_record(CustodyPhase::Rtt, t);
        // Review round 2, Issue 11: the DETERMINISTIC refusals are typed
        // (`Refused { errno }` — never `LockFailed`, which the POSIX-5
        // ladder retries for its whole budget): a schema/status the holder
        // cannot serve, a frame that is not this acquire's, a REJECTED wire
        // word (`EIO` — the caller's word was the defect); the slot
        // mid-handover is `EAGAIN` (retried inside the acquire's budget by
        // the caller). `LockFailed` stays the CONFLICT class alone.
        if reply.status == tp::STATUS_REJECTED {
            return Err(SqueezefsError::refused(
                libc::EIO,
                format!(
                    "PR 9: the slot holder at {} rejected the custody frame for object \
                     {object:#x} on volume {volume} at its service edge: {}",
                    self.endpoint,
                    tp::decode_reply(&reply.body)
                        .map(|f| format!("{:?}", f.reply))
                        .unwrap_or_else(|_| String::from_utf8_lossy(&reply.body).to_string())
                ),
            ));
        }
        if reply.status != tp::STATUS_OK && reply.status != tp::STATUS_REFUSED {
            return Err(SqueezefsError::refused(
                libc::EIO,
                format!(
                    "PR 9: the slot holder at {} refused the custody frame for object \
                     {object:#x} on volume {volume} with status {}: {}",
                    self.endpoint,
                    reply.status,
                    String::from_utf8_lossy(&reply.body)
                ),
            ));
        }
        let frame = tp::decode_reply(&reply.body)?;
        if frame.schema != tp::TOKEN_SCHEMA || frame.request_id != request_id {
            return Err(SqueezefsError::refused(
                libc::EIO,
                format!(
                    "PR 9: the slot holder at {} answered request {request_id} with schema {} \
                     request {} — refusing to adopt custody off a frame that is not this \
                     acquire's",
                    self.endpoint, frame.schema, frame.request_id
                ),
            ));
        }
        match frame.reply {
            tp::TokenReply::CustodyGranted {
                grant,
                records,
                already,
            } => {
                let lease = self.adopt_grant(&grant, mode)?;
                Ok(CarriedAcquire::Granted {
                    lease,
                    records,
                    already,
                })
            }
            tp::TokenReply::NotHolder { holder } => Ok(CarriedAcquire::NotHolder { holder }),
            tp::TokenReply::CustodyRefused { status, reason } => {
                if status == CUSTODY_UNKNOWN_LEASE {
                    self.note_lease_lost(&reason);
                }
                let reason = format!(
                    "S9: the slot holder at {} refused custody of object {object} {span:?} \
                     ({}): {reason}",
                    self.endpoint,
                    status_name(status)
                );
                if status == CUSTODY_DEFERRED {
                    return Ok(CarriedAcquire::Deferred { reason });
                }
                Err(SqueezefsError::LockFailed { reason })
            }
            tp::TokenReply::Rejected { reason } => Err(SqueezefsError::refused(
                libc::EIO,
                format!(
                    "PR 9: the slot holder at {} rejected the custody frame for object \
                     {object:#x} at its service edge: {reason}",
                    self.endpoint
                ),
            )),
            tp::TokenReply::Gone => Err(SqueezefsError::refused(
                libc::ENOENT,
                format!(
                    "PR 9: the slot holder at {} holds no object {object} on volume {volume} — \
                     nothing to hold custody of",
                    self.endpoint
                ),
            )),
            tp::TokenReply::Refused { reason } => Err(SqueezefsError::refused(
                libc::EIO,
                format!(
                    "PR 9: the slot holder at {} refused the custody grant of object \
                     {object}: {reason}",
                    self.endpoint
                ),
            )),
            other => Err(SqueezefsError::refused(
                libc::EIO,
                format!(
                    "PR 9: the slot holder at {} answered a CustodyGrant with {other:?}",
                    self.endpoint
                ),
            )),
        }
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
            fence_token: grant.token,
            recalled: AtomicBool::new(false),
        });
        let _ = self.grants.insert_sync(grant.grant_id, Arc::clone(&handle));
        // Issue 7: the process words move for the SET AUTHORITY's grant
        // alone (see `adopt_grant`).
        let adopt_process_words = self.scope == CustodyScope::SetAuthority;
        if adopt_process_words {
            crate::data_custody::adopt_custody_generation(crate::dlm::token_grant_seq(
                grant.custody_epoch,
            ));
        }
        let lease = crate::dlm::adopt_remote_grant_scoped(
            ino,
            Some(span),
            grant.token,
            LockMode::Exclusive,
            handle as Arc<dyn crate::dlm::RemoteGrant>,
            adopt_process_words,
        )?;
        crate::meta_ship::tokens::record_range_grant(ino, span, grant.token);
        phase_record(CustodyPhase::Adopt, t_adopt);
        GRANTS.fetch_add(1, Ordering::Relaxed);
        RANGE_ACQUIRES_CLIENT.fetch_add(1, Ordering::Relaxed);
        // Finding 16 half (a): the acquire reply is a notice carrier —
        // absorbed after this acquire's own outcome adopts (see `acquire`).
        self.absorb_notices(&r.demotions, &r.shrinks).await;
        if self.absorb_recalls(&r.recalls) {
            self.drain_releases().await;
        }
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
        if self.absorb_recalls(&r.recalls) {
            self.drain_releases().await;
        }
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
        // PR 9: a release reply may CARRY recall notices whose releases
        // queue here — drained by the next pass of this loop (never a
        // recursive drain).
        loop {
            if !self.drain_releases_once().await {
                return;
            }
        }
    }

    /// One drain pass; `true` ⇔ the reply queued further releases (a
    /// recall absorbed) and the caller should drain again.
    async fn drain_releases_once(&self) -> bool {
        // PR 9 (round 3, Issue 20): parked FUSE-layer leases whose in-flight
        // custody uses drained drop HERE — their releases join this pass.
        if let Some(h) = recall_hooks() {
            (h.settle)();
        }
        let batch: Vec<PendingRelease> = std::mem::take(&mut *self.pending_releases.lock());
        if batch.is_empty() {
            return false;
        }
        let gate = release_gate();
        let mut ids: Vec<u64> = Vec::with_capacity(batch.len());
        let mut requeue: Vec<PendingRelease> = Vec::new();
        match gate {
            Some(gate) => {
                // One gate verdict per distinct ino: the max released
                // token rides as the flush-kick's fencing hint (PR 9: a
                // recalled whole-file grant is gated too, hint 0).
                let mut inos: std::collections::BTreeMap<u64, u64> =
                    std::collections::BTreeMap::new();
                for r in &batch {
                    if r.gated {
                        let t = inos.entry(r.ino).or_insert(0);
                        *t = (*t).max(r.hint);
                    }
                }
                let mut deferred: std::collections::HashSet<u64> = std::collections::HashSet::new();
                for (&ino, &token) in &inos {
                    if !gate(ino, token) {
                        deferred.insert(ino);
                    }
                }
                for entry in batch {
                    if entry.gated && deferred.contains(&entry.ino) {
                        requeue.push(entry);
                    } else {
                        ids.push(entry.id);
                    }
                }
                if !requeue.is_empty() {
                    RELEASES_DEFERRED.fetch_add(requeue.len() as u64, Ordering::Relaxed);
                    self.pending_releases.lock().extend(requeue);
                }
            }
            None => ids.extend(batch.into_iter().map(|r| r.id)),
        }
        if ids.is_empty() {
            return false;
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
            return false;
        };
        let mut again = false;
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
                        Ok(r) => {
                            self.absorb_notices(&r.demotions, &r.shrinks).await;
                            again = self.absorb_recalls(&r.recalls);
                        }
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
        again
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

/// The standing poll's arming latch: `0` = the product (armed), `2` =
/// withheld by [`test_set_notice_poll`], `1` = explicitly armed by it.
static NOTICE_POLL_PRESET: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn notice_poll_armed() -> bool {
    NOTICE_POLL_PRESET.load(Ordering::SeqCst) != 2
}

/// Test seam — NOT a knob: `Some(false)` withholds the standing notice
/// poll from every client [`WriteCustodyClient::connect`] builds until
/// `None` (or `Some(true)`) restores the product posture. The product has
/// no opt-out, because a co-writer without its poll hears a demotion only
/// at its renewal cadence (finding 27's half-bandwidth dip). The seam
/// exists for the contracts that pin the barrier's OTHER arms — the
/// renewal-carried notice, the owner-clock expiry, the f16a acquire /
/// release-reply carriers — which are observable only while the poll is
/// not there to ack first (`.benchmarks/2026-09-08-assembler-contracts-notice-poll.md`).
pub fn test_set_notice_poll(on: Option<bool>) {
    NOTICE_POLL_PRESET.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::SeqCst,
    );
}

/// The standing poll's park ASK: half the wire's reply bound
/// ([`crate::cluster_wire::call_reply_bound`]). The authority clamps the
/// park to its renewal cadence, and on the fleet that cadence
/// (`min(10 s, T_self/3)`) is exactly the wire's 10 s bound — so an ask AT
/// the bound made every QUIET round a zero-margin race: the owner's reply
/// lands at park + RTT + its wake, the client's socket read timeout fires
/// at the bound, and the loss is a dropped notice session plus a 500 ms
/// window with no parked poll (counted in `dlm_custody_notice_poll_failures`).
/// Every measured venue is loopback, where the reply wins by the kernel's
/// SO_RCVTIMEO rounding; a fabric RTT or a busy owner loses it. Half the
/// bound leaves the other half as margin; the cost is one small frame per
/// half-bound per quiet co-writer. Derived, never a constant of its own —
/// the tie test is `the_standing_polls_ask_sits_inside_the_wires_reply_bound`.
pub fn notice_poll_park() -> Duration {
    crate::cluster_wire::call_reply_bound() / 2
}

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
            park_ms: notice_poll_park().as_millis() as u64,
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
                    let n = (np.demotions.len() + np.shrinks.len() + np.recalls.len()) as u64;
                    if n > 0 {
                        NOTICE_POLL_NOTICES.fetch_add(n, Ordering::Relaxed);
                        client.absorb_notices(&np.demotions, &np.shrinks).await;
                        // PR 9: a handover recall landing on the standing
                        // poll releases at once (the writer's flush gate
                        // decides when the verb departs).
                        if client.absorb_recalls(&np.recalls) {
                            client.drain_releases().await;
                        }
                    }
                }
            }
            Ok(_) | Err(_) => {
                // Unknown lease / schema refusal / wire error: the
                // renewal path owns diagnosis and re-join — this channel
                // only backs off so a flapping authority is not hammered.
                NOTICE_POLL_FAILURES.fetch_add(1, Ordering::Relaxed);
                backoff = Some(Duration::from_millis(500));
            }
        }
        drop(client);
        if let Some(b) = backoff {
            squeezefs_ipc::sqz_time::sleep(b).await;
        }
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

// ---------------------------------------------------------------------------
// Symmetric PR 9 — custody by the SLOT HOLDER (design-symmetric-metadata
// §5.5 the "S9 custody endpoint" row, §5.1.5 the custody lock class, §5.7.1
// the holder's implicit Write). Under the armed plane the custody server
// for an inode object is the appender leasing its slot — resolved through
// tree 0's lessee + the `SlotHolderCache` exactly as PR 6's shipped steps
// are (`crossvol_tx::step_home`), never a manager RPC. The S9 protocol is
// unchanged (JOIN / RENEW / RELEASE / `T_self` / the custody epoch); what
// moves is WHERE its server is: one JOINed `WriteCustodyClient` per holder
// endpoint (single-flight — a second JOIN at one holder REPLACES the first
// lease and revokes its grants), and the grant rides PR 5's token wire so
// ONE round trip answers custody AND the file's records (Lustre's intent
// lock), installed into a per-`(holder, volume)` `TokenReaderPlane` whose
// standing recall channel is what lets the holder's later commit on the
// file recall the token and this writer re-fetch (gate 5's exactness).
// Unarmed — and for every own-slot file — nothing here is consulted past
// one relaxed load: the local arbiter serves, `dlm_rpcs` stays 0.
// ---------------------------------------------------------------------------

/// The outcome of one slot-holder acquire ([`WriteCustodyClient::acquire_carrying_token`]).
pub enum CarriedAcquire {
    /// Custody adopted; the file's records as the holder served them.
    Granted {
        lease: LockLease,
        records: crate::meta_ship::token_plane::TokenRecords,
        already: bool,
    },
    /// The holder's tree 0 leases the object's slot to `holder` — the
    /// caller re-resolves (a stale `SlotHolderCache` view).
    NotHolder { holder: u32 },
    /// The object's slot is MID-HANDOVER at this holder (its grants were
    /// recalled — [`CUSTODY_DEFERRED`]): the caller re-resolves and
    /// retries inside its budget; the slot's next holder grants it.
    Deferred { reason: String },
}

/// Where an inode object's custody is SERVED under the armed plane, when
/// it is not this mount's own.
#[derive(Debug, Clone)]
pub enum CustodyHome {
    /// Appender `holder` leases the object's slot and serves at `endpoint`;
    /// `object` is the LOCAL key ino on set volume `volume`.
    Holder {
        holder: u32,
        endpoint: Arc<str>,
        volume: u16,
        object: u64,
    },
    /// Appender `holder` leases the slot and this mount knows no endpoint
    /// for it — its ladder has not published one (PR 12 binds every Live
    /// appender's published endpoint at rung 7; a later joiner's arrives
    /// with PR 12b's wire `JoinAppender`): refused loud.
    Unbound { holder: u32 },
}

/// What a slot-holder acquire answers the lock manager (review round 2,
/// Issue 15): custody adopted, or the object became THIS mount's own
/// under the acquire (the slot was handed to us — the holder answered
/// `NotHolder` and the re-resolve names no foreign holder), in which case
/// the caller falls to the local arbiter.
pub enum HolderAcquire {
    Granted(LockLease),
    NowLocal,
}

/// The mount's recall sink per set volume (the `MountRecallSink` shape;
/// the contracts' probe) — what a holder's recall drains and purges on
/// this writer before the ack travels.
pub type RecallSinkFor =
    Arc<dyn Fn(usize) -> Arc<dyn crate::meta_ship::token_plane::RecallDataSink> + Send + Sync>;

/// One dialed holder: its S9 custody client (one lease, renewed on its
/// own cadence) and, per set volume, the token plane its carried records
/// install into (dialed under the holder's OWN plane mutex — I/O per
/// `(holder, volume)`, never under any wider lock).
struct HolderCustody {
    client: Arc<WriteCustodyClient>,
    planes: parking_lot::Mutex<HashMap<u16, Arc<crate::meta_ship::token_plane::TokenReaderPlane>>>,
    plane_dials: crate::sqz_sync::SqzMutex<()>,
}

/// One holder's dial state — the per-holder SINGLE-FLIGHT (review round
/// 2, Issue 6): the async mutex is THIS holder's, held across ITS dial
/// only, so a dead holder's 10 s dial parks its own joiners and nobody
/// else's. A failed dial is remembered for one renewal beat (finding 15's
/// decline shape: the next signal that can change the answer — the
/// holder's recovery, PR 10's death ledger — moves on that cadence, so a
/// storm of acquires against a dead holder costs one dial per beat).
struct HolderSlot {
    dial: crate::sqz_sync::SqzMutex<HolderDial>,
}

#[derive(Default)]
struct HolderDial {
    custody: Option<Arc<HolderCustody>>,
    failed_at: Option<Instant>,
    /// Set by the holder's fence (round 3, Issue 27): this slot was
    /// removed from the arm's map — a dialer still holding it must
    /// re-look the endpoint up rather than JOIN on an orphaned slot.
    retired: bool,
}

/// What the armed plane installs (`arm_slot_custody`): the set to resolve
/// through, this mount's dial identity, and the holders it has dialed.
pub struct SlotCustodyArm {
    routed: std::sync::Weak<crate::meta_backend::RoutedMetaBackend>,
    node_id: String,
    secret: Vec<u8>,
    pr_key: u64,
    sink_for: RecallSinkFor,
    /// The clients' lease clock (monotonic on the mount path; the
    /// contracts inject a manual one to drive `T_self`).
    clock: LeaseClock,
    /// Endpoint → the holder's dial slot. A SHORT sync lock (insert /
    /// lookup only); every dial runs under the slot's own async mutex.
    holders: parking_lot::Mutex<HashMap<Arc<str>, Arc<HolderSlot>>>,
    /// The renewal loops' stop latch (the co-writer arm's own shape).
    stop: Arc<AtomicBool>,
}

static SLOT_CUSTODY: Lazy<ArcSwapOption<SlotCustodyArm>> = Lazy::new(ArcSwapOption::empty);

/// Arm custody by the slot holder over `routed` (the mount path on an
/// armed symmetric set; the contracts directly): `node_id` and `pr_key`
/// are this mount's KD-MW-2 identity and WERO registrant key — the JOIN's
/// words at every holder. Idempotent per process: a re-arm REPLACES the
/// previous arm and stops its renewal loops (review round 2, Issue 17 —
/// the first build leaked them).
pub fn arm_slot_custody(
    routed: &Arc<crate::meta_backend::RoutedMetaBackend>,
    node_id: &str,
    secret: Vec<u8>,
    pr_key: u64,
    sink_for: RecallSinkFor,
) -> Arc<SlotCustodyArm> {
    arm_slot_custody_with_clock(
        routed,
        node_id,
        secret,
        pr_key,
        sink_for,
        LeaseClock::monotonic(),
    )
}

/// [`arm_slot_custody`] naming the custody clients' lease clock (the
/// contracts drive `T_self` on a manual clock; the mount path is
/// monotonic).
pub fn arm_slot_custody_with_clock(
    routed: &Arc<crate::meta_backend::RoutedMetaBackend>,
    node_id: &str,
    secret: Vec<u8>,
    pr_key: u64,
    sink_for: RecallSinkFor,
    clock: LeaseClock,
) -> Arc<SlotCustodyArm> {
    let arm = Arc::new(SlotCustodyArm {
        routed: Arc::downgrade(routed),
        node_id: node_id.to_string(),
        secret,
        pr_key,
        sink_for,
        clock,
        holders: parking_lot::Mutex::new(HashMap::new()),
        stop: Arc::new(AtomicBool::new(false)),
    });
    if let Some(previous) = SLOT_CUSTODY.swap(Some(Arc::clone(&arm))) {
        previous.stop.store(true, Ordering::Release);
    }
    log::info!(
        "symmetric PR 9: write custody by the SLOT HOLDER armed for '{node_id}' — a foreign \
         file's custody is acquired from the appender leasing its slot (tree 0 + \
         SlotHolderCache), the grant carrying the file's records; own files stay local"
    );
    arm
}

/// **The clean leave** (the unmount teardown — `main.rs`, right after the
/// co-writer arm's disarm and BEFORE the membership leave, so no holder
/// still believes this mount holds custody once it stops being a member;
/// the contracts directly — review round 2, Issue 2): for every dialed
/// holder, in order, its tokens are RELEASED (the drain + purge before the
/// holder is told — the token plane's one release path), then EVERY
/// custody grant this mount still holds from it is released and the verb
/// drained (the FUSE layer's cached leases may still be alive at unmount;
/// before this the holder waited a lease TTL for them and its next commit
/// on the file ran a dead-client recall to the deadline — the class PR 5
/// closed for readers), then the renewal loops stop. A writer that dies
/// without this leaves its grants to each holder's lease-expiry arm (the
/// S9 law).
pub async fn disarm_slot_custody() {
    let Some(arm) = SLOT_CUSTODY.swap(None) else {
        return;
    };
    arm.stop.store(true, Ordering::Release);
    let slots: Vec<Arc<HolderSlot>> = arm.holders.lock().drain().map(|(_, s)| s).collect();
    for slot in slots {
        let custody = slot.dial.lock().await.custody.take();
        let Some(h) = custody else {
            continue;
        };
        let planes: Vec<_> = h.planes.lock().values().cloned().collect();
        for plane in planes {
            plane.stop().await;
        }
        if !h.client.fenced() {
            h.client.release_all_grants().await;
        }
    }
    HANDOVER_RECALLS.clear_all();
}

/// **Test seam**: drop the arm WITHOUT the clean leave (a contract's
/// teardown after a panic; the renewal loops stop at their next tick):
/// every dialed holder's grants are left to its lease-expiry arm — the
/// death shape.
pub fn uninstall_slot_custody() {
    if let Some(arm) = SLOT_CUSTODY.swap(None) {
        arm.stop.store(true, Ordering::Release);
    }
    HANDOVER_RECALLS.clear_all();
}

/// **Test seam** (also the stats face's probe): is custody by the slot
/// holder armed in this process?
pub fn slot_custody_armed() -> bool {
    SLOT_CUSTODY.load().is_some()
}

/// The dead-holder fence's arm half (review round 2, Issue 10): the
/// holder at `endpoint` is forgotten — its dial slot dropped (the next
/// acquire re-dials, meeting the dead holder's refusal or PR 10's
/// re-leased slot) and its token planes STOPPED DEAD (no release travels
/// to a holder that answers nothing; every cached entry dropped).
fn holder_fenced(endpoint: &str) {
    let guard = SLOT_CUSTODY.load();
    let Some(arm) = guard.as_ref() else {
        return;
    };
    let slot = arm.holders.lock().remove(endpoint);
    let Some(slot) = slot else {
        return;
    };
    // The slot's dial state is retired UNDER ITS OWN MUTEX (round 3, Issue
    // 27): a concurrent `holder()` that still holds this slot sees
    // `retired` and re-looks the endpoint up instead of re-dialing on an
    // orphan; the planes stop dead through the custody the fenced client
    // belongs to. A dial in flight holds the mutex (toward the dead
    // holder — it fails on its own bound): the retire runs behind it.
    let retire = |dial: &mut HolderDial| {
        dial.retired = true;
        if let Some(h) = dial.custody.take() {
            for plane in h.planes.lock().values() {
                plane.stop_dead();
            }
        }
    };
    if let Ok(mut dial) = slot.dial.try_lock() {
        retire(&mut dial);
        return;
    }
    let slot = Arc::clone(&slot);
    crate::meta_exec::spawn_meta("slot_custody_holder_fenced", async move {
        let mut dial = slot.dial.lock().await;
        dial.retired = true;
        if let Some(h) = dial.custody.take() {
            for plane in h.planes.lock().values() {
                plane.stop_dead();
            }
        }
    });
}

/// Every acquire's deterministic refusal (review round 2, Issue 11):
/// `Refused { errno }` — never `LockFailed`, which the POSIX-5 ladder
/// retries for its whole budget. `EAGAIN` where a later attempt can
/// legitimately succeed (a holder not yet bound, a slot mid-move),
/// `EIO` where the answer cannot change without an operator (no
/// authority armed, a defective frame).
fn acquire_refusal(errno: libc::c_int, reason: String) -> SqueezefsError {
    log::error!("{reason}");
    SqueezefsError::refused(errno, reason)
}

impl SlotCustodyArm {
    /// The dialed holder at `endpoint`, JOINed on first touch (one lease
    /// per holder, its renewal cadence spawned), with a token plane for
    /// set volume `volume` (its standing recall channel started, the
    /// mount's recall sink installed, the holder probed once — a holder
    /// that serves no tokens is found here, not at a later serve).
    /// Single-flight PER HOLDER: two first touches of one holder never
    /// JOIN it twice, and a parked dial of one holder delays no other's.
    async fn holder(
        &self,
        endpoint: &Arc<str>,
        volume: u16,
    ) -> Result<(
        Arc<WriteCustodyClient>,
        Arc<crate::meta_ship::token_plane::TokenReaderPlane>,
    )> {
        use crate::meta_ship::token_plane::{TokenClientConfig, TokenReaderPlane};
        let slot = Arc::clone(
            self.holders
                .lock()
                .entry(Arc::clone(endpoint))
                .or_insert_with(|| {
                    Arc::new(HolderSlot {
                        dial: crate::sqz_sync::SqzMutex::new(HolderDial::default()),
                    })
                }),
        );
        let custody = {
            let mut dial = slot.dial.lock().await;
            if dial.retired {
                // The fence retired this slot under us (Issue 27): the
                // arm's map no longer names it — start over on a fresh
                // slot rather than JOIN on an orphan.
                drop(dial);
                return Err(acquire_refusal(
                    libc::EAGAIN,
                    format!(
                        "PR 9: the slot holder at {endpoint} was fenced under this acquire — \
                         retry: the next acquire re-dials"
                    ),
                ));
            }
            match dial.custody.as_ref() {
                Some(h) if !h.client.fenced() => Arc::clone(h),
                _ => {
                    let backoff = Duration::from_millis(crate::membership::renewal_beat_ms());
                    if let Some(failed) = dial.failed_at {
                        if failed.elapsed() < backoff {
                            return Err(acquire_refusal(
                                libc::EAGAIN,
                                format!(
                                    "PR 9: the slot holder at {endpoint} refused this mount's \
                                     custody JOIN {:?} ago — declined until the next renewal \
                                     beat ({backoff:?}) rather than re-dialing a dead holder \
                                     per acquire",
                                    failed.elapsed()
                                ),
                            ));
                        }
                    }
                    match WriteCustodyClient::connect_slot_holder(
                        endpoint,
                        &self.secret,
                        &self.node_id,
                        self.clock.clone(),
                        self.pr_key,
                    )
                    .await
                    {
                        Ok(client) => {
                            crate::cowriter::spawn_custody_renewal(
                                Arc::clone(&client),
                                Arc::clone(&self.stop),
                            );
                            let h = Arc::new(HolderCustody {
                                client,
                                planes: parking_lot::Mutex::new(HashMap::new()),
                                plane_dials: crate::sqz_sync::SqzMutex::new(()),
                            });
                            dial.failed_at = None;
                            dial.custody = Some(Arc::clone(&h));
                            h
                        }
                        Err(e) => {
                            dial.failed_at = Some(Instant::now());
                            return Err(acquire_refusal(
                                libc::EAGAIN,
                                format!(
                                    "PR 9: the slot holder at {endpoint} refused this mount's \
                                     custody JOIN or could not be reached ({e}) — no custody of \
                                     its files can be acquired until it answers"
                                ),
                            ));
                        }
                    }
                }
            }
        };
        if let Some(plane) = custody.planes.lock().get(&volume) {
            return Ok((Arc::clone(&custody.client), Arc::clone(plane)));
        }
        let _dialing = custody.plane_dials.lock().await;
        if let Some(plane) = custody.planes.lock().get(&volume) {
            return Ok((Arc::clone(&custody.client), Arc::clone(plane)));
        }
        let plane = TokenReaderPlane::new(TokenClientConfig {
            endpoint: endpoint.to_string(),
            secret: self.secret.clone(),
            client_id: self.node_id.clone(),
            volume,
        });
        plane.install_data_sink((self.sink_for)(usize::from(volume)));
        plane.probe().await.map_err(|e| {
            acquire_refusal(
                libc::EIO,
                format!(
                    "PR 9: the slot holder at {endpoint} serves no read tokens for volume \
                     {volume} ({e}) — a custody grant there could carry no records and its \
                     recalls could reach nobody"
                ),
            )
        })?;
        let task = Arc::clone(&plane);
        crate::meta_exec::spawn_meta("slot_custody_recall_channel", async move {
            task.run_recall_channel().await;
        });
        custody.planes.lock().insert(volume, Arc::clone(&plane));
        Ok((Arc::clone(&custody.client), plane))
    }

    async fn dialed(&self, endpoint: &str) -> Option<Arc<HolderCustody>> {
        let slot = Arc::clone(self.holders.lock().get(endpoint)?);
        let dial = slot.dial.lock().await;
        dial.custody.clone()
    }

    /// **Test seam**: the token plane toward `endpoint` for `volume`, if
    /// dialed (the contracts' probe of the carried token's cache).
    pub async fn token_plane(
        &self,
        endpoint: &str,
        volume: u16,
    ) -> Option<Arc<crate::meta_ship::token_plane::TokenReaderPlane>> {
        let h = self.dialed(endpoint).await?;
        let plane = h.planes.lock().get(&volume).cloned();
        plane
    }

    /// **Test seam**: the custody client toward `endpoint`, if dialed (the
    /// contracts' probe of the holder-side lease).
    pub async fn holder_client(&self, endpoint: &str) -> Option<Arc<WriteCustodyClient>> {
        self.dialed(endpoint).await.map(|h| Arc::clone(&h.client))
    }
}

/// Where GLOBAL ino `ino`'s custody is served when it is NOT this mount's:
/// `None` unarmed, or for a slot this mount leases / maintains (the local
/// arbiter — one relaxed load, then PR 6's `step_home`). The lease TABLE
/// decides, not the S4 lock table: a declared region's slot is another
/// appender's for every cross-owner decision (PR 6's law).
pub fn slot_holder_home(ino: u64) -> Option<CustodyHome> {
    use crate::meta_backend::crossvol_tx::{step_home, StepHome};
    let guard = SLOT_CUSTODY.load();
    let arm = guard.as_ref()?;
    let routed = arm.routed.upgrade()?;
    let (v, local) = routed.route_ino(ino);
    match step_home(&routed, v, local) {
        StepHome::Local => None,
        StepHome::Foreign { holder, endpoint } => Some(CustodyHome::Holder {
            holder,
            endpoint,
            volume: u16::try_from(v).ok()?,
            object: local,
        }),
        StepHome::Unreachable { holder } => Some(CustodyHome::Unbound { holder }),
    }
}

/// [`slot_holder_home`] after binding `holder`'s endpoint ON DEMAND
/// (`sym_join::bind_holder_endpoint_on_demand`) — the `Unbound` arm's one
/// retry: `None` when the holder has not published or the home moved to
/// this mount meanwhile.
async fn bind_holder_home(arm: &SlotCustodyArm, ino: u64, holder: u32) -> Option<CustodyHome> {
    let routed = arm.routed.upgrade()?;
    let (v, _) = routed.route_ino(ino);
    crate::sym_join::bind_holder_endpoint_on_demand(routed.volumes.get(v)?, holder).await?;
    slot_holder_home(ino)
}

/// **A WRITER's token plane for a FOREIGN object** (symmetric PR 12b —
/// PR 9's deviation 4 and PR 12's owed "the writer's read divert to its
/// per-holder planes"): volume `vol`'s object lives in a slot appender
/// `holder` serves — another appender's lease, or on a JOINED appender
/// an unleased slot the manager maintains (KD-SYM-2/3) — so its reads are
/// token reads at that holder, through the SAME per-holder plane PR 9's
/// custody arm dials for custody (one JOIN per holder; the plane's
/// standing recall channel + the mount's recall sink come with it).
/// `Ok(None)` = read locally: no arm (a mount without the mount path's
/// `arm_mount_slot_custody` — the in-process fixtures' default), or
/// `vol` is not one of the armed set's volumes (two backends of one
/// process, the arm's routed set another daemon's). A holder with no
/// bound endpoint REFUSES `EAGAIN`-class — the writer's stale projection
/// of a foreign tree is never served in its place (KD-SYM-19, R-SYM-4).
pub async fn foreign_read_plane(
    vol: &crate::meta_backend::kv::backend::KvMetaBackend,
    object: u64,
    holder: u32,
) -> Result<Option<Arc<crate::meta_ship::token_plane::TokenReaderPlane>>> {
    let guard = SLOT_CUSTODY.load();
    let Some(arm) = guard.as_ref() else {
        return Ok(None);
    };
    let Some(routed) = arm.routed.upgrade() else {
        return Ok(None);
    };
    let Some(v) = routed
        .volumes
        .iter()
        .position(|x| std::ptr::eq(Arc::as_ptr(x), vol))
    else {
        return Ok(None);
    };
    // The table first; a holder that joined after this mount's ladder is
    // bound ON DEMAND off durable state (PR 12b, N ≥ 3). Boxed: the
    // resolve reads the claim set through this same divert (ino 1's slot
    // is the manager's — bound at the arm, so the recursion is one level
    // deep by construction).
    let endpoint = Box::pin(crate::sym_join::bind_holder_endpoint_on_demand(
        &routed.volumes[v],
        holder,
    ))
    .await;
    let Some(endpoint) = endpoint else {
        crate::meta_ship::token_plane::note_reader_unbound_holder();
        return Err(unbound_holder(
            holder,
            object,
            "read of an object in its slot",
        ));
    };
    let volume = u16::try_from(v).map_err(|_| {
        SqueezefsError::InvalidOperation(format!(
            "volume ordinal {v} exceeds the wire's u16 volume word"
        ))
    })?;
    let (_client, plane) = match arm.holder(&endpoint, volume).await {
        Ok(h) => h,
        Err(e) => {
            // The holder at `endpoint` is dead or moved: a SUCCESSOR at the
            // same identity publishes a new listener (PR 12b) — re-resolve
            // once and retry there; the same address stays the refusal.
            let Some(moved) = Box::pin(crate::sym_join::rebind_holder_endpoint_if_moved(
                &routed.volumes[v],
                holder,
                &endpoint,
            ))
            .await
            else {
                return Err(e);
            };
            arm.holder(&moved, volume).await?
        }
    };
    // A freshly dialed plane serves nothing until its recall channel's
    // first round lands (`serve_gate`): bounded wait, the serve's own
    // refusal is the honest answer past it.
    if plane.await_channel_fresh().await {
        return Ok(Some(plane));
    }
    // The channel never freshened: the holder at `endpoint` is dead or
    // MOVED (the `sym-crash` fleet leg — a manager failover keeps appender
    // 0's identity and publishes a NEW listener; the plane dialed at the
    // old one refused every root read of every joiner `EIO` for the
    // member's life). Re-resolve once: moved ⇒ the stale plane is stopped
    // dead (its tokens dropped, PR 5's law) and the successor's dialed;
    // the same address ⇒ the stale plane's own refusal stands.
    let Some(moved) = Box::pin(crate::sym_join::rebind_holder_endpoint_if_moved(
        &routed.volumes[v],
        holder,
        &endpoint,
    ))
    .await
    else {
        return Ok(Some(plane));
    };
    plane.stop_dead();
    let (_client, fresh) = arm.holder(&moved, volume).await?;
    fresh.await_channel_fresh().await;
    Ok(Some(fresh))
}

/// **A JOINED appender's custody client at `endpoint`** (symmetric PR
/// 12b): the per-holder client PR 9's arm dials — JOINed on first touch,
/// renewed on its own cadence — for the shipped-free path's lease epoch
/// when this mount is no co-writer (`custody_client()` is `None`) and its
/// data volume's allocation holder is the manager the grant arm named.
/// Refuses `EIO` with no arm (a mount path that armed no slot custody
/// cannot present a lease anywhere).
pub async fn slot_holder_client(endpoint: &str) -> Result<Arc<WriteCustodyClient>> {
    let guard = SLOT_CUSTODY.load();
    let Some(arm) = guard.as_ref() else {
        return Err(acquire_refusal(
            libc::EIO,
            format!(
                "PR 12b: a shipped free to the allocation holder at {endpoint} needs this \
                 mount's custody client there, and custody by the slot holder is not armed"
            ),
        ));
    };
    let (client, _plane) = arm.holder(&Arc::from(endpoint), 0).await?;
    Ok(client)
}

/// [`slot_holder_home`] for a lock object's path form — `Some((ino,
/// home))` only for an inode object whose custody a holder serves.
pub fn slot_holder_home_of_path(file_path: &str) -> Option<(u64, CustodyHome)> {
    // The arm's load before the path parse: the unarmed acquire pays one
    // relaxed load here and nothing else.
    if SLOT_CUSTODY.load().is_none() {
        return None;
    }
    let ino = crate::dlm::ino_of_path(file_path)?;
    slot_holder_home(ino).map(|home| (ino, home))
}

fn unbound_holder(holder: u32, ino: u64, what: &str) -> SqueezefsError {
    acquire_refusal(
        libc::EAGAIN,
        format!(
            "PR 9: inode_{ino}'s slot is leased by appender {holder} and this mount knows no \
             endpoint for it (its ladder has published none — a later joiner is bound by PR \
             12b's wire JoinAppender; a dead holder's \
             slots are re-leased by PR 10's recovery) — refusing the {what} rather than \
             granting custody the holder never issued"
        ),
    )
}

/// **The handover's derived parks and bounds** (round 3, Issues 24/25 —
/// every one a function of the S9 lease clocks, tie-tested in
/// `derivation_sweep_tests`):
///
/// * [`handover_retry_park_for`] — the park between two attempts of a
///   DEFERRED acquire and between two settle polls: `renew / 20` (the
///   recall lands on the standing poll at once and its release follows
///   the settle; the retry should land a small fraction of a beat later —
///   50 ms at the contracts' 1 s beat, 500 ms at the shipped 10 s).
/// * [`handover_recall_bound_for`] — how long a slot stays MID-HANDOVER
///   after its recall began: `2 × renew` (the recall's carrier is at
///   most one beat away, the release one settle behind it; a requester
///   that stopped retrying must not leave the slot's files un-grantable
///   longer).
/// * [`leave_custody_bound_for`] — how long the HOLDER's clean leave
///   waits for its recalled grants' releases: `T_owner + renew` (a writer
///   that never polls is dead by the S9 law at `T_owner` — its `T_self`
///   is strictly earlier — and the sweep retires its grants then).
pub fn handover_retry_park_for(renew_interval: Duration) -> Duration {
    (renew_interval / 20).max(Duration::from_millis(1))
}

/// See [`handover_retry_park_for`].
pub fn handover_recall_bound_for(renew_interval: Duration) -> Duration {
    renew_interval * 2
}

/// See [`handover_retry_park_for`].
pub fn leave_custody_bound_for(t_owner: Duration, renew_interval: Duration) -> Duration {
    t_owner + renew_interval
}

/// **The custody acquire at the slot holder** (`SlotLockManager`'s PR 9
/// arm): dial the holder (JOIN on first touch), one token-wire round trip
/// for custody + the records, the records installed as this writer's
/// token. A `NotHolder` answer RE-RESOLVES the holder through tree 0
/// (review round 2, Issue 15 — the slot may have been handed to THIS
/// mount: `NowLocal`, the caller's local arbiter) once; a second refuses.
/// A `Deferred` answer (the slot mid-handover — its grants recalled)
/// parks briefly, re-resolves and retries inside `ttl` (the handover
/// completes within one renewal beat of the recalled writer's release),
/// then answers the `EAGAIN` class. A carried token whose install fails
/// keeps the custody it was granted (Issue 8 — the caller must never
/// hold custody it does not know about; the next serve fetches).
pub async fn acquire_at_slot_holder(
    home: CustodyHome,
    ino: u64,
    span: Option<(u64, u64)>,
    mode: LockMode,
    ttl: Duration,
) -> Result<HolderAcquire> {
    let arm = SLOT_CUSTODY.load_full().ok_or_else(|| {
        acquire_refusal(
            libc::EIO,
            format!("PR 9: custody by the slot holder disarmed under inode_{ino}'s acquire"),
        )
    })?;
    let (mut holder, mut endpoint, volume, object) = match home {
        CustodyHome::Holder {
            holder,
            endpoint,
            volume,
            object,
        } => (holder, endpoint, volume, object),
        // A holder that joined after this mount's ladder ran: bound ON
        // DEMAND off durable state (PR 12b, N ≥ 3), else the retryable
        // refusal.
        CustodyHome::Unbound { holder } => match bind_holder_home(&arm, ino, holder).await {
            Some(CustodyHome::Holder {
                holder,
                endpoint,
                volume,
                object,
            }) => (holder, endpoint, volume, object),
            _ => return Err(unbound_holder(holder, ino, "acquire")),
        },
    };
    let started = Instant::now();
    let mut redirected = false;
    loop {
        let (client, tokens) = match arm.holder(&endpoint, volume).await {
            Ok(h) => h,
            Err(e) => {
                // A dead holder's SUCCESSOR publishes a new listener (PR
                // 12b): re-resolve once, retry there; the same address
                // stays the refusal.
                let moved = arm
                    .routed
                    .upgrade()
                    .and_then(|r| r.volumes.get(usize::from(volume)).cloned());
                let Some(vol) = moved else {
                    return Err(e);
                };
                let Some(fresh) = Box::pin(crate::sym_join::rebind_holder_endpoint_if_moved(
                    &vol, holder, &endpoint,
                ))
                .await
                else {
                    return Err(e);
                };
                endpoint = fresh;
                arm.holder(&endpoint, volume).await?
            }
        };
        let gen0 = tokens.recall_generation(object);
        match client
            .acquire_carrying_token(volume, object, span, mode, ttl)
            .await?
        {
            CarriedAcquire::Granted { lease, records, .. } => {
                // The holder re-resolved AFTER the grant (round 3 — the
                // stale-resolve window the flat ×20 found: a request
                // resolved to the old holder before the move and served
                // one RTT after it, when the slot read Unleased and the old
                // holder held nothing to grant). A grant from an appender
                // tree 0 no longer names as the slot's holder is RELEASED
                // (the lease's drop is the release verb) and the acquire
                // re-runs where tree 0 now points — the writer side of the
                // same Dekker pair the served side runs.
                let still_holder = matches!(
                    slot_holder_home(ino),
                    Some(CustodyHome::Holder { holder: h, .. }) if h == holder
                );
                if !still_holder {
                    STALE_HOLDER_GRANTS.fetch_add(1, Ordering::Relaxed);
                    log::info!(
                        "PR 9: appender {holder} at {endpoint} granted inode_{ino}'s custody \
                         after its slot moved (a request resolved before the move) — released, \
                         re-acquiring where tree 0 points (dlm_custody_stale_holder_grants)"
                    );
                    drop(lease);
                    client.drain_releases().await;
                    if redirected {
                        return Err(acquire_refusal(
                            libc::EAGAIN,
                            format!(
                                "PR 9: inode_{ino}'s slot moved twice under one acquire (last \
                                 holder {holder} at {endpoint}) — refusing rather than chasing \
                                 a handover storm; the caller retries"
                            ),
                        ));
                    }
                    redirected = true;
                    match slot_holder_home(ino) {
                        None => return Ok(HolderAcquire::NowLocal),
                        Some(CustodyHome::Holder {
                            holder: next,
                            endpoint: next_endpoint,
                            ..
                        }) => {
                            holder = next;
                            endpoint = next_endpoint;
                            continue;
                        }
                        Some(CustodyHome::Unbound { holder: next }) => {
                            return Err(unbound_holder(next, ino, "acquire"))
                        }
                    }
                }
                VIA_SLOT_HOLDER.fetch_add(1, Ordering::Relaxed);
                match tokens
                    .install_carried(object, records, gen0, &TOKEN_CARRIED_PAGES)
                    .await
                {
                    Ok(Some(_)) => {
                        TOKEN_CARRIED.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        // Issue 8: custody was GRANTED — it is the caller's
                        // whatever became of the token (a failed install
                        // costs one later fetch, never a stranded grant).
                        TOKEN_CARRY_FAILURES.fetch_add(1, Ordering::Relaxed);
                        log::warn!(
                            "PR 9: the token carried with inode_{ino}'s custody grant from \
                             {endpoint} could not be installed ({e}) — custody kept, the next \
                             resolve fetches (dlm_custody_token_carry_failures)"
                        );
                    }
                }
                return Ok(HolderAcquire::Granted(lease));
            }
            CarriedAcquire::NotHolder { holder: current } => {
                if redirected {
                    return Err(acquire_refusal(
                        libc::EAGAIN,
                        format!(
                            "PR 9: inode_{ino}'s slot moved twice under one acquire (last \
                             holder {holder} at {endpoint}, now {current}) — refusing rather \
                             than chasing a handover storm; the caller retries"
                        ),
                    ));
                }
                redirected = true;
                match slot_holder_home(ino) {
                    None => {
                        log::info!(
                            "PR 9: appender {holder} at {endpoint} no longer leases inode_{ino}'s \
                             slot — the slot is THIS mount's now; the local arbiter serves"
                        );
                        return Ok(HolderAcquire::NowLocal);
                    }
                    Some(CustodyHome::Holder {
                        holder: next,
                        endpoint: next_endpoint,
                        ..
                    }) => {
                        log::info!(
                            "PR 9: appender {holder} at {endpoint} no longer leases inode_{ino}'s \
                             slot — redirected to appender {next} at {next_endpoint}"
                        );
                        holder = next;
                        endpoint = next_endpoint;
                    }
                    Some(CustodyHome::Unbound { holder: next }) => {
                        return Err(unbound_holder(next, ino, "acquire"))
                    }
                }
            }
            CarriedAcquire::Deferred { reason } => {
                let park = client.handover_retry_park();
                let elapsed = started.elapsed();
                if elapsed + park > ttl {
                    // The slot is still mid-handover past this attempt's
                    // budget: the CONFLICT class — the POSIX-5 ladder
                    // retries it for the op's budget (a write blocks,
                    // never fails, while the slot moves).
                    return Err(SqueezefsError::LockFailed { reason });
                }
                squeezefs_ipc::sqz_time::sleep(park).await;
                // The slot may have moved meanwhile: re-resolve (to THIS
                // mount = local; to another holder = redirect; else the
                // same holder, asked again).
                match slot_holder_home(ino) {
                    None => return Ok(HolderAcquire::NowLocal),
                    Some(CustodyHome::Holder {
                        holder: next,
                        endpoint: next_endpoint,
                        ..
                    }) => {
                        holder = next;
                        endpoint = next_endpoint;
                    }
                    Some(CustodyHome::Unbound { holder: next }) => {
                        return Err(unbound_holder(next, ino, "acquire"))
                    }
                }
            }
        }
    }
}

/// The S11 ranged acquire at the slot holder: the custody wire's own
/// required/desired verb on the holder's JOINed client (the token
/// carriage is the whole-file grant's — a range holder reads under the
/// S8 token cache the S11 plane already rides).
pub async fn acquire_range_at_slot_holder(
    home: CustodyHome,
    ino: u64,
    required: (u64, u64),
    desired: (u64, u64),
    ttl: Duration,
) -> Result<crate::dlm::RangeAcquired> {
    let arm = SLOT_CUSTODY.load_full().ok_or_else(|| {
        acquire_refusal(
            libc::EIO,
            format!("PR 9: custody by the slot holder disarmed under inode_{ino}'s acquire"),
        )
    })?;
    let (endpoint, volume) = match home {
        CustodyHome::Holder {
            endpoint, volume, ..
        } => (endpoint, volume),
        CustodyHome::Unbound { holder } => {
            return Err(unbound_holder(holder, ino, "range acquire"))
        }
    };
    let (client, _) = arm.holder(&endpoint, volume).await?;
    VIA_SLOT_HOLDER.fetch_add(1, Ordering::Relaxed);
    match client.acquire_range(ino, required, desired, ttl).await? {
        RangeAcquireOutcome::New { lease, span, .. } => {
            Ok(crate::dlm::RangeAcquired::New { lease, span })
        }
        RangeAcquireOutcome::Extended { token, span, .. } => {
            Ok(crate::dlm::RangeAcquired::Extended { token, span })
        }
    }
}

// ---- Custody across a handover (design §5.1.4) ----------------------------

/// The slots marked MID-HANDOVER for the grant paths (the
/// [`HandoverMarkCore`] — review round 4, Issue 29: `#[path]`-shared with
/// `loom-models/`, the mark / re-check pair modeled there): a grant of an
/// object in one of them is DEFERRED (`CUSTODY_DEFERRED`) until the slot
/// has moved — else the recalled writer's re-acquire would undo the recall
/// and the handover would never complete, or (round 3's ×10 finding) land
/// at the OLD holder inside the flush-then-transfer and span the move. Two
/// lifetimes: a mark armed by a DEFERRED tick expires after
/// [`handover_recall_bound`] (a requester that stopped retrying must not
/// leave the slot's files un-grantable; the next attempt re-arms it); a
/// mark HELD by a completing handover ([`HandoverCustodyMark`]) or by the
/// leave never expires — it is cleared at the handover's terminal outcome,
/// or dies with the leaving process (the disarm clears it). Keyed on the
/// volume's superblock uuid + forest slot.
static HANDOVER_RECALLS: Lazy<HandoverMarkCore<HandoverKey>> = Lazy::new(HandoverMarkCore::new);

type HandoverKey = (u128, crate::meta_backend::kv::record::ForestSlot);

/// **A handover in flight holds its slot's mark** (round 3 — the ×10
/// stamped finding: the completing tick found no live grant, CLEARED the
/// mark and ran the flush-then-transfer unmarked, so a recalled writer's
/// retry inside that window was GRANTED at the old holder and the grant
/// spanned the move — `held() == 1` at the old holder after it). The
/// mark is armed BEFORE the handover's grant census and dropped at the
/// handover's TERMINAL outcome (the slot moved, or the release aborted
/// and the slot stayed) — with the grant paths' post-grant re-check
/// ([`handover_recall_defers`] read again AFTER `owner.grant`, both
/// sides under the owner's grant lock and the mark's) this is the Dekker
/// pair: a grant that landed before the census is seen and recalled, one
/// that landed after sees the mark and releases itself DEFERRED. A later
/// re-arm (the next tick's, the leave's) is preserved: the drop clears
/// the mark only if it still carries this handover's monotone stamp
/// (round 4, Issue 30 — never an `Instant`). Its lifetime is a NAMED act:
/// `release_slot_handover_locked` binds it, runs the transfer, and drops
/// it at the transfer's one terminal point (round 4, Issue 29).
#[must_use = "the mark is cleared when this guard drops — hold it through the transfer"]
pub struct HandoverCustodyMark {
    key: HandoverKey,
    seq: u64,
}

impl Drop for HandoverCustodyMark {
    fn drop(&mut self) {
        HANDOVER_RECALLS.clear_if(self.key, self.seq);
    }
}

/// [`defer_handover_for_custody`]'s verdict.
pub enum HandoverCustody {
    /// Live grants on the slot were RECALLED (`recalled` newly this tick;
    /// the rest were already recalled) — the handover is DEFERRED; the
    /// mark stands until the next tick re-arms it or the bound expires.
    Deferred { recalled: usize },
    /// No live grant — the handover may proceed; the mark stands while
    /// the guard lives.
    Clear(HandoverCustodyMark),
}

/// How long a slot stays "mid-handover" for the grant path after its
/// recall began: two renewal beats of the holder's S9 clocks — the recall
/// lands on the writer's standing poll at once (or its next carrier, one
/// beat), its release follows once the ino is quiescent, and the
/// requester's next cadence tick completes the move. Derived, never a
/// constant.
fn handover_recall_bound(owner: &WriteCustodyOwner) -> Duration {
    handover_recall_bound_for(owner.clocks.renew_interval)
}

/// The mid-handover mark's bound in force (`slot_handover_recall_bound_ms`
/// — published beside `slot_handover_custody_deferrals`; 0 with no custody
/// authority armed).
pub fn handover_recall_bound_ms() -> u64 {
    custody_owner().map_or(0, |o| handover_recall_bound(&o).as_millis() as u64)
}

/// Is `ino`'s slot mid-handover — its custody grants recalled and the
/// slot not yet moved? The grant paths' one question (the token-wire
/// `CustodyGrant` and the custody-wire ACQUIRE alike). One relaxed load
/// unless a handover is in flight.
pub fn handover_recall_defers(ino: u64) -> bool {
    if HANDOVER_RECALLS.pending() == 0 {
        return false;
    }
    let Some(owner) = custody_owner() else {
        return false;
    };
    let guard = SLOT_CUSTODY.load();
    let Some(arm) = guard.as_ref() else {
        return false;
    };
    let Some(routed) = arm.routed.upgrade() else {
        return false;
    };
    let (v, local) = routed.route_ino(ino);
    let Some(vol) = routed.volumes.get(v) else {
        return false;
    };
    let key = (
        u128::from_le_bytes(vol.superblock().uuid),
        crate::meta_backend::kv::record::forest_slot_of_ino(local),
    );
    HANDOVER_RECALLS.defers(key, handover_recall_bound(&owner))
}

/// The live grants this process's custody authority issued on files of
/// forest `slot` of the volume whose superblock uuid is `volume_uuid`:
/// their GLOBAL inos. Empty unarmed (no arm, no owner — the shipped S9
/// posture, where a co-writer's grants at the authority name no slot
/// lease). O(live grants), off the owner's grant snapshot by value
/// (review round 2, Issue 18 — no `String` clones).
fn slot_custody_inos(
    volume_uuid: u128,
    slot: crate::meta_backend::kv::record::ForestSlot,
) -> Vec<u64> {
    let Some(owner) = custody_owner() else {
        return Vec::new();
    };
    let guard = SLOT_CUSTODY.load();
    let Some(arm) = guard.as_ref() else {
        return Vec::new();
    };
    let Some(routed) = arm.routed.upgrade() else {
        return Vec::new();
    };
    let mut inos: Vec<u64> = owner
        .grants_snapshot_with(|_, g| {
            let (v, local) = routed.route_ino(g.ino);
            (routed
                .volumes
                .get(v)
                .is_some_and(|vol| u128::from_le_bytes(vol.superblock().uuid) == volume_uuid)
                && crate::meta_backend::kv::record::forest_slot_of_ino(local) == slot)
                .then_some(g.ino)
        })
        .into_iter()
        .flatten()
        .collect();
    inos.sort_unstable();
    inos.dedup();
    inos
}

/// **Custody across a handover** (design §5.1.4 — "the departing holder's
/// outstanding custody leases"): does a live grant this holder issued
/// name a file of forest `slot` on the volume whose superblock uuid is
/// `volume_uuid`? The contracts' pure read; the handover's own act is
/// [`defer_handover_for_custody`].
pub fn slot_custody_live(
    volume_uuid: u128,
    slot: crate::meta_backend::kv::record::ForestSlot,
) -> bool {
    !slot_custody_inos(volume_uuid, slot).is_empty()
}

/// **The handover's custody act** (`release_slot_handover_locked`'s ONE
/// call, review round 2 — Issues 5/9, design §5.1.4 flush-then-transfer):
/// a slot whose files a writer holds custody of from this holder does NOT
/// move yet — the grant is live work (the writer's DMA is authorized
/// under it; a new holder's arbiter would know nothing of it and could
/// grant the same bytes twice) — and the S9 channel to the writer is
/// PULL-based, so the handover is DEFERRED (`Some(recalled)` → the
/// requester's retryable `Deferred` class, counted
/// `slot_handover_custody_deferrals`) and BOUNDED: every live grant on
/// the slot is RECALLED through that channel ([`RecallNotice`] — the
/// writer releases once the file's pipeline is quiescent, within one
/// renewal beat) and the slot is marked mid-handover so no re-acquire
/// undoes the recall; the next cadence tick finds the grants gone and
/// answers `None` — the slot moves, and the file's next custody comes
/// from the new holder. A grant never spans a handover: no write lands
/// under a stale holder's custody, and no acked write is lost (nothing is
/// voided — the recall is not the `dead_grants` revocation). The
/// `Clear` verdict carries the HELD mark (round 3): the caller keeps it
/// through the flush-then-transfer, so a re-acquire inside the transfer
/// is deferred to the new holder instead of granted at this one.
pub fn defer_handover_for_custody(
    volume_uuid: u128,
    slot: crate::meta_backend::kv::record::ForestSlot,
) -> HandoverCustody {
    let key = (volume_uuid, slot);
    // The mark FIRST, held: the Dekker's arm half (a grant that lands
    // after the census below reads it and defers itself).
    let seq = HANDOVER_RECALLS.arm(key, true);
    match recall_slot_custody(volume_uuid, slot) {
        Some(recalled) => {
            // Deferred: the mark stays armed but UNHELD — it expires at the
            // bound if the requester abandons the handover.
            HANDOVER_RECALLS.unhold(key);
            HANDOVER_CUSTODY_DEFERRALS.fetch_add(1, Ordering::Relaxed);
            HandoverCustody::Deferred { recalled }
        }
        None => HandoverCustody::Clear(HandoverCustodyMark { key, seq }),
    }
}

/// The recall itself (the handover's and the leave's ONE mechanism), run
/// with `slot`'s mark ALREADY armed by the caller: `Some(newly recalled)`
/// while live grants exist on the slot — every live grant recalled
/// (idempotent per grant) — `None` when nothing is live. Never clears the
/// mark: its lifetime is the caller's (the handover's guard, the leave's
/// process, the deferred tick's bound).
fn recall_slot_custody(
    volume_uuid: u128,
    slot: crate::meta_backend::kv::record::ForestSlot,
) -> Option<usize> {
    let inos = slot_custody_inos(volume_uuid, slot);
    if inos.is_empty() {
        return None;
    }
    let owner = custody_owner()?;
    let recalled = owner.recall_grants_on(&inos);
    if recalled > 0 {
        log::info!(
            "PR 9: forest slot {slot} is mid-handover — {} file(s) under live custody; \
             {recalled} grant(s) recalled through the S9 pull channel (the writers release \
             once quiescent)",
            inos.len()
        );
    }
    Some(recalled)
}

/// **Test seam**: the forest slots currently marked mid-handover on any
/// volume of this process.
pub fn handover_recalls_pending() -> usize {
    HANDOVER_RECALLS.pending()
}

// ---- The custody quarantine on the death path (PR 10, Issue 34) ---------

/// Slots recovered from an EARLY death record (`squeezefs appender clear`,
/// PR 8's same-node takeover — recorders that can run INSIDE a surviving
/// custody writer's `T_self`; the plane's own recorders cannot, `T_self <
/// T_owner`): the dead holder's grants on the slot's files may still stand
/// at their writers, so the recovered slot's arbiter grants NOTHING fresh
/// on those files until `record.ts_ms + T_self` — the S7 dead-epoch
/// quarantine's shape, per slot (the dead holder's grant table died with
/// it, so its files are not enumerable; the slot is the unit). Keyed on
/// the volume's superblock uuid + forest slot, valued the Unix-ms deadline;
/// entries expire lazily. Empty on every mount that never recovered an
/// early record.
static CUSTODY_QUARANTINE: Lazy<parking_lot::Mutex<HashMap<HandoverKey, u64>>> =
    Lazy::new(|| parking_lot::Mutex::new(HashMap::new()));
/// The map's population — the grant paths' one relaxed load on every
/// mount with no quarantine (never the mutex).
static CUSTODY_QUARANTINE_LIVE: AtomicU64 = AtomicU64::new(0);
static CUSTODY_QUARANTINE_REFUSALS: AtomicU64 = AtomicU64::new(0);

/// The recovery's arm (step 7b, after the slots went `Unleased`): refuse
/// fresh custody grants on forest `slot`'s files until `until_ms` (Unix
/// ms). A later, longer quarantine of the same slot extends it; a shorter
/// one never shortens it.
pub fn quarantine_slot_custody(
    volume_uuid: u128,
    slot: crate::meta_backend::kv::record::ForestSlot,
    until_ms: u64,
) {
    let mut q = CUSTODY_QUARANTINE.lock();
    let e = q.entry((volume_uuid, slot)).or_insert(0);
    *e = (*e).max(until_ms);
    CUSTODY_QUARANTINE_LIVE.store(q.len() as u64, Ordering::Release);
}

/// **The quarantine's bound** (review round 8, Issue 36): `T_owner + 2 ×
/// skew_max` past the death record's `ts_ms`. `T_owner` is the OWNER's
/// view of a custody lease — the instant past which a holder may re-grant
/// (`T_self = T_owner − 2·skew_max − D_purge` is the MEMBER's stricter
/// self-fence, the wrong side for an owner-side quarantine); `ts_ms` is
/// the RECORDER's wall clock compared against the quarantining node's, so
/// the bound absorbs `2 × skew_max` of disagreement between them. The
/// clocks are the installed custody authority's (the fleet configuration
/// the dead holder's writers ran under), else the shipped derivation;
/// tie-tested in `derivation_sweep_tests`.
pub fn custody_quarantine_bound_ms() -> u64 {
    custody_quarantine_bound_for(
        &custody_owner()
            .map(|o| o.clocks().clone())
            .or_else(|| LeaseClocks::derive(Duration::ZERO).ok())
            .unwrap_or_else(|| {
                LeaseClocks::with_params(
                    Duration::from_secs(crate::fuse_client::CLIENT_STALE_TTL_SECS),
                    Duration::ZERO,
                    Duration::ZERO,
                )
                .expect("a zero-skew, zero-purge clock set is admissible")
            }),
    )
}

/// The bound's law over one clock set (the tie test's subject).
pub fn custody_quarantine_bound_for(clocks: &LeaseClocks) -> u64 {
    (clocks.t_owner + 2 * clocks.skew_max).as_millis() as u64
}

/// Is GLOBAL ino `ino`'s slot under the death-path custody quarantine?
/// `Some(remaining)` while it is — the grant paths' one question beside
/// [`handover_recall_defers`], answered with the retryable class. One
/// relaxed load on every mount with no quarantine.
pub fn custody_quarantine_remaining(ino: u64) -> Option<Duration> {
    if CUSTODY_QUARANTINE_LIVE.load(Ordering::Acquire) == 0 {
        return None;
    }
    let guard = SLOT_CUSTODY.load();
    let arm = guard.as_ref()?;
    let routed = arm.routed.upgrade()?;
    let (v, local) = routed.route_ino(ino);
    let vol = routed.volumes.get(v)?;
    let key = (
        u128::from_le_bytes(vol.superblock().uuid),
        crate::meta_backend::kv::record::forest_slot_of_ino(local),
    );
    let now = crate::meta_backend::kv::alloc_lease::unix_now_ms();
    let mut q = CUSTODY_QUARANTINE.lock();
    let until = *q.get(&key)?;
    if now >= until {
        q.remove(&key);
        CUSTODY_QUARANTINE_LIVE.store(q.len() as u64, Ordering::Release);
        return None;
    }
    CUSTODY_QUARANTINE_REFUSALS.fetch_add(1, Ordering::Relaxed);
    Some(Duration::from_millis(until - now))
}

/// The quarantine's refusals (`slot_custody_quarantine_refusals`), and the
/// slots under quarantine right now (`slot_custody_quarantined`).
pub fn custody_quarantine_stats() -> (u64, usize) {
    let now = crate::meta_backend::kv::alloc_lease::unix_now_ms();
    let live = CUSTODY_QUARANTINE
        .lock()
        .values()
        .filter(|until| **until > now)
        .count();
    (CUSTODY_QUARANTINE_REFUSALS.load(Ordering::Relaxed), live)
}

/// **Test seam**: drop every quarantine (a contract's teardown).
pub fn test_clear_custody_quarantine() {
    CUSTODY_QUARANTINE.lock().clear();
    CUSTODY_QUARANTINE_LIVE.store(0, Ordering::Release);
}

/// **The HOLDER's clean leave is a handover to nobody** (round 3, Issue
/// 22 — design §5.1.4 extended to the leave): before a covered region's
/// slots are released to `Unleased`, every live custody grant this
/// holder issued on their files is RECALLED (the same recall as the
/// handover's — one mechanism) and the leave WAITS for the releases, at
/// the derived park, up to [`leave_custody_bound_for`] (`T_owner + renew`
/// — a writer that never polls is dead by the S9 law at `T_owner`; the
/// sweep at the bound retires its grants). Returns the slots whose grants
/// are STILL live past the bound: the caller keeps those slots leased
/// (the uncovered-region posture — loud; the next open recovers them as
/// own residue) rather than let another lessee grant the same file while
/// a writer still holds this holder's custody. Empty unarmed (no owner /
/// no arm), the shipped leave verbatim. Every held slot's mark is armed
/// HELD before the census and never cleared here (round 3 — the leave's
/// half of the Dekker pair with the grant paths' re-check): a grant that
/// lands at a leaving holder after its census sees the mark and defers
/// itself to the slot's next holder; the marks die with the process (the
/// disarm clears them).
pub async fn recall_custody_at_leave(
    volume_uuid: u128,
    slots: &[crate::meta_backend::kv::record::ForestSlot],
) -> Vec<crate::meta_backend::kv::record::ForestSlot> {
    let Some(owner) = custody_owner() else {
        return Vec::new();
    };
    if SLOT_CUSTODY.load().is_none() {
        // No arm: no grant of this process names a slot (the shipped S9
        // posture) — nothing to recall, no mark to hold.
        return Vec::new();
    }
    for s in slots {
        HANDOVER_RECALLS.arm((volume_uuid, *s), true);
    }
    let live: Vec<_> = slots
        .iter()
        .copied()
        .filter(|s| recall_slot_custody(volume_uuid, *s).is_some())
        .collect();
    if live.is_empty() {
        return Vec::new();
    }
    let park = handover_retry_park_for(owner.clocks.renew_interval);
    let bound = leave_custody_bound_for(owner.clocks.t_owner, owner.clocks.renew_interval);
    log::warn!(
        "PR 9: the clean leave found live custody grants on {} slot(s) — recalled; waiting up \
         to {bound:?} for the writers' releases before the slots are released",
        live.len()
    );
    let started = Instant::now();
    loop {
        let still: Vec<_> = live
            .iter()
            .copied()
            .filter(|s| slot_custody_live(volume_uuid, *s))
            .collect();
        if still.is_empty() {
            return Vec::new();
        }
        if started.elapsed() >= bound {
            // The S9 sweep: a writer that never released past T_owner is
            // dead by the law (its T_self was strictly earlier).
            let swept = owner.expire_due();
            if !swept.is_empty() {
                log::warn!(
                    "PR 9: the leave's bound {bound:?} passed with {} recalled writer(s) \
                     unreleased — swept at T_owner: {:?}",
                    swept.len(),
                    swept.iter().map(|d| d.client.as_str()).collect::<Vec<_>>()
                );
            }
            let still: Vec<_> = live
                .iter()
                .copied()
                .filter(|s| slot_custody_live(volume_uuid, *s))
                .collect();
            if !still.is_empty() {
                log::error!(
                    "PR 9: the clean leave keeps {} slot(s) LEASED — a live custody grant on \
                     them survived the recall and the T_owner sweep ({still:?}); the next open \
                     recovers them as own residue",
                    still.len()
                );
            }
            return still;
        }
        // Keep the recall current for grants minted meanwhile.
        for s in &live {
            recall_slot_custody(volume_uuid, *s);
        }
        squeezefs_ipc::sqz_time::sleep(park).await;
    }
}

/// **The mount path's arm** (`main.rs`, after PR 8's allocation arm): on
/// an ARMED symmetric set — some volume leases slots — arm custody by the
/// slot holder with this mount's KD-MW-2 member id (the appender page's
/// identity, the string every plane knows it by), the set's cluster secret
/// (the `job:enroll` record — possession of volume access IS membership,
/// ruling D2) and the per-volume `MountRecallSink`. `Ok(false)`, one
/// `Option` test per volume, on every unarmed mount; `Ok(false)` with a
/// WARN when the set carries no cluster secret (no cluster listener was
/// ever enabled, so no holder is dialable — a foreign file's acquire then
/// meets the S9 refusal naming the multi-writer mount). The registrant
/// key travels as 0 (the S9 `connect` default): the armed mount's WERO
/// registration is the metadata namespace's (`SQUEEZEFS_META_PR_WERO`),
/// and the per-namespace key carriage is the join ladder's rung 4 (PR 12). The leave is
/// [`disarm_slot_custody`], in the teardown's outside-in order.
pub async fn arm_mount_slot_custody(
    routed: &Arc<crate::meta_backend::RoutedMetaBackend>,
    router: &crate::routing::DataRouter,
) -> Result<bool> {
    let Some(armed) = routed.volumes.iter().find(|v| v.slot_lease_armed()) else {
        return Ok(false);
    };
    let Some(set) = armed.appenders_public() else {
        return Ok(false);
    };
    let node_id =
        crate::cowriter::node_member_id_of(set.identity.node_token, set.identity.mount_slot);
    let Some(first) = routed.volumes.first() else {
        return Ok(false);
    };
    let Some(secret) = crate::membership::cluster_secret(first).await else {
        log::warn!(
            "symmetric PR 9: custody by the slot holder NOT armed — the set carries no cluster \
             secret (no job:enroll record: enable the cluster listener, SQUEEZEFS_JOB_WIRE_BIND, \
             on the writer that formats it); a foreign file's write custody cannot be acquired \
             on this mount"
        );
        return Ok(false);
    };
    let router = router.clone();
    let sink_for: RecallSinkFor = Arc::new(move |volume: usize| {
        crate::meta_ship::token_plane::MountRecallSink::new(router.clone(), volume)
            as Arc<dyn crate::meta_ship::token_plane::RecallDataSink>
    });
    // PR 12 (PR 9's deviation 6): the registrant key a holder's JOIN
    // carries is the one the join ladder's rung 4 registered on the data
    // namespaces (S7's standing hold) — 0 on the detection-grade lab
    // posture, where no key exists (the S9 `connect` default).
    let pr_key = crate::data_custody::live_wero_key().unwrap_or(0);
    arm_slot_custody(routed, &node_id, secret, pr_key, sink_for);
    Ok(true)
}
