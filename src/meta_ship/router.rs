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
//!
//! [`super::ownership_armed`]: crate::meta_ship::ownership_armed

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
/// frame stays inside the wire's CONTROL class. `cpus` is the
/// fleet-share-DIVIDED sizing root (KD-MW-14 rung 3c — the retired
/// direct `available_parallelism()` read bypassed the divisor and was
/// calling-thread-mask exposed).
pub fn batch_max() -> usize {
    batch_max_from(
        crate::env_knobs::opt_int_knob::<usize>(BATCH_MAX_ENV),
        crate::cpu::process_parallelism(),
    )
}

/// Pure form (tie-tested in the derivation sweep): explicit wins
/// verbatim within its admissible range; derived = `(cpus × 2).clamp(64,
/// 4096)` — floor 64 = the shipped M7 posture.
pub fn batch_max_from(explicit: Option<usize>, cpus: usize) -> usize {
    if let Some(explicit) = explicit {
        return explicit.clamp(1, 4096);
    }
    cpus.saturating_mul(2).clamp(64, 4096)
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
    /// Owners with a running recall channel (rung 12): one standing
    /// DelegRecall round per owner, spawned lazily on the first grant
    /// absorbed from it.
    deleg_channels: scc::HashMap<String, ()>,
    /// A `Weak` to self for the channel tasks (they end when the router
    /// is dropped — structured lifetime without a join registry).
    self_ref: std::sync::OnceLock<std::sync::Weak<MetaShipRouter>>,
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
        let me = Arc::new(Self {
            inner,
            peer_id: Arc::from(peer_id),
            secret: Arc::new(secret),
            client_epoch: epoch,
            next_id: AtomicU64::new(1),
            lanes: scc::HashMap::new(),
            deleg_channels: scc::HashMap::new(),
            self_ref: std::sync::OnceLock::new(),
        });
        let _ = me.self_ref.set(Arc::downgrade(&me));
        me
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

    /// This client's wire identity — what an owner sees as the frame's
    /// `client_id`.
    pub fn peer_id(&self) -> &str {
        &self.peer_id
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
            // §5.10: a POISONED entry answers neither "local" nor "ship
            // there" — both would be a guess about who may append — so
            // the verb refuses loud here.
            match owners::route_volume(v_idx)? {
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

    /// This router's wire identity for the intent lane (rung 13).
    pub(crate) fn intent_ctx(&self) -> super::intents::LaneCtx {
        super::intents::LaneCtx {
            peer_id: Arc::clone(&self.peer_id),
            secret: Arc::clone(&self.secret),
            client_epoch: self.client_epoch,
        }
    }

    /// The ordering barrier (rung 13): a SHIPPED verb naming pending
    /// intent state (a pending ino, a pending name, a dir with pending
    /// ops) flushes first — owner-side application order always respects
    /// local causality. One relaxed load when nothing is pending.
    async fn intent_barrier(&self, inos: &[u64], pairs: &[(u64, &str)]) -> Result<()> {
        if !super::intents::barrier_needed(inos, pairs) {
            return Ok(());
        }
        super::intents::flush_all(true).await.map_err(|errno| {
            SqueezefsError::refused(
                errno,
                "S10 intents: the ordering barrier's flush failed — refusing the shipped verb \
                 rather than letting it overtake the un-applied intents it names"
                    .to_string(),
            )
        })
    }

    /// [`Self::intent_barrier`] for one `MetaCall` (the generic mutating
    /// ship arm's form).
    async fn intent_barrier_call(&self, call: &MetaCall) -> Result<()> {
        let inos = call.named_inos();
        match call {
            MetaCall::LookupDentry { parent, name }
            | MetaCall::CreateWithRdev { parent, name, .. }
            | MetaCall::Unlink { parent, name } => {
                self.intent_barrier(&inos, &[(*parent, name.as_str())])
                    .await
            }
            MetaCall::Rename {
                old_parent,
                old_name,
                new_parent,
                new_name,
                ..
            } => {
                self.intent_barrier(
                    &inos,
                    &[
                        (*old_parent, old_name.as_str()),
                        (*new_parent, new_name.as_str()),
                    ],
                )
                .await
            }
            MetaCall::Link { new_parent, .. } => {
                self.intent_barrier(&inos, &[(*new_parent, "")]).await
            }
            _ => self.intent_barrier(&inos, &[]).await,
        }
    }

    /// Absorb one result's delegation payloads (rung 12): reply-ridden
    /// revocations FIRST (they must land before the caller's await
    /// returns — read-your-own-writes), then the piggybacked grants, then
    /// make sure the recall channel to this owner is running (the grants
    /// are serveable only while it is).
    async fn absorb_delegation(&self, peer: &Arc<PeerOwner>, result: &MetaOpResult) {
        if !result.revokes.is_empty() {
            let fence_term = self.owner_term(&peer.endpoint).unwrap_or(0);
            tokens::revoke_delegations(
                &peer.endpoint,
                &result.revokes,
                fence_term,
                result.revoke_fence,
                tokens::RevokeKind::Reply,
            )
            .await;
        }
        // Rung 13: the piggybacked EXCLUSIVE UPDATE grant (census + ino
        // supply) installs into the intent lane, and the recall channel
        // must run for it (exclusivity is only enforceable while a recall
        // can reach us — the mint gate checks channel freshness).
        if let Some(g) = &result.intent_grant {
            if super::intents::update_intents_enabled() {
                super::intents::absorb_intent_grant(&peer.endpoint, self.intent_ctx(), g);
                self.ensure_recall_channel(peer);
            }
        }
        if result.delegs.is_empty() || !tokens::delegation_enabled() {
            return;
        }
        for g in &result.delegs {
            tokens::install_delegation(&peer.endpoint, g);
        }
        self.ensure_recall_channel(peer);
    }

    /// Spawn the standing recall channel to `peer` once (rung 12): the
    /// client's call IS the owner→holder push path on this dial-only wire
    /// — the owner parks it and answers the instant a recall is enqueued.
    /// The channel record is primed healthy-from-spawn so the grants a
    /// reply just installed can serve during the first round's flight; a
    /// failed connect marks it down (fail-closed) immediately.
    pub(crate) fn ensure_recall_channel(&self, peer: &Arc<PeerOwner>) {
        if self
            .deleg_channels
            .read_sync(&peer.endpoint, |_, _| ())
            .is_some()
        {
            return;
        }
        if self
            .deleg_channels
            .insert_sync(peer.endpoint.clone(), ())
            .is_err()
        {
            return; // raced another absorb — one channel per owner
        }
        let _ = tokens::deleg_channel(&peer.endpoint);
        let weak = self.self_ref.get().cloned().expect("set at construction");
        let peer = Arc::clone(peer);
        let peer_id = Arc::clone(&self.peer_id);
        let secret = Arc::clone(&self.secret);
        let epoch = self.client_epoch;
        crate::meta_exec::spawn_meta(
            "meta_ship_deleg_channel",
            deleg_channel_run(weak, peer, peer_id, secret, epoch),
        );
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
            self.absorb_delegation(peer, result).await;
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
            client_id: self.peer_id.to_string(),
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
                // §5.10's runtime conjunction: a relearn means this owner
                // failed over, so re-derive its volumes from a FRESH read
                // — an adoption by a declared successor is followed, a
                // holder the assignment set does not name POISONS.
                owners::note_era_relearn(&self.lane.peer.peer_id);
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

/// The standing recall channel to one owner (rung 12 — the DelegRecall
/// verb's client half): connect → RE-ASSERT everything held/suspended →
/// poll rounds forever. Every transport failure SUSPENDS the owner's
/// delegations first (fail-closed: serves stop the instant the channel is
/// not known-good) and re-asserts them on reconnect (KD-MW-5's NFSv4
/// reconstruction). A `STATUS_DELEG_FENCED` answer is terminal: the
/// holder's grants died with a recall deadline; re-admission is by
/// remount.
async fn deleg_channel_run(
    router: std::sync::Weak<MetaShipRouter>,
    peer: Arc<PeerOwner>,
    peer_id: Arc<str>,
    secret: Arc<Vec<u8>>,
    client_epoch: u64,
) {
    let ep = peer.endpoint.clone();
    let mut pending_acks: Vec<u64> = Vec::new();
    let mut reassert: Vec<u64> = Vec::new();
    let mut session: Option<crate::cluster_wire::RpcClient> = None;
    let mut backoff = Duration::from_millis(50);
    loop {
        if router.upgrade().is_none() || !tokens::delegation_enabled() {
            tokens::deleg_channel_mark_down(&ep);
            return;
        }
        if session.is_none() {
            match crate::cluster_wire::RpcClient::connect(&ep, &secret, &peer_id, None).await {
                Ok(c) => {
                    session = Some(c);
                    backoff = Duration::from_millis(50);
                    // Re-assert: still-held entries + everything a
                    // suspension parked. Un-reasserted = gone.
                    let mut inos = tokens::held_delegation_inos(&ep);
                    inos.append(&mut reassert);
                    inos.sort_unstable();
                    inos.dedup();
                    if !inos.is_empty()
                        && !deleg_reassert_round(&mut session, &ep, &peer_id, client_epoch, &inos)
                            .await
                    {
                        // Fenced: terminal for this channel.
                        let _ = tokens::suspend_owner_delegations(&ep);
                        return;
                    }
                }
                Err(e) => {
                    log::debug!("S10 delegation channel to {ep}: connect failed ({e}) — retrying");
                    reassert.extend(tokens::suspend_owner_delegations(&ep));
                    squeezefs_ipc::sqz_time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(1));
                    continue;
                }
            }
            if session.is_none() {
                // The re-assert round failed on transport: suspend + retry.
                reassert.extend(tokens::suspend_owner_delegations(&ep));
                squeezefs_ipc::sqz_time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(1));
                continue;
            }
        }
        // One poll round: acks out, recalls in, park at the owner between.
        let acks = std::mem::take(&mut pending_acks);
        let frame = DelegPollFrame {
            schema: META_SHIP_SCHEMA,
            client_epoch,
            client_id: peer_id.to_string(),
            acks: acks.clone(),
        };
        let body = match encode_deleg_poll(&frame) {
            Ok(b) => b,
            Err(e) => {
                log::error!("S10 delegation channel to {ep}: poll encode failed ({e}) — closing");
                let _ = tokens::suspend_owner_delegations(&ep);
                return;
            }
        };
        let outcome = session
            .as_mut()
            .expect("connected above")
            .call(VERB_DELEG_RECALL, body)
            .await;
        match outcome {
            Ok(reply) if reply.status == crate::cluster_wire::RPC_OK => {
                match decode_deleg_poll_reply(&reply.body) {
                    Ok(pr) => {
                        tokens::deleg_channel_mark_ok(&ep, pr.park_ms, pr.owner_term);
                        for f in &pr.frames {
                            // Drain-then-ack: revoke waits for in-flight
                            // serves, so the ack the NEXT round carries can
                            // never precede a serve it should have fenced.
                            tokens::revoke_delegations(
                                &ep,
                                &f.inos,
                                pr.owner_term,
                                pr.fence_seq,
                                tokens::RevokeKind::Recall,
                            )
                            .await;
                            pending_acks.push(f.frame_id);
                        }
                    }
                    Err(e) => {
                        log::warn!("S10 delegation channel to {ep}: undecodable poll reply ({e})");
                        pending_acks.extend(acks);
                        session = None;
                        reassert.extend(tokens::suspend_owner_delegations(&ep));
                    }
                }
            }
            Ok(reply) if reply.status == STATUS_DELEG_FENCED => {
                log::error!(
                    "S10 delegation: this holder is FENCED by {ep} (a recall deadline expired) — \
                     dropping every delegation from it; re-admission is by remount"
                );
                let _ = tokens::suspend_owner_delegations(&ep);
                return;
            }
            Ok(reply) => {
                log::warn!(
                    "S10 delegation channel to {ep}: poll refused (status {}) — reconnecting",
                    reply.status
                );
                pending_acks.extend(acks);
                session = None;
                reassert.extend(tokens::suspend_owner_delegations(&ep));
                squeezefs_ipc::sqz_time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(1));
            }
            Err(e) => {
                log::debug!("S10 delegation channel to {ep}: poll failed ({e}) — reconnecting");
                pending_acks.extend(acks);
                session = None;
                reassert.extend(tokens::suspend_owner_delegations(&ep));
                squeezefs_ipc::sqz_time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(1));
            }
        }
    }
}

/// One re-assertion round. `true` = proceed (grants absorbed, or a
/// transport failure the caller retries — signalled by clearing the
/// session); `false` = FENCED, terminal.
async fn deleg_reassert_round(
    session: &mut Option<crate::cluster_wire::RpcClient>,
    ep: &str,
    peer_id: &str,
    client_epoch: u64,
    inos: &[u64],
) -> bool {
    let frame = DelegReassertFrame {
        schema: META_SHIP_SCHEMA,
        client_epoch,
        client_id: peer_id.to_string(),
        inos: inos.to_vec(),
    };
    let body = match encode_deleg_reassert(&frame) {
        Ok(b) => b,
        Err(_) => {
            *session = None;
            return true;
        }
    };
    match session
        .as_mut()
        .expect("connected")
        .call(VERB_DELEG_REASSERT, body)
        .await
    {
        Ok(reply) if reply.status == crate::cluster_wire::RPC_OK => {
            match decode_deleg_reassert_reply(&reply.body) {
                Ok(rr) => {
                    // Un-reasserted = gone (NFSv4): drop what the
                    // successor did not re-admit, absorb what it did.
                    let admitted: std::collections::HashSet<u64> =
                        rr.grants.iter().map(|g| g.ino).collect();
                    let gone: Vec<u64> = inos
                        .iter()
                        .copied()
                        .filter(|i| !admitted.contains(i))
                        .collect();
                    tokens::drop_delegations(ep, &gone);
                    tokens::absorb_reassert_reply(ep, &rr);
                    true
                }
                Err(_) => {
                    *session = None;
                    true
                }
            }
        }
        Ok(reply) if reply.status == STATUS_DELEG_FENCED => false,
        Ok(_) | Err(_) => {
            *session = None;
            true
        }
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

/// A delegated `lookup`'s three outcomes.
enum DelegLookup {
    /// Not servable — ship as today.
    Miss,
    /// Served locally (parent dentry + child attrs, both delegated and
    /// view-current).
    Hit(Inode),
    /// An AUTHORITATIVE local negative: the parent's grant is live and
    /// view-current, so its dentry set is exact (the coherence law's
    /// EEXIST/ENOENT-decidable-locally argument, read side).
    Negative,
}

impl MetaShipRouter {
    /// The delegated `lookup` serve (rung 12): parent dentry resolution
    /// under the parent's grant, child attrs under the child's own — a
    /// partial cover (dentry known, child not delegated) deliberately
    /// ships the WHOLE verb, because a shipped lookup costs the same one
    /// round trip and re-earns the child's grant for next time.
    async fn deleg_lookup(&self, endpoint: &str, parent: Ino, name: &str) -> DelegLookup {
        let Some(pg) = tokens::deleg_serve_begin(parent, endpoint) else {
            return DelegLookup::Miss;
        };
        // The currency law (live finding #4): the view's covered journal
        // prefix must reach the grant's commit watermark — an aliasing
        // attr compare served a stale authoritative negative on the
        // first fleet tar -x.
        if !pg.dir() || !pg.view_current(self.inner.view_watermark_of(parent)) {
            return DelegLookup::Miss;
        }
        // `lookup_dentry`/`getattr_local`, NEVER the trait verbs: on an
        // armed co-writer a trait body consults the daemon verb router,
        // which routes right back into this serve — the live-rig
        // recursion (rung-12 finding #1, a fuse3 lane stack overflow on
        // the first fleet mount).
        match self.inner.lookup_dentry(parent, name).await {
            Ok(Some((child, _ft))) => {
                let Some(cg) = tokens::deleg_serve_begin(child, endpoint) else {
                    return DelegLookup::Miss;
                };
                if !cg.view_current(self.inner.view_watermark_of(child)) {
                    return DelegLookup::Miss;
                }
                let Ok(ci) = self.inner.getattr_local(child).await else {
                    return DelegLookup::Miss;
                };
                pg.note_hit();
                DelegLookup::Hit(ci)
            }
            Ok(None) => {
                pg.note_hit();
                DelegLookup::Negative
            }
            Err(_) => DelegLookup::Miss,
        }
    }

    /// The delegated `getattr` serve (`getattr_local` — see
    /// `deleg_lookup` on the recursion and currency laws).
    async fn deleg_getattr(&self, endpoint: &str, ino: Ino) -> Option<Inode> {
        let g = tokens::deleg_serve_begin(ino, endpoint)?;
        if !g.view_current(self.inner.view_watermark_of(ino)) {
            return None;
        }
        let i = self.inner.getattr_local(ino).await.ok()?;
        g.note_hit();
        Some(i)
    }

    /// The delegated `readdir` serve.
    async fn deleg_readdir(
        &self,
        endpoint: &str,
        dir: Ino,
        offset: u64,
        max: usize,
    ) -> Option<Vec<DirEntry>> {
        let g = tokens::deleg_serve_begin(dir, endpoint)?;
        if !g.dir() || !g.view_current(self.inner.view_watermark_of(dir)) {
            return None;
        }
        let entries = self.inner.readdir_local(dir, offset, max).await.ok()?;
        g.note_hit();
        Some(entries)
    }
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
            VerbRoute::Ship(peer) => {
                // Rung 13: the intent probes FIRST — a pending mint serves
                // its image, an UPDATE-governed census answers negatives
                // authoritatively, and census names SHIP (the reader view
                // may lag our own applies, so the rung-12 delegated serve
                // is forbidden while the authority governs the dir).
                let mut census_governs = false;
                match super::intents::lookup_probe(&peer.endpoint, parent, name) {
                    super::intents::LookupProbe::Image(inode) => return Ok(inode),
                    super::intents::LookupProbe::Negative => {
                        return Err(SqueezefsError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("Dentry {name} not found in parent {parent}"),
                        )))
                    }
                    super::intents::LookupProbe::Ship => census_governs = true,
                    super::intents::LookupProbe::None => {}
                }
                // Rung 12: the delegated serve — ZERO round trips when the
                // parent (and child) are delegated and view-current.
                if !census_governs {
                    match self.deleg_lookup(&peer.endpoint, parent, name).await {
                        DelegLookup::Hit(inode) => return Ok(inode),
                        DelegLookup::Negative => {
                            return Err(SqueezefsError::Io(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                format!("Dentry {name} not found in parent {parent}"),
                            )))
                        }
                        DelegLookup::Miss => {}
                    }
                }
                match self.ship_one(&peer, call).await? {
                    MetaReply::Inode(inode) => return Ok(Inode::from(inode)),
                    MetaReply::Ino(child) => child,
                    other => {
                        return Err(super::protocol_error(
                            MetaVerb::LookupDentry,
                            &format!("{other:?}"),
                            "an inode or a child ino",
                        ))
                    }
                }
            }
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
                // Rung 13 — the LOCAL MINT (§8.2's zero-round-trip create):
                // under a live EXCLUSIVE UPDATE grant on the parent, the
                // create acks from the local intent; the census answers
                // O_EXCL exactly; declines fall through to the shipped
                // path (which is also what EARNS the grant).
                match super::intents::try_mint_create(
                    &peer.endpoint,
                    parent,
                    name,
                    mode,
                    uid,
                    gid,
                    rdev,
                    0,
                ) {
                    super::intents::MintOutcome::Minted(inode) => return Ok(inode),
                    super::intents::MintOutcome::Exists => {
                        return Err(SqueezefsError::already_exists("File already exists"))
                    }
                    super::intents::MintOutcome::NotEligible => {}
                }
                self.intent_barrier_call(&call).await?;
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
                self.intent_barrier_call(&call).await?;
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
                self.intent_barrier_call(&call).await?;
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
                self.intent_barrier_call(&call).await?;
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
            VerbRoute::Ship(peer) => {
                // Rung 13: pending intents in the dir flush first (the
                // shipped page must be complete), and the rung-12
                // view-serve is forbidden while an UPDATE authority
                // governs the dir (the view may lag our own applies).
                self.intent_barrier(&[dir], &[]).await?;
                if !super::intents::authority_governs(&peer.endpoint, dir) {
                    if let Some(entries) =
                        self.deleg_readdir(&peer.endpoint, dir, offset, max).await
                    {
                        return Ok(entries);
                    }
                }
                match self.ship_one(&peer, call).await? {
                    MetaReply::Dir(entries) => {
                        Ok(entries.into_iter().map(DirEntry::from).collect())
                    }
                    other => Err(super::protocol_error(
                        MetaVerb::Readdir,
                        &format!("{other:?}"),
                        "a directory page",
                    )),
                }
            }
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
                // Rung 13: a pending mint serves its image; a destroyed
                // mint answers the owner's latched errno (§8.2's child
                // poison).
                if let Some(image) = super::intents::pending_image(ino) {
                    return Ok(image);
                }
                // The UPDATE authority's own directory: serve the folded
                // grant image (the D2.c parent refresh's per-create
                // getattr — the measured live finding).
                if let Some(attrs) = super::intents::authority_attr_probe(&peer.endpoint, ino) {
                    return Ok(attrs);
                }
                if let Some(errno) = super::intents::destroyed_errno(ino) {
                    return Err(SqueezefsError::refused(
                        errno,
                        format!(
                            "ino {ino} was a locally-minted intent whose apply was refused \
                             (the deferred-error law) — the mint is destroyed"
                        ),
                    ));
                }
                if let Some(inode) = self.deleg_getattr(&peer.endpoint, ino).await {
                    return Ok(inode);
                }
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
                // Rung 13: a setattr on a PENDING ino defers INTO the
                // batch (the tar utimensat shape — zero wire, ordered
                // after the create it names); size changes and
                // non-pending inos take the barriered shipped path.
                if let Some(image) = super::intents::try_defer_setattr(
                    &peer.endpoint,
                    ino,
                    mode,
                    uid,
                    gid,
                    size,
                    atime,
                    mtime,
                    ctime,
                ) {
                    return Ok(image);
                }
                if let Some(errno) = super::intents::destroyed_errno(ino) {
                    return Err(SqueezefsError::refused(
                        errno,
                        format!(
                            "ino {ino} was a locally-minted intent whose apply was refused \
                             (the deferred-error law) — the mint is destroyed"
                        ),
                    ));
                }
                self.intent_barrier_call(&call).await?;
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
            VerbRoute::Ship(peer) => {
                self.intent_barrier(&[ino], &[]).await?;
                match self.ship_one(&peer, call).await? {
                    MetaReply::Xattr(value) => Ok(value),
                    other => Err(super::protocol_error(
                        MetaVerb::Getxattr,
                        &format!("{other:?}"),
                        "an xattr value",
                    )),
                }
            }
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
                self.intent_barrier_call(&call).await?;
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
                self.intent_barrier_call(&call).await?;
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
            VerbRoute::Ship(peer) => {
                self.intent_barrier(&[ino], &[]).await?;
                match self.ship_one(&peer, call).await? {
                    MetaReply::Names(names) => Ok(names),
                    other => Err(super::protocol_error(
                        MetaVerb::Listxattr,
                        &format!("{other:?}"),
                        "an xattr name list",
                    )),
                }
            }
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
                self.intent_barrier_call(&call).await?;
                expect_unit(self.ship_one(&peer, call).await?, MetaVerb::DestroyInode)
            }
        }
    }
}
