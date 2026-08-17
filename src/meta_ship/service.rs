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
//! 4. **Piggyback the grant** — the object's fencing generation as the
//!    LOCAL authority knows it, which is what makes the client's fencing
//!    read sound without a second round trip.
//!
//! # The venue
//!
//! §6.7 is explicit: owner-side RPC handling runs on the pinned service
//! threads, **never on the conveyor's task**. The frame arrives on a
//! `sqz-cluster-svc{n}` lane; the batch's *execution* is then handed to
//! the **sqz-meta pool** ([`crate::meta_exec::spawn_meta_join`]), and the
//! lane awaits the join.
//!
//! That hop is deliberate and its reason is mechanical: `commit_tx`
//! spawns the per-volume conveyor **pass task** on the sqz-meta pool the
//! first time a volume needs one (`kv/backend.rs`) — the pool IS the venue
//! that owns the backend's tasks. A verb executed inline on a lane would
//! give the volume's entire commit conveyor a lane-lifetime venue — and
//! take it down with the lane. The IPC handoff-economy campaign's lesson
//! (never hand off onto a foreign runtime's global inject queue) is
//! respected in the other direction: this hop lands on the pool that
//! already owns every task the verb will interact with.
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
    pub cross_owner_refusals: u64,
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
    cross_owner: AtomicU64,
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
    /// eviction, their poll/reassert refuse `STATUS_DELEG_FENCED`.
    fenced: parking_lot::Mutex<HashSet<String>>,
    /// Objects with a mutation in flight (ino → count): grants DECLINE
    /// while registered, closing the grant-vs-mutation window.
    in_flight: scc::HashMap<u64, u64>,
    /// The delegation-grant sequence (per owner incarnation): minted
    /// BEFORE the lane records a grant, so a recall's frame-build fence
    /// covers every grant it could name.
    seq: AtomicU64,
}

impl Default for DelegHost {
    fn default() -> Self {
        Self {
            outbox: parking_lot::Mutex::new(HashMap::new()),
            poll_notify: squeezefs_ipc::sqz_notify::Notify::new(),
            ack_notify: squeezefs_ipc::sqz_notify::Notify::new(),
            fenced: parking_lot::Mutex::new(HashSet::new()),
            in_flight: scc::HashMap::new(),
            seq: AtomicU64::new(0),
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
            cross_owner: AtomicU64::new(0),
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
    /// another mount's host, the `daemon_verb_router` law).
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
            cross_owner_refusals: self.cross_owner.load(Ordering::Relaxed),
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

        // The handoff (see the module docs): execution lands on the
        // sqz-meta pool — the venue that owns the backend's tasks; this
        // lane awaits it.
        let t_dispatch = Instant::now();
        let Some(me) = self.owned() else {
            return self.refuse(
                req.id,
                STATUS_MALFORMED,
                "S8 owner service is shutting down — no handle to dispatch the batch on".into(),
            );
        };
        let joined = crate::meta_exec::spawn_meta_join("meta_ship_verb", async move {
            me.run_batch(frame).await
        })
        .await;
        super::owner_phase_record(OwnerPhase::Dispatch, t_dispatch);
        let results = match joined {
            Ok(results) => results,
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
        RpcResponse {
            id: req.id,
            status: STATUS_OK,
            body,
        }
    }

    /// Execute a batch's ops **in order** — in-batch causality is
    /// submission order, which is what lets a client pipeline a create and
    /// a lookup of the same name in one frame.
    ///
    /// The whole batch runs inside the `SHIP_CLIENT`/`SHIP_REVOKES`
    /// task-local scopes (rung 12): the mutation gate — which fires deep
    /// inside the backend's trait impl, where no client identity can be a
    /// parameter — reads the mutator's identity from the scope, and the
    /// surrenders it performs accumulate into each op's reply.
    async fn run_batch(&self, frame: MetaRequestFrame) -> Vec<MetaOpResult> {
        let client_epoch = frame.client_epoch;
        let client_id = frame.client_id.clone();
        let ops = frame.ops;
        SHIP_CLIENT
            .scope(client_id.clone(), async move {
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
        let t = Instant::now();
        let out = self.execute_inner(call).await;
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
                }
            }
            Err(e) => MetaOpResult {
                id,
                outcome: Err(WireError::from_error(&e)),
                grant: None,
                delegs: Vec::new(),
                revokes,
                revoke_fence,
            },
        }
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
    /// 5. read the STAMP only now — past step 4 it is post-commit for any
    ///    mutation the recall snapshot could have missed, so a stamp can
    ///    never name a state older than an un-recalled mutation.
    async fn issue_delegs(
        &self,
        client_id: &str,
        call: &MetaCall,
        reply: &MetaReply,
    ) -> Vec<DelegGrant> {
        if client_id.is_empty() || !tokens::delegation_enabled() {
            return Vec::new();
        }
        if self.deleg.fenced.lock().contains(client_id) {
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
                    ctime: inode.ctime,
                    mtime: inode.mtime,
                    size: inode.size,
                },
            });
        }
        out
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

    /// The cross-owner refusal for a DISCOVERED participant (see the
    /// module docs on why this is resolve-then-execute).
    fn cross_owner(&self, verb: MetaVerb, ino: u64) -> SqueezefsError {
        self.cross_owner.fetch_add(1, Ordering::Relaxed);
        super::CROSS_OWNER_REFUSALS.fetch_add(1, Ordering::Relaxed);
        super::cross_owner_error(
            verb,
            ino,
            "a participant discovered under the operation's guards",
        )
    }

    async fn execute_inner(&self, call: &MetaCall) -> crate::error::Result<MetaReply> {
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
                if let Some((child, _)) = self.inner.lookup_dentry(*parent, name).await? {
                    if !self.has_authority(child) {
                        return Err(self.cross_owner(MetaVerb::Unlink, child));
                    }
                }
                Ok(MetaReply::Ino(self.inner.unlink(*parent, name).await?))
            }
            MetaCall::Link {
                ino,
                new_parent,
                new_name,
            } => {
                if !self.has_authority(*ino) {
                    return Err(self.cross_owner(MetaVerb::Link, *ino));
                }
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
                if !self.has_authority(*new_parent) {
                    return Err(self.cross_owner(MetaVerb::Rename, *new_parent));
                }
                for (parent, name) in [(old_parent, old_name), (new_parent, new_name)] {
                    if let Some((participant, _)) = self.inner.lookup_dentry(*parent, name).await? {
                        if !self.has_authority(participant) {
                            return Err(self.cross_owner(MetaVerb::Rename, participant));
                        }
                    }
                }
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
    /// 100 ms scheduling grain, ceilinged at 5 s — strictly under the
    /// wire's 10 s dial/call socket timeout so a parked round can never
    /// be mistaken for a dead session. Published on every reply (the
    /// holder's freshness arithmetic consumes it).
    fn deleg_park(&self, cfg: &tokens::RecallConfig) -> Duration {
        (cfg.deadline / 2).clamp(Duration::from_millis(100), Duration::from_secs(5))
    }

    /// Is `client` fenced on the delegation plane?
    fn deleg_fenced(&self, client: &str) -> bool {
        self.deleg.fenced.lock().contains(client)
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
            if fenced.insert(t.client.clone()) {
                newly.insert(t.client.clone());
            }
        }
        drop(fenced);
        for client in newly {
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
        let mut wait_inos: Vec<u64> = Vec::new();
        for &ino in &registered {
            // The self-conflict: the mutating holder's own grant retires
            // as a SURRENDER riding this op's reply — never a wire recall
            // (zero added rounds on the serial mutate-then-lookup shape).
            if let Some(c) = &mutator {
                if lane.surrender(ino, c) {
                    let _ = SHIP_REVOKES.try_with(|r| r.borrow_mut().push(ino));
                }
            }
            if lane.recall_object(ino, now) > 0 {
                wait_inos.push(ino);
            } else if lane.holders(ino) > 0 {
                // Recalls already pending from an earlier conflict —
                // still ours to wait out.
                wait_inos.push(ino);
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
            if wait_inos.iter().all(|&ino| lane.holders(ino) == 0) {
                break;
            }
            // Wake on ack/expiry, bounded by the tick (the expire pass's
            // cadence) — the sqz timeout is the select-with-sleep form.
            let _ = squeezefs_ipc::sqz_time::timeout(tick, notified).await;
        }
        super::deleg_phase_record(super::DelegPhase::GateWait, t0);
        Some(registered)
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
                    ctime: inode.ctime,
                    mtime: inode.mtime,
                    size: inode.size,
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
