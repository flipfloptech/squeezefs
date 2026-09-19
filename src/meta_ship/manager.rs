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
use crate::meta_backend::kv::shared_refs;
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

    // ---- PR 7 — the clone protocol's verbs (design §5.4.4; the design's
    // discriminant range 0x70–0x7F — bincode carries the variant index,
    // and every PR of this level appends its block at the end, so the
    // ranges name the BLOCKS; `MANAGER_SCHEMA` versions the wire). ----
    /// **Step 1, served by the SOURCE ino's slot holder**: set the SHARED
    /// bit on `owner_ino`'s reference to `(vol_tag, block_idx,
    /// block_index)` under its 4a guard — or `SharedGone` when no record
    /// exists (the cloner aborts). `owner_ino` is the frame volume's
    /// LOCAL KEY form (the kv layer's key identity).
    MarkShared {
        vol_tag: u64,
        block_idx: u64,
        owner_ino: u64,
        block_index: u32,
    },
    /// **Step 2, served by the index HOME**: one index entry per
    /// `(owner_ino, block_index)` — the source's AND the target's — for
    /// block `(vol_tag, block_idx)`; idempotent.
    ShareBlock {
        vol_tag: u64,
        block_idx: u64,
        refs: Vec<(u64, u32)>,
    },
    /// **Step 4, served by the index HOME**: the releasing reference —
    /// `(owner_ino, block_index)`, ONE entry: the legal same-`off` nested
    /// clone+clip class gives one ino two references to one block and the
    /// other stands — leaves the index (`None` = a GC-only probe) and the
    /// home decides from what remains.
    ReleaseShared {
        vol_tag: u64,
        block_idx: u64,
        owner: Option<(u64, u32)>,
    },
    // PR 6 (design §5.6.4, KD-SYM-14) — the block 0x60–0x6F, appended:
    // the set-wide directory-rename lock, VOLUME 0's manager only.
    /// Take the set-wide `dir_rename` lease for `appender_id` — or learn
    /// who holds it (`DirRenameBusy`; the caller waits).
    DirRenameLock { appender_id: u32 },
    /// Release it (`already` when it was not held by the caller).
    DirRenameUnlock { appender_id: u32 },
    // ---- PR 8 — verb codes 0x80..=0x8F (design §5.5 / §5.5.1 / §5.5.2;
    // the allocation lease, ranged block grants, the death ledger's
    // RECORD half). Bincode encodes a variant by its declaration index,
    // so the codes are the DOCUMENTED identities ([`VERB_CODE_BLOCK_GRANT`]
    // …) every same-commit peer agrees on; the enum order is append-only.
    /// Up to `want` blocks of DATA volume `vol_tag` for `writer` (0 = the
    /// holder's derivation) — served by the volume's ALLOCATION HOLDER;
    /// the bits are SET and journaled before the reply.
    BlockGrant {
        vol_tag: u64,
        writer: WireIdentity,
        want: u32,
        /// Blocks the writer still holds unconsumed (the idempotent
        /// replay's witness: a remainder covering `want` is answered
        /// verbatim).
        held_unconsumed: u64,
    },
    /// An unconsumed range of `writer`'s grant handed back.
    ReturnBlocks {
        vol_tag: u64,
        writer: WireIdentity,
        start: u64,
        len: u32,
    },
    /// The floating allocation lease of DATA volume `vol_tag` for
    /// `identity` (homed on `home_vol`, appender `appender_id` there) —
    /// served by VOLUME 0's manager under the §5.5.1 ordering law.
    AllocLeaseAcquire {
        vol_tag: u64,
        identity: WireIdentity,
        appender_id: u32,
        home_vol: u16,
        control_ino: u64,
        blocks: u64,
    },
    /// The holder at `term` publishes its bitmap pages (volume-qualified
    /// extents) — the successor's copy.
    AllocLeaseBitmap {
        vol_tag: u64,
        identity: WireIdentity,
        term: u64,
        bitmap: Vec<(u16, (u64, u64))>,
    },
    /// The holder at `term` gives the lease up.
    AllocLeaseRelease {
        vol_tag: u64,
        identity: WireIdentity,
        term: u64,
    },
    /// The recovering manager of volume `vol` says `member`'s region there
    /// is `Recovered` — gates the allocation-lease re-grant.
    RecordRecovered { member: WireIdentity, vol: u16 },
    /// The home shard's eviction → the death ledger (design §5.5.2, PR 10
    /// activates PR 8's reservation): volume 0's manager writes
    /// `dead_member:{member}` carrying the dead epoch and the member's
    /// registrant key (`0` = none) — the recovering managers preempt it.
    /// Idempotent against the record (`Recorded { already }`).
    RecordDeath {
        member: WireIdentity,
        epoch: u64,
        pr_key: u64,
    },
    // ---- PR 10 — verb codes 0xA0..=0xAF are RESERVED for the recovery
    // driver. None is defined: the death ledger and the `recovered:`
    // records are a SHARED-DISK projection every manager reads off tree 0
    // of volume 0 (the S5 poller), never a verb; `RecordDeath` /
    // `RecordRecovered` (PR 8's block) are the two writes the driver
    // needs, and `appender clear` is offline. The range stays reserved.
    // ---- PR 12b — the joined appender's LEAVE (design §5.1.3 "region
    // release": the page goes `Free`, ring extents and the unclaimed grant
    // return — `appender_leaves`), appended at the end.
    /// A JOINED appender leaves: its page (Live under `identity`, naming
    /// no slot — every lease was released first) goes `Free`, its ring
    /// extents and `unclaimed` (its remainder) return to the heap.
    /// Idempotent against the page (`Left { already }`).
    LeaveAppender {
        identity: WireIdentity,
        appender_id: u32,
        unclaimed: Vec<WireRun>,
    },
    /// **`PublishEndpoint`** (PR 12b — the holder → endpoint binding's
    /// JOINER half, §5.1.6): the joined appender's S8 listener, written by
    /// the manager into the joiner's claim-set member entry on every
    /// volume it appends to (`sym_join::resolve_holder_endpoint` reads it
    /// on any mount — a reader's per-slot planes, a peer's shipped steps)
    /// and bound into the manager's own slot holder table. Screened: the
    /// page of `appender_id` must be `Live` under `identity`, the endpoint
    /// a socket address. Idempotent (`Published { already }`).
    PublishEndpoint {
        identity: WireIdentity,
        appender_id: u32,
        endpoint: String,
        pr_key: u64,
    },
    /// **`ResolveEndpoint`** (PR 12b, N ≥ 3): the published listener of
    /// appender `appender_id`, off the MANAGER's slot holder table (every
    /// `PublishEndpoint` it served) or its live claim set. A joiner asks
    /// it for a holder its own ladder never saw — its projection of the
    /// claim set is its open's, so the durable resolve is stale there
    /// while the manager's is exact. One `u32` word, one table lookup;
    /// `Endpoint { endpoint: None }` for a holder that has not published.
    ResolveEndpoint { appender_id: u32 },
    /// **`PublishRoots`** (PR 12b round 4 — the WIRE form of PR 4's
    /// page-budget overflow law): a wire lessee holding more slots than
    /// its page names (`SLOT_PAGE_BUDGET`) has roots with no durable home
    /// — an unpublished root is a floor on its ring's tail, and a lessee
    /// that first-touched 64 unleased slots inside one `T_idle` wedged
    /// its ring and fail-stopped the volume. Its checkpoint ships the
    /// roots its page cannot hold (the manager's own overflow selection,
    /// `region_page_overflow`); the manager rewrites tree 0's `Leased`
    /// records with them under ONE entry (the lease itself untouched —
    /// lessee, `g`, seq floor kept), every word screened against the
    /// caller's leased set at its `g`, its grant and the node at the
    /// address; idempotent against tree 0's current words
    /// (`RootsPublished { already }`). The lessee lifts the floors at
    /// the reply — the entry is durable before the manager answers.
    PublishRoots {
        appender_id: u32,
        roots: Vec<WireSlotRoot>,
    },
}

/// PR 12b's documented verb codes (the wire encodes the declaration index).
pub const VERB_CODE_LEAVE_APPENDER: u8 = 0xB0;
pub const VERB_CODE_PUBLISH_ENDPOINT: u8 = 0xB1;
pub const VERB_CODE_RESOLVE_ENDPOINT: u8 = 0xB2;
pub const VERB_CODE_PUBLISH_ROOTS: u8 = 0xB3;

/// One published root on the wire (`PublishRoots`): the routing slot,
/// the lease generation the lessee holds it at, and the tree's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSlotRoot {
    pub slot: u16,
    pub g: u32,
    pub root: (u64, u64),
    pub cursor: u64,
    pub slot_tree_extents: u32,
}

/// PR 8's documented verb codes (the range the level-4 coordination
/// assigned; the wire encodes the enum's declaration index — these are the
/// identities peers and the notes name).
pub const VERB_CODE_BLOCK_GRANT: u8 = 0x80;
pub const VERB_CODE_RETURN_BLOCKS: u8 = 0x81;
pub const VERB_CODE_ALLOC_LEASE_ACQUIRE: u8 = 0x82;
pub const VERB_CODE_ALLOC_LEASE_BITMAP: u8 = 0x83;
pub const VERB_CODE_ALLOC_LEASE_RELEASE: u8 = 0x84;
pub const VERB_CODE_RECORD_RECOVERED: u8 = 0x85;
pub const VERB_CODE_RECORD_DEATH: u8 = 0x86;

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
            Self::MarkShared { .. } => "mark_shared",
            Self::ShareBlock { .. } => "share_block",
            Self::ReleaseShared { .. } => "release_shared",
            Self::DirRenameLock { .. } => "dir_rename_lock",
            Self::DirRenameUnlock { .. } => "dir_rename_unlock",
            // PR 8
            Self::BlockGrant { .. } => "block_grant",
            Self::ReturnBlocks { .. } => "return_blocks",
            Self::AllocLeaseAcquire { .. } => "alloc_lease_acquire",
            Self::AllocLeaseBitmap { .. } => "alloc_lease_bitmap",
            Self::AllocLeaseRelease { .. } => "alloc_lease_release",
            Self::RecordRecovered { .. } => "record_recovered",
            Self::RecordDeath { .. } => "record_death",
            Self::LeaveAppender { .. } => "leave_appender",
            Self::PublishEndpoint { .. } => "publish_endpoint",
            Self::ResolveEndpoint { .. } => "resolve_endpoint",
            Self::PublishRoots { .. } => "publish_roots",
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
        /// The base of THIS incarnation's node-seq space (PR 13,
        /// `kv::node_seq`): minted durable by the manager for every join,
        /// a rejoin included — the joiner's handle starts here.
        node_seq_base: u64,
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

    // ---- PR 7 (design §5.4.4) ----
    /// `MarkShared`: the bit is durable (`already` = it was — a replay or
    /// a clone of a clone; nothing written).
    Marked {
        already: bool,
    },
    /// `MarkShared`: no reference record exists — the block may be freed;
    /// the cloner aborts (ENOENT-class).
    SharedGone,
    /// `ShareBlock`: entries written / already present.
    Shared {
        inserted: u32,
        already: u32,
    },
    /// `ReleaseShared`: the home's verdict — `remaining` entries stand
    /// (`0` with `shared` = the block frees; `shared == false` = the index
    /// never named it, the caller's local verdict stands).
    SharedReleased {
        shared: bool,
        remaining: u32,
    },
    // PR 6 — appended.
    /// The set-wide directory-rename lease is the caller's (`already` =
    /// it was — KD-SYM-7).
    DirRenameLocked {
        already: bool,
    },
    /// Another appender holds it; the caller waits and retries.
    DirRenameBusy {
        holder: u32,
    },
    /// Released (`already` = the caller held nothing).
    DirRenameUnlocked {
        already: bool,
    },
    // ---- PR 8
    /// `BlockGrant`: the ranges the writer holds — one fresh range, or
    /// its unconsumed grants verbatim (`already`).
    BlocksGranted {
        grants: Vec<(u64, u32)>,
        already: bool,
    },
    /// `BlockGrant`: no clear block remains on the holder.
    BlocksFull,
    /// `ReturnBlocks`: blocks cleared (`None` = the range was not the
    /// writer's — refused, nothing cleared).
    BlocksReturned {
        cleared: Option<u64>,
    },
    /// `AllocLeaseAcquire`: granted at `term` (a successor copies
    /// `predecessor_bitmap`), or `already` the caller's.
    AllocLeaseGranted {
        term: u64,
        already: bool,
        predecessor_bitmap: Vec<(u16, (u64, u64))>,
        predecessor_blocks: u64,
    },
    /// `AllocLeaseBitmap` / `AllocLeaseRelease` / `RecordRecovered`: the
    /// durable record landed (`already` = it had).
    Recorded {
        already: bool,
    },
    /// `LeaveAppender` (PR 12b): the page is `Free`, the ring and the
    /// remainder returned (`already` = it was — a replay).
    Left {
        already: bool,
    },
    /// `PublishEndpoint` (PR 12b): the joiner's listener is in its
    /// claim-set entry (`already` = the same address stood — a replay).
    Published {
        already: bool,
    },
    /// `ResolveEndpoint` (PR 12b): the appender's published listener, or
    /// `None` (it has not published).
    Endpoint {
        endpoint: Option<String>,
    },
    /// `PublishRoots` (PR 12b round 4): `published` records rewritten in
    /// tree 0, `already` words tree 0 held verbatim (a replay's).
    RootsPublished {
        published: u32,
        already: u32,
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
    /// The volume's ordinal in the set this service serves for — the
    /// set-wide verbs (`DirRenameLock/Unlock`, volume 0's) screen on it;
    /// `None` = a bare per-volume service that does not know its place
    /// and therefore serves no set-wide verb.
    ordinal: Option<u16>,
    /// The set's LEDGER volume (ordinal 0 — the death ledger's home): a
    /// served `JoinAppender` retires the joining identity's standing
    /// `dead_member:` record there BEFORE any page goes Live under it
    /// (PR 12b round 3, F7). `None` on a bare per-volume service.
    vol0: Option<Arc<KvMetaBackend>>,
}

impl ManagerService {
    pub fn new(volume: Arc<KvMetaBackend>) -> Arc<Self> {
        Arc::new(Self {
            volume,
            ordinal: None,
            vol0: None,
        })
    }

    /// The service for the volume at `ordinal` of its set whose ledger
    /// volume is `vol0`.
    pub fn new_at(volume: Arc<KvMetaBackend>, ordinal: u16, vol0: Arc<KvMetaBackend>) -> Arc<Self> {
        Arc::new(Self {
            volume,
            ordinal: Some(ordinal),
            vol0: Some(vol0),
        })
    }

    /// **The join is the identity's newer incarnation** (PR 12b round 3,
    /// F7 — PR 10 review round 3's Issue-25 obligation): a `dead_member:`
    /// record naming the joining `(node, mount slot)` is a PREDECESSOR's
    /// — retired at the ledger volume before the join writes a page Live,
    /// or the next ledger poll, reading the fresh Live page with the
    /// record still standing (the member's membership join is the
    /// ladder's rung 3, AFTER the open), recovers the live rejoiner's
    /// region and its region open refuses. Ordered before the page: a
    /// poll that sees the Live page re-reads the record and finds it gone.
    /// A ledger volume that is not a manager retires nothing (a joined
    /// appender serves no `JoinAppender`).
    async fn retire_joiners_death_record(
        &self,
        identity: &crate::meta_backend::kv::appender::AppenderIdentity,
    ) -> std::result::Result<(), crate::meta_backend::kv::KvError> {
        let Some(vol0) = self.vol0.as_ref() else {
            return Ok(());
        };
        if vol0.appender_stats().is_none() || vol0.is_joined_appender() {
            return Ok(());
        }
        if vol0.retire_death_record(identity).await? {
            log::warn!(
                "meta volume {}: JoinAppender by node {:#018x} / mount slot {:#x} RETIRED the \
                 identity's standing death record at the ledger volume — the join is its newer \
                 incarnation (the predecessor's regions are the recovery's; a live one would \
                 have been recovered under the record)",
                vol0.device_path().display(),
                identity.node_token,
                identity.mount_slot
            );
        }
        Ok(())
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
        self.serve_frame(req.id, &req.peer, frame, t_admit).await
    }

    /// Serve an already-decoded frame (the set dispatcher decodes once to
    /// read the volume ordinal, then hands the frame here). `peer` is the
    /// session's authenticated peer id.
    async fn serve_frame(
        &self,
        req_id: u64,
        peer: &str,
        frame: ManagerRequestFrame,
        t_admit: Instant,
    ) -> RpcResponse {
        let req = RpcRequest {
            id: req_id,
            verb: VERB_MANAGER_CALL,
            body: Vec::new(),
            peer: peer.to_string(),
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
        // The identity-carrying appender verbs bind their `identity` word
        // to the SESSION's authenticated peer (PR 12b review round 1,
        // Issue 6 — the D2 trust boundary's inside, but PR 3's bounded-
        // execution law binds every other identity word tighter): a
        // `PublishEndpoint` naming ANOTHER live appender would redirect
        // every peer's token / custody / step traffic to the caller's
        // listener; a `LeaveAppender` of another appender would free its
        // region. A production appender's session peer IS its member id
        // (`cowriter::node_member_id()` — what `PublishEndpoint` writes);
        // a frame whose identity derives another member id is REJECTED
        // (`manager_verb_rejected`). `JoinAppender` is bound the same way
        // when the session's peer is a member id (the production shape);
        // a session under an ad-hoc peer id (PR 3's contracts, which
        // pre-date the rule) keeps the join's own witness laws.
        let identity_screen = screen_identity_peer(&frame.call, &req.peer);
        if identity_screen.is_some() {
            set.verbs
                .rejected
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
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
        // The identity screen's verdict rides the executor's own
        // `Rejected` class so the reply frame is the encoded one every
        // peer decodes.
        let served: std::result::Result<ManagerReply, crate::meta_backend::kv::KvError> =
            if let Some(reason) = identity_screen {
                Err(crate::meta_backend::kv::KvError::Rejected(reason))
            } else {
                match &frame.call {
                    ManagerCall::JoinAppender {
                        identity,
                        ring_want_bytes,
                    } => async {
                        let identity: crate::meta_backend::kv::appender::AppenderIdentity =
                            (*identity).into();
                        self.retire_joiners_death_record(&identity).await?;
                        self.volume
                            .manager_join_appender(identity, *ring_want_bytes)
                            .await
                    }
                    .await
                    .map(
                        |JoinOutcome {
                             appender_id,
                             page_addr,
                             ring_segments,
                             grant,
                             already,
                             node_seq_base,
                         }| ManagerReply::Joined {
                            appender_id,
                            page_addr,
                            ring_segments: ring_segments.iter().map(|s| (s.start, s.len)).collect(),
                            grant: runs_to_wire(&grant),
                            already,
                            node_seq_base,
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
                    ManagerCall::ResolveSlot { slot } => {
                        self.volume.manager_resolve_slot_wire(*slot)
                    }
                    // PR 7. The frame's owner is the volume's key form; the
                    // executors validate nothing proportional to a wire
                    // integer beyond the frame's own `refs` length (one
                    // lookup each, bounded by the CONTROL cap).
                    ManagerCall::MarkShared {
                        vol_tag,
                        block_idx,
                        owner_ino,
                        block_index,
                    } => {
                        // PR 12 (PR 7's owed window, review round 2 Issue 20): the
                        // served mark runs UNDER the source file's block guard —
                        // `BLOCK_FLUSH_LOCKS(owner_ino, block_index)`, lock-order
                        // rung 3, the guard the local W1 patch site holds from its
                        // sole-owner predicate through its DMA — so the durable
                        // bit and the RAM mark land either wholly BEFORE a patch's
                        // fenced mark load (the patch declines) or wholly AFTER a
                        // patch's DMA (a legitimate patch of a then-unshared
                        // block). Rung 3 before the commit's rung-4 locks: the
                        // shipped order. The metadata plane never held this guard
                        // before, so a served mark could land between the
                        // patcher's durable probe and its mark load.
                        let guard_key = shared_refs::routed()
                            .and_then(|r| r.global_ino(&self.volume, *owner_ino));
                        let _block_guard = match guard_key {
                            Some(global) => Some(
                                crate::fuse_client::BLOCK_FLUSH_LOCKS
                                    .get_lock(global, *block_index)
                                    .lock()
                                    .await,
                            ),
                            // No routed set (a bare-volume rig): nothing patches.
                            None => None,
                        };
                        self.volume
                            .mark_block_ref_shared(&crate::meta_backend::kv::block_refs::BlockRef {
                                vol_tag: *vol_tag,
                                block_idx: *block_idx,
                                owner_ino: *owner_ino,
                                block_index: *block_index,
                            })
                            .await
                            .map(|o| {
                                // The durable bit is the authority; the RAM mark
                                // the W1 predicate reads synchronously is set
                                // beside it on the serving mount through the
                                // routed hooks (absent = no armed router here,
                                // nothing to set).
                                if o != shared_refs::MarkOutcome::Gone {
                                    if let Some(routed) = shared_refs::routed() {
                                        routed.note_marked(*vol_tag, *block_idx);
                                    }
                                }
                                match o {
                                    shared_refs::MarkOutcome::Marked => {
                                        ManagerReply::Marked { already: false }
                                    }
                                    shared_refs::MarkOutcome::Already => {
                                        ManagerReply::Marked { already: true }
                                    }
                                    shared_refs::MarkOutcome::Gone => ManagerReply::SharedGone,
                                }
                            })
                            .map_err(|e| crate::meta_backend::kv::KvError::Busy(e.to_string()))
                    }
                    ManagerCall::ShareBlock {
                        vol_tag,
                        block_idx,
                        refs,
                    } => {
                        let refs: Vec<crate::meta_backend::kv::block_refs::BlockRef> = refs
                            .iter()
                            .map(|&(owner_ino, block_index)| {
                                crate::meta_backend::kv::block_refs::BlockRef {
                                    vol_tag: *vol_tag,
                                    block_idx: *block_idx,
                                    owner_ino,
                                    block_index,
                                }
                            })
                            .collect();
                        // PR 3/4's law for the wire words: every named reference
                        // is confirmed against durable state (exists, SHARED)
                        // before the index moves; a frame with one bad word is
                        // REJECTED whole (`manager_verb_rejected`).
                        self.volume
                            .share_block_screened(&refs)
                            .await
                            .map(|(inserted, already)| ManagerReply::Shared {
                                inserted: inserted as u32,
                                already: already as u32,
                            })
                    }
                    ManagerCall::ReleaseShared {
                        vol_tag,
                        block_idx,
                        owner,
                    } => {
                        // The wire arm takes NO GC verdict: the index keys the
                        // routed GLOBAL owner and this service sees one volume's
                        // ledger in its own key form — an entry whose reference
                        // it cannot read stands. The routed executor
                        // (`DataRouter::release_shared_at`) is the GC arm.
                        self.volume
                            .release_shared(*vol_tag, *block_idx, *owner, |_| async { Ok(true) })
                            .await
                            .map(|v| match v {
                                shared_refs::SharedRelease::NotShared => {
                                    ManagerReply::SharedReleased {
                                        shared: false,
                                        remaining: 0,
                                    }
                                }
                                shared_refs::SharedRelease::Held { remaining } => {
                                    ManagerReply::SharedReleased {
                                        shared: true,
                                        remaining: remaining as u32,
                                    }
                                }
                                shared_refs::SharedRelease::Freed => ManagerReply::SharedReleased {
                                    shared: true,
                                    remaining: 0,
                                },
                            })
                    }
                    // The two set-wide verbs: the wire words screened first
                    // (volume 0 only, a Live non-own id, the holder for an
                    // unlock — review round 1, Issue 3), the record's term the
                    // SERVING manager's era (provenance, never a check).
                    ManagerCall::DirRenameLock { appender_id } => self
                        .volume
                        .manager_dir_rename_lock_wire(self.ordinal, *appender_id)
                        .await
                        .map(|out| match out {
                            crate::meta_backend::kv::backend::DirRenameOutcome::Locked {
                                already,
                            } => ManagerReply::DirRenameLocked { already },
                            crate::meta_backend::kv::backend::DirRenameOutcome::Busy { holder } => {
                                ManagerReply::DirRenameBusy { holder }
                            }
                        }),
                    ManagerCall::DirRenameUnlock { appender_id } => self
                        .volume
                        .manager_dir_rename_unlock_wire(self.ordinal, *appender_id)
                        .await
                        .map(|already| ManagerReply::DirRenameUnlocked { already }),
                    // ---- PR 8
                    ManagerCall::BlockGrant {
                        vol_tag,
                        writer,
                        want,
                        held_unconsumed,
                    } => self
                        .volume
                        .holder_block_grant(
                            *vol_tag,
                            &wire_writer_name(writer),
                            u64::from(*want),
                            *held_unconsumed,
                        )
                        .await
                        .map(|outcome| match outcome {
                            crate::block_grant::CarveOutcome::Granted(g) => {
                                ManagerReply::BlocksGranted {
                                    grants: vec![(g.start, g.len)],
                                    already: false,
                                }
                            }
                            crate::block_grant::CarveOutcome::Already(gs) => {
                                ManagerReply::BlocksGranted {
                                    grants: gs.iter().map(|g| (g.start, g.len)).collect(),
                                    already: true,
                                }
                            }
                            crate::block_grant::CarveOutcome::Full => ManagerReply::BlocksFull,
                        }),
                    ManagerCall::ReturnBlocks {
                        vol_tag,
                        writer,
                        start,
                        len,
                    } => self
                        .volume
                        .holder_return_blocks(
                            *vol_tag,
                            &wire_writer_name(writer),
                            crate::block_grant::BlockGrant {
                                start: *start,
                                len: *len,
                            },
                        )
                        .await
                        .map(|cleared| ManagerReply::BlocksReturned { cleared }),
                    ManagerCall::AllocLeaseAcquire {
                        vol_tag,
                        identity,
                        appender_id,
                        home_vol,
                        control_ino,
                        blocks,
                    } => self
                        .volume
                        .manager_alloc_lease_acquire(
                            *vol_tag,
                            (*identity).into(),
                            *appender_id,
                            *home_vol,
                            *control_ino,
                            *blocks,
                        )
                        .await
                        .map(|g| ManagerReply::AllocLeaseGranted {
                            term: g.term,
                            already: g.already,
                            predecessor_bitmap: g
                                .predecessor_bitmap
                                .iter()
                                .map(|(v, e)| (*v, (e.start, e.len)))
                                .collect(),
                            predecessor_blocks: g.predecessor_blocks,
                        }),
                    ManagerCall::AllocLeaseBitmap {
                        vol_tag,
                        identity,
                        term,
                        bitmap,
                    } => self
                        .volume
                        .manager_alloc_lease_bitmap(
                            *vol_tag,
                            (*identity).into(),
                            *term,
                            bitmap
                                .iter()
                                .map(|(v, (s, l))| (*v, ExtentRef { start: *s, len: *l }))
                                .collect(),
                        )
                        .await
                        .map(|already| ManagerReply::Recorded { already }),
                    ManagerCall::AllocLeaseRelease {
                        vol_tag,
                        identity,
                        term,
                    } => self
                        .volume
                        .manager_alloc_lease_release(*vol_tag, (*identity).into(), *term)
                        .await
                        .map(|already| ManagerReply::Recorded { already }),
                    ManagerCall::RecordRecovered { member, vol } => self
                        .volume
                        .manager_record_recovered((*member).into(), *vol)
                        .await
                        .map(|already| ManagerReply::Recorded { already }),
                    // PR 10: the home shard's eviction shipped here. The wire
                    // words are an identity, an epoch and a key — none sizes an
                    // allocation or names an ino; the record is idempotent and
                    // every effect it drives (the quarantine, the recovery)
                    // re-reads durable state. A member this manager LISTS as
                    // live is never declared dead by a peer's word (the
                    // screen below).
                    ManagerCall::RecordDeath {
                        member,
                        epoch,
                        pr_key,
                    } => match self.volume.screen_record_death(member, *pr_key) {
                        Err(e) => Err(e),
                        Ok(key) => self
                            .volume
                            .record_death_with_key((*member).into(), *epoch, key)
                            .await
                            .map(|already| ManagerReply::Recorded { already }),
                    },
                    // PR 12b: the runs travel as RUNS and are intersected with
                    // the appender's record run by run (the ReturnExtents
                    // law); the page and the ring extents are the directory's.
                    ManagerCall::LeaveAppender {
                        identity,
                        appender_id,
                        unclaimed,
                    } => self
                        .volume
                        .manager_leave_appender(
                            (*identity).into(),
                            *appender_id,
                            &runs_from_wire(unclaimed),
                        )
                        .await
                        .map(|already| ManagerReply::Left { already }),
                    ManagerCall::PublishEndpoint {
                        identity,
                        appender_id,
                        endpoint,
                        pr_key,
                    } => self
                        .volume
                        .manager_publish_endpoint(
                            (*identity).into(),
                            *appender_id,
                            endpoint,
                            *pr_key,
                        )
                        .await
                        .map(|already| ManagerReply::Published { already }),
                    ManagerCall::ResolveEndpoint { appender_id } => self
                        .volume
                        .manager_resolve_endpoint(*appender_id)
                        .await
                        .map(|endpoint| ManagerReply::Endpoint { endpoint }),
                    ManagerCall::PublishRoots { appender_id, roots } => self
                        .volume
                        .manager_publish_roots_wire(*appender_id, roots)
                        .await
                        .map(|(published, already)| ManagerReply::RootsPublished {
                            published,
                            already,
                        }),
                }
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
            // PR 8: the allocation lease's re-grant waiting on the home
            // recovery — the same retry class.
            Err(e @ crate::meta_backend::kv::KvError::LeaseDeferred(_)) => (
                ManagerReply::Deferred {
                    reason: e.to_string(),
                },
                STATUS_DEFERRED,
            ),
            // Symmetric PR 9: a handover deferred for a live custody grant
            // (the grants recalled; the requester retries) — the same
            // retry class, never `Refused`.
            Err(e @ crate::meta_backend::kv::KvError::HandoverDeferred(_)) => (
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

/// **The identity word against the session's peer** (PR 12b review round
/// 1, Issue 6) — pure, fuzzed by `manager_call_frame`'s poisoned-frame arm
/// + the proptest mirror: `Some(reason)` = REJECT. `PublishEndpoint` and
/// `LeaveAppender` (PR 12b's verbs — a production appender's session peer
/// is its member id, `cowriter::node_member_id()`) REQUIRE the frame's
/// identity to derive the peer; `JoinAppender` requires it whenever the
/// peer IS a member id (an ad-hoc peer keeps the join's own witness laws
/// — PR 3's contracts pre-date the rule). Every other verb passes.
pub fn screen_identity_peer(call: &ManagerCall, peer: &str) -> Option<String> {
    let (verb, identity, strict) = match call {
        ManagerCall::PublishEndpoint { identity, .. } => ("PublishEndpoint", identity, true),
        ManagerCall::LeaveAppender { identity, .. } => ("LeaveAppender", identity, true),
        ManagerCall::JoinAppender { identity, .. } => ("JoinAppender", identity, false),
        _ => return None,
    };
    let derived = crate::cowriter::node_member_id_of(identity.node_token, identity.mount_slot);
    if derived == peer {
        return None;
    }
    if !strict && crate::cowriter::parse_node_member_id(peer).is_none() {
        return None;
    }
    Some(format!(
        "{verb}: the frame's identity ({:#x}, mount slot {}) derives member id '{derived}' but \
         the session's authenticated peer is '{peer}' — an appender speaks for itself alone \
         (REJECTED)",
        identity.node_token, identity.mount_slot
    ))
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
        // The ledger volume is ordinal 0 (`recovery::vol0_of`'s law: the
        // volume hosting routing slot 0 — the first of the set).
        let Some(vol0) = volumes.first() else {
            return Arc::new(Self {
                volumes: Vec::new(),
            });
        };
        Arc::new(Self {
            volumes: volumes
                .iter()
                .enumerate()
                .map(|(i, v)| ManagerService::new_at(Arc::clone(v), i as u16, Arc::clone(vol0)))
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
        svc.serve_frame(req.id, &req.peer, frame, t_admit).await
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

    /// `PublishRoots` (PR 12b round 4): `(published, already)`.
    pub async fn publish_roots(
        &mut self,
        appender_id: u32,
        roots: Vec<WireSlotRoot>,
    ) -> Result<(u32, u32)> {
        match self
            .call(ManagerCall::PublishRoots { appender_id, roots })
            .await?
        {
            ManagerReply::RootsPublished { published, already } => Ok((published, already)),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "PublishRoots answered {other:?}"
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

    /// `DirRenameLock` (PR 6) — `Ok(reply)` is `DirRenameLocked` or
    /// `DirRenameBusy`.
    pub async fn dir_rename_lock(&mut self, appender_id: u32) -> Result<ManagerReply> {
        match self
            .call(ManagerCall::DirRenameLock { appender_id })
            .await?
        {
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Ok(other),
        }
    }

    /// `DirRenameUnlock` (PR 6) — `Ok(reply)` is `DirRenameUnlocked`.
    pub async fn dir_rename_unlock(&mut self, appender_id: u32) -> Result<ManagerReply> {
        match self
            .call(ManagerCall::DirRenameUnlock { appender_id })
            .await?
        {
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Ok(other),
        }
    }
}

// PR 7 — the clone protocol's client half (design §5.4.4).
impl ManagerClient {
    /// `MarkShared` at the source's holder — `Ok(reply)` is `Marked` or
    /// `SharedGone` (the cloner aborts on the latter).
    pub async fn mark_shared(
        &mut self,
        reference: crate::meta_backend::kv::block_refs::BlockRef,
    ) -> Result<ManagerReply> {
        match self
            .call(ManagerCall::MarkShared {
                vol_tag: reference.vol_tag,
                block_idx: reference.block_idx,
                owner_ino: reference.owner_ino,
                block_index: reference.block_index,
            })
            .await?
        {
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Ok(other),
        }
    }

    /// `ShareBlock` at the index home — `Ok((inserted, already))`.
    pub async fn share_block(
        &mut self,
        vol_tag: u64,
        block_idx: u64,
        refs: &[(u64, u32)],
    ) -> Result<(u32, u32)> {
        match self
            .call(ManagerCall::ShareBlock {
                vol_tag,
                block_idx,
                refs: refs.to_vec(),
            })
            .await?
        {
            ManagerReply::Shared { inserted, already } => Ok((inserted, already)),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "ShareBlock answered {other:?}"
            ))),
        }
    }

    /// `ReleaseShared` at the index home — `Ok((shared, remaining))`;
    /// `owner` = the releasing `(ino, block_index)`.
    pub async fn release_shared(
        &mut self,
        vol_tag: u64,
        block_idx: u64,
        owner: Option<(u64, u32)>,
    ) -> Result<(bool, u32)> {
        match self
            .call(ManagerCall::ReleaseShared {
                vol_tag,
                block_idx,
                owner,
            })
            .await?
        {
            ManagerReply::SharedReleased { shared, remaining } => Ok((shared, remaining)),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "ReleaseShared answered {other:?}"
            ))),
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

/// PR 8: the ledger's writer key for a wire identity — the KD-MW-2 pair,
/// the same spelling the membership census uses for a node.
pub fn wire_writer_name(w: &WireIdentity) -> String {
    format!("node_{:016x}.m{:08x}", w.node_token, w.mount_slot)
}

impl ManagerClient {
    /// `BlockGrant` — `Ok(Some(ranges))` granted (or the unconsumed
    /// remainder verbatim), `Ok(None)` the holder is full.
    pub async fn block_grant(
        &mut self,
        vol_tag: u64,
        writer: WireIdentity,
        want: u32,
        held_unconsumed: u64,
    ) -> Result<Option<Vec<crate::block_grant::BlockGrant>>> {
        match self
            .call(ManagerCall::BlockGrant {
                vol_tag,
                writer,
                want,
                held_unconsumed,
            })
            .await?
        {
            ManagerReply::BlocksGranted { grants, .. } => Ok(Some(
                grants
                    .into_iter()
                    .map(|(start, len)| crate::block_grant::BlockGrant { start, len })
                    .collect(),
            )),
            ManagerReply::BlocksFull => Ok(None),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "BlockGrant answered {other:?}"
            ))),
        }
    }

    /// `ReturnBlocks` — `Ok(cleared)`; `None` = the range was refused.
    pub async fn return_blocks(
        &mut self,
        vol_tag: u64,
        writer: WireIdentity,
        range: crate::block_grant::BlockGrant,
    ) -> Result<Option<u64>> {
        match self
            .call(ManagerCall::ReturnBlocks {
                vol_tag,
                writer,
                start: range.start,
                len: range.len,
            })
            .await?
        {
            ManagerReply::BlocksReturned { cleared } => Ok(cleared),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "ReturnBlocks answered {other:?}"
            ))),
        }
    }

    /// `AllocLeaseAcquire` — `Ok(reply)` is `AllocLeaseGranted` or
    /// `Deferred` (the successor before the home recovery: EAGAIN class).
    pub async fn alloc_lease_acquire(
        &mut self,
        vol_tag: u64,
        identity: WireIdentity,
        appender_id: u32,
        home_vol: u16,
        control_ino: u64,
        blocks: u64,
    ) -> Result<ManagerReply> {
        match self
            .call(ManagerCall::AllocLeaseAcquire {
                vol_tag,
                identity,
                appender_id,
                home_vol,
                control_ino,
                blocks,
            })
            .await?
        {
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Ok(other),
        }
    }

    /// `AllocLeaseBitmap` — `Ok(already)`.
    pub async fn alloc_lease_bitmap(
        &mut self,
        vol_tag: u64,
        identity: WireIdentity,
        term: u64,
        bitmap: Vec<(u16, (u64, u64))>,
    ) -> Result<bool> {
        match self
            .call(ManagerCall::AllocLeaseBitmap {
                vol_tag,
                identity,
                term,
                bitmap,
            })
            .await?
        {
            ManagerReply::Recorded { already } => Ok(already),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "AllocLeaseBitmap answered {other:?}"
            ))),
        }
    }

    /// `AllocLeaseRelease` — `Ok(already)`.
    pub async fn alloc_lease_release(
        &mut self,
        vol_tag: u64,
        identity: WireIdentity,
        term: u64,
    ) -> Result<bool> {
        match self
            .call(ManagerCall::AllocLeaseRelease {
                vol_tag,
                identity,
                term,
            })
            .await?
        {
            ManagerReply::Recorded { already } => Ok(already),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "AllocLeaseRelease answered {other:?}"
            ))),
        }
    }

    /// `RecordRecovered` — `Ok(already)`.
    pub async fn record_recovered(&mut self, member: WireIdentity, vol: u16) -> Result<bool> {
        match self
            .call(ManagerCall::RecordRecovered { member, vol })
            .await?
        {
            ManagerReply::Recorded { already } => Ok(already),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "RecordRecovered answered {other:?}"
            ))),
        }
    }

    /// `RecordDeath` (PR 10) — `Ok(already)`; a refusal (this manager
    /// lists the member live) is the error.
    pub async fn record_death(
        &mut self,
        member: WireIdentity,
        epoch: u64,
        pr_key: u64,
    ) -> Result<bool> {
        match self
            .call(ManagerCall::RecordDeath {
                member,
                epoch,
                pr_key,
            })
            .await?
        {
            ManagerReply::Recorded { already } => Ok(already),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "RecordDeath answered {other:?}"
            ))),
        }
    }

    /// `LeaveAppender` (PR 12b) — `Ok(already)`.
    pub async fn leave_appender(
        &mut self,
        identity: AppenderIdentity,
        appender_id: u32,
        unclaimed: &[GrantRun],
    ) -> Result<bool> {
        match self
            .call(ManagerCall::LeaveAppender {
                identity: identity.into(),
                appender_id,
                unclaimed: runs_to_wire(unclaimed),
            })
            .await?
        {
            ManagerReply::Left { already } => Ok(already),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "LeaveAppender answered {other:?}"
            ))),
        }
    }

    /// `PublishEndpoint` (PR 12b) — `Ok(already)`.
    pub async fn publish_endpoint(
        &mut self,
        identity: AppenderIdentity,
        appender_id: u32,
        endpoint: &str,
        pr_key: u64,
    ) -> Result<bool> {
        match self
            .call(ManagerCall::PublishEndpoint {
                identity: identity.into(),
                appender_id,
                endpoint: endpoint.to_string(),
                pr_key,
            })
            .await?
        {
            ManagerReply::Published { already } => Ok(already),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "PublishEndpoint answered {other:?}"
            ))),
        }
    }

    /// `ResolveEndpoint` (PR 12b) — `Ok(None)` = the appender has not
    /// published.
    pub async fn resolve_endpoint(&mut self, appender_id: u32) -> Result<Option<String>> {
        match self
            .call(ManagerCall::ResolveEndpoint { appender_id })
            .await?
        {
            ManagerReply::Endpoint { endpoint } => Ok(endpoint),
            ManagerReply::Refused { reason } => Err(SqueezefsError::InvalidOperation(reason)),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "ResolveEndpoint answered {other:?}"
            ))),
        }
    }
}
