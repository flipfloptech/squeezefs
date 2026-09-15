//! **The manager's verbs on the cluster wire** — `ManagerCall`
//! (docs/design-symmetric-metadata.md §6.3, §5.3.3, §5.3.5; PR 3).
//!
//! A forest volume's MANAGER (the D0 ladder's winner, KD-SYM-3) serves
//! three verbs to the volume's other appenders: `JoinAppender` (a page in
//! the directory — the chain grown when its current extent is full — a
//! ring from the heap, an initial extent grant), `ExtentGrant` and
//! `ReturnExtents`. Every verb is **idempotent against DURABLE state**
//! (KD-SYM-7): a page already `Live` under the caller's identity answers
//! `Joined { already: true }`, a return of extents the grant record no
//! longer names is a no-op — never a RAM dedup window, which a failover
//! leaves behind. The verbs ride the S8 owner service's venue (the
//! `cluster_wire` RPC listener's per-connection lane, one `RpcListener`
//! per manager endpoint, the same per-frame MAC and admission gate) in
//! their own verb block, `0x0500`; the S8 metadata verbs, the S9 custody
//! verbs and the publish plane keep theirs.
//!
//! Frame bodies are bincode: encoded unbounded (we build them), decoded
//! **bounded** (untrusted — a length in a frame is a claim, never an
//! allocation authority), fuzzed by `fuzz/fuzz_targets/manager_call_frame.rs`
//! and mirrored on stable in `tests/decoder_property_tests.rs`.
//!
//! PR 4 added the slot-lease verbs under the same schema (the wire is
//! unreleased): `AcquireSlots` / `AcquireSlot` / `OfferSlot` /
//! `ReleaseSlot` / `ResolveSlot` (design §5.1.2–§5.1.6, §5.3.5). What is
//! NOT here (later rungs, §6.3): `RecordDeath` / `RecordRecovered` (PR
//! 8/10 — the death ledger), `DirRenameLock` (PR 6). They extend this
//! enum under the same schema while the wire is unreleased; a release in
//! between bumps `MANAGER_SCHEMA` again.

use crate::cluster_wire::{RpcAsyncService, RpcClient, RpcRequest, RpcResponse};
use crate::error::{Result, SqueezefsError};
use crate::meta_backend::kv::appender::{AppenderIdentity, GrantRun};
use crate::meta_backend::kv::backend::{JoinOutcome, KvMetaBackend};
use crate::meta_backend::kv::superblock::ExtentRef;
use bincode::Options as _;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

/// The manager vocabulary's schema (independent of the transport's
/// `CLUSTER_WIRE_SCHEMA`, which PR 3 bumped 4 → 5 for this block).
pub const MANAGER_SCHEMA: u32 = 1;

/// The manager verb block: `0x0500..=0x05FF`, disjoint from the S8
/// metadata (16/17), S9 custody (`0x0200`), publish (`0x0300`) and
/// delegation (`0x0400`) blocks.
pub const VERB_MANAGER_BASE: u16 = 0x0500;
/// The ONE verb: a [`ManagerRequestFrame`] carrying a [`ManagerCall`].
pub const VERB_MANAGER_CALL: u16 = VERB_MANAGER_BASE;
/// Last verb of the block.
pub const VERB_MANAGER_LAST: u16 = 0x05FF;

/// Frame status: served — the body is a [`ManagerReplyFrame`].
pub const STATUS_OK: u16 = crate::cluster_wire::RPC_OK;
/// Frame status: the peer speaks another vocabulary version.
pub const STATUS_SCHEMA: u16 = super::wire::STATUS_SCHEMA;
/// Frame status: undecodable body (bounded, refused loud).
pub const STATUS_MALFORMED: u16 = super::wire::STATUS_MALFORMED;
/// Frame status: this node does not hold the volume's manager lease.
pub const STATUS_NOT_MANAGER: u16 = super::wire::STATUS_NOT_OWNER;
/// Frame status: the verb's durable witness contradicts the caller
/// (`manager_verb_refusals` — must-stay-0).
pub const STATUS_REFUSED: u16 = 48;
/// Frame status: the frame's wire integers name what the durable state
/// cannot (a run outside the volume, wider than the caller's record, an
/// overflowing length) — REJECTED at the service edge before any
/// allocation proportional to them (`manager_verb_rejected`, the
/// buggy/hostile-peer class; review round 1 Issue 2). The body is still
/// a [`ManagerReplyFrame`] carrying [`ManagerReply::Refused`].
pub const STATUS_REJECTED: u16 = 49;
/// Frame status: the verb's answer is "not now" — the durable state is
/// healthy, nothing was written, and the requester RETRIES (a grant of an
/// unleased slot whose records ring 0's window still held after the
/// bounded clearing cycles, `KvError::GrantDeferred`; review round 6,
/// Issue 30 / round 7, Issue 32). Distinct from a witness refusal and a
/// rejection so a peer retries without parsing a reason; the body is a
/// [`ManagerReplyFrame`] carrying [`ManagerReply::Deferred`].
pub const STATUS_DEFERRED: u16 = 50;

/// The KD-MW-2 appender identity on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireIdentity {
    pub node_token: u64,
    pub mount_slot: u32,
    pub writer_id: u128,
}

impl From<AppenderIdentity> for WireIdentity {
    fn from(id: AppenderIdentity) -> Self {
        Self {
            node_token: id.node_token,
            mount_slot: id.mount_slot,
            writer_id: id.writer_id,
        }
    }
}

impl From<WireIdentity> for AppenderIdentity {
    fn from(id: WireIdentity) -> Self {
        Self {
            node_token: id.node_token,
            mount_slot: id.mount_slot,
            writer_id: id.writer_id,
        }
    }
}

/// One extent-grant run on the wire: `(start, len)`.
pub type WireRun = (u64, u32);

fn runs_to_wire(runs: &[GrantRun]) -> Vec<WireRun> {
    runs.iter().map(|r| (r.start, r.len)).collect()
}

fn runs_from_wire(runs: &[WireRun]) -> Vec<GrantRun> {
    runs.iter()
        .map(|&(start, len)| GrantRun { start, len })
        .collect()
}

/// The manager's verbs (§6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManagerCall {
    /// A page, a ring and an initial grant for `identity` — or its
    /// existing `Live` page (`already`).
    JoinAppender {
        identity: WireIdentity,
        /// The ring the joiner asks for, bytes (0 = the manager's
        /// derivation; clamped to the volume's floor/ceiling).
        ring_want_bytes: u64,
    },
    /// Up to `want` extents (0 = the manager's derived size).
    ExtentGrant { appender_id: u32, want: u32 },
    /// Extents the appender's tail released, as runs.
    ReturnExtents {
        appender_id: u32,
        runs: Vec<WireRun>,
    },
    /// Up to `want` rotor slots for `appender_id`, `prefer:
    /// unleased-then-idle` (§5.1.2; 0 = the manager's derived `M`).
    AcquireSlots { appender_id: u32, want: u16 },
    /// One named routing slot — first-writer-takes-it, or the accept of
    /// an offer (§5.1.4).
    AcquireSlot { appender_id: u32, slot: u16 },
    /// The HOLDER offers `slot` to appender `to` (RAM at the manager,
    /// expires after one renewal beat).
    OfferSlot {
        appender_id: u32,
        slot: u16,
        to: u32,
    },
    /// Flush-then-transfer's durable step: the holder's `g` and the
    /// slot's final words; the manager writes tree 0 `Unleased`.
    ReleaseSlot {
        appender_id: u32,
        slot: u16,
        g: u32,
        words: WireSlotWords,
        /// `(leaf addr, log tail)` of every leaf flushed under the lease
        /// (§5.8.2 — PR 5's frame screen reads them).
        tails: Vec<(u64, u32)>,
    },
    /// The holder of `slot` (the `SlotHolderCache`'s stale-view fallback,
    /// §5.1.6).
    ResolveSlot { slot: u16 },
}

/// The slot tree's words on the wire (§5.1.4 "four words move" — root,
/// cursor, extent count — plus §5.8.2's seq-space floor): what a release
/// presents and a grant answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WireSlotWords {
    pub root: (u64, u64),
    pub cursor: u64,
    pub slot_tree_extents: u32,
    /// Every record of the slot carries a seq strictly below it; the
    /// lessee's ring stamps above it (`JournalRing::raise_seq_floor`).
    pub seq_floor: u64,
}

impl From<crate::slot_lease_core::SlotWords> for WireSlotWords {
    fn from(w: crate::slot_lease_core::SlotWords) -> Self {
        Self {
            root: w.root,
            cursor: w.cursor,
            slot_tree_extents: w.extents,
            seq_floor: w.seq_floor,
        }
    }
}

impl From<WireSlotWords> for crate::slot_lease_core::SlotWords {
    fn from(w: WireSlotWords) -> Self {
        Self {
            root: w.root,
            cursor: w.cursor,
            extents: w.slot_tree_extents,
            seq_floor: w.seq_floor,
        }
    }
}

/// One granted slot on the wire: the routing slot, its lease generation
/// and the words the tree carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSlotGrant {
    pub slot: u16,
    pub g: u32,
    pub words: WireSlotWords,
}

impl ManagerCall {
    /// The verb's name (log lines, the phase table).
    pub fn name(&self) -> &'static str {
        match self {
            Self::JoinAppender { .. } => "join_appender",
            Self::ExtentGrant { .. } => "extent_grant",
            Self::ReturnExtents { .. } => "return_extents",
            Self::AcquireSlots { .. } => "acquire_slots",
            Self::AcquireSlot { .. } => "acquire_slot",
            Self::OfferSlot { .. } => "offer_slot",
            Self::ReleaseSlot { .. } => "release_slot",
            Self::ResolveSlot { .. } => "resolve_slot",
        }
    }
}

/// One request: the schema, a correlation id the caller chooses, the
/// volume the call is about, the call. Idempotency is the DURABLE
/// state's, so `request_id` is for the log line and the reply's echo
/// only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerRequestFrame {
    pub schema: u32,
    pub request_id: u64,
    /// The target volume's ordinal in the routed set (the slot map's
    /// volume index — every client of the set knows it): one listener
    /// serves every volume a node manages (PR 4's mount-path wiring,
    /// [`ManagerSetService`]), so the frame names which. `0` on a
    /// one-volume set.
    pub volume: u16,
    pub call: ManagerCall,
}

/// The manager's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManagerReply {
    Joined {
        appender_id: u32,
        /// Device offset of the page's directory slot A.
        page_addr: u64,
        /// The ring's segment table, `(start, len)` bytes each.
        ring_segments: Vec<(u64, u64)>,
        /// The grant's runs.
        grant: Vec<WireRun>,
        /// The page was already `Live` under this identity (KD-SYM-7).
        already: bool,
    },
    Granted {
        runs: Vec<WireRun>,
    },
    Returned {
        cleared: u64,
        /// Extents the record no longer granted — a replay's no-op.
        already: u64,
    },
    /// Slots granted (or re-answered — `already` = every slot named was
    /// already the caller's, KD-SYM-7).
    SlotsGranted {
        slots: Vec<WireSlotGrant>,
        already: bool,
    },
    /// Another appender holds the slot: ship to it (the metanode arm).
    SlotRefused {
        slot: u16,
        holder: u32,
        g: u32,
    },
    /// The offer is recorded at the manager.
    Offered,
    /// The release landed in tree 0 (`already` = it had — a replay).
    Released {
        already: bool,
    },
    /// `ResolveSlot`: the lessee.
    Holder {
        appender_id: u32,
        g: u32,
    },
    /// `ResolveSlot`: nobody leases it.
    Unleased {
        g: u32,
    },
    /// The durable witness contradicts the caller, or the manager could
    /// not perform the verb; `reason` is operator-facing.
    Refused {
        reason: String,
    },
    /// Not now: the verb could not be served on this schedule (a grant
    /// deferred behind ring 0's window), nothing was written, the volume
    /// is healthy — retry. `reason` names the schedule.
    Deferred {
        reason: String,
    },
}

/// One reply frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerReplyFrame {
    pub schema: u32,
    pub request_id: u64,
    pub reply: ManagerReply,
}

/// Decode-side allocation bound: the CONTROL class cap, inside the body
/// too (the S8 vocabulary's discipline).
fn decode_limit() -> u64 {
    u64::from(crate::cluster_wire::CONTROL_MAX_FRAME_BYTES)
}

fn encode<T: Serialize>(value: &T, what: &str) -> Result<Vec<u8>> {
    let body = bincode::DefaultOptions::new()
        .serialize(value)
        .map_err(|e| SqueezefsError::InvalidOperation(format!("manager {what} encode: {e}")))?;
    if body.len() as u64 > decode_limit() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "manager {what} of {} B exceeds the cluster wire's CONTROL class cap ({} B)",
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
                "manager {what}: undecodable frame body ({} B): {e}",
                bytes.len()
            ))
        })
}

/// Encode a request frame (trusted — we built it).
pub fn encode_request(frame: &ManagerRequestFrame) -> Result<Vec<u8>> {
    encode(frame, "request")
}

/// Decode a request frame (**untrusted** — bounded).
pub fn decode_request(bytes: &[u8]) -> Result<ManagerRequestFrame> {
    decode(bytes, "request")
}

/// Encode a reply frame.
pub fn encode_reply(frame: &ManagerReplyFrame) -> Result<Vec<u8>> {
    encode(frame, "reply")
}

/// Decode a reply frame (**untrusted** — bounded).
pub fn decode_reply(bytes: &[u8]) -> Result<ManagerReplyFrame> {
    decode(bytes, "reply")
}

/// The manager's service: the verbs executed against ONE volume's
/// durable state (`KvMetaBackend`'s `manager_*` executors), served on
/// the RPC listener's connection lane. Every served verb records its
/// `admit / execute / reply` phases on the volume's manager ledger
/// (`manager_service_ns`, exact-sum).
pub struct ManagerService {
    volume: Arc<KvMetaBackend>,
}

impl ManagerService {
    pub fn new(volume: Arc<KvMetaBackend>) -> Arc<Self> {
        Arc::new(Self { volume })
    }

    fn refuse(&self, id: u64, status: u16, reason: String) -> RpcResponse {
        log::warn!("manager service refused a frame: {reason}");
        RpcResponse {
            id,
            status,
            body: reason.into_bytes(),
        }
    }

    async fn serve(&self, req: RpcRequest) -> RpcResponse {
        let t_admit = Instant::now();
        if req.verb != VERB_MANAGER_CALL {
            return RpcResponse {
                id: req.id,
                status: crate::cluster_wire::RPC_UNKNOWN_VERB,
                body: format!("manager: unknown verb {}", req.verb).into_bytes(),
            };
        }
        let frame = match decode_request(&req.body) {
            Ok(f) => f,
            Err(e) => return self.refuse(req.id, STATUS_MALFORMED, e.to_string()),
        };
        self.serve_frame(req.id, frame, t_admit).await
    }

    /// Serve an already-decoded frame (the set dispatcher decodes once to
    /// read the volume ordinal, then hands the frame here).
    async fn serve_frame(
        &self,
        req_id: u64,
        frame: ManagerRequestFrame,
        t_admit: Instant,
    ) -> RpcResponse {
        let req = RpcRequest {
            id: req_id,
            verb: VERB_MANAGER_CALL,
            body: Vec::new(),
        };
        if frame.schema != MANAGER_SCHEMA {
            return self.refuse(
                req.id,
                STATUS_SCHEMA,
                format!(
                    "peer speaks manager vocabulary schema {} and this manager speaks \
                     {MANAGER_SCHEMA} — refusing rather than guessing at a grant-bearing frame",
                    frame.schema
                ),
            );
        }
        let Some(set) = self.volume.appenders_public() else {
            return self.refuse(
                req.id,
                STATUS_NOT_MANAGER,
                "this volume is not a symmetric-forest volume (bit 17 absent)".to_string(),
            );
        };
        let admit_ns = t_admit.elapsed().as_nanos() as u64;
        let t_execute = Instant::now();
        // Every executor validates the frame's integers against DURABLE
        // state before anything proportional to them is allocated
        // (`want` clamped to the derivation's cap; return runs checked
        // against the volume and the caller's record run by run; the
        // slot verbs' identity and words screened at their wire faces —
        // review round 6, Issue 29) — "bounded codec = bounded
        // execution" (Issue 2). A rejection is its own status so the peer
        // can tell its own defect from a witness refusal: ONE classifier
        // over every verb's error (before round 6 only `ReturnExtents`
        // read the class, so every other verb's `Rejected` left as
        // `STATUS_REFUSED`).
        let served: std::result::Result<ManagerReply, crate::meta_backend::kv::KvError> =
            match &frame.call {
                ManagerCall::JoinAppender {
                    identity,
                    ring_want_bytes,
                } => self
                    .volume
                    .manager_join_appender((*identity).into(), *ring_want_bytes)
                    .await
                    .map(
                        |JoinOutcome {
                             appender_id,
                             page_addr,
                             ring_segments,
                             grant,
                             already,
                         }| ManagerReply::Joined {
                            appender_id,
                            page_addr,
                            ring_segments: ring_segments.iter().map(|s| (s.start, s.len)).collect(),
                            grant: runs_to_wire(&grant),
                            already,
                        },
                    ),
                ManagerCall::ExtentGrant { appender_id, want } => self
                    .volume
                    .manager_extent_grant(*appender_id, *want)
                    .await
                    .map(|runs| ManagerReply::Granted {
                        runs: runs_to_wire(&runs),
                    }),
                // The runs travel as RUNS: the executor intersects them with
                // the record run by run; the only allocation proportional to
                // the frame here is the run list itself, bounded by the frame
                // cap.
                ManagerCall::ReturnExtents { appender_id, runs } => self
                    .volume
                    .manager_return_runs(*appender_id, &runs_from_wire(runs))
                    .await
                    .map(|(cleared, already)| ManagerReply::Returned { cleared, already }),
                ManagerCall::AcquireSlots { appender_id, want } => self
                    .volume
                    .manager_acquire_slots_wire(*appender_id, *want)
                    .await
                    .map(|(slots, already)| ManagerReply::SlotsGranted { slots, already }),
                ManagerCall::AcquireSlot { appender_id, slot } => {
                    self.volume
                        .manager_acquire_slot_wire(*appender_id, *slot)
                        .await
                }
                ManagerCall::OfferSlot {
                    appender_id,
                    slot,
                    to,
                } => self
                    .volume
                    .manager_offer_slot_wire(*appender_id, *slot, *to)
                    .await
                    .map(|()| ManagerReply::Offered),
                ManagerCall::ReleaseSlot {
                    appender_id,
                    slot,
                    g,
                    words,
                    tails,
                } => self
                    .volume
                    .manager_release_slot_wire(*appender_id, *slot, *g, *words, tails)
                    .await
                    .map(|already| ManagerReply::Released { already }),
                ManagerCall::ResolveSlot { slot } => self.volume.manager_resolve_slot_wire(*slot),
            };
        let (reply, status) = match served {
            Ok(reply) => (reply, STATUS_OK),
            Err(e @ crate::meta_backend::kv::KvError::Rejected(_)) => (
                ManagerReply::Refused {
                    reason: e.to_string(),
                },
                STATUS_REJECTED,
            ),
            Err(e @ crate::meta_backend::kv::KvError::GrantDeferred { .. }) => (
                ManagerReply::Deferred {
                    reason: e.to_string(),
                },
                STATUS_DEFERRED,
            ),
            Err(e) => (
                ManagerReply::Refused {
                    reason: e.to_string(),
                },
                STATUS_REFUSED,
            ),
        };
        let execute_ns = t_execute.elapsed().as_nanos() as u64;
        let t_reply = Instant::now();
        let body = match encode_reply(&ManagerReplyFrame {
            schema: MANAGER_SCHEMA,
            request_id: frame.request_id,
            reply,
        }) {
            Ok(b) => b,
            Err(e) => return self.refuse(req.id, STATUS_MALFORMED, format!("reply encode: {e}")),
        };
        let reply_ns = t_reply.elapsed().as_nanos() as u64;
        set.verbs.record(
            admit_ns,
            execute_ns,
            reply_ns,
            crate::mono_core::monotonic_ns_u64(),
        );
        log::debug!(
            "manager served {} (request {}) in {} µs{}",
            frame.call.name(),
            frame.request_id,
            (admit_ns + execute_ns + reply_ns) / 1000,
            match status {
                STATUS_REJECTED => " — REJECTED",
                STATUS_DEFERRED => " — DEFERRED",
                STATUS_REFUSED => " — REFUSED",
                _ => "",
            }
        );
        RpcResponse {
            id: req.id,
            status,
            body,
        }
    }
}

impl RpcAsyncService for ManagerService {
    fn call<'a>(
        &'a self,
        req: RpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcResponse> + Send + 'a>> {
        Box::pin(self.serve(req))
    }
}

/// **The mount path's manager service** (PR 4, deliverable 4 — PR 3's
/// owed wiring): ONE service on the S8 listener for every metadata volume
/// this node manages, dispatching by the frame's `volume` ordinal to that
/// volume's [`ManagerService`]. A frame naming an ordinal the set does
/// not have is refused `STATUS_NOT_MANAGER` naming the width; a volume
/// whose plane is not armed answers through its own service's refusals
/// (`appenders_public()` / the manager gate). Built by the multi-writer
/// arm over the routed set's volumes when any of them armed the
/// symmetric plane (`AsyncVerbRouter::with_manager`), so the manager
/// verbs ride the venue every other owner verb rides.
pub struct ManagerSetService {
    volumes: Vec<Arc<ManagerService>>,
}

impl ManagerSetService {
    pub fn new(volumes: &[Arc<KvMetaBackend>]) -> Arc<Self> {
        Arc::new(Self {
            volumes: volumes
                .iter()
                .map(|v| ManagerService::new(Arc::clone(v)))
                .collect(),
        })
    }

    /// The set's width (volumes served).
    pub fn width(&self) -> usize {
        self.volumes.len()
    }

    async fn serve(&self, req: RpcRequest) -> RpcResponse {
        let t_admit = Instant::now();
        if req.verb != VERB_MANAGER_CALL {
            return RpcResponse {
                id: req.id,
                status: crate::cluster_wire::RPC_UNKNOWN_VERB,
                body: format!("manager: unknown verb {}", req.verb).into_bytes(),
            };
        }
        let frame = match decode_request(&req.body) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("manager set service refused a frame: {e}");
                return RpcResponse {
                    id: req.id,
                    status: STATUS_MALFORMED,
                    body: e.to_string().into_bytes(),
                };
            }
        };
        let Some(svc) = self.volumes.get(usize::from(frame.volume)) else {
            let reason = format!(
                "manager frame names volume ordinal {} on a set of {} volume(s)",
                frame.volume,
                self.volumes.len()
            );
            log::warn!("manager set service refused a frame: {reason}");
            return RpcResponse {
                id: req.id,
                status: STATUS_NOT_MANAGER,
                body: reason.into_bytes(),
            };
        };
        svc.serve_frame(req.id, frame, t_admit).await
    }
}

impl RpcAsyncService for ManagerSetService {
    fn call<'a>(
        &'a self,
        req: RpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcResponse> + Send + 'a>> {
        Box::pin(self.serve(req))
    }
}

/// The client half: one authenticated session to a manager's endpoint,
/// one call per verb. The S8 `RpcClient` underneath — the same dial, the
/// same proof of storage membership, the same per-frame MAC. First
/// product caller: PR 4's mount-path joiner (a co-appender's open dials
/// the manager and joins); the contract suite drives it today.
pub struct ManagerClient {
    rpc: RpcClient,
    next_request: u64,
    /// The volume ordinal every frame of this client names
    /// ([`ManagerRequestFrame::volume`] — the slot map's volume index,
    /// which [`ManagerSetService`] dispatches on).
    volume: u16,
}

impl std::fmt::Debug for ManagerClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagerClient")
            .field("rpc", &self.rpc)
            .finish()
    }
}

impl ManagerClient {
    /// Dial `endpoint` and prove membership with the set's `job:enroll`
    /// secret; every frame addresses the set's volume ordinal `volume`.
    pub async fn connect(
        endpoint: &str,
        secret: &[u8],
        peer_id: &str,
        volume: u16,
    ) -> Result<Self> {
        let rpc = RpcClient::connect(endpoint, secret, peer_id, None).await?;
        Ok(Self {
            rpc,
            next_request: 1,
            volume,
        })
    }

    /// Issue one verb; a `Refused` reply is an error naming its reason.
    pub async fn call(&mut self, call: ManagerCall) -> Result<ManagerReply> {
        let request_id = self.next_request;
        self.next_request += 1;
        let body = encode_request(&ManagerRequestFrame {
            schema: MANAGER_SCHEMA,
            request_id,
            volume: self.volume,
            call,
        })?;
        let resp = self.rpc.call(VERB_MANAGER_CALL, body).await?;
        match resp.status {
            STATUS_OK | STATUS_REFUSED | STATUS_REJECTED | STATUS_DEFERRED => {}
            other => {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "manager refused the frame with status {other}: {}",
                    String::from_utf8_lossy(&resp.body)
                )));
            }
        }
        let frame = decode_reply(&resp.body)?;
        if frame.schema != MANAGER_SCHEMA {
            return Err(SqueezefsError::InvalidOperation(format!(
                "manager answered in vocabulary schema {} (ours is {MANAGER_SCHEMA})",
                frame.schema
            )));
        }
        if frame.request_id != request_id {
            return Err(SqueezefsError::InvalidOperation(format!(
                "manager reply echoes request {} for request {request_id}",
                frame.request_id
            )));
        }
        Ok(frame.reply)
    }

    /// `JoinAppender` for `identity`.
    pub async fn join(
        &mut self,
        identity: AppenderIdentity,
        ring_want_bytes: u64,
    ) -> Result<ManagerReply> {
        self.call(ManagerCall::JoinAppender {
            identity: identity.into(),
            ring_want_bytes,
        })
        .await
    }

    /// `ExtentGrant` for `appender_id`.
    pub async fn extent_grant(&mut self, appender_id: u32, want: u32) -> Result<Vec<GrantRun>> {
        match self
            .call(ManagerCall::ExtentGrant { appender_id, want })
            .await?
        {
            ManagerReply::Granted { runs } => Ok(runs_from_wire(&runs)),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "ExtentGrant answered {other:?}"
            ))),
        }
    }

    /// `ReturnExtents` for `appender_id`.
    pub async fn return_extents(
        &mut self,
        appender_id: u32,
        runs: &[GrantRun],
    ) -> Result<(u64, u64)> {
        match self
            .call(ManagerCall::ReturnExtents {
                appender_id,
                runs: runs_to_wire(runs),
            })
            .await?
        {
            ManagerReply::Returned { cleared, already } => Ok((cleared, already)),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "ReturnExtents answered {other:?}"
            ))),
        }
    }
}

impl ManagerClient {
    /// `AcquireSlots` for `appender_id` (`want` 0 = the manager's `M`).
    pub async fn acquire_slots(
        &mut self,
        appender_id: u32,
        want: u16,
    ) -> Result<(Vec<WireSlotGrant>, bool)> {
        match self
            .call(ManagerCall::AcquireSlots { appender_id, want })
            .await?
        {
            ManagerReply::SlotsGranted { slots, already } => Ok((slots, already)),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            // The retry class, typed by its errno: nothing was written.
            ManagerReply::Deferred { reason } => Err(SqueezefsError::refused(libc::EAGAIN, reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "AcquireSlots answered {other:?}"
            ))),
        }
    }

    /// `AcquireSlot` for `appender_id` — `Ok(reply)` is one of
    /// `SlotsGranted` / `SlotRefused` / `Deferred` (the retry class).
    pub async fn acquire_slot(&mut self, appender_id: u32, slot: u16) -> Result<ManagerReply> {
        match self
            .call(ManagerCall::AcquireSlot { appender_id, slot })
            .await?
        {
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Ok(other),
        }
    }

    /// `OfferSlot`: the holder `appender_id` offers `slot` to `to`.
    pub async fn offer_slot(&mut self, appender_id: u32, slot: u16, to: u32) -> Result<()> {
        match self
            .call(ManagerCall::OfferSlot {
                appender_id,
                slot,
                to,
            })
            .await?
        {
            ManagerReply::Offered => Ok(()),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "OfferSlot answered {other:?}"
            ))),
        }
    }

    /// `ReleaseSlot` — `Ok(already)`.
    pub async fn release_slot(
        &mut self,
        appender_id: u32,
        slot: u16,
        g: u32,
        words: WireSlotWords,
        tails: Vec<(u64, u32)>,
    ) -> Result<bool> {
        match self
            .call(ManagerCall::ReleaseSlot {
                appender_id,
                slot,
                g,
                words,
                tails,
            })
            .await?
        {
            ManagerReply::Released { already } => Ok(already),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "ReleaseSlot answered {other:?}"
            ))),
        }
    }

    /// `ResolveSlot` — `Ok(reply)` is `Holder` or `Unleased`.
    pub async fn resolve_slot(&mut self, slot: u16) -> Result<ManagerReply> {
        match self.call(ManagerCall::ResolveSlot { slot }).await? {
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Ok(other),
        }
    }
}

/// The ring segments a `Joined` reply names, as extents — what PR 4's
/// joiner builds its `JournalRing` from (first product caller); the
/// contract suite's reader today.
pub fn joined_segments(reply: &ManagerReply) -> Vec<ExtentRef> {
    match reply {
        ManagerReply::Joined { ring_segments, .. } => ring_segments
            .iter()
            .map(|&(start, len)| ExtentRef { start, len })
            .collect(),
        _ => Vec::new(),
    }
}
