//! **GPFS-strict read tokens** — the S10 LOOKUP delegation generalized to
//! every foreign object (docs/design-symmetric-metadata.md §5.7, R-SYM-4,
//! KD-SYM-19; §5.7.2 the `-o ro` reader; §5.7.3 recall-driven free-grace;
//! §5.7.4 the recall economics; §11 the Token family; PR 5).
//!
//! # The law
//!
//! Foreign metadata is read under a **token** and ONLY that way: a
//! `-o ro` reader of an armed symmetric volume serves every user-visible
//! object — attrs, xattrs, a directory's dentry set — from records the
//! object's HOLDER handed it in a grant (Lustre's intent lock: the grant
//! reply CARRIES the records, so the reader never reads a foreign leaf
//! from the device), and the holder RECALLS every reader's token on an
//! object BEFORE a commit that mutates it lands (the recall rides the
//! commit's 4a guards, batched once per conveyor pass over the UNION of
//! the pass's objects — §5.7.1). A reader acks a recall only after its
//! in-flight serves of the object drain and its block-key census is
//! purged; the commit proceeds on every ack OR the reader's membership
//! lease expiry (`T_owner` — a dead reader's tokens die with its lease).
//! A foreign create is therefore visible at the reader's NEXT resolve —
//! exact, never bounded (`reader_staleness_bound_ms` reads 0 for metadata
//! under tokens; the S5 epoch poller survives as the CONTROL-plane
//! projection: tree 0, the ledger, the pages).
//!
//! # The two halves
//!
//! * [`TokenHolderPlane`] — the armed writer's side: grants served from
//!   its RAM-authoritative trees, the owner-side [`RecallLane`] reused
//!   verbatim for the batched, rate-limited recall bookkeeping (one
//!   in-flight frame per reader, acks correlated by frame id, the
//!   deadline the reader's LEASE TTL — `T_owner`, never a p99 — and the
//!   valve OFF, because under R-SYM-4 a demoted object would have NO read
//!   method), the standing
//!   recall CHANNEL a reader parks on ([`TokenCall::Recall`] — the
//!   dial-only wire's owner-initiated push, the S10 `DelegRecall` shape),
//!   and the conveyor pass's wait ([`TokenHolderPlane::recall_and_wait`]).
//! * [`TokenReaderPlane`] — the reader's side: the token cache (`scc`,
//!   lock-free; entries bounded by the delegation cache's R5 law — an
//!   eviction is a voluntary [`TokenCall::Release`]), single-flight
//!   grants, the recall task that drops entries, drains their in-flight
//!   serves, runs the mount's data drain + R-6 purge through the
//!   installed [`RecallDataSink`] and acks; the serve gate (an entry
//!   serves only while the recall channel is FRESH and the reader's own
//!   lease is live — fail-closed, never stale).
//!
//! Objects are LOCAL key inos of one volume; one [`TokenService`] per
//! volume, dispatched by the frame's volume ordinal ([`TokenSetService`])
//! on the S8 listener's verb router — its own verb block, `0x0600`.
//!
//! Frame bodies are bincode: encoded unbounded (we build them), decoded
//! **bounded** (untrusted — a length in a frame is a claim, never an
//! allocation authority).

use super::tokens::{RecallFrame, RecallLane};
use crate::cluster_wire::{RpcAsyncService, RpcClient, RpcRequest, RpcResponse};
use crate::error::{Result, SqueezefsError};
use crate::fuse_client::{LatencyHistogram, QueueDepthHistogram};
use crate::meta_backend::kv::backend::KvMetaBackend;
use crate::meta_backend::kv::record::{DentryValue, InodeValue};
use crate::meta_backend::Ino;
use bincode::Options as _;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------

/// The token vocabulary's schema (under `CLUSTER_WIRE_SCHEMA` 5 — the
/// program's unreleased wire).
pub const TOKEN_SCHEMA: u32 = 1;

/// The token verb block: `0x0600..=0x06FF`, disjoint from the S8 metadata
/// (16/17), S9 custody (`0x0200`), publish (`0x0300`), delegation
/// (`0x0400`) and manager (`0x0500`) blocks.
pub const VERB_TOKEN_BASE: u16 = 0x0600;
/// The ONE verb: a [`TokenRequestFrame`] carrying a [`TokenCall`].
pub const VERB_TOKEN_CALL: u16 = VERB_TOKEN_BASE;
/// Last verb of the block.
pub const VERB_TOKEN_LAST: u16 = 0x06FF;

/// Frame status: served — the body is a [`TokenReplyFrame`].
pub const STATUS_OK: u16 = crate::cluster_wire::RPC_OK;
/// Frame status: the peer speaks another vocabulary version.
pub const STATUS_SCHEMA: u16 = super::wire::STATUS_SCHEMA;
/// Frame status: undecodable body (bounded, refused loud).
pub const STATUS_MALFORMED: u16 = super::wire::STATUS_MALFORMED;
/// Frame status: this volume serves no tokens (no armed plane).
pub const STATUS_NOT_HOLDER: u16 = super::wire::STATUS_NOT_OWNER;
/// Frame status: the verb could not be served; the body carries
/// [`TokenReply::Refused`] with the reason.
pub const STATUS_REFUSED: u16 = 56;

/// The token modes on the wire: `Read` is shared (N holders); the
/// holder's `Write` is implicit — the lessee needs no token on its own
/// tree (§5.7.1) — and never travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenMode {
    Read,
}

/// What a grant must carry beyond the object's attrs + xattrs (always
/// carried): a directory's dentry set, paged by the reply cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenWants {
    pub dentries: bool,
}

/// The token verbs (§6.3 — "the S10 delegation verbs": Grant / Recall /
/// RecallAck / Release). The variant ORDER is the wire index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenCall {
    /// A read token on `object` (a LOCAL key ino of the frame's volume),
    /// the records carried; `after` = the dentry continuation cookie (0 =
    /// the directory's start) when `wants.dentries`. Idempotent: a holder
    /// that already granted this client the token answers `already`.
    Grant {
        object: u64,
        mode: TokenMode,
        wants: TokenWants,
        after: u64,
    },
    /// The reader's standing recall channel: the holder PARKS the call
    /// until a recall frame for this client exists (or `wait_ms`
    /// elapses) and answers [`TokenReply::Recall`].
    Recall { wait_ms: u32 },
    /// The reader drained and purged every object of `frame_id`.
    RecallAck { frame_id: u64 },
    /// Voluntary release (the reader's eviction).
    Release { objects: Vec<u64> },
}

impl TokenCall {
    /// The verb's name (logs).
    pub fn name(&self) -> &'static str {
        match self {
            TokenCall::Grant { .. } => "Grant",
            TokenCall::Recall { .. } => "Recall",
            TokenCall::RecallAck { .. } => "RecallAck",
            TokenCall::Release { .. } => "Release",
        }
    }
}

/// An inode record on the wire (the holder's folded `getattr` view).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireAttrs {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub flags: u32,
    pub rdev: u32,
    pub size: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

impl From<InodeValue> for WireAttrs {
    fn from(v: InodeValue) -> Self {
        Self {
            mode: v.mode,
            uid: v.uid,
            gid: v.gid,
            nlink: v.nlink,
            flags: v.flags,
            rdev: v.rdev,
            size: v.size,
            atime: v.atime,
            mtime: v.mtime,
            ctime: v.ctime,
        }
    }
}

impl From<WireAttrs> for InodeValue {
    fn from(w: WireAttrs) -> Self {
        Self {
            mode: w.mode,
            uid: w.uid,
            gid: w.gid,
            nlink: w.nlink,
            flags: w.flags,
            rdev: w.rdev,
            size: w.size,
            atime: w.atime,
            mtime: w.mtime,
            ctime: w.ctime,
        }
    }
}

/// One directory entry on the wire, with its resume cookie (the dentry
/// key suffix the reader's `readdir` pages by).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirRecord {
    pub cookie: u64,
    pub child_ino: u64,
    pub file_type: u8,
    pub name: Vec<u8>,
}

/// The records a grant carries (Lustre's intent lock): attrs, every
/// user-visible xattr, and — when asked — a page of the directory's
/// entries with a completion flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRecords {
    pub attrs: WireAttrs,
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    /// `Some((entries, complete))` when dentries were asked for and the
    /// object is a directory.
    pub dir: Option<(Vec<DirRecord>, bool)>,
}

/// The holder's answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenReply {
    Granted {
        records: TokenRecords,
        already: bool,
    },
    /// The object's slot is another appender's (§5.1.6 — the client
    /// redirects).
    NotHolder {
        holder: u32,
    },
    /// The object does not exist.
    Gone,
    /// A recall frame (`objects` empty with `frame_id` 0 = the park
    /// elapsed with nothing to recall).
    Recall {
        frame_id: u64,
        objects: Vec<u64>,
    },
    Acked,
    Released {
        count: u64,
    },
    Refused {
        reason: String,
    },
}

/// One request frame: the schema, the client's correlation id, the volume
/// ordinal the object lives on, the client's identity (the recall lane's
/// holder key — the KD-MW-2 member id) and the call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRequestFrame {
    pub schema: u32,
    pub request_id: u64,
    pub volume: u16,
    pub client: String,
    pub call: TokenCall,
}

/// One reply frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenReplyFrame {
    pub schema: u32,
    pub request_id: u64,
    pub reply: TokenReply,
}

/// The xattr names a token CARRIES — and whose mutation therefore recalls
/// the object's tokens: every user-visible name (`xattr_name_allowed`)
/// plus the two internal records a reader's own paths resolve, the
/// per-inode `layout` (the block map the data path binds — "the layout
/// resolve" of §5.7.1; the recall before a freeing publish is what makes
/// the old layout unservable, §5.7.3) and `system.symlink` (readlink).
/// Every other internal record (`writer_claim`, `client:`, `job:`, the
/// planes' own) is the control plane's — projected, never tokened, and
/// its write recalls nothing.
pub fn token_carried_xattr(name: &str) -> bool {
    crate::meta_backend::kv::backend::xattr_name_allowed(name)
        || name == "layout"
        || name == "system.symlink"
}

/// Decode-side allocation bound: the CONTROL class cap (the S8
/// vocabulary's discipline).
fn decode_limit() -> u64 {
    u64::from(crate::cluster_wire::CONTROL_MAX_FRAME_BYTES)
}

/// The dentry bytes one grant page carries: half the frame cap (the
/// other half is the attrs, the xattrs, framing and the MAC — the S8
/// encode check's conservative split). A directory past it is paged by
/// continuation (the S8 frame law).
pub fn grant_dentry_budget() -> usize {
    (decode_limit() / 2) as usize
}

fn encode<T: Serialize>(value: &T, what: &str) -> Result<Vec<u8>> {
    let body = bincode::DefaultOptions::new()
        .serialize(value)
        .map_err(|e| SqueezefsError::InvalidOperation(format!("token {what} encode: {e}")))?;
    if body.len() as u64 > decode_limit() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "token {what} of {} B exceeds the cluster wire's CONTROL class cap ({} B)",
            body.len(),
            decode_limit()
        )));
    }
    Ok(body)
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T> {
    bincode::DefaultOptions::new()
        .with_limit(decode_limit())
        .deserialize(bytes)
        .map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "token {what}: undecodable frame body ({} B): {e}",
                bytes.len()
            ))
        })
}

/// Encode a request frame (trusted — we built it).
pub fn encode_request(frame: &TokenRequestFrame) -> Result<Vec<u8>> {
    encode(frame, "request")
}

/// Decode a request frame (**untrusted** — bounded).
pub fn decode_request(bytes: &[u8]) -> Result<TokenRequestFrame> {
    decode(bytes, "request")
}

/// Encode a reply frame.
pub fn encode_reply(frame: &TokenReplyFrame) -> Result<Vec<u8>> {
    encode(frame, "reply")
}

/// Decode a reply frame (**untrusted** — bounded).
pub fn decode_reply(bytes: &[u8]) -> Result<TokenReplyFrame> {
    decode(bytes, "reply")
}

// ---------------------------------------------------------------------------
// The holder
// ---------------------------------------------------------------------------

/// What the holder knows about a reader's lease at a recall timeout
/// (§5.7.1: a dead reader's tokens die with its lease, `T_owner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseVerdict {
    /// The reader's membership lease is LIVE — a recall it did not ack
    /// inside the bound is the must-stay-0 stuck-reader class.
    Live,
    /// The reader's lease expired (or it never held one — its token's
    /// expiry IS the derived bound).
    Expired,
}

/// The lease oracle: `client id → verdict`. The default reads the
/// installed membership owner (`lease_deadline_ms`); the contracts install
/// their own.
pub type LeaseOracle = dyn Fn(&str) -> LeaseVerdict + Send + Sync;

/// The recall channel's park bound on the holder: the recall deadline's
/// quarter, floored at the standing poll's park floor and capped at one
/// second — short enough that a recall issued mid-round is delivered
/// within a fraction of the deadline, long enough that an idle reader's
/// channel is not a busy loop (the S10 `deleg_park` derivation's shape).
fn poll_park_bound(deadline: Duration) -> Duration {
    (deadline / 4).clamp(
        super::tokens::RECALL_POLL_PARK_FLOOR,
        Duration::from_secs(1),
    )
}

/// Phase indices of `dlm_token_recall_rtt_ns`.
const RTT_SEND: usize = 0;
const RTT_DRAIN: usize = 1;
const RTT_ACK: usize = 2;
const RTT_TOTAL: usize = 3;
const RTT_PHASES: usize = 4;
const RTT_PHASE_NAMES: [&str; RTT_PHASES] = ["send", "drain", "ack", "total"];

/// The holder's side of the token plane for ONE volume.
pub struct TokenHolderPlane {
    lane: RecallLane,
    /// Frames issued by [`Self::recall_and_wait`], per client, waiting for
    /// the client's standing poll to take them.
    pending_frames: parking_lot::Mutex<HashMap<String, VecDeque<RecallFrame>>>,
    /// Pollers park here for a frame.
    frame_wake: squeezefs_ipc::sqz_notify::Notify,
    /// The pass parks here for acks (and the expiry sweep's ticks).
    ack_wake: squeezefs_ipc::sqz_notify::Notify,
    /// Objects recalled by a pass whose commit has not yet APPLIED: a
    /// grant on one parks here, else it would hand out the pre-commit
    /// records with no recall coming.
    inflight: scc::HashSet<u64>,
    inflight_done: squeezefs_ipc::sqz_notify::Notify,
    lease_oracle: parking_lot::RwLock<Option<Arc<LeaseOracle>>>,
    /// The plane's monotonic origin, and on it the instants the batch's
    /// phases are cut at: the last frame handed to a reader's poll
    /// (`send` ends) and the last ack's arrival (`drain` ends, `ack` —
    /// the wake hop to the pass — begins). One pass per volume, so one
    /// batch at a time per plane.
    epoch: Instant,
    last_send_ns: AtomicU64,
    last_ack_ns: AtomicU64,
    // ---- gauges (§11 the Token family) ----
    grants_served: AtomicU64,
    recalls: AtomicU64,
    recall_acks: AtomicU64,
    expired_with_lease: AtomicU64,
    timeouts_live: AtomicU64,
    releases: AtomicU64,
    /// Conveyor passes that recalled at least one object (the batching
    /// law's denominator: a storm on one object is ONE batch per pass).
    recall_batches: AtomicU64,
    grant_parks: AtomicU64,
    fanout: QueueDepthHistogram,
    rtt: [LatencyHistogram; RTT_PHASES],
}

impl std::fmt::Debug for TokenHolderPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenHolderPlane")
            .field("outstanding", &self.lane.outstanding_now())
            .field("inflight", &self.inflight.len())
            .finish()
    }
}

impl Default for TokenHolderPlane {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenHolderPlane {
    pub fn new() -> Self {
        Self {
            lane: RecallLane::live_tokens(),
            pending_frames: parking_lot::Mutex::new(HashMap::new()),
            frame_wake: squeezefs_ipc::sqz_notify::Notify::new(),
            ack_wake: squeezefs_ipc::sqz_notify::Notify::new(),
            inflight: scc::HashSet::new(),
            inflight_done: squeezefs_ipc::sqz_notify::Notify::new(),
            lease_oracle: parking_lot::RwLock::new(None),
            epoch: Instant::now(),
            last_send_ns: AtomicU64::new(0),
            last_ack_ns: AtomicU64::new(0),
            grants_served: AtomicU64::new(0),
            recalls: AtomicU64::new(0),
            recall_acks: AtomicU64::new(0),
            expired_with_lease: AtomicU64::new(0),
            timeouts_live: AtomicU64::new(0),
            releases: AtomicU64::new(0),
            recall_batches: AtomicU64::new(0),
            grant_parks: AtomicU64::new(0),
            fanout: QueueDepthHistogram::default(),
            rtt: std::array::from_fn(|_| LatencyHistogram::default()),
        }
    }

    /// The owner-side recall lane (the contracts read its outstanding
    /// population).
    pub fn lane(&self) -> &RecallLane {
        &self.lane
    }

    /// Install a lease oracle (the contracts; the mount installs the
    /// membership owner's).
    pub fn install_lease_oracle(&self, oracle: Arc<LeaseOracle>) {
        *self.lease_oracle.write() = Some(oracle);
    }

    fn lease_verdict(&self, client: &str) -> LeaseVerdict {
        if let Some(o) = self.lease_oracle.read().as_ref() {
            return o(client);
        }
        match crate::membership::installed_owner() {
            // A member whose lease deadline is still ahead of the owner's
            // clock is LIVE.
            Some(owner) => match owner.lease_deadline_ms(client) {
                Some(deadline) if owner.now_ms() < deadline => LeaseVerdict::Live,
                _ => LeaseVerdict::Expired,
            },
            // No membership plane: no lease to outlive — the derived bound
            // is the token's expiry.
            None => LeaseVerdict::Expired,
        }
    }

    /// The grant gate: park while `object`'s recalled commit is in
    /// flight (bounded by the recall deadline — past it the object is
    /// wedged, and the grant refuses rather than serving pre-commit
    /// records).
    async fn await_object_settled(&self, object: u64) -> bool {
        if !self.inflight.contains_sync(&object) {
            return true;
        }
        self.grant_parks.fetch_add(1, Ordering::Relaxed);
        let bound = self.lane.config().deadline;
        let started = Instant::now();
        loop {
            let notified = self.inflight_done.notified();
            if !self.inflight.contains_sync(&object) {
                return true;
            }
            let left = bound.saturating_sub(started.elapsed());
            if left.is_zero() {
                return false;
            }
            let _ = squeezefs_ipc::sqz_time::timeout(left, notified).await;
        }
    }

    /// Serve a grant: the valve-less lane records the `(object, client)`
    /// pair (idempotent — `already`), the records come from `volume`'s
    /// RAM-authoritative trees.
    async fn serve_grant(
        &self,
        volume: &KvMetaBackend,
        client: &str,
        object: u64,
        wants: TokenWants,
        after: u64,
    ) -> TokenReply {
        if !self.await_object_settled(object).await {
            return TokenReply::Refused {
                reason: format!(
                    "object {object}: a recalled commit did not apply inside the recall bound"
                ),
            };
        }
        let records = match volume.token_records_for(object, wants, after).await {
            Ok(Some(r)) => r,
            Ok(None) => return TokenReply::Gone,
            Err(e) => {
                return TokenReply::Refused {
                    reason: format!("object {object}: {e}"),
                }
            }
        };
        // The grant is recorded AFTER the records are read: a recall
        // racing this read is ordered by the in-flight gate above (the
        // pass marks its objects before it recalls), and a pass that
        // starts after this grant lands finds the holder in the table.
        let already = self.lane.holds(object, client);
        if !already {
            // The valve is off on this lane, so the decision is always
            // `Granted`.
            let _ = self.lane.try_grant(object, client, Instant::now());
        }
        self.grants_served.fetch_add(1, Ordering::Relaxed);
        TokenReply::Granted { records, already }
    }

    /// The reader's standing poll: hand over the next issued frame for
    /// `client`, parking up to `wait` for one.
    async fn serve_poll(&self, client: &str, wait: Duration) -> TokenReply {
        let started = Instant::now();
        loop {
            let notified = self.frame_wake.notified();
            let taken = {
                let mut pending = self.pending_frames.lock();
                pending.get_mut(client).and_then(|q| q.pop_front())
            };
            if let Some(frame) = taken {
                self.last_send_ns
                    .fetch_max(self.epoch.elapsed().as_nanos() as u64, Ordering::AcqRel);
                return TokenReply::Recall {
                    frame_id: frame.frame_id,
                    objects: frame.inos,
                };
            }
            let left = wait.saturating_sub(started.elapsed());
            if left.is_zero() {
                return TokenReply::Recall {
                    frame_id: 0,
                    objects: Vec::new(),
                };
            }
            let _ = squeezefs_ipc::sqz_time::timeout(left, notified).await;
        }
    }

    fn serve_ack(&self, client: &str, frame_id: u64) -> TokenReply {
        let now = Instant::now();
        self.last_ack_ns.fetch_max(
            now.saturating_duration_since(self.epoch).as_nanos() as u64,
            Ordering::AcqRel,
        );
        let acked = self.lane.ack_frame(client, frame_id, now) as u64;
        self.recall_acks.fetch_add(acked, Ordering::Relaxed);
        self.ack_wake.notify_waiters();
        TokenReply::Acked
    }

    fn serve_release(&self, client: &str, objects: &[u64]) -> TokenReply {
        let mut count = 0u64;
        for &o in objects {
            if self.lane.surrender(o, client) {
                count += 1;
            }
        }
        self.releases.fetch_add(count, Ordering::Relaxed);
        TokenReply::Released { count }
    }

    /// **The commit-path recall** (§5.7.1, the conveyor pass's hook):
    /// recall every outstanding token on `objects` ONCE (the pass's union),
    /// hand the frames to the readers' standing polls, and wait until
    /// every recalled grant is gone — acked, or expired with the reader's
    /// lease. Returns the objects it recalled (the pass marks them in
    /// flight until its apply lands — [`Self::settle`]). One relaxed load
    /// when nothing is delegated.
    pub async fn recall_and_wait(&self, objects: &[u64]) -> Vec<u64> {
        if self.lane.outstanding_now() == 0 || objects.is_empty() {
            return Vec::new();
        }
        let now = Instant::now();
        let mut recalled: Vec<u64> = Vec::new();
        for &o in objects {
            let holders = self.lane.holders(o);
            if holders == 0 {
                continue;
            }
            let _ = self.inflight.insert_sync(o);
            let n = self.lane.recall_object(o, now);
            self.fanout.record(holders);
            self.recalls.fetch_add(n as u64, Ordering::Relaxed);
            recalled.push(o);
        }
        if recalled.is_empty() {
            return recalled;
        }
        self.recall_batches.fetch_add(1, Ordering::Relaxed);
        let t0 = now.saturating_duration_since(self.epoch).as_nanos() as u64;
        // The phase cuts belong to THIS batch: an earlier batch's instants
        // sit below `t0` and clamp to it.
        self.last_send_ns.fetch_max(t0, Ordering::AcqRel);
        self.last_ack_ns.fetch_max(t0, Ordering::AcqRel);
        let cfg = self.lane.config();
        // The expiry sweep's tick: a quarter of the deadline, floored at
        // the timer grain — the wait is woken by acks; the tick only
        // bounds how late a dead reader's expiry is observed.
        let tick = (cfg.deadline / 4).max(Duration::from_millis(1));
        loop {
            // Issue what can be issued (one in-flight frame per client);
            // frames wait for their reader's poll.
            let frames = self.lane.issue_pass(Instant::now());
            if !frames.is_empty() {
                let mut pending = self.pending_frames.lock();
                for f in frames {
                    pending.entry(f.client.clone()).or_default().push_back(f);
                }
                drop(pending);
                self.frame_wake.notify_waiters();
            }
            let notified = self.ack_wake.notified();
            if recalled.iter().all(|o| self.lane.holders(*o) == 0) {
                break;
            }
            let _ = squeezefs_ipc::sqz_time::timeout(tick, notified).await;
            // Frames past their deadline: classify each by the reader's
            // lease (`expired_with_lease` — the token died with the
            // lease; `timeouts_live` — a LIVE member that did not answer,
            // the must-stay-0 stuck-reader class) and let the lane retire
            // the grants; the commit proceeds either way.
            for t in self.lane.expire_overdue(Instant::now()) {
                self.pending_frames.lock().remove(&t.client);
                match self.lease_verdict(&t.client) {
                    LeaseVerdict::Expired => {
                        self.expired_with_lease.fetch_add(1, Ordering::Relaxed);
                    }
                    LeaseVerdict::Live => {
                        self.timeouts_live.fetch_add(1, Ordering::Relaxed);
                        crate::free_grace::note_recall_unacked_live();
                        crate::note_invariant_tripwire(
                            "dlm_token_recall_timeout_live",
                            &format!(
                                "reader '{}' holds a LIVE membership lease and did not ack the \
                                 recall of object {} inside {:?} — a stuck reader; its token is \
                                 retired and the commit proceeds (dlm_token_recall_timeouts_live)",
                                t.client, t.ino, cfg.deadline
                            ),
                        );
                    }
                }
            }
        }
        // `dlm_token_recall_rtt_ns`, EXACT-SUM per batch: `send` = the
        // pass's call → the last frame handed to a reader's poll; `drain`
        // = → the last ack's arrival (the readers' in-flight serve drain +
        // purge + the ack's travel); `ack` = → the pass observing it (the
        // wake hop, or the expiry tick that ended a batch no ack closed).
        // The cuts are clamped monotone into `[t0, t_end]`, so
        // `send + drain + ack ≡ total` to the nanosecond.
        let t_end = self.epoch.elapsed().as_nanos() as u64;
        let t_send = self.last_send_ns.load(Ordering::Acquire).clamp(t0, t_end);
        let t_ack = self
            .last_ack_ns
            .load(Ordering::Acquire)
            .clamp(t_send, t_end);
        self.rtt[RTT_SEND].record(Duration::from_nanos(t_send - t0));
        self.rtt[RTT_DRAIN].record(Duration::from_nanos(t_ack - t_send));
        self.rtt[RTT_ACK].record(Duration::from_nanos(t_end - t_ack));
        self.rtt[RTT_TOTAL].record(Duration::from_nanos(t_end - t0));
        recalled
    }

    /// The recalled commit APPLIED (its records are visible in RAM):
    /// grants on `objects` may be served again.
    pub fn settle(&self, objects: &[u64]) {
        for o in objects {
            let _ = self.inflight.remove_sync(o);
        }
        if !objects.is_empty() {
            self.inflight_done.notify_waiters();
        }
    }

    /// Outstanding `(object, holder)` pairs (the fast probe).
    pub fn outstanding(&self) -> u64 {
        self.lane.outstanding_now()
    }

    /// Holders of `object`.
    pub fn holders(&self, object: u64) -> usize {
        self.lane.holders(object)
    }

    /// The Token family's holder-side snapshot.
    pub fn stats(&self) -> TokenHolderStats {
        TokenHolderStats {
            grants_served: self.grants_served.load(Ordering::Relaxed),
            recalls: self.recalls.load(Ordering::Relaxed),
            recall_acks: self.recall_acks.load(Ordering::Relaxed),
            expired_with_lease: self.expired_with_lease.load(Ordering::Relaxed),
            timeouts_live: self.timeouts_live.load(Ordering::Relaxed),
            releases: self.releases.load(Ordering::Relaxed),
            recall_batches: self.recall_batches.load(Ordering::Relaxed),
            grant_parks: self.grant_parks.load(Ordering::Relaxed),
            outstanding: self.lane.outstanding_now(),
            fanout_p50: self.fanout_percentile(50),
            fanout_p99: self.fanout_percentile(99),
        }
    }

    /// `dlm_token_recall_fanout` p50 / p99 off the log-bucket histogram
    /// (the bucket's upper bound).
    fn fanout_percentile(&self, pct: u64) -> u64 {
        const UPPER: [u64; 15] = [
            0,
            1,
            2,
            4,
            8,
            16,
            32,
            64,
            128,
            256,
            512,
            1024,
            2048,
            4096,
            u64::MAX,
        ];
        let counts: Vec<u64> = self
            .fanout
            .buckets
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return 0;
        }
        let target = (total * pct).div_ceil(100).max(1);
        let mut seen = 0u64;
        for (i, c) in counts.iter().enumerate() {
            seen += c;
            if seen >= target {
                return UPPER[i];
            }
        }
        UPPER[14]
    }

    /// `dlm_token_recall_rtt_ns` (send / drain / ack / total).
    pub fn rtt_json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        for (i, name) in RTT_PHASE_NAMES.iter().enumerate() {
            m.insert((*name).to_string(), self.rtt[i].to_json());
        }
        serde_json::Value::Object(m)
    }

    /// `dlm_token_recall_fanout` (the distribution).
    pub fn fanout_json(&self) -> serde_json::Value {
        self.fanout.to_json()
    }
}

/// The holder-side Token family snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenHolderStats {
    pub grants_served: u64,
    pub recalls: u64,
    pub recall_acks: u64,
    pub expired_with_lease: u64,
    pub timeouts_live: u64,
    pub releases: u64,
    pub recall_batches: u64,
    pub grant_parks: u64,
    pub outstanding: u64,
    pub fanout_p50: u64,
    pub fanout_p99: u64,
}

/// The token service for ONE volume (the verbs executed against its
/// holder plane).
pub struct TokenService {
    volume: Arc<KvMetaBackend>,
}

impl TokenService {
    pub fn new(volume: Arc<KvMetaBackend>) -> Arc<Self> {
        Arc::new(Self { volume })
    }

    fn refuse(id: u64, status: u16, reason: String) -> RpcResponse {
        log::warn!("token service refused a frame: {reason}");
        RpcResponse {
            id,
            status,
            body: reason.into_bytes(),
        }
    }

    async fn serve(&self, req: RpcRequest) -> RpcResponse {
        if req.verb != VERB_TOKEN_CALL {
            return RpcResponse {
                id: req.id,
                status: crate::cluster_wire::RPC_UNKNOWN_VERB,
                body: format!("tokens: unknown verb {}", req.verb).into_bytes(),
            };
        }
        let frame = match decode_request(&req.body) {
            Ok(f) => f,
            Err(e) => return Self::refuse(req.id, STATUS_MALFORMED, e.to_string()),
        };
        self.serve_frame(req.id, frame).await
    }

    async fn serve_frame(&self, req_id: u64, frame: TokenRequestFrame) -> RpcResponse {
        if frame.schema != TOKEN_SCHEMA {
            return Self::refuse(
                req_id,
                STATUS_SCHEMA,
                format!(
                    "peer speaks token vocabulary schema {} and this holder speaks \
                     {TOKEN_SCHEMA}",
                    frame.schema
                ),
            );
        }
        let Some(plane) = self.volume.token_holder() else {
            return Self::refuse(
                req_id,
                STATUS_NOT_HOLDER,
                "this volume serves no read tokens (no armed symmetric plane)".to_string(),
            );
        };
        let reply = match &frame.call {
            TokenCall::Grant {
                object,
                mode: TokenMode::Read,
                wants,
                after,
            } => {
                plane
                    .serve_grant(&self.volume, &frame.client, *object, *wants, *after)
                    .await
            }
            TokenCall::Recall { wait_ms } => {
                let bound = poll_park_bound(plane.lane.config().deadline);
                plane
                    .serve_poll(
                        &frame.client,
                        Duration::from_millis(u64::from(*wait_ms)).min(bound),
                    )
                    .await
            }
            TokenCall::RecallAck { frame_id } => plane.serve_ack(&frame.client, *frame_id),
            TokenCall::Release { objects } => plane.serve_release(&frame.client, objects),
        };
        let status = match reply {
            TokenReply::Refused { .. } => STATUS_REFUSED,
            _ => STATUS_OK,
        };
        let body = match encode_reply(&TokenReplyFrame {
            schema: TOKEN_SCHEMA,
            request_id: frame.request_id,
            reply,
        }) {
            Ok(b) => b,
            Err(e) => return Self::refuse(req_id, STATUS_MALFORMED, format!("reply encode: {e}")),
        };
        log::debug!(
            "token service served {} (request {}) for '{}'",
            frame.call.name(),
            frame.request_id,
            frame.client
        );
        RpcResponse {
            id: req_id,
            status,
            body,
        }
    }
}

impl RpcAsyncService for TokenService {
    fn call<'a>(
        &'a self,
        req: RpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcResponse> + Send + 'a>> {
        Box::pin(self.serve(req))
    }
}

/// The mount path's token service: ONE service on the S8 listener for
/// every volume of the set, dispatching by the frame's volume ordinal (the
/// `ManagerSetService` shape).
pub struct TokenSetService {
    volumes: Vec<Arc<TokenService>>,
}

impl TokenSetService {
    pub fn new(volumes: &[Arc<KvMetaBackend>]) -> Arc<Self> {
        Arc::new(Self {
            volumes: volumes
                .iter()
                .map(|v| TokenService::new(Arc::clone(v)))
                .collect(),
        })
    }

    async fn serve(&self, req: RpcRequest) -> RpcResponse {
        if req.verb != VERB_TOKEN_CALL {
            return RpcResponse {
                id: req.id,
                status: crate::cluster_wire::RPC_UNKNOWN_VERB,
                body: format!("tokens: unknown verb {}", req.verb).into_bytes(),
            };
        }
        let frame = match decode_request(&req.body) {
            Ok(f) => f,
            Err(e) => return TokenService::refuse(req.id, STATUS_MALFORMED, e.to_string()),
        };
        let Some(svc) = self.volumes.get(usize::from(frame.volume)) else {
            return TokenService::refuse(
                req.id,
                STATUS_NOT_HOLDER,
                format!(
                    "token frame names volume ordinal {} on a set of {} volume(s)",
                    frame.volume,
                    self.volumes.len()
                ),
            );
        };
        svc.serve_frame(req.id, frame).await
    }
}

impl RpcAsyncService for TokenSetService {
    fn call<'a>(
        &'a self,
        req: RpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcResponse> + Send + 'a>> {
        Box::pin(self.serve(req))
    }
}

// ---------------------------------------------------------------------------
// The reader
// ---------------------------------------------------------------------------

/// How a reader dials its holder.
#[derive(Debug, Clone)]
pub struct TokenClientConfig {
    pub endpoint: String,
    pub secret: Vec<u8>,
    /// The reader's identity (the KD-MW-2 member id) — the holder's grant
    /// key.
    pub client_id: String,
    pub volume: u16,
}

/// The mount's data-plane half of a recall ack (§5.7.3): drain the
/// reader's in-flight DMA serves on the recalled objects and purge its
/// block-key census — the R-6 purge — BEFORE the ack travels. The mount
/// installs its router-backed sink; the in-process contracts install a
/// probe.
pub trait RecallDataSink: Send + Sync {
    fn drain_and_purge<'a>(
        &'a self,
        objects: &'a [u64],
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// Entry state: serving.
const ENTRY_LIVE: u8 = 0;
/// Entry state: recalled — no new serve begins; in-flight ones drain.
const ENTRY_REVOKED: u8 = 1;

/// One cached token: the object's records as granted.
pub struct TokenEntry {
    pub attrs: InodeValue,
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    /// The directory's entries in cookie order (`None` = not fetched;
    /// a non-directory never has them).
    pub dir: Option<Vec<DirRecord>>,
    state: AtomicU8,
    inflight: AtomicU64,
    drain: squeezefs_ipc::sqz_notify::Notify,
    used: AtomicBool,
}

/// An in-flight serve of one entry (RAII over its inflight count — what
/// the recall drain waits on).
pub struct TokenServe {
    entry: Arc<TokenEntry>,
}

impl TokenServe {
    pub fn entry(&self) -> &TokenEntry {
        &self.entry
    }
}

impl Drop for TokenServe {
    fn drop(&mut self) {
        self.entry.inflight.fetch_sub(1, Ordering::AcqRel);
        self.entry.drain.notify_waiters();
    }
}

/// Bytes charged per cached token (the entry, its `Arc`, the bucket) —
/// the delegation cache's accounting style; the records' own bytes are
/// added per entry.
const TOKEN_ENTRY_BYTES: u64 = 128;

/// Freshness slack over two park rounds (the S10 channel's law).
const CHANNEL_FRESH_SLACK_MS: u64 = 2_000;

/// The reader's side of the token plane for ONE volume.
pub struct TokenReaderPlane {
    cfg: TokenClientConfig,
    session: crate::sqz_sync::SqzMutex<Option<RpcClient>>,
    cache: scc::HashMap<u64, Arc<TokenEntry>>,
    /// Single-flight grants per object.
    fetching: scc::HashMap<u64, Arc<squeezefs_ipc::sqz_notify::Notify>>,
    /// Per-object recall generation — a fetch that spanned a recall of
    /// its object installs nothing and retries.
    revoke_gens: scc::HashMap<u64, u64>,
    channel_ok: AtomicBool,
    channel_last_round_ms: AtomicU64,
    channel_park_ms: AtomicU64,
    epoch: Instant,
    data_sink: std::sync::OnceLock<Arc<dyn RecallDataSink>>,
    stop: AtomicBool,
    /// Test seam: `0` = read the installed membership session, `1` =
    /// declared live, `2` = declared past `T_self`.
    lease_override: AtomicU8,
    cached_bytes: AtomicU64,
    // ---- gauges ----
    grants: AtomicU64,
    hits: AtomicU64,
    recalls_received: AtomicU64,
    recalls_acked: AtomicU64,
    releases: AtomicU64,
    serve_refusals: AtomicU64,
    channel_rounds: AtomicU64,
    fetch_retries: AtomicU64,
    grant_rtt: LatencyHistogram,
}

impl std::fmt::Debug for TokenReaderPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenReaderPlane")
            .field("endpoint", &self.cfg.endpoint)
            .field("cached", &self.cache.len())
            .finish()
    }
}

/// Why a token could not be served (never a stale answer).
fn fail_closed(what: &str) -> SqueezefsError {
    SqueezefsError::Io(std::io::Error::other(format!(
        "read token unavailable: {what} (R-SYM-4: a foreign object is served under a token or \
         not at all)"
    )))
}

impl TokenReaderPlane {
    pub fn new(cfg: TokenClientConfig) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            session: crate::sqz_sync::SqzMutex::new(None),
            cache: scc::HashMap::new(),
            fetching: scc::HashMap::new(),
            revoke_gens: scc::HashMap::new(),
            channel_ok: AtomicBool::new(false),
            channel_last_round_ms: AtomicU64::new(0),
            channel_park_ms: AtomicU64::new(1_000),
            epoch: Instant::now(),
            data_sink: std::sync::OnceLock::new(),
            stop: AtomicBool::new(false),
            lease_override: AtomicU8::new(0),
            cached_bytes: AtomicU64::new(0),
            grants: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            recalls_received: AtomicU64::new(0),
            recalls_acked: AtomicU64::new(0),
            releases: AtomicU64::new(0),
            serve_refusals: AtomicU64::new(0),
            channel_rounds: AtomicU64::new(0),
            fetch_retries: AtomicU64::new(0),
            grant_rtt: LatencyHistogram::default(),
        })
    }

    /// Install the mount's data drain + purge (once).
    pub fn install_data_sink(&self, sink: Arc<dyn RecallDataSink>) -> bool {
        self.data_sink.set(sink).is_ok()
    }

    /// Test seam: declare the reader's lease live (`Some(true)`), past
    /// `T_self` (`Some(false)`), or read the membership session (`None`).
    pub fn test_set_lease_live(&self, live: Option<bool>) {
        self.lease_override.store(
            match live {
                None => 0,
                Some(true) => 1,
                Some(false) => 2,
            },
            Ordering::Relaxed,
        );
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Is the reader's own lease live? Past `T_self` a reader drops every
    /// token (its cache is invalid, exactly as its slot leases would be —
    /// design §13 R20).
    fn lease_live(&self) -> bool {
        match self.lease_override.load(Ordering::Relaxed) {
            1 => return true,
            2 => return false,
            _ => {}
        }
        match crate::membership::installed_member() {
            Some(m) => !m.fenced() && !m.self_fence_due(),
            None => true,
        }
    }

    /// Is the recall channel fresh enough to serve under? A completed
    /// round within two park bounds + slack — a recall issued while we
    /// serve is deliverable within the window.
    fn channel_fresh(&self) -> bool {
        if !self.channel_ok.load(Ordering::Acquire) {
            return false;
        }
        let age = self
            .now_ms()
            .saturating_sub(self.channel_last_round_ms.load(Ordering::Acquire));
        age <= self.channel_park_ms.load(Ordering::Relaxed) * 2 + CHANNEL_FRESH_SLACK_MS
    }

    fn serve_gate(&self) -> Result<()> {
        if !self.lease_live() {
            self.serve_refusals.fetch_add(1, Ordering::Relaxed);
            self.drop_all();
            return Err(fail_closed("this reader's membership lease is past T_self"));
        }
        if !self.channel_fresh() {
            self.serve_refusals.fetch_add(1, Ordering::Relaxed);
            return Err(fail_closed("the recall channel to the holder is not fresh"));
        }
        Ok(())
    }

    async fn call(&self, call: TokenCall) -> Result<TokenReply> {
        let mut guard = self.session.lock().await;
        let client = match guard.as_mut() {
            Some(c) => c,
            None => guard.insert(
                RpcClient::connect(
                    &self.cfg.endpoint,
                    &self.cfg.secret,
                    &self.cfg.client_id,
                    None,
                )
                .await?,
            ),
        };
        match call_on(client, &self.cfg, call).await {
            Ok(r) => Ok(r),
            Err(e) => {
                *guard = None;
                Err(e)
            }
        }
    }

    /// Fetch `object`'s records from the holder — single-flight per
    /// object, dentries paged to completion, retried when a recall of the
    /// object landed mid-fetch.
    async fn fetch(&self, object: u64, wants: TokenWants) -> Result<Option<Arc<TokenEntry>>> {
        loop {
            // Single flight: one fetcher per object; losers wait and
            // re-check the cache.
            let gate = match self.fetching.entry_sync(object) {
                scc::hash_map::Entry::Occupied(o) => {
                    let n = Arc::clone(o.get());
                    drop(o);
                    let notified = n.notified();
                    notified.await;
                    if let Some(e) = self.cache.read_sync(&object, |_, e| Arc::clone(e)) {
                        if !wants.dentries || e.dir.is_some() {
                            return Ok(Some(e));
                        }
                    }
                    continue;
                }
                scc::hash_map::Entry::Vacant(v) => {
                    let n = Arc::new(squeezefs_ipc::sqz_notify::Notify::new());
                    v.insert_entry(Arc::clone(&n));
                    n
                }
            };
            let out = self.fetch_once(object, wants).await;
            let _ = self.fetching.remove_sync(&object);
            gate.notify_waiters();
            match out {
                Ok(FetchOutcome::Installed(e)) => return Ok(Some(e)),
                Ok(FetchOutcome::Gone) => return Ok(None),
                Ok(FetchOutcome::RecalledMidFetch) => {
                    self.fetch_retries.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn fetch_once(&self, object: u64, wants: TokenWants) -> Result<FetchOutcome> {
        let gen0 = self.revoke_gens.read_sync(&object, |_, g| *g).unwrap_or(0);
        let t0 = Instant::now();
        let mut after = 0u64;
        let mut attrs: Option<WireAttrs> = None;
        let mut xattrs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut entries: Vec<DirRecord> = Vec::new();
        loop {
            let reply = self
                .call(TokenCall::Grant {
                    object,
                    mode: TokenMode::Read,
                    wants,
                    after,
                })
                .await?;
            match reply {
                TokenReply::Granted { records, .. } => {
                    self.grants.fetch_add(1, Ordering::Relaxed);
                    if attrs.is_none() {
                        attrs = Some(records.attrs);
                        xattrs = records.xattrs;
                    }
                    match records.dir {
                        Some((page, complete)) => {
                            after = page.last().map_or(after, |d| d.cookie);
                            entries.extend(page);
                            if complete {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                TokenReply::Gone => return Ok(FetchOutcome::Gone),
                TokenReply::NotHolder { holder } => {
                    return Err(fail_closed(&format!(
                        "object {object}'s slot is held by appender {holder} (the redirect to a \
                         second holder is PR 12's join ladder)"
                    )));
                }
                TokenReply::Refused { reason } => return Err(fail_closed(&reason)),
                other => {
                    return Err(fail_closed(&format!(
                        "the holder answered a Grant with {other:?}"
                    )))
                }
            }
        }
        self.grant_rtt.record(t0.elapsed());
        let gen1 = self.revoke_gens.read_sync(&object, |_, g| *g).unwrap_or(0);
        if gen1 != gen0 {
            // A recall landed inside the fetch: the pages may straddle
            // the mutation. Nothing installed.
            return Ok(FetchOutcome::RecalledMidFetch);
        }
        let attrs = attrs.ok_or_else(|| fail_closed("a grant carried no attrs"))?;
        let is_dir = (attrs.mode & libc::S_IFMT) == libc::S_IFDIR;
        let bytes = TOKEN_ENTRY_BYTES
            + xattrs
                .iter()
                .map(|(n, v)| (n.len() + v.len()) as u64)
                .sum::<u64>()
            + entries
                .iter()
                .map(|d| 24 + d.name.len() as u64)
                .sum::<u64>();
        let entry = Arc::new(TokenEntry {
            attrs: attrs.into(),
            xattrs,
            dir: (wants.dentries && is_dir).then_some(entries),
            state: AtomicU8::new(ENTRY_LIVE),
            inflight: AtomicU64::new(0),
            drain: squeezefs_ipc::sqz_notify::Notify::new(),
            used: AtomicBool::new(true),
        });
        let retired = self.evict_to_cap();
        if !retired.is_empty() {
            // Voluntary releases (best effort — a lost release costs the
            // holder one needless recall, never a wrong answer).
            let _ = self.call(TokenCall::Release { objects: retired }).await;
        }
        match self.cache.entry_sync(object) {
            scc::hash_map::Entry::Occupied(mut o) => {
                let prev = o.get().clone();
                // A concurrent full fetch may have installed a richer
                // entry; a dentry-bearing one is never displaced by a
                // bare one.
                if prev.dir.is_some() && entry.dir.is_none() {
                    return Ok(FetchOutcome::Installed(prev));
                }
                *o.get_mut() = Arc::clone(&entry);
            }
            scc::hash_map::Entry::Vacant(v) => {
                v.insert_entry(Arc::clone(&entry));
            }
        }
        self.cached_bytes.fetch_add(bytes, Ordering::Relaxed);
        Ok(FetchOutcome::Installed(entry))
    }

    /// The token cache's cap: the delegation cache's derivation verbatim
    /// (one R5 law, not two); past it one second-chance sweep — every
    /// retired entry is a voluntary release.
    fn evict_to_cap(&self) -> Vec<u64> {
        if self.cache.len() < super::tokens::cache_cap() {
            return Vec::new();
        }
        let mut retired: Vec<u64> = Vec::new();
        self.cache.retain_sync(|ino, e| {
            if e.used.swap(false, Ordering::Relaxed) {
                true
            } else {
                e.state.store(ENTRY_REVOKED, Ordering::Release);
                retired.push(*ino);
                false
            }
        });
        self.releases
            .fetch_add(retired.len() as u64, Ordering::Relaxed);
        retired
    }

    /// Begin a serve of `object`: the cached entry under the serve gate,
    /// fetched on a miss (or when dentries are wanted and not yet held).
    pub async fn serve(&self, object: u64, wants: TokenWants) -> Result<Option<TokenServe>> {
        self.serve_gate()?;
        loop {
            if let Some(entry) = self.cache.read_sync(&object, |_, e| Arc::clone(e)) {
                if entry.state.load(Ordering::Acquire) == ENTRY_LIVE
                    && (!wants.dentries || entry.dir.is_some())
                {
                    entry.used.store(true, Ordering::Relaxed);
                    entry.inflight.fetch_add(1, Ordering::AcqRel);
                    if entry.state.load(Ordering::Acquire) != ENTRY_LIVE {
                        entry.inflight.fetch_sub(1, Ordering::AcqRel);
                        entry.drain.notify_waiters();
                        continue;
                    }
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(Some(TokenServe { entry }));
                }
            }
            let Some(entry) = self.fetch(object, wants).await? else {
                return Ok(None);
            };
            entry.inflight.fetch_add(1, Ordering::AcqRel);
            if entry.state.load(Ordering::Acquire) != ENTRY_LIVE {
                entry.inflight.fetch_sub(1, Ordering::AcqRel);
                entry.drain.notify_waiters();
                continue;
            }
            return Ok(Some(TokenServe { entry }));
        }
    }

    /// Drop every entry (the lease is gone / the channel died): serves
    /// stop now; in-flight ones complete under the entry they hold.
    fn drop_all(&self) {
        self.cache.retain_sync(|_, e| {
            e.state.store(ENTRY_REVOKED, Ordering::Release);
            false
        });
        self.cached_bytes.store(0, Ordering::Relaxed);
    }

    /// The standing recall channel — run by the reader's task until
    /// `stop`. Its own session: the poll parks at the holder, and a
    /// parked call must never block a grant.
    pub async fn run_recall_channel(self: Arc<Self>) {
        let mut session: Option<RpcClient> = None;
        let mut backoff = Duration::from_millis(50);
        while !self.stop.load(Ordering::Relaxed) {
            let client = match session.as_mut() {
                Some(c) => c,
                None => match RpcClient::connect(
                    &self.cfg.endpoint,
                    &self.cfg.secret,
                    &self.cfg.client_id,
                    None,
                )
                .await
                {
                    Ok(c) => {
                        backoff = Duration::from_millis(50);
                        session.insert(c)
                    }
                    Err(e) => {
                        log::warn!(
                            "token recall channel to {} could not connect: {e} (retry in {:?})",
                            self.cfg.endpoint,
                            backoff
                        );
                        self.channel_ok.store(false, Ordering::Release);
                        self.drop_all();
                        squeezefs_ipc::sqz_time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(5));
                        continue;
                    }
                },
            };
            let wait_ms = self.channel_park_ms.load(Ordering::Relaxed) as u32;
            match call_on(client, &self.cfg, TokenCall::Recall { wait_ms }).await {
                Ok(TokenReply::Recall { frame_id, objects }) => {
                    self.channel_last_round_ms
                        .store(self.now_ms(), Ordering::Release);
                    self.channel_ok.store(true, Ordering::Release);
                    self.channel_rounds.fetch_add(1, Ordering::Relaxed);
                    if frame_id != 0 && !objects.is_empty() {
                        // The ack rides the SAME session (the S10 law:
                        // the next call on the channel carries the acks).
                        if let Err(e) = self.handle_recall_on(client, frame_id, &objects).await {
                            log::warn!(
                                "token recall {frame_id} could not be acked: {e} — the channel \
                                 reconnects; the holder's deadline retires the grants"
                            );
                            session = None;
                            self.channel_ok.store(false, Ordering::Release);
                            self.drop_all();
                        }
                    }
                }
                Ok(other) => {
                    log::warn!("token recall channel answered {other:?}; reconnecting");
                    session = None;
                    self.channel_ok.store(false, Ordering::Release);
                    self.drop_all();
                }
                Err(e) => {
                    log::warn!(
                        "token recall channel to {} failed: {e} — every token is dropped \
                         (fail-closed) until a round completes",
                        self.cfg.endpoint
                    );
                    session = None;
                    self.channel_ok.store(false, Ordering::Release);
                    self.drop_all();
                    squeezefs_ipc::sqz_time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
        }
    }

    /// [`Self::handle_recall`] with the ack on the channel's own session.
    async fn handle_recall_on(
        &self,
        client: &mut RpcClient,
        frame_id: u64,
        objects: &[u64],
    ) -> Result<()> {
        self.recalls_received
            .fetch_add(objects.len() as u64, Ordering::Relaxed);
        for &o in objects {
            match self.revoke_gens.entry_sync(o) {
                scc::hash_map::Entry::Occupied(mut e) => *e.get_mut() += 1,
                scc::hash_map::Entry::Vacant(v) => {
                    v.insert_entry(1);
                }
            }
            let Some(entry) = self.cache.remove_sync(&o).map(|(_, e)| e) else {
                continue;
            };
            entry.state.store(ENTRY_REVOKED, Ordering::Release);
            loop {
                let notified = entry.drain.notified();
                if entry.inflight.load(Ordering::Acquire) == 0 {
                    break;
                }
                notified.await;
            }
        }
        if let Some(sink) = self.data_sink.get() {
            sink.drain_and_purge(objects).await;
        }
        match call_on(client, &self.cfg, TokenCall::RecallAck { frame_id }).await? {
            TokenReply::Acked => {
                self.recalls_acked
                    .fetch_add(objects.len() as u64, Ordering::Relaxed);
                Ok(())
            }
            other => Err(fail_closed(&format!(
                "the holder answered a RecallAck with {other:?}"
            ))),
        }
    }

    /// The clean leave: RELEASE every held token at the holder (a
    /// voluntary release — the holder's next commit on those objects
    /// recalls nobody), then stop the channel task and drop the cache. A
    /// reader that dies without this leaves its grants to the holder's
    /// lease-expiry arm.
    pub async fn stop(&self) {
        let mut held: Vec<u64> = Vec::new();
        self.cache.retain_sync(|ino, _| {
            held.push(*ino);
            true
        });
        if !held.is_empty() && self.channel_ok.load(Ordering::Acquire) {
            self.releases
                .fetch_add(held.len() as u64, Ordering::Relaxed);
            let _ = self.call(TokenCall::Release { objects: held }).await;
        }
        self.stop.store(true, Ordering::Relaxed);
        self.drop_all();
    }

    /// **Test seam**: the reader DIES — its channel task stops without a
    /// release (its grants stay at the holder until the lease bound),
    /// the shape the lease-expiry contracts need.
    pub fn test_die(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.channel_ok.store(false, Ordering::Release);
    }

    /// The reader-side Token family snapshot.
    pub fn stats(&self) -> TokenReaderStats {
        TokenReaderStats {
            grants: self.grants.load(Ordering::Relaxed),
            cached: self.cache.len() as u64,
            hits: self.hits.load(Ordering::Relaxed),
            recalls_received: self.recalls_received.load(Ordering::Relaxed),
            recalls_acked: self.recalls_acked.load(Ordering::Relaxed),
            releases: self.releases.load(Ordering::Relaxed),
            serve_refusals: self.serve_refusals.load(Ordering::Relaxed),
            channel_rounds: self.channel_rounds.load(Ordering::Relaxed),
            fetch_retries: self.fetch_retries.load(Ordering::Relaxed),
            channel_fresh: self.channel_fresh(),
        }
    }

    /// `dlm_token_grant_rtt_ns` (one fetch to completion).
    pub fn grant_rtt_json(&self) -> serde_json::Value {
        self.grant_rtt.to_json()
    }

    /// Is `object` cached LIVE (the contracts' probe)?
    pub fn holds(&self, object: u64) -> bool {
        self.cache
            .read_sync(&object, |_, e| {
                e.state.load(Ordering::Acquire) == ENTRY_LIVE
            })
            .unwrap_or(false)
    }
}

enum FetchOutcome {
    Installed(Arc<TokenEntry>),
    Gone,
    RecalledMidFetch,
}

/// The reader-side Token family snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenReaderStats {
    pub grants: u64,
    pub cached: u64,
    pub hits: u64,
    pub recalls_received: u64,
    pub recalls_acked: u64,
    pub releases: u64,
    pub serve_refusals: u64,
    pub channel_rounds: u64,
    pub fetch_retries: u64,
    pub channel_fresh: bool,
}

/// Issue one verb on `client`; a refusal status with a reply body is
/// decoded (the reply carries the reason), any other status is an error.
async fn call_on(
    client: &mut RpcClient,
    cfg: &TokenClientConfig,
    call: TokenCall,
) -> Result<TokenReply> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let request_id = NEXT.fetch_add(1, Ordering::Relaxed);
    let body = encode_request(&TokenRequestFrame {
        schema: TOKEN_SCHEMA,
        request_id,
        volume: cfg.volume,
        client: cfg.client_id.clone(),
        call,
    })?;
    let resp = client.call(VERB_TOKEN_CALL, body).await?;
    match resp.status {
        STATUS_OK | STATUS_REFUSED => {}
        other => {
            return Err(SqueezefsError::InvalidOperation(format!(
                "token holder refused the frame with status {other}: {}",
                String::from_utf8_lossy(&resp.body)
            )));
        }
    }
    let frame = decode_reply(&resp.body)?;
    if frame.schema != TOKEN_SCHEMA {
        return Err(SqueezefsError::InvalidOperation(format!(
            "token holder answered in vocabulary schema {} (ours is {TOKEN_SCHEMA})",
            frame.schema
        )));
    }
    if frame.request_id != request_id {
        return Err(SqueezefsError::InvalidOperation(format!(
            "token reply echoes request {} for request {request_id}",
            frame.request_id
        )));
    }
    Ok(frame.reply)
}

// ---------------------------------------------------------------------------
// The reader's serve shapes (the KvMetaBackend read verbs' token arms)
// ---------------------------------------------------------------------------

/// A dentry as the reader's `lookup` needs it.
pub fn dentry_of(rec: &DirRecord) -> DentryValue {
    DentryValue {
        child_ino: rec.child_ino,
        file_type: rec.file_type,
        name: rec.name.clone(),
    }
}

/// The reader's `find_dentry`: the parent's token (dentries) searched by
/// name. `Ok(None)` = the parent exists and has no such name; a missing
/// parent is `NotFound`.
pub async fn token_find_dentry(
    plane: &TokenReaderPlane,
    parent: Ino,
    name: &str,
) -> Result<Option<DentryValue>> {
    let Some(serve) = plane.serve(parent, TokenWants { dentries: true }).await? else {
        return Err(SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Inode {parent} not found"),
        )));
    };
    let entry = serve.entry();
    let Some(dir) = entry.dir.as_ref() else {
        return Ok(None);
    };
    Ok(dir
        .iter()
        .find(|d| d.name == name.as_bytes())
        .map(dentry_of))
}

// ---------------------------------------------------------------------------
// The mount path's arms
// ---------------------------------------------------------------------------

/// The FUSE mount's [`RecallDataSink`]: an epoch step on the reader's
/// layout cache + the observed in-flight serve drain
/// (`ro_coherence::drain_in_flight_serves`), then the R-6 purge of the
/// block-key census (`ro_coherence::purge_reader_block_keys`) — the whole
/// census, the ONE legal purge, so the ack is never emitted while a
/// cached block of the recalled object could still serve.
pub struct MountRecallSink {
    router: crate::routing::DataRouter,
}

impl MountRecallSink {
    pub fn new(router: crate::routing::DataRouter) -> Arc<Self> {
        Arc::new(Self { router })
    }
}

impl RecallDataSink for MountRecallSink {
    fn drain_and_purge<'a>(
        &'a self,
        objects: &'a [u64],
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            crate::ro_coherence::drain_in_flight_serves().await;
            let purged = crate::ro_coherence::purge_reader_block_keys(&self.router.cache);
            log::debug!(
                "token recall of {} object(s): in-flight serves drained, {purged} cached block \
                 key(s) purged before the ack",
                objects.len()
            );
        })
    }
}

/// `dlm_token_*` — the Token family for the stats inode, per volume.
pub fn holder_stats_json(volumes: &[Arc<KvMetaBackend>]) -> serde_json::Value {
    let per = |f: &dyn Fn(&TokenHolderStats) -> u64| {
        serde_json::Value::Array(
            volumes
                .iter()
                .map(|v| v.token_holder().map_or(0, |p| f(&p.stats())).into())
                .collect(),
        )
    };
    serde_json::json!({
        "dlm_token_grants_served": per(&|s| s.grants_served),
        "dlm_token_recalls": per(&|s| s.recalls),
        "dlm_token_recall_acks": per(&|s| s.recall_acks),
        "dlm_token_recall_expired_with_lease": per(&|s| s.expired_with_lease),
        "dlm_token_recall_timeouts_live": per(&|s| s.timeouts_live),
        "dlm_token_releases": per(&|s| s.releases),
        "dlm_token_recall_batches": per(&|s| s.recall_batches),
        "dlm_token_grant_parks": per(&|s| s.grant_parks),
        "dlm_token_outstanding": per(&|s| s.outstanding),
        "dlm_token_recall_fanout_p50": per(&|s| s.fanout_p50),
        "dlm_token_recall_fanout_p99": per(&|s| s.fanout_p99),
        "dlm_token_recall_fanout": serde_json::Value::Array(
            volumes
                .iter()
                .map(|v| v.token_holder().map_or(serde_json::Value::Null, |p| p.fanout_json()))
                .collect(),
        ),
        "dlm_token_recall_rtt_ns": serde_json::Value::Array(
            volumes
                .iter()
                .map(|v| v.token_holder().map_or(serde_json::Value::Null, |p| p.rtt_json()))
                .collect(),
        ),
    })
}

/// The reader-side Token family for the stats inode, per volume.
pub fn reader_stats_json(volumes: &[Arc<KvMetaBackend>]) -> serde_json::Value {
    let per = |f: &dyn Fn(&TokenReaderStats) -> u64| {
        serde_json::Value::Array(
            volumes
                .iter()
                .map(|v| v.token_reader().map_or(0, |p| f(&p.stats())).into())
                .collect(),
        )
    };
    serde_json::json!({
        "dlm_token_grants": per(&|s| s.grants),
        "dlm_token_cached": per(&|s| s.cached),
        "dlm_token_hits": per(&|s| s.hits),
        "dlm_token_recalls_received": per(&|s| s.recalls_received),
        "dlm_token_recalls_acked": per(&|s| s.recalls_acked),
        "dlm_token_reader_releases": per(&|s| s.releases),
        "dlm_token_serve_refusals": per(&|s| s.serve_refusals),
        "dlm_token_channel_rounds": per(&|s| s.channel_rounds),
        "dlm_token_fetch_retries": per(&|s| s.fetch_retries),
        "dlm_token_channel_fresh": per(&|s| u64::from(s.channel_fresh)),
        "dlm_token_grant_rtt_ns": serde_json::Value::Array(
            volumes
                .iter()
                .map(|v| v.token_reader().map_or(serde_json::Value::Null, |p| p.grant_rtt_json()))
                .collect(),
        ),
    })
}

/// A set of objects with their union taken (the pass's recall set).
pub fn union_objects(objects: impl IntoIterator<Item = u64>) -> Vec<u64> {
    let set: HashSet<u64> = objects.into_iter().collect();
    let mut v: Vec<u64> = set.into_iter().collect();
    v.sort_unstable();
    v
}
