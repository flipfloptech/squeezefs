//! The **client token cache** — S8's resolution of the S4 fencing-read
//! contract (`dlm_slot`'s module docs, contract "for S6/S8": *"when a
//! foreign home becomes possible, a fencing read on a foreign-home object
//! must become an owner read (or a leased/cached-token read) — it must NOT
//! keep serving the local view, which would then be a stale-generation
//! answer"*).
//!
//! # Why a cache, and not a round trip
//!
//! The census is ~24 fencing-read sites, several per write / publish /
//! flush. Spec §6.5 item 1 is explicit about what that costs: *"Adding one
//! 250 µs fabric RTT takes the create wall from 110 µs to 360 µs — a 69 %
//! regression … **Requirement: ≥ 99.5 % of lock operations must be served
//! from a locally cached or delegated token.**"* A per-read round trip is
//! therefore not a candidate; every reference client (Lustre's client lock
//! cache, Ceph's issued/wanted caps, NFSv4 delegations) caches on the
//! client and reclaims on revocation.
//!
//! # How it stays sound
//!
//! Entries are **only** written from an owner's answer — the grant that
//! piggybacks on a shipped verb's reply (spec §6.7 decision 3). The cache
//! is therefore never an independent generator, only a repeater, and it is
//! monotone: a `fetch_max`-style update can never move an object's
//! generation backwards, which is the property every consumer comparison
//! (`<`, `==`, `.max()`) needs.
//!
//! # The honest hole, made loud
//!
//! `get_fencing_token_ino` cannot fail — it returns `u64`. So a MISS on a
//! foreign object has no error channel, and both available answers are
//! wrong in a different direction: too low adopts a superseded record
//! (§6.11's inversion), too high fences live work. The resolution:
//!
//! * a miss returns the **owner era's base** (`term << 40`), which is
//!   exactly what a fresh local mount already returns for an object with
//!   no grant — the same honest degradation the shipped code has, not a
//!   new one — so pre-era stamps still classify stale and current-era
//!   stamps still classify live;
//! * a miss is a **must-stay-0 tripwire**: counted (`dlm_token_cache_misses`)
//!   and reported through `note_invariant_tripwire` (RES-22's
//!   loud-never-fatal law), because a miss means the intent-lock property
//!   was violated — some site needed a token without having performed a
//!   metadata RPC on the object first.
//!
//! The invariant that makes misses impossible in the operations that
//! matter is spec §6.7 decision 3's: *"There is no operation that needs a
//! token but performs no metadata RPC on the object first."* Where a
//! client's entry has aged out or the era moved,
//! [`super::MetaShipRouter::refresh_token`] re-earns it with a shipped
//! `getattr` — a metadata RPC, never a second lock protocol.
//!
//! # Bounded, because the population is unbounded
//!
//! `FENCING_MAP`'s unbounded growth was RES-2, closed by S1; this cache
//! must not reintroduce it. The cap derives from the R5 budget (the
//! standing "caps derive from system resources" law), and retirement is a
//! **second-chance clock**: one bounded sweep clears the reference bit of
//! recently-used entries and retires the rest. Retiring an entry costs at
//! most one loud miss and one refresh, never a wrong answer.

use super::wire::TokenGrant;
use crate::dlm::{compose_token, GRANT_SEQ_BITS};
use crate::fuse_client::LatencyHistogram;
use crate::token_cache_core::{record_grant_ordered, EraFloor, TokenSlot};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Absolute override for the derived entry cap.
pub const TOKEN_CACHE_MAX_ENV: &str = "SQUEEZEFS_DLM_TOKEN_CACHE_MAX";

/// Bytes charged per entry: the `scc` bucket slot for a `u64` key plus the
/// two words of value. Used for the `dlm_token_cache_bytes` gauge, which
/// is what an R5 registration would consume when S9 makes the cache
/// load-bearing (it is a control-plane cache today: bounded, sheddable,
/// and every eviction costs one refresh).
const ENTRY_BYTES: u64 = 32;

/// Floor for the derived cap: a client with a small budget still caches a
/// real working set of open objects rather than thrashing into misses.
const ENTRY_FLOOR: usize = 4096;

/// Share of the R5 budget the cache may occupy: 1/8192. At a 16 GiB
/// budget that is 2 MiB ≈ 65 k objects — the population of objects one
/// client has live metadata interest in, by construction bounded by its
/// open files and its dirty set, never by objects ever touched.
const BUDGET_DIVISOR: u64 = 8192;

// The per-entry word protocol (monotone token/term merge, the
// second-chance reference bit) lives in `crate::token_cache_core` —
// spec §6.9's `token_cache_core` loom obligation, model-checked there.
static CACHE: Lazy<scc::HashMap<u64, TokenSlot>> = Lazy::new(scc::HashMap::new);

/// The owner era the cache last learned — a miss's floor, and the reason a
/// miss degrades exactly as a fresh mount does rather than answering 0 on
/// a volume whose records name eras.
static OWNER_TERM: EraFloor = EraFloor::new();

static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static EVICTIONS: AtomicU64 = AtomicU64::new(0);
static GRANTS: AtomicU64 = AtomicU64::new(0);

/// The cache's entry cap: derived from the R5 budget, absolute override
/// wins verbatim (the standing precedence law).
pub fn cache_cap() -> usize {
    if let Some(explicit) = crate::env_knobs::opt_int_knob::<usize>(TOKEN_CACHE_MAX_ENV) {
        return explicit.max(1);
    }
    let budget = crate::mem_budget::MEM_BUDGET.budget_bytes();
    let derived = (budget / BUDGET_DIVISOR / ENTRY_BYTES) as usize;
    derived.max(ENTRY_FLOOR)
}

/// Record the owner's era (learned from every reply frame). Monotone.
pub fn record_owner_term(term: u64) {
    OWNER_TERM.record(term);
}

/// The era the cache serves a miss in.
pub fn owner_term() -> u64 {
    OWNER_TERM.get()
}

/// Record a grant an owner sent us. Monotone per object: a reordered or
/// replayed reply can never lower an object's generation.
///
/// The era floor is recorded BEFORE the grant becomes findable
/// ([`record_grant_ordered`] — the core's order, weakening-verified in
/// its loom model): an entry the sweep later retires must never expose a
/// miss whose floor predates the grant's own era.
pub fn record_grant(grant: &TokenGrant) {
    record_grant_ordered(&OWNER_TERM, grant.term, || {
        GRANTS.fetch_add(1, Ordering::Relaxed);
        let updated = CACHE.read_sync(&grant.ino, |_, e| e.merge(grant.token, grant.term));
        if updated.is_some() {
            return;
        }
        if CACHE.len() >= cache_cap() {
            sweep();
        }
        let _ = CACHE.insert_sync(grant.ino, TokenSlot::granted(grant.token, grant.term));
    });
}

/// One bounded second-chance pass: entries used since the last sweep are
/// kept (and their bit cleared), the rest retire.
fn sweep() {
    let mut retired = 0u64;
    CACHE.retain_sync(|_, e| {
        if e.keep_for_another_pass() {
            true
        } else {
            retired += 1;
            false
        }
    });
    if retired > 0 {
        EVICTIONS.fetch_add(retired, Ordering::Relaxed);
    } else {
        // Every entry was hot: clear the whole generation rather than
        // grow without bound. Each retirement costs one loud miss and one
        // refresh — never a wrong answer.
        let mut cleared = 0u64;
        CACHE.retain_sync(|_, _| {
            cleared += 1;
            false
        });
        EVICTIONS.fetch_add(cleared, Ordering::Relaxed);
    }
}

/// The fencing generation of a **foreign-home** object, served from the
/// owner's own last answer.
///
/// A miss returns the owner era's base and trips the must-stay-0
/// tripwire — see the module docs for why that is the only sound
/// direction and why it is loud.
pub fn foreign_fencing_token(ino: u64) -> u64 {
    if let Some(token) = CACHE.read_sync(&ino, |_, e| e.serve()) {
        HITS.fetch_add(1, Ordering::Relaxed);
        return token;
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    let floor = compose_token(owner_term(), 0);
    crate::note_invariant_tripwire(
        "meta_ship::foreign_fencing_token",
        &format!(
            "no cached grant for foreign-home ino {ino}: a fencing read reached this site \
             without a metadata RPC on the object first (spec §6.7 decision 3's intent-lock \
             property) — serving the owner era's base {floor} (term {}, grant 0), which \
             classifies pre-era stamps stale and current-era stamps live, and never invents \
             a local grant",
            owner_term()
        ),
    );
    floor
}

/// Snapshot of the cache's counters (`dlm_token_cache_*` — spec §6.9's
/// named family).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenCacheStats {
    pub hits: u64,
    /// **Must stay 0**: a miss means the intent-lock property was violated.
    pub misses: u64,
    pub evictions: u64,
    pub grants: u64,
    pub entries: u64,
    pub bytes: u64,
    pub cap_entries: u64,
    pub owner_term: u64,
}

/// Read the cache's counters.
pub fn token_cache_stats() -> TokenCacheStats {
    let entries = CACHE.len() as u64;
    TokenCacheStats {
        hits: HITS.load(Ordering::Relaxed),
        misses: MISSES.load(Ordering::Relaxed),
        evictions: EVICTIONS.load(Ordering::Relaxed),
        grants: GRANTS.load(Ordering::Relaxed),
        entries,
        bytes: entries * ENTRY_BYTES,
        cap_entries: cache_cap() as u64,
        owner_term: owner_term(),
    }
}

/// **Test seam**: drop every cached grant, so a suite can exercise the
/// COLD (miss) arm deliberately. Production never calls it — an
/// invalidation on a real revocation is S9's revoke verb, which will
/// retire the named objects, not the whole cache.
pub fn test_clear_token_cache() {
    CACHE.retain_sync(|_, _| false);
}

/// The composed-token width, re-exported for the callers that need to
/// reason about a miss's floor without importing the DLM.
pub const TOKEN_GRANT_BITS: u32 = GRANT_SEQ_BITS;

// ===========================================================================
// DLM S10 rung 11 — the RECALL LANE + THRASH VALVE ("brake before engine").
//
// `docs/design-full-multi-writer.md` §8 (the recall law) + PR row 11; spec
// risk **R5** ("revoke storms on hot shared objects … verify the demotion
// valve engages before the fan-out hurts") and **R3** (deadlines derive
// from LIVE p99 evidence, never a constant); Ceph `mds_recall` lineage
// (spec §6.6). This machinery deliberately lands BEFORE any delegation
// grant exists, so delegation (rows 12–14) can never ship without its
// brake: today the grant population arrives only through the test seams,
// production mounts never touch the lane, and every stat is 0 BY
// CONSTRUCTION on every shipped mount.
//
// # The API contract rows 12–14 consume
//
// * Grant issuance (row 12's DelegGrant) calls [`RecallLane::try_grant`]
//   FIRST — the valve's gate. `Demoted` means the object is owner-served
//   for the remaining cooldown: do not issue, do not invent a second
//   arbitration. Roster admission (only an ADMITTED member may hold a
//   delegation — design §Security) belongs to the CALLER: the lane
//   bookkeeps whatever population the grant path admitted.
// * A conflicting mutation calls [`RecallLane::recall_object`]; the wire
//   half (row 12's DelegRecall verb) drains [`RecallLane::issue_pass`]
//   into frames (one frame per client — the batching law), correlates
//   acks by `(client, frame_id)` into [`RecallLane::ack_frame`], and runs
//   [`RecallLane::expire_overdue`] on its cadence sweep.
// * A recall TIMEOUT is terminal: the grant is DEAD and the object
//   grantable again. Sound because the deadline's CEILING is the
//   membership lease TTL (S6) and a member's own `T_self` fires strictly
//   before the owner's TTL (`T_self = T_owner − 2·skew_max − D_purge`),
//   so a live-but-partitioned holder has self-fenced — and its in-flight
//   DMA is bounded by S7's dead-epoch quarantine — before the owner acts
//   on the timeout. Rows 12+ escalate a timed-out client to membership
//   eviction (the `transport_lease_overlong` precedent: loud, never a
//   silent wait — the lane logs every expiry).
// ===========================================================================

/// Absolute override for the derived recall batch cap (measurement lever).
pub const RECALL_BATCH_MAX_ENV: &str = "SQUEEZEFS_DLM_RECALL_BATCH_MAX";
/// Absolute override for the derived recall deadline (measurement lever).
pub const RECALL_DEADLINE_ENV: &str = "SQUEEZEFS_DLM_RECALL_DEADLINE_MS";
/// Absolute override for the derived demotion cooldown (measurement lever).
pub const RECALL_COOLDOWN_ENV: &str = "SQUEEZEFS_DLM_RECALL_COOLDOWN_MS";

/// Wire bytes one recall entry budgets: an ino (u64, bincode varint ≤ 9 B)
/// plus per-entry framing — measured ≤ 20 B encoded; 32 is the
/// power-of-two ceiling so the batch arithmetic stays exact when row 12's
/// verb grows a generation stamp.
const RECALL_ENTRY_WIRE_BYTES: u32 = 32;

/// Half the CONTROL frame is entry budget; the other half is header, MAC,
/// ids and growth headroom (the same conservative split the S8 encode
/// check enforces at the frame level).
const RECALL_FRAME_HEADROOM_DIV: u32 = 2;

/// Consecutive grant→recall cycles inside the thrash window before the
/// valve demotes: **3** — the smallest run that separates a PATTERN from
/// a coincidence (one cycle is any legitimate writer conflict, two can be
/// one conflict's retry; the fsck verify-before-report settle posture:
/// single evidence is never a verdict).
pub const RECALL_THRASH_CYCLES: u32 = 3;

/// Demotion cooldown in thrash windows: **8** — bounds the worst-case
/// residual thrash duty cycle at `cycles/(cycles+8)` ≈ 27 % of the
/// un-valved volume for a permanently hot object, while a genuinely
/// cooled object re-promotes within one decade of the detection horizon
/// (the AIMD-retreat class: a failed re-promotion probe costs a full
/// threshold run of recalls, so probes are spaced an order of magnitude
/// apart).
const RECALL_COOLDOWN_WINDOWS: u32 = 8;

/// Deadline headroom over live p99: **4×** = two binary octaves — the
/// histogram's power-of-two buckets make a p99 read up to 2× coarse by
/// construction, and one more octave covers the drain the p99 predates
/// (spec R3's unbounded-writeback caveat). A deadline AT p99 would time
/// out ~1 % of healthy recalls, and every false timeout kills a live
/// grant.
const RECALL_DEADLINE_MARGIN: u64 = 4;

/// Deadline floor: the 1 ms timer grain (kernel/tokio scheduling quantum)
/// — below it a deadline is unmeasurable, not strict.
const RECALL_DEADLINE_FLOOR: Duration = Duration::from_millis(1);

/// The recall batch cap: entries per frame, derived from the wire's own
/// CONTROL-class bound (explicit lever wins verbatim — the precedence
/// law). At the shipped 1 MiB cap: 16,384 recalls/frame, so spec R6's
/// "one frame per client carrying its whole token set" fits the 1 k-token
/// shape 16× over.
pub fn recall_batch_max_from(explicit: Option<usize>, frame_cap_bytes: u32) -> usize {
    if let Some(e) = explicit {
        return e.max(1);
    }
    ((frame_cap_bytes / RECALL_FRAME_HEADROOM_DIV / RECALL_ENTRY_WIRE_BYTES) as usize).max(1)
}

/// The recall deadline (spec R3's law — never a constant):
///
/// * explicit lever wins verbatim;
/// * live evidence ⇒ `4 × (rtt_p99 + owner_total_p99)`, floored at the
///   1 ms timer grain, ceilinged at the membership lease TTL (past the
///   TTL the S6/S7 fence arithmetic bounds the client anyway — waiting
///   longer buys nothing);
/// * zero samples ⇒ the TTL itself, the only derivable bound with no
///   evidence (conservative toward the fence bound; on an armed mount
///   the grant's own metadata RPC has already fed the histograms).
pub fn recall_deadline_from(
    explicit_ms: Option<u64>,
    live_p99_us: Option<u64>,
    lease_ttl: Duration,
) -> Duration {
    if let Some(ms) = explicit_ms {
        return Duration::from_millis(ms.max(1));
    }
    let ceiling = lease_ttl.max(RECALL_DEADLINE_FLOOR);
    match live_p99_us {
        None => ceiling,
        Some(us) => Duration::from_micros(us.saturating_mul(RECALL_DEADLINE_MARGIN))
            .clamp(RECALL_DEADLINE_FLOOR, ceiling),
    }
}

/// The demotion cooldown: `8 × thrash_window` (see
/// [`RECALL_COOLDOWN_WINDOWS`]); explicit lever wins verbatim.
pub fn recall_cooldown_from(explicit_ms: Option<u64>, thrash_window: Duration) -> Duration {
    if let Some(ms) = explicit_ms {
        return Duration::from_millis(ms.max(1));
    }
    thrash_window.saturating_mul(RECALL_COOLDOWN_WINDOWS)
}

/// The published aggregate recall rate cap, per second: `roster ×
/// batch_max / deadline` — wire budget × lease arithmetic × roster size,
/// no free constant. The lane ENFORCES it structurally (one in-flight
/// frame of ≤ `batch_max` entries per client per deadline round); this
/// arithmetic is the gauge that makes the bound visible (the
/// `free_grace_bound` publish-the-derivation pattern).
pub fn recall_rate_cap_per_s(batch_max: usize, deadline: Duration, roster: usize) -> u64 {
    let deadline_ms = deadline.as_millis().max(1) as u64;
    (batch_max as u64)
        .saturating_mul(roster.max(1) as u64)
        .saturating_mul(1000)
        / deadline_ms
}

/// The lease TTL the deadline ceilings at: the armed membership plane's
/// own `T_owner` when installed (the lane runs on the OWNER — S6's
/// authority), else the same knob/default `LeaseClocks::derive` reads, so
/// the two planes can never disagree about what "the lease" means.
fn recall_lease_ttl() -> Duration {
    if let Some(o) = crate::membership::installed_owner() {
        return o.clocks().t_owner;
    }
    Duration::from_millis(crate::env_knobs::int_knob(
        "SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS",
        crate::fuse_client::CLIENT_STALE_TTL_SECS * 1000,
    ))
}

/// Roster size for the published rate cap: the armed membership plane's
/// member census when installed (who can hold future delegations), floored
/// at the lane's own live holder population and at 1.
fn recall_roster_size(live_clients: usize) -> usize {
    let members = crate::membership::installed_owner()
        .map(|o| o.len())
        .unwrap_or(0);
    members.max(live_clients).max(1)
}

/// The recall lane's derived configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallConfig {
    /// Ack deadline per issued frame ([`recall_deadline_from`]).
    pub deadline: Duration,
    /// Recall entries per frame ([`recall_batch_max_from`]).
    pub batch_max: usize,
    /// The cycle-detection window: = `deadline` — the lane's own
    /// completion horizon (an object re-granted before its recall round
    /// could even complete is cycling faster than the mechanism serves).
    pub thrash_window: Duration,
    /// Cycles before demotion ([`RECALL_THRASH_CYCLES`]).
    pub thrash_cycles: u32,
    /// Demotion hold ([`recall_cooldown_from`]).
    pub cooldown: Duration,
    /// **Test seam only** (the spec-R5 red half is built against
    /// `valve: false`). Deliberately NOT an env knob: the brake must not
    /// be operationally removable — that is the whole point of landing
    /// this rung before the engine.
    pub valve: bool,
}

impl RecallConfig {
    /// Derive from the live inputs (explicit levers win verbatim).
    pub fn derived() -> Self {
        let batch_max = recall_batch_max_from(
            crate::env_knobs::opt_int_knob::<usize>(RECALL_BATCH_MAX_ENV),
            crate::cluster_wire::CONTROL_MAX_FRAME_BYTES,
        );
        let deadline = recall_deadline_from(
            crate::env_knobs::opt_int_knob::<u64>(RECALL_DEADLINE_ENV),
            super::live_recall_evidence_us(),
            recall_lease_ttl(),
        );
        let thrash_window = deadline;
        let cooldown = recall_cooldown_from(
            crate::env_knobs::opt_int_knob::<u64>(RECALL_COOLDOWN_ENV),
            thrash_window,
        );
        Self {
            deadline,
            batch_max,
            thrash_window,
            thrash_cycles: RECALL_THRASH_CYCLES,
            cooldown,
            valve: true,
        }
    }
}

/// The valve's answer at grant time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantDecision {
    /// The grant was admitted and is now tracked (recall-able).
    Granted,
    /// The object is demoted to owner-served: no grant for `remaining`.
    Demoted {
        /// Cooldown left at the decision instant.
        remaining: Duration,
    },
}

/// One batched recall frame for one client — row 12's DelegRecall payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallFrame {
    /// The holder (KD-MW-2 client id).
    pub client: String,
    /// Ack correlation id, monotone per lane.
    pub frame_id: u64,
    /// The recalled objects (≤ `batch_max`).
    pub inos: Vec<u64>,
}

/// One recall declared DEAD at its deadline (the caller's eviction-
/// escalation input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedOutRecall {
    pub client: String,
    pub ino: u64,
    pub frame_id: u64,
}

/// The lane's counter snapshot (`dlm_recall` on the stats inode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecallLaneStats {
    /// Recalls issued into frames (spec spelling `dlm_revokes_issued` —
    /// the recall lane's face; `dlm_custody.dlm_revokes_issued` remains
    /// the S9 custody plane's, scoped by its own object).
    pub issued: u64,
    /// Recalls acked (`dlm_revokes_acked`).
    pub acked: u64,
    /// Recalls declared dead at the deadline (`dlm_revokes_timed_out`).
    pub timed_out: u64,
    /// Frames issued — the batching-law denominator (frames << issued).
    pub frames: u64,
    /// Recalls elided because one was already pending/in flight.
    pub coalesced: u64,
    /// Recalls held back by the rate discipline (engagement gauge;
    /// counted per pass, so one entry may count across passes — the
    /// `parked_gate_waits` semantics).
    pub rate_deferred: u64,
    /// Acks that matched no in-flight frame (protocol hygiene).
    pub stale_acks: u64,
    /// Grants admitted.
    pub grants: u64,
    /// Grants refused while demoted (the valve's refusal gauge).
    pub grant_refusals: u64,
    /// Valve engagements (spec R5's `dlm_thrash_demotions`).
    pub thrash_demotions: u64,
    /// Demoted objects re-admitted after their cooldown.
    pub repromotions: u64,
    /// GAUGE: outstanding (grant, holder) pairs.
    pub outstanding: u64,
    /// GAUGE: recalls queued, not yet issued.
    pub pending: u64,
    /// GAUGE: objects currently demoted.
    pub demoted_objects: u64,
}

#[derive(Debug, Default)]
struct ThrashSlot {
    /// The last recall episode's instant; cleared when a cycle is counted
    /// (one cycle per grant-after-recall EPISODE, never per holder).
    last_recall_at: Option<Instant>,
    cycles: u32,
    demoted_until: Option<Instant>,
}

#[derive(Debug)]
struct PendingRecall {
    ino: u64,
    enqueued_at: Instant,
}

#[derive(Debug)]
struct InflightFrame {
    frame_id: u64,
    issued_at: Instant,
    deadline_at: Instant,
    recalls: Vec<PendingRecall>,
}

#[derive(Default)]
struct LaneState {
    /// Outstanding grants: object → holders.
    grants: HashMap<u64, HashSet<String>>,
    /// Live recall requests (pending OR in flight): client → objects —
    /// the at-most-one-outstanding-recall-per-(object, client) dedupe.
    requested: HashMap<String, HashSet<u64>>,
    thrash: HashMap<u64, ThrashSlot>,
    /// Queued, not yet framed, per client.
    pending: HashMap<String, VecDeque<PendingRecall>>,
    /// The rate discipline: at most ONE in-flight frame per client.
    inflight: HashMap<String, InflightFrame>,
    next_frame_id: u64,
}

enum ConfigMode {
    /// Pinned (tests; measurement).
    Fixed(RecallConfig),
    /// Re-derived per use, so the deadline tracks the LIVE p99 evidence
    /// (spec R3) instead of freezing at first touch.
    Live,
}

/// Phase indices for `dlm_revoke_phase_ns`.
const PH_ISSUE: usize = 0;
const PH_ACK_WAIT: usize = 1;
const PH_TOTAL: usize = 2;
const RECALL_PHASES: usize = 3;
const RECALL_PHASE_NAMES: [&str; RECALL_PHASES] = ["issue", "ack_wait", "total"];

/// The owner-side recall lane: rate-limited batched recall + the thrash
/// valve. Control-plane machinery — a mutexed table is sanctioned here
/// (lease-transaction class, never the data hot path), and every counter
/// is a lock-free atomic so the stats read stays cheap.
pub struct RecallLane {
    mode: ConfigMode,
    state: Mutex<LaneState>,
    phases: [LatencyHistogram; RECALL_PHASES],
    issued: AtomicU64,
    acked: AtomicU64,
    timed_out: AtomicU64,
    frames: AtomicU64,
    coalesced: AtomicU64,
    rate_deferred: AtomicU64,
    stale_acks: AtomicU64,
    grants: AtomicU64,
    grant_refusals: AtomicU64,
    thrash_demotions: AtomicU64,
    repromotions: AtomicU64,
}

impl RecallLane {
    fn new(mode: ConfigMode) -> Self {
        Self {
            mode,
            state: Mutex::new(LaneState::default()),
            phases: std::array::from_fn(|_| LatencyHistogram::default()),
            issued: AtomicU64::new(0),
            acked: AtomicU64::new(0),
            timed_out: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            coalesced: AtomicU64::new(0),
            rate_deferred: AtomicU64::new(0),
            stale_acks: AtomicU64::new(0),
            grants: AtomicU64::new(0),
            grant_refusals: AtomicU64::new(0),
            thrash_demotions: AtomicU64::new(0),
            repromotions: AtomicU64::new(0),
        }
    }

    /// A lane with a pinned config (tests; measurement rows).
    pub fn with_config(cfg: RecallConfig) -> Self {
        Self::new(ConfigMode::Fixed(cfg))
    }

    /// A lane that re-derives its config per use (the global's mode).
    pub fn live() -> Self {
        Self::new(ConfigMode::Live)
    }

    /// The lane's config as of NOW (fixed lanes answer their pin).
    pub fn config(&self) -> RecallConfig {
        match &self.mode {
            ConfigMode::Fixed(c) => *c,
            ConfigMode::Live => RecallConfig::derived(),
        }
    }

    /// The valve's gate + grant bookkeeping. Rows 12+ call this FIRST at
    /// grant issuance; the population today comes from the test seams.
    ///
    /// One cycle is counted per grant-after-recall EPISODE (the first
    /// grant following a recall inside the thrash window), never per
    /// holder — a 32-holder re-grant wave is ONE cycle, so the threshold
    /// counts grant→recall LOOPS, which is what thrash is.
    pub fn try_grant(&self, ino: u64, client: &str, now: Instant) -> GrantDecision {
        let cfg = self.config();
        let mut st = self.state.lock();
        {
            let slot = st.thrash.entry(ino).or_default();
            if cfg.valve {
                if let Some(until) = slot.demoted_until {
                    if now < until {
                        self.grant_refusals.fetch_add(1, Ordering::Relaxed);
                        return GrantDecision::Demoted {
                            remaining: until.saturating_duration_since(now),
                        };
                    }
                    // Cooldown served: the first attempt re-promotes, with
                    // the thrash evidence reset (a fresh trial, not a
                    // carried verdict).
                    slot.demoted_until = None;
                    slot.cycles = 0;
                    slot.last_recall_at = None;
                    self.repromotions.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(last) = slot.last_recall_at {
                    if now.saturating_duration_since(last) <= cfg.thrash_window {
                        slot.cycles = slot.cycles.saturating_add(1);
                        slot.last_recall_at = None;
                        if slot.cycles >= cfg.thrash_cycles {
                            slot.demoted_until = Some(now + cfg.cooldown);
                            slot.cycles = 0;
                            self.thrash_demotions.fetch_add(1, Ordering::Relaxed);
                            self.grant_refusals.fetch_add(1, Ordering::Relaxed);
                            log::warn!(
                                "S10 recall valve: object ino {ino} demoted to owner-served for \
                                 {:?} — {} grant→recall cycles inside {:?} (spec R5's thrash \
                                 shape; grants resume after the cooldown)",
                                cfg.cooldown,
                                cfg.thrash_cycles,
                                cfg.thrash_window
                            );
                            return GrantDecision::Demoted {
                                remaining: cfg.cooldown,
                            };
                        }
                    } else {
                        // The loop broke on its own: stale evidence resets.
                        slot.cycles = 0;
                        slot.last_recall_at = None;
                    }
                }
            }
        }
        st.grants.entry(ino).or_default().insert(client.to_string());
        self.grants.fetch_add(1, Ordering::Relaxed);
        GrantDecision::Granted
    }

    /// Owner-initiated recall of EVERY outstanding grant on `ino`
    /// (batched per client by [`Self::issue_pass`]). Returns the number
    /// of recalls enqueued; already-pending/in-flight `(object, client)`
    /// recalls coalesce. Stamps the object's thrash slot — the recall
    /// half of the cycle evidence.
    pub fn recall_object(&self, ino: u64, now: Instant) -> usize {
        let mut st = self.state.lock();
        let holders: Vec<String> = st
            .grants
            .get(&ino)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        if holders.is_empty() {
            return 0;
        }
        let mut enqueued = 0usize;
        for client in holders {
            if !st.requested.entry(client.clone()).or_default().insert(ino) {
                self.coalesced.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            st.pending
                .entry(client)
                .or_default()
                .push_back(PendingRecall {
                    ino,
                    enqueued_at: now,
                });
            enqueued += 1;
        }
        st.thrash.entry(ino).or_default().last_recall_at = Some(now);
        enqueued
    }

    /// The batched, rate-limited issue pass: for every client with queued
    /// recalls and NO in-flight frame, drain up to `batch_max` entries
    /// into one frame. The caller (row 12's wire half; the tests today)
    /// puts the frames on the wire. The rate limiter IS the structure:
    /// one in-flight frame of ≤ `batch_max` entries per client per
    /// ack/deadline round — `batch_max/deadline` per client, derived at
    /// both ends, no constant.
    pub fn issue_pass(&self, now: Instant) -> Vec<RecallFrame> {
        let cfg = self.config();
        let mut st = self.state.lock();
        let mut frames = Vec::new();
        let clients: Vec<String> = st
            .pending
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(c, _)| c.clone())
            .collect();
        for client in clients {
            if st.inflight.contains_key(&client) {
                let waiting = st.pending.get(&client).map(|q| q.len()).unwrap_or(0);
                self.rate_deferred
                    .fetch_add(waiting as u64, Ordering::Relaxed);
                continue;
            }
            let Some(q) = st.pending.get_mut(&client) else {
                continue;
            };
            let take = q.len().min(cfg.batch_max);
            let mut recalls = Vec::with_capacity(take);
            let mut inos = Vec::with_capacity(take);
            for _ in 0..take {
                let r = q.pop_front().expect("take <= q.len()");
                self.phases[PH_ISSUE].record(now.saturating_duration_since(r.enqueued_at));
                inos.push(r.ino);
                recalls.push(r);
            }
            let leftover = q.len();
            if leftover == 0 {
                st.pending.remove(&client);
            } else {
                // Held back by the batch cap — the limiter's other face.
                self.rate_deferred
                    .fetch_add(leftover as u64, Ordering::Relaxed);
            }
            st.next_frame_id += 1;
            let frame_id = st.next_frame_id;
            st.inflight.insert(
                client.clone(),
                InflightFrame {
                    frame_id,
                    issued_at: now,
                    deadline_at: now + cfg.deadline,
                    recalls,
                },
            );
            self.issued.fetch_add(take as u64, Ordering::Relaxed);
            self.frames.fetch_add(1, Ordering::Relaxed);
            frames.push(RecallFrame {
                client,
                frame_id,
                inos,
            });
        }
        frames
    }

    /// The client acked its frame: every recall in it is terminal, the
    /// surrendered grants leave the table, and the client's rate slot
    /// frees. An ack that matches no in-flight frame (a resend, a
    /// post-timeout straggler) is counted and changes nothing — the
    /// S8-dedup posture applied to acks.
    pub fn ack_frame(&self, client: &str, frame_id: u64, now: Instant) {
        let mut st = self.state.lock();
        let matches = st
            .inflight
            .get(client)
            .map(|f| f.frame_id == frame_id)
            .unwrap_or(false);
        if !matches {
            self.stale_acks.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let f = st.inflight.remove(client).expect("checked above");
        for r in &f.recalls {
            self.phases[PH_ACK_WAIT].record(now.saturating_duration_since(f.issued_at));
            self.phases[PH_TOTAL].record(now.saturating_duration_since(r.enqueued_at));
            retire_recall(&mut st, client, r.ino);
        }
        self.acked
            .fetch_add(f.recalls.len() as u64, Ordering::Relaxed);
    }

    /// The deadline sweep: every frame past its deadline is DEAD — the
    /// grants it recalled leave the table and the objects are grantable
    /// again. Loud, never silent (the `transport_lease_overlong`
    /// precedent); the returned list is the caller's eviction-escalation
    /// input (rows 12+ feed membership `evict`, minting the S7 dead
    /// epoch).
    ///
    /// Soundness: the deadline's ceiling is the membership lease TTL, and
    /// a member's `T_self` (= `T_owner − 2·skew_max − D_purge`) fires
    /// STRICTLY before the owner's TTL — so a holder that could not ack
    /// inside the deadline has either died or self-fenced before the
    /// owner may act as if the grant were gone; its in-flight DMA is
    /// S7's dead-epoch quarantine's problem, never this table's.
    pub fn expire_overdue(&self, now: Instant) -> Vec<TimedOutRecall> {
        let mut st = self.state.lock();
        let overdue: Vec<String> = st
            .inflight
            .iter()
            .filter(|(_, f)| now >= f.deadline_at)
            .map(|(c, _)| c.clone())
            .collect();
        let mut out = Vec::new();
        for client in overdue {
            let f = st.inflight.remove(&client).expect("collected above");
            log::warn!(
                "S10 recall lane: client '{client}' missed its recall deadline — frame {} \
                 carrying {} recall(s) is DEAD (grants retired; the S6 lease/T_self arithmetic \
                 bounds the holder, and rows 12+ escalate to membership eviction)",
                f.frame_id,
                f.recalls.len()
            );
            for r in f.recalls {
                self.phases[PH_TOTAL].record(now.saturating_duration_since(r.enqueued_at));
                retire_recall(&mut st, &client, r.ino);
                self.timed_out.fetch_add(1, Ordering::Relaxed);
                out.push(TimedOutRecall {
                    client: client.clone(),
                    ino: r.ino,
                    frame_id: f.frame_id,
                });
            }
        }
        out
    }

    /// Outstanding holders of `ino`.
    pub fn holders(&self, ino: u64) -> usize {
        self.state
            .lock()
            .grants
            .get(&ino)
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// Counter snapshot + gauges.
    pub fn stats(&self) -> RecallLaneStats {
        let now = Instant::now();
        let (outstanding, pending, demoted) = {
            let st = self.state.lock();
            (
                st.grants.values().map(|s| s.len() as u64).sum(),
                st.pending.values().map(|q| q.len() as u64).sum(),
                st.thrash
                    .values()
                    .filter(|t| t.demoted_until.is_some_and(|u| now < u))
                    .count() as u64,
            )
        };
        RecallLaneStats {
            issued: self.issued.load(Ordering::Relaxed),
            acked: self.acked.load(Ordering::Relaxed),
            timed_out: self.timed_out.load(Ordering::Relaxed),
            frames: self.frames.load(Ordering::Relaxed),
            coalesced: self.coalesced.load(Ordering::Relaxed),
            rate_deferred: self.rate_deferred.load(Ordering::Relaxed),
            stale_acks: self.stale_acks.load(Ordering::Relaxed),
            grants: self.grants.load(Ordering::Relaxed),
            grant_refusals: self.grant_refusals.load(Ordering::Relaxed),
            thrash_demotions: self.thrash_demotions.load(Ordering::Relaxed),
            repromotions: self.repromotions.load(Ordering::Relaxed),
            outstanding,
            pending,
            demoted_objects: demoted,
        }
    }

    /// `dlm_revoke_phase_ns` — issue / ack_wait / total, on the shared
    /// 26-bucket latency core (bucket-compatible with every other phase
    /// table by construction).
    pub fn phase_json(&self) -> serde_json::Value {
        let mut phases = serde_json::Map::new();
        for (i, name) in RECALL_PHASE_NAMES.iter().enumerate() {
            phases.insert((*name).to_string(), self.phases[i].to_json());
        }
        serde_json::Value::Object(phases)
    }
}

/// A recall reached its terminal outcome (ack or timeout): the grant and
/// the dedupe entry retire together.
fn retire_recall(st: &mut LaneState, client: &str, ino: u64) {
    if let Some(hs) = st.grants.get_mut(&ino) {
        hs.remove(client);
        if hs.is_empty() {
            st.grants.remove(&ino);
        }
    }
    if let Some(req) = st.requested.get_mut(client) {
        req.remove(&ino);
        if req.is_empty() {
            st.requested.remove(client);
        }
    }
}

/// The process-global lane — the one the stats inode exports and the one
/// rows 12–14 populate. Production never touches it today (no delegation
/// grants exist), so every field is 0 on every shipped mount BY
/// CONSTRUCTION — the dark-posture law this rung pins.
static GLOBAL_RECALL_LANE: Lazy<RecallLane> = Lazy::new(RecallLane::live);

/// The global recall lane.
pub fn global_recall_lane() -> &'static RecallLane {
    &GLOBAL_RECALL_LANE
}

/// The `dlm_recall` stats-inode object. Spec spellings
/// (`dlm_revokes_{issued,acked,timed_out}`, `dlm_thrash_demotions`) plus
/// the lane's own engagement gauges and the DERIVED caps published as
/// numbers, so the operator page can never drift from the arithmetic in
/// force. (`dlm_custody.dlm_revokes_{issued,expired}` is the S9 custody
/// plane's pull-model face — no ack, expiry terminal; this object is the
/// S10 push-model recall lane — scoped apart by their objects, same
/// family spelling, never a fork.)
pub fn recall_stats_json() -> serde_json::Value {
    let lane = global_recall_lane();
    let s = lane.stats();
    let cfg = lane.config();
    let live_clients = lane.state.lock().grants.values().flatten().count();
    serde_json::json!({
        "dlm_revokes_issued": s.issued,
        "dlm_revokes_acked": s.acked,
        "dlm_revokes_timed_out": s.timed_out,
        "dlm_recall_frames": s.frames,
        "dlm_recall_coalesced": s.coalesced,
        "dlm_recall_rate_deferred": s.rate_deferred,
        "dlm_recall_stale_acks": s.stale_acks,
        "dlm_recall_grants": s.grants,
        "dlm_recall_grant_refusals": s.grant_refusals,
        "dlm_thrash_demotions": s.thrash_demotions,
        "dlm_recall_repromotions": s.repromotions,
        "dlm_recall_outstanding": s.outstanding,
        "dlm_recall_pending": s.pending,
        "dlm_recall_demoted_objects": s.demoted_objects,
        "dlm_recall_deadline_ms": cfg.deadline.as_millis() as u64,
        "dlm_recall_batch_max": cfg.batch_max as u64,
        "dlm_recall_cooldown_ms": cfg.cooldown.as_millis() as u64,
        "dlm_recall_rate_cap_per_s": recall_rate_cap_per_s(
            cfg.batch_max,
            cfg.deadline,
            recall_roster_size(live_clients),
        ),
    })
}

/// `dlm_revoke_phase_ns` for the stats inode (the global lane's table).
pub fn revoke_phase_json() -> serde_json::Value {
    global_recall_lane().phase_json()
}
