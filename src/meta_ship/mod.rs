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
//! * **No remote CUSTODY transfer — landed as S9's [`crate::data_grant`].**
//!   The grant that rides a reply here is still only the object's
//!   *generation* (what a fencing read needs); remote custody with TTL,
//!   renewal, revocation and dead-epoch quarantine is a lease the OWNER
//!   holds on the client's behalf, and it lives in that module.
//! * **No production arm — closed by S9 (2026-08-06).** [`arm_ownership`]
//!   had no caller in `main`, `mount` or any knob, because *"a mount that
//!   ships metadata but cannot ship data custody is not a product"*. Its
//!   caller is now [`crate::multi_writer::arm_multi_writer`], which arms
//!   ownership, the S7 data-plane fence and S9's remote write-custody plane
//!   together or refuses naming the missing piece.
//! * **The FUSE daemon is not switched onto the router**, and it still is
//!   not: it holds `Arc<RoutedMetaBackend>` and uses the *non-trait*
//!   capability surface (`create_with_rdev_size`, `readdir_stream`,
//!   `xattr_value_cap`, `set_layout_and_size`, `merge_layout_and_size`,
//!   `commit_block_refs`, `park_write_times`, `destroy_inodes`) that S8
//!   deliberately does not ship. **S9 shipped THAT surface instead** — as
//!   its own additive vocabulary in [`publish`], on its own verb block, so
//!   this module's pinned schema did not have to grow for verbs no S8 peer
//!   sends. The daemon's eight call sites now route through it.
//! * **No incompat bit.** Function shipping changes no on-disk structure,
//!   and ownership needs no new durable record: each volume's D0
//!   `writer_claim` already names its holder and (since S2) its durable
//!   `term`, so **the claim holder IS the owner**. With one owner per
//!   volume every §6.2 single-appender structure keeps exactly one
//!   appender. Bit 11 stays free for a stage that genuinely needs one
//!   (pinned by `tests/meta_ship_tests.rs`).
//!
//! # The intent-lock premise, verified against the three call sites
//!
//! §6.7 decision 3 asserts: *"There is no operation that needs a token but
//! performs no metadata RPC on the object first."* That claim is what
//! licenses the grant piggyback, so it was checked against the census's
//! **three** production `acquire_lock` sites rather than assumed:
//!
//! 1. **`fuse_client::get_or_acquire_lease`** (reached from WRITE, FLUSH,
//!    fsync, truncate/SETATTR and the release ladder through
//!    `acquire_write_lease`). The premise holds at **session** granularity
//!    and **not** per operation: an ino can only be named by the kernel
//!    because a LOOKUP or a CREATE resolved it, and both perform a
//!    metadata verb on the child (lookup's own `getattr`, create's mint) —
//!    but kernel entry/attr caching means a later open of a cached dentry
//!    can reach WRITE with no metadata RPC *in that operation*. This is
//!    precisely why the token cache exists, why a cold entry is re-earned
//!    by [`MetaShipRouter::refresh_token`] — a shipped `getattr`, i.e. a
//!    metadata RPC, never a second lock protocol — and why a miss is a
//!    loud must-stay-0 tripwire instead of a silent guess.
//! 2. **`routing::clone_file`**, source and destination (the other two
//!    sites). Here the premise holds per operation: the offline `clone`
//!    verb resolves both paths through metadata lookups before it locks,
//!    and it holds the D0 guard while doing so.
//!
//! One consequence worth stating because it bounds this stage: with an
//! armed plane, all three sites on a **foreign** ino refuse loud at the S4
//! gate — no node grants custody its owner never issued. So today the
//! token cache's foreign-object consumers are the fencing **reads**, not
//! the lease acquisitions; remote acquisition itself waits for S9's
//! custody protocol, and pretending otherwise here would be inventing a
//! custody transfer with no revocation path.
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

/// DLM S10 rung 13: the client half of the per-directory EXCLUSIVE UPDATE
/// grants + asynchronous create-intent batches (KD-MW-13).
pub mod intents;
pub mod owners;
/// DLM S10 rung 14: client-owned-slot placement (KD-MW-6) — the per-client
/// mint-targeting hint + the valve-bounded migration policy over the
/// existing online `migrate-meta-slot` engine.
pub mod placement;
/// DLM stage **S9**: the daemon's *non-trait* publish surface on the wire —
/// the deliberate gap this module's docs name above, closed as its own
/// additive vocabulary on its own verb block rather than by growing S8's
/// pinned schema.
pub mod publish;
pub mod router;
pub mod service;
pub mod tokens;
pub mod wire;

pub use owners::{
    arm_ownership, constrain_mint_volume, disarm_ownership, owner_map, owner_of_volume,
    ownership_armed, owns_volume, rearm_ownership, OwnerMap, PeerOwner,
};
pub use router::{MetaShipRouter, VerbRoute, TEST_SHIP_DRAIN_HOLD_MS};
pub(crate) use service::executing_for_ship_client;
pub use service::{
    owner_authority_token, MetaShipService, ServiceStats, TEST_DELEG_COHERENCE_LAW,
    TEST_INTENT_APPLY_ERRNO, TEST_INTENT_READ_GATE, TEST_INTENT_SUPPLY_CHUNK,
};
pub use tokens::{
    cache_cap, deleg_kernel_ttl_stretch, delegation_enabled, delegation_stats,
    delegation_stats_json, foreign_fencing_token, global_recall_lane, install_delegation,
    recall_batch_max_from, recall_cooldown_from, recall_deadline_from, recall_rate_cap_per_s,
    recall_stats_json, record_grant, revoke_phase_json, set_deleg_inval_sink,
    test_clear_delegations, test_clear_token_cache, token_cache_stats, DelegationStats,
    GrantDecision, RecallConfig, RecallFrame, RecallLane, RecallLaneStats, TimedOutRecall,
    TokenCacheStats, DELEGATION_ENV, RECALL_THRASH_CYCLES, TEST_DELEGATION_OVERRIDE,
};
pub use wire::*;

use crate::error::SqueezefsError;
use crate::fuse_client::LatencyHistogram;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// The DAEMON VERB ROUTER (rung 9 — the S8 arm's client half).
//
// S8 shipped `MetaShipRouter` with no production consumer: the FUSE daemon
// holds `Arc<RoutedMetaBackend>` and calls the `Metadata` trait verbs on it
// directly, so on a co-writer every one of them met the local write gate
// ("S8's un-routed-daemon gap"). The arm closes the gap at the ONE place no
// call site can bypass — the trait impl itself (`meta_backend/mod.rs`)
// consults [`daemon_verb_router`] at verb entry and delegates the foreign
// ones to the router `cowriter::arm` installs here. Correct by
// construction: a 25th call site cannot forget to route, exactly as the
// per-ino deferred-op accumulator argument goes.
//
// Solo cost: `ownership_armed()` is one relaxed load and the FIRST check,
// so every mount that ships today stops there (the solo re-gate's law).
// ---------------------------------------------------------------------------

static DAEMON_VERB_ROUTER: Lazy<arc_swap::ArcSwapOption<router::MetaShipRouter>> =
    Lazy::new(arc_swap::ArcSwapOption::const_empty);

/// Install the process-global daemon verb router (`cowriter::arm`'s act).
pub fn install_daemon_verb_router(router: Arc<router::MetaShipRouter>) {
    DAEMON_VERB_ROUTER.store(Some(router));
}

/// Remove it (`cowriter` disarm/teardown; a stale install with a DISARMED
/// plane is inert — the armed load gates first).
pub fn uninstall_daemon_verb_router() {
    DAEMON_VERB_ROUTER.store(None);
}

/// The daemon-side S8 hook decision: the installed router, iff
///
/// 1. the ownership plane is ARMED (one relaxed load — the solo fast path),
/// 2. a router is installed **and wraps exactly `be`** (a foreign
///    instance — a probe set, a test sandbox — must never be re-routed
///    through another mount's plane), and
/// 3. at least one of `participants` routes to a volume with a FOREIGN
///    owner (the same decision `MetaShipRouter::route_verb` makes, which
///    is what keeps the pair recursion-free: the router's Local arm only
///    ever executes when this function answered `None`).
pub fn daemon_verb_router(
    be: &crate::meta_backend::RoutedMetaBackend,
    participants: &[u64],
) -> Option<Arc<router::MetaShipRouter>> {
    if !owners::ownership_armed() {
        return None;
    }
    let r = DAEMON_VERB_ROUTER.load_full()?;
    if !std::ptr::eq(Arc::as_ptr(r.inner()), be as *const _) {
        return None;
    }
    if participants.iter().any(|&ino| {
        let (v_idx, _) = be.route_ino(ino);
        owners::owner_of_volume(v_idx).is_some()
    }) {
        Some(r)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// The DELEGATION HOST (rung 12 — S10's owner half on this process).
//
// Installed by the multi-writer arm beside the S8 owner service (and by
// the delegation suites); the RoutedMetaBackend mutation surface consults
// [`deleg_mutation_gate`] at verb entry — the same one-place-no-bypass
// argument as the daemon verb router above. Solo cost: one relaxed load.
// ---------------------------------------------------------------------------

/// Is a delegation host installed (the one-relaxed-load fast path).
static DELEG_HOST_ARMED: AtomicBool = AtomicBool::new(false);

static DELEG_HOST: Lazy<arc_swap::ArcSwapOption<service::MetaShipService>> =
    Lazy::new(arc_swap::ArcSwapOption::const_empty);

/// Install the process-global delegation host (the multi-writer arm's
/// act, beside `install_daemon_verb_router`).
pub fn install_delegation_host(svc: Arc<service::MetaShipService>) {
    DELEG_HOST.store(Some(svc));
    DELEG_HOST_ARMED.store(true, Ordering::Release);
}

/// Remove it (disarm/teardown; a stale install is inert — the armed load
/// gates first).
pub fn uninstall_delegation_host() {
    DELEG_HOST_ARMED.store(false, Ordering::Release);
    DELEG_HOST.store(None);
}

/// Held across a gated mutation's execution: while alive, delegation
/// grants on the named objects DECLINE (the grant-vs-mutation
/// check-then-act race's structural half — see `service.rs`).
pub struct DelegGatePermit {
    svc: Arc<service::MetaShipService>,
    inos: Vec<u64>,
}

impl Drop for DelegGatePermit {
    fn drop(&mut self) {
        self.svc.deleg_mutation_end(&self.inos);
    }
}

/// **The coherence law's entry point** (design §8.2: recall-before-
/// conflicting-publish, enforced OWNER-side): called by the
/// `RoutedMetaBackend` mutation surface — trait verbs AND the layout
/// publish funnels — BEFORE any 4a acquisition, with the objects the
/// mutation invalidates. Recalls every outstanding delegation on them
/// through the rung-11 lane (the mutating client's own grant surrenders
/// onto its reply instead) and returns only when every recall is acked or
/// expired-dead. The returned permit is held across the mutation so no
/// grant can be issued into the window.
///
/// Solo cost: ONE relaxed load (`DELEG_HOST_ARMED`). Armed-but-idle cost:
/// one more atomic (the lane's outstanding gauge) — the gate pays real
/// work only while delegations exist.
pub async fn deleg_mutation_gate(
    be: &crate::meta_backend::RoutedMetaBackend,
    inos: &[u64],
) -> Option<DelegGatePermit> {
    if !DELEG_HOST_ARMED.load(Ordering::Relaxed) {
        return None;
    }
    let host = DELEG_HOST.load_full()?;
    if !std::ptr::eq(Arc::as_ptr(host.inner()), be as *const _) {
        return None;
    }
    host.deleg_mutation_begin(inos)
        .await
        .map(|inos| DelegGatePermit { svc: host, inos })
}

/// **The OQ-2 read gate's LOCAL face** (rung 13): the owner's own
/// lookup/readdir of a directory with an outstanding foreign UPDATE grant
/// recalls it first (which flushes the holder's intent batch), exactly as
/// a shipped foreign read does — otherwise the owner's own `ls` could
/// miss un-flushed foreign intents unboundedly. One relaxed load when no
/// delegation host is armed; one more when no UPDATE grant exists.
pub async fn deleg_read_gate(be: &crate::meta_backend::RoutedMetaBackend, dir: u64) {
    if !DELEG_HOST_ARMED.load(Ordering::Relaxed) {
        return;
    }
    let Some(host) = DELEG_HOST.load_full() else {
        return;
    };
    if !std::ptr::eq(Arc::as_ptr(host.inner()), be as *const _) {
        return;
    }
    host.intent_read_gate(dir, "").await;
}

/// Should a mutation site pay participant RESOLUTION (the unlink/rename
/// child reads) for the gate? Only when the host is armed over `be` AND
/// delegations are actually outstanding — so the resolution cost is zero
/// everywhere the plane is dark.
pub fn deleg_gate_wants_children(be: &crate::meta_backend::RoutedMetaBackend) -> bool {
    if !DELEG_HOST_ARMED.load(Ordering::Relaxed) {
        return false;
    }
    let Some(host) = DELEG_HOST.load_full() else {
        return false;
    };
    std::ptr::eq(Arc::as_ptr(host.inner()), be as *const _)
        && tokens::global_recall_lane().outstanding_now() > 0
}

/// Phases of one delegation revocation (`dlm_delegation_recall_phase_ns` —
/// design §13; composes with the rung-11 lane's `dlm_revoke_phase_ns`:
/// the lane times issue/ack_wait, this table times the two halves the
/// DELEGATION adds around it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum DelegPhase {
    /// Owner side: the mutation gate's recall-issued → all-clear wait.
    GateWait = 0,
    /// Holder side: recall received → entries drained and dropped.
    HolderDrain = 1,
}

const DELEG_PHASES: usize = 2;
const DELEG_PHASE_NAMES: [&str; DELEG_PHASES] = ["gate_wait", "holder_drain"];

static DELEG_PROF: Lazy<[LatencyHistogram; DELEG_PHASES]> =
    Lazy::new(|| std::array::from_fn(|_| LatencyHistogram::default()));

/// Record a delegation phase span started at `t0`.
#[inline]
pub(crate) fn deleg_phase_record(phase: DelegPhase, t0: std::time::Instant) {
    DELEG_PROF[phase as usize].record(t0.elapsed());
}

/// `dlm_delegation_recall_phase_ns` — the delegation-scoped decomposition.
pub fn delegation_recall_phase_json() -> serde_json::Value {
    let mut phases = serde_json::Map::new();
    for (i, name) in DELEG_PHASE_NAMES.iter().enumerate() {
        phases.insert((*name).to_string(), DELEG_PROF[i].to_json());
    }
    serde_json::Value::Object(phases)
}

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
/// Owner-side dispatches (both planes) executed ON the accepting
/// connection's thread — the D-5 lever's engagement
/// (`SQUEEZEFS_META_SHIP_INLINE_SERVE=1`; ships OFF on the 2026-09-08
/// fleet row).
static OWNER_DISPATCH_INLINE: AtomicU64 = AtomicU64::new(0);
/// Owner-side dispatches that HOPPED onto the shared `sqz-meta` lanes and
/// were joined from the connection thread (the shipped default).
static OWNER_DISPATCH_HOPS: AtomicU64 = AtomicU64::new(0);
/// Volume ownership records WRITTEN by the offline `volume set-owners`
/// verb (§11.1). 0 on every mount by construction — a mount never
/// assigns; only the verb's own process moves this.
static OWNER_ASSIGNMENTS: AtomicU64 = AtomicU64::new(0);
/// `volume set-owners` invocations refused, each naming its cause.
static OWNER_ASSIGN_REFUSALS: AtomicU64 = AtomicU64::new(0);
/// KD-PV-15's ledger: subtree roots the verb minted. `0` beside a nonzero
/// `volumes_owned` on a peer is the Issue-23 shape (risk R16) — a node
/// owning a volume but no work.
static SUBTREE_ROOTS_MINTED: AtomicU64 = AtomicU64::new(0);

/// One volume's ownership record was written by the assignment verb.
pub fn note_owner_assignment() {
    OWNER_ASSIGNMENTS.fetch_add(1, Ordering::Relaxed);
}

/// One `volume set-owners` invocation was refused.
pub fn note_owner_assign_refusal() {
    OWNER_ASSIGN_REFUSALS.fetch_add(1, Ordering::Relaxed);
}

/// One subtree root was minted (KD-PV-15).
pub fn note_subtree_root_minted() {
    SUBTREE_ROOTS_MINTED.fetch_add(1, Ordering::Relaxed);
}

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
    /// GAUGE (**must stay 0**): volumes whose ownership entry the runtime
    /// re-derivation POISONED — §5.10's fail-closed law engaging.
    pub owner_map_poisoned_volumes: u64,
    /// GAUGE, **not a tripwire and not a liveness monitor**: peer-owned
    /// volumes that had NO appender when this mount DERIVED its map —
    /// owners that had not started yet (a cold fleet) or were down.
    /// `volumes_peer_owned` minus this is how many of the set's other
    /// owners were present at that instant; the set authority mounts
    /// first by the documented order, so it reports `K − 1` for the life
    /// of the mount. `squeezefs volume get-owners` is the live instrument.
    pub volumes_peer_unclaimed: u64,
    /// The `volume set-owners` ledger (§11.1) — volume records written,
    /// invocations refused, and KD-PV-15 roots minted. All three are 0 on
    /// every mount: the verb is an offline process of its own.
    pub owner_assignments: u64,
    pub owner_assign_refusals: u64,
    pub subtree_roots_minted: u64,
    /// D-5: owner-side dispatches (S8 frames, S9 publish calls / groups /
    /// frees / harvests) executed on the accepting connection's thread vs
    /// hopped onto the `sqz-meta` lanes. `inline + hops` ≡
    /// `meta_ship_owner_dispatch_ns.total.count` (an unwound HOP records no
    /// split — its lane-side instants die with the task).
    pub owner_dispatch_inline: u64,
    pub owner_dispatch_hops: u64,
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
        owner_map_poisoned_volumes: owners::poisoned_volumes(),
        volumes_peer_unclaimed: owners::unclaimed_peer_volumes(),
        owner_assignments: OWNER_ASSIGNMENTS.load(Ordering::Relaxed),
        owner_assign_refusals: OWNER_ASSIGN_REFUSALS.load(Ordering::Relaxed),
        subtree_roots_minted: SUBTREE_ROOTS_MINTED.load(Ordering::Relaxed),
        owner_dispatch_inline: OWNER_DISPATCH_INLINE.load(Ordering::Relaxed),
        owner_dispatch_hops: OWNER_DISPATCH_HOPS.load(Ordering::Relaxed),
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
        "owner_map_poisoned_volumes": s.owner_map_poisoned_volumes,
        // The DEGRADED-set gauge (§11.1): how many peer-owned volumes had
        // no appender when this map was derived. Nonzero is a set running
        // with an owner missing — those subtrees refuse loud at the ship
        // site — never a fault in this mount.
        "volumes_peer_unclaimed": s.volumes_peer_unclaimed,
        // The offline verb's ledger (§11.1). A mount never assigns, so
        // all three staying 0 on a live mount is the law, not the load.
        "owner_assignments": s.owner_assignments,
        "owner_assign_refusals": s.owner_assign_refusals,
        "subtree_roots_minted": s.subtree_roots_minted,
        // D-5: which venue served the owner's dispatches (both planes).
        // `hops` ≡ every dispatch on the default; `inline` carries them
        // under SQUEEZEFS_META_SHIP_INLINE_SERVE=1.
        "owner_dispatch_inline": s.owner_dispatch_inline,
        "owner_dispatch_hops": s.owner_dispatch_hops,
        "dlm_rpcs_meta": s.dlm_rpcs_meta,
        "dlm_grace_reclaims": s.grace_reclaims,
        "dlm_grace_conflicts": s.grace_conflicts,
        "dlm_token_cache_hits": t.hits,
        "dlm_token_cache_misses": t.misses,
        "dlm_token_cache_evictions": t.evictions,
        "dlm_token_cache_grants": t.grants,
        "dlm_token_cache_entries": t.entries,
        "dlm_token_cache_bytes": t.bytes,
        // The S11 rung-15 range extension (§9.2): live cached range spans.
        "dlm_token_cache_range_spans": t.range_spans,
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

/// The recall lane's LIVE deadline evidence (spec R3's law: derive from
/// the live p99, never a constant): the shipped-verb round trip
/// (`meta_ship_phase_ns.rtt` p99) plus the owner-side frame service
/// (`meta_ship_owner_phase_ns.total` p99) — the two terms a recall's
/// drain-and-ack must traverse. `None` when neither table has a sample
/// (an unarmed mount; the derivation then falls back to the lease-TTL
/// fence bound).
pub(crate) fn live_recall_evidence_us() -> Option<u64> {
    let rtt = SHIP_PROF[ShipPhase::Rtt as usize].p99_micros();
    let owner = OWNER_PROF[OwnerPhase::Total as usize].p99_micros();
    match (rtt, owner) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
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

/// Record an owner-side phase as an already-measured span (the frame's
/// `dispatch` is its hop's `total` — the same instants, so the two tables
/// agree to the ns).
#[inline]
pub(crate) fn owner_phase_record_span(phase: OwnerPhase, span: std::time::Duration) {
    OWNER_PROF[phase as usize].record(span);
}

/// Exact `(sum_ns, count)` of one owner-side phase (the in-process
/// harness's instrument; the stats inode carries the same words as JSON).
pub fn owner_phase_totals(phase: OwnerPhase) -> (u64, u64) {
    let h = &OWNER_PROF[phase as usize];
    (h.sum_ns(), h.count())
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
// The owner's dispatch — its venue and its decomposition (e2e perf audit
// D-5, DLM board #7; `docs/design-e2e-perf-audit.md` §5.3 row 18).
//
// A served frame is admitted on the connection's own thread
// (`sqz-clw-conn`, one per connection) and then DISPATCHED onto the two
// shared `sqz-meta` lanes and awaited: a cross-thread wake into the lanes
// the co-writers' publish storms saturate, and one back. Quiet ≤ 32 µs;
// on the fleet the S8 frame's `dispatch` read 2.0–2.3 ms per verb — the
// same class C-2 removed from the journal path. `meta_ship_owner_dispatch_
// ns` splits the hop at its two thread boundaries, exact-sum, always-on,
// zero-alloc (the `uring_fs_write_phase_ns` pattern):
//
//   queue_hop : submitted → the lane's first poll (the spawn → lane queue
//               → pop wait, behind whatever the lanes hold);
//   run       : first poll → the work's last instruction — the frame's
//               own execution, INCLUDING every wake it takes back onto a
//               lane (a conveyor fan-out, a 4a guard) while it runs there;
//   wake_hop  : done → the awaiting connection thread resumed (the
//               oneshot's waker → `thread::unpark` → dispatch);
//   total     : submitted → observed (≡ the S8 frame's `dispatch`).
//
// The lever: EXECUTE ON THE ACCEPTING VENUE. The connection thread is
// dedicated and parked for exactly this reply, so polling the frame's
// future there (`sqz_blocking::block_on` already is its executor) deletes
// both hops AND turns every wake inside `run` into a direct unpark of a
// parked thread instead of a lane-queue wait. Nothing the venue rule
// protected depends on the lane any more: since rip-tokio-total every
// task the verb touches spawns on an explicit process-global venue (the
// conveyor's pass on the volume's `sqz-jrnl` lane or `spawn_meta`, the
// checkpoint/times tasks on `spawn_meta`), task-locals are executor-
// agnostic, and the panic containment `contain` gave the hop is applied
// here per dispatch (an unwinding verb answers PANIC and the session
// serves on).
//
// SHIPS OFF on measurement (the 2026-09-08 squeeze-test fleet row,
// `.benchmarks/2026-09-08-d5-fleet-squeeze-test.md`, two same-binary
// A-B-B-A brackets in both orders): the venue deletes the two hops exactly
// (queue_hop 332–459 + wake_hop 183–237 µs → 0) but the served work itself
// runs 0.6–0.9 ms SLOWER on the connection thread (`run` 536–770 →
// 1,211–1,473 µs, p99 bucket 16 → 32 ms) — every wake inside the work is
// now an OS unpark of a dedicated thread instead of a lane re-queue, and
// the durability lane's fan-out pays it (`sqz-jrnl` +17–25 %). Net: the
// dispatch total par-to-worse, co-writer publish latency +15–49 %, ingest
// −1.5…−7 %, verbs/s par. The in-process win was measured under an
// artificial 4 × 200 µs lane hog; the field's lanes run at ρ ≈ 0.2. The
// hop is the shipped default; `SQUEEZEFS_META_SHIP_INLINE_SERVE=1` is the
// same-binary A/B lever for a venue whose lanes ARE saturated.
// ---------------------------------------------------------------------------

/// The venue lever: `0`/off (default) = the shipped `spawn_meta_join` hop;
/// `1` = a served dispatch is polled on the accepting connection's thread.
pub const INLINE_SERVE_ENV: &str = "SQUEEZEFS_META_SHIP_INLINE_SERVE";

/// Read the lever (once per served frame — one getenv per wire round trip,
/// the `SQUEEZEFS_PUBLISH_CONVEYOR_GROUP` precedent).
pub(crate) fn inline_serve_enabled() -> bool {
    crate::env_knobs::bool_knob(INLINE_SERVE_ENV, false)
}

/// Phases of `meta_ship_owner_dispatch_ns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum OwnerDispatchPhase {
    QueueHop = 0,
    Run = 1,
    WakeHop = 2,
    Total = 3,
}

const OWNER_DISPATCH_PHASES: usize = 4;
const OWNER_DISPATCH_PHASE_NAMES: [&str; OWNER_DISPATCH_PHASES] =
    ["queue_hop", "run", "wake_hop", "total"];

static OWNER_DISPATCH_PROF: Lazy<[LatencyHistogram; OWNER_DISPATCH_PHASES]> =
    Lazy::new(|| std::array::from_fn(|_| LatencyHistogram::default()));

/// `meta_ship_owner_dispatch_ns` — the dispatch-hop decomposition.
pub fn owner_dispatch_json() -> serde_json::Value {
    let mut phases = serde_json::Map::new();
    for (i, name) in OWNER_DISPATCH_PHASE_NAMES.iter().enumerate() {
        phases.insert((*name).to_string(), OWNER_DISPATCH_PROF[i].to_json());
    }
    serde_json::Value::Object(phases)
}

/// Exact `(sum_ns, count)` of one dispatch phase.
pub fn owner_dispatch_totals(phase: OwnerDispatchPhase) -> (u64, u64) {
    let h = &OWNER_DISPATCH_PROF[phase as usize];
    (h.sum_ns(), h.count())
}

/// The four instants of one owner-side dispatch.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DispatchStamps {
    pub submitted_at: std::time::Instant,
    pub picked_at: std::time::Instant,
    pub done_at: std::time::Instant,
    pub observed_at: std::time::Instant,
}

impl DispatchStamps {
    /// `submitted → observed`.
    pub fn total(&self) -> std::time::Duration {
        self.observed_at
            .saturating_duration_since(self.submitted_at)
    }

    fn record(&self) {
        let prof = &*OWNER_DISPATCH_PROF;
        prof[OwnerDispatchPhase::QueueHop as usize]
            .record(self.picked_at.saturating_duration_since(self.submitted_at));
        prof[OwnerDispatchPhase::Run as usize]
            .record(self.done_at.saturating_duration_since(self.picked_at));
        prof[OwnerDispatchPhase::WakeHop as usize]
            .record(self.observed_at.saturating_duration_since(self.done_at));
        prof[OwnerDispatchPhase::Total as usize].record(self.total());
    }
}

/// An owner-side dispatch whose work UNWOUND (the `JoinError` face the
/// `spawn_meta_join` receiver had): the caller counts it on its plane's
/// `owner_panics` and answers the PANIC outcome.
#[derive(Debug)]
pub(crate) struct DispatchUnwound {
    site: &'static str,
}

impl std::fmt::Display for DispatchUnwound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "owner dispatch '{}' unwound", self.site)
    }
}

/// **The one door every owner-side dispatch goes through** (both planes):
/// run `fut` on the venue the lever selects and record its decomposition.
///
/// `inline` (the default): poll `fut` HERE, on the accepting connection's
/// thread, with its unwind contained — `queue_hop` and `wake_hop` are 0
/// by construction and `run` is the work. Otherwise: the shipped hop —
/// spawn onto the `sqz-meta` pool and join, the lane-side instants riding
/// the oneshot (`meta_exec::spawn_meta_join_stamped`). Returns the
/// outcome and the stamps; an unwound hop records no split (its lane-side
/// instants died with the task) but is still counted on `hops`.
pub(crate) async fn owner_dispatch<F, T>(
    site: &'static str,
    inline: bool,
    fut: F,
) -> (std::result::Result<T, DispatchUnwound>, DispatchStamps)
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let submitted_at = std::time::Instant::now();
    if inline {
        OWNER_DISPATCH_INLINE.fetch_add(1, Ordering::Relaxed);
        let out = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut)).await;
        let done_at = std::time::Instant::now();
        let stamps = DispatchStamps {
            submitted_at,
            picked_at: submitted_at,
            done_at,
            observed_at: done_at,
        };
        stamps.record();
        let out = out.map_err(|_| {
            // The `contain` discipline, on this venue: the panic is a bug
            // and the record of lost work; the connection serves on.
            log::error!(
                "owner dispatch '{site}' PANICKED on the accepting connection thread — the \
                 verb's work is LOST; the frame answers PANIC and the session serves on"
            );
            DispatchUnwound { site }
        });
        return (out, stamps);
    }
    OWNER_DISPATCH_HOPS.fetch_add(1, Ordering::Relaxed);
    let joined = crate::meta_exec::spawn_meta_join_stamped(site, fut).await;
    let observed_at = std::time::Instant::now();
    match joined {
        Ok((out, lane)) => {
            let stamps = DispatchStamps {
                submitted_at,
                picked_at: lane.picked_at,
                done_at: lane.done_at,
                observed_at,
            };
            stamps.record();
            (Ok(out), stamps)
        }
        Err(_) => (
            Err(DispatchUnwound { site }),
            DispatchStamps {
                submitted_at,
                picked_at: observed_at,
                done_at: observed_at,
                observed_at,
            },
        ),
    }
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
