//! **DLM stage S8 — metadata function shipping**: the stage that makes the
//! metadata plane multi-writer (pre-RC engineering spec §6.7 decision 1,
//! §6.9 stage **S8**, §6.10 risks **R1**/**R4**; execution plan Phase 4,
//! rulings **D4**, **D9**, **D10**, **D11**).
//!
//! Spec §6.7 decision 1 is the whole design, verbatim: *"**Metadata
//! authority is ownable, not lockable.** The KV engine is RAM-authoritative
//! and single-writer by construction, so metadata mutations are
//! **function-shipped to the volume's owner**, not lock-shipped. The 4a
//! `DlmGuard`s stay exactly where they are — a remote node has nothing to
//! serialize into, and inserting a round trip inside a server already at
//! ρ ≈ 0.92 would multiply through the queueing formula."*
//!
//! # The five pieces, and where each lives
//!
//! | Piece | Module | The sentence it implements |
//! |---|---|---|
//! | ownership plane | [`owners`] | §6.10 **R4**: granularity is the VOLUME (per-volume claim, ring, bitmap, ledger, node cache) |
//! | verb vocabulary | [`wire`] | the trait census, the idempotency key, the era, the grant piggyback |
//! | client router | [`router`] | §6.7 decision 3: the uncontended case costs zero network ops; pipelining is the batch |
//! | owner service | [`service`] | §6.7's venue rule + the dedup window + the era and grace gates |
//! | token cache | [`tokens`] | §6.5 item 1's ≥ 99.5 %-locally-served requirement, and S4's fencing-read contract |
//!
//! # What this stage does NOT do, stated plainly
//!
//! * **Cross-OWNER verbs refuse loud, naming S3.5.** `rename`/`link`
//!   across volumes owned by different nodes need ruling D4's
//!   intent-record + compensation machinery, which is not built. Shipping
//!   such an op to one of the two owners would be strictly worse than
//!   today's (already non-atomic, DUR-7-tracked) local cross-volume path:
//!   non-atomic across *owners*, with no compensation record and no single
//!   D0 guard covering both halves. Refusing is the honest answer; the
//!   refusal names the machinery so the seam is discoverable.
//! * **No remote CUSTODY transfer.** The grant that rides a reply is the
//!   object's *generation* (what a fencing read needs), not a lease held
//!   on the client's behalf. Remote custody with TTL, renewal, revocation
//!   and dead-epoch quarantine is S9; until it lands the data plane
//!   refuses a foreign-home lease loud (S4's own refusal), which is
//!   correct and is exactly the gap S9 closes.
//! * **No production arm.** [`arm_ownership`] has no caller in `main`,
//!   `mount`, or any knob: the multi-writer mount belongs to S9, because a
//!   mount that ships metadata but cannot ship data custody is not a
//!   product. The arm is public and tested so the shipped and refused
//!   behaviours are real rather than commentary (the
//!   `dlm_slot::test_set_local_slots` precedent).
//! * **The FUSE daemon is not switched onto the router.** It holds
//!   `Arc<RoutedMetaBackend>` and uses the *non-trait* capability surface
//!   (`create_with_rdev_size`, `readdir_stream`, `xattr_value_cap`,
//!   `set_layout_and_size`, `merge_layout_and_size`, `commit_block_refs`,
//!   `park_write_times`, `destroy_inodes`) that S8 deliberately does not
//!   ship — those are the data plane's publish path. Wiring the daemon is
//!   therefore an S9 deliverable, not a missing S8 line.
//! * **No incompat bit.** Function shipping changes no on-disk structure,
//!   and ownership needs no new durable record: each volume's D0
//!   `writer_claim` already names its holder and (since S2) its durable
//!   `term`, so **the claim holder IS the owner**. With one owner per
//!   volume every §6.2 single-appender structure keeps exactly one
//!   appender. Bit 11 stays free for a stage that genuinely needs one
//!   (pinned by `tests/meta_ship_tests.rs`).
//!
//! # The measured half is deferred (ruling D11)
//!
//! §6.9's S8 gate is *"serial `tar -x` A/B, published even if it
//! regresses"*, and §6.10 R1 prices the risk at 9,100/s → 6.7–20 k/s at
//! 50–150 µs RTT. That row is **not** run here: D11 freezes benches, rigs
//! and suites until the DLM can serve N readers and writers. What this
//! stage owes the deferred row is *attribution*, and that is what the
//! phase tables below are for — when the row runs, the regression can be
//! decomposed into queue wait, encode, RTT, decode and owner-side execute
//! instead of being a single mystery number.

pub mod owners;
pub mod router;
pub mod service;
pub mod tokens;
pub mod wire;

pub use owners::{
    arm_ownership, constrain_mint_volume, disarm_ownership, owner_map, owner_of_volume,
    ownership_armed, owns_volume, OwnerMap, PeerOwner,
};
pub use router::{MetaShipRouter, VerbRoute, TEST_SHIP_DRAIN_HOLD_MS};
pub use service::{owner_authority_token, MetaShipService, ServiceStats};
pub use tokens::{
    cache_cap, foreign_fencing_token, record_grant, test_clear_token_cache, token_cache_stats,
    TokenCacheStats,
};
pub use wire::*;

use crate::error::SqueezefsError;
use crate::fuse_client::LatencyHistogram;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// The ledger. Every counter is 0 on a solo mount BY CONSTRUCTION (nothing
// is armed, so nothing routes anywhere but locally) — which is what makes
// a nonzero shipped/served/refusal count on a single-node mount a bug
// rather than load, exactly as `dlm_rpcs` is for S4.
// ---------------------------------------------------------------------------

/// Is an ownership plane installed (the one-relaxed-load fast path).
pub(crate) static OWNERSHIP_ARMED: AtomicBool = AtomicBool::new(false);
/// Arm events — a remastering gauge.
pub(crate) static OWNERSHIP_ARMS: AtomicU64 = AtomicU64::new(0);
/// Verbs that took today's local path.
pub(crate) static LOCAL_VERBS: AtomicU64 = AtomicU64::new(0);
/// Verbs shipped to an owner (client side).
pub(crate) static SHIPPED_VERBS: AtomicU64 = AtomicU64::new(0);
/// Verbs executed for a peer (owner side).
pub(crate) static SERVED_VERBS: AtomicU64 = AtomicU64::new(0);
/// Frames shipped — the pipelining denominator.
pub(crate) static BATCHES: AtomicU64 = AtomicU64::new(0);
/// Verbs carried by those frames — `batched_verbs / batches` is the live
/// coalesce factor (≈ 1 on a serial stream, which is R1's cost made
/// visible rather than hidden).
pub(crate) static BATCHED_VERBS: AtomicU64 = AtomicU64::new(0);
/// Batches resent after a transport failure (same ids — see `router`).
pub(crate) static RETRIES: AtomicU64 = AtomicU64::new(0);
/// Replays served from the owner's dedup window.
pub(crate) static DEDUP_HITS: AtomicU64 = AtomicU64::new(0);
/// Frames this node refused AS AN OWNER because they named a superseded
/// era. Counted on the issuing side only, so a node that is both an owner
/// and a client cannot double-count one event.
pub(crate) static STALE_TERM_REFUSALS: AtomicU64 = AtomicU64::new(0);
/// Times this node, AS A CLIENT, learned a new owner era from a refusal —
/// the failover-observation gauge (its own counter for the same reason).
pub(crate) static ERA_RELEARNS: AtomicU64 = AtomicU64::new(0);
/// Frames refused because this node holds no authority over a target.
pub(crate) static NOT_OWNER_REFUSALS: AtomicU64 = AtomicU64::new(0);
/// Verbs refused because their participants span two owners (S3.5).
pub(crate) static CROSS_OWNER_REFUSALS: AtomicU64 = AtomicU64::new(0);
/// Reclaims admitted (spec §6.9's `dlm_grace_reclaims`).
pub(crate) static GRACE_RECLAIMS: AtomicU64 = AtomicU64::new(0);
/// Fresh mutations refused inside a grace window (spec §6.9's
/// `dlm_grace_conflicts` — **must stay 0** on a healthy failover, where
/// clients reclaim and wait rather than mutating through the window).
pub(crate) static GRACE_CONFLICTS: AtomicU64 = AtomicU64::new(0);
/// Owner-side batch executions that UNWOUND (**must stay 0** — RES-7/8's
/// discipline: nothing joins a data-path task, so this is the only record).
pub(crate) static OWNER_PANICS: AtomicU64 = AtomicU64::new(0);
/// Metadata round trips — spec §6.9's `dlm_rpcs{meta}` face. The S4
/// `dlm_rpcs` counter keeps its meaning (LOCK round trips) untouched.
pub(crate) static DLM_RPCS_META: AtomicU64 = AtomicU64::new(0);
/// Mints redirected to an owned volume (the §6.10 R4 constraint engaging).
pub(crate) static MINT_REDIRECTS: AtomicU64 = AtomicU64::new(0);

/// The shipped-vs-local ledger, the pipelining factor, the idempotency
/// window and the failover ledger — one snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShipStatsSnapshot {
    pub armed: bool,
    pub arms: u64,
    pub local_verbs: u64,
    pub shipped_verbs: u64,
    pub served_verbs: u64,
    pub batches: u64,
    pub batched_verbs: u64,
    pub retries: u64,
    pub dedup_hits: u64,
    pub stale_term_refusals: u64,
    pub era_relearns: u64,
    pub not_owner_refusals: u64,
    pub cross_owner_refusals: u64,
    pub grace_reclaims: u64,
    pub grace_conflicts: u64,
    pub owner_panics: u64,
    pub dlm_rpcs_meta: u64,
    pub mint_redirects: u64,
}

/// Read the ledger.
pub fn stats() -> ShipStatsSnapshot {
    ShipStatsSnapshot {
        armed: ownership_armed(),
        arms: OWNERSHIP_ARMS.load(Ordering::Relaxed),
        local_verbs: LOCAL_VERBS.load(Ordering::Relaxed),
        shipped_verbs: SHIPPED_VERBS.load(Ordering::Relaxed),
        served_verbs: SERVED_VERBS.load(Ordering::Relaxed),
        batches: BATCHES.load(Ordering::Relaxed),
        batched_verbs: BATCHED_VERBS.load(Ordering::Relaxed),
        retries: RETRIES.load(Ordering::Relaxed),
        dedup_hits: DEDUP_HITS.load(Ordering::Relaxed),
        stale_term_refusals: STALE_TERM_REFUSALS.load(Ordering::Relaxed),
        era_relearns: ERA_RELEARNS.load(Ordering::Relaxed),
        not_owner_refusals: NOT_OWNER_REFUSALS.load(Ordering::Relaxed),
        cross_owner_refusals: CROSS_OWNER_REFUSALS.load(Ordering::Relaxed),
        grace_reclaims: GRACE_RECLAIMS.load(Ordering::Relaxed),
        grace_conflicts: GRACE_CONFLICTS.load(Ordering::Relaxed),
        owner_panics: OWNER_PANICS.load(Ordering::Relaxed),
        dlm_rpcs_meta: DLM_RPCS_META.load(Ordering::Relaxed),
        mint_redirects: MINT_REDIRECTS.load(Ordering::Relaxed),
    }
}

/// The `meta_ship` stats-inode payload (see AGENTS.md's stats clause).
pub fn stats_json() -> serde_json::Value {
    let s = stats();
    let t = token_cache_stats();
    serde_json::json!({
        "armed": s.armed,
        "arms": s.arms,
        "local_verbs": s.local_verbs,
        "shipped_verbs": s.shipped_verbs,
        "served_verbs": s.served_verbs,
        "batches": s.batches,
        "batched_verbs": s.batched_verbs,
        "retries": s.retries,
        "dedup_hits": s.dedup_hits,
        "stale_term_refusals": s.stale_term_refusals,
        "era_relearns": s.era_relearns,
        "not_owner_refusals": s.not_owner_refusals,
        "cross_owner_refusals": s.cross_owner_refusals,
        "owner_panics": s.owner_panics,
        "mint_redirects": s.mint_redirects,
        "dlm_rpcs_meta": s.dlm_rpcs_meta,
        "dlm_grace_reclaims": s.grace_reclaims,
        "dlm_grace_conflicts": s.grace_conflicts,
        "dlm_token_cache_hits": t.hits,
        "dlm_token_cache_misses": t.misses,
        "dlm_token_cache_evictions": t.evictions,
        "dlm_token_cache_grants": t.grants,
        "dlm_token_cache_entries": t.entries,
        "dlm_token_cache_bytes": t.bytes,
        "dlm_token_cache_cap_entries": t.cap_entries,
        "dlm_token_cache_owner_term": t.owner_term,
    })
}

// ---------------------------------------------------------------------------
// Attribution (§6.10 R1's deferred `tar -x` A/B needs terms, not a number)
// ---------------------------------------------------------------------------

/// Client-side phases of one shipped verb (`meta_ship_phase_ns`).
///
/// Same always-on cost contract as `publish_phase_ns` /
/// `write_pipeline_phase_ns`: one `Instant::now()` and one relaxed
/// `fetch_add` per boundary. `route` fires on EVERY verb (local ones
/// included — it is the routing decision's own price, which requirement 1
/// says must be negligible); the rest fire only on shipped verbs.
///
/// Containment: `queue_wait + encode + rtt + decode` ≈ the shipped verb's
/// wall time, and `rtt` is the term §6.5 item 1's arithmetic is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum ShipPhase {
    /// The routing decision itself (every verb).
    Route = 0,
    /// Submission → drained into a frame (the pipelining wait).
    QueueWait = 1,
    /// Frame encode.
    Encode = 2,
    /// The authenticated round trip on the S3 wire.
    Rtt = 3,
    /// Reply decode + grant absorption.
    Decode = 4,
}

const SHIP_PHASES: usize = 5;
const SHIP_PHASE_NAMES: [&str; SHIP_PHASES] = ["route", "queue_wait", "encode", "rtt", "decode"];

static SHIP_PROF: Lazy<[LatencyHistogram; SHIP_PHASES]> =
    Lazy::new(|| std::array::from_fn(|_| LatencyHistogram::default()));

/// Record a client-side phase span started at `t0`.
#[inline]
pub fn phase_record(phase: ShipPhase, t0: std::time::Instant) {
    SHIP_PROF[phase as usize].record(t0.elapsed());
}

/// `meta_ship_phase_ns` — the client-side decomposition.
pub fn phase_json() -> serde_json::Value {
    let mut phases = serde_json::Map::new();
    for (i, name) in SHIP_PHASE_NAMES.iter().enumerate() {
        phases.insert((*name).to_string(), SHIP_PROF[i].to_json());
    }
    serde_json::Value::Object(phases)
}

/// Owner-side phases of one shipped frame (`meta_ship_owner_phase_ns`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum OwnerPhase {
    /// Decode + schema + era + authority + grace gates.
    Admit = 0,
    /// The handoff to the backend's runtime, including the batch's whole
    /// execution as observed from the lane.
    Dispatch = 1,
    /// One verb's own execution against `RoutedMetaBackend`.
    Execute = 2,
    /// Reply encode.
    ReplyEncode = 3,
    /// The whole frame, lane-side.
    Total = 4,
}

const OWNER_PHASES: usize = 5;
const OWNER_PHASE_NAMES: [&str; OWNER_PHASES] =
    ["admit", "dispatch", "execute", "reply_encode", "total"];

static OWNER_PROF: Lazy<[LatencyHistogram; OWNER_PHASES]> =
    Lazy::new(|| std::array::from_fn(|_| LatencyHistogram::default()));

/// Record an owner-side phase span started at `t0`.
#[inline]
pub fn owner_phase_record(phase: OwnerPhase, t0: std::time::Instant) {
    OWNER_PROF[phase as usize].record(t0.elapsed());
}

/// `meta_ship_owner_phase_ns` — the owner-side decomposition.
pub fn owner_phase_json() -> serde_json::Value {
    let mut phases = serde_json::Map::new();
    for (i, name) in OWNER_PHASE_NAMES.iter().enumerate() {
        phases.insert((*name).to_string(), OWNER_PROF[i].to_json());
    }
    serde_json::Value::Object(phases)
}

// ---------------------------------------------------------------------------
// Refusals — one phrasing each, so an operator meets the same text
// wherever the shape is met (the v2-refusal precedent).
// ---------------------------------------------------------------------------

/// The cross-OWNER refusal: `EXDEV`, naming **S3.5**.
///
/// `EXDEV` is the errno POSIX already gives a caller for "these two names
/// are not on the same filesystem object graph", which is what a rename or
/// link across two independently-committing authorities is until the
/// cross-volume transaction machinery exists.
pub fn cross_owner_error(verb: MetaVerb, ino: u64, detail: &str) -> SqueezefsError {
    let msg = format!(
        "S8: {} spans two metadata OWNERS (ino {ino}: {detail}) — refused. A cross-owner \
         mutation needs the S3.5 cross-volume transaction machinery (intent record + \
         compensation + crash recovery; execution-plan ruling D4, spec DUR-7), which is not \
         built. Shipping this op to one owner would be non-atomic across the other with no \
         compensation record and no single D0 guard over both halves — strictly worse than \
         refusing.",
        verb.name()
    );
    log::error!("{msg}");
    SqueezefsError::refused(libc::EXDEV, msg)
}

/// [`cross_owner_error`] plus the ledger increment (the client-side
/// routing refusal).
pub(crate) fn cross_owner_refusal(verb: MetaVerb, ino: u64, detail: &str) -> SqueezefsError {
    CROSS_OWNER_REFUSALS.fetch_add(1, Ordering::Relaxed);
    cross_owner_error(verb, ino, detail)
}

/// A reply that does not match its verb's shape: a protocol violation, not
/// a filesystem error, so it is loud and never coerced into an errno an
/// application might interpret as a filesystem state.
pub(crate) fn protocol_error(verb: MetaVerb, got: &str, want: &str) -> SqueezefsError {
    let msg = format!(
        "S8 protocol violation: the owner answered {} with {got}, but the verb's reply shape is \
         {want} — refusing rather than inventing a result",
        verb.name()
    );
    log::error!("{msg}");
    SqueezefsError::InvalidOperation(msg)
}
