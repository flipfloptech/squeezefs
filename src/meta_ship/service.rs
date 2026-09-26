//! The **owner side** of S8: execute a shipped batch against the volumes
//! this node holds the D0 claim on (spec §6.7 decision 1, §6.9 S8).
//!
//! # What the owner does, in order
//!
//! 1. **Admit the frame** — vocabulary schema, then the era gate, then the
//!    grace gate. All three are refusals of the WHOLE frame, because a
//!    frame is one client's ordered batch and applying half of it would
//!    make the client's own causality unrecoverable.
//! 2. **Per op: the dedup window** — `(client_epoch, id)` is the
//!    idempotency key. The winner executes; a duplicate *awaits the
//!    winner's own outcome* (a `OnceCell`, so a retry that overlaps the
//!    original does not double-apply either) and is counted.
//! 3. **Execute the ORDINARY verb** on `RoutedMetaBackend` — the whole
//!    point of decision 1. Nothing about the owner's execution is special:
//!    the 4a `DlmGuard`s are taken where they always were, the commit
//!    rides the M7 conveyor, one tx is one checksummed journal entry.
//!    Since D-1 (finding F-A) a frame's verbs dispatch **concurrently
//!    across independent objects** — one chain per connected set of
//!    named inodes, serial inside a chain — so N independent mutations
//!    co-queue into ONE conveyor pass instead of paying N serial passes
//!    (see [`dependency_chains`] and `run_batch`).
//! 4. **Piggyback the grant** — the object's fencing generation as the
//!    LOCAL authority knows it, which is what makes the client's fencing
//!    read sound without a second round trip.
//!
//! # The venue
//!
//! §6.7 is explicit: owner-side RPC handling runs on its own threads,
//! **never on the conveyor's task**. The frame arrives on the connection's
//! own OS thread (`sqz-clw-conn`); the batch's *execution* goes through
//! `meta_ship::owner_dispatch` — the ONE door that records the dispatch's
//! decomposition (`meta_ship_owner_dispatch_ns`) and selects its venue.
//!
//! **Since D-5 (e2e perf audit DLM #7) the default venue is the accepting
//! thread itself.** The shipped shape hopped the batch onto the two shared
//! `sqz-meta` lanes ([`crate::meta_exec::spawn_meta_join`]) and awaited
//! the join; C-2's fleet attribution measured that hop at 2.0–2.3 ms per
//! verb — a cross-thread wake into lanes the co-writers' publish storms
//! saturate, and one back — the owner's largest term once the conveyor
//! stopped binding. The hop's original reason was mechanical and is gone:
//! it existed while `commit_tx` spawned the volume's conveyor pass task on
//! the AMBIENT runtime of whoever committed first, so a verb executed
//! inline on a lane would have given the conveyor a lane-lifetime venue.
//! Since rip-tokio-total every task the verb touches spawns on an explicit
//! process-global venue (the pass on the volume's `sqz-jrnl` lane or the
//! `sqz-meta` pool, never the caller's), task-locals are executor-agnostic,
//! and the connection thread is dedicated and parked for exactly this
//! reply — so polling the frame there deletes both hops and turns every
//! wake inside the frame into a direct unpark. The panic containment the
//! hop had is applied per dispatch (an unwinding verb answers
//! `STATUS_PANIC`; the session serves on). `SQUEEZEFS_META_SHIP_INLINE_
//! SERVE=0` restores the hop as the same-binary A/B control.
//!
//! # Cross-owner shapes
//!
//! `unlink` and `rename` discover participants under guards — the client
//! cannot see them — so the owner resolves the dentries first and refuses
//! loud (EXDEV, naming **S3.5**) when a participant lives on a volume
//! outside this node's authority. The resolve-then-execute pre-check is
//! *advisory* against a racing rename of the same name; what it buys is a
//! clean, named refusal instead of a mutation into a foreign volume's
//! tree, and the race window's remaining backstop is the volume's own
//! authority (a node holds the D0 claim only on the volumes it owns, and
//! §6.2's per-volume structures have exactly one appender by construction
//! under this granularity). Making it structural at the *record* layer
//! needs the node cache's third gate state — the residual
//! `kv/revalidate.rs` names — and is not S8's to invent.

use super::tokens;
use super::wire::*;
use super::OwnerPhase;
use crate::cluster_wire::{RpcAsyncService, RpcRequest, RpcResponse};
use crate::error::SqueezefsError;
use crate::meta_backend::{Metadata, RoutedMetaBackend};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// **Test seam** (rung 12; the `RecallConfig { valve: false }` precedent —
/// deliberately NOT a knob: the coherence law must not be operationally
/// removable). `false` disables the owner-side
/// recall-before-conflicting-publish gate so the permanent red half of the
/// delegation suite can DEMONSTRATE the stale serve the law prevents.
pub static TEST_DELEG_COHERENCE_LAW: AtomicBool = AtomicBool::new(true);

/// **Test seam** (rung 13; the same never-a-knob law): `false` disables
/// the OQ-2 foreign-read gate so the intent suite's PERMANENT red half
/// can demonstrate the stale foreign NEGATIVE the recall-forces-flush law
/// prevents.
pub static TEST_INTENT_READ_GATE: AtomicBool = AtomicBool::new(true);

/// **Test seam** (rung 13, the charter's arm-2 injection): a nonzero
/// errno refuses every intent-op APPLY with it — the deferred-error law's
/// deterministic driver (`fsync(dir)` must surface exactly this number).
pub static TEST_INTENT_APPLY_ERRNO: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(0);

/// **Test seam** (rung 13): pin the intent-supply chunk (0 = the derived
/// `intents::supply_chunk`) so the exhaustion/refill arms are
/// deterministic.
pub static TEST_INTENT_SUPPLY_CHUNK: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// PR 6's misdelivery seam, PER REQUEST (review round 1, Issue 20): the
/// op ids whose step `TEST_XV_SERVE_MISDELIVER_ONCE` armed — the frame
/// carrying such an op replies with a wrong correlation id, which the
/// client refuses exactly as it fails a dead session (the same-id resend
/// follows); a concurrent frame's reply is never the one poisoned.
static XV_MISDELIVER_OP_IDS: once_cell::sync::Lazy<parking_lot::Mutex<HashSet<u64>>> =
    once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(HashSet::new()));

squeezefs_ipc::sqz_task_local! {
    /// The shipping CLIENT whose verb is currently executing on this
    /// owner (set by `run_batch` around the batch's execution) — what
    /// lets the mutation gate, running deep inside the backend's trait
    /// impl, tell a self-conflict (surrender onto the reply) from a
    /// foreign one (wire recall). Absent on owner-local mutations.
    static SHIP_CLIENT: String;
}

squeezefs_ipc::sqz_task_local! {
    /// The reply-revoke accumulator: inos whose grants the currently
    /// executing op surrendered. Drained into the op's `MetaOpResult`
    /// so the revocation reaches the holder WITH the mutation's own
    /// reply (read-your-own-writes by construction, zero added rounds).
    static SHIP_REVOKES: RefCell<Vec<u64>>;
}

/// Partition a frame's ops into **dependency chains** (D-1 / F-A): ops
/// that name a common inode — transitively — share a chain and execute in
/// submission order; distinct chains dispatch concurrently. Returns one
/// dense chain index per op (first-appearance numbering, so a frame of
/// pairwise-independent ops maps to `0..n`).
///
/// The relation is `MetaCall::named_inos` overlap, union-find closed:
/// `rename` names both parents, `link` its inode and the new parent, so a
/// verb touching two objects fuses their chains. Children an
/// `unlink`/`rename` DISCOVERS under guards are deliberately not part of
/// the relation — the client cannot see them, so no same-frame op can be
/// causally dependent on them, and two ops racing such a child through
/// its own ino are exactly the 4a-guard race two local tasks have today.
pub fn dependency_chains(ops: &[MetaOp]) -> Vec<usize> {
    let named: Vec<Vec<u64>> = ops.iter().map(|op| op.call.named_inos()).collect();
    chains_by_named_inos(&named)
}

/// The relation behind [`dependency_chains`], over the named-inode lists
/// themselves — shared with the S9 publish plane (D-1b), whose frames
/// partition by the same overlap. `named[i]` is op `i`'s named inodes; an
/// op naming nothing is its own chain.
pub fn chains_by_named_inos(named: &[Vec<u64>]) -> Vec<usize> {
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    let mut parent: Vec<usize> = (0..named.len()).collect();
    let mut last_by_ino: HashMap<u64, usize> = HashMap::new();
    for (i, inos) in named.iter().enumerate() {
        for &ino in inos {
            if let Some(&j) = last_by_ino.get(&ino) {
                let a = find(&mut parent, i);
                let b = find(&mut parent, j);
                if a != b {
                    parent[a] = b;
                }
            }
            last_by_ino.insert(ino, i);
        }
    }
    let mut dense: HashMap<usize, usize> = HashMap::new();
    (0..named.len())
        .map(|i| {
            let root = find(&mut parent, i);
            let next = dense.len();
            *dense.entry(root).or_insert(next)
        })
        .collect()
}

/// Is this task executing a verb an owner service accepted **on behalf of
/// a shipping client** (per-volume claim admission §5.4a)?
///
/// Distinct from [`current_ship_client`], which answers `None` for the
/// empty client id ("the client wants no delegations") — this asks only
/// whether the OWNER-EXECUTE scope is active. The §5.4a M1 pre-check reads
/// it: inside this scope the authority in force is the owner service's own
/// per-volume `authority` vector (and its post-discovery cross-owner
/// checks, which have already run), never this node's client-side
/// `OwnerMap` — a dual-role node is BOTH, and consulting the client half
/// while executing as the owner would refuse a verb the owner is
/// authoritative for.
pub(crate) fn executing_for_ship_client() -> bool {
    SHIP_CLIENT.try_with(|_| ()).is_ok()
}

/// The shipping client whose verb is executing on THIS task, if any —
/// rung 14's placement hint reads it at the mint-slot pick (the same
/// deep-inside-the-backend position the mutation gate reads it from).
/// Absent (or empty — "the client wants no delegations") ⇒ `None`.
pub(crate) fn current_ship_client() -> Option<String> {
    SHIP_CLIENT
        .try_with(|c| (!c.is_empty()).then(|| c.clone()))
        .ok()
        .flatten()
}

squeezefs_ipc::sqz_task_local! {
    /// Rung 13: set (to `true`) around an INTENT APPLY's execution. The
    /// mutation gate then keeps the flushing holder's own UPDATE grant on
    /// the batch's directories alive (the grant IS the authority to apply
    /// these mutations — surrendering it per op would orphan the
    /// owner-side exclusivity record and kill the grant at the first
    /// fsync), while still recalling and waiting out every FOREIGN
    /// holder (the coherence law, unchanged).
    static SHIP_INTENT_APPLY: bool;
}

/// Absolute override for the derived dedup-window size.
pub const DEDUP_MAX_ENV: &str = "SQUEEZEFS_META_SHIP_DEDUP_MAX";

/// The dedup window's derived size: how far back a client's retry may
/// reach and still be answered from the original outcome.
///
/// Derivation (caps derive from system resources): the window must cover a
/// transport retry horizon, which is bounded by the in-flight batches a
/// client can have outstanding — `batch_max` ops per batch, one batch per
/// owner session, and a floor of 8192 so a small box still covers a long
/// pipeline. Retiring an entry costs exactly-once only for a retry that
/// arrives after `cap` newer ops from the SAME client, which is far past
/// any live retry.
pub fn dedup_cap() -> usize {
    if let Some(explicit) = crate::env_knobs::opt_int_knob::<usize>(DEDUP_MAX_ENV) {
        return explicit.max(1);
    }
    (super::router::batch_max() * 128).max(8192)
}

/// One dedup slot: the winner initializes it, duplicates await it.
type DedupSlot<T> = Arc<squeezefs_ipc::sqz_once::OnceCell<T>>;

/// The idempotency window: `(epoch, id)` → the winner's own outcome.
///
/// Generic over the cached outcome (`pub(crate)`) since the S9 co-writer
/// FREE verb: `meta_ship::publish` keys the same window on
/// `(lease_epoch, request_id)` — the spec's instruction is *reuse S8's
/// window or S3.5's post-images, never a third pattern*, and this is that
/// reuse. The semantics are byte-identical for the S8 instantiation.
pub(crate) struct DedupWindow<T> {
    slots: scc::HashMap<(u64, u64), DedupSlot<T>>,
    order: parking_lot::Mutex<VecDeque<(u64, u64)>>,
    cap: usize,
}

impl<T> DedupWindow<T> {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            slots: scc::HashMap::new(),
            order: parking_lot::Mutex::new(VecDeque::with_capacity(cap.min(4096))),
            cap,
        }
    }

    /// The slot for `key`, and whether the caller OWNS execution.
    ///
    /// The FIFO retirement takes a small mutex around a `VecDeque` push
    /// (and at most one pop). It is not a data-path lock: the op it guards
    /// is about to pay a network round trip and a journal commit, and FIFO
    /// order *is* the window's definition.
    pub(crate) fn slot(&self, key: (u64, u64)) -> (DedupSlot<T>, bool) {
        if let Some(slot) = self.slots.read_sync(&key, |_, v| Arc::clone(v)) {
            return (slot, false);
        }
        let fresh: DedupSlot<T> = Arc::new(squeezefs_ipc::sqz_once::OnceCell::new());
        match self.slots.insert_sync(key, Arc::clone(&fresh)) {
            Ok(()) => {
                let retire = {
                    let mut order = self.order.lock();
                    order.push_back(key);
                    if order.len() > self.cap {
                        order.pop_front()
                    } else {
                        None
                    }
                };
                if let Some(old) = retire {
                    let _ = self.slots.remove_sync(&old);
                }
                (fresh, true)
            }
            // Lost the insert race: the winner's slot is authoritative.
            Err(_) => match self.slots.read_sync(&key, |_, v| Arc::clone(v)) {
                Some(slot) => (slot, false),
                // Retired between the failed insert and this read — an
                // absurdly narrow window that can only happen at cap
                // pressure. Executing again is the safe answer here: the
                // alternative is refusing an operation the client never
                // got an answer for.
                None => (fresh, true),
            },
        }
    }

    fn entries(&self) -> u64 {
        self.slots.len() as u64
    }
}

/// Per-instance counters (the global family lives on the stats inode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ServiceStats {
    pub frames: u64,
    pub served: u64,
    pub dedup_hits: u64,
    pub dedup_entries: u64,
    pub stale_term_refusals: u64,
    pub grace_reclaims: u64,
    pub grace_conflicts: u64,
    pub not_owner_refusals: u64,
    /// **Must stay 0**: an owner-side execution unwound.
    pub panics: u64,
}

/// The owner-side S8 service: shipped metadata verbs executed against the
/// volumes this node has authority over.
pub struct MetaShipService {
    inner: Arc<RoutedMetaBackend>,
    /// The volumes this node is the metadata authority for. Defaults to
    /// every volume of `inner`, which is what holding the D0 claim on the
    /// whole set means.
    authority: Vec<bool>,
    term: AtomicU64,
    grace_until: parking_lot::Mutex<Option<Instant>>,
    dedup: DedupWindow<MetaOpResult>,
    frames: AtomicU64,
    served: AtomicU64,
    dedup_hits: AtomicU64,
    stale_term: AtomicU64,
    grace_reclaims: AtomicU64,
    grace_conflicts: AtomicU64,
    not_owner: AtomicU64,
    panics: AtomicU64,
    /// The S10 delegation host state (rung 12).
    deleg: DelegHost,
    /// A `Weak` to itself, so the batch handoff can hand an OWNED handle
    /// to the sqz-meta pool without the trait impl having to be on
    /// `Arc<Self>` (the `KvMetaBackend::conveyor_self` precedent).
    self_ref: std::sync::OnceLock<std::sync::Weak<MetaShipService>>,
}

/// The owner-side delegation state: recall outbox + wakeups, the fenced
/// holder set, the in-flight-mutation registry the grant path declines
/// against, and the grant sequence the reordering fence is built on.
struct DelegHost {
    /// Issued-but-undelivered recall frames per holder (the rung-11
    /// lane's `issue_pass` output, parked until the holder's channel
    /// round picks them up).
    outbox: parking_lot::Mutex<HashMap<String, Vec<WireRecallFrame>>>,
    /// Wakes parked recall-channel rounds (a recall was enqueued).
    poll_notify: squeezefs_ipc::sqz_notify::Notify,
    /// Wakes gate waiters (an ack or an expiry moved the holder set).
    ack_notify: squeezefs_ipc::sqz_notify::Notify,
    /// Holders whose recall deadline expired: escalated to membership
    /// eviction, their poll/reassert refuse `STATUS_DELEG_FENCED`. Keyed
    /// id → the INCARNATION (client_epoch) that was fenced (rung-13 live
    /// finding: "re-admission is by remount" was a dead letter when the
    /// HOLDER died — the remounted same-identity successor presented the
    /// same id string and stayed fenced forever; a fresh incarnation's
    /// first frame now clears the record, while the fenced zombie's own
    /// epoch stays refused).
    fenced: parking_lot::Mutex<HashMap<String, u64>>,
    /// The last incarnation each holder presented (what the expire pass
    /// fences, since a timed-out recall carries no epoch).
    incarnations: scc::HashMap<String, u64>,
    /// Objects with a mutation in flight (ino → count): grants DECLINE
    /// while registered, closing the grant-vs-mutation window.
    in_flight: scc::HashMap<u64, u64>,
    /// The delegation-grant sequence (per owner incarnation): minted
    /// BEFORE the lane records a grant, so a recall's frame-build fence
    /// covers every grant it could name.
    seq: AtomicU64,
    /// Rung 13: the EXCLUSIVE UPDATE holder per directory (§8.2 law 1) —
    /// dir → client id; validity is coupled to the recall lane's live
    /// grant (`lane.holds(dir, client)`), checked at every consult.
    update_holders: parking_lot::Mutex<HashMap<u64, String>>,
    /// Lock-free population gauge of `update_holders` — the read gate's
    /// one-relaxed-load fast path.
    update_count: AtomicU64,
    /// The intent-apply witness: `(lease_epoch, request_id)` → the
    /// winner's outcome (the S8/publish `DedupWindow`, reused — never a
    /// third idempotence pattern).
    intent_dedup: DedupWindow<IntentResult>,
}

impl Default for DelegHost {
    fn default() -> Self {
        Self {
            outbox: parking_lot::Mutex::new(HashMap::new()),
            poll_notify: squeezefs_ipc::sqz_notify::Notify::new(),
            ack_notify: squeezefs_ipc::sqz_notify::Notify::new(),
            fenced: parking_lot::Mutex::new(HashMap::new()),
            incarnations: scc::HashMap::new(),
            in_flight: scc::HashMap::new(),
            seq: AtomicU64::new(0),
            update_holders: parking_lot::Mutex::new(HashMap::new()),
            update_count: AtomicU64::new(0),
            intent_dedup: DedupWindow::new(dedup_cap()),
        }
    }
}

impl std::fmt::Debug for MetaShipService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetaShipService")
            .field("volumes", &self.authority.len())
            .field("term", &self.term.load(Ordering::Relaxed))
            .field("in_grace", &self.in_grace())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl MetaShipService {
    /// A service with authority over every volume of `inner` (the shape a
    /// node that write-mounted the set has: it holds the D0 claim on each
    /// member). Shipped verbs execute on the sqz-meta pool (module docs).
    pub fn new(inner: Arc<RoutedMetaBackend>) -> Arc<Self> {
        let all: Vec<usize> = (0..inner.volumes.len()).collect();
        Self::with_authority(inner, &all)
    }

    /// [`Self::new`] with an explicit authority set — the shape a node
    /// that owns a SUBSET of the set has (the multi-owner deployment S9
    /// mounts).
    pub fn with_authority(inner: Arc<RoutedMetaBackend>, volumes: &[usize]) -> Arc<Self> {
        let mut authority = vec![false; inner.volumes.len()];
        for &v in volumes {
            if let Some(slot) = authority.get_mut(v) {
                *slot = true;
            }
        }
        let term = crate::dlm::durable_term();
        let me = Arc::new(Self {
            inner,
            authority,
            term: AtomicU64::new(term),
            grace_until: parking_lot::Mutex::new(None),
            dedup: DedupWindow::new(dedup_cap()),
            frames: AtomicU64::new(0),
            served: AtomicU64::new(0),
            dedup_hits: AtomicU64::new(0),
            stale_term: AtomicU64::new(0),
            grace_reclaims: AtomicU64::new(0),
            grace_conflicts: AtomicU64::new(0),
            not_owner: AtomicU64::new(0),
            panics: AtomicU64::new(0),
            deleg: DelegHost::default(),
            self_ref: std::sync::OnceLock::new(),
        });
        let _ = me.self_ref.set(Arc::downgrade(&me));
        me
    }

    /// An owned handle to this service (the handoff's requirement).
    fn owned(&self) -> Option<Arc<Self>> {
        self.self_ref.get().and_then(|w| w.upgrade())
    }

    /// The backend this owner executes against (the mutation gate's
    /// ptr-eq identity — a foreign instance must never be gated by
    /// another mount's host).
    pub(crate) fn inner(&self) -> &Arc<RoutedMetaBackend> {
        &self.inner
    }

    /// The era this owner mints and admits in.
    pub fn term(&self) -> u64 {
        self.term.load(Ordering::Acquire)
    }

    /// Adopt a durable era (monotone).
    ///
    /// Call only AFTER the era is durable and barriered — the D0 gate's
    /// law, unchanged: a successor bumps `term` durably **before** arming,
    /// which is what makes every old-era token and every old-era request
    /// stale by construction (spec §6.7 "Recovery").
    pub fn bump_term(&self, term: u64) {
        let prev = self.term.fetch_max(term, Ordering::AcqRel);
        if term > prev {
            log::warn!(
                "S8 owner era {term} adopted (was {prev}): every request and every token from \
                 the earlier era is now stale by construction"
            );
        }
    }

    /// Open the failover **grace window**: only reclaim is admitted;
    /// fresh mutations are refused (spec §6.7 "Recovery" — without the
    /// window a failover triggers a cluster-wide forced-flush storm at the
    /// worst possible moment).
    pub fn open_grace(&self, window: Duration) {
        *self.grace_until.lock() = Some(Instant::now() + window);
        log::warn!(
            "S8 owner grace window OPEN for {window:?} (era {}): reclaim admitted, fresh \
             mutations refused, reads unaffected",
            self.term()
        );
    }

    /// Close the window — the successor's call when reclaim has drained.
    pub fn close_grace(&self) {
        if self.grace_until.lock().take().is_some() {
            log::info!("S8 owner grace window CLOSED (era {})", self.term());
        }
    }

    /// Is the window open? Elapsed windows close themselves.
    pub fn in_grace(&self) -> bool {
        let mut guard = self.grace_until.lock();
        match *guard {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                *guard = None;
                false
            }
            None => false,
        }
    }

    /// The dedup window's size.
    pub fn dedup_cap(&self) -> u64 {
        self.dedup.cap as u64
    }

    /// Per-instance counters.
    pub fn stats(&self) -> ServiceStats {
        ServiceStats {
            frames: self.frames.load(Ordering::Relaxed),
            served: self.served.load(Ordering::Relaxed),
            dedup_hits: self.dedup_hits.load(Ordering::Relaxed),
            dedup_entries: self.dedup.entries(),
            stale_term_refusals: self.stale_term.load(Ordering::Relaxed),
            grace_reclaims: self.grace_reclaims.load(Ordering::Relaxed),
            grace_conflicts: self.grace_conflicts.load(Ordering::Relaxed),
            not_owner_refusals: self.not_owner.load(Ordering::Relaxed),
            panics: self.panics.load(Ordering::Relaxed),
        }
    }

    /// Does this node have metadata authority over `ino`'s volume?
    fn has_authority(&self, ino: u64) -> bool {
        let (v_idx, _) = self.inner.route_ino(ino);
        self.authority.get(v_idx).copied().unwrap_or(false)
    }

    /// A frame-level refusal.
    ///
    /// The correlation id is ECHOED: `cluster_wire`'s dial side refuses a
    /// reply whose id does not match its call (a reflection/misdelivery
    /// guard), so a refusal that dropped the id would reach the client as
    /// "reply id mismatch" and hide the reason it was refused for.
    fn refuse(&self, id: u64, status: u16, reason: String) -> RpcResponse {
        log::warn!("S8 owner refused a frame: {reason}");
        RpcResponse {
            id,
            status,
            body: reason.into_bytes(),
        }
    }

    /// Serve one RPC — the frame gates, then the handed-off batch.
    async fn serve(&self, req: RpcRequest) -> RpcResponse {
        let t_total = Instant::now();
        let out = match req.verb {
            VERB_META_BATCH => self.serve_batch(&req, t_total).await,
            VERB_RECLAIM => self.serve_reclaim(&req).await,
            VERB_DELEG_RECALL => self.serve_deleg_poll(&req).await,
            VERB_DELEG_REASSERT => self.serve_deleg_reassert(&req).await,
            VERB_DELEG_INTENT => self.serve_deleg_intent(&req).await,
            other => RpcResponse {
                id: req.id,
                status: crate::cluster_wire::RPC_UNKNOWN_VERB,
                body: format!("S8: unknown verb {other}").into_bytes(),
            },
        };
        super::owner_phase_record(OwnerPhase::Total, t_total);
        out
    }

    async fn serve_batch(&self, req: &RpcRequest, t_admit: Instant) -> RpcResponse {
        self.frames.fetch_add(1, Ordering::Relaxed);
        let frame = match decode_request(&req.body) {
            Ok(f) => f,
            Err(e) => {
                return self.refuse(req.id, STATUS_MALFORMED, format!("{e}"));
            }
        };
        self.note_client_incarnation(&frame.client_id, frame.client_epoch);
        if frame.schema != META_SHIP_SCHEMA {
            return self.refuse(
                req.id,
                STATUS_SCHEMA,
                format!(
                    "peer speaks S8 vocabulary schema {} and this owner speaks \
                     {META_SHIP_SCHEMA} — refusing rather than guessing at a custody-bearing \
                     frame",
                    frame.schema
                ),
            );
        }
        let term = self.term();
        let mutating = frame.ops.iter().any(|op| op.call.mutating());
        // The era gate. `0` = "unknown, tell me" (a session's first
        // frame); anything else must match exactly, and a mismatch refuses
        // the frame WHOLE — nothing is executed, so a client that saw a
        // pre-fence answer cannot get a post-fence apply.
        //
        // **It gates CUSTODY, so it gates mutations.** A read's answer
        // does not depend on which era the client believes the owner is
        // in: it is served from the owner's current state, and the reply
        // carries the current era, so an old-era reader relearns for free.
        // Refusing reads too would cost every client a mandatory extra
        // round trip per failover and buy nothing — the same reasoning
        // that makes the grace gate mutation-only below.
        if mutating && frame.owner_term != 0 && frame.owner_term != term {
            self.stale_term.fetch_add(1, Ordering::Relaxed);
            super::STALE_TERM_REFUSALS.fetch_add(1, Ordering::Relaxed);
            return RpcResponse {
                id: req.id,
                status: STATUS_STALE_TERM,
                body: encode_reply(&MetaReplyFrame {
                    schema: META_SHIP_SCHEMA,
                    owner_term: term,
                    results: Vec::new(),
                })
                .unwrap_or_default(),
            };
        }
        // Authority: a client whose ownership map is stale must learn it
        // rather than have its verbs silently executed by a node that does
        // not hold the claim.
        if let Some(foreign) = frame
            .ops
            .iter()
            .map(|op| op.call.primary_ino())
            .find(|&ino| !self.has_authority(ino))
        {
            self.not_owner.fetch_add(1, Ordering::Relaxed);
            super::NOT_OWNER_REFUSALS.fetch_add(1, Ordering::Relaxed);
            return self.refuse(
                req.id,
                STATUS_NOT_OWNER,
                format!(
                    "ino {foreign} routes to a metadata volume this node holds no authority \
                     over — the client's ownership map is stale (re-read the volumes' \
                     writer_claim records)"
                ),
            );
        }
        // The grace gate: reclaim only. A read takes no grant, so it is
        // unaffected; a fresh mutation is the "conflicting fresh acquire"
        // the window exists to refuse.
        if mutating && self.in_grace() {
            if let Some(op) = frame.ops.iter().find(|op| op.call.mutating()) {
                self.grace_conflicts.fetch_add(1, Ordering::Relaxed);
                super::GRACE_CONFLICTS.fetch_add(1, Ordering::Relaxed);
                return self.refuse(
                    req.id,
                    STATUS_IN_GRACE,
                    format!(
                        "owner is inside its failover grace window (era {term}): {} is a fresh \
                         mutation, and the window admits only reclaim requests — retry after \
                         reclaiming (spec §6.7 Recovery)",
                        op.call.verb().name()
                    ),
                );
            }
        }
        super::owner_phase_record(OwnerPhase::Admit, t_admit);

        // The dispatch (see the module docs): on the accepting venue —
        // this connection's own thread — by default, or hopped onto the
        // sqz-meta pool and joined under the A/B control; either way
        // through the ONE door that records the split.
        let Some(me) = self.owned() else {
            return self.refuse(
                req.id,
                STATUS_MALFORMED,
                "S8 owner service is shutting down — no handle to dispatch the batch on".into(),
            );
        };
        let inline = super::inline_serve_enabled();
        let (joined, stamps) = super::owner_dispatch("meta_ship_verb", inline, async move {
            me.run_batch(frame, inline).await
        })
        .await;
        super::owner_phase_record_span(OwnerPhase::Dispatch, stamps.total());
        let results = match joined {
            Ok(Some(results)) => results,
            Ok(None) => {
                // A concurrently dispatched CHAIN unwound (its panic is
                // already counted by `contain`); the frame refuses whole,
                // exactly as the serial batch's own unwind did.
                self.panics.fetch_add(1, Ordering::Relaxed);
                super::OWNER_PANICS.fetch_add(1, Ordering::Relaxed);
                log::error!(
                    "S8 owner-side batch execution unwound: a dispatched verb chain panicked"
                );
                return RpcResponse {
                    id: req.id,
                    status: STATUS_PANIC,
                    body: b"S8 owner-side execution panicked: a dispatched verb chain unwound"
                        .to_vec(),
                };
            }
            Err(e) => {
                // A shipped verb UNWOUND. Nothing joins a data-path task,
                // so this counter is the only record its work was lost
                // (the RES-7/RES-8 discipline).
                self.panics.fetch_add(1, Ordering::Relaxed);
                super::OWNER_PANICS.fetch_add(1, Ordering::Relaxed);
                log::error!("S8 owner-side batch execution unwound: {e}");
                return RpcResponse {
                    id: req.id,
                    status: STATUS_PANIC,
                    body: format!("S8 owner-side execution panicked: {e}").into_bytes(),
                };
            }
        };
        // The seam's misdelivery: a reply the client cannot correlate —
        // for the frame that carried the armed op, and no other.
        let poisoned = {
            let mut ids = XV_MISDELIVER_OP_IDS.lock();
            let hit = results.iter().any(|r| ids.contains(&r.id));
            if hit {
                for r in &results {
                    ids.remove(&r.id);
                }
            }
            hit
        };
        let t_encode = Instant::now();
        let body = match encode_reply(&MetaReplyFrame {
            schema: META_SHIP_SCHEMA,
            owner_term: term,
            results,
        }) {
            Ok(b) => b,
            Err(e) => return self.refuse(req.id, STATUS_MALFORMED, format!("reply encode: {e}")),
        };
        super::owner_phase_record(OwnerPhase::ReplyEncode, t_encode);
        let id = if poisoned { req.id ^ (1 << 63) } else { req.id };
        RpcResponse {
            id,
            status: STATUS_OK,
            body,
        }
    }

    /// Execute a batch's ops — **concurrently across independent objects,
    /// in submission order within a dependency chain** (E2E perf audit
    /// D-1, DLM structural finding F-A).
    ///
    /// The serial form this replaced (`for op in ops { run_op(..).await }`)
    /// awaited each mutating verb's `commit_tx` before starting the next,
    /// so a frame of N independent verbs paid N conveyor passes — N
    /// journal writes, N barriers — at ≈ 106–128 µs each: the 9,473
    /// verbs/s authority ceiling. The M7 conveyor batches CONCURRENT
    /// committers into one pass (union leaf locks, one write, one
    /// barrier), so the lever is simply to have the frame's verbs park on
    /// it at the same time.
    ///
    /// **Causality is preserved exactly where it can exist.** Two ops are
    /// dependent iff they NAME a common inode (`MetaCall::named_inos`,
    /// closed transitively): the documented in-batch contract — "a create
    /// and a lookup of the same name in one frame see each other" — is
    /// the same-parent case, and a production frame coalesces
    /// independently in-flight callers (each awaits its own reply, so no
    /// caller can name an inode a same-frame verb has not yet minted).
    /// Dependent ops form one CHAIN executed serially in submission order;
    /// chains run concurrently, each on its own sqz-meta task. Reply
    /// order is op order regardless of completion order, the dedup window
    /// is per op (unchanged), and a chain's unwind refuses the frame whole
    /// (`None`) exactly as the serial batch's did.
    ///
    /// Each chain runs inside its own `SHIP_CLIENT`/`SHIP_REVOKES`
    /// task-local scopes (rung 12): the mutation gate — which fires deep
    /// inside the backend's trait impl, where no client identity can be a
    /// parameter — reads the mutator's identity from the scope, and the
    /// surrenders it performs accumulate into each op's reply. The
    /// per-chain revoke scope is what keeps one chain's surrenders out of
    /// a concurrent sibling's reply.
    ///
    /// **The chains' venue follows the frame's** (D-5): on the accepting
    /// venue the chains are polled concurrently IN this task (`join_all`,
    /// each contained — the publish plane's shape), so a frame's whole
    /// execution wakes the one parked connection thread; under the hop
    /// control each chain is its own `sqz-meta` task, as D-1 landed it.
    async fn run_batch(
        self: Arc<Self>,
        frame: MetaRequestFrame,
        inline: bool,
    ) -> Option<Vec<MetaOpResult>> {
        let client_epoch = frame.client_epoch;
        let client_id = Arc::<str>::from(frame.client_id.as_str());
        let ops = frame.ops;
        let n = ops.len();
        let chains = dependency_chains(&ops);
        if chains.len() <= 1 {
            // One chain (or an empty frame): the serial form, in-task.
            let me = Arc::clone(&self);
            return Some(me.run_chain(client_epoch, client_id, ops).await);
        }
        // Slot the ops into their chains (submission order within each).
        let mut per_chain: Vec<Vec<(usize, MetaOp)>> = vec![Vec::new(); chains.len()];
        for (idx, (op, chain)) in ops.into_iter().zip(chains).enumerate() {
            per_chain[chain].push((idx, op));
        }
        let chain_futs = per_chain.into_iter().map(|chain| {
            let me = Arc::clone(&self);
            let client_id = Arc::clone(&client_id);
            async move {
                let (idxs, calls): (Vec<usize>, Vec<MetaOp>) = chain.into_iter().unzip();
                let results = me.run_chain(client_epoch, client_id, calls).await;
                idxs.into_iter().zip(results).collect::<Vec<_>>()
            }
        });
        let joined: Vec<std::result::Result<Vec<(usize, MetaOpResult)>, ()>> = if inline {
            futures::future::join_all(chain_futs.map(|fut| async move {
                // A chain's unwind is contained HERE (the hop arm's
                // `contain` did it on the lane): the frame refuses whole,
                // the connection thread survives.
                futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut))
                    .await
                    .map_err(|_| {
                        log::error!(
                            "S8 owner: a dispatched verb chain PANICKED on the accepting \
                             connection thread — the frame refuses whole (STATUS_PANIC), the \
                             session serves on"
                        );
                    })
            }))
            .await
        } else {
            let joins: Vec<_> = chain_futs
                .map(|fut| crate::meta_exec::spawn_meta_join("meta_ship_verb_chain", fut))
                .collect();
            let mut out = Vec::with_capacity(joins.len());
            for join in joins {
                out.push(join.await.map_err(|_| ()));
            }
            out
        };
        let mut out: Vec<Option<MetaOpResult>> = (0..n).map(|_| None).collect();
        let mut unwound = false;
        for chain_out in joined {
            match chain_out {
                Ok(results) => {
                    for (idx, res) in results {
                        out[idx] = Some(res);
                    }
                }
                Err(()) => unwound = true,
            }
        }
        if unwound {
            return None;
        }
        Some(
            out.into_iter()
                .map(|r| r.expect("every op slotted into exactly one chain"))
                .collect(),
        )
    }

    /// One dependency chain: its ops in submission order, inside the
    /// rung-12 task-local scopes.
    async fn run_chain(
        self: Arc<Self>,
        client_epoch: u64,
        client_id: Arc<str>,
        ops: Vec<MetaOp>,
    ) -> Vec<MetaOpResult> {
        SHIP_CLIENT
            .scope(client_id.to_string(), async move {
                SHIP_REVOKES
                    .scope(RefCell::new(Vec::new()), async move {
                        let mut out = Vec::with_capacity(ops.len());
                        for op in ops {
                            out.push(self.run_op(client_epoch, &client_id, op).await);
                        }
                        out
                    })
                    .await
            })
            .await
    }

    async fn run_op(&self, client_epoch: u64, client_id: &str, op: MetaOp) -> MetaOpResult {
        // Reads are naturally idempotent, so they never consume a window
        // entry — the window is spent only where a replay would otherwise
        // double-apply or lie (a replayed create answering EEXIST, a
        // replayed unlink answering ENOENT).
        if !op.call.mutating() {
            return self.execute(op.id, &op.call, client_id).await;
        }
        let (slot, owns) = self.dedup.slot((client_epoch, op.id));
        if !owns {
            self.dedup_hits.fetch_add(1, Ordering::Relaxed);
            super::DEDUP_HITS.fetch_add(1, Ordering::Relaxed);
        }
        let id = op.id;
        let call = op.call.clone();
        let client = client_id.to_string();
        slot.get_or_init(|| async { self.execute(id, &call, &client).await })
            .await
            .clone()
    }

    async fn execute(&self, id: u64, call: &MetaCall, client_id: &str) -> MetaOpResult {
        // Reset the reply-revoke accumulator for THIS op (ops run
        // serially inside one batch scope).
        let _ = SHIP_REVOKES.try_with(|r| r.borrow_mut().clear());
        // Rung 13 — the OQ-2 foreign-read gate (recall-forces-flush): a
        // foreign client's lookup/readdir under a directory with an
        // outstanding UPDATE grant recalls it (which flushes the holder's
        // intent batch) BEFORE the serve — coherence over latency on the
        // foreign path. Self reads never recall (the holder answers its
        // own pending names locally).
        match call {
            MetaCall::LookupDentry { parent, .. } => {
                self.intent_read_gate(*parent, client_id).await;
            }
            MetaCall::Readdir { dir, .. } => {
                self.intent_read_gate(*dir, client_id).await;
            }
            _ => {}
        }
        if matches!(call, MetaCall::XvStep { .. }) {
            // PR 6's served-step seams: a HELD step (a live op spanning
            // cadence passes) and the per-request misdelivery marker.
            let hold =
                crate::meta_backend::crossvol_tx::TEST_XV_SERVE_HOLD_MS.load(Ordering::Relaxed);
            if hold > 0 {
                squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(hold)).await;
            }
            if crate::meta_backend::crossvol_tx::TEST_XV_SERVE_MISDELIVER_ONCE
                .swap(false, Ordering::SeqCst)
            {
                XV_MISDELIVER_OP_IDS.lock().insert(id);
            }
        }
        let t = Instant::now();
        let out = self.execute_inner(call, client_id).await;
        super::owner_phase_record(OwnerPhase::Execute, t);
        self.served.fetch_add(1, Ordering::Relaxed);
        super::SERVED_VERBS.fetch_add(1, Ordering::Relaxed);
        // Drain the surrenders the gate performed for THIS client during
        // the op — they ride the reply (the self-conflict law).
        let revokes: Vec<u64> = SHIP_REVOKES
            .try_with(|r| r.borrow_mut().drain(..).collect())
            .unwrap_or_default();
        let revoke_fence = if revokes.is_empty() {
            0
        } else {
            self.deleg.seq.load(Ordering::Acquire)
        };
        match out {
            Ok(reply) => {
                let ino = self.grant_object(call, &reply);
                let delegs = self.issue_delegs(client_id, call, &reply).await;
                let intent_grant = self.issue_update_grant(client_id, call, &reply).await;
                MetaOpResult {
                    id,
                    outcome: Ok(reply),
                    grant: ino.map(|ino| TokenGrant {
                        ino,
                        token: owner_authority_token(ino),
                        term: self.term(),
                    }),
                    delegs,
                    revokes,
                    revoke_fence,
                    intent_grant,
                }
            }
            Err(e) => MetaOpResult {
                id,
                outcome: Err(WireError::from_error(&e)),
                grant: None,
                delegs: Vec::new(),
                revokes,
                revoke_fence,
                intent_grant: None,
            },
        }
    }

    /// Is `dir`'s EXCLUSIVE UPDATE grant held by a client OTHER than
    /// `asking`? Lazily cleans records whose lane grant died.
    fn foreign_update_holder(&self, dir: u64, asking: Option<&str>) -> bool {
        if self.deleg.update_count.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let lane = tokens::global_recall_lane();
        let mut holders = self.deleg.update_holders.lock();
        match holders.get(&dir) {
            None => false,
            Some(client) => {
                if !lane.holds(dir, client) {
                    holders.remove(&dir);
                    self.deleg.update_count.fetch_sub(1, Ordering::Relaxed);
                    return false;
                }
                asking != Some(client.as_str())
            }
        }
    }

    /// The OQ-2 read gate's body: recall `dir`'s outstanding grants and
    /// wait them out (ack or deadline) — the recall FORCES the holder's
    /// flush, so the serve that follows is exact. `asking` empty = the
    /// owner's own local read (every holder is foreign to it).
    pub(crate) async fn intent_read_gate(&self, dir: u64, asking: &str) {
        if !TEST_INTENT_READ_GATE.load(Ordering::Relaxed) {
            return;
        }
        if !self.foreign_update_holder(dir, (!asking.is_empty()).then_some(asking)) {
            return;
        }
        super::intents::note_intent_read_recall();
        self.recall_and_wait(&[dir], None).await;
    }

    /// Which objects a successful LOOKUP-class reply may carry delegations
    /// for (the over-issue law: a lookup earns the PARENT and the resolved
    /// child — Ceph's move; a getattr its object; a readdir its
    /// directory). Mutations and refusals earn nothing.
    fn deleg_targets(call: &MetaCall, reply: &MetaReply) -> Vec<u64> {
        match call {
            MetaCall::LookupDentry { parent, .. } => {
                let mut t = vec![*parent];
                if let MetaReply::Inode(i) = reply {
                    t.push(i.ino);
                }
                t
            }
            MetaCall::Getattr { ino } => vec![*ino],
            MetaCall::Readdir { dir, .. } => vec![*dir],
            _ => Vec::new(),
        }
    }

    /// Issue the piggybacked delegation grants for one successful
    /// LOOKUP-class op (rung 12).
    ///
    /// The ordering here is load-bearing (the grant-vs-mutation
    /// check-then-act race, closed structurally):
    ///
    /// 1. pre-check `in_flight` (cheap decline);
    /// 2. mint `seq` BEFORE the lane records the grant — so any recall's
    ///    frame-build fence (read after `issue_pass`) covers every grant
    ///    it could name;
    /// 3. `try_grant` records it (the valve's gate — `Demoted` means
    ///    owner-served, no grant);
    /// 4. RE-CHECK `in_flight`: a mutation that raced in either shows
    ///    here (we retract by `surrender` — the grant was never sent) or
    ///    had already deregistered, i.e. committed, before this check;
    /// 5. read the STAMP (the volume's commit watermark) only now — past
    ///    step 4 it covers any mutation the recall snapshot could have
    ///    missed, so a stamp can never name a state older than an
    ///    un-recalled mutation.
    async fn issue_delegs(
        &self,
        client_id: &str,
        call: &MetaCall,
        reply: &MetaReply,
    ) -> Vec<DelegGrant> {
        if client_id.is_empty() || !tokens::delegation_enabled() {
            return Vec::new();
        }
        if self.deleg.fenced.lock().contains_key(client_id) {
            return Vec::new();
        }
        let lane = tokens::global_recall_lane();
        let mut out = Vec::new();
        for ino in Self::deleg_targets(call, reply) {
            if out.iter().any(|g: &DelegGrant| g.ino == ino) || !self.has_authority(ino) {
                continue;
            }
            if self.deleg.in_flight.contains_sync(&ino) {
                tokens::note_deleg_decline();
                continue;
            }
            let seq = self.deleg.seq.fetch_add(1, Ordering::AcqRel) + 1;
            match lane.try_grant(ino, client_id, Instant::now()) {
                tokens::GrantDecision::Demoted { .. } => continue,
                tokens::GrantDecision::Granted => {}
            }
            if self.deleg.in_flight.contains_sync(&ino) {
                // Raced a mutation's gate: retract the never-sent grant.
                lane.surrender(ino, client_id);
                tokens::note_deleg_decline();
                continue;
            }
            let inode = match self.inner.getattr_local(ino).await {
                Ok(i) => i,
                Err(_) => {
                    lane.surrender(ino, client_id);
                    tokens::note_deleg_decline();
                    continue;
                }
            };
            tokens::note_deleg_grant_issued();
            out.push(DelegGrant {
                ino,
                class: DELEG_CLASS_LOOKUP,
                dir: (inode.mode & libc::S_IFMT) == libc::S_IFDIR,
                seq,
                term: self.term(),
                stamp: DelegStamp {
                    watermark: self.inner.commit_watermark_of(ino),
                },
            });
        }
        out
    }

    /// Issue the piggybacked **EXCLUSIVE UPDATE grant** for one successful
    /// SHIPPED CREATE (rung 13, KD-MW-13): the intent-lock law applied to
    /// mint authority — the create the client was already shipping earns
    /// it the right to mint the NEXT ones locally.
    ///
    /// Target preference: the CREATED DIRECTORY when the create is a
    /// mkdir (the tar populate target), else the PARENT. One grant per
    /// reply (wire economy; the other candidate earns on the next RPC).
    ///
    /// The gate order is the LOOKUP path's (grant-vs-mutation race closed
    /// structurally), plus the §8.2 law-1 arms: EXCLUSIVITY (a live
    /// foreign holder declines — its recall was already forced by this
    /// create's own mutation gate, so a live record here is a racing
    /// grant, never a stale one), the VALVE (`try_grant` demotes hot
    /// directories — the storm brake), and the CENSUS BUDGET (an
    /// over-budget directory declines — the priced fallback).
    async fn issue_update_grant(
        &self,
        client_id: &str,
        call: &MetaCall,
        reply: &MetaReply,
    ) -> Option<IntentGrant> {
        if client_id.is_empty() || !super::intents::update_intents_enabled() {
            return None;
        }
        let MetaCall::CreateWithRdev { parent, .. } = call else {
            return None;
        };
        if self.deleg.fenced.lock().contains_key(client_id) {
            return None;
        }
        let created_dir = match reply {
            MetaReply::Inode(i) if (i.mode & libc::S_IFMT) == libc::S_IFDIR => Some(i.ino),
            _ => None,
        };
        let lane = tokens::global_recall_lane();
        for dir in [created_dir, Some(*parent)].into_iter().flatten() {
            if !self.has_authority(dir) {
                continue;
            }
            // Exclusivity (§8.2 law 1): one holder per directory.
            {
                let mut holders = self.deleg.update_holders.lock();
                match holders.get(&dir) {
                    Some(c) if c == client_id => {} // re-issue to the same holder
                    Some(c) if lane.holds(dir, c) => {
                        super::intents::note_update_decline();
                        continue;
                    }
                    Some(_) => {
                        // The lane grant died (recall/timeout): the record
                        // is stale — retire it and proceed.
                        holders.remove(&dir);
                        self.deleg.update_count.fetch_sub(1, Ordering::Relaxed);
                    }
                    None => {}
                }
            }
            if self.deleg.in_flight.contains_sync(&dir) {
                super::intents::note_update_decline();
                continue;
            }
            let seq = self.deleg.seq.fetch_add(1, Ordering::AcqRel) + 1;
            match lane.try_grant(dir, client_id, Instant::now()) {
                tokens::GrantDecision::Demoted { .. } => continue,
                tokens::GrantDecision::Granted => {}
            }
            if self.deleg.in_flight.contains_sync(&dir) {
                lane.surrender(dir, client_id);
                super::intents::note_update_decline();
                continue;
            }
            // The census + parent attrs — read AFTER the lane record and
            // the in-flight re-check (the rung-12 stamp law's position:
            // past this point any racing mutation sees the record and
            // recalls us).
            let census_max = intent_census_max();
            let entries = match self.inner.readdir_local(dir, 0, census_max + 1).await {
                Ok(e) => e,
                Err(_) => {
                    lane.surrender(dir, client_id);
                    super::intents::note_update_decline();
                    continue;
                }
            };
            if entries.len() > census_max {
                // Over the grant budget: the §8.2 priced fallback — the
                // grant declines, creates ship as today.
                lane.surrender(dir, client_id);
                super::intents::note_update_decline();
                continue;
            }
            let dir_inode = match self.inner.getattr_local(dir).await {
                Ok(i) => i,
                Err(_) => {
                    lane.surrender(dir, client_id);
                    super::intents::note_update_decline();
                    continue;
                }
            };
            if (dir_inode.mode & libc::S_IFMT) != libc::S_IFDIR {
                lane.surrender(dir, client_id);
                super::intents::note_update_decline();
                continue;
            }
            // The mint supply rides the grant (one chunk per reply).
            let chunk = super::intents::supply_chunk();
            let supply = match self.inner.reserve_intent_supply(dir, chunk).await {
                Ok((first_global, stride, count)) => Some(super::wire::InoSupply {
                    first_global,
                    stride,
                    count,
                }),
                Err(e) => {
                    log::warn!(
                        "S10 intents: supply reservation for dir {dir} failed ({e}) — granting \
                         census-only (mints decline until a flush refill succeeds)"
                    );
                    None
                }
            };
            self.deleg
                .update_holders
                .lock()
                .insert(dir, client_id.to_string());
            self.deleg.update_count.fetch_add(1, Ordering::Relaxed);
            super::intents::note_update_grant();
            // Rung 14 (placement): the granted directory joins the
            // client's hot-slot set and the supply reservation is a
            // policy event (a grant is a run's first evidence).
            super::placement::note_client_dir(client_id, self.inner.slot_of_ino(dir) as u16);
            super::placement::note_supply_event(
                client_id,
                &super::placement::PolicyConfig::derived(),
                Instant::now(),
                |s| self.inner.slot_volume(s),
            );
            let census: Vec<String> = entries
                .into_iter()
                .filter(|e| e.name != "." && e.name != "..")
                .map(|e| e.name)
                .collect();
            return Some(IntentGrant {
                dir,
                seq,
                term: self.term(),
                dir_attrs: super::wire::WireInode::from(&dir_inode),
                census,
                supply,
            });
        }
        None
    }

    /// Which object's generation rides this reply: the object the verb
    /// produced when it produced one (so a create's grant names the CHILD
    /// it just minted), else the primary object.
    fn grant_object(&self, call: &MetaCall, reply: &MetaReply) -> Option<u64> {
        match reply {
            MetaReply::Inode(i) => Some(i.ino),
            MetaReply::Ino(ino) => Some(*ino),
            _ => Some(call.primary_ino()),
        }
    }

    async fn execute_inner(
        &self,
        call: &MetaCall,
        client_id: &str,
    ) -> crate::error::Result<MetaReply> {
        match call {
            MetaCall::LookupDentry { parent, name } => {
                match self.inner.lookup_dentry(*parent, name).await? {
                    // Resolve the child HERE when this node also holds
                    // its volume — the whole-set shape, and every
                    // single-volume set. That makes a shipped `lookup`
                    // ONE round trip in the common case; the ino-only
                    // answer below is the cross-owner fallback the client
                    // routes itself. Both are correct because
                    // `lookup → getattr` was never atomic.
                    Some((child, _ft)) if self.has_authority(child) => {
                        let inode = self.inner.getattr(child).await?;
                        Ok(MetaReply::Inode(WireInode::from(&inode)))
                    }
                    Some((child, _ft)) => Ok(MetaReply::Ino(child)),
                    None => Err(SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("Dentry {name} not found in parent {parent}"),
                    ))),
                }
            }
            MetaCall::CreateWithRdev {
                parent,
                name,
                mode,
                uid,
                gid,
                rdev,
            } => {
                let inode = self
                    .inner
                    .create_with_rdev(*parent, name, *mode, *uid, *gid, *rdev)
                    .await?;
                // The mint is constrained to an owned volume by
                // `owners::constrain_mint_volume`, so this is a check on
                // the outcome rather than a hope: a child that landed
                // outside this node's authority would be a placement bug,
                // and it must be loud rather than silent.
                if !self.has_authority(inode.ino) {
                    log::error!(
                        "S8: create minted ino {} outside this node's authority — the mint \
                         constraint failed (spec §6.10 R4)",
                        inode.ino
                    );
                }
                Ok(MetaReply::Inode(WireInode::from(&inode)))
            }
            MetaCall::Unlink { parent, name } => {
                Ok(MetaReply::Ino(self.inner.unlink(*parent, name).await?))
            }
            MetaCall::Link {
                ino,
                new_parent,
                new_name,
            } => {
                let inode = self.inner.link(*ino, *new_parent, new_name).await?;
                Ok(MetaReply::Inode(WireInode::from(&inode)))
            }
            MetaCall::Rename {
                old_parent,
                old_name,
                new_parent,
                new_name,
                flags,
            } => {
                self.inner
                    .rename(*old_parent, old_name, *new_parent, new_name, *flags)
                    .await?;
                Ok(MetaReply::Unit)
            }
            MetaCall::Readdir { dir, offset, max } => {
                let entries = self.inner.readdir(*dir, *offset, *max as usize).await?;
                Ok(MetaReply::Dir(
                    entries.iter().map(WireDirEntry::from).collect(),
                ))
            }
            MetaCall::Getattr { ino } => {
                let inode = self.inner.getattr(*ino).await?;
                Ok(MetaReply::Inode(WireInode::from(&inode)))
            }
            MetaCall::Setattr {
                ino,
                mode,
                uid,
                gid,
                size,
                atime,
                mtime,
                ctime,
            } => {
                let inode = self
                    .inner
                    .setattr(*ino, *mode, *uid, *gid, *size, *atime, *mtime, *ctime)
                    .await?;
                Ok(MetaReply::Inode(WireInode::from(&inode)))
            }
            MetaCall::Getxattr { ino, name } => {
                Ok(MetaReply::Xattr(self.inner.getxattr(*ino, name).await?))
            }
            MetaCall::Setxattr { ino, name, value } => {
                self.inner.setxattr(*ino, name, value).await?;
                Ok(MetaReply::Unit)
            }
            MetaCall::Removexattr { ino, name } => {
                self.inner.removexattr(*ino, name).await?;
                Ok(MetaReply::Unit)
            }
            MetaCall::Listxattr { ino } => Ok(MetaReply::Names(self.inner.listxattr(*ino).await?)),
            MetaCall::DestroyInode { ino } => {
                self.inner.destroy_inode(*ino).await?;
                Ok(MetaReply::Unit)
            }
            // PR 6: the served side of a cross-owner step — the ONE
            // applier under THIS holder's guards and lease; the reply
            // follows the commit's durability lane by construction.
            MetaCall::XvStep {
                tx_id,
                step_idx,
                step,
                scope,
            } => {
                if crate::meta_backend::crossvol_tx::TEST_XV_SERVE_REFUSE.load(Ordering::SeqCst) {
                    return Err(SqueezefsError::refused(
                        libc::EIO,
                        "TEST_XV_SERVE_REFUSE: the holder is down before its commit".to_string(),
                    ));
                }
                if crate::meta_backend::crossvol_tx::TEST_XV_SERVE_SLOT_BUSY_ONCE
                    .swap(false, Ordering::SeqCst)
                {
                    // The door's own word for a slot that moved (the
                    // `KvError::SlotBusy` text the initiator classifies).
                    return Err(crate::meta_backend::kv::KvError::SlotBusy {
                        slot: crate::meta_backend::kv::record::forest_slot_of_ino(step.home_ino()),
                        holder: 0,
                        g: 0,
                    }
                    .into());
                }
                if crate::meta_backend::crossvol_tx::TEST_XV_SERVE_SKIP_ONCE
                    .swap(false, Ordering::SeqCst)
                {
                    return Ok(MetaReply::XvStep {
                        status: crate::meta_backend::crossvol_tx::status_code(
                            crate::meta_backend::crossvol_tx::XvStepStatus::ForeignSkipped,
                        ),
                        inode: None,
                    });
                }
                let scope = crate::meta_backend::crossvol_tx::GuardScope {
                    client: client_id,
                    scope: *scope,
                };
                let out = self
                    .inner
                    .xv_serve_step(*tx_id, *step_idx, step, scope)
                    .await?;
                Ok(MetaReply::XvStep {
                    status: crate::meta_backend::crossvol_tx::status_code(out.status),
                    inode: out.inode.map(|v| WireInode {
                        ino: step.home_ino(),
                        mode: v.mode,
                        uid: v.uid,
                        gid: v.gid,
                        size: v.size,
                        nlink: v.nlink,
                        atime: v.atime,
                        mtime: v.mtime,
                        ctime: v.ctime,
                        flags: v.flags,
                        rdev: v.rdev,
                    }),
                })
            }
            // Guard-free and exact (Issue 2/19): the initiator holds its
            // own guards across this read, and the answer must be THIS
            // holder's RAM-authoritative tree — a parent whose slot this
            // mount does not lease refuses, never answers from a
            // projection.
            MetaCall::LookupExact { parent, name } => {
                self.inner
                    .refuse_unless_slot_leased_here(*parent, "exact lookup")?;
                Ok(MetaReply::DentryExact(
                    self.inner
                        .lookup_dentry_exact_unguarded(*parent, name)
                        .await?,
                ))
            }
            // The travelling guard's two halves (§5.6 line 1): park the
            // initiator's 4a guards under its scope / release them.
            MetaCall::XvGuards {
                scope,
                inodes,
                dentries,
            } => {
                self.inner
                    .xv_serve_guards(client_id, *scope, inodes, dentries)
                    .await?;
                Ok(MetaReply::Unit)
            }
            MetaCall::XvRelease { scope, .. } => {
                crate::meta_backend::crossvol_tx::release_parked_guards(client_id, *scope);
                Ok(MetaReply::Unit)
            }
            // PR 7b — the striping verbs' served sides (design §5.6.5):
            // every word judged against durable state at the served mount
            // (`dir_stripe.rs`), nothing sized or unlocked by a peer's word.
            MetaCall::SupplyStripeIno {
                dir,
                index,
                supplier,
            } => Ok(MetaReply::StripeInoSupplied {
                ino: self
                    .inner
                    .serve_supply_stripe_ino(*dir, *index, *supplier)
                    .await?,
            }),
            MetaCall::IsEmpty { dir, scope } => {
                let scope = crate::meta_backend::crossvol_tx::GuardScope {
                    client: client_id,
                    scope: *scope,
                };
                Ok(MetaReply::Empty(
                    self.inner.serve_is_empty_scoped(*dir, scope).await?,
                ))
            }
            MetaCall::DestroyStripe { stripe } => {
                self.inner.serve_destroy_stripe(*stripe).await?;
                Ok(MetaReply::Unit)
            }
            // PR 13h (F-R6, review round 1 Issue 1): a peer's FORGET of
            // corpses in slots this mount reclaims — handed to this mount's
            // own FORGET-driven reclaim, whose admission re-checks every
            // ino (open count, the exact record's nlink, the single-drive
            // claim). Bounded EXECUTION (PR 3's law): a hint past the
            // reclaim batch cap is rejected before anything proportional
            // to it RUNS — its `Vec` is already decoded, bounded by the
            // CONTROL class cap (`decode_limit`), which is the allocation's
            // bound; an ino this mount does not reclaim is dropped and
            // counted, never read.
            MetaCall::ReclaimHint { inos, hops } => {
                if inos.len() > super::wire::RECLAIM_HINT_MAX_INOS {
                    return Err(SqueezefsError::refused(
                        libc::EINVAL,
                        format!(
                            "ReclaimHint: {} inos exceed the {} cap (one reclaim batch)",
                            inos.len(),
                            super::wire::RECLAIM_HINT_MAX_INOS
                        ),
                    ));
                }
                let (served, forwarded, misrouted) =
                    self.inner.serve_reclaim_hint(inos, *hops).await;
                log::debug!(
                    "reclaim hint (hop {hops}): {served} ino(s) admitted as this mount's own \
                     forgets, {forwarded} forwarded to the reclaimer tree 0 names, {misrouted} \
                     dropped"
                );
                Ok(MetaReply::Unit)
            }
        }
    }

    /// Serve a grace-window **reclaim**: the client re-asserts the objects
    /// it held before the failover and receives **fresh-era** grants.
    ///
    /// Reclaim is admitted whether or not the window is open — a client
    /// that reconnects late is re-asserting state, not acquiring it, and
    /// refusing it would strand the client's custody without making
    /// anything safer.
    async fn serve_reclaim(&self, req: &RpcRequest) -> RpcResponse {
        let frame = match decode_reclaim(&req.body) {
            Ok(f) => f,
            Err(e) => return self.refuse(req.id, STATUS_MALFORMED, format!("{e}")),
        };
        if frame.schema != META_SHIP_SCHEMA {
            return self.refuse(
                req.id,
                STATUS_SCHEMA,
                format!("reclaim schema {} != {META_SHIP_SCHEMA}", frame.schema),
            );
        }
        let term = self.term();
        let mut grants = Vec::with_capacity(frame.inos.len());
        for ino in frame.inos {
            if !self.has_authority(ino) {
                self.not_owner.fetch_add(1, Ordering::Relaxed);
                super::NOT_OWNER_REFUSALS.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            grants.push(TokenGrant {
                ino,
                token: owner_authority_token(ino),
                term,
            });
        }
        self.grace_reclaims.fetch_add(1, Ordering::Relaxed);
        super::GRACE_RECLAIMS.fetch_add(1, Ordering::Relaxed);
        log::info!(
            "S8 owner admitted a reclaim of {} object(s) in era {term} (grace {})",
            grants.len(),
            if self.in_grace() { "open" } else { "closed" }
        );
        match encode_reclaim_reply(&ReclaimReplyFrame {
            schema: META_SHIP_SCHEMA,
            owner_term: term,
            grants,
        }) {
            Ok(body) => RpcResponse {
                id: req.id,
                status: STATUS_OK,
                body,
            },
            Err(e) => self.refuse(
                req.id,
                STATUS_MALFORMED,
                format!("reclaim reply encode: {e}"),
            ),
        }
    }

    // -----------------------------------------------------------------
    // Rung 12 — the delegation host (S10's owner half).
    // -----------------------------------------------------------------

    /// The recall channel's park bound: how long an empty poll round may
    /// sit at the owner before answering empty. Derived from the live
    /// recall deadline (never a constant): half of it, floored at the
    /// park grain (the same floor the deadline's delivery term is built
    /// from — the two derivations cannot drift apart), ceilinged at 5 s —
    /// strictly under the wire's 10 s dial/call socket timeout so a
    /// parked round can never be mistaken for a dead session. Published
    /// on every reply (the holder's freshness arithmetic consumes it).
    fn deleg_park(&self, cfg: &tokens::RecallConfig) -> Duration {
        (cfg.deadline / 2).clamp(tokens::RECALL_POLL_PARK_FLOOR, Duration::from_secs(5))
    }

    /// Record the incarnation a holder presented, and clear a stale
    /// fence: a DIFFERENT epoch is a new process of the same identity —
    /// the documented re-admission-by-remount posture made real (the
    /// fenced zombie's own epoch stays refused).
    fn note_client_incarnation(&self, client: &str, epoch: u64) {
        if client.is_empty() {
            return;
        }
        match self.deleg.incarnations.entry_sync(client.to_string()) {
            scc::hash_map::Entry::Occupied(mut o) => {
                if *o.get() != epoch {
                    *o.get_mut() = epoch;
                    // Rung 14: a NEW incarnation of this identity — the
                    // old one's placement state (assignments + policy
                    // evidence) dies with it, so a zombie's half-run can
                    // never compose with its successor's into a trigger.
                    super::placement::fence_client(client);
                }
            }
            scc::hash_map::Entry::Vacant(v) => {
                v.insert_entry(epoch);
            }
        }
        let mut fenced = self.deleg.fenced.lock();
        if let Some(&fenced_epoch) = fenced.get(client) {
            if fenced_epoch != epoch {
                fenced.remove(client);
                log::info!(
                    "S10 delegation: holder '{client}' re-admitted — a new incarnation \
                     (epoch {epoch:#x}) replaced the fenced one ({fenced_epoch:#x}); the \
                     re-admission-by-remount posture"
                );
            }
        }
    }

    /// Is `client` fenced on the delegation plane (this incarnation)?
    fn deleg_fenced(&self, client: &str) -> bool {
        self.deleg.fenced.lock().contains_key(client)
    }

    /// Distribute freshly issued recall frames into the per-holder outbox
    /// and wake every parked channel round.
    fn deleg_distribute(&self, frames: Vec<tokens::RecallFrame>) {
        if frames.is_empty() {
            return;
        }
        let mut outbox = self.deleg.outbox.lock();
        for f in frames {
            outbox.entry(f.client).or_default().push(WireRecallFrame {
                frame_id: f.frame_id,
                inos: f.inos,
            });
        }
        drop(outbox);
        self.deleg.poll_notify.notify_waiters();
    }

    /// The deadline sweep + escalation: every timed-out recall is LOUD
    /// (`dlm_delegation_recall_timeouts`), fences the holder on this
    /// plane, and — when the membership plane is armed — evicts it
    /// (minting the S7 dead epoch; rung-11 residual #2, discharged). The
    /// grant is already DEAD (the lane retired it); the fence is what
    /// keeps a zombie holder from re-earning one without a remount.
    fn deleg_expire_pass(&self, now: Instant) {
        let dead = tokens::global_recall_lane().expire_overdue(now);
        if dead.is_empty() {
            return;
        }
        let mut fenced = self.deleg.fenced.lock();
        let mut newly: HashSet<String> = HashSet::new();
        for t in &dead {
            tokens::note_deleg_recall_timeout();
            let epoch = self
                .deleg
                .incarnations
                .read_sync(&t.client, |_, v| *v)
                .unwrap_or(0);
            if fenced.insert(t.client.clone(), epoch).is_none() {
                newly.insert(t.client.clone());
            }
        }
        drop(fenced);
        for client in newly {
            // Rung 14: the fenced holder's placement state dies with its
            // grants (the era-fencing law's deadline-expiry face).
            super::placement::fence_client(&client);
            // Rung 13: a fenced holder's EXCLUSIVE UPDATE records die with
            // its grants (the directory is grantable again).
            {
                let mut holders = self.deleg.update_holders.lock();
                let before = holders.len();
                holders.retain(|_, c| c != &client);
                let removed = before - holders.len();
                if removed > 0 {
                    self.deleg
                        .update_count
                        .fetch_sub(removed as u64, Ordering::Relaxed);
                }
            }
            log::error!(
                "S10 delegation: holder '{client}' missed its recall deadline — its grants are \
                 DEAD, the holder is FENCED on the delegation plane (poll/re-assert refuse; \
                 re-admission is by remount), and the timeout escalates to membership eviction \
                 (the transport_lease_overlong law: loud, never a silent wait)"
            );
            if let Some(owner) = crate::membership::installed_owner() {
                if owner
                    .evict(&client, "S10 delegation recall deadline expired")
                    .is_some()
                {
                    tokens::note_deleg_eviction();
                }
            }
        }
        self.deleg.ack_notify.notify_waiters();
    }

    /// **The coherence law** (design §8.2: recall-before-conflicting-
    /// publish): called through [`super::deleg_mutation_gate`] by the
    /// `RoutedMetaBackend` mutation surface with the objects the mutation
    /// invalidates, BEFORE any 4a acquisition (a waiting mutation holds
    /// no locks; the M7 conveyor batches strictly AFTER this returns, so
    /// the recall-ack happens-before the tx enqueue happens-before the
    /// commit — the two orderings never meet).
    ///
    /// Returns the registered ino set (the permit's payload), or `None`
    /// when the law seam is off (the permanent red half) — in which case
    /// nothing is registered and nothing is recalled.
    pub(crate) async fn deleg_mutation_begin(&self, inos: &[u64]) -> Option<Vec<u64>> {
        if !TEST_DELEG_COHERENCE_LAW.load(Ordering::Relaxed) {
            return None;
        }
        let lane = tokens::global_recall_lane();
        // Register FIRST (grants decline from here), then recall: a grant
        // path that misses the recall snapshot re-checks this registry
        // after recording, so the window is closed from both sides.
        let mut registered = Vec::with_capacity(inos.len());
        for &ino in inos {
            if registered.contains(&ino) {
                continue;
            }
            match self.deleg.in_flight.entry_sync(ino) {
                scc::hash_map::Entry::Occupied(mut o) => *o.get_mut() += 1,
                scc::hash_map::Entry::Vacant(v) => {
                    v.insert_entry(1);
                }
            }
            registered.push(ino);
        }
        if lane.outstanding_now() == 0 {
            // The common armed-but-idle shape: nothing delegated anywhere,
            // one atomic answers it.
            return Some(registered);
        }
        let now = Instant::now();
        let mutator = SHIP_CLIENT
            .try_with(|c| c.clone())
            .ok()
            .filter(|c| !c.is_empty());
        // Rung 13: inside an INTENT APPLY, the flushing holder's own
        // UPDATE grant on the batch's directories stays LIVE (it is the
        // authority these mutations execute under; surrendering it would
        // orphan the exclusivity record and recall the flusher into its
        // own flush). Foreign holders are recalled and waited out exactly
        // as before.
        let intent_apply = SHIP_INTENT_APPLY.try_with(|v| *v).unwrap_or(false);
        let mut wait_inos: Vec<(u64, bool)> = Vec::new();
        for &ino in &registered {
            let keep_self = intent_apply
                && mutator.as_deref().is_some_and(|c| {
                    let holders = self.deleg.update_holders.lock();
                    holders.get(&ino).is_some_and(|h| h == c)
                });
            // The self-conflict: the mutating holder's own grant retires
            // as a SURRENDER riding this op's reply — never a wire recall
            // (zero added rounds on the serial mutate-then-lookup shape).
            if !keep_self {
                if let Some(c) = &mutator {
                    if lane.surrender(ino, c) {
                        let _ = SHIP_REVOKES.try_with(|r| r.borrow_mut().push(ino));
                        // The surrendered grant may have been the UPDATE
                        // authority: retire the exclusivity record with it.
                        let mut holders = self.deleg.update_holders.lock();
                        if holders.get(&ino).is_some_and(|h| h == c) {
                            holders.remove(&ino);
                            self.deleg.update_count.fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            let skip = if keep_self { mutator.as_deref() } else { None };
            if lane.recall_object_excluding(ino, skip, now) > 0 {
                wait_inos.push((ino, keep_self));
            } else {
                let remaining = match skip {
                    Some(c) => lane.holders_excluding(ino, c),
                    None => lane.holders(ino),
                };
                if remaining > 0 {
                    // Recalls already pending from an earlier conflict —
                    // still ours to wait out.
                    wait_inos.push((ino, keep_self));
                }
            }
        }
        if wait_inos.is_empty() {
            return Some(registered);
        }
        self.deleg_distribute(lane.issue_pass(now));
        let cfg = lane.config();
        let tick = (cfg.deadline / 16).clamp(Duration::from_millis(1), Duration::from_millis(100));
        let t0 = Instant::now();
        loop {
            self.deleg_expire_pass(Instant::now());
            // Newly pending recalls (rate-deferred behind an in-flight
            // frame) issue as their holder's slot frees.
            self.deleg_distribute(lane.issue_pass(Instant::now()));
            let notified = self.deleg.ack_notify.notified();
            let all_clear = wait_inos.iter().all(|&(ino, keep_self)| {
                if keep_self {
                    match &mutator {
                        Some(c) => lane.holders_excluding(ino, c) == 0,
                        None => lane.holders(ino) == 0,
                    }
                } else {
                    lane.holders(ino) == 0
                }
            });
            if all_clear {
                break;
            }
            // Wake on ack/expiry, bounded by the tick (the expire pass's
            // cadence) — the sqz timeout is the select-with-sleep form.
            let _ = squeezefs_ipc::sqz_time::timeout(tick, notified).await;
        }
        super::deleg_phase_record(super::DelegPhase::GateWait, t0);
        Some(registered)
    }

    /// Recall every outstanding grant on `inos` (optionally excluding one
    /// holder) and wait them out — the OQ-2 read gate's engine, the
    /// mutation gate's loop factored for a uniform exclusion.
    async fn recall_and_wait(&self, inos: &[u64], exclude: Option<&str>) {
        let lane = tokens::global_recall_lane();
        let now = Instant::now();
        let mut wait_inos: Vec<u64> = Vec::new();
        for &ino in inos {
            if lane.recall_object_excluding(ino, exclude, now) > 0 {
                wait_inos.push(ino);
            } else {
                let remaining = match exclude {
                    Some(c) => lane.holders_excluding(ino, c),
                    None => lane.holders(ino),
                };
                if remaining > 0 {
                    wait_inos.push(ino);
                }
            }
        }
        if wait_inos.is_empty() {
            return;
        }
        self.deleg_distribute(lane.issue_pass(now));
        let cfg = lane.config();
        let tick = (cfg.deadline / 16).clamp(Duration::from_millis(1), Duration::from_millis(100));
        let t0 = Instant::now();
        loop {
            self.deleg_expire_pass(Instant::now());
            self.deleg_distribute(lane.issue_pass(Instant::now()));
            let notified = self.deleg.ack_notify.notified();
            let all_clear = wait_inos.iter().all(|&ino| match exclude {
                Some(c) => lane.holders_excluding(ino, c) == 0,
                None => lane.holders(ino) == 0,
            });
            if all_clear {
                break;
            }
            let _ = squeezefs_ipc::sqz_time::timeout(tick, notified).await;
        }
        super::deleg_phase_record(super::DelegPhase::GateWait, t0);
    }

    /// The permit's release half: the mutation committed (or failed) —
    /// grants on its objects may issue again.
    pub(crate) fn deleg_mutation_end(&self, inos: &[u64]) {
        for &ino in inos {
            let mut remove = false;
            if let Some(mut entry) = self.deleg.in_flight.get_sync(&ino) {
                let c = entry.get_mut();
                *c -= 1;
                remove = *c == 0;
            }
            if remove {
                let _ = self.deleg.in_flight.remove_if_sync(&ino, |c| *c == 0);
            }
        }
    }

    /// The **DelegRecall** verb: the holder's standing recall channel.
    /// Acks first (they free gate waiters), then drain-or-park up to the
    /// derived bound; the reply carries the frames plus the numbers the
    /// holder's validity arithmetic needs.
    async fn serve_deleg_poll(&self, req: &RpcRequest) -> RpcResponse {
        let frame = match decode_deleg_poll(&req.body) {
            Ok(f) => f,
            Err(e) => return self.refuse(req.id, STATUS_MALFORMED, format!("{e}")),
        };
        if frame.schema != META_SHIP_SCHEMA {
            return self.refuse(
                req.id,
                STATUS_SCHEMA,
                format!(
                    "deleg poll schema {} != {META_SHIP_SCHEMA} — refused rather than guessed",
                    frame.schema
                ),
            );
        }
        self.note_client_incarnation(&frame.client_id, frame.client_epoch);
        if self.deleg_fenced(&frame.client_id) {
            return self.refuse(
                req.id,
                STATUS_DELEG_FENCED,
                format!(
                    "holder '{}' is FENCED on the delegation plane (a recall deadline expired; \
                     re-admission is by remount)",
                    frame.client_id
                ),
            );
        }
        let lane = tokens::global_recall_lane();
        let now = Instant::now();
        for frame_id in &frame.acks {
            lane.ack_frame(&frame.client_id, *frame_id, now);
        }
        if !frame.acks.is_empty() {
            self.deleg.ack_notify.notify_waiters();
        }
        let cfg = lane.config();
        let park = self.deleg_park(&cfg);
        let deadline_at = Instant::now() + park;
        let frames = loop {
            let notified = self.deleg.poll_notify.notified();
            self.deleg_distribute(lane.issue_pass(Instant::now()));
            let mine = {
                let mut outbox = self.deleg.outbox.lock();
                outbox.remove(&frame.client_id).unwrap_or_default()
            };
            if !mine.is_empty() {
                break mine;
            }
            let remaining = deadline_at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break Vec::new();
            }
            let _ = squeezefs_ipc::sqz_time::timeout(
                remaining.min(Duration::from_millis(250)),
                notified,
            )
            .await;
        };
        let reply = DelegPollReply {
            schema: META_SHIP_SCHEMA,
            owner_term: self.term(),
            frames,
            fence_seq: self.deleg.seq.load(Ordering::Acquire),
            park_ms: park.as_millis() as u64,
            deadline_ms: cfg.deadline.as_millis() as u64,
        };
        match encode_deleg_poll_reply(&reply) {
            Ok(body) => RpcResponse {
                id: req.id,
                status: STATUS_OK,
                body,
            },
            Err(e) => self.refuse(req.id, STATUS_MALFORMED, format!("poll reply encode: {e}")),
        }
    }

    /// The **DelegReassert** verb: a holder re-asserts the delegations it
    /// held across an authority restart or a channel outage (KD-MW-5:
    /// they are RAM — re-assertion is the reconstruction). Admitted
    /// whether or not the grace window is open (the reclaim precedent:
    /// re-asserting state is not acquiring it); every object passes the
    /// same valve gate and in-flight decline as a fresh grant, and the
    /// grants carry FRESH stamps — the predecessor may have applied
    /// mutations this holder never saw, and the stamp check makes its
    /// serves wait for its view to catch up.
    async fn serve_deleg_reassert(&self, req: &RpcRequest) -> RpcResponse {
        let frame = match decode_deleg_reassert(&req.body) {
            Ok(f) => f,
            Err(e) => return self.refuse(req.id, STATUS_MALFORMED, format!("{e}")),
        };
        if frame.schema != META_SHIP_SCHEMA {
            return self.refuse(
                req.id,
                STATUS_SCHEMA,
                format!(
                    "deleg reassert schema {} != {META_SHIP_SCHEMA} — refused rather than \
                     guessed",
                    frame.schema
                ),
            );
        }
        self.note_client_incarnation(&frame.client_id, frame.client_epoch);
        if self.deleg_fenced(&frame.client_id) {
            return self.refuse(
                req.id,
                STATUS_DELEG_FENCED,
                format!(
                    "holder '{}' is FENCED on the delegation plane — its grants died with the \
                     recall deadline; re-admission is by remount",
                    frame.client_id
                ),
            );
        }
        let lane = tokens::global_recall_lane();
        let asserted = frame.inos.len();
        let mut grants = Vec::new();
        for ino in frame.inos {
            if !self.has_authority(ino)
                || self.deleg.in_flight.contains_sync(&ino)
                || grants.iter().any(|g: &DelegGrant| g.ino == ino)
            {
                continue;
            }
            let seq = self.deleg.seq.fetch_add(1, Ordering::AcqRel) + 1;
            match lane.try_grant(ino, &frame.client_id, Instant::now()) {
                tokens::GrantDecision::Demoted { .. } => continue,
                tokens::GrantDecision::Granted => {}
            }
            if self.deleg.in_flight.contains_sync(&ino) {
                lane.surrender(ino, &frame.client_id);
                continue;
            }
            let inode = match self.inner.getattr_local(ino).await {
                Ok(i) => i,
                Err(_) => {
                    lane.surrender(ino, &frame.client_id);
                    continue;
                }
            };
            tokens::note_deleg_grant_issued();
            grants.push(DelegGrant {
                ino,
                class: DELEG_CLASS_LOOKUP,
                dir: (inode.mode & libc::S_IFMT) == libc::S_IFDIR,
                seq,
                term: self.term(),
                stamp: DelegStamp {
                    watermark: self.inner.commit_watermark_of(ino),
                },
            });
        }
        log::info!(
            "S10 delegation: holder '{}' re-asserted {} object(s), {} re-admitted in era {} \
             (grace {})",
            frame.client_id,
            asserted,
            grants.len(),
            self.term(),
            if self.in_grace() { "open" } else { "closed" }
        );
        let reply = DelegReassertReply {
            schema: META_SHIP_SCHEMA,
            owner_term: self.term(),
            grants,
            fence_seq: self.deleg.seq.load(Ordering::Acquire),
        };
        match encode_deleg_reassert_reply(&reply) {
            Ok(body) => RpcResponse {
                id: req.id,
                status: STATUS_OK,
                body,
            },
            Err(e) => self.refuse(
                req.id,
                STATUS_MALFORMED,
                format!("reassert reply encode: {e}"),
            ),
        }
    }

    /// The **DelegIntent** verb (rung 13): apply one flushed intent batch
    /// IN ORDER, era-gated + witnessed FROM BIRTH (the rung-9 finding-#6
    /// law):
    ///
    /// 1. schema / fenced-holder / **owner-term** gates — the term is the
    ///    SUPPLY's era, so a successor refuses a dead era's numbers whole
    ///    before its recovered cursor could collide;
    /// 2. the **custody era gate** (`validate_publish_era` — the exact
    ///    validator the publish path runs): intents die with the custody
    ///    fence, refused BEFORE the witness window (a dead era's replay
    ///    must never be answered from cache);
    /// 3. the **grace gate** (intent applies are fresh mutations);
    /// 4. per op, the `(lease_epoch, request_id)` **witness**: a replay
    ///    answers the winner's own outcome, never a double-apply.
    async fn serve_deleg_intent(&self, req: &RpcRequest) -> RpcResponse {
        let frame = match decode_intent_batch(&req.body) {
            Ok(f) => f,
            Err(e) => return self.refuse(req.id, STATUS_MALFORMED, format!("{e}")),
        };
        if frame.schema != META_SHIP_SCHEMA {
            return self.refuse(
                req.id,
                STATUS_SCHEMA,
                format!(
                    "intent batch schema {} != {META_SHIP_SCHEMA} — refused rather than guessed",
                    frame.schema
                ),
            );
        }
        self.note_client_incarnation(&frame.client_id, frame.client_epoch);
        if self.deleg_fenced(&frame.client_id) {
            return self.refuse(
                req.id,
                STATUS_DELEG_FENCED,
                format!(
                    "holder '{}' is FENCED on the delegation plane — its intent batches die \
                     with its grants; re-admission is by remount",
                    frame.client_id
                ),
            );
        }
        let term = self.term();
        if frame.owner_term != term {
            self.stale_term.fetch_add(1, Ordering::Relaxed);
            super::STALE_TERM_REFUSALS.fetch_add(1, Ordering::Relaxed);
            return RpcResponse {
                id: req.id,
                status: STATUS_STALE_TERM,
                body: format!(
                    "intent batch names era {} and this owner is in era {term} — a dead era's \
                     supply must never reach a record (refused whole, nothing applied)",
                    frame.owner_term
                )
                .into_bytes(),
            };
        }
        if let Err(reason) =
            crate::data_grant::validate_publish_era(&frame.client_id, frame.lease_epoch)
        {
            super::intents::note_intent_stale_refusal();
            return self.refuse(req.id, STATUS_INTENT_LEASE, reason);
        }
        if self.in_grace() && !frame.ops.is_empty() {
            return self.refuse(
                req.id,
                STATUS_IN_GRACE,
                format!(
                    "owner is inside its failover grace window (era {term}): an intent batch \
                     is a fresh mutation — reclaim first, then retry (spec §6.7 Recovery)"
                ),
            );
        }
        // The handoff (the serve_batch law): execution on the sqz-meta
        // pool, under the SHIP_CLIENT + SHIP_INTENT_APPLY scopes.
        let Some(me) = self.owned() else {
            return self.refuse(
                req.id,
                STATUS_MALFORMED,
                "S8 owner service is shutting down — no handle to dispatch the batch on".into(),
            );
        };
        let client_id = frame.client_id.clone();
        let lease_epoch = frame.lease_epoch;
        let ops = frame.ops;
        let supply_request = frame.supply_request;
        let joined = crate::meta_exec::spawn_meta_join("meta_ship_intent_apply", async move {
            let results = SHIP_CLIENT
                .scope(client_id.clone(), async {
                    SHIP_INTENT_APPLY
                        .scope(true, async {
                            let mut out = Vec::with_capacity(ops.len());
                            for op in ops {
                                out.push(me.run_intent_op(lease_epoch, op).await);
                            }
                            out
                        })
                        .await
                })
                .await;
            // The refill rides the reply (one reservation per request;
            // anchored on the root). It runs INSIDE the client's scope —
            // rung 14: the supply's slot is the CLIENT's dedicated one
            // (the placement hint reads the scope at the pick), and a
            // successful reservation is a policy event (sustained
            // consumption is the migration trigger's evidence).
            let supply = if supply_request > 0 {
                let reserve = SHIP_CLIENT.scope(client_id.clone(), async {
                    me.inner
                        .reserve_intent_supply(
                            1,
                            supply_request.min(super::intents::supply_chunk()),
                        )
                        .await
                });
                match reserve.await {
                    Ok((first_global, stride, count)) => {
                        super::placement::note_supply_event(
                            &client_id,
                            &super::placement::PolicyConfig::derived(),
                            Instant::now(),
                            |s| me.inner.slot_volume(s),
                        );
                        Some(super::wire::InoSupply {
                            first_global,
                            stride,
                            count,
                        })
                    }
                    Err(e) => {
                        log::warn!("S10 intents: supply refill failed ({e})");
                        None
                    }
                }
            } else {
                None
            };
            (results, supply)
        })
        .await;
        let (results, supply) = match joined {
            Ok(out) => out,
            Err(e) => {
                self.panics.fetch_add(1, Ordering::Relaxed);
                super::OWNER_PANICS.fetch_add(1, Ordering::Relaxed);
                log::error!("S10 intent-batch execution unwound: {e}");
                return RpcResponse {
                    id: req.id,
                    status: STATUS_PANIC,
                    body: format!("S10 intent-batch execution panicked: {e}").into_bytes(),
                };
            }
        };
        let reply = IntentBatchReply {
            schema: META_SHIP_SCHEMA,
            owner_term: term,
            results,
            supply,
        };
        match encode_intent_batch_reply(&reply) {
            Ok(body) => RpcResponse {
                id: req.id,
                status: STATUS_OK,
                body,
            },
            Err(e) => self.refuse(
                req.id,
                STATUS_MALFORMED,
                format!("intent reply encode: {e}"),
            ),
        }
    }

    /// One intent op through the witness window — keyed `(lease_epoch,
    /// request_id)` per the charter (the publish window's key, the rung-9
    /// finding-#6 shape): the winner executes, a replay awaits + answers
    /// the winner's own outcome.
    async fn run_intent_op(&self, lease_epoch: u64, op: IntentOp) -> IntentResult {
        let (slot, owns) = self.deleg.intent_dedup.slot((lease_epoch, op.request_id));
        if !owns {
            super::intents::note_intent_replay();
            self.dedup_hits.fetch_add(1, Ordering::Relaxed);
            super::DEDUP_HITS.fetch_add(1, Ordering::Relaxed);
        }
        let request_id = op.request_id;
        let call = op.call.clone();
        slot.get_or_init(|| async { self.execute_intent(request_id, &call).await })
            .await
            .clone()
    }

    /// Execute one intent op (the winner's arm).
    async fn execute_intent(&self, request_id: u64, call: &IntentCall) -> IntentResult {
        let _ = SHIP_REVOKES.try_with(|r| r.borrow_mut().clear());
        let injected = TEST_INTENT_APPLY_ERRNO.load(Ordering::Relaxed);
        let out: crate::error::Result<()> = if injected != 0 {
            Err(SqueezefsError::refused(
                injected,
                "injected intent-apply refusal (TEST_INTENT_APPLY_ERRNO)".to_string(),
            ))
        } else {
            match call {
                IntentCall::CreateAt {
                    parent,
                    name,
                    ino,
                    mode,
                    uid,
                    gid,
                    rdev,
                    initial_size,
                    ts_ns,
                } => {
                    if !self.has_authority(*parent) || !self.has_authority(*ino) {
                        self.not_owner.fetch_add(1, Ordering::Relaxed);
                        super::NOT_OWNER_REFUSALS.fetch_add(1, Ordering::Relaxed);
                        Err(SqueezefsError::refused(
                            libc::EREMOTE,
                            format!(
                                "intent create ({parent}, '{name}') → ino {ino} names a volume \
                                 this node holds no authority over"
                            ),
                        ))
                    } else {
                        self.inner
                            .create_with_rdev_preset(
                                *parent,
                                name,
                                *mode,
                                *uid,
                                *gid,
                                *rdev,
                                *initial_size,
                                Some(crate::meta_backend::IntentCreatePreset {
                                    global_ino: *ino,
                                    ts_ns: *ts_ns,
                                }),
                            )
                            .await
                            .map(|_| ())
                    }
                }
                IntentCall::SetattrAt {
                    ino,
                    mode,
                    uid,
                    gid,
                    atime,
                    mtime,
                    ctime,
                } => {
                    if !self.has_authority(*ino) {
                        self.not_owner.fetch_add(1, Ordering::Relaxed);
                        super::NOT_OWNER_REFUSALS.fetch_add(1, Ordering::Relaxed);
                        Err(SqueezefsError::refused(
                            libc::EREMOTE,
                            format!("intent setattr names foreign ino {ino}"),
                        ))
                    } else {
                        self.inner
                            .setattr(*ino, *mode, *uid, *gid, None, *atime, *mtime, *ctime)
                            .await
                            .map(|_| ())
                    }
                }
            }
        };
        self.served.fetch_add(1, Ordering::Relaxed);
        super::SERVED_VERBS.fetch_add(1, Ordering::Relaxed);
        let revokes: Vec<u64> = SHIP_REVOKES
            .try_with(|r| r.borrow_mut().drain(..).collect())
            .unwrap_or_default();
        let revoke_fence = if revokes.is_empty() {
            0
        } else {
            self.deleg.seq.load(Ordering::Acquire)
        };
        match out {
            Ok(()) => {
                super::intents::note_intent_applied();
                IntentResult {
                    request_id,
                    outcome: Ok(()),
                    revokes,
                    revoke_fence,
                }
            }
            Err(e) => IntentResult {
                request_id,
                outcome: Err(WireError::from_error(&e)),
                revokes,
                revoke_fence,
            },
        }
    }
}

/// The census budget (the §8.2 "grant budget" — a directory too large to
/// carry declines the UPDATE grant): derived from the wire's CONTROL
/// frame class — half the frame at a 64 B/name budget, floored for small
/// boxes, capped so a grant stays a small fraction of the frame.
pub(crate) fn intent_census_max() -> usize {
    ((crate::cluster_wire::CONTROL_MAX_FRAME_BYTES as usize) / 2 / 64).clamp(256, 65_536)
}

impl RpcAsyncService for MetaShipService {
    fn call<'a>(
        &'a self,
        req: RpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcResponse> + Send + 'a>> {
        Box::pin(self.serve(req))
    }
}

/// The **local authority's** view of an object's fencing generation.
///
/// One function so the owner's grant read and any caller comparing a
/// client's cached token against the owner's answer read the SAME view —
/// deliberately the `LocalLockManager`, never the homing `DlmClient`,
/// because for an object it owns this node IS the authority and routing
/// its own read through the homing gate could bounce it off a client-side
/// cache.
pub fn owner_authority_token(ino: u64) -> u64 {
    static LOCAL: once_cell::sync::Lazy<crate::dlm::LocalLockManager> =
        once_cell::sync::Lazy::new(|| {
            crate::dlm::LocalLockManager::new().expect("the local lock view is infallible")
        });
    LOCAL.get_fencing_token_ino(ino)
}
