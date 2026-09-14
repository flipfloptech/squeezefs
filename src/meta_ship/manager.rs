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
//! What is NOT here (later rungs, §6.3): `AcquireSlot` / `AcquireSlots` /
//! `OfferSlot` / `ReleaseSlot` / `ResolveSlot` (PR 4 — slot leases),
//! `RecordDeath` / `RecordRecovered` (PR 8/10 — the death ledger),
//! `DirRenameLock` (PR 6). They extend this enum under the same schema
//! while the wire is unreleased; a release in between bumps
//! `MANAGER_SCHEMA` again.

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
}

impl ManagerCall {
    /// The verb's name (log lines, the phase table).
    pub fn name(&self) -> &'static str {
        match self {
            Self::JoinAppender { .. } => "join_appender",
            Self::ExtentGrant { .. } => "extent_grant",
            Self::ReturnExtents { .. } => "return_extents",
        }
    }
}

/// One request: the schema, a correlation id the caller chooses, the
/// call. Idempotency is the DURABLE state's, so `request_id` is for the
/// log line and the reply's echo only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerRequestFrame {
    pub schema: u32,
    pub request_id: u64,
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
    /// The durable witness contradicts the caller, or the manager could
    /// not perform the verb; `reason` is operator-facing.
    Refused {
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
        let (reply, refusal) = match &frame.call {
            ManagerCall::JoinAppender {
                identity,
                ring_want_bytes,
            } => match self
                .volume
                .manager_join_appender((*identity).into(), *ring_want_bytes)
                .await
            {
                Ok(JoinOutcome {
                    appender_id,
                    page_addr,
                    ring_segments,
                    grant,
                    already,
                }) => (
                    ManagerReply::Joined {
                        appender_id,
                        page_addr,
                        ring_segments: ring_segments.iter().map(|s| (s.start, s.len)).collect(),
                        grant: runs_to_wire(&grant),
                        already,
                    },
                    false,
                ),
                Err(e) => (
                    ManagerReply::Refused {
                        reason: e.to_string(),
                    },
                    true,
                ),
            },
            ManagerCall::ExtentGrant { appender_id, want } => {
                match self.volume.manager_extent_grant(*appender_id, *want).await {
                    Ok(runs) => (
                        ManagerReply::Granted {
                            runs: runs_to_wire(&runs),
                        },
                        false,
                    ),
                    Err(e) => (
                        ManagerReply::Refused {
                            reason: e.to_string(),
                        },
                        true,
                    ),
                }
            }
            ManagerCall::ReturnExtents { appender_id, runs } => {
                let extents: Vec<u64> = runs_from_wire(runs)
                    .iter()
                    .flat_map(|r| r.start..r.start + u64::from(r.len))
                    .collect();
                match self
                    .volume
                    .manager_return_extents(*appender_id, &extents)
                    .await
                {
                    Ok((cleared, already)) => (ManagerReply::Returned { cleared, already }, false),
                    Err(e) => (
                        ManagerReply::Refused {
                            reason: e.to_string(),
                        },
                        true,
                    ),
                }
            }
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
            if refusal { " — REFUSED" } else { "" }
        );
        RpcResponse {
            id: req.id,
            status: if refusal { STATUS_REFUSED } else { STATUS_OK },
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

/// The client half: one authenticated session to a manager's endpoint,
/// one call per verb. The S8 `RpcClient` underneath — the same dial, the
/// same proof of storage membership, the same per-frame MAC.
pub struct ManagerClient {
    rpc: RpcClient,
    next_request: u64,
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
    /// secret.
    pub async fn connect(endpoint: &str, secret: &[u8], peer_id: &str) -> Result<Self> {
        let rpc = RpcClient::connect(endpoint, secret, peer_id, None).await?;
        Ok(Self {
            rpc,
            next_request: 1,
        })
    }

    /// Issue one verb; a `Refused` reply is an error naming its reason.
    pub async fn call(&mut self, call: ManagerCall) -> Result<ManagerReply> {
        let request_id = self.next_request;
        self.next_request += 1;
        let body = encode_request(&ManagerRequestFrame {
            schema: MANAGER_SCHEMA,
            request_id,
            call,
        })?;
        let resp = self.rpc.call(VERB_MANAGER_CALL, body).await?;
        match resp.status {
            STATUS_OK | STATUS_REFUSED => {}
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

/// The ring segments a `Joined` reply names, as extents.
pub fn joined_segments(reply: &ManagerReply) -> Vec<ExtentRef> {
    match reply {
        ManagerReply::Joined { ring_segments, .. } => ring_segments
            .iter()
            .map(|&(start, len)| ExtentRef { start, len })
            .collect(),
        _ => Vec::new(),
    }
}
