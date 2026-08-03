//! DLM **stage S9** — the **publish vocabulary**: the daemon's *non-trait*
//! metadata surface on the wire, so a foreign-home inode's layout publish
//! is SHIPPED instead of refused.
//!
//! # Why this exists as its own vocabulary
//!
//! S8 shipped the 13 `Metadata` trait verbs and stated the gap in its own
//! words: *"the FUSE daemon is not switched onto the router. It holds
//! `Arc<RoutedMetaBackend>` and uses the non-trait capability surface
//! (`create_with_rdev_size`, `readdir_stream`, `xattr_value_cap`,
//! `set_layout_and_size`, `merge_layout_and_size`, `commit_block_refs`,
//! `park_write_times`, `destroy_inodes`) that S8 deliberately does not ship
//! — those are the data plane's publish path. Wiring the daemon is
//! therefore an S9 deliverable."*
//!
//! It is a **separate** vocabulary rather than eight more `MetaCall`
//! variants for one reason: S8's `META_SHIP_SCHEMA` is a landed, pinned
//! contract, and growing it would bump the schema of every metadata frame
//! for verbs no S8 peer will ever send. Additive is the same discipline
//! [`crate::membership_wire::VerbRouter`] applies to verb ranges, one layer
//! up.
//!
//! # The routing decision, and its cost when nothing is armed
//!
//! Exactly S8's: one relaxed load ([`super::ownership_armed`]) and then
//! today's call, verbatim. An unarmed mount — every mount that ships —
//! pays that load and nothing else. The eight call sites in
//! `routing.rs`/`fuse_client.rs` therefore keep their shape: the helper
//! they now call *is* the local method plus that load.
//!
//! # What this vocabulary deliberately does NOT do
//!
//! * **No retry.** S8's owner-side dedup window is what makes a resend
//!   safe; this vocabulary has none, and its one non-idempotent verb
//!   (`create_with_rdev_size`) would create a second name on a resend after
//!   a lost reply. So a transport failure is REPORTED, never re-applied —
//!   the writeback ladder above already re-publishes from current state,
//!   and a layout delta is a *final-state* record (absolute
//!   `(block_index, key)` inserts), so re-publishing converges instead of
//!   double-applying. A durable reply cache is S3.5's machinery; the
//!   retry+window extension is a named residual, not a silent gap.
//! * **No batching conveyor.** The ops that arrive here are already
//!   coalesced (the M7 commit conveyor, the publish coalescer's
//!   `publish_commit_group*`), so a second batching layer would coalesce
//!   already-coalesced work. S8's lane shape is the precedent if a measured
//!   row ever asks for one.
//! * **No cross-owner transaction.** `destroy_inodes` is per-ino by
//!   construction and groups by owner; nothing here spans two authorities
//!   inside one transaction (that is S3.5, and S8's `cross_owner_error` is
//!   the refusal wherever it can be reached).

use super::wire::{WireDirEntry, WireError, WireInode};
use crate::cluster_wire::{
    RpcAsyncService, RpcClient, RpcRequest, RpcResponse, RPC_OK, RPC_UNKNOWN_VERB,
};
use crate::error::{Result, SqueezefsError};
use crate::meta_backend::kv::block_refs::{BlockRef, BlockRefOp};
use crate::meta_backend::{DirEntry, Ino, Inode, RoutedMetaBackend};
use bincode::Options as _;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// The publish vocabulary's schema — independent of S8's
/// (`META_SHIP_SCHEMA`) and of the transport's.
///
/// **2 since the co-writer allocation lane landed** (`RaiseAllocLane` joined
/// the call enum). The bump is deliberate rather than a silent additive
/// variant: a peer that speaks 1 cannot serve a reservation raise, and the
/// honest answer is `PUBLISH_SCHEMA_MISMATCH` naming both numbers at the
/// first frame — not an undecodable body refused as malformed halfway
/// through a mount's first allocation.
pub const PUBLISH_SCHEMA: u32 = 2;

/// First verb of S9's publish block. S3's ping is 0, S8's metadata verbs
/// are 16/17, S6's membership owns `0x0100..=0x01FF`, S9's custody
/// `0x0200..=0x02FF`; the publish path takes `0x0300..=0x03FF`.
pub const VERB_PUBLISH_BASE: u16 = 0x0300;
/// Last verb of S9's publish block.
pub const VERB_PUBLISH_LAST: u16 = 0x03FF;
/// One publish call per frame (see the module docs on batching).
pub const VERB_PUBLISH_CALL: u16 = VERB_PUBLISH_BASE;

/// Status: the call was executed and its own outcome is in the body.
pub const PUBLISH_OK: u16 = RPC_OK;
/// Status: vocabulary schema mismatch.
pub const PUBLISH_SCHEMA_MISMATCH: u16 = 0x51;
/// Status: undecodable body.
pub const PUBLISH_MALFORMED: u16 = 0x52;
/// Status: the owner holds no authority over the named object — the
/// client's ownership map is stale.
pub const PUBLISH_NOT_OWNER: u16 = 0x53;
/// Status: the owner-side execution PANICKED (must stay 0).
pub const PUBLISH_PANIC: u16 = 0x54;
/// Status: a reservation raise named a lane this client was not assigned, a
/// width the authority does not run, or a lease that is not custody — the
/// **must-stay-0** class (`alloc_lane_raise_refusals`).
pub const PUBLISH_LANE_REFUSED: u16 = 0x55;

/// One durable block-reference operation on the wire — a mirror of
/// [`BlockRefOp`] on purpose: the internal struct may gain fields without
/// silently changing a wire format, and this vocabulary's schema is what
/// versions this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireBlockRefOp {
    pub vol_tag: u64,
    pub block_idx: u64,
    pub owner_ino: u64,
    pub block_index: u32,
    pub take: bool,
}

impl From<&BlockRefOp> for WireBlockRefOp {
    fn from(op: &BlockRefOp) -> Self {
        Self {
            vol_tag: op.reference.vol_tag,
            block_idx: op.reference.block_idx,
            owner_ino: op.reference.owner_ino,
            block_index: op.reference.block_index,
            take: op.take,
        }
    }
}

impl From<WireBlockRefOp> for BlockRefOp {
    fn from(w: WireBlockRefOp) -> Self {
        let reference = BlockRef {
            vol_tag: w.vol_tag,
            block_idx: w.block_idx,
            owner_ino: w.owner_ino,
            block_index: w.block_index,
        };
        if w.take {
            BlockRefOp::taken(reference)
        } else {
            BlockRefOp::released(reference)
        }
    }
}

/// The publish path's calls, arguments verbatim from the non-trait surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishCall {
    SetLayoutAndSize {
        ino: u64,
        layout: Vec<u8>,
        size: u64,
        refs: Vec<WireBlockRefOp>,
    },
    /// The delta travels **encoded** (`LayoutDelta::encode`), which is the
    /// same strict, bounded, magic-checked codec the on-disk record uses —
    /// so the wire cannot express a delta the volume could not store.
    MergeLayoutAndSize {
        ino: u64,
        delta: Vec<u8>,
        full_layout: Vec<u8>,
        size: u64,
        refs: Vec<WireBlockRefOp>,
    },
    CommitBlockRefs {
        ino: u64,
        refs: Vec<WireBlockRefOp>,
    },
    ParkWriteTimes {
        ino: u64,
        mtime: u64,
        ctime: u64,
    },
    DestroyInodes {
        inos: Vec<u64>,
    },
    CreateWithRdevSize {
        parent: u64,
        name: String,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
        initial_size: u64,
    },
    XattrValueCap {
        ino: u64,
    },
    ReaddirStream {
        dir: u64,
        offset: u64,
        max: u32,
    },
    /// **Raise (or OPEN) a data-plane allocation lane's durable reservation**
    /// — DLM S9's co-writer allocation seam
    /// (`docs/design-mw-data-alloc-partition.md` §3).
    ///
    /// The client names a **lane**, never a record: the owner derives the
    /// record's name from `(vol_tag, lane)` itself
    /// ([`crate::data_alloc_lane::lane_record_name`]), so this verb cannot
    /// address `writer_claim`, `claim_set`, `job:` or any other internal
    /// name — and it validates the lane against the assignment IT made
    /// before committing anything.
    ///
    /// `upto == 0` is the OPEN: commit nothing, answer the frontier this
    /// lane must resume at (the reply's [`PublishReply::LaneFrontier`]).
    RaiseAllocLane {
        /// The durable data-volume identity (KD-5's `vol-{hex}` as
        /// `block_refs::volume_tag` decodes it) — never a path, an ordinal
        /// or a set position.
        vol_tag: u64,
        lane: u16,
        writers: u16,
        upto: u64,
        /// The custody lease epoch the caller holds — minted by this
        /// authority, monotone, never reused, and handed only to the member
        /// it names. It is what makes the lane claim checkable rather than
        /// self-asserted.
        lease_epoch: u64,
    },
}

impl PublishCall {
    /// The verb's name (logs, refusals, the phase table).
    pub fn name(&self) -> &'static str {
        match self {
            Self::SetLayoutAndSize { .. } => "set_layout_and_size",
            Self::MergeLayoutAndSize { .. } => "merge_layout_and_size",
            Self::CommitBlockRefs { .. } => "commit_block_refs",
            Self::ParkWriteTimes { .. } => "park_write_times",
            Self::DestroyInodes { .. } => "destroy_inodes",
            Self::CreateWithRdevSize { .. } => "create_with_rdev_size",
            Self::XattrValueCap { .. } => "xattr_value_cap",
            Self::ReaddirStream { .. } => "readdir_stream",
            Self::RaiseAllocLane { .. } => "raise_alloc_lane",
        }
    }

    /// Every inode the call names — the authority check's input.
    pub fn named_inos(&self) -> Vec<u64> {
        match self {
            Self::SetLayoutAndSize { ino, .. }
            | Self::MergeLayoutAndSize { ino, .. }
            | Self::CommitBlockRefs { ino, .. }
            | Self::ParkWriteTimes { ino, .. }
            | Self::XattrValueCap { ino } => vec![*ino],
            Self::DestroyInodes { inos } => inos.clone(),
            Self::CreateWithRdevSize { parent, .. } => vec![*parent],
            Self::ReaddirStream { dir, .. } => vec![*dir],
            // The reservation record lives on ino 1 (KD-2's plane), so the
            // authority check is the same check every other verb gets: the
            // node serving it must hold authority over the volume ino 1
            // routes to.
            Self::RaiseAllocLane { .. } => vec![1],
        }
    }
}

/// One publish call's successful payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishReply {
    Unit,
    /// `merge_layout_and_size`: whether a delta record was staged.
    DeltaUsed(bool),
    /// `create_with_rdev_size`.
    Inode(WireInode),
    /// `xattr_value_cap`.
    Cap(u64),
    /// `readdir_stream`: `(resume cookie, entry)` pairs.
    Page(Vec<(u64, WireDirEntry)>),
    /// `raise_alloc_lane`: the durable reservation frontier now in force for
    /// that lane — an exclusive block-index bound this lane may mint below.
    LaneFrontier(u64),
}

/// A publish request frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishRequestFrame {
    pub schema: u32,
    /// The client's identity (logs and audit only — authentication is the
    /// transport's, and the storage-trust secret is the root).
    pub client: String,
    pub call: PublishCall,
}

/// A publish reply frame: the op's own outcome, errno-preserving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishReplyFrame {
    pub schema: u32,
    pub outcome: std::result::Result<PublishReply, WireError>,
}

fn decode_limit() -> u64 {
    u64::from(crate::cluster_wire::CONTROL_MAX_FRAME_BYTES)
}

fn encode<T: Serialize>(value: &T, what: &str) -> Result<Vec<u8>> {
    let body = bincode::DefaultOptions::new()
        .serialize(value)
        .map_err(|e| SqueezefsError::InvalidOperation(format!("S9 publish {what} encode: {e}")))?;
    if body.len() as u64 > decode_limit() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "S9 publish {what} of {} B exceeds the cluster wire's CONTROL class cap ({} B) — a \
             layout this large must ride the indirect map blob, not the wire",
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
                "S9 publish {what}: undecodable frame body ({} B): {e}",
                bytes.len()
            ))
        })
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

static SHIPPED: AtomicU64 = AtomicU64::new(0);
static LOCAL: AtomicU64 = AtomicU64::new(0);
static SERVED: AtomicU64 = AtomicU64::new(0);
static REFUSALS: AtomicU64 = AtomicU64::new(0);
static NOT_OWNER: AtomicU64 = AtomicU64::new(0);
static PANICS: AtomicU64 = AtomicU64::new(0);

/// The publish path's shipped-vs-local ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PublishStats {
    /// Publish calls that travelled to an owner.
    pub shipped: u64,
    /// Publish calls that took today's local path.
    pub local: u64,
    /// Publish calls executed FOR a peer (owner side).
    pub served: u64,
    /// Foreign-home calls REFUSED because no publish client is armed — a
    /// must-stay-0 tripwire on an armed multi-writer mount: it means the
    /// ownership plane was armed without its publish half, and the honest
    /// answer is a refusal rather than a silent local execution.
    pub refusals: u64,
    /// Frames refused because this node holds no authority over the target.
    pub not_owner: u64,
    /// Owner-side executions that UNWOUND (**must stay 0**).
    pub panics: u64,
}

/// Read the publish ledger.
pub fn stats() -> PublishStats {
    PublishStats {
        shipped: SHIPPED.load(Ordering::Relaxed),
        local: LOCAL.load(Ordering::Relaxed),
        served: SERVED.load(Ordering::Relaxed),
        refusals: REFUSALS.load(Ordering::Relaxed),
        not_owner: NOT_OWNER.load(Ordering::Relaxed),
        panics: PANICS.load(Ordering::Relaxed),
    }
}

/// The `meta_ship_publish` stats-inode object. Every field is 0 on a solo
/// mount BY CONSTRUCTION (nothing is armed, so nothing routes anywhere but
/// locally) — except `local`, which is the daemon's own publish traffic.
pub fn stats_json() -> serde_json::Value {
    let s = stats();
    serde_json::json!({
        "shipped": s.shipped,
        "local": s.local,
        "served": s.served,
        "refusals": s.refusals,
        "not_owner_refusals": s.not_owner,
        "owner_panics": s.panics,
    })
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// The client half: one authenticated session per authority endpoint, kept
/// warm.
pub struct PublishClient {
    peer_id: Arc<str>,
    secret: Arc<Vec<u8>>,
    sessions: scc::HashMap<String, Arc<tokio::sync::Mutex<Option<RpcClient>>>>,
}

impl std::fmt::Debug for PublishClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishClient")
            .field("peer_id", &self.peer_id)
            .field("sessions", &self.sessions.len())
            .finish_non_exhaustive()
    }
}

impl PublishClient {
    /// A client identifying itself as `peer_id`, proving storage membership
    /// with the volume set's `job:enroll` secret.
    pub fn new(peer_id: &str, secret: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            peer_id: Arc::from(peer_id),
            secret: Arc::new(secret),
            sessions: scc::HashMap::new(),
        })
    }

    fn lane(&self, endpoint: &str) -> Arc<tokio::sync::Mutex<Option<RpcClient>>> {
        if let Some(lane) = self.sessions.read_sync(endpoint, |_, l| Arc::clone(l)) {
            return lane;
        }
        let lane = Arc::new(tokio::sync::Mutex::new(None));
        match self
            .sessions
            .insert_sync(endpoint.to_string(), Arc::clone(&lane))
        {
            Ok(()) => lane,
            Err(_) => self
                .sessions
                .read_sync(endpoint, |_, l| Arc::clone(l))
                .unwrap_or(lane),
        }
    }

    /// Ship one call to `endpoint` and return its outcome.
    ///
    /// **One attempt, deliberately** — see the module docs: the vocabulary
    /// has no dedup window, so a resend of `create_with_rdev_size` after a
    /// lost reply would mint a second name.
    pub async fn ship(&self, endpoint: &str, call: PublishCall) -> Result<PublishReply> {
        let name = call.name();
        let body = encode(
            &PublishRequestFrame {
                schema: PUBLISH_SCHEMA,
                client: self.peer_id.to_string(),
                call,
            },
            name,
        )?;
        let lane = self.lane(endpoint);
        let mut guard = lane.lock().await;
        if guard.is_none() {
            *guard = Some(RpcClient::connect(endpoint, &self.secret, &self.peer_id, None).await?);
        }
        let out = guard
            .as_mut()
            .expect("connected above")
            .call(VERB_PUBLISH_CALL, body)
            .await;
        let reply = match out {
            Ok(reply) => reply,
            Err(e) => {
                *guard = None;
                return Err(e);
            }
        };
        drop(guard);
        SHIPPED.fetch_add(1, Ordering::Relaxed);
        if reply.status != PUBLISH_OK {
            return Err(SqueezefsError::InvalidOperation(format!(
                "S9: the owner at {endpoint} refused {name} (status {}): {}",
                reply.status,
                String::from_utf8_lossy(&reply.body)
            )));
        }
        let frame: PublishReplyFrame = decode(&reply.body, name)?;
        if frame.schema != PUBLISH_SCHEMA {
            return Err(SqueezefsError::InvalidOperation(format!(
                "S9: the owner at {endpoint} replied in publish schema {} (this build speaks \
                 {PUBLISH_SCHEMA})",
                frame.schema
            )));
        }
        frame.outcome.map_err(WireError::into_error)
    }
}

static CLIENT: Lazy<arc_swap::ArcSwapOption<PublishClient>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the process's publish client (the multi-writer mount arm's act).
pub fn install_client(client: Arc<PublishClient>) {
    CLIENT.store(Some(client));
}

/// Uninstall it (disarm / unmount / test teardown).
pub fn uninstall_client() {
    CLIENT.store(None);
}

// ---------------------------------------------------------------------------
// The routing helpers the daemon calls
// ---------------------------------------------------------------------------

/// The owner of `ino`'s volume, or `None` when this node owns it.
///
/// One relaxed load on an unarmed mount, which is every mount that ships.
#[inline]
fn owner_of(be: &Arc<RoutedMetaBackend>, ino: Ino) -> Option<Arc<super::PeerOwner>> {
    if !super::ownership_armed() {
        return None;
    }
    let (v_idx, _) = be.route_ino(ino);
    super::owner_of_volume(v_idx)
}

fn note_local() {
    LOCAL.fetch_add(1, Ordering::Relaxed);
}

/// Ship `call` to `peer`, or refuse loud when the ownership plane is armed
/// without its publish half.
///
/// Refusing is the only correct answer: executing a foreign-home publish
/// locally would append to a tree whose journal ring, extent bitmap and
/// root ledger belong to another writer — the silent divergence S4's
/// refusal and S8's mint constraint both exist to prevent.
async fn ship(peer: &Arc<super::PeerOwner>, call: PublishCall) -> Result<PublishReply> {
    let Some(client) = CLIENT.load_full() else {
        REFUSALS.fetch_add(1, Ordering::Relaxed);
        let msg = format!(
            "S9: {} on a volume owned by {} cannot be published — the ownership plane is armed \
             but no publish client is installed. Executing it locally would append to a tree \
             whose journal ring, extent bitmap and root ledger belong to another writer, so \
             this refuses instead (arm the multi-writer mount, which installs both halves)",
            call.name(),
            peer.peer_id
        );
        log::error!("{msg}");
        return Err(SqueezefsError::InvalidOperation(msg));
    };
    client.ship(&peer.endpoint, call).await
}

fn wire_refs(refs: &[BlockRefOp]) -> Vec<WireBlockRefOp> {
    refs.iter().map(WireBlockRefOp::from).collect()
}

fn expect_unit(reply: PublishReply, what: &str) -> Result<()> {
    match reply {
        PublishReply::Unit => Ok(()),
        other => Err(protocol_error(what, &format!("{other:?}"), "unit")),
    }
}

fn protocol_error(what: &str, got: &str, want: &str) -> SqueezefsError {
    let msg = format!(
        "S9 publish protocol violation: the owner answered {what} with {got}, but the verb's \
         reply shape is {want} — refusing rather than inventing a result"
    );
    log::error!("{msg}");
    SqueezefsError::InvalidOperation(msg)
}

/// Routed [`RoutedMetaBackend::set_layout_and_size`].
pub async fn set_layout_and_size(
    be: &Arc<RoutedMetaBackend>,
    ino: Ino,
    layout: &[u8],
    size: u64,
    refs: &[BlockRefOp],
) -> Result<()> {
    match owner_of(be, ino) {
        None => {
            note_local();
            be.set_layout_and_size(ino, layout, size, refs).await
        }
        Some(peer) => expect_unit(
            ship(
                &peer,
                PublishCall::SetLayoutAndSize {
                    ino,
                    layout: layout.to_vec(),
                    size,
                    refs: wire_refs(refs),
                },
            )
            .await?,
            "set_layout_and_size",
        ),
    }
}

/// Routed [`RoutedMetaBackend::merge_layout_and_size`].
pub async fn merge_layout_and_size(
    be: &Arc<RoutedMetaBackend>,
    ino: Ino,
    delta: &crate::layout_wire::LayoutDelta,
    full_layout: bytes::Bytes,
    size: u64,
    refs: Vec<BlockRefOp>,
) -> Result<bool> {
    match owner_of(be, ino) {
        None => {
            note_local();
            be.merge_layout_and_size(ino, delta, full_layout, size, refs)
                .await
        }
        Some(peer) => {
            let call = PublishCall::MergeLayoutAndSize {
                ino,
                delta: delta.encode(),
                full_layout: full_layout.to_vec(),
                size,
                refs: wire_refs(&refs),
            };
            match ship(&peer, call).await? {
                PublishReply::DeltaUsed(used) => Ok(used),
                other => Err(protocol_error(
                    "merge_layout_and_size",
                    &format!("{other:?}"),
                    "a delta-used flag",
                )),
            }
        }
    }
}

/// Routed [`RoutedMetaBackend::commit_block_refs`].
pub async fn commit_block_refs(
    be: &Arc<RoutedMetaBackend>,
    ino: Ino,
    refs: &[BlockRefOp],
) -> Result<()> {
    match owner_of(be, ino) {
        None => {
            note_local();
            be.commit_block_refs(ino, refs).await
        }
        Some(peer) => expect_unit(
            ship(
                &peer,
                PublishCall::CommitBlockRefs {
                    ino,
                    refs: wire_refs(refs),
                },
            )
            .await?,
            "commit_block_refs",
        ),
    }
}

/// Routed [`RoutedMetaBackend::park_write_times`].
pub async fn park_write_times(
    be: &Arc<RoutedMetaBackend>,
    ino: Ino,
    mtime: u64,
    ctime: u64,
) -> Result<()> {
    match owner_of(be, ino) {
        None => {
            note_local();
            be.park_write_times(ino, mtime, ctime).await
        }
        Some(peer) => expect_unit(
            ship(&peer, PublishCall::ParkWriteTimes { ino, mtime, ctime }).await?,
            "park_write_times",
        ),
    }
}

/// Routed [`RoutedMetaBackend::destroy_inodes`].
///
/// The set is grouped by owner — destroy is per-ino by construction, so a
/// set that spans two authorities is two independent commits, exactly as it
/// is two independent volume commits today.
pub async fn destroy_inodes(be: &Arc<RoutedMetaBackend>, inos: &[Ino]) -> Result<()> {
    if inos.is_empty() {
        return Ok(());
    }
    let mut local: Vec<Ino> = Vec::new();
    let mut remote: Vec<(Arc<super::PeerOwner>, Vec<Ino>)> = Vec::new();
    for &ino in inos {
        match owner_of(be, ino) {
            None => local.push(ino),
            Some(peer) => match remote.iter_mut().find(|(p, _)| p.endpoint == peer.endpoint) {
                Some((_, batch)) => batch.push(ino),
                None => remote.push((peer, vec![ino])),
            },
        }
    }
    if !local.is_empty() {
        note_local();
        be.destroy_inodes(&local).await?;
    }
    for (peer, batch) in remote {
        expect_unit(
            ship(&peer, PublishCall::DestroyInodes { inos: batch }).await?,
            "destroy_inodes",
        )?;
    }
    Ok(())
}

/// Routed [`RoutedMetaBackend::create_with_rdev_size`].
#[allow(clippy::too_many_arguments)]
pub async fn create_with_rdev_size(
    be: &Arc<RoutedMetaBackend>,
    parent: Ino,
    name: &str,
    mode: u32,
    uid: u32,
    gid: u32,
    rdev: u32,
    initial_size: u64,
) -> Result<Inode> {
    match owner_of(be, parent) {
        None => {
            note_local();
            be.create_with_rdev_size(parent, name, mode, uid, gid, rdev, initial_size)
                .await
        }
        Some(peer) => {
            let call = PublishCall::CreateWithRdevSize {
                parent,
                name: name.to_string(),
                mode,
                uid,
                gid,
                rdev,
                initial_size,
            };
            match ship(&peer, call).await? {
                PublishReply::Inode(i) => Ok(Inode::from(i)),
                other => Err(protocol_error(
                    "create_with_rdev_size",
                    &format!("{other:?}"),
                    "an inode",
                )),
            }
        }
    }
}

/// Routed [`RoutedMetaBackend::xattr_value_cap`].
///
/// A READ, but a routed one: the layout-inline ceiling
/// (`LAYOUT_INLINE_MAX`) derives from it, so answering with the LOCAL
/// volume's geometry for an object another volume stores would size the
/// caller's own record against the wrong cap.
pub async fn xattr_value_cap(be: &Arc<RoutedMetaBackend>, ino: Ino) -> Result<usize> {
    match owner_of(be, ino) {
        None => {
            note_local();
            Ok(be.xattr_value_cap(ino))
        }
        Some(peer) => match ship(&peer, PublishCall::XattrValueCap { ino }).await? {
            PublishReply::Cap(cap) => Ok(cap as usize),
            other => Err(protocol_error(
                "xattr_value_cap",
                &format!("{other:?}"),
                "a byte cap",
            )),
        },
    }
}

// ---------------------------------------------------------------------------
// DLM S9 — the co-writer's allocation lane (docs/design-mw-data-alloc-partition.md
// §3; contracts tests/mw_cowriter_lane_tests.rs). Kept in its own block: the
// verb is the ONLY metadata commit a co-writer's DATA path performs, and it
// is the one place a remote caller's value reaches a durable record.
// ---------------------------------------------------------------------------

/// **Raise (or OPEN) one allocation lane's durable reservation** — routed.
///
/// * **we hold the authority** (every mount that ships): the local commit,
///   monotone, exactly as [`crate::data_alloc_lane::commit_lane_raise`]
///   defines it, with no open floor (this mount's own recovery established
///   its floor);
/// * **a peer holds it** (a co-writer): the raise SHIPS, and the peer commits
///   it after checking that the lane is the one it assigned to us. That is
///   the whole answer to *"a lane reservation is a metadata commit and a
///   co-writer has no metadata authority"*: the co-writer does not write the
///   record — it asks the node that can, and the offset the record covers is
///   handed out only after the reply lands
///   (`BlockAllocator::hand_out_reserved`).
///
/// Returns the frontier now in force. `upto == 0` is the OPEN: it commits
/// nothing and answers where this lane must resume — the number a mount that
/// runs no ownership-recovery walk cannot compute for itself.
pub async fn raise_alloc_lane(
    be: &Arc<RoutedMetaBackend>,
    vol_tag: u64,
    lane: u16,
    writers: u16,
    upto: u64,
) -> Result<u64> {
    match owner_of(be, 1) {
        None => {
            note_local();
            crate::data_alloc_lane::commit_lane_raise(be, vol_tag, lane, writers, upto, None).await
        }
        Some(peer) => {
            // The lease epoch is this node's proof that the lane it names is
            // the lane it was granted: the authority minted it, it is
            // monotone and never reused, and it was handed only to us.
            let lease_epoch = crate::data_grant::custody_client()
                .map(|c| c.lease_epoch())
                .unwrap_or(0);
            let call = PublishCall::RaiseAllocLane {
                vol_tag,
                lane,
                writers,
                upto,
                lease_epoch,
            };
            let out = ship(&peer, call).await;
            match out {
                Ok(PublishReply::LaneFrontier(f)) => {
                    crate::fuse_client::METRICS
                        .alloc_lane_shipped_reservations
                        .fetch_add(1, Ordering::Relaxed);
                    Ok(f)
                }
                Ok(other) => Err(protocol_error(
                    "raise_alloc_lane",
                    &format!("{other:?}"),
                    "a lane frontier",
                )),
                Err(e) => {
                    crate::fuse_client::METRICS
                        .alloc_lane_raise_refusals
                        .fetch_add(1, Ordering::Relaxed);
                    log::error!(
                        "S9: the authority at {} refused this mount's allocation-lane raise \
                         (lane {lane} of {writers}, vol_tag {vol_tag:#016x}, upto {upto}): {e} — \
                         no offset is handed out, because an offset whose reservation is not \
                         durable is an offset a successor of this lane may mint again \
                         (alloc_lane_raise_refusals)",
                        peer.endpoint
                    );
                    Err(e)
                }
            }
        }
    }
}

/// Routed [`RoutedMetaBackend::readdir_stream`].
pub async fn readdir_stream(
    be: &Arc<RoutedMetaBackend>,
    dir: Ino,
    offset: u64,
    max: usize,
) -> Result<Vec<(u64, DirEntry)>> {
    match owner_of(be, dir) {
        None => {
            note_local();
            be.readdir_stream(dir, offset, max).await
        }
        Some(peer) => {
            let call = PublishCall::ReaddirStream {
                dir,
                offset,
                max: max.min(u32::MAX as usize) as u32,
            };
            match ship(&peer, call).await? {
                PublishReply::Page(rows) => Ok(rows
                    .into_iter()
                    .map(|(cookie, e)| (cookie, DirEntry::from(e)))
                    .collect()),
                other => Err(protocol_error(
                    "readdir_stream",
                    &format!("{other:?}"),
                    "a directory page",
                )),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The owner side
// ---------------------------------------------------------------------------

/// The publish path executed for a peer, against the volumes this node has
/// authority over.
///
/// The **venue rule** is S8's, verbatim and for the same mechanical reason:
/// the frame arrives on a pinned `sqz-cluster-svc{n}` lane, and the
/// execution is handed to the runtime that owns the backend's tasks —
/// because `commit_tx` spawns the per-volume conveyor pass task on the
/// committer's own runtime, and a verb executed inline on a lane would give
/// a volume's whole commit conveyor a runtime whose lifetime is the lane's.
pub struct PublishService {
    inner: Arc<RoutedMetaBackend>,
    runtime: tokio::runtime::Handle,
    authority: Vec<bool>,
    self_ref: std::sync::OnceLock<std::sync::Weak<PublishService>>,
}

impl std::fmt::Debug for PublishService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishService")
            .field("volumes", &self.authority.len())
            .field("stats", &stats())
            .finish_non_exhaustive()
    }
}

impl PublishService {
    /// A service with authority over every volume of `inner` (the shape a
    /// node that write-mounted the set has: it holds the D0 claim on each
    /// member).
    pub fn new(inner: Arc<RoutedMetaBackend>, runtime: tokio::runtime::Handle) -> Arc<Self> {
        let all: Vec<usize> = (0..inner.volumes.len()).collect();
        Self::with_authority(inner, runtime, &all)
    }

    /// [`Self::new`] with an explicit authority set — the shape a node that
    /// owns a SUBSET of the set has.
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
        let me = Arc::new(Self {
            inner,
            runtime,
            authority,
            self_ref: std::sync::OnceLock::new(),
        });
        let _ = me.self_ref.set(Arc::downgrade(&me));
        me
    }

    fn owned(&self) -> Option<Arc<Self>> {
        self.self_ref.get().and_then(|w| w.upgrade())
    }

    fn has_authority(&self, ino: u64) -> bool {
        let (v_idx, _) = self.inner.route_ino(ino);
        self.authority.get(v_idx).copied().unwrap_or(false)
    }

    fn refuse(id: u64, status: u16, reason: String) -> RpcResponse {
        log::warn!("S9 publish owner refused a frame: {reason}");
        RpcResponse {
            id,
            status,
            body: reason.into_bytes(),
        }
    }

    async fn serve(&self, req: RpcRequest) -> RpcResponse {
        if req.verb != VERB_PUBLISH_CALL {
            return RpcResponse {
                id: req.id,
                status: RPC_UNKNOWN_VERB,
                body: format!("S9: unknown publish verb {}", req.verb).into_bytes(),
            };
        }
        let frame: PublishRequestFrame = match decode(&req.body, "request") {
            Ok(f) => f,
            Err(e) => return Self::refuse(req.id, PUBLISH_MALFORMED, format!("{e}")),
        };
        if frame.schema != PUBLISH_SCHEMA {
            return Self::refuse(
                req.id,
                PUBLISH_SCHEMA_MISMATCH,
                format!(
                    "peer speaks publish schema {} and this owner speaks {PUBLISH_SCHEMA} — \
                     refusing rather than guessing at a layout-bearing frame",
                    frame.schema
                ),
            );
        }
        if let Some(foreign) = frame
            .call
            .named_inos()
            .into_iter()
            .find(|ino| !self.has_authority(*ino))
        {
            NOT_OWNER.fetch_add(1, Ordering::Relaxed);
            return Self::refuse(
                req.id,
                PUBLISH_NOT_OWNER,
                format!(
                    "ino {foreign} routes to a metadata volume this node holds no authority \
                     over — the client's ownership map is stale (re-read the volumes' \
                     writer_claim records)"
                ),
            );
        }
        // DLM S9's allocation-lane seam: the ONE verb whose argument reaches a
        // durable record from a REMOTE caller, so it is validated here —
        // against the assignment this authority itself made — before anything
        // is dispatched. The client names a lane; only the authority decides
        // whose lane it is.
        if let PublishCall::RaiseAllocLane {
            lane,
            writers,
            lease_epoch,
            ..
        } = &frame.call
        {
            if let Err(reason) =
                crate::data_grant::validate_lane_raise(&frame.client, *lease_epoch, *lane, *writers)
            {
                crate::fuse_client::METRICS
                    .alloc_lane_raise_refusals
                    .fetch_add(1, Ordering::Relaxed);
                return Self::refuse(req.id, PUBLISH_LANE_REFUSED, reason);
            }
        }
        let Some(me) = self.owned() else {
            return Self::refuse(
                req.id,
                PUBLISH_MALFORMED,
                "S9 publish service is shutting down — no handle to dispatch on".into(),
            );
        };
        let name = frame.call.name();
        let joined = self
            .runtime
            .spawn(async move { me.execute(frame.call).await })
            .await;
        let outcome = match joined {
            Ok(out) => out,
            Err(e) => {
                // An owner-side publish UNWOUND. Nothing joins a data-path
                // task, so this counter is the only record its work was
                // lost (the RES-7/RES-8 discipline).
                PANICS.fetch_add(1, Ordering::Relaxed);
                log::error!("S9 publish owner-side execution of {name} unwound: {e}");
                return RpcResponse {
                    id: req.id,
                    status: PUBLISH_PANIC,
                    body: format!("S9 publish owner-side execution panicked: {e}").into_bytes(),
                };
            }
        };
        SERVED.fetch_add(1, Ordering::Relaxed);
        let frame = PublishReplyFrame {
            schema: PUBLISH_SCHEMA,
            outcome: outcome.map_err(|e| WireError::from_error(&e)),
        };
        match encode(&frame, name) {
            Ok(body) => RpcResponse {
                id: req.id,
                status: PUBLISH_OK,
                body,
            },
            Err(e) => Self::refuse(req.id, PUBLISH_MALFORMED, format!("reply encode: {e}")),
        }
    }

    async fn execute(&self, call: PublishCall) -> Result<PublishReply> {
        match call {
            PublishCall::SetLayoutAndSize {
                ino,
                layout,
                size,
                refs,
            } => {
                let refs: Vec<BlockRefOp> = refs.into_iter().map(BlockRefOp::from).collect();
                self.inner
                    .set_layout_and_size(ino, &layout, size, &refs)
                    .await?;
                Ok(PublishReply::Unit)
            }
            PublishCall::MergeLayoutAndSize {
                ino,
                delta,
                full_layout,
                size,
                refs,
            } => {
                let delta = crate::layout_wire::LayoutDelta::decode(&delta).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "S9 publish: undecodable layout delta for ino {ino}: {e}"
                    ))
                })?;
                let refs: Vec<BlockRefOp> = refs.into_iter().map(BlockRefOp::from).collect();
                let used = self
                    .inner
                    .merge_layout_and_size(ino, &delta, bytes::Bytes::from(full_layout), size, refs)
                    .await?;
                Ok(PublishReply::DeltaUsed(used))
            }
            PublishCall::CommitBlockRefs { ino, refs } => {
                let refs: Vec<BlockRefOp> = refs.into_iter().map(BlockRefOp::from).collect();
                self.inner.commit_block_refs(ino, &refs).await?;
                Ok(PublishReply::Unit)
            }
            PublishCall::ParkWriteTimes { ino, mtime, ctime } => {
                self.inner.park_write_times(ino, mtime, ctime).await?;
                Ok(PublishReply::Unit)
            }
            PublishCall::DestroyInodes { inos } => {
                self.inner.destroy_inodes(&inos).await?;
                Ok(PublishReply::Unit)
            }
            PublishCall::CreateWithRdevSize {
                parent,
                name,
                mode,
                uid,
                gid,
                rdev,
                initial_size,
            } => {
                let inode = self
                    .inner
                    .create_with_rdev_size(parent, &name, mode, uid, gid, rdev, initial_size)
                    .await?;
                // The mint is constrained to an owned volume
                // (`owners::constrain_mint_volume`), so this is a check on
                // the OUTCOME rather than a hope: a child that landed
                // outside this node's authority would be a placement bug,
                // and it must be loud rather than silent.
                if !self.has_authority(inode.ino) {
                    log::error!(
                        "S9 publish: create minted ino {} outside this node's authority — the \
                         mint constraint failed (spec §6.10 R4)",
                        inode.ino
                    );
                }
                Ok(PublishReply::Inode(WireInode::from(&inode)))
            }
            PublishCall::XattrValueCap { ino } => {
                Ok(PublishReply::Cap(self.inner.xattr_value_cap(ino) as u64))
            }
            PublishCall::ReaddirStream { dir, offset, max } => {
                let rows = self.inner.readdir_stream(dir, offset, max as usize).await?;
                Ok(PublishReply::Page(
                    rows.iter()
                        .map(|(cookie, e)| (*cookie, WireDirEntry::from(e)))
                        .collect(),
                ))
            }
            PublishCall::RaiseAllocLane {
                vol_tag,
                lane,
                writers,
                upto,
                lease_epoch: _,
            } => {
                // The lane was validated in `serve`. The floor is the
                // recovery rule's answer computed HERE, from state only a
                // node with metadata authority (and a live cursor) has: the
                // durable reference ledger's dense frontier, this mount's own
                // cursor for that data volume, and every `alloc_lane:` record
                // the volume carries. A peer that never walks the tree cannot
                // compute it, which is why the OPEN exists.
                let floor = crate::data_alloc_lane::lane_open_floor(
                    &self.inner,
                    vol_tag,
                    lane,
                    writers,
                    crate::alloc_lane_grant::local_dense_frontier(vol_tag).unwrap_or(0),
                )
                .await?;
                let frontier = crate::data_alloc_lane::commit_lane_raise(
                    &self.inner,
                    vol_tag,
                    lane,
                    writers,
                    upto,
                    floor,
                )
                .await?;
                log::debug!(
                    "S9: served an allocation-lane raise for lane {lane} of {writers} on \
                     vol_tag {vol_tag:#016x} (asked {upto}, frontier now {frontier})"
                );
                Ok(PublishReply::LaneFrontier(frontier))
            }
        }
    }
}

impl RpcAsyncService for PublishService {
    fn call<'a>(
        &'a self,
        req: RpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcResponse> + Send + 'a>> {
        Box::pin(self.serve(req))
    }
}
