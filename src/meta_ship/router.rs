//! The **client side** of S8: decide locally, ship what is foreign, and
//! pipeline it (spec §6.7 decisions 1/3, §6.9 S8, risk **R1** accepted as
//! ruling **D10**).
//!
//! # The routing decision, and why it is free
//!
//! A verb's target volume is already determined: `route_ino` is one
//! `ArcSwap::load` plus an array index over the durable slot map — the
//! same lookup every metadata op performs today. Ownership is one relaxed
//! load ([`super::ownership_armed`]) and, when armed, one more arc-swap
//! load plus a `Vec` index. There is no lock anywhere on the decision, no
//! allocation, and an **unarmed** mount stops after the relaxed load and
//! calls the inner backend directly — today's path, byte for byte.
//!
//! # Pipelining: the unit is the BATCH
//!
//! Per owner there is one submission conveyor and one drain task (the M7
//! commit conveyor's shape, one layer up): callers enqueue `{ops, oneshot}`
//! and park; the drain takes **everything queued** and ships it as ONE
//! frame. So:
//!
//! * a **concurrent** stream amortizes: N verbs in flight to one owner
//!   cost one round trip, and `batched_verbs / batches` is the live
//!   coalesce factor on the stats inode;
//! * a **serial** stream (`tar -x`, `make`, `rsync`) pays one RTT per verb
//!   and the conveyor never *adds* delay waiting for a batch to fill. That
//!   cost is spec §6.10 **R1**, accepted verbatim by ruling **D10**: the
//!   serial regression is published rather than designed around, and S10's
//!   subtree delegation is the recovery.
//!
//! **Ordering and causality.** Ops inside one frame execute in submission
//! order on the owner, so a batch is a totally ordered unit: a create and
//! a lookup of the same name submitted in that order in one batch see each
//! other. Across batches, a caller's own await chain is the order (a task
//! has at most one verb in flight because it awaits its reply), and two
//! tasks racing the same object are serialized by the owner's 4a
//! `D{parent:name}` / `I{ino}` guards exactly as two local tasks are
//! today. Nothing about concurrency-visible ordering changes: the shipped
//! layer adds no new interleaving that a local mount does not already
//! have.
//!
//! # Retry, and why it is safe
//!
//! A session that dies mid-batch is reconnected and the batch is **resent
//! with the same request ids**, which is precisely what makes the owner's
//! dedup window meaningful: the retry either finds the original outcome or
//! executes for the first time. Exactly-once holds within an era; across
//! an owner failover the window is gone (it is RAM) and the client sees
//! at-least-once with durable state as the tiebreak — the same guarantee
//! NFSv4's non-persistent reply cache gives. Making that exactly-once
//! needs a DURABLE reply cache, which is S3.5's intent-record machinery;
//! this stage does not fake it.

use super::owners::{self, PeerOwner};
use super::tokens;
use super::wire::*;
use super::ShipPhase;
use crate::error::{Result, SqueezefsError};
use crate::meta_backend::{DirEntry, Ino, Inode, Metadata, RoutedMetaBackend};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Absolute override for the derived per-frame batch cap.
pub const BATCH_MAX_ENV: &str = "SQUEEZEFS_META_SHIP_BATCH_MAX";

/// **Test seam** (the `TEST_LAYOUT_MERGE_HOLD_MS` precedent): milliseconds
/// a drain iteration waits at its head before taking the queue, so
/// concurrent arrivals accumulate deterministically and the coalescing
/// contract is testable without using a sleep as coordination. `0` = off,
/// one relaxed load per drain iteration.
pub static TEST_SHIP_DRAIN_HOLD_MS: AtomicU64 = AtomicU64::new(0);

/// The per-frame batch cap.
///
/// Derivation (caps derive from system resources): the same shape the M7
/// conveyor's batch cap uses — `max(64, cpus × 2)` — because the ops in a
/// frame become that many transactions on the owner's conveyor, so sizing
/// a frame past what the owner can drain in one pass buys queueing, not
/// throughput. Floored at 64 (the shipped M7 posture) and ceilinged so a
/// frame stays inside the wire's CONTROL class.
pub fn batch_max() -> usize {
    if let Some(explicit) = crate::env_knobs::opt_int_knob::<usize>(BATCH_MAX_ENV) {
        return explicit.clamp(1, 4096);
    }
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    (cpus * 2).clamp(64, 4096)
}

/// Where a verb executes.
#[derive(Debug, Clone)]
pub enum VerbRoute {
    /// This node owns every volume the verb names: today's path.
    Local,
    /// The verb travels to this owner.
    Ship(Arc<PeerOwner>),
}

/// One submission on a lane: a batch of ops plus the parking spot for
/// their results.
struct Submission {
    ops: Vec<MetaOp>,
    owner_term: u64,
    reply: squeezefs_ipc::sqz_channel::oneshot::Sender<Result<Vec<MetaOpResult>>>,
    queued_at: Instant,
}

/// One owner's lane: the bounded submission queue plus the era this client
/// has learned for that owner.
struct ShipLane {
    peer: Arc<PeerOwner>,
    tx: squeezefs_ipc::sqz_channel::mpsc::Sender<Submission>,
    /// The owner's era, as learned from its replies. `0` = not yet known
    /// (the first frame asks).
    term: AtomicU64,
}

/// The client-side shipping router: a `Metadata` implementation that
/// delegates locally-owned verbs to the inner backend untouched and ships
/// the rest.
pub struct MetaShipRouter {
    inner: Arc<RoutedMetaBackend>,
    /// This client's identity on the wire (its cluster peer id).
    peer_id: Arc<str>,
    /// The storage-trust `job:enroll` secret both ends prove possession of.
    secret: Arc<Vec<u8>>,
    /// This client's incarnation — half of the owner's dedup key, so a
    /// restarted client's reused ids can never alias its old ones.
    client_epoch: u64,
    next_id: AtomicU64,
    lanes: scc::HashMap<String, Arc<ShipLane>>,
}

impl std::fmt::Debug for MetaShipRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetaShipRouter")
            .field("peer_id", &self.peer_id)
            .field("client_epoch", &self.client_epoch)
            .field("lanes", &self.lanes.len())
            .finish_non_exhaustive()
    }
}

impl MetaShipRouter {
    /// A router over `inner`, identifying itself as `peer_id` and proving
    /// storage membership with `secret` (the `job:enroll` value).
    pub fn new(inner: Arc<RoutedMetaBackend>, peer_id: &str, secret: Vec<u8>) -> Arc<Self> {
        let (epoch, _) = uuid::Uuid::new_v4().as_u64_pair();
        Arc::new(Self {
            inner,
            peer_id: Arc::from(peer_id),
            secret: Arc::new(secret),
            client_epoch: epoch,
            next_id: AtomicU64::new(1),
            lanes: scc::HashMap::new(),
        })
    }

    /// The backend underneath (the local path's target, and the surface
    /// the non-trait capability methods still come from — see
    /// `wire.rs`'s census for why those do not ship yet).
    pub fn inner(&self) -> &Arc<RoutedMetaBackend> {
        &self.inner
    }

    /// The next idempotency id for this client epoch.
    pub fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// This client's incarnation.
    pub fn client_epoch(&self) -> u64 {
        self.client_epoch
    }

    /// The owner of `ino`'s volume, or `None` when it is local.
    pub fn owner_for_ino(&self, ino: Ino) -> Option<Arc<PeerOwner>> {
        if !owners::ownership_armed() {
            return None;
        }
        let (v_idx, _) = self.inner.route_ino(ino);
        owners::owner_of_volume(v_idx)
    }

    /// The era this client has learned for `endpoint` (`None` = no reply
    /// seen yet).
    pub fn owner_term(&self, endpoint: &str) -> Option<u64> {
        self.lanes
            .read_sync(endpoint, |_, lane| lane.term.load(Ordering::Acquire))
    }

    /// **The routing decision** — lock-free, allocation-free, and one
    /// relaxed load on an unarmed mount.
    ///
    /// Refuses loud when the verb's named participants live on volumes
    /// owned by DIFFERENT nodes: that shape needs S3.5's cross-volume
    /// transaction machinery (ruling D4), which is not built.
    pub fn route_verb(&self, call: &MetaCall) -> Result<VerbRoute> {
        let t = Instant::now();
        let route = self.route_verb_inner(call);
        super::phase_record(ShipPhase::Route, t);
        route
    }

    fn route_verb_inner(&self, call: &MetaCall) -> Result<VerbRoute> {
        if !owners::ownership_armed() {
            return Ok(VerbRoute::Local);
        }
        let mut owner: Option<Arc<PeerOwner>> = None;
        let mut have_local = false;
        for ino in call.named_inos() {
            let (v_idx, _) = self.inner.route_ino(ino);
            match owners::owner_of_volume(v_idx) {
                None => {
                    if let Some(peer) = &owner {
                        return Err(super::cross_owner_refusal(
                            call.verb(),
                            ino,
                            &format!(
                                "this node owns one participant's volume and {} owns another's",
                                peer.peer_id
                            ),
                        ));
                    }
                    have_local = true;
                }
                Some(peer) => {
                    if have_local {
                        return Err(super::cross_owner_refusal(
                            call.verb(),
                            ino,
                            &format!(
                                "{} owns this participant's volume and this node owns another's",
                                peer.peer_id
                            ),
                        ));
                    }
                    match &owner {
                        None => owner = Some(peer),
                        Some(first) if first.endpoint == peer.endpoint => {}
                        Some(first) => {
                            return Err(super::cross_owner_refusal(
                                call.verb(),
                                ino,
                                &format!(
                                    "participants are owned by two different nodes ({} and {})",
                                    first.peer_id, peer.peer_id
                                ),
                            ));
                        }
                    }
                }
            }
        }
        Ok(match owner {
            None => VerbRoute::Local,
            Some(peer) => VerbRoute::Ship(peer),
        })
    }

    /// Ship one call and unwrap its single result.
    async fn ship_one(&self, peer: &Arc<PeerOwner>, call: MetaCall) -> Result<MetaReply> {
        let op = MetaOp {
            id: self.next_request_id(),
            call,
        };
        let mut results = self.ship_ops(peer, vec![op]).await?;
        let result = results.pop().ok_or_else(|| {
            SqueezefsError::InvalidOperation(
                "S8: the owner returned an empty result set for a one-op batch".into(),
            )
        })?;
        match result.outcome {
            Ok(reply) => Ok(reply),
            Err(e) => Err(e.into_error()),
        }
    }

    /// Ship a batch of ops to `peer` **as one frame**, using the era this
    /// client has learned for it.
    pub async fn ship_ops(
        &self,
        peer: &Arc<PeerOwner>,
        ops: Vec<MetaOp>,
    ) -> Result<Vec<MetaOpResult>> {
        let term = self.owner_term(&peer.endpoint).unwrap_or(0);
        self.ship_ops_with_term(peer, ops, term).await
    }

    /// [`Self::ship_ops`] naming the era explicitly — the shape a client
    /// that believes a stale era produces, which is exactly what the
    /// owner's era gate must refuse.
    pub async fn ship_ops_with_term(
        &self,
        peer: &Arc<PeerOwner>,
        ops: Vec<MetaOp>,
        owner_term: u64,
    ) -> Result<Vec<MetaOpResult>> {
        let count = ops.len() as u64;
        let lane = self.lane(peer)?;
        let (tx, rx) = squeezefs_ipc::sqz_channel::oneshot::channel();
        lane.tx
            .send(Submission {
                ops,
                owner_term,
                reply: tx,
                queued_at: Instant::now(),
            })
            .await
            .map_err(|_| {
                SqueezefsError::InvalidOperation(format!(
                    "S8: the shipping lane to {} is gone",
                    peer.endpoint
                ))
            })?;
        let out = rx.await.map_err(|_| {
            SqueezefsError::InvalidOperation(format!(
                "S8: the shipping lane to {} dropped a batch's results",
                peer.endpoint
            ))
        })??;
        super::SHIPPED_VERBS.fetch_add(count, Ordering::Relaxed);
        for result in &out {
            if let Some(grant) = &result.grant {
                tokens::record_grant(grant);
            }
        }
        Ok(out)
    }

    /// Re-earn a foreign object's fencing token **through a metadata
    /// RPC** — spec §6.7 decision 3's intent-lock property, not a second
    /// lock protocol: the verb a caller would have issued anyway (a
    /// `getattr`) carries the grant.
    ///
    /// Returns the local answer for an object this node owns.
    pub async fn refresh_token(&self, ino: Ino) -> Result<u64> {
        match self.owner_for_ino(ino) {
            None => Ok(super::owner_authority_token(ino)),
            Some(peer) => {
                self.ship_one(&peer, MetaCall::Getattr { ino }).await?;
                Ok(tokens::foreign_fencing_token(ino))
            }
        }
    }

    /// Send a grace-window **reclaim** to `endpoint` and absorb the
    /// fresh-era grants it returns.
    pub async fn reclaim(&self, endpoint: &str, inos: &[Ino]) -> Result<Vec<TokenGrant>> {
        let peer = Arc::new(PeerOwner::new(self.peer_id.to_string(), endpoint));
        let body = encode_reclaim(&ReclaimFrame {
            schema: META_SHIP_SCHEMA,
            client_epoch: self.client_epoch,
            inos: inos.to_vec(),
        })?;
        let mut session = self.connect(&peer).await?;
        let reply = session.call(VERB_RECLAIM, body).await?;
        if reply.status != STATUS_OK {
            return Err(SqueezefsError::InvalidOperation(format!(
                "S8 reclaim refused by {endpoint} (status {}): {}",
                reply.status,
                String::from_utf8_lossy(&reply.body)
            )));
        }
        let frame = decode_reclaim_reply(&reply.body)?;
        tokens::record_owner_term(frame.owner_term);
        for grant in &frame.grants {
            tokens::record_grant(grant);
        }
        Ok(frame.grants)
    }

    /// The lane for `peer`, started on first use.
    fn lane(&self, peer: &Arc<PeerOwner>) -> Result<Arc<ShipLane>> {
        if let Some(lane) = self
            .lanes
            .read_sync(&peer.endpoint, |_, lane| Arc::clone(lane))
        {
            return Ok(lane);
        }
        // Bounded by law: batch_max × 8 submissions in flight, so a
        // saturated owner backpressures its clients instead of growing a
        // queue without limit.
        let (tx, rx) = squeezefs_ipc::sqz_channel::mpsc::channel::<Submission>(batch_max() * 8);
        let lane = Arc::new(ShipLane {
            peer: Arc::clone(peer),
            tx,
            term: AtomicU64::new(0),
        });
        match self
            .lanes
            .insert_sync(peer.endpoint.clone(), Arc::clone(&lane))
        {
            Ok(()) => {
                let drain = LaneDrain {
                    lane: Arc::clone(&lane),
                    peer_id: Arc::clone(&self.peer_id),
                    secret: Arc::clone(&self.secret),
                    client_epoch: self.client_epoch,
                };
                // The drain rides the sqz-meta pool — the venue that owns
                // the daemon's plane tasks; it ends when the router (and
                // hence the lane's sender) is dropped.
                crate::meta_exec::spawn_meta("meta_ship_lane_drain", drain.run(rx));
                Ok(lane)
            }
            Err(_) => self
                .lanes
                .read_sync(&peer.endpoint, |_, lane| Arc::clone(lane))
                .ok_or_else(|| {
                    SqueezefsError::InvalidOperation(
                        "S8: a shipping lane raced in and out".to_string(),
                    )
                }),
        }
    }

    /// Dial one authenticated session to `peer` on the S3 wire.
    async fn connect(&self, peer: &Arc<PeerOwner>) -> Result<crate::cluster_wire::RpcClient> {
        crate::cluster_wire::RpcClient::connect(&peer.endpoint, &self.secret, &self.peer_id, None)
            .await
    }
}

/// One owner lane's drain task: coalesce, ship, fan results back.
struct LaneDrain {
    lane: Arc<ShipLane>,
    peer_id: Arc<str>,
    secret: Arc<Vec<u8>>,
    client_epoch: u64,
}

impl LaneDrain {
    async fn run(self, mut rx: squeezefs_ipc::sqz_channel::mpsc::Receiver<Submission>) {
        let mut session: Option<crate::cluster_wire::RpcClient> = None;
        while let Some(first) = rx.recv().await {
            // The head-of-iteration hold is a TEST seam only (0 in
            // production, one relaxed load): the drain never adds delay to
            // a serial stream.
            let hold = TEST_SHIP_DRAIN_HOLD_MS.load(Ordering::Relaxed);
            if hold > 0 {
                squeezefs_ipc::sqz_time::sleep(Duration::from_millis(hold)).await;
            }
            let mut batch = vec![first];
            let cap = batch_max();
            let mut verbs = batch[0].ops.len();
            while verbs < cap {
                match rx.try_recv() {
                    Ok(next) => {
                        verbs += next.ops.len();
                        batch.push(next);
                    }
                    Err(_) => break,
                }
            }
            self.ship_batch(&mut session, batch).await;
        }
    }

    /// Ship one coalesced frame and fan the results back to each waiter.
    async fn ship_batch(
        &self,
        session: &mut Option<crate::cluster_wire::RpcClient>,
        batch: Vec<Submission>,
    ) {
        // The era: the highest any waiter named (a waiter naming an
        // explicitly stale era gets its own refusal — the frame is
        // refused whole, which is the contract).
        let owner_term = batch.iter().map(|s| s.owner_term).max().unwrap_or(0);
        let mut ops: Vec<MetaOp> = Vec::new();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for sub in &batch {
            let start = ops.len();
            ops.extend(sub.ops.iter().cloned());
            spans.push((start, ops.len()));
            super::phase_record(ShipPhase::QueueWait, sub.queued_at);
        }
        super::BATCHES.fetch_add(1, Ordering::Relaxed);
        super::BATCHED_VERBS.fetch_add(ops.len() as u64, Ordering::Relaxed);

        let outcome = self.exchange(session, owner_term, ops).await;
        for (sub, (start, end)) in batch.into_iter().zip(spans) {
            let slice = match &outcome {
                Ok(results) => Ok(results[start..end].to_vec()),
                Err(e) => Err(SqueezefsError::InvalidOperation(format!("{e}"))),
            };
            let _ = sub.reply.send(slice);
        }
    }

    /// Encode, send, decode — reconnecting once and **resending the same
    /// request ids** on a transport failure, which is what makes the
    /// owner's dedup window the idempotency mechanism rather than a hope.
    async fn exchange(
        &self,
        session: &mut Option<crate::cluster_wire::RpcClient>,
        owner_term: u64,
        ops: Vec<MetaOp>,
    ) -> Result<Vec<MetaOpResult>> {
        let t_encode = Instant::now();
        let body = encode_request(&MetaRequestFrame {
            schema: META_SHIP_SCHEMA,
            client_epoch: self.client_epoch,
            owner_term,
            ops,
        })?;
        super::phase_record(ShipPhase::Encode, t_encode);

        let mut last: Option<SqueezefsError> = None;
        for attempt in 0..2 {
            if session.is_none() {
                match crate::cluster_wire::RpcClient::connect(
                    &self.lane.peer.endpoint,
                    &self.secret,
                    &self.peer_id,
                    None,
                )
                .await
                {
                    Ok(c) => *session = Some(c),
                    Err(e) => {
                        last = Some(e);
                        continue;
                    }
                }
            }
            if attempt > 0 {
                super::RETRIES.fetch_add(1, Ordering::Relaxed);
            }
            let t_rtt = Instant::now();
            let call = session
                .as_mut()
                .expect("connected above")
                .call(VERB_META_BATCH, body.clone())
                .await;
            super::phase_record(ShipPhase::Rtt, t_rtt);
            super::DLM_RPCS_META.fetch_add(1, Ordering::Relaxed);
            match call {
                Ok(reply) => return self.interpret(reply),
                Err(e) => {
                    // A dead session: drop it and retry ONCE with the
                    // same ids.
                    log::warn!(
                        "S8: batch to {} failed ({e}) — reconnecting and resending the same \
                         request ids (the owner's dedup window makes the resend exactly-once)",
                        self.lane.peer.endpoint
                    );
                    *session = None;
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "S8: no session to {} and no error to report",
                self.lane.peer.endpoint
            ))
        }))
    }

    /// Turn a frame-level reply into results, learning the owner's era and
    /// mapping every frame-level refusal to a loud, named error.
    fn interpret(&self, reply: crate::cluster_wire::RpcResponse) -> Result<Vec<MetaOpResult>> {
        let t = Instant::now();
        let out = match reply.status {
            STATUS_OK => {
                let frame = decode_reply(&reply.body)?;
                if frame.schema != META_SHIP_SCHEMA {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "S8: owner {} replied in vocabulary schema {} (this build speaks \
                         {META_SHIP_SCHEMA})",
                        self.lane.peer.endpoint, frame.schema
                    )));
                }
                self.learn_term(frame.owner_term);
                Ok(frame.results)
            }
            STATUS_STALE_TERM => {
                // Learn the new era so the caller's retry is admissible,
                // then refuse THIS attempt loud: the frame was refused
                // whole, so nothing was applied.
                if let Ok(frame) = decode_reply(&reply.body) {
                    self.learn_term(frame.owner_term);
                }
                // The client-observed face of the refusal is its OWN
                // counter: `stale_term_refusals` counts refusals this node
                // ISSUED as an owner, so a node that is both would
                // otherwise double-count one event.
                super::ERA_RELEARNS.fetch_add(1, Ordering::Relaxed);
                Err(SqueezefsError::InvalidOperation(format!(
                    "S8: owner {} refused the batch — it named a stale writer era; the owner is \
                     now in era {} (a successor bumps `term` durably before arming, so every \
                     old-era request is stale by construction). Nothing was applied.",
                    self.lane.peer.endpoint,
                    self.lane.term.load(Ordering::Acquire)
                )))
            }
            STATUS_IN_GRACE => Err(SqueezefsError::busy(format!(
                "S8: owner {} is inside its failover grace window and refuses fresh mutations \
                 ({}) — reclaim first, then retry (spec §6.7 Recovery)",
                self.lane.peer.endpoint,
                String::from_utf8_lossy(&reply.body)
            ))),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "S8: owner {} refused the batch with status {other}: {}",
                self.lane.peer.endpoint,
                String::from_utf8_lossy(&reply.body)
            ))),
        };
        super::phase_record(ShipPhase::Decode, t);
        out
    }

    fn learn_term(&self, term: u64) {
        self.lane.term.fetch_max(term, Ordering::AcqRel);
        tokens::record_owner_term(term);
    }
}

/// Unwrap a reply the caller expects to be an inode.
fn expect_inode(reply: MetaReply, verb: MetaVerb) -> Result<Inode> {
    match reply {
        MetaReply::Inode(i) => Ok(Inode::from(i)),
        other => Err(super::protocol_error(
            verb,
            &format!("{other:?}"),
            "an inode",
        )),
    }
}

fn expect_ino(reply: MetaReply, verb: MetaVerb) -> Result<Ino> {
    match reply {
        MetaReply::Ino(ino) => Ok(ino),
        other => Err(super::protocol_error(verb, &format!("{other:?}"), "an ino")),
    }
}

fn expect_unit(reply: MetaReply, verb: MetaVerb) -> Result<()> {
    match reply {
        MetaReply::Unit => Ok(()),
        other => Err(super::protocol_error(verb, &format!("{other:?}"), "unit")),
    }
}

/// The local-path accounting every locally-routed verb performs: one
/// relaxed increment, so the shipped-vs-local ledger is complete.
fn note_local() {
    super::LOCAL_VERBS.fetch_add(1, Ordering::Relaxed);
}

#[async_trait::async_trait]
impl Metadata for MetaShipRouter {
    /// `lookup` = `LookupDentry` ⊕ `getattr`, each independently routed,
    /// because the child's inode can live on a volume another node owns
    /// (see `wire.rs`). Unarmed, it is the inner backend's own `lookup`,
    /// untouched.
    async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode> {
        if !owners::ownership_armed() {
            note_local();
            return self.inner.lookup(parent, name).await;
        }
        // "..": the reverse-dentry walk needs the whole set and is a
        // cold, rare reconnect path — the inner backend performs it over
        // the volumes it can read, and a foreign parent's answer is not
        // expressible until S10's placement work makes the walk routable.
        if name == ".." {
            note_local();
            return self.inner.lookup(parent, name).await;
        }
        let call = MetaCall::LookupDentry {
            parent,
            name: name.to_string(),
        };
        let child = match self.route_verb(&call)? {
            VerbRoute::Local => {
                note_local();
                match self.inner.lookup_dentry(parent, name).await? {
                    Some((child, _)) => child,
                    None => {
                        return Err(SqueezefsError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("Dentry {name} not found in parent {parent}"),
                        )))
                    }
                }
            }
            // The owner resolves the child itself when it holds the
            // child's volume too — the common shape, and the reason a
            // shipped lookup is ONE round trip rather than two. An
            // ino-only answer means the child is owned elsewhere, so the
            // getattr routes on its own below.
            VerbRoute::Ship(peer) => match self.ship_one(&peer, call).await? {
                MetaReply::Inode(inode) => return Ok(Inode::from(inode)),
                MetaReply::Ino(child) => child,
                other => {
                    return Err(super::protocol_error(
                        MetaVerb::LookupDentry,
                        &format!("{other:?}"),
                        "an inode or a child ino",
                    ))
                }
            },
        };
        self.getattr(child).await
    }

    async fn create_with_rdev(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
    ) -> Result<Inode> {
        let call = MetaCall::CreateWithRdev {
            parent,
            name: name.to_string(),
            mode,
            uid,
            gid,
            rdev,
        };
        match self.route_verb(&call)? {
            VerbRoute::Local => {
                note_local();
                self.inner
                    .create_with_rdev(parent, name, mode, uid, gid, rdev)
                    .await
            }
            VerbRoute::Ship(peer) => {
                expect_inode(self.ship_one(&peer, call).await?, MetaVerb::CreateWithRdev)
            }
        }
    }

    async fn unlink(&self, parent: Ino, name: &str) -> Result<Ino> {
        let call = MetaCall::Unlink {
            parent,
            name: name.to_string(),
        };
        match self.route_verb(&call)? {
            VerbRoute::Local => {
                note_local();
                self.inner.unlink(parent, name).await
            }
            VerbRoute::Ship(peer) => {
                expect_ino(self.ship_one(&peer, call).await?, MetaVerb::Unlink)
            }
        }
    }

    async fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Inode> {
        let call = MetaCall::Link {
            ino,
            new_parent,
            new_name: new_name.to_string(),
        };
        match self.route_verb(&call)? {
            VerbRoute::Local => {
                note_local();
                self.inner.link(ino, new_parent, new_name).await
            }
            VerbRoute::Ship(peer) => {
                expect_inode(self.ship_one(&peer, call).await?, MetaVerb::Link)
            }
        }
    }

    async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
        flags: u32,
    ) -> Result<()> {
        let call = MetaCall::Rename {
            old_parent,
            old_name: old_name.to_string(),
            new_parent,
            new_name: new_name.to_string(),
            flags,
        };
        match self.route_verb(&call)? {
            VerbRoute::Local => {
                note_local();
                self.inner
                    .rename(old_parent, old_name, new_parent, new_name, flags)
                    .await
            }
            VerbRoute::Ship(peer) => {
                expect_unit(self.ship_one(&peer, call).await?, MetaVerb::Rename)
            }
        }
    }

    async fn readdir(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<DirEntry>> {
        let call = MetaCall::Readdir {
            dir,
            offset,
            max: max.min(u32::MAX as usize) as u32,
        };
        match self.route_verb(&call)? {
            VerbRoute::Local => {
                note_local();
                self.inner.readdir(dir, offset, max).await
            }
            VerbRoute::Ship(peer) => match self.ship_one(&peer, call).await? {
                MetaReply::Dir(entries) => Ok(entries.into_iter().map(DirEntry::from).collect()),
                other => Err(super::protocol_error(
                    MetaVerb::Readdir,
                    &format!("{other:?}"),
                    "a directory page",
                )),
            },
        }
    }

    async fn getattr(&self, ino: Ino) -> Result<Inode> {
        let call = MetaCall::Getattr { ino };
        match self.route_verb(&call)? {
            VerbRoute::Local => {
                note_local();
                self.inner.getattr(ino).await
            }
            VerbRoute::Ship(peer) => {
                expect_inode(self.ship_one(&peer, call).await?, MetaVerb::Getattr)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn setattr(
        &self,
        ino: Ino,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<u64>,
        mtime: Option<u64>,
        ctime: Option<u64>,
    ) -> Result<Inode> {
        let call = MetaCall::Setattr {
            ino,
            mode,
            uid,
            gid,
            size,
            atime,
            mtime,
            ctime,
        };
        match self.route_verb(&call)? {
            VerbRoute::Local => {
                note_local();
                self.inner
                    .setattr(ino, mode, uid, gid, size, atime, mtime, ctime)
                    .await
            }
            VerbRoute::Ship(peer) => {
                expect_inode(self.ship_one(&peer, call).await?, MetaVerb::Setattr)
            }
        }
    }

    async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>> {
        let call = MetaCall::Getxattr {
            ino,
            name: name.to_string(),
        };
        match self.route_verb(&call)? {
            VerbRoute::Local => {
                note_local();
                self.inner.getxattr(ino, name).await
            }
            VerbRoute::Ship(peer) => match self.ship_one(&peer, call).await? {
                MetaReply::Xattr(value) => Ok(value),
                other => Err(super::protocol_error(
                    MetaVerb::Getxattr,
                    &format!("{other:?}"),
                    "an xattr value",
                )),
            },
        }
    }

    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        let call = MetaCall::Setxattr {
            ino,
            name: name.to_string(),
            value: value.to_vec(),
        };
        match self.route_verb(&call)? {
            VerbRoute::Local => {
                note_local();
                self.inner.setxattr(ino, name, value).await
            }
            VerbRoute::Ship(peer) => {
                expect_unit(self.ship_one(&peer, call).await?, MetaVerb::Setxattr)
            }
        }
    }

    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        let call = MetaCall::Removexattr {
            ino,
            name: name.to_string(),
        };
        match self.route_verb(&call)? {
            VerbRoute::Local => {
                note_local();
                self.inner.removexattr(ino, name).await
            }
            VerbRoute::Ship(peer) => {
                expect_unit(self.ship_one(&peer, call).await?, MetaVerb::Removexattr)
            }
        }
    }

    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        let call = MetaCall::Listxattr { ino };
        match self.route_verb(&call)? {
            VerbRoute::Local => {
                note_local();
                self.inner.listxattr(ino).await
            }
            VerbRoute::Ship(peer) => match self.ship_one(&peer, call).await? {
                MetaReply::Names(names) => Ok(names),
                other => Err(super::protocol_error(
                    MetaVerb::Listxattr,
                    &format!("{other:?}"),
                    "an xattr name list",
                )),
            },
        }
    }

    async fn destroy_inode(&self, ino: Ino) -> Result<()> {
        let call = MetaCall::DestroyInode { ino };
        match self.route_verb(&call)? {
            VerbRoute::Local => {
                note_local();
                self.inner.destroy_inode(ino).await
            }
            VerbRoute::Ship(peer) => {
                expect_unit(self.ship_one(&peer, call).await?, MetaVerb::DestroyInode)
            }
        }
    }
}
