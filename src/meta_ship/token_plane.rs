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
/// Frame status (PR 9, review round 2 — Issue 3): a wire word REJECTED at
/// the service edge before any effect — the buggy/hostile-peer class the
/// manager wire's `STATUS_REJECTED` names (`CustodyGrant.object` past the
/// forest codec's bound, a slot this forest never minted, a control
/// record, an ino with no durable record, a malformed span). The body
/// carries [`TokenReply::Rejected`]; counted `dlm_token_custody_rejected`
/// (must-stay-0 on a healthy fleet), never on a refusal gauge.
pub const STATUS_REJECTED: u16 = 57;

/// The token modes on the wire: `Read` is shared (N holders); the
/// holder's `Write` is implicit — the lessee needs no token on its own
/// tree (§5.7.1) — and never travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenMode {
    Read,
}

/// What a grant page must carry: the object's records (attrs — always
/// carried, they are one fixed word — and the xattrs, paged by name
/// under the grant budget; `records` is set on the reader's first page
/// and every xattr continuation, clear on a pure dentry continuation, so
/// no page re-carries what an earlier one did — review round 1, Issue
/// 16), and a directory's dentry set, paged by cookie under the same
/// budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenWants {
    pub dentries: bool,
    pub records: bool,
}

impl Default for TokenWants {
    /// The first page's wants: the records, no dentries.
    fn default() -> Self {
        Self {
            dentries: false,
            records: true,
        }
    }
}

impl TokenWants {
    /// The first page of a directory grant: records + dentries.
    pub fn with_dentries() -> Self {
        Self {
            dentries: true,
            records: true,
        }
    }
}

/// The token verbs (§6.3 — "the S10 delegation verbs": Grant / Recall /
/// RecallAck / Release). The variant ORDER is the wire index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenCall {
    /// A read token on `object` (a LOCAL key ino of the frame's volume),
    /// the records carried; `after` = the dentry continuation cookie (0 =
    /// the directory's start) when `wants.dentries`; `xattr_after` = the
    /// xattr continuation (the last name carried; empty = from the first
    /// name) when `wants.records`. Idempotent: a holder that already
    /// granted this client the token answers `already`.
    Grant {
        object: u64,
        mode: TokenMode,
        wants: TokenWants,
        after: u64,
        xattr_after: Vec<u8>,
    },
    /// The reader's standing recall channel: the holder PARKS the call
    /// until a recall frame for this client exists (or `wait_ms`
    /// elapses) and answers [`TokenReply::Recall`].
    Recall { wait_ms: u32 },
    /// The reader drained and purged every object of `frame_id`.
    RecallAck { frame_id: u64 },
    /// Voluntary release (the reader's eviction).
    Release { objects: Vec<u64> },
    // PR 9 — custody by the slot holder (design-symmetric-metadata §5.5
    // the "S9 custody endpoint" row, §5.1.5): the S9 write-custody ACQUIRE
    // of `object` (a LOCAL key ino of the frame's volume) served by its
    // slot HOLDER in ONE round trip that also carries the file's read
    // token — the S9 arbitration under the caller's custody lease at this
    // holder (`lease_epoch`, its JOIN's), then the records
    // (`TokenWants::default()`: the attrs + the carried xattrs, `layout`
    // among them). The holder's later commit on the object recalls the
    // token like any reader's. Never retried on a transport failure (the
    // S9 acquire's law — a re-sent acquire could strand a grant the caller
    // cannot name).
    CustodyGrant {
        object: u64,
        span: Option<(u64, u64)>,
        concurrent_write: bool,
        wait_ms: u64,
        lease_epoch: u64,
    },
}

impl TokenCall {
    /// The verb's name (logs).
    pub fn name(&self) -> &'static str {
        match self {
            TokenCall::Grant { .. } => "Grant",
            TokenCall::Recall { .. } => "Recall",
            TokenCall::RecallAck { .. } => "RecallAck",
            TokenCall::Release { .. } => "Release",
            TokenCall::CustodyGrant { .. } => "CustodyGrant",
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

/// The records one grant PAGE carries (Lustre's intent lock): attrs, a
/// page of the user-visible xattrs (by name, `xattrs_complete` when it
/// ended the set; empty on a page that did not ask for records), and —
/// when asked, once the xattrs are complete — a page of the directory's
/// entries with a completion flag. One byte budget covers the xattrs
/// and the dentries of a page ([`grant_dentry_budget`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRecords {
    pub attrs: WireAttrs,
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    pub xattrs_complete: bool,
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
    // PR 9 — the slot holder's answers to `CustodyGrant`.
    /// Custody granted by the slot holder (the S9 grant record — the
    /// caller adopts it exactly as an authority's), the object's records
    /// carried beside it. `already` is the TOKEN's word (the grant ∥ pass
    /// gate's re-registration of a token this client already held on the
    /// object), never custody's: a custody grant is minted fresh at every
    /// acquire (the S9 arbiter answers `CustodyRefused` where it cannot).
    CustodyGranted {
        grant: crate::data_grant::GrantRecord,
        records: TokenRecords,
        already: bool,
    },
    /// The S9 arbitration refused: `status` is the custody wire's own
    /// status word (`CUSTODY_CONFLICT` / `CUSTODY_UNKNOWN_LEASE` /
    /// `CUSTODY_DEFERRED` / …), so the caller runs the S9 client's exact
    /// refusal ladder.
    CustodyRefused {
        status: u16,
        reason: String,
    },
    /// Review round 2, Issue 3: a wire word REJECTED at the service edge
    /// ([`STATUS_REJECTED`]) — nothing was granted, registered or read.
    Rejected {
        reason: String,
    },
    /// PR 12b round 3, F2: the object's slot is leased to an appender the
    /// S6 owner no longer lists LIVE — its slots are the recovery's within
    /// the ledger poll; the client is NOT redirected to an address nobody
    /// answers at, it retries (EAGAIN) and reads the recovered tree here.
    HolderDead {
        holder: u32,
    },
    /// PR 13b (record §4.4ag): the caller holds no LIVE membership lease
    /// with this holder's owner — a read token is granted to members only
    /// (PR 5 review round 3, Issue 27) — the TYPED word of the dispatch's
    /// membership screen: inside a manager failover a joiner's lease is
    /// re-asserted at the successor a beat after its reads resume, and the
    /// client PARKS on its own reclaim and retries
    /// (`membership::await_grant_adopted`) instead of surfacing `EIO`.
    NotMember,
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
    /// The membership owner sees the reader's lease EXPIRED: its tokens
    /// died with it — swept at once, no wait.
    Expired,
    /// No lease to read (no membership plane, or a client the owner never
    /// granted): recalled and waited for like a live reader, and at the
    /// deadline its token's expiry IS the derived bound (counted with the
    /// lease, never the stuck-reader class).
    Unknown,
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

/// Test seam: the token dispatch parks this many ms between its
/// membership check and the grant's registration (review round 4, Issue
/// 29 — an eviction inside that window must leave the grant judged
/// `Expired`, never `Unknown`).
pub static TEST_DISPATCH_HOLD_AFTER_CHECK_MS: AtomicU64 = AtomicU64::new(0);

/// PR 9: the words of one `CustodyGrant` as the holder serves them.
struct CustodyAsk {
    object: u64,
    span: Option<(u64, u64)>,
    concurrent_write: bool,
    wait_ms: u64,
    lease_epoch: u64,
}

/// Test seam (PR 9, review round 2 — Issue 8's pin): the NEXT carried
/// install fails after custody was granted, so the contract can see the
/// caller keep the lease it was granted.
pub static TEST_INSTALL_CARRIED_FAIL_ONCE: AtomicBool = AtomicBool::new(false);

/// Test seam (PR 9, review round 2 — Issue 15's pin): the NEXT
/// `CustodyGrant` is answered `NotHolder { holder }` with this appender
/// id (`u64::MAX` = off) — the cross-process shape where the holder's
/// tree 0 moved the slot under the writer's view, which one process's
/// shared tree 0 cannot produce.
pub static TEST_CUSTODY_NOT_HOLDER_ONCE: AtomicU64 = AtomicU64::new(u64::MAX);

/// **PR 9, review round 2 (Issue 3) — the pure screen of a `CustodyGrant`
/// frame's words** (PR 3's bounded-execution law: every wire-carried
/// integer is validated against durable / derived state BEFORE anything
/// acts on it). `object` is the peer's LOCAL key ino; `forest_names`
/// answers the ROUTING slot of a forest slot this volume's forest names
/// (`None` = never minted here). Returns the object's `(routing slot,
/// raw local ino)` — the GLOBAL ino's two words — or the rejection's
/// reason. Fuzzed by `token_call_frame`'s service-edge arm + the proptest
/// mirror: total over every `u64`, never a panic (the forest codec's
/// `split_guest_local` `debug_assert`s the slot word — what a peer word
/// reached before this screen existed).
pub fn screen_custody_words(
    object: u64,
    span: Option<(u64, u64)>,
    forest_names: &dyn Fn(crate::meta_backend::kv::record::ForestSlot) -> Option<u16>,
) -> std::result::Result<(u16, u64), String> {
    use crate::meta_backend::kv::record::{
        forest_slot_of_ino, FOREST_SLOT_MAX, NATIVE_FOREST_SLOT,
    };
    let forest_slot = forest_slot_of_ino(object);
    if forest_slot > FOREST_SLOT_MAX {
        return Err(format!(
            "object {object:#x} names forest slot {forest_slot}, past the codec's bound \
             {FOREST_SLOT_MAX}"
        ));
    }
    let raw = if forest_slot == NATIVE_FOREST_SLOT {
        object
    } else {
        object & ((1u64 << crate::meta_backend::GUEST_NS_SHIFT) - 1)
    };
    if raw < 2 {
        return Err(format!(
            "object {object:#x} is a control record (raw local ino {raw} has no global \
             encoding) — nothing to hold custody of"
        ));
    }
    let Some(routing) = forest_names(forest_slot) else {
        return Err(format!(
            "object {object:#x} names forest slot {forest_slot}, which this volume's forest \
             never minted"
        ));
    };
    if let Some((start, end)) = span {
        if start >= end {
            return Err(format!(
                "span [{start}, {end}) is malformed — spans are [start, end) with end \
                 EXCLUSIVE and non-empty"
            ));
        }
    }
    Ok((routing, raw))
}

/// [`screen_custody_words`] bound to `volume`, then the DURABLE record:
/// the object's inode record must exist (one leaf read — the grant reads
/// it again a moment later through the node cache) AND be a REGULAR FILE
/// before the arbiter is handed the GLOBAL ino (the key every write of
/// the file takes in the lock table). Write custody is the data plane's
/// word — the FUSE layer acquires it for WRITE / truncate / fallocate /
/// a dirty handle's flush and fsync, every one a file's op — so a
/// directory is never a custody object: not a striped directory, not one
/// of its stripe inos, not the targets of its reserved-name markers (the
/// PR 7b rebase's seam (c)); a frame naming one is REJECTED like any
/// wire word the durable state contradicts. `Err(reason)` = REJECTED,
/// nothing acted.
async fn screen_custody_object(
    volume: &KvMetaBackend,
    object: u64,
    span: Option<(u64, u64)>,
) -> std::result::Result<u64, String> {
    let (routing, raw) = screen_custody_words(object, span, &|forest_slot| {
        let routing = volume.routing_slot_of_forest(forest_slot).ok()?;
        // The native slot is always this volume's; a guest slot must have
        // a tree here (the forest names a slot at its first record).
        (forest_slot == crate::meta_backend::kv::record::NATIVE_FOREST_SLOT
            || volume.slot_tree(forest_slot).is_some())
        .then_some(routing)
    })?;
    match volume.token_record_mode(object).await {
        Ok(Some(mode)) if custody_object_mode_admissible(mode) => {}
        Ok(Some(mode)) => {
            return Err(format!(
                "object {object:#x} is not a regular file (mode {mode:#o}) — a directory, a \
                 stripe ino or a marker target is never a custody object"
            ))
        }
        Ok(None) => {
            return Err(format!(
                "object {object:#x} has no durable inode record on this volume"
            ))
        }
        Err(e) => return Err(format!("object {object:#x}: the record read failed: {e}")),
    }
    Ok(crate::meta_backend::make_global_ino_width(
        raw,
        u64::from(routing),
        crate::dlm_slot::routing_width(),
    ))
}

/// **The custody screen's type rule** (pure — the fuzz target and the
/// proptest mirror drive it): a custody object is a REGULAR FILE. Write
/// custody guards a file's DMA; a directory takes none (its mutations are
/// the metadata plane's 4a guards), so a frame naming one — a striped
/// directory, a stripe ino, a marker's target — is the wire-word class
/// the durable state refutes.
pub fn custody_object_mode_admissible(mode: u32) -> bool {
    (mode & CUSTODY_MODE_TYPE_MASK) == CUSTODY_MODE_REGULAR_FILE
}

/// `S_IFMT` / `S_IFREG` as the rule reads them — named here so the fuzz
/// mirror (a workspace without `libc`) states the law in the same words.
pub const CUSTODY_MODE_TYPE_MASK: u32 = libc::S_IFMT;
pub const CUSTODY_MODE_REGULAR_FILE: u32 = libc::S_IFREG;

/// The token CLIENTS this process's holder planes have served — every
/// member id that reached a token verb. `free_grace`'s recall-gated free
/// reads it against the membership census: a live `Reader` member NOT in
/// this set is an S5 reader, whose freed-offset protection is the ring's
/// epoch law (review round 1, Issue 3). Process-global because one
/// writer process holds every volume of its set. BOUNDED by the
/// membership census: a member's id leaves the set at its departure
/// (`note_member_departed`, after its grants are swept — review round 3,
/// Issue 28), so churning reader identities over a long-lived writer
/// never grow it past the live population.
static TOKEN_CLIENTS: once_cell::sync::Lazy<scc::HashSet<String>> =
    once_cell::sync::Lazy::new(scc::HashSet::new);
/// Bumped when a NEW token client is noted (review round 2, Issue 22:
/// the recall gate's reader-class verdict caches against it, beside the
/// membership census generation).
static TOKEN_CLIENTS_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Record `client` as a token client of this holder.
pub fn note_token_client(client: &str) {
    if !TOKEN_CLIENTS.contains_sync(client) && TOKEN_CLIENTS.insert_sync(client.to_string()).is_ok()
    {
        TOKEN_CLIENTS_GENERATION.fetch_add(1, Ordering::Release);
    }
}

/// Is `client` a token client of this holder?
pub fn is_token_client(client: &str) -> bool {
    TOKEN_CLIENTS.contains_sync(client)
}

/// The token-client registry's generation (monotone).
pub fn token_clients_generation() -> u64 {
    TOKEN_CLIENTS_GENERATION.load(Ordering::Acquire)
}

/// Test seam: forget every token client (a fresh holder).
pub fn test_clear_token_clients() {
    TOKEN_CLIENTS.clear_sync();
    TOKEN_CLIENTS_GENERATION.fetch_add(1, Ordering::Release);
}

/// Every armed holder plane in the process (weak — a dropped backend
/// drops its plane): the membership departure sink's fan-out.
static TOKEN_HOLDER_PLANES: once_cell::sync::Lazy<
    parking_lot::Mutex<Vec<std::sync::Weak<TokenHolderPlane>>>,
> = once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(Vec::new()));

static DEPARTURE_SINK_INSTALLED: std::sync::Once = std::sync::Once::new();

/// Register an armed holder for the departure sweep and install the
/// membership departure sink once per process (review round 2, Issue 5):
/// a member the owner EVICTS (its cadence sweep) or that LEAVES has its
/// grants retired at that instant on every holder — a pass parked on its
/// recall wakes and completes there, never at the recall deadline.
pub fn register_holder(plane: &Arc<TokenHolderPlane>) {
    {
        let mut planes = TOKEN_HOLDER_PLANES.lock();
        planes.retain(|w| w.strong_count() > 0);
        planes.push(Arc::downgrade(plane));
    }
    DEPARTURE_SINK_INSTALLED.call_once(|| {
        crate::membership::install_departure_sink(Arc::new(note_member_departed));
    });
}

/// The membership departure sink: sweep `client`'s grants on every
/// armed holder (a member that is not a token client holds none), then
/// forget it as a token client — the registry stays bounded by the
/// census, and a verb from the departed id meets the dispatch's
/// membership check like any stranger's.
fn note_member_departed(client: &str) {
    if !is_token_client(client) {
        return;
    }
    let planes: Vec<Arc<TokenHolderPlane>> = TOKEN_HOLDER_PLANES
        .lock()
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .collect();
    for plane in planes {
        plane.sweep_departed(client);
    }
    if TOKEN_CLIENTS.remove_sync(client).is_some() {
        TOKEN_CLIENTS_GENERATION.fetch_add(1, Ordering::Release);
    }
}

/// The holder's side of the token plane for ONE volume.
///
/// **Two users** (review round 2, Issue 23): the conveyor PASS task
/// (stage A — `recall_and_wait` before its apply, `settle` after) and the
/// durability lane's ROLLBACK of a failed window (stage B, Issue 14 — the
/// same pair around its removal); the two overlap by design (D-2), so the
/// gate's in-flight set is a REFCOUNT (`token_grant_core`): an object
/// leaves the flight with its LAST user, and a grant registered under
/// either window parks. The RTT phase cuts (`last_send_ns` / `last_ack_ns`)
/// are per BATCH and clamp monotone, so two concurrent batches read each
/// other's cuts as bounds, never as holes; `pending_frames` is per client
/// and the lane issues one frame per client at a time.
pub struct TokenHolderPlane {
    lane: RecallLane,
    /// Frames issued by [`Self::recall_and_wait`], per client, waiting for
    /// the client's standing poll to take them.
    pending_frames: parking_lot::Mutex<HashMap<String, VecDeque<RecallFrame>>>,
    /// Pollers park here for a frame.
    frame_wake: squeezefs_ipc::sqz_notify::Notify,
    /// The pass parks here for acks (and the expiry sweep's ticks).
    ack_wake: squeezefs_ipc::sqz_notify::Notify,
    /// The grant ∥ pass gate (`token_grant_core`): every object of a
    /// pass's union is IN FLIGHT from the union to the apply's settle; a
    /// grant registers its token first and parks while its object is in
    /// flight, so it is either recalled by the pass or served the
    /// post-commit records — never handed the pre-commit ones.
    gate: crate::token_grant_core::GrantPassGate,
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
    /// Grants a DEAD member held that no recall ever reached — swept the
    /// moment its lease was seen expired (beside the recalls on it, which
    /// count `expired_with_lease`; the closure's terms stay exact).
    lease_swept_grants: AtomicU64,
    /// Grants answered `NotHolder` — the object's slot is leased to an
    /// appender that is not one of this mount's regions (PR 12's redirect
    /// trigger; review round 1, Issue 12).
    not_holder_redirects: AtomicU64,
    /// Redirects NOT handed out because the lessee is dead
    /// (`slot_resolve_dead_redirects`).
    dead_redirects: AtomicU64,
    /// Verbs refused because this holder's appender park EXPIRED (PR 8's
    /// `park_gate::admits_token_service` — the successor owns the slots).
    park_expired_refusals: AtomicU64,
    /// Verbs refused because the caller holds NO membership lease with the
    /// installed owner (review round 3, Issue 27): a grant is a promise the
    /// recall's lease law must be able to judge, so the lease is checked
    /// FIRST — a non-member is never granted, never registered as a token
    /// client. **Must stay 0** on a fleet whose readers arm through
    /// `arm_token_readers` (which requires the lease).
    nonmember_refusals: AtomicU64,
    /// PR 9 (review round 2, Issue 3): `CustodyGrant` frames REJECTED at
    /// the service edge — a wire word the durable / derived state does not
    /// name (`screen_custody_words`). The buggy/hostile-peer class, apart
    /// from every refusal gauge; **must stay 0** on a healthy fleet.
    custody_rejected: AtomicU64,
    timeouts_live: AtomicU64,
    releases: AtomicU64,
    /// Conveyor passes that recalled at least one object (the batching
    /// law's denominator: a storm on one object is ONE batch per pass).
    recall_batches: AtomicU64,
    grant_parks: AtomicU64,
    /// Grants that found the client's registration under a PENDING recall
    /// and waited it out before registering afresh (symmetric PR 12b round
    /// 4 — the storm legs' stale negative: served on the doomed
    /// registration, the token stood at the reader untracked here).
    regrant_under_recall_waits: AtomicU64,
    fanout: QueueDepthHistogram,
    rtt: [LatencyHistogram; RTT_PHASES],
}

impl std::fmt::Debug for TokenHolderPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenHolderPlane")
            .field("outstanding", &self.lane.outstanding_now())
            .field("inflight", &self.gate.inflight_len())
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
            gate: crate::token_grant_core::GrantPassGate::new(),
            inflight_done: squeezefs_ipc::sqz_notify::Notify::new(),
            lease_oracle: parking_lot::RwLock::new(None),
            epoch: Instant::now(),
            last_send_ns: AtomicU64::new(0),
            last_ack_ns: AtomicU64::new(0),
            grants_served: AtomicU64::new(0),
            recalls: AtomicU64::new(0),
            recall_acks: AtomicU64::new(0),
            expired_with_lease: AtomicU64::new(0),
            lease_swept_grants: AtomicU64::new(0),
            not_holder_redirects: AtomicU64::new(0),
            dead_redirects: AtomicU64::new(0),
            park_expired_refusals: AtomicU64::new(0),
            nonmember_refusals: AtomicU64::new(0),
            custody_rejected: AtomicU64::new(0),
            timeouts_live: AtomicU64::new(0),
            releases: AtomicU64::new(0),
            recall_batches: AtomicU64::new(0),
            grant_parks: AtomicU64::new(0),
            regrant_under_recall_waits: AtomicU64::new(0),
            fanout: QueueDepthHistogram::default(),
            rtt: std::array::from_fn(|_| LatencyHistogram::default()),
        }
    }

    /// The owner-side recall lane (the contracts read its outstanding
    /// population).
    pub fn lane(&self) -> &RecallLane {
        &self.lane
    }

    /// Install a lease oracle — the contracts' seam. The mount installs
    /// none: the default verdict reads the installed membership owner
    /// (`membership::installed_owner`) directly.
    pub fn install_lease_oracle(&self, oracle: Arc<LeaseOracle>) {
        *self.lease_oracle.write() = Some(oracle);
    }

    fn lease_verdict(&self, client: &str) -> LeaseVerdict {
        if let Some(o) = self.lease_oracle.read().as_ref() {
            return o(client);
        }
        match crate::membership::installed_owner() {
            // A member whose lease deadline is still ahead of the owner's
            // clock is LIVE; one past it is EXPIRED. One the owner does
            // NOT list is EXPIRED too: the dispatch grants members only
            // (review round 3, Issue 27), so any holder the owner does not
            // list has LEFT or been EVICTED (the cadence sweep removes a
            // member the moment its lease passes) — never waited like
            // live to the deadline (review round 2, Issue 5; round 4,
            // Issue 29: the `is_token_client` qualifier this arm once
            // carried re-opened a sub-ms window at the departure prune,
            // and with Issue 27 it carries no information).
            Some(owner) => match owner.lease_deadline_ms(client) {
                Some(deadline) if owner.now_ms() < deadline => LeaseVerdict::Live,
                Some(_) | None => LeaseVerdict::Expired,
            },
            // No membership plane: no lease to outlive — the derived bound
            // is the token's expiry.
            None => LeaseVerdict::Unknown,
        }
    }

    /// How long `client`'s membership lease still has on the owner's
    /// clock — the wait the pass parks for at most, so a dead reader's
    /// recall completes AT its lease expiry, never a tick later (`None`
    /// = unknown: an installed oracle, or no plane).
    fn lease_remaining(&self, client: &str) -> Option<Duration> {
        if self.lease_oracle.read().is_some() {
            return None;
        }
        let owner = crate::membership::installed_owner()?;
        let deadline = owner.lease_deadline_ms(client)?;
        Some(Duration::from_millis(
            deadline.saturating_sub(owner.now_ms()),
        ))
    }

    /// **The lease sweep**: every client of `clients` whose lease the
    /// membership owner sees EXPIRED loses everything it holds or owes.
    /// `wanted` = the grants of each such client the CURRENT pass needed
    /// gone (its objects' tokens): those complete as recalls that expired
    /// with the lease (`recalls` + `expired_with_lease`, the closure's
    /// terms); the recalls already outstanding on it count the same way;
    /// every other grant it held is `lease_swept_grants`. Returns whether
    /// anything was retired.
    fn sweep_expired(&self, clients: impl IntoIterator<Item = (String, usize)>) -> bool {
        let mut any = false;
        for (c, wanted) in clients {
            if self.lease_verdict(&c) != LeaseVerdict::Expired {
                continue;
            }
            let (pending, grants) = self.lane.retire_client(&c);
            if pending + grants == 0 {
                continue;
            }
            any = true;
            self.pending_frames.lock().remove(&c);
            let wanted = wanted.min(grants.saturating_sub(pending));
            let swept = grants.saturating_sub(pending + wanted);
            self.recalls.fetch_add(wanted as u64, Ordering::Relaxed);
            self.expired_with_lease
                .fetch_add((pending + wanted) as u64, Ordering::Relaxed);
            self.lease_swept_grants
                .fetch_add(swept as u64, Ordering::Relaxed);
            log::info!(
                "read tokens: reader '{c}' left its membership lease — {} recall(s) completed \
                 as expired_with_lease and {swept} unrecalled grant(s) swept",
                pending + wanted
            );
        }
        any
    }

    /// The grant gate: park while `object` is in a pass's flight (bounded
    /// by the recall deadline — past it the object is wedged, and the
    /// grant refuses rather than serving pre-commit records). Counts the
    /// park.
    async fn await_object_settled(&self, object: u64) -> bool {
        self.grant_parks.fetch_add(1, Ordering::Relaxed);
        let bound = self.lane.config().deadline;
        let started = Instant::now();
        loop {
            let notified = self.inflight_done.notified();
            if !self.gate.is_inflight(object) {
                return true;
            }
            let left = bound.saturating_sub(started.elapsed());
            if left.is_zero() {
                return false;
            }
            let _ = squeezefs_ipc::sqz_time::timeout(left, notified).await;
        }
    }

    /// Wait until no recall of `client`'s grant on `object` is requested
    /// — the ack (or the expiry sweep) retired it; `false` past the recall
    /// bound. Woken by every ack.
    async fn await_recall_retired(&self, object: u64, client: &str) -> bool {
        let bound = self.lane.config().deadline;
        let started = Instant::now();
        loop {
            let notified = self.ack_wake.notified();
            if !self.lane.recall_requested(object, client) {
                return true;
            }
            let left = bound.saturating_sub(started.elapsed());
            if left.is_zero() {
                return false;
            }
            let _ = squeezefs_ipc::sqz_time::timeout(left, notified).await;
        }
    }

    /// Serve a grant under the grant ∥ pass gate (`token_grant_core`):
    /// the token is REGISTERED in the lane before anything is read, so a
    /// pass beginning from here on recalls it; a pass already holding the
    /// object in flight parks the read until its apply settled, and a
    /// pass that began DURING the read (it saw the registration and is
    /// recalling us) makes the read repeat after its settle — the records
    /// served are always the post-commit ones. A read that finds nothing
    /// retracts a fresh registration.
    async fn serve_grant(
        &self,
        volume: &KvMetaBackend,
        client: &str,
        object: u64,
        wants: TokenWants,
        after: u64,
        xattr_after: &[u8],
    ) -> TokenReply {
        use crate::token_grant_core::GrantAdmission;
        // The slot's lessee first (one lease-table read): a slot another
        // appender leases has its RAM-authoritative records THERE, and a
        // grant of this manager's view would be stale by construction.
        if let Some(holder) = volume.foreign_slot_holder(object) {
            if holder != 0 && !volume.foreign_slot_holder_live(holder) {
                self.dead_redirects.fetch_add(1, Ordering::Relaxed);
                return TokenReply::HolderDead { holder };
            }
            self.not_holder_redirects.fetch_add(1, Ordering::Relaxed);
            return TokenReply::NotHolder { holder };
        }
        // **The registration this grant is served on must be the one that
        // survives the reply** (symmetric PR 12b round 4 — the `sym-storm`
        // legs' stale negative). Two ways the registration `register`
        // finds or makes can be RETIRED before the records go out: (a)
        // the client's previous token on `object` is under a PENDING
        // recall — its frame queued or in flight, its ack about to retire
        // the registration `register` just found (`already`); (b) this
        // grant registered fresh, then a pass took the object in flight,
        // saw the registration and recalled it — the reader (holding
        // nothing yet) acks at once and the ack retires it while the
        // grant parks for the settle. Served on either, the token stands
        // at the reader untracked here and is never recalled again (the
        // reader read its recall generation before or after the frame —
        // nothing at its end catches a registration retired underneath).
        // So: a recall of this (client, object) is WAITED OUT (the ack
        // retires; the reader's next resolve is this very fetch), the
        // registration is RE-ARMED before every read (idempotent — the
        // gate's register-then-check order, so a pass beginning from here
        // sees it), and the read repeats until no pass straddles it.
        let (mut already, mut admission) = self.gate.grant_register(object, client, &self.lane);
        let retract = |plane: &Self, already: bool| {
            if !already {
                plane.lane.surrender(object, client);
            }
        };
        let records = loop {
            if self.lane.recall_requested(object, client) {
                self.regrant_under_recall_waits
                    .fetch_add(1, Ordering::Relaxed);
                log::debug!(
                    "token grant for '{client}' on object {object} found its registration under \
                     a pending recall — waiting the recall out before registering afresh"
                );
                if !self.await_recall_retired(object, client).await {
                    return TokenReply::Refused {
                        reason: format!(
                            "object {object}: the client's previous token's recall did not \
                             complete inside the recall bound"
                        ),
                    };
                }
                (already, admission) = self.gate.grant_register(object, client, &self.lane);
            } else if crate::token_grant_core::HolderTable::register(&self.lane, object, client) {
                // Retired by an ack while this grant parked: fresh again.
                already = false;
            }
            if admission == GrantAdmission::Park && !self.await_object_settled(object).await {
                retract(self, already);
                return TokenReply::Refused {
                    reason: format!(
                        "object {object}: a recalled commit did not apply inside the recall bound"
                    ),
                };
            }
            admission = GrantAdmission::Proceed;
            let read = volume
                .token_records_for(object, wants, after, xattr_after)
                .await;
            if self.gate.is_inflight(object) {
                // A pass took the object in flight during the read: its
                // apply may straddle what was read. It saw this
                // registration and recalls it; the grant answers the
                // post-commit records once the pass settles, on a
                // registration re-armed at the top of the loop.
                if !self.await_object_settled(object).await {
                    retract(self, already);
                    return TokenReply::Refused {
                        reason: format!(
                            "object {object}: a recalled commit did not apply inside the \
                             recall bound"
                        ),
                    };
                }
                continue;
            }
            if self.lane.recall_requested(object, client) {
                // A pass that settled inside the read recalled this
                // registration: its ack retires it — waited out and
                // re-armed at the top, the records read again.
                continue;
            }
            match read {
                Ok(Some(r)) => break r,
                Ok(None) => {
                    retract(self, already);
                    return TokenReply::Gone;
                }
                Err(e) => {
                    retract(self, already);
                    return TokenReply::Refused {
                        reason: format!("object {object}: {e}"),
                    };
                }
            }
        };
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
                log::debug!(
                    "token recall frame {} handed to '{client}''s poll ({} object(s))",
                    frame.frame_id,
                    frame.inos.len()
                );
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

    /// **The harness's reader half** (PR 13, SIM-1's recall fan-out row):
    /// the standing poll's body — the next issued frame for `client`, parked
    /// up to `wait` — without the wire, so `membership_sim` can drive
    /// 12,500 token holders through ONE plane in one process (`(frame_id,
    /// objects)`; `None` = nothing issued inside `wait`).
    pub async fn poll_recall_frame(&self, client: &str, wait: Duration) -> Option<(u64, Vec<u64>)> {
        match self.serve_poll(client, wait).await {
            TokenReply::Recall { frame_id, objects } if frame_id != 0 => Some((frame_id, objects)),
            _ => None,
        }
    }

    /// The harness's ack half (see [`Self::poll_recall_frame`]): `client`
    /// acks `frame_id` exactly as its wire `RecallAck` would.
    pub fn ack_recall_frame(&self, client: &str, frame_id: u64) {
        let _ = self.serve_ack(client, frame_id);
    }

    fn serve_ack(&self, client: &str, frame_id: u64) -> TokenReply {
        let now = Instant::now();
        self.last_ack_ns.fetch_max(
            now.saturating_duration_since(self.epoch).as_nanos() as u64,
            Ordering::AcqRel,
        );
        let acked = self.lane.ack_frame(client, frame_id, now) as u64;
        if acked == 0 {
            log::debug!(
                "token recall frame {frame_id} acked by '{client}' matched no in-flight frame \
                 (a resend or a post-expiry straggler — nothing retired)"
            );
        }
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

    /// **PR 9 — the slot holder's custody grant** (design §5.5 the "S9
    /// custody endpoint" row): the S9 arbitration on this holder's
    /// installed custody authority under the caller's lease at it, then
    /// the object's read token with its records — ONE round trip, one
    /// arbitration, one registered token. Custody first: the arbiter's
    /// `inode_{ino}` lease is what keeps this holder's own writes off the
    /// file while the caller holds it; the records then read under the
    /// grant ∥ pass gate ([`Self::serve_grant`] — register before read),
    /// so a commit on the object recalls the caller's token whether it
    /// landed before or after the read. A token that cannot be served
    /// (`Gone`, a refused read) releases the custody it just took: the
    /// caller adopts both words or neither.
    async fn serve_custody_grant(
        &self,
        volume: &KvMetaBackend,
        owner: Option<Arc<crate::data_grant::WriteCustodyOwner>>,
        client: &str,
        ask: CustodyAsk,
    ) -> TokenReply {
        // The wire words FIRST (review round 2, Issue 3 — PR 3's
        // bounded-execution law): a slot word past the codec's bound
        // reached `split_guest_local`'s debug_assert (a panic on the
        // service lane) and, in release, truncated into an exclusive
        // arbiter lease on an ino the peer never named. The screen answers
        // the GLOBAL ino — the arbiter's key, the key this holder's own
        // writes take in the same lock table; the wire carries ONE ino,
        // derived here, never a second word to trust.
        let ino = match screen_custody_object(volume, ask.object, ask.span).await {
            Ok(ino) => ino,
            Err(reason) => {
                self.custody_rejected.fetch_add(1, Ordering::Relaxed);
                log::warn!(
                    "PR 9: CustodyGrant from '{client}' REJECTED at the service edge \
                     (dlm_token_custody_rejected): {reason}"
                );
                return TokenReply::Rejected { reason };
            }
        };
        if let Some(holder) = volume.foreign_slot_holder(ask.object) {
            if holder != 0 && !volume.foreign_slot_holder_live(holder) {
                self.dead_redirects.fetch_add(1, Ordering::Relaxed);
                return TokenReply::HolderDead { holder };
            }
            self.not_holder_redirects.fetch_add(1, Ordering::Relaxed);
            return TokenReply::NotHolder { holder };
        }
        let seam = TEST_CUSTODY_NOT_HOLDER_ONCE.swap(u64::MAX, Ordering::AcqRel);
        if seam != u64::MAX {
            self.not_holder_redirects.fetch_add(1, Ordering::Relaxed);
            return TokenReply::NotHolder {
                holder: seam as u32,
            };
        }
        let Some(owner) = owner.or_else(crate::data_grant::custody_owner) else {
            return TokenReply::Refused {
                reason: "this slot holder arms no write-custody authority (the S9 owner half \
                         the multi-writer arm installs) — custody by the slot holder needs it \
                         on every writer (PR 12's join ladder)"
                    .to_string(),
            };
        };
        // Review round 2, Issues 5/9 — a slot mid-handover whose custody
        // grants were RECALLED grants nothing new until the slot has moved
        // (the writer's re-acquire would otherwise undo the recall and the
        // handover would never complete): the retryable class.
        if crate::data_grant::handover_recall_defers(ino) {
            return TokenReply::CustodyRefused {
                status: crate::data_grant::CUSTODY_DEFERRED,
                reason: format!(
                    "inode_{ino}'s slot is mid-handover (its custody grants were recalled) — \
                     retry: the slot's next holder grants it"
                ),
            };
        }
        // The death-path custody quarantine (PR 10, Issue 34): a slot
        // recovered from an EARLY death record grants nothing fresh until
        // the dead holder's writers' `T_self` has elapsed since the record
        // — the same retryable class as the handover's deferral.
        if let Some(remaining) = crate::data_grant::custody_quarantine_remaining(ino) {
            return TokenReply::CustodyRefused {
                status: crate::data_grant::CUSTODY_DEFERRED,
                reason: format!(
                    "inode_{ino}'s slot was recovered from an early death record — its custody \
                     is quarantined for another {remaining:?} (the dead holder's writers' \
                     T_self); retry"
                ),
            };
        }
        let frame = crate::data_grant::AcquireFrame {
            schema: crate::data_grant::CUSTODY_SCHEMA,
            client: client.to_string(),
            lease_epoch: ask.lease_epoch,
            ino,
            span: ask.span,
            concurrent_write: ask.concurrent_write,
            wait_ms: ask.wait_ms,
            desired: None,
        };
        let grant = match owner.grant(&frame).await {
            Ok(grant) => grant,
            Err(status) => {
                return TokenReply::CustodyRefused {
                    status,
                    reason: format!(
                        "custody of inode_{ino} {:?} refused to '{client}' by its slot holder",
                        ask.span
                    ),
                }
            }
        };
        // The mark read AGAIN after the grant (round 3 — the grant side of
        // the Dekker pair with the handover's arm-then-census): a handover
        // that armed the mark between the check above and this grant took
        // its census without it, so the grant would span the move — it is
        // released here and the caller told to retry at the next holder.
        if crate::data_grant::handover_recall_defers(ino) {
            owner.release(client, &[grant.grant_id]);
            return TokenReply::CustodyRefused {
                status: crate::data_grant::CUSTODY_DEFERRED,
                reason: format!(
                    "inode_{ino}'s slot went mid-handover as it was granted — released; retry: \
                     the slot's next holder grants it"
                ),
            };
        }
        match self
            .serve_grant(volume, client, ask.object, TokenWants::default(), 0, &[])
            .await
        {
            TokenReply::Granted { records, already } => TokenReply::CustodyGranted {
                grant,
                records,
                already,
            },
            other => {
                owner.release(client, &[grant.grant_id]);
                other
            }
        }
    }

    /// **The commit-path recall** (§5.7.1, the conveyor pass's hook):
    /// take the pass's UNION in flight (the gate's pass half — every
    /// object, holders or not, so a first-touch grant registered from
    /// here on parks until the settle), recall every outstanding token on
    /// it ONCE, hand the frames to the readers' standing polls, and wait
    /// until every recalled grant is gone — acked, or its reader's
    /// membership lease EXPIRED as the owner sees it (the wait parks no
    /// longer than the earliest live lease has left, so a dead reader's
    /// recall completes AT its expiry). Returns the union the pass settles
    /// after its apply ([`Self::settle`]) — never empty on a non-empty
    /// union.
    pub async fn recall_and_wait(&self, objects: &[u64]) -> Vec<u64> {
        if objects.is_empty() {
            return Vec::new();
        }
        let now = Instant::now();
        // Dead members first: a reader whose lease the owner already saw
        // expire holds nothing — its grants are swept, no frame is issued
        // to it, and the pass waits for nobody on its account.
        let seen = self.gate.pass_begin(objects, &self.lane);
        let mut recalled = false;
        for (o, holders) in seen {
            if holders == 0 {
                continue;
            }
            let holders = if self.sweep_expired(self.lane.holders_of(o).into_iter().map(|c| (c, 1)))
            {
                self.lane.holders(o)
            } else {
                holders
            };
            if holders == 0 {
                continue;
            }
            let n = self.lane.recall_object(o, now);
            self.fanout.record(holders);
            self.recalls.fetch_add(n as u64, Ordering::Relaxed);
            recalled = true;
        }
        if !recalled {
            return objects.to_vec();
        }
        self.recall_batches.fetch_add(1, Ordering::Relaxed);
        let t0 = now.saturating_duration_since(self.epoch).as_nanos() as u64;
        // The phase cuts belong to THIS batch: an earlier batch's instants
        // sit below `t0` and clamp to it.
        self.last_send_ns.fetch_max(t0, Ordering::AcqRel);
        self.last_ack_ns.fetch_max(t0, Ordering::AcqRel);
        let cfg = self.lane.config();
        // The expiry sweep's tick: a quarter of the deadline, floored at
        // the timer grain — the wait is woken by acks and cut short at the
        // earliest live lease's expiry; the tick only bounds how late a
        // lease whose remaining time is unknown is re-read.
        let tick = (cfg.deadline / 4).max(Duration::from_millis(1));
        loop {
            // Issue what can be issued (one in-flight frame per client);
            // frames wait for their reader's poll.
            let frames = self.lane.issue_pass(Instant::now());
            if !frames.is_empty() {
                let mut pending = self.pending_frames.lock();
                for f in frames {
                    log::debug!(
                        "token recall frame {} issued to '{}' for object(s) {:?}",
                        f.frame_id,
                        f.client,
                        f.inos
                    );
                    pending.entry(f.client.clone()).or_default().push_back(f);
                }
                drop(pending);
                self.frame_wake.notify_waiters();
            }
            let notified = self.ack_wake.notified();
            // The pass waits for the recalls it ISSUED (symmetric PR 12b
            // round 4): a holder registered after the union went in
            // flight was never recalled — it is parked on the gate and
            // reads the post-commit records at the settle — so counting
            // it here held every pass to that grant's park bound (the
            // recall deadline: the storm legs' 18.75 s stall at a
            // contended directory, four daemons' `mkdir -p` under one
            // root at once).
            if objects
                .iter()
                .all(|o| self.lane.holders_under_recall(*o) == 0)
            {
                break;
            }
            // The wait: to the next ack, the earliest live lease's expiry,
            // or the tick — whichever comes first.
            let clients = self.lane.recall_clients();
            let mut wait = tick;
            for c in &clients {
                if let Some(left) = self.lease_remaining(c) {
                    wait = wait.min(left.max(Duration::from_millis(1)));
                }
            }
            let _ = squeezefs_ipc::sqz_time::timeout(wait, notified).await;
            // Leases the owner now sees expired: their recalls complete as
            // `expired_with_lease`, their other grants are swept.
            self.sweep_expired(self.lane.recall_clients().into_iter().map(|c| (c, 0)));
            // Frames past the DEADLINE on a member whose lease is still
            // live: the must-stay-0 stuck-reader class — the token is
            // retired and the commit proceeds.
            for t in self.lane.expire_overdue(Instant::now()) {
                self.pending_frames.lock().remove(&t.client);
                match self.lease_verdict(&t.client) {
                    LeaseVerdict::Expired | LeaseVerdict::Unknown => {
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
        // wake hop, or the expiry that ended a batch no ack closed).
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
        objects.to_vec()
    }

    /// **The departure sweep** (review round 2, Issue 5): `client` left
    /// the membership census (a clean leave, or the owner's eviction) —
    /// every grant it holds is retired NOW: its outstanding recalls
    /// complete as `expired_with_lease` (a pass parked on them wakes),
    /// every other grant is `lease_swept_grants`.
    pub fn sweep_departed(&self, client: &str) {
        let (pending, grants) = self.lane.retire_client(client);
        if pending + grants == 0 {
            return;
        }
        self.pending_frames.lock().remove(client);
        self.expired_with_lease
            .fetch_add(pending as u64, Ordering::Relaxed);
        self.lease_swept_grants
            .fetch_add(grants.saturating_sub(pending) as u64, Ordering::Relaxed);
        log::info!(
            "read tokens: reader '{client}' left the membership census — {pending} recall(s) \
             completed as expired_with_lease and {} unrecalled grant(s) swept at the departure",
            grants.saturating_sub(pending)
        );
        self.ack_wake.notify_waiters();
    }

    /// **The slot transfer's recall** (symmetric PR 12b — the token half
    /// of flush-then-transfer, §5.7.1 applied to §5.1.4): every
    /// outstanding token on an object of forest slot `slot` is recalled
    /// and waited for exactly as a commit's would be, then settled at
    /// once (no apply follows — from the tree-0 write on, this plane
    /// answers `NotHolder` for the slot). Without it a token the DEPARTING
    /// holder granted stood after the move, and the NEW lessee's commits
    /// recall at ITS plane alone — a reader's cached view of the moved
    /// directory would have served stale until its next epoch step, the
    /// bounded staleness R-SYM-4 forbids. Returns the objects recalled.
    pub async fn recall_slot(&self, slot: crate::meta_backend::kv::record::ForestSlot) -> usize {
        let objects = self
            .lane
            .objects_where(|o| crate::meta_backend::kv::record::forest_slot_of_ino(o) == slot);
        if objects.is_empty() {
            return 0;
        }
        let union = self.recall_and_wait(&objects).await;
        self.settle(&union);
        objects.len()
    }

    /// The pass's commit APPLIED (its records are visible in RAM) or
    /// failed as a unit: its union leaves the gate's flight and the
    /// parked grants read.
    pub fn settle(&self, objects: &[u64]) {
        if self.gate.settle(objects) {
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
    /// Count a redirect withheld because the lessee is dead (the manager's
    /// own divert's face of `slot_resolve_dead_redirects`).
    pub fn note_dead_redirect(&self) {
        self.dead_redirects.fetch_add(1, Ordering::Relaxed);
    }

    pub fn stats(&self) -> TokenHolderStats {
        TokenHolderStats {
            grants_served: self.grants_served.load(Ordering::Relaxed),
            recalls: self.recalls.load(Ordering::Relaxed),
            recall_acks: self.recall_acks.load(Ordering::Relaxed),
            expired_with_lease: self.expired_with_lease.load(Ordering::Relaxed),
            lease_swept_grants: self.lease_swept_grants.load(Ordering::Relaxed),
            not_holder_redirects: self.not_holder_redirects.load(Ordering::Relaxed),
            dead_redirects: self.dead_redirects.load(Ordering::Relaxed),
            park_expired_refusals: self.park_expired_refusals.load(Ordering::Relaxed),
            nonmember_refusals: self.nonmember_refusals.load(Ordering::Relaxed),
            custody_rejected: self.custody_rejected.load(Ordering::Relaxed),
            timeouts_live: self.timeouts_live.load(Ordering::Relaxed),
            releases: self.releases.load(Ordering::Relaxed),
            recall_batches: self.recall_batches.load(Ordering::Relaxed),
            grant_parks: self.grant_parks.load(Ordering::Relaxed),
            regrant_under_recall_waits: self.regrant_under_recall_waits.load(Ordering::Relaxed),
            outstanding: self.lane.outstanding_now(),
            fanout_p50: self.fanout_percentile(50),
            fanout_p99: self.fanout_percentile(99),
        }
    }

    /// `dlm_token_recall_fanout` p50 / p99 off the log-bucket histogram
    /// (its own bucket law — `QueueDepthHistogram::percentile`).
    fn fanout_percentile(&self, pct: u64) -> u64 {
        self.fanout.percentile(pct)
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
    pub lease_swept_grants: u64,
    pub not_holder_redirects: u64,
    /// `slot_resolve_dead_redirects` — a `NotHolder` withheld because the
    /// lessee is dead at the S6 owner (F2).
    pub dead_redirects: u64,
    pub park_expired_refusals: u64,
    pub nonmember_refusals: u64,
    pub custody_rejected: u64,
    pub timeouts_live: u64,
    pub releases: u64,
    pub recall_batches: u64,
    pub grant_parks: u64,
    pub regrant_under_recall_waits: u64,
    pub outstanding: u64,
    pub fanout_p50: u64,
    pub fanout_p99: u64,
}

/// The token service for ONE volume (the verbs executed against its
/// holder plane).
pub struct TokenService {
    volume: Arc<KvMetaBackend>,
    /// PR 9: the S9 custody authority `CustodyGrant` arbitrates on —
    /// this service's own when set, else the process-installed owner
    /// (`data_grant::custody_owner`). The N-holder contracts stand N
    /// holders up in one process, each with its own authority.
    custody_owner: Option<Arc<crate::data_grant::WriteCustodyOwner>>,
}

impl TokenService {
    pub fn new(volume: Arc<KvMetaBackend>) -> Arc<Self> {
        Arc::new(Self {
            volume,
            custody_owner: None,
        })
    }

    /// [`Self::new`] arbitrating custody on `owner` instead of the
    /// process-installed authority.
    pub fn with_custody_owner(
        volume: Arc<KvMetaBackend>,
        owner: Arc<crate::data_grant::WriteCustodyOwner>,
    ) -> Arc<Self> {
        Arc::new(Self {
            volume,
            custody_owner: Some(owner),
        })
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
        // PR 8 (KD-SYM-15 extended): a PARKED lessee is still the lock
        // master for its slots — grants and recalls continue through the
        // park; a park that EXPIRED poisoned custody and its slots are the
        // successor's, so nothing is granted off this holder's view.
        if !crate::park_gate::admits_token_service() {
            plane.park_expired_refusals.fetch_add(1, Ordering::Relaxed);
            return Self::refuse(
                req_id,
                STATUS_NOT_HOLDER,
                "this holder's appender park EXPIRED (custody poisoned): its slots are the \
                 successor's — resolve the holder again"
                    .to_string(),
            );
        }
        // The membership lease FIRST (review round 3, Issue 27), through
        // the ONE accessor the recall judges by (`lease_verdict` — round
        // 4, Issue 30: a member whose deadline has passed but whose
        // eviction sweep has not run yet is refused a beat earlier): a
        // caller whose lease the installed owner sees EXPIRED — or does
        // not list — is refused before it is granted anything or
        // registered as a token client. `Unknown` (no owner, no oracle:
        // the in-process contracts, a plane with no membership) checks
        // nothing.
        if plane.lease_verdict(&frame.client) == LeaseVerdict::Expired {
            plane.nonmember_refusals.fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "token service refused a frame: client '{}' holds no live membership lease with \
                 this set's owner — a read token is granted to members only (join through the \
                 membership plane first; a member mid-reclaim at a successor retries)",
                frame.client
            );
            // TYPED (PR 13b, §4.4ag): the client keys its park-and-retry on
            // the word, never the prose.
            return match encode_reply(&TokenReplyFrame {
                schema: TOKEN_SCHEMA,
                request_id: frame.request_id,
                reply: TokenReply::NotMember,
            }) {
                Ok(body) => RpcResponse {
                    id: req_id,
                    status: STATUS_REFUSED,
                    body,
                },
                Err(e) => Self::refuse(req_id, STATUS_MALFORMED, format!("reply encode: {e}")),
            };
        }
        // Every verb names its client: a member that reached this service
        // is a TOKEN client — the class the recall-gated free bypasses the
        // ring for (an S5 reader never dials it).
        note_token_client(&frame.client);
        // Test seam (review round 4, Issue 29's pin): park the dispatch
        // here — after the membership check and the client's registration,
        // before the grant's — so an eviction (its sweep + prune) can run
        // inside the window.
        let hold = TEST_DISPATCH_HOLD_AFTER_CHECK_MS.load(Ordering::Relaxed);
        if hold > 0 {
            squeezefs_ipc::sqz_time::sleep(Duration::from_millis(hold)).await;
        }
        let reply = match &frame.call {
            TokenCall::Grant {
                object,
                mode: TokenMode::Read,
                wants,
                after,
                xattr_after,
            } => {
                plane
                    .serve_grant(
                        &self.volume,
                        &frame.client,
                        *object,
                        *wants,
                        *after,
                        xattr_after,
                    )
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
            TokenCall::CustodyGrant {
                object,
                span,
                concurrent_write,
                wait_ms,
                lease_epoch,
            } => {
                plane
                    .serve_custody_grant(
                        &self.volume,
                        self.custody_owner.clone(),
                        &frame.client,
                        CustodyAsk {
                            object: *object,
                            span: *span,
                            concurrent_write: *concurrent_write,
                            wait_ms: *wait_ms,
                            lease_epoch: *lease_epoch,
                        },
                    )
                    .await
            }
        };
        let status = match reply {
            TokenReply::Refused { .. }
            | TokenReply::CustodyRefused { .. }
            | TokenReply::NotMember => STATUS_REFUSED,
            TokenReply::Rejected { .. } => STATUS_REJECTED,
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

    /// [`Self::new`] with every volume's `CustodyGrant` arbitrating on
    /// `owner` (PR 9 — one authority per listener; the N-holder venue).
    pub fn with_custody_owner(
        volumes: &[Arc<KvMetaBackend>],
        owner: Arc<crate::data_grant::WriteCustodyOwner>,
    ) -> Arc<Self> {
        Arc::new(Self {
            volumes: volumes
                .iter()
                .map(|v| TokenService::with_custody_owner(Arc::clone(v), Arc::clone(&owner)))
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

/// One object of a recall or a release as the data sink sees it: the
/// LOCAL key ino and the token entry the reader held for it — `None`
/// when it held none (shed under R5 pressure, or a recall of an object
/// this reader never fetched). The entry carries the object's `layout`,
/// which is what lets the sink purge the object's block keys and no
/// other's (review round 1, Issue 10).
pub struct RecalledObject {
    pub ino: u64,
    pub entry: Option<Arc<TokenEntry>>,
}

impl RecalledObject {
    /// An object the reader held no entry for.
    pub fn bare(ino: u64) -> Self {
        Self { ino, entry: None }
    }
}

/// The mount's data-plane half of a recall ack (§5.7.3): drain the
/// reader's in-flight DMA serves on the recalled objects and purge its
/// block-key census — the R-6 purge — BEFORE the ack travels. The mount
/// installs its router-backed sink; the in-process contracts install a
/// probe.
pub trait RecallDataSink: Send + Sync {
    fn drain_and_purge<'a>(
        &'a self,
        objects: &'a [RecalledObject],
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// Entry state: serving.
const ENTRY_LIVE: u8 = 0;
/// Entry state: recalled or evicted — a serve that finds it re-fetches.
const ENTRY_REVOKED: u8 = 1;

/// One cached token: the object's records as granted. Immutable once
/// installed — a serve that began before the entry's recall serves the
/// records it holds (linearizable at its start); the DMA hazard a recall
/// exists for is the `ServeStamp` drain's, run by the recall sink, which
/// is why the entry carries no in-flight count of its own (review round
/// 1, Issue 15).
pub struct TokenEntry {
    pub attrs: InodeValue,
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    /// The directory's entries in cookie order (`None` = not fetched;
    /// a non-directory never has them).
    pub dir: Option<Vec<DirRecord>>,
    /// name → index into `dir` (a `lookup` is one hash probe, never a
    /// scan of the set — review round 1, Issue 11).
    names: Option<HashMap<Vec<u8>, usize>>,
    /// The bytes this entry is charged to the token records budget.
    bytes: u64,
    state: AtomicU8,
    used: AtomicBool,
}

impl TokenEntry {
    /// The dentry named `name`, if the directory holds one.
    pub fn find(&self, name: &[u8]) -> Option<&DirRecord> {
        let dir = self.dir.as_ref()?;
        let at = *self.names.as_ref()?.get(name)?;
        dir.get(at)
    }

    /// The dentries with a cookie strictly above `after`, at most `max` —
    /// a binary search on the cookie-ordered set, so a whole `readdir` of
    /// N entries costs O(N) rather than O(N²/page).
    pub fn page_after(&self, after: u64, max: usize) -> &[DirRecord] {
        let Some(dir) = self.dir.as_ref() else {
            return &[];
        };
        let from = dir.partition_point(|d| d.cookie <= after);
        &dir[from..dir.len().min(from + max)]
    }

    /// The bytes this entry is charged (`dlm_token_cached_bytes`).
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// A serve of one entry: the `Arc` keeps the records alive for the
/// serve's duration — a recall dropping the entry from the cache never
/// invalidates a serve already begun.
pub struct TokenServe {
    entry: Arc<TokenEntry>,
}

impl TokenServe {
    pub fn entry(&self) -> &TokenEntry {
        &self.entry
    }
}

/// Bytes charged per cached token beside its records (the entry, its
/// `Arc`, the `scc` bucket, the name index's header) — the delegation
/// cache's accounting style.
const TOKEN_ENTRY_BYTES: u64 = 128;
/// Bytes charged per dentry beside its name (cookie, ino, type, the
/// index slot).
const DIR_RECORD_BYTES: u64 = 24;

/// The recall channel's reconnect backoff: from a twentieth of the
/// channel's birth park (one RTT-class retry) doubling to five parks
/// (the S10 park derivation's own ceiling) — both derived from
/// `DELEG_PARK_DEFAULT_MS`, never free constants.
const RECONNECT_BACKOFF_FLOOR: Duration =
    Duration::from_millis(super::tokens::DELEG_PARK_DEFAULT_MS / 20);
const RECONNECT_BACKOFF_CEILING: Duration =
    Duration::from_millis(super::tokens::DELEG_PARK_DEFAULT_MS * 5);

/// Bytes the whole process's token readers hold — the gauge of the
/// `dlm_token_records_bytes` R5 component (one component for every
/// volume's plane; each plane mirrors its own `cached_bytes` here).
static TOKEN_RECORDS_BYTES: AtomicU64 = AtomicU64::new(0);

/// The planes registered for the component's shed.
static TOKEN_READER_PLANES: once_cell::sync::Lazy<
    parking_lot::Mutex<Vec<std::sync::Weak<TokenReaderPlane>>>,
> = once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(Vec::new()));

/// Register `dlm_token_records_bytes` with the R5 authority (once) and
/// the plane for its shed: floor 0, weight 1 — a re-earnable cache (a shed
/// DROPS entries without a release: the holder keeps the grant and its
/// next commit recalls a reader that holds nothing, one needless recall
/// per shed object, never a wrong answer).
fn ensure_records_r5(plane: &Arc<TokenReaderPlane>) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        crate::mem_budget::MEM_BUDGET.register(crate::mem_budget::Component::new(
            "dlm_token_records_bytes",
            0,
            1,
            Arc::new(|| TOKEN_RECORDS_BYTES.load(Ordering::Relaxed)),
            Arc::new(|target| {
                let planes: Vec<Arc<TokenReaderPlane>> = TOKEN_READER_PLANES
                    .lock()
                    .iter()
                    .filter_map(|w| w.upgrade())
                    .collect();
                for p in planes {
                    if TOKEN_RECORDS_BYTES.load(Ordering::Relaxed) <= target {
                        break;
                    }
                    p.shed_all();
                }
            }),
        ));
    });
    TOKEN_READER_PLANES.lock().push(Arc::downgrade(plane));
}

/// The reader's side of the token plane for ONE volume.
pub struct TokenReaderPlane {
    /// The identity words (secret, client id, volume ordinal) and the
    /// BIRTH endpoint; the endpoint in force is `endpoint` below.
    cfg: TokenClientConfig,
    /// The holder's endpoint in force — `cfg.endpoint` at birth, replaced
    /// by [`Self::repoint`] when the holder MOVED its listener (PR 13: a
    /// manager failover keeps appender 0's identity and publishes a new
    /// address; a `-o ro` reader's plane dialed the dead one for ever).
    endpoint: arc_swap::ArcSwap<String>,
    /// Bumped per re-point: a session dialed under an older generation is
    /// dropped before its next call (the pool below, the channel task).
    endpoint_gen: AtomicU64,
    /// Wakes the channel task out of its reconnect backoff at a re-point.
    repoint_wake: squeezefs_ipc::sqz_notify::Notify,
    /// The grant session pool (Issue 16a) — request/reply sessions,
    /// dialed lazily, one call each at a time; each carries the endpoint
    /// generation it was dialed under.
    sessions: Vec<crate::sqz_sync::SqzMutex<Option<(u64, RpcClient)>>>,
    session_rr: std::sync::atomic::AtomicUsize,
    /// Sessions the pool has DIALED (`dlm_token_grant_sessions`).
    grant_sessions: AtomicU64,
    cache: scc::HashMap<u64, Arc<TokenEntry>>,
    /// Single-flight grants per object.
    fetching: scc::HashMap<u64, Arc<squeezefs_ipc::sqz_notify::Notify>>,
    /// Per-object recall generation — a fetch that spanned a recall of
    /// its object installs nothing and retries.
    revoke_gens: scc::HashMap<u64, u64>,
    channel_ok: AtomicBool,
    /// The channel FAILED since its last completed round (a dial the
    /// holder refused, a round that errored) — what tells a dead holder
    /// from a channel on its first round, both `!channel_ok`.
    channel_failed: AtomicBool,
    channel_last_round_ms: AtomicU64,
    /// Woken at every completed channel round AND at every channel
    /// failure — `await_channel_fresh`'s park (register-recheck; never a
    /// poll).
    channel_round_wake: squeezefs_ipc::sqz_notify::Notify,
    channel_park_ms: AtomicU64,
    epoch: Instant,
    data_sink: std::sync::OnceLock<Arc<dyn RecallDataSink>>,
    stop: AtomicBool,
    /// The recall channel task is running (`dlm_token_channel_alive`).
    channel_alive: AtomicBool,
    /// Test seam: `0` = read the installed membership session, `1` =
    /// declared live, `2` = declared past `T_self`.
    lease_override: AtomicU8,
    /// Bytes the cache holds (`dlm_token_cached_bytes`) — charged at
    /// install, credited at recall / eviction / shed; mirrored into the
    /// process-wide R5 gauge.
    cached_bytes: AtomicU64,
    /// Test seam: a records budget in place of the derived one (0 = the
    /// derivation).
    budget_override: AtomicU64,
    // ---- gauges ----
    grants: AtomicU64,
    hits: AtomicU64,
    recalls_received: AtomicU64,
    recalls_acked: AtomicU64,
    releases: AtomicU64,
    /// Entries the R5 authority shed under pressure (dropped, not
    /// released — the holder's next recall finds nothing cached).
    sheds: AtomicU64,
    /// Grants refused because ONE entry would exceed the whole records
    /// budget (a directory too large for this reader's budget — R5
    /// sizing is the remedy; the streaming form is owed).
    oversize_refusals: AtomicU64,
    serve_refusals: AtomicU64,
    channel_rounds: AtomicU64,
    fetch_retries: AtomicU64,
    /// Re-points to a MOVED holder (`dlm_token_holder_repoints`).
    holder_repoints: AtomicU64,
    /// Verbs the holder's membership screen refused `NotMember` and this
    /// plane parked on the member's reclaim for (PR 13b, §4.4ag —
    /// `dlm_token_membership_waits`).
    membership_waits: AtomicU64,
    /// The recall channel's last round was refused by the holder's
    /// membership screen — the serve gate parks on this word instead of
    /// failing closed (cleared by the next completed round).
    channel_membership_pending: AtomicBool,
    grant_rtt: LatencyHistogram,
}

impl std::fmt::Debug for TokenReaderPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenReaderPlane")
            .field("endpoint", &self.endpoint())
            .field("cached", &self.cache.len())
            .finish()
    }
}

/// What a resolve's bounded wait on a plane's recall channel found
/// ([`TokenReaderPlane::await_channel_fresh_or_failed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelWait {
    /// A round completed inside the window.
    Fresh,
    /// The channel FAILED (its dial refused, its round errored) — the
    /// holder is dead or MOVED; the caller re-resolves it off durable
    /// state before it spends the window.
    Failed,
    /// The window passed with neither.
    Expired,
}

/// The typed word of a `NotHolder` answer: `object`'s slot is served by
/// appender `holder`, not the one this plane dials. A READER fails closed
/// on it until its epoch step re-reads tree 0 (R-SYM-4); a WRITER's
/// divert (PR 12b) refreshes its lease projection and retries ONCE at
/// `holder` inside the same op — the manager's word is fresher than the
/// projection the ledger cadence hands a joiner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotHolderRedirect {
    pub object: u64,
    pub holder: u32,
}

impl std::fmt::Display for NotHolderRedirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "read token unavailable: object {}'s slot is held by appender {}, not the holder \
             this plane dials — the reader's tree 0 lags the lease; the next resolve after its \
             epoch step dials the holder tree 0 names (R-SYM-4: a foreign object is served under \
             a token or not at all)",
            self.object, self.holder
        )
    }
}

impl std::error::Error for NotHolderRedirect {}

/// The `NotHolder` redirect an error carries, if it is one.
pub fn not_holder_redirect(e: &SqueezefsError) -> Option<NotHolderRedirect> {
    match e {
        SqueezefsError::Io(io) => io
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<NotHolderRedirect>())
            .copied(),
        _ => None,
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
        let sessions = crate::meta_ship::publish::publish_ship_depth_from(
            None,
            crate::cpu::process_parallelism(),
        );
        let plane = Arc::new(Self {
            endpoint: arc_swap::ArcSwap::from_pointee(cfg.endpoint.clone()),
            endpoint_gen: AtomicU64::new(0),
            repoint_wake: squeezefs_ipc::sqz_notify::Notify::new(),
            cfg,
            sessions: (0..sessions)
                .map(|_| crate::sqz_sync::SqzMutex::new(None))
                .collect(),
            session_rr: std::sync::atomic::AtomicUsize::new(0),
            grant_sessions: AtomicU64::new(0),
            cache: scc::HashMap::new(),
            fetching: scc::HashMap::new(),
            revoke_gens: scc::HashMap::new(),
            channel_ok: AtomicBool::new(false),
            channel_failed: AtomicBool::new(false),
            channel_last_round_ms: AtomicU64::new(0),
            channel_round_wake: squeezefs_ipc::sqz_notify::Notify::new(),
            // The S10 channel's birth park (the first poll's reply
            // replaces it with the holder's bound).
            channel_park_ms: AtomicU64::new(super::tokens::DELEG_PARK_DEFAULT_MS),
            epoch: Instant::now(),
            data_sink: std::sync::OnceLock::new(),
            stop: AtomicBool::new(false),
            channel_alive: AtomicBool::new(false),
            lease_override: AtomicU8::new(0),
            cached_bytes: AtomicU64::new(0),
            budget_override: AtomicU64::new(0),
            grants: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            recalls_received: AtomicU64::new(0),
            recalls_acked: AtomicU64::new(0),
            releases: AtomicU64::new(0),
            sheds: AtomicU64::new(0),
            oversize_refusals: AtomicU64::new(0),
            serve_refusals: AtomicU64::new(0),
            channel_rounds: AtomicU64::new(0),
            fetch_retries: AtomicU64::new(0),
            holder_repoints: AtomicU64::new(0),
            membership_waits: AtomicU64::new(0),
            channel_membership_pending: AtomicBool::new(false),
            grant_rtt: LatencyHistogram::default(),
        });
        ensure_records_r5(&plane);
        plane
    }

    /// **The arm's probe** (review round 1, Issue 17): one empty recall
    /// round (`Recall { wait_ms: 0 }`) on the grant session — the holder
    /// must answer it, so a writer whose plane is unarmed (the token verbs
    /// are not on its listener), a listener that is down, or a wrong
    /// endpoint is found at the ARM and refuses the mount, never at the
    /// first resolve as an EIO.
    pub async fn probe(&self) -> Result<()> {
        match self.call(TokenCall::Recall { wait_ms: 0 }).await? {
            TokenReply::Recall { .. } => Ok(()),
            other => Err(fail_closed(&format!(
                "the holder answered the arm's probe with {other:?}"
            ))),
        }
    }

    /// Test seam: the records budget in force (`None` = the derivation).
    pub fn test_set_records_budget(&self, bytes: Option<u64>) {
        self.budget_override
            .store(bytes.unwrap_or(0), Ordering::Relaxed);
    }

    /// The records budget in force, bytes.
    fn records_budget(&self) -> u64 {
        match self.budget_override.load(Ordering::Relaxed) {
            0 => super::tokens::records_budget_bytes(),
            b => b,
        }
    }

    /// Charge / credit the byte gauges (the plane's and the R5 one).
    fn charge(&self, bytes: u64) {
        self.cached_bytes.fetch_add(bytes, Ordering::Relaxed);
        TOKEN_RECORDS_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }

    fn credit(&self, bytes: u64) {
        self.cached_bytes.fetch_sub(bytes, Ordering::Relaxed);
        TOKEN_RECORDS_BYTES.fetch_sub(bytes, Ordering::Relaxed);
    }

    /// Install the mount's data drain + purge (once).
    pub fn install_data_sink(&self, sink: Arc<dyn RecallDataSink>) -> bool {
        self.data_sink.set(sink).is_ok()
    }

    /// The installed data sink, shared with a per-holder plane of the same
    /// volume (PR 12 — one purge per volume, whichever holder recalls).
    pub fn data_sink(&self) -> Option<Arc<dyn RecallDataSink>> {
        self.data_sink.get().map(Arc::clone)
    }

    /// This plane's config with the endpoint replaced — how a reader dials
    /// a second HOLDER of the same volume under the same identity, secret
    /// and volume ordinal (PR 12's per-slot binding).
    pub fn config_for_endpoint(&self, endpoint: &str) -> TokenClientConfig {
        TokenClientConfig {
            endpoint: endpoint.to_string(),
            ..self.cfg.clone()
        }
    }

    /// The endpoint this plane dials NOW (the birth endpoint until a
    /// [`Self::repoint`]).
    pub fn endpoint(&self) -> Arc<String> {
        self.endpoint.load_full()
    }

    /// **Re-point this plane at `endpoint` — the holder MOVED its
    /// listener** (PR 13, the fleet's `sym-crash` leg: a manager failover
    /// keeps appender 0's identity and publishes a NEW address into the
    /// same claim-set entry; a `-o ro` reader's manager plane dialed the
    /// dead one for the rest of its life — every read `EIO`, `.stats`
    /// unreadable). The plane keeps its identity, its gauges, its data
    /// sink and its R5 registration; what changes: the endpoint word, the
    /// generation (every pooled session and the channel's session were
    /// dialed under the old one and are dropped before their next call),
    /// every cached token (a holder that moved may have re-granted the
    /// object — PR 5's law for a dead holder, `stop_dead`'s drop) with its
    /// purge, and the channel task is woken out of its backoff to dial the
    /// new address at once. `false` = the same endpoint, nothing done.
    pub async fn repoint(&self, endpoint: &str) -> bool {
        let stale = self.endpoint();
        if stale.as_str() == endpoint {
            return false;
        }
        self.endpoint.store(Arc::new(endpoint.to_string()));
        self.endpoint_gen.fetch_add(1, Ordering::AcqRel);
        self.channel_ok.store(false, Ordering::Release);
        self.channel_failed.store(false, Ordering::Release);
        self.holder_repoints.fetch_add(1, Ordering::Relaxed);
        log::info!(
            "token plane (volume {}): the holder MOVED its listener {stale} → {endpoint} — \
             re-pointed in place; every cached token dropped, the recall channel re-dials",
            self.cfg.volume
        );
        self.drop_all_and_purge().await;
        self.repoint_wake.notify_waiters();
        true
    }

    /// Dial the endpoint in force; the session carries the generation it
    /// was dialed under.
    async fn dial(&self) -> Result<(u64, RpcClient)> {
        let gen = self.endpoint_gen.load(Ordering::Acquire);
        let endpoint = self.endpoint();
        let client = RpcClient::connect(
            endpoint.as_str(),
            &self.cfg.secret,
            &self.cfg.client_id,
            None,
        )
        .await?;
        Ok((gen, client))
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
        age <= self.channel_park_ms.load(Ordering::Relaxed) * 2
            + super::tokens::DELEG_FRESH_SLACK_MS
    }

    /// Wait — at most the freshness window itself — for the standing
    /// recall channel's FIRST round (symmetric PR 12b: a writer's lazily
    /// dialed per-holder plane serves its first foreign read right after
    /// the dial, and `serve_gate` refuses an object under a channel that
    /// has not yet completed a round; a bounded park here turns that
    /// refusal into the round's latency). Returns whether the channel is
    /// fresh; a `false` is the caller's honest refusal.
    pub async fn await_channel_fresh(&self) -> bool {
        self.await_channel(false).await == ChannelWait::Fresh
    }

    /// [`Self::await_channel_fresh`] that returns EARLY when the channel
    /// FAILED (PR 13): a dial the holder refused or a round that errored
    /// since the last completed round is the shape of a dead or MOVED
    /// holder, and the resolve that called re-resolves the holder off
    /// durable state instead of spending the whole window at the dead
    /// address. A channel on its first round (never failed) waits the
    /// window as before.
    pub async fn await_channel_fresh_or_failed(&self) -> ChannelWait {
        self.await_channel(true).await
    }

    async fn await_channel(&self, early_on_failure: bool) -> ChannelWait {
        let bound = std::time::Duration::from_millis(
            self.channel_park_ms.load(Ordering::Relaxed) * 2 + super::tokens::DELEG_FRESH_SLACK_MS,
        );
        let started = Instant::now();
        // Register-recheck-await on the round's wake (PR 12b review round
        // 1, Issue 8 — never a 1 ms poll): the loop registers, re-reads
        // the word, then parks bounded by what is left of the window.
        while !self.channel_fresh() {
            if self.stop.load(Ordering::Relaxed) {
                return ChannelWait::Expired;
            }
            if early_on_failure && self.channel_failed.load(Ordering::Acquire) {
                return ChannelWait::Failed;
            }
            let Some(left) = bound.checked_sub(started.elapsed()) else {
                return ChannelWait::Expired;
            };
            let woken = self.channel_round_wake.notified();
            if self.channel_fresh() {
                return ChannelWait::Fresh;
            }
            if early_on_failure && self.channel_failed.load(Ordering::Acquire) {
                return ChannelWait::Failed;
            }
            let _ = squeezefs_ipc::sqz_time::timeout(left, woken).await;
        }
        ChannelWait::Fresh
    }

    /// The channel failed: the word, and the waiters woken to read it.
    fn note_channel_failure(&self) {
        self.channel_ok.store(false, Ordering::Release);
        self.channel_failed.store(true, Ordering::Release);
        self.channel_round_wake.notify_waiters();
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

    /// One call on the grant session POOL (review round 1, Issue 16a):
    /// the first free session, else the round-robin one — a grant parked
    /// at the holder (its object in flight under a pass) holds ONE
    /// session, and every other grant of the volume rides the others.
    /// Pool depth = the D-1b session-depth derivation (one per 8 cores,
    /// 2..=8 — the owner's RPC-lane slope), dialed lazily.
    /// A USER-facing verb at the holder (a fetch, a custody grant),
    /// PARKING on this member's reclaim when the holder's membership
    /// screen refuses it (PR 13b, record §4.4ag — the typed
    /// [`RefusalClass::MembershipPending`] `call_on` mints from
    /// `TokenReply::NotMember`): inside a manager failover the successor's
    /// owner does not list this member until its renewal loop re-asserts
    /// there, a beat after its reads resume; the verb waits for the next
    /// adopted grant (`membership::await_grant_adopted`, bounded by
    /// `reassertion_wait_bound`) and asks again. Past the bound the
    /// retryable class surfaces, never `EIO`. Counted on
    /// `dlm_token_membership_waits`. The recall channel's standing poll,
    /// the arm's probe, acks and releases take [`Self::call`] — their
    /// loops and callers own the retry, and a park there would hold the
    /// channel's first round behind a member that never joins.
    async fn call_parking(&self, call: TokenCall) -> Result<TokenReply> {
        let deadline = Instant::now() + crate::membership::reassertion_wait_bound();
        loop {
            let gen0 = crate::membership::grant_generation();
            match self.call(call.clone()).await {
                Err(e)
                    if matches!(
                        e.refusal_class(),
                        Some(crate::error::RefusalClass::MembershipPending)
                    ) && crate::membership::installed_member_session().is_some() =>
                {
                    self.membership_waits.fetch_add(1, Ordering::Relaxed);
                    let now = Instant::now();
                    if now < deadline
                        && crate::membership::await_grant_adopted(gen0, deadline - now).await
                    {
                        continue;
                    }
                    return Err(e);
                }
                other => return other,
            }
        }
    }

    async fn call(&self, call: TokenCall) -> Result<TokenReply> {
        let n = self.sessions.len();
        let start = self.session_rr.fetch_add(1, Ordering::Relaxed) % n;
        let mut guard = None;
        for i in 0..n {
            if let Ok(g) = self.sessions[(start + i) % n].try_lock() {
                guard = Some(g);
                break;
            }
        }
        let mut guard = match guard {
            Some(g) => g,
            None => self.sessions[start].lock().await,
        };
        // A session dialed before a re-point addresses the MOVED holder's
        // dead listener: dropped, re-dialed at the endpoint in force.
        let gen = self.endpoint_gen.load(Ordering::Acquire);
        if guard.as_ref().is_some_and(|(g, _)| *g != gen) {
            *guard = None;
        }
        let (_, client) = match guard.as_mut() {
            Some(c) => c,
            None => {
                self.grant_sessions.fetch_add(1, Ordering::Relaxed);
                guard.insert(self.dial().await?)
            }
        };
        match call_on(client, &self.cfg, call.clone()).await {
            Ok(r) => Ok(r),
            Err(e) => {
                *guard = None;
                // A TRANSPORT failure on a pooled session — the peer closed
                // it idle (the wire's 60 s idle close), a reset — is
                // re-dialed and the call runs once more (every token verb
                // is idempotent against the holder's state: a re-grant
                // answers `already`, an ack / a release of nothing is a
                // no-op). Before it, a session the holder closed cost ONE
                // user op per pooled session: the `sym-storm` fleet leg's
                // manager read a live joiner's first four names of a round
                // `EINVAL` — its four pooled grant sessions to that holder
                // had idled past a minute since the previous round (four
                // misses = the pool depth, the fifth name on a fresh
                // session). A refusal the holder decoded is returned as is.
                if !crate::cluster_wire::is_transport_failure(&e) {
                    return Err(e);
                }
                self.grant_sessions.fetch_add(1, Ordering::Relaxed);
                let (_, fresh) = guard.insert(self.dial().await?);
                match call_on(fresh, &self.cfg, call).await {
                    Ok(r) => Ok(r),
                    Err(e) => {
                        *guard = None;
                        Err(e)
                    }
                }
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
                    // Test seam (PR 13): park the LOSER between the entry
                    // read and its registration — the lost-wake window.
                    if TEST_FETCH_LOSER_HOLD.load(Ordering::Acquire) {
                        TEST_FETCH_LOSER_PARKED.fetch_add(1, Ordering::AcqRel);
                        while TEST_FETCH_LOSER_HOLD.load(Ordering::Acquire) {
                            let released = TEST_FETCH_LOSER_RELEASE.notified();
                            if !TEST_FETCH_LOSER_HOLD.load(Ordering::Acquire) {
                                break;
                            }
                            released.await;
                        }
                    }
                    // Register-recheck-await (PR 13 — the fleet's `sym-
                    // walls` row: a joiner's `lookup(1)` parked 455 s past
                    // the entry station while a fresh lookup of the same
                    // name served at once — a LONE lost wake): the winner
                    // that finished between the entry read above and the
                    // registration below removed its entry and bumped the
                    // epoch BEFORE the loser registered, and a `notified()`
                    // created after the bump is never woken — the loser
                    // parked for ever, unhealable (the tick re-polls the
                    // epoch-gated future alone). Register FIRST, then
                    // re-check the entry is still the winner's; gone ⇒ the
                    // winner finished ⇒ no await, re-read the cache.
                    let notified = n.notified();
                    let still_inflight = self
                        .fetching
                        .read_sync(&object, |_, v| Arc::ptr_eq(v, &n))
                        .unwrap_or(false);
                    if still_inflight {
                        notified.await;
                    }
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
        let gen0 = self.revoke_gen(object);
        let t0 = Instant::now();
        let mut after = 0u64;
        let mut xattr_after: Vec<u8> = Vec::new();
        let mut records_done = false;
        let mut attrs: Option<WireAttrs> = None;
        let mut xattrs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut entries: Vec<DirRecord> = Vec::new();
        loop {
            let reply = self
                .call_parking(TokenCall::Grant {
                    object,
                    mode: TokenMode::Read,
                    wants: TokenWants {
                        dentries: wants.dentries,
                        records: !records_done,
                    },
                    after,
                    xattr_after: std::mem::take(&mut xattr_after),
                })
                .await?;
            match reply {
                TokenReply::Granted { records, .. } => {
                    self.grants.fetch_add(1, Ordering::Relaxed);
                    if attrs.is_none() {
                        attrs = Some(records.attrs);
                    }
                    if !records_done {
                        // The xattr pages come first; a page that did not
                        // end the set carries no dentries yet. An
                        // incomplete page that carried nothing can make
                        // no progress: refused, never spun on.
                        if !records.xattrs_complete && records.xattrs.is_empty() {
                            return Err(fail_closed(
                                "the holder answered an empty, incomplete xattr page",
                            ));
                        }
                        xattr_after = records.xattrs.last().map_or(Vec::new(), |(n, _)| n.clone());
                        xattrs.extend(records.xattrs);
                        if !records.xattrs_complete {
                            continue;
                        }
                        records_done = true;
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
                TokenReply::HolderDead { holder } => {
                    // F2: the lessee is dead at the owner; nobody answers at
                    // its address — the retryable class, never a dial.
                    return Err(SqueezefsError::refused(
                        libc::EAGAIN,
                        format!(
                            "object {object}'s slot is leased to appender {holder}, which the \
                             membership owner lists DEAD — its slots are the recovery's within \
                             the ledger poll; retry (slot_resolve_dead_redirects)"
                        ),
                    ));
                }
                TokenReply::NotHolder { holder } => {
                    // PR 12: the reader resolved the holder off ITS tree 0
                    // before dialing (`KvMetaBackend::token_reader_for`),
                    // so this is a slot that MOVED between the reader's
                    // last poll and the grant — refused now, exact at the
                    // next resolve after the epoch step re-reads tree 0.
                    // TYPED (PR 12b): a WRITER's divert re-resolves on it
                    // inside the same op (`KvMetaBackend::token_serve`).
                    return Err(SqueezefsError::Io(std::io::Error::other(
                        NotHolderRedirect { object, holder },
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
        // A recall that landed inside the pages: the pages may straddle
        // the mutation — nothing installed. (The same check is repeated
        // ATOMICALLY with the install below, after the eviction's await —
        // review round 2, Issue 21.)
        if self.revoke_gen(object) != gen0 {
            return Ok(FetchOutcome::RecalledMidFetch);
        }
        let attrs = attrs.ok_or_else(|| fail_closed("a grant carried no attrs"))?;
        let is_dir = (attrs.mode & libc::S_IFMT) == libc::S_IFDIR;
        let dir = (wants.dentries && is_dir).then_some(entries);
        self.install_records(object, gen0, attrs, xattrs, dir).await
    }

    /// `object`'s recall generation before a grant is asked for — the
    /// witness [`Self::install_carried`] installs under (the fetch's own
    /// `gen0`).
    pub fn recall_generation(&self, object: u64) -> u64 {
        self.revoke_gen(object)
    }

    /// **PR 9 — install the records a custody grant CARRIED** (the slot
    /// holder's `CustodyGranted`): the token the holder registered for
    /// this client lands in the cache under the SAME law a fetched page
    /// does — nothing installed when a recall of the object landed since
    /// `gen0` was read (the holder's channel already retired it; the next
    /// serve re-fetches). A carried page that did NOT end the xattr set
    /// (`xattrs_complete == false` — a set wider than one grant page) is
    /// PAGED to completion from the holder first (review round 2, Issue
    /// 4: the first build installed the partial page as a complete token
    /// and a `listxattr` off it misread the file); the pages ride the
    /// grant the holder already registered (`already`), counted on
    /// `pages`. `Ok(None)` = not installed.
    pub async fn install_carried(
        &self,
        object: u64,
        records: TokenRecords,
        gen0: u64,
        pages: &AtomicU64,
    ) -> Result<Option<Arc<TokenEntry>>> {
        if self.revoke_gen(object) != gen0 {
            return Ok(None);
        }
        if TEST_INSTALL_CARRIED_FAIL_ONCE.swap(false, Ordering::AcqRel) {
            return Err(fail_closed(
                "test seam: the carried install failed after custody was granted",
            ));
        }
        self.grants.fetch_add(1, Ordering::Relaxed);
        let TokenRecords {
            attrs,
            mut xattrs,
            mut xattrs_complete,
            ..
        } = records;
        while !xattrs_complete {
            // An incomplete page that carried nothing can make no
            // progress: refused, never spun on (the fetch's own law).
            let Some((last, _)) = xattrs.last() else {
                return Err(fail_closed(
                    "the holder carried an empty, incomplete xattr page on a custody grant",
                ));
            };
            let reply = self
                .call_parking(TokenCall::Grant {
                    object,
                    mode: TokenMode::Read,
                    wants: TokenWants {
                        dentries: false,
                        records: true,
                    },
                    after: 0,
                    xattr_after: last.clone(),
                })
                .await?;
            match reply {
                TokenReply::Granted { records, .. } => {
                    pages.fetch_add(1, Ordering::Relaxed);
                    if !records.xattrs_complete && records.xattrs.is_empty() {
                        return Err(fail_closed(
                            "the holder answered an empty, incomplete xattr page",
                        ));
                    }
                    xattrs.extend(records.xattrs);
                    xattrs_complete = records.xattrs_complete;
                }
                TokenReply::Gone => return Ok(None),
                TokenReply::Refused { reason } => return Err(fail_closed(&reason)),
                other => {
                    return Err(fail_closed(&format!(
                        "the holder answered a carried token's xattr page with {other:?}"
                    )))
                }
            }
            if self.revoke_gen(object) != gen0 {
                return Ok(None);
            }
        }
        match self
            .install_records(object, gen0, attrs, xattrs, None)
            .await?
        {
            FetchOutcome::Installed(e) => Ok(Some(e)),
            FetchOutcome::Gone | FetchOutcome::RecalledMidFetch => Ok(None),
        }
    }

    /// The install tail every grant page set shares: the entry built,
    /// charged against the records budget (ONE oversize entry refused
    /// loud), room made by voluntary releases, and installed iff no
    /// recall of `object` landed since `gen0` — decided UNDER the cache
    /// entry (review round 2, Issue 21).
    async fn install_records(
        &self,
        object: u64,
        gen0: u64,
        attrs: WireAttrs,
        xattrs: Vec<(Vec<u8>, Vec<u8>)>,
        dir: Option<Vec<DirRecord>>,
    ) -> Result<FetchOutcome> {
        let bytes = TOKEN_ENTRY_BYTES
            + xattrs
                .iter()
                .map(|(n, v)| (n.len() + v.len()) as u64)
                .sum::<u64>()
            + dir.as_ref().map_or(0, |d| {
                d.iter()
                    .map(|r| DIR_RECORD_BYTES + r.name.len() as u64)
                    .sum::<u64>()
            });
        let budget = self.records_budget();
        if bytes > budget {
            // ONE entry larger than the whole budget is never resident:
            // refused loud (fail-closed — R-SYM-4 leaves no second read
            // method), the remedy the message names.
            self.oversize_refusals.fetch_add(1, Ordering::Relaxed);
            return Err(fail_closed(&format!(
                "object {object}'s records ({bytes} B) exceed this reader's token records \
                 budget ({budget} B — 1/{} of the R5 memory budget; \
                 dlm_token_oversize_refusals): a directory too large for this reader's \
                 memory budget",
                super::tokens::RECORDS_BUDGET_DIVISOR
            )));
        }
        let names = dir.as_ref().map(|d| {
            d.iter()
                .enumerate()
                .map(|(i, r)| (r.name.clone(), i))
                .collect::<HashMap<Vec<u8>, usize>>()
        });
        let entry = Arc::new(TokenEntry {
            attrs: attrs.into(),
            xattrs,
            dir,
            names,
            bytes,
            state: AtomicU8::new(ENTRY_LIVE),
            used: AtomicBool::new(true),
        });
        // Room for the entry under the byte budget: evict (voluntary
        // releases — each one drained and purged before the holder is
        // told, the recall's own law) until it fits. An AWAIT: a recall
        // of `object` can land inside it.
        self.evict_to_budget(bytes).await;
        // The install is conditional on the generation the records were
        // read under, decided UNDER the cache entry (review round 2, Issue
        // 21 — the register-before-read discipline on the reader's own
        // install): the recall handler bumps the generation BEFORE it
        // removes the entry, so a bump the check sees aborts the install,
        // and a bump after the check finds the installed entry to remove.
        let installed = match self.cache.entry_sync(object) {
            scc::hash_map::Entry::Occupied(mut o) => {
                if self.revoke_gen(object) != gen0 {
                    None
                } else {
                    let prev = o.get().clone();
                    // A concurrent full fetch may have installed a richer
                    // entry; a dentry-bearing one is never displaced by a
                    // bare one.
                    if prev.dir.is_some() && entry.dir.is_none() {
                        return Ok(FetchOutcome::Installed(prev));
                    }
                    self.credit(prev.bytes);
                    prev.state.store(ENTRY_REVOKED, Ordering::Release);
                    *o.get_mut() = Arc::clone(&entry);
                    Some(())
                }
            }
            scc::hash_map::Entry::Vacant(v) => {
                if self.revoke_gen(object) != gen0 {
                    None
                } else {
                    v.insert_entry(Arc::clone(&entry));
                    Some(())
                }
            }
        };
        if installed.is_none() {
            return Ok(FetchOutcome::RecalledMidFetch);
        }
        self.charge(bytes);
        Ok(FetchOutcome::Installed(entry))
    }

    /// `object`'s recall generation (0 = never recalled).
    fn revoke_gen(&self, object: u64) -> u64 {
        self.revoke_gens.read_sync(&object, |_, g| *g).unwrap_or(0)
    }

    /// **Eviction = a voluntary release, drained and purged first**
    /// (review round 1, Issue 4): while the cache with `incoming` bytes
    /// added sits over the records budget, retire entries — the second-
    /// chance sweep first (unused since their last serve), then any — run
    /// the SAME data-plane drain + R-6 purge a recall runs on the retired
    /// objects, and only then tell the holder (`Release`). A release the
    /// holder never receives costs it one needless recall, never a wrong
    /// answer; a purge skipped would let the reader DMA a block the
    /// holder's next free — which recalls nobody for a released object —
    /// reallocated.
    async fn evict_to_budget(&self, incoming: u64) {
        let budget = self.records_budget();
        if self.cached_bytes.load(Ordering::Relaxed) + incoming <= budget {
            return;
        }
        let mut retired: Vec<RecalledObject> = Vec::new();
        for pass in 0..2u8 {
            self.cache.retain_sync(|ino, e| {
                if self.cached_bytes.load(Ordering::Relaxed) + incoming <= budget {
                    return true;
                }
                if pass == 0 && e.used.swap(false, Ordering::Relaxed) {
                    return true;
                }
                e.state.store(ENTRY_REVOKED, Ordering::Release);
                self.credit(e.bytes);
                retired.push(RecalledObject {
                    ino: *ino,
                    entry: Some(Arc::clone(e)),
                });
                false
            });
            if self.cached_bytes.load(Ordering::Relaxed) + incoming <= budget {
                break;
            }
        }
        if retired.is_empty() {
            return;
        }
        self.release_retired(retired).await;
    }

    /// The release path: the data-plane drain + purge on `objects`, then
    /// the holder is told. ONE path, two triggers (eviction, the clean
    /// leave).
    async fn release_retired(&self, retired: Vec<RecalledObject>) {
        if let Some(sink) = self.data_sink.get() {
            sink.drain_and_purge(&retired).await;
        }
        self.releases
            .fetch_add(retired.len() as u64, Ordering::Relaxed);
        // Best effort: a lost release costs the holder one needless
        // recall, never a wrong answer.
        let objects = retired.into_iter().map(|o| o.ino).collect();
        let _ = self.call(TokenCall::Release { objects }).await;
    }

    /// The R5 authority's shed: DROP every entry (no release — the holder
    /// keeps the grant and recalls a reader that holds nothing; the drain
    /// a release needs cannot run inside the authority's synchronous
    /// trim, so the tokens stay registered and the recall law covers the
    /// data plane).
    fn shed_all(&self) {
        let mut n = 0u64;
        self.cache.retain_sync(|_, e| {
            e.state.store(ENTRY_REVOKED, Ordering::Release);
            self.credit(e.bytes);
            n += 1;
            false
        });
        self.sheds.fetch_add(n, Ordering::Relaxed);
    }

    /// [`Self::serve_gate`] PARKING while the recall channel's last round
    /// was refused by the holder's membership screen (PR 13b, §4.4ag): a
    /// stale channel inside a manager failover is this member's reclaim
    /// not yet landed at the successor, not a dead holder — the serve
    /// waits for the channel's next round or an adopted grant, bounded by
    /// `reassertion_wait_bound`, and re-reads the gate; past the bound the
    /// typed retryable class surfaces ([`RefusalClass::MembershipPending`]),
    /// never the fail-closed `EIO`. Every other stale channel keeps the
    /// shipped fail-closed word.
    async fn serve_gate_parking(&self) -> Result<()> {
        let deadline = Instant::now() + crate::membership::reassertion_wait_bound();
        loop {
            let gen0 = crate::membership::grant_generation();
            let round = self.channel_round_wake.notified();
            match self.serve_gate() {
                Ok(()) => return Ok(()),
                // Only a MEMBER can be re-asserted: a process with no
                // membership session (a misconfigured reader — the ghost)
                // keeps the shipped fail-closed word at once.
                Err(_)
                    if self.channel_membership_pending.load(Ordering::Acquire)
                        && crate::membership::installed_member_session().is_some() =>
                {
                    self.membership_waits.fetch_add(1, Ordering::Relaxed);
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(SqueezefsError::retryable(
                            crate::error::RefusalClass::MembershipPending,
                            format!(
                                "token holder at {} refused '{}': no live membership lease with \
                                 its owner, and no grant was re-asserted within {:?} — retry \
                                 (dlm_token_membership_waits)",
                                self.endpoint(),
                                self.cfg.client_id,
                                crate::membership::reassertion_wait_bound()
                            ),
                        ));
                    }
                    // Whichever lands first: the channel's next round (the
                    // holder lists us again) or an adopted grant (the loop
                    // re-polls on it).
                    let remaining = deadline - now;
                    let _ = squeezefs_ipc::sqz_future::race2(
                        squeezefs_ipc::sqz_time::timeout(remaining, round),
                        crate::membership::await_grant_adopted(gen0, remaining),
                    )
                    .await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Begin a serve of `object`: the cached entry under the serve gate,
    /// fetched on a miss (or when dentries are wanted and not yet held).
    pub async fn serve(&self, object: u64, wants: TokenWants) -> Result<Option<TokenServe>> {
        self.serve_gate_parking().await?;
        loop {
            if let Some(entry) = self.cache.read_sync(&object, |_, e| Arc::clone(e)) {
                if entry.state.load(Ordering::Acquire) == ENTRY_LIVE
                    && (!wants.dentries || entry.dir.is_some())
                {
                    entry.used.store(true, Ordering::Relaxed);
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(Some(TokenServe { entry }));
                }
            }
            let Some(entry) = self.fetch(object, wants).await? else {
                return Ok(None);
            };
            if entry.state.load(Ordering::Acquire) != ENTRY_LIVE {
                continue;
            }
            return Ok(Some(TokenServe { entry }));
        }
    }

    /// Drop every entry (the lease is gone / the channel died): serves
    /// stop now; ones already begun complete under the `Arc` they hold.
    /// No release — the holder's lease arm retires the grants.
    fn drop_all(&self) {
        self.cache.retain_sync(|_, e| {
            e.state.store(ENTRY_REVOKED, Ordering::Release);
            self.credit(e.bytes);
            false
        });
    }

    /// [`Self::drop_all`] followed by the data-plane drain + purge of
    /// every dropped object — the channel task's arm on a channel loss:
    /// the holder's lease arm retires the grants and its frees will ship
    /// without recalling this reader, so no serve under the dropped
    /// records may stay in flight and no block key of theirs cached.
    async fn drop_all_and_purge(&self) {
        let mut dropped: Vec<RecalledObject> = Vec::new();
        self.cache.retain_sync(|ino, e| {
            e.state.store(ENTRY_REVOKED, Ordering::Release);
            self.credit(e.bytes);
            dropped.push(RecalledObject {
                ino: *ino,
                entry: Some(Arc::clone(e)),
            });
            false
        });
        if dropped.is_empty() {
            return;
        }
        if let Some(sink) = self.data_sink.get() {
            sink.drain_and_purge(&dropped).await;
        }
    }

    /// The standing recall channel — run by the reader's task until
    /// `stop`. Its own session: the poll parks at the holder, and a
    /// parked call must never block a grant.
    pub async fn run_recall_channel(self: Arc<Self>) {
        self.channel_alive.store(true, Ordering::Release);
        let mut session: Option<(u64, RpcClient)> = None;
        let mut backoff = RECONNECT_BACKOFF_FLOOR;
        // The backoff park, cut short by a re-point (the task dials the
        // MOVED holder's new address at once, not at the backoff's end).
        let me = &*self;
        let backoff_park = move |d: Duration| async move {
            let woken = me.repoint_wake.notified();
            let _ = squeezefs_ipc::sqz_time::timeout(d, woken).await;
        };
        while !self.stop.load(Ordering::Relaxed) {
            // A session dialed before a re-point is the dead listener's.
            let gen = self.endpoint_gen.load(Ordering::Acquire);
            if session.as_ref().is_some_and(|(g, _)| *g != gen) {
                session = None;
                backoff = RECONNECT_BACKOFF_FLOOR;
            }
            let (_, client) = match session.as_mut() {
                Some(c) => c,
                None => match self.dial().await {
                    // The backoff resets at a completed ROUND, never at
                    // the dial: a holder that accepts the connection and
                    // refuses every frame (a successor whose membership
                    // census does not list this reader yet — the
                    // failover's re-assertion window) would otherwise be
                    // re-dialed at the floor 20× a second.
                    Ok(c) => session.insert(c),
                    Err(e) => {
                        log::warn!(
                            "token recall channel to {} could not connect: {e} (retry in {:?})",
                            self.endpoint(),
                            backoff
                        );
                        self.note_channel_failure();
                        self.drop_all_and_purge().await;
                        backoff_park(backoff).await;
                        backoff = (backoff * 2).min(RECONNECT_BACKOFF_CEILING);
                        continue;
                    }
                },
            };
            let wait_ms = self.channel_park_ms.load(Ordering::Relaxed) as u32;
            let round = call_on(client, &self.cfg, TokenCall::Recall { wait_ms }).await;
            // Stopped while the round was parked: a KILLED reader acks
            // nothing it was handed (the seam models a dead process; the
            // clean leave released everything before it stopped).
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            // A round that returned from a listener re-pointed away from
            // meanwhile says nothing about the endpoint in force.
            if self.endpoint_gen.load(Ordering::Acquire) != gen {
                session = None;
                continue;
            }
            match round {
                Ok(TokenReply::Recall { frame_id, objects }) => {
                    backoff = RECONNECT_BACKOFF_FLOOR;
                    self.channel_membership_pending
                        .store(false, Ordering::Release);
                    self.channel_last_round_ms
                        .store(self.now_ms(), Ordering::Release);
                    self.channel_failed.store(false, Ordering::Release);
                    self.channel_ok.store(true, Ordering::Release);
                    self.channel_rounds.fetch_add(1, Ordering::Relaxed);
                    self.channel_round_wake.notify_waiters();
                    if frame_id != 0 && !objects.is_empty() {
                        // The ack rides the SAME session (the S10 law:
                        // the next call on the channel carries the acks).
                        if let Err(e) = self.handle_recall_on(client, frame_id, &objects).await {
                            log::warn!(
                                "token recall {frame_id} could not be acked: {e} — the channel \
                                 reconnects; the holder's deadline retires the grants"
                            );
                            session = None;
                            self.note_channel_failure();
                            self.drop_all_and_purge().await;
                        }
                    }
                }
                Ok(other) => {
                    log::warn!("token recall channel answered {other:?}; reconnecting");
                    session = None;
                    self.note_channel_failure();
                    self.drop_all_and_purge().await;
                }
                // The holder's membership screen (PR 13b, §4.4ag): this
                // member's reclaim at a successor has not landed. The
                // tokens still drop (the holder swept them with the lease
                // it does not know), the gate reads the class so a serve
                // PARKS instead of failing closed, and the channel re-polls
                // as soon as a grant is adopted — never at the doubling
                // backoff a dead holder earns.
                Err(e)
                    if matches!(
                        e.refusal_class(),
                        Some(crate::error::RefusalClass::MembershipPending)
                    ) =>
                {
                    log::info!(
                        "token recall channel to {} refused as a non-member ({e}) — every token \
                         is dropped; the channel re-polls when this member's grant is adopted",
                        self.endpoint()
                    );
                    let gen0 = crate::membership::grant_generation();
                    self.channel_membership_pending
                        .store(true, Ordering::Release);
                    self.note_channel_failure();
                    self.drop_all_and_purge().await;
                    let _ = crate::membership::await_grant_adopted(gen0, backoff).await;
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_CEILING);
                }
                Err(e) => {
                    log::warn!(
                        "token recall channel to {} failed: {e} — every token is dropped \
                         (fail-closed) until a round completes",
                        self.endpoint()
                    );
                    session = None;
                    self.note_channel_failure();
                    self.drop_all_and_purge().await;
                    backoff_park(backoff).await;
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_CEILING);
                }
            }
        }
        self.channel_alive.store(false, Ordering::Release);
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
        let mut recalled: Vec<RecalledObject> = Vec::with_capacity(objects.len());
        for &o in objects {
            match self.revoke_gens.entry_sync(o) {
                scc::hash_map::Entry::Occupied(mut e) => *e.get_mut() += 1,
                scc::hash_map::Entry::Vacant(v) => {
                    v.insert_entry(1);
                }
            }
            let entry = self.cache.remove_sync(&o).map(|(_, entry)| {
                entry.state.store(ENTRY_REVOKED, Ordering::Release);
                self.credit(entry.bytes);
                entry
            });
            recalled.push(RecalledObject { ino: o, entry });
        }
        // The data-plane half BEFORE the ack: every serve that began under
        // the recalled records drains (`ServeStamp`) and the objects' block
        // keys are purged — the ack is what lets the holder's free ship.
        if let Some(sink) = self.data_sink.get() {
            sink.drain_and_purge(&recalled).await;
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

    /// Test seam: forget `object`'s cached entry without a release or a
    /// recall (the holder keeps the grant), so the next serve re-fetches
    /// — the scoping instrument's way of pricing one token grant.
    pub fn test_drop_entry(&self, object: u64) {
        if let Some((_, e)) = self.cache.remove_sync(&object) {
            e.state.store(ENTRY_REVOKED, Ordering::Release);
            self.credit(e.bytes);
        }
    }

    /// Test seam — DEATH: the channel task exits at its next round
    /// without acking what it was handed and nothing is released; the
    /// holder's lease arm (the membership owner's eviction) is what
    /// retires this reader's grants.
    pub fn test_kill(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// The clean leave: RELEASE every held token at the holder (a
    /// voluntary release — the holder's next commit on those objects
    /// recalls nobody), then stop the channel task and drop the cache. A
    /// reader that dies without this leaves its grants to the holder's
    /// lease-expiry arm.
    pub async fn stop(&self) {
        let mut held: Vec<RecalledObject> = Vec::new();
        self.cache.iter_sync(|ino, e| {
            held.push(RecalledObject {
                ino: *ino,
                entry: Some(Arc::clone(e)),
            });
            true
        });
        // A channel that completed a round, OR one still on its first
        // (PR 9: a custody grant's carried token installs before the
        // recall channel's first round lands, and a writer that leaves
        // inside that window must still release it — every channel
        // FAILURE drops the cache, so a held entry never means a broken
        // channel).
        if !held.is_empty()
            && (self.channel_ok.load(Ordering::Acquire)
                || self.channel_alive.load(Ordering::Acquire))
        {
            self.release_retired(held).await;
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

    /// **The HOLDER died** (PR 9, review round 2 — Issue 10: the writer's
    /// custody client at a slot holder reached its `T_self`): the channel
    /// task stops, no release travels (nobody answers), and every cached
    /// entry is DROPPED — a token whose holder may have re-granted the
    /// object serves nothing (PR 5's `T_self` law for a reader, scoped to
    /// this one holder's plane).
    pub fn stop_dead(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.channel_ok.store(false, Ordering::Release);
        self.drop_all();
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
            cached_bytes: self.cached_bytes.load(Ordering::Relaxed),
            sheds: self.sheds.load(Ordering::Relaxed),
            oversize_refusals: self.oversize_refusals.load(Ordering::Relaxed),
            serve_refusals: self.serve_refusals.load(Ordering::Relaxed),
            channel_rounds: self.channel_rounds.load(Ordering::Relaxed),
            fetch_retries: self.fetch_retries.load(Ordering::Relaxed),
            channel_fresh: self.channel_fresh(),
            channel_alive: self.channel_alive.load(Ordering::Acquire),
            grant_sessions: self.sessions.len() as u64,
            grant_sessions_dialed: self.grant_sessions.load(Ordering::Relaxed),
            holder_repoints: self.holder_repoints.load(Ordering::Relaxed),
            membership_waits: self.membership_waits.load(Ordering::Relaxed),
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
    pub cached_bytes: u64,
    pub sheds: u64,
    pub oversize_refusals: u64,
    pub serve_refusals: u64,
    pub channel_rounds: u64,
    pub fetch_retries: u64,
    pub channel_fresh: bool,
    /// The recall channel task is running.
    pub channel_alive: bool,
    /// The grant session pool's depth (the derivation in force).
    pub grant_sessions: u64,
    /// Sessions of the pool dialed so far.
    pub grant_sessions_dialed: u64,
    /// Re-points to a holder that MOVED its listener (PR 13) — 0 on a
    /// fleet that never failed over.
    pub holder_repoints: u64,
    /// Parks on this member's reclaim after a holder's `NotMember`
    /// refusal (PR 13b, §4.4ag) — 0 on a fleet that never failed over.
    pub membership_waits: u64,
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
    // A REFUSED status carries either an encoded reply (the holder's word
    // — `Refused` / `Rejected` / `NotHolder` …) or the dispatch's own
    // TEXT reason (the membership screen's "not a member" while a
    // joiner re-enrols at a successor — PR 12b round 3, the `sym-crash`
    // leg: decoded as a reply it read `invalid value: integer 105` and
    // surfaced a user op's EIO). The text is the retryable class.
    let frame = match decode_reply(&resp.body) {
        Ok(f) => f,
        Err(_) if resp.status == STATUS_REFUSED && !resp.body.is_empty() => {
            return Err(SqueezefsError::refused(
                libc::EAGAIN,
                format!(
                    "token holder refused the frame: {}",
                    String::from_utf8_lossy(&resp.body)
                ),
            ));
        }
        Err(e) => return Err(e),
    };
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
    // The membership screen's word (PR 13b, §4.4ag) is the TYPED retryable
    // class: a read parks on this member's reclaim (`call_parking`), every
    // other verb surfaces it as EAGAIN at once.
    if matches!(frame.reply, TokenReply::NotMember) {
        return Err(SqueezefsError::retryable(
            crate::error::RefusalClass::MembershipPending,
            format!(
                "token holder refused the frame: client '{}' holds no live membership lease with \
                 this set's owner — a read token is granted to members only (a member \
                 mid-reclaim at a successor retries)",
                cfg.client_id
            ),
        ));
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

// ---------------------------------------------------------------------------
// The mount path's arms
// ---------------------------------------------------------------------------

/// **Test seam** (PR 13): park a single-flight fetch LOSER between its
/// read of the in-flight entry and its registration on the winner's
/// wake — the lost-wake window `TokenReaderPlane::fetch` closes with the
/// register-recheck-await idiom. `TEST_FETCH_LOSER_PARKED` counts the
/// losers parked; `TEST_FETCH_LOSER_RELEASE` lets them go once the flag
/// is cleared.
pub static TEST_FETCH_LOSER_HOLD: AtomicBool = AtomicBool::new(false);
pub static TEST_FETCH_LOSER_PARKED: AtomicU64 = AtomicU64::new(0);
pub static TEST_FETCH_LOSER_RELEASE: squeezefs_ipc::sqz_notify::Notify =
    squeezefs_ipc::sqz_notify::Notify::new();

/// Release every loser the seam parked (the flag cleared first).
pub fn test_fetch_loser_release() {
    TEST_FETCH_LOSER_HOLD.store(false, Ordering::Release);
    TEST_FETCH_LOSER_RELEASE.notify_waiters();
}

/// Block keys the scoped recall purge dropped (`dlm_token_recall_purged_keys`).
static RECALL_PURGE_KEYS: AtomicU64 = AtomicU64::new(0);
/// Objects purged by their OWN layout (`dlm_token_recall_scoped_purges`).
static RECALL_PURGE_SCOPED: AtomicU64 = AtomicU64::new(0);
/// Sink calls that fell back to the WHOLE census
/// (`dlm_token_recall_census_purges`) — an object whose block keys the
/// reader could not enumerate (no entry held, an indirect map).
static RECALL_PURGE_CENSUS: AtomicU64 = AtomicU64::new(0);

/// Resolves a reader REFUSED because the object's slot holder had no
/// bound endpoint (`dlm_token_reader_unbound_holders` — PR 12's per-slot
/// binding; the projection is never served instead).
static READER_UNBOUND_HOLDERS: AtomicU64 = AtomicU64::new(0);
/// Durable holder resolves ATTEMPTED by a reader (each = one appender
/// directory read + one claim-set `getxattr`) — the binding's cost face,
/// bounded to one per (holder, epoch step) by the negative cache.
static READER_HOLDER_RESOLVES: AtomicU64 = AtomicU64::new(0);

/// `NotHolder { holder }` redirects a READER followed once at the named
/// holder's plane (PR 13, defect 28 — a slot granted since the reader's
/// last epoch step; `dlm_token_reader_redirects_followed`). Before it the
/// reader failed closed for the whole poll interval after every grant.
static READER_REDIRECTS_FOLLOWED: AtomicU64 = AtomicU64::new(0);

pub fn note_reader_unbound_holder() {
    READER_UNBOUND_HOLDERS.fetch_add(1, Ordering::Relaxed);
}

pub fn note_reader_redirect_followed() {
    READER_REDIRECTS_FOLLOWED.fetch_add(1, Ordering::Relaxed);
}

/// The followed-redirect count (the contracts' witness; the stats face is
/// `dlm_token_reader_redirects_followed`).
pub fn test_reader_redirects_followed() -> u64 {
    READER_REDIRECTS_FOLLOWED.load(Ordering::Relaxed)
}

pub fn note_reader_holder_resolve() {
    READER_HOLDER_RESOLVES.fetch_add(1, Ordering::Relaxed);
}

/// The unbound-holder refusal count (the contracts' witness; the stats
/// face is `dlm_token_reader_unbound_holders`).
pub fn test_reader_unbound_holders() -> u64 {
    READER_UNBOUND_HOLDERS.load(Ordering::Relaxed)
}

/// The durable-resolve count (the contracts' witness; the stats face is
/// `dlm_token_reader_holder_resolves`).
pub fn test_reader_holder_resolves() -> u64 {
    READER_HOLDER_RESOLVES.load(Ordering::Relaxed)
}

/// The scoped-purge ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallPurgeCounts {
    pub keys: u64,
    pub scoped: u64,
    pub census: u64,
}

pub fn recall_purge_counts() -> RecallPurgeCounts {
    RecallPurgeCounts {
        keys: RECALL_PURGE_KEYS.load(Ordering::Relaxed),
        scoped: RECALL_PURGE_SCOPED.load(Ordering::Relaxed),
        census: RECALL_PURGE_CENSUS.load(Ordering::Relaxed),
    }
}

/// The block keys a layout value names, as the tiers key them: the STORED
/// map value (a decorated mapping is keyed verbatim) and, where the
/// decoration differs from the free-able base, that base too. `None` when
/// the layout cannot be enumerated from the record alone — an indirect or
/// kvmap head (its map lives off-record), or a value that does not decode.
fn layout_block_keys(layout: &[u8], block_size: u64) -> Option<Vec<String>> {
    let l = crate::layout_wire::decode_layout_any(layout).ok()?;
    if l.block_map_id.is_some() {
        return None;
    }
    let mut keys: Vec<String> = Vec::new();
    if let Some(map) = &l.block_map {
        for v in map.values() {
            let base = crate::routing::clean_block_key_ref(v);
            if base != v.as_str() {
                keys.push(base.to_string());
            }
            keys.push(v.clone());
        }
    } else if let Some(prefix) = &l.block_prefix {
        if block_size == 0 {
            return None;
        }
        let blocks = l.size.div_ceil(block_size);
        keys.extend((0..blocks).map(|b| format!("{prefix}/part_{b}")));
    }
    keys.sort_unstable();
    keys.dedup();
    Some(keys)
}

/// The FUSE mount's [`RecallDataSink`], one per metadata volume: an epoch
/// step on the reader's layout cache + the observed in-flight serve
/// drain (`ro_coherence::drain_in_flight_serves`), then the R-6 purge
/// SCOPED to the recalled objects (review round 1, Issue 10) — each
/// object's layout entry is dropped from the router's cache and the
/// block keys its `layout` names are purged through the ONE legal purge;
/// an object whose keys cannot be enumerated (the reader held no entry —
/// shed, or never granted — or an off-record map) falls back to the
/// whole census once per call, counted. The step stays GLOBAL: it is the
/// drain's generation and the stamp gate that makes a layout resolve
/// racing the recall miss (`ro_coherence::layout_entry_pre_step`) — under
/// tokens that miss re-decodes off the token cache, an RPC to nobody.
pub struct MountRecallSink {
    router: crate::routing::DataRouter,
    /// The volume's index in the routed set — the local → global ino step
    /// the router's layout cache is keyed by.
    volume: usize,
}

impl MountRecallSink {
    pub fn new(router: crate::routing::DataRouter, volume: usize) -> Arc<Self> {
        Arc::new(Self { router, volume })
    }

    /// The object's global ino (the router's key), if it has one.
    fn global_ino(&self, local: u64) -> Option<u64> {
        self.router
            .meta_backend
            .get()
            .and_then(|routed| routed.try_make_global_ino(local, self.volume))
    }

    /// Purge one object's keys; `false` = not enumerable (the caller
    /// falls back to the census).
    fn purge_scoped(&self, object: &RecalledObject) -> bool {
        let Some(entry) = object.entry.as_deref() else {
            return false;
        };
        let Some(global) = self.global_ino(object.ino) else {
            return false;
        };
        let block_size = self.router.block_size.load(Ordering::Relaxed);
        let layout = entry
            .xattrs
            .iter()
            .find(|(n, _)| n.as_slice() == b"layout")
            .map(|(_, v)| v.as_slice());
        // An object with no layout record owns no blocks (a directory, a
        // symlink, an empty file): its layout entry alone is dropped.
        let keys = match layout {
            Some(bytes) => match layout_block_keys(bytes, block_size) {
                Some(keys) => keys,
                None => return false,
            },
            None => Vec::new(),
        };
        self.router.discard_layout_cache(global);
        for k in &keys {
            self.router.cache.purge_block_key(k);
        }
        RECALL_PURGE_KEYS.fetch_add(keys.len() as u64, Ordering::Relaxed);
        RECALL_PURGE_SCOPED.fetch_add(1, Ordering::Relaxed);
        true
    }
}

impl RecallDataSink for MountRecallSink {
    fn drain_and_purge<'a>(
        &'a self,
        objects: &'a [RecalledObject],
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            crate::ro_coherence::drain_in_flight_serves().await;
            let mut scoped = 0usize;
            let mut census = false;
            for o in objects {
                if census {
                    break;
                }
                if self.purge_scoped(o) {
                    scoped += 1;
                } else {
                    census = true;
                }
            }
            let purged = if census {
                RECALL_PURGE_CENSUS.fetch_add(1, Ordering::Relaxed);
                crate::ro_coherence::purge_reader_block_keys(&self.router.cache)
            } else {
                0
            };
            log::debug!(
                "token recall of {} object(s): in-flight serves drained, {scoped} purged by \
                 their layout, census fallback {census} ({purged} key(s)) before the ack",
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
        "dlm_token_lease_swept_grants": per(&|s| s.lease_swept_grants),
        "dlm_token_not_holder_redirects": per(&|s| s.not_holder_redirects),
        "slot_resolve_dead_redirects": per(&|s| s.dead_redirects),
        "dlm_token_park_expired_refusals": per(&|s| s.park_expired_refusals),
        "dlm_token_nonmember_refusals": per(&|s| s.nonmember_refusals),
        "dlm_token_custody_rejected": per(&|s| s.custody_rejected),
        "dlm_token_recall_timeouts_live": per(&|s| s.timeouts_live),
        "dlm_token_releases": per(&|s| s.releases),
        "dlm_token_recall_batches": per(&|s| s.recall_batches),
        "dlm_token_grant_parks": per(&|s| s.grant_parks),
        "dlm_token_regrant_under_recall_waits": per(&|s| s.regrant_under_recall_waits),
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

/// The reader-side Token family for the stats inode, per volume — the
/// FOLD over every plane the volume reads through: the manager's and the
/// per-holder ones `token_reader_for` dialed (PR 13: the manager's alone
/// read `dlm_token_grants` short of the reader's foreign first touches on
/// every joiner-held object — gate 5's engagement law). Counters sum; the
/// two posture words (`channel_fresh` / `channel_alive`) read 1 only when
/// EVERY channel says so — a stale per-holder channel is the reader's
/// refusal on that holder's objects, and a face that hid it behind the
/// manager's fresh one would say "fresh" of a reader serving EIO.
pub fn reader_stats_json(volumes: &[Arc<KvMetaBackend>]) -> serde_json::Value {
    let planes_of = |v: &Arc<KvMetaBackend>| -> Vec<Arc<TokenReaderPlane>> {
        let mut planes: Vec<Arc<TokenReaderPlane>> =
            v.token_reader().cloned().into_iter().collect();
        planes.extend(v.reader_holder_planes());
        planes
    };
    let per = |f: &dyn Fn(&TokenReaderStats) -> u64| {
        serde_json::Value::Array(
            volumes
                .iter()
                .map(|v| {
                    planes_of(v)
                        .iter()
                        .map(|p| f(&p.stats()))
                        .sum::<u64>()
                        .into()
                })
                .collect(),
        )
    };
    let every = |f: &dyn Fn(&TokenReaderStats) -> bool| {
        serde_json::Value::Array(
            volumes
                .iter()
                .map(|v| {
                    let planes = planes_of(v);
                    u64::from(!planes.is_empty() && planes.iter().all(|p| f(&p.stats()))).into()
                })
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
        "dlm_token_cached_bytes": per(&|s| s.cached_bytes),
        "dlm_token_sheds": per(&|s| s.sheds),
        "dlm_token_oversize_refusals": per(&|s| s.oversize_refusals),
        "dlm_token_serve_refusals": per(&|s| s.serve_refusals),
        "dlm_token_channel_rounds": per(&|s| s.channel_rounds),
        "dlm_token_fetch_retries": per(&|s| s.fetch_retries),
        "dlm_token_channel_fresh": every(&|s| s.channel_fresh),
        "dlm_token_channel_alive": every(&|s| s.channel_alive),
        "dlm_token_grant_sessions": per(&|s| s.grant_sessions),
        "dlm_token_grant_sessions_dialed": per(&|s| s.grant_sessions_dialed),
        // PR 13 — a reader plane RE-POINTED at a holder that moved its
        // listener (a manager failover, a joiner's rejoin): 0 on a fleet
        // that never failed over; one per (plane, move) otherwise.
        "dlm_token_holder_repoints": per(&|s| s.holder_repoints),
        "dlm_token_membership_waits": per(&|s| s.membership_waits),
        "dlm_token_grant_rtt_ns": serde_json::Value::Array(
            volumes
                .iter()
                .map(|v| v.token_reader().map_or(serde_json::Value::Null, |p| p.grant_rtt_json()))
                .collect(),
        ),
        // PR 12 — the per-slot binding: planes dialed to holders OTHER
        // than the manager (one per (volume, holder), lazily), and
        // resolves REFUSED because the object's holder had no bound
        // endpoint (never served from the projection — must-stay-0 on a
        // fleet whose join ladder binds every holder).
        "dlm_token_reader_holder_planes": serde_json::Value::Array(
            volumes
                .iter()
                .map(|v| (v.reader_holder_planes().len() as u64).into())
                .collect(),
        ),
        "dlm_token_reader_unbound_holders": READER_UNBOUND_HOLDERS.load(Ordering::Relaxed),
        "dlm_token_reader_redirects_followed": READER_REDIRECTS_FOLLOWED.load(Ordering::Relaxed),
        "dlm_token_reader_holder_resolves": READER_HOLDER_RESOLVES.load(Ordering::Relaxed),
        // The mount's recall sink (one ledger — the sinks share it).
        "dlm_token_recall_purged_keys": RECALL_PURGE_KEYS.load(Ordering::Relaxed),
        "dlm_token_recall_scoped_purges": RECALL_PURGE_SCOPED.load(Ordering::Relaxed),
        "dlm_token_recall_census_purges": RECALL_PURGE_CENSUS.load(Ordering::Relaxed),
    })
}

/// A set of objects with their union taken (the pass's recall set).
pub fn union_objects(objects: impl IntoIterator<Item = u64>) -> Vec<u64> {
    let set: HashSet<u64> = objects.into_iter().collect();
    let mut v: Vec<u64> = set.into_iter().collect();
    v.sort_unstable();
    v
}
