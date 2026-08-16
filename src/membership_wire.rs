//! DLM **stage S6** — the membership plane's wire: verbs, the owner-side
//! service, the additive verb router, the dial side, and DISC-1's
//! projection off the census
//! (`docs/pre-rc-engineering-spec.md` §6.5 item 3, §6.7 *Transport*, §6.9
//! S6; contracts in `tests/dlm_membership_tests.rs`).
//!
//! Everything here rides **`cluster_wire`** — the ONE cluster transport
//! (DLM S3): binary framing with per-class caps, zero-config storage-trust
//! mutual authentication from the `job:enroll` secret, a per-frame session
//! MAC, and owner-side handling on pinned `sqz-cluster-svc{n}` lanes.
//! Nothing new is invented at the transport layer, which is the point of
//! S3 having landed first.
//!
//! # Verb numbering, and why it is a BLOCK
//!
//! Membership owns `0x0100..=0x01FF` ([`VERB_MEMBERSHIP_BASE`]). S3 shipped
//! `VERB_PING = 0`, S4's lock verbs and S8's twelve metadata verbs take
//! their own low numbers, and this stage deliberately starts a block far
//! above them so parallel stages cannot collide by picking "the next free
//! number" simultaneously — the same failure mode that produced two
//! parallel incompat-bit collisions in this program.
//!
//! # The router is ADDITIVE
//!
//! `cluster_wire::RpcListener` takes ONE `RpcService`. [`VerbRouter`]
//! dispatches by verb RANGE to registered services, so membership,
//! S4's lock verbs and S8's metadata verbs compose onto one listener
//! without any of them rewriting the dispatch — an unclaimed verb still
//! answers `RPC_UNKNOWN_VERB`, never a silent `RPC_OK`.
//!
//! # Why the census is paged
//!
//! A 15,000-member census at ~120 B/row is ~1.8 MB — past the
//! `CONTROL_MAX_FRAME_BYTES` cap by design. [`MemberClient::census`] walks
//! pages ([`CENSUS_PAGE_MAX`] rows each), so the read side pays
//! `ceil(N/page)` round trips off the metadata plane instead of
//! `listxattr(1)` plus one `getxattr` per client under a shared `I{1}`
//! lock (§6.5 item 3's "the read side is worse").

use crate::cluster_wire::{
    ClusterPeer, RpcClient, RpcListener, RpcListenerConfig, RpcRequest, RpcResponse, RpcService,
    RPC_OK, RPC_UNKNOWN_VERB,
};
use crate::error::{Result, SqueezefsError};
use crate::membership::{
    self, Grant, JoinOutcome, JoinRequest, LeaseClock, MemberRole, MemberSession, MemberSnapshot,
    MembershipOwner, RenewOutcome,
};
use crate::meta_backend::kv::backend::{KvMetaBackend, MountRegistration};
use bincode::Options as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Verbs and statuses
// ---------------------------------------------------------------------------

/// First verb of the membership block (`0x0100..=0x01FF`). See the module
/// docs for why membership takes a block far above S3/S4/S8's numbers.
pub const VERB_MEMBERSHIP_BASE: u16 = 0x0100;
/// Last verb of the membership block.
pub const VERB_MEMBERSHIP_LAST: u16 = 0x01FF;

/// Join (fresh) or reclaim (carrying the prior epoch).
pub const VERB_MEMBERSHIP_JOIN: u16 = VERB_MEMBERSHIP_BASE;
/// Renew a lease — the plane's hot verb, and the whole heartbeat.
pub const VERB_MEMBERSHIP_RENEW: u16 = VERB_MEMBERSHIP_BASE + 1;
/// Leave cleanly (unmount): no TTL wait for a mount that said goodbye.
pub const VERB_MEMBERSHIP_LEAVE: u16 = VERB_MEMBERSHIP_BASE + 2;
/// One census page.
pub const VERB_MEMBERSHIP_CENSUS: u16 = VERB_MEMBERSHIP_BASE + 3;

/// Status: the owner refused (grace window, garbled request). The body is a
/// [`RefusedFrame`] carrying the reason and an honest retry-after.
pub const RPC_MEMBERSHIP_REFUSED: u16 = 0x11;
/// Status: the presented lease is not custody (evicted, swept, or minted by
/// a previous owner). Its own status because the member's correct response
/// is **self-fence then re-join**, not retry.
pub const RPC_MEMBERSHIP_UNKNOWN_LEASE: u16 = 0x12;
/// Status: the request body did not decode. Loud, never guessed.
pub const RPC_MEMBERSHIP_BAD_REQUEST: u16 = 0x13;

/// Rows per census page. Sized so a page stays far inside
/// `cluster_wire::CONTROL_MAX_FRAME_BYTES` (1 MiB) at the ~120 B/row this
/// snapshot encodes to, with room for long ids and endpoints.
pub const CENSUS_PAGE_MAX: usize = 1024;

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// `VERB_MEMBERSHIP_RENEW` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewFrame {
    /// The member's identity.
    pub id: String,
    /// The lease epoch it believes it holds.
    pub epoch: u64,
    /// §6.8 item 3: the freed-offset epoch this member has passed.
    pub acked_free_epoch: u64,
}

/// `VERB_MEMBERSHIP_LEAVE` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaveFrame {
    /// The departing member's identity.
    pub id: String,
}

/// `VERB_MEMBERSHIP_CENSUS` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CensusRequestFrame {
    /// Exclusive lower bound (`0` = from the start).
    pub cursor: u64,
    /// Rows wanted, clamped to [`CENSUS_PAGE_MAX`].
    pub limit: u32,
}

/// One census page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CensusPageFrame {
    /// The rows.
    pub rows: Vec<MemberSnapshot>,
    /// The next cursor, or `None` on the last page.
    pub next: Option<u64>,
}

/// A refusal, with the reason an operator reads and the backoff a member
/// obeys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedFrame {
    /// Operator-facing reason.
    pub reason: String,
    /// When to retry, ms.
    pub retry_after_ms: u64,
}

/// The codec: bincode with a decode limit equal to the delivered bytes, so
/// a lying in-body length can never become an allocation authority — the
/// same law `cluster_wire` applies to its own frames (VAL-6).
fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    bincode::DefaultOptions::new().serialize(v).map_err(|e| {
        SqueezefsError::InvalidOperation(format!("membership frame encode failed: {e}"))
    })
}

fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    bincode::DefaultOptions::new()
        .with_limit(body.len() as u64)
        .deserialize(body)
        .map_err(|e| SqueezefsError::InvalidOperation(format!("undecodable membership frame: {e}")))
}

/// Encode a census request body (the dial side and the router contract test
/// both need it, so it is public rather than duplicated).
pub fn encode_census_request(cursor: u64, limit: u32) -> Result<Vec<u8>> {
    encode(&CensusRequestFrame { cursor, limit })
}

// ---------------------------------------------------------------------------
// The owner-side service
// ---------------------------------------------------------------------------

/// The membership verbs, served from the RAM lease table.
///
/// `call` is synchronous by contract (`cluster_wire::RpcService`) and every
/// operation here is RAM-only — one `scc` probe plus a couple of atomics —
/// which is exactly §6.7's rule that owner-side RPC never awaits on a lane
/// and never runs on the commit conveyor's task.
#[derive(Debug)]
pub struct MembershipService {
    owner: Arc<MembershipOwner>,
}

impl MembershipService {
    /// Serve `owner`'s census.
    pub fn new(owner: Arc<MembershipOwner>) -> Self {
        Self { owner }
    }

    fn refuse(id: u64, status: u16, reason: String, retry_after_ms: u64) -> RpcResponse {
        let body = encode(&RefusedFrame {
            reason,
            retry_after_ms,
        })
        .unwrap_or_default();
        RpcResponse { id, status, body }
    }
}

impl RpcService for MembershipService {
    fn call(&self, req: RpcRequest) -> RpcResponse {
        match req.verb {
            VERB_MEMBERSHIP_JOIN => match decode::<JoinRequest>(&req.body) {
                Ok(join) => match self.owner.join(join) {
                    JoinOutcome::Granted(grant) => RpcResponse {
                        id: req.id,
                        status: RPC_OK,
                        body: encode(&grant).unwrap_or_default(),
                    },
                    JoinOutcome::Refused {
                        reason,
                        retry_after_ms,
                    } => Self::refuse(req.id, RPC_MEMBERSHIP_REFUSED, reason, retry_after_ms),
                    // A dead reclaim: the member must self-fence, then
                    // re-join fresh — its own status, never a retry.
                    JoinOutcome::UnknownLease { reason } => {
                        Self::refuse(req.id, RPC_MEMBERSHIP_UNKNOWN_LEASE, reason, 0)
                    }
                },
                Err(e) => Self::refuse(req.id, RPC_MEMBERSHIP_BAD_REQUEST, e.to_string(), 0),
            },
            VERB_MEMBERSHIP_RENEW => match decode::<RenewFrame>(&req.body) {
                Ok(r) => match self.owner.renew(&r.id, r.epoch, r.acked_free_epoch) {
                    RenewOutcome::Renewed(grant) => RpcResponse {
                        id: req.id,
                        status: RPC_OK,
                        body: encode(&grant).unwrap_or_default(),
                    },
                    RenewOutcome::UnknownLease { reason } => Self::refuse(
                        req.id,
                        RPC_MEMBERSHIP_UNKNOWN_LEASE,
                        reason,
                        // No retry-after: the correct response is
                        // self-fence then re-join, not a retry.
                        0,
                    ),
                },
                Err(e) => Self::refuse(req.id, RPC_MEMBERSHIP_BAD_REQUEST, e.to_string(), 0),
            },
            VERB_MEMBERSHIP_LEAVE => match decode::<LeaveFrame>(&req.body) {
                Ok(l) => {
                    let left = self.owner.leave(&l.id);
                    RpcResponse {
                        id: req.id,
                        status: RPC_OK,
                        body: vec![u8::from(left)],
                    }
                }
                Err(e) => Self::refuse(req.id, RPC_MEMBERSHIP_BAD_REQUEST, e.to_string(), 0),
            },
            VERB_MEMBERSHIP_CENSUS => match decode::<CensusRequestFrame>(&req.body) {
                Ok(c) => {
                    let limit = (c.limit as usize).clamp(1, CENSUS_PAGE_MAX);
                    let (rows, next) = self.owner.census(c.cursor, limit);
                    RpcResponse {
                        id: req.id,
                        status: RPC_OK,
                        body: encode(&CensusPageFrame { rows, next }).unwrap_or_default(),
                    }
                }
                Err(e) => Self::refuse(req.id, RPC_MEMBERSHIP_BAD_REQUEST, e.to_string(), 0),
            },
            other => RpcResponse {
                id: req.id,
                status: RPC_UNKNOWN_VERB,
                body: format!("verb {other:#x} is not a membership verb").into_bytes(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// The additive router
// ---------------------------------------------------------------------------

/// Dispatch by verb RANGE to registered services — the additive
/// registration shape, so several stages can put verbs on ONE listener
/// without editing each other's dispatch (see the module docs).
///
/// Ranges are checked in registration order; an unclaimed verb answers
/// `RPC_UNKNOWN_VERB` rather than being handed to whoever is last.
pub struct VerbRouter {
    routes: Vec<(u16, u16, Arc<dyn RpcService>)>,
}

impl std::fmt::Debug for VerbRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerbRouter")
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

impl Default for VerbRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl VerbRouter {
    /// An empty router (every verb unclaimed).
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    /// Claim `lo..=hi` for `service`.
    pub fn with(mut self, lo: u16, hi: u16, service: Arc<dyn RpcService>) -> Self {
        self.routes.push((lo.min(hi), hi.max(lo), service));
        self
    }

    /// Claim the membership block for `owner`'s census.
    pub fn with_membership(self, owner: Arc<MembershipOwner>) -> Self {
        self.with(
            VERB_MEMBERSHIP_BASE,
            VERB_MEMBERSHIP_LAST,
            Arc::new(MembershipService::new(owner)),
        )
    }
}

impl RpcService for VerbRouter {
    fn call(&self, req: RpcRequest) -> RpcResponse {
        for (lo, hi, svc) in &self.routes {
            if req.verb >= *lo && req.verb <= *hi {
                return svc.call(req);
            }
        }
        RpcResponse {
            id: req.id,
            status: RPC_UNKNOWN_VERB,
            body: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// The plane (owner side)
// ---------------------------------------------------------------------------

/// Where and how the owner listens. Defaults follow the job wire's posture
/// (plaintext-but-authenticated, bounded, ephemeral port) because it is the
/// same transport with the same storage-trust root.
#[derive(Debug, Clone)]
pub struct MembershipPlaneConfig {
    /// Bind address.
    pub bind_addr: SocketAddr,
    /// Owner-side RPC lanes (pinned service threads).
    pub service_threads: usize,
    /// Idle bound on an admitted session — a member that neither renews nor
    /// leaves inside several renewal cadences is dropped at the transport,
    /// and its lease then expires on the owner's TTL like any other.
    pub session_idle: Duration,
}

impl MembershipPlaneConfig {
    /// The loopback posture the suites and the simulated-client harness
    /// use: `127.0.0.1:0`, one lane, short idle bound.
    pub fn loopback() -> Self {
        Self {
            bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
            service_threads: 1,
            session_idle: Duration::from_secs(30),
        }
    }

    /// The mount posture: bind where the operator said, lanes from the
    /// cluster-wire derivation.
    pub fn for_mount(bind_addr: SocketAddr, session_idle: Duration) -> Self {
        Self {
            bind_addr,
            service_threads: crate::cluster_wire::default_service_threads(),
            session_idle,
        }
    }
}

/// The owner's listener plus its lease authority.
#[derive(Debug)]
pub struct MembershipPlane {
    listener: Arc<RpcListener>,
    owner: Arc<MembershipOwner>,
}

impl MembershipPlane {
    /// Bind and serve the membership verbs for `owner`, authenticated
    /// against the volume set's storage-trust `secret`.
    pub fn start(
        cfg: MembershipPlaneConfig,
        secret: Vec<u8>,
        owner: Arc<MembershipOwner>,
    ) -> Result<Arc<Self>> {
        let service: Arc<dyn RpcService> =
            Arc::new(VerbRouter::new().with_membership(Arc::clone(&owner)));
        let listener = RpcListener::start(
            RpcListenerConfig {
                bind_addr: cfg.bind_addr,
                security: None,
                session_idle_timeout: cfg.session_idle,
                service_threads: cfg.service_threads.max(1),
                ..RpcListenerConfig::default()
            },
            secret,
            service,
        )?;
        log::info!(
            "membership plane listening on {} (owner '{}', term {}) — liveness for every \
             member is RAM state renewed over this wire, so a beat costs ZERO journal \
             transactions (DLM S6)",
            listener.endpoint(),
            owner.id(),
            owner.term()
        );
        Ok(Arc::new(Self { listener, owner }))
    }

    /// The bound address (published in the rendezvous record).
    pub fn endpoint(&self) -> SocketAddr {
        self.listener.endpoint()
    }

    /// The lease authority.
    pub fn owner(&self) -> &Arc<MembershipOwner> {
        &self.owner
    }

    /// Transport counters (`mac_failures` / `service_refusals` are
    /// must-stay-0 tripwires on a healthy cluster — the S3 law, unchanged).
    pub fn transport_stats(&self) -> crate::cluster_wire::RpcStats {
        self.listener.stats()
    }

    /// Stop serving and join the lanes.
    pub fn shutdown(&self) {
        self.listener.shutdown();
    }
}

// ---------------------------------------------------------------------------
// The dial side (member)
// ---------------------------------------------------------------------------

/// One-shot clean leave — the member DISARM path's synchronous goodbye.
///
/// `MembershipOwner::leave`'s contract is "no TTL wait for a mount that
/// said goodbye", and the renewal loop's own leave only fires at its next
/// wake — which a normal umount never reaches. The disarm therefore dials
/// the leave itself, bounded and best-effort (the owner's TTL sweep is
/// the backstop when the wire is already gone). A second leave for the
/// same id is a no-op at the owner.
pub async fn leave_once(endpoint: &str, secret: &[u8], id: &str) -> Result<bool> {
    let mut rpc = RpcClient::connect(endpoint, secret, id, None).await?;
    let reply = rpc
        .call(
            VERB_MEMBERSHIP_LEAVE,
            encode(&LeaveFrame { id: id.to_string() })?,
        )
        .await?;
    if reply.status != RPC_OK {
        return Err(SqueezefsError::InvalidOperation(format!(
            "membership leave got status {:#x} from the owner",
            reply.status
        )));
    }
    Ok(reply.body.first().copied() == Some(1))
}

/// A member's authenticated session with the owner, plus the member's own
/// (stricter) lease view.
pub struct MemberClient {
    rpc: RpcClient,
    session: Arc<MemberSession>,
    clock: LeaseClock,
    id: String,
}

impl std::fmt::Debug for MemberClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemberClient")
            .field("id", &self.id)
            .field("epoch", &self.session.epoch())
            .field("t_self_deadline_ms", &self.session.t_self_deadline_ms())
            .finish_non_exhaustive()
    }
}

impl MemberClient {
    /// Dial the owner, prove storage membership (S3's ladder), and join.
    ///
    /// The lease is anchored on the instant the JOIN was **sent** — never on
    /// a value from the owner's clock — which is what makes the member's
    /// deadline strictly earlier than the owner's by the round trip as well
    /// as by `2·skew_max + D_purge`.
    pub async fn join(
        endpoint: &str,
        secret: &[u8],
        req: JoinRequest,
        clock: LeaseClock,
    ) -> Result<Self> {
        let id = req.id.clone();
        let role = req.role;
        let mut rpc = RpcClient::connect(endpoint, secret, &id, None).await?;
        let anchor = clock.now_ms();
        let reply = rpc.call(VERB_MEMBERSHIP_JOIN, encode(&req)?).await?;
        let grant = Self::grant_or_error(reply, "join")?;
        let session = Arc::new(MemberSession::adopt(
            &id,
            role,
            &grant,
            anchor,
            clock.clone(),
        ));
        Ok(Self {
            rpc,
            session,
            clock,
            id,
        })
    }

    fn grant_or_error(reply: RpcResponse, what: &str) -> Result<Grant> {
        match reply.status {
            RPC_OK => decode::<Grant>(&reply.body),
            RPC_MEMBERSHIP_UNKNOWN_LEASE => {
                let r: RefusedFrame = decode(&reply.body)?;
                // Structural, not prose: the member's renewal ladder keys
                // on this class — self-fence then re-join, never a retry
                // (`membership::member_renewal_tick`).
                Err(SqueezefsError::MembershipLeaseNotCustody(format!(
                    "membership {what} refused: {}",
                    r.reason
                )))
            }
            RPC_MEMBERSHIP_REFUSED => {
                let r: RefusedFrame = decode(&reply.body)?;
                Err(SqueezefsError::InvalidOperation(format!(
                    "membership {what} refused by the owner: {} (retry after {} ms)",
                    r.reason, r.retry_after_ms
                )))
            }
            other => Err(SqueezefsError::InvalidOperation(format!(
                "membership {what} got status {other:#x} from the owner"
            ))),
        }
    }

    /// `true` ⇔ the session is authenticated (storage-trust proof plus a
    /// per-frame MAC — S3 admits nothing else).
    pub fn authenticated(&self) -> bool {
        self.rpc.authn().authenticated()
    }

    /// The member's lease view.
    pub fn session(&self) -> &Arc<MemberSession> {
        &self.session
    }

    /// Renew — the heartbeat. Carries the member's acknowledged
    /// freed-offset epoch (§6.8 item 3) and re-anchors the member's own
    /// deadline on THIS send instant.
    pub async fn renew(&mut self) -> Result<()> {
        let anchor = self.clock.now_ms();
        let body = encode(&RenewFrame {
            id: self.id.clone(),
            epoch: self.session.epoch(),
            acked_free_epoch: self.session.acked_free_epoch(),
        })?;
        let reply = self.rpc.call(VERB_MEMBERSHIP_RENEW, body).await?;
        let grant = Self::grant_or_error(reply, "renew")?;
        self.session.renewed(&grant, anchor);
        Ok(())
    }

    /// One census page (`limit` clamped owner-side to
    /// [`CENSUS_PAGE_MAX`]).
    pub async fn census(
        &mut self,
        cursor: u64,
        limit: u32,
    ) -> Result<(Vec<MemberSnapshot>, Option<u64>)> {
        let reply = self
            .rpc
            .call(
                VERB_MEMBERSHIP_CENSUS,
                encode_census_request(cursor, limit)?,
            )
            .await?;
        if reply.status != RPC_OK {
            return Err(SqueezefsError::InvalidOperation(format!(
                "membership census got status {:#x} from the owner",
                reply.status
            )));
        }
        let page: CensusPageFrame = decode(&reply.body)?;
        Ok((page.rows, page.next))
    }

    /// Leave cleanly: the owner drops the lease immediately, so nothing
    /// waits out a TTL for a mount that said goodbye.
    pub async fn leave(mut self) -> Result<()> {
        let body = encode(&LeaveFrame {
            id: self.id.clone(),
        })?;
        let reply = self.rpc.call(VERB_MEMBERSHIP_LEAVE, body).await?;
        if reply.status != RPC_OK {
            return Err(SqueezefsError::InvalidOperation(format!(
                "membership leave got status {:#x} from the owner",
                reply.status
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DISC-1 over the census + the read-side projection
// ---------------------------------------------------------------------------

/// **DISC-1's final form** (ruling D2, spec §6.5 item 3): peers projected
/// from the membership census instead of from `client:{uuid}` records, so
/// discovery adds **zero** load to the ino-1 hotspot.
///
/// The selection law is IDENTICAL to
/// [`crate::cluster_wire::peers_from_registrations`] — live only, an
/// endpoint required (a member that dials but does not serve is not a
/// peer), `self_id` excluded, deduplicated, ordered by id so discovery is
/// deterministic. Pure and total, so the law is testable without a volume
/// or a listener.
///
/// Public API surface with no in-tree dial-side caller yet BY DESIGN: the
/// projection lands and is pinned here, while swapping
/// `cluster_wire::discover_peers` onto it is one line in a file S8 is
/// concurrently rewriting (the coordination rule — a rewrite collision in
/// the shared dispatch costs more than the one-line deferral).
pub fn peers_from_census(rows: &[MemberSnapshot], self_id: Option<&str>) -> Vec<ClusterPeer> {
    let mut by_id: std::collections::BTreeMap<String, ClusterPeer> =
        std::collections::BTreeMap::new();
    for row in rows {
        if row.state != "live" {
            continue;
        }
        if self_id.is_some_and(|s| s == row.id) {
            continue;
        }
        let Some(endpoint) = row.endpoint.as_ref() else {
            continue;
        };
        by_id.entry(row.id.clone()).or_insert_with(|| ClusterPeer {
            id: row.id.clone(),
            endpoint: endpoint.clone(),
            age_secs: Some(row.age_ms / 1000),
            fresh: true,
        });
    }
    by_id.into_values().collect()
}

/// The read side's `O(1)`-metadata path for `squeezefs clients` /
/// `squeezefs status`: read ONE rendezvous key, dial the owner, and page
/// its RAM census — instead of `listxattr(1)` plus one `getxattr` per
/// client under a shared `I{1}` lock (§6.5 item 3).
///
/// Rows come back in the SAME shape the record surfaces print
/// ([`MountRegistration`]), with `kind` naming the role
/// (`member-writer` / `member-reader`), so the CLI merges them without a
/// second renderer and an operator reads one classification.
///
/// `None` = this volume publishes no membership plane (the shipped default
/// until `SQUEEZEFS_MEMBERSHIP_BIND` is set) or the owner it names does not
/// answer inside `deadline` — a crashed owner, which the caller reports
/// from the records exactly as before.
pub async fn census_as_registrations(
    be: &KvMetaBackend,
    deadline: Duration,
) -> Option<Vec<MountRegistration>> {
    let rec = membership::read_owner_record(be).await?;
    let secret = membership::cluster_secret(be).await?;
    let probe_id = format!("probe-{}", uuid::Uuid::new_v4());
    let rows = squeezefs_ipc::sqz_time::timeout(deadline, async {
        let mut client = CensusProbe::connect(&rec.endpoint, &secret, &probe_id).await?;
        let mut cursor = Some(0u64);
        let mut out: Vec<MemberSnapshot> = Vec::new();
        while let Some(c) = cursor {
            let (rows, next) = client.census(c, CENSUS_PAGE_MAX as u32).await?;
            out.extend(rows);
            cursor = next;
        }
        Ok::<_, SqueezefsError>(out)
    })
    .await
    .map_err(|_| {
        log::warn!(
            "membership: owner '{}' at {} did not answer a census within {deadline:?} — \
             reporting from the durable records only",
            rec.id,
            rec.endpoint
        );
    })
    .ok()?
    .map_err(|e| {
        log::warn!(
            "membership: census probe of owner '{}' at {} failed: {e}",
            rec.id,
            rec.endpoint
        );
    })
    .ok()?;
    let now = membership::unix_now_secs();
    Some(
        rows.into_iter()
            .map(|row| MountRegistration {
                key: format!("member:{}", row.id),
                kind: match row.role {
                    MemberRole::Writer => "member-writer",
                    MemberRole::Reader => "member-reader",
                },
                id: row.id,
                pid: Some(row.pid),
                boot: Some(row.boot),
                heartbeat_ts: Some(now.saturating_sub(row.age_ms / 1000)),
                age_secs: Some(row.age_ms / 1000),
                heartbeat_fresh: row.state == "live",
                holder_provably_dead: false,
                job_endpoint: row.endpoint,
            })
            .collect(),
    )
}

/// A **census-only** session: authenticated exactly like a member, but it
/// joins nothing, so it takes no lease and appears in no census. What
/// `squeezefs clients` and `squeezefs status` dial with — an observer must
/// never become a member (that is how an observability tool starts holding
/// custody).
pub struct CensusProbe {
    rpc: RpcClient,
}

impl std::fmt::Debug for CensusProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CensusProbe").finish_non_exhaustive()
    }
}

impl CensusProbe {
    /// Dial and authenticate, without joining.
    pub async fn connect(endpoint: &str, secret: &[u8], probe_id: &str) -> Result<Self> {
        Ok(Self {
            rpc: RpcClient::connect(endpoint, secret, probe_id, None).await?,
        })
    }

    /// One census page.
    pub async fn census(
        &mut self,
        cursor: u64,
        limit: u32,
    ) -> Result<(Vec<MemberSnapshot>, Option<u64>)> {
        let reply = self
            .rpc
            .call(
                VERB_MEMBERSHIP_CENSUS,
                encode_census_request(cursor, limit)?,
            )
            .await?;
        if reply.status != RPC_OK {
            return Err(SqueezefsError::InvalidOperation(format!(
                "membership census got status {:#x} from the owner",
                reply.status
            )));
        }
        let page: CensusPageFrame = decode(&reply.body)?;
        Ok((page.rows, page.next))
    }
}
