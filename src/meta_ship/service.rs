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
//! the runtime that owns the backend's tasks
//! ([`MetaShipService::new`]'s `runtime` argument), and the lane awaits
//! the join.
//!
//! That hop is deliberate and its reason is mechanical: `commit_tx`
//! spawns the per-volume conveyor **pass task** on the committer's own
//! runtime the first time a volume needs one (`kv/backend.rs`). A verb
//! executed inline on a lane would therefore give the volume's entire
//! commit conveyor a single-threaded current-thread runtime whose lifetime
//! is the lane's — and take it down with the lane. The IPC handoff-economy
//! campaign's lesson (never hand off onto a foreign runtime's global
//! inject queue) is respected in the other direction: this hop lands on
//! the runtime that already owns every task the verb will interact with.
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

use super::wire::*;
use super::OwnerPhase;
use crate::cluster_wire::{RpcAsyncService, RpcRequest, RpcResponse};
use crate::error::SqueezefsError;
use crate::meta_backend::{Metadata, RoutedMetaBackend};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// The runtime that owns the backend's tasks — see the module docs for
    /// why the hop is deliberate.
    runtime: tokio::runtime::Handle,
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
    /// A `Weak` to itself, so the batch handoff can hand an OWNED handle
    /// to the backend's runtime without the trait impl having to be on
    /// `Arc<Self>` (the `KvMetaBackend::conveyor_self` precedent).
    self_ref: std::sync::OnceLock<std::sync::Weak<MetaShipService>>,
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
    /// member).
    ///
    /// `runtime` is where shipped verbs execute — pass the handle of the
    /// runtime the backend was opened on.
    pub fn new(inner: Arc<RoutedMetaBackend>, runtime: tokio::runtime::Handle) -> Arc<Self> {
        let all: Vec<usize> = (0..inner.volumes.len()).collect();
        Self::with_authority(inner, runtime, &all)
    }

    /// [`Self::new`] with an explicit authority set — the shape a node
    /// that owns a SUBSET of the set has (the multi-owner deployment S9
    /// mounts).
    pub fn with_authority(
        inner: Arc<RoutedMetaBackend>,
        runtime: tokio::runtime::Handle,
        volumes: &[usize],
    ) -> Arc<Self> {
        let mut authority = vec![false; inner.volumes.len()];
        for &v in volumes {
            if let Some(slot) = authority.get_mut(v) {
                *slot = true;
            }
        }
        let term = crate::dlm::durable_term();
        let me = Arc::new(Self {
            inner,
            runtime,
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
            self_ref: std::sync::OnceLock::new(),
        });
        let _ = me.self_ref.set(Arc::downgrade(&me));
        me
    }

    /// An owned handle to this service (the handoff's requirement).
    fn owned(&self) -> Option<Arc<Self>> {
        self.self_ref.get().and_then(|w| w.upgrade())
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
        // runtime that owns the backend's tasks; this lane awaits it.
        let t_dispatch = Instant::now();
        let Some(me) = self.owned() else {
            return self.refuse(
                req.id,
                STATUS_MALFORMED,
                "S8 owner service is shutting down — no handle to dispatch the batch on".into(),
            );
        };
        let joined = self
            .runtime
            .spawn(async move { me.run_batch(frame).await })
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
    async fn run_batch(&self, frame: MetaRequestFrame) -> Vec<MetaOpResult> {
        let mut out = Vec::with_capacity(frame.ops.len());
        for op in frame.ops {
            out.push(self.run_op(frame.client_epoch, op).await);
        }
        out
    }

    async fn run_op(&self, client_epoch: u64, op: MetaOp) -> MetaOpResult {
        // Reads are naturally idempotent, so they never consume a window
        // entry — the window is spent only where a replay would otherwise
        // double-apply or lie (a replayed create answering EEXIST, a
        // replayed unlink answering ENOENT).
        if !op.call.mutating() {
            return self.execute(op.id, &op.call).await;
        }
        let (slot, owns) = self.dedup.slot((client_epoch, op.id));
        if !owns {
            self.dedup_hits.fetch_add(1, Ordering::Relaxed);
            super::DEDUP_HITS.fetch_add(1, Ordering::Relaxed);
        }
        let id = op.id;
        let call = op.call.clone();
        slot.get_or_init(|| async { self.execute(id, &call).await })
            .await
            .clone()
    }

    async fn execute(&self, id: u64, call: &MetaCall) -> MetaOpResult {
        let t = Instant::now();
        let out = self.execute_inner(call).await;
        super::owner_phase_record(OwnerPhase::Execute, t);
        self.served.fetch_add(1, Ordering::Relaxed);
        super::SERVED_VERBS.fetch_add(1, Ordering::Relaxed);
        match out {
            Ok(reply) => {
                let ino = self.grant_object(call, &reply);
                MetaOpResult {
                    id,
                    outcome: Ok(reply),
                    grant: ino.map(|ino| TokenGrant {
                        ino,
                        token: owner_authority_token(ino),
                        term: self.term(),
                    }),
                }
            }
            Err(e) => MetaOpResult {
                id,
                outcome: Err(WireError::from_error(&e)),
                grant: None,
            },
        }
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
